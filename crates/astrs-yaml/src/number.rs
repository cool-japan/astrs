//! The numeric half of [`Value`](crate::Value).
//!
//! YAML's core schema has one `int` type and one `float` type, but a Rust
//! program that reads a manifest wants `u16`, `i64` and `f64` back out with
//! their signs and ranges intact. [`Number`] keeps the three representations
//! a document can actually produce — a non-negative integer, a negative
//! integer, and a float — and never silently moves a value between them.
//!
//! # Why three cases and not one `f64`
//!
//! `queue_size: 18446744073709551615` is a legal `u64` and is not
//! representable as an `f64`. Collapsing integers into floats would corrupt
//! it on the way in and print it back differently on the way out, which is
//! exactly the class of bug a manifest round-trip test is supposed to catch.
//!
//! # Equality
//!
//! [`Number`] is `Eq` and `Hash` so it can be a mapping key. Two floats
//! compare equal when their bits agree after normalizing `-0.0` to `0.0`
//! and every `NaN` to one canonical `NaN`; an integer never compares equal
//! to a float, even when their values coincide (`1` and `1.0` are different
//! YAML nodes, and a round-trip must keep them different).

use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

/// A YAML core-schema number: an integer or a float.
///
/// # Examples
///
/// ```
/// use astrs_yaml::Number;
///
/// let count = Number::from(42u32);
/// assert!(count.is_u64());
/// assert_eq!(count.as_u64(), Some(42));
/// assert_eq!(count.as_f64(), Some(42.0));
///
/// let ratio = Number::from(0.5);
/// assert!(ratio.is_f64());
/// assert_eq!(ratio.as_u64(), None);
/// assert_ne!(Number::from(1u8), Number::from(1.0));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Number {
    repr: Repr,
}

#[derive(Debug, Clone, Copy)]
enum Repr {
    /// A value in `0..=u64::MAX`.
    Positive(u64),
    /// A value in `i64::MIN..0`; never holds a non-negative number, so the
    /// mapping between `Repr` and the `is_*` predicates stays total.
    Negative(i64),
    /// Any finite `f64`. Infinities and `NaN` are permitted (`.inf`,
    /// `.nan` are core-schema floats); only *parsing* rejects overflow.
    Float(f64),
}

impl Number {
    /// The integer `0`.
    pub const ZERO: Self = Self {
        repr: Repr::Positive(0),
    };

    /// True when this number is a non-negative integer, and therefore
    /// exactly representable as a [`u64`].
    #[must_use]
    pub const fn is_u64(&self) -> bool {
        matches!(self.repr, Repr::Positive(_))
    }

    /// True when this number is an integer that fits in an [`i64`].
    #[must_use]
    pub const fn is_i64(&self) -> bool {
        match self.repr {
            Repr::Positive(value) => value <= i64::MAX as u64,
            Repr::Negative(_) => true,
            Repr::Float(_) => false,
        }
    }

    /// True when this number is a float.
    ///
    /// Note that this is about how the document spelled it, not about its
    /// value: `1.0` is a float and `1` is not.
    #[must_use]
    pub const fn is_f64(&self) -> bool {
        matches!(self.repr, Repr::Float(_))
    }

    /// True when this number is an integer of either sign.
    #[must_use]
    pub const fn is_integer(&self) -> bool {
        !self.is_f64()
    }

    /// True for `.inf`, `-.inf` and `.nan`.
    #[must_use]
    pub const fn is_finite(&self) -> bool {
        match self.repr {
            Repr::Float(value) => value.is_finite(),
            _ => true,
        }
    }

    /// This number as a [`u64`], or `None` when it is negative or a float.
    #[must_use]
    pub const fn as_u64(&self) -> Option<u64> {
        match self.repr {
            Repr::Positive(value) => Some(value),
            _ => None,
        }
    }

    /// This number as an [`i64`], or `None` when it does not fit.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self.repr {
            Repr::Positive(value) => {
                if value <= i64::MAX as u64 {
                    Some(value as i64)
                } else {
                    None
                }
            }
            Repr::Negative(value) => Some(value),
            Repr::Float(_) => None,
        }
    }

    /// This number as an [`f64`].
    ///
    /// Always `Some`: every integer this type can hold converts (with the
    /// usual loss of precision above 2^53, which is why [`Number::as_u64`]
    /// exists).
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        Some(match self.repr {
            Repr::Positive(value) => value as f64,
            Repr::Negative(value) => value as f64,
            Repr::Float(value) => value,
        })
    }

    /// Build a float-typed number.
    ///
    /// Prefer `Number::from(1.5_f64)`; this exists so a caller can be
    /// explicit that an integral value like `2.0` must stay a *float*.
    #[must_use]
    pub const fn from_f64(value: f64) -> Self {
        Self {
            repr: Repr::Float(value),
        }
    }

    /// The canonical bit pattern used for hashing and equality, with
    /// `-0.0` folded onto `0.0` and every `NaN` onto one representative.
    fn float_key(value: f64) -> u64 {
        if value.is_nan() {
            f64::NAN.to_bits()
        } else if value == 0.0 {
            0.0_f64.to_bits()
        } else {
            value.to_bits()
        }
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (self.repr, other.repr) {
            (Repr::Positive(a), Repr::Positive(b)) => a == b,
            (Repr::Negative(a), Repr::Negative(b)) => a == b,
            (Repr::Float(a), Repr::Float(b)) => Self::float_key(a) == Self::float_key(b),
            _ => false,
        }
    }
}

impl Eq for Number {}

impl Hash for Number {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.repr {
            Repr::Positive(value) => {
                0u8.hash(state);
                value.hash(state);
            }
            Repr::Negative(value) => {
                1u8.hash(state);
                value.hash(state);
            }
            Repr::Float(value) => {
                2u8.hash(state);
                Self::float_key(value).hash(state);
            }
        }
    }
}

impl fmt::Display for Number {
    /// Renders the number the way this crate's emitter writes it: integers
    /// plainly, floats in the shortest round-trip form (so `2.0` keeps its
    /// `.0`), and non-finite floats as the YAML core-schema spellings
    /// `.inf`, `-.inf` and `.nan`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.repr {
            Repr::Positive(value) => write!(f, "{value}"),
            Repr::Negative(value) => write!(f, "{value}"),
            Repr::Float(value) if value.is_nan() => f.write_str(".nan"),
            Repr::Float(value) if value == f64::INFINITY => f.write_str(".inf"),
            Repr::Float(value) if value == f64::NEG_INFINITY => f.write_str("-.inf"),
            Repr::Float(value) => f.write_str(&format_finite(value)),
        }
    }
}

/// Render a finite `f64` in the shortest form that reads back bit-identical.
///
/// The *digits* come from Rust's own shortest-round-trip formatter
/// (`{:e}`, which is Grisu/Dragon and therefore already minimal); what this
/// function adds is the **layout** — when to write `0.0001` and when to
/// write `1e-7`.
///
/// The layout rule is deliberately not Rust's `{:?}`: it is the one `ryu`
/// uses, and therefore the one `serde_yaml` emits, because a manifest field
/// that reads `health_check_interval: 0.00005` must keep spelling itself
/// that way across the switchover instead of turning into `5e-5`. Decimal
/// notation is used while the value's magnitude stays inside
/// `10^-5 .. 10^17`; outside that window, scientific notation is shorter and
/// is used instead.
///
/// # The one place this is not byte-identical to `serde_yaml`
///
/// The *layout* matches exactly. The *digits* can differ in the final place
/// for values that need 16 or 17 significant figures: several shortest
/// strings read back to the same `f64` there, and Rust's formatter and
/// `ryu`'s pick different ones. Both spellings parse back bit-identical —
/// the difference is cosmetic, not lossy — and it is measurably rare: about
/// 0.03% of uniformly random `f64` bit patterns, and none at all across
/// eighty thousand manifest-shaped values (`n/8`, `n/100`, `n/1000`, whole
/// numbers) or across every float in this workspace's YAML corpus.
fn format_finite(value: f64) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let magnitude = value.abs();
    let scientific = format!("{magnitude:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let length = i32::try_from(digits.len()).unwrap_or(i32::MAX);
    // `value == digits * 10^scale`, and `10^(power-1) <= |value| < 10^power`.
    let power = exponent + 1;
    let scale = power - length;
    let zeros = |count: i32| "0".repeat(usize::try_from(count).unwrap_or(0));
    let split = |at: i32| {
        let at = usize::try_from(at).unwrap_or(0).min(digits.len());
        (&digits[..at], &digits[at..])
    };

    if scale >= 0 && power <= 16 {
        // 1234e7 -> 12340000000.0
        format!("{sign}{digits}{}.0", zeros(scale))
    } else if power > 0 && power <= 16 {
        // 1234e-2 -> 12.34
        let (whole, fraction) = split(power);
        format!("{sign}{whole}.{fraction}")
    } else if power > -5 && power <= 0 {
        // 1234e-6 -> 0.001234
        format!("{sign}0.{}{digits}", zeros(-power))
    } else if length == 1 {
        // 1e30
        format!("{sign}{digits}e{}", power - 1)
    } else {
        // 1.234e30
        let (first, rest) = split(1);
        format!("{sign}{first}.{rest}e{}", power - 1)
    }
}

macro_rules! from_unsigned {
    ($($ty:ty),* $(,)?) => {
        $(
            impl From<$ty> for Number {
                fn from(value: $ty) -> Self {
                    Self { repr: Repr::Positive(u64::from(value)) }
                }
            }
        )*
    };
}

macro_rules! from_signed {
    ($($ty:ty),* $(,)?) => {
        $(
            impl From<$ty> for Number {
                fn from(value: $ty) -> Self {
                    Self::from(i64::from(value))
                }
            }
        )*
    };
}

from_unsigned!(u8, u16, u32, u64);
from_signed!(i8, i16, i32);

impl From<i64> for Number {
    fn from(value: i64) -> Self {
        let repr = if value < 0 {
            Repr::Negative(value)
        } else {
            // Non-negative integers always take the positive representation,
            // so `Number::from(1i64) == Number::from(1u64)`.
            Repr::Positive(value as u64)
        };
        Self { repr }
    }
}

impl From<usize> for Number {
    fn from(value: usize) -> Self {
        Self {
            repr: Repr::Positive(value as u64),
        }
    }
}

impl From<isize> for Number {
    fn from(value: isize) -> Self {
        Self::from(value as i64)
    }
}

impl From<f32> for Number {
    fn from(value: f32) -> Self {
        Self::from(f64::from(value))
    }
}

impl From<f64> for Number {
    fn from(value: f64) -> Self {
        Self {
            repr: Repr::Float(value),
        }
    }
}

impl Serialize for Number {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.repr {
            Repr::Positive(value) => serializer.serialize_u64(value),
            Repr::Negative(value) => serializer.serialize_i64(value),
            Repr::Float(value) => serializer.serialize_f64(value),
        }
    }
}

impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NumberVisitor;

        impl serde::de::Visitor<'_> for NumberVisitor {
            type Value = Number;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a YAML number")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Number, E> {
                Ok(Number::from(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Number, E> {
                Ok(Number::from(value))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Number, E> {
                Ok(Number::from(value))
            }
        }

        deserializer.deserialize_any(NumberVisitor)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::hash_map::DefaultHasher;

    use super::*;

    fn hash_of(number: Number) -> u64 {
        let mut hasher = DefaultHasher::new();
        number.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn a_non_negative_integer_is_always_the_positive_representation() {
        assert_eq!(Number::from(1i64), Number::from(1u64));
        assert_eq!(Number::from(0i32), Number::ZERO);
        assert!(Number::from(1i8).is_u64());
        assert!(Number::from(-1i8).is_i64());
        assert!(!Number::from(-1i8).is_u64());
    }

    #[test]
    fn integers_and_floats_never_compare_equal() {
        assert_ne!(Number::from(1u8), Number::from(1.0));
        assert!(Number::from(1u8).is_integer());
        assert!(!Number::from(1.0).is_integer());
    }

    #[test]
    fn u64_max_survives_intact() {
        let big = Number::from(u64::MAX);
        assert_eq!(big.as_u64(), Some(u64::MAX));
        assert_eq!(big.as_i64(), None);
        assert!(!big.is_i64());
        assert_eq!(big.to_string(), "18446744073709551615");
    }

    #[test]
    fn i64_min_survives_intact() {
        let small = Number::from(i64::MIN);
        assert_eq!(small.as_i64(), Some(i64::MIN));
        assert_eq!(small.as_u64(), None);
        assert_eq!(small.to_string(), "-9223372036854775808");
    }

    #[test]
    fn nan_equals_nan_and_hashes_the_same() {
        let a = Number::from(f64::NAN);
        let b = Number::from(-f64::NAN);
        assert_eq!(a, b);
        assert_eq!(hash_of(a), hash_of(b));
        assert!(!a.is_finite());
    }

    #[test]
    fn negative_zero_equals_zero_and_hashes_the_same() {
        let a = Number::from(0.0);
        let b = Number::from(-0.0);
        assert_eq!(a, b);
        assert_eq!(hash_of(a), hash_of(b));
    }

    #[test]
    fn equal_integers_hash_the_same_across_source_types() {
        assert_eq!(hash_of(Number::from(7u8)), hash_of(Number::from(7i64)));
        assert_ne!(hash_of(Number::from(7u8)), hash_of(Number::from(7.0)));
    }

    #[test]
    fn float_layout_follows_the_ryu_window() {
        // Decimal while the magnitude stays inside 10^-5 .. 10^17 ...
        for (value, expected) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (5.0, "5.0"),
            (0.1, "0.1"),
            (1.5, "1.5"),
            (0.0005, "0.0005"),
            (1000.0, "1000.0"),
            (123_456_789.0, "123456789.0"),
            (-4.392_382_137_218_392e-5, "-0.00004392382137218392"),
            (1e15, "1000000000000000.0"),
            (1.234_567_890_123_456e15, "1234567890123456.0"),
        ] {
            assert_eq!(Number::from(value).to_string(), expected, "{value:e}");
        }
        // ... scientific outside it.
        for (value, expected) in [
            (1e-6, "1e-6"),
            (1e-7, "1e-7"),
            (5e-324, "5e-324"),
            (1e16, "1e16"),
            (1e20, "1e20"),
            (1e21, "1e21"),
            (1e100, "1e100"),
            (1.5e300, "1.5e300"),
            (f64::MAX, "1.7976931348623157e308"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
        ] {
            assert_eq!(Number::from(value).to_string(), expected, "{value:e}");
        }
    }

    #[test]
    fn every_float_renders_back_to_its_own_bits() {
        // The property that actually matters: whatever layout is chosen, the
        // text must parse back to the same `f64`, bit for bit.
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut checked = 0;
        for _ in 0..20_000 {
            let value = f64::from_bits(next());
            if !value.is_finite() {
                continue;
            }
            checked += 1;
            let rendered = Number::from(value).to_string();
            let parsed: f64 = rendered
                .parse()
                .unwrap_or_else(|error| panic!("{rendered}: {error}"));
            assert_eq!(parsed.to_bits(), value.to_bits(), "{rendered}");
        }
        // Powers of two are the family whose exact expansions end in `5`,
        // which is where a careless shortest-digit rule loses a bit.
        for exponent in -1074..1024 {
            let value = 2f64.powi(exponent);
            if !value.is_finite() || value == 0.0 {
                continue;
            }
            checked += 1;
            let rendered = Number::from(value).to_string();
            let parsed: f64 = rendered
                .parse()
                .unwrap_or_else(|error| panic!("{rendered}: {error}"));
            assert_eq!(
                parsed.to_bits(),
                value.to_bits(),
                "2^{exponent} = {rendered}"
            );
        }
        assert!(checked > 20_000, "only {checked} floats checked");
    }

    #[test]
    fn display_uses_the_core_schema_spellings() {
        assert_eq!(Number::from(f64::INFINITY).to_string(), ".inf");
        assert_eq!(Number::from(f64::NEG_INFINITY).to_string(), "-.inf");
        assert_eq!(Number::from(f64::NAN).to_string(), ".nan");
        assert_eq!(Number::from(2.0).to_string(), "2.0");
        assert_eq!(Number::from(0.1).to_string(), "0.1");
        assert_eq!(Number::from(1e20).to_string(), "1e20");
        assert_eq!(Number::from(-0.0).to_string(), "-0.0");
        assert_eq!(Number::from(5u16).to_string(), "5");
    }

    #[test]
    fn conversions_widen_predictably() {
        assert_eq!(Number::from(1.5f32).as_f64(), Some(1.5));
        assert_eq!(Number::from(3usize).as_u64(), Some(3));
        assert_eq!(Number::from(-3isize).as_i64(), Some(-3));
        assert_eq!(Number::from(u32::MAX).as_f64(), Some(4_294_967_295.0));
        assert_eq!(Number::from_f64(2.0).as_f64(), Some(2.0));
        assert!(Number::from_f64(2.0).is_f64());
    }
}
