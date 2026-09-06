//! Path-shaped log filters: `astrs/logs[/level[/node]]` (blueprint §8.4).
//!
//! These are the virtual-input paths a node subscribes to in order to
//! receive its peers' logs as ordinary events, and the same shape backs
//! `astrs logs --filter astrs/logs/warn/camera` on the CLI.

use std::str::FromStr;

use thiserror::Error;

use crate::level::{LevelParseError, LogLevel};
use crate::record::LogRecord;

/// A parsed `astrs/logs[/level[/node]]` subscription: a minimum severity
/// and an optional node restriction.
///
/// # Examples
///
/// ```
/// use astrs_log::LogFilter;
///
/// let filter: LogFilter = "astrs/logs/warn/camera".parse().unwrap();
/// assert_eq!(filter.node.as_deref(), Some("camera"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    /// Only records at this severity or higher pass [`LogFilter::matches`].
    pub min_level: LogLevel,
    /// When set, only records from this node pass [`LogFilter::matches`].
    /// `None` matches every node, including records with no node set.
    ///
    /// A node name containing `/` does not round-trip through
    /// [`LogFilter`]'s `Display`/`FromStr`, since `/` is this path
    /// grammar's own delimiter — but unlike a delimiter inside an
    /// [`EnvFilterLite`](crate::EnvFilterLite) target, it fails loudly
    /// rather than silently: the extra segment(s) make `FromStr` reject
    /// the string as [`FilterPathError::TooManySegments`] instead of
    /// misparsing it. AstRS node ids never contain `/` in practice.
    pub node: Option<String>,
}

impl LogFilter {
    /// The unrestricted filter (`astrs/logs`): every level, every node.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::{LogFilter, LogLevel};
    ///
    /// let f = LogFilter::all();
    /// assert_eq!(f.min_level, LogLevel::Trace);
    /// assert!(f.node.is_none());
    /// ```
    #[must_use]
    pub const fn all() -> Self {
        Self {
            min_level: LogLevel::Trace,
            node: None,
        }
    }

    /// Builds a filter with a minimum level and no node restriction.
    #[must_use]
    pub const fn at_least(min_level: LogLevel) -> Self {
        Self {
            min_level,
            node: None,
        }
    }

    /// Reports whether `record` satisfies this filter: `record.level >=
    /// min_level`, and if `node` is set, `record.node` must equal it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::{HlcTimestamp, LogFilter, LogLevel, LogRecord};
    ///
    /// let record = LogRecord::new(HlcTimestamp::default(), LogLevel::Warn, "cam", "frame drop");
    /// assert!(LogFilter::at_least(LogLevel::Info).matches(&record));
    /// assert!(!LogFilter::at_least(LogLevel::Error).matches(&record));
    /// ```
    #[must_use]
    pub fn matches(&self, record: &LogRecord) -> bool {
        if record.level < self.min_level {
            return false;
        }
        match &self.node {
            None => true,
            Some(want) => record.node.as_deref() == Some(want.as_str()),
        }
    }
}

/// [`LogFilter::from_str`] was given text that is not a valid
/// `astrs/logs[/level[/node]]` path.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FilterPathError {
    /// The path does not start with the literal segments `astrs`, `logs`.
    #[error("filter path {path:?} must start with \"astrs/logs\"")]
    BadPrefix {
        /// The offending path, as given.
        path: String,
    },
    /// The path has more than the `astrs/logs/<level>/<node>` maximum of
    /// four segments.
    #[error("filter path {path:?} has more than 4 segments (astrs/logs[/level[/node]])")]
    TooManySegments {
        /// The offending path, as given.
        path: String,
    },
    /// The level segment did not name a known [`LogLevel`].
    #[error("filter path {path:?} has an invalid level segment: {source}")]
    Level {
        /// The offending path, as given.
        path: String,
        /// Why the level segment failed to parse.
        #[source]
        source: LevelParseError,
    },
}

impl FromStr for LogFilter {
    type Err = FilterPathError;

    /// Parses `"astrs/logs"`, `"astrs/logs/<level>"`, or
    /// `"astrs/logs/<level>/<node>"`. Leading and trailing `/` are
    /// tolerated.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim().trim_matches('/');
        let segments: Vec<&str> = if trimmed.is_empty() {
            Vec::new()
        } else {
            trimmed.split('/').collect()
        };
        match segments.as_slice() {
            ["astrs", "logs"] => Ok(LogFilter::all()),
            ["astrs", "logs", level] => {
                let min_level =
                    level
                        .parse::<LogLevel>()
                        .map_err(|source| FilterPathError::Level {
                            path: s.to_owned(),
                            source,
                        })?;
                Ok(LogFilter {
                    min_level,
                    node: None,
                })
            }
            ["astrs", "logs", level, node] => {
                let min_level =
                    level
                        .parse::<LogLevel>()
                        .map_err(|source| FilterPathError::Level {
                            path: s.to_owned(),
                            source,
                        })?;
                Ok(LogFilter {
                    min_level,
                    node: Some((*node).to_owned()),
                })
            }
            ["astrs", "logs", ..] => Err(FilterPathError::TooManySegments { path: s.to_owned() }),
            _ => Err(FilterPathError::BadPrefix { path: s.to_owned() }),
        }
    }
}

impl std::fmt::Display for LogFilter {
    /// Renders back to the canonical `astrs/logs[/level[/node]]` path —
    /// the inverse of [`LogFilter::from_str`]. `astrs/logs` alone is
    /// emitted only for [`LogFilter::all`]; a node restriction without an
    /// explicit level (not constructible via `FromStr`, but reachable by
    /// building a `LogFilter` directly) still renders its level, since
    /// the node segment cannot appear without one.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::LogFilter;
    ///
    /// let filter: LogFilter = "astrs/logs/warn/camera".parse().unwrap();
    /// assert_eq!(filter.to_string(), "astrs/logs/warn/camera");
    /// assert_eq!(LogFilter::all().to_string(), "astrs/logs");
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.node, self.min_level == LogLevel::Trace) {
            (None, true) => write!(f, "astrs/logs"),
            (None, false) => write!(f, "astrs/logs/{}", self.min_level),
            (Some(node), _) => write!(f, "astrs/logs/{}/{node}", self.min_level),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn rec(level: LogLevel, node: Option<&str>) -> LogRecord {
        let mut r = LogRecord::new(HlcTimestamp::default(), level, "t", "m");
        r.node = node.map(str::to_owned);
        r
    }

    #[test]
    fn parses_bare_path() {
        let f: LogFilter = "astrs/logs".parse().unwrap();
        assert_eq!(f, LogFilter::all());
    }

    #[test]
    fn parses_level_only() {
        let f: LogFilter = "astrs/logs/warn".parse().unwrap();
        assert_eq!(f.min_level, LogLevel::Warn);
        assert_eq!(f.node, None);
    }

    #[test]
    fn display_round_trips_through_from_str_for_every_shape() {
        for original in ["astrs/logs", "astrs/logs/warn", "astrs/logs/error/planner"] {
            let filter: LogFilter = original.parse().unwrap();
            let rendered = filter.to_string();
            assert_eq!(rendered, original);
            let reparsed: LogFilter = rendered.parse().unwrap();
            assert_eq!(reparsed, filter);
        }
    }

    #[test]
    fn display_renders_level_without_node_when_min_level_is_not_trace() {
        let filter = LogFilter::at_least(LogLevel::Debug);
        assert_eq!(filter.to_string(), "astrs/logs/debug");
    }

    #[test]
    fn parses_level_and_node() {
        let f: LogFilter = "astrs/logs/error/planner".parse().unwrap();
        assert_eq!(f.min_level, LogLevel::Error);
        assert_eq!(f.node.as_deref(), Some("planner"));
    }

    #[test]
    fn tolerates_leading_and_trailing_slashes() {
        let f: LogFilter = "/astrs/logs/info/".parse().unwrap();
        assert_eq!(f.min_level, LogLevel::Info);
    }

    #[test]
    fn rejects_bad_prefix() {
        let err = "dora/logs".parse::<LogFilter>().unwrap_err();
        assert!(matches!(err, FilterPathError::BadPrefix { .. }));
    }

    #[test]
    fn rejects_empty_path() {
        assert!(matches!(
            "".parse::<LogFilter>(),
            Err(FilterPathError::BadPrefix { .. })
        ));
    }

    #[test]
    fn rejects_too_many_segments() {
        let err = "astrs/logs/warn/camera/extra"
            .parse::<LogFilter>()
            .unwrap_err();
        assert!(matches!(err, FilterPathError::TooManySegments { .. }));
    }

    #[test]
    fn rejects_bad_level_segment() {
        let err = "astrs/logs/verbose".parse::<LogFilter>().unwrap_err();
        assert!(matches!(err, FilterPathError::Level { .. }));
    }

    /// One row of the [`filter_matrix`] test table.
    struct FilterCase {
        min_level: LogLevel,
        filter_node: Option<&'static str>,
        record_level: LogLevel,
        record_node: Option<&'static str>,
        expected: bool,
    }

    #[test]
    fn filter_matrix() {
        let cases = [
            // No node restriction: level threshold alone decides.
            FilterCase {
                min_level: LogLevel::Info,
                filter_node: None,
                record_level: LogLevel::Info,
                record_node: Some("cam"),
                expected: true,
            },
            FilterCase {
                min_level: LogLevel::Info,
                filter_node: None,
                record_level: LogLevel::Debug,
                record_node: Some("cam"),
                expected: false,
            },
            FilterCase {
                min_level: LogLevel::Info,
                filter_node: None,
                record_level: LogLevel::Error,
                record_node: None,
                expected: true,
            },
            // Node restriction: both level and node must agree.
            FilterCase {
                min_level: LogLevel::Warn,
                filter_node: Some("cam"),
                record_level: LogLevel::Error,
                record_node: Some("cam"),
                expected: true,
            },
            FilterCase {
                min_level: LogLevel::Warn,
                filter_node: Some("cam"),
                record_level: LogLevel::Error,
                record_node: Some("planner"),
                expected: false,
            },
            FilterCase {
                min_level: LogLevel::Warn,
                filter_node: Some("cam"),
                record_level: LogLevel::Info,
                record_node: Some("cam"),
                expected: false,
            },
            FilterCase {
                min_level: LogLevel::Warn,
                filter_node: Some("cam"),
                record_level: LogLevel::Error,
                record_node: None,
                expected: false,
            },
            // Trace threshold: everything at or above trace passes (i.e. everything).
            FilterCase {
                min_level: LogLevel::Trace,
                filter_node: None,
                record_level: LogLevel::Trace,
                record_node: None,
                expected: true,
            },
        ];
        for case in cases {
            let filter = LogFilter {
                min_level: case.min_level,
                node: case.filter_node.map(str::to_owned),
            };
            let record = rec(case.record_level, case.record_node);
            assert_eq!(
                filter.matches(&record),
                case.expected,
                "min_level={:?} filter_node={:?} record_level={:?} record_node={:?}",
                case.min_level,
                case.filter_node,
                case.record_level,
                case.record_node,
            );
        }
    }
}
