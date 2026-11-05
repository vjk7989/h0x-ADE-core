//! OpenCode: since 1.x, the `session` table of `<data>/opencode/opencode.db`
//! (SQLite, opened read-only); before, JSON storage:
//! `<data>/opencode/storage/session/<project id>/<session id>.json`, each
//! `{id, directory, title, parentID?, time: {updated}}`.

use super::*;
use rusqlite::{Connection, OpenFlags};
use std::time::Duration;

/// A session file is a small JSON object.
const MAX: u64 = 64 * 1024;
/// Sessions read from the database per folder.
const ROWS: usize = 500;
/// How long a write by a running OpenCode may hold us up.
const BUSY: Duration = Duration::from_millis(250);
/// Root sessions still listed: a child (a subagent's) is resumed through its parent.
const ROOT: &str = "parent_id IS NULL AND (time_archived IS NULL OR time_archived = 0)";

pub(super) fn list(data: &Path, want: &str) -> Vec<SessionInfo> {
    let mut found = from_db(&data.join("opencode.db"), want);
    let storage = data.join("storage").join("session");
    let older = dirs(&storage).flat_map(|project| files(&project, "json")).filter_map(|p| read(&p, want));
    for s in older {
        if !found.iter().any(|f| f.id == s.id) {
            found.push(s);
        }
    }
    found
}

/// The database's sessions in the folder; none when it is missing, locked or unreadable.
fn from_db(db: &Path, want: &str) -> Vec<SessionInfo> {
    // A real file in the store, never a link out of it.
    if !std::fs::symlink_metadata(db).is_ok_and(|m| m.is_file()) {
        return Vec::new();
    }
    query(db, want).unwrap_or_else(|e| {
        skipped(db, e);
        Vec::new()
    })
}

/// Whether the database holds session `id` (one that cannot be read holds none).
pub(super) fn in_db(db: &Path, id: &str) -> bool {
    std::fs::symlink_metadata(db).is_ok_and(|m| m.is_file())
        && connect(db).and_then(|conn| conn.query_row("SELECT 1 FROM session WHERE id = ?1", [id], |_| Ok(()))).is_ok()
}

fn connect(db: &Path) -> rusqlite::Result<Connection> {
    // Read-only, never created: OpenCode's database is its own.
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(uri(db), flags)?;
    conn.busy_timeout(BUSY)?;
    Ok(conn)
}

fn query(db: &Path, want: &str) -> rusqlite::Result<Vec<SessionInfo>> {
    let conn = connect(db)?;
    // Paths are stored as OpenCode got them: compare folders here, by our rules.
    let mut folders = conn.prepare(&format!("SELECT DISTINCT directory FROM session WHERE {ROOT}"))?;
    let folders: Vec<String> = folders
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(Result::ok)
        .filter(|d| normal(d) == want)
        .collect();
    let mut rows = conn.prepare(&format!(
        "SELECT id, title, time_updated, directory FROM session WHERE {ROOT} AND directory = ?1 \
         ORDER BY time_updated DESC LIMIT {ROWS}"
    ))?;
    let mut found = Vec::new();
    for folder in &folders {
        let sessions = rows.query_map([folder], |row| {
            Ok(SessionInfo {
                agent: "opencode".into(),
                id: row.get(0)?,
                title: row.get::<_, Option<String>>(1)?.as_deref().and_then(title),
                last_used: row.get::<_, Option<i64>>(2)?.map_or(0, |ms| ms.max(0) as u64),
                path: db.display().to_string(),
            })
        })?;
        found.extend(sessions.filter_map(Result::ok));
    }
    Ok(found)
}

/// The database as an SQLite URI (`file:///C:/x/opencode.db` on Windows), so `mode=ro` holds.
fn uri(db: &Path) -> String {
    let path = db.display().to_string().replace('\\', "/");
    let mut out = String::from("file://");
    if !path.starts_with('/') {
        out.push('/');
    }
    for c in path.chars() {
        match c {
            '%' | '?' | '#' => out.push_str(&format!("%{:02X}", c as u32)),
            _ => out.push(c),
        }
    }
    out + "?mode=ro"
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
