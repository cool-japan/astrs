//! `std/core/v1` and `std/time/v1` — the scalar channels (§24.3).
//!
//! The simplest useful message there is: a single value, or a run of them, in
//! one unnested column. A [`Scalar<f64>`](Scalar) message is a one-row
//! `Float64` column; a [`ScalarRun<f64>`](ScalarRun) is an *n*-row one. Both
//! declare the same URN, because the URN names the element type and the row
//! count is data — which is what lets a node batch a hundred readings into one
//! message without changing its port's declared type.
//!
//! | Rust | URN | Layout |
//! |---|---|---|
//! | [`Scalar<T>`] / [`ScalarRun<T>`] | `std/core/v1/{Int,UInt,Float}N` | the matching primitive |
//! | [`Flag`] / [`FlagRun`] | `std/core/v1/Bool` | `Bool` |
//! | [`Text`] / [`TextRun`] | `std/core/v1/String` | `Utf8` |
//! | [`Bytes`] / [`BytesRun`] | `std/core/v1/Bytes` | `Binary` |
//! | [`Empty`] | `std/core/v1/Empty` | `Null` |
//! | [`Timestamp`] / [`TimestampRun`] | `std/time/v1/Timestamp` | `Timestamp(ns)` |
//! | [`Duration`] | `std/time/v1/Duration` | `Duration(ns)` |
//!
//! # Why newtypes rather than `impl AstrsMessage for f64`
//!
//! [`AstrsMessage`] is declared in `astrs-data` (it has to be — see that
//! crate's `message` module), and `f64` is declared in `core`. Rust's orphan
//! rule forbids this crate from pairing two foreign items, so the primitives
//! reach the trait through a local wrapper. The wrapper is not friction in
//! practice: every one of these types is `From` its inner value, and the typed
//! output handle takes `impl Into<T>`, so a node writes `speed.send(2.5, …)`
//! and never names [`Scalar`] at all.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{AstrsMessage, Bytes, Scalar, ScalarRun, Timestamp};
//!
//! let batch = Scalar::from(2.5_f64).to_record_batch()?;
//! assert_eq!(Scalar::<f64>::from_record_batch(&batch)?.into_inner(), 2.5);
//!
//! let many = ScalarRun::from(vec![1.0_f64, 2.0, 3.0]).to_record_batch()?;
//! assert_eq!(many.num_rows(), 3);
//!
//! assert_eq!(Bytes::URN, "std/core/v1/Bytes");
//! assert_eq!(Timestamp::from_nanos(7).nanos(), 7);
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::array::{
    Array, ArrayExt, ArrayRef, BinaryArray, BooleanArray, DurationArray, IntoArrayRef, NullArray,
    StringArray, TimestampArray,
};
use astrs_data::{ArrowNativeType, AstrsMessage, DataError, DataType, RecordBatch, Result};

use super::{build, payload_column, read, single_row_column};

/// A primitive with a `std/core/v1` URN.
///
/// The bridge between `astrs-data`'s sealed [`ArrowNativeType`] and the §24.3
/// registry: each primitive knows which scalar channel it *is*.
///
/// # Examples
///
/// ```
/// use astrs_node_api::message::scalar::ScalarType;
///
/// assert_eq!(<f32 as ScalarType>::SCALAR_URN, "std/core/v1/Float32");
/// assert_eq!(<u16 as ScalarType>::SCALAR_URN, "std/core/v1/UInt16");
/// ```
pub trait ScalarType: ArrowNativeType {
    /// The `std/core/v1` URN of this element type.
    const SCALAR_URN: &'static str;
}

/// Declares a primitive's scalar channel.
macro_rules! scalar_type {
    ($ty:ty, $urn:literal) => {
        impl ScalarType for $ty {
            const SCALAR_URN: &'static str = $urn;
        }
    };
}

scalar_type!(i8, "std/core/v1/Int8");
scalar_type!(i16, "std/core/v1/Int16");
scalar_type!(i32, "std/core/v1/Int32");
scalar_type!(i64, "std/core/v1/Int64");
scalar_type!(u8, "std/core/v1/UInt8");
scalar_type!(u16, "std/core/v1/UInt16");
scalar_type!(u32, "std/core/v1/UInt32");
scalar_type!(u64, "std/core/v1/UInt64");
scalar_type!(f32, "std/core/v1/Float32");
scalar_type!(f64, "std/core/v1/Float64");
scalar_type!(astrs_data::F16, "std/core/v1/Float16");

/// One primitive reading — a one-row `std/core/v1` channel message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scalar<T: ScalarType>(pub T);

impl<T: ScalarType> Scalar<T> {
    /// Wraps a value.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Unwraps the value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }

    /// The value.
    #[must_use]
    pub const fn get(&self) -> &T {
        &self.0
    }
}

impl<T: ScalarType> From<T> for Scalar<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T: ScalarType> AstrsMessage for Scalar<T> {
    const URN: &'static str = T::SCALAR_URN;

    fn data_type() -> DataType {
        T::DATA_TYPE
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::primitive::<T>(
            core::slice::from_ref(&self.0),
        )))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(read::primitive_at::<T>(single_row_column(batch)?, 0)?))
    }
}

impl<T: ScalarType> super::FromPayload for Scalar<T> {
    fn from_batch(batch: &RecordBatch) -> Result<Self> {
        Self::from_record_batch(batch)
    }
}

/// A run of primitive readings — an *n*-row `std/core/v1` channel message.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScalarRun<T: ScalarType>(pub Vec<T>);

impl<T: ScalarType> ScalarRun<T> {
    /// Wraps a vector of values.
    #[must_use]
    pub const fn new(values: Vec<T>) -> Self {
        Self(values)
    }

    /// The values.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// Unwraps the vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }

    /// How many readings the run holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the run is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: ScalarType> From<Vec<T>> for ScalarRun<T> {
    fn from(values: Vec<T>) -> Self {
        Self(values)
    }
}

impl<T: ScalarType> AstrsMessage for ScalarRun<T> {
    const URN: &'static str = T::SCALAR_URN;

    fn data_type() -> DataType {
        T::DATA_TYPE
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::primitive::<T>(&self.0)))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = payload_column(batch)?;
        (0..column.len())
            .map(|row| read::primitive_at::<T>(column, row))
            .collect::<Result<Vec<T>>>()
            .map(Self)
    }
}

impl<T: ScalarType> super::FromPayload for ScalarRun<T> {
    fn from_batch(batch: &RecordBatch) -> Result<Self> {
        Self::from_record_batch(batch)
    }
}

/// One boolean reading — `std/core/v1/Bool`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Flag(pub bool);

impl Flag {
    /// The value.
    #[must_use]
    pub const fn get(self) -> bool {
        self.0
    }
}

impl From<bool> for Flag {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

impl AstrsMessage for Flag {
    const URN: &'static str = "std/core/v1/Bool";

    fn data_type() -> DataType {
        DataType::Bool
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::boolean(&[self.0])))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(read::bool_at(single_row_column(batch)?, 0)?))
    }
}

super::impl_from_payload!(Flag);

/// A run of boolean readings — `std/core/v1/Bool`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlagRun(pub Vec<bool>);

impl From<Vec<bool>> for FlagRun {
    fn from(values: Vec<bool>) -> Self {
        Self(values)
    }
}

impl AstrsMessage for FlagRun {
    const URN: &'static str = "std/core/v1/Bool";

    fn data_type() -> DataType {
        DataType::Bool
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::boolean(&self.0)))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(booleans_of(batch)?))
    }
}

super::impl_from_payload!(FlagRun);

/// One UTF-8 string — `std/core/v1/String`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Text(pub String);

impl Text {
    /// The string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for Text {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Text {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl AstrsMessage for Text {
    const URN: &'static str = "std/core/v1/String";

    fn data_type() -> DataType {
        DataType::Utf8
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::strings(&[&self.0])))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(read::str_at(single_row_column(batch)?, 0)?.to_owned()))
    }
}

super::impl_from_payload!(Text);

/// A run of UTF-8 strings — `std/core/v1/String`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextRun(pub Vec<String>);

impl From<Vec<String>> for TextRun {
    fn from(values: Vec<String>) -> Self {
        Self(values)
    }
}

impl AstrsMessage for TextRun {
    const URN: &'static str = "std/core/v1/String";

    fn data_type() -> DataType {
        DataType::Utf8
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::strings(&self.0)))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(strings_of(batch)?))
    }
}

super::impl_from_payload!(TextRun);

/// Opaque bytes — `std/core/v1/Bytes`.
///
/// One blob is **one row**, in contrast to `ScalarRun<u8>`, where one sample
/// is one row. Saying which of the two a port carries is the whole point of
/// having both.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    /// Wraps a byte vector.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Unwraps the byte vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AstrsMessage for Bytes {
    const URN: &'static str = "std/core/v1/Bytes";

    fn data_type() -> DataType {
        DataType::Binary
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::binaries(&[&self.0])))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(read::bytes_at(single_row_column(batch)?, 0)?.to_vec()))
    }
}

super::impl_from_payload!(Bytes);

/// A run of opaque blobs — `std/core/v1/Bytes`, one blob per row.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BytesRun(pub Vec<Vec<u8>>);

impl From<Vec<Vec<u8>>> for BytesRun {
    fn from(values: Vec<Vec<u8>>) -> Self {
        Self(values)
    }
}

impl AstrsMessage for BytesRun {
    const URN: &'static str = "std/core/v1/Bytes";

    fn data_type() -> DataType {
        DataType::Binary
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::binaries(&self.0)))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(blobs_of(batch)?))
    }
}

super::impl_from_payload!(BytesRun);

/// A signal with no payload — `std/core/v1/Empty`.
///
/// The columnar layout is `Null`, whose row carries no bytes at all; a
/// heartbeat or a trigger edge is exactly this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Empty;

impl AstrsMessage for Empty {
    const URN: &'static str = "std/core/v1/Empty";

    fn data_type() -> DataType {
        DataType::Null
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(
            NullArray::new(1).into_array_ref(),
        ))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        if column.data_type() == &DataType::Null {
            Ok(Self)
        } else {
            Err(DataError::type_mismatch(
                DataType::Null,
                column.data_type().clone(),
            ))
        }
    }
}

super::impl_from_payload!(Empty);

/// A point in time — `std/time/v1/Timestamp`, nanoseconds since the Unix
/// epoch, timezone-less.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    /// Nanoseconds since the Unix epoch.
    pub nanos: i64,
}

impl Timestamp {
    /// A timestamp at `nanos` since the epoch.
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self { nanos }
    }

    /// The value in nanoseconds.
    #[must_use]
    pub const fn nanos(self) -> i64 {
        self.nanos
    }

    /// The whole seconds since the epoch, rounding towards negative infinity
    /// so a pre-epoch stamp's sub-second part stays non-negative.
    #[must_use]
    pub const fn seconds(self) -> i64 {
        self.nanos.div_euclid(1_000_000_000)
    }

    /// The nanoseconds past [`Timestamp::seconds`], always in `0..1e9`.
    #[must_use]
    pub fn subsec_nanos(self) -> u32 {
        // `rem_euclid` with a positive divisor lands in `0..1e9`, which always
        // fits a `u32`; the fallback keeps the conversion total.
        u32::try_from(self.nanos.rem_euclid(1_000_000_000)).unwrap_or(0)
    }

    /// The matching [`astrs_time::HlcTimestamp`] reading, with `counter` as
    /// its logical component.
    #[must_use]
    pub fn to_hlc(self, counter: u32) -> astrs_time::HlcTimestamp {
        astrs_time::HlcTimestamp::new(self.nanos.max(0).unsigned_abs(), counter)
    }

    /// The timestamp an [`astrs_time::HlcTimestamp`] names.
    #[must_use]
    pub fn from_hlc(hlc: astrs_time::HlcTimestamp) -> Self {
        Self::from_nanos(i64::try_from(hlc.physical_ns()).unwrap_or(i64::MAX))
    }
}

impl From<i64> for Timestamp {
    fn from(nanos: i64) -> Self {
        Self::from_nanos(nanos)
    }
}

impl AstrsMessage for Timestamp {
    const URN: &'static str = "std/time/v1/Timestamp";

    fn data_type() -> DataType {
        DataType::Timestamp
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(build::timestamps(&[self.nanos])))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self::from_nanos(read::timestamp_at(
            single_row_column(batch)?,
            0,
        )?))
    }
}

super::impl_from_payload!(Timestamp);

/// A run of timestamps — `std/time/v1/Timestamp`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimestampRun(pub Vec<Timestamp>);

impl From<Vec<Timestamp>> for TimestampRun {
    fn from(values: Vec<Timestamp>) -> Self {
        Self(values)
    }
}

impl AstrsMessage for TimestampRun {
    const URN: &'static str = "std/time/v1/Timestamp";

    fn data_type() -> DataType {
        DataType::Timestamp
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let nanos: Vec<i64> = self.0.iter().map(|stamp| stamp.nanos).collect();
        Ok(RecordBatch::from_payload(build::timestamps(&nanos)))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Ok(Self(
            timestamps_of(batch)?
                .into_iter()
                .map(Timestamp::from_nanos)
                .collect(),
        ))
    }
}

super::impl_from_payload!(TimestampRun);

/// A time interval — `std/time/v1/Duration`, in nanoseconds.
///
/// Signed, because "how far behind schedule" is as useful a reading as "how
/// far ahead".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration {
    /// The interval in nanoseconds.
    pub nanos: i64,
}

impl Duration {
    /// An interval of `nanos` nanoseconds.
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self { nanos }
    }

    /// An interval of `millis` milliseconds.
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self {
            nanos: millis.saturating_mul(1_000_000),
        }
    }

    /// The interval in nanoseconds.
    #[must_use]
    pub const fn nanos(self) -> i64 {
        self.nanos
    }

    /// Whether the interval runs backwards.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.nanos < 0
    }

    /// The interval as a [`core::time::Duration`], clamped at zero.
    #[must_use]
    pub const fn to_std(self) -> core::time::Duration {
        if self.nanos <= 0 {
            core::time::Duration::ZERO
        } else {
            core::time::Duration::from_nanos(self.nanos.unsigned_abs())
        }
    }
}

impl AstrsMessage for Duration {
    const URN: &'static str = "std/time/v1/Duration";

    fn data_type() -> DataType {
        DataType::Duration
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(
            DurationArray::from_nanos([self.nanos]).into_array_ref(),
        ))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let array = column.downcast::<DurationArray>().ok_or_else(|| {
            DataError::type_mismatch(DataType::Duration, column.data_type().clone())
        })?;
        let nanos = array
            .value(0)
            .ok_or(DataError::IndexOutOfBounds { index: 0, len: 0 })?;
        Ok(Self::from_nanos(nanos))
    }
}

super::impl_from_payload!(Duration);

/// The `"data"` column of `batch` as owned strings, whatever its row count.
///
/// # Errors
///
/// [`DataError`] when the column is not `Utf8`.
pub fn strings_of(batch: &RecordBatch) -> Result<Vec<String>> {
    let column = payload_column(batch)?;
    let array = column
        .downcast::<StringArray>()
        .ok_or_else(|| DataError::type_mismatch(DataType::Utf8, column.data_type().clone()))?;
    (0..array.len())
        .map(|row| {
            array
                .value(row)
                .map(str::to_owned)
                .ok_or(DataError::IndexOutOfBounds {
                    index: row,
                    len: array.len(),
                })
        })
        .collect()
}

/// The `"data"` column of `batch` as a boolean run.
///
/// # Errors
///
/// [`DataError`] when the column is not `Bool`.
pub fn booleans_of(batch: &RecordBatch) -> Result<Vec<bool>> {
    let column = payload_column(batch)?;
    let array = column
        .downcast::<BooleanArray>()
        .ok_or_else(|| DataError::type_mismatch(DataType::Bool, column.data_type().clone()))?;
    (0..array.len())
        .map(|row| {
            array.value(row).ok_or(DataError::IndexOutOfBounds {
                index: row,
                len: array.len(),
            })
        })
        .collect()
}

/// The `"data"` column of `batch` as a binary run.
///
/// # Errors
///
/// [`DataError`] when the column is not `Binary`.
pub fn blobs_of(batch: &RecordBatch) -> Result<Vec<Vec<u8>>> {
    let column = payload_column(batch)?;
    let array = column
        .downcast::<BinaryArray>()
        .ok_or_else(|| DataError::type_mismatch(DataType::Binary, column.data_type().clone()))?;
    (0..array.len())
        .map(|row| {
            array
                .value(row)
                .map(<[u8]>::to_vec)
                .ok_or(DataError::IndexOutOfBounds {
                    index: row,
                    len: array.len(),
                })
        })
        .collect()
}

/// The `"data"` column of `batch` as nanosecond timestamps.
///
/// # Errors
///
/// [`DataError`] when the column is not `Timestamp`.
pub fn timestamps_of(batch: &RecordBatch) -> Result<Vec<i64>> {
    let column = payload_column(batch)?;
    let array = column
        .downcast::<TimestampArray>()
        .ok_or_else(|| DataError::type_mismatch(DataType::Timestamp, column.data_type().clone()))?;
    (0..array.len())
        .map(|row| {
            array.value(row).ok_or(DataError::IndexOutOfBounds {
                index: row,
                len: array.len(),
            })
        })
        .collect()
}

/// The `"data"` column, whatever it is — the untyped escape hatch for a
/// `type: any` port (§3.7).
///
/// # Errors
///
/// [`DataError::ColumnCountMismatch`] when the batch carries no column.
pub fn any_column(batch: &RecordBatch) -> Result<ArrayRef> {
    Ok(std::sync::Arc::clone(payload_column(batch)?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::assert_registry_layout;

    #[test]
    fn every_numeric_scalar_round_trips_and_conforms() {
        macro_rules! check {
            ($ty:ty, $value:expr) => {{
                assert_registry_layout::<Scalar<$ty>>().unwrap();
                assert_registry_layout::<ScalarRun<$ty>>().unwrap();
                let value = Scalar::from($value);
                let batch = value.to_record_batch().unwrap();
                assert_eq!(batch.num_rows(), 1);
                assert_eq!(Scalar::<$ty>::from_record_batch(&batch).unwrap(), value);
                assert_eq!(*value.get(), $value);
                assert_eq!(value.into_inner(), $value);
            }};
        }
        check!(i8, -8);
        check!(i16, -16);
        check!(i32, -32);
        check!(i64, -64);
        check!(u8, 8);
        check!(u16, 16);
        check!(u32, 32);
        check!(u64, 64);
        check!(f32, 1.5);
        check!(f64, -2.5);
    }

    #[test]
    fn float16_has_a_channel_too() {
        assert_registry_layout::<Scalar<astrs_data::F16>>().unwrap();
        assert_eq!(
            <astrs_data::F16 as ScalarType>::SCALAR_URN,
            "std/core/v1/Float16"
        );
    }

    #[test]
    fn runs_round_trip_with_the_same_urn() {
        assert_eq!(
            <ScalarRun<f64> as AstrsMessage>::URN,
            <Scalar<f64> as AstrsMessage>::URN
        );
        let values = ScalarRun::from(vec![1.0_f64, 2.0, 3.0]);
        let batch = values.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(ScalarRun::<f64>::from_record_batch(&batch).unwrap(), values);
        assert_eq!(values.len(), 3);
        assert!(!values.is_empty());
        assert_eq!(values.as_slice()[1], 2.0);
        assert_eq!(values.clone().into_vec().len(), 3);
        assert!(
            Scalar::<f64>::from_record_batch(&batch).is_err(),
            "a scalar decode of a three-row batch is a mistake, not a truncation"
        );
    }

    #[test]
    fn flags_round_trip() {
        assert_registry_layout::<Flag>().unwrap();
        assert_registry_layout::<FlagRun>().unwrap();
        let batch = Flag::from(true).to_record_batch().unwrap();
        assert!(Flag::from_record_batch(&batch).unwrap().get());
        let run = FlagRun::from(vec![true, false, true])
            .to_record_batch()
            .unwrap();
        assert_eq!(
            FlagRun::from_record_batch(&run).unwrap().0,
            vec![true, false, true]
        );
        assert_eq!(booleans_of(&run).unwrap(), vec![true, false, true]);
    }

    #[test]
    fn text_round_trips() {
        assert_registry_layout::<Text>().unwrap();
        assert_registry_layout::<TextRun>().unwrap();
        let batch = Text::from("hello").to_record_batch().unwrap();
        assert_eq!(Text::from_record_batch(&batch).unwrap().as_str(), "hello");
        assert_eq!(Text::from("x".to_owned()).into_string(), "x");
        let run = TextRun::from(vec!["a".to_owned(), "bb".to_owned()])
            .to_record_batch()
            .unwrap();
        assert_eq!(TextRun::from_record_batch(&run).unwrap().0.len(), 2);
        assert_eq!(strings_of(&run).unwrap(), vec!["a", "bb"]);
    }

    #[test]
    fn bytes_are_one_row_and_u8_runs_are_many() {
        assert_registry_layout::<Bytes>().unwrap();
        assert_registry_layout::<BytesRun>().unwrap();
        let blob = Bytes::new(vec![1, 2, 3]);
        let batch = blob.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1, "one blob is one row");
        assert_eq!(Bytes::from_record_batch(&batch).unwrap(), blob);
        assert_eq!(blobs_of(&batch).unwrap(), vec![vec![1, 2, 3]]);

        let samples = ScalarRun::from(vec![1u8, 2, 3]).to_record_batch().unwrap();
        assert_eq!(samples.num_rows(), 3, "three samples are three rows");
        assert_ne!(
            <Bytes as AstrsMessage>::URN,
            <ScalarRun<u8> as AstrsMessage>::URN
        );

        assert_eq!(Bytes::from(vec![7u8]).as_slice(), &[7]);
        assert_eq!(Bytes::new(vec![7]).into_vec(), vec![7]);
        assert_eq!(Bytes::default().as_ref(), &[] as &[u8]);

        let run = BytesRun::from(vec![vec![1u8], vec![2, 3]])
            .to_record_batch()
            .unwrap();
        assert_eq!(BytesRun::from_record_batch(&run).unwrap().0.len(), 2);
    }

    #[test]
    fn empty_carries_nothing() {
        assert_registry_layout::<Empty>().unwrap();
        let batch = Empty.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(Empty::from_record_batch(&batch).unwrap(), Empty);
        let wrong = Scalar::from(1.0_f64).to_record_batch().unwrap();
        assert!(Empty::from_record_batch(&wrong).is_err());
    }

    #[test]
    fn timestamps_round_trip_and_split_correctly() {
        assert_registry_layout::<Timestamp>().unwrap();
        assert_registry_layout::<TimestampRun>().unwrap();
        let stamp = Timestamp::from_nanos(1_700_000_000_123_456_789);
        let batch = stamp.to_record_batch().unwrap();
        assert_eq!(Timestamp::from_record_batch(&batch).unwrap(), stamp);
        assert_eq!(stamp.seconds(), 1_700_000_000);
        assert_eq!(stamp.subsec_nanos(), 123_456_789);
        assert_eq!(stamp.nanos(), 1_700_000_000_123_456_789);
        assert_eq!(Timestamp::from(5_i64).nanos, 5);

        // Pre-epoch stamps keep a non-negative sub-second part.
        let before = Timestamp::from_nanos(-1);
        assert_eq!(before.seconds(), -1);
        assert_eq!(before.subsec_nanos(), 999_999_999);

        let run = TimestampRun::from(vec![stamp, before])
            .to_record_batch()
            .unwrap();
        assert_eq!(TimestampRun::from_record_batch(&run).unwrap().0.len(), 2);
        assert_eq!(timestamps_of(&run).unwrap().len(), 2);

        let hlc = stamp.to_hlc(3);
        assert_eq!(hlc.logical(), 3);
        assert_eq!(Timestamp::from_hlc(hlc), stamp);
    }

    #[test]
    fn durations_round_trip() {
        assert_registry_layout::<Duration>().unwrap();
        let value = Duration::from_millis(-250);
        assert_eq!(value.nanos(), -250_000_000);
        assert!(value.is_negative());
        assert_eq!(value.to_std(), core::time::Duration::ZERO);
        let batch = value.to_record_batch().unwrap();
        assert_eq!(Duration::from_record_batch(&batch).unwrap(), value);
        assert_eq!(
            Duration::from_nanos(1_500_000_000).to_std(),
            core::time::Duration::from_millis(1_500)
        );
    }

    #[test]
    fn a_wrong_column_type_is_reported_rather_than_reinterpreted() {
        let batch = Scalar::from(1.0_f64).to_record_batch().unwrap();
        assert!(Scalar::<i32>::from_record_batch(&batch).is_err());
        assert!(Text::from_record_batch(&batch).is_err());
        assert!(Bytes::from_record_batch(&batch).is_err());
        assert!(Timestamp::from_record_batch(&batch).is_err());
        assert!(Duration::from_record_batch(&batch).is_err());
        assert!(Flag::from_record_batch(&batch).is_err());
        assert!(strings_of(&batch).is_err());
        assert!(booleans_of(&batch).is_err());
        assert!(blobs_of(&batch).is_err());
        assert!(timestamps_of(&batch).is_err());
    }

    #[test]
    fn the_untyped_escape_hatch_returns_the_column() {
        let batch = ScalarRun::from(vec![1.0_f64, 2.0])
            .to_record_batch()
            .unwrap();
        let column = any_column(&batch).unwrap();
        assert_eq!(column.len(), 2);
        assert_eq!(column.data_type(), &DataType::Float64);
    }

    #[test]
    fn empty_runs_are_legal() {
        let batch = ScalarRun::<f64>::default().to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 0);
        assert!(
            ScalarRun::<f64>::from_record_batch(&batch)
                .unwrap()
                .is_empty()
        );
    }
}
