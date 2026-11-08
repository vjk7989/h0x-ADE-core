//! Bridge secrets (the Telegram token) in the OS secret store, not in a plain
//! file (ADR-064). Windows: a DPAPI blob in `<name>.dpapi` (current user only).
//! macOS: the login keychain through `security -i`, the secret on stdin.
//! Linux: `secret-tool` (libsecret), the secret on stdin; without it, or without
//! a running keyring, a 0600 file `<name>` and one warning per process.
//! An entry is tied to the bridge folder, so scratch homes never meet real ones.
//! A legacy plaintext `<name>` is moved into the store on first read.

use super::Bridge;
use std::io;

fn read_file(path: &std::path::Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn remove_file(path: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub(super) fn get(b: &Bridge, name: &str) -> io::Result<Option<Vec<u8>>> {
    if let Some(value) = imp::get(b, name)? {
        return Ok(Some(value));
    }
    // A plaintext file from before the store (or the Linux fallback): move it in.
    let path = b.dir.join(name);
    let Some(value) = read_file(&path)? else { return Ok(None) };
    if imp::put(b, name, &value).is_ok() && imp::STORE_IS_NOT_THE_FILE {
        remove_file(&path)?;
    }
    Ok(Some(value))
}

pub(super) fn put(b: &Bridge, name: &str, value: &[u8]) -> io::Result<()> {
    imp::put(b, name, value)?;
    if imp::STORE_IS_NOT_THE_FILE {
        remove_file(&b.dir.join(name))?;
    }
    Ok(())
}

pub(super) fn del(b: &Bridge, name: &str) -> io::Result<()> {
    imp::del(b, name)?;
    remove_file(&b.dir.join(name))
}

/// The `security` / `secret-tool` account for `name` in this bridge folder.
#[cfg(unix)]
fn account(b: &Bridge, name: &str) -> io::Result<String> {
    let account = format!("{}/{name}", b.dir.display());
    if account.chars().any(char::is_control) {
        return Err(io::Error::other("the bridge folder name has control characters"));
    }
    Ok(account)
}

/// Runs `program` with `input` on stdin; its stdout when it exits 0.
/// `Ok(None)` for exit code `not_found`. Errors never carry the output.
#[cfg(unix)]
fn run(program: &str, args: &[&str], input: &[u8], not_found: i32) -> io::Result<Option<Vec<u8>>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child =
        Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let _ = stdin.write_all(input); // a tool that exits early is judged by its status
    drop(stdin);
    let out = child.wait_with_output()?;
    match out.status.code() {
        Some(0) => Ok(Some(out.stdout)),
        Some(code) if code == not_found => Ok(None),
        code => Err(io::Error::other(format!("{program} failed ({code:?})"))),
    }
}

/// Text form kept in the store: never empty, safe for a command line or stdin.
#[cfg(unix)]
fn to_text(value: &[u8]) -> String {
    format!("b64:{}", crate::put::encode(value))
}

#[cfg(unix)]
fn from_text(text: &[u8]) -> io::Result<Vec<u8>> {
    std::str::from_utf8(text)
        .ok()
        .and_then(|t| t.trim().strip_prefix("b64:"))
        .and_then(crate::put::decode)
        .ok_or_else(|| io::Error::other("the stored secret is not in the expected form"))
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    pub const STORE_IS_NOT_THE_FILE: bool = true;

    fn blob_file(name: &str) -> String {
        format!("{name}.dpapi")
    }

    /// Runs one DPAPI call over `input`; copies the result and frees the system's buffer.
    fn crypt(input: &[u8], protect: bool) -> io::Result<Vec<u8>> {
        let len = u32::try_from(input.len()).map_err(|_| io::Error::other("secret too large"))?;
        let data = CRYPT_INTEGER_BLOB { cbData: len, pbData: input.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: `data` points at `input` for the call; `out` is filled by the
        // system and released with LocalFree below; the other arguments are optional.
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &data,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            } else {
                CryptUnprotectData(
                    &data,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            }
        };
        if ok == 0 {
            return Err(io::Error::other(if protect {
                "the secret could not be protected for this user"
            } else {
                "the stored secret could not be unprotected (damaged, or saved by another user)"
            }));
        }
        // SAFETY: on success `out` is `cbData` bytes owned by the system.
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        // SAFETY: allocated by DPAPI for us; freed once.
        unsafe { LocalFree(out.pbData as _) };
        Ok(bytes)
    }

    pub fn get(b: &Bridge, name: &str) -> io::Result<Option<Vec<u8>>> {
        match read_file(&b.dir.join(blob_file(name)))? {
            Some(blob) => crypt(&blob, false).map(Some),
            None => Ok(None),
        }
    }

    pub fn put(b: &Bridge, name: &str, value: &[u8]) -> io::Result<()> {
        let blob = crypt(value, true)?;
        b.write_private(&blob_file(name), &blob).map_err(|e| io::Error::other(e.message))
    }

    pub fn del(b: &Bridge, name: &str) -> io::Result<()> {
        remove_file(&b.dir.join(blob_file(name)))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub const STORE_IS_NOT_THE_FILE: bool = true;
    const SERVICE: &str = "h0x-core bridge";
    /// `security` exit code for "item not found".
    const NOT_FOUND: i32 = 44;

    /// A double-quoted argument for `security -i`.
    fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    pub fn get(b: &Bridge, name: &str) -> io::Result<Option<Vec<u8>>> {
        let acct = account(b, name)?;
        let found =
            run("/usr/bin/security", &["find-generic-password", "-s", SERVICE, "-a", &acct, "-w"], b"", NOT_FOUND)?;
        found.map(|text| from_text(&text)).transpose()
    }

    pub fn put(b: &Bridge, name: &str, value: &[u8]) -> io::Result<()> {
        // The command, with the secret in it, goes over stdin, never argv.
        let cmd = format!(
            "add-generic-password -U -s {} -a {} -w {}\n",
            quote(SERVICE),
            quote(&account(b, name)?),
            quote(&to_text(value))
        );
        run("/usr/bin/security", &["-i"], cmd.as_bytes(), NOT_FOUND).map(|_| ())
    }

    pub fn del(b: &Bridge, name: &str) -> io::Result<()> {
        let acct = account(b, name)?;
        run("/usr/bin/security", &["delete-generic-password", "-s", SERVICE, "-a", &acct], b"", NOT_FOUND).map(|_| ())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;
    use std::sync::Once;

    /// The fallback is the file itself.
    pub const STORE_IS_NOT_THE_FILE: bool = false;
    /// `secret-tool lookup` and `clear` exit 1 for no match.
    const NOT_FOUND: i32 = 1;

    fn warn_once() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            eprintln!("h0x-core: no usable secret-tool/keyring; bridge secrets stay in an owner-only file instead");
        });
    }

    fn lookup(acct: &str) -> io::Result<Option<Vec<u8>>> {
        let args = ["lookup", "service", "h0x-core bridge", "account", acct];
        run("secret-tool", &args, b"", NOT_FOUND)?.map(|text| from_text(&text)).transpose()
    }

    pub fn get(b: &Bridge, name: &str) -> io::Result<Option<Vec<u8>>> {
        match lookup(&account(b, name)?) {
            Ok(found) => Ok(found),
            Err(_) => {
                warn_once();
                Ok(None)
            }
        }
    }

    pub fn put(b: &Bridge, name: &str, value: &[u8]) -> io::Result<()> {
        let acct = account(b, name)?;
        let args = ["store", "--label=h0x-core bridge", "service", "h0x-core bridge", "account", &acct];
        match run("secret-tool", &args, to_text(value).as_bytes(), i32::MAX) {
            Ok(_) => Ok(()),
            Err(_) => {
                warn_once();
                b.write_private(name, value).map_err(|e| io::Error::other(e.message))
            }
        }
    }

    pub fn del(b: &Bridge, name: &str) -> io::Result<()> {
        let acct = account(b, name)?;
        // Missing tool or entry: the file removal that follows is what counts.
        let _ = run("secret-tool", &["clear", "service", "h0x-core bridge", "account", &acct], b"", NOT_FOUND);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::scratch;

    #[test]
    fn a_secret_round_trips_and_goes() {
        let b = scratch();
        assert_eq!(b.secret_get("s").unwrap(), None, "no entry");
        b.secret_del("s").unwrap();
        b.secret_put("s", b"hunter2-secret").unwrap();
        assert_eq!(b.secret_get("s").unwrap().as_deref(), Some(&b"hunter2-secret"[..]));
        b.secret_put("s", b"").unwrap();
        assert_eq!(b.secret_get("s").unwrap().as_deref(), Some(&b""[..]), "empty is a value");
        b.secret_put("s", &[0, 255, 10, 13, b'"']).unwrap();
        assert_eq!(b.secret_get("s").unwrap().as_deref(), Some(&[0, 255, 10, 13, b'"'][..]), "binary");
        b.secret_del("s").unwrap();
        b.secret_del("s").unwrap();
        assert_eq!(b.secret_get("s").unwrap(), None);
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_plaintext_file_moves_into_the_store() {
        let b = scratch();
        std::fs::create_dir_all(&b.dir).unwrap();
        std::fs::write(b.dir.join("m"), b"legacy-value").unwrap();
        assert_eq!(b.secret_get("m").unwrap().as_deref(), Some(&b"legacy-value"[..]));
        assert_eq!(b.secret_get("m").unwrap().as_deref(), Some(&b"legacy-value"[..]), "second read");
        #[cfg(any(windows, target_os = "macos"))]
        assert!(!b.dir.join("m").exists(), "the plaintext is gone");
        b.secret_del("m").unwrap();
        assert_eq!(b.secret_get("m").unwrap(), None);
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_damaged_blob_is_an_error_and_the_file_holds_no_plaintext() {
        let b = scratch();
        b.secret_put("d", b"plain-needle").unwrap();
        let blob = std::fs::read(b.dir.join("d.dpapi")).unwrap();
        assert!(!blob.windows(12).any(|w| w == b"plain-needle"));
        std::fs::write(b.dir.join("d.dpapi"), b"not a blob").unwrap();
        assert!(b.secret_get("d").is_err());
        b.secret_del("d").unwrap();
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    /// Whether any file in the bridge folder holds `needle`.
    fn folder_holds(b: &super::Bridge, needle: &[u8]) -> bool {
        let Ok(entries) = std::fs::read_dir(&b.dir) else { return false };
        entries
            .map(|e| std::fs::read(e.unwrap().path()).unwrap_or_default())
            .any(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
    }

    #[test]
    fn two_bridge_folders_never_see_each_others_secrets() {
        let (a, b) = (scratch(), scratch());
        a.secret_put("t", b"secret-of-a").unwrap();
        assert_eq!(b.secret_get("t").unwrap(), None, "b sees nothing of a");
        b.secret_put("t", b"secret-of-b").unwrap();
        assert_eq!(a.secret_get("t").unwrap().as_deref(), Some(&b"secret-of-a"[..]), "b's put leaves a alone");
        b.secret_del("t").unwrap();
        assert_eq!(a.secret_get("t").unwrap().as_deref(), Some(&b"secret-of-a"[..]), "b's delete leaves a alone");
        assert_eq!(b.secret_get("t").unwrap(), None);
        a.secret_del("t").unwrap();
        assert_eq!(a.secret_get("t").unwrap(), None);
        let _ = std::fs::remove_dir_all(&a.dir);
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_second_put_replaces_the_first_and_four_kib_round_trips() {
        let b = scratch();
        b.secret_put("r", b"first-value").unwrap();
        b.secret_put("r", b"second-value").unwrap();
        assert_eq!(b.secret_get("r").unwrap().as_deref(), Some(&b"second-value"[..]), "the latest wins");
        let large: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        b.secret_put("r", &large).unwrap();
        assert_eq!(b.secret_get("r").unwrap().as_deref(), Some(&large[..]), "4 KiB, every byte value");
        let leftovers: Vec<_> = std::fs::read_dir(&b.dir)
            .map(|d| d.map(|e| e.unwrap().file_name()).filter(|n| n.to_string_lossy().ends_with(".tmp")).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "no temp files left: {leftovers:?}");
        b.secret_del("r").unwrap();
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn delete_then_get_is_none_and_no_file_keeps_the_secret() {
        let b = scratch();
        b.secret_put("c", b"cleared-needle-42").unwrap();
        b.secret_del("c").unwrap();
        assert_eq!(b.secret_get("c").unwrap(), None);
        assert!(!folder_holds(&b, b"cleared-needle-42"), "nothing left holds the secret");
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_legacy_file_beside_a_stored_entry_does_not_survive_a_read() {
        let b = scratch();
        b.secret_put("both", b"stored-value").unwrap();
        std::fs::write(b.dir.join("both"), b"legacy-plaintext").unwrap();
        let got = b.secret_get("both").unwrap();
        #[cfg(any(windows, target_os = "macos"))]
        {
            assert_eq!(got.as_deref(), Some(&b"stored-value"[..]), "the store wins");
            assert!(!b.dir.join("both").exists(), "the legacy plaintext is removed even when the store had an entry");
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(got.is_some());
        b.secret_del("both").unwrap();
        assert!(!b.dir.join("both").exists());
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_bridge_folder_with_spaces_and_non_ascii_keeps_its_secret() {
        let dir = std::env::temp_dir()
            .join(format!("h0x core brücke 秘密 {}", crate::platform::random_hex()))
            .join("bridge dir");
        let b = super::Bridge::at(dir, crate::config::BridgeConfig::default());
        b.secret_put("telegram.token", b"spaced-value").unwrap();
        assert_eq!(b.secret_get("telegram.token").unwrap().as_deref(), Some(&b"spaced-value"[..]));
        assert_eq!(scratch().secret_get("telegram.token").unwrap(), None, "another folder sees nothing");
        b.secret_del("telegram.token").unwrap();
        assert_eq!(b.secret_get("telegram.token").unwrap(), None);
        let _ = std::fs::remove_dir_all(b.dir.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_bridge_folder_with_a_control_character_is_refused_without_the_secret() {
        let dir = std::env::temp_dir().join(format!("h0x-core-bridge-{}\nx", crate::platform::random_hex()));
        let b = super::Bridge::at(dir, crate::config::BridgeConfig::default());
        let e = b.secret_put("s", b"ctl-needle").unwrap_err();
        assert!(!e.to_string().contains("ctl-needle"), "{e}");
        assert!(b.secret_get("s").is_err());
        assert!(!b.dir.exists() || !folder_holds(&b, b"ctl-needle"), "nothing written");
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[cfg(windows)]
    #[test]
    fn empty_or_cut_blobs_are_errors_that_name_no_secret_bytes() {
        let b = scratch();
        b.secret_put("d", b"cut-blob-needle").unwrap();
        let blob = std::fs::read(b.dir.join("d.dpapi")).unwrap();
        for (why, damaged) in
            [("empty", &blob[..0]), ("half", &blob[..blob.len() / 2]), ("one short", &blob[..blob.len() - 1])]
        {
            std::fs::write(b.dir.join("d.dpapi"), damaged).unwrap();
            let e = b.secret_get("d").unwrap_err();
            assert!(!e.to_string().contains("cut-blob-needle"), "{why}: {e}");
        }
        b.secret_del("d").unwrap();
        assert!(!b.dir.join("d.dpapi").exists(), "a damaged entry can still be deleted");
        let _ = std::fs::remove_dir_all(&b.dir);
    }
}
