//! Output side-effects the FSM emits per event. PRD §2.2 (published
//! column) + the TTS / audio control channel.
//!
//! [`Action`] is pure data. The driver loop is responsible for
//! translating each variant into an agorabus publish (iter-3+) or a
//! direct TTS / audio call.

use serde::{Deserialize, Serialize};

use crate::state::StateTag;

/// One side-effect to emit after handling an [`crate::Event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Publish `wm.dialog.state`.
    PublishState {
        /// Pre-transition tag.
        prior: StateTag,
        /// Post-transition tag.
        next: StateTag,
        /// Wall-clock ms spent in `prior` before this transition.
        since_ms: u64,
    },
    /// Publish `wm.dialog.turn.user`.
    PublishTurnUser {
        /// Final transcript.
        transcript: String,
        /// `[0.0, 1.0]` recognizer confidence.
        confidence: f32,
    },
    /// Publish `wm.dialog.turn.system`.
    PublishTurnSystem {
        /// Reply text that's being narrated.
        text: String,
    },
    /// Publish `wm.dialog.confirm.granted`.
    PublishConfirmGranted {
        /// Brain-issued intent correlation id.
        intent_id: String,
    },
    /// Publish `wm.dialog.confirm.denied`.
    PublishConfirmDenied {
        /// Brain-issued intent correlation id.
        intent_id: String,
        /// Why the confirm was denied.
        reason: DenyReason,
    },
    /// Publish `wm.audio.mute` — gate wake at the audio layer.
    PublishAudioMute,
    /// Publish `wm.audio.unmute` — release the wake gate.
    PublishAudioUnmute,
    /// Publish `wm.tts.cancel` — kill in-flight TTS.
    PublishTtsCancel,
    /// Publish `wm.tts.say` (or equivalent) — render `text`.
    PublishTtsSay {
        /// Text to render.
        text: String,
    },
    /// Publish `wm.brain.utterance` — forward final transcript.
    PublishBrainUtterance {
        /// Final transcript.
        transcript: String,
        /// `[0.0, 1.0]` recognizer confidence.
        confidence: f32,
    },
    /// Start (or restart) the confirm-timeout timer.
    StartConfirmTimer {
        /// Timeout in milliseconds.
        ms: u32,
    },
    /// Cancel any in-flight confirm-timeout timer.
    CancelConfirmTimer,
    /// Publish `wm.dialog.attention` — wake was detected, FSM is
    /// now listening for speech. UI hook (LED, sound, etc.).
    PublishDialogAttention,
    /// Publish `wm.dialog.heard` — STT final transcript was forwarded
    /// to the brain. Carries the transcript text for observers.
    PublishDialogHeard {
        /// The recognised utterance text.
        text: String,
    },
    /// Publish `wm.dialog.unheard` — STT was uncertain or timed out;
    /// FSM is returning to Idle after a degrade phrase.
    PublishDialogUnheard,
    /// Publish `wm.dialog.timeout` — a state-machine deadline elapsed
    /// (capture 8s, transcribe 3s, or think 10s); FSM returns to Idle.
    PublishDialogTimeout,
    /// Start (or restart) the capture-timeout timer (PRD §2.1: 8s after
    /// wake, reset to Idle if no speech arrives).
    StartCaptureTimer {
        /// Timeout in milliseconds.
        ms: u32,
    },
    /// Cancel any in-flight capture-timeout timer.
    CancelCaptureTimer,
    /// Start (or restart) the transcribe-timeout timer (PRD §2.1: 3s
    /// after speech-end, reset to Idle if no STT result arrives).
    StartTranscribeTimer {
        /// Timeout in milliseconds.
        ms: u32,
    },
    /// Cancel any in-flight transcribe-timeout timer.
    CancelTranscribeTimer,
    /// Start (or restart) the think-timeout timer (PRD §2.1: 10s
    /// after brain forward, emit degrade phrase and reset to Idle).
    StartThinkTimer {
        /// Timeout in milliseconds.
        ms: u32,
    },
    /// Cancel any in-flight think-timeout timer.
    CancelThinkTimer,
}

/// Why a verbal-confirm flow ended in deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyReason {
    /// User said `no`, `cancel`, or `stop`.
    UserSaidNo,
    /// 30-second timer elapsed with no further utterance.
    Silence,
    /// One re-prompt already fired and the next utterance still didn't
    /// match `yes <keyword>`.
    Ambiguous,
    /// `child_lock = true` auto-denied without prompting.
    ChildLock,
    /// Wake fired during `confirming`; the user is effectively
    /// interrupting / cancelling the flow.
    BargeIn,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn deny_reason_serde_snake_case() {
        let r = DenyReason::UserSaidNo;
        let json = serde_json::to_string(&r).expect("serialises");
        assert_eq!(json, "\"user_said_no\"");
        let back: DenyReason = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(back, r);
    }

    #[test]
    fn action_serde_tagged() {
        let a = Action::PublishConfirmDenied {
            intent_id: "intent-1".to_string(),
            reason: DenyReason::Silence,
        };
        let v = serde_json::to_value(&a).expect("serialises");
        assert_eq!(v["kind"], "publish_confirm_denied");
        assert_eq!(v["intent_id"], "intent-1");
        assert_eq!(v["reason"], "silence");
    }
}
