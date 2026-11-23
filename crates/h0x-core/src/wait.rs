//! `wait`: until a terminal is not working, driven by its events so even a
//! quick flicker counts. Already not working: it returns at once.

use crate::events::{Filter, Sink};
use crate::runtime::Core;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Output kept for `output` matching.
const SEEN_MAX: usize = 64 * 1024;

pub struct WaitSpec {
    /// Ends when output produced after the wait started contains this; then
    /// only blocked, error or exit end it otherwise.
    pub output: Option<String>,
    pub timeout: Duration,
    /// End at once if this turn already ended (a wait right after a send).
    pub after_turn: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct WaitResult {
    pub terminal: String,
    /// False on timeout: a check-in, the terminal is still running.
    pub ended: bool,
    /// status, exit (including program loss in a kept terminal), output, timeout,
    /// or not_started (after a send: it did not start working; a plain wait
    /// still follows that send's turn); with
    /// `children`: children; message (someone messaged the waiter, any
    /// wait); for each child in `children`: status, exit, or running (still
    /// working).
    pub reason: String,
    /// The exact word the program reported (current word on timeout or output).
    pub status: Option<String>,
    pub turn: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// With `children`: one result per child it reports on.
    pub children: Vec<WaitResult>,
    /// With a message: the sender's address (the text is already in your input);
    /// null when a person sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Whether a status change from `prev` to `new` ends the wait.
fn ends(prev: Option<&str>, new: Option<&str>) -> bool {
    new != Some("working") && (prev == Some("working") || new != prev)
}

/// Blocked or errored: no output is coming, the caller has to look.
fn stuck(word: Option<&str>) -> bool {
    matches!(word, Some("blocked" | "error"))
}

/// The text of a chunk of output after what the last one `held`: a character
/// cut at the chunk's end waits there for the rest instead of becoming U+FFFD.
fn text_of(held: &mut Vec<u8>, bytes: &[u8]) -> String {
    held.extend_from_slice(bytes);
    // Only a start that more bytes could still make valid; anything else is U+FFFD now.
    let open =
        |at: usize| std::str::from_utf8(&held[at..]).is_err_and(|e| e.valid_up_to() == 0 && e.error_len().is_none());
    let at = (held.len().saturating_sub(3)..held.len()).find(|at| open(*at)).unwrap_or(held.len());
    let rest = held.split_off(at);
    let text = strip(held);
    *held = rest;
    text
}

/// Output without escape sequences, for matching what a person would read.
fn strip(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if !c.is_control() || c == '\n' {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            Some('[') => while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {},
            Some(']') | Some('P') | Some('_') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A wait's subscription and output watch, gone with it however it returns.
struct Registered<'a> {
    core: &'a Core,
    id: u32,
    events: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    output: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        let mut st = self.core.lock();
        self.core.unsubscribe_waiter(&mut st, &self.events);
        if let Some(watchers) = st.output_watch.get_mut(&self.id) {
            watchers.retain(|w| !w.same_channel(&self.output));
            if watchers.is_empty() {
                st.output_watch.remove(&self.id);
            }
        }
    }
}

impl Core {
    /// A wait's answer from the terminal's current facts; `word` overrides its status.
    pub fn wait_result(&self, id: u32, ended: bool, reason: &str, word: Option<Option<String>>) -> WaitResult {
        let st = self.lock();
        let t = st.reg.terminals.get(&id);
        WaitResult {
            terminal: crate::terminals::address(id),
            ended,
            reason: reason.into(),
            status: word.unwrap_or_else(|| t.and_then(|t| t.status.as_ref().map(|s| s.state.clone()))),
            turn: t.map_or(0, |t| t.turn),
            exit_code: t.and_then(|t| t.exit_code),
            signal: t.and_then(|t| t.signal),
            children: Vec::new(),
            from: None,
        }
    }

    /// A message typed into `waiter` while it works ends the wait too: it
    /// sits in the waiter's queue until this returns.
    pub async fn wait(&self, id: u32, spec: WaitSpec, waiter: Option<u32>) -> WaitResult {
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut output) = tokio::sync::mpsc::unbounded_channel();
        let start = {
            let mut st = self.lock();
            self.subscribe(
                &mut st,
                Filter { terminal: Some(crate::terminals::address(id)), ..Default::default() },
                Sink::Waiter(tx.clone()),
            );
            if spec.output.is_some() {
                st.output_watch.entry(id).or_default().push(out_tx.clone());
            }
            st.reg.terminals.get(&id).cloned()
        };
        let _registered = Registered { core: self, id, events: tx, output: out_tx };
        let result =
            |ended: bool, reason: &str, word: Option<Option<String>>| self.wait_result(id, ended, reason, word);
        let Some(start) = start.filter(|t| t.is_live() && !t.program_lost()) else {
            return result(true, "exit", None);
        };
        if spec.after_turn.is_some_and(|k| start.turn > k) {
            return result(true, "status", None);
        }
        // Already not working: that is the answer. A program that never reported
        // is working until it exits. Not after a send whose turn hasn't started:
        // idle then is the old turn's word.
        let now = start.status.as_ref().map(|s| s.state.as_str());
        let pending = spec.after_turn.is_some() && !stuck(now);
        let already =
            if spec.output.is_some() { stuck(now) } else { start.reported && !pending && ends(Some("working"), now) };
        if already {
            return result(true, "status", None);
        }
        let deadline = tokio::time::Instant::now() + spec.timeout;
        let mut prev = start.status.map(|s| s.state);
        let mut seen = String::new();
        let mut held = Vec::new();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(from) = waiter.and_then(|w| self.lock().messages.remove(&w)) {
                return WaitResult { from, ..result(false, "message", None) };
            }
            tokio::select! {
                _ = changed, if waiter.is_some() => {}
                event = events.recv() => {
                    let Some(event) = event else { return result(true, "exit", None) };
                    match event["type"].as_str() {
                        Some("status") => {
                            let new = event["data"]["status"]["state"].as_str().map(String::from);
                            let stop = if spec.output.is_some() { stuck(new.as_deref()) } else { ends(prev.as_deref(), new.as_deref()) };
                            // A finish clears the status just before it says how the program
                            // ended: that is an exit, and its finished or lost event follows.
                            let gone = || !self.lock().reg.terminals.get(&id).is_some_and(|t| t.is_live() && !t.program_lost());
                            if stop && !(new.is_none() && gone()) {
                                return result(true, "status", Some(new));
                            }
                            prev = new;
                        }
                        Some("finished") | Some("lost") | Some("removed") => {
                            // With --rm the record may already be gone: the event says how it ended.
                            let mut r = result(true, "exit", None);
                            let data = &event["data"];
                            r.exit_code = r.exit_code.or(data["exit_code"].as_i64().map(|c| c as i32));
                            r.signal = r.signal.or(data["signal"].as_i64().map(|s| s as i32));
                            r.turn = r.turn.max(start.turn);
                            return r;
                        }
                        _ => {}
                    }
                }
                Some(bytes) = output.recv() => {
                    seen.push_str(&text_of(&mut held, &bytes));
                    if spec.output.as_deref().is_some_and(|needle| seen.contains(needle)) {
                        return result(true, "output", None);
                    }
                    if seen.len() > SEEN_MAX {
                        let cut = seen.len() - SEEN_MAX / 2;
                        let cut = (cut..seen.len()).find(|i| seen.is_char_boundary(*i)).unwrap_or(0);
                        seen.drain(..cut);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => return result(false, "timeout", None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_ends_a_wait() {
        assert!(ends(Some("working"), Some("done")));
        assert!(ends(Some("working"), Some("blocked")));
        assert!(ends(Some("working"), Some("idle")));
        assert!(ends(Some("working"), Some("error")));
        assert!(ends(Some("working"), None), "cleared counts as not working");
        assert!(!ends(Some("idle"), Some("idle")), "a repeated word is no change");
        assert!(!ends(Some("idle"), Some("working")));
        assert!(ends(Some("idle"), Some("done")));
    }

    #[test]
    fn output_is_matched_without_escape_sequences() {
        assert_eq!(strip(b"\x1b[31mRE\x1b[0mADY\x1b]2;t\x07!\r\n"), "READY!\n");
    }

    #[test]
    fn a_character_cut_between_chunks_is_matched_whole() {
        let out = "é 준비 ✓ 완료 🚀\n".as_bytes();
        for cut in 0..=out.len() {
            let mut held = Vec::new();
            let text = text_of(&mut held, &out[..cut]) + &text_of(&mut held, &out[cut..]);
            assert_eq!(text, "é 준비 ✓ 완료 🚀\n", "cut at byte {cut}");
            assert!(held.is_empty());
        }
        let mut held = Vec::new();
        let text: String = out.iter().map(|b| text_of(&mut held, &[*b])).collect();
        assert_eq!(text, "é 준비 ✓ 완료 🚀\n", "one byte at a time");
        for bad in [&b"a\xc0"[..], b"a\xe0\x80", b"a\xf4\x90"] {
            assert_eq!(text_of(&mut held, bad), String::from_utf8_lossy(bad), "{bad:?} can never become a character");
            assert!(held.is_empty());
        }
    }

    /// Feeds `chunks` one after another; also what is still held at the end.
    fn fed(chunks: &[&[u8]]) -> (String, Vec<u8>) {
        let mut held = Vec::new();
        let text = chunks.iter().map(|c| text_of(&mut held, c)).collect();
        (text, held)
    }

    #[test]
    fn empty_chunks_add_nothing_and_keep_what_is_held() {
        assert_eq!(fed(&[]), (String::new(), vec![]));
        assert_eq!(fed(&[b""]), (String::new(), vec![]));
        assert_eq!(fed(&[b"\xe2", b"", b""]), (String::new(), vec![0xe2]), "an empty chunk is no stream end");
        assert_eq!(fed(&[b"\xe2", b"", b"\x9c\x93"]), ("\u{2713}".into(), vec![]));
    }

    #[test]
    fn each_width_alone_at_the_very_end_survives_any_three_way_cut() {
        for whole in ["\u{e9}", "\u{2713}", "\u{1f680}", "a\u{e9}", "a\u{2713}", "a\u{1f680}"] {
            let b = whole.as_bytes();
            for i in 0..=b.len() {
                for j in i..=b.len() {
                    let got = fed(&[&b[..i], &b[i..j], &b[j..]]);
                    assert_eq!(got, (whole.to_string(), vec![]), "{whole:?} cut at {i} and {j}");
                }
            }
        }
    }

    #[test]
    fn an_incomplete_character_at_stream_end_is_held_not_dropped() {
        let cases: [(&[u8], &[u8]); 3] =
            [(b"ok\xc3", b"\xc3"), (b"ok\xe2\x9c", b"\xe2\x9c"), (b"ok\xf0\x9f\x9a", b"\xf0\x9f\x9a")];
        for (start, kept) in cases {
            let mut held = Vec::new();
            assert_eq!(text_of(&mut held, start), "ok");
            assert_eq!(held, kept, "the cut character waits for its rest");
            // Something that cannot finish it: the held bytes come out as U+FFFD, then the rest.
            assert_eq!(text_of(&mut held, b"!"), "\u{fffd}!", "{start:?}");
            assert!(held.is_empty());
        }
        // A character's start followed by another start: the first can never finish.
        assert_eq!(fed(&[b"\xe2", b"\xf0\x9f"]), ("\u{fffd}".into(), vec![0xf0, 0x9f]));
    }

    #[test]
    fn invalid_bytes_anywhere_read_as_the_whole_stream_would() {
        let stream: &[u8] =
            b"\xff\x80a\xc3\xa9\xc3(\xe2\x9c\x93\xed\xa0\x80\xf0\x9f\x9a\x80\xf5\xc0\xaf\xe2\x9cz\xf0\x9f\x9a";
        let whole = strip(stream);
        assert!(whole.contains('\u{fffd}') && whole.contains('\u{e9}') && whole.contains('\u{2713}'), "{whole:?}");
        for i in 0..=stream.len() {
            for j in i..=stream.len() {
                let (text, held) = fed(&[&stream[..i], &stream[i..j], &stream[j..]]);
                // The stream ends inside a character: nothing reads it yet, but it is kept.
                assert_eq!(held, b"\xf0\x9f\x9a", "cut at {i} and {j}");
                assert_eq!(text + &strip(&held), whole, "cut at {i} and {j}");
            }
        }
        let (text, held) = fed(&stream.iter().map(std::slice::from_ref).collect::<Vec<_>>());
        assert_eq!(text + &strip(&held), whole, "one byte at a time");
    }

    #[test]
    fn what_is_held_never_grows_past_three_bytes() {
        let mut held = Vec::new();
        let chunks: [&[u8]; 7] = [b"\xf0", b"\x9f", b"\x9a", b"\xf0", b"\xf0\x9f\x9a", b"\x80\xc3", b"\xa9\xe2\x9c"];
        for chunk in chunks {
            let _ = text_of(&mut held, chunk);
            assert!(held.len() <= 3, "{held:?}");
        }
        assert_eq!(held, b"\xe2\x9c");
    }
}
