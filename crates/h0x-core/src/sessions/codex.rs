//! Codex CLI: `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`,
//! whose first line is the `session_meta` with `id` and `cwd`. Names given
//! to sessions are lines `{id, thread_name}` in `session_index.jsonl`.

use super::*;
use std::collections::HashMap;
use std::io::BufRead;

/// The first line holds Codex's whole system prompt (~20 KB).
const FIRST_LINE: u64 = 256 * 1024;
/// The end of the name index read (later lines win).
const INDEX: u64 = 1024 * 1024;

pub(super) fn list(home: &Path, want: &str) -> Vec<SessionInfo> {
    let mut found: Vec<SessionInfo> = dirs(&home.join("sessions"))
        .flat_map(|year| dirs(&year))
        .flat_map(|month| dirs(&month))
        .flat_map(|day| files(&day, "jsonl"))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-")))
        .filter_map(|path| read(&path, want))
        .collect();
    if !found.is_empty() {
        let names = names(&home.join("session_index.jsonl"));
        for s in &mut found {
            s.title = names.get(&s.id).cloned();
        }
    }
    found
}

fn read(path: &Path, want: &str) -> Option<SessionInfo> {
    let (file, meta) = open(path)?;
    let mut line = Vec::new();
    if let Err(e) = std::io::BufReader::new(file.take(FIRST_LINE)).read_until(b'\n', &mut line) {
        skipped(path, e);
        return None;
    }
    let first: serde_json::Value = match serde_json::from_slice(&line) {
        Ok(first) => first,
        Err(e) => {
            skipped(path, format!("first line: {e}"));
            return None;
        }
    };
    let (Some(id), Some(cwd)) = (first["payload"]["id"].as_str(), first["payload"]["cwd"].as_str()) else {
        skipped(path, "no session_meta id and cwd");
        return None;
    };
    (first["type"] == "session_meta" && normal(cwd) == want).then(|| SessionInfo {
        agent: "codex".into(),
        id: id.into(),
        title: None,
        last_used: modified_ms(&meta),
        path: path.display().to_string(),
    })
}

/// Session names by id.
fn names(index: &Path) -> HashMap<String, String> {
    let Some((mut file, meta)) = open(index) else { return HashMap::new() };
    let lines = tail(index, &mut file, meta.len(), INDEX).map(|bytes| json_lines(&bytes)).unwrap_or_default();
    lines.iter().filter_map(|l| Some((l["id"].as_str()?.to_string(), title(l["thread_name"].as_str()?)?))).collect()
}
