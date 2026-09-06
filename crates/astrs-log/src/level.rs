//! Log severity levels.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Severity of a [`LogRecord`](crate::LogRecord).
///
/// Variants are declared in **severity-ascending** order (`Trace` is least
/// severe, `Error` is most severe) and derive [`Ord`] from that declaration
/// order, so `level >= min_level` is exactly "at least as severe as
/// `min_level`" — the natural reading of [`LogFilter::min_level`](crate::LogFilter::min_level)
/// and of the `astrs/logs/<level>` virtual-input path (blueprint §8.4),
/// where subscribing at `warn` means "warn and error, not info/debug/trace".
///
/// This is the **opposite** direction from [`tracing::Level`]'s own
/// [`Ord`] impl, which orders by verbosity (`TRACE > DEBUG > INFO > WARN >
/// ERROR`) so that a `LevelFilter` reads as "at most this verbose". The two
/// types are never compared against each other directly — only converted
/// via [`From`] — so the direction flip is contained entirely inside those
/// conversions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Fine-grained diagnostic detail, off by default in most deployments.
    Trace,
    /// Diagnostic detail useful during development.
    Debug,
    /// Routine operational information.
    Info,
    /// An unexpected condition that is not (yet) an error.
    Warn,
    /// A failure that affects correctness or availability.
    Error,
}

impl LogLevel {
    /// All five levels in severity-ascending order.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::LogLevel;
    ///
    /// assert_eq!(LogLevel::ALL.len(), 5);
    /// assert_eq!(LogLevel::ALL[0], LogLevel::Trace);
    /// assert_eq!(LogLevel::ALL[4], LogLevel::Error);
    /// ```
    pub const ALL: [LogLevel; 5] = [
        LogLevel::Trace,
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
    ];

    /// The canonical lowercase spelling used on the wire, in
    /// `astrs/logs/<level>` virtual-input paths, and in `EnvFilterLite`
    /// directives (e.g. `"warn"`).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::LogLevel;
    ///
    /// assert_eq!(LogLevel::Warn.as_str(), "warn");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    /// The level padded to a fixed 5-character uppercase field, used by the
    /// aligned human formatter (`format::human`) so message columns line
    /// up regardless of level.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::LogLevel;
    ///
    /// assert_eq!(LogLevel::Info.as_padded_str(), "INFO ");
    /// assert_eq!(LogLevel::Error.as_padded_str(), "ERROR");
    /// ```
    #[must_use]
    pub const fn as_padded_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO ",
            LogLevel::Warn => "WARN ",
            LogLevel::Error => "ERROR",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`LogLevel::from_str`] was given text that does not name a known level.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unrecognized log level {input:?} (expected one of trace, debug, info, warn, error)")]
pub struct LevelParseError {
    /// The text that failed to parse.
    pub input: String,
}

impl FromStr for LogLevel {
    type Err = LevelParseError;

    /// Parses a level name case-insensitively; `"warning"` and `"err"` are
    /// accepted as aliases for `Warn` and `Error` respectively, matching
    /// common CLI ergonomics (`RUST_LOG=warning`, `--level err`).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::LogLevel;
    ///
    /// assert_eq!("WARN".parse::<LogLevel>().unwrap(), LogLevel::Warn);
    /// assert_eq!("Warning".parse::<LogLevel>().unwrap(), LogLevel::Warn);
    /// assert!("nope".parse::<LogLevel>().is_err());
    /// ```
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Ok(LogLevel::Trace),
            "debug" => Ok(LogLevel::Debug),
            "info" => Ok(LogLevel::Info),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "error" | "err" => Ok(LogLevel::Error),
            _ => Err(LevelParseError {
                input: s.to_owned(),
            }),
        }
    }
}

impl From<LogLevel> for tracing::Level {
    /// Converts to `tracing::Level`, flipping to that type's
    /// verbosity-ordered convention. Total: every [`LogLevel`] variant has
    /// exactly one `tracing::Level` counterpart.
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => tracing::Level::TRACE,
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Error => tracing::Level::ERROR,
        }
    }
}

impl From<tracing::Level> for LogLevel {
    /// Converts from `tracing::Level`. Total: every `tracing::Level`
    /// variant has exactly one [`LogLevel`] counterpart.
    fn from(level: tracing::Level) -> Self {
        match level {
            tracing::Level::TRACE => LogLevel::Trace,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::ERROR => LogLevel::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn severity_ascending_order() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn round_trips_through_display_and_from_str() {
        for level in LogLevel::ALL {
            let parsed: LogLevel = level.to_string().parse().expect("canonical string parses");
            assert_eq!(parsed, level);
        }
    }

    #[test]
    fn from_str_is_case_insensitive_with_aliases() {
        assert_eq!("TRACE".parse::<LogLevel>().unwrap(), LogLevel::Trace);
        assert_eq!("Info".parse::<LogLevel>().unwrap(), LogLevel::Info);
        assert_eq!("WARNING".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("err".parse::<LogLevel>().unwrap(), LogLevel::Error);
    }

    #[test]
    fn from_str_rejects_unknown_text() {
        let err = "verbose".parse::<LogLevel>().unwrap_err();
        assert_eq!(err.input, "verbose");
        assert!(err.to_string().contains("verbose"));
    }

    #[test]
    fn tracing_level_conversion_round_trips_and_flips_direction() {
        for level in LogLevel::ALL {
            let t: tracing::Level = level.into();
            let back: LogLevel = t.into();
            assert_eq!(level, back);
        }
        // tracing::Level orders by verbosity (TRACE is greatest); LogLevel
        // orders by severity (Trace is least) -- confirm the flip.
        assert!(tracing::Level::TRACE > tracing::Level::ERROR);
        assert!(LogLevel::Trace < LogLevel::Error);
    }

    #[test]
    fn padded_strings_are_all_five_columns_wide() {
        for level in LogLevel::ALL {
            assert_eq!(level.as_padded_str().len(), 5);
        }
    }

    #[test]
    fn serde_uses_lowercase_names() {
        let json = serde_json::to_string(&LogLevel::Warn).expect("serialize");
        assert_eq!(json, "\"warn\"");
        let back: LogLevel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, LogLevel::Warn);
    }
}
