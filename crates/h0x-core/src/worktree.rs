//! Git worktrees: list them with their status, create one at a fixed place, merge one into the
//! default branch, and remove one safely. All of it is done by `h0x-wt` (HyperOx Worktrees, a
//! modified copy of worktrunk, MIT OR Apache-2.0), the tool shipped beside this binary in
//! `h0x-core-tools/`. Core only runs it (never through a shell, never on PATH, never with `--yes`
//! or hooks) and maps its JSON and exit codes onto the types below.

use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(test)]
mod tests;

/// Why a worktree call failed. Each kind has a stable name in the API.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// The folder is not inside a git repository (or git cannot read it).
    NotGit(String),
    /// A parameter is wrong: the message says which.
    Invalid(String),
    /// The worktree at this path has uncommitted changes.
    Dirty(String),
    /// The branch is already checked out at this path.
    CheckedOut(String),
    /// Submodules with commits that exist on no remote.
    Unpushed(Vec<String>),
    /// The main checkout cannot be removed or merged away.
    MainCheckout,
    /// The merge conflicts in these files; nothing was changed.
    Conflict(Vec<String>),
    /// `h0x-wt` is not installed beside this binary (an older h0x-core, or a bare binary).
    MissingTool,
    /// The tool or git failed: its message.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotGit(m) | Error::Invalid(m) | Error::Failed(m) => f.write_str(m),
            Error::Dirty(m) => f.write_str(m),
            Error::Unpushed(names) => write!(f, "submodules with unpushed commits: {}", names.join(", ")),
            Error::CheckedOut(path) => write!(f, "that branch is already checked out at {path}"),
            Error::MainCheckout => f.write_str("the main checkout cannot be removed"),
            Error::Conflict(files) => write!(f, "merge conflict in: {}", files.join(", ")),
            Error::MissingTool => write!(f, "{TOOL} is not installed next to h0x-core"),
        }
    }
}

impl Error {
    /// The API error for this failure. (Not a `From` impl: another `From` for `RpcError` would
    /// make the `?` in `bridge/web` ambiguous.)
    pub fn rpc(self) -> RpcError {
        let e = self;
        let kind = match &e {
            Error::Invalid(m) => return RpcError::invalid_params(m.clone()),
            Error::MissingTool => {
                return RpcError::new("missing_tool", e.to_string(), "Update h0x-core to use Worktrees");
            }
            Error::NotGit(_) => "not_git",
            Error::Dirty(_) => "dirty",
            Error::Unpushed(_) => "unpushed",
            Error::CheckedOut(_) => "checked_out",
            Error::MainCheckout => "main_checkout",
            Error::Conflict(_) => "conflict",
            Error::Failed(_) => "failed",
        };
        RpcError::new(kind, e.to_string(), "")
    }
}

/// How a branch's work is already in the default branch (worktrunk's IntegrationReason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Integrated {
    /// The branch is the same commit as the target.
    SameCommit,
    /// The target has moved past the branch.
    Ancestor,
    /// The branch changes no files beyond where it forked.
    NoAddedChanges,
    /// Different history, identical files.
    TreesMatch,
    /// Merging it would not change the target (a squash merge landed).
    MergeAddsNothing,
    /// Every change of it is in the target under other commits.
    PatchIdMatch,
    /// The tool names a reason this core does not know (a newer tool): integrated, but why is unknown.
    #[serde(other)]
    Unknown,
}

/// One checkout of a repository.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Worktree {
    /// Its folder, as git writes it (forward slashes, also on Windows).
    pub path: String,
    /// The branch it has checked out; null when detached.
    pub branch: Option<String>,
    /// The main checkout: the first in the list, the one that cannot be removed.
    pub main: bool,
    /// Uncommitted changes, untracked files included.
    pub dirty: bool,
    /// Commits it has that the default branch does not.
    pub ahead: u32,
    /// Commits the default branch has that it does not.
    pub behind: u32,
    /// Why its work is already in the default branch; null when it is not (and for the main checkout).
    pub integrated: Option<Integrated>,
    /// When its head commit was made (Unix seconds).
    pub at: i64,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Listing {
    /// The repository's default branch; null when none exists here.
    pub default_branch: Option<String>,
    /// The main checkout first, then the others, most recent commit first.
    pub worktrees: Vec<Worktree>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Created {
    pub path: String,
    pub branch: String,
    /// False when the branch already existed and was checked out as it was.
    pub new_branch: bool,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Removed {
    pub path: String,
    pub branch: Option<String>,
    /// The branch goes only when its work is in the target; otherwise it is kept.
    pub branch_deleted: bool,
    /// Why the branch counts as integrated; null when it does not.
    pub integrated: Option<Integrated>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Merged {
    /// The branch it was merged into.
    pub target: String,
    /// False when the work was already in the target, so nothing was merged.
    pub merged: bool,
    pub removed: Removed,
}

const TOOL: &str = if cfg!(windows) { "h0x-wt.exe" } else { "h0x-wt" };

/// Where the tool lives: `h0x-core-tools/` beside this binary, never on PATH (a user's own `wt`
/// is never used). Tests may point `H0X_WT_BIN` at a built one; a release binary ignores it.
fn tool() -> Result<PathBuf, Error> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("H0X_WT_BIN") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).map_err(|_| Error::MissingTool)?;
    let path = exe.parent().ok_or(Error::MissingTool)?.join("h0x-core-tools").join(TOOL);
    if path.is_file() { Ok(path) } else { Err(Error::MissingTool) }
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `program` in `dir`: hidden on Windows, no stdin, never asking a question.
fn spawn(program: &Path, dir: &Path, args: &[String], env: &[(&str, String)]) -> Result<Out, Error> {
    let mut cmd = Command::new(program);
    cmd.current_dir(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", "true").stdin(Stdio::null());
    // A controlled environment: nothing may point git at another repository, the messages are plain
    // English (the error kinds are read from them) and file names are never quoted. HOME and the
    // user's git identity stay (a squash commits as them).
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX", "GIT_NAMESPACE", "CLICOLOR_FORCE"] {
        cmd.env_remove(var);
    }
    cmd.env("NO_COLOR", "1").env("LC_ALL", "C").env("LANGUAGE", "C");
    cmd.env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", "core.quotePath").env("GIT_CONFIG_VALUE_0", "false");
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    // The install path is not for users' eyes: the message says nothing about where the tool is.
    let out = cmd.output().map_err(|_| Error::Failed("HyperOx Worktrees tool failed to start".into()))?;
    Ok(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

fn run_tool(dir: &Path, args: &[String], env: &[(&str, String)]) -> Result<Out, Error> {
    spawn(&tool()?, dir, args, env)
}

fn git(dir: &Path, args: &[&str]) -> Option<Out> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    spawn(Path::new("git"), dir, &args, &[]).ok().filter(|o| o.ok)
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| a.to_string()).collect()
}

/// The tool's argv per verb. Always JSON; hooks always off (there is no trust prompt here yet);
/// never `--yes`; remove waits for the removal to finish.
fn list_args() -> Vec<String> {
    args(&["list", "--format", "json"])
}

fn switch_args(branch: &str, create: bool, base: Option<&str>) -> Vec<String> {
    let mut a = args(&["switch"]);
    if create {
        a.push("--create".into());
    }
    a.push(branch.into());
    if let (true, Some(base)) = (create, base) {
        a.extend(["--base".into(), base.into()]);
    }
    a.extend(args(&["--no-cd", "--no-hooks", "--format", "json"]));
    a
}

fn remove_args(target: &str, force: bool) -> Vec<String> {
    let mut a = args(&["remove", target, "--foreground"]);
    if force {
        a.push("--force".into());
    }
    a.extend(args(&["--no-hooks", "--format", "json"]));
    a
}

/// The removal is a separate call (`--no-remove`) so it runs in the foreground from the main checkout.
fn merge_args(target: Option<&str>, squash: bool) -> Vec<String> {
    let mut a = args(&["merge"]);
    a.extend(target.map(str::to_string));
    a.push("--no-remove".into());
    if !squash {
        a.push("--no-squash".into());
    }
    a.extend(args(&["--no-hooks", "--format", "json"]));
    a
}

/// The first `✗` line of the tool's stderr, else its first line.
fn headline(stderr: &str) -> String {
    let plain = strip_ansi(stderr);
    let line = plain.lines().find(|l| l.trim_start().starts_with('✗')).or_else(|| plain.lines().next()).unwrap_or("");
    line.trim().trim_start_matches('✗').trim().to_string()
}

/// `text` without terminal escape sequences (colors and the like).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
        } else if chars.peek() == Some(&'[') {
            chars.next();
            // A CSI sequence ends at its first byte in @..~.
            for n in chars.by_ref() {
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        }
    }
    out
}

/// A failed call as a typed error. The messages are the pinned, bundled tool's own.
fn failed(out: &Out) -> Error {
    let msg = headline(&out.stderr);
    let lower = msg.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if has(&["not a git repository"]) || strip_ansi(&out.stderr).to_lowercase().contains("not a git repository") {
        Error::NotGit(msg)
    } else if has(&["main worktree cannot be removed"]) {
        Error::MainCheckout
    } else if has(&["has uncommitted changes"]) {
        Error::Dirty(msg)
    } else if has(&["invalid branch name", "already exists", "no branch named", "no branch, tag, or commit"]) {
        Error::Invalid(msg)
    } else if msg.is_empty() {
        Error::Failed("h0x-wt failed".into())
    } else {
        Error::Failed(msg)
    }
}

fn ok_or_failed(out: Out) -> Result<String, Error> {
    if out.ok { Ok(out.stdout) } else { Err(failed(&out)) }
}

fn unexpected<E: std::fmt::Display>(what: &str, e: E) -> Error {
    Error::Failed(format!("unexpected {what} output from {TOOL}: {e}"))
}

#[derive(Deserialize)]
struct ListJson {
    repo: RepoJson,
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct RepoJson {
    default_branch: Option<String>,
}

#[derive(Deserialize)]
struct Item {
    branch: Option<String>,
    head: Option<Head>,
    worktree: Option<Checkout>,
    default_branch: Option<Compared>,
}

#[derive(Deserialize)]
struct Head {
    committed_at: Option<String>,
}

#[derive(Deserialize)]
struct Checkout {
    path: String,
    #[serde(default)]
    main: bool,
    #[serde(default)]
    changes: Changes,
}

#[derive(Deserialize, Default)]
struct Changes {
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    modified: bool,
    #[serde(default)]
    untracked: bool,
    #[serde(default)]
    renamed: bool,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    conflicted: bool,
}

#[derive(Deserialize)]
struct Compared {
    #[serde(default)]
    ahead: u32,
    #[serde(default)]
    behind: u32,
    integration: Option<serde_json::Value>,
}

/// The reason in an `integration` object; anything the types do not know is `Unknown`, not a failure.
fn reason_of(integration: &serde_json::Value) -> Integrated {
    integration.get("reason").and_then(|r| serde_json::from_value(r.clone()).ok()).unwrap_or(Integrated::Unknown)
}

/// Unix seconds of an RFC 3339 time such as `2026-11-26T08:00:51Z` (a numeric offset is applied);
/// 0 (unknown) when the text is out of range.
fn unix_time(text: &str) -> i64 {
    unix_time_checked(text).unwrap_or(0)
}

fn unix_time_checked(text: &str) -> Option<i64> {
    let num = |s: &str| s.parse::<i64>().unwrap_or(0);
    let (date, time) = text.split_once('T').unwrap_or((text, ""));
    let mut d = date.split('-');
    let (y, m, day) = (num(d.next().unwrap_or("")), num(d.next().unwrap_or("")), num(d.next().unwrap_or("")));
    let split = time.find(['Z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(split);
    let mut c = clock.split(':');
    let (h, mi) = (num(c.next().unwrap_or("")), num(c.next().unwrap_or("")));
    let s = num(c.next().unwrap_or("").split('.').next().unwrap_or(""));
    if !(1..=12).contains(&m)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&h)
        || !(0..60).contains(&mi)
        || !(0..=60).contains(&s)
    {
        return None;
    }
    // Days since 1970-01-01 (civil calendar).
    let y = if m <= 2 { y.checked_sub(1)? } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let days = era.checked_mul(146097)?.checked_add(yoe * 365 + yoe / 4 - yoe / 100 + doy)?.checked_sub(719468)?;
    let offset = match zone.get(..1) {
        Some(sign @ ("+" | "-")) => {
            let (oh, om) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
            let seconds = num(oh).checked_mul(3600)?.checked_add(num(om).checked_mul(60)?)?;
            if sign == "+" { seconds } else { seconds.checked_neg()? }
        }
        _ => 0,
    };
    days.checked_mul(86400)?
        .checked_add(h.checked_mul(3600)?)?
        .checked_add(mi.checked_mul(60)?)?
        .checked_add(s)?
        .checked_sub(offset)
}

fn slashes(path: &str) -> String {
    path.replace('\\', "/")
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| slashes(s.trim_start_matches(r"\\?\")).trim_end_matches('/').to_string();
    let (a, b) = (norm(a), norm(b));
    if cfg!(windows) { a.eq_ignore_ascii_case(&b) } else { a == b }
}

fn parse_list(json: &str) -> Result<ListJson, Error> {
    serde_json::from_str(json).map_err(|e| unexpected("list", e))
}

fn describe(item: &Item) -> Option<Worktree> {
    let checkout = item.worktree.as_ref()?;
    let c = &checkout.changes;
    let compared = if checkout.main { None } else { item.default_branch.as_ref() };
    Some(Worktree {
        path: slashes(&checkout.path),
        branch: item.branch.clone(),
        main: checkout.main,
        dirty: c.staged || c.modified || c.untracked || c.renamed || c.deleted || c.conflicted,
        ahead: compared.map_or(0, |c| c.ahead),
        behind: compared.map_or(0, |c| c.behind),
        integrated: compared.and_then(|c| c.integration.as_ref()).filter(|v| !v.is_null()).map(reason_of),
        at: item.head.as_ref().and_then(|h| h.committed_at.as_deref()).map_or(0, unix_time),
    })
}

fn listing(list: &ListJson) -> Listing {
    let mut worktrees: Vec<Worktree> = list.items.iter().filter_map(describe).collect();
    worktrees.sort_by(|a, b| b.main.cmp(&a.main).then(b.at.cmp(&a.at)));
    Listing { default_branch: list.repo.default_branch.clone(), worktrees }
}

fn fetch(dir: &Path) -> Result<ListJson, Error> {
    if !dir.is_dir() {
        return Err(Error::NotGit(format!("{} is not a folder", dir.display())));
    }
    parse_list(&ok_or_failed(run_tool(dir, &list_args(), &[])?)?)
}

/// Every worktree of the repository `folder` belongs to, with its status.
pub fn list(folder: &str) -> Result<Listing, Error> {
    Ok(listing(&fetch(Path::new(folder))?))
}

#[derive(Deserialize)]
struct Switched {
    action: String,
    branch: String,
    path: String,
    #[serde(default)]
    created_branch: bool,
}

fn parse_switch(json: &str) -> Result<Created, Error> {
    let s: Switched = serde_json::from_str(json).map_err(|e| unexpected("switch", e))?;
    let path = slashes(&s.path);
    // `existing` or `already_at`: the branch already has a worktree, which is not ours to hand out again.
    if s.action != "created" {
        return Err(Error::CheckedOut(path));
    }
    Ok(Created { path, branch: s.branch, new_branch: s.created_branch })
}

/// Git's rules for a branch name (check-ref-format), plus: no leading dash, at most 255 bytes.
fn valid_branch(name: &str) -> bool {
    let bad_char = |c: char| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c);
    !name.is_empty()
        && name.len() <= 255
        && name != "@"
        && !name.starts_with(['-', '/'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name.chars().any(bad_char)
        && name.split('/').all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

/// Makes a worktree for `branch` at `<home>/.h0x/worktrees/<repo>/<branch folder>`: a new branch
/// from `base` (default: the default branch), or the existing branch as it is.
pub fn create(home: &Path, folder: &str, branch: &str, base: Option<&str>) -> Result<Created, Error> {
    let branch = branch.trim();
    if !valid_branch(branch) {
        return Err(Error::Invalid(format!(
            "{:?} is not a valid branch name",
            branch.chars().take(60).collect::<String>()
        )));
    }
    let base = base.map(str::trim).filter(|b| !b.is_empty());
    if base.is_some_and(|b| b.starts_with('-')) {
        return Err(Error::Invalid(format!("base {:?} is not a commit", base.unwrap_or(""))));
    }
    let dir = Path::new(folder);
    if !dir.is_dir() {
        return Err(Error::NotGit(format!("{folder} is not a folder")));
    }
    let root = slashes(&home.display().to_string());
    let place = format!("{root}/.h0x/worktrees/{{{{ repo }}}}/{{{{ branch | sanitize }}}}");
    let env = [("H0X_WT__WORKTREE_PATH", place)];
    let out = run_tool(dir, &switch_args(branch, true, base), &env)?;
    if out.ok {
        return parse_switch(&out.stdout);
    }
    match failed(&out) {
        // The branch exists: check it out as it is.
        Error::Invalid(m) if m.to_lowercase().contains("already exists") => {
            parse_switch(&ok_or_failed(run_tool(dir, &switch_args(branch, false, None), &env)?)?)
        }
        e => Err(e),
    }
}

#[derive(Deserialize)]
struct Removal {
    branch: Option<String>,
    branch_outcome: Option<String>,
}

fn parse_removal(json: &str) -> Result<Removal, Error> {
    let mut all: Vec<Removal> = serde_json::from_str(json).map_err(|e| unexpected("remove", e))?;
    if all.is_empty() { Err(unexpected("remove", "an empty list")) } else { Ok(all.remove(0)) }
}

/// The worktree at `path` and the main checkout's folder (where removals run from).
fn locate(path: &str) -> Result<(Worktree, String, Option<String>), Error> {
    let list = fetch(Path::new(path))?;
    let all = listing(&list);
    let main = all
        .worktrees
        .iter()
        .find(|w| w.main)
        .map(|w| w.path.clone())
        .ok_or_else(|| Error::Failed("no main checkout".into()))?;
    let wt = all
        .worktrees
        .into_iter()
        .find(|w| same_path(&w.path, path))
        .ok_or_else(|| Error::Invalid(format!("{path} is not a worktree of this repository")))?;
    Ok((wt, main, list.repo.default_branch))
}

/// Removes the worktree at `path` (and its branch, when its work is in the default branch).
/// `force` removes it even with uncommitted changes.
pub fn remove(path: &str, force: bool) -> Result<Removed, Error> {
    let (wt, main, _) = locate(path)?;
    remove_located(&wt, &main, force, false)
}

/// Submodules of the checkout at `dir` (nested ones too) holding commits that no remote has.
fn unpushed_submodules(dir: &Path) -> Vec<String> {
    let Some(status) = git(dir, &["submodule", "status", "--recursive"]) else { return vec![] };
    let mut names = vec![];
    for line in status.stdout.lines() {
        // " <sha> <path> (<describe>)"; a leading "-" is a submodule that is not checked out.
        if line.starts_with('-') {
            continue;
        }
        let Some((_, rest)) = line.trim_start_matches(['+', 'U', ' ']).split_once(' ') else { continue };
        let path = rest.rsplit_once(" (").map_or(rest, |(p, _)| p);
        let unpushed = git(&dir.join(path), &["rev-list", "--count", "HEAD", "--not", "--remotes"])
            .and_then(|o| o.stdout.trim().parse::<u32>().ok())
            .unwrap_or(0);
        if unpushed > 0 {
            names.push(path.to_string());
        }
    }
    names
}

fn remove_located(wt: &Worktree, main: &str, force: bool, merged: bool) -> Result<Removed, Error> {
    if wt.main {
        return Err(Error::MainCheckout);
    }
    if !force {
        let names = unpushed_submodules(Path::new(&wt.path));
        if !names.is_empty() {
            return Err(Error::Unpushed(names));
        }
    }
    let mut argv = remove_args(&wt.path, force);
    if merged {
        // Like `h0x-wt merge` itself: the branch is in the target now, whichever branch that is.
        argv.insert(3, "--force-delete".into());
    }
    let json = ok_or_failed(run_tool(Path::new(main), &argv, &[])?)?;
    let removal = parse_removal(&json)?;
    Ok(Removed {
        path: wt.path.clone(),
        branch: removal.branch.or_else(|| wt.branch.clone()),
        branch_deleted: removal.branch_outcome.as_deref() == Some("deleted"),
        integrated: wt.integrated,
    })
}

fn tip(dir: &Path, branch: &str) -> Option<String> {
    git(dir, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .map(|o| o.stdout.trim().to_string())
}

/// After a failed merge: the conflicted files, with the half-done rebase undone and the branch put
/// back on `tip` (a squash may already have rewritten it).
fn abandon(dir: &Path, tip: Option<&str>) -> Vec<String> {
    let files = git(dir, &["diff", "--name-only", "--diff-filter=U"])
        .map(|o| o.stdout.lines().map(str::to_string).collect())
        .unwrap_or_default();
    git(dir, &["rebase", "--abort"]);
    if let Some(tip) = tip {
        git(dir, &["reset", "--hard", "--quiet", tip]);
    }
    files
}

/// Merges the worktree at `path` into `target` (default: the default branch) and removes it, the
/// branch with it. `squash` joins its commits into one; otherwise it is rebased and fast-forwarded.
/// A conflict leaves the branches as they were.
pub fn merge(path: &str, target: Option<&str>, squash: bool) -> Result<Merged, Error> {
    let (wt, _, default) = locate(path)?;
    if wt.main {
        return Err(Error::MainCheckout);
    }
    if wt.branch.is_none() {
        return Err(Error::Invalid("a detached worktree has no branch to merge".into()));
    }
    let target = target.map(str::trim).filter(|t| !t.is_empty());
    let into = target
        .map(str::to_string)
        .or(default)
        .ok_or_else(|| Error::Invalid("no default branch to merge into".into()))?;
    let dir = Path::new(&wt.path);
    let before = tip(dir, &into);
    // Only a clean worktree is reset: uncommitted work is never discarded.
    let own_tip = wt.branch.as_deref().filter(|_| !wt.dirty).and_then(|b| tip(dir, b));
    let out = run_tool(dir, &merge_args(target, squash), &[])?;
    if !out.ok {
        if out.stderr.contains("CONFLICT") || out.stderr.contains("incomplete") {
            return Err(Error::Conflict(abandon(dir, own_tip.as_deref())));
        }
        return Err(failed(&out));
    }
    let merged = tip(dir, &into) != before;
    // Look again: the branch's relation to the default branch changed with the merge.
    let (wt, main, _) = locate(path)?;
    let removed = remove_located(&wt, &main, false, true)?;
    Ok(Merged { target: into, merged, removed })
}
