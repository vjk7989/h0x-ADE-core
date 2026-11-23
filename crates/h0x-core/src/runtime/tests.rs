use super::*;
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn keys_pressed_during_a_paste_follow_its_submit() {
    use crate::viewers::{AttachRequest, Display};
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let (core, root) = test_core();
    let (writer, frames) = mpsc::channel();
    let (bye, _) = mpsc::channel();
    let id = {
        let mut st = core.lock();
        let id =
            st.reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap();
        st.reg.terminals.get_mut(&id).unwrap().holder = Some("fake".into());
        st.screens.insert(id, Screen::new(80, 24, 1 << 20));
        st.holders.push(Link { writer, pid: 0, instance: "fake".into(), current: true, bye, bye_rx: None });
        id
    };
    let person = Arc::new(crate::wire::Conn::default());
    let channel = core
        .attach(
            &person,
            id,
            AttachRequest {
                observe: false,
                resize_notifications: false,
                history: 0,
                size: (80, 24),
                display: Display::default(),
                resumed: false,
            },
        )
        .unwrap();
    // Ignore attach's resize frames. From here the fake holder sees only input.
    while frames.try_recv().is_ok() {}
    {
        let sending = core.send(id, "abc", false, None);
        tokio::pin!(sending);
        poll_fn(|cx| {
            assert!(sending.as_mut().poll(cx).is_pending(), "the paste must pause before submit");
            Poll::Ready(())
        })
        .await;
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"abc"));
        assert!(frames.try_recv().is_err());
        core.viewer_input(&person, channel, b"Z", false);
        assert!(frames.try_recv().is_err(), "the person's keys must stay held");
        tokio::time::advance(crate::input::PASTE_GAP).await;
        sending.await.unwrap();
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"\r"));
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"Z"));
    }
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn server_exit_flushes_even_when_background_tasks_still_own_core() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let (release, blocked) = mpsc::channel();
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(())
    })
    .unwrap();
    let background_owner = core.clone();
    core.save(&core.lock());
    let finish = crate::server::finish(&core);
    tokio::pin!(finish);
    let waited = tokio::time::timeout(Duration::from_millis(10), &mut finish).await.is_err();
    release.send(()).unwrap();
    if waited {
        finish.await.unwrap();
    }
    drop(background_owner);
    remove_root(&root);
    assert!(waited, "serve returned before its queued snapshot finished");
}

#[tokio::test]
async fn server_stop_succeeds_even_when_the_registry_flush_times_out() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let (release, blocked) = mpsc::channel();
    let mut first = true;
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        if first {
            first = false;
            blocked.recv_timeout(Duration::from_secs(15)).unwrap();
        }
        Ok(())
    })
    .unwrap();
    let call = crate::api::Call { core: &core, conn: None, plain: None };
    let started = Instant::now();
    let stop = crate::api::dispatch(&call, "server.stop", json!({})).await;
    release.send(()).unwrap();
    core.flush_saved().await.unwrap();
    drop(core);
    remove_root(&root);
    assert!(started.elapsed() >= Duration::from_secs(10), "stuck disks must still time out");
    assert_eq!(stop.unwrap()["stopped"], true);
}

#[tokio::test]
async fn a_wait_leaves_nothing_registered_however_it_ends() {
    let (core, root) = test_core();
    let spec = || crate::wait::WaitSpec { output: Some("never".into()), timeout: Duration::ZERO, after_turn: None };
    let new = || crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() };
    let (done, quiet) = {
        let mut st = core.lock();
        let done = st.reg.create(new(), now_ms()).unwrap();
        st.reg.finish(done, Some(0), None, "exited", now_ms());
        (done, st.reg.create(new(), now_ms()).unwrap())
    };
    // Neither sends anything afterwards that would clear a stale registration.
    assert_eq!(core.wait(done, spec(), None).await.reason, "exit");
    assert_eq!(core.wait(quiet, spec(), None).await.reason, "timeout");
    let left = {
        let st = core.lock();
        (st.events.subs.len(), st.output_watch.values().map(Vec::len).sum::<usize>())
    };
    drop(core);
    remove_root(&root);
    assert_eq!(left, (0, 0), "(event subscriptions, output watchers) left behind");
}

/// (event subscriptions, output watchers) the core holds right now.
fn waits_registered(core: &Core) -> (usize, usize) {
    let st = core.lock();
    (st.events.subs.len(), st.output_watch.values().map(Vec::len).sum::<usize>())
}

#[tokio::test]
async fn a_cancelled_wait_leaves_the_others_and_a_matched_one_leaves_nothing() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    let spec = |needle: &str| crate::wait::WaitSpec {
        output: Some(needle.into()),
        timeout: Duration::from_secs(60),
        after_turn: None,
    };
    // A client that goes away mid-wait drops the future.
    let mut cancelled = Box::pin(core.wait(id, spec("never"), None));
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut cancelled).await.is_err());
    assert_eq!(waits_registered(&core), (1, 1), "the wait registered before it was cut");
    let mut matching = Box::pin(core.wait(id, spec("준비"), None));
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut matching).await.is_err());
    assert_eq!(waits_registered(&core), (2, 2));
    drop(cancelled);
    let after_cancel = waits_registered(&core);
    // The needle arrives cut inside its first character, as a pipe may deliver it.
    let watcher = core.lock().output_watch[&id][0].clone();
    let word = "준비".as_bytes();
    watcher.send(word[..2].to_vec()).unwrap();
    watcher.send(word[2..].to_vec()).unwrap();
    let matched = tokio::time::timeout(Duration::from_secs(5), matching).await;
    drop(watcher);
    let left = waits_registered(&core);
    drop(core);
    remove_root(&root);
    assert_eq!(after_cancel, (1, 1), "a cancelled wait takes only its own registrations");
    assert_eq!(matched.expect("the cut needle matched").reason, "output");
    assert_eq!(left, (0, 0), "(event subscriptions, output watchers) left behind");
}

#[tokio::test]
async fn waits_that_end_at_once_leave_nothing_registered() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    core.lock().reg.terminals.get_mut(&id).unwrap().turn = 1;
    let spec = |output: Option<&str>, after_turn| crate::wait::WaitSpec {
        output: output.map(String::from),
        timeout: Duration::ZERO,
        after_turn,
    };
    let mut seen = Vec::new();
    // No such terminal: the error path.
    seen.push((core.wait(9999, spec(Some("x"), None), None).await.reason, waits_registered(&core)));
    // Its turn already ended.
    seen.push((core.wait(id, spec(Some("x"), Some(0)), None).await.reason, waits_registered(&core)));
    // A plain wait (no output watch) that times out at once.
    seen.push((core.wait(id, spec(None, None), None).await.reason, waits_registered(&core)));
    // An empty needle: any output matches.
    seen.push((core.wait(id, spec(Some(""), None), None).await.reason, waits_registered(&core)));
    drop(core);
    remove_root(&root);
    let reasons: Vec<_> = seen.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(reasons, ["exit", "status", "timeout", "timeout"]);
    assert!(seen.iter().all(|(_, left)| *left == (0, 0)), "{seen:?}");
}

#[tokio::test]
async fn a_working_program_that_exits_mid_wait_ends_it_with_exit_and_leaves_nothing() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    let idle = new_terminal(&core);
    {
        let mut st = core.lock();
        core.report_status(&mut st, id, &[root_report("working")]);
        core.report_status(&mut st, idle, &[root_report("working")]);
    }
    let plain = || crate::wait::WaitSpec { output: None, timeout: Duration::from_secs(60), after_turn: None };
    let mut exiting = Box::pin(core.wait(id, plain(), None));
    let mut stopping = Box::pin(core.wait(idle, plain(), None));
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut exiting).await.is_err(), "still working");
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut stopping).await.is_err(), "still working");
    assert_eq!(waits_registered(&core), (2, 0));
    {
        let mut st = core.lock();
        // A finish clears the status first, then says how the program ended.
        core.record_finish(&mut st, id, Exit { code: Some(4), signal: None }, "exited");
        core.report_status(&mut st, idle, &[root_report("idle")]);
    }
    let exited = tokio::time::timeout(Duration::from_secs(5), exiting).await;
    let stopped = tokio::time::timeout(Duration::from_secs(5), stopping).await;
    let left = waits_registered(&core);
    drop(core);
    remove_root(&root);
    let exited = exited.expect("the exit ended the wait");
    assert_eq!((exited.reason.as_str(), exited.exit_code), ("exit", Some(4)));
    let stopped = stopped.expect("idle ended the wait");
    assert_eq!(
        (stopped.reason.as_str(), stopped.status.as_deref()),
        ("status", Some("idle")),
        "a live stop is unchanged"
    );
    assert_eq!(left, (0, 0), "(event subscriptions, output watchers) left behind");
}

#[tokio::test]
async fn unsubscribing_a_waiter_twice_or_one_never_subscribed_is_harmless() {
    use crate::events::{Filter, Sink};
    let (core, root) = test_core();
    let (gone, _gone_rx) = tokio::sync::mpsc::unbounded_channel();
    let (kept, _kept_rx) = tokio::sync::mpsc::unbounded_channel();
    let (never, _never_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let conn = Arc::new(crate::wire::Conn::default());
    let counts = {
        let mut st = core.lock();
        // Nothing subscribed at all.
        core.unsubscribe_waiter(&mut st, &never);
        let empty = st.events.subs.len();
        core.subscribe(&mut st, Filter::default(), Sink::Waiter(gone.clone()));
        core.subscribe(&mut st, Filter::default(), Sink::Waiter(kept));
        core.subscribe(&mut st, Filter::default(), Sink::Framed { conn: conn.clone(), channel: 1 });
        core.unsubscribe_waiter(&mut st, &gone);
        let once = st.events.subs.len();
        core.unsubscribe_waiter(&mut st, &gone);
        core.unsubscribe_waiter(&mut st, &never);
        (empty, once, st.events.subs.len(), core.subscribed(&st, &conn, 1))
    };
    drop(core);
    remove_root(&root);
    assert_eq!(counts, (0, 2, 2, true), "(empty, after one, after repeats, framed kept)");
}

fn test_core() -> (Arc<Core>, PathBuf) {
    let root = std::env::temp_dir().join(format!("h0x-core-persistence-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    (core, root)
}

#[tokio::test]
async fn label_and_kill_reply_with_the_record_observed_before_flushing() {
    for method in ["terminal.kill", "terminal.label"] {
        let (mut core, root) = test_core();
        core.flush_saved().await.unwrap();
        let (release, blocked) = mpsc::channel();
        Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
        let id = {
            let mut st = core.lock();
            let id = st
                .reg
                .create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, now_ms())
                .unwrap();
            st.reg.finish(id, Some(0), None, "exited", now_ms());
            core.save(&st);
            id
        };
        let reply = {
            let call = crate::api::Call { core: &core, conn: None, plain: None };
            let request =
                crate::api::dispatch(&call, method, json!({"terminal": crate::terminals::address(id), "labels": {}}));
            tokio::pin!(request);
            assert!(tokio::time::timeout(Duration::from_millis(10), &mut request).await.is_err());
            core.lock().reg.terminals.get_mut(&id).unwrap().turn = 1;
            // Label queued a second snapshot; kill reused the existing one.
            release.send(()).unwrap();
            let _ = release.send(());
            request.await.unwrap()
        };
        let captured = reply["turn"].clone();
        drop(core);
        remove_root(&root);
        assert_eq!(captured, 0, "{method} returned a turn observed after its barrier: {reply}");
    }
}

#[cfg(unix)]
#[test]
fn a_blocked_registry_write_does_not_hold_the_core_lock() {
    use std::os::unix::ffi::OsStrExt;
    let root = PathBuf::from(format!("/var/tmp/h0x-core-save-lock-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !core.paths.state.join("terminals.json").exists() {
        assert!(Instant::now() < deadline, "startup registry was not written");
        std::thread::sleep(Duration::from_millis(1));
    }
    let tmp = core.paths.state.join("terminals.tmp");
    let name = std::ffi::CString::new(tmp.as_os_str().as_bytes()).unwrap();
    // Opening this temporary file for writing waits for a reader, like a stalled disk.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (saved, done) = mpsc::channel();
    let saving = core.clone();
    let worker = std::thread::spawn(move || {
        saving.save(&saving.lock());
        saved.send(()).unwrap();
    });
    let returned = done.recv_timeout(Duration::from_millis(250)).is_ok();
    let unlocked = core.state.try_lock().is_ok();
    // Always release the blocked write, even when checking the old synchronous implementation.
    let reader = std::fs::File::open(&tmp).unwrap();
    worker.join().unwrap();
    drop(core);
    drop(reader);
    remove_root(&root);
    assert!(returned && unlocked, "saving to a stalled disk must leave output and input free to take the core lock");
}

#[tokio::test]
async fn a_prepared_restart_survives_sigterm_and_rejects_stop() {
    let root = std::env::temp_dir().join(format!("h0x-core-restart-stop-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let call = crate::api::Call { core: &core, conn: None, plain: None };
    crate::api::dispatch(&call, "server.restart", serde_json::json!({})).await.unwrap();
    crate::restart::on_sigterm(&core);
    assert!(core.paths.state.join("restart-env.json").exists(), "SIGTERM keeps the explicit restart's note");
    let error = crate::api::dispatch(&call, "server.stop", serde_json::json!({})).await.unwrap_err();
    assert_eq!(error.kind, "unavailable");
    assert!(!core.lock().stopping, "a prepared handoff keeps its holders");
    crate::restart::discard(&core.paths);
    core.restart.store(false, std::sync::atomic::Ordering::SeqCst);
    crate::api::dispatch(&call, "server.stop", serde_json::json!({})).await.unwrap();
    assert!(crate::restart::prepare(&core).is_err(), "a stop already in progress wins");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_second_restart_request_is_refused_and_keeps_the_first_note() {
    let (core, root) = test_core();
    let note = core.paths.state.join("restart-env.json");
    crate::restart::prepare(&core).unwrap();
    let first = std::fs::read(&note).unwrap();
    assert!(crate::restart::prepare(&core).is_err());
    assert_eq!(std::fs::read(&note).unwrap(), first, "the refused request neither rewrote nor removed it");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn sigterm_without_a_requested_restart_discards_a_stale_note() {
    let (core, root) = test_core();
    let note = core.paths.state.join("restart-env.json");
    let tmp = core.paths.state.join("restart-env.abandoned.tmp");
    std::fs::write(&note, b"{}").unwrap();
    std::fs::write(&tmp, b"{}").unwrap();
    crate::restart::on_sigterm(&core);
    assert!(!note.exists(), "SIGTERM is not a restart: nothing is handed on");
    assert!(!tmp.exists());
    assert!(crate::restart::prepare(&core).is_err(), "a restart cannot start after SIGTERM");
    assert!(!note.exists());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn an_unsavable_note_still_restarts_and_leaves_no_temporary_behind() {
    let (core, root) = test_core();
    let note = core.paths.state.join("restart-env.json");
    std::fs::create_dir(&note).unwrap();
    crate::restart::prepare(&core).unwrap();
    assert!(core.restart.load(std::sync::atomic::Ordering::SeqCst), "the restart goes ahead");
    let left: Vec<_> = std::fs::read_dir(&core.paths.state)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    std::fs::remove_dir(&note).unwrap();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn host_shutdown_is_lost_without_overriding_explicit_actions() {
    for (killed, stopping, shutdown, expected) in [
        (false, false, false, "exited"),
        (false, false, true, "host_shutdown"),
        (false, true, false, "server_stop"),
        (false, true, true, "server_stop"),
        (true, false, false, "killed"),
        (true, false, true, "killed"),
        (true, true, false, "killed"),
        (true, true, true, "killed"),
    ] {
        assert_eq!(finish_reason(killed, stopping, shutdown), expected);
    }
}

#[cfg(unix)]
#[test]
fn a_new_holder_of_another_build_reports_both_identities() {
    let root = PathBuf::from(format!("/var/tmp/h0x-core-holder-build-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let (stream, mut holder) = UnixStream::pair().unwrap();
    let fake = std::thread::spawn(move || {
        assert_eq!(proto::read(&mut holder).unwrap().unwrap().0, proto::HELLO);
        let welcome = proto::Welcome {
            version: proto::VERSION,
            instance: "other".into(),
            pid: 0,
            build: "other-build".into(),
            previous: "none".into(),
            terminals: vec![],
        };
        io::Write::write_all(&mut holder, &proto::json_frame(proto::WELCOME, 0, &welcome)).unwrap();
        assert_eq!(proto::read(&mut holder).unwrap().unwrap().0, proto::DRAIN);
    });
    let err = core.take_over(stream, true, &root.join("run/core.holder.x.sock"), None).unwrap_err().to_string();
    assert!(err.contains("other-build"), "{err}");
    assert!(err.contains(&crate::client::build_id()), "{err}");
    assert!(core.lock().holders.is_empty());
    fake.join().unwrap();
    drop(core);
    remove_root(&root);
}

/// A frame no core can read: the reader of a link that gets it fails.
#[cfg(unix)]
const GARBAGE: [u8; 9] = [proto::OUTPUT, 1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];

/// A core with one live terminal on a fake holder. Each connection's hello is
/// answered as told ("welcome", "garbage", or both), and the fake reports
/// what it saw: connection number, then "hello", "no hello" or "closed".
#[cfg(unix)]
fn on_fake_holder(root: &std::path::Path, answers: &[&str]) -> (Arc<Core>, u32, mpsc::Receiver<String>) {
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let id = {
        let mut st = core.lock();
        let new = crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() };
        let id = st.reg.create(new, now_ms()).unwrap();
        st.reg.terminals.get_mut(&id).unwrap().holder = Some("fake".into());
        id
    };
    let held = proto::HeldTerminal { id, pid: 0, cols: 80, rows: 24, exit: None };
    let welcome = proto::Welcome {
        version: proto::VERSION,
        instance: "fake".into(),
        pid: 0,
        build: crate::client::build_id(),
        previous: "crash".into(),
        terminals: vec![held],
    };
    let mut welcome = proto::json_frame(proto::WELCOME, 0, &welcome);
    welcome.extend(proto::frame(proto::TAIL, id, b""));
    let socket = root.join("run/core.holder.fake.sock");
    let listener = platform::listen(&socket).unwrap();
    let answers: Vec<Vec<u8>> = answers
        .iter()
        .map(|a| {
            let mut frames = if a.contains("welcome") { welcome.clone() } else { vec![] };
            if a.contains("garbage") {
                frames.extend(GARBAGE);
            }
            frames
        })
        .collect();
    let (heard, reports) = mpsc::channel();
    std::thread::spawn(move || {
        for (n, (stream, answer)) in listener.incoming().zip(answers).enumerate() {
            let (mut stream, heard) = (stream.unwrap(), heard.clone());
            std::thread::spawn(move || {
                let Ok(Some(_)) = proto::read(&mut stream) else {
                    return heard.send(format!("{n} no hello")).unwrap();
                };
                heard.send(format!("{n} hello")).unwrap();
                std::thread::sleep(Duration::from_millis(50)); // time for the test to act, well within RELINK_WAIT
                let _ = io::Write::write_all(&mut stream, &answer);
                while let Ok(Some(_)) = proto::read(&mut stream) {} // the core's frames, until it closes
                let _ = heard.send(format!("{n} closed"));
            });
        }
    });
    core.take_over(platform::connect(&socket).unwrap(), false, &socket, None).unwrap();
    (core, id, reports)
}

#[cfg(unix)]
fn reported(reports: &mpsc::Receiver<String>, n: usize) -> Vec<String> {
    let mut seen: Vec<String> = (0..n).map_while(|_| reports.recv_timeout(Duration::from_secs(5)).ok()).collect();
    seen.sort();
    seen
}

/// Live and not ended by losing its holder: left for whoever adopts it next.
#[cfg(unix)]
fn still_live(core: &Core, id: u32) -> bool {
    core.lock().reg.terminals.get(&id).is_some_and(|t| t.is_live() && t.reason.is_none())
}

#[cfg(unix)]
#[test]
fn a_link_whose_reading_fails_reconnects_but_not_twice_in_a_row() {
    let root = PathBuf::from(format!("/var/tmp/h0x-core-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "welcome garbage", ""]);
    assert_eq!(
        reported(&reports, 5),
        ["0 closed", "0 hello", "1 closed", "1 hello", "2 no hello"],
        "each failed link closes; the first failure reconnects, the second soon after does not"
    );
    assert!(core.lock().holders.is_empty());
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_the_holder_never_answers_gives_up_and_frees_holder_start() {
    let root = PathBuf::from(format!("/var/tmp/h0x-core-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", ""]);
    assert_eq!(reported(&reports, 4), ["0 closed", "0 hello", "1 closed", "1 hello"]);
    let start = Instant::now();
    while core.holder_start.try_lock().is_err() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(core.holder_start.try_lock().is_ok(), "runs and stops can start holders again");
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_whose_handshake_is_done_is_not_cut_however_slow_the_rest() {
    let root = PathBuf::from(format!("/var/tmp/h0x-core-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "welcome"]);
    while reports.recv_timeout(Duration::from_secs(5)).unwrap() != "1 hello" {}
    // Adopting waits for this lock: slow, but after the welcome came in time.
    let st = core.lock();
    std::thread::sleep(RELINK_WAIT * 3);
    drop(st);
    let start = Instant::now();
    while core.lock().holders.is_empty() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(RELINK_WAIT * 2);
    assert_eq!(core.lock().holders.len(), 1, "the new link stays");
    assert!(reports.try_iter().all(|r| r != "1 closed"), "and is not cut");
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_that_fails_leaves_the_holders_terminals_live() {
    let root = PathBuf::from(format!("/var/tmp/h0x-core-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "garbage"]);
    assert_eq!(reported(&reports, 4), ["0 closed", "0 hello", "1 closed", "1 hello"]);
    assert!(core.lock().holders.is_empty());
    assert!(still_live(&core, id), "its holder still answers: nothing ends");
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn waiting_for_holder_start_does_not_block_the_core_executor() {
    let root = PathBuf::from(format!("/var/tmp/h0x-core-holder-start-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let (ready, entered) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let c = core.clone();
    let blocker = std::thread::spawn(move || {
        let _starting = c.holder_start.lock().unwrap();
        ready.send(()).unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(1));
        // Prevent any real holder from starting, including if the regression stalls the executor.
        c.lock().stopping = true;
    });
    entered.recv().unwrap();
    let c = core.clone();
    let start = tokio::spawn(async move { c.ensure_holder().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let answered_while_starting = !core.lock().stopping;
    let _ = release.send(());
    blocker.join().unwrap();
    assert!(start.await.unwrap().is_err());
    drop(core);
    remove_root(&root);
    assert!(answered_while_starting, "holder startup blocked the core executor");
}

/// Holder threads can keep the core, and so its registry writer, alive a
/// moment after the test drops it: a last snapshot may still land.
fn remove_root(root: impl AsRef<std::path::Path>) {
    let start = Instant::now();
    loop {
        match std::fs::remove_dir_all(root.as_ref()) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if start.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("removing {}: {e}", root.as_ref().display()),
        }
    }
}

#[tokio::test]
async fn a_flood_of_status_flips_costs_no_event_text_and_few_saves() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let writes = count_writes(&mut core);
    let report = |state: &'static str, id: &str, message: String| ghostty_vt::StatusReport {
        state,
        kind: None,
        progress: None,
        id: id.into(),
        app: String::new(),
        title: String::new(),
        message,
    };
    let id = {
        let mut st = core.lock();
        let id =
            st.reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap();
        let big = "m".repeat(2048);
        let fill: Vec<_> = (0..300).map(|i| report("idle", &format!("r{i}"), big.clone())).collect();
        core.report_status(&mut st, id, &fill);
        id
    };
    core.save_if_dirty();
    core.flush_saved().await.unwrap();
    let (saved, seq) = (writes.load(std::sync::atomic::Ordering::SeqCst), core.lock().events.seq);
    let flips: Vec<_> =
        (0..4000).map(|i| report(if i % 2 == 0 { "working" } else { "idle" }, "", String::new())).collect();
    // The burst window is one that cannot end (an instant ahead of now has no time elapsed): a slow
    // runner cannot cross into a new second while the flips go in.
    *core.word_saves.lock().unwrap() = (Instant::now() + Duration::from_secs(3600), 0);
    let started = Instant::now();
    {
        let mut st = core.lock();
        core.report_status(&mut st, id, &flips);
        let t = &st.reg.terminals[&id];
        assert_eq!(t.records.len(), 255, "the cap holds");
        assert_eq!(t.turn, 2000, "every working to idle still counts a turn");
        // The same events as before: a status per word, a turn per ended turn (and the
        // update of the records that was still waiting for its timer, told first).
        assert_eq!(st.events.seq - seq, 1 + 4000 + 2000);
    }
    assert!(started.elapsed() < Duration::from_secs(5), "4000 flips took {:?}", started.elapsed());
    let written = || writes.load(std::sync::atomic::Ordering::SeqCst);
    assert!(written() <= saved + SAVE_BURST as usize, "saves are paced, not one per flip: {}", written() - saved);
    core.save_if_dirty();
    core.flush_saved().await.unwrap();
    let after = written();
    core.save_if_dirty();
    core.flush_saved().await.unwrap();
    assert_eq!(written(), after, "clean state is not saved again");
    assert!(after > saved && !core.dirty.load(std::sync::atomic::Ordering::SeqCst), "the final state was written");
    drop(core);
    remove_root(&root);
}

fn root_report(state: &'static str) -> ghostty_vt::StatusReport {
    ghostty_vt::StatusReport {
        state,
        kind: None,
        progress: None,
        id: String::new(),
        app: String::new(),
        title: String::new(),
        message: String::new(),
    }
}

fn new_terminal(core: &Core) -> u32 {
    core.lock().reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap()
}

/// Swaps the registry writer for one that only counts writes (flush the old one first).
fn count_writes(core: &mut Arc<Core>) -> Arc<std::sync::atomic::AtomicUsize> {
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = writes.clone();
    Arc::get_mut(core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    writes
}

/// The terminal as the registry file on disk has it.
fn saved_terminal(root: &std::path::Path, id: u32) -> crate::terminals::Terminal {
    let bytes = std::fs::read(root.join("state").join("terminals.json")).unwrap();
    let mut reg: crate::terminals::Registry = serde_json::from_slice(&bytes).unwrap();
    reg.terminals.remove(&id).expect("saved")
}

#[tokio::test]
async fn new_words_are_saved_at_once_up_to_a_burst_a_second_and_then_paced() {
    use std::sync::atomic::Ordering::SeqCst;
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let writes = count_writes(&mut core);
    let id = new_terminal(&core);
    let seq = core.lock().events.seq;
    core.report_status(&mut core.lock(), id, &[]);
    core.flush_saved().await.unwrap();
    assert_eq!((core.lock().events.seq, writes.load(SeqCst)), (seq, 0), "an empty batch tells and saves nothing");

    // The burst window cannot end (an instant ahead of now has no time elapsed), so a slow runner
    // cannot cross into a new second. And each flip is flushed before the next: the registry writer
    // keeps only the newest of the saves queued behind a busy write, so without it the count of
    // writes would depend on how fast the writer thread wakes.
    *core.word_saves.lock().unwrap() = (Instant::now() + Duration::from_secs(3600), 0);
    for i in 0..10 {
        core.report_status(&mut core.lock(), id, &[root_report(if i % 2 == 0 { "working" } else { "idle" })]);
        core.flush_saved().await.unwrap();
    }
    assert_eq!(writes.load(SeqCst), SAVE_BURST as usize, "the first words of a second are saved at once");
    assert!(core.dirty.load(SeqCst) && core.save_urgent.load(SeqCst), "the rest is owed within SAVE_PACE");
    core.save_if_dirty();
    core.flush_saved().await.unwrap();
    assert_eq!(writes.load(SeqCst), SAVE_BURST as usize + 1);

    *core.word_saves.lock().unwrap() = (Instant::now() - Duration::from_secs(2), SAVE_BURST);
    core.report_status(&mut core.lock(), id, &[root_report("working")]);
    core.flush_saved().await.unwrap();
    assert_eq!(writes.load(SeqCst), SAVE_BURST as usize + 2, "a new second: saved at once again");
    assert!(!core.dirty.load(SeqCst));

    let progress = ghostty_vt::StatusReport { message: "half way".into(), ..root_report("working") };
    core.report_status(&mut core.lock(), id, &[progress]);
    core.flush_saved().await.unwrap();
    assert_eq!(writes.load(SeqCst), SAVE_BURST as usize + 2, "the same word with a new message waits for save_soon");
    assert!(core.dirty.load(SeqCst));
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn updates_to_the_same_word_are_told_once_a_window_and_in_order_with_word_changes() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    core.subscribe(&mut core.lock(), crate::events::Filter::default(), crate::events::Sink::Waiter(tx));
    let drain = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>| {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    };
    let said = |message: &str| ghostty_vt::StatusReport { message: message.into(), ..root_report("working") };
    core.report_status(&mut core.lock(), id, &[root_report("working")]);
    assert_eq!(drain(&mut rx).len(), 1, "a new word is told at once");
    // Inside a window that has just opened, updates wait.
    core.lock().reg.terminals.get_mut(&id).unwrap().status_sent = Some(Instant::now() + Duration::from_secs(3600));
    let updates: Vec<_> = (0..100).map(|i| said(&format!("step {i}"))).collect();
    core.report_status(&mut core.lock(), id, &updates);
    assert!(drain(&mut rx).is_empty(), "no event per report");
    assert!(core.lock().reg.terminals[&id].status_owed);
    // The next word change tells what was owed first, so the order holds.
    core.report_status(&mut core.lock(), id, &[said("last"), root_report("blocked")]);
    let events = drain(&mut rx);
    let words: Vec<_> = events
        .iter()
        .map(|e| {
            (e["type"].as_str().unwrap(), e["data"]["status"]["state"].as_str(), e["data"]["status"]["msg"].as_str())
        })
        .collect();
    assert_eq!(
        words,
        [("status", Some("working"), Some("last")), ("status", Some("blocked"), None), ("turn", None, None)]
    );
    assert_eq!(events[2]["data"]["state"], "blocked");
    assert!(!core.lock().reg.terminals[&id].status_owed);
    drop(core);
    remove_root(&root);
}

#[tokio::test(start_paused = true)]
async fn an_update_left_waiting_is_told_by_the_timer() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    tokio::spawn(core.clone().flush_statuses_when_due());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    core.subscribe(&mut core.lock(), crate::events::Filter::default(), crate::events::Sink::Waiter(tx));
    core.report_status(&mut core.lock(), id, &[root_report("working")]);
    assert!(rx.try_recv().is_ok());
    core.lock().reg.terminals.get_mut(&id).unwrap().status_sent = Some(Instant::now() + Duration::from_secs(3600));
    let said = ghostty_vt::StatusReport { message: "later".into(), ..root_report("working") };
    core.report_status(&mut core.lock(), id, &[said]);
    assert!(rx.try_recv().is_err(), "not at once");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let event = rx.try_recv().expect("told when the window is over");
    assert_eq!(event["data"]["status"]["msg"], "later");
    assert!(rx.try_recv().is_err(), "once");
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn a_graceful_stop_writes_what_save_soon_held_back() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    core.report_status(&mut core.lock(), id, &[root_report("working")]);
    let held = ghostty_vt::StatusReport { message: "held back".into(), ..root_report("working") };
    core.report_status(&mut core.lock(), id, &[held]);
    core.flush_saved().await.unwrap();
    assert_eq!(saved_terminal(&root, id).status.unwrap().msg, "", "a message alone waits for save_soon");
    crate::server::finish(&core).await.unwrap();
    assert_eq!(saved_terminal(&root, id).status.unwrap().msg, "held back", "a stop does not lose it");
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn a_new_word_is_on_disk_before_any_stop() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    core.report_status(&mut core.lock(), id, &[root_report("working"), root_report("blocked")]);
    // Killed here: no stop and no save_if_dirty, only what the writer already has.
    core.flush_saved().await.unwrap();
    let saved = saved_terminal(&root, id);
    assert_eq!((saved.status.unwrap().state.as_str(), saved.turn), ("blocked", 1));
    assert!(!core.dirty.load(std::sync::atomic::Ordering::SeqCst), "nothing left owed");
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn every_stuck_word_and_turn_end_in_a_chunk_is_told_in_order_as_it_was() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    core.subscribe(&mut core.lock(), crate::events::Filter::default(), crate::events::Sink::Waiter(tx));
    let mut told = |reports: &[ghostty_vt::StatusReport]| {
        core.report_status(&mut core.lock(), id, reports);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    };
    // Status events by word, turn events by turn number and the word it ended in.
    let kinds = |events: &[serde_json::Value]| -> Vec<(String, String)> {
        events
            .iter()
            .map(|e| match e["type"].as_str().unwrap() {
                "status" => ("status".into(), e["data"]["status"]["state"].as_str().unwrap().into()),
                _ => ("turn".into(), format!("{} {}", e["data"]["turn"], e["data"]["state"].as_str().unwrap())),
            })
            .collect()
    };
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    told(&[root_report("working")]);
    core.lock().reg.terminals.get_mut(&id).unwrap().status_sent = Some(Instant::now() + Duration::from_secs(3600));
    let child = ghostty_vt::StatusReport { id: "sub".into(), ..root_report("working") };
    let events = told(&[child, root_report("error"), root_report("working")]);
    assert_eq!(
        kinds(&events),
        [pair("status", "working"), pair("status", "error"), pair("turn", "1 error"), pair("status", "working")]
    );
    for status in &events[..2] {
        assert_eq!(status["data"]["records"]["sub"]["state"], "working", "the whole record set, as before");
        assert_eq!(status["data"]["reported"], true);
    }

    let events =
        told(&[root_report("blocked"), root_report("working"), root_report("blocked"), root_report("working")]);
    assert_eq!(
        kinds(&events),
        [
            pair("status", "blocked"),
            pair("turn", "2 blocked"),
            pair("status", "working"),
            pair("status", "blocked"),
            pair("turn", "3 blocked"),
            pair("status", "working"),
        ],
        "each stuck word keeps its own event, with the state it had"
    );
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn a_wait_ends_on_a_turn_that_ended_and_restarted_in_one_chunk() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    core.report_status(&mut core.lock(), id, &[root_report("working")]);
    let spec = crate::wait::WaitSpec { output: None, timeout: Duration::from_secs(5), after_turn: None };
    let ended = {
        let waiting = core.wait(id, spec, None);
        tokio::pin!(waiting);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut waiting).await.is_err(), "still working");
        // An agent that finishes and picks up its next prompt before the core reads.
        core.report_status(&mut core.lock(), id, &[root_report("idle"), root_report("working")]);
        tokio::time::timeout(Duration::from_secs(2), waiting).await
    };
    let ended = ended.expect("the wait missed the turn's end");
    assert_eq!((ended.ended, ended.reason.as_str()), (true, "status"));
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn a_turn_that_ended_done_and_restarted_in_one_chunk_still_says_done() {
    let (core, root) = test_core();
    let id = new_terminal(&core);
    core.report_status(&mut core.lock(), id, &[root_report("working")]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    core.subscribe(&mut core.lock(), crate::events::Filter::default(), crate::events::Sink::Waiter(tx));
    core.report_status(&mut core.lock(), id, &[root_report("done"), root_report("working")]);
    let mut turns = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if event["type"] == "turn" {
            turns.push(event["data"].clone());
        }
    }
    // The bridge tells the phone a turn finished from a turn event that says done or idle.
    assert!(turns.iter().any(|t| t["state"] == "done"), "the finished turn is told: {turns:?}");
    drop(core);
    remove_root(&root);
}

// `text.redact` as a client calls it: through `api::dispatch`, params as JSON.
// Tokens are split with `concat!` so no whole secret shape sits in the source.

/// The reply's `text` (the only key in the result), or the method's error.
async fn text_redact(params: serde_json::Value) -> Result<String, crate::rpc::RpcError> {
    let (core, root) = test_core();
    let call = crate::api::Call { core: &core, conn: None, plain: None };
    let reply = crate::api::dispatch(&call, "text.redact", params).await;
    drop(core);
    remove_root(&root);
    reply.map(|v| {
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["text"], "{v}");
        v["text"].as_str().unwrap().to_string()
    })
}

async fn redacted(text: &str) -> String {
    text_redact(json!({"text": text})).await.unwrap_or_else(|e| panic!("{}: {}", e.kind, e.message))
}

#[tokio::test]
async fn text_redact_rpc_hides_the_common_secret_shapes() {
    let cases = [
        (
            concat!(
                "use sk-ant-api03-",
                "Xq9vB2mN7pL4kR8tW1yZ3cF6hJ0dS5gA-_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789-AbCdEfAA now"
            ),
            "use [redacted] now",
        ),
        (
            concat!("OPENAI sk-proj-", "Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78St90Uv12Wx34Yz56_T3BlbkFJ."),
            "OPENAI [redacted].",
        ),
        (concat!("sk-", "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789aBcDeFgHiJkL"), "[redacted]"),
        (
            concat!("git clone https://gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789@github.com/o/r"),
            "git clone https://[redacted]@github.com/o/r",
        ),
        (
            concat!("github", "_pat_11ABCDEFG0aBcDeFgHiJkLmN_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789aBcDeFgHiJkLmNoPq"),
            "[redacted]",
        ),
        (concat!("id AKIA", "IOSFODNN7EXAMPLE end"), "id [redacted] end"),
        (
            concat!("AWS_SECRET_ACCESS_KEY=", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            "AWS_SECRET_ACCESS_KEY=[redacted]",
        ),
        (concat!("slack xo", "xb-2048123456789-2048123456789-AbCdEfGhIjKlMnOpQrStUvWx"), "slack [redacted]"),
        (
            "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA1x\nq2Zr+abc/def==\n-----END RSA PRIVATE KEY-----\nafter",
            "before\n[redacted]\nafter",
        ),
        ("password=correcthorsebattery", "password=[redacted]"),
        (
            concat!("Authorization: Bearer ", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJlMTIz"),
            "Authorization: Bearer [redacted]",
        ),
        (r#"api_key: "abcd1234efgh5678ijkl""#, r#"api_key: "[redacted]""#),
        // A prefixed token inside once-escaped JSON (a log line holding a JSON body).
        (
            concat!(r#"{"body":"{\"token\":\"gh"#, r#"p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789\"}"}"#),
            r#"{"body":"{\"token\":\"[redacted]\"}"}"#,
        ),
    ];
    for (text, want) in cases {
        assert_eq!(redacted(text).await, want, "{text}");
    }
}

#[tokio::test]
async fn text_redact_rpc_hides_a_bare_jwt() {
    let jwt = concat!(
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ".eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
    );
    assert_eq!(redacted(&format!("session {jwt} expired")).await, "session [redacted] expired");
}

#[tokio::test]
async fn text_redact_rpc_hides_a_google_api_key() {
    let out = redacted(concat!("GOOGLE AI", "zaSyD-9tSrke72PouQMnMX-a7eZSW0jkFMBWY")).await;
    assert_eq!(out, "GOOGLE [redacted]");
}

#[tokio::test]
async fn text_redact_rpc_hides_a_stripe_live_key() {
    let out = redacted(concat!("stripe sk_", "live_4eC39HqLyjWDarjtT1zdp7dc")).await;
    assert_eq!(out, "stripe [redacted]");
}

#[tokio::test]
async fn text_redact_rpc_hides_the_secret_line_of_an_aws_credentials_file() {
    // ~/.aws/credentials writes `name = value`, with spaces around the `=`.
    let file = concat!(
        "[default]\naws_access_key_id = AKIA",
        "IOSFODNN7EXAMPLE\naws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n"
    );
    let out = redacted(file).await;
    assert_eq!(out, "[default]\naws_access_key_id = [redacted]\naws_secret_access_key = [redacted]\n");
}

#[tokio::test]
async fn text_redact_rpc_hides_all_of_a_password_with_symbols() {
    let out = redacted("password=Sup3r!Secret#99 then").await;
    assert!(!out.contains("Secret#99"), "{out}");
    assert_eq!(out, "password=[redacted] then");
}

#[tokio::test]
async fn text_redact_rpc_finds_a_key_name_inside_escaped_json() {
    // The name has no known prefix: only `api_key` says the value is a secret.
    let out = redacted(r#"{"body":"{\"api_key\":\"abcd1234efgh5678\"}"}"#).await;
    assert_eq!(out, r#"{"body":"{\"api_key\":\"[redacted]\"}"}"#);
}

#[tokio::test]
async fn text_redact_rpc_missing_null_or_empty_text_is_empty() {
    for params in [json!({}), serde_json::Value::Null, json!({"text": ""})] {
        assert_eq!(text_redact(params.clone()).await.map_err(|e| e.kind), Ok(String::new()), "{params}");
    }
}

#[tokio::test]
async fn text_redact_rpc_rejects_a_text_that_is_not_a_string_without_echoing_it() {
    let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    for params in [json!({"text": 42}), json!({"text": [token]}), json!({"text": {"inner": token}})] {
        let err = text_redact(params.clone()).await.unwrap_err();
        assert_eq!(err.kind, "invalid_params", "{params}");
        assert!(!err.message.contains(token) && !err.hint.contains(token), "{}", err.message);
    }
}

#[tokio::test]
async fn text_redact_rpc_redacts_a_secret_ending_exactly_at_64_kb_and_refuses_one_byte_more() {
    let max = 65_536;
    let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let pad = "x".repeat(max - token.len() - 1);
    let text = format!("{pad} {token}");
    assert_eq!(text.len(), max);
    assert_eq!(redacted(&text).await, format!("{pad} [redacted]"));
    // One byte more: an error and no text at all, not even a redacted part.
    let err = text_redact(json!({"text": format!("x{text}")})).await.unwrap_err();
    assert_eq!((err.kind.as_str(), err.code), ("too_large", -32000));
    assert!(
        !err.message.contains(token) && !err.hint.contains(token) && !err.message.contains("xxxx"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn text_redact_rpc_redacts_a_key_split_across_a_line_break() {
    // A Bearer token on the next line is still the token.
    let out = redacted(concat!("Authorization: Bearer\r\n", "eyJhbGciOiJIUzI1NiJ9.e30.c2lnbmF0dXJl1")).await;
    assert_eq!(out, "Authorization: Bearer\r\n[redacted]");
    // A long key wrapped by a terminal: the part before the break goes, the break stays.
    let out = redacted(concat!("key sk-ant-api03-", "ABCDEFGHIJKLMNOPQRST\r\nUVWXYZ0123456789abcd end")).await;
    assert!(out.starts_with("key [redacted]\r\n"), "{out}");
    assert!(!out.contains("ABCDEFGHIJ"), "{out}");
}

#[tokio::test]
async fn text_redact_rpc_redacts_many_secrets_in_one_text() {
    let shapes = [
        concat!("sk-proj-", "aBcDeFgHiJkLmNoPqRsT"),
        concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
        concat!("xo", "xb-2048123456789-AbCdEfGh"),
        concat!("AKIA", "IOSFODNN7EXAMPLE"),
    ];
    let text: String = (0..400).map(|i| format!("line {i}: {} ok\n", shapes[i % shapes.len()])).collect();
    let want: String = (0..400).map(|i| format!("line {i}: [redacted] ok\n")).collect();
    assert_eq!(redacted(&text).await, want);
}

#[tokio::test]
async fn text_redact_rpc_keeps_unicode_cjk_and_emoji_intact() {
    let token = concat!("sk-proj-", "aBcDeFgHiJkLmNoPqRsT");
    let text = format!("日本語 {token} 🔑 naïve café ✓ — 中文\n鍵{token}🔑 👩‍💻 ok");
    assert_eq!(redacted(&text).await, "日本語 [redacted] 🔑 naïve café ✓ — 中文\n鍵[redacted]🔑 👩‍💻 ok");
    let plain = "提示：请修复 bug 🐛 in état été — Ünïcödé ✓ 🇮🇳";
    assert_eq!(redacted(plain).await, plain);
}

#[tokio::test]
async fn text_redact_rpc_leaves_windows_paths_alone() {
    for path in [
        r"C:\Users\x\proj",
        r"C:\Users\x\proj\src\main.rs",
        r"cd C:\Program Files\HyperOx\bin && dir",
        r"\\server\share\team-project\build-output",
        r"\\?\C:\Users\x\AppData\Local\Temp\h0x-1234567890abcdef",
        r"C:\Users\x\.ssh\id_ed25519.pub",
        r#"{"cwd":"C:\\Users\\x\\proj"}"#,
    ] {
        assert_eq!(redacted(path).await, path);
    }
}

#[tokio::test]
async fn text_redact_rpc_handles_a_64_kb_line_without_spaces_quickly() {
    let blob = "QUJDRA".repeat(10_900);
    let text = format!("{blob}&token=abcdefgh12345678");
    assert!(text.len() <= 65_536);
    let started = Instant::now();
    assert_eq!(redacted(&text).await, format!("{blob}&token=[redacted]"));
    // Shapes that start a match at every byte but never finish one.
    for unit in ["-----BEGIN ", "Bearer\t", "key:", "sk-x"] {
        let text = unit.repeat(65_536 / unit.len());
        assert!(text_redact(json!({"text": text})).await.is_ok(), "{unit}");
    }
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}
