//! Status facts from OSC 7501, kept per terminal exactly as reported.
//! A report replaces its record; a child record without `app` takes its nearest ancestor's; `clear` removes a record and its children.
//! Title and msg are stripped of controls, bidi and zero-width characters.
//! The turn goes up each time the root record leaves `working`, for any reason.
//! When the program exits only `done` and `error` records stay; a new shell prompt
//! (OSC 133 A) drops `working` and `blocked` ones, and a full reset (RIS) all of them.

use crate::terminals::Terminal;
use ghostty_vt::StatusReport;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const MAX_RECORDS: usize = 256;

/// Record ids by update time, least recent first. A tick per update keeps
/// touching, removing and finding the oldest at O(log n).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordOrder {
    tick: u64,
    ticks: HashMap<String, u64>,
    by_tick: BTreeMap<u64, String>,
}

impl RecordOrder {
    fn touch(&mut self, id: &str) {
        self.remove(id);
        self.tick += 1;
        self.ticks.insert(id.to_string(), self.tick);
        self.by_tick.insert(self.tick, id.to_string());
    }

    fn remove(&mut self, id: &str) {
        if let Some(tick) = self.ticks.remove(id) {
            self.by_tick.remove(&tick);
        }
    }

    fn oldest(&self) -> Option<&String> {
        self.by_tick.values().next()
    }

    fn contains(&self, id: &str) -> bool {
        self.ticks.contains_key(id)
    }

    fn len(&self) -> usize {
        self.ticks.len()
    }

    pub fn clear(&mut self) {
        self.ticks.clear();
        self.by_tick.clear();
    }

    fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        let gone: Vec<String> = self.ticks.keys().filter(|k| !keep(k)).cloned().collect();
        for id in gone {
            self.remove(&id);
        }
    }

    #[cfg(test)]
    pub fn ids(&self) -> Vec<String> {
        self.by_tick.values().cloned().collect()
    }
}

/// One 7501 record. Words are the program's own; nothing is translated.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Status {
    /// idle, working, done, blocked or error.
    pub state: String,
    /// permission, question or auth (blocked only, when the program said).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// 0..100 (working or blocked, when the program said).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub app: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub msg: String,
}

/// Text with nothing that can spoof or hide what a person reads: no C0/C1 controls,
/// bidi overrides, marks and isolates, zero-width spaces, word joiners, invisible
/// math operators, the Mongolian vowel separator or BOM. The joiners ZWNJ/ZWJ
/// (U+200C/D) stay: emoji and Persian or Devanagari words need them.
fn plain(text: &str) -> String {
    let hidden = |c: &char| {
        c.is_control()
            || matches!(
                c,
                '\u{200B}'
                    | '\u{200E}'
                    | '\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{061C}'
                    | '\u{180E}'
                    | '\u{FEFF}'
            )
    };
    text.chars().filter(|c| !hidden(c)).collect()
}

impl Status {
    fn from(r: &StatusReport) -> Status {
        Status {
            state: r.state.into(),
            kind: r.kind.map(Into::into),
            progress: r.progress,
            app: r.app.clone(),
            title: plain(&r.title),
            msg: plain(&r.message),
        }
    }

    /// The report again as OSC 7501, for a new viewer's first paint.
    pub fn encode(&self, id: &str) -> String {
        let mut fields = vec![format!("state={}", self.state)];
        let mut push = |key: &str, value: &str| {
            if !value.is_empty() {
                fields.push(format!("{key}={value}"));
            }
        };
        push("id", id);
        push("kind", self.kind.as_deref().unwrap_or(""));
        push("progress", &self.progress.map(|p| p.to_string()).unwrap_or_default());
        push("app", &self.app);
        push("title", &base64(self.title.as_bytes()));
        push("msg", &base64(self.msg.as_bytes()));
        format!("\x1b]7501;{}\x1b\\", fields.join(":"))
    }
}

/// What a report changed, for events and waiters.
#[derive(Default, Debug, PartialEq)]
pub struct Change {
    pub status: bool,
    /// A fresh root report, not a child record or an unchanged recovery replay.
    pub root_report: bool,
    pub turn_ended: bool,
    /// It just got stuck: blocked on the user or errored (wasn't that before).
    pub stuck: bool,
}

fn working(t: &Terminal) -> bool {
    t.status.as_ref().is_some_and(|s| s.state == "working")
}

fn end_turn(t: &mut Terminal, was_working: bool) -> bool {
    let ended = was_working && !working(t);
    if ended {
        t.turn += 1;
    }
    ended
}

fn stuck(t: &Terminal) -> Option<&str> {
    t.status.as_ref().map(|s| s.state.as_str()).filter(|w| *w == "blocked" || *w == "error")
}

fn word(t: &Terminal) -> Option<String> {
    t.status.as_ref().map(|s| s.state.clone())
}

fn stamp(t: &mut Terminal, was: Option<String>) {
    if word(t) != was {
        t.status_ms = Some(crate::runtime::now_ms());
    }
}

pub fn apply(t: &mut Terminal, r: &StatusReport) -> Change {
    if r.state == "prompt" {
        // A new shell prompt: whatever was running there is over, so working and blocked go.
        return drop_states(t, &["working", "blocked"]);
    }
    let (was_working, was_word) = (working(t), word(t));
    let was = stuck(t).map(String::from);
    t.reported = true;
    if r.state == "clear" {
        if r.id.is_empty() {
            t.status = None;
            t.records.clear();
            t.record_order.clear();
        } else {
            let child = format!("{}/", r.id);
            t.records.retain(|id, _| *id != r.id && !id.starts_with(&child));
            forget_order(t);
        }
    } else if r.id.is_empty() {
        t.status = Some(Status::from(r));
    } else {
        // Over the limit, the record updated longest ago goes. One with no place in
        // `record_order` (saved by an older core) is older than any that has.
        if !t.records.contains_key(&r.id) && t.records.len() >= MAX_RECORDS - 1 {
            // Only a reload leaves records out of the order, so the scan is rare.
            let unordered = (t.record_order.len() < t.records.len())
                .then(|| t.records.keys().find(|k| !t.record_order.contains(k)))
                .flatten();
            let oldest = unordered.or(t.record_order.oldest()).cloned().expect("non-empty");
            t.records.remove(&oldest);
            t.record_order.remove(&oldest);
        }
        let mut status = Status::from(r);
        if status.app.is_empty() {
            status.app = inherited_app(t, &r.id);
        }
        t.records.insert(r.id.clone(), status);
        t.record_order.touch(&r.id);
    }
    stamp(t, was_word);
    Change {
        status: true,
        root_report: r.id.is_empty(),
        turn_ended: end_turn(t, was_working),
        stuck: stuck(t).is_some() && stuck(t) != was.as_deref(),
    }
}

/// The `app` of the nearest ancestor of `id` (a/b/c: a/b, a, then the root) that has one.
fn inherited_app(t: &Terminal, id: &str) -> String {
    let mut parent = id;
    while let Some((up, _)) = parent.rsplit_once('/') {
        parent = up;
        if let Some(app) = t.records.get(parent).map(|s| &s.app).filter(|a| !a.is_empty()) {
            return app.clone();
        }
    }
    t.status.as_ref().map(|s| s.app.clone()).unwrap_or_default()
}

fn forget_order(t: &mut Terminal) {
    t.record_order.retain(|k| t.records.contains_key(k));
}

/// Drops the root and child records whose state is one of `gone`.
fn drop_states(t: &mut Terminal, gone: &[&str]) -> Change {
    let (was_working, was_word) = (working(t), word(t));
    let had = t.records.len() + t.status.is_some() as usize;
    if t.status.as_ref().is_some_and(|s| gone.contains(&s.state.as_str())) {
        t.status = None;
    }
    t.records.retain(|_, s| !gone.contains(&s.state.as_str()));
    forget_order(t);
    stamp(t, was_word);
    let changed = t.records.len() + t.status.is_some() as usize != had;
    Change { status: changed, turn_ended: end_turn(t, was_working), ..Default::default() }
}

/// The program ended: what it was doing is over, but its outcomes (done, error) stand.
pub fn reset(t: &mut Terminal) -> Change {
    drop_states(t, &["working", "blocked", "idle"])
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { TABLE[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::{NewTerminal, Registry};

    fn terminal() -> Terminal {
        let mut r = Registry::default();
        let new = NewTerminal { argv: vec!["x".into()], cwd: "/".into(), ..Default::default() };
        let id = r.create(new, 0).unwrap();
        r.terminals.remove(&id).unwrap()
    }

    fn report(state: &'static str, id: &str) -> StatusReport {
        StatusReport {
            state,
            kind: None,
            progress: None,
            id: id.into(),
            app: String::new(),
            title: String::new(),
            message: String::new(),
        }
    }

    #[test]
    fn the_turn_counts_every_way_of_leaving_working() {
        let mut t = terminal();
        assert!(!apply(&mut t, &report("idle", "")).turn_ended);
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("done", "")).turn_ended); // finished
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("idle", "")).turn_ended); // interrupted
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("blocked", "")).turn_ended); // asks permission
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("clear", "")).turn_ended); // cleared
        apply(&mut t, &report("working", ""));
        assert!(!apply(&mut t, &report("working", "sub")).turn_ended, "child records don't count");
        assert!(reset(&mut t).turn_ended); // exited
        assert_eq!(t.turn, 5);
        assert!(t.status.is_none() && t.reported);
    }

    #[test]
    fn the_status_time_moves_only_when_the_word_changes() {
        let mut t = terminal();
        assert_eq!(t.status_ms, None, "never said anything");
        apply(&mut t, &report("working", ""));
        let since = t.status_ms.expect("its first word");
        t.status_ms = Some(1);
        apply(&mut t, &report("working", ""));
        apply(&mut t, &report("done", "sub"));
        assert_eq!(t.status_ms, Some(1), "the same word again, or a child record");
        apply(&mut t, &report("done", ""));
        assert!(t.status_ms >= Some(since));
        t.status_ms = Some(1);
        reset(&mut t);
        assert_eq!(t.status_ms, Some(1), "exiting keeps a done word, and so its time");
        apply(&mut t, &report("working", ""));
        t.status_ms = Some(1);
        reset(&mut t);
        assert!(t.status_ms > Some(1), "exiting clears a working word");
    }

    #[test]
    fn getting_stuck_is_news_even_outside_a_turn() {
        let mut t = terminal();
        apply(&mut t, &report("idle", ""));
        assert!(apply(&mut t, &report("error", "")).stuck, "e.g. it failed while starting");
        assert!(!apply(&mut t, &report("error", "")).stuck, "once");
        assert!(apply(&mut t, &report("blocked", "")).stuck);
        assert!(!apply(&mut t, &report("idle", "")).stuck);
    }

    #[test]
    fn clear_removes_a_record_and_its_children() {
        let mut t = terminal();
        for id in ["build", "build/test", "buildx", "deploy"] {
            apply(&mut t, &report("working", id));
        }
        apply(&mut t, &report("clear", "build"));
        assert_eq!(t.records.keys().collect::<Vec<_>>(), ["buildx", "deploy"]);
    }

    #[test]
    fn exiting_keeps_outcomes_and_drops_what_was_in_progress() {
        let mut t = terminal();
        assert_eq!(reset(&mut t), Change::default(), "nothing to reset");
        apply(&mut t, &report("done", ""));
        for (id, state) in [("w", "working"), ("b", "blocked"), ("i", "idle"), ("d", "done"), ("e", "error")] {
            apply(&mut t, &report(state, id));
        }
        t.status_ms = Some(1);
        let change = reset(&mut t);
        assert!(change.status && !change.turn_ended && !change.stuck && !change.root_report);
        assert_eq!(t.records.keys().collect::<Vec<_>>(), ["d", "e"]);
        assert_eq!(t.status.as_ref().map(|s| s.state.as_str()), Some("done"));
        assert_eq!(t.status_ms, Some(1), "the root word did not change");
        assert_eq!(reset(&mut t), Change::default(), "a second exit changes nothing");
    }

    #[test]
    fn exiting_while_working_ends_the_turn_and_a_failed_root_stays() {
        let mut t = terminal();
        apply(&mut t, &report("working", ""));
        let change = reset(&mut t);
        assert!(change.status && change.turn_ended);
        assert!(t.status.is_none() && t.reported);
        assert_eq!((t.turn, t.status_ms.is_some()), (1, true));
        apply(&mut t, &report("error", ""));
        assert!(!reset(&mut t).status);
        assert_eq!(t.status.as_ref().map(|s| s.state.as_str()), Some("error"));
        assert_eq!(t.turn, 1);
    }

    #[test]
    fn a_new_shell_prompt_drops_working_and_blocked_records_only() {
        let mut t = terminal();
        let prompt = report("prompt", "");
        assert_eq!(apply(&mut t, &prompt), Change::default(), "nothing to drop");
        assert!(!t.reported, "a prompt is not a report");
        apply(&mut t, &report("working", ""));
        for (id, state) in [("w", "working"), ("b", "blocked"), ("i", "idle"), ("d", "done"), ("e", "error")] {
            apply(&mut t, &report(state, id));
        }
        let change = apply(&mut t, &prompt);
        assert!(change.status && change.turn_ended && !change.root_report && !change.stuck);
        assert!(t.status.is_none());
        assert_eq!(t.records.keys().collect::<Vec<_>>(), ["d", "e", "i"]);
        assert_eq!(apply(&mut t, &prompt), Change::default(), "a repeated prompt changes nothing");
        apply(&mut t, &report("idle", ""));
        assert!(!apply(&mut t, &prompt).status, "an idle root stays");
        assert_eq!(t.turn, 1);
    }

    #[test]
    fn a_full_reset_arrives_as_clear_without_id_and_removes_everything() {
        let mut screen = ghostty_vt::Terminal::new(80, 24, 1 << 20);
        let mut t = terminal();
        screen.write(b"\x1b]7501;state=working\x07\x1b]7501;state=done:id=a/b\x07\x1b]7501;state=error:id=c\x07");
        screen.write(b"\x1bc");
        let reports = screen.take_status_reports();
        assert_eq!(reports.len(), 4, "{reports:?}");
        for r in &reports {
            apply(&mut t, r);
        }
        assert!(t.status.is_none() && t.records.is_empty());
        assert_eq!(t.turn, 1);
    }

    fn report_app(state: &'static str, id: &str, app: &str) -> StatusReport {
        StatusReport { app: app.into(), ..report(state, id) }
    }

    fn app_of(t: &Terminal, id: &str) -> String {
        t.records[id].app.clone()
    }

    #[test]
    fn a_child_takes_the_app_of_its_nearest_ancestor_that_has_one() {
        let mut t = terminal();
        apply(&mut t, &report_app("working", "a", "cargo"));
        apply(&mut t, &report("working", "a/b"));
        apply(&mut t, &report_app("working", "a/b/c", "own"));
        apply(&mut t, &report("working", "a/b/c/d/e"));
        apply(&mut t, &report("working", "a/b/c/d"));
        assert_eq!(app_of(&t, "a/b"), "cargo", "from its parent");
        assert_eq!(app_of(&t, "a/b/c"), "own", "its own app wins");
        assert_eq!(app_of(&t, "a/b/c/d/e"), "own", "the nearest, though a parent in between is missing");
        assert_eq!(app_of(&t, "a/b/c/d"), "own", "the same one once that parent exists");
    }

    #[test]
    fn inheriting_stops_at_missing_or_app_less_ancestors_and_never_matches_a_prefix() {
        let mut t = terminal();
        apply(&mut t, &report_app("working", "ab", "other"));
        apply(&mut t, &report("working", "a"));
        apply(&mut t, &report("working", "a/b"));
        apply(&mut t, &report("working", "x/y/z"));
        assert_eq!(app_of(&t, "a/b"), "", "the parent has none, there is no root");
        assert_eq!(app_of(&t, "x/y/z"), "", "no ancestor exists");
        apply(&mut t, &report_app("idle", "", "root-app"));
        apply(&mut t, &report("working", "a/b"));
        assert_eq!(app_of(&t, "a/b"), "root-app", "the root record is the last ancestor");
        apply(&mut t, &report("working", "a"));
        assert_eq!(app_of(&t, "a"), "root-app");
        assert_eq!(app_of(&t, "ab"), "other", "stored apps are untouched");
        let mut screen = ghostty_vt::Terminal::new(80, 24, 1 << 20);
        screen.write(t.records["a/b"].encode("a/b").as_bytes());
        assert_eq!(screen.take_status_reports()[0].app, "root-app");
    }

    fn said(title: &str, message: &str) -> (String, String) {
        let s = Status::from(&StatusReport { title: title.into(), message: message.into(), ..report("working", "") });
        (s.title, s.msg)
    }

    #[test]
    fn title_and_msg_lose_controls_bidi_and_zero_width_characters() {
        assert_eq!(said("", ""), (String::new(), String::new()), "empty stays empty");
        let kept = "caf\u{e9} \u{2713} \u{1f980}";
        assert_eq!(said("Run tests", kept), ("Run tests".into(), kept.into()));
        // Every range at both ends, and the neighbours just outside each.
        let hidden = ['\0', '\u{1f}', '\n', '\t', '\u{1b}', '\u{7f}', '\u{80}', '\u{9f}']
            .into_iter()
            .chain(['\u{200B}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2066}', '\u{2069}', '\u{FEFF}']);
        for c in hidden {
            assert_eq!(said(&format!("a{c}b"), &format!("{c}x{c}")), ("ab".into(), "x".into()), "{c:?}");
        }
        let shown = [
            ' ', '\u{a0}', '\u{200A}', '\u{2010}', '\u{2029}', '\u{202F}', '\u{2065}', '\u{206A}', '\u{FEFE}',
            '\u{FF00}',
        ];
        for c in shown {
            let text = format!("a{c}b");
            assert_eq!(said(&text, &text), (text.clone(), text), "{c:?}");
        }
        assert_eq!(said("\u{202E}\u{200B}\x07", "\r\n\u{2066}"), (String::new(), String::new()), "nothing left");
        assert_eq!(said("a\u{202E}txt.exe", "x").0, "atxt.exe");
    }

    fn fill(t: &mut Terminal, n: usize) {
        for i in 0..n {
            apply(t, &report("working", &format!("r{i:03}")));
        }
    }

    #[test]
    fn at_the_cap_the_least_recently_updated_record_goes() {
        let cap = MAX_RECORDS - 1;
        let mut t = terminal();
        fill(&mut t, cap);
        assert_eq!(t.records.len(), cap, "exactly N fits");
        apply(&mut t, &report("done", "r000")); // the lexicographically first is now the newest
        apply(&mut t, &report("working", "new")); // N+1
        assert_eq!(t.records.len(), cap);
        assert!(t.records.contains_key("r000") && t.records.contains_key("new"));
        assert!(!t.records.contains_key("r001"), "the oldest update was evicted");
        apply(&mut t, &report("working", "r002")); // an update of a present record evicts nothing
        assert_eq!(t.records.len(), cap);
        assert!(t.records.contains_key("r003"));
        apply(&mut t, &report("working", "newer"));
        assert!(!t.records.contains_key("r003") && t.records.contains_key("r002"));
    }

    #[test]
    fn eviction_order_survives_clears_and_a_reload() {
        let cap = MAX_RECORDS - 1;
        let mut t = terminal();
        fill(&mut t, cap);
        apply(&mut t, &report("clear", "r000")); // frees a slot: nothing evicted next
        apply(&mut t, &report("working", "a"));
        assert_eq!(t.records.len(), cap);
        assert!(t.records.contains_key("r001"));
        // Saved without the order: records it does not know are older than any it does.
        t.record_order.clear();
        apply(&mut t, &report("working", "b"));
        apply(&mut t, &report("working", "c"));
        assert_eq!(t.records.len(), cap);
        assert!(!t.records.contains_key("a") && !t.records.contains_key("r001"), "the unknown ones go first");
        assert!(t.records.contains_key("b") && t.records.contains_key("c") && t.records.contains_key("r002"));
        assert_eq!(t.record_order.len(), 2);
    }

    #[test]
    fn reports_encode_back_to_what_the_parser_reads() {
        let mut screen = ghostty_vt::Terminal::new(80, 24, 1 << 20);
        let sent = StatusReport {
            state: "blocked",
            kind: Some("permission"),
            progress: Some(40),
            id: "a/b".into(),
            app: "claude-code".into(),
            title: "Run tests".into(),
            message: "Apply these changes?".into(),
        };
        screen.write(Status::from(&sent).encode("a/b").as_bytes());
        assert_eq!(screen.take_status_reports(), vec![sent]);
        assert_eq!(base64(b"Apply?"), "QXBwbHk/");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    /// Feeds bytes to a screen and applies the reports it parsed, as the runtime does.
    fn applied(t: &mut Terminal, screen: &mut ghostty_vt::Terminal, bytes: &[u8]) {
        screen.write(bytes);
        for r in screen.take_status_reports() {
            apply(t, &r);
        }
    }

    #[test]
    fn a_prompt_between_working_and_done_ends_the_turn_once_in_one_write_or_split() {
        let sent: &[u8] =
            b"\x1b]7501;state=working:id=job\x1b\\\x1b]7501;state=working\x07\x1b]133;A\x07\x1b]7501;state=done\x1b\\";
        let mut whole = terminal();
        applied(&mut whole, &mut ghostty_vt::Terminal::new(80, 24, 1 << 20), sent);
        assert_eq!(whole.status.as_ref().map(|s| s.state.as_str()), Some("done"));
        assert!(whole.records.is_empty(), "the working child went with the prompt");
        assert_eq!(whole.turn, 1, "the prompt ended the turn; a done after it is no second one");
        for cut in 1..sent.len() {
            let (mut t, mut screen) = (terminal(), ghostty_vt::Terminal::new(80, 24, 1 << 20));
            applied(&mut t, &mut screen, &sent[..cut]);
            applied(&mut t, &mut screen, &sent[cut..]);
            assert_eq!((&t.status, &t.records, t.turn), (&whole.status, &whole.records, whole.turn), "cut at {cut}");
        }
    }

    #[test]
    fn a_shell_prompt_inside_an_agent_drops_its_working_records_too_as_the_spec_says() {
        // The terminal cannot tell whose prompt it is; the spec drops working and blocked
        // records on any OSC 133 A, so a nested shell's prompt ends the agent's turn.
        let mut t = terminal();
        apply(&mut t, &report_app("working", "", "claude-code"));
        apply(&mut t, &report("blocked", "tool"));
        apply(&mut t, &report("done", "tool/earlier"));
        let change = apply(&mut t, &report("prompt", ""));
        assert!(change.status && change.turn_ended);
        assert!(t.status.is_none());
        assert_eq!(t.records.keys().collect::<Vec<_>>(), ["tool/earlier"]);
        assert_eq!(app_of(&t, "tool/earlier"), "claude-code", "inherited when stored, kept after the root went");
        apply(&mut t, &report_app("working", "", "claude-code")); // the agent's next report brings it back
        let change = apply(&mut t, &report("done", ""));
        assert!(change.turn_ended && change.root_report);
        assert_eq!(t.turn, 2, "one turn for the dropped work, one for the real finish");
    }

    #[test]
    fn a_full_reset_inside_a_report_drops_everything_before_it_and_keeps_what_follows() {
        let (mut t, mut screen) = (terminal(), ghostty_vt::Terminal::new(80, 24, 1 << 20));
        applied(
            &mut t,
            &mut screen,
            b"\x1b]7501;state=working\x07\x1b]7501;state=error:id=a\x07\x1b]7501;state=done:id=b:msg=QWxs",
        );
        applied(&mut t, &mut screen, b"\x1bc");
        applied(&mut t, &mut screen, b"bCBkb25l\x1b\\\x1b]7501;state=idle:id=c\x07");
        assert!(t.status.is_none(), "{:?}", t.status);
        assert_eq!(t.records.keys().collect::<Vec<_>>(), ["c"], "only the record reported after the reset");
        assert_eq!(t.turn, 1);
    }

    #[test]
    fn clearing_every_record_forgets_their_order_too() {
        let cap = MAX_RECORDS - 1;
        let mut t = terminal();
        fill(&mut t, cap);
        apply(&mut t, &report("clear", "")); // a full reset (RIS) arrives the same way
        for i in 0..=cap {
            apply(&mut t, &report("working", &format!("n{i:03}")));
        }
        assert_eq!(t.records.len(), cap, "one past the cap evicts, also after everything was cleared");
        assert!(!t.records.contains_key("n000") && t.records.contains_key("n001"));
        // A program that reports and clears over and over must not grow the order without end.
        let mut t = terminal();
        for round in 0..20 {
            for i in 0..10 {
                apply(&mut t, &report("done", &format!("x{round}-{i}")));
            }
            apply(&mut t, &report("clear", ""));
        }
        assert_eq!(t.record_order.ids(), Vec::<String>::new());
    }

    #[test]
    fn after_a_reload_the_cap_holds_at_256_records_with_the_root() {
        let mut t = terminal();
        apply(&mut t, &report("idle", ""));
        fill(&mut t, MAX_RECORDS - 2); // 254 children and the root: 255 records
        let mut t: Terminal = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert!(t.record_order.ids().is_empty(), "the order is not saved");
        assert_eq!(t.records.len(), MAX_RECORDS - 2);
        apply(&mut t, &report("working", "x")); // 256: the cap, nothing goes
        assert_eq!(t.records.len(), MAX_RECORDS - 1);
        assert!(t.records.contains_key("r000"));
        apply(&mut t, &report("working", "y")); // 257 would be one too many: the first unknown goes
        assert_eq!(t.records.len(), MAX_RECORDS - 1);
        assert!(!t.records.contains_key("r000") && t.records.contains_key("x"));
        apply(&mut t, &report("done", "r001")); // updated: now newer than every unknown one
        apply(&mut t, &report("working", "z"));
        assert_eq!(t.records.len(), MAX_RECORDS - 1);
        assert!(!t.records.contains_key("r002"));
        assert!(["x", "y", "z", "r001"].iter().all(|k| t.records.contains_key(*k)));
        assert_eq!(t.record_order.ids(), ["x", "y", "r001", "z"]);
        assert!(t.status.is_some(), "the root is never evicted");
    }

    #[test]
    fn a_child_reported_before_its_ancestor_keeps_no_app_until_it_reports_again() {
        // Known limitation: the app is inherited when the child is stored, not when it is shown.
        let mut t = terminal();
        apply(&mut t, &report("working", "a/b"));
        apply(&mut t, &report_app("working", "a", "cargo"));
        assert_eq!(app_of(&t, "a/b"), "", "the parent came later");
        apply(&mut t, &report("done", "a/b"));
        assert_eq!(app_of(&t, "a/b"), "cargo");
        apply(&mut t, &report_app("working", "a", "rustc"));
        assert_eq!(app_of(&t, "a/b"), "cargo", "nor does it follow a parent that changes its app");
        apply(&mut t, &report("clear", "a"));
        assert!(t.records.is_empty());
    }

    #[test]
    fn title_and_msg_keep_combining_marks_variation_selectors_and_text_around_the_surrogates() {
        let kept = [
            "e\u{301}",
            "\u{2764}\u{fe0f}",
            "\u{d7ff}\u{e000}\u{10ffff}",
            "\u{1f1fa}\u{1f1f8}",
            "\u{5d0}\u{5d1} abc",
            r"C:\Users\me\src\main.rs",
        ];
        for text in kept {
            assert_eq!(said(text, text), (text.into(), text.into()), "{text:?}");
        }
        assert_eq!(said("\u{200b}e\u{301}\u{202e}", "").0, "e\u{301}", "a mark keeps its base");
    }

    #[test]
    fn joiners_that_build_emoji_and_words_stay() {
        // U+200D joins a family emoji into one; U+200C is part of Persian and Indic spelling.
        // Neither reorders nor hides text, which is what the spec asks to disarm.
        let joined = [
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}",
            "\u{915}\u{94d}\u{200d}\u{937}",
        ];
        for text in joined {
            assert_eq!(said(text, text), (text.into(), text.into()), "{text:?}");
        }
    }

    #[test]
    fn the_arabic_letter_mark_and_the_word_joiner_go_like_their_siblings() {
        // U+061C is a bidi mark like U+200E and U+200F; U+2060 is zero-width like U+FEFF.
        for c in ['\u{61c}', '\u{2060}', '\u{2061}', '\u{2062}', '\u{2063}', '\u{2064}', '\u{180E}'] {
            assert_eq!(said(&format!("a{c}b"), &format!("{c}x")), ("ab".into(), "x".into()), "{c:?}");
        }
    }

    #[test]
    fn length_limits_count_the_text_as_sent_and_stripping_only_shortens_it() {
        let send = |title: &str, msg: &str| {
            let mut screen = ghostty_vt::Terminal::new(80, 24, 1 << 20);
            let (title, msg) = (base64(title.as_bytes()), base64(msg.as_bytes()));
            screen.write(format!("\x1b]7501;state=working:title={title}:msg={msg}\x1b\\").as_bytes());
            screen.take_status_reports().first().map(Status::from).map(|s| (s.title, s.msg))
        };
        let (title, msg) = ("\u{200b}".repeat(64), format!("{}ab", "\u{200b}".repeat(682)));
        assert_eq!((title.len(), msg.len()), (192, 2048));
        assert_eq!(send(&title, &msg), Some((String::new(), "ab".into())), "at the limits, all invisible");
        assert_eq!(send(&format!("{title}a"), "x"), None, "a title one byte over is refused whole");
        assert_eq!(send("x", &format!("{msg}a")), None, "a msg one byte over is refused whole");
    }
}
