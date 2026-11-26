//! Kilo (an OpenCode fork): the `session` table of `<data>/kilo/kilo.db`, the
//! stable channel's database, read as OpenCode's is. Kept apart from OpenCode's
//! own store: separate folder, separate agent.

use super::*;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    data_home(env).map(|d| d.join("kilo"))
}

pub(super) fn list(data: &Path, want: &str) -> Vec<SessionInfo> {
    opencode::from_db(&data.join("kilo.db"), want, "kilo")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::Scratch;
    use rusqlite::Connection;

    const TABLE: &str = "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL, \
                         title TEXT NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER)";

    fn db(dir: &Path) -> Connection {
        std::fs::create_dir_all(dir).unwrap();
        let conn = Connection::open(dir.join("kilo.db")).unwrap();
        conn.execute(TABLE, []).unwrap();
        conn
    }

    fn add(conn: &Connection, id: &str, directory: &str, updated: i64) {
        conn.execute(
            "INSERT INTO session (id, directory, title, time_updated) VALUES (?1, ?2, 'a title', ?3)",
            (id, directory, updated),
        )
        .unwrap();
    }

    #[test]
    fn root_is_the_kilo_folder_in_the_data_home() {
        let own = |n: &str| (n == "XDG_DATA_HOME").then(|| "/data".to_string());
        assert_eq!(root(&own), Some(Path::new("/data").join("kilo")));
        let home = |n: &str| (n == "HOME").then(|| "/home/u".to_string());
        assert_eq!(root(&home), Some(Path::new("/home/u").join(".local").join("share").join("kilo")));
        assert_eq!(root(&|_| None), None);
    }

    #[test]
    fn missing_or_corrupt_store_lists_nothing() {
        let h = Scratch::new();
        assert!(list(h.path(), "/w").is_empty());
        h.put("kilo.db", b"not a database at all, just text", 1);
        assert!(list(h.path(), "/w").is_empty());
        let empty = Scratch::new();
        db(empty.path());
        assert!(list(empty.path(), "/w").is_empty());
    }

    #[test]
    fn folder_match_newest_first_and_children_left_out() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "ses_old", "/w", 10);
        add(&conn, "ses_new", "/w", 20);
        add(&conn, "ses_other", "/elsewhere", 30);
        conn.execute("INSERT INTO session VALUES ('ses_child', 'ses_new', '/w', 't', 40, NULL)", []).unwrap();
        conn.execute("INSERT INTO session VALUES ('ses_gone', NULL, '/w', 't', 50, 5)", []).unwrap();
        drop(conn);
        let mut found = list(h.path(), "/w");
        found.sort_by_key(|s| std::cmp::Reverse(s.last_used));
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["ses_new", "ses_old"]);
        assert!(found.iter().all(|s| s.agent == "kilo" && s.title.as_deref() == Some("a title")));
        assert_eq!(found[0].last_used, 20);
        assert!(found[0].path.ends_with("kilo.db"));
    }

    #[test]
    fn windows_drive_case_matches() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "ses_w", r"C:\Work\App", 1);
        drop(conn);
        assert_eq!(list(h.path(), &normal("c:/work/app/")).len(), 1);
        assert!(list(h.path(), &normal(r"C:\Work\Other")).is_empty());
    }

    #[test]
    fn rows_only_in_the_wal_file_are_read() {
        let h = Scratch::new();
        let conn = db(h.path());
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        add(&conn, "ses_wal", "/w", 7);
        // The writer stays open, as a running Kilo does: the row may only be in the -wal file.
        assert_eq!(list(h.path(), "/w").len(), 1);
        drop(conn);
    }

    #[test]
    fn kilo_and_opencode_stores_are_not_mixed() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "ses_k", "/w", 1);
        drop(conn);
        assert!(opencode::list(h.path(), "/w").is_empty(), "opencode reads opencode.db, not kilo.db");
        let other = Scratch::new();
        std::fs::copy(h.path().join("kilo.db"), other.path().join("opencode.db")).unwrap();
        assert!(list(other.path(), "/w").is_empty(), "kilo reads kilo.db, not opencode.db");
        assert_eq!(opencode::list(other.path(), "/w")[0].agent, "opencode");
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
        assert!(snapshot(h.path()).is_empty(), "no kilo.db, -journal, -wal or -shm made");
        let gone = h.path().join("missing");
        assert!(list(&gone, "/w").is_empty());
        assert!(!gone.exists());
        let file = h.put("kilo-file", b"x", 1);
        assert!(list(&file, "/w").is_empty(), "the data folder is a file");
        std::fs::create_dir_all(h.path().join("d/kilo.db")).unwrap();
        assert!(list(&h.path().join("d"), "/w").is_empty(), "kilo.db is a folder");
    }

    #[test]
    fn finder_edge_not_sqlite_or_no_table_is_left_untouched() {
        let h = Scratch::new();
        h.put("kilo.db", b"SQLite format 3\0 but then garbage, not a page", 1);
        let before = snapshot(h.path());
        assert!(list(h.path(), "/w").is_empty());
        assert_eq!(snapshot(h.path()), before, "never written, no side files");
        let t = Scratch::new();
        let conn = Connection::open(t.path().join("kilo.db")).unwrap();
        conn.execute("CREATE TABLE other (directory TEXT)", []).unwrap();
        conn.execute("INSERT INTO other VALUES ('/w')", []).unwrap();
        drop(conn);
        let before = snapshot(t.path());
        assert!(list(t.path(), "/w").is_empty(), "no session table");
        assert_eq!(snapshot(t.path()), before);
        let e = Scratch::new();
        e.put("kilo.db", b"", 1);
        assert!(list(e.path(), "/w").is_empty(), "an empty file");
        assert_eq!(snapshot(e.path()).len(), 1);
    }

    #[test]
    fn finder_edge_reading_never_modifies_the_database() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "ses_1", "/w", 5);
        drop(conn);
        let before = snapshot(h.path());
        let bytes = std::fs::read(h.path().join("kilo.db")).unwrap();
        assert_eq!(list(h.path(), "/w").len(), 1);
        assert!(list(h.path(), "/other").is_empty());
        assert_eq!(snapshot(h.path()), before, "same files, sizes and mtimes");
        assert_eq!(std::fs::read(h.path().join("kilo.db")).unwrap(), bytes);
    }

    #[test]
    fn finder_edge_closed_wal_database_is_read_without_new_files() {
        let h = Scratch::new();
        let conn = db(h.path());
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)).unwrap();
        add(&conn, "ses_w", "/w", 3);
        drop(conn);
        let before = snapshot(h.path());
        let ids: Vec<String> = list(h.path(), "/w").into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["ses_w"]);
        assert_eq!(snapshot(h.path()), before, "no -wal or -shm left behind, db untouched");
    }

    #[test]
    fn finder_edge_locked_database_lists_nothing_quickly() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "ses_1", "/w", 1);
        conn.execute_batch("BEGIN EXCLUSIVE; UPDATE session SET title = 'x';").unwrap();
        let start = std::time::Instant::now();
        let found = list(h.path(), "/w");
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "held up {:?}", start.elapsed());
        // Locked: nothing, or the committed row; never a panic or a half-written one.
        assert!(found.iter().all(|s| s.id == "ses_1" && s.title.as_deref() == Some("a title")));
        conn.execute_batch("ROLLBACK").unwrap();
        assert_eq!(list(h.path(), "/w").len(), 1);
    }

    #[test]
    fn finder_edge_directories_with_slashes_dots_and_prefixes() {
        let h = Scratch::new();
        let conn = db(h.path());
        add(&conn, "ses_a", "/w/", 1);
        add(&conn, "ses_b", "/w/x/..", 2);
        add(&conn, "ses_c", "/w/x", 3);
        add(&conn, "ses_d", r"\\?\UNC\Srv\Share\P", 4);
        add(&conn, "ses_e", r"\\?\C:\Work\App\", 5);
        add(&conn, "ses_f", "", 6);
        drop(conn);
        let ids = |want: &str| {
            let mut ids: Vec<String> = list(h.path(), &normal(want)).into_iter().map(|s| s.id).collect();
            ids.sort();
            ids
        };
        assert_eq!(ids("/w"), ["ses_a", "ses_b"]);
        assert_eq!(ids(r"\\srv\share\p\"), ["ses_d"]);
        assert_eq!(ids("c:/work/app"), ["ses_e"]);
        assert!(list(h.path(), "").iter().all(|s| s.id == "ses_f"), "an empty folder matches only its own");
    }

    #[test]
    fn finder_edge_null_and_negative_times() {
        let h = Scratch::new();
        let conn = Connection::open(h.path().join("kilo.db")).unwrap();
        conn.execute(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, \
             time_updated INTEGER, time_archived INTEGER)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO session VALUES ('ses_n', NULL, '/w', NULL, NULL, NULL)", []).unwrap();
        conn.execute("INSERT INTO session VALUES ('ses_m', NULL, '/w', 't', -5, 0)", []).unwrap();
        conn.execute("INSERT INTO session VALUES ('ses_z', NULL, NULL, 't', 1, NULL)", []).unwrap();
        drop(conn);
        let mut found = list(h.path(), "/w");
        found.sort_by(|a, b| a.id.cmp(&b.id));
        let got: Vec<(&str, u64)> = found.iter().map(|s| (s.id.as_str(), s.last_used)).collect();
        assert_eq!(got, [("ses_m", 0), ("ses_n", 0)]);
    }

    #[test]
    fn finder_edge_empty_xdg_data_home_falls_back_to_home() {
        let env = |n: &str| match n {
            "XDG_DATA_HOME" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(Path::new("/h").join(".local").join("share").join("kilo")));
    }

    #[test]
    fn finder_edge_kilo_and_opencode_side_by_side_never_mix() {
        let data = Scratch::new();
        let env = |n: &str| (n == "XDG_DATA_HOME").then(|| data.path().display().to_string());
        let kilo_root = root(&env).unwrap();
        let open_root = opencode::root(&env).unwrap();
        assert_ne!(kilo_root, open_root);
        let k = db(&kilo_root);
        add(&k, "ses_k", "/w", 1);
        drop(k);
        std::fs::create_dir_all(&open_root).unwrap();
        let o = Connection::open(open_root.join("opencode.db")).unwrap();
        o.execute(TABLE, []).unwrap();
        o.execute("INSERT INTO session (id, directory, title, time_updated) VALUES ('ses_o', '/w', 't', 2)", [])
            .unwrap();
        drop(o);
        // A stray opencode.db in kilo's folder is not kilo's.
        std::fs::copy(open_root.join("opencode.db"), kilo_root.join("opencode.db")).unwrap();
        let ids = |v: Vec<SessionInfo>| v.into_iter().map(|s| (s.agent, s.id)).collect::<Vec<_>>();
        assert_eq!(ids(list(&kilo_root, "/w")), [("kilo".to_string(), "ses_k".to_string())]);
        assert_eq!(ids(opencode::list(&open_root, "/w")), [("opencode".to_string(), "ses_o".to_string())]);
    }
}
