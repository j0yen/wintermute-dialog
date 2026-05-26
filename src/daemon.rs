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
//! iter-5 scope is wiring the subscribe + publish path only. The
//! confirm-timeout timer subsystem ([`crate::Action::StartConfirmTimer`]
//! / [`crate::Action::CancelConfirmTimer`]) is logged but not yet
//! driven — without it the FSM stays in `Confirming` indefinitely on
//! silence. That wiring lands in iter-6 alongside the
//! `Event::ConfirmTimeout` re-entry path.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::action::DenyReason;
use crate::bus::{
    self, ConfirmDeniedEvent, ConfirmGrantedEvent, MuteRequestEvent, Request, StateEvent,
    TtsCancelEvent, TurnSystemEvent, TurnUserEvent, decode_request, now_unix_ms, outgoing,
};
use crate::state::StateTag;
use crate::{Action, Event, Fsm};

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
pub struct AgoraSink {
    /// The underlying agorabus publisher client.
    pub inner: agorabus::Client,
}

#[async_trait::async_trait]
impl EventSink for AgoraSink {
    async fn publish(&mut self, topic: &str, data: Value) -> Result<()> {
        let reply = self.inner.publish(topic, data).await?;
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
        Action::StartConfirmTimer { .. } | Action::CancelConfirmTimer => None,
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
        } => Some(serde_json::to_value(&StateEvent {
            state: state_tag_snake(*next).to_string(),
            prior_state: state_tag_snake(*prior).to_string(),
            since_ms: *since_ms,
            ts,
        })?),
        Action::PublishTurnUser {
            transcript,
            confidence,
        } => Some(serde_json::to_value(&TurnUserEvent {
            transcript: transcript.clone(),
            confidence: *confidence,
            ts,
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
        Action::StartConfirmTimer { .. } | Action::CancelConfirmTimer => None,
    })
}

/// Dispatch one decoded request: feed it to the FSM, then publish every
/// resulting action through `publish`.
///
/// # Errors
/// Returns the first publish failure encountered while flushing the
/// FSM's action list. The outer loop logs and continues.
pub async fn dispatch(
    state: &DaemonState,
    publish: &mut dyn EventSink,
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
            None => {
                // Timer-manipulation variant. iter-6 wires the scheduler;
                // for now we record the intent and continue.
                debug!(
                    ?action,
                    "wm-dialog: confirm-timer action not yet driven (iter-6)"
                );
            }
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

/// Run the live daemon: build the FSM, connect to agorabus, subscribe to
/// each prefix in [`bus::SUBSCRIBE_PREFIXES`], dispatch each event until
/// the bus closes.
///
/// # Errors
/// Propagates I/O failures from the agorabus client. A missing agorabus
/// socket is *not* an error: the daemon logs and exits cleanly so the
/// systemd unit restarts it when the bus comes back (same pattern as
/// `wm-stt` / `wm-tts`).
pub async fn run() -> Result<()> {
    let state = fresh_state(now_unix_ms());

    let sock = agorabus::default_socket_path();
    let Some(mut sub_client) = agorabus::Client::try_connect(&sock).await? else {
        warn!(socket = %sock.display(), "wm-dialog: agorabus not reachable; exiting");
        return Ok(());
    };
    for prefix in bus::SUBSCRIBE_PREFIXES {
        sub_client.subscribe(prefix).await?;
    }
    info!(
        prefixes = ?bus::SUBSCRIBE_PREFIXES,
        "wm-dialog: subscribed"
    );

    let pub_client = agorabus::Client::connect(&sock).await?;
    let mut sink = AgoraSink { inner: pub_client };

    while let Some(ev) = sub_client.next_event().await? {
        match decode_request(&ev.topic, &ev.data) {
            Ok(req) => {
                let event = request_to_event(req);
                let now = now_unix_ms();
                if let Err(err) = dispatch(state.as_ref(), &mut sink, event, now).await {
                    error!(topic = %ev.topic, err = %err, "wm-dialog: dispatch failed");
                }
            }
            Err(err) => {
                warn!(topic = %ev.topic, err = %err, "wm-dialog: decode failed");
            }
        }
    }
    info!("wm-dialog: bus closed; daemon exiting");
    Ok(())
}

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
            })),
            Event::SttFinal {
                transcript: "hi".to_string(),
                confidence: 0.8,
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
            }),
            Some(outgoing::STATE)
        );
        assert_eq!(
            topic_for_action(&Action::PublishTurnUser {
                transcript: String::new(),
                confidence: 0.0,
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
        dispatch(state.as_ref(), &mut sink, Event::AudioWake, 100)
            .await
            .expect("dispatch ok");
        assert_eq!(sink.topics(), vec![outgoing::STATE.to_string()]);
        let p = sink.payload(outgoing::STATE);
        assert_eq!(p["prior_state"], "idle");
        assert_eq!(p["state"], "listening");
        assert_eq!(p["since_ms"], 100);
    }

    #[tokio::test]
    async fn dispatch_stt_final_publishes_turn_user_brain_utterance_then_state() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        // Drive to Transcribing first.
        dispatch(state.as_ref(), &mut sink, Event::AudioWake, 10)
            .await
            .expect("wake");
        dispatch(state.as_ref(), &mut sink, Event::AudioSpeechStart, 20)
            .await
            .expect("speech start");
        sink.events.lock().unwrap().clear();
        dispatch(
            state.as_ref(),
            &mut sink,
            Event::SttFinal {
                transcript: "what time is it".to_string(),
                confidence: 0.91,
            },
            30,
        )
        .await
        .expect("stt final");
        let topics = sink.topics();
        assert_eq!(
            topics,
            vec![
                outgoing::TURN_USER.to_string(),
                outgoing::BRAIN_UTTERANCE.to_string(),
                outgoing::STATE.to_string(),
            ]
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
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "thinking");
    }

    #[tokio::test]
    async fn dispatch_brain_reply_publishes_turn_system_tts_speak_then_state() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        // Drive to Thinking.
        dispatch(state.as_ref(), &mut sink, Event::AudioWake, 10)
            .await
            .unwrap();
        dispatch(state.as_ref(), &mut sink, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            Event::SttFinal {
                transcript: "hi".into(),
                confidence: 0.99,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        dispatch(
            state.as_ref(),
            &mut sink,
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
        // Drive to Speaking.
        dispatch(state.as_ref(), &mut sink, Event::AudioWake, 10)
            .await
            .unwrap();
        dispatch(state.as_ref(), &mut sink, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            Event::SttFinal {
                transcript: "hi".into(),
                confidence: 1.0,
            },
            30,
        )
        .await
        .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            Event::BrainReply {
                text: "long reply".into(),
            },
            40,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        // Wake during speaking → barge-in.
        dispatch(state.as_ref(), &mut sink, Event::AudioWake, 50)
            .await
            .expect("barge-in");
        let topics = sink.topics();
        assert_eq!(
            topics,
            vec![
                outgoing::TTS_CANCEL.to_string(),
                outgoing::STATE.to_string(),
            ]
        );
        let state_p = sink.payload(outgoing::STATE);
        assert_eq!(state_p["state"], "listening");
        assert_eq!(state_p["prior_state"], "speaking");
    }

    #[tokio::test]
    async fn dispatch_destructive_publishes_tts_speak_then_state_and_swallows_timer_action() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        // Drive to Thinking.
        dispatch(state.as_ref(), &mut sink, Event::AudioWake, 10)
            .await
            .unwrap();
        dispatch(state.as_ref(), &mut sink, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            Event::SttFinal {
                transcript: "delete it".into(),
                confidence: 0.9,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        dispatch(
            state.as_ref(),
            &mut sink,
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
        // Order: PublishTtsSay → StartConfirmTimer (silently dropped) → PublishState.
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
    }

    #[tokio::test]
    async fn dispatch_child_lock_destructive_publishes_confirm_denied_silently() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        // Engage child lock + drive to Thinking.
        dispatch(
            state.as_ref(),
            &mut sink,
            Event::SetChildLock { enabled: true },
            5,
        )
        .await
        .unwrap();
        dispatch(state.as_ref(), &mut sink, Event::AudioWake, 10)
            .await
            .unwrap();
        dispatch(state.as_ref(), &mut sink, Event::AudioSpeechStart, 20)
            .await
            .unwrap();
        dispatch(
            state.as_ref(),
            &mut sink,
            Event::SttFinal {
                transcript: "drop the db".into(),
                confidence: 0.9,
            },
            30,
        )
        .await
        .unwrap();
        sink.events.lock().unwrap().clear();

        dispatch(
            state.as_ref(),
            &mut sink,
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

    #[tokio::test]
    async fn dispatch_mute_publishes_audio_mute_and_unmute() {
        let state = fresh_state(0);
        let mut sink = MemSink::default();
        // Mute from idle: publishes wm.audio.mute only (no state transition).
        dispatch(state.as_ref(), &mut sink, Event::MuteRequest, 10)
            .await
            .expect("mute");
        assert_eq!(sink.topics(), vec![outgoing::AUDIO_MUTE.to_string()]);
        sink.events.lock().unwrap().clear();

        dispatch(state.as_ref(), &mut sink, Event::UnmuteRequest, 20)
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
}
