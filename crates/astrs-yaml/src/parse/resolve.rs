//! YAML 1.2 **core schema** resolution: turning a plain scalar's text into
//! `null`, a boolean, an integer, a float, or a string.
//!
//! The core schema is the one YAML 1.2 defines for "typical" documents, and
//! it is deliberately narrower than YAML 1.1's:
//!
//! - `y`, `yes`, `no`, `on`, `off` are **strings**, not booleans. This is
//!   the famous "Norway problem" — `country: NO` means the string `"NO"` —
//!   and getting it right is one of the reasons a manifest parser should not
//!   be a YAML 1.1 parser.
//! - `017` is the **string** `"017"`, not octal 15. Leading zeros disqualify
//!   a decimal integer outright, so a zero-padded id stays text.
//! - Octal is spelled `0o17` and binary `0b101`, the way Rust spells them.
//!
//! Every rule here was checked against `serde_yaml` 0.9 — the parser this
//! crate replaces — so that switching over cannot silently retype a field in
//! somebody's manifest. `tests/oracle.rs` pins each one.

use crate::number::Number;
use crate::value::Value;

/// Outcome of trying to read a scalar as an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntOutcome {
    /// The text is not an integer literal at all.
    NotAnInteger,
    /// The text is an integer literal whose value does not fit in `i64` or
    /// `u64`.
    OutOfRange,
    /// A successfully parsed integer.
    Parsed(Number),
}

/// What a plain scalar resolves to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Resolved {
    /// The scalar's core-schema value.
    Value(Value),
    /// The scalar is an integer literal too large for `i64` or `u64`.
    ///
    /// Reported rather than widened to a float (which would lose digits) or
    /// demoted to a string (which would change the field's type without
    /// saying so). `serde_yaml` refuses the same literals.
    IntegerOutOfRange,
}

/// Resolve a plain (unquoted, untagged) scalar under the core schema.
#[must_use]
pub(crate) fn resolve_plain(text: &str) -> Resolved {
    if let Some(value) = resolve_null(text) {
        return Resolved::Value(value);
    }
    if let Some(value) = resolve_bool(text) {
        return Resolved::Value(Value::Bool(value));
    }
    match parse_integer(text) {
        IntOutcome::Parsed(number) => return Resolved::Value(Value::Number(number)),
        IntOutcome::OutOfRange => return Resolved::IntegerOutOfRange,
        IntOutcome::NotAnInteger => {}
    }
    if let Some(number) = parse_float(text) {
        return Resolved::Value(Value::Number(number));
    }
    Resolved::Value(Value::String(text.to_owned()))
}

/// `null`, `Null`, `NULL`, `~`, or nothing at all.
#[must_use]
pub(crate) fn resolve_null(text: &str) -> Option<Value> {
    matches!(text, "" | "~" | "null" | "Null" | "NULL").then_some(Value::Null)
}

/// `true`/`false` in the three spellings the core schema allows.
///
/// Mixed case such as `tRue` is a string, exactly as `serde_yaml` reads it.
#[must_use]
pub(crate) fn resolve_bool(text: &str) -> Option<bool> {
    match text {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

/// Read `text` as a core-schema integer.
///
/// Accepts an optional `+`/`-` sign followed by a decimal literal with no
/// leading zeros (`0`, `7`, `42`), or a `0x`/`0o`/`0b` radix literal. The
/// radix prefixes are lowercase-only: `0X1f` is a string, matching
/// `serde_yaml`.
#[must_use]
pub(crate) fn parse_integer(text: &str) -> IntOutcome {
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, body) = if let Some(rest) = digits.strip_prefix("0x") {
        (16, rest)
    } else if let Some(rest) = digits.strip_prefix("0o") {
        (8, rest)
    } else if let Some(rest) = digits.strip_prefix("0b") {
        (2, rest)
    } else {
        (10, digits)
    };
    if body.is_empty() || !body.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return IntOutcome::NotAnInteger;
    }
    if radix == 10 && body.len() > 1 && body.starts_with('0') {
        // A leading zero makes it a string: `017` is an id, not octal.
        return IntOutcome::NotAnInteger;
    }
    let Ok(magnitude) = u64::from_str_radix(body, radix) else {
        // Either a digit outside the radix (a string) or an overflow (a
        // genuine out-of-range integer). Telling them apart needs one more
        // look at the digits.
        return if body.bytes().all(|byte| is_digit_in_radix(byte, radix)) {
            IntOutcome::OutOfRange
        } else {
            IntOutcome::NotAnInteger
        };
    };
    if negative {
        // `-0` is the integer zero, and `Number::from(0i64)` normalizes it
        // to the non-negative representation, so `-0 == 0` holds.
        match i64::try_from(magnitude) {
            Ok(value) => IntOutcome::Parsed(Number::from(-value)),
            Err(_) => {
                if magnitude == 1 << 63 {
                    IntOutcome::Parsed(Number::from(i64::MIN))
                } else {
                    IntOutcome::OutOfRange
                }
            }
        }
    } else {
        IntOutcome::Parsed(Number::from(magnitude))
    }
}

fn is_digit_in_radix(byte: u8, radix: u32) -> bool {
    char::from(byte).is_digit(radix)
}

/// Read `text` as a core-schema float.
///
/// Accepts `.inf`/`.Inf`/`.INF` with an optional sign, the unsigned
/// `.nan`/`.NaN`/`.NAN`, and the usual decimal/exponent forms — including
/// the ones with a dot on only one side (`.5`, `5.`) and the ones with no
/// dot at all (`1e3`). A literal whose value overflows to infinity (`1e400`)
/// is **not** a float: it stays a string, which is what `serde_yaml` does
/// and is the only lossless answer available.
#[must_use]
pub(crate) fn parse_float(text: &str) -> Option<Number> {
    if matches!(text, ".nan" | ".NaN" | ".NAN") {
        return Some(Number::from(f64::NAN));
    }
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if matches!(body, ".inf" | ".Inf" | ".INF") {
        return Some(Number::from(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }));
    }
    if !is_decimal_float(body) {
        return None;
    }
    let parsed: f64 = body.parse().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    Some(Number::from(if negative { -parsed } else { parsed }))
}

/// `\d+(\.\d*)?([eE][+-]?\d+)?` or `\.\d+([eE][+-]?\d+)?`, exactly — and
/// with at least one of the dot and the exponent actually present, so a bare
/// run of digits stays an *integer* (or, with a leading zero, a string).
fn is_decimal_float(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut index = 0;
    let integer_digits = count_digits(bytes, &mut index);
    let mut fraction_digits = 0;
    let mut saw_dot = false;
    if bytes.get(index) == Some(&b'.') {
        saw_dot = true;
        index += 1;
        fraction_digits = count_digits(bytes, &mut index);
    }
    if integer_digits == 0 && fraction_digits == 0 {
        return false;
    }
    if integer_digits == 0 && !saw_dot {
        return false;
    }
    let mut saw_exponent = false;
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        saw_exponent = true;
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        if count_digits(bytes, &mut index) == 0 {
            return false;
        }
    }
    if !saw_dot && !saw_exponent {
        return false;
    }
    index == bytes.len()
}

fn count_digits(bytes: &[u8], index: &mut usize) -> usize {
    let start = *index;
    while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        *index += 1;
    }
    *index - start
}

/// True when `text` would resolve to something other than a string, and
/// therefore has to be quoted when emitted.
///
/// The second clause mirrors `serde_yaml`'s `digits_but_not_number`: a
/// zero-padded run of digits resolves to a *string*, but emitting it plainly
/// looks so much like a number that both parsers quote it anyway. Matching
/// that keeps generated manifests byte-identical across the switchover.
#[must_use]
pub(crate) fn needs_quoting_to_stay_a_string(text: &str) -> bool {
    if !matches!(resolve_plain(text), Resolved::Value(Value::String(_))) {
        return true;
    }
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    digits.len() > 1 && digits.starts_with('0') && digits.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn resolved(text: &str) -> Value {
        match resolve_plain(text) {
            Resolved::Value(value) => value,
            Resolved::IntegerOutOfRange => panic!("{text} overflowed"),
        }
    }

    #[test]
    fn the_null_spellings_are_exactly_four_plus_empty() {
        for text in ["", "~", "null", "Null", "NULL"] {
            assert_eq!(resolved(text), Value::Null, "{text}");
        }
        assert_eq!(resolved("nUll"), Value::from("nUll"));
        assert_eq!(resolved("NuLL"), Value::from("NuLL"));
    }

    #[test]
    fn only_true_and_false_are_booleans() {
        for text in ["true", "True", "TRUE"] {
            assert_eq!(resolved(text), Value::Bool(true), "{text}");
        }
        for text in ["false", "False", "FALSE"] {
            assert_eq!(resolved(text), Value::Bool(false), "{text}");
        }
        // The Norway problem: none of these is a boolean under core schema.
        for text in [
            "y", "Y", "yes", "Yes", "YES", "n", "N", "no", "No", "on", "off", "tRue",
        ] {
            assert_eq!(resolved(text), Value::from(text), "{text}");
        }
    }

    #[test]
    fn decimal_integers_reject_leading_zeros() {
        assert_eq!(resolved("0"), Value::from(0u64));
        assert_eq!(resolved("42"), Value::from(42u64));
        assert_eq!(resolved("+1"), Value::from(1u64));
        assert_eq!(resolved("-1"), Value::from(-1i64));
        assert_eq!(resolved("-0"), Value::from(0u64));
        for text in ["017", "00", "-00", "+00", "0123"] {
            assert_eq!(resolved(text), Value::from(text), "{text}");
        }
    }

    #[test]
    fn radix_prefixes_are_lowercase_only() {
        assert_eq!(resolved("0x1F"), Value::from(31u64));
        assert_eq!(resolved("0o17"), Value::from(15u64));
        assert_eq!(resolved("0b101"), Value::from(5u64));
        assert_eq!(resolved("-0x1F"), Value::from(-31i64));
        assert_eq!(resolved("+0o17"), Value::from(15u64));
        assert_eq!(resolved("-0b101"), Value::from(-5i64));
        for text in [
            "0X1f", "0O17", "0B101", "0x", "0o", "0b", "0o8", "0b2", "0xzz",
        ] {
            assert_eq!(resolved(text), Value::from(text), "{text}");
        }
    }

    #[test]
    fn integer_boundaries_are_exact() {
        assert_eq!(resolved("18446744073709551615"), Value::from(u64::MAX));
        assert_eq!(resolved("-9223372036854775808"), Value::from(i64::MIN));
        assert_eq!(resolved("9223372036854775808"), Value::from(1u64 << 63));
        assert_eq!(
            parse_integer("18446744073709551616"),
            IntOutcome::OutOfRange
        );
        assert_eq!(
            parse_integer("-9223372036854775809"),
            IntOutcome::OutOfRange
        );
        assert_eq!(parse_integer("0xFFFFFFFFFFFFFFFFF"), IntOutcome::OutOfRange);
        assert_eq!(parse_integer("x"), IntOutcome::NotAnInteger);
        assert_eq!(parse_integer(""), IntOutcome::NotAnInteger);
        assert_eq!(parse_integer("-"), IntOutcome::NotAnInteger);
    }

    #[test]
    fn floats_accept_a_dot_on_either_side_or_none() {
        for (text, expected) in [
            ("1e3", 1000.0),
            ("1E3", 1000.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("+.5", 0.5),
            ("-1.", -1.0),
            ("1.0e+3", 1000.0),
            ("0.5e-3", 0.0005),
            ("3.75", 3.75),
            ("0.0", 0.0),
            ("5e-324", 5e-324),
        ] {
            assert_eq!(resolved(text), Value::from(expected), "{text}");
        }
        assert_eq!(resolved("-0.0").as_f64(), Some(-0.0));
        assert!(
            resolved("-0.0")
                .as_f64()
                .is_some_and(|f| f.is_sign_negative())
        );
    }

    #[test]
    fn infinities_and_nan_use_the_core_spellings() {
        for text in [".inf", ".Inf", ".INF", "+.inf"] {
            assert_eq!(resolved(text).as_f64(), Some(f64::INFINITY), "{text}");
        }
        assert_eq!(resolved("-.inf").as_f64(), Some(f64::NEG_INFINITY));
        for text in [".nan", ".NaN", ".NAN"] {
            assert!(resolved(text).as_f64().is_some_and(f64::is_nan), "{text}");
        }
        for text in ["inf", "nan", "Infinity", "-.nan", "+.nan"] {
            assert_eq!(resolved(text), Value::from(text), "{text}");
        }
    }

    #[test]
    fn overflowing_floats_stay_strings() {
        for text in ["1e400", "1.7976931348623157e309", "-1e400"] {
            assert_eq!(resolved(text), Value::from(text), "{text}");
        }
    }

    #[test]
    fn near_misses_stay_strings() {
        for text in [
            ".", "..", "e5", "1e", "1e+", ".e5", "1.2.3", "1_000", "1:30", "-", "1 2",
        ] {
            assert_eq!(resolved(text), Value::from(text), "{text}");
        }
    }

    #[test]
    fn quoting_is_needed_for_anything_that_would_re_resolve() {
        for text in [
            "true", "null", "~", "1", "1.0", ".inf", ".nan", "0x1f", "0o17", "0b1", "1e3", "00",
            "017", "-00", "",
        ] {
            assert!(needs_quoting_to_stay_a_string(text), "{text}");
        }
        for text in ["y", "on", "1_0", "hello", "1.2.3", "0", "a"] {
            assert_eq!(needs_quoting_to_stay_a_string(text), text == "0", "{text}");
        }
    }
}
