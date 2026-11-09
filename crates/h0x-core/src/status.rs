//! Status facts from OSC 7501, kept per terminal exactly as reported.
//! A report replaces its record; `clear` removes a record and its children.
//! The turn goes up each time the root record leaves `working`, for any reason.
//! When the program exits only `done` and `error` records stay; a new shell prompt
//! (OSC 133 A) drops `working` and `blocked` ones, and a full reset (RIS) all of them.

use crate::terminals::Terminal;
use ghostty_vt::StatusReport;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_RECORDS: usize = 256;

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

impl Status {
    fn from(r: &StatusReport) -> Status {
        Status {
            state: r.state.into(),
            kind: r.kind.map(Into::into),
            progress: r.progress,
            app: r.app.clone(),
            title: r.title.clone(),
            msg: r.message.clone(),
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
            let unordered = t.records.keys().find(|k| !t.record_order.contains(k));
            let oldest = unordered.or(t.record_order.first()).cloned().expect("non-empty");
            t.records.remove(&oldest);
            forget_order(t);
        }
        t.records.insert(r.id.clone(), Status::from(r));
        t.record_order.retain(|k| *k != r.id);
        t.record_order.push(r.id.clone());
    }
    stamp(t, was_word);
    Change {
        status: true,
        root_report: r.id.is_empty(),
        turn_ended: end_turn(t, was_working),
        stuck: stuck(t).is_some() && stuck(t) != was.as_deref(),
    }
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
}
