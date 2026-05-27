//! `wm-dialog` CLI entrypoint.
//!
//! iter-3 wires a clap dispatcher and `tracing` initialisation for
//! the four PRD §2.6 subcommands (`state`, `mute`/`unmute`,
//! `child-lock`, `say`). The `state` subcommand emits a JSON
//! snapshot of a freshly-initialised [`Fsm`] (suitable for
//! schema-inspection and downstream tooling); a future iter
//! replaces the fresh FSM with a live agorabus query of the running
//! daemon. The remaining subcommands warn-and-exit-2 — they become
//! agorabus producers in iter-4 once the bus schema lands.

#![cfg_attr(not(test), forbid(unsafe_code))]

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use wintermute_dialog::daemon;
use wintermute_dialog::state::{Flags, StateTag};
use wintermute_dialog::{Fsm, StateSnapshot, Transition};

#[derive(Parser, Debug)]
#[command(
    name = "wm-dialog",
    version,
    about = "wintermute conversational-FSM daemon and CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the long-lived daemon: subscribe to `wm.audio.*` / `wm.stt.*`
    /// / `wm.brain.*` and publish `wm.dialog.*` plus the TTS/audio
    /// control topics. Blocks until the agorabus closes the connection.
    Start,
    /// Print the running daemon's FSM snapshot as JSON. iter-3 emits
    /// a fresh-FSM snapshot (no daemon to query yet); a future iter
    /// wires the live agorabus query.
    State {
        /// Number of historical transitions to include (oldest →
        /// newest). 0 = none. PRD §2.6 `state --history N`.
        #[arg(long, default_value_t = 0)]
        history: usize,
    },
    /// Request a global mute. Publishes `wm.dialog.mute_request` once
    /// iter-4 wires the agorabus producer.
    Mute,
    /// Release a global mute. Publishes `wm.dialog.unmute_request`
    /// once iter-4 wires the agorabus producer.
    Unmute,
    /// Toggle the child-lock policy. PRD §2.5: `child_lock = true`
    /// causes destructive intents to be auto-denied silently.
    ChildLock {
        /// `on` or `off`.
        toggle: ChildLockToggle,
    },
    /// Debug: drive the `speaking` state from the CLI. Publishes
    /// `wm.brain.reply` once iter-4 wires the agorabus producer.
    Say {
        /// Text to speak.
        text: String,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum ChildLockToggle {
    On,
    Off,
}

/// JSON-stable snapshot of an [`Fsm`] returned by `wm-dialog state`.
/// Used for the fresh-FSM fallback when the live daemon's snapshot
/// file is missing; live reads use [`StateSnapshot`] directly.
#[derive(Debug, Serialize)]
struct StateReport {
    state: StateTag,
    flags: Flags,
    since_ms: u64,
    history: Vec<Transition>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    drop(tracing_subscriber::fmt().with_env_filter(filter).try_init());
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Command::Start => run_start(),
        Command::State { history } => run_state(history),
        Command::Mute => run_mute(),
        Command::Unmute => run_unmute(),
        Command::ChildLock { toggle } => run_child_lock(toggle),
        Command::Say { text } => run_say(&text),
    }
}

fn run_start() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            error!(error = %err, "wm-dialog start: failed to build tokio runtime");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(daemon::run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "wm-dialog start: daemon exited with error");
            ExitCode::from(1)
        }
    }
}

#[allow(clippy::print_stdout)]
fn run_state(history: usize) -> ExitCode {
    let snapshot_path = daemon::default_snapshot_path();
    match daemon::read_snapshot(&snapshot_path) {
        Ok(Some(mut snap)) => {
            if snap.history.len() > history {
                let start = snap.history.len().saturating_sub(history);
                snap.history = snap.history.split_off(start);
            }
            print_snapshot(&snap)
        }
        Ok(None) => {
            warn!(
                path = %snapshot_path.display(),
                "wm-dialog state: no live daemon snapshot; emitting fresh-FSM fallback"
            );
            print_fresh_fallback(history)
        }
        Err(err) => {
            warn!(
                path = %snapshot_path.display(),
                err = %err,
                "wm-dialog state: snapshot parse failed; emitting fresh-FSM fallback"
            );
            print_fresh_fallback(history)
        }
    }
}

#[allow(clippy::print_stdout)]
fn print_snapshot(snap: &StateSnapshot) -> ExitCode {
    match serde_json::to_string_pretty(snap) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            error!(error = %err, "wm-dialog state: failed to serialise live snapshot");
            ExitCode::from(1)
        }
    }
}

#[allow(clippy::print_stdout)]
fn print_fresh_fallback(history: usize) -> ExitCode {
    let fsm = Fsm::new(0);
    let report = StateReport {
        state: fsm.state().tag(),
        flags: fsm.flags(),
        since_ms: 0,
        history: fsm.history(history),
    };
    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            error!(error = %err, "wm-dialog state: failed to serialise FSM snapshot");
            ExitCode::from(1)
        }
    }
}

fn run_mute() -> ExitCode {
    info!("wm-dialog mute: would publish wm.dialog.mute_request");
    warn!("wm-dialog mute: agorabus producer deferred to iter-4");
    ExitCode::from(2)
}

fn run_unmute() -> ExitCode {
    info!("wm-dialog unmute: would publish wm.dialog.unmute_request");
    warn!("wm-dialog unmute: agorabus producer deferred to iter-4");
    ExitCode::from(2)
}

fn run_child_lock(toggle: ChildLockToggle) -> ExitCode {
    let target = match toggle {
        ChildLockToggle::On => "on",
        ChildLockToggle::Off => "off",
    };
    info!(target, "wm-dialog child-lock: would update bootstrap policy");
    warn!("wm-dialog child-lock: agorabus producer deferred to iter-4");
    ExitCode::from(2)
}

fn run_say(text: &str) -> ExitCode {
    info!(text, "wm-dialog say: would publish wm.brain.reply");
    warn!("wm-dialog say: agorabus producer deferred to iter-4");
    ExitCode::from(2)
}
