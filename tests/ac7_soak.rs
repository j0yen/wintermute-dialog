//! AC7: 50-turn soak — the FSM must complete every conversational turn
//! without wedging in `Confirming`, `Listening`, or any non-idle state.
//!
//! The PRD calls for a 60-minute, 50-turn steady-state run. We compress
//! the wall-clock dimension via the injected monotonic clock — each
//! turn advances `now_ms` by 72 s — but keep the 50-turn coverage and
//! the cross-scenario variety the soak exists to catch. A wedge in any
//! mode (mute, child-lock, destructive-confirm, barge-in, uncertain
//! re-prompt) trips the per-turn `assert_state(Idle)` post-condition.

#![allow(
    clippy::too_many_lines,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "scenario table + 50-iteration loop is intentionally explicit"
)]

use wintermute_dialog::{Event, Fsm, StateTag};

/// One conversational scenario the FSM should be able to absorb and
/// finish back at `Idle`.
#[derive(Clone, Copy)]
enum Scenario {
    SimpleReply,
    DestructiveGrant,
    DestructiveDenyNo,
    DestructiveTimeout,
    DestructiveChildLock,
    UncertainThenFinal,
    BargeInDuringSpeaking,
    MuteThenUnmute,
}

const SCENARIOS: [Scenario; 8] = [
    Scenario::SimpleReply,
    Scenario::DestructiveGrant,
    Scenario::DestructiveDenyNo,
    Scenario::DestructiveTimeout,
    Scenario::DestructiveChildLock,
    Scenario::UncertainThenFinal,
    Scenario::BargeInDuringSpeaking,
    Scenario::MuteThenUnmute,
];

fn drive_turn(fsm: &mut Fsm, scenario: Scenario, t0: u64) {
    match scenario {
        Scenario::SimpleReply => {
            fsm.handle(Event::AudioWake, t0 + 10);
            fsm.handle(Event::AudioSpeechStart, t0 + 100);
            fsm.handle(
                Event::SttFinal {
                    transcript: "what time is it".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 500,
            );
            fsm.handle(
                Event::BrainReply {
                    text: "ten past three".to_string(),
                },
                t0 + 900,
            );
            fsm.handle(Event::TtsEnd, t0 + 1500);
        }
        Scenario::DestructiveGrant => {
            fsm.handle(Event::AudioWake, t0 + 10);
            fsm.handle(Event::AudioSpeechStart, t0 + 100);
            fsm.handle(
                Event::SttFinal {
                    transcript: "delete the inbox".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 500,
            );
            fsm.handle(
                Event::BrainReplyDestructive {
                    intent_id: format!("intent-{t0}"),
                    summary: "delete inbox".to_string(),
                    confirm_keyword: "delete-inbox".to_string(),
                },
                t0 + 900,
            );
            fsm.handle(
                Event::SttFinal {
                    transcript: "yes delete-inbox".to_string(),
                    confidence: 0.97,
                    turn_id: None,
                },
                t0 + 1500,
            );
        }
        Scenario::DestructiveDenyNo => {
            fsm.handle(Event::AudioWake, t0 + 10);
            fsm.handle(Event::AudioSpeechStart, t0 + 100);
            fsm.handle(
                Event::SttFinal {
                    transcript: "drop the table".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 500,
            );
            fsm.handle(
                Event::BrainReplyDestructive {
                    intent_id: format!("intent-{t0}"),
                    summary: "drop table".to_string(),
                    confirm_keyword: "drop-table".to_string(),
                },
                t0 + 900,
            );
            fsm.handle(
                Event::SttFinal {
                    transcript: "no".to_string(),
                    confidence: 0.96,
                    turn_id: None,
                },
                t0 + 1500,
            );
        }
        Scenario::DestructiveTimeout => {
            fsm.handle(Event::AudioWake, t0 + 10);
            fsm.handle(Event::AudioSpeechStart, t0 + 100);
            fsm.handle(
                Event::SttFinal {
                    transcript: "wipe the disk".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 500,
            );
            fsm.handle(
                Event::BrainReplyDestructive {
                    intent_id: format!("intent-{t0}"),
                    summary: "wipe disk".to_string(),
                    confirm_keyword: "wipe-disk".to_string(),
                },
                t0 + 900,
            );
            // Timeouts now drive the patience sequence from earshot-gentle-reprompt.
            // Default max_reprompts=2: two warm check-in reprompts, then denial.
            // First timeout (attempt 0 < 2) → warm check-in, stays Confirming.
            fsm.handle(Event::ConfirmTimeout, t0 + 31_000);
            // Second timeout (attempt 1 < 2) → warm check-in, stays Confirming.
            fsm.handle(Event::ConfirmTimeout, t0 + 62_000);
            // Third timeout (attempt 2 >= 2) → spoken close + deny + back to Idle.
            fsm.handle(Event::ConfirmTimeout, t0 + 93_000);
        }
        Scenario::DestructiveChildLock => {
            fsm.handle(Event::SetChildLock { enabled: true }, t0 + 5);
            fsm.handle(Event::AudioWake, t0 + 10);
            fsm.handle(Event::AudioSpeechStart, t0 + 100);
            fsm.handle(
                Event::SttFinal {
                    transcript: "format the laptop".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 500,
            );
            fsm.handle(
                Event::BrainReplyDestructive {
                    intent_id: format!("intent-{t0}"),
                    summary: "format laptop".to_string(),
                    confirm_keyword: "format-laptop".to_string(),
                },
                t0 + 900,
            );
            // Clear child-lock so subsequent turns aren't permanently silent.
            fsm.handle(Event::SetChildLock { enabled: false }, t0 + 1100);
        }
        Scenario::UncertainThenFinal => {
            fsm.handle(Event::AudioWake, t0 + 10);
            fsm.handle(Event::AudioSpeechStart, t0 + 100);
            fsm.handle(Event::SttUncertain, t0 + 400);
            // After uncertain we're back in Listening; need a fresh
            // speech-start before the next final transcript.
            fsm.handle(Event::AudioSpeechStart, t0 + 800);
            fsm.handle(
                Event::SttFinal {
                    transcript: "set a timer".to_string(),
                    confidence: 0.94,
                    turn_id: None,
                },
                t0 + 1200,
            );
            fsm.handle(
                Event::BrainReply {
                    text: "timer set".to_string(),
                },
                t0 + 1700,
            );
            fsm.handle(Event::TtsEnd, t0 + 2300);
        }
        Scenario::BargeInDuringSpeaking => {
            fsm.handle(Event::AudioWake, t0 + 10);
            fsm.handle(Event::AudioSpeechStart, t0 + 100);
            fsm.handle(
                Event::SttFinal {
                    transcript: "tell me a story".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 500,
            );
            fsm.handle(
                Event::BrainReply {
                    text: "once upon a time".to_string(),
                },
                t0 + 900,
            );
            // Wake during speaking — barge-in cancels TTS, enters listening.
            fsm.handle(Event::AudioWake, t0 + 1100);
            fsm.handle(Event::AudioSpeechStart, t0 + 1200);
            fsm.handle(
                Event::SttFinal {
                    transcript: "never mind".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 1700,
            );
            fsm.handle(
                Event::BrainReply {
                    text: "ok".to_string(),
                },
                t0 + 2000,
            );
            fsm.handle(Event::TtsEnd, t0 + 2400);
        }
        Scenario::MuteThenUnmute => {
            fsm.handle(Event::MuteRequest, t0 + 10);
            // Wake while muted is ignored.
            fsm.handle(Event::AudioWake, t0 + 100);
            fsm.handle(Event::UnmuteRequest, t0 + 200);
            // Now drive a normal turn.
            fsm.handle(Event::AudioWake, t0 + 300);
            fsm.handle(Event::AudioSpeechStart, t0 + 400);
            fsm.handle(
                Event::SttFinal {
                    transcript: "weather please".to_string(),
                    confidence: 0.95,
                    turn_id: None,
                },
                t0 + 900,
            );
            fsm.handle(
                Event::BrainReply {
                    text: "sunny".to_string(),
                },
                t0 + 1300,
            );
            fsm.handle(Event::TtsEnd, t0 + 1900);
        }
    }
}

#[test]
fn ac7_fifty_turn_soak_returns_to_idle_every_turn() {
    let mut fsm = Fsm::new(0);
    let per_turn_ms: u64 = 72_000; // 50 × 72 s > 60 min coverage.
    for i in 0..50_u64 {
        let scenario = SCENARIOS[(i as usize) % SCENARIOS.len()];
        let t0 = i * per_turn_ms;
        drive_turn(&mut fsm, scenario, t0);
        assert_eq!(
            fsm.state().tag(),
            StateTag::Idle,
            "FSM wedged after turn {i} (scenario {:?})",
            scenario_name(scenario),
        );
    }
    let history = fsm.history(256);
    assert!(
        !history.is_empty(),
        "soak generated no transitions — driver bug",
    );
    assert!(
        history.iter().all(|t| t.elapsed_ms <= per_turn_ms * 2),
        "transition recorded an absurd elapsed_ms — clock regression",
    );
}

const fn scenario_name(s: Scenario) -> &'static str {
    match s {
        Scenario::SimpleReply => "SimpleReply",
        Scenario::DestructiveGrant => "DestructiveGrant",
        Scenario::DestructiveDenyNo => "DestructiveDenyNo",
        Scenario::DestructiveTimeout => "DestructiveTimeout",
        Scenario::DestructiveChildLock => "DestructiveChildLock",
        Scenario::UncertainThenFinal => "UncertainThenFinal",
        Scenario::BargeInDuringSpeaking => "BargeInDuringSpeaking",
        Scenario::MuteThenUnmute => "MuteThenUnmute",
    }
}
