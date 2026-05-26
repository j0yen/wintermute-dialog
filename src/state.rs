//! Conversational-state model for `wm-dialog`.
//!
//! [`State`] holds the FSM's current node — including payload for
//! `Confirming` (intent id, keyword, attempt count). [`StateTag`] is
//! the payload-less variant used in history entries and transition
//! summaries (cheap to copy, easy to compare).
//!
//! [`Flags`] tracks the two orthogonal policies that cut across all
//! states: `muted` gates wake + TTS; `child_locked` auto-denies every
//! destructive intent silently.

use serde::{Deserialize, Serialize};

/// Conversational FSM node. PRD §2.1 diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// No interaction in progress.
    Idle,
    /// Wake was detected; awaiting speech start.
    Listening,
    /// Speech is being captured + transcribed by `wm-stt`.
    Transcribing,
    /// Final transcript forwarded to `wm-brain`; awaiting reply.
    Thinking,
    /// `wm-tts` is rendering a reply.
    Speaking,
    /// Verbal-confirmation flow for a destructive intent is in flight.
    Confirming(ConfirmContext),
}

/// Per-confirm payload — the intent under review plus the running
/// re-prompt attempt count (0 = first prompt, 1 = after one re-prompt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmContext {
    /// Brain-issued id we echo back on `wm.dialog.confirm.{granted,denied}`.
    pub intent_id: String,
    /// Human-readable summary of the destructive action.
    pub summary: String,
    /// Short content-specific keyword the user must say with `yes`.
    pub confirm_keyword: String,
    /// Number of re-prompts that have already fired. PRD allows one.
    pub attempts: u8,
}

/// Payload-less state tag — used for history entries, comparisons,
/// and serialized state reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateTag {
    /// See [`State::Idle`].
    Idle,
    /// See [`State::Listening`].
    Listening,
    /// See [`State::Transcribing`].
    Transcribing,
    /// See [`State::Thinking`].
    Thinking,
    /// See [`State::Speaking`].
    Speaking,
    /// See [`State::Confirming`].
    Confirming,
}

impl State {
    /// Payload-less projection used by history + JSON state reports.
    #[must_use]
    pub const fn tag(&self) -> StateTag {
        match self {
            Self::Idle => StateTag::Idle,
            Self::Listening => StateTag::Listening,
            Self::Transcribing => StateTag::Transcribing,
            Self::Thinking => StateTag::Thinking,
            Self::Speaking => StateTag::Speaking,
            Self::Confirming(_) => StateTag::Confirming,
        }
    }
}

/// Orthogonal flags that cut across every state node. PRD §2.5.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flags {
    /// `mute_request` gate. While true, TTS is cancelled and wake is ignored.
    pub muted: bool,
    /// `child_lock` policy. While true, destructive intents auto-deny.
    pub child_locked: bool,
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
    fn state_tag_round_trips_for_each_node() {
        let cases = [
            (State::Idle, StateTag::Idle),
            (State::Listening, StateTag::Listening),
            (State::Transcribing, StateTag::Transcribing),
            (State::Thinking, StateTag::Thinking),
            (State::Speaking, StateTag::Speaking),
            (
                State::Confirming(ConfirmContext {
                    intent_id: "x".to_string(),
                    summary: "y".to_string(),
                    confirm_keyword: "z".to_string(),
                    attempts: 0,
                }),
                StateTag::Confirming,
            ),
        ];
        for (state, expected) in cases {
            assert_eq!(state.tag(), expected);
        }
    }

    #[test]
    fn flags_default_unset() {
        let f = Flags::default();
        assert!(!f.muted);
        assert!(!f.child_locked);
    }

    #[test]
    fn state_tag_serde_snake_case() {
        let json = serde_json::to_string(&StateTag::Confirming).expect("serialises");
        assert_eq!(json, "\"confirming\"");
    }
}
