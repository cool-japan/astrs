//! Default-lane fuzz tests for `astrs-data`'s Arrow IPC reader (blueprint
//! §15).
//!
//! `cargo test -p astrs-fuzz` / `cargo nextest run -p astrs-fuzz` runs the
//! structured fuzz loop at `ASTRS_FUZZ_ITERS` (10 000 by default);
//! `scripts/fuzz-nightly.sh` overrides that to 5 000 000. The corpus replay
//! is unconditional: it runs every time, regardless of `ASTRS_FUZZ_ITERS`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_fuzz::support::driver::{fuzz_surface, replay_corpus};
use astrs_fuzz::surfaces::data;

#[test]
fn arrow_ipc_reader_survives_structured_fuzzing() {
    let seeds = data::seeds();
    let report = fuzz_surface("data", &seeds, data::generate, data::check);
    assert!(report.is_clean(), "{}", report.summary());
    assert!(report.cases_run > 0);
}

#[test]
fn arrow_ipc_reader_regression_corpus_replays_clean() {
    let report = replay_corpus(data::CORPUS_DIR, data::check);
    assert!(
        report.cases_run > 0,
        "corpus directory {} is empty",
        data::CORPUS_DIR
    );
    assert!(report.is_clean(), "{}", report.summary());
}
