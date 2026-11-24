//! `h0x-core gate` as Claude Code runs it: the hook event on stdin, the answer on stdout, exit 0.
//!
//! Since SC-2 the gate answers only inside a HyperOx terminal: the tests that expect an answer run it
//! in a terminal of their own core (`sh` on unix, `cmd` on Windows via `tests/inside`), and the ones
//! that run it outside assert it stays silent.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]
#[cfg(windows)]
mod inside;

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

/// The gate in a HyperOx terminal of session "gate-test".
#[cfg(unix)]
fn gate(input: &[u8]) -> Output {
    gate_as("gate-test", input, &[])
}

/// The gate in a HyperOx terminal of a core of its own, stopped after.
#[cfg(windows)]
fn gate(input: &[u8]) -> Output {
    inside::Session::new().in_terminal(&["gate"], input, None, &[])
}

/// The gate inside a terminal of session `name`'s core (started if needed), with `env` set first.
#[cfg(windows)]
fn gate_as(name: &str, input: &[u8], env: &[(&str, &str)]) -> Output {
    inside::in_terminal(&|a| core_cmd(name, a), &scratch(name), &["gate"], input, None, env)
}

/// The gate run inside a terminal of session `name`'s core (started if needed), with `env`
/// exported first. The terminal waits for `finish`, so a test can touch it before the gate runs.
#[cfg(unix)]
struct Inside {
    address: String,
    files: Box<dyn Fn(&str) -> std::path::PathBuf>,
    runs: usize,
}

#[cfg(unix)]
fn gate_start(name: &str, inputs: &[&[u8]], env: &[(&str, &str)]) -> Inside {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let dir = scratch(name);
    let tag = N.fetch_add(1, Ordering::Relaxed);
    let files = move |what: &str| dir.join(format!("g{tag}-{what}"));
    let q = |what: &str| format!("'{}'", files(what).display());
    let mut body = format!("while [ ! -e {} ]; do sleep 0.02; done; ", q("go"));
    body += &env.iter().map(|(k, v)| format!("export {k}='{v}'; ")).collect::<String>();
    for (i, input) in inputs.iter().enumerate() {
        std::fs::write(files(&format!("in{i}")), input).unwrap();
        body += &format!(
            "'{}' gate < {} > {} 2> {}; echo $? > {}; ",
            env!("CARGO_BIN_EXE_h0x-core"),
            q(&format!("in{i}")),
            q(&format!("out{i}")),
            q(&format!("err{i}")),
            q(&format!("code{i}"))
        );
    }
    body += &format!(": > {}; sleep 60", q("done"));
    let started = core_cmd(name, &["--json", "run", "--bg", "sh", "-c", &body]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let address = serde_json::from_slice::<Value>(&started.stdout).unwrap()["address"].as_str().unwrap().to_string();
    Inside { address, files: Box::new(files), runs: inputs.len() }
}

#[cfg(unix)]
impl Inside {
    /// Lets the gate run and returns what each run printed.
    fn finish(self) -> Vec<Output> {
        use std::os::unix::process::ExitStatusExt;
        std::fs::write((self.files)("go"), "").unwrap();
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !(self.files)("done").exists() {
            assert!(std::time::Instant::now() < limit, "the gate never finished in its terminal");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        (0..self.runs)
            .map(|i| {
                let read = |what: &str| std::fs::read((self.files)(&format!("{what}{i}"))).unwrap();
                let code: i32 = String::from_utf8(read("code")).unwrap().trim().parse().unwrap();
                Output {
                    status: std::process::ExitStatus::from_raw(code << 8),
                    stdout: read("out"),
                    stderr: read("err"),
                }
            })
            .collect()
    }
}

/// The gate inside a terminal, once per input, in order: the terminal's address and the outputs.
#[cfg(unix)]
fn gate_runs(name: &str, inputs: &[&[u8]], env: &[(&str, &str)]) -> (String, Vec<Output>) {
    let inside = gate_start(name, inputs, env);
    let address = inside.address.clone();
    (address, inside.finish())
}

#[cfg(unix)]
fn gate_as(name: &str, input: &[u8], env: &[(&str, &str)]) -> Output {
    gate_runs(name, &[input], env).1.remove(0)
}

/// The gate with `input` on stdin, outside any terminal, in an isolated home and session.
fn gate_outside(name: &str, input: &[u8], env: &[(&str, &str)]) -> Output {
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

/// Exit 0, nothing on stdout, no panic.
fn silent(out: &Output, what: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{what}: {stderr}");
    assert!(out.stdout.is_empty(), "{what}: {}", String::from_utf8_lossy(&out.stdout));
    assert!(!stderr.contains("panicked"), "{what}: {stderr}");
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

#[cfg(unix)]
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

#[cfg(unix)]
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
fn a_gate_outside_a_terminal_is_silent_with_or_without_a_core_or_a_terminal_id() {
    let name = "gt-missing";
    let _core = Core(name.into());
    // No core listens: the gate must not start one, and says nothing.
    silent(&gate_outside(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", "t99")]), "no core");
    assert!(!counter(name).exists(), "nothing is booked outside a terminal");
    // A core, but no such terminal.
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    silent(&gate_outside(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", "t99")]), "missing terminal");
    silent(&gate_outside(name, big_event().as_bytes(), &[]), "no terminal id");
    assert!(!counter(name).exists(), "nothing is booked outside a terminal");
}

#[test]
fn inside_a_terminal_the_answer_and_the_count_are_there() {
    let name = "gt-inside";
    let _core = Core(name.into());
    let out = gate_as(name, big_event().as_bytes(), &[]);
    quiet(&out, "inside");
    assert_eq!(stats(name)["tokens_saved"], saved_by(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
}

#[cfg(unix)]
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
    // Two runs in one terminal: its label is their running total.
    let (address, runs) = gate_runs(name, &[big_event().as_bytes(), big_event().as_bytes()], &[]);
    let (one, two) = (&runs[0], &runs[1]);
    quiet(one, "first");
    quiet(two, "second");
    assert_eq!(saved_label(name, &address), Some((saved_by(one) + saved_by(two)).to_string()), "a running total");
    let s = rpc("gate.stats");
    assert_eq!(s["outputs"], 2);
    assert_eq!(s["tokens_saved"].as_u64().unwrap(), saved_by(one) + saved_by(two));
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn a_saved_label_that_is_not_a_count_is_overwritten_and_the_top_stays_the_top() {
    let name = "gt-badlabel";
    let _core = Core(name.into());
    for junk in ["abc", "-5", "1e3", "", "12abc"] {
        let inside = gate_start(name, &[big_event().as_bytes()], &[]);
        let address = inside.address.clone();
        let set = core_cmd(name, &["label", &address, &format!("h0x.saved={junk}")]).output().unwrap();
        assert!(set.status.success(), "{junk:?}: {}", String::from_utf8_lossy(&set.stderr));
        let out = inside.finish().remove(0);
        quiet(&out, junk);
        assert_eq!(saved_label(name, &address), Some(saved_by(&out).to_string()), "{junk:?} counts as 0");
    }
    let top = u64::MAX.to_string();
    let inside = gate_start(name, &[big_event().as_bytes()], &[]);
    let address = inside.address.clone();
    core_cmd(name, &["label", &address, &format!("h0x.saved={top}")]).output().unwrap();
    quiet(&inside.finish().remove(0), "top");
    assert_eq!(saved_label(name, &address), Some(top), "saturates, no wrap");
}

#[test]
fn an_empty_terminal_id_is_silent_and_labels_nothing() {
    let name = "gt-emptyid";
    let _core = Core(name.into());
    let address = core_with_terminal(name);
    let out = gate_outside(name, big_event().as_bytes(), &[("H0X_TERMINAL_ID", "")]);
    silent(&out, "empty id");
    assert!(!counter(name).exists(), "nothing is booked outside a terminal");
    assert_eq!(saved_label(name, &address), None, "no terminal was guessed");
}

#[test]
fn a_core_that_never_vouches_makes_the_gate_silent_within_two_seconds() {
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
        .env("H0X_TERMINAL_SOCKET", &socket)
        .env("H0X_TERMINAL_ID", "t1")
        .env("H0X_SCOPE", "s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    child.stdin.take().unwrap().write_all(big_event().as_bytes()).unwrap();
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
    assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
    let request: Value = serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(request["method"], "terminal.vouch", "the fake core was really asked");
    let mut stdout = Vec::new();
    std::io::Read::read_to_end(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
    assert!(stdout.is_empty(), "a core that cannot vouch gets no answer: {}", String::from_utf8_lossy(&stdout));
    assert!(!counter(name).exists(), "nothing is booked");
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

#[cfg(unix)]
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

fn stats_cli(name: &str, args: &[&str]) -> Output {
    core_cmd(name, &[&["gate", "stats"], args].concat()).stdin(Stdio::null()).output().unwrap()
}

#[cfg(unix)]
#[test]
fn gate_stats_shows_the_counter_with_or_without_a_core() {
    let name = "gt-statscli";
    let _core = Core(name.into());
    let zeros = json!({"tokens_saved": 0, "outputs": 0, "bytes_in": 0, "bytes_shown": 0});
    // No core runs: it reads the file (here: none yet) and does not start one.
    let out = stats_cli(name, &["--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), zeros);
    let first = gate_as(name, big_event().as_bytes(), &[]);
    let second = gate_as(name, big_event().as_bytes(), &[]);
    let want = saved_by(&first) + saved_by(&second);
    let read = |args: &[&str]| serde_json::from_slice::<Value>(&stats_cli(name, args).stdout).unwrap();
    let file = read(&["--json"]);
    assert_eq!((file["outputs"].as_u64(), file["tokens_saved"].as_u64()), (Some(2), Some(want)));
    // With a core it asks the core, and the answer is the same.
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    assert_eq!(read(&["--json"]), file);
    let human = stats_cli(name, &[]);
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains(&format!("{want} tokens saved (2 outputs")) || text.trim_start().starts_with('{'), "{text}");
}

#[test]
fn gate_stats_rejects_bad_arguments_without_panicking() {
    let name = "gt-statsbad";
    let _core = Core(name.into());
    for args in [&["--bogus"][..], &["extra"]] {
        let out = stats_cli(name, args);
        assert_ne!(out.status.code(), Some(0), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked") && !err.is_empty(), "{args:?}: {err}");
    }
    assert!(!String::from_utf8_lossy(&core_cmd(name, &["--help"]).output().unwrap().stdout).contains("gate"));
}

/// A Bash event whose stdout is over two megabytes: more than any pipe buffer.
fn huge_event() -> String {
    let stdout: String = (0..150_000).map(|i| format!("row {i} is fine\n")).collect();
    let ev =
        json!({"tool_name": "Bash", "tool_input": {}, "tool_response": {"stdout": stdout, "stderr": ""}}).to_string();
    assert!(ev.len() > 2 << 20);
    ev
}

/// `h0x-core <args>` outside any terminal with `env` and `input` on stdin: what it printed, how long
/// it took from start to exit, and whether every byte of stdin was taken (a command that exits
/// without draining breaks the writer's pipe).
fn outside(name: &str, args: &[&str], input: &[u8], env: &[(&str, &str)]) -> (Output, std::time::Duration, bool) {
    let started = std::time::Instant::now();
    let mut child = core_cmd(name, args)
        .env_remove("H0X_SCOPE")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let feed = std::thread::spawn(move || stdin.write_all(&input).is_ok());
    let out = child.wait_with_output().unwrap();
    let took = started.elapsed();
    (out, took, feed.join().unwrap())
}

/// Exit 2, nothing on stdout, the one line on stderr.
fn map_refused(out: &Output, what: &str) {
    assert_eq!(out.status.code(), Some(2), "{what}: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "{what}: {}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim_end(), "map works inside HyperOx terminals", "{what}");
}

#[test]
fn inside_a_terminal_a_stdin_over_two_megabytes_is_read_whole_and_replaced() {
    let name = "gt-bigin";
    let _core = Core(name.into());
    let out = gate_as(name, huge_event().as_bytes(), &[]);
    quiet(&out, "huge");
    let v: Value = serde_json::from_slice(&out.stdout).expect("one JSON object on stdout");
    let text = v["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str().unwrap();
    assert!(text.starts_with("[HyperOx Token Saver: output "), "{text}");
    assert_eq!(stats(name)["outputs"], 1, "the huge output is counted");
}

#[test]
fn hook_commands_drain_empty_or_huge_stdin_and_stay_silent_fast_without_or_with_a_dead_socket() {
    let name = "gt-dead";
    let _core = Core(name.into());
    // A socket nobody listens on: a pipe name never created, or a unix socket file left behind.
    let dead = if cfg!(windows) {
        format!(r"\\.\pipe\h0x-gt-{}-dead", std::process::id())
    } else {
        let path = scratch(name).join("dead.sock");
        #[cfg(unix)]
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        path.display().to_string()
    };
    let huge = huge_event();
    let dead_env = [("H0X_TERMINAL_ID", "t1"), ("H0X_SCOPE", "copied"), ("H0X_TERMINAL_SOCKET", dead.as_str())];
    for (place, env) in [("no variables", &[][..]), ("a dead socket", &dead_env[..])] {
        for cmd in ["gate", "handoff", "route"] {
            for (size, input) in [("empty", &b""[..]), ("huge", huge.as_bytes())] {
                let what = format!("{cmd}, {place}, {size} stdin");
                let (out, took, fed) = outside(name, &[cmd], input, env);
                silent(&out, &what);
                assert!(out.stderr.is_empty(), "{what}: {}", String::from_utf8_lossy(&out.stderr));
                assert!(fed, "{what}: stdin was not drained");
                assert!(took < std::time::Duration::from_secs(2), "{what}: took {took:?}");
            }
        }
        let (out, took, _) = outside(name, &["map", "grep", "needle"], b"", env);
        map_refused(&out, place);
        assert!(took < std::time::Duration::from_secs(2), "map, {place}: took {took:?}");
    }
    assert!(!counter(name).exists(), "nothing is booked outside a terminal");
}

#[test]
fn every_hook_command_asks_a_stalled_core_once_and_gives_up_under_a_second() {
    let name = "gt-once";
    let _core = Core(name.into());
    let socket = if cfg!(windows) {
        std::path::PathBuf::from(format!(r"\\.\pipe\h0x-gt-{}-once", std::process::id()))
    } else {
        scratch(name).join("once.sock")
    };
    // Counts every connection; never answers and never hangs up.
    let listener = h0x_core::platform::listen(&socket).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let _ = tx.send(());
            std::mem::forget(stream);
        }
    });
    let socket = socket.display().to_string();
    let env = [("H0X_TERMINAL_ID", "t1"), ("H0X_SCOPE", "s"), ("H0X_TERMINAL_SOCKET", socket.as_str())];
    let event = big_event();
    let cases: [(&[&str], &[u8]); 4] = [
        (&["gate"], event.as_bytes()),
        (&["handoff"], event.as_bytes()),
        (&["route"], event.as_bytes()),
        (&["map", "grep", "needle"], b""),
    ];
    for (args, input) in cases {
        let (out, took, _) = outside(name, args, input, &env);
        if args[0] == "map" {
            map_refused(&out, "map");
        } else {
            silent(&out, args[0]);
        }
        // A 1 s vouch cap plus process start; parallel tests can slow the start, so allow 4 s.
        // The point is that a stalled core never makes the hook hang.
        assert!(took < std::time::Duration::from_secs(4), "{args:?}: took {took:?}");
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or_else(|_| panic!("{args:?} never asked the core"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(rx.try_iter().count(), 0, "{args:?} asked the core more than once");
    }
    assert!(!counter(name).exists(), "nothing is booked");
}

/// The `H0X_*` variables a real terminal of session `name`'s core got, read from inside it; the
/// terminal stays running.
fn terminal_vars(name: &str) -> Vec<(String, String)> {
    let started = core_cmd(name, &["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let dir = scratch(name);
    let dump = dir.join("vars.txt");
    let tmp = dir.join("vars.tmp");
    let program: Vec<String> = if cfg!(windows) {
        let bat = dir.join("vars.cmd");
        let body = format!(
            "@echo off\r\nset H0X_ > \"{}\"\r\nmove /y \"{}\" \"{}\" >nul\r\nping -n 60 127.0.0.1 >nul\r\n",
            tmp.display(),
            tmp.display(),
            dump.display()
        );
        std::fs::write(&bat, body).unwrap();
        vec!["cmd".into(), "/c".into(), bat.display().to_string()]
    } else {
        let body = format!("env > '{}' && mv '{}' '{}'; sleep 60", tmp.display(), tmp.display(), dump.display());
        vec!["sh".into(), "-c".into(), body]
    };
    let mut args = vec!["--json", "run", "--bg"];
    args.extend(program.iter().map(String::as_str));
    let ran = core_cmd(name, &args).output().unwrap();
    assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
    let limit = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !dump.exists() {
        assert!(std::time::Instant::now() < limit, "the terminal never wrote its variables");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::read_to_string(&dump)
        .unwrap()
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').split_once('='))
        .filter(|(k, _)| k.starts_with("H0X_"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn a_real_terminals_secret_copied_into_a_process_outside_its_tree_is_silence() {
    let name = "gt-copied";
    let _core = Core(name.into());
    let vars = terminal_vars(name);
    let get = |key: &str| {
        let found = vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        found.unwrap_or_else(|| panic!("{key} missing in the terminal: {vars:?}"))
    };
    let (id, scope, socket) = (get("H0X_TERMINAL_ID"), get("H0X_SCOPE"), get("H0X_TERMINAL_SOCKET"));
    assert!(!id.is_empty() && !scope.is_empty(), "{vars:?}");
    // Everything a terminal has, but this test process is not below its program.
    let env =
        [("H0X_TERMINAL_ID", id.as_str()), ("H0X_SCOPE", scope.as_str()), ("H0X_TERMINAL_SOCKET", socket.as_str())];
    for cmd in ["gate", "handoff", "route"] {
        let (out, took, fed) = outside(name, &[cmd], big_event().as_bytes(), &env);
        silent(&out, &format!("{cmd} with a copied secret"));
        assert!(fed, "{cmd}: stdin was not drained");
        assert!(took < std::time::Duration::from_secs(2), "{cmd}: took {took:?}");
    }
    let (out, _, _) = outside(name, &["map", "grep", "needle"], b"", &env);
    map_refused(&out, "map with a copied secret");
    assert!(!counter(name).exists(), "nothing is booked for a copied secret");
}
