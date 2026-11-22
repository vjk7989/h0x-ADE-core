//! `bridge web --tailscale` with the `tailscale` CLI at hand: its own answer for this
//! machine's tailnet address, and HTTPS through a foreground `tailscale serve` (a secure
//! context for the page: notifications, native WebCrypto). tailscaled drops a foreground
//! serve config when the CLI exits, so killing the child undoes it.

use super::http::Request;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long `tailscale serve` has to start listening on the tailnet address.
const READY: Duration = Duration::from_secs(8);

/// What a device is told when serve never came up (typically HTTPS is off for the tailnet).
pub const HTTPS_OFF: &str = "HTTPS isn't on for this tailnet; turn it on in the Tailscale admin console \
                             (DNS, Enable HTTPS); using http";

/// The `tailscale` CLI: the first on `path` (a PATH value), else the Windows installer's.
pub fn cli(path: Option<&std::ffi::OsStr>, program_files: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let name = if cfg!(windows) { "tailscale.exe" } else { "tailscale" };
    // Absolute entries only: an empty or relative one would run whatever sits in the current folder.
    let on_path = path.into_iter().flat_map(std::env::split_paths).filter(|dir| dir.is_absolute());
    let installed = program_files.map(|dir| Path::new(dir).join("Tailscale"));
    on_path.chain(installed).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// The CLI on this machine.
pub fn system_cli() -> Option<PathBuf> {
    let program_files = if cfg!(windows) { std::env::var_os("ProgramFiles") } else { None };
    cli(std::env::var_os("PATH").as_deref(), program_files.as_deref())
}

/// The first line of `tailscale ip -4`, if it is an address in Tailscale's 100.64.0.0/10.
pub fn first_address(output: &str) -> Option<Ipv4Addr> {
    let ip = output.lines().next()?.trim().parse::<Ipv4Addr>().ok()?;
    super::tailscale_address(Some(IpAddr::V4(ip))).ok()
}

/// `.Self.DNSName` of `tailscale status --json`, without its trailing dot. None when
/// absent, empty or not a plain host name.
pub fn dns_name(status: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(status).ok()?;
    let name = value.get("Self")?.get("DNSName")?.as_str()?.trim().trim_end_matches('.');
    let plain = name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    let labels = name.split('.').all(|label| label.len() <= 63);
    (!name.is_empty() && name.len() <= 253 && labels && plain && !name.starts_with(['.', '-']))
        .then(|| name.to_string())
}

fn run(program: &Path, args: &[&str]) -> Option<String> {
    let out = command(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn command(program: &Path) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// This machine's tailnet address: the CLI's when it has one, else `route` (the OS's
/// route to Tailscale's service IP); either way only inside 100.64.0.0/10.
pub fn address(cli: Option<&Path>, route: Option<IpAddr>) -> Result<Ipv4Addr, String> {
    cli.and_then(|cli| run(cli, &["ip", "-4"]))
        .and_then(|out| first_address(&out))
        .map_or_else(|| super::tailscale_address(route), Ok)
}

/// This machine's MagicDNS name, from the CLI.
pub fn machine_name(cli: &Path) -> Option<String> {
    dns_name(&run(cli, &["status", "--json"])?)
}

/// A running `tailscale serve --https=<port>` for this panel; killed on drop.
pub struct Serve {
    child: Child,
    /// The Host (and Origin) the proxy forwards: `<name>:<port>`.
    pub host: String,
}

impl Serve {
    /// Starts serving `http://127.0.0.1:<port>` as `https://<name>:<port>` on `tailnet`, and
    /// waits for the proxy to listen. Killed again if it exits or is not up in time.
    pub fn start(cli: &Path, tailnet: Ipv4Addr, port: u16, name: &str) -> Result<Serve, String> {
        let child = command(cli)
            .arg("serve")
            .arg(format!("--https={port}"))
            .arg(format!("http://127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("tailscale serve: {e}"))?;
        let mut serve = Serve { child, host: format!("{name}:{port}") };
        let deadline = Instant::now() + READY;
        let target = SocketAddr::new(IpAddr::V4(tailnet), port);
        while Instant::now() < deadline {
            if !matches!(serve.child.try_wait(), Ok(None)) {
                break;
            }
            if TcpStream::connect_timeout(&target, Duration::from_millis(250)).is_ok() {
                return Ok(serve);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(HTTPS_OFF.into())
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `host` as a browser writes it: https drops the default port 443.
pub fn bare(host: &str) -> &str {
    host.strip_suffix(":443").unwrap_or(host)
}

/// serve's page is https, so the browser's `Origin` is `https://<host>` (no `:443`); the
/// listener's check knows `http://` only. Makes exactly that origin look like its http form.
pub fn origin_as_http(req: &mut Request, host: &str) {
    for (name, value) in &mut req.headers {
        let rest = value.get(8..).filter(|_| value[..8].eq_ignore_ascii_case("https://"));
        let ours = |rest: &str| rest.eq_ignore_ascii_case(host) || rest.eq_ignore_ascii_case(bare(host));
        if name == "origin" && rest.is_some_and(ours) {
            *value = format!("http://{host}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_lines_must_be_tailnet_ipv4() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(first_address("100.101.102.103\n"), Some(ip("100.101.102.103")));
        assert_eq!(first_address("100.64.0.1\r\n100.64.0.2\n"), Some(ip("100.64.0.1")), "first line wins");
        for bad in ["", "\n", "192.168.1.5\n", "100.128.0.1\n", "fd7a:115c:a1e0::1\n", "garbage\n", "\n100.64.0.1\n"] {
            assert_eq!(first_address(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn machine_name_comes_from_self_dnsname() {
        let status = |name: &str| format!(r#"{{"Self": {{"DNSName": "{name}"}}, "Peer": {{}}}}"#);
        assert_eq!(dns_name(&status("box.tail1234.ts.net.")).as_deref(), Some("box.tail1234.ts.net"));
        assert_eq!(dns_name(&status("box.tail1234.ts.net")).as_deref(), Some("box.tail1234.ts.net"));
        for bad in ["", ".", "bad host.ts.net.", "a/b.ts.net.", "-x.ts.net.", "x.ts.net:80."] {
            assert_eq!(dns_name(&status(bad)), None, "{bad:?}");
        }
        for bad in ["", "not json", "{}", r#"{"Self": {}}"#, r#"{"Self": {"DNSName": 5}}"#, r#"{"Self": null}"#] {
            assert_eq!(dns_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn without_the_cli_the_route_lookup_decides() {
        let route = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        assert_eq!(address(None, route("100.90.1.2")), Ok("100.90.1.2".parse().unwrap()));
        assert!(address(None, route("192.168.1.2")).unwrap_err().contains("Tailscale isn't connected"));
        assert!(address(None, None).is_err());
        let missing = Path::new("definitely-not-a-tailscale-cli");
        assert!(address(Some(missing), route("10.0.0.1")).is_err(), "an unusable CLI falls back, then refuses");
        assert_eq!(address(Some(missing), route("100.64.0.9")), Ok("100.64.0.9".parse().unwrap()));
    }

    #[test]
    fn the_cli_is_found_on_an_absolute_path_entry_only() {
        let dir = std::env::temp_dir().join(format!("h0x-ts-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "tailscale.exe" } else { "tailscale" };
        assert_eq!(cli(Some(dir.as_os_str()), None), None, "nothing there yet");
        std::fs::write(dir.join(name), b"").unwrap();
        assert_eq!(cli(Some(dir.as_os_str()), None), Some(dir.join(name)));
        assert_eq!(cli(Some(std::ffi::OsStr::new("relative")), None), None);
        assert_eq!(cli(None, None), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A request head as `parse` takes it: lines joined by CRLF, no blank line at the end.
    fn head(lines: &[String]) -> crate::bridge::web::http::Request {
        crate::bridge::web::http::parse(lines.join("\r\n").as_bytes()).unwrap()
    }

    #[test]
    fn port_443_is_written_without_its_port_by_browsers() {
        let host = "box.tail1234.ts.net:443";
        let name = "box.tail1234.ts.net";
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let get = |host: &str, origin: &str, allowed: &[&str]| {
            let mut req = head(&["GET / HTTP/1.1".into(), format!("Host: {host}"), format!("Origin: {origin}")]);
            origin_as_http(&mut req, "box.tail1234.ts.net:443");
            let allowed: Vec<String> = allowed.iter().map(|h| h.to_string()).collect();
            super::super::http::allowed(&req, &allowed)
        };
        let hosts = [host, name];
        assert_eq!(get(name, &format!("https://{name}"), &hosts), Ok(()));
        assert_eq!(get(host, &format!("https://{host}"), &hosts), Ok(()));
        assert_eq!(get(name, "https://evil.example", &hosts), Err(403));
        // Another port: the bare name is not ours, and neither is its origin.
        let other = ["box.tail1234.ts.net:8377"];
        let mut req = head(&["GET / HTTP/1.1".into(), format!("Host: {name}"), format!("Origin: https://{name}")]);
        origin_as_http(&mut req, other[0]);
        assert_eq!(super::super::http::allowed(&req, &[other[0].to_string()]), Err(403));
        let xff = head(&["GET / HTTP/1.1".into(), format!("Host: {name}"), "X-Forwarded-For: 100.64.0.7".into()]);
        let client = crate::bridge::web::tunnel::client;
        assert_eq!(client(&xff, Some(lo), false, Some(bare(host))), Some("100.64.0.7".parse().unwrap()));
        assert_eq!(client(&xff, Some(lo), false, Some(bare(other[0]))), Some(lo), "port 8377 is not 443");
    }

    #[test]
    fn host_names_obey_the_dns_limits() {
        let status = |name: &str| format!(r#"{{"Self": {{"DNSName": "{name}."}}}}"#);
        let label = |n: usize| "a".repeat(n);
        let long = |total: usize| {
            // labels of 63, joined by dots, then a last label to reach `total`.
            let mut name = String::new();
            while total - name.len() > 64 {
                name.push_str(&label(63));
                name.push('.');
            }
            name.push_str(&label(total - name.len()));
            name
        };
        assert_eq!(long(253).len(), 253);
        assert!(dns_name(&status(&long(253))).is_some());
        assert_eq!(dns_name(&status(&long(254))), None);
        assert!(dns_name(&status(&label(63))).is_some());
        assert_eq!(dns_name(&status(&label(64))), None);
        assert_eq!(dns_name(&status(&format!("{}.ts.net", label(64)))), None);
    }

    #[test]
    fn only_the_serve_origin_is_taken_as_ours() {
        let host = "box.tail1234.ts.net:8377";
        let get = |origin: &str| {
            let mut req = head(&["GET / HTTP/1.1".into(), format!("Host: {host}"), format!("Origin: {origin}")]);
            origin_as_http(&mut req, host);
            super::super::http::allowed(&req, &[host.to_string()])
        };
        assert_eq!(get(&format!("https://{host}")), Ok(()));
        assert_eq!(get(&format!("https://{}", host.to_uppercase())), Ok(()));
        for bad in
            ["https://evil.example", "https://box.tail1234.ts.net", "https://127.0.0.1:8377", "http://evil.example"]
        {
            assert_eq!(get(bad), Err(403), "{bad}");
        }
    }

    #[test]
    fn serve_requests_are_attributed_to_the_forwarded_client() {
        let host = "box.tail1234.ts.net:8377";
        let client = crate::bridge::web::tunnel::client;
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let from = |host: &str| {
            head(&["GET / HTTP/1.1".into(), format!("Host: {host}"), "X-Forwarded-For: 100.64.0.7".into()])
        };
        let via = from(host);
        assert_eq!(client(&via, Some(lo), false, Some(host)), Some("100.64.0.7".parse().unwrap()));
        assert_eq!(client(&via, Some(lo), false, None), Some(lo), "not in serve mode");
        assert_eq!(client(&from("127.0.0.1:8377"), Some(lo), false, Some(host)), Some(lo), "another Host");
        let remote: IpAddr = "100.64.0.9".parse().unwrap();
        assert_eq!(client(&via, Some(remote), false, Some(host)), Some(remote), "only loopback peers are proxies");
    }

    #[test]
    fn the_first_ip_line_is_checked_at_the_range_edges_and_trimmed() {
        let ip = |s: &str| Some(s.parse::<Ipv4Addr>().unwrap());
        assert_eq!(first_address("100.64.0.0\n"), ip("100.64.0.0"), "lowest tailnet address");
        assert_eq!(first_address("100.127.255.255\n"), ip("100.127.255.255"), "highest tailnet address");
        assert_eq!(first_address("  100.64.0.1 \t\r\n"), ip("100.64.0.1"), "spaces and CRLF around it");
        assert_eq!(first_address("100.64.0.1"), ip("100.64.0.1"), "no line end at all");
        for bad in [
            "   \n100.64.0.1\n",
            "\r\n",
            "100.63.255.255\n",
            "100.128.0.0\n",
            "fd7a:115c:a1e0::1\n100.64.0.1\n",
            "100.064.0.1\n",
            "100.64.0.256\n",
            "100.64.0.1/32\n",
            "100.64.0.1 100.64.0.2\n",
        ] {
            assert_eq!(first_address(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn odd_dns_names_are_refused_and_good_ones_keep_their_case() {
        let status = |name: &str| format!(r#"{{"Self": {{"DNSName": "{name}"}}}}"#);
        let name = |s: &str| dns_name(&status(s));
        assert_eq!(name(" Box.Tail1234.ts.net. ").as_deref(), Some("Box.Tail1234.ts.net"), "trimmed, case kept");
        assert_eq!(name("box.tail1234.ts.net..").as_deref(), Some("box.tail1234.ts.net"), "every trailing dot");
        for bad in [
            "...",
            "   ",
            "-",
            ".box.ts.net",
            "box tail.ts.net.",
            "box\\u0009x.ts.net.",
            "b\u{f8}x.ts.net.",
            "user@box.ts.net.",
            "box%2f.ts.net.",
            "box_x.ts.net.",
            "box.ts.net:443.",
            "box.ts.net/",
        ] {
            assert_eq!(name(bad), None, "{bad:?}");
        }
        for bad in [
            r#"{"Self": {"DNSName": null}}"#,
            r#"{"Self": {"DNSName": ["box.ts.net."]}}"#,
            r#"{"Self": []}"#,
            r#"{"self": {"DNSName": "box.ts.net."}}"#,
            "[]",
            "null",
        ] {
            assert_eq!(dns_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_installed_cli_is_found_under_program_files_with_spaces() {
        let root = std::env::temp_dir().join(format!("h0x-ts-pf-{}", crate::platform::random_hex()));
        let name = if cfg!(windows) { "tailscale.exe" } else { "tailscale" };
        let program_files = root.join("Program Files");
        let installed = program_files.join("Tailscale").join(name);
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, b"").unwrap();
        let pf = Some(program_files.as_os_str());
        assert_eq!(cli(None, pf), Some(installed.clone()), "no PATH at all");
        let sep = if cfg!(windows) { ';' } else { ':' };
        let odd = std::ffi::OsString::from(format!("relative{sep}{sep}."));
        assert_eq!(cli(Some(&odd), pf), Some(installed.clone()), "relative and empty entries are skipped");
        assert_eq!(cli(Some(&odd), None), None, "missing ProgramFiles: nothing else to try");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(cli(None, Some(empty.as_os_str())), None, "an installer folder without the CLI");
        // A folder that only carries the name is not the CLI; a real file on PATH beats the installer's.
        let bin = root.join("Program Files").join("bin dir");
        std::fs::create_dir_all(bin.join(name)).unwrap();
        let path = std::env::join_paths([bin.clone()]).unwrap();
        assert_eq!(cli(Some(&path), pf), Some(installed), "a folder named like the CLI is skipped");
        std::fs::remove_dir(bin.join(name)).unwrap();
        std::fs::write(bin.join(name), b"").unwrap();
        assert_eq!(cli(Some(&path), pf), Some(bin.join(name)), "PATH first");
        if cfg!(windows) {
            let quoted = std::ffi::OsString::from(format!("relative;\"{}\"", bin.display()));
            assert_eq!(cli(Some(&quoted), None), Some(bin.join(name)), "a quoted entry with spaces");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A stand-in `tailscale` in `dir`: `sh` as a shell script, or `cmd` as a batch file on Windows.
    fn fake(dir: &Path, sh: &str, cmd: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        if cfg!(windows) {
            let path = dir.join("tailscale.cmd");
            std::fs::write(&path, format!("@echo off\r\n{cmd}\r\n")).unwrap();
            path
        } else {
            let path = dir.join("tailscale");
            std::fs::write(&path, format!("#!/bin/sh\n{sh}\n")).unwrap();
            assert!(Command::new("chmod").arg("755").arg(&path).status().unwrap().success());
            path
        }
    }

    #[test]
    fn the_cli_address_wins_and_any_bad_answer_falls_back_to_the_route() {
        let root = std::env::temp_dir().join(format!("h0x-ts-ip-{}", crate::platform::random_hex()));
        let route = Some("100.90.1.2".parse::<IpAddr>().unwrap());
        let ok = fake(&root.join("ok"), "echo 100.100.1.2", "echo 100.100.1.2");
        assert_eq!(address(Some(&ok), route), Ok("100.100.1.2".parse().unwrap()), "the CLI's answer first");
        let lan = fake(&root.join("lan"), "echo 192.168.1.5", "echo 192.168.1.5");
        assert_eq!(address(Some(&lan), route), Ok("100.90.1.2".parse().unwrap()), "not a tailnet address");
        let failed = fake(&root.join("failed"), "echo 100.100.1.2\nexit 3", "echo 100.100.1.2\r\nexit /b 3");
        assert_eq!(address(Some(&failed), route), Ok("100.90.1.2".parse().unwrap()), "a failing CLI");
        let silent = fake(&root.join("silent"), "exit 0", "exit /b 0");
        assert_eq!(address(Some(&silent), route), Ok("100.90.1.2".parse().unwrap()), "no output");
        assert!(address(Some(&silent), None).is_err(), "no output and no route");
        let v6 = fake(&root.join("v6"), "echo fd7a:115c:a1e0::1", "echo fd7a:115c:a1e0::1");
        assert!(address(Some(&v6), Some("192.168.1.2".parse().unwrap())).is_err(), "IPv6 only, off-tailnet route");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_machine_name_is_read_only_from_a_successful_status() {
        let root = std::env::temp_dir().join(format!("h0x-ts-name-{}", crate::platform::random_hex()));
        let json = r#"{"Self":{"DNSName":"Box.tail1.ts.net."}}"#;
        let ok = fake(&root.join("ok"), &format!("echo '{json}'"), &format!("echo {json}"));
        assert_eq!(machine_name(&ok).as_deref(), Some("Box.tail1.ts.net"));
        let failed =
            fake(&root.join("failed"), &format!("echo '{json}'\nexit 1"), &format!("echo {json}\r\nexit /b 1"));
        assert_eq!(machine_name(&failed), None, "a failing status");
        assert_eq!(machine_name(&root.join("missing")), None, "no CLI there");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A loopback port nothing listens on (bound, then let go).
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn a_serve_that_exits_or_cannot_start_fails_fast() {
        let root = std::env::temp_dir().join(format!("h0x-ts-exit-{}", crate::platform::random_hex()));
        let quits = fake(&root, "exit 1", "exit /b 1");
        let began = Instant::now();
        let failed = Serve::start(&quits, Ipv4Addr::LOCALHOST, closed_port(), "box.ts.net").err();
        assert_eq!(failed.as_deref(), Some(HTTPS_OFF));
        assert!(began.elapsed() < Duration::from_secs(4), "no 8 s wait: {:?}", began.elapsed());
        let began = Instant::now();
        let missing = Serve::start(&root.join("missing"), Ipv4Addr::LOCALHOST, closed_port(), "box.ts.net").err();
        assert!(missing.as_deref().is_some_and(|e| e.starts_with("tailscale serve: ")), "{missing:?}");
        assert!(began.elapsed() < Duration::from_secs(1), "{:?}", began.elapsed());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_serve_that_never_listens_times_out_and_is_killed() {
        let root = std::env::temp_dir().join(format!("h0x-ts-slow-{}", crate::platform::random_hex()));
        let survived = root.join("survived");
        let s = survived.display();
        let slow =
            fake(&root, &format!("sleep 10\ntouch '{s}'"), &format!("ping -n 11 127.0.0.1 >nul\r\n>\"{s}\" echo x"));
        let began = Instant::now();
        let failed = Serve::start(&slow, Ipv4Addr::LOCALHOST, closed_port(), "box.ts.net").err();
        assert_eq!(failed.as_deref(), Some(HTTPS_OFF));
        assert!(began.elapsed() >= Duration::from_secs(7), "it waited for the port: {:?}", began.elapsed());
        std::thread::sleep(Duration::from_secs(12).saturating_sub(began.elapsed()));
        assert!(!survived.exists(), "the timed-out serve was killed, not left running");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_ready_serve_gets_its_arguments_and_dropping_it_kills_the_child() {
        let root = std::env::temp_dir().join(format!("h0x-ts-ready-{}", crate::platform::random_hex()));
        let (args, survived) = (root.join("args"), root.join("survived"));
        let (a, s) = (args.display(), survived.display());
        let sh = format!("echo \"$@\" > '{a}'\nsleep 3\ntouch '{s}'");
        let cmd = format!(">\"{a}\" echo %*\r\nping -n 4 127.0.0.1 >nul\r\n>\"{s}\" echo x");
        let up = fake(&root, &sh, &cmd);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let began = Instant::now();
        let serve = Serve::start(&up, Ipv4Addr::LOCALHOST, port, "box.ts.net").unwrap();
        assert!(began.elapsed() < Duration::from_secs(3), "ready as soon as the port answers: {:?}", began.elapsed());
        assert_eq!(serve.host, format!("box.ts.net:{port}"));
        let wrote = |p: &Path| std::fs::read_to_string(p).is_ok_and(|t| t.ends_with('\n'));
        while !wrote(&args) && began.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let line = std::fs::read_to_string(&args).unwrap().replace('"', "");
        assert_eq!(line.trim(), format!("serve --https={port} http://127.0.0.1:{port}"));
        let dropped = Instant::now();
        drop(serve);
        assert!(dropped.elapsed() < Duration::from_secs(2), "drop kills, not waits: {:?}", dropped.elapsed());
        std::thread::sleep(Duration::from_millis(4500));
        assert!(!survived.exists(), "the dropped serve was killed");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_origin_rewrite_leaves_every_near_miss_alone() {
        let host = "box.tail1234.ts.net:8377";
        let hosts = [host.to_string(), "127.0.0.1:8377".into(), "localhost:8377".into()];
        let rewrite = |origins: &[&str]| {
            let mut lines =
                vec!["GET / HTTP/1.1".to_string(), format!("Host: {host}"), format!("Referer: https://{host}/")];
            lines.extend(origins.iter().map(|o| format!("Origin: {o}")));
            let mut req = head(&lines);
            origin_as_http(&mut req, host);
            let checked = super::super::http::allowed(&req, &hosts);
            (req.header("origin").map(String::from), req.header("referer").map(String::from), checked)
        };
        let (origin, referer, checked) = rewrite(&["HTTPS://BOX.tail1234.ts.net:8377"]);
        assert_eq!(origin.as_deref(), Some("http://box.tail1234.ts.net:8377"), "scheme and name in any case");
        assert_eq!(referer.as_deref(), Some("https://box.tail1234.ts.net:8377/"), "only Origin is rewritten");
        assert_eq!(checked, Ok(()));
        for near in [
            "https://box.tail1234.ts.net:8378",
            "https://box.tail1234.ts.net:8377/",
            "https://u@box.tail1234.ts.net:8377",
            "https://other.tail1234.ts.net:8377",
            "https://box.tail1234.ts.net:8377.evil",
            "https://box.tail1234.ts.net",
            "https://",
            "https:/box.tail1234.ts.net:8377",
            "https://b\u{f8}x.tail1234.ts.net:8377",
            "\u{e9}\u{e9}\u{e9}\u{e9}x",
            "null",
            "",
        ] {
            let (origin, _, checked) = rewrite(&[near]);
            assert_eq!(origin.as_deref(), Some(near), "{near:?} untouched");
            assert_eq!(checked, Err(403), "{near:?}");
        }
        let (origin, _, checked) = rewrite(&["https://evil.example", &format!("https://{host}")]);
        assert_eq!(
            (origin.as_deref(), checked),
            (Some("https://evil.example"), Err(403)),
            "a second Origin does not help"
        );
    }

    #[test]
    fn forwarded_addresses_count_only_from_loopback_with_the_exact_serve_host() {
        let serve = Some("box.tail1234.ts.net:8377");
        let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        let req = |host: &str, extra: &str| head(&["GET /ws HTTP/1.1".into(), format!("Host: {host}"), extra.into()]);
        let seen = |r: &Request, peer: &str| crate::bridge::web::tunnel::client(r, ip(peer), false, serve);
        let hops = req("box.tail1234.ts.net:8377", "X-Forwarded-For: 6.6.6.6, 100.64.0.7");
        assert_eq!(seen(&hops, "127.0.0.1"), ip("100.64.0.7"), "the last hop, the one serve added");
        assert_eq!(seen(&hops, "::1"), ip("100.64.0.7"), "IPv6 loopback");
        assert_eq!(seen(&hops, "::ffff:127.0.0.1"), ip("100.64.0.7"), "mapped loopback");
        let upper = req("BOX.Tail1234.TS.NET:8377", "X-Forwarded-For: 100.64.0.7");
        assert_eq!(seen(&upper, "127.0.0.1"), ip("100.64.0.7"), "Host in any case");
        let real = req("box.tail1234.ts.net:8377", "X-Real-IP: 100.64.0.8");
        assert_eq!(seen(&real, "::1"), ip("100.64.0.8"), "X-Real-IP without X-Forwarded-For");
        for spoofer in ["100.64.0.9", "192.168.1.20", "fd7a:115c:a1e0::9", "::ffff:192.168.1.20"] {
            assert_eq!(seen(&hops, spoofer), ip(spoofer), "{spoofer}: not a local proxy");
        }
        for other in [
            "box.tail1234.ts.net",
            "box.tail1234.ts.net:8378",
            "box.tail1234.ts.net:443",
            "127.0.0.1:8377",
            "localhost:8377",
            "other.tail1234.ts.net:8377",
            "box.tail1234.ts.net.:8377",
        ] {
            let spoofed = req(other, "X-Forwarded-For: 100.64.0.7");
            assert_eq!(seen(&spoofed, "127.0.0.1"), ip("127.0.0.1"), "{other}: not the serve host");
        }
    }
}
