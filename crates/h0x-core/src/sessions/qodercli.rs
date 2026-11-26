//! qodercli 1.1.98 (read from its published bundle): `<config>/projects/<folder, every
//! character but ASCII letters and digits as '-'>/<session id>.jsonl`, as Claude Code keeps
//! it. The config is `$QODER_CONFIG_DIR`, else `$QODER_CLI_HOME/.qoder`, else `~/.qoder`.
//! Every line carries `cwd` and `sessionId`. Its titles are not read.

use super::*;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(env: Env) -> Option<PathBuf> {
    var(env, "QODER_CONFIG_DIR").or_else(|| {
        let home = var(env, "QODER_CLI_HOME").or_else(|| var(env, "HOME")).or_else(|| var(env, "USERPROFILE"))?;
        Some(home.join(".qoder"))
    })
}

pub(super) fn list(root: &Path, want: &str) -> Vec<SessionInfo> {
    claude::list_as("qodercli", root, want)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::Scratch;
    use super::*;

    fn session(h: &Scratch, dir: &str, id: &str, cwd: &str, ms: u64) {
        let line = serde_json::json!({"type": "user", "sessionId": id, "cwd": cwd});
        h.put(
            &format!("projects/{dir}/{id}.jsonl"),
            format!(
                "{line}
"
            )
            .as_bytes(),
            ms,
        );
    }

    #[test]
    fn root_from_config_dir_cli_home_or_home() {
        let env = |n: &str| (n == "QODER_CONFIG_DIR").then(|| "/q".to_string());
        assert_eq!(root(&env), Some(PathBuf::from("/q")));
        let env = |n: &str| match n {
            "QODER_CLI_HOME" => Some("/c".to_string()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/c/.qoder")));
        let env = |n: &str| (n == "HOME").then(|| "/h".to_string());
        assert_eq!(root(&env), Some(PathBuf::from("/h/.qoder")));
        assert_eq!(root(&|_| Some(String::new())), None);
    }

    #[test]
    fn lists_this_folders_sessions_only() {
        let h = Scratch::new();
        session(&h, "-w", "a1", "/w", 5);
        session(&h, "-w", "a2", "/w", 9);
        // Names collide (`/a-b`, `/a_b`): the line's own folder decides.
        session(&h, "-a-b", "b1", "/a_b", 7);
        session(&h, "-other", "c1", "/other", 8);
        let got = list(h.path(), "/w");
        let mut ids: Vec<(&str, &str)> = got.iter().map(|s| (s.agent.as_str(), s.id.as_str())).collect();
        ids.sort();
        assert_eq!(ids, [("qodercli", "a1"), ("qodercli", "a2")]);
        assert!(list(h.path(), "/a-b").is_empty());
        assert_eq!(list(h.path(), "/a_b").len(), 1);
        assert!(list(h.path(), "").is_empty());
        assert!(list(&h.path().join("missing"), "/w").is_empty());
    }

    #[test]
    fn empty_corrupt_and_linked_entries_are_skipped() {
        let h = Scratch::new();
        h.put("projects/-w/empty.jsonl", b"", 1);
        h.put(
            "projects/-w/corrupt.jsonl",
            b"{not json
",
            2,
        );
        session(&h, "-w", "ok", "/w", 3);
        #[cfg(unix)]
        {
            let outside = Scratch::new();
            session(&outside, "-w", "out", "/w", 4);
            std::os::unix::fs::symlink(
                outside.path().join("projects/-w/out.jsonl"),
                h.path().join("projects/-w/out.jsonl"),
            )
            .unwrap();
            std::os::unix::fs::symlink(outside.path().join("projects/-w"), h.path().join("projects/-link")).unwrap();
            let ids: Vec<String> = list(h.path(), "/w").into_iter().map(|s| s.id).collect();
            assert_eq!(ids, ["ok"], "links out of the store are skipped");
        }
        let ids: Vec<String> = list(h.path(), "/w").into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["ok"]);
    }

    fn ids(root: &Path, want: &str) -> Vec<String> {
        let mut ids: Vec<String> = list(root, want).into_iter().map(|s| s.id).collect();
        ids.sort();
        ids
    }

    #[test]
    fn qodercli_edge_three_folders_in_one_encoded_dir_each_get_only_their_own() {
        let h = Scratch::new();
        // `/a-b`, `/a_b`, `/a.b` and `/a b` all become `-a-b`.
        session(&h, "-a-b", "dash", "/a-b", 1);
        session(&h, "-a-b", "under", "/a_b", 2);
        session(&h, "-a-b", "dot", "/a.b", 3);
        session(&h, "-a-b", "space", "/a b", 4);
        for (want, id) in [("/a-b", "dash"), ("/a_b", "under"), ("/a.b", "dot"), ("/a b", "space")] {
            assert_eq!(ids(h.path(), want), [id], "{want}");
        }
        assert!(ids(h.path(), "/a/b").is_empty(), "`/a/b` encodes the same; no record says it");
    }

    #[test]
    fn qodercli_edge_the_first_cwd_in_the_record_decides() {
        let h = Scratch::new();
        // The first line has no cwd; the second names `/a_b`; a later one `/a-b`.
        let text = [
            serde_json::json!({"type": "system", "sessionId": "x"}),
            serde_json::json!({"type": "user", "sessionId": "x", "cwd": "/a_b"}),
            serde_json::json!({"type": "user", "sessionId": "x", "cwd": "/a-b"}),
        ]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<String>();
        h.put("projects/-a-b/x.jsonl", text.as_bytes(), 1);
        assert_eq!(ids(h.path(), "/a_b"), ["x"]);
        assert!(ids(h.path(), "/a-b").is_empty());
    }

    #[test]
    fn qodercli_edge_titles_are_never_read_and_side_chains_are_skipped() {
        let h = Scratch::new();
        let lines = [
            serde_json::json!({"type": "user", "sessionId": "t", "cwd": "/w"}),
            serde_json::json!({"type": "custom-title", "customTitle": "Named", "sessionId": "t"}),
            serde_json::json!({"type": "summary", "summary": "Summed", "sessionId": "t"}),
            serde_json::json!({"type": "last-prompt", "lastPrompt": "hello", "sessionId": "t"}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        h.put("projects/-w/t.jsonl", text.as_bytes(), 1);
        session(&h, "-w", "agent-side", "/w", 2);
        let found = list(h.path(), "/w");
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].agent.as_str(), found[0].id.as_str(), found[0].title.as_deref()), ("qodercli", "t", None));
    }

    #[test]
    fn qodercli_edge_a_windows_folder_matches_its_encoded_dir() {
        let h = Scratch::new();
        session(&h, "C--Work-My-App", "win", r"C:\Work\My App", 1);
        assert_eq!(ids(h.path(), &normal("c:/work/my app/")), ["win"]);
        assert_eq!(ids(h.path(), &normal(r"C:\WORK\MY APP")), ["win"]);
        assert!(ids(h.path(), &normal(r"C:\Work\My_App")).is_empty(), "same dir, another folder");
    }

    #[test]
    fn qodercli_edge_root_skips_empty_variables_and_takes_userprofile() {
        let env = |n: &str| match n {
            "QODER_CONFIG_DIR" => Some(String::new()),
            "QODER_CLI_HOME" => Some(String::new()),
            "HOME" => Some("/h".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/h/.qoder")));
        let env = |n: &str| match n {
            "QODER_CONFIG_DIR" => Some("/q".to_string()),
            "QODER_CLI_HOME" => Some("/c".to_string()),
            _ => None,
        };
        assert_eq!(root(&env), Some(PathBuf::from("/q")), "the config dir wins");
        let profile = |n: &str| (n == "USERPROFILE").then(|| r"C:\Users\u".to_string());
        assert_eq!(root(&profile), Some(PathBuf::from(r"C:\Users\u").join(".qoder")));
        assert_eq!(root(&|_| None), None);
    }
}
