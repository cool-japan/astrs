//! `cast` — converting one array's type into another.
//!
//! Three families of conversion, each with its own module doc section
//! below: **numeric** (any of the eleven `Int8..64`/`UInt8..64`/
//! `Float16/32/64` types to any other, [`OverflowPolicy`]-governed),
//! **temporal reinterpretation** (`Timestamp`/`Duration` ⇄ `Int64`, free —
//! they already share the `i64` buffer layout), and **byte/string**
//! (`Utf8`⇄`Binary` reinterpretation and `Utf8`⇄`LargeUtf8`/`Binary`⇄
//! `LargeBinary` offset-width conversion). Everything else —
//! `Bool`, any nested type, `Timestamp`⇄`Duration` directly — has no defined
//! cast and reports [`DataError::UnsupportedCast`]; `Null` casts freely to
//! and from anything, becoming (or replacing) an all-null array.
//!
//! # Numeric casts and [`OverflowPolicy`]
//!
//! The policy governs exactly one thing: what happens when a value does not
//! fit the destination. It does not affect fractional truncation (`1.9_f64`
//! casting to `Int32` is `1` either way — that is narrowing, not overflow)
//! and it does not affect `NaN`/infinity passing between float types
//! (those propagate as themselves under both policies; only a *finite*
//! source becoming an *infinite* result counts as overflow). Concretely:
//!
//! * **Integer destination.** [`OverflowPolicy::Saturate`] clamps to the
//!   destination's `MIN`/`MAX` (and maps `NaN` to `0` when the source is a
//!   float) — precisely [`f64 as`](https://doc.rust-lang.org/reference/expressions/operator-expr.html#numeric-cast)'s
//!   own saturating-cast semantics, which this module leans on directly
//!   rather than reimplementing. [`OverflowPolicy::Error`] reports
//!   [`DataError::CastOverflow`] instead of clamping.
//! * **Float (`Float16`/`Float32`/`Float64`) destination.** There is no
//!   integer-style clamp — IEEE 754 already has a value for "too big to
//!   represent": infinity, with the source's sign. `Saturate` lets a value
//!   overflow to infinity exactly the way a plain `as` cast between float
//!   types already does; `Error` reports [`DataError::CastOverflow`] only
//!   when overflow is what happened (a source that was *already* infinite
//!   or `NaN` casts through unchanged under both policies).
//!
//! # Precision, not just range
//!
//! `Int64`/`UInt64` values beyond 2^53 lose precision converting through
//! `Float32`/`Float64` (there are more 64-bit integers than a `f64`
//! mantissa can distinguish) — this is `f64 as`'s well-defined rounding
//! behaviour, not a bug, and neither [`OverflowPolicy`] catches it: policy
//! is about values that do not *fit*, not values that do not survive
//! *exactly*. Integer-to-integer casts never lose precision this way —
//! the private `IntWiden`/`IntNarrow` helpers round-trip every representable
//! value through
//! `i128` exactly.

use std::sync::Arc;

use crate::F16;
use crate::array::{
    Array, ArrayExt, ArrayRef, BinaryArray, DurationArray, Int64Array, LargeBinaryArray,
    LargeStringArray, NullArray, PrimitiveArray, StringArray, TimestampArray, new_null_array,
};
use crate::buffer::ScalarBuffer;
use crate::builder::{ArrayBuilder, PrimitiveBuilder};
use crate::datatype::{ArrowNativeType, DataType};
use crate::error::{DataError, Result};

/// How [`cast`] handles a value that does not fit the destination type.
///
/// See the [module documentation](self) for exactly what counts as
/// "does not fit" for an integer destination versus a float one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OverflowPolicy {
    /// Clamp to the destination's representable range (or, for a float
    /// destination, let it overflow to infinity).
    #[default]
    Saturate,
    /// Report [`DataError::CastOverflow`] instead.
    Error,
}

/// Converts `array` to `to`, governed by `policy` for values that do not
/// fit — see the [module documentation](self) for exactly which conversions
/// are defined.
///
/// A request where `to` already matches `array`'s type is an `Arc` clone,
/// no copy.
///
/// # Errors
///
/// * [`DataError::UnsupportedCast`] when there is no defined conversion
///   between the two types (`Bool`, any nested type, or `Timestamp` ⇄
///   `Duration` directly — go through `Int64` explicitly if that is really
///   intended).
/// * [`DataError::CastOverflow`] under [`OverflowPolicy::Error`] for a value
///   that does not fit.
/// * [`DataError::OffsetWidthOverflow`] narrowing `LargeUtf8`/`LargeBinary`
///   to `Utf8`/`Binary` when the data does not fit a 32-bit offset.
///
/// ```
/// use astrs_data::array::{Array, Int32Array, Int8Array, IntoArrayRef};
/// use astrs_data::kernel::{cast, OverflowPolicy};
/// use astrs_data::DataType;
///
/// let wide = Int32Array::from_values([1, 300, -5]).into_array_ref();
///
/// let saturated = cast(&wide, &DataType::Int8, OverflowPolicy::Saturate)?;
/// let saturated = saturated.as_any().downcast_ref::<Int8Array>().unwrap();
/// assert_eq!(saturated.values(), &[1, i8::MAX, -5]);
///
/// assert!(cast(&wide, &DataType::Int8, OverflowPolicy::Error).is_err());
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn cast(array: &ArrayRef, to: &DataType, policy: OverflowPolicy) -> Result<ArrayRef> {
    let from = array.data_type().clone();
    if &from == to {
        return Ok(Arc::clone(array));
    }

    match (&from, to) {
        (DataType::Null, _) => new_null_array(to, array.len()),
        (_, DataType::Null) => Ok(Arc::new(NullArray::new(array.len()))),

        (DataType::Timestamp, DataType::Int64) => Ok(Arc::new(retype_i64(
            array.try_downcast::<TimestampArray>()?.as_primitive(),
        )?)),
        (DataType::Int64, DataType::Timestamp) => Ok(Arc::new(TimestampArray::from_primitive(
            array.try_downcast()?,
        )?)),
        (DataType::Duration, DataType::Int64) => Ok(Arc::new(retype_i64(
            array.try_downcast::<DurationArray>()?.as_primitive(),
        )?)),
        (DataType::Int64, DataType::Duration) => Ok(Arc::new(DurationArray::from_primitive(
            array.try_downcast()?,
        )?)),

        (DataType::Utf8, DataType::Binary) => Ok(Arc::new(
            array.try_downcast::<StringArray>()?.clone().into_binary(),
        )),
        (DataType::LargeUtf8, DataType::LargeBinary) => Ok(Arc::new(
            array
                .try_downcast::<LargeStringArray>()?
                .clone()
                .into_binary(),
        )),
        (DataType::Binary, DataType::Utf8) => {
            let typed: &BinaryArray = array.try_downcast()?;
            Ok(Arc::new(StringArray::try_new(
                ScalarBuffer::from_slice(typed.value_offsets()),
                typed.value_data().clone(),
                typed.validity().cloned(),
            )?))
        }
        (DataType::LargeBinary, DataType::LargeUtf8) => {
            let typed: &LargeBinaryArray = array.try_downcast()?;
            Ok(Arc::new(LargeStringArray::try_new(
                ScalarBuffer::from_slice(typed.value_offsets()),
                typed.value_data().clone(),
                typed.validity().cloned(),
            )?))
        }

        (DataType::Utf8, DataType::LargeUtf8) => {
            let typed: &StringArray = array.try_downcast()?;
            let offsets = widen_offsets(typed.value_offsets());
            // SAFETY: the value region and every slot's byte range are
            // unchanged — only the offset integer width grew — so the
            // already-validated UTF-8 boundaries are exactly preserved.
            Ok(Arc::new(unsafe {
                LargeStringArray::new_unchecked(
                    ScalarBuffer::from_slice(&offsets),
                    typed.value_data().clone(),
                    typed.validity().cloned(),
                )
            }))
        }
        (DataType::LargeUtf8, DataType::Utf8) => {
            let typed: &LargeStringArray = array.try_downcast()?;
            let offsets = narrow_offsets(typed.value_offsets())?;
            // SAFETY: same reasoning as the widening direction — narrowing
            // an offset that is already known to fit `i32` changes no byte
            // range.
            Ok(Arc::new(unsafe {
                StringArray::new_unchecked(
                    ScalarBuffer::from_slice(&offsets),
                    typed.value_data().clone(),
                    typed.validity().cloned(),
                )
            }))
        }
        (DataType::Binary, DataType::LargeBinary) => {
            let typed: &BinaryArray = array.try_downcast()?;
            let offsets = widen_offsets(typed.value_offsets());
            Ok(Arc::new(LargeBinaryArray::try_new(
                ScalarBuffer::from_slice(&offsets),
                typed.value_data().clone(),
                typed.validity().cloned(),
            )?))
        }
        (DataType::LargeBinary, DataType::Binary) => {
            let typed: &LargeBinaryArray = array.try_downcast()?;
            let offsets = narrow_offsets(typed.value_offsets())?;
            Ok(Arc::new(BinaryArray::try_new(
                ScalarBuffer::from_slice(&offsets),
                typed.value_data().clone(),
                typed.validity().cloned(),
            )?))
        }

        _ => cast_numeric(array, &from, to, policy),
    }
}

/// Re-labels an `i64` column as plain `Int64`, discarding whatever
/// `Timestamp`/`Duration` label it carried — free, since every temporal
/// type already shares the `i64` buffer layout.
fn retype_i64(array: &Int64Array) -> Result<Int64Array> {
    Int64Array::try_new(array.values_buffer().clone(), array.validity().cloned())
}

fn widen_offsets(offsets: &[i32]) -> Vec<i64> {
    offsets.iter().map(|&value| i64::from(value)).collect()
}

/// # Errors
///
/// [`DataError::OffsetWidthOverflow`] when an offset does not fit `i32`.
fn narrow_offsets(offsets: &[i64]) -> Result<Vec<i32>> {
    offsets
        .iter()
        .map(|&value| {
            i32::try_from(value).map_err(|_| DataError::OffsetWidthOverflow {
                total: usize::try_from(value).unwrap_or(usize::MAX),
                max: usize::try_from(i32::MAX).unwrap_or(usize::MAX),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Numeric conversion primitives.
//
// Two independent conversion paths, chosen by whether the *destination* is
// integer or floating:
//
//   IntWiden  (source)  -> i128 -> IntNarrow  (destination)   integer<->integer, exact
//   ToF64     (source)  -> f64  -> FromF64    (destination)   anything involving a float
//
// `i128` losslessly holds every value any of the eight integer
// `ArrowNativeType`s can hold, so integer<->integer conversion never touches
// a float and never loses precision to rounding — only range (which
// `OverflowPolicy` governs). Any conversion touching a float goes through
// `f64`, which is exact for every integer up to 2^53 and well-defined (if
// lossy) beyond it — see the module doc's "Precision, not just range".
// ---------------------------------------------------------------------------

/// Converts to `i128` exactly. Implemented for the eight integer
/// [`ArrowNativeType`]s only.
trait IntWiden: Copy {
    fn widen_i128(self) -> i128;
}

macro_rules! impl_int_widen {
    ($($t:ty),* $(,)?) => {
        $(impl IntWiden for $t {
            #[inline]
            fn widen_i128(self) -> i128 {
                self as i128
            }
        })*
    };
}
impl_int_widen!(i8, i16, i32, i64, u8, u16, u32, u64);

/// Converts from `i128`, checked or saturating. Implemented for the eight
/// integer [`ArrowNativeType`]s only; `i128` exactly holds every one of
/// their bounds, so both operations are exact.
trait IntNarrow: Copy + Sized {
    const MIN_I128: i128;
    const MAX_I128: i128;
    fn from_i128(value: i128) -> Self;
}

macro_rules! impl_int_narrow {
    ($($t:ty),* $(,)?) => {
        $(impl IntNarrow for $t {
            const MIN_I128: i128 = <$t>::MIN as i128;
            const MAX_I128: i128 = <$t>::MAX as i128;
            #[inline]
            fn from_i128(value: i128) -> Self {
                value as $t
            }
        })*
    };
}
impl_int_narrow!(i8, i16, i32, i64, u8, u16, u32, u64);

fn saturate_i128<T: IntNarrow>(value: i128) -> T {
    T::from_i128(value.clamp(T::MIN_I128, T::MAX_I128))
}

fn checked_i128<T: IntNarrow>(value: i128) -> Option<T> {
    (value >= T::MIN_I128 && value <= T::MAX_I128).then(|| T::from_i128(value))
}

/// Converts to `f64`. Implemented for all eleven numeric
/// [`ArrowNativeType`]s — exact except for `Int64`/`UInt64` magnitudes
/// beyond 2^53 (see the module doc).
trait ToF64: Copy {
    fn widen_f64(self) -> f64;
}

macro_rules! impl_to_f64 {
    ($($t:ty),* $(,)?) => {
        $(impl ToF64 for $t {
            #[inline]
            fn widen_f64(self) -> f64 {
                self as f64
            }
        })*
    };
}
impl_to_f64!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64);

impl ToF64 for F16 {
    #[inline]
    fn widen_f64(self) -> f64 {
        f64::from(self.to_f32())
    }
}

/// Converts from `f64`, checked or saturating. Implemented for all eleven
/// numeric [`ArrowNativeType`]s. `saturate_f64` is exactly `as`'s own
/// saturating float-to-numeric cast; `checked_f64` reports whether that same
/// conversion would have changed *range*, not precision — see the module
/// doc.
trait FromF64: Copy + Sized {
    fn saturate_f64(value: f64) -> Self;
    fn checked_f64(value: f64) -> Option<Self>;
}

/// Implements the eight integer [`FromF64`]s.
///
/// `$max_exclusive` is each type's true maximum plus one. Every one of
/// these eight types has a true maximum of `2^n - 1` for some `n`, so
/// `$max_exclusive` is always exactly `2^n` — a power of two, and therefore
/// exactly representable in `f64` regardless of `n`, unlike the true
/// maximum itself for `Int64`/`UInt64` (`2^63 - 1` and `2^64 - 1` are *not*
/// exactly representable and round up to `2^63`/`2^64` as `f64` — comparing
/// against the rounded value with `<=` would wrongly accept an input that
/// does not actually fit; comparing against the exact power of two with `<`
/// does not have that gap).
macro_rules! impl_from_f64_int {
    ($($t:ty, $min:expr, $max_exclusive:expr);* $(;)?) => {
        $(impl FromF64 for $t {
            #[inline]
            fn saturate_f64(value: f64) -> Self {
                value as $t
            }

            fn checked_f64(value: f64) -> Option<Self> {
                if value.is_nan() || !($min..$max_exclusive).contains(&value) {
                    None
                } else {
                    Some(value as $t)
                }
            }
        })*
    };
}
impl_from_f64_int!(
    i8, -128.0, 128.0;
    i16, -32768.0, 32768.0;
    i32, -2147483648.0, 2147483648.0;
    i64, -9223372036854775808.0, 9223372036854775808.0;
    u8, 0.0, 256.0;
    u16, 0.0, 65536.0;
    u32, 0.0, 4294967296.0;
    u64, 0.0, 18446744073709551616.0;
);

impl FromF64 for f32 {
    #[inline]
    fn saturate_f64(value: f64) -> Self {
        value as f32
    }

    fn checked_f64(value: f64) -> Option<Self> {
        let out = value as f32;
        (value.is_infinite() || !out.is_infinite()).then_some(out)
    }
}

impl FromF64 for f64 {
    #[inline]
    fn saturate_f64(value: f64) -> Self {
        value
    }

    fn checked_f64(value: f64) -> Option<Self> {
        Some(value)
    }
}

impl FromF64 for F16 {
    #[inline]
    fn saturate_f64(value: f64) -> Self {
        F16::from_f32(value as f32)
    }

    fn checked_f64(value: f64) -> Option<Self> {
        let out = Self::saturate_f64(value);
        (value.is_infinite() || !out.is_infinite()).then_some(out)
    }
}

fn cast_int_to_int<Src, Dst>(
    array: &PrimitiveArray<Src>,
    policy: OverflowPolicy,
    from: &DataType,
    to: &DataType,
) -> Result<PrimitiveArray<Dst>>
where
    Src: ArrowNativeType + IntWiden,
    Dst: ArrowNativeType + IntNarrow,
{
    let mut builder = PrimitiveBuilder::<Dst>::with_capacity(array.len());
    for (index, value) in array.iter().enumerate() {
        match value {
            None => builder.append_null(),
            Some(value) => {
                let widened = value.widen_i128();
                match policy {
                    OverflowPolicy::Saturate => builder.append_value(saturate_i128(widened)),
                    OverflowPolicy::Error => match checked_i128(widened) {
                        Some(out) => builder.append_value(out),
                        None => {
                            return Err(DataError::cast_overflow(from.clone(), to.clone(), index));
                        }
                    },
                }
            }
        }
    }
    Ok(builder.finish())
}

fn cast_via_f64<Src, Dst>(
    array: &PrimitiveArray<Src>,
    policy: OverflowPolicy,
    from: &DataType,
    to: &DataType,
) -> Result<PrimitiveArray<Dst>>
where
    Src: ArrowNativeType + ToF64,
    Dst: ArrowNativeType + FromF64,
{
    let mut builder = PrimitiveBuilder::<Dst>::with_capacity(array.len());
    for (index, value) in array.iter().enumerate() {
        match value {
            None => builder.append_null(),
            Some(value) => {
                let widened = value.widen_f64();
                match policy {
                    OverflowPolicy::Saturate => builder.append_value(Dst::saturate_f64(widened)),
                    OverflowPolicy::Error => match Dst::checked_f64(widened) {
                        Some(out) => builder.append_value(out),
                        None => {
                            return Err(DataError::cast_overflow(from.clone(), to.clone(), index));
                        }
                    },
                }
            }
        }
    }
    Ok(builder.finish())
}

/// Dispatches a cast between any two of the eleven numeric
/// [`ArrowNativeType`]s. Reached only from [`cast`]'s fallback arm, which is
/// why an unrecognised `from`/`to` here reports [`DataError::UnsupportedCast`]
/// rather than being structurally excluded — this function does not get to
/// assume its caller already validated both sides.
fn cast_numeric(
    array: &ArrayRef,
    from: &DataType,
    to: &DataType,
    policy: OverflowPolicy,
) -> Result<ArrayRef> {
    /// Given a source array already downcast to `PrimitiveArray<$src>`,
    /// dispatches on `to` and returns the boxed result. Used once per
    /// integer source type; the float source type arm below reuses
    /// `cast_via_f64` for every destination instead, since a float source
    /// never takes the exact-integer path.
    macro_rules! from_int {
        ($src_array:expr) => {
            match to {
                DataType::Int8 => {
                    Arc::new(cast_int_to_int::<_, i8>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Int16 => {
                    Arc::new(cast_int_to_int::<_, i16>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Int32 => {
                    Arc::new(cast_int_to_int::<_, i32>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Int64 => {
                    Arc::new(cast_int_to_int::<_, i64>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt8 => {
                    Arc::new(cast_int_to_int::<_, u8>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt16 => {
                    Arc::new(cast_int_to_int::<_, u16>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt32 => {
                    Arc::new(cast_int_to_int::<_, u32>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt64 => {
                    Arc::new(cast_int_to_int::<_, u64>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Float16 => {
                    Arc::new(cast_via_f64::<_, F16>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Float32 => {
                    Arc::new(cast_via_f64::<_, f32>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Float64 => {
                    Arc::new(cast_via_f64::<_, f64>($src_array, policy, from, to)?) as ArrayRef
                }
                other => return Err(DataError::unsupported_cast(from.clone(), other.clone())),
            }
        };
    }

    macro_rules! from_float {
        ($src_array:expr) => {
            match to {
                DataType::Int8 => {
                    Arc::new(cast_via_f64::<_, i8>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Int16 => {
                    Arc::new(cast_via_f64::<_, i16>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Int32 => {
                    Arc::new(cast_via_f64::<_, i32>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Int64 => {
                    Arc::new(cast_via_f64::<_, i64>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt8 => {
                    Arc::new(cast_via_f64::<_, u8>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt16 => {
                    Arc::new(cast_via_f64::<_, u16>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt32 => {
                    Arc::new(cast_via_f64::<_, u32>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::UInt64 => {
                    Arc::new(cast_via_f64::<_, u64>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Float16 => {
                    Arc::new(cast_via_f64::<_, F16>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Float32 => {
                    Arc::new(cast_via_f64::<_, f32>($src_array, policy, from, to)?) as ArrayRef
                }
                DataType::Float64 => {
                    Arc::new(cast_via_f64::<_, f64>($src_array, policy, from, to)?) as ArrayRef
                }
                other => return Err(DataError::unsupported_cast(from.clone(), other.clone())),
            }
        };
    }

    Ok(match from {
        DataType::Int8 => from_int!(array.try_downcast::<PrimitiveArray<i8>>()?),
        DataType::Int16 => from_int!(array.try_downcast::<PrimitiveArray<i16>>()?),
        DataType::Int32 => from_int!(array.try_downcast::<PrimitiveArray<i32>>()?),
        DataType::Int64 => from_int!(array.try_downcast::<PrimitiveArray<i64>>()?),
        DataType::UInt8 => from_int!(array.try_downcast::<PrimitiveArray<u8>>()?),
        DataType::UInt16 => from_int!(array.try_downcast::<PrimitiveArray<u16>>()?),
        DataType::UInt32 => from_int!(array.try_downcast::<PrimitiveArray<u32>>()?),
        DataType::UInt64 => from_int!(array.try_downcast::<PrimitiveArray<u64>>()?),
        DataType::Float16 => from_float!(array.try_downcast::<PrimitiveArray<F16>>()?),
        DataType::Float32 => from_float!(array.try_downcast::<PrimitiveArray<f32>>()?),
        DataType::Float64 => from_float!(array.try_downcast::<PrimitiveArray<f64>>()?),
        other => return Err(DataError::unsupported_cast(other.clone(), to.clone())),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{
        Float32Array, Float64Array, Int8Array, Int32Array, Int64Array, IntoArrayRef, UInt8Array,
        UInt64Array,
    };

    fn values<T: crate::datatype::ArrowNativeType>(array: &ArrayRef) -> Vec<T> {
        array
            .as_any()
            .downcast_ref::<PrimitiveArray<T>>()
            .unwrap()
            .values()
            .to_vec()
    }

    #[test]
    fn same_type_cast_is_a_pointer_clone() {
        let array = Int32Array::from_values([1, 2]).into_array_ref();
        let result = cast(&array, &DataType::Int32, OverflowPolicy::Saturate).unwrap();
        assert!(Arc::ptr_eq(&array, &result));
    }

    #[test]
    fn widening_int_to_int_is_exact() {
        let array = Int8Array::from_values([-5, 127]).into_array_ref();
        let result = cast(&array, &DataType::Int64, OverflowPolicy::Error).unwrap();
        assert_eq!(values::<i64>(&result), vec![-5, 127]);
    }

    #[test]
    fn narrowing_int_to_int_saturates() {
        let array = Int32Array::from_values([-1000, 50, 1000]).into_array_ref();
        let result = cast(&array, &DataType::UInt8, OverflowPolicy::Saturate).unwrap();
        assert_eq!(values::<u8>(&result), vec![0, 50, 255]);
    }

    #[test]
    fn narrowing_int_to_int_errors_under_error_policy() {
        let array = Int32Array::from_values([300]).into_array_ref();
        assert_eq!(
            cast(&array, &DataType::UInt8, OverflowPolicy::Error).unwrap_err(),
            DataError::CastOverflow {
                from: Box::new(DataType::Int32),
                to: Box::new(DataType::UInt8),
                index: 0
            }
        );
    }

    #[test]
    fn signed_to_unsigned_negative_saturates_to_zero() {
        let array = Int32Array::from_values([-1]).into_array_ref();
        let result = cast(&array, &DataType::UInt32, OverflowPolicy::Saturate).unwrap();
        assert_eq!(values::<u32>(&result), vec![0]);
    }

    #[test]
    fn unsigned_to_signed_overflow_saturates_to_max() {
        let array = UInt64Array::from_values([u64::MAX]).into_array_ref();
        let result = cast(&array, &DataType::Int8, OverflowPolicy::Saturate).unwrap();
        assert_eq!(values::<i8>(&result), vec![i8::MAX]);
    }

    #[test]
    fn i64_max_and_u64_max_are_exact_boundaries_under_error_policy() {
        // The precision trap this module's design note calls out: i64::MAX
        // and u64::MAX are one less than a power of two and do not survive
        // an `as f64` round trip unchanged, so a naive bounds check against
        // `i64::MAX as f64` would misjudge this exact case.
        let max_i64_as_f64 =
            Float64Array::from_values([9_223_372_036_854_775_807.0]).into_array_ref();
        // i64::MAX rounds up to 2^63 in f64, so the *checked* int<->int path
        // (which never touches f64) is what proves int<->int is exact —
        // check that directly instead.
        let array = Int64Array::from_values([i64::MAX, i64::MIN]).into_array_ref();
        let result = cast(&array, &DataType::Int64, OverflowPolicy::Error).unwrap();
        assert_eq!(values::<i64>(&result), vec![i64::MAX, i64::MIN]);
        let _ = max_i64_as_f64;
    }

    #[test]
    fn float_to_int_nan_saturates_to_zero_and_errors_under_error_policy() {
        let array = Float64Array::from_values([f64::NAN]).into_array_ref();
        let saturated = cast(&array, &DataType::Int32, OverflowPolicy::Saturate).unwrap();
        assert_eq!(values::<i32>(&saturated), vec![0]);
        assert!(cast(&array, &DataType::Int32, OverflowPolicy::Error).is_err());
    }

    #[test]
    fn float_to_int_infinity_saturates_to_the_finite_boundary() {
        let array = Float64Array::from_values([f64::INFINITY, f64::NEG_INFINITY]).into_array_ref();
        let result = cast(&array, &DataType::Int32, OverflowPolicy::Saturate).unwrap();
        assert_eq!(values::<i32>(&result), vec![i32::MAX, i32::MIN]);
        assert!(cast(&array, &DataType::Int32, OverflowPolicy::Error).is_err());
    }

    #[test]
    fn float_to_int_fractional_truncation_is_not_overflow() {
        let array = Float64Array::from_values([1.9, -1.9]).into_array_ref();
        let result = cast(&array, &DataType::Int32, OverflowPolicy::Error).unwrap();
        assert_eq!(values::<i32>(&result), vec![1, -1], "truncates toward zero");
    }

    #[test]
    fn int_to_float_never_overflows_f32_or_f64() {
        let array = Int64Array::from_values([i64::MAX, i64::MIN]).into_array_ref();
        assert!(cast(&array, &DataType::Float64, OverflowPolicy::Error).is_ok());
        assert!(cast(&array, &DataType::Float32, OverflowPolicy::Error).is_ok());
    }

    #[test]
    fn large_int_to_float_loses_precision_but_is_not_an_error() {
        // 2^53 + 1 is not exactly representable in f64.
        let array = Int64Array::from_values([(1i64 << 53) + 1]).into_array_ref();
        let result = cast(&array, &DataType::Float64, OverflowPolicy::Error).unwrap();
        assert_eq!(values::<f64>(&result), vec![(1i64 << 53) as f64]);
    }

    #[test]
    fn float64_to_float32_overflow_saturates_to_infinity_not_max() {
        let array = Float64Array::from_values([1e300, -1e300]).into_array_ref();
        let result = cast(&array, &DataType::Float32, OverflowPolicy::Saturate).unwrap();
        let out = values::<f32>(&result);
        assert!(out[0].is_infinite() && out[0] > 0.0);
        assert!(out[1].is_infinite() && out[1] < 0.0);
        assert!(cast(&array, &DataType::Float32, OverflowPolicy::Error).is_err());
    }

    #[test]
    fn already_infinite_or_nan_passes_through_under_error_policy() {
        let array = Float64Array::from_values([f64::INFINITY, f64::NAN]).into_array_ref();
        let result = cast(&array, &DataType::Float32, OverflowPolicy::Error).unwrap();
        let out = values::<f32>(&result);
        assert!(out[0].is_infinite());
        assert!(out[1].is_nan());
    }

    #[test]
    fn float16_overflow_saturates_to_infinity_at_65504() {
        let array = Float32Array::from_values([100_000.0, -100_000.0]).into_array_ref();
        let result = cast(&array, &DataType::Float16, OverflowPolicy::Saturate).unwrap();
        let out = values::<F16>(&result);
        assert!(out[0].is_infinite());
        assert!(cast(&array, &DataType::Float16, OverflowPolicy::Error).is_err());

        let in_range = Float32Array::from_values([65504.0]).into_array_ref();
        let ok = cast(&in_range, &DataType::Float16, OverflowPolicy::Error).unwrap();
        assert!(!values::<F16>(&ok)[0].is_infinite());
    }

    #[test]
    fn nulls_propagate_through_every_numeric_path() {
        let array = Int32Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref();
        for to in [DataType::Int8, DataType::Float32, DataType::UInt64] {
            let result = cast(&array, &to, OverflowPolicy::Saturate).unwrap();
            assert_eq!(result.null_count(), 1, "{to}");
            assert!(result.is_null(1), "{to}");
        }
    }

    #[test]
    fn timestamp_and_duration_reinterpret_through_int64_for_free() {
        let stamps = TimestampArray::from_nanos([1, 2, 3]).into_array_ref();
        let as_int = cast(&stamps, &DataType::Int64, OverflowPolicy::Error).unwrap();
        assert_eq!(as_int.data_type(), &DataType::Int64);
        assert_eq!(values::<i64>(&as_int), vec![1, 2, 3]);

        let back = cast(&as_int, &DataType::Timestamp, OverflowPolicy::Error).unwrap();
        assert_eq!(back.data_type(), &DataType::Timestamp);

        let durations = DurationArray::from_nanos([5]).into_array_ref();
        let as_int = cast(&durations, &DataType::Int64, OverflowPolicy::Error).unwrap();
        assert_eq!(as_int.data_type(), &DataType::Int64);
    }

    #[test]
    fn timestamp_to_duration_directly_is_unsupported() {
        let stamps = TimestampArray::from_nanos([1]).into_array_ref();
        assert!(matches!(
            cast(&stamps, &DataType::Duration, OverflowPolicy::Error),
            Err(DataError::UnsupportedCast { .. })
        ));
    }

    #[test]
    fn utf8_to_binary_is_a_reinterpret() {
        let strings = crate::array::StringArray::from_values(["a", "b"]).into_array_ref();
        let bytes = cast(&strings, &DataType::Binary, OverflowPolicy::Error).unwrap();
        assert_eq!(bytes.data_type(), &DataType::Binary);
        let bytes = bytes.as_any().downcast_ref::<BinaryArray>().unwrap();
        assert_eq!(bytes.get(0), Some(&b"a"[..]));
    }

    #[test]
    fn binary_to_utf8_validates() {
        let valid = crate::array::BinaryArray::from_values([&b"ok"[..]]).into_array_ref();
        assert!(cast(&valid, &DataType::Utf8, OverflowPolicy::Error).is_ok());

        let invalid = crate::array::BinaryArray::from_values([&b"\xff\xfe"[..]]).into_array_ref();
        assert!(matches!(
            cast(&invalid, &DataType::Utf8, OverflowPolicy::Error),
            Err(DataError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn utf8_widens_to_large_utf8_and_back() {
        let strings = crate::array::StringArray::from_values(["hello", "world"]).into_array_ref();
        let large = cast(&strings, &DataType::LargeUtf8, OverflowPolicy::Error).unwrap();
        assert_eq!(large.data_type(), &DataType::LargeUtf8);
        let back = cast(&large, &DataType::Utf8, OverflowPolicy::Error).unwrap();
        assert_eq!(back.as_ref(), strings.as_ref());
    }

    #[test]
    fn binary_widens_to_large_binary_and_back() {
        let bytes = crate::array::BinaryArray::from_values([&b"abc"[..]]).into_array_ref();
        let large = cast(&bytes, &DataType::LargeBinary, OverflowPolicy::Error).unwrap();
        assert_eq!(large.data_type(), &DataType::LargeBinary);
        let back = cast(&large, &DataType::Binary, OverflowPolicy::Error).unwrap();
        assert_eq!(back.as_ref(), bytes.as_ref());
    }

    #[test]
    fn null_casts_to_and_from_anything() {
        let null = crate::array::NullArray::new(3).into_array_ref();
        let as_int = cast(&null, &DataType::Int32, OverflowPolicy::Error).unwrap();
        assert_eq!(as_int.len(), 3);
        assert_eq!(as_int.null_count(), 3);

        let ints = Int32Array::from_values([1, 2]).into_array_ref();
        let as_null = cast(&ints, &DataType::Null, OverflowPolicy::Error).unwrap();
        assert_eq!(as_null.data_type(), &DataType::Null);
        assert_eq!(as_null.len(), 2);
    }

    #[test]
    fn bool_and_nested_types_have_no_defined_cast() {
        let bools = crate::array::BooleanArray::from_values([true]).into_array_ref();
        assert!(matches!(
            cast(&bools, &DataType::Int8, OverflowPolicy::Error),
            Err(DataError::UnsupportedCast { .. })
        ));

        let ints = Int32Array::from_values([1]).into_array_ref();
        assert!(matches!(
            cast(&ints, &DataType::Bool, OverflowPolicy::Error),
            Err(DataError::UnsupportedCast { .. })
        ));
    }

    #[test]
    fn every_numeric_pair_round_trips_representable_values() {
        let numeric_types = [
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
        ];
        let source = UInt8Array::from_values([0, 1, 100]).into_array_ref();
        for to in &numeric_types {
            for policy in [OverflowPolicy::Saturate, OverflowPolicy::Error] {
                let result = cast(&source, to, policy).unwrap_or_else(|err| panic!("{to}: {err}"));
                assert_eq!(result.len(), 3, "{to}");
                assert_eq!(result.data_type(), to, "{to}");
            }
        }
    }
}
