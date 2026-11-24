//! `h0x-core map callers <sym>` / `map grep <word>`: a lexical code map, built from the files on
//! every call (no index, no cache). `callers` lists the whole-word uses of a symbol with the
//! declaration each sits in; `grep` lists the matching lines. Comments and strings count: this is
//! a word search, not a parse. Output is capped so it never floods an agent's context.

use crate::gate::{has_word, is_decl};
use crate::route::SRC_EXTS;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The most bytes one answer prints, footer included.
const CAP: usize = 8192;
/// Larger files are skipped.
const MAX_FILE: u64 = 2 * 1024 * 1024;
/// A shown line is cut to this many characters.
const LINE_MAX: usize = 200;
const SKIP_DIRS: [&str; 5] = [".git", "target", "node_modules", "dist", "build"];
/// Searched by `grep` only.
const DOC_EXTS: [&str; 5] = ["md", "toml", "json", "yaml", "yml"];
/// Words before the name in a declaration line.
const DECL_WORDS: [&str; 29] = [
    "private",
    "protected",
    "internal",
    "pub",
    "crate",
    "super",
    "self",
    "in",
    "async",
    "unsafe",
    "extern",
    "export",
    "default",
    "abstract",
    "fn",
    "struct",
    "enum",
    "trait",
    "impl",
    "mod",
    "const",
    "static",
    "type",
    "class",
    "def",
    "function",
    "interface",
    "final",
    "public",
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Callers,
    Grep,
}

/// The command: prints the answer, or one error line on stderr with exit 2.
pub fn run(mode: Mode, word: &str, dir: Option<&Path>) -> i32 {
    if !crate::client::vouched() {
        eprintln!("map works inside HyperOx terminals");
        return 2;
    }
    match search(mode, word, dir.unwrap_or(Path::new("."))) {
        Ok(text) => {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            0
        }
        Err(msg) => {
            eprintln!("map: {msg}");
            2
        }
    }
}

/// The whole answer text (each line ends with a newline), at most `CAP` bytes.
pub fn search(mode: Mode, word: &str, root: &Path) -> Result<String, String> {
    if word.trim().is_empty() {
        return Err("nothing to search for".into());
    }
    if !root.is_dir() {
        return Err(format!("no such directory: {}", root.display()));
    }
    let mut hits: Vec<(usize, String)> = Vec::new();
    let mut kept_bytes = 0;
    let (mut total, mut later) = (0usize, HashSet::new());
    let files = list_files(root, mode == Mode::Grep);
    for (idx, (rel, path)) in files.iter().enumerate() {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Ok(text) = String::from_utf8(bytes) else { continue };
        for line in scan(mode, word, rel, &text) {
            total += 1;
            if kept_bytes <= CAP {
                kept_bytes += line.len() + 1;
                hits.push((idx, line));
            } else {
                later.insert(idx);
            }
        }
    }
    if total == 0 {
        return Ok("no matches\n".into());
    }
    let shown = |hits: &[(usize, String)]| hits.iter().map(|(_, l)| l.len() + 1).sum::<usize>();
    if total == hits.len() && shown(&hits) <= CAP {
        return Ok(hits.iter().map(|(_, l)| format!("{l}\n")).collect());
    }
    // Drop lines from the end until the lines and the footer fit.
    let footer = |kept: &[(usize, String)]| {
        let mut files: HashSet<usize> = later.clone();
        files.extend(kept_tail(&hits, kept.len()));
        format!("[… {} more matches in {} files; narrow with --dir]\n", total - kept.len(), files.len())
    };
    let mut n = hits.len();
    while n > 0 && shown(&hits[..n]) + footer(&hits[..n]).len() > CAP {
        n -= 1;
    }
    let mut out: String = hits[..n].iter().map(|(_, l)| format!("{l}\n")).collect();
    out.push_str(&footer(&hits[..n]));
    Ok(out)
}

/// The file indexes of the hits after the first `n`.
fn kept_tail(hits: &[(usize, String)], n: usize) -> impl Iterator<Item = usize> + '_ {
    hits[n..].iter().map(|(i, _)| *i)
}

/// The output lines for one file.
fn scan(mode: Mode, word: &str, rel: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut enclosing = String::from("-");
    // A byte order mark is not part of the first line.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (i, raw) in text.lines().enumerate() {
        let decl = parse_decl(raw);
        if let Some(d) = &decl {
            enclosing.clone_from(&d.label);
        }
        if !has_word(raw, word, &[""]) {
            continue;
        }
        let shown = cut(raw.trim());
        let n = i + 1;
        match mode {
            Mode::Grep => out.push(format!("{rel}:{n}  {shown}")),
            // The line that declares the symbol is not a use of it.
            Mode::Callers if declared(raw, decl.as_ref()).as_deref() == Some(word) => {}
            Mode::Callers => out.push(format!("{rel}:{n}  {enclosing}  {shown}")),
        }
    }
    out
}

/// A declaration line: the name it declares (none for an impl, which uses its names) and how the
/// block it opens is shown as the enclosing one.
struct Decl {
    declared: Option<String>,
    label: String,
}

fn parse_decl(line: &str) -> Option<Decl> {
    let trimmed = line.trim_start();
    if is_decl(line) {
        if words(trimmed).find(|w| !["pub", "crate", "super", "self", "in", "async"].contains(w)) == Some("impl") {
            let at = trimmed.find("impl").unwrap_or(0) + "impl".len();
            return Some(Decl { declared: None, label: impl_label(&trimmed[at..]) });
        }
        let name = words(trimmed).find(|w| !DECL_WORDS.contains(w)).map(str::to_string);
        return Some(Decl { label: name.clone().unwrap_or("-".into()), declared: name });
    }
    // Java and C#: modifiers, then a method or constructor: `public static void run(`.
    let rest = strip_modifiers(trimmed);
    if rest.len() < trimmed.len() {
        let head = &rest[..rest.find('(')?];
        if !head.contains(['=', ';']) {
            let name = words(head).last()?.to_string();
            return Some(Decl { label: name.clone(), declared: Some(name) });
        }
    }
    None
}

/// What a line declares for `callers` to leave out: its declaration's name, or a `let`/`var` binding.
fn declared(line: &str, decl: Option<&Decl>) -> Option<String> {
    if let Some(d) = decl {
        return d.declared.clone();
    }
    let t = line.trim_start();
    let t = t.strip_prefix("let ").or_else(|| t.strip_prefix("var "))?;
    let t = t.trim_start().strip_prefix("mut ").unwrap_or(t);
    words(t).next().map(str::to_string)
}

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

fn strip_modifiers(mut s: &str) -> &str {
    const MODS: [&str; 7] = ["public", "private", "protected", "internal", "static", "final", "abstract"];
    while let Some(next) = MODS.iter().find_map(|m| s.strip_prefix(m).filter(|r| r.starts_with(' '))) {
        s = next.trim_start();
    }
    s
}

/// `impl Display for Foo` or `impl Foo`, from what follows the keyword: generics and paths dropped.
fn impl_label(after: &str) -> String {
    let header = after.split('{').next().unwrap_or("");
    let header = header.split(" where ").next().unwrap_or(header);
    let mut depth = 0usize;
    let bare: String = header
        .chars()
        .filter(|&c| match c {
            '<' => {
                depth += 1;
                false
            }
            '>' if depth > 0 => {
                depth -= 1;
                false
            }
            _ => depth == 0,
        })
        .collect();
    let last = |p: &str| p.trim().rsplit("::").next().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    match bare.split_once(" for ") {
        Some((tr, ty)) => format!("impl {} for {}", last(tr), last(ty)),
        None => format!("impl {}", last(&bare)).trim_end().to_string(),
    }
}

fn cut(s: &str) -> String {
    match s.char_indices().nth(LINE_MAX) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Searchable files under `root` as (path relative to it with `/`, real path), sorted by that path.
/// Symlinks are not followed, so a link loop cannot hang the walk.
fn list_files(root: &Path, docs: bool) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    dirs.push(entry.path());
                }
            } else if kind.is_file() && wanted(&name, docs) && entry.metadata().is_ok_and(|m| m.len() < MAX_FILE) {
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                files.push((rel, path));
            }
        }
    }
    files.sort();
    files
}

fn wanted(name: &str, docs: bool) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| SRC_EXTS.contains(&ext) || (docs && DOC_EXTS.contains(&ext)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("h0x-map-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }

        fn put(&self, rel: &str, body: impl AsRef<[u8]>) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn callers(t: &Tmp, sym: &str) -> String {
        search(Mode::Callers, sym, &t.0).unwrap()
    }

    #[test]
    fn rust_callers_name_the_enclosing_fn_and_skip_the_definition() {
        let t = Tmp::new();
        t.put(
            "src/a.rs",
            "pub fn forward(x: u8) {}\n\npub(crate) async fn handle() {\n    forward(1);\n    let forwarded = 2;\n}\n",
        );
        t.put("src/b.rs", "impl S {\n    fn other(&self) {\n        crate::a::forward(2)\n    }\n}\n");
        assert_eq!(
            callers(&t, "forward"),
            "src/a.rs:4  handle  forward(1);\nsrc/b.rs:3  other  crate::a::forward(2)\n"
        );
    }

    #[test]
    fn ts_js_and_python_samples() {
        let t = Tmp::new();
        t.put(
            "web/app.ts",
            "export function boot() {\n  run();\n}\nexport default class Page {\n  go() { run(); }\n}\n",
        );
        t.put("web/old.js", "function run() {}\nconst x = () => run();\n");
        t.put("tool/job.py", "def run():\n    pass\n\nclass Job:\n    def go(self):\n        run()\n");
        assert_eq!(
            callers(&t, "run"),
            "tool/job.py:6  go  run()\nweb/app.ts:2  boot  run();\nweb/app.ts:5  Page  go() { run(); }\nweb/old.js:2  x  const x = () => run();\n"
        );
    }

    #[test]
    fn comments_and_strings_count() {
        let t = Tmp::new();
        t.put("a.rs", "fn f() {\n    // call forward here\n    let s = \"forward\";\n}\n");
        assert_eq!(callers(&t, "forward").lines().count(), 2);
    }

    #[test]
    fn grep_lists_lines_without_a_declaration_and_reads_docs() {
        let t = Tmp::new();
        t.put("a.rs", "fn forward() {}\n");
        t.put("README.md", "use forward\n");
        t.put("c.txt", "forward\n");
        assert_eq!(search(Mode::Grep, "forward", &t.0).unwrap(), "README.md:1  use forward\na.rs:1  fn forward() {}\n");
        // `callers` leaves docs alone.
        assert_eq!(callers(&t, "forward"), "no matches\n");
    }

    #[test]
    fn no_match_empty_dir_and_bad_input() {
        let t = Tmp::new();
        assert_eq!(callers(&t, "x"), "no matches\n");
        assert!(search(Mode::Grep, "", &t.0).is_err());
        assert!(search(Mode::Grep, "   ", &t.0).is_err());
        let missing = search(Mode::Grep, "x", &t.0.join("nope")).unwrap_err();
        assert!(missing.starts_with("no such directory") && !missing.contains('\n'));
        t.put("f.rs", "x\n");
        assert!(search(Mode::Grep, "x", &t.0.join("f.rs")).is_err());
    }

    #[test]
    fn skipped_dirs_and_files() {
        let t = Tmp::new();
        for d in [".git", "target", "node_modules", "dist", "build", ".hidden", "a/target"] {
            t.put(&format!("{d}/x.rs"), "needle\n");
        }
        t.put("big.rs", format!("needle\n{}", "x".repeat(MAX_FILE as usize)));
        t.put("bad.rs", [0xff, 0xfe, b'n', b'e', b'e', b'd', b'l', b'e', b'\n']);
        t.put("ok.rs", "needle\n");
        assert_eq!(search(Mode::Grep, "needle", &t.0).unwrap(), "ok.rs:1  needle\n");
    }

    #[test]
    fn crlf_files_match_cleanly() {
        let t = Tmp::new();
        t.put("w.rs", "fn a() {\r\n    go();\r\n}\r\n");
        assert_eq!(callers(&t, "go"), "w.rs:2  a  go();\n");
    }

    #[test]
    fn long_lines_are_cut() {
        let t = Tmp::new();
        t.put("a.rs", format!("go {}\n", "é".repeat(500)));
        let out = callers(&t, "go");
        assert_eq!(out.trim_end().chars().count(), "a.rs:1  -  ".chars().count() + LINE_MAX + 1);
    }

    /// `n` lines of `w.rs:1  - go` style, each exactly `len` bytes with the newline.
    fn filler(t: &Tmp, n: usize, len: usize) {
        for i in 0..n {
            let head = format!("f{i:03}.rs:1  -  ");
            let pad = len - 1 - head.len() - 3;
            t.put(&format!("f{i:03}.rs"), format!("go {}\n", "x".repeat(pad)));
        }
    }

    #[test]
    fn the_cap_boundary_is_exactly_8192() {
        let t = Tmp::new();
        filler(&t, 64, 128);
        let out = callers(&t, "go");
        assert_eq!(out.len(), 8192);
        assert!(!out.contains("more matches"));
        // One byte more in the last file pushes it over.
        t.put("f063.rs", format!("go {}\n", "x".repeat(128 - 1 - "f063.rs:1  -  ".len() - 3 + 1)));
        let out = callers(&t, "go");
        assert!(out.len() <= 8192, "{}", out.len());
        let last = out.lines().last().unwrap();
        assert!(last.starts_with("[… ") && last.ends_with("; narrow with --dir]"), "{last}");
        assert!(last.contains("1 more matches in 1 files"), "{last}");
    }

    #[test]
    fn a_capped_answer_counts_what_it_left_out() {
        let t = Tmp::new();
        for i in 0..300 {
            t.put(&format!("d{:03}/m.rs", i), "fn a() {\n    go();\n    go();\n}\n");
        }
        let out = callers(&t, "go");
        assert!(out.len() <= 8192);
        let last = out.lines().last().unwrap();
        let shown = out.lines().count() - 1;
        let more: usize = last.trim_start_matches("[… ").split(' ').next().unwrap().parse().unwrap();
        assert_eq!(shown + more, 600);
        assert!(out.starts_with("d000/m.rs:2  a  go();\n"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_does_not_hang() {
        let t = Tmp::new();
        t.put("a/x.rs", "needle\n");
        std::os::unix::fs::symlink(&t.0, t.0.join("a").join("loop")).unwrap();
        assert_eq!(search(Mode::Grep, "needle", &t.0).unwrap(), "a/x.rs:1  needle\n");
    }

    #[test]
    fn two_thousand_files_are_fast() {
        let t = Tmp::new();
        for i in 0..2000 {
            let body: String = (0..40).map(|l| format!("fn f{l}() {{ let v = value{i}(); }}\n")).collect();
            t.put(&format!("m{}/f{i}.rs", i % 40), body);
        }
        let started = std::time::Instant::now();
        let out = callers(&t, "value7");
        assert!(out.contains("value7"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn a_symbol_with_path_separators_or_regex_characters_is_matched_literally() {
        let t = Tmp::new();
        t.put("a.rs", "fn f() {\n    Foo::bar();\n    XFoo::bar();\n    Foo::barx();\n    crate::Foo::bar(1);\n}\n");
        t.put("b.cc", "int g() {\n  a.operator+(b);\n  operator+x;\n}\n");
        t.put("c.rs", "fn h() {\n    a.b();\n    axb();\n}\n");
        assert_eq!(callers(&t, "Foo::bar"), "a.rs:2  f  Foo::bar();\na.rs:5  f  crate::Foo::bar(1);\n");
        assert_eq!(search(Mode::Grep, "operator+", &t.0).unwrap(), "b.cc:2  a.operator+(b);\n");
        // `.` is a dot, not "any character".
        assert_eq!(search(Mode::Grep, "a.b", &t.0).unwrap(), "c.rs:2  a.b();\n");
    }

    #[test]
    fn a_symbol_inside_a_longer_identifier_does_not_match() {
        let t = Tmp::new();
        t.put("a.rs", "fn f() {\n    forwarder();\n    unforward();\n    forward_all();\n    forward2();\n}\n");
        assert_eq!(callers(&t, "forward"), "no matches\n");
    }

    #[test]
    fn unicode_letters_join_a_word_and_unicode_symbols_do_not() {
        let t = Tmp::new();
        t.put(
            "a.rs",
            "fn f() {\n    éforward();\n    forwardé();\n    日本forward();\n    →forward();\n    größe();\n    größer();\n}\n",
        );
        assert_eq!(callers(&t, "forward"), "a.rs:5  f  →forward();\n");
        assert_eq!(callers(&t, "größe"), "a.rs:6  f  größe();\n");
    }

    #[test]
    fn a_recursive_call_on_the_declaration_line_is_not_listed_but_later_ones_are() {
        let t = Tmp::new();
        t.put("a.rs", "fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }\nfn fib(n: u64) -> u64 {\n    fib(n - 1) + fib(n - 2)\n}\n");
        assert_eq!(callers(&t, "fact"), "no matches\n");
        assert_eq!(callers(&t, "fib"), "a.rs:3  fib  fib(n - 1) + fib(n - 2)\n");
    }

    #[test]
    fn a_declaration_line_of_another_symbol_is_a_use() {
        let t = Tmp::new();
        t.put("a.rs", "struct Foo;\nfn make() -> Foo {\n    Foo\n}\nimpl Display for Foo {}\n");
        assert_eq!(
            callers(&t, "Foo"),
            "a.rs:2  make  fn make() -> Foo {\na.rs:3  make  Foo\na.rs:5  impl Display for Foo  impl Display for Foo {}\n"
        );
        // `impl Display for Foo` declares neither: it uses both.
        assert_eq!(callers(&t, "Display"), "a.rs:5  impl Display for Foo  impl Display for Foo {}\n");
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_first_declaration() {
        let t = Tmp::new();
        t.put("a.rs", "\u{feff}fn forward() {}\nfn g() {\n    forward();\n}\n");
        assert_eq!(callers(&t, "forward"), "a.rs:3  g  forward();\n");
    }

    #[test]
    fn the_line_cut_counts_characters_not_bytes_at_the_boundary() {
        let t = Tmp::new();
        // After trimming: "go " + 197 four-byte chars = exactly 200 characters, so no cut.
        t.put("a.rs", format!("    go {}\n", "😀".repeat(LINE_MAX - 3)));
        // 201 characters, the 200th a three-byte char: cut after it.
        t.put("b.rs", format!("go {}é€x\n", "x".repeat(LINE_MAX - 5)));
        let out = search(Mode::Grep, "go", &t.0).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], format!("a.rs:1  go {}", "😀".repeat(LINE_MAX - 3)));
        assert_eq!(lines[1], format!("b.rs:1  go {}é€…", "x".repeat(LINE_MAX - 5)));
    }

    #[test]
    fn a_file_of_exactly_two_mb_is_skipped_and_one_byte_less_is_read() {
        let t = Tmp::new();
        let body = |len: u64| format!("needle\n{}", "x".repeat(len as usize - "needle\n".len()));
        t.put("at.rs", body(MAX_FILE));
        t.put("under.rs", body(MAX_FILE - 1));
        assert_eq!(std::fs::metadata(t.0.join("at.rs")).unwrap().len(), 2 * 1024 * 1024);
        assert_eq!(search(Mode::Grep, "needle", &t.0).unwrap(), "under.rs:1  needle\n");
    }

    #[test]
    fn the_order_is_by_path_then_line_number_and_repeats_exactly() {
        let t = Tmp::new();
        // Created out of order; line 10 must come after line 9 (not as text "10" < "9").
        let body: String = (1..=12).map(|i| if i >= 9 { "go\n".to_string() } else { "x\n".to_string() }).collect();
        for name in ["z.rs", "m/b.rs", "a.rs", "m/a.rs"] {
            t.put(name, &body);
        }
        let first = search(Mode::Grep, "go", &t.0).unwrap();
        for _ in 0..5 {
            assert_eq!(search(Mode::Grep, "go", &t.0).unwrap(), first);
        }
        let keys: Vec<(String, usize)> = first
            .lines()
            .map(|l| {
                let (path, rest) = l.split_once(':').unwrap();
                (path.to_string(), rest.split_once("  ").unwrap().0.parse().unwrap())
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert_eq!(keys.len(), 16);
        assert_eq!(keys[0], ("a.rs".to_string(), 9));
        assert_eq!(keys[1], ("a.rs".to_string(), 10));
    }

    #[test]
    fn the_footer_counts_one_file_when_only_the_last_file_overflows() {
        let t = Tmp::new();
        t.put("a.rs", "fn a() {\n    go();\n}\n");
        let body: String = (0..1000).map(|_| "go();\n").collect();
        t.put("z.rs", body);
        let out = callers(&t, "go");
        assert!(out.len() <= CAP, "{}", out.len());
        assert!(out.starts_with("a.rs:2  a  go();\nz.rs:1  -  go();\n"), "{out}");
        let last = out.lines().last().unwrap();
        let shown = out.lines().count() - 1;
        assert_eq!(last, format!("[… {} more matches in 1 files; narrow with --dir]", 1001 - shown));
    }

    #[cfg(windows)]
    #[test]
    fn windows_dir_spellings_all_give_forward_slash_paths() {
        let t = Tmp::new();
        t.put("with space/sub dir/a.rs", "fn f() { needle(); }\n");
        // `\\?\G:\…` from canonicalize; the plain spelling without the prefix.
        let verbatim = std::fs::canonicalize(&t.0).unwrap().join("with space").to_string_lossy().into_owned();
        assert!(verbatim.starts_with(r"\\?\"), "{verbatim}");
        let plain = verbatim.trim_start_matches(r"\\?\").to_string();
        let want = "sub dir/a.rs:1  fn f() { needle(); }\n";
        for dir in [
            plain.clone(),
            format!(r"{plain}\"),
            plain.replace('\\', "/"),
            format!("{}/", plain.replace('\\', "/")),
            verbatim.clone(),
            format!(r"{verbatim}\"),
        ] {
            assert_eq!(search(Mode::Grep, "needle", Path::new(&dir)).unwrap(), want, "{dir}");
        }
    }
}
