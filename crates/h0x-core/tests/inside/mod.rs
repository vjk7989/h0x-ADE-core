//! `h0x-core <args>` run inside a HyperOx terminal on Windows. `tests/common` is unix-only (its
//! terminals run `sh -c`); this is the cmd counterpart for the hook-style commands (SC-2), which
//! answer only inside a terminal: a batch file the terminal runs, stdin from a file.
#![allow(dead_code)]

use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_h0x-core");

/// `text` for a batch line: `%` doubled so cmd takes it literally.
fn lit(text: &str) -> String {
    text.replace('%', "%%")
}

fn quoted(path: &Path) -> String {
    format!("\"{}\"", lit(&path.display().to_string()))
}

/// `h0x-core <args>` inside a terminal of the core `core` reaches (started if needed), in `cwd` when
/// given, with `env` set first (an empty value unsets, as in cmd) and `input` on its stdin. Its
/// files go in `dir`. Returns what it printed and its exit code; the terminal ends with it.
pub fn in_terminal(
    core: &dyn Fn(&[&str]) -> Command,
    dir: &Path,
    args: &[&str],
    input: &[u8],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
) -> Output {
    static N: AtomicU32 = AtomicU32::new(0);
    let tag = N.fetch_add(1, Ordering::Relaxed);
    std::fs::create_dir_all(dir).unwrap();
    let file = |what: &str| dir.join(format!("wt{tag}-{what}"));
    std::fs::write(file("in"), input).unwrap();
    let started = core(&["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let mut bat = String::from("@echo off\r\n");
    for (k, v) in env {
        bat += &format!("set \"{k}={}\"\r\n", lit(v));
    }
    if let Some(cwd) = cwd {
        bat += &format!("cd /d {}\r\n", quoted(cwd));
    }
    // A closing quote after backslashes needs them doubled (the C runtime rules every exe parses by).
    let args: String = args
        .iter()
        .map(|a| {
            let trail = a.len() - a.trim_end_matches('\\').len();
            format!(" \"{}{}\"", lit(a), "\\".repeat(trail))
        })
        .collect();
    bat += &format!(
        "{}{args} < {} > {} 2> {}\r\n",
        quoted(Path::new(BIN)),
        quoted(&file("in")),
        quoted(&file("out")),
        quoted(&file("err"))
    );
    // Redirection first: `echo 2> file` would be stderr.
    bat += &format!("> {} echo %errorlevel%\r\n", quoted(&file("code")));
    bat += &format!("type nul > {}\r\n", quoted(&file("done")));
    let script = file("run.cmd");
    std::fs::write(&script, bat).unwrap();
    let ran = core(&["--json", "run", "--bg", "cmd", "/c", script.to_str().unwrap()]).output().unwrap();
    assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
    let limit = Instant::now() + Duration::from_secs(30);
    while !file("done").exists() {
        assert!(Instant::now() < limit, "the command never finished in its terminal: {args}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let code: i32 = std::fs::read_to_string(file("code")).unwrap().trim().parse().unwrap();
    Output {
        status: std::process::ExitStatus::from_raw(code as u32),
        stdout: std::fs::read(file("out")).unwrap(),
        stderr: std::fs::read(file("err")).unwrap(),
    }
}

/// A throwaway core of its own: its session, home and XDG dirs under the target's temp folder.
/// Dropping it stops the core and removes the folder.
pub struct Session {
    pub name: String,
    pub root: PathBuf,
}

impl Session {
    pub fn new() -> Session {
        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!("wt-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("h0x-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Session { name, root }
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("H0X_CORE_SESSION", &self.name)
            .env("H0X_CORE_SETTLE_MS", "0")
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("H0X_TERMINAL_ID")
            .env_remove("H0X_TERMINAL_SOCKET")
            .env_remove("H0X_SCOPE")
            .env_remove("H0X_CORE_SOCKET");
        cmd
    }

    pub fn in_terminal(&self, args: &[&str], input: &[u8], cwd: Option<&Path>, env: &[(&str, &str)]) -> Output {
        in_terminal(&|a| self.cmd(a), &self.root.join("it"), args, input, cwd, env)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
