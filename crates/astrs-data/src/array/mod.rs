//! The array families and the [`Array`] trait that unifies them.
//!
//! # Why trait objects rather than a closed enum
//!
//! The task allowed either `Arc<dyn Array>` or a closed `ArrayEnum`. AstRS
//! uses the sealed trait plus [`ArrayRef`], for three reasons that all come
//! back to keeping zero-copy slicing simple:
//!
//! 1. **Slicing stays on the concrete type.** Every array has an inherent
//!    `slice(&self, offset, len) -> Self` that clones `Arc`s and adjusts
//!    windows — no allocation, no data type re-dispatch. The trait's
//!    [`Array::slice`] is a thin `Arc::new` wrapper over it. With a closed
//!    enum, every slice would go through a 23-arm match that reconstructs the
//!    variant, and generic code (`PrimitiveArray<T>`) could not be sliced
//!    without first widening it.
//! 2. **Generic families stay generic.** `PrimitiveArray<T>` covers eleven
//!    scalar types and `GenericStringArray<O>` covers two offset widths. An
//!    enum would have to enumerate all thirteen instantiations by hand, and
//!    downstream code written against `PrimitiveArray<i32>` would lose its
//!    static type on every round trip through the enum.
//! 3. **`Arc<dyn Array>` is already the sharing unit.** A record batch column,
//!    a list array's child and a struct array's field are all shared,
//!    refcounted values; `Arc<dyn Array>` clones in one atomic increment,
//!    while an enum in an `Arc` adds a layer or forces a large `memcpy`.
//!
//! The cost of the trait-object choice is that exhaustiveness is not checked
//! by the compiler: stage 2's IPC encoder matches on
//! [`Array::data_type`] and downcasts. The sealed supertrait
//! ([`crate::sealed::Sealed`]) keeps the set closed anyway — no downstream
//! crate can add a variant — and [`array_kinds`] lists every family so a test
//! can assert full coverage.
//!
//! Equality across trait objects works too, so [`ArrayRef`] and
//! `Vec<ArrayRef>` compare structurally and [`crate::RecordBatch`] derives
//! `PartialEq`:
//!
//! ```
//! use astrs_data::array::{Array, ArrayRef, Int32Array, IntoArrayRef};
//!
//! let a: ArrayRef = Int32Array::from_values([1, 2, 3]).into_array_ref();
//! let b: ArrayRef = Int32Array::from_values([1, 2, 3]).into_array_ref();
//! assert_eq!(a, b);
//! assert_ne!(a, Int32Array::from_values([1, 2]).into_array_ref());
//! ```
//!
//! # The slicing convention
//!
//! Every `slice` in this crate — on [`crate::Buffer`], [`crate::Bitmap`],
//! every array and [`crate::RecordBatch`] — **clamps** an out-of-range request
//! to the largest valid window instead of panicking, and every one of them has
//! a `try_slice` counterpart that reports
//! [`crate::DataError::SliceOutOfBounds`] instead. See [`Array::slice`].

pub mod binary;
pub mod boolean;
pub mod fixed_size_binary;
pub mod fixed_size_list;
pub mod iter;
pub mod list;
pub mod null;
pub mod offset;
pub mod primitive;
pub mod string;
pub mod structs;
pub mod temporal;

use std::any::Any;
use std::sync::Arc;

pub use crate::array::binary::{BinaryArray, GenericBinaryArray, LargeBinaryArray};
pub use crate::array::boolean::BooleanArray;
pub use crate::array::fixed_size_binary::FixedSizeBinaryArray;
pub use crate::array::fixed_size_list::FixedSizeListArray;
pub use crate::array::iter::{ArrayAccessor, ArrayIter};
pub use crate::array::list::ListArray;
pub use crate::array::null::NullArray;
pub use crate::array::offset::{OffsetSizeTrait, validate_offsets};
pub use crate::array::primitive::{
    Float16Array, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array,
    PrimitiveArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
pub use crate::array::string::{GenericStringArray, LargeStringArray, StringArray};
pub use crate::array::structs::StructArray;
pub use crate::array::temporal::{DurationArray, TimestampArray};

use crate::buffer::Bitmap;
use crate::datatype::{DataType, Field};
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A shared, immutable, dynamically typed column.
///
/// This is the unit of sharing everywhere in AstRS: a record batch column, a
/// list's child, a struct's field. Cloning is one atomic increment.
pub type ArrayRef = Arc<dyn Array>;

/// The behaviour every array family provides.
///
/// Sealed: the implementor set is fixed by this crate (see [`array_kinds`]).
pub trait Array: std::fmt::Debug + Send + Sync + Sealed + 'static {
    /// Erased self, for [`ArrayExt::downcast`].
    fn as_any(&self) -> &dyn Any;

    /// The array's layout.
    fn data_type(&self) -> &DataType;

    /// Number of logical slots.
    fn len(&self) -> usize;

    /// Returns `true` when the array has no slots.
    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The validity bitmap, or `None` when every slot is valid.
    ///
    /// Arrow calls this the *null buffer*; here `1` means valid, matching the
    /// bit convention in [`crate::buffer::bitmap`].
    fn validity(&self) -> Option<&Bitmap>;

    /// Number of null slots.
    #[inline]
    fn null_count(&self) -> usize {
        self.validity().map_or(0, Bitmap::count_unset)
    }

    /// Returns `true` when slot `index` is null. Out-of-range indices are
    /// reported as null.
    #[inline]
    fn is_null(&self, index: usize) -> bool {
        !self.is_valid(index)
    }

    /// Returns `true` when slot `index` holds a value.
    #[inline]
    fn is_valid(&self, index: usize) -> bool {
        index < self.len() && self.validity().is_none_or(|bits| bits.value(index))
    }

    /// A zero-copy sub-range.
    ///
    /// **Clamping is the crate-wide convention**: `offset` beyond the end
    /// yields an empty array, and `len` beyond the end is truncated. Nothing
    /// panics. Use [`try_slice`] when an out-of-range request is a bug worth
    /// reporting.
    ///
    /// ```
    /// use astrs_data::array::{Array, Int32Array};
    ///
    /// let values = Int32Array::from_values([1, 2, 3, 4]);
    /// assert_eq!(Array::slice(&values, 1, 2).len(), 2);
    /// assert_eq!(Array::slice(&values, 3, 99).len(), 1);
    /// assert_eq!(Array::slice(&values, 99, 1).len(), 0);
    /// ```
    fn slice(&self, offset: usize, len: usize) -> ArrayRef;

    /// Bytes of buffer memory this array pins, including shared allocations it
    /// only partly covers.
    ///
    /// Used by the daemon's memory accounting and by stage 2 to size an IPC
    /// body before encoding.
    fn buffer_memory_size(&self) -> usize;

    /// The child arrays this array owns, in field order. Empty for leaf types.
    fn children(&self) -> Vec<&ArrayRef> {
        Vec::new()
    }

    /// Structural equality against another array, used by the [`PartialEq`]
    /// implementation on `dyn Array`.
    ///
    /// Implementors downcast and compare their own representation, so a
    /// sliced array equals an unsliced one with the same logical values.
    fn equals(&self, other: &dyn Array) -> bool;
}

impl PartialEq for dyn Array {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl PartialEq<dyn Array> for ArrayRef {
    #[inline]
    fn eq(&self, other: &dyn Array) -> bool {
        self.as_ref().equals(other)
    }
}

/// Downcast helpers on `dyn Array`.
///
/// ```
/// use astrs_data::array::{ArrayExt, ArrayRef, Int32Array, IntoArrayRef, StringArray};
///
/// let column: ArrayRef = Int32Array::from_values([1, 2]).into_array_ref();
/// assert!(column.downcast::<Int32Array>().is_some());
/// assert!(column.downcast::<StringArray>().is_none());
/// assert!(column.try_downcast::<StringArray>().is_err());
/// ```
pub trait ArrayExt {
    /// Downcasts to a concrete array type, or `None` on a type mismatch.
    fn downcast<A: Array>(&self) -> Option<&A>;

    /// Downcasts to a concrete array type.
    ///
    /// # Errors
    ///
    /// [`DataError::DowncastFailed`] on a type mismatch, naming both the
    /// requested Rust type and the actual [`DataType`].
    fn try_downcast<A: Array>(&self) -> Result<&A>;
}

impl ArrayExt for dyn Array {
    #[inline]
    fn downcast<A: Array>(&self) -> Option<&A> {
        self.as_any().downcast_ref::<A>()
    }

    fn try_downcast<A: Array>(&self) -> Result<&A> {
        self.downcast::<A>()
            .ok_or_else(|| DataError::downcast_failed::<A>(self.data_type().clone()))
    }
}

impl ArrayExt for ArrayRef {
    #[inline]
    fn downcast<A: Array>(&self) -> Option<&A> {
        self.as_ref().downcast::<A>()
    }

    #[inline]
    fn try_downcast<A: Array>(&self) -> Result<&A> {
        self.as_ref().try_downcast::<A>()
    }
}

/// Wraps a concrete array in an [`ArrayRef`].
///
/// ```
/// use astrs_data::array::{Array, BooleanArray, IntoArrayRef};
///
/// let column = BooleanArray::from_values([true, false]).into_array_ref();
/// assert_eq!(column.len(), 2);
/// ```
pub trait IntoArrayRef: Array + Sized {
    /// Moves `self` into a shared, dynamically typed column.
    fn into_array_ref(self) -> ArrayRef {
        Arc::new(self)
    }
}

impl<A: Array + Sized> IntoArrayRef for A {}

/// Checked slicing for any array.
///
/// # Errors
///
/// [`DataError::SliceOutOfBounds`] when the window leaves the array.
///
/// ```
/// use astrs_data::array::{try_slice, Int32Array, IntoArrayRef};
///
/// let column = Int32Array::from_values([1, 2, 3]).into_array_ref();
/// assert_eq!(try_slice(&column, 1, 2)?.len(), 2);
/// assert!(try_slice(&column, 2, 2).is_err());
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn try_slice(array: &ArrayRef, offset: usize, len: usize) -> Result<ArrayRef> {
    if offset.saturating_add(len) > array.len() {
        return Err(DataError::SliceOutOfBounds {
            offset,
            len,
            available: array.len(),
        });
    }
    Ok(slice_array(array, offset, len))
}

/// Slices an [`ArrayRef`], short-circuiting a full-range request to an `Arc`
/// clone instead of rebuilding the array.
///
/// ```
/// use astrs_data::array::{slice_array, Int32Array, IntoArrayRef};
///
/// let column = Int32Array::from_values([1, 2, 3]).into_array_ref();
/// let whole = slice_array(&column, 0, 3);
/// assert!(std::sync::Arc::ptr_eq(&column, &whole), "no rebuild for a full slice");
/// assert_eq!(slice_array(&column, 1, 1).len(), 1);
/// ```
#[must_use]
pub fn slice_array(array: &ArrayRef, offset: usize, len: usize) -> ArrayRef {
    if offset == 0 && len >= array.len() {
        return Arc::clone(array);
    }
    array.slice(offset, len)
}

/// An empty array of the given type.
///
/// # Errors
///
/// [`DataError::InvalidFixedSize`] when a fixed-size type carries a
/// non-positive width.
///
/// ```
/// use astrs_data::array::new_empty_array;
/// use astrs_data::DataType;
///
/// assert_eq!(new_empty_array(&DataType::Utf8)?.len(), 0);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn new_empty_array(data_type: &DataType) -> Result<ArrayRef> {
    new_null_array(data_type, 0)
}

/// An all-null array of the given type and length.
///
/// Stage 2's decoder and the record-batch padding path both need this, and it
/// doubles as the coverage check that every family can be built from a bare
/// [`DataType`].
///
/// # Errors
///
/// [`DataError::InvalidFixedSize`] when a fixed-size type carries a
/// non-positive width.
///
/// ```
/// use astrs_data::array::{new_null_array, Array};
/// use astrs_data::DataType;
///
/// let nulls = new_null_array(&DataType::Float32, 4)?;
/// assert_eq!(nulls.len(), 4);
/// assert_eq!(nulls.null_count(), 4);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn new_null_array(data_type: &DataType, len: usize) -> Result<ArrayRef> {
    let validity = Bitmap::new_unset(len);
    Ok(match data_type {
        DataType::Null => Arc::new(NullArray::new(len)),
        DataType::Bool => Arc::new(BooleanArray::new(Bitmap::new_unset(len), Some(validity))?),
        DataType::Int8 => Arc::new(Int8Array::new_null(len)),
        DataType::Int16 => Arc::new(Int16Array::new_null(len)),
        DataType::Int32 => Arc::new(Int32Array::new_null(len)),
        DataType::Int64 => Arc::new(Int64Array::new_null(len)),
        DataType::UInt8 => Arc::new(UInt8Array::new_null(len)),
        DataType::UInt16 => Arc::new(UInt16Array::new_null(len)),
        DataType::UInt32 => Arc::new(UInt32Array::new_null(len)),
        DataType::UInt64 => Arc::new(UInt64Array::new_null(len)),
        DataType::Float16 => Arc::new(Float16Array::new_null(len)),
        DataType::Float32 => Arc::new(Float32Array::new_null(len)),
        DataType::Float64 => Arc::new(Float64Array::new_null(len)),
        DataType::Timestamp => Arc::new(TimestampArray::new_null(len)),
        DataType::Duration => Arc::new(DurationArray::new_null(len)),
        DataType::Binary => Arc::new(BinaryArray::new_null(len)),
        DataType::LargeBinary => Arc::new(LargeBinaryArray::new_null(len)),
        DataType::Utf8 => Arc::new(StringArray::new_null(len)),
        DataType::LargeUtf8 => Arc::new(LargeStringArray::new_null(len)),
        DataType::FixedSizeBinary(size) => Arc::new(FixedSizeBinaryArray::new_null(*size, len)?),
        DataType::FixedSizeList(field, size) => Arc::new(FixedSizeListArray::new_null(
            field.as_ref().clone(),
            *size,
            len,
        )?),
        DataType::List(field) => Arc::new(ListArray::new_null(field.as_ref().clone(), len)),
        DataType::Struct(fields) => Arc::new(StructArray::new_null(fields.clone(), len)?),
    })
}

/// The name of every array family in the crate, in [`DataType`] order.
///
/// Exists so a test can assert that the sealed implementor set and the closed
/// [`DataType`] set stay in step, which is the exhaustiveness guarantee the
/// trait-object design gives up at compile time.
///
/// ```
/// use astrs_data::array::array_kinds;
///
/// assert!(array_kinds().contains(&"StructArray"));
/// ```
#[must_use]
pub fn array_kinds() -> &'static [&'static str] {
    &[
        "NullArray",
        "BooleanArray",
        "PrimitiveArray",
        "BinaryArray",
        "LargeBinaryArray",
        "StringArray",
        "LargeStringArray",
        "FixedSizeBinaryArray",
        "FixedSizeListArray",
        "ListArray",
        "StructArray",
        "TimestampArray",
        "DurationArray",
    ]
}

/// The unit [`DataType`] variants, as statics.
///
/// [`Array::data_type`] returns `&DataType`, and `DataType` owns heap data in
/// its nested variants, so the compiler cannot promote `&DataType::Null` to
/// `'static`. A `static` can hold it, because statics are never dropped. This
/// keeps the leaf arrays free of a per-instance `DataType` field; the
/// parameterised families (`FixedSizeBinary`, `List`, `FixedSizeList`,
/// `Struct`) store theirs instead.
pub(crate) static NULL_TYPE: DataType = DataType::Null;
/// See [`NULL_TYPE`].
pub(crate) static BOOL_TYPE: DataType = DataType::Bool;
/// See [`NULL_TYPE`].
pub(crate) static BINARY_TYPE: DataType = DataType::Binary;
/// See [`NULL_TYPE`].
pub(crate) static LARGE_BINARY_TYPE: DataType = DataType::LargeBinary;
/// See [`NULL_TYPE`].
pub(crate) static UTF8_TYPE: DataType = DataType::Utf8;
/// See [`NULL_TYPE`].
pub(crate) static LARGE_UTF8_TYPE: DataType = DataType::LargeUtf8;
/// See [`NULL_TYPE`].
pub(crate) static TIMESTAMP_TYPE: DataType = DataType::Timestamp;
/// See [`NULL_TYPE`].
pub(crate) static DURATION_TYPE: DataType = DataType::Duration;

/// Validates a validity bitmap against a logical length.
///
/// Shared by every array constructor, so the error is identical everywhere.
///
/// # Errors
///
/// [`DataError::ValidityLengthMismatch`] when the bitmap does not cover
/// exactly `len` slots.
pub(crate) fn check_validity(validity: Option<&Bitmap>, len: usize) -> Result<()> {
    match validity {
        Some(bits) if bits.len() != len => Err(DataError::ValidityLengthMismatch {
            array_len: len,
            validity_len: bits.len(),
        }),
        _ => Ok(()),
    }
}

/// Clamps a requested window to `len`, returning `(offset, len)`.
#[inline]
pub(crate) fn clamp_window(array_len: usize, offset: usize, len: usize) -> (usize, usize) {
    let offset = offset.min(array_len);
    (offset, len.min(array_len - offset))
}

/// Compares two optional validity bitmaps by their logical null pattern.
#[inline]
pub(crate) fn validity_eq(left: Option<&Bitmap>, right: Option<&Bitmap>, len: usize) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(bits), None) | (None, Some(bits)) => bits.count_set() == len,
        (Some(a), Some(b)) => a == b,
    }
}

/// Rejects a non-positive fixed width.
pub(crate) fn check_fixed_size(size: i32) -> Result<usize> {
    if size <= 0 {
        return Err(DataError::InvalidFixedSize { size });
    }
    usize::try_from(size).map_err(|_| DataError::InvalidFixedSize { size })
}

/// Renders `len`, `null_count` and a value preview the way every array's
/// `Debug` does, so the output is uniform across families.
pub(crate) fn debug_array<T: std::fmt::Debug>(
    f: &mut std::fmt::Formatter<'_>,
    name: &str,
    data_type: &DataType,
    len: usize,
    null_count: usize,
    values: impl Iterator<Item = Option<T>>,
) -> std::fmt::Result {
    const PREVIEW: usize = 10;
    let mut rendered = Vec::with_capacity(PREVIEW);
    for value in values.take(PREVIEW) {
        rendered.push(match value {
            Some(value) => format!("{value:?}"),
            None => "null".to_owned(),
        });
    }
    if len > PREVIEW {
        rendered.push("…".to_owned());
    }
    write!(
        f,
        "{name}[{data_type}; len={len}, nulls={null_count}] {{{}}}",
        rendered.join(", ")
    )
}

/// The field a nested type must expose for its child column.
pub(crate) fn child_field(data_type: &DataType) -> Option<&Field> {
    match data_type {
        DataType::List(field) | DataType::FixedSizeList(field, _) => Some(field.as_ref()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn every_data_type() -> Vec<DataType> {
        vec![
            DataType::Null,
            DataType::Bool,
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
            DataType::Binary,
            DataType::LargeBinary,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::FixedSizeBinary(4),
            DataType::fixed_size_list(Field::new("xyz", DataType::Float32, false), 3),
            DataType::list(Field::new("item", DataType::Utf8, true)),
            DataType::strukt([Field::new("a", DataType::Int32, true)]),
            DataType::Timestamp,
            DataType::Duration,
        ]
    }

    #[test]
    fn arrays_are_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn Array>();
        assert_send_sync::<ArrayRef>();
    }

    #[test]
    fn null_arrays_exist_for_every_data_type() {
        for data_type in every_data_type() {
            let array =
                new_null_array(&data_type, 5).unwrap_or_else(|e| panic!("{data_type}: {e}"));
            assert_eq!(array.len(), 5, "{data_type}");
            assert_eq!(array.null_count(), 5, "{data_type}");
            assert_eq!(array.data_type(), &data_type, "{data_type}");
            for i in 0..5 {
                assert!(array.is_null(i), "{data_type} index {i}");
                assert!(!array.is_valid(i), "{data_type} index {i}");
            }
            assert!(array.is_null(99), "out of range reads as null");
        }
    }

    #[test]
    fn empty_arrays_exist_for_every_data_type() {
        for data_type in every_data_type() {
            let array = new_empty_array(&data_type).unwrap();
            assert!(array.is_empty(), "{data_type}");
            assert_eq!(array.null_count(), 0, "{data_type}");
            assert_eq!(Array::slice(array.as_ref(), 0, 0).len(), 0);
        }
    }

    #[test]
    fn null_arrays_reject_invalid_fixed_widths() {
        assert!(new_null_array(&DataType::FixedSizeBinary(0), 1).is_err());
        assert!(new_null_array(&DataType::FixedSizeBinary(-3), 1).is_err());
        assert!(
            new_null_array(
                &DataType::fixed_size_list(Field::new("x", DataType::Int8, true), 0),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn kinds_cover_every_family() {
        assert_eq!(array_kinds().len(), 13);
        let mut sorted = array_kinds().to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), array_kinds().len(), "no duplicates");
    }

    #[test]
    fn downcasting() {
        let column: ArrayRef = Int32Array::from_values([1, 2]).into_array_ref();
        assert!(column.downcast::<Int32Array>().is_some());
        assert!(column.downcast::<StringArray>().is_none());
        assert_eq!(column.try_downcast::<Int32Array>().unwrap().len(), 2);
        let err = column.try_downcast::<BooleanArray>().unwrap_err();
        assert!(matches!(err, DataError::DowncastFailed { .. }));
        assert!(column.as_ref().downcast::<Int32Array>().is_some());
    }

    #[test]
    fn slice_array_short_circuits_full_ranges() {
        let column: ArrayRef = Int32Array::from_values([1, 2, 3]).into_array_ref();
        let whole = slice_array(&column, 0, 3);
        assert!(Arc::ptr_eq(&column, &whole));
        let over = slice_array(&column, 0, 99);
        assert!(Arc::ptr_eq(&column, &over));
        let part = slice_array(&column, 1, 2);
        assert!(!Arc::ptr_eq(&column, &part));
        assert_eq!(part.len(), 2);
    }

    #[test]
    fn try_slice_reports_out_of_range() {
        let column: ArrayRef = Int32Array::from_values([1, 2, 3]).into_array_ref();
        assert_eq!(try_slice(&column, 1, 2).unwrap().len(), 2);
        assert_eq!(
            try_slice(&column, 2, 2).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 2,
                len: 2,
                available: 3
            }
        );
        assert!(try_slice(&column, usize::MAX, 1).is_err());
    }

    #[test]
    fn equality_across_trait_objects() {
        let a: ArrayRef = Int32Array::from_values([1, 2, 3]).into_array_ref();
        let b: ArrayRef = Int32Array::from_values([1, 2, 3]).into_array_ref();
        let c: ArrayRef = Int32Array::from_values([1, 2]).into_array_ref();
        let d: ArrayRef = Int64Array::from_values([1, 2, 3]).into_array_ref();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d, "different data types are never equal");
        assert_eq!(vec![Arc::clone(&a)], vec![b]);
    }

    #[test]
    fn sliced_arrays_equal_freshly_built_ones() {
        let full: ArrayRef = Int32Array::from_values([9, 1, 2, 3, 9]).into_array_ref();
        let window = slice_array(&full, 1, 3);
        let fresh: ArrayRef = Int32Array::from_values([1, 2, 3]).into_array_ref();
        assert_eq!(window, fresh);
    }

    #[test]
    fn helper_predicates() {
        assert!(check_validity(None, 3).is_ok());
        let bits = Bitmap::new_set(3);
        assert!(check_validity(Some(&bits), 3).is_ok());
        assert_eq!(
            check_validity(Some(&bits), 4).unwrap_err(),
            DataError::ValidityLengthMismatch {
                array_len: 4,
                validity_len: 3
            }
        );
        assert_eq!(clamp_window(5, 3, 99), (3, 2));
        assert_eq!(clamp_window(5, 99, 1), (5, 0));
        assert_eq!(check_fixed_size(4).unwrap(), 4);
        assert!(check_fixed_size(0).is_err());
        assert!(check_fixed_size(-1).is_err());
        assert!(validity_eq(None, Some(&Bitmap::new_set(3)), 3));
        assert!(!validity_eq(None, Some(&Bitmap::new_unset(3)), 3));
        assert!(child_field(&DataType::Int8).is_none());
        assert!(child_field(&DataType::list(Field::new("i", DataType::Int8, true))).is_some());
    }
}
