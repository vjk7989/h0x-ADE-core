use super::*;
use crate::bridge::Key;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

const T: u64 = 7_000_000;
const TOKEN: &str = "123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123";
const ME: i64 = 42;

/// A private message from `user`.
fn said(user: i64, text: &str) -> Value {
    json!({"update_id": 1, "message": {"message_id": 5, "from": {"id": user, "username": "vijay"},
        "chat": {"id": user, "type": "private"}, "text": text}})
}

fn tapped(user: i64, data: &str) -> Value {
    json!({"update_id": 2, "callback_query": {"id": "q1", "from": {"id": user}, "data": data,
        "message": {"message_id": 77, "chat": {"id": user, "type": "private"}}}})
}

/// A paired frontend (user 42) and a fake core: t1 a plain shell, t2 an
/// agent blocked on a question, t3 an agent at work.
struct Fixture {
    front: Frontend,
    calls: RefCell<Vec<(String, Value)>>,
}

fn fixture() -> Fixture {
    let bridge = crate::bridge::scratch();
    let (code, _) = bridge.pair("telegram", T).unwrap();
    bridge.pair_verify("telegram", &ME.to_string(), "vijay", &code, T).unwrap();
    Fixture {
        front: Frontend {
            bridge,
            replies: Arc::default(),
            own: "h0x_bot".into(),
            selected: HashMap::new(),
            selected_at_lock: 0,
        },
        calls: RefCell::default(),
    }
}

/// The fake core: t1 a plain shell, t2 an agent blocked on a question, t3 an agent at work.
fn fake_core(method: &str, params: &Value) -> Result<Value, RpcError> {
    let t = params["terminal"].as_str().unwrap_or_default();
    let record = |state: Option<&str>| {
        let status = state.map_or(Value::Null, |s| json!({"state": s, "kind": "permission", "app": "claude-code"}));
        // Started as a shell; a reporting terminal runs an agent now.
        let front = if state.is_some() { "claude" } else { "bash" };
        json!({"address": t, "state": "live", "argv": ["/bin/sh"], "cwd": "/work", "foreground": {"name": front},
               "reported": state.is_some(), "status": status})
    };
    Ok(match (method, t) {
        ("terminal.get", "t1") => record(None),
        ("terminal.get", "t2") => record(Some("blocked")),
        ("terminal.get", _) => record(Some("working")),
        // Everything, finished included: t1 a shell, t2 blocked, t3 working, t9 ended.
        ("terminal.list", _) if params["all"] == true => json!({"terminals": [
            {"address": "t1", "name": "shell", "argv": ["bash"], "cwd": "/work", "state": "live", "status": null},
            {"address": "t2", "argv": ["claude"], "cwd": "/w", "state": "live", "status": {"state": "blocked", "app": "claude"}},
            {"address": "t3", "name": "api", "argv": ["claude"], "cwd": "/work/api", "state": "live", "status": {"state": "working", "app": "claude"}},
            {"address": "t9", "name": "old", "argv": ["codex"], "cwd": "C:\\Users\\me\\old", "state": "exited", "exit_code": 0},
        ]}),
        ("terminal.list", _) => {
            json!({"terminals": [{"address": "t2", "argv": ["claude"], "cwd": "/w", "state": "live", "status": {"state": "blocked", "kind": "permission"}}]})
        }
        ("terminal.read", _) => json!({"text": "a < b && c > d\nAPI_KEY=abcdef0123456789\n"}),
        _ => json!({}),
    })
}

impl Fixture {
    fn handle(&mut self, update: Value) -> Vec<Out> {
        let calls = &self.calls;
        let core = |method: &str, params: Value| {
            calls.borrow_mut().push((method.into(), params.clone()));
            fake_core(method, &params)
        };
        self.front.handle(&core, &update, T)
    }

    /// Core calls that act, minus lookups.
    fn acted(&self) -> Vec<(String, Value)> {
        self.calls.borrow_mut().drain(..).filter(|(m, _)| m != "terminal.get").collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.front.bridge.dir);
    }
}

fn texts(out: &[Out]) -> Vec<&str> {
    out.iter()
        .map(|o| match o {
            Out::Send { html, .. } | Out::Edit { html, .. } => html.as_str(),
            Out::Answer { .. } => "<answer>",
        })
        .collect()
}

#[test]
fn bot_commands_parse_into_bridge_commands() {
    let t = |s: &str| s.to_string();
    let cases = [
        ("/agents", Ask::Do(Command::List)),
        ("/agents@h0x_bot", Ask::Do(Command::List)),
        ("  /HELP  ", Ask::Help),
        ("/start", Ask::Help),
        ("/pair abcd efgh", Ask::Pair(t("abcd efgh"))),
        ("/read t3", Ask::Do(Command::Read { terminal: t("t3"), lines: 20 })),
        ("/read box/t3 50", Ask::Do(Command::Read { terminal: t("box/t3"), lines: 50 })),
        ("/send t3 fix it\nthen  test", Ask::Do(Command::Send { terminal: t("t3"), text: t("fix it\nthen  test") })),
        (
            "/keys t1 Enter ESC ctrl+c up y",
            Ask::Do(Command::Keys { terminal: t("t1"), keys: vec![Key::Enter, Key::Esc, Key::CtrlC, Key::Up, Key::Y] }),
        ),
        ("/approve t2", Ask::Do(Command::Approve { terminal: t("t2") })),
        ("/deny t2", Ask::Do(Command::Deny { terminal: t("t2") })),
        ("/interrupt t3", Ask::Do(Command::Interrupt { terminal: t("t3") })),
        ("/kill t3", Ask::Do(Command::Kill { terminal: t("t3") })),
        ("/lock", Ask::Do(Command::Lock)),
        (r"/run C:\work claude", Ask::Do(Command::Run { folder: t(r"C:\work"), command: t("claude") })),
        (
            r#"/run "C:\Users\me\My App" npm test"#,
            Ask::Do(Command::Run { folder: t(r"C:\Users\me\My App"), command: t("npm test") }),
        ),
        (
            r"/run \\server\share\app claude",
            Ask::Do(Command::Run { folder: t(r"\\server\share\app"), command: t("claude") }),
        ),
        (
            "/run /home/me/app claude --resume",
            Ask::Do(Command::Run { folder: t("/home/me/app"), command: t("claude --resume") }),
        ),
    ];
    for (text, want) in cases {
        assert_eq!(parse(text), Some(want), "{text:?}");
    }
    for bad in [
        "/pair",
        "/read",
        "/read t3 many",
        "/send t3",
        "/send",
        "/keys t1",
        "/keys t1 enter jump",
        "/approve",
        "/approve t2 t3",
        "/kill",
        "/run",
        "/run /x",
        r#"/run "C:\open claude"#,
        "/unknown",
        "/",
    ] {
        assert!(matches!(parse(bad), Some(Ask::Usage(_))), "{bad:?}");
    }
    for plain in ["hello", "", "  ", "yes /approve t2"] {
        assert_eq!(parse(plain), None, "{plain:?}");
    }
    assert_eq!(Key::parse("RETURN"), Some(Key::Enter));
    assert_eq!(Key::parse("f5"), None);
}

#[test]
fn html_is_escaped_and_long_text_is_split_under_4096() {
    assert_eq!(escape("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
    assert_eq!(chunks("", false), ["(empty)"]);
    assert_eq!(chunks("<x>", true), ["<pre>&lt;x&gt;</pre>"]);
    let lines: String = (0..400).map(|n| format!("line {n:04} <&>\n")).collect();
    for pre in [false, true] {
        let parts = chunks(&lines, pre);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(part.encode_utf16().count() <= LIMIT, "{}", part.len());
            let inner = if pre { part.strip_prefix("<pre>").unwrap().strip_suffix("</pre>").unwrap() } else { part };
            assert!(inner.ends_with('\n') || part == parts.last().unwrap(), "cut at a line end");
        }
        let joined: String = parts.iter().map(|p| p.trim_start_matches("<pre>").trim_end_matches("</pre>")).collect();
        assert_eq!(joined, escape(&lines), "nothing lost");
    }
    // One long line: cut inside it, never inside an escape, counting UTF-16 units.
    let wide = "😀&".repeat(3000);
    let parts = chunks(&wide, false);
    assert!(parts.iter().all(|p| p.encode_utf16().count() <= LIMIT && !p.ends_with("&am")));
    assert_eq!(parts.concat(), escape(&wide));
}

#[test]
fn a_paired_account_commands_and_gets_answers() {
    let mut f = fixture();
    let out = f.handle(said(ME, "/agents"));
    assert_eq!(texts(&out), ["t2 · claude · /w · blocked"]);
    let out = f.handle(said(ME, "/read t3 5"));
    assert_eq!(texts(&out), ["<pre>a &lt; b &amp;&amp; c &gt; d\nAPI_KEY=[redacted]</pre>"], "escaped and redacted");
    assert_eq!(texts(&f.handle(said(ME, "/send t3"))), ["Usage: /send &lt;t&gt; &lt;text&gt;"]);
    assert!(texts(&f.handle(said(ME, "/help")))[0].starts_with("Commands:\n/menu"));
    assert!(texts(&f.handle(said(ME, "hello")))[0].contains("reply to a notice"));
    f.acted();
    f.handle(said(ME, "/send t3 go on"));
    assert_eq!(f.acted(), [("terminal.send".to_string(), json!({"terminal": "t3", "text": "go on"}))]);
    if let [Out::Send { chat, .. }] = f.handle(said(ME, "/lock")).as_slice() {
        assert_eq!(*chat, ME);
    } else {
        panic!("one answer");
    }
    assert!(texts(&f.handle(said(ME, "/agents")))[0].starts_with("Remote control is locked"));
}

#[test]
fn strangers_and_groups_get_nothing_but_pairing_answers() {
    let mut f = fixture();
    for text in ["/agents", "/help", "hello", "/kill t1"] {
        assert!(f.handle(said(7, text)).is_empty(), "{text}");
    }
    assert!(f.handle(tapped(7, "a:t2")).is_empty());
    let mut group = said(ME, "/agents");
    group["message"]["chat"] = json!({"id": -100, "type": "supergroup"});
    assert!(f.handle(group).is_empty(), "a paired account in a group");
    let mut group_tap = tapped(ME, "a:t2");
    group_tap["callback_query"]["message"]["chat"]["type"] = "group".into();
    assert!(f.handle(group_tap).is_empty());
    assert!(f.handle(json!({"update_id": 3, "edited_message": {}})).is_empty());
    assert!(f.handle(said(7, "/pair WRONGONE")) == [text(7, "Not paired: wrong or expired code".into())]);
    assert!(f.acted().is_empty(), "the core never heard of them");

    let (code, _) = f.front.bridge.pair("telegram", T).unwrap();
    assert_eq!(texts(&f.handle(said(7, &format!("/pair {code}")))), ["Paired as vijay. Send /help for commands."]);
    assert!(!f.handle(said(7, "/agents")).is_empty(), "paired now");
}

#[test]
fn destructive_commands_ask_with_buttons_and_a_tap_confirms() {
    let mut f = fixture();
    let out = f.handle(said(ME, "/kill t1"));
    let [Out::Send { html, buttons, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(html, "Confirm: kill t1 (sh in /work)?");
    let data = &buttons[0][0].1;
    assert!(data.starts_with("c:") && buttons[0][1] == ("Cancel".into(), "x:".into()));
    assert!(f.acted().is_empty(), "nothing yet");

    assert!(f.handle(tapped(7, data)).is_empty(), "a stranger can't confirm");
    let out = f.handle(tapped(ME, data));
    assert_eq!(out[0], Out::Answer { callback: "q1".into() });
    assert_eq!(out[1], Out::Edit { chat: ME, message: 77, html: "done: kill t1 (sh in /work)".into() });
    assert_eq!(f.acted(), [("terminal.kill".to_string(), json!({"terminal": "t1"}))]);
    let again = f.handle(tapped(ME, data));
    assert!(matches!(&again[1], Out::Edit { html, .. } if html.starts_with("Failed: no such confirmation")));

    f.handle(said(ME, "/kill t1"));
    let out = f.handle(tapped(ME, "x:"));
    assert_eq!(out[1], Out::Edit { chat: ME, message: 77, html: "Cancelled.".into() });
    assert!(f.acted().is_empty());
}

#[test]
fn notices_come_with_buttons_and_replies_type_into_their_terminal() {
    let n = Notice {
        terminal: "t2".into(),
        agent: "claude".into(),
        folder: r"C:\work\a&b".into(),
        kind: Kind::NeedsYou,
        excerpt: "Allow <rm -rf build>?".into(),
    };
    let Out::Send { chat, html, buttons, about } = notice(&n, ME) else { panic!() };
    assert_eq!((chat, about.as_deref()), (ME, Some("t2")));
    assert_eq!(
        html,
        "<b>t2</b> needs you\nclaude · C:\\work\\a&amp;b\n\nAllow &lt;rm -rf build&gt;?\n\nReply to this message to type into t2."
    );
    let labels: Vec<Vec<&str>> = buttons.iter().map(|r| r.iter().map(|(l, _)| l.as_str()).collect()).collect();
    assert_eq!(labels, [vec!["Approve", "Deny"], vec!["Show", "Interrupt"]]);
    let Out::Send { buttons, .. } = notice(&Notice { kind: Kind::Finished, ..n }, ME) else { panic!() };
    assert_eq!(buttons, [vec![("Show".to_string(), "s:t2".to_string())]]);

    let mut f = fixture();
    let out = f.handle(tapped(ME, "a:t2"));
    assert_eq!(texts(&out)[1], "done: approve in t2: press enter");
    assert_eq!(f.acted()[0].1["keys"], json!(["enter"]));
    assert!(texts(&f.handle(tapped(ME, "s:t2")))[1].starts_with("<pre>"));
    f.acted();

    f.front.replies.lock().unwrap().remember((ME, 900), "t2".into());
    let mut reply = said(ME, "use the second option");
    reply["message"]["reply_to_message"] = json!({"message_id": 900});
    f.handle(reply.clone());
    assert_eq!(f.acted(), [("terminal.send".to_string(), json!({"terminal": "t2", "text": "use the second option"}))]);
    reply["message"]["reply_to_message"]["message_id"] = 901.into();
    assert!(texts(&f.handle(reply))[0].contains("reply to a notice"), "not a notice");
    assert!(f.acted().is_empty());
}

#[test]
fn only_the_last_thousand_notices_are_remembered() {
    let mut r = Replies::default();
    for id in 0..1001 {
        r.remember((ME, id), format!("t{id}"));
    }
    assert_eq!((r.terminals.len(), r.order.len()), (1000, 1000));
    assert!(!r.terminals.contains_key(&(ME, 0)) && r.terminals.contains_key(&(ME, 1000)));
}

#[test]
fn back_off_doubles_up_to_a_minute() {
    let secs: Vec<u64> = [0, 1, 2, 3, 6, 7, 8, 1000].into_iter().map(|n| backoff(n).as_secs()).collect();
    assert_eq!(secs, [1, 1, 2, 4, 32, 60, 60, 60]);
}

#[test]
fn the_token_is_kept_owner_only_and_checked() {
    let b = crate::bridge::scratch();
    assert_eq!(b.telegram_token().unwrap(), None);
    for bad in [
        "",
        "abc",
        "123:short",
        "x123456:ABCdef_ghi-JKLmnoPQRstuVWXyz",
        "123456:ABC def_ghi-JKLmnoPQRstuVWXyz",
        "123456:ABC\"def_ghi-JKLmnoPQRstuVW",
    ] {
        assert_eq!(b.set_telegram_token(bad).unwrap_err().kind, "invalid_params", "{bad:?}");
    }
    b.set_telegram_token(&format!("  {TOKEN}\n")).unwrap();
    assert_eq!(b.telegram_token().unwrap().as_deref(), Some(TOKEN));
    // Never plaintext in the folder, except the Linux no-keyring fallback, which is 0600.
    for entry in std::fs::read_dir(&b.dir).unwrap() {
        let path = entry.unwrap().path();
        if !std::fs::read(&path).unwrap().windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()) {
            continue;
        }
        #[cfg(any(windows, target_os = "macos"))]
        panic!("{path:?} holds the token");
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
    assert!(b.clear_telegram_token().unwrap());
    assert!(!b.clear_telegram_token().unwrap());
    assert_eq!(b.telegram_token().unwrap(), None);
    std::fs::remove_dir_all(&b.dir).unwrap();
}

#[test]
fn a_legacy_plaintext_token_is_moved_in_on_first_read() {
    let b = crate::bridge::scratch();
    std::fs::create_dir_all(&b.dir).unwrap();
    std::fs::write(b.dir.join("telegram.token"), format!("  {TOKEN}\r\n")).unwrap();
    assert_eq!(b.telegram_token().unwrap().as_deref(), Some(TOKEN), "read and trimmed");
    #[cfg(any(windows, target_os = "macos"))]
    {
        assert!(!b.dir.join("telegram.token").exists(), "the plaintext file is removed");
        for entry in std::fs::read_dir(&b.dir).unwrap() {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            assert!(!bytes.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()), "{path:?} holds the token");
        }
    }
    assert_eq!(b.telegram_token().unwrap().as_deref(), Some(TOKEN), "still there on the second read");
    assert!(b.clear_telegram_token().unwrap());
    assert_eq!(b.telegram_token().unwrap(), None);
    assert!(!b.dir.join("telegram.token").exists());
    std::fs::remove_dir_all(&b.dir).unwrap();
}

#[cfg(windows)]
#[test]
fn a_damaged_stored_token_is_an_error_without_the_token_and_can_be_cleared() {
    let b = crate::bridge::scratch();
    b.set_telegram_token(TOKEN).unwrap();
    std::fs::write(b.dir.join("telegram.token.dpapi"), b"not a blob").unwrap();
    let e = b.telegram_token().unwrap_err();
    assert!(!format!("{}{}", e.message, e.hint).contains(TOKEN), "the token is never in an error");
    b.clear_telegram_token().expect("--clear removes a damaged token like it removed any file before");
    assert!(!b.dir.join("telegram.token.dpapi").exists());
    assert_eq!(b.telegram_token().unwrap(), None);
    std::fs::remove_dir_all(&b.dir).unwrap();
}

/// A loopback Bot API: answers each request with `answer(method, body)` and
/// records (path, body).
struct Stub {
    url: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

fn stub(answer: impl Fn(&str, &Value) -> Value + Send + 'static) -> Stub {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
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
            let body: Value = serde_json::from_slice(&body).unwrap();
            let reply = answer(path.rsplit('/').next().unwrap_or_default(), &body).to_string();
            log.lock().unwrap().push((path, body));
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(format!("{head}{reply}").as_bytes());
        }
    });
    Stub { url, seen }
}

#[test]
fn polls_move_the_offset_and_answers_reach_telegram() {
    let counted = AtomicUsize::new(0);
    let server = stub(move |method, _| match method {
        "getUpdates" if counted.fetch_add(1, Ordering::SeqCst) == 0 => json!({"ok": true, "result": [
            {"update_id": 10, "message": {"message_id": 1, "from": {"id": ME}, "chat": {"id": ME, "type": "private"}, "text": "/agents"}},
            {"update_id": 11, "message": {"message_id": 2, "from": {"id": ME}, "chat": {"id": -5, "type": "group"}, "text": "/agents"}},
        ]}),
        "getUpdates" => json!({"ok": true, "result": []}),
        _ => json!({"ok": true, "result": {"message_id": 99}}),
    });
    let api = Api::at(&server.url, TOKEN);
    let mut f = fixture();
    let core = |method: &str, params: Value| fake_core(method, &params);
    let mut offset = 0;
    f.front.step(&api, &core, &mut offset).unwrap();
    assert_eq!(offset, 12);
    f.front.step(&api, &core, &mut offset).unwrap();
    assert_eq!(offset, 12, "nothing new");
    let seen = server.seen.lock().unwrap().clone();
    let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
    let bot = format!("/bot{TOKEN}");
    assert_eq!(paths, [format!("{bot}/getUpdates"), format!("{bot}/sendMessage"), format!("{bot}/getUpdates")]);
    assert_eq!((&seen[0].1["offset"], &seen[0].1["timeout"]), (&json!(0), &json!(50)));
    assert_eq!(seen[1].1, json!({"chat_id": ME, "text": "t2 · claude · /w · blocked", "parse_mode": "HTML"}));
    assert_eq!(seen[2].1["offset"], 12, "updates are acknowledged");
}

#[test]
fn a_notice_is_sent_with_its_keyboard_and_remembered() {
    let server = stub(|_, _| json!({"ok": true, "result": {"message_id": 314}}));
    let api = Api::at(&server.url, TOKEN);
    let replies = Mutex::new(Replies::default());
    let n = Notice {
        terminal: "t2".into(),
        agent: String::new(),
        folder: String::new(),
        kind: Kind::Error,
        excerpt: String::new(),
    };
    deliver(&api, &replies, notice(&n, ME)).unwrap();
    let (path, body) = server.seen.lock().unwrap()[0].clone();
    assert!(path.ends_with("/sendMessage"));
    assert_eq!(
        body["reply_markup"],
        json!({"inline_keyboard": [[{"text": "Show", "callback_data": "k:1"}, {"text": "Interrupt", "callback_data": "k:2"}]]}),
        "buttons carry short ids (spec change: Telegram takes 64 bytes of button data)"
    );
    let memory = replies.lock().unwrap();
    assert_eq!((memory.action("k:1"), memory.action("k:2")), (Some("s:t2".into()), Some("i:t2".into())));
    drop(memory);
    assert_eq!(replies.lock().unwrap().terminals.get(&(ME, 314)).map(String::as_str), Some("t2"));
    deliver(&api, &replies, Out::Answer { callback: "q9".into() }).unwrap();
    deliver(&api, &replies, Out::Edit { chat: ME, message: 314, html: "x".into() }).unwrap();
    let methods: Vec<String> =
        server.seen.lock().unwrap().iter().map(|(p, _)| p.rsplit('/').next().unwrap().to_string()).collect();
    assert_eq!(methods, ["sendMessage", "answerCallbackQuery", "editMessageText"]);
}

#[test]
fn refusals_and_dead_ends_are_errors_without_the_token() {
    let server = stub(|_, _| json!({"ok": false, "error_code": 401, "description": "Unauthorized"}));
    let mut f = fixture();
    let core = |method: &str, params: Value| fake_core(method, &params);
    let mut offset = 5;
    let err = f.front.step(&Api::at(&server.url, TOKEN), &core, &mut offset).unwrap_err();
    assert_eq!((err.as_str(), offset), ("getUpdates: Unauthorized", 5));
    // Nothing listens there: curl fails fast, and the error says only that.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let err = Api::at(&format!("http://{closed}"), TOKEN).call("getMe", &json!({}), 5).unwrap_err();
    assert!(err.starts_with("getMe: no answer") && !err.contains(TOKEN) && !err.contains("123456"), "{err}");
}

#[test]
fn exactly_4096_units_is_one_message_and_one_more_is_two() {
    for (pre, budget) in [(false, LIMIT), (true, LIMIT - "<pre></pre>".len())] {
        assert_eq!(chunks(&"a".repeat(budget), pre).len(), 1, "pre {pre}");
        let parts = chunks(&"a".repeat(budget + 1), pre);
        assert_eq!(parts.len(), 2, "pre {pre}");
        assert_eq!(parts[0].encode_utf16().count(), LIMIT, "pre {pre}: the first one is full");
        // An emoji (2 units) or an entity (5 units) that would cross the line moves whole.
        for tail in ["😀", "&"] {
            let parts = chunks(&format!("{}{tail}", "a".repeat(budget - 1)), pre);
            assert_eq!(parts.len(), 2, "pre {pre}, {tail}");
            assert!(parts.iter().all(|p| p.encode_utf16().count() <= LIMIT), "pre {pre}, {tail}");
            assert!(parts[1].contains(&escape(tail)), "pre {pre}, {tail}: {}", parts[1]);
        }
    }
    let script = chunks("<script>alert(1)</script> <b>x</b> &amp;", false);
    assert_eq!(script, ["&lt;script&gt;alert(1)&lt;/script&gt; &lt;b&gt;x&lt;/b&gt; &amp;amp;"]);
}

#[test]
fn a_cut_back_to_a_line_end_still_leaves_room_for_a_wide_character() {
    // The only line end is the first character: the cut keeps everything after
    // it, and the emoji or entity that did not fit must still fit afterwards.
    for (pre, budget) in [(false, LIMIT), (true, LIMIT - "<pre></pre>".len())] {
        for tail in ["😀", "&", "<"] {
            let text = format!("\n{}{tail}", "a".repeat(budget - 1));
            let parts = chunks(&text, pre);
            for part in &parts {
                assert!(
                    part.encode_utf16().count() <= LIMIT,
                    "pre {pre}, {tail}: {} units",
                    part.encode_utf16().count()
                );
            }
            let joined: String =
                parts.iter().map(|p| p.trim_start_matches("<pre>").trim_end_matches("</pre>")).collect();
            assert_eq!(joined, escape(&text), "pre {pre}, {tail}: nothing lost");
        }
    }
}

#[test]
fn edge_arguments_parse_or_ask_for_usage() {
    let t = |s: &str| s.to_string();
    let cases = [
        ("/read t3 0", Ask::Do(Command::Read { terminal: t("t3"), lines: 0 })),
        ("/read t3 99999", Ask::Do(Command::Read { terminal: t("t3"), lines: 99999 })),
        ("/send@h0x_bot t3  hi", Ask::Do(Command::Send { terminal: t("t3"), text: t("hi") })),
        ("/PAIR@h0x_bot ABCD", Ask::Pair(t("ABCD"))),
        ("/keys\tt1\tENTER", Ask::Do(Command::Keys { terminal: t("t1"), keys: vec![Key::Enter] })),
        (
            r#"/run "\\server\share\My App" claude --resume"#,
            Ask::Do(Command::Run { folder: t(r"\\server\share\My App"), command: t("claude --resume") }),
        ),
        (
            r#"/run "/home/me/my app"  npm test"#,
            Ask::Do(Command::Run { folder: t("/home/me/my app"), command: t("npm test") }),
        ),
    ];
    for (text, want) in cases {
        assert_eq!(parse(text), Some(want), "{text:?}");
    }
    for bad in [
        "/read t3 -1",
        "/read t3 5 6",
        "/read t3 18446744073709551616",
        "/keys",
        "/keys t1 enter,esc",
        "/send   ",
        "/interrupt",
        "/deny t2 now",
        r#"/run "C:\My App""#,
        r#"/run "C:\My App"   "#,
        r#"/run "" claude"#,
        "/run claude",
        "/@h0x_bot",
    ] {
        assert!(matches!(parse(bad), Some(Ask::Usage(_))), "{bad:?}");
    }
}

#[test]
fn read_line_counts_are_clamped_before_the_core_hears_them() {
    let mut f = fixture();
    for (asked, sent) in [("0", 1), ("1", 1), ("200", 200), ("201", 200), ("99999", 200)] {
        f.handle(said(ME, &format!("/read t3 {asked}")));
        let reads: Vec<Value> =
            f.acted().into_iter().filter(|(m, _)| m == "terminal.read").map(|(_, p)| p["lines"].clone()).collect();
        assert_eq!(reads, [json!(sent)], "/read t3 {asked}");
    }
    assert_eq!(texts(&f.handle(said(ME, "/read t3 -1"))), ["Usage: /read &lt;t&gt; [lines]"]);
    assert!(f.acted().is_empty());
}

#[test]
fn updates_without_a_numeric_sender_or_text_are_ignored() {
    let mut f = fixture();
    let private = json!({"id": ME, "type": "private"});
    for update in [
        json!({"update_id": 4}),
        json!({"update_id": 4, "edited_message": {"message_id": 5, "from": {"id": ME}, "chat": private, "text": "/kill t1"}}),
        json!({"update_id": 4, "channel_post": {"message_id": 5, "chat": {"id": -100, "type": "channel"}, "text": "/agents"}}),
        json!({"update_id": 4, "message": {"message_id": 5, "chat": private, "text": "/agents"}}),
        json!({"update_id": 4, "message": {"message_id": 5, "from": {"id": "42"}, "chat": private, "text": "/agents"}}),
        json!({"update_id": 4, "message": {"message_id": 5, "from": {"id": 7, "username": "vijay"}, "chat": private, "text": "/agents"}}),
        json!({"update_id": 4, "message": {"message_id": 5, "from": {"id": ME}, "chat": private, "caption": "/agents"}}),
        json!({"update_id": 4, "message": {"message_id": 5, "from": {"id": ME}, "chat": private, "text": null}}),
        json!({"update_id": 4, "callback_query": {"id": "q1", "from": {"id": ME}, "data": "a:t2"}}),
        json!({"update_id": 4, "callback_query": {"id": "q1", "from": {"id": ME},
            "message": {"message_id": 77, "chat": private}}}),
    ] {
        assert!(f.handle(update.clone()).is_empty(), "{update}");
    }
    assert!(f.acted().is_empty(), "the core never heard of them");
}

#[test]
fn unknown_or_oversized_button_data_is_only_acknowledged() {
    let mut f = fixture();
    for data in ["", ":", "zz:t2", "A:t2", "approve t2", "q"] {
        assert_eq!(f.handle(tapped(ME, data)), [Out::Answer { callback: "q1".into() }], "{data:?}");
    }
    // A made-up confirmation, far past Telegram's 64 bytes: answered, refused, nothing run.
    let out = f.handle(tapped(ME, &format!("c:{}", "Z".repeat(200))));
    assert_eq!(out[0], Out::Answer { callback: "q1".into() });
    assert!(matches!(&out[1], Out::Edit { html, .. } if html.starts_with("Failed: no such confirmation")), "{out:?}");
    assert!(f.acted().is_empty(), "nothing acted");
}

#[test]
fn a_confirmation_tapped_by_another_paired_account_does_nothing() {
    let mut f = fixture();
    let (code, _) = f.front.bridge.pair("telegram", T).unwrap();
    f.front.bridge.pair_verify("telegram", "7", "other", &code, T).unwrap();
    let out = f.handle(said(ME, "/kill t1"));
    let [Out::Send { buttons, .. }] = out.as_slice() else { panic!("{out:?}") };
    let data = buttons[0][0].1.clone();
    assert!(data.len() <= 64, "Telegram takes callback data up to 64 bytes: {data}");
    f.acted();

    let out = f.handle(tapped(7, &data));
    assert_eq!(out[0], Out::Answer { callback: "q1".into() });
    assert!(
        matches!(&out[1], Out::Edit { chat: 7, html, .. } if html.starts_with("Failed: no such confirmation")),
        "{out:?}"
    );
    assert!(f.acted().is_empty(), "nothing killed");
    let out = f.handle(tapped(ME, &data));
    assert_eq!(out[1], Out::Edit { chat: ME, message: 77, html: "done: kill t1 (sh in /work)".into() });
    assert_eq!(f.acted(), [("terminal.kill".to_string(), json!({"terminal": "t1"}))], "still the asker's");
}

#[test]
fn the_offset_never_moves_backwards_and_odd_results_are_harmless() {
    let counted = AtomicUsize::new(0);
    let server = stub(move |method, _| match (method, counted.fetch_add(1, Ordering::SeqCst)) {
        ("getUpdates", 0) => json!({"ok": true, "result": [{"update_id": 10}, {"update_id": 3}, {"update_id": "11"}]}),
        ("getUpdates", 1) => json!({"ok": true, "result": {"update_id": 50}}),
        ("getUpdates", _) => json!({"ok": true}),
        _ => json!({"ok": true, "result": {"message_id": 1}}),
    });
    let api = Api::at(&server.url, TOKEN);
    let mut f = fixture();
    let core = |method: &str, params: Value| fake_core(method, &params);
    let mut offset = 0;
    f.front.step(&api, &core, &mut offset).unwrap();
    assert_eq!(offset, 11, "the highest id plus one; a lower id after it does not pull it back");
    f.front.step(&api, &core, &mut offset).unwrap();
    assert_eq!(offset, 11, "a result that is not a list moves nothing");
    f.front.step(&api, &core, &mut offset).unwrap();
    assert_eq!(offset, 11, "no result at all moves nothing");
    let seen = server.seen.lock().unwrap();
    let offsets: Vec<&Value> = seen.iter().map(|(_, b)| &b["offset"]).collect();
    assert_eq!(offsets, [&json!(0), &json!(11), &json!(11)]);
    assert_eq!(seen[0].1["allowed_updates"], json!(["message", "callback_query"]));
}

#[test]
fn a_conflict_or_bad_token_is_reported_without_the_token() {
    for (code, description) in [
        (409, format!("Conflict: terminated by other getUpdates request at /bot{TOKEN}/getUpdates")),
        (401, format!("Unauthorized: /bot{TOKEN}")),
    ] {
        let server = stub(move |_, _| json!({"ok": false, "error_code": code, "description": description.clone()}));
        let mut f = fixture();
        let core = |method: &str, params: Value| fake_core(method, &params);
        let mut offset = 9;
        let err = f.front.step(&Api::at(&server.url, TOKEN), &core, &mut offset).unwrap_err();
        assert!(err.starts_with("getUpdates: ") && err.contains("/bot[token]"), "{code}: {err}");
        assert!(!err.contains(TOKEN) && !err.contains("ABCdef") && !err.contains("123456"), "{code}: {err}");
        assert_eq!(offset, 9, "{code}: nothing acknowledged");
    }
    let server = stub(|_, _| json!("not an object"));
    let err = Api::at(&server.url, TOKEN).call("getMe", &json!({}), 5).unwrap_err();
    assert_eq!(err, "getMe: refused");
}

#[test]
fn a_token_with_line_ends_or_the_wrong_shape_is_handled() {
    let b = crate::bridge::scratch();
    let secret19 = format!("123456:{}", "a".repeat(19));
    let secret20 = format!("123456:{}", "a".repeat(20));
    for bad in [
        secret19.as_str(),
        "123456:",
        ":ABCdef_ghi-JKLmnoPQRstuVWXyz0123",
        "123456:ABCdef_ghi-JKLmno:PQRstuVWXyz0123",
        "123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123\n123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123",
        "١٢٣:ABCdef_ghi-JKLmnoPQRstuVWXyz0123",
        "123456:ABCdef_ghi-JKLmnoPQRstuVWXyzé",
        "bot123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123",
        " \r\n ",
    ] {
        let err = b.set_telegram_token(bad).unwrap_err();
        assert_eq!(err.kind, "invalid_params", "{bad:?}");
        assert!(!err.message.contains("ABCdef") && !err.hint.contains("ABCdef"), "the error never echoes it");
        assert_eq!(b.telegram_token().unwrap(), None, "{bad:?}: nothing stored");
    }
    b.set_telegram_token(&secret20).unwrap();
    assert_eq!(b.telegram_token().unwrap(), Some(secret20));
    b.set_telegram_token(&format!("\t{TOKEN} \r\n")).unwrap();
    assert_eq!(b.telegram_token().unwrap().as_deref(), Some(TOKEN), "CRLF and blanks trimmed; the new one replaces");
    b.secret_put("telegram.token", b" \r\n").unwrap();
    assert_eq!(b.telegram_token().unwrap(), None, "a blank secret is no token");
    b.clear_telegram_token().unwrap();
    std::fs::remove_dir_all(&b.dir).unwrap();
}

#[test]
fn a_long_remote_address_fits_its_buttons_and_taps_find_it_again() {
    let address = format!("{}a/t7", "gpu-box-".repeat(7));
    assert_eq!(address.len(), 60);
    let server = stub(|_, _| json!({"ok": true, "result": {"message_id": 500}}));
    let mut f = fixture();
    let n = Notice {
        terminal: address.clone(),
        agent: "claude".into(),
        folder: "/srv".into(),
        kind: Kind::NeedsYou,
        excerpt: String::new(),
    };
    deliver(&Api::at(&server.url, TOKEN), &f.front.replies, notice(&n, ME)).unwrap();
    let body = server.seen.lock().unwrap()[0].1.clone();
    let data: Vec<String> = body["reply_markup"]["inline_keyboard"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row.as_array().unwrap().iter().map(|b| b["callback_data"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(data.len(), 4);
    assert!(data.iter().all(|d| d.starts_with("k:") && d.len() <= 64), "{data:?}");
    f.handle(tapped(ME, &data[3]));
    assert_eq!(f.acted(), [("terminal.interrupt".to_string(), json!({"terminal": address}))], "the Interrupt button");
    // An id never handed out, or long forgotten: acknowledged, nothing else.
    for gone in ["k:999", "k:", "k:x", "k:-1"] {
        assert_eq!(f.handle(tapped(ME, gone)), [Out::Answer { callback: "q1".into() }], "{gone}");
    }
    assert!(f.acted().is_empty());
}

#[test]
fn button_ids_are_bounded_like_the_notice_map() {
    let mut r = Replies::default();
    let first = r.button("a:t1".into());
    for n in 0..REMEMBERED {
        r.button(format!("s:t{n}"));
    }
    assert_eq!((r.buttons.len(), r.pressed.len()), (REMEMBERED, REMEMBERED));
    assert_eq!(r.action(&first), None, "the oldest went");
    assert_eq!(r.action(&format!("k:{}", REMEMBERED + 1)), Some(format!("s:t{}", REMEMBERED - 1)));
    assert_eq!(r.action("x:"), Some("x:".into()), "plain data stays as it is");
}

#[test]
fn commands_for_other_bots_and_messages_from_bots_are_ignored() {
    let mut f = fixture();
    for text in ["/agents@OtherBot", "/kill@other_bot t1", "/pair@OtherBot ABCDEFGH"] {
        assert!(f.handle(said(ME, text)).is_empty(), "{text}");
    }
    assert!(!f.handle(said(ME, "/agents@H0X_BOT")).is_empty(), "our own name, any case");
    assert!(!f.handle(said(ME, "/agents")).is_empty());
    assert!(!f.handle(said(ME, "mail me @OtherBot")).is_empty(), "plain text is not a command");
    assert!(for_another_bot("  /x@a b", "b") && !for_another_bot("/x", "b") && !for_another_bot("x@a", "b"));
    f.acted();

    let mut bot = said(ME, "/agents");
    bot["message"]["from"]["is_bot"] = true.into();
    assert!(f.handle(bot).is_empty(), "a bot with a paired id");
    let mut bot_tap = tapped(ME, "a:t2");
    bot_tap["callback_query"]["from"]["is_bot"] = true.into();
    assert!(f.handle(bot_tap).is_empty());
    let mut no_id = tapped(ME, "a:t2");
    no_id["callback_query"].as_object_mut().unwrap().remove("id");
    assert!(f.handle(no_id).is_empty(), "nothing to answer, nothing done");
    assert!(f.acted().is_empty(), "none of them reached the core");
}

#[test]
fn notices_go_to_every_paired_telegram_account_unless_locked() {
    let id = |n: u32, transport: &str, user: &str| crate::bridge::Identity {
        id: n,
        transport: transport.into(),
        user_id: user.into(),
        label: String::new(),
        paired_at_ms: T,
    };
    let ids = [
        id(1, "telegram", "42"),
        id(2, "relay", "43"),
        id(3, "telegram", "-7"),
        id(4, "telegram", "@bob"),
        id(5, "telegram", "9"),
    ];
    assert_eq!(notice_chats(false, &ids), [42, -7, 9]);
    assert!(notice_chats(true, &ids).is_empty(), "locked: nothing goes out");
    assert!(notice_chats(false, &[]).is_empty());
}

/// The buttons of the `n`th message: (label, action).
fn keyboard(out: &[Out], n: usize) -> Vec<(String, String)> {
    match &out[n] {
        Out::Send { buttons, .. } => buttons.iter().flatten().cloned().collect(),
        other => panic!("not a message: {other:?}"),
    }
}

#[test]
fn the_new_commands_parse_and_every_menu_entry_is_known() {
    for (text, want) in [
        ("/menu", Ask::Menu),
        ("/menu@h0x_bot", Ask::Menu),
        ("/SESSIONS", Ask::Sessions),
        ("/sessions extra words", Ask::Sessions),
        ("/projects", Ask::Projects),
        ("/folders", Ask::Projects),
    ] {
        assert_eq!(parse(text), Some(want), "{text:?}");
    }
    for (command, description) in MENU {
        assert!(!matches!(parse(&format!("/{command}")), Some(Ask::Usage("unknown command; send /help"))), "{command}");
        assert!((1..=32).contains(&command.len()) && (3..=256).contains(&description.len()), "{command}");
        assert!(command == "help" || HELP.contains(&format!("/{command}")), "/help lists /{command}");
    }
    assert_eq!(MENU.map(|(c, _)| c), ["menu", "sessions", "projects", "help", "agents", "read", "send", "lock"]);
}

#[test]
fn the_menu_has_three_buttons_that_work() {
    let mut f = fixture();
    let out = f.handle(said(ME, "/menu"));
    assert_eq!(
        keyboard(&out, 0),
        [("Sessions".into(), "m:s".into()), ("Projects".into(), "m:p".into()), ("Help".into(), "m:h".into())]
    );
    assert!(f.acted().is_empty(), "the menu itself asks the core nothing");
    let tap = f.handle(tapped(ME, "m:s"));
    assert_eq!(tap[0], Out::Answer { callback: "q1".into() });
    assert_eq!(keyboard(&tap, 1).len(), 4, "the sessions");
    assert_eq!(texts(&f.handle(tapped(ME, "m:h")))[1], escape(HELP));
    assert!(f.handle(tapped(ME, "m:p")).len() >= 2);
    assert_eq!(f.handle(tapped(ME, "m:zzz")), [Out::Answer { callback: "q1".into() }]);
}

#[test]
fn sessions_lists_live_and_finished_terminals_as_buttons() {
    let mut f = fixture();
    let out = f.handle(said(ME, "/sessions"));
    assert_eq!(f.acted(), [("terminal.list".to_string(), json!({"all": true}))]);
    assert_eq!(
        keyboard(&out, 0),
        [
            ("shell · bash · running".into(), "t:t1".into()),
            ("t2 · claude · blocked".into(), "t:t2".into()),
            ("api · claude · working".into(), "t:t3".into()),
            ("old · codex · exit 0".into(), "t:t9".into()),
        ]
    );
}

#[test]
fn tapping_a_session_selects_it_and_plain_text_goes_there_through_the_gates() {
    let mut f = fixture();
    // No selection: today's answer, nothing reaches the core.
    assert_eq!(
        texts(&f.handle(said(ME, "fix the test")))[0],
        "Send /help for commands, or reply to a notice to type into its terminal."
    );
    f.acted();
    let out = f.handle(tapped(ME, "t:t3"));
    assert_eq!(texts(&out)[1], "Selected api. Send a message to type it there.");
    assert_eq!(f.front.selected.get(&ME.to_string()).map(String::as_str), Some("t3"));
    f.acted();
    let out = f.handle(said(ME, "fix the <test>"));
    assert_eq!(texts(&out), ["done: type 14 characters into t3 and press enter"]);
    assert_eq!(f.acted(), [("terminal.send".to_string(), json!({"terminal": "t3", "text": "fix the <test>"}))]);
    // A command still works and keeps the selection; a reply to a notice wins over it.
    f.handle(said(ME, "/agents"));
    assert!(f.front.selected.contains_key(&ME.to_string()));
    f.front.replies.lock().unwrap().remember((ME, 5), "t2".into());
    let mut reply = said(ME, "yes");
    reply["message"]["reply_to_message"] = json!({"message_id": 5});
    f.acted();
    f.handle(reply);
    assert_eq!(f.acted()[0].1["terminal"], "t2");
    // A message that is not a reply goes to the selection even right after.
    f.handle(said(ME, "again"));
    assert_eq!(f.acted()[0].1["terminal"], "t3");
}

#[test]
fn a_selected_plain_shell_still_asks_for_confirmation() {
    let mut f = fixture();
    f.handle(tapped(ME, "t:t1"));
    f.acted();
    let out = f.handle(said(ME, "rm -rf build"));
    assert!(f.acted().is_empty(), "nothing typed before the tap");
    let Out::Send { buttons, .. } = &out[0] else { panic!("{out:?}") };
    assert_eq!(buttons[0][0].0, "Confirm");
}

#[test]
fn finished_unknown_and_unrelated_taps_select_nothing() {
    let mut f = fixture();
    let out = f.handle(tapped(ME, "t:t9"));
    assert_eq!(texts(&out)[1], "old has finished; there is nothing to type into.");
    let out = f.handle(tapped(ME, "t:t404"));
    assert_eq!(texts(&out)[1], "That session is gone.");
    assert!(f.front.selected.is_empty());
    // The selection belongs to the account that tapped.
    f.handle(tapped(ME, "t:t3"));
    assert_eq!(f.front.selected.len(), 1);
    assert_eq!(f.front.selected.keys().next().map(String::as_str), Some("42"));
}

#[test]
fn lock_unpair_and_a_gone_terminal_clear_the_selection() {
    // /lock from the chat.
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    f.handle(said(ME, "/lock"));
    assert!(f.front.selected.is_empty(), "/lock clears it");
    f.acted();
    assert_eq!(
        texts(&f.handle(said(ME, "hello")))[0],
        "Send /help for commands, or reply to a notice to type into its terminal."
    );
    assert!(f.acted().is_empty(), "locked: nothing is typed");

    // Locked at the machine: the selection is dropped and nothing is typed.
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    f.front.bridge.set_locked(true).unwrap();
    f.acted();
    f.handle(said(ME, "hello"));
    assert!(f.front.selected.is_empty());
    assert!(f.acted().is_empty());
    f.front.bridge.set_locked(false).unwrap();
    assert_eq!(
        texts(&f.handle(said(ME, "hello")))[0],
        "Send /help for commands, or reply to a notice to type into its terminal."
    );

    // Unpaired: dropped, and the stranger now gets nothing at all.
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    f.front.bridge.unpair(None).unwrap();
    f.acted();
    assert!(f.handle(said(ME, "hello")).is_empty());
    assert!(f.front.selected.is_empty());
    assert!(f.acted().is_empty());

    // The terminal ended: the next listing forgets it ...
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    let gone = |method: &str, _: Value| match method {
        "terminal.list" => {
            Ok(json!({"terminals": [{"address": "t3", "argv": ["claude"], "cwd": "/work", "state": "exited"}]}))
        }
        _ => Ok(json!({})),
    };
    f.front.handle(&gone, &said(ME, "/sessions"), T);
    assert!(f.front.selected.is_empty(), "ended terminals are not selected any more");

    // ... and so does a send the core refuses.
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    let refuses = |_: &str, _: Value| Err(RpcError::new("not_found", "no such terminal", ""));
    let out = f.front.handle(&refuses, &said(ME, "hello"), T);
    assert!(texts(&out)[0].starts_with("Failed: "), "{out:?}");
    assert!(f.front.selected.is_empty());
}

#[test]
fn a_locked_bridge_refuses_the_session_lists_too() {
    let mut f = fixture();
    f.front.bridge.set_locked(true).unwrap();
    f.acted();
    let out = f.handle(said(ME, "/sessions"));
    assert!(texts(&out)[0].starts_with("Remote control is locked"), "{out:?}");
    let out = f.handle(said(ME, "/projects"));
    assert!(texts(&out)[0].starts_with("Remote control is locked"), "{out:?}");
    assert!(f.acted().is_empty());
}

#[test]
fn projects_are_the_saved_spaces_plus_live_folders_and_a_tap_lists_that_folders_sessions() {
    let mut f = fixture();
    let spaces = r#"[{"name":"Work","folder":"/work"},{"name":"Old","folder":"C:\\Users\\me\\old"},{"name":"Empty","folder":"/nowhere"}]"#;
    assert_eq!(f.front.bridge.set_spaces(spaces.as_bytes()).unwrap(), 3);
    let out = f.handle(said(ME, "/folders"));
    // t2 lives in /w, which no space holds; t1 and t3 are inside /work, t9 in Old (finished).
    assert_eq!(
        keyboard(&out, 0),
        [
            ("Work (2)".into(), "p:/work".into()),
            ("Old (1)".into(), r"p:C:\Users\me\old".into()),
            ("Empty (0)".into(), "p:/nowhere".into()),
            ("w (1)".into(), "p:/w".into()),
        ]
    );
    let out = f.handle(tapped(ME, "p:/work"));
    assert_eq!(
        keyboard(&out, 1),
        [("shell · bash · running".into(), "t:t1".into()), ("api · claude · working".into(), "t:t3".into())],
        "inside the folder, a subfolder too"
    );
    assert!(texts(&out)[1].starts_with("Sessions in /work"));
    let out = f.handle(tapped(ME, "p:c:/USERS/me/old/"));
    assert_eq!(
        keyboard(&out, 1),
        [("old · codex · exit 0".into(), "t:t9".into())],
        "drive paths compare without case or separator"
    );
    assert_eq!(texts(&f.handle(tapped(ME, "p:/nowhere")))[1], "No sessions in that folder.");
    assert!(!within("/workshop", "/work"), "a folder name that merely starts the same");
    assert!(within("/work", "/work/") && within("/work/a/b", "/work"));
}

#[test]
fn without_spaces_projects_are_the_folders_of_live_terminals_only() {
    let mut f = fixture();
    let out = f.handle(said(ME, "/projects"));
    let labels: Vec<String> = keyboard(&out, 0).into_iter().map(|b| b.0).collect();
    assert_eq!(labels, ["work (2)", "w (1)"], "t9 is finished, and t3 is inside t1's folder");
    let none = |_: &str, _: Value| Ok(json!({"terminals": []}));
    assert_eq!(texts(&f.front.handle(&none, &said(ME, "/projects"), T)), ["No projects."]);
    assert_eq!(texts(&f.front.handle(&none, &said(ME, "/sessions"), T)), ["No sessions."]);
}

#[test]
fn long_lists_are_cut_to_what_one_message_can_hold() {
    let mut f = fixture();
    let many = |_: &str, _: Value| {
        let rows: Vec<Value> = (0..60)
            .map(|n| json!({"address": format!("t{n}"), "name": "n".repeat(120), "argv": ["sh"], "cwd": format!("/p/{n}"), "state": "live"}))
            .collect();
        Ok(json!({"terminals": rows}))
    };
    let out = f.front.handle(&many, &said(ME, "/sessions"), T);
    let keys = keyboard(&out, 0);
    assert_eq!(keys.len(), MAX_BUTTONS);
    assert!(keys.iter().all(|(label, _)| label.chars().count() <= 61));
    assert!(texts(&out)[0].contains("first 40 of 60"));
    let out = f.front.handle(&many, &said(ME, "/projects"), T);
    assert_eq!(keyboard(&out, 0).len(), MAX_BUTTONS);
}

#[test]
fn strangers_and_groups_get_nothing_from_the_new_commands() {
    let mut f = fixture();
    for text in ["/menu", "/sessions", "/projects", "/folders"] {
        assert!(f.handle(said(7, text)).is_empty(), "{text}: an unpaired account");
        let mut group = said(ME, text);
        group["message"]["chat"]["type"] = "group".into();
        assert!(f.handle(group).is_empty(), "{text}: a group");
    }
    for data in ["m:s", "m:p", "m:h", "t:t3", "p:/work"] {
        assert!(f.handle(tapped(7, data)).is_empty(), "{data}: an unpaired tap");
    }
    assert!(f.acted().is_empty() && f.front.selected.is_empty());
}

#[test]
fn the_command_menu_is_sent_once_and_a_failure_is_not_fatal() {
    let server = stub(|_, _| json!({"ok": true, "result": true}));
    set_menu(&Api::at(&server.url, TOKEN));
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, format!("/bot{TOKEN}/setMyCommands"));
    let commands = seen[0].1["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 8);
    assert_eq!(commands[0], json!({"command": "menu", "description": "Sessions, projects and help"}));
    assert!(!seen[0].1.to_string().contains(TOKEN));
    // Telegram says no, or nothing listens: logged, never a panic.
    let refused = stub(|_, _| json!({"ok": false, "error_code": 400, "description": "Bad Request"}));
    set_menu(&Api::at(&refused.url, TOKEN));
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    set_menu(&Api::at(&format!("http://{closed}"), TOKEN));
}

const NO_SELECTION: &str = "Send /help for commands, or reply to a notice to type into its terminal.";

/// The callback data of every button in a request body sent to Telegram.
fn sent_data(body: &Value) -> Vec<String> {
    body["reply_markup"]["inline_keyboard"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row.as_array().unwrap().iter().map(|b| b["callback_data"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn each_paired_account_has_its_own_selection_and_never_types_into_anothers() {
    let mut f = fixture();
    let (code, _) = f.front.bridge.pair("telegram", T).unwrap();
    let other = f.front.bridge.pair_verify("telegram", "7", "other", &code, T).unwrap();
    f.handle(tapped(ME, "t:t3"));
    f.acted();
    assert_eq!(texts(&f.handle(said(7, "hello")))[0], NO_SELECTION, "42's selection is not 7's");
    assert!(f.acted().is_empty(), "nothing typed anywhere");

    f.handle(tapped(7, "t:t2"));
    f.acted();
    f.handle(said(7, "for t2"));
    f.handle(said(ME, "for t3"));
    let sent: Vec<Value> = f.acted().into_iter().map(|(_, p)| p).collect();
    assert_eq!(sent, [json!({"terminal": "t2", "text": "for t2"}), json!({"terminal": "t3", "text": "for t3"})]);

    // A group message from a paired account with a selection types nothing.
    let mut group = said(ME, "in a group");
    group["message"]["chat"]["type"] = "supergroup".into();
    assert!(f.handle(group).is_empty());
    assert!(f.acted().is_empty());

    // Unpairing 7 drops only 7's selection.
    f.front.bridge.unpair(Some(other.id)).unwrap();
    assert!(f.handle(said(7, "still t2?")).is_empty(), "an unpaired account gets nothing");
    f.handle(said(ME, "again"));
    assert_eq!(f.acted(), [("terminal.send".to_string(), json!({"terminal": "t3", "text": "again"}))]);
    assert_eq!(f.front.selected.keys().collect::<Vec<_>>(), ["42"]);
}

#[test]
fn a_delivered_session_button_does_nothing_for_strangers_groups_or_bots() {
    let server = stub(|_, _| json!({"ok": true, "result": {"message_id": 500}}));
    let mut f = fixture();
    let out = f.handle(said(ME, "/sessions"));
    let api = Api::at(&server.url, TOKEN);
    for o in out {
        deliver(&api, &f.front.replies, o).unwrap();
    }
    let data = sent_data(&server.seen.lock().unwrap()[0].1);
    assert_eq!(data.len(), 4);
    let t3 = data[2].clone();
    assert!(t3.starts_with("k:") && t3.len() <= 64, "{t3}");
    f.acted();

    // A forwarded or copied button tapped by someone else is ignored outright.
    assert!(f.handle(tapped(7, &t3)).is_empty(), "a stranger");
    let mut group = tapped(ME, &t3);
    group["callback_query"]["message"]["chat"]["type"] = "group".into();
    assert!(f.handle(group).is_empty(), "the button forwarded into a group");
    let mut bot = tapped(ME, &t3);
    bot["callback_query"]["from"]["is_bot"] = true.into();
    assert!(f.handle(bot).is_empty(), "a bot with the paired id");
    assert!(f.front.selected.is_empty() && f.acted().is_empty());

    let out = f.handle(tapped(ME, &t3));
    assert_eq!(texts(&out)[1], "Selected api. Send a message to type it there.");
}

#[test]
fn after_unpairing_and_pairing_again_the_old_selection_is_gone() {
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    f.front.bridge.unpair(None).unwrap();
    let (code, _) = f.front.bridge.pair("telegram", T).unwrap();
    let out = f.handle(said(ME, &format!("/pair {code}")));
    assert!(texts(&out)[0].starts_with("Paired as"), "{out:?}");
    f.acted();
    assert_eq!(texts(&f.handle(said(ME, "hello")))[0], NO_SELECTION);
    assert!(f.acted().is_empty(), "nothing typed into the old selection");
}

#[test]
fn a_lock_and_unlock_at_the_machine_between_two_messages_still_clears_the_selection() {
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    // Locked and unlocked again before the bot hears anything: like a pending
    // confirmation (which dies with a lock), the selection must not survive.
    f.front.bridge.set_locked(true).unwrap();
    f.front.bridge.set_locked(false).unwrap();
    f.acted();
    assert_eq!(texts(&f.handle(said(ME, "hello")))[0], NO_SELECTION);
    assert!(f.acted().is_empty(), "nothing typed after a lock");
}

#[test]
fn a_refused_send_to_the_selection_clears_it_even_as_a_reply_to_a_plain_message() {
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    let refuses = |_: &str, _: Value| Err(RpcError::new("not_found", "no such terminal", ""));
    let mut reply = said(ME, "hello");
    reply["message"]["reply_to_message"] = json!({"message_id": 4242});
    let out = f.front.handle(&refuses, &reply, T);
    assert!(texts(&out)[0].starts_with("Failed: "), "{out:?}");
    assert!(f.front.selected.is_empty());
}

#[test]
fn session_lists_and_selected_text_are_audited_and_rate_limited() {
    let mut f = fixture();
    f.handle(said(ME, "/sessions"));
    f.handle(tapped(ME, "t:t3"));
    f.handle(said(ME, "secret plan API_KEY=abcdef0123456789"));
    let log = f.front.bridge.log(3).unwrap();
    let words: Vec<(&str, &str)> =
        log.iter().map(|e| (e["command"].as_str().unwrap(), e["result"].as_str().unwrap())).collect();
    assert_eq!(words, [("sessions", "done"), ("sessions", "done"), ("send", "done")], "{log:?}");
    assert!(log.iter().all(|e| e["identity"] == "telegram:42"), "{log:?}");
    assert_eq!(log[2]["terminal"], "t3");
    let lines = log.iter().map(Value::to_string).collect::<String>();
    assert!(!lines.contains("abcdef0123456789"), "no secret in the log: {lines}");
    assert_eq!(log[2]["chars"], 36);
    assert!(log[2]["text"].as_str().unwrap().ends_with('…'), "never the whole text: {lines}");

    // Three of thirty used; the rest goes to the selection, then everything is refused.
    for n in 3..30 {
        f.handle(said(ME, &format!("line {n}")));
    }
    f.acted();
    let refused = "Too many commands; try again in 60 s.";
    assert_eq!(texts(&f.handle(said(ME, "one more"))), [refused], "text to the selection");
    assert_eq!(texts(&f.handle(said(ME, "/sessions"))), [refused]);
    assert_eq!(texts(&f.handle(said(ME, "/projects"))), [refused]);
    assert_eq!(texts(&f.handle(tapped(ME, "t:t1")))[1], refused, "a tap on a session");
    assert!(f.acted().is_empty(), "the core heard nothing");
    assert_eq!(f.front.selected.get("42").map(String::as_str), Some("t3"), "a refusal keeps the selection");
    assert_eq!(f.front.bridge.log(1).unwrap()[0]["command"], "send", "refusals are not logged as run");

    // The limit is per account.
    let (code, _) = f.front.bridge.pair("telegram", T).unwrap();
    f.front.bridge.pair_verify("telegram", "7", "other", &code, T).unwrap();
    assert_eq!(keyboard(&f.handle(said(7, "/sessions")), 0).len(), 4);
}

#[test]
fn a_reply_to_an_unknown_message_goes_to_the_selection_and_a_failed_notice_reply_keeps_it() {
    let mut f = fixture();
    f.handle(tapped(ME, "t:t3"));
    f.acted();
    // A reply to something that is not a notice (the menu, a list): the selection.
    let mut reply = said(ME, "to the selection");
    reply["message"]["reply_to_message"] = json!({"message_id": 999});
    f.handle(reply);
    assert_eq!(f.acted()[0].1["terminal"], "t3");

    // A reply to a notice whose terminal is gone fails, but the selection stays.
    f.front.replies.lock().unwrap().remember((ME, 5), "t2".into());
    let mut reply = said(ME, "to the notice");
    reply["message"]["reply_to_message"] = json!({"message_id": 5});
    let refuses = |_: &str, _: Value| Err(RpcError::new("not_found", "no such terminal", ""));
    let out = f.front.handle(&refuses, &reply, T);
    assert!(texts(&out)[0].starts_with("Failed: "), "{out:?}");
    assert_eq!(f.front.selected.get("42").map(String::as_str), Some("t3"));
}

#[test]
fn long_unc_and_unicode_folders_and_addresses_fit_button_data_and_taps_find_them() {
    let folder = format!(r"\\{}\share\{}", "server-".repeat(20), "проект-ü".repeat(60));
    let address = format!("{}/t1", "gpu-box-".repeat(30));
    let space = serde_json::to_vec(&[json!({"name": "Ünïcode 项目 🚀".repeat(10), "folder": folder})]).unwrap();
    let server = stub(|_, _| json!({"ok": true, "result": {"message_id": 500}}));
    let api = Api::at(&server.url, TOKEN);
    let mut f = fixture();
    f.front.bridge.set_spaces(&space).unwrap();
    let inside = format!(r"{folder}\src\");
    let core = |method: &str, _: Value| -> Result<Value, RpcError> {
        Ok(match method {
            "terminal.list" => json!({"terminals": [
                {"address": address, "name": "агент", "argv": ["claude"], "cwd": inside, "state": "live"}]}),
            _ => json!({}),
        })
    };
    let replies = f.front.replies.clone();
    let press = |out: Vec<Out>| -> Vec<String> {
        for o in out {
            deliver(&api, &replies, o).unwrap();
        }
        let body = server.seen.lock().unwrap().last().unwrap().1.clone();
        let data = sent_data(&body);
        assert!(data.iter().all(|d| d.starts_with("k:") && d.len() <= 64), "{data:?}");
        let labels = body["reply_markup"]["inline_keyboard"].to_string();
        assert!(!labels.contains(&folder), "the folder itself never goes out as a label");
        data
    };

    let out = f.front.handle(&core, &said(ME, "/projects"), T);
    let label = &keyboard(&out, 0)[0].0;
    assert!(label.chars().count() <= 61 && label.ends_with('…'), "{label}");
    let projects = press(out);
    assert_eq!(projects.len(), 1, "the live terminal is inside the space");
    assert_eq!(f.front.replies.lock().unwrap().action(&projects[0]), Some(format!("p:{folder}")), "kept whole");

    let out = f.front.handle(&core, &tapped(ME, &projects[0]), T);
    let sessions = press(out);
    assert_eq!(sessions.len(), 1);
    let out = f.front.handle(&core, &tapped(ME, &sessions[0]), T);
    assert_eq!(texts(&out)[1], "Selected агент. Send a message to type it there.");
    assert_eq!(f.front.selected.get("42"), Some(&address));
}

#[test]
fn an_evicted_button_id_selects_nothing_and_ids_are_never_handed_out_twice() {
    let mut f = fixture();
    let first = f.front.replies.lock().unwrap().button("t:t3".into());
    let mut seen = std::collections::HashSet::from([first.clone()]);
    for _ in 0..REMEMBERED {
        let id = f.front.replies.lock().unwrap().button("t:t1".into());
        assert!(seen.insert(id), "a fresh id");
    }
    f.acted();
    assert_eq!(f.handle(tapped(ME, &first)), [Out::Answer { callback: "q1".into() }]);
    assert!(f.front.selected.is_empty(), "neither t3 nor what replaced it");
    assert!(f.acted().is_empty());
}

#[test]
fn folders_match_by_whole_parts_with_case_ignored_only_on_windows_paths() {
    for (path, folder, want) in [
        (r"C:\proj\src", r"C:\proj", true),
        (r"C:\project", r"C:\proj", false),
        (r"C:\proj", r"C:\project", false),
        (r"c:/PROJ/", r"C:\proj", true),
        (r"C:/proj\sub\deeper\", r"C:\proj\", true),
        (r"C:\proj", r"C:\proj\\", true),
        (r"D:\proj", r"C:\proj", false),
        (r"C:\anything", r"C:\", true),
        ("/Work/a", "/work", false),
        ("/work", "/Work", false),
        ("/work//", "/work", true),
        ("/workshop/x", "/work", false),
        ("/anything", "/", true),
        ("/home/jäne/项目/a", "/home/jäne/项目", true),
        ("/home/jäne/项目x", "/home/jäne/项目", false),
        (r"\\server\share\app", r"\\server\share", true),
        (r"\\server\shared", r"\\server\share", false),
        (r"\\SERVER\share\app", r"\\server\share", true),
        (r"\\?\C:\Ü\x", r"\\?\c:\ü", true),
        (r"C:\Ü\x", r"c:\ü", true),
        ("/Ü/x", "/ü", false),
        ("", "/work", false),
    ] {
        assert_eq!(within(path, folder), want, "{path:?} in {folder:?}");
    }
}

#[test]
fn duplicate_spaces_and_unicode_names_are_listed_as_given() {
    let mut f = fixture();
    let spaces = r#"[{"name":"Ünïcode 项目 🚀","folder":"C:\\Users\\Jane Doe\\项目"},
        {"name":"Again","folder":"c:/users/jane doe/项目/"},{"name":"Work","folder":"/work"},{"name":"Work","folder":"/work"}]"#;
    assert_eq!(f.front.bridge.set_spaces(spaces.as_bytes()).unwrap(), 4);
    let core = |method: &str, _: Value| -> Result<Value, RpcError> {
        Ok(match method {
            "terminal.list" => json!({"terminals": [
                {"address": "t1", "argv": ["sh"], "cwd": "C:\\Users\\Jane Doe\\项目\\src", "state": "live"},
                {"address": "t2", "argv": ["sh"], "cwd": "/work/a", "state": "live"}]}),
            _ => json!({}),
        })
    };
    let out = f.front.handle(&core, &said(ME, "/projects"), T);
    assert_eq!(
        keyboard(&out, 0),
        [
            ("Ünïcode 项目 🚀 (1)".into(), r"p:C:\Users\Jane Doe\项目".into()),
            ("Again (1)".into(), "p:c:/users/jane doe/项目/".into()),
            ("Work (1)".into(), "p:/work".into()),
            ("Work (1)".into(), "p:/work".into()),
        ],
        "no extra button for a folder a space already holds"
    );
}

#[test]
fn lists_of_39_and_40_are_whole_and_41_is_cut_to_40() {
    for (n, title) in [(39, "Sessions"), (40, "Sessions"), (41, "Sessions (first 40 of 41)")] {
        let mut f = fixture();
        let many = move |_: &str, _: Value| {
            let rows: Vec<Value> = (0..n)
                .map(|i| json!({"address": format!("t{i}"), "argv": ["sh"], "cwd": "/p", "state": "live"}))
                .collect();
            Ok(json!({"terminals": rows}))
        };
        let out = f.front.handle(&many, &said(ME, "/sessions"), T);
        assert_eq!(texts(&out), [title]);
        assert_eq!(keyboard(&out, 0).len(), n.min(MAX_BUTTONS), "{n}");
        assert_eq!(keyboard(&out, 0).last().unwrap().1, format!("t:t{}", n.min(MAX_BUTTONS) - 1), "the first ones");
    }
    for (n, title) in [(39, "Projects"), (40, "Projects"), (41, "Projects (first 40 of 41)")] {
        let mut f = fixture();
        let spaces: Vec<Value> = (0..n).map(|i| json!({"name": format!("s{i}"), "folder": format!("/p{i}")})).collect();
        f.front.bridge.set_spaces(&serde_json::to_vec(&spaces).unwrap()).unwrap();
        let none = |_: &str, _: Value| Ok(json!({"terminals": []}));
        let out = f.front.handle(&none, &said(ME, "/projects"), T);
        assert_eq!(texts(&out), [title]);
        assert_eq!(keyboard(&out, 0).len(), n.min(MAX_BUTTONS), "{n}");
    }
}

#[test]
fn an_empty_stored_spaces_list_and_no_terminals_say_no_projects() {
    let mut f = fixture();
    assert_eq!(f.front.bridge.set_spaces(b"[]").unwrap(), 0);
    let none = |_: &str, _: Value| Ok(json!({"terminals": []}));
    assert_eq!(texts(&f.front.handle(&none, &said(ME, "/projects"), T)), ["No projects."]);
    // Only finished terminals, or live ones without a folder: still none.
    let idle = |_: &str, _: Value| {
        Ok(json!({"terminals": [
            {"address": "t1", "argv": ["sh"], "cwd": "/old", "state": "exited"},
            {"address": "t2", "argv": ["sh"], "state": "live"}]}))
    };
    assert_eq!(texts(&f.front.handle(&idle, &said(ME, "/projects"), T)), ["No projects."]);
}

#[test]
fn every_command_in_help_parses_and_menu_names_suit_telegram() {
    let named: Vec<&str> = HELP
        .split_whitespace()
        .map(|w| w.trim_matches(|c| matches!(c, '(' | ')' | ',' | '.')))
        .filter(|w| w.len() > 1 && w.starts_with('/') && w[1..].bytes().all(|b| b.is_ascii_lowercase()))
        .collect();
    assert!(named.len() >= 14, "{named:?}");
    for command in named {
        assert_ne!(parse(command), Some(Ask::Usage("unknown command; send /help")), "{command} is in /help");
    }
    for (command, _) in MENU {
        assert!(command.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'), "{command}");
    }
    let mut names: Vec<&str> = MENU.map(|(c, _)| c).to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), MENU.len(), "no command twice");
}
