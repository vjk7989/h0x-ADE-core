#![cfg(unix)] // the deciding code is tested on every OS in src/bridge; this drives a real core

mod common;

use common::*;
use h0x_core::bridge::{Bridge, Command, Reply, Sender};
use h0x_core::runtime::now_ms;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

fn bridge(s: &Session) -> Bridge {
    Bridge::at(s.root.join("config/h0x-core/bridge"), Default::default())
}

fn me() -> Sender {
    Sender { transport: "telegram".into(), user_id: "42".into() }
}

/// The session's core, one connection per call, like the CLI.
fn core(s: &Session) -> impl Fn(&str, Value) -> Result<Value, h0x_core::rpc::RpcError> + '_ {
    move |method: &str, params: Value| {
        h0x_core::client::call(&h0x_core::platform::connect(&s.socket()).unwrap(), method, params)
    }
}

#[test]
fn pairing_locking_and_the_log_through_the_cli() {
    let s = Session::new();
    assert_eq!(s.json(&["bridge", "ls"]), json!({"identities": [], "locked": false}));
    let pair = s.json(&["bridge", "pair"]);
    assert_eq!(pair["transport"], "telegram");
    bridge(&s).pair_verify("telegram", "42", "vijay", pair["code"].as_str().unwrap(), now_ms()).unwrap();
    let ls = s.json(&["bridge", "ls"]);
    assert_eq!((&ls["identities"][0]["label"], &ls["identities"][0]["id"]), (&json!("vijay"), &json!(1)));

    assert_eq!(s.json(&["bridge", "lock"]), json!({"locked": true}));
    assert_eq!(bridge(&s).execute(&core(&s), &me(), Command::List, now_ms()), Reply::Locked);
    assert_eq!(s.err(&["bridge", "pair"]).1["error"]["message"], "remote control is locked");
    assert_eq!(s.json(&["bridge", "unlock"]), json!({"locked": false}));
    let log = s.json(&["bridge", "log", "-n", "1"]);
    assert_eq!((&log["entries"][0]["command"], &log["entries"][0]["result"]), (&json!("list"), &json!("locked")));
    assert_eq!(s.json(&["bridge", "log"])["entries"].as_array().unwrap().len(), 2, "the pairing too");

    let dir = s.root.join("config/h0x-core/bridge");
    let mode = |name: &str| std::fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode("."), mode("state.json"), mode("audit.jsonl")), (0o700, 0o600, 0o600));

    assert_eq!(s.err(&["bridge", "unpair", "9"]).0, 3, "not found");
    assert_eq!(s.err(&["bridge", "unpair"]).0, 2, "an id or --all");
    assert_eq!(s.err(&["bridge", "pair", "--transport", "sms"]).0, 2);
    assert_eq!(s.json(&["bridge", "unpair", "--all"]), json!({"removed": 1}));
    std::fs::write(dir.join("state.json"), "[]").unwrap();
    let message = s.err(&["bridge", "ls"]).1["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("h0x-core bridge reset"), "{message}");
    let moved = s.json(&["bridge", "reset"]);
    assert!(moved["moved_to"].as_str().unwrap().contains("state.json.bad-"), "{moved}");
    assert_eq!(s.json(&["bridge", "ls"]), json!({"identities": [], "locked": false}));
    assert_eq!(s.json(&["bridge", "reset"]), json!({"moved_to": null}));
    assert!(!s.connect_ok(), "bridge commands never start a core");
}

#[test]
fn a_paired_account_drives_real_terminals() {
    let s = Session::new();
    let mut b = bridge(&s);
    let (code, _) = b.pair("telegram", now_ms()).unwrap();
    b.pair_verify("telegram", "42", "", &code, now_ms()).unwrap();
    let shell = run(&s, &["cat"]);
    let agent =
        run(&s, &["sh", "-c", &script(&s, "r state=blocked:kind=permission; read -r l; echo answered:$l; sleep 100")]);
    wait_until("the agent blocks", || get(&s, &agent)["status"]["state"] == "blocked");
    let call = core(&s);
    let mut ask = |command| b.execute(&call, &me(), command, now_ms());

    let Reply::Done(list) = ask(Command::List) else { panic!() };
    assert!(list.contains(&format!("{shell} · cat · ")) && list.contains("blocked"), "{list}");

    // A plain shell takes text only after a confirmation; what comes back is redacted.
    let send = Command::Send { terminal: shell.clone(), text: "hello ghp_ABCDEFGHIJKLMNOPQRSTUVWX".into() };
    let Reply::NeedsConfirm { token, .. } = ask(send) else { panic!() };
    assert!(matches!(ask(Command::Confirm { token }), Reply::Done(_)));
    wait_until("cat echoes", || text(&s, &shell).contains("hello"));
    let Reply::Done(read) = ask(Command::Read { terminal: shell.clone(), lines: 5 }) else { panic!() };
    assert!(read.contains("hello [redacted]") && !read.contains("ghp_"), "{read}");

    // An agent blocked on a dialog: approve presses enter, no confirmation.
    assert!(matches!(ask(Command::Approve { terminal: agent.clone() }), Reply::Done(_)));
    wait_until("the agent got its answer", || text(&s, &agent).contains("answered:"));

    let Reply::NeedsConfirm { token, what } = ask(Command::Kill { terminal: shell.clone() }) else { panic!() };
    assert!(what.starts_with(&format!("kill {shell} (cat in ")), "{what}");
    assert_eq!(get(&s, &shell)["state"], "live", "not before the confirmation");
    assert!(matches!(ask(Command::Confirm { token }), Reply::Done(_)));
    wait_until("killed", || get(&s, &shell)["state"] == "finished");
}
