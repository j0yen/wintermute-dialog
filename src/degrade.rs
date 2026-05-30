//! Degrade phrase bank — turn-fsm.
//!
//! When the FSM encounters an error condition during a conversational
//! turn (STT uncertain, transcription timeout, brain error, or think
//! timeout), it speaks a warm degrade phrase rather than going silently
//! back to Idle.
//!
//! Two phrase sets cover two failure modes:
//!
//! - **Heard-nothing path** ([`degrade_heard_nothing`]): the user spoke
//!   but we couldn't parse it.  Used on `SttUncertain`, transcribe
//!   timeout, and similar.
//! - **Think-error path** ([`degrade_think_error`]): the brain returned
//!   an error or took too long.  Used on `BrainError` and think timeout.
//!
//! Phrase selection is deliberately stable (index-based, not random) so
//! unit tests can assert a specific phrase.  Future work may add
//! selection logic based on recent-use history to reduce repetition.

/// Ordered phrase set for the "heard nothing / couldn't parse" path.
const HEARD_NOTHING_PHRASES: &[&str] = &[
    "Sorry, I didn't catch that.",
    "I didn't quite hear you — could you try again?",
    "My apologies, could you repeat that?",
];

/// Ordered phrase set for the "brain error / timeout" path.
const THINK_ERROR_PHRASES: &[&str] = &[
    "I'm having trouble thinking right now. Please try again in a moment.",
    "Sorry, something went wrong on my end.",
    "I couldn't work that out — give me a moment and try again.",
];

/// Return the degrade phrase for the heard-nothing path at attempt `n`
/// (0-indexed). Out-of-range indices repeat the last phrase.
///
/// # Examples
///
/// ```
/// use wintermute_dialog::degrade::degrade_heard_nothing;
/// assert_eq!(degrade_heard_nothing(0), "Sorry, I didn't catch that.");
/// // Out-of-range: last phrase repeats.
/// assert_eq!(degrade_heard_nothing(99), "My apologies, could you repeat that?");
/// ```
#[must_use]
pub fn degrade_heard_nothing(attempt: usize) -> &'static str {
    let idx = attempt.min(HEARD_NOTHING_PHRASES.len().saturating_sub(1));
    HEARD_NOTHING_PHRASES[idx]
}

/// Return the degrade phrase for the think-error path at attempt `n`
/// (0-indexed). Out-of-range indices repeat the last phrase.
///
/// # Examples
///
/// ```
/// use wintermute_dialog::degrade::degrade_think_error;
/// assert_eq!(degrade_think_error(0), "I'm having trouble thinking right now. Please try again in a moment.");
/// ```
#[must_use]
pub fn degrade_think_error(attempt: usize) -> &'static str {
    let idx = attempt.min(THINK_ERROR_PHRASES.len().saturating_sub(1));
    THINK_ERROR_PHRASES[idx]
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
    fn heard_nothing_attempt_0_is_non_empty() {
        assert!(!degrade_heard_nothing(0).is_empty());
    }

    #[test]
    fn heard_nothing_out_of_range_returns_last() {
        let last = *HEARD_NOTHING_PHRASES.last().expect("non-empty");
        assert_eq!(degrade_heard_nothing(100), last);
    }

    #[test]
    fn heard_nothing_phrases_are_distinct() {
        // Each phrase in the set should be unique.
        for i in 0..HEARD_NOTHING_PHRASES.len() {
            for j in (i + 1)..HEARD_NOTHING_PHRASES.len() {
                assert_ne!(
                    HEARD_NOTHING_PHRASES[i], HEARD_NOTHING_PHRASES[j],
                    "degrade phrases at index {i} and {j} are identical"
                );
            }
        }
    }

    #[test]
    fn think_error_attempt_0_is_non_empty() {
        assert!(!degrade_think_error(0).is_empty());
    }

    #[test]
    fn think_error_out_of_range_returns_last() {
        let last = *THINK_ERROR_PHRASES.last().expect("non-empty");
        assert_eq!(degrade_think_error(100), last);
    }

    #[test]
    fn think_error_phrases_are_distinct() {
        for i in 0..THINK_ERROR_PHRASES.len() {
            for j in (i + 1)..THINK_ERROR_PHRASES.len() {
                assert_ne!(
                    THINK_ERROR_PHRASES[i], THINK_ERROR_PHRASES[j],
                    "think-error phrases at index {i} and {j} are identical"
                );
            }
        }
    }

    #[test]
    fn heard_nothing_differs_from_think_error_at_same_index() {
        assert_ne!(
            degrade_heard_nothing(0),
            degrade_think_error(0),
            "heard-nothing and think-error phrase[0] should differ"
        );
    }
}
