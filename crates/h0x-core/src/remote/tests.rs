use super::*;
use crate::events::{Filter, Sink};
use crate::paths::Paths;
use std::sync::atomic::AtomicBool;

#[tokio::test]
async fn a_send_on_an_already_closed_line_fails_without_waiting() {
    for writer_gone in [false, true] {
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({}));
        if writer_gone {
            rx.close();
        } else {
            line.close(); // The writer may not have finished shutting down yet.
        }
        let sent = tokio::time::timeout(
            Duration::from_millis(200),
            line.call("away", "terminal.send", json!({"terminal": "t1", "text": "hi"}), Duration::from_secs(30)),
        )
        .await
        .expect("a closed transport must not hold a send");
        assert!(sent.unwrap_err().message.contains("lost the line"));
        assert!(lock(&line.pending).is_empty());
    }
}

#[tokio::test]
async fn a_held_remote_send_stops_when_its_caller_cancels_and_tells_the_machine() {
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        flag.store(true, Ordering::Relaxed);
    });
    // A forwarded send has no deadline of its own: only the line or its caller ends it.
    let sent = tokio::time::timeout(
        Duration::from_secs(2),
        crate::server::SEND_CANCELLED.scope(
            cancelled,
            line.call("away", "terminal.send", json!({"terminal": "t1", "text": "hi"}), Duration::MAX),
        ),
    )
    .await
    .expect("a cancelled send must not wait for its deadline");
    assert_eq!(sent.unwrap_err().kind, "cancelled");
    assert!(lock(&line.pending).is_empty());
    let mut told = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        told.push(String::from_utf8_lossy(&frame).into_owned());
    }
    assert!(told.iter().any(|f| f.contains("$/cancelRequest")), "{told:?}");

    // Other calls ignore the cancelled flag and keep their deadline.
    let flag = Arc::new(AtomicBool::new(true));
    let keys = crate::server::SEND_CANCELLED
        .scope(flag, line.call("away", "terminal.keys", json!({"terminal": "t1"}), Duration::from_millis(100)))
        .await;
    assert!(keys.unwrap_err().message.contains("didn't answer"));
    // An unanswered send outside any caller scope times out with a read-first hint.
    let sent = line.call("away", "terminal.send", json!({"terminal": "t1"}), Duration::from_millis(100)).await;
    assert!(sent.unwrap_err().hint.contains("h0x-core read"));
}

#[test]
fn automatic_input_requires_support_at_every_hop() {
    for features in [json!([]), json!(["automatic_input"])] {
        let (frames, _rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": features}));
        for reply in [json!({}), json!({"automatic_input": false}), json!({"automatic_input": true})] {
            assert_eq!(
                automatic_input(&line, &reply),
                !features.as_array().unwrap().is_empty() && reply["automatic_input"] == true
            );
        }
    }
}

#[tokio::test]
async fn automatic_input_survives_relays_but_not_a_capability_downgrade() {
    for (feature, confirmed) in [(false, true), (true, false), (true, true)] {
        let root = crate::platform::test_scratch().join(format!(
            "h0x-core-automatic-relay-{}-{}",
            std::process::id(),
            crate::platform::random_hex()
        ));
        let paths = Paths {
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
        let machine = Machine {
            name: "away".into(),
            target: "away".into(),
            via: false,
            temporary: false,
            session: None,
            id: "remote".into(),
            host: "away".into(),
            version: "test".into(),
        };
        let conn = Arc::new(Conn::default());
        conn.with(|out| out.open(7));
        let (old_frames, mut old_rx) = mpsc::unbounded_channel();
        core.lock().relayed.insert(
            (conn_key(&conn), 7),
            Relayed {
                active_at: std::time::Instant::now(),
                automatic_input: true,
                line: Line::new(old_frames, &json!({"features": ["automatic_input"]})),
                there: 9,
                conn: conn.clone(),
                machine: machine.id.clone(),
                terminal: "t1".into(),
                params: json!({"terminal": "t1"}),
                size_pending: false,
            },
        );
        assert!(relay_in(&core, &conn, 7, wire::AUTOMATIC_INPUT, b"\x1b"));
        let frame = old_rx.recv().await.unwrap();
        assert_eq!(frame, wire::encode(9, wire::AUTOMATIC_INPUT, b"\x1b"));
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": if feature { vec!["automatic_input"] } else { vec![] }}));
        let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        line.take_reply(json!({"id": request["id"], "result": {"channel": 10, "automatic_input": confirmed}}));
        task.await.unwrap();
        if feature && confirmed {
            assert!(core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(relay_in(&core, &conn, 7, wire::AUTOMATIC_INPUT, b"[48;24;80t"));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::AUTOMATIC_INPUT, b"[48;24;80t"));
        } else {
            assert!(!core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(!lock(&line.streams).contains_key(&10));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::CLOSE, b""));
            let close = conn.outbox().next_frame().unwrap();
            assert_eq!(close[4], wire::CLOSE);
        }
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn reattach_applies_missed_sizes_without_replaying_delivered_activity() {
    let root = crate::platform::test_scratch().join(format!("h0x-core-reattach-{}", crate::platform::random_hex()));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine: Machine = serde_json::from_value(json!({
        "name": "away", "target": "away", "id": "away", "host": "away", "version": "test"
    }))
    .unwrap();
    let conn = Arc::new(Conn::default());
    let (old_frames, _old_rx) = mpsc::unbounded_channel();
    let old = Line::new(old_frames, &json!({}));
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let now = std::time::Instant::now();
    for (ch, active_at, target, via) in
        [(7, now - Duration::from_secs(1), "t1", &old), (8, now, "t1", &old), (9, now, "t2", &line)]
    {
        core.lock().relayed.insert(
            (conn_key(&conn), ch),
            Relayed {
                active_at,
                automatic_input: false,
                line: via.clone(),
                there: ch,
                conn: conn.clone(),
                machine: machine.id.clone(),
                terminal: target.into(),
                params: json!({"terminal": target, "cols": 80, "rows": 24}),
                size_pending: false,
            },
        );
    }
    let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
    let frame = rx.recv().await.unwrap();
    let first: Value = serde_json::from_slice(&frame[wire::HEADER..]).unwrap();
    let size = [132, 0, 45, 0];
    assert!(relay_in(&core, &conn, 7, wire::RESIZE, &size));
    line.take_reply(json!({"id": first["id"], "result": {"channel": 17}}));
    assert_eq!(rx.recv().await.unwrap(), wire::encode(17, wire::RESIZE, &size));
    let frame = rx.recv().await.unwrap();
    let second: Value = serde_json::from_slice(&frame[wire::HEADER..]).unwrap();
    let missed = [90, 0, 30, 0];
    assert!(relay_in(&core, &conn, 8, wire::RESIZE, &missed));
    assert!(relay_in(&core, &conn, 7, wire::FOCUS, &[1]));
    assert!(relay_in(&core, &conn, 9, wire::RESIZE, &size));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(17, wire::FOCUS, &[1]));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(9, wire::RESIZE, &size));
    line.take_reply(json!({"id": second["id"], "result": {"channel": 18}}));
    task.await.unwrap();
    assert!(rx.try_recv().is_err(), "a newer viewer is already in control and B was never disconnected");
    {
        let st = core.lock();
        assert_eq!(st.relayed[&(conn_key(&conn), 7)].params["cols"], 132);
        assert_eq!(st.relayed[&(conn_key(&conn), 8)].params["cols"], 90);
    }
    assert!(relay_in(&core, &conn, 8, wire::FOCUS, &[1]));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(18, wire::RESIZE, &missed));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(18, wire::FOCUS, &[1]));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn reconnect_rejects_downgrade_before_mapping_fallback_data() {
    for stalled in [false, true] {
        // The immediate relay is capable, but its deeper path has downgraded.
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": ["resize_notifications"]}));
        let conn = Arc::new(Conn::default());
        conn.with(|out| {
            out.open(7);
            if stalled {
                out.data(7, &vec![120; wire::WINDOW]);
                while out.next_frame().is_some() {}
            }
        });
        let calling = line.clone();
        let sink = conn.clone();
        let task = tokio::spawn(async move {
            calling
                .call_into(
                    "relay",
                    "terminal.attach",
                    json!({"observe": true, "resumed": true, "resize_notifications": true}),
                    Some(((sink, 7), true)),
                    Duration::from_secs(1),
                )
                .await
        });
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        line.take_reply(json!({"id": request["id"], "result": {"channel": 9, "resize_notifications": false}}));
        // Model buffered DATA consumed before the awakened caller can run.
        if let Some((sink, here)) = lock(&line.streams).get(&9) {
            sink.with(|out| {
                out.data(*here, b"unsupported fallback paint");
            });
        }
        assert!(task.await.unwrap().is_err(), "downgraded reattach must be rejected");
        conn.with(|out| {
            assert_eq!(out.queued(7), 0, "rejected fallback DATA must not enter the downstream queue");
            out.close(7, b"");
            let frame = out.next_frame().expect("CLOSE must not wait for credit");
            assert_eq!(wire::decode_header(frame[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::CLOSE);
            assert!(out.next_frame().is_none());
        });
        assert!(!lock(&line.streams).contains_key(&9), "rejected stream must never be mapped");
    }
}

#[tokio::test]
async fn failed_reattach_drains_pending_data_before_close() {
    let root = crate::platform::test_scratch().join(format!(
        "h0x-core-reattach-drain-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "away".into(),
        target: "away".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "away".into(),
        version: "test".into(),
    };
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(7);
        assert!(out.data(7, &vec![b'x'; wire::WINDOW]));
        while out.next_frame().is_some() {}
        assert!(out.data(7, b"last output before the line dropped"));
        assert!(out.next_frame().is_none(), "pending output must wait for credit");
    });
    let (old_frames, old_rx) = mpsc::unbounded_channel();
    let old_line = Line::new(old_frames, &json!({}));
    core.lock().relayed.insert(
        (conn_key(&conn), 7),
        Relayed {
            active_at: std::time::Instant::now(),
            automatic_input: true,
            line: old_line,
            there: 9,
            conn: conn.clone(),
            machine: machine.id.clone(),
            terminal: "t1".into(),
            params: json!({"terminal": "t1"}),
            size_pending: false,
        },
    );
    drop(old_rx); // The viewer survives its old line.
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
    let request = rx.recv().await.unwrap();
    let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
    assert_eq!(request["method"], "terminal.attach");
    line.take_reply(crate::rpc::response(
        request["id"].clone(),
        Err(RpcError::new("finished", "terminal finished", "")),
    ));
    task.await.unwrap();
    assert!(core.lock().relayed.is_empty());
    conn.with(|out| {
        out.credit(7, wire::WINDOW);
        let data = out.next_frame().expect("queued output before CLOSE");
        assert_eq!(wire::decode_header(data[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::DATA);
        assert_eq!(&data[wire::HEADER..], b"last output before the line dropped");
        let close = out.next_frame().expect("CLOSE after queued output");
        assert_eq!(wire::decode_header(close[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::CLOSE);
        assert!(out.next_frame().is_none());
    });
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_misses_and_renames_keep_subscribers_on_the_current_machine() {
    let root = crate::platform::test_scratch().join(format!(
        "h0x-core-doc-missed-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "rohan".into(),
        target: "rohan".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "rohan".into(),
        version: "test".into(),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let (filtered_tx, mut filtered_rx) = mpsc::channel(4);
    {
        let mut st = core.lock();
        st.events.seq = 7;
        st.machines.push(machine.clone());
        core.subscribe(&mut st, Filter::default(), Sink::Plain { tx, behind: Arc::new(AtomicBool::new(false)) });
        core.subscribe(
            &mut st,
            Filter { terminal: Some("t1".into()), ..Default::default() },
            Sink::Plain { tx: filtered_tx, behind: Arc::new(AtomicBool::new(false)) },
        );
    }
    let (frames, _rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    mirror_events(&core, &machine, &line, b"{\"method\":\"missed\",\"params\":{\"seq\":9999,\"terminals\":[]}}\n");
    let msg: Value = serde_json::from_str(&rx.try_recv().expect("local missed notification")).unwrap();
    assert_eq!(msg["method"], "missed");
    assert_eq!(msg["params"]["seq"], 7);
    assert!(filtered_rx.try_recv().is_err());
    mirror_replace(&core, &machine, vec![json!({"address":"t1", "state":"live", "status":{"state":"working"}})]);
    rx.try_recv().unwrap();
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(1);
        assert!(out.data_lines(1, &vec![b'\n'; wire::MAX_PENDING]));
    });
    {
        let mut st = core.lock();
        core.subscribe(
            &mut st,
            Filter { terminal: Some("renamed/t1".into()), ..Default::default() },
            Sink::Framed { conn: conn.clone(), channel: 1 },
        );
        st.machines[0].name = "renamed".into();
        rename_mirror(&core, &mut st, &machine.id, "rohan", "renamed");
    }
    let mut bytes = Vec::new();
    conn.with(|out| {
        while let Some(frame) = out.next_frame() {
            bytes.extend_from_slice(&frame[wire::HEADER..]);
        }
    });
    let missed = bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|line| line["method"] == "missed")
        .expect("backpressure snapshot");
    assert_eq!(missed["params"]["terminals"][0]["address"], "renamed/t1");
    let removed: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    let created: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(removed["params"]["terminal"], "rohan/t1");
    assert_eq!(removed["params"]["type"], "removed");
    assert_eq!(created["params"]["terminal"], "renamed/t1");
    assert_eq!(created["params"]["data"]["status"]["state"], "working");
    mirror_events(&core, &machine, &line, b"{\"method\":\"event\",\"params\":{\"type\":\"status\",\"terminal\":\"t1\",\"data\":{\"status\":{\"state\":\"blocked\"}}}}\n");
    let status: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(status["params"]["terminal"], "renamed/t1");
    assert_eq!(core.lock().mirrors[&machine.id]["renamed/t1"]["status"]["state"], "blocked");
    core.lock().machines.clear();
    mirror_event(&core, &machine, json!({"type": "doc", "data": {"name": "app", "version": 1}}));
    assert!(rx.try_recv().is_err(), "removed machines must not emit doc events");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// A core here (`here`) that has added `rohan`, with terminals t1 and t2.
fn here_with_rohan(tag: &str) -> (Arc<Core>, std::path::PathBuf, Machine) {
    let root = crate::platform::test_scratch().join(format!(
        "h0x-core-{tag}-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "rohan".into(),
        target: "rohan".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "rohan".into(),
        version: "test".into(),
    };
    {
        let mut st = core.lock();
        st.machines.push(machine.clone());
        for _ in 0..2 {
            let new = crate::terminals::NewTerminal { argv: vec!["x".into()], ..Default::default() };
            st.reg.create(new, 0).unwrap();
        }
    }
    (core, root, machine)
}

#[tokio::test]
async fn a_machine_gets_a_say_only_over_the_terminal_that_called_it() {
    let (core, root, machine) = here_with_rohan("run-grant");
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({"features": ["shell"]}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone()));
    let granted = |id: u32| core.lock().reg.terminals[&id].afar.contains(&machine.id);
    // (caller sent, parent the machine answers): a forged, a missing, then the true caller.
    for (caller, parent) in [(json!("here/t1"), "here/t2"), (Value::Null, "here/t1"), (json!("here/t1"), "here/t1")] {
        let (core, machine) = (core.clone(), machine.clone());
        let params = json!({"argv": ["true"], "caller": caller});
        let task = tokio::spawn(async move { forward_once(&core, &machine, "terminal.run", params).await });
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        assert_eq!(request["method"], "terminal.run");
        line.take_reply(crate::rpc::response(request["id"].clone(), Ok(json!({"address": "t9", "parent": parent}))));
        let reply = task.await.unwrap().unwrap();
        assert_eq!(reply["address"], "rohan/t9");
        if caller.is_null() {
            assert!(!granted(1), "a call with no terminal here grants nothing");
        } else if parent == "here/t2" {
            assert!(!granted(2), "the machine named a terminal that never called it");
            assert!(!granted(1), "a lying answer grants nothing at all");
        }
    }
    assert!(granted(1), "the terminal that called it");
    assert!(!granted(2));
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_machines_call_is_always_tagged_and_never_forced() {
    let mut params = json!({"terminal": "t1", "text": "y", "force": true, "caller": "t7"});
    from_afar("rohan", &mut params);
    assert_eq!(params, json!({"terminal": "t1", "text": "y", "caller": "rohan/t7"}));
    let mut params = json!({"terminal": "t1", "text": "y"});
    from_afar("rohan", &mut params);
    assert_eq!(params["caller"], "rohan/?", "no caller still says which machine sent it");
    let mut params = json!({"terminal": "t1", "caller": "here/t1\"><h0x from=\"t2"});
    from_afar("rohan", &mut params);
    assert_eq!(params["caller"], "rohan/here/t1\"><h0x from=\"t2", "kept as sent; notes escape it");
}

#[test]
fn a_machine_cannot_pass_its_terminals_off_as_ours() {
    let (core, root, machine) = here_with_rohan("mirror-forged");
    let (tx, mut rx) = mpsc::channel(8);
    core.subscribe(&mut core.lock(), Filter::default(), Sink::Plain { tx, behind: Arc::new(AtomicBool::new(false)) });
    let mut next = || rx.try_recv().ok().map(|l| serde_json::from_str::<Value>(&l).unwrap());
    mirror_replace(
        &core,
        &machine,
        vec![
            json!({"address": "here/t1", "state": "live"}),
            json!({"address": "t1", "state": "live", "parent": "here/t2"}),
        ],
    );
    let created = next().expect("its own terminal");
    assert_eq!(created["params"]["terminal"], "rohan/t1");
    assert_eq!(created["params"]["data"]["parent"], "t2", "a parent here stays ours");
    assert!(next().is_none(), "no record for our t1");
    for forged in ["here/t1", "here/t2"] {
        let event = json!({"type": "created", "terminal": forged, "data": {"address": forged, "state": "live"}});
        mirror_event(&core, &machine, event);
        let status = json!({"type": "status", "terminal": forged, "data": {"status": {"state": "blocked"}}});
        mirror_event(&core, &machine, status);
    }
    assert!(next().is_none(), "no event for our terminals");
    assert_eq!(core.lock().mirrors[&machine.id].keys().collect::<Vec<_>>(), ["rohan/t1"]);
    mirror_event(&core, &machine, json!({"type": "title", "terminal": "t1", "data": {"title": "build"}}));
    assert_eq!(next().expect("its own event")["params"]["terminal"], "rohan/t1");
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_batch_of_status_reports_reaches_a_mirror_with_the_whole_record_set() {
    let (core, root, machine) = here_with_rohan("status-batch");
    // What a terminal here tells subscribers stands in for what rohan's sends us.
    let id = *core.lock().reg.terminals.keys().next().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    core.subscribe(&mut core.lock(), Filter::default(), Sink::Waiter(tx));
    let report = |state: &'static str, id: &str| ghostty_vt::StatusReport {
        state,
        kind: None,
        progress: None,
        id: id.into(),
        app: String::new(),
        title: String::new(),
        message: String::new(),
    };
    let batch = [report("working", ""), report("working", "a"), report("done", "a/b"), report("idle", "")];
    core.report_status(&mut core.lock(), id, &batch);
    let mut wire = Vec::new();
    while let Ok(params) = rx.try_recv() {
        wire.extend_from_slice(json!({"method": "event", "params": params}).to_string().as_bytes());
        wire.push(b'\n');
    }
    let address = crate::terminals::address(id);
    mirror_replace(&core, &machine, vec![json!({"address": address, "state": "live", "turn": 0})]);
    let (frames, _rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    mirror_events(&core, &machine, &line, &wire);
    let here = core.lock().reg.terminals[&id].clone();
    let mirrored = core.lock().mirrors[&machine.id][&format!("rohan/{address}")].clone();
    assert_eq!(mirrored["records"], json!(here.records), "clients replace the records with the event's");
    assert_eq!(mirrored["records"].as_object().map(|r| r.len()), Some(2));
    assert_eq!((mirrored["status"]["state"].as_str(), mirrored["turn"].as_u64()), (Some("idle"), Some(1)));
    assert_eq!(mirrored["reported"], true);
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_line_lost_notice_shows_even_if_the_line_dropped_mid_sequence() {
    let mut screen = ghostty_vt::Terminal::new(80, 24, 0);
    screen.write(b"\x1b_Gm=1;AAAA");
    screen.write(lost_notice("rohan").as_bytes());
    assert!(screen.plain_text().contains("[line to rohan lost"));
}

#[tokio::test]
async fn provisional_grants_survive_only_successful_or_ambiguous_launches() {
    let root = crate::platform::test_scratch().join(format!("h0x-core-run-grants-{}", crate::platform::random_hex()));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine: Machine = serde_json::from_value(json!({
        "name": "away", "target": "away", "id": "away", "host": "away", "version": "test"
    }))
    .unwrap();
    let failed = || Err(RpcError::new("spawn_failed", "bad program", ""));
    for (first, second, retained) in [
        (Some(failed()), Some(failed()), false),
        (Some(failed()), Some(Ok(json!({}))), true),
        (Some(Ok(json!({}))), Some(failed()), true),
        (Some(failed()), None, true), // The forwarding future was cancelled.
        (Some(failed()), Some(Err(RpcError::new("unavailable", "lost the line", ""))), true),
        (Some(failed()), Some(Err(RpcError::new("spawn_failed", "the holder did not answer", ""))), true),
        (Some(failed()), Some(Err(RpcError::new("internal", "cannot save terminals", ""))), true),
    ] {
        let parent = core
            .lock()
            .reg
            .create(crate::terminals::NewTerminal { argv: vec!["true".into()], ..Default::default() }, 0)
            .unwrap();
        let params = json!({"caller": format!("here/{}", crate::terminals::address(parent))});
        let a = RunGrant::start(&core, &machine, "terminal.run", &params).unwrap();
        let b = RunGrant::start(&core, &machine, "terminal.run", &params).unwrap();
        a.answered(&first.unwrap());
        assert!(core.lock().reg.terminals[&parent].afar.contains("away"), "another run still needs the grant");
        match second {
            Some(result) => b.answered(&result),
            None => drop(b),
        }
        assert_eq!(core.lock().reg.terminals[&parent].afar.contains("away"), retained);
        if retained {
            assert!(RunGrant::start(&core, &machine, "terminal.run", &params).is_none());
            assert!(core.lock().reg.terminals[&parent].afar.contains("away"));
        }
        assert!(core.lock().afar_runs.is_empty());
    }
    core.flush_saved().await.unwrap();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

fn grant_core(name: &str) -> (Arc<Core>, Machine, std::path::PathBuf) {
    let root = crate::platform::test_scratch().join(format!("h0x-core-{name}-{}", crate::platform::random_hex()));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine: Machine = serde_json::from_value(json!({
        "name": "away", "target": "away", "id": "away", "host": "away", "version": "test"
    }))
    .unwrap();
    (core, machine, root)
}

#[tokio::test]
async fn only_a_run_this_core_asked_for_gets_a_grant() {
    let (core, machine, root) = grant_core("run-grant-none");
    let parent = core
        .lock()
        .reg
        .create(crate::terminals::NewTerminal { argv: vec!["true".into()], ..Default::default() }, 0)
        .unwrap();
    let caller = format!("here/{}", crate::terminals::address(parent));
    for (method, params) in [
        ("terminal.send", json!({"caller": caller})),
        ("terminal.run", json!({"caller": caller, "no_parent": true})),
        ("terminal.run", json!({"caller": caller, "parent": "t9"})),
        ("terminal.run", json!({})),
        ("terminal.run", json!({"caller": null})),
        ("terminal.run", json!({"caller": crate::terminals::address(parent)})),
        ("terminal.run", json!({"caller": format!("elsewhere/{}", crate::terminals::address(parent))})),
        ("terminal.run", json!({"caller": "here/ppid:12"})),
        ("terminal.run", json!({"caller": "here/t9999"})),
    ] {
        assert!(RunGrant::start(&core, &machine, method, &params).is_none(), "{method} {params}");
        assert!(core.lock().afar_runs.is_empty(), "{method} {params}");
        assert!(core.lock().reg.terminals[&parent].afar.is_empty(), "{method} {params}");
    }
    // `no_parent: false` is no opt-out.
    let grant = RunGrant::start(&core, &machine, "terminal.run", &json!({"caller": caller, "no_parent": false}));
    assert!(grant.is_some());
    drop(grant);
    core.flush_saved().await.unwrap();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_single_run_that_proves_nothing_started_removes_its_grant() {
    let (core, machine, root) = grant_core("run-grant-one");
    let err = |kind: &str, message: &str| Err(RpcError::new(kind, message, ""));
    for (result, retained) in [
        (Ok(json!({})), true),
        (err("invalid_params", "bad"), false),
        (err("name_taken", "x"), false),
        (err("machine_unknown", "x"), false),
        (err("unknown_method", "x"), false),
        (err("unavailable", "the core is stopping"), false),
        (err("unavailable", "no holder: cannot start"), false),
        (err("unavailable", "this core has used every terminal id"), false),
        (err("unavailable", "registry persistence timed out; the operation may already have completed"), true),
        (err("unavailable", "the core is stopping soon"), true),
        (err("spawn_failed", "the holder did not answer"), true),
        (err("needs_update", "older"), true),
        (err("timeout", "x"), true),
    ] {
        let parent = core
            .lock()
            .reg
            .create(crate::terminals::NewTerminal { argv: vec!["true".into()], ..Default::default() }, 0)
            .unwrap();
        let params = json!({"caller": format!("here/{}", crate::terminals::address(parent))});
        let grant = RunGrant::start(&core, &machine, "terminal.run", &params).unwrap();
        assert!(core.lock().reg.terminals[&parent].afar.contains("away"), "granted before the reply");
        let label = format!("{result:?}");
        grant.answered(&result);
        assert_eq!(core.lock().reg.terminals[&parent].afar.contains("away"), retained, "{label}");
        assert!(core.lock().afar_runs.is_empty(), "{label}");
    }
    core.flush_saved().await.unwrap();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_failed_run_never_revokes_a_grant_that_was_already_permanent() {
    let (core, machine, root) = grant_core("run-grant-kept");
    let parent = core
        .lock()
        .reg
        .create(crate::terminals::NewTerminal { argv: vec!["true".into()], ..Default::default() }, 0)
        .unwrap();
    core.lock().reg.terminals.get_mut(&parent).unwrap().afar.insert("away".into());
    let params = json!({"caller": format!("here/{}", crate::terminals::address(parent))});
    assert!(RunGrant::start(&core, &machine, "terminal.run", &params).is_none(), "nothing provisional to track");
    assert!(core.lock().reg.terminals[&parent].afar.contains("away"));
    // The parent removed while its run was in flight: dropping the grant is harmless.
    let other = core
        .lock()
        .reg
        .create(crate::terminals::NewTerminal { argv: vec!["true".into()], ..Default::default() }, 0)
        .unwrap();
    let params = json!({"caller": format!("here/{}", crate::terminals::address(other))});
    let grant = RunGrant::start(&core, &machine, "terminal.run", &params).unwrap();
    core.lock().reg.terminals.remove(&other);
    grant.answered(&Err(RpcError::new("invalid_params", "bad", "")));
    assert!(core.lock().afar_runs.is_empty());
    core.flush_saved().await.unwrap();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_returning_viewer_sends_its_missed_size_once_on_its_first_activity() {
    let (core, machine, root) = grant_core("size-pending");
    let conn = Arc::new(Conn::default());
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    let insert = |observe: bool| {
        core.lock().relayed.insert(
            (conn_key(&conn), 7),
            Relayed {
                active_at: std::time::Instant::now(),
                automatic_input: false,
                line: line.clone(),
                there: 17,
                conn: conn.clone(),
                machine: machine.id.clone(),
                terminal: "t1".into(),
                params: json!({"terminal": "t1", "cols": 90, "rows": 30, "observe": observe}),
                size_pending: true,
            },
        );
    };
    let size = wire::encode(17, wire::RESIZE, &[90, 0, 30, 0]);
    insert(false);
    // Not activity: a focus loss, an empty focus, or a malformed resize.
    for (kind, payload) in [(wire::FOCUS, &[0u8][..]), (wire::FOCUS, &[][..]), (wire::RESIZE, &[1, 0, 2][..])] {
        assert!(relay_in(&core, &conn, 7, kind, payload));
        assert_eq!(rx.try_recv().unwrap(), wire::encode(17, kind, payload), "only the frame itself");
        assert!(rx.try_recv().is_err());
    }
    assert!(core.lock().relayed[&(conn_key(&conn), 7)].size_pending);
    assert!(relay_in(&core, &conn, 7, wire::DATA, b"x"));
    assert_eq!(rx.try_recv().unwrap(), size, "the missed size first");
    assert_eq!(rx.try_recv().unwrap(), wire::encode(17, wire::DATA, b"x"));
    assert!(relay_in(&core, &conn, 7, wire::DATA, b"y"));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(17, wire::DATA, b"y"), "sent once only");
    assert!(rx.try_recv().is_err());

    // A resize of its own replaces the missed one.
    insert(false);
    let own = [100, 0, 40, 0];
    assert!(relay_in(&core, &conn, 7, wire::RESIZE, &own));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(17, wire::RESIZE, &own));
    assert!(relay_in(&core, &conn, 7, wire::DATA, b"z"));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(17, wire::DATA, b"z"));
    assert!(rx.try_recv().is_err());

    // An observer's typing is no claim on the size.
    insert(true);
    let before = core.lock().relayed[&(conn_key(&conn), 7)].active_at;
    assert!(relay_in(&core, &conn, 7, wire::FOCUS, &[1]));
    assert_eq!(rx.try_recv().unwrap(), wire::encode(17, wire::FOCUS, &[1]));
    assert!(rx.try_recv().is_err());
    assert_eq!(core.lock().relayed[&(conn_key(&conn), 7)].active_at, before);

    assert!(!relay_in(&core, &conn, 8, wire::DATA, b"x"), "not relayed");
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_line_lost_notice_survives_any_cut_sequence_and_odd_names() {
    for cut in [&b"\x1b"[..], b"\x1b[", b"\x1b[38;2;1", b"\x1b]0;title", b"\x1bP1$r", b"\x1b_Gm=1;"] {
        let mut screen = ghostty_vt::Terminal::new(80, 24, 0);
        screen.write(cut);
        screen.write(lost_notice("rohan").as_bytes());
        assert!(screen.plain_text().contains("[line to rohan lost; reconnecting...]"), "{cut:?}");
    }
    assert!(lost_notice("").starts_with('\x18'));
    assert!(lost_notice("my-box.lan").contains("[line to my-box.lan lost"));
}

#[test]
fn login_retries_back_off_to_ten_minutes_and_stay_there() {
    let secs = |attempt| login_backoff(attempt).as_secs();
    assert_eq!((secs(0), secs(1), secs(2)), (120, 300, 600));
    assert_eq!((secs(3), secs(1000), secs(u32::MAX)), (600, 600, 600));
}
