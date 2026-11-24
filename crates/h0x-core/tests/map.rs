//! `h0x-core map callers|grep`: the built binary, an isolated home, a generated source tree. The map
//! answers only inside a HyperOx terminal (SC-2), so the tests that expect an answer run it in a
//! terminal of their own core (`sh` on unix, `cmd` on Windows via `tests/inside`); the ones outside
//! one expect exit 2 and the message.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]
#[cfg(unix)]
mod common;
#[cfg(windows)]
mod inside;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A short folder of our own for one test.
fn scratch(name: &str) -> PathBuf {
    let base = if cfg!(unix) { "/var/tmp".into() } else { PathBuf::from(env!("CARGO_TARGET_TMPDIR")) };
    let root = base.join(format!("h0x-map-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn put(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// `h0x-core map <args>` inside a terminal of an isolated core; `cwd` is where it runs.
fn map(_name: &str, cwd: &Path, args: &[&str]) -> Output {
    map_with(cwd, args, &[])
}

#[cfg(unix)]
fn map_with(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let s = common::Session::new();
    common::in_terminal(&s, &[&["map"], args].concat(), b"", Some(cwd), env)
}

/// In a `cmd` terminal: an empty value in `env` unsets the variable there.
#[cfg(windows)]
fn map_with(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    inside::Session::new().in_terminal(&[&["map"], args].concat(), b"", Some(cwd), env)
}

/// `h0x-core map <args>` outside any terminal, in an isolated home and session.
fn map_outside(name: &str, cwd: &Path, args: &[&str]) -> Output {
    let home = scratch(&format!("{name}-home"));
    Command::new(env!("CARGO_BIN_EXE_h0x-core"))
        .arg("map")
        .args(args)
        .current_dir(cwd)
        .env("H0X_CORE_SESSION", format!("map-{name}"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env_remove("H0X_TERMINAL_ID")
        .env_remove("H0X_TERMINAL_SOCKET")
        .env_remove("H0X_SCOPE")
        .output()
        .unwrap()
}

#[test]
fn outside_a_terminal_the_map_is_exit_2_with_one_line() {
    let root = scratch("outside");
    put(&root, "a.rs", "fn f() { needle(); }\n");
    for args in [&["grep", "needle"][..], &["callers", "needle"], &["grep", "needle", "--dir", "."]] {
        let out = map_outside("outside", &root, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim_end(), "map works inside HyperOx terminals", "{args:?}");
    }
}

#[test]
fn inside_a_terminal_the_map_answers() {
    let root = scratch("inside");
    put(&root, "a.rs", "fn f() { needle(); }\n");
    assert_eq!(ok(&map("inside", &root, &["grep", "needle"])), "a.rs:1  fn f() { needle(); }\n");
}

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("panicked"));
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn callers_and_grep_in_the_current_folder() {
    let root = scratch("basic");
    put(&root, "src/a.rs", "pub fn forward() {}\nfn handle() {\n    forward();\n}\n");
    put(&root, "web/b.ts", "export function boot() {\n  forward();\n}\n");
    put(&root, "tool/c.py", "def go():\n    forward()\n");
    assert_eq!(
        ok(&map("basic", &root, &["callers", "forward"])),
        "src/a.rs:3  handle  forward();\ntool/c.py:2  go  forward()\nweb/b.ts:2  boot  forward();\n"
    );
    assert_eq!(
        ok(&map("basic", &root, &["grep", "forward"])),
        "src/a.rs:1  pub fn forward() {}\nsrc/a.rs:3  forward();\ntool/c.py:2  forward()\nweb/b.ts:2  forward();\n"
    );
}

#[test]
fn dir_flag_and_paths_relative_to_it() {
    let root = scratch("dirflag");
    put(&root, "x/y/a.rs", "fn f() { needle(); }\n");
    let out =
        map("dirflag", &scratch("dirflag-elsewhere"), &["grep", "needle", "--dir", root.join("x").to_str().unwrap()]);
    assert_eq!(ok(&out), "y/a.rs:1  fn f() { needle(); }\n");
}

#[cfg(unix)]
#[test]
fn missing_dir_is_exit_2_with_one_line() {
    let root = scratch("missing");
    let out = map("missing", &root, &["callers", "x", "--dir", root.join("nope").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.contains("no such directory"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_file_as_the_dir_and_an_empty_word_are_exit_2_with_one_line() {
    let root = scratch("reject");
    put(&root, "f.rs", "x\n");
    let rejected = |out: &Output| {
        assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty());
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(err.lines().count(), 1, "{err}");
    };
    rejected(&map("reject", &root, &["grep", "x", "--dir", root.join("f.rs").to_str().unwrap()]));
    rejected(&map("reject", &root, &["callers", ""]));
    rejected(&map("reject", &root, &["grep", "  "]));
}

#[cfg(unix)]
#[test]
fn an_invalid_session_name_does_not_stop_a_map() {
    // The map needs no session of its own: it asks the terminal's core by the terminal's socket.
    let root = scratch("badsession");
    put(&root, "a.rs", "fn f() { needle(); }\n");
    let out = map_with(&root, &["grep", "needle"], &[("H0X_CORE_SESSION", "a/b:c")]);
    assert_eq!(ok(&out), "a.rs:1  fn f() { needle(); }\n");
}

#[cfg(windows)]
#[test]
fn windows_dir_with_backslashes_spaces_and_a_trailing_backslash() {
    // No terminal shell on Windows: this runs the search the command runs, on the same paths.
    use h0x_core::map::{Mode, search};
    let root = scratch("winpaths");
    put(&root, "with space/sub dir/a.rs", "fn f() { needle(); }\n");
    let full = std::fs::canonicalize(root.join("with space")).unwrap().to_string_lossy().into_owned();
    let plain = full.trim_start_matches(r"\\?\").to_string();
    let want = "sub dir/a.rs:1  fn f() { needle(); }\n";
    for dir in [format!(r"{plain}\"), plain, full] {
        assert_eq!(search(Mode::Grep, "needle", Path::new(&dir)).unwrap(), want, "{dir}");
    }
    // Relative, from the folder above, with backslashes.
    assert_eq!(search(Mode::Grep, "needle", &root.join(r"with space\")).unwrap(), want);
    assert_eq!(
        search(Mode::Grep, "needle", &root.join(r".\with space\sub dir")).unwrap(),
        "a.rs:1  fn f() { needle(); }\n"
    );
}

#[cfg(unix)]
#[test]
fn no_match_and_an_empty_dir_are_exit_0() {
    let root = scratch("nomatch");
    assert_eq!(ok(&map("nomatch", &root, &["callers", "x"])), "no matches\n");
    put(&root, "a.rs", "fn a() {}\n");
    assert_eq!(ok(&map("nomatch", &root, &["grep", "zzz"])), "no matches\n");
}

#[cfg(unix)]
#[test]
fn crlf_and_non_utf8_files() {
    let root = scratch("crlf");
    put(&root, "w.rs", "fn a() {\r\n    go();\r\n}\r\n");
    put(&root, "bin.rs", [0xff, 0xfe, 0x00, b'g', b'o', b'(', b')', b'\n']);
    assert_eq!(ok(&map("crlf", &root, &["callers", "go"])), "w.rs:2  a  go();\n");
}

#[cfg(unix)]
#[test]
fn the_cap_is_8192_bytes_with_a_footer() {
    let root = scratch("cap");
    for i in 0..600 {
        put(&root, &format!("d{i:03}/m.rs"), "fn a() {\n    go();\n}\n");
    }
    let text = ok(&map("cap", &root, &["callers", "go"]));
    assert!(text.len() <= 8192, "{}", text.len());
    let last = text.lines().last().unwrap();
    assert!(last.starts_with("[… ") && last.ends_with("; narrow with --dir]"), "{last}");
}

#[cfg(unix)]
#[test]
fn a_symlink_loop_does_not_hang() {
    let root = scratch("loop");
    put(&root, "a/x.rs", "needle\n");
    std::os::unix::fs::symlink(&root, root.join("a").join("loop")).unwrap();
    assert_eq!(ok(&map("loop", &root, &["grep", "needle"])), "a/x.rs:1  needle\n");
}

#[cfg(unix)]
#[test]
fn two_thousand_files_answer_quickly() {
    let root = scratch("perf");
    for i in 0..2000 {
        let body: String = (0..40).map(|l| format!("fn f{l}() {{ let v = value{i}(); }}\n")).collect();
        put(&root, &format!("m{}/f{i}.rs", i % 40), body);
    }
    let started = std::time::Instant::now();
    let text = ok(&map("perf", &root, &["callers", "value7"]));
    assert!(text.contains("value7"));
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn inside_a_terminal_a_windows_style_dir_with_spaces_is_searched() {
    let root = scratch("inside spaces");
    put(&root, "with space/sub dir/a.rs", "fn f() { needle(); }\n");
    let dir = root.join("with space").display().to_string();
    let dir = if cfg!(windows) { format!("{dir}\\") } else { dir };
    let out = map("inside spaces", &root, &["grep", "needle", "--dir", &dir]);
    assert_eq!(ok(&out), "sub dir/a.rs:1  fn f() { needle(); }\n");
}
