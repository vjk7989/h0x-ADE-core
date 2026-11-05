//! Secrets out of text that leaves this machine: common token shapes become
//! `[redacted]`. A pattern list, not a guarantee (see docs/bridge.md).

const MARK: &str = "[redacted]";

/// `text` with API keys, tokens, private keys and `password=...` values replaced.
pub fn redact(text: &str) -> String {
    let b = text.as_bytes();
    let mut cuts = Vec::new();
    for i in 0..b.len() {
        if i == 0 || !is_word(b[i - 1]) {
            cuts.extend(prefixed(b, i).map(|end| (i, end)));
            cuts.extend(bearer(b, i));
        }
        if matches!(b[i], b'=' | b':') {
            cuts.extend(assigned(b, i));
        }
        if b[i..].starts_with(b"-----BEGIN ") {
            cuts.extend(pem(b, i));
        }
    }
    cuts.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in cuts {
        if start < at {
            at = at.max(end);
            continue;
        }
        out.push_str(&text[at..start]);
        out.push_str(MARK);
        at = end;
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
    let github = [b"ghp_".as_slice(), b"gho_", b"ghu_", b"ghs_", b"ghr_", b"github_pat_"];
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
    } else {
        return None;
    };
    let run = rest[skip..].iter().take_while(|c| ok(**c)).count();
    (run >= min).then_some(i + skip + run)
}

/// `Bearer <token>` at `i`: the token (the word stays readable).
fn bearer(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if !b.get(i..i + 6)?.eq_ignore_ascii_case(b"bearer") {
        return None;
    }
    let spaces = b[i + 6..].iter().take_while(|c| **c == b' ').count();
    let start = i + 6 + spaces;
    let run = &b[start..][..b[start..].iter().take_while(|c| is_secret(**c)).count()];
    // Prose ("Bearer authentication") has no digits; tokens do, or are long.
    let token = run.len() >= 8 && (run.iter().any(u8::is_ascii_digit) || run.len() >= 20);
    (spaces > 0 && token).then_some((start, start + run.len()))
}

/// The value after `api_key=`, `"token": "..."`, `PASSWORD=` and the like, at
/// the `=` or `:` at `i`.
fn assigned(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let end = if i > 0 && matches!(b[i - 1], b'"' | b'\'') { i - 1 } else { i };
    let start = b[..end].iter().rposition(|c| !(is_word(*c) || *c == b'-')).map_or(0, |p| p + 1);
    let name = b[start..end].to_ascii_lowercase();
    if !["token", "secret", "password", "key"].iter().any(|k| name.ends_with(k.as_bytes())) {
        return None;
    }
    let at = i + 1 + b[i + 1..].iter().take_while(|c| matches!(c, b' ' | b'"' | b'\'')).count();
    let run = b[at..].iter().take_while(|c| is_secret(**c)).count();
    (run >= 8).then_some((at, at + run))
}

/// A `-----BEGIN ... KEY-----` block at `i`, through its END line (or the end of the text).
fn pem(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let line_end = b[i..].iter().position(|c| *c == b'\n').map_or(b.len(), |p| i + p);
    find(&b[i..line_end], b"KEY-----")?;
    let end = find(&b[line_end..], b"-----END ").map_or(b.len(), |p| {
        let after = line_end + p + 9;
        find(&b[after..], b"-----").map_or(b.len(), |q| after + q + 5)
    });
    Some((i, end))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::redact;

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
            "the key = value pair",
            "password=short",
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
            ("token=abcdefg", "token=abcdefg"),
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
            ("token=abcdef0123456789é", "token=[redacted]é"),
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
}
