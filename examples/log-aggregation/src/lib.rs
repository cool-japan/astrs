//! Shared types for `log-aggregation` — multi-node structured logs
//! collected through the daemon's `astrs/logs` virtual source (blueprint
//! §8.4, §13).
//!
//! ```text
//!   [sensor-a] ──┐
//!                ├──► astrs/logs ──► [aggregator]
//!   [sensor-b] ──┘
//! ```
//!
//! Two `log-worker` instances each emit a deterministic run of marker-tagged
//! log lines, cycling through every severity level. `aggregator` reads them
//! all back off the *virtual* `astrs/logs` input — no `node/output` edge
//! produces it; the daemon synthesizes it from log capture and its own
//! records (§8.4) — and tallies which `(node, seq)` markers it actually saw.
//!
//! # Two `LogRecord` types, and why this crate only trusts one
//!
//! `astrs-node-api`'s `Node::log`/`log_with_fields` build an
//! **`astrs_wire::LogRecord`** — the frame a node sends *to* the daemon.
//! What a subscriber reads back *off* `astrs/logs` is a different,
//! same-named type: **`astrs_log::LogRecord`**, documented on that crate's
//! own `record` module as "the JSON payload format carried on
//! `astrs/logs/*` virtual inputs". This crate depends on `astrs-log` for
//! that reason — `aggregator` decodes the payload as `astrs_log::LogRecord`,
//! never `astrs_wire::LogRecord`, and this module only ever names the
//! former.
//!
//! # Why the marker lives in the message text, not in structured fields
//!
//! `Node::log_with_fields` can attach a `BTreeMap<String, String>` to a
//! record, which would be a more structured way to carry a marker. This
//! example does not use it: the message text is the one field both
//! `LogRecord` types unambiguously agree carries the same string
//! end-to-end, and this example's claim (every marker a worker sent is a
//! marker the aggregator saw) should not also depend on exactly how a
//! structured field survives the wire → virtual-source conversion — a
//! question this crate leaves to whichever example is written to answer it
//! specifically.
//!
//! # Why `astrs/logs` needs no level filter
//!
//! `astrs/logs` alone (no `/level` segment) is deliberately unfiltered
//! (§8.4): the workers cycle through every severity on purpose, so a
//! level-restricted subscription would silently under-count by design
//! rather than by bug. Everything that is *not* a marker line — the
//! daemon's own records, another node's captured stdout/stderr — still
//! arrives, and [`LogTally::accept`] is what tells them apart.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The aggregator's sole input: the unfiltered `astrs/logs` virtual source.
pub const LOGS_PORT: &str = "logs";
/// A worker's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable naming this worker instance's own identity.
pub const ENV_WORKER_ID: &str = "LOG_WORKER_ID";
/// Environment variable overriding how many records one worker emits.
pub const ENV_WORKER_RECORDS: &str = "LOG_WORKER_RECORDS";
/// Environment variable naming the JSON file the aggregator writes its
/// [`LogTally`] to.
pub const ENV_REPORT_PATH: &str = "LOG_AGGREGATION_REPORT";

/// The worker ids the committed manifest spawns, in the order they appear
/// there.
pub const WORKER_IDS: &[&str] = &["sensor-a", "sensor-b"];
/// How many records one worker emits by default.
pub const DEFAULT_RECORDS_PER_WORKER: u64 = 6;
/// The severity cycle a worker's records step through, in order, wrapping —
/// so a run of six records exercises every level at least once.
pub const LEVEL_CYCLE: &[&str] = &["trace", "debug", "info", "warn", "error"];
/// Every marker message starts with this, followed by its sequence number —
/// see [`marker_message`]/[`seq_of_marker`].
pub const MARKER_PREFIX: &str = "log-aggregation marker seq=";

/// This process's own worker identity, from [`ENV_WORKER_ID`], defaulting to
/// `"worker"` for a manual run outside the manifest.
#[must_use]
pub fn worker_id() -> String {
    std::env::var(ENV_WORKER_ID).unwrap_or_else(|_| "worker".to_owned())
}

/// How many records this run's worker should emit.
#[must_use]
pub fn records_per_worker() -> u64 {
    std::env::var(ENV_WORKER_RECORDS)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_RECORDS_PER_WORKER)
}

/// Where the aggregator writes its report when the manifest names no path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-log-aggregation-report.json")
}

/// The JSON file this run's aggregator writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// The severity label a worker's `seq`-th record should carry — a plain
/// string, deliberately independent of either crate's own `LogLevel` type
/// (see the crate docs' "Two `LogRecord` types" section): a worker converts
/// this to `astrs_wire::LogLevel` to send, and the aggregator reads it back
/// out of `astrs_log::LogLevel::as_str()`, and neither has to agree on
/// anything beyond the string.
#[must_use]
pub fn level_label_for(seq: u64) -> &'static str {
    LEVEL_CYCLE[(seq as usize) % LEVEL_CYCLE.len()]
}

/// The message text a worker's `seq`-th marker record carries.
#[must_use]
pub fn marker_message(seq: u64) -> String {
    format!("{MARKER_PREFIX}{seq}")
}

/// The sequence number encoded in a marker message, if `message` is one.
///
/// [`None`] for any message this example's own workers did not produce —
/// which is most of what actually arrives on `astrs/logs` (see the crate
/// docs' "Why `astrs/logs` needs no level filter" section).
#[must_use]
pub fn seq_of_marker(message: &str) -> Option<u64> {
    message.strip_prefix(MARKER_PREFIX)?.trim().parse().ok()
}

/// The aggregator's running tally, and the report it writes once its budget
/// is spent (or a deadline passes).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LogTally {
    /// Every marker `seq` seen, keyed by the producing node's id.
    pub seen: BTreeMap<String, BTreeSet<u64>>,
    /// How many marker records were seen at each severity label.
    pub level_counts: BTreeMap<String, u64>,
    /// Total marker records accepted, across every worker.
    pub total_matched: u64,
    /// Every `astrs/logs` payload that failed to decode as a
    /// `astrs_log::LogRecord` at all — a real problem, since every payload
    /// on this virtual source is meant to be one, marker or not.
    pub decode_problems: Vec<String>,
}

impl LogTally {
    /// An empty tally.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers one already-decoded record. Returns whether it was one of
    /// this example's own marker records (and was therefore tallied) —
    /// `false` for the daemon's own records, another node's captured
    /// output, or anything else that is not a marker line.
    pub fn accept(&mut self, node: Option<&str>, level_label: &str, message: &str) -> bool {
        let Some(seq) = seq_of_marker(message) else {
            return false;
        };
        let node = node.unwrap_or("<unknown>").to_owned();
        let _new = self.seen.entry(node).or_default().insert(seq);
        *self.level_counts.entry(level_label.to_owned()).or_insert(0) += 1;
        self.total_matched += 1;
        true
    }

    /// Records an undecodable payload, at most a handful of times.
    pub fn decode_problem(&mut self, problem: impl Into<String>) {
        if self.decode_problems.len() < 8 {
            self.decode_problems.push(problem.into());
        }
    }

    /// Every `(worker, seq)` pair that never arrived, checked against
    /// `worker_ids` each emitting `0..records_per_worker`.
    #[must_use]
    pub fn missing(&self, worker_ids: &[&str], records_per_worker: u64) -> Vec<(String, u64)> {
        let mut missing = Vec::new();
        for &worker in worker_ids {
            let seen = self.seen.get(worker);
            for seq in 0..records_per_worker {
                if !seen.is_some_and(|seqs| seqs.contains(&seq)) {
                    missing.push((worker.to_owned(), seq));
                }
            }
        }
        missing
    }

    /// Whether every worker's every expected marker arrived and no payload
    /// failed to decode.
    #[must_use]
    pub fn is_complete(&self, worker_ids: &[&str], records_per_worker: u64) -> bool {
        self.decode_problems.is_empty() && self.missing(worker_ids, records_per_worker).is_empty()
    }

    /// Renders the tally as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field
    /// types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A marker round-trips through encode/decode, and an unrelated
    /// message is recognised as not being one at all.
    #[test]
    fn markers_round_trip_and_unrelated_messages_are_not_markers() {
        for seq in [0_u64, 1, 42, 999] {
            let message = marker_message(seq);
            assert_eq!(seq_of_marker(&message), Some(seq));
        }
        assert_eq!(seq_of_marker("daemon ready"), None);
        assert_eq!(
            seq_of_marker("log-aggregation marker seq=not-a-number"),
            None
        );
    }

    /// The level cycle wraps and covers every one of its own entries over a
    /// run at least as long as itself.
    #[test]
    fn the_level_cycle_wraps_and_covers_every_level() {
        let seen: BTreeSet<&str> = (0..LEVEL_CYCLE.len() as u64 * 2)
            .map(level_label_for)
            .collect();
        assert_eq!(seen.len(), LEVEL_CYCLE.len());
        assert_eq!(
            level_label_for(0),
            level_label_for(LEVEL_CYCLE.len() as u64)
        );
    }

    /// A tally offered every expected marker from both workers is complete.
    #[test]
    fn a_tally_of_every_expected_marker_is_complete() {
        let mut tally = LogTally::new();
        for &worker in WORKER_IDS {
            for seq in 0..DEFAULT_RECORDS_PER_WORKER {
                let accepted =
                    tally.accept(Some(worker), level_label_for(seq), &marker_message(seq));
                assert!(accepted);
            }
        }
        assert!(
            tally.is_complete(WORKER_IDS, DEFAULT_RECORDS_PER_WORKER),
            "{tally:?}"
        );
        assert_eq!(
            tally.total_matched,
            WORKER_IDS.len() as u64 * DEFAULT_RECORDS_PER_WORKER
        );
    }

    /// A non-marker record (the daemon's own log, another node's captured
    /// stdout) is not tallied, and does not count against completeness.
    #[test]
    fn non_marker_records_are_ignored_not_miscounted() {
        let mut tally = LogTally::new();
        assert!(!tally.accept(Some("daemon"), "info", "daemon ready"));
        assert_eq!(tally.total_matched, 0);
        assert!(tally.seen.is_empty());
    }

    /// A missing marker is named precisely: which worker, which sequence
    /// number.
    #[test]
    fn a_missing_marker_is_named() {
        let mut tally = LogTally::new();
        for seq in 0..3u64 {
            if seq == 1 {
                continue; // sensor-a's seq 1 never arrives
            }
            tally.accept(Some("sensor-a"), level_label_for(seq), &marker_message(seq));
        }
        let missing = tally.missing(&["sensor-a"], 3);
        assert_eq!(missing, vec![("sensor-a".to_owned(), 1)]);
        assert!(!tally.is_complete(&["sensor-a"], 3));
    }

    /// A worker that never sent anything at all is reported as every one of
    /// its markers missing, not silently skipped.
    #[test]
    fn a_silent_worker_is_entirely_missing() {
        let tally = LogTally::new();
        let missing = tally.missing(&["sensor-a"], 2);
        assert_eq!(
            missing,
            vec![("sensor-a".to_owned(), 0), ("sensor-a".to_owned(), 1)]
        );
    }

    /// A decode problem makes the tally incomplete even when every marker
    /// otherwise arrived.
    #[test]
    fn a_decode_problem_blocks_completeness() {
        let mut tally = LogTally::new();
        for seq in 0..2u64 {
            tally.accept(Some("sensor-a"), level_label_for(seq), &marker_message(seq));
        }
        tally.decode_problem("truncated JSON payload");
        assert!(!tally.is_complete(&["sensor-a"], 2));
    }

    /// The decode-problem list is capped, the same way every other report
    /// in this estate caps its problem list.
    #[test]
    fn the_decode_problem_list_is_capped() {
        let mut tally = LogTally::new();
        for index in 0..20 {
            tally.decode_problem(format!("problem {index}"));
        }
        assert_eq!(tally.decode_problems.len(), 8);
    }

    /// The tally round-trips as JSON.
    #[test]
    fn a_tally_round_trips_as_json() {
        let mut tally = LogTally::new();
        tally.accept(Some("sensor-a"), "info", &marker_message(0));
        let json = tally.to_json().unwrap();
        let parsed: LogTally = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tally);
    }

    /// The report path defaults under the temporary directory.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, names this dataflow, wires
    /// both workers' env to a worker id in [`WORKER_IDS`], and wires the
    /// aggregator's sole input to the unfiltered `astrs/logs` virtual
    /// source.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("log-aggregation"));
        assert_eq!(manifest.nodes.len(), 3);

        let aggregator = manifest
            .nodes
            .iter()
            .find(|node| node.id == "aggregator")
            .expect("an aggregator node");
        let logs_input = aggregator.inputs.get(LOGS_PORT).expect("a logs input");
        assert_eq!(logs_input.source, "astrs/logs");
    }
}
