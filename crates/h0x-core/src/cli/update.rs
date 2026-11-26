//! `h0x-core update`: swap this h0x-core for another build, keep the terminals running.

use super::*;

/// What a binary says it is (`h0x-core 0.2.0`), if it is a working h0x-core.
pub(super) fn version_of(path: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new(path).arg("--version").stdin(std::process::Stdio::null()).output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && v.starts_with("h0x-core ")).then_some(v)
}

/// A release name for `--version`: `0.1.4` (it goes into a URL).
pub(super) fn release_version(s: &str) -> Result<String, String> {
    let parts: Vec<&str> = s.split('.').collect();
    let number = |p: &&str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if parts.len() == 3 && parts.iter().all(number) {
        Ok(s.into())
    } else {
        Err(format!("expected a release like 0.1.4, got {s:?}"))
    }
}

/// `h0x-core 0.1.9` is older than `h0x-core 0.2.0`; anything unparsed is not older.
pub(super) fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Option<Vec<u64>> {
        v.strip_prefix("h0x-core ")?.split(['.', '-']).take(3).map(|p| p.parse().ok()).collect()
    };
    matches!((parts(a), parts(b)), (Some(a), Some(b)) if a < b)
}

/// The installed binary can be current while a core started before it was
/// replaced is still running. Do not start a core just to check its version.
pub(super) fn up_to_date(paths: &Paths, release: &str, human: String) -> Result<Out, RpcError> {
    let this = format!("h0x-core {}", env!("CARGO_PKG_VERSION"));
    let mut out = Out { json: json!({"release": release, "now": this, "updated": false}), human };
    if let Some(stream) = client::connect_existing(paths) {
        let status = client::call(&stream, "server.status", json!({}))?;
        if older(&format!("h0x-core {}", status["version"].as_str().unwrap_or_default()), &this) {
            client::call(&stream, "server.restart", json!({}))?;
            wait_closed(&stream);
            let status = call(paths, "server.status", json!({}))?;
            out.json["restarted"] = true.into();
            out.json["pid"] = status["pid"].clone();
            out.human.push_str(&format!("; older core restarted (pid {}), terminals kept", status["pid"]));
        }
    }
    Ok(out)
}

/// Swaps the binary this h0x-core runs from for `from` (a binary whose release
/// signature was already checked, see `releases::from_file`), then restarts the core
/// the planned way: every terminal keeps running. One swap path, for people
/// and for apps that bundle h0x-core.
pub(super) fn update(paths: &Paths, from: &std::path::Path, force: bool) -> Result<Out, RpcError> {
    let refused = |why: String, hint: &str| RpcError::new("refused", why, hint);
    let dest = std::env::current_exe().and_then(|e| e.canonicalize()).map_err(|e| RpcError::internal(e.to_string()))?;
    let shown = dest.display().to_string();
    if let Some(manager) = client::installed_by() {
        let reinstall = "reinstall h0x-core from the GitHub release: https://github.com/vjk7989/h0x-ADE-core/releases";
        return Err(match manager {
            "nix" => refused(format!("{shown} belongs to nix"), "update it in your Nix configuration"),
            "system package manager" => {
                refused(format!("{} isn't writable by you", dest.parent().unwrap().display()), reinstall)
            }
            _ => refused(format!("{shown} belongs to {manager}"), reinstall),
        });
    }
    let was = format!("h0x-core {}", env!("CARGO_PKG_VERSION"));
    let now = version_of(from).ok_or_else(|| refused(format!("{} is not a working h0x-core", from.display()), ""))?;
    if older(&now, &was) && !force {
        return Err(refused(format!("{now} is older than this {was}"), "--force to go back anyway"));
    }
    // The running h0x-core's terminals (finished ones too), to check they all survive the handoff.
    let before = client::connect_existing(paths).map(|s| terminals_of(&s)).transpose()?;
    let kept = std::cell::RefCell::new(None);
    // Staged beside it, checked, then (only then) the binary being replaced is kept, then swapped in:
    // never a half-written h0x-core, and a refused update leaves the old `previous` alone.
    crate::platform::swap_in(from, &dest, |staged| {
        if version_of(staged).as_deref() != Some(now.as_str()) {
            return false;
        }
        *kept.borrow_mut() = keep_previous(&dest).err();
        kept.borrow().is_none()
    })
    .map_err(|e| match (kept.take(), e.kind()) {
        (Some(why), _) => {
            RpcError::internal(format!("cannot keep {shown} as {}: {why}", previous_dir(&dest).display()))
        }
        (None, std::io::ErrorKind::InvalidData) => refused(format!("the copy of {} does not run", from.display()), ""),
        _ => RpcError::internal(format!("cannot replace {shown}: {e}")),
    })?;
    let restarted = before.is_some();
    let healthy = match &before {
        Some(ids) => restart_and_check(paths, &now, ids),
        // No core runs: none is started, the binary is all there is to check.
        None => binary_says(&dest, &now),
    };
    if let Err(why) = healthy {
        crate::platform::swap_in(&previous_exe(&dest), &dest, |p| version_of(p).as_deref() == Some(was.as_str()))
            .map_err(|e| RpcError::internal(format!("update failed ({why}) and {was} could not be put back: {e}")))?;
        let mut message = format!("Update to {now} failed, went back to {was}: {why}");
        let handed = if restarted {
            hand_off(paths).and_then(|_| call(paths, "server.status", json!({})))
        } else {
            Ok(json!({}))
        };
        match handed {
            Err(e) => message.push_str(&format!(" (the core needs a restart: {})", e.message)),
            Ok(_) if !restarted => {
                if let Err(why) = binary_says(&dest, &was) {
                    message.push_str(&format!(" ({why})"));
                }
            }
            Ok(_) => {}
        }
        return Err(RpcError::new("rolled_back", message, ""));
    }
    let pid = if restarted { call(paths, "server.status", json!({}))?["pid"].clone() } else { Value::Null };
    let version = now.strip_prefix("h0x-core ").unwrap_or(&now);
    std::fs::write(last_good_path(&dest), json!({"version": version}).to_string())
        .map_err(|e| RpcError::internal(format!("cannot write last-good.json: {e}")))?;
    let human = if restarted {
        format!("{shown}: {was} -> {now}; core running again (pid {pid}), terminals kept")
    } else {
        format!("{shown}: {was} -> {now}; no core was running")
    };
    Ok(Out {
        json: json!({"path": shown, "was": was, "now": now, "restarted": restarted, "pid": pid, "rolled_back": false}),
        human,
    })
}

/// Where the replaced h0x-core (and, on Windows, its ConPTY files) is kept: a folder beside the install.
fn previous_dir(dest: &std::path::Path) -> std::path::PathBuf {
    dest.with_file_name("previous")
}

/// The h0x-core inside `previous_dir`: what a rollback installs.
fn previous_exe(dest: &std::path::Path) -> std::path::PathBuf {
    previous_dir(dest).join(dest.file_name().unwrap_or_default())
}

/// Written beside the install once an update's core passed its health check.
fn last_good_path(dest: &std::path::Path) -> std::path::PathBuf {
    dest.with_file_name("last-good.json")
}

/// What `--json` prints when an update was rolled back.
pub(super) fn rolled_back_body(message: &str) -> Value {
    json!({"rolled_back": true, "error": message})
}

/// Replaces `previous_dir` with a copy of the live release: the h0x-core, `h0x-core-tools/h0x-wt` and, on Windows, its ConPTY files.
fn keep_previous(dest: &std::path::Path) -> std::io::Result<()> {
    let (dir, src) = (previous_dir(dest), dest.parent().unwrap_or(dest));
    clear_previous(&dir)?;
    let exe = dest.file_name().unwrap_or_default();
    let conpty = ["conpty/conpty.dll", "conpty/x64/OpenConsole.exe"].into_iter().filter(|_| cfg!(windows));
    let tool = if cfg!(windows) { "h0x-core-tools/h0x-wt.exe" } else { "h0x-core-tools/h0x-wt" };
    for rel in std::iter::once(std::path::Path::new(exe)).chain(conpty.chain([tool]).map(std::path::Path::new)) {
        let from = src.join(rel);
        if std::fs::symlink_metadata(&from).is_ok_and(|m| m.file_type().is_file()) {
            let to = dir.join(rel);
            std::fs::create_dir_all(to.parent().unwrap_or(&dir))?;
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Removes an earlier `previous` folder, but only one that holds nothing except what is put there
/// (h0x-core / h0x-core.exe, a `conpty` folder and an `h0x-core-tools` folder): the install folder may be shared (`~/.local/bin`),
/// and a foreign folder named `previous` is left alone with an error.
fn clear_previous(dir: &std::path::Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let refuse = |inside: &std::path::Path, name: &std::ffi::OsStr| {
        Err(std::io::Error::other(format!(
            "{} holds {}, which h0x-core did not put there; move or remove that folder",
            inside.display(),
            name.to_string_lossy()
        )))
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        // The names h0x-core puts in each folder; anything else, at any depth, is foreign.
        let (is_dir, inside): (bool, &[&str]) = match name.to_str() {
            Some("h0x-core" | "h0x-core.exe") => (false, &[]),
            Some("conpty") => (true, &["conpty.dll", "x64"]),
            Some("h0x-core-tools") => (true, &["h0x-wt", "h0x-wt.exe"]),
            _ => return refuse(dir, &name),
        };
        if entry.file_type()?.is_dir() != is_dir || (!is_dir && !entry.file_type()?.is_file()) {
            return refuse(dir, &name);
        }
        if is_dir {
            for inner in std::fs::read_dir(entry.path())? {
                let inner = inner?;
                let kind = inner.file_type()?;
                let ours = match inner.file_name().to_str() {
                    Some("x64") => kind.is_dir() && name == "conpty",
                    Some(f) if inside.contains(&f) => kind.is_file(),
                    _ => false,
                };
                if !ours {
                    return refuse(&entry.path(), &inner.file_name());
                }
                if kind.is_dir() {
                    for deep in std::fs::read_dir(inner.path())? {
                        let deep = deep?;
                        if deep.file_name() != "OpenConsole.exe" || !deep.file_type()?.is_file() {
                            return refuse(&inner.path(), &deep.file_name());
                        }
                    }
                }
            }
        }
    }
    std::fs::remove_dir_all(dir)
}

/// A binary that runs and says it is `release`.
fn binary_says(path: &std::path::Path, release: &str) -> Result<(), String> {
    match version_of(path) {
        Some(v) if v == release => Ok(()),
        _ => Err(format!("{} stopped answering --version", path.display())),
    }
}

/// The planned handoff: the running core starts the installed binary and keeps every terminal.
fn hand_off(paths: &Paths) -> Result<(), RpcError> {
    if let Some(stream) = client::connect_existing(paths) {
        client::call(&stream, "server.restart", json!({}))?;
        wait_closed(&stream);
    }
    Ok(())
}

/// This core's own terminals (finished ones too; other machines' carry `machine/` in front).
fn local_terminals(list: &Value) -> Vec<String> {
    let all = list["terminals"].as_array().into_iter().flatten();
    all.filter_map(|t| t["address"].as_str()).filter(|a| !a.contains('/')).map(Into::into).collect()
}

fn terminals_of(stream: &crate::platform::Stream) -> Result<Vec<String>, RpcError> {
    Ok(local_terminals(&client::call(stream, "terminal.list", json!({"all": true}))?))
}

/// Hands the running core over to the binary now installed, then waits (30 s for a core to answer)
/// for it to report `now`: a wrong version fails at once, a missing terminal after 5 s.
fn restart_and_check(paths: &Paths, now: &str, before: &[String]) -> Result<(), String> {
    hand_off(paths).map_err(|e| e.message)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut missing_since: Option<std::time::Instant> = None;
    let mut last = "no core answered".to_string();
    while std::time::Instant::now() < deadline {
        // A core that stopped for the handoff and has nobody to start it is started here, as `update` always did.
        if let Ok(stream) = client::connect_or_start(paths) {
            let answer = client::call(&stream, "server.status", json!({})).and_then(|s| {
                let running = format!("h0x-core {}", s["version"].as_str().unwrap_or_default());
                if running != now {
                    return Ok(Err(Ok(format!("the core reports {running}"))));
                }
                let have = terminals_of(&stream)?;
                Ok(before.iter().find(|t| !have.contains(t)).cloned().map_or(Ok(()), |lost| Err(Err(lost))))
            });
            match answer {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(Ok(wrong))) => return Err(wrong),
                Ok(Err(Err(lost))) => {
                    let since = *missing_since.get_or_insert_with(std::time::Instant::now);
                    last = format!("terminal {lost} is gone");
                    if since.elapsed() >= std::time::Duration::from_secs(5) {
                        return Err(last);
                    }
                }
                Err(e) => last = e.message,
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::{
        clear_previous, keep_previous, last_good_path, local_terminals, older, previous_dir, previous_exe,
        release_version, rolled_back_body,
    };
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn a_release_name_is_three_numbers_with_no_v() {
        assert_eq!(release_version("0.1.4").as_deref(), Ok("0.1.4"));
        for bad in ["v0.1.4", "", "0.1", "0.1.4.5", "0.1.x", "0..4"] {
            assert!(release_version(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_a_versioned_h0x_core_is_older_than_another() {
        assert!(older("h0x-core 0.1.9", "h0x-core 0.2.0"));
        assert!(!older("h0x-core 0.2.0", "h0x-core 0.2.0"));
        assert!(!older("hc 0.1.0", "h0x-core 0.2.0"), "the old name is not a version at all");
        assert!(!older("h0x-core 0.1.0", "h0x-core x.y.z"));
    }

    #[test]
    fn the_previous_release_and_last_good_sit_beside_the_install_even_in_a_folder_with_spaces() {
        let dest = Path::new("C:/Program Files/H0X Core/h0x-core.exe");
        assert_eq!(previous_dir(dest), Path::new("C:/Program Files/H0X Core/previous"));
        assert_eq!(previous_exe(dest), Path::new("C:/Program Files/H0X Core/previous/h0x-core.exe"));
        assert_eq!(last_good_path(dest), Path::new("C:/Program Files/H0X Core/last-good.json"));
        let unix = Path::new("/opt/h0x/h0x-core");
        assert_eq!(previous_exe(unix), Path::new("/opt/h0x/previous/h0x-core"));
    }

    #[test]
    fn a_rolled_back_update_says_so_in_json() {
        assert_eq!(rolled_back_body("x failed"), json!({"rolled_back": true, "error": "x failed"}));
    }

    #[test]
    fn only_this_cores_own_terminals_count_finished_ones_included() {
        let list = json!({"terminals": [
            {"address": "t1", "state": "live"},
            {"address": "t2", "state": "finished"},
            {"address": "box/t1", "state": "live"},
            {"state": "live"},
        ]});
        assert_eq!(local_terminals(&list), ["t1", "t2"]);
        assert!(local_terminals(&json!({})).is_empty());
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("h0x-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_or_empty_previous_folder_is_fine() {
        let dir = scratch("empty");
        assert!(clear_previous(&dir.join("previous")).is_ok());
        std::fs::create_dir(dir.join("previous")).unwrap();
        assert!(clear_previous(&dir.join("previous")).is_ok());
        assert!(!dir.join("previous").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_previous_folder_with_only_our_files_is_replaced() {
        let dir = scratch("ours");
        let prev = dir.join("previous");
        std::fs::create_dir_all(prev.join("conpty/x64")).unwrap();
        std::fs::write(prev.join("h0x-core"), "old").unwrap();
        std::fs::write(prev.join("h0x-core.exe"), "old").unwrap();
        std::fs::write(prev.join("conpty/conpty.dll"), "old").unwrap();
        std::fs::write(dir.join("h0x-core"), "live").unwrap();
        keep_previous(&dir.join("h0x-core")).unwrap();
        assert_eq!(std::fs::read_to_string(prev.join("h0x-core")).unwrap(), "live");
        assert!(!prev.join("h0x-core.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn h0x_wt_is_kept_in_previous_with_the_core_and_dropped_when_the_live_release_has_none() {
        let dir = scratch("tools");
        let tool = if cfg!(windows) { "h0x-core-tools/h0x-wt.exe" } else { "h0x-core-tools/h0x-wt" };
        std::fs::create_dir_all(dir.join("h0x-core-tools")).unwrap();
        std::fs::write(dir.join(tool), "wt 1").unwrap();
        std::fs::write(dir.join("h0x-core"), "live").unwrap();
        keep_previous(&dir.join("h0x-core")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("previous").join(tool)).unwrap(), "wt 1");
        // The next release was installed from an archive without the folder: nothing stale is kept.
        std::fs::remove_dir_all(dir.join("h0x-core-tools")).unwrap();
        keep_previous(&dir.join("h0x-core")).unwrap();
        assert!(!dir.join("previous/h0x-core-tools").exists());
        assert!(dir.join("previous/h0x-core").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_h0x_core_tools_entry_in_previous_is_refused_like_any_other() {
        let dir = scratch("tools-foreign");
        let prev = dir.join("previous");
        std::fs::create_dir_all(&prev).unwrap();
        std::fs::write(prev.join("h0x-core-tools"), "a file, not our folder").unwrap();
        std::fs::write(dir.join("h0x-core"), "live").unwrap();
        let err = keep_previous(&dir.join("h0x-core")).unwrap_err();
        assert!(err.to_string().contains("did not put there"), "{err}");
        assert_eq!(std::fs::read_to_string(prev.join("h0x-core-tools")).unwrap(), "a file, not our folder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_previous_folder_is_refused_and_left_alone() {
        for foreign in ["notes.txt", "h0x-core.bak", "other/deep.txt"] {
            let dir = scratch("foreign");
            let prev = dir.join("previous");
            std::fs::create_dir_all(prev.join("conpty")).unwrap();
            std::fs::write(prev.join("h0x-core"), "old").unwrap();
            let file = prev.join(foreign);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "mine").unwrap();
            std::fs::write(dir.join("h0x-core"), "live").unwrap();
            let err = keep_previous(&dir.join("h0x-core")).unwrap_err();
            assert!(err.to_string().contains("did not put there"), "{foreign}: {err}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine", "{foreign}");
            assert_eq!(std::fs::read_to_string(prev.join("h0x-core")).unwrap(), "old", "{foreign}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_previous_that_is_a_file_or_a_folder_named_like_our_binary_is_refused() {
        let dir = scratch("odd");
        std::fs::write(dir.join("previous"), "a file").unwrap();
        assert!(clear_previous(&dir.join("previous")).is_err());
        assert!(dir.join("previous").is_file());
        std::fs::remove_file(dir.join("previous")).unwrap();
        std::fs::create_dir_all(dir.join("previous/h0x-core")).unwrap();
        assert!(clear_previous(&dir.join("previous")).is_err());
        assert!(dir.join("previous/h0x-core").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- edge cases (WT-S1 test writer) ----------------------------------------------------

    const EXE: &str = if cfg!(windows) { "h0x-core.exe" } else { "h0x-core" };
    const TOOL: &str = if cfg!(windows) { "h0x-core-tools/h0x-wt.exe" } else { "h0x-core-tools/h0x-wt" };

    /// A release folder in `dir`: the core saying `core`, and h0x-wt saying `tool` when given.
    fn release_in(dir: &Path, core: &str, tool: Option<&str>) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(EXE), core).unwrap();
        if let Some(tool) = tool {
            std::fs::create_dir_all(dir.join("h0x-core-tools")).unwrap();
            std::fs::write(dir.join(TOOL), tool).unwrap();
        }
        dir.join(EXE)
    }

    /// An update of the install in `live` to `release` as `update` does it: the old release is kept
    /// in `previous` from inside the check, then swapped. The install's h0x-core.
    fn update_to(live: &Path, release: &Path) -> std::path::PathBuf {
        let dest = live.join(EXE);
        crate::platform::swap_in(release, &dest, |_| keep_previous(&dest).is_ok()).unwrap();
        dest
    }

    #[test]
    fn with_h0x_wt_to_with_h0x_wt_and_back_restores_the_old_h0x_wt() {
        let root = scratch("wt-both");
        let live = root.join("live");
        release_in(&live, "old core", Some("old wt"));
        let new = release_in(&root.join("new"), "new core", Some("new wt"));
        let dest = update_to(&live, &new);
        assert_eq!(std::fs::read_to_string(live.join(TOOL)).unwrap(), "new wt");
        assert_eq!(std::fs::read_to_string(live.join("previous").join(TOOL)).unwrap(), "old wt");
        crate::platform::swap_in(&previous_exe(&dest), &dest, |_| true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old core");
        assert_eq!(
            std::fs::read_to_string(live.join(TOOL)).unwrap(),
            "old wt",
            "the rollback puts the old h0x-wt back"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn without_h0x_wt_to_with_h0x_wt_keeps_none_in_previous_and_a_rollback_leaves_no_stale_h0x_wt() {
        let root = scratch("wt-none-then-some");
        let live = root.join("live");
        release_in(&live, "old core", None);
        let new = release_in(&root.join("new"), "new core", Some("new wt"));
        let dest = update_to(&live, &new);
        assert_eq!(std::fs::read_to_string(live.join(TOOL)).unwrap(), "new wt");
        assert!(!live.join("previous/h0x-core-tools").exists(), "the old release had no h0x-wt to keep");
        crate::platform::swap_in(&previous_exe(&dest), &dest, |_| true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old core");
        // The release rolled back to shipped no h0x-wt: the new one must not stay behind (and keep its shim alive).
        assert!(!live.join(TOOL).exists(), "a stale h0x-wt survives the rollback to a core without it");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn with_h0x_wt_to_without_keeps_it_in_previous_and_a_rollback_still_has_it() {
        let root = scratch("wt-some-then-none");
        let live = root.join("live");
        release_in(&live, "old core", Some("old wt"));
        let new = release_in(&root.join("new"), "new core", None);
        let dest = update_to(&live, &new);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new core");
        assert_eq!(std::fs::read_to_string(live.join("previous").join(TOOL)).unwrap(), "old wt");
        crate::platform::swap_in(&previous_exe(&dest), &dest, |_| true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old core");
        assert_eq!(std::fs::read_to_string(live.join(TOOL)).unwrap(), "old wt");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_live_h0x_core_tools_that_is_a_file_or_an_h0x_wt_that_is_a_folder_is_not_kept() {
        let dir = scratch("wt-odd-live");
        std::fs::write(dir.join(EXE), "live").unwrap();
        std::fs::write(dir.join("h0x-core-tools"), "a file, not the folder").unwrap();
        keep_previous(&dir.join(EXE)).unwrap();
        assert!(dir.join("previous").join(EXE).is_file());
        assert!(!dir.join("previous/h0x-core-tools").exists());

        std::fs::remove_file(dir.join("h0x-core-tools")).unwrap();
        std::fs::create_dir_all(dir.join(TOOL)).unwrap(); // a folder named like the tool
        keep_previous(&dir.join(EXE)).unwrap();
        assert!(!dir.join("previous").join(TOOL).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_update_into_an_install_whose_h0x_core_tools_is_a_file_fails_and_keeps_the_core() {
        let root = scratch("wt-tools-file");
        let live = root.join("live");
        release_in(&live, "old core", None);
        std::fs::write(live.join("h0x-core-tools"), "a file, not the folder").unwrap();
        let new = release_in(&root.join("new"), "new core", Some("new wt"));
        assert!(crate::platform::swap_in(&new, &live.join(EXE), |_| true).is_err());
        assert_eq!(std::fs::read_to_string(live.join(EXE)).unwrap(), "old core", "no half update");
        assert_eq!(std::fs::read_to_string(live.join("h0x-core-tools")).unwrap(), "a file, not the folder");
        let left: Vec<String> = std::fs::read_dir(&live)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".new"))
            .collect();
        assert!(left.is_empty(), "staged copies left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn h0x_wt_is_kept_in_previous_in_a_folder_with_spaces_and_unicode() {
        let root = scratch("wt-unicode");
        let live = root.join("Program Files").join("H0X Core \u{00fc}\u{65e5}\u{672c}");
        release_in(&live, "live", Some("wt"));
        keep_previous(&live.join(EXE)).unwrap();
        assert_eq!(std::fs::read_to_string(live.join("previous").join(TOOL)).unwrap(), "wt");
        keep_previous(&live.join(EXE)).unwrap(); // our own previous/h0x-core-tools is replaced, not refused
        assert_eq!(std::fs::read_to_string(live.join("previous").join(TOOL)).unwrap(), "wt");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `previous/h0x-core-tools` is a folder, but holds something h0x-core never put there: refused,
    /// as a foreign entry at the top of `previous` is, and not deleted.
    #[test]
    fn a_foreign_file_inside_previous_h0x_core_tools_is_refused_and_left_alone() {
        let dir = scratch("wt-foreign-inside");
        let prev = dir.join("previous");
        std::fs::create_dir_all(prev.join("h0x-core-tools")).unwrap();
        std::fs::write(prev.join("h0x-core-tools/notes.txt"), "mine").unwrap();
        std::fs::write(dir.join(EXE), "live").unwrap();
        assert!(keep_previous(&dir.join(EXE)).is_err(), "a foreign file inside h0x-core-tools is deleted");
        assert_eq!(std::fs::read_to_string(prev.join("h0x-core-tools/notes.txt")).unwrap(), "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A release whose h0x-wt is a symlink (to anything) is not installed through it: install.sh
    /// refuses one too.
    #[cfg(unix)]
    #[test]
    fn a_release_whose_h0x_wt_is_a_symlink_does_not_install_it() {
        let root = scratch("wt-symlink");
        let live = root.join("live");
        release_in(&live, "old core", None);
        let elsewhere = root.join("elsewhere");
        std::fs::write(&elsewhere, "not the tool").unwrap();
        let new = release_in(&root.join("new"), "new core", None);
        std::fs::create_dir_all(root.join("new/h0x-core-tools")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("new").join(TOOL)).unwrap();
        crate::platform::swap_in(&new, &live.join(EXE), |_| true).unwrap();
        assert_eq!(std::fs::read_to_string(live.join(EXE)).unwrap(), "new core");
        assert!(!live.join(TOOL).exists(), "the symlink's target was installed as h0x-wt");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A running h0x-wt.exe can't be overwritten but can be renamed away: the update still lands.
    #[cfg(windows)]
    #[test]
    fn on_windows_a_running_h0x_wt_exe_is_renamed_away_by_the_update() {
        use std::os::windows::process::CommandExt;
        let root = scratch("wt-running");
        let live = root.join("live");
        release_in(&live, "old core", None);
        std::fs::create_dir_all(live.join("h0x-core-tools")).unwrap();
        let cmd = Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("cmd.exe");
        std::fs::copy(cmd, live.join(TOOL)).unwrap();
        let mut child = std::process::Command::new(live.join(TOOL))
            .args(["/d", "/q", "/k"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .unwrap();
        let new = release_in(&root.join("new"), "new core", Some("new wt"));
        let swapped = crate::platform::swap_in(&new, &live.join(EXE), |_| true);
        let still = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        swapped.unwrap();
        assert!(still, "the running h0x-wt was not disturbed");
        assert_eq!(std::fs::read_to_string(live.join(TOOL)).unwrap(), "new wt");
        assert_eq!(std::fs::read_to_string(live.join(EXE)).unwrap(), "new core");
        let _ = std::fs::remove_dir_all(&root);
    }
}
