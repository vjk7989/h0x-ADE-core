//! `h0x-core route`: a rules-only session classifier. From the working directory and the first
//! prompt it picks the session kind, the tool list, whether a code map helps and whether the
//! cheap model is enough. No model call, no network; the same input gives the same answer.
//! Anything unclear (empty prompt, missing dir, no directory) falls back to the edit profile,
//! the one that takes nothing away.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;

const MANIFESTS: [&str; 7] =
    ["Cargo.toml", "package.json", "pyproject.toml", "setup.py", "go.mod", "pom.xml", "build.gradle"];
pub(crate) const SRC_EXTS: [&str; 14] =
    ["rs", "js", "ts", "tsx", "py", "go", "java", "kt", "c", "cc", "cpp", "h", "rb", "cs"];
const SRC_DIRS: [&str; 3] = ["src", "lib", "crates"];
/// Source files that make a manifest-less directory a code repo.
const SRC_MIN: usize = 3;
const READ_ONLY_TOOLS: [&str; 3] = ["Bash", "Read", "Grep"];
const EDIT_TOOLS: [&str; 5] = ["Bash", "Read", "Grep", "Edit", "Write"];

#[derive(Serialize, Deserialize, JsonSchema, Default, Debug)]
#[serde(default)]
pub struct RouteParams {
    /// The session's working directory. Absent on a remote machine: no repo is looked up.
    pub cwd: Option<String>,
    /// The first prompt of the session.
    pub prompt: String,
}

#[derive(Serialize, Deserialize, JsonSchema, PartialEq, Debug)]
pub struct RouteResult {
    /// `edit`, `code-nav`, `code-read` or `logs-data-general`.
    pub kind: String,
    /// The directory is a code repo.
    pub code: bool,
    /// The prompt forbids changes.
    pub read_only: bool,
    /// The prompt asks about callers, users or definitions.
    pub nav: bool,
    /// A code map is worth adding: a code repo, read-only, navigation-shaped.
    pub graft: bool,
    /// The tools the session needs.
    pub tools: Vec<String>,
    /// `haiku` for a read-only logs, data or general session, else null (keep the chosen model).
    pub model: Option<String>,
}

/// Classify one session start.
pub fn route(params: &RouteParams) -> RouteResult {
    let code = params.cwd.as_deref().is_some_and(|d| is_code_repo(Path::new(d)));
    // JS semantics, as measured: ASCII-only case folding, gap limits in UTF-16 units.
    let chars: Vec<u16> =
        params.prompt.encode_utf16().map(|u| if u < 128 { (u as u8).to_ascii_lowercase() as u16 } else { u }).collect();
    let read_only = is_read_only(&chars);
    let nav = is_nav(&chars);
    let kind = match (read_only, code, nav) {
        (false, ..) => "edit",
        (true, true, true) => "code-nav",
        (true, true, false) => "code-read",
        (true, false, _) => "logs-data-general",
    };
    let tools: &[&str] = if read_only { &READ_ONLY_TOOLS } else { &EDIT_TOOLS };
    let haiku = kind == "logs-data-general" && params.cwd.is_some();
    RouteResult {
        kind: kind.into(),
        code,
        read_only,
        nav,
        graft: code && read_only && nav,
        tools: tools.iter().map(|t| t.to_string()).collect(),
        model: haiku.then(|| "haiku".into()),
    }
}

/// The command: `{cwd?, prompt}` on stdin, the decision on stdout. Unreadable input is an empty
/// request, so the answer is always the edit profile; exit 0.
pub fn run() -> i32 {
    let mut raw = Vec::new();
    let params = match std::io::stdin().read_to_end(&mut raw) {
        Ok(_) => serde_json::from_slice(&raw).unwrap_or_default(),
        Err(_) => RouteParams::default(),
    };
    if !crate::client::vouched() {
        return 0; // outside a HyperOx terminal: no routing decision
    }
    let out = serde_json::to_string(&route(&params)).unwrap_or_default();
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{out}");
    let _ = stdout.flush();
    0
}

fn is_source(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| SRC_EXTS.contains(&ext))
}

/// A manifest in the directory, or at least `SRC_MIN` source files in it and one level down
/// (`src`, `lib`, `crates`). Unreadable means no.
fn is_code_repo(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    if names.iter().any(|n| MANIFESTS.contains(&n.as_str())) {
        return true;
    }
    let mut src = 0;
    for name in &names {
        if is_source(name) {
            src += 1;
        } else if SRC_DIRS.contains(&name.as_str())
            && let Ok(inner) = std::fs::read_dir(dir.join(name))
        {
            src += inner.flatten().filter(|e| is_source(&e.file_name().to_string_lossy())).count();
        }
    }
    src >= SRC_MIN
}

/// The JS `\w`: ASCII letters, digits and `_` only.
fn is_word(u: u16) -> bool {
    u < 128 && ((u as u8).is_ascii_alphanumeric() || u == b'_' as u16)
}

/// Marks where `needle` (ASCII) starts with a word boundary on both sides.
fn starts_of(hay: &[u16], needle: &str, marks: &mut [bool]) {
    let n = needle.as_bytes();
    if hay.len() < n.len() {
        return;
    }
    for start in 0..=hay.len() - n.len() {
        let end = start + n.len();
        if hay[start..end].iter().zip(n).all(|(&h, &b)| h == b as u16)
            && (start == 0 || !is_word(hay[start - 1]))
            && (end == hay.len() || !is_word(hay[end]))
        {
            marks[start] = true;
        }
    }
}

/// Ends of the occurrences of `needle` with word boundaries.
fn ends_of(hay: &[u16], needle: &str) -> Vec<usize> {
    let mut marks = vec![false; hay.len()];
    starts_of(hay, needle, &mut marks);
    (0..hay.len()).filter(|&i| marks[i]).map(|i| i + needle.len()).collect()
}

/// `first`, then up to `gap` UTF-16 units that are not `.` or `?`, then `second`.
/// One scan of at most `gap` units per `first` match: linear in the prompt.
fn find_gap(hay: &[u16], first: &[&str], gap: usize, second: &[&str]) -> bool {
    let mut starts = vec![false; hay.len()];
    for s in second {
        starts_of(hay, s, &mut starts);
    }
    first.iter().flat_map(|f| ends_of(hay, f)).any(|end| {
        for s in end..=(end + gap).min(hay.len().saturating_sub(1)) {
            if starts[s] {
                return true;
            }
            if hay[s] == b'.' as u16 || hay[s] == b'?' as u16 {
                return false;
            }
        }
        false
    })
}

fn any_of(hay: &[u16], needles: &[&str]) -> bool {
    needles.iter().any(|n| !ends_of(hay, n).is_empty())
}

/// The prompt forbids changes.
fn is_read_only(hay: &[u16]) -> bool {
    any_of(hay, &["do not change anything", "do not answer or change anything", "don't change anything", "read-only"])
}

/// The prompt is about callers, users, definitions or implementors.
fn is_nav(hay: &[u16]) -> bool {
    any_of(
        hay,
        &[
            "call site",
            "call sites",
            "caller",
            "callers",
            "who calls",
            "who uses",
            "what breaks",
            "blast radius",
            "implement",
            "implements",
            "used by",
            "reference to",
            "references to",
        ],
    ) || find_gap(hay, &["where is", "where are"], 60, &["defined", "handled", "declared", "implemented"])
        || find_gap(hay, &["where"], 40, &["handled"])
        || find_gap(hay, &["which files"], 60, &["use"])
}

#[cfg(test)]
mod tests {
    use super::*;

    const RO: &str = "Do not change anything.";

    /// A fresh empty folder under the system temp dir, removed on drop.
    struct Tmp(std::path::PathBuf);

    impl Tmp {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("h0x-route-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn go(cwd: Option<&Path>, prompt: &str) -> RouteResult {
        route(&RouteParams { cwd: cwd.map(|p| p.to_string_lossy().into_owned()), prompt: prompt.into() })
    }

    fn repo() -> Tmp {
        let d = Tmp::new();
        std::fs::write(d.path().join("Cargo.toml"), "").unwrap();
        d
    }

    fn plain() -> Tmp {
        Tmp::new()
    }

    fn touch(dir: &Path, names: &[&str]) {
        for n in names {
            std::fs::write(dir.join(n), "").unwrap();
        }
    }

    fn edit_profile(r: &RouteResult) {
        assert_eq!(r.kind, "edit");
        assert!(!r.read_only && !r.graft);
        assert_eq!(r.tools, EDIT_TOOLS);
        assert_eq!(r.model, None);
    }

    #[test]
    fn read_only_phrases() {
        for p in [
            "Do not change anything",
            "please do not answer or change anything here",
            "Don't change anything",
            "this is read-only",
            "DO NOT CHANGE ANYTHING",
            "READ-ONLY task",
            "Don't Change Anything.",
        ] {
            assert!(go(None, p).read_only, "{p}");
        }
        for p in ["change everything", "readonly", "read only", "do not changes anything", "undo not change anything"] {
            assert!(!go(None, p).read_only, "{p}");
        }
    }

    #[test]
    fn nav_phrases() {
        for p in [
            "list the call sites of foo",
            "one call site",
            "Callers of parse",
            "who calls run",
            "WHO USES this",
            "what breaks if I rename it",
            "the blast radius",
            "where is the parser defined",
            "Where are errors handled?",
            "where do we see this handled",
            "what implements Display",
            "it implement Foo",
            "which files use the config",
            "used by whom",
            "references to x",
            "reference to x",
        ] {
            assert!(go(None, p).nav, "{p}");
        }
        for p in [
            "where is the parser. defined",
            "where is the parser? defined",
            "callersx",
            "recaller",
            "which files are great",
            "fix the bug",
        ] {
            assert!(!go(None, p).nav, "{p}");
        }
    }

    #[test]
    fn nav_gap_limits() {
        let pad = |n: usize| "x".repeat(n);
        // "where is" + gap + "defined": the gap counts the characters between, spaces included.
        assert!(go(None, &format!("where is {} defined", pad(58))).nav);
        assert!(!go(None, &format!("where is {} defined", pad(59))).nav);
        assert!(go(None, &format!("where {} handled", pad(38))).nav);
        assert!(!go(None, &format!("where {} handled", pad(39))).nav);
        assert!(go(None, &format!("which files {} use", pad(58))).nav);
        assert!(!go(None, &format!("which files {} use", pad(59))).nav);
    }

    #[test]
    fn kinds_and_tools() {
        let r = repo();
        let nav = go(Some(r.path()), &format!("{RO} Who calls main?"));
        assert_eq!(nav.kind, "code-nav");
        assert!(nav.code && nav.read_only && nav.nav && nav.graft);
        assert_eq!(nav.tools, READ_ONLY_TOOLS);
        assert_eq!(nav.model, None);

        let read = go(Some(r.path()), &format!("{RO} Summarize it."));
        assert_eq!(read.kind, "code-read");
        assert!(!read.graft);
        assert_eq!(read.tools, READ_ONLY_TOOLS);
        assert_eq!(read.model, None);

        let p = plain();
        let data = go(Some(p.path()), &format!("{RO} Summarize the log."));
        assert_eq!(data.kind, "logs-data-general");
        assert!(!data.code && !data.graft);
        assert_eq!(data.tools, READ_ONLY_TOOLS);
        assert_eq!(data.model.as_deref(), Some("haiku"));

        let edit = go(Some(r.path()), "Fix the bug in parse.");
        edit_profile(&edit);
        assert!(edit.code);
    }

    #[test]
    fn navigation_words_in_an_edit_prompt() {
        let r = repo();
        let d = go(Some(r.path()), "Rename the function; who calls it? Update the callers.");
        edit_profile(&d);
        assert!(d.nav && d.code);
    }

    #[test]
    fn read_only_nav_in_a_plain_dir_gets_no_graft() {
        let p = plain();
        let d = go(Some(p.path()), &format!("{RO} Who calls main?"));
        assert_eq!(d.kind, "logs-data-general");
        assert!(d.nav && !d.graft);
        assert_eq!(d.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn remote_has_no_graft_and_no_model() {
        let d = go(None, &format!("{RO} Who calls main?"));
        assert!(!d.code && !d.graft && d.read_only);
        assert_eq!(d.model, None);
        assert_eq!(d.tools, READ_ONLY_TOOLS);
        edit_profile(&go(None, "Fix it"));
    }

    #[test]
    fn empty_and_blank_prompts_are_the_edit_profile() {
        let r = repo();
        let p = plain();
        for dir in [Some(r.path()), Some(p.path()), None] {
            for prompt in ["", " ", "  \t\r\n  "] {
                edit_profile(&go(dir, prompt));
            }
        }
    }

    #[test]
    fn missing_or_unreadable_dir_is_not_a_repo() {
        let p = plain();
        let missing = p.path().join("nope");
        let d = go(Some(&missing), "Fix it");
        edit_profile(&d);
        assert!(!d.code);
        // A file is not a directory.
        let f = p.path().join("Cargo.toml");
        touch(p.path(), &["Cargo.toml"]);
        assert!(!go(Some(&f), "Fix it").code);
        // A read-only prompt there is general work, but nothing is routed to haiku wrongly: the dir is given.
        assert_eq!(go(Some(&missing), RO).kind, "logs-data-general");
        assert!(!go(Some(Path::new("")), "x").code);
    }

    #[test]
    fn windows_style_cwd_never_panics() {
        for cwd in [
            r"C:\",
            r"C:\Users\nobody\no-such-dir-h0x",
            r"\\?\C:\no-such-dir-h0x",
            r"\\server\share\no-such-dir-h0x",
            r"\\?\UNC\server\share\x",
            "C:/mixed\\slashes/no-such-dir-h0x",
            "Z:\\nope",
        ] {
            let d = route(&RouteParams { cwd: Some(cwd.into()), prompt: "Fix it".into() });
            assert_eq!(d.kind, "edit", "{cwd}");
            assert_eq!(d.model, None);
            let ro = route(&RouteParams { cwd: Some(cwd.into()), prompt: RO.into() });
            assert!(!ro.graft, "{cwd}");
        }
    }

    #[test]
    fn manifests_make_a_repo() {
        for m in MANIFESTS {
            let d = plain();
            touch(d.path(), &[m]);
            assert!(go(Some(d.path()), "x").code, "{m}");
        }
    }

    #[test]
    fn source_file_count_boundary() {
        let two = plain();
        touch(two.path(), &["a.rs", "b.py"]);
        assert!(!go(Some(two.path()), "x").code);
        let three = plain();
        touch(three.path(), &["a.rs", "b.py", "c.go"]);
        assert!(go(Some(three.path()), "x").code);
        let other = plain();
        touch(other.path(), &["a.txt", "b.md", "c.json", "d.rs", "e.RS", "rs"]);
        assert!(!go(Some(other.path()), "x").code);
    }

    #[test]
    fn source_files_one_level_down_count() {
        for sub in SRC_DIRS {
            let d = plain();
            touch(d.path(), &["a.rs"]);
            std::fs::create_dir(d.path().join(sub)).unwrap();
            touch(&d.path().join(sub), &["x.ts", "y.rb"]);
            assert!(go(Some(d.path()), "x").code, "{sub}");
        }
        // 1 + 1 is not enough.
        let d = plain();
        touch(d.path(), &["a.rs"]);
        std::fs::create_dir(d.path().join("src")).unwrap();
        touch(&d.path().join("src"), &["x.ts"]);
        assert!(!go(Some(d.path()), "x").code);
        // Two levels down and other folders are not read.
        let d = plain();
        std::fs::create_dir_all(d.path().join("src").join("deep")).unwrap();
        touch(&d.path().join("src").join("deep"), &["a.rs", "b.rs", "c.rs"]);
        std::fs::create_dir(d.path().join("other")).unwrap();
        touch(&d.path().join("other"), &["a.rs", "b.rs", "c.rs"]);
        assert!(!go(Some(d.path()), "x").code);
    }

    #[test]
    fn tool_names_are_safe_to_join_with_commas() {
        for tools in [READ_ONLY_TOOLS.as_slice(), EDIT_TOOLS.as_slice()] {
            for t in tools {
                assert!(!t.is_empty() && !t.contains(',') && t.trim() == *t, "{t:?}");
            }
        }
        for prompt in ["", RO, "Fix it"] {
            for t in go(None, prompt).tools {
                assert!(!t.is_empty() && !t.contains(','), "{t:?}");
            }
        }
    }

    #[test]
    fn json_shape() {
        let v = serde_json::to_value(go(None, "hi")).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"kind":"edit","code":false,"read_only":false,"nav":false,"graft":false,
                "tools":["Bash","Read","Grep","Edit","Write"],"model":null})
        );
        let p: RouteParams = serde_json::from_str("{}").unwrap();
        assert!(p.cwd.is_none() && p.prompt.is_empty());
    }

    /// The R7 task suite (r6/tasks6.js), each first prompt as `glue/router7.js` classifies it: id, the
    /// prototype's `code` for the task's real folder, read_only, nav, kind, the model the AC gives that
    /// kind with a cwd, and the kind without a cwd (`decide({prompt})`).
    #[test]
    #[allow(clippy::type_complexity)]
    fn the_r7_task_suite_routes_as_the_prototype_does() {
        #[rustfmt::skip]
        let rows: [(&str, bool, bool, bool, &str, Option<&str>, &str, &str); 36] = [
            ("G1", true, true, true, "code-nav", None, "logs-data-general", "Task: Find every call site of the Rust function `connect_or_start` in this repository (not its definition). For each call site give the file path, the line number and the name of the enclosing function. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"call_sites\" (array of [file path, enclosing function name] pairs, one per call site)."),
            ("C2", true, true, true, "code-nav", None, "logs-data-general", "Task: When a client calls the JSON-RPC method `terminal.list` on the core, where is it handled? Give the handler function, its file and line, and explain how it treats the `all` parameter and other machines. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"handler_fn\" (handler function name), \"file\" (file path), \"line\" (line number of the handler function), \"all_includes_finished\" (true if `all` makes the list include finished (not live) terminals), \"asks_other_machines\" (true if the handler also asks other machines for their terminals)."),
            ("C3", true, true, true, "code-nav", None, "logs-data-general", "Task: If the signature of the Rust function `crate::remote::forward` changes, what breaks? List every direct call site (file, line, enclosing function), and for any caller that is itself a thin wrapper, list its callers too. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"direct\" (array of [file, enclosing function] for every direct call site), \"wrapper_callers\" (array of [file, enclosing function] calling the thin wrapper(s))."),
            ("C4", true, true, true, "code-nav", None, "logs-data-general", "Task: Explain the module crates/h0x-core/src/sweep.rs: its purpose, its public items (constants and functions), and which files outside the module use each public item. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"public_items\" (names of the public items (each name once)), \"users\" (object: public item name -> array of files outside sweep.rs that use it)."),
            ("G5", true, true, false, "code-read", None, "logs-data-general", "Task: Read the whole file crates/h0x-core/src/bridge/web/tunnel.rs and give a short overview of it: its purpose, the names of all its public functions (including public methods), what `trusted` checks before cloudflared is started, and the reasons `must_stop` can return. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"public_fns\" (names of all public functions and public methods (each name once)), \"public_fn_count\" (number of `pub fn` / `pub async fn` definitions in the file), \"must_stop_reasons\" (array of the reasons `must_stop` can return)."),
            ("G6", true, true, false, "code-read", None, "logs-data-general", "Task: Read the whole file crates/h0x-core/src/bridge/web/tests.rs. How many test functions (marked #[test]) does it contain, and what are the names of the last three test functions in the file? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"test_count\" (number of #[test] functions), \"last_three\" (names of the last three test functions, in file order)."),
            ("N7", true, true, true, "code-nav", None, "logs-data-general", "Task: Name every type that implements the trait `Drop` in a file under crates/h0x-core/src/bridge/ (any depth), and the file of each implementation. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"implementors\" (array of [type name, file path] pairs, one per `impl Drop for` block)."),
            ("G2", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: The file build/test-run.log is the output of a long `cargo test` run. Which test failed? Give its full test path and the assertion values (left and right). Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"test_path\" (full test path), \"left\" (left value), \"right\" (right value)."),
            ("L2", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: logs/app.log is the log of an app with six services. Find the first ERROR line of service `billing` at or after 14:30:00 (the time as written in the log). Give its timestamp, request id and message code. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"timestamp\" (the full timestamp as written), \"request_id\" (the req= value), \"code\" (the code= value)."),
            ("L3", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: logs/ci.log is a CI log with one section per job (each starts with `=== job <name>`). Which jobs failed? For each give its exit code and the step that failed. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"failed_jobs\" (array of [job name, exit code, failing step name])."),
            ("L4", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: logs/worker.log has periodic stats lines from 12 workers. What is the peak `rss_mb` reported by `worker-7`, and on which line of the file (1-based) is it? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"peak_rss_mb\" (peak rss_mb of worker-7), \"line\" (1-based line number of that line)."),
            ("L5", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: logs/reload.log is a small service log. Which config key was reloaded most often (lines `config reloaded: key=...`), and how many times? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"key\" (the config key), \"count\" (number of reloads of that key)."),
            ("G3", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: data/fleet.json describes a fleet of machines and their terminals. Find the terminal labelled exactly `deploy-canary-17`: which machine is it on, which port does it listen on, and what is its state? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"machine\" (machine name), \"port\" (port), \"state\" (state)."),
            ("J2", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: data/orders.json is an array of orders. What is the total `amount` of the orders whose `status` is exactly `failed`, and how many such orders are there? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"total_amount\" (sum of amount, 2 decimals), \"count\" (number of such orders)."),
            ("J3", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: data/flags.json lists feature flags. Which flags have `enabled` set to the boolean false (not a string), and who owns each? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"disabled\" (array of [flag name, owner])."),
            ("J4", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: data/config.json is a nested service config. What is the value of `services.gateway.routes[3].timeout_ms` (0-based index), and what is the `name` of that route? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"timeout_ms\" (the timeout_ms value), \"route_name\" (the route name)."),
            ("J5", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: data/events.ndjson has one JSON event per line. How many distinct `user_id` values have at least one event whose `event` is exactly `login_failed`, and which user has the most `login_failed` events? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"distinct_users\" (number of distinct users), \"top_user\" (user_id with the most login_failed events)."),
            ("F1", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: For each terminal that is still working, summarise its last 50 lines of output in one or two sentences (errors, failures, counts). Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"working_terminals\" (names of all terminals that are still working), \"c1_errors\" (number of ERROR lines in the last 50 lines of c1), \"c1_timeouts\" (how many of those errors are timeouts), \"c2_passed\" (tests passed reported by c2), \"c2_failed\" (tests failed reported by c2), \"c2_failed_tests\" (names of the failed tests in c2)."),
            ("F2", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: One terminal printed a deploy token (TOKEN-...) somewhere in its output, possibly scrolled off the screen. Which terminal, in which folder, and what is the token? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"terminal\" (terminal name), \"folder\" (folder (cwd) of that terminal), \"token\" (the token)."),
            ("F3", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: Which terminals are waiting for me, and what is each one asking? Reply with a short list: terminal name, folder, kind of request, and the question. Do not answer or change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"waiting\" (array of [terminal name, kind] where kind is \"permission\" or \"question\"), \"b2_question\" (the question terminal b2 asks)."),
            ("F4", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "Task: Which terminals have exited with a non-zero exit code? For each give the exit code and the class of the last error it printed (for example `SyntaxError`). Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"exited_nonzero\" (array of [terminal name, exit code, error class])."),
            ("F5", false, false, false, "edit", None, "edit", "Task: Approve every pending permission prompt of the terminals in folder proj-a (and only there). Do not touch terminals in other folders. Then confirm what you approved.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"approved\" (names of the terminals you approved)."),
            ("G4", true, true, false, "code-read", None, "logs-data-general", "Task: Using this repository's git history, summarise the commits dated 2026-11-08 to 2026-11-10 (inclusive, commit dates in the commits' own +0530 time zone): how many commits are there, how many of them are releases (subject `chore: release ...`), and which commit changed the most files (short hash and subject)? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"commit_count\" (number of commits), \"release_count\" (number of release commits), \"most_files_commit\" (short hash of the commit that changed the most files)."),
            ("T2", true, true, false, "code-read", None, "logs-data-general", "Task: Using this repository's git history, which commit first added the Rust function `code_ms`? Give its short hash, its author date (YYYY-MM-DD) and its subject. Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"commit\" (short hash), \"date\" (author date YYYY-MM-DD), \"subject\" (commit subject)."),
            ("T3", true, true, false, "code-read", None, "logs-data-general", "Task: Between the git tags v0.1.1 and v0.1.15 of this repository, how many files changed, and which three files have the most changed lines (added plus deleted), most first? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"files_changed\" (number of files changed), \"top3\" (the three file paths, most changed lines first)."),
            ("T4", true, true, false, "code-read", None, "logs-data-general", "Task: In commit 88005e3 of this repository, which files were added (not modified)? Do not change anything.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"added\" (paths of the added files)."),
            ("E1", true, false, false, "edit", None, "edit", "Task: The `paginate(items, page, pageSize)` helper in this small Node project returns the wrong items: every page comes back one item short compared to what `pageSize` asks for, and the page tests in `test/paginate.test.js` fail. Find the cause in the source and fix it so pagination behaves correctly, including for the last page and for empty lists. Run the tests with `node --test`. Do not edit the existing test files.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"files_changed\" (paths of the files you changed)."),
            ("E2", false, false, false, "edit", None, "edit", "Task: In this project's `src/schedule.js`, schedules written like `every 2h at :15` start at the wrong minute: the parsed schedule runs at minute 1 of the hour instead of minute 15, and `every 3h at :45` and `every 4h at :30` are off as well. Find the cause and fix it without changing how other schedule strings, durations, zones or formats behave. Run the tests with `node --test`. Do not edit the existing test files.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"files_changed\" (paths of the files you changed)."),
            ("E3", false, false, false, "edit", None, "edit", "Task: Run the test suite verbosely (`python -m unittest discover -s tests -t . -v`); one test fails. Find it and fix the source code. Do not edit the existing test files.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"files_changed\" (paths of the files you changed)."),
            ("E4", true, false, true, "edit", None, "edit", "Task: The user lookup function `fetchUser` in this Node project has a misleading name, since it reads from the local table rather than fetching anything over the network. Rename it to `loadUser` everywhere in the project: its definition, every import and re-export, every call site, and any comments or docs that mention the old name. Behaviour must stay exactly the same. Run the tests with `node --test`. Do not edit the existing test files.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"files_changed\" (paths of the files you changed)."),
            ("E5", false, false, false, "edit", None, "edit", "Task: Add a `--json` flag to the `wc_tool` command line tool (`wc_tool/cli.py`). With `--json`, instead of the usual text rows, print exactly one line of compact JSON (separators `,` and `:` with no spaces, as produced by `json.dumps(obj, separators=(\",\", \":\"))`) followed by a single newline. The document has the keys, in this order, `files` and `total`. `files` is a list with one object per file in command-line order, each with keys in the order `path` (the path exactly as given), `lines`, `words`, `chars` (integers); `total` is an object with keys `lines`, `words`, `chars` and is present even for a single file. Without the flag the output must stay exactly as it is today, and a missing file must still print nothing to stdout and exit with code 1. Run the tests with `python -m unittest discover -s tests -t .`. Do not edit the existing test files.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"files_changed\" (paths of the files you changed)."),
            ("E6", false, false, false, "edit", None, "edit", "Task: In `config.json` of this project, make three changes and nothing else: set `services.payments.retry.max_attempts` to `5`, set `features.beta_search.enabled` to `true`, and set the top-level `logging.level` to `\"warn\"`. Other keys with the same names exist elsewhere in the file (for example under other services), so edit only the exact paths given, and keep the file's existing 2-space JSON formatting. Check the result with `node validate.js` and `node --test`. Do not edit the existing test files.\nBe concise. End with a line \"ANSWER:\" followed by one JSON object with exactly these keys: \"files_changed\" (paths of the files you changed)."),
            ("S1", false, false, false, "edit", None, "edit", "List all terminals. How many are there in total, and how many in each folder?"),
            ("S2", true, true, false, "code-read", None, "logs-data-general", "Read the whole file crates/h0x-core/src/bridge/web/tunnel.rs and give a short overview of it: its purpose, the names of all its public functions (including public methods), what `trusted` checks before cloudflared is started, and the reasons `must_stop` can return. Do not change anything."),
            ("S3", false, false, false, "edit", None, "edit", "Run the test suite (`python -m unittest discover -s tests -t .`). How many tests ran, and how many failed?"),
            ("S4", false, true, false, "logs-data-general", Some("haiku"), "logs-data-general", "The file build/test-run.log is the output of a long `cargo test` run. Which test failed? Give its full test path and the assertion values (left and right). Do not change anything."),
        ];
        let r = repo();
        let p = plain();
        for (id, code, read_only, nav, kind, model, remote_kind, prompt) in rows {
            let d = go(Some(if code { r.path() } else { p.path() }), prompt);
            assert_eq!(d.kind, kind, "{id}");
            assert_eq!((d.code, d.read_only, d.nav), (code, read_only, nav), "{id}");
            assert_eq!(d.graft, kind == "code-nav", "{id}");
            assert_eq!(d.model.as_deref(), model, "{id}");
            assert_eq!(d.tools, if read_only { &READ_ONLY_TOOLS[..] } else { &EDIT_TOOLS[..] }, "{id}");
            let remote = go(None, prompt);
            assert_eq!(remote.kind, remote_kind, "{id} without a cwd");
            assert!(!remote.graft && remote.model.is_none(), "{id} without a cwd");
        }
    }

    #[test]
    fn three_csharp_files_make_a_repo_as_in_the_prototype() {
        // router7.js: SRC = /\.(rs|js|ts|tsx|py|go|java|kt|c|cc|cpp|h|rb|cs)$/
        let d = plain();
        touch(d.path(), &["a.cs", "b.cs", "c.cs"]);
        let r = go(Some(d.path()), RO);
        assert!(r.code);
        assert_eq!(r.kind, "code-read");
        assert_eq!(r.model, None);
    }

    #[test]
    fn unicode_prompts_match_only_the_ascii_phrases() {
        // Dotless i (U+0131) in either case is not an `i`.
        for p in ["do not change anyth\u{131}ng", "DO NOT CHANGE ANYTH\u{131}NG", "DON'T CHANGE ANYTH\u{131}NG"] {
            assert!(!go(None, p).read_only, "{p}");
        }
        // Full-width letters are other letters.
        for p in [
            "\u{ff52}\u{ff45}\u{ff41}\u{ff44}-\u{ff4f}\u{ff4e}\u{ff4c}\u{ff59}",
            "\u{ff32}\u{ff25}\u{ff21}\u{ff24}-ONLY",
        ] {
            assert!(!go(None, p).read_only, "{p}");
        }
        // A no-break space is not the space of the phrase.
        assert!(!go(None, "do not\u{a0}change anything").read_only);
        // Emoji and a combining mark next to a phrase are word boundaries.
        let d = go(None, "\u{1f600} read-only \u{1f600} who calls x\u{1f600}");
        assert!(d.read_only && d.nav);
        assert!(go(None, "read-only\u{301}").read_only);
        // `_` and digits are word characters.
        assert!(!go(None, "_read-only").read_only);
        assert!(!go(None, "1read-only").read_only);
        // Letters whose lower case is longer than one char never panic or shift the match.
        for p in ["\u{130}\u{130}\u{130} read-only who calls", "read-only \u{130} who calls \u{1e9e}\u{fb00}"] {
            let d = go(None, p);
            assert!(d.read_only && d.nav, "{p}");
        }
    }

    #[test]
    fn a_nav_gap_counts_characters_not_bytes() {
        // 29 four-byte emoji + 2 spaces = 31 chars, well inside the 60 limit.
        let d = go(None, &format!("{RO} where is {} defined", "\u{1f600}".repeat(29)));
        assert!(d.nav);
        let d = go(None, &format!("{RO} where is {} defined", "\u{e9}".repeat(58)));
        assert!(d.nav);
        assert!(!go(None, &format!("{RO} where is {} defined", "\u{e9}".repeat(59))).nav);
    }

    #[test]
    fn a_symlinked_cwd_is_read_through_its_link() {
        let r = repo();
        let p = plain();
        let link = p.path().join("link");
        let gone = p.path().join("dangling");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(r.path(), &link)
            .and_then(|()| std::os::unix::fs::symlink(p.path().join("missing"), &gone));
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(r.path(), &link)
            .and_then(|()| std::os::windows::fs::symlink_dir(p.path().join("missing"), &gone));
        if let Err(e) = made {
            // Windows without developer mode cannot make symlinks.
            eprintln!("no symlinks here: {e}");
            return;
        }
        let d = go(Some(&link), &format!("{RO} Who calls main?"));
        assert!(d.code && d.graft);
        assert_eq!(d.kind, "code-nav");
        let d = go(Some(&gone), &format!("{RO} Who calls main?"));
        assert!(!d.code && !d.graft);
        edit_profile(&go(Some(&gone), "Fix it"));
    }

    #[test]
    fn a_cwd_that_is_a_file_is_not_a_repo_even_with_a_code_name() {
        let p = plain();
        touch(p.path(), &["src", "main.rs", "a.rs", "b.rs"]);
        for name in ["src", "main.rs"] {
            let d = go(Some(&p.path().join(name)), &format!("{RO} Who calls main?"));
            assert!(!d.code && !d.graft, "{name}");
            assert_eq!(d.kind, "logs-data-general", "{name}");
        }
        // A file named like a source folder adds nothing: two sources stay two.
        let q = plain();
        touch(q.path(), &["src", "a.rs", "b.rs"]);
        assert!(!go(Some(q.path()), "x").code);
    }

    #[test]
    fn source_files_with_non_utf8_names_still_count() {
        let d = plain();
        let mut made = 0;
        for stem in ["a", "b", "c"] {
            #[cfg(unix)]
            let name: std::ffi::OsString = {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec([stem.as_bytes(), b"\xff.rs"].concat())
            };
            #[cfg(windows)]
            let name: std::ffi::OsString = {
                use std::os::windows::ffi::OsStringExt;
                let wide: Vec<u16> = stem.encode_utf16().chain([0xD800]).chain(".rs".encode_utf16()).collect();
                std::ffi::OsString::from_wide(&wide)
            };
            if std::fs::write(d.path().join(name), "").is_ok() {
                made += 1;
            }
        }
        if made < 3 {
            // Some file systems (APFS) refuse names that are not UTF-8.
            eprintln!("only {made} non-UTF-8 names could be made");
            return;
        }
        assert!(go(Some(d.path()), "x").code);
    }
}
