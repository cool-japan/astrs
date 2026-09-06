//! [`NullBuilder`] — the degenerate builder, for completeness of the
//! [`ArrayBuilder`] set.
//!
//! A `Null` column has no buffers, so the builder is a counter. It exists so
//! that code driving builders from a runtime [`DataType`] list — stage 2's
//! decoder, the struct builder — never has to special-case the type.
//!
//! ```
//! use astrs_data::array::Array;
//! use astrs_data::builder::{ArrayBuilder, NullBuilder};
//!
//! let mut b = NullBuilder::new();
//! b.append_nulls(3);
//! let array = b.finish();
//! assert_eq!(array.len(), 3);
//! assert_eq!(array.null_count(), 3);
//! ```

use std::any::Any;
use std::sync::Arc;

use crate::array::{ArrayRef, NullArray};
use crate::builder::ArrayBuilder;
use crate::datatype::DataType;

/// Row-at-a-time writer producing a [`NullArray`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullBuilder {
    /// Slots appended so far.
    len: usize,
}

impl NullBuilder {
    /// An empty builder.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self { len: 0 }
    }

    /// An empty builder. The capacity is ignored: there are no buffers.
    #[inline]
    #[must_use]
    pub const fn with_capacity(_capacity: usize) -> Self {
        Self::new()
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub const fn finish(&mut self) -> NullArray {
        let len = self.len;
        self.len = 0;
        NullArray::new(len)
    }

    /// Seals the column without resetting the builder.
    #[inline]
    #[must_use]
    pub const fn finish_cloned(&self) -> NullArray {
        NullArray::new(self.len)
    }

    /// Drops every appended slot.
    #[inline]
    pub const fn clear(&mut self) {
        self.len = 0;
    }
}

impl ArrayBuilder for NullBuilder {
    fn len(&self) -> usize {
        self.len
    }

    fn data_type(&self) -> DataType {
        DataType::Null
    }

    fn append_null(&mut self) {
        self.len += 1;
    }

    fn append_nulls(&mut self, count: usize) {
        self.len = self.len.saturating_add(count);
    }

    fn reserve(&mut self, _additional: usize) {
        // Nothing to reserve: a `Null` column has no buffers.
    }

    fn finish_array(&mut self) -> ArrayRef {
        Arc::new(self.finish())
    }

    fn finish_array_cloned(&self) -> ArrayRef {
        Arc::new(self.finish_cloned())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::Array;

    #[test]
    fn counts_appends() {
        let mut builder = NullBuilder::new();
        assert!(builder.is_empty());
        builder.append_null();
        builder.append_nulls(4);
        assert_eq!(builder.len(), 5);
        let array = builder.finish();
        assert_eq!(array.len(), 5);
        assert_eq!(array.null_count(), 5);
        assert!(builder.is_empty(), "finish resets the builder");
    }

    #[test]
    fn finish_cloned_and_clear() {
        let mut builder = NullBuilder::with_capacity(16);
        builder.append_nulls(3);
        assert_eq!(builder.finish_cloned().len(), 3);
        assert_eq!(builder.len(), 3);
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.finish().len(), 0);
    }

    #[test]
    fn dynamic_interface() {
        let mut builder: Box<dyn ArrayBuilder> = Box::new(NullBuilder::new());
        builder.reserve(100);
        builder.append_nulls(2);
        assert_eq!(builder.data_type(), DataType::Null);
        assert_eq!(builder.finish_array_cloned().len(), 2);
        assert_eq!(builder.finish_array().len(), 2);
    }

    #[test]
    fn empty_builder_produces_an_empty_array() {
        assert!(NullBuilder::new().finish_cloned().is_empty());
        assert_eq!(NullBuilder::default(), NullBuilder::new());
    }
}
