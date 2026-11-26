//! worktree list|create|remove|merge through the CLI and the API of a core with no h0x-wt beside
//! it: typed errors, params validation, folders, and machine routing.

#![cfg(unix)]

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::Path;

const HINT: &str = "Update h0x-core to use Worktrees";

/// These tests need a test core with no `h0x-core-tools/` beside it.
fn bare_core() -> bool {
    let bare = !Path::new(BIN).parent().unwrap().join("h0x-core-tools").exists();
    if !bare {
        eprintln!("skipped: h0x-core-tools is beside the test core");
    }
    bare
}

fn kind(r: &Value) -> &str {
    r["error"]["data"]["kind"].as_str().unwrap_or("")
}

#[test]
fn without_h0x_wt_every_verb_is_missing_tool_with_the_update_hint() {
    if !bare_core() {
        return;
    }
    let s = Session::new();
    let repo = s.root.join("my repo é");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.display().to_string();
    let install = Path::new(BIN).parent().unwrap().display().to_string();
    for args in [
        vec!["worktree", "list", repo.as_str()],
        vec!["worktree", "create", "feat/x", "--folder", repo.as_str()],
        vec!["worktree", "remove", repo.as_str()],
        vec!["worktree", "remove", "--force", repo.as_str()],
        vec!["worktree", "merge", repo.as_str()],
        vec!["worktree", "merge", "--no-squash", "--target", "dev", repo.as_str()],
    ] {
        let (code, e) = s.err(&args);
        assert_ne!(code, 0, "{args:?}");
        assert_eq!(e["error"]["kind"], "missing_tool", "{args:?}: {e}");
        assert_eq!(e["error"]["hint"], HINT, "{args:?}: {e}");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(message.contains("h0x-wt") && !message.contains(&install), "{message}");
    }
    s.json(&["server", "status"]); // starts the core
    let mut conn = s.connect();
    for (n, method, params) in [
        (1, "worktree.list", json!({"folder": repo})),
        (2, "worktree.create", json!({"branch": "feat/x", "folder": repo})),
        (3, "worktree.remove", json!({"path": repo, "force": true})),
        (4, "worktree.merge", json!({"path": repo, "squash": false})),
    ] {
        let r = conn.call(n, method, params);
        assert_eq!(kind(&r), "missing_tool", "{method}: {r}");
        assert_eq!(r["error"]["data"]["hint"], HINT, "{method}: {r}");
    }
}

#[test]
fn bad_params_are_invalid_params_before_anything_runs() {
    let s = Session::new();
    let repo = s.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.display().to_string();
    s.json(&["server", "status"]); // starts the core
    let mut conn = s.connect();
    for (n, method, params) in [
        (1, "worktree.list", json!({})),
        (2, "worktree.list", json!({"folder": null})),
        (3, "worktree.list", json!({"folder": 7})),
        (4, "worktree.list", json!({"folder": repo, "on": 7})),
        (5, "worktree.create", json!({"folder": repo})),
        (6, "worktree.create", json!({"branch": "", "folder": repo})),
        (7, "worktree.create", json!({"branch": "  ", "folder": repo})),
        (8, "worktree.create", json!({"branch": "-x", "folder": repo})),
        (9, "worktree.create", json!({"branch": "--upload-pack=x", "folder": repo})),
        (10, "worktree.create", json!({"branch": "ok", "base": "-x", "folder": repo})),
        (11, "worktree.remove", json!({})),
        (12, "worktree.remove", json!({"path": repo, "force": "yes"})),
        (13, "worktree.merge", json!({})),
        (14, "worktree.merge", json!({"path": repo, "squash": "no"})),
        (15, "worktree.merge", json!({"path": repo, "target": 1})),
        // A NUL is in no branch name: refused here, never passed on to the tool.
        (16, "worktree.create", json!({"branch": "a\u{0}b", "folder": repo})),
        // An empty folder or path is a mistake, as in session.list: never "where the core runs".
        (17, "worktree.list", json!({"folder": ""})),
        (18, "worktree.remove", json!({"path": ""})),
        (19, "worktree.merge", json!({"path": ""})),
        (20, "worktree.create", json!({"branch": "x", "folder": ""})),
    ] {
        let r = conn.call(n, method, params.clone());
        assert_eq!(kind(&r), "invalid_params", "{method} {params}: {r}");
    }
}

#[test]
fn folders_that_are_not_there_are_not_git_relative_or_windows_style() {
    let s = Session::new();
    let here = s.root.join("work dir");
    std::fs::create_dir_all(&here).unwrap();
    // The CLI reads a relative folder from where it runs.
    let out = s.cmd().current_dir(&here).args(["worktree", "list", "no such"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let e: Value = serde_json::from_str(stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap()).unwrap();
    assert_eq!(e["error"]["kind"], "not_git", "{e}");
    assert!(e["error"]["message"].as_str().unwrap().contains("work dir/no such"), "{e}");
    if bare_core() {
        // No folder: where the CLI runs, which is a folder: the tool is what is missing.
        let out = s.cmd().current_dir(&here).args(["worktree", "list"]).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("missing_tool"), "{stderr}");
    }
    s.json(&["server", "status"]); // starts the core
    let mut conn = s.connect();
    for (n, folder) in [(1, "/no/such/repo é"), (2, r"C:\Users\dev\my repo"), (3, "C:/Users/dev/my repo")] {
        let r = conn.call(n, "worktree.list", json!({"folder": folder}));
        assert_eq!(kind(&r), "not_git", "{folder}: {r}");
        let r = conn.call(n + 10, "worktree.remove", json!({"path": folder}));
        assert_eq!(kind(&r), "not_git", "{folder}: {r}");
        let r = conn.call(n + 20, "worktree.create", json!({"branch": "x", "folder": folder}));
        assert_eq!(kind(&r), "not_git", "{folder}: {r}");
    }
}

#[test]
fn worktree_calls_go_to_a_machine_and_read_folders_there() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    std::fs::create_dir_all(rohan.root.join("srv")).unwrap();

    let (_, e) = s.err(&["worktree", "list", "--on", "nope", "/srv"]);
    assert_eq!(e["error"]["kind"], "machine_unknown", "{e}");
    // A folder relative to that machine's home: not there is not_git, named as it is there.
    let (_, e) = s.err(&["worktree", "list", "--on", "rohan", "nowhere"]);
    assert_eq!(e["error"]["kind"], "not_git", "{e}");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(message.contains(&rohan.root.join("nowhere").display().to_string()), "{message}");
    // Validation happens there too.
    let (_, e) = s.err(&["worktree", "create", "--on", "rohan", "--folder", "srv", "--", "-x"]);
    assert_eq!(e["error"]["kind"], "invalid_params", "{e}");
    if bare_core() {
        // That machine's own core runs its own h0x-wt: it has none either.
        for args in [
            vec!["worktree", "list", "--on", "rohan", "srv"],
            vec!["worktree", "create", "--on", "rohan", "--folder", "srv", "feat/x"],
            vec!["worktree", "remove", "--on", "rohan", "srv"],
            vec!["worktree", "merge", "--on", "rohan", "srv"],
        ] {
            let (_, e) = s.err(&args);
            assert_eq!(e["error"]["kind"], "missing_tool", "{args:?}: {e}");
            assert_eq!(e["error"]["hint"], HINT, "{args:?}: {e}");
        }
    }
    s.json(&["server", "status"]); // starts the core
    let mut conn = s.connect();
    let r = conn.call(1, "worktree.merge", json!({"path": "/srv", "on": "nope"}));
    assert_eq!(kind(&r), "machine_unknown", "{r}");
}
