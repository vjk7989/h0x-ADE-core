use super::*;
use std::path::PathBuf;

fn host(name: &str) -> Result<String, String> {
    check_public_host(name)
}

#[test]
fn public_host_accepts_dns_names_and_lowercases() {
    assert_eq!(host("h0x.example.com").unwrap(), "h0x.example.com");
    assert_eq!(host("H0X.Example.COM").unwrap(), "h0x.example.com");
    assert_eq!(host("a-b.c1.io").unwrap(), "a-b.c1.io");
    assert_eq!(host("xn--bcher-kva.example").unwrap(), "xn--bcher-kva.example", "punycode");
    assert_eq!(host(&format!("{}.com", "a".repeat(63))).unwrap().len(), 67, "longest label");
}

#[test]
fn public_host_rejects_everything_else() {
    let long_label = format!("{}.com", "a".repeat(64));
    let too_long = format!("{}.com", vec!["a".repeat(60); 5].join("."));
    let bad = [
        "",
        "localhost",
        "example",
        "1.2.3.4",
        "1.2.3.4.",
        "10.0.0.1.example.123",
        "[::1]",
        "::1",
        "example.com:443",
        "example.com:",
        "user@example.com",
        "user:pw@example.com",
        "*.example.com",
        "ex*mple.com",
        "example.com.",
        ".example.com",
        "a..example.com",
        "-a.example.com",
        "a-.example.com",
        "exa mple.com",
        "example.com/path",
        "https://example.com",
        "example.com\n",
        "ex_ample.com",
        // look-alikes: Cyrillic a, fullwidth dot, Turkish dotless i, trailing-space
        "ex\u{430}mple.com",
        "example\u{ff0e}com",
        "b\u{fc}cher.example",
        "example.com ",
        "xn--.example.com",
        &long_label,
        &too_long,
    ];
    for name in bad {
        assert!(host(name).is_err(), "{name:?} must be refused");
    }
}

#[test]
fn remote_port_bounds() {
    assert!(check_remote_port(0).is_err());
    assert!(check_remote_port(1).is_err());
    assert!(check_remote_port(1023).is_err());
    assert_eq!(check_remote_port(1024), Ok(1024));
    assert_eq!(check_remote_port(8377), Ok(8377));
    assert_eq!(check_remote_port(65535), Ok(65535));
}

#[test]
fn argv_binds_the_vps_loopback_only_with_forced_options() {
    let args = argv("me@vps.example.com", 8377, 54321);
    assert_eq!(args.first().map(String::as_str), Some("-N"));
    let at = args.iter().position(|a| a == "-R").unwrap();
    assert_eq!(args[at + 1], "127.0.0.1:8377:127.0.0.1:54321");
    assert_eq!(args[args.len() - 2..], ["--", "me@vps.example.com"], "target last, after --");
    for forced in ["ExitOnForwardFailure=yes", "ServerAliveInterval=15", "BatchMode=yes"] {
        let at = args.iter().position(|a| a == forced).unwrap_or_else(|| panic!("{forced} missing"));
        assert_eq!(args[at - 1], "-o");
    }
    for never in ["-a", "-A", "-g", "-L", "-D", "-X", "-Y"] {
        assert!(!args.iter().any(|a| a == never), "{never} must not be passed");
    }
    assert!(args.contains(&"ForwardAgent=no".to_string()));
    let forwards: Vec<_> = args.iter().filter(|a| a.contains("0.0.0.0") || a.starts_with("*:")).collect();
    assert!(forwards.is_empty(), "never a wide bind: {forwards:?}");
}

#[test]
fn a_target_that_looks_like_an_option_stays_after_the_separator() {
    let args = argv("-oProxyCommand=x", 8377, 1);
    let at = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(args[at + 1], "-oProxyCommand=x", "plan() refuses it; argv still cannot make it an option");
}

#[test]
fn windows_prefers_the_system_openssh() {
    let system = r"C:\Windows\System32\OpenSSH\ssh.exe";
    assert_eq!(ssh_program(Some(r"C:\Windows"), |p| p == system), system);
    assert_eq!(ssh_program(Some(r"C:\Windows\"), |p| p == system), system, "trailing separator");
    assert_eq!(ssh_program(Some(r"D:\Win dows"), |_| true), r"D:\Win dows\System32\OpenSSH\ssh.exe", "spaces");
    assert_eq!(ssh_program(Some(r"C:\Windows"), |_| false), "ssh", "not installed: PATH");
    assert_eq!(ssh_program(None, |_| true), "ssh");
    assert_eq!(ssh_program(Some(""), |_| true), "ssh");
}

fn scratch_paths(machines: &str) -> (Paths, PathBuf) {
    let dir = std::env::temp_dir().join(format!("h0x-vps-{}", crate::platform::random_hex()));
    std::fs::create_dir_all(&dir).unwrap();
    if !machines.is_empty() {
        std::fs::write(dir.join("machines.json"), machines).unwrap();
    }
    let paths = Paths {
        socket: dir.join("s.sock"),
        holder_socket: dir.join("h.sock"),
        state: dir.clone(),
        config: dir.join("config.toml"),
    };
    (paths, dir)
}

fn machine(name: &str, target: &str, via: bool) -> String {
    format!(r#"{{"name":"{name}","target":"{target}","via":{via},"session":null,"id":"x","host":"h","version":"0"}}"#)
}

#[test]
fn plan_needs_an_existing_plain_ssh_machine() {
    let json = format!(
        "[{},{},{}]",
        machine("vps", "me@vps.example.com", false),
        machine("fly", "fly ssh", true),
        machine("bad", "-oX=1", false)
    );
    let (paths, dir) = scratch_paths(&json);
    let ok = plan(&paths, "vps", "H0X.example.com", 8377).unwrap();
    assert_eq!(
        ok,
        Vps { target: "me@vps.example.com".into(), public_host: "h0x.example.com".into(), remote_port: 8377 }
    );
    let unknown = plan(&paths, "nope", "h0x.example.com", 8377).unwrap_err();
    assert_eq!(unknown.kind, "machine_unknown");
    assert!(unknown.hint.contains("vps"), "lists the machines: {}", unknown.hint);
    assert!(plan(&paths, "fly", "h0x.example.com", 8377).is_err(), "custom command machines");
    assert!(plan(&paths, "bad", "h0x.example.com", 8377).is_err(), "option-like target");
    assert!(plan(&paths, "vps", "1.2.3.4", 8377).is_err());
    assert!(plan(&paths, "vps", "h0x.example.com", 80).is_err());
    std::fs::remove_dir_all(dir).unwrap();

    let (paths, dir) = scratch_paths("");
    assert_eq!(plan(&paths, "vps", "h0x.example.com", 8377).unwrap_err().kind, "machine_unknown", "no machines at all");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn backoff_is_pure_and_resets_after_a_stable_run() {
    let quick = Duration::from_secs(1);
    let mut failures = 0;
    let waits: Vec<u64> = (0..9)
        .map(|_| {
            failures = failures_after(failures, quick);
            crate::bridge::telegram::backoff(failures).as_secs()
        })
        .collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(failures_after(7, STABLE), 1, "a long run starts over");
    assert_eq!(failures_after(7, STABLE - Duration::from_millis(1)), 8);
    assert_eq!(failures_after(u32::MAX, quick), u32::MAX, "no overflow");
}

#[test]
fn allow_list_gets_the_public_host_only_when_asked() {
    let base = vec!["127.0.0.1:8377".to_string(), "localhost:8377".to_string()];
    assert_eq!(
        with_public(base.clone(), Some("h0x.example.com")),
        ["127.0.0.1:8377", "localhost:8377", "h0x.example.com"]
    );
    assert_eq!(with_public(base.clone(), None), base, "without --via it is not there");
}

fn request(origin: &str) -> Request {
    Request {
        method: "GET".into(),
        path: "/ws".into(),
        headers: vec![("host".into(), "h0x.example.com".into()), ("origin".into(), origin.into())],
    }
}

#[test]
fn only_our_public_hosts_https_origin_is_accepted() {
    let hosts = with_public(vec!["127.0.0.1:8377".into()], Some("h0x.example.com"));
    let check = |origin: &str| {
        let mut req = request(origin);
        https_origin_as_http(&mut req, &hosts);
        super::super::http::allowed(&req, &hosts)
    };
    assert_eq!(check("https://h0x.example.com"), Ok(()));
    assert_eq!(check("https://H0X.example.com"), Ok(()));
    assert_eq!(check("http://h0x.example.com"), Ok(()), "unchanged behaviour");
    assert_eq!(check("https://evil.example.com"), Err(403));
    assert_eq!(check("https://h0x.example.com.evil.com"), Err(403));
    assert_eq!(check("https://h0x.example.com:8443"), Err(403));
    assert_eq!(check("https://127.0.0.1:8377"), Err(403), "a host with a port stays http-only");
    assert_eq!(check("null"), Err(403));
    let mut req = request("https://h0x.example.com");
    req.headers.retain(|(n, _)| n != "origin");
    assert_eq!(super::super::http::allowed(&req, &hosts), Ok(()), "no origin: as before");
}

/// The stand-in for ssh: this test binary, asleep.
const FAKE: &str = "H0X_VPS_FAKE_SSH";

#[test]
fn fake_ssh_sleeps() {
    if std::env::var_os(FAKE).is_some() {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// A command that is the fake ssh and notes each start in `log`.
fn fake(log: PathBuf) -> impl Fn() -> Command + Send + 'static {
    move || {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        std::io::Write::write_all(&mut file, b"x").unwrap();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "bridge::web::vps::tests::fake_ssh_sleeps", "--nocapture", "--test-threads", "1"]);
        cmd.env(FAKE, "1");
        cmd
    }
}

fn starts(log: &std::path::Path) -> usize {
    std::fs::read(log).map_or(0, |b| b.len())
}

fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn lifecycle_starts_restarts_pauses_on_lock_and_stops() {
    let dir = std::env::temp_dir().join(format!("h0x-vps-life-{}", crate::platform::random_hex()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("starts");
    let locked = Arc::new(AtomicBool::new(false));
    let flag = locked.clone();
    let tunnel = Tunnel::start(fake(log.clone()), move || flag.load(Ordering::SeqCst), Duration::from_millis(20));
    wait_for("first start", || starts(&log) == 1);

    // Lock: ssh is killed and nothing restarts it.
    locked.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(starts(&log), 1, "no restart while locked");
    // Unlock: it comes back.
    locked.store(false, Ordering::SeqCst);
    wait_for("start after unlock", || starts(&log) == 2);

    // Stop returns (the sleeping child is killed, not waited out) and ends it all.
    let began = Instant::now();
    tunnel.stop();
    assert!(began.elapsed() < Duration::from_secs(10), "stop kills ssh");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(starts(&log), 2, "nothing starts after stop");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_ssh_that_ends_is_restarted_with_backoff_and_a_missing_one_is_survived() {
    let dir = std::env::temp_dir().join(format!("h0x-vps-dead-{}", crate::platform::random_hex()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("starts");
    let (counted, path) = (log.clone(), log);
    let make = move || {
        std::io::Write::write_all(
            &mut std::fs::OpenOptions::new().create(true).append(true).open(&counted).unwrap(),
            b"x",
        )
        .unwrap();
        // Exits at once, every time.
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "bridge::web::vps::tests::fake_ssh_sleeps", "--test-threads", "1"]);
        cmd
    };
    let tunnel = Tunnel::start(make, || false, Duration::from_millis(10));
    wait_for("a restart after the first exit", || starts(&path) >= 2);
    let (before, started) = (starts(&path), Instant::now());
    std::thread::sleep(Duration::from_millis(300));
    assert!(starts(&path) - before <= 1, "backoff grows: not a restart per tick");
    assert!(started.elapsed() >= Duration::from_millis(300));
    drop(tunnel);

    let missing = Tunnel::start(|| Command::new(r"C:\no\such\ssh.exe"), || false, Duration::from_millis(10));
    std::thread::sleep(Duration::from_millis(100));
    missing.stop();
    std::fs::remove_dir_all(dir).unwrap();
}
