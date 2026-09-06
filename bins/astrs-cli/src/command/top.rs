//! `astrs top` (blueprint §13, §17): the live TUI monitor, against a
//! coordinator or (`--replay`) an `.arec` recording.
//!
//! This is the one verb in the CLI that hands control to
//! [`astrs_tui::run`] rather than printing a report: there is no `--json`
//! form, because there is no batch output to serialize — the whole point
//! is the interactive view.

use std::path::PathBuf;
use std::time::Duration;

use astrs_tui::{App, ClusterView, CoordinatorSource, ReplaySource};

use crate::command::client::Endpoint;
use crate::error::CliError;

/// How often the render loop redraws and polls
/// [`astrs_tui::ClusterView::refresh`] (blueprint: responsive resize, no
/// busy-poll).
const TICK_RATE: Duration = Duration::from_millis(250);

/// `astrs top`'s arguments.
#[derive(Debug, Clone, Default)]
pub struct TopArgs {
    /// Render this recording instead of dialling a coordinator.
    pub replay: Option<PathBuf>,
}

/// Runs `astrs top`: opens the TUI against `endpoint`, or against
/// `args.replay` when set — in which case `endpoint` is never dialled.
///
/// # Errors
///
/// - [`CliError::Replay`] if `--replay` names a file that cannot be
///   opened as an `.arec` recording.
/// - [`CliError::CoordinatorSource`] if the live connection cannot be
///   established.
/// - [`CliError::Tui`] if the terminal cannot be entered (not a TTY,
///   raw-mode setup failed, …) or the render loop itself fails.
pub fn run(endpoint: &Endpoint, args: &TopArgs) -> Result<(), CliError> {
    let view: Box<dyn ClusterView> = match &args.replay {
        Some(path) => Box::new(ReplaySource::open(path)?),
        None => Box::new(CoordinatorSource::connect(
            endpoint.addr,
            endpoint.token.clone(),
        )?),
    };
    let app = App::new(view);
    astrs_tui::run(app, TICK_RATE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::AuthToken;

    use super::*;

    fn endpoint() -> Endpoint {
        // Port 1 on loopback: never bound by this suite, refused instantly.
        Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            AuthToken::ZERO,
        )
    }

    #[test]
    fn a_dead_coordinator_is_reported_rather_than_entering_the_terminal() {
        let args = TopArgs::default();
        let error = run(&endpoint(), &args).unwrap_err();
        assert!(matches!(error, CliError::CoordinatorSource(_)));
    }

    #[test]
    fn a_missing_replay_file_is_reported_rather_than_entering_the_terminal() {
        let args = TopArgs {
            replay: Some(std::env::temp_dir().join(format!(
                "astrs-cli-top-test-does-not-exist-{}.arec",
                std::process::id()
            ))),
        };
        let error = run(&endpoint(), &args).unwrap_err();
        assert!(matches!(error, CliError::Replay(_)));
    }
}
