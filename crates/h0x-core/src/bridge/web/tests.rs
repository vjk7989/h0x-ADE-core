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
    fixture_via(hosts, None)
}

/// The same, answering as a vps reverse forward for `public` when there is one.
fn fixture_via(hosts: Vec<String>, public: Option<&str>) -> Fixture {
    let bridge = crate::bridge::scratch();
    let dir = bridge.dir.clone();
    let (notices, _) = tokio::sync::broadcast::channel(16);
    let web = Web {
        bridge: Mutex::new(bridge),
        core: Box::new(fake_core),
        hosts,
        key: e2e::ServerKey::generate(),
        notices,
        public: public.map(String::from),
        unauthenticated: Default::default(),
        strikes: Default::default(),
        pair_strikes: Default::default(),
    };
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
        self.paired_at(T)
    }

    /// The same, paired at `now`: sessions are checked against the real clock on a live socket.
    fn paired_at(&self, now: u64) -> (Conn, String) {
        let mut conn = phone();
        let code = self.code(now);
        let step = self.say(&mut conn, "pair", json!({"token": code, "label": "phone"}), now);
        let session = step.out[0]["data"]["session"].as_str().unwrap().to_string();
        (conn, session)
    }
}

/// Two devices' long-term keys (32-byte secrets).
const PHONE: [u8; 32] = [7; 32];
const STRANGER: [u8; 32] = [9; 32];

/// A connection that came through the handshake with the phone's device key.
fn phone() -> Conn {
    with_device(&PHONE)
}

/// A connection whose hello carried the public half of `secret`.
fn with_device(secret: &[u8; 32]) -> Conn {
    let pending = e2e::client_start(secret).unwrap();
    let key = e2e::ServerKey::generate();
    Conn { device_key: Some(e2e::server_accept(pending.hello(), &key).unwrap().device), ..Conn::default() }
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
    let mut conn = phone();
    let refused = f.say(&mut conn, "pair", json!({"token": "WRONGONE"}), T);
    assert_eq!((names(&refused), refused.close), (vec!["error"], true));
    assert_eq!(refused.out[0]["data"]["message"], "wrong or expired code");

    let code = f.code(T);
    let late = f.say(&mut phone(), "pair", json!({"token": code}), T + 5 * 60_000);
    assert!(late.close, "5 minutes for a web link");
    let code = f.code(T);
    let step = f.say(&mut conn, "pair", json!({"token": code.to_lowercase(), "label": "  Vijay's phone  "}), T);
    assert_eq!((names(&step), step.close), (vec!["paired", "snapshot"], false));
    let session = step.out[0]["data"]["session"].as_str().unwrap();
    assert_eq!(session.len(), 64);
    assert_eq!(step.out[0]["data"]["device"], "Vijay's phone");
    assert_eq!(step.out[1]["data"]["lines"], json!(["t2 · claude · /w · blocked"]));
    assert!(f.say(&mut phone(), "pair", json!({"token": code}), T).close, "single use");

    let bridge = f.web.bridge.lock().unwrap();
    let me = bridge.identities().unwrap().into_iter().find(|i| i.transport == "web").unwrap();
    assert_eq!(me.label, "Vijay's phone");
    let state = std::fs::read_to_string(f.dir.join("state.json")).unwrap();
    assert!(!state.contains(session), "only its hash is kept");
    assert!(state.contains(&sha::hex(&sha::sha256(session.as_bytes()))));
}

#[test]
fn guessing_tokens_disables_pairing_from_that_source() {
    let f = fixture(Vec::new());
    let code = f.code(T);
    let guesser = || Conn { peer: Some("192.168.1.7".parse().unwrap()), ..phone() };
    for n in 0..5 {
        assert!(f.say(&mut guesser(), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    let blocked = f.say(&mut guesser(), "pair", json!({"token": code}), T + 10);
    assert!(blocked.out[0]["data"]["message"].as_str().unwrap().contains("too many"), "{blocked:?}");
}

#[test]
fn sessions_authenticate_until_revoked_and_obey_the_lock() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    let mut conn = phone();
    assert!(f.say(&mut conn, "command", json!({"cmd": "list"}), T).close, "pair or auth first");
    assert!(!f.say(&mut conn, "ping", Value::Null, T).close, "a ping is fine before");
    for bad in
        [json!({}), json!({"session": ""}), json!({"session": "0".repeat(64)}), json!({"session": session[..63]})]
    {
        let step = f.say(&mut phone(), "auth", bad.clone(), T);
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
    assert_eq!(f.web.notice_for(&phone(), &notice), None, "not to strangers");

    f.web.bridge.lock().unwrap().unpair(None).unwrap();
    let revoked = f.say(&mut conn, "command", json!({"cmd": "list"}), T);
    assert_eq!((revoked.out[0]["data"]["kind"].as_str(), revoked.close), (Some("not_paired"), true));
    assert_eq!(f.web.notice_for(&conn, &notice), None);
    assert!(f.say(&mut phone(), "auth", json!({"session": session}), T).close, "revoked for good");
    let state = std::fs::read_to_string(f.dir.join("state.json")).unwrap();
    assert!(state.contains("\"sessions\": {}"), "its hash went too: {state}");
    assert!(state.contains("\"devices\": {}"), "its device key went too: {state}");
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
    chan: Option<e2e::Channel>,
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
    Client { stream, chan: None }
}

/// A WebSocket that has done the hello exchange as the device with this secret.
fn ws_secure(f: &Fixture, addr: SocketAddr, device: &[u8; 32]) -> Client {
    let mut client = ws(addr, &format!("http://{addr}"));
    client.secure(f, device);
    client
}

impl Client {
    /// One message: sealed once there is a handshake, a legacy clear text frame before it.
    fn send(&mut self, name: &str, data: Value) {
        let text = json!({"name": name, "data": data}).to_string();
        if self.chan.is_some() {
            self.send_sealed(text.as_bytes());
        } else {
            self.stream.write_all(&http::client_frame(1, text.as_bytes())).unwrap();
        }
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

    /// The next message other than a heartbeat; None once closed. Every message
    /// is one sealed binary frame: anything else fails the test.
    fn recv(&mut self) -> Option<Value> {
        loop {
            let (opcode, payload) = self.frame()?;
            if opcode == 8 {
                return None;
            }
            assert_eq!(opcode, 2, "a message that is not sealed: {payload:?}");
            let plain = self.chan.as_mut().expect("a handshake first").open(&payload).unwrap();
            let msg: Value = serde_json::from_slice(&plain).unwrap();
            if msg["name"] != "heartbeat" {
                return Some(msg);
            }
        }
    }

    /// The hello exchange, as the page does it, from the device with this secret.
    fn secure(&mut self, f: &Fixture, device: &[u8; 32]) {
        let pending = e2e::client_start(device).unwrap();
        self.stream.write_all(&http::client_frame(1, pending.hello())).unwrap();
        let (opcode, hello) = self.frame().unwrap();
        assert_eq!(opcode, 1, "the server hello is the one text frame");
        self.chan = Some(pending.finish(&hello, &f.web.key.fp()).unwrap());
    }

    /// Sends `frame` after sealing it, as if it were a message.
    fn send_sealed(&mut self, plain: &[u8]) {
        let sealed = self.chan.as_mut().expect("a handshake first").seal(plain).unwrap();
        self.stream.write_all(&http::client_frame(2, &sealed)).unwrap();
    }

    /// True when the server hangs up (a close frame, then nothing) without any
    /// message in between that is not sealed.
    fn hung_up(&mut self) -> bool {
        let last = std::iter::from_fn(|| self.frame()).find(|(op, _)| *op != 2);
        last.is_some_and(|(op, _)| op == 8) && self.frame().is_none()
    }
}

#[test]
fn a_phone_pairs_reconnects_commands_and_hears_notices_over_a_websocket() {
    let (f, addr) = start();
    let mut phone = ws_secure(&f, addr, &PHONE);
    phone.send("pair", json!({"token": f.code(crate::runtime::now_ms()), "label": "phone"}));
    let paired = phone.recv().unwrap();
    assert_eq!(paired["name"], "paired");
    assert_eq!(phone.recv().unwrap()["name"], "snapshot");
    let session = paired["data"]["session"].as_str().unwrap().to_string();
    drop(phone);

    let mut phone = ws_secure(&f, addr, &PHONE);
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
    let pong = std::iter::from_fn(|| phone.frame()).find(|(op, _)| *op == 10).unwrap();
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
fn the_page_and_its_assets_are_served_whole_with_the_security_headers_and_nothing_else_is() {
    let (_f, addr) = start();
    let ask = |method: &str, path: &str| request(addr, &format!("{method} {path} HTTP/1.1\nHost: {addr}\n\n"));
    for (path, kind, content) in [
        ("/", "text/html", PAGE),
        ("/app.js", "text/javascript", SCRIPT),
        ("/style.css", "text/css", STYLE),
        ("/noble.js", "text/javascript", NOBLE),
        ("/app.js?v=2", "text/javascript", SCRIPT),
        ("/style.css?", "text/css", STYLE),
        ("/app.js#top", "text/javascript", SCRIPT),
    ] {
        let answer = ask("GET", path);
        let (head, body) = answer.split_once("\r\n\r\n").unwrap_or_default();
        assert!(head.starts_with("HTTP/1.1 200 "), "{path}: {head}");
        for header in [
            format!("Content-Type: {kind}; charset=utf-8"),
            format!("Content-Length: {}", content.len()),
            "Content-Security-Policy: default-src 'self'".to_string(),
            "X-Content-Type-Options: nosniff".to_string(),
            "X-Frame-Options: DENY".to_string(),
            "Referrer-Policy: no-referrer".to_string(),
            "Cache-Control: no-store".to_string(),
        ] {
            assert!(head.contains(&format!("\r\n{header}")), "{path} lacks {header}: {head}");
        }
        assert_eq!(body, content, "{path}");
    }
    for path in [
        "/style.css/",
        "/app.js/",
        "/APP.JS",
        "/Style.css",
        "/app.js.map",
        "/NOBLE.JS",
        "/noble.js/",
        "/style.css.map",
        "/static/app.js",
        "/./app.js",
        "/../app.js",
        "//app.js",
        "/app%2Ejs",
        "/style.css%00",
        "/page.html",
        "/assets.rs",
        "/favicon.ico",
        "/C:\\app.js",
        "/..\\app.js",
    ] {
        assert!(ask("GET", path).starts_with("HTTP/1.1 404 "), "{path}");
    }
    assert!(ask("GET", "app.js").starts_with("HTTP/1.1 400 "), "a path without its slash");
    for method in ["HEAD", "POST", "OPTIONS"] {
        for path in ["/app.js", "/style.css", "/noble.js", "/app.js?v=2"] {
            assert!(ask(method, path).starts_with("HTTP/1.1 405 "), "{method} {path}");
        }
    }
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
        let mut conn = phone();
        let step = f.say(&mut conn, name, json!({"cmd": "list"}), T);
        assert_eq!((names(&step), step.close), (vec!["error"], true), "{name:?}");
        assert_eq!(step.out[0]["data"]["message"], "pair or auth first", "{name:?}");
        assert!(conn.device.is_none(), "{name:?}");
    }
    for raw in ["", "[]", "\"pair\"", "null", "{\"name\": 5}", "{\"data\": {}}", "{\"name\": \"auth\"", "\u{0}"] {
        let step = f.web.incoming(&mut phone(), raw, T);
        assert!(step.close && names(&step) == ["error"], "{raw:?}: {step:?}");
    }
    let pong = f.web.incoming(&mut phone(), r#"{"name": "ping"}"#, T);
    assert_eq!((names(&pong), pong.close), (vec!["pong"], false), "no data needed");
    assert!(f.say(&mut phone(), "pair", Value::Null, T).close, "a pair without a code");
}

#[test]
fn an_authenticated_socket_keeps_its_device() {
    let f = fixture(Vec::new());
    let (_, first) = f.paired();
    let (_, second) = f.paired();
    let mut conn = phone();
    assert!(!f.say(&mut conn, "auth", json!({"session": first}), T).close);
    let me = conn.device.clone().unwrap().user_id;
    let again = f.say(&mut conn, "auth", json!({"session": second}), T);
    assert_eq!((names(&again), again.close), (vec!["error"], false), "a second auth");
    let code = f.code(T);
    let pair = f.say(&mut conn, "pair", json!({"token": code}), T);
    assert_eq!((names(&pair), pair.close), (vec!["error"], false), "a pair after auth");
    assert_eq!(conn.device.map(|d| d.user_id), Some(me), "still the first device");
    let other = f.say(&mut phone(), "pair", json!({"token": code}), T);
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
        let step = f.say(&mut phone(), "auth", json!({"session": bad}), T);
        assert_eq!((names(&step), step.close), (vec!["error"], true), "{bad}");
    }
    assert!(!f.say(&mut phone(), "auth", json!({"session": session}), T).close, "the exact token works");
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
        let step = f.say(&mut phone(), "pair", json!({"token": f.code(T), "label": given}), T);
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
    let (f, addr) = start();
    let origin = format!("http://{addr}");
    let huge = vec![0x81, 0xff, 0x80, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4];
    let bye = [&1000u16.to_be_bytes()[..], b"bye"].concat();
    for (what, frame) in [
        ("binary before the hello", http::client_frame(2, b"hi")),
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
    let mut early = ws_secure(&f, addr, &PHONE);
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
    frames_from(&heard[heard.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..])
}

fn frames_from(heard: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut at = 0;
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

async fn read_frame(rd: &mut tokio::net::tcp::OwnedReadHalf) -> (u8, Vec<u8>) {
    let (b0, b1) = (rd.read_u8().await.unwrap(), rd.read_u8().await.unwrap());
    let len = match b1 {
        126 => usize::from(rd.read_u16().await.unwrap()),
        n => usize::from(n),
    };
    let mut payload = vec![0; len];
    rd.read_exact(&mut payload).await.unwrap();
    (b0 & 0x0f, payload)
}

/// Like [`on_a_paused_clock`], with a device that says hello and then `first`
/// sealed, as the page does. What the server said after its hello, each sealed
/// message opened, and how much (virtual) time that took.
fn on_a_paused_clock_secure(
    f: &Fixture,
    listener: std::net::TcpListener,
    device: &[u8; 32],
    first: &Value,
) -> (Vec<(u8, Vec<u8>)>, Duration) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
    runtime.block_on(async {
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = client.into_split();
        let pending = e2e::client_start(device).unwrap();
        wr.write_all(&[upgrade_head(addr), http::client_frame(1, pending.hello())].concat()).await.unwrap();
        server.readable().await.unwrap();
        let start = tokio::time::Instant::now();
        let served = tokio::spawn(connection(server, f.web.clone()));
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(rd.read_u8().await.unwrap());
        }
        let (opcode, hello) = read_frame(&mut rd).await;
        assert_eq!(opcode, 1);
        let mut chan = pending.finish(&hello, &f.web.key.fp()).unwrap();
        let sealed = chan.seal(first.to_string().as_bytes()).unwrap();
        wr.write_all(&http::client_frame(2, &sealed)).await.unwrap();
        let mut heard = Vec::new();
        rd.read_to_end(&mut heard).await.unwrap();
        let took = start.elapsed();
        served.await.unwrap();
        drop(wr);
        let frames = frames_from(&heard)
            .into_iter()
            .map(|(op, payload)| if op == 2 { (op, chan.open(&payload).unwrap()) } else { (op, payload) })
            .collect();
        (frames, took)
    })
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
    assert_eq!(frames.len(), 1, "no heartbeat and nothing else is sent in the clear before the hello: {frames:?}");
    assert!(took >= AUTH && took <= AUTH + HEARTBEAT, "{took:?}");
}

#[test]
fn an_authenticated_socket_hears_heartbeats_and_closes_when_idle() {
    let (f, listener) = unserved();
    let (_, session) = f.paired_at(crate::runtime::now_ms());
    let auth = json!({"name": "auth", "data": {"session": session}});
    let (frames, took) = on_a_paused_clock_secure(&f, listener, &PHONE, &auth);
    let said: Vec<String> = frames
        .iter()
        .filter(|(op, _)| *op == 2)
        .map(|(_, text)| serde_json::from_slice::<Value>(text).unwrap()["name"].as_str().unwrap().to_string())
        .collect();
    assert!(frames.iter().all(|(op, _)| matches!(op, 2 | 8)), "nothing in the clear: {frames:?}");
    let replies: Vec<&str> = said.iter().map(String::as_str).filter(|name| *name != "heartbeat").collect();
    assert_eq!(replies, ["hello", "snapshot"], "{said:?}");
    let beats = said.iter().filter(|name| *name == "heartbeat").count() as u32;
    assert!(beats >= IDLE.as_secs() as u32 / HEARTBEAT.as_secs() as u32, "every 20 s: {said:?}");
    assert_eq!(frames.last(), Some(&(8, 1001u16.to_be_bytes().to_vec())), "going away");
    assert!(took >= IDLE && took <= IDLE + HEARTBEAT, "{took:?}");
}

#[test]
fn every_refusal_closes_with_its_own_code() {
    let (f, addr) = start();
    let origin = format!("http://{addr}");
    let masked = |first: u8| vec![first, 0x80, 0, 0, 0, 0];
    for (what, frame, code) in [
        ("unmasked", http::encode(1, b"{}"), 1002u16),
        ("reserved bits", masked(0xc1), 1002),
        ("fragmented", masked(0x01), 1002),
        ("continuation without a start", http::client_frame(0, b"hi"), 1002),
        ("unknown opcode", http::client_frame(3, b"hi"), 1002),
        ("binary before the hello", http::client_frame(2, b"hi"), 1008),
        ("invalid UTF-8", http::client_frame(1, &[0xff]), 1007),
        ("too big", http::client_frame(1, &vec![b'x'; http::MAX_FRAME + 1]), 1009),
    ] {
        let mut client = ws(addr, &origin);
        let _ = client.stream.write_all(&frame);
        let close = std::iter::from_fn(|| client.frame()).find(|(op, _)| *op != 1);
        assert_eq!(close, Some((8, code.to_be_bytes().to_vec())), "{what}");
    }
    let mut early = ws_secure(&f, addr, &PHONE);
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
    let (_, session) = f.paired_at(crate::runtime::now_ms());
    let auth = json!({"name": "auth", "data": {"session": session}});
    let (frames, took) = on_a_paused_clock_secure(&f, listener, &PHONE, &auth);
    assert_eq!(frames.last(), Some(&(8, 1001u16.to_be_bytes().to_vec())));
    assert!(took >= IDLE && took <= Duration::from_secs(61), "{took:?}");
}

#[test]
fn five_failed_auths_from_one_address_block_it_and_are_logged_without_the_token() {
    let f = fixture(Vec::new());
    let (_, good) = f.paired();
    let ip: IpAddr = "192.168.1.7".parse().unwrap();
    let peer = || Conn { peer: Some(ip), ..phone() };
    for n in 0..5 {
        let step = f.say(&mut peer(), "auth", json!({"session": "secret-guess"}), T + n);
        assert!(step.close, "a failed auth closes");
    }
    let blocked = f.say(&mut peer(), "auth", json!({"session": good}), T + 10);
    assert!(blocked.close && blocked.out[0]["data"]["message"].as_str().unwrap().contains("too many"), "{blocked:?}");
    let mut other = Conn { peer: Some("192.168.1.8".parse().unwrap()), ..phone() };
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
        let step = f.say(&mut phone(), "pair", json!({"token": f.code(T), "label": given}), T);
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
        let step = f.say(&mut phone(), "pair", json!({"token": blank}), T);
        assert!(step.close && step.out[0]["data"]["message"].as_str().unwrap().contains("usage"), "{step:?}");
    }
    assert!(f.say(&mut phone(), "pair", json!({}), T).close);
    for n in 0..4 {
        assert!(f.say(&mut phone(), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    let paired = f.say(&mut phone(), "pair", json!({"token": code}), T + 5);
    assert_eq!(paired.out[0]["name"], "paired", "four real guesses, not five");
}

#[test]
fn qr_json_is_a_square_of_ones_and_zeros_and_refuses_empty_or_huge_text() {
    let q = qr_json("http://127.0.0.1:7777/#pair=ABCD2345").unwrap();
    assert_eq!(q["size"], 29);
    let rows: Vec<&str> = q["rows"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(rows.len(), 29);
    assert!(rows.iter().all(|r| r.len() == 29 && r.bytes().all(|b| b == b'0' || b == b'1')));
    assert_eq!(&rows[0][..7], "1111111");
    assert_eq!(&rows[1][..7], "1000001");
    let long = qr_json(&"x".repeat(2000)).unwrap();
    assert_eq!(long["rows"].as_array().unwrap().len() as u64, long["size"].as_u64().unwrap());
    assert_eq!(qr_json("").unwrap_err().kind, "invalid_params");
    assert_eq!(qr_json(&"x".repeat(3000)).unwrap_err().message, "that text is too long for a QR code");
    assert_eq!(qr(&"x".repeat(3000)), "", "no drawing for text that does not fit");
}

#[test]
fn the_ready_line_has_exactly_five_keys_with_nulls_for_what_is_absent() {
    let plain = ready_line("http://127.0.0.1:7777/", Some("http://127.0.0.1:7777/#pair=ABCD2345"), None, None, None);
    assert_eq!(
        plain,
        json!({"url": "http://127.0.0.1:7777/", "pair": "http://127.0.0.1:7777/#pair=ABCD2345", "tunnel": null, "via": null, "warning": null})
    );
    let all = ready_line(
        "http://10.0.0.5:7777/",
        None,
        Some("https://a.trycloudflare.com/"),
        Some("h0x.example.com"),
        Some("careful"),
    );
    assert_eq!(all["pair"], Value::Null);
    assert_eq!(
        (all["tunnel"].as_str(), all["via"].as_str()),
        (Some("https://a.trycloudflare.com/"), Some("h0x.example.com"))
    );
    assert_eq!(all["warning"], "careful");
    assert_eq!(all.as_object().unwrap().len(), 5);
    assert!(!all.to_string().contains('\n'), "one line");
}

#[test]
fn qr_json_takes_one_character_multibyte_text_and_a_newline_up_to_the_exact_byte_capacity() {
    let size = |text: &str| qr_json(text).map(|q| q["size"].as_u64().unwrap());
    assert_eq!(size("a"), Ok(21), "one character: version 1");
    let one = qr_json("a").unwrap();
    let rows: Vec<&str> = one["rows"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(rows.len(), 21);
    assert!(rows.iter().all(|r| r.len() == 21), "no quiet zone");
    assert_eq!(&rows[20][..7], "1111111", "the bottom-left finder starts at the edge");

    let line = qr_json("a\nb").unwrap();
    assert_eq!(line["size"], 21, "a newline is just a byte");
    assert!(line["rows"].as_array().unwrap().iter().all(|r| !r.as_str().unwrap().contains('\n')));
    assert!(size("héllo 🙂 ünïcode").is_ok());

    // Version 40 at medium correction holds 2331 bytes.
    assert_eq!(size(&"x".repeat(2331)), Ok(177), "exactly at capacity");
    let over = qr_json(&"x".repeat(2332)).unwrap_err();
    assert_eq!((over.kind.as_str(), over.message.as_str()), ("invalid_params", "that text is too long for a QR code"));
    // Capacity counts UTF-8 bytes, not characters: four bytes per emoji.
    assert_eq!(size(&"🙂".repeat(582)), Ok(177), "2328 bytes");
    assert_eq!(qr_json(&"🙂".repeat(583)).unwrap_err().message, "that text is too long for a QR code", "2332 bytes");
    assert_eq!(qr(&"🙂".repeat(583)), "");
    assert_eq!(qr_json("").unwrap_err().message, "no text to encode");
}

#[test]
fn qr_draws_terminal_blocks_two_module_rows_per_line_inside_a_quiet_zone() {
    let drawn = qr("a");
    let lines: Vec<&str> = drawn.lines().collect();
    assert_eq!(lines.len(), 13, "21 modules and a 2-module quiet zone, two rows per line");
    assert!(lines.iter().all(|l| l.chars().count() == 25), "{drawn}");
    assert!(drawn.chars().all(|c| matches!(c, ' ' | '▀' | '▄' | '█' | '\n')), "{drawn}");
    assert!(lines[0].chars().all(|c| c == '█'), "the quiet zone is light, drawn as full blocks");
}

#[test]
fn the_ready_line_keeps_null_pair_and_tunnel_when_locked_or_the_tunnel_failed() {
    let url = "http://127.0.0.1:7777/";
    for (line, via) in [
        (ready_line(url, None, None, None, None), Value::Null),
        (ready_line(url, None, None, Some("h0x.example.com"), None), json!("h0x.example.com")),
    ] {
        let text = line.to_string();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        let keys = parsed.as_object().unwrap();
        assert_eq!(keys.len(), 5, "{text}");
        for key in ["url", "pair", "tunnel", "via", "warning"] {
            assert!(keys.contains_key(key), "{key} is present even when null: {text}");
        }
        assert_eq!((&parsed["url"], &parsed["pair"], &parsed["tunnel"]), (&json!(url), &Value::Null, &Value::Null));
        assert_eq!(parsed["via"], via);
        assert!(!text.contains('\n'), "one line");
    }
}

#[test]
fn a_session_is_good_only_from_the_device_key_it_paired_with() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    let state = std::fs::read_to_string(f.dir.join("state.json")).unwrap();
    assert!(state.contains(&e2e::b64(&phone().device_key.unwrap())), "the device key is kept: {state}");

    let ip: IpAddr = "192.168.1.7".parse().unwrap();
    for n in 0..5 {
        let mut thief = Conn { peer: Some(ip), ..with_device(&STRANGER) };
        let step = f.say(&mut thief, "auth", json!({"session": session}), T + n);
        assert!(step.close && thief.device.is_none(), "{step:?}");
        assert!(step.out[0]["data"]["message"].as_str().unwrap().contains("pair again"), "{step:?}");
    }
    let mut right_key_same_address = Conn { peer: Some(ip), ..phone() };
    let blocked = f.say(&mut right_key_same_address, "auth", json!({"session": session}), T + 10);
    assert!(blocked.out[0]["data"]["message"].as_str().unwrap().contains("too many"), "counted: {blocked:?}");
    let log = std::fs::read_to_string(f.dir.join("audit.jsonl")).unwrap();
    assert_eq!(log.matches("\"command\":\"auth\"").count(), 5, "{log}");
    assert!(!log.contains(&session), "{log}");
    assert_eq!(names(&f.say(&mut phone(), "auth", json!({"session": session}), T + 10)), ["hello", "snapshot"]);
}

#[test]
fn a_session_from_before_device_keys_is_refused_and_the_old_state_still_loads() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    let path = f.dir.join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(state.as_object_mut().unwrap().remove("devices").is_some());
    std::fs::write(&path, state.to_string()).unwrap();
    assert_eq!(f.web.bridge.lock().unwrap().identities().unwrap().len(), 1, "an old state.json still loads");

    let old = f.say(&mut phone(), "auth", json!({"session": session}), T);
    assert!(old.close && old.out[0]["data"]["message"].as_str().unwrap().contains("pair again"), "{old:?}");
    let code = f.code(T);
    let again = f.say(&mut phone(), "pair", json!({"token": code}), T);
    assert_eq!(names(&again), ["paired", "snapshot"], "pairing again works");
    let fresh = again.out[0]["data"]["session"].as_str().unwrap();
    assert_eq!(names(&f.say(&mut phone(), "auth", json!({"session": fresh}), T)), ["hello", "snapshot"]);
}

#[test]
fn a_connection_without_a_device_key_cannot_pair_or_auth_and_keeps_its_code() {
    let f = fixture(Vec::new());
    let code = f.code(T);
    let mut bare = Conn::default();
    let step = f.say(&mut bare, "pair", json!({"token": code}), T);
    assert!(step.close && step.out[0]["data"]["message"] == "no device key", "{step:?}");
    assert!(f.say(&mut bare, "auth", json!({"session": "0".repeat(64)}), T).close);
    assert_eq!(names(&f.say(&mut phone(), "pair", json!({"token": code}), T)), ["paired", "snapshot"]);
}

#[test]
fn links_carry_the_servers_fingerprint_and_it_survives_a_restart() {
    let bridge = crate::bridge::scratch();
    let key = server_key(&bridge).unwrap();
    assert_eq!(key.fp().len(), 43, "base64url of a SHA-256");
    assert_eq!(
        pair_link("http://192.168.1.20:8377", "ABCD2345", &key),
        format!("http://192.168.1.20:8377/#pair=ABCD2345&fp={}", key.fp())
    );
    let restarted = Bridge::at(bridge.dir.clone(), Default::default());
    assert_eq!(server_key(&restarted).unwrap().fp(), key.fp(), "stable across restarts");

    // A damaged entry is replaced (and stays replaced), never used.
    bridge.secret_put("web.key", b"short").unwrap();
    let fresh = server_key(&bridge).unwrap();
    assert_ne!(fresh.fp(), key.fp());
    assert_eq!(server_key(&bridge).unwrap().fp(), fresh.fp());
    bridge.secret_put("web.key", &[0; 32]).unwrap();
    assert_ne!(server_key(&bridge).unwrap().fp(), fresh.fp(), "zero is no key");
    let _ = bridge.secret_del("web.key");
    let _ = std::fs::remove_dir_all(&bridge.dir);
}

#[test]
fn a_plaintext_legacy_client_is_closed_and_its_code_is_not_spent() {
    let (f, addr) = start();
    let code = f.code(crate::runtime::now_ms());
    let mut old = ws(addr, &format!("http://{addr}"));
    old.send("pair", json!({"token": code, "label": "old"}));
    assert_eq!(old.frame(), Some((8, 1008u16.to_be_bytes().to_vec())), "closed, with not one byte in the clear");
    assert!(old.frame().is_none());

    let mut phone = ws_secure(&f, addr, &PHONE);
    phone.send("pair", json!({"token": code}));
    assert_eq!(phone.recv().unwrap()["name"], "paired", "the code was not consumed");
}

#[test]
fn after_the_handshake_only_sealed_binary_frames_are_heard() {
    let (f, addr) = start();
    let text = http::client_frame(1, br#"{"name":"ping"}"#);
    let second_hello = e2e::client_start(&PHONE).unwrap().hello().to_vec();
    let mut other_channel = ws_secure(&f, addr, &STRANGER).chan.take().unwrap();
    let junk = other_channel.seal(br#"{"name":"ping"}"#).unwrap();
    for (what, frame) in [
        ("a text frame", text),
        ("a second hello", http::client_frame(1, &second_hello)),
        ("a frame sealed under another key", http::client_frame(2, &junk)),
    ] {
        let mut client = ws_secure(&f, addr, &PHONE);
        client.stream.write_all(&frame).unwrap();
        assert!(client.hung_up(), "{what}");
    }

    let mut client = ws_secure(&f, addr, &PHONE);
    let mut sealed = client.chan.as_mut().unwrap().seal(br#"{"name":"ping"}"#).unwrap();
    let last = sealed.len() - 1;
    sealed[last] ^= 1;
    client.stream.write_all(&http::client_frame(2, &sealed)).unwrap();
    assert!(client.hung_up(), "a tampered frame");

    let mut client = ws_secure(&f, addr, &PHONE);
    let ping = client.chan.as_mut().unwrap().seal(br#"{"name":"ping"}"#).unwrap();
    client.stream.write_all(&http::client_frame(2, &ping)).unwrap();
    assert_eq!(client.recv().unwrap()["name"], "pong");
    client.stream.write_all(&http::client_frame(2, &ping)).unwrap();
    assert!(client.hung_up(), "a replayed frame");

    let mut client = ws_secure(&f, addr, &PHONE);
    client.send_sealed(&[0xff, 0xfe]);
    assert!(client.hung_up(), "sealed bytes that are not UTF-8");
    let mut client = ws_secure(&f, addr, &PHONE);
    client.send_sealed(b"not json");
    assert_eq!(client.recv().unwrap()["data"]["message"], "not JSON", "a sealed refusal is itself sealed");
    assert!(client.hung_up());
}

#[test]
fn a_first_frame_that_is_not_a_small_good_hello_closes_the_socket() {
    let (_f, addr) = start();
    let hello = String::from_utf8(e2e::client_start(&PHONE).unwrap().hello().to_vec()).unwrap();
    let padded = format!("{},\"x\":\"{}\"}}", hello.trim_end_matches('}'), "a".repeat(MAX_HELLO));
    let swapped = hello.replace("\"v\":1", "\"v\":2");
    for (what, frame) in [
        ("an empty text", http::client_frame(1, b"")),
        ("not JSON", http::client_frame(1, b"hello")),
        ("an old-style message", http::client_frame(1, br#"{"name":"auth","data":{"session":"x"}}"#)),
        ("a hello over the cap", http::client_frame(1, padded.as_bytes())),
        ("another version", http::client_frame(1, swapped.as_bytes())),
        ("binary", http::client_frame(2, hello.as_bytes())),
    ] {
        let mut client = ws(addr, &format!("http://{addr}"));
        client.stream.write_all(&frame).unwrap();
        assert_eq!(client.frame(), Some((8, 1008u16.to_be_bytes().to_vec())), "{what}");
    }
}

#[test]
fn auth_from_another_device_key_is_refused_and_counted_over_the_socket() {
    let (f, addr) = start();
    let mut phone = ws_secure(&f, addr, &PHONE);
    phone.send("pair", json!({"token": f.code(crate::runtime::now_ms()), "label": "phone"}));
    let session = phone.recv().unwrap()["data"]["session"].as_str().unwrap().to_string();

    let mut thief = ws_secure(&f, addr, &STRANGER);
    thief.send("auth", json!({"session": session}));
    let refused = thief.recv().unwrap();
    assert_eq!(refused["name"], "error");
    assert!(refused["data"]["message"].as_str().unwrap().contains("pair again"), "{refused}");
    assert!(thief.hung_up());
    let log = std::fs::read_to_string(f.dir.join("audit.jsonl")).unwrap();
    assert_eq!(log.matches("\"command\":\"auth\"").count(), 1, "counted: {log}");

    let mut again = ws_secure(&f, addr, &PHONE);
    again.send("auth", json!({"session": session}));
    assert_eq!(again.recv().unwrap()["name"], "hello", "the paired device still gets in");
}

#[test]
fn a_hello_of_exactly_the_cap_is_answered_and_one_byte_more_closes() {
    let (f, addr) = start();
    let hello = String::from_utf8(e2e::client_start(&PHONE).unwrap().hello().to_vec()).unwrap();
    let base = hello.trim_end_matches('}');
    let padded = |len: usize| format!(r#"{base},"x":"{}"}}"#, "a".repeat(len - base.len() - 8));
    let (at_cap, over) = (padded(MAX_HELLO), padded(MAX_HELLO + 1));
    assert_eq!((at_cap.len(), over.len()), (MAX_HELLO, MAX_HELLO + 1));

    let mut client = ws(addr, &format!("http://{addr}"));
    client.stream.write_all(&http::client_frame(1, at_cap.as_bytes())).unwrap();
    let (opcode, answer) = client.frame().unwrap();
    assert_eq!(opcode, 1, "the server hello: {answer:?}");
    let answer: Value = serde_json::from_slice(&answer).unwrap();
    assert_eq!(answer["s"], e2e::b64(&f.web.key.public()), "{answer}");

    let mut client = ws(addr, &format!("http://{addr}"));
    client.stream.write_all(&http::client_frame(1, over.as_bytes())).unwrap();
    assert_eq!(client.frame(), Some((8, 1008u16.to_be_bytes().to_vec())), "one byte over the cap");
    assert!(client.frame().is_none());
}

#[test]
fn many_small_sealed_frames_keep_both_counters_in_step() {
    let (f, addr) = start();
    let mut client = ws_secure(&f, addr, &PHONE);
    for _ in 0..300 {
        client.send("ping", Value::Null);
    }
    for n in 0..300 {
        assert_eq!(client.recv().unwrap()["name"], "pong", "reply {n}");
    }
    client.send("command", json!({"cmd": "list"}));
    assert_eq!(client.recv().unwrap()["data"]["message"], "pair or auth first", "the channel still works");
}

#[test]
fn a_sealed_frame_may_fill_the_frame_cap_and_one_byte_more_closes_with_1009() {
    let (f, addr) = start();
    // The wire payload is the ciphertext then a 16-byte tag.
    let ping = |sealed_len: usize| {
        let pad = sealed_len - 16 - r#"{"name":"ping","data":""}"#.len();
        json!({"name": "ping", "data": "x".repeat(pad)}).to_string()
    };
    let mut client = ws_secure(&f, addr, &PHONE);
    let sealed = client.chan.as_mut().unwrap().seal(ping(http::MAX_FRAME).as_bytes()).unwrap();
    assert_eq!(sealed.len(), http::MAX_FRAME);
    client.stream.write_all(&http::client_frame(2, &sealed)).unwrap();
    assert_eq!(client.recv().unwrap()["name"], "pong", "exactly the cap is fine");

    let mut client = ws_secure(&f, addr, &PHONE);
    let sealed = client.chan.as_mut().unwrap().seal(ping(http::MAX_FRAME + 1).as_bytes()).unwrap();
    assert_eq!(sealed.len(), http::MAX_FRAME + 1);
    client.stream.write_all(&http::client_frame(2, &sealed)).unwrap();
    let close = std::iter::from_fn(|| client.frame()).find(|(op, _)| *op != 2);
    assert_eq!(close, Some((8, 1009u16.to_be_bytes().to_vec())), "too big");
    assert!(client.frame().is_none());
}

#[test]
fn a_hello_alone_does_not_extend_the_auth_deadline_and_nothing_is_said_in_the_clear() {
    let (f, listener) = unserved();
    let (frames, took) = on_a_paused_clock_secure(&f, listener, &PHONE, &json!({"name": "ping"}));
    assert_eq!(frames.last(), Some(&(8, 1001u16.to_be_bytes().to_vec())), "going away");
    let before = &frames[..frames.len() - 1];
    assert!(before.iter().all(|(op, _)| *op == 2), "only sealed frames after the hello: {frames:?}");
    let said: Vec<String> = before
        .iter()
        .map(|(_, plain)| serde_json::from_slice::<Value>(plain).unwrap()["name"].as_str().unwrap().to_string())
        .collect();
    let replies: Vec<&str> = said.iter().map(String::as_str).filter(|name| *name != "heartbeat").collect();
    assert_eq!(replies, ["pong"], "only the reply and sealed heartbeats: {said:?}");
    assert!(took >= AUTH && took < AUTH + Duration::from_secs(1), "the auth deadline, exactly: {took:?}");
}

#[test]
fn two_users_get_in_only_with_their_own_device_keys_and_unpairing_one_keeps_the_other() {
    let f = fixture(Vec::new());
    let pair = |conn: &mut Conn| {
        let step = f.say(conn, "pair", json!({"token": f.code(T), "label": "device"}), T);
        let data = &step.out[0]["data"];
        (data["session"].as_str().unwrap().to_string(), u32::try_from(data["id"].as_u64().unwrap()).unwrap())
    };
    let (mine, my_id) = pair(&mut phone());
    let (theirs, _) = pair(&mut with_device(&STRANGER));

    for (session, device) in [(&mine, &STRANGER), (&theirs, &PHONE)] {
        let step = f.say(&mut with_device(device), "auth", json!({"session": session}), T);
        assert!(step.close && step.out[0]["data"]["message"].as_str().unwrap().contains("pair again"), "{step:?}");
    }
    assert_eq!(names(&f.say(&mut phone(), "auth", json!({"session": mine}), T)), ["hello", "snapshot"]);
    let other = f.say(&mut with_device(&STRANGER), "auth", json!({"session": theirs}), T);
    assert_eq!(names(&other), ["hello", "snapshot"]);

    f.web.bridge.lock().unwrap().unpair(Some(my_id)).unwrap();
    let gone = f.say(&mut phone(), "auth", json!({"session": mine}), T);
    assert!(gone.close, "the unpaired device's old session, from its own key: {gone:?}");
    let other = f.say(&mut with_device(&STRANGER), "auth", json!({"session": theirs}), T);
    assert_eq!(names(&other), ["hello", "snapshot"], "the other binding stays");
    let state: Value = serde_json::from_slice(&std::fs::read(f.dir.join("state.json")).unwrap()).unwrap();
    let devices = state["devices"].as_object().unwrap();
    assert_eq!(devices.len(), 1, "{state}");
    let stranger = e2e::b64(&with_device(&STRANGER).device_key.unwrap());
    assert!(devices.values().all(|d| *d == stranger), "{state}");
}

#[test]
fn a_damaged_stored_device_key_refuses_only_its_own_session() {
    let f = fixture(Vec::new());
    let (_, first) = f.paired();
    let path = f.dir.join("state.json");
    let state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let first_user = state["devices"].as_object().unwrap().keys().next().unwrap().clone();
    let (_, second) = f.paired();

    for garbage in [json!("!!not base64!!"), json!(""), json!("AAAA")] {
        let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state["devices"][&first_user] = garbage.clone();
        std::fs::write(&path, state.to_string()).unwrap();
        assert_eq!(f.web.bridge.lock().unwrap().identities().unwrap().len(), 2, "it still loads: {garbage}");
        let refused = f.say(&mut phone(), "auth", json!({"session": first}), T);
        let message = refused.out[0]["data"]["message"].as_str().unwrap();
        assert!(refused.close && message.contains("pair again"), "{garbage}: {refused:?}");
        let fine = f.say(&mut phone(), "auth", json!({"session": second}), T);
        assert_eq!(names(&fine), ["hello", "snapshot"], "the other session is untouched: {garbage}");
    }
}

#[test]
fn a_missing_or_wrong_length_or_out_of_range_server_key_is_replaced_and_old_pins_fail() {
    let bridge = crate::bridge::scratch();
    let mut old = server_key(&bridge).unwrap();
    for damage in [None, Some(vec![1u8; 31]), Some(vec![1u8; 33]), Some(vec![1u8; 64]), Some(vec![0xff; 32])] {
        match &damage {
            None => assert!(bridge.secret_del("web.key").unwrap(), "there was one"),
            Some(bytes) => bridge.secret_put("web.key", bytes).unwrap(),
        }
        let fresh = server_key(&bridge).unwrap();
        assert_ne!(fresh.fp(), old.fp(), "{damage:?}");
        assert_eq!(server_key(&bridge).unwrap().fp(), fresh.fp(), "the new key is kept: {damage:?}");
        let pending = e2e::client_start(&PHONE).unwrap();
        let accepted = e2e::server_accept(pending.hello(), &fresh).unwrap();
        assert!(pending.finish(&accepted.server_hello, &old.fp()).is_err(), "pinned to the old key: {damage:?}");
        old = fresh;
    }
    let _ = bridge.secret_del("web.key");
    let _ = std::fs::remove_dir_all(&bridge.dir);
}

#[test]
fn a_bridge_folder_with_spaces_keeps_its_own_stable_fingerprint() {
    let root = std::env::temp_dir().join(format!("h0x core bridge {}", crate::platform::random_hex()));
    let (spaced, other) = (root.join("Program Files (x86)").join("my bridge"), root.join("my bridge 2"));
    let key = server_key(&Bridge::at(spaced.clone(), Default::default())).unwrap();
    let again = server_key(&Bridge::at(spaced.clone(), Default::default())).unwrap();
    assert_eq!(again.fp(), key.fp(), "stable across restarts");
    let neighbour = server_key(&Bridge::at(other.clone(), Default::default())).unwrap();
    assert_ne!(neighbour.fp(), key.fp(), "each bridge folder has its own key");
    for dir in [spaced, other] {
        let _ = Bridge::at(dir, Default::default()).secret_del("web.key");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A connection from `ip`.
fn from(ip: &str) -> Conn {
    Conn { peer: Some(ip.parse().unwrap()), ..phone() }
}

#[test]
fn every_way_to_reach_the_panel_but_localhost_warns_about_the_page() {
    assert_eq!(integrity_warning(false, false, false), None, "localhost only: nothing to say");
    for (lan, cloudflare, vps, path) in [
        (true, false, false, "your Wi-Fi"),
        (false, true, false, "Cloudflare"),
        (false, false, true, "your VPS"),
        (true, true, false, "your Wi-Fi, Cloudflare"),
        (true, true, true, "your Wi-Fi, Cloudflare, your VPS"),
    ] {
        let said = integrity_warning(lan, cloudflare, vps).unwrap();
        assert_eq!(
            said,
            format!(
                "messages are end-to-end encrypted, but the page itself is delivered by {path}, so an attacker who controls that path could change it"
            )
        );
    }
}

const DAY: u64 = 24 * 60 * 60_000;

#[test]
fn a_session_lasts_exactly_thirty_days_from_pairing() {
    assert_eq!(SESSION_MS, 30 * DAY);
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    let auth = |now: u64| f.say(&mut from("192.168.1.9"), "auth", json!({"session": session}), now);
    assert_eq!(names(&auth(T + 30 * DAY - 1)), ["hello", "snapshot"], "one millisecond before it turns 30 days old");
    let late = auth(T + 30 * DAY);
    assert!(late.close, "30 days old: expired");
    assert_eq!(late.out[0]["data"]["message"], "unknown or revoked session; pair again");
    assert!(auth(T + 400 * DAY).close, "and it stays expired");
    let again = f.say(&mut phone(), "pair", json!({"token": f.code(T + 31 * DAY)}), T + 31 * DAY);
    assert_eq!(names(&again), ["paired", "snapshot"], "the device pairs again and gets a fresh 30 days");
}

#[test]
fn a_session_saved_before_expiry_existed_counts_from_when_it_is_first_seen() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    let path = f.dir.join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(state.as_object_mut().unwrap().remove("session_times").is_some(), "the new field is saved");
    std::fs::write(&path, state.to_string()).unwrap();
    let auth = |now: u64| f.say(&mut from("192.168.1.9"), "auth", json!({"session": session}), now);
    let first = T + 5 * DAY;
    assert_eq!(names(&auth(first)), ["hello", "snapshot"], "an old state.json still loads and works");
    let state = std::fs::read_to_string(&path).unwrap();
    assert!(state.contains("session_times") && state.contains(&first.to_string()), "dated now, kept: {state}");
    assert_eq!(names(&auth(first + 30 * DAY - 1)), ["hello", "snapshot"]);
    assert!(auth(first + 30 * DAY).close, "30 days after it was first seen");
}

#[test]
fn wrong_pairing_codes_are_limited_per_source_and_before_the_code_is_checked() {
    let f = fixture(Vec::new());
    let code = f.code(T);
    for n in 0..5 {
        assert!(f.say(&mut from("203.0.113.5"), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    let block = T + 4 + PAIR_BLOCK_MS;
    for now in [T + 10, block - 1] {
        let refused = f.say(&mut from("203.0.113.5"), "pair", json!({"token": code}), now);
        assert!(refused.close && refused.out[0]["data"]["message"].as_str().unwrap().contains("too many"), "{now}");
    }
    let log = std::fs::read_to_string(f.dir.join("audit.jsonl")).unwrap();
    assert_eq!(log.matches("\"command\":\"pair\"").count(), 5, "a refused source is not even checked: {log}");
    let mut second = from("203.0.113.6");
    let step = f.say(&mut second, "pair", json!({"token": code}), T + 20);
    assert_eq!(names(&step), ["paired", "snapshot"], "another source still pairs, and the code was not spent");
    let again = f.say(&mut from("203.0.113.5"), "pair", json!({"token": f.code(block)}), block);
    assert_eq!(names(&again), ["paired", "snapshot"], "the block ends after ten minutes");
}

#[test]
fn a_right_code_clears_the_sources_wrong_ones() {
    let f = fixture(Vec::new());
    let code = f.code(T);
    for n in 0..4 {
        assert!(f.say(&mut from("203.0.113.5"), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    assert_eq!(names(&f.say(&mut from("203.0.113.5"), "pair", json!({"token": code}), T + 5)), ["paired", "snapshot"]);
    let code = f.code(T + 6);
    for n in 0..4 {
        assert!(f.say(&mut from("203.0.113.5"), "pair", json!({"token": "AAAAAAAA"}), T + 7 + n).close);
    }
    let step = f.say(&mut from("203.0.113.5"), "pair", json!({"token": code}), T + 12);
    assert_eq!(names(&step), ["paired", "snapshot"], "four wrong, one right, four wrong: never five in a row");
}

#[test]
fn web_failures_do_not_touch_telegram_pairing_and_the_other_way_round() {
    let f = fixture(Vec::new());
    for n in 0..20 {
        let ip = format!("203.0.113.{n}");
        assert!(f.say(&mut from(&ip), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    let bridge = f.web.bridge.lock().unwrap();
    let (code, _) = bridge.pair("telegram", T + 30).unwrap();
    assert!(bridge.pair_verify("telegram", "42", "me", &code, T + 31).is_ok(), "telegram pairs after 20 web failures");
    for n in 0..5 {
        assert!(bridge.pair_verify("telegram", "666", "", "AAAAAAAA", T + 40 + n).is_err());
    }
    assert!(bridge.pair("telegram", T + 50).is_err(), "telegram is blocked by its own failures");
    let (web_code, _) = bridge.pair("web", T + 51).unwrap();
    drop(bridge);
    let step = f.say(&mut from("203.0.113.99"), "pair", json!({"token": web_code}), T + 52);
    assert_eq!(names(&step), ["paired", "snapshot"], "web pairing is not blocked by telegram failures");
}

#[test]
fn a_full_server_refuses_the_extra_connection_with_503_and_serves_again_when_one_leaves() {
    let (_f, addr) = start();
    let held: Vec<TcpStream> = (0..MAX_CONNECTIONS).map(|_| TcpStream::connect(addr).unwrap()).collect();
    let mut extra = TcpStream::connect(addr).unwrap();
    extra.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut answer = Vec::new();
    let _ = extra.read_to_end(&mut answer);
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 503 "), "{answer:?}");
    drop(held);
    let head = format!("GET / HTTP/1.1\nHost: {addr}\n\n");
    let served = (0..50).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        request(addr, &head).starts_with("HTTP/1.1 200 ")
    });
    assert!(served, "the slots came back");
}

#[test]
fn the_warning_names_every_path_in_one_fixed_order() {
    for (lan, cloudflare, vps, path) in
        [(true, false, true, "by your Wi-Fi, your VPS, so"), (false, true, true, "by Cloudflare, your VPS, so")]
    {
        let said = integrity_warning(lan, cloudflare, vps).unwrap();
        assert!(said.contains(path), "{said}");
    }
}

/// The `n`th of many distinct sources.
fn source(n: u32) -> Option<IpAddr> {
    Some(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n)))
}

#[test]
fn the_strike_map_at_its_cap_forgets_only_the_sources_it_is_not_refusing() {
    let strikes = Strikes::default();
    for _ in 0..AUTH_STRIKES {
        strikes.add(source(0), T, PAIR_BLOCK_MS);
    }
    for n in 1..u32::try_from(MAX_SOURCES).unwrap() {
        strikes.add(source(n), T, PAIR_BLOCK_MS);
    }
    assert_eq!(strikes.0.lock().unwrap().len(), MAX_SOURCES, "full, not yet trimmed");
    let late = u32::try_from(MAX_SOURCES).unwrap();
    strikes.add(source(late), T + 1, PAIR_BLOCK_MS);
    assert_eq!(strikes.0.lock().unwrap().len(), MAX_SOURCES, "never past the cap");
    assert!(strikes.refused(source(0), T + 1), "a refused source stays refused at the cap");
    assert!(!strikes.0.lock().unwrap().contains_key(&source(late).unwrap()), "the newcomer goes untracked");
    for _ in 1..AUTH_STRIKES - 1 {
        strikes.add(source(1), T + 2, PAIR_BLOCK_MS);
    }
    assert!(!strikes.refused(source(1), T + 2), "a partial count survives the cap: 1 + 3 = 4 strikes");
    strikes.add(source(1), T + 3, PAIR_BLOCK_MS);
    assert!(strikes.refused(source(1), T + 3), "and the fifth refuses it, cap or no cap");
    assert_eq!(strikes.0.lock().unwrap().len(), MAX_SOURCES);
    // Once a block has ended it is room for a newcomer.
    strikes.add(source(late), T + PAIR_BLOCK_MS + 10, PAIR_BLOCK_MS);
    assert!(strikes.0.lock().unwrap().len() <= MAX_SOURCES);
    assert!(strikes.0.lock().unwrap().contains_key(&source(late).unwrap()), "tracked once a block ended");
}

#[test]
fn a_full_map_of_blocked_sources_stays_bounded() {
    let strikes = Strikes::default();
    for n in 0..u32::try_from(MAX_SOURCES).unwrap() + 50 {
        for _ in 0..AUTH_STRIKES {
            strikes.add(source(n), T, PAIR_BLOCK_MS);
        }
    }
    assert_eq!(strikes.0.lock().unwrap().len(), MAX_SOURCES);
    assert!(strikes.refused(source(0), T + 1));
    assert!(!strikes.refused(source(u32::try_from(MAX_SOURCES).unwrap() + 1), T + 1), "untracked, not refused");
}

#[test]
fn an_unknown_source_is_never_counted_nor_refused() {
    let strikes = Strikes::default();
    for n in 0..20 {
        strikes.add(None, T + n, PAIR_BLOCK_MS);
    }
    assert!(strikes.0.lock().unwrap().is_empty());
    assert!(!strikes.refused(None, T + 20));
    strikes.clear(None);
    strikes.clear(source(1));
}

#[test]
fn failed_auths_and_wrong_pairing_codes_are_counted_apart() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    for n in 0..5 {
        assert!(f.say(&mut from("203.0.113.7"), "auth", json!({"session": "nope"}), T + n).close);
    }
    let refused = f.say(&mut from("203.0.113.7"), "auth", json!({"session": session}), T + 10);
    assert!(refused.out[0]["data"]["message"].as_str().unwrap().contains("too many"), "{refused:?}");
    let step = f.say(&mut from("203.0.113.7"), "pair", json!({"token": f.code(T + 11)}), T + 11);
    assert_eq!(names(&step), ["paired", "snapshot"], "auth failures do not block pairing");

    f.code(T + 20);
    for n in 0..5 {
        assert!(f.say(&mut from("203.0.113.8"), "pair", json!({"token": "AAAAAAAA"}), T + 20 + n).close);
    }
    let auth = f.say(&mut from("203.0.113.8"), "auth", json!({"session": session}), T + 30);
    assert_eq!(names(&auth), ["hello", "snapshot"], "wrong codes do not block auth");
}

#[test]
fn web_and_telegram_pairing_blocks_end_independently() {
    let f = fixture(Vec::new());
    f.code(T);
    for n in 0..5 {
        assert!(f.say(&mut from("203.0.113.5"), "pair", json!({"token": "AAAAAAAA"}), T + n).close);
    }
    let web_ends = T + 4 + PAIR_BLOCK_MS;
    let chat = T + 100_000;
    let bridge = f.web.bridge.lock().unwrap();
    bridge.pair("telegram", chat).unwrap();
    for n in 0..5 {
        assert!(bridge.pair_verify("telegram", "666", "", "AAAAAAAA", chat + n).is_err());
    }
    let chat_ends = chat + 4 + crate::bridge::BLOCK_MS;
    assert!(web_ends < chat_ends);
    drop(bridge);

    let step = f.say(&mut from("203.0.113.5"), "pair", json!({"token": f.code(web_ends)}), web_ends);
    assert_eq!(names(&step), ["paired", "snapshot"], "the web block ended on its own clock");
    let bridge = f.web.bridge.lock().unwrap();
    assert!(bridge.pair("telegram", web_ends).is_err(), "telegram is still blocked");
    assert!(bridge.pair("telegram", chat_ends - 1).is_err());
    let (code, _) = bridge.pair("telegram", chat_ends).unwrap();
    assert!(bridge.pair_verify("telegram", "42", "me", &code, chat_ends + 1).is_ok(), "and telegram's on its own");
}

#[test]
fn a_session_survives_the_clock_going_back_but_still_expires_thirty_days_after_pairing() {
    let f = fixture(Vec::new());
    let (_, session) = f.paired();
    let auth = |now: u64| f.say(&mut from("192.168.1.9"), "auth", json!({"session": session}), now);
    assert_eq!(names(&auth(T / 2)), ["hello", "snapshot"], "a clock set back");
    assert_eq!(names(&auth(0)), ["hello", "snapshot"], "a clock at the epoch");
    assert_eq!(names(&auth(T + 30 * DAY - 1)), ["hello", "snapshot"]);
    assert!(auth(T + 30 * DAY).close, "going back did not move the pairing date");
}

#[test]
fn a_new_pairing_gets_a_new_thirty_days_and_unpairing_forgets_the_date() {
    let f = fixture(Vec::new());
    let (_, old) = f.paired();
    let later = T + 20 * DAY;
    let (_, session) = f.paired_at(later);
    let auth = |token: &str, now: u64| f.say(&mut from("192.168.1.9"), "auth", json!({"session": token}), now);
    assert!(auth(&old, later + 10 * DAY).close, "the first pairing's 30 days are its own");
    assert_eq!(names(&auth(&session, later + 30 * DAY - 1)), ["hello", "snapshot"]);
    assert!(auth(&session, later + 30 * DAY).close);

    assert_eq!(f.web.bridge.lock().unwrap().unpair(None).unwrap(), 2);
    let state: Value = serde_json::from_slice(&std::fs::read(f.dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["session_times"], json!({}), "{state}");
    assert!(auth(&session, later).close, "an unpaired device's session is gone");
}

const PANEL: &str = "panel.example.org";

/// The server on a loopback port, as the vps forward: `PANEL` is its public host.
fn start_via() -> (Fixture, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let f = fixture_via(vps::with_public(hosts(Ipv4Addr::LOCALHOST, addr.port()), Some(PANEL)), Some(PANEL));
    let web = f.web.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move { serve(tokio::net::TcpListener::from_std(listener).unwrap(), web).await });
    });
    (f, addr)
}

/// A WebSocket upgrade as the vps proxy sends it for `visitor`: the head of the answer, and the open stream.
fn ws_via(addr: SocketAddr, visitor: &str) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let head = format!(
        "GET /ws HTTP/1.1\r\nHost: {PANEL}\r\nOrigin: https://{PANEL}\r\nX-Forwarded-For: 6.6.6.6, {visitor}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
    let mut answer = Vec::new();
    let mut byte = [0u8];
    while !answer.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
        answer.push(byte[0]);
    }
    (String::from_utf8_lossy(&answer).into_owned(), stream)
}

#[test]
fn one_source_cannot_hold_every_slot_before_it_authenticates() {
    let (_f, addr) = start_via();
    let (a, b) = ("198.51.100.7", "198.51.100.8");
    let mut open: Vec<_> = (0..MAX_UNAUTHENTICATED_PER_SOURCE).map(|_| ws_via(addr, a)).collect();
    assert!(open.iter().all(|(head, _)| head.starts_with("HTTP/1.1 101 ")), "up to the share is served");
    let (refused, _) = ws_via(addr, a);
    assert!(refused.starts_with("HTTP/1.1 503 "), "that source is full: {refused}");
    let (other, _keep) = ws_via(addr, b);
    assert!(other.starts_with("HTTP/1.1 101 "), "another source is still served: {other}");
    // Hanging up frees its place.
    open.truncate(MAX_UNAUTHENTICATED_PER_SOURCE - 1);
    let mut served = false;
    for _ in 0..100 {
        served = ws_via(addr, a).0.starts_with("HTTP/1.1 101 ");
        if served {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(served, "a closed connection gives its place back");
}

#[test]
fn a_sources_places_are_counted_per_address_and_freed_on_drop() {
    let sources = Arc::new(Sources::default());
    let (a, b): (IpAddr, IpAddr) = ("203.0.113.5".parse().unwrap(), "2001:db8::5".parse().unwrap());
    let mut held: Vec<_> = (0..MAX_UNAUTHENTICATED_PER_SOURCE).map(|_| sources.take(a).unwrap()).collect();
    assert!(sources.take(a).is_none(), "the share is spent");
    assert!(sources.take(b).is_some(), "another address has its own");
    drop(held.pop());
    assert!(sources.take(a).is_some(), "one freed, one taken");
    drop(held);
    assert!(sources.0.lock().unwrap().is_empty(), "nothing is remembered for sources with no connection");
}

#[test]
fn a_flood_of_failures_cannot_push_real_entries_out_of_the_audit_log() {
    let f = fixture(Vec::new());
    let real = json!({"time_ms": T, "identity": "web:me", "command": "list", "result": "ok"});
    f.web.bridge.lock().unwrap().audit(real.clone()).unwrap();
    for n in 0..200u32 {
        let source = format!("203.0.{}.{}", n / 250, n % 250 + 1);
        assert!(f.say(&mut from(&source), "auth", json!({"session": "wrong"}), T + 10).close);
    }
    for n in 0..50u32 {
        let result = f.web.bridge.lock().unwrap().pair_verify("telegram", &n.to_string(), "", "AAAAAAAA", T + 20);
        assert!(result.is_err());
    }
    let read = || std::fs::read_to_string(f.dir.join("audit.jsonl")).unwrap();
    let log = read();
    assert_eq!(log.matches("\"command\":\"list\"").count(), 1, "the real entry is still there, verbatim: {log}");
    assert!(log.contains(&real.to_string()));
    let attempts =
        log.lines().filter(|l| l.contains("\"command\":\"auth\"") || l.contains("\"command\":\"pair\"")).count();
    assert_eq!(attempts, 5, "only the first few failures of the minute are lines: {log}");
    assert!(log.len() < 2_000, "the log did not grow with the flood: {} bytes", log.len());
    // The next minute: an authenticated entry brings the summary first, then itself, verbatim.
    let later = json!({"time_ms": T + 70_000, "identity": "web:me", "command": "status", "result": "ok"});
    f.web.bridge.lock().unwrap().audit(later.clone()).unwrap();
    let log = read();
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        lines[lines.len() - 2].contains("245 more failed attempts from 245 sources in the last minute"),
        "one aggregate line: {log}"
    );
    assert_eq!(lines[lines.len() - 1], later.to_string());
    // And a new minute writes its first failures verbatim again.
    assert!(f.say(&mut from("198.51.100.1"), "auth", json!({"session": "wrong"}), T + 71_000).close);
    assert_eq!(read().matches("\"command\":\"auth\"").count(), 6);
}

#[test]
fn an_open_socket_closes_at_the_next_heartbeat_once_its_session_turns_thirty_days_old() {
    let (f, listener) = unserved();
    let (_, session) = f.paired_at(crate::runtime::now_ms());
    let auth = json!({"name": "auth", "data": {"session": session}});
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
    let (frames, took) = runtime.block_on(async {
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = client.into_split();
        let pending = e2e::client_start(&PHONE).unwrap();
        wr.write_all(&[upgrade_head(addr), http::client_frame(1, pending.hello())].concat()).await.unwrap();
        server.readable().await.unwrap();
        let start = tokio::time::Instant::now();
        let served = tokio::spawn(connection(server, f.web.clone()));
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(rd.read_u8().await.unwrap());
        }
        let (_, hello) = read_frame(&mut rd).await;
        let mut chan = pending.finish(&hello, &f.web.key.fp()).unwrap();
        wr.write_all(&http::client_frame(2, &chan.seal(auth.to_string().as_bytes()).unwrap())).await.unwrap();
        // Authenticated: the snapshot has arrived. The wait spins instead of idling: an idle
        // paused runtime jumps the clock ahead, and the server would run its heartbeats before
        // the session below is aged.
        loop {
            let mut next = Box::pin(read_frame(&mut rd));
            let (op, payload) = loop {
                tokio::select! {
                    biased;
                    frame = &mut next => break frame,
                    () = tokio::task::yield_now() => {}
                }
            };
            assert_eq!(op, 2);
            let msg: Value = serde_json::from_slice(&chan.open(&payload).unwrap()).unwrap();
            if msg["name"] == "snapshot" {
                break;
            }
        }
        // The session now dates from the epoch: far more than 30 days old.
        let path = f.dir.join("state.json");
        let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for made in state["session_times"].as_object_mut().unwrap().values_mut() {
            *made = json!(0);
        }
        std::fs::write(&path, state.to_string()).unwrap();
        let mut heard = Vec::new();
        rd.read_to_end(&mut heard).await.unwrap();
        let took = start.elapsed();
        served.await.unwrap();
        drop(wr);
        (frames_from(&heard), took)
    });
    assert_eq!(frames.last(), Some(&(8, 1008u16.to_be_bytes().to_vec())), "closed by policy: {frames:?}");
    assert!(frames.iter().all(|(op, _)| *op == 8), "not even a heartbeat after it expired: {frames:?}");
    assert!(took <= HEARTBEAT + Duration::from_secs(1) && took < IDLE, "at the next heartbeat, not at idle: {took:?}");
}

#[test]
fn an_open_sessions_age_ends_exactly_at_thirty_days_and_what_cannot_be_aged_stays_open() {
    let f = fixture(Vec::new());
    let (conn, _) = f.paired_at(T);
    assert!(f.web.session_alive(&conn, T + SESSION_MS - 1), "one millisecond under thirty days");
    assert!(!f.web.session_alive(&conn, T + SESSION_MS), "exactly thirty days: closed at that heartbeat");
    assert!(!f.web.session_alive(&conn, u64::MAX), "the far future");
    assert!(f.web.session_alive(&conn, 0), "a clock gone back does not end it");
    assert!(f.web.session_alive(&phone(), u64::MAX), "a socket that never authenticated has no session to age");
    let nobody = Conn { device: Some(Sender { transport: "web".into(), user_id: "nobody".into() }), ..phone() };
    assert!(!f.web.session_alive(&nobody, T), "a user the state does not know is closed at the heartbeat");
    let path = f.dir.join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state.as_object_mut().unwrap().remove("session_times");
    std::fs::write(&path, state.to_string()).unwrap();
    assert!(f.web.session_alive(&conn, u64::MAX), "still paired, from before dates: the next auth dates it");
    std::fs::write(f.dir.join("state.json"), "not json").unwrap();
    assert!(f.web.session_alive(&conn, u64::MAX), "an unreadable state: the next auth decides");
}

#[test]
fn authenticating_gives_one_place_back_to_its_source_and_closing_after_frees_nothing_more() {
    let (f, addr) = start_via();
    let (_, session) = f.paired_at(crate::runtime::now_ms());
    let a = "198.51.100.7";
    let mut open: Vec<_> = (0..MAX_UNAUTHENTICATED_PER_SOURCE).map(|_| ws_via(addr, a)).collect();
    assert!(open.iter().all(|(head, _)| head.starts_with("HTTP/1.1 101 ")), "up to the share is served");
    let (_, stream) = open.pop().unwrap();
    let mut phone = Client { stream, chan: None };
    phone.secure(&f, &PHONE);
    phone.send("auth", json!({"session": session}));
    assert_eq!(phone.recv().unwrap()["name"], "hello");
    let (freed, _freed) = ws_via(addr, a);
    assert!(freed.starts_with("HTTP/1.1 101 "), "the authenticated socket no longer holds a place: {freed}");
    let (full, _) = ws_via(addr, a);
    assert!(full.starts_with("HTTP/1.1 503 "), "one place came back, not more: {full}");
    // The same visitor's header on a request for this machine's own host is not the forward: uncapped.
    let mut local = TcpStream::connect(addr).unwrap();
    local.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let head = format!(
        "GET /ws HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nX-Forwarded-For: {a}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    local.write_all(head.as_bytes()).unwrap();
    let mut answer = Vec::new();
    let mut byte = [0u8];
    while !answer.ends_with(b"\r\n\r\n") && local.read_exact(&mut byte).is_ok() {
        answer.push(byte[0]);
    }
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 101 "), "localhost is counted by its socket, not the header: {answer}");
    // Its place went at auth: the authenticated socket leaving gives nothing back twice.
    drop(phone);
    std::thread::sleep(Duration::from_millis(300));
    let (still, _) = ws_via(addr, a);
    assert!(still.starts_with("HTTP/1.1 503 "), "still full: {still}");
}

#[test]
fn an_unpaired_or_revoked_devices_open_socket_is_closed_at_the_next_check() {
    let f = fixture(Vec::new());
    let (one, _) = f.paired_at(T);
    let (two, _) = f.paired_at(T);
    assert!(f.web.session_alive(&one, T + 1) && f.web.session_alive(&two, T + 1));
    let id = f.web.bridge.lock().unwrap().identities().unwrap()[0].id;
    f.web.bridge.lock().unwrap().unpair(Some(id)).unwrap();
    let alive = [&one, &two].map(|c| f.web.session_alive(c, T + 2));
    assert_eq!(alive.iter().filter(|a| **a).count(), 1, "exactly the unpaired device is closed: {alive:?}");
    f.web.bridge.lock().unwrap().unpair(None).unwrap();
    assert!(!f.web.session_alive(&one, T + 3) && !f.web.session_alive(&two, T + 3), "unpair all closes all");
}

#[test]
fn the_vps_host_is_the_public_host_with_or_without_its_default_port_only() {
    let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
    for (host, via) in [
        (PANEL, true),
        ("panel.example.org:443", true),
        ("panel.example.org:80", true),
        ("PANEL.Example.ORG:443", true),
        ("panel.example.org:8377", false),
        ("panel.example.org:4430", false),
        ("panel.example.org:", false),
        ("panel.example.org.evil.test", false),
        ("127.0.0.1:443", false),
    ] {
        let req = http::Request {
            method: "GET".into(),
            path: "/ws".into(),
            headers: vec![("host".into(), host.into()), ("x-forwarded-for".into(), "198.51.100.7".into())],
        };
        let seen = tunnel::client(&req, ip("127.0.0.1"), false, Some(PANEL));
        assert_eq!(seen, if via { ip("198.51.100.7") } else { ip("127.0.0.1") }, "{host}");
    }
}
