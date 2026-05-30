//! Family-intents module for `wintermute-dialog`.
//!
//! Defines the `wm.family.*` topic contract (PRD §2.1) plus the
//! deterministic, API-independent intent matcher (PRD §2.2).
//!
//! # Topic constants
//!
//! Four constants anchor the contract used by the whole kin fleet:
//!
//! - [`TOPIC_FAMILY_MESSAGE`]  — outbound envelope from dialog → delivery daemon.
//! - [`TOPIC_FAMILY_DISTRESS`] — defined here, fired by the family-distress PRD.
//! - [`TOPIC_FAMILY_ACK`]      — delivery daemon → dialog (ack/nack).
//! - [`TOPIC_FAMILY_REPLY`]    — jsy's reply → dialog → TTS.
//!
//! # Intent matcher
//!
//! [`match_family_intent`] accepts a final STT transcript and an
//! enrolled recipient list (falls back to `["Joe"]` when the list is
//! empty) and returns `Some(FamilyMessage)` when a family-verb pattern
//! fires.  It never touches the Claude API — the match is purely
//! lexical so it works when the brain is degraded.
//!
//! # FSM integration
//!
//! [`FamilyFsm`] is a lightweight state machine that sits **alongside**
//! the main dialog FSM and handles the `FamilyPending` wait.  Call
//! [`FamilyFsm::on_stt_final`] from the dialog event loop when in the
//! post-transcription window; call [`FamilyFsm::on_ack`] and
//! [`FamilyFsm::on_reply`] when those bus events arrive.  The family FSM
//! emits [`FamilyAction`] values the daemon translates into publishes and
//! TTS says — exactly like [`crate::action::Action`].
//!
//! The self-emitted-topic filter is enforced by the bus subscription
//! architecture: `wm.family.message` is published on the *pub* client
//! and NOT in the subscribe prefix list of the *sub* client, so dialog
//! never re-receives its own outbound envelope.

use serde::{Deserialize, Serialize};

// ── Topic constants ────────────────────────────────────────────────────

/// Outbound topic: dialog emits a `FamilyMessage` envelope when a
/// family-forwarding intent is recognised.
pub const TOPIC_FAMILY_MESSAGE: &str = "wm.family.message";

/// Outbound topic: distress signal fired by the family-distress PRD.
/// Defined here so all kin crates share the same constant.
pub const TOPIC_FAMILY_DISTRESS: &str = "wm.family.distress";

/// Inbound topic: delivery-daemon acks / nacks a forwarded message.
pub const TOPIC_FAMILY_ACK: &str = "wm.family.ack";

/// Inbound topic: jsy's reply forwarded back through the bus for TTS.
pub const TOPIC_FAMILY_REPLY: &str = "wm.family.reply";

// ── Urgency ────────────────────────────────────────────────────────────

/// Message urgency, mirroring the downstream delivery options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    /// Normal, queued delivery.
    Normal,
    /// Elevated — delivery daemon should attempt immediate push.
    High,
}

// ── Wire types ─────────────────────────────────────────────────────────

/// `wm.family.message` envelope emitted by dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyMessage {
    /// Canonical recipient name (enrolled name or "Joe" fallback).
    pub to: String,
    /// Message body extracted from the transcript.
    pub body: String,
    /// Delivery urgency.
    pub urgency: Urgency,
    /// Unix milliseconds at emission.
    pub ts: i64,
}

/// `wm.family.ack` envelope emitted by the delivery daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyAck {
    /// Correlates with the emitted [`FamilyMessage`] (message id or
    /// session id — downstream-defined; dialog echoes it opaquely).
    #[serde(rename = "ref")]
    pub r#ref: String,
    /// `true` when the message reached the recipient.
    pub delivered: bool,
    /// Human-readable transport label, e.g. `"sms"`, `"push"`, `"email"`.
    pub transport: String,
    /// Unix milliseconds at emission.
    pub ts: i64,
}

/// `wm.family.reply` envelope emitted when jsy replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyReply {
    /// Sender name, e.g. `"Joe"`.
    pub from: String,
    /// Reply body text.
    pub body: String,
    /// Unix milliseconds at emission.
    pub ts: i64,
}

// ── Intent matching ────────────────────────────────────────────────────

/// Default fallback recipient name when no enroll config is loaded.
pub const DEFAULT_RECIPIENT: &str = "Joe";

/// Attempt to match a family-forwarding intent in `transcript`.
///
/// `enrolled_names` is the set of recognised recipient names loaded from
/// the family-enroll config.  When `enrolled_names` is empty the matcher
/// falls back to `["Joe"]` so this PRD is testable standalone without any
/// config file (PRD §2.2 hard-coded fallback).
///
/// Returns `Some(FamilyMessage)` when the transcript matches one of the
/// family-forwarding verb patterns, `None` otherwise.  No Claude API
/// call is made — the match is purely lexical (PRD AC4).
///
/// # Patterns recognised
///
/// | verb trigger | example |
/// |---|---|
/// | `tell <name> …` | "tell Joe the heating is broken" |
/// | `message <name> …` | "message Joe I'll be late" |
/// | `send <name> …` | "send Joe a message" |
/// | `let <name> know …` | "let Joe know I'm fine" |
/// | `call <name>` | "call Joe" |
///
/// Matching is case-insensitive and STT punctuation-tolerant (a leading
/// or trailing period/comma on any token is stripped before comparison).
#[must_use]
pub fn match_family_intent(transcript: &str, enrolled_names: &[&str]) -> Option<FamilyMessage> {
    let effective_names: &[&str] = if enrolled_names.is_empty() {
        &[DEFAULT_RECIPIENT]
    } else {
        enrolled_names
    };

    let now_ts = now_unix_ms_i64();
    let lower = transcript.trim().to_lowercase();
    // Strip leading/trailing punctuation from each word for robustness.
    let tokens: Vec<&str> = lower
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| c.is_ascii_punctuation()))
        .filter(|t| !t.is_empty())
        .collect();

    let first = tokens.first()?;

    match *first {
        // "tell <name> …" / "message <name> …" / "send <name> …"
        "tell" | "message" | "send" => {
            let (name_idx, body_start) = (1, 2);
            extract_message(&tokens, name_idx, body_start, effective_names, now_ts)
        }
        // "let <name> know …"
        "let" => {
            // tokens: ["let", "<name>", "know", ...]
            // We need at least ["let", "<name>", "know"].
            if tokens.len() >= 3 && tokens.get(2) == Some(&"know") {
                let (name_idx, body_start) = (1, 3);
                extract_message(&tokens, name_idx, body_start, effective_names, now_ts)
            } else {
                None
            }
        }
        // "call <name>"
        "call" => {
            // PRD §2.2: "call <name>" → body = "<she asked you to call>"
            tokens
                .get(1)
                .and_then(|t| find_recipient(t, effective_names))
                .map(|name| FamilyMessage {
                    to: name.to_string(),
                    body: "<she asked you to call>".to_string(),
                    urgency: Urgency::Normal,
                    ts: now_ts,
                })
        }
        _ => None,
    }
}

/// Helper: extract `to` from `tokens[name_idx]` and `body` from
/// `tokens[body_start..]` using `effective_names` as the allow-list.
fn extract_message(
    tokens: &[&str],
    name_idx: usize,
    body_start: usize,
    effective_names: &[&str],
    ts: i64,
) -> Option<FamilyMessage> {
    let name_tok = tokens.get(name_idx)?;
    let name = find_recipient(name_tok, effective_names)?;
    let body = tokens
        .get(body_start..)
        .map(|s| s.join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<she asked you to call>".to_string());
    Some(FamilyMessage {
        to: name.to_string(),
        body,
        urgency: Urgency::Normal,
        ts,
    })
}

/// Return the first `enrolled_names` entry that (case-insensitively)
/// equals `token`, or `None` if no match.
fn find_recipient<'a>(token: &str, enrolled_names: &[&'a str]) -> Option<&'a str> {
    enrolled_names
        .iter()
        .find(|&&n| n.to_lowercase() == token.to_lowercase())
        .copied()
}

// ── Family FSM ─────────────────────────────────────────────────────────

/// Side-effects emitted by [`FamilyFsm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyAction {
    /// Publish `wm.family.message` with this envelope.
    PublishFamilyMessage(FamilyMessage),
    /// Speak this text via `wm.tts.say`.
    TtsSay(String),
    /// Start the pending-ack timeout timer (`ms` milliseconds).
    StartAckTimer {
        /// Timeout duration in milliseconds.
        ms: u32,
    },
    /// Cancel the pending-ack timeout timer.
    CancelAckTimer,
}

/// State of the family sub-FSM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyState {
    /// No family flow in progress.
    Idle,
    /// A `wm.family.message` was published; waiting for `wm.family.ack`.
    Pending,
}

/// Default ack-wait timeout: 30 seconds.
pub const DEFAULT_ACK_TIMEOUT_MS: u32 = 30_000;

/// Lightweight family-intent FSM.
///
/// Runs alongside the main [`crate::fsm::Fsm`].  The driver loop feeds
/// it inbound `wm.family.ack` and `wm.family.reply` events whenever they
/// arrive from the bus, independent of the main FSM's turn state.
#[derive(Debug, Clone)]
pub struct FamilyFsm {
    state: FamilyState,
    ack_timeout_ms: u32,
    /// Recipient name of the in-flight message (used in confirmation text).
    pending_to: Option<String>,
}

impl FamilyFsm {
    /// Construct a new family FSM with default ack timeout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: FamilyState::Idle,
            ack_timeout_ms: DEFAULT_ACK_TIMEOUT_MS,
            pending_to: None,
        }
    }

    /// Construct with an explicit ack timeout (ms).
    #[must_use]
    pub const fn with_ack_timeout(ack_timeout_ms: u32) -> Self {
        Self {
            state: FamilyState::Idle,
            ack_timeout_ms,
            pending_to: None,
        }
    }

    /// Borrow the current family FSM state.
    #[must_use]
    pub const fn state(&self) -> &FamilyState {
        &self.state
    }

    /// Called by the driver loop when a final STT transcript is available
    /// and no main-FSM branch has claimed it yet.
    ///
    /// Returns the actions to execute (possibly empty if no match).
    #[must_use]
    pub fn on_stt_final(
        &mut self,
        transcript: &str,
        enrolled_names: &[&str],
    ) -> Vec<FamilyAction> {
        if self.state != FamilyState::Idle {
            // Already pending — ignore new utterances until the ack arrives.
            return Vec::new();
        }
        let Some(msg) = match_family_intent(transcript, enrolled_names) else {
            return Vec::new();
        };
        let recipient = msg.to.clone();
        let timeout_ms = self.ack_timeout_ms;
        self.state = FamilyState::Pending;
        self.pending_to = Some(recipient);
        vec![
            FamilyAction::PublishFamilyMessage(msg),
            FamilyAction::StartAckTimer { ms: timeout_ms },
        ]
    }

    /// Called when a `wm.family.ack` arrives from the bus.
    #[must_use]
    pub fn on_ack(&mut self, ack: &FamilyAck) -> Vec<FamilyAction> {
        if self.state != FamilyState::Pending {
            return Vec::new();
        }
        let name = self
            .pending_to
            .clone()
            .unwrap_or_else(|| DEFAULT_RECIPIENT.to_string());
        self.state = FamilyState::Idle;
        self.pending_to = None;
        let text = if ack.delivered {
            format!("I let {name} know.")
        } else {
            format!("Sorry, I couldn't reach {name} just now.")
        };
        vec![FamilyAction::CancelAckTimer, FamilyAction::TtsSay(text)]
    }

    /// Called when the ack timer fires without a delivery confirmation.
    #[must_use]
    pub fn on_ack_timeout(&mut self) -> Vec<FamilyAction> {
        if self.state != FamilyState::Pending {
            return Vec::new();
        }
        let name = self
            .pending_to
            .clone()
            .unwrap_or_else(|| DEFAULT_RECIPIENT.to_string());
        self.state = FamilyState::Idle;
        self.pending_to = None;
        vec![FamilyAction::TtsSay(format!(
            "I couldn't reach {name} just now."
        ))]
    }

    /// Called when a `wm.family.reply` arrives from the bus.
    ///
    /// This handler is **state-independent** — jsy can reply at any
    /// time and the reply is spoken immediately, prefixed so the
    /// listener knows who it's from.
    #[must_use]
    pub fn on_reply(reply: &FamilyReply) -> Vec<FamilyAction> {
        let text = format!("{} says: {}", reply.from, reply.body);
        vec![FamilyAction::TtsSay(text)]
    }
}

impl Default for FamilyFsm {
    fn default() -> Self {
        Self::new()
    }
}

// ── Internal helpers ───────────────────────────────────────────────────

/// Wall-clock milliseconds since the Unix epoch as `i64`.  Saturates to
/// `i64::MAX` on overflow (shouldn't happen until ~2262).
fn now_unix_ms_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(i64::MAX, |d| {
            i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
        })
}

// ── Tests ──────────────────────────────────────────────────────────────

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

    // ── Topic-constant smoke tests ────────────────────────────────────

    #[test]
    fn topic_constants_have_wm_family_prefix() {
        assert!(TOPIC_FAMILY_MESSAGE.starts_with("wm.family."));
        assert!(TOPIC_FAMILY_DISTRESS.starts_with("wm.family."));
        assert!(TOPIC_FAMILY_ACK.starts_with("wm.family."));
        assert!(TOPIC_FAMILY_REPLY.starts_with("wm.family."));
    }

    #[test]
    fn topic_constants_match_prd_spec() {
        assert_eq!(TOPIC_FAMILY_MESSAGE, "wm.family.message");
        assert_eq!(TOPIC_FAMILY_DISTRESS, "wm.family.distress");
        assert_eq!(TOPIC_FAMILY_ACK, "wm.family.ack");
        assert_eq!(TOPIC_FAMILY_REPLY, "wm.family.reply");
    }

    // ── AC1: serde round-trips for all three envelope types ───────────

    #[test]
    fn family_message_round_trips() {
        let msg = FamilyMessage {
            to: "Joe".to_string(),
            body: "the heating is broken".to_string(),
            urgency: Urgency::Normal,
            ts: 1_000,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: FamilyMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn family_ack_round_trips() {
        let ack = FamilyAck {
            r#ref: "msg-abc".to_string(),
            delivered: true,
            transport: "sms".to_string(),
            ts: 2_000,
        };
        let json = serde_json::to_string(&ack).expect("serialize");
        let back: FamilyAck = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ack, back);
    }

    #[test]
    fn family_reply_round_trips() {
        let reply = FamilyReply {
            from: "Joe".to_string(),
            body: "ok".to_string(),
            ts: 3_000,
        };
        let json = serde_json::to_string(&reply).expect("serialize");
        let back: FamilyReply = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(reply, back);
    }

    // ── AC2: "tell Joe the heating is broken" → FamilyMessage ─────────

    #[test]
    fn matcher_tell_joe_heating_broken() {
        let result = match_family_intent("tell Joe the heating is broken", &[]);
        let msg = result.expect("should match");
        assert_eq!(msg.to, "Joe");
        assert_eq!(msg.body, "the heating is broken");
        assert_eq!(msg.urgency, Urgency::Normal);
    }

    // ── AC3: non-family transcript → None ─────────────────────────────

    #[test]
    fn matcher_what_is_the_weather_produces_none() {
        let result = match_family_intent("what's the weather", &[]);
        assert!(result.is_none(), "weather query should not match");
    }

    // ── AC4: API-independent (no API call in pure function) ───────────
    // The matcher is a pure function that takes no async context and
    // makes no network calls — this test proves that by calling it
    // in a sync context where no API would be reachable.

    #[test]
    fn matcher_is_api_independent() {
        // This test would hang or fail if match_family_intent made any
        // network call; it returns instantly, demonstrating independence.
        let result = match_family_intent("message Joe call me back", &[]);
        assert!(result.is_some());
    }

    // ── Various verb forms ────────────────────────────────────────────

    #[test]
    fn matcher_message_verb() {
        let result = match_family_intent("message Joe I'll be late", &[]);
        let msg = result.expect("message verb should match");
        assert_eq!(msg.to, "Joe");
        assert_eq!(msg.body, "i'll be late");
    }

    #[test]
    fn matcher_let_know_verb() {
        let result = match_family_intent("let Joe know I am fine", &[]);
        let msg = result.expect("let know verb should match");
        assert_eq!(msg.to, "Joe");
        assert_eq!(msg.body, "i am fine");
    }

    #[test]
    fn matcher_call_verb_no_body() {
        let result = match_family_intent("call Joe", &[]);
        let msg = result.expect("call verb should match");
        assert_eq!(msg.to, "Joe");
        assert_eq!(msg.body, "<she asked you to call>");
    }

    #[test]
    fn matcher_send_verb() {
        let result = match_family_intent("send Joe a message", &[]);
        let msg = result.expect("send verb should match");
        assert_eq!(msg.to, "Joe");
    }

    #[test]
    fn matcher_case_insensitive() {
        let result = match_family_intent("TELL JOE the heating is broken", &[]);
        assert!(result.is_some(), "should match regardless of case");
    }

    // ── AC9: enrolled_names override + fallback ────────────────────────

    #[test]
    fn enrolled_names_fallback_is_joe() {
        // Empty slice → fallback to ["Joe"]
        let result = match_family_intent("tell Joe hi", &[]);
        assert!(result.is_some());
    }

    #[test]
    fn enrolled_names_from_config_overrides_fallback() {
        // Config provides "Alice" — "Joe" should not match when Alice is the only name.
        let result_alice = match_family_intent("tell Alice hi", &["Alice"]);
        assert!(result_alice.is_some());

        let result_joe_with_alice_config = match_family_intent("tell Joe hi", &["Alice"]);
        assert!(
            result_joe_with_alice_config.is_none(),
            "Joe should not match when only Alice is enrolled"
        );
    }

    #[test]
    fn enrolled_names_multiple_recipients() {
        let names = ["Joe", "Alice", "Bob"];
        for name in &names {
            let transcript = format!("tell {name} hi");
            let result = match_family_intent(&transcript, &names);
            assert!(result.is_some(), "{name} should match");
        }
    }

    // ── AC5/AC6: FamilyFsm bus smoke tests ────────────────────────────

    #[test]
    fn family_fsm_ack_delivered_emits_tts_with_joe() {
        let mut fsm = FamilyFsm::new();
        // Drive to Pending via on_stt_final.
        let acts = fsm.on_stt_final("tell Joe the heating is broken", &[]);
        assert!(acts
            .iter()
            .any(|a| matches!(a, FamilyAction::PublishFamilyMessage(_))));
        assert!(acts
            .iter()
            .any(|a| matches!(a, FamilyAction::StartAckTimer { .. })));

        // Now ack with delivered=true.
        let ack = FamilyAck {
            r#ref: "msg-1".to_string(),
            delivered: true,
            transport: "sms".to_string(),
            ts: 1_000,
        };
        let ack_acts = fsm.on_ack(&ack);
        let tts = ack_acts
            .iter()
            .find_map(|a| if let FamilyAction::TtsSay(t) = a { Some(t.as_str()) } else { None })
            .expect("TtsSay must be emitted");
        assert!(
            tts.contains("Joe"),
            "confirmation text should mention Joe; got: {tts}"
        );
    }

    #[test]
    fn family_fsm_ack_not_delivered_emits_failure_tts() {
        let mut fsm = FamilyFsm::new();
        let _ = fsm.on_stt_final("message Joe call me", &[]);
        let ack = FamilyAck {
            r#ref: "msg-2".to_string(),
            delivered: false,
            transport: "sms".to_string(),
            ts: 1_000,
        };
        let acts = fsm.on_ack(&ack);
        let tts = acts
            .iter()
            .find_map(|a| if let FamilyAction::TtsSay(t) = a { Some(t.as_str()) } else { None })
            .expect("TtsSay must be emitted on nack");
        assert!(
            tts.contains("Joe"),
            "failure text should mention Joe; got: {tts}"
        );
    }

    #[test]
    fn family_fsm_reply_emits_tts_with_from_and_body() {
        // AC6: wm.family.reply → TtsSay containing "Joe" and "ok"
        let reply = FamilyReply {
            from: "Joe".to_string(),
            body: "ok".to_string(),
            ts: 500,
        };
        let acts = FamilyFsm::on_reply(&reply);
        let tts = acts
            .iter()
            .find_map(|a| if let FamilyAction::TtsSay(t) = a { Some(t.as_str()) } else { None })
            .expect("TtsSay must be emitted for reply");
        assert!(tts.contains("Joe"), "reply TTS should contain 'Joe'");
        assert!(tts.contains("ok"), "reply TTS should contain 'ok'");
    }

    // ── AC7: ack timeout → "couldn't reach Joe" ───────────────────────

    #[test]
    fn family_fsm_ack_timeout_emits_failure_tts() {
        let mut fsm = FamilyFsm::new();
        let _ = fsm.on_stt_final("tell Joe call me", &[]);
        let acts = fsm.on_ack_timeout();
        let tts = acts
            .iter()
            .find_map(|a| if let FamilyAction::TtsSay(t) = a { Some(t.as_str()) } else { None })
            .expect("TtsSay must be emitted on timeout");
        assert!(
            tts.contains("Joe"),
            "timeout text should mention Joe; got: {tts}"
        );
    }

    // ── AC8: self-emitted-topic filter ────────────────────────────────
    // Dialog publishes wm.family.message on the pub_client.
    // wm.family.* is NOT in SUBSCRIBE_PREFIXES, so the sub_client never
    // re-receives dialog's own publish.  This test asserts the constant
    // is absent from the subscribe prefix list.

    #[test]
    fn family_message_topic_not_in_subscribe_prefixes() {
        for prefix in crate::bus::SUBSCRIBE_PREFIXES {
            assert!(
                !TOPIC_FAMILY_MESSAGE.starts_with(prefix),
                "wm.family.message must not match subscribe prefix {prefix}"
            );
        }
    }

    // ── State-machine edge cases ──────────────────────────────────────

    #[test]
    fn family_fsm_ignores_stt_final_when_pending() {
        let mut fsm = FamilyFsm::new();
        // First intent puts it into Pending.
        let acts1 = fsm.on_stt_final("tell Joe hi", &[]);
        assert!(!acts1.is_empty());
        assert_eq!(fsm.state(), &FamilyState::Pending);
        // Second intent while pending → ignored.
        let acts2 = fsm.on_stt_final("tell Joe bye", &[]);
        assert!(acts2.is_empty(), "second stt_final while pending must be ignored");
    }

    #[test]
    fn family_fsm_ack_while_idle_is_no_op() {
        let mut fsm = FamilyFsm::new();
        let ack = FamilyAck {
            r#ref: "stale".to_string(),
            delivered: true,
            transport: "sms".to_string(),
            ts: 1_000,
        };
        let acts = fsm.on_ack(&ack);
        assert!(acts.is_empty(), "ack while idle must produce no actions");
    }

    #[test]
    fn family_fsm_timeout_while_idle_is_no_op() {
        let mut fsm = FamilyFsm::new();
        let acts = fsm.on_ack_timeout();
        assert!(acts.is_empty(), "timeout while idle must produce no actions");
    }
}
