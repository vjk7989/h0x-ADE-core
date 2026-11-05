//! Claude Code: `<config>/projects/<folder, every character but ASCII letters
//! and digits as '-'>/<session id>.jsonl`. Lines carry the `cwd`; titles are
//! lines appended again and again, so the latest is near the end.

use super::*;

/// Claude cuts longer folder names here and adds `-<hash of the whole path>`.
const MAX_NAME: usize = 200;
/// Read from each end of a session file.
const ENDS: u64 = 64 * 1024;
/// Title lines by preference: renamed by the person, named by Claude, older summaries, the last prompt.
const TITLES: [(&str, &str); 4] =
    [("custom-title", "customTitle"), ("ai-title", "aiTitle"), ("summary", "summary"), ("last-prompt", "lastPrompt")];

pub(super) fn list(projects: &Path, want: &str) -> Vec<SessionInfo> {
    let name = encode(want);
    let windows = windows(want);
    dirs(projects)
        .filter(|dir| dir.file_name().and_then(|n| n.to_str()).is_some_and(|n| named(n, &name, windows)))
        .flat_map(|dir| files(&dir, "jsonl"))
        .filter_map(|path| read(&path, want))
        .collect()
}

/// The folder as Claude names its project directory (per UTF-16 unit, as in JavaScript).
fn encode(folder: &str) -> String {
    let mut name = String::new();
    for c in folder.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else {
            (0..c.len_utf16()).for_each(|_| name.push('-'));
        }
    }
    name
}

/// Whether a project directory is the folder's, by name. Windows folders match in any case.
fn named(dir: &str, name: &str, windows: bool) -> bool {
    let same = |a: &str, b: &str| if windows { a.eq_ignore_ascii_case(b) } else { a == b };
    if name.len() <= MAX_NAME {
        return same(dir, name);
    }
    dir.as_bytes().get(MAX_NAME) == Some(&b'-') && dir.get(..MAX_NAME).is_some_and(|cut| same(cut, &name[..MAX_NAME]))
}

fn read(path: &Path, want: &str) -> Option<SessionInfo> {
    let id = path.file_stem()?.to_str()?;
    // An older subagent's side chain, not a session.
    if id.starts_with("agent-") {
        return None;
    }
    let (mut file, meta) = open(path)?;
    let mut lines = json_lines(&head(path, &mut file, ENDS)?);
    if meta.len() > ENDS {
        lines.extend(json_lines(&tail(path, &mut file, meta.len(), ENDS)?));
    }
    if lines.is_empty() {
        skipped(path, "no JSON lines");
        return None;
    }
    // Names collide (`a-b`, `a_b`): the folder it ran in decides.
    if lines.iter().find_map(|l| l["cwd"].as_str()).is_some_and(|cwd| normal(cwd) != want) {
        return None;
    }
    let title = TITLES.iter().find_map(|(kind, key)| {
        lines.iter().rev().filter(|l| l["type"] == *kind).find_map(|l| l[*key].as_str().and_then(title))
    });
    Some(SessionInfo {
        agent: "claude".into(),
        id: id.into(),
        title,
        last_used: modified_ms(&meta),
        path: path.display().to_string(),
    })
}
