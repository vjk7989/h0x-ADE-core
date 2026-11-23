#![cfg(target_os = "linux")]

mod common;
use common::*;

#[test]
fn a_login_profile_running_h0x_core_captures_only_once() {
    use std::os::unix::fs::PermissionsExt;
    let mut s = Session::new();
    let shell = s.root.join("login-shell");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\necho capture >> \"$HOME/captures\"\n\
             if [ \"$(wc -l < \"$HOME/captures\")\" -lt 4 ]; then '{BIN}' ls >/dev/null; fi\n\
             [ \"$1\" != -l ] || shift\nexec /bin/sh \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.env.extend([("SSH_CONNECTION".into(), "test".into()), ("SHELL".into(), shell.to_str().unwrap().into())]);
    s.json(&["ls"]);
    assert_eq!(std::fs::read_to_string(s.root.join("captures")).unwrap(), "capture\n");
}

#[test]
fn failed_login_profile_cleans_up_its_background_child() {
    let s = ssh_session("sleep 60 &\necho $! > \"$HOME/profile-child\"\nexit 17\n");
    assert_eq!(value(&s), "");
    let pid = std::fs::read_to_string(s.root.join("profile-child")).unwrap();
    let pid: i32 = pid.trim().parse().unwrap();
    for _ in 0..100 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        if stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    panic!("profile child survived fallback");
}

#[test]
fn login_profile_output_is_bounded_while_it_runs() {
    for redirect in ["", " >&2"] {
        let s = ssh_session(&format!("head -c 4194304 /dev/zero{redirect}\ntouch \"$HOME/overrun\"\n"));
        assert_eq!(value(&s), "");
        assert!(!s.root.join("overrun").exists(), "writer ran beyond the capture budget");
        let log = std::fs::read_to_string(s.root.join("state/h0x-core/test/core.log")).unwrap();
        assert!(log.contains("exceeded 1 MiB"), "missing output-limit diagnostic");
        assert!(log.len() < (1 << 20) + 1024);
    }
}

#[test]
fn login_profile_exit_chatter_preserves_exports() {
    let s = ssh_session("trap 'echo goodbye' EXIT\nexport LOGIN_TEST=loaded\n");
    assert_eq!(value(&s), "loaded");
}

#[test]
fn holder_environment_does_not_reload_login_profile() {
    let mut s = ssh_session("export LOGIN_TEST=\"x$LOGIN_TEST\"\n");
    s.env.push(("H0X_CORE_HOLDER_GRACE_MS".into(), "100".into()));
    assert_eq!(value(&s), "x");
    let terminal = run(&s, &["sleep", "60"]);
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until("holder starts a replacement core", || {
        let Ok(stream) = h0x_core::platform::connect(&s.socket()) else { return false };
        h0x_core::client::call(&stream, "server.status", serde_json::json!({}))
            .is_ok_and(|status| status["pid"].as_i64() != Some(pid as i64))
    });
    assert_eq!(value(&s), "x", "holder recovery must keep the already loaded environment");
    s.json(&["kill", &terminal]);
}

fn ssh_session(profile: &str) -> Session {
    let mut s = Session::new();
    s.env.extend([("SSH_CONNECTION".into(), "test".into()), ("SHELL".into(), "/bin/bash".into())]);
    std::fs::write(s.root.join(".bash_profile"), profile).unwrap();
    s
}

fn value(s: &Session) -> String {
    let out = s.core(&["exec", "/bin/sh", "-c", "printf '%s' \"$LOGIN_TEST\""]);
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn ssh_start_loads_login_exports_once_and_keeps_them_across_restart() {
    let s = ssh_session("echo profile chatter\nexport LOGIN_TEST='login value'\n");
    assert_eq!(value(&s), "login value");
    std::fs::write(s.root.join(".bash_profile"), "export LOGIN_TEST=changed\n").unwrap();
    assert_eq!(value(&s), "login value", "reconnecting must not reload profiles");
    s.json(&["server", "restart"]);
    assert_eq!(value(&s), "login value", "a restart keeps the settled environment");
}

#[test]
fn local_linux_start_keeps_its_inherited_environment() {
    let mut s = ssh_session("export LOGIN_TEST=profile\n");
    s.env.retain(|(key, _)| key != "SSH_CONNECTION");
    s.env.push(("LOGIN_TEST".into(), "desktop".into()));
    assert_eq!(value(&s), "desktop");
}

#[test]
fn failing_login_profile_falls_back_and_logs_the_shell_and_reason() {
    let mut s = ssh_session("echo profile failed\nexit 17\n");
    s.env.push(("LOGIN_TEST".into(), "inherited".into()));
    assert_eq!(value(&s), "inherited");
    let log = std::fs::read_to_string(s.root.join("state/h0x-core/test/core.log")).unwrap();
    assert!(log.contains("/bin/bash") && log.contains("17") && log.contains("using inherited environment"), "{log}");
}

#[test]
fn a_login_profile_cannot_block_machine_start_forever() {
    let mut s = ssh_session("sleep 30\n");
    s.env.push(("LOGIN_TEST".into(), "inherited".into()));
    let start = std::time::Instant::now();
    assert_eq!(value(&s), "inherited");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let log = std::fs::read_to_string(s.root.join("state/h0x-core/test/core.log")).unwrap();
    assert!(log.contains("/bin/bash") && log.contains("exceeded 5 s"), "{log}");
}

#[test]
fn a_profile_printing_garbage_still_loads_its_exports() {
    let s = ssh_session(
        "head -c 4096 /dev/urandom | tr -d '\000'\nprintf 'H0X_LOGIN_END\n=\n'\nexport LOGIN_TEST=loaded\n",
    );
    assert_eq!(value(&s), "loaded");
}

#[test]
fn a_profile_faking_the_capture_marker_falls_back_to_the_inherited_environment() {
    let mut s = ssh_session("printf '\000H0X_LOGIN_ENV\000not a setting'\nexport LOGIN_TEST=profile\n");
    s.env.push(("LOGIN_TEST".into(), "inherited".into()));
    assert_eq!(value(&s), "inherited");
    let log = std::fs::read_to_string(s.root.join("state/h0x-core/test/core.log")).unwrap();
    assert!(log.contains("did not produce a complete environment"), "{log}");
}

#[test]
fn an_inherited_capture_guard_skips_the_capture_and_is_not_passed_on() {
    let mut s = ssh_session("echo capture >> \"$HOME/captures\"\nexport LOGIN_TEST=profile\n");
    s.env.push(("H0X_CORE_LOGIN_CAPTURE".into(), "1".into()));
    s.env.push(("LOGIN_TEST".into(), "inherited".into()));
    assert_eq!(value(&s), "inherited", "already inside a capture: no second one");
    assert!(!s.root.join("captures").exists());
    let out = s.core(&["exec", "/bin/sh", "-c", "printf '%s' \"${H0X_CORE_LOGIN_CAPTURE-unset}\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "unset", "the guard does not leak into programs");
}
