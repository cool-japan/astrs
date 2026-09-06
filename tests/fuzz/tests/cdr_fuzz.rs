//! Default-lane fuzz tests for `astrs-cdr`'s reader (blueprint §15).
//!
//! `cargo test -p astrs-fuzz` / `cargo nextest run -p astrs-fuzz` runs the
//! structured fuzz loop at `ASTRS_FUZZ_ITERS` (10 000 by default);
//! `scripts/fuzz-nightly.sh` overrides that to 5 000 000. The corpus replay
//! is unconditional: it runs every time, regardless of `ASTRS_FUZZ_ITERS`.
//! `astrs-cdr`'s own `cargo test` path never runs this deep -- see that
//! crate's module docs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_fuzz::support::driver::{fuzz_surface, replay_corpus};
use astrs_fuzz::surfaces::cdr;

#[test]
fn cdr_reader_survives_structured_fuzzing() {
    let seeds = cdr::seeds();
    let report = fuzz_surface("cdr", &seeds, cdr::generate, cdr::check);
    assert!(report.is_clean(), "{}", report.summary());
    assert!(report.cases_run > 0);
}

#[test]
fn cdr_reader_regression_corpus_replays_clean() {
    let report = replay_corpus(cdr::CORPUS_DIR, cdr::check);
    assert!(
        report.cases_run > 0,
        "corpus directory {} is empty",
        cdr::CORPUS_DIR
    );
    assert!(report.is_clean(), "{}", report.summary());
}
