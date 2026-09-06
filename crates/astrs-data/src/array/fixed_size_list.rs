//! [`FixedSizeListArray`] — a column whose every value is exactly `size`
//! child values.
//!
//! No offset buffer at all: slot `i` is `values[i * size .. (i + 1) * size]`.
//! This is the tensor layout of the AstRS data plane — a `FixedSizeList(
//! Float32, 3)` is a column of XYZ points, a `FixedSizeList(FixedSizeList(
//! UInt8, 3), 640)` is a row of RGB pixels — and it is what the stage-3 tensor
//! views are built on.
//!
//! ```
//! use astrs_data::array::{Array, FixedSizeListArray, Float32Array, IntoArrayRef};
//! use astrs_data::{DataType, Field};
//!
//! let child = Float32Array::from_values([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).into_array_ref();
//! let points = FixedSizeListArray::try_new(
//!     Field::new("xyz", DataType::Float32, false),
//!     3,
//!     child,
//!     None,
//! )?;
//!
//! assert_eq!(points.len(), 2, "six floats, three per point");
//! assert_eq!(points.value_size(), 3);
//! assert_eq!(points.get(1).map(|p| p.len()), Some(3));
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::{
    Array, ArrayRef, check_fixed_size, check_validity, clamp_window, new_empty_array, slice_array,
    validity_eq,
};
use crate::buffer::Bitmap;
use crate::datatype::{DataType, Field};
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of fixed-length runs over a shared child array.
#[derive(Clone)]
pub struct FixedSizeListArray {
    /// Always `DataType::FixedSizeList(field, size)`.
    data_type: DataType,
    /// Child values per slot. Always positive.
    size: usize,
    /// The flat child array, `len * size` values long.
    values: ArrayRef,
    /// Logical length, `values.len() / size`.
    len: usize,
    /// Validity, or `None` when every slot is valid.
    validity: Option<Bitmap>,
}

impl FixedSizeListArray {
    /// Builds a fixed-size list array from its child and validity.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidFixedSize`] when `size` is not positive.
    /// * [`DataError::TypeMismatch`] when the child's type differs from the
    ///   field's.
    /// * [`DataError::ChildLengthMismatch`] when the child length is not a
    ///   multiple of `size`.
    /// * [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn try_new(
        field: Field,
        size: i32,
        values: ArrayRef,
        validity: Option<Bitmap>,
    ) -> Result<Self> {
        let width = check_fixed_size(size)?;
        if field.data_type() != values.data_type() {
            return Err(DataError::type_mismatch(
                field.data_type().clone(),
                values.data_type().clone(),
            ));
        }
        if !values.len().is_multiple_of(width) {
            return Err(DataError::ChildLengthMismatch {
                expected: values.len().next_multiple_of(width),
                actual: values.len(),
            });
        }
        let len = values.len() / width;
        check_validity(validity.as_ref(), len)?;
        Ok(Self {
            data_type: DataType::fixed_size_list(field, size),
            size: width,
            values,
            len,
            validity,
        })
    }

    /// An all-null array of `len` slots.
    ///
    /// The child is `len * size` null values of the field's type, which keeps
    /// the layout well formed even though every slot is null.
    ///
    /// # Errors
    ///
    /// [`DataError::InvalidFixedSize`] when `size` is not positive.
    pub fn new_null(field: Field, size: i32, len: usize) -> Result<Self> {
        let width = check_fixed_size(size)?;
        let values = crate::array::new_null_array(field.data_type(), len * width)
            .or_else(|_| new_empty_array(field.data_type()))?;
        Ok(Self {
            data_type: DataType::fixed_size_list(field, size),
            size: width,
            values,
            len,
            validity: Some(Bitmap::new_unset(len)),
        })
    }

    /// Assembles the parts without revalidating.
    ///
    /// Safe, and crate-internal: see [`crate::array::ListArray::from_parts`].
    pub(crate) fn from_parts(
        field: Field,
        size: i32,
        values: ArrayRef,
        validity: Option<Bitmap>,
    ) -> Self {
        let width = usize::try_from(size).unwrap_or(1).max(1);
        debug_assert_eq!(field.data_type(), values.data_type());
        debug_assert_eq!(values.len() % width, 0);
        Self {
            data_type: DataType::fixed_size_list(field, size),
            size: width,
            len: values.len() / width,
            values,
            validity,
        }
    }

    /// Child values per slot.
    #[inline]
    #[must_use]
    pub const fn value_size(&self) -> usize {
        self.size
    }

    /// The child field describing one element.
    #[must_use]
    pub fn field(&self) -> Option<&Field> {
        crate::array::child_field(&self.data_type)
    }

    /// The flat child array.
    #[inline]
    #[must_use]
    pub const fn values(&self) -> &ArrayRef {
        &self.values
    }

    /// The run at `index` as a zero-copy slice of the child, ignoring
    /// validity.
    ///
    /// Returns `None` only when `index` is out of range.
    #[must_use]
    pub fn value(&self, index: usize) -> Option<ArrayRef> {
        if index >= self.len {
            return None;
        }
        Some(slice_array(&self.values, index * self.size, self.size))
    }

    /// The logical run at `index`: `None` when the slot is null or out of
    /// range.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<ArrayRef> {
        if self.is_valid_index(index) {
            self.value(index)
        } else {
            None
        }
    }

    /// Iterates over the logical runs, `None` for nulls.
    pub fn iter(&self) -> ArrayIter<&Self> {
        ArrayIter::new(self)
    }

    /// A zero-copy sub-range, clamped to the array (the crate-wide slicing
    /// convention).
    ///
    /// Unlike [`crate::array::ListArray`], the child *is* narrowed here: the
    /// fixed stride makes the child window exactly `offset * size` long, so
    /// slicing releases the child memory outside the window.
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let (offset, len) = clamp_window(self.len, offset, len);
        Self {
            data_type: self.data_type.clone(),
            size: self.size,
            values: slice_array(&self.values, offset * self.size, len * self.size),
            len,
            validity: self.validity.as_ref().map(|bits| bits.slice(offset, len)),
        }
    }

    /// Checked [`FixedSizeListArray::slice`].
    ///
    /// # Errors
    ///
    /// [`DataError::SliceOutOfBounds`] when the window leaves the array.
    pub fn try_slice(&self, offset: usize, len: usize) -> Result<Self> {
        if offset.saturating_add(len) > self.len {
            return Err(DataError::SliceOutOfBounds {
                offset,
                len,
                available: self.len,
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
        check_validity(validity.as_ref(), self.len)?;
        Ok(Self {
            data_type: self.data_type.clone(),
            size: self.size,
            values: Arc::clone(&self.values),
            len: self.len,
            validity,
        })
    }

    /// Pushes this array's validity down onto the child, one parent bit per
    /// `size` child slots.
    ///
    /// Stage 2 needs the flattened form when encoding a nested body, and
    /// kernels need it when a fixed-size list is turned into a dense tensor.
    ///
    /// ```
    /// use astrs_data::array::{Array, FixedSizeListArray, Int32Array, IntoArrayRef};
    /// use astrs_data::{Bitmap, DataType, Field};
    ///
    /// let child = Int32Array::from_values([1, 2, 3, 4]).into_array_ref();
    /// let lists = FixedSizeListArray::try_new(
    ///     Field::new("pair", DataType::Int32, false),
    ///     2,
    ///     child,
    ///     Some([true, false].into_iter().collect()),
    /// )?;
    /// let flat = lists.child_validity();
    /// assert_eq!(flat.map(|b| b.count_unset()), Some(2));
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    #[must_use]
    pub fn child_validity(&self) -> Option<Bitmap> {
        self.validity
            .as_ref()
            .map(|bits| bits.repeat_each(self.size))
    }

    /// Whether `index` is inside the array and not null.
    #[inline]
    fn is_valid_index(&self, index: usize) -> bool {
        index < self.len && self.validity.as_ref().is_none_or(|bits| bits.value(index))
    }
}

impl Sealed for FixedSizeListArray {}

impl Array for FixedSizeListArray {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        &self.data_type
    }

    fn len(&self) -> usize {
        self.len
    }

    fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    fn slice(&self, offset: usize, len: usize) -> ArrayRef {
        Arc::new(Self::slice(self, offset, len))
    }

    fn buffer_memory_size(&self) -> usize {
        self.values.buffer_memory_size()
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
            || self.len != other.len
            || !validity_eq(self.validity.as_ref(), other.validity.as_ref(), self.len)
        {
            return false;
        }
        (0..self.len).all(|index| match (self.get(index), other.get(index)) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        })
    }
}

impl ArrayAccessor for &FixedSizeListArray {
    type Item = ArrayRef;

    #[inline]
    fn accessor_len(&self) -> usize {
        self.len
    }

    #[inline]
    fn accessor_get(&self, index: usize) -> Option<ArrayRef> {
        FixedSizeListArray::get(self, index)
    }
}

impl PartialEq for FixedSizeListArray {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl<'a> IntoIterator for &'a FixedSizeListArray {
    type Item = Option<ArrayRef>;
    type IntoIter = ArrayIter<&'a FixedSizeListArray>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for FixedSizeListArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FixedSizeListArray[{}; len={}, nulls={}, size={}]",
            self.data_type,
            self.len,
            self.null_count(),
            self.size
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Float32Array, Int32Array, IntoArrayRef, StringArray};

    fn xyz_field() -> Field {
        Field::new("xyz", DataType::Float32, false)
    }

    fn points() -> FixedSizeListArray {
        let child = Float32Array::from_values([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).into_array_ref();
        FixedSizeListArray::try_new(xyz_field(), 3, child, None).unwrap()
    }

    fn as_floats(array: &ArrayRef) -> Vec<f32> {
        array
            .as_any()
            .downcast_ref::<Float32Array>()
            .map(|a| a.values().to_vec())
            .unwrap_or_default()
    }

    #[test]
    fn empty_array() {
        let child = Float32Array::from_values([] as [f32; 0]).into_array_ref();
        let points = FixedSizeListArray::try_new(xyz_field(), 3, child, None).unwrap();
        assert!(points.is_empty());
        assert_eq!(points.value_size(), 3);
        assert!(points.get(0).is_none());
        assert_eq!(points.iter().count(), 0);
    }

    #[test]
    fn no_null_array() {
        let points = points();
        assert_eq!(points.len(), 2);
        assert_eq!(points.null_count(), 0);
        assert_eq!(as_floats(&points.get(0).unwrap()), vec![1.0, 2.0, 3.0]);
        assert_eq!(as_floats(&points.get(1).unwrap()), vec![4.0, 5.0, 6.0]);
        assert!(points.get(2).is_none());
        assert_eq!(points.field().map(Field::name), Some("xyz"));
        assert_eq!(points.values().len(), 6);
        assert!(points.child_validity().is_none());
    }

    #[test]
    fn all_null_array() {
        let points = FixedSizeListArray::new_null(xyz_field(), 3, 2).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points.null_count(), 2);
        assert_eq!(points.values().len(), 6);
        assert!(points.get(0).is_none());
        assert_eq!(points.value(0).map(|v| v.len()), Some(3));
    }

    #[test]
    fn mixed_nulls_and_child_validity() {
        let child = Int32Array::from_values([1, 2, 3, 4]).into_array_ref();
        let lists = FixedSizeListArray::try_new(
            Field::new("pair", DataType::Int32, false),
            2,
            child,
            Some([true, false].into_iter().collect()),
        )
        .unwrap();
        assert_eq!(lists.null_count(), 1);
        assert!(lists.get(1).is_none());
        assert_eq!(lists.value(1).map(|v| v.len()), Some(2));
        let flat = lists.child_validity().unwrap();
        assert_eq!(flat.len(), 4);
        assert_eq!(
            flat.iter().collect::<Vec<_>>(),
            vec![true, true, false, false]
        );
    }

    #[test]
    fn invalid_sizes_are_rejected() {
        let child = Float32Array::from_values([1.0]).into_array_ref();
        assert!(FixedSizeListArray::try_new(xyz_field(), 0, child.clone(), None).is_err());
        assert!(FixedSizeListArray::try_new(xyz_field(), -2, child, None).is_err());
        assert!(FixedSizeListArray::new_null(xyz_field(), 0, 1).is_err());
    }

    #[test]
    fn child_length_must_be_a_multiple_of_the_size() {
        let child = Float32Array::from_values([1.0, 2.0, 3.0, 4.0]).into_array_ref();
        assert_eq!(
            FixedSizeListArray::try_new(xyz_field(), 3, child, None).unwrap_err(),
            DataError::ChildLengthMismatch {
                expected: 6,
                actual: 4
            }
        );
    }

    #[test]
    fn child_type_must_match_the_field() {
        let child = StringArray::from_values(["a"]).into_array_ref();
        assert!(matches!(
            FixedSizeListArray::try_new(xyz_field(), 1, child, None),
            Err(DataError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn validity_length_is_checked() {
        let child = Float32Array::from_values([1.0, 2.0]).into_array_ref();
        assert!(matches!(
            FixedSizeListArray::try_new(xyz_field(), 1, child, Some(Bitmap::new_set(5))),
            Err(DataError::ValidityLengthMismatch { .. })
        ));
    }

    #[test]
    fn slicing_narrows_the_child_too() {
        let child = Int32Array::from_values(0..30).into_array_ref();
        let lists = FixedSizeListArray::try_new(
            Field::new("triple", DataType::Int32, false),
            3,
            child,
            None,
        )
        .unwrap();
        assert_eq!(lists.len(), 10);
        let window = lists.slice(2, 3);
        assert_eq!(window.len(), 3);
        assert_eq!(window.values().len(), 9, "the child window is exact");
        assert_eq!(
            window.get(0).and_then(|v| v
                .as_any()
                .downcast_ref::<Int32Array>()
                .map(|a| a.values()[0])),
            Some(6)
        );
        assert_eq!(lists.slice(9, 99).len(), 1);
        assert_eq!(lists.slice(99, 1).len(), 0);
        assert_eq!(lists.slice(1, 5).slice(2, 2).len(), 2);
        assert!(lists.try_slice(9, 2).is_err());
    }

    #[test]
    fn nested_fixed_size_lists_model_an_image_row() {
        // 4 pixels of RGB = FixedSizeList(FixedSizeList(UInt8, 3), 4)
        let bytes = crate::array::UInt8Array::from_values(0..12u8).into_array_ref();
        let pixels = FixedSizeListArray::try_new(
            Field::new("channel", DataType::UInt8, false),
            3,
            bytes,
            None,
        )
        .unwrap();
        assert_eq!(pixels.len(), 4);
        let row = FixedSizeListArray::try_new(
            Field::new("pixel", pixels.data_type().clone(), false),
            4,
            pixels.into_array_ref(),
            None,
        )
        .unwrap();
        assert_eq!(row.len(), 1);
        let first = row.get(0).unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!(row.children().len(), 1);
        assert!(row.buffer_memory_size() >= 12);
    }

    #[test]
    fn equality_is_by_logical_content() {
        assert_eq!(points(), points());
        let other = FixedSizeListArray::try_new(
            xyz_field(),
            3,
            Float32Array::from_values([9.0, 2.0, 3.0, 4.0, 5.0, 6.0]).into_array_ref(),
            None,
        )
        .unwrap();
        assert_ne!(points(), other);
        assert_ne!(points(), points().slice(0, 1));

        let masked = points()
            .with_validity(Some([true, false].into_iter().collect()))
            .unwrap();
        assert_ne!(points(), masked);
        assert!(points().with_validity(Some(Bitmap::new_set(9))).is_err());
    }

    #[test]
    fn different_sizes_are_not_equal() {
        let child = Int32Array::from_values([1, 2, 3, 4]).into_array_ref();
        let field = Field::new("v", DataType::Int32, false);
        let two = FixedSizeListArray::try_new(field.clone(), 2, child.clone(), None).unwrap();
        let four = FixedSizeListArray::try_new(field, 4, child, None).unwrap();
        assert_ne!(two, four);
    }

    #[test]
    fn debug_output_shows_the_size() {
        let rendered = format!("{:?}", points());
        assert!(rendered.contains("FixedSizeList(Float32, 3)"), "{rendered}");
        assert!(rendered.contains("size=3"), "{rendered}");
        assert!(rendered.contains("len=2"), "{rendered}");
    }

    #[test]
    fn iteration_yields_child_slices() {
        let masked = points()
            .with_validity(Some([true, false].into_iter().collect()))
            .unwrap();
        let lengths: Vec<Option<usize>> = masked.iter().map(|v| v.map(|a| a.len())).collect();
        assert_eq!(lengths, vec![Some(3), None]);
        assert_eq!((&masked).into_iter().count(), 2);
    }
}
