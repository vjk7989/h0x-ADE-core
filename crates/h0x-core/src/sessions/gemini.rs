//! Gemini CLI: `<home>/.gemini/tmp/<slug>/chats/session-<time>-<id8>.jsonl`,
//! whose first line holds `sessionId`; `<slug>/.project_root` holds the
//! project folder. Other `.jsonl` files there (a subagent's) are not sessions.

use super::*;

/// A project marker is one path; a session's first line is a small header.
const MAX: u64 = 64 * 1024;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    // The home Gemini uses outside its macOS sandbox.
    let home = var(env, "HOME").or_else(|| var(env, "USERPROFILE"))?;
    Some(home.join(".gemini"))
}

pub(super) fn list(home: &Path, want: &str) -> Vec<SessionInfo> {
    dirs(&home.join("tmp"))
        .filter(|slug| owner(slug).is_some_and(|folder| normal(&folder) == want))
        .map(|slug| slug.join("chats"))
        // A real folder in the store, never a link out of it.
        .filter(|chats| std::fs::symlink_metadata(chats).is_ok_and(|m| m.is_dir()))
        .flat_map(|chats| files(&chats, "jsonl"))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("session-")))
        .filter_map(|path| read(&path))
        .collect()
}

/// The project folder a slug folder belongs to.
fn owner(slug: &Path) -> Option<String> {
    let marker = slug.join(".project_root");
    if !std::fs::symlink_metadata(&marker).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let (mut file, _) = open(&marker)?;
    let text = String::from_utf8(head(&marker, &mut file, MAX)?).ok()?;
    let folder = text.trim();
    (!folder.is_empty()).then(|| folder.to_string())
}

fn read(path: &Path) -> Option<SessionInfo> {
    let (mut file, meta) = open(path)?;
    let bytes = head(path, &mut file, MAX)?;
    let line = bytes.split(|&c| c == b'\n').next().unwrap_or_default();
    let first: serde_json::Value = match serde_json::from_slice(line) {
        Ok(first) => first,
        Err(e) => {
            skipped(path, format!("first line: {e}"));
            return None;
        }
    };
    let Some(id) = first["sessionId"].as_str().filter(|id| resume::is_uuid(id)) else {
        skipped(path, "no sessionId of a UUID shape");
        return None;
    };
    // The file is appended to as the chat goes on; the header's time is only its start.
    Some(SessionInfo {
        agent: "gemini".into(),
        id: id.into(),
        title: None,
        last_used: modified_ms(&meta),
        path: path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::Scratch;

    const ID: &str = "0a1b2c3d-0000-4000-8000-000000000001";

    fn header(id: &str) -> Vec<u8> {
        let line = serde_json::json!({
            "sessionId": id, "projectHash": "h", "startTime": "2026-03-01T10:00:00.000Z",
            "lastUpdated": "2026-03-01T10:00:00.000Z"
        });
        format!("{line}\n{{\"id\":\"m1\",\"type\":\"user\"}}\n").into_bytes()
    }

    fn project(h: &Scratch, slug: &str, folder: &str) {
        h.put(&format!(".gemini/tmp/{slug}/.project_root"), format!("{folder}\n").as_bytes(), 1);
    }

    fn chat(h: &Scratch, slug: &str, name: &str, bytes: &[u8], ms: u64) {
        h.put(&format!(".gemini/tmp/{slug}/chats/{name}"), bytes, ms);
    }

    fn found(h: &Scratch, want: &str) -> Vec<SessionInfo> {
        list(&h.path().join(".gemini"), want)
    }

    #[test]
    fn root_is_dot_gemini_in_home() {
        let home = |n: &str| (n == "HOME").then(|| "/home/u".to_string());
        assert_eq!(root(&home), Some(Path::new("/home/u").join(".gemini")));
        assert_eq!(root(&|_| None), None);
    }

    #[test]
    fn missing_store_lists_nothing() {
        let h = Scratch::new();
        assert!(found(&h, "/w").is_empty());
        // A slug folder without a marker, and a marker without chats.
        chat(&h, "nomark", &format!("session-2026-03-01T10-00-{ID}.jsonl"), &header(ID), 1);
        project(&h, "nochats", "/w");
        assert!(found(&h, "/w").is_empty());
    }

    #[test]
    fn project_match_and_newest_wins() {
        let h = Scratch::new();
        project(&h, "w", "/w");
        project(&h, "other", "/elsewhere");
        chat(&h, "w", "session-2026-03-01T10-00-0a1b2c3d.jsonl", &header(ID), 1000);
        chat(&h, "w", "session-2026-03-02T10-00-0b1b2c3d.jsonl", &header("0b1b2c3d-0000-4000-8000-000000000002"), 2000);
        chat(&h, "other", "session-2026-03-03T10-00-0c1b2c3d.jsonl", &header("0c1b2c3d"), 3000);
        let mut all = found(&h, "/w");
        all.sort_by_key(|s| std::cmp::Reverse(s.last_used));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "0b1b2c3d-0000-4000-8000-000000000002");
        assert_eq!(all[0].last_used, 2000);
        assert!(all.iter().all(|s| s.agent == "gemini" && s.title.is_none()));
        assert!(all[1].path.contains("session-2026-03-01T10-00-0a1b2c3d.jsonl"));
    }

    #[test]
    fn files_without_the_session_prefix_are_ignored() {
        let h = Scratch::new();
        project(&h, "w", "/w");
        chat(&h, "w", &format!("{ID}.jsonl"), &header(ID), 1);
        chat(&h, "w", "notes.jsonl", &header(ID), 1);
        chat(&h, "w", "session-2026-03-01T10-00-0a1b2c3d.json", &header(ID), 1);
        chat(&h, "w", "session-2026-03-01T10-00-0a1b2c3d.jsonl.bak", &header(ID), 1);
        // A subagent's chats live in a folder named for its parent.
        chat(&h, "w", &format!("{ID}/session-2026-03-01T10-00-0a1b2c3d.jsonl"), &header(ID), 1);
        assert!(found(&h, "/w").is_empty());
    }

    #[test]
    fn corrupt_files_are_left_out() {
        let h = Scratch::new();
        project(&h, "w", "/w");
        chat(&h, "w", "session-a.jsonl", b"", 1);
        chat(&h, "w", "session-b.jsonl", b"{not json\n", 1);
        chat(&h, "w", "session-c.jsonl", b"{\"projectHash\":\"h\"}\n", 1);
        chat(&h, "w", "session-d.jsonl", b"{\"sessionId\":\"\"}\n", 1);
        chat(&h, "w", "session-e.jsonl", b"{\"sessionId\":7}\n", 1);
        chat(&h, "w", "session-f.jsonl", &[0xff, 0xfe, b'\n'], 1);
        chat(&h, "w", "session-ok.jsonl", &header(ID), 5);
        let all = found(&h, "/w");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, ID);
    }

    #[test]
    fn bad_markers_leave_the_project_out() {
        let h = Scratch::new();
        project(&h, "empty", "");
        h.put(".gemini/tmp/binary/.project_root", &[0xff, 0xfe], 1);
        for slug in ["empty", "binary"] {
            chat(&h, slug, "session-x.jsonl", &header(ID), 1);
        }
        assert!(found(&h, "/w").is_empty());
        assert!(found(&h, "").is_empty());
    }

    #[test]
    fn windows_drive_case_matches() {
        let h = Scratch::new();
        project(&h, "app", r"C:\Work\App");
        chat(&h, "app", "session-x.jsonl", &header(ID), 1);
        assert_eq!(found(&h, &normal("c:/work/app/")).len(), 1);
        assert!(found(&h, &normal(r"C:\Work\Other")).is_empty());
    }

    #[test]
    fn finder_edge_project_root_with_newlines_slashes_and_prefixes() {
        let h = Scratch::new();
        h.put(".gemini/tmp/a/.project_root", b"/w/\r\n", 1);
        h.put(".gemini/tmp/b/.project_root", b"  /w/x/..\n\n", 1);
        h.put(".gemini/tmp/c/.project_root", br"\\?\UNC\Srv\Share\P", 1);
        h.put(".gemini/tmp/d/.project_root", b"/w/x", 1);
        chat(&h, "a", "session-a.jsonl", &header("aaaaaaaa-0000-4000-8000-000000000001"), 1);
        chat(&h, "b", "session-b.jsonl", &header("bbbbbbbb-0000-4000-8000-000000000002"), 2);
        chat(&h, "c", "session-c.jsonl", &header("cccccccc-0000-4000-8000-000000000003"), 3);
        chat(&h, "d", "session-d.jsonl", &header("dddddddd-0000-4000-8000-000000000004"), 4);
        let mut ids: Vec<String> = found(&h, "/w").into_iter().map(|s| s.id).collect();
        ids.sort();
        assert_eq!(ids, ["aaaaaaaa-0000-4000-8000-000000000001", "bbbbbbbb-0000-4000-8000-000000000002"]);
        let unc: Vec<String> = found(&h, &normal(r"\\srv\share\p\")).into_iter().map(|s| s.id).collect();
        assert_eq!(unc, ["cccccccc-0000-4000-8000-000000000003"]);
    }

    #[test]
    fn finder_edge_session_id_mismatching_the_name_is_the_headers() {
        let h = Scratch::new();
        project(&h, "w", "/w");
        // The name's id8 says 0a1b2c3d; the header names another session.
        chat(&h, "w", "session-2026-03-01T10-00-0a1b2c3d.jsonl", &header("ffffffff-0000-4000-8000-00000000000f"), 9);
        let all = found(&h, "/w");
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].id.as_str(), all[0].last_used), ("ffffffff-0000-4000-8000-00000000000f", 9));
    }

    #[test]
    fn finder_edge_only_session_star_jsonl_counts() {
        let h = Scratch::new();
        project(&h, "w", "/w");
        chat(&h, "w", "Session-x.jsonl", &header(ID), 1);
        chat(&h, "w", "session-x.JSONL", &header(ID), 1);
        chat(&h, "w", "xsession-x.jsonl", &header(ID), 1);
        chat(&h, "w", "session-x", &header(ID), 1);
        // A session file at the slug's top, not in chats.
        h.put(".gemini/tmp/w/session-top.jsonl", &header(ID), 1);
        assert!(found(&h, "/w").is_empty());
        chat(&h, "w", "session-.jsonl", &header(ID), 2);
        assert_eq!(found(&h, "/w").len(), 1);
    }

    #[test]
    fn finder_edge_first_line_limit_crlf_and_bom() {
        let h = Scratch::new();
        project(&h, "w", "/w");
        let bare = format!("{{\"sessionId\":\"{ID}\",\"pad\":\"\"}}").len();
        let line = |len: usize| format!("{{\"sessionId\":\"{ID}\",\"pad\":\"{}\"}}\n", "x".repeat(len - bare));
        chat(&h, "w", "session-fit.jsonl", line(64 * 1024).as_bytes(), 1);
        assert_eq!(found(&h, "/w").len(), 1, "a 64 KiB first line is read");
        chat(&h, "w", "session-fit.jsonl", line(64 * 1024 + 1).as_bytes(), 1);
        assert!(found(&h, "/w").is_empty(), "one byte past 64 KiB is not");
        chat(&h, "w", "session-fit.jsonl", format!("{{\"sessionId\":\"{ID}\"}}\r\n{{}}\r\n").as_bytes(), 1);
        assert_eq!(found(&h, "/w").len(), 1, "a CRLF first line");
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend(header(ID));
        chat(&h, "w", "session-fit.jsonl", &bom, 1);
        assert!(found(&h, "/w").iter().all(|s| s.id == ID), "never a panic");
    }

    #[test]
    fn finder_edge_store_parts_that_are_files_or_folders() {
        let h = Scratch::new();
        let file = h.put("home-file", b"x", 1);
        assert!(list(&file, "/w").is_empty(), "the home is a file");
        h.put(".gemini/tmp", b"a file", 1);
        assert!(found(&h, "/w").is_empty());
        let g = Scratch::new();
        std::fs::create_dir_all(g.path().join(".gemini/tmp/w/.project_root")).unwrap();
        chat(&g, "w", "session-x.jsonl", &header(ID), 1);
        g.put(".gemini/tmp/v/.project_root", b"/w", 1);
        g.put(".gemini/tmp/v/chats", b"a file", 1);
        std::fs::create_dir_all(g.path().join(".gemini/tmp/v2/chats/session-d.jsonl")).unwrap();
        g.put(".gemini/tmp/v2/.project_root", b"/w", 1);
        assert!(found(&g, "/w").is_empty());
        // A marker larger than the limit is cut, never a panic.
        let big = Scratch::new();
        big.put(".gemini/tmp/w/.project_root", "/w".repeat(40 * 1024).as_bytes(), 1);
        chat(&big, "w", "session-x.jsonl", &header(ID), 1);
        assert!(found(&big, "/w").is_empty());
    }

    #[test]
    fn finder_edge_empty_home_falls_back_to_userprofile() {
        let env = |n: &str| match n {
            "HOME" => Some(String::new()),
            "USERPROFILE" => Some(r"C:\Users\u".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from(r"C:\Users\u").join(".gemini")));
        assert_eq!(root(&|_| Some(String::new())), None);
    }

    #[test]
    fn finder_edge_resume_row_takes_only_uuids() {
        assert_eq!(super::super::resume_argv("gemini", ID).unwrap(), ["gemini", "--resume", ID]);
        let upper = ID.to_uppercase();
        assert_eq!(super::super::resume_argv("gemini", &upper).unwrap()[2], upper);
        for bad in [
            "",
            "0a1b2c3d",
            "latest",
            "1",
            "--resume",
            "-0a1b2c3d-0000-4000-8000-00000000000",
            "0a1b2c3d-0000-4000-8000-0000000000011",
            "0a1b2c3d-0000-4000-8000-00000000000",
            "0a1b2c3d-0000-4000-8000-00000000000g",
            "0a1b2c3d00000-4000-8000-000000000001",
            " 0a1b2c3d-0000-4000-8000-00000000001",
            "0a1b2c3d-0000-4000-8000-00000000000\n",
            "session-2026-03-01T10-00-0a1b2c3d",
            r"C:\x\session.jsonl",
        ] {
            assert!(super::super::resume_argv("gemini", bad).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn finder_edge_links_unreadable_and_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let h = Scratch::new();
        let outside = Scratch::new();
        outside.put("chats/session-out.jsonl", &header("eeeeeeee-0000-4000-8000-00000000000e"), 1);
        let marker = outside.put("marker", b"/w", 1);
        // A linked `chats` inside a real slug folder leads out of the store.
        project(&h, "w", "/w");
        std::os::unix::fs::symlink(outside.path().join("chats"), h.path().join(".gemini/tmp/w/chats")).unwrap();
        // A linked marker is not read.
        chat(&h, "m", "session-m.jsonl", &header("aaaaaaaa-0000-4000-8000-00000000000a"), 1);
        std::os::unix::fs::symlink(&marker, h.path().join(".gemini/tmp/m/.project_root")).unwrap();
        project(&h, "ok", "/w");
        chat(&h, "ok", "session-ok.jsonl", &header(ID), 1);
        let name = std::ffi::OsStr::from_bytes(b"session-\xff.jsonl");
        // macOS file systems refuse names that are not UTF-8: there is nothing to skip there.
        let _ = std::fs::write(h.path().join(".gemini/tmp/ok/chats").join(name), header(ID));
        let slug = std::ffi::OsStr::from_bytes(b"\xfe");
        let raw = h.path().join(".gemini/tmp").join(slug);
        let slug_made = std::fs::create_dir_all(raw.join("chats")).is_ok();
        if slug_made {
            std::fs::write(raw.join(".project_root"), "/w").unwrap();
            std::fs::write(raw.join("chats/session-raw.jsonl"), header("bbbbbbbb-0000-4000-8000-00000000000b"))
                .unwrap();
        }
        project(&h, "shut", "/w");
        chat(&h, "shut", "session-s.jsonl", &header("cccccccc-0000-4000-8000-00000000000c"), 1);
        let shut = h.path().join(".gemini/tmp/shut/chats");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
        let ids: Vec<String> = found(&h, "/w").into_iter().map(|s| s.id).collect();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ids.contains(&ID.to_string()), "{ids:?}");
        assert!(
            !slug_made || ids.contains(&"bbbbbbbb-0000-4000-8000-00000000000b".to_string()),
            "the slug's name is never read"
        );
        assert!(!ids.iter().any(|i| i.starts_with("eeeeeeee")), "a linked chats folder is not followed: {ids:?}");
        assert!(!ids.iter().any(|i| i.starts_with("aaaaaaaa")), "a linked marker is not read: {ids:?}");
    }
}
