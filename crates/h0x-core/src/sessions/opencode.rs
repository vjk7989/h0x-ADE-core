//! OpenCode's JSON storage: `<data>/opencode/storage/session/<project id>/<session id>.json`,
//! each `{id, directory, title, parentID?, time: {updated}}`. OpenCode 1.x
//! moved sessions into its SQLite database (`opencode.db`), not read here.

use super::*;

/// A session file is a small JSON object.
const MAX: u64 = 64 * 1024;

pub(super) fn list(storage: &Path, want: &str) -> Vec<SessionInfo> {
    dirs(&storage.join("session")).flat_map(|project| files(&project, "json")).filter_map(|p| read(&p, want)).collect()
}

fn read(path: &Path, want: &str) -> Option<SessionInfo> {
    let (mut file, meta) = open(path)?;
    if meta.len() > MAX {
        skipped(path, "too big for a session");
        return None;
    }
    let s: serde_json::Value = match serde_json::from_slice(&head(path, &mut file, MAX)?) {
        Ok(s) => s,
        Err(e) => {
            skipped(path, e);
            return None;
        }
    };
    let (Some(id), Some(directory)) = (s["id"].as_str(), s["directory"].as_str()) else {
        skipped(path, "no id and directory");
        return None;
    };
    // A child session (a subagent's) is resumed through its parent.
    if !s["parentID"].is_null() || normal(directory) != want {
        return None;
    }
    Some(SessionInfo {
        agent: "opencode".into(),
        id: id.into(),
        title: s["title"].as_str().and_then(title),
        last_used: s["time"]["updated"].as_u64().unwrap_or_else(|| modified_ms(&meta)),
        path: path.display().to_string(),
    })
}
