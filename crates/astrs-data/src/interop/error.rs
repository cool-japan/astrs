//! [`InteropError`] — every recoverable failure the `arrow-interop`
//! conversions can report.
//!
//! A separate enum rather than new [`crate::DataError`] variants: `DataError`
//! is a non-`arrow-interop` public surface (every default build sees it), and
//! this crate's error philosophy is structured fields over strings (see
//! `crate::error`'s module docs) — which [`arrow_schema::ArrowError`] cannot
//! give us, since it is not `Clone`/`PartialEq` (it can carry a boxed `dyn
//! Error`). Keeping the two enums apart means [`DataError`] keeps its
//! `Clone + PartialEq + Eq` promise unconditionally, and [`InteropError`]
//! keeps the same promise for every non-default build too — the one variant
//! that wraps an arrow-rs error renders it once, at the boundary, into an
//! owned `String`.

use arrow_schema::{ArrowError, DataType as ArrowDataType};

use crate::DataError;

/// Result alias used throughout `interop`.
pub type Result<T, E = InteropError> = core::result::Result<T, E>;

/// Every recoverable failure a `crate::interop` conversion can report.
///
/// `#[non_exhaustive]`: this module's own append-only evolution rule applies
/// (P2 Arrow types — dictionary, union, map, decimal, view — arrive as new
/// variants here, not as breaking changes to existing ones), and every
/// `match` over it must therefore keep a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InteropError {
    /// An `arrow_schema::DataType` variant outside the closed P0 set
    /// (blueprint §6.1) — dictionary, union, map, decimal, interval, date,
    /// time, view or run-end-encoded — has no `astrs_data::DataType`
    /// counterpart.
    #[error(
        "arrow DataType `{arrow_type}` has no astrs-data counterpart (outside the closed P0 set, blueprint §6.1)"
    )]
    UnmappableArrowType {
        /// The rejected type, rendered via its own `Display`.
        arrow_type: String,
    },

    /// An `arrow_schema::DataType::Timestamp`/`Duration` was not the one
    /// parameterisation `astrs_data::DataType::Timestamp`/`Duration` accepts:
    /// nanoseconds, and (`Timestamp` only) no time zone.
    #[error(
        "arrow {kind} is not the nanosecond, timezone-less parameterisation astrs-data requires: {arrow_type}"
    )]
    UnsupportedTemporalUnit {
        /// `"Timestamp"` or `"Duration"`.
        kind: &'static str,
        /// The rejected type, rendered via its own `Display`.
        arrow_type: String,
    },

    /// An `arrow_array::Array`'s runtime [`arrow_schema::DataType`] does not
    /// match the [`arrow_schema::DataType`] its caller expected at this
    /// position (for example, a `Field::data_type()` disagreeing with its
    /// column's actual array).
    #[error("expected an arrow array of type {expected}, found {actual}")]
    ArrowTypeMismatch {
        /// The type the caller expected.
        expected: ArrowDataType,
        /// The type the array actually reports.
        actual: ArrowDataType,
    },

    /// A downcast from `&dyn arrow_array::Array` to the concrete arrow-rs
    /// array type its [`arrow_schema::DataType`] promised failed. Arrow-rs's
    /// own invariant (`data_type()` names the concrete type `as_any()`
    /// downcasts to) makes this unreachable in practice; reported rather than
    /// asserted so a future arrow-rs release that breaks the invariant fails
    /// loudly here instead of panicking.
    #[error("arrow array reports type {data_type} but does not downcast to its concrete type")]
    ArrowDowncastFailed {
        /// The type the array reported.
        data_type: ArrowDataType,
    },

    /// [`crate::record_batch::RecordBatch::try_new`] (or one of the array
    /// constructors it calls) rejected data built by an `arrow-interop`
    /// conversion — always a logical-content problem in the source arrow
    /// array (for example, a null in a column the arrow `Field` declares
    /// non-nullable), never a bug in the buffer-level conversion itself.
    #[error(transparent)]
    Data(#[from] DataError),

    /// arrow-rs itself rejected a conversion (an [`ArrowDataType`]
    /// construction, an [`arrow_data::ArrayDataBuilder::build`] validation,
    /// or an `arrow_array::RecordBatch::try_new`). Rendered once, at the
    /// boundary, into an owned message — see the module docs for why the
    /// original [`ArrowError`] is not carried through.
    #[error("arrow-rs rejected the conversion: {0}")]
    Arrow(String),

    /// A [`crate::datatype::Schema`]/`arrow_schema::Schema` pair disagree on
    /// column count during a [`crate::record_batch::RecordBatch`] conversion.
    #[error("schema declares {expected} field(s) but the record batch has {actual} column(s)")]
    ColumnCountMismatch {
        /// Fields the schema declares.
        expected: usize,
        /// Columns the batch actually carries.
        actual: usize,
    },
}

impl From<ArrowError> for InteropError {
    /// Renders the arrow-rs error once, at the boundary — see the module and
    /// [`InteropError::Arrow`] docs for why the original is not kept.
    fn from(error: ArrowError) -> Self {
        Self::Arrow(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn arrow_errors_render_into_an_owned_message() {
        let arrow_err = ArrowError::InvalidArgumentError("bad offsets".to_owned());
        let rendered = arrow_err.to_string();
        let err: InteropError = arrow_err.into();
        assert_eq!(err, InteropError::Arrow(rendered));
    }

    #[test]
    fn data_errors_flatten_transparently() {
        let inner = DataError::UnknownStructLength;
        let err: InteropError = inner.clone().into();
        assert_eq!(err.to_string(), inner.to_string());
        assert_eq!(err, InteropError::Data(inner));
    }

    #[test]
    fn errors_compare_by_value() {
        let a = InteropError::UnmappableArrowType {
            arrow_type: "Union".to_owned(),
        };
        let b = InteropError::UnmappableArrowType {
            arrow_type: "Union".to_owned(),
        };
        assert_eq!(a, b);
        assert_ne!(
            a,
            InteropError::UnmappableArrowType {
                arrow_type: "Map".to_owned()
            }
        );
    }

    #[test]
    fn arrow_type_mismatch_names_both_sides() {
        let err = InteropError::ArrowTypeMismatch {
            expected: ArrowDataType::Int32,
            actual: ArrowDataType::Utf8,
        };
        assert_eq!(
            err.to_string(),
            "expected an arrow array of type Int32, found Utf8"
        );
    }
}
