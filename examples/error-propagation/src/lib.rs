//! Shared types for `error-propagation` — blueprint §12's error path, made
//! observable from inside the graph.
//!
//! > *Error propagation: a failing node emits `NodeFailed` to graph peers
//! > (visible on `astrs/status`), the dataflow FSM aggregates per-node exit
//! > causes into a `DataflowResult` with **typed causes (not strings)**.*
//!
//! ```text
//!   [source] ──readings──► [watcher] ──► $FAULT_REPORT (JSON)
//!       │                      ▲
//!       │  exits 23            │  Event::NodeFailed { peer, cause }
//!       └──────────────────────┘  Event::InputClosed { id, source, reason }
//!            astrs/status
//! ```
//!
//! The watcher publishes nothing and subscribes to two things: the readings
//! it is meant to process, and `astrs/status` (§8.4), the reserved
//! lifecycle stream a supervisor-shaped node reads to react to its peers. It
//! writes down what it saw, so a test can assert on the *events the
//! application received* rather than on a log line or an exit code alone.
//!
//! # What "typed cause" means here
//!
//! `Event::NodeFailed` carries an
//! [`astrs_wire::NodeExitCause`] — an enum, with the exit code inside it — not
//! a rendered string. The watcher records both the discriminant name and the
//! rendered text, and the conformance suite asserts on the discriminant: a
//! change that turned the typed cause into a message would fail here rather
//! than pass with a different-looking string.

use serde::{Deserialize, Serialize};

/// The source's output port, and the watcher's input.
pub const READINGS_PORT: &str = "readings";

/// The watcher's `astrs/status` input.
pub const STATUS_PORT: &str = "status";

/// The source's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable naming the JSON file the watcher writes.
pub const ENV_REPORT_PATH: &str = "FAULT_REPORT";

/// Environment variable setting how many readings the source publishes before
/// it fails.
pub const ENV_READINGS: &str = "FAULT_READINGS";

/// How many readings the source publishes by default.
pub const DEFAULT_READINGS: u64 = 3;

/// The exit code the source fails with.
///
/// A number nothing else in this workspace produces, so a cause carrying it is
/// unambiguously *this* example's doing rather than a spawn failure or a
/// signal.
pub const FAILURE_EXIT_CODE: u8 = 23;

/// Where the watcher writes its report when the manifest names no path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-error-propagation-report.json")
}

/// The JSON file this run's watcher writes.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// How many readings the source should publish before failing.
#[must_use]
pub fn reading_budget() -> u64 {
    std::env::var(ENV_READINGS)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_READINGS)
}

/// One peer failure, as the watcher received it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerFailure {
    /// The node that failed.
    pub peer: String,
    /// The *typed* cause's variant name — `exit_code`, `signal`, … — taken
    /// from [`astrs_wire::NodeExitCause::kind_name`] rather than from its
    /// rendered text.
    pub cause_kind: String,
    /// The rendered cause, for a human reading the artefact.
    pub cause: String,
    /// The exit code the cause carries, when it is an exit code at all.
    pub exit_code: Option<i32>,
}

impl PeerFailure {
    /// Records one [`astrs_wire::NodeExitCause`] as the watcher saw it.
    ///
    /// Keeps the *discriminant* alongside the rendered text, which is what
    /// makes "typed causes, not strings" checkable: a change that turned the
    /// cause into a message would show up as a missing `cause_kind`, not as a
    /// different-looking sentence.
    #[must_use]
    pub fn new(peer: &astrs_wire::NodeId, cause: &astrs_wire::NodeExitCause) -> Self {
        Self {
            peer: peer.as_str().to_owned(),
            cause_kind: cause.kind_name().to_owned(),
            cause: cause.to_string(),
            exit_code: match cause {
                astrs_wire::NodeExitCause::ExitCode { code } => Some(*code),
                _ => None,
            },
        }
    }
}

/// What the watcher saw, written as JSON when its inputs close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultReport {
    /// How many readings arrived before the producer failed.
    pub readings: u64,
    /// Every `NodeFailed` this node was told about, in arrival order.
    pub failures: Vec<PeerFailure>,
    /// Every input that closed, and the reason given.
    pub closed_inputs: Vec<String>,
    /// Whether the watcher finished because it was told the graph was done,
    /// rather than being killed.
    pub finished_cleanly: bool,
}

impl FaultReport {
    /// An empty report.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            readings: 0,
            failures: Vec::new(),
            closed_inputs: Vec::new(),
            finished_cleanly: false,
        }
    }

    /// Whether the watcher was told about `peer` failing.
    #[must_use]
    pub fn saw_failure_of(&self, peer: &str) -> bool {
        self.failures.iter().any(|failure| failure.peer == peer)
    }

    /// The failure recorded for `peer`, if any.
    #[must_use]
    pub fn failure_of(&self, peer: &str) -> Option<&PeerFailure> {
        self.failures.iter().find(|failure| failure.peer == peer)
    }

    /// Renders the report as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field types
    /// make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

impl Default for FaultReport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn failure(peer: &str, code: i32) -> PeerFailure {
        PeerFailure {
            peer: peer.to_owned(),
            cause_kind: "exit_code".to_owned(),
            cause: format!("exited with code {code}"),
            exit_code: Some(code),
        }
    }

    /// A report answers "did you hear about this peer" without the caller
    /// scanning it.
    #[test]
    fn a_report_finds_the_peer_it_was_told_about() {
        let mut report = FaultReport::new();
        report.readings = 3;
        report.failures.push(failure("source", 23));
        assert!(report.saw_failure_of("source"));
        assert!(!report.saw_failure_of("planner"));
        assert_eq!(
            report.failure_of("source").and_then(|f| f.exit_code),
            Some(23)
        );
        assert!(report.failure_of("planner").is_none());
    }

    /// An empty report is the default and claims nothing.
    #[test]
    fn an_empty_report_claims_nothing() {
        let report = FaultReport::default();
        assert_eq!(report.readings, 0);
        assert!(report.failures.is_empty());
        assert!(!report.finished_cleanly);
        assert!(!report.saw_failure_of("source"));
    }

    /// The report round-trips as JSON, which is how the suite reads it.
    #[test]
    fn the_report_round_trips_as_json() {
        let mut report = FaultReport::new();
        report.readings = 3;
        report.failures.push(failure("source", 23));
        report.closed_inputs.push("readings".to_owned());
        report.finished_cleanly = true;
        let parsed: FaultReport = serde_json::from_str(&report.to_json().unwrap()).unwrap();
        assert_eq!(parsed, report);
    }

    /// The artefact path defaults under the temporary directory, so a run
    /// never writes into the checkout.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }
}
