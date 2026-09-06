//! The crate's unified error type.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::env_filter::DirectiveError;
use crate::filter::FilterPathError;
use crate::level::LevelParseError;

/// Everything that can go wrong in `astrs-log`: rotation I/O, JSON
/// encode/decode of [`LogRecord`](crate::LogRecord)s, and parsing of
/// levels, filter paths, and env-filter-lite directives.
#[derive(Debug, Error)]
pub enum LogError {
    /// A filesystem operation on a log file failed.
    #[error("log I/O error at {path}: {source}")]
    Io {
        /// The file the operation was acting on.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: io::Error,
    },

    /// A [`LogRecord`](crate::LogRecord) failed to serialize to JSON.
    ///
    /// In practice this is effectively unreachable for well-formed
    /// records (every field type this crate exposes serializes
    /// infallibly), but `serde_json::to_string` still returns a
    /// `Result`, and this crate does not `unwrap()`/`expect()` it away.
    #[error("failed to encode log record as JSON: {0}")]
    Encode(#[source] serde_json::Error),

    /// A line read back from a log file was not valid `LogRecord` JSON.
    #[error("malformed log record at {path} line {line}: {source}")]
    Decode {
        /// The file the malformed line was read from.
        path: PathBuf,
        /// The 1-based line number of the malformed record.
        line: u64,
        /// The underlying JSON decode failure.
        #[source]
        source: serde_json::Error,
    },

    /// [`LogLevel`](crate::LogLevel)'s `FromStr` rejected its input.
    #[error(transparent)]
    InvalidLevel(#[from] LevelParseError),

    /// [`LogFilter`](crate::LogFilter)'s `FromStr` rejected its input
    /// path.
    #[error(transparent)]
    InvalidFilterPath(#[from] FilterPathError),

    /// [`EnvFilterLite`](crate::EnvFilterLite)'s `FromStr` rejected one of
    /// its directives.
    #[error(transparent)]
    InvalidDirective(#[from] DirectiveError),
}

/// Result alias used throughout this crate.
pub type Result<T> = std::result::Result<T, LogError>;

/// Attaches a file path to an [`io::Error`], turning it into a
/// [`LogError::Io`].
///
/// Kept private: it exists purely to keep the rotating writer and the log
/// file reader from repeating `.map_err(|source| LogError::Io { path:
/// ..., source })` at every fallible filesystem call.
pub(crate) trait IoResultExt<T> {
    /// Wraps an I/O error with the path it occurred on.
    fn log_io(self, path: &Path) -> Result<T>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn log_io(self, path: &Path) -> Result<T> {
        self.map_err(|source| LogError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn io_error_carries_the_path() {
        let path = PathBuf::from("does/not/exist.log");
        let io_err = io::Error::new(io::ErrorKind::NotFound, "nope");
        let err: LogError = Err::<(), _>(io_err).log_io(&path).unwrap_err();
        assert!(err.to_string().contains("does/not/exist.log"));
    }

    #[test]
    fn invalid_level_converts_via_from() {
        let parse_err = LevelParseError {
            input: "bogus".to_owned(),
        };
        let err: LogError = parse_err.into();
        assert!(matches!(err, LogError::InvalidLevel(_)));
        assert!(err.to_string().contains("bogus"));
    }
}
