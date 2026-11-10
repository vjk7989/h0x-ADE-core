//! `keep_awake`: while any live terminal's root status is `working`, the machine may not go to
//! sleep. Only system sleep is blocked: the display still turns off and the screen still locks.

use crate::runtime::Core;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Whether a sleep block is held now (`server status`).
static HELD: AtomicBool = AtomicBool::new(false);

pub fn held() -> bool {
    HELD.load(Ordering::SeqCst)
}

/// What the operating system gives us: a block on system sleep that is given back on `release`.
pub trait Inhibitor {
    /// Takes the block; false when the system would not give it (the reason is logged).
    fn hold(&mut self) -> bool;
    /// The block is still in force (a helper process may have exited).
    fn alive(&mut self) -> bool;
    fn release(&mut self);
}

/// Decides when the block is held; the OS part is the `Inhibitor`.
pub struct KeepAwake<I: Inhibitor> {
    inhibitor: I,
    /// The block is wanted now. Kept even when it could not be taken: no retry until the next turn.
    held: bool,
    /// The block is really in force.
    took: bool,
}

impl<I: Inhibitor> KeepAwake<I> {
    pub fn new(inhibitor: I) -> Self {
        Self { inhibitor, held: false, took: false }
    }

    /// `enabled`: the setting. `working`: some live terminal's root status is `working`.
    pub fn update(&mut self, enabled: bool, working: bool) {
        let want = enabled && working;
        if want && !self.held {
            self.took = self.inhibitor.hold();
        } else if want && self.took && !self.inhibitor.alive() {
            eprintln!("core: keep_awake: the sleep block ended on its own; nothing keeps the machine awake now");
            self.took = false;
        } else if !want && self.held {
            self.inhibitor.release();
            self.took = false;
        }
        self.held = want;
        HELD.store(self.holding(), Ordering::SeqCst);
    }

    /// The block is in force.
    pub fn holding(&self) -> bool {
        self.held && self.took
    }
}

impl<I: Inhibitor> Drop for KeepAwake<I> {
    fn drop(&mut self) {
        if self.held {
            self.inhibitor.release();
            HELD.store(false, Ordering::SeqCst);
        }
    }
}

/// Windows: `ES_SYSTEM_REQUIRED` lasts as long as the thread that set it, so a thread holds it.
#[cfg(windows)]
#[derive(Default)]
pub struct Os(Option<std::sync::mpsc::Sender<()>>);

#[cfg(windows)]
impl Inhibitor for Os {
    fn hold(&mut self) -> bool {
        use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let spawned = std::thread::Builder::new().name("keep-awake".into()).spawn(move || {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            let _ = wait.recv();
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        });
        match spawned {
            Ok(_) => self.0 = Some(release),
            Err(e) => eprintln!("core: keep_awake: cannot start a thread: {e}"),
        }
        self.0.is_some()
    }

    fn alive(&mut self) -> bool {
        true
    }

    fn release(&mut self) {
        // The thread wakes when its sender goes, and lets go.
        self.0 = None;
    }
}

/// macOS (`caffeinate -i`) and Linux (`systemd-inhibit`): a helper process that ends with us.
#[cfg(unix)]
#[derive(Default)]
pub struct Os(Option<std::process::Child>);

/// A helper that ships with the system, at a fixed place. Never looked up on PATH: a planted
/// binary earlier on it would run with the user's rights. Debug builds only: the integration
/// tests point `H0X_CORE_HELPER_DIRS` at a stand-in; a release build never reads it.
#[cfg(unix)]
fn system_helper(name: &str) -> Option<std::path::PathBuf> {
    let dirs = if cfg!(debug_assertions) { std::env::var("H0X_CORE_HELPER_DIRS").ok() } else { None };
    dirs.as_deref()
        .unwrap_or("/usr/bin:/bin")
        .split(':')
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|p| p.is_file())
}

#[cfg(unix)]
impl Inhibitor for Os {
    fn hold(&mut self) -> bool {
        use std::os::unix::process::CommandExt;
        let pid = std::process::id().to_string();
        let mut helper = if cfg!(target_os = "macos") {
            let mut c = std::process::Command::new("/usr/bin/caffeinate");
            c.args(["-i", "-w", &pid]);
            c
        } else {
            let mut found = Vec::new();
            for name in ["systemd-inhibit", "tail"] {
                match system_helper(name) {
                    Some(path) => found.push(path),
                    None => {
                        eprintln!("core: keep_awake: cannot start {name}: not found in /usr/bin or /bin");
                        return false;
                    }
                }
            }
            let mut c = std::process::Command::new(&found[0]);
            c.args(["--what=sleep", "--mode=block", "--who=h0x-core", "--why=an agent is working"]);
            c.arg(&found[1]).args(["--pid", &pid, "-f", "/dev/null"]);
            c
        };
        // Its own group: release ends the helper and whatever it started (`tail` under systemd-inhibit).
        helper
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match helper.spawn() {
            Ok(child) => self.0 = Some(child),
            Err(e) => eprintln!("core: keep_awake: cannot start {}: {e}", helper.get_program().to_string_lossy()),
        }
        self.0.is_some()
    }

    fn alive(&mut self) -> bool {
        let Some(child) = self.0.as_mut() else { return false };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => {
                // Reaped: its pid may be reused, so release must not signal that group any more.
                self.0 = None;
                false
            }
            Err(_) => false,
        }
    }

    fn release(&mut self) {
        if let Some(mut child) = self.0.take() {
            // The group id is the helper's pid.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Core {
    /// Follows the setting and the terminals: on each tick, hold or release.
    pub async fn watch_awake(self: Arc<Self>) {
        let mut awake = KeepAwake::new(Os::default());
        let mut complained: Option<String> = None;
        loop {
            // A config that cannot be read means off; say so once per distinct error.
            let enabled = match crate::config::load(&self.paths.config) {
                Ok(c) => {
                    complained = None;
                    c.keep_awake.unwrap_or(false)
                }
                Err(e) => {
                    if complained.as_deref() != Some(e.message.as_str()) {
                        eprintln!("core: keep_awake is off until the config is fixed: {}", e.message);
                        complained = Some(e.message);
                    }
                    false
                }
            };
            let working = self
                .lock()
                .reg
                .terminals
                .values()
                .any(|t| t.is_live() && t.status.as_ref().is_some_and(|s| s.state == "working"));
            awake.update(enabled, working);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Fake(Arc<Mutex<Vec<&'static str>>>, Arc<Mutex<(bool, bool)>>);

    impl Fake {
        /// `(takes the block, stays in force)`; the default is both.
        fn behaving(takes: bool, stays: bool) -> Self {
            Fake(Arc::default(), Arc::new(Mutex::new((takes, stays))))
        }
    }

    impl Default for Fake {
        fn default() -> Self {
            Fake::behaving(true, true)
        }
    }

    impl Inhibitor for Fake {
        fn hold(&mut self) -> bool {
            self.0.lock().unwrap().push("hold");
            self.1.lock().unwrap().0
        }
        fn alive(&mut self) -> bool {
            self.1.lock().unwrap().1
        }
        fn release(&mut self) {
            self.0.lock().unwrap().push("release");
        }
    }

    fn log(fake: &Fake) -> Vec<&'static str> {
        std::mem::take(&mut *fake.0.lock().unwrap())
    }

    #[test]
    fn it_holds_only_when_enabled_and_working_and_once() {
        let fake = Fake::default();
        let mut awake = KeepAwake::new(fake.clone());
        awake.update(false, true);
        awake.update(true, false);
        assert!(log(&fake).is_empty(), "off by default; idle holds nothing");
        awake.update(true, true);
        awake.update(true, true);
        assert_eq!(log(&fake), ["hold"], "one block however many ticks");
    }

    #[test]
    fn it_releases_when_work_stops_and_holds_again_for_the_next_turn() {
        let fake = Fake::default();
        let mut awake = KeepAwake::new(fake.clone());
        awake.update(true, true);
        awake.update(true, false);
        awake.update(true, false);
        awake.update(true, true);
        assert_eq!(log(&fake), ["hold", "release", "hold"]);
    }

    #[test]
    fn turning_the_setting_off_releases_at_once_and_on_again_holds() {
        let fake = Fake::default();
        let mut awake = KeepAwake::new(fake.clone());
        awake.update(true, true);
        awake.update(false, true);
        assert_eq!(log(&fake), ["hold", "release"]);
        awake.update(true, true);
        assert_eq!(log(&fake), ["hold"]);
    }

    #[test]
    fn shutdown_releases_a_held_block_and_only_a_held_one() {
        let fake = Fake::default();
        drop(KeepAwake::new(fake.clone()));
        assert!(log(&fake).is_empty());
        let mut awake = KeepAwake::new(fake.clone());
        awake.update(true, true);
        drop(awake);
        assert_eq!(log(&fake), ["hold", "release"]);
    }

    #[test]
    fn rapid_toggling_never_holds_twice_or_releases_what_it_does_not_hold() {
        let fake = Fake::default();
        let mut awake = KeepAwake::new(fake.clone());
        // A fixed pseudo-random walk over (setting, working), including runs of the same input.
        let mut seed: u32 = 0x9e37_79b9;
        let mut want = false;
        for _ in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let (enabled, working) = (seed & 1 == 1, seed & 2 == 2);
            awake.update(enabled, working);
            want = enabled && working;
        }
        let calls = log(&fake);
        assert!(!calls.is_empty());
        for pair in calls.windows(2) {
            assert_ne!(pair[0], pair[1], "a hold is always followed by a release and back: {calls:?}");
        }
        assert_eq!(calls[0], "hold", "nothing to release before the first hold");
        assert_eq!(calls.last() == Some(&"hold"), want, "the last call matches the last input");
        // Shutdown gives back exactly what is still held.
        drop(awake);
        assert_eq!(log(&fake), if want { vec!["release"] } else { vec![] });
    }

    #[test]
    fn a_release_after_the_setting_goes_off_is_not_repeated_at_shutdown() {
        let fake = Fake::default();
        let mut awake = KeepAwake::new(fake.clone());
        awake.update(true, true);
        awake.update(false, true);
        awake.update(false, false);
        drop(awake);
        assert_eq!(log(&fake), ["hold", "release"]);
    }

    #[test]
    fn a_block_the_system_would_not_give_is_not_claimed_nor_retried() {
        let fake = Fake::behaving(false, true);
        let mut awake = KeepAwake::new(fake.clone());
        for _ in 0..3 {
            awake.update(true, true);
            assert!(!awake.holding(), "nothing was taken");
        }
        assert_eq!(log(&fake), ["hold"], "one try per working turn");
        awake.update(true, false);
        awake.update(true, true);
        assert_eq!(log(&fake), ["release", "hold"], "the next turn tries again");
    }

    #[test]
    fn a_helper_that_starts_and_then_dies_stops_being_claimed_until_the_next_turn() {
        let fake = Fake::default();
        let mut awake = KeepAwake::new(fake.clone());
        awake.update(true, true);
        assert!(awake.holding());
        *fake.1.lock().unwrap() = (true, false);
        awake.update(true, true);
        assert!(!awake.holding(), "the block ended on its own");
        awake.update(true, true);
        assert_eq!(log(&fake), ["hold"], "no respawn while the same turn goes on");
        awake.update(true, false);
        *fake.1.lock().unwrap() = (true, true);
        awake.update(true, true);
        assert!(awake.holding());
        assert_eq!(log(&fake), ["release", "hold"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_reaped_helper_is_forgotten_so_release_never_signals_its_old_group() {
        let mut os = Os(Some(std::process::Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().unwrap()));
        while os.alive() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(os.0.is_none(), "the reaped child is dropped");
        os.release();
    }

    #[cfg(unix)]
    #[test]
    fn system_helpers_come_from_fixed_system_dirs_only() {
        let sh = system_helper("sh").unwrap();
        assert!(sh.starts_with("/usr/bin") || sh.starts_with("/bin"), "{sh:?}");
        assert!(system_helper("h0x-no-such-helper").is_none());
    }

    /// Runs under `cargo test --release` only: there the override must be dead. (No other test
    /// can race on the variable: a release build never reads it.)
    #[cfg(all(unix, not(debug_assertions)))]
    #[test]
    fn a_release_build_ignores_the_helper_dir_override() {
        let dir = std::env::temp_dir().join(format!("h0x-core-helper-dirs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("h0x-planted-helper"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("sh"), "#!/bin/sh\n").unwrap();
        unsafe { std::env::set_var("H0X_CORE_HELPER_DIRS", &dir) };
        let planted = system_helper("h0x-planted-helper");
        let sh = system_helper("sh");
        unsafe { std::env::remove_var("H0X_CORE_HELPER_DIRS") };
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(planted, None);
        let sh = sh.unwrap();
        assert!(sh.starts_with("/usr/bin") || sh.starts_with("/bin"), "{sh:?}");
    }
}
