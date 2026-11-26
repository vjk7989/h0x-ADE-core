//! Qwen Code: `<root>/projects/<folder>/chats/<session id>.jsonl`, the root
//! being `$QWEN_HOME`, else `~/.qwen`. Every record has `sessionId` and `cwd`:
//! the first one decides the folder (the directory name is a lossy encoding),
//! and `sessionId` must be the file name.

use super::*;

/// Read from the start of a session file.
const FIRST: u64 = 64 * 1024;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    or_home(env, "QWEN_HOME", &[".qwen"])
}

pub(super) fn list(root: &Path, want: &str) -> Vec<SessionInfo> {
    dirs(&root.join("projects"))
        .map(|project| project.join("chats"))
        // A real folder, not a link out of the store.
        .filter(|chats| std::fs::symlink_metadata(chats).is_ok_and(|m| m.is_dir()))
        .flat_map(|chats| files(&chats, "jsonl"))
        .filter_map(|path| read(&path, want))
        .collect()
}

fn read(path: &Path, want: &str) -> Option<SessionInfo> {
    let id = path.file_stem()?.to_str()?;
    let (mut file, meta) = open(path)?;
    let bytes = head(path, &mut file, FIRST)?;
    let first: serde_json::Value = match serde_json::from_slice(bytes.split(|&c| c == b'\n').next()?) {
        Ok(first) => first,
        Err(e) => {
            skipped(path, e);
            return None;
        }
    };
    if first["sessionId"].as_str() != Some(id) || normal(first["cwd"].as_str()?) != want {
        return None;
    }
    Some(SessionInfo {
        agent: "qwen".into(),
        id: id.into(),
        title: None,
        last_used: modified_ms(&meta),
        path: path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixture::Scratch;
    use super::*;

    fn line(id: &str, cwd: &str) -> Vec<u8> {
        let rec = serde_json::json!({"sessionId": id, "cwd": cwd, "type": "user"});
        format!("{rec}\n{rec}\n").into_bytes()
    }

    #[test]
    fn root_from_env_or_home() {
        let env = |n: &str| (n == "QWEN_HOME").then(|| "/q".to_string());
        assert_eq!(root(&env), Some(PathBuf::from("/q")));
        let env = |n: &str| (n == "HOME").then(|| "/h".to_string());
        assert_eq!(root(&env), Some(PathBuf::from("/h").join(".qwen")));
        assert_eq!(root(&|_| None), None);
    }

    #[test]
    fn missing_root_and_empty_store_list_nothing() {
        assert!(list(Path::new("/no/such/qwen"), "/w/a").is_empty());
        let s = Scratch::new();
        assert!(list(s.path(), "/w/a").is_empty());
        std::fs::create_dir_all(s.path().join("projects/p/chats")).unwrap();
        assert!(list(s.path(), "/w/a").is_empty());
    }

    #[test]
    fn newest_mtime_and_other_folders_left_out() {
        let s = Scratch::new();
        s.put("projects/-w-a/chats/s1.jsonl", &line("s1", "/w/a"), 1_000);
        s.put("projects/-w-a/chats/s2.jsonl", &line("s2", "/w/a"), 8_000);
        s.put("projects/-w-b/chats/s3.jsonl", &line("s3", "/w/b"), 9_000);
        let mut got = list(s.path(), "/w/a");
        got.sort_by_key(|g| std::cmp::Reverse(g.last_used));
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].id.as_str(), got[0].last_used, got[0].agent.as_str()), ("s2", 8_000, "qwen"));
        assert_eq!(got[1].id, "s1");
    }

    #[test]
    fn the_cwd_decides_not_the_directory_name() {
        let s = Scratch::new();
        s.put("projects/-w-b/chats/s1.jsonl", &line("s1", "/w/a"), 1);
        assert_eq!(list(s.path(), "/w/a").len(), 1);
        assert!(list(s.path(), "/w/b").is_empty());
    }

    #[test]
    fn stem_and_session_id_must_match() {
        let s = Scratch::new();
        s.put("projects/p/chats/s1.jsonl", &line("other", "/w/a"), 1);
        s.put("projects/p/chats/s2.jsonl", br#"{"cwd":"/w/a"}"#, 1);
        assert!(list(s.path(), "/w/a").is_empty());
    }

    #[test]
    fn corrupt_first_line_is_skipped() {
        let s = Scratch::new();
        s.put("projects/p/chats/s1.jsonl", b"not json\n", 1);
        s.put("projects/p/chats/s2.jsonl", b"", 1);
        let mut later = b"garbage\n".to_vec();
        later.extend(line("s3", "/w/a"));
        s.put("projects/p/chats/s3.jsonl", &later, 1);
        s.put("projects/p/chats/s4.txt", &line("s4", "/w/a"), 1);
        assert!(list(s.path(), "/w/a").is_empty());
    }

    #[test]
    fn windows_drive_case_matches_through_normal() {
        let s = Scratch::new();
        s.put("projects/c--Users-u-p/chats/s1.jsonl", &line("s1", r"C:\Users\U\p"), 3);
        let got = list(s.path(), &normal("c:/users/u/p/"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "s1");
    }

    /// A first record of exactly `len` bytes before its newline.
    fn padded(id: &str, len: usize) -> Vec<u8> {
        let bare = format!("{{\"sessionId\":\"{id}\",\"cwd\":\"/w/a\",\"pad\":\"\"}}").len();
        format!("{{\"sessionId\":\"{id}\",\"cwd\":\"/w/a\",\"pad\":\"{}\"}}\n{{}}\n", "x".repeat(len - bare))
            .into_bytes()
    }

    #[test]
    fn finder_edge_first_line_limit_on_both_sides() {
        let s = Scratch::new();
        s.put("projects/p/chats/s1.jsonl", &padded("s1", 64 * 1024), 1);
        s.put("projects/p/chats/s2.jsonl", &padded("s2", 64 * 1024 + 1), 1);
        let ids: Vec<String> = list(s.path(), "/w/a").into_iter().map(|g| g.id).collect();
        assert_eq!(ids, ["s1"]);
    }

    #[test]
    fn finder_edge_root_and_store_parts_that_are_files() {
        let s = Scratch::new();
        let file = s.put("f", &line("s1", "/w/a"), 1);
        assert!(list(&file, "/w/a").is_empty());
        s.put("projects", b"a file", 1);
        assert!(list(s.path(), "/w/a").is_empty());
        let t = Scratch::new();
        t.put("projects/p/chats", b"a file", 1);
        std::fs::create_dir_all(t.path().join("projects/q/chats/s1.jsonl")).unwrap();
        assert!(list(t.path(), "/w/a").is_empty());
    }

    #[test]
    fn finder_edge_crlf_trailing_slash_dots_and_bom() {
        let s = Scratch::new();
        s.put("projects/p/chats/s1.jsonl", b"{\"sessionId\":\"s1\",\"cwd\":\"/w/a/\"}\r\n{}\r\n", 11);
        s.put("projects/p/chats/s2.jsonl", &line("s2", "/w/b/../a/."), 12);
        s.put("projects/p/chats/s3.jsonl", &line("s3", "/w/a/b"), 13);
        let mut got = list(s.path(), "/w/a");
        got.sort_by_key(|g| g.last_used);
        let ids: Vec<(&str, u64)> = got.iter().map(|g| (g.id.as_str(), g.last_used)).collect();
        assert_eq!(ids, [("s1", 11), ("s2", 12)]);
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend(line("s4", "/w/a"));
        let t = Scratch::new();
        t.put("projects/p/chats/s4.jsonl", &bom, 1);
        assert!(list(t.path(), "/w/a").iter().all(|g| g.id == "s4"), "never a panic");
    }

    #[test]
    fn finder_edge_unc_and_verbatim_prefixes() {
        let s = Scratch::new();
        s.put("projects/u/chats/s1.jsonl", &line("s1", r"\\Srv\Share\P\"), 1);
        s.put("projects/v/chats/s2.jsonl", &line("s2", r"\\?\C:\Work\App"), 2);
        let unc: Vec<String> = list(s.path(), &normal(r"\\?\UNC\srv\share\p")).into_iter().map(|g| g.id).collect();
        assert_eq!(unc, ["s1"]);
        let drive: Vec<String> = list(s.path(), &normal("c:/work/app")).into_iter().map(|g| g.id).collect();
        assert_eq!(drive, ["s2"]);
    }

    #[test]
    fn finder_edge_cwd_missing_or_not_text() {
        let s = Scratch::new();
        s.put("projects/p/chats/s1.jsonl", br#"{"sessionId":"s1","cwd":5}"#, 1);
        s.put("projects/p/chats/s2.jsonl", br#"{"sessionId":"s2"}"#, 1);
        s.put("projects/p/chats/s3.jsonl", br#"{"sessionId":"s3","cwd":null}"#, 1);
        s.put("projects/p/chats/.jsonl", &line("", "/w/a"), 1);
        assert!(list(s.path(), "/w/a").is_empty());
        assert!(list(s.path(), "").is_empty());
    }

    #[test]
    fn finder_edge_empty_env_falls_back_to_home() {
        let env = |n: &str| match n {
            "QWEN_HOME" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/h").join(".qwen")));
    }

    #[cfg(unix)]
    #[test]
    fn finder_edge_links_unreadable_and_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new();
        let outside = Scratch::new();
        outside.put("chats/s1.jsonl", &line("s1", "/w/a"), 1);
        outside.put("proj/chats/s2.jsonl", &line("s2", "/w/a"), 1);
        std::fs::create_dir_all(s.path().join("projects/p")).unwrap();
        // A linked `chats` inside a real project folder leads out of the store.
        std::os::unix::fs::symlink(outside.path().join("chats"), s.path().join("projects/p/chats")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("proj"), s.path().join("projects/l")).unwrap();
        s.put("projects/ok/chats/s3.jsonl", &line("s3", "/w/a"), 1);
        let name = std::ffi::OsStr::from_bytes(b"\xff.jsonl");
        // macOS file systems refuse names that are not UTF-8: there is nothing to skip there.
        let _ = std::fs::write(s.path().join("projects/ok/chats").join(name), line("\u{fffd}", "/w/a"));
        let shut = s.put("projects/shut/chats/s4.jsonl", &line("s4", "/w/a"), 1);
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
        let ids: Vec<String> = list(s.path(), "/w/a").into_iter().map(|g| g.id).collect();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ids.contains(&"s3".to_string()), "{ids:?}");
        assert!(!ids.iter().any(|i| i == "s1" || i == "s2" || i == "\u{fffd}"), "{ids:?}");
    }
}
