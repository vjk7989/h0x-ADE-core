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

/// Why `file` is not the release asset `name` signed by `key` (its trusted
/// comment's `file:`, so one signed file cannot stand in for another).
fn check(file: &Path, sig: &Path, key: &str, name: &str) -> Result<(), String> {
    checked_bytes(file, sig, key, name).map(|_| ())
}

/// `check`, giving the very bytes that were verified.
fn checked_bytes(file: &Path, sig: &Path, key: &str, name: &str) -> Result<Vec<u8>, String> {
    let key = minisign_verify::PublicKey::from_base64(key).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(sig).map_err(|_| "it has no signature".to_string())?;
    let signature =
        minisign_verify::Signature::decode(&text).map_err(|e| format!("its signature is unreadable: {e}"))?;
    let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
    key.verify(&bytes, &signature, false).map_err(|e| format!("its signature does not match: {e}"))?;
    let file_field = format!("file:{name}");
    if signature.trusted_comment().split('\t').any(|f| f == file_field) {
        Ok(bytes)
    } else {
        Err(format!("its signature is for another file, not {name}"))
    }
}

fn not_signed(file: &Path, why: String) -> RpcError {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    RpcError::new(
        "refused",
        format!("the download {name} is not signed by the h0x-core release key: {why}"),
        "the download may have been tampered with; try again later",
    )
}

/// Checks a download `file` against its signature `sig`, made for the release
/// asset `name`; on any failure both are deleted and the download is refused.
fn verify(file: &Path, sig: &Path, key: &str, name: &str) -> Result<(), RpcError> {
    let checked = check(file, sig, key, name);
    let _ = std::fs::remove_file(sig);
    checked.map_err(|why| {
        let _ = std::fs::remove_file(file);
        not_signed(file, why)
    })
}

/// A release archive, by name (`h0x-core-linux-x86_64.tar.gz`, `h0x-core-windows-x86_64.zip`).
fn is_archive(file: &Path) -> bool {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    name.ends_with(".tar.gz") || name.ends_with(".zip")
}

/// `update --from`: checks a release archive the person has against the
/// release key, with `sig` or the `<archive>.minisig` beside it, bound to the
/// archive's own file name. The files are the person's: never deleted.
pub fn verify_archive(archive: &Path, sig: Option<&Path>) -> Result<(), RpcError> {
    verify_archive_by(archive, sig, PUBLIC_KEY)
}

fn verify_archive_by(archive: &Path, sig: Option<&Path>, key: &str) -> Result<(), RpcError> {
    let sig = sig.map(Path::to_path_buf).unwrap_or_else(|| minisig(archive));
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    check(archive, &sig, key, &name).map_err(|why| not_signed(archive, why))
}

/// Verifies the archive and keeps the verified bytes in a new private file in
/// `scratch` (so what is unpacked cannot differ from what was checked, even
/// if the person's file is swapped meanwhile).
fn sealed(archive: &Path, sig: Option<&Path>, key: &str, scratch: &Path) -> Result<PathBuf, RpcError> {
    let sig = sig.map(Path::to_path_buf).unwrap_or_else(|| minisig(archive));
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    let bytes = checked_bytes(archive, &sig, key, &name).map_err(|why| not_signed(archive, why))?;
    let copy = scratch.join(&*name);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&copy)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes));
    written.map_err(|e| unavailable(format!("cannot keep {} for unpacking: {e}", archive.display())))?;
    Ok(copy)
}

/// The h0x-core inside what `update --from` was given: a signed release
/// archive, unpacked into `scratch` once its signature checked out. Debug and
/// test builds also take a bare binary as it is (the tests' own builds).
pub fn from_file(from: &Path, sig: Option<&Path>, scratch: &Path) -> Result<PathBuf, RpcError> {
    if is_archive(from) {
        return unpack(&sealed(from, sig, PUBLIC_KEY, scratch)?, scratch);
    }
    if cfg!(any(debug_assertions, test)) && sig.is_none() {
        return Ok(from.to_path_buf());
    }
    Err(RpcError::new(
        "refused",
        format!("{} is not a signed release archive", from.display()),
        "give the release's h0x-core-<target>.tar.gz or .zip, with its .minisig beside it",
    ))
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
    unpack(&tar, scratch).map_err(|_| {
        RpcError::new("refused", format!("h0x-core {}'s download for {target} holds no h0x-core binary", m.version), "")
    })
}

/// Unpacks a release archive into `scratch`: the path of its h0x-core.
fn unpack(archive: &Path, scratch: &Path) -> Result<PathBuf, RpcError> {
    let exe = if archive.extension().is_some_and(|e| e == "zip") { "h0x-core.exe" } else { "h0x-core" };
    let ok =
        Command::new(tar_program()).arg("-xf").arg(archive).arg("-C").arg(scratch).status().is_ok_and(|s| s.success());
    let bin = scratch.join(exe);
    // A regular file: not a link to somewhere else, not a folder.
    if ok && std::fs::symlink_metadata(&bin).is_ok_and(|m| m.file_type().is_file()) {
        Ok(bin)
    } else {
        Err(RpcError::new("refused", format!("{} holds no h0x-core binary", archive.display()), ""))
    }
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

/// The release signature of an archive `fetch` got, saved beside it
/// (`<archive>.minisig`) for a machine that checks it itself. None for a test
/// site (H0X_CORE_RELEASES_URL), which is not signed.
pub fn fetch_signature(m: &Manifest, target: &str, archive: &Path) -> Result<Option<PathBuf>, RpcError> {
    let Some(t) = m.targets.get(target).filter(|_| overridden().is_none()) else { return Ok(None) };
    let sig = minisig(archive);
    curl(&format!("{}.minisig", t.url), &sig)?;
    Ok(Some(sig))
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

    /// `update --from`: the person's own files, checked and never deleted.
    #[test]
    fn a_local_archive_needs_its_own_signature_and_is_never_deleted() {
        let (_scratch, file, sig) = signed(PAYLOAD, Some(SIG));
        super::verify_archive_by(&file, None, TEST_KEY).unwrap();
        assert!(file.exists() && sig.exists(), "the person's files stay");
        // The signature may live elsewhere.
        let elsewhere = file.with_file_name("other-place.minisig");
        std::fs::rename(&sig, &elsewhere).unwrap();
        assert!(super::verify_archive_by(&file, None, TEST_KEY).is_err(), "nothing beside the archive");
        super::verify_archive_by(&file, Some(&elsewhere), TEST_KEY).unwrap();
        // Tampered, wrong key, signed for another file, empty signature: all refused, all kept.
        let (_scratch, file, sig) = signed("h0x-core test releasE\n", Some(SIG));
        let err = super::verify_archive_by(&file, None, TEST_KEY).unwrap_err();
        assert_eq!(err.kind, "refused");
        assert!(err.message.contains("its signature does not match"), "{}", err.message);
        assert!(file.exists() && sig.exists());
        for (text, why) in [
            (OTHER_KEY_SIG, "its signature does not match"),
            (OTHER_FILE_SIG, "its signature is for another file, not h0x-core-linux-x86_64.tar.gz"),
            ("", "its signature is unreadable"),
        ] {
            let (_scratch, file, sig) = signed(PAYLOAD, Some(text));
            let err = super::verify_archive_by(&file, None, TEST_KEY).unwrap_err();
            assert!(err.message.contains(why), "{why}: {}", err.message);
            assert!(file.exists() && sig.exists());
        }
        let (_scratch, file, _) = signed(PAYLOAD, None);
        let err = super::verify_archive_by(&file, None, TEST_KEY).unwrap_err();
        assert!(err.message.contains("it has no signature"), "{}", err.message);
        // The embedded release key does not take the test key's signature.
        let (_scratch, file, _) = signed(PAYLOAD, Some(SIG));
        assert!(super::verify_archive(&file, None).is_err());
    }

    #[test]
    fn a_signature_for_another_file_name_refuses_a_renamed_archive() {
        let (_scratch, file, _) = signed(PAYLOAD, Some(SIG));
        let renamed = file.with_file_name("h0x-core-linux-aarch64.tar.gz");
        std::fs::rename(&file, &renamed).unwrap();
        std::fs::rename(super::minisig(&file), super::minisig(&renamed)).unwrap();
        let err = super::verify_archive_by(&renamed, None, TEST_KEY).unwrap_err();
        assert!(err.message.contains("its signature is for another file, not h0x-core-linux-aarch64.tar.gz"));
    }

    #[test]
    fn what_is_unpacked_is_the_archive_that_was_verified_even_if_the_file_is_swapped() {
        let (_scratch, file, _) = signed(PAYLOAD, Some(SIG));
        let keep = super::Scratch::new().unwrap();
        let copy = super::sealed(&file, None, TEST_KEY, &keep.0).unwrap();
        std::fs::write(&file, b"swapped after the check").unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), PAYLOAD.as_bytes());
        assert_ne!(copy, file);
        // Unverified archives leave nothing behind.
        let other = super::Scratch::new().unwrap();
        assert!(super::sealed(&file, None, TEST_KEY, &other.0).is_err());
        assert_eq!(std::fs::read_dir(&other.0).unwrap().count(), 0);
    }

    #[test]
    fn the_sealed_copy_never_replaces_what_is_already_in_its_folder() {
        let (scratch, file, sig) = signed(PAYLOAD, Some(SIG));
        // The person's archive in a folder with spaces and non-ASCII letters.
        let folder = scratch.0.join("my downloads ü");
        std::fs::create_dir(&folder).unwrap();
        let file = {
            let moved = folder.join(file.file_name().unwrap());
            std::fs::rename(&file, &moved).unwrap();
            std::fs::rename(&sig, super::minisig(&moved)).unwrap();
            moved
        };
        let name = file.file_name().unwrap();
        // Something already there under the archive's name: refused, left as it was.
        let keep = super::Scratch::new().unwrap();
        std::fs::write(keep.0.join(name), "planted").unwrap();
        let err = super::sealed(&file, None, TEST_KEY, &keep.0).unwrap_err();
        assert_eq!(err.kind, "unavailable");
        assert!(err.message.contains("cannot keep"), "{}", err.message);
        assert_eq!(std::fs::read(keep.0.join(name)).unwrap(), b"planted");
        // A folder that is not there: refused, nothing made.
        let gone = keep.0.join("gone");
        assert_eq!(super::sealed(&file, None, TEST_KEY, &gone).unwrap_err().kind, "unavailable");
        assert!(!gone.exists());
        // A refusal never deletes the person's own archive or its signature.
        let wrong = super::Scratch::new().unwrap();
        assert_eq!(super::sealed(&file, None, super::PUBLIC_KEY, &wrong.0).unwrap_err().kind, "refused");
        assert_eq!(std::fs::read_dir(&wrong.0).unwrap().count(), 0);
        assert!(file.exists() && super::minisig(&file).exists());
        // An empty folder: the copy alone, under the archive's own name.
        let fresh = super::Scratch::new().unwrap();
        let copy = super::sealed(&file, None, TEST_KEY, &fresh.0).unwrap();
        assert_eq!(copy, fresh.0.join(name));
        assert_eq!(std::fs::read_dir(&fresh.0).unwrap().count(), 1, "the signature is not copied");
    }

    #[test]
    fn from_takes_only_a_signed_archive_or_a_debug_binary() {
        let scratch = super::Scratch::new().unwrap();
        let bare = scratch.0.join("h0x-core");
        std::fs::write(&bare, "x").unwrap();
        // Debug and test builds: a bare binary as it is; a signature given for it makes no sense.
        assert_eq!(super::from_file(&bare, None, &scratch.0).unwrap(), bare);
        let err = super::from_file(&bare, Some(&bare), &scratch.0).unwrap_err();
        assert_eq!(err.kind, "refused");
        // An archive is always checked, here against the embedded key: refused.
        let (_s, archive, _) = signed(PAYLOAD, Some(SIG));
        let err = super::from_file(&archive, None, &scratch.0).unwrap_err();
        assert!(err.message.contains("not signed by the h0x-core release key"), "{}", err.message);
    }

    #[test]
    fn a_signature_saved_with_windows_line_endings_still_verifies() {
        let (_scratch, file, sig) = signed(PAYLOAD, Some(SIG.replace('\n', "\r\n").as_str()));
        super::verify_archive_by(&file, None, TEST_KEY).unwrap();
        assert!(file.exists() && sig.exists());
    }

    #[test]
    fn a_signature_given_as_a_folder_is_refused_as_missing() {
        let (_scratch, file, sig) = signed(PAYLOAD, Some(SIG));
        let folder = file.with_file_name("sigs");
        std::fs::create_dir(&folder).unwrap();
        let err = super::verify_archive_by(&file, Some(&folder), TEST_KEY).unwrap_err();
        assert_eq!(err.kind, "refused");
        assert!(err.message.contains("it has no signature"), "{}", err.message);
        assert!(file.exists() && sig.exists() && folder.is_dir(), "nothing of the person's is deleted");
    }

    /// Only the archive's file name is bound: any folder holds it (spaces,
    /// non-ASCII), but a browser's renamed copy (` (1)`) is another file.
    #[test]
    fn the_folder_does_not_matter_but_the_archive_file_name_does() {
        let (scratch, file, sig) = signed(PAYLOAD, Some(SIG));
        let folder = scratch.0.join("Jane Doe Ünïcödé");
        std::fs::create_dir(&folder).unwrap();
        let moved = folder.join(NAME);
        std::fs::rename(&file, &moved).unwrap();
        std::fs::rename(&sig, super::minisig(&moved)).unwrap();
        super::verify_archive_by(&moved, None, TEST_KEY).unwrap();
        let copy = folder.join("h0x-core-linux-x86_64 (1).tar.gz");
        std::fs::copy(&moved, &copy).unwrap();
        std::fs::copy(super::minisig(&moved), super::minisig(&copy)).unwrap();
        let err = super::verify_archive_by(&copy, None, TEST_KEY).unwrap_err();
        assert!(
            err.message.contains("its signature is for another file, not h0x-core-linux-x86_64 (1).tar.gz"),
            "{}",
            err.message
        );
        // The same signature handed over explicitly for the renamed copy: still refused.
        assert!(super::verify_archive_by(&copy, Some(&super::minisig(&moved)), TEST_KEY).is_err());
        assert!(copy.exists() && moved.exists());
    }

    #[test]
    fn only_tar_gz_and_zip_names_count_as_release_archives() {
        for name in ["h0x-core-linux-x86_64.tar.gz", "h0x-core-windows-x86_64.zip", "a b.zip", ".zip"] {
            assert!(super::is_archive(std::path::Path::new(name)), "{name}");
        }
        for name in ["", "h0x-core", "h0x-core.exe", "h0x-core.tgz", "h0x-core.tar", "h0x-core.gz", "x.zip.minisig"] {
            assert!(!super::is_archive(std::path::Path::new(name)), "{name}");
        }
    }

    /// An uncompressed ustar archive of `entries` (tar -x reads it whatever its name).
    fn ustar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..108].copy_from_slice(b"0000755\0");
            h[108..116].copy_from_slice(b"0000000\0");
            h[116..124].copy_from_slice(b"0000000\0");
            h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
            h[136..148].copy_from_slice(b"00000000000\0");
            h[148..156].copy_from_slice(b"        ");
            h[156] = b'0';
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
            h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            out.extend_from_slice(&h);
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    #[test]
    fn an_archive_without_the_h0x_core_binary_is_refused_and_extra_files_do_not_matter() {
        let scratch = super::Scratch::new().unwrap();
        let archive = scratch.0.join(NAME);
        std::fs::write(&archive, ustar(&[("README", b"hi"), ("h0x-core.exe", b"x")])).unwrap();
        let into = scratch.0.join("a");
        std::fs::create_dir(&into).unwrap();
        let err = super::unpack(&archive, &into).unwrap_err();
        assert_eq!(err.kind, "refused");
        assert!(err.message.ends_with("holds no h0x-core binary"), "{}", err.message);
        // An empty archive, and a file that is no archive at all.
        std::fs::write(&archive, ustar(&[])).unwrap();
        assert!(super::unpack(&archive, &into).is_err());
        std::fs::write(&archive, b"").unwrap();
        assert!(super::unpack(&archive, &into).is_err());
        // The binary beside extra files and a folder: found.
        std::fs::write(&archive, ustar(&[("LICENSE", b"l"), ("conpty/x", b"c"), ("h0x-core", b"#!")])).unwrap();
        let into = scratch.0.join("b");
        std::fs::create_dir(&into).unwrap();
        assert_eq!(super::unpack(&archive, &into).unwrap(), into.join("h0x-core"));
    }

    #[test]
    fn unpacking_never_writes_outside_its_folder() {
        let scratch = super::Scratch::new().unwrap();
        let archive = scratch.0.join(NAME);
        std::fs::write(&archive, ustar(&[("../escaped", b"e"), ("sub/../../escaped-too", b"e"), ("h0x-core", b"#!")]))
            .unwrap();
        let into = scratch.0.join("into");
        std::fs::create_dir(&into).unwrap();
        let _ = super::unpack(&archive, &into);
        assert!(!scratch.0.join("escaped").exists(), "a ../ entry stays inside");
        assert!(!scratch.0.join("escaped-too").exists(), "a sub/../../ entry stays inside");
    }

    #[test]
    fn the_unpacked_binary_must_be_a_regular_file() {
        let scratch = super::Scratch::new().unwrap();
        let archive = scratch.0.join(NAME);
        // A folder named h0x-core.
        std::fs::write(&archive, ustar(&[("h0x-core/inner", b"x")])).unwrap();
        let into = scratch.0.join("dir");
        std::fs::create_dir(&into).unwrap();
        assert_eq!(super::unpack(&archive, &into).unwrap_err().kind, "refused");
        // An archive without it.
        std::fs::write(&archive, ustar(&[("other", b"x")])).unwrap();
        let empty = scratch.0.join("none");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(super::unpack(&archive, &empty).unwrap_err().kind, "refused");
        // A regular file is taken.
        std::fs::write(&archive, ustar(&[("h0x-core", b"#!")])).unwrap();
        let good = scratch.0.join("good");
        std::fs::create_dir(&good).unwrap();
        assert_eq!(super::unpack(&archive, &good).unwrap(), good.join("h0x-core"));
        // A link already in the folder (the archive brings no h0x-core of its own).
        #[cfg(unix)]
        {
            std::fs::write(&archive, ustar(&[("other", b"x")])).unwrap();
            let linked = scratch.0.join("linked");
            std::fs::create_dir(&linked).unwrap();
            std::os::unix::fs::symlink("/bin/sh", linked.join("h0x-core")).unwrap();
            assert_eq!(super::unpack(&archive, &linked).unwrap_err().kind, "refused");
        }
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
