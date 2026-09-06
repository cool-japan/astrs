//! Shared types for `benchmark-latency` — a closed-loop round-trip latency
//! measurement across the local (same-daemon) plane (blueprint §20.4).
//!
//! ```text
//!   [prober] ──ping──► [reflector]
//!      ▲                    │
//!      └───────pong─────────┘
//! ```
//!
//! `prober` holds exactly one round trip in flight: it sends `ping`, waits
//! for the matching `pong`, times the gap with its own [`std::time::Instant`]
//! clock, and only then sends the next `ping`. That closed-loop discipline is
//! what makes the measurement a round-trip latency rather than a queueing
//! delay — an open-loop sender (fixed-rate regardless of whether the last
//! reply came back) would let a slow reflector's backlog leak into every
//! sample after the first.
//!
//! # Why the clock never crosses a process boundary
//!
//! [`std::time::Instant`] is deliberately *not* serialized onto the wire and
//! compared against a reading taken in a different process: the standard
//! library gives no portable guarantee that two processes' monotonic clocks
//! share an epoch, so a `reflector`-side timestamp subtracted from a
//! `prober`-side one would measure clock skew as often as it measured
//! latency. `prober` instead times itself: one `Instant::now()` right before
//! `ping` is sent, one right after the matching `pong` arrives, both read by
//! the same clock in the same process. The 8-byte sequence number in the
//! payload is what lets it *recognise* the matching `pong` — not what times
//! it.
//!
//! # The percentile definition, pinned
//!
//! [`percentile_us`] is the classic **nearest-rank** percentile: the
//! `ceil(p * n)`-th smallest sample (1-indexed), no interpolation between
//! neighbours. `p` is expressed as **permille** (parts per 1000: `500` for
//! p50, `990` for p99) specifically so the rank computation is exact integer
//! arithmetic — `f64` multiplication (`0.99 * n`) can land a hair either side
//! of an integer boundary depending on `n`, which would make `ceil` pick a
//! different rank than the one a reader expects for the same nominal
//! percentile on different sample counts. Integer permille has no such edge:
//! the same `p99` always means the same rank for the same `n`.
//!
//! # Loose sanity bounds, on purpose
//!
//! [`LatencyReport::sanity_bounds_ok`] checks a ceiling three orders of
//! magnitude above blueprint §20.4's aspirational same-host RTT target (25
//! µs) rather than that target itself. This example's own `-p`-scoped test
//! runs under parallel `cargo nextest` on whatever machine happens to be
//! building the workspace at the time — CI runners, a laptop under load, a
//! container with a noisy neighbour — and a bound tight enough to catch a
//! real performance regression is also tight enough to fail on a slow CI
//! runner for reasons that have nothing to do with AstRS. `astrs-bench`'s
//! criterion suite (§20.4) is where the real target is gated; this example
//! only proves the measurement pipeline (timing, correlation, percentiles)
//! is wired correctly, with a bound loose enough that only a wire that is
//! actually broken — a stuck reflector, a hung queue — would trip it.

use serde::{Deserialize, Serialize};

/// The prober's output port, and the reflector's input.
pub const PING_PORT: &str = "ping";
/// The reflector's output port, and the prober's input.
pub const PONG_PORT: &str = "pong";

/// Environment variable overriding how many round trips the prober times.
pub const ENV_SAMPLES: &str = "BENCHMARK_LATENCY_SAMPLES";
/// Environment variable naming the JSON file the prober writes its
/// [`LatencyReport`] to.
pub const ENV_REPORT_PATH: &str = "BENCHMARK_LATENCY_REPORT";

/// How many round trips the prober times by default.
pub const DEFAULT_SAMPLES: u64 = 300;

/// How many bytes a ping/pong payload carries: one big-endian sequence
/// number, nothing else — the claim under test is about the wire, not the
/// encoding (record-replay's `frames` edge makes the same choice, for the
/// same reason).
pub const PAYLOAD_BYTES: usize = 8;

/// A generous, deliberately loose ceiling on p99, in microseconds — see the
/// crate docs' "Loose sanity bounds" section for why 250 ms and not §20.4's
/// 25 µs target.
pub const SANITY_P99_CEILING_US: u64 = 250_000;

/// How many round trips this run should time.
#[must_use]
pub fn sample_budget() -> u64 {
    std::env::var(ENV_SAMPLES)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|budget| *budget > 0)
        .unwrap_or(DEFAULT_SAMPLES)
}

/// Where the prober writes its [`LatencyReport`] when the manifest names no
/// path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-benchmark-latency-report.json")
}

/// The JSON file this run's prober writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// Encodes a ping/pong sequence number as its wire payload: eight
/// big-endian bytes, nothing else.
#[must_use]
pub fn seq_payload(seq: u64) -> [u8; PAYLOAD_BYTES] {
    seq.to_be_bytes()
}

/// Decodes a sequence number out of a ping/pong payload.
///
/// [`None`] if `payload` is not exactly [`PAYLOAD_BYTES`] long, which a
/// prober treats as a stray message rather than assuming it is frame zero.
#[must_use]
pub fn seq_of(payload: &[u8]) -> Option<u64> {
    let bytes: [u8; PAYLOAD_BYTES] = payload.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// The nearest-rank percentile of an **already sorted, ascending** sample
/// set, in whatever unit the caller filled it with (this crate always uses
/// microseconds).
///
/// `permille` is the percentile expressed as parts per 1000 (`500` = p50,
/// `990` = p99); values above `1000` saturate to `1000` (the maximum).
/// [`None`] only for an empty `sorted`.
///
/// See the crate docs' "The percentile definition, pinned" section for why
/// this takes integer permille rather than an `f64` fraction.
#[must_use]
pub fn percentile_us(sorted: &[u64], permille: u32) -> Option<u64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let permille = u64::from(permille.min(1000));
    // Ceiling division of `n * permille / 1000`, entirely in integers: the
    // 1-indexed rank of the nearest-rank percentile. `rank` is always in
    // `1..=n` (see this function's tests), so the cast back to `usize`
    // never truncates a value the subtraction/`min` below would notice.
    let rank = (n as u64 * permille).div_ceil(1000);
    let index = (rank as usize).saturating_sub(1).min(n - 1);
    Some(sorted[index])
}

/// A closed-loop latency run's summary: what the prober measured, and the
/// percentiles derived from it.
///
/// Written as JSON to [`report_path`] when the prober's budget is spent —
/// what a human, or `tests/conformance`'s eventual estate coverage, reads to
/// check the run without scraping the terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyReport {
    /// How many round trips completed and were timed.
    pub samples: u64,
    /// How many round trips did not complete cleanly — a `pong` whose
    /// decoded sequence number did not match the `ping` currently in
    /// flight. Counted, not fatal: the round trip that produced it is
    /// simply not added to the timed sample set.
    pub mismatched: u64,
    /// The fastest round trip, in microseconds.
    pub min_us: u64,
    /// The 50th percentile, in microseconds.
    pub p50_us: u64,
    /// The 90th percentile, in microseconds.
    pub p90_us: u64,
    /// The 99th percentile, in microseconds.
    pub p99_us: u64,
    /// The slowest round trip, in microseconds.
    pub max_us: u64,
    /// The arithmetic mean, in microseconds.
    pub mean_us: f64,
}

impl LatencyReport {
    /// Builds a report from raw round-trip samples, in microseconds, in the
    /// order they were measured (this function sorts its own working copy).
    ///
    /// A `samples` of `[]` reports every field as `0`, not an error: a
    /// prober whose reflector never answered still has to write *a* report,
    /// and a reader checking `samples == 0` learns exactly what happened.
    #[must_use]
    pub fn from_samples_us(samples: &[u64], mismatched: u64) -> Self {
        if samples.is_empty() {
            return Self {
                samples: 0,
                mismatched,
                min_us: 0,
                p50_us: 0,
                p90_us: 0,
                p99_us: 0,
                max_us: 0,
                mean_us: 0.0,
            };
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let sum: u128 = sorted.iter().copied().map(u128::from).sum();
        #[allow(clippy::cast_precision_loss)] // a report is a summary, not an exact ledger
        let mean_us = sum as f64 / sorted.len() as f64;
        Self {
            samples: sorted.len() as u64,
            mismatched,
            // `sorted` is non-empty here, so every lookup below is `Some`.
            min_us: percentile_us(&sorted, 0).unwrap_or(0),
            p50_us: percentile_us(&sorted, 500).unwrap_or(0),
            p90_us: percentile_us(&sorted, 900).unwrap_or(0),
            p99_us: percentile_us(&sorted, 990).unwrap_or(0),
            max_us: percentile_us(&sorted, 1000).unwrap_or(0),
            mean_us,
        }
    }

    /// Whether this report's own numbers are internally consistent —
    /// `min <= p50 <= p90 <= p99 <= max`, `min <= mean <= max` — and stay
    /// under the deliberately loose ceiling documented on
    /// [`SANITY_P99_CEILING_US`].
    ///
    /// A structural check, not a performance gate: see the crate docs'
    /// "Loose sanity bounds" section for why this example does not assert
    /// blueprint §20.4's real target.
    #[must_use]
    pub fn sanity_bounds_ok(&self) -> bool {
        self.samples > 0
            && self.min_us <= self.p50_us
            && self.p50_us <= self.p90_us
            && self.p90_us <= self.p99_us
            && self.p99_us <= self.max_us
            && (self.mean_us >= self.min_us as f64)
            && (self.mean_us <= self.max_us as f64)
            && self.p99_us <= SANITY_P99_CEILING_US
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

    /// A round-trip sequence number survives encode/decode, and a
    /// wrong-length payload is reported as absent rather than guessed at.
    #[test]
    fn seq_payloads_round_trip_and_reject_the_wrong_length() {
        for seq in [0_u64, 1, 42, u64::MAX] {
            let payload = seq_payload(seq);
            assert_eq!(payload.len(), PAYLOAD_BYTES);
            assert_eq!(seq_of(&payload), Some(seq));
        }
        assert_eq!(seq_of(&[1, 2, 3]), None);
        assert_eq!(seq_of(&[0; 9]), None);
    }

    /// The percentile of an empty set is `None`, not a panic or a default
    /// value that looks like real data.
    #[test]
    fn percentile_of_an_empty_set_is_none() {
        assert_eq!(percentile_us(&[], 500), None);
    }

    /// p0 is always the minimum and p100 (`1000` permille) is always the
    /// maximum, for any non-empty input — the two ends nearest-rank must
    /// agree on regardless of `n`.
    #[test]
    fn p0_is_the_minimum_and_p1000_is_the_maximum() {
        let sorted = [5_u64, 9, 12, 40, 41, 87];
        assert_eq!(percentile_us(&sorted, 0), Some(5));
        assert_eq!(percentile_us(&sorted, 1000), Some(87));
        // A permille above 1000 saturates rather than reading out of bounds.
        assert_eq!(percentile_us(&sorted, 5000), Some(87));
        // A single-element set: every percentile is that one element.
        assert_eq!(percentile_us(&[7], 500), Some(7));
        assert_eq!(percentile_us(&[7], 990), Some(7));
    }

    /// The nearest-rank formula against a hand-worked vector: `1..=100`
    /// (`sorted[i] == i + 1`), where `n * permille` divides evenly by 1000
    /// so the expected rank has no rounding ambiguity to get wrong.
    #[test]
    fn percentile_matches_a_hand_worked_hundred_element_vector() {
        let sorted: Vec<u64> = (1..=100).collect();
        // rank = ceil(100 * 500 / 1000) = 50 -> sorted[49] == 50.
        assert_eq!(percentile_us(&sorted, 500), Some(50));
        // rank = ceil(100 * 900 / 1000) = 90 -> sorted[89] == 90.
        assert_eq!(percentile_us(&sorted, 900), Some(90));
        // rank = ceil(100 * 990 / 1000) = 99 -> sorted[98] == 99.
        assert_eq!(percentile_us(&sorted, 990), Some(99));
    }

    /// A vector too small to divide evenly exercises the `ceil`, not just
    /// the exact-division case above: `n = 4`, where p50 and p90 both round
    /// *up* to a rank that is not `n * p` exactly.
    #[test]
    fn percentile_rounds_up_on_a_vector_that_does_not_divide_evenly() {
        let sorted = [10_u64, 20, 30, 40];
        // rank = ceil(4 * 500 / 1000) = ceil(2.0) = 2 -> sorted[1] == 20.
        assert_eq!(percentile_us(&sorted, 500), Some(20));
        // rank = ceil(4 * 900 / 1000) = ceil(3.6) = 4 -> sorted[3] == 40.
        assert_eq!(percentile_us(&sorted, 900), Some(40));
    }

    /// [`percentile_us`] is fed pre-sorted input by
    /// [`LatencyReport::from_samples_us`]; feeding it out of order is the
    /// caller's bug, not this function's — pin that [`LatencyReport`] does
    /// the sorting so a future caller cannot skip it by accident.
    #[test]
    fn from_samples_sorts_its_own_working_copy() {
        let report = LatencyReport::from_samples_us(&[300, 100, 200, 500, 400], 0);
        assert_eq!(report.min_us, 100);
        assert_eq!(report.max_us, 500);
        assert_eq!(report.samples, 5);
    }

    /// An empty sample set reports zeroed fields rather than panicking —
    /// the shape a prober whose reflector never answered still has to be
    /// able to write.
    #[test]
    fn an_empty_sample_set_reports_zeros_not_a_panic() {
        let report = LatencyReport::from_samples_us(&[], 3);
        assert_eq!(report.samples, 0);
        assert_eq!(report.mismatched, 3);
        assert_eq!(report.min_us, 0);
        assert_eq!(report.max_us, 0);
        assert_eq!(report.mean_us, 0.0);
        // Zero samples is itself insane: nothing was ever measured.
        assert!(!report.sanity_bounds_ok());
    }

    /// The mean is computed correctly and independently of the percentile
    /// machinery (a plain arithmetic mean, not derived from a percentile).
    #[test]
    fn the_mean_is_the_plain_arithmetic_average() {
        let report = LatencyReport::from_samples_us(&[10, 20, 30, 40], 0);
        assert_eq!(report.mean_us, 25.0);
    }

    /// A run built from a realistic, spread-out sample set is internally
    /// consistent and passes the loose sanity ceiling.
    #[test]
    fn a_realistic_run_is_sane() {
        // 300 samples spread from 20 us to 2000 us — comfortably inside
        // SANITY_P99_CEILING_US, and varied enough that p50 < p90 < p99
        // must hold for real, not by coincidence of a constant vector.
        let samples: Vec<u64> = (0..300).map(|i| 20 + i * 7).collect();
        let report = LatencyReport::from_samples_us(&samples, 0);
        assert!(report.sanity_bounds_ok(), "{report:?}");
        assert!(report.min_us <= report.p50_us);
        assert!(report.p50_us <= report.p90_us);
        assert!(report.p90_us <= report.p99_us);
        assert!(report.p99_us <= report.max_us);
    }

    /// A report whose percentiles were tampered with (out of the order a
    /// real measurement could ever produce) fails the sanity check —
    /// proving the check actually discriminates rather than always passing.
    #[test]
    fn an_internally_inconsistent_report_fails_sanity() {
        let mut report = LatencyReport::from_samples_us(&[10, 20, 30, 40, 50], 0);
        report.p50_us = report.p99_us + 1;
        assert!(!report.sanity_bounds_ok(), "{report:?}");
    }

    /// A p99 above the deliberately loose ceiling fails sanity even when
    /// every other ordering invariant holds — the ceiling is load-bearing,
    /// not decorative.
    #[test]
    fn a_p99_over_the_ceiling_fails_sanity() {
        let mut report = LatencyReport::from_samples_us(&[1, 2, 3], 0);
        report.p99_us = SANITY_P99_CEILING_US + 1;
        report.max_us = report.p99_us;
        assert!(!report.sanity_bounds_ok());
    }

    /// The report round-trips as JSON, which is how a reader (and any
    /// future conformance coverage) inspects a run without scraping stdout.
    #[test]
    fn a_report_round_trips_as_json() {
        let report = LatencyReport::from_samples_us(&[15, 25, 35], 1);
        let json = report.to_json().unwrap();
        let parsed: LatencyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    /// The sample budget honours the environment override and falls back
    /// to the default for anything unusable (unset, empty, zero, or not a
    /// number).
    #[test]
    fn the_sample_budget_is_a_positive_default_or_env_override() {
        // We cannot mutate the real process environment safely in a
        // parallel test binary, so this exercises the parsing helper the
        // same way `sample_budget` itself does.
        let parse = |raw: &str| -> u64 {
            raw.trim()
                .parse()
                .ok()
                .filter(|budget: &u64| *budget > 0)
                .unwrap_or(DEFAULT_SAMPLES)
        };
        assert_eq!(parse("500"), 500);
        assert_eq!(parse("0"), DEFAULT_SAMPLES);
        assert_eq!(parse("not-a-number"), DEFAULT_SAMPLES);
        assert_eq!(parse(""), DEFAULT_SAMPLES);
    }

    /// The report path defaults under the temporary directory, so a run
    /// never writes into the checkout.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, and names this dataflow —
    /// checked directly against `astrs-manifest` rather than only by
    /// eyeball, so a typo in `dataflow.yml` fails `cargo test -p
    /// benchmark-latency` instead of surfacing only when someone runs it.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("benchmark-latency"));
        assert_eq!(manifest.nodes.len(), 2);
    }
}
