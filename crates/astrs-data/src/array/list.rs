//! [`ListArray`] — a column whose every value is a variable-length run of
//! child values.
//!
//! # Layout
//!
//! ```text
//!   offsets  [o0, o1, o2, …]      len + 1 entries into the child array
//!   values   ├─ list 0 ─┼─ list 1 ─┼ …    one flat child array
//!   validity  1          0                optional, one bit per list
//! ```
//!
//! The child is itself an [`ArrayRef`], so lists nest arbitrarily: a
//! `List(List(Float32))` is a list array whose child is a list array whose
//! child is a `Float32` column. `value(i)` returns a *slice* of the child
//! array, which is an `Arc` clone plus a window adjustment — no copying.
//!
//! Blueprint §6.1 keeps only 32-bit offsets in the P0 set (there is no
//! `LargeList`), so this type is not generic over the offset width.
//!
//! ```
//! use astrs_data::array::{Array, Int32Array, IntoArrayRef, ListArray};
//! use astrs_data::{DataType, Field, ScalarBuffer};
//!
//! let child = Int32Array::from_values([1, 2, 3, 4, 5]).into_array_ref();
//! let lists = ListArray::try_new(
//!     Field::new("item", DataType::Int32, false),
//!     ScalarBuffer::from_slice(&[0, 2, 2, 5]),
//!     child,
//!     None,
//! )?;
//!
//! assert_eq!(lists.len(), 3);
//! assert_eq!(lists.value_length(0), Some(2));
//! assert_eq!(lists.value_length(1), Some(0), "an empty list is not a null list");
//! assert_eq!(lists.get(2).map(|v| v.len()), Some(3));
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::offset::validate_offsets;
use crate::array::{
    Array, ArrayRef, check_validity, clamp_window, new_empty_array, slice_array, validity_eq,
};
use crate::buffer::{Bitmap, ScalarBuffer};
use crate::datatype::{DataType, Field};
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of variable-length lists over a shared child array.
#[derive(Clone)]
pub struct ListArray {
    /// Always `DataType::List(field)`.
    data_type: DataType,
    /// `len + 1` non-decreasing offsets into `values`.
    offsets: ScalarBuffer<i32>,
    /// The flat child array every list points into.
    values: ArrayRef,
    /// Validity, or `None` when every list is valid.
    validity: Option<Bitmap>,
}

impl ListArray {
    /// Builds a list array from its child, offsets and validity.
    ///
    /// # Errors
    ///
    /// * The offset errors listed on [`validate_offsets`].
    /// * [`DataError::TypeMismatch`] when the child array's type differs from
    ///   the field's.
    /// * [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn try_new(
        field: Field,
        offsets: ScalarBuffer<i32>,
        values: ArrayRef,
        validity: Option<Bitmap>,
    ) -> Result<Self> {
        if field.data_type() != values.data_type() {
            return Err(DataError::type_mismatch(
                field.data_type().clone(),
                values.data_type().clone(),
            ));
        }
        let len = validate_offsets(offsets.as_slice(), values.len())?;
        check_validity(validity.as_ref(), len)?;
        Ok(Self {
            data_type: DataType::list(field),
            offsets,
            values,
            validity,
        })
    }

    /// Builds a list array from a child and per-list lengths.
    ///
    /// The lengths must sum to the child's length.
    ///
    /// # Errors
    ///
    /// * [`DataError::TypeMismatch`] when the child's type differs from the
    ///   field's.
    /// * [`DataError::ChildLengthMismatch`] when the lengths do not add up.
    ///
    /// ```
    /// use astrs_data::array::{Array, Int32Array, IntoArrayRef, ListArray};
    /// use astrs_data::{DataType, Field};
    ///
    /// let child = Int32Array::from_values([1, 2, 3]).into_array_ref();
    /// let lists = ListArray::try_from_lengths(
    ///     Field::new("item", DataType::Int32, false),
    ///     [2, 1],
    ///     child,
    /// )?;
    /// assert_eq!(lists.len(), 2);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_from_lengths(
        field: Field,
        lengths: impl IntoIterator<Item = usize>,
        values: ArrayRef,
    ) -> Result<Self> {
        let mut offsets = vec![0i32];
        let mut total = 0usize;
        for length in lengths {
            total += length;
            offsets.push(
                i32::try_from(total).map_err(|_| DataError::OffsetOutOfBounds {
                    index: offsets.len(),
                    offset: total,
                    values_len: values.len(),
                })?,
            );
        }
        if total != values.len() {
            return Err(DataError::ChildLengthMismatch {
                expected: total,
                actual: values.len(),
            });
        }
        Self::try_new(field, ScalarBuffer::from_slice(&offsets), values, None)
    }

    /// An all-null list array of `len` slots over an empty child.
    ///
    /// The child type comes from `field`; a field whose type cannot be
    /// instantiated empty falls back to a `Null` child, which keeps the
    /// constructor infallible.
    #[must_use]
    pub fn new_null(field: Field, len: usize) -> Self {
        let values = new_empty_array(field.data_type())
            .unwrap_or_else(|_| Arc::new(crate::array::NullArray::new(0)));
        Self {
            data_type: DataType::list(field),
            offsets: ScalarBuffer::zeroed(len + 1),
            values,
            validity: Some(Bitmap::new_unset(len)),
        }
    }

    /// Assembles the parts without revalidating.
    ///
    /// Safe, and crate-internal: [`crate::builder::ListBuilder`] constructs
    /// its own offsets and checks the child type up front, so revalidating on
    /// every `finish` would be pure overhead.
    pub(crate) fn from_parts(
        field: Field,
        offsets: ScalarBuffer<i32>,
        values: ArrayRef,
        validity: Option<Bitmap>,
    ) -> Self {
        debug_assert_eq!(field.data_type(), values.data_type());
        debug_assert!(validate_offsets(offsets.as_slice(), values.len()).is_ok());
        Self {
            data_type: DataType::list(field),
            offsets,
            values,
            validity,
        }
    }

    /// The child field describing one element.
    #[must_use]
    pub fn field(&self) -> Option<&Field> {
        crate::array::child_field(&self.data_type)
    }

    /// The flat child array every list points into.
    #[inline]
    #[must_use]
    pub const fn values(&self) -> &ArrayRef {
        &self.values
    }

    /// The offset buffer, `len + 1` entries.
    #[inline]
    #[must_use]
    pub fn value_offsets(&self) -> &[i32] {
        self.offsets.as_slice()
    }

    /// The offset buffer as a shareable typed window.
    ///
    /// See [`crate::array::GenericBinaryArray::offsets_buffer`]: the IPC
    /// encoder clones this window rather than copying the offsets out of a
    /// slice.
    ///
    /// ```
    /// use astrs_data::array::{Int32Array, IntoArrayRef, ListArray};
    /// use astrs_data::{DataType, Field};
    ///
    /// let child = Int32Array::from_values([1, 2, 3]).into_array_ref();
    /// let lists =
    ///     ListArray::try_from_lengths(Field::new("item", DataType::Int32, false), [2, 1], child)?;
    /// assert_eq!(lists.offsets_buffer().as_slice(), &[0, 2, 3]);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    #[inline]
    #[must_use]
    pub const fn offsets_buffer(&self) -> &ScalarBuffer<i32> {
        &self.offsets
    }

    /// The list at `index` as a zero-copy slice of the child, ignoring
    /// validity.
    ///
    /// Returns `None` only when `index` is out of range.
    #[must_use]
    pub fn value(&self, index: usize) -> Option<ArrayRef> {
        let start = usize::try_from(*self.offsets.as_slice().get(index)?).ok()?;
        let end = usize::try_from(*self.offsets.as_slice().get(index + 1)?).ok()?;
        Some(slice_array(&self.values, start, end.checked_sub(start)?))
    }

    /// The logical list at `index`: `None` when the slot is null or out of
    /// range.
    ///
    /// A *null* list and an *empty* list are different: the first returns
    /// `None`, the second a zero-length array.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<ArrayRef> {
        if self.is_valid_index(index) {
            self.value(index)
        } else {
            None
        }
    }

    /// Number of child values in list `index`, ignoring validity.
    #[inline]
    #[must_use]
    pub fn value_length(&self, index: usize) -> Option<usize> {
        let offsets = self.offsets.as_slice();
        let start = usize::try_from(*offsets.get(index)?).ok()?;
        let end = usize::try_from(*offsets.get(index + 1)?).ok()?;
        end.checked_sub(start)
    }

    /// Iterates over the logical lists, `None` for nulls.
    pub fn iter(&self) -> ArrayIter<&Self> {
        ArrayIter::new(self)
    }

    /// A zero-copy sub-range, clamped to the array (the crate-wide slicing
    /// convention).
    ///
    /// Only the offset buffer and validity are narrowed; the child array is
    /// shared unchanged, exactly as for [`crate::array::GenericBinaryArray`].
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let (offset, len) = clamp_window(self.len(), offset, len);
        Self {
            data_type: self.data_type.clone(),
            offsets: self.offsets.slice(offset, len + 1),
            values: Arc::clone(&self.values),
            validity: self.validity.as_ref().map(|bits| bits.slice(offset, len)),
        }
    }

    /// Checked [`ListArray::slice`].
    ///
    /// # Errors
    ///
    /// [`DataError::SliceOutOfBounds`] when the window leaves the array.
    pub fn try_slice(&self, offset: usize, len: usize) -> Result<Self> {
        if offset.saturating_add(len) > self.len() {
            return Err(DataError::SliceOutOfBounds {
                offset,
                len,
                available: self.len(),
            });
        }
        Ok(self.slice(offset, len))
    }

    /// Returns a copy with a different validity bitmap.
    ///
    /// # Errors
    ///
    /// [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn with_validity(&self, validity: Option<Bitmap>) -> Result<Self> {
        check_validity(validity.as_ref(), self.len())?;
        Ok(Self {
            data_type: self.data_type.clone(),
            offsets: self.offsets.clone(),
            values: Arc::clone(&self.values),
            validity,
        })
    }

    /// Whether `index` is inside the array and not null.
    #[inline]
    fn is_valid_index(&self, index: usize) -> bool {
        index < self.len() && self.validity.as_ref().is_none_or(|bits| bits.value(index))
    }
}

impl Sealed for ListArray {}

impl Array for ListArray {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        &self.data_type
    }

    fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    fn slice(&self, offset: usize, len: usize) -> ArrayRef {
        Arc::new(Self::slice(self, offset, len))
    }

    fn buffer_memory_size(&self) -> usize {
        self.offsets.inner().backing_len()
            + self.values.buffer_memory_size()
            + self
                .validity
                .as_ref()
                .map_or(0, |bits| bits.buffer().backing_len())
    }

    fn children(&self) -> Vec<&ArrayRef> {
        vec![&self.values]
    }

    fn equals(&self, other: &dyn Array) -> bool {
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            return false;
        };
        if self.data_type != other.data_type
            || self.len() != other.len()
            || !validity_eq(self.validity.as_ref(), other.validity.as_ref(), self.len())
        {
            return false;
        }
        (0..self.len()).all(|index| match (self.get(index), other.get(index)) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        })
    }
}

impl ArrayAccessor for &ListArray {
    type Item = ArrayRef;

    #[inline]
    fn accessor_len(&self) -> usize {
        Array::len(*self)
    }

    #[inline]
    fn accessor_get(&self, index: usize) -> Option<ArrayRef> {
        ListArray::get(self, index)
    }
}

impl PartialEq for ListArray {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl<'a> IntoIterator for &'a ListArray {
    type Item = Option<ArrayRef>;
    type IntoIter = ArrayIter<&'a ListArray>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for ListArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lengths: Vec<Option<usize>> = (0..self.len().min(10))
            .map(|index| {
                self.is_valid(index)
                    .then(|| self.value_length(index))
                    .flatten()
            })
            .collect();
        write!(
            f,
            "ListArray[{}; len={}, nulls={}] lengths={lengths:?}",
            self.data_type,
            self.len(),
            self.null_count()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int32Array, IntoArrayRef, StringArray};

    fn item_field() -> Field {
        Field::new("item", DataType::Int32, false)
    }

    fn sample() -> ListArray {
        let child = Int32Array::from_values([1, 2, 3, 4, 5]).into_array_ref();
        ListArray::try_new(
            item_field(),
            ScalarBuffer::from_slice(&[0, 2, 2, 5]),
            child,
            None,
        )
        .unwrap()
    }

    fn as_values(array: &ArrayRef) -> Vec<i32> {
        array
            .as_any()
            .downcast_ref::<Int32Array>()
            .map(|a| a.values().to_vec())
            .unwrap_or_default()
    }

    #[test]
    fn empty_array() {
        let child = Int32Array::from_values([] as [i32; 0]).into_array_ref();
        let lists =
            ListArray::try_new(item_field(), ScalarBuffer::from_slice(&[0]), child, None).unwrap();
        assert!(lists.is_empty());
        assert_eq!(lists.null_count(), 0);
        assert!(lists.get(0).is_none());
        assert_eq!(lists.iter().count(), 0);
    }

    #[test]
    fn no_null_array() {
        let lists = sample();
        assert_eq!(lists.len(), 3);
        assert_eq!(lists.null_count(), 0);
        assert_eq!(as_values(&lists.get(0).unwrap()), vec![1, 2]);
        assert_eq!(as_values(&lists.get(1).unwrap()), Vec::<i32>::new());
        assert_eq!(as_values(&lists.get(2).unwrap()), vec![3, 4, 5]);
        assert!(lists.get(3).is_none());
        assert_eq!(lists.value_offsets(), &[0, 2, 2, 5]);
        assert_eq!(lists.field().map(Field::name), Some("item"));
        assert_eq!(lists.values().len(), 5);
    }

    #[test]
    fn empty_lists_differ_from_null_lists() {
        let lists = sample()
            .with_validity(Some([true, false, true].into_iter().collect()))
            .unwrap();
        assert_eq!(lists.null_count(), 1);
        assert!(lists.get(1).is_none(), "null list");
        assert_eq!(
            lists.value(1).map(|v| v.len()),
            Some(0),
            "the raw slot is still an empty list"
        );
        assert_eq!(lists.value_length(1), Some(0));
    }

    #[test]
    fn all_null_array() {
        let lists = ListArray::new_null(item_field(), 3);
        assert_eq!(lists.len(), 3);
        assert_eq!(lists.null_count(), 3);
        assert_eq!(lists.data_type(), &DataType::list(item_field()));
        for index in 0..3 {
            assert!(lists.get(index).is_none());
        }
    }

    #[test]
    fn from_lengths() {
        let child = Int32Array::from_values([1, 2, 3]).into_array_ref();
        let lists = ListArray::try_from_lengths(item_field(), [2, 0, 1], child.clone()).unwrap();
        assert_eq!(lists.len(), 3);
        assert_eq!(lists.value_offsets(), &[0, 2, 2, 3]);
        assert_eq!(
            ListArray::try_from_lengths(item_field(), [2, 2], child).unwrap_err(),
            DataError::ChildLengthMismatch {
                expected: 4,
                actual: 3
            }
        );
    }

    #[test]
    fn child_type_must_match_the_field() {
        let child = StringArray::from_values(["a"]).into_array_ref();
        let err = ListArray::try_new(item_field(), ScalarBuffer::from_slice(&[0, 1]), child, None)
            .unwrap_err();
        assert!(matches!(err, DataError::TypeMismatch { .. }));
    }

    #[test]
    fn offsets_are_validated_against_the_child() {
        let child = Int32Array::from_values([1, 2]).into_array_ref();
        assert!(matches!(
            ListArray::try_new(
                item_field(),
                ScalarBuffer::from_slice(&[0, 9]),
                child.clone(),
                None
            ),
            Err(DataError::OffsetOutOfBounds { .. })
        ));
        assert!(matches!(
            ListArray::try_new(
                item_field(),
                ScalarBuffer::from_slice(&[0, 2, 1]),
                child.clone(),
                None
            ),
            Err(DataError::NonMonotonicOffsets { .. })
        ));
        assert!(matches!(
            ListArray::try_new(
                item_field(),
                ScalarBuffer::from_slice(&[0, 2]),
                child,
                Some(Bitmap::new_set(5))
            ),
            Err(DataError::ValidityLengthMismatch { .. })
        ));
    }

    #[test]
    fn slicing_shares_the_child() {
        let lists = sample();
        let window = lists.slice(1, 2);
        assert_eq!(window.len(), 2);
        assert_eq!(as_values(&window.get(0).unwrap()), Vec::<i32>::new());
        assert_eq!(as_values(&window.get(1).unwrap()), vec![3, 4, 5]);
        assert_eq!(window.values().len(), 5, "the child is shared whole");
        assert_eq!(window.value_offsets(), &[2, 2, 5]);

        assert_eq!(lists.slice(2, 99).len(), 1);
        assert_eq!(lists.slice(99, 1).len(), 0);
        assert_eq!(lists.slice(0, 3).slice(1, 1).len(), 1);
        assert!(lists.try_slice(2, 2).is_err());
    }

    #[test]
    fn nested_lists() {
        let leaf = Int32Array::from_values([1, 2, 3, 4]).into_array_ref();
        let inner = ListArray::try_new(
            item_field(),
            ScalarBuffer::from_slice(&[0, 2, 4]),
            leaf,
            None,
        )
        .unwrap();
        let inner_field = Field::new("inner", inner.data_type().clone(), true);
        let outer = ListArray::try_new(
            inner_field,
            ScalarBuffer::from_slice(&[0, 1, 2]),
            inner.into_array_ref(),
            None,
        )
        .unwrap();
        assert_eq!(outer.len(), 2);
        let first = outer.get(0).unwrap();
        assert_eq!(first.len(), 1);
        let first_inner = first.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(as_values(&first_inner.get(0).unwrap()), vec![1, 2]);
        assert!(outer.buffer_memory_size() > 0);
        assert_eq!(outer.children().len(), 1);
    }

    #[test]
    fn equality_is_by_logical_content() {
        let a = sample();
        let b = sample();
        assert_eq!(a, b);
        let different_child = ListArray::try_new(
            item_field(),
            ScalarBuffer::from_slice(&[0, 2, 2, 5]),
            Int32Array::from_values([9, 2, 3, 4, 5]).into_array_ref(),
            None,
        )
        .unwrap();
        assert_ne!(a, different_child);
        assert_ne!(a, a.slice(0, 2));

        // A null list and an empty list are not equal.
        let with_null = a
            .with_validity(Some([true, false, true].into_iter().collect()))
            .unwrap();
        assert_ne!(a, with_null);
    }

    #[test]
    fn debug_output_shows_lengths() {
        let lists = sample();
        let rendered = format!("{lists:?}");
        assert!(rendered.contains("List(Int32)"), "{rendered}");
        assert!(rendered.contains("len=3"), "{rendered}");
        assert!(rendered.contains("Some(2)"), "{rendered}");
    }

    #[test]
    fn iteration_yields_child_slices() {
        let lists = sample()
            .with_validity(Some([true, false, true].into_iter().collect()))
            .unwrap();
        let lengths: Vec<Option<usize>> = lists.iter().map(|v| v.map(|a| a.len())).collect();
        assert_eq!(lengths, vec![Some(2), None, Some(3)]);
        assert_eq!((&lists).into_iter().count(), 3);
    }
}
