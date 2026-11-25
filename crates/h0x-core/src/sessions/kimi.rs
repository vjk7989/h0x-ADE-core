//! Kimi: `<root>/sessions/<md5 of the folder>/<uuid>/context.jsonl`, the root
//! being `$KIMI_SHARE_DIR`, else `~/.kimi`. The folder is only in
//! `<root>/kimi.json`: `work_dirs: [{path, kaos}]`; for a `kaos` other than
//! `local` the directory is `<kaos>_<md5>`. The session id is the uuid directory.

use super::*;
use md5::{Digest, Md5};

/// `kimi.json` is a small file.
const MAX: u64 = 1024 * 1024;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    or_home(env, "KIMI_SHARE_DIR", &[".kimi"])
}

pub(super) fn list(root: &Path, want: &str) -> Vec<SessionInfo> {
    let Some(work_dirs) = work_dirs(root) else { return Vec::new() };
    let mut names: Vec<String> = Vec::new();
    for (path, kaos) in work_dirs {
        if normal(&path) != want {
            continue;
        }
        let sum: String = Md5::digest(path.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        let name = match kaos.as_str() {
            "" | "local" => sum,
            // A name that could lead out of the store is not a kaos.
            k if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => format!("{k}_{sum}"),
            _ => continue,
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let store = root.join("sessions");
    names
        .into_iter()
        .map(|name| store.join(name))
        .filter(|dir| real_dir(dir))
        .flat_map(|dir| dirs(&dir))
        .filter_map(|dir| read(&dir))
        .collect()
}

/// Whether a path is a real folder, not a link out of the store.
fn real_dir(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
}

/// The `(path, kaos)` of each work dir in `kimi.json`; none when it is missing or broken.
fn work_dirs(root: &Path) -> Option<Vec<(String, String)>> {
    let path = root.join("kimi.json");
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let (mut file, _) = open(&path)?;
    let json: serde_json::Value = match serde_json::from_slice(&head(&path, &mut file, MAX)?) {
        Ok(json) => json,
        Err(e) => {
            skipped(&path, e);
            return None;
        }
    };
    let list = json["work_dirs"].as_array()?;
    Some(
        list.iter()
            .filter_map(|w| Some((w["path"].as_str()?.to_string(), w["kaos"].as_str().unwrap_or("local").to_string())))
            .collect(),
    )
}

/// Whether a name is a uuid: 8-4-4-4-12 hex digits.
fn uuid(name: &str) -> bool {
    name.len() == 36
        && name.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn read(dir: &Path) -> Option<SessionInfo> {
    let id = dir.file_name()?.to_str().filter(|n| uuid(n))?;
    let context = dir.join("context.jsonl");
    if !std::fs::symlink_metadata(&context).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let (_, meta) = open(&context)?;
    Some(SessionInfo {
        agent: "kimi".into(),
        id: id.into(),
        title: None,
        last_used: modified_ms(&meta),
        path: context.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixture::Scratch;
    use super::*;

    const A: &str = "11111111-2222-3333-4444-555555555555";
    const B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    /// md5("/home/u/p"), from `md5sum`.
    const SUM: &str = "3d999e8407eafb4c2d9dd5641fe8303f";

    fn store(json: &str) -> Scratch {
        let s = Scratch::new();
        s.put("kimi.json", json.as_bytes(), 1);
        s
    }

    #[test]
    fn root_from_env_or_home() {
        let env = |n: &str| (n == "KIMI_SHARE_DIR").then(|| "/k".to_string());
        assert_eq!(root(&env), Some(PathBuf::from("/k")));
        let env = |n: &str| (n == "HOME").then(|| "/h".to_string());
        assert_eq!(root(&env), Some(PathBuf::from("/h").join(".kimi")));
        assert_eq!(root(&|_| None), None);
    }

    #[test]
    fn missing_root_and_empty_store_list_nothing() {
        assert!(list(Path::new("/no/such/kimi"), "/home/u/p").is_empty());
        let s = store(r#"{"work_dirs":[]}"#);
        assert!(list(s.path(), "/home/u/p").is_empty());
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"}]}"#);
        assert!(list(s.path(), "/home/u/p").is_empty());
    }

    #[test]
    fn corrupt_kimi_json_lists_nothing() {
        for bad in ["not json", "", r#"{"work_dirs":5}"#, "[]"] {
            let s = store(bad);
            s.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 5);
            assert!(list(s.path(), "/home/u/p").is_empty(), "{bad}");
        }
    }

    #[test]
    fn finds_by_md5_dir_with_newest_mtime() {
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"},{"path":"/other","kaos":"local"}]}"#);
        s.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 5_000);
        s.put(&format!("sessions/{SUM}/{B}/context.jsonl"), b"{}\n", 9_000);
        let mut got = list(s.path(), "/home/u/p");
        got.sort_by_key(|g| std::cmp::Reverse(g.last_used));
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].id.as_str(), got[0].last_used, got[0].agent.as_str()), (B, 9_000, "kimi"));
        assert_eq!((got[1].id.as_str(), got[1].last_used), (A, 5_000));
        // Another folder is left out.
        assert!(list(s.path(), "/else").is_empty());
    }

    #[test]
    fn non_local_kaos_has_a_prefix() {
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"ssh"}]}"#);
        s.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        assert!(list(s.path(), "/home/u/p").is_empty());
        s.put(&format!("sessions/ssh_{SUM}/{B}/context.jsonl"), b"{}\n", 2);
        let got = list(s.path(), "/home/u/p");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, B);
    }

    #[test]
    fn kaos_that_leaves_the_store_is_ignored() {
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"../x"}]}"#);
        s.put(&format!("sessions/../x_{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        assert!(list(s.path(), "/home/u/p").is_empty());
    }

    #[test]
    fn non_uuid_dir_and_missing_context_are_skipped() {
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"}]}"#);
        s.put(&format!("sessions/{SUM}/not-a-uuid/context.jsonl"), b"{}\n", 1);
        s.put(&format!("sessions/{SUM}/{A}/other.jsonl"), b"{}\n", 1);
        assert!(list(s.path(), "/home/u/p").is_empty());
    }

    #[test]
    fn windows_drive_case_matches_through_normal() {
        // The directory is the md5 of the path as kimi.json holds it.
        let sum: String = Md5::digest(br"C:\Users\u\p").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sum, "60fb4c5c49a2c67f8476e16ca840b6bd");
        let s = store(r#"{"work_dirs":[{"path":"C:\\Users\\u\\p","kaos":"local"}]}"#);
        s.put(&format!("sessions/{sum}/{A}/context.jsonl"), b"{}\n", 7);
        let got = list(s.path(), &normal("c:/users/U/p/"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, A);
    }

    #[test]
    fn md5_known_vector() {
        let sum: String = Md5::digest(b"/home/u/p").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sum, SUM);
    }

    #[test]
    fn finder_edge_unicode_work_dir_hashes_its_utf8_bytes() {
        // md5 of the UTF-8 bytes of "/home/ü/项目", from `printf %s ... | md5sum`.
        let sum = "1888a80e992c8e665ffc097a9992bed4";
        let s = store(r#"{"work_dirs":[{"path":"/home/ü/项目","kaos":"local"}]}"#);
        s.put(&format!("sessions/{sum}/{A}/context.jsonl"), b"{}\n", 42);
        let got = list(s.path(), "/home/ü/项目");
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].id.as_str(), got[0].last_used), (A, 42));
        assert!(list(s.path(), "/home/u/项目").is_empty());
    }

    #[test]
    fn finder_edge_duplicate_work_dirs_list_each_session_once() {
        let s = store(
            r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"},{"path":"/home/u/p"},{"path":"/home/u/p","kaos":"local"}]}"#,
        );
        s.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        let ids: Vec<String> = list(s.path(), "/home/u/p").into_iter().map(|g| g.id).collect();
        assert_eq!(ids, [A], "one session, however often kimi.json names its folder");
    }

    #[test]
    fn finder_edge_trailing_slash_work_dir_uses_its_own_hash() {
        // md5("/home/u/p/"): kimi hashes the path as it wrote it.
        let slash = "b543569c9dd1562718a111626a5cc7d2";
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p/","kaos":"local"}]}"#);
        s.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        assert!(list(s.path(), "/home/u/p").is_empty(), "the other spelling's folder is not this one's");
        s.put(&format!("sessions/{slash}/{B}/context.jsonl"), b"{}\n", 2);
        let ids: Vec<String> = list(s.path(), &normal("/home/u/x/../p")).into_iter().map(|g| g.id).collect();
        assert_eq!(ids, [B]);
    }

    #[test]
    fn finder_edge_root_and_store_parts_that_are_files() {
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"}]}"#);
        assert!(list(&s.path().join("kimi.json"), "/home/u/p").is_empty(), "the root is a file");
        s.put(&format!("sessions/{SUM}"), b"a file", 1);
        assert!(list(s.path(), "/home/u/p").is_empty());
        let t = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"}]}"#);
        t.put(&format!("sessions/{SUM}/{A}"), b"a file, not a session folder", 1);
        std::fs::create_dir_all(t.path().join(format!("sessions/{SUM}/{B}/context.jsonl"))).unwrap();
        assert!(list(t.path(), "/home/u/p").is_empty(), "context.jsonl must be a file");
        // kimi.json as a folder.
        let u = Scratch::new();
        std::fs::create_dir_all(u.path().join("kimi.json")).unwrap();
        assert!(list(u.path(), "/home/u/p").is_empty());
    }

    #[test]
    fn finder_edge_bad_work_dir_entries_and_bom() {
        let s = store(r#"{"work_dirs":[{"path":5},{"kaos":"local"},null,"x",{"path":"/home/u/p","kaos":7}]}"#);
        s.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        // A non-text kaos counts as local; the others are left out.
        assert_eq!(list(s.path(), "/home/u/p").len(), 1);
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend(br#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"}]}"#);
        let t = Scratch::new();
        t.put("kimi.json", &bom, 1);
        t.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        assert!(list(t.path(), "/home/u/p").iter().all(|g| g.id == A), "never a panic");
        let crlf = store("{\"work_dirs\":[\r\n{\"path\":\"/home/u/p\",\"kaos\":\"local\"}\r\n]}\r\n");
        crlf.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        assert_eq!(list(crlf.path(), "/home/u/p").len(), 1);
    }

    #[test]
    fn finder_edge_unc_and_verbatim_work_dirs() {
        let path = r"\\Srv\Share\P";
        let sum: String = Md5::digest(path.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        let s = store(r#"{"work_dirs":[{"path":"\\\\Srv\\Share\\P","kaos":"local"}]}"#);
        s.put(&format!("sessions/{sum}/{A}/context.jsonl"), b"{}\n", 1);
        assert_eq!(list(s.path(), &normal(r"\\?\UNC\srv\share\p\")).len(), 1);
        assert!(list(s.path(), &normal(r"\\srv\share\q")).is_empty());
    }

    #[test]
    fn finder_edge_empty_env_falls_back_to_home() {
        let env = |n: &str| match n {
            "KIMI_SHARE_DIR" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/h").join(".kimi")));
    }

    #[cfg(unix)]
    #[test]
    fn finder_edge_linked_session_folders_are_not_followed() {
        let s = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"}]}"#);
        let outside = Scratch::new();
        outside.put(&format!("{A}/context.jsonl"), b"{}\n", 1);
        outside.put(&format!("md5/{B}/context.jsonl"), b"{}\n", 1);
        std::fs::create_dir_all(s.path().join("sessions")).unwrap();
        // A link at the folder's own hash leads out of the store.
        std::os::unix::fs::symlink(outside.path().join("md5"), s.path().join(format!("sessions/{SUM}"))).unwrap();
        assert!(list(s.path(), "/home/u/p").is_empty(), "a linked <md5> folder is not followed");
        let t = store(r#"{"work_dirs":[{"path":"/home/u/p","kaos":"local"}]}"#);
        std::fs::create_dir_all(t.path().join(format!("sessions/{SUM}"))).unwrap();
        std::os::unix::fs::symlink(outside.path().join(A), t.path().join(format!("sessions/{SUM}/{A}"))).unwrap();
        assert!(list(t.path(), "/home/u/p").is_empty(), "a linked <uuid> folder is not followed");
        let u = Scratch::new();
        u.put(&format!("sessions/{SUM}/{A}/context.jsonl"), b"{}\n", 1);
        std::os::unix::fs::symlink(t.path().join("kimi.json"), u.path().join("kimi.json")).unwrap();
        assert!(list(u.path(), "/home/u/p").is_empty(), "a linked kimi.json is not read");
    }
}
