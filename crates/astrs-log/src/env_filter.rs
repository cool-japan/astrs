//! `RUST_LOG`-style directive parsing — a deliberately small subset.
//!
//! This is **not** a reimplementation of `tracing_subscriber::EnvFilter`:
//! no span-field predicates, no regex, no `target[span{field=value}]`
//! syntax. It supports exactly two directive shapes, comma-separated:
//!
//! - a bare level (`info`) — sets the default threshold for every target;
//! - a `target=level` pair (`astrs_daemon=debug`) — overrides the
//!   threshold for that target and everything nested under it
//!   (`astrs_daemon::spawn`, `astrs_daemon::spawn::backoff`, ...).
//!
//! When several directives' targets match a record's target, the
//! **longest** (most specific) target wins, exactly as `RUST_LOG` behaves.
//! Building the full directive grammar is `astrs-telemetry`'s job (W2) if
//! it ever needs it — this crate only needs enough to filter `astrs logs`.
//!
//! **Constraint on target strings:** a target must not contain `,` or `=`
//! — both are directive-grammar delimiters, so a target containing either
//! does not round-trip through `Display`/`FromStr` (a comma splits it
//! into extra directives; an equals sign splits it into a target/level
//! pair). This is not a limitation specific to this crate's "lite"
//! subset: `tracing_subscriber::EnvFilter` parses the same grammar and
//! has the identical constraint. It is never a practical concern here,
//! since every real target this crate sees is a Rust module path
//! (`astrs_daemon::spawn`), which cannot contain either character.

use std::str::FromStr;

use thiserror::Error;

use crate::level::{LevelParseError, LogLevel};
use crate::record::LogRecord;

/// A parsed `RUST_LOG`-lite directive string, e.g. `"info,astrs_daemon=debug"`.
///
/// # Examples
///
/// ```
/// use astrs_log::{EnvFilterLite, HlcTimestamp, LogLevel, LogRecord};
///
/// let filter: EnvFilterLite = "info,astrs_daemon=debug".parse().unwrap();
///
/// let daemon_debug = LogRecord::new(HlcTimestamp::default(), LogLevel::Debug, "astrs_daemon::spawn", "spawned");
/// assert!(filter.matches(&daemon_debug)); // astrs_daemon is overridden to debug
///
/// let other_debug = LogRecord::new(HlcTimestamp::default(), LogLevel::Debug, "astrs_cli", "parsing args");
/// assert!(!filter.matches(&other_debug)); // falls back to the "info" default
/// ```
#[derive(Debug, Clone)]
pub struct EnvFilterLite {
    /// Threshold used for any target that no directive's target matches.
    default_level: LogLevel,
    /// `(target, level)` overrides, in the order they were added. Not
    /// pre-sorted: [`EnvFilterLite::threshold_for`] scans all of them and
    /// keeps the longest match, which is cheap for the handful of
    /// directives a real filter string carries.
    directives: Vec<(String, LogLevel)>,
}

impl PartialEq for EnvFilterLite {
    /// Compares `default_level` and the directives *as a set*: insertion
    /// order never affects [`EnvFilterLite::matches`] (every directive is
    /// scanned regardless of position), so two filters with the same
    /// default and the same target/level pairs in a different order are
    /// equal. A derived, order-sensitive `PartialEq` would make this type
    /// fail its own round-trip property (`Display` always sorts;
    /// `FromStr` preserves input order), which would be a worse bug than
    /// the one extra `sort()` this costs.
    fn eq(&self, other: &Self) -> bool {
        if self.default_level != other.default_level
            || self.directives.len() != other.directives.len()
        {
            return false;
        }
        let mut mine: Vec<&(String, LogLevel)> = self.directives.iter().collect();
        let mut theirs: Vec<&(String, LogLevel)> = other.directives.iter().collect();
        mine.sort();
        theirs.sort();
        mine == theirs
    }
}

impl Eq for EnvFilterLite {}

impl EnvFilterLite {
    /// Builds a filter with only a default threshold and no per-target
    /// overrides.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::{EnvFilterLite, LogLevel};
    ///
    /// let f = EnvFilterLite::new(LogLevel::Warn);
    /// assert_eq!(f.default_level(), LogLevel::Warn);
    /// ```
    #[must_use]
    pub const fn new(default_level: LogLevel) -> Self {
        Self {
            default_level,
            directives: Vec::new(),
        }
    }

    /// Adds (or replaces) a per-target override, builder-style. Useful for
    /// constructing a filter programmatically instead of formatting and
    /// re-parsing a directive string.
    ///
    /// `target` must not contain `,` or `=` — both are directive-grammar
    /// delimiters (see this module's top-level docs) — or the resulting
    /// filter will not round-trip through `Display`/`FromStr`; this is
    /// not validated here since every real caller passes a Rust module
    /// path, which cannot contain either character.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::{EnvFilterLite, LogLevel};
    ///
    /// let f = EnvFilterLite::new(LogLevel::Info).with_target("astrs_daemon", LogLevel::Debug);
    /// assert_eq!(f.default_level(), LogLevel::Info);
    /// ```
    #[must_use]
    pub fn with_target(mut self, target: impl Into<String>, level: LogLevel) -> Self {
        let target = target.into();
        match self.directives.iter_mut().find(|(t, _)| *t == target) {
            Some(entry) => entry.1 = level,
            None => self.directives.push((target, level)),
        }
        self
    }

    /// The threshold applied to targets that no directive matches.
    #[must_use]
    pub const fn default_level(&self) -> LogLevel {
        self.default_level
    }

    /// Reports whether `record.level` meets the threshold for
    /// `record.target` (the most specific matching directive, or the
    /// default level if none match).
    #[must_use]
    pub fn matches(&self, record: &LogRecord) -> bool {
        record.level >= self.threshold_for(&record.target)
    }

    /// Resolves the effective threshold for a target string: the level of
    /// the longest directive target that is a prefix-match of `target`
    /// (equal, or followed by `::`), or [`EnvFilterLite::default_level`]
    /// if nothing matches.
    fn threshold_for(&self, target: &str) -> LogLevel {
        let mut best: Option<(usize, LogLevel)> = None;
        for (prefix, level) in &self.directives {
            if !target_matches(target, prefix) {
                continue;
            }
            if best.is_none_or_shorter_than(prefix.len()) {
                best = Some((prefix.len(), *level));
            }
        }
        best.map_or(self.default_level, |(_, level)| level)
    }
}

/// `target == prefix`, or `target` starts with `prefix` followed by a
/// `::` module separator — a crate/module-path prefix match with no
/// allocation.
fn target_matches(target: &str, prefix: &str) -> bool {
    if !target.starts_with(prefix) {
        return false;
    }
    let rest = &target[prefix.len()..];
    rest.is_empty() || rest.starts_with("::")
}

/// Tiny local helper so `threshold_for` reads as "is this candidate more
/// specific than the current best" without importing an extra crate for
/// one comparison.
trait BestLenExt {
    fn is_none_or_shorter_than(&self, len: usize) -> bool;
}

impl BestLenExt for Option<(usize, LogLevel)> {
    fn is_none_or_shorter_than(&self, len: usize) -> bool {
        match self {
            None => true,
            Some((best_len, _)) => len > *best_len,
        }
    }
}

/// [`EnvFilterLite::from_str`] was given a directive it could not parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DirectiveError {
    /// The level half of a directive (`info`, or the right side of
    /// `target=level`) did not name a known [`LogLevel`].
    #[error("invalid level in directive {directive:?}: {source}")]
    Level {
        /// The full directive fragment that failed.
        directive: String,
        /// Why the level text failed to parse.
        #[source]
        source: LevelParseError,
    },
    /// A `target=level` directive had nothing before the `=`.
    #[error("directive {directive:?} has an empty target before '='")]
    EmptyTarget {
        /// The full directive fragment that failed.
        directive: String,
    },
}

impl FromStr for EnvFilterLite {
    type Err = DirectiveError;

    /// Parses a comma-separated directive string. Whitespace around
    /// directives and empty segments (e.g. a trailing comma) are ignored.
    /// If no bare-level directive is present, [`EnvFilterLite::default_level`]
    /// is [`LogLevel::Info`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut filter = EnvFilterLite::new(LogLevel::Info);
        for raw in s.split(',') {
            let part = raw.trim();
            if part.is_empty() {
                continue;
            }
            match part.split_once('=') {
                None => {
                    let level =
                        part.parse::<LogLevel>()
                            .map_err(|source| DirectiveError::Level {
                                directive: part.to_owned(),
                                source,
                            })?;
                    filter.default_level = level;
                }
                Some((target, level_str)) => {
                    // `part` is already outer-trimmed, but whitespace can
                    // still surround the '=' itself (e.g. "target = level").
                    let target = target.trim();
                    let level_str = level_str.trim();
                    if target.is_empty() {
                        return Err(DirectiveError::EmptyTarget {
                            directive: part.to_owned(),
                        });
                    }
                    let level =
                        level_str
                            .parse::<LogLevel>()
                            .map_err(|source| DirectiveError::Level {
                                directive: part.to_owned(),
                                source,
                            })?;
                    filter = filter.with_target(target, level);
                }
            }
        }
        Ok(filter)
    }
}

impl std::fmt::Display for EnvFilterLite {
    /// Renders back to directive-string form: the default level first,
    /// then every per-target override as `target=level`, sorted by
    /// target name for a deterministic result independent of the order
    /// directives were added in (matching this crate's general
    /// determinism requirement for anything format-related).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::EnvFilterLite;
    ///
    /// let filter: EnvFilterLite = "astrs_cli=warn,info,astrs_daemon=debug".parse().unwrap();
    /// assert_eq!(filter.to_string(), "info,astrs_cli=warn,astrs_daemon=debug");
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.default_level)?;
        let mut sorted: Vec<&(String, LogLevel)> = self.directives.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (target, level) in sorted {
            write!(f, ",{target}={level}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn rec(level: LogLevel, target: &str) -> LogRecord {
        LogRecord::new(HlcTimestamp::default(), level, target, "m")
    }

    #[test]
    fn equality_is_independent_of_directive_insertion_order() {
        let a = EnvFilterLite::new(LogLevel::Info)
            .with_target("astrs_daemon", LogLevel::Debug)
            .with_target("astrs_cli", LogLevel::Warn);
        let b = EnvFilterLite::new(LogLevel::Info)
            .with_target("astrs_cli", LogLevel::Warn)
            .with_target("astrs_daemon", LogLevel::Debug);
        assert_eq!(a, b);

        let different_level =
            EnvFilterLite::new(LogLevel::Info).with_target("astrs_cli", LogLevel::Error);
        assert_ne!(a, different_level);
    }

    #[test]
    fn display_is_sorted_by_target_regardless_of_insertion_order() {
        let a = EnvFilterLite::new(LogLevel::Info)
            .with_target("astrs_daemon", LogLevel::Debug)
            .with_target("astrs_cli", LogLevel::Warn);
        let b = EnvFilterLite::new(LogLevel::Info)
            .with_target("astrs_cli", LogLevel::Warn)
            .with_target("astrs_daemon", LogLevel::Debug);
        assert_eq!(a.to_string(), b.to_string());
        assert_eq!(a.to_string(), "info,astrs_cli=warn,astrs_daemon=debug");
    }

    #[test]
    fn display_round_trips_through_from_str() {
        let original = "warn,astrs_daemon=debug,astrs_cli=trace";
        let filter: EnvFilterLite = original.parse().unwrap();
        let rendered = filter.to_string();
        let reparsed: EnvFilterLite = rendered.parse().unwrap();
        assert_eq!(reparsed, filter);
    }

    #[test]
    fn display_with_no_overrides_is_just_the_default_level() {
        assert_eq!(EnvFilterLite::new(LogLevel::Error).to_string(), "error");
    }

    #[test]
    fn bare_level_sets_default_for_every_target() {
        let f: EnvFilterLite = "warn".parse().unwrap();
        assert_eq!(f.default_level(), LogLevel::Warn);
        assert!(f.matches(&rec(LogLevel::Error, "anything::at::all")));
        assert!(!f.matches(&rec(LogLevel::Info, "anything::at::all")));
    }

    #[test]
    fn target_override_wins_over_default() {
        let f: EnvFilterLite = "info,astrs_daemon=debug".parse().unwrap();
        assert!(f.matches(&rec(LogLevel::Debug, "astrs_daemon")));
        assert!(f.matches(&rec(LogLevel::Debug, "astrs_daemon::spawn")));
        assert!(!f.matches(&rec(LogLevel::Debug, "astrs_cli")));
    }

    #[test]
    fn module_path_prefix_does_not_match_unrelated_crate_with_shared_prefix() {
        // "astrs_daemon" must not match "astrs_daemon_extra" (no "::" boundary).
        let f: EnvFilterLite = "error,astrs_daemon=trace".parse().unwrap();
        assert!(!f.matches(&rec(LogLevel::Debug, "astrs_daemon_extra")));
        assert!(f.matches(&rec(LogLevel::Debug, "astrs_daemon")));
    }

    #[test]
    fn longest_matching_target_wins() {
        let f: EnvFilterLite = "error,astrs_daemon=info,astrs_daemon::spawn=trace"
            .parse()
            .unwrap();
        // Falls back to the "astrs_daemon" directive (info), not the crate default (error).
        assert!(f.matches(&rec(LogLevel::Info, "astrs_daemon::routes")));
        assert!(!f.matches(&rec(LogLevel::Debug, "astrs_daemon::routes")));
        // The more specific "astrs_daemon::spawn" directive (trace) wins here.
        assert!(f.matches(&rec(LogLevel::Debug, "astrs_daemon::spawn")));
    }

    #[test]
    fn no_bare_directive_defaults_to_info() {
        let f: EnvFilterLite = "astrs_daemon=trace".parse().unwrap();
        assert_eq!(f.default_level(), LogLevel::Info);
    }

    #[test]
    fn ignores_whitespace_and_empty_segments() {
        let f: EnvFilterLite = " info , astrs_daemon = debug ,, ".parse().unwrap();
        assert_eq!(f.default_level(), LogLevel::Info);
        assert!(f.matches(&rec(LogLevel::Debug, "astrs_daemon")));
    }

    #[test]
    fn rejects_bad_level() {
        let err = "astrs_daemon=verbose".parse::<EnvFilterLite>().unwrap_err();
        assert!(matches!(err, DirectiveError::Level { .. }));
    }

    #[test]
    fn rejects_empty_target() {
        let err = "=debug".parse::<EnvFilterLite>().unwrap_err();
        assert!(matches!(err, DirectiveError::EmptyTarget { .. }));
    }

    #[test]
    fn with_target_replaces_existing_entry() {
        let f = EnvFilterLite::new(LogLevel::Info)
            .with_target("astrs_daemon", LogLevel::Debug)
            .with_target("astrs_daemon", LogLevel::Trace);
        assert!(f.matches(&rec(LogLevel::Debug, "astrs_daemon")));
    }

    /// Pins the documented delimiter constraint (module docs, and
    /// [`EnvFilterLite::with_target`]): a target containing `,` does not
    /// round-trip through `Display`/`FromStr`. Deliberately chosen so the
    /// re-parse *succeeds* with a different, silently wrong filter rather
    /// than erroring — the comma splits `"info,astrs_cli=warn"` into a
    /// bare-level directive (`"info"`, itself a valid level name) plus a
    /// `target=level` pair (`"astrs_cli=warn"`), so nothing here ever
    /// fails loudly; this is the real shape of the risk the docs warn
    /// about, not merely a rejected input. If this test ever starts
    /// asserting equality instead of inequality, the docs above are wrong
    /// and need updating in the same change, not left stale.
    #[test]
    fn a_target_containing_a_comma_silently_misparses_on_round_trip() {
        let f = EnvFilterLite::new(LogLevel::Error).with_target("info,astrs_cli", LogLevel::Warn);
        let rendered = f.to_string();
        assert_eq!(rendered, "error,info,astrs_cli=warn");
        let reparsed: EnvFilterLite = rendered.parse().expect("re-parse succeeds -- silently");
        assert_ne!(
            reparsed, f,
            "a comma in a target string breaks the round trip by design -- \
             see the module-level docs' delimiter constraint"
        );
    }
}
