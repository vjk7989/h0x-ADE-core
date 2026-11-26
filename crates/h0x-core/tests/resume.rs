//! RES-C3 end to end: a kept terminal whose holder is lost gets its agent's resume line
//! typed into the replacement shell, or a plain shell when anything is doubtful.
//! (Windows records only the first program: ADR-124 fifth addendum (3); unit-tested in kept.rs.)
#![cfg(unix)]
mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;

const ID: &str = "123e4567-e89b-42d3-a456-426614174000";

fn executable_at(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Types `launch` into a kept shell with a fake `claude` on PATH, loses the holder, and returns
/// each run of the fake (its arguments) once the replacement shell has had time to take a line.
/// `label` is set on the terminal before the loss; `shell` replaces the configured shell.
fn lose(launch: &str, label: Option<&str>, shell: Option<&str>) -> Vec<String> {
    let mut s = Session::new();
    let bin = s.root.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = s.root.join("claude.log");
    let quit = s.root.join("quit");
    executable_at(
        &bin.join("claude"),
        &format!(
            "#!/bin/sh\necho \"$@\" >> {}\nwhile [ ! -e {} ]; do sleep 0.05; done\n",
            log.display(),
            quit.display()
        ),
    );
    s.env.push(("PATH".into(), format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())));
    s.env.push(("SHELL".into(), "/bin/sh".into()));
    s.env.push(("H0X_CORE_FOREGROUND_MS".into(), "30".into()));
    if let Some(shell) = shell {
        let path = s.root.join(shell);
        executable_at(&path, "#!/bin/sh\nexec /bin/sh\n");
        let config = s.root.join("config/h0x-core/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, format!("shell = [\"{}\"]", path.display())).unwrap();
    }
    let runs = || -> Vec<String> {
        std::fs::read_to_string(&log).map(|l| l.lines().map(str::to_string).collect()).unwrap_or_default()
    };
    let t = run(&s, &["--keep"]);
    if let Some(label) = label {
        s.json(&["label", &t, label]);
    }
    s.json(&["send", &t, launch]);
    wait_until("claude runs and is recorded", || {
        let raw = std::fs::read_to_string(s.root.join("state/h0x-core/test/terminals.json")).unwrap_or_default();
        let state: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        runs().len() == 1 && state["terminals"]["1"]["agent"]["agent"] == "claude"
    });
    let old = get(&s, &t)["pid"].as_i64().unwrap();
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    unsafe {
        libc::kill(holder as i32, libc::SIGKILL);
    }
    wait_until("replacement shell", || {
        let r = get(&s, &t);
        r["state"] == "live" && r["pid"].as_i64().is_some_and(|p| p != old)
    });
    // The line is typed once the new shell's prompt has been quiet for 0.8 s.
    std::thread::sleep(std::time::Duration::from_millis(3000));
    std::fs::write(&quit, "").unwrap();
    runs()
}

#[test]
fn a_lost_claude_resumes_its_session_with_its_kept_flags_and_no_prompt() {
    let runs = lose(&format!("claude --session-id {ID} --model x 'secret prompt'"), None, None);
    assert_eq!(runs.len(), 2, "{runs:?}");
    // The shim adds exactly its `--settings` (a vouched terminal, no settings of the person's), then only the resume words.
    let settings = h0x_core::shim::claude_settings(true);
    assert_eq!(runs[1], format!("--settings {settings} --resume {ID}"), "{runs:?}");
}

#[test]
fn resume_off_leaves_a_plain_shell() {
    let runs = lose(&format!("claude --session-id {ID} --model x"), Some("h0x.resume=off"), None);
    assert_eq!(runs.len(), 1, "{runs:?}");
}

#[test]
fn the_nil_uuid_and_an_evil_session_label_leave_a_plain_shell() {
    let nil = "00000000-0000-0000-0000-000000000000";
    assert_eq!(lose(&format!("claude --session-id {nil}"), None, None).len(), 1);
    assert_eq!(lose("claude", Some("h0x.session=claude:--evil"), None).len(), 1);
}

#[test]
fn an_unknown_shell_gets_no_typed_line() {
    assert_eq!(lose(&format!("claude --session-id {ID}"), None, Some("xonsh")).len(), 1);
}
