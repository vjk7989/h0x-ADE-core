//! Core events turned into notices for a person away from the screen: it
//! needs you, it is done, it hit an error, it finished. At most one notice
//! per terminal per quiet period (default 10 s); a newer one replaces a
//! waiting one, and going back to work drops it.

use super::redact;
use crate::cli::output::{agent, cwd, status};
use crate::paths::Paths;
use crate::rpc::RpcError;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// Blocked on a person (a permission, a question).
    NeedsYou,
    /// A turn ended.
    Done,
    Error,
    /// Its program exited or was killed.
    Finished,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub terminal: String,
    pub agent: String,
    pub folder: String,
    pub kind: Kind,
    /// What it said about it (its status message or title, else how it
    /// ended), redacted, at most 200 characters.
    pub excerpt: String,
}

pub struct Notices {
    quiet_ms: u64,
    /// Every terminal's record as last heard, by address.
    records: HashMap<String, Value>,
    /// When each terminal's last notice went out.
    sent: HashMap<String, u64>,
    /// Notices held back by the quiet period (sorted: notices come out in a stable order).
    waiting: BTreeMap<String, Notice>,
}

impl Notices {
    /// `terminals`: the subscription's snapshot.
    pub fn new(quiet_ms: u64, terminals: &Value) -> Notices {
        let mut notices = Notices { quiet_ms, records: HashMap::new(), sent: HashMap::new(), waiting: BTreeMap::new() };
        notices.remember(terminals);
        notices
    }

    fn remember(&mut self, terminals: &Value) {
        for t in terminals.as_array().into_iter().flatten() {
            if let Some(address) = t["address"].as_str() {
                self.records.insert(address.into(), t.clone());
            }
        }
    }

    /// One line from `events.subscribe` (an event, a `missed` snapshot, a
    /// heartbeat): the notices due now.
    pub fn line(&mut self, message: &Value, now: u64) -> Vec<Notice> {
        let params = &message["params"];
        match message["method"].as_str() {
            Some("missed") => self.remember(&params["terminals"]),
            Some("event") => {
                if let Some(notice) = self.event(params) {
                    self.waiting.insert(notice.terminal.clone(), notice);
                }
            }
            _ => {}
        }
        let quiet = self.quiet_ms;
        let due: Vec<String> = self
            .waiting
            .keys()
            .filter(|t| self.sent.get(*t).is_none_or(|at| now.saturating_sub(*at) >= quiet))
            .cloned()
            .collect();
        due.into_iter()
            .filter_map(|t| {
                self.sent.insert(t.clone(), now);
                self.waiting.remove(&t)
            })
            .collect()
    }

    fn event(&mut self, params: &Value) -> Option<Notice> {
        let address = params["terminal"].as_str()?.to_string();
        let data = &params["data"];
        let kind = params["type"].as_str()?;
        if matches!(kind, "created" | "restarted") {
            self.records.insert(address, data.clone());
            return None;
        }
        if kind == "removed" {
            self.records.remove(&address);
            self.sent.remove(&address);
            self.waiting.remove(&address);
            return None;
        }
        let record = self.records.entry(address.clone()).or_insert_with(|| json!({"address": address}));
        let kind = match kind {
            "title" => {
                record["title"] = data["title"].clone();
                return None;
            }
            "status" => {
                let was = record["status"]["state"].clone();
                record["status"] = data["status"].clone();
                let state = data["status"]["state"].as_str();
                if state == Some("working") {
                    self.waiting.remove(&address);
                }
                match state {
                    _ if was == data["status"]["state"] => return None,
                    Some("blocked") => Kind::NeedsYou,
                    Some("error") => Kind::Error,
                    _ => return None,
                }
            }
            // A turn ending blocked or in error is told by its status.
            "turn" if matches!(data["state"].as_str(), Some("done" | "idle")) => Kind::Done,
            "finished" => {
                record["state"] = "finished".into();
                (record["exit_code"], record["signal"]) = (data["exit_code"].clone(), data["signal"].clone());
                Kind::Finished
            }
            _ => return None,
        };
        let said =
            [&record["status"]["msg"], &record["title"]].into_iter().filter_map(Value::as_str).find(|s| !s.is_empty());
        let excerpt = match (kind, said) {
            (Kind::Finished, _) => match data["reason"].as_str() {
                Some(reason) if reason != "exited" => reason.into(),
                _ => status(record).0,
            },
            (_, Some(said)) => redact(said).chars().take(200).collect(),
            (_, None) => String::new(),
        };
        Some(Notice {
            terminal: address,
            agent: agent(record),
            folder: cwd(record).unwrap_or_default().into(),
            kind,
            excerpt,
        })
    }
}

/// Subscribes to every terminal here and on connected machines and calls
/// `on` with each notice until the core goes away.
pub fn watch(paths: &Paths, quiet_ms: u64, mut on: impl FnMut(Notice)) -> Result<(), RpcError> {
    use std::io::BufRead;
    let stream = crate::client::connect_or_start(paths)?;
    let snapshot = crate::client::call(&stream, "events.subscribe", json!({}))?;
    let mut notices = Notices::new(quiet_ms, &snapshot["terminals"]);
    for line in std::io::BufReader::new(&stream).lines() {
        let Ok(line) = line else { break };
        let message = serde_json::from_str(&line).unwrap_or_default();
        notices.line(&message, crate::runtime::now_ms()).into_iter().for_each(&mut on);
    }
    Err(crate::client::explain_closed(paths, RpcError::new("unavailable", "the core went away", "")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, terminal: &str, data: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "event", "params": {"type": kind, "terminal": terminal, "data": data, "seq": 1}})
    }

    fn status(state: &str, msg: &str) -> Value {
        json!({"status": {"state": state, "app": "claude-code", "msg": msg}, "records": {}, "reported": true})
    }

    fn heartbeat() -> Value {
        json!({"jsonrpc": "2.0", "method": "heartbeat", "params": {"seq": 1}})
    }

    fn kinds(notices: Vec<Notice>) -> Vec<(String, Kind)> {
        notices.into_iter().map(|n| (n.terminal, n.kind)).collect()
    }

    fn snapshot() -> Value {
        json!([{"address": "t1", "argv": ["/usr/bin/claude"], "cwd": r"C:\work\app", "state": "live", "status": null},
               {"address": "m/t2", "argv": ["codex"], "cwd": "/srv/api", "state": "live", "status": null}])
    }

    #[test]
    fn status_turn_and_finished_become_notices() {
        let mut n = Notices::new(10_000, &snapshot());
        assert!(n.line(&event("status", "t1", status("working", "")), 0).is_empty());
        let blocked =
            n.line(&event("status", "t1", status("blocked", "Allow Bash: rm -rf build? token=abcdef0123456789")), 1);
        assert_eq!(
            blocked,
            [Notice {
                terminal: "t1".into(),
                agent: "claude-code".into(),
                folder: r"C:\work\app".into(),
                kind: Kind::NeedsYou,
                excerpt: "Allow Bash: rm -rf build? token=[redacted]".into(),
            }]
        );
        assert!(n.line(&event("status", "t1", status("blocked", "again")), 20_000).is_empty(), "no change");
        assert!(n.line(&event("turn", "t1", json!({"turn": 1, "state": "blocked"})), 20_001).is_empty());
        n.line(&event("status", "t1", status("working", "")), 20_002);
        n.line(&event("status", "t1", status("done", "")), 30_000);
        let done = n.line(&event("turn", "t1", json!({"turn": 2, "state": "done"})), 30_000);
        assert_eq!((done[0].kind, done[0].excerpt.as_str()), (Kind::Done, ""));
        let error = n.line(&event("status", "m/t2", status("error", "")), 30_000);
        assert_eq!(
            (error[0].terminal.as_str(), error[0].kind, error[0].folder.as_str()),
            ("m/t2", Kind::Error, "/srv/api")
        );
        let end = json!({"reason": "exited", "exit_code": 3, "signal": null});
        let finished = n.line(&event("finished", "m/t2", end), 50_000);
        assert_eq!((finished[0].kind, finished[0].excerpt.as_str()), (Kind::Finished, "exit 3"));
        let killed = n.line(&event("finished", "t1", json!({"reason": "killed"})), 50_000);
        assert_eq!(killed[0].excerpt, "killed");
    }

    #[test]
    fn one_notice_per_terminal_per_quiet_period() {
        let mut n = Notices::new(10_000, &snapshot());
        let blocked = || event("status", "t1", status("blocked", "?"));
        let working = || event("status", "t1", status("working", ""));
        assert_eq!(kinds(n.line(&blocked(), 0)), [("t1".into(), Kind::NeedsYou)]);
        // Flapping: answered and blocked again within the period is held back...
        n.line(&working(), 1_000);
        assert!(n.line(&blocked(), 2_000).is_empty());
        assert!(n.line(&heartbeat(), 9_999).is_empty());
        // ...and other terminals are not held up by it.
        assert_eq!(kinds(n.line(&event("status", "m/t2", status("error", "")), 9_999)), [("m/t2".into(), Kind::Error)]);
        assert_eq!(kinds(n.line(&heartbeat(), 10_000)), [("t1".into(), Kind::NeedsYou)], "sent once the period ends");
        // Back at work before the period ends: the held notice is dropped.
        n.line(&working(), 11_000);
        assert!(n.line(&blocked(), 12_000).is_empty());
        n.line(&working(), 13_000);
        assert!(n.line(&heartbeat(), 30_000).is_empty());
        // A newer notice replaces a held one.
        assert_eq!(kinds(n.line(&blocked(), 40_000)), [("t1".into(), Kind::NeedsYou)]);
        assert!(n.line(&event("status", "t1", status("error", "")), 41_000).is_empty());
        assert!(n.line(&event("finished", "t1", json!({"reason": "exited", "exit_code": 0})), 42_000).is_empty());
        assert_eq!(kinds(n.line(&heartbeat(), 50_000)), [("t1".into(), Kind::Finished)]);
        // Removed terminals are forgotten, held notices included.
        n.line(&event("status", "t1", status("blocked", "")), 51_000);
        n.line(&event("removed", "t1", json!({})), 52_000);
        assert!(n.line(&heartbeat(), 70_000).is_empty());
    }

    #[test]
    fn new_and_unknown_terminals_and_odd_lines() {
        let mut n = Notices::new(0, &Value::Null);
        let record = json!({"address": "t5", "argv": ["/bin/zsh"], "cwd": "/tmp", "state": "live"});
        n.line(&event("created", "t5", record), 0);
        n.line(&event("title", "t5", json!({"title": "make test"})), 0);
        let done = n.line(&event("turn", "t5", json!({"turn": 1, "state": "idle"})), 0);
        assert_eq!(
            (done[0].agent.as_str(), done[0].folder.as_str(), done[0].excerpt.as_str()),
            ("zsh", "/tmp", "make test")
        );
        let stranger = n.line(&event("status", "t6", status("blocked", "")), 0);
        assert_eq!((stranger[0].agent.as_str(), stranger[0].folder.as_str()), ("claude-code", ""));
        for odd in [
            Value::Null,
            json!({"method": "event"}),
            json!({"method": "event", "params": {"type": "size", "terminal": "t5"}}),
        ] {
            assert!(n.line(&odd, 0).is_empty(), "{odd}");
        }
        let missed = json!({"method": "missed", "params": {"seq": 9, "terminals": [{"address": "t7", "argv": ["x"], "cwd": "/y"}]}});
        n.line(&missed, 0);
        assert_eq!(n.line(&event("status", "t7", status("error", "")), 0)[0].folder, "/y");
        let long = "x".repeat(500);
        assert_eq!(n.line(&event("status", "t5", status("blocked", &long)), 0)[0].excerpt.len(), 200);
    }
}
