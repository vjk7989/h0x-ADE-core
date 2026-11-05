//! The Telegram Bot API through the system curl (ADR-050): https only, to
//! api.telegram.org. The bot token is in the URL, so the whole request goes
//! to curl as a config on stdin (`-K -`), never as an argument another
//! process could list. Bodies stay off the command line too.

use crate::rpc::RpcError;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

const HOST: &str = "https://api.telegram.org";

#[derive(Clone)]
pub struct Api {
    base: String,
    token: String,
}

/// `H0X_CORE_TELEGRAM_URL` (a local stub): debug and test builds only.
#[cfg(any(debug_assertions, test))]
fn overridden() -> Option<String> {
    std::env::var("H0X_CORE_TELEGRAM_URL").ok().filter(|v| !v.trim().is_empty())
}

#[cfg(not(any(debug_assertions, test)))]
fn overridden() -> Option<String> {
    None
}

/// A stand-in for the Bot API must be on this machine.
fn loopback(url: &str) -> bool {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"));
    let host = rest.and_then(|r| r.split(['/', '?', '#']).next()).unwrap_or_default();
    let host = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// Text for a curl config file, inside double quotes.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

impl Api {
    /// The Bot API for `token`: api.telegram.org, or in debug and test
    /// builds a loopback stub named by H0X_CORE_TELEGRAM_URL.
    pub fn new(token: String) -> Result<Api, RpcError> {
        match overridden() {
            Some(url) if loopback(&url) => Ok(Api { base: url.trim_end_matches('/').into(), token }),
            Some(url) => Err(RpcError::new(
                "refused",
                format!("H0X_CORE_TELEGRAM_URL {url:?} is not on this machine"),
                "use a loopback stub, like http://127.0.0.1:8080",
            )),
            None => Ok(Api { base: HOST.into(), token }),
        }
    }

    #[cfg(test)]
    pub(super) fn at(base: &str, token: &str) -> Api {
        Api { base: base.into(), token: token.into() }
    }

    /// The curl process for one call and the config it reads on stdin.
    fn request(&self, method: &str, body: &Value, seconds: u64) -> (Command, String) {
        // The real API is https only; a loopback stub (tests) may be http.
        let proto = if self.base == HOST { "=https" } else { "=http,https" };
        let mut curl = Command::new("curl");
        curl.args(["-s", "--proto", proto, "--proto-redir", proto, "--connect-timeout", "10", "--max-time"])
            .arg(seconds.to_string())
            .args(["-K", "-"]);
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        let config = format!(
            "url = \"{}\"\nheader = \"Content-Type: application/json\"\nheader = \"Expect:\"\ndata-binary = \"{}\"\n",
            quote(&url),
            quote(&body.to_string())
        );
        (curl, config)
    }

    /// One Bot API call: its `result`, or why not (never the token or the payload).
    pub fn call(&self, method: &str, body: &Value, seconds: u64) -> Result<Value, String> {
        let (mut curl, config) = self.request(method, body, seconds);
        let mut child = curl
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{method}: can't run curl: {e}"))?;
        let fed = child.stdin.take().expect("piped").write_all(config.as_bytes());
        let out = child.wait_with_output().map_err(|e| format!("{method}: curl: {e}"))?;
        if fed.is_err() || !out.status.success() {
            return Err(format!("{method}: no answer (curl exit {})", out.status.code().unwrap_or(-1)));
        }
        let reply: Value =
            serde_json::from_slice(&out.stdout).map_err(|_| format!("{method}: not a Bot API answer"))?;
        match reply["ok"].as_bool() {
            Some(true) => Ok(reply["result"].clone()),
            _ => Err(format!("{method}: {}", reply["description"].as_str().unwrap_or("refused"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TOKEN: &str = "123456:ABCdef_ghi-JKLmnoPQRstuVWXyz0123";

    #[test]
    fn the_token_and_the_body_never_reach_curls_arguments() {
        let body = json!({"chat_id": 42, "text": "say \"hi\" \\ then\nleave"});
        for base in [HOST, "http://127.0.0.1:9"] {
            let (curl, config) = Api::at(base, TOKEN).request("sendMessage", &body, 30);
            let args: Vec<String> = curl.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
            assert!(!args.iter().any(|a| a.contains(TOKEN) || a.contains("123456") || a.contains("hi")), "{args:?}");
            assert_eq!(args[args.len() - 2..], ["-K", "-"], "the request comes on stdin");
            assert!(config.contains(&format!("url = \"{base}/bot{TOKEN}/sendMessage\"\n")), "{config}");
            let data = config.lines().find_map(|l| l.strip_prefix("data-binary = ")).unwrap();
            assert_eq!(data, r#""{\"chat_id\":42,\"text\":\"say \\\"hi\\\" \\\\ then\\nleave\"}""#);
        }
        let (curl, _) = Api::at(HOST, TOKEN).request("getUpdates", &json!({}), 65);
        let args: Vec<String> = curl.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..5], ["-s", "--proto", "=https", "--proto-redir", "=https"], "https only to Telegram");
        assert!(args.windows(2).any(|w| w == ["--max-time", "65"]));
    }

    #[test]
    fn only_a_loopback_stub_may_stand_in() {
        for url in ["http://127.0.0.1:8080", "http://localhost:1/x", "http://[::1]:9", "https://127.0.0.1"] {
            assert!(loopback(url), "{url}");
        }
        for url in [
            "http://10.0.0.1:80",
            "https://api.telegram.org",
            "http://127.0.0.1.evil.com",
            "http://localhost@evil.com",
            "ftp://127.0.0.1",
            "127.0.0.1:80",
            r"C:\stub",
            "",
        ] {
            assert!(!loopback(url), "{url}");
        }
    }
}
