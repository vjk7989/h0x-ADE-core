//! The served page must work under `default-src 'self'` and never turn
//! server text into markup.

use super::{PAGE, SCRIPT, STYLE};

/// Every `name="value"` attribute value in `html`.
fn values<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let key = format!(" {name}=\"");
    html.match_indices(&key).map(|(at, _)| html[at + key.len()..].split('"').next().unwrap()).collect()
}

#[test]
fn page_loads_only_its_own_assets() {
    let mut refs = values(PAGE, "src");
    refs.extend(values(PAGE, "href"));
    refs.sort_unstable();
    assert_eq!(refs, ["/app.js", "/style.css"]);
    for r in refs {
        assert!(r.starts_with('/') && !r.starts_with("//"), "{r} is not same-origin");
    }
}

#[test]
fn page_has_no_inline_script_style_or_handlers() {
    let lower = PAGE.to_ascii_lowercase();
    let scripts: Vec<_> = lower.match_indices("<script").map(|(at, _)| &lower[at..]).collect();
    assert_eq!(scripts.len(), 1);
    for s in scripts {
        let open = &s[..s.find('>').unwrap() + 1];
        assert!(open.contains(" src=\"/"), "inline script: {open}");
        assert!(s[open.len()..].starts_with("</script>"), "script with a body");
    }
    assert!(!lower.contains("<style"), "inline <style>");
    assert!(!lower.contains(" style="), "style= attribute");
    assert!(!lower.contains("javascript:"), "javascript: URL");
    // No on*= handler attributes (onclick, onload, ...).
    for (at, _) in lower.match_indices(" on") {
        let word: String = lower[at + 1..].chars().take_while(char::is_ascii_alphabetic).collect();
        assert!(!lower[at + 1 + word.len()..].starts_with('='), "handler attribute {word}");
    }
}

#[test]
fn script_never_builds_markup_or_reaches_elsewhere() {
    for banned in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
        "setAttribute('style'",
    ] {
        assert!(!SCRIPT.contains(banned), "app.js uses {banned}");
    }
    for scheme in ["http://", "https://", "ws://", "wss://", "//cdn", "import("] {
        assert!(!SCRIPT.contains(scheme), "app.js names {scheme}");
    }
    // The WebSocket goes back to the host that served the page.
    assert!(SCRIPT.contains("${proto}//${location.host}/ws"));
}

#[test]
fn style_loads_nothing() {
    for banned in ["url(", "@import", "expression("] {
        assert!(!STYLE.contains(banned), "style.css uses {banned}");
    }
}

#[test]
fn assets_stay_small() {
    let total = PAGE.len() + SCRIPT.len() + STYLE.len();
    assert!(total < 40 * 1024, "{total} bytes");
}

/// app.test.mjs runs app.js against a fake page with node's own test runner;
/// without node on PATH it is skipped locally, but fails on CI (the `CI` env
/// var), where a missing node would otherwise hide that the tests never ran.
#[test]
fn app_logic_passes_its_node_tests() {
    let file = concat!(env!("CARGO_MANIFEST_DIR"), "/src/bridge/web/app.test.mjs");
    let out = match std::process::Command::new("node").arg("--test").arg(file).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            assert!(std::env::var_os("CI").is_none(), "node not found on CI: app.test.mjs did not run");
            return eprintln!("node not found: app.test.mjs not run");
        }
        Err(e) => panic!("node: {e}"),
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    // The summary line ("pass 16"), visible with --nocapture.
    if let Some(line) =
        text.lines().find(|l| l.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).starts_with("pass "))
    {
        println!("{}", line.trim());
    }
    assert!(out.status.success(), "{text}");
    assert!(text.contains("fail 0"), "{text}");
}
