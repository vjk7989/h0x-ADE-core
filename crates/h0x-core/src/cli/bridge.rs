//! `h0x-core bridge pair|unpair|ls|lock|unlock|reset|log|telegram|run`: who
//! may control this machine remotely, and the Telegram bot. Local files only,
//! except `run`.

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
    /// Move state.json aside (kept as state.json.bad-<time>) and start empty:
    /// no paired accounts, unlocked. For a corrupt state; only here.
    Reset,
    /// The last audit log entries: every remote command and pairing attempt.
    Log {
        /// How many entries.
        #[arg(short = 'n', default_value_t = 20)]
        n: usize,
    },
    /// The Telegram bot's settings.
    Telegram {
        #[command(subcommand)]
        action: TelegramCmd,
    },
    /// Run the Telegram bot until stopped: answers paired accounts, sends notices.
    /// Under core: h0x-core run --bg --keep -- h0x-core bridge run
    Run,
    /// Serve the web panel until stopped: a page and a WebSocket API for paired devices.
    /// On 127.0.0.1; --lan: this machine's private network address (plain http).
    Web {
        /// Listen on this machine's private LAN address instead of 127.0.0.1.
        #[arg(long)]
        lan: bool,
        /// The port (0: any free one).
        #[arg(long, default_value_t = crate::bridge::web::PORT)]
        port: u16,
        /// Reach it through your VPS: the name of an ssh machine (`machine add`).
        /// Opens a reverse port on the server's loopback; your HTTPS proxy there serves it.
        #[arg(long = "via", value_name = "MACHINE", requires = "public_host", conflicts_with = "lan")]
        via: Option<String>,
        /// The DNS name your HTTPS proxy serves, for the pairing link.
        #[arg(long, requires = "via")]
        public_host: Option<String>,
        /// The port on the server's loopback.
        #[arg(long, default_value_t = crate::bridge::web::PORT, requires = "via")]
        remote_port: u16,
    },
}

#[derive(Subcommand)]
pub(super) enum TelegramCmd {
    /// Store the bot token from @BotFather, read from stdin (not shown as you type).
    Token {
        /// Remove the stored token instead.
        #[arg(long)]
        clear: bool,
    },
}

/// The bot token: typed without echo in a terminal, else the first line piped in.
fn read_token() -> Result<String, RpcError> {
    if !std::io::stdin().is_terminal() {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map_err(|e| RpcError::internal(e.to_string()))?;
        return Ok(text.lines().next().unwrap_or_default().to_string());
    }
    eprint!("bot token from @BotFather (not shown), then enter: ");
    let mut token = Vec::new();
    {
        let _raw = crate::platform::console::RawMode::enter();
        let mut input = crate::platform::console::stdin();
        let mut byte = [0u8];
        while input.read(&mut byte).map_err(|e| RpcError::internal(e.to_string()))? == 1 {
            match byte[0] {
                b'\r' | b'\n' => break,
                3 | 4 => return Err(cancelled()),
                8 | 127 => drop(token.pop()),
                b => token.push(b),
            }
        }
    }
    eprintln!();
    String::from_utf8(token).map_err(|_| RpcError::invalid_params("that is not a bot token"))
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
            let minutes = (expires_ms - now) / 60_000;
            let mut human = format!(
                "pairing code for {transport}: {code}\nvalid {minutes} minutes, once; it gives that account full control of this machine"
            );
            if transport == "web" {
                human.push_str("\nenter it on the web panel, or use the link and QR code `h0x-core bridge web` prints");
            }
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
            let token = if bridge.telegram_token()?.is_some() { "set" } else { "unset" };
            rows.push(format!("telegram token: {token}"));
            let json = json!({"identities": identities, "locked": locked, "telegram_token": token});
            Out { json, human: rows.join("\n") }
        }
        BridgeCmd::Telegram { action: TelegramCmd::Token { clear: true } } => {
            let human =
                if bridge.clear_telegram_token()? { "telegram token removed" } else { "no telegram token was set" };
            Out { json: json!({"telegram_token": "unset"}), human: human.into() }
        }
        BridgeCmd::Telegram { action: TelegramCmd::Token { clear: false } } => {
            bridge.set_telegram_token(&read_token()?)?;
            Out { json: json!({"telegram_token": "set"}), human: "telegram token stored".into() }
        }
        BridgeCmd::Web { lan, port, via, public_host, remote_port } => {
            let via = match (via, public_host) {
                (Some(machine), Some(host)) => {
                    Some(crate::bridge::web::vps::plan(paths, &machine, &host, remote_port)?)
                }
                _ => None,
            };
            crate::bridge::web::run(paths, lan, port, via)?;
            Out { json: Value::Null, human: String::new() }
        }
        BridgeCmd::Run => {
            crate::bridge::telegram::run(paths)?;
            Out { json: Value::Null, human: String::new() }
        }
        BridgeCmd::Lock => {
            bridge.set_locked(true)?;
            locked(true)
        }
        BridgeCmd::Unlock => {
            bridge.set_locked(false)?;
            locked(false)
        }
        BridgeCmd::Reset => {
            let aside = bridge.reset(now)?;
            let human = match &aside {
                Some(path) => format!("moved the old state to {}; no paired accounts now", path.display()),
                None => "nothing to reset".into(),
            };
            Out { json: json!({"moved_to": aside}), human }
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

    #[test]
    fn web_via_needs_a_public_host_and_excludes_lan() {
        use super::{BridgeCmd, Cli, Cmd};
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from([&["h0x-core", "bridge", "web"], args].concat());
        for args in [
            &["--via", "vps"][..],
            &["--public-host", "h0x.example.com"],
            &["--remote-port", "9000"],
            &["--public-host", "h0x.example.com", "--remote-port", "9000"],
            &["--via", "vps", "--public-host", "h0x.example.com", "--lan"],
            &["--via", "vps", "--public-host", "h0x.example.com", "--remote-port", "65536"],
            &["--via", "vps", "--public-host", "h0x.example.com", "--remote-port", "-1"],
        ] {
            assert!(parse(args).is_err(), "{args:?} must be refused");
        }
        let Some(Cmd::Bridge { action: BridgeCmd::Web { lan, via, public_host, remote_port, .. } }) =
            parse(&["--via", "vps", "--public-host", "h0x.example.com"]).unwrap().cmd
        else {
            panic!("bridge web")
        };
        assert!(!lan);
        assert_eq!((via.as_deref(), public_host.as_deref()), (Some("vps"), Some("h0x.example.com")));
        assert_eq!(remote_port, crate::bridge::web::PORT, "default remote port");
        let Some(Cmd::Bridge { action: BridgeCmd::Web { via, remote_port, .. } }) = parse(&[]).unwrap().cmd else {
            panic!("bridge web")
        };
        assert_eq!(via, None, "plain bridge web still parses");
        assert_eq!(remote_port, crate::bridge::web::PORT);
    }
}
