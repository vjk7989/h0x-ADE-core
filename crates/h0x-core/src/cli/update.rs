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

/// Replaces `previous_dir` with a copy of the live release: the h0x-core and, on Windows, its ConPTY files.
fn keep_previous(dest: &std::path::Path) -> std::io::Result<()> {
    let (dir, src) = (previous_dir(dest), dest.parent().unwrap_or(dest));
    let _ = std::fs::remove_dir_all(&dir);
    let exe = dest.file_name().unwrap_or_default();
    let conpty = ["conpty/conpty.dll", "conpty/x64/OpenConsole.exe"].into_iter().filter(|_| cfg!(windows));
    for rel in std::iter::once(std::path::Path::new(exe)).chain(conpty.map(std::path::Path::new)) {
        let from = src.join(rel);
        if from.is_file() {
            let to = dir.join(rel);
            std::fs::create_dir_all(to.parent().unwrap_or(&dir))?;
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
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
        last_good_path, local_terminals, older, previous_dir, previous_exe, release_version, rolled_back_body,
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
}
