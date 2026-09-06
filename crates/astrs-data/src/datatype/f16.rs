//! [`F16`] — IEEE 754 binary16, implemented in-crate.
//!
//! The blueprint's dependency policy (§18.1) keeps the retained external list
//! closed, and `half` is not on it, so `astrs-data` owns its half-precision
//! type. `F16` is a `#[repr(transparent)]` newtype over `u16`, which is what
//! makes it a legal [`crate::ArrowNativeType`]: an array of `F16` has exactly
//! the Arrow `Float16` buffer layout, so decoding is a reinterpretation rather
//! than a conversion.
//!
//! # Layout
//!
//! ```text
//!   bit 15   bits 14..10      bits 9..0
//!   ┌─────┬───────────────┬───────────────┐
//!   │sign │  exponent (5) │ mantissa (10) │   bias 15
//!   └─────┴───────────────┴───────────────┘
//! ```
//!
//! # Rounding
//!
//! [`F16::from_f32`] rounds **to nearest, ties to even** — the IEEE 754
//! default and the mode arrow-rs, numpy and PyTorch all use, so a tensor that
//! round-trips through an AstRS payload matches bit for bit. The four edge
//! regimes are handled explicitly:
//!
//! | `f32` input | result |
//! |---|---|
//! | magnitude ≥ 65520 | ±infinity (the tie point rounds up out of range) |
//! | normal range | normal `F16`, mantissa rounded to nearest even |
//! | 2⁻²⁵ … 2⁻¹⁴ | subnormal `F16` (the implicit leading one is shifted in) |
//! | < 2⁻²⁵, or `f32` subnormal | ±zero |
//! | NaN | quiet NaN, sign and the top payload bits preserved |
//!
//! [`F16::to_f32`] is always exact: every `F16` value has an `f32`
//! representation.
//!
//! ```
//! use astrs_data::F16;
//!
//! assert_eq!(F16::from_f32(1.0).to_f32(), 1.0);
//! assert_eq!(F16::from_f32(-2.5).to_f32(), -2.5);
//! assert!(F16::from_f32(1e6).is_infinite());
//! assert!(F16::from_f32(f32::NAN).is_nan());
//!
//! // Ties round to even: 2049 sits exactly between 2048 and 2050.
//! assert_eq!(F16::from_f32(2049.0).to_f32(), 2048.0);
//! assert_eq!(F16::from_f32(2051.0).to_f32(), 2052.0);
//! ```

use std::cmp::Ordering;
use std::fmt;

/// Half-precision (IEEE 754 binary16) floating point.
///
/// See the [module documentation](self) for the layout and rounding rules.
#[derive(Clone, Copy, Default)]
#[repr(transparent)]
pub struct F16(u16);

impl F16 {
    /// Positive zero.
    pub const ZERO: Self = Self(0x0000);
    /// Negative zero.
    pub const NEG_ZERO: Self = Self(0x8000);
    /// `1.0`.
    pub const ONE: Self = Self(0x3c00);
    /// `-1.0`.
    pub const NEG_ONE: Self = Self(0xbc00);
    /// Positive infinity.
    pub const INFINITY: Self = Self(0x7c00);
    /// Negative infinity.
    pub const NEG_INFINITY: Self = Self(0xfc00);
    /// A quiet NaN.
    pub const NAN: Self = Self(0x7e00);
    /// Largest finite value, `65504`.
    pub const MAX: Self = Self(0x7bff);
    /// Smallest (most negative) finite value, `-65504`.
    pub const MIN: Self = Self(0xfbff);
    /// Smallest positive normal value, `2^-14`.
    pub const MIN_POSITIVE: Self = Self(0x0400);
    /// Smallest positive subnormal value, `2^-24`.
    pub const MIN_POSITIVE_SUBNORMAL: Self = Self(0x0001);
    /// Difference between `1.0` and the next larger value, `2^-10`.
    pub const EPSILON: Self = Self(0x1400);
    /// Number of bits in the representation.
    pub const BITS: u32 = 16;

    /// Reinterprets a raw bit pattern as an `F16`.
    #[inline]
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// The raw bit pattern.
    #[inline]
    #[must_use]
    pub const fn to_bits(self) -> u16 {
        self.0
    }

    /// The little-endian byte representation, as it appears in an Arrow buffer.
    #[inline]
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    /// Reads an `F16` from its little-endian byte representation.
    #[inline]
    #[must_use]
    pub const fn from_le_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_le_bytes(bytes))
    }

    /// Converts from `f32`, rounding to nearest with ties to even.
    ///
    /// See the [module documentation](self) for the exact regime table.
    ///
    /// ```
    /// use astrs_data::F16;
    ///
    /// assert_eq!(F16::from_f32(0.0), F16::ZERO);
    /// assert_eq!(F16::from_f32(-0.0), F16::NEG_ZERO);
    /// assert_eq!(F16::from_f32(65504.0), F16::MAX);
    /// assert_eq!(F16::from_f32(65520.0), F16::INFINITY, "ties round out of range");
    /// assert_eq!(F16::from_f32(2f32.powi(-24)), F16::MIN_POSITIVE_SUBNORMAL);
    /// assert_eq!(F16::from_f32(2f32.powi(-26)), F16::ZERO);
    /// ```
    #[must_use]
    pub const fn from_f32(value: f32) -> Self {
        let bits = value.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let biased = ((bits >> 23) & 0xff) as i32;
        let mantissa = bits & 0x007f_ffff;

        // Infinity and NaN keep their class; NaN is quieted and keeps the top
        // payload bits so a debugger still shows which NaN it was.
        if biased == 0xff {
            if mantissa == 0 {
                return Self(sign | 0x7c00);
            }
            return Self(sign | 0x7e00 | ((mantissa >> 13) as u16));
        }

        // Re-bias: f32 uses 127, f16 uses 15.
        let exponent = biased - 127 + 15;

        if exponent >= 0x1f {
            // Overflows the f16 exponent range.
            return Self(sign | 0x7c00);
        }

        if exponent <= 0 {
            // Subnormal territory, or too small to represent at all. An f32
            // subnormal (biased == 0) lands here too and always underflows.
            if exponent < -10 {
                return Self(sign);
            }
            // Restore the implicit leading one, then shift into place. The
            // shift is in 14..=24, so it never overflows the u32.
            let significand = mantissa | 0x0080_0000;
            let shift = (14 - exponent) as u32;
            let value = significand >> shift;
            let round_bit = 1u32 << (shift - 1);
            let remainder = significand & (round_bit * 2 - 1);
            let rounded = if remainder > round_bit || (remainder == round_bit && value & 1 == 1) {
                value + 1
            } else {
                value
            };
            return Self(sign | rounded as u16);
        }

        // Normal: drop 13 mantissa bits, rounding to nearest even. A carry out
        // of the mantissa correctly increments the exponent, and a carry out
        // of the exponent correctly produces infinity.
        let half = ((exponent as u16) << 10) | ((mantissa >> 13) as u16);
        let remainder = mantissa & 0x1fff;
        let rounded = if remainder > 0x1000 || (remainder == 0x1000 && half & 1 == 1) {
            half + 1
        } else {
            half
        };
        Self(sign | rounded)
    }

    /// Converts to `f32`. Always exact.
    ///
    /// ```
    /// use astrs_data::F16;
    ///
    /// assert_eq!(F16::MAX.to_f32(), 65504.0);
    /// assert_eq!(F16::MIN_POSITIVE_SUBNORMAL.to_f32(), 2f32.powi(-24));
    /// assert!(F16::INFINITY.to_f32().is_infinite());
    /// ```
    #[must_use]
    pub const fn to_f32(self) -> f32 {
        let half = self.0 as u32;
        let sign = (half & 0x8000) << 16;
        let exponent = (half >> 10) & 0x1f;
        let mantissa = half & 0x3ff;

        let bits = if exponent == 0 {
            if mantissa == 0 {
                // Signed zero.
                sign
            } else {
                // Subnormal: normalise by the position of the highest set bit.
                // With `k` that position, the value is `mantissa * 2^-24`, so
                // the f32 biased exponent is `k + 103`.
                let k = 31 - mantissa.leading_zeros();
                let exponent = k + 103;
                let fraction = (mantissa << (23 - k)) & 0x007f_ffff;
                sign | (exponent << 23) | fraction
            }
        } else if exponent == 0x1f {
            // Infinity or NaN; the payload is shifted into place unchanged.
            sign | 0x7f80_0000 | (mantissa << 13)
        } else {
            sign | ((exponent + 127 - 15) << 23) | (mantissa << 13)
        };
        f32::from_bits(bits)
    }

    /// Converts from `f64` by way of `f32`.
    ///
    /// Double rounding is harmless here: `f32` has 24 significand bits against
    /// `F16`'s 11, so the intermediate is always exact enough for the second
    /// rounding to land on the same value as a direct conversion.
    #[inline]
    #[must_use]
    pub const fn from_f64(value: f64) -> Self {
        Self::from_f32(value as f32)
    }

    /// Converts to `f64`. Always exact.
    #[inline]
    #[must_use]
    pub const fn to_f64(self) -> f64 {
        self.to_f32() as f64
    }

    /// Returns `true` for NaN.
    #[inline]
    #[must_use]
    pub const fn is_nan(self) -> bool {
        self.0 & 0x7c00 == 0x7c00 && self.0 & 0x03ff != 0
    }

    /// Returns `true` for ±infinity.
    #[inline]
    #[must_use]
    pub const fn is_infinite(self) -> bool {
        self.0 & 0x7fff == 0x7c00
    }

    /// Returns `true` for a finite value (neither NaN nor infinity).
    #[inline]
    #[must_use]
    pub const fn is_finite(self) -> bool {
        self.0 & 0x7c00 != 0x7c00
    }

    /// Returns `true` for ±zero.
    #[inline]
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 & 0x7fff == 0
    }

    /// Returns `true` for a subnormal (denormal) value. Zero is not subnormal.
    #[inline]
    #[must_use]
    pub const fn is_subnormal(self) -> bool {
        self.0 & 0x7c00 == 0 && self.0 & 0x03ff != 0
    }

    /// Returns `true` when the sign bit is clear (including `+0.0` and NaN
    /// with a clear sign).
    #[inline]
    #[must_use]
    pub const fn is_sign_positive(self) -> bool {
        self.0 & 0x8000 == 0
    }

    /// Returns `true` when the sign bit is set.
    #[inline]
    #[must_use]
    pub const fn is_sign_negative(self) -> bool {
        self.0 & 0x8000 != 0
    }

    /// The magnitude, with the sign bit cleared.
    #[inline]
    #[must_use]
    pub const fn abs(self) -> Self {
        Self(self.0 & 0x7fff)
    }

    /// The value with the sign bit flipped.
    #[inline]
    #[must_use]
    pub const fn neg(self) -> Self {
        Self(self.0 ^ 0x8000)
    }

    /// Total ordering over the bit patterns, as IEEE 754 `totalOrder`.
    ///
    /// Unlike [`PartialOrd`], this is a total order: NaNs sort at the ends and
    /// `-0.0` sorts below `+0.0`. Sorting kernels use it so a column of
    /// `Float16` has a deterministic order even with NaNs present.
    ///
    /// ```
    /// use astrs_data::F16;
    ///
    /// let mut values = [F16::ONE, F16::NAN, F16::NEG_ZERO, F16::ZERO];
    /// values.sort_by(|a, b| a.total_cmp(*b));
    /// // Compared bit-wise: `PartialEq` is IEEE, so `NaN != NaN` would make a
    /// // direct value comparison fail however well the sort worked.
    /// assert_eq!(
    ///     values.map(F16::to_bits),
    ///     [F16::NEG_ZERO, F16::ZERO, F16::ONE, F16::NAN].map(F16::to_bits),
    /// );
    /// ```
    #[must_use]
    pub const fn total_cmp(self, other: Self) -> Ordering {
        // Flip the sign bit for positives and every bit for negatives, which
        // maps the IEEE bit patterns onto a monotone i16 ordering.
        let left = flip_for_total_order(self.0);
        let right = flip_for_total_order(other.0);
        if left < right {
            Ordering::Less
        } else if left > right {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

/// Maps an `F16` bit pattern onto a monotonically ordered `u16` key.
#[inline]
const fn flip_for_total_order(bits: u16) -> u16 {
    if bits & 0x8000 == 0 {
        bits | 0x8000
    } else {
        !bits
    }
}

impl PartialEq for F16 {
    /// IEEE 754 equality, matching `f32`/`f64`: `NaN != NaN` and
    /// `-0.0 == +0.0`.
    ///
    /// `F16` therefore implements neither [`Eq`] nor [`std::hash::Hash`], for
    /// the same reason the primitive floats do not. Use [`F16::to_bits`] for
    /// bit-exact comparison and [`F16::total_cmp`] for a total order; array
    /// equality goes through [`crate::ArrowNativeType::value_eq`], which
    /// treats NaN as reflexive so a recorded payload equals itself.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // Both zero (either sign), or identical bits and not NaN.
        if self.0 | other.0 == 0x8000 || self.0 == 0 && other.0 == 0 {
            return true;
        }
        self.0 == other.0 && !self.is_nan()
    }
}

impl PartialOrd for F16 {
    /// IEEE 754 comparison: NaN compares to nothing, `-0.0 == +0.0`.
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.to_f32().partial_cmp(&other.to_f32())
    }
}

impl fmt::Debug for F16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_f32())
    }
}

impl fmt::Display for F16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_f32(), f)
    }
}

impl fmt::LowerExp for F16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerExp::fmt(&self.to_f32(), f)
    }
}

impl From<f32> for F16 {
    #[inline]
    fn from(value: f32) -> Self {
        Self::from_f32(value)
    }
}

impl From<f64> for F16 {
    #[inline]
    fn from(value: f64) -> Self {
        Self::from_f64(value)
    }
}

impl From<F16> for f32 {
    #[inline]
    fn from(value: F16) -> Self {
        value.to_f32()
    }
}

impl From<F16> for f64 {
    #[inline]
    fn from(value: F16) -> Self {
        value.to_f64()
    }
}

impl std::ops::Neg for F16 {
    type Output = Self;

    #[inline]
    fn neg(self) -> Self {
        Self::neg(self)
    }
}

impl serde::Serialize for F16 {
    /// Serialises as the `f32` value, so JSON and YAML manifests stay readable.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f32(self.to_f32())
    }
}

impl<'de> serde::Deserialize<'de> for F16 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        f32::deserialize(deserializer).map(Self::from_f32)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn layout_is_two_bytes() {
        assert_eq!(std::mem::size_of::<F16>(), 2);
        assert_eq!(std::mem::align_of::<F16>(), 2);
    }

    #[test]
    fn constants_have_the_documented_bits() {
        assert_eq!(F16::ZERO.to_f32(), 0.0);
        assert_eq!(F16::ONE.to_f32(), 1.0);
        assert_eq!(F16::NEG_ONE.to_f32(), -1.0);
        assert_eq!(F16::MAX.to_f32(), 65504.0);
        assert_eq!(F16::MIN.to_f32(), -65504.0);
        assert_eq!(F16::MIN_POSITIVE.to_f32(), 2f32.powi(-14));
        assert_eq!(F16::MIN_POSITIVE_SUBNORMAL.to_f32(), 2f32.powi(-24));
        assert_eq!(F16::EPSILON.to_f32(), 2f32.powi(-10));
        assert!(F16::INFINITY.to_f32().is_infinite());
        assert!(F16::INFINITY.to_f32().is_sign_positive());
        assert!(F16::NEG_INFINITY.to_f32().is_sign_negative());
        assert!(F16::NAN.is_nan());
        assert_eq!(F16::BITS, 16);
    }

    #[test]
    fn exact_values_round_trip() {
        let exact = [
            0.0f32, -0.0, 1.0, -1.0, 0.5, -0.5, 2.0, 1024.0, -1024.0, 65504.0, -65504.0, 0.25, 1.5,
            100.0, 3.5,
        ];
        for value in exact {
            let half = F16::from_f32(value);
            assert_eq!(half.to_f32(), value, "round trip failed for {value}");
        }
    }

    #[test]
    fn equality_follows_ieee() {
        assert_eq!(F16::ZERO, F16::NEG_ZERO, "signed zeros compare equal");
        assert_ne!(F16::NAN, F16::NAN, "NaN compares to nothing");
        assert_eq!(F16::ONE, F16::from_f32(1.0));
        assert_ne!(F16::ONE, F16::NEG_ONE);
        assert_ne!(F16::NAN, F16::ZERO);
        assert_ne!(F16::ZERO, F16::NAN);
    }

    #[test]
    fn signed_zero_is_preserved() {
        assert_eq!(F16::from_f32(0.0).to_bits(), 0x0000);
        assert_eq!(F16::from_f32(-0.0).to_bits(), 0x8000);
        assert!(F16::from_f32(-0.0).is_zero());
        assert!(F16::from_f32(-0.0).is_sign_negative());
        assert_eq!(F16::from_f32(-0.0).to_bits(), F16::NEG_ZERO.to_bits());
        // IEEE equality: the two zeroes compare equal as floats.
        assert_eq!(F16::ZERO.partial_cmp(&F16::NEG_ZERO), Some(Ordering::Equal));
    }

    #[test]
    fn ties_round_to_even() {
        // 2048 and 2050 are adjacent f16 values (spacing 2 at that magnitude).
        assert_eq!(F16::from_f32(2049.0).to_f32(), 2048.0, "tie down to even");
        assert_eq!(F16::from_f32(2051.0).to_f32(), 2052.0, "tie up to even");
        assert_eq!(F16::from_f32(2050.0).to_f32(), 2050.0);
        // 1.0 + eps/2 is a tie that resolves down.
        let tie = 1.0f32 + 2f32.powi(-11);
        assert_eq!(F16::from_f32(tie).to_f32(), 1.0);
        let above = 1.0f32 + 2f32.powi(-11) + 2f32.powi(-20);
        assert_eq!(F16::from_f32(above).to_f32(), 1.0 + 2f32.powi(-10));
    }

    #[test]
    fn overflow_saturates_to_infinity() {
        assert_eq!(F16::from_f32(65504.0), F16::MAX);
        // 65519.996 is the largest value that still rounds down to MAX.
        assert_eq!(F16::from_f32(65519.0), F16::MAX);
        assert_eq!(F16::from_f32(65520.0), F16::INFINITY);
        assert_eq!(F16::from_f32(-65520.0), F16::NEG_INFINITY);
        assert_eq!(F16::from_f32(1e30), F16::INFINITY);
        assert_eq!(F16::from_f32(f32::INFINITY), F16::INFINITY);
        assert_eq!(F16::from_f32(f32::NEG_INFINITY), F16::NEG_INFINITY);
        assert_eq!(F16::from_f32(f32::MAX), F16::INFINITY);
    }

    #[test]
    fn subnormals_convert_both_ways() {
        for exponent in -24i32..=-15 {
            let value = 2f32.powi(exponent);
            let half = F16::from_f32(value);
            assert!(half.is_subnormal(), "2^{exponent} should be subnormal");
            assert_eq!(half.to_f32(), value, "2^{exponent}");
        }
        // Every subnormal bit pattern round-trips.
        for bits in 1u16..0x0400 {
            let half = F16::from_bits(bits);
            assert!(half.is_subnormal());
            assert_eq!(F16::from_f32(half.to_f32()), half, "bits {bits:#06x}");
        }
    }

    #[test]
    fn underflow_reaches_zero_through_the_tie() {
        // 2^-25 is exactly half of the smallest subnormal: ties to even => 0.
        assert_eq!(F16::from_f32(2f32.powi(-25)), F16::ZERO);
        // Anything above the tie rounds up to the smallest subnormal.
        assert_eq!(
            F16::from_f32(2f32.powi(-25) + 2f32.powi(-30)),
            F16::MIN_POSITIVE_SUBNORMAL
        );
        assert_eq!(F16::from_f32(2f32.powi(-26)), F16::ZERO);
        assert_eq!(F16::from_f32(-2f32.powi(-30)), F16::NEG_ZERO);
        // f32 subnormals underflow to signed zero.
        assert_eq!(F16::from_f32(f32::from_bits(1)), F16::ZERO);
        assert_eq!(F16::from_f32(-f32::MIN_POSITIVE), F16::NEG_ZERO);
    }

    #[test]
    fn nan_is_quieted_and_keeps_its_sign() {
        let nan = F16::from_f32(f32::NAN);
        assert!(nan.is_nan());
        assert!(!nan.is_finite());
        assert!(!nan.is_infinite());
        let negative = F16::from_f32(-f32::NAN);
        assert!(negative.is_nan());
        assert!(negative.is_sign_negative());
        // A signalling NaN payload is preserved in the top bits.
        let signalling = f32::from_bits(0x7f80_0001);
        assert!(F16::from_f32(signalling).is_nan());
        assert!(F16::NAN.to_f32().is_nan());
    }

    #[test]
    fn every_bit_pattern_survives_f32_round_trip() {
        for bits in 0u16..=u16::MAX {
            let half = F16::from_bits(bits);
            let back = F16::from_f32(half.to_f32());
            if half.is_nan() {
                assert!(back.is_nan(), "bits {bits:#06x}");
            } else {
                assert_eq!(back.to_bits(), bits, "bits {bits:#06x}");
            }
        }
    }

    #[test]
    fn f64_conversions_agree_with_f32() {
        for bits in (0u16..=u16::MAX).step_by(37) {
            let half = F16::from_bits(bits);
            if half.is_nan() {
                continue;
            }
            assert_eq!(half.to_f64(), f64::from(half.to_f32()));
            assert_eq!(F16::from_f64(half.to_f64()), half);
        }
        assert_eq!(F16::from_f64(1.5), F16::from_f32(1.5));
        assert_eq!(F16::from_f64(1e300), F16::INFINITY);
    }

    #[test]
    fn classification_predicates() {
        assert!(F16::ZERO.is_zero() && F16::ZERO.is_finite());
        assert!(!F16::ZERO.is_subnormal());
        assert!(F16::MIN_POSITIVE_SUBNORMAL.is_subnormal());
        assert!(!F16::MIN_POSITIVE.is_subnormal());
        assert!(F16::INFINITY.is_infinite() && !F16::INFINITY.is_finite());
        assert!(F16::NAN.is_nan() && !F16::NAN.is_finite());
        assert!(F16::ONE.is_sign_positive());
        assert!(F16::NEG_ONE.is_sign_negative());
    }

    #[test]
    fn abs_and_neg() {
        assert_eq!(F16::NEG_ONE.abs(), F16::ONE);
        assert_eq!(F16::ONE.abs(), F16::ONE);
        assert_eq!(F16::NEG_ZERO.abs(), F16::ZERO);
        assert_eq!(-F16::ONE, F16::NEG_ONE);
        assert_eq!(F16::ONE.neg(), F16::NEG_ONE);
        assert_eq!(F16::NEG_INFINITY.abs(), F16::INFINITY);
    }

    #[test]
    fn partial_ord_follows_ieee() {
        assert!(F16::from_f32(1.0) < F16::from_f32(2.0));
        assert!(F16::NEG_INFINITY < F16::MIN);
        assert!(F16::MAX < F16::INFINITY);
        assert_eq!(F16::NAN.partial_cmp(&F16::ONE), None);
        assert_ne!(F16::NAN, F16::from_f32(f32::NAN).neg());
    }

    #[test]
    fn total_cmp_is_a_total_order() {
        let mut values = [
            F16::NAN,
            F16::ONE,
            F16::NEG_INFINITY,
            F16::ZERO,
            F16::NEG_ZERO,
            F16::INFINITY,
            F16::NEG_ONE,
        ];
        values.sort_by(|a, b| a.total_cmp(*b));
        assert_eq!(
            values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            vec![
                F16::NEG_INFINITY.to_bits(),
                F16::NEG_ONE.to_bits(),
                F16::NEG_ZERO.to_bits(),
                F16::ZERO.to_bits(),
                F16::ONE.to_bits(),
                F16::INFINITY.to_bits(),
                F16::NAN.to_bits(),
            ]
        );
        assert_eq!(F16::ONE.total_cmp(F16::ONE), Ordering::Equal);
    }

    #[test]
    fn byte_representation_is_little_endian() {
        let value = F16::from_f32(1.0);
        assert_eq!(value.to_bits(), 0x3c00);
        assert_eq!(value.to_le_bytes(), [0x00, 0x3c]);
        assert_eq!(F16::from_le_bytes([0x00, 0x3c]), value);
    }

    #[test]
    fn formatting() {
        assert_eq!(format!("{}", F16::from_f32(1.5)), "1.5");
        assert_eq!(format!("{:?}", F16::from_f32(-2.0)), "-2");
        assert_eq!(format!("{:e}", F16::from_f32(1000.0)), "1e3");
    }

    #[test]
    fn conversions_via_from() {
        assert_eq!(F16::from(1.5f32), F16::from_f32(1.5));
        assert_eq!(F16::from(1.5f64), F16::from_f32(1.5));
        assert_eq!(f32::from(F16::ONE), 1.0);
        assert_eq!(f64::from(F16::ONE), 1.0);
    }

    #[test]
    fn serde_round_trip_through_f32() {
        let value = F16::from_f32(-2.5);
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, "-2.5");
        assert_eq!(serde_json::from_str::<F16>(&json).unwrap(), value);
        assert_eq!(serde_json::from_str::<F16>("1.0").unwrap(), F16::ONE);
        // Values outside the f16 range still deserialise, saturating.
        assert_eq!(serde_json::from_str::<F16>("1e30").unwrap(), F16::INFINITY);
    }
}
