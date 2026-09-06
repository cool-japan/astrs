//! Row-at-a-time writers for every array family.
//!
//! Arrays are immutable; builders are how they come into existence. Every
//! builder follows the same four-call shape:
//!
//! ```text
//!   Builder::with_capacity(n)   pre-size the buffers
//!   append_value(v)             one value
//!   append_null()               one null
//!   append_option(Some(v)/None) either, from an Option
//!   finish() -> ConcreteArray   seal it, resetting the builder for reuse
//! ```
//!
//! [`ArrayBuilder`] is the dynamic face of that shape: it carries only the
//! type-independent half (`len`, `append_null`, `finish_array`), which is what
//! [`StructBuilder`] needs to hold a heterogeneous set of child builders in a
//! `Vec<Box<dyn ArrayBuilder>>`. The value-typed half stays inherent on each
//! concrete builder, so `append_value` takes `i32`, `&str` or `&[u8]` directly
//! rather than going through an enum.
//!
//! # Validity is lazy
//!
//! A builder that never sees a null never allocates a validity bitmap: the
//! internal [`ValidityBuilder`] starts empty and back-fills `len` set bits the
//! first time [`ArrayBuilder::append_null`] is called. Columns without nulls —
//! the common case for sensor payloads — therefore cost one buffer, not two.
//!
//! ```
//! use astrs_data::array::Array;
//! use astrs_data::builder::{ArrayBuilder, Int32Builder};
//!
//! let mut b = Int32Builder::with_capacity(4);
//! b.append_value(1);
//! b.append_option(Some(2));
//! b.append_option(None);
//! b.append_null();
//!
//! let array = b.finish();
//! assert_eq!(array.len(), 4);
//! assert_eq!(array.null_count(), 2);
//! assert!(b.is_empty(), "finish resets the builder");
//! ```

pub mod boolean;
pub mod bytes;
pub mod nested;
pub mod null;
pub mod primitive;
pub mod temporal;

use std::any::Any;

pub use crate::builder::boolean::BooleanBuilder;
pub use crate::builder::bytes::{
    BinaryBuilder, FixedSizeBinaryBuilder, GenericBinaryBuilder, GenericStringBuilder,
    LargeBinaryBuilder, LargeStringBuilder, StringBuilder,
};
pub use crate::builder::nested::{FixedSizeListBuilder, ListBuilder, StructBuilder};
pub use crate::builder::null::NullBuilder;
pub use crate::builder::primitive::{
    Float16Builder, Float32Builder, Float64Builder, Int8Builder, Int16Builder, Int32Builder,
    Int64Builder, PrimitiveBuilder, UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
pub use crate::builder::temporal::{DurationBuilder, TimestampBuilder};

use crate::array::ArrayRef;
use crate::buffer::{Bitmap, BitmapBuilder};
use crate::datatype::DataType;

/// The type-independent half of every builder.
///
/// Object-safe on purpose: [`StructBuilder`] stores `Box<dyn ArrayBuilder>`
/// children, and stage 2's decoder builds a column set from a runtime
/// [`DataType`] list.
pub trait ArrayBuilder: std::fmt::Debug + Send + Sync + 'static {
    /// Values appended since the last `finish`.
    fn len(&self) -> usize;

    /// Returns `true` when nothing has been appended.
    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The type of array this builder produces.
    fn data_type(&self) -> DataType;

    /// Appends one null.
    fn append_null(&mut self);

    /// Appends `count` nulls.
    fn append_nulls(&mut self, count: usize) {
        for _ in 0..count {
            self.append_null();
        }
    }

    /// Reserves room for `additional` more values.
    fn reserve(&mut self, additional: usize);

    /// Seals the column and resets the builder.
    fn finish_array(&mut self) -> ArrayRef;

    /// Seals the column without resetting the builder.
    fn finish_array_cloned(&self) -> ArrayRef;

    /// Erased self, so a `Box<dyn ArrayBuilder>` child can be downcast back to
    /// its concrete type to call `append_value`.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Downcast helper for `dyn ArrayBuilder`.
///
/// ```
/// use astrs_data::builder::{ArrayBuilder, BuilderExt, Int32Builder};
///
/// let mut boxed: Box<dyn ArrayBuilder> = Box::new(Int32Builder::new());
/// boxed.downcast_mut::<Int32Builder>().map(|b| b.append_value(7));
/// assert_eq!(boxed.len(), 1);
/// ```
pub trait BuilderExt {
    /// Downcasts to a concrete builder, or `None` on a type mismatch.
    fn downcast_mut<B: ArrayBuilder>(&mut self) -> Option<&mut B>;
}

impl BuilderExt for dyn ArrayBuilder {
    #[inline]
    fn downcast_mut<B: ArrayBuilder>(&mut self) -> Option<&mut B> {
        self.as_any_mut().downcast_mut::<B>()
    }
}

impl BuilderExt for Box<dyn ArrayBuilder> {
    #[inline]
    fn downcast_mut<B: ArrayBuilder>(&mut self) -> Option<&mut B> {
        self.as_mut().as_any_mut().downcast_mut::<B>()
    }
}

/// A validity bitmap that only materialises once a null appears.
///
/// Shared by every builder. Appending `true` while no null has been seen is a
/// counter bump and nothing else; the first `false` back-fills the set bits.
#[derive(Debug, Default)]
pub struct ValidityBuilder {
    /// `None` until the first null.
    bits: Option<BitmapBuilder>,
    /// Slots appended so far.
    len: usize,
}

impl ValidityBuilder {
    /// A builder with room for `capacity` slots.
    ///
    /// No allocation happens until a null is appended.
    #[must_use]
    pub const fn with_capacity(_capacity: usize) -> Self {
        Self { bits: None, len: 0 }
    }

    /// Slots appended so far.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when nothing has been appended.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns `true` once at least one null has been appended.
    #[inline]
    #[must_use]
    pub const fn has_nulls(&self) -> bool {
        self.bits.is_some()
    }

    /// Appends one slot.
    #[inline]
    pub fn append(&mut self, valid: bool) {
        if valid {
            if let Some(bits) = &mut self.bits {
                bits.append(true);
            }
        } else {
            let len = self.len;
            let bits = self.bits.get_or_insert_with(|| {
                let mut builder = BitmapBuilder::with_capacity(len + 1);
                builder.append_n(len, true);
                builder
            });
            bits.append(false);
        }
        self.len += 1;
    }

    /// Appends `count` slots with the same validity.
    pub fn append_n(&mut self, count: usize, valid: bool) {
        if count == 0 {
            return;
        }
        if valid {
            if let Some(bits) = &mut self.bits {
                bits.append_n(count, true);
            }
            self.len += count;
        } else {
            let len = self.len;
            let bits = self.bits.get_or_insert_with(|| {
                let mut builder = BitmapBuilder::with_capacity(len + count);
                builder.append_n(len, true);
                builder
            });
            bits.append_n(count, false);
            self.len += count;
        }
    }

    /// Seals the bitmap and resets the builder.
    ///
    /// Returns `None` when no null was ever appended, which is what tells the
    /// array to omit its validity buffer entirely.
    pub fn finish(&mut self) -> Option<Bitmap> {
        self.len = 0;
        self.bits.take().map(|mut bits| bits.finish())
    }

    /// Seals the bitmap without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> Option<Bitmap> {
        self.bits.as_ref().map(BitmapBuilder::finish_cloned)
    }

    /// Drops every appended slot.
    pub fn clear(&mut self) {
        self.bits = None;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn validity_stays_absent_without_nulls() {
        let mut validity = ValidityBuilder::with_capacity(8);
        for _ in 0..5 {
            validity.append(true);
        }
        assert_eq!(validity.len(), 5);
        assert!(!validity.has_nulls());
        assert!(validity.finish().is_none());
        assert!(validity.is_empty(), "finish resets");
    }

    #[test]
    fn validity_backfills_on_the_first_null() {
        let mut validity = ValidityBuilder::default();
        validity.append_n(5, true);
        validity.append(false);
        validity.append(true);
        assert!(validity.has_nulls());
        assert_eq!(validity.len(), 7);
        let bits = validity.finish().unwrap();
        assert_eq!(bits.len(), 7);
        assert_eq!(
            bits.iter().collect::<Vec<_>>(),
            vec![true, true, true, true, true, false, true]
        );
    }

    #[test]
    fn validity_append_n_of_nulls() {
        let mut validity = ValidityBuilder::default();
        validity.append(true);
        validity.append_n(3, false);
        validity.append_n(0, false);
        let bits = validity.finish().unwrap();
        assert_eq!(bits.len(), 4);
        assert_eq!(bits.count_unset(), 3);
    }

    #[test]
    fn validity_finish_cloned_and_clear() {
        let mut validity = ValidityBuilder::default();
        validity.append(false);
        assert_eq!(validity.finish_cloned().map(|b| b.len()), Some(1));
        assert_eq!(validity.len(), 1, "finish_cloned keeps the builder");
        validity.clear();
        assert!(validity.is_empty());
        assert!(!validity.has_nulls());
    }

    #[test]
    fn builders_are_object_safe_and_downcastable() {
        let mut boxed: Box<dyn ArrayBuilder> = Box::new(Int32Builder::new());
        boxed.append_null();
        if let Some(concrete) = boxed.downcast_mut::<Int32Builder>() {
            concrete.append_value(7);
        }
        assert_eq!(boxed.len(), 2);
        assert!(boxed.downcast_mut::<BooleanBuilder>().is_none());
        let array = boxed.finish_array();
        assert_eq!(array.len(), 2);
        assert_eq!(array.null_count(), 1);
    }

    #[test]
    fn append_nulls_default_walks_the_single_path() {
        let mut boxed: Box<dyn ArrayBuilder> = Box::new(StringBuilder::new());
        boxed.append_nulls(3);
        boxed.reserve(10);
        assert_eq!(boxed.len(), 3);
        assert_eq!(boxed.data_type(), DataType::Utf8);
        assert_eq!(boxed.finish_array().null_count(), 3);
    }

    #[test]
    fn every_family_has_a_builder() {
        let builders: Vec<Box<dyn ArrayBuilder>> = vec![
            Box::new(NullBuilder::new()),
            Box::new(BooleanBuilder::new()),
            Box::new(Int8Builder::new()),
            Box::new(Int16Builder::new()),
            Box::new(Int32Builder::new()),
            Box::new(Int64Builder::new()),
            Box::new(UInt8Builder::new()),
            Box::new(UInt16Builder::new()),
            Box::new(UInt32Builder::new()),
            Box::new(UInt64Builder::new()),
            Box::new(Float16Builder::new()),
            Box::new(Float32Builder::new()),
            Box::new(Float64Builder::new()),
            Box::new(BinaryBuilder::new()),
            Box::new(LargeBinaryBuilder::new()),
            Box::new(StringBuilder::new()),
            Box::new(LargeStringBuilder::new()),
            Box::new(TimestampBuilder::new()),
            Box::new(DurationBuilder::new()),
        ];
        assert_eq!(builders.len(), 19);
        for mut builder in builders {
            let data_type = builder.data_type();
            builder.append_null();
            let array = builder.finish_array();
            assert_eq!(array.len(), 1, "{data_type}");
            assert_eq!(array.data_type(), &data_type, "{data_type}");
            assert!(
                crate::array::Array::is_null(array.as_ref(), 0),
                "{data_type}"
            );
        }
    }
}
