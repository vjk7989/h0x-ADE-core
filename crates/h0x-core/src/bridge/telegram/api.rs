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
        curl.args(["-sS", "--proto", proto, "--proto-redir", proto, "--connect-timeout", "10", "--max-time"])
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
        self.answer(method, body, seconds).map_err(|e| scrub(&format!("{method}: {e}")))
    }

    fn answer(&self, method: &str, body: &Value, seconds: u64) -> Result<Value, String> {
        let (mut curl, config) = self.request(method, body, seconds);
        let mut child = curl
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("can't run curl: {e}"))?;
        let fed = child.stdin.take().expect("piped").write_all(config.as_bytes());
        let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
        if fed.is_err() || !out.status.success() {
            return Err(failure(out.status.code(), &String::from_utf8_lossy(&out.stderr)));
        }
        let reply: Value = serde_json::from_slice(&out.stdout).map_err(|_| "not a Bot API answer".to_string())?;
        match reply["ok"].as_bool() {
            Some(true) => Ok(reply["result"].clone()),
            _ => Err(reply["description"].as_str().unwrap_or("refused").into()),
        }
    }
}

/// Why curl failed: its exit code and its own message (which may name the URL;
/// [`Api::call`] scrubs the token out of every error).
fn failure(code: Option<i32>, stderr: &str) -> String {
    let said = stderr.trim();
    let code = code.unwrap_or(-1);
    if said.is_empty() {
        format!("no answer (curl exit {code})")
    } else {
        format!("no answer (curl exit {code}: {said})")
    }
}

/// `text` with every `/bot<id>:<secret>` (a Bot API URL's token) replaced.
pub(super) fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("/bot") {
        out.push_str(&rest[..at + 4]);
        let after = &rest[at + 4..];
        let id = after.bytes().take_while(u8::is_ascii_digit).count();
        let secret = match after.as_bytes().get(id) {
            Some(b':') if id > 0 => {
                after[id + 1..].bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-').count()
            }
            _ => 0,
        };
        if secret > 0 {
            out.push_str("[token]");
            rest = &after[id + 1 + secret..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
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
        assert_eq!(args[..5], ["-sS", "--proto", "=https", "--proto-redir", "=https"], "https only to Telegram");
        assert!(args.windows(2).any(|w| w == ["--max-time", "65"]));
    }

    #[test]
    fn a_curl_failure_naming_the_url_never_carries_the_token() {
        let stderr = format!(
            "curl: (28) Operation timed out after 65001 milliseconds with 0 bytes received\n\
             curl: (6) Could not resolve host for https://api.telegram.org/bot{TOKEN}/getUpdates?x=1\n"
        );
        let err = scrub(&format!("getUpdates: {}", failure(Some(28), &stderr)));
        assert!(!err.contains(TOKEN) && !err.contains("ABCdef") && !err.contains("123456"), "{err}");
        assert!(
            err.contains("https://api.telegram.org/bot[token]/getUpdates?x=1") && err.contains("curl exit 28"),
            "{err}"
        );
        assert_eq!(failure(None, "  "), "no answer (curl exit -1)");
        for (text, want) in [
            (format!("a /bot{TOKEN}/x and /bot{TOKEN}"), "a /bot[token]/x and /bot[token]".to_string()),
            (
                "the /bot is fine, /bot42 too, /bot:x, /bot42:".into(),
                "the /bot is fine, /bot42 too, /bot:x, /bot42:".into(),
            ),
            ("é/bot1:a/".into(), "é/bot[token]/".into()),
            (String::new(), String::new()),
        ] {
            assert_eq!(scrub(&text), want, "{text}");
        }
    }

    #[test]
    fn a_refusal_that_echoes_the_url_is_scrubbed_too() {
        // A real call: nothing listens, and curl's message reaches the error scrubbed.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let err = Api::at(&format!("http://{closed}"), TOKEN).call("getMe", &json!({}), 5).unwrap_err();
        assert!(err.starts_with("getMe: no answer (curl exit ") && !err.contains(TOKEN), "{err}");
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

    #[test]
    fn look_alike_hosts_never_pass_as_loopback() {
        for url in [
            "https://localhost.evil.com",
            "http://127.0.0.1.nip.io",
            "http://127.0.0.1.nip.io:8080/x",
            "http://127.0.0.1:80@evil.com",
            "http://localhost:8080.evil.com",
            "http://evil.com/127.0.0.1",
            "http://evil.com?host=localhost",
            "http://evil.com#@127.0.0.1",
            "http://127.0.0.2",
            "http://0.0.0.0:80",
            "http://[::ffff:127.0.0.1]",
            "http://127.0.0.1:",
            "HTTP://127.0.0.1",
            r"\\127.0.0.1\share",
            "file:///127.0.0.1",
        ] {
            assert!(!loopback(url), "{url}");
        }
        for url in ["http://127.0.0.1", "http://localhost/", "http://127.0.0.1:65535?x#y", "http://[::1]"] {
            assert!(loopback(url), "{url}");
        }
    }

    #[test]
    fn scrub_stops_at_the_secret_and_keeps_what_follows() {
        for (text, want) in [
            (format!("x/bot{TOKEN}"), "x/bot[token]".to_string()),
            (format!("/bot{TOKEN}?offset=3 /bot{TOKEN}#f"), "/bot[token]?offset=3 /bot[token]#f".into()),
            (format!("//bot/bot{TOKEN}\n"), "//bot/bot[token]\n".into()),
            (format!("/bot/bot{TOKEN}"), "/bot/bot[token]".into()),
            ("/bot😀:x /bot1:😀".into(), "/bot😀:x /bot1:😀".into()),
            ("/bot".into(), "/bot".into()),
        ] {
            assert_eq!(scrub(&text), want, "{text:?}");
        }
    }
}
