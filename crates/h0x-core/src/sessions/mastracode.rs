//! Mastra Code: the `mastra_threads` table of `mastra.db` (LibSQL, which is
//! SQLite) in the app data folder `mastracode`. A thread's folder is
//! `metadata.projectPath`; its time is `updatedAt` (ISO text). Opened
//! read-only through OpenCode's connection.

use super::*;

/// Threads read from the database per folder.
const ROWS: usize = 500;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

/// macOS `~/Library/Application Support/mastracode`, Windows `%APPDATA%\mastracode`,
/// else `$XDG_DATA_HOME|~/.local/share/mastracode`.
pub(super) fn root(env: Env) -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        let home = var(env, "HOME").or_else(|| var(env, "USERPROFILE"))?;
        home.join("Library").join("Application Support")
    } else if cfg!(windows) {
        or_home(env, "APPDATA", &["AppData", "Roaming"])?
    } else {
        data_home(env)?
    };
    Some(base.join("mastracode"))
}

pub(super) fn list(data: &Path, want: &str) -> Vec<SessionInfo> {
    let db = data.join("mastra.db");
    // A real file in the store, never a link out of it.
    if !std::fs::symlink_metadata(&db).is_ok_and(|m| m.is_file()) {
        return Vec::new();
    }
    query(&db, want).unwrap_or_else(|e| {
        skipped(&db, e);
        Vec::new()
    })
}

fn query(db: &Path, want: &str) -> rusqlite::Result<Vec<SessionInfo>> {
    let conn = opencode::connect(db)?;
    // Paths are stored as Mastra Code got them: compare folders here, by our rules.
    let mut folders = conn.prepare(
        "SELECT DISTINCT json_extract(metadata, '$.projectPath') FROM mastra_threads \
         WHERE json_valid(metadata) AND json_type(metadata, '$.projectPath') = 'text'",
    )?;
    let folders: Vec<String> = folders
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(Result::ok)
        .filter(|d| normal(d) == want)
        .collect();
    let mut rows = conn.prepare(&format!(
        "SELECT id, title, CAST(ROUND((julianday(updatedAt) - 2440587.5) * 86400000) AS INTEGER) \
         FROM mastra_threads WHERE json_valid(metadata) AND json_extract(metadata, '$.projectPath') = ?1 \
         ORDER BY updatedAt DESC LIMIT {ROWS}"
    ))?;
    let mut found = Vec::new();
    for folder in &folders {
        let threads = rows.query_map([folder], |row| {
            Ok(SessionInfo {
                agent: "mastracode".into(),
                id: row.get(0)?,
                title: row.get::<_, Option<String>>(1)?.as_deref().and_then(title),
                last_used: row.get::<_, Option<i64>>(2)?.map_or(0, |ms| ms.max(0) as u64),
                path: db.display().to_string(),
            })
        })?;
        found.extend(threads.filter_map(Result::ok));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::Scratch;
    use rusqlite::Connection;

    const TABLE: &str = "CREATE TABLE mastra_threads (id TEXT PRIMARY KEY, resourceId TEXT NOT NULL, \
                         title TEXT NOT NULL, metadata TEXT, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL)";

    fn db(dir: &Path) -> Connection {
        std::fs::create_dir_all(dir).unwrap();
        let conn = Connection::open(dir.join("mastra.db")).unwrap();
        conn.execute(TABLE, []).unwrap();
        conn
    }

    fn add(conn: &Connection, id: &str, metadata: Option<&str>, updated: &str) {
        conn.execute(
            "INSERT INTO mastra_threads VALUES (?1, 'r', 'a title', ?2, '2026-01-01T00:00:00.000Z', ?3)",
            (id, metadata, updated),
        )
        .unwrap();
    }

    fn project(path: &str) -> String {
        serde_json::json!({ "projectPath": path }).to_string()
    }

    #[test]
    fn root_follows_the_platform() {
        let env = |n: &str| match n {
            "XDG_DATA_HOME" => Some("/data".to_string()),
            "APPDATA" => Some("/roaming".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        let expect = if cfg!(target_os = "macos") {
            Path::new("/home/u").join("Library").join("Application Support")
        } else if cfg!(windows) {
            PathBuf::from("/roaming")
        } else {
            PathBuf::from("/data")
        };
        assert_eq!(root(&env), Some(expect.join("mastracode")));
        assert_eq!(root(&|_| None), None);
    }

    #[test]
    fn missing_or_corrupt_store_lists_nothing() {
        let h = Scratch::new();
        assert!(list(h.path(), "/w").is_empty());
        h.put("mastra.db", b"this is not sqlite", 1);
        assert!(list(h.path(), "/w").is_empty());
        let empty = Scratch::new();
        db(empty.path());
        assert!(list(empty.path(), "/w").is_empty());
        // A database of another program: no such table.
        let other = Scratch::new();
        Connection::open(other.path().join("mastra.db")).unwrap().execute("CREATE TABLE x (a)", []).unwrap();
        assert!(list(other.path(), "/w").is_empty());
    }

    #[test]
    fn folder_match_newest_first_and_bad_rows_left_out() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "t_old", Some(&project("/w")), "2026-03-01T10:00:00.000Z");
        add(&conn, "t_new", Some(&project("/w")), "2026-03-02T10:00:00.500Z");
        add(&conn, "t_other", Some(&project("/elsewhere")), "2026-03-03T10:00:00.000Z");
        add(&conn, "t_nometa", None, "2026-03-04T10:00:00.000Z");
        add(&conn, "t_broken", Some("{not json"), "2026-03-04T10:00:00.000Z");
        add(&conn, "t_number", Some(r#"{"projectPath": 5}"#), "2026-03-04T10:00:00.000Z");
        add(&conn, "t_nopath", Some("{}"), "2026-03-04T10:00:00.000Z");
        drop(conn);
        let mut found = list(h.path(), "/w");
        found.sort_by_key(|s| std::cmp::Reverse(s.last_used));
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["t_new", "t_old"]);
        assert!(found.iter().all(|s| s.agent == "mastracode" && s.title.as_deref() == Some("a title")));
        // 2026-03-02T10:00:00.500Z
        assert_eq!(found[0].last_used, 1_772_445_600_500);
    }

    #[test]
    fn unreadable_time_is_zero_not_a_failure() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "t_bad", Some(&project("/w")), "yesterday");
        drop(conn);
        let found = list(h.path(), "/w");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].last_used, 0);
    }

    #[test]
    fn windows_drive_case_matches() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "t_w", Some(&project(r"C:\Work\App")), "2026-03-01T10:00:00.000Z");
        drop(conn);
        assert_eq!(list(h.path(), &normal("c:/work/app/")).len(), 1);
        assert!(list(h.path(), &normal(r"C:\Work\Other")).is_empty());
    }

    /// Every file in `dir` with its length and mtime.
    fn snapshot(dir: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
        let mut all: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let meta = e.metadata().unwrap();
                (e.file_name().to_string_lossy().into_owned(), meta.len(), meta.modified().unwrap())
            })
            .collect();
        all.sort();
        all
    }

    #[test]
    fn finder_edge_no_database_is_never_created() {
        let h = Scratch::new();
        assert!(list(h.path(), "/w").is_empty());
        assert!(snapshot(h.path()).is_empty());
        let file = h.put("data-file", b"x", 1);
        assert!(list(&file, "/w").is_empty(), "the data folder is a file");
        std::fs::create_dir_all(h.path().join("d/mastra.db")).unwrap();
        assert!(list(&h.path().join("d"), "/w").is_empty(), "mastra.db is a folder");
    }

    #[test]
    fn finder_edge_not_sqlite_empty_or_locked_is_left_untouched() {
        let h = Scratch::new();
        h.put("mastra.db", b"SQLite format 3\0 then garbage", 1);
        let before = snapshot(h.path());
        assert!(list(h.path(), "/w").is_empty());
        assert_eq!(snapshot(h.path()), before);
        let e = Scratch::new();
        e.put("mastra.db", b"", 1);
        assert!(list(e.path(), "/w").is_empty());
        assert_eq!(snapshot(e.path()).len(), 1);
        let l = Scratch::new();
        let conn = db(l.path());
        add(&conn, "t1", Some(&project("/w")), "2026-03-01T10:00:00.000Z");
        conn.execute_batch("BEGIN EXCLUSIVE; UPDATE mastra_threads SET title = 'x';").unwrap();
        let start = std::time::Instant::now();
        let found = list(l.path(), "/w");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert!(found.iter().all(|s| s.id == "t1" && s.title.as_deref() == Some("a title")));
        conn.execute_batch("ROLLBACK").unwrap();
        assert_eq!(list(l.path(), "/w").len(), 1);
    }

    #[test]
    fn finder_edge_reading_never_modifies_the_database() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "t1", Some(&project("/w")), "2026-03-01T10:00:00.000Z");
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)).unwrap();
        drop(conn);
        let before = snapshot(h.path());
        let bytes = std::fs::read(h.path().join("mastra.db")).unwrap();
        assert_eq!(list(h.path(), "/w").len(), 1);
        assert_eq!(snapshot(h.path()), before, "no -wal or -shm left behind, db untouched");
        assert_eq!(std::fs::read(h.path().join("mastra.db")).unwrap(), bytes);
    }

    #[test]
    fn finder_edge_project_paths_with_slashes_dots_and_prefixes() {
        let h = Scratch::new();
        let conn = db(h.path());
        let t = "2026-03-01T10:00:00.000Z";
        add(&conn, "t_a", Some(&project("/w/")), t);
        add(&conn, "t_b", Some(&project("/w/x/..")), t);
        add(&conn, "t_c", Some(&project("/w/x")), t);
        add(&conn, "t_d", Some(&project(r"\\?\UNC\Srv\Share\P")), t);
        add(&conn, "t_e", Some(&project(r"\\?\C:\Work\App\")), t);
        add(&conn, "t_f", Some(r#"{"projectPath":"/w","other":1}"#), t);
        drop(conn);
        let ids = |want: &str| {
            let mut ids: Vec<String> = list(h.path(), &normal(want)).into_iter().map(|s| s.id).collect();
            ids.sort();
            ids
        };
        assert_eq!(ids("/w"), ["t_a", "t_b", "t_f"]);
        assert_eq!(ids(r"\\srv\share\p"), ["t_d"]);
        assert_eq!(ids("c:/work/app/"), ["t_e"]);
    }

    #[test]
    fn finder_edge_time_is_updated_at_whatever_the_mtime() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "t_epoch", Some(&project("/w")), "1970-01-01T00:00:00.000Z");
        add(&conn, "t_before", Some(&project("/w")), "1969-12-31T23:59:59.000Z");
        add(&conn, "t_z", Some(&project("/w")), "2026-03-02T10:00:00Z");
        drop(conn);
        let mut found = list(h.path(), "/w");
        found.sort_by(|a, b| a.id.cmp(&b.id));
        let got: Vec<(&str, u64)> = found.iter().map(|s| (s.id.as_str(), s.last_used)).collect();
        assert_eq!(got, [("t_before", 0), ("t_epoch", 0), ("t_z", 1_772_445_600_000)]);
    }

    #[test]
    fn finder_edge_empty_env_falls_back_to_home() {
        let env = |n: &str| match n {
            "XDG_DATA_HOME" | "APPDATA" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        let expect = if cfg!(target_os = "macos") {
            Path::new("/h").join("Library").join("Application Support")
        } else if cfg!(windows) {
            Path::new("/h").join("AppData").join("Roaming")
        } else {
            Path::new("/h").join(".local").join("share")
        };
        assert_eq!(root(&env), Some(expect.join("mastracode")));
    }
}
