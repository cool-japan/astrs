//! Duration fields for fault-tolerance timing (blueprint §8.3).
//!
//! `restart_delay` / `max_restart_delay` / `restart_window` /
//! `health_check_timeout` / `finish_grace_secs` all measure time. The
//! blueprint leaves the exact representation to the implementer
//! ("humantime-style strings parsed via your own small parser or plain f64
//! seconds — pick one, document it"). This crate picks **both**, unified
//! behind one type: [`DurationSecs`] accepts a bare YAML number (seconds)
//! *or* a humantime-style string (`"500ms"`, `"1.5s"`, `"2m"`, `"1h"`), and
//! always re-serializes as a plain number of seconds — the canonical form
//! downstream crates (`astrs-daemon`, `astrs-scheduler`) can consume without
//! re-parsing a string.
//!
//! The parser intentionally supports only a **single** `<number><unit>`
//! term (no compound durations like `"1h30m"`) — that is the "small parser"
//! the blueprint asks for; a compound-duration syntax is deferred to
//! `astrs-time`'s own duration parser (§5.2), which this crate does not
//! depend on.

use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A non-negative duration, stored internally as seconds.
///
/// Accepts either a bare number (interpreted as seconds) or a
/// humantime-style string with one of the units `ns`, `us`/`µs`, `ms`, `s`,
/// `m`, `h`. See [`DurationSecs::parse`] for the exact grammar, and this
/// module's top-level docs for why both forms are accepted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DurationSecs(f64);

impl DurationSecs {
    /// Build a duration directly from a seconds value.
    ///
    /// # Errors
    ///
    /// Returns [`DurationParseError::NotFinite`] for NaN/infinite input, or
    /// [`DurationParseError::Negative`] for a negative value.
    pub fn from_secs_f64(secs: f64) -> Result<Self, DurationParseError> {
        if !secs.is_finite() {
            return Err(DurationParseError::NotFinite);
        }
        if secs < 0.0 {
            return Err(DurationParseError::Negative(secs));
        }
        Ok(Self(secs))
    }

    /// The duration in fractional seconds.
    #[must_use]
    pub fn as_secs_f64(self) -> f64 {
        self.0
    }

    /// Parse a humantime-style single-unit duration string, or a bare
    /// number interpreted as seconds.
    ///
    /// The numeric part must match `-?[0-9]+(\.[0-9]+)?` — the same grammar
    /// [`DurationSecs::json_schema`] advertises for the string form (see
    /// this module's private `numeric_prefix` helper for why a leading `+`
    /// or a bare `.5` are deliberately rejected rather than accepted here
    /// and only there).
    ///
    /// # Errors
    ///
    /// Returns [`DurationParseError`] if `s` is empty, has no recognizable
    /// leading number (or an unparsable one), an unrecognized unit suffix,
    /// or resolves to a negative/non-finite value.
    pub fn parse(s: &str) -> Result<Self, DurationParseError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(DurationParseError::Empty);
        }

        let number = numeric_prefix(trimmed);
        if number.is_empty() {
            // Nothing matching `-?[0-9]+(\.[0-9]+)?` at all (a bare sign, a
            // leading-dot decimal, or non-numeric text) — report the whole
            // input rather than an empty numeric part, which would be a
            // useless error message.
            return Err(DurationParseError::InvalidNumber(trimmed.to_string()));
        }
        let unit = &trimmed[number.len()..];

        let value: f64 = number
            .parse()
            .map_err(|_| DurationParseError::InvalidNumber(number.to_string()))?;

        let seconds = if unit.is_empty() {
            value
        } else {
            let factor = unit_factor(unit)
                .ok_or_else(|| DurationParseError::UnknownUnit(unit.to_string()))?;
            value * factor
        };

        Self::from_secs_f64(seconds)
    }
}

/// The longest prefix of `s` matching `-?[0-9]+(\.[0-9]+)?` — the numeric
/// grammar [`DurationSecs::json_schema`] advertises for the string form.
///
/// Returns `""` when `s` has no such prefix at all.
///
/// A leading `+` sign and a bare-decimal `.5` (no integer digit before the
/// point) are deliberately **excluded** from this grammar even though
/// `str::parse::<f64>` would happily accept `"+5"` or `".5"` on its own:
/// accepting them here while the emitted JSON Schema's `pattern` (correctly,
/// per the same grammar) rejects them would mean an editor validating a
/// manifest against `astrs-schema.json` and this parser disagree about what
/// counts as a well-formed duration string — exactly the "config that lies"
/// failure mode blueprint §2.2 exists to eliminate, just relocated from a
/// dead field to a schema/parser split-brain. A leading `-` is still
/// recognized, but only so a syntactically-plausible negative duration
/// fails with the specific, actionable [`DurationParseError::Negative`]
/// rather than the more generic [`DurationParseError::InvalidNumber`]; the
/// schema pattern rejects a leading `-` outright, since no negative
/// duration is ever accepted regardless of how it fails.
fn numeric_prefix(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = if bytes.first() == Some(&b'-') { 1 } else { 0 };
    let integer_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == integer_start {
        // No integer digits at all: a bare sign, a bare `.5`, or garbage —
        // none of these have a recognizable numeric prefix under this
        // grammar.
        return "";
    }
    if bytes.get(end) == Some(&b'.') {
        let mut fraction_end = end + 1;
        while fraction_end < bytes.len() && bytes[fraction_end].is_ascii_digit() {
            fraction_end += 1;
        }
        // Only consume the `.` when at least one fractional digit follows
        // it — `5.` is not a valid number under `-?[0-9]+(\.[0-9]+)?`
        // either, so the bare `.` is left for the unit suffix to (fail to)
        // recognize instead.
        if fraction_end > end + 1 {
            end = fraction_end;
        }
    }
    &s[..end]
}

/// The seconds-per-unit factor for a recognized duration suffix.
///
/// Both common encodings of the micro sign are accepted for `"us"`: U+00B5
/// (MICRO SIGN) and U+03BC (GREEK SMALL LETTER MU) are visually
/// indistinguishable in most fonts and both appear in the wild depending on
/// how a user's editor/IME inserted the character.
fn unit_factor(unit: &str) -> Option<f64> {
    match unit {
        "ns" => Some(1e-9),
        "us" | "\u{b5}s" | "\u{3bc}s" => Some(1e-6),
        "ms" => Some(1e-3),
        "s" => Some(1.0),
        "m" => Some(60.0),
        "h" => Some(3600.0),
        _ => None,
    }
}

impl fmt::Display for DurationSecs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

/// An error raised while parsing a [`DurationSecs`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum DurationParseError {
    /// The input string was empty (after trimming whitespace).
    #[error("duration string is empty")]
    Empty,
    /// The leading numeric portion could not be parsed as an `f64`.
    #[error("`{0}` is not a valid duration number")]
    InvalidNumber(String),
    /// The trailing suffix was not one of `ns`, `us`/`µs`, `ms`, `s`, `m`, `h`.
    #[error("`{0}` is not a recognized duration unit (expected ns, us, ms, s, m, or h)")]
    UnknownUnit(String),
    /// The resolved value was negative.
    #[error("duration must not be negative, found {0}")]
    Negative(f64),
    /// The resolved value was NaN or infinite.
    #[error("duration must be a finite number")]
    NotFinite,
}

impl Serialize for DurationSecs {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for DurationSecs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DurationVisitor;

        impl Visitor<'_> for DurationVisitor {
            type Value = DurationSecs;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a number of seconds, or a humantime-style string like \"500ms\", \"1.5s\", \"2m\", \"1h\"",
                )
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                DurationSecs::from_secs_f64(v).map_err(de::Error::custom)
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                self.visit_f64(v as f64)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                self.visit_f64(v as f64)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                DurationSecs::parse(v).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(DurationVisitor)
    }
}

impl JsonSchema for DurationSecs {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DurationSecs".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A duration: either a plain number of seconds, or a humantime-style string such as \"500ms\", \"1.5s\", \"2m\", \"1h\".",
            "anyOf": [
                { "type": "number", "minimum": 0.0 },
                { "type": "string", "pattern": "^[0-9]+(\\.[0-9]+)?(ns|us|\u{b5}s|ms|s|m|h)?$" },
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_bare_number_as_seconds() {
        assert_eq!(DurationSecs::parse("5").unwrap().as_secs_f64(), 5.0);
        assert_eq!(DurationSecs::parse("2.5").unwrap().as_secs_f64(), 2.5);
    }

    /// `unit_factor` multiplication (e.g. `100.0 * 1e-9`) does not always
    /// land on the closest `f64` to the "obvious" decimal answer, so the
    /// nanosecond/microsecond cases below compare with a tolerance rather
    /// than bit-exact equality.
    fn approx_eq(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-15, "expected {a} ~= {b}");
    }

    #[test]
    fn parses_each_unit() {
        assert_eq!(DurationSecs::parse("500ms").unwrap().as_secs_f64(), 0.5);
        assert_eq!(DurationSecs::parse("1.5s").unwrap().as_secs_f64(), 1.5);
        assert_eq!(DurationSecs::parse("2m").unwrap().as_secs_f64(), 120.0);
        assert_eq!(DurationSecs::parse("1h").unwrap().as_secs_f64(), 3600.0);
        approx_eq(DurationSecs::parse("100ns").unwrap().as_secs_f64(), 1e-7);
        approx_eq(DurationSecs::parse("10us").unwrap().as_secs_f64(), 1e-5);
        approx_eq(DurationSecs::parse("10µs").unwrap().as_secs_f64(), 1e-5);
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(matches!(
            DurationSecs::parse("5fortnights"),
            Err(DurationParseError::UnknownUnit(_))
        ));
    }

    #[test]
    fn rejects_negative() {
        assert!(matches!(
            DurationSecs::parse("-5s"),
            Err(DurationParseError::Negative(_))
        ));
        assert!(matches!(
            DurationSecs::from_secs_f64(-1.0),
            Err(DurationParseError::Negative(_))
        ));
    }

    #[test]
    fn rejects_non_finite() {
        assert!(matches!(
            DurationSecs::from_secs_f64(f64::NAN),
            Err(DurationParseError::NotFinite)
        ));
        assert!(matches!(
            DurationSecs::from_secs_f64(f64::INFINITY),
            Err(DurationParseError::NotFinite)
        ));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            DurationSecs::parse(""),
            Err(DurationParseError::Empty)
        ));
        assert!(matches!(
            DurationSecs::parse("   "),
            Err(DurationParseError::Empty)
        ));
    }

    #[test]
    fn rejects_invalid_number() {
        assert!(matches!(
            DurationSecs::parse("abcms"),
            Err(DurationParseError::InvalidNumber(_))
        ));
    }

    /// Regression test: `str::parse::<f64>` alone accepts a leading `+`
    /// (`"+5".parse::<f64>()` is `Ok(5.0)`), so the old `parse` — which
    /// just sliced off a `[0-9.+-]*` run and handed it to `f64::parse` —
    /// silently accepted `"+5s"` even though the JSON Schema `pattern`
    /// (`^[0-9]+...`) has never allowed a leading `+`. See
    /// [`numeric_prefix`]'s docs for why the two must agree.
    #[test]
    fn rejects_leading_plus_sign() {
        assert!(matches!(
            DurationSecs::parse("+5s"),
            Err(DurationParseError::InvalidNumber(_))
        ));
    }

    /// Regression test: same schema/parser split-brain as
    /// `rejects_leading_plus_sign`, for a bare-decimal number with no
    /// leading integer digit (`".5".parse::<f64>()` is `Ok(0.5)`, but the
    /// schema pattern requires `[0-9]+` before the optional `\.[0-9]+`).
    #[test]
    fn rejects_bare_leading_dot() {
        assert!(matches!(
            DurationSecs::parse(".5s"),
            Err(DurationParseError::InvalidNumber(_))
        ));
    }

    /// A trailing `.` with no fractional digit after it is also outside
    /// `-?[0-9]+(\.[0-9]+)?` — `"5."` must not silently mean `"5"`.
    #[test]
    fn rejects_trailing_dot_with_no_fraction_digits() {
        assert!(matches!(
            DurationSecs::parse("5."),
            Err(DurationParseError::UnknownUnit(_))
        ));
    }

    #[test]
    fn duration_string_grammar_matches_its_own_json_schema_pattern() {
        // A small executable cross-check, rather than only prose, that the
        // parser and the schema's `pattern` accept exactly the same
        // strings for every case this module's tests exercise. Goes
        // through the crate's own `emit_schema()` (rather than poking
        // `schemars` internals directly) so this test exercises exactly
        // the artifact `astrs-schema.json` will ship, via the same
        // `$defs.DurationSecs` entry `tests/schema.rs` checks for.
        let schema: serde_json::Value =
            serde_json::from_str(&crate::emit_schema()).expect("emit_schema must produce JSON");
        let pattern = schema["$defs"]["DurationSecs"]["anyOf"][1]["pattern"]
            .as_str()
            .expect("DurationSecs schema must carry a string-variant `pattern`");
        let re = regex::Regex::new(pattern).expect("schema pattern must itself be valid regex");

        for (input, should_match) in [
            ("5", true),
            ("2.5", true),
            ("500ms", true),
            ("1.5s", true),
            ("2m", true),
            ("1h", true),
            ("100ns", true),
            ("10us", true),
            ("+5s", false),
            (".5s", false),
            ("5.", false),
            ("abcms", false),
        ] {
            assert_eq!(
                re.is_match(input),
                should_match,
                "schema pattern disagreement for {input:?}"
            );
            assert_eq!(
                DurationSecs::parse(input).is_ok(),
                should_match,
                "parser disagreement for {input:?}"
            );
        }
    }

    #[test]
    fn deserializes_from_yaml_number() {
        let d: DurationSecs = astrs_yaml::from_str("5.0").unwrap();
        assert_eq!(d.as_secs_f64(), 5.0);
        let d: DurationSecs = astrs_yaml::from_str("5").unwrap();
        assert_eq!(d.as_secs_f64(), 5.0);
    }

    #[test]
    fn deserializes_from_yaml_string() {
        let d: DurationSecs = astrs_yaml::from_str("\"500ms\"").unwrap();
        assert_eq!(d.as_secs_f64(), 0.5);
    }

    #[test]
    fn serializes_as_plain_number() {
        let d = DurationSecs::from_secs_f64(0.5).unwrap();
        let yaml = astrs_yaml::to_string(&d).unwrap();
        assert_eq!(yaml.trim(), "0.5");
    }

    #[test]
    fn string_and_number_forms_are_semantically_equal_after_round_trip() {
        let from_string: DurationSecs = astrs_yaml::from_str("\"1.5s\"").unwrap();
        let yaml = astrs_yaml::to_string(&from_string).unwrap();
        let from_number: DurationSecs = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(from_string, from_number);
    }
}
