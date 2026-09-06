//! Human-readable duration parsing and formatting (`"250ms"`, `"5s"`,
//! `"1.5h"`, `"100us"`, ...) for manifests, CLI flags, and config.
//!
//! # Grammar
//!
//! A duration string is one or more whitespace-separated `<number><unit>`
//! tokens, summed: `"5s"`, `"1.5h"`, and `"1h30m"` / `"1h 30m 5s"` are all
//! valid. `<number>` is an unsigned integer or decimal literal
//! (`\d+(\.\d+)?`); a leading `-` is rejected with a specific error rather
//! than silently accepted or misparsed. `<unit>` is one of `ns`, `us`
//! (`µs`/`μs` also accepted on input), `ms`, `s`, `m`, `h`, `d` —
//! **lowercase ASCII only**. `"5S"`, `"5MS"`, and `"5H"` are rejected
//! ([`DurationParseError::UnknownUnit`]), not silently accepted as their
//! lowercase equivalents: this is a manifest/config grammar, not a
//! case-insensitive human-typed one, and a single fixed case keeps the
//! grammar's prefix-matching (`"ms"` must not be read as `"m"` then a
//! stray `"S"`) unambiguous without also having to reason about every case
//! permutation.
//!
//! # Exactness
//!
//! Every conversion in [`parse_duration`] is done with exact integer
//! arithmetic — never by round-tripping through `f64` — because a decimal
//! fraction like `0.1` has no exact binary floating-point representation,
//! and this crate exists partly to serve a *determinism* feature (§14 of
//! the blueprint): the same manifest string must parse to the same
//! [`Duration`] on every run, on every platform. A fractional component
//! that cannot be represented in whole nanoseconds is rounded to the
//! nearest nanosecond (round-half-up, computed exactly), not truncated or
//! rejected — sub-nanosecond precision is finer than `Duration` itself can
//! represent, so rejecting it would only be pedantic, not more correct.
//!
//! [`format_duration`] formats a [`Duration`] using the **largest unit
//! that divides it exactly**, favoring exactness over maximal terseness:
//! 5400 seconds formats as `"90m"`, not `"1.5h"`, because 5400 divides
//! evenly by minutes (`60s`) but not by hours (`3600s`). This guarantees
//! `parse_duration(&format_duration(d)) == Ok(d)` for every representable
//! `d` without ever needing a fractional *output* — nanoseconds, the finest
//! unit, always divides evenly, so the algorithm never fails to find a
//! match.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Error returned when [`parse_duration`] or [`HumanDuration::from_str`]
/// fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurationParseError {
    /// The input was empty (or whitespace-only).
    #[error("duration string is empty")]
    Empty,
    /// The input started with `-`. Negative durations are not
    /// representable by [`Duration`] and are rejected explicitly rather
    /// than silently misparsed.
    #[error("negative durations are not supported: {0:?}")]
    Negative(String),
    /// A numeric literal was missing or malformed at the given position.
    #[error("invalid number in duration string: {0:?}")]
    InvalidNumber(String),
    /// A unit suffix did not match any of `ns`, `us`/`µs`/`μs`, `ms`, `s`,
    /// `m`, `h`, `d`.
    #[error("unknown duration unit {0:?} (expected one of: ns, us, ms, s, m, h, d)")]
    UnknownUnit(String),
    /// The value overflowed while converting to nanoseconds, or the total
    /// exceeded what a [`Duration`] can represent.
    #[error("duration value overflowed while parsing {0:?}")]
    Overflow(String),
}

/// Parses one `<number><unit>` token from the start of `input`, returning
/// its value in nanoseconds and the unparsed remainder of `input`.
fn parse_token(input: &str) -> Result<(u128, &str), DurationParseError> {
    if input.starts_with('-') {
        return Err(DurationParseError::Negative(input.to_owned()));
    }
    let mut s = input.strip_prefix('+').unwrap_or(input);

    let int_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if int_end == 0 {
        return Err(DurationParseError::InvalidNumber(input.to_owned()));
    }
    let int_str = &s[..int_end];
    s = &s[int_end..];

    let frac_str = if let Some(after_dot) = s.strip_prefix('.') {
        let frac_end = after_dot
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_dot.len());
        if frac_end == 0 {
            return Err(DurationParseError::InvalidNumber(input.to_owned()));
        }
        s = &after_dot[frac_end..];
        &after_dot[..frac_end]
    } else {
        ""
    };

    // A single optional space between the number and the unit.
    s = s.strip_prefix(' ').unwrap_or(s);

    let (unit_ns, remainder): (u128, &str) = if let Some(r) = s.strip_prefix("ns") {
        (1, r)
    } else if let Some(r) = s
        .strip_prefix("us")
        .or_else(|| s.strip_prefix("µs"))
        .or_else(|| s.strip_prefix("μs"))
    {
        (1_000, r)
    } else if let Some(r) = s.strip_prefix("ms") {
        (1_000_000, r)
    } else if let Some(r) = s.strip_prefix('s') {
        (1_000_000_000, r)
    } else if let Some(r) = s.strip_prefix('m') {
        (60_000_000_000, r)
    } else if let Some(r) = s.strip_prefix('h') {
        (3_600_000_000_000, r)
    } else if let Some(r) = s.strip_prefix('d') {
        (86_400_000_000_000, r)
    } else {
        let word_end = s
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(s.len());
        return Err(DurationParseError::UnknownUnit(s[..word_end].to_owned()));
    };

    let overflow = || DurationParseError::Overflow(input.to_owned());

    let int_part: u128 = int_str.parse().map_err(|_| overflow())?;
    let int_ns = int_part.checked_mul(unit_ns).ok_or_else(overflow)?;

    let total_ns = if frac_str.is_empty() {
        int_ns
    } else {
        let frac_value: u128 = frac_str.parse().map_err(|_| overflow())?;
        let frac_digits = u32::try_from(frac_str.len()).map_err(|_| overflow())?;
        let divisor = 10u128.checked_pow(frac_digits).ok_or_else(overflow)?;
        let numerator = frac_value.checked_mul(unit_ns).ok_or_else(overflow)?;
        // Round to the nearest whole nanosecond (round-half-up), entirely
        // in exact integer arithmetic — see the module docs.
        let half = divisor / 2;
        let rounded_numerator = numerator.checked_add(half).ok_or_else(overflow)?;
        let frac_ns = rounded_numerator / divisor;
        int_ns.checked_add(frac_ns).ok_or_else(overflow)?
    };

    Ok((total_ns, remainder))
}

/// Parses a human-readable duration string.
///
/// See the [module docs](self) for the grammar and the exactness
/// guarantees. Accepts a single token (`"250ms"`) or a whitespace-optional
/// sequence of tokens summed together (`"1h30m"`, `"1h 30m 5s"`).
///
/// # Errors
///
/// Returns [`DurationParseError`] if `s` is empty, contains a negative
/// number, a malformed number, an unrecognized unit, or a value that
/// overflows.
///
/// # Examples
///
/// ```
/// use astrs_time::parse_duration;
/// use std::time::Duration;
/// # fn main() -> Result<(), astrs_time::DurationParseError> {
/// assert_eq!(parse_duration("250ms")?, Duration::from_millis(250));
/// assert_eq!(parse_duration("5s")?, Duration::from_secs(5));
/// assert_eq!(parse_duration("1.5h")?, Duration::from_secs(5_400));
/// assert_eq!(parse_duration("100us")?, Duration::from_micros(100));
/// assert_eq!(parse_duration("1h30m")?, Duration::from_secs(5_400));
/// # Ok(())
/// # }
/// ```
pub fn parse_duration(s: &str) -> Result<Duration, DurationParseError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(DurationParseError::Empty);
    }

    let mut remaining = trimmed;
    let mut total_ns: u128 = 0;
    while !remaining.is_empty() {
        let (token_ns, rest) = parse_token(remaining)?;
        total_ns = total_ns
            .checked_add(token_ns)
            .ok_or_else(|| DurationParseError::Overflow(s.to_owned()))?;
        remaining = rest.trim_start_matches(' ');
    }

    let secs = u64::try_from(total_ns / 1_000_000_000)
        .map_err(|_| DurationParseError::Overflow(s.to_owned()))?;
    // `total_ns % 1_000_000_000` is always in `0..1_000_000_000`, which
    // fits comfortably in a `u32`.
    let nanos = (total_ns % 1_000_000_000) as u32;
    Ok(Duration::new(secs, nanos))
}

/// The units [`format_duration`] tries, largest first. `ns` (the last
/// entry, unit size 1) always divides any nanosecond count exactly, so it
/// is handled as an unconditional fallback below rather than appearing in
/// this table.
const COARSE_UNITS: &[(&str, u128)] = &[
    ("d", 86_400_000_000_000),
    ("h", 3_600_000_000_000),
    ("m", 60_000_000_000),
    ("s", 1_000_000_000),
    ("ms", 1_000_000),
    ("us", 1_000),
];

/// Formats a [`Duration`] as a human-readable string.
///
/// See the [module docs](self) for the "largest exactly-dividing unit"
/// policy and its round-trip guarantee with [`parse_duration`].
///
/// # Examples
///
/// ```
/// use astrs_time::format_duration;
/// use std::time::Duration;
///
/// assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
/// assert_eq!(format_duration(Duration::from_secs(5)), "5s");
/// assert_eq!(format_duration(Duration::from_secs(5_400)), "90m");
/// assert_eq!(format_duration(Duration::ZERO), "0ns");
/// ```
#[must_use]
pub fn format_duration(d: Duration) -> String {
    let total_ns: u128 = d.as_nanos();
    if total_ns == 0 {
        return "0ns".to_owned();
    }
    for &(suffix, unit_ns) in COARSE_UNITS {
        if total_ns.is_multiple_of(unit_ns) {
            return format!("{}{suffix}", total_ns / unit_ns);
        }
    }
    // No coarser unit divided evenly; nanoseconds (implicitly, size 1)
    // always do, so this is the unconditional final case, not a
    // best-effort fallback.
    format!("{total_ns}ns")
}

/// A [`Duration`] newtype implementing [`fmt::Display`] and [`FromStr`]
/// using the human duration grammar ([`parse_duration`]/
/// [`format_duration`]).
///
/// Exists for contexts that need those trait bounds directly — a `clap`
/// value parser, a map key, anywhere generic code asks for `T: Display +
/// FromStr` — without every call site wrapping and unwrapping the free
/// functions by hand.
///
/// # Examples
///
/// ```
/// use astrs_time::HumanDuration;
/// use std::time::Duration;
/// # fn main() -> Result<(), astrs_time::DurationParseError> {
/// let hd: HumanDuration = "1.5h".parse()?;
/// assert_eq!(hd.0, Duration::from_secs(5_400));
/// assert_eq!(hd.to_string(), "90m");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HumanDuration(pub Duration);

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_duration(self.0))
    }
}

impl FromStr for HumanDuration {
    type Err = DurationParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_duration(s).map(HumanDuration)
    }
}

impl From<Duration> for HumanDuration {
    fn from(d: Duration) -> Self {
        Self(d)
    }
}

impl From<HumanDuration> for Duration {
    fn from(h: HumanDuration) -> Self {
        h.0
    }
}

/// `serde(with = "...")` helpers for representing a plain [`Duration`]
/// field as a human duration string (e.g. `"5s"`) — for human-facing
/// formats like the dataflow manifest's YAML (§8 of the blueprint;
/// `health_check_interval` and similar fields, §24.2).
///
/// # Examples
///
/// ```
/// use astrs_time::duration::serde_human;
/// use serde::{Deserialize, Serialize};
/// use std::time::Duration;
///
/// #[derive(Serialize, Deserialize)]
/// struct Config {
///     #[serde(with = "serde_human")]
///     health_check_interval: Duration,
/// }
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let cfg = Config { health_check_interval: Duration::from_secs(5) };
/// let json = serde_json::to_string(&cfg)?;
/// assert_eq!(json, r#"{"health_check_interval":"5s"}"#);
/// let back: Config = serde_json::from_str(&json)?;
/// assert_eq!(back.health_check_interval, Duration::from_secs(5));
/// # Ok(())
/// # }
/// ```
pub mod serde_human {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes `duration` via [`super::format_duration`].
    ///
    /// # Errors
    ///
    /// Never fails; the [`Result`] return type is `serde`'s
    /// `serialize_with` contract, not a real fallible path here.
    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::format_duration(*duration))
    }

    /// Deserializes a [`Duration`] via [`super::parse_duration`].
    ///
    /// # Errors
    ///
    /// Returns a `serde` deserialization error if the string is not a
    /// valid human duration; see [`super::DurationParseError`].
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(deserializer)?;
        super::parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

/// `serde(with = "...")` helpers for an `Option<Duration>` field
/// represented as an optional human duration string (`null`/absent ↔
/// `None`), for manifest fields that are not required.
pub mod serde_human_option {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes `duration` as `null` when `None`, or its human duration
    /// string form (via [`super::format_duration`]) when `Some`.
    ///
    /// # Errors
    ///
    /// Never fails; see [`super::serde_human::serialize`].
    pub fn serialize<S: Serializer>(
        duration: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match duration {
            Some(d) => serializer.serialize_str(&super::format_duration(*d)),
            None => serializer.serialize_none(),
        }
    }

    /// Deserializes an `Option<Duration>` from an optional human duration
    /// string via [`super::parse_duration`].
    ///
    /// # Errors
    ///
    /// Returns a `serde` deserialization error if a present value is not a
    /// valid human duration; see [`super::DurationParseError`].
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        let opt = Option::<String>::deserialize(deserializer)?;
        opt.map(|s| super::parse_duration(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_unit() {
        assert_eq!(parse_duration("7ns").unwrap(), Duration::from_nanos(7));
        assert_eq!(parse_duration("7us").unwrap(), Duration::from_micros(7));
        assert_eq!(parse_duration("7µs").unwrap(), Duration::from_micros(7));
        assert_eq!(parse_duration("7μs").unwrap(), Duration::from_micros(7));
        assert_eq!(parse_duration("7ms").unwrap(), Duration::from_millis(7));
        assert_eq!(parse_duration("7s").unwrap(), Duration::from_secs(7));
        assert_eq!(parse_duration("7m").unwrap(), Duration::from_secs(7 * 60));
        assert_eq!(
            parse_duration("7h").unwrap(),
            Duration::from_secs(7 * 3_600)
        );
        assert_eq!(
            parse_duration("7d").unwrap(),
            Duration::from_secs(7 * 86_400)
        );
    }

    #[test]
    fn parses_task_examples() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("1.5h").unwrap(), Duration::from_secs(5_400));
        assert_eq!(parse_duration("100us").unwrap(), Duration::from_micros(100));
    }

    #[test]
    fn parses_fractional_values_exactly() {
        // 0.1h = 360s exactly; verifies no float-rounding artifact.
        assert_eq!(parse_duration("0.1h").unwrap(), Duration::from_secs(360));
        // 0.001h = 3.6s exactly.
        assert_eq!(
            parse_duration("0.001h").unwrap(),
            Duration::from_millis(3_600)
        );
        assert_eq!(
            parse_duration("1.5s").unwrap(),
            Duration::from_millis(1_500)
        );
        assert_eq!(parse_duration("0.5ms").unwrap(), Duration::from_micros(500));
    }

    #[test]
    fn rounds_inexact_fractions_half_up() {
        // 1 / 3 ms = 333.333... us -> 333333.33... ns -> rounds to 333333ns.
        let parsed = parse_duration("0.333333333333ms").unwrap();
        assert_eq!(parsed, Duration::from_nanos(333_333));
    }

    #[test]
    fn parses_compound_durations() {
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5_400));
        assert_eq!(
            parse_duration("1h 30m 5s").unwrap(),
            Duration::from_secs(5_400 + 5)
        );
        assert_eq!(
            parse_duration("  1h   30m  ").unwrap(),
            Duration::from_secs(5_400)
        );
    }

    #[test]
    fn allows_a_space_between_number_and_unit() {
        assert_eq!(parse_duration("5 s").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(parse_duration(""), Err(DurationParseError::Empty));
        assert_eq!(parse_duration("   "), Err(DurationParseError::Empty));
    }

    #[test]
    fn rejects_negative_input() {
        assert!(matches!(
            parse_duration("-5s"),
            Err(DurationParseError::Negative(_))
        ));
        assert!(matches!(
            parse_duration("1s-2ms"),
            Err(DurationParseError::Negative(_))
        ));
    }

    #[test]
    fn rejects_invalid_numbers() {
        assert!(matches!(
            parse_duration("abc"),
            Err(DurationParseError::InvalidNumber(_))
        ));
        assert!(matches!(
            parse_duration("1.s"),
            Err(DurationParseError::InvalidNumber(_))
        ));
        assert!(matches!(
            parse_duration("5sxyz"),
            Err(DurationParseError::InvalidNumber(_))
        ));
    }

    #[test]
    fn rejects_unknown_units() {
        assert!(matches!(
            parse_duration("5x"),
            Err(DurationParseError::UnknownUnit(_))
        ));
        assert!(matches!(
            parse_duration("5"),
            Err(DurationParseError::UnknownUnit(_))
        ));
    }

    #[test]
    fn does_not_confuse_milliseconds_with_minutes() {
        // "ms" must not be parsed as "m" followed by a stray "s".
        assert_eq!(parse_duration("100ms").unwrap(), Duration::from_millis(100));
    }

    #[test]
    fn units_are_lowercase_ascii_only() {
        // Uppercase units are rejected, not silently treated as their
        // lowercase equivalents — see the module docs' grammar section.
        assert!(matches!(
            parse_duration("5S"),
            Err(DurationParseError::UnknownUnit(_))
        ));
        assert!(matches!(
            parse_duration("5H"),
            Err(DurationParseError::UnknownUnit(_))
        ));
        assert!(matches!(
            parse_duration("5D"),
            Err(DurationParseError::UnknownUnit(_))
        ));
        // "MS" must not be misread as "M" (minutes) plus a stray "S".
        assert!(matches!(
            parse_duration("5MS"),
            Err(DurationParseError::UnknownUnit(_))
        ));
        // Mixed case is equally rejected.
        assert!(matches!(
            parse_duration("5Ms"),
            Err(DurationParseError::UnknownUnit(_))
        ));
    }

    #[test]
    fn overflow_from_a_single_astronomically_large_token() {
        // A 40-digit integer overflows the `checked_mul` by the unit's
        // nanosecond scale well before it could ever reach `Duration`.
        let huge = "9".repeat(40);
        let err = parse_duration(&format!("{huge}d")).unwrap_err();
        assert!(matches!(err, DurationParseError::Overflow(_)));
    }

    #[test]
    fn overflow_from_summing_two_individually_valid_tokens() {
        // Each token alone is comfortably representable (`u128` holds
        // `1e19 * 1e9 = 1e28` with enormous headroom below `u128::MAX`),
        // but their *sum* in seconds (`2e19`) exceeds `u64::MAX`
        // (~1.8446744e19) — the failure is caught at the final
        // `u64::try_from` on total seconds, not at either token's own
        // `checked_mul`, exercising the accumulator overflow path
        // specifically (distinct from the single-token case above).
        let one = "10000000000000000000s"; // 1e19 seconds
        let err = parse_duration(&format!("{one}{one}")).unwrap_err();
        assert!(matches!(err, DurationParseError::Overflow(_)));

        // Each token in isolation still parses fine.
        assert!(parse_duration(one).is_ok());
    }

    #[test]
    fn format_uses_largest_exactly_dividing_unit() {
        assert_eq!(format_duration(Duration::ZERO), "0ns");
        assert_eq!(format_duration(Duration::from_nanos(500)), "500ns");
        assert_eq!(format_duration(Duration::from_micros(100)), "100us");
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        // 5400s divides evenly by minutes (90) but not by hours (1.5).
        assert_eq!(format_duration(Duration::from_secs(5_400)), "90m");
        assert_eq!(format_duration(Duration::from_secs(3_600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(86_400)), "1d");
        // 90 seconds divides evenly by neither minutes nor hours.
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
    }

    #[test]
    fn format_then_parse_round_trips_for_representative_values() {
        for d in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_nanos(500),
            Duration::from_micros(100),
            Duration::from_millis(250),
            Duration::from_secs(5),
            Duration::from_secs(90),
            Duration::from_secs(5_400),
            Duration::from_secs(86_400 * 3),
            Duration::from_nanos(u64::MAX),
        ] {
            let formatted = format_duration(d);
            let reparsed = parse_duration(&formatted).unwrap();
            assert_eq!(reparsed, d, "round trip failed for {d:?} -> {formatted:?}");
        }
    }

    #[test]
    fn human_duration_display_and_from_str() {
        let hd: HumanDuration = "1.5h".parse().unwrap();
        assert_eq!(hd.0, Duration::from_secs(5_400));
        assert_eq!(hd.to_string(), "90m");
        assert_eq!(Duration::from(hd), Duration::from_secs(5_400));
        assert_eq!(
            HumanDuration::from(Duration::from_secs(5)).to_string(),
            "5s"
        );
    }

    #[test]
    fn serde_human_round_trip() {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Config {
            #[serde(with = "serde_human")]
            interval: Duration,
        }

        let cfg = Config {
            interval: Duration::from_millis(250),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, r#"{"interval":"250ms"}"#);
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn serde_human_rejects_invalid_string() {
        #[derive(Debug, serde::Deserialize)]
        struct Config {
            #[serde(with = "serde_human")]
            // The field exists only so `serde_human` has something to
            // deserialize into; the test asserts on the error, never on it.
            #[allow(dead_code)]
            interval: Duration,
        }

        let err = serde_json::from_str::<Config>(r#"{"interval":"nonsense"}"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown duration unit")
                || err.to_string().contains("invalid number")
        );
    }

    #[test]
    fn serde_human_option_round_trip_some_and_none() {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Config {
            #[serde(with = "serde_human_option")]
            interval: Option<Duration>,
        }

        let with_value = Config {
            interval: Some(Duration::from_secs(2)),
        };
        let json = serde_json::to_string(&with_value).unwrap();
        assert_eq!(json, r#"{"interval":"2s"}"#);
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), with_value);

        let without_value = Config { interval: None };
        let json_none = serde_json::to_string(&without_value).unwrap();
        assert_eq!(json_none, r#"{"interval":null}"#);
        assert_eq!(
            serde_json::from_str::<Config>(&json_none).unwrap(),
            without_value
        );
    }
}
