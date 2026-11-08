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
    // Staged beside it, checked, then swapped in: never a half-written h0x-core.
    crate::platform::swap_in(from, &dest, |staged| version_of(staged).as_deref() == Some(now.as_str())).map_err(
        |e| match e.kind() {
            std::io::ErrorKind::InvalidData => refused(format!("the copy of {} does not run", from.display()), ""),
            _ => RpcError::internal(format!("cannot replace {shown}: {e}")),
        },
    )?;
    let restarted = match client::connect_existing(paths) {
        Some(stream) => {
            client::call(&stream, "server.restart", json!({}))?;
            wait_closed(&stream);
            true
        }
        None => false,
    };
    let status = call(paths, "server.status", json!({}))?;
    let human = format!("{shown}: {was} -> {now}; core running again (pid {}), terminals kept", status["pid"]);
    Ok(Out {
        json: json!({"path": shown, "was": was, "now": now, "restarted": restarted, "pid": status["pid"]}),
        human,
    })
}

#[cfg(test)]
mod tests {
    use super::{older, release_version};

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
}
