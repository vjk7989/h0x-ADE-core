//! pi: `<root>/sessions/--<folder>--/<time>_<id>.jsonl`, whose first line is
//! the header `{type: "session", id, cwd, timestamp}`. Last used is the file's
//! mtime. oh-my-pi (`omp.rs`) keeps the same layout under its own root.

use super::*;
use std::io::BufRead;

/// A header is a small JSON object.
const FIRST_LINE: u64 = 64 * 1024;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    or_home(env, "PI_CODING_AGENT_DIR", &[".pi", "agent"])
}

pub(super) fn list(root: &Path, want: &str) -> Vec<SessionInfo> {
    header_list(root, "pi", want)
}

/// The sessions of the folder under `<root>/sessions`; the header's `cwd` decides, not the folder name.
pub(super) fn header_list(root: &Path, agent: &str, want: &str) -> Vec<SessionInfo> {
    dirs(&root.join("sessions"))
        .flat_map(|dir| files(&dir, "jsonl"))
        .filter_map(|path| read(&path, agent, want))
        .collect()
}

fn read(path: &Path, agent: &str, want: &str) -> Option<SessionInfo> {
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
    let (Some(id), Some(cwd)) = (first["id"].as_str(), first["cwd"].as_str()) else {
        skipped(path, "no session id and cwd");
        return None;
    };
    (first["type"] == "session" && normal(cwd) == want).then(|| SessionInfo {
        agent: agent.into(),
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

    fn header(id: &str, cwd: &str) -> Vec<u8> {
        format!(
            "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":{},\"timestamp\":\"t\"}}\n{{\"x\":1}}\n",
            serde_json::json!(cwd)
        )
        .into_bytes()
    }

    #[test]
    fn root_follows_env_then_home() {
        let set = |k: &str| (k == "PI_CODING_AGENT_DIR").then(|| "/p".to_string());
        assert_eq!(root(&set), Some(PathBuf::from("/p")));
        let home = |k: &str| (k == "HOME").then(|| "/h".to_string());
        assert_eq!(root(&home), Some(PathBuf::from("/h").join(".pi").join("agent")));
    }

    #[test]
    fn empty_and_missing_store() {
        let s = Scratch::new();
        assert!(list(s.path(), "/w").is_empty());
        assert!(list(&s.path().join("none"), "/w").is_empty());
        std::fs::create_dir_all(s.path().join("sessions").join("--w--")).unwrap();
        assert!(list(s.path(), "/w").is_empty());
    }

    #[test]
    fn bad_first_lines_are_skipped() {
        let s = Scratch::new();
        s.put("sessions/--w--/a.jsonl", b"{\"type\":\"session\",\"id\":\"a\",\"cw", 1);
        s.put("sessions/--w--/b.jsonl", b"not json\n", 1);
        s.put("sessions/--w--/c.jsonl", b"", 1);
        s.put(
            "sessions/--w--/d.jsonl",
            b"{\"type\":\"message\",\"id\":\"d\",\"cwd\":\"/w\"}
",
            1,
        );
        assert!(list(s.path(), "/w").is_empty());
    }

    #[test]
    fn other_folders_are_left_out() {
        let s = Scratch::new();
        s.put("sessions/--w--/a.jsonl", &header("a", "/w"), 5);
        s.put("sessions/--w--/b.jsonl", &header("b", "/other"), 6);
        let found = list(s.path(), "/w");
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].agent.as_str(), found[0].id.as_str(), found[0].last_used), ("pi", "a", 5));
    }

    #[test]
    fn windows_drive_case_matches() {
        let s = Scratch::new();
        s.put("sessions/--C--Proj--/a.jsonl", &header("a", r"C:\Users\X\Proj"), 1);
        let found = list(s.path(), &normal(r"c:/users/x/proj"));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn newest_mtime_is_last_used() {
        let s = Scratch::new();
        s.put("sessions/--w--/a.jsonl", &header("a", "/w"), 10);
        s.put("sessions/--v--/b.jsonl", &header("b", "/w"), 30);
        let mut found = list(s.path(), "/w");
        found.sort_by_key(|f| std::cmp::Reverse(f.last_used));
        let ids: Vec<(&str, u64)> = found.iter().map(|f| (f.id.as_str(), f.last_used)).collect();
        assert_eq!(ids, [("b", 30), ("a", 10)]);
    }

    #[test]
    fn oversized_first_line_is_skipped() {
        let s = Scratch::new();
        let big =
            format!("{{\"type\":\"session\",\"id\":\"a\",\"cwd\":\"/w\",\"pad\":\"{}\"}}\n", "x".repeat(70 * 1024));
        s.put("sessions/--w--/a.jsonl", big.as_bytes(), 1);
        assert!(list(s.path(), "/w").is_empty());
    }

    /// A header line of exactly `len` bytes before its newline.
    fn padded(len: usize) -> Vec<u8> {
        let bare = "{\"type\":\"session\",\"id\":\"a\",\"cwd\":\"/w\",\"pad\":\"\"}".len();
        format!("{{\"type\":\"session\",\"id\":\"a\",\"cwd\":\"/w\",\"pad\":\"{}\"}}\n", "x".repeat(len - bare))
            .into_bytes()
    }

    #[test]
    fn finder_edge_first_line_limit_on_both_sides() {
        let s = Scratch::new();
        s.put("sessions/--w--/a.jsonl", &padded(64 * 1024), 1);
        assert_eq!(list(s.path(), "/w").len(), 1, "a 64 KiB header is read");
        s.put("sessions/--w--/a.jsonl", &padded(64 * 1024 + 1), 1);
        assert!(list(s.path(), "/w").is_empty(), "one byte past 64 KiB is not");
    }

    #[test]
    fn finder_edge_root_and_sessions_are_files() {
        let s = Scratch::new();
        let file = s.put("root-file", &header("a", "/w"), 1);
        assert!(list(&file, "/w").is_empty());
        s.put("sessions", b"not a folder", 1);
        assert!(list(s.path(), "/w").is_empty());
    }

    #[test]
    fn finder_edge_crlf_and_bom_headers() {
        let s = Scratch::new();
        s.put("sessions/--w--/a.jsonl", b"{\"type\":\"session\",\"id\":\"a\",\"cwd\":\"/w\"}\r\n{}\r\n", 3);
        let found = list(s.path(), "/w");
        assert_eq!(found.len(), 1, "a CRLF header is still one header");
        assert_eq!((found[0].id.as_str(), found[0].last_used), ("a", 3));
        let s = Scratch::new();
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend(header("b", "/w"));
        s.put("sessions/--w--/b.jsonl", &bom, 1);
        // Never a panic; if listed, by its own id.
        assert!(list(s.path(), "/w").iter().all(|f| f.id == "b"));
    }

    #[test]
    fn finder_edge_cwd_with_trailing_slash_or_dots_matches() {
        let s = Scratch::new();
        s.put("sessions/--a--/a.jsonl", &header("a", "/w/"), 1);
        s.put("sessions/--b--/b.jsonl", &header("b", "/w/sub/.."), 2);
        s.put("sessions/--c--/c.jsonl", &header("c", "/w/./"), 3);
        s.put("sessions/--d--/d.jsonl", &header("d", "/w/sub"), 4);
        let mut ids: Vec<String> = list(s.path(), &normal("/w")).into_iter().map(|f| f.id).collect();
        ids.sort();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn finder_edge_unc_and_verbatim_prefixes() {
        let s = Scratch::new();
        s.put("sessions/--u--/u.jsonl", &header("u", r"\\Srv\Share\Proj\"), 1);
        s.put("sessions/--v--/v.jsonl", &header("v", r"\\?\C:\Work\App"), 2);
        let unc: Vec<String> = list(s.path(), &normal(r"\\?\UNC\srv\share\proj")).into_iter().map(|f| f.id).collect();
        assert_eq!(unc, ["u"]);
        let drive: Vec<String> = list(s.path(), &normal("c:/work/app")).into_iter().map(|f| f.id).collect();
        assert_eq!(drive, ["v"]);
    }

    #[test]
    fn finder_edge_missing_id_or_cwd_or_wrong_types() {
        let s = Scratch::new();
        s.put("sessions/--w--/a.jsonl", b"{\"type\":\"session\",\"cwd\":\"/w\"}\n", 1);
        s.put("sessions/--w--/b.jsonl", b"{\"type\":\"session\",\"id\":\"b\"}\n", 1);
        s.put("sessions/--w--/c.jsonl", b"{\"type\":\"session\",\"id\":7,\"cwd\":\"/w\"}\n", 1);
        s.put("sessions/--w--/d.jsonl", b"[1,2]\n", 1);
        s.put("sessions/--w--/e.jsonl", b"null\n", 1);
        s.put("sessions/--w--/f.json", &header("f", "/w"), 1);
        assert!(list(s.path(), "/w").is_empty());
        assert!(list(s.path(), "").is_empty());
    }

    #[test]
    fn finder_edge_empty_env_falls_back_to_home() {
        let env = |k: &str| match k {
            "PI_CODING_AGENT_DIR" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/h").join(".pi").join("agent")));
        let profile = |k: &str| match k {
            "HOME" => Some(String::new()),
            "USERPROFILE" => Some(r"C:\Users\u".to_string()),
            _ => None,
        };
        assert_eq!(root(&profile), Some(PathBuf::from(r"C:\Users\u").join(".pi").join("agent")));
        assert_eq!(root(&|_| Some(String::new())), None);
    }

    #[cfg(unix)]
    #[test]
    fn finder_edge_links_unreadable_and_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new();
        let outside = Scratch::new();
        outside.put("proj/x.jsonl", &header("x", "/w"), 1);
        let away = outside.put("y.jsonl", &header("y", "/w"), 1);
        std::fs::create_dir_all(s.path().join("sessions/--w--")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("proj"), s.path().join("sessions/--l--")).unwrap();
        std::os::unix::fs::symlink(&away, s.path().join("sessions/--w--/y.jsonl")).unwrap();
        s.put("sessions/--w--/ok.jsonl", &header("ok", "/w"), 5);
        let name = std::ffi::OsStr::from_bytes(b"\xff\xfe.jsonl");
        // macOS file systems refuse names that are not UTF-8: there is nothing to skip there.
        let raw_made = std::fs::write(s.path().join("sessions/--w--").join(name), header("raw", "/w")).is_ok();
        let shut = s.put("sessions/--shut--/z.jsonl", &header("z", "/w"), 1);
        let shut_dir = shut.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&shut_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut ids: Vec<String> = list(s.path(), "/w").into_iter().map(|f| f.id).collect();
        std::fs::set_permissions(&shut_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ids.sort();
        assert!(!ids.contains(&"x".to_string()) && !ids.contains(&"y".to_string()), "links are not followed: {ids:?}");
        assert!(ids.contains(&"ok".to_string()), "{ids:?}");
        // The id comes from the header, whatever the file is called.
        assert!(!raw_made || ids.contains(&"raw".to_string()), "{ids:?}");
    }
}
