//! [`ArrowNativeType`] — the sealed bridge between Rust scalars and Arrow
//! value buffers.
//!
//! # Contract
//!
//! The trait is sealed through [`crate::sealed::Sealed`] and implemented for
//! exactly eleven types: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`,
//! `f32`, `f64` and [`F16`]. Every one of them is:
//!
//! * fixed-width, with `size_of` equal to its Arrow buffer width;
//! * free of padding bytes, so `size_of_val(&[T])` is exactly the buffer size;
//! * valid for every bit pattern, so decoding never has to validate;
//! * `Copy`, with no `Drop` and no interior mutability.
//!
//! [`crate::buffer::scalar`] relies on all four properties to reinterpret
//! bytes as `[T]` without copying. **Implementing this trait for a type that
//! breaks any of them would make that module unsound** — which is why it is
//! sealed.
//!
//! ```
//! use astrs_data::{ArrowNativeType, DataType, F16};
//!
//! assert_eq!(i32::DATA_TYPE, DataType::Int32);
//! assert_eq!(F16::DATA_TYPE, DataType::Float16);
//! assert_eq!(<u16 as ArrowNativeType>::WIDTH, 2);
//! assert_eq!(i16::from_le(0x0102i16.to_le()), 0x0102);
//! ```

use crate::datatype::{DataType, F16};
use crate::sealed::Sealed;

/// A Rust scalar that maps one-to-one onto an Arrow fixed-width buffer slot.
///
/// See the [module documentation](self) for the safety-relevant contract this
/// sealed trait encodes.
pub trait ArrowNativeType:
    Sealed + Copy + Default + Send + Sync + std::fmt::Debug + PartialEq + 'static
{
    /// The Arrow type this scalar maps onto.
    const DATA_TYPE: DataType;

    /// Width of one value in an Arrow buffer, in bytes.
    const WIDTH: usize = std::mem::size_of::<Self>();

    /// Converts to little-endian byte order, the order Arrow buffers use.
    ///
    /// A no-op on the little-endian hosts AstRS targets; stage 2's IPC decoder
    /// uses it to normalise a payload from a big-endian producer.
    #[must_use]
    fn to_le(self) -> Self;

    /// Converts from little-endian byte order.
    #[must_use]
    fn from_le(value: Self) -> Self;

    /// Equality used when comparing two arrays slot by slot.
    ///
    /// Defaults to [`PartialEq`]. The float types override it so that
    /// `NaN == NaN`, which makes array equality an equivalence relation —
    /// without it, a recorded payload containing a NaN would not compare equal
    /// to itself after a replay round trip. Signed zeros still compare equal,
    /// as IEEE says they should.
    ///
    /// ```
    /// use astrs_data::ArrowNativeType;
    ///
    /// assert!(f32::NAN.value_eq(&f32::NAN));
    /// assert!(0.0f64.value_eq(&-0.0f64));
    /// assert!(!1.0f32.value_eq(&2.0f32));
    /// ```
    fn value_eq(&self, other: &Self) -> bool {
        self == other
    }

    /// Renders the value for `Debug` output of an array.
    ///
    /// Defaults to the type's own `Debug`; [`F16`] needs no override, because
    /// its `Debug` already prints the decoded float.
    fn render(&self) -> String {
        format!("{self:?}")
    }
}

/// Implements [`ArrowNativeType`] for an integer type, whose endian
/// conversions the standard library already provides.
macro_rules! impl_native_int {
    ($ty:ty, $variant:ident) => {
        impl Sealed for $ty {}

        impl ArrowNativeType for $ty {
            const DATA_TYPE: DataType = DataType::$variant;

            #[inline]
            fn to_le(self) -> Self {
                <$ty>::to_le(self)
            }

            #[inline]
            fn from_le(value: Self) -> Self {
                <$ty>::from_le(value)
            }
        }
    };
}

/// Implements [`ArrowNativeType`] for a float type, whose endian conversions
/// go through the bit pattern.
macro_rules! impl_native_float {
    ($ty:ty, $bits:ty, $variant:ident) => {
        impl Sealed for $ty {}

        impl ArrowNativeType for $ty {
            const DATA_TYPE: DataType = DataType::$variant;

            #[inline]
            fn to_le(self) -> Self {
                <$ty>::from_bits(<$bits>::to_le(self.to_bits()))
            }

            #[inline]
            fn from_le(value: Self) -> Self {
                <$ty>::from_bits(<$bits>::from_le(value.to_bits()))
            }

            #[inline]
            fn value_eq(&self, other: &Self) -> bool {
                self == other || (self.is_nan() && other.is_nan())
            }
        }
    };
}

impl_native_int!(i8, Int8);
impl_native_int!(i16, Int16);
impl_native_int!(i32, Int32);
impl_native_int!(i64, Int64);
impl_native_int!(u8, UInt8);
impl_native_int!(u16, UInt16);
impl_native_int!(u32, UInt32);
impl_native_int!(u64, UInt64);
impl_native_float!(f32, u32, Float32);
impl_native_float!(f64, u64, Float64);

impl Sealed for F16 {}

impl ArrowNativeType for F16 {
    const DATA_TYPE: DataType = DataType::Float16;

    #[inline]
    fn to_le(self) -> Self {
        Self::from_bits(u16::to_le(self.to_bits()))
    }

    #[inline]
    fn from_le(value: Self) -> Self {
        Self::from_bits(u16::from_le(value.to_bits()))
    }

    #[inline]
    fn value_eq(&self, other: &Self) -> bool {
        self == other || (self.is_nan() && other.is_nan())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_native_type_declares_its_arrow_type() {
        assert_eq!(i8::DATA_TYPE, DataType::Int8);
        assert_eq!(i16::DATA_TYPE, DataType::Int16);
        assert_eq!(i32::DATA_TYPE, DataType::Int32);
        assert_eq!(i64::DATA_TYPE, DataType::Int64);
        assert_eq!(u8::DATA_TYPE, DataType::UInt8);
        assert_eq!(u16::DATA_TYPE, DataType::UInt16);
        assert_eq!(u32::DATA_TYPE, DataType::UInt32);
        assert_eq!(u64::DATA_TYPE, DataType::UInt64);
        assert_eq!(f32::DATA_TYPE, DataType::Float32);
        assert_eq!(f64::DATA_TYPE, DataType::Float64);
        assert_eq!(F16::DATA_TYPE, DataType::Float16);
    }

    #[test]
    fn widths_match_the_arrow_buffer_layout() {
        assert_eq!(<i8 as ArrowNativeType>::WIDTH, 1);
        assert_eq!(<u8 as ArrowNativeType>::WIDTH, 1);
        assert_eq!(<i16 as ArrowNativeType>::WIDTH, 2);
        assert_eq!(<F16 as ArrowNativeType>::WIDTH, 2);
        assert_eq!(<i32 as ArrowNativeType>::WIDTH, 4);
        assert_eq!(<f32 as ArrowNativeType>::WIDTH, 4);
        assert_eq!(<i64 as ArrowNativeType>::WIDTH, 8);
        assert_eq!(<f64 as ArrowNativeType>::WIDTH, 8);
        assert_eq!(
            <u64 as ArrowNativeType>::WIDTH,
            u64::DATA_TYPE.primitive_width().unwrap()
        );
    }

    #[test]
    fn endian_conversions_round_trip() {
        assert_eq!(
            i32::from_le(ArrowNativeType::to_le(0x0102_0304i32)),
            0x0102_0304
        );
        assert_eq!(u16::from_le(ArrowNativeType::to_le(0xabcdu16)), 0xabcd);
        assert_eq!(f32::from_le(ArrowNativeType::to_le(1.5f32)), 1.5);
        assert_eq!(f64::from_le(ArrowNativeType::to_le(-2.25f64)), -2.25);
        let half = F16::from_f32(3.5);
        assert_eq!(F16::from_le(ArrowNativeType::to_le(half)), half);
        assert_eq!(i8::from_le(ArrowNativeType::to_le(-5i8)), -5);
    }

    #[test]
    fn nan_survives_the_endian_round_trip() {
        let nan = f64::NAN;
        assert!(f64::from_le(ArrowNativeType::to_le(nan)).is_nan());
        assert!(F16::from_le(ArrowNativeType::to_le(F16::NAN)).is_nan());
    }

    #[test]
    fn value_eq_makes_nan_reflexive() {
        assert!(f32::NAN.value_eq(&f32::NAN));
        assert!(f64::NAN.value_eq(&f64::NAN));
        assert!(F16::NAN.value_eq(&F16::NAN));
        assert!(0.0f32.value_eq(&-0.0f32));
        assert!(F16::ZERO.value_eq(&F16::NEG_ZERO));
        assert!(!1.0f64.value_eq(&2.0f64));
        assert!(!f32::NAN.value_eq(&1.0));
        assert!(7i32.value_eq(&7));
        assert!(!7i32.value_eq(&8));
    }

    #[test]
    fn render_uses_debug() {
        assert_eq!(1i32.render(), "1");
        assert_eq!((-2.5f64).render(), "-2.5");
        assert_eq!(F16::from_f32(1.5).render(), "1.5");
    }

    #[test]
    fn defaults_are_zero() {
        assert_eq!(i64::default(), 0);
        assert_eq!(f32::default(), 0.0);
        assert_eq!(F16::default(), F16::ZERO);
    }
}
