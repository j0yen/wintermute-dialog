//! Input events the FSM consumes. PRD §2.2 (subscribed column).
//!
//! Wire format lives downstream (agorabus decoder is iter-3+). The
//! FSM itself only handles parsed [`Event`] values.

use serde::{Deserialize, Serialize};

/// A single input event delivered to [`crate::Fsm::handle`].
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `wm.audio.wake` — wake-word detector fired.
    AudioWake,
    /// `wm.audio.speech.start` — user started speaking.
    AudioSpeechStart,
    /// `wm.audio.speech.end` — user stopped speaking.
    AudioSpeechEnd,
    /// `wm.stt.partial` — informational partial transcript; ignored
    /// outside debug.
    SttPartial,
    /// `wm.stt.final` — finalized transcript ready for the brain
    /// (or for confirm-keyword matching when in [`crate::State::Confirming`]).
    SttFinal {
        /// The recognized text. Already utf-8 normalized upstream.
        transcript: String,
        /// `[0.0, 1.0]` confidence from the recognizer.
        confidence: f32,
    },
    /// `wm.stt.uncertain` — recognizer abstained; re-prompt the user.
    SttUncertain,
    /// `wm.brain.reply` — non-destructive reply text to speak.
    BrainReply {
        /// The text `wm-tts` will render.
        text: String,
    },
    /// `wm.brain.reply.destructive` — destructive intent that must
    /// pass verbal confirmation (or auto-deny under child-lock).
    BrainReplyDestructive {
        /// Brain-issued correlation id.
        intent_id: String,
        /// Human-readable summary the prompt narrates.
        summary: String,
        /// Short content-specific keyword the user must say with `yes`.
        confirm_keyword: String,
    },
    /// `wm.tts.end` — TTS finished a render; if we were speaking, fall
    /// back to idle.
    TtsEnd,
    /// `wm.dialog.mute_request` — operator (or wm-cli) asked for mute.
    MuteRequest,
    /// `wm.dialog.unmute_request` — opposite of [`Self::MuteRequest`].
    UnmuteRequest,
    /// `wm child-lock on|off` — operator policy toggle.
    SetChildLock {
        /// Target value for [`crate::Flags::child_locked`].
        enabled: bool,
    },
    /// Internal `confirm-timeout` tick fired by the FSM driver.
    ConfirmTimeout,
    /// Internal capture-timeout tick: wake fired but no speech detected
    /// within the allotted window (PRD §2.1 `Listening + timeout 8s`).
    CaptureTimeout,
    /// Internal transcribe-timeout tick: speech ended but no STT result
    /// within the allotted window (PRD §2.1 `Transcribing + timeout 3s`).
    TranscribeTimeout,
    /// Internal think-timeout tick: utterance forwarded to brain but no
    /// reply within the allotted window (PRD §2.1 `Thinking + timeout 10s`).
    ThinkTimeout,
    /// `wm.brain.error` — brain encountered an error processing the
    /// utterance; triggers degrade-path TTS then returns to Idle.
    BrainError,
}

/// Payload-less event identifier used in transition history entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTag {
    /// See [`Event::AudioWake`].
    AudioWake,
    /// See [`Event::AudioSpeechStart`].
    AudioSpeechStart,
    /// See [`Event::AudioSpeechEnd`].
    AudioSpeechEnd,
    /// See [`Event::SttPartial`].
    SttPartial,
    /// See [`Event::SttFinal`].
    SttFinal,
    /// See [`Event::SttUncertain`].
    SttUncertain,
    /// See [`Event::BrainReply`].
    BrainReply,
    /// See [`Event::BrainReplyDestructive`].
    BrainReplyDestructive,
    /// See [`Event::TtsEnd`].
    TtsEnd,
    /// See [`Event::MuteRequest`].
    MuteRequest,
    /// See [`Event::UnmuteRequest`].
    UnmuteRequest,
    /// See [`Event::SetChildLock`].
    SetChildLock,
    /// See [`Event::ConfirmTimeout`].
    ConfirmTimeout,
    /// See [`Event::CaptureTimeout`].
    CaptureTimeout,
    /// See [`Event::TranscribeTimeout`].
    TranscribeTimeout,
    /// See [`Event::ThinkTimeout`].
    ThinkTimeout,
    /// See [`Event::BrainError`].
    BrainError,
}

impl Event {
    /// Payload-less projection used by transition history.
    #[must_use]
    pub const fn tag(&self) -> EventTag {
        match self {
            Self::AudioWake => EventTag::AudioWake,
            Self::AudioSpeechStart => EventTag::AudioSpeechStart,
            Self::AudioSpeechEnd => EventTag::AudioSpeechEnd,
            Self::SttPartial => EventTag::SttPartial,
            Self::SttFinal { .. } => EventTag::SttFinal,
            Self::SttUncertain => EventTag::SttUncertain,
            Self::BrainReply { .. } => EventTag::BrainReply,
            Self::BrainReplyDestructive { .. } => EventTag::BrainReplyDestructive,
            Self::TtsEnd => EventTag::TtsEnd,
            Self::MuteRequest => EventTag::MuteRequest,
            Self::UnmuteRequest => EventTag::UnmuteRequest,
            Self::SetChildLock { .. } => EventTag::SetChildLock,
            Self::ConfirmTimeout => EventTag::ConfirmTimeout,
            Self::CaptureTimeout => EventTag::CaptureTimeout,
            Self::TranscribeTimeout => EventTag::TranscribeTimeout,
            Self::ThinkTimeout => EventTag::ThinkTimeout,
            Self::BrainError => EventTag::BrainError,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trips_each_variant() {
        let pairs: &[(Event, EventTag)] = &[
            (Event::AudioWake, EventTag::AudioWake),
            (Event::AudioSpeechStart, EventTag::AudioSpeechStart),
            (Event::AudioSpeechEnd, EventTag::AudioSpeechEnd),
            (Event::SttPartial, EventTag::SttPartial),
            (
                Event::SttFinal {
                    transcript: "hi".to_string(),
                    confidence: 0.9,
                },
                EventTag::SttFinal,
            ),
            (Event::SttUncertain, EventTag::SttUncertain),
            (
                Event::BrainReply {
                    text: "ok".to_string(),
                },
                EventTag::BrainReply,
            ),
            (
                Event::BrainReplyDestructive {
                    intent_id: "i".to_string(),
                    summary: "s".to_string(),
                    confirm_keyword: "k".to_string(),
                },
                EventTag::BrainReplyDestructive,
            ),
            (Event::TtsEnd, EventTag::TtsEnd),
            (Event::MuteRequest, EventTag::MuteRequest),
            (Event::UnmuteRequest, EventTag::UnmuteRequest),
            (Event::SetChildLock { enabled: true }, EventTag::SetChildLock),
            (Event::ConfirmTimeout, EventTag::ConfirmTimeout),
            (Event::CaptureTimeout, EventTag::CaptureTimeout),
            (Event::TranscribeTimeout, EventTag::TranscribeTimeout),
            (Event::ThinkTimeout, EventTag::ThinkTimeout),
            (Event::BrainError, EventTag::BrainError),
        ];
        for (ev, expected) in pairs {
            assert_eq!(ev.tag(), *expected);
        }
    }

    #[test]
    fn tag_serde_snake_case() {
        let json = serde_json::to_string(&EventTag::BrainReplyDestructive).expect("serialises");
        assert_eq!(json, "\"brain_reply_destructive\"");
    }
}
