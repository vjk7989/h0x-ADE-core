//! `h0x-core bridge web`: a local web panel. It serves a small page and a
//! WebSocket API at `/ws` on 127.0.0.1, or with `--lan` on this machine's
//! private LAN address. A device pairs once with a one-time link (shown as a
//! QR code) and gets a session token, stored here only as its SHA-256; every
//! later connection authenticates with it first. Commands go through the
//! bridge like any other transport: allow-list, lock, rate limit, redaction
//! and audit.
//!
//! Messages are JSON `{name, data}` both ways (see docs/bridge.md).

mod http;
mod sha;

use super::{Bridge, Command, Key, Kind, Notice, Reply, Sender};
use crate::paths::Paths;
use crate::rpc::RpcError;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The port when none is given.
pub const PORT: u16 = 8377;
/// A request head must arrive this fast.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// A WebSocket that says nothing (not even a ping) for this long is closed.
const IDLE: Duration = Duration::from_secs(60);
/// A WebSocket must pair or authenticate within this.
const AUTH: Duration = Duration::from_secs(30);
/// How often the server says it is alive.
const HEARTBEAT: Duration = Duration::from_secs(20);

const PAGE: &str = include_str!("page.html");
const SCRIPT: &str = include_str!("app.js");
const STYLE: &str = include_str!("style.css");

/// The address `--lan` binds: the private IPv4 address this machine reaches
/// the network from (home, office, or a private overlay like Tailscale's
/// 100.64.0.0/10). Never a public, loopback or link-local one.
pub fn lan_address(found: Option<IpAddr>) -> Result<Ipv4Addr, String> {
    match found {
        Some(IpAddr::V4(ip)) if ip.is_private() || (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64) => Ok(ip),
        Some(ip) => Err(format!("{ip} is not a private LAN address; --lan only binds a home or office network")),
        None => Err("no network found; join a Wi-Fi or LAN first".into()),
    }
}

/// The address this machine would reach the internet from. A UDP "connect"
/// sends nothing; it only picks the route.
fn outbound_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// The Host values this listener answers to: its own address only.
fn hosts(ip: Ipv4Addr, port: u16) -> Vec<String> {
    let mut hosts = vec![format!("{ip}:{port}")];
    if ip.is_loopback() {
        hosts.push(format!("localhost:{port}"));
    }
    hosts
}

/// `text` as a QR code for a terminal: two rows per line, light modules
/// drawn (scanners read it on dark and light terminals alike), with a quiet zone.
fn qr(text: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let Ok(code) = QrCode::encode_text(text, QrCodeEcc::Medium) else { return String::new() };
    let (quiet, size) = (2, code.size());
    let mut out = String::new();
    for y in (-quiet..size + quiet).step_by(2) {
        for x in -quiet..size + quiet {
            out.push(match (code.get_module(x, y), code.get_module(x, y + 1)) {
                (true, true) => ' ',
                (true, false) => '▄',
                (false, true) => '▀',
                (false, false) => '█',
            });
        }
        out.push('\n');
    }
    out
}

impl Bridge {
    /// Remembers a web device's session token, hashed.
    fn add_session(&self, user: &str, token: &str) -> Result<(), RpcError> {
        let mut st = self.load()?;
        st.sessions.insert(user.into(), sha::hex(&sha::sha256(token.as_bytes())));
        self.save(&st)
    }

    /// The paired web device a session token belongs to, if any. Every
    /// stored hash is compared, each in constant time.
    fn session_user(&self, token: &str) -> Result<Option<String>, RpcError> {
        let st = self.load()?;
        let hash = sha::hex(&sha::sha256(token.as_bytes()));
        let mut found = None;
        for (user, stored) in &st.sessions {
            if super::same(stored.as_bytes(), hash.as_bytes()) {
                found = Some(user.clone());
            }
        }
        Ok(found.filter(|user| st.identities.iter().any(|i| i.transport == "web" && &i.user_id == user)))
    }
}

/// A call into the core, from any thread.
type CoreCall = dyn Fn(&str, Value) -> Result<Value, RpcError> + Send + Sync;

/// Everything connections share.
pub(crate) struct Web {
    bridge: Mutex<Bridge>,
    core: Box<CoreCall>,
    hosts: Vec<String>,
    notices: tokio::sync::broadcast::Sender<Notice>,
}

/// One connection's device, once it paired or authenticated.
#[derive(Clone, Default)]
struct Conn {
    device: Option<Sender>,
}

/// What to send back for one message, and whether to hang up after.
#[derive(Debug, Default, PartialEq)]
struct Step {
    out: Vec<Value>,
    close: bool,
}

fn message(name: &str, data: Value) -> Value {
    json!({"name": name, "data": data})
}

fn refuse(why: &str) -> Step {
    Step { out: vec![message("error", json!({"message": why}))], close: true }
}

/// A reply as the client sees it; `id` is the client's, echoed.
fn reply_json(id: &Value, reply: Reply) -> Value {
    let mut data = match reply {
        Reply::Done(text) => json!({"kind": "done", "text": text}),
        Reply::NeedsConfirm { token, what } => json!({"kind": "needs_confirm", "token": token, "what": what}),
        Reply::NotPaired => json!({"kind": "not_paired"}),
        Reply::Locked => json!({"kind": "locked"}),
        Reply::RateLimited { retry_secs } => json!({"kind": "rate_limited", "retry_secs": retry_secs}),
        Reply::Failed(message) => json!({"kind": "failed", "message": message}),
    };
    data["id"] = id.clone();
    data
}

/// A client's `command` data as a bridge command.
fn command(data: &Value) -> Result<Command, String> {
    let text = |key: &str| data[key].as_str().map(String::from).ok_or_else(|| format!("missing {key}"));
    let terminal = || text("terminal");
    Ok(match data["cmd"].as_str().unwrap_or_default() {
        "list" => Command::List,
        "read" => Command::Read {
            terminal: terminal()?,
            lines: data["lines"].as_u64().map_or(20, |n| usize::try_from(n).unwrap_or(usize::MAX)),
        },
        "send" => Command::Send { terminal: terminal()?, text: text("text")? },
        "keys" => {
            let names = data["keys"].as_array().ok_or("missing keys")?;
            let keys = names.iter().map(|k| k.as_str().and_then(Key::parse).ok_or(format!("unknown key {k}")));
            Command::Keys { terminal: terminal()?, keys: keys.collect::<Result<_, _>>()? }
        }
        "approve" => Command::Approve { terminal: terminal()? },
        "deny" => Command::Deny { terminal: terminal()? },
        "interrupt" => Command::Interrupt { terminal: terminal()? },
        "kill" => Command::Kill { terminal: terminal()? },
        "run" => Command::Run { folder: text("folder")?, command: text("command")? },
        "lock" => Command::Lock,
        "confirm" => Command::Confirm { token: text("token")? },
        other => return Err(format!("unknown command {other:?}")),
    })
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::NeedsYou => "needs_you",
        Kind::Done => "done",
        Kind::Error => "error",
        Kind::Finished => "finished",
    }
}

impl Web {
    fn execute(&self, from: &Sender, command: Command, now: u64) -> Reply {
        self.bridge.lock().unwrap().execute(&self.core, from, command, now)
    }

    /// The terminal list, right after a device comes in.
    fn snapshot(&self, from: &Sender, now: u64) -> Value {
        match self.execute(from, Command::List, now) {
            Reply::Done(text) => message("snapshot", json!({"lines": text.lines().collect::<Vec<_>>()})),
            other => message("reply", reply_json(&Value::Null, other)),
        }
    }

    /// One text message from a client.
    fn incoming(&self, conn: &mut Conn, text: &str, now: u64) -> Step {
        let Ok(msg) = serde_json::from_str::<Value>(text) else { return refuse("not JSON") };
        let (name, data) = (msg["name"].as_str().unwrap_or_default(), &msg["data"]);
        if name == "ping" {
            return Step { out: vec![message("pong", Value::Null)], close: false };
        }
        let Some(from) = conn.device.clone() else {
            return match name {
                "pair" => self.pair(conn, data, now),
                "auth" => self.auth(conn, data, now),
                _ => refuse("pair or auth first"),
            };
        };
        match name {
            "command" => {
                let id = &data["id"];
                let reply = match command(data) {
                    Ok(command) => self.execute(&from, command, now),
                    Err(why) => Reply::Failed(why),
                };
                // Unpaired while connected: this was the last word.
                let close = reply == Reply::NotPaired;
                Step { out: vec![message("reply", reply_json(id, reply))], close }
            }
            "snapshot" => Step { out: vec![self.snapshot(&from, now)], close: false },
            _ => Step {
                out: vec![message("error", json!({"message": format!("unknown message {name:?}")}))],
                close: false,
            },
        }
    }

    /// A one-time pairing token becomes a device and its session token.
    fn pair(&self, conn: &mut Conn, data: &Value, now: u64) -> Step {
        let token = data["token"].as_str().unwrap_or_default();
        let label: String = data["label"].as_str().unwrap_or_default().trim().chars().take(40).collect();
        let label = if label.is_empty() { "web device".to_string() } else { label };
        let user = crate::platform::random_hex()[..16].to_string();
        let bridge = self.bridge.lock().unwrap();
        let me = match bridge.pair_verify("web", &user, &label, token, now) {
            Ok(me) => me,
            Err(e) => return refuse(&e.message),
        };
        let mut secret = [0u8; 32];
        crate::platform::random_bytes(&mut secret);
        let session = sha::hex(&secret);
        if let Err(e) = bridge.add_session(&user, &session) {
            return refuse(&e.message);
        }
        drop(bridge);
        let from = Sender { transport: "web".into(), user_id: user };
        conn.device = Some(from.clone());
        let paired = message("paired", json!({"session": session, "device": me.label, "id": me.id}));
        Step { out: vec![paired, self.snapshot(&from, now)], close: false }
    }

    fn auth(&self, conn: &mut Conn, data: &Value, now: u64) -> Step {
        let token = data["session"].as_str().unwrap_or_default();
        let user = match self.bridge.lock().unwrap().session_user(token) {
            Ok(Some(user)) => user,
            Ok(None) => return refuse("unknown or revoked session; pair again"),
            Err(e) => return refuse(&e.message),
        };
        let from = Sender { transport: "web".into(), user_id: user };
        conn.device = Some(from.clone());
        Step { out: vec![message("hello", Value::Null), self.snapshot(&from, now)], close: false }
    }

    /// A notice for this connection: only to a device that is still paired,
    /// and nothing while the bridge is locked.
    fn notice_for(&self, conn: &Conn, n: &Notice) -> Option<Value> {
        let from = conn.device.as_ref()?;
        let bridge = self.bridge.lock().unwrap();
        let paired = bridge.identities().ok()?.iter().any(|i| i.transport == "web" && i.user_id == from.user_id);
        if !paired || bridge.locked().unwrap_or(true) {
            return None;
        }
        let data = json!({"terminal": n.terminal, "agent": n.agent, "folder": n.folder, "kind": kind(n.kind), "excerpt": n.excerpt});
        Some(message("notice", data))
    }
}

/// Reads a request head (through the blank line) into `buf`: where it ends.
async fn read_head(stream: &mut tokio::net::TcpStream, buf: &mut Vec<u8>) -> Result<usize, u16> {
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return if end > http::MAX_HEAD { Err(431) } else { Ok(end) };
        }
        if buf.len() > http::MAX_HEAD {
            return Err(431);
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Err(400),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

async fn connection(mut stream: tokio::net::TcpStream, web: Arc<Web>) {
    let mut buf = Vec::new();
    let end = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream, &mut buf)).await {
        Ok(Ok(end)) => end,
        Ok(Err(status)) => return drop(stream.write_all(&http::error(status)).await),
        Err(_) => return drop(stream.write_all(&http::error(408)).await),
    };
    let answer = http::parse(&buf[..end]).and_then(|req| http::allowed(&req, &web.hosts).map(|()| req));
    let req = match answer {
        Ok(req) => req,
        Err(status) => return drop(stream.write_all(&http::error(status)).await),
    };
    let page = |body: &str, kind: &str| http::response(200, kind, body.as_bytes());
    let out = match req.path.split(['?', '#']).next().unwrap_or_default() {
        "/" => page(PAGE, "text/html; charset=utf-8"),
        "/app.js" => page(SCRIPT, "text/javascript; charset=utf-8"),
        "/style.css" => page(STYLE, "text/css; charset=utf-8"),
        "/ws" => match http::upgrade(&req) {
            Ok(head) => {
                if stream.write_all(&head).await.is_ok() {
                    websocket(stream, buf.split_off(end + 4), web).await;
                }
                return;
            }
            Err(status) => http::error(status),
        },
        _ => http::error(404),
    };
    let _ = stream.write_all(&out).await;
}

async fn send(wr: &mut tokio::net::tcp::OwnedWriteHalf, msg: &Value) -> bool {
    wr.write_all(&http::encode(1, msg.to_string().as_bytes())).await.is_ok()
}

async fn websocket(stream: tokio::net::TcpStream, mut buf: Vec<u8>, web: Arc<Web>) {
    let (mut rd, mut wr) = stream.into_split();
    let mut conn = Conn::default();
    let mut notices = web.notices.subscribe();
    let mut beat = tokio::time::interval(HEARTBEAT);
    let (opened, mut last) = (tokio::time::Instant::now(), tokio::time::Instant::now());
    let mut chunk = vec![0u8; 8192];
    loop {
        // Every whole frame received so far.
        loop {
            let frame = match http::decode(&buf, http::MAX_FRAME) {
                Ok(Some((frame, used))) => {
                    buf.drain(..used);
                    frame
                }
                Ok(None) => break,
                Err(_) => return drop(wr.write_all(&http::encode(8, &1009u16.to_be_bytes())).await),
            };
            last = tokio::time::Instant::now();
            match frame {
                http::Frame::Text(text) => {
                    let (shared, now) = (web.clone(), crate::runtime::now_ms());
                    let mut mine = conn.clone();
                    let Ok((mine, step)) = tokio::task::spawn_blocking(move || {
                        let step = shared.incoming(&mut mine, &text, now);
                        (mine, step)
                    })
                    .await
                    else {
                        return;
                    };
                    conn = mine;
                    for msg in &step.out {
                        if !send(&mut wr, msg).await {
                            return;
                        }
                    }
                    if step.close {
                        return drop(wr.write_all(&http::encode(8, &1008u16.to_be_bytes())).await);
                    }
                }
                http::Frame::Ping(data) => {
                    if wr.write_all(&http::encode(10, &data)).await.is_err() {
                        return;
                    }
                }
                http::Frame::Pong => {}
                http::Frame::Close => return drop(wr.write_all(&http::encode(8, &[])).await),
            }
        }
        tokio::select! {
            read = rd.read(&mut chunk) => match read {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            },
            notice = notices.recv() => match notice {
                Ok(n) => {
                    let (shared, mine) = (web.clone(), conn.clone());
                    let msg = tokio::task::spawn_blocking(move || shared.notice_for(&mine, &n)).await.ok().flatten();
                    if let Some(msg) = msg && !send(&mut wr, &msg).await {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = beat.tick() => {
                if last.elapsed() > IDLE || (conn.device.is_none() && opened.elapsed() > AUTH) {
                    return drop(wr.write_all(&http::encode(8, &1001u16.to_be_bytes())).await);
                }
                if !send(&mut wr, &message("heartbeat", Value::Null)).await {
                    return;
                }
            }
        }
    }
}

async fn serve(listener: tokio::net::TcpListener, web: Arc<Web>) {
    loop {
        if let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(connection(stream, web.clone()));
        }
    }
}

/// `h0x-core bridge web`: serves until the process is stopped.
pub fn run(paths: &Paths, lan: bool, port: u16) -> Result<(), RpcError> {
    let failed = |e: String| RpcError::new("bridge", e, "see docs/bridge.md, Web panel");
    let ip = if lan { lan_address(outbound_ip()).map_err(failed)? } else { Ipv4Addr::LOCALHOST };
    let bridge = Bridge::open(paths)?;
    let listener = std::net::TcpListener::bind((ip, port)).map_err(|e| failed(format!("{ip}:{port}: {e}")))?;
    let port = listener.local_addr().map_err(|e| failed(e.to_string()))?.port();
    let hosts = hosts(ip, port);
    let base = format!("http://{}", hosts[0]);
    println!("web panel: {base}/");
    if bridge.locked()? {
        println!("remote control is locked: no pairing until `h0x-core bridge unlock`");
    } else {
        let (code, _) = bridge.pair("web", crate::runtime::now_ms())?;
        let link = format!("{base}/#pair={code}");
        println!("pair a device within 5 minutes (once): {link}\n{}", qr(&link));
    }
    if lan {
        eprintln!(
            "warning: this is plain http on your network; anyone on it can see the traffic, pairing links included"
        );
    }
    let (notices, _) = tokio::sync::broadcast::channel(64);
    let feed = notices.clone();
    let quiet = bridge.config.notice_quiet_ms();
    std::thread::spawn(move || {
        let mut failures = 0;
        loop {
            let watched = Paths::from_env().and_then(|paths| super::watch(&paths, quiet, |n| drop(feed.send(n))));
            if let Err(e) = watched {
                eprintln!("bridge: notices: {}", e.message);
            }
            failures += 1;
            std::thread::sleep(super::telegram::backoff(failures));
        }
    });
    let core = |method: &str, params: Value| {
        let paths = Paths::from_env()?;
        let stream = crate::client::connect_or_start(&paths)?;
        crate::client::call(&stream, method, params).map_err(|e| crate::client::explain_closed(&paths, e))
    };
    let web = Arc::new(Web { bridge: Mutex::new(bridge), core: Box::new(core), hosts, notices });
    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| failed(e.to_string()))?;
    runtime.block_on(async {
        listener.set_nonblocking(true).map_err(|e| failed(e.to_string()))?;
        let listener = tokio::net::TcpListener::from_std(listener).map_err(|e| failed(e.to_string()))?;
        serve(listener, web).await;
        Ok(())
    })
}

#[cfg(test)]
mod assets;
#[cfg(test)]
mod tests;
