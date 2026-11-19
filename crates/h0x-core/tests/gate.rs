//! `h0x-core gate` as Claude Code runs it: the hook event on stdin, the answer on stdout, exit 0.
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// The gate with `input` on stdin, in an isolated home and session.
fn gate(input: &[u8]) -> Output {
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_h0x-core"))
        .arg("gate")
        .env("H0X_CORE_SESSION", "gate-test")
        .env("HOME", &root)
        .env("USERPROFILE", &root)
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("H0X_TERMINAL_ID")
        .env_remove("H0X_CORE_SOCKET")
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
