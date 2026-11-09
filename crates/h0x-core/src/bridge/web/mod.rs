//! `h0x-core bridge web`: a local web panel. It serves a small page and a
//! WebSocket API at `/ws` on 127.0.0.1, or with `--lan` on this machine's
//! private LAN address. A device pairs once with a one-time link (shown as a
//! QR code) and gets a session token, stored here only as its SHA-256; every
//! later connection authenticates with it first. Commands go through the
//! bridge like any other transport: allow-list, lock, rate limit, redaction
//! and audit.
//!
//! Messages are JSON `{name, data}` both ways (see docs/bridge.md), but never in
//! the clear: the first frame is the client hello, the answer is the server
//! hello, and every later frame is one sealed message (ADR-064, `e2e`).

mod e2e;
mod http;
mod sha;
pub mod tunnel;
pub mod vps;

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
/// After this many failed `auth`s from one address it is refused for [`AUTH_BLOCK_MS`].
const AUTH_STRIKES: u32 = 5;
const AUTH_BLOCK_MS: u64 = 60_000;
/// The same for wrong pairing codes: [`AUTH_STRIKES`] from one source, then [`PAIR_BLOCK_MS`].
const PAIR_BLOCK_MS: u64 = 10 * 60_000;
/// Sources remembered at once; the map never grows past this.
const MAX_SOURCES: usize = 4096;
/// A web session lives this long from pairing; then the device pairs again.
const SESSION_MS: u64 = 30 * 24 * 60 * 60_000;
/// Connections served at once; more are refused with 503.
const MAX_CONNECTIONS: usize = 64;
/// Connections one source may hold before it has authenticated; more are refused with 503.
const MAX_UNAUTHENTICATED_PER_SOURCE: usize = 8;
/// How long to wait after a failed `accept`, so a persistent error cannot spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// After closing, how long to keep reading so unread data cannot turn the
/// close into a reset (RFC 6455 7.1.1).
const LINGER: Duration = Duration::from_secs(1);

const PAGE: &str = include_str!("page.html");
const SCRIPT: &str = include_str!("app.js");
const STYLE: &str = include_str!("style.css");
/// The pure-JS crypto for pages without `crypto.subtle`; app.js fetches it only then.
const NOBLE: &str = include_str!("noble.js");

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

/// Medium error correction, the level every QR code here uses.
fn encode(text: &str) -> Option<qrcodegen::QrCode> {
    qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium).ok()
}

/// `text` as a QR code for a terminal: two rows per line, light modules
/// drawn (scanners read it on dark and light terminals alike), with a quiet zone.
pub fn qr(text: &str) -> String {
    let Some(code) = encode(text) else { return String::new() };
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

/// `text` as `{"size": N, "rows": ["0101...", ...]}`: N rows of N modules, 1 dark,
/// no quiet zone. Empty text, or more than a QR code holds, is refused.
pub fn qr_json(text: &str) -> Result<Value, RpcError> {
    if text.is_empty() {
        return Err(RpcError::invalid_params("no text to encode"));
    }
    let code = encode(text).ok_or_else(|| RpcError::invalid_params("that text is too long for a QR code"))?;
    let size = code.size();
    let rows: Vec<String> =
        (0..size).map(|y| (0..size).map(|x| if code.get_module(x, y) { '1' } else { '0' }).collect()).collect();
    Ok(json!({"size": size, "rows": rows}))
}

/// The one line `bridge web --json` prints once it is reachable. With a tunnel
/// the pairing link is the one through it.
/// `warning`: what [`integrity_warning`] says (null for a localhost-only panel).
pub fn ready_line(
    url: &str,
    pair: Option<&str>,
    tunnel: Option<&str>,
    via: Option<&str>,
    warning: Option<&str>,
) -> Value {
    json!({"url": url, "pair": pair, "tunnel": tunnel, "via": via, "warning": warning})
}

/// What a device must know about the path that delivers the page (ADR-064): the
/// messages are encrypted, but whoever controls that path could change the page.
/// None when the panel is reachable from this machine only.
pub(crate) fn integrity_warning(lan: bool, cloudflare: bool, vps: bool) -> Option<String> {
    let paths: Vec<&str> = [(lan, "your Wi-Fi"), (cloudflare, "Cloudflare"), (vps, "your VPS")]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
    (!paths.is_empty()).then(|| {
        format!(
            "messages are end-to-end encrypted, but the page itself is delivered by {}, so an attacker who controls that path could change it",
            paths.join(", ")
        )
    })
}

/// Prints one line to stdout and flushes it, so a reader of the pipe or terminal sees it now.
fn emit(line: &Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    drop(writeln!(out, "{line}"));
    drop(out.flush());
}

impl Bridge {
    /// Remembers a web device's session token, hashed, and the device key it paired with.
    fn add_session(&self, user: &str, token: &str, device: &[u8], now: u64) -> Result<(), RpcError> {
        let mut st = self.load()?;
        st.sessions.insert(user.into(), sha::hex(&sha::sha256(token.as_bytes())));
        st.session_times.insert(user.into(), now);
        st.devices.insert(user.into(), e2e::b64(device));
        self.save(&st)
    }

    /// The paired web device a session token belongs to, if any, and only
    /// when the connection's device key is the one it paired with (a session
    /// from before device keys has none, so it is refused). Every stored hash
    /// is compared, each in constant time. A session is good for [`SESSION_MS`]
    /// from the moment it was made: valid until the instant it turns 30 days old.
    /// One from before sessions expired is dated now, the first time it is seen.
    fn session_user(&self, token: &str, device: &[u8], now: u64) -> Result<Option<String>, RpcError> {
        let mut st = self.load()?;
        let hash = sha::hex(&sha::sha256(token.as_bytes()));
        let mut found = None;
        for (user, stored) in &st.sessions {
            if super::same(stored.as_bytes(), hash.as_bytes()) {
                found = Some(user.clone());
            }
        }
        let bound = |user: &String| st.devices.get(user).is_some_and(|d| *d == e2e::b64(device));
        let paired = |user: &String| st.identities.iter().any(|i| i.transport == "web" && &i.user_id == user);
        let Some(user) = found.filter(bound).filter(paired) else { return Ok(None) };
        let made = match st.session_times.get(&user) {
            Some(made) => *made,
            None => {
                st.session_times.insert(user.clone(), now);
                self.save(&st)?;
                now
            }
        };
        Ok((now.saturating_sub(made) < SESSION_MS).then_some(user))
    }

    /// Whether `user`'s session still stands at `now`, for a socket that authenticated
    /// earlier: the device is still paired with a session, and the session is under
    /// [`SESSION_MS`] old. A session from before dates has none yet (the next `auth` dates it),
    /// and a state that cannot be read counts as alive.
    fn session_live(&self, user: &str, now: u64) -> bool {
        let Ok(st) = self.load() else { return true };
        let paired = st.identities.iter().any(|i| i.transport == "web" && i.user_id == user);
        paired
            && st.sessions.contains_key(user)
            && st.session_times.get(user).is_none_or(|made| now.saturating_sub(*made) < SESSION_MS)
    }
}

/// The panel's long-term key S, kept in the secret store so the pairing
/// fingerprint survives restarts; made on first use. Never printed or logged.
fn server_key(bridge: &Bridge) -> Result<e2e::ServerKey, RpcError> {
    let failed = |e: String| RpcError::new("bridge", e, "see docs/bridge.md, Web panel");
    let stored = bridge.secret_get("web.key").map_err(|e| failed(format!("web.key: {e}")))?;
    if let Some(key) = stored.as_deref().and_then(|bytes| e2e::ServerKey::from_bytes(bytes).ok()) {
        return Ok(key);
    }
    if stored.is_some() {
        eprintln!("bridge: web.key was damaged; made a new one, so paired phones must pair again");
    }
    let key = e2e::ServerKey::generate();
    bridge.secret_put("web.key", &key.to_bytes()).map_err(|e| failed(format!("web.key: {e}")))?;
    Ok(key)
}

/// The link a device opens to pair: the one-time code and the fingerprint of
/// the panel's key, which the page pins before it says anything (ADR-064).
fn pair_link(base: &str, code: &str, key: &e2e::ServerKey) -> String {
    format!("{base}/#pair={code}&fp={}", key.fp())
}

/// A call into the core, from any thread.
type CoreCall = dyn Fn(&str, Value) -> Result<Value, RpcError> + Send + Sync;

/// Everything connections share.
pub(crate) struct Web {
    bridge: Mutex<Bridge>,
    core: Box<CoreCall>,
    hosts: Vec<String>,
    key: e2e::ServerKey,
    notices: tokio::sync::broadcast::Sender<Notice>,
    /// The vps's public host while `--via` is on: requests for it come through the reverse forward.
    public: Option<String>,
    /// Connections each source holds before it authenticated.
    unauthenticated: Arc<Sources>,
    /// Failed `auth`s per source address.
    strikes: Strikes,
    /// Wrong pairing codes per source address, counted before the code is checked.
    pair_strikes: Strikes,
}

/// Failures per source address: (strikes, refused until ms).
#[derive(Default)]
struct Strikes(Mutex<std::collections::HashMap<IpAddr, (u32, u64)>>);

impl Strikes {
    /// Whether `source` is being refused at `now`. An unknown source is never.
    fn refused(&self, source: Option<IpAddr>, now: u64) -> bool {
        source.is_some_and(|ip| self.0.lock().unwrap().get(&ip).is_some_and(|s| now < s.1))
    }

    /// One more failure; the [`AUTH_STRIKES`]th refuses the source for `block_ms`.
    fn add(&self, source: Option<IpAddr>, now: u64, block_ms: u64) {
        let Some(ip) = source else { return };
        let mut map = self.0.lock().unwrap();
        // Never past the cap: room is made only from blocks that have ended; if
        // there is none, this newcomer goes untracked and nobody else's count is touched.
        if !map.contains_key(&ip) && map.len() >= MAX_SOURCES {
            map.retain(|_, s| s.0 > 0 || now < s.1);
            if map.len() >= MAX_SOURCES {
                return;
            }
        }
        let strike = map.entry(ip).or_default();
        strike.0 += 1;
        if strike.0 >= AUTH_STRIKES {
            *strike = (0, now + block_ms);
        }
    }

    fn clear(&self, source: Option<IpAddr>) {
        if let Some(ip) = source {
            self.0.lock().unwrap().remove(&ip);
        }
    }
}

/// Connections not yet authenticated, per source address.
#[derive(Default)]
struct Sources(Mutex<std::collections::HashMap<IpAddr, usize>>);

/// One source's place among its unauthenticated connections; freed on drop.
struct Held {
    sources: Arc<Sources>,
    ip: IpAddr,
}

impl Sources {
    /// A place for `ip`, or None when it already holds [`MAX_UNAUTHENTICATED_PER_SOURCE`].
    fn take(self: &Arc<Self>, ip: IpAddr) -> Option<Held> {
        let mut map = self.0.lock().unwrap();
        let held = map.entry(ip).or_default();
        if *held >= MAX_UNAUTHENTICATED_PER_SOURCE {
            return None;
        }
        *held += 1;
        Some(Held { sources: self.clone(), ip })
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut map = self.sources.0.lock().unwrap();
        if let Some(held) = map.get_mut(&self.ip) {
            *held -= 1;
            if *held == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// One connection's device once it paired or authenticated, the device key from its
/// hello, whether it came through the Cloudflare tunnel, and its source address
/// (behind our tunnel the visitor's, from `CF-Connecting-IP`; else the socket's).
#[derive(Clone, Default)]
struct Conn {
    device: Option<Sender>,
    device_key: Option<[u8; 65]>,
    tunnel: bool,
    peer: Option<IpAddr>,
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
    fn snapshot(&self, conn: &Conn, from: &Sender, now: u64) -> Value {
        match self.execute(from, Command::List, now) {
            Reply::Done(text) => {
                message("snapshot", json!({"lines": text.lines().collect::<Vec<_>>(), "tunnel": conn.tunnel}))
            }
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
            "snapshot" => Step { out: vec![self.snapshot(conn, &from, now)], close: false },
            _ => Step {
                out: vec![message("error", json!({"message": format!("unknown message {name:?}")}))],
                close: false,
            },
        }
    }

    /// A one-time pairing token becomes a device and its session token.
    fn pair(&self, conn: &mut Conn, data: &Value, now: u64) -> Step {
        let token = data["token"].as_str().unwrap_or_default();
        // Nothing typed is a usage error, not a wrong guess.
        if token.chars().all(|c| matches!(c, ' ' | '-')) {
            return refuse("usage: pair needs the code from `h0x-core bridge pair`");
        }
        let Some(device_key) = conn.device_key else { return refuse("no device key") };
        let source = conn.peer;
        if self.pair_strikes.refused(source, now) {
            return refuse("too many wrong codes; try again later");
        }
        let label = data["label"].as_str().unwrap_or_default().replace(char::is_control, "");
        let label: String = label.trim().chars().take(40).collect();
        let label = if label.is_empty() { "web device".to_string() } else { label };
        let user = crate::platform::random_hex()[..16].to_string();
        let bridge = self.bridge.lock().unwrap();
        let me = match bridge.pair_verify("web", &user, &label, token, now) {
            Ok(me) => me,
            Err(e) => {
                drop(bridge);
                self.pair_strikes.add(source, now, PAIR_BLOCK_MS);
                return refuse(&e.message);
            }
        };
        self.pair_strikes.clear(source);
        let mut secret = [0u8; 32];
        crate::platform::random_bytes(&mut secret);
        let session = sha::hex(&secret);
        if let Err(e) = bridge.add_session(&user, &session, &device_key, now) {
            return refuse(&e.message);
        }
        drop(bridge);
        let from = Sender { transport: "web".into(), user_id: user };
        conn.device = Some(from.clone());
        let paired = message("paired", json!({"session": session, "device": me.label, "id": me.id}));
        Step { out: vec![paired, self.snapshot(conn, &from, now)], close: false }
    }

    fn auth(&self, conn: &mut Conn, data: &Value, now: u64) -> Step {
        let Some(device_key) = conn.device_key else { return refuse("no device key") };
        let token = data["session"].as_str().unwrap_or_default();
        let peer = conn.peer;
        if self.strikes.refused(peer, now) {
            return refuse("too many failed attempts; try again later");
        }
        // The guard must be gone before a failure takes the lock again.
        let found = self.bridge.lock().unwrap().session_user(token, &device_key, now);
        let user = match found {
            Ok(Some(user)) => user,
            Ok(None) => return self.auth_failed(peer, "unknown or revoked session; pair again", now),
            Err(e) => return refuse(&e.message),
        };
        self.strikes.clear(peer);
        let from = Sender { transport: "web".into(), user_id: user };
        conn.device = Some(from.clone());
        let hello = message("hello", json!({"tunnel": conn.tunnel}));
        Step { out: vec![hello, self.snapshot(conn, &from, now)], close: false }
    }

    /// A failed `auth`: logged (never the token) and counted against the
    /// peer's address. The connection closes either way.
    fn auth_failed(&self, peer: Option<IpAddr>, why: &str, now: u64) -> Step {
        let who = peer.map_or_else(|| "unknown".to_string(), |ip| ip.to_string());
        let entry = json!({"time_ms": now, "identity": format!("web:{who}"), "command": "auth", "result": format!("refused: {why}")});
        let _ = self.bridge.lock().unwrap().audit_failed(entry);
        self.strikes.add(peer, now, AUTH_BLOCK_MS);
        refuse(why)
    }

    /// Whether the session behind an open socket is still inside [`SESSION_MS`]; an
    /// unreadable state counts as alive (the next `auth` decides).
    fn session_alive(&self, conn: &Conn, now: u64) -> bool {
        conn.device.as_ref().is_none_or(|from| self.bridge.lock().unwrap().session_live(&from.user_id, now))
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
    let peer = stream.peer_addr().ok().map(|a| a.ip());
    // A source's connections that have not authenticated yet are capped. A loopback peer
    // is a proxy (the vps forward, cloudflared): its visitors are capped below, by address.
    let mut held = Vec::new();
    if let Some(ip) = peer.filter(|ip| !tunnel::loopback(*ip)) {
        match web.unauthenticated.take(ip) {
            Some(place) => held.push(place),
            None => return drop(stream.write_all(&http::error(503)).await),
        }
    }
    let mut buf = Vec::new();
    let end = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream, &mut buf)).await {
        Ok(Ok(end)) => end,
        Ok(Err(status)) => return drop(stream.write_all(&http::error(status)).await),
        Err(_) => return drop(stream.write_all(&http::error(408)).await),
    };
    let tunnel = tunnel::host();
    let answer = http::parse(&buf[..end]).and_then(|mut req| match tunnel::through(&req, tunnel.as_deref())? {
        true => Ok((req, true)),
        false => {
            vps::https_origin_as_http(&mut req, &web.hosts);
            http::allowed(&req, &web.hosts).map(|()| (req, false))
        }
    });
    let (req, via_tunnel) = match answer {
        Ok(answer) => answer,
        Err(status) => return drop(stream.write_all(&http::error(status)).await),
    };
    let page = |body: &str, kind: &str| http::response(200, kind, body.as_bytes());
    let out = match req.path.split(['?', '#']).next().unwrap_or_default() {
        "/" => page(PAGE, "text/html; charset=utf-8"),
        "/app.js" => page(SCRIPT, "text/javascript; charset=utf-8"),
        "/style.css" => page(STYLE, "text/css; charset=utf-8"),
        "/noble.js" => page(NOBLE, "text/javascript; charset=utf-8"),
        "/ws" => match http::upgrade(&req) {
            Ok(head) => {
                let client = tunnel::client(&req, peer, via_tunnel, web.public.as_deref());
                if let Some(ip) = client.filter(|ip| Some(*ip) != peer) {
                    match web.unauthenticated.take(ip) {
                        Some(place) => held.push(place),
                        None => return drop(stream.write_all(&http::error(503)).await),
                    }
                }
                if stream.write_all(&head).await.is_ok() {
                    websocket(stream, buf.split_off(end + 4), web, via_tunnel, client, held).await;
                }
                return;
            }
            Err(status) => http::error(status),
        },
        _ => http::error(404),
    };
    let _ = stream.write_all(&out).await;
}

/// Seals `msg` as one binary frame. Before the handshake there is no channel
/// and nothing is sent: no byte ever goes out in the clear.
async fn send(wr: &mut tokio::net::tcp::OwnedWriteHalf, chan: &mut Option<e2e::Channel>, msg: &Value) -> bool {
    let Some(chan) = chan else { return true };
    let Ok(sealed) = chan.seal(msg.to_string().as_bytes()) else { return false };
    wr.write_all(&http::encode(2, &sealed)).await.is_ok()
}

/// The client hello is about 200 bytes; this is generous and still small.
const MAX_HELLO: usize = 1024;

/// The close code for a frame the decoder refused.
fn close_code(why: &str) -> u16 {
    match why {
        "frame too big" => 1009,
        "text is not UTF-8" => 1007,
        _ => 1002,
    }
}

/// Closes as RFC 6455 7.1.1 says: the close frame, then our write side, then
/// read (and drop) what the client still sends until it ends or [`LINGER`]
/// passes. Dropping the socket with unread data would send a reset that can
/// erase the close frame.
async fn hang_up(mut rd: tokio::net::tcp::OwnedReadHalf, mut wr: tokio::net::tcp::OwnedWriteHalf, payload: &[u8]) {
    let _ = wr.write_all(&http::encode(8, payload)).await;
    let _ = wr.shutdown().await;
    let mut sink = [0u8; 4096];
    let _ = tokio::time::timeout(LINGER, async { while matches!(rd.read(&mut sink).await, Ok(n) if n > 0) {} }).await;
}

async fn websocket(
    stream: tokio::net::TcpStream,
    mut buf: Vec<u8>,
    web: Arc<Web>,
    tunnel: bool,
    peer: Option<IpAddr>,
    mut held: Vec<Held>,
) {
    let (mut rd, mut wr) = stream.into_split();
    let mut conn = Conn { peer, tunnel, ..Conn::default() };
    let mut chan: Option<e2e::Channel> = None;
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
                Err(why) => return hang_up(rd, wr, &close_code(why).to_be_bytes()).await,
            };
            last = tokio::time::Instant::now();
            let text = match frame {
                // The first frame, and only the first, is the client hello.
                http::Frame::Text(hello) => {
                    let accepted = match chan {
                        None if hello.len() <= MAX_HELLO => e2e::server_accept(hello.as_bytes(), &web.key).ok(),
                        _ => None,
                    };
                    let Some(accepted) = accepted else { return hang_up(rd, wr, &1008u16.to_be_bytes()).await };
                    if wr.write_all(&http::encode(1, &accepted.server_hello)).await.is_err() {
                        return;
                    }
                    conn.device_key = Some(accepted.device);
                    chan = Some(accepted.channel);
                    continue;
                }
                // Everything after it is one sealed message; any failure closes.
                http::Frame::Binary(sealed) => {
                    let plain = chan.as_mut().and_then(|c| c.open(&sealed).ok());
                    match plain.and_then(|p| String::from_utf8(p).ok()) {
                        Some(text) => text,
                        None => return hang_up(rd, wr, &1008u16.to_be_bytes()).await,
                    }
                }
                http::Frame::Ping(data) => {
                    if wr.write_all(&http::encode(10, &data)).await.is_err() {
                        return;
                    }
                    continue;
                }
                http::Frame::Pong => continue,
                http::Frame::Close => return hang_up(rd, wr, &[]).await,
            };
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
            // Once authenticated, the connection no longer counts against its source's share.
            if conn.device.is_some() {
                held.clear();
            }
            for msg in &step.out {
                if !send(&mut wr, &mut chan, msg).await {
                    return;
                }
            }
            if step.close {
                return hang_up(rd, wr, &1008u16.to_be_bytes()).await;
            }
        }
        let idle = last + IDLE;
        let due = if conn.device.is_none() { idle.min(opened + AUTH) } else { idle };
        tokio::select! {
            read = rd.read(&mut chunk) => match read {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            },
            notice = notices.recv() => match notice {
                Ok(n) => {
                    let (shared, mine) = (web.clone(), conn.clone());
                    let msg = tokio::task::spawn_blocking(move || shared.notice_for(&mine, &n)).await.ok().flatten();
                    if let Some(msg) = msg && !send(&mut wr, &mut chan, &msg).await {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = tokio::time::sleep_until(due) => return hang_up(rd, wr, &1001u16.to_be_bytes()).await,
            _ = beat.tick() => {
                // A session that turned 30 days old since `auth` ends here, not at the next reconnect.
                let (shared, mine, now) = (web.clone(), conn.clone(), crate::runtime::now_ms());
                if !tokio::task::spawn_blocking(move || shared.session_alive(&mine, now)).await.unwrap_or(true) {
                    return hang_up(rd, wr, &1008u16.to_be_bytes()).await;
                }
                if !send(&mut wr, &mut chan, &message("heartbeat", Value::Null)).await {
                    return;
                }
            }
        }
    }
}

async fn serve(listener: tokio::net::TcpListener, web: Arc<Web>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        match listener.accept().await {
            Ok((stream, _)) => match slots.clone().try_acquire_owned() {
                Ok(slot) => {
                    let web = web.clone();
                    tokio::spawn(async move {
                        connection(stream, web).await;
                        drop(slot);
                    });
                }
                // Full: a short answer, best effort, and the socket is closed.
                Err(_) => drop(stream.try_write(&http::error(503))),
            },
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// `h0x-core bridge web`: serves until the process is stopped (or Ctrl+C,
/// which also ends a tunnel). `via`: reverse the port through your vps.
/// `tunnel`: open a Cloudflare tunnel at once, for that many minutes.
/// `json`: print one [`ready_line`] instead of the human lines, once reachable.
pub fn run(
    paths: &Paths,
    lan: bool,
    port: u16,
    via: Option<vps::Vps>,
    tunnel: Option<u64>,
    json: bool,
) -> Result<(), RpcError> {
    let failed = |e: String| RpcError::new("bridge", e, "see docs/bridge.md, Web panel");
    let ip = if lan { lan_address(outbound_ip()).map_err(failed)? } else { Ipv4Addr::LOCALHOST };
    let bridge = Bridge::open(paths)?;
    let key = server_key(&bridge)?;
    let listener = std::net::TcpListener::bind((ip, port)).map_err(|e| failed(format!("{ip}:{port}: {e}")))?;
    let port = listener.local_addr().map_err(|e| failed(e.to_string()))?.port();
    let public = via.as_ref().map(|v| v.public_host.as_str());
    let hosts = vps::with_public(hosts(ip, port), public);
    let local = format!("http://{}", hosts[0]);
    let base = public.map_or_else(|| local.clone(), |host| format!("https://{host}"));
    let mut pair = None;
    if !json {
        println!("web panel: {local}/");
    }
    if bridge.locked()? {
        if !json {
            println!("remote control is locked: no pairing until `h0x-core bridge unlock`");
        }
    } else if tunnel.is_some() {
        if !json {
            println!("starting a Cloudflare tunnel; its pairing link follows");
        }
    } else {
        let (code, _) = bridge.pair("web", crate::runtime::now_ms())?;
        let link = pair_link(&base, &code, &key);
        if !json {
            println!("pair a device within 5 minutes (once): {link}\n{}", qr(&link));
        }
        pair = Some(link);
    }
    let url = format!("{local}/");
    let warning = integrity_warning(lan, tunnel.is_some(), via.is_some());
    let ready = (json && tunnel.is_some()).then(|| (url.clone(), public.map(String::from), warning.clone()));
    if json && tunnel.is_none() {
        emit(&ready_line(&url, pair.as_deref(), None, public, warning.as_deref()));
    }
    // A tunnel prints its own warning when it opens.
    if !json && let Some(why) = integrity_warning(lan, false, via.is_some()) {
        println!("warning: {why}");
    }
    let ssh = via.as_ref().map(|plan| {
        if !json {
            println!(
                "through your vps: reverse port 127.0.0.1:{} on the server; point your HTTPS proxy there",
                plan.remote_port
            );
        }
        let locked = vps::lock_check(Bridge::open(paths)?, |why| eprintln!("bridge: vps: {why}"));
        Ok(vps::Tunnel::ssh(plan, port, locked))
    });
    let ssh = ssh.transpose()?;
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
    let manager = tunnel::manage(Bridge::open(paths)?, hosts[0].clone(), tunnel, json, ready);
    let web = Arc::new(Web {
        bridge: Mutex::new(bridge),
        core: Box::new(core),
        hosts,
        key,
        notices,
        public: public.map(String::from),
        unauthenticated: Default::default(),
        strikes: Default::default(),
        pair_strikes: Default::default(),
    });
    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| failed(e.to_string()))?;
    runtime
        .block_on(async {
            listener.set_nonblocking(true).map_err(|e| failed(e.to_string()))?;
            let listener = tokio::net::TcpListener::from_std(listener).map_err(|e| failed(e.to_string()))?;
            tokio::select! {
                () = serve(listener, web) => {}
                () = manager => {}
                () = stopped() => {}
            }
            Ok(())
        })
        // The vps tunnel is stopped (ssh killed) as it drops here, on every way out.
        .inspect(|()| drop(ssh))
}

/// Ctrl-C, or on Unix a termination signal.
async fn stopped() {
    #[cfg(unix)]
    {
        let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    drop(tokio::signal::ctrl_c().await);
}

#[cfg(test)]
mod assets;
#[cfg(test)]
mod tests;

#[cfg(all(test, windows))]
mod store_tests {
    use super::*;

    #[test]
    fn an_unusable_secret_store_never_replaces_web_key() {
        let bridge = crate::bridge::scratch();
        let key = server_key(&bridge).unwrap();
        let blob = bridge.dir.join("web.key.dpapi");
        std::fs::write(&blob, b"not a DPAPI blob").unwrap();
        let err = server_key(&bridge).err().expect("a store that cannot be read is an error, not an absent key");
        assert!(err.message.contains("web.key"), "{}", err.message);
        assert!(bridge.secret_get("web.key").is_err(), "distinct from not found");
        assert_eq!(std::fs::read(&blob).unwrap(), b"not a DPAPI blob", "web.key was left alone");
        assert!(bridge.secret_del("web.key").unwrap());
        assert_ne!(server_key(&bridge).unwrap().fp(), key.fp(), "truly absent: a new one");
        let _ = bridge.secret_del("web.key");
        let _ = std::fs::remove_dir_all(&bridge.dir);
    }
}
