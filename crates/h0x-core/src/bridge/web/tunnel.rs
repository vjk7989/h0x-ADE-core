//! "Access from anywhere": a Cloudflare quick tunnel to the web panel.
//!
//! The panel runs `cloudflared tunnel --url http://<panel> --no-autoupdate
//! --protocol http2` as a child, reads its `https://<name>.trycloudflare.com`
//! address from its log, and answers that Host (and only an https Origin of
//! it) while the tunnel is up. The tunnel stops when its time is up, when the
//! bridge is locked, on `h0x-core bridge tunnel off`, and with the panel;
//! dropping a [`Tunnel`] kills cloudflared on every OS.
//!
//! `bridge tunnel on|off` talks to a running panel through two files in the
//! bridge folder (0700, this user only): `tunnel.request`, which the panel
//! takes within a second, and `tunnel.json`, the panel's answer while a
//! tunnel is up.

use super::super::Bridge;
use super::{http, sha};
use crate::rpc::RpcError;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

/// How long a tunnel stays up when no `--for` is given.
pub const MINUTES: u64 = 60;
/// The longest `--for`: a day.
pub const MAX_MINUTES: u64 = 24 * 60;
/// cloudflared must print its address within this.
const URL_WAIT: Duration = Duration::from_secs(45);
/// A running panel takes a request within this.
const TAKEN_WAIT: Duration = Duration::from_secs(3);
const REQUEST: &str = "tunnel.request";
const STATUS: &str = "tunnel.json";

/// The cloudflared downloaded when none is on PATH, and the SHA-256 of each
/// release asset. To move to a newer release: pick one at
/// github.com/cloudflare/cloudflared/releases, run
/// `gh api repos/cloudflare/cloudflared/releases/tags/<version> --jq '.assets[] | [.name, .digest] | @tsv'`,
/// and copy each asset's digest here. Use GitHub's digest (of the file it
/// serves): the darwin lines in the release notes have not always matched it.
const VERSION: &str = "2026.10.7";
const RELEASES: &str = "https://github.com/cloudflare/cloudflared/releases/download";
/// (`std::env::consts::OS`, `ARCH`, asset, SHA-256).
const ASSETS: [(&str, &str, &str, &str); 5] = [
    (
        "windows",
        "x86_64",
        "cloudflared-windows-amd64.exe",
        "2493dbc168923bb3e59e5e7716f71deced92992eea9acd6e9d74f84ac7751343",
    ),
    ("linux", "x86_64", "cloudflared-linux-amd64", "1f99fb518b83dec2ba8d870a7cc991d82a77e6196295a3d2754ba17ab9f91872"),
    ("linux", "aarch64", "cloudflared-linux-arm64", "3df43aedc9ff291d1735b5cb576cb97e0b157488796db2e32b28aecabb445775"),
    (
        "macos",
        "x86_64",
        "cloudflared-darwin-amd64.tgz",
        "02f3edfd87684dd5b705607688e4eb4b323f6e777b81e7b3a85af556aa356848",
    ),
    (
        "macos",
        "aarch64",
        "cloudflared-darwin-arm64.tgz",
        "3586ccf397e98e5f437e60e7c307ca6ac463e110580503af8bb8e6f9ebf831bd",
    ),
];

/// The tunnel's host while one is up. One panel per process.
static HOST: Mutex<Option<String>> = Mutex::new(None);

fn failed(why: impl std::fmt::Display) -> RpcError {
    RpcError::new("bridge", why.to_string(), "see docs/bridge.md, Access from anywhere")
}

/// The host of a quick-tunnel address: `https://<one label>.trycloudflare.com`,
/// with at most a trailing `/`. Anything else (http, another domain, a
/// deeper name, a port, a path, user info) is not one.
pub fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.strip_suffix('/').unwrap_or(rest);
    let label = host.strip_suffix(".trycloudflare.com")?;
    let ok = (1..=63).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then_some(host)
}

/// The tunnel host in one line of cloudflared's log, if it has one.
pub fn host_in(line: &str) -> Option<&str> {
    line.split(|c: char| c.is_whitespace() || c == '|').find_map(host_of)
}

/// Whether a request came through the tunnel: its Host is the tunnel's.
/// Then an Origin, when sent, must be the tunnel's https page (Err 403), and
/// the panel's other rules apply. Ok(false): not the tunnel's; the panel
/// checks it against its own address.
pub fn through(req: &http::Request, tunnel: Option<&str>) -> Result<bool, u16> {
    let Some(host) = tunnel.filter(|t| req.header("host").is_some_and(|h| h.eq_ignore_ascii_case(t))) else {
        return Ok(false);
    };
    if let Some(origin) = req.header("origin")
        && !origin.strip_prefix("https://").is_some_and(|o| o.eq_ignore_ascii_case(host))
    {
        return Err(403);
    }
    let headers = req.headers.iter().filter(|(name, _)| name != "origin").cloned().collect();
    let rest = http::Request { method: req.method.clone(), path: req.path.clone(), headers };
    http::allowed(&rest, &[host.to_string()]).map(|()| true)
}

/// The host the panel answers through the tunnel, while one is up.
pub fn host() -> Option<String> {
    HOST.lock().unwrap().clone()
}

fn forget(host: &str) {
    let mut current = HOST.lock().unwrap();
    if current.as_deref() == Some(host) {
        *current = None;
    }
}

/// When a tunnel that starts at `now_ms` for `minutes` stops.
pub fn until(now_ms: u64, minutes: u64) -> u64 {
    now_ms.saturating_add(minutes.saturating_mul(60_000))
}

/// Why a tunnel must stop now, if it must: cloudflared ended, the bridge is
/// locked, or its time is up.
pub fn must_stop(now_ms: u64, until_ms: u64, locked: bool, exited: bool) -> Option<&'static str> {
    if exited {
        Some("cloudflared ended")
    } else if locked {
        Some("remote control is locked")
    } else if now_ms >= until_ms {
        Some("its time is up")
    } else {
        None
    }
}

/// A running cloudflared. Dropping it ends the process (on Windows too) and
/// takes its host off the allow-list.
pub struct Tunnel {
    pub host: String,
    child: tokio::process::Child,
}

impl Tunnel {
    /// Starts `program` as a quick tunnel to `origin` (`ip:port`) and waits
    /// for its address.
    pub async fn start(program: &Path, origin: &str) -> Result<Tunnel, String> {
        let mut child = tokio::process::Command::new(program)
            .args(["tunnel", "--url", &format!("http://{origin}"), "--no-autoupdate", "--protocol", "http2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
        let mut lines = tokio::io::BufReader::new(child.stderr.take().expect("piped")).lines();
        let found = tokio::time::timeout(URL_WAIT, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(host) = host_in(&line) {
                    return Some(host.to_string());
                }
            }
            None
        })
        .await;
        let Ok(Some(host)) = found else {
            return Err("cloudflared gave no tunnel address; check your connection and try again".into());
        };
        // Keep reading, so cloudflared never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        *HOST.lock().unwrap() = Some(host.clone());
        Ok(Tunnel { host, child })
    }

    fn exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// Off the allow-list, then cloudflared killed and waited for.
    pub async fn stop(mut self) {
        forget(&self.host);
        let _ = self.child.kill().await;
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        forget(&self.host);
    }
}

/// The pinned asset and its SHA-256 for an OS and architecture.
pub fn asset(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    ASSETS.iter().find(|(o, a, _, _)| *o == os && *a == arch).map(|(_, _, name, sum)| (*name, *sum))
}

fn exe_name() -> String {
    format!("cloudflared{}", std::env::consts::EXE_SUFFIX)
}

/// Where a downloaded cloudflared goes: beside h0x-core.
pub fn install_dir() -> Result<PathBuf, RpcError> {
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).map_err(failed)?;
    Ok(exe.parent().map(Path::to_path_buf).unwrap_or_default())
}

/// The cloudflared to run: the first on `path` (a PATH value), else one
/// downloaded into `install`.
pub fn find(path: Option<&std::ffi::OsStr>, install: &Path) -> Option<PathBuf> {
    let name = exe_name();
    let on_path = path.into_iter().flat_map(std::env::split_paths).map(|dir| dir.join(&name));
    on_path.chain([install.join(&name)]).find(|p| p.is_file())
}

/// Moves a downloaded `asset` (at `file`) into `dir` as cloudflared, only if
/// its SHA-256 is `sum`. A mismatch deletes the download and installs nothing.
pub fn install(file: &Path, asset: &str, sum: &str, dir: &Path) -> Result<PathBuf, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let got = sha::hex(&sha::sha256(&bytes));
    if got != sum {
        let _ = std::fs::remove_file(file);
        return Err(format!("refusing {asset}: its SHA-256 is {got}, not the pinned {sum}"));
    }
    let dest = dir.join(exe_name());
    let placed = if asset.ends_with(".tgz") {
        let ok = Command::new("tar").arg("-xzf").arg(file).arg("-C").arg(dir).arg("cloudflared").status();
        let _ = std::fs::remove_file(file);
        ok.map_err(|e| e.to_string()).and_then(|s| s.success().then_some(()).ok_or(format!("cannot unpack {asset}")))
    } else {
        std::fs::rename(file, &dest).map_err(|e| e.to_string()).and_then(|()| {
            // Unix only (no suffix there): a downloaded file is not executable yet.
            if std::env::consts::EXE_SUFFIX.is_empty() {
                let ok = Command::new("chmod").arg("755").arg(&dest).status().is_ok_and(|s| s.success());
                return ok.then_some(()).ok_or(format!("cannot make {} executable", dest.display()));
            }
            Ok(())
        })
    };
    placed.map(|()| dest)
}

/// Downloads the pinned cloudflared for this machine into `dir` (system
/// curl, https only) and installs it if its SHA-256 matches.
pub fn download(dir: &Path) -> Result<PathBuf, RpcError> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let Some((asset, sum)) = asset(os, arch) else {
        return Err(failed(format!("no pinned cloudflared for {os}/{arch}; install cloudflared yourself (on PATH)")));
    };
    let url = format!("{RELEASES}/{VERSION}/{asset}");
    let part = dir.join(format!("{asset}.part"));
    let ok = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--retry", "2", "--connect-timeout", "10"])
        .arg("-o")
        .arg(&part)
        .arg(&url)
        .status();
    if !ok.is_ok_and(|s| s.success()) {
        let _ = std::fs::remove_file(&part);
        return Err(failed(format!("cannot download {url}")));
    }
    install(&part, asset, sum, dir).map_err(failed)
}

/// What the user is asked before a download.
pub fn download_question(dir: &Path) -> String {
    format!(
        "cloudflared is not on PATH. Download cloudflared {VERSION} from github.com/cloudflare/cloudflared (30-60 MB, SHA-256 checked) into {}? [y/N] ",
        dir.display()
    )
}

/// What a tunnel's start prints: its address, how long it stays, the
/// warning, and a pairing link (with its QR code) when there is one.
fn announce(status: &Value) -> String {
    let mut out = format!(
        "tunnel: {} for {} minutes (`h0x-core bridge tunnel off` stops it)\n\
         warning: traffic passes through Cloudflare; it is https to Cloudflare, not end-to-end",
        status["url"].as_str().unwrap_or_default(),
        status["minutes"],
    );
    if let Some(link) = status["pair"].as_str() {
        out.push_str(&format!("\npair a device within 5 minutes (once): {link}\n{}", super::qr(link)));
    }
    out
}

/// A request from `bridge tunnel on|off`, taken: Some(Some(minutes)) on,
/// Some(None) off. Anything unreadable is dropped.
fn take(bridge: &Bridge) -> Option<Option<u64>> {
    let path = bridge.dir.join(REQUEST);
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let request: Value = serde_json::from_slice(&bytes).ok()?;
    Some(request["minutes"].as_u64().map(|m| m.clamp(1, MAX_MINUTES)))
}

/// A started tunnel: the pairing link through it, the answer for `bridge
/// tunnel on`, and the panel's own printout.
fn opened(bridge: &Bridge, host: &str, minutes: u64, until_ms: u64, now: u64) -> Value {
    let pair = bridge.pair("web", now).ok().map(|(code, _)| format!("https://{host}/#pair={code}"));
    let status = json!({"url": format!("https://{host}/"), "minutes": minutes, "until_ms": until_ms, "pair": pair});
    let _ = bridge.write_private(STATUS, status.to_string().as_bytes());
    println!("{}", announce(&status));
    status
}

/// The tunnel side of a running panel at `origin` (`ip:port`): opens one
/// now when `first` says for how long, then each second takes `bridge
/// tunnel on|off` requests and stops the tunnel when [`must_stop`] says so.
/// The tunnel ends with this future.
pub async fn manage(bridge: Bridge, origin: String, first: Option<u64>) {
    let _ = std::fs::remove_file(bridge.dir.join(STATUS));
    let mut up: Option<(Tunnel, u64)> = None;
    let mut pending = first.map(Some);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let now = crate::runtime::now_ms();
        match pending.take().or_else(|| take(&bridge)) {
            Some(Some(minutes)) => {
                let locked = bridge.locked().unwrap_or(true);
                if let Some((tunnel, until_ms)) = up.as_mut().filter(|_| !locked) {
                    *until_ms = until(now, minutes);
                    opened(&bridge, &tunnel.host, minutes, *until_ms, now);
                } else if locked {
                    let why = "remote control is locked; `h0x-core bridge unlock` here first";
                    let _ = bridge.write_private(STATUS, json!({"error": why}).to_string().as_bytes());
                    eprintln!("tunnel: {why}");
                } else {
                    let program = install_dir().ok().and_then(|dir| find(std::env::var_os("PATH").as_deref(), &dir));
                    let started = match program {
                        Some(program) => Tunnel::start(&program, &origin).await,
                        None => Err("no cloudflared; `h0x-core bridge tunnel on` downloads it".into()),
                    };
                    match started {
                        Ok(tunnel) => {
                            let until_ms = until(now, minutes);
                            opened(&bridge, &tunnel.host, minutes, until_ms, now);
                            up = Some((tunnel, until_ms));
                        }
                        Err(why) => {
                            let _ = bridge.write_private(STATUS, json!({"error": why}).to_string().as_bytes());
                            eprintln!("tunnel: {why}");
                        }
                    }
                }
            }
            Some(None) => {
                if let Some((tunnel, _)) = up.take() {
                    tunnel.stop().await;
                    println!("tunnel stopped: turned off");
                }
                let _ = std::fs::remove_file(bridge.dir.join(STATUS));
            }
            None => {}
        }
        if let Some((tunnel, until_ms)) = up.as_mut()
            && let Some(why) = must_stop(now, *until_ms, bridge.locked().unwrap_or(true), tunnel.exited())
        {
            if let Some((tunnel, _)) = up.take() {
                tunnel.stop().await;
            }
            let _ = std::fs::remove_file(bridge.dir.join(STATUS));
            println!("tunnel stopped: {why}");
        }
    }
}

/// `h0x-core bridge tunnel on|off`: asks the running panel and waits for its
/// answer. `minutes`: Some to turn it on (the answer and its printout), None
/// to turn it off.
pub fn ask(bridge: &Bridge, minutes: Option<u64>) -> Result<(Value, String), RpcError> {
    let (request, status) = (bridge.dir.join(REQUEST), bridge.dir.join(STATUS));
    let _ = std::fs::remove_file(&status);
    bridge.write_private(REQUEST, json!({"minutes": minutes}).to_string().as_bytes())?;
    let started = std::time::Instant::now();
    while request.exists() {
        if started.elapsed() > TAKEN_WAIT {
            let _ = std::fs::remove_file(&request);
            return Err(RpcError::new(
                "bridge",
                "no web panel is running here",
                "start one with `h0x-core bridge web --tunnel`",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if minutes.is_none() {
        return Ok((json!({"tunnel": false}), "tunnel off".into()));
    }
    while started.elapsed() < URL_WAIT + TAKEN_WAIT * 2 {
        if let Some(answer) = std::fs::read(&status).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            if let Some(why) = answer["error"].as_str() {
                return Err(failed(why));
            }
            return Ok((answer.clone(), announce(&answer)));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(failed("the panel did not report a tunnel in time; see its output"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "2026-11-06T10:00:01Z INF Requesting new quick Tunnel on trycloudflare.com...\n\
        2026-11-06T10:00:03Z INF +--------------------------------------------------------------------------------------------+\n\
        2026-11-06T10:00:03Z INF |  Your quick Tunnel has been created! Visit it at (it may take some time to be reachable):  |\n\
        2026-11-06T10:00:03Z INF |  https://brave-otter-sample-words.trycloudflare.com                                        |\n\
        2026-11-06T10:00:03Z INF +--------------------------------------------------------------------------------------------+\n\
        2026-11-06T10:00:04Z INF Registered tunnel connection connIndex=0 location=fra08 protocol=http2";

    #[test]
    fn the_tunnel_address_is_found_in_cloudflareds_log() {
        let found: Vec<_> = LOG.lines().filter_map(host_in).collect();
        assert_eq!(found, ["brave-otter-sample-words.trycloudflare.com"]);
        assert_eq!(host_in("see https://developers.cloudflare.com/cloudflare-one/ for help"), None);
        assert_eq!(host_in(""), None);
    }

    #[test]
    fn only_a_real_quick_tunnel_address_counts() {
        assert_eq!(host_of("https://abc-1.trycloudflare.com"), Some("abc-1.trycloudflare.com"));
        assert_eq!(host_of("https://abc-1.trycloudflare.com/"), Some("abc-1.trycloudflare.com"));
        for bad in [
            "https://x.trycloudflare.com.evil.com",
            "http://x.trycloudflare.com",
            "https://a.b.trycloudflare.com",
            "https://user@x.trycloudflare.com",
            "https://evil.com@x.trycloudflare.com",
            "https://x.trycloudflare.com@evil.com",
            "https://x.trycloudflare.com:8443",
            "https://x.trycloudflare.com/path",
            "https://x.trycloudflare.com?q",
            "https://.trycloudflare.com",
            "https://trycloudflare.com",
            "https://-x.trycloudflare.com",
            "https://X.trycloudflare.com",
            "https://x.TRYCLOUDFLARE.com",
            "HTTPS://x.trycloudflare.com",
            "https://x_y.trycloudflare.com",
            "https://xtrycloudflare.com",
            "https://x.trycloudflare.com\\@evil.com",
            &format!("https://{}.trycloudflare.com", "a".repeat(64)),
            "",
        ] {
            assert_eq!(host_of(bad), None, "{bad:?}");
        }
        assert!(host_of(&format!("https://{}.trycloudflare.com", "a".repeat(63))).is_some(), "63 is the limit");
    }

    fn req(text: &str) -> http::Request {
        http::parse(text.replace('\n', "\r\n").as_bytes()).unwrap()
    }

    #[test]
    fn requests_through_the_tunnel_need_its_host_and_https_origin() {
        let t = Some("abc.trycloudflare.com");
        let through = |text: &str| through(&req(text), t);
        assert_eq!(
            through("GET /ws HTTP/1.1\nHost: abc.trycloudflare.com\nOrigin: https://abc.trycloudflare.com"),
            Ok(true)
        );
        assert_eq!(through("GET / HTTP/1.1\nHost: ABC.trycloudflare.com"), Ok(true), "no Origin: an app");
        assert_eq!(through("GET / HTTP/1.1\nHost: 127.0.0.1:8377"), Ok(false), "the panel's own rules then");
        for bad in [
            "GET / HTTP/1.1\nHost: abc.trycloudflare.com\nOrigin: http://abc.trycloudflare.com",
            "GET / HTTP/1.1\nHost: abc.trycloudflare.com\nOrigin: https://evil.trycloudflare.com",
            "GET / HTTP/1.1\nHost: abc.trycloudflare.com\nOrigin: https://abc.trycloudflare.com.evil.com",
            "GET / HTTP/1.1\nHost: abc.trycloudflare.com\nOrigin: null",
            "POST / HTTP/1.1\nHost: abc.trycloudflare.com",
        ] {
            assert!(through(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            super::through(&req("GET / HTTP/1.1\nHost: abc.trycloudflare.com"), None),
            Ok(false),
            "no tunnel up"
        );
    }

    #[test]
    fn a_tunnel_stops_when_its_time_is_up_on_lock_and_when_cloudflared_ends() {
        let start = 1_000_000;
        let end = until(start, 60);
        assert_eq!(end, start + 3_600_000);
        assert_eq!(must_stop(start, end, false, false), None);
        assert_eq!(must_stop(end - 1, end, false, false), None, "one millisecond left");
        assert_eq!(must_stop(end, end, false, false), Some("its time is up"));
        assert_eq!(must_stop(start, end, true, false), Some("remote control is locked"));
        assert_eq!(must_stop(start, end, false, true), Some("cloudflared ended"));
        assert_eq!(until(u64::MAX - 5, MAX_MINUTES), u64::MAX, "no overflow");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("h0x-core-tunnel-{name}-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_cloudflared_on_path_wins_over_the_downloaded_one() {
        let root = scratch("path");
        let (first, second, install) = (root.join("Program Files (x86)"), root.join("tools"), root.join("core bin"));
        for dir in [&first, &second, &install] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(find(Some(&path), &install), None, "none anywhere");
        std::fs::write(install.join(exe_name()), "").unwrap();
        assert_eq!(find(Some(&path), &install), Some(install.join(exe_name())), "the downloaded one");
        assert_eq!(find(None, &install), Some(install.join(exe_name())), "no PATH at all");
        std::fs::write(second.join(exe_name()), "").unwrap();
        assert_eq!(find(Some(&path), &install), Some(second.join(exe_name())), "PATH first");
        std::fs::create_dir(first.join(exe_name())).unwrap();
        assert_eq!(find(Some(&path), &install), Some(second.join(exe_name())), "a folder by that name is not it");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn windows_paths_split_and_pick_the_exe() {
        assert_eq!(asset("windows", "x86_64").unwrap().0, "cloudflared-windows-amd64.exe");
        assert_eq!(asset("windows", "aarch64"), None, "no pinned build: install it yourself");
        if std::env::consts::OS == "windows" {
            let path = std::ffi::OsString::from(r#"C:\Windows\System32;"C:\Program Files (x86)\cloudflared";"#);
            let dirs: Vec<_> = std::env::split_paths(&path).collect();
            assert_eq!(dirs[1], Path::new(r"C:\Program Files (x86)\cloudflared"));
            assert_eq!(exe_name(), "cloudflared.exe");
        }
    }

    #[test]
    fn every_pinned_asset_has_a_sha256() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
        ] {
            let (name, sum) = asset(os, arch).unwrap();
            assert!(name.starts_with("cloudflared-"), "{name}");
            assert!(sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{sum}");
        }
        assert_eq!(asset("freebsd", "x86_64"), None);
        assert_eq!(asset("", ""), None);
    }

    #[test]
    fn a_download_with_the_wrong_sha256_is_refused_and_deleted() {
        let dir = scratch("sha");
        let part = dir.join("cloudflared-linux-amd64.part");
        std::fs::write(&part, b"not cloudflared").unwrap();
        let (_, pinned) = asset("linux", "x86_64").unwrap();
        let refused = install(&part, "cloudflared-linux-amd64", pinned, &dir).unwrap_err();
        assert!(refused.contains("SHA-256"), "{refused}");
        assert!(!part.exists(), "the download is gone");
        assert!(!dir.join(exe_name()).exists(), "nothing installed");

        std::fs::write(&part, b"").unwrap();
        let empty = sha::hex(&sha::sha256(b""));
        let placed = install(&part, "cloudflared-linux-amd64", &empty, &dir).unwrap();
        assert_eq!(placed, dir.join(exe_name()));
        assert!(placed.is_file() && !part.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A stand-in cloudflared: prints `line` to stderr, then sleeps (or not).
    fn fake(dir: &Path, line: &str, sleep: bool) -> PathBuf {
        if std::env::consts::OS == "windows" {
            let path = dir.join("cloudflared.cmd");
            let wait = if sleep { "ping -n 30 127.0.0.1 >nul\r\n" } else { "" };
            std::fs::write(&path, format!("@echo off\r\necho {} 1>&2\r\n{wait}", line.replace('|', "^|"))).unwrap();
            path
        } else {
            let path = dir.join("cloudflared");
            let wait = if sleep { "exec sleep 30\n" } else { "" };
            std::fs::write(&path, format!("#!/bin/sh\necho '{line}' >&2\n{wait}")).unwrap();
            assert!(Command::new("chmod").arg("755").arg(&path).status().unwrap().success());
            path
        }
    }

    /// One test for everything that starts a (fake) cloudflared: the host
    /// allow-list is one per process, so these must not run side by side.
    #[test]
    fn a_tunnel_lists_its_host_while_up_and_cloudflared_ends_with_it() {
        let dir = scratch("child");
        let at = |name: &str| {
            let sub = dir.join(name);
            std::fs::create_dir_all(&sub).unwrap();
            sub
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let up = fake(&at("up"), "INF |  https://fake-tunnel-words.trycloudflare.com  |", true);
            let mut tunnel = Tunnel::start(&up, "127.0.0.1:9").await.unwrap();
            assert_eq!(tunnel.host, "fake-tunnel-words.trycloudflare.com");
            assert_eq!(host().as_deref(), Some("fake-tunnel-words.trycloudflare.com"), "on the allow-list");
            assert!(!tunnel.exited(), "still running");
            tokio::time::timeout(Duration::from_secs(10), tunnel.stop()).await.expect("killed and reaped");
            assert_eq!(host(), None, "off the allow-list");

            let dropped = Tunnel::start(&up, "127.0.0.1:9").await.unwrap();
            assert!(host().is_some());
            drop(dropped);
            assert_eq!(host(), None, "dropped: off the allow-list too");

            let quiet = fake(&at("quiet"), "INF |  http://not-https.trycloudflare.com  |", false);
            let err = Tunnel::start(&quiet, "127.0.0.1:9").await.err().unwrap();
            assert!(err.contains("no tunnel address"), "{err}");
            assert_eq!(host(), None, "nothing listed for a refused address");

            let missing = Tunnel::start(&dir.join("no-such-cloudflared"), "127.0.0.1:9").await.err().unwrap();
            assert!(missing.contains("cannot start"), "{missing}");

            let short = fake(&at("short"), "|  https://short-lived.trycloudflare.com  |", false);
            let mut ended = Tunnel::start(&short, "127.0.0.1:9").await.unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            while !ended.exited() {
                assert!(tokio::time::Instant::now() < deadline, "cloudflared ended but it was not noticed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(must_stop(0, u64::MAX, false, ended.exited()), Some("cloudflared ended"));
        });
        assert_eq!(host(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn requests_are_taken_once_and_bad_ones_dropped() {
        let bridge = crate::bridge::scratch();
        assert_eq!(take(&bridge), None, "nothing asked");
        bridge.write_private(REQUEST, br#"{"minutes": 30}"#).unwrap();
        assert_eq!(take(&bridge), Some(Some(30)));
        assert_eq!(take(&bridge), None, "taken once");
        bridge.write_private(REQUEST, br#"{"minutes": null}"#).unwrap();
        assert_eq!(take(&bridge), Some(None), "off");
        bridge.write_private(REQUEST, br#"{"minutes": 0}"#).unwrap();
        assert_eq!(take(&bridge), Some(Some(1)), "at least a minute");
        bridge.write_private(REQUEST, br#"{"minutes": 99999999}"#).unwrap();
        assert_eq!(take(&bridge), Some(Some(MAX_MINUTES)), "at most a day");
        bridge.write_private(REQUEST, b"garbage").unwrap();
        assert_eq!(take(&bridge), None, "unreadable: dropped");
        assert!(!bridge.dir.join(REQUEST).exists());
        let _ = std::fs::remove_dir_all(&bridge.dir);
    }

    #[test]
    fn asking_with_no_panel_running_says_so_and_cleans_up() {
        let bridge = crate::bridge::scratch();
        let err = ask(&bridge, None).unwrap_err();
        assert!(err.message.contains("no web panel"), "{}", err.message);
        assert!(!bridge.dir.join(REQUEST).exists(), "the request is not left behind");
        let _ = std::fs::remove_dir_all(&bridge.dir);
    }

    #[test]
    fn a_started_tunnel_prints_its_warning_and_a_pairing_link_through_it() {
        let bridge = crate::bridge::scratch();
        let status = opened(&bridge, "abc.trycloudflare.com", 60, 42, 1_000);
        assert_eq!(status["url"], "https://abc.trycloudflare.com/");
        let link = status["pair"].as_str().unwrap();
        assert!(link.starts_with("https://abc.trycloudflare.com/#pair="), "{link}");
        let text = announce(&status);
        assert!(text.contains("not end-to-end") && text.contains(link) && text.contains("60 minutes"), "{text}");
        let saved: Value = serde_json::from_slice(&std::fs::read(bridge.dir.join(STATUS)).unwrap()).unwrap();
        assert_eq!(saved, status, "what `bridge tunnel on` reads");

        bridge.set_locked(true).unwrap();
        let locked = opened(&bridge, "abc.trycloudflare.com", 60, 42, 1_000);
        assert_eq!(locked["pair"], Value::Null, "no pairing while locked");
        assert!(!announce(&locked).contains("#pair="));
        let _ = std::fs::remove_dir_all(&bridge.dir);
    }
}
