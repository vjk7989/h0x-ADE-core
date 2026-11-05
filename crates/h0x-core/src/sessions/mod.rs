//! `session.list`: the agent sessions that ran in a folder, read from each
//! agent's own store. Read-only and headers only: session logs grow to many
//! MB. The stores are other programs' formats, so a file that does not parse
//! is left out, never an error. Only real folders and files inside a store
//! are read: links could lead out of it, and the asked folder is compared,
//! never joined into a path.

use crate::api::SessionInfo;
use std::fs::{DirEntry, File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

mod claude;
mod codex;
mod opencode;
#[cfg(test)]
mod tests;

/// Where each agent keeps its sessions, found as the agent finds it.
pub struct Roots {
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude: Option<PathBuf>,
    /// `$CODEX_HOME`, else `~/.codex`.
    pub codex: Option<PathBuf>,
    /// `$XDG_DATA_HOME/opencode`, else `~/.local/share/opencode` (on every OS).
    pub opencode: Option<PathBuf>,
}

impl Roots {
    pub fn from_env() -> Roots {
        Roots::new(|name| std::env::var(name).ok())
    }

    pub fn new(env: impl Fn(&str) -> Option<String>) -> Roots {
        let env = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = env("HOME").or_else(|| env("USERPROFILE"));
        let or_home = |var: &str, under: &[&str]| {
            env(var).or_else(|| home.as_ref().map(|h| under.iter().fold(h.clone(), |p, part| p.join(part))))
        };
        Roots {
            claude: or_home("CLAUDE_CONFIG_DIR", &[".claude"]),
            codex: or_home("CODEX_HOME", &[".codex"]),
            opencode: or_home("XDG_DATA_HOME", &[".local", "share"]).map(|d| d.join("opencode")),
        }
    }
}

/// Every session that ran in `folder` (absolute), newest first.
pub fn list(folder: &str, roots: &Roots) -> Vec<SessionInfo> {
    let want = normal(folder);
    let mut all = Vec::new();
    if let Some(root) = &roots.claude {
        all.extend(claude::list(&root.join("projects"), &want));
    }
    if let Some(root) = &roots.codex {
        all.extend(codex::list(root, &want));
    }
    if let Some(root) = &roots.opencode {
        all.extend(opencode::list(root, &want));
    }
    all.sort_by(|a, b| b.last_used.cmp(&a.last_used).then_with(|| (&a.agent, &a.id).cmp(&(&b.agent, &b.id))));
    all
}

/// Whether a path is a Windows one: a drive letter or a `\\server` share.
fn windows(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || path.starts_with(r"\\")
}

/// A folder as compared: `\\?\` dropped, `.` and `..` resolved, no trailing
/// separator; Windows paths with `\` and lowercase, as Windows compares them.
pub fn normal(path: &str) -> String {
    let path = match path.strip_prefix(r"\\?\") {
        Some(rest) => rest.strip_prefix(r"UNC\").map_or_else(|| rest.to_string(), |share| format!(r"\\{share}")),
        None => path.to_string(),
    };
    let drive = windows(&path) && !path.starts_with(r"\\");
    let (path, sep) = if windows(&path) { (path.replace('/', r"\").to_lowercase(), '\\') } else { (path, '/') };
    let lead = &path[..path.len() - path.trim_start_matches(sep).len()];
    let mut parts: Vec<&str> = Vec::new();
    for part in path[lead.len()..].split(sep) {
        match part {
            "" | "." => {}
            // Never above the drive.
            ".." if parts.len() > usize::from(drive) => {
                parts.pop();
            }
            ".." => {}
            _ => parts.push(part),
        }
    }
    let mut out = format!("{lead}{}", parts.join(&sep.to_string()));
    if drive && parts.len() == 1 {
        out.push('\\');
    }
    out
}

/// A store file that is left out: said in debug builds, never an error.
fn skipped(path: &Path, why: impl std::fmt::Display) {
    if cfg!(debug_assertions) {
        eprintln!("core: sessions: skipped {}: {why}", path.display());
    }
}

/// What a store folder really holds: links and junctions are skipped.
fn entries(dir: &Path) -> impl Iterator<Item = (DirEntry, std::fs::FileType)> + use<> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| {
        let kind = e.file_type().ok()?;
        if kind.is_symlink() {
            skipped(&e.path(), "a link");
            return None;
        }
        Some((e, kind))
    })
}

fn dirs(dir: &Path) -> impl Iterator<Item = PathBuf> + use<> {
    entries(dir).filter(|(_, kind)| kind.is_dir()).map(|(e, _)| e.path())
}

fn files<'a>(dir: &Path, extension: &'a str) -> impl Iterator<Item = PathBuf> + use<'a> {
    entries(dir)
        .filter(|(_, kind)| kind.is_file())
        .map(|(e, _)| e.path())
        .filter(move |p| p.extension().is_some_and(|x| x == extension))
}

fn open(path: &Path) -> Option<(File, Metadata)> {
    match File::open(path).and_then(|f| f.metadata().map(|meta| (f, meta))) {
        Ok(opened) => Some(opened),
        Err(e) => {
            skipped(path, e);
            None
        }
    }
}

/// The first `max` bytes.
fn head(path: &Path, file: &mut File, max: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    match file.by_ref().take(max).read_to_end(&mut bytes) {
        Ok(_) => Some(bytes),
        Err(e) => {
            skipped(path, e);
            None
        }
    }
}

/// The last `max` bytes of a file `len` long, from the first whole line in them.
fn tail(path: &Path, file: &mut File, len: u64, max: u64) -> Option<Vec<u8>> {
    let start = len.saturating_sub(max);
    if let Err(e) = file.seek(SeekFrom::Start(start)) {
        skipped(path, e);
        return None;
    }
    let mut bytes = head(path, file, max)?;
    if start > 0 {
        let cut = bytes.iter().position(|&c| c == b'\n').map_or(bytes.len(), |i| i + 1);
        bytes.drain(..cut);
    }
    Some(bytes)
}

/// The lines that are whole JSON; a cut or broken line is left out.
fn json_lines(bytes: &[u8]) -> Vec<serde_json::Value> {
    bytes.split(|&c| c == b'\n').filter_map(|line| serde_json::from_slice(line).ok()).collect()
}

fn modified_ms(meta: &Metadata) -> u64 {
    let since = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    since.map_or(0, |d| d.as_millis() as u64)
}

/// A title fit for a list: its first line, at most 120 characters.
fn title(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.chars().take(120).collect())
}
