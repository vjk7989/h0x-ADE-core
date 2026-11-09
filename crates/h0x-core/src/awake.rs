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
    fn hold(&mut self);
    fn release(&mut self);
}

/// Decides when the block is held; the OS part is the `Inhibitor`.
pub struct KeepAwake<I: Inhibitor> {
    inhibitor: I,
    held: bool,
}

impl<I: Inhibitor> KeepAwake<I> {
    pub fn new(inhibitor: I) -> Self {
        Self { inhibitor, held: false }
    }

    /// `enabled`: the setting. `working`: some live terminal's root status is `working`.
    pub fn update(&mut self, enabled: bool, working: bool) {
        let want = enabled && working;
        if want && !self.held {
            self.inhibitor.hold();
        } else if !want && self.held {
            self.inhibitor.release();
        }
        self.held = want;
        HELD.store(want, Ordering::SeqCst);
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
    fn hold(&mut self) {
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

#[cfg(unix)]
impl Inhibitor for Os {
    fn hold(&mut self) {
        let pid = std::process::id().to_string();
        let mut helper = if cfg!(target_os = "macos") {
            let mut c = std::process::Command::new("caffeinate");
            c.args(["-i", "-w", &pid]);
            c
        } else {
            let mut c = std::process::Command::new("systemd-inhibit");
            c.args(["--what=sleep", "--mode=block", "--who=h0x-core", "--why=an agent is working"]);
            c.args(["tail", "--pid", &pid, "-f", "/dev/null"]);
            c
        };
        helper
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match helper.spawn() {
            Ok(child) => self.0 = Some(child),
            Err(e) => eprintln!("core: keep_awake: cannot start {}: {e}", helper.get_program().to_string_lossy()),
        }
    }

    fn release(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Core {
    /// Follows the setting and the terminals: on each tick, hold or release.
    pub async fn watch_awake(self: Arc<Self>) {
        let mut awake = KeepAwake::new(Os::default());
        loop {
            let enabled = crate::config::load(&self.paths.config).is_ok_and(|c| c.keep_awake.unwrap_or(false));
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

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Vec<&'static str>>>);

    impl Inhibitor for Fake {
        fn hold(&mut self) {
            self.0.lock().unwrap().push("hold");
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
}
