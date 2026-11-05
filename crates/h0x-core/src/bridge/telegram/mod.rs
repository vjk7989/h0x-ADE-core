//! Telegram frontend (`h0x-core bridge run`): long-polls the Bot API, turns
//! private messages and button taps from paired accounts into bridge
//! commands, and sends notices with buttons to every paired account.
//! Outbound only. Update payloads and the token are never logged; the audit
//! log is the bridge's.

mod api;

pub use api::Api;

use super::{Bridge, Command, Kind, Notice, Reply, Sender};
use crate::paths::Paths;
use crate::rpc::RpcError;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Telegram's longest message, in UTF-16 units.
const LIMIT: usize = 4096;
/// Seconds one getUpdates waits for news.
const POLL: u64 = 50;
/// Notices remembered for replies (a reply to one types into its terminal).
const REMEMBERED: usize = 1000;

const HELP: &str = "Commands:
/agents - terminals: agent, folder, status
/read <t> [lines] - its last lines (default 20)
/send <t> <text> - type text and press enter
/keys <t> <keys...> - enter esc ctrl+c up down left right y n
/approve <t>, /deny <t> - answer what it is waiting on
/interrupt <t> - press its stop key
/run <folder> <command> - start an agent or command (\"quote\" a folder with spaces)
/kill <t> - end its program
/lock - refuse every remote command until unlocked at the machine
Reply to a notice to type into its terminal.";

impl Bridge {
    fn token_file(&self) -> std::path::PathBuf {
        self.dir.join("telegram.token")
    }

    /// The bot token, if one is stored.
    pub fn telegram_token(&self) -> Result<Option<String>, RpcError> {
        match std::fs::read_to_string(self.token_file()) {
            Ok(text) => Ok(Some(text.trim().to_string()).filter(|t| !t.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(super::failed(e)),
        }
    }

    /// Stores the bot token from @BotFather (0600). It must look like one:
    /// digits, a colon, then letters, digits, `_` and `-`.
    pub fn set_telegram_token(&self, token: &str) -> Result<(), RpcError> {
        let token = token.trim();
        let valid = token.split_once(':').is_some_and(|(id, secret)| {
            !id.is_empty()
                && id.bytes().all(|c| c.is_ascii_digit())
                && secret.len() >= 20
                && secret.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        });
        if !valid {
            return Err(RpcError::new(
                "invalid_params",
                "that is not a bot token",
                "copy the token @BotFather gave you, like 123456789:AA...",
            ));
        }
        self.write_private("telegram.token", token.as_bytes())
    }

    /// Removes the bot token: whether there was one.
    pub fn clear_telegram_token(&self) -> Result<bool, RpcError> {
        match std::fs::remove_file(self.token_file()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(super::failed(e)),
        }
    }
}

/// What a chat message asks for.
#[derive(Debug, PartialEq)]
enum Ask {
    Pair(String),
    Help,
    Do(Command),
    Usage(&'static str),
}

/// A bot command (`/read t3 50`, `/agents@mybot`), else None: plain text.
fn parse(text: &str) -> Option<Ask> {
    let rest = text.trim().strip_prefix('/')?;
    let (word, args) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(w, a)| (w, a.trim()));
    let word = word.split('@').next().unwrap_or_default().to_ascii_lowercase();
    // The first word of the arguments, and the rest as typed.
    let (first, more) = args.split_once(char::is_whitespace).map_or((args, ""), |(f, m)| (f, m.trim_start()));
    let terminal = || first.to_string();
    let one = |usage, make: fn(String) -> Command| {
        if first.is_empty() || !more.is_empty() { Ask::Usage(usage) } else { Ask::Do(make(terminal())) }
    };
    Some(match word.as_str() {
        "start" | "help" => Ask::Help,
        "pair" if !args.is_empty() => Ask::Pair(args.into()),
        "pair" => Ask::Usage("/pair CODE (from `h0x-core bridge pair` on the machine)"),
        "agents" => Ask::Do(Command::List),
        "lock" => Ask::Do(Command::Lock),
        "read" => match (first, more.parse::<usize>()) {
            ("", _) => Ask::Usage("/read <t> [lines]"),
            (_, _) if more.is_empty() => Ask::Do(Command::Read { terminal: terminal(), lines: 20 }),
            (_, Ok(lines)) => Ask::Do(Command::Read { terminal: terminal(), lines }),
            (_, Err(_)) => Ask::Usage("/read <t> [lines]"),
        },
        "send" if !first.is_empty() && !more.is_empty() => {
            Ask::Do(Command::Send { terminal: terminal(), text: more.into() })
        }
        "send" => Ask::Usage("/send <t> <text>"),
        "keys" => {
            let keys: Option<Vec<_>> = more.split_whitespace().map(super::Key::parse).collect();
            match keys {
                Some(keys) if !first.is_empty() && !keys.is_empty() => {
                    Ask::Do(Command::Keys { terminal: terminal(), keys })
                }
                _ => Ask::Usage("/keys <t> <keys...>: enter esc ctrl+c up down left right y n"),
            }
        }
        "approve" => one("/approve <t>", |terminal| Command::Approve { terminal }),
        "deny" => one("/deny <t>", |terminal| Command::Deny { terminal }),
        "interrupt" => one("/interrupt <t>", |terminal| Command::Interrupt { terminal }),
        "kill" => one("/kill <t>", |terminal| Command::Kill { terminal }),
        "run" => {
            // A folder with spaces comes in double quotes.
            let split = match args.strip_prefix('"') {
                Some(quoted) => quoted.split_once('"').map(|(folder, command)| (folder, command.trim())),
                None => Some((first, more)),
            };
            match split {
                Some((folder, command)) if !folder.is_empty() && !command.is_empty() => {
                    Ask::Do(Command::Run { folder: folder.into(), command: command.into() })
                }
                _ => Ask::Usage("/run <folder> <command>"),
            }
        }
        _ => Ask::Usage("unknown command; send /help"),
    })
}

/// Text for Telegram's HTML mode.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `text` escaped, in messages Telegram takes (4096 UTF-16 units each, `<pre>`
/// included when `pre`), cut at line ends where it can be.
fn chunks(text: &str, pre: bool) -> Vec<String> {
    let budget = LIMIT - if pre { "<pre></pre>".len() } else { 0 };
    let (mut out, mut cur, mut len) = (Vec::new(), String::new(), 0);
    for c in text.chars() {
        let piece = escape(c.encode_utf8(&mut [0; 4]));
        let units: usize = piece.chars().map(char::len_utf16).sum();
        if len + units > budget {
            // Back to the last line end, if this message has one.
            let at = cur.rfind('\n').map_or(cur.len(), |i| i + 1);
            let rest = cur.split_off(at);
            out.push(std::mem::replace(&mut cur, rest));
            len = cur.chars().map(char::len_utf16).sum();
        }
        cur.push_str(&piece);
        len += units;
    }
    out.push(cur);
    out.retain(|m| !m.is_empty());
    if out.is_empty() {
        out.push("(empty)".into());
    }
    if pre {
        out.iter_mut().for_each(|m| *m = format!("<pre>{m}</pre>"));
    }
    out
}

/// One thing to tell Telegram.
#[derive(Debug, PartialEq)]
enum Out {
    /// A message; buttons are rows of (label, callback data); `about`: the
    /// terminal a reply to it types into.
    Send {
        chat: i64,
        html: String,
        buttons: Vec<Vec<(String, String)>>,
        about: Option<String>,
    },
    Edit {
        chat: i64,
        message: i64,
        html: String,
    },
    Answer {
        callback: String,
    },
}

fn text(chat: i64, html: String) -> Out {
    Out::Send { chat, html, buttons: Vec::new(), about: None }
}

fn button(label: &str, data: String) -> (String, String) {
    (label.into(), data)
}

/// What a reply looks like in the chat.
fn replied(chat: i64, reply: Reply, pre: bool) -> Vec<Out> {
    let say = |line: String| vec![text(chat, escape(&line))];
    match reply {
        Reply::Done(done) => chunks(&done, pre).into_iter().map(|html| text(chat, html)).collect(),
        Reply::NeedsConfirm { token, what } => vec![Out::Send {
            chat,
            html: format!("Confirm: {}?", escape(&what)),
            buttons: vec![vec![button("Confirm", format!("c:{token}")), button("Cancel", "x:".into())]],
            about: None,
        }],
        Reply::NotPaired => Vec::new(),
        Reply::Locked => say("Remote control is locked. Unlock it at the machine: h0x-core bridge unlock".into()),
        Reply::RateLimited { retry_secs } => say(format!("Too many commands; try again in {retry_secs} s.")),
        Reply::Failed(message) => say(format!("Failed: {message}")),
    }
}

/// A notice as a message with buttons, for `chat`.
fn notice(n: &Notice, chat: i64) -> Out {
    let what = match n.kind {
        Kind::NeedsYou => "needs you",
        Kind::Done => "is done",
        Kind::Error => "hit an error",
        Kind::Finished => "finished",
    };
    let t = &n.terminal;
    let mut html = format!("<b>{}</b> {what}", escape(t));
    let place: Vec<&str> = [n.agent.as_str(), n.folder.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    if !place.is_empty() {
        html.push_str(&format!("\n{}", escape(&place.join(" · "))));
    }
    if !n.excerpt.is_empty() {
        html.push_str(&format!("\n\n{}", escape(&n.excerpt)));
    }
    html.push_str(&format!("\n\nReply to this message to type into {}.", escape(t)));
    let show = vec![button("Show", format!("s:{t}")), button("Interrupt", format!("i:{t}"))];
    let buttons = match n.kind {
        Kind::NeedsYou => vec![vec![button("Approve", format!("a:{t}")), button("Deny", format!("d:{t}"))], show],
        Kind::Done | Kind::Error => vec![show],
        Kind::Finished => vec![vec![button("Show", format!("s:{t}"))]],
    };
    Out::Send { chat, html, buttons, about: Some(t.clone()) }
}

/// Notice messages, by (chat, message id), and the terminals they are about.
#[derive(Default)]
struct Replies {
    terminals: HashMap<(i64, i64), String>,
    order: VecDeque<(i64, i64)>,
}

impl Replies {
    fn remember(&mut self, at: (i64, i64), terminal: String) {
        if self.order.len() >= REMEMBERED
            && let Some(old) = self.order.pop_front()
        {
            self.terminals.remove(&old);
        }
        self.order.push_back(at);
        self.terminals.insert(at, terminal);
    }
}

struct Frontend {
    bridge: Bridge,
    replies: Arc<Mutex<Replies>>,
}

type Core<'a> = &'a dyn Fn(&str, Value) -> Result<Value, RpcError>;

impl Frontend {
    fn paired(&self, user: &str) -> bool {
        self.bridge.identities().is_ok_and(|ids| ids.iter().any(|i| i.transport == "telegram" && i.user_id == user))
    }

    /// What to tell Telegram about one update. Group chats and unpaired
    /// accounts (except `/pair`) get nothing at all.
    fn handle(&mut self, core: Core, update: &Value, now: u64) -> Vec<Out> {
        if let Some(message) = update.get("message") {
            self.message(core, message, now)
        } else if let Some(query) = update.get("callback_query") {
            self.callback(core, query, now)
        } else {
            Vec::new()
        }
    }

    fn message(&mut self, core: Core, m: &Value, now: u64) -> Vec<Out> {
        let (Some(chat), Some(user), Some(typed)) =
            (m["chat"]["id"].as_i64(), m["from"]["id"].as_i64(), m["text"].as_str())
        else {
            return Vec::new();
        };
        if m["chat"]["type"] != "private" {
            return Vec::new();
        }
        let user = user.to_string();
        let ask = parse(typed);
        if let Some(Ask::Pair(code)) = &ask {
            let label = m["from"]["username"].as_str().or(m["from"]["first_name"].as_str()).unwrap_or_default();
            let said = match self.bridge.pair_verify("telegram", &user, label, code, now) {
                Ok(me) => format!("Paired as {}. Send /help for commands.", me.label),
                Err(e) => format!("Not paired: {}", e.message),
            };
            return vec![text(chat, escape(&said))];
        }
        if !self.paired(&user) {
            return Vec::new();
        }
        let from = Sender { transport: "telegram".into(), user_id: user };
        let command = match ask {
            Some(Ask::Do(command)) => command,
            Some(Ask::Help) => return vec![text(chat, escape(HELP))],
            Some(Ask::Usage(usage)) => return vec![text(chat, escape(&format!("Usage: {usage}")))],
            Some(Ask::Pair(_)) => unreachable!("answered above"),
            None => {
                let to = m["reply_to_message"]["message_id"].as_i64();
                let terminal = to.and_then(|id| self.replies.lock().unwrap().terminals.get(&(chat, id)).cloned());
                match terminal {
                    Some(terminal) => Command::Send { terminal, text: typed.into() },
                    None => {
                        return vec![text(
                            chat,
                            escape("Send /help for commands, or reply to a notice to type into its terminal."),
                        )];
                    }
                }
            }
        };
        let pre = matches!(command, Command::Read { .. });
        replied(chat, self.bridge.execute(&core, &from, command, now), pre)
    }

    fn callback(&mut self, core: Core, q: &Value, now: u64) -> Vec<Out> {
        let message = &q["message"];
        let (Some(chat), Some(id), Some(user), Some(data)) = (
            message["chat"]["id"].as_i64(),
            message["message_id"].as_i64(),
            q["from"]["id"].as_i64(),
            q["data"].as_str(),
        ) else {
            return Vec::new();
        };
        let user = user.to_string();
        if message["chat"]["type"] != "private" || !self.paired(&user) {
            return Vec::new();
        }
        let mut out = vec![Out::Answer { callback: q["id"].as_str().unwrap_or_default().into() }];
        let (kind, arg) = data.split_once(':').unwrap_or((data, ""));
        let terminal = arg.to_string();
        let command = match kind {
            "a" => Command::Approve { terminal },
            "d" => Command::Deny { terminal },
            "i" => Command::Interrupt { terminal },
            "s" => Command::Read { terminal, lines: 20 },
            "c" => Command::Confirm { token: terminal },
            "x" => {
                out.push(Out::Edit { chat, message: id, html: "Cancelled.".into() });
                return out;
            }
            _ => return out,
        };
        let confirm = kind == "c";
        let pre = kind == "s";
        let from = Sender { transport: "telegram".into(), user_id: user };
        let mut said = replied(chat, self.bridge.execute(&core, &from, command, now), pre);
        // A confirmation's result replaces its question (and its buttons).
        if confirm
            && let Some(Out::Send { html, buttons, .. }) = said.first_mut()
            && buttons.is_empty()
        {
            let html = std::mem::take(html);
            said[0] = Out::Edit { chat, message: id, html };
        }
        out.extend(said);
        out
    }
}

/// Seconds to wait after `failures` network errors in a row: 1, 2, 4... up to 60.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(failures.saturating_sub(1)).min(60))
}

/// Sends one thing; remembers which terminal a notice is about.
fn deliver(api: &Api, replies: &Mutex<Replies>, out: Out) -> Result<(), String> {
    match out {
        Out::Send { chat, html, buttons, about } => {
            let mut body = json!({"chat_id": chat, "text": html, "parse_mode": "HTML"});
            if !buttons.is_empty() {
                let rows: Vec<Vec<Value>> = buttons
                    .into_iter()
                    .map(|row| {
                        row.into_iter().map(|(text, data)| json!({"text": text, "callback_data": data})).collect()
                    })
                    .collect();
                body["reply_markup"] = json!({"inline_keyboard": rows});
            }
            let sent = api.call("sendMessage", &body, 30)?;
            if let (Some(terminal), Some(id)) = (about, sent["message_id"].as_i64()) {
                replies.lock().unwrap().remember((chat, id), terminal);
            }
            Ok(())
        }
        Out::Edit { chat, message, html } => {
            let body = json!({"chat_id": chat, "message_id": message, "text": html, "parse_mode": "HTML"});
            api.call("editMessageText", &body, 30).map(drop)
        }
        Out::Answer { callback } => {
            api.call("answerCallbackQuery", &json!({"callback_query_id": callback}), 30).map(drop)
        }
    }
}

impl Frontend {
    /// One long poll: handles what came and moves `offset` past it.
    fn step(&mut self, api: &Api, core: Core, offset: &mut i64) -> Result<(), String> {
        let ask = json!({"offset": *offset, "timeout": POLL, "allowed_updates": ["message", "callback_query"]});
        let updates = api.call("getUpdates", &ask, POLL + 15)?;
        for update in updates.as_array().into_iter().flatten() {
            if let Some(id) = update["update_id"].as_i64() {
                *offset = (*offset).max(id + 1);
            }
            for out in self.handle(core, update, crate::runtime::now_ms()) {
                if let Err(e) = deliver(api, &self.replies, out) {
                    eprintln!("bridge: {e}");
                }
            }
        }
        Ok(())
    }
}

/// Sends each notice to every paired Telegram account, unless locked.
fn notices(api: Api, replies: Arc<Mutex<Replies>>) {
    let mut failures = 0;
    loop {
        let watched = Paths::from_env().and_then(|paths| {
            let bridge = Bridge::open(&paths)?;
            super::watch(&paths, bridge.config.notice_quiet_ms(), |n| {
                failures = 0;
                if bridge.locked().unwrap_or(true) {
                    return;
                }
                let ids = bridge.identities().unwrap_or_default();
                for chat in ids.iter().filter(|i| i.transport == "telegram").filter_map(|i| i.user_id.parse().ok()) {
                    if let Err(e) = deliver(&api, &replies, notice(&n, chat)) {
                        eprintln!("bridge: {e}");
                    }
                }
            })
        });
        if let Err(e) = watched {
            eprintln!("bridge: notices: {}", e.message);
        }
        failures += 1;
        std::thread::sleep(backoff(failures));
    }
}

/// `h0x-core bridge run`: the bot, until the process is stopped.
pub fn run(paths: &Paths) -> Result<(), RpcError> {
    let bridge = Bridge::open(paths)?;
    let token = bridge
        .telegram_token()?
        .ok_or_else(|| RpcError::new("bridge", "no bot token", "store one first: h0x-core bridge telegram token"))?;
    let api = Api::new(token)?;
    let replies = Arc::new(Mutex::new(Replies::default()));
    let (for_notices, shared) = (api.clone(), replies.clone());
    std::thread::spawn(move || notices(for_notices, shared));
    let core = |method: &str, params: Value| {
        let stream = crate::client::connect_or_start(paths)?;
        crate::client::call(&stream, method, params).map_err(|e| crate::client::explain_closed(paths, e))
    };
    let mut front = Frontend { bridge, replies };
    let (mut offset, mut failures) = (0, 0);
    eprintln!("bridge: running; stop it with ctrl+c (or kill its terminal)");
    loop {
        match front.step(&api, &core, &mut offset) {
            Ok(()) => failures = 0,
            Err(e) => {
                failures += 1;
                let wait = backoff(failures);
                eprintln!("bridge: {e}; trying again in {} s", wait.as_secs());
                std::thread::sleep(wait);
            }
        }
    }
}

#[cfg(test)]
mod tests;
