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

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::action::{Action, DenyReason};
use crate::event::{Event, EventTag};
use crate::state::{ConfirmContext, Flags, State, StateTag};

/// Default history-ring capacity. PRD §2.5 / intent-card `history_ring_size`.
pub const DEFAULT_HISTORY_CAPACITY: usize = 256;

/// PRD §2.4: verbal-confirm timeout (30 s → `30_000` ms).
pub const CONFIRM_TIMEOUT_MS: u32 = 30_000;

/// PRD §2.4: at most one re-prompt before deny.
pub const MAX_REPROMPTS: u8 = 1;

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
}

impl Fsm {
    /// Construct a fresh FSM in `Idle` with default flags + default
    /// history capacity. `now_ms` is the FSM's epoch — `since_ms`
    /// values are computed against this until the first transition.
    #[must_use]
    pub const fn new(now_ms: u64) -> Self {
        Self {
            state: State::Idle,
            flags: Flags {
                muted: false,
                child_locked: false,
            },
            last_change_ms: now_ms,
            history: VecDeque::new(),
            history_cap: DEFAULT_HISTORY_CAPACITY,
        }
    }

    /// Construct an FSM with a non-default history capacity.
    #[must_use]
    pub fn with_history_capacity(now_ms: u64, cap: usize) -> Self {
        let mut fsm = Self::new(now_ms);
        fsm.history_cap = cap.max(1);
        fsm
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
                let intent_id = ctx.intent_id.clone();
                let mut acts = vec![
                    Action::CancelConfirmTimer,
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
        let mut acts = vec![
            Action::PublishTtsSay { text: prompt },
            Action::StartConfirmTimer {
                ms: CONFIRM_TIMEOUT_MS,
            },
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
        match classify_confirm(transcript, &ctx.confirm_keyword, ctx.attempts) {
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
                vec![
                    Action::PublishTtsSay {
                        text: reprompt_text,
                    },
                    Action::StartConfirmTimer {
                        ms: CONFIRM_TIMEOUT_MS,
                    },
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
/// `attempts == 0` allows one re-prompt; `attempts >= MAX_REPROMPTS`
/// folds the re-prompt slot into a deny on ambiguity.
fn classify_confirm(transcript: &str, keyword: &str, attempts: u8) -> ConfirmDecision {
    let lower = transcript.trim().to_lowercase();
    let keyword_lower = keyword.trim().to_lowercase();
    if lower.is_empty() {
        return if attempts >= MAX_REPROMPTS {
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

    // After one re-prompt, accept the bare keyword too — the prompt
    // explicitly asked for it.
    if attempts >= MAX_REPROMPTS && lower == keyword_lower {
        return ConfirmDecision::Grant;
    }

    let yes_alone = parts.len() == 1 && parts.first().is_some_and(|p| *p == "yes");
    if yes_alone && attempts < MAX_REPROMPTS {
        return ConfirmDecision::Reprompt;
    }

    if attempts >= MAX_REPROMPTS {
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

    #[test]
    fn confirm_grants_after_yes_alone_then_keyword() {
        let mut fsm = Fsm::new(0);
        drive_to_confirming(&mut fsm, "delete-email");
        let acts1 = fsm.handle(
            Event::SttFinal {
                transcript: "yes".to_string(),
                confidence: 0.9,
            },
            700,
        );
        // Stays confirming, emits re-prompt + restarts timer.
        assert_state(&fsm, StateTag::Confirming);
        assert!(acts1
            .iter()
            .any(|a| matches!(a, Action::PublishTtsSay { .. })));
        assert!(acts1.iter().any(|a| matches!(
            a,
            Action::StartConfirmTimer { ms } if *ms == CONFIRM_TIMEOUT_MS
        )));
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

    #[test]
    fn confirm_denies_on_timeout() {
        let mut fsm = Fsm::new(0);
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
    }

    #[test]
    fn confirm_denies_on_ambiguous_after_one_reprompt() {
        let mut fsm = Fsm::new(0);
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
        // Second ambiguous → deny.
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
