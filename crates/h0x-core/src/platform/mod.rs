//! What differs between Unix (Linux, macOS) and Windows, behind one small
//! surface: connections, terminals, process trees, the console, files.
//! Everything above it is shared.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(target_os = "linux")]
pub mod systemd;

#[cfg(target_os = "macos")]
mod macos_launchd;
#[cfg(target_os = "macos")]
mod macos_shutdown;

/// Run before threads or runtime setup: a launchd launcher forks the core here.
pub fn daemon_startup() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_launchd::startup()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// A parsed serve may have come from an older updater's direct spawn.
pub fn prepare_core() {
    #[cfg(target_os = "macos")]
    macos_launchd::prepare_core();
}

/// Daemons must not keep launchd's 256-fd or systemd's 1024-fd soft limit.
pub(crate) fn raise_nofile_limit() {
    #[cfg(unix)]
    if let Err(error) = unix::raise_nofile_limit() {
        eprintln!("cannot raise open file limit; using inherited limit: {error}");
    }
}

/// macOS cores leave the app's process coalition; holders inherit the core's.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn spawn_core_job(cmd: &mut std::process::Command, log: &std::path::Path) -> bool {
    #[cfg(target_os = "macos")]
    if cmd.get_args().next() == Some(std::ffi::OsStr::new("serve")) {
        return macos_launchd::spawn(cmd, log);
    }
    false
}

/// Keep the early shutdown watch alive for the core's lifetime.
pub fn monitor_host_shutdown() -> impl Sized {
    #[cfg(target_os = "macos")]
    {
        macos_shutdown::monitor()
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
#[cfg(windows)]
mod windows_exec;
#[cfg(windows)]
pub use windows_exec::*;
#[cfg(windows)]
mod windows_env;
#[cfg(windows)]
mod windows_session;

/// Whether the host is shutting down (Windows also includes logout).
pub fn host_shutdown_in_progress() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
        // Keep USER32 statically linked: first loading it during shutdown can interrupt exit recording.
        unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
    }
    #[cfg(target_os = "macos")]
    {
        macos_shutdown::in_progress()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// What marks a process as an ssh login's.
pub const SSH_LOGIN: &[&str] = &["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

/// This process's environment, without what isn't UTF-8.
pub fn utf8_vars() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
}

/// Application settings survive a restart; session paths and lifecycle marks do not move.
pub(crate) fn restored_environment(
    saved: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    current: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    use std::ffi::OsStr;
    let named = |key: &OsStr, name: &str| {
        if cfg!(windows) { key.eq_ignore_ascii_case(OsStr::new(name)) } else { key == name }
    };
    let local = |key: &OsStr| {
        let paths = [
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
            "H0X_CORE_SESSION",
            "H0X_CORE_SOCKET",
        ];
        #[allow(unused_mut)]
        let mut keep = paths.iter().any(|name| named(key, name));
        #[cfg(target_os = "linux")]
        {
            keep |= systemd::SERVICE.iter().any(|name| named(key, name));
        }
        #[cfg(target_os = "macos")]
        {
            keep |= named(key, "XPC_SERVICE_NAME");
        }
        keep
    };
    let discarded = |key: &OsStr| {
        [crate::sweep::VAR, crate::sweep::RUNTIME, "H0X_CORE_LAUNCHD_DIRECT", "H0X_CORE_LAUNCHD_DETACH"]
            .iter()
            .any(|name| named(key, name))
    };
    let mut env: std::collections::BTreeMap<_, _> =
        saved.into_iter().filter(|(key, _)| !local(key) && !discarded(key)).collect();
    env.extend(current.into_iter().filter(|(key, _)| local(key) && !discarded(key)));
    env.insert(crate::sweep::RUNTIME.into(), "1".into());
    env
}

/// The core's environment as programs get it: never an ssh login's marks,
/// and on Linux the login session as the user manager has it now.
pub async fn program_env() -> std::collections::BTreeMap<String, String> {
    #[allow(unused_mut)]
    let mut env: std::collections::BTreeMap<String, String> = utf8_vars().collect();
    #[cfg(target_os = "linux")]
    {
        env.retain(|k, _| !systemd::SERVICE.contains(&k.as_str()));
        if let Ok(Some(manager)) = tokio::task::spawn_blocking(systemd::manager_env).await {
            systemd::apply_session(&mut env, &manager);
        }
    }
    #[cfg(windows)]
    {
        env = tokio::task::spawn_blocking(move || {
            if let Err(error) = windows_env::refresh(&mut env) {
                eprintln!("cannot refresh Windows environment; using inherited settings: {error}");
            }
            env
        })
        .await
        .expect("environment reader panicked");
    }
    env.retain(|k, _| !SSH_LOGIN.contains(&k.as_str()));
    drop_unreachable_displays(&mut env);
    env
}

/// 32 random hex digits (ids, socket and folder names).
pub fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Signal numbers as the holder protocol carries them (the Unix ones; the
/// Windows holder maps them onto its own means).
pub mod signal {
    pub const HUP: i32 = 1;
    pub const INT: i32 = 2;
    pub const KILL: i32 = 9;
    pub const TERM: i32 = 15;
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod environment_tests;

/// Where tests keep a core's files. Unix socket paths are length-limited and the macOS temp dir is
/// long, so unix uses `/var/tmp`. Windows has neither `/var/tmp` (it would be `<drive>:\var\tmp`)
/// nor a TEMP that is safe (it may be on C:): it uses a dir next to the test binary, in the cargo
/// target dir.
#[cfg(test)]
pub fn test_scratch() -> std::path::PathBuf {
    if cfg!(windows) {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().and_then(|deps| deps.parent()).unwrap().join("h0x-test-scratch");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    } else {
        "/var/tmp".into()
    }
}
