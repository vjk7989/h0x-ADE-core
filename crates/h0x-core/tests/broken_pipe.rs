//! Output to a closed pipe ends quietly instead of panicking.
use std::process::{Command, Stdio};

#[test]
fn a_closed_stdout_exits_141_without_a_panic() {
    // The read end is closed before the child exists, so its first write cannot succeed.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_h0x-core"))
        .arg("--skill")
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(141), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}

#[test]
fn an_open_stdout_still_exits_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_h0x-core")).arg("--skill").output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(!output.stdout.is_empty());
}

#[test]
fn a_closed_stderr_exits_141_too_with_or_without_json() {
    for args in [&["--no-such-flag"][..], &["--json", "--no-such-flag"]] {
        // A usage error goes to stderr; stdout is not a terminal, so it is the JSON error.
        let mut child = Command::new(env!("CARGO_BIN_EXE_h0x-core"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stderr.take());
        assert_eq!(child.wait().unwrap().code(), Some(141), "{args:?}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_full_disk_is_not_a_closed_pipe_and_still_panics() {
    let full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_h0x-core")).arg("--skill").stdout(full).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    // ENOSPC (os error 28): the default panic, exit 101, with its message.
    assert_eq!(output.status.code(), Some(101), "stderr: {stderr}");
    assert!(stderr.contains("failed printing to stdout") && stderr.contains("(os error 28)"), "stderr: {stderr}");
}

#[cfg(unix)]
mod common;

#[cfg(unix)]
#[test]
fn json_from_the_core_to_a_closed_stdout_exits_141() {
    let s = common::Session::new();
    s.json(&["server", "start"]);
    let mut child =
        s.cmd().args(["--json", "server", "status"]).stdin(Stdio::null()).stdout(Stdio::piped()).spawn().unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(141), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}

#[cfg(unix)]
#[test]
fn the_server_keeps_running_on_a_closed_stderr() {
    // A core that cannot log must not take its terminals down with it.
    let s = common::Session::new();
    let state = s.root.join("state/h0x-core/test");
    std::fs::create_dir_all(&state).unwrap();
    // Corrupt documents make `serve` write a line to stderr on its main thread at startup.
    std::fs::write(state.join("docs.json"), b"{broken").unwrap();
    let mut core = s
        .cmd()
        .env("SSH_CONNECTION", "test-direct")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(core.stderr.take());
    common::wait_until("the core answers", || s.connect_ok());
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(core.try_wait().unwrap().is_none(), "serve exited after its stderr write failed");
    assert!(s.json(&["server", "status"])["pid"].as_u64().is_some());
    let t = common::run(&s, &["sleep", "600"]);
    assert!(common::get(&s, &t)["pid"].is_number());
    s.json(&["server", "stop"]);
    let start = std::time::Instant::now();
    while core.try_wait().unwrap().is_none() {
        if start.elapsed() > std::time::Duration::from_secs(10) {
            let _ = core.kill();
            panic!("serve did not stop cleanly");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
