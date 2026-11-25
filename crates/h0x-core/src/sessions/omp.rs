//! omp (oh-my-pi): pi's session layout under `~/.omp/agent` (default profile).

use super::*;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    or_home(env, "PI_CODING_AGENT_DIR", &[".omp", "agent"])
}

pub(super) fn list(root: &Path, want: &str) -> Vec<SessionInfo> {
    super::pi::header_list(root, "omp", want)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::Scratch;
    use super::*;

    fn header(id: &str, cwd: &str) -> Vec<u8> {
        format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":{}}}\n", serde_json::json!(cwd)).into_bytes()
    }

    #[test]
    fn root_follows_env_then_home() {
        let set = |k: &str| (k == "PI_CODING_AGENT_DIR").then(|| "/p".to_string());
        assert_eq!(root(&set), Some(PathBuf::from("/p")));
        let home = |k: &str| (k == "HOME").then(|| "/h".to_string());
        assert_eq!(root(&home), Some(PathBuf::from("/h").join(".omp").join("agent")));
    }

    #[test]
    fn empty_and_missing_store() {
        let s = Scratch::new();
        assert!(list(s.path(), "/w").is_empty());
        assert!(list(&s.path().join("none"), "/w").is_empty());
    }

    #[test]
    fn bad_headers_and_other_folders_are_left_out() {
        let s = Scratch::new();
        s.put("sessions/--w--/a.jsonl", b"{\"type\":\"session\",\"id\":", 1);
        s.put("sessions/--w--/b.jsonl", b"{\"type\":\"message\",\"id\":\"b\",\"cwd\":\"/w\"}\n", 1);
        s.put("sessions/--w--/c.jsonl", &header("c", "/other"), 1);
        let big =
            format!("{{\"type\":\"session\",\"id\":\"d\",\"cwd\":\"/w\",\"pad\":\"{}\"}}\n", "x".repeat(70 * 1024));
        s.put("sessions/--w--/d.jsonl", big.as_bytes(), 1);
        assert!(list(s.path(), "/w").is_empty());
    }

    #[test]
    fn matches_drive_case_and_orders_by_mtime() {
        let s = Scratch::new();
        s.put("sessions/--C--P--/a.jsonl", &header("a", r"C:\Users\X\P"), 10);
        s.put("sessions/--c--p--/b.jsonl", &header("b", r"c:/users/x/p"), 30);
        let mut found = list(s.path(), &normal(r"c:\users\x\p"));
        found.sort_by_key(|f| std::cmp::Reverse(f.last_used));
        let ids: Vec<(&str, &str, u64)> =
            found.iter().map(|f| (f.agent.as_str(), f.id.as_str(), f.last_used)).collect();
        assert_eq!(ids, [("omp", "b", 30), ("omp", "a", 10)]);
    }

    #[test]
    fn finder_edge_root_is_a_file_and_empty_env_falls_back() {
        let s = Scratch::new();
        let file = s.put("f", &header("a", "/w"), 1);
        assert!(list(&file, "/w").is_empty());
        let env = |k: &str| match k {
            "PI_CODING_AGENT_DIR" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/h").join(".omp").join("agent")));
    }

    #[test]
    fn finder_edge_crlf_trailing_slash_and_unc() {
        let s = Scratch::new();
        s.put("sessions/--a--/a.jsonl", b"{\"type\":\"session\",\"id\":\"a\",\"cwd\":\"/w/x/../\"}\r\n", 4);
        s.put("sessions/--u--/u.jsonl", &header("u", r"\\?\UNC\Srv\Share\P"), 1);
        let found = list(s.path(), "/w");
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].agent.as_str(), found[0].id.as_str(), found[0].last_used), ("omp", "a", 4));
        assert_eq!(list(s.path(), &normal(r"\\srv\share\p\")).len(), 1);
    }
}
