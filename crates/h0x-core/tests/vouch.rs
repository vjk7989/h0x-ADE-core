//! The process-tree half of `terminal.vouch` (`platform::descends_from`) and the parts of
//! `h0x-core hook session` that need no core: both run on Windows too. Tests that need a
//! real process chain or a stalled socket are Unix-only and marked so.

use h0x_core::platform::{descends_from, parent_pid};
use std::io::Write;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_h0x-core");
const ID: &str = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";
const MAX_INPUT: usize = 4 * 1024 * 1024;

/// A child that stays alive for a while and prints nothing.
fn sleeper() -> Child {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("ping");
        c.args(["-n", "30", "127.0.0.1"]);
        c
    };
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sleep");
        c.arg("30");
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

fn end(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn the_program_itself_counts_as_inside_its_own_tree() {
    let me = std::process::id();
    assert!(descends_from(me, me));
}

#[test]
fn a_child_descends_from_us_and_we_from_our_parent_never_the_other_way() {
    let me = std::process::id();
    let (a, b) = (sleeper(), sleeper());
    assert!(descends_from(a.id(), me), "our child");
    assert!(descends_from(me, parent_pid()), "us, below our parent");
    assert!(descends_from(a.id(), parent_pid()), "our child, two levels below our parent");
    assert!(!descends_from(me, a.id()), "a parent is not below its child");
    assert!(!descends_from(a.id(), b.id()), "siblings");
    assert!(!descends_from(b.id(), a.id()), "siblings");
    end(a);
    end(b);
}

#[test]
fn a_process_that_is_gone_is_inside_nothing_and_nothing_is_inside_it() {
    let me = std::process::id();
    let child = sleeper();
    let gone = child.id();
    end(child);
    assert!(!descends_from(gone, me), "an exited child has no parent to walk");
    assert!(!descends_from(me, gone));
    assert!(!descends_from(u32::MAX, me), "a pid that never existed");
    assert!(!descends_from(u32::MAX - 3, me), "a Windows-shaped (multiple of 4) pid that never existed");
}

#[test]
fn pid_0_and_1_are_never_a_way_in() {
    let me = std::process::id();
    assert!(!descends_from(0, me), "no peer pid");
    assert!(!descends_from(me, 0), "no program pid: the walk stops before 0");
    assert!(!descends_from(1, me));
    // Unix: the walk stops below init, so even a real descendant of 1 is not "below" it.
    // Windows: there is no pid 1.
    assert!(!descends_from(me, 1));
    // The code's choice, worth knowing: equal pids match before any lookup, so even 0 is
    // "inside" 0. The callers never pass 0 (the pipe peer filters it, `vouched_pid` needs a
    // program pid), but descends_from alone does not refuse it.
    assert!(descends_from(0, 0));
    assert!(descends_from(1, 1));
}

/// `h0x-core hook session <args>` with `input` on stdin, run outside any terminal: no
/// terminal variables unless `env` adds them, and an isolated core session name.
fn hook(args: &[&str], env: &[(&str, &str)], input: &[u8]) -> (Output, Duration) {
    let mut cmd = Command::new(BIN);
    cmd.args(["hook", "session"]).args(args);
    for var in ["H0X_TERMINAL_ID", "H0X_SCOPE", "H0X_TERMINAL_SOCKET", "HC_SESSION"] {
        cmd.env_remove(var);
    }
    let root = std::env::temp_dir().join(format!("h0x-core-vouch-{}", std::process::id()));
    cmd.env("H0X_CORE_SESSION", format!("vouch-test-{}", std::process::id()))
        .env("H0X_ADE_HOME", &root)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    // The hook stops reading past its cap: a broken pipe here is expected.
    let feed = std::thread::spawn(move || drop(stdin.write_all(&input)));
    let out = child.wait_with_output().unwrap();
    let took = start.elapsed();
    feed.join().unwrap();
    (out, took)
}

fn quiet_zero(what: &str, out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{what}: {out:?}");
    assert!(out.stdout.is_empty(), "{what}: stdout {:?}", String::from_utf8_lossy(&out.stdout));
    assert!(out.stderr.is_empty(), "{what}: stderr {:?}", String::from_utf8_lossy(&out.stderr));
}

fn event(id: &str) -> String {
    format!(r#"{{"session_id":"{id}","hook_event_name":"SessionStart","source":"startup"}}"#)
}

/// A valid event padded with trailing whitespace (still valid JSON) to exactly `len` bytes.
fn padded(len: usize) -> Vec<u8> {
    let mut e = event(ID).into_bytes();
    e.resize(len, b' ');
    e
}

#[test]
fn any_input_exits_0_and_prints_nothing() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty stdin", Vec::new()),
        ("invalid UTF-8", vec![0xff, 0xfe, b'{', 0xc3, 0x28, b'}']),
        ("a JSON array", format!("[{}]", event(ID)).into_bytes()),
        ("upper-case id", event(&ID.to_uppercase()).into_bytes()),
        ("nil id", event("00000000-0000-0000-0000-000000000000").into_bytes()),
        ("not json", b"not json".to_vec()),
        ("exactly 4 MB", padded(MAX_INPUT)),
        ("4 MB + 1", padded(MAX_INPUT + 1)),
        ("8 MB", padded(2 * MAX_INPUT)),
    ];
    for (what, input) in cases {
        let (out, _) = hook(&[], &[], &input);
        quiet_zero(what, &out);
    }
}

#[test]
fn a_bad_agent_exits_0_and_prints_nothing() {
    for args in [
        &["--agent", "Claude"][..],
        &["--agent", "a b"],
        &["--agent", ""],
        &["--agent"],
        &["--agent=codex"],
        &["--agent", "codex", "extra"],
        &["--agent", "cl\u{e9}"],
    ] {
        let (out, _) = hook(args, &[], event(ID).as_bytes());
        quiet_zero(&format!("{args:?}"), &out);
    }
}

#[test]
fn missing_or_dead_terminal_variables_exit_0_quickly() {
    #[cfg(windows)]
    let dead = format!(r"\\.\pipe\h0x-core-vouch-dead-{}", std::process::id());
    #[cfg(unix)]
    let dead =
        std::env::temp_dir().join(format!("h0x-core-vouch-dead-{}.sock", std::process::id())).display().to_string();
    let dead = dead.as_str();
    let cases: [(&str, Vec<(&str, &str)>); 5] = [
        ("no variables", vec![]),
        ("no H0X_TERMINAL_SOCKET", vec![("H0X_TERMINAL_ID", "t1"), ("H0X_SCOPE", "1-abc")]),
        ("no H0X_SCOPE", vec![("H0X_TERMINAL_ID", "t1"), ("H0X_TERMINAL_SOCKET", dead)]),
        ("empty H0X_SCOPE", vec![("H0X_TERMINAL_ID", "t1"), ("H0X_SCOPE", ""), ("H0X_TERMINAL_SOCKET", dead)]),
        ("a dead socket", vec![("H0X_TERMINAL_ID", "t1"), ("H0X_SCOPE", "1-abc"), ("H0X_TERMINAL_SOCKET", dead)]),
    ];
    for (what, env) in cases {
        let (out, took) = hook(&[], &env, event(ID).as_bytes());
        quiet_zero(what, &out);
        assert!(took < Duration::from_secs(3), "{what}: took {took:?}");
    }
}

/// A server that accepts the connection and never answers: the hook gives up after
/// `vouched()`'s 1 s cap instead of hanging the agent. Unix only (std has no pipe server).
#[cfg(unix)]
#[test]
fn a_stalled_server_costs_the_hook_about_300_ms() {
    let dir = std::env::temp_dir().join(format!("h0x-core-vouch-stall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("stall.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    // Accept and hold every connection without reading or replying.
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    let socket = path.display().to_string();
    let env = [("H0X_TERMINAL_ID", "t1"), ("H0X_SCOPE", "1-abc"), ("H0X_TERMINAL_SOCKET", socket.as_str())];
    let (out, took) = hook(&[], &env, event(ID).as_bytes());
    quiet_zero("a stalled server", &out);
    assert!(took >= Duration::from_millis(250), "it did ask the server: {took:?}");
    if std::env::var_os("H0X_CORE_TEST_SLOW_RUNNER").is_none() {
        assert!(took < Duration::from_secs(2), "capped near 1 s: {took:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A real chain of `depth` shells; returns the top one (in its own process group) and the
/// pid of the shell at the bottom.
#[cfg(unix)]
fn chain(dir: &std::path::Path, depth: u32) -> (Child, u32) {
    use std::os::unix::process::CommandExt;
    let script = dir.join("chain.sh");
    std::fs::write(
        &script,
        "if [ \"$1\" -gt 0 ]; then sh \"$0\" $(($1 - 1)) \"$2\" & wait; \
         else echo $$ > \"$2.tmp\"; mv \"$2.tmp\" \"$2\"; exec sleep 30; fi\n",
    )
    .unwrap();
    let out = dir.join(format!("bottom-{depth}"));
    let top = Command::new("sh")
        .arg(&script)
        .arg(depth.to_string())
        .arg(&out)
        .process_group(0)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let bottom = loop {
        if let Some(pid) = std::fs::read_to_string(&out).ok().and_then(|s| s.trim().parse().ok()) {
            break pid;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "chain of {depth} never finished");
        std::thread::sleep(Duration::from_millis(20));
    };
    (top, bottom)
}

/// How many parents up `root` is from `pid`, by `ps` (independent of the code under test).
#[cfg(unix)]
fn levels(mut pid: u32, root: u32) -> u32 {
    let mut n = 0;
    while pid != root {
        let out = Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().unwrap();
        pid = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        n += 1;
        assert!(pid > 1 && n < 200, "lost the chain");
    }
    n
}

#[cfg(unix)]
fn end_chain(mut top: Child) {
    unsafe { libc::killpg(top.id() as libc::pid_t, libc::SIGKILL) };
    let _ = top.wait();
}

/// "Up to 64 levels": a caller 64 parents below the program is inside, 65 is not.
#[cfg(unix)]
#[test]
fn the_walk_reaches_64_levels_and_no_further() {
    let dir = std::env::temp_dir().join(format!("h0x-core-vouch-chain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (depth, inside) in [(63, true), (64, true), (65, false)] {
        let (top, bottom) = chain(&dir, depth);
        assert_eq!(levels(bottom, top.id()), depth, "the chain really is {depth} deep");
        let got = descends_from(bottom, top.id());
        end_chain(top);
        assert_eq!(got, inside, "{depth} levels below the program");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
