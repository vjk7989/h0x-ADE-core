//! Output to a closed pipe ends quietly instead of panicking.
use std::process::{Command, Stdio};

#[test]
fn a_closed_stdout_exits_141_without_a_panic() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_h0x-core"))
        .arg("--skill")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close our end before the child starts printing.
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
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
