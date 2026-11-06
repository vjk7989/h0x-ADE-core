//! `bridge web --via <machine> --public-host <name>`: reach the web panel
//! through the user's own VPS. A managed `ssh -N -R` opens a port on the VPS's
//! loopback only; the user's own HTTPS reverse proxy there serves it under
//! `<name>`. Pairing and session auth apply unchanged.

use super::http::Request;
use crate::machines::{Machine, check_target, unknown};
use crate::paths::Paths;
use crate::rpc::RpcError;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What `--via`, `--public-host` and `--remote-port` ask for, checked.
#[derive(Debug, PartialEq)]
pub struct Vps {
    /// The ssh target of the machine.
    pub target: String,
    /// Lowercase DNS name the reverse proxy serves.
    pub public_host: String,
    pub remote_port: u16,
}

/// A DNS hostname for the reverse proxy: ASCII letters, digits and hyphens in
/// 2 or more labels (punycode `xn--` for non-ASCII), no IP literal, port,
/// userinfo, wildcard or trailing dot, at most 253 characters. Lowercased.
pub fn check_public_host(name: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("bad --public-host {name:?}: {why}"));
    if name.is_empty() || name.len() > 253 {
        return bad("1 to 253 characters");
    }
    if !name.is_ascii() {
        return bad("ASCII only (write international names as punycode, xn--...)");
    }
    let name = name.to_ascii_lowercase();
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return bad("a full DNS name like h0x.example.com");
    }
    for label in &labels {
        let ok = (1..=63).contains(&label.len())
            && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return bad("letters, digits and hyphens only; no port, userinfo, wildcard, empty label or trailing dot");
        }
    }
    if labels.last().is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit())) {
        return bad("an IP address is not a DNS name");
    }
    if labels.iter().any(|l| l.starts_with("xn--") && l.len() == 4) {
        return bad("empty punycode label");
    }
    Ok(name)
}

pub fn check_remote_port(port: u16) -> Result<u16, String> {
    if port < 1024 {
        return Err(format!("bad --remote-port {port}: 1024 to 65535"));
    }
    Ok(port)
}

/// Checks the three options against the stored machines.
pub fn plan(paths: &Paths, machine: &str, public_host: &str, remote_port: u16) -> Result<Vps, RpcError> {
    let invalid = |e: String| RpcError::invalid_params(e);
    let public_host = check_public_host(public_host).map_err(invalid)?;
    let remote_port = check_remote_port(remote_port).map_err(invalid)?;
    let machines: Vec<Machine> =
        crate::paths::read_state(&paths.state.join("machines.json")).map_err(|e| RpcError::internal(e.to_string()))?;
    let found = machines.iter().find(|m| m.name == machine).ok_or_else(|| unknown(&machines, machine))?;
    if found.via {
        return Err(RpcError::invalid_params(format!(
            "machine {machine:?} connects with a custom command; --via needs a plain ssh machine"
        )));
    }
    check_target(found.reach())?;
    Ok(Vps { target: found.target.clone(), public_host, remote_port })
}

/// ssh's arguments: the reverse port binds the VPS's loopback only, and
/// nothing a user's ssh config says can keep ssh up half-working or ask a
/// question. No agent forwarding, no `GatewayPorts`-style wider bind.
pub fn argv(target: &str, remote_port: u16, local_port: u16) -> Vec<String> {
    let mut args: Vec<String> = ["-N"].map(String::from).into();
    for option in [
        "ExitOnForwardFailure=yes",
        "ServerAliveInterval=15",
        "BatchMode=yes",
        "ForwardAgent=no",
        "ControlMaster=no",
        "ControlPath=none",
    ] {
        args.extend(["-o".into(), option.into()]);
    }
    args.extend(["-R".into(), format!("127.0.0.1:{remote_port}:127.0.0.1:{local_port}"), "--".into(), target.into()]);
    args
}

/// The ssh to run: on Windows the system's OpenSSH when it is there (Git's or
/// another ssh earlier on PATH may not behave), else `ssh` from PATH.
pub fn ssh_program(system_root: Option<&str>, exists: impl Fn(&str) -> bool) -> String {
    if let Some(root) = system_root.map(|r| r.trim_end_matches(['\\', '/'])).filter(|r| !r.is_empty()) {
        let system = format!(r"{root}\System32\OpenSSH\ssh.exe");
        if exists(&system) {
            return system;
        }
    }
    "ssh".into()
}

fn command(target: &str, remote_port: u16, local_port: u16) -> Command {
    let program = if cfg!(windows) {
        let root = std::env::var("SystemRoot").ok();
        ssh_program(root.as_deref(), |p| std::path::Path::new(p).is_file())
    } else {
        "ssh".into()
    };
    let mut cmd = Command::new(program);
    cmd.args(argv(target, remote_port, local_port));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// The Host values of the listener, plus the public name (no port: the proxy
/// serves it on 443).
pub fn with_public(mut base: Vec<String>, public_host: Option<&str>) -> Vec<String> {
    base.extend(public_host.map(String::from));
    base
}

/// The page is served over https by the proxy, so the browser's `Origin` is
/// `https://<public-host>`; the listener's check knows `http://` only. Makes
/// that one origin (a host of ours without a port) look like its http form.
pub fn https_origin_as_http(req: &mut Request, hosts: &[String]) {
    for (name, value) in &mut req.headers {
        let rest = value.strip_prefix("https://").map(String::from);
        if name == "origin"
            && let Some(rest) = rest
            && hosts.iter().any(|h| !h.contains(':') && h.eq_ignore_ascii_case(&rest))
        {
            *value = format!("http://{rest}");
        }
    }
}

/// How many failures in a row to count after a run: one that lasted this long
/// was a working tunnel, so it starts over.
const STABLE: Duration = Duration::from_secs(30);

/// The failure count after ssh ran for `ran` and ended.
pub fn failures_after(failures: u32, ran: Duration) -> u32 {
    if ran >= STABLE { 1 } else { failures.saturating_add(1) }
}

/// A managed ssh: restarted with backoff when it ends, stopped (killed) while
/// the bridge is locked, on `stop` and on drop.
pub struct Tunnel {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Tunnel {
    /// `make` builds the command to run each time; `locked` says whether
    /// remote control is locked; `tick` is how often both are looked at.
    pub fn start(
        make: impl Fn() -> Command + Send + 'static,
        locked: impl Fn() -> bool + Send + 'static,
        tick: Duration,
    ) -> Tunnel {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || supervise(&make, &locked, tick, &flag));
        Tunnel { stop, thread: Some(thread) }
    }

    /// Starts ssh for `plan` toward the listener on `local_port`.
    pub fn ssh(plan: &Vps, local_port: u16, locked: impl Fn() -> bool + Send + 'static) -> Tunnel {
        let (target, remote) = (plan.target.clone(), plan.remote_port);
        Tunnel::start(move || command(&target, remote, local_port), locked, Duration::from_secs(1))
    }

    /// Kills ssh and waits for it.
    pub fn stop(mut self) {
        self.end();
    }

    fn end(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.end();
    }
}

fn nap(total: Duration, tick: Duration, stop: &AtomicBool) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(tick.min(end.saturating_duration_since(Instant::now())));
    }
}

fn supervise(make: &dyn Fn() -> Command, locked: &dyn Fn() -> bool, tick: Duration, stop: &AtomicBool) {
    let mut failures = 0;
    while !stop.load(Ordering::SeqCst) {
        if locked() {
            nap(tick, tick, stop);
            continue;
        }
        let mut cmd = make();
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("bridge: vps: cannot start ssh: {e}");
                failures += 1;
                nap(super::super::telegram::backoff(failures), tick, stop);
                continue;
            }
        };
        let started = Instant::now();
        let mut ended = false;
        while !ended {
            if stop.load(Ordering::SeqCst) || locked() {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            match child.try_wait() {
                Ok(None) => std::thread::sleep(tick),
                Ok(Some(status)) => {
                    eprintln!("bridge: vps: ssh ended ({status}); restarting");
                    ended = true;
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    ended = true;
                }
            }
        }
        if ended {
            failures = failures_after(failures, started.elapsed());
            nap(super::super::telegram::backoff(failures), tick, stop);
        }
    }
}

#[cfg(test)]
mod tests;
