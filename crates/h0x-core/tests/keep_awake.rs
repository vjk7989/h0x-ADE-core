#![cfg(target_os = "linux")]
//! `keep_awake` on Linux: the helper the core starts, with a stand-in `systemd-inhibit`
//! (a CI runner may not let a user block sleep) or with none on PATH.
mod common;

use common::*;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

fn config(s: &Session, toml: &str) {
    let path = s.root.join("config/h0x-core/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, toml).unwrap();
}

/// `serve` started directly with this PATH, its stderr in `core.err`.
fn serve(s: &Session, path: &str) -> Child {
    serve_with(s, path, path)
}

/// `serve` with its own helper dirs, started in the session's root.
fn serve_with(s: &Session, path: &str, helper_dirs: &str) -> Child {
    let err = std::fs::File::create(s.root.join("core.err")).unwrap();
    let core = s
        .cmd()
        .current_dir(&s.root)
        .env("PATH", path)
        .env("H0X_CORE_HELPER_DIRS", helper_dirs)
        .env("SSH_CONNECTION", "test-direct")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .spawn()
        .unwrap();
    wait_until("the directly started core", || s.connect_ok());
    core
}

/// A `systemd-inhibit` that logs its arguments, then becomes the command it was given.
fn fake_inhibit(s: &Session) -> String {
    let bin = s.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = s.root.join("inhibit.log");
    let exe = bin.join("systemd-inhibit");
    std::fs::write(&exe, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nshift 4\nexec \"$@\"\n", log.display()))
        .unwrap();
    std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
}

fn inhibit_calls(s: &Session) -> Vec<String> {
    std::fs::read_to_string(s.root.join("inhibit.log")).unwrap_or_default().lines().map(String::from).collect()
}

/// The `tail` the core runs: by absolute path, from the first of these that has it.
fn tail() -> String {
    ["/usr/bin/tail", "/bin/tail"].into_iter().find(|p| Path::new(p).is_file()).unwrap().to_string()
}

/// Processes running `<tail> --pid <core> -f /dev/null`: the helper holding the block.
fn helpers(core: u64) -> usize {
    let want = format!("{}\0--pid\0{core}\0-f\0/dev/null\0", tail());
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == want.as_bytes()))
        .count()
}

fn held(s: &Session) -> bool {
    s.json(&["server", "status"])["keep_awake"] == true
}

/// A program that reports `working` until the file `go` exists, then `done`.
fn agent(s: &Session, go: &str) -> String {
    run(s, &["/bin/sh", "-c", &script(s, &format!("r state=working; w {go}; r state=done; sleep 100"))])
}

fn stop(s: &Session, mut core: Child) {
    s.json(&["server", "stop"]);
    core.wait().unwrap();
}

#[test]
fn the_sleep_helper_starts_once_per_working_turn_with_fixed_arguments_and_goes_with_it() {
    let s = Session::new();
    config(&s, "keep_awake = true");
    let core = serve(&s, &fake_inhibit(&s));
    let pid = s.json(&["server", "status"])["pid"].as_u64().unwrap();
    let expected = format!(
        "--what=sleep --mode=block --who=h0x-core --why=an agent is working {} --pid {pid} -f /dev/null",
        tail()
    );

    let first = agent(&s, "go1");
    wait_until("held", || held(&s));
    // Several 2 s ticks of the same work start nothing more.
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(inhibit_calls(&s), std::slice::from_ref(&expected), "only our own pid, nothing from a terminal");
    assert_eq!(helpers(pid), 1);

    touch(&s, "go1");
    wait_until("done", || get(&s, &first)["status"]["state"] == "done");
    wait_until("released", || !held(&s));
    wait_until("the helper ended", || helpers(pid) == 0);

    agent(&s, "go2");
    wait_until("held for the next turn", || held(&s));
    assert_eq!(inhibit_calls(&s), [expected.clone(), expected]);
    touch(&s, "go2");
    wait_until("released again", || !held(&s));
    wait_until("the second helper ended", || helpers(pid) == 0);
    stop(&s, core);
}

#[test]
fn an_invalid_keep_awake_value_is_a_config_error_and_releases_until_it_is_fixed() {
    let s = Session::new();
    config(&s, "keep_awake = true");
    let core = serve(&s, &fake_inhibit(&s));
    let pid = s.json(&["server", "status"])["pid"].as_u64().unwrap();
    agent(&s, "go");
    wait_until("held", || held(&s));
    assert_eq!(helpers(pid), 1);
    // Not a boolean: commands say so, and the running core stops holding.
    config(&s, "keep_awake = \"yes\"");
    let (_, err) = s.err(&["run", "--bg", "/bin/sh", "-c", "true"]);
    assert_eq!(err["error"]["kind"], "config", "{err}");
    wait_until("released while the value is invalid", || helpers(pid) == 0);
    config(&s, "keep_awake = true");
    wait_until("held once the value is a boolean", || held(&s));
    assert_eq!(inhibit_calls(&s).len(), 2);
    touch(&s, "go");
    wait_until("released", || !held(&s));
    stop(&s, core);
}

/// A PATH with a shell and `sleep` but no `systemd-inhibit` (as on a system without systemd).
fn without_inhibit(s: &Session) -> String {
    let bin = s.root.join("bare");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["sh", "sleep"] {
        std::os::unix::fs::symlink(Path::new("/bin").join(tool), bin.join(tool)).unwrap();
    }
    bin.display().to_string()
}

#[test]
fn a_missing_systemd_inhibit_is_logged_once_per_working_turn_not_every_tick() {
    let s = Session::new();
    config(&s, "keep_awake = true");
    let core = serve(&s, &without_inhibit(&s));
    let t = agent(&s, "go");
    wait_until("working", || get(&s, &t)["status"]["state"] == "working");
    std::thread::sleep(Duration::from_secs(7));
    let log = std::fs::read_to_string(s.root.join("core.err")).unwrap();
    assert_eq!(log.matches("keep_awake: cannot start systemd-inhibit").count(), 1, "core.err: {log}");
    stop(&s, core);
}

#[test]
fn server_status_does_not_claim_a_block_the_helper_could_not_take() {
    let s = Session::new();
    config(&s, "keep_awake = true");
    let core = serve(&s, &without_inhibit(&s));
    let t = agent(&s, "go");
    wait_until("working", || get(&s, &t)["status"]["state"] == "working");
    std::thread::sleep(Duration::from_secs(5));
    assert!(!held(&s), "systemd-inhibit could not start: nothing keeps the machine awake");
    stop(&s, core);
}

fn core_err(s: &Session) -> String {
    std::fs::read_to_string(s.root.join("core.err")).unwrap_or_default()
}

/// `<dir>/tail`, a link to the system's.
fn link_tail(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::os::unix::fs::symlink(tail(), dir.join("tail")).unwrap();
}

#[test]
fn a_systemd_inhibit_on_path_but_not_in_the_helper_dirs_is_never_run() {
    let s = Session::new();
    config(&s, "keep_awake = true");
    let core = serve_with(&s, &fake_inhibit(&s), &without_inhibit(&s));
    let t = agent(&s, "go");
    wait_until("working", || get(&s, &t)["status"]["state"] == "working");
    wait_until("the missing helper is reported", || core_err(&s).contains("keep_awake: cannot start systemd-inhibit"));
    assert_eq!(inhibit_calls(&s), [""; 0], "the one on PATH ran");
    assert!(!held(&s));
    stop(&s, core);
}

#[test]
fn empty_colon_only_and_relative_helper_dirs_never_find_a_helper() {
    for dirs in ["", "::", "bin", "./bin"] {
        let s = Session::new();
        config(&s, "keep_awake = true");
        // A fake in `bin` (also first on PATH), and both helpers in the core's working directory.
        let path = fake_inhibit(&s);
        link_tail(&s.root.join("bin"));
        std::os::unix::fs::symlink(s.root.join("bin/systemd-inhibit"), s.root.join("systemd-inhibit")).unwrap();
        link_tail(&s.root);
        let core = serve_with(&s, &path, dirs);
        let t = agent(&s, "go");
        wait_until("working", || get(&s, &t)["status"]["state"] == "working");
        wait_until("a helper found or reported missing", || {
            !inhibit_calls(&s).is_empty() || core_err(&s).contains("keep_awake: cannot start systemd-inhibit")
        });
        assert_eq!(inhibit_calls(&s), [""; 0], "helper dirs {dirs:?}: a helper ran by a relative path or from PATH");
        stop(&s, core);
    }
}

#[test]
fn the_first_real_helper_file_wins_past_a_missing_dir_a_directory_and_a_dangling_link() {
    let s = Session::new();
    config(&s, "keep_awake = true");
    fake_inhibit(&s);
    // As on a system with no /usr/bin: the first dir is not there.
    let odd = s.root.join("odd");
    std::fs::create_dir_all(odd.join("systemd-inhibit")).unwrap();
    std::os::unix::fs::symlink(s.root.join("no-such-tail"), odd.join("tail")).unwrap();
    // The helper found is a link to the fake, as `/bin` is a link to `/usr/bin` on merged-usr systems.
    let linked = s.root.join("linked");
    link_tail(&linked);
    std::os::unix::fs::symlink(s.root.join("bin/systemd-inhibit"), linked.join("systemd-inhibit")).unwrap();
    let dirs = format!("{}:{}:{}", s.root.join("no-such-dir").display(), odd.display(), linked.display());
    let core = serve_with(&s, "/usr/bin:/bin", &dirs);
    let pid = s.json(&["server", "status"])["pid"].as_u64().unwrap();
    agent(&s, "go");
    wait_until("held", || held(&s));
    let expected = format!(
        "--what=sleep --mode=block --who=h0x-core --why=an agent is working {} --pid {pid} -f /dev/null",
        linked.join("tail").display()
    );
    assert_eq!(inhibit_calls(&s), [expected]);
    touch(&s, "go");
    wait_until("released", || !held(&s));
    stop(&s, core);
}

#[test]
fn a_helper_that_is_not_executable_is_reported_and_no_block_is_claimed() {
    let s = Session::new();
    config(&s, "keep_awake = true");
    let bin = s.root.join("noexec");
    link_tail(&bin);
    std::fs::write(bin.join("systemd-inhibit"), "#!/bin/sh\nexec \"$@\"\n").unwrap();
    std::fs::set_permissions(bin.join("systemd-inhibit"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let core = serve_with(&s, "/usr/bin:/bin", &bin.display().to_string());
    let t = agent(&s, "go");
    wait_until("working", || get(&s, &t)["status"]["state"] == "working");
    wait_until("the failed start is reported", || core_err(&s).contains("keep_awake: cannot start"));
    let log = core_err(&s);
    assert!(log.contains("systemd-inhibit") && log.contains("ermission denied"), "core.err: {log}");
    assert!(!log.contains("panicked"), "core.err: {log}");
    assert!(!held(&s), "nothing keeps the machine awake");
    stop(&s, core);
}
