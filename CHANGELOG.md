# Changelog

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
