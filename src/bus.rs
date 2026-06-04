//! Agorabus topic + payload schema for `wm-dialog`.
//!
//! Three subscribe prefixes ([`SUBSCRIBE_PREFIXES`]) — `wm.audio.`,
//! `wm.stt.`, and `wm.brain.` — capture every topic listed in
//! `PRD-wintermute-dialog.md` §2.2 subscribed-table. The live subscribe
//! loop (iter-5) calls [`agorabus::Client::subscribe`] once per prefix
//! and routes each [`agorabus::ServerEvent`] through [`decode_request`].
//!
//! Outbound publishes use the constants in [`outgoing`]: the six
//! `wm.dialog.*` topics from the PRD published-table plus
//! [`outgoing::TTS_CANCEL`] (§2.3 barge-in, fired against the
//! `wm.tts.` namespace owned by [`wintermute-tts`]).
//!
//! Payload shapes match the producer crates' wire formats:
//! [`wintermute-audio`](../../../wintermute-audio/) for `wm.audio.*` and
//! [`wintermute-stt`](../../../wintermute-stt/) for `wm.stt.*`. We
//! intentionally don't depend on those crates here — each fleet crate
//! owns its own bus view so test isolation stays clean.

use serde::{Deserialize, Serialize};

/// Topics the daemon subscribes to — the EXACT set it handles, not broad
/// prefixes. The old `wm.audio.` prefix also matched the high-volume
/// `wm.audio.speech.chunk` PCM stream (which dialog doesn't decode), flooding
/// the single-consumer loop with thousands of decode-failed warnings and
/// delaying real wake/speech.end events. `wm.brain.reply` also covers
/// `wm.brain.reply.destructive` (prefix match).
pub const SUBSCRIBE_PREFIXES: [&str; 7] = [
    "wm.audio.wake",
    "wm.audio.speech.start",
    "wm.audio.speech.end",
    "wm.stt.partial",
    "wm.stt.final",
    "wm.stt.uncertain",
    "wm.brain.reply",
];

/// Incoming topics handled by the daemon (PRD §2.2 subscribed-table).
pub mod incoming {
    /// Wake-word detection (`wintermute-audio`).
    pub const WAKE: &str = "wm.audio.wake";
    /// Rising edge of an utterance (`wintermute-audio`).
    pub const SPEECH_START: &str = "wm.audio.speech.start";
    /// Falling edge of an utterance (`wintermute-audio`).
    pub const SPEECH_END: &str = "wm.audio.speech.end";
    /// Partial transcript (`wintermute-stt`); informational.
    pub const STT_PARTIAL: &str = "wm.stt.partial";
    /// Finalised transcript (`wintermute-stt`); forwarded to brain.
    pub const STT_FINAL: &str = "wm.stt.final";
    /// Low-confidence transcript (`wintermute-stt`); triggers a re-prompt.
    pub const STT_UNCERTAIN: &str = "wm.stt.uncertain";
    /// Plain assistant reply (`wintermute-brain`); routes to wm-tts.
    pub const BRAIN_REPLY: &str = "wm.brain.reply";
    /// Destructive intent reply (`wintermute-brain`); triggers verbal confirm.
    pub const BRAIN_REPLY_DESTRUCTIVE: &str = "wm.brain.reply.destructive";
}

/// Outgoing topics published by the daemon (PRD §2.2 published-table
/// plus §2.3 barge-in cancel).
pub mod outgoing {
    /// Conversational-state snapshot. Fires on every transition.
    pub const STATE: &str = "wm.dialog.state";
    /// Transcribed user turn forwarded to `wintermute-brain`.
    pub const TURN_USER: &str = "wm.dialog.turn.user";
    /// System (assistant) turn sent to `wintermute-tts`.
    pub const TURN_SYSTEM: &str = "wm.dialog.turn.system";
    /// Destructive intent granted by the user.
    pub const CONFIRM_GRANTED: &str = "wm.dialog.confirm.granted";
    /// Destructive intent denied (explicit, timeout, or child-lock).
    pub const CONFIRM_DENIED: &str = "wm.dialog.confirm.denied";
    /// User asked to mute (downstream: `wm.audio.mute`).
    pub const MUTE_REQUEST: &str = "wm.dialog.mute_request";
    /// User asked to unmute.
    pub const UNMUTE_REQUEST: &str = "wm.dialog.unmute_request";
    /// Barge-in cancel into `wintermute-tts` (PRD §2.3).
    pub const TTS_CANCEL: &str = "wm.tts.cancel";
    /// Render request into `wintermute-tts` for a brain reply or confirm
    /// prompt (matches `wintermute-tts::incoming::SPEAK`).
    pub const TTS_SPEAK: &str = "wm.tts.speak";
    /// Forward the finalised transcript into `wintermute-brain` (PRD §2.2
    /// `wm.stt.final` row — "forward to brain (wm.brain.utterance)").
    pub const BRAIN_UTTERANCE: &str = "wm.brain.utterance";
    /// Mute the audio capture layer (matches `wintermute-audio::Topics::MUTE`).
    pub const AUDIO_MUTE: &str = "wm.audio.mute";
    /// Release the audio capture mute gate (matches `wintermute-audio::Topics::UNMUTE`).
    pub const AUDIO_UNMUTE: &str = "wm.audio.unmute";
    /// Wake detected; FSM is now armed to capture speech. UI hook.
    pub const DIALOG_ATTENTION: &str = "wm.dialog.attention";
    /// STT final forwarded to brain; emitted with transcript text.
    pub const DIALOG_HEARD: &str = "wm.dialog.heard";
    /// STT uncertain or transcribe timeout; no usable utterance.
    pub const DIALOG_UNHEARD: &str = "wm.dialog.unheard";
    /// A state-machine deadline elapsed; FSM returned to Idle.
    pub const DIALOG_TIMEOUT: &str = "wm.dialog.timeout";
}

/// Decoded request payloads. Returned by [`decode_request`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Request {
    /// `wm.audio.wake` payload.
    Wake(WakePayload),
    /// `wm.audio.speech.start` payload.
    SpeechStart(SpeechStartPayload),
    /// `wm.audio.speech.end` payload.
    SpeechEnd(SpeechEndPayload),
    /// `wm.stt.partial` payload.
    SttPartial(SttPartialPayload),
    /// `wm.stt.final` payload.
    SttFinal(SttFinalPayload),
    /// `wm.stt.uncertain` payload.
    SttUncertain(SttUncertainPayload),
    /// `wm.brain.reply` payload.
    BrainReply(BrainReplyPayload),
    /// `wm.brain.reply.destructive` payload.
    BrainReplyDestructive(BrainReplyDestructivePayload),
}

/// `wm.audio.wake` payload from `wintermute-audio` (`WakeDetected`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WakePayload {
    /// Kebab-case wake-word label, e.g. `"hey-jarvis"`.
    pub wake_word: String,
    /// Wake-model confidence, `0.0..=1.0`.
    pub confidence: f32,
    /// Emission timestamp (unix ms).
    pub ts: u64,
}

/// `wm.audio.speech.start` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpeechStartPayload {
    /// Rising-edge timestamp (unix ms).
    pub ts: u64,
}

/// `wm.audio.speech.end` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpeechEndPayload {
    /// Duration of the just-completed utterance in milliseconds.
    pub duration_ms: u32,
    /// Falling-edge timestamp (unix ms).
    pub ts: u64,
}

/// `wm.stt.partial` payload (`wintermute-stt::PartialEvent`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SttPartialPayload {
    /// Current best-guess transcript.
    pub text: String,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// `wm.stt.final` payload (`wintermute-stt::FinalEvent`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SttFinalPayload {
    /// Finalised transcript.
    pub text: String,
    /// Confidence in `(0.0, 1.0]`.
    pub confidence: f32,
    /// Wall-clock duration of the source audio in milliseconds.
    #[serde(default)]
    pub audio_duration_ms: Option<u32>,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// `wm.stt.uncertain` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SttUncertainPayload {
    /// Low-confidence transcript.
    pub text: String,
    /// Confidence below the active threshold.
    pub confidence: f32,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// `wm.brain.reply` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrainReplyPayload {
    /// Assistant reply text to speak via wm-tts.
    pub text: String,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// `wm.brain.reply.destructive` payload (PRD §2.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrainReplyDestructivePayload {
    /// Stable id for this destructive intent. Echoed in `confirm.granted` / `denied`.
    pub intent_id: String,
    /// Human-readable summary (e.g. `"delete 3 emails matching 'newsletter'"`).
    pub summary: String,
    /// Short, content-specific keyword required to grant (e.g. `"delete-email"`).
    pub confirm_keyword: String,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// Outbound `wm.dialog.state` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateEvent {
    /// Current conversational state (kebab-case: `idle`, `listening`, ...).
    pub state: String,
    /// State the FSM departed (same vocabulary as `state`).
    pub prior_state: String,
    /// Milliseconds the FSM spent in `prior_state`.
    pub since_ms: u64,
    /// Unix milliseconds when the transition fired.
    pub ts: u64,
}

/// Outbound `wm.dialog.turn.user` payload (consumed by `wintermute-brain`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnUserEvent {
    /// Finalised user transcript.
    pub transcript: String,
    /// stt confidence carried through.
    pub confidence: f32,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// Outbound `wm.dialog.turn.system` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnSystemEvent {
    /// Text the daemon handed to wm-tts.
    pub text: String,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// Outbound `wm.dialog.confirm.granted` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfirmGrantedEvent {
    /// `intent_id` carried over from the originating `brain.reply.destructive`.
    pub intent_id: String,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// Outbound `wm.dialog.confirm.denied` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfirmDeniedEvent {
    /// `intent_id` carried over from the originating `brain.reply.destructive`.
    pub intent_id: String,
    /// Short reason tag: `"keyword-mismatch" | "no" | "timeout" | "child-lock" | "muted"`.
    pub reason: String,
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// Outbound `wm.dialog.mute_request` / `wm.dialog.unmute_request` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MuteRequestEvent {
    /// Unix milliseconds at emission.
    pub ts: u64,
}

/// Outbound `wm.tts.cancel` payload (barge-in, PRD §2.3). Body is empty;
/// wm-tts tolerates `{}` and `null`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TtsCancelEvent {}

/// Errors raised while decoding an inbound payload.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Topic was not one of the known incoming names.
    #[error("unknown topic: {0}")]
    UnknownTopic(String),
    /// JSON decode of the payload failed.
    #[error("payload decode failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Decode a raw `(topic, data)` pair into a strongly-typed [`Request`].
///
/// # Errors
/// Returns [`DecodeError::UnknownTopic`] for topics outside the
/// [`incoming`] set, or [`DecodeError::Json`] when the payload shape
/// doesn't match the expected struct for that topic.
pub fn decode_request(topic: &str, data: &serde_json::Value) -> Result<Request, DecodeError> {
    match topic {
        incoming::WAKE => Ok(Request::Wake(serde_json::from_value(data.clone())?)),
        incoming::SPEECH_START => {
            Ok(Request::SpeechStart(serde_json::from_value(data.clone())?))
        }
        incoming::SPEECH_END => Ok(Request::SpeechEnd(serde_json::from_value(data.clone())?)),
        incoming::STT_PARTIAL => Ok(Request::SttPartial(serde_json::from_value(data.clone())?)),
        incoming::STT_FINAL => Ok(Request::SttFinal(serde_json::from_value(data.clone())?)),
        incoming::STT_UNCERTAIN => {
            Ok(Request::SttUncertain(serde_json::from_value(data.clone())?))
        }
        incoming::BRAIN_REPLY => Ok(Request::BrainReply(serde_json::from_value(data.clone())?)),
        incoming::BRAIN_REPLY_DESTRUCTIVE => Ok(Request::BrainReplyDestructive(
            serde_json::from_value(data.clone())?,
        )),
        other => Err(DecodeError::UnknownTopic(other.to_string())),
    }
}

/// Wall-clock milliseconds since the Unix epoch. Saturates to `u64::MAX`
/// if the clock is set before 1970 (shouldn't happen).
#[must_use]
pub fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::float_cmp
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decode_wake() {
        let req = decode_request(
            incoming::WAKE,
            &json!({ "wake_word": "hey-jarvis", "confidence": 0.93, "ts": 100 }),
        )
        .expect("wake parses");
        assert_eq!(
            req,
            Request::Wake(WakePayload {
                wake_word: "hey-jarvis".into(),
                confidence: 0.93,
                ts: 100,
            })
        );
    }

    #[test]
    fn decode_speech_start_and_end() {
        let start =
            decode_request(incoming::SPEECH_START, &json!({ "ts": 1 })).expect("start parses");
        assert_eq!(start, Request::SpeechStart(SpeechStartPayload { ts: 1 }));

        let end = decode_request(incoming::SPEECH_END, &json!({ "duration_ms": 2300, "ts": 2 }))
            .expect("end parses");
        assert_eq!(
            end,
            Request::SpeechEnd(SpeechEndPayload {
                duration_ms: 2300,
                ts: 2,
            })
        );
    }

    #[test]
    fn decode_stt_partial() {
        let req = decode_request(incoming::STT_PARTIAL, &json!({ "text": "hel", "ts": 10 }))
            .expect("partial parses");
        assert_eq!(
            req,
            Request::SttPartial(SttPartialPayload {
                text: "hel".into(),
                ts: 10,
            })
        );
    }

    #[test]
    fn decode_stt_final_with_optional_audio_duration() {
        let with_dur = decode_request(
            incoming::STT_FINAL,
            &json!({ "text": "hello there", "confidence": 0.84, "audio_duration_ms": 1500, "ts": 11 }),
        )
        .expect("final-with-dur parses");
        assert!(matches!(
            with_dur,
            Request::SttFinal(SttFinalPayload {
                ref text,
                audio_duration_ms: Some(1500),
                ..
            }) if text == "hello there"
        ));

        let without_dur = decode_request(
            incoming::STT_FINAL,
            &json!({ "text": "hi", "confidence": 0.7, "ts": 12 }),
        )
        .expect("final-no-dur parses");
        assert!(matches!(
            without_dur,
            Request::SttFinal(SttFinalPayload { audio_duration_ms: None, .. })
        ));
    }

    #[test]
    fn decode_stt_uncertain() {
        let req = decode_request(
            incoming::STT_UNCERTAIN,
            &json!({ "text": "???", "confidence": 0.2, "ts": 13 }),
        )
        .expect("uncertain parses");
        assert_eq!(
            req,
            Request::SttUncertain(SttUncertainPayload {
                text: "???".into(),
                confidence: 0.2,
                ts: 13,
            })
        );
    }

    #[test]
    fn decode_brain_reply() {
        let req = decode_request(
            incoming::BRAIN_REPLY,
            &json!({ "text": "Sure, I can help.", "ts": 20 }),
        )
        .expect("brain.reply parses");
        assert_eq!(
            req,
            Request::BrainReply(BrainReplyPayload {
                text: "Sure, I can help.".into(),
                ts: 20,
            })
        );
    }

    #[test]
    fn decode_brain_reply_destructive() {
        let req = decode_request(
            incoming::BRAIN_REPLY_DESTRUCTIVE,
            &json!({
                "intent_id": "intent-7f3a",
                "summary": "delete 3 emails matching 'newsletter'",
                "confirm_keyword": "delete-email",
                "ts": 30,
            }),
        )
        .expect("destructive parses");
        assert_eq!(
            req,
            Request::BrainReplyDestructive(BrainReplyDestructivePayload {
                intent_id: "intent-7f3a".into(),
                summary: "delete 3 emails matching 'newsletter'".into(),
                confirm_keyword: "delete-email".into(),
                ts: 30,
            })
        );
    }

    #[test]
    fn decode_unknown_topic() {
        let result = decode_request("wm.dialog.bogus", &json!({}));
        assert!(matches!(result, Err(DecodeError::UnknownTopic(t)) if t == "wm.dialog.bogus"));
    }

    #[test]
    fn decode_bad_payload_for_known_topic() {
        let result = decode_request(incoming::WAKE, &json!({ "wake_word": "x" }));
        assert!(matches!(result, Err(DecodeError::Json(_))));
    }

    #[test]
    fn outbound_state_event_roundtrips() {
        let ev = StateEvent {
            state: "speaking".into(),
            prior_state: "thinking".into(),
            since_ms: 1200,
            ts: 100,
        };
        let v = serde_json::to_value(&ev).expect("serializes");
        let back: StateEvent = serde_json::from_value(v).expect("round trips");
        assert_eq!(ev, back);
    }

    #[test]
    fn outbound_turn_events_roundtrip() {
        let user = TurnUserEvent {
            transcript: "what time is it".into(),
            confidence: 0.88,
            ts: 50,
        };
        let v = serde_json::to_value(&user).expect("serializes");
        let back: TurnUserEvent = serde_json::from_value(v).expect("round trips");
        assert_eq!(user, back);

        let sys = TurnSystemEvent {
            text: "It's 4:20 PM.".into(),
            ts: 51,
        };
        let v = serde_json::to_value(&sys).expect("serializes");
        let back: TurnSystemEvent = serde_json::from_value(v).expect("round trips");
        assert_eq!(sys, back);
    }

    #[test]
    fn outbound_confirm_events_roundtrip() {
        let granted = ConfirmGrantedEvent {
            intent_id: "intent-7f3a".into(),
            ts: 70,
        };
        let v = serde_json::to_value(&granted).expect("serializes");
        let back: ConfirmGrantedEvent = serde_json::from_value(v).expect("round trips");
        assert_eq!(granted, back);

        let denied = ConfirmDeniedEvent {
            intent_id: "intent-7f3a".into(),
            reason: "timeout".into(),
            ts: 71,
        };
        let v = serde_json::to_value(&denied).expect("serializes");
        let back: ConfirmDeniedEvent = serde_json::from_value(v).expect("round trips");
        assert_eq!(denied, back);
    }

    #[test]
    fn outbound_mute_request_and_tts_cancel_roundtrip() {
        let mute = MuteRequestEvent { ts: 80 };
        let v = serde_json::to_value(&mute).expect("serializes");
        let back: MuteRequestEvent = serde_json::from_value(v).expect("round trips");
        assert_eq!(mute, back);

        let cancel = TtsCancelEvent {};
        let v = serde_json::to_value(&cancel).expect("serializes");
        let back: TtsCancelEvent = serde_json::from_value(v).expect("round trips");
        assert_eq!(cancel, back);
    }

    #[test]
    #[allow(
        clippy::cognitive_complexity,
        reason = "flat assertion table — splitting hurts the PRD-row-per-line layout"
    )]
    fn topic_constants_match_prd_table() {
        assert_eq!(incoming::WAKE, "wm.audio.wake");
        assert_eq!(incoming::SPEECH_START, "wm.audio.speech.start");
        assert_eq!(incoming::SPEECH_END, "wm.audio.speech.end");
        assert_eq!(incoming::STT_PARTIAL, "wm.stt.partial");
        assert_eq!(incoming::STT_FINAL, "wm.stt.final");
        assert_eq!(incoming::STT_UNCERTAIN, "wm.stt.uncertain");
        assert_eq!(incoming::BRAIN_REPLY, "wm.brain.reply");
        assert_eq!(incoming::BRAIN_REPLY_DESTRUCTIVE, "wm.brain.reply.destructive");
        assert_eq!(outgoing::STATE, "wm.dialog.state");
        assert_eq!(outgoing::TURN_USER, "wm.dialog.turn.user");
        assert_eq!(outgoing::TURN_SYSTEM, "wm.dialog.turn.system");
        assert_eq!(outgoing::CONFIRM_GRANTED, "wm.dialog.confirm.granted");
        assert_eq!(outgoing::CONFIRM_DENIED, "wm.dialog.confirm.denied");
        assert_eq!(outgoing::MUTE_REQUEST, "wm.dialog.mute_request");
        assert_eq!(outgoing::UNMUTE_REQUEST, "wm.dialog.unmute_request");
        assert_eq!(outgoing::TTS_CANCEL, "wm.tts.cancel");
        assert_eq!(outgoing::TTS_SPEAK, "wm.tts.speak");
        assert_eq!(outgoing::BRAIN_UTTERANCE, "wm.brain.utterance");
        assert_eq!(outgoing::AUDIO_MUTE, "wm.audio.mute");
        assert_eq!(outgoing::AUDIO_UNMUTE, "wm.audio.unmute");
        assert_eq!(SUBSCRIBE_PREFIXES, ["wm.audio.", "wm.stt.", "wm.brain."]);
    }

    #[test]
    fn now_unix_ms_monotonic() {
        let a = now_unix_ms();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = now_unix_ms();
        assert!(b >= a);
    }
}
