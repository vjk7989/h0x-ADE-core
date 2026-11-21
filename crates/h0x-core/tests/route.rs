//! `h0x-core route`: `{cwd?, prompt}` on stdin, one JSON line on stdout, exit 0; and the RPC method `route`.
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Output, Stdio};

const RO_NAV: &str = "Do not change anything. Who calls main?";

/// A short folder of our own for one test: the socket path limit is small on unix.
fn scratch(name: &str) -> std::path::PathBuf {
    let base = if cfg!(unix) { "/var/tmp".into() } else { std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")) };
    let root = base.join(format!("h0x-rt-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// `h0x-core <args>` in an isolated home and session named `name`.
fn core_cmd(name: &str, args: &[&str]) -> Command {
    let root = scratch(name);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_h0x-core"));
    cmd.args(args)
        .env("H0X_CORE_SESSION", name)
        .env("H0X_CORE_SETTLE_MS", "0")
        .env("HOME", &root)
        .env("USERPROFILE", &root)
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("H0X_TERMINAL_ID")
        .env_remove("H0X_TERMINAL_SOCKET")
        .env_remove("H0X_CORE_SOCKET");
    cmd
}

/// `h0x-core route` with `input` on stdin; checks exit 0, no panic and exactly one JSON line.
fn route(input: &[u8]) -> Value {
    let mut child = core_cmd("route-cli", &["route"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let feed = std::thread::spawn(move || drop(stdin.write_all(&input)));
    let out: Output = child.wait_with_output().unwrap();
    feed.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout.strip_suffix('\n').unwrap_or_else(|| panic!("no newline at the end: {stdout:?}"));
    assert!(!line.contains('\n'), "more than one line: {stdout:?}");
    let v: Value = serde_json::from_str(line.trim_end_matches('\r')).unwrap_or_else(|e| panic!("{e}: {line:?}"));
    for t in v["tools"].as_array().unwrap() {
        let t = t.as_str().unwrap();
        assert!(!t.is_empty() && !t.contains(','), "{t:?}");
    }
    v
}

fn edit_profile() -> Value {
    json!({"kind": "edit", "code": false, "read_only": false, "nav": false, "graft": false,
        "tools": ["Bash", "Read", "Grep", "Edit", "Write"], "model": null})
}

/// This crate's folder: it has a Cargo.toml, so it is a code repo.
fn repo_dir() -> String {
    env!("CARGO_MANIFEST_DIR").into()
}

#[test]
fn a_valid_request_is_one_json_line_with_the_decision() {
    let v = route(json!({"cwd": repo_dir(), "prompt": RO_NAV}).to_string().as_bytes());
    assert_eq!(
        v,
        json!({"kind": "code-nav", "code": true, "read_only": true, "nav": true, "graft": true,
            "tools": ["Bash", "Read", "Grep"], "model": null})
    );
    // A plain folder with a read-only prompt: the cheap model.
    let plain = scratch("route-plain");
    let v = route(json!({"cwd": plain, "prompt": "Do not change anything. Count the errors."}).to_string().as_bytes());
    assert_eq!(v["kind"], "logs-data-general");
    assert_eq!(v["model"], "haiku");
    // No cwd (a remote machine): no code map and no model.
    let v = route(json!({"prompt": RO_NAV}).to_string().as_bytes());
    assert_eq!(
        (v["kind"].as_str(), v["graft"].as_bool(), &v["model"]),
        (Some("logs-data-general"), Some(false), &Value::Null)
    );
    // `cwd: null` is the same as no cwd.
    assert_eq!(route(json!({"cwd": null, "prompt": RO_NAV}).to_string().as_bytes()), v);
}

#[test]
fn a_windows_cwd_with_spaces_and_a_trailing_backslash_is_read() {
    let dir = scratch("route win dir");
    std::fs::write(dir.join("go.mod"), "").unwrap();
    let mut cwd = dir.to_string_lossy().into_owned();
    if cfg!(windows) {
        cwd.push('\\');
    }
    let v = route(json!({"cwd": cwd, "prompt": RO_NAV}).to_string().as_bytes());
    assert_eq!(v["kind"], "code-nav", "{cwd}");
}

#[test]
fn bad_or_empty_input_is_the_edit_profile() {
    let cwd = repo_dir();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty stdin", b"".to_vec()),
        ("whitespace", b" \r\n\t".to_vec()),
        ("garbage", b"{\"prompt\": ".to_vec()),
        ("not utf-8", b"\xff\xfe{\"prompt\": \"read-only\"}".to_vec()),
        ("null", b"null".to_vec()),
        ("an array", b"[\"read-only\"]".to_vec()),
        ("a string", b"\"read-only\"".to_vec()),
        ("a number prompt", json!({"cwd": cwd, "prompt": 5}).to_string().into_bytes()),
        ("a null prompt", json!({"cwd": cwd, "prompt": null}).to_string().into_bytes()),
        ("a number cwd", json!({"cwd": 5, "prompt": RO_NAV}).to_string().into_bytes()),
        ("a lone surrogate", b"{\"cwd\": \"\\ud800\", \"prompt\": \"read-only\"}".to_vec()),
        ("trailing junk", format!("{} x", json!({"cwd": cwd, "prompt": RO_NAV})).into_bytes()),
        ("two objects", format!("{0}\n{0}", json!({"cwd": cwd, "prompt": RO_NAV})).into_bytes()),
        ("an empty object", b"{}".to_vec()),
        ("an empty prompt in a repo", json!({"cwd": cwd, "prompt": ""}).to_string().into_bytes()),
    ];
    for (what, input) in cases {
        let v = route(&input);
        let mut want = edit_profile();
        if what == "an empty prompt in a repo" {
            want["code"] = json!(true);
        }
        assert_eq!(v, want, "{what}");
    }
}

#[test]
fn a_missing_cwd_or_a_file_as_cwd_is_no_repo() {
    let dir = scratch("route-missing");
    std::fs::write(dir.join("Cargo.toml"), "").unwrap();
    for cwd in [dir.join("nope"), dir.join("Cargo.toml")] {
        let v = route(json!({"cwd": cwd, "prompt": "Fix the bug."}).to_string().as_bytes());
        assert_eq!(v, edit_profile(), "{}", cwd.display());
        let v = route(json!({"cwd": cwd, "prompt": RO_NAV}).to_string().as_bytes());
        assert_eq!((v["code"].as_bool(), v["graft"].as_bool()), (Some(false), Some(false)), "{}", cwd.display());
    }
}

#[test]
fn a_one_megabyte_prompt_is_classified_and_answered_in_one_line() {
    let mut prompt = "lorem ipsum dolor sit amet ".repeat((1 << 20) / 27 + 1);
    prompt.push_str(RO_NAV);
    assert!(prompt.len() > 1 << 20);
    let v = route(json!({"cwd": repo_dir(), "prompt": prompt}).to_string().as_bytes());
    assert_eq!(v["kind"], "code-nav");
    // Over a megabyte of plain text: still the edit profile, still one line.
    let v = route(json!({"prompt": "x ".repeat(1 << 20)}).to_string().as_bytes());
    assert_eq!(v, edit_profile());
}

#[test]
fn a_megabyte_of_navigation_words_answers_in_seconds() {
    // No `where … handled` pair matches (a `.` sits in every gap), so a pairwise search tries every
    // `where` against every `handled`; the regex it ports does not.
    let prompt = format!("Do not change anything. {}", "where. handled ".repeat((1 << 20) / 15));
    let started = std::time::Instant::now();
    let v = route(json!({"prompt": prompt}).to_string().as_bytes());
    assert_eq!((v["read_only"].as_bool(), v["nav"].as_bool()), (Some(true), Some(false)));
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
}

/// Stops the session's core when a test ends, however it ends.
struct Core(String);

impl Drop for Core {
    fn drop(&mut self) {
        let _ = core_cmd(&self.0, &["server", "stop"]).output();
        let _ = std::fs::remove_dir_all(scratch(&self.0));
    }
}

#[test]
fn the_route_rpc_method_answers_like_the_command() {
    let name = "rt-rpc";
    let _core = Core(name.into());
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let call = |params: Value| -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "route", "params": params}).to_string() + "\n";
        let mut child = core_cmd(name, &["connect"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    };
    let cases = [
        json!({"cwd": repo_dir(), "prompt": RO_NAV}),
        json!({"prompt": RO_NAV}),
        json!({"cwd": scratch("rt-rpc-plain"), "prompt": "Do not change anything."}),
        json!({"cwd": repo_dir(), "prompt": "Fix the bug."}),
    ];
    for params in cases {
        assert_eq!(call(params.clone()), route(params.to_string().as_bytes()), "{params}");
    }
    assert_eq!(call(json!({})), edit_profile());
    assert_eq!(call(Value::Null), edit_profile());
}

#[test]
fn the_schema_has_route_with_an_optional_cwd_and_the_seven_result_keys() {
    let schema = h0x_core::api::openrpc();
    let methods = schema["methods"].as_array().unwrap();
    let method = methods.iter().find(|m| m["name"] == "route").expect("route in the schema");
    let params = method["params"].as_array().unwrap();
    let names: Vec<&str> = params.iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(names.contains(&"cwd") && names.contains(&"prompt"), "{params:?}");
    let cwd = params.iter().find(|p| p["name"] == "cwd").unwrap();
    assert_ne!(cwd["required"], true, "{cwd}");
    let reference = method["result"]["schema"]["$ref"].as_str().unwrap();
    let def = reference.strip_prefix("#/components/schemas/").unwrap();
    let props = schema["components"]["schemas"][def]["properties"].as_object().unwrap();
    let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["code", "graft", "kind", "model", "nav", "read_only", "tools"]);
}
