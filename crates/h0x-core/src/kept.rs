//! Lost kept records stay live without a holder until a replacement shell starts.
use crate::holder_proto as proto;
use crate::input::Step;
use crate::runtime::{Core, now_ms};
use crate::terminals::{KEEP_RETRY_MS, Terminal};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

impl Terminal {
    pub(crate) fn schedule_restart(&mut self, now: u64) {
        let stable = self.program_started_ms.is_some_and(|at| now.saturating_sub(at) >= 10_000);
        self.restart_delay_ms = if stable {
            KEEP_RETRY_MS
        } else {
            match self.restart_delay_ms {
                0 => KEEP_RETRY_MS,
                KEEP_RETRY_MS => 3000,
                3000 => 6000,
                6000 => 12_000,
                12_000 => 30_000,
                _ => 60_000,
            }
        };
        self.restart_after_ms = now + self.restart_delay_ms;
        self.program_started_ms = None;
    }
}

impl Core {
    pub(crate) fn spawn_failed(&self, st: &mut crate::runtime::State, id: u32) {
        let t = st.reg.terminals.get_mut(&id).expect("failed spawn");
        t.holder = None;
        t.pid = None;
        if st.killing.contains(&id) || !t.keep || t.reason.as_deref().is_none_or(|r| r == "spawn_pending") {
            let reason = t.reason.clone().filter(|r| r != "spawn_pending").unwrap_or_else(|| "holder_lost".into());
            self.record_finish(st, id, proto::Exit { code: None, signal: None }, &reason);
        } else {
            t.schedule_restart(now_ms());
        }
        self.save(st);
        self.changed.notify_waiters();
    }

    pub(crate) async fn recover_kept(self: Arc<Self>, startup: bool) {
        let pending = self.lock().reg.terminals.values().any(|t| t.is_live() && t.keep && t.holder.is_none());
        if !pending || crate::platform::host_shutdown_in_progress() {
            return;
        }
        if self.sweeps.load(Ordering::SeqCst) != 0 || self.lock().stopping || self.restart.load(Ordering::SeqCst) {
            return;
        }
        if let Err(e) = self.ensure_holder().await {
            let _ = writeln!(io::stderr(), "core: kept terminals waiting for a holder: {e}");
            return;
        }
        let shell = match crate::config::load(&self.paths.config).and_then(|settings| settings.shell()) {
            Ok(shell) => shell,
            Err(e) => {
                let _ = writeln!(io::stderr(), "core: kept terminals waiting for a shell: {}", e.message);
                return;
            }
        };
        let base_env = crate::platform::program_env().await;
        let mut st = self.lock();
        if st.stopping
            || self.restart.load(Ordering::SeqCst)
            || self.sweeps.load(Ordering::SeqCst) != 0
            || crate::platform::host_shutdown_in_progress()
        {
            return;
        }
        let ids: Vec<u32> = st
            .reg
            .terminals
            .values()
            .filter(|t| t.is_live() && t.keep && t.holder.is_none() && (startup || t.restart_after_ms <= now_ms()))
            .map(|t| t.id())
            .collect();
        let shell_path = Path::new(shell.first().map_or("", String::as_str));
        let (mut claimed, mut typed) = (HashSet::new(), Vec::new());
        for id in ids {
            let Some(holder) = st.current() else { break };
            let instance = holder.instance.clone();
            let resume = resume_line(&st.reg, id, shell_path, &claimed);
            let seen = st.output_seq.get(&id).copied().unwrap_or(0);
            let t = st.reg.terminals.get_mut(&id).expect("pending");
            // The agent's run ends with its program: a crash loop never resumes twice.
            t.agent = None;
            let env = crate::api::child_env(base_env.clone(), t.restart_env.clone());
            let cwd = t
                .last_cwd
                .as_ref()
                .filter(|p| std::path::Path::new(p).is_dir())
                .or_else(|| std::path::Path::new(&t.cwd).is_dir().then_some(&t.cwd))
                .cloned()
                .unwrap_or_else(|| {
                    env.get("HOME").or_else(|| env.get("USERPROFILE")).cloned().unwrap_or_else(|| "/".into())
                });
            let (env, scope) = crate::api::terminal_env(&self, id, &cwd, env);
            t.scope = Some(scope);
            t.holder = Some(instance);
            t.argv = shell.clone();
            t.cwd = cwd.clone();
            t.template = None;
            t.reported = false;
            t.title.clear();
            t.full_screen = false;
            t.win32_input = false;
            t.printed = 0;
            t.screen = "exact".into();
            t.submit = None;
            t.stop = None;
            t.paste_gap_ms = None;
            let spawn = proto::Spawn { argv: t.argv.clone(), env, cwd, cols: t.cols, rows: t.rows };
            let mut screen = ghostty_vt::Terminal::new(t.cols, t.rows, self.history_bytes());
            screen.set_xtversion(&t.identity);
            st.screens.insert(id, screen);
            // Claim and queue the snapshot before sending: a slow holder never gets a second spawn.
            // If a crash leaves the spawn unrecorded, the next start kills it.
            self.save(&st);
            if let Some(link) = st.link(id) {
                link.send(&proto::json_frame(proto::SPAWN, id, &spawn));
            }
            if let Some((session, line)) = resume {
                claimed.insert(session);
                typed.push((id, seen, line));
            }
        }
        drop(st);
        for (id, seen, line) in typed {
            tokio::spawn(self.clone().type_resume(id, seen, line));
        }
    }

    /// Types `line` into the replacement shell of terminal `id` once its prompt has
    /// settled (it printed since `seen` and went quiet); after 10 s, anyway: the
    /// terminal holds the input for the shell.
    async fn type_resume(self: Arc<Self>, id: u32, seen: u64, line: String) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let ready = {
                let st = self.lock();
                if !st.reg.terminals.get(&id).is_some_and(|t| t.is_live()) {
                    return;
                }
                st.output_seq.get(&id).copied().unwrap_or(0) > seen
                    && st.last_output.get(&id).is_some_and(|at| at.elapsed() >= crate::input::OUTPUT_SETTLE)
            };
            if ready || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let bytes = format!("{line}\r").into_bytes();
        let typed = self.type_in(id, Duration::ZERO, false, |_, _| Ok(((), vec![Step::Write(bytes)]))).await;
        if let Err(e) = typed {
            let _ = writeln!(io::stderr(), "core: resume line not typed: {}", e.message);
        }
    }
}

/// The session a lost terminal's agent resumes and the line that types it into
/// `shell`. None if resume is off (`h0x.resume=off`), no agent was running, no
/// session can be told, or the shell cannot take a line. Never a session in `claimed`.
fn resume_line(
    reg: &crate::terminals::Registry,
    id: u32,
    shell: &Path,
    claimed: &HashSet<String>,
) -> Option<(String, String)> {
    let t = reg.terminals.get(&id)?;
    if t.labels.get("h0x.resume").is_some_and(|v| v == "off") {
        return None;
    }
    let run = t.agent.as_ref()?;
    let labelled = |o: &Terminal| {
        let (agent, session) = o.labels.get("h0x.session")?.split_once(':')?;
        (agent == run.agent).then(|| session.to_string())
    };
    let session = run
        .id
        .clone()
        .or_else(|| labelled(t))
        .or_else(|| {
            let held = |s: &str| {
                reg.terminals.values().filter(|o| o.id() != id).any(|o| {
                    o.agent.as_ref().is_some_and(|a| a.agent == run.agent && a.id.as_deref() == Some(s))
                        || labelled(o).as_deref() == Some(s)
                })
            };
            let cwd = t.last_cwd.as_deref().unwrap_or(&t.cwd);
            crate::sessions::newest_since(&run.agent, cwd, run.started_ms, &held)
        })
        .filter(|s| !claimed.contains(s))?;
    Some((session.clone(), crate::sessions::resume_line(run, &session, shell)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::{NewTerminal, Registry};

    #[test]
    fn retries_back_off_forever_and_reset_only_after_a_stable_program() {
        let mut reg = Registry::default();
        let id = reg.create(NewTerminal { argv: vec!["sh".into()], keep: true, ..Default::default() }, 0).unwrap();
        let t = reg.terminals.get_mut(&id).unwrap();
        for delay in [1500, 3000, 6000, 12_000, 30_000, 60_000, 60_000] {
            t.schedule_restart(100_000);
            assert_eq!(t.restart_delay_ms, delay);
            assert_eq!(t.restart_after_ms, 100_000 + delay);
        }
        t.program_started_ms = Some(90_001);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 60_000, "a short-lived program does not reset retries");
        t.program_started_ms = Some(90_000);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 1500);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 3000);
        assert!(t.is_live());
    }

    const UUID: &str = "123e4567-e89b-42d3-a456-426614174000";
    const OTHER: &str = "223e4567-e89b-42d3-a456-426614174000";

    fn lost(reg: &mut Registry, agent: &str, id: Option<&str>) -> u32 {
        let t = reg.create(NewTerminal { argv: vec!["sh".into()], keep: true, ..Default::default() }, 0).unwrap();
        reg.terminals.get_mut(&t).unwrap().agent = Some(crate::terminals::AgentRun {
            agent: agent.into(),
            id: id.map(Into::into),
            flags: vec![],
            started_ms: 5,
        });
        t
    }

    fn line(reg: &Registry, id: u32, claimed: &[&str]) -> Option<(String, String)> {
        let claimed = claimed.iter().map(|s| s.to_string()).collect();
        resume_line(reg, id, Path::new("/bin/sh"), &claimed)
    }

    #[test]
    fn auto_resume_types_the_table_resume_words_for_the_recorded_id() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        reg.terminals.get_mut(&id).unwrap().argv = vec!["secret-program".into(), "--token=x".into()];
        let (session, line) = line(&reg, id, &[]).unwrap();
        assert_eq!(session, UUID);
        assert_eq!(line, format!("'codex' 'resume' '{UUID}'"));
    }

    #[test]
    fn auto_resume_is_off_when_the_label_says_off_and_on_otherwise() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        let t = reg.terminals.get_mut(&id).unwrap();
        t.labels.insert("h0x.resume".into(), "off".into());
        assert!(line(&reg, id, &[]).is_none());
        let t = reg.terminals.get_mut(&id).unwrap();
        t.labels.insert("h0x.resume".into(), "on".into());
        assert!(line(&reg, id, &[]).is_some());
    }

    #[test]
    fn auto_resume_needs_a_running_agent_and_a_session() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        reg.terminals.get_mut(&id).unwrap().agent = None;
        assert!(line(&reg, id, &[]).is_none(), "the agent exited normally");
        // No id, no label and no store to find one in: a plain shell.
        let id = lost(&mut reg, "codex", None);
        assert!(line(&reg, id, &[]).is_none());
        let id = lost(&mut reg, "amp", Some(UUID));
        assert!(line(&reg, id, &[]).is_none(), "no resume row");
    }

    #[test]
    fn auto_resume_falls_back_to_the_claude_session_label_of_its_own_terminal() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "claude", None);
        reg.terminals.get_mut(&id).unwrap().labels.insert("h0x.session".into(), format!("claude:{OTHER}"));
        assert_eq!(line(&reg, id, &[]).unwrap().1, format!("'claude' '--resume' '{OTHER}'"));
        reg.terminals.get_mut(&id).unwrap().labels.insert("h0x.session".into(), "claude:--evil".into());
        assert!(line(&reg, id, &[]).is_none(), "an id of the wrong shape is never typed");
    }

    #[test]
    fn auto_resume_resumes_a_session_once() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        assert!(line(&reg, id, &[UUID]).is_none());
        assert!(line(&reg, id, &[OTHER]).is_some());
    }

    #[test]
    fn auto_resume_refuses_a_shell_it_cannot_quote_for() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        assert!(resume_line(&reg, id, Path::new(r"C:\Windows\System32\cmd.exe"), &HashSet::new()).is_none());
    }

    /// A lost terminal in a folder no agent store can hold, so only its own id or label can resume it.
    fn nowhere(reg: &mut Registry, agent: &str, id: Option<&str>) -> u32 {
        let t = lost(reg, agent, id);
        let dir = std::env::temp_dir().join(format!("h0x-core-no-sessions-{}", crate::platform::random_hex()));
        reg.terminals.get_mut(&t).unwrap().last_cwd = Some(dir.display().to_string());
        t
    }

    #[test]
    fn auto_resume_edge_only_the_exact_label_off_turns_it_off() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        for on in ["OFF", "Off", "", "on", "off ", " off", "no", "false", "0"] {
            reg.terminals.get_mut(&id).unwrap().labels.insert("h0x.resume".into(), on.into());
            assert!(line(&reg, id, &[]).is_some(), "{on:?}");
        }
        reg.terminals.get_mut(&id).unwrap().labels.insert("h0x.resume".into(), "off".into());
        assert!(line(&reg, id, &[]).is_none());
        reg.terminals.get_mut(&id).unwrap().labels.remove("h0x.resume");
        assert!(line(&reg, id, &[]).is_some(), "no label: on");
    }

    #[test]
    fn auto_resume_edge_a_session_label_counts_only_for_its_own_agent() {
        let mut reg = Registry::default();
        let id = nowhere(&mut reg, "claude", None);
        let label = |reg: &mut Registry, v: &str| {
            reg.terminals.get_mut(&id).unwrap().labels.insert("h0x.session".into(), v.into());
        };
        for ignored in [
            format!("codex:{UUID}"),
            format!("Claude:{UUID}"),
            format!("claude :{UUID}"),
            format!(":{UUID}"),
            UUID.to_string(),
            "claude:".to_string(),
            "claude".to_string(),
            String::new(),
            format!("claude:{UUID} "),
            format!("claude:{UUID}\n"),
        ] {
            label(&mut reg, &ignored);
            assert!(line(&reg, id, &[]).is_none(), "{ignored:?}");
        }
        label(&mut reg, &format!("claude:{UUID}"));
        assert_eq!(line(&reg, id, &[]).unwrap().0, UUID);
        // The recorded id wins over the label.
        reg.terminals.get_mut(&id).unwrap().agent.as_mut().unwrap().id = Some(OTHER.into());
        assert_eq!(line(&reg, id, &[]).unwrap().0, OTHER);
        // Another terminal's label is never this terminal's session.
        let mut reg = Registry::default();
        let mine = nowhere(&mut reg, "claude", None);
        let other = nowhere(&mut reg, "claude", None);
        reg.terminals.get_mut(&other).unwrap().labels.insert("h0x.session".into(), format!("claude:{UUID}"));
        assert!(line(&reg, mine, &[]).is_none());
    }

    #[test]
    fn auto_resume_edge_two_terminals_with_one_session_resume_it_once() {
        let mut reg = Registry::default();
        let a = nowhere(&mut reg, "codex", Some(UUID));
        let b = nowhere(&mut reg, "claude", None);
        reg.terminals.get_mut(&b).unwrap().labels.insert("h0x.session".into(), format!("claude:{UUID}"));
        let c = nowhere(&mut reg, "codex", Some(UUID));
        // As one recovery pass does: each resumed session is claimed before the next terminal.
        let mut claimed = HashSet::new();
        let mut resumed = Vec::new();
        for id in [a, b, c] {
            if let Some((session, _)) = resume_line(&reg, id, Path::new("/bin/bash"), &claimed) {
                assert!(claimed.insert(session.clone()), "{session} twice");
                resumed.push(id);
            }
        }
        assert_eq!(resumed, vec![a], "the same id, whichever agent or source named it, resumes once");
    }

    #[test]
    fn auto_resume_edge_never_types_the_programs_argv_prompt_or_env() {
        let evil = ["$(touch /tmp/pwned)", "`id`", "a'b\"c", "line\nnext", "; rm -rf ~", "--settings=/x.json"];
        let mut argv = vec!["codex".to_string(), "resume".into(), UUID.into()];
        argv.extend(evil.iter().map(|s| s.to_string()));
        let run = crate::sessions::detect(&argv, 9).unwrap();
        let mut reg = Registry::default();
        let id = nowhere(&mut reg, "codex", None);
        let t = reg.terminals.get_mut(&id).unwrap();
        t.agent = Some(run);
        t.argv = argv.clone();
        t.restart_env.insert("SECRET_TOKEN".into(), "hunter2".into());
        for (shell, want) in [
            ("/bin/sh", format!("'codex' 'resume' '{UUID}'")),
            ("/usr/bin/zsh", format!("'codex' 'resume' '{UUID}'")),
            ("/usr/local/bin/fish", format!("'codex' 'resume' '{UUID}'")),
            (
                r"C:\Program Files\PowerShell\7\pwsh.exe",
                format!("& {{ $PSNativeCommandArgumentPassing = 'Legacy'; & 'codex' 'resume' '{UUID}' }}"),
            ),
        ] {
            let (session, typed) = resume_line(&reg, id, Path::new(shell), &HashSet::new()).unwrap();
            assert_eq!(session, UUID);
            assert_eq!(typed, want, "{shell}");
            for word in evil.iter().chain(&["hunter2", "SECRET_TOKEN", "pwned", "settings"]) {
                assert!(!typed.contains(word), "{shell}: {word:?} in {typed:?}");
            }
        }
    }

    #[test]
    fn auto_resume_edge_shells_that_cannot_take_a_line_get_none() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        for shell in ["", "/", "cmd.exe", "CMD.EXE", r"C:\WINDOWS\system32\CMD.EXE", "/usr/bin/nu", "/bin/csh", "xonsh"]
        {
            assert!(resume_line(&reg, id, Path::new(shell), &HashSet::new()).is_none(), "{shell:?}");
        }
        for shell in ["/bin/BASH", "dash", r"C:\Program Files\Git\bin\bash.exe", "powershell.exe", "/opt/fish"] {
            assert!(resume_line(&reg, id, Path::new(shell), &HashSet::new()).is_some(), "{shell:?}");
        }
    }

    #[test]
    fn auto_resume_edge_kept_flags_go_before_the_resume_words_and_bad_ones_refuse() {
        let mut reg = Registry::default();
        let id = lost(&mut reg, "codex", Some(UUID));
        let flags = |reg: &mut Registry, f: &[&str]| {
            reg.terminals.get_mut(&id).unwrap().agent.as_mut().unwrap().flags =
                f.iter().map(|s| s.to_string()).collect();
        };
        flags(&mut reg, &["--model", "o'3 $(x)"]);
        assert_eq!(line(&reg, id, &[]).unwrap().1, format!(r"'codex' '--model' 'o'\''3 $(x)' 'resume' '{UUID}'"));
        // A control character would end the line early: nothing is typed.
        flags(&mut reg, &["--model", "a\rb"]);
        assert!(line(&reg, id, &[]).is_none());
        flags(&mut reg, &["--model", "a\nb"]);
        assert!(line(&reg, id, &[]).is_none());
    }

    #[test]
    fn auto_resume_edge_a_malformed_or_unknown_recorded_run_is_no_line() {
        let mut reg = Registry::default();
        for (agent, id) in [
            ("codex", "123e4567-e89b-42d3-a456-42661417400"),
            ("codex", "--last"),
            ("codex", ""),
            ("devin", "-x"),
            ("devin", "a b"),
            ("opencode", "abc"),
        ] {
            let t = nowhere(&mut reg, agent, Some(id));
            assert!(line(&reg, t, &[]).is_none(), "{agent} {id:?}");
        }
        for agent in ["", "unknown", "cursor-agent", "Codex"] {
            let t = nowhere(&mut reg, agent, Some(UUID));
            assert!(line(&reg, t, &[]).is_none(), "{agent:?}");
        }
        assert!(line(&reg, 9999, &[]).is_none(), "no such terminal");
    }
}
