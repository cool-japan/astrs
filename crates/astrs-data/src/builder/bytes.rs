//! Builders for the byte-shaped columns: [`GenericBinaryBuilder`],
//! [`GenericStringBuilder`] and [`FixedSizeBinaryBuilder`].
//!
//! The variable-length builders keep a growing value region plus an offset
//! vector, exactly the layout the finished array holds, so `finish` moves the
//! buffers rather than rebuilding them. The string builder is the binary
//! builder with a `&str`-typed front door: because every appended value is
//! already a `&str`, the finished array satisfies the UTF-8 invariant by
//! construction and skips validation entirely.
//!
//! ```
//! use astrs_data::array::Array;
//! use astrs_data::builder::{ArrayBuilder, StringBuilder};
//!
//! let mut b = StringBuilder::with_capacity(3, 32);
//! b.append_value("lidar");
//! b.append_null();
//! b.append_value("日本語");
//!
//! let array = b.finish();
//! assert_eq!(array.len(), 3);
//! assert_eq!(array.get(2), Some("日本語"));
//! ```

use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::array::{
    Array, ArrayRef, FixedSizeBinaryArray, GenericBinaryArray, GenericStringArray, OffsetSizeTrait,
};
use crate::buffer::{AlignedBuf, Buffer, ScalarBuffer};
use crate::builder::{ArrayBuilder, ValidityBuilder};
use crate::datatype::DataType;
use crate::error::{DataError, Result};

/// Row-at-a-time writer producing a [`GenericBinaryArray`].
#[derive(Debug)]
pub struct GenericBinaryBuilder<O: OffsetSizeTrait> {
    /// `len + 1` offsets; starts as `[0]`.
    offsets: Vec<O>,
    /// The flat value region.
    values: AlignedBuf,
    /// Lazily materialised validity.
    validity: ValidityBuilder,
    /// Set once an offset overflows `O`; makes `finish` fall back to a valid
    /// but empty column rather than emitting a corrupt one.
    overflowed: bool,
    /// `GenericBinaryBuilder<O>` produces a `GenericBinaryArray<O>`.
    marker: PhantomData<O>,
}

/// Builds a `Binary` column.
pub type BinaryBuilder = GenericBinaryBuilder<i32>;
/// Builds a `LargeBinary` column.
pub type LargeBinaryBuilder = GenericBinaryBuilder<i64>;

impl<O: OffsetSizeTrait> GenericBinaryBuilder<O> {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0, 0)
    }

    /// An empty builder with room for `values` entries and `bytes` bytes of
    /// value data.
    #[must_use]
    pub fn with_capacity(values: usize, bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(values + 1);
        offsets.push(O::ZERO);
        Self {
            offsets,
            values: AlignedBuf::with_capacity(bytes),
            validity: ValidityBuilder::with_capacity(values),
            overflowed: false,
            marker: PhantomData,
        }
    }

    /// Appends one value.
    pub fn append_value(&mut self, value: impl AsRef<[u8]>) {
        self.values.extend_from_slice(value.as_ref());
        self.push_offset();
        self.validity.append(true);
    }

    /// Appends a value or a null.
    pub fn append_option(&mut self, value: Option<impl AsRef<[u8]>>) {
        match value {
            Some(value) => self.append_value(value),
            None => ArrayBuilder::append_null(self),
        }
    }

    /// Total bytes of value data written so far.
    #[inline]
    #[must_use]
    pub fn value_bytes(&self) -> usize {
        self.values.len()
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> GenericBinaryArray<O> {
        if self.overflowed {
            self.clear();
            return GenericBinaryArray::new_null(0);
        }
        let offsets = std::mem::replace(&mut self.offsets, vec![O::ZERO]);
        let values = std::mem::take(&mut self.values);
        let validity = self.validity.finish();
        GenericBinaryArray::from_parts(
            ScalarBuffer::from_slice(&offsets),
            Buffer::from(values),
            validity,
        )
    }

    /// Seals the column without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> GenericBinaryArray<O> {
        if self.overflowed {
            return GenericBinaryArray::new_null(0);
        }
        GenericBinaryArray::from_parts(
            ScalarBuffer::from_slice(&self.offsets),
            Buffer::from(self.values.clone()),
            self.validity.finish_cloned(),
        )
    }

    /// Drops every appended value, keeping the allocations.
    pub fn clear(&mut self) {
        self.offsets.clear();
        self.offsets.push(O::ZERO);
        self.values.clear();
        self.validity.clear();
        self.overflowed = false;
    }

    /// Records the current value-region length as the next offset.
    ///
    /// A `Binary` column overflows its 32-bit offsets at 2 GiB; rather than
    /// emitting a silently wrong offset the builder latches `overflowed` and
    /// `finish` returns an empty column. Callers who expect that much data
    /// should use [`LargeBinaryBuilder`].
    fn push_offset(&mut self) {
        match O::from_usize(self.values.len()) {
            Some(offset) => self.offsets.push(offset),
            None => {
                self.overflowed = true;
                self.offsets.push(O::ZERO);
            }
        }
    }
}

impl<O: OffsetSizeTrait> Default for GenericBinaryBuilder<O> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: OffsetSizeTrait> ArrayBuilder for GenericBinaryBuilder<O> {
    fn len(&self) -> usize {
        self.validity.len()
    }

    fn data_type(&self) -> DataType {
        O::BINARY_TYPE
    }

    fn append_null(&mut self) {
        self.push_offset();
        self.validity.append(false);
    }

    fn reserve(&mut self, additional: usize) {
        self.offsets.reserve(additional);
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

/// Row-at-a-time writer producing a [`GenericStringArray`].
#[derive(Debug)]
pub struct GenericStringBuilder<O: OffsetSizeTrait> {
    /// The byte layout; every value appended through this type is a `&str`.
    inner: GenericBinaryBuilder<O>,
}

/// Builds a `Utf8` column.
pub type StringBuilder = GenericStringBuilder<i32>;
/// Builds a `LargeUtf8` column.
pub type LargeStringBuilder = GenericStringBuilder<i64>;

impl<O: OffsetSizeTrait> GenericStringBuilder<O> {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: GenericBinaryBuilder::new(),
        }
    }

    /// An empty builder with room for `values` entries and `bytes` bytes of
    /// text.
    #[must_use]
    pub fn with_capacity(values: usize, bytes: usize) -> Self {
        Self {
            inner: GenericBinaryBuilder::with_capacity(values, bytes),
        }
    }

    /// Appends one value.
    pub fn append_value(&mut self, value: impl AsRef<str>) {
        self.inner.append_value(value.as_ref().as_bytes());
    }

    /// Appends a value or a null.
    pub fn append_option(&mut self, value: Option<impl AsRef<str>>) {
        match value {
            Some(value) => self.append_value(value),
            None => ArrayBuilder::append_null(self),
        }
    }

    /// Total bytes of text written so far.
    #[inline]
    #[must_use]
    pub fn value_bytes(&self) -> usize {
        self.inner.value_bytes()
    }

    /// Seals the column, resetting the builder.
    ///
    /// No UTF-8 validation happens: every value came in as a `&str`.
    #[must_use]
    pub fn finish(&mut self) -> GenericStringArray<O> {
        let bytes = self.inner.finish();
        Self::wrap(bytes)
    }

    /// Seals the column without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> GenericStringArray<O> {
        Self::wrap(self.inner.finish_cloned())
    }

    /// Drops every appended value, keeping the allocations.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Reinterprets the byte column as text, which is sound by construction.
    fn wrap(bytes: GenericBinaryArray<O>) -> GenericStringArray<O> {
        GenericStringArray::from_parts(
            ScalarBuffer::from_slice(bytes.value_offsets()),
            bytes.value_data().clone(),
            bytes.validity().cloned(),
        )
    }
}

impl<O: OffsetSizeTrait> Default for GenericStringBuilder<O> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: OffsetSizeTrait> ArrayBuilder for GenericStringBuilder<O> {
    fn len(&self) -> usize {
        self.inner.len()
    }

    fn data_type(&self) -> DataType {
        O::UTF8_TYPE
    }

    fn append_null(&mut self) {
        ArrayBuilder::append_null(&mut self.inner);
    }

    fn reserve(&mut self, additional: usize) {
        self.inner.reserve(additional);
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

impl<O: OffsetSizeTrait, V: AsRef<str>> Extend<V> for GenericStringBuilder<O> {
    fn extend<I: IntoIterator<Item = V>>(&mut self, iter: I) {
        for value in iter {
            self.append_value(value);
        }
    }
}

/// Row-at-a-time writer producing a [`FixedSizeBinaryArray`].
///
/// Every appended value must be exactly `size` bytes; anything else is
/// rejected at `append` rather than at `finish`, so the offending row is easy
/// to attribute.
#[derive(Debug)]
pub struct FixedSizeBinaryBuilder {
    /// Bytes per slot. Always positive.
    size: usize,
    /// `len * size` bytes.
    values: AlignedBuf,
    /// Lazily materialised validity.
    validity: ValidityBuilder,
}

impl FixedSizeBinaryBuilder {
    /// An empty builder for `size`-byte values.
    ///
    /// # Errors
    ///
    /// [`DataError::InvalidFixedSize`] when `size` is not positive.
    pub fn new(size: i32) -> Result<Self> {
        Self::with_capacity(size, 0)
    }

    /// An empty builder with room for `capacity` values.
    ///
    /// # Errors
    ///
    /// [`DataError::InvalidFixedSize`] when `size` is not positive.
    ///
    /// ```
    /// use astrs_data::array::Array;
    /// use astrs_data::builder::{ArrayBuilder, FixedSizeBinaryBuilder};
    ///
    /// let mut b = FixedSizeBinaryBuilder::with_capacity(2, 4)?;
    /// b.append_value(b"ab")?;
    /// b.append_null();
    /// assert!(b.append_value(b"too long").is_err());
    /// assert_eq!(b.finish().len(), 2);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn with_capacity(size: i32, capacity: usize) -> Result<Self> {
        let width = crate::array::check_fixed_size(size)?;
        Ok(Self {
            size: width,
            values: AlignedBuf::with_capacity(capacity.saturating_mul(width)),
            validity: ValidityBuilder::with_capacity(capacity),
        })
    }

    /// Bytes per slot.
    #[inline]
    #[must_use]
    pub const fn value_size(&self) -> usize {
        self.size
    }

    /// Appends one value.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`] when the value is not exactly
    /// `size` bytes; the builder is left untouched.
    pub fn append_value(&mut self, value: impl AsRef<[u8]>) -> Result<()> {
        let bytes = value.as_ref();
        if bytes.len() != self.size {
            return Err(DataError::ChildLengthMismatch {
                expected: self.size,
                actual: bytes.len(),
            });
        }
        self.values.extend_from_slice(bytes);
        self.validity.append(true);
        Ok(())
    }

    /// Appends a value or a null.
    ///
    /// # Errors
    ///
    /// As [`FixedSizeBinaryBuilder::append_value`].
    pub fn append_option(&mut self, value: Option<impl AsRef<[u8]>>) -> Result<()> {
        match value {
            Some(value) => self.append_value(value),
            None => {
                ArrayBuilder::append_null(self);
                Ok(())
            }
        }
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> FixedSizeBinaryArray {
        let values = std::mem::take(&mut self.values);
        let validity = self.validity.finish();
        self.assemble(values, validity)
    }

    /// Seals the column without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> FixedSizeBinaryArray {
        self.assemble(self.values.clone(), self.validity.finish_cloned())
    }

    /// Drops every appended value, keeping the allocation.
    pub fn clear(&mut self) {
        self.values.clear();
        self.validity.clear();
    }

    /// Shared tail of `finish` and `finish_cloned`.
    fn assemble(
        &self,
        values: AlignedBuf,
        validity: Option<crate::buffer::Bitmap>,
    ) -> FixedSizeBinaryArray {
        let size = i32::try_from(self.size).unwrap_or(1);
        FixedSizeBinaryArray::from_parts(size, Buffer::from(values), validity)
    }
}

impl ArrayBuilder for FixedSizeBinaryBuilder {
    fn len(&self) -> usize {
        self.validity.len()
    }

    fn data_type(&self) -> DataType {
        DataType::FixedSizeBinary(i32::try_from(self.size).unwrap_or(1))
    }

    fn append_null(&mut self) {
        self.values.extend_zeroed(self.size);
        self.validity.append(false);
    }

    fn append_nulls(&mut self, count: usize) {
        self.values.extend_zeroed(count.saturating_mul(self.size));
        self.validity.append_n(count, false);
    }

    fn reserve(&mut self, additional: usize) {
        self.values.reserve(additional.saturating_mul(self.size));
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
    fn empty_binary_builder() {
        let mut builder = BinaryBuilder::new();
        assert!(builder.is_empty());
        let array = builder.finish();
        assert!(array.is_empty());
        assert_eq!(array.value_offsets(), &[0]);
        assert!(array.validity().is_none());
        assert_eq!(array.data_type(), &DataType::Binary);
    }

    #[test]
    fn binary_round_trip() {
        let mut builder = BinaryBuilder::with_capacity(3, 16);
        builder.append_value(b"aa");
        builder.append_null();
        builder.append_option(Some(&b"cccc"[..]));
        builder.append_option(None::<&[u8]>);
        assert_eq!(builder.value_bytes(), 6);
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(array.get(0), Some(&b"aa"[..]));
        assert_eq!(array.get(1), None);
        assert_eq!(array.get(2), Some(&b"cccc"[..]));
        assert_eq!(array.value_offsets(), &[0, 2, 2, 6, 6]);
        assert!(builder.is_empty());
    }

    #[test]
    fn binary_all_null() {
        let mut builder = LargeBinaryBuilder::new();
        builder.append_nulls(4);
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 4);
        assert_eq!(array.data_type(), &DataType::LargeBinary);
    }

    #[test]
    fn binary_finish_cloned_and_clear() {
        let mut builder = BinaryBuilder::new();
        builder.append_value(b"x");
        let snapshot = builder.finish_cloned();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(builder.len(), 1);
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.finish().len(), 0);
        assert_eq!(snapshot.get(0), Some(&b"x"[..]));
    }

    #[test]
    fn string_round_trip() {
        let mut builder = StringBuilder::with_capacity(3, 32);
        builder.append_value("lidar");
        builder.append_null();
        builder.append_value("日本語");
        builder.append_option(Some("🤖"));
        builder.append_option(None::<&str>);
        let array = builder.finish();
        assert_eq!(array.len(), 5);
        assert_eq!(array.null_count(), 2);
        assert_eq!(array.get(0), Some("lidar"));
        assert_eq!(array.get(2), Some("日本語"));
        assert_eq!(array.get(3), Some("🤖"));
        assert_eq!(array.data_type(), &DataType::Utf8);
    }

    #[test]
    fn string_offsets_land_on_char_boundaries() {
        let mut builder = LargeStringBuilder::new();
        for text in ["é", "日本", "🤖", ""] {
            builder.append_value(text);
        }
        let array = builder.finish();
        // Rebuilding through the validating constructor must succeed, which is
        // the real proof the builder never splits a code point.
        let rebuilt = crate::array::LargeStringArray::try_new(
            ScalarBuffer::from_slice(array.value_offsets()),
            array.value_data().clone(),
            None,
        )
        .unwrap();
        assert_eq!(rebuilt, array);
    }

    #[test]
    fn string_extend_and_owned_values() {
        let mut builder = StringBuilder::new();
        builder.extend(["a", "b"]);
        builder.append_value(String::from("owned"));
        assert_eq!(builder.value_bytes(), 7);
        let array = builder.finish();
        assert_eq!(array.len(), 3);
        assert_eq!(array.get(2), Some("owned"));
    }

    #[test]
    fn string_finish_cloned_and_clear() {
        let mut builder = StringBuilder::new();
        builder.append_value("keep");
        let snapshot = builder.finish_cloned();
        assert_eq!(snapshot.get(0), Some("keep"));
        builder.clear();
        assert!(builder.is_empty());
    }

    #[test]
    fn fixed_size_binary_round_trip() {
        let mut builder = FixedSizeBinaryBuilder::with_capacity(2, 4).unwrap();
        assert_eq!(builder.value_size(), 2);
        builder.append_value(b"ab").unwrap();
        builder.append_null();
        builder.append_option(Some(&b"cd"[..])).unwrap();
        builder.append_option(None::<&[u8]>).unwrap();
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(array.get(0), Some(&b"ab"[..]));
        assert_eq!(array.get(2), Some(&b"cd"[..]));
        assert_eq!(array.data_type(), &DataType::FixedSizeBinary(2));
    }

    #[test]
    fn fixed_size_binary_rejects_wrong_widths_without_corrupting_state() {
        let mut builder = FixedSizeBinaryBuilder::new(2).unwrap();
        builder.append_value(b"ab").unwrap();
        assert_eq!(
            builder.append_value(b"abc").unwrap_err(),
            DataError::ChildLengthMismatch {
                expected: 2,
                actual: 3
            }
        );
        assert_eq!(builder.len(), 1, "the failed append changed nothing");
        builder.append_value(b"cd").unwrap();
        let array = builder.finish();
        assert_eq!(array.len(), 2);
        assert_eq!(array.get(1), Some(&b"cd"[..]));
    }

    #[test]
    fn fixed_size_binary_rejects_invalid_sizes() {
        assert!(FixedSizeBinaryBuilder::new(0).is_err());
        assert!(FixedSizeBinaryBuilder::new(-1).is_err());
        assert!(FixedSizeBinaryBuilder::with_capacity(-4, 10).is_err());
    }

    #[test]
    fn fixed_size_binary_finish_cloned_and_clear() {
        let mut builder = FixedSizeBinaryBuilder::new(1).unwrap();
        builder.append_value(b"x").unwrap();
        let snapshot = builder.finish_cloned();
        assert_eq!(snapshot.get(0), Some(&b"x"[..]));
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.finish().len(), 0);
    }

    #[test]
    fn dynamic_interface() {
        let mut builders: Vec<Box<dyn ArrayBuilder>> = vec![
            Box::new(BinaryBuilder::new()),
            Box::new(LargeBinaryBuilder::new()),
            Box::new(StringBuilder::new()),
            Box::new(LargeStringBuilder::new()),
            Box::new(FixedSizeBinaryBuilder::new(3).unwrap()),
        ];
        for builder in &mut builders {
            builder.reserve(4);
            builder.append_nulls(2);
            assert_eq!(builder.len(), 2);
            let array = builder.finish_array();
            assert_eq!(array.len(), 2);
            assert_eq!(array.null_count(), 2);
        }
    }

    #[test]
    fn large_text_column() {
        let mut builder = LargeStringBuilder::with_capacity(10_000, 200_000);
        for index in 0..10_000 {
            builder.append_option((index % 5 != 0).then(|| format!("node-{index}")));
        }
        let array = builder.finish();
        assert_eq!(array.len(), 10_000);
        assert_eq!(array.null_count(), 2_000);
        assert_eq!(array.get(1), Some("node-1"));
        assert_eq!(array.get(5), None);
    }
}
