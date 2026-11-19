//! `h0x-core gate`: the Token Saver, a Claude Code PostToolUse hook. It reads the hook event on
//! stdin and prints a smaller `updatedToolOutput` for a big Bash/Read/Grep result, or nothing
//! (the output stays as it was). It never fails: any problem is a pass-through with exit 0.

use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};

/// Outputs up to this many bytes pass through untouched.
const LIMIT: usize = 8192;
/// A shown text over this many bytes is cut or summarized.
const BUDGET: usize = 6000;
/// A JSON output over this many bytes is a candidate for a lossless re-encode.
const JSON_MIN: usize = 1024;
const CODE_EXTS: [&str; 17] =
    ["rs", "ts", "tsx", "js", "mjs", "py", "go", "java", "c", "cc", "cpp", "h", "hpp", "cs", "rb", "kt", "swift"];

/// The hook: stdin in, the replacement (if any) on stdout. Always 0.
pub fn run() -> i32 {
    let mut raw = Vec::new();
    if std::io::stdin().read_to_end(&mut raw).is_err() {
        return 0;
    }
    let out = std::panic::catch_unwind(|| String::from_utf8(raw).ok().and_then(|s| process(&s)));
    if let Ok(Some(text)) = out {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }
    0
}

/// The hook's stdout for one event, or None to leave the tool output alone.
fn process(input: &str) -> Option<String> {
    let ev: Value = serde_json::from_str(input).ok()?;
    if !matches!(ev["tool_name"].as_str(), Some("Bash" | "Read" | "Grep")) {
        return None;
    }
    let resp = &ev["tool_response"];
    let full = text_of(resp)?;
    let bytes = full.len();
    let text = full.replace("\r\n", "\n");
    let shown = shrink(&ev, &text, bytes)?;
    if shown.len() * 10 >= bytes * 9 {
        return None;
    }
    let updated = replace_text(resp, shown);
    Some(json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": updated}}).to_string())
}

/// The text of a tool response: a string, a file's content, stdout and stderr, or `content`.
fn text_of(resp: &Value) -> Option<String> {
    Some(match resp {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        _ => {
            if let Some(c) = resp["file"]["content"].as_str() {
                c.to_string()
            } else if let Some(o) = resp["stdout"].as_str() {
                format!("{o}{}", resp["stderr"].as_str().unwrap_or_default())
            } else if let Some(c) = resp["content"].as_str() {
                c.to_string()
            } else {
                resp.to_string()
            }
        }
    })
}

/// The response with its text replaced, in the same shape.
fn replace_text(resp: &Value, text: String) -> Value {
    let mut out = resp.clone();
    if resp["file"]["content"].is_string() {
        out["file"]["content"] = json!(text);
        out["file"]["numLines"] = json!(text.split('\n').count());
        out["file"]["startLine"] = json!(1);
    } else if resp["stdout"].is_string() {
        out["stdout"] = json!(text);
        out["stderr"] = json!("");
    } else if resp["content"].is_string() {
        out["content"] = json!(text);
    } else {
        out = json!(text);
    }
    out
}

/// The replacement text, note line included.
fn shrink(ev: &Value, text: &str, bytes: usize) -> Option<String> {
    let parsed = match text.trim_start().chars().next() {
        Some('[' | '{') => serde_json::from_str::<Value>(text).ok(),
        _ => None,
    };
    if bytes > JSON_MIN
        && let Some(v) = &parsed
    {
        let (t, tsv) = reencode(v);
        if t.len() < bytes && t.len() <= LIMIT {
            let how = if tsv { "TSV; empty cell = key absent" } else { "compact JSON" };
            return Some(format!("[HyperOx Token Saver: JSON re-encoded losslessly as {how}]\n{t}"));
        }
    }
    if bytes <= LIMIT {
        return None;
    }
    let file = ev["tool_input"]["file_path"].as_str().unwrap_or_default();
    let code = ev["tool_name"] == "Read"
        && file.rsplit_once('.').is_some_and(|(_, ext)| CODE_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)));
    let (body, how) = if code {
        (outline(text), "declaration outline with line numbers")
    } else if parsed.is_some() {
        let lines: Vec<&str> = text.split('\n').collect();
        let joined = if lines.len() > 50 {
            [&lines[..40], &["…"], &lines[lines.len() - 10..]].concat().join("\n")
        } else {
            lines.join("\n")
        };
        (cut_bytes(&joined, BUDGET).to_string(), "JSON head and tail")
    } else {
        let folded = fold_keep(&ws(text));
        if folded.len() <= BUDGET {
            (folded, "lines that differ only in numbers are shown once with [xN alike]; error/result lines kept")
        } else {
            (section_summary(text), "summary per section: key lines (errors, failures, results) and last lines")
        }
    };
    let note = format!(
        "[HyperOx Token Saver: output {:.1} KB, {} lines, not shown in full: {how}. Re-run narrower for exact lines.]",
        bytes as f64 / 1024.0,
        text.split('\n').count()
    );
    Some(format!("{note}\n{body}"))
}

/// JSON as TSV (an array of objects, or an object holding one), else compact JSON.
fn reencode(v: &Value) -> (String, bool) {
    let rows = |a: &Value| a.as_array().is_some_and(|a| a.len() > 1 && a.iter().all(Value::is_object));
    if rows(v) {
        return (tsv(v), true);
    }
    if let Value::Object(m) = v
        && let Some(k) = m.keys().find(|k| rows(&m[*k]))
    {
        let mut rest = m.clone();
        rest.remove(k);
        return (format!("{}\n{k} (TSV):\n{}", Value::Object(rest), tsv(&m[k])), true);
    }
    (v.to_string(), false)
}

fn flat(prefix: &str, m: &Map<String, Value>, out: &mut Vec<(String, String)>) {
    for (k, v) in m {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v {
            Value::Object(o) if !o.is_empty() => flat(&key, o, out),
            Value::String(s) if s.contains(['\t', '\n', '\r']) => out.push((key, v.to_string())),
            Value::String(s) => out.push((key, s.clone())),
            _ => out.push((key, v.to_string())),
        }
    }
}

/// Rows of objects as a header and one tab-separated line each; nested objects get dotted
/// keys, arrays stay JSON, a missing key is an empty cell.
fn tsv(rows: &Value) -> String {
    let rows: Vec<Vec<(String, String)>> = rows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .map(|m| {
            let mut cells = Vec::new();
            flat("", m, &mut cells);
            cells
        })
        .collect();
    let mut cols: Vec<&str> = Vec::new();
    for (k, _) in rows.iter().flatten() {
        if !cols.contains(&k.as_str()) {
            cols.push(k);
        }
    }
    let mut lines = vec![cols.join("\t")];
    for row in &rows {
        let cell = |c: &&str| row.iter().find(|(k, _)| k == c).map_or("", |(_, v)| v.as_str());
        lines.push(cols.iter().map(cell).collect::<Vec<_>>().join("\t"));
    }
    lines.join("\n")
}

/// Declaration lines of a code file as `L<n>: <line>`, at most 3000 chars.
fn outline(text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if !is_decl(line) {
            continue;
        }
        let l = format!("L{}: {}", i + 1, cut_chars(line.trim(), 160));
        if out.len() + l.len() + 1 > 3000 {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&l);
    }
    out
}

fn is_decl(line: &str) -> bool {
    const KW: [&str; 12] =
        ["fn", "struct", "enum", "trait", "impl", "mod", "const", "static", "type", "class", "def", "function"];
    const KW2: [&str; 2] = ["export", "interface"];
    let mut s = line.trim_start();
    if let Some(r) = s.strip_prefix("pub") {
        let r = if r.starts_with('(') { r.find(')').map_or(r, |i| &r[i + 1..]) } else { r };
        let t = r.trim_start();
        if t.len() < r.len() {
            s = t;
        }
    }
    if let Some(r) = s.strip_prefix("async") {
        let t = r.trim_start();
        if t.len() < r.len() {
            s = t;
        }
    }
    KW.iter().chain(&KW2).any(|k| s.strip_prefix(k).is_some_and(|r| !r.starts_with(is_word)))
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn cut_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// At most `max` bytes, cut on a char boundary.
fn cut_bytes(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Indentation as one space per 4 columns, runs of blanks collapsed, blank lines dropped.
fn ws(s: &str) -> String {
    let lines: Vec<String> = s
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let rest = l.trim_start_matches([' ', '\t']);
            let width: usize = l[..l.len() - rest.len()].chars().map(|c| if c == '\t' { 4 } else { 1 }).sum();
            let mut line = " ".repeat(width.div_ceil(4));
            let mut blank = false;
            for c in rest.trim_end().chars() {
                if c == ' ' || c == '\t' {
                    if !blank {
                        line.push(' ');
                    }
                    blank = true;
                } else {
                    line.push(c);
                    blank = false;
                }
            }
            line
        })
        .collect();
    lines.join("\n")
}

/// Replaces each run of chars matching `pred` that is at least `min` long with `#`.
fn hash_runs(s: &str, pred: fn(char) -> bool, min: usize) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= min {
            out.push('#');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if pred(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// `a::b::c` (word chars, `.` and `-` inside) becomes `~`.
fn qualified_names(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let part = |c: char| is_word(c) || c == '.' || c == '-';
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if !part(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && part(chars[j]) {
            j += 1;
        }
        let mut colons = false;
        while j + 2 < chars.len() && chars[j] == ':' && chars[j + 1] == ':' && part(chars[j + 2]) {
            colons = true;
            j += 2;
            while j < chars.len() && part(chars[j]) {
                j += 1;
            }
        }
        let cand: String = chars[i..j].iter().collect();
        let core = cand.trim_matches(|c: char| !is_word(c));
        if colons && core.contains("::") {
            let at = cand.find(core).unwrap_or(0);
            out.push_str(&cand[..at]);
            out.push('~');
            out.push_str(&cand[at + core.len()..]);
        } else {
            out.push_str(&cand);
        }
        i = j;
    }
    out
}

/// A line without its numbers and hex ids: lines alike except for those share a skeleton.
/// Paths keep their words, so `/api/users/1` and `/api/cart/2` stay apart.
fn skel(line: &str) -> String {
    let s = qualified_names(line);
    let s = hash_runs(&s, |c| c.is_ascii_hexdigit(), 7);
    let s = hash_runs(&s, |c| c.is_ascii_digit(), 1);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_word(hay: &str, word: &str, ends: &[&str]) -> bool {
    hay.match_indices(word).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after: String = hay[i + word.len()..].chars().take_while(|c| is_word(*c)).collect();
        !before.is_some_and(is_word) && ends.contains(&after.as_str())
    })
}

/// Errors, failures and results: such lines fold only from 5 alike.
fn is_key(line: &str) -> bool {
    const WORDS: [&str; 19] = [
        "error",
        "panic",
        "warn",
        "exception",
        "timeout",
        "timed out",
        "denied",
        "refused",
        "abort",
        "fatal",
        "assert",
        "left:",
        "right:",
        "test result",
        "summary",
        "total",
        "token",
        "exit code",
        "killed",
    ];
    let l = line.to_lowercase();
    WORDS.iter().any(|w| l.contains(w))
        || l.contains("unavailable")
        || has_word(&l, "fail", &["", "ed", "ure", "ures", "s"])
}

/// The wider key set for the per-section summary.
fn is_summary_key(line: &str) -> bool {
    const WORDS: [&str; 26] = [
        "error",
        "fail",
        "panic",
        "warn",
        "exception",
        "timeout",
        "timed out",
        "denied",
        "refused",
        "abort",
        "fatal",
        "assert",
        "left",
        "right",
        "result",
        "summary",
        "total",
        "passed",
        "skipped",
        "token",
        "exit",
        "status",
        "done",
        "finished",
        "killed",
        "unavailable",
    ];
    let l = line.to_lowercase();
    WORDS.iter().any(|w| l.contains(w))
        || l.match_indices("ok.").any(|(i, _)| !l[..i].chars().next_back().is_some_and(is_word))
}

struct Section<'a> {
    head: &'a str,
    body: Vec<&'a str>,
}

fn some_after(line: &str, skip: usize) -> bool {
    line.get(skip..).is_some_and(|r| r.chars().any(|c| !c.is_whitespace()))
}

/// Splits at a repeated header shape (`=== x`, `--- x`, `## x`, `==> x <==`, `[x]`), else one section.
fn sections(s: &str) -> Vec<Section<'_>> {
    let lines: Vec<&str> = s.split('\n').collect();
    let shapes: [fn(&str) -> bool; 5] = [
        |l| l.starts_with("==") && some_after(l, 2),
        |l| l.starts_with("---") && some_after(l, 3),
        |l| {
            let n = l.chars().take_while(|c| *c == '#').count();
            (1..=3).contains(&n) && l[n..].starts_with(char::is_whitespace) && some_after(l, n)
        },
        |l| l.len() >= 6 && l.starts_with("==>") && l.ends_with("<=="),
        |l| {
            let t = l.trim_end();
            t.len() >= 3 && t.starts_with('[') && t.ends_with(']') && !t[1..t.len() - 1].contains(']')
        },
    ];
    for shape in shapes {
        let idx: Vec<usize> = (0..lines.len()).filter(|i| shape(lines[*i])).collect();
        if idx.len() >= 3 {
            let mut out = Vec::new();
            if idx[0] > 0 {
                out.push(Section { head: "", body: lines[..idx[0]].to_vec() });
            }
            for (j, st) in idx.iter().enumerate() {
                let end = idx.get(j + 1).copied().unwrap_or(lines.len());
                out.push(Section { head: lines[*st], body: lines[st + 1..end].to_vec() });
            }
            return out;
        }
    }
    vec![Section { head: "", body: lines }]
}

/// Groups line positions by skeleton.
fn groups(body: &[&str]) -> (Vec<String>, HashMap<String, Vec<usize>>) {
    let skels: Vec<String> = body.iter().map(|l| skel(l)).collect();
    let mut g: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, k) in skels.iter().enumerate() {
        if !k.is_empty() {
            g.entry(k.clone()).or_default().push(i);
        }
    }
    (skels, g)
}

/// Lines alike except for numbers are shown once with `[xN alike]`; key lines fold only from
/// 5 alike and show the last one.
fn fold_keep(s: &str) -> String {
    let parts: Vec<String> = sections(s)
        .into_iter()
        .map(|Section { head, body }| {
            let (skels, g) = groups(&body);
            let mut out: Vec<String> = Vec::new();
            for (i, l) in body.iter().enumerate() {
                if skels[i].is_empty() {
                    continue;
                }
                let g = &g[&skels[i]];
                let key = is_key(l);
                if g.len() < if key { 5 } else { 3 } {
                    out.push((*l).to_string());
                } else if g[0] == i {
                    out.push(if key {
                        let last = body[g[g.len() - 1]].trim();
                        format!("{l}  [x{} alike; last: {}]", g.len(), cut_chars(last, 160))
                    } else {
                        format!("{l}  [x{} alike]", g.len())
                    });
                }
            }
            std::iter::once(head.to_string()).chain(out).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
        })
        .collect();
    parts.join("\n")
}

/// Per section within the budget: its header, its folded key lines and its last lines.
fn section_summary(s: &str) -> String {
    let secs = sections(s);
    let per = (BUDGET / secs.len()).max(400);
    let cut = |l: &str| if l.chars().count() > 160 { format!("{}…", cut_chars(l, 160)) } else { l.to_string() };
    let parts: Vec<String> = secs
        .iter()
        .map(|Section { head, body }| {
            let (skels, g) = groups(body);
            let mut folded: Vec<String> = Vec::new();
            for (i, l) in body.iter().enumerate() {
                if skels[i].is_empty() {
                    continue;
                }
                let g = &g[&skels[i]];
                if g.len() < 3 {
                    folded.push((*l).to_string());
                } else if g[0] == i {
                    folded.push(format!("{l}  [x{} alike]", g.len()));
                }
            }
            let tail: Vec<&&str> = body.iter().filter(|l| !l.trim().is_empty()).collect();
            let tail = &tail[tail.len().saturating_sub(4)..];
            let mut txt: Vec<String> = Vec::new();
            let all = std::iter::once(head.to_string())
                .chain(folded.iter().filter(|l| is_summary_key(l)).map(|l| cut(l)))
                .chain(std::iter::once("…last lines:".to_string()))
                .chain(tail.iter().map(|l| cut(l)));
            for l in all.filter(|l| !l.is_empty()) {
                if !txt.contains(&l) {
                    txt.push(l);
                }
            }
            let joined = txt.join("\n");
            if joined.chars().count() > per { format!("{}…", cut_chars(&joined, per)) } else { joined }
        })
        .collect();
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(tool: &str, input: Value, resp: Value) -> String {
        json!({"tool_name": tool, "tool_input": input, "tool_response": resp}).to_string()
    }

    fn bash(stdout: &str) -> String {
        event("Bash", json!({"command": "x"}), json!({"stdout": stdout, "stderr": "", "interrupted": false}))
    }

    fn reply(out: Option<String>) -> Value {
        serde_json::from_str(&out.expect("the gate replaced the output")).unwrap()
    }

    fn shown(out: Option<String>) -> String {
        let v = reply(out);
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let u = &v["hookSpecificOutput"]["updatedToolOutput"];
        u["stdout"].as_str().or_else(|| u["file"]["content"].as_str()).or_else(|| u.as_str()).unwrap().to_string()
    }

    /// Exactly `n` bytes of ASCII lines that differ only in a number.
    fn ticks(n: usize) -> String {
        let mut s = String::new();
        let mut i = 0;
        while s.len() < n {
            s.push_str(&format!("tick {i} done ok\n"));
            i += 1;
        }
        s.truncate(n);
        s
    }

    fn rust_file(bytes: usize) -> String {
        let mut s = String::new();
        let mut i = 0;
        while s.len() < bytes {
            s.push_str(&format!(
                "pub fn item_{i}(x: u32) -> u32 {{\n    let y = x + {i}; // padding padding padding padding\n    y\n}}\n\n"
            ));
            i += 1;
        }
        s
    }

    #[test]
    fn empty_and_broken_input_pass_through() {
        assert_eq!(process(""), None);
        assert_eq!(process("not json"), None);
        assert_eq!(process("{}"), None);
        assert_eq!(process("[1,2"), None);
        assert_eq!(process(&event("Bash", json!({}), Value::Null)), None);
    }

    #[test]
    fn small_output_passes_through() {
        assert_eq!(process(&bash("hello\nworld\n")), None);
    }

    #[test]
    fn the_limit_is_8192_bytes_inclusive() {
        assert_eq!(process(&bash(&ticks(8192))), None);
        let out = shown(process(&bash(&ticks(8193))));
        assert!(out.starts_with("[HyperOx Token Saver: output 8.0 KB, "), "{out}");
        assert!(out.contains("[x"), "{out}");
    }

    #[test]
    fn a_big_rust_read_becomes_an_outline() {
        let src = rust_file(70_000);
        let ev = event(
            "Read",
            json!({"file_path": "/work/src/lib.rs"}),
            json!({"type": "text", "file": {"filePath": "/work/src/lib.rs", "content": src, "numLines": 99, "startLine": 7, "totalLines": 99}}),
        );
        let v = reply(process(&ev));
        let file = &v["hookSpecificOutput"]["updatedToolOutput"]["file"];
        let content = file["content"].as_str().unwrap();
        assert!(content.contains("declaration outline"), "{content}");
        assert!(content.contains("\nL1: pub fn item_0(x: u32) -> u32 {"));
        assert!(content.contains("\nL6: pub fn item_1("));
        let body = content.split_once('\n').unwrap().1;
        assert!(body.len() <= 3000);
        assert_eq!(file["startLine"], 1);
        assert_eq!(file["numLines"], content.split('\n').count());
        assert_eq!(file["filePath"], "/work/src/lib.rs", "other fields are kept");
        assert_eq!(file["totalLines"], 99);
        assert_eq!(v["hookSpecificOutput"]["updatedToolOutput"]["type"], "text");
    }

    #[test]
    fn a_repeated_log_folds_and_keeps_key_lines() {
        let mut log = String::new();
        for i in 0..400 {
            log.push_str(&format!("INFO request {i} handled in {}ms by worker\n", i * 3));
            if i % 100 == 0 {
                log.push_str(&format!("ERROR connection refused port {}\n", 5000 + i));
            }
        }
        log.push_str("test result: FAILED. 3 passed; 1 failed\n");
        assert!(log.len() > LIMIT);
        let out = shown(process(&bash(&log)));
        assert!(out.contains("INFO request 0 handled in 0ms by worker  [x400 alike]"), "{out}");
        for i in [0, 100, 200, 300] {
            assert!(out.contains(&format!("ERROR connection refused port {}", 5000 + i)), "{out}");
        }
        assert!(out.contains("test result: FAILED. 3 passed; 1 failed"));
        assert!(!out.contains("INFO request 1 handled"));
    }

    #[test]
    fn key_lines_fold_from_five_and_show_the_last() {
        let mut log = String::new();
        for i in 0..6 {
            log.push_str(&format!("ERROR job {i} failed after {} tries\n", i + 10));
        }
        log.push_str(&ticks(9000));
        let out = shown(process(&bash(&log)));
        assert!(
            out.contains("ERROR job 0 failed after 10 tries  [x6 alike; last: ERROR job 5 failed after 15 tries]"),
            "{out}"
        );
        assert!(!out.contains("ERROR job 1 "));
    }

    #[test]
    fn url_paths_are_not_merged() {
        let mut log = String::new();
        for i in 0..150 {
            log.push_str(&format!("GET /api/users/{i} 200 served from cache node alpha\n"));
            log.push_str(&format!("GET /api/cart/{} 200 served from cache node alpha\n", i + 7));
        }
        let out = shown(process(&bash(&log)));
        assert!(out.contains("GET /api/users/0 200 served from cache node alpha  [x150 alike]"), "{out}");
        assert!(out.contains("GET /api/cart/7 200 served from cache node alpha  [x150 alike]"), "{out}");
    }

    #[test]
    fn skeletons_keep_words_and_drop_numbers() {
        assert_eq!(skel("GET /api/users/12 took 3ms"), "GET /api/users/# took #ms");
        assert_ne!(skel("GET /api/users/1"), skel("GET /api/cart/1"));
        assert_eq!(skel("id deadbeef01 at 2024-01-02"), "id # at #-#-#");
        assert_eq!(skel("  a::b::c   x "), "~ x");
        assert_eq!(skel("a::-"), "a::-");
    }

    #[test]
    fn json_rows_become_tsv() {
        let rows: Vec<Value> = (0..40)
            .map(|i| {
                let mut o = json!({"id": i, "name": format!("user {i}"), "meta": {"role": "dev", "tags": ["a", "b"]}});
                if i % 2 == 0 {
                    o["note"] = json!("tab\there");
                }
                o
            })
            .collect();
        let pretty = serde_json::to_string_pretty(&rows).unwrap();
        assert!(pretty.len() > 1024);
        let out = shown(process(&bash(&pretty)));
        let (note, tsv) = out.split_once('\n').unwrap();
        assert_eq!(note, "[HyperOx Token Saver: JSON re-encoded losslessly as TSV; empty cell = key absent]");
        let lines: Vec<&str> = tsv.split('\n').collect();
        assert_eq!(lines.len(), 41);
        let mut cols: Vec<&str> = lines[0].split('\t').collect();
        cols.sort_unstable();
        assert_eq!(cols, ["id", "meta.role", "meta.tags", "name", "note"]);
        let col = |name: &str| lines[0].split('\t').position(|c| c == name).unwrap();
        let row1: Vec<&str> = lines[2].split('\t').collect();
        assert_eq!(row1[col("meta.tags")], "[\"a\",\"b\"]");
        assert_eq!(row1[col("note")], "", "absent key is an empty cell");
        let row0: Vec<&str> = lines[1].split('\t').collect();
        assert_eq!(row0[col("note")], "\"tab\\there\"", "a tab in a cell is kept as JSON text");
    }

    #[test]
    fn an_object_holding_rows_keeps_the_rest_as_compact_json() {
        let rows: Vec<Value> = (0..60).map(|i| json!({"n": i, "label": format!("row number {i}")})).collect();
        let doc = serde_json::to_string_pretty(&json!({"total": 60, "items": rows})).unwrap();
        let out = shown(process(&bash(&doc)));
        assert!(out.contains("\n{\"total\":60}\nitems (TSV):\n"), "{out}");
    }

    #[test]
    fn other_json_is_compacted_when_small_enough() {
        let doc = serde_json::to_string_pretty(&json!({"a": vec![1; 400], "b": {"c": "d"}})).unwrap();
        let out = shown(process(&bash(&doc)));
        assert!(out.starts_with("[HyperOx Token Saver: JSON re-encoded losslessly as compact JSON]\n{"), "{out}");
    }

    #[test]
    fn big_json_that_does_not_shrink_keeps_head_and_tail() {
        let words: Vec<String> = (0..2000).map(|i| format!("{i:04}-é-{}", i * 7919 % 10007)).collect();
        let doc = serde_json::to_string_pretty(&json!({"words": words})).unwrap();
        assert!(doc.lines().count() > 50);
        let out = shown(process(&bash(&doc)));
        let (note, body) = out.split_once('\n').unwrap();
        assert!(note.contains("JSON head and tail"), "{note}");
        assert!(body.len() <= BUDGET);
        assert!(body.starts_with("{\n  \"words\": ["));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        assert_eq!(cut_bytes("ééééé", 5), "éé");
        assert_eq!(cut_bytes("ééééé", 0), "");
        assert_eq!(cut_bytes("ab", 10), "ab");
        assert_eq!(cut_chars("日本語です", 3), "日本語");
        assert_eq!(cut_chars("ab", 3), "ab");
        let doc = json!({"text": "日".repeat(20_000)}).to_string();
        let out = shown(process(&bash(&doc)));
        let (_, body) = out.split_once('\n').unwrap();
        assert!(body.len() <= BUDGET && body.starts_with("{\"text\":\"日"));
        let long = format!("{}\n{}", "é".repeat(400), ticks(9000));
        assert!(shown(process(&bash(&long))).contains(&"é".repeat(400)));
    }

    #[test]
    fn output_that_would_not_shrink_passes_through() {
        // Every section is all key lines, so its summary is as long as the section itself.
        let letters =
            |i: usize| [i / 676, i / 26 % 26, i % 26].map(|d| (b'a' + d as u8) as char).iter().collect::<String>();
        let mut text = String::new();
        for s in 0..30 {
            text.push_str(&format!("== part {s}\n"));
            for i in 0..5 {
                text.push_str(&format!(
                    "error {} while reading the configuration file from disk\n",
                    letters(s * 5 + i)
                ));
            }
        }
        assert!(text.len() > LIMIT);
        assert_eq!(process(&bash(&text)), None);
        // Repeated lines of the same size do shrink.
        assert!(process(&bash(&ticks(text.len()))).is_some());
    }

    #[test]
    fn crlf_input_works() {
        let text = ticks(9000).replace('\n', "\r\n");
        let out = shown(process(&bash(&text)));
        assert!(!out.contains('\r'), "{out:?}");
        assert!(out.contains("tick 0 done ok  [x"), "{out}");
        let src = rust_file(20_000).replace('\n', "\r\n");
        let ev = event("Read", json!({"file_path": "a.rs"}), json!({"file": {"content": src}}));
        let out = shown(process(&ev));
        assert!(!out.contains('\r') && out.contains("\nL1: pub fn item_0"), "{out}");
    }

    #[test]
    fn a_windows_path_picks_the_file_kind() {
        let src = rust_file(20_000);
        let read = |path: &str| {
            let ev = event("Read", json!({"file_path": path}), json!({"file": {"content": src}}));
            shown(process(&ev))
        };
        assert!(read("C:\\proj\\src\\main.RS").contains("declaration outline"));
        assert!(read("C:\\proj\\notes.txt").contains("alike"));
        assert!(read("C:\\a.rs\\Makefile").contains("alike"));
        assert!(!read("C:\\a.b\\rs").contains("declaration outline"));
    }

    #[test]
    fn other_response_shapes_keep_their_shape() {
        let big = ticks(9000);
        let v = reply(process(&event(
            "Bash",
            json!({}),
            json!({"stdout": big, "stderr": "warning: x\n", "interrupted": true}),
        )));
        let u = &v["hookSpecificOutput"]["updatedToolOutput"];
        assert_eq!(u["stderr"], "");
        assert_eq!(u["interrupted"], true);
        assert!(u["stdout"].as_str().unwrap().contains("warning: x"), "stderr is part of the text");
        let v = reply(process(&event("Grep", json!({}), json!({"mode": "content", "content": big, "numLines": 9}))));
        let u = &v["hookSpecificOutput"]["updatedToolOutput"];
        assert_eq!(u["mode"], "content");
        assert!(u["content"].as_str().unwrap().contains("[x"));
        let v = reply(process(&event("Bash", json!({}), json!(big))));
        assert!(v["hookSpecificOutput"]["updatedToolOutput"].as_str().unwrap().contains("[x"));
    }

    #[test]
    fn an_unknown_tool_passes_through() {
        let big = ticks(20_000);
        for tool in ["Write", "WebFetch", "bash", "mcp__x__y"] {
            assert_eq!(process(&event(tool, json!({}), json!({"stdout": big}))), None, "{tool}");
        }
        assert_eq!(process(&json!({"tool_input": {}, "tool_response": {"stdout": big}}).to_string()), None);
    }

    #[test]
    fn a_long_mixed_output_falls_back_to_a_section_summary() {
        let mut text = String::new();
        for s in 0..40 {
            text.push_str(&format!("=== suite {s}\n"));
            for i in 0..30 {
                text.push_str(&format!(
                    "case {i} alpha{} beta{} gamma{} ok\n",
                    "z".repeat(i),
                    "q".repeat(i % 7),
                    "r".repeat(i % 5)
                ));
            }
            text.push_str(&format!("test result: ok. {s} passed\n"));
        }
        let out = shown(process(&bash(&text)));
        assert!(out.contains("summary per section"), "{out}");
        assert!(out.contains("=== suite 39") && out.contains("test result: ok. 39 passed"), "{out}");
    }

    #[test]
    fn the_matchers_agree_with_their_regexes() {
        assert!(is_key("Build FAILED"));
        assert!(is_key("3 failures"));
        assert!(!is_key("failing soon"));
        assert!(!is_key("unfailed"));
        assert!(is_key("Total: 3"));
        assert!(is_summary_key("all ok."));
        assert!(!is_summary_key("book."));
        assert!(!is_summary_key("fine"));
        assert!(is_decl("    pub(crate) async fn f()"));
        assert!(is_decl("export default"));
        assert!(!is_decl("pubfn x"));
        assert!(!is_decl("function_call(x)"));
        assert!(!is_decl("let x = 1;"));
        assert_eq!(sections("a\n== x\nb\n== y\nc\n== z\nd").len(), 4);
        assert_eq!(sections("a\n== x\nb\n== y\nc").len(), 1);
        assert_eq!(sections("## a\n### b\n# c\n#### d").len(), 3);
        assert_eq!(sections("[a]\n[b]\n[c]\n").len(), 3);
    }
}
