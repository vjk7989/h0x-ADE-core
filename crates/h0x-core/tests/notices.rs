//! WT-C5: HyperOx Worktrees is a copy of worktrunk; NOTICE and README credit it, and the
//! notices h0x-core embeds and ships (`h0x-core --licenses`) carry its licence text in full,
//! put there by `scripts/notices` from `distribution/notices/worktrunk.txt`.

use std::path::PathBuf;

/// A file of the repository, with Windows line ends folded.
fn repo(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())).replace("\r\n", "\n")
}

const URL: &str = "https://github.com/max-sixty/worktrunk";
const LABEL: &str = "HyperOx Worktrees, from worktrunk (MIT OR Apache-2.0)";

#[test]
fn notices_edge_notice_and_readme_credit_worktrunk_with_its_url_and_licences() {
    let notice = repo("NOTICE");
    for want in ["HyperOx Worktrees", "worktrunk", URL, "MIT OR Apache-2.0", "Maximilian Roos"] {
        assert!(notice.contains(want), "NOTICE lacks {want:?}:\n{notice}");
    }
    // The herdr and Ghostty credits are still there.
    assert!(notice.contains("herdr") && notice.contains("ghostty"), "{notice}");
    let readme = repo("README.md");
    assert!(readme.contains(URL) && readme.contains("MIT OR Apache-2.0"), "README lacks the worktrunk credit");
}

#[test]
fn notices_edge_the_checked_in_licence_is_the_full_mit_text() {
    let text = repo("distribution/notices/worktrunk.txt");
    assert!(!text.trim().is_empty());
    for want in [
        URL,
        "Copyright (c) 2025 Maximilian Roos",
        "Permission is hereby granted, free of charge, to any person obtaining a copy",
        "The above copyright notice and this permission notice shall be included",
        "THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND",
    ] {
        assert!(text.contains(want), "worktrunk.txt lacks {want:?}");
    }
    let readme = repo("distribution/notices/README.md");
    assert!(readme.contains("`worktrunk.txt`"), "the notices README explains worktrunk.txt");
}

#[test]
fn notices_edge_third_party_notices_carry_the_worktrunk_part_exactly_as_the_script_writes_it() {
    let embedded = repo("crates/h0x-core/THIRD-PARTY-NOTICES");
    let licence = repo("distribution/notices/worktrunk.txt");
    let part = format!("\n==== {LABEL}\n\n{licence}");
    assert_eq!(embedded.matches(&part).count(), 1, "the worktrunk part, once, byte for byte");
    // It sits with the checked-in parts, after noble and before the first crate.
    let noble = embedded.find("\n==== noble crypto libraries").expect("the noble part");
    let worktrunk = embedded.find(&format!("\n==== {LABEL}")).unwrap();
    assert!(noble < worktrunk, "worktrunk comes after noble, in the script's order");
}

/// The words of the script's `for part in … ; do` list, quoted labels removed.
fn parts(script: &str) -> Vec<String> {
    let start = script.find("for part in ").expect("the parts loop") + "for part in ".len();
    let end = start + script[start..].find("; do").expect("its end");
    let mut bare = String::new();
    let mut quoted = false;
    for c in script[start..end].chars() {
        if c == '"' {
            quoted = !quoted;
        } else if !quoted {
            bare.push(c);
        }
    }
    bare.split_whitespace().map(String::from).collect()
}

#[test]
fn notices_edge_the_script_lists_worktrunk_as_a_part_with_a_file_and_nothing_stray() {
    let script = repo("scripts/notices");
    let words = parts(&script);
    // Line continuations aside, every word is `<name>:` naming a checked-in notice.
    let names: Vec<&str> = words.iter().filter(|w| *w != "\\").map(String::as_str).collect();
    for word in &names {
        let name = word.strip_suffix(':').unwrap_or_else(|| panic!("a stray word {word:?} in the parts list"));
        assert!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "{word:?}"
        );
        let file =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../distribution/notices").join(format!("{name}.txt"));
        assert!(file.is_file(), "{} is missing", file.display());
    }
    assert!(names.contains(&"worktrunk:"), "{names:?}");
    assert!(script.contains(&format!("worktrunk:\"{LABEL}\"")), "the label matches THIRD-PARTY-NOTICES");
}
