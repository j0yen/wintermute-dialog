//! Silence-path phrase bank — earshot-gentle-reprompt.
//!
//! When an elder doesn't answer the verbal-confirm in time, the FSM
//! walks through an escalating series of warm check-ins rather than
//! dropping silently back to idle.  This module owns the phrase set
//! for that no-response sequence.
//!
//! **Scope:** only the *silence / no-response* path.  STT-uncertain /
//! transcribe-timeout phrases live in `degrade.rs` (hearth-dialog-
//! degrade-warmth), not here.
//!
//! ## Phrase selection
//!
//! [`silence_reprompt`] selects the check-in phrase for attempt `n`
//! (0-indexed, where 0 is the first reprompt that fires after the
//! initial question times out).  [`silence_close`] returns the warm
//! farewell spoken *before* the FSM transitions to Idle on the final
//! timeout.
//!
//! Defaults are provided inline; future work may wire these into the
//! `[timing]` config table alongside `max_reprompts`.

/// Ordered set of warm reprompt check-ins for the silence path.
///
/// Attempt 0 fires on the first timeout, attempt 1 on the second, etc.
/// If the attempt index exceeds the slice, the last entry is reused.
const REPROMPT_PHRASES: &[&str] = &[
    "I'm still here — take your time.",
    "Whenever you're ready.",
];

/// The warm spoken close emitted on the final timeout (just before
/// `Confirming → Idle`). `DenyReason::Silence` is still recorded;
/// this phrase accompanies it so the return-to-idle is announced rather
/// than silent.
const CLOSE_PHRASE: &str = "I'll be right here when you need me.";

/// Return the warm check-in phrase for silence-path reprompt attempt
/// `n` (0-indexed).
///
/// # Examples
///
/// ```
/// use wintermute_dialog::silence::silence_reprompt;
/// assert_eq!(silence_reprompt(0), "I'm still here — take your time.");
/// assert_eq!(silence_reprompt(1), "Whenever you're ready.");
/// // Out-of-range: last phrase repeats.
/// assert_eq!(silence_reprompt(99), "Whenever you're ready.");
/// ```
#[must_use]
pub fn silence_reprompt(attempt: usize) -> &'static str {
    let idx = attempt.min(REPROMPT_PHRASES.len().saturating_sub(1));
    REPROMPT_PHRASES[idx]
}

/// Return the warm close phrase spoken before the FSM transitions to
/// Idle on the final confirm timeout.
#[must_use]
pub const fn silence_close() -> &'static str {
    CLOSE_PHRASE
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
    fn reprompt_attempt_0_first_phrase() {
        assert_eq!(silence_reprompt(0), REPROMPT_PHRASES[0]);
    }

    #[test]
    fn reprompt_attempt_1_second_phrase() {
        assert_eq!(silence_reprompt(1), REPROMPT_PHRASES[1]);
    }

    #[test]
    fn reprompt_out_of_range_returns_last() {
        let last = *REPROMPT_PHRASES.last().expect("non-empty");
        assert_eq!(silence_reprompt(100), last);
    }

    #[test]
    fn reprompt_attempt_0_differs_from_attempt_1() {
        assert_ne!(
            silence_reprompt(0),
            silence_reprompt(1),
            "attempt 0 and 1 must produce distinct phrases"
        );
    }

    #[test]
    fn close_is_non_empty() {
        assert!(!silence_close().is_empty());
    }
}
