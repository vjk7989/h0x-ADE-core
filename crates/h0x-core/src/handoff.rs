//! `h0x-core handoff`: a Claude Code SessionStart hook for `/clear`. It reads the hook event on
//! stdin, finds the session that was just cleared (the newest other transcript beside the new
//! one), and prints a compact package of what that session did as extra context: no model call.
//! It never fails: any problem is no output with exit 0.

use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The package, in bytes.
const MAX_PACKAGE: usize = 3000;
/// How much of the end of a transcript is read.
const TAIL: u64 = 2 * 1024 * 1024;
/// The most requests shown in the objective and findings (fewer when the package would not fit).
const TURNS: usize = 8;
const FILES: usize = 15;
const COMMANDS: usize = 10;
/// The most hook input read; larger is bad input.
const MAX_INPUT: u64 = 4 * 1024 * 1024;

pub fn run() -> i32 {
    let mut raw = Vec::new();
    if std::io::stdin().take(MAX_INPUT + 1).read_to_end(&mut raw).is_err() || raw.len() as u64 > MAX_INPUT {
        return 0;
    }
    std::panic::set_hook(Box::new(|_| {})); // a hook never talks to the agent: stay quiet
    if let Ok(Some(out)) = std::panic::catch_unwind(|| output(&raw)) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{out}");
        let _ = stdout.flush();
    }
    0
}

/// The hook's stdout line for one event, or None for no output.
fn output(event: &[u8]) -> Option<String> {
    let ev: Value = serde_json::from_slice(event).ok()?;
    if ev["source"].as_str() != Some("clear") {
        return None;
    }
    let current = Path::new(ev["transcript_path"].as_str()?);
    if current.is_dir() {
        return None;
    }
    let previous = previous_transcript(current, ev["session_id"].as_str())?;
    let package = package(&read_tail(&previous)?)?;
    Some(json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": package}}).to_string())
}

/// The newest `*.jsonl` beside `current`, other than the current session's own file.
fn previous_transcript(current: &Path, session_id: Option<&str>) -> Option<PathBuf> {
    let dir = current.parent()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let own = path == current || session_id.is_some_and(|id| path.file_stem().is_some_and(|s| s == id));
        if own || path.extension().is_none_or(|e| e != "jsonl") || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
        if best.as_ref().is_none_or(|(t, p)| (modified, &path) > (*t, p)) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p)
}

/// The last `TAIL` bytes of the file, from the start of a whole line.
fn read_tail(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(TAIL).read_to_end(&mut buf).ok()?;
    if start > 0 {
        let skip = buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1);
        buf.drain(..skip);
    }
    Some(buf)
}

struct Turn {
    prompt: String,
    answer: Option<String>,
}

/// The package for a transcript (JSON lines; lines that do not parse are skipped), or None when
/// it holds no request.
fn package(transcript: &[u8]) -> Option<String> {
    let mut turns: Vec<Turn> = Vec::new();
    let (mut files, mut commands): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for line in transcript.split(|&b| b == b'\n') {
        let Ok(ev) = serde_json::from_slice::<Value>(line) else { continue };
        if ev["isSidechain"].as_bool() == Some(true) || ev["isMeta"].as_bool() == Some(true) {
            continue;
        }
        let content = &ev["message"]["content"];
        match ev["type"].as_str() {
            Some("user") => {
                if let Some(prompt) = prompt_text(content) {
                    turns.push(Turn { prompt, answer: None });
                }
            }
            Some("assistant") => {
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = squash(block["text"].as_str().unwrap_or(""));
                            if let (false, Some(turn)) = (text.is_empty(), turns.last_mut()) {
                                turn.answer = Some(text);
                            }
                        }
                        Some("tool_use") => tool_use(&block["input"], &mut files, &mut commands),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if turns.is_empty() {
        return None;
    }
    let none = |s: String| if s.is_empty() { "none".to_string() } else { s };
    let tail = format!(
        "Files referenced: {}\nCommands run: {}\nTests run: {}\nPending work: continue with the next request below.",
        none(files.iter().take(FILES).cloned().collect::<Vec<_>>().join(", ")),
        none(quote(commands.iter())),
        none(quote(commands.iter().filter(|c| c.contains("test") || c.contains("unittest") || c.contains("pytest")))),
    );
    // The latest requests that leave room for every section; the cut is the last resort.
    let mut text = String::new();
    for count in (1..=turns.len().min(TURNS)).rev() {
        let first = turns.len() - count;
        let mut lines = vec![
            "## Context handoff from the previous session (compact; trust it, do not redo this work)".to_string(),
            format!(
                "Objective so far: {}",
                turns[first..]
                    .iter()
                    .enumerate()
                    .map(|(k, t)| format!("({}) {}", first + k + 1, take(&t.prompt, 110)))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            "Findings / decisions (the answers already given):".to_string(),
        ];
        for (k, t) in turns[first..].iter().enumerate() {
            let answer = t.answer.as_deref().map_or("(no answer)".to_string(), |a| take(a, 400));
            lines.push(format!("- step {}: {answer}", first + k + 1));
        }
        lines.push(tail.clone());
        text = lines.join("\n");
        if text.len() <= MAX_PACKAGE {
            break;
        }
    }
    Some(cut_bytes(&text, MAX_PACKAGE).to_string())
}

/// The text of a user message that is a request: a string, or text blocks; not a tool result and
/// not a harness message such as a command echo.
fn prompt_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = squash(&text);
    (!text.is_empty() && !text.starts_with('<')).then_some(text)
}

/// Files a tool touched and shell commands it ran (first `FILES` / `COMMANDS` kept).
fn tool_use(input: &Value, files: &mut Vec<String>, commands: &mut Vec<String>) {
    let mut add = |f: &str| {
        let f = clean(f).replace('\\', "/");
        if !files.contains(&f) {
            files.push(f);
        }
    };
    if let Some(f) = input["file_path"].as_str().or_else(|| input["path"].as_str()) {
        add(f);
    }
    if let Some(c) = input["command"].as_str().filter(|_| commands.len() < COMMANDS) {
        commands.push(take(&squash(c), 100));
    }
}

/// Commands in backticks, joined with `; `.
fn quote<'a>(commands: impl Iterator<Item = &'a String>) -> String {
    commands.map(|c| format!("`{c}`")).collect::<Vec<_>>().join("; ")
}

/// Without control characters other than whitespace: the model never gets an escape or a NUL.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r')).collect()
}

fn squash(s: &str) -> String {
    clean(s).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn take(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

/// At most `max` bytes, cut on a char boundary.
fn cut_bytes(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
    }

    fn assistant(blocks: Value) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": blocks}}).to_string()
    }

    fn text(t: &str) -> String {
        assistant(json!([{"type": "text", "text": t}]))
    }

    fn tool(name: &str, input: Value) -> String {
        assistant(json!([{"type": "tool_use", "name": name, "input": input}]))
    }

    fn make(lines: &[String]) -> Vec<u8> {
        lines.join("\n").into_bytes()
    }

    #[test]
    fn the_package_has_the_sections_in_order() {
        let t = make(&[
            user("Fix the parser"),
            tool("Read", json!({"file_path": "C:\\w\\src\\p.rs"})),
            tool("Bash", json!({"command": "cargo test -p parser"})),
            tool("Bash", json!({"command": "ls"})),
            tool("Grep", json!({"pattern": "x", "path": "src"})),
            text("Fixed it\n  in two places."),
            user("Now the lexer"),
            text("Done."),
        ]);
        let p = package(&t).unwrap();
        assert_eq!(
            p,
            "## Context handoff from the previous session (compact; trust it, do not redo this work)\n\
             Objective so far: (1) Fix the parser (2) Now the lexer\n\
             Findings / decisions (the answers already given):\n\
             - step 1: Fixed it in two places.\n\
             - step 2: Done.\n\
             Files referenced: C:/w/src/p.rs, src\n\
             Commands run: `cargo test -p parser`; `ls`\n\
             Tests run: `cargo test -p parser`\n\
             Pending work: continue with the next request below."
        );
    }

    #[test]
    fn empty_sections_say_none_and_a_missing_answer_is_marked() {
        let p = package(&make(&[user("hi")])).unwrap();
        assert!(p.contains("- step 1: (no answer)"));
        assert!(p.contains("Files referenced: none\nCommands run: none\nTests run: none\n"));
    }

    #[test]
    fn nothing_useful_is_no_package() {
        assert_eq!(package(b""), None);
        assert_eq!(package(b"\n\n"), None);
        assert_eq!(package(&make(&[text("an answer without a request")])), None);
        // Tool results, command echoes and meta lines are not requests.
        let t = make(&[
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "x"}]}}).to_string(),
            user("<command-name>/clear</command-name>"),
            json!({"type": "user", "isMeta": true, "message": {"content": "meta"}}).to_string(),
            json!({"type": "user", "isSidechain": true, "message": {"content": "side"}}).to_string(),
            user("   "),
        ]);
        assert_eq!(package(&t), None);
    }

    #[test]
    fn text_blocks_in_a_user_message_are_the_request() {
        let t =
            make(&[json!({"type": "user", "message": {"content": [{"type": "text", "text": "do   it"}]}}).to_string()]);
        assert!(package(&t).unwrap().contains("(1) do it"));
    }

    #[test]
    fn broken_lines_are_skipped() {
        let mut t = make(&[user("one"), text("a1")]);
        t.extend_from_slice(b"\n{\"type\":\"user\",\"message\":{\"content\":\"tru");
        t.extend_from_slice(b"\n\xff\xfe not json\n");
        t.extend_from_slice(user("two").as_bytes());
        let p = package(&t).unwrap();
        assert!(p.contains("(1) one (2) two"), "{p}");
    }

    #[test]
    fn a_non_utf8_transcript_is_no_panic() {
        assert_eq!(package(&[0xff, 0xfe, 0x00, b'\n', 0x80, 0x81]), None);
        let mut t = user("ok").into_bytes();
        t.extend_from_slice(b"\n\xc3\x28\n");
        assert!(package(&t).is_some());
    }

    #[test]
    fn only_the_latest_turns_are_shown_with_their_numbers() {
        let lines: Vec<String> = (1..=12).flat_map(|i| [user(&format!("req{i}")), text(&format!("ans{i}"))]).collect();
        let p = package(&make(&lines)).unwrap();
        assert!(p.contains("(5) req5 (6) req6") && p.contains("(12) req12") && !p.contains("(4)"), "{p}");
        assert!(p.contains("- step 5: ans5") && p.contains("- step 12: ans12") && !p.contains("step 4"));
    }

    #[test]
    fn commands_and_files_are_bounded_and_deduplicated() {
        let mut lines = vec![user("go")];
        for i in 0..30 {
            lines.push(tool("Bash", json!({"command": format!("cmd{i} {}", "x".repeat(200))})));
            lines.push(tool("Read", json!({"file_path": format!("f{}", i % 20)})));
        }
        let p = package(&make(&lines)).unwrap();
        assert_eq!(p.matches("`cmd").count(), 10);
        let files = p.lines().find(|l| l.starts_with("Files referenced: ")).unwrap();
        assert_eq!(files.matches(", ").count(), 14);
        assert_eq!(p.matches("f0,").count(), 1);
    }

    #[test]
    fn the_cut_never_splits_a_character() {
        // Four-byte characters, shifted by 0-3 ASCII bytes so the 3000-byte mark lands on every
        // position inside a character.
        for shift in 0..4 {
            let lines: Vec<String> = std::iter::once(user("go"))
                .chain((0..10).map(|_| {
                    tool("Bash", json!({"command": format!("test{}{}", "a".repeat(shift), "\u{1f600}".repeat(100))}))
                }))
                .collect();
            let p = package(&make(&lines)).unwrap();
            assert!(p.len() <= MAX_PACKAGE && p.len() > MAX_PACKAGE - 4, "{shift}: {}", p.len());
        }
        assert_eq!(cut_bytes("a\u{1f600}", 2), "a");
        assert_eq!(cut_bytes("abc", 10), "abc");
        assert_eq!(cut_bytes("", 0), "");
    }

    #[test]
    fn every_section_after_the_requests_is_kept_at_3000_and_3001_bytes() {
        let build = |pad: usize| {
            let mut lines = vec![user("first"), text("one"), user("second")];
            for i in 0..15 {
                lines.push(tool("Read", json!({"file_path": format!("C:/w/src/file{i:02}.rs")})));
            }
            for i in 0..10 {
                lines.push(tool("Bash", json!({"command": format!("cargo test {i} {}", "x".repeat(90))})));
            }
            lines.push(text(&format!("two{}", "y".repeat(pad))));
            package(&make(&lines)).unwrap()
        };
        let small = build(0);
        let tail = small[small.find("Files referenced: ").unwrap()..].to_string();
        assert!(tail.ends_with("Pending work: continue with the next request below."), "{tail}");
        let pad = MAX_PACKAGE - small.len();
        assert!((1..=396).contains(&pad), "the fixture no longer lands near the limit: {}", small.len());
        // Exactly 3000 bytes: both requests and every section, nothing cut.
        let fits = build(pad);
        assert_eq!(fits.len(), MAX_PACKAGE);
        assert!(fits.contains("Objective so far: (1) first (2) second") && fits.ends_with(&tail), "{fits}");
        // One byte more: the oldest request goes, the sections stay whole.
        let over = build(pad + 1);
        assert!(over.len() < MAX_PACKAGE, "{}", over.len());
        assert!(over.contains("Objective so far: (2) second\n") && !over.contains("(1) first"), "{over}");
        assert!(over.contains(&format!("- step 2: two{}\n", "y".repeat(pad + 1))) && over.ends_with(&tail));
    }

    #[test]
    fn backslashes_in_file_paths_become_slashes_once() {
        let t = make(&[
            user("go"),
            tool("Read", json!({"file_path": "C:\\w\\a.rs"})),
            tool("Edit", json!({"file_path": "C:/w/a.rs"})),
            tool("Read", json!({"file_path": "C:\\w/b.rs"})),
            tool("Read", json!({"file_path": "\\\\srv\\share\\c.rs"})),
            tool("Read", json!({"file_path": "\\\\?\\C:\\w\\d.rs"})),
            tool("Grep", json!({"pattern": "x", "path": "src\\e"})),
        ]);
        let p = package(&t).unwrap();
        let files = p.lines().find(|l| l.starts_with("Files referenced: ")).unwrap();
        assert_eq!(files, "Files referenced: C:/w/a.rs, C:/w/b.rs, //srv/share/c.rs, //?/C:/w/d.rs, src/e");
    }

    #[test]
    fn control_characters_never_reach_the_package() {
        let t = make(&[
            user("go\u{1b}[31m red\u{0}\u{7f}\u{85}x\u{9f}"),
            tool("Bash", json!({"command": "echo \u{1b}[0m\u{7}hi\tthere"})),
            tool("Read", json!({"file_path": "a\u{1b}b/c\u{0}.rs"})),
            text("done\u{1b}]0;title\u{7}\nnext\u{8}"),
        ]);
        let p = package(&t).unwrap();
        assert!(!p.chars().any(|c| c.is_control() && c != '\n'), "{p:?}");
        assert!(p.contains("(1) go[31m redx") && p.contains("`echo [0mhi there`") && p.contains("ab/c.rs"), "{p}");
        assert!(p.contains("- step 1: done]0;title next"), "{p}");
    }

    #[test]
    fn a_transcript_path_that_is_a_directory_gives_nothing() {
        let base = std::env::temp_dir().join(format!("h0x-handoff-dir-{}", std::process::id()));
        let dir = base.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(base.join("old.jsonl"), user("hello")).unwrap();
        let ev = json!({"source": "clear", "transcript_path": dir.to_string_lossy(), "session_id": "x"});
        assert_eq!(output(ev.to_string().as_bytes()), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_clear_event_with_a_transcript_answers() {
        for ev in [
            json!({"source": "startup", "transcript_path": "/x/y.jsonl"}),
            json!({"source": "resume", "transcript_path": "/x/y.jsonl"}),
            json!({"source": "clear"}),
            json!({"source": "clear", "transcript_path": "/no/such/dir/y.jsonl", "session_id": "y"}),
            json!({"transcript_path": "/x/y.jsonl"}),
            json!([1, 2]),
        ] {
            assert_eq!(output(ev.to_string().as_bytes()), None, "{ev}");
        }
        assert_eq!(output(b""), None);
        assert_eq!(output(b"{not json"), None);
    }
}
