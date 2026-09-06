//! [`AstrsMessage`] — the trait a typed message implements to cross into
//! columnar space (blueprint §9.1, §9.2).
//!
//! ```text
//! #[derive(AstrsMessage)]                       // astrs-operator-macros
//! #[astrs(urn = "std/vision/v1/Detections")]
//! struct Detections { boxes: Vec<[f32; 4]>, scores: Vec<f32>, labels: Vec<u32> }
//! ```
//!
//! The derive above (blueprint §9.1's canonical example) maps each field onto
//! the closed columnar type set (§6.1) and implements this trait: struct
//! fields become the columns of one [`crate::datatype::DataType::Struct`],
//! wrapped in a one-row [`RecordBatch`] per the "one message per batch"
//! convention.
//!
//! # Ownership
//!
//! This trait is declared here, in `astrs-data`, rather than in the crate
//! that actually derives it (`astrs-operator-macros`) or the crate blueprint
//! §9.1 shows using it (`astrs-node-api`): every one of those sits in a
//! *higher* layer than `astrs-data` (blueprint §4.1's strictly-downward
//! dependency rule), and `astrs-idl`'s planned ROS 2 codegen (§10.3) — a
//! *sibling* Layer 2 crate to the node/operator APIs, generating types that
//! implement both `CdrSerde` and `AstrsMessage` — needs to implement it too,
//! without depending upward on either. `astrs-data` is the one crate every
//! future implementor already depends on for its columnar types, so it is
//! the only layer-legal home for the trait itself. `astrs-node-api` and the
//! top-level `astrs` facade should re-export it (alongside the
//! `#[derive(AstrsMessage)]` macro from `astrs-operator-macros`) from their
//! own preludes once they land.
//!
//! # What the trait does not do
//!
//! It does not validate `URN` against the [`crate::urn`] registry itself —
//! that is a runtime lookup ([`crate::urn::layout_of_str`]), and a message
//! type's `data_type()` is a compile-time-fixed Rust value. A caller that
//! wants the cross-check runs it explicitly:
//!
//! ```
//! use astrs_data::urn::layout_of_str;
//! use astrs_data::{AstrsMessage, DataType, Field, RecordBatch};
//!
//! struct Ping { value: f64 }
//!
//! impl AstrsMessage for Ping {
//!     const URN: &'static str = "std/core/v1/Float64";
//!
//!     fn data_type() -> DataType {
//!         DataType::Float64
//!     }
//!
//!     fn to_record_batch(&self) -> astrs_data::Result<RecordBatch> {
//!         use astrs_data::array::{Float64Array, IntoArrayRef};
//!         Ok(RecordBatch::from_payload(
//!             Float64Array::from_values([self.value]).into_array_ref(),
//!         ))
//!     }
//!
//!     fn from_record_batch(batch: &RecordBatch) -> astrs_data::Result<Self> {
//!         use astrs_data::array::{ArrayExt, Float64Array};
//!         use astrs_data::DataError;
//!
//!         if batch.num_rows() != 1 {
//!             return Err(DataError::MessageRowCount { actual: batch.num_rows() });
//!         }
//!         let column = batch
//!             .payload_column()
//!             .ok_or(DataError::ColumnCountMismatch { fields: 1, columns: 0 })?;
//!         let value = column
//!             .try_downcast::<Float64Array>()?
//!             .get(0)
//!             .ok_or_else(|| DataError::RequiredFieldIsNull { field: "value".to_owned(), row: 0 })?;
//!         Ok(Self { value })
//!     }
//! }
//!
//! let ping = Ping { value: 2.5 };
//! let batch = ping.to_record_batch()?;
//! assert_eq!(Ping::from_record_batch(&batch)?.value, 2.5);
//! assert_eq!(layout_of_str(Ping::URN), Ok(Ping::data_type()));
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use crate::datatype::{DataType, Schema};
use crate::error::Result;
use crate::record_batch::RecordBatch;
use crate::urn::{TypeUrn, TypeUrnError};

/// A Rust type that maps onto a single-column AstRS payload (blueprint
/// §6.1, §9.2).
///
/// See the [module documentation](self) for why this trait lives in
/// `astrs-data` and what it deliberately leaves to callers.
pub trait AstrsMessage: Sized {
    /// The type URN this message's columnar layout is declared against
    /// (blueprint §24.3), e.g. `"std/vision/v1/Detections"`.
    const URN: &'static str;

    /// The columnar layout [`AstrsMessage::to_record_batch`] produces and
    /// [`AstrsMessage::from_record_batch`] expects.
    fn data_type() -> DataType;

    /// The single-column [`Schema`] wrapping [`AstrsMessage::data_type`]
    /// under [`crate::DATA_COLUMN`] (blueprint §6.1).
    #[must_use]
    fn schema() -> Schema {
        Schema::payload(Self::data_type(), false)
    }

    /// Parses [`AstrsMessage::URN`].
    ///
    /// # Errors
    ///
    /// Whatever [`TypeUrn::parse`] reports, wrapped through
    /// [`crate::DataError::TypeUrn`]. A malformed constant is a programmer
    /// error rather than a data error, but the parse is still fallible at
    /// the type level — a `#[derive(AstrsMessage)]` impl's constant has
    /// already passed the macro's own syntax check (see
    /// `astrs-operator-macros`), so this should only ever fail for a
    /// hand-written implementation.
    fn type_urn() -> Result<TypeUrn, TypeUrnError> {
        TypeUrn::parse(Self::URN)
    }

    /// Encodes one message into a one-row [`RecordBatch`] (blueprint §6.1's
    /// "one record batch per message").
    ///
    /// # Errors
    ///
    /// A [`crate::DataError`] from whichever columnar constructor rejects
    /// the value. Practically unreachable for a `#[derive(AstrsMessage)]`
    /// impl, since the derive only emits calls that cannot disagree with
    /// its own [`AstrsMessage::data_type`], but the trait stays fallible for
    /// hand-written implementors.
    fn to_record_batch(&self) -> Result<RecordBatch>;

    /// Decodes one message from a one-row [`RecordBatch`].
    ///
    /// # Errors
    ///
    /// [`crate::DataError::MessageRowCount`] when `batch` does not hold
    /// exactly one row, or another [`crate::DataError`] when a column's
    /// shape or nullability disagrees with [`AstrsMessage::data_type`].
    fn from_record_batch(batch: &RecordBatch) -> Result<Self>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{ArrayExt, Float64Array, IntoArrayRef, StructArray};
    use crate::datatype::Field;
    use crate::error::DataError;

    /// A minimal hand-written implementor, exercising the trait's default
    /// methods and error paths without needing the derive macro (which
    /// lives in a higher-layer crate — see the module docs).
    #[derive(Debug, PartialEq)]
    struct Ping {
        value: f64,
    }

    impl AstrsMessage for Ping {
        const URN: &'static str = "std/core/v1/PingMsg";

        fn data_type() -> DataType {
            DataType::strukt([Field::required("value", DataType::Float64)])
        }

        fn to_record_batch(&self) -> Result<RecordBatch> {
            let DataType::Struct(fields) = Self::data_type() else {
                unreachable!("Ping::data_type is always a Struct")
            };
            let column = Float64Array::from_values([self.value]).into_array_ref();
            let strukt = StructArray::try_new(fields, vec![column], None)?;
            Ok(RecordBatch::from_payload(strukt.into_array_ref()))
        }

        fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
            if batch.num_rows() != 1 {
                return Err(DataError::MessageRowCount {
                    actual: batch.num_rows(),
                });
            }
            let column = batch
                .payload_column()
                .ok_or(DataError::ColumnCountMismatch {
                    fields: 1,
                    columns: 0,
                })?;
            let strukt = column.try_downcast::<StructArray>()?;
            let value_column = strukt
                .columns()
                .first()
                .ok_or(DataError::ColumnCountMismatch {
                    fields: 1,
                    columns: 0,
                })?;
            let value = value_column
                .try_downcast::<Float64Array>()?
                .get(0)
                .ok_or_else(|| DataError::RequiredFieldIsNull {
                    field: "value".to_owned(),
                    row: 0,
                })?;
            Ok(Self { value })
        }
    }

    #[test]
    fn round_trips_through_a_record_batch() {
        let ping = Ping { value: 2.5 };
        let batch = ping.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(Ping::from_record_batch(&batch).unwrap(), ping);
    }

    #[test]
    fn schema_wraps_data_type_in_the_payload_column() {
        let schema = Ping::schema();
        assert_eq!(schema.len(), 1);
        assert_eq!(
            schema.field(0).map(Field::data_type),
            Some(&Ping::data_type())
        );
        assert_eq!(schema.field(0).map(Field::name), Some(crate::DATA_COLUMN));
    }

    #[test]
    fn type_urn_parses_the_constant() {
        let urn = Ping::type_urn().unwrap();
        assert_eq!(urn.as_str(), Ping::URN);
    }

    #[test]
    fn decode_rejects_a_batch_that_is_not_exactly_one_row() {
        let DataType::Struct(fields) = Ping::data_type() else {
            unreachable!()
        };
        let column = Float64Array::from_values([1.0, 2.0]).into_array_ref();
        let strukt = StructArray::try_new(fields, vec![column], None).unwrap();
        let batch = RecordBatch::from_payload(strukt.into_array_ref());
        assert_eq!(
            Ping::from_record_batch(&batch).unwrap_err(),
            DataError::MessageRowCount { actual: 2 }
        );

        let empty = StructArray::try_new_with_len(
            Ping::data_type().children().into_iter().cloned().collect(),
            vec![Float64Array::new_null(0).into_array_ref()],
            0,
            None,
        )
        .unwrap();
        let empty_batch = RecordBatch::from_payload(empty.into_array_ref());
        assert_eq!(
            Ping::from_record_batch(&empty_batch).unwrap_err(),
            DataError::MessageRowCount { actual: 0 }
        );
    }

    #[test]
    fn decode_rejects_a_null_required_field() {
        let DataType::Struct(fields) = Ping::data_type() else {
            unreachable!()
        };
        let column = Float64Array::new_null(1).into_array_ref();
        let strukt = StructArray::try_new(fields, vec![column], None).unwrap();
        let batch = RecordBatch::from_payload(strukt.into_array_ref());
        assert_eq!(
            Ping::from_record_batch(&batch).unwrap_err(),
            DataError::RequiredFieldIsNull {
                field: "value".to_owned(),
                row: 0,
            }
        );
    }

    #[test]
    fn decode_rejects_the_wrong_shape() {
        // A payload column that isn't a `Struct` at all.
        let column = Float64Array::from_values([1.0]).into_array_ref();
        let batch = RecordBatch::from_payload(column);
        assert!(Ping::from_record_batch(&batch).is_err());
    }

    #[test]
    fn message_error_variants_report_useful_context() {
        let row_count = DataError::MessageRowCount { actual: 3 };
        assert_eq!(
            row_count.to_string(),
            "expected exactly one message row but the batch has 3"
        );

        let fixed = DataError::MessageFixedArrayLength {
            field: "quat".to_owned(),
            expected: 4,
            actual: 3,
        };
        assert_eq!(
            fixed.to_string(),
            "fixed-size field \"quat\" decoded 3 element(s), expected 4"
        );
    }
}
