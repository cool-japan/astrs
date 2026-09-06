//! The `astrs` binary — one executable for the whole middleware
//! (blueprint §17).
//!
//! Deliberately thin: parse `argv`, hand the result to
//! [`astrs_cli::dispatch`], and turn its `i32` into a process
//! [`std::process::ExitCode`] — every actual verb implementation lives in
//! [`astrs_cli`], where it is reachable (and tested) without a process
//! boundary at all. See that crate's own docs for the current verb
//! coverage.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use astrs_cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let code = astrs_cli::dispatch(
        cli,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        stdout_is_terminal,
    );
    // `i32` exit codes used throughout this crate (`0`..=`2`, and
    // `EXIT_UNAVAILABLE` = 69) all fit in `u8`; a negative or >255 value
    // is not producible by anything in `astrs_cli::dispatch`, so this
    // truncation is exact in practice, not lossy — the workspace's
    // "no unwrap/expect" policy still means this is written as a
    // deliberate, documented cast rather than a fallible conversion this
    // function would otherwise have to handle.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    ExitCode::from(code as u8)
}
