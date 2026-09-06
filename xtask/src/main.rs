//! Repository automation for AstRS.
//!
//! Run with `cargo xtask <command>`. Each subcommand below dispatches to its
//! own module -- this file is intentionally thin: argument parsing and
//! result-to-exit-code plumbing only, never the check logic itself (that
//! lives in, and is tested by, [`schema`], [`snapshot_protocol`],
//! [`layer_lint`] and [`preflight`]).

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod error;
mod layer_lint;
mod preflight;
mod schema;
mod snapshot_protocol;
mod sys_sweep;
mod version_pins;
mod workspace;

use error::XtaskError;

/// Repository automation tasks for the AstRS workspace.
#[derive(Debug, Parser)]
#[command(name = "xtask", version, about, long_about = None)]
struct Cli {
    /// The task to run.
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Emit the JSON schema for the dataflow manifest to
    /// `astrs-schema.json` at the workspace root.
    Schema {
        /// Verify the committed file matches the current schema; never
        /// writes. The mode a CI/preflight gate needs.
        #[arg(long)]
        check: bool,
    },
    /// Verify the wire-protocol freeze (blueprint §7.2, §24.1).
    SnapshotProtocol {
        /// Compare only the two committed golden files against each other,
        /// without compiling or running `astrs-wire` -- a fast, strictly
        /// weaker sanity check. Omit for the authoritative check.
        #[arg(long)]
        frozen_only: bool,
    },
    /// Check that crate dependencies only ever point down the layer stack
    /// (blueprint §4.1).
    LayerLint,
    /// Run the full release preflight: every §20 quality gate, cheapest and
    /// most structural first, reporting pass/fail per step in one pass.
    Preflight {
        /// Also run `cargo publish --dry-run` for every publishable crate,
        /// in dependency order.
        #[arg(long)]
        publish_dry_run: bool,
    },
}

fn main() -> ExitCode {
    let root = workspace::workspace_root();
    match run(&Cli::parse().command, &root) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch one parsed [`Command`], print its human-readable result, and
/// report whether it succeeded. Split from [`main`] so the exit-code
/// mapping in `main` stays a single, obvious `match`.
///
/// # Errors
///
/// Whatever the dispatched module's own entry point returns -- always a
/// failure of `xtask` itself (an unreadable file, an un-spawnable `cargo`),
/// never a step that ran and legitimately failed; that is `Ok(false)`.
fn run(command: &Command, root: &Path) -> Result<bool, XtaskError> {
    match command {
        Command::Schema { check: false } => {
            let path = schema::write(root)?;
            println!("wrote {}", path.display());
            Ok(true)
        }
        Command::Schema { check: true } => {
            let outcome = schema::check(root)?;
            println!("{outcome}");
            Ok(outcome.is_match())
        }
        Command::SnapshotProtocol { frozen_only: true } => {
            let outcome = snapshot_protocol::run_frozen_only(root)?;
            println!("{}", outcome.detail);
            Ok(outcome.passed)
        }
        Command::SnapshotProtocol { frozen_only: false } => {
            let outcome = snapshot_protocol::run_full(root)?;
            println!("{}", outcome.detail);
            Ok(outcome.passed)
        }
        Command::LayerLint => {
            let report = layer_lint::run(root, layer_lint::PRODUCTION_LAYERS)?;
            println!(
                "layer-lint: {} crate(s) checked, {} skipped, {} violation(s)",
                report.checked,
                report.skipped.len(),
                report.violations.len()
            );
            for violation in &report.violations {
                println!("  {violation}");
            }
            Ok(report.is_clean())
        }
        Command::Preflight { publish_dry_run } => {
            let report = preflight::run(root, *publish_dry_run)?;
            print!("{}", report.summary());
            Ok(report.is_success())
        }
    }
}
