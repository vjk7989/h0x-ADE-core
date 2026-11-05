//! What runs in a terminal right now: its foreground process and that
//! process's folder. Raw facts, read about twice a second; clients name and
//! draw programs themselves.

use crate::runtime::{Core, State};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Foreground {
    /// The foreground process group's leader.
    pub pid: i32,
    /// Its name as the OS has it (`node`, `vim`, `sh`).
    pub name: String,
    pub argv: Vec<String>,
    /// Its working directory now (resolved by the OS).
    pub cwd: String,
}

/// The facts about `pid`, if it is still there to read.
#[cfg(target_os = "linux")]
pub fn about(pid: i32) -> Option<Foreground> {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?.trim_end().to_string();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv =
        cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?.display().to_string();
    Some(Foreground { pid, name, argv, cwd })
}

/// A pseudo console has no foreground group: this is the terminal's first
/// process, with the folder and command line Windows keeps in its memory.
/// None for a 32-bit process, whose own copy of them lives elsewhere.
#[cfg(windows)]
pub fn about(pid: i32) -> Option<Foreground> {
    use std::ffi::c_void;
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows_sys::Win32::Foundation::{HANDLE, LocalFree, UNICODE_STRING};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid as u32) };
    if raw.is_null() {
        return None;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let h = process.as_raw_handle() as HANDLE;
    let mut wow64 = 0;
    if unsafe { IsWow64Process(h, &mut wow64) } == 0 || wow64 != 0 {
        return None;
    }
    let read = |at: *const c_void, into: *mut c_void, len: usize| {
        let mut n = 0;
        unsafe { ReadProcessMemory(h, at, into, len, &mut n) != 0 && n == len }
    };
    fn value<T>(read: impl Fn(*const c_void, *mut c_void, usize) -> bool, at: *const c_void) -> Option<T> {
        let mut v = MaybeUninit::<T>::uninit();
        read(at, v.as_mut_ptr().cast(), size_of::<T>()).then(|| unsafe { v.assume_init() })
    }
    let text = |s: UNICODE_STRING| -> Option<Vec<u16>> {
        let mut buf = vec![0u16; s.Length as usize / 2];
        read(s.Buffer.cast(), buf.as_mut_ptr().cast(), buf.len() * 2).then_some(buf)
    };

    let mut basic: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let size = size_of::<PROCESS_BASIC_INFORMATION>() as u32;
    let status =
        unsafe { NtQueryInformationProcess(h, ProcessBasicInformation, (&raw mut basic).cast(), size, &mut 0) };
    if status != 0 || basic.PebBaseAddress.is_null() {
        return None;
    }
    let peb: PEB = value(read, basic.PebBaseAddress.cast())?;
    let params: RTL_USER_PROCESS_PARAMETERS = value(read, peb.ProcessParameters.cast())?;
    // CurrentDirectory.DosPath: right after the standard handles, in what the SDK calls Reserved2.
    let dir: UNICODE_STRING = value(read, peb.ProcessParameters.cast::<u8>().wrapping_add(0x38).cast())?;
    let mut cwd = String::from_utf16(&text(dir)?).ok()?;
    if cwd.len() > 3 && cwd.ends_with('\\') {
        cwd.pop(); // Windows keeps C:\dir\; only a drive's root has one
    }
    let image = String::from_utf16(&text(params.ImagePathName)?).ok()?;
    let name = image.rsplit('\\').next().unwrap_or(&image).to_string();

    let mut line = text(params.CommandLine)?;
    line.push(0);
    let mut argc = 0;
    let list = unsafe { CommandLineToArgvW(line.as_ptr(), &mut argc) };
    if list.is_null() {
        return None;
    }
    let argv = (0..argc as usize)
        .map(|i| unsafe {
            let arg = *list.add(i);
            String::from_utf16_lossy(std::slice::from_raw_parts(arg, (0..).take_while(|&n| *arg.add(n) != 0).count()))
        })
        .collect();
    unsafe { LocalFree(list.cast()) };
    Some(Foreground { pid, name, argv, cwd })
}

#[cfg(target_os = "macos")]
pub fn about(pid: i32) -> Option<Foreground> {
    let mut name = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
    let name = String::from_utf8_lossy(&name[..n.max(0) as usize]).into_owned();
    let mut vnode: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    if unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut vnode).cast(), size) } != size {
        return None;
    }
    let path = unsafe { std::ffi::CStr::from_ptr(vnode.pvi_cdir.vip_path.as_ptr().cast()) };
    let (argv, _) = procargs(pid)?;
    Some(Foreground { pid, name, argv, cwd: path.to_string_lossy().into_owned() })
}

/// KERN_PROCARGS2: argc, the executable path, the arguments, then the
/// environment, each NUL-terminated. Returns (arguments, environment).
#[cfg(target_os = "macos")]
pub fn procargs(pid: i32) -> Option<(Vec<String>, Vec<u8>)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let call = |buf: *mut libc::c_void, size: &mut libc::size_t, mib: &mut [i32; 3]| unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buf, size, std::ptr::null_mut(), 0)
    };
    if call(std::ptr::null_mut(), &mut size, &mut mib) != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    if call(buf.as_mut_ptr().cast(), &mut size, &mut mib) != 0 {
        return None;
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let mut rest = &buf[4..];
    let one = |r: &[u8]| -> Option<usize> { r.iter().position(|b| *b == 0) };
    rest = &rest[one(rest)? + 1..]; // the executable path
    while rest.first() == Some(&0) {
        rest = &rest[1..]; // its padding
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = one(rest)?;
        argv.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = &rest[end + 1..];
    }
    Some((argv, rest.to_vec()))
}

impl Core {
    /// Keeps every live terminal's `foreground` current, with an event on each change.
    pub async fn watch_foreground(self: Arc<Self>) {
        let ms = std::env::var("H0X_CORE_FOREGROUND_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
        loop {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let live: Vec<u32> = self.lock().reg.terminals.values().filter(|t| t.is_live()).map(|t| t.id()).collect();
            for id in live {
                let pgid = self.foreground(id).await;
                let now = (pgid > 0).then(|| about(pgid)).flatten();
                self.foreground_is(&mut self.lock(), id, now);
            }
        }
    }

    /// Records what is in front of terminal `id` now. A program its shell
    /// started takes its mouse and focus reports with it when it hands the
    /// terminal back: one that crashed or never turned them off would have
    /// every move of the mouse typed at the shell's prompt.
    pub(crate) fn foreground_is(&self, st: &mut State, id: u32, now: Option<Foreground>) {
        let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.is_live() && t.foreground != now) else {
            return;
        };
        let back = back_at_shell(t.pid, t.foreground.as_ref(), now.as_ref());
        if let Some(f) = &now {
            t.last_cwd = Some(f.cwd.clone());
        }
        t.foreground = now.clone();
        self.save_soon();
        self.emit(st, "foreground", id, serde_json::to_value(&now).unwrap_or_default());
        if back {
            self.end_input_reports(st, id);
        }
    }

    /// Turns off the reports still on, on the screen and in every viewer
    /// (one attaching later is painted from the screen).
    fn end_input_reports(&self, st: &mut State, id: u32) {
        let Some(screen) = st.screens.get(&id) else { return };
        let on: Vec<u16> = INPUT_REPORTS.into_iter().filter(|m| screen.dec_mode(*m)).collect();
        // Ours would land inside a sequence still arriving, and break it.
        // (No continuation: one too long to track; an empty one: at ground.)
        let midway =
            st.relays.get(&id).is_some_and(|r| !r.idle()) || screen.continuation().is_none_or(|c| !c.is_empty());
        if on.is_empty() || midway {
            return;
        }
        let off: String = on.iter().map(|m| format!("\x1b[?{m}l")).collect();
        self.output(st, id, off.as_bytes());
    }
}

/// Mouse reports (X10, normal, button and any motion, and their encodings)
/// and focus reports. Bracketed paste (2004) stays: bash and zsh turn it on
/// at their own prompt, which a reset arriving after it would undo, letting
/// a paste run its lines.
const INPUT_REPORTS: [u16; 9] = [9, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1016];

/// Whether a program other than the terminal's first process (its shell)
/// was in front and the shell is now. Never on Windows, whose pseudo console
/// always has the first process in front: PowerShell's prompt ends the
/// reports there (`own_shell`).
fn back_at_shell(first: Option<u32>, before: Option<&Foreground>, now: Option<&Foreground>) -> bool {
    let Some(first) = first.map(|p| p as i32) else { return false };
    before.is_some_and(|b| b.pid != first) && now.is_some_and(|n| n.pid == first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(pid: i32) -> Option<Foreground> {
        Some(Foreground { pid, name: "p".into(), argv: vec!["p".into()], cwd: "/".into() })
    }

    #[test]
    fn only_the_shell_taking_the_terminal_back_counts() {
        let back = |first, before: Option<Foreground>, now: Option<Foreground>| {
            back_at_shell(first, before.as_ref(), now.as_ref())
        };
        assert!(back(Some(10), front(20), front(10)), "a program handed back to the shell");
        assert!(!back(Some(10), front(10), front(20)), "the shell started a program");
        assert!(!back(Some(10), front(20), front(30)), "one program after another");
        assert!(!back(Some(10), None, front(10)), "first look: nothing was seen in front");
        assert!(!back(Some(10), front(20), None), "nothing readable in front");
        assert!(!back(None, front(20), front(10)), "no first process (not started yet)");
        // Windows: the pseudo console's first process is always in front.
        assert!(!back(Some(10), front(10), front(10)));
    }

    fn core_with_viewer() -> (Arc<Core>, u32, Arc<crate::wire::Conn>, u32, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("h0x-core-reports-{}", crate::platform::random_hex()));
        let paths = crate::paths::Paths {
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
        let id = {
            let mut st = core.lock();
            let new =
                crate::terminals::NewTerminal { argv: vec!["sh".into()], cols: 80, rows: 24, ..Default::default() };
            let id = st.reg.create(new, 0).unwrap();
            st.reg.terminals.get_mut(&id).unwrap().pid = Some(10);
            st.screens.insert(id, ghostty_vt::Terminal::new(80, 24, 1 << 20));
            id
        };
        let conn = Arc::new(crate::wire::Conn::default());
        let request = crate::viewers::AttachRequest {
            observe: false,
            resize_notifications: false,
            history: 0,
            size: (80, 24),
            display: Default::default(),
            resumed: false,
        };
        let channel = core.attach(&conn, id, request).unwrap();
        (core, id, conn, channel, root)
    }

    /// Everything the viewer was sent since the last call.
    fn sent(conn: &crate::wire::Conn, channel: u32) -> Vec<u8> {
        let mut out = Vec::new();
        conn.with(|ob| {
            ob.resume(channel);
            while let Some(frame) = ob.next_frame() {
                if frame[4] == crate::wire::DATA {
                    out.extend_from_slice(&frame[crate::wire::HEADER..]);
                }
            }
        });
        out
    }

    #[test]
    fn a_program_that_left_its_mouse_reports_on_loses_them_when_the_shell_is_back() {
        let (core, id, conn, channel, root) = core_with_viewer();
        let mut viewer = ghostty_vt::Terminal::new(80, 24, 0);
        viewer.write(&sent(&conn, channel));
        let modes = b"\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h";
        {
            let mut st = core.lock();
            core.foreground_is(&mut st, id, front(10));
            core.foreground_is(&mut st, id, front(20));
            core.output(&mut st, id, modes);
            // Still in front, even moving to another folder: its modes stay.
            core.foreground_is(&mut st, id, Some(Foreground { cwd: "/elsewhere".into(), ..front(20).unwrap() }));
            assert!(st.screens[&id].dec_mode(1003) && st.screens[&id].dec_mode(1004));
            assert!(!st.screens[&id].wheel(true).is_empty(), "the program gets the mouse");
        }
        viewer.write(&sent(&conn, channel));
        assert!(viewer.dec_mode(1003) && viewer.dec_mode(1006));

        // It exited (or crashed) without turning them off.
        core.foreground_is(&mut core.lock(), id, front(10));
        let st = core.lock();
        let screen = &st.screens[&id];
        for mode in INPUT_REPORTS {
            assert!(!screen.dec_mode(mode), "?{mode} still on");
        }
        assert!(screen.wheel(true).is_empty(), "no mouse report reaches the shell");
        assert!(screen.dec_mode(2004), "bracketed paste is the shell's to set");
        drop(st);
        viewer.write(&sent(&conn, channel));
        assert!(!viewer.dec_mode(1003) && !viewer.dec_mode(1006) && !viewer.dec_mode(1004), "the viewer heard it");
        assert!(viewer.wheel(true).is_empty());
        // A tab shown again attaches anew: its paint has them off too.
        let mut fresh = ghostty_vt::Terminal::new(80, 24, 0);
        fresh.write(&core.lock().screens[&id].paint(0));
        assert!(!fresh.dec_mode(1003) && !fresh.dec_mode(1006) && fresh.dec_mode(2004));
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn nothing_is_sent_when_no_report_was_left_on_or_a_sequence_is_arriving() {
        let (core, id, conn, channel, root) = core_with_viewer();
        sent(&conn, channel);
        {
            let mut st = core.lock();
            core.foreground_is(&mut st, id, front(20));
            core.foreground_is(&mut st, id, front(10));
        }
        assert!(sent(&conn, channel).is_empty(), "a clean program leaves nothing to undo");

        let mut st = core.lock();
        core.foreground_is(&mut st, id, front(20));
        core.output(&mut st, id, b"\x1b[?1003h\x1b[?1006h\x1b]0;half a tit");
        core.foreground_is(&mut st, id, front(10));
        assert!(st.screens[&id].dec_mode(1003), "never written into the middle of a sequence");
        drop(st);
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }
}
