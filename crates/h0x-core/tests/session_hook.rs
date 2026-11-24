#![cfg(unix)] // Windows: tests/windows.rs

//! `h0x-core hook session`: inside a HyperOx terminal it records the session id as a private
//! label; outside, or from a process that merely carries the terminal's secret, it does nothing.
mod common;

use common::*;
use std::process::Stdio;

const INSIDE: &str = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";
const OUTSIDE: &str = "11111111-2222-4333-8444-555555555555";

fn event(id: &str) -> String {
    format!(r#"{{"session_id":"{id}","hook_event_name":"SessionStart","source":"startup"}}"#)
}

/// Runs the hook with `env` added (the terminal's variables, when the caller has them) and `input` on stdin.
fn hook(s: &Session, env: &[(&str, String)], input: &str) -> std::process::Output {
    let mut cmd = s.cmd();
    cmd.args(["hook", "session"]).envs(env.iter().map(|(k, v)| (*k, v.as_str())));
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn silent_success(out: &std::process::Output) {
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
}

#[test]
fn only_a_hook_inside_the_terminal_records_the_session() {
    let s = Session::new();
    let root = s.root.display();
    std::fs::write(s.root.join("event.json"), event(INSIDE)).unwrap();
    let body = format!(
        "printf %s \"$H0X_SCOPE\" > {root}/scope; printf %s \"$H0X_TERMINAL_ID\" > {root}/id; \
         printf %s \"$H0X_TERMINAL_SOCKET\" > {root}/socket; \
         {BIN} hook session < {root}/event.json > {root}/hook.out 2>&1; echo $? > {root}/hook.code; \
         : > {root}/done; sleep 100"
    );
    let t = run(&s, &["sh", "-c", &body]);
    wait_until("the hook inside the terminal", || s.root.join("done").exists());
    let read = |name: &str| std::fs::read_to_string(s.root.join(name)).unwrap();
    assert_eq!(read("hook.code").trim(), "0");
    assert_eq!(read("hook.out"), "", "a hook prints nothing");
    assert_eq!(get(&s, &t)["labels"]["h0x.session"], format!("claude:{INSIDE}"), "vouched: the label is set");

    // Outside the terminal, with its secret copied: the secret alone is not enough.
    let carried =
        [("H0X_TERMINAL_ID", read("id")), ("H0X_TERMINAL_SOCKET", read("socket")), ("H0X_SCOPE", read("scope"))];
    silent_success(&hook(&s, &carried, &event(OUTSIDE)));
    // Outside with a wrong secret, and with none.
    let wrong =
        [("H0X_TERMINAL_ID", read("id")), ("H0X_TERMINAL_SOCKET", read("socket")), ("H0X_SCOPE", "nope".into())];
    silent_success(&hook(&s, &wrong, &event(OUTSIDE)));
    silent_success(&hook(&s, &[], &event(OUTSIDE)));
    // Bad input, even inside the terminal's environment.
    silent_success(&hook(&s, &carried, "not json"));
    assert_eq!(get(&s, &t)["labels"]["h0x.session"], format!("claude:{INSIDE}"), "nothing replaced it");
}

#[test]
fn a_terminal_without_a_recorded_session_has_no_label() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    silent_success(&hook(&s, &[], &event(OUTSIDE)));
    assert!(get(&s, &t)["labels"].get("h0x.session").is_none());
}

/// Asks `terminal.vouch` over the terminal's own socket from a Python program, with the right
/// secret and with near misses; writes the raw replies (and the secret, for the outside check).
const VOUCH_PY: &str = r#"
import json, os, socket, sys, time
def vouch(params):
    s = socket.socket(socket.AF_UNIX)
    s.connect(os.environ["H0X_TERMINAL_SOCKET"])
    req = {"jsonrpc": "2.0", "id": 1, "method": "terminal.vouch", "params": params}
    s.sendall((json.dumps(req) + "\n").encode())
    line = s.makefile().readline()
    s.close()
    return line
t, k = os.environ["H0X_TERMINAL_ID"], os.environ["H0X_SCOPE"]
out = {
    "scope": k,
    "right": vouch({"terminal": t, "scope": k}),
    "empty": vouch({"terminal": t, "scope": ""}),
    "missing": vouch({"terminal": t}),
    "prefix": vouch({"terminal": t, "scope": k[:-1]}),
    "longer": vouch({"terminal": t, "scope": k + "x"}),
    "upper": vouch({"terminal": t, "scope": k.upper() if k.upper() != k else k + "X"}),
    "unknown": vouch({"terminal": "t999", "scope": k}),
}
open(sys.argv[1] + ".tmp", "w").write(json.dumps(out))
os.rename(sys.argv[1] + ".tmp", sys.argv[1])
time.sleep(100)
"#;

/// Runs VOUCH_PY in a terminal (`program`, with `{py}` and `{out}` filled in); returns the address and replies.
fn vouch_inside(s: &Session, name: &str, program: &[&str]) -> (String, serde_json::Value) {
    let py = s.root.join("vouch.py");
    std::fs::write(&py, VOUCH_PY).unwrap();
    let out = s.root.join(name);
    let (py, out_s) = (py.display().to_string(), out.display().to_string());
    let args: Vec<String> = program.iter().map(|a| a.replace("{py}", &py).replace("{out}", &out_s)).collect();
    let t = run(s, &args.iter().map(String::as_str).collect::<Vec<_>>());
    wait_until("the vouch calls inside the terminal", || out.exists());
    (t, serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap())
}

fn ok_of(raw: &serde_json::Value) -> serde_json::Value {
    let reply: serde_json::Value = serde_json::from_str(raw.as_str().unwrap()).unwrap();
    reply["result"]["ok"].clone()
}

#[test]
fn terminal_vouch_is_true_only_from_inside_the_terminal_with_its_exact_secret() {
    let s = Session::new();
    // The caller as a child of the terminal's program, and as the program itself.
    let programs: [(&str, &[&str]); 2] =
        [("child", &["sh", "-c", "python3 {py} {out}; sleep 100"]), ("itself", &["python3", "{py}", "{out}"])];
    for (name, program) in programs {
        let (t, got) = vouch_inside(&s, name, program);
        let scope = got["scope"].as_str().unwrap().to_string();
        assert!(!scope.is_empty(), "{name}: the terminal has a secret");
        assert_eq!(ok_of(&got["right"]), true, "{name}: inside, right secret: {}", got["right"]);
        for wrong in ["empty", "missing", "prefix", "longer", "upper", "unknown"] {
            assert_eq!(ok_of(&got[wrong]), false, "{name}: {wrong}: {}", got[wrong]);
        }
        for (key, raw) in got.as_object().unwrap() {
            if key != "scope" {
                assert!(!raw.as_str().unwrap().contains(&scope), "{name}: {key} echoes the secret: {raw}");
            }
        }

        // The same secret from outside the tree (this test process): plain and framed.
        let mut plain = s.connect();
        let reply = plain.call(1, "terminal.vouch", serde_json::json!({"terminal": t, "scope": scope}));
        assert_eq!(reply["result"]["ok"], false, "{name}: outside, plain: {reply}");
        let mut framed = framed::Framed::open(&s);
        let reply = framed.call("terminal.vouch", serde_json::json!({"terminal": t, "scope": scope}));
        assert_eq!(reply["result"]["ok"], false, "{name}: outside, framed (no peer pid): {reply}");

        // Errors never echo the secret either.
        for params in [
            serde_json::json!({"scope": scope}),
            serde_json::json!({"terminal": 7, "scope": scope}),
            serde_json::json!({"terminal": t, "scope": [scope]}),
            serde_json::json!({"terminal": t, "scope": {"k": scope}}),
            serde_json::json!([t, scope]),
        ] {
            let reply = plain.call(2, "terminal.vouch", params.clone());
            assert!(!reply.to_string().contains(&scope), "{name}: {params} echoes the secret: {reply}");
            assert_ne!(reply["result"]["ok"], true, "{name}: {params}");
        }
    }
}

#[test]
fn terminal_vouch_is_not_a_cli_command() {
    let s = Session::new();
    let help = String::from_utf8(s.core(&["--help"]).stdout).unwrap();
    assert!(!help.contains("vouch"), "{help}");
    assert!(!s.core(&["terminal", "vouch", "t1", "x"]).status.success());
    assert!(!s.core(&["vouch", "t1", "x"]).status.success());
}

/// Inside a vouched terminal, every bad input or argument is a silent exit 0 that sets nothing;
/// then exactly 4 MB of input is accepted, and a `compact` start with `--agent codex` replaces it.
#[test]
fn inside_the_terminal_only_well_formed_input_records_the_session() {
    let s = Session::new();
    let (a, b, c) = (
        "aaaaaaaa-0000-4000-8000-000000000001",
        "bbbbbbbb-0000-4000-8000-000000000002",
        "cccccccc-0000-4000-8000-000000000003",
    );
    let file = |name: &str, bytes: Vec<u8>| std::fs::write(s.root.join(name), bytes).unwrap();
    let padded = |id: &str, len: usize| {
        let mut e = event(id).into_bytes();
        e.resize(len, b' ');
        e
    };
    const MAX: usize = 4 * 1024 * 1024;
    file("over", padded(a, MAX + 1));
    file("exact", padded(b, MAX));
    file("empty", Vec::new());
    file("utf8", vec![0xff, 0xfe, b'{', b'}']);
    file("array", format!("[{}]", event(a)).into_bytes());
    file("upper", event(&a.to_uppercase()).into_bytes());
    file("stop", event(a).replace("SessionStart", "Stop").into_bytes());
    file("valid", event(a).into_bytes());
    file(
        "compact",
        format!(r#"{{"session_id":"{c}","hook_event_name":"SessionStart","source":"compact"}}"#).into_bytes(),
    );
    let root = s.root.display();
    let body = script(
        &s,
        &format!(
            "cd {root}; h(){{ {BIN} hook session \"$@\" >> hook.out 2>&1; echo $? >> codes; }}; \
             h < over; h < empty; h < utf8; h < array; h < upper; h < stop; \
             h --agent Claude < valid; h --agent 'a b' < valid; h --agent '' < valid; h --agent < valid; \
             env -u H0X_TERMINAL_SOCKET {BIN} hook session < valid >> hook.out 2>&1; echo $? >> codes; \
             env -u H0X_SCOPE {BIN} hook session < valid >> hook.out 2>&1; echo $? >> codes; \
             : > step1; w go1; h < exact; : > step2; w go2; h --agent codex < compact; : > done; sleep 100"
        ),
    );
    let t = run(&s, &["sh", "-c", &body]);
    let read = |name: &str| std::fs::read_to_string(s.root.join(name)).unwrap();
    let label = || get(&s, &t)["labels"].get("h0x.session").cloned();

    wait_until("the bad inputs", || s.root.join("step1").exists());
    assert_eq!(label(), None, "4 MB + 1, empty, bad UTF-8, an array, upper-case id, Stop, bad agents, missing vars");
    touch(&s, "go1");
    wait_until("exactly 4 MB", || s.root.join("step2").exists());
    assert_eq!(label(), Some(format!("claude:{b}").into()), "exactly 4 MB is accepted");
    touch(&s, "go2");
    wait_until("the compact start", || s.root.join("done").exists());
    assert_eq!(label(), Some(format!("codex:{c}").into()), "any source, compact too; the agent is named");

    assert_eq!(read("hook.out"), "", "a hook prints nothing");
    let codes = read("codes");
    assert_eq!(codes.lines().count(), 14, "{codes}");
    assert!(codes.lines().all(|c| c == "0"), "{codes}");
}
