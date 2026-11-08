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
    assert_eq!(s.json(&["bridge", "ls"]), json!({"identities": [], "locked": false, "telegram_token": "unset"}));
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
    assert_eq!(s.json(&["bridge", "ls"]), json!({"identities": [], "locked": false, "telegram_token": "unset"}));
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

const TOKEN: &str = "123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123";

/// Requests a stub saw: (path, body).
type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>;

/// A loopback Bot API: one `/agents` from user 42, then nothing; records
/// every request's method and body.
fn stub() -> (String, Seen) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        let mut polls = 0;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let path = first.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let method = path.rsplit('/').next().unwrap_or_default().to_string();
            polls += usize::from(method == "getUpdates");
            let reply = match method.as_str() {
                "getUpdates" if polls == 1 => {
                    json!({"ok": true, "result": [{"update_id": 1, "message": {"message_id": 1,
                    "from": {"id": 42}, "chat": {"id": 42, "type": "private"}, "text": "/agents"}}]})
                }
                "getUpdates" => {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    json!({"ok": true, "result": []})
                }
                "getMe" => json!({"ok": true, "result": {"is_bot": true, "username": "h0x_bot"}}),
                _ => json!({"ok": true, "result": {"message_id": 7}}),
            }
            .to_string();
            log.lock().unwrap().push((path, serde_json::from_slice(&body).unwrap()));
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len());
            let _ = stream.write_all(format!("{head}{reply}").as_bytes());
        }
    });
    (url, seen)
}

#[test]
fn the_telegram_bot_answers_a_paired_account() {
    let s = Session::new();
    let (code, err) = s.err(&["bridge", "run"]);
    assert_eq!((code, err["error"]["message"].as_str()), (1, Some("no bot token")));
    assert_eq!(s.err(&["bridge", "telegram", "token"]).1["error"]["kind"], "invalid_params", "nothing piped in");
    let stored = s.hc_input(&["--json", "bridge", "telegram", "token"], format!("{TOKEN}\n").as_bytes());
    assert!(stored.status.success(), "{}", String::from_utf8_lossy(&stored.stderr));
    let ls = s.core(&["bridge", "ls"]);
    let shown = String::from_utf8_lossy(&ls.stdout).to_string();
    assert!(shown.contains(r#""telegram_token":"set""#) && !shown.contains("ABCdef"), "{shown}");
    let file = s.root.join("config/h0x-core/bridge/telegram.token");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);

    let (code, _) = bridge(&s).pair("telegram", now_ms()).unwrap();
    bridge(&s).pair_verify("telegram", "42", "", &code, now_ms()).unwrap();
    run(&s, &["sleep", "100"]);
    let (url, seen) = stub();
    let mut far = s.cmd();
    far.args(["bridge", "run"]).env("H0X_CORE_TELEGRAM_URL", "http://10.1.2.3:80");
    let refused = far.output().unwrap();
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not on this machine"), "only a loopback stub");

    let mut bot = s.cmd().args(["bridge", "run"]).env("H0X_CORE_TELEGRAM_URL", &url).spawn().unwrap();
    wait_until("the bot answers /agents and polls again", || {
        let seen = seen.lock().unwrap();
        seen.iter().any(|(p, _)| p.ends_with("/sendMessage"))
            && seen.iter().filter(|(p, _)| p.ends_with("/getUpdates")).count() >= 2
    });
    bot.kill().unwrap();
    bot.wait().unwrap();
    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().all(|(path, _)| path.starts_with(&format!("/bot{TOKEN}/"))), "the token rode in the URL");
    let (_, answer) = seen.iter().find(|(p, _)| p.ends_with("/sendMessage")).unwrap();
    assert_eq!(answer["chat_id"], 42);
    assert!(answer["text"].as_str().unwrap().contains("sleep"), "{answer}");
    let polls: Vec<&Value> =
        seen.iter().filter(|(p, _)| p.ends_with("/getUpdates")).map(|(_, b)| &b["offset"]).collect();
    assert_eq!(polls[..2], [&json!(0), &json!(2)], "the update was acknowledged");

    let cleared = s.json(&["bridge", "telegram", "token", "--clear"]);
    assert_eq!(cleared, json!({"telegram_token": "unset"}));
    assert!(!file.exists());
}

#[test]
fn piped_tokens_with_line_ends_or_the_wrong_shape() {
    let s = Session::new();
    let file = s.root.join("config/h0x-core/bridge/telegram.token");
    let cleared = s.core(&["bridge", "telegram", "token", "--clear"]);
    assert!(cleared.status.success());
    // stdout is piped, so the CLI prints JSON (cli.rs:187: `json = asked || piped`).
    let cleared: Value = serde_json::from_slice(&cleared.stdout).unwrap();
    assert_eq!(cleared, json!({"telegram_token": "unset"}), "nothing was set: still unset");

    let near = "123456:ABCdef ghi-JKLmnoPQRstuVWXyz0123";
    for bad in [String::new(), "\n".into(), format!("{near}\n"), format!("\n{TOKEN}\n"), "123456:short\r\n".into()] {
        let out = s.hc_input(&["bridge", "telegram", "token"], bad.as_bytes());
        assert!(!out.status.success(), "{bad:?}");
        let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(said.contains("invalid_params") && !said.contains("ABCdef"), "{bad:?}: {said}");
        assert!(!file.exists(), "{bad:?}: nothing stored");
    }

    let stored =
        s.hc_input(&["--json", "bridge", "telegram", "token"], format!("  {TOKEN}\r\nsecond line\n").as_bytes());
    assert!(stored.status.success(), "{}", String::from_utf8_lossy(&stored.stderr));
    assert!(
        !String::from_utf8_lossy(&stored.stdout).contains("ABCdef")
            && !String::from_utf8_lossy(&stored.stderr).contains("ABCdef")
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), TOKEN, "the first line, trimmed of CRLF and blanks");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    let ls = s.core(&["bridge", "ls"]);
    let shown = String::from_utf8_lossy(&ls.stdout);
    // Piped, so JSON (cli.rs:187).
    assert!(shown.contains(r#""telegram_token":"set""#) && !shown.contains("ABCdef"), "{shown}");
}

#[test]
fn the_web_panel_prints_a_pairing_link_and_serves_only_its_own_host() {
    use std::io::{BufRead, BufReader, Read, Write};
    let s = Session::new();
    let pair = s.json(&["bridge", "pair", "--transport", "web"]);
    assert_eq!(pair["transport"], "web");
    assert!(pair["expires_ms"].as_u64().unwrap() <= now_ms() + 5 * 60_000, "a web code lives 5 minutes");

    let mut web = s.cmd().args(["bridge", "web", "--port", "0"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out = BufReader::new(web.stdout.take().unwrap());
    let mut lines = [String::new(), String::new()];
    for line in &mut lines {
        out.read_line(line).unwrap();
    }
    let base = lines[0].trim().strip_prefix("web panel: ").unwrap().trim_end_matches('/').to_string();
    let addr = base.strip_prefix("http://127.0.0.1:").map(|port| format!("127.0.0.1:{port}")).unwrap();
    assert!(lines[1].contains(&format!("{base}/#pair=")), "{}", lines[1]);
    let get = |host: &str| {
        let mut stream = std::net::TcpStream::connect(&addr).unwrap();
        stream.write_all(format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes()).unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer
    };
    let page = get(&addr);
    assert!(page.starts_with("HTTP/1.1 200 ") && page.contains("frame-ancestors 'none'"), "{page}");
    assert!(get("attacker.example").starts_with("HTTP/1.1 403 "), "DNS rebinding");
    web.kill().unwrap();
    web.wait().unwrap();
}
