use super::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

const T: u64 = 9_000_000;

/// The fake core: t1 a plain shell, t2 an agent blocked on a question.
fn fake_core(method: &str, params: Value) -> Result<Value, RpcError> {
    let t = params["terminal"].as_str().unwrap_or_default();
    Ok(match method {
        "terminal.get" if t == "t2" => {
            json!({"address": "t2", "state": "live", "argv": ["claude"], "cwd": "/w", "reported": true, "status": {"state": "blocked"}})
        }
        "terminal.get" => {
            json!({"address": t, "state": "live", "argv": ["/bin/sh"], "cwd": r"C:\work", "reported": false})
        }
        "terminal.list" => {
            json!({"terminals": [{"address": "t2", "argv": ["claude"], "cwd": "/w", "state": "live", "status": {"state": "blocked"}}]})
        }
        _ => json!({}),
    })
}

struct Fixture {
    web: Arc<Web>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(hosts: Vec<String>) -> Fixture {
    let bridge = crate::bridge::scratch();
    let dir = bridge.dir.clone();
    let (notices, _) = tokio::sync::broadcast::channel(16);
    let web = Web { bridge: Mutex::new(bridge), core: Box::new(fake_core), hosts, notices };
    Fixture { web: Arc::new(web), dir }
}

impl Fixture {
    fn code(&self, now: u64) -> String {
        self.web.bridge.lock().unwrap().pair("web", now).unwrap().0
    }

    fn say(&self, conn: &mut Conn, name: &str, data: Value, now: u64) -> Step {
        self.web.incoming(conn, &json!({"name": name, "data": data}).to_string(), now)
    }

    /// A paired connection and its session token.
    fn paired(&self) -> (Conn, String) {
        let mut conn = Conn::default();
        let code = self.code(T);
        let step = self.say(&mut conn, "pair", json!({"token": code, "label": "phone"}), T);
        let session = step.out[0]["data"]["session"].as_str().unwrap().to_string();
        (conn, session)
    }
}

fn names(step: &Step) -> Vec<&str> {
    step.out.iter().map(|m| m["name"].as_str().unwrap()).collect()
}

#[test]
fn lan_mode_binds_only_a_private_ipv4_address() {
    let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
    for ok in ["192.168.1.20", "10.0.0.5", "172.16.0.1", "172.31.255.254", "100.64.0.1", "100.127.255.1"] {
        assert_eq!(lan_address(ip(ok)), Ok(ok.parse().unwrap()), "{ok}");
    }
    for bad in
        ["8.8.8.8", "172.32.0.1", "100.128.0.1", "127.0.0.1", "169.254.1.1", "0.0.0.0", "fe80::1", "::1", "2001:db8::1"]
    {
        assert!(lan_address(ip(bad)).is_err(), "{bad}");
    }
    assert!(lan_address(None).unwrap_err().contains("no network"));
    assert_eq!(hosts(Ipv4Addr::LOCALHOST, 8377), ["127.0.0.1:8377", "localhost:8377"]);
    assert_eq!(
        hosts("192.168.1.20".parse().unwrap(), 9),
        ["192.168.1.20:9"],
        "a LAN listener answers to its own address only"
    );
}

#[test]
fn the_pairing_link_draws_as_a_square_qr_code() {
    let art = qr("http://192.168.1.20:8377/#pair=ABCDEFGH");
    let lines: Vec<&str> = art.lines().collect();
    let width = lines[0].chars().count();
    assert!(width >= 25 && lines.iter().all(|l| l.chars().count() == width), "{art}");
    assert_eq!(lines.len(), width.div_ceil(2));
    assert!(lines[0].chars().all(|c| c == '█'), "a light quiet zone on top");
    assert!(art.contains(' ') && art.contains('▀') && art.contains('▄'));
}

#[test]
fn a_one_time_token_pairs_one_device_once() {
    let f = fixture(Vec::new());
    let mut conn = Conn::default();
    let refused = f.say(&mut conn, "pair", json!({"token": "WRONGONE"}), T);
    assert_eq!((names(&refused), refused.close), (vec!["error"], true));
    assert_eq!(refused.out[0]["data"]["message"], "wrong or expired code");

    let code = f.code(T);
    let late = f.say(&mut Conn::default(), "pair", json!({"token": code}), T + 5 * 60_000);
    assert!(late.close, "5 minutes for a web link");
    let code = f.code(T);
    let step = f.say(&mut conn, "pair", json!({"token": code.to_lowercase(), "label": "  Vijay's phone  "}), T);
    assert_eq!((names(&step), step.close), (vec!["paired", "snapshot"], false));
    let session = step.out[0]["data"]["session"].as_str().unwrap();
    assert_eq!(session.len(), 64);
    assert_eq!(step.out[0]["data"]["device"], "Vijay's phone");
    assert_eq!(step.out[1]["data"]["lines"], json!(["t2 · claude · /w · blocked"]));
    assert!(f.say(&mut Conn::default(), "pair", json!({"token": code}), T).close, "single use");

    let bridge = f.web.bridge.lock().unwrap();
    let me = bridge.identities().unwrap().into_iter().find(|i| i.transport == "web").unwrap();
    assert_eq!(me.label, "Vijay's phone");
    let state = std::fs::read_to_string(f.dir.join("state.json")).unwrap();
    assert!(!state.contains(session), "only its hash is kept");
    assert!(state.contains(&sha::hex(&sha::sha256(session.as_bytes()))));
}

#[test]
fn guessing_tokens_disables_pairing() {
    let f = fixture(Vec::new());
    let code = f.code(T);
    for n in 0..5 {
        assert!(f.say(&mut Conn::default(), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    let blocked = f.say(&mut Conn::default(), "pair", json!({"token": code}), T + 10);
    assert!(blocked.out[0]["data"]["message"].as_str().unwrap().contains("disabled"), "{blocked:?}");
}

#[test]
fn sessions_authenticate_until_revoked_and_obey_the_lock() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    let mut conn = Conn::default();
    assert!(f.say(&mut conn, "command", json!({"cmd": "list"}), T).close, "pair or auth first");
    assert!(!f.say(&mut conn, "ping", Value::Null, T).close, "a ping is fine before");
    for bad in
        [json!({}), json!({"session": ""}), json!({"session": "0".repeat(64)}), json!({"session": session[..63]})]
    {
        let step = f.say(&mut Conn::default(), "auth", bad.clone(), T);
        assert_eq!((names(&step), step.close), (vec!["error"], true), "{bad}");
    }
    let step = f.say(&mut conn, "auth", json!({"session": session}), T);
    assert_eq!((names(&step), step.close), (vec!["hello", "snapshot"], false));

    f.web.bridge.lock().unwrap().set_locked(true).unwrap();
    let locked = f.say(&mut conn, "command", json!({"cmd": "list", "id": 3}), T);
    assert_eq!(locked.out[0]["data"], json!({"kind": "locked", "id": 3}));
    let notice = Notice {
        terminal: "t2".into(),
        agent: String::new(),
        folder: String::new(),
        kind: Kind::NeedsYou,
        excerpt: String::new(),
    };
    assert_eq!(f.web.notice_for(&conn, &notice), None, "no notices while locked");
    f.web.bridge.lock().unwrap().set_locked(false).unwrap();
    assert_eq!(f.web.notice_for(&conn, &notice).unwrap()["data"]["kind"], "needs_you");
    assert_eq!(f.web.notice_for(&Conn::default(), &notice), None, "not to strangers");

    f.web.bridge.lock().unwrap().unpair(None).unwrap();
    let revoked = f.say(&mut conn, "command", json!({"cmd": "list"}), T);
    assert_eq!((revoked.out[0]["data"]["kind"].as_str(), revoked.close), (Some("not_paired"), true));
    assert_eq!(f.web.notice_for(&conn, &notice), None);
    assert!(f.say(&mut Conn::default(), "auth", json!({"session": session}), T).close, "revoked for good");
    let state = std::fs::read_to_string(f.dir.join("state.json")).unwrap();
    assert!(state.contains("\"sessions\": {}"), "its hash went too: {state}");
}

#[test]
fn commands_round_trip_with_confirmations() {
    let f = fixture(Vec::new());
    let (mut conn, _) = f.paired();
    let ask = |conn: &mut Conn, data: Value| f.say(conn, "command", data, T).out.remove(0)["data"].clone();
    assert_eq!(
        ask(&mut conn, json!({"cmd": "approve", "terminal": "t2", "id": "a"}))["text"],
        "done: approve in t2: press enter"
    );
    let kill = ask(&mut conn, json!({"cmd": "kill", "terminal": "t1", "id": 1}));
    assert_eq!(
        (kill["kind"].as_str(), kill["what"].as_str()),
        (Some("needs_confirm"), Some(r"kill t1 (sh in C:\work)"))
    );
    let done = ask(&mut conn, json!({"cmd": "confirm", "token": kill["token"], "id": 2}));
    assert_eq!(done, json!({"kind": "done", "text": r"done: kill t1 (sh in C:\work)", "id": 2}));
    let run = ask(&mut conn, json!({"cmd": "run", "folder": r"\\server\share\app", "command": "claude"}));
    assert_eq!(
        run["kind"],
        if cfg!(windows) { "needs_confirm" } else { "failed" },
        "a UNC folder is only full on Windows"
    );
    let keys = ask(&mut conn, json!({"cmd": "keys", "terminal": "t2", "keys": ["Enter", "ctrl+c"]}));
    assert_eq!(keys["kind"], "done");
    for (bad, why) in [
        (json!({"cmd": "keys", "terminal": "t2", "keys": ["jump"]}), "unknown key \"jump\""),
        (json!({"cmd": "keys", "terminal": "t2"}), "missing keys"),
        (json!({"cmd": "send", "terminal": "t2"}), "missing text"),
        (json!({"cmd": "reboot"}), "unknown command \"reboot\""),
        (json!({}), "unknown command \"\""),
    ] {
        assert_eq!(ask(&mut conn, bad.clone())["message"], why, "{bad}");
    }
    let odd = f.say(&mut conn, "dance", Value::Null, T);
    assert_eq!((names(&odd), odd.close), (vec!["error"], false), "known devices aren't hung up on");
    assert!(f.web.incoming(&mut conn, "not json", T).close);
}

/// The server on a loopback port, answering to that address only.
fn start() -> (Fixture, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let f = fixture(hosts(Ipv4Addr::LOCALHOST, addr.port()));
    let web = f.web.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move { serve(tokio::net::TcpListener::from_std(listener).unwrap(), web).await });
    });
    (f, addr)
}

/// A raw request and the whole answer.
fn request(addr: SocketAddr, head: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(head.replace('\n', "\r\n").as_bytes()).unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    String::from_utf8_lossy(&answer).into_owned()
}

struct Client {
    stream: TcpStream,
}

fn ws(addr: SocketAddr, origin: &str) -> Client {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let head = format!(
        "GET /ws HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
    let mut answer = Vec::new();
    let mut byte = [0u8];
    while !answer.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        answer.push(byte[0]);
    }
    let answer = String::from_utf8(answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 101 ") && answer.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{answer}");
    Client { stream }
}

impl Client {
    fn send(&mut self, name: &str, data: Value) {
        let text = json!({"name": name, "data": data}).to_string();
        self.stream.write_all(&http::client_frame(1, text.as_bytes())).unwrap();
    }

    /// The next server frame: its opcode and payload.
    fn frame(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut head = [0u8; 2];
        self.stream.read_exact(&mut head).ok()?;
        let len = match head[1] & 0x7f {
            126 => {
                let mut n = [0u8; 2];
                self.stream.read_exact(&mut n).ok()?;
                usize::from(u16::from_be_bytes(n))
            }
            127 => {
                let mut n = [0u8; 8];
                self.stream.read_exact(&mut n).ok()?;
                u64::from_be_bytes(n) as usize
            }
            n => usize::from(n),
        };
        let mut payload = vec![0; len];
        self.stream.read_exact(&mut payload).ok()?;
        Some((head[0] & 0x0f, payload))
    }

    /// The next message other than a heartbeat; None once closed.
    fn recv(&mut self) -> Option<Value> {
        loop {
            let (opcode, payload) = self.frame()?;
            if opcode == 8 {
                return None;
            }
            let msg: Value = serde_json::from_slice(&payload).unwrap();
            if msg["name"] != "heartbeat" {
                return Some(msg);
            }
        }
    }
}

#[test]
fn a_phone_pairs_reconnects_commands_and_hears_notices_over_a_websocket() {
    let (f, addr) = start();
    let origin = format!("http://{addr}");
    let mut phone = ws(addr, &origin);
    phone.send("pair", json!({"token": f.code(crate::runtime::now_ms()), "label": "phone"}));
    let paired = phone.recv().unwrap();
    assert_eq!(paired["name"], "paired");
    assert_eq!(phone.recv().unwrap()["name"], "snapshot");
    let session = paired["data"]["session"].as_str().unwrap().to_string();
    drop(phone);

    let mut phone = ws(addr, &origin);
    phone.send("auth", json!({"session": session}));
    assert_eq!(phone.recv().unwrap()["name"], "hello");
    assert_eq!(phone.recv().unwrap()["data"]["lines"][0], "t2 · claude · /w · blocked");
    phone.send("command", json!({"cmd": "kill", "terminal": "t1", "id": 7}));
    let reply = phone.recv().unwrap();
    assert_eq!(
        (reply["name"].as_str(), reply["data"]["kind"].as_str(), &reply["data"]["id"]),
        (Some("reply"), Some("needs_confirm"), &json!(7))
    );

    let notice = Notice {
        terminal: "t2".into(),
        agent: "claude".into(),
        folder: "/w".into(),
        kind: Kind::Done,
        excerpt: "ok".into(),
    };
    // The notice reaches every connected device; resend until this one is subscribed.
    let heard = (0..50).find_map(|_| {
        let _ = f.web.notices.send(notice.clone());
        phone.stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        phone.recv()
    });
    let heard = heard.expect("a notice");
    assert_eq!(
        heard["data"],
        json!({"terminal": "t2", "agent": "claude", "folder": "/w", "kind": "done", "excerpt": "ok"})
    );

    // A ping is answered with a pong carrying the same bytes.
    phone.stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    phone.stream.write_all(&http::client_frame(9, b"hi")).unwrap();
    let pong = std::iter::from_fn(|| phone.frame()).find(|(op, _)| *op != 1).unwrap();
    assert_eq!(pong, (10, b"hi".to_vec()));
}

#[test]
fn strangers_by_host_or_origin_and_oversized_frames_are_turned_away() {
    let (_f, addr) = start();
    let page = request(addr, &format!("GET / HTTP/1.1\nHost: {addr}\n\n"));
    assert!(
        page.starts_with("HTTP/1.1 200 ") && page.contains("default-src 'self'") && page.contains("/app.js"),
        "{page}"
    );
    assert!(
        request(addr, &format!("GET /app.js HTTP/1.1\nHost: localhost:{}\n\n", addr.port()))
            .starts_with("HTTP/1.1 200 ")
    );
    for (head, status) in [
        ("GET / HTTP/1.1\nHost: evil.example\n\n".to_string(), "403"),
        (format!("GET /ws HTTP/1.1\nHost: {addr}\nOrigin: http://evil.example\nUpgrade: websocket\n\n"), "403"),
        (format!("GET /nope HTTP/1.1\nHost: {addr}\n\n"), "404"),
        (format!("GET /ws HTTP/1.1\nHost: {addr}\n\n"), "426"),
        (format!("PUT / HTTP/1.1\nHost: {addr}\n\n"), "405"),
        (format!("GET / HTTP/1.1\nHost: {addr}\nX: {}\n\n", "y".repeat(9000)), "431"),
    ] {
        let answer = request(addr, &head);
        assert!(answer.starts_with(&format!("HTTP/1.1 {status} ")), "{status}: {answer}");
    }
    let mut big = ws(addr, &format!("http://{addr}"));
    let _ = big.stream.write_all(&http::client_frame(1, &vec![b'x'; http::MAX_FRAME + 1]));
    assert_eq!(std::iter::from_fn(|| big.frame()).map(|(op, _)| op).find(|op| *op != 1), Some(8), "closed");
    let mut plain = ws(addr, &format!("http://{addr}"));
    plain.stream.write_all(&http::encode(1, b"{}")).unwrap();
    assert_eq!(
        std::iter::from_fn(|| plain.frame()).map(|(op, _)| op).find(|op| *op != 1),
        Some(8),
        "unmasked frames are refused"
    );
}
