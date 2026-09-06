//! Shared types for `restart-policies` — blueprint §12's supervision, made
//! countable.
//!
//! > *Restart policies per node: supervised respawn with exponential backoff
//! > (`restart_delay`×2^n capped by `max_restart_delay`), budget
//! > `max_restarts` within `restart_window`. New incarnation ⇒ new
//! > generation.*
//!
//! Two claims, two manifests, one binary:
//!
//! | Manifest | `max_restarts` | The worker | What it proves |
//! |---|---:|---|---|
//! | `dataflow.yml` | 5 | fails twice, then succeeds | a flaky node **recovers inside its budget**, and the run finishes clean |
//! | `budget-exhausted.yml` | 2 | fails every time | the budget is a **ceiling**: exactly `1 + max_restarts` starts, then the dataflow fails |
//!
//! # How the count is made durable
//!
//! Every incarnation appends one line to `$RESTART_LOG` before it does
//! anything else, so the file is a record of *starts* that survives the
//! process that wrote it — which is the only way to count something that
//! keeps dying. Each line carries the node API's own view of the same fact
//! ([`astrs_node_api::Node::restart_count`] and
//! [`astrs_node_api::Node::generation`]), so the two agree or the test says
//! which one is wrong.
//!
//! Nothing here asserts on *time*. Backoff is `restart_delay × 2^n`, and a
//! test that measured it would be measuring the machine it ran on; what is
//! checked is the count, the generations, and the exit cause.

use serde::{Deserialize, Serialize};

/// The worker's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable naming the file every incarnation appends to.
pub const ENV_RESTART_LOG: &str = "RESTART_LOG";

/// Environment variable naming the JSON summary the surviving incarnation
/// writes.
pub const ENV_SUMMARY_PATH: &str = "RESTART_SUMMARY";

/// Environment variable setting how many incarnations fail before one
/// succeeds.
pub const ENV_FAILURES: &str = "RESTART_FAILURES";

/// How many incarnations fail by default.
pub const DEFAULT_FAILURES: u64 = 2;

/// The exit code a deliberately failing incarnation uses.
///
/// A number nothing else in this workspace produces, so a `DataflowResult`
/// carrying it is unambiguously *this* example's doing rather than a spawn
/// failure or a signal.
pub const FAILURE_EXIT_CODE: u8 = 17;

/// Where the incarnation log goes when the manifest names no path.
#[must_use]
pub fn default_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-restart-policies-log.txt")
}

/// Where the summary goes when the manifest names no path.
#[must_use]
pub fn default_summary_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-restart-policies-summary.json")
}

/// The file every incarnation appends to.
#[must_use]
pub fn log_path() -> std::path::PathBuf {
    path_from_env(ENV_RESTART_LOG, default_log_path)
}

/// The file the surviving incarnation writes.
#[must_use]
pub fn summary_path() -> std::path::PathBuf {
    path_from_env(ENV_SUMMARY_PATH, default_summary_path)
}

/// How many incarnations should fail before one succeeds.
///
/// A number larger than any plausible budget — the `budget-exhausted.yml`
/// manifest sets one — means "every incarnation fails".
#[must_use]
pub fn failure_budget() -> u64 {
    std::env::var(ENV_FAILURES)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_FAILURES)
}

/// Reads a path from `name`, falling back to `default` when it is unset or
/// empty.
fn path_from_env(name: &str, default: impl FnOnce() -> std::path::PathBuf) -> std::path::PathBuf {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default, std::path::PathBuf::from)
}

/// One line of the incarnation log: what one start of the worker knew about
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incarnation {
    /// [`astrs_node_api::Node::restart_count`] — zero for the first start.
    pub restart_count: u64,
    /// [`astrs_node_api::Node::generation`] — the daemon's incarnation stamp.
    pub generation: u64,
    /// Whether this incarnation is going to fail deliberately.
    pub will_fail: bool,
}

impl Incarnation {
    /// Renders the line this incarnation appends to the log.
    ///
    /// One JSON object per line: appendable from a process that may not live
    /// to write another, and readable without a parser that has to understand
    /// a partially written file.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field types
    /// make impossible in practice.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        Ok(format!("{}\n", serde_json::to_string(self)?))
    }

    /// Reads every incarnation out of a log file's text, ignoring a trailing
    /// partial line.
    #[must_use]
    pub fn parse_log(text: &str) -> Vec<Self> {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

/// What the incarnation that finally survived saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartSummary {
    /// How many times this node had been restarted when it succeeded.
    pub restart_count: u64,
    /// Its generation stamp at that point.
    pub generation: u64,
    /// How many ticks it processed before finishing.
    pub ticks: u64,
}

impl RestartSummary {
    /// Renders the summary as pretty JSON.
    ///
    /// # Errors
    ///
    /// As [`Incarnation::to_line`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A log line round-trips, and a whole log parses back in order.
    #[test]
    fn the_incarnation_log_round_trips_line_by_line() {
        let entries = [
            Incarnation {
                restart_count: 0,
                generation: 1,
                will_fail: true,
            },
            Incarnation {
                restart_count: 1,
                generation: 2,
                will_fail: true,
            },
            Incarnation {
                restart_count: 2,
                generation: 3,
                will_fail: false,
            },
        ];
        let text: String = entries
            .iter()
            .map(|entry| entry.to_line().unwrap())
            .collect();
        assert_eq!(Incarnation::parse_log(&text), entries);
    }

    /// A half-written trailing line is ignored rather than failing the read —
    /// the file is appended to by a process that may be killed mid-write.
    #[test]
    fn a_partial_trailing_line_is_ignored() {
        let text = "{\"restart_count\":0,\"generation\":1,\"will_fail\":true}\n{\"restart_c";
        assert_eq!(Incarnation::parse_log(text).len(), 1);
    }

    /// The summary round-trips as JSON, which is how the suite reads it.
    #[test]
    fn the_summary_round_trips_as_json() {
        let summary = RestartSummary {
            restart_count: 2,
            generation: 3,
            ticks: 4,
        };
        let parsed: RestartSummary = serde_json::from_str(&summary.to_json().unwrap()).unwrap();
        assert_eq!(parsed, summary);
    }

    /// Both artefact paths default under the temporary directory, so a run
    /// never writes into the checkout.
    #[test]
    fn the_artefact_paths_default_to_the_temp_dir() {
        assert!(default_log_path().starts_with(std::env::temp_dir()));
        assert!(default_summary_path().starts_with(std::env::temp_dir()));
    }
}
