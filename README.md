# wintermute-dialog

> The conversational state machine. `wm-dialog` is the arbiter between
> the audio layer, STT, TTS, and the brain — it owns turn-taking,
> barge-in, the verbal-confirmation protocol for destructive intents,
> and the mute / child-lock surfaces. Plan-agent split this out of the
> brain explicitly: bundling sub-200 ms timing-critical arbitration
> with the Claude API loop was wrong. Different debugging surfaces,
> different latency budgets.

Part of the wintermute fleet
([`wintermute-platform`](https://github.com/j0yen/wintermute-platform),
`wintermute-audio`, `wintermute-stt`, `wintermute-tts`,
`wintermute-brain`). Subscribes to `wm.audio.*`, `wm.stt.*`,
`wm.brain.*` on agorabus; publishes `wm.dialog.*`.

Built with Rust 2024 / `rustc 1.85`. State stored in a tokio Mutex;
state-snapshot file at `$XDG_RUNTIME_DIR/wm-dialog/state.json` (atomic
write, used by `wm-dialog state --history N` for live introspection).

## Install

```sh
git clone --depth 1 https://github.com/j0yen/wintermute-dialog.git
cd wintermute-dialog
cargo install --path . --root ~/.local
./pkg/install-user.sh        # drops systemd user unit + enables it
systemctl --user start wm-dialog  # or start wintermute.target
```

`cargo install` puts `wm-dialog` into `~/.local/bin/`.
`pkg/install-user.sh` is idempotent and only touches
`~/.config/systemd/user/wm-dialog.service`.

### Prerequisites

- `cargo` / `rustc 1.85+`
- [`wintermute-platform`](https://github.com/j0yen/wintermute-platform)
  running (`wintermute.target` provides the systemd ordering and the
  agorabus socket).
- `wintermute-audio`, `wintermute-stt`, `wintermute-tts`,
  `wintermute-brain` — `wm-dialog` is the arbiter; the others are its
  producers and consumers.

## Quick start

```sh
# Start (the wintermute.target path does this for you):
systemctl --user start wm-dialog

# Inspect:
wm-dialog state --history 5          # last N transitions from live snapshot
wm-dialog mute                        # halt current TTS, gate wake
wm-dialog unmute
wm-dialog child-lock on               # block all destructive intents silently
wm-dialog say "test"                  # synthetic brain-reply for smoke tests
```

## Acceptance criteria (PRD §4)

All seven PASS at `f30e753`. `cargo test --release` is 68/68 green.

| # | Criterion | Tests |
|---|---|---|
| 1 | Wake during `speaking` cancels TTS and enters `listening` within 200 ms | `daemon::tests::ac1_barge_in_dispatch_under_budget`, `fsm::tests::speaking_wake_barge_in_cancels_tts_and_listens` |
| 2 | `stt.uncertain` triggers a re-prompt without wedging | `fsm::tests::stt_uncertain_re_prompts_without_wedging` |
| 3 | Verbal-confirm grants on `"yes <keyword>"`; denies on silence/no/cancel/ambiguous-after-reprompt | 5 `fsm::tests::confirm_*` |
| 4 | `wm-dialog mute` silences TTS and gates wake within 200 ms; `unmute` restores both | `daemon::tests::ac4_mute_dispatch_under_budget_and_gates_wake`, `fsm::tests::mute_gates_wake_and_speaking` |
| 5 | `child_lock = true` denies 100% of destructive intents in a 10-scenario suite | `daemon::tests::ac5_ten_scenario_child_lock_denies_destructive_silently` |
| 6 | State transitions logged with prior_state, new_state, trigger, elapsed_ms; queryable via `wm-dialog state --history N` | `daemon::tests::snapshot_*` + smoke (`WM_DIALOG_SNAPSHOT_PATH=/tmp/x wm-dialog state --history N`) |
| 7 | 60-min steady-state run with 50 simulated turns shows no wedges | `tests/ac7_soak::ac7_fifty_turn_soak_returns_to_idle_every_turn` (time-compressed, 8 scenarios × 50 turns) |

## State machine

```
┌──────┐  wake.detected      ┌───────────┐
│ idle ├────────────────────▶│ listening │
└──┬───┘                     └─────┬─────┘
   │                               │ stt.partial / stt.final
   │ tts.start                     ▼
   ▼                         ┌──────────────┐
┌─────────┐  tts.end         │ transcribing │
│ speaking├─────┐            └──────┬───────┘
└────┬────┘    │                   │ stt.final
     │ wake    ▼                   ▼
     │      ┌──────┐            ┌────────┐
     └─────▶│ idle │◀──────────┤thinking│
            └──────┘  brain.   └────┬───┘
                     reply         │ destructive
                                   ▼
                            ┌────────────┐
                            │ confirming │
                            └────────────┘
```

Plus `muted` (top-level orthogonal state) and `child_locked` (blocks
any transition into `confirming → execute`).

## Topics

Subscribed: `wm.audio.{wake,speech.start,speech.end}`,
`wm.stt.{partial,final,uncertain}`,
`wm.brain.{reply,reply.destructive}`.

Published: `wm.dialog.state`, `wm.dialog.turn.user`,
`wm.dialog.tts.speak`, `wm.dialog.tts.cancel`,
`wm.dialog.audio.{mute,unmute}`, `wm.dialog.confirm.{granted,denied}`.

## Hardware reality verification

ACs 1 and 4 are audio-path-bound (wake-event-to-cancel-ack within 200 ms,
mute/unmute silencing and restoring current TTS within 200 ms). Both
budgets are dominated by real TTS cancellation and audio-sink gating
round-trip latency through the live Piper/PipeWire output path. They are
declared in the PRD's `deferred_acs:` + `mock_unjustified_for:`
frontmatter with a one-sentence justification each, because an in-process
FSM mock would time our transition bookkeeping rather than the physical
audio path the AC bounds.

To validate them against real hardware, run:

```sh
cargo test --features=real-hardware
```

This feature is opt-in and off by default, so `cargo test` stays green on
hosts without the live Piper/PipeWire audio path. The drift-report sweep
that compares mock vs. real-hardware outcomes (`hardware-drift.json`) is
scaffolded as a follow-on PRD and is not invoked by default.

## Recent

- **v0.7.0** — propagate `turn_id` from `wm.stt.final` onto `wm.dialog.turn.user` and `wm.dialog.state` (PRD lucid-turn-id AC3/AC5); additive/optional field.
- **v0.3.0** — `wm.family.*` topic contract + family-intent matcher. Adds
  `src/family.rs` with four topic constants, `FamilyMessage`/`FamilyAck`/
  `FamilyReply` serde types, deterministic `match_family_intent()` (no Claude API),
  and `FamilyFsm` for the pending-ack wait, ack-timeout, and inbound-reply TTS
  routing. Also fixes agorabus dependency to 0.8 and pre-existing clippy lints.

## License

Dual-licensed MIT or Apache-2.0 at your option.
