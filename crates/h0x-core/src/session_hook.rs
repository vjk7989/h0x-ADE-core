//! `h0x-core hook session [--agent <name>]`: a SessionStart hook that records the agent's exact
//! session id on the HyperOx terminal it runs in (private label `h0x.session=<agent>:<id>`).
//! Only a vouched terminal gets the label. It never fails and never prints: any problem is exit 0.

use serde_json::{Value, json};
use std::io::Read;
use std::time::Duration;

/// The most hook input read; larger is bad input.
const MAX_INPUT: u64 = 4 * 1024 * 1024;
/// The most the label update may delay the hook's exit.
const LABEL_WAIT: Duration = Duration::from_millis(500);
const LABEL: &str = "h0x.session";

pub fn run(args: &[Option<&str>]) -> i32 {
    let mut raw = Vec::new();
    let mut stdin = std::io::stdin();
    if (&mut stdin).take(MAX_INPUT + 1).read_to_end(&mut raw).is_err() {
        return 0;
    }
    if raw.len() as u64 > MAX_INPUT {
        let _ = std::io::copy(&mut stdin, &mut std::io::sink()); // the writer must never see a broken pipe
        return 0;
    }
    std::panic::set_hook(Box::new(|_| {})); // a hook never talks to the agent: stay quiet
    let agent = agent(args);
    let _ = std::panic::catch_unwind(|| {
        if let Some(value) = agent.and_then(|a| value(a, &raw))
            && crate::client::vouched()
        {
            record(value);
        }
    });
    0
}

/// The agent named by `--agent`, `claude` without it; None for an unknown shape of arguments.
fn agent<'a>(args: &[Option<&'a str>]) -> Option<&'a str> {
    let name = match args {
        [] => "claude",
        [Some("--agent"), Some(name)] => name,
        _ => return None,
    };
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        .then_some(name)
}

/// The label value for one SessionStart event, or None when it is not one or its id is not a
/// canonical lower-case UUID.
fn value(agent: &str, event: &[u8]) -> Option<String> {
    let ev: Value = serde_json::from_slice(event).ok()?;
    if ev.get("hook_event_name").is_some_and(|n| n.as_str() != Some("SessionStart")) {
        return None;
    }
    let id = ev["session_id"].as_str().filter(|id| canonical_uuid(id))?;
    Some(format!("{agent}:{id}"))
}

fn canonical_uuid(id: &str) -> bool {
    id != "00000000-0000-0000-0000-000000000000"
        && id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// Sets the label through the terminal's own socket, without holding the hook up for long.
fn record(value: String) {
    let (Ok(terminal), Some(socket)) = (std::env::var("H0X_TERMINAL_ID"), std::env::var_os("H0X_TERMINAL_SOCKET"))
    else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = crate::platform::connect(std::path::Path::new(&socket)).ok();
        let labels = json!({(LABEL): value});
        let _ =
            stream.map(|s| crate::client::call(&s, "terminal.label", json!({"terminal": terminal, "labels": labels})));
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(LABEL_WAIT);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";

    fn event(extra: &str) -> Vec<u8> {
        format!(r#"{{"session_id":"{ID}","hook_event_name":"SessionStart","source":"startup"{extra}}}"#).into_bytes()
    }

    #[test]
    fn a_session_start_gives_the_agent_and_id() {
        assert_eq!(value("claude", &event("")).as_deref(), Some(format!("claude:{ID}").as_str()));
        for source in ["startup", "resume", "clear", "compact"] {
            let e = format!(r#"{{"session_id":"{ID}","source":"{source}"}}"#);
            assert!(value("claude", e.as_bytes()).is_some(), "{source}");
        }
    }

    #[test]
    fn anything_else_gives_nothing() {
        let upper = ID.to_uppercase();
        let bad = [
            String::new(),
            "not json".into(),
            "[]".into(),
            "{}".into(),
            r#"{"session_id":null}"#.into(),
            r#"{"session_id":7}"#.into(),
            r#"{"session_id":""}"#.into(),
            format!(r#"{{"session_id":"{upper}"}}"#),
            format!(r#"{{"session_id":"{}"}}"#, ID.replace('-', "")),
            format!(r#"{{"session_id":"{ID}x"}}"#),
            format!(r#"{{"session_id":" {ID}"}}"#),
            format!(r#"{{"session_id":"{}"}}"#, ID.replace('0', "g")),
            format!(r#"{{"session_id":"{ID}","hook_event_name":"Stop"}}"#),
        ];
        for e in bad {
            assert_eq!(value("claude", e.as_bytes()), None, "{e}");
        }
    }

    #[test]
    fn the_agent_defaults_to_claude_and_must_be_a_plain_name() {
        assert_eq!(agent(&[]), Some("claude"));
        assert_eq!(agent(&[Some("--agent"), Some("codex")]), Some("codex"));
        assert_eq!(agent(&[Some("--agent"), Some("gemini-cli")]), Some("gemini-cli"));
        for bad in [
            vec![Some("--agent")],
            vec![Some("--agent"), Some("")],
            vec![Some("--agent"), Some("Claude")],
            vec![Some("--agent"), Some("a:b")],
            vec![Some("--agent"), None],
            vec![Some("--other"), Some("x")],
            vec![Some("--agent"), Some("a"), Some("b")],
        ] {
            assert_eq!(agent(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_agent_name_is_lower_case_letters_digits_and_dashes_only() {
        for good in ["a", "0", "-", "codex2", "a-b-c"] {
            assert_eq!(agent(&[Some("--agent"), Some(good)]), Some(good), "{good:?}");
        }
        for bad in ["a b", " claude", "claude ", "CLAUDE", "cl\u{e9}", "a_b", "a.b", "a/b", "a\tb", "a\nb", "a=b"] {
            assert_eq!(agent(&[Some("--agent"), Some(bad)]), None, "{bad:?}");
        }
        assert_eq!(agent(&[Some("--agent=codex")]), None, "only the two-word form");
        assert_eq!(agent(&[None]), None, "a non-UTF-8 argument");
    }

    #[test]
    fn only_a_canonical_lower_case_uuid_is_an_id() {
        let ok = |id: &str| value("claude", format!(r#"{{"session_id":"{id}"}}"#).as_bytes()).is_some();
        assert!(ok(ID));
        assert!(ok("ffffffff-ffff-ffff-ffff-ffffffffffff"));
        assert!(!ok("00000000-0000-0000-0000-000000000000"), "the nil UUID is no session");
        let mixed = format!("{}A{}", &ID[..1], &ID[2..]);
        for bad in [
            ID.to_uppercase(),
            mixed,
            format!("{{{ID}}}"),
            format!("urn:uuid:{ID}"),
            ID[..35].to_string(),
            format!("{ID}0"),
            ID.replacen('-', "", 1) + "0",
            // Dashes one place off.
            format!("{}-{}", &ID[..7], &ID[8..]).replacen("--", "-0", 1),
            // 36 bytes but not ASCII.
            format!("{}\u{e9}", &ID[..34]),
        ] {
            assert!(!ok(&bad), "{bad:?}");
        }
    }

    #[test]
    fn only_a_session_start_event_object_counts() {
        let id = |extra: &str| format!(r#"{{"session_id":"{ID}"{extra}}}"#);
        assert!(value("claude", id("").as_bytes()).is_some(), "a missing event name is accepted");
        assert!(value("claude", id(r#","hook_event_name":"SessionStart""#).as_bytes()).is_some());
        assert!(value("claude", id(r#","source":"compact""#).as_bytes()).is_some(), "any source, compact too");
        assert!(value("claude", id(r#","source":"something-new""#).as_bytes()).is_some());
        for other in ["Stop", "sessionstart", "SessionStart ", "", "SessionEnd"] {
            let e = id(&format!(r#","hook_event_name":"{other}""#));
            assert_eq!(value("claude", e.as_bytes()), None, "{other:?}");
        }
        // A non-string event name is "other than SessionStart" too.
        for other in ["7", "true", "{}", r#"["SessionStart"]"#] {
            let e = id(&format!(r#","hook_event_name":{other}"#));
            assert_eq!(value("claude", e.as_bytes()), None, "{other}");
        }
        assert_eq!(value("claude", format!("[{}]", id("")).as_bytes()), None, "an array holding a valid event");
        assert_eq!(value("claude", &[0xff, 0xfe, b'{', b'}']), None, "invalid UTF-8");
        assert_eq!(value("claude", b""), None, "empty");
        assert_eq!(value("claude", b"null"), None);
        assert_eq!(value("claude", format!(r#""{ID}""#).as_bytes()), None, "a bare string");
        let pad = format!("{}{}", id(""), " ".repeat(64));
        assert!(value("claude", pad.as_bytes()).is_some(), "trailing whitespace is still one object");
        assert_eq!(value("claude", format!("{}{}", id(""), id("")).as_bytes()), None, "two objects");
    }

    #[test]
    fn the_label_value_is_agent_colon_id() {
        assert_eq!(value("gemini-cli", &event("")).as_deref(), Some(format!("gemini-cli:{ID}").as_str()));
    }
}
