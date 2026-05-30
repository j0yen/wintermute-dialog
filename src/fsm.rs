//! Pure-data conversational state machine.
//!
//! [`Fsm::handle`] takes one [`Event`] + the current monotonic
//! millisecond clock and returns the side-effects the driver loop
//! should emit, mutating internal state in place. No I/O, no async —
//! that lives in iter-3's bus wiring.
//!
//! Time is injected (`now_ms`) so tests are fully deterministic; the
//! production daemon will feed `Instant::now()` through a thin shim.
//!
//! History is a bounded ring of [`Transition`] entries; the default
//! capacity is [`DEFAULT_HISTORY_CAPACITY`] (PRD §2.5 / intent-card
//! `history_ring_size` = 256). [`Fsm::history`] returns the last `N`
//! in chronological order (oldest → newest).
//!
//! Timing constants are now **configuration-sourced** (earshot-dialog-timing):
//! the FSM stores a [`crate::config::DialogTimingConfig`] and reads
//! deadlines from it at runtime.  The compile-time constants
//! [`CONFIRM_TIMEOUT_MS`] and [`MAX_REPROMPTS`] are kept for
//! backward-compatibility and as the `Default` source-of-truth reference,
//! but the transition code no longer reads them directly.
//!
//! earshot-gentle-reprompt: `ConfirmTimeout` now drives a multi-attempt
//! patience sequence before returning to Idle.  Each intermediate timeout
//! emits a warm check-in phrase (from [`crate::silence`]) and restarts
//! the confirm timer.  The final timeout emits a spoken close line before
//! the `Confirming → Idle` transition so the return-to-idle is announced,
//! not silent.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::action::{Action, DenyReason};
use crate::config::DialogTimingConfig;
use crate::event::{Event, EventTag};
use crate::silence::{silence_close, silence_reprompt};
use crate::state::{ConfirmContext, Flags, State, StateTag};

/// Default history-ring capacity. PRD §2.5 / intent-card `history_ring_size`.
pub const DEFAULT_HISTORY_CAPACITY: usize = 256;

/// Compile-time reference value for the verbal-confirm timeout (30 s).
/// Kept for backward-compatibility; the FSM reads the runtime value from
/// [`DialogTimingConfig::confirm_timeout_ms`].
pub const CONFIRM_TIMEOUT_MS: u32 = crate::config::LEGACY_CONFIRM_TIMEOUT_MS;

/// Compile-time reference value for the maximum re-prompt count.
/// Kept for backward-compatibility; the FSM reads the runtime value from
/// [`DialogTimingConfig::max_reprompts`].
pub const MAX_REPROMPTS: u8 = crate::config::LEGACY_MAX_REPROMPTS;

/// One historical transition entry. Returned by [`Fsm::history`] and
/// the (future) `wm-dialog state --history N` CLI surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    /// State before the transition.
    pub prior: StateTag,
    /// State after the transition.
    pub next: StateTag,
    /// Event tag that triggered the transition.
    pub trigger: EventTag,
    /// Monotonic ms at which the transition fired.
    pub at_ms: u64,
    /// Wall-clock ms spent in `prior` before transitioning.
    pub elapsed_ms: u64,
}

/// Conversational state machine — see crate docs for the diagram.
#[derive(Debug, Clone)]
pub struct Fsm {
    state: State,
    flags: Flags,
    last_change_ms: u64,
    history: VecDeque<Transition>,
    history_cap: usize,
    /// Runtime timing configuration. All deadline values are read from
    /// here; no hard-coded constants remain in the transition code.
    timing: DialogTimingConfig,
}

impl Fsm {
    /// Construct a fresh FSM in `Idle` with default flags, default
    /// history capacity, and **elder-friendly** timing defaults.
    /// `now_ms` is the FSM's epoch — `since_ms` values are computed
    /// against this until the first transition.
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self::with_timing(now_ms, DialogTimingConfig::default())
    }

    /// Construct an FSM with explicit timing configuration.  Use this
    /// when loading timing from a `[timing]` config table.
    #[must_use]
    pub const fn with_timing(now_ms: u64, timing: DialogTimingConfig) -> Self {
        Self {
            state: State::Idle,
            flags: Flags {
                muted: false,
                child_locked: false,
            },
            last_change_ms: now_ms,
            history: VecDeque::new(),
            history_cap: DEFAULT_HISTORY_CAPACITY,
            timing,
        }
    }

    /// Construct an FSM with a non-default history capacity and
    /// elder-friendly timing defaults.
    #[must_use]
    pub fn with_history_capacity(now_ms: u64, cap: usize) -> Self {
        let mut fsm = Self::new(now_ms);
        fsm.history_cap = cap.max(1);
        fsm
    }

    /// Construct an FSM with both a non-default history capacity and
    /// explicit timing configuration.
    #[must_use]
    pub fn with_history_capacity_and_timing(
        now_ms: u64,
        cap: usize,
        timing: DialogTimingConfig,
    ) -> Self {
        let mut fsm = Self::with_timing(now_ms, timing);
        fsm.history_cap = cap.max(1);
        fsm
    }

    /// Borrow the timing configuration this FSM was constructed with.
    #[must_use]
    pub const fn timing(&self) -> &DialogTimingConfig {
        &self.timing
    }

    /// Borrow the current state node.
    #[must_use]
    pub const fn state(&self) -> &State {
        &self.state
    }

    /// Snapshot of the orthogonal flags.
    #[must_use]
    pub const fn flags(&self) -> Flags {
        self.flags
    }

    /// Monotonic-ish timestamp (ms) of the most recent state transition.
    /// `now_ms - last_change_ms()` is the canonical `since_ms` used in
    /// the public `StateReport` / `StateSnapshot`.
    #[must_use]
    pub const fn last_change_ms(&self) -> u64 {
        self.last_change_ms
    }

    /// Most-recent `n` transitions in chronological order. If `n`
    /// exceeds the ring's length, returns the whole ring.
    #[must_use]
    pub fn history(&self, n: usize) -> Vec<Transition> {
        let len = self.history.len();
        let take = n.min(len);
        let skip = len.saturating_sub(take);
        self.history.iter().skip(skip).copied().collect()
    }

    /// Handle one event. Mutates state in place; returns the
    /// side-effects the driver loop should publish.
    ///
    /// `now_ms` is the monotonic clock at event arrival.
    #[allow(
        clippy::too_many_lines,
        reason = "single-place transition table; splitting hurts readability"
    )]
    pub fn handle(&mut self, event: Event, now_ms: u64) -> Vec<Action> {
        // Mute gates wake + stt.final outright (silence the FSM rather
        // than half-process). MuteRequest / UnmuteRequest / ChildLock
        // toggles always pass through so the user can recover.
        if self.flags.muted {
            match &event {
                Event::AudioWake | Event::SttFinal { .. } | Event::SttPartial => {
                    return Vec::new();
                }
                _ => {}
            }
        }

        match (&self.state, event) {
            // ── flag toggles (state-independent) ─────────────────────
            (_, Event::MuteRequest) => self.do_mute(now_ms),
            (_, Event::UnmuteRequest) => self.do_unmute(now_ms),
            (_, Event::SetChildLock { enabled }) => {
                self.flags.child_locked = enabled;
                Vec::new()
            }

            // ── idle ────────────────────────────────────────────────
            (State::Idle, Event::AudioWake) => {
                self.transition_to(State::Listening, EventTag::AudioWake, now_ms)
            }

            // ── listening ───────────────────────────────────────────
            (State::Listening, Event::AudioSpeechStart) => {
                self.transition_to(State::Transcribing, EventTag::AudioSpeechStart, now_ms)
            }
            (State::Listening | State::Transcribing, Event::SttUncertain) => {
                let mut acts = vec![Action::PublishTtsSay {
                    text: "Sorry, could you repeat that?".to_string(),
                }];
                acts.extend(self.transition_to(
                    State::Listening,
                    EventTag::SttUncertain,
                    now_ms,
                ));
                acts
            }

            // ── transcribing ────────────────────────────────────────
            (
                State::Transcribing,
                Event::SttFinal {
                    transcript,
                    confidence,
                },
            ) => {
                let mut acts = vec![
                    Action::PublishTurnUser {
                        transcript: transcript.clone(),
                        confidence,
                    },
                    Action::PublishBrainUtterance {
                        transcript,
                        confidence,
                    },
                ];
                acts.extend(self.transition_to(
                    State::Thinking,
                    EventTag::SttFinal,
                    now_ms,
                ));
                acts
            }
            // ── thinking ────────────────────────────────────────────
            (State::Thinking, Event::BrainReply { text }) => {
                let mut acts = vec![
                    Action::PublishTurnSystem { text: text.clone() },
                    Action::PublishTtsSay { text },
                ];
                acts.extend(self.transition_to(
                    State::Speaking,
                    EventTag::BrainReply,
                    now_ms,
                ));
                acts
            }
            (
                State::Thinking,
                Event::BrainReplyDestructive {
                    intent_id,
                    summary,
                    confirm_keyword,
                },
            ) => self.enter_confirm_or_deny(intent_id, summary, confirm_keyword, now_ms),

            // ── speaking ────────────────────────────────────────────
            (State::Speaking, Event::TtsEnd) => {
                self.transition_to(State::Idle, EventTag::TtsEnd, now_ms)
            }
            (State::Speaking, Event::AudioWake) => {
                // Barge-in: cancel TTS, return to listening.
                let mut acts = vec![Action::PublishTtsCancel];
                acts.extend(self.transition_to(
                    State::Listening,
                    EventTag::AudioWake,
                    now_ms,
                ));
                acts
            }

            // ── confirming ──────────────────────────────────────────
            (
                State::Confirming(ctx),
                Event::SttFinal {
                    transcript,
                    confidence: _,
                },
            ) => self.handle_confirm_utterance(ctx.clone(), &transcript, now_ms),
            (State::Confirming(ctx), Event::ConfirmTimeout) => {
                let max_reprompts = self.timing.max_reprompts;
                let attempts = ctx.attempts;
                if attempts < max_reprompts {
                    // Patience reprompt: emit a warm check-in phrase,
                    // bump the attempt counter, restart the timer — stay
                    // in Confirming without a state transition.
                    let phrase = silence_reprompt(usize::from(attempts));
                    let confirm_ms = self.timing.confirm_timeout_ms;
                    let new_ctx = ConfirmContext {
                        attempts: attempts.saturating_add(1),
                        ..ctx.clone()
                    };
                    self.state = State::Confirming(new_ctx);
                    vec![
                        Action::PublishTtsSay {
                            text: phrase.to_string(),
                        },
                        Action::StartConfirmTimer { ms: confirm_ms },
                    ]
                } else {
                    // Final timeout — announce the return-to-idle with a
                    // warm close before transitioning. DenyReason::Silence
                    // is still recorded; the close line accompanies it.
                    let intent_id = ctx.intent_id.clone();
                    let mut acts = vec![
                        Action::CancelConfirmTimer,
                        Action::PublishTtsSay {
                            text: silence_close().to_string(),
                        },
                        Action::PublishConfirmDenied {
                            intent_id,
                            reason: DenyReason::Silence,
                        },
                    ];
                    acts.extend(self.transition_to(
                        State::Idle,
                        EventTag::ConfirmTimeout,
                        now_ms,
                    ));
                    acts
                }
            }
            (State::Confirming(ctx), Event::AudioWake) => {
                let intent_id = ctx.intent_id.clone();
                let mut acts = vec![
                    Action::CancelConfirmTimer,
                    Action::PublishConfirmDenied {
                        intent_id,
                        reason: DenyReason::BargeIn,
                    },
                ];
                acts.extend(self.transition_to(
                    State::Idle,
                    EventTag::AudioWake,
                    now_ms,
                ));
                acts
            }

            // ── everything else is an ignored no-op ────────────────
            _ => Vec::new(),
        }
    }

    // ── helpers ──────────────────────────────────────────────────

    fn transition_to(&mut self, new: State, trigger: EventTag, now_ms: u64) -> Vec<Action> {
        let prior_tag = self.state.tag();
        let new_tag = new.tag();
        let since_ms = now_ms.saturating_sub(self.last_change_ms);

        self.state = new;
        self.last_change_ms = now_ms;
        self.push_history(Transition {
            prior: prior_tag,
            next: new_tag,
            trigger,
            at_ms: now_ms,
            elapsed_ms: since_ms,
        });

        vec![Action::PublishState {
            prior: prior_tag,
            next: new_tag,
            since_ms,
        }]
    }

    fn push_history(&mut self, t: Transition) {
        if self.history.len() == self.history_cap {
            let _ = self.history.pop_front();
        }
        self.history.push_back(t);
    }

    fn do_mute(&mut self, now_ms: u64) -> Vec<Action> {
        let was_muted = self.flags.muted;
        self.flags.muted = true;
        let mut acts = Vec::new();
        if matches!(self.state, State::Speaking) {
            acts.push(Action::PublishTtsCancel);
            acts.extend(self.transition_to(State::Idle, EventTag::MuteRequest, now_ms));
        }
        if !was_muted {
            acts.push(Action::PublishAudioMute);
        }
        acts
    }

    fn do_unmute(&mut self, _now_ms: u64) -> Vec<Action> {
        let was_muted = self.flags.muted;
        self.flags.muted = false;
        if was_muted {
            vec![Action::PublishAudioUnmute]
        } else {
            Vec::new()
        }
    }

    fn enter_confirm_or_deny(
        &mut self,
        intent_id: String,
        summary: String,
        confirm_keyword: String,
        now_ms: u64,
    ) -> Vec<Action> {
        if self.flags.child_locked {
            // Silent deny — no TTS, no prompt. PRD §2.5.
            let mut acts = vec![Action::PublishConfirmDenied {
                intent_id,
                reason: DenyReason::ChildLock,
            }];
            acts.extend(self.transition_to(
                State::Idle,
                EventTag::BrainReplyDestructive,
                now_ms,
            ));
            return acts;
        }

        let prompt = format!(
            "You want me to {summary}. Say 'yes {confirm_keyword}' if that's right."
        );
        let next = State::Confirming(ConfirmContext {
            intent_id,
            summary,
            confirm_keyword,
            attempts: 0,
        });
        // Read deadline from config — no magic constant in transition code.
        let confirm_ms = self.timing.confirm_timeout_ms;
        let mut acts = vec![
            Action::PublishTtsSay { text: prompt },
            Action::StartConfirmTimer { ms: confirm_ms },
        ];
        acts.extend(self.transition_to(next, EventTag::BrainReplyDestructive, now_ms));
        acts
    }

    fn handle_confirm_utterance(
        &mut self,
        ctx: ConfirmContext,
        transcript: &str,
        now_ms: u64,
    ) -> Vec<Action> {
        // Read max_reprompts from config — no magic constant in transition code.
        let max_reprompts = self.timing.max_reprompts;
        match classify_confirm(transcript, &ctx.confirm_keyword, ctx.attempts, max_reprompts) {
            ConfirmDecision::Grant => {
                let intent_id = ctx.intent_id;
                let mut acts = vec![
                    Action::CancelConfirmTimer,
                    Action::PublishConfirmGranted { intent_id },
                ];
                acts.extend(self.transition_to(
                    State::Idle,
                    EventTag::SttFinal,
                    now_ms,
                ));
                acts
            }
            ConfirmDecision::Deny(reason) => {
                let intent_id = ctx.intent_id;
                let mut acts = vec![
                    Action::CancelConfirmTimer,
                    Action::PublishConfirmDenied { intent_id, reason },
                ];
                acts.extend(self.transition_to(
                    State::Idle,
                    EventTag::SttFinal,
                    now_ms,
                ));
                acts
            }
            ConfirmDecision::Reprompt => {
                let reprompt_text = format!(
                    "Please say 'yes {keyword}' to confirm, or 'no' to cancel.",
                    keyword = ctx.confirm_keyword
                );
                // Bump attempt counter; stay in confirming with restarted timer.
                let new_ctx = ConfirmContext {
                    attempts: ctx.attempts.saturating_add(1),
                    ..ctx
                };
                self.state = State::Confirming(new_ctx);
                // Read deadline from config — no magic constant.
                let confirm_ms = self.timing.confirm_timeout_ms;
                vec![
                    Action::PublishTtsSay {
                        text: reprompt_text,
                    },
                    Action::StartConfirmTimer { ms: confirm_ms },
                ]
            }
        }
    }
}

/// Outcome of matching a single utterance against the confirm protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmDecision {
    Grant,
    Deny(DenyReason),
    Reprompt,
}

/// PRD §2.4 verbal-confirm match table.
///
/// `attempts == 0` allows re-prompts up to `max_reprompts`; once
/// `attempts >= max_reprompts` any ambiguous response folds into a deny.
/// The `max_reprompts` parameter is now sourced from
/// [`DialogTimingConfig::max_reprompts`] rather than a compile-time
/// constant so the operator can tune the threshold without recompiling.
fn classify_confirm(
    transcript: &str,
    keyword: &str,
    attempts: u8,
    max_reprompts: u8,
) -> ConfirmDecision {
    let lower = transcript.trim().to_lowercase();
    let keyword_lower = keyword.trim().to_lowercase();
    if lower.is_empty() {
        return if attempts >= max_reprompts {
            ConfirmDecision::Deny(DenyReason::Ambiguous)
        } else {
            ConfirmDecision::Reprompt
        };
    }

    // Hard-deny words match first so "no please" → deny, not ambiguous.
    if matches!(lower.as_str(), "no" | "cancel" | "stop")
        || lower.starts_with("no ")
        || lower.starts_with("cancel ")
        || lower.starts_with("stop ")
    {
        return ConfirmDecision::Deny(DenyReason::UserSaidNo);
    }

    let parts: Vec<&str> = lower.split_whitespace().collect();
    let yes_keyword = parts.first().is_some_and(|p| *p == "yes")
        && parts.get(1).is_some_and(|p| *p == keyword_lower.as_str());
    if yes_keyword {
        return ConfirmDecision::Grant;
    }

    // After enough re-prompts, accept the bare keyword too — the prompt
    // explicitly asked for it.
    if attempts >= max_reprompts && lower == keyword_lower {
        return ConfirmDecision::Grant;
    }

    let yes_alone = parts.len() == 1 && parts.first().is_some_and(|p| *p == "yes");
    if yes_alone && attempts < max_reprompts {
        return ConfirmDecision::Reprompt;
    }

    if attempts >= max_reprompts {
        ConfirmDecision::Deny(DenyReason::Ambiguous)
    } else {
        ConfirmDecision::Reprompt
    }
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

    fn assert_state(fsm: &Fsm, expected: StateTag) {
        assert_eq!(fsm.state().tag(), expected, "state mismatch");
    }

    #[test]
    fn idle_wake_enters_listening() {
        let mut fsm = Fsm::new(0);
        let acts = fsm.handle(Event::AudioWake, 100);
        assert_state(&fsm, StateTag::Listening);
        assert_eq!(acts.len(), 1);
        assert!(matches!(
            acts[0],
            Action::PublishState {
                prior: StateTag::Idle,
                next: StateTag::Listening,
                since_ms: 100
            }
        ));
    }

    #[test]
    fn listening_speech_start_enters_transcribing() {
        let mut fsm = Fsm::new(0);
        fsm.handle(Event::AudioWake, 100);
        fsm.handle(Event::AudioSpeechStart, 200);
        assert_state(&fsm, StateTag::Transcribing);
    }

    #[test]
    fn transcribing_stt_final_enters_thinking_with_brain_forward() {
        let mut fsm = Fsm::new(0);
        fsm.handle(Event::AudioWake, 100);
        fsm.handle(Event::AudioSpeechStart, 200);
        let acts = fsm.handle(
            Event::SttFinal {
                transcript: "what time is it".to_string(),
                confidence: 0.93,
            },
            300,
        );
        assert_state(&fsm, StateTag::Thinking);
        assert!(acts.iter().any(|a| matches!(
            a,
            Action::PublishBrainUtterance { transcript, .. } if transcript == "what time is it"
        )));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::PublishTurnUser { .. })));
    }

    #[test]
    fn thinking_brain_reply_enters_speaking() {
        let mut fsm = Fsm::new(0);
        fsm.handle(Event::AudioWake, 100);
        fsm.handle(Event::AudioSpeechStart, 200);
        fsm.handle(
            Event::SttFinal {
                transcript: "hi".to_string(),
                confidence: 1.0,
            },
            300,
        );
        let acts = fsm.handle(
            Event::BrainReply {
                text: "hello there".to_string(),
            },
            400,
        );
        assert_state(&fsm, StateTag::Speaking);
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::PublishTtsSay { text } if text == "hello there")));
    }

    #[test]
    fn speaking_tts_end_returns_to_idle() {
        let mut fsm = Fsm::new(0);
        drive_to_speaking(&mut fsm);
        fsm.handle(Event::TtsEnd, 500);
        assert_state(&fsm, StateTag::Idle);
    }

    #[test]
    fn speaking_wake_barge_in_cancels_tts_and_listens() {
        let mut fsm = Fsm::new(0);
        drive_to_speaking(&mut fsm);
        let acts = fsm.handle(Event::AudioWake, 500);
        assert_state(&fsm, StateTag::Listening);
        assert!(acts.contains(&Action::PublishTtsCancel));
    }

    #[test]
    fn stt_uncertain_re_prompts_without_wedging() {
        // AC2: 5 sequential uncertains return to listening every time
        // with exactly one re-prompt utterance per event.
        let mut fsm = Fsm::new(0);
        fsm.handle(Event::AudioWake, 50);
        for i in 0..5 {
            let t = 100 + i * 200;
            fsm.handle(Event::AudioSpeechStart, t);
            let acts = fsm.handle(Event::SttUncertain, t + 50);
            assert_state(&fsm, StateTag::Listening);
            let reprompts = acts
                .iter()
                .filter(|a| matches!(a, Action::PublishTtsSay { .. }))
                .count();
            assert_eq!(reprompts, 1, "exactly one re-prompt per uncertain");
        }
    }

    #[test]
    fn confirm_grants_on_exact_yes_keyword() {
        let mut fsm = Fsm::new(0);
        drive_to_confirming(&mut fsm, "delete-email");
        let acts = fsm.handle(
            Event::SttFinal {
                transcript: "yes delete-email".to_string(),
                confidence: 0.99,
            },
            700,
        );
        assert_state(&fsm, StateTag::Idle);
        assert!(acts.iter().any(|a| matches!(
            a,
            Action::PublishConfirmGranted { intent_id } if intent_id == "intent-1"
        )));
        assert!(acts.contains(&Action::CancelConfirmTimer));
    }

    /// Verify that the FSM grants when the bare keyword is said after
    /// `max_reprompts` have been exhausted.  Uses `max_reprompts = 1`
    /// explicitly so the bare-keyword grant fires after exactly one
    /// re-prompt, and asserts that `StartConfirmTimer { ms }` carries
    /// the config-sourced value rather than the compile-time constant.
    #[test]
    fn confirm_grants_after_yes_alone_then_keyword() {
        use crate::config::DialogTimingConfig;
        let timing = DialogTimingConfig {
            max_reprompts: 1,
            ..DialogTimingConfig::default()
        };
        let mut fsm = Fsm::with_timing(0, timing);
        let expected_confirm_ms = fsm.timing().confirm_timeout_ms;
        drive_to_confirming(&mut fsm, "delete-email");
        let acts1 = fsm.handle(
            Event::SttFinal {
                transcript: "yes".to_string(),
                confidence: 0.9,
            },
            700,
        );
        // Stays confirming (attempts=0 < max_reprompts=1), emits re-prompt
        // + restarts timer with the config-sourced ms value.
        assert_state(&fsm, StateTag::Confirming);
        assert!(acts1
            .iter()
            .any(|a| matches!(a, Action::PublishTtsSay { .. })));
        // Timer restart uses the config-sourced value, not the compile-time const.
        assert!(acts1.iter().any(|a| matches!(
            a,
            Action::StartConfirmTimer { ms } if *ms == expected_confirm_ms
        )));
        // Now attempts=1 >= max_reprompts=1; bare keyword → Grant.
        let acts2 = fsm.handle(
            Event::SttFinal {
                transcript: "delete-email".to_string(),
                confidence: 0.9,
            },
            800,
        );
        assert_state(&fsm, StateTag::Idle);
        assert!(acts2
            .iter()
            .any(|a| matches!(a, Action::PublishConfirmGranted { .. })));
    }

    #[test]
    fn confirm_denies_on_no_cancel_stop() {
        for word in ["no", "cancel", "stop"] {
            let mut fsm = Fsm::new(0);
            drive_to_confirming(&mut fsm, "delete-email");
            let acts = fsm.handle(
                Event::SttFinal {
                    transcript: word.to_string(),
                    confidence: 0.9,
                },
                700,
            );
            assert_state(&fsm, StateTag::Idle);
            assert!(
                acts.iter().any(|a| matches!(
                    a,
                    Action::PublishConfirmDenied {
                        reason: DenyReason::UserSaidNo,
                        ..
                    }
                )),
                "{word} should deny with UserSaidNo"
            );
        }
    }

    /// With `max_reprompts=0` the very first `ConfirmTimeout` should
    /// immediately deny (no patience reprompts at all) and emit the
    /// warm close line — reproducing today's "single-shot-then-silent"
    /// behavior but with a spoken farewell.
    #[test]
    fn confirm_denies_on_timeout_with_zero_reprompts() {
        use crate::config::DialogTimingConfig;
        let timing = DialogTimingConfig {
            max_reprompts: 0,
            ..DialogTimingConfig::default()
        };
        let mut fsm = Fsm::with_timing(0, timing);
        drive_to_confirming(&mut fsm, "delete-email");
        let acts = fsm.handle(Event::ConfirmTimeout, 30_700);
        assert_state(&fsm, StateTag::Idle);
        assert!(acts.iter().any(|a| matches!(
            a,
            Action::PublishConfirmDenied {
                reason: DenyReason::Silence,
                ..
            }
        )));
        // Warm close line is emitted before transitioning.
        assert!(
            acts.iter().any(|a| matches!(a, Action::PublishTtsSay { .. })),
            "final timeout must emit a warm close via TTS"
        );
    }

    /// AC1/AC2: with `max_reprompts=2`, two `ConfirmTimeout` events
    /// should each reprompt with a distinct, escalating phrase (not deny),
    /// and the third timeout should deny with `DenyReason::Silence`.
    ///
    /// AC3: the third (final) timeout emits a spoken close phrase before
    /// the `Confirming → Idle` transition.
    #[test]
    fn confirm_timeout_reprompts_escalating_then_closes_warmly() {
        use crate::config::DialogTimingConfig;
        let timing = DialogTimingConfig {
            max_reprompts: 2,
            ..DialogTimingConfig::default()
        };
        let mut fsm = Fsm::with_timing(0, timing);
        let confirm_ms = fsm.timing().confirm_timeout_ms;
        drive_to_confirming(&mut fsm, "delete-email");

        // First timeout → reprompt (attempt 0, stays Confirming).
        let acts1 = fsm.handle(Event::ConfirmTimeout, 45_700);
        assert_state(&fsm, StateTag::Confirming);
        assert!(
            acts1.iter().any(|a| matches!(a, Action::PublishTtsSay { .. })),
            "first timeout: must emit warm check-in phrase"
        );
        // Timer restarted.
        assert!(
            acts1
                .iter()
                .any(|a| matches!(a, Action::StartConfirmTimer { ms } if *ms == confirm_ms)),
            "first timeout: must restart confirm timer"
        );
        // No deny yet.
        assert!(
            !acts1.iter().any(|a| matches!(a, Action::PublishConfirmDenied { .. })),
            "first timeout: must not deny yet"
        );

        // Collect first reprompt text.
        let text1 = acts1
            .iter()
            .find_map(|a| {
                if let Action::PublishTtsSay { text } = a {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .expect("first timeout should emit TtsSay");

        // Second timeout → reprompt (attempt 1, stays Confirming).
        let acts2 = fsm.handle(Event::ConfirmTimeout, 91_400);
        assert_state(&fsm, StateTag::Confirming);
        assert!(
            acts2.iter().any(|a| matches!(a, Action::PublishTtsSay { .. })),
            "second timeout: must emit warm check-in phrase"
        );
        assert!(
            !acts2.iter().any(|a| matches!(a, Action::PublishConfirmDenied { .. })),
            "second timeout: must not deny yet"
        );

        // Collect second reprompt text — must differ from first (AC2).
        let text2 = acts2
            .iter()
            .find_map(|a| {
                if let Action::PublishTtsSay { text } = a {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .expect("second timeout should emit TtsSay");
        assert_ne!(
            text1, text2,
            "reprompt phrases must escalate (attempt 0 ≠ attempt 1)"
        );

        // Third timeout → final deny with spoken close (AC3).
        let acts3 = fsm.handle(Event::ConfirmTimeout, 137_100);
        assert_state(&fsm, StateTag::Idle);
        assert!(
            acts3.iter().any(|a| matches!(
                a,
                Action::PublishConfirmDenied {
                    reason: DenyReason::Silence,
                    ..
                }
            )),
            "final timeout: DenyReason must be Silence"
        );
        // AC3: warm close phrase spoken before Idle transition.
        assert!(
            acts3.iter().any(|a| matches!(a, Action::PublishTtsSay { .. })),
            "final timeout: must emit warm close line via TTS before returning to Idle"
        );
    }

    /// AC4: `max_reprompts=1` reproduces the old single-shot behavior —
    /// one intermediate reprompt, then deny on the second timeout.
    #[test]
    fn confirm_timeout_single_shot_regression_guard() {
        use crate::config::DialogTimingConfig;
        let timing = DialogTimingConfig {
            max_reprompts: 1,
            ..DialogTimingConfig::default()
        };
        let mut fsm = Fsm::with_timing(0, timing);
        drive_to_confirming(&mut fsm, "delete-email");

        // First timeout → one reprompt, stays Confirming.
        let acts1 = fsm.handle(Event::ConfirmTimeout, 45_700);
        assert_state(&fsm, StateTag::Confirming);
        assert!(
            acts1.iter().any(|a| matches!(a, Action::PublishTtsSay { .. })),
            "one reprompt must still fire with max_reprompts=1"
        );
        assert!(
            !acts1.iter().any(|a| matches!(a, Action::PublishConfirmDenied { .. })),
            "must not deny on first timeout with max_reprompts=1"
        );

        // Second timeout → deny.
        let acts2 = fsm.handle(Event::ConfirmTimeout, 91_400);
        assert_state(&fsm, StateTag::Idle);
        assert!(
            acts2.iter().any(|a| matches!(
                a,
                Action::PublishConfirmDenied {
                    reason: DenyReason::Silence,
                    ..
                }
            )),
            "second timeout with max_reprompts=1 must deny with Silence"
        );
    }

    /// Verify that the FSM denies on ambiguity once `max_reprompts`
    /// attempts have been exhausted.  This test uses an explicit
    /// `max_reprompts = 1` config so it exercises the "one re-prompt"
    /// boundary regardless of the elder-friendly default (2).
    #[test]
    fn confirm_denies_on_ambiguous_after_configured_reprompts() {
        use crate::config::DialogTimingConfig;
        let timing = DialogTimingConfig {
            max_reprompts: 1,
            ..DialogTimingConfig::default()
        };
        let mut fsm = Fsm::with_timing(0, timing);
        drive_to_confirming(&mut fsm, "delete-email");
        // First ambiguous → re-prompt (attempts=0 < max_reprompts=1).
        fsm.handle(
            Event::SttFinal {
                transcript: "what?".to_string(),
                confidence: 0.9,
            },
            700,
        );
        assert_state(&fsm, StateTag::Confirming);
        // Second ambiguous → deny (attempts=1 >= max_reprompts=1).
        let acts = fsm.handle(
            Event::SttFinal {
                transcript: "yeah maybe".to_string(),
                confidence: 0.9,
            },
            800,
        );
        assert_state(&fsm, StateTag::Idle);
        assert!(acts.iter().any(|a| matches!(
            a,
            Action::PublishConfirmDenied {
                reason: DenyReason::Ambiguous,
                ..
            }
        )));
    }

    /// With the elder-friendly default (max_reprompts=2), a second
    /// ambiguous utterance still gives another re-prompt rather than
    /// denying immediately.
    #[test]
    fn confirm_reprompts_twice_with_default_config() {
        let mut fsm = Fsm::new(0); // elder-friendly defaults: max_reprompts=2
        drive_to_confirming(&mut fsm, "delete-email");
        // First ambiguous → re-prompt.
        fsm.handle(
            Event::SttFinal {
                transcript: "what?".to_string(),
                confidence: 0.9,
            },
            700,
        );
        assert_state(&fsm, StateTag::Confirming);
        // Second ambiguous → re-prompt again (attempts=1 < max_reprompts=2).
        let acts2 = fsm.handle(
            Event::SttFinal {
                transcript: "hmm?".to_string(),
                confidence: 0.9,
            },
            800,
        );
        assert_state(&fsm, StateTag::Confirming);
        assert!(
            acts2.iter().any(|a| matches!(a, Action::PublishTtsSay { .. })),
            "second ambiguous should re-prompt with max_reprompts=2"
        );
        // Third ambiguous → deny (attempts=2 >= max_reprompts=2).
        let acts3 = fsm.handle(
            Event::SttFinal {
                transcript: "yeah maybe".to_string(),
                confidence: 0.9,
            },
            900,
        );
        assert_state(&fsm, StateTag::Idle);
        assert!(acts3.iter().any(|a| matches!(
            a,
            Action::PublishConfirmDenied {
                reason: DenyReason::Ambiguous,
                ..
            }
        )));
    }

    #[test]
    fn confirm_wake_barges_in_and_denies() {
        let mut fsm = Fsm::new(0);
        drive_to_confirming(&mut fsm, "delete-email");
        let acts = fsm.handle(Event::AudioWake, 700);
        assert_state(&fsm, StateTag::Idle);
        assert!(acts.iter().any(|a| matches!(
            a,
            Action::PublishConfirmDenied {
                reason: DenyReason::BargeIn,
                ..
            }
        )));
    }

    #[test]
    fn child_lock_silently_denies_destructive() {
        let mut fsm = Fsm::new(0);
        fsm.handle(Event::SetChildLock { enabled: true }, 50);
        drive_to_thinking(&mut fsm);
        let acts = fsm.handle(
            Event::BrainReplyDestructive {
                intent_id: "i-1".to_string(),
                summary: "drop the database".to_string(),
                confirm_keyword: "drop-db".to_string(),
            },
            400,
        );
        assert_state(&fsm, StateTag::Idle);
        assert!(acts.iter().any(|a| matches!(
            a,
            Action::PublishConfirmDenied {
                reason: DenyReason::ChildLock,
                ..
            }
        )));
        // No prompt utterance.
        assert!(!acts
            .iter()
            .any(|a| matches!(a, Action::PublishTtsSay { .. })));
    }

    #[test]
    fn mute_gates_wake_and_speaking() {
        let mut fsm = Fsm::new(0);
        drive_to_speaking(&mut fsm);
        let mute_acts = fsm.handle(Event::MuteRequest, 500);
        // Speaking → idle, publishes TtsCancel + AudioMute.
        assert_state(&fsm, StateTag::Idle);
        assert!(mute_acts.contains(&Action::PublishTtsCancel));
        assert!(mute_acts.contains(&Action::PublishAudioMute));
        // Wake while muted is ignored.
        let gated = fsm.handle(Event::AudioWake, 600);
        assert!(gated.is_empty());
        assert_state(&fsm, StateTag::Idle);
        // Unmute → publishes AudioUnmute; subsequent wake works.
        let unmute_acts = fsm.handle(Event::UnmuteRequest, 700);
        assert!(unmute_acts.contains(&Action::PublishAudioUnmute));
        fsm.handle(Event::AudioWake, 800);
        assert_state(&fsm, StateTag::Listening);
    }

    #[test]
    fn history_returns_last_n_in_chronological_order() {
        let mut fsm = Fsm::with_history_capacity(0, 4);
        // Drive 6 transitions; only the last 4 remain.
        fsm.handle(Event::AudioWake, 100); // idle→listening
        fsm.handle(Event::AudioSpeechStart, 150); // listening→transcribing
        fsm.handle(Event::SttUncertain, 200); // transcribing→listening
        fsm.handle(Event::AudioSpeechStart, 250); // listening→transcribing
        fsm.handle(
            Event::SttFinal {
                transcript: "hi".to_string(),
                confidence: 0.9,
            },
            300,
        ); // transcribing→thinking
        fsm.handle(
            Event::BrainReply {
                text: "ok".to_string(),
            },
            400,
        ); // thinking→speaking

        let last3 = fsm.history(3);
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0].next, StateTag::Transcribing);
        assert_eq!(last3[1].next, StateTag::Thinking);
        assert_eq!(last3[2].next, StateTag::Speaking);
        // Each entry has elapsed_ms = at_ms - prior at_ms (monotonic).
        for w in last3.windows(2) {
            assert!(w[1].at_ms >= w[0].at_ms);
        }
    }

    /// AC3 / AC4: a non-default `confirm_timeout_ms` value is reflected in
    /// the `StartConfirmTimer { ms }` action emitted when the FSM enters
    /// `Confirming`.  This test uses `confirm_timeout_ms = 12_000` and
    /// asserts that the timer action carries 12_000, not the legacy 30_000
    /// or the elder-friendly default 45_000.
    #[test]
    fn custom_confirm_ms_schedules_correct_timer() {
        use crate::config::DialogTimingConfig;
        let custom_ms: u32 = 12_000;
        let timing = DialogTimingConfig {
            confirm_timeout_ms: custom_ms,
            ..DialogTimingConfig::default()
        };
        let mut fsm = Fsm::with_timing(0, timing);
        drive_to_thinking(&mut fsm);
        let acts = fsm.handle(
            Event::BrainReplyDestructive {
                intent_id: "i-custom".to_string(),
                summary: "do the thing".to_string(),
                confirm_keyword: "do-it".to_string(),
            },
            500,
        );
        assert_state(&fsm, StateTag::Confirming);
        let timer_ms = acts
            .iter()
            .find_map(|a| {
                if let Action::StartConfirmTimer { ms } = a {
                    Some(*ms)
                } else {
                    None
                }
            })
            .expect("StartConfirmTimer action must be present");
        assert_eq!(
            timer_ms, custom_ms,
            "StartConfirmTimer {{ms}} should use config value {custom_ms}, got {timer_ms}"
        );
    }

    /// AC3: verify that no timing magic numbers remain — the FSM reads
    /// `confirm_timeout_ms` from its `timing` field.  Changing the config
    /// changes the scheduled ms, demonstrating the transition code is
    /// config-sourced.
    #[test]
    fn timing_field_drives_all_confirm_timer_schedules() {
        use crate::config::DialogTimingConfig;
        for confirm_ms in [5_000_u32, 30_000, 45_000, 90_000] {
            let timing = DialogTimingConfig {
                confirm_timeout_ms: confirm_ms,
                ..DialogTimingConfig::default()
            };
            let mut fsm = Fsm::with_timing(0, timing);
            drive_to_thinking(&mut fsm);
            let acts = fsm.handle(
                Event::BrainReplyDestructive {
                    intent_id: "i-sweep".to_string(),
                    summary: "do something destructive".to_string(),
                    confirm_keyword: "confirm-it".to_string(),
                },
                500,
            );
            let scheduled = acts
                .iter()
                .find_map(|a| {
                    if let Action::StartConfirmTimer { ms } = a {
                        Some(*ms)
                    } else {
                        None
                    }
                })
                .expect("StartConfirmTimer must appear");
            assert_eq!(
                scheduled, confirm_ms,
                "config confirm_ms={confirm_ms} → timer ms should be {confirm_ms}, got {scheduled}"
            );
        }
    }

    // ── drivers ─────────────────────────────────────────────────

    fn drive_to_thinking(fsm: &mut Fsm) {
        fsm.handle(Event::AudioWake, 100);
        fsm.handle(Event::AudioSpeechStart, 200);
        fsm.handle(
            Event::SttFinal {
                transcript: "hi".to_string(),
                confidence: 0.99,
            },
            300,
        );
    }

    fn drive_to_speaking(fsm: &mut Fsm) {
        drive_to_thinking(fsm);
        fsm.handle(
            Event::BrainReply {
                text: "hello".to_string(),
            },
            400,
        );
    }

    fn drive_to_confirming(fsm: &mut Fsm, keyword: &str) {
        drive_to_thinking(fsm);
        fsm.handle(
            Event::BrainReplyDestructive {
                intent_id: "intent-1".to_string(),
                summary: "delete the email".to_string(),
                confirm_keyword: keyword.to_string(),
            },
            500,
        );
    }
}
