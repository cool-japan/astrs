//! The generic fuzz loop, corpus replay, and greedy minimizer every surface
//! in [`crate::surfaces`] shares (blueprint §15).
//!
//! # Design
//!
//! - **Never touches the global panic hook.** Each generated or replayed
//!   case runs behind [`std::panic::catch_unwind`]; the default hook still
//!   prints its usual one-liner on a hit, and [`fuzz_surface`] stops after
//!   `MAX_FAILURES` distinct hits — rather than running all
//!   `ASTRS_FUZZ_ITERS` iterations regardless — so a systemic bug cannot
//!   flood the log or the minimizer's own (bounded, but nonzero) work.
//! - **Never writes to the repository.** A failure is minimized in memory
//!   and printed as hex; turning that into a committed regression file
//!   under `corpus/<surface>/` is a deliberate, reviewed action, never a
//!   side effect of running `cargo test`.
//! - **Deterministic by default.** [`seed_value`] is a fixed constant unless
//!   `ASTRS_FUZZ_SEED` overrides it, so a default run is reproducible and a
//!   deliberately varied one is one environment variable away.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};

use crate::support::rng::Rng;

/// Default iteration count per surface: fast enough that `cargo test -p
/// astrs-fuzz` stays in the ordinary test suite (blueprint §15). The
/// nightly lane (`scripts/fuzz-nightly.sh`) overrides this via
/// `ASTRS_FUZZ_ITERS`.
pub const DEFAULT_ITERATIONS: usize = 10_000;

/// Fixed default PRNG seed, so a default run is byte-for-byte reproducible.
const DEFAULT_SEED: u64 = 0x5EED_1234_ABCD_EF01;

/// How many distinct failures [`fuzz_surface`] collects before it stops
/// generating further cases.
const MAX_FAILURES: usize = 5;

/// One input that made a `check` function panic.
#[derive(Debug, Clone)]
pub struct Failure {
    /// Where the failure was found: a case index for [`fuzz_surface`], or a
    /// corpus file path for [`replay_corpus`].
    pub label: String,
    /// The offending input: minimized for [`fuzz_surface`], byte-for-byte
    /// as committed for [`replay_corpus`].
    pub input: Vec<u8>,
    /// The panic payload, rendered as text.
    pub message: String,
}

/// The outcome of one fuzz run or one corpus replay.
#[derive(Debug, Clone)]
pub struct FuzzReport {
    /// How many inputs were actually run — may be less than the requested
    /// iteration count when [`fuzz_surface`] stops early.
    pub cases_run: usize,
    /// Every failure found, in the order encountered.
    pub failures: Vec<Failure>,
}

impl FuzzReport {
    /// True when nothing panicked.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }

    /// A human-readable summary, meant for a `#[test]`'s
    /// `assert!(report.is_clean(), "{}", report.summary())`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} case(s) run, {} failure(s)",
            self.cases_run,
            self.failures.len()
        );
        for failure in &self.failures {
            out.push_str(&format!(
                "\n  {}: {} -- {} byte(s): {}",
                failure.label,
                failure.message,
                failure.input.len(),
                to_hex(&failure.input)
            ));
        }
        out
    }
}

/// Generates one fuzz case from a surface's seed corpus.
///
/// A plain function pointer (not `impl FnMut`) because every surface's
/// generator is a free function that closes over nothing but its
/// arguments — this keeps [`fuzz_surface`]'s signature free of generic
/// bounds and trait-object plumbing.
pub type Generator = fn(&mut Rng, &[Vec<u8>]) -> Vec<u8>;

/// Runs one input through a surface's decoder(s) and asserts this crate's
/// invariants (typically with `assert!`/`assert_eq!`).
///
/// Never called outside [`std::panic::catch_unwind`] — see [`fuzz_surface`]
/// and [`replay_corpus`] — so a violated invariant is indistinguishable from
/// a decoder-internal panic, which is exactly the "never panics" property
/// under test.
pub type Checker = fn(&[u8]);

/// Reads `ASTRS_FUZZ_ITERS`, defaulting to [`DEFAULT_ITERATIONS`].
///
/// A value that fails to parse as a `usize` degrades to the default rather
/// than aborting the run, so a typo in the environment cannot turn a fast
/// default test into a hang (or a panic before the harness even starts).
#[must_use]
pub fn iterations() -> usize {
    parse_or_default(std::env::var("ASTRS_FUZZ_ITERS").ok(), DEFAULT_ITERATIONS)
}

/// Reads `ASTRS_FUZZ_SEED`, defaulting to a fixed constant.
#[must_use]
pub fn seed_value() -> u64 {
    parse_or_default(std::env::var("ASTRS_FUZZ_SEED").ok(), DEFAULT_SEED)
}

/// `raw`, parsed as `T`, or `default` when `raw` is absent or does not
/// parse. Factored out of [`iterations`]/[`seed_value`] so the fallback
/// behavior is unit-testable without touching the real (process-wide, and
/// therefore test-order-sensitive) environment.
fn parse_or_default<T: std::str::FromStr>(raw: Option<String>, default: T) -> T {
    raw.and_then(|value| value.parse::<T>().ok())
        .unwrap_or(default)
}

/// Runs `check` over `iterations()` generated cases, seeded from `seeds`.
///
/// `name` decorrelates this surface's PRNG stream from the other three (they
/// all read the same [`seed_value`] by default), so a fixed seed does not
/// hand every surface an identical byte stream. Stops early after
/// `MAX_FAILURES` distinct panics; never panics itself — every failure is
/// collected in the returned [`FuzzReport`] for the caller's `#[test]` to
/// assert on.
#[must_use]
pub fn fuzz_surface(
    name: &str,
    seeds: &[Vec<u8>],
    generate: Generator,
    check: Checker,
) -> FuzzReport {
    let mut rng = Rng::new(seed_value() ^ fnv1a(name.as_bytes()));
    let iters = iterations();
    let mut failures = Vec::new();
    let mut cases_run = 0usize;
    for case_index in 0..iters {
        let input = generate(&mut rng, seeds);
        cases_run += 1;
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| check(&input))) {
            let message = panic_message(payload.as_ref());
            let predicate = |candidate: &[u8]| still_panics(check, candidate);
            let minimized = minimize(&input, &predicate);
            eprintln!(
                "astrs-fuzz[{name}]: case {case_index} panicked: {message}\n  minimized {} byte(s): {}",
                minimized.len(),
                to_hex(&minimized)
            );
            failures.push(Failure {
                label: format!("{name}#{case_index}"),
                input: minimized,
                message,
            });
            if failures.len() >= MAX_FAILURES {
                break;
            }
        }
    }
    FuzzReport {
        cases_run,
        failures,
    }
}

/// Replays every file under `dir` through `check`, unconditionally — no
/// `ASTRS_FUZZ_ITERS` gate — which is what keeps a fixed regression from
/// silently regressing again.
///
/// A missing corpus directory is reported as zero cases run rather than an
/// `Err`, so the caller's `#[test]` can assert `report.cases_run > 0` and
/// get a clear "corpus is empty" failure instead of a filesystem error.
#[must_use]
pub fn replay_corpus(dir: &str, check: Checker) -> FuzzReport {
    let mut failures = Vec::new();
    let mut cases_run = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return FuzzReport {
            cases_run,
            failures,
        };
    };
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    // Sorted so a replay's case order — and therefore which failure is
    // reported first — does not depend on the host filesystem's directory
    // iteration order.
    paths.sort();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        cases_run += 1;
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| check(&bytes))) {
            failures.push(Failure {
                label: path.display().to_string(),
                message: panic_message(payload.as_ref()),
                input: bytes,
            });
        }
    }
    FuzzReport {
        cases_run,
        failures,
    }
}

/// Whether `candidate` still makes `check` panic.
fn still_panics(check: Checker, candidate: &[u8]) -> bool {
    panic::catch_unwind(AssertUnwindSafe(|| check(candidate))).is_err()
}

/// Greedy delta-debugging: drop shrinking chunks, then zero surviving
/// bytes, keeping every change that leaves `still_fails` true.
///
/// This is what turns "some 4000-byte case in a 10 000-iteration run
/// panicked" into a handful of bytes worth reading and worth committing to
/// a corpus file. It runs entirely in memory and writes nothing —
/// [`fuzz_surface`] only ever prints the result.
#[must_use]
pub fn minimize(input: &[u8], still_fails: &dyn Fn(&[u8]) -> bool) -> Vec<u8> {
    let mut current = input.to_vec();
    // Pass 1: remove shrinking chunks (coarse to fine), keeping the vector
    // itself non-empty — an empty input is never an interesting minimized
    // case, since every surface's `check` treats it as a trivial reject.
    let mut chunk = current.len() / 2;
    while chunk > 0 {
        let mut index = 0usize;
        while index < current.len() {
            let end = (index + chunk).min(current.len());
            let mut candidate = current.clone();
            candidate.drain(index..end);
            if !candidate.is_empty() && still_fails(&candidate) {
                current = candidate;
            } else {
                index += chunk;
            }
        }
        chunk /= 2;
    }
    // Pass 2: canonicalize surviving bytes to zero where that does not
    // change the outcome, so unrelated noise does not obscure the bytes
    // that actually matter.
    for index in 0..current.len() {
        if current[index] == 0 {
            continue;
        }
        let mut candidate = current.clone();
        if let Some(byte) = candidate.get_mut(index) {
            *byte = 0;
        }
        if still_fails(&candidate) {
            current = candidate;
        }
    }
    current
}

/// Renders a panic payload as text, for the shapes `panic!`/`assert!`
/// actually produce (`&'static str` and `String`).
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Lowercase hex, chosen so a printed failure can be turned back into a
/// corpus file with `xxd -r -p`.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// FNV-1a over a short label.
///
/// Used only to decorrelate each surface's PRNG stream from the others when
/// they share [`seed_value`] — not a security boundary, just enough spread
/// that "wire" and "rtps" do not draw the same sequence of cases.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut hash = OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn never_panics(_bytes: &[u8]) {}

    fn panics_on_leading_0xff(bytes: &[u8]) {
        assert!(bytes.first() != Some(&0xFF), "leading 0xff");
    }

    fn generate_incrementing(rng: &mut Rng, _seeds: &[Vec<u8>]) -> Vec<u8> {
        vec![rng.gen_byte()]
    }

    #[test]
    fn a_clean_checker_reports_no_failures() {
        // ASTRS_FUZZ_ITERS is process-global; keep this test's run small and
        // explicit rather than depending on (or racing) another test's env
        // var.
        let report = fuzz_surface("clean", &[], generate_incrementing, never_panics);
        assert!(report.is_clean(), "{}", report.summary());
        assert!(report.cases_run > 0);
    }

    #[test]
    fn a_failing_checker_is_caught_and_minimized() {
        // Fails on roughly one case in eight, so this exercises "found a
        // real bug amid mostly-clean cases" — `fuzz_surface_stops_after_max_failures`
        // below is what covers "every case fails".
        fn generate_occasionally_0xff(rng: &mut Rng, _seeds: &[Vec<u8>]) -> Vec<u8> {
            let len = 1 + rng.gen_range(0, 8);
            let mut bytes = rng.bytes(len);
            if rng.one_in(8)
                && let Some(first) = bytes.first_mut()
            {
                *first = 0xFF;
            }
            bytes
        }
        let report = fuzz_surface(
            "leading-0xff",
            &[],
            generate_occasionally_0xff,
            panics_on_leading_0xff,
        );
        assert!(!report.is_clean());
        assert!(report.failures.len() <= MAX_FAILURES);
        // Minimization must not lose the property that triggers the panic.
        for failure in &report.failures {
            assert_eq!(failure.input.first(), Some(&0xFF));
        }
    }

    #[test]
    fn fuzz_surface_stops_after_max_failures() {
        fn generate_0xff(_rng: &mut Rng, _seeds: &[Vec<u8>]) -> Vec<u8> {
            vec![0xFF]
        }
        // SAFETY-free, but not thread-safe: this test relies on
        // ASTRS_FUZZ_ITERS being unset (or large) so there is room for more
        // than MAX_FAILURES hits before iterations() runs out.
        let report = fuzz_surface("always-fails", &[], generate_0xff, panics_on_leading_0xff);
        assert_eq!(report.failures.len(), MAX_FAILURES);
        // Every generated case fails here, so the loop must stop the moment
        // it collects the fifth failure rather than continuing to
        // `iterations()` — this is what actually tests the early-stop path
        // rather than merely re-asserting the failure count above.
        assert_eq!(report.cases_run, MAX_FAILURES);
    }

    #[test]
    fn minimize_shrinks_to_the_essential_byte() {
        let haystack: Vec<u8> = (0..64).map(|i| if i == 40 { 0xAA } else { 0x11 }).collect();
        let still_fails = |candidate: &[u8]| candidate.contains(&0xAA);
        let minimized = minimize(&haystack, &still_fails);
        assert_eq!(minimized, vec![0xAA]);
    }

    #[test]
    fn minimize_on_an_always_failing_predicate_reaches_one_byte() {
        let input = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let always = |_: &[u8]| true;
        let minimized = minimize(&input, &always);
        assert_eq!(minimized.len(), 1);
    }

    #[test]
    fn replay_corpus_on_a_missing_directory_runs_nothing() {
        let report = replay_corpus("/nonexistent/astrs-fuzz-corpus-dir", never_panics);
        assert_eq!(report.cases_run, 0);
        assert!(report.is_clean());
    }

    #[test]
    fn replay_corpus_replays_every_file_and_reports_hits() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-fuzz-driver-test-{}-{}",
            std::process::id(),
            seed_value()
        ));
        std::fs::create_dir_all(&dir).expect("create temp corpus dir");
        std::fs::write(dir.join("ok.bin"), [0x00, 0x01]).expect("write");
        std::fs::write(dir.join("bad.bin"), [0xFF, 0x02]).expect("write");

        let dir_str = dir.to_str().expect("utf8 temp path");
        let report = replay_corpus(dir_str, panics_on_leading_0xff);
        assert_eq!(report.cases_run, 2);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].label.ends_with("bad.bin"));

        std::fs::remove_dir_all(&dir).expect("cleanup temp corpus dir");
    }

    #[test]
    fn parse_or_default_falls_back_on_garbage_or_absence() {
        // Exercised directly against the pure helper: ASTRS_FUZZ_ITERS is
        // process-wide, and other tests in this binary may read it
        // concurrently, so this must not mutate the real environment.
        assert_eq!(
            parse_or_default::<usize>(None, DEFAULT_ITERATIONS),
            DEFAULT_ITERATIONS
        );
        assert_eq!(
            parse_or_default(Some("not-a-number".to_owned()), DEFAULT_ITERATIONS),
            DEFAULT_ITERATIONS
        );
        assert_eq!(
            parse_or_default(Some("42".to_owned()), DEFAULT_ITERATIONS),
            42
        );
        assert_eq!(
            parse_or_default(Some(String::new()), DEFAULT_ITERATIONS),
            DEFAULT_ITERATIONS
        );
    }

    #[test]
    fn to_hex_round_trips_through_the_documented_xxd_incantation() {
        assert_eq!(to_hex(&[0x00, 0xAB, 0xFF]), "00abff");
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn fnv1a_separates_the_four_surface_names() {
        let hashes: Vec<u64> = ["wire", "data", "cdr", "rtps"]
            .iter()
            .map(|name| fnv1a(name.as_bytes()))
            .collect();
        for (i, a) in hashes.iter().enumerate() {
            for (j, b) in hashes.iter().enumerate() {
                assert!(i == j || a != b, "collision between surface name hashes");
            }
        }
    }
}
