//! worktree.create -> list -> merge -> remove over RPC, against the real `h0x-wt`, through an
//! isolated core installed the way a release is (`h0x-core-tools/h0x-wt` beside `h0x-core`).
//! `H0X_WT_BIN` names the built tool; CI always sets it, and a missing one fails there.

#![cfg(unix)]

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

/// A session whose core sits in an install folder with the tool beside it; None when no tool is
/// given (a local run without one). In CI that is a failure, never a skip.
fn installed() -> Option<Session> {
    let Some(tool) = std::env::var_os("H0X_WT_BIN") else {
        assert!(std::env::var_os("CI").is_none(), "H0X_WT_BIN must name the built h0x-wt in CI");
        eprintln!("skipped: H0X_WT_BIN is not set");
        return None;
    };
    assert!(Path::new(&tool).is_file(), "H0X_WT_BIN is not a file: {tool:?}");
    let mut s = Session::new();
    let install = s.root.join("install dir");
    std::fs::create_dir_all(install.join("h0x-core-tools")).unwrap();
    // A copy, not a link: the core finds the tool beside its real path.
    std::fs::copy(BIN, install.join("h0x-core")).unwrap();
    std::fs::copy(&tool, install.join("h0x-core-tools/h0x-wt")).unwrap();
    s.bin = install.join("h0x-core");
    // No global or system git config; the commits get their identity from each repository.
    s.env.push(("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()));
    s.env.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    Some(s)
}

fn git(s: &Session, dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).env("HOME", &s.root).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1");
    for (k, _) in std::env::vars() {
        if k.starts_with("GIT_") && !k.starts_with("GIT_CONFIG") {
            cmd.env_remove(k);
        }
    }
    let out = cmd.args(["-c", "commit.gpgsign=false"]).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(s: &Session, dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    git(s, dir, &["add", "."]);
    git(s, dir, &["commit", "-q", "-m", file]);
}

fn repo(s: &Session, name: &str) -> std::path::PathBuf {
    let dir = s.root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(s, &dir, &["init", "-q", "-b", "main"]);
    git(s, &dir, &["config", "user.name", "t"]);
    git(s, &dir, &["config", "user.email", "t@example.com"]);
    commit(s, &dir, "a.txt", "one\n");
    dir
}

fn ok(r: &Value) -> &Value {
    assert!(r.get("error").is_none(), "{r}");
    &r["result"]
}

fn kind(r: &Value) -> &str {
    r["error"]["data"]["kind"].as_str().unwrap_or("")
}

#[test]
fn create_list_merge_and_remove_through_the_core() {
    let Some(s) = installed() else { return };
    let main = repo(&s, "proj é");
    let folder = main.display().to_string();
    s.json(&["server", "status"]); // starts the core
    let mut c = s.connect();

    // create: a new branch under the home's worktrees folder.
    let r = c.call(1, "worktree.create", json!({"branch": "feat/x", "folder": folder}));
    let x = ok(&r).clone();
    let x_path = x["path"].as_str().unwrap().to_string();
    assert_eq!(x["new_branch"], true, "{r}");
    assert!(x_path.ends_with("/.h0x/worktrees/proj é/feat-x"), "{x_path}");
    // The branch already has a worktree.
    let r = c.call(2, "worktree.create", json!({"branch": "feat/x", "folder": folder}));
    assert_eq!(kind(&r), "checked_out", "{r}");

    // list: the main checkout first, then the new one.
    let r = c.call(3, "worktree.list", json!({"folder": folder}));
    let l = ok(&r);
    assert_eq!(l["default_branch"], "main", "{r}");
    let paths: Vec<&str> = l["worktrees"].as_array().unwrap().iter().map(|w| w["path"].as_str().unwrap()).collect();
    assert_eq!(l["worktrees"][0]["main"], true, "{r}");
    assert!(paths.contains(&x_path.as_str()), "{paths:?}");

    // dirty: remove is refused unless forced; the main checkout is never removed.
    commit(&s, Path::new(&x_path), "b.txt", "two\n");
    std::fs::write(Path::new(&x_path).join("scratch.txt"), "x").unwrap();
    git(&s, Path::new(&x_path), &["add", "scratch.txt"]);
    let r = c.call(4, "worktree.remove", json!({"path": x_path}));
    assert_eq!(kind(&r), "dirty", "{r}");
    assert!(Path::new(&x_path).is_dir());
    let r = c.call(5, "worktree.remove", json!({"path": folder, "force": true}));
    assert_eq!(kind(&r), "main_checkout", "{r}");
    assert!(main.join("a.txt").is_file());
    git(&s, Path::new(&x_path), &["reset", "-q", "scratch.txt"]);
    std::fs::remove_file(Path::new(&x_path).join("scratch.txt")).unwrap();

    // merge (squash): the work lands on main, the worktree and branch are gone.
    let r = c.call(6, "worktree.merge", json!({"path": x_path}));
    let m = ok(&r);
    assert_eq!((m["target"].as_str(), m["merged"].clone()), (Some("main"), json!(true)), "{r}");
    assert!(main.join("b.txt").is_file());
    assert!(!Path::new(&x_path).exists());
    assert!(!git(&s, &main, &["branch", "--list", "feat/x"]).contains("feat/x"));

    // remove: a clean worktree whose work is not in main keeps its branch.
    let r = c.call(7, "worktree.create", json!({"branch": "feat/y", "folder": folder}));
    let y_path = ok(&r)["path"].as_str().unwrap().to_string();
    commit(&s, Path::new(&y_path), "c.txt", "three\n");
    let r = c.call(8, "worktree.remove", json!({"path": y_path}));
    let removed = ok(&r);
    assert_eq!(removed["branch_deleted"], false, "{r}");
    assert!(!Path::new(&y_path).exists());
    assert!(git(&s, &main, &["branch", "--list", "feat/y"]).contains("feat/y"));
}

#[test]
fn a_conflicting_merge_changes_nothing_and_names_the_files() {
    let Some(s) = installed() else { return };
    let main = repo(&s, "conflict");
    let folder = main.display().to_string();
    s.json(&["server", "status"]); // starts the core
    let mut c = s.connect();
    let r = c.call(1, "worktree.create", json!({"branch": "feat/c", "folder": folder}));
    let path = ok(&r)["path"].as_str().unwrap().to_string();
    commit(&s, Path::new(&path), "a.txt", "theirs\n");
    commit(&s, &main, "a.txt", "ours\n");
    let (main_tip, branch_tip) = (git(&s, &main, &["rev-parse", "main"]), git(&s, &main, &["rev-parse", "feat/c"]));

    let r = c.call(2, "worktree.merge", json!({"path": path}));
    assert_eq!(kind(&r), "conflict", "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("a.txt"), "{r}");
    assert_eq!(git(&s, &main, &["rev-parse", "main"]), main_tip);
    assert_eq!(git(&s, &main, &["rev-parse", "feat/c"]), branch_tip);
    assert!(Path::new(&path).is_dir());
    assert_eq!(git(&s, Path::new(&path), &["status", "--porcelain"]), "");
}
