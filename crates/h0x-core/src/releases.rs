//! Released builds on GitHub Releases: the list (`latest/download/latest.json`,
//! or `download/v<version>/manifest.json` for one version) and its checked
//! downloads. curl, sha256sum/shasum (Windows: its own crypto) and tar do
//! the work; every file used must carry the release key's minisign signature.

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

/// The public half of the key CI signs every release asset with (key id 29957E064EAB76C4).
const PUBLIC_KEY: &str = "RWTEdqtOBn6VKQ/x9x3qWk/gIWUb/I1OlZXsTlHyrkp9Yq30qBki+tdt";

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

/// Downloads `url` to `to`, then `<url>.minisig`, and keeps the file only if
/// the signature is the release key's. A test site (H0X_CORE_RELEASES_URL,
/// debug and test builds only) is not signed.
fn curl_signed(url: &str, to: &Path) -> Result<(), RpcError> {
    curl(url, to)?;
    if overridden().is_some() {
        return Ok(());
    }
    let sig = minisig(to);
    // A signature that does not download is refused below as missing.
    let _ = curl(&format!("{url}.minisig"), &sig);
    verify(to, &sig, PUBLIC_KEY, url.rsplit('/').next().unwrap_or(url))
}

fn minisig(file: &Path) -> PathBuf {
    let mut sig = file.as_os_str().to_owned();
    sig.push(".minisig");
    sig.into()
}

/// Checks `file` against its minisign signature `sig` by `key`, made for the
/// release asset `name` (its trusted comment's `file:`, so one signed file
/// cannot stand in for another); on any failure both are deleted and the
/// download is refused.
fn verify(file: &Path, sig: &Path, key: &str, name: &str) -> Result<(), RpcError> {
    let checked = (|| {
        let key = minisign_verify::PublicKey::from_base64(key).map_err(|e| e.to_string())?;
        let text = std::fs::read_to_string(sig).map_err(|_| "it has no signature".to_string())?;
        let signature =
            minisign_verify::Signature::decode(&text).map_err(|e| format!("its signature is unreadable: {e}"))?;
        let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
        key.verify(&bytes, &signature, false).map_err(|e| format!("its signature does not match: {e}"))?;
        let file_field = format!("file:{name}");
        if signature.trusted_comment().split('\t').any(|f| f == file_field) {
            Ok(())
        } else {
            Err(format!("its signature is for another file, not {name}"))
        }
    })();
    let _ = std::fs::remove_file(sig);
    checked.map_err(|why| {
        let _ = std::fs::remove_file(file);
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        RpcError::new(
            "refused",
            format!("the download {name} is not signed by the h0x-core release key: {why}"),
            "the download may have been tampered with; try again later",
        )
    })
}

/// The latest release's list, or one version's.
pub fn manifest(version: Option<&str>, scratch: &Path) -> Result<Manifest, RpcError> {
    let url = match version {
        Some(v) => format!("{}/download/v{v}/manifest.json", base()),
        None => format!("{}/latest/download/latest.json", base()),
    };
    let file = scratch.join("manifest.json");
    curl_signed(&url, &file)?;
    let bytes = std::fs::read(&file).map_err(|e| unavailable(e.to_string()))?;
    parse(&bytes, version, &url)
}

/// Reads the list from `url`; one asked for by `version` must be that
/// version's, so an older signed list served in its place is refused.
fn parse(bytes: &[u8], version: Option<&str>, url: &str) -> Result<Manifest, RpcError> {
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|e| unavailable(format!("{url} is not a release list: {e}")))?;
    match version {
        Some(v) if m.version != v => Err(RpcError::new(
            "refused",
            format!("{url} lists h0x-core {}, not the requested {v}", m.version),
            "the release list may be an old one served in its place; try again later",
        )),
        _ => Ok(m),
    }
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
    curl_signed(&t.url, &file)?;
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

    /// A throwaway test key pair (not the release key), its signature of
    /// `PAYLOAD`, and the same payload signed by a second throwaway key; made
    /// with rsign2 (`generate -W`, `sign -W -t`), trusted comments the way
    /// `minisign -S` writes them.
    const TEST_KEY: &str = "RWTHiUHUIZ1i3u/akRYT+UiUTYEqrBvGkGLpy2lzDfps3RwUxtdUXTKO";
    const PAYLOAD: &str = "h0x-core test release\n";
    const NAME: &str = "h0x-core-linux-x86_64.tar.gz";
    const SIG: &str = "untrusted comment: signature from rsign secret key
RUTHiUHUIZ1i3vJF5wZcNdLt0b9OxVXS7NuLtQBMRSg+xug2295Im+ymvBxnc8iO/w/ahI6e16y78CsjUO5fAsv9N7jGtfnYgAM=
trusted comment: timestamp:1\tfile:h0x-core-linux-x86_64.tar.gz\thashed
FbX3ytewV+m9TXZSgfXaMzSiKNyvXzEHmgYQU7Vu/bq7CxChZ7MyisG+BWwI4P+k9WAbNFZV0mRq0lkczqgUCA==
";
    /// The same payload and key, signed as `other.json`.
    const OTHER_FILE_SIG: &str = "untrusted comment: signature from rsign secret key
RUTHiUHUIZ1i3kQAqYZ6AxJDfx/3aFhJkiyVhPBbsHBSUoWqL2NMpd1Zn1BgJM99GjjuGmByGOcYaHlhWf+W7ACBcXl3KniR+gI=
trusted comment: timestamp:1\tfile:other.json\thashed
UD5fqWKQDxUt4uJF6V83QziiAFOerEgVvRfWXbqhGztaExyVpc5nMxVKs5J5x5w3BxhiFtnNBPiuZDa+AzjgDg==
";
    const OTHER_KEY_SIG: &str = "untrusted comment: signature from rsign secret key
RURaRIrUmDKGf+VfuMRCB6G+7YmOZuDtq2kGvfhr9JFuy2kWFMZ6J+L3UogLdr/J7fvVdNbKYfGvxVj6sSbZQmp4hSeTA28H6g8=
trusted comment: test
ZJhj3MtbsDPyt278bwB0BcVRB5ZoKqA6VCCZ5s51lq49BobYTBUuzlJqofyqGhaBwWB6iLyCd1cVXXQhXnGlBg==
";

    /// Writes `payload` and (if given) its signature into a fresh scratch folder.
    fn signed(payload: &str, sig: Option<&str>) -> (super::Scratch, std::path::PathBuf, std::path::PathBuf) {
        let scratch = super::Scratch::new().unwrap();
        let file = scratch.0.join("h0x-core-linux-x86_64.tar.gz");
        let sig_path = super::minisig(&file);
        std::fs::write(&file, payload).unwrap();
        if let Some(sig) = sig {
            std::fs::write(&sig_path, sig).unwrap();
        }
        (scratch, file, sig_path)
    }

    fn refused(file: &std::path::Path, sig: &std::path::Path, key: &str, why: &str) {
        let err = super::verify(file, sig, key, NAME).unwrap_err();
        assert_eq!(err.kind, "refused");
        assert!(
            err.message
                .starts_with("the download h0x-core-linux-x86_64.tar.gz is not signed by the h0x-core release key"),
            "{}",
            err.message
        );
        assert!(err.message.contains(why), "{why}: {}", err.message);
        assert!(!file.exists(), "a refused download is deleted");
        assert!(!sig.exists(), "its signature is deleted too");
    }

    #[test]
    fn a_file_signed_by_the_key_is_kept() {
        let (_scratch, file, sig) = signed(PAYLOAD, Some(SIG));
        super::verify(&file, &sig, TEST_KEY, NAME).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), PAYLOAD);
        assert!(!sig.exists(), "the signature is not left behind");
    }

    #[test]
    fn a_tampered_file_is_refused_and_deleted() {
        let (_scratch, file, sig) = signed("h0x-core test releasE\n", Some(SIG));
        refused(&file, &sig, TEST_KEY, "its signature does not match");
        let (_scratch, file, sig) = signed("", Some(SIG));
        refused(&file, &sig, TEST_KEY, "its signature does not match");
    }

    #[test]
    fn a_missing_or_empty_signature_is_refused() {
        let (_scratch, file, sig) = signed(PAYLOAD, None);
        refused(&file, &sig, TEST_KEY, "it has no signature");
        let (_scratch, file, sig) = signed(PAYLOAD, Some(""));
        refused(&file, &sig, TEST_KEY, "its signature is unreadable");
    }

    #[test]
    fn a_signature_by_another_key_is_refused() {
        let (_scratch, file, sig) = signed(PAYLOAD, Some(OTHER_KEY_SIG));
        refused(&file, &sig, TEST_KEY, "its signature does not match");
        // The test signature does not pass for the embedded release key either.
        let (_scratch, file, sig) = signed(PAYLOAD, Some(SIG));
        refused(&file, &sig, super::PUBLIC_KEY, "its signature does not match");
    }

    #[test]
    fn a_signature_made_for_another_file_is_refused() {
        let (_scratch, file, sig) = signed(PAYLOAD, Some(OTHER_FILE_SIG));
        refused(&file, &sig, TEST_KEY, "its signature is for another file, not h0x-core-linux-x86_64.tar.gz");
        // `file:` must name the asset whole, not just begin with its name.
        let (_scratch, file, sig) = signed(PAYLOAD, Some(SIG));
        let err = super::verify(&file, &sig, TEST_KEY, "h0x-core-linux-x86_64.tar").unwrap_err();
        assert_eq!(err.kind, "refused");
        assert!(err.message.contains("its signature is for another file"), "{}", err.message);
        assert!(!file.exists() && !sig.exists(), "a refused download and its signature are deleted");
    }

    #[test]
    fn a_list_for_another_version_than_the_one_asked_for_is_refused() {
        let list = br#"{"version":"0.1.0","generation":1,"targets":{}}"#;
        let url = "https://github.com/vjk7989/h0x-ADE-core/releases/download/v0.1.1/manifest.json";
        let err = super::parse(list, Some("0.1.1"), url).err().unwrap();
        assert_eq!(err.kind, "refused");
        assert_eq!(err.message, format!("{url} lists h0x-core 0.1.0, not the requested 0.1.1"));
        assert_eq!(super::parse(list, Some("0.1.0"), url).unwrap().version, "0.1.0");
        // The latest list may be any version.
        assert_eq!(super::parse(list, None, url).unwrap().version, "0.1.0");
        let err = super::parse(b"", Some("0.1.0"), url).err().unwrap();
        assert_eq!(err.kind, "unavailable");
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
