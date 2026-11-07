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
    let web =
        Web { bridge: Mutex::new(bridge), core: Box::new(fake_core), hosts, notices, strikes: Default::default() };
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

#[test]
fn lan_mode_refuses_the_addresses_just_outside_the_private_ranges() {
    let lan = |s: &str| lan_address(Some(s.parse::<IpAddr>().unwrap()));
    for ok in
        ["10.0.0.0", "10.255.255.255", "172.16.0.0", "192.168.0.0", "192.168.255.255", "100.64.0.0", "100.127.255.255"]
    {
        assert!(lan(ok).is_ok(), "{ok}");
    }
    for bad in [
        "9.255.255.255",
        "11.0.0.0",
        "172.15.255.255",
        "172.32.0.0",
        "192.167.255.255",
        "192.169.0.0",
        "100.63.255.255",
        "100.128.0.0",
        "169.254.0.1",
        "169.254.255.254",
        "224.0.0.1",
        "255.255.255.255",
        "::ffff:192.168.1.20",
        "fd00::1",
    ] {
        let err = lan(bad).unwrap_err();
        assert!(err.contains(bad) && err.contains("not a private LAN address"), "{bad}: {err}");
    }
}

#[test]
fn before_auth_only_ping_pair_and_auth_are_heard() {
    let f = fixture(Vec::new());
    for name in ["snapshot", "command", "hello", "paired", ""] {
        let mut conn = Conn::default();
        let step = f.say(&mut conn, name, json!({"cmd": "list"}), T);
        assert_eq!((names(&step), step.close), (vec!["error"], true), "{name:?}");
        assert_eq!(step.out[0]["data"]["message"], "pair or auth first", "{name:?}");
        assert!(conn.device.is_none(), "{name:?}");
    }
    for raw in ["", "[]", "\"pair\"", "null", "{\"name\": 5}", "{\"data\": {}}", "{\"name\": \"auth\"", "\u{0}"] {
        let step = f.web.incoming(&mut Conn::default(), raw, T);
        assert!(step.close && names(&step) == ["error"], "{raw:?}: {step:?}");
    }
    let pong = f.web.incoming(&mut Conn::default(), r#"{"name": "ping"}"#, T);
    assert_eq!((names(&pong), pong.close), (vec!["pong"], false), "no data needed");
    assert!(f.say(&mut Conn::default(), "pair", Value::Null, T).close, "a pair without a code");
}

#[test]
fn an_authenticated_socket_keeps_its_device() {
    let f = fixture(Vec::new());
    let (_, first) = f.paired();
    let (_, second) = f.paired();
    let mut conn = Conn::default();
    assert!(!f.say(&mut conn, "auth", json!({"session": first}), T).close);
    let me = conn.device.clone().unwrap().user_id;
    let again = f.say(&mut conn, "auth", json!({"session": second}), T);
    assert_eq!((names(&again), again.close), (vec!["error"], false), "a second auth");
    let code = f.code(T);
    let pair = f.say(&mut conn, "pair", json!({"token": code}), T);
    assert_eq!((names(&pair), pair.close), (vec!["error"], false), "a pair after auth");
    assert_eq!(conn.device.map(|d| d.user_id), Some(me), "still the first device");
    let other = f.say(&mut Conn::default(), "pair", json!({"token": code}), T);
    assert_eq!(other.out[0]["name"], "paired", "the code was not spent");
}

#[test]
fn a_session_token_matches_only_exactly() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    for bad in [
        json!(session.to_uppercase()),
        json!(format!(" {session}")),
        json!(format!("{session}\n")),
        json!(format!("{session}0")),
        json!(&session[1..]),
        json!(null),
        json!(42),
        json!([session.clone()]),
        json!({"session": session.clone()}),
    ] {
        let step = f.say(&mut Conn::default(), "auth", json!({"session": bad}), T);
        assert_eq!((names(&step), step.close), (vec!["error"], true), "{bad}");
    }
    assert!(!f.say(&mut Conn::default(), "auth", json!({"session": session}), T).close, "the exact token works");
}

#[test]
fn command_ids_echo_as_given_and_odd_data_is_a_failed_reply() {
    let f = fixture(Vec::new());
    let (mut conn, _) = f.paired();
    for id in [json!("a"), json!(-1), json!(1.5), json!({"n": [1]}), json!(null), json!(true)] {
        let step = f.say(&mut conn, "command", json!({"cmd": "list", "id": id}), T);
        assert_eq!((&step.out[0]["data"]["id"], step.close), (&id, false), "{id}");
    }
    let missing = f.say(&mut conn, "command", json!({"cmd": "list"}), T);
    assert_eq!(missing.out[0]["data"]["id"], Value::Null, "no id: null");
    for (data, why) in [
        (json!(5), "unknown command \"\""),
        (json!("list"), "unknown command \"\""),
        (json!(["list"]), "unknown command \"\""),
        (json!({"cmd": 5}), "unknown command \"\""),
        (json!({"cmd": "LIST"}), "unknown command \"LIST\""),
        (json!({"cmd": "read"}), "missing terminal"),
        (json!({"cmd": "read", "terminal": 2}), "missing terminal"),
        (json!({"cmd": "run", "folder": "/w"}), "missing command"),
        (json!({"cmd": "confirm"}), "missing token"),
        (json!({"cmd": "keys", "terminal": "t2", "keys": "Enter"}), "missing keys"),
        (json!({"cmd": "keys", "terminal": "t2", "keys": [5]}), "unknown key 5"),
    ] {
        let step = f.say(&mut conn, "command", data.clone(), T);
        let reply = &step.out[0]["data"];
        assert_eq!(
            (reply["kind"].as_str(), reply["message"].as_str(), step.close),
            (Some("failed"), Some(why), false),
            "{data}"
        );
    }
}

#[test]
fn a_device_label_is_trimmed_to_40_characters_or_defaulted() {
    let f = fixture(Vec::new());
    let label = |given: Value| {
        let step = f.say(&mut Conn::default(), "pair", json!({"token": f.code(T), "label": given}), T);
        step.out[0]["data"]["device"].as_str().unwrap().to_string()
    };
    assert_eq!(label(json!("é".repeat(50))), "é".repeat(40), "characters, not bytes");
    assert_eq!(label(json!(format!("  {}x  ", "a".repeat(39)))), format!("{}x", "a".repeat(39)));
    for blank in [json!("   "), json!(""), json!(null), json!(7)] {
        assert_eq!(label(blank.clone()), "web device", "{blank}");
    }
}

#[test]
fn a_request_head_may_be_8_kib_and_not_a_byte_more() {
    let (_f, addr) = start();
    let head = |size: usize| {
        let start = format!("GET / HTTP/1.1\nHost: {addr}\nX: ");
        // Each \n goes out as \r\n: two more bytes before the padding.
        format!("{start}{}\n\n", "y".repeat(size - start.len() - 2))
    };
    let at = request(addr, &head(http::MAX_HEAD));
    assert!(at.starts_with("HTTP/1.1 200 "), "{}", &at[..at.len().min(200)]);
    let over = request(addr, &head(http::MAX_HEAD + 1));
    assert!(over.starts_with("HTTP/1.1 431 "), "{over}");
}

#[test]
fn binary_continuation_huge_bad_and_close_frames_end_the_socket() {
    let (_f, addr) = start();
    let origin = format!("http://{addr}");
    let huge = vec![0x81, 0xff, 0x80, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4];
    let bye = [&1000u16.to_be_bytes()[..], b"bye"].concat();
    for (what, frame) in [
        ("binary", http::client_frame(2, b"hi")),
        ("continuation", http::client_frame(0, b"hi")),
        ("64-bit length", huge),
        ("bad UTF-8", http::client_frame(1, &[0xff])),
        ("close with a reason", http::client_frame(8, &bye)),
    ] {
        let mut client = ws(addr, &origin);
        client.stream.write_all(&frame).unwrap();
        let last = std::iter::from_fn(|| client.frame()).find(|(op, _)| *op != 1);
        assert_eq!(last.map(|(op, _)| op), Some(8), "{what}");
        assert!(client.frame().is_none(), "{what}: then the connection ends");
    }
    let mut early = ws(addr, &origin);
    early.send("command", json!({"cmd": "list"}));
    assert_eq!(early.recv().unwrap()["data"]["message"], "pair or auth first");
    assert_eq!(early.frame(), Some((8, 1008u16.to_be_bytes().to_vec())), "a policy violation");
}

/// A fixture for a loopback listener that is not serving yet.
fn unserved() -> (Fixture, std::net::TcpListener) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let f = fixture(hosts(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port()));
    (f, listener)
}

fn upgrade_head(addr: SocketAddr) -> Vec<u8> {
    format!(
        "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .into_bytes()
}

/// One connection on a paused clock: the client writes `sent`, then one byte
/// of `trickle` a second, then stays silent until the server hangs up.
/// Everything the server said, and how much (virtual) time that took.
fn on_a_paused_clock(f: &Fixture, listener: std::net::TcpListener, sent: &[u8], trickle: &[u8]) -> (Vec<u8>, Duration) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
    runtime.block_on(async {
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = client.into_split();
        wr.write_all(sent).await.unwrap();
        // The bytes are there before the server's clock starts.
        server.readable().await.unwrap();
        let start = tokio::time::Instant::now();
        let served = tokio::spawn(connection(server, f.web.clone()));
        let talk = async {
            for byte in trickle {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = wr.write_all(&[*byte]).await;
            }
        };
        let mut heard = Vec::new();
        let ((), _) = tokio::join!(talk, rd.read_to_end(&mut heard));
        let took = start.elapsed();
        served.await.unwrap();
        drop(wr);
        (heard, took)
    })
}

/// The server frames after its 101 answer: opcode and payload.
fn server_frames(heard: &[u8]) -> Vec<(u8, Vec<u8>)> {
    assert!(heard.starts_with(b"HTTP/1.1 101 "), "{}", String::from_utf8_lossy(heard));
    let mut at = heard.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let mut frames = Vec::new();
    while at < heard.len() {
        let (len, head) = match heard[at + 1] {
            126 => (usize::from(u16::from_be_bytes([heard[at + 2], heard[at + 3]])), 4),
            n => (usize::from(n), 2),
        };
        frames.push((heard[at] & 0x0f, heard[at + head..at + head + len].to_vec()));
        at += head + len;
    }
    frames
}

#[test]
fn a_head_trickling_in_a_byte_a_second_still_times_out_after_10_seconds() {
    let (f, listener) = unserved();
    let (heard, took) = on_a_paused_clock(&f, listener, b"GET / HTTP/1.1\r\n", b"Host: 127");
    assert!(heard.starts_with(b"HTTP/1.1 408 "), "{}", String::from_utf8_lossy(&heard));
    assert!(took >= HEAD_TIMEOUT && took < HEAD_TIMEOUT + Duration::from_secs(1), "{took:?}");
}

#[test]
fn a_socket_that_never_authenticates_is_closed_after_the_auth_deadline() {
    let (f, listener) = unserved();
    let head = upgrade_head(listener.local_addr().unwrap());
    let (heard, took) = on_a_paused_clock(&f, listener, &head, b"");
    let frames = server_frames(&heard);
    assert_eq!(frames.last(), Some(&(8, 1001u16.to_be_bytes().to_vec())), "going away");
    assert!(frames[..frames.len() - 1].iter().all(|(op, _)| *op == 1), "only heartbeats before: {frames:?}");
    assert!(took >= AUTH && took <= AUTH + HEARTBEAT, "{took:?}");
}

#[test]
fn an_authenticated_socket_hears_heartbeats_and_closes_when_idle() {
    let (f, listener) = unserved();
    let (_, session) = f.paired();
    let auth = json!({"name": "auth", "data": {"session": session}}).to_string();
    let sent = [upgrade_head(listener.local_addr().unwrap()), http::client_frame(1, auth.as_bytes())].concat();
    let (heard, took) = on_a_paused_clock(&f, listener, &sent, b"");
    let frames = server_frames(&heard);
    let said: Vec<String> = frames
        .iter()
        .filter(|(op, _)| *op == 1)
        .map(|(_, text)| serde_json::from_slice::<Value>(text).unwrap()["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(said[..2], ["hello", "snapshot"].map(String::from)[..], "{said:?}");
    let beats = said.iter().filter(|name| *name == "heartbeat").count() as u32;
    assert!(beats >= IDLE.as_secs() as u32 / HEARTBEAT.as_secs() as u32, "every 20 s: {said:?}");
    assert_eq!(frames.last(), Some(&(8, 1001u16.to_be_bytes().to_vec())), "going away");
    assert!(took >= IDLE && took <= IDLE + HEARTBEAT, "{took:?}");
}

#[test]
fn every_refusal_closes_with_its_own_code() {
    let (_f, addr) = start();
    let origin = format!("http://{addr}");
    let masked = |first: u8| vec![first, 0x80, 0, 0, 0, 0];
    for (what, frame, code) in [
        ("unmasked", http::encode(1, b"{}"), 1002u16),
        ("reserved bits", masked(0xc1), 1002),
        ("fragmented", masked(0x01), 1002),
        ("continuation without a start", http::client_frame(0, b"hi"), 1002),
        ("unknown opcode", http::client_frame(3, b"hi"), 1002),
        ("binary", http::client_frame(2, b"hi"), 1003),
        ("invalid UTF-8", http::client_frame(1, &[0xff]), 1007),
        ("too big", http::client_frame(1, &vec![b'x'; http::MAX_FRAME + 1]), 1009),
    ] {
        let mut client = ws(addr, &origin);
        let _ = client.stream.write_all(&frame);
        let close = std::iter::from_fn(|| client.frame()).find(|(op, _)| *op != 1);
        assert_eq!(close, Some((8, code.to_be_bytes().to_vec())), "{what}");
    }
    let mut early = ws(addr, &origin);
    early.send("command", json!({"cmd": "list"}));
    assert_eq!(early.recv().unwrap()["data"]["message"], "pair or auth first");
    assert_eq!(early.frame(), Some((8, 1008u16.to_be_bytes().to_vec())), "policy");
}

#[test]
fn the_close_frame_survives_unread_data_in_the_servers_buffer() {
    let (_f, addr) = start();
    let mut client = ws(addr, &format!("http://{addr}"));
    // An unmasked header, then more bytes the server never reads on its own.
    let mut frame = http::encode(1, &vec![b'x'; 2000]);
    frame.extend_from_slice(&[0; 5000]);
    client.stream.write_all(&frame).unwrap();
    let close = std::iter::from_fn(|| client.frame()).find(|(op, _)| *op != 1);
    assert_eq!(close, Some((8, 1002u16.to_be_bytes().to_vec())));
    assert!(client.frame().is_none(), "then the server's end of the stream is shut");
}

#[test]
fn deadlines_are_exact_not_the_next_heartbeat() {
    let (f, listener) = unserved();
    let head = upgrade_head(listener.local_addr().unwrap());
    let (heard, took) = on_a_paused_clock(&f, listener, &head, b"");
    assert_eq!(server_frames(&heard).last(), Some(&(8, 1001u16.to_be_bytes().to_vec())));
    assert!(took >= AUTH && took <= Duration::from_secs(31), "{took:?}");

    let (f, listener) = unserved();
    let (_, session) = f.paired();
    let auth = json!({"name": "auth", "data": {"session": session}}).to_string();
    let sent = [upgrade_head(listener.local_addr().unwrap()), http::client_frame(1, auth.as_bytes())].concat();
    let (heard, took) = on_a_paused_clock(&f, listener, &sent, b"");
    assert_eq!(server_frames(&heard).last(), Some(&(8, 1001u16.to_be_bytes().to_vec())));
    assert!(took >= IDLE && took <= Duration::from_secs(61), "{took:?}");
}

#[test]
fn five_failed_auths_from_one_address_block_it_and_are_logged_without_the_token() {
    let f = fixture(Vec::new());
    let (_, good) = f.paired();
    let ip: IpAddr = "192.168.1.7".parse().unwrap();
    let peer = || Conn { peer: Some(ip), ..Conn::default() };
    for n in 0..5 {
        let step = f.say(&mut peer(), "auth", json!({"session": "secret-guess"}), T + n);
        assert!(step.close, "a failed auth closes");
    }
    let blocked = f.say(&mut peer(), "auth", json!({"session": good}), T + 10);
    assert!(blocked.close && blocked.out[0]["data"]["message"].as_str().unwrap().contains("too many"), "{blocked:?}");
    let mut other = Conn { peer: Some("192.168.1.8".parse().unwrap()), ..Conn::default() };
    assert_eq!(names(&f.say(&mut other, "auth", json!({"session": good}), T + 10)), ["hello", "snapshot"]);
    let after = f.say(&mut peer(), "auth", json!({"session": good}), T + 60_000 + 5);
    assert_eq!(names(&after), ["hello", "snapshot"], "refused for 60 s only");
    let log = std::fs::read_to_string(f.dir.join("audit.jsonl")).unwrap();
    assert_eq!(log.matches("\"command\":\"auth\"").count(), 5, "{log}");
    assert!(log.contains("web:192.168.1.7") && !log.contains("secret-guess") && !log.contains(&good), "{log}");
}

#[test]
fn control_characters_never_reach_a_device_label() {
    let f = fixture(Vec::new());
    let label = |given: &str| {
        let step = f.say(&mut Conn::default(), "pair", json!({"token": f.code(T), "label": given}), T);
        step.out[0]["data"]["device"].as_str().unwrap().to_string()
    };
    assert_eq!(label("\u{1b}]0;x\u{7}ph\u{1b}[31mone\u{9b}\u{7f}\n"), "]0;xph[31mone");
    assert_eq!(label(&format!("{}{}", "\u{1}".repeat(50), "a".repeat(45))), "a".repeat(40), "stripped before the cut");
    assert_eq!(label("\u{1b}\u{85}"), "web device");
}

#[test]
fn an_empty_pair_code_is_usage_and_not_a_wrong_guess() {
    let f = fixture(Vec::new());
    let code = f.code(T);
    for blank in [json!(""), json!(null), json!(" - "), json!(7)] {
        let step = f.say(&mut Conn::default(), "pair", json!({"token": blank}), T);
        assert!(step.close && step.out[0]["data"]["message"].as_str().unwrap().contains("usage"), "{step:?}");
    }
    assert!(f.say(&mut Conn::default(), "pair", json!({}), T).close);
    for n in 0..4 {
        assert!(f.say(&mut Conn::default(), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    let paired = f.say(&mut Conn::default(), "pair", json!({"token": code}), T + 5);
    assert_eq!(paired.out[0]["name"], "paired", "four real guesses, not five");
}
