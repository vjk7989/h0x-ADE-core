//! GitHub Copilot CLI 1.0.116 (read from its published package, `cli-main.js` and the
//! `WorkspaceSummary` type of its SDK): `<home>/session-state/<session id>/workspace.yaml`, the
//! home being `$COPILOT_HOME`, else `~/.copilot`. The file holds `id` (the folder's name, a
//! UUID), `cwd` (where the session started), `name` and timestamps, one `key: value` per line.
//! `events.jsonl` beside it is the conversation, and is never read.

use super::*;

/// A workspace file is a few lines.
const MAX: u64 = 16 * 1024;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    or_home(env, "COPILOT_HOME", &[".copilot"])
}

pub(super) fn list(home: &Path, want: &str) -> Vec<SessionInfo> {
    dirs(&home.join("session-state")).filter_map(|dir| read(&dir, want)).collect()
}

fn read(dir: &Path, want: &str) -> Option<SessionInfo> {
    let id = dir.file_name()?.to_str()?;
    if !resume::is_uuid(id) {
        return None;
    }
    let path = dir.join("workspace.yaml");
    let (mut file, meta) = open(&path)?;
    if !meta.is_file() {
        return None;
    }
    // Lossy: a multi-byte character cut by the size limit must not drop the keys before it.
    let text = String::from_utf8_lossy(&head(&path, &mut file, MAX)?).into_owned();
    let value = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')).and_then(scalar);
    if value("id").as_deref() != Some(id) || normal(&value("cwd")?) != want {
        return None;
    }
    // The newest of the two files a running session touches.
    let events = std::fs::metadata(dir.join("events.jsonl")).ok().filter(|m| m.is_file()).map(|m| modified_ms(&m));
    Some(SessionInfo {
        agent: "copilot".into(),
        id: id.into(),
        title: value("name").as_deref().and_then(title),
        last_used: modified_ms(&meta).max(events.unwrap_or(0)),
        path: path.display().to_string(),
    })
}

/// A YAML scalar written on one line: plain, 'single' or "double" quoted.
fn scalar(text: &str) -> Option<String> {
    let text = text.trim();
    let quoted = |q: char| text.len() >= 2 && text.starts_with(q) && text.ends_with(q);
    let value = if quoted('\'') {
        text[1..text.len() - 1].replace("''", "'")
    } else if quoted('"') {
        serde_json::from_str::<String>(text).ok()?
    } else {
        text.to_string()
    };
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::Scratch;
    use super::*;

    const A: &str = "123e4567-e89b-42d3-a456-426614174000";
    const B: &str = "223e4567-e89b-42d3-a456-426614174000";

    fn put(h: &Scratch, id: &str, yaml: &str, ms: u64) {
        h.put(&format!("session-state/{id}/workspace.yaml"), yaml.as_bytes(), ms);
    }

    #[test]
    fn root_from_copilot_home_or_home() {
        let own = |n: &str| (n == "COPILOT_HOME").then(|| "/c".to_string());
        assert_eq!(root(&own), Some(PathBuf::from("/c")));
        let home = |n: &str| (n == "HOME").then(|| "/h".to_string());
        assert_eq!(root(&home), Some(PathBuf::from("/h/.copilot")));
        assert_eq!(root(&|_| None), None);
    }

    #[test]
    fn lists_this_folders_sessions_with_their_names() {
        let h = Scratch::new();
        put(&h, A, &format!("id: {A}\ncwd: /w\nname: Fix the build\ncreated_at: 2026-11-01T10:00:00.000Z\n"), 5);
        put(&h, B, &format!("id: {B}\ncwd: /elsewhere\n"), 9);
        h.put(&format!("session-state/{A}/events.jsonl"), b"{}\n", 40);
        let found = list(h.path(), "/w");
        let got: Vec<_> =
            found.iter().map(|s| (s.agent.as_str(), s.id.as_str(), s.title.as_deref(), s.last_used)).collect();
        assert_eq!(got, [("copilot", A, Some("Fix the build"), 40)]);
        assert!(list(h.path(), "").is_empty());
    }

    #[test]
    fn quoted_values_and_windows_folders_match() {
        let h = Scratch::new();
        put(&h, A, &format!("id: \"{A}\"\ncwd: 'C:\\Work\\App'\nname: \"it\\\"s\"\n"), 1);
        let found = list(h.path(), &normal("c:/work/app/"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title.as_deref(), Some("it\"s"));
        put(&h, B, &format!("id: {B}\ncwd: \"C:\\\\Work\\\\App\"\n"), 1);
        assert_eq!(list(h.path(), &normal("c:/work/app")).len(), 2);
    }

    #[test]
    fn empty_corrupt_mismatched_and_stray_entries_are_skipped() {
        let h = Scratch::new();
        put(&h, A, "", 1);
        put(&h, B, "\u{0}\u{1}not yaml", 1);
        let c = "323e4567-e89b-42d3-a456-426614174000";
        put(&h, c, &format!("id: {A}\ncwd: /w\n"), 1);
        put(&h, "not-a-uuid", "id: not-a-uuid\ncwd: /w\n", 1);
        let d = "423e4567-e89b-42d3-a456-426614174000";
        put(&h, d, &format!("id: {d}\ncwd:\n"), 1);
        h.put("session-state/loose-file", b"x", 1);
        std::fs::create_dir_all(h.path().join("session-state/523e4567-e89b-42d3-a456-426614174000")).unwrap();
        assert!(list(h.path(), "/w").is_empty());
        assert!(list(&h.path().join("missing"), "/w").is_empty());
        assert!(!h.path().join("missing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn linked_sessions_are_not_read() {
        let outside = Scratch::new();
        put(&outside, A, &format!("id: {A}\ncwd: /w\n"), 1);
        let h = Scratch::new();
        std::fs::create_dir_all(h.path().join("session-state")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("session-state").join(A),
            h.path().join("session-state").join(A),
        )
        .unwrap();
        assert!(list(h.path(), "/w").is_empty());
    }

    /// A distinct lower-case UUID per case.
    fn uuid(n: u8) -> String {
        format!("{n:02x}3e4567-e89b-42d3-a456-426614174000")
    }

    #[test]
    fn copilot_edge_malformed_workspace_files_are_skipped_without_a_panic() {
        let h = Scratch::new();
        let cases: [fn(&str) -> String; 5] = [
            // An unterminated quote: the id is not the folder's name.
            |id: &str| format!("id: \"{id}\ncwd: /w\n"),
            // A flow mapping on one line: no `id:` line at all.
            |id: &str| format!("{{id: {id}, cwd: /w}}\n"),
            // The keys only nested under another key.
            |id: &str| format!("session:\n  id: {id}\n  cwd: /w\n"),
            // A double-quoted value YAML allows but JSON does not.
            |id: &str| format!("id: {id}\ncwd: \"\\x2Fw\"\n"),
            // A key with no colon, and a lone colon.
            |id: &str| format!("id {id}\n:\ncwd /w\n"),
        ];
        for (n, yaml) in cases.iter().enumerate() {
            let id = uuid(n as u8);
            put(&h, &id, &yaml(&id), 1);
        }
        let bad = uuid(99);
        h.put(&format!("session-state/{bad}/workspace.yaml"), &[b'i', b'd', b':', b' ', 0xff, 0xfe, b'\n'], 1);
        assert!(list(h.path(), "/w").is_empty());
        assert!(list(h.path(), "/w/").is_empty());
    }

    #[test]
    fn copilot_edge_a_missing_id_or_cwd_key_lists_nothing() {
        let h = Scratch::new();
        let (a, b, c, d) = (uuid(1), uuid(2), uuid(3), uuid(4));
        put(&h, &a, "cwd: /w\nname: no id\n", 1);
        put(&h, &b, &format!("id: {b}\nname: no cwd\n"), 1);
        put(&h, &c, "id: ''\ncwd: /w\n", 1);
        put(&h, &d, &format!("id: {d}\ncwd: ''\n"), 1);
        assert!(list(h.path(), "/w").is_empty());
        // The id and cwd in any order, with CRLF line ends (Windows), still list.
        let e = uuid(5);
        put(&h, &e, &format!("name: ok\r\ncwd: /w\r\nid: {e}\r\n"), 2);
        let found = list(h.path(), "/w");
        assert_eq!(found.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [e.as_str()]);
        assert_eq!(found[0].title.as_deref(), Some("ok"));
    }

    #[test]
    fn copilot_edge_folders_that_are_not_a_uuid_are_never_read() {
        let h = Scratch::new();
        let good = uuid(7);
        let names = [
            format!("{good}x"),
            good[..35].to_string(),
            format!("{{{good}}}"),
            good.replace('-', ""),
            "zz3e4567-e89b-42d3-a456-426614174000".to_string(),
            good.replace('-', "_"),
            format!(" {good}"),
        ];
        for name in &names {
            put(&h, name, &format!("id: {name}\ncwd: /w\n"), 1);
        }
        assert!(list(h.path(), "/w").is_empty());
        // The yaml's id must be the folder's name exactly, case included.
        put(&h, &good, &format!("id: {}\ncwd: /w\n", good.to_uppercase()), 1);
        assert!(list(h.path(), "/w").is_empty());
    }

    #[test]
    fn copilot_edge_workspace_yaml_that_is_a_folder_or_events_that_are_a_folder() {
        let h = Scratch::new();
        let (a, b) = (uuid(8), uuid(9));
        std::fs::create_dir_all(h.path().join(format!("session-state/{a}/workspace.yaml"))).unwrap();
        put(&h, &b, &format!("id: {b}\ncwd: /w\n"), 7);
        std::fs::create_dir_all(h.path().join(format!("session-state/{b}/events.jsonl"))).unwrap();
        let found = list(h.path(), "/w");
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].id.as_str(), found[0].last_used),
            (b.as_str(), 7),
            "a folder named events.jsonl is not used"
        );
    }

    #[test]
    fn copilot_edge_a_huge_workspace_file_lists_from_its_head_only() {
        let h = Scratch::new();
        let (a, b, c) = (uuid(10), uuid(11), uuid(12));
        // id and cwd at the top, then far more than the cap of ASCII.
        let mut big = format!("id: {a}\ncwd: /w\nsummary: ");
        big.push_str(&"x".repeat(4 * MAX as usize));
        put(&h, &a, &big, 1);
        // The cwd only after the cap: never read.
        let mut late = format!("id: {b}\nsummary: ");
        late.push_str(&"x".repeat(MAX as usize));
        late.push_str("\ncwd: /w\n");
        put(&h, &b, &late, 1);
        let ids: Vec<String> = list(h.path(), "/w").into_iter().map(|s| s.id).collect();
        assert_eq!(ids, std::slice::from_ref(&a));
        // A multi-byte character cut at the cap must not hide a session whose id and cwd are on top.
        let mut wide = format!("id: {c}\ncwd: /w\nsummary: ");
        if (MAX as usize - wide.len()).is_multiple_of(2) {
            wide.push('x');
        }
        wide.push_str(&"\u{e9}".repeat(MAX as usize));
        put(&h, &c, &wide, 2);
        let mut ids: Vec<String> = list(h.path(), "/w").into_iter().map(|s| s.id).collect();
        ids.sort();
        assert_eq!(ids, [a, c], "a UTF-8 character split by the 16 KB cut drops the whole session");
    }

    #[test]
    fn copilot_edge_root_skips_an_empty_copilot_home_and_takes_userprofile() {
        let env = |n: &str| match n {
            "COPILOT_HOME" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/h/.copilot")));
        let profile = |n: &str| (n == "USERPROFILE").then(|| r"C:\Users\u".to_string());
        assert_eq!(root(&profile), Some(PathBuf::from(r"C:\Users\u").join(".copilot")));
    }
}
