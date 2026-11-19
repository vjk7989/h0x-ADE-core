//! `h0x-core gate`: the Token Saver, a Claude Code PostToolUse hook. It reads the hook event on
//! stdin and prints a smaller `updatedToolOutput` for a big Bash/Read/Grep result, or nothing
//! (the output stays as it was). It never fails: any problem is a pass-through with exit 0.
//! After the answer is printed it books the saving: a lifetime counter in the state folder and
//! the calling terminal's `h0x.saved` label, both best effort and bounded.

use crate::paths::Paths;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// Outputs up to this many bytes pass through untouched.
const LIMIT: usize = 8192;
/// A shown text over this many bytes is cut or summarized.
const BUDGET: usize = 6000;
/// A JSON output over this many bytes is a candidate for a lossless re-encode.
const JSON_MIN: usize = 1024;
const CODE_EXTS: [&str; 17] =
    ["rs", "ts", "tsx", "js", "mjs", "py", "go", "java", "c", "cc", "cpp", "h", "hpp", "cs", "rb", "kt", "swift"];
/// The lifetime counter, in the core's state folder.
const STATS_FILE: &str = "token-saver.json";
/// A terminal's running total of saved tokens.
const SAVED_LABEL: &str = "h0x.saved";
/// The most the label update may delay the hook's exit.
const LABEL_WAIT: Duration = Duration::from_millis(200);

/// What the Token Saver has saved on this machine (this session's state folder), for good.
#[derive(Serialize, Deserialize, JsonSchema, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct GateStats {
    /// Tokens saved: (bytes in - bytes shown) / 4, summed per replaced output.
    pub tokens_saved: u64,
    /// Outputs replaced.
    pub outputs: u64,
    /// Bytes the tools produced for those outputs.
    pub bytes_in: u64,
    /// Bytes shown to the agent instead.
    pub bytes_shown: u64,
}

/// The counter: zeros when the file is missing or broken.
pub fn read_stats(state: &Path) -> GateStats {
    let read = std::fs::read(state.join(STATS_FILE)).ok();
    let object = read.and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object);
    object.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

/// Adds one replaced output. Two hooks racing may lose an update; that is accepted over a lock.
fn add_stats(state: &Path, bytes_in: usize, shown: usize) {
    let mut s = read_stats(state);
    s.tokens_saved = s.tokens_saved.saturating_add((bytes_in.saturating_sub(shown) / 4) as u64);
    s.outputs = s.outputs.saturating_add(1);
    s.bytes_in = s.bytes_in.saturating_add(bytes_in as u64);
    s.bytes_shown = s.bytes_shown.saturating_add(shown as u64);
    let _ = std::fs::create_dir_all(state);
    let _ = write_unique(&state.join(STATS_FILE), &serde_json::to_vec(&s).unwrap_or_default());
}

/// Swaps `bytes` in whole through a temp file no other process shares, so racing hooks never
/// rename each other's half-written file.
fn write_unique(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = path.with_extension(format!("{}.{nanos}.tmp", std::process::id()));
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Raises the terminal's `h0x.saved` label by `saved`; nothing happens when no core listens.
fn raise_label(paths: &Paths, terminal: &str, saved: u64) -> Option<()> {
    let stream = crate::client::connect_existing(paths)?;
    let now = crate::client::call(&stream, "terminal.get", json!({"terminal": terminal})).ok()?;
    let total = now["labels"][SAVED_LABEL].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let labels = json!({(SAVED_LABEL): total.saturating_add(saved).to_string()});
    crate::client::call(&stream, "terminal.label", json!({"terminal": terminal, "labels": labels})).ok()?;
    Some(())
}

/// Books a replaced output after its answer is out.
fn book(bytes_in: usize, shown: usize) {
    let Ok(paths) = Paths::from_env() else { return };
    add_stats(&paths.state, bytes_in, shown);
    let Some(terminal) = crate::client::terminal_id().filter(|t| !t.is_empty()) else { return };
    let saved = (bytes_in.saturating_sub(shown) / 4) as u64;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(raise_label(&paths, &terminal, saved));
    });
    let _ = rx.recv_timeout(LABEL_WAIT); // a stuck core must not hold the agent's tool call
}

/// The hook: stdin in, the replacement (if any) on stdout. Always 0.
pub fn run() -> i32 {
    let mut raw = Vec::new();
    if std::io::stdin().read_to_end(&mut raw).is_err() {
        return 0;
    }
    std::panic::set_hook(Box::new(|_| {})); // a hook never talks to the agent: pass through quietly
    let out = std::panic::catch_unwind(|| String::from_utf8(raw).ok().and_then(|s| replace(&s)));
    if let Ok(Some((text, bytes_in, shown))) = out {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
        drop(stdout);
        let _ = std::panic::catch_unwind(|| book(bytes_in, shown));
    }
    0
}

/// The hook's stdout for one event, or None to leave the tool output alone.
#[cfg(test)]
fn process(input: &str) -> Option<String> {
    replace(input).map(|(text, _, _)| text)
}

/// The hook's stdout, the bytes the tool produced and the bytes shown instead.
fn replace(input: &str) -> Option<(String, usize, usize)> {
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
    let size = shown.len();
    let updated = replace_text(resp, shown);
    let out = json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": updated}});
    Some((out.to_string(), bytes, size))
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
                return None; // an image, a notebook, a file list: nothing to rewrite
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
            let how = if tsv { "TSV; empty cell = key absent, \"quoted\" cell = JSON string" } else { "compact JSON" };
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
        if let Some(t) = tsv(v) {
            return (t, true);
        }
    } else if let Value::Object(m) = v
        && let Some(k) = m.keys().find(|k| rows(&m[*k]))
        && let Some(t) = tsv(&m[k])
    {
        let mut rest = m.clone();
        rest.remove(k);
        return (format!("{}\n{k} (TSV):\n{t}", Value::Object(rest)), true);
    }
    (v.to_string(), false)
}

/// One cell: numbers, bools, null and arrays as JSON; a string as it is unless a reader could
/// take it for something else (empty, a JSON scalar, an object or array, a tab or newline), then
/// JSON-quoted. So 1 and "1", null and "null", {} and "{}", "" and an absent key all differ.
fn cell(v: &Value) -> String {
    let Value::String(s) = v else { return v.to_string() };
    let scalar = || serde_json::from_str::<Value>(s).is_ok_and(|p| !p.is_object() && !p.is_array());
    if s.is_empty() || s.starts_with(['{', '[']) || s.contains(['\t', '\n', '\r']) || scalar() {
        v.to_string()
    } else {
        s.clone()
    }
}

/// Cells of one row as (column name, key path, cell); nested objects get dotted names.
fn flat(prefix: &str, path: &str, m: &Map<String, Value>, out: &mut Vec<(String, String, String)>) {
    for (k, v) in m {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        let at = format!("{path}\u{0}{k}");
        match v {
            Value::Object(o) if !o.is_empty() => flat(&key, &at, o, out),
            _ => out.push((key, at, cell(v))),
        }
    }
}

/// Rows of objects as a header and one tab-separated line each; nested objects get dotted
/// names, arrays stay JSON, a missing key is an empty cell. None when two keys would share a
/// column (`a.b`, and `a` holding `b`) or a column name could not be written.
fn tsv(rows: &Value) -> Option<String> {
    let rows: Vec<Vec<(String, String, String)>> = rows
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .map(|m| {
            let mut cells = Vec::new();
            flat("", "", m, &mut cells);
            cells
        })
        .collect();
    let mut cols: Vec<&str> = Vec::new();
    let mut paths: HashMap<&str, &str> = HashMap::new();
    for (name, path, _) in rows.iter().flatten() {
        if name.contains(['\t', '\n', '\r']) {
            return None;
        }
        match paths.get(name.as_str()) {
            Some(p) if p != path => return None,
            Some(_) => {}
            None => {
                paths.insert(name, path);
                cols.push(name);
            }
        }
    }
    let mut lines = vec![cols.join("\t")];
    for row in &rows {
        let by: HashMap<&str, &str> = row.iter().map(|(k, _, v)| (k.as_str(), v.as_str())).collect();
        lines.push(cols.iter().map(|c| by.get(c).copied().unwrap_or("")).collect::<Vec<_>>().join("\t"));
    }
    Some(lines.join("\n"))
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

/// Replaces each run of chars matching `pred` that is at least `min` long (and, with `digit`,
/// holds a digit) with `#`.
fn hash_runs(s: &str, pred: fn(char) -> bool, min: usize, digit: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let flush = |out: &mut String, run: &str| {
        if run.chars().count() >= min && (!digit || run.bytes().any(|b| b.is_ascii_digit())) {
            out.push('#');
        } else {
            out.push_str(run);
        }
    };
    let mut start = None;
    for (i, c) in s.char_indices() {
        if pred(c) {
            start.get_or_insert(i);
        } else {
            if let Some(st) = start.take() {
                flush(&mut out, &s[st..i]);
            }
            out.push(c);
        }
    }
    if let Some(st) = start {
        flush(&mut out, &s[st..]);
    }
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
    let s = qualified_names(&uuids(line));
    let s = hash_runs(&s, |c| c.is_ascii_hexdigit(), 7, true);
    let s = hash_runs(&s, |c| c.is_ascii_digit(), 1, false);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Each 8-4-4-4-12 hex UUID becomes `#`.
fn uuids(s: &str) -> String {
    const DASHES: [usize; 4] = [8, 13, 18, 23];
    let b = s.as_bytes();
    let at = |i: usize| {
        i + 36 <= b.len()
            && (0..36).all(|k| if DASHES.contains(&k) { b[i + k] == b'-' } else { b[i + k].is_ascii_hexdigit() })
    };
    let mut out = String::with_capacity(s.len());
    let (mut last, mut i) = (0, 0);
    while i < b.len() {
        if b[i].is_ascii_hexdigit() && at(i) {
            out.push_str(&s[last..i]);
            out.push('#');
            i += 36;
            last = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// Whether `word` occurs not preceded by a word char and followed by one of `ends` and then a
/// non-word char (one pass; a match is only looked at when its start is a boundary).
fn has_word(hay: &str, word: &str, ends: &[&str]) -> bool {
    hay.match_indices(word).any(|(i, _)| {
        if hay[..i].chars().next_back().is_some_and(is_word) {
            return false;
        }
        let rest = &hay[i + word.len()..];
        let end = rest.find(|c| !is_word(c)).unwrap_or(rest.len());
        ends.contains(&&rest[..end])
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
fn groups(skels: &[String]) -> HashMap<&str, Vec<usize>> {
    let mut g: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, k) in skels.iter().enumerate() {
        if !k.is_empty() {
            g.entry(k).or_default().push(i);
        }
    }
    g
}

/// Lines alike except for numbers are shown once with `[xN alike]`; key lines fold only from
/// 5 alike and show the last one.
fn fold_keep(s: &str) -> String {
    let parts: Vec<String> = sections(s)
        .into_iter()
        .map(|Section { head, body }| {
            let skels: Vec<String> = body.iter().map(|l| skel(l)).collect();
            let g = groups(&skels);
            let mut out: Vec<String> = Vec::new();
            for (i, l) in body.iter().enumerate() {
                if skels[i].is_empty() {
                    continue;
                }
                let g = &g[skels[i].as_str()];
                let key = g.len() >= 3 && is_key(l); // under 3 alike nothing folds: skip the test
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
            let skels: Vec<String> = body.iter().map(|l| skel(l)).collect();
            let g = groups(&skels);
            // The key lines of the folded text, in order.
            let mut keys: Vec<String> = Vec::new();
            let mut size = 0;
            for (i, l) in body.iter().enumerate() {
                if size > per || skels[i].is_empty() || !is_summary_key(l) {
                    continue;
                }
                let g = &g[skels[i].as_str()];
                if g.len() < 3 {
                    keys.push(cut(l));
                } else if g[0] == i {
                    keys.push(cut(&format!("{l}  [x{} alike]", g.len())));
                } else {
                    continue;
                }
                size += keys[keys.len() - 1].len() + 1;
            }
            let tail: Vec<&&str> = body.iter().filter(|l| !l.trim().is_empty()).collect();
            let tail = &tail[tail.len().saturating_sub(4)..];
            let mut seen: HashSet<String> = HashSet::new();
            let mut txt: Vec<String> = Vec::new();
            let all = std::iter::once(head.to_string())
                .chain(keys)
                .chain(std::iter::once("…last lines:".to_string()))
                .chain(tail.iter().map(|l| cut(l)));
            for l in all.filter(|l| !l.is_empty()) {
                if seen.insert(l.clone()) {
                    txt.push(l);
                }
            }
            let joined = txt.join("\n");
            if joined.len() > per { format!("{}…", cut_bytes(&joined, per - 3)) } else { joined }
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
        assert_eq!(
            note,
            "[HyperOx Token Saver: JSON re-encoded losslessly as TSV; empty cell = key absent, \"quoted\" cell = JSON string]"
        );
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

    /// `f` on its own thread; fails if it panics or takes longer than `secs`.
    fn within<T: Send + 'static>(secs: u64, what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || drop(tx.send(f())));
        let out = rx.recv_timeout(std::time::Duration::from_secs(secs));
        out.unwrap_or_else(|e| panic!("{what}: not done within {secs} s, or panicked ({e})"))
    }

    /// Six non-hex letters per number, so lines built from it never fold.
    fn letters(i: usize) -> String {
        (0..6u32).map(|k| (b'g' + (i / 20usize.pow(k) % 20) as u8) as char).collect()
    }

    #[test]
    fn exactly_90_percent_passes_through_and_one_byte_more_is_replaced() {
        let note = "[HyperOx Token Saver: JSON re-encoded losslessly as compact JSON]\n";
        let mut fill = 900;
        let compact = loop {
            let c = json!({"k": "a".repeat(fill)}).to_string();
            if (note.len() + c.len()).is_multiple_of(9) {
                break c;
            }
            fill += 1;
        };
        let replacement = note.len() + compact.len();
        let at = replacement * 10 / 9;
        assert!(at > JSON_MIN && at <= LIMIT);
        let padded = |n: usize| format!("{compact}{}", " ".repeat(n - compact.len()));
        assert_eq!(process(&bash(&padded(at))), None, "a replacement of exactly 90% is not worth it");
        assert_eq!(shown(process(&bash(&padded(at + 1)))), format!("{note}{compact}"));
    }

    #[test]
    fn json_is_reencoded_only_above_1024_bytes() {
        let doc = json!({"name": "x".repeat(200), "list": [1, 2, 3]}).to_string();
        let padded = |n: usize| format!("{doc}{}", " ".repeat(n - doc.len()));
        assert_eq!(process(&bash(&padded(1024))), None);
        let want = format!("[HyperOx Token Saver: JSON re-encoded losslessly as compact JSON]\n{doc}");
        assert_eq!(shown(process(&bash(&padded(1025)))), want);
        let leading = format!("{}{doc}", " \r\n".repeat(400));
        assert_eq!(shown(process(&bash(&leading))), want, "leading blank lines do not hide the JSON");
        let rows: Vec<Value> = (0..2).map(|i| json!({"id": i, "pad": "p".repeat(200)})).collect();
        let rows = json!(rows).to_string();
        assert_eq!(process(&bash(&format!("{rows}{}", " ".repeat(1024 - rows.len())))), None, "rows too");
    }

    #[test]
    fn arrays_that_are_not_rows_stay_lossless_compact_json() {
        let big = "y".repeat(300);
        for v in [
            json!([{"a": 1, "s": big}, 2, "three", null, {"b": [big]}]),
            json!([[1, 2, big], [3, 4, big], [5, 6, big]]),
            json!([{"only": big, "row": {"n": 1, "s": big}}]),
            json!({"items": [{"a": big}], "more": big}),
            json!([{}, {}]),
            json!([]),
        ] {
            let pretty = serde_json::to_string_pretty(&v).unwrap();
            let doc = format!("{pretty}{}", " ".repeat(4000 - pretty.len()));
            let out = shown(process(&bash(&doc)));
            let (note, body) = out.split_once('\n').unwrap();
            if note.contains("TSV") {
                assert_eq!(v, json!([{}, {}]), "only rows become TSV: {out}");
                continue;
            }
            assert_eq!(note, "[HyperOx Token Saver: JSON re-encoded losslessly as compact JSON]", "{v}");
            assert_eq!(serde_json::from_str::<Value>(body).unwrap(), v, "lossless");
        }
    }

    #[test]
    fn tsv_does_not_read_an_empty_string_as_an_absent_key() {
        let rows: Vec<Value> =
            (0..40).map(|i| if i % 2 == 0 { json!({"name": "x", "note": ""}) } else { json!({"name": "x"}) }).collect();
        let doc = serde_json::to_string_pretty(&rows).unwrap();
        let out = shown(process(&bash(&doc)));
        let (note, body) = out.split_once('\n').unwrap();
        if note.contains("TSV") {
            let lines: Vec<&str> = body.split('\n').collect();
            assert_ne!(lines[1], lines[2], "{{\"note\":\"\"}} and no note read the same:\n{body}");
        } else {
            assert_eq!(serde_json::from_str::<Value>(body).unwrap(), json!(rows));
        }
    }

    #[test]
    fn tsv_does_not_drop_a_dotted_key_that_matches_a_nested_one() {
        let rows: Vec<Value> =
            (0..40).map(|i| json!({"id": i, "a.b": format!("flat{i}"), "a": {"b": format!("nested{i}")}})).collect();
        let doc = serde_json::to_string_pretty(&rows).unwrap();
        let out = shown(process(&bash(&doc)));
        for i in 0..40 {
            assert!(
                out.contains(&format!("flat{i}")) && out.contains(&format!("nested{i}")),
                "row {i} lost a value:\n{out}"
            );
        }
    }

    #[test]
    fn a_response_that_is_a_number_bool_or_null_passes_through() {
        for resp in [json!(42), json!(-1.5e300), json!(true), Value::Null, json!(""), json!({}), json!([])] {
            assert_eq!(process(&event("Bash", json!({}), resp.clone())), None, "{resp}");
        }
        assert_eq!(process(&json!({"tool_name": "Bash", "tool_input": {}}).to_string()), None, "no response");
        assert_eq!(process(&json!({"tool_name": 7, "tool_response": ticks(9000)}).to_string()), None);
        assert_eq!(process("null"), None);
        assert_eq!(process(&json!([ticks(9000)]).to_string()), None, "an event that is not an object");
    }

    #[test]
    fn a_read_whose_content_is_not_a_string_is_left_alone() {
        for file in [json!({"content": null}), json!({"content": 7}), json!({"content": ["a"]}), json!(7), Value::Null]
        {
            let ev = event("Read", json!({"file_path": "/a/b.rs"}), json!({"type": "text", "file": file}));
            assert_eq!(process(&ev), None, "{file}");
        }
    }

    #[test]
    fn a_response_without_a_text_field_keeps_its_shape_or_passes_through() {
        let lines: Vec<String> = (0..600).map(|i| format!("src/module_{i}/file.rs")).collect();
        let cases = [
            ("Read", json!({"type": "image", "file": {"base64": "A".repeat(20_000), "type": "image/png"}})),
            ("Read", json!({"type": "text", "file": {"filePath": "/a/b.rs", "content": lines}})),
            ("Grep", json!({"mode": "files_with_matches", "numFiles": 600, "filenames": lines})),
            ("Bash", json!({"stdout": null, "stderr": lines.join("\n")})),
        ];
        for (tool, resp) in cases {
            let Some(out) = process(&event(tool, json!({"file_path": "/a/b.png"}), resp.clone())) else { continue };
            let v = reply(Some(out));
            let u = &v["hookSpecificOutput"]["updatedToolOutput"];
            assert!(u.is_object(), "{tool} {}: an object became {u}", resp["type"]);
            for key in resp.as_object().unwrap().keys() {
                assert!(u.get(key).is_some(), "{tool}: {key} dropped");
            }
        }
    }

    #[test]
    fn stdout_without_stderr_gets_an_empty_stderr() {
        let tool = |resp: Value| {
            let v = reply(process(&event("Bash", json!({}), resp)));
            v["hookSpecificOutput"]["updatedToolOutput"].clone()
        };
        let u = tool(json!({"stdout": ticks(9000)}));
        assert!(u["stdout"].as_str().unwrap().contains("tick 0 done ok  [x"), "{u}");
        assert_eq!(u["stderr"], "");
        let u = tool(json!({"stdout": ticks(9000), "stderr": 5}));
        assert!(u["stdout"].as_str().unwrap().contains("tick 0 done ok  [x"), "{u}");
        assert_eq!(u["stderr"], "");
        let u = tool(json!({"stdout": "", "stderr": ticks(9000)}));
        assert!(u["stdout"].as_str().unwrap().contains("tick 0 done ok  [x"), "only stderr is big: {u}");
        assert_eq!(u["stderr"], "");
    }

    #[test]
    fn a_huge_single_line_is_cut_quickly() {
        for line in [
            "word ".repeat(200_000),
            "日".repeat(350_000),
            "a::".repeat(300_000),
            "7".repeat(1_000_000),
            format!("{{{}", "x".repeat(1_000_000)),
            "fail".repeat(250_000),
        ] {
            let what = format!("a 1 MB line of {:?}", cut_chars(&line, 4));
            let out = within(20, &what, move || process(&bash(&line)));
            let out = shown(out);
            let (note, body) = out.split_once('\n').unwrap();
            assert!(note.starts_with("[HyperOx Token Saver: output "), "{what}: {note}");
            assert!(body.len() <= BUDGET, "{what}: {} bytes", body.len());
        }
    }

    #[test]
    fn whitespace_only_lines_do_not_count_or_break_folding() {
        let blank = " \t \n\n   \r\n\t\t\n\u{3000}\n".repeat(1200);
        assert!(blank.len() > LIMIT);
        if let Some(out) = process(&bash(&blank)) {
            let out = shown(Some(out));
            let (_, body) = out.split_once('\n').unwrap();
            assert!(body.trim().is_empty(), "{body:?}");
        }
        let mut log = String::new();
        for i in 0..600 {
            log.push_str(&format!("tick {i} done ok\n   \n\t\r\n"));
        }
        let out = shown(process(&bash(&log)));
        assert!(out.contains("\ntick 0 done ok  [x600 alike]"), "{out}");
        assert_eq!(out.split('\n').count(), 2, "{out}");
    }

    #[test]
    fn a_cut_at_160_chars_keeps_emoji_and_cjk_whole() {
        let wide = "😀日".repeat(100);
        let mut log = String::new();
        for i in 0..6 {
            log.push_str(&format!("ERROR job {i} {wide}\n"));
        }
        log.push_str(&ticks(9000));
        let out = shown(process(&bash(&log)));
        let last = out.split("; last: ").nth(1).and_then(|r| r.split(']').next()).unwrap();
        assert_eq!(last.chars().count(), 160, "{last}");
        assert!(last.starts_with("ERROR job 5 😀日"), "{last}");

        let src = format!("pub fn f() -> &'static str {{ \"{}\" }}\n", "👍🏽語".repeat(100)).repeat(60);
        let ev = event("Read", json!({"file_path": "/w/a.rs"}), json!({"file": {"content": src}}));
        let out = shown(process(&ev));
        let first = out.split('\n').nth(1).unwrap();
        assert_eq!(first.strip_prefix("L1: ").unwrap().chars().count(), 160, "{first}");
        assert!(out.split_once('\n').unwrap().1.len() <= 3000);
    }

    #[test]
    fn a_windows_path_with_spaces_still_picks_the_file_kind() {
        let src = rust_file(20_000);
        let read = |path: &str| {
            let ev = event("Read", json!({"file_path": path}), json!({"file": {"filePath": path, "content": src}}));
            shown(process(&ev))
        };
        for code in [
            "C:\\Program Files\\My App\\src\\main.rs",
            "C:\\Users\\Jane Doe\\proj v1.2\\lib.Rs",
            "\\\\?\\D:\\with space\\x.RS",
            "\\\\server\\share name\\a b.py",
        ] {
            assert!(read(code).contains("declaration outline"), "{code}");
        }
        for text in [
            "C:\\Program Files\\My App.rs\\README",
            "C:\\dir with.dots\\file.",
            "C:\\x y\\a.rs.txt",
            "C:\\x y\\a.rs ",
            "",
        ] {
            assert!(!read(text).contains("declaration outline"), "{text}");
        }
    }

    #[test]
    fn grep_content_lines_fold_per_match_and_keep_files_apart() {
        let mut content = String::new();
        for i in 1..=200 {
            content.push_str(&format!("src/alpha.rs:{i}:    let value = compute(input);\n"));
            content.push_str(&format!("C:\\work dir\\beta.rs:{i}:    let value = compute(input);\n"));
        }
        let ev = event(
            "Grep",
            json!({"pattern": "compute", "output_mode": "content", "-n": true}),
            json!({"mode": "content", "numFiles": 2, "filenames": [], "content": content, "numLines": 400}),
        );
        let v = reply(process(&ev));
        let u = &v["hookSpecificOutput"]["updatedToolOutput"];
        assert_eq!(u["mode"], "content");
        assert_eq!(u["numFiles"], 2);
        let c = u["content"].as_str().unwrap();
        assert!(c.contains("\nsrc/alpha.rs:1: let value = compute(input);  [x200 alike]"), "{c}");
        assert!(c.contains("\nC:\\work dir\\beta.rs:1: let value = compute(input);  [x200 alike]"), "{c}");
    }

    #[test]
    fn deeply_nested_json_neither_overflows_nor_panics() {
        within(60, "10k levels of nesting", || {
            let arrays = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
            let objects = format!("{}1{}", "{\"a\":".repeat(10_000), "}".repeat(10_000));
            for deep in [arrays, objects] {
                let whole = format!("{{\"tool_name\":\"Bash\",\"tool_input\":{{}},\"tool_response\":{deep}}}");
                assert_eq!(process(&whole), None, "an event too deep to parse passes through");
                let out = shown(process(&bash(&deep)));
                assert!(out.starts_with("[HyperOx Token Saver: output "), "{out}");
            }
        });
    }

    #[test]
    fn a_20_mb_output_finishes_in_bounded_time() {
        let started = std::time::Instant::now();
        let out = within(180, "20 MB of alike lines", || process(&bash(&ticks(20 << 20))));
        assert!(shown(out).contains("tick 0 done ok  [x"));
        // Distinct lines that are all summary keys: only the section summary can shrink them.
        let out = within(180, "20 MB of distinct summary-key lines", || {
            let mut text = String::new();
            let mut i = 0;
            while text.len() < 20 << 20 {
                text.push_str(&format!("status of {} left unchanged\n", letters(i)));
                i += 1;
            }
            process(&bash(&text))
        });
        assert!(shown(out).contains("summary per section"));
        assert!(started.elapsed() < std::time::Duration::from_secs(240), "{:?}", started.elapsed());
    }

    #[test]
    fn an_output_of_only_key_lines_folds_from_five_and_not_from_four() {
        let mut log = String::new();
        for i in 0..400 {
            log.push_str(&format!("ERROR request {i} timed out after {}ms\n", i * 2));
        }
        for i in 0..4 {
            log.push_str(&format!("WARN disk {i} at 9{i} percent\n"));
        }
        for i in 0..5 {
            log.push_str(&format!("FATAL node {i} lost\n"));
        }
        let out = shown(process(&bash(&log)));
        assert!(
            out.contains(
                "\nERROR request 0 timed out after 0ms  [x400 alike; last: ERROR request 399 timed out after 798ms]"
            ),
            "{out}"
        );
        for i in 0..4 {
            assert!(out.contains(&format!("\nWARN disk {i} at 9{i} percent\n")), "4 alike key lines stay: {out}");
        }
        assert!(out.ends_with("\nFATAL node 0 lost  [x5 alike; last: FATAL node 4 lost]"), "{out}");

        let mut log = ticks(9000);
        log.push_str("\ncopied 0 files\ncopied 1 files\nmoved 0 files\nmoved 1 files\nmoved 2 files\n");
        let out = shown(process(&bash(&log)));
        assert!(out.contains("\ncopied 0 files\ncopied 1 files\n"), "2 alike stay: {out}");
        assert!(out.ends_with("\nmoved 0 files  [x3 alike]"), "{out}");
    }

    #[test]
    fn the_same_id_on_two_paths_stays_apart_and_hex_ids_fold() {
        assert_ne!(skel("GET /api/users/17"), skel("GET /api/cart/17"));
        assert_eq!(skel("commit 3f2a9c1 merged"), skel("commit a1b2c3d merged"));
        assert_eq!(skel("id DEADBEEF00 ok"), skel("id 0badf00d11 ok"));
        assert_ne!(skel("tag abcdef ok"), skel("tag fedcba ok"), "six hex letters can be a word");
        let mut log = String::new();
        for i in 0..150u64 {
            log.push_str(&format!("GET /api/users/17 200 in {i}ms\n"));
            log.push_str(&format!("GET /api/cart/17 200 in {i}ms\n"));
            log.push_str(&format!("stored object {:010x} in bucket main\n", i * 2_654_435_761));
        }
        let out = shown(process(&bash(&log)));
        assert!(out.contains("\nGET /api/users/17 200 in 0ms  [x150 alike]"), "{out}");
        assert!(out.contains("\nGET /api/cart/17 200 in 0ms  [x150 alike]"), "{out}");
        assert!(out.contains("\nstored object 0000000000 in bucket main  [x150 alike]"), "{out}");
    }

    #[test]
    fn uuids_are_hex_ids_and_fold() {
        assert_eq!(
            skel("req f47ac10b-58cc-4372-a567-0e02b2c3d479 ok"),
            skel("req 550e8400-e29b-41d4-a716-446655440000 ok")
        );
    }

    #[test]
    fn words_made_of_hex_letters_are_not_ids() {
        assert_ne!(skel("file defaced here"), skel("file effaced here"));
        assert_eq!(skel("file defaced here"), "file defaced here");
        assert_eq!(skel("id deadbeef01 x"), skel("id cafebabe99 x"), "ids with a digit fold");
        assert_eq!(skel("id deadbeef01 x"), "id # x");
        let mut log = String::new();
        for i in 0..80 {
            log.push_str(&format!("blob {:08x}{i} stored on disk node number\n", 0xabcdef12u32 + i));
            log.push_str("object defaced rebuilt on disk node number\nobject effaced rebuilt on disk node number\n");
        }
        let out = shown(process(&bash(&log)));
        assert!(out.contains("blob abcdef120 stored on disk node number  [x80 alike]"), "{out}");
        assert!(out.contains("object defaced rebuilt on disk node number  [x80 alike]"), "{out}");
        assert!(out.contains("object effaced rebuilt on disk node number  [x80 alike]"), "{out}");
    }

    #[test]
    fn a_wide_character_summary_stays_inside_the_budget_in_bytes() {
        for unit in ["日本語のエラー状態", "🙂status🙂"] {
            let mut text = String::new();
            for i in 0..3000 {
                text.push_str(&format!("{unit}{} status {}\n", letters(i), unit.repeat(8)));
            }
            let out = shown(process(&bash(&text)));
            let (note, body) = out.split_once('\n').unwrap();
            assert!(note.contains("summary per section"), "{note}");
            assert!(body.len() <= BUDGET, "{unit}: {} bytes", body.len());
        }
    }

    /// Reads a TSV cell back: a quoted cell is a JSON string, an empty one is an absent key.
    fn untsv(cell: &str) -> Option<Value> {
        if cell.is_empty() {
            return None;
        }
        match serde_json::from_str::<Value>(cell) {
            Ok(v) if !v.is_string() || cell.starts_with('"') => Some(v),
            _ => Some(json!(cell)),
        }
    }

    #[test]
    fn tsv_keeps_one_and_a_string_one_apart() {
        let values = [
            json!(1),
            json!("1"),
            Value::Null,
            json!("null"),
            json!({}),
            json!("{}"),
            json!(""),
            json!("true"),
            json!("plain"),
            json!([1, "a"]),
            json!("[1]"),
            json!("a\tb"),
            json!(false),
            json!("\"q\""),
        ];
        let mut rows: Vec<Value> =
            values.iter().enumerate().map(|(i, v)| json!({"id": i, "pad": "padding ".repeat(8), "v": v})).collect();
        rows.push(json!({"id": 99, "pad": "padding ".repeat(8)}));
        let doc = serde_json::to_string_pretty(&rows).unwrap();
        let out = shown(process(&bash(&doc)));
        let (note, body) = out.split_once('\n').unwrap();
        assert!(note.contains("TSV; empty cell = key absent, \"quoted\" cell = JSON string"), "{note}");
        let lines: Vec<&str> = body.split('\n').collect();
        let col = lines[0].split('\t').position(|c| c == "v").unwrap();
        for (i, want) in values.iter().enumerate() {
            let cell = lines[i + 1].split('\t').nth(col).unwrap();
            assert_eq!(untsv(cell).as_ref(), Some(want), "row {i}: cell {cell:?}");
        }
        assert_eq!(untsv(lines[values.len() + 1].split('\t').nth(col).unwrap()), None, "absent key");
    }
}
