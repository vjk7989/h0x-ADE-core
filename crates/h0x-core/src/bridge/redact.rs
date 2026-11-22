//! Secrets out of text that leaves this machine: common token shapes become
//! `[redacted]`. A pattern list, not a guarantee (see docs/bridge.md); a
//! secret split across lines is not recognized.
//!
//! One pass, left to right: after a cut the scan resumes at its end, and a
//! pattern that fails looks only a bounded distance ahead, so the time is
//! linear in the text, whatever it repeats.
//!
//! Known limit: a secret wrapped across a line break is only cut where it is
//! recognized on each line; the part after the break may stay.

use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MARK: &str = "[redacted]";
/// How far a key name, the gap after `Bearer`, or a key block's header line may reach.
const NAME: usize = 64;
const GAP: usize = 8;
const HEADER: usize = 80;

/// The longest text the `text.redact` method takes, in bytes.
pub const MAX_RPC_TEXT: usize = 64 * 1024;

#[derive(Serialize, Deserialize, JsonSchema, Default, Debug)]
#[serde(default)]
pub struct RedactParams {
    /// The text to clean, at most 64 KB.
    pub text: String,
}

#[derive(Serialize, Deserialize, JsonSchema, PartialEq, Debug)]
pub struct RedactResult {
    /// The text with secrets replaced by `[redacted]`.
    pub text: String,
}

/// `redact` for the `text.redact` method: refuses more than 64 KB, never returns part of it.
pub fn redact_rpc(params: &RedactParams) -> Result<RedactResult, RpcError> {
    if params.text.len() > MAX_RPC_TEXT {
        return Err(RpcError::new(
            "too_large",
            format!("text is {} bytes, the limit is {MAX_RPC_TEXT}", params.text.len()),
            "send the text in pieces of at most 64 KB",
        ));
    }
    Ok(RedactResult { text: redact(&params.text) })
}

/// `text` with API keys, tokens, private keys and `password=...` values replaced.
pub fn redact(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut at) = (0, 0);
    while i < b.len() {
        let word_start = i == 0 || !is_word(b[i - 1]);
        let cut = (if word_start { prefixed(b, i).map(|end| (i, end)).or_else(|| bearer(text, i)) } else { None })
            .or_else(|| if matches!(b[i], b'=' | b':') { assigned(b, i) } else { None })
            .or_else(|| pem(b, i));
        match cut {
            Some((start, end)) => {
                out.push_str(&text[at..start]);
                out.push_str(MARK);
                (at, i) = (end, end);
            }
            None => i += 1,
        }
    }
    out.push_str(&text[at..]);
    out
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Characters a token, key or base64/hex run is made of.
fn is_secret(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"_-.~+/=".contains(&c)
}

/// A token with a known prefix (sk-, ghp_, xoxb-, AKIA...) starting at `i`: where it ends.
fn prefixed(b: &[u8], i: usize) -> Option<usize> {
    let rest = &b[i..];
    let dashed = |c: u8| is_word(c) || c == b'-';
    let upper = |c: u8| c.is_ascii_uppercase() || c.is_ascii_digit();
    if rest.starts_with(b"eyJ") {
        return jwt(rest).map(|n| i + n);
    }
    let github = [b"ghp_".as_slice(), b"gho_", b"ghu_", b"ghs_", b"ghr_", b"github_pat_"];
    let stripe = [b"sk_live_".as_slice(), b"sk_test_", b"rk_live_", b"rk_test_"];
    let (skip, min, ok): (usize, usize, &dyn Fn(u8) -> bool) = if rest.starts_with(b"sk-") {
        (3, 16, &dashed)
    } else if let Some(p) = github.iter().find(|p| rest.starts_with(p)) {
        (p.len(), 16, &is_word)
    } else if rest.starts_with(b"xox")
        && rest.get(3).is_some_and(|c| b"abposr".contains(c))
        && rest.get(4) == Some(&b'-')
    {
        (5, 10, &dashed)
    } else if rest.starts_with(b"AKIA") {
        (4, 16, &upper)
    } else if rest.starts_with(b"AIza") {
        (4, 35, &dashed)
    } else if let Some(p) = stripe.iter().find(|p| rest.starts_with(p)) {
        (p.len(), 8, &is_word)
    } else {
        return None;
    };
    // A run shorter than `min` fails within `min` bytes; a longer one is cut and skipped.
    let run = rest[skip..].iter().take_while(|c| ok(**c)).count();
    (run >= min).then_some(i + skip + run)
}

/// A bare JWT at the start of `rest` (`eyJ...` header, `eyJ...` payload, signature): its length.
fn jwt(rest: &[u8]) -> Option<usize> {
    let part = |c: &u8| is_word(*c) || *c == b'-';
    let mut at = 0;
    for (n, head) in [b"eyJ".as_slice(), b"eyJ", b""].into_iter().enumerate() {
        if !rest[at..].starts_with(head) {
            return None;
        }
        at += head.len();
        let run = rest[at..].iter().take_while(|c| part(c)).count();
        if run < 8 {
            return None;
        }
        at += run;
        if n < 2 {
            if rest.get(at) != Some(&b'.') {
                return None;
            }
            at += 1;
        }
    }
    Some(at)
}

/// `Bearer <token>` at `i`: the token (the word stays readable). Any
/// whitespace may separate them, a no-break space too.
fn bearer(text: &str, i: usize) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    if !b.get(i..i + 6)?.eq_ignore_ascii_case(b"bearer") {
        return None;
    }
    // `i + 6` follows six ASCII bytes: a character boundary.
    let gap: usize = text[i + 6..].chars().take(GAP).take_while(|c| c.is_whitespace()).map(char::len_utf8).sum();
    let start = i + 6 + gap;
    let run = &b[start..][..b[start..].iter().take_while(|c| is_secret(**c)).count()];
    // Prose ("Bearer authentication") has no digits; tokens do, or are long.
    let token = run.len() >= 8 && (run.iter().any(u8::is_ascii_digit) || run.len() >= 20);
    (gap > 0 && token).then_some((start, start + run.len()))
}

/// The value after `api_key=`, `"token": "..."`, `PASSWORD = ...` and the like,
/// at the `=` or `:` at `i`: all of it, up to whitespace, a closing quote
/// (escaped or not), `,`, `;` or `}`. Spaces and tabs may surround the separator.
fn assigned(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let blank = |c: &&u8| matches!(c, b' ' | b'\t');
    let mut end = i - b[i.saturating_sub(GAP)..i].iter().rev().take_while(blank).count();
    if end > 0 && matches!(b[end - 1], b'"' | b'\'') {
        end -= 1;
        if end > 0 && b[end - 1] == b'\\' {
            end -= 1;
        }
    }
    let from = end.saturating_sub(NAME);
    let start = b[from..end].iter().rposition(|c| !(is_word(*c) || *c == b'-')).map_or(from, |p| from + p + 1);
    let name = b[start..end].to_ascii_lowercase();
    if !["token", "secret", "password", "key"].iter().any(|k| name.ends_with(k.as_bytes())) {
        return None;
    }
    // A quote, or a backslash that escapes one (the text is JSON inside JSON).
    let quote = |k: usize| match b[k] {
        b'"' | b'\'' => 1,
        b'\\' if matches!(b.get(k + 1), Some(b'"' | b'\'')) => 2,
        _ => 0,
    };
    let mut at = i + 1;
    while at < b.len() && at < i + 1 + GAP {
        let step = if matches!(b[at], b' ' | b'\t') { 1 } else { quote(at) };
        if step == 0 {
            break;
        }
        at += step;
    }
    let ends = |k: usize| b[k].is_ascii_whitespace() || b",;}".contains(&b[k]) || quote(k) > 0;
    let run = (at..b.len()).take_while(|k| !ends(*k)).count();
    (run > 0).then_some((at, at + run))
}

/// A `-----BEGIN ... KEY-----` block at `i`, through its END line (or the
/// end of the text: everything after it is cut, so it is searched once).
fn pem(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if !b[i..].starts_with(b"-----BEGIN ") {
        return None;
    }
    let header = &b[i..b.len().min(i + HEADER)];
    let line_end = i + header.iter().position(|c| *c == b'\n').unwrap_or(header.len());
    find(&b[i..line_end], b"KEY-----")?;
    let Some(p) = find(&b[line_end..], b"-----END ") else { return Some((i, b.len())) };
    let after = line_end + p + 9;
    let tail = &b[after..b.len().min(after + HEADER)];
    let end = find(tail, b"-----").map(|q| after + q + 5);
    let end = end.unwrap_or_else(|| after + tail.iter().position(|c| *c == b'\n').unwrap_or(tail.len()));
    Some((i, end))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RPC_TEXT, RedactParams, redact, redact_rpc};

    fn rpc(text: String) -> Result<String, String> {
        redact_rpc(&RedactParams { text }).map(|r| r.text).map_err(|e| e.kind)
    }

    #[test]
    fn the_rpc_redacts_and_keeps_empty_empty() {
        assert_eq!(rpc(String::new()), Ok(String::new()));
        assert_eq!(rpc("key ghp_ABCDEFGHIJKLMNOPQRSTuvwx ✓ naïve".into()), Ok("key [redacted] ✓ naïve".into()));
    }

    #[test]
    fn the_rpc_takes_exactly_64_kb_and_refuses_more() {
        assert_eq!(rpc("a".repeat(MAX_RPC_TEXT)).map(|t| t.len()), Ok(MAX_RPC_TEXT));
        assert_eq!(rpc("a".repeat(MAX_RPC_TEXT + 1)), Err("too_large".to_string()));
        // The limit is in bytes: 3-byte characters count as 3.
        assert_eq!(rpc("✓".repeat(MAX_RPC_TEXT / 3 + 1)), Err("too_large".to_string()));
    }

    #[test]
    fn the_schema_has_text_redact_with_an_optional_text_and_too_large() {
        let schema = crate::api::openrpc();
        let methods = schema["methods"].as_array().unwrap();
        let method = methods.iter().find(|m| m["name"] == "text.redact").expect("text.redact in the schema");
        let params = method["params"].as_array().unwrap();
        let names: Vec<&str> = params.iter().filter_map(|p| p["name"].as_str()).collect();
        assert_eq!(names, ["text"], "{params:?}");
        assert_ne!(params[0]["required"], true, "a missing text is an empty one");
        assert_eq!(params[0]["schema"]["type"], "string");
        let errors: Vec<&str> =
            method["errors"].as_array().unwrap().iter().filter_map(|e| e["data"]["kind"].as_str()).collect();
        assert_eq!(errors, ["too_large"]);
        let reference = method["result"]["schema"]["$ref"].as_str().unwrap();
        let def = reference.strip_prefix("#/components/schemas/").unwrap();
        let result = &schema["components"]["schemas"][def];
        let keys: Vec<&String> = result["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["text"]);
        assert_eq!(result["required"], serde_json::json!(["text"]));
    }

    #[test]
    fn common_token_shapes_are_redacted() {
        let cases = [
            ("key sk-proj-abcdefghijklmnop1234 here", "key [redacted] here"),
            ("ghp_ABCDEFGHIJKLMNOPQRSTuvwx", "[redacted]"),
            ("github_pat_11ABCDEFG0123456789_abc", "[redacted]"),
            ("slack xoxb-1234567890-abcdef", "slack [redacted]"),
            ("AKIAIOSFODNN7EXAMPLE.", "[redacted]."),
            ("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.e30.x", "Authorization: Bearer [redacted]"),
            ("export OPENAI_API_KEY=abcd1234efgh5678", "export OPENAI_API_KEY=[redacted]"),
            (r#"{"auth_token": "0123456789abcdef"}"#, r#"{"auth_token": "[redacted]"}"#),
            ("PASSWORD='hunter22hunter'", "PASSWORD='[redacted]'"),
            ("client_secret: c2VjcmV0c2VjcmV0", "client_secret: [redacted]"),
        ];
        for (text, want) in cases {
            assert_eq!(redact(text), want, "{text}");
        }
    }

    #[test]
    fn private_key_blocks_go_whole_even_when_cut_off() {
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        assert_eq!(redact(pem), "before\n[redacted]\nafter");
        assert_eq!(redact("x -----BEGIN RSA PRIVATE KEY-----\nMIIE"), "x [redacted]");
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB";
        assert_eq!(redact(cert), cert, "a certificate is public");
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        for text in [
            "",
            "task-1234567890abcdefghij is not an sk- key",
            "sk-short",
            "Bearer authentication is used",
            "the pair = value",
            "password=",
            "token: ",
            r#"{"key": ""}"#,
            "AKIA lowercase akiaiosfodnn7example",
            "naïve café ✓ sk-",
            "https://example.com:8080/path",
            r"C:\Users\me\key.txt",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn each_pattern_starts_at_its_minimum_length() {
        let cases = [
            ("sk-abcdefghijklmnop", "[redacted]"),
            ("sk-abcdefghijklmno", "sk-abcdefghijklmno"),
            ("ghp_abcdefghijklmnop", "[redacted]"),
            ("ghp_abcdefghijklmno", "ghp_abcdefghijklmno"),
            ("AKIAIOSFODNN7EXAMPL", "AKIAIOSFODNN7EXAMPL"),
            ("xoxb-1234567890", "[redacted]"),
            ("xoxb-123456789", "xoxb-123456789"),
            ("token=abcdefgh", "token=[redacted]"),
            ("token=abcdefg", "token=[redacted]"),
            ("token=a", "token=[redacted]"),
            ("Bearer abcdefg1", "Bearer [redacted]"),
            ("Bearer abcdef1", "Bearer abcdef1"),
            ("Bearer abcdefghijklmnopqrs", "Bearer abcdefghijklmnopqrs"),
            ("Bearer abcdefghijklmnopqrst", "Bearer [redacted]"),
            ("xsk-abcdefghijklmnop", "xsk-abcdefghijklmnop"),
        ];
        for (text, want) in cases {
            assert_eq!(redact(text), want, "{text}");
        }
    }

    #[test]
    fn a_git_sha_alone_is_left_but_not_after_a_key_name() {
        let sha = "3f2a9c1e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a";
        for text in [format!("commit {sha}"), format!("sha={sha}"), "HEAD is now at 3f2a9c1 fix".into()] {
            assert_eq!(redact(&text), text);
        }
        assert_eq!(redact(&format!("key={sha}")), "key=[redacted]");
        assert_eq!(redact(&format!("deploy_key: {sha} ok")), "deploy_key: [redacted] ok");
    }

    #[test]
    fn line_breaks_and_unicode_around_secrets_stay() {
        let cases = [
            ("API_KEY=abcd1234efgh5678\r\nnext", "API_KEY=[redacted]\r\nnext"),
            ("one\ntoken=abcdef0123456789\nthree", "one\ntoken=[redacted]\nthree"),
            ("→sk-abcdefghijklmnop←", "→[redacted]←"),
            ("token=abcdef0123456789é", "token=[redacted]"),
            ("密码 password: hunter22hunter 完成", "密码 password: [redacted] 完成"),
            ("ghp_ABCDEFGHIJKLMNOPQRSTUVWX🎉", "[redacted]🎉"),
            (
                "a\r\n-----BEGIN RSA PRIVATE KEY-----\r\nMIIE\r\n-----END RSA PRIVATE KEY-----\r\nb",
                "a\r\n[redacted]\r\nb",
            ),
            (
                "-----BEGIN EC PRIVATE KEY-----\nAAA\n-----END EC PRIVATE KEY-----\nmid\n-----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY-----",
                "[redacted]\nmid\n[redacted]",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(redact(text), want, "{text:?}");
        }
    }

    #[test]
    fn a_megabyte_of_output_stays_fast() {
        let line = "the quick brown fox: jumps = over 42 lazy dogs at https://example.com:8080/x\n";
        let mut text = String::new();
        for n in 0.. {
            if text.len() >= 1 << 20 {
                break;
            }
            text.push_str(if n % 1000 == 0 { "export API_TOKEN=abcdef0123456789\n" } else { line });
        }
        let started = std::time::Instant::now();
        let out = redact(&text);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(!out.contains("abcdef0123456789") && out.contains("API_TOKEN=[redacted]"));
        assert_eq!(out.len(), text.len() - text.matches("abcdef0123456789").count() * 6);
        for one in ["a".repeat(1 << 20), "=".repeat(1 << 20), ":".repeat(1 << 20)] {
            let started = std::time::Instant::now();
            assert_eq!(redact(&one), one);
            assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
        }
    }

    #[test]
    fn overlapping_matches_leave_one_mark_and_unicode_survives() {
        assert_eq!(redact("token=sk-abcdefghijklmnopqrst ✓"), "token=[redacted] ✓");
        assert_eq!(redact("é Bearer sk-abcdefghijklmnop1234"), "é Bearer [redacted]");
    }

    #[test]
    fn repeated_patterns_take_linear_time() {
        let cases: [(&str, &str); 5] = [
            ("sk-", "[redacted]"),
            ("key=", "key=[redacted]"),
            ("xoxb-", "[redacted]"),
            ("-----BEGIN RSA PRIVATE KEY-----\n", "[redacted]"),
            ("Bearer ", ""),
        ];
        for size in [64 << 10, 1 << 20] {
            for (unit, want) in cases {
                let text = unit.repeat(size / unit.len());
                let started = std::time::Instant::now();
                let out = redact(&text);
                let took = started.elapsed();
                assert!(took < std::time::Duration::from_secs(1), "{unit:?} x {size}: {took:?}");
                let want = if want.is_empty() { text.as_str() } else { want };
                assert_eq!(out, want, "{unit:?} x {size}");
            }
        }
    }

    #[test]
    fn any_whitespace_may_follow_bearer() {
        for gap in [" ", "\t", "\u{a0}", "\u{2003}", "  \u{a0} "] {
            let text = format!("Authorization: Bearer{gap}abcdef0123456789");
            assert_eq!(redact(&text), format!("Authorization: Bearer{gap}[redacted]"), "{gap:?}");
        }
        assert_eq!(redact("Bearer\u{a0}abc"), "Bearer\u{a0}abc", "short tokens stay");
    }
}
