//! Hermes Agent 0.19.0 (read from its published wheel, `hermes_state.py`, schema 22): the
//! `sessions` table of `<home>/state.db`, opened read-only. The home is `$HERMES_HOME`, else
//! `%LOCALAPPDATA%\hermes` on Windows, else `~/.hermes`. A session's `cwd` is the folder it ran in
//! (it may be empty); times are seconds. A row with a `parent_session_id` continues another
//! session and is left out, as OpenCode's children are.

use super::*;

/// Sessions read per folder.
const ROWS: usize = 500;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    var(env, "HERMES_HOME").or_else(|| {
        if cfg!(windows) {
            return var(env, "LOCALAPPDATA")
                .or_else(|| or_home(env, "", &["AppData", "Local"]))
                .map(|d| d.join("hermes"));
        }
        or_home(env, "", &[".hermes"])
    })
}

pub(super) fn list(home: &Path, want: &str) -> Vec<SessionInfo> {
    let db = home.join("state.db");
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
    // Paths are stored as Hermes got them: compare folders here, by our rules.
    let mut folders =
        conn.prepare("SELECT DISTINCT cwd FROM sessions WHERE parent_session_id IS NULL AND typeof(cwd) = 'text'")?;
    let folders: Vec<String> = folders
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(Result::ok)
        .filter(|d| !d.is_empty() && normal(d) == want)
        .collect();
    let mut rows = conn.prepare(&format!(
        "SELECT id, title, CAST(COALESCE(ended_at, started_at) * 1000 AS INTEGER) FROM sessions          WHERE parent_session_id IS NULL AND cwd = ?1 ORDER BY COALESCE(ended_at, started_at) DESC LIMIT {ROWS}"
    ))?;
    let mut found = Vec::new();
    for folder in &folders {
        let sessions = rows.query_map([folder], |row| {
            Ok(SessionInfo {
                agent: "hermes".into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::Scratch;
    use rusqlite::Connection;

    /// The columns of Hermes 0.19.0's `sessions` table that the finder reads, in its types.
    const TABLE: &str = "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, parent_session_id TEXT,                          started_at REAL NOT NULL, ended_at REAL, cwd TEXT, title TEXT)";

    fn db(dir: &Path) -> Connection {
        let conn = Connection::open(dir.join("state.db")).unwrap();
        conn.execute(TABLE, []).unwrap();
        conn
    }

    fn add(conn: &Connection, id: &str, parent: Option<&str>, started: f64, ended: Option<f64>, cwd: Option<&str>) {
        conn.execute(
            "INSERT INTO sessions (id, source, parent_session_id, started_at, ended_at, cwd, title)              VALUES (?1, 'cli', ?2, ?3, ?4, ?5, 'a title')",
            rusqlite::params![id, parent, started, ended, cwd],
        )
        .unwrap();
    }

    #[test]
    fn root_from_hermes_home_or_the_platform_default() {
        let own = |n: &str| (n == "HERMES_HOME").then(|| "/hh".to_string());
        assert_eq!(root(&own), Some(PathBuf::from("/hh")));
        let home = |n: &str| (n == "HOME" || n == "USERPROFILE").then(|| "/home/u".to_string());
        let want = if cfg!(windows) { Path::new("/home/u/AppData/Local/hermes") } else { Path::new("/home/u/.hermes") };
        assert_eq!(root(&home), Some(want.to_path_buf()));
        assert_eq!(root(&|_| None), None);
        assert_eq!(root(&|_| Some(String::new())), None);
    }

    #[test]
    fn missing_empty_or_corrupt_store_lists_nothing() {
        let h = Scratch::new();
        assert!(list(h.path(), "/w").is_empty());
        assert!(!h.path().join("state.db").exists(), "never created");
        h.put("state.db", b"not a database at all, just text", 1);
        assert!(list(h.path(), "/w").is_empty());
        let empty = Scratch::new();
        db(empty.path());
        assert!(list(empty.path(), "/w").is_empty());
        let other_schema = Scratch::new();
        Connection::open(other_schema.path().join("state.db")).unwrap().execute("CREATE TABLE t (x)", []).unwrap();
        assert!(list(other_schema.path(), "/w").is_empty());
    }

    #[test]
    fn folder_match_newest_first_without_continuations_or_unbound_sessions() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "old", None, 10.0, Some(11.5), Some("/w"));
        add(&conn, "live", None, 20.25, None, Some("/w"));
        add(&conn, "other", None, 30.0, None, Some("/elsewhere"));
        add(&conn, "child", Some("live"), 40.0, None, Some("/w"));
        add(&conn, "unbound", None, 50.0, None, None);
        add(&conn, "blank", None, 60.0, None, Some(""));
        drop(conn);
        let found = list(h.path(), "/w");
        let mut ids: Vec<(&str, u64)> = found.iter().map(|s| (s.id.as_str(), s.last_used)).collect();
        ids.sort_by_key(|(_, ms)| std::cmp::Reverse(*ms));
        assert_eq!(ids, [("live", 20_250), ("old", 11_500)]);
        assert!(found.iter().all(|s| s.agent == "hermes" && s.title.as_deref() == Some("a title")));
        assert!(found[0].path.ends_with("state.db"));
        assert!(list(h.path(), "").is_empty());
    }

    #[test]
    fn windows_drive_case_matches() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "w", None, 1.0, None, Some(r"C:\Work\App"));
        drop(conn);
        assert_eq!(list(h.path(), &normal("c:/work/app/")).len(), 1);
        assert!(list(h.path(), &normal(r"C:\Work\Other")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_database_is_not_read() {
        let outside = Scratch::new();
        add(&db(outside.path()), "x", None, 1.0, None, Some("/w"));
        let h = Scratch::new();
        std::os::unix::fs::symlink(outside.path().join("state.db"), h.path().join("state.db")).unwrap();
        assert!(list(h.path(), "/w").is_empty());
    }

    /// Every file in `dir` with its length and modification time.
    fn snapshot(dir: &Path) -> Vec<(String, u64, Option<std::time::SystemTime>)> {
        let mut all: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| {
                let meta = e.metadata().unwrap();
                (e.file_name().to_string_lossy().into_owned(), meta.len(), meta.modified().ok())
            })
            .collect();
        all.sort();
        all
    }

    #[test]
    fn hermes_edge_a_file_that_is_not_sqlite_is_read_as_nothing_and_left_as_it_was() {
        let cases: [&[u8]; 4] = [b"", b"SQLite format 3\0 truncated header", &[0xff; 4096], b"\0\0\0\0"];
        for bytes in cases {
            let h = Scratch::new();
            h.put("state.db", bytes, 1);
            let before = snapshot(h.path());
            assert!(list(h.path(), "/w").is_empty(), "{bytes:?}");
            assert_eq!(snapshot(h.path()), before, "nothing created or changed: {:?}", &bytes[..bytes.len().min(8)]);
        }
        // A folder named state.db is not a store.
        let h = Scratch::new();
        std::fs::create_dir(h.path().join("state.db")).unwrap();
        assert!(list(h.path(), "/w").is_empty());
    }

    #[test]
    fn hermes_edge_a_sessions_table_missing_a_column_lists_nothing() {
        let h = Scratch::new();
        let conn = Connection::open(h.path().join("state.db")).unwrap();
        conn.execute("CREATE TABLE sessions (id TEXT PRIMARY KEY, started_at REAL NOT NULL, cwd TEXT, title TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO sessions VALUES ('x', 1.0, '/w', 't')", []).unwrap();
        drop(conn);
        assert!(list(h.path(), "/w").is_empty(), "no parent_session_id column: an unknown schema");
    }

    #[test]
    fn hermes_edge_null_and_odd_values_skip_only_their_own_row() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "good", None, 2.0, None, Some("/w"));
        conn.execute("INSERT INTO sessions (id, source, started_at, cwd) VALUES (NULL, 'cli', 3.0, '/w')", []).unwrap();
        conn.execute("INSERT INTO sessions (id, source, started_at, cwd) VALUES ('int-cwd', 'cli', 4.0, 7)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, cwd, title) VALUES ('no-title', 'cli', 1.0, '/w', NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, cwd, title) VALUES ('blank-title', 'cli', 1.5, '/w', '  ')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, cwd) VALUES ('before-1970', 'cli', -5.0, '/w')",
            [],
        )
        .unwrap();
        drop(conn);
        let mut got: Vec<(String, Option<String>, u64)> =
            list(h.path(), "/w").into_iter().map(|s| (s.id, s.title, s.last_used)).collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("before-1970".to_string(), None, 0),
                ("blank-title".to_string(), None, 1_500),
                ("good".to_string(), Some("a title".to_string()), 2_000),
                ("no-title".to_string(), None, 1_000),
            ]
        );
    }

    #[test]
    fn hermes_edge_every_continuation_is_left_out_even_an_orphan_or_a_grandchild() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "root", None, 1.0, None, Some("/w"));
        add(&conn, "child", Some("root"), 2.0, None, Some("/w"));
        add(&conn, "grandchild", Some("child"), 3.0, None, Some("/w"));
        add(&conn, "orphan", Some("deleted-parent"), 4.0, None, Some("/w"));
        drop(conn);
        let ids: Vec<String> = list(h.path(), "/w").into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["root"]);
    }

    #[test]
    fn hermes_edge_two_spellings_of_one_folder_both_list_once() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "plain", None, 1.0, None, Some("/w"));
        add(&conn, "slash", None, 2.0, None, Some("/w/"));
        add(&conn, "dot", None, 3.0, None, Some("/w/./"));
        drop(conn);
        let mut ids: Vec<String> = list(h.path(), "/w").into_iter().map(|s| s.id).collect();
        ids.sort();
        assert_eq!(ids, ["dot", "plain", "slash"]);
    }

    #[test]
    fn hermes_edge_at_most_rows_sessions_per_folder_the_newest_kept() {
        for count in [ROWS, ROWS + 1] {
            let h = Scratch::new();
            let conn = db(h.path());
            conn.execute_batch("BEGIN").unwrap();
            for n in 0..count {
                add(&conn, &format!("s{n:04}"), None, n as f64, None, Some("/w"));
            }
            conn.execute_batch("COMMIT").unwrap();
            drop(conn);
            let found = list(h.path(), "/w");
            assert_eq!(found.len(), ROWS, "{count} rows");
            assert!(found.iter().any(|s| s.id == format!("s{:04}", count - 1)), "the newest is kept");
            if count > ROWS {
                assert!(found.iter().all(|s| s.id != "s0000"), "the oldest is the one dropped");
            }
        }
    }

    #[test]
    fn hermes_edge_reading_creates_no_file_in_any_journal_mode() {
        // A rollback-journal database.
        let h = Scratch::new();
        add(&db(h.path()), "a", None, 1.0, None, Some("/w"));
        let before = snapshot(h.path());
        assert_eq!(list(h.path(), "/w").len(), 1);
        assert_eq!(snapshot(h.path()), before, "rollback journal");

        // A WAL database with no writer: its -wal and -shm are gone.
        let idle = Scratch::new();
        let conn = db(idle.path());
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        add(&conn, "a", None, 1.0, None, Some("/w"));
        drop(conn);
        let before = snapshot(idle.path());
        assert_eq!(before.len(), 1, "{before:?}");
        assert_eq!(list(idle.path(), "/w").len(), 1);
        assert_eq!(snapshot(idle.path()), before, "idle WAL: no -wal or -shm created");

        // A WAL database while Hermes holds it open: its rows in the -wal are seen, nothing new appears.
        let live = Scratch::new();
        let conn = db(live.path());
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
        conn.execute_batch("PRAGMA wal_autocheckpoint=0").unwrap();
        add(&conn, "in-wal", None, 1.0, None, Some("/w"));
        let names = |dir: &Path| snapshot(dir).into_iter().map(|(n, _, _)| n).collect::<Vec<_>>();
        let before = names(live.path());
        let found = list(live.path(), "/w");
        assert_eq!(found.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["in-wal"]);
        assert_eq!(names(live.path()), before);
        drop(conn);
    }

    #[test]
    fn hermes_edge_root_skips_an_empty_hermes_home_and_uses_localappdata_only_on_windows() {
        let env = |n: &str| match n {
            "HERMES_HOME" => Some(String::new()),
            "LOCALAPPDATA" => Some("/la".to_string()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        let want = if cfg!(windows) { PathBuf::from("/la").join("hermes") } else { PathBuf::from("/h/.hermes") };
        assert_eq!(root(&env), Some(want));
        let profile_only = |n: &str| (n == "USERPROFILE").then(|| r"C:\Users\u".to_string());
        let want = if cfg!(windows) {
            PathBuf::from(r"C:\Users\u").join("AppData").join("Local").join("hermes")
        } else {
            PathBuf::from(r"C:\Users\u").join(".hermes")
        };
        assert_eq!(root(&profile_only), Some(want));
    }
}
