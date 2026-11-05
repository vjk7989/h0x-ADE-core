//! session list: the agent sessions that ran in a folder, here or on a machine.

#![cfg(unix)]

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::Path;

/// A Claude Code session in `home`'s store, as Claude writes it.
fn claude_session(home: &Path, folder: &str, id: &str, title: &str) {
    let dir = home.join(".claude/projects").join(folder.replace(|c: char| !c.is_ascii_alphanumeric(), "-"));
    std::fs::create_dir_all(&dir).unwrap();
    let lines = [
        json!({"type": "user", "cwd": folder, "sessionId": id, "message": {"role": "user", "content": "hi"}}),
        json!({"type": "ai-title", "aiTitle": title, "sessionId": id}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
}

fn ids(list: &Value) -> Vec<(&str, &str, &str)> {
    let sessions = list["sessions"].as_array().unwrap();
    sessions
        .iter()
        .map(|s| (s["agent"].as_str().unwrap(), s["id"].as_str().unwrap(), s["title"].as_str().unwrap_or("")))
        .collect()
}

#[test]
fn sessions_are_listed_here_and_on_a_machine() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);

    let here = std::fs::canonicalize(&s.root).unwrap().join("my app");
    std::fs::create_dir_all(&here).unwrap();
    let here = here.display().to_string();
    claude_session(&s.root, &here, "s-here", "Local work");
    let listed = s.json(&["session", "list", &here]);
    assert_eq!(ids(&listed), [("claude", "s-here", "Local work")]);
    let path = listed["sessions"][0]["path"].as_str().unwrap();
    assert!(path.starts_with(&s.root.join(".claude/projects").display().to_string()), "{path}");
    assert!(listed["sessions"][0]["last_used"].as_u64().unwrap() > 0);

    // No folder: where h0x-core runs.
    let out = s.cmd().current_dir(&here).args(["--json", "session", "list"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), listed);
    assert!(ids(&s.json(&["session", "list", "/nowhere"])).is_empty());

    // On a machine: its stores, and relative folders from its home.
    let there = rohan.root.join("srv").display().to_string();
    claude_session(&rohan.root, &there, "s-there", "Remote work");
    for folder in [there.as_str(), "srv"] {
        assert_eq!(ids(&s.json(&["session", "list", "--on", "rohan", folder])), [("claude", "s-there", "Remote work")]);
    }
    assert!(ids(&s.json(&["session", "list", "--on", "rohan", &here])).is_empty(), "not this machine's");
    let (_, e) = s.err(&["session", "list", "--on", "nope", "/srv"]);
    assert_eq!(e["error"]["kind"], "machine_unknown", "{e}");

    let mut conn = s.connect();
    let empty = conn.call(1, "session.list", json!({"folder": ""}));
    assert_eq!(empty["error"]["data"]["kind"], "invalid_params", "{empty}");
}
