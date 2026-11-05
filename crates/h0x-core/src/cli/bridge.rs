//! `h0x-core bridge pair|unpair|ls|lock|unlock|log`: who may control this
//! machine remotely. Local files only: no core needed.

use super::*;
use crate::bridge::Bridge;

#[derive(Subcommand)]
pub(super) enum BridgeCmd {
    /// Print a one-time pairing code (8 characters, valid 10 minutes, single use).
    Pair {
        /// Where the account to pair writes from.
        #[arg(long, default_value = "telegram", value_parser = clap::builder::PossibleValuesParser::new(crate::bridge::TRANSPORTS))]
        transport: String,
    },
    /// Remove a paired account (its id in `bridge ls`), or every one.
    Unpair {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<u32>,
        #[arg(long)]
        all: bool,
    },
    /// List paired accounts and whether remote control is locked.
    Ls,
    /// Refuse every remote command until `bridge unlock` on this machine.
    Lock,
    /// Allow remote commands again (only here, never remotely).
    Unlock,
    /// The last audit log entries: every remote command and pairing attempt.
    Log {
        /// How many entries.
        #[arg(short = 'n', default_value_t = 20)]
        n: usize,
    },
}

/// `5m ago`, from two ms timestamps.
fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then) / 1000;
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

pub(super) fn bridge(paths: &Paths, action: BridgeCmd) -> Result<Out, RpcError> {
    let bridge = Bridge::open(paths)?;
    let now = crate::runtime::now_ms();
    let locked = |locked: bool| {
        let human = if locked { "remote control locked" } else { "remote control unlocked" };
        Out { json: json!({"locked": locked}), human: human.into() }
    };
    Ok(match action {
        BridgeCmd::Pair { transport } => {
            let (code, expires_ms) = bridge.pair(&transport, now)?;
            let human = format!(
                "pairing code for {transport}: {code}\nvalid 10 minutes, once; it gives that account full control of this machine"
            );
            Out { json: json!({"code": code, "transport": transport, "expires_ms": expires_ms}), human }
        }
        BridgeCmd::Unpair { id, all: _ } => {
            let removed = bridge.unpair(id)?;
            Out { json: json!({"removed": removed}), human: format!("removed {removed} paired account(s)") }
        }
        BridgeCmd::Ls => {
            let identities = bridge.identities()?;
            let locked = bridge.locked()?;
            let mut rows: Vec<String> = identities
                .iter()
                .map(|i| {
                    let when = ago(now, i.paired_at_ms);
                    format!("{:<4} {:<9} {:<16} {:<20} paired {when}", i.id, i.transport, i.user_id, i.label)
                })
                .collect();
            if rows.is_empty() {
                rows.push("no paired accounts (h0x-core bridge pair)".into());
            }
            rows.push(format!("remote control {}", if locked { "locked" } else { "unlocked" }));
            Out { json: json!({"identities": identities, "locked": locked}), human: rows.join("\n") }
        }
        BridgeCmd::Lock => {
            bridge.set_locked(true)?;
            locked(true)
        }
        BridgeCmd::Unlock => {
            bridge.set_locked(false)?;
            locked(false)
        }
        BridgeCmd::Log { n } => {
            let entries = bridge.log(n)?;
            let rows: Vec<String> = entries
                .iter()
                .map(|e| {
                    let field = |k: &str| e[k].as_str().unwrap_or_default().to_string();
                    let when = ago(now, e["time_ms"].as_u64().unwrap_or_default());
                    format!(
                        "{when:<8} {:<20} {:<9} {:<6} {}",
                        field("identity"),
                        field("command"),
                        field("terminal"),
                        field("result")
                    )
                })
                .collect();
            let human = if rows.is_empty() { "no remote commands yet".into() } else { rows.join("\n") };
            Out { json: json!({"entries": entries}), human }
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn ages_read_like_people_say_them() {
        let ago = |s: u64| super::ago(1_000_000_000 + s * 1000, 1_000_000_000);
        assert_eq!(
            [ago(0), ago(59), ago(60), ago(3599), ago(3600), ago(86_400)],
            ["0s ago", "59s ago", "1m ago", "59m ago", "1h ago", "1d ago"]
        );
        assert_eq!(super::ago(0, 5_000), "0s ago", "a clock that went back");
    }
}
