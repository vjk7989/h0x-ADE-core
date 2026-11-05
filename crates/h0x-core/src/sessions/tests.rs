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

const UUID: &str = "0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6f";

fn argv(agent: &str, id: &str) -> Vec<String> {
    resume_argv(agent, id).unwrap_or_else(|e| panic!("{agent} {id}: {e}"))
}

#[test]
fn resume_commands_per_agent_as_argv() {
    let cursor = if cfg!(windows) { "cursor-agent.cmd" } else { "cursor-agent" };
    let cases: [(&str, &str, &[&str]); 19] = [
        ("claude", UUID, &["claude", "--resume", UUID]),
        ("codex", UUID, &["codex", "resume", UUID]),
        ("copilot", UUID, &["copilot", "--resume=0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6f"]),
        ("devin", "d-1", &["devin", "--resume", "d-1"]),
        ("droid", UUID, &["droid", "--resume", UUID]),
        ("kimi", "k_1", &["kimi", "--session", "k_1"]),
        ("mastracode", "t1", &["mastracode", "--thread", "t1"]),
        ("pi", "p1", &["pi", "--session", "p1"]),
        ("omp", "abc", &["omp", "--resume=abc"]),
        ("hermes", "20260101_120000_ab12", &["hermes", "--resume", "20260101_120000_ab12"]),
        ("opencode", "ses_4f1a2bXyZ", &["opencode", "--session", "ses_4f1a2bXyZ"]),
        ("qodercli", "q1", &["qodercli", "--resume", "q1"]),
        ("qwen", UUID, &["qwen", "--resume", UUID]),
        ("kilo", "ses_1", &["kilo", "--session", "ses_1"]),
        ("cursor", "c1", &[cursor, "--resume", "c1"]),
        ("agy", "a.1", &["agy", "--conversation", "a.1"]),
        ("grok", "g1", &["grok", "--resume", "g1"]),
        ("letta", "conv-1", &["letta", "--conversation", "conv-1"]),
        ("letta", "default:agent-9", &["letta", "--conversation", "default", "--agent", "agent-9"]),
    ];
    for (agent, id, want) in cases {
        assert_eq!(argv(agent, id), want, "{agent}");
    }
    // Any case of hex digits.
    assert_eq!(argv("claude", &UUID.to_uppercase())[2], UUID.to_uppercase());
}

#[test]
fn resume_refuses_ids_not_of_their_agents_shape() {
    let long = "a".repeat(513);
    let max = "a".repeat(512);
    for agent in ["claude", "codex", "opencode", "devin", "letta", "cursor"] {
        for id in [
            "",
            long.as_str(),
            "--flag",
            "-r",
            "a b",
            " a",
            "a/b",
            r"a\b",
            "..",
            "../x",
            r"C:\x",
            "/tmp/s.jsonl",
            "a\nb",
            "a\u{0}b",
            "it's",
            "a;b",
            "$(x)",
            "é",
        ] {
            let e = resume_argv(agent, id).expect_err(&format!("{agent} {id:?}"));
            assert!(e.contains("session id") || e.contains("resume"), "{e}");
        }
    }
    assert_eq!(argv("devin", &max)[2], max, "the longest id");
    for (agent, id) in [
        ("claude", "s-here"),
        ("claude", "0b5f3c3e1d2a4c4b9e7f2a1b3c4d5e6f"),
        ("claude", "0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6g"),
        ("claude", "0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6f0"),
        ("codex", "rollout-x"),
        ("opencode", UUID),
        ("opencode", "ses_"),
        ("opencode", "ses_a-b"),
        ("letta", "default:"),
        ("letta", "default:--x"),
    ] {
        assert!(resume_argv(agent, id).is_err(), "{agent} {id}");
    }
    let e = resume_argv("vim", "x").unwrap_err();
    assert!(e.contains("no resume command"), "{e}");
}

fn kind(r: Result<Vec<PathBuf>, crate::rpc::RpcError>) -> String {
    r.expect_err("an error").kind
}

/// Whether a test may really move files to the trash: the freedesktop Trash
/// under the scratch HOME; the Recycle Bin, which is the person's own, only on
/// CI (`CI` set); Finder never.
fn trashes() -> bool {
    let may = !cfg!(target_os = "macos") && (!cfg!(windows) || std::env::var_os("CI").is_some());
    if !may {
        eprintln!("note: the move to the trash is skipped here (Windows: only with CI set; macOS: never)");
    }
    may
}

#[test]
fn trash_moves_a_claude_session_and_its_folder_out_of_the_store() {
    let h = Home::new();
    let session = h.claude("-home-u-app", UUID, &[said("/home/u/app")], 1);
    let side = h.put(&format!(".claude/projects/-home-u-app/{UUID}/subagents/agent-1.jsonl"), b"{}\n", 1);
    let other = h.claude("-home-u-app", "1b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6f", &[said("/home/u/app")], 1);
    if !trashes() {
        return;
    }
    let trashed = trash("claude", UUID, &h.roots()).unwrap();
    assert_eq!(trashed, [session.clone(), side.parent().unwrap().parent().unwrap().to_path_buf()]);
    assert!(!session.exists() && !side.exists());
    assert!(other.exists(), "only that session");
    assert_eq!(h.list("/home/u/app").len(), 1);
    assert_eq!(kind(trash("claude", UUID, &h.roots())), "not_found", "already gone");
}

#[test]
fn trash_moves_a_codex_rollout_and_an_opencode_json_session() {
    let h = Home::new();
    let rollout = h.codex(UUID, "/home/u/app", 1);
    let session = h.opencode("ses_old", "/home/u/app", 1);
    let messages = h.put(".local/share/opencode/storage/message/ses_old/msg_1.json", b"{}", 1);
    if !trashes() {
        return;
    }
    assert_eq!(trash("codex", UUID, &h.roots()).unwrap(), std::slice::from_ref(&rollout));
    assert!(!rollout.exists());
    assert_eq!(
        trash("opencode", "ses_old", &h.roots()).unwrap(),
        [session.clone(), messages.parent().unwrap().to_path_buf()]
    );
    assert!(!session.exists() && !messages.exists());
    assert!(h.list("/home/u/app").is_empty());
}

#[test]
fn trash_refuses_what_is_not_a_session_in_the_store() {
    let h = Home::new();
    let roots = h.roots();
    // Not of the agent's shape: never a path.
    for id in ["", "../x", r"..\x", "/etc/passwd", "--all", "a b", "s-here"] {
        assert_eq!(kind(trash("claude", id, &roots)), "invalid_params", "{id:?}");
    }
    assert_eq!(kind(trash("vim", "x", &roots)), "invalid_params");
    // Resumable, but h0x-core does not read its store.
    assert_eq!(kind(trash("copilot", UUID, &roots)), "unsupported");
    // Missing, or only beside the store.
    assert_eq!(kind(trash("claude", UUID, &roots)), "not_found");
    let beside = h.put(&format!(".claude/{UUID}.jsonl"), b"{}\n", 1);
    let wrong_name = h.put(&format!(".codex/sessions/2026/11/05/{UUID}.jsonl"), b"{}\n", 1);
    let too_deep = h.put(&format!(".codex/sessions/2026/11/05/x/rollout-1-{UUID}.jsonl"), b"{}\n", 1);
    assert_eq!(kind(trash("claude", UUID, &roots)), "not_found");
    assert_eq!(kind(trash("codex", UUID, &roots)), "not_found");
    assert_eq!(kind(trash("opencode", "ses_none", &roots)), "not_found");
    assert!(beside.exists() && wrong_name.exists() && too_deep.exists());
    // No store at all.
    let none = Roots::new(|_| None);
    assert_eq!(kind(trash("claude", UUID, &none)), "not_found");
}

#[test]
fn trash_never_follows_links_out_of_the_store() {
    let h = Home::new();
    let outside = Home::new();
    let target = outside.put("s.jsonl", jsonl(&[said("/home/u/app")]).as_bytes(), 1);
    let store = h.0.join(".claude/projects");
    if !link(&target, &store.join(format!("-home-u-app/{UUID}.jsonl"))) {
        return; // this OS account cannot make links
    }
    let project = outside.put(&format!("proj/{UUID}.jsonl"), jsonl(&[said("/home/u/x")]).as_bytes(), 1);
    assert!(link(project.parent().unwrap(), &store.join("-home-u-x")));
    assert_eq!(kind(trash("claude", UUID, &h.roots())), "not_found");
    let rollout = outside.codex(UUID, "/home/u/app", 1);
    assert!(link(rollout.parent().unwrap(), &h.0.join(".codex/sessions/2026/11/05")));
    assert_eq!(kind(trash("codex", UUID, &h.roots())), "not_found");
    assert!(target.exists() && project.exists() && rollout.exists(), "nothing behind a link is touched");
}

#[test]
fn trash_refuses_opencode_sessions_in_its_database() {
    let h = Home::new();
    h.opencode("ses_1", "/home/u/app", 1);
    let db = opencode_db(&h, OPENCODE_TABLE, &[("ses_1", None, "/home/u/app", "First", 1, None)]);
    let e = trash("opencode", "ses_1", &h.roots()).unwrap_err();
    assert_eq!(e.kind, "unsupported", "{e:?}");
    assert!(e.message.contains("database"), "{}", e.message);
    assert!(db.exists() && h.0.join(".local/share/opencode/storage/session/p1/ses_1.json").exists());
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn trash_follows_the_freedesktop_layout() {
    let h = Home::new();
    let trash_dir = h.0.join("data/Trash");
    let data = h.0.join("data").display().to_string();
    let home = h.0.display().to_string();
    let roots = Roots::new(|name| match name {
        "HOME" => Some(home.clone()),
        "XDG_DATA_HOME" => Some(data.clone()),
        _ => None,
    });
    assert_eq!(roots.trash.as_deref(), Some(trash_dir.as_path()));
    let dir = "-home-u-my-app";
    let first = h.claude(dir, UUID, &[said("/home/u/my app")], 1);
    // A name already in the Trash: the next one is `.2`.
    h.put(&format!("data/Trash/info/{UUID}.jsonl.trashinfo"), b"[Trash Info]\n", 1);
    assert_eq!(trash("claude", UUID, &roots).unwrap(), std::slice::from_ref(&first));
    let moved = trash_dir.join(format!("files/{UUID}.jsonl.2"));
    assert!(moved.is_file() && !first.exists());
    let info = std::fs::read_to_string(trash_dir.join(format!("info/{UUID}.jsonl.2.trashinfo"))).unwrap();
    let lines: Vec<&str> = info.lines().collect();
    assert_eq!(lines[0], "[Trash Info]");
    let path = first.display().to_string().replace(' ', "%20");
    assert_eq!(lines[1], format!("Path={path}"));
    let date = lines[2].strip_prefix("DeletionDate=").unwrap();
    assert_eq!(date.len(), 19, "{date}");
    assert_eq!((&date[4..5], &date[10..11], &date[13..14]), ("-", "T", ":"));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(trash_dir.join("files")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    // Percent-encoded: a space and a non-ASCII character in the path.
    let encoded = Home::new();
    let session = encoded.put(&format!("a b/ü/.claude/projects/-p/{UUID}.jsonl"), b"{}\n", 1);
    let root = encoded.0.join("a b/ü/.claude").display().to_string();
    let trash_home = encoded.0.display().to_string();
    let roots = Roots::new(|name| match name {
        "HOME" => Some(trash_home.clone()),
        "CLAUDE_CONFIG_DIR" => Some(root.clone()),
        _ => None,
    });
    trash("claude", UUID, &roots).unwrap();
    let info =
        std::fs::read_to_string(encoded.0.join(format!(".local/share/Trash/info/{UUID}.jsonl.trashinfo"))).unwrap();
    let want = format!("Path={}/a%20b/%C3%BC/.claude/projects/-p/{UUID}.jsonl", encoded.0.display());
    assert!(info.contains(&want), "{info}");
    assert!(!session.exists());
}

#[cfg(windows)]
#[test]
fn trash_on_windows_goes_to_the_recycle_bin_from_a_temp_dir() {
    let h = Home::new();
    let session = h.codex(UUID, r"C:\Users\u\app", 1);
    assert_eq!(h.list(r"c:\users\u\app").len(), 1);
    // The store named with `/`, as `CODEX_HOME` may be.
    let home = h.0.display().to_string().replace('\\', "/");
    let codex = format!("{home}/.codex");
    let roots = Roots::new(|name| match name {
        "HOME" => Some(home.clone()),
        "CODEX_HOME" => Some(codex.clone()),
        _ => None,
    });
    if !trashes() {
        return;
    }
    let trashed = trash("codex", UUID, &roots).unwrap();
    assert_eq!(trashed.len(), 1);
    assert!(!session.exists(), "moved");
    assert!(h.list(r"c:\users\u\app").is_empty());
}

#[test]
fn resume_token_ids_may_hold_dots_after_their_first_byte_and_letta_names_one_agent() {
    // An argument, never a path: dots anywhere but first are only characters.
    for id in ["a.", "a..b", "x..", "1", "A-_.z"] {
        assert_eq!(argv("devin", id), ["devin", "--resume", id], "{id:?}");
    }
    for id in [".a", "_a", ".", "...", "a:b", "a,b", "a\tb", "a\u{7f}b", "a\u{200b}"] {
        assert!(resume_argv("devin", id).is_err(), "{id:?}");
    }
    assert_eq!(argv("letta", "default"), ["letta", "--conversation", "default"], "no colon: a conversation id");
    assert_eq!(argv("letta", "default:a.b"), ["letta", "--conversation", "default", "--agent", "a.b"]);
    for id in ["default:default:x", "default:.x", "default:a/b", r"default:a\b", "default: x", "Default:x", ":x"] {
        assert!(resume_argv("letta", id).is_err(), "{id:?}");
    }
    // `default:` is Letta's alone.
    assert!(resume_argv("devin", "default:x").is_err());
}

#[test]
fn resume_uuid_and_opencode_ids_are_checked_at_their_edges() {
    assert_eq!(argv("qwen", "00000000-0000-0000-0000-000000000000")[2], "00000000-0000-0000-0000-000000000000");
    for id in [
        "0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6",
        "0b5f3c3e1-d2a-4c4b-9e7f-2a1b3c4d5e6f",
        "0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e-f",
        "0b5f3c3e_1d2a_4c4b_9e7f_2a1b3c4d5e6f",
        "{0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e}",
        "0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6f\n",
        " 0b5f3c3e-1d2a-4c4b-9e7f-2a1b3c4d5e6f",
    ] {
        for agent in ["claude", "codex", "copilot", "qwen"] {
            assert!(resume_argv(agent, id).is_err(), "{agent} {id:?}");
        }
    }
    let max = format!("ses_{}", "a".repeat(508));
    assert_eq!(argv("opencode", &max)[2], max, "512 bytes");
    let over = format!("ses_{}", "a".repeat(509));
    for id in [over.as_str(), "SES_abc", "ses_a.b", "ses_a_b", "xses_a", "ses_é", "ses_a/b", r"ses_a\b"] {
        assert!(resume_argv("opencode", id).is_err(), "{id:?}");
    }
    // Agent names are exact.
    for agent in ["", "Claude", "claude ", "claude-code", "../claude"] {
        let e = resume_argv(agent, UUID).unwrap_err();
        assert!(e.contains("no resume command"), "{agent:?}: {e}");
    }
}

#[test]
fn trash_finds_nothing_when_every_part_of_a_session_is_a_link() {
    let h = Home::new();
    let outside = Home::new();
    let target = outside.put("s.jsonl", jsonl(&[said("/home/u/app")]).as_bytes(), 1);
    let side = outside.put("side/subagents/agent-1.jsonl", b"{}\n", 1);
    let project = h.0.join(".claude/projects/-home-u-app");
    if !link(&target, &project.join(format!("{UUID}.jsonl"))) {
        return; // this OS account cannot make links
    }
    assert!(link(&outside.0.join("side"), &project.join(UUID)));
    assert_eq!(kind(trash("claude", UUID, &h.roots())), "not_found", "a linked file and a linked folder");
    // OpenCode: the session file, its messages and a whole project folder as links.
    let storage = h.0.join(".local/share/opencode/storage");
    let json = outside.put("ses_1.json", br#"{"id":"ses_1"}"#, 1);
    let messages = outside.put("msgs/msg_1.json", b"{}", 1);
    assert!(link(&json, &storage.join("session/p1/ses_1.json")));
    assert!(link(messages.parent().unwrap(), &storage.join("message/ses_1")));
    let elsewhere = outside.put("proj/ses_2.json", br#"{"id":"ses_2"}"#, 1);
    assert!(link(elsewhere.parent().unwrap(), &storage.join("session/p2")));
    assert_eq!(kind(trash("opencode", "ses_1", &h.roots())), "not_found");
    assert_eq!(kind(trash("opencode", "ses_2", &h.roots())), "not_found");
    assert!(target.exists() && side.exists() && json.exists() && messages.exists() && elsewhere.exists());
}

#[test]
fn trash_reads_an_opencode_database_without_the_session_as_holding_none() {
    // A database holding other sessions only: not found, not `unsupported`.
    let h = Home::new();
    opencode_db(&h, OPENCODE_TABLE, &[("ses_other", None, "/home/u/app", "Other", 1, None)]);
    assert_eq!(kind(trash("opencode", "ses_1", &h.roots())), "not_found");
    // Not a database, or a folder where it should be: holds none.
    let garbage = Home::new();
    let db = garbage.put(".local/share/opencode/opencode.db", b"not sqlite", 1);
    assert_eq!(kind(trash("opencode", "ses_1", &garbage.roots())), "not_found");
    assert_eq!(std::fs::read(&db).unwrap(), b"not sqlite", "read-only");
    let folder = Home::new();
    folder.put(".local/share/opencode/opencode.db/x", b"", 1);
    // A folder in the database's place is not a plain file: refused (ADR-056 S2 addendum).
    assert_eq!(kind(trash("opencode", "ses_1", &folder.roots())), "unsupported");
}

#[test]
fn the_trash_is_found_under_xdg_data_home_or_home_and_windows_store_paths_are_kept() {
    let roots = Roots::new(|name| (name == "HOME").then(|| "/home/u".to_string()));
    assert_eq!(roots.trash, Some(Path::new("/home/u").join(".local").join("share").join("Trash")));
    let empty = Roots::new(|name| match name {
        "HOME" => Some("/home/u".into()),
        "XDG_DATA_HOME" => Some(String::new()),
        _ => None,
    });
    assert_eq!(empty.trash, roots.trash, "an empty XDG_DATA_HOME is unset");
    assert!(Roots::new(|_| None).trash.is_none());
    let win = Roots::new(|name| match name {
        "XDG_DATA_HOME" => Some(r"Z:\h0x-none\data".into()),
        "CLAUDE_CONFIG_DIR" => Some(r"Z:\h0x-none\Me\.claude".into()),
        "CODEX_HOME" => Some(r"Z:\h0x-none\Me\.codex".into()),
        _ => None,
    });
    assert_eq!(win.trash, Some(Path::new(r"Z:\h0x-none\data").join("Trash")));
    assert_eq!(win.opencode, Some(Path::new(r"Z:\h0x-none\data").join("opencode")));
    assert_eq!(win.claude, Some(PathBuf::from(r"Z:\h0x-none\Me\.claude")));
    assert_eq!(win.codex, Some(PathBuf::from(r"Z:\h0x-none\Me\.codex")));
    // Nothing there: not found, never an error about the path.
    assert_eq!(kind(trash("claude", UUID, &win)), "not_found");
    assert_eq!(kind(trash("codex", UUID, &win)), "not_found");
    assert_eq!(kind(trash("opencode", "ses_1", &win)), "not_found");
}

#[cfg(windows)]
#[test]
fn trash_on_windows_refuses_a_store_on_a_share_without_a_recycle_bin() {
    let h = Home::new();
    let session = h.claude("C--Users-u-app", UUID, &[said(r"C:\Users\u\app")], 1);
    let local = h.0.display().to_string();
    let Some((drive, rest)) = local.split_once(r":\") else { return };
    let share = format!(r"\\localhost\{drive}$\{rest}\.claude");
    if !Path::new(&share).join("projects").is_dir() {
        return; // admin shares are off here
    }
    let roots = Roots::new(|name| (name == "CLAUDE_CONFIG_DIR").then(|| share.clone()));
    assert_eq!(kind(trash("claude", UUID, &roots)), "unsupported");
    assert!(session.exists(), "nothing moved or deleted");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn trash_takes_the_first_free_trash_name_past_dot_two() {
    let h = Home::new();
    let session = h.claude("-p", UUID, &[said("/p")], 1);
    let name = format!("{UUID}.jsonl");
    h.put(&format!(".local/share/Trash/info/{name}.trashinfo"), b"[Trash Info]\n", 1);
    h.put(&format!(".local/share/Trash/info/{name}.2.trashinfo"), b"[Trash Info]\n", 1);
    // A file with no record of its own still holds its name.
    let squatter = h.put(&format!(".local/share/Trash/files/{name}.3"), b"other", 1);
    assert_eq!(trash("claude", UUID, &h.roots()).unwrap(), std::slice::from_ref(&session));
    let trash_dir = h.0.join(".local/share/Trash");
    assert!(trash_dir.join(format!("files/{name}.4")).is_file() && !session.exists());
    assert!(trash_dir.join(format!("info/{name}.4.trashinfo")).is_file());
    assert!(!trash_dir.join(format!("info/{name}.3.trashinfo")).exists(), "no record for a name it could not use");
    assert_eq!(std::fs::read(&squatter).unwrap(), b"other");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn trash_moves_only_the_real_half_of_a_session_beside_a_link() {
    let h = Home::new();
    let outside = Home::new();
    let session = h.claude("-p", UUID, &[said("/p")], 1);
    let side = outside.put("side/subagents/agent-1.jsonl", b"{}\n", 1);
    let linked = h.0.join(format!(".claude/projects/-p/{UUID}"));
    if !link(&outside.0.join("side"), &linked) {
        return;
    }
    assert_eq!(trash("claude", UUID, &h.roots()).unwrap(), std::slice::from_ref(&session));
    let json = h.opencode("ses_1", "/p", 1);
    let messages = outside.put("msgs/msg_1.json", b"{}", 1);
    let linked_messages = h.0.join(".local/share/opencode/storage/message/ses_1");
    assert!(link(messages.parent().unwrap(), &linked_messages));
    assert_eq!(trash("opencode", "ses_1", &h.roots()).unwrap(), std::slice::from_ref(&json));
    assert!(!session.exists() && !json.exists());
    assert!(std::fs::symlink_metadata(&linked).is_ok() && std::fs::symlink_metadata(&linked_messages).is_ok());
    assert!(side.exists() && messages.exists(), "nothing behind a link is touched");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn trash_without_a_usable_home_trash_refuses_and_keeps_the_session() {
    let h = Home::new();
    let session = h.claude("-p", UUID, &[said("/p")], 1);
    let root = h.0.join(".claude").display().to_string();
    let no_home = Roots::new(|name| (name == "CLAUDE_CONFIG_DIR").then(|| root.clone()));
    assert!(no_home.trash.is_none());
    assert_eq!(kind(trash("claude", UUID, &no_home)), "unsupported");
    // A file where the Trash folder should be.
    h.put(".local/share/Trash", b"not a folder", 1);
    assert_eq!(kind(trash("claude", UUID, &h.roots())), "unavailable");
    assert!(session.exists(), "never deleted");
}

#[test]
fn a_claude_session_folder_without_its_jsonl_is_not_a_session() {
    let h = Home::new();
    let side = h.put(&format!(".claude/projects/-home-u-app/{UUID}/subagents/agent-1.jsonl"), b"{}\n", 1);
    assert!(h.list("/home/u/app").is_empty(), "session.list does not show it");
    assert_eq!(kind(trash("claude", UUID, &h.roots())), "not_found");
    assert!(side.exists(), "left where it is");
}

#[test]
fn letta_default_ids_are_held_to_the_limit_whole() {
    let fits = format!("default:{}", "a".repeat(512 - "default:".len()));
    assert_eq!(argv("letta", &fits)[4].len(), 504);
    let over = format!("default:{}", "a".repeat(512 - "default:".len() + 1));
    assert_eq!(over.len(), 513);
    assert!(resume_argv("letta", &over).is_err());
    assert!(resume_argv("letta", &format!("default:{}", "a".repeat(512))).is_err(), "520 bytes");
}

#[test]
fn trash_refuses_opencode_when_its_database_is_not_a_plain_file() {
    let h = Home::new();
    let json = h.opencode("ses_1", "/home/u/app", 1);
    let outside = Home::new();
    let elsewhere = opencode_db(&outside, OPENCODE_TABLE, &[]);
    let db = h.0.join(".local/share/opencode/opencode.db");
    if link(&elsewhere, &db) {
        let e = trash("opencode", "ses_1", &h.roots()).unwrap_err();
        assert_eq!(e.kind, "unsupported", "a linked database: {e:?}");
        std::fs::remove_file(&db).or_else(|_| std::fs::remove_dir(&db)).unwrap();
    }
    std::fs::create_dir(&db).unwrap();
    assert_eq!(kind(trash("opencode", "ses_1", &h.roots())), "unsupported", "a folder");
    assert!(json.exists() && elsewhere.exists(), "nothing moved");
}

#[cfg(windows)]
#[test]
fn trash_on_windows_refuses_a_share_before_reading_it() {
    // A share that can never answer: refused from its name, not after a wait.
    let started = std::time::Instant::now();
    for agent in ["claude", "codex"] {
        let roots = Roots::new(|name| {
            matches!(name, "CLAUDE_CONFIG_DIR" | "CODEX_HOME").then(|| r"\\h0x-none.invalid\share\store".to_string())
        });
        assert_eq!(kind(trash(agent, UUID, &roots)), "unsupported", "{agent}");
    }
    let roots = Roots::new(|name| (name == "XDG_DATA_HOME").then(|| r"\\?\UNC\h0x-none.invalid\share".to_string()));
    assert_eq!(kind(trash("opencode", "ses_1", &roots)), "unsupported");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    // A drive letter that is not there holds nothing.
    let roots = Roots::new(|name| (name == "CLAUDE_CONFIG_DIR").then(|| r"Z:\h0x-none\.claude".to_string()));
    if !Path::new(r"Z:\").exists() {
        assert_eq!(kind(trash("claude", UUID, &roots)), "not_found");
    }
}
