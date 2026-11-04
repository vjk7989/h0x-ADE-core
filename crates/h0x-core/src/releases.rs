//! Released builds on GitHub Releases: the list (`latest/download/latest.json`,
//! or `download/v<version>/manifest.json` for one version) and its checked
//! downloads. curl, sha256sum/shasum (Windows: its own crypto) and tar do
//! the work: no HTTP or crypto code in h0x-core for something run this rarely.

use crate::rpc::RpcError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Deserialize)]
pub struct Manifest {
    pub version: String,
    pub generation: u32,
    pub targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
pub struct Target {
    pub url: String,
    pub sha256: String,
}

const GITHUB: &str = "https://github.com/vjk7989/h0x-ADE-core/releases";

/// Hosts a download from the default releases site may come from: GitHub and
/// the hosts its release downloads redirect to.
const HOSTS: [&str; 3] = ["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];

/// `H0X_CORE_RELEASES_URL` (a local test site): debug and test builds only.
#[cfg(any(debug_assertions, test))]
fn overridden() -> Option<String> {
    std::env::var("H0X_CORE_RELEASES_URL").ok().filter(|v| !v.trim().is_empty()).map(|v| v.trim_end_matches('/').into())
}

#[cfg(not(any(debug_assertions, test)))]
fn overridden() -> Option<String> {
    None
}

fn base() -> String {
    overridden().unwrap_or_else(|| GITHUB.into())
}

/// Refuses a download link that is not https on GitHub.
fn check_url(url: &str) -> Result<(), RpcError> {
    let host = url.strip_prefix("https://").and_then(|rest| rest.split(['/', '?', '#']).next());
    match host {
        Some(h) if HOSTS.iter().any(|ok| h.eq_ignore_ascii_case(ok)) => Ok(()),
        _ => Err(RpcError::new(
            "refused",
            format!("refusing to download {url:?}: not an https link on GitHub"),
            "the release list is damaged or not from GitHub; try again",
        )),
    }
}

fn unavailable(why: String) -> RpcError {
    RpcError::new(
        "unavailable",
        why,
        "check your connection; the releases are at github.com/vjk7989/h0x-ADE-core/releases",
    )
}

fn curl(url: &str, to: &Path) -> Result<(), RpcError> {
    // A test site set by H0X_CORE_RELEASES_URL (debug builds) is trusted as it
    // is, local http or file included; GitHub and its redirects are https only.
    let pinned = overridden().is_none();
    if pinned {
        check_url(url)?;
    }
    let proto = if pinned { "=https" } else { "=https,http,file" };
    // Quiet: our error says what failed, and piped stderr is one JSON error.
    let ok = Command::new("curl")
        .args(["-fsL", "--proto", proto, "--proto-redir", proto, "--retry", "2", "--connect-timeout", "10", "-o"])
        .arg(to)
        .arg(url)
        .stderr(std::process::Stdio::null())
        .status();
    match ok {
        Ok(s) if s.success() => Ok(()),
        _ => Err(unavailable(format!("cannot download {url}"))),
    }
}

/// The latest release's list, or one version's.
pub fn manifest(version: Option<&str>, scratch: &Path) -> Result<Manifest, RpcError> {
    let url = match version {
        Some(v) => format!("{}/download/v{v}/manifest.json", base()),
        None => format!("{}/latest/download/latest.json", base()),
    };
    let file = scratch.join("manifest.json");
    curl(&url, &file)?;
    let bytes = std::fs::read(&file).map_err(|e| unavailable(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| unavailable(format!("{url} is not a release list: {e}")))
}

/// Windows' own tar opens zips; Git Bash's GNU tar, often first on PATH there, can't.
fn tar_program() -> PathBuf {
    match std::env::var_os("SystemRoot") {
        Some(root) if cfg!(windows) => Path::new(&root).join(r"System32\tar.exe"),
        _ => "tar".into(),
    }
}

#[cfg(windows)]
fn sha256(file: &Path) -> Option<String> {
    crate::platform::sha256(&std::fs::read(file).ok()?)
}

#[cfg(unix)]
fn sha256(file: &Path) -> Option<String> {
    let run =
        |cmd: &str, args: &[&str]| Command::new(cmd).args(args).arg(file).output().ok().filter(|o| o.status.success());
    let out = run("sha256sum", &[]).or_else(|| run("shasum", &["-a", "256"]))?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(String::from)
}

/// Downloads `m`'s build for `target` (`linux-x86_64`) into `scratch`,
/// checks it against the list, unpacks it: the path of its h0x-core.
/// Windows builds are a zip (the exe and its ConPTY); Windows' own tar
/// unpacks it.
pub fn download(m: &Manifest, target: &str, scratch: &Path) -> Result<PathBuf, RpcError> {
    let tar = fetch(m, target, scratch)?;
    let exe = if target.starts_with("windows") { "h0x-core.exe" } else { "h0x-core" };
    let ok =
        Command::new(tar_program()).arg("-xf").arg(&tar).arg("-C").arg(scratch).status().is_ok_and(|s| s.success());
    let bin = scratch.join(exe);
    if !ok || !bin.is_file() {
        return Err(RpcError::new(
            "refused",
            format!("h0x-core {}'s download for {target} holds no h0x-core binary", m.version),
            "",
        ));
    }
    Ok(bin)
}

/// Downloads `m`'s archive for `target` into `scratch` and checks it against
/// the list: its path, still packed.
pub fn fetch(m: &Manifest, target: &str, scratch: &Path) -> Result<PathBuf, RpcError> {
    let t = m.targets.get(target).ok_or_else(|| {
        RpcError::new(
            "refused",
            format!("h0x-core {} has no build for {target}", m.version),
            "install h0x-core there yourself",
        )
    })?;
    let archive = if target.starts_with("windows") { "zip" } else { "tar.gz" };
    let file = scratch.join(format!("h0x-core-{target}.{archive}"));
    curl(&t.url, &file)?;
    if sha256(&file).as_deref() != Some(t.sha256.as_str()) {
        return Err(RpcError::new(
            "refused",
            format!("the download of h0x-core {} for {target} is damaged (checksum mismatch)", m.version),
            "try again",
        ));
    }
    Ok(file)
}

/// A private folder for one download, removed when dropped.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Result<Scratch, RpcError> {
        let dir = std::env::temp_dir().join(format!(
            "h0x-core-download-{}-{}",
            std::process::id(),
            crate::platform::random_hex()
        ));
        std::fs::create_dir_all(&dir).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// This machine as release targets name it: `linux-x86_64`, `macos-aarch64`.
pub fn this_target() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_release_base_drops_a_trailing_slash_and_defaults_to_github() {
        // Only this test reads H0X_CORE_RELEASES_URL inside the unit-test binary.
        unsafe { std::env::set_var("H0X_CORE_RELEASES_URL", "https://mirror.example/releases/") };
        assert_eq!(super::base(), "https://mirror.example/releases");
        unsafe { std::env::set_var("H0X_CORE_RELEASES_URL", " ") };
        assert_eq!(super::base(), "https://github.com/vjk7989/h0x-ADE-core/releases", "empty counts as unset");
        unsafe { std::env::remove_var("H0X_CORE_RELEASES_URL") };
        assert_eq!(super::base(), "https://github.com/vjk7989/h0x-ADE-core/releases");
    }

    #[test]
    fn only_https_links_on_github_are_downloaded_from_the_default_site() {
        for ok in [
            "https://github.com/vjk7989/h0x-ADE-core/releases/download/v1.0.0/h0x-core-linux-x86_64.tar.gz",
            "https://objects.githubusercontent.com/x?sig=a",
            "https://release-assets.githubusercontent.com/x#y",
            "https://GitHub.com/vjk7989/h0x-ADE-core/releases",
        ] {
            assert!(super::check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "http://github.com/vjk7989/h0x-ADE-core/releases",
            "https://evil.com/h0x-core.tar.gz",
            "https://github.com.evil.com/h0x-core.tar.gz",
            "https://evil.com/github.com",
            "https://github.com@evil.com/x",
            "https://github.com:8443/x",
            "file:///etc/passwd",
            "github.com/x",
        ] {
            let err = super::check_url(bad).unwrap_err();
            assert_eq!(err.kind, "refused", "{bad}");
            assert!(err.message.contains("not an https link on GitHub"), "{bad}: {}", err.message);
        }
    }

    #[test]
    fn an_unknown_target_is_refused_before_any_download() {
        let m = super::Manifest { version: "1.2.3".into(), generation: 0, targets: Default::default() };
        let scratch = std::env::temp_dir();
        let err = super::fetch(&m, "plan9-mips", &scratch).unwrap_err();
        assert_eq!(err.kind, "refused");
        assert_eq!(err.message, "h0x-core 1.2.3 has no build for plan9-mips");
        assert_eq!(err.hint, "install h0x-core there yourself");
    }
}
