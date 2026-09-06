//! Shared types for `typed-vs-any` — a typed columnar port and a raw-bytes
//! port carrying the same values, side by side (blueprint §9.1, §9.2).
//!
//! ```text
//!   [source] ──typed (std/core/v1/Float64)──► [sink]
//!      │
//!      └──────raw (no type URN)─────────────► [sink]
//! ```
//!
//! `source` publishes the identical deterministic sequence twice on the same
//! tick: once through `node.output::<Scalar<f64>>()` (§9.1's typed handle,
//! encoding a one-row Arrow `Float64` column, its port's declared URN
//! checked against `T` at construction — §9.2), and once through
//! `RawOutput::send_bytes` as eight hand-encoded little-endian bytes, on a
//! port that declares no type URN at all. `sink` decodes both and compares
//! them: the claim under test is that the two encodings of the *same value*
//! survive the wire identically, which is what "typed by default, raw when
//! you need it" (§3.7) is worth only if both paths actually agree.
//!
//! # Why the raw port is not just a worse typed port
//!
//! A typed port is checked once, at construction, against the manifest's
//! declared URN (§9.2) — after that, the wire format is `astrs-data`'s
//! problem, not the node author's. A raw port has neither: the encoding is
//! whatever the node writes, and a change to it is invisible to
//! `astrs validate`. This example's whole point is showing that both are
//! legitimate — `typed` is what §3.7 recommends by default, `raw` is what a
//! node reaches for when it is moving bytes it does not need to interpret
//! (`record-replay`'s `frames` edge is exactly that case) — never that one
//! subsumes the other.

use astrs_node_api::message::Scalar;
use serde::{Deserialize, Serialize};

/// The source's typed output port, and the sink's typed input.
pub const TYPED_PORT: &str = "typed";
/// The source's raw output port, and the sink's raw input.
pub const RAW_PORT: &str = "raw";
/// The source's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable overriding how many values the source publishes.
pub const ENV_SAMPLES: &str = "TYPED_VS_ANY_SAMPLES";
/// Environment variable naming the JSON file the sink writes its
/// [`ComparisonReport`] to.
pub const ENV_REPORT_PATH: &str = "TYPED_VS_ANY_REPORT";

/// How many values the source publishes by default.
pub const DEFAULT_SAMPLES: u64 = 200;

/// How many bytes a raw payload carries: one little-endian `f64`.
pub const RAW_PAYLOAD_BYTES: usize = 8;

/// How many values this run should publish/expect.
#[must_use]
pub fn sample_budget() -> u64 {
    std::env::var(ENV_SAMPLES)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|budget| *budget > 0)
        .unwrap_or(DEFAULT_SAMPLES)
}

/// Where the sink writes its report when the manifest names no path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-typed-vs-any-report.json")
}

/// The JSON file this run's sink writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// The value the source publishes for sample `index`.
///
/// A pure function of the index, with a fractional part (so a byte-level or
/// rounding bug in either encoding path shows up rather than surviving by
/// coincidence) and no two indices ever mapping to the same value.
#[must_use]
pub fn value_for(index: u64) -> f64 {
    (index as f64).mul_add(1.5, 0.25)
}

/// Encodes a value as the `raw` port's wire payload: eight little-endian
/// bytes, nothing else.
#[must_use]
pub fn raw_payload(value: f64) -> [u8; RAW_PAYLOAD_BYTES] {
    value.to_le_bytes()
}

/// Decodes a value out of a `raw` port payload.
///
/// [`None`] if `payload` is not exactly [`RAW_PAYLOAD_BYTES`] long.
#[must_use]
pub fn raw_value_of(payload: &[u8]) -> Option<f64> {
    let bytes: [u8; RAW_PAYLOAD_BYTES] = payload.try_into().ok()?;
    Some(f64::from_le_bytes(bytes))
}

/// The typed message the `typed` port carries: `std/core/v1/Float64`.
pub type TypedValue = Scalar<f64>;

/// One port's accumulated arrivals, in order, plus every index whose value
/// did not match [`value_for`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PortTally {
    /// Every value received, in arrival order.
    pub values: Vec<f64>,
    /// `index` of every arrival whose value was not `value_for(index)`.
    pub unexpected: Vec<u64>,
    /// Whether this port's input closed (rather than the run being cut
    /// short some other way).
    pub closed: bool,
}

impl PortTally {
    /// Records one arrival, checking it against [`value_for`] at its own
    /// position in the stream.
    pub fn push(&mut self, value: f64) {
        let index = self.values.len() as u64;
        if value != value_for(index) {
            self.unexpected.push(index);
        }
        self.values.push(value);
    }
}

/// The sink's verdict on one run: what each port delivered, and whether the
/// two agree.
///
/// Written as JSON to [`report_path`] once both inputs close — what a human
/// (or future conformance coverage) reads to check a run without scraping
/// stdout.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    /// The typed port's tally.
    pub typed: PortTally,
    /// The raw port's tally.
    pub raw: PortTally,
    /// Every index at which the two ports' values disagreed (both arrived,
    /// but were not equal).
    pub disagreements: Vec<u64>,
}

impl ComparisonReport {
    /// Builds the report from two completed tallies, recording every index
    /// at which they disagree.
    #[must_use]
    pub fn compare(typed: PortTally, raw: PortTally) -> Self {
        let disagreements = typed
            .values
            .iter()
            .zip(raw.values.iter())
            .enumerate()
            .filter_map(|(index, (typed_value, raw_value))| {
                (typed_value != raw_value).then_some(index as u64)
            })
            .collect();
        Self {
            typed,
            raw,
            disagreements,
        }
    }

    /// Whether both ports delivered the same non-empty sequence, with no
    /// port-internal mismatches and no cross-port disagreement.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.typed.values.is_empty()
            && self.typed.closed
            && self.raw.closed
            && self.typed.unexpected.is_empty()
            && self.raw.unexpected.is_empty()
            && self.disagreements.is_empty()
            && self.typed.values.len() == self.raw.values.len()
    }

    /// Renders the report as pretty JSON.
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

    /// `value_for` is deterministic and never repeats within a reasonable
    /// range.
    #[test]
    fn value_for_is_deterministic_and_distinct() {
        for index in 0..64u64 {
            assert_eq!(value_for(index), value_for(index));
        }
        assert_ne!(value_for(3), value_for(4));
        // Carries a fractional part, so a bug that truncated to an integer
        // on either encoding path would be visible.
        assert_eq!(value_for(0), 0.25);
        assert_eq!(value_for(1), 1.75);
    }

    /// A raw payload round-trips through encode/decode, and a payload of
    /// the wrong length is reported as absent rather than guessed at.
    #[test]
    fn raw_payloads_round_trip_and_reject_the_wrong_length() {
        for index in 0..16u64 {
            let value = value_for(index);
            let payload = raw_payload(value);
            assert_eq!(payload.len(), RAW_PAYLOAD_BYTES);
            assert_eq!(raw_value_of(&payload), Some(value));
        }
        assert_eq!(raw_value_of(&[1, 2, 3]), None);
        assert_eq!(raw_value_of(&[0; 9]), None);
    }

    /// A tally fed the expected sequence records no unexpected arrivals.
    #[test]
    fn a_tally_of_the_expected_sequence_has_no_unexpected_entries() {
        let mut tally = PortTally::default();
        for index in 0..10u64 {
            tally.push(value_for(index));
        }
        assert_eq!(tally.values.len(), 10);
        assert!(tally.unexpected.is_empty());
    }

    /// A tally fed a value out of sequence records exactly which index was
    /// wrong.
    #[test]
    fn a_tally_reports_which_index_was_wrong() {
        let mut tally = PortTally::default();
        tally.push(value_for(0));
        tally.push(999.0); // should have been value_for(1)
        tally.push(value_for(2));
        assert_eq!(tally.unexpected, vec![1]);
    }

    /// Two tallies fed the identical sequence compare as clean.
    #[test]
    fn matching_tallies_compare_as_clean() {
        let mut typed = PortTally::default();
        let mut raw = PortTally::default();
        for index in 0..50u64 {
            typed.push(value_for(index));
            raw.push(value_for(index));
        }
        typed.closed = true;
        raw.closed = true;
        let report = ComparisonReport::compare(typed, raw);
        assert!(report.is_clean(), "{report:?}");
        assert!(report.disagreements.is_empty());
    }

    /// A single disagreement between the two ports is caught, and the
    /// report is not clean.
    #[test]
    fn a_cross_port_disagreement_is_caught() {
        let mut typed = PortTally::default();
        let mut raw = PortTally::default();
        for index in 0..5u64 {
            typed.push(value_for(index));
            raw.push(value_for(index));
        }
        raw.values[2] = 12345.0; // corrupt one raw-side value directly
        typed.closed = true;
        raw.closed = true;
        let report = ComparisonReport::compare(typed, raw);
        assert_eq!(report.disagreements, vec![2]);
        assert!(!report.is_clean());
    }

    /// A run that never closed both ports is not clean, even with matching
    /// values so far — a partial run is not a proof of equivalence.
    #[test]
    fn an_unclosed_run_is_never_clean() {
        let mut typed = PortTally::default();
        let mut raw = PortTally::default();
        typed.push(value_for(0));
        raw.push(value_for(0));
        let report = ComparisonReport::compare(typed, raw);
        assert!(!report.is_clean());
    }

    /// An empty run (nothing ever arrived) is not clean either.
    #[test]
    fn an_empty_run_is_not_clean() {
        let report = ComparisonReport::compare(PortTally::default(), PortTally::default());
        assert!(!report.is_clean());
    }

    /// The report round-trips as JSON.
    #[test]
    fn a_report_round_trips_as_json() {
        let mut typed = PortTally::default();
        typed.push(value_for(0));
        typed.closed = true;
        let mut raw = PortTally::default();
        raw.push(value_for(0));
        raw.closed = true;
        let report = ComparisonReport::compare(typed, raw);
        let json = report.to_json().unwrap();
        let parsed: ComparisonReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    /// Both artefact paths default under the temporary directory.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, and names this dataflow —
    /// and declares a type URN on `typed` but not on `raw`, which is the
    /// whole point of this example.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("typed-vs-any"));
        let source = manifest
            .nodes
            .iter()
            .find(|node| node.id == "source")
            .expect("a source node");
        assert!(source.output_types.contains_key(TYPED_PORT));
        assert!(!source.output_types.contains_key(RAW_PORT));
    }
}
