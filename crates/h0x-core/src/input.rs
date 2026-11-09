//! Input from agents: one queue per terminal, so send, keys and interrupt
//! never interleave with each other, and never with a person typing.

use crate::holder_proto as proto;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::terminals::Terminal;
use std::time::{Duration, Instant};

/// Pause between the paste and Enter, so the program sees them apart.
pub const PASTE_GAP: Duration = Duration::from_millis(200);
/// Pause between keys, so ESC then x isn't read as alt+x.
const KEY_GAP: Duration = Duration::from_millis(30);
/// A program whose output stopped this long ago has finished drawing (`ready: quiet`).
pub const OUTPUT_SETTLE: Duration = Duration::from_millis(800);
/// How long after start an agent may still be setting itself up.
const STARTUP_MS: u64 = 10_000;
/// The longest a send waits for a starting agent to settle.
const SETTLE_MAX: Duration = Duration::from_secs(5);

/// A person's typing, as it affects agent input to one terminal.
#[derive(Default)]
pub struct Gate {
    last_human: Option<Instant>,
    /// An agent's input is going in right now: viewer input waits.
    busy: bool,
    held: Vec<u8>,
    held_human: bool,
    draft: usize,
    pasting: bool,
    sequence: Vec<u8>,
    draft_ended: bool,
}

impl Gate {
    /// Submitting input also waits for the draft, bounded since the last human key.
    fn wait(&self, quiet: Duration, submits: bool) -> Duration {
        let Some(at) = self.last_human else { return Duration::ZERO };
        if submits && self.draft_ended {
            return PASTE_GAP.saturating_sub(at.elapsed());
        }
        let hold = if submits && self.draft > 0 { quiet.max(draft_timeout()) } else { quiet };
        hold.saturating_sub(at.elapsed())
    }

    fn key(&mut self, code: u32, modifiers: u32) {
        self.draft_ended = false;
        let modifiers = modifiers.saturating_sub(1) & !0xc0;
        match (code, modifiers + 1) {
            (13 | 3 | 21, 1) | (99 | 117, 5) => {
                self.draft = 0;
                self.draft_ended = true;
            }
            (8 | 127, 1) => self.draft = self.draft.saturating_sub(1),
            (32..=0x10ffff, 1 | 2)
                if char::from_u32(code).is_some_and(|c| !c.is_control()) && !(57344..=63743).contains(&code) =>
            {
                self.draft = self.draft.saturating_add(1);
            }
            _ => {}
        }
    }

    /// Only the common CSI-u press forms; other terminal sequences type no text.
    fn escape(&mut self) {
        let seq = &self.sequence;
        if seq == b"\x1b[200~" {
            self.pasting = true;
        } else if seq.starts_with(b"\x1b[") && seq.ends_with(b"u") {
            let Ok(body) = std::str::from_utf8(&seq[2..seq.len() - 1]) else { return };
            let mut params = body.split(';');
            let code = params.next().and_then(|p| p.split(':').next()?.parse::<u32>().ok());
            let mut mods = params.next().unwrap_or("").split(':');
            let modifiers = mods.next().filter(|s| !s.is_empty()).unwrap_or("1").parse::<u32>().ok();
            let event = mods.next().unwrap_or("1");
            // Releases do not type another character.
            if event == "3" {
                return;
            }
            if let (Some(code), Some(modifiers)) = (code, modifiers) {
                self.key(code, modifiers);
            }
            return;
        }
        self.draft_ended = false;
    }

    fn track(&mut self, bytes: &[u8]) {
        for (index, &b) in bytes.iter().enumerate() {
            if self.pasting {
                if b == 0x1b || !self.sequence.is_empty() {
                    self.sequence.push(b);
                    if self.sequence == b"\x1b[201~" {
                        self.pasting = false;
                        self.sequence.clear();
                    } else if !b"\x1b[201~".starts_with(&self.sequence) {
                        self.draft =
                            self.draft.saturating_add(self.sequence.iter().filter(|b| **b & 0xc0 != 0x80).count());
                        self.sequence.clear();
                    }
                } else if b & 0xc0 != 0x80 {
                    self.draft = self.draft.saturating_add(1);
                }
                self.draft_ended = false;
            } else if self.sequence == b"\x1b" && (b == 0x1b || (index == 0 && !matches!(b, b'[' | b'O'))) {
                self.sequence.clear();
                if b == 0x1b {
                    self.sequence.push(b);
                    self.draft_ended = false;
                } else if b < 0x80 {
                    self.key(b.into(), 1);
                } else if b & 0xc0 != 0x80 {
                    self.draft_ended = false;
                    self.draft = self.draft.saturating_add(1);
                }
            } else if b == 0x1b || !self.sequence.is_empty() {
                self.sequence.push(b);
                let seq = &self.sequence;
                let complete = match seq.as_slice() {
                    [0x1b] | [0x1b, b'[' | b'O'] => false,
                    [0x1b, b'[', b'M', ..] => seq.len() >= 6, // X10 mouse coordinates
                    [0x1b, b'[', ..] => (0x40..=0x7e).contains(&b),
                    _ => true,
                };
                if complete || self.sequence.len() >= 128 {
                    self.escape();
                    self.sequence.clear();
                }
            } else if b < 0x80 {
                self.key(b.into(), 1);
            } else if b & 0xc0 != 0x80 {
                self.draft_ended = false;
                self.draft = self.draft.saturating_add(1);
            }
        }
    }
    /// A person pressed keys while an agent's input was going in.
    pub fn has_held(&self) -> bool {
        self.held_human
    }

    /// Starts or ends an agent's input; ending returns the viewer bytes held meanwhile.
    pub fn set_busy(&mut self, busy: bool) -> Vec<u8> {
        self.busy = busy;
        if busy {
            Vec::new()
        } else {
            self.held_human = false;
            std::mem::take(&mut self.held)
        }
    }

    /// Viewer bytes: straight through, or held while an agent types.
    /// Only human input resets the quiet period.
    pub fn viewer(&mut self, bytes: &[u8], human: bool) -> Vec<u8> {
        if human {
            self.last_human = Some(Instant::now());
            self.track(bytes);
        }
        if self.busy {
            self.held_human |= human && !bytes.is_empty();
            self.held.extend_from_slice(bytes);
            Vec::new()
        } else {
            bytes.to_vec()
        }
    }
}

pub enum Step {
    FirstTask(Vec<u8>),
    Write(Vec<u8>),
    Pause(Duration),
    EndDraft(Vec<u8>),
}

fn env_ms(var: &str, default: u64) -> Duration {
    Duration::from_millis(std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
}

/// How long a person must have stopped typing before an agent's send goes in.
pub fn quiet() -> Duration {
    env_ms("H0X_CORE_QUIET_MS", 3000)
}

pub(crate) fn draft_timeout() -> Duration {
    clamp_draft(env_ms("H0X_CORE_DRAFT_MS", 60_000))
}

/// A draft never holds a message longer than ten minutes, whatever the setting.
fn clamp_draft(timeout: Duration) -> Duration {
    timeout.min(Duration::from_millis(600_000))
}

/// How long `send` watches for the program to start working.
fn started_window() -> Duration {
    env_ms("H0X_CORE_STARTED_MS", 5000)
}

fn working(t: &Terminal) -> bool {
    t.status.as_ref().is_some_and(|s| s.state == "working")
}

fn state_word(t: &Terminal) -> Option<String> {
    t.status.as_ref().map(|s| s.state.clone())
}

/// A paste the way a terminal sends one: bracketed when the program asked.
pub fn paste(bracketed: bool, text: &str) -> Vec<u8> {
    // Until none is left: one pass would let `ESC[20ESC[201~1~` re-form the marker.
    let mut text = text.to_owned();
    while text.contains("\x1b[201~") {
        text = text.replace("\x1b[201~", "");
    }
    if bracketed { format!("\x1b[200~{text}\x1b[201~").into_bytes() } else { text.into_bytes() }
}

pub struct Sent {
    pub status: Option<String>,
    pub turn: u64,
    /// Whether it went `working` soon after; None when it never reports status.
    pub started: Option<bool>,
}

pub enum Interrupted {
    Stopped,
    Exited,
    StillWorking,
    Unknown,
}

impl Core {
    /// Types `steps` (built by `prepare` from the live state) into terminal `id`.
    /// `quiet`: how long a person must have stopped typing first.
    /// `submits`: also wait for their unfinished draft, releasing immediately on send/clear.
    pub async fn type_in<T>(
        &self,
        id: u32,
        quiet: Duration,
        submits: bool,
        prepare: impl FnOnce(&State, &Terminal) -> Result<(T, Vec<Step>), RpcError>,
    ) -> Result<T, RpcError> {
        let queue = self.lock().queues.entry(id).or_default().clone();
        let sends = self.lock().sends.entry(id).or_default().clone();
        let _send_turn = if submits { Some(sends.lock().await) } else { None };
        let (out, steps, _turn) = 'waiting: loop {
            if submits {
                loop {
                    crate::server::check_cancelled()?;
                    let left = self.lock().gates.get(&id).map_or(Duration::ZERO, |g| g.wait(quiet, true));
                    if left.is_zero() || !self.lock().reg.terminals.get(&id).is_some_and(|t| t.is_live()) {
                        break;
                    }
                    tokio::time::sleep(left.min(Duration::from_millis(25))).await;
                }
            }
            let turn = queue.lock().await;
            loop {
                if submits {
                    crate::server::check_cancelled()?;
                }
                let left = {
                    let mut st = self.lock();
                    let t = st.reg.terminals.get(&id).filter(|t| t.is_live()).ok_or_else(|| {
                        RpcError::new(
                            "finished",
                            format!("{} has finished", crate::terminals::address(id)),
                            format!("h0x-core read {}", crate::terminals::address(id)),
                        )
                    })?;
                    t.check_program()?;
                    let left = st.gates.get(&id).map_or(Duration::ZERO, |g| g.wait(quiet, submits));
                    if left.is_zero() {
                        let prepared = prepare(&st, t)?;
                        let gate = st.gates.entry(id).or_default();
                        gate.busy = true;
                        let ends_draft = prepared.1.iter().any(|step| matches!(step, Step::EndDraft(_)));
                        if submits || ends_draft {
                            gate.draft = 0;
                        }
                        if ends_draft {
                            gate.draft_ended = true;
                        }
                        break 'waiting (prepared.0, prepared.1, turn);
                    }
                    left
                };
                if submits {
                    drop(turn);
                    continue 'waiting;
                }
                tokio::time::sleep(left.min(Duration::from_millis(25))).await;
            }
        };
        for step in steps {
            match step {
                Step::FirstTask(bytes) => {
                    let mut st = self.lock();
                    if st.link(id).is_some() {
                        if let Some(t) = st.reg.terminals.get_mut(&id) {
                            t.first_task_turn = Some(t.turn);
                        }
                        st.link(id).expect("delivery link").input(id, &bytes);
                        self.save(&st);
                        self.changed.notify_waiters();
                    }
                }
                Step::Write(bytes) | Step::EndDraft(bytes) => {
                    if let Some(link) = self.lock().link(id) {
                        link.input(id, &bytes);
                    }
                }
                Step::Pause(d) => tokio::time::sleep(d).await,
            }
        }
        let mut st = self.lock();
        let gate = st.gates.entry(id).or_default();
        let held = gate.set_busy(false);
        if let Some(link) = st.link(id) {
            link.input(id, &held);
        }
        Ok(out)
    }

    /// Paste + Enter, after a person has paused typing. Refuses a blocked
    /// program (typing would answer its dialog) unless `force`.
    pub async fn send(&self, id: u32, text: &str, force: bool, only_if: Option<&str>) -> Result<Sent, RpcError> {
        self.send_message(id, text, force, only_if, false).await
    }

    pub(crate) async fn send_first_task(&self, id: u32, text: &str) -> Result<Sent, RpcError> {
        self.send_message(id, text, false, None, true).await
    }

    async fn send_message(
        &self,
        id: u32,
        text: &str,
        force: bool,
        only_if: Option<&str>,
        first_task: bool,
    ) -> Result<Sent, RpcError> {
        if let Some(t) = self.lock().reg.terminals.get(&id) {
            t.check_program()?;
        }
        self.settle(id).await?;
        let (status, turn, reported) = self
            .type_in(id, quiet(), true, |st, t| {
                let word = state_word(t);
                if word.as_deref() == Some("blocked") && !force {
                    return Err(RpcError::new(
                        "blocked",
                        format!(
                            "{} is blocked ({})",
                            crate::terminals::address(id),
                            t.status.as_ref().and_then(|s| s.kind.as_deref()).unwrap_or("waiting")
                        ),
                        format!(
                            "typing would answer its dialog; read its screen first (h0x-core read {}), or --force",
                            crate::terminals::address(id)
                        ),
                    ));
                }
                if let Some(want) = only_if
                    && word.as_deref() != Some(want)
                {
                    return Err(RpcError::new(
                        "status_mismatch",
                        format!(
                            "{} is {}, not {want}",
                            crate::terminals::address(id),
                            word.as_deref().unwrap_or("silent")
                        ),
                        "nothing was typed",
                    ));
                }
                let screen = st.screens.get(&id).expect("live terminal has a screen");
                // A template may say how its agent submits (default: Enter after 200 ms).
                let submit = match &t.submit {
                    Some(key) => crate::keys::encode(screen, key)?,
                    None => b"\r".to_vec(),
                };
                let gap = t.paste_gap_ms.map_or(PASTE_GAP, Duration::from_millis);
                let bytes = paste(screen.dec_mode(2004), text);
                let paste = if first_task { Step::FirstTask(bytes) } else { Step::Write(bytes) };
                let steps = vec![paste, Step::Pause(gap), Step::Write(submit)];
                Ok(((word, t.turn, t.reported), steps))
            })
            .await?;
        let started = if reported {
            let went =
                |st: &State| st.reg.terminals.get(&id).is_none_or(|t| t.turn > turn || working(t) || !t.is_live());
            self.wait_for(started_window(), went).await;
            let st = self.lock();
            Some(st.reg.terminals.get(&id).is_some_and(|t| t.turn > turn || working(t)))
        } else {
            None
        };
        Ok(Sent { status, turn, started })
    }

    /// A starting agent can report before it is ready for a paste (Pi swaps its
    /// input box right after its first report and drops a paste in flight):
    /// in its first seconds, wait until its screen has been still a moment.
    async fn settle(&self, id: u32) -> Result<(), RpcError> {
        let deadline = Instant::now() + SETTLE_MAX;
        loop {
            let ready = {
                let st = self.lock();
                let age = st
                    .reg
                    .terminals
                    .get(&id)
                    .map(|t| Duration::from_millis(crate::runtime::now_ms().saturating_sub(t.created_ms)));
                age.is_none_or(|age| {
                    age >= Duration::from_millis(STARTUP_MS)
                        || st.last_output.get(&id).map_or(age, Instant::elapsed) >= env_ms("H0X_CORE_SETTLE_MS", 800)
                })
            };
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(RpcError::new(
                    "unavailable",
                    format!("{} is still starting: its output has not settled", crate::terminals::address(id)),
                    format!("nothing was typed; check h0x-core read {}, then retry", crate::terminals::address(id)),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Named keys, after a person has paused typing.
    pub async fn keys(&self, id: u32, names: &[String]) -> Result<(Option<String>, u64), RpcError> {
        self.type_in(id, quiet(), false, |st, t| {
            let screen = st.screens.get(&id).expect("live terminal has a screen");
            let mut steps = Vec::new();
            for name in names {
                let bytes = crate::keys::encode(screen, name)?;
                let mut key = Gate::default();
                key.track(&bytes);
                steps.push(if key.draft_ended { Step::EndDraft(bytes) } else { Step::Write(bytes) });
                steps.push(Step::Pause(KEY_GAP));
            }
            Ok(((state_word(t), t.turn), steps))
        })
        .await
    }

    /// Esc for programs that report status, else ctrl+c; then watch what happens.
    pub async fn interrupt(&self, id: u32, timeout: Duration) -> Result<(Interrupted, String), RpcError> {
        let (reported, pid) = {
            let st = self.lock();
            let t = st.reg.terminals.get(&id).ok_or_else(|| RpcError::new("not_found", "terminal removed", ""))?;
            t.check_program()?;
            (t.reported, t.pid.unwrap_or(0) as i32)
        };
        let before = if reported { -1 } else { self.foreground(id).await };
        let key = self
            .type_in(id, Duration::ZERO, false, |st, t| {
                let default = if reported { "esc" } else { "ctrl+c" };
                let key = t.stop.clone().unwrap_or_else(|| default.into());
                let screen = st.screens.get(&id).expect("live terminal has a screen");
                let bytes = crate::keys::encode(screen, &key)?;
                Ok((key, vec![Step::Write(bytes)]))
            })
            .await?;
        let live = |st: &State| st.reg.terminals.get(&id).is_some_and(|t| t.is_live());
        if reported {
            let settled = |st: &State| st.reg.terminals.get(&id).is_none_or(|t| !t.is_live() || !working(t));
            self.wait_for(timeout, settled).await;
            let st = self.lock();
            let outcome = match st.reg.terminals.get(&id) {
                Some(t) if !t.is_live() => Interrupted::Exited,
                Some(t) if working(t) => Interrupted::StillWorking,
                _ => Interrupted::Stopped,
            };
            return Ok((outcome, key));
        }
        // No status reports: a shell that gets the terminal back has stopped its job.
        let deadline = Instant::now() + timeout;
        loop {
            if !live(&self.lock()) {
                return Ok((Interrupted::Exited, key));
            }
            if before != pid && self.foreground(id).await == pid {
                return Ok((Interrupted::Stopped, key));
            }
            if Instant::now() >= deadline {
                return Ok((Interrupted::Unknown, key));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The process group in the foreground of terminal `id`, or -1.
    pub(crate) async fn foreground(&self, id: u32) -> i32 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut st = self.lock();
            st.foreground.entry(id).or_default().push(tx);
            let Some(link) = st.link(id) else { return -1 };
            link.send(&proto::frame(proto::FOREGROUND, id, &[]));
        }
        tokio::time::timeout(Duration::from_secs(1), rx).await.ok().and_then(Result::ok).unwrap_or(-1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_replies_preserve_the_human_quiet_gate() {
        let mut gate = Gate::default();
        assert_eq!(gate.viewer(b"reply", false), b"reply");
        assert!(gate.last_human.is_none());
        gate.viewer(b"x", true);
        let human = gate.last_human;
        gate.set_busy(true);
        assert!(gate.viewer(b"automatic", false).is_empty());
        assert!(!gate.has_held(), "automatic replies are not a person interrupting a scroll");
        assert_eq!(gate.last_human, human);
        gate.viewer(b"typed", true);
        assert!(gate.has_held());
        assert_eq!(gate.set_busy(false), b"automatictyped");
        assert!(!gate.has_held());
    }

    #[test]
    fn a_held_note_waits_for_the_draft_then_goes_in_order_after_it_is_sent() {
        let mut g = Gate::default();
        let quiet = Duration::from_secs(3);
        // No person has typed: nothing to wait for.
        assert!(g.wait(quiet, true).is_zero());
        // Mid-typing: every note is held, for the draft timeout at most.
        g.viewer(b"half a sentence", true);
        assert!(g.wait(quiet, true) > quiet, "a held note waits longer than the quiet gap");
        assert!(g.wait(quiet, true) <= draft_timeout());
        // The draft is the same for the second note: still held, not interleaved.
        assert!(!g.wait(Duration::ZERO, true).is_zero());
        // Enter submits the draft; a CRLF pair empties it too (the LF types nothing).
        let mut crlf = Gate::default();
        crlf.viewer(b"abc\r\n", true);
        assert_eq!(crlf.draft, 0);
        g.viewer(b"\r", true);
        assert_eq!(g.draft, 0);
        assert!(g.draft_ended);
        assert!(g.wait(quiet, true) <= PASTE_GAP, "after a send the notes follow within the paste gap");
        // Typing again starts a new draft and holds the next note again.
        g.viewer(b"b", true);
        assert_eq!(g.draft, 1);
        assert!(!g.draft_ended);
        assert!(g.wait(quiet, true) > quiet);
    }

    #[test]
    fn a_pasted_draft_counts_characters_and_a_pasted_enter_never_submits_it() {
        let draft = |chunks: &[&[u8]]| {
            let mut g = Gate::default();
            for chunk in chunks {
                g.viewer(chunk, true);
            }
            (g.draft, g.draft_ended)
        };
        assert_eq!(draft(&[b"\x1b[200~\x1b[201~"]), (0, false), "an empty paste leaves no draft");
        assert_eq!(draft(&[b"\x1b[200~\xc3\xa9\xe2\x82\xac\x1b[201~"]).0, 2, "characters, not bytes");
        assert_eq!(draft(&[b"\x1b[200~line\r\nline\x1b[201~"]).0, 10, "pasted CR and LF count as text");
        assert_eq!(draft(&[b"\x1b[200~a\r", b"b\x1b[201~"]).0, 3, "a paste split across writes");
        assert_eq!(draft(&[b"\x1b[200~a\x1b[201~\r"]), (0, true), "Enter after the paste submits it");
        // An unterminated paste keeps counting: the draft is not dropped.
        assert_eq!(draft(&[b"\x1b[200~abc"]).0, 3);
        // A stray marker inside a paste does not end it early.
        assert!(draft(&[b"\x1b[200~a\x1b[201xb\x1b[201~\r"]).1);
    }

    #[test]
    fn keys_an_agent_sends_never_make_a_draft_and_typed_input_overrides_an_old_end() {
        let mut g = Gate::default();
        g.viewer(b"\r", false);
        g.viewer(b"abc", false);
        assert_eq!((g.draft, g.draft_ended), (0, false), "automatic input is not a person's draft");
        assert!(g.wait(Duration::from_secs(3), true).is_zero());
        g.viewer(b"x", true);
        g.viewer(b"\x08", true);
        assert_eq!(g.draft, 0, "deleting the last character empties the draft");
        g.viewer(b"\x08\x08", true);
        assert_eq!(g.draft, 0, "backspace on an empty draft stays at zero");
    }

    #[test]
    fn a_draft_lasts_until_sent_or_cleared_in_legacy_and_kitty_modes() {
        let draft = |chunks: &[&[u8]]| {
            let mut g = Gate::default();
            for chunk in chunks {
                g.viewer(chunk, true);
            }
            g.draft
        };
        assert_eq!(draft(&[b"hey", "é".as_bytes()]), 4);
        for clear in [b"\r".as_slice(), b"\x03", b"\x15", b"\x1b[13u", b"\x1b[99;5u", b"\x1b[117;5:1u"] {
            assert_eq!(draft(&[b"hey", clear]), 0, "{clear:?}");
        }
        assert_eq!(draft(&[b"hi\x7f\x08\x7f"]), 0);
        assert_eq!(draft(&[b"\x1b[97u\x1b[98;;98u\x1b[127u"]), 1);
        assert_eq!(draft(&[b"\x1b[97;1:1u\x1b[97;1:2u\x1b[97;1:3u"]), 2);
        assert_eq!(draft(&[b"\x1b[97;129u\x1b[97;129:2u\x7f"]), 1);
        assert_eq!(draft(&[b"hey\x1b[99;133u"]), 0);
        assert_eq!(draft(&[b"hey\x1b", b"\r"]), 0);
        assert_eq!(draft(&[b"hey\x1b\r"]), 3, "legacy shift+enter does not submit");
        assert_eq!(draft(&[b"\x1b", b"\x1b[200~a\r\x1b[201~"]), 2);
        assert_eq!(draft(&[b"hey\x1b[13;1:3u\x1b[99;5:3u"]), 3);
        assert_eq!(draft(&[b"\x1b[A\x1bOB\x1bb\x1b[120;3u\x1b[13;2u\x1b[I\x1b[<0;1;1M"]), 0);
        assert_eq!(draft(&[b"\x1b[M !!"]), 0, "legacy mouse report");
        assert_eq!(draft(&[b"\x1b[200~a\rb\x1b[201~"]), 3, "pasted Enter does not submit");
        assert_eq!(draft(&[b"\x1b[20", b"0~ab\x1b[20", b"1~\x1b[13", b"u"]), 0, "fragmented sequences");
        let mut g = Gate::default();
        g.viewer(b"automatic", false);
        assert_eq!(g.draft, 0);
        g.viewer(b"x", true);
        g.viewer(b"\r", false);
        assert_eq!(g.draft, 1);
        assert!(!g.wait(Duration::ZERO, true).is_zero());
        g.viewer(b"\x1b[99;5u", true);
        assert!(!g.wait(Duration::from_secs(3), true).is_zero());
        g.last_human = Some(Instant::now() - PASTE_GAP);
        assert!(g.wait(Duration::from_secs(3), true).is_zero());
        assert!(!g.wait(Duration::from_secs(3), false).is_zero(), "keys keep their quiet wait");
        g.viewer(b"\x1b[A", true);
        assert!(!g.wait(Duration::from_secs(3), true).is_zero(), "other keys keep the quiet wait");
        g.viewer(b"x", true);
        g.last_human = Some(Instant::now() - Duration::from_secs(61));
        assert!(g.wait(Duration::from_secs(3), true).is_zero(), "a draft expires after the last key");
    }

    #[test]
    fn editing_keys_keep_the_draft_and_the_note_held() {
        let quiet = Duration::from_secs(3);
        for edit in [
            b"\x1b[D".as_slice(),
            b"\x1b[C",
            b"\x1bOD",
            b"\x1b[H",
            b"\x1b[F",
            b"\x1bOH",
            b"\x1b[1~",
            b"\x1b[4~",
            b"\x1b[3~",
            b"\x1b[1;5D",
            b"\x17", // ctrl+w
            b"\x0b", // ctrl+k
            b"\x01",
            b"\x05",
            b"\x1bb",
            b"\x1bf",
            b"\x1b\x7f",
            b"\x1b[100;3u",
            b"\x1b[107;5u", // kitty ctrl+k
        ] {
            let mut g = Gate::default();
            g.viewer(b"hello", true);
            g.viewer(edit, true);
            assert_eq!((g.draft, g.draft_ended), (5, false), "{edit:?}");
            assert!(g.wait(quiet, true) > quiet, "{edit:?}: the note is still held");
            assert!(g.wait(quiet, true) <= draft_timeout().max(quiet), "{edit:?}: and only up to the draft timeout");
        }
    }

    #[test]
    fn a_lone_esc_then_a_key_in_its_own_write_still_types_a_draft() {
        // A separate write is a separate key press: esc, then a.
        let mut g = Gate::default();
        g.viewer(b"\x1b", true);
        g.viewer(b"a", true);
        assert_eq!(g.draft, 1, "the a after a lone esc is part of the draft");
        assert!(g.wait(Duration::from_secs(3), true) > Duration::from_secs(3));
        // In one write it is alt+a, which types nothing.
        let mut alt = Gate::default();
        alt.viewer(b"\x1ba", true);
        assert_eq!(alt.draft, 0);
    }

    #[test]
    fn ime_and_unicode_input_count_characters_and_malformed_keys_count_nothing() {
        let draft = |chunks: &[&[u8]]| {
            let mut g = Gate::default();
            for chunk in chunks {
                g.viewer(chunk, true);
            }
            (g.draft, g.draft_ended)
        };
        assert_eq!(draft(&["日本語".as_bytes()]), (3, false), "an IME commit");
        assert_eq!(draft(&["👍".as_bytes()]).0, 1);
        assert_eq!(draft(&["e\u{301}".as_bytes()]).0, 2, "a combining mark counts on its own");
        let ni = "日".as_bytes();
        assert_eq!(draft(&[&ni[..1], &ni[1..]]).0, 1, "a character split across writes");
        assert_eq!(draft(&[b"\x1b[26085u"]).0, 1, "a kitty key with a non-ASCII code point");
        for junk in [
            b"\x1b[55296u".as_slice(), // a surrogate
            b"\x1b[1114112u",          // beyond Unicode
            b"\x1b[57399u",            // kitty functional key (private use)
            b"\x1b[99999999999999999999u",
            b"\x1b[u",
            b"\x1b[;u",
            b"\x1b[=;?u",
            b"\x1b[97;=u",
            b"\x1b[\xffu",
        ] {
            assert_eq!(draft(&[b"hey", junk]), (3, false), "{junk:?}");
        }
    }

    #[test]
    fn windows_enter_forms_submit_or_fall_back_to_the_quiet_wait() {
        let quiet = Duration::from_secs(3);
        // CR-only Enter, as ConPTY and Windows terminals send it.
        let mut cr = Gate::default();
        cr.viewer(br"cd C:\Users\me", true);
        assert_eq!(cr.draft, 14, "backslashes in a Windows path are typed text");
        cr.viewer(b"\r", true);
        assert_eq!((cr.draft, cr.draft_ended), (0, true));
        assert!(cr.wait(quiet, true) <= PASTE_GAP);
        // CRLF, in one write or split: the LF clears draft_ended, so the quiet wait
        // applies, never the longer draft timeout.
        for chunks in [&[b"abc\r\n".as_slice()][..], &[b"abc\r", b"\n"]] {
            let mut g = Gate::default();
            for chunk in chunks {
                g.viewer(chunk, true);
            }
            assert_eq!((g.draft, g.draft_ended), (0, false), "{chunks:?}");
            let left = g.wait(quiet, true);
            assert!(left <= quiet && left > PASTE_GAP, "{chunks:?}: {left:?}");
        }
        // win32-input-mode key records type no text and do not submit: the draft
        // stays held, bounded by the draft timeout.
        let mut g = Gate::default();
        g.viewer(b"hey", true);
        g.viewer(b"\x1b[65;30;97;1;0;1_\x1b[13;28;13;1;0;1_", true);
        assert_eq!((g.draft, g.draft_ended), (3, false));
        assert!(g.wait(quiet, true) <= draft_timeout().max(quiet));
    }

    #[test]
    fn empty_input_huge_waits_and_long_sequences_never_panic_or_grow() {
        let quiet = Duration::from_secs(3);
        // A fresh gate (also what a restarted core has) holds nothing.
        let mut g = Gate::default();
        assert!(g.wait(Duration::MAX, true).is_zero());
        g.viewer(b"", true);
        assert_eq!((g.draft, g.draft_ended), (0, false), "an empty write types nothing");
        assert!(g.wait(quiet, true) <= quiet);
        g.viewer(b"ab", true);
        g.viewer(b"", true);
        assert_eq!(g.draft, 2, "an empty write keeps the draft");
        // The largest quiet wins over the draft timeout without overflowing.
        assert!(g.wait(Duration::MAX, true) > draft_timeout());
        assert!(g.wait(Duration::MAX, false) > draft_timeout());
        // The draft count saturates.
        g.draft = usize::MAX;
        g.viewer(b"a", true);
        assert_eq!(g.draft, usize::MAX);
        // An unterminated sequence is cut off at 128 bytes.
        let mut long = b"\x1b[".to_vec();
        long.extend([b'1'; 300]);
        let mut g = Gate::default();
        g.viewer(&long, true);
        assert!(g.sequence.len() < 128, "{}", g.sequence.len());
    }

    #[test]
    fn timeout_settings_fall_back_on_bad_values_and_never_panic() {
        // Only this test reads this variable, so setting it races nothing.
        let var = "H0X_CORE_TEST_DRAFT_MS_PARSE";
        let parse = |value: Option<&str>| {
            match value {
                Some(v) => unsafe { std::env::set_var(var, v) },
                None => unsafe { std::env::remove_var(var) },
            }
            env_ms(var, 60_000)
        };
        let default = Duration::from_secs(60);
        assert_eq!(parse(None), default, "unset");
        assert_eq!(parse(Some("0")), Duration::ZERO, "zero: only the quiet wait is left");
        assert_eq!(parse(Some("1500")), Duration::from_millis(1500));
        assert_eq!(parse(Some("+1500")), Duration::from_millis(1500));
        for bad in ["", "-1", "-60000", "abc", "1.5", "1e3", " 1500", "1500 ", "\t1500\n", "18446744073709551616"] {
            assert_eq!(parse(Some(bad)), default, "{bad:?}");
        }
        assert_eq!(parse(Some("18446744073709551615")), Duration::from_millis(u64::MAX), "the largest is taken as is");
        let ten_minutes = Duration::from_millis(600_000);
        assert_eq!(clamp_draft(parse(Some("18446744073709551615"))), ten_minutes, "clamped, not rejected");
        assert_eq!(clamp_draft(parse(Some("600000"))), ten_minutes);
        assert_eq!(clamp_draft(parse(Some("600001"))), ten_minutes);
        assert_eq!(clamp_draft(parse(Some("1500"))), Duration::from_millis(1500), "smaller values are kept");
        parse(None);
    }

    #[test]
    fn pastes_are_bracketed_only_when_asked_and_cannot_end_early() {
        assert_eq!(paste(true, "hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste(false, "hi"), b"hi");
        assert_eq!(paste(true, "a\x1b[201~b"), b"\x1b[200~ab\x1b[201~");
    }

    #[test]
    fn a_split_end_marker_cannot_re_form_and_end_the_paste_early() {
        assert_eq!(paste(true, "a\x1b[20\x1b[201~1~b"), b"\x1b[200~ab\x1b[201~");
        assert_eq!(paste(true, "a\x1b[20\x1b[20\x1b[201~1~1~b"), b"\x1b[200~ab\x1b[201~");
        assert_eq!(paste(false, "a\x1b[20\x1b[201~1~b"), b"ab");
        assert_eq!(paste(true, ""), b"\x1b[200~\x1b[201~");
        assert_eq!(paste(true, "a\x1bb"), b"\x1b[200~a\x1bb\x1b[201~");
    }
}
