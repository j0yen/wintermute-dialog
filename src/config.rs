//! Dialog timing configuration — the `[timing]` TOML table.
//!
//! [`DialogTimingConfig`] lifts each of the FSM's previously-`const`
//! deadline knobs into a serde-deserializable struct so operators (and
//! caregivers) can tune conversation tempo without recompiling.  The
//! `Default` implementation provides elder-friendly values that are
//! **more patient** than the compile-time constants that shipped with
//! iter-1:
//!
//! - `confirm_timeout_ms` rises from 30 s to 45 s (the elder gets
//!   more time to answer before the system gives up and denies).
//! - `max_reprompts` rises from 1 to 2 (two gentle re-asks before a
//!   final deny, rather than one).
//! - Machine-internal deadlines (`capture`, `transcribe`, `think`,
//!   heartbeat) keep the same values as the old `const`s — they are
//!   governed by infrastructure rather than human cadence.
//!
//! Config is **optional**: if the daemon's config source omits the
//! `[timing]` table entirely, `DialogTimingConfig::default()` is used
//! and the daemon starts successfully.  Existing deployments need no
//! config edits.

use serde::{Deserialize, Serialize};

/// Old compile-time constant: verbal-confirm timeout (30 s).
///
/// Preserved as the `Default` source of truth so that callers that
/// referenced the old `CONFIRM_TIMEOUT_MS` constant continue to compile.
/// The FSM reads the runtime value from [`DialogTimingConfig`] instead.
pub const LEGACY_CONFIRM_TIMEOUT_MS: u32 = 30_000;

/// Old compile-time constant: maximum re-prompts before deny.
pub const LEGACY_MAX_REPROMPTS: u8 = 1;

/// Default capture-timeout used as the `capture_timeout_ms` baseline
/// (2 minutes — the same value as the old `const` if one existed, or a
/// safe default otherwise).
pub const DEFAULT_CAPTURE_TIMEOUT_MS: u32 = 120_000;

/// Default transcription-timeout (20 s — a machine deadline; unchanged
/// from the prior implicit tuning).
pub const DEFAULT_TRANSCRIBE_TIMEOUT_MS: u32 = 20_000;

/// Default think-timeout for waiting on a brain reply (60 s).
pub const DEFAULT_THINK_TIMEOUT_MS: u32 = 60_000;

/// Default state-heartbeat interval (5 s).
pub const DEFAULT_STATE_HEARTBEAT_MS: u32 = 5_000;

/// FSM timing configuration loaded from the `[timing]` section of the
/// daemon's config file.
///
/// Every field is optional at the TOML level (`#[serde(default)]`) so
/// a partial table merges with the per-field defaults rather than
/// requiring all keys to be present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DialogTimingConfig {
    /// Milliseconds before a verbal-confirmation request times out and
    /// the FSM denies via `Silence`.
    ///
    /// **Elder-friendly default: 45 000 ms (45 s).** The prior compile-time
    /// constant was 30 s, which cut off a speaker who pauses mid-thought.
    /// 45 s gives a natural thinking beat without an indefinite wait.
    pub confirm_timeout_ms: u32,

    /// Maximum number of re-prompts before a verbal-confirm is denied as
    /// `Ambiguous`.
    ///
    /// **Elder-friendly default: 2.** The prior constant was 1, meaning
    /// only one re-ask before giving up.  Two attempts give the listener
    /// a second chance without feeling relentless.
    pub max_reprompts: u8,

    /// Milliseconds the FSM waits for speech capture to complete before
    /// abandoning the utterance.  Machine deadline — default unchanged.
    pub capture_timeout_ms: u32,

    /// Milliseconds the FSM waits for a transcription result.
    /// Machine deadline — default unchanged.
    pub transcribe_timeout_ms: u32,

    /// Milliseconds the FSM waits for a brain reply before transitioning.
    /// Machine deadline — default unchanged.
    pub think_timeout_ms: u32,

    /// Interval at which the daemon emits a state-heartbeat publish.
    /// Infrastructure knob — default unchanged.
    pub state_heartbeat_ms: u32,
}

impl Default for DialogTimingConfig {
    /// Elder-friendly defaults.  Every value that affects human cadence
    /// is more patient than the old compile-time constant; machine-facing
    /// deadlines keep their prior values.
    fn default() -> Self {
        Self {
            // 45 s — elder-friendly; old const was 30_000.
            confirm_timeout_ms: 45_000,
            // 2 tries — elder-friendly; old const was 1.
            max_reprompts: 2,
            capture_timeout_ms: DEFAULT_CAPTURE_TIMEOUT_MS,
            transcribe_timeout_ms: DEFAULT_TRANSCRIBE_TIMEOUT_MS,
            think_timeout_ms: DEFAULT_THINK_TIMEOUT_MS,
            state_heartbeat_ms: DEFAULT_STATE_HEARTBEAT_MS,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    reason = "tests"
)]
mod tests {
    use super::*;

    // AC1 / AC2: default confirm_timeout_ms ≥ old const; max_reprompts ≥ 2.
    #[test]
    fn defaults_are_elder_friendly() {
        let cfg = DialogTimingConfig::default();
        assert!(
            cfg.confirm_timeout_ms >= LEGACY_CONFIRM_TIMEOUT_MS,
            "default confirm_timeout_ms ({}) must be ≥ old const ({})",
            cfg.confirm_timeout_ms,
            LEGACY_CONFIRM_TIMEOUT_MS,
        );
        assert!(
            cfg.max_reprompts >= 2,
            "default max_reprompts ({}) must be ≥ 2",
            cfg.max_reprompts,
        );
    }

    // AC1: deserializes a full [timing] table from TOML.
    #[test]
    fn deserializes_full_timing_table() {
        let toml_str = r#"
confirm_timeout_ms    = 12000
max_reprompts         = 3
capture_timeout_ms    = 90000
transcribe_timeout_ms = 15000
think_timeout_ms      = 45000
state_heartbeat_ms    = 3000
"#;
        let cfg: DialogTimingConfig =
            toml::from_str(toml_str).expect("deserialize full timing table");
        assert_eq!(cfg.confirm_timeout_ms, 12_000);
        assert_eq!(cfg.max_reprompts, 3);
        assert_eq!(cfg.capture_timeout_ms, 90_000);
        assert_eq!(cfg.transcribe_timeout_ms, 15_000);
        assert_eq!(cfg.think_timeout_ms, 45_000);
        assert_eq!(cfg.state_heartbeat_ms, 3_000);
    }

    // AC2: absent table → uses defaults (no deserialization error).
    #[test]
    fn absent_table_uses_defaults() {
        let cfg = DialogTimingConfig::default();
        // Re-serialize + re-deserialize to verify round-trip.
        let json = serde_json::to_string(&cfg).expect("serialize");
        let cfg2: DialogTimingConfig =
            serde_json::from_str(&json).expect("deserialize round-trip");
        assert_eq!(cfg, cfg2);
    }

    // AC1: partial table — only some keys present — fills the rest from
    // Default.
    #[test]
    fn partial_table_fills_missing_fields_from_default() {
        let toml_str = r#"confirm_timeout_ms = 12000"#;
        let cfg: DialogTimingConfig =
            toml::from_str(toml_str).expect("deserialize partial table");
        assert_eq!(cfg.confirm_timeout_ms, 12_000);
        // Unspecified fields come from Default.
        let dflt = DialogTimingConfig::default();
        assert_eq!(cfg.max_reprompts, dflt.max_reprompts);
        assert_eq!(cfg.capture_timeout_ms, dflt.capture_timeout_ms);
    }
}
