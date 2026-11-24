#![cfg(target_os = "linux")]
//! The core does not keep the folder it was started in as its working directory
//! (on Windows that would block renaming it; Linux is where /proc lets us check).
mod common;

use common::*;
use std::process::Stdio;

#[test]
fn serve_leaves_the_folder_it_was_started_in() {
    let s = Session::new();
    let start = s.root.join("start-here");
    std::fs::create_dir_all(&start).unwrap();
    let mut core = s
        .cmd()
        .current_dir(&start)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the core", || s.connect_ok());
    let cwd = std::fs::read_link(format!("/proc/{}/cwd", core.id())).unwrap();
    let _ = core.kill();
    let _ = core.wait();
    assert_ne!(cwd, start.canonicalize().unwrap());
}

fn serve_in(s: &Session, start: &std::path::Path) -> std::process::Child {
    let core = s
        .cmd()
        .current_dir(start)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the core", || s.connect_ok());
    core
}

/// Runs `pwd` and prints the handover var via raw `machine.exec` (the CLI always sends its own
/// folder); returns what it wrote, or the error reply.
fn exec_here(s: &Session, cwd: Option<&str>) -> Result<String, serde_json::Value> {
    let out = s.root.join(format!("out-{}", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let line = format!("{{ pwd; echo \"${{H0X_CORE_LAUNCH_DIR-unset}}\"; }} > '{}'", out.display());
    let mut params = serde_json::json!({"argv": ["sh", "-c", line]});
    if let Some(cwd) = cwd {
        params["cwd"] = cwd.into();
    }
    let reply = s.connect().call(1, "machine.exec", params);
    if reply.get("error").is_some() {
        return Err(reply);
    }
    Ok(std::fs::read_to_string(&out).unwrap())
}

#[test]
fn terminals_without_a_folder_still_start_where_the_core_was_launched_even_after_a_restart() {
    let s = Session::new();
    let start = s.root.join("start here ü 文");
    std::fs::create_dir_all(start.join("sub dir")).unwrap();
    let start = start.canonicalize().unwrap();
    let mut core = serve_in(&s, &start);
    let want = format!("{}\nunset\n", start.display());
    assert_eq!(exec_here(&s, None).unwrap(), want, "no cwd: the launch folder, and no handover var");
    let sub = format!("{}/sub dir\nunset\n", start.display());
    assert_eq!(
        exec_here(&s, Some("sub dir")).unwrap(),
        sub,
        "a relative cwd joins the launch folder, not the state dir"
    );

    s.json(&["server", "restart"]);
    wait_until("the successor", || s.connect_ok());
    assert_eq!(exec_here(&s, None).unwrap(), want, "the successor keeps the original launch folder");
    assert_eq!(exec_here(&s, Some("sub dir")).unwrap(), sub);
    s.json(&["server", "stop"]);
    let _ = core.kill();
    let _ = core.wait();
}

#[test]
fn a_launch_folder_deleted_while_the_core_runs_falls_back_as_before() {
    // Before the change the core's cwd was the deleted folder: current_dir() failed and `folder` used "/".
    let s = Session::new();
    let start = s.root.join("gone soon");
    std::fs::create_dir_all(&start).unwrap();
    let mut core = serve_in(&s, &start);
    std::fs::remove_dir_all(&start).unwrap();
    assert_eq!(exec_here(&s, None).expect("no cwd still starts"), "/\nunset\n");
    s.json(&["server", "stop"]);
    let _ = core.kill();
    let _ = core.wait();
}

#[test]
fn an_empty_handover_value_means_the_folder_the_core_starts_in() {
    let mut s = Session::new();
    s.env.push(("H0X_CORE_LAUNCH_DIR".into(), String::new()));
    let start = s.root.join("plain start");
    std::fs::create_dir_all(&start).unwrap();
    let start = start.canonicalize().unwrap();
    let mut core = serve_in(&s, &start);
    assert_eq!(exec_here(&s, None).unwrap(), format!("{}\nunset\n", start.display()));
    s.json(&["server", "stop"]);
    let _ = core.kill();
    let _ = core.wait();
}
