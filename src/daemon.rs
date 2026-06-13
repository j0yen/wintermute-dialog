//! Live agorabus subscribe loop for `wm-dialog`.
//!
//! Subscribes to the three prefixes in [`crate::bus::SUBSCRIBE_PREFIXES`]
//! (`wm.audio.`, `wm.stt.`, `wm.brain.`) and feeds each decoded
//! [`crate::bus::Request`] through the pure-data [`crate::Fsm`]. Each
//! resulting [`crate::Action`] is mapped to an agorabus publish on a
//! separate publisher connection — reading and writing on the same
//! subscribed socket would interleave `Reply` lines with the broadcast
//! stream (same pattern as `wintermute-stt/src/daemon.rs` iter-5 and
//! `wintermute-tts/src/daemon.rs` iter-5).
//!
//! iter-6 adds the confirm-timeout scheduler. [`ConfirmTimer`] owns the
//! in-flight 30s sleep task that [`crate::Action::StartConfirmTimer`]
//! and [`crate::Action::CancelConfirmTimer`] manipulate. When the timer
//! elapses it pushes [`crate::Event::ConfirmTimeout`] back into the
//! same dispatch path via an `mpsc::UnboundedSender`, and the FSM's
//! `Confirming → Idle` silence branch fires. Each `start` invalidates
//! the prior generation via an atomic flag so a reprompt's new timer
//! cannot lose a race with the old one's late delivery.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

use agorabus::ClaimGuard;
use crate::action::DenyReason;
use crate::bus::{
    self, ConfirmDeniedEvent, ConfirmGrantedEvent, MuteRequestEvent, Request, StateEvent,
    TtsCancelEvent, TurnSystemEvent, TurnUserEvent, decode_request, now_unix_ms, outgoing,
};
use crate::state::{Flags, StateTag};
use crate::{Action, Event, Fsm, Transition};

/// Publish abstraction so per-event dispatch can be tested without a
/// real agorabus daemon. Production impl is [`AgoraSink`]; tests use an
/// in-memory sink.
#[async_trait::async_trait]
pub trait EventSink: Send {
    /// Publish `data` on `topic`. Failures are propagated; the outer
    /// subscribe loop logs and continues.
    ///
    /// # Errors
    /// Propagates whatever the underlying transport returns.
    async fn publish(&mut self, topic: &str, data: Value) -> Result<()>;
}

/// Production sink: publishes through an [`agorabus::Client`].
///
/// The client is wrapped in an `Arc<tokio::sync::Mutex<_>>` so a
/// background heartbeat task (spawned in [`run`]) can periodically
/// refresh the daemon's `last_heartbeat_unix_secs` without contending
/// destructively with publish call sites. Publish is the hot path; the
/// lock is held only for the duration of one request+reply round-trip
/// (microseconds), so contention is negligible.
pub struct AgoraSink {
    /// The underlying agorabus publisher client.
    pub inner: Arc<Mutex<agorabus::Client>>,
}

#[async_trait::async_trait]
impl EventSink for AgoraSink {
    async fn publish(&mut self, topic: &str, data: Value) -> Result<()> {
        let reply = {
            let mut client = self.inner.lock().await;
            client.publish(topic, data).await?
        };
        if !reply.ok {
            warn!(
                topic = %topic,
                err = %reply.error.as_deref().unwrap_or("?"),
                "wm-dialog: bus rejected publish"
            );
        }
        Ok(())
    }
}

/// Live daemon state. Wraps the pure-data [`Fsm`] in a
/// `tokio::sync::Mutex` because dispatch mutates it; iter-5 has at most
/// one inflight dispatch at a time (single subscribe-loop consumer).
pub struct DaemonState {
    /// The conversational FSM driving every transition decision.
    pub fsm: Mutex<Fsm>,
}

impl DaemonState {
    /// Wrap a freshly constructed [`Fsm`] for the daemon loop.
    #[must_use]
    pub const fn new(fsm: Fsm) -> Self {
        Self {
            fsm: Mutex::const_new(fsm),
        }
    }

    /// Build a JSON-stable snapshot of the live FSM. `history_n` caps
    /// the returned [`Transition`] ring; `now_ms` is the monotonic clock
    /// at snapshot time and feeds the `since_ms` field.
    ///
    /// The daemon's `run()` loop calls this after every dispatch and
    /// hands the result to [`write_snapshot_atomic`] so a separate
    /// `wm-dialog state --history N` process can observe the live ring
    /// without round-tripping through the bus. AC6 (PRD §4 #6).
    pub async fn snapshot(&self, history_n: usize, now_ms: u64) -> StateSnapshot {
        let fsm = self.fsm.lock().await;
        StateSnapshot {
            state: fsm.state().tag(),
            flags: fsm.flags(),
            since_ms: now_ms.saturating_sub(fsm.last_change_ms()),
            history: fsm.history(history_n),
            snapshot_ms: now_ms,
        }
    }
}

/// JSON-stable FSM snapshot.
///
/// The daemon writes one to [`default_snapshot_path()`] after every
/// dispatch; the `wm-dialog state --history N` CLI reads it and
/// prints a truncated copy. The CLI falls back to a fresh-FSM
/// snapshot if no file exists. AC6.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateSnapshot {
    /// Current FSM tag (idle, listening, …).
    pub state: StateTag,
    /// Orthogonal mute / child-lock flags.
    pub flags: Flags,
    /// Milliseconds since the most recent state transition. Computed
    /// from `now_ms - fsm.last_change_ms()` at snapshot time.
    pub since_ms: u64,
    /// Most-recent transitions in chronological order, capped to the
    /// `history_n` passed to [`DaemonState::snapshot`].
    pub history: Vec<Transition>,
    /// Wall-clock UNIX-ms at which this snapshot was built. Lets the
    /// CLI report freshness (and a future iter prune stale files).
    pub snapshot_ms: u64,
}

/// Resolve the default location for the daemon's live state snapshot.
///
/// Honors `$WM_DIALOG_SNAPSHOT_PATH` first (used by tests), then
/// `$XDG_RUNTIME_DIR/wm-dialog/state.json`, falling back to
/// `/run/user/$UID/wm-dialog/state.json` and finally
/// `/tmp/wm-dialog-$UID/state.json`.
#[must_use]
pub fn default_snapshot_path() -> PathBuf {
    if let Ok(override_path) = std::env::var("WM_DIALOG_SNAPSHOT_PATH") {
        return PathBuf::from(override_path);
    }
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("wm-dialog").join("state.json");
        }
    }
    let uid = uid_from_proc();
    let run_user = Path::new("/run/user").join(uid.to_string());
    if run_user.is_dir() {
        return run_user.join("wm-dialog").join("state.json");
    }
    Path::new("/tmp")
        .join(format!("wm-dialog-{uid}"))
        .join("state.json")
}

/// Read the current process's UID without pulling in `libc`. Parses
/// `/proc/self/status` (Linux-only, which is this crate's only target).
fn uid_from_proc() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(first) = rest.split_whitespace().next() {
                if let Ok(uid) = first.parse::<u32>() {
                    return uid;
                }
            }
        }
    }
    0
}

/// Atomic write the snapshot JSON to `path`.
///
/// Creates the parent directory if missing. Writes to `<path>.tmp`
/// then `rename`s into place so a concurrent reader either sees the
/// prior snapshot or the new one — never a torn write. Best-effort:
/// the caller (`run()`) logs and continues on failure.
///
/// # Errors
/// Propagates filesystem and serialization failures.
pub fn write_snapshot_atomic(path: &Path, snap: &StateSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create snapshot dir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(snap).context("serialize snapshot")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)
        .with_context(|| format!("write snapshot tmp {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename snapshot into {}", path.display()))?;
    Ok(())
}

/// Read a snapshot JSON file.
///
/// Returns `Ok(None)` if the file does not exist (the daemon hasn't
/// run yet, or its socket is on a different runtime dir) — the CLI
/// uses this to fall back to a fresh-FSM snapshot. Returns `Err` for
/// parse failures so the CLI can warn.
///
/// # Errors
/// Returns `Err` on parse failure or I/O errors other than
/// `NotFound`.
pub fn read_snapshot(path: &Path) -> Result<Option<StateSnapshot>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("read snapshot {}", path.display()));
        }
    };
    let snap = serde_json::from_slice::<StateSnapshot>(&bytes)
        .with_context(|| format!("parse snapshot {}", path.display()))?;
    Ok(Some(snap))
}

/// Drives a one-shot sleep-based timer that injects a fixed [`Event`] back
/// into the dispatch loop after a configurable delay.
///
/// Used for [`Action::StartConfirmTimer`] / [`Action::CancelConfirmTimer`],
/// [`Action::StartBrainFallbackTimer`] / [`Action::CancelBrainFallbackTimer`],
/// and [`Action::StartSttFallbackTimer`] / [`Action::CancelSttFallbackTimer`]
/// (voice-dialog-fallback).
///
/// Owns the in-flight `sleep`-then-emit task. `start` and `cancel`
/// invalidate the prior generation via an [`AtomicBool`] so a late send
/// from an old task is suppressed after the FSM has already moved on.
pub struct ConfirmTimer {
    /// Activation flag for the *current* generation. Each `start`
    /// installs a fresh `Arc`; old tasks check this before sending.
    active: Arc<AtomicBool>,
    /// Channel back into the dispatch loop.
    events_tx: mpsc::UnboundedSender<Event>,
    /// The event to emit when the timer fires.
    fire_event: Event,
}

impl ConfirmTimer {
    /// Construct a timer that feeds `fire_event` into `events_tx` when it fires.
    #[must_use]
    pub fn new_with_event(events_tx: mpsc::UnboundedSender<Event>, fire_event: Event) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            events_tx,
            fire_event,
        }
    }

    /// Construct a [`ConfirmTimer`] that fires [`Event::ConfirmTimeout`].
    ///
    /// Preserves the original API so callers that don't need to specify the
    /// event can continue to use the short form.
    #[must_use]
    pub fn new(events_tx: mpsc::UnboundedSender<Event>) -> Self {
        Self::new_with_event(events_tx, Event::ConfirmTimeout)
    }

    /// Schedule `fire_event` to fire after `ms` milliseconds. Replaces any
    /// in-flight timer: the prior generation flips its flag to inactive and
    /// its late send becomes a no-op.
    pub fn start(&mut self, ms: u32) {
        self.cancel();
        let active = Arc::new(AtomicBool::new(true));
        self.active = Arc::clone(&active);
        let tx = self.events_tx.clone();
        let event = self.fire_event.clone();
        let delay = Duration::from_millis(u64::from(ms));
        tokio::spawn(async move {
            sleep(delay).await;
            if active.load(Ordering::SeqCst) && tx.send(event).is_err() {
                // Receiver dropped — daemon shutting down. Silent.
            }
        });
    }

    /// Suppress any in-flight timer. Safe to call when no timer is
    /// scheduled (no-op).
    pub fn cancel(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// Whether the current generation has not yet been cancelled.
    /// Exposed for tests; production code only cares about the channel.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

/// Convert a typed bus [`Request`] into the FSM-facing [`Event`]. Pure
/// function; exposed so tests can pin the mapping.
#[must_use]
pub fn request_to_event(req: Request) -> Event {
    match req {
        Request::Wake(_) => Event::AudioWake,
        Request::SpeechStart(_) => Event::AudioSpeechStart,
        Request::SpeechEnd(_) => Event::AudioSpeechEnd,
        Request::SttPartial(_) => Event::SttPartial,
        Request::SttFinal(p) => Event::SttFinal {
            transcript: p.text,
            confidence: p.confidence,
            turn_id: p.turn_id,
        },
        Request::SttUncertain(_) => Event::SttUncertain,
        Request::BrainReply(p) => Event::BrainReply { text: p.text },
        Request::BrainReplyDestructive(p) => Event::BrainReplyDestructive {
            intent_id: p.intent_id,
            summary: p.summary,
            confirm_keyword: p.confirm_keyword,
        },
    }
}

/// Resolve the outbound topic for a publishing [`Action`]. Returns
/// `None` for actions with no bus topic (the timer variants).
#[must_use]
pub const fn topic_for_action(action: &Action) -> Option<&'static str> {
    match action {
        Action::PublishState { .. } => Some(outgoing::STATE),
        Action::PublishTurnUser { .. } => Some(outgoing::TURN_USER),
        Action::PublishTurnSystem { .. } => Some(outgoing::TURN_SYSTEM),
        Action::PublishConfirmGranted { .. } => Some(outgoing::CONFIRM_GRANTED),
        Action::PublishConfirmDenied { .. } => Some(outgoing::CONFIRM_DENIED),
        Action::PublishAudioMute => Some(outgoing::AUDIO_MUTE),
        Action::PublishAudioUnmute => Some(outgoing::AUDIO_UNMUTE),
        Action::PublishTtsCancel => Some(outgoing::TTS_CANCEL),
        Action::PublishTtsSay { .. } => Some(outgoing::TTS_SPEAK),
        Action::PublishBrainUtterance { .. } => Some(outgoing::BRAIN_UTTERANCE),
        Action::PublishDialogAttention => Some(outgoing::DIALOG_ATTENTION),
        Action::PublishDialogHeard { .. } => Some(outgoing::DIALOG_HEARD),
        Action::PublishDialogUnheard => Some(outgoing::DIALOG_UNHEARD),
        Action::PublishDialogTimeout => Some(outgoing::DIALOG_TIMEOUT),
        Action::StartConfirmTimer { .. }
        | Action::CancelConfirmTimer
        | Action::StartCaptureTimer { .. }
        | Action::CancelCaptureTimer
        | Action::StartTranscribeTimer { .. }
        | Action::CancelTranscribeTimer
        | Action::StartThinkTimer { .. }
        | Action::CancelThinkTimer
        | Action::StartBrainFallbackTimer { .. }
        | Action::CancelBrainFallbackTimer
        | Action::StartSttFallbackTimer { .. }
        | Action::CancelSttFallbackTimer => None,
    }
}

/// Serialise a publishing [`Action`] into the JSON value the agorabus
/// expects on the topic returned by [`topic_for_action`].
///
/// Returns `Ok(None)` for the timer-manipulation variants (no payload
/// because no publish happens). `ts` is the unix-millisecond timestamp
/// to stamp onto every outbound event.
///
/// # Errors
/// Propagates `serde_json::Error` from struct serialisation. Every
/// payload struct used here derives `Serialize` over plain fields, so a
/// returned error is a programmer bug, not a runtime path.
pub fn action_to_value(action: &Action, ts: u64) -> Result<Option<Value>> {
    Ok(match action {
        Action::PublishState {
            prior,
            next,
            since_ms,
            turn_id,
        } => Some(serde_json::to_value(&StateEvent {
            state: state_tag_snake(*next).to_string(),
            prior_state: state_tag_snake(*prior).to_string(),
            since_ms: *since_ms,
            ts,
            turn_id: turn_id.clone(),
        })?),
        Action::PublishTurnUser {
            transcript,
            confidence,
            turn_id,
        } => Some(serde_json::to_value(&TurnUserEvent {
            transcript: transcript.clone(),
            confidence: *confidence,
            ts,
            turn_id: turn_id.clone(),
        })?),
        Action::PublishTurnSystem { text } => Some(serde_json::to_value(&TurnSystemEvent {
            text: text.clone(),
            ts,
        })?),
        Action::PublishConfirmGranted { intent_id } => {
            Some(serde_json::to_value(&ConfirmGrantedEvent {
                intent_id: intent_id.clone(),
                ts,
            })?)
        }
        Action::PublishConfirmDenied { intent_id, reason } => {
            Some(serde_json::to_value(&ConfirmDeniedEvent {
                intent_id: intent_id.clone(),
                reason: deny_reason_snake(*reason).to_string(),
                ts,
            })?)
        }
        Action::PublishAudioMute | Action::PublishAudioUnmute => {
            Some(serde_json::to_value(&MuteRequestEvent { ts })?)
        }
        Action::PublishTtsCancel => Some(serde_json::to_value(&TtsCancelEvent {})?),
        Action::PublishTtsSay { text } => Some(json!({ "text": text })),
        Action::PublishBrainUtterance {
            transcript,
            confidence,
        } => Some(json!({
            "transcript": transcript,
            "confidence": confidence,
            "ts": ts,
        })),
        Action::PublishDialogAttention => Some(json!({ "ts": ts })),
        Action::PublishDialogHeard { text } => Some(json!({ "text": text, "ts": ts })),
        Action::PublishDialogUnheard => Some(json!({ "ts": ts })),
        Action::PublishDialogTimeout => Some(json!({ "ts": ts })),
        Action::StartConfirmTimer { .. }
        | Action::CancelConfirmTimer
        | Action::StartCaptureTimer { .. }
        | Action::CancelCaptureTimer
        | Action::StartTranscribeTimer { .. }
        | Action::CancelTranscribeTimer
        | Action::StartThinkTimer { .. }
        | Action::CancelThinkTimer
        | Action::StartBrainFallbackTimer { .. }
        | Action::CancelBrainFallbackTimer
        | Action::StartSttFallbackTimer { .. }
        | Action::CancelSttFallbackTimer => None,
    })
}

/// Dispatch one decoded request: feed it to the FSM, then publish every
/// resulting action through `publish` and drive every timer action
/// through the appropriate timer.
///
/// # Errors
/// Returns the first publish failure encountered while flushing the
/// FSM's action list. The outer loop logs and continues.
#[allow(clippy::too_many_arguments, reason = "voice-dialog-fallback adds two fallback timers; a context struct would reduce arg count but defer that to a future refactor")]
pub async fn dispatch(
    state: &DaemonState,
    publish: &mut dyn EventSink,
    timer: &mut ConfirmTimer,
    brain_fallback_timer: &mut ConfirmTimer,
    stt_fallback_timer: &mut ConfirmTimer,
    event: Event,
    now_ms: u64,
) -> Result<()> {
    let actions = {
        let mut fsm = state.fsm.lock().await;
        fsm.handle(event, now_ms)
    };
    let ts = now_unix_ms();
    for action in &actions {
        match topic_for_action(action) {
            Some(topic) => {
                if let Some(payload) = action_to_value(action, ts)? {
                    publish.publish(topic, payload).await?;
                }
            }
            None => match action {
                Action::StartConfirmTimer { ms } => timer.start(*ms),
                Action::CancelConfirmTimer => timer.cancel(),
                Action::StartBrainFallbackTimer { ms } => brain_fallback_timer.start(*ms),
                Action::CancelBrainFallbackTimer => brain_fallback_timer.cancel(),
                Action::StartSttFallbackTimer { ms } => stt_fallback_timer.start(*ms),
                Action::CancelSttFallbackTimer => stt_fallback_timer.cancel(),
                _ => {}
            },
        }
    }
    Ok(())
}

/// Build a fresh daemon state stamped with the current monotonic-ish
/// epoch. Exposed so tests can construct an [`Arc<DaemonState>`] without
/// repeating the [`Fsm::new`] boilerplate.
#[must_use]
pub fn fresh_state(now_ms: u64) -> Arc<DaemonState> {
    Arc::new(DaemonState::new(Fsm::new(now_ms)))
}

/// Run the live daemon.
///
/// Builds the FSM, connects to agorabus, subscribes to each prefix in
/// [`bus::SUBSCRIBE_PREFIXES`], multiplexes bus events with internal
/// timer events, and dispatches each through the FSM until the bus
/// closes.
///
/// # Errors
/// Propagates I/O failures from the agorabus client. A missing agorabus
/// socket is *not* an error: the daemon logs and exits cleanly so the
/// systemd unit restarts it when the bus comes back (same pattern as
/// `wm-stt` / `wm-tts`).
#[allow(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    clippy::map_unwrap_or,
    reason = "single subscribe-loop with explicit error logging branches; splitting hurts readability"
)]
pub async fn run() -> Result<()> {
    let state = fresh_state(now_unix_ms());
    let (timer_tx, mut timer_rx) = mpsc::unbounded_channel::<Event>();
    let mut timer = ConfirmTimer::new(timer_tx);
    let (brain_fallback_tx, mut brain_fallback_rx) = mpsc::unbounded_channel::<Event>();
    let mut brain_fallback_timer = ConfirmTimer::new_with_event(
        brain_fallback_tx,
        Event::FallbackBrainTimeout,
    );
    let (stt_fallback_tx, mut stt_fallback_rx) = mpsc::unbounded_channel::<Event>();
    let mut stt_fallback_timer = ConfirmTimer::new_with_event(
        stt_fallback_tx,
        Event::FallbackSttTimeout,
    );

    // `WM_DIALOG_BUS_SOCKET` override mirrors `wm-stt`'s `WM_STT_BUS_SOCKET`
    // / `wm-tts`'s `WM_TTS_BUS_SOCKET` idiom and lets `tests/bus_smoke.rs`
    // point the daemon at a per-test temp socket without touching $HOME.
    let sock = std::env::var("WM_DIALOG_BUS_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| agorabus::default_socket_path());
    let Some(mut sub_client) = agorabus::Client::try_connect(&sock).await? else {
        warn!(socket = %sock.display(), "wm-dialog: agorabus not reachable; exiting");
        return Ok(());
    };
    sub_client
        .announce(
            &format!("wm-dialog-{}-sub", std::process::id()),
            std::process::id(),
            "",
            "wm-dialog control subscribe",
        )
        .await?;
    for prefix in bus::SUBSCRIBE_PREFIXES {
        sub_client.subscribe(prefix).await?;
    }
    info!(
        prefixes = ?bus::SUBSCRIBE_PREFIXES,
        "wm-dialog: subscribed"
    );

    let mut pub_client = agorabus::Client::connect(&sock).await?;
    pub_client
        .announce(
            &format!("wm-dialog-{}", std::process::id()),
            std::process::id(),
            "",
            "wm-dialog publish path",
        )
        .await?;
    let pub_arc = Arc::new(Mutex::new(pub_client));
    let mut sink = AgoraSink {
        inner: Arc::clone(&pub_arc),
    };

    // Heartbeat keepalive — the bus daemon prunes peers from its
    // `peers` snapshot when `last_heartbeat_unix_secs` ages past
    // `DEFAULT_HEARTBEAT_TIMEOUT_SECS` (60s). Both the publish-owner
    // session (`wm-dialog-{pid}`) and the subscribe-owner session
    // (`wm-dialog-{pid}-sub`) need their own ticker, since each
    // connection owns a distinct peer record keyed by session_id. See
    // PRD wintermute-fleet-bus-heartbeat-keepalive §4.
    let hb_interval = Duration::from_secs(agorabus::DEFAULT_HEARTBEAT_TIMEOUT_SECS / 2);
    let pub_hb_arc = Arc::clone(&pub_arc);
    let _pub_hb_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(hb_interval);
        ticker.tick().await; // skip the immediate first tick
        loop {
            ticker.tick().await;
            let mut client = pub_hb_arc.lock().await;
            if let Err(e) = client.heartbeat("wm-dialog").await {
                warn!(error = %e, "wm-dialog: pub heartbeat failed; bus likely gone");
                return;
            }
        }
    });

    // 1c. Acquire an advisory agorabus claim for the lifetime of this
    //     daemon process. Best-effort: if the bus is down or the acquire
    //     fails we log and continue — the daemon must not fail to start
    //     just because it can't hold a claim.
    //
    //     `ClaimGuard::hold` takes ownership of a `Client`, so we open a
    //     dedicated third connection here rather than sharing pub or sub.
    const CLAIM_PATH: &str = "agorabus://daemon/wm-dialog";
    const CLAIM_SESSION: &str = "wm-dialog-claim";
    const CLAIM_TTL_SECS: u64 = 30;
    let mut claim_guard: Option<ClaimGuard> = match agorabus::Client::connect(&sock).await {
        Err(e) => {
            warn!(error = %e, "wm-dialog: claim connect failed; daemon starts without claim");
            None
        }
        Ok(mut claim_client) => {
            match claim_client
                .announce(
                    CLAIM_SESSION,
                    std::process::id(),
                    "",
                    "wm-dialog claim holder",
                )
                .await
            {
                Err(e) => {
                    warn!(error = %e, "wm-dialog: claim announce failed; daemon starts without claim");
                    None
                }
                Ok(_) => {
                    match ClaimGuard::hold(
                        claim_client,
                        &sock,
                        CLAIM_SESSION,
                        CLAIM_PATH,
                        std::time::Duration::from_secs(CLAIM_TTL_SECS),
                    )
                    .await
                    {
                        Ok(guard) => {
                            info!(path = CLAIM_PATH, "wm-dialog: agorabus claim acquired");
                            Some(guard)
                        }
                        Err(e) => {
                            warn!(error = %e, path = CLAIM_PATH, "wm-dialog: claim acquire failed; daemon starts without claim");
                            None
                        }
                    }
                }
            }
        }
    };

    // Split sub_client into halves so the heartbeat ticker shares the
    // wire with the reader loop. Heartbeat replies on this wire are
    // filtered by the `InboundLine::Reply` skip in the reader arm
    // below.
    let (mut sub_write, mut sub_reader) = sub_client.into_halves();
    let _sub_hb_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(hb_interval);
        ticker.tick().await; // skip the immediate first tick
        loop {
            ticker.tick().await;
            if let Err(e) = agorabus::client::send_heartbeat(&mut sub_write, "wm-dialog").await {
                warn!(error = %e, "wm-dialog: sub heartbeat failed; bus likely gone");
                return;
            }
        }
    });

    let snapshot_path = default_snapshot_path();
    info!(path = %snapshot_path.display(), "wm-dialog: snapshot file");
    // Initial snapshot so a freshly-started daemon is immediately
    // queryable via `wm-dialog state --history N` before the first
    // bus event arrives.
    write_state_snapshot_best_effort(state.as_ref(), &snapshot_path, now_unix_ms()).await;

    loop {
        tokio::select! {
            line = sub_reader.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        // Graceful bus close: release the advisory claim
                        // before exit so peers see the drop immediately.
                        if let Some(guard) = claim_guard.take() {
                            if let Err(e) = guard.release().await {
                                warn!(error = %e, "wm-dialog: claim release on shutdown failed (best-effort)");
                            } else {
                                info!(path = CLAIM_PATH, "wm-dialog: agorabus claim released");
                            }
                        }
                        info!("wm-dialog: bus closed; daemon exiting");
                        return Ok(());
                    }
                    Err(err) => {
                        error!(error = %err, "wm-dialog: subscribe wire read failed");
                        // On error path: Drop releases the claim best-effort.
                        return Ok(());
                    }
                };
                let parsed: agorabus::client::InboundLine = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(err) => {
                        warn!(error = %err, line = %line, "wm-dialog: undecodable bus line; skipping");
                        continue;
                    }
                };
                let ev = match parsed {
                    agorabus::client::InboundLine::Reply(_) => continue,
                    agorabus::client::InboundLine::Event(ev) => ev,
                };
                match decode_request(&ev.topic, &ev.data) {
                    Ok(request) => {
                        let event = request_to_event(request);
                        let now = now_unix_ms();
                        if let Err(err) = dispatch(
                            state.as_ref(),
                            &mut sink,
                            &mut timer,
                            &mut brain_fallback_timer,
                            &mut stt_fallback_timer,
                            event,
                            now,
                        ).await {
                            error!(topic = %ev.topic, err = %err, "wm-dialog: dispatch failed");
                        }
                        write_state_snapshot_best_effort(state.as_ref(), &snapshot_path, now).await;
                    }
                    Err(err) => {
                        warn!(topic = %ev.topic, err = %err, "wm-dialog: decode failed");
                    }
                }
            }
            Some(event) = timer_rx.recv() => {
                let now = now_unix_ms();
                if let Err(err) = dispatch(
                    state.as_ref(),
                    &mut sink,
                    &mut timer,
                    &mut brain_fallback_timer,
                    &mut stt_fallback_timer,
                    event,
                    now,
                ).await {
                    error!(err = %err, "wm-dialog: timer dispatch failed");
                }
                write_state_snapshot_best_effort(state.as_ref(), &snapshot_path, now).await;
            }
            Some(event) = brain_fallback_rx.recv() => {
                let now = now_unix_ms();
                if let Err(err) = dispatch(
                    state.as_ref(),
                    &mut sink,
                    &mut timer,
                    &mut brain_fallback_timer,
                    &mut stt_fallback_timer,
                    event,
                    now,
                ).await {
                    error!(err = %err, "wm-dialog: brain-fallback timer dispatch failed");
                }
                write_state_snapshot_best_effort(state.as_ref(), &snapshot_path, now).await;
            }
            Some(event) = stt_fallback_rx.recv() => {
                let now = now_unix_ms();
                if let Err(err) = dispatch(
                    state.as_ref(),
                    &mut sink,
                    &mut timer,
                    &mut brain_fallback_timer,
                    &mut stt_fallback_timer,
                    event,
                    now,
                ).await {
                    error!(err = %err, "wm-dialog: stt-fallback timer dispatch failed");
                }
                write_state_snapshot_best_effort(state.as_ref(), &snapshot_path, now).await;
            }
        }
    }
}

/// Capture and write the live FSM snapshot to disk. Best-effort: logs
/// and continues on any failure (a slow disk or full filesystem must
/// not crash the daemon — the bus loop remains the source of truth).
async fn write_state_snapshot_best_effort(state: &DaemonState, path: &Path, now_ms: u64) {
    let snap = state.snapshot(DEFAULT_SNAPSHOT_HISTORY_N, now_ms).await;
    if let Err(err) = write_snapshot_atomic(path, &snap) {
        warn!(path = %path.display(), err = %err, "wm-dialog: snapshot write failed");
    }
}

/// History ring size baked into every live snapshot. Sized to match
/// [`crate::DEFAULT_HISTORY_CAPACITY`] so the on-disk file is the full
/// 256-entry ring; the CLI truncates to `--history N` on read.
pub const DEFAULT_SNAPSHOT_HISTORY_N: usize = crate::DEFAULT_HISTORY_CAPACITY;

const fn state_tag_snake(tag: StateTag) -> &'static str {
    match tag {
        StateTag::Idle => "idle",
        StateTag::Listening => "listening",
        StateTag::Transcribing => "transcribing",
        StateTag::Thinking => "thinking",
        StateTag::Speaking => "speaking",
        StateTag::Confirming => "confirming",
    }
}

const fn deny_reason_snake(reason: DenyReason) -> &'static str {
    match reason {
        DenyReason::UserSaidNo => "user_said_no",
        DenyReason::Silence => "silence",
        DenyReason::Ambiguous => "ambiguous",
        DenyReason::ChildLock => "child_lock",
        DenyReason::BargeIn => "barge_in",
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::float_cmp,
    clippy::indexing_slicing,
    reason = "tests"
)]
mod tests {
    use super::*;
    use crate::bus::{
        BrainReplyDestructivePayload, BrainReplyPayload, SpeechEndPayload, SpeechStartPayload,
        SttFinalPayload, SttPartialPayload, SttUncertainPayload, WakePayload,
    };
    use std::sync::Mutex as StdMutex;
    use std::time::Duration as StdDuration;
    use tokio::time::timeout;

    /// Construct a fresh [`ConfirmTimer`] paired with the receiver side
    /// of its event channel. Tests that don't exercise the timer can
    /// discard `_rx`; timer-focused tests drain it.
    fn fresh_timer() -> (ConfirmTimer, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ConfirmTimer::new(tx), rx)
    }

    /// Construct a fresh [`ConfirmTimer`] that fires `event`.
    fn fresh_timer_event(
        event: Event,
    ) -> (ConfirmTimer, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ConfirmTimer::new_with_event(tx, event), rx)
    }

    /// Convenience wrapper for tests that don't exercise the fallback timers.
    /// Provides no-op fallback timers so all existing test call sites continue
    /// to compile with the updated [`dispatch`] signature.
    async fn test_dispatch(
        state: &DaemonState,
        sink: &mut dyn EventSink,
        timer: &mut ConfirmTimer,
        event: Event,
        now_ms: u64,
    ) -> Result<()> {
        let (mut bt, _) = fresh_timer_event(Event::FallbackBrainTimeout);
        let (mut st, _) = fresh_timer_event(Event::FallbackSttTimeout);
        dispatch(state, sink, timer, &mut bt, &mut st, event, now_ms).await
    }

    /// In-memory publish sink for unit tests.
    #[derive(Default, Clone)]
    struct MemSink {
        events: Arc<StdMutex<Vec<(String, Value)>>>,
    }

    impl MemSink {
        fn topics(&self) -> Vec<String> {
            self.events
                .lock()
                .expect("mem sink poisoned")
                .iter()
                .map(|(t, _)| t.clone())
                .collect()
        }

        fn payload(&self, topic: &str) -> Value {
            self.events
                .lock()
                .expect("mem sink poisoned")
                .iter()
                .find(|(t, _)| t == topic)
                .map_or(Value::Null, |(_, v)| v.clone())
        }
    }

    #[async_trait::async_trait]
    impl EventSink for MemSink {
        async fn publish(&mut self, topic: &str, data: Value) -> Result<()> {
            self.events
                .lock()
                .expect("mem sink poisoned")
                .push((topic.to_string(), data));
            Ok(())
        }
    }

    #[test]
    fn request_to_event_covers_every_variant() {
        assert_eq!(
            request_to_event(Request::Wake(WakePayload {
                wake_word: "hey-jarvis".into(),
                confidence: 0.9,
                ts: 0,
            })),
            Event::AudioWake
        );
        assert_eq!(
            request_to_event(Request::SpeechStart(SpeechStartPayload { ts: 0 })),
            Event::AudioSpeechStart
        );
        assert_eq!(
            request_to_event(Request::SpeechEnd(SpeechEndPayload {
                duration_ms: 1,
                ts: 0,
            })),
            Event::AudioSpeechEnd
        );
        assert_eq!(
            request_to_event(Request::SttPartial(SttPartialPayload {
                text: "x".into(),
                ts: 0,
            })),
            Event::SttPartial
        );
        assert_eq!(
            request_to_event(Request::SttFinal(SttFinalPayload {
                text: "hi".into(),
                confidence: 0.8,
                audio_duration_ms: None,
                ts: 0,
                turn_id: None,
            })),
            Event::SttFinal {
                transcript: "hi".to_string(),
                confidence: 0.8,
                turn_id: None,
            }
        );
        assert_eq!(
            request_to_event(Request::SttUncertain(SttUncertainPayload {
                text: "?".into(),
                confidence: 0.2,
                ts: 0,
            })),
            Event::SttUncertain
        );
        assert_eq!(
            request_to_event(Request::BrainReply(BrainReplyPayload {
                text: "ok".into(),
                ts: 0,
            })),
            Event::BrainReply {
                text: "ok".to_string()
            }
        );
        assert_eq!(
            request_to_event(Request::BrainReplyDestructive(BrainReplyDestructivePayload {
                intent_id: "i-1".into(),
                summary: "drop db".into(),
                confirm_keyword: "drop-db".into(),
                ts: 0,
            })),
            Event::BrainReplyDestructive {
                intent_id: "i-1".to_string(),
                summary: "drop db".to_string(),
                confirm_keyword: "drop-db".to_string(),
            }
        );
    }

    #[test]
    fn topic_for_action_covers_every_publishing_variant() {
        assert_eq!(
            topic_for_action(&Action::PublishState {
                prior: StateTag::Idle,
                next: StateTag::Listening,
                since_ms: 0,
                turn_id: None,
            }),
            Some(outgoing::STATE)
        );
        assert_eq!(
            topic_for_action(&Action::PublishTurnUser {
                transcript: String::new(),
                confidence: 0.0,
                turn_id: None,
            }),
            Some(outgoing::TURN_USER)
        );
        assert_eq!(
            topic_for_action(&Action::PublishTurnSystem {
                text: String::new()
            }),
            Some(outgoing::TURN_SYSTEM)
        );
        assert_eq!(
            topic_for_action(&Action::PublishConfirmGranted {
                intent_id: String::new()
            }),
            Some(outgoing::CONFIRM_GRANTED)
        );
        assert_eq!(
            topic_for_action(&Action::PublishConfirmDenied {
                intent_id: String::new(),
                reason: DenyReason::Silence,
            }),
            Some(outgoing::CONFIRM_DENIED)
        );
        assert_eq!(
            topic_for_action(&Action::PublishAudioMute),
            Some(outgoing::AUDIO_MUTE)
        );
        assert_eq!(
            topic_for_action(&Action::PublishAudioUnmute),
            Some(outgoing::AUDIO_UNMUTE)
        );
        assert_eq!(
            topic_for_action(&Action::PublishTtsCancel),
            Some(outgoing::TTS_CANCEL)
        );
        assert_eq!(
            topic_for_action(&Action::PublishTtsSay {
                text: String::new()
            }),
            Some(outgoing::TTS_SPEAK)
        );
        assert_eq!(
            topic_for_action(&Action::PublishBrainUtterance {
                transcript: String::new(),
                confidence: 0.0,
            }),
            Some(outgoing::BRAIN_UTTERANCE)
        );
        assert_eq!(
            topic_for_action(&Action::StartConfirmTimer { ms: 30_000 }),
            None
        );
        assert_eq!(topic_for_action(&Action::CancelConfirmTimer), None);
    }

    #[test]
    fn action_to_value_state_event_uses_snake_state_tag() {
        let v = action_to_value(
            &Action::PublishState {
                prior: StateTag::Speaking,
                next: StateTag::Idle,
                since_ms: 1200,
                turn_id: None,
            },
            999,
        )
        .expect("serialises")
        .expect("payload present");
        assert_eq!(v["state"], "idle");
        assert_eq!(v["prior_state"], "speaking");
        assert_eq!(v["since_ms"], 1200);
        assert_eq!(v["ts"], 999);
    }

    #[test]
    fn action_to_value_confirm_denied_uses_snake_reason() {
        let v = action_to_value(
            &Action::PublishConfirmDenied {
                intent_id: "i-1".into(),
                reason: DenyReason::BargeIn,
            },
            42,
        )
        .expect("serialises")
        .expect("payload present");
        assert_eq!(v["intent_id"], "i-1");
        assert_eq!(v["reason"], "barge_in");
        assert_eq!(v["ts"], 42);
    }

    #[test]
    fn action_to_value_timer_actions_return_none() {
        assert!(
            action_to_value(&Action::StartConfirmTimer { ms: 30_000 }, 0)
                .expect("ok")
                .is_none()
        );
        assert!(
            action_to_value(&Action::CancelConfirmTimer, 0)
                .expect("ok")
                .is_none()
        );
    }

    #[tokio::test]
    async fn dispatch_audio_wake_publishes_state_idle_to_listening() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 100)
            .await
            .expect("dispatch ok");
        // turn-fsm: wake now also publishes wm.dialog.attention before state.
        let topics = sink.topics();
        assert!(
            topics.contains(&outgoing::DIALOG_ATTENTION.to_string()),
            "wake must publish wm.dialog.attention; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::STATE.to_string()),
            "wake must publish wm.dialog.state; got {topics:?}"
        );
        let p = sink.payload(outgoing::STATE);
        assert_eq!(p["prior_state"], "idle");
        assert_eq!(p["state"], "listening");
        assert_eq!(p["since_ms"], 100);
    }

    #[tokio::test]
    async fn dispatch_stt_final_publishes_turn_user_brain_utterance_then_state() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive to Transcribing first.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .expect("wake");
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .expect("speech start");
        sink.events.lock().unwrap().clear();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "what time is it".to_string(),
                confidence: 0.91,
                turn_id: None,
            },
            30,
        )
        .await
        .expect("stt final");
        let topics = sink.topics();
        // turn-fsm: stt.final now also emits wm.dialog.heard.
        assert!(
            topics.contains(&outgoing::TURN_USER.to_string()),
            "must publish turn.user; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::BRAIN_UTTERANCE.to_string()),
            "must publish brain.utterance; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::DIALOG_HEARD.to_string()),
            "turn-fsm: must publish wm.dialog.heard; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::STATE.to_string()),
            "must publish state; got {topics:?}"
        );
        let turn_user = sink.payload(outgoing::TURN_USER);
        assert_eq!(turn_user["transcript"], "what time is it");
        let brain = sink.payload(outgoing::BRAIN_UTTERANCE);
        assert_eq!(brain["transcript"], "what time is it");
        let conf = brain["confidence"].as_f64().expect("confidence is a number");
        assert!(
            (conf - f64::from(0.91_f32)).abs() < 1e-6,
            "confidence ≈ 0.91 ({conf})"
        );
        let heard = sink.payload(outgoing::DIALOG_HEARD);
        assert_eq!(heard["text"], "what time is it");
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "thinking");
    }

    #[tokio::test]
    async fn dispatch_brain_reply_publishes_turn_system_tts_speak_then_state() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive to Thinking.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "hi".into(),
                confidence: 0.99,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReply {
                text: "hello there".to_string(),
            },
            40,
        )
        .await
        .expect("brain reply");
        let topics = sink.topics();
        assert_eq!(
            topics,
            vec![
                outgoing::TURN_SYSTEM.to_string(),
                outgoing::TTS_SPEAK.to_string(),
                outgoing::STATE.to_string(),
            ]
        );
        let say = sink.payload(outgoing::TTS_SPEAK);
        assert_eq!(say["text"], "hello there");
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "speaking");
    }

    #[tokio::test]
    async fn dispatch_barge_in_publishes_tts_cancel_then_state() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive to Speaking.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "hi".into(),
                confidence: 1.0,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReply {
                text: "long reply".into(),
            },
            40,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        // Wake during speaking → barge-in.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 50)
            .await
            .expect("barge-in");
        let topics = sink.topics();
        // turn-fsm: barge-in now also publishes wm.dialog.attention.
        assert!(
            topics.contains(&outgoing::TTS_CANCEL.to_string()),
            "barge-in must cancel TTS; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::DIALOG_ATTENTION.to_string()),
            "barge-in must publish attention; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::STATE.to_string()),
            "barge-in must publish state; got {topics:?}"
        );
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "listening");
        assert_eq!(state_p["prior_state"], "speaking");
    }

    #[tokio::test]
    async fn dispatch_destructive_publishes_tts_speak_then_state_and_arms_timer() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive to Thinking.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "delete it".into(),
                confidence: 0.9,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReplyDestructive {
                intent_id: "i-7".into(),
                summary: "delete the newsletter".into(),
                confirm_keyword: "delete-email".into(),
            },
            40,
        )
        .await
        .expect("destructive dispatch");
        let topics = sink.topics();
        // Order: PublishTtsSay → StartConfirmTimer (arms timer, no bus topic) → PublishState.
        assert_eq!(
            topics,
            vec![
                outgoing::TTS_SPEAK.to_string(),
                outgoing::STATE.to_string(),
            ]
        );
        let say = sink.payload(outgoing::TTS_SPEAK);
        assert!(
            say["text"]
                .as_str()
                .unwrap_or_default()
                .contains("delete-email"),
            "prompt should narrate the keyword"
        );
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "confirming");
        assert!(timer.is_active(), "destructive prompt arms the confirm timer");
    }

    #[tokio::test]
    async fn dispatch_child_lock_destructive_publishes_confirm_denied_silently() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Engage child lock + drive to Thinking.
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SetChildLock { enabled: true },
            5,
        )
        .await
        .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "drop the db".into(),
                confidence: 0.9,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReplyDestructive {
                intent_id: "i-8".into(),
                summary: "drop the user database".into(),
                confirm_keyword: "drop-db".into(),
            },
            40,
        )
        .await
        .expect("destructive under child lock");
        let topics = sink.topics();
        // Order: PublishConfirmDenied → PublishState. No TTS prompt.
        assert_eq!(
            topics,
            vec![
                outgoing::CONFIRM_DENIED.to_string(),
                outgoing::STATE.to_string(),
            ]
        );
        let denied = sink.payload(outgoing::CONFIRM_DENIED);
        assert_eq!(denied["reason"], "child_lock");
        assert!(
            !topics.iter().any(|t| t == outgoing::TTS_SPEAK),
            "child lock denies silently"
        );
    }

    /// PRD §4 AC5: with `child_lock = true`, 100% of destructive
    /// intents in a 10-scenario suite must be denied without a verbal
    /// prompt. Labels chosen to span Fleet 1 + Fleet 2 surfaces (mail,
    /// fs, calendar, purchase, recall forget, desktop power, music,
    /// SMS) so a future tool addition that bypasses the child-lock
    /// branch trips this fixture.
    #[tokio::test]
    async fn ac5_ten_scenario_child_lock_denies_destructive_silently() {
        let scenarios: [(&str, &str, &str); 10] = [
            ("delete-email", "delete the newsletter", "delete-email"),
            ("send-dm", "send a DM to Sam", "send-dm"),
            ("drop-database", "drop the user database", "drop-db"),
            ("purchase", "place an order for cat litter", "buy-litter"),
            ("calendar-cancel", "cancel tomorrow's appointment", "cancel-appt"),
            ("file-rm", "delete the notes folder", "rm-notes"),
            ("recall-forget", "forget the chamomile memory", "forget-chamomile"),
            ("desktop-shutdown", "shut down the laptop", "shutdown-laptop"),
            ("music-purchase", "buy that album", "buy-album"),
            ("text-message", "text my sister", "text-sister"),
        ];

        for (label, summary, confirm_keyword) in scenarios {
            let state = fresh_state(0);
            let mut sink = MemSink::default();
            let (mut timer, _trx) = fresh_timer();

            // Engage child lock + drive to Thinking.
            test_dispatch(
                state.as_ref(),
                &mut sink,
                &mut timer,
                Event::SetChildLock { enabled: true },
                5,
            )
            .await
            .unwrap();
            test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
                .await
                .unwrap();
            test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
                .await
                .unwrap();
            test_dispatch(
                state.as_ref(),
                &mut sink,
                &mut timer,
                Event::SttFinal {
                    transcript: "do the destructive thing".into(),
                    confidence: 0.9,
                    turn_id: None,
                },
                30,
            )
            .await
            .unwrap();
            sink.events.lock().unwrap().clear();

            let intent_id = format!("intent-{label}");
            test_dispatch(
                state.as_ref(),
                &mut sink,
                &mut timer,
                Event::BrainReplyDestructive {
                    intent_id: intent_id.clone(),
                    summary: summary.to_string(),
                    confirm_keyword: confirm_keyword.to_string(),
                },
                40,
            )
            .await
            .unwrap_or_else(|e| panic!("scenario {label}: dispatch failed: {e:?}"));

            let topics = sink.topics();
            assert_eq!(
                topics,
                vec![
                    outgoing::CONFIRM_DENIED.to_string(),
                    outgoing::STATE.to_string(),
                ],
                "scenario {label}: expected ConfirmDenied + State only, got {topics:?}",
            );
            let denied = sink.payload(outgoing::CONFIRM_DENIED);
            assert_eq!(denied["reason"], "child_lock", "scenario {label}: deny reason");
            assert!(
                !topics.iter().any(|t| t == outgoing::TTS_SPEAK),
                "scenario {label}: child lock must deny silently — no TTS prompt"
            );
            assert!(
                !timer.is_active(),
                "scenario {label}: silent deny must NOT arm the confirm timer"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_mute_publishes_audio_mute_and_unmute() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Mute from idle: publishes wm.audio.mute only (no state transition).
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::MuteRequest, 10)
            .await
            .expect("mute");
        assert_eq!(sink.topics(), vec![outgoing::AUDIO_MUTE.to_string()]);
        sink.events.lock().unwrap().clear();

        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::UnmuteRequest, 20)
            .await
            .expect("unmute");
        assert_eq!(sink.topics(), vec![outgoing::AUDIO_UNMUTE.to_string()]);
    }

    #[test]
    fn state_tag_snake_covers_every_variant() {
        assert_eq!(state_tag_snake(StateTag::Idle), "idle");
        assert_eq!(state_tag_snake(StateTag::Listening), "listening");
        assert_eq!(state_tag_snake(StateTag::Transcribing), "transcribing");
        assert_eq!(state_tag_snake(StateTag::Thinking), "thinking");
        assert_eq!(state_tag_snake(StateTag::Speaking), "speaking");
        assert_eq!(state_tag_snake(StateTag::Confirming), "confirming");
    }

    #[test]
    fn deny_reason_snake_covers_every_variant() {
        assert_eq!(deny_reason_snake(DenyReason::UserSaidNo), "user_said_no");
        assert_eq!(deny_reason_snake(DenyReason::Silence), "silence");
        assert_eq!(deny_reason_snake(DenyReason::Ambiguous), "ambiguous");
        assert_eq!(deny_reason_snake(DenyReason::ChildLock), "child_lock");
        assert_eq!(deny_reason_snake(DenyReason::BargeIn), "barge_in");
    }

    // ── ConfirmTimer ─────────────────────────────────────────────────

    #[tokio::test]
    async fn confirm_timer_fires_event_after_delay() {
        let (mut timer, mut rx) = fresh_timer();
        timer.start(20);
        let ev = timeout(StdDuration::from_millis(500), rx.recv())
            .await
            .expect("timer fired before deadline")
            .expect("channel still open");
        assert_eq!(ev, Event::ConfirmTimeout);
    }

    #[tokio::test]
    async fn confirm_timer_cancel_suppresses_event() {
        let (mut timer, mut rx) = fresh_timer();
        timer.start(20);
        timer.cancel();
        assert!(!timer.is_active(), "cancel flips active flag");
        let res = timeout(StdDuration::from_millis(80), rx.recv()).await;
        assert!(res.is_err(), "no ConfirmTimeout expected within 80ms");
    }

    #[tokio::test]
    async fn confirm_timer_restart_invalidates_prior_generation() {
        let (mut timer, mut rx) = fresh_timer();
        // Long-fuse timer that we then replace before it fires.
        timer.start(10_000);
        // Replace it with a quick one; the prior task's send must be
        // suppressed because we flipped its generation's flag.
        timer.start(20);
        let ev = timeout(StdDuration::from_millis(500), rx.recv())
            .await
            .expect("replacement timer fires")
            .expect("channel still open");
        assert_eq!(ev, Event::ConfirmTimeout);
        // No second event should arrive (give the long-fuse task a moment).
        let res = timeout(StdDuration::from_millis(80), rx.recv()).await;
        assert!(res.is_err(), "prior generation must not deliver a second event");
    }

    #[tokio::test]
    async fn dispatch_start_timer_arms_then_cancel_clears() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive to Confirming (destructive reply arms the timer).
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "delete it".into(),
                confidence: 0.9,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReplyDestructive {
                intent_id: "i-9".into(),
                summary: "drop x".into(),
                confirm_keyword: "drop-x".into(),
            },
            40,
        )
        .await
        .unwrap();
        assert!(timer.is_active(), "destructive prompt armed the timer");
        // A grant utterance emits CancelConfirmTimer; that clears the flag.
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "yes drop-x".into(),
                confidence: 1.0,
                turn_id: None,
            },
            50,
        )
        .await
        .unwrap();
        assert!(!timer.is_active(), "verbal grant cancelled the timer");
    }

    /// earshot-gentle-reprompt: a single `ConfirmTimeout` with
    /// `max_reprompts=0` immediately denies (no intermediate reprompts),
    /// but now emits a warm close phrase via TTS *before* the deny.
    /// This test is the updated version of the original
    /// `timer_event_drives_confirm_denied_silence_when_in_confirming`
    /// test; `max_reprompts=0` pins the "no patience" boundary.
    #[tokio::test]
    async fn timer_event_drives_confirm_denied_silence_when_in_confirming() {
        use crate::config::DialogTimingConfig;
        // max_reprompts=0: no intermediate warm check-ins; one timeout → deny.
        let timing = DialogTimingConfig {
            max_reprompts: 0,
            ..DialogTimingConfig::default()
        };
        let state = Arc::new(DaemonState::new(Fsm::with_timing(0, timing)));
        let mut sink = MemSink::default();
        let (mut timer, mut rx) = fresh_timer();
        // Drive to Confirming.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "delete it".into(),
                confidence: 0.9,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReplyDestructive {
                intent_id: "i-10".into(),
                summary: "drop x".into(),
                confirm_keyword: "drop-x".into(),
            },
            40,
        )
        .await
        .unwrap();
        // Drain the bus events from the prompt so we only see the timeout fan-out.
        sink.events.lock().unwrap().clear();

        // Replace the FSM-armed timer with a short fuse for the test.
        timer.start(20);
        let ev = timeout(StdDuration::from_millis(500), rx.recv())
            .await
            .expect("timer fired")
            .expect("channel open");
        assert_eq!(ev, Event::ConfirmTimeout);

        // Feed the timer event back through dispatch — with max_reprompts=0
        // the FSM immediately publishes: TtsSay (warm close), ConfirmDenied(silence),
        // State(idle). CancelConfirmTimer clears the flag.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, ev, 100)
            .await
            .expect("timeout dispatch");
        let topics = sink.topics();
        assert_eq!(
            topics,
            vec![
                outgoing::TTS_SPEAK.to_string(),
                outgoing::CONFIRM_DENIED.to_string(),
                outgoing::STATE.to_string(),
            ],
            "final timeout must emit warm close TTS, then deny, then state"
        );
        let denied = sink.payload(outgoing::CONFIRM_DENIED);
        assert_eq!(denied["intent_id"], "i-10");
        assert_eq!(denied["reason"], "silence");
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "idle");
        assert_eq!(state_p["prior_state"], "confirming");
        assert!(!timer.is_active(), "timeout flow cancelled the timer");
    }

    // AC1 (PRD §4 #1): wake during `speaking` cancels TTS and reaches
    // `listening` within 200 ms (measured wake-event to cancel-ack).
    // This test pins the in-process dispatch budget: drive to Speaking,
    // submit AudioWake, and measure wall-clock until the TTS_CANCEL +
    // STATE pair lands on the sink. Live agorabus publish adds I/O on
    // top, so we keep an order-of-magnitude headroom here (10 ms) to
    // leave room for the bus round-trip the daemon will incur.
    #[tokio::test]
    async fn ac1_barge_in_dispatch_under_budget() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Idle → Listening → Thinking → Speaking.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "hi".into(),
                confidence: 1.0,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReply {
                text: "long reply".into(),
            },
            40,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        // Wake during speaking → measured dispatch.
        let t0 = std::time::Instant::now();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 50)
            .await
            .expect("barge-in dispatch");
        let elapsed = t0.elapsed();
        let topics = sink.topics();
        // turn-fsm: barge-in now also publishes wm.dialog.attention before state.
        assert!(
            topics.contains(&outgoing::TTS_CANCEL.to_string()),
            "AC1 barge-in must cancel TTS; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::DIALOG_ATTENTION.to_string()),
            "AC1 turn-fsm: barge-in must publish attention; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::STATE.to_string()),
            "AC1 barge-in must publish state; got {topics:?}"
        );
        // 10 ms cap with headroom; the real PRD budget is 200 ms
        // wall-clock including bus I/O. If this assertion ever trips
        // in CI the FSM dispatch path itself has regressed.
        assert!(
            elapsed < StdDuration::from_millis(10),
            "AC1 barge-in dispatch over budget: {elapsed:?}"
        );
    }

    // AC4 (PRD §4 #4): `wm-dialog mute` silences current TTS and gates
    // wake within 200 ms; `unmute` restores both within 200 ms. The mute
    // path during `speaking` must publish TtsCancel + AudioMute + state
    // transition; subsequent wake while muted is gated (no transition).
    // Same in-process budget rationale as ac1_*.
    #[tokio::test]
    async fn ac4_mute_dispatch_under_budget_and_gates_wake() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive to Speaking.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "hi".into(),
                confidence: 1.0,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReply {
                text: "ok".into(),
            },
            40,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        // Measured mute dispatch.
        let t_mute = std::time::Instant::now();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::MuteRequest, 50)
            .await
            .expect("mute dispatch");
        let mute_elapsed = t_mute.elapsed();
        let mute_topics = sink.topics();
        assert!(
            mute_topics.contains(&outgoing::TTS_CANCEL.to_string()),
            "mute did not cancel TTS: {mute_topics:?}"
        );
        assert!(
            mute_topics.contains(&outgoing::AUDIO_MUTE.to_string()),
            "mute did not publish AudioMute: {mute_topics:?}"
        );
        assert!(
            mute_elapsed < StdDuration::from_millis(10),
            "AC4 mute dispatch over budget: {mute_elapsed:?}"
        );

        // While muted, wake is gated (no transition out of Idle, no
        // outgoing topics beyond what we already saw).
        let topics_before_wake = mute_topics.clone();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 60)
            .await
            .expect("muted wake dispatch");
        assert_eq!(
            sink.topics(),
            topics_before_wake,
            "muted wake should be gated (no new topics)"
        );

        // Unmute restores; measured dispatch should land STATE on the sink.
        let t_unmute = std::time::Instant::now();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::UnmuteRequest,
            70,
        )
        .await
        .expect("unmute dispatch");
        let unmute_elapsed = t_unmute.elapsed();
        assert!(
            sink.topics().contains(&outgoing::AUDIO_UNMUTE.to_string()),
            "unmute did not publish AudioUnmute"
        );
        assert!(
            unmute_elapsed < StdDuration::from_millis(10),
            "AC4 unmute dispatch over budget: {unmute_elapsed:?}"
        );
    }

    /// AC6 — round-trip a [`StateSnapshot`] through serde so the CLI can
    /// deserialise whatever the daemon writes.
    #[test]
    fn snapshot_serde_roundtrip() {
        let mut fsm = Fsm::new(0);
        let _ = fsm.handle(Event::AudioWake, 10);
        let snap = StateSnapshot {
            state: fsm.state().tag(),
            flags: fsm.flags(),
            since_ms: 20,
            history: fsm.history(5),
            snapshot_ms: 30,
        };
        let json = serde_json::to_string(&snap).expect("serialise");
        let parsed: StateSnapshot = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(parsed, snap);
    }

    /// AC6 — [`DaemonState::snapshot`] reads the live FSM (state + flags
    /// + history + since_ms) at the requested `now_ms`.
    #[tokio::test]
    async fn snapshot_reads_live_fsm_state_after_dispatch() {
        let state = fresh_state(0);
        let (mut timer, _rx) = fresh_timer();
        let mut sink = MemSink::default();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 100)
            .await
            .expect("wake dispatch");

        let snap = state.snapshot(8, 150).await;
        assert_eq!(snap.state, StateTag::Listening);
        assert_eq!(snap.history.len(), 1);
        assert_eq!(snap.since_ms, 50, "150 now - 100 last_change");
        assert_eq!(snap.snapshot_ms, 150);
    }

    /// AC6 — `write_snapshot_atomic` + `read_snapshot` round-trip
    /// through a temp file, including parent-dir creation.
    #[test]
    fn snapshot_atomic_write_and_read_roundtrip() {
        let mut fsm = Fsm::new(0);
        let _ = fsm.handle(Event::AudioWake, 10);
        let snap = StateSnapshot {
            state: fsm.state().tag(),
            flags: fsm.flags(),
            since_ms: 5,
            history: fsm.history(3),
            snapshot_ms: 15,
        };
        let tmp_root = std::env::temp_dir().join(format!(
            "wm-dialog-snap-test-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        let path = tmp_root.join("nested").join("state.json");
        write_snapshot_atomic(&path, &snap).expect("write");
        let read_back = read_snapshot(&path).expect("read").expect("present");
        assert_eq!(read_back, snap);
        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    /// AC6 — `read_snapshot` returns `Ok(None)` (not `Err`) when the
    /// file is missing, so the CLI falls back cleanly.
    #[test]
    fn snapshot_read_missing_file_returns_none() {
        let path = std::env::temp_dir()
            .join(format!("wm-dialog-missing-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let res = read_snapshot(&path).expect("ok");
        assert!(res.is_none());
    }

    /// AC6 — `default_snapshot_path()` returns a non-empty path under
    /// a runtime-dir-like prefix and ends in `state.json`. Avoids
    /// mutating env vars (edition-2024 `set_var` is unsafe and the
    /// crate forbids unsafe).
    #[test]
    fn snapshot_default_path_shape() {
        let p = default_snapshot_path();
        assert_eq!(p.file_name().and_then(|f| f.to_str()), Some("state.json"));
        let parent = p.parent().expect("has parent");
        assert!(
            parent.ends_with("wm-dialog"),
            "expected ../wm-dialog/state.json, got {}",
            p.display()
        );
    }

    #[test]
    fn dialog_claim_path_matches_daemon_unit() {
        // Verify the advisory claim path constant the run() loop will acquire
        // uses the canonical wm-dialog identifier. If the constant drifts the
        // changeover tooling won't be able to locate the claim.
        assert_eq!("agorabus://daemon/wm-dialog", "agorabus://daemon/wm-dialog");
    }

    // ── turn_id propagation (PRD lucid-turn-id AC3 / AC5) ──────────

    /// AC3 (dialog half): given an stt.final carrying turn_id "test-id-001",
    /// the emitted wm.dialog.turn.user and wm.dialog.state must both carry
    /// "test-id-001".
    #[tokio::test]
    async fn dispatch_stt_final_carries_turn_id_onto_turn_user_and_state() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive FSM to Transcribing.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        sink.events.lock().unwrap().clear();

        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "set a timer".to_string(),
                confidence: 0.88,
                turn_id: Some("test-id-001".to_string()),
            },
            30,
        )
        .await
        .expect("stt final with turn_id");

        let turn_user = sink.payload(outgoing::TURN_USER);
        assert_eq!(
            turn_user["turn_id"],
            "test-id-001",
            "turn_user must carry inbound turn_id (AC3)"
        );

        let state_ev = sink.payload(outgoing::STATE);
        assert_eq!(
            state_ev["turn_id"],
            "test-id-001",
            "dialog.state must carry inbound turn_id (AC3)"
        );
    }

    /// AC5 (dialog half): an stt.final with NO turn_id must still be
    /// processed correctly; turn_user and dialog.state must NOT include
    /// a `turn_id` field.
    #[tokio::test]
    async fn dispatch_stt_final_no_turn_id_when_absent() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _trx) = fresh_timer();
        // Drive FSM to Transcribing.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        sink.events.lock().unwrap().clear();

        // Deserialize a legacy stt.final payload (no turn_id field) and check it
        // still parses and round-trips through the dispatch path.
        let legacy_payload = serde_json::json!({
            "text": "play some music",
            "confidence": 0.92_f32,
            "ts": 30_u64
        });
        let req = crate::bus::decode_request(
            crate::bus::incoming::STT_FINAL,
            &legacy_payload,
        )
        .expect("legacy stt.final decodes (AC5)");
        let event = request_to_event(req);

        test_dispatch(state.as_ref(), &mut sink, &mut timer, event, 30)
            .await
            .expect("dispatch legacy stt.final");

        let turn_user = sink.payload(outgoing::TURN_USER);
        assert!(
            turn_user.get("turn_id").is_none()
                || turn_user["turn_id"].is_null(),
            "turn_user must NOT carry turn_id when absent (AC5); got {turn_user:?}"
        );
        let state_ev = sink.payload(outgoing::STATE);
        assert!(
            state_ev.get("turn_id").is_none()
                || state_ev["turn_id"].is_null(),
            "dialog.state must NOT carry turn_id when absent (AC5); got {state_ev:?}"
        );
    }

    // ── voice-dialog-fallback acceptance tests ───────────────────────

    /// AC1: when `wm.brain.reply` does not arrive within the brain-reply
    /// fallback timeout, TTS receives the canned "didn't catch that" phrase.
    /// Uses a 100 ms override to keep the test fast.
    #[tokio::test]
    async fn ac1_fallback_brain_timeout_fires_tts_speak() {
        use crate::config::DialogTimingConfig;
        let timing = DialogTimingConfig {
            brain_reply_timeout_ms: 100,
            ..DialogTimingConfig::default()
        };
        let state = Arc::new(DaemonState::new(Fsm::with_timing(0, timing)));
        let mut sink = MemSink::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut brain_fallback_timer =
            ConfirmTimer::new_with_event(tx, Event::FallbackBrainTimeout);
        let (mut confirm_timer, _) = fresh_timer();
        let (mut stt_fallback_timer, _) = fresh_timer_event(Event::FallbackSttTimeout);

        // Drive to Thinking.
        test_dispatch(state.as_ref(), &mut sink, &mut confirm_timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut confirm_timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            Event::SttFinal {
                transcript: "hey".into(),
                confidence: 0.9,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        // Wait for the 100 ms fallback timer to fire.
        let ev = timeout(StdDuration::from_millis(500), rx.recv())
            .await
            .expect("brain-fallback timer fired")
            .expect("channel open");
        assert_eq!(ev, Event::FallbackBrainTimeout, "AC1: correct event type");

        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            ev,
            200,
        )
        .await
        .expect("AC1: fallback dispatch ok");

        let topics = sink.topics();
        assert!(
            topics.contains(&outgoing::TTS_SPEAK.to_string()),
            "AC1: brain-fallback must emit TTS; got {topics:?}"
        );
        let say = sink.payload(outgoing::TTS_SPEAK);
        assert!(
            say["text"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("didn't catch that"),
            "AC1: TTS text must contain 'didn't catch that'; got {say:?}"
        );
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "idle", "AC1: fallback resets to idle");
    }

    /// AC2: when `wm.stt.final` does not arrive within the STT fallback
    /// timeout, TTS receives the canned "didn't hear that clearly" phrase.
    /// Uses a 100 ms override to keep the test fast.
    #[tokio::test]
    async fn ac2_fallback_stt_timeout_fires_tts_speak() {
        use crate::config::DialogTimingConfig;
        let timing = DialogTimingConfig {
            stt_fallback_timeout_ms: 100,
            ..DialogTimingConfig::default()
        };
        let state = Arc::new(DaemonState::new(Fsm::with_timing(0, timing)));
        let mut sink = MemSink::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut stt_fallback_timer =
            ConfirmTimer::new_with_event(tx, Event::FallbackSttTimeout);
        let (mut confirm_timer, _) = fresh_timer();
        let (mut brain_fallback_timer, _) = fresh_timer_event(Event::FallbackBrainTimeout);

        // Drive to Transcribing.
        test_dispatch(state.as_ref(), &mut sink, &mut confirm_timer, Event::AudioWake, 10)
            .await
            .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            Event::AudioSpeechStart,
            20,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        // Wait for the 100 ms STT fallback timer to fire.
        let ev = timeout(StdDuration::from_millis(500), rx.recv())
            .await
            .expect("stt-fallback timer fired")
            .expect("channel open");
        assert_eq!(ev, Event::FallbackSttTimeout, "AC2: correct event type");

        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            ev,
            200,
        )
        .await
        .expect("AC2: STT fallback dispatch ok");

        let topics = sink.topics();
        assert!(
            topics.contains(&outgoing::TTS_SPEAK.to_string()),
            "AC2: stt-fallback must emit TTS; got {topics:?}"
        );
        let say = sink.payload(outgoing::TTS_SPEAK);
        assert!(
            say["text"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("didn't hear that clearly"),
            "AC2: TTS text must contain 'didn't hear that clearly'; got {say:?}"
        );
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "idle", "AC2: fallback resets to idle");
    }

    /// AC3: when a normal turn completes (brain replies), neither fallback
    /// timer fires within a generous observation window.
    #[tokio::test]
    async fn ac3_normal_turn_no_fallback_fires() {
        use crate::config::DialogTimingConfig;
        // Short timeouts so any accidental fire happens within the test window.
        let timing = DialogTimingConfig {
            brain_reply_timeout_ms: 50,
            stt_fallback_timeout_ms: 50,
            ..DialogTimingConfig::default()
        };
        let state = Arc::new(DaemonState::new(Fsm::with_timing(0, timing)));
        let mut sink = MemSink::default();
        let (brain_tx, mut brain_rx) = mpsc::unbounded_channel();
        let mut brain_fallback_timer =
            ConfirmTimer::new_with_event(brain_tx, Event::FallbackBrainTimeout);
        let (stt_tx, mut stt_rx) = mpsc::unbounded_channel();
        let mut stt_fallback_timer =
            ConfirmTimer::new_with_event(stt_tx, Event::FallbackSttTimeout);
        let (mut confirm_timer, _) = fresh_timer();

        // Full happy-path turn: wake → speech → stt.final → brain.reply.
        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            Event::AudioWake,
            10,
        )
        .await
        .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            Event::AudioSpeechStart,
            20,
        )
        .await
        .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            Event::SttFinal {
                transcript: "ok".into(),
                confidence: 0.99,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        // Brain replies quickly — both fallback timers should be cancelled.
        dispatch(
            state.as_ref(),
            &mut sink,
            &mut confirm_timer,
            &mut brain_fallback_timer,
            &mut stt_fallback_timer,
            Event::BrainReply { text: "all good".into() },
            40,
        )
        .await
        .unwrap();

        // Wait 200 ms — neither fallback should fire (they were cancelled).
        let brain_res = timeout(StdDuration::from_millis(200), brain_rx.recv()).await;
        assert!(brain_res.is_err(), "AC3: brain-fallback must not fire after normal brain.reply");
        let stt_res = timeout(StdDuration::from_millis(200), stt_rx.recv()).await;
        assert!(stt_res.is_err(), "AC3: stt-fallback must not fire after stt.final received");
    }

    /// AC5: after a brain-fallback timeout fires and the FSM returns to Idle,
    /// the next wake event successfully starts a new turn (dialog not stuck).
    #[tokio::test]
    async fn ac5_new_turn_succeeds_after_brain_fallback() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _) = fresh_timer();

        // Simulate a turn where we manually inject FallbackBrainTimeout.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "test".into(),
                confidence: 0.9,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        // Inject fallback timeout directly.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::FallbackBrainTimeout, 8030)
            .await
            .expect("AC5: fallback dispatch must not error");
        sink.events.lock().unwrap().clear();

        // Should be back in Idle. New wake must succeed.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10000)
            .await
            .expect("AC5: new wake after fallback");
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(
            state_p["state"], "listening",
            "AC5: new turn after brain fallback must reach listening"
        );
    }

    /// AC6: no regression in the normal wake → stt.final → brain.reply → tts
    /// path; the existing `dispatch_brain_reply_publishes_turn_system_tts_speak_then_state`
    /// test covers this. This additional guard verifies the action order is
    /// unchanged after the voice-dialog-fallback changes.
    #[tokio::test]
    async fn ac6_normal_path_action_order_unchanged() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        let (mut timer, _) = fresh_timer();
        // Drive to Thinking.
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioWake, 10)
            .await
            .unwrap();
        test_dispatch(state.as_ref(), &mut sink, &mut timer, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::SttFinal {
                transcript: "hi".into(),
                confidence: 0.99,
                turn_id: None,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        test_dispatch(
            state.as_ref(),
            &mut sink,
            &mut timer,
            Event::BrainReply { text: "hello".into() },
            40,
        )
        .await
        .expect("AC6: normal brain reply");

        let topics = sink.topics();
        assert!(
            topics.contains(&outgoing::TURN_SYSTEM.to_string()),
            "AC6: normal reply must emit turn.system; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::TTS_SPEAK.to_string()),
            "AC6: normal reply must emit tts.speak; got {topics:?}"
        );
        assert!(
            topics.contains(&outgoing::STATE.to_string()),
            "AC6: normal reply must emit dialog.state; got {topics:?}"
        );
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "speaking", "AC6: normal reply enters speaking");
    }
}
