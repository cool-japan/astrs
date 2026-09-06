//! Virtual input sources (blueprint §8.4).
//!
//! Every node input `source:` is either a `node/output` reference (resolved
//! against declared nodes by [`crate::Manifest::validate`]) or one of a
//! small, fixed family of synthetic sources the daemon/scheduler
//! manufacture without a producing node:
//!
//! - `astrs/timer/millis/N`, `astrs/timer/secs/N`, `astrs/timer/hz/N` (N > 0)
//! - `astrs/logs[/level[/node]]`
//! - `astrs/status`
//!
//! This module only *recognizes and validates the shape* of these strings;
//! it does not schedule timers or filter logs (that is `astrs-scheduler`
//! and `astrs-log`, per the crate catalog, §5.2). It does, however, hand
//! back a *parsed* [`VirtualSource`] rather than a bare pass/fail — see
//! [`recognize`] and [`Node::virtual_inputs`](crate::Node::virtual_inputs)
//! — so those downstream crates parse an `astrs/timer/hz/50` string
//! exactly once, here, rather than each re-deriving the same grammar.

use crate::DurationSecs;

/// The recognized levels for the optional `/level` segment of
/// `astrs/logs[/level[/node]]`, matching [`crate::node::LogLevel`]'s wire
/// spelling. Kept as a local constant (rather than reusing `LogLevel`
/// directly) so this module has no dependency on the `node` module.
const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

/// A recognized virtual input source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualSource {
    /// `astrs/timer/millis/N` — fires every `N` milliseconds.
    TimerMillis(u64),
    /// `astrs/timer/secs/N` — fires every `N` seconds.
    TimerSecs(u64),
    /// `astrs/timer/hz/N` — fires `N` times per second.
    TimerHz(u64),
    /// `astrs/logs[/level[/node]]` — the structured log fan-out, optionally
    /// filtered by minimum level and/or emitting node id.
    Logs {
        /// The optional `/level` filter segment.
        level: Option<String>,
        /// The optional `/node` filter segment (only meaningful when
        /// `level` is also present, per the `astrs/logs[/level[/node]]`
        /// grammar).
        node: Option<String>,
    },
    /// `astrs/status` — node lifecycle events (restarts, failures) as an
    /// ordinary input.
    Status,
}

impl VirtualSource {
    /// The synthetic firing period of a timer variant, converted to
    /// seconds — `None` for [`VirtualSource::Logs`]/[`VirtualSource::Status`],
    /// which are event-driven rather than periodic.
    ///
    /// `TimerHz(n)` converts to `1/n` seconds; `recognize` guarantees
    /// `n > 0` for every constructed [`VirtualSource::TimerHz`], so this
    /// never divides by zero. All three timer kinds are guaranteed
    /// representable as a non-negative finite `f64` for any `u64` input
    /// (the smallest possible period, `1 / u64::MAX` Hz, is still far
    /// above `f64`'s smallest positive normal value), so this always
    /// returns `Some` for a timer variant in practice — it returns
    /// `DurationSecs`'s `Option` as-is rather than asserting that, keeping
    /// this function panic-free regardless.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::recognize_virtual_source;
    ///
    /// fn period_secs(s: &str) -> Option<f64> {
    ///     let source = recognize_virtual_source(s)?.ok()?;
    ///     Some(source.period()?.as_secs_f64())
    /// }
    ///
    /// assert_eq!(period_secs("astrs/timer/hz/50"), Some(0.02));
    /// assert_eq!(period_secs("astrs/timer/secs/2"), Some(2.0));
    /// assert_eq!(period_secs("astrs/status"), None); // event-driven, no period
    /// ```
    #[must_use]
    pub fn period(&self) -> Option<DurationSecs> {
        match self {
            Self::TimerMillis(n) => DurationSecs::from_secs_f64(*n as f64 / 1000.0).ok(),
            Self::TimerSecs(n) => DurationSecs::from_secs_f64(*n as f64).ok(),
            Self::TimerHz(n) => DurationSecs::from_secs_f64(1.0 / (*n as f64)).ok(),
            Self::Logs { .. } | Self::Status => None,
        }
    }
}

/// An error raised while recognizing a string starting with `astrs/` as a
/// virtual source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VirtualSourceError {
    /// The segment after `astrs/` is not `timer`, `logs`, or `status`.
    #[error("unknown virtual source family (expected astrs/timer/*, astrs/logs*, or astrs/status)")]
    UnknownFamily,
    /// `astrs/timer/...` was malformed.
    #[error("{0}")]
    InvalidTimer(String),
    /// `astrs/logs...` was malformed.
    #[error("{0}")]
    InvalidLogs(String),
    /// `astrs/status` had unexpected trailing path segments.
    #[error("{0}")]
    InvalidStatus(String),
}

/// Recognize `source` as a virtual source, if it starts with the `astrs/`
/// prefix.
///
/// Returns `None` when `source` does not start with `astrs/` at all — the
/// caller should then treat it as an ordinary `node/output` reference.
/// Returns `Some(Err(_))` when it starts with `astrs/` but does not match
/// any known virtual-source grammar (so callers can tell "not a virtual
/// source" apart from "an invalid attempt at one").
#[must_use]
pub fn recognize(source: &str) -> Option<Result<VirtualSource, VirtualSourceError>> {
    let rest = source.strip_prefix("astrs/")?;
    Some(recognize_rest(rest))
}

fn recognize_rest(rest: &str) -> Result<VirtualSource, VirtualSourceError> {
    let mut segments = rest.split('/');
    match segments.next() {
        Some("timer") => recognize_timer(&mut segments),
        Some("logs") => recognize_logs(&mut segments),
        Some("status") => recognize_status(&mut segments),
        _ => Err(VirtualSourceError::UnknownFamily),
    }
}

fn recognize_timer<'a>(
    segments: &mut impl Iterator<Item = &'a str>,
) -> Result<VirtualSource, VirtualSourceError> {
    let kind = segments.next().ok_or_else(|| {
        VirtualSourceError::InvalidTimer(
            "astrs/timer/ requires a kind (millis, secs, or hz) and a period N".to_string(),
        )
    })?;
    let n_str = segments.next().ok_or_else(|| {
        VirtualSourceError::InvalidTimer(format!(
            "astrs/timer/{kind}/ requires a period N, e.g. astrs/timer/{kind}/50"
        ))
    })?;
    if segments.next().is_some() {
        return Err(VirtualSourceError::InvalidTimer(format!(
            "astrs/timer/{kind}/{n_str} has unexpected trailing path segments"
        )));
    }
    let n: u64 = n_str.parse().map_err(|_| {
        VirtualSourceError::InvalidTimer(format!(
            "`{n_str}` in astrs/timer/{kind}/{n_str} is not a positive integer"
        ))
    })?;
    if n == 0 {
        return Err(VirtualSourceError::InvalidTimer(format!(
            "astrs/timer/{kind}/0 is invalid: N must be > 0"
        )));
    }
    match kind {
        "millis" => Ok(VirtualSource::TimerMillis(n)),
        "secs" => Ok(VirtualSource::TimerSecs(n)),
        "hz" => Ok(VirtualSource::TimerHz(n)),
        other => Err(VirtualSourceError::InvalidTimer(format!(
            "`{other}` is not a recognized timer kind (expected millis, secs, or hz)"
        ))),
    }
}

fn recognize_logs<'a>(
    segments: &mut impl Iterator<Item = &'a str>,
) -> Result<VirtualSource, VirtualSourceError> {
    let level = segments.next().map(str::to_string);
    let node = segments.next().map(str::to_string);
    if segments.next().is_some() {
        return Err(VirtualSourceError::InvalidLogs(
            "astrs/logs accepts at most two path segments: astrs/logs[/level[/node]]".to_string(),
        ));
    }
    if let Some(level) = &level
        && !LOG_LEVELS.contains(&level.as_str())
    {
        return Err(VirtualSourceError::InvalidLogs(format!(
            "`{level}` is not a recognized log level (expected one of {LOG_LEVELS:?})"
        )));
    }
    Ok(VirtualSource::Logs { level, node })
}

fn recognize_status<'a>(
    segments: &mut impl Iterator<Item = &'a str>,
) -> Result<VirtualSource, VirtualSourceError> {
    if let Some(extra) = segments.next() {
        return Err(VirtualSourceError::InvalidStatus(format!(
            "astrs/status takes no further path segments, found `{extra}`"
        )));
    }
    Ok(VirtualSource::Status)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn non_virtual_sources_return_none() {
        assert_eq!(recognize("camera/frames"), None);
        assert_eq!(recognize("detector/detections"), None);
    }

    #[test]
    fn period_converts_millis_secs_hz_to_seconds() {
        assert_eq!(
            VirtualSource::TimerMillis(500)
                .period()
                .unwrap()
                .as_secs_f64(),
            0.5
        );
        assert_eq!(
            VirtualSource::TimerSecs(2).period().unwrap().as_secs_f64(),
            2.0
        );
        assert_eq!(
            VirtualSource::TimerHz(50).period().unwrap().as_secs_f64(),
            0.02
        );
    }

    #[test]
    fn period_handles_hz_edge_cases() {
        // hz/1 is the slowest legal rate: exactly one second per tick.
        assert_eq!(
            VirtualSource::TimerHz(1).period().unwrap().as_secs_f64(),
            1.0
        );
        // A very large hz still yields a tiny but positive, finite period —
        // never zero (which would imply an infinite rate) and never NaN.
        let tiny = VirtualSource::TimerHz(1_000_000_000)
            .period()
            .unwrap()
            .as_secs_f64();
        assert!(tiny > 0.0 && tiny.is_finite());
        // u64::MAX is the extreme case the type system allows, even though
        // no real timer would ever request it.
        let extreme = VirtualSource::TimerHz(u64::MAX)
            .period()
            .unwrap()
            .as_secs_f64();
        assert!(extreme > 0.0 && extreme.is_finite());
    }

    #[test]
    fn period_is_none_for_non_periodic_variants() {
        assert!(VirtualSource::Status.period().is_none());
        assert!(
            VirtualSource::Logs {
                level: None,
                node: None
            }
            .period()
            .is_none()
        );
    }

    #[test]
    fn recognize_then_period_matches_the_source_string() {
        let Some(Ok(source)) = recognize("astrs/timer/hz/50") else {
            panic!("expected astrs/timer/hz/50 to recognize as a valid virtual source");
        };
        assert_eq!(source.period().unwrap().as_secs_f64(), 0.02);
    }

    #[test]
    fn recognizes_timer_variants() {
        assert_eq!(
            recognize("astrs/timer/millis/10"),
            Some(Ok(VirtualSource::TimerMillis(10)))
        );
        assert_eq!(
            recognize("astrs/timer/secs/2"),
            Some(Ok(VirtualSource::TimerSecs(2)))
        );
        assert_eq!(
            recognize("astrs/timer/hz/50"),
            Some(Ok(VirtualSource::TimerHz(50)))
        );
    }

    #[test]
    fn timer_period_must_be_positive() {
        assert!(matches!(
            recognize("astrs/timer/hz/0"),
            Some(Err(VirtualSourceError::InvalidTimer(_)))
        ));
    }

    #[test]
    fn timer_period_must_be_a_positive_integer() {
        assert!(matches!(
            recognize("astrs/timer/hz/-5"),
            Some(Err(VirtualSourceError::InvalidTimer(_)))
        ));
        assert!(matches!(
            recognize("astrs/timer/hz/abc"),
            Some(Err(VirtualSourceError::InvalidTimer(_)))
        ));
    }

    #[test]
    fn timer_kind_must_be_known() {
        assert!(matches!(
            recognize("astrs/timer/fortnights/1"),
            Some(Err(VirtualSourceError::InvalidTimer(_)))
        ));
    }

    #[test]
    fn timer_requires_period() {
        assert!(matches!(
            recognize("astrs/timer/hz"),
            Some(Err(VirtualSourceError::InvalidTimer(_)))
        ));
        assert!(matches!(
            recognize("astrs/timer"),
            Some(Err(VirtualSourceError::InvalidTimer(_)))
        ));
    }

    #[test]
    fn recognizes_bare_logs() {
        assert_eq!(
            recognize("astrs/logs"),
            Some(Ok(VirtualSource::Logs {
                level: None,
                node: None
            }))
        );
    }

    #[test]
    fn recognizes_logs_with_level() {
        assert_eq!(
            recognize("astrs/logs/error"),
            Some(Ok(VirtualSource::Logs {
                level: Some("error".to_string()),
                node: None
            }))
        );
    }

    #[test]
    fn recognizes_logs_with_level_and_node() {
        assert_eq!(
            recognize("astrs/logs/warn/detector"),
            Some(Ok(VirtualSource::Logs {
                level: Some("warn".to_string()),
                node: Some("detector".to_string())
            }))
        );
    }

    #[test]
    fn rejects_unknown_log_level() {
        assert!(matches!(
            recognize("astrs/logs/critical"),
            Some(Err(VirtualSourceError::InvalidLogs(_)))
        ));
    }

    #[test]
    fn rejects_logs_with_too_many_segments() {
        assert!(matches!(
            recognize("astrs/logs/warn/detector/extra"),
            Some(Err(VirtualSourceError::InvalidLogs(_)))
        ));
    }

    #[test]
    fn recognizes_status() {
        assert_eq!(recognize("astrs/status"), Some(Ok(VirtualSource::Status)));
    }

    #[test]
    fn rejects_status_with_extra_segments() {
        assert!(matches!(
            recognize("astrs/status/extra"),
            Some(Err(VirtualSourceError::InvalidStatus(_)))
        ));
    }

    #[test]
    fn rejects_unknown_family() {
        assert!(matches!(
            recognize("astrs/bogus"),
            Some(Err(VirtualSourceError::UnknownFamily))
        ));
        assert!(matches!(
            recognize("astrs/"),
            Some(Err(VirtualSourceError::UnknownFamily))
        ));
    }
}
