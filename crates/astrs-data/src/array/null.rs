//! [`NullArray`] — the type whose every value is null and whose every buffer
//! is absent.
//!
//! Arrow's `Null` type carries no validity bitmap and no values buffer at all;
//! the length alone is the entire representation. AstRS uses it for ports
//! declared `type: any` that never carry data, and stage 2 encodes it as a
//! record-batch node with zero buffers.
//!
//! ```
//! use astrs_data::array::{Array, NullArray};
//!
//! let nulls = NullArray::new(3);
//! assert_eq!(nulls.len(), 3);
//! assert_eq!(nulls.null_count(), 3);
//! assert_eq!(nulls.buffer_memory_size(), 0, "no buffers at all");
//! assert!(nulls.is_null(0));
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::{Array, ArrayRef, NULL_TYPE, clamp_window};
use crate::buffer::Bitmap;
use crate::datatype::DataType;
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of `len` nulls and nothing else.
#[derive(Clone, PartialEq, Eq)]
pub struct NullArray {
    /// Logical length. The whole representation.
    len: usize,
}

impl NullArray {
    /// A column of `len` nulls.
    #[inline]
    #[must_use]
    pub const fn new(len: usize) -> Self {
        Self { len }
    }

    /// Always `None`; present so the accessor shape matches the other
    /// families.
    #[inline]
    #[must_use]
    pub const fn get(&self, _index: usize) -> Option<()> {
        None
    }

    /// Iterates, yielding `None` `len` times.
    #[inline]
    pub fn iter(&self) -> ArrayIter<&Self> {
        ArrayIter::new(self)
    }

    /// A sub-range, clamped to the array (the crate-wide slicing convention).
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let (_, len) = clamp_window(self.len, offset, len);
        Self { len }
    }

    /// Checked [`NullArray::slice`].
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
}

impl Sealed for NullArray {}

impl Array for NullArray {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        &NULL_TYPE
    }

    fn len(&self) -> usize {
        self.len
    }

    fn validity(&self) -> Option<&Bitmap> {
        // The Null type has no validity buffer; `null_count` is overridden
        // instead so callers still see every slot as null.
        None
    }

    fn null_count(&self) -> usize {
        self.len
    }

    fn is_valid(&self, _index: usize) -> bool {
        false
    }

    fn slice(&self, offset: usize, len: usize) -> ArrayRef {
        Arc::new(Self::slice(self, offset, len))
    }

    fn buffer_memory_size(&self) -> usize {
        0
    }

    fn equals(&self, other: &dyn Array) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self.len == other.len)
    }
}

impl ArrayAccessor for &NullArray {
    type Item = ();

    #[inline]
    fn accessor_len(&self) -> usize {
        self.len
    }

    #[inline]
    fn accessor_get(&self, _index: usize) -> Option<()> {
        None
    }
}

impl<'a> IntoIterator for &'a NullArray {
    type Item = Option<()>;
    type IntoIter = ArrayIter<&'a NullArray>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for NullArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NullArray[Null; len={}]", self.len)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn empty_null_array() {
        let array = NullArray::new(0);
        assert!(array.is_empty());
        assert_eq!(array.null_count(), 0);
        assert_eq!(array.iter().count(), 0);
    }

    #[test]
    fn every_slot_is_null() {
        let array = NullArray::new(4);
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 4);
        assert!(array.validity().is_none());
        for index in 0..5 {
            assert!(array.is_null(index));
            assert!(!array.is_valid(index));
            assert_eq!(array.get(index), None);
        }
        assert_eq!(array.iter().collect::<Vec<_>>(), vec![None; 4]);
    }

    #[test]
    fn slicing_only_changes_the_length() {
        let array = NullArray::new(10);
        assert_eq!(array.slice(3, 4).len(), 4);
        assert_eq!(array.slice(8, 99).len(), 2);
        assert_eq!(array.slice(99, 1).len(), 0);
        assert_eq!(array.try_slice(3, 4).unwrap().len(), 4);
        assert_eq!(
            array.try_slice(8, 5).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 8,
                len: 5,
                available: 10
            }
        );
    }

    #[test]
    fn carries_no_buffers() {
        assert_eq!(NullArray::new(1_000_000).buffer_memory_size(), 0);
    }

    #[test]
    fn equality_is_by_length() {
        assert_eq!(NullArray::new(3), NullArray::new(3));
        assert_ne!(NullArray::new(3), NullArray::new(4));
        let other: ArrayRef = Arc::new(crate::array::Int32Array::from_values([1]));
        assert!(!NullArray::new(1).equals(other.as_ref()));
    }

    #[test]
    fn debug_output() {
        assert_eq!(format!("{:?}", NullArray::new(7)), "NullArray[Null; len=7]");
    }

    #[test]
    fn trait_object_round_trip() {
        let array: ArrayRef = Arc::new(NullArray::new(5));
        assert_eq!(array.data_type(), &DataType::Null);
        assert_eq!(array.slice(1, 2).len(), 2);
        assert_eq!((&NullArray::new(2)).into_iter().count(), 2);
    }
}
