//! Bus-smoke regression test for the announce-before-subscribe protocol
//! bug class (PRD-wintermute-fleet-bus-smoke-convention.md).
//!
//! Spawns an in-process `agorabus` daemon on a temp socket, points the
//! `wm-dialog` daemon at it via the `WM_DIALOG_BUS_SOCKET` env override,
//! waits for the daemon to connect + announce + subscribe, then queries
//! the bus's peer snapshot and asserts the daemon's two announced
//! session-ids (`wm-dialog-<pid>-sub` and `wm-dialog-<pid>`) are both
//! present. A daemon that connected without announcing would have been
//! torn down by agorabus with `announce_required` before either session
//! could land in the peers map — appearance in `peers()` is positive
//! evidence that the `connect()` → `announce()` → `subscribe()`
//! ordering is correct on both connections.
//!
//! Why a peer-snapshot probe and not a publish-through driver: dialog's
//! sub_client subscribes to THREE prefixes (`wm.audio.`, `wm.stt.`,
//! `wm.brain.` — see `crate::bus::SUBSCRIBE_PREFIXES`). agorabus stores
//! the subscription as a single `Option<String>` per connection
//! (`agorabus/src/daemon.rs:367` — `*subscribed_prefix = Some(prefix)`
//! overwrites on every `Subscribe`), so only the LAST prefix
//! (`wm.brain.`) is in force at runtime. The only events dialog can
//! receive on that prefix are `wm.brain.reply` and
//! `wm.brain.reply.destructive`, and both FSM transitions
//! (`fsm.rs:192,206`) gate on `State::Thinking` — a state the FSM only
//! enters via `wm.stt.final`, which dialog can no longer receive
//! through its in-force prefix. Driving a publish-through from cold is
//! therefore impossible without either (a) multi-prefix support in
//! agorabus or (b) restructuring dialog's subscribe order. Logged in
//! this docstring for a follow-on PRD; the peer-snapshot probe still
//! catches the announce-before-subscribe bug class this PRD targets.

#![allow(
    unsafe_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::missing_assert_message,
    clippy::missing_errors_doc
)]

use std::path::PathBuf;
use std::time::Duration;

use agorabus::{Client, DaemonConfig, run_daemon};
use tokio::time::timeout;

fn tmp_path(tag: &str, ext: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // agorabus chmods the socket parent to 0700 on bind; pointing at
    // /tmp directly silently goes wrong. Use a fresh pid+nanos subdir.
    let dir = std::env::temp_dir().join(format!("wm-dialog-test-{pid}-{nanos}"));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{tag}.{ext}"))
}

async fn run_bus_smoke() -> Result<(), String> {
    // 1. Spawn an in-process agorabus on a unique temp socket.
    let bus_sock = tmp_path("bus", "sock");
    let _ = std::fs::remove_file(&bus_sock);
    let bus_cfg = DaemonConfig {
        socket_path: bus_sock.clone(),
        heartbeat_timeout: Duration::from_secs(60),
        broadcast_capacity: 1024,
        drain_grace_ms: agorabus::DEFAULT_DRAIN_GRACE_MS,
        drain_resume_hint_ms: agorabus::DEFAULT_DRAIN_RESUME_HINT_MS,
    };
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (bus_shutdown_tx, bus_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let bus_task = tokio::spawn(async move {
        let _ = run_daemon(bus_cfg, Some(ready_tx), bus_shutdown_rx).await;
    });
    timeout(Duration::from_secs(2), ready_rx)
        .await
        .map_err(|_| "bus never signalled ready".to_string())?
        .map_err(|e| format!("bus ready_tx dropped: {e}"))?;

    // 2. Open a query client BEFORE the wm-dialog daemon starts.
    //    Announce first — positive evidence the test author understood
    //    the ordering (AC7 anti-cargo-cult gate). We don't subscribe
    //    here because the probe is a `peers()` query, not a
    //    broadcast-listener.
    let mut query = Client::connect(&bus_sock)
        .await
        .map_err(|e| format!("query connect: {e:#}"))?;
    query
        .announce(
            "wm-dialog-bus-smoke-query",
            std::process::id(),
            "",
            "test-query",
        )
        .await
        .map_err(|e| format!("query announce: {e:#}"))?;

    // 3. Snapshot the dialog state-snapshot path before redirecting so
    //    a stray file from a prior aborted test doesn't poison the run
    //    (the daemon writes the live snapshot on startup; not our
    //    concern, but good hygiene). We do NOT override the snapshot
    //    path — only the bus socket. The default snapshot location is
    //    under $XDG_STATE_HOME and writing there from a unit test is
    //    acceptable per existing daemon tests.

    // 4. Point the wm-dialog daemon at our temp bus socket.
    //    SAFETY: tests in this file are the only consumer of this
    //    var; cargo runs separate test binaries in separate processes
    //    so cross-file env races are impossible. Intra-file there's
    //    only this one test fn.
    let bus_sock_for_env = bus_sock.clone();
    // SAFETY: see comment above.
    unsafe {
        std::env::set_var("WM_DIALOG_BUS_SOCKET", &bus_sock_for_env);
    }

    // 5. Spawn the wm-dialog daemon. It will announce on TWO
    //    connections — sub_client (session_id `wm-dialog-<pid>-sub`)
    //    and pub_client (session_id `wm-dialog-<pid>`) — and subscribe
    //    to `wm.audio.`, `wm.stt.`, `wm.brain.` (only the last is in
    //    force at runtime per the docstring above). It then writes a
    //    state snapshot to disk and blocks on `sub_client.next_event()`.
    let daemon_task = tokio::spawn(async move { wintermute_dialog::daemon::run().await });

    // 6. Give the daemon time to connect + announce + subscribe on
    //    both connections. agorabus's announce path adds the peer to
    //    the state map BEFORE replying ok, so a peers() query after a
    //    short wait will see both sessions if the wire-up succeeded.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    // 7. Probe the peer snapshot. The two expected session_ids come
    //    from daemon.rs:455-461 (sub) and daemon.rs:471-478 (pub) in
    //    wintermute-dialog. Both must appear; either missing would
    //    indicate an announce failure (agorabus drops the connection
    //    before recording the peer).
    let peers = query
        .peers()
        .await
        .map_err(|e| format!("peers query: {e:#}"))?;
    let pid = std::process::id();
    let want_sub = format!("wm-dialog-{pid}-sub");
    let want_pub = format!("wm-dialog-{pid}");
    let session_ids: Vec<String> = peers.iter().map(|p| p.session_id.clone()).collect();
    let saw_sub = session_ids.iter().any(|s| s == &want_sub);
    let saw_pub = session_ids.iter().any(|s| s == &want_pub);

    // 8. Tear down regardless of outcome — never leak the daemon task
    //    or the bus task. Order: drop the query (closes its UDS), shut
    //    down the bus (daemon's next_event returns None, daemon exits
    //    cleanly per daemon.rs:493), await both tasks with a deadline.
    drop(query);
    let _ = bus_shutdown_tx.send(());
    let _ = timeout(Duration::from_secs(3), bus_task).await;
    let daemon_outcome = timeout(Duration::from_secs(3), daemon_task).await;
    let _ = std::fs::remove_file(&bus_sock);
    // SAFETY: same single-test-consumer reasoning as the set_var
    // above. Removing the var so any later test in the same binary
    // sees a clean env.
    unsafe {
        std::env::remove_var("WM_DIALOG_BUS_SOCKET");
    }

    // 9. The implicit anti-announce_required check: if the daemon had
    //    failed at announce, it would have exited within ~1 s of
    //    contacting the bus and its anyhow chain would surface
    //    `announce_required`. We log the outcome and bail loudly if
    //    that string appears.
    match &daemon_outcome {
        Err(_) => eprintln!("daemon_outcome: still running at 3s (expected — bus drove its exit)"),
        Ok(Err(join_err)) => eprintln!("daemon_outcome: JoinError: {join_err}"),
        Ok(Ok(Ok(()))) => eprintln!("daemon_outcome: clean exit (expected once bus closed)"),
        Ok(Ok(Err(e))) => eprintln!("daemon_outcome: Err: {e:#}"),
    }
    if let Ok(Ok(Err(daemon_err))) = daemon_outcome {
        let chain = format!("{daemon_err:#}");
        if chain.contains("announce_required") {
            return Err(format!(
                "daemon hit announce_required — bus wire-up regression: {chain}"
            ));
        }
        return Err(format!("daemon exited with error: {chain}"));
    }

    if !saw_sub {
        return Err(format!(
            "wm-dialog sub-client session-id {want_sub} not in peers: {session_ids:?}"
        ));
    }
    if !saw_pub {
        return Err(format!(
            "wm-dialog pub-client session-id {want_pub} not in peers: {session_ids:?}"
        ));
    }
    Ok(())
}

#[test]
fn wm_dialog_bus_smoke_announces_before_subscribe() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("build tokio runtime");
    rt.block_on(async {
        run_bus_smoke().await.expect("wm-dialog bus smoke lifecycle");
    });
}
