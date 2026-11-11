//! `h0x-core bridge pair|unpair|ls|lock|unlock|reset|log|qr|telegram|run|web|tunnel`:
//! who may control this machine remotely, the Telegram bot and the web panel.
//! Local files only, except `run`, `web` and a cloudflared download.

use super::*;
use crate::bridge::Bridge;
use crate::bridge::web::tunnel;

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
    /// A QR code for some text: terminal blocks, or with --json {"size": N, "rows": ["0101...", ...]}.
    Qr {
        /// The text to encode (a pairing link, say).
        text: String,
    },
    /// The Telegram bot's settings.
    Telegram {
        #[command(subcommand)]
        action: TelegramCmd,
    },
    /// Store the app's saved spaces for the bot's /projects: a JSON file of
    /// [{"name": "...", "folder": "/absolute/path"}] (at most 1000), or --clear.
    Spaces {
        /// Remove the stored spaces instead.
        #[arg(long)]
        clear: bool,
        /// The JSON file to read.
        #[arg(long, value_name = "PATH", required_unless_present = "clear", conflicts_with = "clear")]
        file: Option<std::path::PathBuf>,
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
        /// Also reach it from anywhere through a Cloudflare tunnel (end-to-end encrypted; Cloudflare delivers the page).
        #[arg(long)]
        tunnel: bool,
        #[command(flatten)]
        how: TunnelArgs,
    },
    /// Turn "access from anywhere" (a Cloudflare tunnel) on or off for the running web panel.
    Tunnel {
        #[command(subcommand)]
        action: TunnelCmd,
    },
}

#[derive(clap::Args)]
pub(super) struct TunnelArgs {
    /// Stop the tunnel after this many minutes (1 to 1440).
    #[arg(long = "for", value_name = "MINUTES", default_value_t = tunnel::MINUTES,
          value_parser = clap::value_parser!(u64).range(1..=tunnel::MAX_MINUTES))]
    minutes: u64,
    /// Download the pinned cloudflared without asking, if none is on PATH.
    #[arg(long)]
    yes: bool,
}

#[derive(Subcommand)]
pub(super) enum TunnelCmd {
    /// Start a tunnel to the running panel and print a pairing link through it.
    On {
        #[command(flatten)]
        how: TunnelArgs,
    },
    /// Stop the tunnel.
    Off,
}

#[derive(Subcommand)]
pub(super) enum TelegramCmd {
    /// Store the bot token from @BotFather, read from stdin (not shown as you type)
    /// or from the first line of a file. The file is left for you to delete.
    Token {
        /// Remove the stored token instead.
        #[arg(long)]
        clear: bool,
        /// Read the token from the first line of this file instead of stdin.
        #[arg(long, value_name = "PATH", conflicts_with = "clear")]
        file: Option<std::path::PathBuf>,
    },
}

/// cloudflared: one on PATH, else the one downloaded before, else the pinned
/// release, downloaded once the user says yes (`--yes`, or y at a prompt).
fn cloudflared(yes: bool) -> Result<(), RpcError> {
    let dir = tunnel::install_dir()?;
    if tunnel::find(std::env::var_os("PATH").as_deref(), &dir).is_some() {
        return Ok(());
    }
    let refused = |why: &str| RpcError::new("bridge", why, "install cloudflared on PATH, or pass --yes to download it");
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(refused("cloudflared is not installed"));
        }
        eprint!("{}", tunnel::download_question(&dir));
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).map_err(|e| RpcError::internal(e.to_string()))?;
        if !answer.trim().eq_ignore_ascii_case("y") && !answer.trim().eq_ignore_ascii_case("yes") {
            return Err(refused("cloudflared was not downloaded"));
        }
    }
    eprintln!("downloading cloudflared into {}", dir.display());
    tunnel::download(&dir).map(drop)
}

/// The bot token: the first line of `file`, else typed without echo in a
/// terminal, else the first line piped in.
fn read_token(file: Option<&std::path::Path>) -> Result<String, RpcError> {
    if let Some(file) = file {
        let text = std::fs::read_to_string(file).map_err(|e| {
            RpcError::invalid_params(format!("cannot read the token file {}: {}", file.display(), e.kind()))
        })?;
        // Windows PowerShell 5.1 and Notepad start a UTF-8 file with a BOM.
        return Ok(text.trim_start_matches('\u{feff}').lines().next().unwrap_or_default().to_string());
    }
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

/// The bytes of a spaces file; one bigger than any valid list is refused unread.
fn read_spaces(file: &std::path::Path) -> Result<Vec<u8>, RpcError> {
    const MAX: u64 = 8 << 20;
    let unreadable = |e: std::io::Error| {
        RpcError::invalid_params(format!("cannot read the spaces file {}: {}", file.display(), e.kind()))
    };
    let mut bytes = Vec::new();
    std::fs::File::open(file).and_then(|f| f.take(MAX + 1).read_to_end(&mut bytes)).map_err(unreadable)?;
    if bytes.len() as u64 > MAX {
        return Err(RpcError::invalid_params("the spaces file is too big"));
    }
    Ok(bytes)
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

/// `json`: --json was asked for (not merely piped); `web` then prints one JSON line.
pub(super) fn bridge(paths: &Paths, action: BridgeCmd, json: bool) -> Result<Out, RpcError> {
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
        BridgeCmd::Telegram { action: TelegramCmd::Token { clear: true, .. } } => {
            let human =
                if bridge.clear_telegram_token()? { "telegram token removed" } else { "no telegram token was set" };
            Out { json: json!({"telegram_token": "unset"}), human: human.into() }
        }
        BridgeCmd::Spaces { clear: true, .. } => {
            let human = if bridge.clear_spaces()? { "spaces removed" } else { "no spaces were stored" };
            Out { json: json!({"spaces": 0}), human: human.into() }
        }
        BridgeCmd::Spaces { clear: false, file } => {
            let bytes = read_spaces(&file.unwrap_or_default())?;
            let n = bridge.set_spaces(&bytes)?;
            Out { json: json!({"spaces": n}), human: format!("{n} space(s) stored") }
        }
        BridgeCmd::Qr { text } => {
            Out { json: crate::bridge::web::qr_json(&text)?, human: crate::bridge::web::qr(&text) }
        }
        BridgeCmd::Telegram { action: TelegramCmd::Token { clear: false, file } } => {
            bridge.set_telegram_token(&read_token(file.as_deref())?)?;
            let note =
                if Bridge::secrets_in_file() { " (no usable OS secret store: kept in an owner-only file)" } else { "" };
            Out { json: json!({"telegram_token": "set"}), human: format!("telegram token stored{note}") }
        }
        BridgeCmd::Web { lan, port, via, public_host, remote_port, tunnel, how } => {
            let via = match (via, public_host) {
                (Some(machine), Some(host)) => {
                    Some(crate::bridge::web::vps::plan(paths, &machine, &host, remote_port)?)
                }
                _ => None,
            };
            if tunnel {
                cloudflared(how.yes)?;
            }
            crate::bridge::web::run(paths, lan, port, via, tunnel.then_some(how.minutes), json)?;
            Out { json: Value::Null, human: String::new() }
        }
        BridgeCmd::Tunnel { action: TunnelCmd::On { how } } => {
            cloudflared(how.yes)?;
            let (json, human) = tunnel::ask(&bridge, Some(how.minutes))?;
            Out { json, human }
        }
        BridgeCmd::Tunnel { action: TunnelCmd::Off } => {
            let (json, human) = tunnel::ask(&bridge, None)?;
            Out { json, human }
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

    #[test]
    fn a_token_file_may_start_with_a_utf8_bom() {
        const TOKEN: &str = "123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123";
        let dir = std::env::temp_dir().join(format!("h0x core bom {}", crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.txt");
        std::fs::write(&path, format!("\u{feff}{TOKEN}\r\n")).unwrap();
        assert_eq!(super::read_token(Some(&path)).unwrap(), TOKEN);
        std::fs::write(&path, "\u{feff}").unwrap();
        assert_eq!(super::read_token(Some(&path)).unwrap(), "", "a BOM alone is an empty file");
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn a_token_file_gives_its_first_line_validated_like_stdin_and_names_only_the_file_on_failure() {
        const TOKEN: &str = "123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123";
        let dir = std::env::temp_dir().join(format!("h0x core token {}", crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let bridge = crate::bridge::Bridge::at(dir.join("bridge"), Default::default());
        let path = dir.join("bot token.txt");
        let store = |content: &[u8]| {
            std::fs::write(&path, content).unwrap();
            super::read_token(Some(&path)).and_then(|line| bridge.set_telegram_token(&line))
        };

        for (content, why) in [
            (&b"\r\n"[..], "only a line end"),
            (b"", "an empty file"),
            (b"\n123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123\n", "the token on the second line"),
            (b"123456:short\r\n", "the wrong shape"),
        ] {
            let e = store(content).unwrap_err();
            assert_eq!((e.kind.as_str(), e.message.as_str()), ("invalid_params", "that is not a bot token"), "{why}");
            assert!(!format!("{}{}", e.message, e.hint).contains("ABCdef"), "{why}: the token is never in an error");
        }
        assert_eq!(bridge.telegram_token().unwrap(), None, "nothing stored");

        for content in [
            format!("  {TOKEN}  \r\nsecond line\r\n"),
            format!("\t{TOKEN}\n{TOKEN}x\n"),
            format!("{TOKEN}\r"),
            TOKEN.to_string(),
        ] {
            store(content.as_bytes()).unwrap_or_else(|e| panic!("{content:?}: {}", e.message));
            assert_eq!(bridge.telegram_token().unwrap().as_deref(), Some(TOKEN), "{content:?}");
            assert!(path.exists(), "the file is left for the caller to delete");
        }

        let gone = dir.join("no such dir").join("token.txt");
        let e = super::read_token(Some(&gone)).unwrap_err();
        assert_eq!(e.kind, "invalid_params");
        assert_eq!(
            e.message,
            format!("cannot read the token file {}: {}", gone.display(), std::io::ErrorKind::NotFound)
        );

        let e = super::read_token(Some(&dir)).unwrap_err();
        assert_eq!(e.kind, "invalid_params", "a directory, not a file");
        assert!(e.message.starts_with(&format!("cannot read the token file {}: ", dir.display())), "{}", e.message);

        std::fs::write(&path, [0xff, 0xfe, b'1', b'\n']).unwrap();
        let e = super::read_token(Some(&path)).unwrap_err();
        assert_eq!(
            e.message,
            format!("cannot read the token file {}: {}", path.display(), std::io::ErrorKind::InvalidData),
            "not UTF-8"
        );
        bridge.clear_telegram_token().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_file_takes_a_windows_path_with_spaces_and_conflicts_with_clear() {
        use super::{BridgeCmd, TelegramCmd};
        use clap::Parser;
        let parse = |args: &[&str]| {
            let cli = crate::cli::Cli::try_parse_from([&["h0x-core", "bridge", "telegram", "token"], args].concat());
            match cli.map_err(|e| e.kind())?.cmd {
                Some(crate::cli::Cmd::Bridge {
                    action: BridgeCmd::Telegram { action: TelegramCmd::Token { clear, file } },
                }) => Ok((clear, file)),
                _ => panic!("bridge telegram token"),
            }
        };
        let windows = r"C:\Users\Jane Doe\My Tokens\bot token.txt";
        assert_eq!(parse(&["--file", windows]), Ok((false, Some(std::path::PathBuf::from(windows)))));
        assert_eq!(
            parse(&["--file", r"\\server\share\bot.txt"]).unwrap().1.unwrap().to_str(),
            Some(r"\\server\share\bot.txt")
        );
        assert_eq!(parse(&[]), Ok((false, None)), "stdin, as before");
        assert_eq!(parse(&["--clear"]), Ok((true, None)));
        assert_eq!(parse(&["--clear", "--file", windows]), Err(clap::error::ErrorKind::ArgumentConflict));
        assert_eq!(parse(&["--file", windows, "--clear"]), Err(clap::error::ErrorKind::ArgumentConflict));
        assert_eq!(parse(&["--file"]), Err(clap::error::ErrorKind::InvalidValue), "a path is required");
    }

    #[test]
    fn spaces_take_a_file_or_clear_but_not_both_nor_neither() {
        use super::BridgeCmd;
        use clap::Parser;
        let parse = |args: &[&str]| {
            let cli = crate::cli::Cli::try_parse_from([&["h0x-core", "bridge", "spaces"], args].concat());
            match cli.map_err(|e| e.kind())?.cmd {
                Some(crate::cli::Cmd::Bridge { action: BridgeCmd::Spaces { clear, file } }) => Ok((clear, file)),
                _ => panic!("bridge spaces"),
            }
        };
        let windows = r"C:\Users\Jane Doe\spaces.json";
        assert_eq!(parse(&["--file", windows]), Ok((false, Some(std::path::PathBuf::from(windows)))));
        assert_eq!(parse(&["--clear"]), Ok((true, None)));
        assert_eq!(parse(&[]), Err(clap::error::ErrorKind::MissingRequiredArgument));
        assert_eq!(parse(&["--clear", "--file", windows]), Err(clap::error::ErrorKind::ArgumentConflict));
        assert_eq!(parse(&["--file"]), Err(clap::error::ErrorKind::InvalidValue));
    }

    #[test]
    fn a_spaces_file_is_validated_whole_and_nothing_is_stored_when_it_is_not() {
        let dir = std::env::temp_dir().join(format!("h0x core spaces {}", crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let bridge = crate::bridge::Bridge::at(dir.join("bridge"), Default::default());
        let path = dir.join("spaces list.json");
        let store = |content: &[u8]| {
            std::fs::write(&path, content).unwrap();
            super::read_spaces(&path).and_then(|bytes| bridge.set_spaces(&bytes))
        };
        let entry = |name: &str, folder: &str| serde_json::json!({"name": name, "folder": folder});
        let list = |entries: Vec<serde_json::Value>| serde_json::to_vec(&entries).unwrap();

        // Good lists: empty, Unix, drive, UNC, a BOM in front, the longest name and folder.
        assert_eq!(store(b"[]").unwrap(), 0);
        let good = list(vec![
            entry("Work", "/home/me/work"),
            entry("Win", r"C:\Users\Jane Doe\app"),
            entry("Win2", "D:/code"),
            entry("Share", r"\\server\share\app"),
            entry(&"n".repeat(200), &format!("/{}", "f".repeat(4095))),
        ]);
        assert_eq!(store(&good).unwrap(), 5);
        assert_eq!(bridge.spaces().len(), 5);
        assert_eq!(store(&[&b"\xef\xbb\xbf"[..], b"[]"].concat()).unwrap(), 0, "a BOM from PowerShell or Notepad");
        assert!(bridge.spaces().is_empty());
        assert_eq!(store(&good).unwrap(), 5);

        // Each bad one is invalid_params and leaves the five stored.
        let too_many = list((0..1001).map(|n| entry(&format!("s{n}"), "/x")).collect());
        let exactly = list((0..1000).map(|n| entry(&format!("s{n}"), "/x")).collect());
        let bad: Vec<(Vec<u8>, &str)> = vec![
            (b"".to_vec(), "an empty file"),
            (b"not json".to_vec(), "not JSON"),
            (vec![0xff, 0xfe, b'[', b']'], "not UTF-8"),
            (b"{\"name\":\"a\",\"folder\":\"/x\"}".to_vec(), "an object, not a list"),
            (b"[{\"name\":\"a\"}]".to_vec(), "a missing folder"),
            (b"[{\"name\":1,\"folder\":\"/x\"}]".to_vec(), "a number for a name"),
            (b"[\"/x\"]".to_vec(), "a string for an entry"),
            (too_many, "1001 entries"),
            (list(vec![entry("", "/x")]), "an empty name"),
            (list(vec![entry(&"n".repeat(201), "/x")]), "a name of 201 characters"),
            (list(vec![entry("a\nb", "/x")]), "a line end in the name"),
            (list(vec![entry("a", "relative/path")]), "a relative folder"),
            (list(vec![entry("a", "")]), "an empty folder"),
            (list(vec![entry("a", "C:")]), "a drive without a folder"),
            (list(vec![entry("a", "~/work")]), "a home shorthand"),
            (list(vec![entry("a", &format!("/{}", "f".repeat(4096)))]), "a folder of 4097 characters"),
            (list(vec![entry("a", "/x\u{7}")]), "a control character in the folder"),
            (list(vec![entry("ok", "/x"), entry("bad", "x")]), "one good entry then a bad one"),
        ];
        for (content, why) in &bad {
            let e = store(content).unwrap_err();
            assert_eq!(e.kind, "invalid_params", "{why}");
            assert_eq!(bridge.spaces().len(), 5, "{why}: the stored list is untouched");
        }
        assert_eq!(store(&exactly).unwrap(), 1000, "1000 is the limit");

        // A file that is not there, a folder, and one too big to read.
        let e = super::read_spaces(&dir.join("none.json")).unwrap_err();
        assert_eq!(e.kind, "invalid_params");
        assert!(e.message.starts_with("cannot read the spaces file "), "{}", e.message);
        assert_eq!(super::read_spaces(&dir).unwrap_err().kind, "invalid_params", "a folder");
        std::fs::write(&path, vec![b' '; (8 << 20) + 1]).unwrap();
        assert_eq!(super::read_spaces(&path).unwrap_err().message, "the spaces file is too big");

        assert!(bridge.clear_spaces().unwrap());
        assert!(!bridge.clear_spaces().unwrap(), "nothing left to clear");
        assert!(bridge.spaces().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spaces_keep_duplicates_and_dot_dot_folders_as_given_but_refuse_nul_and_read_exactly_8_mib() {
        let dir = std::env::temp_dir().join(format!("h0x-core-spaces-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let bridge = crate::bridge::Bridge::at(dir.join("bridge"), Default::default());
        assert!(!bridge.clear_spaces().unwrap(), "--clear with nothing stored, not even the folder");
        let entry = |name: &str, folder: &str| serde_json::json!({"name": name, "folder": folder});
        let list = |entries: Vec<serde_json::Value>| serde_json::to_vec(&entries).unwrap();

        // Absolute with `..` inside is still absolute: kept as typed, never resolved.
        let kept = list(vec![
            entry("Dup", "/work"),
            entry("Dup", "/work"),
            entry("Up", "/work/../etc"),
            entry("WinUp", r"C:\Users\..\Windows"),
            entry("UNC", r"\\server\share\..\other"),
            entry("Ünïcode 项目", "/home/jäne/项目"),
        ]);
        assert_eq!(bridge.set_spaces(&kept).unwrap(), 6);
        let stored = bridge.spaces();
        assert_eq!(stored.len(), 6, "duplicates are not merged");
        assert_eq!((stored[2].folder.as_str(), stored[3].folder.as_str()), ("/work/../etc", r"C:\Users\..\Windows"));
        assert_eq!(stored[5].name, "Ünïcode 项目");

        for (bad, why) in [
            (list(vec![entry("a\0b", "/x")]), "a NUL in the name"),
            (list(vec![entry("a", "/x\0/y")]), "a NUL in the folder"),
            (list(vec![entry("a", "/x\u{1b}[31m")]), "an escape in the folder"),
            (list(vec![entry("a\u{7f}", "/x")]), "DEL in the name"),
            (list(vec![entry("a", "../work")]), "a relative folder with .."),
            (list(vec![entry("a", r"\work")]), "one backslash is not UNC"),
            (list(vec![entry("a", "C:work")]), "a drive-relative folder"),
        ] {
            assert_eq!(bridge.set_spaces(&bad).unwrap_err().kind, "invalid_params", "{why}");
            assert_eq!(bridge.spaces().len(), 6, "{why}: the stored list is untouched");
        }
        // A verbatim path starts with two backslashes: taken like UNC.
        assert_eq!(bridge.set_spaces(&list(vec![entry("a", r"\\?\C:\work")])).unwrap(), 1);
        assert_eq!(bridge.set_spaces(&kept).unwrap(), 6);

        // Exactly 8 MiB is read whole (then judged as JSON); one byte more is refused.
        let path = dir.join("big.json");
        std::fs::write(&path, vec![b' '; 8 << 20]).unwrap();
        assert_eq!(super::read_spaces(&path).unwrap().len(), 8 << 20);
        assert_eq!(bridge.set_spaces(&super::read_spaces(&path).unwrap()).unwrap_err().kind, "invalid_params");
        assert_eq!(bridge.spaces().len(), 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_spaces_file_is_read_through_its_link() {
        let dir = std::env::temp_dir().join(format!("h0x-core-spaces-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.json");
        std::fs::write(&real, br#"[{"name":"a","folder":"/x"}]"#).unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(super::read_spaces(&link).unwrap(), std::fs::read(&real).unwrap());
        std::os::unix::fs::symlink(dir.join("gone.json"), dir.join("dangling.json")).unwrap();
        let e = super::read_spaces(&dir.join("dangling.json")).unwrap_err();
        assert!(e.message.starts_with("cannot read the spaces file "), "{}", e.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tunnel_minutes_are_one_to_a_day_and_default_to_an_hour() {
        use super::{BridgeCmd, TunnelCmd};
        use clap::Parser;
        let parse = |args: &[&str]| {
            let cli = crate::cli::Cli::try_parse_from([&["h0x-core", "bridge"], args].concat()).ok()?;
            match cli.cmd? {
                crate::cli::Cmd::Bridge { action: BridgeCmd::Web { tunnel: true, how, .. } }
                | crate::cli::Cmd::Bridge { action: BridgeCmd::Tunnel { action: TunnelCmd::On { how } } } => {
                    Some((how.minutes, how.yes))
                }
                _ => None,
            }
        };
        for on in [&["web", "--tunnel"][..], &["tunnel", "on"][..]] {
            let with = |extra: &[&str]| parse(&[on, extra].concat());
            assert_eq!(with(&[]), Some((60, false)), "{on:?}: an hour by default");
            assert_eq!(with(&["--for", "1"]), Some((1, false)));
            assert_eq!(with(&["--for", "1440", "--yes"]), Some((1440, true)));
            for bad in ["0", "1441", "-1", "", "1.5", "60m", "18446744073709551616"] {
                assert_eq!(with(&["--for", bad]), None, "{on:?} --for {bad:?}");
            }
        }
        assert!(crate::cli::Cli::try_parse_from(["h0x-core", "bridge", "tunnel", "off", "--for", "5"]).is_err());
        assert!(crate::cli::Cli::try_parse_from(["h0x-core", "bridge", "tunnel"]).is_err(), "on or off");
    }
}
