//! `h0x-core gate` as Claude Code runs it: the hook event on stdin, the answer on stdout, exit 0.
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// A short folder of our own for one test: the socket path limit is small on unix.
fn scratch(name: &str) -> std::path::PathBuf {
    let base = if cfg!(unix) { "/var/tmp".into() } else { std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")) };
    let root = base.join(format!("h0x-gt-{}-{name}", std::process::id()));
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

/// The counter file of session `name`.
fn counter(name: &str) -> std::path::PathBuf {
    scratch(name).join("state").join("h0x-core").join(name).join("token-saver.json")
}

/// Stops the session's core when a test ends, however it ends.
struct Core(String);

impl Drop for Core {
    fn drop(&mut self) {
        let _ = core_cmd(&self.0, &["server", "stop"]).output();
        let _ = std::fs::remove_dir_all(scratch(&self.0));
    }
}

/// The gate with `input` on stdin, in an isolated home and session.
fn gate(input: &[u8]) -> Output {
    gate_as("gate-test", input, &[])
}

fn gate_as(name: &str, input: &[u8], env: &[(&str, &str)]) -> Output {
    let mut child = core_cmd(name, &["gate"])
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let feed = std::thread::spawn(move || drop(stdin.write_all(&input)));
    let out = child.wait_with_output().unwrap();
    feed.join().unwrap();
    out
}

fn quiet(out: &Output, what: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{what}: {stderr}");
    assert!(!stderr.contains("panicked"), "{what}: {stderr}");
}

#[test]
fn a_big_bash_output_is_replaced_on_stdout() {
    let stdout: String = (0..2000).map(|i| format!("line {i} copied ok\r\n")).collect();
    let ev = json!({"tool_name": "Bash", "tool_input": {"command": "x"}, "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false}});
    let out = gate(ev.to_string().as_bytes());
    quiet(&out, "bash");
    let v: Value = serde_json::from_slice(&out.stdout).expect("one JSON object on stdout");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let u = &v["hookSpecificOutput"]["updatedToolOutput"];
    assert_eq!(u["interrupted"], false);
    let text = u["stdout"].as_str().unwrap();
    assert!(text.starts_with("[HyperOx Token Saver: output "), "{text}");
    assert!(text.ends_with("\nline 0 copied ok  [x2000 alike]"), "{text}");
}

#[test]
fn a_big_read_from_a_windows_path_with_spaces_is_an_outline() {
    let src: String =
        (0..400).map(|i| format!("def handler_{i}(event):\r\n    return event  # padding padding\r\n\r\n")).collect();
    let path = "C:\\Users\\Jane Doe\\My Project\\app.PY";
    let ev = json!({"tool_name": "Read", "tool_input": {"file_path": path}, "tool_response": {"type": "text", "file": {"filePath": path, "content": src, "numLines": 1200, "startLine": 1, "totalLines": 1200}}});
    let out = gate(ev.to_string().as_bytes());
    quiet(&out, "read");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let file = &v["hookSpecificOutput"]["updatedToolOutput"]["file"];
    let content = file["content"].as_str().unwrap();
    assert!(content.contains("\nL1: def handler_0(event):\nL4: def handler_1(event):"), "{content}");
    assert_eq!(file["filePath"], path);
    assert_eq!(file["startLine"], 1);
    assert_eq!(file["numLines"], content.split('\n').count());
}

#[test]
fn nothing_to_do_prints_nothing_and_exits_0() {
    let big = "x y z\n".repeat(5000);
    let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty stdin", Vec::new()),
        ("not json", b"not json".to_vec()),
        ("invalid utf-8", vec![b'{', 0xff, 0xfe, b'}']),
        ("small output", json!({"tool_name": "Bash", "tool_response": {"stdout": "hi"}}).to_string().into_bytes()),
        ("other tool", json!({"tool_name": "Write", "tool_response": {"stdout": big}}).to_string().into_bytes()),
        ("null response", json!({"tool_name": "Bash", "tool_response": null}).to_string().into_bytes()),
        ("too deep", format!("{{\"tool_name\":\"Bash\",\"tool_response\":{deep}}}").into_bytes()),
    ];
    for (what, input) in cases {
        let out = gate(&input);
        quiet(&out, what);
        assert!(out.stdout.is_empty(), "{what}: {}", String::from_utf8_lossy(&out.stdout));
    }
}

fn big_event() -> String {
    let stdout: String = (0..3000).map(|i| format!("step {i} finished fine\n")).collect();
    json!({"tool_name": "Bash", "tool_input": {}, "tool_response": {"stdout": stdout, "stderr": ""}}).to_string()
}

/// Tokens the gate saved on one `big_event`, from what it printed.
fn saved_by(out: &Output) -> u64 {
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let shown = v["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str().unwrap().len();
    let bytes_in: usize = (0..3000).map(|i| format!("step {i} finished fine\n").len()).sum();
    ((bytes_in - shown) / 4) as u64
}

fn stats(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(counter(name)).unwrap()).unwrap()
}

#[test]
fn the_counter_adds_up_over_replaced_outputs_and_ignores_pass_throughs() {
    let name = "gt-count";
    let _core = Core(name.into());
    assert!(!counter(name).exists(), "nothing is booked before the first output");
    let small = json!({"tool_name": "Bash", "tool_response": {"stdout": "hi"}}).to_string();
    quiet(&gate_as(name, small.as_bytes(), &[]), "small");
    assert!(!counter(name).exists(), "a pass-through books nothing");
    let first = gate_as(name, big_event().as_bytes(), &[]);
    quiet(&first, "first");
    let one = stats(name);
    assert_eq!(one["outputs"], 1);
    assert_eq!(one["tokens_saved"], saved_by(&first));
    assert!(one["bytes_in"].as_u64().unwrap() > one["bytes_shown"].as_u64().unwrap());
    quiet(&gate_as(name, small.as_bytes(), &[]), "small again");
    quiet(&gate_as(name, big_event().as_bytes(), &[]), "second");
    let two = stats(name);
    assert_eq!(two["outputs"], 2);
    for key in ["tokens_saved", "bytes_in", "bytes_shown"] {
        assert_eq!(two[key].as_u64().unwrap(), 2 * one[key].as_u64().unwrap(), "{key}");
    }
}

#[test]
fn a_corrupt_or_odd_counter_reads_as_zero_and_is_rewritten() {
    let name = "gt-corrupt";
    let _core = Core(name.into());
    let cases: [(&str, &[u8]); 4] =
        [("garbage", b"\xff not json"), ("wrong type", b"[1,2]"), ("negative", b"{\"outputs\": -4}"), ("empty", b"")];
    for (what, junk) in cases {
        std::fs::create_dir_all(counter(name).parent().unwrap()).unwrap();
        std::fs::write(counter(name), junk).unwrap();
        quiet(&gate_as(name, big_event().as_bytes(), &[]), what);
        assert_eq!(stats(name)["outputs"], 1, "{what}: restarted from zero");
    }
}

#[test]
fn a_missing_terminal_or_no_core_leaves_the_answer_and_exit_code_alone() {
    let name = "gt-missing";
    let _core = Core(name.into());
    // No core listens: the gate must not start one.
    let out = gate_as(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", "t99")]);
    quiet(&out, "no core");
    assert_eq!(stats(name)["tokens_saved"], saved_by(&out));
    // A core, but no such terminal.
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let out = gate_as(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", "t99")]);
    quiet(&out, "missing terminal");
    assert_eq!(stats(name)["outputs"], 2);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
}

#[test]
fn a_terminals_saved_label_is_a_running_total_and_gate_stats_reports_the_counter() {
    let name = "gt-label";
    let _core = Core(name.into());
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let rpc = |method: &str| -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {}}).to_string() + "\n";
        let mut child = core_cmd(name, &["connect"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        serde_json::from_str::<Value>(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))["result"].clone()
    };
    assert_eq!(rpc("gate.stats"), json!({"tokens_saved": 0, "outputs": 0, "bytes_in": 0, "bytes_shown": 0}));
    let program: &[&str] = if cfg!(windows) { &["cmd", "/c", "ping -n 60 127.0.0.1 >nul"] } else { &["sleep", "60"] };
    let started = core_cmd(name, &[&["--json", "run", "--bg"], program].concat()).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let address = serde_json::from_slice::<Value>(&started.stdout).unwrap()["address"].as_str().unwrap().to_string();
    let label = || {
        let out = core_cmd(name, &["--json", "get", &address]).output().unwrap();
        let t: Value = serde_json::from_slice(&out.stdout).unwrap();
        t["labels"]["h0x.saved"].as_str().map(str::to_string)
    };
    assert_eq!(label(), None);
    let one = gate_as(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", &address)]);
    quiet(&one, "first");
    assert_eq!(label(), Some(saved_by(&one).to_string()));
    let two = gate_as(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", &address)]);
    quiet(&two, "second");
    assert_eq!(label(), Some((saved_by(&one) + saved_by(&two)).to_string()), "a running total");
    let s = rpc("gate.stats");
    assert_eq!(s["outputs"], 2);
    assert_eq!(s["tokens_saved"].as_u64().unwrap(), saved_by(&one) + saved_by(&two));
}

#[test]
fn a_counter_at_the_u64_limit_saturates_and_one_past_it_restarts_from_zero() {
    let name = "gt-max";
    let _core = Core(name.into());
    let max = u64::MAX;
    std::fs::create_dir_all(counter(name).parent().unwrap()).unwrap();
    let top = json!({"tokens_saved": max - 1, "outputs": max, "bytes_in": max - 2, "bytes_shown": max});
    std::fs::write(counter(name), top.to_string()).unwrap();
    quiet(&gate_as(name, big_event().as_bytes(), &[]), "at the top");
    assert_eq!(stats(name), json!({"tokens_saved": max, "outputs": max, "bytes_in": max, "bytes_shown": max}));
    // 2^64 is no u64: the file is broken, so it counts again from zero.
    std::fs::write(counter(name), b"{\"outputs\": 18446744073709551616, \"tokens_saved\": 5}").unwrap();
    let out = gate_as(name, big_event().as_bytes(), &[]);
    quiet(&out, "past the top");
    assert_eq!(stats(name)["outputs"], 1);
    assert_eq!(stats(name)["tokens_saved"], saved_by(&out));
}

#[test]
fn a_state_folder_that_cannot_be_made_still_gets_the_whole_answer_and_exit_0() {
    let name = "gt-nostate";
    let _core = Core(name.into());
    // The state home's parent is a file: no folder can be made under it.
    let blocker = scratch(name).join("not a folder");
    std::fs::write(&blocker, b"x").unwrap();
    let state = blocker.join("state");
    let blocked = gate_as(name, big_event().as_bytes(), &[("XDG_STATE_HOME", state.to_str().unwrap())]);
    quiet(&blocked, "blocked");
    assert!(std::fs::metadata(&blocker).unwrap().is_file(), "nothing was written there");
    let normal = gate_as(name, big_event().as_bytes(), &[]);
    quiet(&normal, "normal");
    assert!(!blocked.stdout.is_empty());
    assert_eq!(blocked.stdout, normal.stdout, "the answer does not depend on the bookkeeping");
    assert_eq!(stats(name)["outputs"], 1, "only the normal run was booked");
}

#[test]
fn a_session_and_state_path_with_spaces_book_the_counter_there() {
    let name = "gt spaced";
    let _core = Core(name.into());
    let state = scratch(name).join("Program Data").join("h0x state");
    let out = gate_as(name, big_event().as_bytes(), &[("XDG_STATE_HOME", state.to_str().unwrap())]);
    quiet(&out, "spaces");
    let file = state.join("h0x-core").join(name).join("token-saver.json");
    let s: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(s["outputs"], 1);
    assert_eq!(s["tokens_saved"], saved_by(&out));
    assert!(!counter(name).exists(), "the default state home was left alone");
}

/// Starts session `name`'s core with one long-lived terminal and returns its address.
fn core_with_terminal(name: &str) -> String {
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let program: &[&str] = if cfg!(windows) { &["cmd", "/c", "ping -n 60 127.0.0.1 >nul"] } else { &["sleep", "60"] };
    let started = core_cmd(name, &[&["--json", "run", "--bg"], program].concat()).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    serde_json::from_slice::<Value>(&started.stdout).unwrap()["address"].as_str().unwrap().to_string()
}

/// The `h0x.saved` label of `address` in session `name`.
fn saved_label(name: &str, address: &str) -> Option<String> {
    let out = core_cmd(name, &["--json", "get", address]).output().unwrap();
    let t: Value = serde_json::from_slice(&out.stdout).unwrap();
    t["labels"]["h0x.saved"].as_str().map(str::to_string)
}

#[test]
fn a_saved_label_that_is_not_a_count_is_overwritten_and_the_top_stays_the_top() {
    let name = "gt-badlabel";
    let _core = Core(name.into());
    let address = core_with_terminal(name);
    for junk in ["abc", "-5", "1e3", "", "12abc"] {
        let set = core_cmd(name, &["label", &address, &format!("h0x.saved={junk}")]).output().unwrap();
        assert!(set.status.success(), "{junk:?}: {}", String::from_utf8_lossy(&set.stderr));
        let out = gate_as(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", &address)]);
        quiet(&out, junk);
        assert_eq!(saved_label(name, &address), Some(saved_by(&out).to_string()), "{junk:?} counts as 0");
    }
    let top = u64::MAX.to_string();
    core_cmd(name, &["label", &address, &format!("h0x.saved={top}")]).output().unwrap();
    quiet(&gate_as(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", &address)]), "top");
    assert_eq!(saved_label(name, &address), Some(top), "saturates, no wrap");
}

#[test]
fn an_empty_terminal_id_labels_nothing_but_still_counts() {
    let name = "gt-emptyid";
    let _core = Core(name.into());
    let address = core_with_terminal(name);
    let out = gate_as(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", "")]);
    quiet(&out, "empty id");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert_eq!(stats(name)["outputs"], 1);
    assert_eq!(saved_label(name, &address), None, "no terminal was guessed");
}

#[test]
fn a_core_that_never_answers_holds_the_gate_well_under_a_second_after_the_answer() {
    let name = "gt-hang";
    let _core = Core(name.into());
    let socket = if cfg!(windows) {
        std::path::PathBuf::from(format!(r"\\.\pipe\h0x-gt-{}-hang", std::process::id()))
    } else {
        scratch(name).join("hang.sock")
    };
    // Accepts, reads the request, never answers and never hangs up.
    let listener = h0x_core::platform::listen(&socket).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(&mut std::io::BufReader::new(&stream), &mut line);
            let _ = tx.send(line);
            std::mem::forget(stream);
        }
    });
    let mut child = core_cmd(name, &["gate"])
        .env("H0X_CORE_SOCKET", &socket)
        .env("H0X_TERMINAL_ID", "t1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    child.stdin.take().unwrap().write_all(big_event().as_bytes()).unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut got = Vec::new();
    let answer: Value = loop {
        let mut buf = [0u8; 4096];
        let n = std::io::Read::read(&mut stdout, &mut buf).unwrap();
        assert!(n > 0, "stdout closed before a whole answer: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
        if let Ok(v) = serde_json::from_slice(&got) {
            break v;
        }
    };
    assert!(child.try_wait().unwrap().is_none(), "the answer is out before the label update gives up");
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            panic!("the gate waited on a silent core");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let took = started.elapsed();
    assert_eq!(status.code(), Some(0));
    assert!(took < std::time::Duration::from_secs(1), "took {took:?}");
    let request: Value = serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(request["method"], "terminal.get", "the fake core was really asked");
    assert_eq!(answer["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert_eq!(stats(name)["outputs"], 1);
}

#[test]
fn gate_stats_reads_a_broken_counter_as_zeros_not_an_error() {
    let name = "gt-rpcbroken";
    let _core = Core(name.into());
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let reply = || -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "gate.stats", "params": {}}).to_string() + "\n";
        let mut child = core_cmd(name, &["connect"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    };
    let zeros = json!({"tokens_saved": 0, "outputs": 0, "bytes_in": 0, "bytes_shown": 0});
    let file = counter(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let cases: [(&str, &[u8]); 5] = [
        ("garbage", b"{\"tokens_saved\": 12,"),
        ("not utf-8", b"\xff\xfe"),
        ("an array", b"[1, 2]"),
        ("a string count", b"{\"outputs\": \"7\"}"),
        ("empty", b""),
    ];
    for (what, junk) in cases {
        std::fs::write(&file, junk).unwrap();
        let r = reply();
        assert!(r.get("error").is_none(), "{what}: {r}");
        assert_eq!(r["result"], zeros, "{what}");
    }
    // A folder where the file should be.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert_eq!(reply()["result"], zeros, "a folder");
    std::fs::remove_dir(&file).unwrap();
    // Read live, and a missing field is 0.
    std::fs::write(&file, b"{\"outputs\": 3}").unwrap();
    assert_eq!(reply()["result"], json!({"tokens_saved": 0, "outputs": 3, "bytes_in": 0, "bytes_shown": 0}));
}

#[test]
fn the_schema_has_gate_stats_with_its_four_counters_and_no_params() {
    let schema = h0x_core::api::openrpc();
    let methods = schema["methods"].as_array().unwrap();
    let method = methods.iter().find(|m| m["name"] == "gate.stats").expect("gate.stats in the schema");
    assert_eq!(method["params"], json!([]));
    let reference = method["result"]["schema"]["$ref"].as_str().unwrap();
    let def = reference.strip_prefix("#/components/schemas/").unwrap();
    let props = schema["components"]["schemas"][def]["properties"].as_object().unwrap();
    let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["bytes_in", "bytes_shown", "outputs", "tokens_saved"]);
    for (key, prop) in props {
        assert_eq!(prop["type"], "integer", "{key}");
        assert_eq!(prop["minimum"], 0, "{key}");
    }
}

#[test]
fn racing_gates_never_leave_a_torn_counter() {
    let name = "gt-race";
    let _core = Core(name.into());
    let event = big_event();
    let runs: Vec<_> = (0..8)
        .map(|_| {
            let event = event.clone();
            std::thread::spawn(move || gate_as(name, event.as_bytes(), &[]))
        })
        .collect();
    for run in runs {
        quiet(&run.join().unwrap(), "racing gate");
    }
    let s = stats(name);
    let outputs = s["outputs"].as_u64().unwrap();
    assert!((1..=8).contains(&outputs), "{s}");
    let left: Vec<_> = std::fs::read_dir(counter(name).parent().unwrap()).unwrap().flatten().collect();
    assert!(left.iter().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")), "temp files left: {left:?}");
}
