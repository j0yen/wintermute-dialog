//! `wintermute-dialog` — conversational state machine for the
//! wintermute fleet.
//!
//! iter-2 surface: a pure-data finite state machine ([`Fsm`]) over
//! the six conversational states (idle, listening, transcribing,
//! thinking, speaking, confirming) plus the two orthogonal flags
//! (`muted`, `child_locked`). Driven by typed [`Event`]s, emits typed
//! [`Action`]s the daemon loop translates into agorabus publishes and
//! TTS / audio calls.
//!
//! The agorabus subscriber + publisher wiring, the systemd unit, and
//! the `wm-dialog` CLI surface (`state`, `mute`, `child-lock`, `say`,
//! `state --history N`) land in subsequent iterations per
//! `PRD-wintermute-dialog.md` §2.
//!
//! earshot-dialog-timing: timing constants are now deployment-tunable
//! via [`config::DialogTimingConfig`].  Callers that previously relied
//! on the `const` values can use [`fsm::CONFIRM_TIMEOUT_MS`] and
//! [`fsm::MAX_REPROMPTS`] as reference values; the FSM itself reads
//! from the runtime config passed to [`Fsm::with_timing`].

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod action;
pub mod bus;
pub mod config;
pub mod daemon;
pub mod event;
pub mod family;
pub mod fsm;
pub mod silence;
pub mod state;

pub use action::{Action, DenyReason};
pub use config::DialogTimingConfig;
pub use daemon::{
    DEFAULT_SNAPSHOT_HISTORY_N, StateSnapshot, default_snapshot_path, read_snapshot,
    write_snapshot_atomic,
};
pub use event::{Event, EventTag};
pub use family::{
    DEFAULT_ACK_TIMEOUT_MS, DEFAULT_RECIPIENT, FamilyAck, FamilyAction, FamilyFsm, FamilyMessage,
    FamilyReply, FamilyState, TOPIC_FAMILY_ACK, TOPIC_FAMILY_DISTRESS, TOPIC_FAMILY_MESSAGE,
    TOPIC_FAMILY_REPLY, Urgency, match_family_intent,
};
pub use fsm::{CONFIRM_TIMEOUT_MS, DEFAULT_HISTORY_CAPACITY, Fsm, MAX_REPROMPTS, Transition};
pub use state::{ConfirmContext, Flags, State, StateTag};
