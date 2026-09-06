//! Supervision — restart policies, deadlines, exit causes (§12).
//!
//! | Module | Concern |
//! |---|---|
//! | [`exit`] | Turning an `ExitStatus` plus the daemon's intent into a typed [`astrs_wire::NodeExitCause`] |
//! | [`history`] | The sliding restart-budget window: total restarts (the backoff exponent) versus in-window restarts (the budget) |
//! | [`policy`] | The decision itself — restart after a backoff, or stay down with a recorded cause |
//! | [`watchdog`] | Generation-stamped deadlines and the `SIGTERM`→`SIGKILL` finish-straggler ladder |
//! | [`tasks`] | The background tasks that turn a blocking wait — `wait(2)`, a pipe, a backoff — into one internal event |
//!
//! The first four are pure functions of their inputs plus an
//! [`std::time::Instant`] the caller supplies: none of them spawns, signals,
//! logs or reads a clock, which is what keeps the restart *timing* testable
//! without a stopwatch and reproducible under `--deterministic` replay (§14).
//! [`tasks`] is the one exception, and deliberately the thinnest module here —
//! it holds the `await` points the other four must not have.
//!
//! # The life of one failure
//!
//! ```text
//!   waiter task sees the child exit
//!            │  ExitStatus + ExitIntent
//!            ▼
//!   exit::classify_with_intent ──► NodeExitCause
//!            │
//!            ▼
//!   policy::decide(config, cause, history, daemon_initiated, now)
//!       │                                    │
//!       ├─ Restart { delay } ────────────────┤
//!       │      generation += 1               │
//!       │      new NodeConfig blob           │
//!       │      new segment keys              │
//!       │                                    │
//!       └─ Stop { cause } ───────────────────┴─► DataflowResult::record
//!                                                NodeFailed fan-out to peers
//! ```
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::supervise::{ExitIntent, RestartHistory, classify_with_intent, decide};
//! use astrs_wire::{DurationMs, RestartConfig, RestartPolicy};
//!
//! # #[cfg(unix)] {
//! use std::os::unix::process::ExitStatusExt;
//! let config = RestartConfig {
//!     policy: RestartPolicy::OnFailure,
//!     max_restarts: Some(3),
//!     restart_delay: DurationMs::new(250),
//!     max_restart_delay: DurationMs::from_secs(30),
//!     restart_window: DurationMs::from_secs(60),
//! };
//! let mut history = RestartHistory::new(config.restart_window.to_duration());
//!
//! let crashed = std::process::ExitStatus::from_raw(11); // SIGSEGV
//! let cause = classify_with_intent(crashed, ExitIntent::Running);
//! let decision = decide(&config, &cause, &mut history, false, Instant::now());
//!
//! assert_eq!(decision.delay(), Some(Duration::from_millis(250)));
//! # }
//! ```

pub mod exit;
pub mod history;
pub mod policy;
pub mod tasks;
pub mod watchdog;

pub use exit::{
    ExitIntent, classify, classify_with_intent, describe, is_crash_signal, signal_name, signal_of,
    warrants_peer_notification,
};
pub use history::RestartHistory;
pub use policy::{RestartDecision, backoff_ladder, config_for, decide, next_backoff};
pub use tasks::{
    MAX_CAPTURED_LINE, TRUNCATION_MARKER, spawn_log_pump, spawn_restart_timer, spawn_waiter,
    truncate_line,
};
pub use watchdog::{
    ArmedDeadline, DeadlineTable, Escalation, EscalationStep, Expiry, FinishWatchdogSet,
};
