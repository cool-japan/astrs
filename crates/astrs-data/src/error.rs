//! The crate-wide error type.
//!
//! `astrs-data` funnels every recoverable failure through [`DataError`]. The
//! variants are deliberately structured (fields, not pre-formatted strings) so
//! callers — the node API, the daemon, the CLI diagnostics — can react to a
//! specific failure without parsing text.
//!
//! [`DataError`] is `Clone + PartialEq + Eq`, which keeps assertions in tests
//! and in the conformance zoo readable:
//!
//! ```
//! use astrs_data::{Bitmap, DataError};
//!
//! let bits = Bitmap::new_set(10);
//! assert_eq!(
//!     bits.try_slice(8, 5).unwrap_err(),
//!     DataError::SliceOutOfBounds { offset: 8, len: 5, available: 10 }
//! );
//! ```

use crate::datatype::DataType;
use crate::urn::TypeUrnError;

/// Result alias used throughout `astrs-data`.
pub type Result<T, E = DataError> = core::result::Result<T, E>;

/// Every recoverable failure the columnar core can report.
///
/// The enum is `#[non_exhaustive]`: stages 2 and 3 (Arrow IPC, kernels) extend
/// it at the tail, and downstream crates must keep a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DataError {
    /// A requested buffer capacity cannot be expressed as a valid allocation
    /// layout (it overflows `isize::MAX` once padded to the 64-byte alignment).
    #[error("buffer capacity overflow: {requested} bytes cannot be allocated")]
    CapacityOverflow {
        /// The capacity, in bytes, that was requested.
        requested: usize,
    },

    /// The global allocator refused an allocation.
    #[error("allocation of {bytes} bytes failed")]
    AllocationFailed {
        /// Size of the failed allocation, in bytes.
        bytes: usize,
    },

    /// A buffer is shorter than the layout it is supposed to back.
    #[error("buffer holds {actual} bytes but {required} are required")]
    BufferTooSmall {
        /// Bytes the layout needs.
        required: usize,
        /// Bytes the buffer actually holds.
        actual: usize,
    },

    /// A byte window cannot be reinterpreted as a typed slice because its
    /// start address is not aligned for the element type.
    #[error("buffer address is {actual}-byte aligned but {required}-byte alignment is required")]
    UnalignedBuffer {
        /// Alignment the element type requires, in bytes.
        required: usize,
        /// Largest power-of-two alignment the buffer address actually has.
        actual: usize,
    },

    /// A byte window cannot be reinterpreted as a typed slice because its
    /// length is not a whole number of elements.
    #[error("buffer length {len} is not a multiple of the {width}-byte element width")]
    BufferLengthNotMultiple {
        /// Length of the byte window.
        len: usize,
        /// Width of one element, in bytes.
        width: usize,
    },

    /// String data failed UTF-8 validation.
    #[error("invalid UTF-8 in string values: first error at byte {valid_up_to}")]
    InvalidUtf8 {
        /// Index of the first byte that is not part of a valid sequence.
        valid_up_to: usize,
    },

    /// An offset in a `Utf8`/`LargeUtf8` array splits a multi-byte code point.
    ///
    /// The concatenated value region can be valid UTF-8 while an individual
    /// slice boundary is not — both checks are required.
    #[error("offset {offset} (entry {index}) is not a UTF-8 character boundary")]
    OffsetNotCharBoundary {
        /// Index of the offending entry in the offset buffer.
        index: usize,
        /// The byte offset that fell inside a code point.
        offset: usize,
    },

    /// An offset buffer does not have `len + 1` entries.
    #[error("offset buffer holds {actual} entries but {expected} are required")]
    OffsetBufferLength {
        /// Entries required (`array length + 1`).
        expected: usize,
        /// Entries present.
        actual: usize,
    },

    /// An offset buffer is not monotonically non-decreasing.
    #[error("offsets are not monotonic: entry {index} ({offset}) precedes entry {index}-1")]
    NonMonotonicOffsets {
        /// Index of the first offset smaller than its predecessor.
        index: usize,
        /// Value of that offset.
        offset: i64,
    },

    /// An offset buffer contains a negative entry.
    #[error("offset entry {index} is negative ({offset})")]
    NegativeOffset {
        /// Index of the negative entry.
        index: usize,
        /// Value of that entry.
        offset: i64,
    },

    /// The last offset points past the end of the values buffer.
    #[error("offset entry {index} ({offset}) exceeds the {values_len}-byte value region")]
    OffsetOutOfBounds {
        /// Index of the offending entry.
        index: usize,
        /// Value of that entry.
        offset: usize,
        /// Length of the region the offsets index into.
        values_len: usize,
    },

    /// A validity bitmap does not cover exactly the array's logical length.
    #[error("validity bitmap covers {validity_len} slots but the array has {array_len}")]
    ValidityLengthMismatch {
        /// Logical length of the array.
        array_len: usize,
        /// Number of bits in the supplied bitmap.
        validity_len: usize,
    },

    /// Two bitmaps of different lengths were combined.
    #[error("bitmap length mismatch: {left} vs {right}")]
    BitmapLengthMismatch {
        /// Length of the left operand.
        left: usize,
        /// Length of the right operand.
        right: usize,
    },

    /// A checked `try_slice` request left the available range.
    ///
    /// The infallible `slice` methods clamp instead of reporting this.
    #[error("slice [{offset}, {offset}+{len}) leaves the available range of {available}")]
    SliceOutOfBounds {
        /// Start of the requested window.
        offset: usize,
        /// Length of the requested window.
        len: usize,
        /// Length actually available.
        available: usize,
    },

    /// A checked element access left the array.
    #[error("index {index} is out of bounds for length {len}")]
    IndexOutOfBounds {
        /// The index that was requested.
        index: usize,
        /// Length of the collection.
        len: usize,
    },

    /// A `FixedSizeBinary`/`FixedSizeList` width is not a positive value.
    #[error("fixed size must be positive, got {size}")]
    InvalidFixedSize {
        /// The rejected width.
        size: i32,
    },

    /// A nested array's child does not have the length its parent implies.
    #[error("child array holds {actual} values but the parent implies {expected}")]
    ChildLengthMismatch {
        /// Length the parent layout implies.
        expected: usize,
        /// Length the child actually has.
        actual: usize,
    },

    /// A value's data type does not match the type its container declares.
    #[error("type mismatch: expected {expected}, found {actual}")]
    TypeMismatch {
        /// Type the container declares.
        expected: Box<DataType>,
        /// Type actually supplied.
        actual: Box<DataType>,
    },

    /// A dynamic downcast on an [`crate::array::ArrayRef`] failed.
    #[error("cannot downcast an array of type {actual} to {expected}")]
    DowncastFailed {
        /// Rust type name of the requested concrete array.
        expected: &'static str,
        /// Data type of the array actually held.
        actual: Box<DataType>,
    },

    /// A record batch was built with a column count that disagrees with its
    /// schema.
    #[error("schema declares {fields} field(s) but {columns} column(s) were supplied")]
    ColumnCountMismatch {
        /// Number of fields in the schema.
        fields: usize,
        /// Number of columns supplied.
        columns: usize,
    },

    /// A record batch column has a different length from the batch.
    #[error("column {index} ({name}) holds {actual} rows but the batch has {expected}")]
    ColumnLengthMismatch {
        /// Position of the offending column.
        index: usize,
        /// Name of the field the column belongs to.
        name: String,
        /// Row count the batch declares.
        expected: usize,
        /// Row count the column has.
        actual: usize,
    },

    /// A column carries nulls for a field the schema declares non-nullable.
    #[error("column {index} ({name}) holds {null_count} null(s) in a non-nullable field")]
    NullsInNonNullableColumn {
        /// Position of the offending column.
        index: usize,
        /// Name of the field.
        name: String,
        /// Number of nulls found.
        null_count: usize,
    },

    /// A schema declares the same field name twice.
    #[error("duplicate field name {name:?} in schema")]
    DuplicateFieldName {
        /// The repeated name.
        name: String,
    },

    /// A field lookup by name found nothing.
    #[error("no field named {name:?} in schema")]
    FieldNotFound {
        /// The name that was looked up.
        name: String,
    },

    /// A struct array was built without children and without an explicit
    /// length, so its logical length is undefined.
    #[error("a struct array with no child columns needs an explicit length")]
    UnknownStructLength,

    /// A type URN could not be parsed or resolved.
    #[error(transparent)]
    TypeUrn(#[from] TypeUrnError),

    /// A [`crate::tensor::TensorView`] was built from a flat buffer whose
    /// length does not equal the product of the requested shape.
    #[error("tensor shape implies {expected} elements but the buffer holds {actual}")]
    TensorShapeMismatch {
        /// Product of the requested shape's dimensions.
        expected: usize,
        /// Length of the buffer actually supplied.
        actual: usize,
    },

    /// A [`crate::tensor::TensorView`] shape's dimensions multiply out to
    /// more elements than `usize` can count.
    ///
    /// Reported instead of letting the product silently wrap (or panic on a
    /// debug build) — no real buffer can back such a shape, so this is
    /// always a caller/decode mistake, never a size this crate could ever
    /// satisfy.
    #[error("tensor shape {shape:?} needs more elements than usize can represent")]
    TensorShapeOverflow {
        /// The requested shape, verbatim.
        shape: Vec<usize>,
    },

    /// A [`crate::tensor::TensorView`] index did not name one component per
    /// dimension.
    #[error("tensor index has {actual} component(s) but the tensor has {expected} dimension(s)")]
    TensorRankMismatch {
        /// The tensor's dimensionality.
        expected: usize,
        /// Components the index actually supplied.
        actual: usize,
    },

    /// A [`crate::tensor::TensorView`] axis operation named an axis the
    /// tensor does not have.
    #[error("axis {axis} is out of bounds for a {ndim}-dimensional tensor")]
    TensorAxisOutOfBounds {
        /// The axis that was requested.
        axis: usize,
        /// The tensor's dimensionality.
        ndim: usize,
    },

    /// A [`crate::tensor::TensorView`] index's component for one axis is not
    /// smaller than that axis's extent.
    #[error("index {index} is out of bounds for axis {axis} (extent {dim})")]
    TensorIndexOutOfBounds {
        /// The axis the offending component belongs to.
        axis: usize,
        /// The component's value.
        index: usize,
        /// The axis's extent.
        dim: usize,
    },

    /// A row-level accessor (for example
    /// [`crate::tensor::ImageView::from_struct_row`]) found a null in a
    /// field the layout declares non-nullable.
    #[error("field {field:?} is null at row {row}, but the layout declares it non-nullable")]
    RequiredFieldIsNull {
        /// The field's name.
        field: String,
        /// The row at which it was found null.
        row: usize,
    },

    /// [`crate::kernel::concat()`] (or a `Large*` narrowing
    /// [`crate::kernel::cast()`]) needed more offset range than the
    /// destination's offset width can express.
    #[error("concatenated length {total} exceeds the {max}-byte/-element offset width")]
    OffsetWidthOverflow {
        /// Bytes or child elements the result would need.
        total: usize,
        /// The offset type's maximum representable value.
        max: usize,
    },

    /// [`crate::kernel::cast()`] has no defined conversion between two types
    /// (for example `Bool` to anything, or `Timestamp` to `Duration`
    /// directly — the type system treats those as different units on
    /// purpose; go through `Int64` explicitly if a conversion is really
    /// intended).
    #[error("no defined cast from {from} to {to}")]
    UnsupportedCast {
        /// The source type.
        from: Box<DataType>,
        /// The requested destination type.
        to: Box<DataType>,
    },

    /// [`crate::kernel::cast()`] under [`crate::kernel::OverflowPolicy::Error`]
    /// found a value that does not fit the destination type.
    #[error("value at index {index} does not fit while casting {from} to {to}")]
    CastOverflow {
        /// The source type.
        from: Box<DataType>,
        /// The destination type.
        to: Box<DataType>,
        /// The offending row.
        index: usize,
    },

    /// [`crate::kernel::filter()`]'s mask is a different length from the array
    /// it is filtering.
    #[error("filter mask holds {mask_len} value(s) but the array has {array_len}")]
    MaskLengthMismatch {
        /// The array's length.
        array_len: usize,
        /// The mask's length.
        mask_len: usize,
    },

    /// [`crate::kernel::take()`] was given an index that is negative, does not
    /// fit `usize`, or is not smaller than the source's length.
    #[error("take index at position {position} does not name a row (source has {len})")]
    TakeIndexInvalid {
        /// The offending index's position within the indices array.
        position: usize,
        /// The source array's length.
        len: usize,
    },

    /// [`crate::message::AstrsMessage::from_record_batch`] requires exactly
    /// one row per message (blueprint §6.1's "one record batch per message"
    /// convention), and the batch supplied did not have one.
    #[error("expected exactly one message row but the batch has {actual}")]
    MessageRowCount {
        /// The batch's actual row count.
        actual: usize,
    },

    /// A `[T; N]`-shaped message field ([`crate::message::AstrsMessage`])
    /// decoded a slice with a different length than `N`.
    ///
    /// Unreachable in practice — [`crate::array::FixedSizeListArray`]
    /// guarantees every row's child slice is exactly `N` elements wide by
    /// construction — but `TryFrom<&[T]> for [T; N]` is still fallible at
    /// the type level, so a `#[derive(AstrsMessage)]` decoder must handle
    /// the branch rather than `unwrap`/`expect` it away.
    #[error("fixed-size field {field:?} decoded {actual} element(s), expected {expected}")]
    MessageFixedArrayLength {
        /// The field's name.
        field: String,
        /// The array size the layout declares.
        expected: usize,
        /// The number of elements actually decoded.
        actual: usize,
    },
}

impl DataError {
    /// Builds a [`DataError::TypeMismatch`] without repeating the boxing at
    /// every call site.
    ///
    /// ```
    /// use astrs_data::{DataError, DataType};
    ///
    /// let err = DataError::type_mismatch(DataType::Int32, DataType::Int64);
    /// assert_eq!(err.to_string(), "type mismatch: expected Int32, found Int64");
    /// ```
    #[must_use]
    pub fn type_mismatch(expected: DataType, actual: DataType) -> Self {
        Self::TypeMismatch {
            expected: Box::new(expected),
            actual: Box::new(actual),
        }
    }

    /// Builds a [`DataError::DowncastFailed`] for the concrete array type `A`.
    #[must_use]
    pub fn downcast_failed<A: ?Sized>(actual: DataType) -> Self {
        Self::DowncastFailed {
            expected: core::any::type_name::<A>(),
            actual: Box::new(actual),
        }
    }

    /// Builds a [`DataError::UnsupportedCast`] without repeating the boxing
    /// at every call site.
    #[must_use]
    pub fn unsupported_cast(from: DataType, to: DataType) -> Self {
        Self::UnsupportedCast {
            from: Box::new(from),
            to: Box::new(to),
        }
    }

    /// Builds a [`DataError::CastOverflow`] without repeating the boxing at
    /// every call site.
    #[must_use]
    pub fn cast_overflow(from: DataType, to: DataType, index: usize) -> Self {
        Self::CastOverflow {
            from: Box::new(from),
            to: Box::new(to),
            index,
        }
    }

    /// Returns `true` when the error describes a malformed buffer layout
    /// rather than a caller mistake.
    ///
    /// Stage 2 uses this to decide whether a decode failure should be reported
    /// as a protocol violation (peer sent garbage) or as a local bug.
    ///
    /// ```
    /// use astrs_data::DataError;
    ///
    /// assert!(DataError::InvalidUtf8 { valid_up_to: 3 }.is_layout_violation());
    /// assert!(!DataError::IndexOutOfBounds { index: 9, len: 2 }.is_layout_violation());
    /// ```
    #[must_use]
    pub const fn is_layout_violation(&self) -> bool {
        matches!(
            self,
            Self::BufferTooSmall { .. }
                | Self::UnalignedBuffer { .. }
                | Self::BufferLengthNotMultiple { .. }
                | Self::InvalidUtf8 { .. }
                | Self::OffsetNotCharBoundary { .. }
                | Self::OffsetBufferLength { .. }
                | Self::NonMonotonicOffsets { .. }
                | Self::NegativeOffset { .. }
                | Self::OffsetOutOfBounds { .. }
                | Self::ValidityLengthMismatch { .. }
                | Self::ChildLengthMismatch { .. }
                | Self::TensorShapeMismatch { .. }
                | Self::TensorShapeOverflow { .. }
                | Self::RequiredFieldIsNull { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn display_is_actionable() {
        let err = DataError::ColumnLengthMismatch {
            index: 1,
            name: "name".to_owned(),
            expected: 3,
            actual: 2,
        };
        assert_eq!(
            err.to_string(),
            "column 1 (name) holds 2 rows but the batch has 3"
        );
    }

    #[test]
    fn type_urn_errors_flatten() {
        let err: DataError = TypeUrnError::Empty.into();
        assert_eq!(err.to_string(), TypeUrnError::Empty.to_string());
    }

    #[test]
    fn helpers_construct_boxed_variants() {
        let err = DataError::downcast_failed::<u32>(DataType::Utf8);
        match err {
            DataError::DowncastFailed { expected, actual } => {
                assert_eq!(expected, "u32");
                assert_eq!(*actual, DataType::Utf8);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn layout_violations_are_classified() {
        assert!(
            DataError::NonMonotonicOffsets {
                index: 1,
                offset: -1
            }
            .is_layout_violation()
        );
        assert!(!DataError::UnknownStructLength.is_layout_violation());
        assert!(
            !DataError::CapacityOverflow {
                requested: usize::MAX
            }
            .is_layout_violation()
        );
    }

    #[test]
    fn errors_compare_by_value() {
        assert_eq!(
            DataError::IndexOutOfBounds { index: 1, len: 0 },
            DataError::IndexOutOfBounds { index: 1, len: 0 }
        );
        assert_ne!(
            DataError::IndexOutOfBounds { index: 1, len: 0 },
            DataError::IndexOutOfBounds { index: 2, len: 0 }
        );
    }
}
