//! [`TuiError`]: the top-level error `astrs top` reports for a session that
//! could not even start (as opposed to [`crate::view::ViewError`], which a
//! running session absorbs and keeps going through).

use std::path::PathBuf;

/// Why an `astrs top` session could not run at all.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TuiError {
    /// Standard output is not a terminal (blueprint: refuse cleanly rather
    /// than writing escape codes into a pipe or a file).
    #[error(
        "astrs top needs an interactive terminal on stdout (got a pipe or file); \
         redirect elsewhere or run this in a real terminal"
    )]
    NotATerminal,
    /// Entering or leaving the alternate screen, or another terminal
    /// setup/teardown step, failed.
    #[error("terminal setup failed: {0}")]
    Terminal(#[source] std::io::Error),
    /// The `.arec` recording could not be opened for replay.
    #[error("cannot open recording {path}: {source}")]
    Recording {
        /// The file that was opened.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The coordinator connection could not be established at all.
    #[error("cannot reach the coordinator at {endpoint}: {reason}")]
    Connect {
        /// The endpoint dialled.
        endpoint: String,
        /// Why it failed.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_variant_renders_a_non_empty_message() {
        let errors = [
            TuiError::NotATerminal,
            TuiError::Terminal(std::io::Error::other("boom")),
            TuiError::Recording {
                path: PathBuf::from("session.arec"),
                source: std::io::Error::other("truncated"),
            },
            TuiError::Connect {
                endpoint: "127.0.0.1:7407".to_owned(),
                reason: "refused".to_owned(),
            },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn not_a_terminal_names_the_fix() {
        assert!(TuiError::NotATerminal.to_string().contains("terminal"));
    }
}
