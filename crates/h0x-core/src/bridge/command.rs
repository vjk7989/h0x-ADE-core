//! What a paired account can ask for, and how each request runs against the core.

use super::{Bridge, Identity, code, failed, redact};
use crate::cli::output::{agent, cwd, status};
use crate::rpc::RpcError;
use serde_json::{Value, json};
use std::io::Write;

/// Most lines one read returns.
const MAX_LINES: usize = 200;
const CONFIRM_MS: u64 = 2 * 60_000;
const WINDOW_MS: u64 = 60_000;

/// Who sent a command: the transport, and the account's id there.
#[derive(Clone, Debug, PartialEq)]
pub struct Sender {
    pub transport: String,
    pub user_id: String,
}

impl Sender {
    fn key(&self) -> String {
        format!("{}:{}", self.transport, self.user_id)
    }
}

/// The keys a remote person can press.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Enter,
    Esc,
    CtrlC,
    Up,
    Down,
    Left,
    Right,
    Y,
    N,
}

impl Key {
    /// Its name for `terminal.keys`.
    pub fn name(self) -> &'static str {
        match self {
            Key::Enter => "enter",
            Key::Esc => "esc",
            Key::CtrlC => "ctrl+c",
            Key::Up => "up",
            Key::Down => "down",
            Key::Left => "left",
            Key::Right => "right",
            Key::Y => "y",
            Key::N => "n",
        }
    }

    /// A key a person typed (`enter`, `Esc`, `ctrl+c`, `up`, `y`...), else None.
    pub fn parse(word: &str) -> Option<Key> {
        let all = [Key::Enter, Key::Esc, Key::CtrlC, Key::Up, Key::Down, Key::Left, Key::Right, Key::Y, Key::N];
        let word = word.to_ascii_lowercase();
        let word = match word.as_str() {
            "return" => "enter",
            "escape" => "esc",
            "ctrl-c" | "^c" => "ctrl+c",
            other => other,
        };
        all.into_iter().find(|k| k.name() == word)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Live terminals: address, name, agent, folder, status.
    List,
    /// The last `lines` lines of a terminal (1-200), redacted.
    Read {
        terminal: String,
        lines: usize,
    },
    /// Paste text and press enter.
    Send {
        terminal: String,
        text: String,
    },
    Keys {
        terminal: String,
        keys: Vec<Key>,
    },
    /// Answer the prompt a terminal is blocked on: yes.
    Approve {
        terminal: String,
    },
    /// Answer it: no.
    Deny {
        terminal: String,
    },
    /// Press the stop key and report what happened.
    Interrupt {
        terminal: String,
    },
    /// Start an agent, template or shell command in a folder (a full path here).
    Run {
        folder: String,
        command: String,
    },
    Kill {
        terminal: String,
    },
    /// Refuse every remote command until `h0x-core bridge unlock` on this machine.
    Lock,
    /// Run what a `NeedsConfirm` reply described.
    Confirm {
        token: String,
    },
}

impl Command {
    fn terminal(&self) -> Option<&str> {
        match self {
            Command::Read { terminal, .. }
            | Command::Send { terminal, .. }
            | Command::Keys { terminal, .. }
            | Command::Approve { terminal }
            | Command::Deny { terminal }
            | Command::Interrupt { terminal }
            | Command::Kill { terminal } => Some(terminal),
            Command::List | Command::Run { .. } | Command::Lock | Command::Confirm { .. } => None,
        }
    }

    /// What the audit log keeps: never a whole text, never a token.
    fn audit(&self) -> Value {
        let word = match self {
            Command::List => "list",
            Command::Read { .. } => "read",
            Command::Send { .. } => "send",
            Command::Keys { .. } => "keys",
            Command::Approve { .. } => "approve",
            Command::Deny { .. } => "deny",
            Command::Interrupt { .. } => "interrupt",
            Command::Run { .. } => "run",
            Command::Kill { .. } => "kill",
            Command::Lock => "lock",
            Command::Confirm { .. } => "confirm",
        };
        let mut entry = json!({"command": word});
        if let Some(terminal) = self.terminal() {
            entry["terminal"] = brief(terminal).into();
        }
        match self {
            Command::Send { text, .. } | Command::Run { command: text, .. } => {
                entry["text"] = brief(text).into();
                entry["chars"] = text.chars().count().into();
            }
            _ => {}
        }
        match self {
            Command::Run { folder, .. } => entry["folder"] = brief(folder).into(),
            Command::Keys { keys, .. } => entry["keys"] = keys.iter().map(|k| k.name()).collect(),
            Command::Read { lines, .. } => entry["lines"] = (*lines).into(),
            _ => {}
        }
        entry
    }
}

#[derive(Debug, PartialEq)]
pub enum Reply {
    /// It ran: what happened (redacted).
    Done(String),
    /// Destructive: `Command::Confirm { token }` from the same account within
    /// 2 minutes runs `what`. Single use.
    NeedsConfirm {
        token: String,
        what: String,
    },
    /// This account isn't paired.
    NotPaired,
    /// Remote control is locked.
    Locked,
    RateLimited {
        retry_secs: u64,
    },
    /// The core refused it, or it made no sense (redacted).
    Failed(String),
}

impl Reply {
    fn word(&self) -> &'static str {
        match self {
            Reply::Done(_) => "done",
            Reply::NeedsConfirm { .. } => "needs_confirm",
            Reply::NotPaired => "not_paired",
            Reply::Locked => "locked",
            Reply::RateLimited { .. } => "rate_limited",
            Reply::Failed(_) => "failed",
        }
    }
}

/// One core call, decided: what it does in words.
pub(super) struct Planned {
    method: &'static str,
    params: Value,
    what: String,
    /// An approve or deny: its terminal's status is checked again when it runs.
    answer: bool,
}

/// A destructive command waiting for its confirmation. It is void once the
/// account is unpaired or paired again (a new identity), or after any lock
/// (ADR-061 addendum).
pub(super) struct Waiting {
    identity: Identity,
    locks: u64,
    planned: Planned,
    expires_ms: u64,
}

/// Whether what runs in the terminal now is a plain shell: its foreground
/// program, else the program it was started with.
fn at_shell(t: &Value) -> bool {
    let program = [&t["foreground"]["name"], &t["foreground"]["argv"][0], &t["argv"][0]]
        .into_iter()
        .filter_map(Value::as_str)
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let name = program.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    ["cmd", "powershell", "pwsh", "bash", "sh", "zsh", "fish", "nu", "dash", "ash", "ksh", "csh", "tcsh", "xonsh"]
        .contains(&name)
}

/// A remote yes or no answers a blocked terminal only when it asks for a permission. A
/// question or a sign-in is the person's to answer in the terminal; a missing or unknown
/// kind counts as one of those. (Not blocked: nothing here to refuse.)
fn answerable(t: &Value, address: &str) -> Result<(), RpcError> {
    if t["status"]["state"] != "blocked" {
        return Ok(());
    }
    let asks = match t["status"]["kind"].as_str() {
        Some("permission") => return Ok(()),
        Some("question") => "a question",
        Some("auth") => "a sign-in",
        _ => "a request of unknown kind",
    };
    Err(failed(format!("{address} is blocked on {asks}: answer it in the terminal, not from here")))
}

/// A text for the log: redacted, then cut short.
fn brief(text: &str) -> String {
    let text = redact(text);
    match text.char_indices().nth(24) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}

impl Bridge {
    /// Runs one command from `from` against the core `call` reaches: rate
    /// limit, then the allow-list, then the lock. Every command that gets
    /// past the rate limit is audited; none runs when the log can't be written.
    pub fn execute(
        &mut self,
        call: &impl Fn(&str, Value) -> Result<Value, RpcError>,
        from: &Sender,
        command: Command,
        now: u64,
    ) -> Reply {
        let key = from.key();
        if let Err(retry_secs) = self.admit(&key, now) {
            return Reply::RateLimited { retry_secs };
        }
        // A summary of held-back failures that is due goes before this command's line.
        if let Err(e) = self.settle(&mut self.failures.lock().unwrap(), now) {
            return Reply::Failed(format!("the audit log can't be written: {}", e.message));
        }
        let mut log = match self.rotate_audit(super::AUDIT_MAX).and_then(|()| self.audit_file()) {
            Ok(file) => file,
            Err(e) => return Reply::Failed(format!("the audit log can't be written: {e}")),
        };
        let (reply, what) = match self.load() {
            Err(e) => (Reply::Failed(e.message), None),
            Ok(st) => match st.identities.iter().find(|i| i.transport == from.transport && i.user_id == from.user_id) {
                None => (Reply::NotPaired, None),
                Some(_) if st.locked => (Reply::Locked, None),
                Some(me) => {
                    let me = me.clone();
                    self.run(call, &me, st.locks, &command, now)
                }
            },
        };
        let mut entry = command.audit();
        (entry["time_ms"], entry["identity"], entry["result"]) = (now.into(), key.into(), reply.word().into());
        if let Some(what) = what {
            entry["what"] = what.into();
        }
        if let Reply::Failed(message) = &reply {
            entry["error"] = message.as_str().into();
        }
        match log.write_all(format!("{entry}\n").as_bytes()) {
            Ok(()) => reply,
            Err(e) => Reply::Failed(format!("{} ran, but the audit log can't be written: {e}", reply.word())),
        }
    }

    /// Room for one more command from `key` this minute, else seconds until there is.
    fn admit(&mut self, key: &str, now: u64) -> Result<(), u64> {
        let times = self.recent.entry(key.into()).or_default();
        while times.front().is_some_and(|t| now.saturating_sub(*t) >= WINDOW_MS) {
            times.pop_front();
        }
        if times.len() >= self.config.rate_limit() as usize {
            return Err(times.front().map_or(60, |t| (t + WINDOW_MS - now).div_ceil(1000)));
        }
        times.push_back(now);
        Ok(())
    }

    /// The reply, and for the log what a confirmation asks for or runs.
    fn run(
        &mut self,
        call: &impl Fn(&str, Value) -> Result<Value, RpcError>,
        me: &Identity,
        locks: u64,
        command: &Command,
        now: u64,
    ) -> (Reply, Option<String>) {
        let done = |result: Result<String, RpcError>| match result {
            Ok(text) => Reply::Done(redact(&text)),
            Err(e) => Reply::Failed(redact(&e.message)),
        };
        match command {
            Command::Lock => {
                self.tokens.clear();
                let locked = self.set_locked(true).map(|()| {
                    "locked: remote commands are refused until `h0x-core bridge unlock` on that machine".into()
                });
                (done(locked), None)
            }
            Command::List => (done(list(call)), None),
            Command::Read { terminal, lines } => (done(read(call, terminal, *lines)), None),
            Command::Confirm { token } => match self.take(me, locks, token, now) {
                Some(planned) => {
                    let what = redact(&planned.what);
                    // The status may have changed since the person was asked.
                    let still = match planned.answer {
                        true => {
                            let address = planned.params["terminal"].as_str().unwrap_or_default();
                            call("terminal.get", json!({"terminal": address})).and_then(|t| answerable(&t, address))
                        }
                        false => Ok(()),
                    };
                    (done(still.and_then(|()| perform(call, planned))), Some(what))
                }
                None => {
                    (Reply::Failed("no such confirmation: used, expired (2 minutes), voided or not yours".into()), None)
                }
            },
            command => match plan(call, command) {
                Err(e) => (Reply::Failed(redact(&e.message)), None),
                Ok((planned, false)) => (done(perform(call, planned)), None),
                Ok((planned, true)) => {
                    let token = code();
                    let what = redact(&planned.what);
                    self.tokens.retain(|_, w| now < w.expires_ms);
                    let waiting = Waiting { identity: me.clone(), locks, planned, expires_ms: now + CONFIRM_MS };
                    self.tokens.insert(token.clone(), waiting);
                    (Reply::NeedsConfirm { token, what: what.clone() }, Some(what))
                }
            },
        }
    }

    /// The command `token` holds, once, for the identity it was given to,
    /// if no lock came in between.
    fn take(&mut self, me: &Identity, locks: u64, token: &str, now: u64) -> Option<Planned> {
        let token = token.trim().to_ascii_uppercase();
        if self.tokens.get(&token)?.identity != *me {
            return None;
        }
        let waiting = self.tokens.remove(&token)?;
        (now < waiting.expires_ms && waiting.locks == locks).then_some(waiting.planned)
    }
}

fn list(call: &impl Fn(&str, Value) -> Result<Value, RpcError>) -> Result<String, RpcError> {
    let r = call("terminal.list", json!({}))?;
    let rows: Vec<String> = r["terminals"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let name = t["name"].as_str().map(|n| format!(" {n}")).unwrap_or_default();
            let address = t["address"].as_str().unwrap_or("?");
            format!("{address}{name} · {} · {} · {}", agent(t), cwd(t).unwrap_or("?"), status(t).0)
        })
        .collect();
    Ok(if rows.is_empty() { "no live terminals".into() } else { rows.join("\n") })
}

fn read(
    call: &impl Fn(&str, Value) -> Result<Value, RpcError>,
    terminal: &str,
    lines: usize,
) -> Result<String, RpcError> {
    let lines = lines.clamp(1, MAX_LINES);
    let r = call("terminal.read", json!({"terminal": terminal, "lines": lines}))?;
    let text: Vec<&str> = r["text"].as_str().unwrap_or_default().trim_end().lines().collect();
    Ok(text[text.len().saturating_sub(lines)..].join("\n"))
}

/// The core call a command makes, and whether a person must confirm it
/// first: kill and run always; text or keys into a program that doesn't
/// report status over OSC 7501 (a plain shell) too.
fn plan(
    call: &impl Fn(&str, Value) -> Result<Value, RpcError>,
    command: &Command,
) -> Result<(Planned, bool), RpcError> {
    let planned = |method, params, what: String| Planned { method, params, what, answer: false };
    if let Command::Run { folder, command } = command {
        if command.trim().is_empty() {
            return Err(failed("nothing to run"));
        }
        if !std::path::Path::new(folder).is_absolute() {
            return Err(failed(format!("{folder:?} is not a full path on this machine")));
        }
        let argv = vec![command.clone()];
        let mut params = match crate::cli::shell_line(&argv) {
            Some(line) => json!({"line": line}),
            None => json!({"argv": argv}),
        };
        (params["cwd"], params["no_parent"]) = (folder.as_str().into(), true.into());
        return Ok((planned("terminal.run", params, format!("run `{command}` in {folder}")), true));
    }
    let terminal = command.terminal().expect("run, list, read, lock and confirm are handled before");
    let t = call("terminal.get", json!({"terminal": terminal}))?;
    let address = t["address"].as_str().unwrap_or(terminal).to_string();
    // Reports its status: an agent, answering its own dialogs. A plain shell
    // can print the same report, so it never counts.
    let reports = t["reported"] == true && !at_shell(&t);
    let press = |names: &[&str]| json!({"terminal": address, "keys": names});
    Ok(match command {
        Command::Send { text, .. } => {
            if text.is_empty() {
                return Err(failed("nothing to send"));
            }
            let what = format!("type {} characters into {address} and press enter", text.chars().count());
            (planned("terminal.send", json!({"terminal": address, "text": text}), what), !reports)
        }
        Command::Keys { keys, .. } => {
            if keys.is_empty() {
                return Err(failed("no keys to press"));
            }
            let names: Vec<&str> = keys.iter().map(|k| k.name()).collect();
            (planned("terminal.keys", press(&names), format!("press {} in {address}", names.join(" "))), !reports)
        }
        Command::Approve { .. } | Command::Deny { .. } => {
            let yes = matches!(command, Command::Approve { .. });
            let state = t["status"]["state"].as_str().unwrap_or("no status");
            if reports && state != "blocked" {
                return Err(failed(format!("{address} is not waiting on you (it is {state})")));
            }
            // A status-reporting dialog: enter takes the highlighted first
            // choice (yes), esc cancels. Anything else: a y/n question.
            let names: &[&str] = match (reports, yes) {
                (true, true) => &["enter"],
                (true, false) => &["esc"],
                (false, true) => &["y", "enter"],
                (false, false) => &["n", "enter"],
            };
            answerable(&t, &address)?;
            let what = format!("{} in {address}: press {}", if yes { "approve" } else { "deny" }, names.join(" "));
            (Planned { answer: true, ..planned("terminal.keys", press(names), what) }, !reports)
        }
        Command::Interrupt { .. } => {
            (planned("terminal.interrupt", json!({"terminal": address}), format!("interrupt {address}")), false)
        }
        Command::Kill { .. } => {
            let what = format!("kill {address} ({} in {})", agent(&t), cwd(&t).unwrap_or("?"));
            (planned("terminal.kill", json!({"terminal": address}), what), true)
        }
        Command::List | Command::Read { .. } | Command::Run { .. } | Command::Lock | Command::Confirm { .. } => {
            unreachable!("handled before")
        }
    })
}

fn perform(call: &impl Fn(&str, Value) -> Result<Value, RpcError>, planned: Planned) -> Result<String, RpcError> {
    let r = call(planned.method, planned.params)?;
    Ok(match planned.method {
        "terminal.run" => format!("started {}", r["address"].as_str().unwrap_or("?")),
        "terminal.interrupt" => format!("{}: {}", planned.what, r["outcome"].as_str().unwrap_or("?")),
        _ => format!("done: {}", planned.what),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BridgeConfig;
    use std::cell::RefCell;

    const T: u64 = 5_000_000;

    /// A paired bridge (`telegram:42`) and a fake core: t1 a plain shell, t2
    /// an agent blocked on a question, t3 an agent at work.
    struct Fixture {
        bridge: Bridge,
        calls: RefCell<Vec<(String, Value)>>,
        /// The `kind` of what k1 (an agent) and k2 (a shell printing a report) are blocked on.
        kind: RefCell<Value>,
    }

    fn me() -> Sender {
        Sender { transport: "telegram".into(), user_id: "42".into() }
    }

    fn fixture(config: BridgeConfig) -> Fixture {
        let mut bridge = crate::bridge::scratch();
        bridge.config = config;
        let (code, _) = bridge.pair("telegram", T).unwrap();
        bridge.pair_verify("telegram", "42", "me", &code, T).unwrap();
        Fixture { bridge, calls: RefCell::new(Vec::new()), kind: RefCell::new(Value::Null) }
    }

    impl Fixture {
        fn send(&mut self, command: Command, now: u64) -> Reply {
            self.send_as(&me(), command, now)
        }

        fn send_as(&mut self, from: &Sender, command: Command, now: u64) -> Reply {
            let (calls, kind) = (&self.calls, &self.kind);
            let core = |method: &str, params: Value| -> Result<Value, RpcError> {
                calls.borrow_mut().push((method.into(), params.clone()));
                let t = params["terminal"].as_str().unwrap_or_default();
                let record = |address: &str, state: Option<&str>| {
                    let status = state.map_or(Value::Null, |state| json!({"state": state, "app": "claude-code"}));
                    // The terminal was started as a shell; a reporting one runs an agent now.
                    let front = if state.is_some() { "claude" } else { "bash" };
                    json!({"address": address, "state": "live", "argv": ["/bin/sh"], "cwd": "/work",
                           "foreground": {"name": front, "argv": [front], "cwd": "/work"},
                           "reported": state.is_some(), "status": status})
                };
                Ok(match (method, t) {
                    ("terminal.get", "t1") => record(t, None),
                    ("terminal.get", "t2") => {
                        let mut r = record(t, Some("blocked"));
                        r["status"]["kind"] = "permission".into();
                        r
                    }
                    ("terminal.get", "k1") => {
                        let mut r = record(t, Some("blocked"));
                        r["status"]["kind"] = kind.borrow().clone();
                        r
                    }
                    ("terminal.get", "k2") => {
                        json!({"address": t, "state": "live", "argv": ["pwsh.exe"], "cwd": "/work",
                               "foreground": {"name": "pwsh.exe", "argv": ["pwsh.exe"], "cwd": "/work"},
                               "reported": true, "status": {"state": "blocked", "kind": kind.borrow().clone()}})
                    }
                    ("terminal.get", "t3") => record(t, Some("working")),
                    // A shell printing a status report of its own: forged.
                    ("terminal.get", "t4") => {
                        json!({"address": t, "state": "live", "argv": ["pwsh.exe"], "cwd": "/work",
                               "foreground": {"name": "pwsh.exe", "argv": ["pwsh.exe"], "cwd": "/work"},
                               "reported": true, "status": {"state": "blocked", "kind": "permission", "app": "claude-code"}})
                    }
                    ("terminal.get", "t6") => {
                        json!({"address": t, "state": "live", "argv": ["C:\\Windows\\System32\\cmd.exe"], "cwd": "/work",
                               "reported": true, "status": {"state": "blocked", "kind": "permission"}})
                    }
                    ("terminal.get", _) => return Err(RpcError::new("not_found", format!("no terminal {t}"), "")),
                    ("terminal.list", _) => {
                        json!({"terminals": [record("t2", Some("blocked")), {"address": "t9", "name": "web",
                        "state": "live", "argv": ["/usr/bin/npm"], "cwd": r"C:\work\web", "reported": false}]})
                    }
                    ("terminal.read", _) => json!({"text": "one\ntwo\nexport API_KEY=abcdef0123456789\nfour\n\n\n"}),
                    ("terminal.run", _) => json!({"address": "t5"}),
                    ("terminal.interrupt", _) => json!({"outcome": "stopped", "key": "esc"}),
                    _ => json!({}),
                })
            };
            self.bridge.execute(&core, from, command, now)
        }

        /// The calls since the last look, minus lookups.
        fn acted(&self) -> Vec<(String, Value)> {
            self.calls.borrow_mut().drain(..).filter(|(m, _)| m != "terminal.get").collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.bridge.dir);
        }
    }

    fn token(reply: &Reply) -> String {
        match reply {
            Reply::NeedsConfirm { token, .. } => token.clone(),
            other => panic!("wanted a confirmation, got {other:?}"),
        }
    }

    #[test]
    fn only_paired_accounts_get_through() {
        let mut f = fixture(BridgeConfig::default());
        let stranger = Sender { transport: "telegram".into(), user_id: "666".into() };
        assert_eq!(f.send_as(&stranger, Command::List, T), Reply::NotPaired);
        let other_transport = Sender { transport: "relay".into(), user_id: "42".into() };
        assert_eq!(f.send_as(&other_transport, Command::List, T), Reply::NotPaired);
        assert!(f.acted().is_empty(), "the core never heard of them");
        assert!(matches!(f.send(Command::List, T), Reply::Done(_)));
        f.bridge.unpair(None).unwrap();
        assert_eq!(f.send(Command::List, T), Reply::NotPaired, "unpairing takes effect at once");
        let log = f.bridge.log(10).unwrap();
        let results: Vec<&str> = log.iter().filter_map(|e| e["result"].as_str()).collect();
        assert_eq!(results, ["paired", "not_paired", "not_paired", "done", "not_paired"]);
        assert_eq!(log[1]["identity"], "telegram:666");
    }

    #[test]
    fn thirty_commands_a_minute_per_account() {
        let mut f = fixture(BridgeConfig::default());
        for n in 0..30 {
            assert!(matches!(f.send(Command::List, T + n), Reply::Done(_)), "{n}");
        }
        assert_eq!(f.send(Command::List, T + 30), Reply::RateLimited { retry_secs: 60 });
        assert_eq!(f.send(Command::List, T + 59_500), Reply::RateLimited { retry_secs: 1 });
        let stranger = Sender { transport: "telegram".into(), user_id: "7".into() };
        assert_eq!(f.send_as(&stranger, Command::List, T + 30), Reply::NotPaired, "counted per account");
        assert!(matches!(f.send(Command::List, T + 60_000), Reply::Done(_)), "the oldest left the window");
        let mut zero = fixture(BridgeConfig { rate_limit: Some(0), ..Default::default() });
        assert_eq!(zero.send(Command::List, T), Reply::RateLimited { retry_secs: 60 });
    }

    #[test]
    fn the_window_slides_and_rate_limited_commands_leave_no_trace() {
        let mut f = fixture(BridgeConfig::default());
        for at in [T, T + 30_000] {
            for n in 0..15 {
                assert!(matches!(f.send(Command::List, at), Reply::Done(_)), "{at} {n}");
            }
        }
        assert_eq!(f.send(Command::List, T + 59_999), Reply::RateLimited { retry_secs: 1 });
        for n in 0..15 {
            assert!(matches!(f.send(Command::List, T + 60_000), Reply::Done(_)), "{n}");
        }
        assert_eq!(f.send(Command::List, T + 60_000), Reply::RateLimited { retry_secs: 30 });
        // Unpaired senders are counted too: a flood of them is cut off.
        let stranger = Sender { transport: "telegram".into(), user_id: "666".into() };
        for _ in 0..30 {
            assert_eq!(f.send_as(&stranger, Command::Kill { terminal: "t1".into() }, T), Reply::NotPaired);
        }
        assert_eq!(f.send_as(&stranger, Command::List, T), Reply::RateLimited { retry_secs: 60 });
        let log = f.bridge.log(1_000).unwrap();
        assert_eq!(log.len(), 1 + 45 + 30, "the pairing, my commands, the stranger's");
        assert!(log.iter().all(|e| e["result"] != "rate_limited"));
    }

    #[test]
    fn a_confirmation_holds_until_its_last_millisecond_and_only_while_paired() {
        let mut f = fixture(BridgeConfig::default());
        let kill = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        assert_eq!(kill.len(), 8);
        let blank = f.send(Command::Confirm { token: String::new() }, T);
        assert!(matches!(blank, Reply::Failed(_)), "{blank:?}");
        assert!(matches!(f.send(Command::Confirm { token: format!(" {kill} ") }, T + 119_999), Reply::Done(_)));
        assert_eq!(f.acted(), [("terminal.kill".to_string(), json!({"terminal": "t1"}))]);

        let kill = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        f.bridge.unpair(None).unwrap();
        assert_eq!(f.send(Command::Confirm { token: kill }, T + 1), Reply::NotPaired);
        assert!(f.acted().is_empty(), "unpairing voids what was waiting");
    }

    #[test]
    fn a_stranger_cannot_lock_and_a_lock_outlives_this_bridge() {
        let mut f = fixture(BridgeConfig::default());
        let stranger = Sender { transport: "telegram".into(), user_id: "666".into() };
        assert_eq!(f.send_as(&stranger, Command::Lock, T), Reply::NotPaired);
        assert!(!f.bridge.locked().unwrap());
        assert!(matches!(f.send(Command::Lock, T), Reply::Done(_)));
        assert_eq!(f.send(Command::Lock, T), Reply::Locked, "already locked");
        let mut again = Bridge::at(f.bridge.dir.clone(), BridgeConfig::default());
        let core = |_: &str, _: Value| -> Result<Value, RpcError> { panic!("a locked bridge called the core") };
        assert_eq!(again.execute(&core, &me(), Command::List, T), Reply::Locked, "a restart does not unlock");
    }

    #[test]
    fn a_corrupt_state_file_runs_nothing() {
        let mut f = fixture(BridgeConfig::default());
        for bad in ["", r#"{"identities": [{"id": 1, "transport": "telegram""#, "not json"] {
            std::fs::write(f.bridge.dir.join("state.json"), bad).unwrap();
            assert!(matches!(f.send(Command::List, T), Reply::Failed(_)), "{bad:?}");
            assert!(matches!(f.send(Command::Send { terminal: "t3".into(), text: "hi".into() }, T), Reply::Failed(_)));
        }
        assert!(f.calls.borrow().is_empty(), "the core never heard of it");
        assert_eq!(f.bridge.log(1).unwrap()[0]["result"], "failed", "still audited");
    }

    #[test]
    fn an_unwritable_audit_log_runs_nothing() {
        let mut f = fixture(BridgeConfig::default());
        let kill = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        f.calls.borrow_mut().clear();
        let audit = f.bridge.dir.join("audit.jsonl");
        std::fs::remove_file(&audit).unwrap();
        std::fs::create_dir(&audit).unwrap();
        let send = Command::Send { terminal: "t3".into(), text: "hi".into() };
        for command in [Command::List, send, Command::Confirm { token: kill.clone() }, Command::Lock] {
            let reply = f.send(command, T);
            assert!(matches!(&reply, Reply::Failed(m) if m.contains("audit log")), "{reply:?}");
        }
        assert!(f.calls.borrow().is_empty(), "nothing reached the core");
        assert!(!f.bridge.locked().unwrap(), "nor the lock");
        std::fs::remove_dir(&audit).unwrap();
        assert!(matches!(f.send(Command::Confirm { token: kill }, T), Reply::Done(_)), "the token was not spent");
    }

    #[test]
    fn run_folders_follow_this_machines_path_rules() {
        let mut f = fixture(BridgeConfig::default());
        let run = |folder: &str| Command::Run { folder: folder.into(), command: "claude".into() };
        let (full, relative): (&[&str], &[&str]) = if cfg!(windows) {
            (&[r"C:\x", "C:/x", r"\\server\share", r"\\server\share\app", r"\\?\C:\x"], &[r"\x", "/x", r".\x", "~"])
        } else {
            (&["/", "/x/../y"], &[r"C:\x", r"\\server\share", r"\x", "./x", "~/x"])
        };
        for folder in full {
            let reply = f.send(run(folder), T);
            let Reply::NeedsConfirm { what, .. } = &reply else { panic!("{folder:?}: {reply:?}") };
            assert_eq!(*what, format!("run `claude` in {folder}"));
        }
        for folder in relative {
            assert!(matches!(f.send(run(folder), T), Reply::Failed(_)), "{folder:?}");
        }
        assert_eq!(
            f.send(run("C:x"), T),
            Reply::Failed(r#""C:x" is not a full path on this machine"#.into()),
            "drive-relative is never full"
        );
        assert!(f.acted().is_empty());
    }

    #[test]
    fn the_log_cuts_text_after_redacting_it() {
        assert_eq!(brief(""), "");
        assert_eq!(brief(&"a".repeat(24)), "a".repeat(24));
        assert_eq!(brief(&"a".repeat(25)), format!("{}…", "a".repeat(24)));
        assert_eq!(brief(&"é".repeat(30)), format!("{}…", "é".repeat(24)));
        let secret = format!("{} sk-abcdefghijklmnopqrstu", "a".repeat(20));
        assert_eq!(brief(&secret), format!("{} [re…", "a".repeat(20)), "no prefix of the key");
    }

    #[test]
    fn destructive_commands_wait_for_a_single_use_confirmation() {
        let mut f = fixture(BridgeConfig::default());
        let asked = f.send(Command::Kill { terminal: "t2".into() }, T);
        let Reply::NeedsConfirm { what, .. } = &asked else { panic!("{asked:?}") };
        assert_eq!(what, "kill t2 (claude-code in /work)");
        assert!(f.acted().is_empty(), "nothing killed yet");
        let kill = token(&asked);

        let stranger = Sender { transport: "telegram".into(), user_id: "43".into() };
        let (code, _) = f.bridge.pair("telegram", T).unwrap();
        f.bridge.pair_verify("telegram", "43", "", &code, T).unwrap();
        assert!(matches!(f.send_as(&stranger, Command::Confirm { token: kill.clone() }, T), Reply::Failed(_)));
        assert!(matches!(f.send(Command::Confirm { token: "WRONG123".into() }, T), Reply::Failed(_)));
        let lower = kill.to_lowercase();
        assert_eq!(
            f.send(Command::Confirm { token: lower }, T + 1),
            Reply::Done("done: kill t2 (claude-code in /work)".into())
        );
        assert_eq!(f.acted(), [("terminal.kill".to_string(), json!({"terminal": "t2"}))]);
        assert!(matches!(f.send(Command::Confirm { token: kill }, T + 2), Reply::Failed(_)), "single use");
        assert!(f.acted().is_empty());

        let late = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        assert!(matches!(f.send(Command::Confirm { token: late }, T + 120_000), Reply::Failed(_)), "2 minutes");
        assert!(f.acted().is_empty());
        let unknown = f.send(Command::Kill { terminal: "t404".into() }, T);
        assert_eq!(unknown, Reply::Failed("no terminal t404".into()));
    }

    #[test]
    fn a_status_report_from_a_shell_still_needs_confirm() {
        let mut f = fixture(BridgeConfig::default());
        for t in ["t4", "t6"] {
            let send = f.send(Command::Send { terminal: t.into(), text: "ls".into() }, T);
            assert!(matches!(send, Reply::NeedsConfirm { .. }), "{t}");
            let approve = f.send(Command::Approve { terminal: t.into() }, T);
            assert!(matches!(approve, Reply::NeedsConfirm { .. }), "{t}");
            assert!(f.acted().is_empty(), "{t}");
            assert!(matches!(f.send(Command::Confirm { token: token(&approve) }, T), Reply::Done(_)));
            assert_eq!(f.acted()[0].1["keys"], json!(["y", "enter"]), "{t}");
        }
    }

    /// What a null kind (the report said none) or the given one makes of approve and deny.
    fn answer_with(f: &mut Fixture, from: &Sender, terminal: &str, kind: Value, yes: bool) -> Reply {
        *f.kind.borrow_mut() = kind;
        let terminal = terminal.to_string();
        f.send_as(from, if yes { Command::Approve { terminal } } else { Command::Deny { terminal } }, T)
    }

    #[test]
    fn only_a_permission_is_answered_from_a_remote_chat() {
        let mut f = fixture(BridgeConfig::default());
        let (code, _) = f.bridge.pair("web", T).unwrap();
        f.bridge.pair_verify("web", "7", "phone", &code, T).unwrap();
        let web = Sender { transport: "web".into(), user_id: "7".into() };
        for from in [me(), web] {
            for yes in [true, false] {
                let permission = answer_with(&mut f, &from, "k1", json!("permission"), yes);
                assert!(matches!(permission, Reply::Done(_)), "{from:?} {yes}: {permission:?}");
                assert_eq!(f.acted()[0].1["keys"], json!([if yes { "enter" } else { "esc" }]));
                for (kind, said) in [
                    (json!("question"), "a question"),
                    (json!("auth"), "a sign-in"),
                    (Value::Null, "a request of unknown kind"),
                    (json!("banana"), "a request of unknown kind"),
                    (json!("Permission"), "a request of unknown kind"),
                    (json!(""), "a request of unknown kind"),
                    (json!(7), "a request of unknown kind"),
                ] {
                    let reply = answer_with(&mut f, &from, "k1", kind.clone(), yes);
                    let Reply::Failed(message) = &reply else { panic!("{from:?} {yes} {kind}: {reply:?}") };
                    assert!(message.contains(said) && message.contains("in the terminal"), "{message}");
                    assert!(f.acted().is_empty(), "{kind}: no key was pressed");
                }
            }
        }
        let last = f.bridge.log(1).unwrap();
        assert_eq!(last[0]["result"], "failed", "a refusal is audited");
    }

    #[test]
    fn a_shell_printing_a_blocked_report_follows_kind_too_and_is_checked_again_at_the_confirmation() {
        let mut f = fixture(BridgeConfig::default());
        for yes in [true, false] {
            // A permission still needs the confirmation, then types y/n and enter.
            let ask = answer_with(&mut f, &me(), "k2", json!("permission"), yes);
            let confirm = Command::Confirm { token: token(&ask) };
            assert!(f.acted().is_empty());
            assert!(matches!(f.send(confirm, T), Reply::Done(_)));
            assert_eq!(f.acted()[0].1["keys"], json!([if yes { "y" } else { "n" }, "enter"]));
            // A question, an auth, no kind or an odd one: refused before any confirmation is offered.
            for kind in [json!("question"), json!("auth"), Value::Null, json!("banana")] {
                let reply = answer_with(&mut f, &me(), "k2", kind.clone(), yes);
                assert!(matches!(reply, Reply::Failed(_)), "{kind}: {reply:?}");
            }
            // The status changes while the confirmation waits: nothing is typed.
            let ask = answer_with(&mut f, &me(), "k2", json!("permission"), yes);
            *f.kind.borrow_mut() = json!("question");
            let reply = f.send(Command::Confirm { token: token(&ask) }, T);
            assert!(matches!(&reply, Reply::Failed(m) if m.contains("a question")), "{reply:?}");
            assert!(f.acted().is_empty(), "nothing typed after the kind changed");
        }
        // Not blocked at all (a shell with no report): unchanged, y/n after a confirmation.
        assert!(matches!(f.send(Command::Approve { terminal: "t1".into() }, T), Reply::NeedsConfirm { .. }));
    }

    #[test]
    fn shell_names_are_recognised_by_program_not_path_or_case() {
        for p in ["bash", "/usr/bin/zsh", "C:\\Windows\\System32\\cmd.exe", "PowerShell.EXE", "nu"] {
            assert!(at_shell(&json!({"argv": [p]})), "{p}");
        }
        for p in ["claude", "/usr/bin/node", "bashful", ""] {
            assert!(!at_shell(&json!({"argv": [p]})), "{p}");
        }
        assert!(!at_shell(&json!({"argv": ["bash"], "foreground": {"name": "claude"}})));
        assert!(at_shell(&json!({"argv": ["claude"], "foreground": {"name": "fish"}})));
        assert!(!at_shell(&json!({})));
        // An empty name is a missing one.
        assert!(at_shell(&json!({"argv": ["zsh"], "foreground": {"name": ""}})));
        assert!(!at_shell(&json!({"argv": ["zsh"], "foreground": {"name": "", "argv": ["claude"]}})));
    }

    #[test]
    fn shell_detection_reads_program_paths_with_spaces_and_skips_look_alikes() {
        // Windows paths with spaces and arguments after the program; mixed separators and case.
        for argv in [
            json!(["C:\\Program Files\\PowerShell\\7\\pwsh.exe", "-NoLogo"]),
            json!(["C:/Program Files/Git/usr/bin/bash.EXE", "--login", "-i"]),
            json!(["\\\\server\\share\\tools\\CMD.exe", "/k"]),
        ] {
            assert!(at_shell(&json!({ "argv": argv })), "{argv}");
        }
        // Names that only start or end like a shell are programs of their own.
        for p in ["shellcheck", "C:\\tools\\shellcheck.exe", "/usr/bin/bash-language-server", "zsh.exe.bak", "pwsh."] {
            assert!(!at_shell(&json!({"argv": [p]})), "{p}");
        }
        // No program at all, or not a string: not a shell.
        for t in [json!({"argv": []}), json!({"argv": [null]}), json!({"argv": [7]}), json!({"foreground": {}})] {
            assert!(!at_shell(&t), "{t}");
        }
        // A foreground without a name falls back to its argv, then to the start argv.
        assert!(at_shell(&json!({"argv": ["claude"], "foreground": {"name": null, "argv": ["/bin/zsh", "-l"]}})));
        assert!(at_shell(&json!({"argv": ["pwsh.exe"], "foreground": {"argv": []}})));
        assert!(!at_shell(&json!({"argv": ["bash"], "foreground": {"argv": ["C:\\agents\\codex.exe"]}})));
    }

    #[test]
    fn agents_take_text_and_answers_directly_plain_shells_after_confirming() {
        let mut f = fixture(BridgeConfig::default());
        let send = |t: &str| Command::Send { terminal: t.into(), text: "fix the test".into() };
        assert!(matches!(f.send(send("t3"), T), Reply::Done(_)));
        assert_eq!(f.acted(), [("terminal.send".to_string(), json!({"terminal": "t3", "text": "fix the test"}))]);
        let shell = f.send(send("t1"), T);
        assert!(f.acted().is_empty());
        assert!(matches!(f.send(Command::Confirm { token: token(&shell) }, T), Reply::Done(_)));
        assert_eq!(f.acted()[0].0, "terminal.send");
        let keys = Command::Keys { terminal: "t1".into(), keys: vec![Key::CtrlC, Key::Up] };
        assert!(matches!(f.send(keys, T), Reply::NeedsConfirm { .. }));
        let empty = Command::Send { terminal: "t3".into(), text: String::new() };
        assert_eq!(f.send(empty, T), Reply::Failed("nothing to send".into()));
        assert_eq!(
            f.send(Command::Keys { terminal: "t3".into(), keys: vec![] }, T),
            Reply::Failed("no keys to press".into())
        );

        assert!(matches!(f.send(Command::Approve { terminal: "t2".into() }, T), Reply::Done(_)));
        assert!(matches!(f.send(Command::Deny { terminal: "t2".into() }, T), Reply::Done(_)));
        let pressed: Vec<Value> = f.acted().into_iter().map(|(_, p)| p["keys"].clone()).collect();
        assert_eq!(pressed, [json!(["enter"]), json!(["esc"])]);
        let busy = f.send(Command::Approve { terminal: "t3".into() }, T);
        assert_eq!(busy, Reply::Failed("t3 is not waiting on you (it is working)".into()));
        let plain = f.send(Command::Deny { terminal: "t1".into() }, T);
        assert!(matches!(f.send(Command::Confirm { token: token(&plain) }, T), Reply::Done(_)));
        assert_eq!(f.acted()[0].1["keys"], json!(["n", "enter"]));

        assert_eq!(
            f.send(Command::Interrupt { terminal: "t3".into() }, T),
            Reply::Done("interrupt t3: stopped".into())
        );
    }

    #[test]
    fn run_needs_a_full_path_and_a_confirmation() {
        let mut f = fixture(BridgeConfig::default());
        let here = if cfg!(windows) { r"C:\Users\me\app" } else { "/home/me/app" };
        let run = |folder: &str, command: &str| Command::Run { folder: folder.into(), command: command.into() };
        for (folder, command) in [("app", "claude"), (r"C:app", "claude"), ("", "claude"), (here, "  ")] {
            assert!(matches!(f.send(run(folder, command), T), Reply::Failed(_)), "{folder:?} {command:?}");
        }
        if cfg!(windows) {
            assert!(matches!(f.send(run(r"\\server\share\app", "claude"), T), Reply::NeedsConfirm { .. }), "UNC");
            assert!(matches!(f.send(run("/home/me", "claude"), T), Reply::Failed(_)), "no drive: not full here");
        }
        let agent = f.send(run(here, "claude"), T);
        assert_eq!(f.send(Command::Confirm { token: token(&agent) }, T), Reply::Done("started t5".into()));
        let line = f.send(run(here, "npm test && npm run lint"), T);
        f.send(Command::Confirm { token: token(&line) }, T);
        let params: Vec<Value> = f.acted().into_iter().map(|(_, p)| p).collect();
        assert_eq!(params[0], json!({"argv": ["claude"], "cwd": here, "no_parent": true}));
        assert_eq!(params[1], json!({"line": "npm test && npm run lint", "cwd": here, "no_parent": true}));
    }

    #[test]
    fn lists_and_reads_go_out_redacted() {
        let mut f = fixture(BridgeConfig::default());
        let Reply::Done(list) = f.send(Command::List, T) else { panic!() };
        assert_eq!(list, "t2 · claude-code · /work · blocked\nt9 web · npm · C:\\work\\web · running");
        let read = |lines| Command::Read { terminal: "t3".into(), lines };
        assert_eq!(f.send(read(2), T), Reply::Done("export API_KEY=[redacted]\nfour".into()));
        assert_eq!(f.send(read(0), T), Reply::Done("four".into()), "at least one line");
        assert_eq!(f.send(read(10_000), T), Reply::Done("one\ntwo\nexport API_KEY=[redacted]\nfour".into()));
        assert_eq!(f.acted()[3].1["lines"], 200, "capped");
    }

    #[test]
    fn a_remote_lock_holds_until_unlocked_here() {
        let mut f = fixture(BridgeConfig::default());
        let kill = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        assert!(matches!(f.send(Command::Lock, T), Reply::Done(_)));
        assert!(f.bridge.locked().unwrap());
        assert_eq!(f.send(Command::List, T), Reply::Locked);
        assert_eq!(f.send(Command::Confirm { token: kill.clone() }, T), Reply::Locked, "waiting commands too");
        assert!(f.acted().is_empty());
        f.bridge.set_locked(false).unwrap();
        // Spec change (ADR-061 addendum): a lock voids every waiting confirmation.
        assert!(matches!(f.send(Command::Confirm { token: kill }, T), Reply::Failed(_)), "void after unlock");
        assert!(f.acted().is_empty());
        assert!(matches!(f.send(Command::List, T), Reply::Done(_)), "unlocked");
    }

    #[test]
    fn a_local_lock_or_a_new_pairing_voids_waiting_confirmations() {
        let mut f = fixture(BridgeConfig::default());
        // Locked and unlocked by another process (the CLI): only the state file says so.
        let kill = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        let cli = Bridge::at(f.bridge.dir.clone(), BridgeConfig::default());
        cli.set_locked(true).unwrap();
        cli.set_locked(false).unwrap();
        assert!(matches!(f.send(Command::Confirm { token: kill }, T), Reply::Failed(_)));
        // Unpaired and paired again: a new identity, the old tokens are not its.
        let kill = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        f.bridge.unpair(None).unwrap();
        let (code, _) = f.bridge.pair("telegram", T).unwrap();
        f.bridge.pair_verify("telegram", "42", "me", &code, T).unwrap();
        assert!(matches!(f.send(Command::Confirm { token: kill }, T), Reply::Failed(_)));
        assert!(f.acted().is_empty(), "nothing was killed");
    }

    #[test]
    fn the_command_path_rotates_the_audit_log_at_its_size_limit() {
        let mut f = fixture(BridgeConfig::default());
        let path = f.bridge.dir.join("audit.jsonl");
        std::fs::write(&path, vec![b'x'; usize::try_from(super::super::AUDIT_MAX).unwrap()]).unwrap();
        f.send(Command::List, T);
        let old = std::fs::metadata(f.bridge.dir.join("audit.jsonl.1")).unwrap().len();
        assert_eq!(old, super::super::AUDIT_MAX, "the full log moved aside");
        assert_eq!(f.bridge.log(5).unwrap().len(), 1, "a fresh log with just the new entry");
        f.send(Command::List, T);
        assert!(std::fs::metadata(&path).unwrap().len() < super::super::AUDIT_MAX);
    }

    #[test]
    fn a_command_writes_the_pending_failure_summary_before_its_own_line() {
        let mut f = fixture(BridgeConfig::default());
        f.bridge.pair("web", T).unwrap();
        for n in 0..8 {
            assert!(f.bridge.pair_verify("web", "u1", "", "WRONGWRONG", T + n).is_err());
        }
        // Inside the minute the summary is not due: the command's line follows the failures.
        f.send(Command::List, T + 100);
        let log = f.bridge.log(100).unwrap();
        assert!(log.iter().all(|e| e["command"] != "failed-attempts"), "{log:?}");
        // A quiet ten minutes later the next command brings the summary first, dated then.
        let late = T + 600_000;
        f.send(Command::List, late);
        let log = f.bridge.log(100).unwrap();
        let [.., summary, own] = &log[..] else { panic!("{log:?}") };
        assert_eq!(summary["command"], "failed-attempts");
        assert_eq!(summary["time_ms"], late);
        assert_eq!(summary["result"], "3 more failed attempts from 1 sources in the last minute");
        assert_eq!((own["identity"].as_str(), own["command"].as_str()), (Some("telegram:42"), Some("list")));
    }

    #[test]
    fn the_command_path_leaves_a_log_one_byte_under_its_limit_in_place() {
        let mut f = fixture(BridgeConfig::default());
        let path = f.bridge.dir.join("audit.jsonl");
        let under = super::super::AUDIT_MAX - 1;
        std::fs::write(&path, vec![b'x'; usize::try_from(under).unwrap()]).unwrap();
        assert!(matches!(f.send(Command::List, T), Reply::Done(_)));
        assert!(!f.bridge.dir.join("audit.jsonl.1").exists(), "no rotation under the limit");
        assert!(std::fs::metadata(&path).unwrap().len() > under, "the entry was appended");
        f.send(Command::List, T);
        assert!(f.bridge.dir.join("audit.jsonl.1").exists(), "the next command rotates the now full log");
    }

    #[test]
    fn a_full_audit_log_that_cannot_rotate_fails_the_command_and_runs_nothing() {
        let mut f = fixture(BridgeConfig::default());
        let kill = token(&f.send(Command::Kill { terminal: "t1".into() }, T));
        f.calls.borrow_mut().clear();
        let path = f.bridge.dir.join("audit.jsonl");
        let max = super::super::AUDIT_MAX;
        std::fs::write(&path, vec![b'x'; usize::try_from(max).unwrap()]).unwrap();
        let old = f.bridge.dir.join("audit.jsonl.1");
        std::fs::create_dir(&old).unwrap();
        for command in [Command::List, Command::Confirm { token: kill.clone() }, Command::Lock] {
            let reply = f.send(command, T);
            assert!(matches!(&reply, Reply::Failed(m) if m.contains("audit log")), "{reply:?}");
        }
        assert!(f.calls.borrow().is_empty(), "nothing reached the core");
        assert!(!f.bridge.locked().unwrap(), "nor the lock");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), max, "the full log is untouched");
        std::fs::remove_dir(&old).unwrap();
        assert!(matches!(f.send(Command::Confirm { token: kill }, T), Reply::Done(_)), "the token was not spent");
        assert_eq!(std::fs::metadata(&old).unwrap().len(), max, "the log rotated once it could");
    }

    #[test]
    fn tokens_never_reach_the_log_but_the_confirmed_action_does() {
        let mut f = fixture(BridgeConfig::default());
        let kill = token(&f.send(Command::Kill { terminal: "t2".into() }, T));
        assert!(matches!(f.send(Command::Confirm { token: kill.clone() }, T), Reply::Done(_)));
        f.send(Command::Confirm { token: kill.clone() }, T);
        let text = std::fs::read_to_string(f.bridge.dir.join("audit.jsonl")).unwrap();
        assert!(!text.contains(&kill), "{text}");
        let log = f.bridge.log(3).unwrap();
        assert_eq!((&log[0]["command"], &log[0]["what"]), (&json!("kill"), &json!("kill t2 (claude-code in /work)")));
        assert_eq!(
            (&log[1]["command"], &log[1]["what"]),
            (&json!("confirm"), &json!("kill t2 (claude-code in /work)"))
        );
        assert_eq!((&log[2]["result"], log[2].get("what")), (&json!("failed"), None), "a spent token runs nothing");
    }

    #[test]
    fn folders_and_terminal_names_are_logged_like_text() {
        let mut f = fixture(BridgeConfig::default());
        let folder = "/srv/projects/a-very-long-folder/token=abcdef0123456789";
        f.send(Command::Run { folder: folder.into(), command: "claude".into() }, T);
        f.send(Command::Read { terminal: "sk-abcdefghijklmnopqrstuvwx".into(), lines: 1 }, T);
        let log = f.bridge.log(2).unwrap();
        assert_eq!(log[0]["folder"], "/srv/projects/a-very-lon…", "redacted, then cut");
        assert!(!log.iter().any(|e| e.to_string().contains("abcdef0123456789")));
        assert_eq!(log[1]["terminal"], "[redacted]");
    }

    #[test]
    fn the_audit_log_keeps_no_whole_text() {
        let mut f = fixture(BridgeConfig::default());
        let secret = "use sk-abcdefghijklmnopqrstuvwx then write a very long essay about it";
        f.send(Command::Send { terminal: "t3".into(), text: secret.into() }, T);
        f.send(Command::Keys { terminal: "t1".into(), keys: vec![Key::Y, Key::Enter] }, T);
        let log = f.bridge.log(2).unwrap();
        assert_eq!(log[0]["text"], "use [redacted] then writ…");
        assert_eq!(log[0]["chars"], secret.chars().count());
        assert_eq!(
            (&log[0]["terminal"], &log[0]["identity"], &log[0]["time_ms"]),
            (&json!("t3"), &json!("telegram:42"), &json!(T))
        );
        assert_eq!((&log[1]["keys"], &log[1]["result"]), (&json!(["y", "enter"]), &json!("needs_confirm")));
        assert!(log[1]["what"].as_str().unwrap().contains("press y enter in t1"));
        let text = std::fs::read_to_string(f.bridge.dir.join("audit.jsonl")).unwrap();
        assert!(!text.contains("sk-abc") && !text.contains("essay"));
    }
}
