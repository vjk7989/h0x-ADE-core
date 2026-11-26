use super::*;

// Captured with `h0x-wt <verb> --format json` (see docs/adr in the release notes), paths rewritten.
const LIST_MAIN_ONLY: &str = include_str!("../../tests/fixtures/h0x-wt/list-main-only.json");
const LIST_WORKTREES: &str = include_str!("../../tests/fixtures/h0x-wt/list-worktrees.json");
const SWITCH_CREATED: &str = include_str!("../../tests/fixtures/h0x-wt/switch-created.json");
const SWITCH_EXISTING: &str = include_str!("../../tests/fixtures/h0x-wt/switch-existing.json");
const REMOVE_DELETED: &str = include_str!("../../tests/fixtures/h0x-wt/remove-deleted.json");
const REMOVE_RETAINED: &str = include_str!("../../tests/fixtures/h0x-wt/remove-retained.json");
const ERR_NOT_GIT: &str = include_str!("../../tests/fixtures/h0x-wt/err-not-git.txt");
const ERR_DIRTY: &str = include_str!("../../tests/fixtures/h0x-wt/err-dirty.txt");
const ERR_MAIN: &str = include_str!("../../tests/fixtures/h0x-wt/err-main.txt");
const ERR_INVALID: &str = include_str!("../../tests/fixtures/h0x-wt/err-invalid-branch.txt");
const ERR_EXISTS: &str = include_str!("../../tests/fixtures/h0x-wt/err-exists.txt");
const ERR_CONFLICT: &str = include_str!("../../tests/fixtures/h0x-wt/err-conflict.txt");

fn listed(json: &str) -> Listing {
    listing(&parse_list(json).unwrap())
}

fn failure(stderr: &str) -> Error {
    failed(&Out { ok: false, stdout: String::new(), stderr: stderr.trim().to_string() })
}

#[test]
fn a_list_with_only_the_main_checkout() {
    let l = listed(LIST_MAIN_ONLY);
    assert_eq!(l.default_branch.as_deref(), Some("main"));
    assert_eq!(l.worktrees.len(), 1);
    let main = &l.worktrees[0];
    // A Windows drive path with a space and a non-ASCII letter, as git writes it.
    assert_eq!(main.path, "C:/Users/dev/my repo é");
    assert_eq!(main.branch.as_deref(), Some("main"));
    assert!(main.main && !main.dirty);
    assert_eq!((main.ahead, main.behind, main.integrated), (0, 0, None));
    assert!(main.at > 1_700_000_000);
}

#[test]
fn an_empty_list_and_a_repository_without_a_default_branch() {
    let l = listed(r#"{"schema":2,"repo":{"default_branch":null},"collected":{},"items":[]}"#);
    assert_eq!(l, Listing { default_branch: None, worktrees: vec![] });
    // Branch-only rows (no worktree) are not worktrees.
    let l = listed(r#"{"repo":{},"items":[{"branch":"topic","head":{"committed_at":"2026-01-01T00:00:00Z"}}]}"#);
    assert!(l.worktrees.is_empty());
}

#[test]
fn a_list_maps_status_integration_detached_and_order() {
    let l = listed(LIST_WORKTREES);
    let rows: Vec<_> =
        l.worktrees.iter().map(|w| (w.branch.as_deref(), w.main, w.dirty, w.ahead, w.behind, w.integrated)).collect();
    assert_eq!(
        rows,
        vec![
            (Some("main"), true, false, 0, 0, None),
            // Detached HEAD: no branch.
            (None, false, false, 0, 0, Some(Integrated::SameCommit)),
            (Some("feat/ahead"), false, false, 2, 1, None),
            // Modified and untracked files make it dirty; non-ASCII branch.
            (Some("fix/ünï-dirty"), false, true, 0, 1, Some(Integrated::Ancestor)),
            (Some("inmain"), false, false, 0, 1, Some(Integrated::Ancestor)),
        ]
    );
    assert_eq!(l.worktrees[1].path, "C:/Users/dev/.h0x/worktrees/my repo é/detached");
    // The main checkout is first, then the most recent commit first.
    assert!(l.worktrees.windows(2).skip(1).all(|p| p[0].at >= p[1].at));
}

#[test]
fn every_integration_reason_parses() {
    for (name, reason) in [
        ("same_commit", Integrated::SameCommit),
        ("ancestor", Integrated::Ancestor),
        ("no_added_changes", Integrated::NoAddedChanges),
        ("trees_match", Integrated::TreesMatch),
        ("merge_adds_nothing", Integrated::MergeAddsNothing),
        ("patch_id_match", Integrated::PatchIdMatch),
    ] {
        let json = format!(
            r#"{{"repo":{{}},"items":[{{"branch":"b","worktree":{{"path":"/r/b"}},"default_branch":{{"ahead":1,"behind":0,"integration":{{"reason":"{name}"}}}}}}]}}"#
        );
        assert_eq!(listed(&json).worktrees[0].integrated, Some(reason), "{name}");
    }
}

#[test]
fn a_list_that_is_not_the_expected_json_is_a_failure_not_a_panic() {
    for bad in ["", "not json", "[]", r#"{"repo":{},"items":[{"branch":1}]}"#] {
        assert!(matches!(parse_list(bad), Err(Error::Failed(m)) if m.contains("unexpected list output")), "{bad:?}");
    }
}

#[test]
fn times_are_unix_seconds() {
    assert_eq!(unix_time("1970-01-01T00:00:00Z"), 0);
    assert_eq!(unix_time("2000-01-01T00:00:00Z"), 946_684_800);
    assert_eq!(unix_time("2024-02-29T12:30:15Z"), 1_709_209_815);
    assert_eq!(unix_time("2024-02-29T12:30:15.987Z"), 1_709_209_815);
    assert_eq!(unix_time("2024-02-29T13:30:15+01:00"), 1_709_209_815);
    assert_eq!(unix_time("2024-02-29T07:30:15-05:00"), 1_709_209_815);
    assert_eq!(unix_time("1970-01-01"), 0);
}

#[test]
fn paths_compare_across_slash_styles() {
    assert!(same_path(r"C:\Users\dev\my repo é\", "C:/Users/dev/my repo é"));
    assert!(same_path(r"\\?\C:\work\x", "C:/work/x"));
    assert!(!same_path("/a/b", "/a/c"));
    assert_eq!(slashes(r"C:\Users\dev"), "C:/Users/dev");
}

#[test]
fn a_created_worktree_maps_with_forward_slashes() {
    assert_eq!(
        parse_switch(SWITCH_CREATED).unwrap(),
        Created {
            path: "C:/Users/dev/.h0x/worktrees/my repo é/feat-ahead".into(),
            branch: "feat/ahead".into(),
            new_branch: true
        }
    );
    let existing = r#"{"action":"created","branch":"old","path":"/h/old","created_branch":false}"#;
    assert!(!parse_switch(existing).unwrap().new_branch);
    // The branch already has a worktree: that one is named, not handed out again.
    assert_eq!(
        parse_switch(SWITCH_EXISTING),
        Err(Error::CheckedOut("C:/Users/dev/.h0x/worktrees/my repo é/feat-ahead".into()))
    );
    assert!(matches!(parse_switch("{}"), Err(Error::Failed(_))));
}

#[test]
fn a_removal_maps_the_branch_outcome() {
    let deleted = parse_removal(REMOVE_DELETED).unwrap();
    assert_eq!((deleted.branch.as_deref(), deleted.branch_outcome.as_deref()), (Some("inmain"), Some("deleted")));
    let kept = parse_removal(REMOVE_RETAINED).unwrap();
    assert_eq!(kept.branch_outcome.as_deref(), Some("retained_unmerged"));
    assert!(matches!(parse_removal("[]"), Err(Error::Failed(_))));
    assert!(matches!(parse_removal("{}"), Err(Error::Failed(_))));
}

#[test]
fn a_non_zero_exit_becomes_a_typed_error() {
    assert!(matches!(failure(ERR_NOT_GIT), Error::NotGit(m) if m.contains("git-common-dir")));
    assert!(matches!(failure(ERR_DIRTY), Error::Dirty(m) if m.contains("has uncommitted changes")));
    assert_eq!(failure(ERR_MAIN), Error::MainCheckout);
    assert!(matches!(failure(ERR_INVALID), Error::Invalid(m) if m.contains("bad..name")));
    assert!(matches!(failure(ERR_EXISTS), Error::Invalid(m) if m.contains("already exists")));
    // Anything else keeps the tool's own words; no words at all still says something.
    assert_eq!(failure("✗ disk is on fire\n  more"), Error::Failed("disk is on fire".into()));
    assert_eq!(failure(""), Error::Failed("h0x-wt failed".into()));
    assert!(ERR_CONFLICT.contains("CONFLICT"));
}

#[test]
fn argv_is_json_without_hooks_and_never_yes() {
    let all = [
        switch_args("feat/x", true, Some("main")),
        switch_args("feat/x", false, Some("ignored")),
        remove_args("/w/x", false),
        remove_args("/w/x", true),
        merge_args(None, true),
        merge_args(Some("dev"), false),
    ];
    for a in &all {
        let line = a.join(" ");
        assert!(line.contains("--format json"), "{line}");
        assert!(line.contains("--no-hooks"), "{line}");
        assert!(!a.iter().any(|x| x == "--yes" || x == "-y"), "{line}");
    }
    // `list` runs no hooks and takes no such flag.
    assert_eq!(list_args(), ["list", "--format", "json"]);
    assert_eq!(switch_args("feat/x", true, Some("main"))[..5], ["switch", "--create", "feat/x", "--base", "main"]);
    // --base only means something with --create.
    assert!(!switch_args("feat/x", false, Some("main")).contains(&"--base".to_string()));
    assert!(remove_args("/w/x", false).contains(&"--foreground".to_string()));
    assert!(!remove_args("/w/x", false).contains(&"--force".to_string()));
    assert!(remove_args("/w/x", true).contains(&"--force".to_string()));
    assert_eq!(
        merge_args(Some("dev"), false),
        ["merge", "dev", "--no-remove", "--no-squash", "--no-hooks", "--format", "json"]
    );
    assert_eq!(merge_args(None, true), ["merge", "--no-remove", "--no-hooks", "--format", "json"]);
}

#[test]
fn calls_reject_bad_input_before_running_anything() {
    let home = Path::new("/h");
    for branch in ["", "  ", "-x"] {
        assert!(matches!(create(home, "/", branch, None), Err(Error::Invalid(_))), "{branch:?}");
    }
    assert!(matches!(create(home, "/", "ok", Some("--upload-pack=x")), Err(Error::Invalid(_))));
    assert!(matches!(create(home, "/no/such/folder", "ok", None), Err(Error::NotGit(_))));
    assert!(matches!(list("/no/such/folder"), Err(Error::NotGit(_))));
    assert!(matches!(remove("/no/such/folder", false), Err(Error::NotGit(_))));
    assert!(matches!(merge("/no/such/folder", None, true), Err(Error::NotGit(_))));
}

#[test]
fn without_the_tool_beside_core_every_call_says_so() {
    if std::env::var_os("H0X_WT_BIN").is_some() {
        return;
    }
    let here = std::env::temp_dir();
    let here = here.to_str().unwrap();
    assert_eq!(list(here), Err(Error::MissingTool));
    assert_eq!(create(Path::new("/h"), here, "x", None), Err(Error::MissingTool));
    assert_eq!(remove(here, false), Err(Error::MissingTool));
    assert_eq!(merge(here, None, true), Err(Error::MissingTool));
}

#[test]
fn errors_map_to_stable_api_kinds() {
    let kind = |e: Error| e.rpc().kind;
    assert_eq!(kind(Error::NotGit("x".into())), "not_git");
    assert_eq!(kind(Error::Invalid("x".into())), "invalid_params");
    assert_eq!(kind(Error::Dirty("x".into())), "dirty");
    assert_eq!(kind(Error::CheckedOut("x".into())), "checked_out");
    assert_eq!(kind(Error::MainCheckout), "main_checkout");
    assert_eq!(kind(Error::Conflict(vec!["a".into()])), "conflict");
    assert_eq!(kind(Error::MissingTool), "missing_tool");
    assert_eq!(kind(Error::Failed("x".into())), "failed");
    assert!(Error::Conflict(vec!["a.txt".into(), "b.txt".into()]).rpc().message.contains("a.txt, b.txt"));
    assert_eq!(Error::MissingTool.rpc().hint, "Update h0x-core to use Worktrees");
}

/// The first `n` bytes of a fixture, cut wherever that falls (also inside a UTF-8 letter). The last
/// cut is made from `trim_end()` so CRLF checkouts lose the closing brace too.
fn cut(text: &str, n: usize) -> String {
    String::from_utf8_lossy(&text.as_bytes()[..n.min(text.len())]).into_owned()
}

#[test]
fn truncated_empty_or_null_tool_output_is_a_failure_not_a_panic() {
    for fixture in [LIST_MAIN_ONLY, LIST_WORKTREES] {
        for n in [0, 1, fixture.len() / 3, fixture.len() / 2, fixture.trim_end().len() - 1] {
            let bad = cut(fixture, n);
            assert!(matches!(parse_list(&bad), Err(Error::Failed(m)) if m.contains("unexpected list output")), "{n}");
        }
    }
    for bad in ["", " \n", "null", "{}", r#"{"repo":{}}"#, r#"{"items":[]}"#, r#"{"repo":{},"items":null}"#] {
        assert!(matches!(parse_list(bad), Err(Error::Failed(_))), "{bad:?}");
    }
    for n in [0, 1, SWITCH_CREATED.len() / 2, SWITCH_CREATED.trim_end().len() - 1] {
        assert!(matches!(parse_switch(&cut(SWITCH_CREATED, n)), Err(Error::Failed(m)) if m.contains("switch")), "{n}");
    }
    for bad in ["", "null", "[]", r#"{"action":"created","path":"/h/x"}"#] {
        assert!(matches!(parse_switch(bad), Err(Error::Failed(_))), "{bad:?}");
    }
    for n in [0, 1, REMOVE_DELETED.len() / 2, REMOVE_DELETED.trim_end().len() - 1] {
        assert!(matches!(parse_removal(&cut(REMOVE_DELETED, n)), Err(Error::Failed(m)) if m.contains("remove")), "{n}");
    }
    for bad in ["", "null", r#"[null]"#, r#"[{"branch":7}]"#] {
        assert!(matches!(parse_removal(bad), Err(Error::Failed(_))), "{bad:?}");
    }
}

#[test]
fn unknown_fields_are_ignored_everywhere() {
    let list = LIST_MAIN_ONLY.replacen(r#""schema": 2,"#, r#""schema": 3, "future": {"x": [1, 2]},"#, 1).replacen(
        r#""branch": "main","#,
        r#""branch": "main", "ci": {"state": "green"}, "extra": null,"#,
        1,
    );
    assert_ne!(list, LIST_MAIN_ONLY, "the fixture changed shape: update this test");
    assert_eq!(listed(&list), listed(LIST_MAIN_ONLY));
    let switch =
        r#"{"action":"created","branch":"b","path":"/h/b","created_branch":true,"hooks":[],"new_key":{"a":1}}"#;
    assert_eq!(parse_switch(switch).unwrap(), Created { path: "/h/b".into(), branch: "b".into(), new_branch: true });
    let removal =
        parse_removal(r#"[{"branch":"b","branch_outcome":"some_new_outcome","size":4},{"branch":"c"}]"#).unwrap();
    // Only the first entry counts; an outcome other than "deleted" is not a deletion.
    assert_eq!((removal.branch.as_deref(), removal.branch_outcome.as_deref()), (Some("b"), Some("some_new_outcome")));
}

#[test]
fn an_unknown_or_missing_integration_reason() {
    let row = |integration: &str| {
        format!(
            r#"{{"repo":{{"default_branch":"main"}},"items":[{{"branch":"b","worktree":{{"path":"/r/b"}},"default_branch":{{"ahead":0,"behind":0,"integration":{integration}}}}}]}}"#
        )
    };
    assert_eq!(listed(&row("null")).worktrees[0].integrated, None);
    // A reason this core does not know (a newer tool than the types): that row is "unknown", the
    // rest of the list still parses.
    for unknown in [
        r#"{"reason":"rebased_away"}"#,
        r#"{"reason":""}"#,
        r#"{"reason":null}"#,
        "{}",
        r#""ancestor""#,
        r#"{"reason":"SameCommit"}"#,
    ] {
        let l = listed(&row(unknown));
        assert_eq!(l.worktrees[0].integrated, Some(Integrated::Unknown), "{unknown}");
    }
}

#[test]
fn ahead_and_behind_take_any_u32_and_nothing_else() {
    let row = |ahead: &str| {
        format!(
            r#"{{"repo":{{}},"items":[{{"branch":"b","worktree":{{"path":"/r/b"}},"default_branch":{{"ahead":{ahead},"behind":0}}}}]}}"#
        )
    };
    assert_eq!(listed(&row("0")).worktrees[0].ahead, 0);
    assert_eq!(listed(&row("4294967295")).worktrees[0].ahead, u32::MAX);
    for bad in ["4294967296", "-1", "1.5", r#""3""#, "null"] {
        assert!(matches!(parse_list(&row(bad)), Err(Error::Failed(_))), "{bad}");
    }
}

#[test]
fn the_main_checkout_is_first_wherever_the_tool_puts_it() {
    let json = r#"{"repo":{"default_branch":"main"},"items":[
        {"branch":"old","head":{"committed_at":"2020-01-01T00:00:00Z"},"worktree":{"path":"/w/old"}},
        {"branch":"new","head":{"committed_at":"2026-01-01T00:00:00Z"},"worktree":{"path":"/w/new"}},
        {"branch":"main","head":{"committed_at":"2001-01-01T00:00:00Z"},"worktree":{"path":"/r","main":true},
         "default_branch":{"ahead":9,"behind":9,"integration":{"reason":"same_commit"}}},
        {"branch":"notime","worktree":{"path":"/w/notime"}}
    ]}"#;
    let l = listed(json);
    let order: Vec<_> = l.worktrees.iter().map(|w| w.branch.as_deref().unwrap()).collect();
    assert_eq!(order, ["main", "new", "old", "notime"]);
    // The main checkout is never compared with the default branch.
    assert_eq!((l.worktrees[0].ahead, l.worktrees[0].behind, l.worktrees[0].integrated), (0, 0, None));
    // No head time: 0, not a failure.
    assert_eq!(l.worktrees[3].at, 0);
}

#[test]
fn garbage_commit_times_never_panic() {
    for text in [
        "",
        "T",
        "garbage",
        "2026-13-45T99:99:99Z",
        "2026-11-26T08:00:51+",
        "2026-11-26T08:00:51-0",
        "2026-11-26T08:00:51+é",
        "--T::Z",
        // Absurd years from a broken tool must not overflow the day count.
        "9999999999999999-01-01T00:00:00Z",
        "-9999999999999999-01-01T00:00:00Z",
    ] {
        let json = format!(
            r#"{{"repo":{{}},"items":[{{"branch":"b","head":{{"committed_at":"{text}"}},"worktree":{{"path":"/r/b"}}}}]}}"#
        );
        let _ = listed(&json);
    }
}

#[test]
fn a_failed_call_says_only_the_headline_never_the_rest_of_stderr() {
    let stderr = "◎ Removing feat/x worktree...\n✗ Cannot remove worktree\n  fatal: cannot read /home/alice/.ssh/id_ed25519\n  token=ghp_SECRET123";
    let e = failure(stderr);
    assert_eq!(e, Error::Failed("Cannot remove worktree".into()));
    let rpc = e.rpc();
    assert_eq!((rpc.kind.as_str(), rpc.message.as_str(), rpc.hint.as_str()), ("failed", "Cannot remove worktree", ""));
    // not_git is found in a later line, but the message is still the headline alone.
    let e = failure(ERR_NOT_GIT);
    assert!(matches!(&e, Error::NotGit(m) if !m.contains("fatal") && !m.contains(".git\n")), "{e:?}");
    assert!(!e.rpc().message.contains("parent directories"));
    // The dirty headline names the branch, not the changed files.
    let e = failure(ERR_DIRTY).rpc();
    assert_eq!(e.kind, "dirty");
    assert!(!e.message.contains("u.txt") && !e.message.contains("stash"), "{}", e.message);
    // The missing tool names the tool, not where this core is installed.
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().display().to_string();
    let e = Error::MissingTool.rpc();
    assert!(e.message.contains("h0x-wt") && !e.message.contains(&dir), "{}", e.message);
}

#[test]
fn headlines_with_odd_line_ends_or_markers_still_map() {
    assert!(
        matches!(failure("✗ Invalid branch name bad..name\r\n  more\r\n"), Error::Invalid(m) if m == "Invalid branch name bad..name")
    );
    assert_eq!(failure("✗"), Error::Failed("h0x-wt failed".into()));
    // No ✗ line: the first line.
    assert_eq!(failure("plain words\nsecond"), Error::Failed("plain words".into()));
    // Case does not matter for the kind.
    assert_eq!(failure("✗ THE MAIN WORKTREE CANNOT BE REMOVED"), Error::MainCheckout);
}

#[test]
fn colored_stderr_still_maps_to_the_kind_and_a_plain_message() {
    // A user environment with CLICOLOR_FORCE makes the tool style its stderr even into a pipe.
    let e = failure("\x1b[31m✗\x1b[39m \x1b[31mInvalid branch name \x1b[1mbad..name\x1b[22m\x1b[39m");
    assert!(matches!(&e, Error::Invalid(m) if m == "Invalid branch name bad..name"), "{e:?}");
}

#[test]
fn checked_out_and_conflict_messages_name_the_path_and_files() {
    let e = Error::CheckedOut("C:/Users/dev/.h0x/worktrees/my repo é/feat-x".into()).rpc();
    assert_eq!(e.kind, "checked_out");
    assert!(e.message.contains("C:/Users/dev/.h0x/worktrees/my repo é/feat-x"), "{}", e.message);
    let e = Error::Conflict(vec!["a b.txt".into(), "dir/ü.rs".into()]).rpc();
    assert!(e.message.contains("a b.txt") && e.message.contains("dir/ü.rs"), "{}", e.message);
    // invalid_params is the protocol error; every other kind is an application error.
    assert_eq!(Error::Invalid("x".into()).rpc().code, -32602);
    for e in [Error::NotGit("x".into()), Error::Dirty("x".into()), Error::MainCheckout, Error::MissingTool] {
        assert_eq!(e.rpc().code, -32000);
    }
}

#[test]
fn a_real_non_zero_exit_with_stderr_becomes_a_typed_error() {
    if ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"].iter().any(|v| std::env::var_os(v).is_some()) {
        return; // cannot isolate git from the caller's repository
    }
    let root = crate::platform::test_scratch().join(format!("h0x-core-wt-plain-{}", crate::platform::random_hex()));
    let plain = root.join("plain é");
    std::fs::create_dir_all(&plain).unwrap();
    let empty = root.join("empty.gitconfig");
    std::fs::write(&empty, "").unwrap();
    let empty = empty.display().to_string();
    let env = [
        ("GIT_CEILING_DIRECTORIES", root.display().to_string()),
        ("GIT_CONFIG_GLOBAL", empty.clone()),
        ("GIT_CONFIG_SYSTEM", empty),
        ("LC_ALL", "C".to_string()),
    ];
    // Read-only, and git stops at the scratch root: never another repository.
    let out = spawn(Path::new("git"), &plain, &args(&["rev-parse", "--git-common-dir"]), &env).unwrap();
    assert!(!out.ok && out.stdout.is_empty());
    assert!(matches!(failed(&out), Error::NotGit(_)), "{}", out.stderr);
    // A program that is not there: a typed failure.
    assert!(matches!(spawn(&root.join("no-such-tool"), &plain, &[], &[]), Err(Error::Failed(_))));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn branch_names_core_must_refuse_before_running_anything() {
    let root = crate::platform::test_scratch().join(format!("h0x-core-wt-names-{}", crate::platform::random_hex()));
    std::fs::create_dir_all(&root).unwrap();
    let folder = root.display().to_string();
    let home = root.join("home");
    for branch in ["", " ", "\t\n", "-x", " -x", "--create", "-"] {
        assert!(matches!(create(&home, &folder, branch, None), Err(Error::Invalid(_))), "{branch:?}");
    }
    for base in ["-x", " --orphan", "-"] {
        assert!(matches!(create(&home, &folder, "ok", Some(base)), Err(Error::Invalid(_))), "{base:?}");
    }
    // A NUL cannot be in a branch name, nor in an argv: invalid_params, never "cannot run <tool path>".
    assert!(matches!(create(&home, &folder, "a\0b", None), Err(Error::Invalid(_))));
    assert!(!home.exists(), "nothing was made");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn windows_and_relative_folders_that_do_not_exist_are_not_git() {
    let home = Path::new("/h");
    for folder in [r"C:\no\such\repo é", r"\\?\C:\no\such", "relative/no/such", "", "C:/no such/x"] {
        let r = create(home, folder, "x", None);
        // "" is no folder at all: never the process cwd.
        assert!(matches!(r, Err(Error::NotGit(_))), "{folder:?}: {r:?}");
        assert!(matches!(list(folder), Err(Error::NotGit(_))), "{folder:?}");
    }
}

#[test]
fn paths_compare_with_windows_case_and_trailing_slashes() {
    assert!(same_path("C:/work/x/", r"C:\work\x"));
    assert!(same_path(r"\\?\C:\work\x\", "C:/work/x"));
    assert_eq!(same_path("c:/Work/X", "C:/work/x"), cfg!(windows), "case only matters off Windows");
    assert!(!same_path("C:/work/x", "C:/work/x2"));
    assert!(!same_path("C:/work/x", "C:/work"), "a parent is not the worktree");
    assert!(!same_path("C:/work/x/sub", "C:/work/x"), "a subfolder is not the worktree");
}

#[test]
fn api_params_validate_their_shape() {
    use crate::api::{WorktreeCreateParams, WorktreeListParams, WorktreeMergeParams, WorktreeRemoveParams};
    use serde_json::json;
    fn de<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, serde_json::Error> {
        serde_json::from_value(v)
    }
    // Over the API a list names its folder; create defaults to the core's own.
    assert!(de::<WorktreeListParams>(json!({})).is_err());
    assert!(de::<WorktreeListParams>(json!({"folder": null})).is_err());
    assert!(de::<WorktreeListParams>(json!({"folder": 7})).is_err());
    assert!(de::<WorktreeListParams>(json!({"folder": "/r", "on": 7})).is_err());
    let c: WorktreeCreateParams = de(json!({"branch": "feat/x"})).unwrap();
    assert_eq!((c.folder.as_str(), c.base.is_none(), c.on.is_none()), (".", true, true));
    assert!(de::<WorktreeCreateParams>(json!({"folder": "/r"})).is_err(), "branch is required");
    assert!(de::<WorktreeCreateParams>(json!({"branch": null})).is_err());
    assert!(de::<WorktreeRemoveParams>(json!({})).is_err(), "path is required");
    assert!(de::<WorktreeRemoveParams>(json!({"path": "/w", "force": "yes"})).is_err());
    assert!(!de::<WorktreeRemoveParams>(json!({"path": "/w"})).unwrap().force);
    assert!(de::<WorktreeMergeParams>(json!({})).is_err());
    assert!(de::<WorktreeMergeParams>(json!({"path": "/w", "squash": "no"})).is_err());
    // A Windows path stays as it was sent (the core on that machine reads it).
    let m: WorktreeMergeParams = de(json!({"path": r"C:\Users\dev\.h0x\worktrees\r é\x"})).unwrap();
    assert_eq!((m.path.as_str(), m.squash, m.target.is_none()), (r"C:\Users\dev\.h0x\worktrees\r é\x", true, true));
}

#[test]
fn api_folders_resolve_absolute_windows_and_relative_paths() {
    if cfg!(windows) {
        assert_eq!(crate::exec::folder(Some(r"C:\Users\dev\my repo é"), false), r"C:\Users\dev\my repo é");
        assert_eq!(crate::exec::folder(Some("D:/r"), false), "D:/r");
    } else {
        assert_eq!(crate::exec::folder(Some("/srv/my repo é"), false), "/srv/my repo é");
    }
    let rel = crate::exec::folder(Some("sub dir"), false);
    assert!(Path::new(&rel).is_absolute() && rel.ends_with("sub dir"), "{rel}");
}

#[test]
fn the_schema_has_the_four_worktree_methods_and_their_error_kinds() {
    let committed: serde_json::Value =
        serde_json::from_str(include_str!("../../../../schema/h0x-core.openrpc.json")).unwrap();
    for schema in [crate::api::openrpc(), committed] {
        let methods = schema["methods"].as_array().unwrap();
        for (name, required, kinds) in [
            (
                "worktree.list",
                &["folder"][..],
                &["not_git", "missing_tool", "failed", "invalid_params", "machine_unknown"][..],
            ),
            (
                "worktree.create",
                &["branch"][..],
                &["not_git", "missing_tool", "checked_out", "failed", "invalid_params", "machine_unknown"][..],
            ),
            (
                "worktree.remove",
                &["path"][..],
                &["not_git", "missing_tool", "dirty", "main_checkout", "failed", "invalid_params"][..],
            ),
            (
                "worktree.merge",
                &["path"][..],
                &["not_git", "missing_tool", "conflict", "main_checkout", "failed", "invalid_params"][..],
            ),
        ] {
            let m = methods.iter().find(|m| m["name"] == name).unwrap_or_else(|| panic!("{name} in the schema"));
            let errors: Vec<&str> =
                m["errors"].as_array().unwrap().iter().filter_map(|e| e["data"]["kind"].as_str()).collect();
            for kind in kinds {
                assert!(errors.contains(kind), "{name} lacks {kind}: {errors:?}");
            }
            let params = m["params"].as_array().unwrap();
            assert!(params.iter().any(|p| p["name"] == "on" && p["required"] != true), "{name}: optional on");
            for p in params {
                let must = required.contains(&p["name"].as_str().unwrap());
                assert_eq!(p["required"] == true, must, "{name}: {p}");
            }
        }
    }
}

/// Against the real `h0x-wt`: only when `H0X_WT_BIN` names one (the e2e suite requires it).
mod live {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &str, file: &str, text: &str) {
        std::fs::write(Path::new(dir).join(file), text).unwrap();
        git(Path::new(dir), &["add", "."]);
        git(Path::new(dir), &["commit", "-q", "-m", file]);
    }

    #[test]
    fn create_list_merge_and_remove_with_the_real_tool() {
        if std::env::var_os("H0X_WT_BIN").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("h0x-core-wt-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(root.join("proj é")).unwrap();
        // Git reports real paths (macOS /var is /private/var); Windows paths stay without \?.
        let root = if cfg!(windows) { root } else { root.canonicalize().unwrap() };
        let main = root.join("proj é");
        git(&main, &["init", "-q", "-b", "main"]);
        // The tool commits (squash) itself, so the repository needs an identity of its own.
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        let dir = main.to_str().unwrap().to_string();
        commit(&dir, "a.txt", "one\n");
        let home = root.join("home");

        let x = create(&home, &dir, "feat/x", None).unwrap();
        assert!(x.new_branch && x.path.ends_with("/.h0x/worktrees/proj é/feat-x"), "{x:?}");
        assert_eq!(create(&home, &dir, "feat/x", None), Err(Error::CheckedOut(x.path.clone())));
        assert!(matches!(create(&home, &dir, "bad..name", None), Err(Error::Invalid(_))));
        assert!(matches!(create(&home, &dir, "nobase", Some("no-such-ref")), Err(Error::Invalid(_))));
        let l = list(&dir).unwrap();
        assert_eq!(l.default_branch.as_deref(), Some("main"));
        assert_eq!(l.worktrees.len(), 2);
        assert!(l.worktrees[0].main && l.worktrees[1].branch.as_deref() == Some("feat/x"));

        // Squash merge: the file lands on main, the worktree and branch go.
        commit(&x.path, "x1.txt", "1\n");
        commit(&x.path, "x2.txt", "2\n");
        let m = merge(&x.path, None, true).unwrap();
        assert_eq!((m.target.as_str(), m.merged, m.removed.branch_deleted), ("main", true, true), "{m:?}");
        assert!(main.join("x1.txt").exists() && main.join("x2.txt").exists());
        assert!(!Path::new(&x.path).exists());
        assert_eq!(git(&main, &["log", "--oneline"]).lines().count(), 2);

        // Dirty: refused unless forced; the unmerged branch is kept.
        let y = create(&home, &dir, "feat/y", None).unwrap();
        commit(&y.path, "y.txt", "y\n");
        std::fs::write(Path::new(&y.path).join("scratch.txt"), "s").unwrap();
        assert!(matches!(remove(&y.path, false), Err(Error::Dirty(_))));
        let r = remove(&y.path, true).unwrap();
        assert_eq!((r.branch.as_deref(), r.branch_deleted), (Some("feat/y"), false), "{r:?}");
        assert!(!Path::new(&y.path).exists());
        // The existing branch is checked out again as it is.
        let again = create(&home, &dir, "feat/y", None).unwrap();
        assert!(!again.new_branch);
        assert_eq!(remove(&main.to_string_lossy(), false), Err(Error::MainCheckout));
        assert!(matches!(remove(&root.join("nowhere").to_string_lossy(), false), Err(Error::NotGit(_))));

        // A conflict changes nothing and names the file.
        let c1 = create(&home, &dir, "c1", None).unwrap();
        let c2 = create(&home, &dir, "c2", None).unwrap();
        commit(&c1.path, "a.txt", "from c1\n");
        commit(&c2.path, "a.txt", "from c2\n");
        merge(&c1.path, None, true).unwrap();
        let tip = git(Path::new(&c2.path), &["rev-parse", "HEAD"]);
        let main_tip = git(&main, &["rev-parse", "main"]);
        assert_eq!(merge(&c2.path, None, true), Err(Error::Conflict(vec!["a.txt".into()])));
        assert_eq!(git(Path::new(&c2.path), &["rev-parse", "HEAD"]), tip);
        // (The tool may check files out with CRLF under the user's own git config: ignore line ends.)
        git(Path::new(&c2.path), &["diff", "--ignore-cr-at-eol", "--quiet", "HEAD"]);
        assert_eq!(git(&main, &["rev-parse", "main"]), main_tip);
        // Detached worktrees list without a branch and cannot be merged.
        let det = root.join("det");
        git(&main, &["worktree", "add", "--detach", det.to_str().unwrap()]);
        let l = list(&dir).unwrap();
        assert!(l.worktrees.iter().any(|w| w.branch.is_none() && w.path.ends_with("/det")));
        assert!(matches!(merge(&det.to_string_lossy(), None, true), Err(Error::Invalid(_))));
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// More edges against the real `h0x-wt` (only with `H0X_WT_BIN`): each test in a fresh repository
/// under its own scratch root, git cut off from the caller's env, config and parent folders.
mod live_edges {
    use super::*;

    struct Scratch {
        root: PathBuf,
        empty: PathBuf,
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// None without the tool, or where this process's env would point the tool's git elsewhere
    /// (the tool inherits it: core has no way to clear it for the tool).
    fn scratch(tag: &str) -> Option<Scratch> {
        std::env::var_os("H0X_WT_BIN")?;
        if let Some(var) =
            ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"].into_iter().find(|v| std::env::var_os(v).is_some())
        {
            eprintln!("skipped: {var} is set and h0x-wt would inherit it");
            return None;
        }
        let root = crate::platform::test_scratch().join(format!("h0x-core-wt-{tag}-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        // Git reports real paths (macOS /var is /private/var); Windows paths stay without \\?\.
        let root = if cfg!(windows) { root } else { root.canonicalize().unwrap() };
        let empty = root.join("empty.gitconfig");
        std::fs::write(&empty, "").unwrap();
        std::fs::create_dir_all(root.join("no-hooks")).unwrap();
        Some(Scratch { root, empty })
    }

    fn s(p: &Path) -> String {
        p.display().to_string()
    }

    impl Scratch {
        fn try_git(&self, dir: &Path, args: &[&str]) -> (bool, String, String) {
            // Never the process cwd, never outside the scratch root.
            assert!(
                dir.is_absolute() && dir.starts_with(&self.root),
                "git outside the scratch root: {}",
                dir.display()
            );
            let out = Command::new("git")
                .current_dir(dir)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env("GIT_CONFIG_GLOBAL", &self.empty)
                .env("GIT_CONFIG_SYSTEM", &self.empty)
                .env("GIT_CEILING_DIRECTORIES", &self.root)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "h0x test")
                .env("GIT_AUTHOR_EMAIL", "test@h0x.invalid")
                .env("GIT_COMMITTER_NAME", "h0x test")
                .env("GIT_COMMITTER_EMAIL", "test@h0x.invalid")
                .env("LC_ALL", "C")
                .args(["-c", "commit.gpgsign=false", "-c", "protocol.file.allow=always"])
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
            (out.status.success(), text(&out.stdout), text(&out.stderr))
        }

        fn git(&self, dir: &Path, args: &[&str]) -> String {
            let (ok, out, err) = self.try_git(dir, args);
            assert!(ok, "git {args:?} in {}: {err}", dir.display());
            out
        }

        /// A fresh repository with one commit on `main`, its own identity, and no hooks.
        fn repo(&self, name: &str) -> PathBuf {
            let dir = self.root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            self.git(&dir, &["init", "-q", "-b", "main"]);
            // Guard: everything below commits here, never into an enclosing repository.
            let top = self.git(&dir, &["rev-parse", "--show-toplevel"]);
            assert!(same_path(&top, &s(&dir)), "{top} is not {}", dir.display());
            let hooks = s(&self.root.join("no-hooks"));
            for (k, v) in [
                ("user.name", "h0x test"),
                ("user.email", "test@h0x.invalid"),
                ("commit.gpgsign", "false"),
                ("core.autocrlf", "false"),
                ("core.hooksPath", hooks.as_str()),
            ] {
                self.git(&dir, &["config", k, v]);
            }
            self.commit(&dir, "a.txt", "one\n");
            dir
        }

        fn commit(&self, dir: &Path, file: &str, text: &str) {
            std::fs::write(dir.join(file), text).unwrap();
            self.git(dir, &["add", "--", file]);
            self.git(dir, &["commit", "-q", "-m", file]);
        }

        fn home(&self) -> PathBuf {
            self.root.join("home")
        }

        fn tip(&self, dir: &Path, rev: &str) -> String {
            self.git(dir, &["rev-parse", "--verify", "--quiet", rev])
        }

        fn has_branch(&self, dir: &Path, name: &str) -> bool {
            !self.git(dir, &["branch", "--list", name]).is_empty()
        }

        /// Whether git, looking up past the scratch root too, finds no repository around `dir`.
        fn outside_any_repo(&self, dir: &Path) -> bool {
            let out = Command::new("git")
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", &self.empty)
                .env("GIT_CONFIG_SYSTEM", &self.empty)
                .args(["rev-parse", "--show-toplevel"])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            !out.status.success()
        }
    }

    #[test]
    fn branch_names_git_refuses_are_invalid_params_and_leave_nothing_behind() {
        let Some(sc) = scratch("names") else { return };
        let main = sc.repo("proj");
        let home = sc.home();
        let long = "a".repeat(300);
        for branch in [
            "..",
            "a..b",
            "a b",
            "~x",
            "x~1",
            "x.lock",
            "x/",
            "/x",
            "a//b",
            "@",
            "a@{b",
            "a:b",
            "a?b",
            "a*b",
            "a[b",
            r"a\b",
            ".x",
            "a\tb",
            long.as_str(),
        ] {
            let r = create(&home, &s(&main), branch, None);
            assert!(matches!(r, Err(Error::Invalid(_))), "{:?}: {r:?}", &branch[..branch.len().min(20)]);
        }
        assert_eq!(sc.git(&main, &["branch", "--format=%(refname:short)"]), "main", "no branch was made");
        assert_eq!(sc.git(&main, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
        // Valid: non-ASCII letters; spaces around a name, and a blank base, are dropped.
        let u = create(&home, &s(&main), "fix/ünï", None).unwrap();
        assert!(u.new_branch && u.branch == "fix/ünï" && Path::new(&u.path).is_dir(), "{u:?}");
        let t = create(&home, &s(&main), "  feat/t  ", Some("  ")).unwrap();
        assert_eq!((t.branch.as_str(), t.new_branch), ("feat/t", true));
        assert!(t.path.contains("/.h0x/worktrees/"), "{t:?}");
    }

    #[test]
    fn a_folder_outside_any_repository_is_not_git_for_every_verb() {
        let Some(sc) = scratch("notgit") else { return };
        let plain = sc.root.join("plain é");
        std::fs::create_dir_all(&plain).unwrap();
        if !sc.outside_any_repo(&plain) {
            eprintln!("skipped: the scratch folder is inside a repository, which the tool would use");
            return;
        }
        let p = s(&plain);
        assert!(matches!(list(&p), Err(Error::NotGit(_))));
        assert!(matches!(create(&sc.home(), &p, "x", None), Err(Error::NotGit(_))));
        assert!(matches!(remove(&p, false), Err(Error::NotGit(_))));
        assert!(matches!(remove(&p, true), Err(Error::NotGit(_))));
        assert!(matches!(merge(&p, None, true), Err(Error::NotGit(_))));
        assert!(!sc.home().exists() && std::fs::read_dir(&plain).unwrap().next().is_none(), "nothing was written");
    }

    #[test]
    fn a_branch_checked_out_elsewhere_is_checked_out_naming_that_path() {
        let Some(sc) = scratch("out") else { return };
        let main = sc.repo("proj");
        let home = sc.home();
        // The main checkout has `main`.
        let r = create(&home, &s(&main), "main", None);
        assert!(matches!(&r, Err(Error::CheckedOut(p)) if same_path(p, &s(&main))), "{r:?}");
        // A worktree git made somewhere else, not under ~/.h0x.
        let other = sc.root.join("other place é");
        sc.git(&main, &["worktree", "add", "-q", "-b", "side", &s(&other)]);
        let r = create(&home, &s(&main), "side", None);
        assert!(matches!(&r, Err(Error::CheckedOut(p)) if same_path(p, &s(&other))), "{r:?}");
        // Asked from inside that worktree.
        let r = create(&home, &s(&other), "main", None);
        assert!(matches!(&r, Err(Error::CheckedOut(p)) if same_path(p, &s(&main))), "{r:?}");
        assert!(!home.join(".h0x").exists(), "no second worktree was made");
    }

    #[test]
    fn the_main_checkout_is_never_removed_or_merged_in_any_spelling() {
        let Some(sc) = scratch("main") else { return };
        let main = sc.repo("proj é");
        let tip = sc.tip(&main, "refs/heads/main");
        let mut spellings = vec![s(&main), format!("{}/", s(&main))];
        if cfg!(windows) {
            spellings.push(s(&main).replace('/', "\\") + "\\");
            spellings.push(s(&main).replace('\\', "/"));
        }
        for p in &spellings {
            for force in [false, true] {
                assert_eq!(remove(p, force), Err(Error::MainCheckout), "{p} force={force}");
            }
            assert_eq!(merge(p, None, true), Err(Error::MainCheckout), "{p}");
            assert_eq!(merge(p, None, false), Err(Error::MainCheckout), "{p}");
        }
        assert!(main.join(".git").is_dir() && main.join("a.txt").is_file());
        assert_eq!(sc.tip(&main, "refs/heads/main"), tip);
    }

    #[test]
    fn staged_modified_or_deleted_changes_are_dirty_until_forced() {
        let Some(sc) = scratch("dirty") else { return };
        let main = sc.repo("proj");
        let home = sc.home();
        for (name, change) in [("staged", 0), ("modified", 1), ("deleted", 2)] {
            let w = create(&home, &s(&main), name, None).unwrap();
            let wp = PathBuf::from(&w.path);
            match change {
                0 => {
                    std::fs::write(wp.join("new.txt"), "n").unwrap();
                    sc.git(&wp, &["add", "new.txt"]);
                }
                1 => std::fs::write(wp.join("a.txt"), "changed\n").unwrap(),
                _ => std::fs::remove_file(wp.join("a.txt")).unwrap(),
            }
            let r = remove(&w.path, false);
            assert!(matches!(&r, Err(Error::Dirty(_))), "{name}: {r:?}");
            assert!(wp.is_dir() && sc.has_branch(&main, name), "{name}: refused means untouched");
            let r = remove(&w.path, true).unwrap();
            assert!(!wp.exists(), "{name}");
            // No commits of its own: its work is in main, so the branch goes too.
            assert_eq!((r.branch.as_deref(), r.branch_deleted), (Some(name), true), "{name}: {r:?}");
        }
    }

    #[test]
    fn remove_takes_the_worktree_in_any_spelling_but_never_a_subfolder() {
        let Some(sc) = scratch("spell") else { return };
        let main = sc.repo("proj");
        let w = create(&sc.home(), &s(&main), "feat/s", None).unwrap();
        let sub = Path::new(&w.path).join("sub dir");
        std::fs::create_dir_all(&sub).unwrap();
        let r = remove(&s(&sub), false);
        assert!(matches!(&r, Err(Error::Invalid(_))), "{r:?}");
        assert!(sub.is_dir(), "a subfolder names no worktree: nothing removed");
        std::fs::remove_dir(&sub).unwrap();
        let spelled = if cfg!(windows) { w.path.replace('/', "\\") + "\\" } else { format!("{}/", w.path) };
        let r = remove(&spelled, false).unwrap();
        assert!(same_path(&r.path, &w.path) && !Path::new(&w.path).exists(), "{r:?}");
    }

    /// A worktree whose submodule has a commit that exists nowhere else; clean otherwise.
    fn with_an_unpushed_submodule_commit(sc: &Scratch) -> (Created, PathBuf, String) {
        let main = sc.repo("proj");
        let origin = sc.repo("sub origin");
        let w = create(&sc.home(), &s(&main), "with-sub", None).unwrap();
        let wp = PathBuf::from(&w.path);
        sc.git(&wp, &["submodule", "add", "-q", &s(&origin), "sub"]);
        sc.git(&wp, &["commit", "-q", "-m", "add sub"]);
        let sub = wp.join("sub");
        sc.commit(&sub, "only-here.txt", "unpushed\n");
        sc.git(&wp, &["add", "sub"]);
        sc.git(&wp, &["commit", "-q", "-m", "bump sub"]);
        let tip = sc.tip(&sub, "HEAD");
        assert_eq!(sc.git(&wp, &["status", "--porcelain"]), "", "clean: only the push is missing");
        assert!(!sc.try_git(&origin, &["cat-file", "-e", &tip]).0, "the commit is not in the origin");
        (w, sub, tip)
    }

    #[test]
    fn a_worktree_with_unpushed_submodule_commits_is_refused() {
        let Some(sc) = scratch("sub") else { return };
        let (w, sub, tip) = with_an_unpushed_submodule_commit(&sc);
        let r = remove(&w.path, false);
        assert!(r.is_err(), "{r:?}");
        assert!(sub.is_dir() && sc.tip(&sub, "HEAD") == tip, "the unpushed commit is still there");
    }

    #[test]
    fn a_forced_remove_of_a_worktree_with_unpushed_submodule_commits_is_allowed() {
        let Some(sc) = scratch("subforce") else { return };
        let (w, _, _) = with_an_unpushed_submodule_commit(&sc);
        // Decision: force is the user's say-so, also for commits that exist nowhere else.
        let r = remove(&w.path, true);
        assert!(r.is_ok(), "{r:?}");
        assert!(!Path::new(&w.path).exists());
    }

    #[test]
    fn a_conflict_lists_every_file_and_leaves_the_repository_as_it_was() {
        let Some(sc) = scratch("conflict") else { return };
        let main = sc.repo("proj");
        sc.commit(&main, "b c.txt", "base\n");
        sc.commit(&main, "ü.txt", "base\n");
        let home = sc.home();
        let c1 = create(&home, &s(&main), "c1", None).unwrap();
        let c2 = create(&home, &s(&main), "c2", None).unwrap();
        let (p1, p2) = (PathBuf::from(&c1.path), PathBuf::from(&c2.path));
        for (dir, text) in [(&p1, "from c1\n"), (&p2, "from c2\n")] {
            for file in ["a.txt", "b c.txt", "ü.txt"] {
                std::fs::write(dir.join(file), text).unwrap();
            }
            sc.git(dir, &["add", "-A"]);
            sc.git(dir, &["commit", "-q", "-m", "all three"]);
        }
        merge(&c1.path, None, true).unwrap();
        // A second commit, so a squash would have to rewrite the branch.
        sc.commit(&p2, "d.txt", "d\n");
        let (tip, main_tip) = (sc.tip(&main, "refs/heads/c2"), sc.tip(&main, "refs/heads/main"));
        for squash in [true, false] {
            let r = merge(&c2.path, None, squash);
            let mut files = match r {
                Err(Error::Conflict(files)) => files,
                other => panic!("squash={squash}: {other:?}"),
            };
            files.sort();
            assert_eq!(files, ["a.txt", "b c.txt", "ü.txt"], "squash={squash}: plain names, unquoted");
            assert_eq!(sc.tip(&main, "refs/heads/c2"), tip, "squash={squash}: the branch is unchanged");
            assert_eq!(sc.tip(&p2, "HEAD"), tip, "squash={squash}");
            assert_eq!(sc.tip(&main, "refs/heads/main"), main_tip, "squash={squash}: the target is unchanged");
            for state in ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD"] {
                let at = sc.git(&p2, &["rev-parse", "--git-path", state]);
                assert!(!p2.join(&at).exists(), "squash={squash}: {state} left behind");
            }
            assert_eq!(sc.git(&p2, &["status", "--porcelain"]), "", "squash={squash}: clean");
        }
    }

    #[test]
    fn a_branch_with_nothing_to_merge_is_already_integrated_and_removed() {
        let Some(sc) = scratch("noop") else { return };
        let main = sc.repo("proj");
        let home = sc.home();
        let main_tip = sc.tip(&main, "refs/heads/main");
        let w = create(&home, &s(&main), "noop", None).unwrap();
        let m = merge(&w.path, None, true).unwrap();
        assert_eq!((m.target.as_str(), m.merged, m.removed.branch.as_deref()), ("main", false, Some("noop")), "{m:?}");
        assert!(m.removed.branch_deleted && m.removed.integrated.is_some(), "{m:?}");
        assert!(!Path::new(&w.path).exists() && !sc.has_branch(&main, "noop"));
        assert_eq!(sc.tip(&main, "refs/heads/main"), main_tip);
        // The same change already landed on main under another commit.
        let w = create(&home, &s(&main), "landed", None).unwrap();
        sc.commit(Path::new(&w.path), "l.txt", "same\n");
        sc.commit(&main, "l.txt", "same\n");
        let main_tip = sc.tip(&main, "refs/heads/main");
        let m = merge(&w.path, None, true).unwrap();
        assert!(!m.merged && m.removed.branch_deleted, "{m:?}");
        assert!(!Path::new(&w.path).exists() && !sc.has_branch(&main, "landed"));
        assert_eq!(sc.tip(&main, "refs/heads/main"), main_tip);
    }

    #[test]
    fn a_merge_into_another_target_lands_there_and_removes_the_branch() {
        let Some(sc) = scratch("target") else { return };
        let main = sc.repo("proj");
        sc.git(&main, &["branch", "dev"]);
        let w = create(&sc.home(), &s(&main), "feat/d", None).unwrap();
        sc.commit(Path::new(&w.path), "d.txt", "d\n");
        let (main_tip, dev_tip, tip) =
            (sc.tip(&main, "refs/heads/main"), sc.tip(&main, "refs/heads/dev"), sc.tip(&main, "refs/heads/feat/d"));
        // A target that does not exist: refused, nothing changed.
        let r = merge(&w.path, Some("no-such-branch"), true);
        assert!(matches!(r, Err(Error::Invalid(_) | Error::Failed(_))), "{r:?}");
        assert!(Path::new(&w.path).is_dir() && sc.tip(&main, "refs/heads/feat/d") == tip);
        // Spaces around the target are dropped.
        let m = merge(&w.path, Some(" dev "), true).unwrap();
        assert_eq!((m.target.as_str(), m.merged), ("dev", true), "{m:?}");
        assert_ne!(sc.tip(&main, "refs/heads/dev"), dev_tip);
        assert_eq!(sc.tip(&main, "refs/heads/main"), main_tip, "main was not the target");
        assert!(!Path::new(&w.path).exists());
        // The API promises the branch goes with the worktree after a merge.
        assert!(m.removed.branch_deleted && !sc.has_branch(&main, "feat/d"), "{m:?}");
    }
}
