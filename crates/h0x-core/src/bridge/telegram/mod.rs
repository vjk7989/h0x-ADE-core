//! Telegram frontend (`h0x-core bridge run`): long-polls the Bot API, turns
//! private messages and button taps from paired accounts into bridge
//! commands, and sends notices with buttons to every paired account.
//! Outbound only. Update payloads and the token are never logged; the audit
//! log is the bridge's.

mod api;

pub use api::Api;

use super::{Bridge, Command, Kind, Notice, Reply, Sender, Session};
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

/// Buttons in one message (Telegram allows 100).
const MAX_BUTTONS: usize = 40;

/// The bot's command menu (setMyCommands).
const MENU: [(&str, &str); 8] = [
    ("menu", "Sessions, projects and help"),
    ("sessions", "Pick a terminal to type into"),
    ("projects", "Browse by folder"),
    ("help", "All commands"),
    ("agents", "Terminals: agent, folder, status"),
    ("read", "Last lines of a terminal"),
    ("send", "Type text into a terminal"),
    ("lock", "Refuse remote commands"),
];

const HELP: &str = "Commands:
/menu - buttons for sessions, projects and help
/sessions - pick a terminal; then plain messages are typed into it
/projects (/folders) - pick a folder, then one of its terminals
/agents - terminals: agent, folder, status
/read <t> [lines] - its last lines (default 20)
/send <t> <text> - type text and press enter
/keys <t> <keys...> - enter esc ctrl+c up down left right y n
/approve <t>, /deny <t> - answer what it is waiting on
/interrupt <t> - press its stop key
/run <folder> <command> - start an agent or command (\"quote\" a folder with spaces)
/kill <t> - end its program
/lock - refuse every remote command until unlocked at the machine
Reply to a notice to type into its terminal, or select one with /sessions and just write.";

/// The secret name of the bot token.
const TOKEN: &str = "telegram.token";

impl Bridge {
    /// The bot token, if one is stored.
    pub fn telegram_token(&self) -> Result<Option<String>, RpcError> {
        match self.secret_get(TOKEN) {
            Ok(found) => Ok(found
                .and_then(|b| String::from_utf8(b).ok())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())),
            Err(e) => Err(super::failed(e)),
        }
    }

    /// Stores the bot token from @BotFather (OS secret store). It must look like one:
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
        self.secret_put(TOKEN, token.as_bytes()).map_err(super::failed)
    }

    /// Removes the bot token: whether there was one.
    pub fn clear_telegram_token(&self) -> Result<bool, RpcError> {
        self.secret_del(TOKEN).map_err(super::failed)
    }
}

/// What a chat message asks for.
#[derive(Debug, PartialEq)]
enum Ask {
    Pair(String),
    Help,
    Menu,
    Sessions,
    Projects,
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
        "menu" => Ask::Menu,
        "sessions" => Ask::Sessions,
        "projects" | "folders" => Ask::Projects,
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
            // What follows that line end may still leave no room: cut right here.
            if len + units > budget {
                out.push(std::mem::take(&mut cur));
                len = 0;
            }
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
        // Only a listing asks for these, and it takes them apart itself.
        Reply::NotPaired | Reply::Sessions(_) => Vec::new(),
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

/// The menu message: three buttons.
fn menu(chat: i64) -> Out {
    let row = vec![button("Sessions", "m:s".into()), button("Projects", "m:p".into()), button("Help", "m:h".into())];
    Out::Send { chat, html: "Menu".into(), buttons: vec![row], about: None }
}

/// What a session is called: its name, else its address.
fn called(s: &Session) -> &str {
    if s.name.is_empty() { &s.address } else { &s.name }
}

/// `text` cut to at most `n` characters, with `…` where it was cut.
fn shorten(text: &str, n: usize) -> String {
    match text.char_indices().nth(n) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.into(),
    }
}

/// A path for comparing: one kind of separator, none at the end, and on a
/// drive path (`C:/..`) no case.
fn normal(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    if path.as_bytes().get(1) == Some(&b':') { path.to_ascii_lowercase() } else { path.into() }
}

/// Whether `path` is `folder` or inside it.
fn within(path: &str, folder: &str) -> bool {
    let (path, folder) = (normal(path), normal(folder));
    path == folder || path.strip_prefix(&folder).is_some_and(|rest| rest.starts_with('/'))
}

/// The sessions as a message with one button each (`folder`: the ones in it).
fn sessions_out(chat: i64, folder: Option<&str>, shown: &[Session]) -> Out {
    if shown.is_empty() {
        let none = if folder.is_some() { "No sessions in that folder." } else { "No sessions." };
        return text(chat, none.into());
    }
    let mut title = folder.map_or("Sessions".to_string(), |f| format!("Sessions in {f}"));
    if shown.len() > MAX_BUTTONS {
        title.push_str(&format!(" (first {MAX_BUTTONS} of {})", shown.len()));
    }
    let buttons = shown
        .iter()
        .take(MAX_BUTTONS)
        .map(|s| {
            let label = format!("{} · {} · {}", called(s), s.agent, s.status);
            vec![button(&shorten(&label, 60), format!("t:{}", s.address))]
        })
        .collect();
    Out::Send { chat, html: escape(&title), buttons, about: None }
}

/// The folders as a message with one button each: the spaces, then the
/// folders of live terminals no space holds.
fn projects_out(chat: i64, spaces: Vec<super::Space>, all: &[Session]) -> Out {
    let mut folders: Vec<(String, String)> = spaces.into_iter().map(|s| (s.name, s.folder)).collect();
    for s in all.iter().filter(|s| s.live && !s.folder.is_empty()) {
        if !folders.iter().any(|(_, f)| within(&s.folder, f)) {
            let leaf = s.folder.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default();
            folders.push((if leaf.is_empty() { s.folder.clone() } else { leaf.into() }, s.folder.clone()));
        }
    }
    if folders.is_empty() {
        return text(chat, "No projects.".into());
    }
    let mut title = "Projects".to_string();
    if folders.len() > MAX_BUTTONS {
        title.push_str(&format!(" (first {MAX_BUTTONS} of {})", folders.len()));
    }
    let buttons = folders
        .into_iter()
        .take(MAX_BUTTONS)
        .map(|(name, folder)| {
            let n = all.iter().filter(|s| within(&s.folder, &folder)).count();
            vec![button(&shorten(&format!("{name} ({n})"), 60), format!("p:{folder}"))]
        })
        .collect();
    Out::Send { chat, html: title, buttons, about: None }
}

/// What sent messages stand for, both kept for the last 1000: notice
/// messages by (chat, message id) and the terminals they are about, and
/// button ids and the action each stands for (`a:<terminal>`, `c:<token>`...).
/// Telegram takes at most 64 bytes of button data, and an address can be
/// longer, so buttons carry only `k:<id>`.
#[derive(Default)]
struct Replies {
    terminals: HashMap<(i64, i64), String>,
    order: VecDeque<(i64, i64)>,
    buttons: HashMap<u64, String>,
    pressed: VecDeque<u64>,
    last: u64,
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

    /// The short data for a button standing for `action`.
    fn button(&mut self, action: String) -> String {
        if self.pressed.len() >= REMEMBERED
            && let Some(old) = self.pressed.pop_front()
        {
            self.buttons.remove(&old);
        }
        self.last += 1;
        self.pressed.push_back(self.last);
        self.buttons.insert(self.last, action);
        format!("k:{}", self.last)
    }

    /// The action behind button data: `k:<id>` looked up, anything else as it is.
    fn action(&self, data: &str) -> Option<String> {
        match data.strip_prefix("k:") {
            Some(id) => id.parse().ok().and_then(|id| self.buttons.get(&id)).cloned(),
            None => Some(data.into()),
        }
    }
}

/// The accounts a notice goes to: every paired Telegram account (its chat
/// is its user id), none while the bridge is locked.
fn notice_chats(locked: bool, identities: &[super::Identity]) -> Vec<i64> {
    if locked {
        return Vec::new();
    }
    identities.iter().filter(|i| i.transport == "telegram").filter_map(|i| i.user_id.parse().ok()).collect()
}

/// A command for another bot in the chat (`/agents@OtherBot`): not ours.
/// `own` is this bot's username, from getMe.
fn for_another_bot(text: &str, own: &str) -> bool {
    let word = text.trim_start().split(char::is_whitespace).next().unwrap_or_default();
    match word.strip_prefix('/').and_then(|w| w.split_once('@')) {
        Some((_, name)) => !name.eq_ignore_ascii_case(own),
        None => false,
    }
}

struct Frontend {
    bridge: Bridge,
    replies: Arc<Mutex<Replies>>,
    /// This bot's username (getMe): commands addressed to other bots are ignored.
    own: String,
    /// The terminal each paired account (by user id) selected: plain text goes there.
    selected: HashMap<String, String>,
}

type Core<'a> = &'a dyn Fn(&str, Value) -> Result<Value, RpcError>;

impl Frontend {
    fn paired(&self, user: &str) -> bool {
        self.bridge.identities().is_ok_and(|ids| ids.iter().any(|i| i.transport == "telegram" && i.user_id == user))
    }

    /// Selections that no longer stand: every one while locked, and an unpaired account's.
    fn forget_selections(&mut self) {
        if self.selected.is_empty() {
            return;
        }
        let ids = self.bridge.identities().unwrap_or_default();
        let locked = self.bridge.locked().unwrap_or(true);
        self.selected.retain(|user, _| !locked && ids.iter().any(|i| i.transport == "telegram" && &i.user_id == user));
    }

    /// Every terminal, through the bridge's gates; else what to tell the account instead.
    /// A selected terminal that is no longer live is forgotten.
    fn sessions(&mut self, core: Core, from: &Sender, chat: i64, now: u64) -> Result<Vec<Session>, Vec<Out>> {
        match self.bridge.execute(&core, from, Command::Sessions, now) {
            Reply::Sessions(all) => {
                self.selected.retain(|_, t| all.iter().any(|s| s.live && &s.address == t));
                Ok(all)
            }
            other => Err(replied(chat, other, false)),
        }
    }

    /// The sessions (those in `folder`, if given) as buttons.
    fn sessions_view(&mut self, core: Core, from: &Sender, chat: i64, now: u64, folder: Option<&str>) -> Vec<Out> {
        match self.sessions(core, from, chat, now) {
            Ok(all) => {
                let shown: Vec<Session> =
                    all.into_iter().filter(|s| folder.is_none_or(|f| within(&s.folder, f))).collect();
                vec![sessions_out(chat, folder, &shown)]
            }
            Err(said) => said,
        }
    }

    fn projects_view(&mut self, core: Core, from: &Sender, chat: i64, now: u64) -> Vec<Out> {
        match self.sessions(core, from, chat, now) {
            Ok(all) => vec![projects_out(chat, self.bridge.spaces(), &all)],
            Err(said) => said,
        }
    }

    /// A tap on a session: it becomes what this account's plain text goes to.
    fn select(&mut self, core: Core, from: &Sender, chat: i64, now: u64, address: &str) -> Vec<Out> {
        let all = match self.sessions(core, from, chat, now) {
            Ok(all) => all,
            Err(said) => return said,
        };
        let say = |line: String| vec![text(chat, escape(&line))];
        match all.iter().find(|s| s.address == address) {
            None => say("That session is gone.".into()),
            Some(s) if !s.live => say(format!("{} has finished; there is nothing to type into.", called(s))),
            Some(s) => {
                self.selected.insert(from.user_id.clone(), s.address.clone());
                say(format!("Selected {}. Send a message to type it there.", called(s)))
            }
        }
    }

    /// What to tell Telegram about one update. Group chats and unpaired
    /// accounts (except `/pair`) get nothing at all.
    fn handle(&mut self, core: Core, update: &Value, now: u64) -> Vec<Out> {
        self.forget_selections();
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
        // Private chats with people only, and commands for this bot (or none in particular).
        if m["chat"]["type"] != "private" || m["from"]["is_bot"] == true || for_another_bot(typed, &self.own) {
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
            Some(Ask::Menu) => return vec![menu(chat)],
            Some(Ask::Sessions) => return self.sessions_view(core, &from, chat, now, None),
            Some(Ask::Projects) => return self.projects_view(core, &from, chat, now),
            Some(Ask::Usage(usage)) => return vec![text(chat, escape(&format!("Usage: {usage}")))],
            Some(Ask::Pair(_)) => unreachable!("answered above"),
            None => {
                let to = m["reply_to_message"]["message_id"].as_i64();
                let terminal = to.and_then(|id| self.replies.lock().unwrap().terminals.get(&(chat, id)).cloned());
                match terminal.or_else(|| self.selected.get(&from.user_id).cloned()) {
                    Some(terminal) => {
                        // Typed into the selection: a terminal the core no longer has ends it.
                        let reply =
                            self.bridge.execute(&core, &from, Command::Send { terminal, text: typed.into() }, now);
                        if matches!(reply, Reply::Failed(_)) && to.is_none() {
                            self.selected.remove(&from.user_id);
                        }
                        return replied(chat, reply, false);
                    }
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
        if matches!(command, Command::Lock) {
            self.selected.clear();
        }
        replied(chat, self.bridge.execute(&core, &from, command, now), pre)
    }

    fn callback(&mut self, core: Core, q: &Value, now: u64) -> Vec<Out> {
        let message = &q["message"];
        let (Some(chat), Some(id), Some(user), Some(data), Some(callback)) = (
            message["chat"]["id"].as_i64(),
            message["message_id"].as_i64(),
            q["from"]["id"].as_i64(),
            q["data"].as_str(),
            q["id"].as_str(),
        ) else {
            return Vec::new();
        };
        let user = user.to_string();
        if message["chat"]["type"] != "private" || q["from"]["is_bot"] == true || !self.paired(&user) {
            return Vec::new();
        }
        let mut out = vec![Out::Answer { callback: callback.into() }];
        // A button we no longer remember does nothing.
        let Some(data) = self.replies.lock().unwrap().action(data) else { return out };
        let (kind, arg) = data.split_once(":").unwrap_or((data.as_str(), ""));
        let terminal = arg.to_string();
        let from = Sender { transport: "telegram".into(), user_id: user };
        let command = match kind {
            "t" => {
                out.extend(self.select(core, &from, chat, now, arg));
                return out;
            }
            "p" => {
                out.extend(self.sessions_view(core, &from, chat, now, Some(arg)));
                return out;
            }
            "m" => {
                match arg {
                    "s" => out.extend(self.sessions_view(core, &from, chat, now, None)),
                    "p" => out.extend(self.projects_view(core, &from, chat, now)),
                    "h" => out.push(text(chat, escape(HELP))),
                    _ => {}
                }
                return out;
            }
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
pub(super) fn backoff(failures: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(failures.saturating_sub(1)).min(60))
}

/// Sends one thing; remembers which terminal a notice is about.
fn deliver(api: &Api, replies: &Mutex<Replies>, out: Out) -> Result<(), String> {
    match out {
        Out::Send { chat, html, buttons, about } => {
            let mut body = json!({"chat_id": chat, "text": html, "parse_mode": "HTML"});
            if !buttons.is_empty() {
                let mut memory = replies.lock().unwrap();
                let mut rows = Vec::new();
                for row in buttons {
                    let row: Vec<Value> = row
                        .into_iter()
                        .map(|(text, action)| json!({"text": text, "callback_data": memory.button(action)}))
                        .collect();
                    rows.push(row);
                }
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
                // An unreadable state counts as locked: nothing goes out.
                let locked = bridge.locked().unwrap_or(true);
                for chat in notice_chats(locked, &bridge.identities().unwrap_or_default()) {
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

/// Tells Telegram the command menu. A failure is only logged: the bot works without it.
fn set_menu(api: &Api) {
    let commands: Vec<Value> = MENU.iter().map(|(c, d)| json!({"command": c, "description": d})).collect();
    if let Err(e) = api.call("setMyCommands", &json!({"commands": commands}), 30) {
        eprintln!("bridge: setMyCommands: {e}");
    }
}

/// `h0x-core bridge run`: the bot, until the process is stopped.
pub fn run(paths: &Paths) -> Result<(), RpcError> {
    let bridge = Bridge::open(paths)?;
    let token = bridge
        .telegram_token()?
        .ok_or_else(|| RpcError::new("bridge", "no bot token", "store one first: h0x-core bridge telegram token"))?;
    if Bridge::secrets_in_file() {
        eprintln!("bridge: no usable OS secret store; the bot token is in an owner-only file");
    }
    let api = Api::new(token)?;
    // Who we are, so commands for other bots in a chat are left alone.
    let mut failures = 0;
    let own = loop {
        match api.call("getMe", &json!({}), 30) {
            Ok(me) if me["username"].is_string() => break me["username"].as_str().unwrap_or_default().to_string(),
            Ok(_) => return Err(RpcError::new("bridge", "getMe: no username", "check the bot token")),
            Err(e) => {
                failures += 1;
                let wait = backoff(failures);
                eprintln!("bridge: {e}; trying again in {} s", wait.as_secs());
                std::thread::sleep(wait);
            }
        }
    };
    set_menu(&api);
    let replies = Arc::new(Mutex::new(Replies::default()));
    let (for_notices, shared) = (api.clone(), replies.clone());
    std::thread::spawn(move || notices(for_notices, shared));
    let core = |method: &str, params: Value| {
        let stream = crate::client::connect_or_start(paths)?;
        crate::client::call(&stream, method, params).map_err(|e| crate::client::explain_closed(paths, e))
    };
    let mut front = Frontend { bridge, replies, own, selected: HashMap::new() };
    let (mut offset, mut failures) = (0, 0u32);
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
