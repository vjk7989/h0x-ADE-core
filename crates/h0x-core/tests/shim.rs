//! `claude` typed by hand (SC-SHIM): the core's program started under the name `claude` finds the
//! real one further down PATH (skipping shim folders, so no loop), adds HyperOx's `--settings` only
//! when its core vouches for the terminal and the person gave none, and passes everything else and
//! the exit code through. A fake `claude` prints its arguments, one `[..]` each (a `.cmd` on Windows
//! prints its 1st and 3rd).
#![cfg_attr(not(any(unix, windows)), allow(dead_code, unused_imports))]
#[cfg(unix)]
mod common;
#[cfg(windows)]
mod inside;

use h0x_core::shim::{MARKER, claude_settings};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_h0x-core");
const SEP: &str = if cfg!(windows) { ";" } else { ":" };

/// One test's folders: `shim` (marker, the program as `claude`), `bait` (marker, a `claude` that
/// must never run) and `real` (where the fake goes; a space in its name on Windows).
struct Rig {
    root: PathBuf,
    shim: PathBuf,
    real: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let base = if cfg!(unix) { PathBuf::from("/var/tmp") } else { PathBuf::from(env!("CARGO_TARGET_TMPDIR")) };
        let root = base.join(format!("h0x-shim-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (shim_dir, bait, real) = (root.join("shim"), root.join("bait"), root.join("real dir"));
        for dir in [&shim_dir, &bait, &real] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for dir in [&shim_dir, &bait] {
            std::fs::write(dir.join(MARKER), "").unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(BIN, shim_dir.join("claude")).unwrap();
            write_fake(&bait.join("claude"), "#!/bin/sh\necho BAIT\n");
        }
        #[cfg(windows)]
        {
            std::fs::copy(BIN, shim_dir.join("claude.exe")).unwrap();
            write_fake(&bait.join("claude.cmd"), "@echo off\r\necho BAIT\r\n");
        }
        let shim = shim_dir.join(if cfg!(windows) { "claude.exe" } else { "claude" });
        Rig { root, shim, real }
    }

    /// The fake `claude` that prints its arguments and exits with $FAKE_EXIT.
    fn fake(&self) {
        #[cfg(unix)]
        write_fake(
            &self.real.join("claude"),
            "#!/bin/sh\nfor a; do printf '[%s]\\n' \"$a\"; done\nexit ${FAKE_EXIT:-0}\n",
        );
        #[cfg(windows)]
        write_fake(&self.real.join("claude.cmd"), "@echo off\r\necho [%~1][%~3]\r\nexit /b %FAKE_EXIT%\r\n");
    }

    /// PATH: shim folder, bait, real, then the ordinary PATH.
    fn path(&self) -> String {
        let dir = |d: &str| self.root.join(d).display().to_string();
        [dir("shim"), dir("bait"), self.real.display().to_string(), std::env::var("PATH").unwrap_or_default()].join(SEP)
    }

    /// The shim run directly, outside any terminal.
    fn run(&self, path: &str, exit: &str, args: &[&str]) -> Output {
        Command::new(&self.shim)
            .args(args)
            .env("PATH", path)
            .env("FAKE_EXIT", exit)
            .env_remove("H0X_TERMINAL_ID")
            .env_remove("H0X_TERMINAL_SOCKET")
            .env_remove("H0X_SCOPE")
            .env_remove("H0X_TOKEN_SAVER")
            .output()
            .unwrap()
    }

    /// The shim run inside a terminal of a core of its own (so the core vouches).
    fn vouched(&self, exit: &str, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let path = self.path();
        let mut env = vec![("PATH", path.as_str()), ("FAKE_EXIT", exit)];
        env.extend_from_slice(extra);
        #[cfg(unix)]
        {
            let s = common::Session::new();
            common::in_terminal_as(&s, self.shim.to_str().unwrap(), args, b"", None, &env)
        }
        #[cfg(windows)]
        {
            let s = inside::Session::new();
            inside::in_terminal_as(&|a| s.cmd(a), &s.root.join("it"), &self.shim, args, b"", None, &env)
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_fake(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim_end().to_owned()).collect()
}

/// Arguments with a space, an empty word, `=`, a quote and a flag-like word.
#[cfg(unix)]
const ARGS: [&str; 5] = ["a b", "", "--model=x", "q\"uote", "-p"];
#[cfg(windows)]
const ARGS: [&str; 3] = ["a b", "x", "y z"];

#[test]
fn outside_a_terminal_the_arguments_pass_through_byte_identical() {
    let rig = Rig::new("plain");
    rig.fake();
    let out = rig.run(&rig.path(), "0", &ARGS);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    #[cfg(unix)]
    assert_eq!(lines(&out), ["[a b]", "[]", "[--model=x]", "[q\"uote]", "[-p]"]);
    #[cfg(windows)]
    assert_eq!(lines(&out), ["[a b][y z]"], "a .cmd target, in a folder with a space");
}

#[test]
fn a_shim_folder_is_never_the_real_program_and_a_missing_one_exits_127() {
    let rig = Rig::new("none");
    let only_shims = format!("{}{SEP}{}", rig.root.join("shim").display(), rig.root.join("bait").display());
    let out = rig.run(&only_shims, "0", &["x"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("claude not found on PATH"));
    assert!(out.stdout.is_empty(), "the bait must not run");
}

#[test]
fn the_exit_code_passes_through() {
    let rig = Rig::new("code");
    rig.fake();
    assert_eq!(rig.run(&rig.path(), "42", &["x"]).status.code(), Some(42));
    assert_eq!(rig.run(&rig.path(), "0", &["x"]).status.code(), Some(0));
}

#[cfg(unix)]
#[test]
fn inside_a_terminal_the_settings_are_added_once_and_the_persons_own_win() {
    let rig = Rig::new("vouched");
    rig.fake();
    let out = rig.vouched("0", &["hello", "-p"], &[]);
    let got = lines(&out);
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0], "[--settings]");
    assert_eq!(got[1], format!("[{}]", claude_settings(true)), "Token Saver on by default");
    assert_eq!(got[2..], ["[hello]", "[-p]"]);
    let off = lines(&rig.vouched("0", &["x"], &[("H0X_TOKEN_SAVER", "0")]));
    assert_eq!(off[1], format!("[{}]", claude_settings(false)));
    let on = lines(&rig.vouched("0", &["x"], &[("H0X_TOKEN_SAVER", "1")]));
    assert_eq!(on[1], format!("[{}]", claude_settings(true)));
    for own in [["--settings", "mine.json", "x"], ["--settings=mine.json", "x", "y"]] {
        assert_eq!(lines(&rig.vouched("0", &own, &[])).len(), 3, "untouched: {own:?}");
    }
    let own = lines(&rig.vouched("3", &["--settings", "mine.json"], &[]));
    assert_eq!(own, ["[--settings]", "[mine.json]"]);
}

#[cfg(windows)]
#[test]
fn inside_a_terminal_the_settings_go_first_unless_the_person_gave_some() {
    let rig = Rig::new("vouched");
    rig.fake();
    // The .cmd prints its 1st and 3rd word: ours come first, so the 3rd is the person's 1st.
    assert_eq!(lines(&rig.vouched("0", &["a", "b"], &[])), ["[--settings][a]"]);
    assert_eq!(lines(&rig.vouched("0", &["--settings", "mine.json", "y"], &[])), ["[--settings][y]"]);
    assert_eq!(rig.vouched("9", &["a"], &[]).status.code(), Some(9));
}

#[cfg(windows)]
#[test]
fn an_exe_target_in_a_folder_with_a_space_runs_with_its_arguments_and_code() {
    let rig = Rig::new("exe");
    let cmd = Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("cmd.exe");
    std::fs::copy(cmd, rig.real.join("claude.exe")).unwrap();
    let out = rig.run(&rig.path(), "0", &["/c", "echo", "hello world"]);
    // cmd's echo prints the quotes it was given: the words arrived as one argument.
    assert_eq!(lines(&out), ["\"hello world\""]);
    assert_eq!(rig.run(&rig.path(), "0", &["/c", "exit", "5"]).status.code(), Some(5));
}

/// The core's own folder: made at start with its marker, first on PATH for terminals only (never
/// for `exec` children), and pointed at the running program again after a change.
#[cfg(unix)]
#[test]
fn the_core_keeps_a_shim_folder_that_only_its_terminals_have_on_path() {
    let s = common::Session::new();
    assert!(s.core(&["server", "start"]).status.success());
    let bin = s.root.join("state/h0x-core/test/bin");
    assert!(bin.join(MARKER).is_file());
    let link = bin.join("claude");
    let target = |p: &Path| std::fs::canonicalize(p).unwrap();
    assert_eq!(target(&link), target(Path::new(BIN)));
    assert_eq!(std::fs::metadata(&bin).map(|m| std::os::unix::fs::PermissionsExt::mode(&m) & 0o777).unwrap(), 0o700);

    let path = common::in_terminal_as(&s, "/bin/sh", &["-c", "echo $PATH"], b"", None, &[]);
    let path = String::from_utf8_lossy(&path.stdout).into_owned();
    assert!(path.starts_with(&format!("{}:", bin.display())), "{path}");

    let exec = String::from_utf8_lossy(&s.core(&["exec", "echo $PATH"]).stdout).into_owned();
    assert!(!exec.contains(&bin.display().to_string()), "{exec}");

    // A stale link is replaced when the core starts again.
    s.core(&["server", "stop"]);
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("/bin/false", &link).unwrap();
    assert!(s.core(&["server", "start"]).status.success());
    assert_eq!(target(&link), target(Path::new(BIN)));
}

// --- edge cases (SC-SHIM test writer) -------------------------------------------------------

/// PATH with empty, relative and repeated entries, and both shim folders listed twice.
#[test]
fn empty_relative_repeated_entries_and_doubled_shim_folders_still_find_the_real_one() {
    let rig = Rig::new("messy");
    rig.fake();
    let dir = |d: &str| rig.root.join(d).display().to_string();
    let real = rig.real.display().to_string();
    let path = ["", &dir("shim"), &dir("shim"), "no-such-relative-dir", &dir("bait"), "", &dir("bait"), &real, &real]
        .join(SEP);
    let out = rig.run(&path, "0", &["a", "b", "c"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    #[cfg(unix)]
    assert_eq!(lines(&out), ["[a]", "[b]", "[c]"]);
    #[cfg(windows)]
    assert_eq!(lines(&out), ["[a][c]"]);
}

/// A `claude` in an unmarked folder that is this very program must not start it again and again.
#[test]
fn a_claude_that_is_the_shim_itself_is_not_run_again() {
    let rig = Rig::new("self");
    let other = rig.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(BIN, other.join("claude")).unwrap();
    #[cfg(windows)]
    std::fs::copy(BIN, other.join("claude.exe")).unwrap();
    let path = format!("{}{SEP}{}", rig.root.join("shim").display(), other.display());
    let mut cmd = Command::new(&rig.shim);
    cmd.arg("x")
        .env("PATH", path)
        .env_remove("H0X_TERMINAL_ID")
        .env_remove("H0X_TERMINAL_SOCKET")
        .env_remove("H0X_SCOPE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let code = bounded(cmd);
    assert_eq!(code, Some(127), "the shim found itself: it must stop with 127, not loop");
}

/// Runs `cmd` for at most 20 s; None when it had to be ended. Unix: an exec loop keeps the pid.
#[cfg(unix)]
fn bounded(mut cmd: Command) -> Option<i32> {
    let mut child = cmd.spawn().unwrap();
    let limit = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        if std::time::Instant::now() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Windows: each level would start a child and wait, a chain without end. The child is started
/// suspended inside a job that allows 6 processes and ends them all when dropped.
#[cfg(windows)]
fn bounded(mut cmd: Command) -> Option<i32> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::*;
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = cmd.spawn().unwrap();
    let job = unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        assert!(!job.is_null());
        let job = OwnedHandle::from_raw_handle(job as RawHandle);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 6;
        let set = SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        assert!(set != 0, "{}", std::io::Error::last_os_error());
        let joined = AssignProcessToJobObject(job.as_raw_handle() as HANDLE, child.as_raw_handle() as HANDLE);
        if joined == 0 {
            let _ = child.kill();
            panic!("cannot contain the child: {}", std::io::Error::last_os_error());
        }
        // Resume its threads (std keeps the main thread's handle to itself).
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        assert!(snapshot != INVALID_HANDLE_VALUE);
        let snapshot = OwnedHandle::from_raw_handle(snapshot as RawHandle);
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut more = Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry);
        while more != 0 {
            if entry.th32OwnerProcessID == child.id() {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    ResumeThread(thread);
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry);
        }
        job
    };
    let limit = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if std::time::Instant::now() > limit {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) };
    drop(job);
    code
}

/// The fake that prints its arguments and $H0X_SCOPE (last, as `SCOPE=`).
#[cfg(unix)]
fn fake_with_scope(rig: &Rig) {
    write_fake(
        &rig.real.join("claude"),
        "#!/bin/sh\nfor a; do printf '[%s]\\n' \"$a\"; done\nprintf 'SCOPE=%s\\n' \"$H0X_SCOPE\"\n",
    );
}

/// Only "0" turns the gate off; "", "1", "false" and " 0" leave it on. The scope token never
/// rides in argv.
#[cfg(unix)]
#[test]
fn only_token_saver_zero_drops_the_gate_and_the_scope_stays_out_of_argv() {
    let rig = Rig::new("saver");
    fake_with_scope(&rig);
    for (value, on) in [("", true), ("0", false), ("1", true), ("false", true), (" 0", true)] {
        let got = lines(&rig.vouched("0", &["x"], &[("H0X_TOKEN_SAVER", value)]));
        assert_eq!(got.len(), 4, "{value:?}: {got:?}");
        assert_eq!(got[0], "[--settings]", "{value:?}");
        assert_eq!(got[1], format!("[{}]", claude_settings(on)), "H0X_TOKEN_SAVER={value:?}");
        assert_eq!(got[2], "[x]");
        let scope = got[3].strip_prefix("SCOPE=").unwrap();
        assert!(!scope.is_empty(), "inside a terminal the scope is set");
        assert!(got[..3].iter().all(|a| !a.contains(scope) && !a.contains("H0X_SCOPE")), "{got:?}");
    }
}

/// Windows: a fake `claude.cmd` that saves its raw arguments (`%*`) to `argv.txt` beside it, so
/// the test reads them back the way a C runtime program (node behind the real claude.cmd) would.
#[cfg(windows)]
fn fake_argv(rig: &Rig) {
    write_fake(
        &rig.real.join("claude.cmd"),
        "@echo off\r\n>\"%~dp0argv.txt\" echo(%*\r\n>\"%~dp0scope.txt\" echo(%H0X_SCOPE%\r\nexit /b %FAKE_EXIT%\r\n",
    );
}

/// The words of a command line by the C runtime's rules (2008+: `""` inside quotes is one `"`).
#[cfg(windows)]
fn c_runtime_words(line: &str) -> Vec<String> {
    let c: Vec<char> = line.chars().collect();
    let (mut out, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => {
                let mut n = 0;
                while i < c.len() && c[i] == '\\' {
                    n += 1;
                    i += 1;
                }
                if c.get(i) == Some(&'"') {
                    cur.push_str(&"\\".repeat(n / 2));
                    if n % 2 == 1 {
                        cur.push('"');
                        i += 1;
                    }
                } else {
                    cur.push_str(&"\\".repeat(n));
                }
                any = true;
                continue;
            }
            '"' if quoted && c.get(i + 1) == Some(&'"') => {
                cur.push('"');
                i += 1;
            }
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' | '\t' if !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            ch => {
                cur.push(ch);
                any = true;
            }
        }
        i += 1;
    }
    if any {
        out.push(cur);
    }
    out
}

#[cfg(windows)]
fn saved_argv(rig: &Rig) -> Vec<String> {
    let text = std::fs::read_to_string(rig.real.join("argv.txt")).unwrap();
    c_runtime_words(text.trim_end_matches(['\r', '\n']))
}

/// cmd's special characters reach a `.cmd` target intact (std quotes them for cmd), or std refuses
/// the line and the shim says so with exit 1. Each word goes alone so one refusal hides nothing.
#[cfg(windows)]
#[test]
fn on_windows_cmd_special_characters_reach_a_cmd_claude_intact() {
    let rig = Rig::new("special");
    fake_argv(&rig);
    let words = [
        "a b",
        "q\"uote",
        "\"",
        "50%",
        "%PATH%",
        "%FAKE_EXIT%",
        "x^y",
        "^",
        "a&b",
        "a & echo pwned",
        "bang!",
        "!PATH!",
        "a|b",
        "<in>",
        "(paren)",
        "trail\\",
        "",
        "--model=x",
    ];
    for word in words {
        let _ = std::fs::remove_file(rig.real.join("argv.txt"));
        let out = rig.run(&rig.path(), "0", &[word]);
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.code() == Some(1) && err.contains("claude:") {
            assert!(!rig.real.join("argv.txt").exists(), "{word:?}: refused, yet the fake ran");
            continue;
        }
        assert!(out.status.success(), "{word:?}: {err}");
        assert!(!lines(&out).iter().any(|l| l.contains("pwned")), "{word:?}: cmd ran part of an argument");
        assert_eq!(saved_argv(&rig), [word], "{word:?}");
    }
    // All at once, in order.
    let out = rig.run(&rig.path(), "0", &words);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(saved_argv(&rig), words);
}

/// A line break can't be passed to a batch file safely: std refuses, the shim exits 1 and says why.
#[cfg(windows)]
#[test]
fn on_windows_an_argument_std_refuses_for_a_cmd_claude_exits_1_with_a_message() {
    let rig = Rig::new("refused");
    fake_argv(&rig);
    for word in ["two\nlines", "cr\rhere"] {
        let out = rig.run(&rig.path(), "0", &["ok", word]);
        assert_eq!(out.status.code(), Some(1), "{word:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("claude:") && err.to_ascii_lowercase().contains("claude.cmd"), "{err}");
        assert!(!rig.real.join("argv.txt").exists(), "the fake must not run");
    }
}

/// Windows: the exact words a vouched `claude.cmd` gets, for each H0X_TOKEN_SAVER value ("" unsets
/// in the cmd harness, so it reads as on). The scope token never rides in argv.
#[cfg(windows)]
#[test]
fn on_windows_only_token_saver_zero_drops_the_gate_and_the_scope_stays_out_of_argv() {
    let rig = Rig::new("saver");
    fake_argv(&rig);
    for (value, on) in [("", true), ("0", false), ("1", true), ("false", true)] {
        let out = rig.vouched("0", &["x", "a b"], &[("H0X_TOKEN_SAVER", value)]);
        assert!(out.status.success(), "{value:?}: {}", String::from_utf8_lossy(&out.stderr));
        let got = saved_argv(&rig);
        assert_eq!(got, ["--settings".to_owned(), claude_settings(on), "x".into(), "a b".into()], "{value:?}");
        let scope = std::fs::read_to_string(rig.real.join("scope.txt")).unwrap().trim().to_owned();
        assert!(!scope.is_empty() && !scope.contains("H0X_SCOPE"), "inside a terminal the scope is set");
        assert!(got.iter().all(|a| !a.contains(&scope) && !a.contains("H0X_SCOPE")), "{got:?}");
    }
}

/// Windows: the core makes `<state>\bin` with the marker and `claude.exe` (the same bytes as the
/// core), its terminals find that `claude` first, `exec` children don't, and a changed
/// `claude.exe` is linked to the core again at the next start.
#[cfg(windows)]
#[test]
fn on_windows_the_core_keeps_a_shim_folder_that_only_its_terminals_have_on_path() {
    let s = inside::Session::new();
    let start = s.cmd(&["server", "start"]).output().unwrap();
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stderr));
    let bin = s.root.join("state").join("h0x-core").join(&s.name).join("bin");
    assert!(bin.join(MARKER).is_file());
    let shim = bin.join("claude.exe");
    let core = std::fs::read(BIN).unwrap();
    assert!(std::fs::read(&shim).unwrap() == core, "claude.exe is the core");

    // `where claude` lists PATH matches in order: the shim folder's comes first.
    let where_exe = Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("where.exe");
    let found = inside::in_terminal_as(&|a| s.cmd(a), &s.root.join("it"), &where_exe, &["claude"], b"", None, &[]);
    let first = lines(&found).into_iter().next().unwrap_or_default();
    // Compare as paths: the test root may be spelled with `/` (from CARGO_TARGET_DIR), `where` prints `\`.
    let norm = |p: &str| p.replace('/', "\\").to_ascii_lowercase();
    assert_eq!(norm(&first), norm(&shim.display().to_string()), "first claude on a terminal's PATH");

    let exec = s.cmd(&["exec", "where claude"]).output().unwrap();
    let exec = norm(&String::from_utf8_lossy(&exec.stdout));
    assert!(!exec.contains(&norm(&bin.display().to_string())), "exec children don't get it: {exec}");

    // Another claude.exe (never written through: it is a hardlink of the core) is replaced.
    assert!(s.cmd(&["server", "stop"]).output().unwrap().status.success());
    std::fs::remove_file(&shim).unwrap();
    std::fs::write(&shim, b"a stale shim").unwrap();
    assert!(s.cmd(&["server", "start"]).output().unwrap().status.success());
    assert!(std::fs::read(&shim).unwrap() == core, "linked to the core again");
    assert!(std::fs::read(BIN).unwrap() == core, "the core itself untouched");
}

// Ctrl+C (Windows) is not tested here. The shim installs a handler, not SetConsoleCtrlHandler(NULL),
// so the child's inherited "ignore Ctrl+C" flag stays clear; reading that flag needs the child's
// PEB, and a real Ctrl+C needs a console shared with the test runner, which would stop the whole
// test process. Manual check: run `claude` in a HyperOx terminal and press Ctrl+C.

#[test]
fn a_nested_launch_runs_and_a_runaway_chain_is_refused() {
    let rig = Rig::new("depth");
    rig.fake();
    let at = |depth: &str| {
        Command::new(&rig.shim)
            .arg("x")
            .env("PATH", rig.path())
            .env("FAKE_EXIT", "0")
            .env("H0X_SHIM_DEPTH", depth)
            .env_remove("H0X_TERMINAL_ID")
            .env_remove("H0X_SCOPE")
            .output()
            .unwrap()
    };
    for ok in ["1", "7", "junk"] {
        assert!(at(ok).status.success(), "depth {ok}");
    }
    let refused = at("8");
    assert_eq!(refused.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("resolves back to HyperOx"));
}

/// The shim as the terminal's very first program asks the core before the program's pid may be
/// recorded: it is still vouched.
#[cfg(unix)]
#[test]
fn the_first_program_of_a_terminal_is_vouched_at_once() {
    let rig = Rig::new("first");
    rig.fake();
    let mut s = common::Session::new();
    s.env.push(("PATH".into(), rig.path()));
    let t = common::run(&s, &[rig.shim.to_str().unwrap(), "x"]);
    common::wait_until("the settings reach the fake claude", || common::text(&s, &t).contains("[--settings]"));
}
