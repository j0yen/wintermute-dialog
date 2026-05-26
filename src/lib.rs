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

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod action;
pub mod bus;
pub mod daemon;
pub mod event;
pub mod fsm;
pub mod state;

pub use action::{Action, DenyReason};
pub use event::{Event, EventTag};
pub use fsm::{CONFIRM_TIMEOUT_MS, DEFAULT_HISTORY_CAPACITY, Fsm, MAX_REPROMPTS, Transition};
pub use state::{ConfirmContext, Flags, State, StateTag};
