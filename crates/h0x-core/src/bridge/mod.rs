//! Remote control: chat accounts paired with this machine drive its terminals.
//! Transport-agnostic: a transport (Telegram, a relay) turns messages into a
//! [`Command`] from a [`Sender`] and shows the [`Reply`]; this module decides
//! who may do what, and remembers it. See docs/bridge.md for the threat model.
//!
//! Everything lives in `<config dir>/h0x-core/bridge/` (a private folder):
//! `state.json` (paired accounts, the pending code, the lock; 0600) and
//! `audit.jsonl` (append-only, 0600). Limits are `[bridge]` in config.toml.

mod command;
mod notice;
mod redact;
mod secret;
pub mod telegram;
pub mod web;

pub use command::{Command, Key, Reply, Sender};
pub use notice::{Kind, Notice, Notices, watch};
pub use redact::redact;

use crate::config::BridgeConfig;
use crate::rpc::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;

/// Who can pair today.
pub const TRANSPORTS: &[&str] = &["telegram", "web"];
/// How long a pairing code lives: a web link (shown as a QR code) 5 minutes, a chat code 10.
fn code_ms(transport: &str) -> u64 {
    if transport == "web" { 5 * 60_000 } else { 10 * 60_000 }
}
const WRONG_CODES: u32 = 5;
const BLOCK_MS: u64 = 10 * 60_000;
/// audit.jsonl moves to audit.jsonl.1 (replacing the older one) once it holds this many bytes.
const AUDIT_MAX: u64 = 1 << 20;
/// No 0/O or 1/I: read off a screen and typed on a phone. 32 letters: no modulo bias.
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A chat account allowed to control this machine.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Identity {
    pub id: u32,
    pub transport: String,
    pub user_id: String,
    pub label: String,
    pub paired_at_ms: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    #[serde(default)]
    identities: Vec<Identity>,
    #[serde(default)]
    last_id: u32,
    #[serde(default)]
    pending: Option<Pending>,
    /// Wrong codes in a row.
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    blocked_until_ms: u64,
    #[serde(default)]
    locked: bool,
    /// Times locked: a confirmation given before a lock is void after it,
    /// whichever process locked (ADR-061 addendum).
    #[serde(default)]
    locks: u64,
    /// Web devices: their user id and the SHA-256 of their session token.
    #[serde(default)]
    sessions: BTreeMap<String, String>,
    /// Web devices: their user id and the device public key (base64url) they paired with;
    /// a session is only good from that key (ADR-064). Older sessions have none and are refused.
    #[serde(default)]
    devices: BTreeMap<String, String>,
    /// Web devices: their user id and when the session was made (ms). A session
    /// without one (from before sessions expired) counts as made when first seen.
    #[serde(default)]
    session_times: BTreeMap<String, u64>,
}

/// The one live pairing code.
#[derive(Serialize, Deserialize)]
struct Pending {
    code: String,
    transport: String,
    expires_ms: u64,
}

pub struct Bridge {
    dir: PathBuf,
    pub config: BridgeConfig,
    /// Command times in the last minute, per sender.
    recent: HashMap<String, VecDeque<u64>>,
    /// Destructive commands waiting for `confirm`, by token.
    tokens: HashMap<String, command::Waiting>,
}

fn failed(e: impl std::fmt::Display) -> RpcError {
    RpcError::new("bridge", e.to_string(), "see `h0x-core bridge --help`")
}

impl Bridge {
    /// This user's bridge, with the limits from config.toml.
    pub fn open(paths: &crate::paths::Paths) -> Result<Bridge, RpcError> {
        let config = crate::config::load(&paths.config)?.bridge;
        Ok(Bridge::at(paths.config.parent().expect("config has a folder").join("bridge"), config))
    }

    pub fn at(dir: PathBuf, config: BridgeConfig) -> Bridge {
        Bridge { dir, config, recent: HashMap::new(), tokens: HashMap::new() }
    }

    /// The state, or empty when there is none. A file that isn't a JSON
    /// object of the right shape is an error, left untouched for `reset`.
    fn load(&self) -> Result<State, RpcError> {
        let path = self.dir.join("state.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(failed(format!("{}: {e}", path.display()))),
        };
        let parsed = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => serde_json::from_value(value).map_err(|e| e.to_string()),
            Ok(_) => Err("not a JSON object".into()),
            Err(e) => Err(e.to_string()),
        };
        parsed.map_err(|e| {
            RpcError::new(
                "bridge",
                format!("the bridge state is corrupt ({e}); run `h0x-core bridge reset` on that machine"),
                "reset moves it aside and starts with no paired accounts",
            )
        })
    }

    /// Moves state.json aside as `state.json.bad-<now>` and starts empty: no
    /// paired accounts, no code, unlocked. Local only. Where it went, if anywhere.
    pub fn reset(&self, now: u64) -> Result<Option<PathBuf>, RpcError> {
        let (path, aside) = (self.dir.join("state.json"), self.dir.join(format!("state.json.bad-{now}")));
        match std::fs::rename(&path, &aside) {
            Ok(()) => Ok(Some(aside)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failed(format!("{}: {e}", path.display()))),
        }
    }

    /// Replaces state.json with a new 0600 file.
    fn save(&self, st: &State) -> Result<(), RpcError> {
        self.write_private("state.json", &serde_json::to_vec_pretty(st).map_err(failed)?)
    }

    /// Replaces `name` in the bridge folder with a new 0600 file holding `bytes`.
    fn write_private(&self, name: &str, bytes: &[u8]) -> Result<(), RpcError> {
        let write = || -> std::io::Result<()> {
            crate::platform::private_dir(&self.dir)?;
            let tmp = self.dir.join(format!("{name}.tmp"));
            let _ = std::fs::remove_file(&tmp);
            let mut file = crate::platform::private_file(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(tmp, self.dir.join(name))
        };
        write().map_err(failed)
    }

    /// The secret `name` from the OS secret store (a legacy plaintext file is moved in).
    pub fn secret_get(&self, name: &str) -> std::io::Result<Option<Vec<u8>>> {
        secret::get(self, name)
    }

    /// Stores (replaces) the secret `name` in the OS secret store.
    pub fn secret_put(&self, name: &str, value: &[u8]) -> std::io::Result<()> {
        secret::put(self, name, value)
    }

    /// True when this process kept a secret in an owner-only file because the OS
    /// secret store was unusable.
    pub fn secrets_in_file() -> bool {
        secret::fell_back()
    }

    /// Removes the secret `name` (even a damaged one); whether there was one.
    pub fn secret_del(&self, name: &str) -> std::io::Result<bool> {
        secret::del(self, name)
    }

    /// A new single-use code for `transport`, valid 10 minutes; replaces any earlier one.
    pub fn pair(&self, transport: &str, now: u64) -> Result<(String, u64), RpcError> {
        if !TRANSPORTS.contains(&transport) {
            return Err(failed(format!("unknown transport {transport:?}; one of: {}", TRANSPORTS.join(", "))));
        }
        let mut st = self.load()?;
        if st.locked {
            return Err(RpcError::new("bridge", "remote control is locked", "run `h0x-core bridge unlock` first"));
        }
        if transport != "web" {
            blocked(&st, now)?;
        }
        let code = code();
        let expires_ms = now + code_ms(transport);
        st.pending = Some(Pending { code: code.clone(), transport: transport.into(), expires_ms });
        self.save(&st)?;
        Ok((code, expires_ms))
    }

    /// Pairs `user_id` on `transport` if `code` is the live one. Five wrong
    /// chat codes in a row disable chat pairing for 10 minutes. Wrong web codes
    /// count against the connection's source in the panel instead, so a web
    /// stranger can't block Telegram pairing (nor the other way round).
    pub fn pair_verify(
        &self,
        transport: &str,
        user_id: &str,
        label: &str,
        code: &str,
        now: u64,
    ) -> Result<Identity, RpcError> {
        let mut st = self.load()?;
        let result = verify(&mut st, transport, user_id, label, code, now);
        let outcome = match &result {
            Ok(_) => "paired".to_string(),
            Err(e) => format!("refused: {}", e.message),
        };
        // Logged before it counts: an attempt the log can't keep pairs no one.
        self.audit(
            json!({"time_ms": now, "identity": format!("{transport}:{user_id}"), "command": "pair", "result": outcome}),
        )?;
        self.save(&st)?;
        result
    }

    pub fn identities(&self) -> Result<Vec<Identity>, RpcError> {
        Ok(self.load()?.identities)
    }

    /// Removes one paired account, or all of them (`None`): how many went.
    pub fn unpair(&self, id: Option<u32>) -> Result<usize, RpcError> {
        let mut st = self.load()?;
        let before = st.identities.len();
        st.identities.retain(|i| id.is_some_and(|id| i.id != id));
        // An unpaired device's session goes with it.
        let ids = &st.identities;
        st.sessions.retain(|user, _| ids.iter().any(|i| i.transport == "web" && &i.user_id == user));
        st.session_times.retain(|user, _| ids.iter().any(|i| i.transport == "web" && &i.user_id == user));
        st.devices.retain(|user, _| ids.iter().any(|i| i.transport == "web" && &i.user_id == user));
        let removed = before - st.identities.len();
        if let (Some(id), 0) = (id, removed) {
            return Err(RpcError::new("not_found", format!("no paired account {id}"), "see `h0x-core bridge ls`"));
        }
        self.save(&st)?;
        Ok(removed)
    }

    pub fn locked(&self) -> Result<bool, RpcError> {
        Ok(self.load()?.locked)
    }

    /// Lock refuses every remote command and pairing; only `bridge unlock`
    /// here lifts it (no [`Command`] unlocks).
    pub fn set_locked(&self, locked: bool) -> Result<(), RpcError> {
        let mut st = self.load()?;
        st.locked = locked;
        if locked {
            st.pending = None;
            st.locks += 1;
        }
        self.save(&st)
    }

    /// The last `n` audit entries, oldest first.
    pub fn log(&self, n: usize) -> Result<Vec<Value>, RpcError> {
        let text = match std::fs::read_to_string(self.dir.join("audit.jsonl")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(failed(e)),
        };
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        Ok(lines[lines.len().saturating_sub(n)..].iter().map(|l| serde_json::from_str(l).unwrap_or_default()).collect())
    }

    /// The audit log, opened for one more line (created 0600).
    fn audit_file(&self) -> std::io::Result<std::fs::File> {
        crate::platform::private_dir(&self.dir)?;
        let path = self.dir.join("audit.jsonl");
        match crate::platform::private_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::OpenOptions::new().append(true).open(&path)
            }
            opened => opened,
        }
    }

    /// Moves a full audit log aside as audit.jsonl.1, so it cannot grow without end.
    fn rotate_audit(&self, max: u64) -> std::io::Result<()> {
        let path = self.dir.join("audit.jsonl");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() >= max) {
            let old = self.dir.join("audit.jsonl.1");
            match std::fs::remove_file(&old) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            std::fs::rename(&path, &old)?;
        }
        Ok(())
    }

    fn audit(&self, entry: Value) -> Result<(), RpcError> {
        self.rotate_audit(AUDIT_MAX).map_err(failed)?;
        let mut file = self.audit_file().map_err(failed)?;
        file.write_all(format!("{entry}\n").as_bytes()).map_err(failed)
    }
}

/// 8 characters from the OS's CSPRNG: pairing codes and confirmation tokens (40 bits).
fn code() -> String {
    let mut bytes = [0u8; 8];
    crate::platform::random_bytes(&mut bytes);
    bytes.iter().map(|b| ALPHABET[usize::from(b % 32)] as char).collect()
}

fn blocked(st: &State, now: u64) -> Result<(), RpcError> {
    if now < st.blocked_until_ms {
        let minutes = (st.blocked_until_ms - now).div_ceil(60_000);
        return Err(RpcError::new(
            "bridge",
            format!("pairing is disabled for {minutes} more minute(s) after {WRONG_CODES} wrong codes"),
            "wait, then run `h0x-core bridge pair` again",
        ));
    }
    Ok(())
}

/// Same answer in the same time, wherever the first difference is.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn verify(
    st: &mut State,
    transport: &str,
    user_id: &str,
    label: &str,
    code: &str,
    now: u64,
) -> Result<Identity, RpcError> {
    if st.locked {
        return Err(RpcError::new("bridge", "remote control is locked", "unlock it on that machine"));
    }
    if user_id.is_empty() {
        return Err(failed("no user id"));
    }
    if transport != "web" {
        blocked(st, now)?;
    }
    // Typed on a phone: any case, spaces and dashes are fine.
    let typed: String = code.chars().filter(|c| !matches!(c, ' ' | '-')).collect::<String>().to_ascii_uppercase();
    let live = st.pending.take().filter(|p| now < p.expires_ms);
    let good = live.as_ref().is_some_and(|p| p.transport == transport && same(typed.as_bytes(), p.code.as_bytes()));
    if !good {
        st.pending = live;
        if transport != "web" {
            st.failures += 1;
            if st.failures >= WRONG_CODES {
                (st.failures, st.blocked_until_ms) = (0, now + BLOCK_MS);
                // Only chat's own code goes; a live web link is not the chat's to spoil.
                if st.pending.as_ref().is_some_and(|p| p.transport == transport) {
                    st.pending = None;
                }
            }
        }
        return Err(RpcError::new("bridge", "wrong or expired code", "run `h0x-core bridge pair` for a new one"));
    }
    if transport != "web" {
        st.failures = 0;
    }
    st.identities.retain(|i| !(i.transport == transport && i.user_id == user_id));
    st.last_id += 1;
    let label = if label.is_empty() { user_id } else { label };
    let identity = Identity {
        id: st.last_id,
        transport: transport.into(),
        user_id: user_id.into(),
        label: label.into(),
        paired_at_ms: now,
    };
    st.identities.push(identity.clone());
    Ok(identity)
}

#[cfg(test)]
pub(crate) fn scratch() -> Bridge {
    let dir = std::env::temp_dir().join(format!("h0x-core-bridge-{}", crate::platform::random_hex()));
    Bridge::at(dir, BridgeConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_000_000;

    #[test]
    fn a_code_pairs_once_within_ten_minutes() {
        let b = scratch();
        let (code, expires) = b.pair("telegram", T).unwrap();
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|c| ALPHABET.contains(&c)), "{code}");
        assert_eq!(expires, T + 600_000);
        let typed = format!("{}-{}", &code[..4], code[4..].to_lowercase());
        let me = b.pair_verify("telegram", "42", "vijay", &typed, T + 1).unwrap();
        assert_eq!((me.id, me.user_id.as_str(), me.label.as_str()), (1, "42", "vijay"));
        let err = b.pair_verify("telegram", "43", "", &code, T + 2).unwrap_err();
        assert_eq!(err.message, "wrong or expired code", "single use");
        assert_eq!(b.identities().unwrap(), [me]);

        let (code, _) = b.pair("telegram", T).unwrap();
        assert!(b.pair_verify("telegram", "43", "", &code, T + 600_000).is_err(), "expired");
        let (code, _) = b.pair("telegram", T).unwrap();
        assert!(b.pair_verify("relay", "43", "", &code, T + 1).is_err(), "another transport's code");
        let again = b.pair_verify("telegram", "42", "", &code, T + 1).unwrap();
        assert_eq!((again.id, again.label.as_str()), (2, "42"), "re-pairing replaces, label defaults to the id");
        assert_eq!(b.identities().unwrap().len(), 1);
        assert!(b.pair("sms", T).is_err());
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn five_wrong_codes_disable_pairing_for_ten_minutes() {
        let b = scratch();
        let (code, _) = b.pair("telegram", T).unwrap();
        for n in 1..=4 {
            assert!(b.pair_verify("telegram", "666", "", "AAAAAAAA", T + n).is_err());
        }
        assert!(b.pair_verify("telegram", "666", "", "", T + 5).is_err(), "the fifth, empty");
        let err = b.pair_verify("telegram", "42", "", &code, T + 6).unwrap_err();
        assert!(err.message.contains("disabled for 10 more minute"), "{}", err.message);
        assert!(b.pair("telegram", T + 7).is_err(), "no new code while disabled either");
        let later = T + 5 + 600_000;
        assert!(b.pair_verify("telegram", "42", "", &code, later).is_err(), "the old code was dropped");
        let (code, _) = b.pair("telegram", later).unwrap();
        assert!(b.pair_verify("telegram", "42", "", &code, later + 1).is_ok());
        let log = b.log(100).unwrap();
        assert_eq!(log.len(), 8, "every attempt is audited");
        assert_eq!(log[7]["result"], "paired");
        assert!(log.iter().all(|e| !e.to_string().contains(&code)), "codes never reach the log");
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn a_code_holds_until_its_last_millisecond_and_near_misses_keep_it() {
        let b = scratch();
        let (code, expires) = b.pair("telegram", T).unwrap();
        let near = [String::new(), code[..7].to_string(), format!("{code}A"), format!("{code} 2")];
        for wrong in &near {
            assert!(b.pair_verify("telegram", "42", "", wrong, T + 1).is_err(), "{wrong:?}");
        }
        let err = b.pair_verify("telegram", "", "", &code, T + 1).unwrap_err();
        assert_eq!(err.message, "no user id", "not a wrong code: no fifth strike");
        let me = b.pair_verify("telegram", "42", "", &code.to_lowercase(), expires - 1).unwrap();
        assert_eq!(me.paired_at_ms, T + 599_999);
        // A good code clears the strikes: four more wrong ones don't block.
        let (code, _) = b.pair("telegram", T).unwrap();
        for n in 1..=4 {
            assert!(b.pair_verify("telegram", "43", "", "AAAAAAAA", T + n).is_err());
        }
        assert!(b.pair_verify("telegram", "43", "", &code, T + 5).is_ok());
        let log = b.log(100).unwrap();
        assert_eq!(log.len(), 11, "every attempt, the no-user one too");
        assert!(log.iter().all(|e| !e.to_string().contains(&near[1])), "typed codes never reach the log");
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn the_block_lifts_at_exactly_ten_minutes() {
        let b = scratch();
        let (code, _) = b.pair("telegram", T).unwrap();
        for _ in 0..4 {
            assert!(b.pair_verify("telegram", "666", "", "AAAAAAAA", T).is_err());
        }
        assert!(b.pair("telegram", T).is_ok(), "four wrong codes: not blocked yet");
        assert!(b.pair_verify("telegram", "666", "", "AAAAAAAA", T).is_err());
        let err = b.pair("telegram", T + 599_999).unwrap_err();
        assert!(err.message.contains("disabled for 1 more minute"), "{}", err.message);
        assert!(b.pair_verify("telegram", "42", "", &code, T + 599_999).is_err());
        let (code, _) = b.pair("telegram", T + 600_000).unwrap();
        assert!(b.pair_verify("telegram", "42", "", &code, T + 600_000).is_ok());
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn unpair_all_while_locked_keeps_the_lock() {
        let b = scratch();
        for user in ["1", "2"] {
            let (code, _) = b.pair("telegram", T).unwrap();
            b.pair_verify("telegram", user, "", &code, T).unwrap();
        }
        b.set_locked(true).unwrap();
        assert_eq!(b.unpair(None).unwrap(), 2);
        assert!(b.locked().unwrap(), "unpairing does not unlock");
        assert!(b.pair("telegram", T).is_err());
        b.set_locked(false).unwrap();
        assert!(b.identities().unwrap().is_empty());
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn a_corrupt_or_truncated_state_file_pairs_no_one() {
        let b = scratch();
        std::fs::create_dir_all(&b.dir).unwrap();
        for bad in ["", "{", r#"{"identities": ["#, "null", "\0\0\0\0", r#"{"identities": [{"id": 1}]}"#] {
            std::fs::write(b.dir.join("state.json"), bad).unwrap();
            assert!(b.identities().is_err(), "{bad:?}");
            assert!(b.locked().is_err(), "{bad:?}");
            assert!(b.pair("telegram", T).is_err(), "{bad:?}");
            assert!(b.pair_verify("telegram", "42", "", "ABCDEFGH", T).is_err(), "{bad:?}");
            assert_eq!(std::fs::read_to_string(b.dir.join("state.json")).unwrap(), bad, "left as it was");
        }
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn pairing_with_an_unwritable_audit_log_pairs_no_one() {
        let b = scratch();
        let (code, _) = b.pair("telegram", T).unwrap();
        std::fs::create_dir(b.dir.join("audit.jsonl")).unwrap();
        assert!(b.pair_verify("telegram", "42", "", &code, T).is_err());
        assert!(b.identities().unwrap().is_empty(), "an attempt the log can't keep must not pair");
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn reset_moves_a_corrupt_state_aside_and_starts_empty() {
        let b = scratch();
        assert_eq!(b.reset(T).unwrap(), None, "nothing to move");
        std::fs::create_dir_all(&b.dir).unwrap();
        for (n, bad) in ["[]", "null", "42", "\"x\"", "{", r#"{"locked": "yes"}"#].into_iter().enumerate() {
            std::fs::write(b.dir.join("state.json"), bad).unwrap();
            let err = b.identities().unwrap_err();
            assert!(err.message.contains("run `h0x-core bridge reset`"), "{bad}: {}", err.message);
            let at = T + n as u64;
            let aside = b.reset(at).unwrap().unwrap();
            assert_eq!(aside, b.dir.join(format!("state.json.bad-{at}")));
            assert_eq!(std::fs::read_to_string(&aside).unwrap(), bad, "kept as it was");
            assert!(b.identities().unwrap().is_empty() && !b.locked().unwrap(), "{bad}");
        }
        let (code, _) = b.pair("telegram", T).unwrap();
        b.pair_verify("telegram", "42", "", &code, T).unwrap();
        b.reset(T + 100).unwrap();
        assert!(b.identities().unwrap().is_empty(), "a good state can be reset too");
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn unpair_one_or_all() {
        let b = scratch();
        for user in ["1", "2", "3"] {
            let (code, _) = b.pair("telegram", T).unwrap();
            b.pair_verify("telegram", user, "", &code, T).unwrap();
        }
        assert_eq!(b.unpair(Some(2)).unwrap(), 1);
        assert_eq!(b.unpair(Some(2)).unwrap_err().kind, "not_found");
        let ids: Vec<u32> = b.identities().unwrap().iter().map(|i| i.id).collect();
        assert_eq!(ids, [1, 3]);
        assert_eq!(b.unpair(None).unwrap(), 2);
        assert_eq!(b.unpair(None).unwrap(), 0);
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn a_lock_stops_pairing_and_drops_the_code() {
        let b = scratch();
        assert!(!b.locked().unwrap(), "nothing on disk: unlocked");
        let (code, _) = b.pair("telegram", T).unwrap();
        b.set_locked(true).unwrap();
        assert!(b.pair("telegram", T).is_err());
        assert!(b.pair_verify("telegram", "42", "", &code, T).is_err());
        b.set_locked(false).unwrap();
        assert!(b.pair_verify("telegram", "42", "", &code, T).is_err(), "locking dropped the code");
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn state_and_audit_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let b = scratch();
        let (code, _) = b.pair("telegram", T).unwrap();
        b.pair_verify("telegram", "42", "", &code, T).unwrap();
        let mode = |name: &str| std::fs::metadata(b.dir.join(name)).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode("state.json"), mode("audit.jsonl"), mode(".")), (0o600, 0o600, 0o700));
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn a_windows_style_config_folder_holds_the_bridge() {
        let root = std::env::temp_dir().join(format!("h0x-core-bridge-{}", crate::platform::random_hex()));
        let config = root.join(r"AppData\Roaming").join("h0x-core").join("config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[bridge]\nrate_limit = 3\nnotice_quiet = 2.5\n").unwrap();
        let paths = crate::paths::Paths {
            socket: root.join("s.sock"),
            holder_socket: root.join("h.sock"),
            state: root.join("state"),
            config: config.clone(),
        };
        let b = Bridge::open(&paths).unwrap();
        assert_eq!(b.dir, config.parent().unwrap().join("bridge"));
        assert_eq!((b.config.rate_limit(), b.config.notice_quiet_ms()), (3, 2500));
        b.pair("telegram", T).unwrap();
        assert!(b.dir.join("state.json").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod audit_and_pairing_tests {
    use super::*;

    const T: u64 = 1_000_000;

    fn fill_audit(b: &Bridge, bytes: u64) {
        std::fs::create_dir_all(&b.dir).unwrap();
        std::fs::write(b.dir.join("audit.jsonl"), vec![b'x'; usize::try_from(bytes).unwrap()]).unwrap();
    }

    #[test]
    fn the_audit_log_rotates_exactly_at_its_size_limit() {
        let b = scratch();
        fill_audit(&b, AUDIT_MAX - 1);
        b.audit(json!({"n": 1})).unwrap();
        assert!(!b.dir.join("audit.jsonl.1").exists(), "one byte under the limit: no rotation");
        let size = std::fs::metadata(b.dir.join("audit.jsonl")).unwrap().len();
        assert!(size >= AUDIT_MAX, "the entry was appended: {size}");

        b.audit(json!({"n": 2})).unwrap();
        let old = std::fs::metadata(b.dir.join("audit.jsonl.1")).unwrap().len();
        assert_eq!(old, size, "the full log moved aside whole");
        assert_eq!(b.log(10).unwrap(), [json!({"n": 2})], "a new log starts with the new entry");

        fill_audit(&b, AUDIT_MAX);
        b.audit(json!({"n": 3})).unwrap();
        let second = std::fs::read(b.dir.join("audit.jsonl.1")).unwrap();
        assert_eq!(second.len() as u64, AUDIT_MAX, "exactly at the limit rotates, replacing the older .1");
        assert_eq!(b.log(10).unwrap(), [json!({"n": 3})]);
        std::fs::remove_dir_all(&b.dir).unwrap();
    }

    #[test]
    fn wrong_web_codes_never_count_against_chat_pairing_and_chat_failures_never_block_web() {
        let b = scratch();
        b.pair("web", T).unwrap();
        for n in 0..10 {
            assert!(b.pair_verify("web", &format!("u{n}"), "", "AAAAAAAA", T + n).is_err());
        }
        let (chat, _) = b.pair("telegram", T + 20).unwrap();
        assert!(b.pair_verify("telegram", "42", "", &chat, T + 21).is_ok(), "no block from web guesses");

        let (web, _) = b.pair("web", T + 30).unwrap();
        for n in 0..5 {
            assert!(b.pair_verify("telegram", "666", "", "AAAAAAAA", T + 40 + n).is_err());
        }
        assert!(b.pair("telegram", T + 50).is_err(), "chat is blocked");
        assert!(b.pair_verify("web", "u99", "", &web, T + 51).is_ok(), "web verify is not");
        std::fs::remove_dir_all(&b.dir).unwrap();
    }
}
