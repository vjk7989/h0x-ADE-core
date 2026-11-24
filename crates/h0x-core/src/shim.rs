//! `claude` typed by hand inside a HyperOx terminal (ADR-129). Each core keeps a private
//! folder (`Paths::bin_dir`, marked by `.h0x-shim`) with `claude` pointing at this program and
//! puts it first on PATH for its own terminals only. Started under that name, the program
//! finds the real `claude` further down PATH and runs it, adding HyperOx's `--settings` when the
//! core vouches for the terminal and the person gave none. Anywhere else it runs Claude unchanged.

use crate::paths::Paths;
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// A folder holding this file is a shim folder: the lookup of the real program skips it.
pub const MARKER: &str = ".h0x-shim";

/// Claude Code's `--settings` JSON for a HyperOx launch. Core sets `H0X_CORE_EXE` in every
/// terminal; Claude runs each hook through a shell, so it is read at run time, never from argv.
/// Always the SessionStart hooks (the session id changes on `/clear` and resume); with
/// `token_saver`, also the PostToolUse hook.
pub fn claude_settings(token_saver: bool) -> String {
    let mut hooks = json!({"SessionStart": [
        {"matcher": "clear", "hooks": [{"type": "command", "command": "\"$H0X_CORE_EXE\" handoff"}]},
        {"hooks": [{"type": "command", "command": "\"$H0X_CORE_EXE\" hook session --agent claude", "timeout": 10}]}
    ]});
    if token_saver {
        hooks["PostToolUse"] = json!([{"matcher": "^(Bash|Read|Grep)$", "hooks": [{"type": "command", "command": "\"$H0X_CORE_EXE\" gate", "timeout": 20}]}]);
    }
    json!({ "hooks": hooks }).to_string()
}

/// Whether the person already gave `--settings` (before any `--`): theirs wins.
pub fn has_settings<S: AsRef<OsStr>>(args: &[S]) -> bool {
    args.iter()
        .map(|a| a.as_ref().as_encoded_bytes())
        .take_while(|a| *a != b"--")
        .any(|a| a == b"--settings" || a.starts_with(b"--settings="))
}

/// Whether this process was started as `claude` (its folder's shim link).
pub fn is_claude(argv0: &OsStr) -> bool {
    Path::new(argv0)
        .file_stem()
        .is_some_and(|s| if cfg!(windows) { s.eq_ignore_ascii_case("claude") } else { s == "claude" })
}

/// How many shims run above this one (set for the child only; Claude's own children inherit it, so
/// nested launches are legitimate). A runaway backstop: `is_self` is the real protection.
const DEPTH_VAR: &str = "H0X_SHIM_DEPTH";
const MAX_DEPTH: u32 = 8;

/// The depth for the child, or None at the limit. Missing or invalid counts as 0.
fn next_depth(current: Option<&OsStr>) -> Option<u32> {
    let depth = current.and_then(OsStr::to_str).and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
    (depth < MAX_DEPTH).then_some(depth + 1)
}

/// Whether `p` is this very program: the same path, or the same file (a link or hardlink to it).
fn is_self(p: &Path) -> bool {
    let Ok(me) = std::env::current_exe() else { return false };
    if p.canonicalize().ok().zip(me.canonicalize().ok()).is_some_and(|(a, b)| a == b) {
        return true;
    }
    let (Ok(a), Ok(b)) = (std::fs::metadata(p), std::fs::metadata(&me)) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (a.dev(), a.ino()) == (b.dev(), b.ino())
    }
    #[cfg(windows)]
    {
        a.len() == b.len() && crate::platform::file_identity(&a) == crate::platform::file_identity(&b)
    }
}

/// The real `claude`: the first one on `path` outside every shim folder, and not this program.
fn find_real(path: &OsStr) -> Option<PathBuf> {
    let dirs = std::env::split_paths(path).filter(|d| !d.as_os_str().is_empty() && !d.join(MARKER).exists());
    #[cfg(windows)]
    {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        dirs.filter_map(|d| crate::platform::resolve("claude", &d.to_string_lossy(), &exts)).find(|p| !is_self(p))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        dirs.map(|d| d.join("claude")).find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) && !is_self(p)
        })
    }
}

/// Shim mode: run the real `claude` with `args`; returns the exit code (unix: only on failure).
pub fn run(mut args: Vec<OsString>) -> i32 {
    let Some(depth) = next_depth(std::env::var_os(DEPTH_VAR).as_deref()) else {
        eprintln!("claude resolves back to HyperOx");
        return 127;
    };
    let Some(real) = find_real(&std::env::var_os("PATH").unwrap_or_default()) else {
        eprintln!("claude not found on PATH");
        return 127;
    };
    if crate::client::vouched() && !has_settings(&args) {
        let token_saver = std::env::var_os("H0X_TOKEN_SAVER").is_none_or(|v| v != "0");
        args.splice(0..0, [OsString::from("--settings"), claude_settings(token_saver).into()]);
    }
    let mut cmd = std::process::Command::new(&real);
    cmd.args(&args).env(DEPTH_VAR, depth.to_string());
    launch(&real, cmd)
}

#[cfg(unix)]
fn launch(real: &Path, mut cmd: std::process::Command) -> i32 {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    eprintln!("claude: {}: {err}", real.display());
    126
}

/// The child shares our console: Ctrl+C reaches it, and only its end ends the shim.
#[cfg(windows)]
fn launch(real: &Path, mut cmd: std::process::Command) -> i32 {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn ignore(kind: u32) -> windows_sys::core::BOOL {
        i32::from(kind <= 1) // Ctrl+C and Ctrl+Break only
    }
    unsafe { SetConsoleCtrlHandler(Some(ignore), 1) };
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("claude: {}: {e}", real.display());
            1
        }
    }
}

/// `bin` first on the terminal's PATH (the key's spelling kept; any case on Windows).
pub fn prepend_path(env: &mut BTreeMap<String, String>, bin: &Path) {
    let key = env.keys().find(|k| if cfg!(windows) { k.eq_ignore_ascii_case("PATH") } else { k.as_str() == "PATH" });
    let key = key.cloned().unwrap_or_else(|| "PATH".into());
    let old = env.get(&key).map(OsString::from).or_else(|| std::env::var_os("PATH")).unwrap_or_default();
    let same = |d: &Path| {
        if cfg!(windows) { d.as_os_str().eq_ignore_ascii_case(bin.as_os_str()) } else { d == bin }
    };
    // Empty entries (an empty PATH splits into one) mean the current folder: dropped. So are other
    // cores' shim folders: a nested session must never find a foreign shim first.
    let rest: Vec<PathBuf> = std::env::split_paths(&old)
        .filter(|d| !d.as_os_str().is_empty() && !same(d) && !d.join(MARKER).exists())
        .collect();
    if let Ok(joined) = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(rest)) {
        env.insert(key, joined.to_string_lossy().into_owned());
    }
}

/// Create or refresh this core's shim folder: the marker and `claude` pointing at the running
/// program. Unix: a symlink, swapped atomically. Windows: a hardlink `claude.exe` (a copy across
/// volumes); a shim that is still running can't be replaced but can be renamed away, and the
/// renamed ones are removed here at the next start.
pub fn ensure(paths: &Paths) -> io::Result<()> {
    let bin = paths.bin_dir();
    crate::platform::private_dir(&bin)?;
    let marker = bin.join(MARKER);
    if !marker.exists() {
        std::fs::write(marker, b"")?;
    }
    let exe = crate::client::own_exe().ok_or_else(|| io::Error::other("no path for this program"))?;
    link(&bin, &exe)
}

#[cfg(unix)]
fn link(bin: &Path, exe: &Path) -> io::Result<()> {
    let shim = bin.join("claude");
    if std::fs::read_link(&shim).is_ok_and(|t| t == exe) {
        return Ok(());
    }
    let tmp = bin.join(".claude.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(exe, &tmp)?;
    std::fs::rename(tmp, shim)
}

#[cfg(windows)]
fn link(bin: &Path, exe: &Path) -> io::Result<()> {
    for old in std::fs::read_dir(bin)?.flatten() {
        if old.file_name().to_string_lossy().starts_with("claude.exe.old-") {
            let _ = std::fs::remove_file(old.path()); // fails while it still runs: next start
        }
    }
    let shim = bin.join("claude.exe");
    let same = |a: &std::fs::Metadata, b: &std::fs::Metadata| {
        crate::platform::file_identity(a) == crate::platform::file_identity(b)
            || (a.len() == b.len() && a.modified().ok() == b.modified().ok()) // a copy of the same build
    };
    if let (Ok(a), Ok(b)) = (std::fs::metadata(&shim), std::fs::metadata(exe)) {
        if same(&a, &b) {
            return Ok(());
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let old = bin.join(format!("claude.exe.old-{}-{nanos}", std::process::id()));
        std::fs::rename(&shim, &old)?; // a running exe can be renamed, not deleted
        let _ = std::fs::remove_file(old);
    }
    std::fs::hard_link(exe, &shim).or_else(|_| std::fs::copy(exe, &shim).map(drop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_have_the_session_hooks_and_the_gate_only_with_token_saver() {
        let on: serde_json::Value = serde_json::from_str(&claude_settings(true)).unwrap();
        let off: serde_json::Value = serde_json::from_str(&claude_settings(false)).unwrap();
        assert_eq!(on["hooks"]["PostToolUse"][0]["matcher"], "^(Bash|Read|Grep)$");
        assert!(off["hooks"].get("PostToolUse").is_none());
        assert_eq!(on["hooks"]["SessionStart"], off["hooks"]["SessionStart"]);
        assert_eq!(on["hooks"]["SessionStart"][0]["matcher"], "clear");
        assert!(claude_settings(true).contains("$H0X_CORE_EXE"));
        assert!(!claude_settings(true).contains("H0X_SCOPE"));
    }

    #[test]
    fn the_persons_settings_flag_is_found_before_a_double_dash_only() {
        let has = |a: &[&str]| has_settings(a);
        assert!(has(&["--settings", "x.json"]));
        assert!(has(&["-p", "--settings=x.json"]));
        assert!(!has(&["--", "--settings", "x"]));
        assert!(!has(&["--settingsx", "1"]));
        assert!(!has(&[]));
    }

    #[test]
    fn only_a_program_named_claude_is_the_shim() {
        assert!(is_claude(OsStr::new("claude")));
        assert!(is_claude(OsStr::new("/a/b/claude")));
        assert!(!is_claude(OsStr::new("h0x-core")));
        assert!(!is_claude(OsStr::new("claudex")));
    }

    #[test]
    fn path_gets_the_shim_folder_first() {
        let mut env = BTreeMap::from([("PATH".to_owned(), "x".to_owned())]);
        prepend_path(&mut env, Path::new("bin"));
        let first = std::env::split_paths(&env["PATH"]).next().unwrap();
        assert_eq!(first, Path::new("bin"));
    }

    // --- edge cases (SC-SHIM test writer) ---------------------------------------------------

    const SEP: &str = if cfg!(windows) { ";" } else { ":" };

    /// A fresh folder under the temp dir (the gate points TEMP at the workspace cache).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("h0x-shim-{name}-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn paths(state: &Path) -> Paths {
        Paths {
            socket: state.join("s.sock"),
            holder_socket: state.join("s.holder.sock"),
            state: state.to_path_buf(),
            config: state.join("config.toml"),
        }
    }

    /// A runnable `claude` in `dir` (unix: mode 755; Windows: a `.cmd`, found through PATHEXT).
    fn real_in(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(if cfg!(windows) { "claude.cmd" } else { "claude" });
        std::fs::write(&p, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// A shim folder: the marker and a runnable `claude` that must never be chosen.
    fn shim_in(dir: &Path) {
        real_in(dir);
        std::fs::write(dir.join(MARKER), "").unwrap();
    }

    /// PATH from raw entries (empty ones kept, which `join_paths` would also allow).
    fn path_of(entries: &[&Path]) -> OsString {
        let mut out = OsString::new();
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                out.push(SEP);
            }
            out.push(e.as_os_str());
        }
        out
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
    }

    #[test]
    fn the_lookup_skips_empty_missing_and_relative_entries() {
        let root = scratch("lookup");
        let real = real_in(&root.join("real"));
        let empty = Path::new("");
        let rel = Path::new("no-such-relative-dir");
        let found = find_real(&path_of(&[empty, empty, rel, &root.join("missing"), &root.join("real"), empty]));
        assert!(found.is_some_and(|f| same_file(&f, &real)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_shim_folder_listed_twice_is_skipped_both_times() {
        let root = scratch("twice");
        let shim = root.join("shim");
        shim_in(&shim);
        let real = real_in(&root.join("real"));
        let found = find_real(&path_of(&[&shim, &shim, &root.join("real"), &shim]));
        assert!(found.is_some_and(|f| same_file(&f, &real)));
        assert_eq!(find_real(&path_of(&[&shim, &shim])), None, "only shim folders: nothing");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_first_real_folder_wins_when_folders_repeat() {
        let root = scratch("first");
        let one = real_in(&root.join("one"));
        real_in(&root.join("two"));
        let found = find_real(&path_of(&[&root.join("one"), &root.join("two"), &root.join("one")]));
        assert!(found.is_some_and(|f| same_file(&f, &one)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_empty_path_or_only_separators_find_nothing() {
        assert_eq!(find_real(OsStr::new("")), None);
        assert_eq!(find_real(&OsString::from(SEP.repeat(3))), None);
    }

    #[test]
    fn a_folder_named_claude_is_not_the_program() {
        let root = scratch("dirname");
        let decoy = root.join("decoy");
        for name in ["claude", "claude.cmd", "claude.exe"] {
            std::fs::create_dir_all(decoy.join(name)).unwrap();
        }
        let real = real_in(&root.join("real"));
        let found = find_real(&path_of(&[&decoy, &root.join("real")]));
        assert!(found.is_some_and(|f| same_file(&f, &real)));
        assert_eq!(find_real(&path_of(&[&decoy])), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_without_the_execute_bit_is_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("noexec");
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("claude"), "").unwrap();
        std::fs::set_permissions(plain.join("claude"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let real = real_in(&root.join("real"));
        assert!(find_real(&path_of(&[&plain, &root.join("real")])).is_some_and(|f| same_file(&f, &real)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_name_check_is_exact_on_the_stem() {
        for no in ["", "claude-code", "claudex", "xclaude", ".claude", "claude.exe.old-12", "/claude/h0x-core"] {
            assert!(!is_claude(OsStr::new(no)), "{no:?}");
        }
        assert!(is_claude(OsStr::new("claude.exe")));
        assert!(is_claude(OsStr::new("./claude")));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_name_check_ignores_case_and_the_extension() {
        for yes in
            [r"C:\Users\me\.state\bin\CLAUDE.EXE", "Claude.exe", "claude.cmd", r"\\?\G:\x y\claude.exe", "cLaUdE"]
        {
            assert!(is_claude(OsStr::new(yes)), "{yes:?}");
        }
        assert!(!is_claude(OsStr::new(r"C:\claude\h0x-core.exe")));
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_the_name_check_is_case_sensitive() {
        assert!(!is_claude(OsStr::new("Claude")));
        assert!(!is_claude(OsStr::new("/usr/bin/CLAUDE")));
    }

    #[test]
    fn an_empty_inline_settings_value_still_counts_as_the_persons() {
        assert!(has_settings(&["--settings="]));
        assert!(has_settings(&["x", "--settings"]), "the last word, no value");
        assert!(!has_settings(&["-settings", "x"]));
        assert!(!has_settings(&["--Settings", "x"]), "Claude's flags are case-sensitive");
        assert!(!has_settings(&["--"]));
    }

    #[test]
    fn a_missing_path_key_takes_the_process_path_after_the_shim_folder() {
        let mut env = BTreeMap::new();
        prepend_path(&mut env, Path::new("bin"));
        let got: Vec<PathBuf> = std::env::split_paths(&env["PATH"]).collect();
        let process: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        assert_eq!(got[0], Path::new("bin"));
        assert_eq!(got[1..], process[..]);
    }

    #[test]
    fn the_shim_folder_is_on_path_once_even_when_it_is_there_already() {
        let bin = Path::new("bin");
        let mut env = BTreeMap::from([("PATH".to_owned(), format!("bin{SEP}x{SEP}bin"))]);
        prepend_path(&mut env, bin);
        prepend_path(&mut env, bin); // a restarted terminal, or a core started inside a terminal
        let count = std::env::split_paths(&env["PATH"]).filter(|d| d == bin).count();
        assert_eq!(count, 1, "{}", env["PATH"]);
        assert_eq!(std::env::split_paths(&env["PATH"]).next().unwrap(), bin);
    }

    #[test]
    fn the_depth_counts_up_and_stops_at_eight() {
        let next = |v: &str| next_depth(Some(OsStr::new(v)));
        assert_eq!(next_depth(None), Some(1));
        assert_eq!(next("1"), Some(2), "a nested legitimate launch");
        assert_eq!(next("7"), Some(8));
        assert_eq!(next("8"), None);
        assert_eq!(next("400"), None);
        for junk in ["", "x", "-1", "1.5"] {
            assert_eq!(next(junk), Some(1), "{junk:?} counts as 0");
        }
    }

    #[test]
    fn another_cores_shim_folder_leaves_the_path_and_ours_is_first() {
        let root = std::env::temp_dir().join(format!("h0x-shim-foreign-{}", std::process::id()));
        let (foreign, plain) = (root.join("other"), root.join("plain"));
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join(MARKER), "").unwrap();
        let path = std::env::join_paths([&foreign, &plain]).unwrap().to_string_lossy().into_owned();
        let mut env = BTreeMap::from([("PATH".to_owned(), path)]);
        prepend_path(&mut env, Path::new("mine"));
        let got: Vec<PathBuf> = std::env::split_paths(&env["PATH"]).collect();
        assert_eq!(got, [PathBuf::from("mine"), plain]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_empty_path_does_not_gain_an_empty_entry() {
        // An empty PATH entry means the current folder to a unix shell.
        let mut env = BTreeMap::from([("PATH".to_owned(), String::new())]);
        prepend_path(&mut env, Path::new("bin"));
        assert!(std::env::split_paths(&env["PATH"]).all(|d| !d.as_os_str().is_empty()), "{:?}", env["PATH"]);
        assert_eq!(std::env::split_paths(&env["PATH"]).next().unwrap(), Path::new("bin"));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_path_key_is_found_in_any_case_and_kept() {
        for key in ["Path", "path", "PATH", "pAtH"] {
            let mut env =
                BTreeMap::from([(key.to_owned(), r"C:\a;C:\b c".to_owned()), ("X".to_owned(), "1".to_owned())]);
            prepend_path(&mut env, Path::new(r"G:\state\bin"));
            let keys: Vec<&str> = env.keys().map(String::as_str).filter(|k| k.eq_ignore_ascii_case("PATH")).collect();
            assert_eq!(keys, [key], "one key, the spelling kept");
            assert_eq!(env[key], r"G:\state\bin;C:\a;C:\b c");
        }
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_shim_folder_already_on_path_in_another_case_is_not_added_again() {
        let mut env = BTreeMap::from([("Path".to_owned(), r"g:\STATE\bin;C:\a".to_owned())]);
        prepend_path(&mut env, Path::new(r"G:\state\bin"));
        let n =
            std::env::split_paths(&env["Path"]).filter(|d| d.as_os_str().eq_ignore_ascii_case(r"G:\state\bin")).count();
        assert_eq!(n, 1, "{}", env["Path"]);
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_shim_folder_that_cannot_go_on_path_leaves_it_untouched() {
        // `"` can't be in a Windows PATH entry: join_paths refuses, and the old PATH stays.
        let mut env = BTreeMap::from([("Path".to_owned(), r"C:\a".to_owned())]);
        prepend_path(&mut env, Path::new("G:\\we\"ird\\bin"));
        assert_eq!(env["Path"], r"C:\a");
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_only_the_exact_path_key_counts() {
        let mut env = BTreeMap::from([("Path".to_owned(), "/x".to_owned())]);
        prepend_path(&mut env, Path::new("/bin-dir"));
        assert_eq!(env["Path"], "/x", "another variable on unix");
        assert_eq!(std::env::split_paths(&env["PATH"]).next().unwrap(), Path::new("/bin-dir"));
    }

    #[test]
    fn ensure_makes_the_folder_the_marker_and_the_link_and_a_second_run_changes_nothing() {
        let state = scratch("ensure");
        ensure(&paths(&state)).unwrap();
        let bin = state.join("bin");
        assert!(bin.join(MARKER).is_file());
        let exe = crate::client::own_exe().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::read_link(bin.join("claude")).unwrap(), exe);
            assert_eq!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777, 0o700);
            ensure(&paths(&state)).unwrap();
            assert_eq!(std::fs::read_link(bin.join("claude")).unwrap(), exe);
        }
        #[cfg(windows)]
        {
            let shim = bin.join("claude.exe");
            let id = |p: &Path| crate::platform::file_identity(&std::fs::metadata(p).unwrap());
            // A hardlink when the state folder shares the exe's volume; a copy (same bytes) across
            // volumes, as on CI where the temp folder and the build folder are on different drives.
            let same = |a: &Path, b: &Path| id(a) == id(b) || std::fs::read(a).unwrap() == std::fs::read(b).unwrap();
            assert!(same(&shim, &exe), "claude.exe is the running program (a hardlink or a same-bytes copy)");
            let before = id(&shim);
            ensure(&paths(&state)).unwrap();
            assert_eq!(id(&shim), before, "a second run leaves an up-to-date claude.exe alone");
            assert_eq!(old_ones(&bin), Vec::<String>::new(), "nothing renamed when nothing changed");
        }
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn ensure_keeps_an_existing_marker_and_fails_when_bin_is_a_file() {
        let state = scratch("marker");
        std::fs::create_dir_all(state.join("bin")).unwrap();
        std::fs::write(state.join("bin").join(MARKER), "keep").unwrap();
        ensure(&paths(&state)).unwrap();
        assert_eq!(std::fs::read_to_string(state.join("bin").join(MARKER)).unwrap(), "keep");

        let other = scratch("binfile");
        std::fs::write(other.join("bin"), "not a folder").unwrap();
        assert!(ensure(&paths(&other)).is_err(), "a file where the folder goes is an error, not a panic");
        let _ = std::fs::remove_dir_all(state);
        let _ = std::fs::remove_dir_all(other);
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_a_link_elsewhere_a_plain_file_and_a_leftover_tmp_are_all_replaced() {
        let bin = scratch("relink");
        let exe = crate::client::own_exe().unwrap();
        std::os::unix::fs::symlink("/bin/false", bin.join("claude")).unwrap();
        std::fs::write(bin.join(".claude.tmp"), "left by a crash").unwrap();
        link(&bin, &exe).unwrap();
        assert_eq!(std::fs::read_link(bin.join("claude")).unwrap(), exe);
        assert!(!bin.join(".claude.tmp").exists());

        std::fs::remove_file(bin.join("claude")).unwrap();
        std::fs::write(bin.join("claude"), "a plain file").unwrap();
        link(&bin, &exe).unwrap();
        assert_eq!(std::fs::read_link(bin.join("claude")).unwrap(), exe);

        std::fs::remove_file(bin.join("claude")).unwrap();
        std::os::unix::fs::symlink(bin.join("gone"), bin.join("claude")).unwrap(); // dangling
        link(&bin, &exe).unwrap();
        assert_eq!(std::fs::read_link(bin.join("claude")).unwrap(), exe);
        let _ = std::fs::remove_dir_all(bin);
    }

    /// The `claude.exe.old-*` names in `bin`, sorted.
    #[cfg(windows)]
    fn old_ones(bin: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(bin)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("claude.exe.old-"))
            .collect();
        names.sort();
        names
    }

    /// A copy of cmd.exe at `to` (same volume as the shim folder, so it can be hardlinked).
    #[cfg(windows)]
    fn cmd_copy(to: &Path) {
        let cmd = Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("cmd.exe");
        std::fs::copy(cmd, to).unwrap();
    }

    /// `program` running, waiting on its stdin, with no window.
    #[cfg(windows)]
    fn running(program: &Path) -> std::process::Child {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(program)
            .args(["/d", "/q", "/k"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_different_claude_exe_is_replaced_by_a_link_to_the_new_build() {
        let bin = scratch("refresh");
        let exe = bin.join("..").join(format!("new-{}.exe", crate::platform::random_hex()));
        std::fs::write(&exe, b"the new build").unwrap();
        std::fs::write(bin.join("claude.exe"), b"an older build").unwrap();
        link(&bin, &exe).unwrap();
        assert_eq!(std::fs::read(bin.join("claude.exe")).unwrap(), b"the new build");
        assert_eq!(old_ones(&bin), Vec::<String>::new(), "an idle old one is removed at once");
        let _ = std::fs::remove_file(exe);
        let _ = std::fs::remove_dir_all(bin);
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_copy_of_the_same_build_is_left_alone() {
        let root = scratch("samecopy");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = root.join("build.exe");
        std::fs::write(&exe, b"one build").unwrap();
        std::fs::copy(&exe, bin.join("claude.exe")).unwrap(); // same size and write time, another file
        let before = crate::platform::file_identity(&std::fs::metadata(bin.join("claude.exe")).unwrap());
        link(&bin, &exe).unwrap();
        let after = crate::platform::file_identity(&std::fs::metadata(bin.join("claude.exe")).unwrap());
        assert_eq!(before, after, "not relinked");
        assert_eq!(old_ones(&bin), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(root);
    }

    /// The shim still running from the old build: renamed to `.old-<pid>-<time>` (it can't be deleted),
    /// a fresh link takes its name, and the next start sweeps the renamed one once it has ended.
    #[cfg(windows)]
    #[test]
    fn on_windows_a_running_shim_is_renamed_away_and_swept_at_the_next_start() {
        let root = scratch("running");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let old_build = root.join("old.exe");
        cmd_copy(&old_build);
        link(&bin, &old_build).unwrap();
        let shim = bin.join("claude.exe");
        let mut child = running(&shim);

        let new_build = root.join("new.exe");
        std::fs::write(&new_build, b"the new build").unwrap();
        // Unrelated names, and one that only looks close, are never swept.
        std::fs::write(bin.join("claude.exe.older"), "").unwrap();
        std::fs::write(bin.join("notes.txt"), "").unwrap();
        link(&bin, &new_build).unwrap();
        let renamed = old_ones(&bin);
        // Recent Windows lets std delete a running exe's name at once; older ones keep the renamed file.
        assert!(renamed.len() <= 1, "{renamed:?}");
        assert!(
            renamed.iter().all(|n| n.starts_with(&format!("claude.exe.old-{}-", std::process::id()))),
            "{renamed:?}"
        );
        assert_eq!(std::fs::read(&shim).unwrap(), b"the new build");
        assert!(child.try_wait().unwrap().is_none(), "the running shim was not disturbed");

        // While it still runs, a second start neither fails nor removes it.
        link(&bin, &new_build).unwrap();
        assert_eq!(old_ones(&bin), renamed);

        child.kill().unwrap();
        child.wait().unwrap();
        link(&bin, &new_build).unwrap();
        assert_eq!(old_ones(&bin), Vec::<String>::new(), "swept once it ended");
        assert!(bin.join("claude.exe.older").exists() && bin.join("notes.txt").exists());
        assert_eq!(std::fs::read(&shim).unwrap(), b"the new build");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A renamed shim of another core run (another pid) that still runs is left for later, and
    /// does not stop this start.
    #[cfg(windows)]
    #[test]
    fn on_windows_a_stale_old_shim_that_still_runs_does_not_fail_the_start() {
        let root = scratch("stale");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stale = bin.join("claude.exe.old-1");
        cmd_copy(&stale);
        let mut child = running(&stale);
        std::fs::write(bin.join("claude.exe.old-2"), "ended").unwrap();
        let build = root.join("build.exe");
        std::fs::write(&build, b"build").unwrap();
        link(&bin, &build).unwrap();
        assert_eq!(old_ones(&bin), ["claude.exe.old-1"], "the ended one is swept, the running one kept");
        child.kill().unwrap();
        child.wait().unwrap();
        link(&bin, &build).unwrap();
        assert_eq!(old_ones(&bin), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(root);
    }
}
