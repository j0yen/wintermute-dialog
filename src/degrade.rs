//! Degrade phrase bank — turn-fsm.
//!
//! When the FSM encounters an error condition during a conversational
//! turn (STT uncertain, transcription timeout, brain error, or think
//! timeout), it speaks a warm degrade phrase rather than going silently
//! back to Idle.
//!
//! [`DegradeBank`] holds a small, mode-distinct phrase set per
//! [`DegradeKind`] and rotates through them so consecutive failures of
//! the same kind produce different output.  Rotation is deterministic
//! (round-robin modulo phrase count) so unit tests can assert exact
//! values.
//!
//! The free functions [`degrade_heard_nothing`] and [`degrade_think_error`]
//! are preserved for backward compatibility; new callers should prefer
//! [`DegradeBank`].

// ── phrase banks ────────────────────────────────────────────────────────────

/// Phrases for [`DegradeKind::SttUncertain`] — recognizer abstained.
const STT_UNCERTAIN_PHRASES: &[&str] = &[
    "Sorry, I didn't catch that.",
    "Hm, I didn't quite hear you.",
    "Could you say that again?",
];

/// Phrases for [`DegradeKind::TranscribeTimeout`] — transcribe timer elapsed.
const TRANSCRIBE_TIMEOUT_PHRASES: &[&str] = &[
    "I'm still listening — go ahead.",
    "Sorry, I lost the thread there. Once more?",
];

/// Phrases for [`DegradeKind::BrainError`] — brain returned an error.
const BRAIN_ERROR_PHRASES: &[&str] = &[
    "Something went wrong on my end. One moment.",
    "Sorry — let me try that again.",
];

/// Phrases for [`DegradeKind::ThinkTimeout`] — brain took too long.
const THINK_TIMEOUT_PHRASES: &[&str] = &[
    "That's taking me a moment. Bear with me.",
    "Sorry, that took too long.",
];

// ── backward-compat phrase sets (index-clamped) ──────────────────────────────

/// Ordered phrase set for the "heard nothing / couldn't parse" path.
///
/// Used by the backward-compatible [`degrade_heard_nothing`] helper.
const HEARD_NOTHING_PHRASES: &[&str] = STT_UNCERTAIN_PHRASES;

/// Ordered phrase set for the "brain error / timeout" path.
///
/// Used by the backward-compatible [`degrade_think_error`] helper.
const THINK_ERROR_PHRASES: &[&str] = THINK_TIMEOUT_PHRASES;

// ── DegradeKind ──────────────────────────────────────────────────────────────

/// The conversational failure kind that triggered a degrade response.
///
/// Each variant maps to a distinct phrase set in [`DegradeBank`] so
/// repeated stumbles of the same kind rotate through varied phrasing,
/// and mixed-mode failures never sound identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DegradeKind {
    /// STT recognizer abstained — the user spoke but we couldn't parse it.
    SttUncertain,
    /// Transcribe timer elapsed — speech ended without an STT result.
    TranscribeTimeout,
    /// Brain returned an explicit error.
    BrainError,
    /// Think timer elapsed — brain took too long.
    ThinkTimeout,
}

impl DegradeKind {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        match self {
            Self::SttUncertain => 0,
            Self::TranscribeTimeout => 1,
            Self::BrainError => 2,
            Self::ThinkTimeout => 3,
        }
    }

    const fn phrases(self) -> &'static [&'static str] {
        match self {
            Self::SttUncertain => STT_UNCERTAIN_PHRASES,
            Self::TranscribeTimeout => TRANSCRIBE_TIMEOUT_PHRASES,
            Self::BrainError => BRAIN_ERROR_PHRASES,
            Self::ThinkTimeout => THINK_TIMEOUT_PHRASES,
        }
    }
}

// ── DegradeBank ──────────────────────────────────────────────────────────────

/// Rotating phrase bank — one independent cursor per [`DegradeKind`].
///
/// Each call to [`DegradeBank::next_phrase`] returns the next phrase for
/// that kind (modulo phrase count) and advances the cursor, so two
/// consecutive failures of the same kind produce different output.
///
/// A freshly constructed bank always starts at cursor 0 for every kind,
/// so `next_phrase(SttUncertain)` on a new bank returns the legacy
/// `"Sorry, I didn't catch that."` phrase — preserving the AC6 contract
/// from the FSM PRD.
#[derive(Debug, Clone)]
pub struct DegradeBank {
    cursors: [usize; DegradeKind::COUNT],
}

impl DegradeBank {
    /// Construct a fresh bank with all cursors at 0.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cursors: [0; DegradeKind::COUNT],
        }
    }

    /// Return the next phrase for `kind` and advance that kind's cursor.
    ///
    /// The cursor wraps modulo the phrase count, so calling this method
    /// `len + 1` times returns the first phrase again.
    #[allow(clippy::indexing_slicing, reason = "cursors are always < phrases.len() by construction")]
    pub fn next_phrase(&mut self, kind: DegradeKind) -> &'static str {
        let idx = kind.index();
        let phrases = kind.phrases();
        let cursor = self.cursors[idx];
        self.cursors[idx] = (cursor + 1) % phrases.len();
        phrases[cursor]
    }

    /// Peek at the phrase that would be returned by the next
    /// [`DegradeBank::next_phrase`] call for `kind`, without advancing
    /// the cursor.
    #[must_use]
    #[allow(clippy::indexing_slicing, reason = "cursors are always < phrases.len() by construction")]
    pub fn peek_phrase(&self, kind: DegradeKind) -> &'static str {
        let idx = kind.index();
        let phrases = kind.phrases();
        phrases[self.cursors[idx]]
    }
}

impl Default for DegradeBank {
    fn default() -> Self {
        Self::new()
    }
}

// ── backward-compatible free functions ──────────────────────────────────────

/// Return the degrade phrase for the heard-nothing path at attempt `n`
/// (0-indexed). Out-of-range indices repeat the last phrase.
///
/// # Examples
///
/// ```
/// use wintermute_dialog::degrade::degrade_heard_nothing;
/// assert_eq!(degrade_heard_nothing(0), "Sorry, I didn't catch that.");
/// // Out-of-range: last phrase repeats.
/// assert_eq!(degrade_heard_nothing(99), "Could you say that again?");
/// ```
#[must_use]
#[allow(clippy::indexing_slicing, reason = "idx is clamped to len-1")]
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
/// assert_eq!(degrade_think_error(0), "That's taking me a moment. Bear with me.");
/// ```
#[must_use]
#[allow(clippy::indexing_slicing, reason = "idx is clamped to len-1")]
pub fn degrade_think_error(attempt: usize) -> &'static str {
    let idx = attempt.min(THINK_ERROR_PHRASES.len().saturating_sub(1));
    THINK_ERROR_PHRASES[idx]
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::needless_range_loop,
    clippy::doc_markdown,
    reason = "tests"
)]
mod tests {
    use super::*;

    // ── DegradeBank AC tests ─────────────────────────────────────────────

    /// AC2 — modes differ: fresh bank, SttUncertain ≠ TranscribeTimeout.
    #[test]
    fn bank_modes_differ_on_fresh_bank() {
        let mut bank = DegradeBank::new();
        let uncertain = bank.next_phrase(DegradeKind::SttUncertain);
        let mut bank2 = DegradeBank::new();
        let transcribe = bank2.next_phrase(DegradeKind::TranscribeTimeout);
        assert_ne!(
            uncertain, transcribe,
            "SttUncertain and TranscribeTimeout first phrases must differ"
        );
    }

    /// AC3 — no immediate repeat: consecutive next_phrase calls for a kind
    /// with ≥ 2 phrases return different strings.
    #[test]
    fn bank_no_immediate_repeat_stt_uncertain() {
        let mut bank = DegradeBank::new();
        let p1 = bank.next_phrase(DegradeKind::SttUncertain);
        let p2 = bank.next_phrase(DegradeKind::SttUncertain);
        assert_ne!(p1, p2, "consecutive SttUncertain phrases must differ");
    }

    #[test]
    fn bank_no_immediate_repeat_transcribe_timeout() {
        let mut bank = DegradeBank::new();
        let p1 = bank.next_phrase(DegradeKind::TranscribeTimeout);
        let p2 = bank.next_phrase(DegradeKind::TranscribeTimeout);
        assert_ne!(
            p1, p2,
            "consecutive TranscribeTimeout phrases must differ"
        );
    }

    #[test]
    fn bank_no_immediate_repeat_brain_error() {
        let mut bank = DegradeBank::new();
        let p1 = bank.next_phrase(DegradeKind::BrainError);
        let p2 = bank.next_phrase(DegradeKind::BrainError);
        assert_ne!(p1, p2, "consecutive BrainError phrases must differ");
    }

    #[test]
    fn bank_no_immediate_repeat_think_timeout() {
        let mut bank = DegradeBank::new();
        let p1 = bank.next_phrase(DegradeKind::ThinkTimeout);
        let p2 = bank.next_phrase(DegradeKind::ThinkTimeout);
        assert_ne!(p1, p2, "consecutive ThinkTimeout phrases must differ");
    }

    /// AC4 — rotation wraps: calling next_phrase len+1 times returns the
    /// first phrase again.
    #[test]
    fn bank_rotation_wraps_stt_uncertain() {
        let mut bank = DegradeBank::new();
        let first = bank.next_phrase(DegradeKind::SttUncertain);
        let len = STT_UNCERTAIN_PHRASES.len();
        for _ in 1..len {
            bank.next_phrase(DegradeKind::SttUncertain);
        }
        let wrapped = bank.next_phrase(DegradeKind::SttUncertain);
        assert_eq!(first, wrapped, "cursor must wrap modulo phrase count");
    }

    #[test]
    fn bank_rotation_wraps_transcribe_timeout() {
        let mut bank = DegradeBank::new();
        let first = bank.next_phrase(DegradeKind::TranscribeTimeout);
        let len = TRANSCRIBE_TIMEOUT_PHRASES.len();
        for _ in 1..len {
            bank.next_phrase(DegradeKind::TranscribeTimeout);
        }
        let wrapped = bank.next_phrase(DegradeKind::TranscribeTimeout);
        assert_eq!(first, wrapped, "TranscribeTimeout cursor must wrap");
    }

    #[test]
    fn bank_rotation_wraps_brain_error() {
        let mut bank = DegradeBank::new();
        let first = bank.next_phrase(DegradeKind::BrainError);
        let len = BRAIN_ERROR_PHRASES.len();
        for _ in 1..len {
            bank.next_phrase(DegradeKind::BrainError);
        }
        let wrapped = bank.next_phrase(DegradeKind::BrainError);
        assert_eq!(first, wrapped, "BrainError cursor must wrap");
    }

    #[test]
    fn bank_rotation_wraps_think_timeout() {
        let mut bank = DegradeBank::new();
        let first = bank.next_phrase(DegradeKind::ThinkTimeout);
        let len = THINK_TIMEOUT_PHRASES.len();
        for _ in 1..len {
            bank.next_phrase(DegradeKind::ThinkTimeout);
        }
        let wrapped = bank.next_phrase(DegradeKind::ThinkTimeout);
        assert_eq!(first, wrapped, "ThinkTimeout cursor must wrap");
    }

    /// AC5 — TTS ceiling: every phrase for every kind is non-empty and < 80 chars.
    #[test]
    fn bank_all_phrases_tts_ceiling() {
        let all: &[(&str, &[&str])] = &[
            ("SttUncertain", STT_UNCERTAIN_PHRASES),
            ("TranscribeTimeout", TRANSCRIBE_TIMEOUT_PHRASES),
            ("BrainError", BRAIN_ERROR_PHRASES),
            ("ThinkTimeout", THINK_TIMEOUT_PHRASES),
        ];
        for (name, phrases) in all {
            for (i, phrase) in phrases.iter().enumerate() {
                assert!(!phrase.is_empty(), "{name}[{i}] must be non-empty");
                assert!(
                    phrase.len() < 80,
                    "{name}[{i}] must be < 80 chars (TTS ceiling), got {}",
                    phrase.len()
                );
            }
        }
    }

    /// AC6 — legacy contract: fresh bank's first SttUncertain phrase
    /// (lowercased) contains "didn't catch".
    #[test]
    fn bank_first_stt_uncertain_contains_didnt_catch() {
        let mut bank = DegradeBank::new();
        let phrase = bank.next_phrase(DegradeKind::SttUncertain);
        assert!(
            phrase.to_lowercase().contains("didn't catch"),
            "first SttUncertain phrase must contain \"didn't catch\", got: {phrase:?}"
        );
    }

    /// AC7 — cursors are independent: advancing one kind does not affect
    /// another kind's cursor.
    #[test]
    fn bank_cursors_are_independent() {
        let mut bank = DegradeBank::new();
        for _ in 0..5 {
            bank.next_phrase(DegradeKind::SttUncertain);
        }
        let t = bank.peek_phrase(DegradeKind::TranscribeTimeout);
        assert_eq!(
            t, TRANSCRIBE_TIMEOUT_PHRASES[0],
            "TranscribeTimeout cursor should not be affected by SttUncertain advances"
        );
    }

    // ── backward-compat free-function tests ─────────────────────────────

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
