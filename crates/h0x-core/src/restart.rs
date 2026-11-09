//! A short-lived handoff for explicitly requested restarts, never for SIGTERM.

use crate::paths::Paths;
use crate::runtime::Core;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

const ENVIRONMENT: &str = "restart-env.json";
const VALID_MS: u64 = 3 * 60 * 1000;

#[derive(serde::Serialize, serde::Deserialize)]
struct Environment {
    machine_id: Option<String>,
    boot_id: Option<String>,
    written_ms: u64,
    vars: Vec<(OsString, OsString)>,
}

fn now_ms() -> io::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_millis() as u64)
}

pub(crate) fn prepare(core: &Core) -> io::Result<()> {
    let state = core.lock();
    if state.stopping || core.restart.swap(true, Ordering::SeqCst) {
        return Err(io::Error::other("the core is stopping or restarting"));
    }
    drop(state);
    discard(&core.paths);
    if let Err(error) = save(&core.paths) {
        eprintln!("core: cannot preserve restart environment; using the next launcher's environment: {error}");
        discard(&core.paths);
    }
    Ok(())
}

fn save(paths: &Paths) -> io::Result<()> {
    let env = Environment {
        machine_id: crate::platform::machine_id(),
        boot_id: crate::platform::boot_id()?,
        written_ms: now_ms()?,
        vars: std::env::vars_os().filter(|(key, _)| !key.as_encoded_bytes().starts_with(b"=")).collect(),
    };
    let tmp = paths.state.join("restart-env.tmp");
    let mut file = crate::platform::private_file(&tmp)?;
    serde_json::to_writer(&mut file, &env)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, paths.state.join(ENVIRONMENT))
}

fn load(paths: &Paths) -> io::Result<Option<Vec<(OsString, OsString)>>> {
    let bytes = match std::fs::read(paths.state.join(ENVIRONMENT)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => result?,
    };
    // Decode and validate everything before touching the process environment.
    let mut env: Environment =
        serde_json::from_slice(&bytes).map_err(|_| io::Error::other("invalid restart environment"))?;
    // cmd.exe's per-drive working directories are not variables set_var can restore.
    env.vars.retain(|(key, _)| !key.as_encoded_bytes().starts_with(b"="));
    let age = now_ms()?.checked_sub(env.written_ms);
    if env.machine_id != crate::platform::machine_id()
        || env.boot_id != crate::platform::boot_id()?
        || age.is_none_or(|age| age > VALID_MS)
    {
        return Err(io::Error::other("restart environment is from another machine, boot, or time"));
    }
    if env.vars.iter().any(|(key, value)| !valid_key(key) || value.as_encoded_bytes().contains(&0)) {
        return Err(io::Error::other("invalid restart environment entry"));
    }
    // Command uses the OS's key comparison, including Windows' case-insensitive keys.
    let mut unique = std::process::Command::new("unused");
    unique.env_clear().envs(env.vars.iter().cloned());
    if unique.get_envs().count() != env.vars.len() {
        return Err(io::Error::other("duplicate restart environment entries"));
    }
    Ok(Some(env.vars))
}

/// The caller owns core.lock and has not started threads or adopted holders.
/// The selected session's paths and the new process's lifecycle marks stay its own.
///
/// # Safety
/// No other thread may read or write the process environment.
pub(crate) unsafe fn restore(paths: &Paths) {
    sweep_temporary(paths);
    let vars = match load(paths) {
        Ok(Some(vars)) => vars,
        Ok(None) => return,
        Err(error) => {
            eprintln!("core: ignoring restart environment: {error}");
            discard(paths);
            return;
        }
    };
    let restored = crate::platform::restored_environment(vars, std::env::vars_os());
    unsafe {
        for (key, _) in std::env::vars_os().filter(|(key, _)| valid_key(key)) {
            std::env::remove_var(key);
        }
        for (key, value) in restored {
            std::env::set_var(key, value);
        }
    }
}

fn valid_key(key: &OsStr) -> bool {
    !key.is_empty() && !key.as_encoded_bytes().iter().any(|byte| matches!(byte, 0 | b'='))
}

pub(crate) fn on_sigterm(core: &Core) {
    if !core.restart.swap(true, Ordering::SeqCst) {
        discard(&core.paths);
    }
}

fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("core: cannot remove {}: {error}", path.display());
    }
}

fn sweep_temporary(paths: &Paths) {
    if let Ok(entries) = std::fs::read_dir(&paths.state) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("restart-env.") && name.ends_with(".tmp") {
                remove(&entry.path());
            }
        }
    }
}

pub(crate) fn discard(paths: &Paths) {
    remove(&paths.state.join(ENVIRONMENT));
    sweep_temporary(paths);
}

/// A connect-only stop can race an active handoff; only an idle session is ours to clean.
pub(crate) fn discard_if_stopped(paths: &Paths) {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths.state.join("core.lock"));
    match lock {
        Ok(lock) if crate::platform::try_lock(&lock) => discard(paths),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("core: cannot lock restart environment for cleanup: {error}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn inherited_keys_that_cannot_be_unset_are_skipped() {
        for key in ["", "=C:", "=::", "BAD=KEY", "BAD\0KEY"] {
            assert!(!super::valid_key(std::ffi::OsStr::new(key)), "{key:?}");
        }
        assert!(super::valid_key(std::ffi::OsStr::new("PATH")));
    }

    use super::{ENVIRONMENT, Paths, VALID_MS};
    use serde_json::{Value, json};
    use std::ffi::OsString;

    fn state() -> (Paths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("h0x-core-restart-note-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(root.join("state")).unwrap();
        let paths = Paths {
            state: root.join("state"),
            config: root.join("config.toml"),
            socket: root.join("run/core.sock"),
            holder_socket: root.join("run/core.holder.sock"),
        };
        (paths, root)
    }

    /// A note this machine and boot would have written, with the given time and entries.
    fn note(written_ms: Value, vars: &[(&str, &str)]) -> Value {
        let vars: Vec<(OsString, OsString)> = vars.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        json!({
            "machine_id": crate::platform::machine_id(),
            "boot_id": crate::platform::boot_id().unwrap(),
            "written_ms": written_ms,
            "vars": serde_json::to_value(vars).unwrap(),
        })
    }

    fn load(paths: &Paths, bytes: &[u8]) -> std::io::Result<Option<Vec<(OsString, OsString)>>> {
        std::fs::write(paths.state.join(ENVIRONMENT), bytes).unwrap();
        super::load(paths)
    }

    #[test]
    fn a_note_is_accepted_only_within_three_minutes_of_being_written() {
        let (paths, root) = state();
        let now = super::now_ms().unwrap();
        let margin = 10_000;
        let at = |ms: Value| serde_json::to_vec(&note(ms, &[("H0X_VALUE", "saved")])).unwrap();
        assert!(load(&paths, &at(now.into())).unwrap().is_some(), "just written");
        assert!(load(&paths, &at((now - VALID_MS + margin).into())).unwrap().is_some(), "just inside the window");
        for (why, ms) in [
            ("just outside the window", Value::from(now - VALID_MS - margin)),
            ("written in the future (clock skew)", Value::from(now + margin)),
            ("zero", Value::from(0)),
            ("the largest time", Value::from(u64::MAX)),
        ] {
            assert!(load(&paths, &at(ms)).is_err(), "{why}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_empty_truncated_or_mistyped_note_is_rejected() {
        let (paths, root) = state();
        assert!(super::load(&paths).unwrap().is_none(), "no note at all is not an error");
        let now = super::now_ms().unwrap();
        let valid = serde_json::to_vec(&note(now.into(), &[("H0X_VALUE", "saved")])).unwrap();
        let mut broken: Vec<(&str, Vec<u8>)> = vec![
            ("empty file", Vec::new()),
            ("truncated", valid[..valid.len() / 2].to_vec()),
            ("json null", b"null".to_vec()),
            ("json array", b"[]".to_vec()),
            ("not utf-8", b"\xff\xfe{}".to_vec()),
            ("time past u64", {
                let text = String::from_utf8(valid.clone()).unwrap();
                text.replace(&now.to_string(), "18446744073709551616").into_bytes()
            }),
        ];
        for (why, mutate) in
            [("negative time", json!(-1)), ("fractional time", json!(1.5)), ("time as text", json!(now.to_string()))]
        {
            broken.push((why, serde_json::to_vec(&note(mutate, &[("H0X_VALUE", "saved")])).unwrap()));
        }
        for (why, field, value) in [
            ("missing time", "written_ms", None),
            ("missing vars", "vars", None),
            ("null vars", "vars", Some(Value::Null)),
            ("vars as an object", "vars", Some(json!({"H0X_VALUE": "saved"}))),
            ("vars as plain strings", "vars", Some(json!([["H0X_VALUE", "saved"]]))),
        ] {
            let mut bad = note(now.into(), &[("H0X_VALUE", "saved")]);
            match value {
                Some(value) => bad[field] = value,
                None => {
                    bad.as_object_mut().unwrap().remove(field);
                }
            }
            broken.push((why, serde_json::to_vec(&bad).unwrap()));
        }
        for (why, bytes) in broken {
            assert!(load(&paths, &bytes).is_err(), "{why}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_fields_are_ignored_but_a_missing_machine_or_boot_must_match_this_one() {
        let (paths, root) = state();
        let now = super::now_ms().unwrap();
        let mut extra = note(now.into(), &[("H0X_VALUE", "saved")]);
        extra["from_a_newer_core"] = json!({"nested": [1, 2]});
        assert!(load(&paths, &serde_json::to_vec(&extra).unwrap()).unwrap().is_some());
        let here = [
            ("machine_id", crate::platform::machine_id().is_none()),
            ("boot_id", crate::platform::boot_id().unwrap().is_none()),
        ];
        for (field, this_one_has_none) in here {
            for absent in [None, Some(Value::Null)] {
                let mut bad = note(now.into(), &[("H0X_VALUE", "saved")]);
                match absent.clone() {
                    Some(value) => bad[field] = value,
                    None => {
                        bad.as_object_mut().unwrap().remove(field);
                    }
                }
                let loaded = load(&paths, &serde_json::to_vec(&bad).unwrap());
                assert_eq!(loaded.is_ok(), this_one_has_none, "{field} {absent:?}");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_entry_is_checked_before_the_note_is_accepted() {
        let (paths, root) = state();
        let now = super::now_ms().unwrap();
        let entries = |vars: &[(&str, &str)]| load(&paths, &serde_json::to_vec(&note(now.into(), vars)).unwrap());
        for (why, bad) in [
            ("empty name", &[("H0X_VALUE", "saved"), ("", "value")][..]),
            ("NUL in a name", &[("H0X\0VALUE", "saved")]),
            ("= in a name", &[("H0X=VALUE", "saved")]),
            ("NUL in a value", &[("H0X_VALUE", "sa\0ved")]),
            ("the same name twice", &[("H0X_VALUE", "one"), ("H0X_VALUE", "two")]),
        ] {
            assert!(entries(bad).is_err(), "{why}");
        }
        assert_eq!(entries(&[]).unwrap(), Some(Vec::new()), "an empty environment is still a note");
        let kept = [
            ("H0X_EQUALS", "a=b=c"),
            ("H0X_EMPTY", ""),
            ("H0X_UNICODE", "上 é ☃"),
            ("H0X_WINPATH", r"C:\Program Files\h0x;\\server\share\bin;D:\"),
        ];
        let mut with_drive = kept.to_vec();
        with_drive.push(("=C:", r"C:\Users\someone"));
        let loaded = entries(&with_drive).unwrap().unwrap();
        let expected: Vec<(OsString, OsString)> = kept.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        assert_eq!(loaded, expected, "cmd.exe drive entries are dropped, everything else is kept exactly");
        // Windows names are case-insensitive, so these would overwrite each other there.
        assert_eq!(entries(&[("Path", "a"), ("PATH", "b")]).is_err(), cfg!(windows));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_saved_note_round_trips_and_is_only_the_users() {
        let (paths, root) = state();
        super::save(&paths).unwrap();
        assert!(!paths.state.join("restart-env.tmp").exists(), "the temporary was renamed into place");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.state.join(ENVIRONMENT)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let vars = super::load(&paths).unwrap().unwrap();
        let path = vars.iter().find(|(key, _)| key.eq_ignore_ascii_case("PATH")).map(|(_, value)| value.clone());
        assert_eq!(path, std::env::var_os("PATH"));
        assert!(vars.iter().all(|(key, _)| super::valid_key(key)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discard_removes_the_note_and_leftover_temporaries_but_nothing_else() {
        let (paths, root) = state();
        super::discard(&paths);
        let removed = [ENVIRONMENT, "restart-env.tmp", "restart-env.0123abcd.tmp"];
        let kept = ["terminals.json", "restart-env.json.bak", "restart-env.tmp.keep", "other.tmp"];
        for name in removed.iter().chain(&kept) {
            std::fs::write(paths.state.join(name), b"x").unwrap();
        }
        super::discard(&paths);
        for name in removed {
            assert!(!paths.state.join(name).exists(), "{name}");
        }
        for name in kept {
            assert!(paths.state.join(name).exists(), "{name}");
        }
        std::fs::create_dir(paths.state.join(ENVIRONMENT)).unwrap();
        super::discard(&paths);
        assert!(super::load(&paths).is_err(), "a directory in its place is not a note");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_note_is_unlinked_without_touching_its_target() {
        let (paths, root) = state();
        let target = root.join("elsewhere.json");
        std::fs::write(&target, b"keep me").unwrap();
        std::os::unix::fs::symlink(&target, paths.state.join(ENVIRONMENT)).unwrap();
        super::discard(&paths);
        assert!(std::fs::symlink_metadata(paths.state.join(ENVIRONMENT)).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        std::os::unix::fs::symlink(root.join("missing.json"), paths.state.join(ENVIRONMENT)).unwrap();
        assert!(super::load(&paths).unwrap().is_none(), "a dangling link is no note");
        std::fs::remove_dir_all(root).unwrap();
    }
}
