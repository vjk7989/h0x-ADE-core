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

/// A pseudo console has no foreground group: this is the program the
/// platform put in front, with the folder and command line Windows keeps in
/// its memory. None for a 32-bit process, whose own copy of them lives elsewhere.
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

fn shell_root(root: &Foreground) -> bool {
    // Command and script wrappers can share their foreground group with a live agent.
    if root.argv.iter().skip(1).any(|arg| {
        (!arg.starts_with('-') && !arg.starts_with('+'))
            || matches!(arg.split('=').next(), Some("--command" | "--commands"))
            || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('c'))
    }) {
        return false;
    }
    let program =
        if root.name.is_empty() { root.argv.first().map(String::as_str).unwrap_or_default() } else { &root.name };
    let name = program.rsplit('/').next().unwrap_or(program);
    matches!(name, "sh" | "bash" | "zsh" | "fish" | "nu" | "dash" | "ksh" | "tcsh")
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
        let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.is_live()) else {
            return;
        };
        // A child that exited back to the shell leaves its status behind: clear it.
        let at_shell = crate::platform::foreground_tracks_jobs()
            && now.as_ref().is_some_and(|n| Some(n.pid as u32) == t.pid && shell_root(n));
        let change = (at_shell && (t.status.is_some() || !t.records.is_empty())).then(|| {
            t.first_task_turn = None;
            crate::status::reset(t)
        });
        let foreground_changed = t.foreground != now;
        let back = foreground_changed && back_at_shell(t.pid, t.foreground.as_ref(), now.as_ref());
        if let Some(f) = &now {
            t.last_cwd = Some(f.cwd.clone());
        }
        t.foreground = now.clone();
        let agent_changed = note_agent(t, now.as_ref(), at_shell && !crate::platform::host_shutdown_in_progress());
        if change.is_some() || foreground_changed || agent_changed {
            self.save_soon();
        }
        if let Some(change) = change {
            self.status_changed(st, id, change);
        }
        if foreground_changed {
            self.emit(st, "foreground", id, serde_json::to_value(&now).unwrap_or_default());
        }
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

/// Records the agent now in front, or clears it once the shell has the
/// terminal back (`at_shell`, never during host shutdown). True if it changed.
fn note_agent(t: &mut crate::terminals::Terminal, now: Option<&Foreground>, at_shell: bool) -> bool {
    let run = now.and_then(|n| crate::sessions::detect(&n.argv, crate::runtime::now_ms()));
    let next = match run {
        Some(mut run) => {
            if let Some(old) = t.agent.as_ref().filter(|old| old.agent == run.agent) {
                run.started_ms = old.started_ms;
            }
            Some(run)
        }
        None if at_shell => None,
        None => return false,
    };
    let changed = t.agent != next;
    t.agent = next;
    changed
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

    fn with_argv(argv: &[&str]) -> Option<Foreground> {
        Some(Foreground {
            pid: 20,
            name: "x".into(),
            argv: argv.iter().map(|a| a.to_string()).collect(),
            cwd: "/".into(),
        })
    }

    fn terminal() -> crate::terminals::Terminal {
        let mut reg = crate::terminals::Registry::default();
        let id =
            reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap();
        reg.terminals.remove(&id).unwrap()
    }

    const UUID: &str = "123e4567-e89b-42d3-a456-426614174000";

    #[test]
    fn agent_run_records_the_agent_and_its_explicit_id_but_never_a_prompt() {
        let mut t = terminal();
        let now = with_argv(&["/usr/bin/codex", "resume", UUID, "secret prompt"]);
        assert!(note_agent(&mut t, now.as_ref(), false));
        let run = t.agent.clone().unwrap();
        assert_eq!((run.agent.as_str(), run.id.as_deref()), ("codex", Some(UUID)));
        assert!(run.flags.is_empty() && run.started_ms > 0);
        assert!(!serde_json::to_string(&t).unwrap().contains("secret"));
        // The same program again changes nothing, and keeps when it started.
        t.agent.as_mut().unwrap().started_ms = 7;
        assert!(!note_agent(&mut t, now.as_ref(), false));
        assert_eq!(t.agent.as_ref().unwrap().started_ms, 7);
    }

    #[test]
    fn agent_run_without_an_id_or_with_a_bad_one_has_none() {
        let mut t = terminal();
        note_agent(&mut t, with_argv(&["codex", "fix it"]).as_ref(), false);
        assert_eq!(t.agent.as_ref().unwrap().id, None);
        note_agent(&mut t, with_argv(&["codex", "resume", "--evil"]).as_ref(), false);
        assert_eq!(t.agent.as_ref().unwrap().id, None);
        note_agent(&mut t, with_argv(&["claude", "--session-id", UUID]).as_ref(), false);
        assert_eq!(t.agent.as_ref().unwrap().id.as_deref(), Some(UUID));
    }

    #[test]
    fn agent_run_knows_scripts_by_their_stem_and_ignores_other_programs() {
        let mut t = terminal();
        note_agent(&mut t, with_argv(&["/usr/bin/node", "/x/bin/codex.js", "resume", UUID]).as_ref(), false);
        assert_eq!(t.agent.as_ref().unwrap().agent, "codex");
        let mut t = terminal();
        for argv in [
            &["node", "/x/server.js", "codex"][..],
            &["node", "--inspect"],
            &["vim", "codex"],
            &["cursor", "."],
            &["amp"],
        ] {
            assert!(!note_agent(&mut t, with_argv(argv).as_ref(), true), "{argv:?}");
            assert!(t.agent.is_none());
        }
        note_agent(&mut t, with_argv(&["cursor-agent"]).as_ref(), false);
        assert_eq!(t.agent.as_ref().unwrap().agent, "cursor");
    }

    #[test]
    fn agent_run_clears_only_at_the_shell() {
        let mut t = terminal();
        note_agent(&mut t, with_argv(&["codex"]).as_ref(), false);
        assert!(!note_agent(&mut t, with_argv(&["git", "status"]).as_ref(), false), "a child in front");
        assert!(!note_agent(&mut t, None, false), "nothing readable in front");
        assert!(t.agent.is_some());
        assert!(note_agent(&mut t, with_argv(&["bash"]).as_ref(), true), "back at the shell");
        assert!(t.agent.is_none());
    }

    fn agent_of(argv: &[&str]) -> Option<crate::terminals::AgentRun> {
        let mut t = terminal();
        note_agent(&mut t, with_argv(argv).as_ref(), false);
        t.agent
    }

    #[test]
    fn agent_run_edge_empty_or_pathless_argv_is_no_agent_and_changes_nothing() {
        for argv in [&[][..], &[""], &["/"], &["\\"], &["", "codex"], &["/", "resume", UUID]] {
            assert_eq!(agent_of(argv), None, "{argv:?}");
        }
        // Not at the shell, a nameless program leaves a recorded agent as it was.
        let mut t = terminal();
        note_agent(&mut t, with_argv(&["codex"]).as_ref(), false);
        let before = t.agent.clone();
        for argv in [&[][..], &[""], &["/"]] {
            assert!(!note_agent(&mut t, with_argv(argv).as_ref(), false), "{argv:?}");
            assert_eq!(t.agent, before, "{argv:?}");
        }
    }

    #[test]
    fn agent_run_edge_windows_launcher_paths_name_their_agent_on_every_host() {
        for argv in [
            &[r"C:\Users\x\AppData\Roaming\npm\codex.cmd", "resume", UUID][..],
            &[r"C:\Users\x\AppData\Roaming\npm\codex.ps1", "resume", UUID],
            &[r"C:\Program Files\codex\codex.exe", "resume", UUID],
            &[r"\\server\share\bin\codex.exe", "resume", UUID],
            &["C:/Users/x/AppData/Roaming/npm/codex.cmd", "resume", UUID],
        ] {
            let run = agent_of(argv).unwrap_or_else(|| panic!("{argv:?}"));
            assert_eq!((run.agent.as_str(), run.id.as_deref()), ("codex", Some(UUID)), "{argv:?}");
        }
        // An interpreter given as a Windows path still runs a script by its stem.
        let run = agent_of(&[
            r"C:\Program Files\nodejs\node.exe",
            r"C:\Users\x\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js",
        ])
        .unwrap();
        assert_eq!(run.agent, "codex");
        // Only the three launcher suffixes are stripped.
        for argv in [&["codex.bat"][..], &["codex.sh"], &["codex.exe.bak"], &["codexx"], &["xcodex"]] {
            assert_eq!(agent_of(argv), None, "{argv:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn agent_run_edge_names_are_case_insensitive_on_windows() {
        for argv in [&["CLAUDE.EXE"][..], &[r"C:\Tools\Claude.Exe"], &["CODEX.CMD"], &["Codex"]] {
            assert!(agent_of(argv).is_some(), "{argv:?}");
        }
        assert_eq!(agent_of(&["CLAUDE.EXE"]).unwrap().agent, "claude");
    }

    #[cfg(not(windows))]
    #[test]
    fn agent_run_edge_names_are_case_sensitive_off_windows() {
        for argv in [&["CLAUDE.EXE"][..], &["Claude"], &["CODEX"], &["/usr/bin/Codex"]] {
            assert_eq!(agent_of(argv), None, "{argv:?}");
        }
    }

    #[test]
    fn agent_run_edge_interpreters_need_a_script_named_as_the_agent() {
        for argv in [
            &["node"][..],
            &["/usr/bin/node"],
            &["node", ""],
            &["node", "-e", "codex"],
            &["bun", "--smol"],
            &["node", "/x/codex-helper.js"],
            &["node", "/x/my-codex.js"],
            &["node", "/x/codex.min.js"],
            &["node", "/x/claude-code/cli.js"],
            &["bash", "-c", "codex resume 123e4567-e89b-42d3-a456-426614174000"],
            &["sh"],
        ] {
            assert_eq!(agent_of(argv), None, "{argv:?}");
        }
        for (argv, agent) in [
            (&["nodejs", "/x/codex.js"][..], "codex"),
            (&["bun", "/x/claude.ts"], "claude"),
            (&["node", "/x/bin/codex"], "codex"),
            (&["sh", "/x/codex.sh"], "codex"),
        ] {
            assert_eq!(agent_of(argv).map(|r| r.agent).as_deref(), Some(agent), "{argv:?}");
        }
        // The script's own words are its arguments: the id after it is read.
        let run = agent_of(&["node", "/x/codex.js", "resume", UUID]).unwrap();
        assert_eq!(run.id.as_deref(), Some(UUID));
    }

    #[test]
    fn agent_run_edge_malformed_ids_are_none() {
        for bad in [
            "123e4567-e89b-42d3-a456-42661417400",   // one short
            "123e4567-e89b-42d3-a456-4266141740000", // one long
            "123e4567e89b42d3a456426614174000",      // no dashes
            "{123e4567-e89b-42d3-a456-426614174000}",
            "123e4567-e89b-42d3-a456-42661417400g",
            "123e4567_e89b_42d3_a456_426614174000",
            "",
            "--last",
            "../123e4567-e89b-42d3-a456-426614174000",
        ] {
            assert_eq!(agent_of(&["codex", "resume", bad]).unwrap().id, None, "{bad:?}");
            assert_eq!(agent_of(&["claude", "--resume", bad]).unwrap().id, None, "{bad:?}");
            assert_eq!(agent_of(&["claude", "--session-id", bad]).unwrap().id, None, "{bad:?}");
        }
        // A UUID that is a prompt, or a word of one, or after another word, is no id.
        for argv in [
            &["codex", "fix", UUID][..],
            &["codex", format!("resume {UUID}").as_str()],
            &["claude", UUID],
            &["claude", "--model", UUID],
            &["codex", "--resume", UUID],
        ] {
            assert_eq!(agent_of(argv).unwrap().id, None, "{argv:?}");
        }
    }

    #[test]
    fn agent_run_edge_the_nil_uuid_is_no_id() {
        let nil = "00000000-0000-0000-0000-000000000000";
        assert_eq!(agent_of(&["codex", "resume", nil]).unwrap().id, None);
        assert_eq!(agent_of(&["claude", "--resume", nil]).unwrap().id, None);
    }

    #[test]
    fn agent_run_edge_inline_resume_and_session_flags_name_the_id() {
        let id = |argv: &[&str]| agent_of(argv).unwrap().id;
        assert_eq!(id(&["claude", format!("--resume={UUID}").as_str()]).as_deref(), Some(UUID));
        assert_eq!(id(&["claude", format!("--session-id={UUID}").as_str()]).as_deref(), Some(UUID));
        assert_eq!(id(&["copilot", format!("--resume={UUID}").as_str()]).as_deref(), Some(UUID));
    }

    #[test]
    fn agent_run_edge_flags_never_hold_prompts_settings_or_session_flags() {
        let run = agent_of(&[
            "claude",
            "--settings",
            "/tmp/x.json",
            "--settings=/tmp/y.json",
            "-p",
            "say hi",
            "--print",
            "--continue",
            "--fork-session",
            "--output-format",
            "json",
            "--resume",
            UUID,
            "$(rm -rf ~)",
            "a\nb",
        ])
        .unwrap();
        assert!(run.flags.is_empty(), "{:?}", run.flags);
        assert_eq!(run.id.as_deref(), Some(UUID));
        // Past both caps (33 tokens, a 4097 byte value): still nothing kept, nothing panics.
        let mut argv = vec!["codex".to_string()];
        argv.extend(std::iter::repeat_n("--yolo".to_string(), 40));
        argv.extend(["--model".to_string(), "x".repeat(4097)]);
        let mut t = terminal();
        let now = Some(Foreground { pid: 20, name: "x".into(), argv, cwd: "/".into() });
        note_agent(&mut t, now.as_ref(), false);
        let run = t.agent.unwrap();
        assert!(run.flags.len() <= 32 && run.flags.iter().all(|f| f.len() <= 4096 + 16), "{:?}", run.flags.len());
    }

    #[test]
    fn agent_run_edge_started_ms_resets_only_when_the_agent_changes() {
        let mut t = terminal();
        note_agent(&mut t, with_argv(&["codex"]).as_ref(), false);
        t.agent.as_mut().unwrap().started_ms = 7;
        // The same agent with a new id: changed, but it started when it started.
        assert!(note_agent(&mut t, with_argv(&["codex", "resume", UUID]).as_ref(), false));
        assert_eq!(t.agent.as_ref().unwrap().started_ms, 7);
        assert_eq!(t.agent.as_ref().unwrap().id.as_deref(), Some(UUID));
        // A child in front, then the agent again: still the same start.
        assert!(!note_agent(&mut t, with_argv(&["git", "log"]).as_ref(), false));
        assert!(!note_agent(&mut t, with_argv(&["codex", "resume", UUID]).as_ref(), false));
        assert_eq!(t.agent.as_ref().unwrap().started_ms, 7);
        // Another agent: a new start.
        assert!(note_agent(&mut t, with_argv(&["claude"]).as_ref(), false));
        let run = t.agent.clone().unwrap();
        assert_eq!(run.agent, "claude");
        assert_ne!(run.started_ms, 7);
        assert_eq!(run.id, None, "the old agent's id is not carried over");
        // At the shell with nothing recorded: no change, no save.
        let mut t = terminal();
        assert!(!note_agent(&mut t, with_argv(&["bash"]).as_ref(), true));
        assert!(!note_agent(&mut t, None, true));
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

    #[test]
    fn command_flags_keep_shell_roots_from_clearing_live_status() {
        for shell in ["sh", "bash", "zsh", "fish", "nu", "dash", "ksh", "tcsh"] {
            let mut root = Foreground { pid: 1, name: shell.into(), argv: vec![shell.into()], cwd: "/".into() };
            assert!(shell_root(&root), "{shell}");
            for arg in ["-i", "-li", "--login", "--no-config"] {
                root.argv = vec![shell.into(), arg.into()];
                assert!(shell_root(&root), "{shell} {arg}");
            }
            for arg in ["-c", "-cecho", "-ic echo", "--command", "--command=echo", "--commands", "--commands=echo"] {
                root.argv = vec![shell.into(), arg.into()];
                assert!(!shell_root(&root), "{shell} {arg}");
            }
        }
    }

    #[test]
    fn a_shell_root_is_known_by_its_name_or_else_its_first_argument() {
        let root = |name: &str, argv: &[&str]| Foreground {
            pid: 1,
            name: name.into(),
            argv: argv.iter().map(|a| a.to_string()).collect(),
            cwd: "/".into(),
        };
        assert!(shell_root(&root("", &["/usr/bin/zsh"])), "no name: the path's last part");
        assert!(shell_root(&root("bash", &["-bash"])), "a login shell's dash name");
        assert!(shell_root(&root("bash", &[])), "no arguments at all");
        assert!(shell_root(&root("zsh", &["zsh", "+o", "-l"])));
        assert!(!shell_root(&root("", &[])), "nothing to go by");
        assert!(!shell_root(&root("", &["-bash"])), "only a dash name, no process name");
        for other in ["python3", "node", "Bash", "pwsh", "cmd.exe", "bash.exe", "shell"] {
            assert!(!shell_root(&root(other, &[other])), "{other}");
        }
        assert!(!shell_root(&root("bash", &["bash", "script.sh"])), "a script runs in it");
        assert!(!shell_root(&root("bash", &["bash", "-x", "script.sh"])));
    }

    #[test]
    fn a_child_exiting_back_to_the_shell_clears_status_where_jobs_are_seen() {
        let (core, id, _conn, _channel, root) = core_with_viewer();
        let shell = |argv: &[&str]| {
            Some(Foreground {
                pid: 10,
                name: "sh".into(),
                argv: argv.iter().map(|a| a.to_string()).collect(),
                cwd: "/".into(),
            })
        };
        let working = || crate::status::Status {
            state: "working".into(),
            kind: None,
            progress: None,
            app: String::new(),
            title: String::new(),
            msg: String::new(),
        };
        let mut st = core.lock();
        st.reg.terminals.get_mut(&id).unwrap().status = Some(working());
        core.foreground_is(&mut st, id, shell(&["sh"]));
        let cleared = st.reg.terminals[&id].status.is_none();
        assert_eq!(cleared, crate::platform::foreground_tracks_jobs(), "Windows cannot tell the shell from its jobs");

        // Not the shell: another pid, a command wrapper, or nothing readable.
        for now in [front(20), shell(&["sh", "-c", "agent"]), None] {
            st.reg.terminals.get_mut(&id).unwrap().status = Some(working());
            core.foreground_is(&mut st, id, now.clone());
            assert!(st.reg.terminals[&id].status.is_some(), "{now:?} kept the status");
        }
        // The same shell seen again: still cleared, though nothing else changed.
        core.foreground_is(&mut st, id, shell(&["sh"]));
        st.reg.terminals.get_mut(&id).unwrap().status = Some(working());
        core.foreground_is(&mut st, id, shell(&["sh"]));
        assert_eq!(st.reg.terminals[&id].status.is_none(), crate::platform::foreground_tracks_jobs());
        drop(st);
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }
}
