//! `h0x-core handoff` as Claude Code runs it for `/clear`: the hook event on stdin, the package on
//! stdout, exit 0, and nothing at all whenever there is nothing useful to say.
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const FIXTURE: &str = include_str!("fixtures/handoff-s4.jsonl");

/// A short folder of our own for one test.
fn scratch(name: &str) -> PathBuf {
    let base = if cfg!(unix) { "/var/tmp".into() } else { PathBuf::from(env!("CARGO_TARGET_TMPDIR")) };
    let root = base.join(format!("h0x-ho-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A transcript file whose modified time is `age` seconds ago.
fn put(dir: &Path, name: &str, body: impl AsRef<[u8]>, age: u64) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(age)).unwrap();
    path
}

/// `h0x-core handoff` with `input` on stdin in an isolated home and session; returns stdout.
fn handoff(name: &str, input: &[u8]) -> String {
    let home = scratch(&format!("{name}-home"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_h0x-core"))
        .arg("handoff")
        .env("H0X_CORE_SESSION", format!("ho-{name}"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let feed = std::thread::spawn(move || drop(stdin.write_all(&input)));
    let out = child.wait_with_output().unwrap();
    feed.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    String::from_utf8(out.stdout).unwrap()
}

fn event(source: &str, current: &Path, id: &str) -> Vec<u8> {
    json!({"session_id": id, "transcript_path": current.to_string_lossy(), "source": source, "cwd": "/w", "hook_event_name": "SessionStart"})
        .to_string()
        .into_bytes()
}

/// The package from a stdout that must be one JSON line of the documented shape.
fn context(out: &str) -> String {
    let line = out.strip_suffix('\n').unwrap_or_else(|| panic!("no newline: {out:?}"));
    assert!(!line.contains('\n'), "{out:?}");
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert_eq!(v.as_object().unwrap().len(), 1);
    let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    assert!(ctx.len() <= 3000, "{}", ctx.len());
    ctx
}

#[test]
fn a_cleared_session_hands_its_work_to_the_next() {
    let dir = scratch("happy");
    put(&dir, "old-session.jsonl", FIXTURE, 100);
    let current = put(&dir, "new-session.jsonl", "", 0);
    let ctx = context(&handoff("happy", &event("clear", &current, "new-session")));
    let at = |s: &str| ctx.find(s).unwrap_or_else(|| panic!("{s} missing in {ctx}"));
    assert!(ctx.starts_with("## Context handoff from the previous session"));
    assert!(at("Objective so far: (8) Earlier, another session") < at("Findings / decisions"));
    assert!(at("Findings / decisions") < at("Files referenced:"));
    assert!(at("Files referenced:") < at("Commands run:"));
    assert!(at("Commands run:") < at("Tests run:"));
    assert!(at("Tests run:") < at("Pending work: continue with the next request below."));
    assert!(ctx.contains("deploy-canary-17"));
    assert!(ctx.contains("deploy-canary-17"));
    let files = ctx.lines().find(|l| l.starts_with("Files referenced: ")).unwrap();
    assert!(files.contains("/work/runs/") && !files.contains('\\'), "paths use forward slashes: {files}");
}

#[test]
fn only_clear_hands_off() {
    let dir = scratch("source");
    put(&dir, "old.jsonl", FIXTURE, 100);
    let current = put(&dir, "new.jsonl", "", 0);
    for source in ["startup", "resume", "compact", "", "CLEAR"] {
        assert_eq!(handoff("source", &event(source, &current, "new")), "", "{source}");
    }
    assert!(!handoff("source", &event("clear", &current, "new")).is_empty());
}

#[test]
fn nothing_to_hand_off_is_silence() {
    let dir = scratch("silent");
    let current = put(&dir, "only.jsonl", FIXTURE, 0);
    // The current transcript is the only one: it is never its own handoff.
    assert_eq!(handoff("silent", &event("clear", &current, "only")), "");
    // The same, found by path when the id does not match the name.
    assert_eq!(handoff("silent", &event("clear", &current, "other-id")), "");
    // A missing folder.
    let ghost = dir.join("no-such-dir").join("x.jsonl");
    assert_eq!(handoff("silent", &event("clear", &ghost, "x")), "");
    // Other files that are not transcripts.
    put(&dir, "notes.txt", FIXTURE, 50);
    std::fs::create_dir(dir.join("dir.jsonl")).unwrap();
    assert_eq!(handoff("silent", &event("clear", &current, "only")), "");
    // A previous transcript with nothing in it.
    put(&dir, "empty.jsonl", "", 10);
    assert_eq!(handoff("silent", &event("clear", &current, "only")), "");
}

#[test]
fn bad_events_are_silence() {
    for input in [
        &b""[..],
        b"not json",
        b"{}",
        b"[]",
        b"null",
        b"{\"source\":\"clear\"}",
        b"{\"source\":\"clear\",\"transcript_path\":5}",
        &[0xff, 0xfe],
    ] {
        assert_eq!(handoff("badev", input), "", "{input:?}");
    }
}

#[test]
fn the_newest_other_transcript_is_the_previous_one() {
    let dir = scratch("newest");
    let older = FIXTURE.replace("deploy-canary-17", "older-run-marker");
    put(&dir, "a.jsonl", &older, 500);
    put(&dir, "b.jsonl", FIXTURE, 100);
    let current = put(&dir, "c.jsonl", "", 0);
    let ctx = context(&handoff("newest", &event("clear", &current, "c")));
    assert!(ctx.contains("deploy-canary-17") && !ctx.contains("older-run-marker"));
    // A transcript newer than the previous one is still not the previous one when it is the current.
    let newer_current = put(&dir, "c.jsonl", FIXTURE.replace("deploy-canary-17", "current-marker"), 0);
    let ctx = context(&handoff("newest", &event("clear", &newer_current, "c")));
    assert!(ctx.contains("deploy-canary-17") && !ctx.contains("current-marker"));
}

#[test]
fn truncated_and_non_utf8_transcripts_do_not_panic() {
    let dir = scratch("broken");
    let current = put(&dir, "cur.jsonl", "", 0);
    // The last line is cut in the middle, one line is not UTF-8: the rest still counts.
    let mut body = FIXTURE.as_bytes().to_vec();
    body.extend_from_slice(b"\xff\xfe\xfd\n{\"type\":\"user\",\"message\":{\"content\":\"cut off in the mid");
    put(&dir, "old.jsonl", &body, 100);
    let ctx = context(&handoff("broken", &event("clear", &current, "cur")));
    assert!(ctx.contains("deploy-canary-17"));
    // Nothing but bytes: silence.
    put(&dir, "old.jsonl", [0xff_u8, 0xfe, 0x00, 0x80, b'\n', 0xc3, 0x28], 100);
    assert_eq!(handoff("broken", &event("clear", &current, "cur")), "");
}

#[test]
fn a_fifty_megabyte_transcript_is_read_from_its_end_and_fast() {
    let dir = scratch("huge");
    let current = put(&dir, "cur.jsonl", "", 0);
    let filler = format!("{}\n", json!({"type": "attachment", "data": "x".repeat(1000)}));
    let mut body = "{\"type\":\"user\",\"message\":{\"content\":\"the request from long ago\"}}\n".to_string();
    body.push_str(&filler.repeat(50 * 1024));
    body.push_str(FIXTURE);
    assert!(body.len() > 50 * 1024 * 1024);
    put(&dir, "old.jsonl", &body, 100);
    let started = std::time::Instant::now();
    let ctx = context(&handoff("huge", &event("clear", &current, "cur")));
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(ctx.contains("deploy-canary-17"));
    assert!(!ctx.contains("long ago"), "only the end of the transcript is read");
}

#[test]
fn the_package_is_at_most_3000_bytes_even_with_wide_characters() {
    let dir = scratch("wide");
    let current = put(&dir, "cur.jsonl", "", 0);
    let mut lines = format!("{}\n", json!({"type": "user", "message": {"content": "go"}}));
    for i in 0..10 {
        let command = format!("test{i}{}", "\u{4e2d}".repeat(100));
        let block = json!([{"type": "tool_use", "name": "Bash", "input": {"command": command}}]);
        lines += &format!("{}\n", json!({"type": "assistant", "message": {"content": block}}));
    }
    put(&dir, "old.jsonl", lines, 100);
    let ctx = context(&handoff("wide", &event("clear", &current, "cur")));
    assert!(ctx.len() > 2996, "cut at the limit, on a character: {}", ctx.len());
}

#[test]
fn a_path_with_spaces_and_the_platform_separators_works() {
    let dir = scratch("paths").join("with space").join("sub dir");
    std::fs::create_dir_all(&dir).unwrap();
    put(&dir, "old.jsonl", FIXTURE, 100);
    let current = put(&dir, "cur.jsonl", "", 0);
    assert!(handoff("paths", &event("clear", &current, "cur")).contains("deploy-canary-17"));
    if cfg!(windows) {
        // Backslashes as Claude Code writes them, and the mixed spelling.
        let native = current.to_string_lossy().replace('/', "\\");
        assert!(native.contains('\\'));
        let ev = json!({"session_id": "cur", "transcript_path": native, "source": "clear"}).to_string();
        assert!(handoff("paths", ev.as_bytes()).contains("deploy-canary-17"));
        let mixed = current.to_string_lossy().replacen('\\', "/", 1).replace("sub dir\\", "sub dir/");
        let ev = json!({"session_id": "cur", "transcript_path": mixed, "source": "clear"}).to_string();
        assert!(handoff("paths", ev.as_bytes()).contains("deploy-canary-17"));
    }
}

#[test]
fn quotes_backslashes_nul_and_crlf_give_one_valid_json_line() {
    let dir = scratch("escape");
    let current = put(&dir, "cur.jsonl", "", 0);
    let request = "say \"hi\" to C:\\tmp\\x and nul\u{0}here\r\nline two\ttab";
    let answer = "answer with \"quotes\" and \\ and\r\nend";
    let lines = [
        json!({"type": "user", "message": {"content": request}}),
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "Read", "input": {"file_path": "C:\\w\\e.rs"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test \"a b\" \\\r\n --x"}},
            {"type": "text", "text": answer},
        ]}}),
    ];
    // CRLF between the transcript lines too.
    put(&dir, "old.jsonl", lines.map(|v| v.to_string()).join("\r\n") + "\r\n", 100);
    let out = handoff("escape", &event("clear", &current, "cur"));
    let line = out.strip_suffix('\n').unwrap();
    assert!(line.chars().all(|c| c >= ' '), "a raw control character in the output: {line:?}");
    let ctx = context(&out);
    assert!(ctx.contains("Objective so far: (1) say \"hi\" to C:\\tmp\\x and nul"), "{ctx:?}");
    assert!(ctx.contains("here line two tab\n"), "{ctx:?}");
    assert!(ctx.contains("- step 1: answer with \"quotes\" and \\ and end\n"), "{ctx:?}");
    assert!(ctx.contains("Files referenced: C:/w/e.rs\n"), "{ctx:?}");
    assert!(ctx.contains("Tests run: `cargo test \"a b\" \\ --x`\n"), "{ctx:?}");
    assert!(!ctx.contains('\r') && !ctx.contains('\t'), "{ctx:?}");
}

#[test]
fn a_transcript_path_that_is_a_folder_or_not_a_transcript() {
    let dir = scratch("shape");
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    put(&sub, "old.jsonl", FIXTURE, 100);
    // A folder: the transcripts inside it are not beside it.
    assert_eq!(handoff("shape", &event("clear", &sub, "sub")), "");
    // A file that is not a transcript: the transcripts in its folder still count.
    let notes = put(&sub, "cur.txt", "", 0);
    assert!(context(&handoff("shape", &event("clear", &notes, "cur"))).contains("deploy-canary-17"));
    // Paths with no folder at all.
    for path in ["", "/"] {
        let ev = json!({"session_id": "x", "transcript_path": path, "source": "clear"}).to_string();
        assert_eq!(handoff("shape", ev.as_bytes()), "", "{path:?}");
    }
}

#[test]
fn an_empty_or_missing_session_id_still_finds_the_previous_transcript() {
    let dir = scratch("noid");
    put(&dir, "old.jsonl", FIXTURE, 100);
    // The current transcript is newer and has its own marker: it is excluded by its path alone.
    let current = put(&dir, "cur.jsonl", FIXTURE.replace("deploy-canary-17", "current-marker"), 0);
    let missing = json!({"transcript_path": current.to_string_lossy(), "source": "clear"}).to_string();
    for input in [event("clear", &current, ""), missing.into_bytes()] {
        let ctx = context(&handoff("noid", &input));
        assert!(ctx.contains("deploy-canary-17") && !ctx.contains("current-marker"), "{ctx}");
    }
}

#[test]
fn transcripts_with_the_same_time_are_chosen_the_same_way_every_time() {
    let when = SystemTime::now() - Duration::from_secs(100);
    let mut picked = Vec::new();
    // The same two files, created in both orders.
    for (k, order) in [["a", "b"], ["b", "a"]].into_iter().enumerate() {
        let dir = scratch(&format!("tie{k}"));
        for name in order {
            let path = dir.join(format!("{name}.jsonl"));
            std::fs::write(&path, FIXTURE.replace("deploy-canary-17", &format!("marker-{name}"))).unwrap();
            std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(when).unwrap();
        }
        let current = put(&dir, "cur.jsonl", "", 0);
        for _ in 0..2 {
            let ctx = context(&handoff(&format!("tie{k}"), &event("clear", &current, "cur")));
            let found: Vec<&str> = ["a", "b"].into_iter().filter(|n| ctx.contains(&format!("marker-{n}"))).collect();
            assert_eq!(found.len(), 1, "{ctx}");
            picked.push(found[0]);
        }
    }
    assert!(picked.iter().all(|p| *p == picked[0]), "{picked:?}");
}

#[test]
fn unknown_fields_are_ignored_and_the_source_must_be_exactly_clear() {
    let dir = scratch("fields");
    put(&dir, "old.jsonl", FIXTURE, 100);
    let current = put(&dir, "cur.jsonl", "", 0);
    let ev = |source: Value| {
        json!({
            "session_id": "cur", "transcript_path": current.to_string_lossy(), "source": source,
            "model": {"id": "m", "display_name": "M"}, "permission_mode": "default",
            "future": [1, null, {"deep": [true, 2.5]}], "hook_event_name": "SessionStart",
        })
        .to_string()
    };
    assert!(context(&handoff("fields", ev(json!("clear")).as_bytes())).contains("deploy-canary-17"));
    for source in [json!("Clear"), json!(" clear"), json!("clear "), json!(["clear"]), json!(true), Value::Null] {
        assert_eq!(handoff("fields", ev(source.clone()).as_bytes()), "", "{source}");
    }
}

#[test]
fn stdin_larger_than_a_megabyte_is_read_whole() {
    let dir = scratch("bigin");
    put(&dir, "old.jsonl", FIXTURE, 100);
    let current = put(&dir, "cur.jsonl", "", 0);
    let mut ev: Value = serde_json::from_slice(&event("clear", &current, "cur")).unwrap();
    ev["padding"] = json!("x".repeat(3 * 1024 * 1024 / 2));
    let input = ev.to_string();
    assert!(input.len() > 1024 * 1024);
    assert!(context(&handoff("bigin", input.as_bytes())).contains("deploy-canary-17"));
    // The same size of garbage is silence.
    assert_eq!(handoff("bigin", &vec![b'{'; input.len()]), "");
}

#[test]
fn a_transcript_without_a_request_is_silence() {
    let dir = scratch("noreq");
    let current = put(&dir, "cur.jsonl", "", 0);
    let lines = [
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "an answer"}]}}),
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}},
        ]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "done"}]}}),
    ];
    put(&dir, "old.jsonl", lines.map(|v| v.to_string()).join("\n") + "\n", 100);
    assert_eq!(handoff("noreq", &event("clear", &current, "cur")), "");
}

#[test]
fn the_new_transcript_need_not_exist_yet() {
    // Claude Code may run the hook before it writes the new session's file.
    let dir = scratch("unborn");
    put(&dir, "old.jsonl", FIXTURE, 100);
    let current = dir.join("new-session.jsonl");
    assert!(!current.exists());
    let ctx = context(&handoff("unborn", &event("clear", &current, "new-session")));
    assert!(ctx.contains("deploy-canary-17"));
}

#[test]
fn stdin_over_the_cap_is_silence() {
    let dir = scratch("hugein");
    put(&dir, "old.jsonl", FIXTURE, 100);
    let current = put(&dir, "cur.jsonl", "", 0);
    let mut ev: Value = serde_json::from_slice(&event("clear", &current, "cur")).unwrap();
    ev["padding"] = json!("x".repeat(5 * 1024 * 1024));
    assert_eq!(handoff("hugein", ev.to_string().as_bytes()), "");
}
