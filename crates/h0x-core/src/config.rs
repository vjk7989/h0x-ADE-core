//! `~/.config/h0x-core/config.toml`. A missing file means defaults.

use crate::rpc::RpcError;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Default)]
pub struct Config {
    /// Executable and arguments for new shells (default: this machine's shell).
    pub shell: Option<Vec<String>>,
    /// The chord that detaches `h0x-core attach`: `ctrl+<key>`, default `ctrl+\`.
    pub detach_key: Option<String>,
    /// Seconds a `wait` runs before it checks in (default 180).
    pub wait_timeout: Option<f64>,
    /// Notes to parent agents and `<h0x>` tags on messages between agents, on
    /// or off (default on). Off: h0x-core types nothing on its own.
    pub notes: Option<bool>,
    /// Seconds of quiet before a note (default 3); after Enter or a clear it goes in at once.
    pub notes_quiet: Option<f64>,
    /// Size of a terminal nobody is typing in, `COLSxROWS` (default 160x50).
    pub headless: Option<String>,
    /// Scrollback kept per live terminal, in MB (default 3.6).
    pub history_mb: Option<f64>,
    /// Keep the machine from sleeping while a terminal's status is `working` (default off).
    /// The display and the screen lock are not affected.
    /// A stuck `working` agent keeps the machine awake until it reports `done` or this is
    /// turned off: there is no time limit.
    pub keep_awake: Option<bool>,
    /// Where the core listens (default under $XDG_RUNTIME_DIR). `H0X_CORE_SOCKET` wins.
    pub socket: Option<String>,
    /// Seconds another machine gets to answer a call (a wait gets its own
    /// timeout on top), and to send anything at all before its line is
    /// given up and dialed again (default 30).
    pub machine_timeout: Option<f64>,
    /// Remote control limits (`h0x-core bridge`).
    #[serde(default)]
    pub bridge: BridgeConfig,
}

/// `[bridge]`: limits on what paired chat accounts may do.
#[derive(Deserialize, Default)]
pub struct BridgeConfig {
    /// Commands one paired account may send per minute (default 30).
    pub rate_limit: Option<u32>,
    /// Seconds between two notices about one terminal (default 10).
    pub notice_quiet: Option<f64>,
}

impl BridgeConfig {
    pub fn rate_limit(&self) -> u32 {
        self.rate_limit.unwrap_or(30)
    }

    pub fn notice_quiet_ms(&self) -> u64 {
        // At most an hour: a longer quiet period would hide that it needs you.
        (self.notice_quiet.filter(|s| s.is_finite() && *s >= 0.0).unwrap_or(10.0).min(3600.0) * 1000.0) as u64
    }
}

impl Config {
    pub fn shell(&self) -> Result<Vec<String>, RpcError> {
        match &self.shell {
            None => Ok(crate::platform::own_shell()),
            Some(argv) if argv.first().is_some_and(|exe| !exe.is_empty()) => Ok(argv.clone()),
            Some(_) => Err(invalid("shell: give an executable and optional arguments, like [\"nu\"]".into())),
        }
    }

    pub fn headless(&self) -> Result<(u16, u16), RpcError> {
        let Some(spec) = &self.headless else { return Ok((160, 50)) };
        spec.split_once('x')
            .and_then(|(c, r)| Some((c.trim().parse().ok()?, r.trim().parse().ok()?)))
            .filter(|(c, r): &(u16, u16)| *c > 0 && *r > 0)
            .ok_or_else(|| invalid(format!("headless {spec:?}: use COLSxROWS, like \"160x50\"")))
    }

    pub fn machine_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.machine_timeout.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(30.0))
    }

    pub fn history_bytes(&self) -> usize {
        (self.history_mb.unwrap_or(3.6).max(0.0) * 1024.0 * 1024.0) as usize
    }
}

fn invalid(message: String) -> RpcError {
    RpcError::new("config", message, "fix ~/.config/h0x-core/config.toml")
}

pub fn load(path: &Path) -> Result<Config, RpcError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| invalid(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(invalid(format!("{}: {e}", path.display()))),
    }
}

/// A detach chord: the control byte it sends, and its key code for the
/// kitty and modifyOtherKeys encodings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetachKey {
    pub byte: u8,
    pub code: u32,
}

impl Config {
    pub fn detach_key(&self) -> Result<DetachKey, RpcError> {
        let spec = self.detach_key.as_deref().unwrap_or("ctrl+\\");
        let key = spec.strip_prefix("ctrl+").and_then(|k| {
            let mut chars = k.chars();
            let c = chars.next()?.to_ascii_lowercase();
            (chars.next().is_none() && (c.is_ascii_lowercase() || "@[\\]^_".contains(c))).then_some(c)
        });
        let c = key.ok_or_else(|| {
            invalid(format!("detach_key {spec:?}: use a ctrl chord like \"ctrl+\\\\\" or \"ctrl+]\""))
        })?;
        Ok(DetachKey { byte: c as u8 & 0x1f, code: c as u32 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &str) -> Result<DetachKey, RpcError> {
        Config { detach_key: Some(spec.into()), ..Default::default() }.detach_key()
    }

    #[test]
    fn detach_keys_are_ctrl_chords_only() {
        assert_eq!(Config::default().detach_key().unwrap(), DetachKey { byte: 0x1c, code: 92 });
        assert_eq!(key("ctrl+]").unwrap(), DetachKey { byte: 0x1d, code: 93 });
        assert_eq!(key("ctrl+A").unwrap(), DetachKey { byte: 0x01, code: 97 });
        for bad in ["alt+x", "ctrl+", "ctrl+ab", "ctrl+1", "x"] {
            assert_eq!(key(bad).unwrap_err().kind, "config", "{bad}");
        }
    }

    fn with(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    fn headless(spec: &str) -> Result<(u16, u16), RpcError> {
        Config { headless: Some(spec.into()), ..Default::default() }.headless()
    }

    fn history(mb: f64) -> usize {
        Config { history_mb: Some(mb), ..Default::default() }.history_bytes()
    }

    #[test]
    fn without_settings_a_terminal_keeps_3_6_mb_and_starts_at_160x50() {
        // 3.6 * 1024 * 1024 = 3_774_873.6, truncated.
        for config in [Config::default(), with(""), with("notes = true\n")] {
            assert_eq!(config.history_bytes(), 3_774_873);
            assert_eq!(config.history_bytes(), (3.6 * 1024.0 * 1024.0) as usize);
            assert_eq!(config.headless().unwrap(), (160, 50));
        }
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-dir").join("config.toml");
        let config = load(&missing).unwrap();
        assert_eq!((config.history_bytes(), config.headless().unwrap()), (3_774_873, (160, 50)));
    }

    #[test]
    fn an_explicit_history_mb_wins_and_negatives_are_zero() {
        assert_eq!(history(0.0), 0);
        assert_eq!(history(-0.0), 0);
        assert_eq!(history(-1.0), 0);
        assert_eq!(history(-1e300), 0);
        assert_eq!(history(f64::NEG_INFINITY), 0);
        assert_eq!(history(f64::NAN), 0, "NaN.max(0.0) is 0");
        assert_eq!(history(0.05), 52_428, "0.05 MB = 52_428.8 bytes, truncated");
        assert_eq!(history(1.0 / 1024.0 / 1024.0), 1, "one byte");
        assert_eq!(history(0.5 / 1024.0 / 1024.0), 0, "half a byte truncates to none");
        assert_eq!(history(3.6), 3_774_873, "explicit default equals the default");
        assert_eq!(history(5.0), 5 * 1024 * 1024, "the old default is still reachable");
        assert_eq!(history(1024.0), 1024 * 1024 * 1024);
        assert_eq!(history(1e300), usize::MAX, "huge values saturate");
        assert_eq!(history(f64::INFINITY), usize::MAX);
        // Through the file format too.
        assert_eq!(with("history_mb = 0.0").history_bytes(), 0);
        assert_eq!(with("history_mb = -2.5").history_bytes(), 0);
        assert_eq!(with("history_mb = 0.05").history_bytes(), 52_428);
        assert_eq!(with("history_mb = 200.0").history_bytes(), 200 * 1024 * 1024);
        assert!(toml::from_str::<Config>("history_mb = \"3.6\"").is_err(), "a string is not a size");
    }

    #[test]
    fn an_explicit_headless_size_wins() {
        assert_eq!(headless("200x120").unwrap(), (200, 120), "the old default is still reachable");
        assert_eq!(headless("160x50").unwrap(), (160, 50));
        assert_eq!(headless("80x24").unwrap(), (80, 24));
        assert_eq!(headless("1x1").unwrap(), (1, 1), "smallest size");
        assert_eq!(headless("65535x65535").unwrap(), (65535, 65535), "largest size");
        assert_eq!(headless(" 160 x 50 ").unwrap(), (160, 50), "spaces around the numbers are trimmed");
        assert_eq!(with("headless = \"200x120\"").headless().unwrap(), (200, 120));
    }

    #[test]
    fn a_malformed_headless_size_is_a_config_error_that_suggests_160x50() {
        for bad in [
            "",
            " ",
            "x",
            "160x",
            "x50",
            "160",
            "0x0",
            "0x50",
            "160x0",
            "160X50",
            "160 50",
            "160*50",
            "16 0x50",
            "70000x50",
            "160x70000",
            "65536x1",
            "-1x50",
            "160x-50",
            "1.5x2",
            "160x50x2",
            "wide",
            "160x50 rows",
        ] {
            let err = headless(bad).unwrap_err();
            assert_eq!(err.kind, "config", "{bad:?}");
            assert!(
                err.message.contains("like \"160x50\""),
                "{bad:?}: the example names the new default: {}",
                err.message
            );
            assert!(!err.message.contains("200x120"), "{bad:?}: the old default is gone: {}", err.message);
        }
    }

    #[test]
    fn notice_quiet_is_capped_at_an_hour() {
        let quiet = |secs: f64| BridgeConfig { notice_quiet: Some(secs), ..Default::default() }.notice_quiet_ms();
        assert_eq!((quiet(3600.0), quiet(3601.0), quiet(1e12)), (3_600_000, 3_600_000, 3_600_000));
    }

    #[test]
    fn bridge_limits_fall_back_on_odd_values() {
        let bridge = |text: &str| toml::from_str::<Config>(text).map(|c| c.bridge);
        let quiet = |value: &str| bridge(&format!("[bridge]\nnotice_quiet = {value}")).unwrap().notice_quiet_ms();
        assert_eq!(bridge("").unwrap().notice_quiet_ms(), 10_000);
        assert_eq!(bridge("").unwrap().rate_limit(), 30);
        for (value, ms) in
            [("0.0", 0), ("0.0004", 0), ("-1.0", 10_000), ("nan", 10_000), ("inf", 10_000), ("-inf", 10_000)]
        {
            assert_eq!(quiet(value), ms, "{value}");
        }
        for bad in ["-1", "4294967296", "\"30\""] {
            assert!(bridge(&format!("[bridge]\nrate_limit = {bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn keep_awake_is_off_unless_a_readable_config_says_true() {
        let keep_awake = |text: &str| toml::from_str::<Config>(text).map(|c| c.keep_awake);
        assert_eq!(keep_awake("").unwrap(), None, "absent means the default, off");
        assert_eq!(keep_awake("keep_awake = true").unwrap(), Some(true));
        assert_eq!(keep_awake("keep_awake = false").unwrap(), Some(false));
        // Not a boolean: the whole file is rejected, so the watcher sees no `true`.
        for bad in ["\"true\"", "\"yes\"", "1", "0", "[]", "\"\""] {
            assert!(keep_awake(&format!("keep_awake = {bad}")).is_err(), "{bad}");
        }
        // A missing file is the defaults (off); a path that cannot be read as a file is an error.
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-dir").join("config.toml");
        assert_eq!(load(&missing).unwrap().keep_awake, None);
        assert!(load(Path::new(env!("CARGO_MANIFEST_DIR"))).is_err(), "a directory is not a config file");
    }
}
