//! [`DataType`]/[`Field`]/[`Schema`] <-> arrow-rs [`ArrowDataType`]/
//! [`ArrowField`]/[`ArrowSchema`] mapping.
//!
//! # Which direction can fail
//!
//! `astrs_data::DataType` (blueprint §6.1) is a *closed* subset of arrow's:
//! every P0 variant has exactly one arrow-rs counterpart, so
//! [`to_arrow_data_type`] cannot fail for any value this crate can actually
//! construct today. Its `match` is written exhaustively with no wildcard arm
//! — `#[non_exhaustive]` only restricts matching from *outside* the defining
//! crate, and this module is part of `astrs_data` itself, so a future P2
//! addition (dictionary, union, map — blueprint §6.1) fails this file's own
//! build with a missing-arm error instead of silently falling through
//! somewhere. It still returns [`Result`] rather than `ArrowDataType`
//! directly, so that a genuinely unrepresentable future addition (should one
//! ever arrive) is a signature-compatible error return, not a breaking API
//! change.
//!
//! `arrow_schema::DataType` is the open superset, so [`from_arrow_data_type`]
//! is genuinely fallible: anything outside the P0 set (`Union`, `Dictionary`,
//! `Map`, every `Decimal*`, `Interval`, `Date32`/`Date64`, `Time32`/`Time64`,
//! `BinaryView`/`Utf8View`, `ListView`/`LargeListView`/`LargeList`,
//! `RunEndEncoded`) is [`InteropError::UnmappableArrowType`], and a
//! `Timestamp`/`Duration` whose [`arrow_schema::TimeUnit`] is not
//! `Nanosecond` — or whose `Timestamp` carries a time zone — is
//! [`InteropError::UnsupportedTemporalUnit`] (blueprint §6.1 pins both to
//! nanoseconds, `Timestamp` additionally timezone-less).
//!
//! Per-field metadata is asymmetric by design: `astrs_data::Field` carries
//! none (see `crate::datatype::field`'s module docs — metadata lives on
//! [`Schema`] instead), so [`to_arrow_field`] never sets any and
//! [`from_arrow_field`] silently drops whatever an arrow `Field` carries.
//! Schema-level metadata round-trips in full (`BTreeMap` <-> `HashMap`,
//! re-collected — the key/value pairs are identical, only the map type and
//! its ordering guarantee differ).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arrow_schema::{
    DataType as ArrowDataType, Field as ArrowField, Fields as ArrowFields, Schema as ArrowSchema,
    TimeUnit,
};

use crate::datatype::{DataType, Field, Schema};
use crate::interop::error::{InteropError, Result};

/// Converts the closed P0 [`DataType`] set to its arrow-rs counterpart.
///
/// # Errors
///
/// Never actually returns `Err` for any [`DataType`] value that exists
/// today — every P0 variant maps unconditionally to exactly one arrow-rs
/// counterpart (see the module docs). The `Result` return exists only so a
/// hypothetical future variant beyond today's closed set could report a
/// typed error without an API-breaking signature change.
///
/// ```
/// use arrow_schema::{DataType as ArrowDataType, TimeUnit};
/// use astrs_data::DataType;
/// use astrs_data::interop::to_arrow_data_type;
///
/// assert_eq!(to_arrow_data_type(&DataType::Int32)?, ArrowDataType::Int32);
/// assert_eq!(
///     to_arrow_data_type(&DataType::Timestamp)?,
///     ArrowDataType::Timestamp(TimeUnit::Nanosecond, None)
/// );
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn to_arrow_data_type(data_type: &DataType) -> Result<ArrowDataType> {
    Ok(match data_type {
        DataType::Null => ArrowDataType::Null,
        DataType::Bool => ArrowDataType::Boolean,
        DataType::Int8 => ArrowDataType::Int8,
        DataType::Int16 => ArrowDataType::Int16,
        DataType::Int32 => ArrowDataType::Int32,
        DataType::Int64 => ArrowDataType::Int64,
        DataType::UInt8 => ArrowDataType::UInt8,
        DataType::UInt16 => ArrowDataType::UInt16,
        DataType::UInt32 => ArrowDataType::UInt32,
        DataType::UInt64 => ArrowDataType::UInt64,
        DataType::Float16 => ArrowDataType::Float16,
        DataType::Float32 => ArrowDataType::Float32,
        DataType::Float64 => ArrowDataType::Float64,
        DataType::Binary => ArrowDataType::Binary,
        DataType::LargeBinary => ArrowDataType::LargeBinary,
        DataType::Utf8 => ArrowDataType::Utf8,
        DataType::LargeUtf8 => ArrowDataType::LargeUtf8,
        DataType::FixedSizeBinary(size) => ArrowDataType::FixedSizeBinary(*size),
        DataType::FixedSizeList(field, size) => {
            ArrowDataType::FixedSizeList(Arc::new(to_arrow_field(field)?), *size)
        }
        DataType::List(field) => ArrowDataType::List(Arc::new(to_arrow_field(field)?)),
        DataType::Struct(fields) => ArrowDataType::Struct(to_arrow_fields(fields)?),
        DataType::Timestamp => ArrowDataType::Timestamp(TimeUnit::Nanosecond, None),
        DataType::Duration => ArrowDataType::Duration(TimeUnit::Nanosecond),
    })
}

/// Converts an arrow-rs [`ArrowDataType`] to the closed P0 [`DataType`] set.
///
/// # Errors
///
/// [`InteropError::UnmappableArrowType`] for anything outside the P0 set,
/// [`InteropError::UnsupportedTemporalUnit`] for a `Timestamp`/`Duration`
/// whose unit is not nanoseconds (or whose `Timestamp` carries a time zone)
/// — see the module docs for the exact boundary.
///
/// ```
/// use arrow_schema::{DataType as ArrowDataType, TimeUnit};
/// use astrs_data::DataType;
/// use astrs_data::interop::{from_arrow_data_type, InteropError};
///
/// assert_eq!(from_arrow_data_type(&ArrowDataType::Int32)?, DataType::Int32);
/// assert!(from_arrow_data_type(&ArrowDataType::Date32).is_err());
/// assert!(matches!(
///     from_arrow_data_type(&ArrowDataType::Timestamp(TimeUnit::Second, None)),
///     Err(InteropError::UnsupportedTemporalUnit { kind: "Timestamp", .. })
/// ));
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn from_arrow_data_type(data_type: &ArrowDataType) -> Result<DataType> {
    Ok(match data_type {
        ArrowDataType::Null => DataType::Null,
        ArrowDataType::Boolean => DataType::Bool,
        ArrowDataType::Int8 => DataType::Int8,
        ArrowDataType::Int16 => DataType::Int16,
        ArrowDataType::Int32 => DataType::Int32,
        ArrowDataType::Int64 => DataType::Int64,
        ArrowDataType::UInt8 => DataType::UInt8,
        ArrowDataType::UInt16 => DataType::UInt16,
        ArrowDataType::UInt32 => DataType::UInt32,
        ArrowDataType::UInt64 => DataType::UInt64,
        ArrowDataType::Float16 => DataType::Float16,
        ArrowDataType::Float32 => DataType::Float32,
        ArrowDataType::Float64 => DataType::Float64,
        ArrowDataType::Binary => DataType::Binary,
        ArrowDataType::LargeBinary => DataType::LargeBinary,
        ArrowDataType::Utf8 => DataType::Utf8,
        ArrowDataType::LargeUtf8 => DataType::LargeUtf8,
        ArrowDataType::FixedSizeBinary(size) => DataType::FixedSizeBinary(*size),
        ArrowDataType::FixedSizeList(field, size) => {
            DataType::fixed_size_list(from_arrow_field(field)?, *size)
        }
        ArrowDataType::List(field) => DataType::list(from_arrow_field(field)?),
        ArrowDataType::Struct(fields) => DataType::strukt(from_arrow_fields(fields)?),
        ArrowDataType::Timestamp(TimeUnit::Nanosecond, None) => DataType::Timestamp,
        ArrowDataType::Timestamp(_, _) => {
            return Err(InteropError::UnsupportedTemporalUnit {
                kind: "Timestamp",
                arrow_type: data_type.to_string(),
            });
        }
        ArrowDataType::Duration(TimeUnit::Nanosecond) => DataType::Duration,
        ArrowDataType::Duration(_) => {
            return Err(InteropError::UnsupportedTemporalUnit {
                kind: "Duration",
                arrow_type: data_type.to_string(),
            });
        }
        other => {
            return Err(InteropError::UnmappableArrowType {
                arrow_type: other.to_string(),
            });
        }
    })
}

/// Converts a [`Field`] to its arrow-rs counterpart. Never carries metadata —
/// see the module docs.
///
/// # Errors
///
/// Whatever [`to_arrow_data_type`] reports for the field's type.
pub fn to_arrow_field(field: &Field) -> Result<ArrowField> {
    Ok(ArrowField::new(
        field.name(),
        to_arrow_data_type(field.data_type())?,
        field.is_nullable(),
    ))
}

/// Converts an arrow-rs field to a [`Field`]. Drops any per-field metadata
/// the arrow field carries — see the module docs.
///
/// # Errors
///
/// Whatever [`from_arrow_data_type`] reports for the field's type.
pub fn from_arrow_field(field: &ArrowField) -> Result<Field> {
    Ok(Field::new(
        field.name().clone(),
        from_arrow_data_type(field.data_type())?,
        field.is_nullable(),
    ))
}

/// Converts a field slice (a [`crate::datatype::Schema`]'s columns, or a
/// [`DataType::Struct`]'s children) to an arrow-rs [`ArrowFields`].
///
/// # Errors
///
/// Whatever [`to_arrow_field`] reports for the first offending field.
pub fn to_arrow_fields(fields: &[Field]) -> Result<ArrowFields> {
    fields.iter().map(to_arrow_field).collect()
}

/// Converts an arrow-rs [`ArrowFields`] to a `Vec<Field>`.
///
/// # Errors
///
/// Whatever [`from_arrow_field`] reports for the first offending field.
pub fn from_arrow_fields(fields: &ArrowFields) -> Result<Vec<Field>> {
    fields.iter().map(|field| from_arrow_field(field)).collect()
}

/// Converts a [`Schema`] to its arrow-rs counterpart, metadata included.
///
/// # Errors
///
/// Whatever [`to_arrow_fields`] reports for the first offending field.
///
/// ```
/// use astrs_data::{DataType, Field, Schema};
/// use astrs_data::interop::to_arrow_schema;
///
/// let schema = Schema::new(vec![Field::new("a", DataType::Int32, true)])
///     .with_metadata_entry("k", "v");
/// let arrow_schema = to_arrow_schema(&schema)?;
/// assert_eq!(arrow_schema.fields().len(), 1);
/// assert_eq!(arrow_schema.metadata().get("k"), Some(&"v".to_owned()));
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn to_arrow_schema(schema: &Schema) -> Result<ArrowSchema> {
    let fields = to_arrow_fields(schema.fields())?;
    let metadata: HashMap<String, String> = schema
        .metadata()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Ok(ArrowSchema::new_with_metadata(fields, metadata))
}

/// Converts an arrow-rs schema to a [`Schema`], metadata included.
///
/// # Errors
///
/// Whatever [`from_arrow_fields`] reports for the first offending field.
///
/// ```
/// use arrow_schema::{DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema};
/// use astrs_data::interop::from_arrow_schema;
///
/// let arrow_schema = ArrowSchema::new(vec![ArrowField::new("a", ArrowDataType::Int32, true)]);
/// let schema = from_arrow_schema(&arrow_schema)?;
/// assert_eq!(schema.len(), 1);
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn from_arrow_schema(schema: &ArrowSchema) -> Result<Schema> {
    let fields = from_arrow_fields(schema.fields())?;
    let metadata: BTreeMap<String, String> = schema
        .metadata()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Ok(Schema::new(fields).with_metadata(metadata))
}

// -- `TryFrom` shapes over the six functions above ---------------------------
//
// Blueprint §6.1 phrases this feature as adding "`From`/`TryFrom`" between
// astrs-data and arrow-rs types; the free functions above are the primary,
// always-available surface (they are what the array- and RecordBatch-level
// conversions build on, and the only spelling possible at all for the
// dyn-erased array types — see `crate::interop::array`'s own docs), but
// `DataType`/`Field`/`Schema` are concrete, non-generic types on both sides,
// so the trait spelling is also just a three-line delegation with no
// coherence obstacle. Both exist rather than only one: callers who want
// `.try_into()` at a type-directed call site get it, without this module
// carrying two independent implementations of anything.

impl TryFrom<&DataType> for ArrowDataType {
    type Error = InteropError;

    fn try_from(data_type: &DataType) -> Result<Self> {
        to_arrow_data_type(data_type)
    }
}

impl TryFrom<&ArrowDataType> for DataType {
    type Error = InteropError;

    fn try_from(data_type: &ArrowDataType) -> Result<Self> {
        from_arrow_data_type(data_type)
    }
}

impl TryFrom<&Field> for ArrowField {
    type Error = InteropError;

    fn try_from(field: &Field) -> Result<Self> {
        to_arrow_field(field)
    }
}

impl TryFrom<&ArrowField> for Field {
    type Error = InteropError;

    fn try_from(field: &ArrowField) -> Result<Self> {
        from_arrow_field(field)
    }
}

impl TryFrom<&Schema> for ArrowSchema {
    type Error = InteropError;

    fn try_from(schema: &Schema) -> Result<Self> {
        to_arrow_schema(schema)
    }
}

impl TryFrom<&ArrowSchema> for Schema {
    type Error = InteropError;

    fn try_from(schema: &ArrowSchema) -> Result<Self> {
        from_arrow_schema(schema)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn every_astrs_type() -> Vec<DataType> {
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
            DataType::strukt([
                Field::new("a", DataType::Int32, true),
                Field::new("b", DataType::Bool, false),
            ]),
            DataType::Timestamp,
            DataType::Duration,
        ]
    }

    #[test]
    fn every_p0_type_round_trips() {
        for data_type in every_astrs_type() {
            let arrow_type = to_arrow_data_type(&data_type)
                .unwrap_or_else(|e| panic!("{data_type}: to_arrow failed: {e}"));
            let back = from_arrow_data_type(&arrow_type)
                .unwrap_or_else(|e| panic!("{data_type}: from_arrow failed: {e}"));
            assert_eq!(back, data_type, "arrow_type={arrow_type}");
        }
    }

    #[test]
    fn bool_maps_to_boolean() {
        assert_eq!(
            to_arrow_data_type(&DataType::Bool).unwrap(),
            ArrowDataType::Boolean
        );
        assert_eq!(
            from_arrow_data_type(&ArrowDataType::Boolean).unwrap(),
            DataType::Bool
        );
    }

    #[test]
    fn temporal_types_pin_to_nanoseconds_and_no_timezone() {
        assert_eq!(
            to_arrow_data_type(&DataType::Timestamp).unwrap(),
            ArrowDataType::Timestamp(TimeUnit::Nanosecond, None)
        );
        assert_eq!(
            to_arrow_data_type(&DataType::Duration).unwrap(),
            ArrowDataType::Duration(TimeUnit::Nanosecond)
        );

        for rejected in [
            ArrowDataType::Timestamp(TimeUnit::Second, None),
            ArrowDataType::Timestamp(TimeUnit::Millisecond, None),
            ArrowDataType::Timestamp(TimeUnit::Microsecond, None),
            ArrowDataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
        ] {
            assert!(
                matches!(
                    from_arrow_data_type(&rejected),
                    Err(InteropError::UnsupportedTemporalUnit {
                        kind: "Timestamp",
                        ..
                    })
                ),
                "{rejected} should be rejected"
            );
        }
        assert!(matches!(
            from_arrow_data_type(&ArrowDataType::Duration(TimeUnit::Microsecond)),
            Err(InteropError::UnsupportedTemporalUnit {
                kind: "Duration",
                ..
            })
        ));
    }

    #[test]
    fn types_outside_the_p0_set_are_rejected() {
        for rejected in [
            ArrowDataType::Date32,
            ArrowDataType::Date64,
            ArrowDataType::Time32(TimeUnit::Second),
            ArrowDataType::Time64(TimeUnit::Nanosecond),
            ArrowDataType::Decimal128(10, 2),
            ArrowDataType::BinaryView,
            ArrowDataType::Utf8View,
            ArrowDataType::LargeList(Arc::new(ArrowField::new("item", ArrowDataType::Int8, true))),
            ArrowDataType::Dictionary(
                Box::new(ArrowDataType::Int32),
                Box::new(ArrowDataType::Utf8),
            ),
        ] {
            assert!(
                matches!(
                    from_arrow_data_type(&rejected),
                    Err(InteropError::UnmappableArrowType { .. })
                ),
                "{rejected} should be rejected"
            );
        }
    }

    #[test]
    fn fields_preserve_name_type_and_nullability_but_not_metadata() {
        let field = Field::new("range", DataType::Float32, true);
        let arrow_field = to_arrow_field(&field).unwrap();
        assert_eq!(arrow_field.name(), "range");
        assert_eq!(arrow_field.data_type(), &ArrowDataType::Float32);
        assert!(arrow_field.is_nullable());

        let mut with_metadata = arrow_field.clone();
        with_metadata.set_metadata(HashMap::from([("k".to_owned(), "v".to_owned())]));
        let back = from_arrow_field(&with_metadata).unwrap();
        assert_eq!(back, field, "arrow-side metadata is dropped, not merged in");
    }

    #[test]
    fn nested_struct_and_list_fields_recurse() {
        let data_type = DataType::list(Field::new(
            "row",
            DataType::strukt([Field::new("x", DataType::Int32, false)]),
            true,
        ));
        let arrow_type = to_arrow_data_type(&data_type).unwrap();
        let back = from_arrow_data_type(&arrow_type).unwrap();
        assert_eq!(back, data_type);
    }

    #[test]
    fn schema_round_trips_with_metadata() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Utf8, false),
        ])
        .with_metadata_entry("_schema_hash", "deadbeef")
        .with_metadata_entry("producer", "camera");

        let arrow_schema = to_arrow_schema(&schema).unwrap();
        assert_eq!(arrow_schema.fields().len(), 2);
        assert_eq!(
            arrow_schema.metadata().get("producer").map(String::as_str),
            Some("camera")
        );

        let back = from_arrow_schema(&arrow_schema).unwrap();
        assert_eq!(back, schema);
    }

    #[test]
    fn empty_schema_round_trips() {
        let schema = Schema::default();
        let arrow_schema = to_arrow_schema(&schema).unwrap();
        assert_eq!(arrow_schema.fields().len(), 0);
        assert_eq!(from_arrow_schema(&arrow_schema).unwrap(), schema);
    }

    #[test]
    fn the_try_from_impls_agree_with_the_functions_they_delegate_to() {
        let data_type = DataType::list(Field::new("item", DataType::Float32, true));
        assert_eq!(
            ArrowDataType::try_from(&data_type).unwrap(),
            to_arrow_data_type(&data_type).unwrap()
        );

        let field = Field::new("range_m", DataType::Float32, true);
        assert_eq!(
            ArrowField::try_from(&field).unwrap(),
            to_arrow_field(&field).unwrap()
        );

        let schema =
            Schema::new(vec![Field::new("a", DataType::Int32, true)]).with_metadata_entry("k", "v");
        let via_trait = ArrowSchema::try_from(&schema).unwrap();
        assert_eq!(via_trait, to_arrow_schema(&schema).unwrap());
        assert_eq!(Schema::try_from(&via_trait).unwrap(), schema);
    }
}
