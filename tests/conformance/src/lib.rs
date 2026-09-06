//! The AstRS conformance suite's shared machinery (blueprint §20.3).
//!
//! The suite runs *committed example manifests* against the real `astrs run`
//! verb, with no build step of its own. This crate is where the problems that
//! makes possible are solved once for every test.
//!
//! # The two ways a milestone is proved
//!
//! | Lane | Entry point | What only it proves |
//! |---|---|---|
//! | In process | [`stage_manifest`] + `astrs_cli::command::run::run` | the dataflow machinery, with the whole captured terminal available to explain a failure |
//! | Out of process | [`AstrsCli`] | clap parsing, `main`'s exit code, the real process's stdout — the command an adopter types |
//!
//! `tests/m1_single_machine.rs` drives the first, `tests/m1_cli_process.rs`
//! the second, and `tests/m1_example_estate.rs` checks that the committed
//! files the other two depend on have not drifted from each other.
//!
//! # The modules
//!
//! * [`error`] — the one error type, written so a failure message is a
//!   next action rather than a puzzle.
//! * [`paths`] — finding the workspace, the built binaries (without
//!   `CARGO_BIN_EXE_`, which does not cross packages) and the committed
//!   manifests.
//! * [`stage`] — rewriting a committed manifest so a test can run it from a
//!   temporary directory against binaries cargo already built.
//! * [`cli`] — driving the real `astrs` binary as a child process.
//! * [`report`] — reading the JSON the `--json` verbs print.
//!
//! # Building what the suite runs
//!
//! The suite compiles nothing. Cargo builds the *libraries* of the example
//! packages because they are dev-dependencies, but not their binaries and not
//! the `astrs` binary, so those are an explicit precondition:
//!
//! ```bash
//! cargo build -p astrs-cli -p hello-timer -p rust-pipeline \
//!             -p service-roundtrip -p shm-zero-copy-probe
//! cargo test  -p astrs-conformance
//! ```
//!
//! A missing binary fails with exactly the `cargo build -p …` line that
//! produces it. The suite never skips: a conformance suite that silently skips
//! is a conformance suite that proves nothing.

pub mod cli;
pub mod error;
pub mod paths;
pub mod report;
pub mod stage;

pub use cli::{AstrsCli, CliOutcome, DEFAULT_TIMEOUT};
pub use error::FixtureError;
pub use paths::{
    CLI_BINARY, CLI_PACKAGE, ENV_BIN_DIR, EXAMPLES, binary, cli_binary, example_dir,
    example_manifest, example_manifest_relative, example_package, expected_unconsumed_output,
    fixture, target_dir, workspace_root,
};
pub use report::{CliDiagnostic, CliNodeResult, CliRunReport, CliValidateReport, json_tail};
pub use stage::{
    CommittedNodeBinary, Fixture, StageOptions, committed_node_binaries, make_scratch_dir,
    scratch_dir, stage_example, stage_example_env, stage_manifest, stage_manifest_with,
};

/// A per-run temporary file a node writes a result to.
///
/// Named after the process that asked for it, so two suites running at once
/// never read each other's artefacts, and placed under
/// [`std::env::temp_dir`] so nothing is written into the checkout.
#[must_use]
pub fn result_file(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "astrs-m1-{pid}-{seq}-{name}",
        pid = std::process::id(),
        seq = RESULTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// Distinguishes one result file from the next within a process.
static RESULTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Result files are unique per call and live under the temporary
    /// directory, never in the checkout.
    #[test]
    fn result_files_are_unique_and_temporary() {
        let first = result_file("tally.json");
        let second = result_file("tally.json");
        assert_ne!(first, second);
        assert!(first.starts_with(std::env::temp_dir()));
        assert!(first.to_string_lossy().ends_with("tally.json"));
    }
}
