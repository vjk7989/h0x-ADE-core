use super::*;
use serde_json::{Value, json};
use std::time::{Duration, UNIX_EPOCH};

/// A scratch HOME holding agent stores; removed afterwards.
struct Home(PathBuf);

impl Home {
    fn new() -> Home {
        let root = std::env::temp_dir().join(format!("h0x-core-sessions-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        Home(root)
    }

    fn roots(&self) -> Roots {
        let home = self.0.display().to_string();
        Roots::new(|name| (name == "HOME").then(|| home.clone()))
    }

    fn list(&self, folder: &str) -> Vec<(String, String, Option<String>)> {
        list(folder, &self.roots()).into_iter().map(|s| (s.agent, s.id, s.title)).collect()
    }

    /// Writes a file under HOME, last used at `ms`.
    fn put(&self, rel: &str, bytes: &[u8], ms: u64) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis(ms)).unwrap();
        path
    }

    fn claude(&self, dir: &str, id: &str, lines: &[Value], ms: u64) -> PathBuf {
        self.put(&format!(".claude/projects/{dir}/{id}.jsonl"), jsonl(lines).as_bytes(), ms)
    }

    fn codex(&self, id: &str, cwd: &str, ms: u64) -> PathBuf {
        let meta = json!({"timestamp": "2026-11-05T10:00:00.000Z", "type": "session_meta",
            "payload": {"id": id, "cwd": cwd, "base_instructions": {"text": "be helpful"}}});
        let turn = json!({"type": "event_msg", "payload": {"type": "user_message", "message": "hi"}});
        self.put(
            &format!(".codex/sessions/2026/11/05/rollout-2026-11-05T10-00-00-{id}.jsonl"),
            jsonl(&[meta, turn]).as_bytes(),
            ms,
        )
    }

    fn opencode(&self, id: &str, directory: &str, updated: u64) -> PathBuf {
        let s = json!({"id": id, "version": "0.15.0", "projectID": "p1", "directory": directory, "title": format!("{id} title"),
            "time": {"created": 1, "updated": updated}});
        self.put(&format!(".local/share/opencode/storage/session/p1/{id}.json"), s.to_string().as_bytes(), 1)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn jsonl(lines: &[Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn said(cwd: &str) -> Value {
    json!({"type": "user", "cwd": cwd, "sessionId": "s", "message": {"role": "user", "content": "hi"}})
}

fn named(kind: &str, key: &str, value: &str) -> Value {
    json!({"type": kind, key: value, "sessionId": "s"})
}

fn s(agent: &str, id: &str, title: Option<&str>) -> (String, String, Option<String>) {
    (agent.into(), id.into(), title.map(String::from))
}

#[test]
fn stores_are_found_from_home_or_each_agents_own_variable() {
    let roots = Roots::new(|name| (name == "HOME").then(|| "/home/u".to_string()));
    assert_eq!(roots.claude, Some(Path::new("/home/u").join(".claude")));
    assert_eq!(roots.codex, Some(Path::new("/home/u").join(".codex")));
    assert_eq!(roots.opencode, Some(Path::new("/home/u").join(".local").join("share").join("opencode")));

    let profile = Roots::new(|name| match name {
        "HOME" => Some(String::new()),
        "USERPROFILE" => Some(r"C:\Users\Me".to_string()),
        _ => None,
    });
    assert_eq!(profile.claude, Some(Path::new(r"C:\Users\Me").join(".claude")), "an empty HOME is no home");

    let own = Roots::new(|name| match name {
        "CLAUDE_CONFIG_DIR" => Some("/cfg/claude".into()),
        "CODEX_HOME" => Some("/cfg/codex".into()),
        "XDG_DATA_HOME" => Some("/data".into()),
        _ => None,
    });
    assert_eq!(own.claude, Some(PathBuf::from("/cfg/claude")));
    assert_eq!(own.codex, Some(PathBuf::from("/cfg/codex")));
    assert_eq!(own.opencode, Some(Path::new("/data").join("opencode")));

    let none = Roots::new(|_| None);
    assert!(none.claude.is_none() && none.codex.is_none() && none.opencode.is_none());
    assert!(list("/anywhere", &none).is_empty(), "no home: nothing, no error");
}

#[test]
fn folders_compare_as_their_os_compares_them() {
    let proj = r"c:\users\me\my proj";
    for same in
        [r"C:\Users\Me\My Proj", r"\\?\C:\USERS\me\My Proj\", "c:/users/me/my proj/", r"C:\Users\Me\x\..\My Proj\."]
    {
        assert_eq!(normal(same), proj, "{same}");
    }
    assert_eq!(normal(r"\\?\UNC\Server\Share\Dir"), r"\\server\share\dir");
    assert_eq!(normal(r"\\Server\Share\Dir\"), r"\\server\share\dir");
    assert_eq!(normal(r"C:\"), r"c:\");
    assert_eq!(normal(r"C:\..\.."), r"c:\", "never above the drive");
    assert_eq!(normal("/home/u/a/./b/../c/"), "/home/u/a/c");
    assert_eq!(normal("/../.."), "/");
    assert_ne!(normal("/Home/U"), normal("/home/u"), "Unix paths keep their case");
    assert_eq!(normal(""), "");
}

#[test]
fn claude_sessions_of_a_folder_with_their_latest_title_newest_first() {
    let h = Home::new();
    let cwd = "/home/u/app";
    h.claude(
        "-home-u-app",
        "a1",
        &[said(cwd), named("ai-title", "aiTitle", "Old name"), named("ai-title", "aiTitle", "Fix tests")],
        1_000,
    );
    let renamed =
        [said(cwd), named("custom-title", "customTitle", "Mine"), named("ai-title", "aiTitle", "Later AI name")];
    let b2 = h.claude("-home-u-app", "b2", &renamed, 3_000);
    h.claude("-home-u-app", "c3", &[said(cwd), named("last-prompt", "lastPrompt", "\n  first line  \nsecond")], 2_000);
    h.claude("-home-u-app", "agent-x", &[said(cwd)], 4_000);
    h.claude("-home-u-other", "d4", &[said("/home/u/other")], 5_000);
    std::fs::write(h.0.join(".claude/projects/-home-u-app/notes.txt"), "not a session").unwrap();

    assert_eq!(
        h.list(cwd),
        [s("claude", "b2", Some("Mine")), s("claude", "c3", Some("first line")), s("claude", "a1", Some("Fix tests"))]
    );
    let first = list(cwd, &h.roots()).remove(0);
    assert_eq!((first.last_used, PathBuf::from(first.path)), (3_000, b2));
    assert_eq!(h.list("/home/u/app/"), h.list(cwd), "a trailing separator is the same folder");
    assert!(h.list("/home/u/none").is_empty(), "a folder without sessions");
    assert!(Home::new().list(cwd).is_empty(), "no stores at all");
}

#[test]
fn claude_names_collide_and_the_cwd_inside_decides() {
    let h = Home::new();
    h.claude("-a-b", "dash", &[said("/a-b")], 2);
    h.claude("-a-b", "under", &[said("/a_b")], 1);
    h.claude("-a-b", "unknown", &[named("ai-title", "aiTitle", "no cwd yet")], 0);
    assert_eq!(h.list("/a-b"), [s("claude", "dash", None), s("claude", "unknown", Some("no cwd yet"))]);
    assert_eq!(h.list("/a_b"), [s("claude", "under", None), s("claude", "unknown", Some("no cwd yet"))]);
}

#[test]
fn claude_long_folders_match_by_their_cut_name() {
    let h = Home::new();
    let folder = format!("/{}", "x".repeat(250));
    let dir = format!("-{}-1a2b3c", "x".repeat(199));
    h.claude(&dir, "long", &[said(&folder)], 1);
    assert_eq!(h.list(&folder), [s("claude", "long", None)]);
    assert!(h.list(&format!("/{}", "x".repeat(150))).is_empty());
}

#[test]
fn spaces_and_unicode_in_the_folder() {
    let h = Home::new();
    let folder = "/home/u/my app/\u{e7}af\u{e9} \u{1F680}";
    // Each UTF-16 unit that is not an ASCII letter or digit becomes '-'.
    h.claude("-home-u-my-app--af----", "c", &[said(folder)], 3);
    h.codex("x", folder, 2);
    h.opencode("ses_o", folder, 1);
    assert_eq!(
        h.list(folder),
        [s("claude", "c", None), s("codex", "x", None), s("opencode", "ses_o", Some("ses_o title"))]
    );
}

#[test]
fn windows_folders_match_in_any_case_and_with_the_verbatim_prefix() {
    let h = Home::new();
    let folder = r"C:\Users\Me\My Proj";
    h.claude("C--Users-Me-My-Proj", "c", &[said(folder)], 3);
    h.codex("x", folder, 2);
    h.opencode("ses_o", folder, 1);
    let all = [s("claude", "c", None), s("codex", "x", None), s("opencode", "ses_o", Some("ses_o title"))];
    for asked in [folder, r"c:\users\me\my proj", r"\\?\C:\USERS\ME\MY PROJ\", "c:/Users/Me/My Proj"] {
        assert_eq!(h.list(asked), all, "{asked}");
    }
    assert!(h.list(r"C:\Users\Me").is_empty());
}

#[test]
fn codex_sessions_by_their_first_line_named_from_the_index() {
    let h = Home::new();
    let cwd = "/home/u/app";
    h.codex("019e-1", cwd, 1_000);
    let newer = h.codex("019e-2", cwd, 2_000);
    h.codex("019e-3", "/home/u/other", 3_000);
    assert_eq!(h.list(cwd), [s("codex", "019e-2", None), s("codex", "019e-1", None)], "no index: no names");

    let index = [
        json!({"id": "019e-1", "thread_name": "first", "updated_at": "x"}),
        json!({"id": "019e-1", "thread_name": "renamed", "updated_at": "y"}),
        json!({"id": "019e-2"}),
    ];
    h.put(".codex/session_index.jsonl", format!("{}not json\n", jsonl(&index)).as_bytes(), 0);
    assert_eq!(h.list(cwd), [s("codex", "019e-2", None), s("codex", "019e-1", Some("renamed"))], "later lines win");
    assert_eq!(PathBuf::from(&list(cwd, &h.roots())[0].path), newer);
}

#[test]
fn opencode_root_sessions_with_their_own_times() {
    let h = Home::new();
    let cwd = "/home/u/app";
    h.opencode("ses_a", cwd, 5_000);
    h.opencode("ses_b", cwd, 7_000);
    h.opencode("ses_c", "/home/u/other", 9_000);
    let child =
        json!({"id": "ses_d", "directory": cwd, "parentID": "ses_a", "title": "subagent", "time": {"updated": 8_000}});
    h.put(".local/share/opencode/storage/session/p1/ses_d.json", child.to_string().as_bytes(), 1);
    let untimed = json!({"id": "ses_e", "directory": cwd});
    h.put(".local/share/opencode/storage/session/p2/ses_e.json", untimed.to_string().as_bytes(), 6_000);
    assert_eq!(
        h.list(cwd),
        [
            s("opencode", "ses_b", Some("ses_b title")),
            s("opencode", "ses_e", None),
            s("opencode", "ses_a", Some("ses_a title"))
        ]
    );
}

#[test]
fn empty_corrupt_and_partial_files_are_left_out() {
    let h = Home::new();
    let cwd = "/home/u/app";
    h.claude("-home-u-app", "good", &[said(cwd)], 1);
    h.put(".claude/projects/-home-u-app/empty.jsonl", b"", 2);
    h.put(".claude/projects/-home-u-app/garbage.jsonl", b"\x00\xff not json {{{\n", 2);
    h.put(".claude/projects/-home-u-app/cut.jsonl", br#"{"type":"user","cwd":"/home/u/app","mess"#, 2);
    h.codex("good", cwd, 1);
    for (name, text) in [
        ("empty", ""),
        ("garbage", "\u{0}nope\n"),
        ("old", "{\"id\":\"old\",\"timestamp\":\"t\"}\n"),
        ("other", "{\"type\":\"turn_context\",\"payload\":{\"id\":\"o\",\"cwd\":\"/home/u/app\"}}\n"),
    ] {
        h.put(&format!(".codex/sessions/2026/11/05/rollout-{name}.jsonl"), text.as_bytes(), 2);
    }
    h.put(".codex/sessions/2026/11/05/notes.jsonl", jsonl(&[said(cwd)]).as_bytes(), 2);
    h.put(".codex/sessions/2026/stray.jsonl", b"", 2);
    h.opencode("ses_good", cwd, 1);
    h.put(".local/share/opencode/storage/session/p1/broken.json", b"{\"id\":", 2);
    h.put(".local/share/opencode/storage/session/p1/nodir.json", b"{\"id\":\"ses_x\"}", 2);
    h.put(".local/share/opencode/storage/session/p1/empty.json", b"", 2);
    assert_eq!(
        h.list(cwd),
        [s("claude", "good", None), s("codex", "good", None), s("opencode", "ses_good", Some("ses_good title"))]
    );
}

#[test]
fn huge_files_are_read_at_their_ends_only() {
    let h = Home::new();
    let cwd = "/home/u/app";
    // A title only in the middle would win if the whole file were read.
    let middle = jsonl(&[named("custom-title", "customTitle", "MIDDLE")]).repeat(80_000);
    assert!(middle.len() > 4 << 20);
    let filler = jsonl(&[json!({"type": "progress"})]).repeat(5_000);
    assert!(filler.len() > 64 * 1024);
    let (first, last) = (jsonl(&[said(cwd)]), jsonl(&[named("ai-title", "aiTitle", "END")]));
    let text = format!("{first}{filler}{middle}{filler}{last}");
    h.put(".claude/projects/-home-u-app/big.jsonl", text.as_bytes(), 2);
    // Codex: a first line past its bound is left out; a normal one followed by MBs is not.
    let long = json!({"type": "session_meta", "payload": {"id": "long", "cwd": cwd, "base_instructions": "x".repeat(300 * 1024)}});
    h.put(".codex/sessions/2026/11/05/rollout-long.jsonl", jsonl(&[long]).as_bytes(), 3);
    let big = h.codex("big", cwd, 1);
    std::fs::write(&big, format!("{}{middle}", std::fs::read_to_string(&big).unwrap())).unwrap();
    let file = std::fs::File::options().write(true).open(&big).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_millis(1)).unwrap();
    // OpenCode: a session file is small; a huge one is not a session.
    let padded = json!({"id": "ses_big", "directory": cwd, "pad": "x".repeat(100 * 1024)});
    h.put(".local/share/opencode/storage/session/p1/ses_big.json", padded.to_string().as_bytes(), 4);
    assert_eq!(h.list(cwd), [s("claude", "big", Some("END")), s("codex", "big", None)]);
}

#[test]
fn sessions_of_all_agents_sort_newest_first() {
    let h = Home::new();
    let cwd = "/srv/app";
    h.claude("-srv-app", "c1", &[said(cwd)], 10);
    h.claude("-srv-app", "c2", &[said(cwd)], 40);
    h.codex("x1", cwd, 30);
    h.codex("x2", cwd, 40);
    h.opencode("ses_1", cwd, 20);
    h.opencode("ses_2", cwd, 50);
    let order: Vec<String> = h.list(cwd).into_iter().map(|(_, id, _)| id).collect();
    assert_eq!(order, ["ses_2", "c2", "x2", "x1", "ses_1", "c1"], "a tie goes by agent, then id");
}

#[test]
fn nothing_outside_the_stores_is_read() {
    let h = Home::new();
    // Beside the stores, not in them: a folder can only be compared, never walked to.
    h.claude("-secret", "inside", &[said("/secret")], 1);
    h.put(".claude/secret/outside.jsonl", jsonl(&[said("secret")]).as_bytes(), 1);
    h.put(".claude/outside.jsonl", jsonl(&[said("/home/u/app")]).as_bytes(), 1);
    for folder in ["secret", "../secret", "/home/u/app/../../..", "/home/u/app/../../../.claude"] {
        assert!(h.list(folder).iter().all(|(_, id, _)| id == "inside"), "{folder}");
    }

    // Links (and junctions) out of a store are not followed.
    let outside = Home::new();
    let session = outside.put("proj/s.jsonl", jsonl(&[said("/home/u/app")]).as_bytes(), 1);
    let store = h.0.join(".claude/projects");
    if !link(&outside.0.join("proj"), &store.join("-home-u-app")) || !link(&session, &store.join("-home-u-x/s.jsonl")) {
        return; // this OS account cannot make links
    }
    let codex_day = h.0.join(".codex/sessions/2026/11");
    std::fs::create_dir_all(&codex_day).unwrap();
    let rollout = outside.codex("away", "/home/u/app", 1);
    assert!(link(rollout.parent().unwrap(), &codex_day.join("05")));
    assert!(h.list("/home/u/app").is_empty());
    assert!(h.list("/home/u/x").is_empty());
}

fn link(target: &Path, at: &Path) -> bool {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, at);
    #[cfg(windows)]
    let made = if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, at)
    } else {
        std::os::windows::fs::symlink_file(target, at)
    };
    made.is_ok()
}

/// OpenCode 1.x's `session` table, as `PRAGMA table_info(session)` shows it (the columns read).
const OPENCODE_TABLE: &str = "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, \
    directory TEXT NOT NULL, title TEXT NOT NULL, time_created INTEGER, time_updated INTEGER NOT NULL, time_archived INTEGER)";

type Row<'a> = (&'a str, Option<&'a str>, &'a str, &'a str, i64, Option<i64>);

/// OpenCode's database under HOME, made with `table` and holding `rows`.
fn opencode_db(h: &Home, table: &str, rows: &[Row]) -> PathBuf {
    let path = h.0.join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(table).unwrap();
    for (id, parent, directory, title, updated, archived) in rows {
        db.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_updated, time_archived) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, parent, directory, title, updated, archived],
        )
        .unwrap();
    }
    path
}

#[test]
fn opencode_database_root_sessions_of_the_folder_read_only() {
    let h = Home::new();
    let db = opencode_db(
        &h,
        OPENCODE_TABLE,
        &[
            ("ses_1", None, "/home/u/app", "First", 1_000, None),
            ("ses_2", None, "/home/u/app/", "Second", 3_000, Some(0)),
            ("ses_other", None, "/home/u/other", "Other", 5_000, None),
            ("ses_child", Some("ses_1"), "/home/u/app", "Subagent", 6_000, None),
            ("ses_archived", None, "/home/u/app", "Archived", 7_000, Some(6_500)),
            ("ses_untitled", None, "/home/u/app", "", 2_000, None),
            ("ses_win", None, r"C:\Users\Me\My Proj", "Windows", 4_000, None),
        ],
    );
    // Older JSON storage still counts; the database wins for a session in both.
    h.opencode("ses_json", "/home/u/app", 2_500);
    h.opencode("ses_1", "/home/u/app", 9_000);
    let before = std::fs::read(&db).unwrap();

    assert_eq!(
        h.list("/home/u/app"),
        [
            s("opencode", "ses_2", Some("Second")),
            s("opencode", "ses_json", Some("ses_json title")),
            s("opencode", "ses_untitled", None),
            s("opencode", "ses_1", Some("First")),
        ]
    );
    let first = list("/home/u/app", &h.roots()).remove(0);
    assert_eq!((first.last_used, PathBuf::from(first.path)), (3_000, db.clone()));
    assert_eq!(h.list(r"\\?\c:\users\me\my proj\"), [s("opencode", "ses_win", Some("Windows"))]);
    assert!(h.list("/home/u/none").is_empty());
    assert_eq!(std::fs::read(&db).unwrap(), before, "never written");
    assert!(!h.0.join(".local/share/opencode/opencode.db-journal").exists());
}

#[test]
fn opencode_databases_in_wal_mode_are_read_with_or_without_opencode_running() {
    let h = Home::new();
    let db = opencode_db(&h, OPENCODE_TABLE, &[("ses_1", None, "/home/u/app", "First", 1, None)]);
    let opencode = rusqlite::Connection::open(&db).unwrap();
    opencode.execute_batch("PRAGMA journal_mode=WAL; UPDATE session SET title = 'Running';").unwrap();
    assert_eq!(h.list("/home/u/app"), [s("opencode", "ses_1", Some("Running"))], "OpenCode running");
    drop(opencode);
    assert_eq!(h.list("/home/u/app"), [s("opencode", "ses_1", Some("Running"))], "OpenCode closed");
}

#[test]
fn an_unreadable_opencode_database_lists_nothing_but_json_storage_still_does() {
    let only_json = [s("opencode", "ses_json", Some("ses_json title"))];

    let corrupt = Home::new();
    corrupt.opencode("ses_json", "/home/u/app", 1);
    corrupt.put(".local/share/opencode/opencode.db", b"not a database, not at all, not even close", 1);
    assert_eq!(corrupt.list("/home/u/app"), only_json, "corrupt");

    let older = Home::new();
    older.opencode("ses_json", "/home/u/app", 1);
    opencode_db(&older, "CREATE TABLE session (id TEXT, directory TEXT, title TEXT)", &[]);
    assert_eq!(older.list("/home/u/app"), only_json, "a column missing");

    let empty = Home::new();
    empty.opencode("ses_json", "/home/u/app", 1);
    empty.put(".local/share/opencode/opencode.db", b"", 1);
    assert_eq!(empty.list("/home/u/app"), only_json, "empty");

    // A writer holding the database: we give up after the busy timeout, without an error.
    let locked = Home::new();
    locked.opencode("ses_json", "/home/u/app", 1);
    let db = opencode_db(&locked, OPENCODE_TABLE, &[("ses_1", None, "/home/u/app", "First", 1, None)]);
    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE; UPDATE session SET title = 'Busy';").unwrap();
    let started = std::time::Instant::now();
    assert_eq!(locked.list("/home/u/app"), only_json, "locked");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    drop(writer);
    assert_eq!(locked.list("/home/u/app")[0], s("opencode", "ses_1", Some("First")), "readable again");
}
