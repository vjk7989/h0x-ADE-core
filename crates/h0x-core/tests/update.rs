#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

#[test]
fn an_up_to_date_binary_restarts_only_an_older_running_core() {
    let mut s = Session::new();
    let version = env!("CARGO_PKG_VERSION");
    std::fs::create_dir_all(s.root.join("latest/download")).unwrap();
    std::fs::write(
        s.root.join("latest/download/latest.json"),
        json!({"version": version, "generation": h0x_core::api::GENERATION, "targets": {}}).to_string(),
    )
    .unwrap();
    s.env.push(("H0X_CORE_RELEASES_URL".into(), format!("file://{}", s.root.display())));
    let args = ["update", "--version", version];
    assert_eq!(s.json(&args)["updated"], false);
    assert!(!s.connect_ok(), "checking an absent core must not start one");
    std::fs::create_dir_all(s.socket().parent().unwrap()).unwrap();
    for (running, latest) in
        [(version, false), ("999.0.0", false), ("unknown", false), ("0.0.0", false), ("0.0.0", true)]
    {
        let listener = UnixListener::bind(s.socket()).unwrap();
        let socket = s.socket();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], "server.status");
            writeln!(stream, "{}", json!({"id": 1, "result": {"version": running}})).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            if running == "0.0.0" {
                let req: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(req["method"], "server.restart");
                writeln!(stream, "{}", json!({"id": 1, "result": {}})).unwrap();
            } else {
                assert!(line.is_empty(), "must leave {running} running");
            }
            drop(listener);
            std::fs::remove_file(socket).unwrap();
        });
        let out = s.json(if latest { &["update"] } else { &args });
        server.join().unwrap();
        assert_eq!(out["updated"], false);
        if running == "0.0.0" {
            assert_eq!(out["restarted"], true);
            assert_eq!(s.json(&["server", "status"])["version"], version);
            s.json(&["server", "stop"]);
        } else {
            assert_ne!(out["restarted"], true);
        }
    }
}

#[test]
fn an_install_in_a_read_only_folder_explains_how_to_update_it() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // Root can replace files in read-only directories.
    }
    let mut s = Session::new();
    let dir = s.root.join("bin");
    std::fs::create_dir(&dir).unwrap();
    s.bin = dir.join("h0x-core");
    std::fs::copy(BIN, &s.bin).unwrap();
    s.json(&["server", "start"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let status = s.cmd().args(["server", "status"]).output().unwrap();
    let update = s.cmd().args(["update", "--from", BIN]).output().unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["installed_by"], "system package manager");
    let error: Value = serde_json::from_slice(&update.stderr).unwrap();
    assert!(!update.status.success());
    assert_eq!(error["error"]["kind"], "refused", "{error}");
    assert_eq!(error["error"]["message"], format!("{} isn't writable by you", dir.canonicalize().unwrap().display()));
    assert_eq!(
        error["error"]["hint"],
        "reinstall h0x-core from the GitHub release: https://github.com/vjk7989/h0x-ADE-core/releases"
    );
}

#[test]
fn update_from_a_file_swaps_hc_and_keeps_everything_running() {
    let mut s = Session::new();
    s.env.push(("H0X_CORE_SESSION_VALUE".into(), "original".into()));
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("h0x-core")).unwrap();
    std::os::unix::fs::symlink("h0x-core", dir.join("h0x-core-link")).unwrap();
    s.bin = dir.join("h0x-core-link"); // run through a link, the way people do
    let t = run(&s, &["sh", "-c", "echo before; while read l; do echo got-$l; done"]);
    let pid = s.json(&["server", "status"])["pid"].clone();

    let out = s
        .cmd()
        .env("NO_COLOR", "1")
        .env("H0X_CORE_SESSION_VALUE", "updater")
        .env("H0X_CORE_UPDATER_ONLY", "leaked")
        .args(["update", "--from", BIN])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    let real = std::fs::canonicalize(dir.join("h0x-core")).unwrap(); // macOS: /var is /private/var
    assert_eq!(r["path"], real.display().to_string(), "the file h0x-core runs from, not the link: {r}");
    assert!(
        r["was"].as_str().unwrap().starts_with("h0x-core ") && r["now"].as_str().unwrap().starts_with("h0x-core "),
        "{r}"
    );
    assert_eq!(r["restarted"], true);
    assert_eq!(r["rolled_back"], false);
    assert!(dir.join("h0x-core.previous").is_file(), "the replaced binary is kept");
    let good: Value = serde_json::from_slice(&std::fs::read(dir.join("last-good.json")).unwrap()).unwrap();
    assert_eq!(good["version"], env!("CARGO_PKG_VERSION"));
    assert_ne!(s.json(&["server", "status"])["pid"], pid, "a new core runs");
    assert!(dir.join("h0x-core-link").is_symlink(), "the link stays a link");
    s.json(&["send", &t, "after"]);
    wait_until("the terminal still answers", || text(&s, &t).contains("got-after"));
    let probe =
        "printf 'ENV:%s|%s|%s\\n' \"${NO_COLOR-unset}\" \"$H0X_CORE_SESSION_VALUE\" \"${H0X_CORE_UPDATER_ONLY-unset}\"";
    let out = s.core(&["run", "sh", "-c", probe]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("ENV:unset|original|unset"), "{out:?}");
    let stream = h0x_core::platform::connect(&s.socket()).unwrap();
    let terminal = h0x_core::client::call(
        &stream,
        "terminal.run",
        json!({
            "argv": ["sh", "-c", probe],
            "env": {"NO_COLOR": "1", "H0X_CORE_SESSION_VALUE": "override"},
        }),
    )
    .unwrap();
    wait_until("per-terminal environment override", || {
        text(&s, terminal["address"].as_str().unwrap()).contains("ENV:1|override|unset")
    });

    let older = s.root.join("older");
    std::fs::write(&older, "#!/bin/sh\necho 'h0x-core 0.0.1'\n").unwrap();
    std::fs::set_permissions(&older, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (_, e) = s.err(&["--json", "update", "--from", older.to_str().unwrap()]);
    assert_eq!(e["error"]["kind"], "refused", "never older by accident: {e}");
    let junk = s.root.join("junk");
    std::fs::write(&junk, "not a program").unwrap();
    assert_eq!(s.err(&["--json", "update", "--from", junk.to_str().unwrap()]).1["error"]["kind"], "refused");
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".new")),
        "no leftovers"
    );
}

#[test]
fn a_package_manager_install_is_sent_to_the_github_release_except_nix() {
    let github = "reinstall h0x-core from the GitHub release: https://github.com/vjk7989/h0x-ADE-core/releases";
    for (folder, manager, hint) in [
        ("Cellar/h0x-core/0.2.0/bin", "homebrew", github),
        ("mise/installs/h0x-core/0.2.0", "mise", github),
        ("nix/store/abc-h0x-core/bin", "nix", "update it in your Nix configuration"),
    ] {
        let mut s = Session::new();
        let dir = s.root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        s.bin = dir.join("h0x-core");
        std::fs::copy(BIN, &s.bin).unwrap();
        let update = s.cmd().args(["update", "--from", BIN]).output().unwrap();
        assert!(!update.status.success(), "{manager}");
        let error: Value = serde_json::from_slice(&update.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "refused", "{error}");
        assert_eq!(
            error["error"]["message"],
            format!("{} belongs to {manager}", s.bin.canonicalize().unwrap().display()),
            "{error}"
        );
        assert_eq!(error["error"]["hint"], hint, "{error}");
    }
}

#[test]
fn a_new_core_that_does_not_come_up_is_rolled_back_with_its_terminals() {
    let mut s = Session::new();
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("h0x-core")).unwrap();
    s.bin = dir.join("h0x-core");
    let t = run(&s, &["sh", "-c", "echo before; while read l; do echo got-$l; done"]);
    s.json(&["server", "status"]);
    // Says it is a newer h0x-core, but cannot be one.
    let broken = s.root.join("broken");
    std::fs::write(&broken, "#!/bin/sh\necho 'h0x-core 99.0.0'\n").unwrap();
    std::fs::set_permissions(&broken, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (code, e) = s.err(&["--json", "update", "--from", broken.to_str().unwrap()]);
    assert_ne!(code, 0);
    assert_eq!(e["rolled_back"], true, "{e}");
    assert!(e["error"].as_str().unwrap().contains("went back to h0x-core"), "{e}");
    let version = std::process::Command::new(&s.bin).arg("--version").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), format!("h0x-core {}", env!("CARGO_PKG_VERSION")));
    assert!(!dir.join("last-good.json").exists(), "only a healthy update is last good");
    assert_eq!(s.json(&["server", "status"])["version"], env!("CARGO_PKG_VERSION"));
    s.json(&["send", &t, "after"]);
    wait_until("the terminal still answers", || text(&s, &t).contains("got-after"));
}
