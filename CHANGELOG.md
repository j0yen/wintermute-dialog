# Changelog

## vv0.9.0 — 2026-06-13

Wire wm-dialog ClaimGuard: hold agorabus://daemon/wm-dialog for process lifetime; release on SIGTERM before exit. Enables rollout warm-swap claim detection.

## v0.8.0 — 2026-06-12

Add brain-reply and STT fallback timeouts (voice-dialog-fallback)
- FallbackBrainTimeout (WM_DIALOG_BRAIN_TIMEOUT_MS, default 8s): after wm.stt.final, speak canned phrase if brain doesn't reply
- FallbackSttTimeout (WM_DIALOG_STT_TIMEOUT_MS, default 12s): after wm.audio.speech.end, speak canned phrase if no STT result
- Both cancel on the expected event; all 6 ACs tested

## v0.7.0 — 2026-06-05

propagate turn_id from wm.stt.final onto wm.dialog.turn.user / wm.dialog.state (PRD lucid-turn-id AC3/AC5)

## v0.6.0 — 2026-05-30

Add DegradeBank with per-kind rotating cursors to wintermute-dialog.
SttUncertain, TranscribeTimeout, BrainError, and ThinkTimeout each get
distinct phrase sets; consecutive failures of the same kind vary output
via round-robin rotation. Preserves backward-compat free functions and
the legacy "didn't catch" AC6 contract. +16 tests.

## v0.5.0 — 2026-05-30

Adds deterministic distress fast-path to wintermute-dialog (family-distress PRD).
`src/distress.rs`: classify() phrase bank (Hard/Soft severity), DistressFsm with immediate Hard fire + Soft confirm loop, FamilyDistress envelope, assurance/failure phrases sourced from dedicated phrase bank (parallel to degrade.rs). Hard distress publishes wm.family.distress + speaks assurance with no API call; Soft prompts confirmation first; on_ack() failure is never silent. lib.rs: pub mod distress + re-exports. 32 unit tests covering all PRD §3 ACs.

## v0.4.0 — 2026-05-30

## wintermute-dialog-turn-fsm

Ships the complete PRD turn-taking FSM transitions:

- New `degrade.rs` phrase bank for heard-nothing and think-error paths
- New timeout events: `CaptureTimeout` (8s), `TranscribeTimeout` (3s), `ThinkTimeout` (10s)
- New `BrainError` event routes through degrade-think-error phrases
- New actions: `PublishDialogAttention`, `PublishDialogHeard`, `PublishDialogUnheard`, `PublishDialogTimeout` + capture/transcribe/think timer arms
- `SttUncertain` now correctly returns FSM to Idle (not Listening) with degrade
- Barge-in from Speaking now emits attention signal + arms capture timer
- 95 lib tests pass (+16 net, exceeds AC1 requirement of +10)

## v0.3.0 — 2026-05-30

Adds wm.family.* topic contract and family-intent matcher to wintermute-dialog.
Defines four topic constants (TOPIC_FAMILY_MESSAGE/DISTRESS/ACK/REPLY), the
FamilyMessage/FamilyAck/FamilyReply envelope types, a deterministic API-independent
intent matcher (tell/message/let know/call + enrolled-name matching), and
FamilyFsm for the pending-ack wait with timeout and inbound-reply routing.
Also fixes pre-existing clippy lints (indexing_slicing in silence.rs,
as_conversions in fsm.rs) and updates agorabus dep to 0.8.

## v0.3.0 — 2026-05-29

earshot-gentle-reprompt: patient, spoken, more-than-once silence path.

`ConfirmTimeout` now drives a configurable escalating patience sequence instead of a single reprompt followed by silent idle. Intermediate timeouts (attempts < `max_reprompts`) emit warm check-in phrases ("I'm still here — take your time.", "Whenever you're ready.") and restart the confirm timer; the final timeout (attempts >= `max_reprompts`) emits a spoken close line ("I'll be right here when you need me.") before transitioning to Idle. `DenyReason::Silence` is still recorded. Silence phrases live in a new `silence` module, separate from `degrade.rs`. No new FSM states added.

## v0.2.0 — 2026-05-30

Lift the FSM's timing constants into a `[timing]` config table (`DialogTimingConfig`) with elder-friendly defaults: `confirm_timeout_ms = 45_000` (was 30 s) and `max_reprompts = 2` (was 1). The FSM and daemon read all deadlines from the runtime config; no timing magic numbers remain in transition code. Absent `[timing]` table → elder-friendly defaults, so existing deployments need no config edits.

## v0.1.1 — 2026-05-28

Fix post-announce bus-startup defect (PRD-wintermute-fleet-bus-startup-defect).

The announce-before-subscribe fix that shipped overnight was install-stale, not
source-buggy: the binaries under ~/.local/bin/ predated the fix, while the source
already had the dual-Client + announce-first pattern. Tightened the agorabus
path-dependency pin from a wildcard/^0.1 to ^0.3 (agorabus 0.3.0's let_chains
need system cargo 1.95), rebuilt, and reinstalled so the systemd-launched daemons
run post-fix bytes. Daemons now survive a 60s soak (NRestarts=0) and round-trip
their subscribed topics. Note: AC3-strict (peer presence after the 60s window)
is deferred to PRD-wintermute-fleet-bus-heartbeat-keepalive — these daemons still
lack a post-announce heartbeat, so the bus prunes them from the peer snapshot.
