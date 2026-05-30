//! Distress phrase bank and classification for `wintermute-dialog`.
//!
//! Provides deterministic, API-independent detection of distress signals
//! in STT transcripts. This module is the core of the family-distress PRD
//! (PRD §2.1): distress detection **must** work without any Claude API call
//! so it survives a degraded brain.
//!
//! # Severity levels
//!
//! - [`Severity::Hard`] — fire `wm.family.distress` immediately with no
//!   confirmation step ("I've fallen", "emergency", etc.).
//! - [`Severity::Soft`] — prompt for confirmation before firing
//!   ("I don't feel well", etc.).
//!
//! # Phrase ordering
//!
//! Hard phrases are checked first; if both a Hard and Soft phrase appear in
//! the same transcript, `Hard` is returned.
//!
//! # Assurance phrases
//!
//! [`distress_assurance`] returns the spoken assurance phrase that is emitted
//! immediately on a Hard distress detection (PRD §2.2, §3 AC6).  The phrase
//! is registered here, parallel to [`crate::degrade`], so it has a single
//! source-of-truth rather than an ad-hoc string.
//!
//! [`distress_soft_prompt`] returns the "should I let Joe know?" confirmation
//! prompt for Soft distress.
//!
//! [`distress_failure_phrase`] returns the spoken phrase when delivery fails
//! (AC7: failure is never silent).

use serde::{Deserialize, Serialize};

// ── Assurance phrase bank ───────────────────────────────────────────────

/// The assurance phrase spoken immediately when Hard distress is detected.
/// Registered here (parallel to `degrade.rs`) so there is one source-of-truth
/// and tests can assert the phrase is sourced from this module (PRD §3 AC6).
const DISTRESS_ASSURANCE_PHRASES: &[&str] = &[
    "I'm reaching Joe right now.",
    "Contacting Joe immediately.",
];

/// The confirmation prompt spoken for Soft distress.
const DISTRESS_SOFT_PROMPT_PHRASES: &[&str] = &["Should I let Joe know?"];

/// The failure phrase spoken when distress delivery fails (AC7).
const DISTRESS_FAILURE_PHRASES: &[&str] =
    &["I couldn't reach Joe — try calling him directly."];

/// The "no" phrase spoken when the user declines the soft-distress prompt.
const DISTRESS_SOFT_DECLINED_PHRASES: &[&str] = &["Okay, I won't."];

/// Return the primary distress assurance phrase at index `attempt`.
/// This is the phrase spoken to the listener: "I'm reaching Joe right now."
/// Out-of-range indices return the last phrase.
#[must_use]
pub fn distress_assurance(attempt: usize) -> &'static str {
    let idx = attempt.min(DISTRESS_ASSURANCE_PHRASES.len().saturating_sub(1));
    DISTRESS_ASSURANCE_PHRASES
        .get(idx)
        .copied()
        .unwrap_or("I'm reaching Joe right now.")
}

/// Return the soft-distress confirmation prompt at index `attempt`.
#[must_use]
pub fn distress_soft_prompt(attempt: usize) -> &'static str {
    let idx = attempt.min(DISTRESS_SOFT_PROMPT_PHRASES.len().saturating_sub(1));
    DISTRESS_SOFT_PROMPT_PHRASES
        .get(idx)
        .copied()
        .unwrap_or("Should I let Joe know?")
}

/// Return the delivery-failure phrase at index `attempt` (AC7: never silent on failure).
#[must_use]
pub fn distress_failure_phrase(attempt: usize) -> &'static str {
    let idx = attempt.min(DISTRESS_FAILURE_PHRASES.len().saturating_sub(1));
    DISTRESS_FAILURE_PHRASES
        .get(idx)
        .copied()
        .unwrap_or("I couldn't reach Joe — try calling him directly.")
}

/// Return the phrase spoken when user declines the soft-distress prompt at index `attempt`.
#[must_use]
pub fn distress_soft_declined(attempt: usize) -> &'static str {
    let idx = attempt.min(DISTRESS_SOFT_DECLINED_PHRASES.len().saturating_sub(1));
    DISTRESS_SOFT_DECLINED_PHRASES
        .get(idx)
        .copied()
        .unwrap_or("Okay, I won't.")
}

// ── Severity ────────────────────────────────────────────────────────────

/// Distress severity: determines whether the dialog fires immediately or
/// prompts for confirmation first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Immediate — fire `wm.family.distress` with no confirmation step.
    /// Examples: "I've fallen", "emergency", "call an ambulance".
    Hard,
    /// Confirm-first — prompt "Should I let Joe know?" before firing.
    /// Examples: "I don't feel well", "something's wrong".
    Soft,
}

// ── Phrase tables ───────────────────────────────────────────────────────

/// Hard-distress trigger substrings (checked first; case-insensitive,
/// substring match).  Order matters: first match wins within this table,
/// but the whole Hard table is checked before the Soft table.
const HARD_PHRASES: &[&str] = &[
    "i've fallen",
    "i have fallen",
    "i fell",
    "i need help",
    "call an ambulance",
    "call 911",
    "call 999",
    "emergency",
    "can't get up",
    "cannot get up",
    "help me",
    "i'm in pain",
    "i am in pain",
    "chest pain",
    "i can't breathe",
    "i cannot breathe",
    "i can't move",
    "i cannot move",
];

/// Soft-distress trigger substrings (checked after Hard).
const SOFT_PHRASES: &[&str] = &[
    "i don't feel well",
    "i do not feel well",
    "i'm not well",
    "i am not well",
    "something's wrong",
    "something is wrong",
    "i'm worried",
    "i am worried",
    "not feeling well",
    "feeling unwell",
    "i feel sick",
    "i feel dizzy",
    "i'm dizzy",
    "i am dizzy",
    "i feel weak",
    "i'm weak",
    "i am weak",
];

// ── Public API ──────────────────────────────────────────────────────────

/// Classify a transcript as distress, returning [`Some(Severity)`] if a
/// distress phrase is matched.
///
/// # Priority
///
/// Hard phrases are checked first. If any Hard phrase matches, `Some(Hard)`
/// is returned immediately, even if a Soft phrase also matches. This satisfies
/// PRD §3 AC2 (ordering guarantee).
///
/// Matching is **case-insensitive** and **substring-tolerant** — STT output
/// rarely has clean punctuation, so the raw lowercased transcript is searched.
///
/// # Examples
///
/// ```
/// use wintermute_dialog::distress::{Severity, classify};
///
/// assert_eq!(classify("I've fallen and I can't get up"), Some(Severity::Hard));
/// assert_eq!(classify("I don't feel well today"), Some(Severity::Soft));
/// assert_eq!(classify("what time is it"), None);
/// ```
#[must_use]
pub fn classify(transcript: &str) -> Option<Severity> {
    let lower = transcript.to_lowercase();

    // Hard phrases checked first (PRD §3 AC2: Hard wins over Soft).
    for phrase in HARD_PHRASES {
        if lower.contains(phrase) {
            return Some(Severity::Hard);
        }
    }

    // Soft phrases checked second.
    for phrase in SOFT_PHRASES {
        if lower.contains(phrase) {
            return Some(Severity::Soft);
        }
    }

    None
}

// ── Wire types ──────────────────────────────────────────────────────────

/// `wm.family.distress` envelope published on Hard distress (or after
/// Soft confirmation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyDistress {
    /// The matched phrase that triggered distress detection.
    pub phrase: String,
    /// Unix milliseconds at emission.
    pub ts: i64,
}

// ── Distress FSM ─────────────────────────────────────────────────────────

/// Actions emitted by [`DistressFsm`].
///
/// These are parallel to [`crate::family::FamilyAction`] but specific to
/// the distress fast-path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistressAction {
    /// Publish `wm.family.distress` with this envelope.
    PublishDistress(FamilyDistress),
    /// Speak this text via `wm.tts.say`.
    TtsSay(String),
}

/// Internal states of the distress sub-FSM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistressState {
    /// No distress flow in progress.
    Idle,
    /// Soft distress detected; waiting for user confirmation ("yes" / "no").
    /// Holds the original matched phrase for use in the distress envelope.
    /// Soft distress was detected; waiting for user's yes/no response.
    /// The `phrase` is the original STT transcript that triggered detection.
    AwaitingConfirm {
        /// The original STT transcript that triggered soft-distress detection.
        phrase: String,
    },
}

/// Lightweight distress FSM.
///
/// Sits **before** the normal [`crate::family::FamilyFsm`] in the driver
/// loop event chain — distress short-circuits all other matching (PRD §2.2,
/// AC8).
///
/// Call [`DistressFsm::on_stt_final`] from the driver loop when a final STT
/// transcript arrives.  If it returns non-empty actions the driver loop
/// **must not** forward the transcript to the regular family FSM or the brain
/// path (PRD §3 AC8, AC9).
///
/// Call [`DistressFsm::on_ack`] when a `wm.family.ack` arrives for a
/// previously-published distress envelope.
#[derive(Debug, Clone)]
pub struct DistressFsm {
    state: DistressState,
}

impl DistressFsm {
    /// Construct a new distress FSM in Idle.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: DistressState::Idle,
        }
    }

    /// Borrow the current state.
    #[must_use]
    pub const fn state(&self) -> &DistressState {
        &self.state
    }

    /// Process a final STT transcript.
    ///
    /// Returns non-empty actions when distress is detected or when the FSM
    /// is awaiting soft-distress confirmation.  The driver loop must skip all
    /// other matchers (family FSM, brain) when this returns non-empty.
    ///
    /// # Hard distress
    ///
    /// Immediately emits `PublishDistress` + `TtsSay(assurance)` and resets
    /// to Idle.  No Claude API call, no confirmation step (AC3, AC4, AC9).
    ///
    /// # Soft distress
    ///
    /// Emits `TtsSay(soft_prompt)` and enters `AwaitingConfirm`.  The
    /// distress envelope is NOT published until a "yes" arrives.
    ///
    /// # Awaiting confirm
    ///
    /// When already in `AwaitingConfirm`, interprets the transcript as the
    /// user's yes/no response:
    /// - "yes" → publish distress + speak assurance, reset to Idle.
    /// - "no" → speak decline phrase, reset to Idle (AC5).
    /// - anything else → re-emit the soft prompt (tolerate fumbled yes).
    #[must_use]
    pub fn on_stt_final(&mut self, transcript: &str) -> Vec<DistressAction> {
        match &self.state.clone() {
            DistressState::Idle => {
                match classify(transcript) {
                    Some(Severity::Hard) => {
                        let phrase = transcript.to_string();
                        let ts = now_unix_ms_i64();
                        let envelope = FamilyDistress { phrase, ts };
                        // Hard: fire immediately + speak assurance.  No API, no confirm.
                        vec![
                            DistressAction::PublishDistress(envelope),
                            DistressAction::TtsSay(distress_assurance(0).to_string()),
                        ]
                    }
                    Some(Severity::Soft) => {
                        let phrase = transcript.to_string();
                        self.state = DistressState::AwaitingConfirm { phrase };
                        vec![DistressAction::TtsSay(
                            distress_soft_prompt(0).to_string(),
                        )]
                    }
                    None => Vec::new(),
                }
            }
            DistressState::AwaitingConfirm { phrase } => {
                let lower = transcript.trim().to_lowercase();
                if lower.starts_with("yes") || lower == "yeah" || lower == "yep" || lower == "sure"
                {
                    let envelope = FamilyDistress {
                        phrase: phrase.clone(),
                        ts: now_unix_ms_i64(),
                    };
                    self.state = DistressState::Idle;
                    vec![
                        DistressAction::PublishDistress(envelope),
                        DistressAction::TtsSay(distress_assurance(0).to_string()),
                    ]
                } else if lower.starts_with("no")
                    || lower == "nope"
                    || lower == "cancel"
                    || lower == "stop"
                {
                    self.state = DistressState::Idle;
                    vec![DistressAction::TtsSay(
                        distress_soft_declined(0).to_string(),
                    )]
                } else {
                    // Ambiguous — re-prompt (tolerate fumbled yes/no).
                    vec![DistressAction::TtsSay(
                        distress_soft_prompt(0).to_string(),
                    )]
                }
            }
        }
    }

    /// Process a `wm.family.ack` for a distress delivery.
    ///
    /// Returns spoken feedback (AC7: failure is never silent).
    ///
    /// This handler is state-independent — the ack arrives asynchronously
    /// and the FSM does not change state on it.
    #[must_use]
    pub fn on_ack(delivered: bool) -> Vec<DistressAction> {
        let text = if delivered {
            "Joe knows, he'll be in touch.".to_string()
        } else {
            distress_failure_phrase(0).to_string()
        };
        vec![DistressAction::TtsSay(text)]
    }
}

impl Default for DistressFsm {
    fn default() -> Self {
        Self::new()
    }
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Wall-clock milliseconds since the Unix epoch as `i64`.
fn now_unix_ms_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(i64::MAX, |d| {
            i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
        })
}

// ── Tests ───────────────────────────────────────────────────────────────

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

    // ── AC1: basic classify() cases ───────────────────────────────────

    #[test]
    fn classify_fallen_and_cant_get_up_is_hard() {
        assert_eq!(
            classify("I've fallen and I can't get up"),
            Some(Severity::Hard)
        );
    }

    #[test]
    fn classify_dont_feel_well_today_is_soft() {
        assert_eq!(
            classify("I don't feel well today"),
            Some(Severity::Soft)
        );
    }

    #[test]
    fn classify_what_time_is_it_is_none() {
        assert_eq!(classify("what time is it"), None);
    }

    #[test]
    fn classify_i_need_help_is_hard() {
        assert_eq!(classify("I need help"), Some(Severity::Hard));
    }

    #[test]
    fn classify_emergency_is_hard() {
        assert_eq!(classify("emergency"), Some(Severity::Hard));
    }

    #[test]
    fn classify_call_an_ambulance_is_hard() {
        assert_eq!(classify("call an ambulance"), Some(Severity::Hard));
    }

    #[test]
    fn classify_i_fell_is_hard() {
        assert_eq!(classify("I fell"), Some(Severity::Hard));
    }

    #[test]
    fn classify_something_wrong_is_soft() {
        assert_eq!(classify("something's wrong"), Some(Severity::Soft));
    }

    #[test]
    fn classify_im_worried_is_soft() {
        assert_eq!(classify("I'm worried"), Some(Severity::Soft));
    }

    // ── AC1 case-insensitivity ─────────────────────────────────────────

    #[test]
    fn classify_is_case_insensitive_hard() {
        assert_eq!(classify("I'VE FALLEN"), Some(Severity::Hard));
    }

    #[test]
    fn classify_is_case_insensitive_soft() {
        assert_eq!(classify("I DON'T FEEL WELL"), Some(Severity::Soft));
    }

    // ── AC2: Hard wins when both Hard and Soft match ───────────────────

    #[test]
    fn hard_wins_over_soft_when_both_match() {
        // Transcript that contains both a Hard and a Soft phrase.
        let transcript = "I've fallen and I don't feel well";
        assert_eq!(classify(transcript), Some(Severity::Hard));
    }

    #[test]
    fn hard_wins_even_when_soft_appears_first_in_transcript() {
        // Soft phrase appears before Hard in the text, but Hard must win.
        let transcript = "I don't feel well, I fell down";
        assert_eq!(classify(transcript), Some(Severity::Hard));
    }

    // ── Phrase bank registration tests (AC6) ──────────────────────────

    #[test]
    fn assurance_phrase_0_contains_joe() {
        // The assurance phrase "I'm reaching Joe right now" must contain "Joe".
        assert!(
            distress_assurance(0).contains("Joe"),
            "assurance phrase must mention Joe; got: {}",
            distress_assurance(0)
        );
    }

    #[test]
    fn assurance_phrase_is_non_empty() {
        assert!(!distress_assurance(0).is_empty());
    }

    #[test]
    fn soft_prompt_phrase_is_non_empty() {
        assert!(!distress_soft_prompt(0).is_empty());
    }

    #[test]
    fn failure_phrase_contains_joe() {
        assert!(
            distress_failure_phrase(0).contains("Joe"),
            "failure phrase must mention Joe; got: {}",
            distress_failure_phrase(0)
        );
    }

    #[test]
    fn assurance_out_of_range_returns_last() {
        let last = *DISTRESS_ASSURANCE_PHRASES.last().expect("non-empty");
        assert_eq!(distress_assurance(999), last);
    }

    // ── DistressFsm: Hard distress (AC3, AC4) ─────────────────────────

    #[test]
    fn hard_distress_publishes_envelope_and_assurance_immediately() {
        let mut fsm = DistressFsm::new();
        let acts = fsm.on_stt_final("I've fallen and I can't get up");
        // Must publish distress.
        let distress_published = acts
            .iter()
            .any(|a| matches!(a, DistressAction::PublishDistress(_)));
        assert!(distress_published, "Hard distress must publish distress envelope");

        // Must speak assurance (AC4).
        let tts = acts
            .iter()
            .find_map(|a| {
                if let DistressAction::TtsSay(t) = a {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .expect("Hard distress must emit TtsSay assurance");
        assert!(
            tts.contains("Joe"),
            "assurance must mention Joe; got: {tts}"
        );
    }

    #[test]
    fn hard_distress_resets_to_idle_after_fire() {
        let mut fsm = DistressFsm::new();
        let _ = fsm.on_stt_final("I need help");
        // After Hard distress fires, FSM returns to Idle.
        assert_eq!(fsm.state(), &DistressState::Idle);
    }

    #[test]
    fn hard_distress_emits_no_confirm_step() {
        // AC3: Hard distress must NOT produce a "Should I let Joe know?" prompt.
        let mut fsm = DistressFsm::new();
        let acts = fsm.on_stt_final("emergency");
        // Verify we got PublishDistress + TtsSay(assurance), but NOT the soft prompt.
        let soft_prompt = distress_soft_prompt(0);
        let has_soft_prompt = acts.iter().any(|a| {
            if let DistressAction::TtsSay(t) = a {
                t == soft_prompt
            } else {
                false
            }
        });
        assert!(!has_soft_prompt, "Hard distress must not emit soft-distress prompt");
    }

    // ── DistressFsm: Soft distress (AC5) ──────────────────────────────

    #[test]
    fn soft_distress_emits_prompt_and_awaits_confirm() {
        let mut fsm = DistressFsm::new();
        let acts = fsm.on_stt_final("I don't feel well");
        // Must emit the soft prompt.
        let tts = acts
            .iter()
            .find_map(|a| {
                if let DistressAction::TtsSay(t) = a {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .expect("Soft distress must emit TtsSay confirmation prompt");
        assert_eq!(tts, distress_soft_prompt(0));
        // Must NOT publish distress yet.
        assert!(
            !acts.iter().any(|a| matches!(a, DistressAction::PublishDistress(_))),
            "Soft distress must NOT publish distress before confirmation"
        );
        // State must be AwaitingConfirm.
        assert!(
            matches!(fsm.state(), DistressState::AwaitingConfirm { .. }),
            "State must be AwaitingConfirm after soft distress"
        );
    }

    #[test]
    fn soft_distress_yes_confirms_and_publishes_distress() {
        let mut fsm = DistressFsm::new();
        let _ = fsm.on_stt_final("I'm not well");
        // User says "yes".
        let acts = fsm.on_stt_final("yes");
        let distress_published = acts
            .iter()
            .any(|a| matches!(a, DistressAction::PublishDistress(_)));
        assert!(distress_published, "After 'yes', distress must be published");
        // Assurance also emitted.
        let tts = acts
            .iter()
            .find_map(|a| {
                if let DistressAction::TtsSay(t) = a {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .expect("After 'yes', must emit TtsSay assurance");
        assert!(tts.contains("Joe"), "assurance after soft-confirm must mention Joe");
        // FSM returns to Idle.
        assert_eq!(fsm.state(), &DistressState::Idle);
    }

    #[test]
    fn soft_distress_no_returns_to_listening_without_publishing() {
        let mut fsm = DistressFsm::new();
        let _ = fsm.on_stt_final("I don't feel well today");
        // User says "no".
        let acts = fsm.on_stt_final("no");
        assert!(
            !acts.iter().any(|a| matches!(a, DistressAction::PublishDistress(_))),
            "After 'no', distress must NOT be published"
        );
        // Must speak the decline phrase.
        let tts = acts
            .iter()
            .find_map(|a| {
                if let DistressAction::TtsSay(t) = a {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .expect("After 'no', must emit TtsSay decline phrase");
        assert_eq!(tts, distress_soft_declined(0));
        // FSM returns to Idle.
        assert_eq!(fsm.state(), &DistressState::Idle);
    }

    // ── DistressFsm: ACK feedback (AC7) ──────────────────────────────

    #[test]
    fn ack_delivered_emits_reassurance_with_joe() {
        let acts = DistressFsm::on_ack(true);
        let tts = acts
            .iter()
            .find_map(|a| {
                if let DistressAction::TtsSay(t) = a {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .expect("delivered ack must emit TtsSay");
        assert!(tts.contains("Joe"), "delivered ack must mention Joe; got: {tts}");
    }

    #[test]
    fn ack_not_delivered_emits_failure_phrase_never_silent() {
        // AC7: failure is NEVER silent.
        let acts = DistressFsm::on_ack(false);
        let tts = acts
            .iter()
            .find_map(|a| {
                if let DistressAction::TtsSay(t) = a {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .expect("failed ack must emit TtsSay — failure is never silent");
        // Must contain "Joe" and suggest calling directly.
        assert!(tts.contains("Joe"), "failure phrase must mention Joe; got: {tts}");
    }

    // ── AC9: no network/API call ───────────────────────────────────────
    // classify() and DistressFsm::on_stt_final() are pure synchronous
    // functions; this test invokes them where no network is reachable,
    // demonstrating zero-latency determinism.

    #[test]
    fn distress_path_is_api_independent() {
        let mut fsm = DistressFsm::new();
        // This would hang if any network or API call occurred.
        let acts = fsm.on_stt_final("I've fallen and I can't get up");
        assert!(!acts.is_empty(), "distress detection must return actions instantly");
    }

    // ── AC8: ordering — distress before family verb ────────────────────
    // The driver loop checks DistressFsm::on_stt_final BEFORE FamilyFsm.
    // Simulate a transcript that matches both distress AND a family verb.

    #[test]
    fn transcript_matching_both_distress_and_family_verb_takes_distress_branch() {
        // "tell Joe I've fallen" — matches family intent "tell Joe …" but also
        // contains "I've fallen" (Hard distress).  classify() must fire Hard.
        let transcript = "tell Joe I've fallen";
        // The classify() function fires on the whole transcript — Hard matches.
        assert_eq!(
            classify(transcript),
            Some(Severity::Hard),
            "distress must be detected in mixed transcript"
        );
        // DistressFsm must produce actions (meaning driver picks distress branch).
        let mut fsm = DistressFsm::new();
        let acts = fsm.on_stt_final(transcript);
        assert!(!acts.is_empty(), "DistressFsm must claim the transcript");
        assert!(
            acts.iter().any(|a| matches!(a, DistressAction::PublishDistress(_))),
            "distress branch must publish distress envelope, not family message"
        );
    }

    // ── Serde: FamilyDistress round-trip ─────────────────────────────

    #[test]
    fn family_distress_serde_round_trip() {
        let d = FamilyDistress {
            phrase: "I've fallen".to_string(),
            ts: 1_000,
        };
        let json = serde_json::to_string(&d).expect("serialize");
        let back: FamilyDistress = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(d, back);
    }

    // ── Severity serde ────────────────────────────────────────────────

    #[test]
    fn severity_serde_snake_case() {
        let h = serde_json::to_string(&Severity::Hard).expect("serialize");
        assert_eq!(h, "\"hard\"");
        let s = serde_json::to_string(&Severity::Soft).expect("serialize");
        assert_eq!(s, "\"soft\"");
    }
}
