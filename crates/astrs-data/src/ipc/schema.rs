//! The Arrow `Schema` and `Field` flatbuffer tables, encoded and decoded
//! against the closed AstRS type set.
//!
//! This module is the type-system border of the IPC layer. Encoding is total —
//! every [`DataType`] AstRS models has exactly one Arrow spelling — while
//! decoding is deliberately narrow: anything outside blueprint §6.1's closed
//! set is rejected with a typed error that names the offending field and the
//! exact Arrow construct, so a mismatched producer is diagnosed in one line
//! instead of a hex dump.
//!
//! | Arrow `Type` | AstRS [`DataType`] | Rejected when |
//! |---|---|---|
//! | `Null` | [`DataType::Null`] | — |
//! | `Bool` | [`DataType::Bool`] | — |
//! | `Int` | `Int8..64` / `UInt8..64` | bit width not 8/16/32/64 |
//! | `FloatingPoint` | `Float16/32/64` | — |
//! | `Binary`/`LargeBinary` | `Binary`/`LargeBinary` | — |
//! | `Utf8`/`LargeUtf8` | `Utf8`/`LargeUtf8` | — |
//! | `FixedSizeBinary` | `FixedSizeBinary(w)` | `w <= 0` |
//! | `FixedSizeList` | `FixedSizeList(f, n)` | `n <= 0`, child count ≠ 1 |
//! | `List` | `List(f)` | child count ≠ 1 |
//! | `Struct_` | `Struct(fields)` | — |
//! | `Timestamp` | `Timestamp` | unit ≠ ns, or a time zone is set |
//! | `Duration` | `Duration` | unit ≠ ns |
//! | anything else | — | always ([`IpcError::UnsupportedType`]) |
//!
//! # What is dropped
//!
//! Arrow lets every `Field` carry its own `custom_metadata`. [`Field`] has no
//! metadata map — only [`Schema`] does — so field-level metadata is **read
//! and discarded**. Schema-level metadata round-trips exactly, sorted by key
//! (AstRS keeps it in a `BTreeMap`, so the encoded order is deterministic
//! where arrow-rs's `HashMap` order is not).
//!
//! Dictionary-encoded fields are rejected outright ([`IpcError::UnsupportedDictionary`]):
//! dictionaries are a P2 feature, and silently dropping the encoding would
//! turn indices into values.
//!
//! ```
//! use astrs_data::ipc::schema::{decode_schema_bytes, encode_schema_bytes};
//! use astrs_data::{DataType, Field, Schema};
//!
//! let schema = Schema::new(vec![
//!     Field::new("stamp", DataType::Timestamp, false),
//!     Field::new("data", DataType::Float32, true),
//! ])
//! .with_metadata_entry("std/type", "std/core/v1/Float32");
//!
//! let bytes = encode_schema_bytes(&schema)?;
//! let decoded = decode_schema_bytes(&bytes)?;
//! assert_eq!(decoded, schema);
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

use std::collections::BTreeMap;

use crate::datatype::{DataType, Field, Schema};
use crate::ipc::error::{IpcError, Result};
use crate::ipc::fb::{FbBuilder, SIZE_UOFFSET, Table, WipOffset, root_table};
use crate::ipc::format::{
    endianness, field, key_value, precision, schema as schema_slot, time_unit, type_code,
    type_table,
};
use crate::ipc::layout::MAX_NESTING_DEPTH;

/// Encodes a schema as a standalone flatbuffer.
///
/// The result is the `Schema` table on its own, **not** wrapped in a
/// `Message`; the stream writer embeds it with
/// [`crate::ipc::message::write_schema_message`].
///
/// # Errors
///
/// [`IpcError::NestingTooDeep`] when a field nests deeper than
/// [`MAX_NESTING_DEPTH`].
pub fn encode_schema_bytes(schema: &Schema) -> Result<Vec<u8>> {
    let mut builder = FbBuilder::new();
    let root = encode_schema(&mut builder, schema)?;
    builder.finish(root);
    Ok(builder.finished_bytes().to_vec())
}

/// Decodes a standalone `Schema` flatbuffer, as produced by
/// [`encode_schema_bytes`].
///
/// # Errors
///
/// Every decode-side variant of [`IpcError`]: malformed flatbuffers,
/// unsupported types, big-endian streams.
pub fn decode_schema_bytes(bytes: &[u8]) -> Result<Schema> {
    let table = root_table(bytes)?;
    decode_schema(&table)
}

/// Writes the `Schema` table into `builder`, returning its offset.
///
/// Every child object (strings, child fields, metadata entries) is created
/// before the table that refers to it, as the builder requires.
///
/// # Errors
///
/// [`IpcError::NestingTooDeep`] when a field nests deeper than
/// [`MAX_NESTING_DEPTH`].
pub fn encode_schema(builder: &mut FbBuilder, schema: &Schema) -> Result<WipOffset> {
    let mut field_offsets = Vec::with_capacity(schema.len());
    for item in schema.fields() {
        field_offsets.push(encode_field(builder, item, 1)?);
    }
    let fields = builder.create_offset_vector(&field_offsets);
    let metadata = encode_metadata(builder, schema.metadata());

    let table = builder.start_table();
    // `endianness` is elided: little-endian is the flatbuffer default and the
    // only value AstRS writes.
    builder.push_slot_offset(schema_slot::FIELDS, fields);
    if let Some(metadata) = metadata {
        builder.push_slot_offset(schema_slot::CUSTOM_METADATA, metadata);
    }
    Ok(builder.end_table(table))
}

/// Reads a `Schema` table.
///
/// Duplicate field names are **accepted**: Arrow permits them, and a foreign
/// producer that emits two `data` columns should still decode rather than
/// fail at the border. [`Schema::field_by_name`] then resolves to the first,
/// exactly as arrow-rs does.
///
/// # Errors
///
/// * [`IpcError::UnsupportedEndianness`] for a big-endian stream.
/// * [`IpcError::MissingField`] when `fields` is absent.
/// * Whatever [`decode_field`] rejects.
pub fn decode_schema(table: &Table<'_>) -> Result<Schema> {
    let endian = table.i16(schema_slot::ENDIANNESS, endianness::LITTLE)?;
    if endian != endianness::LITTLE {
        return Err(IpcError::UnsupportedEndianness { endianness: endian });
    }
    let fields = table
        .vector(schema_slot::FIELDS, SIZE_UOFFSET)?
        .ok_or_else(|| IpcError::missing("Schema", "fields"))?;
    let mut decoded = Vec::with_capacity(fields.len());
    for index in 0..fields.len() {
        let child = fields.table(index)?;
        decoded.push(decode_field(&child, 1)?);
    }
    let metadata = decode_metadata(table, schema_slot::CUSTOM_METADATA)?;
    let schema = Schema::new(decoded);
    Ok(if metadata.is_empty() {
        schema
    } else {
        schema.with_metadata(metadata)
    })
}

/// Writes one `Field` table (and, first, its children and type table).
///
/// # Errors
///
/// [`IpcError::NestingTooDeep`] when `depth` exceeds [`MAX_NESTING_DEPTH`].
pub fn encode_field(builder: &mut FbBuilder, item: &Field, depth: usize) -> Result<WipOffset> {
    if depth > MAX_NESTING_DEPTH {
        return Err(IpcError::NestingTooDeep {
            depth,
            limit: MAX_NESTING_DEPTH,
        });
    }
    let name = builder.create_string(item.name());

    let mut child_offsets = Vec::new();
    for child in item.data_type().children() {
        child_offsets.push(encode_field(builder, child, depth + 1)?);
    }
    let children = builder.create_offset_vector(&child_offsets);
    let (code, type_offset) = encode_type(builder, item.data_type());

    let table = builder.start_table();
    builder.push_slot_offset(field::NAME, name);
    builder.push_slot_bool(field::NULLABLE, item.is_nullable(), false);
    builder.push_slot_u8(field::TYPE_TYPE, code, type_code::NONE);
    builder.push_slot_offset(field::TYPE, type_offset);
    builder.push_slot_offset(field::CHILDREN, children);
    Ok(builder.end_table(table))
}

/// Reads one `Field` table.
///
/// # Errors
///
/// * [`IpcError::NestingTooDeep`] past [`MAX_NESTING_DEPTH`].
/// * [`IpcError::UnsupportedDictionary`] for a dictionary-encoded field.
/// * [`IpcError::UnsupportedType`] / [`IpcError::UnsupportedTypeParameters`] /
///   [`IpcError::ChildCountMismatch`] from [`decode_type`].
pub fn decode_field(table: &Table<'_>, depth: usize) -> Result<Field> {
    if depth > MAX_NESTING_DEPTH {
        return Err(IpcError::NestingTooDeep {
            depth,
            limit: MAX_NESTING_DEPTH,
        });
    }
    let name = table.string(field::NAME)?.unwrap_or_default().to_owned();
    let nullable = table.bool(field::NULLABLE, false)?;
    if table.field(field::DICTIONARY)?.is_some() {
        return Err(IpcError::UnsupportedDictionary);
    }
    let code = table.u8(field::TYPE_TYPE, type_code::NONE)?;
    let type_table = table
        .table(field::TYPE)?
        .ok_or_else(|| IpcError::missing("Field", "type"))?;

    let mut children = Vec::new();
    if let Some(vector) = table.vector(field::CHILDREN, SIZE_UOFFSET)? {
        for index in 0..vector.len() {
            let child = vector.table(index)?;
            children.push(decode_field(&child, depth + 1)?);
        }
    }
    // Field-level `custom_metadata` has nowhere to go in `Field`; see the
    // module documentation.
    let data_type = decode_type(code, &type_table, &name, children)?;
    Ok(Field::new(name, data_type, nullable))
}

/// Encodes the `Type` union member for `data_type`, returning its
/// discriminant and offset.
///
/// The child *fields* are not written here — they live in `Field.children`,
/// which [`encode_field`] fills in.
fn encode_type(builder: &mut FbBuilder, data_type: &DataType) -> (u8, WipOffset) {
    let (code, table) = match data_type {
        DataType::Null => (type_code::NULL, empty_table(builder)),
        DataType::Bool => (type_code::BOOL, empty_table(builder)),
        DataType::Int8 => (type_code::INT, int_table(builder, 8, true)),
        DataType::Int16 => (type_code::INT, int_table(builder, 16, true)),
        DataType::Int32 => (type_code::INT, int_table(builder, 32, true)),
        DataType::Int64 => (type_code::INT, int_table(builder, 64, true)),
        DataType::UInt8 => (type_code::INT, int_table(builder, 8, false)),
        DataType::UInt16 => (type_code::INT, int_table(builder, 16, false)),
        DataType::UInt32 => (type_code::INT, int_table(builder, 32, false)),
        DataType::UInt64 => (type_code::INT, int_table(builder, 64, false)),
        DataType::Float16 => (
            type_code::FLOATING_POINT,
            float_table(builder, precision::HALF),
        ),
        DataType::Float32 => (
            type_code::FLOATING_POINT,
            float_table(builder, precision::SINGLE),
        ),
        DataType::Float64 => (
            type_code::FLOATING_POINT,
            float_table(builder, precision::DOUBLE),
        ),
        DataType::Binary => (type_code::BINARY, empty_table(builder)),
        DataType::LargeBinary => (type_code::LARGE_BINARY, empty_table(builder)),
        DataType::Utf8 => (type_code::UTF8, empty_table(builder)),
        DataType::LargeUtf8 => (type_code::LARGE_UTF8, empty_table(builder)),
        DataType::FixedSizeBinary(width) => (
            type_code::FIXED_SIZE_BINARY,
            scalar_i32_table(builder, type_table::FIXED_SIZE_BINARY_BYTE_WIDTH, *width),
        ),
        DataType::FixedSizeList(_, size) => (
            type_code::FIXED_SIZE_LIST,
            scalar_i32_table(builder, type_table::FIXED_SIZE_LIST_LIST_SIZE, *size),
        ),
        DataType::List(_) => (type_code::LIST, empty_table(builder)),
        DataType::Struct(_) => (type_code::STRUCT, empty_table(builder)),
        DataType::Timestamp => (
            type_code::TIMESTAMP,
            // `Timestamp.unit` defaults to SECOND, so nanoseconds is written.
            // The time zone slot stays absent: AstRS timestamps are UTC by
            // construction (blueprint §6.1) and Arrow spells that "no zone".
            scalar_i16_table(
                builder,
                type_table::TIMESTAMP_UNIT,
                time_unit::NANOSECOND,
                time_unit::SECOND,
            ),
        ),
        DataType::Duration => (
            type_code::DURATION,
            // `Duration.unit` defaults to MILLISECOND.
            scalar_i16_table(
                builder,
                type_table::DURATION_UNIT,
                time_unit::NANOSECOND,
                time_unit::MILLISECOND,
            ),
        ),
    };
    (code, table)
}

/// Reads the `Type` union member `code` out of `table`.
///
/// `children` are the already-decoded `Field.children`, which the nested types
/// consume and the leaf types must find empty.
///
/// # Errors
///
/// [`IpcError::UnsupportedType`], [`IpcError::UnsupportedTypeParameters`] or
/// [`IpcError::ChildCountMismatch`].
pub fn decode_type(
    code: u8,
    table: &Table<'_>,
    name: &str,
    mut children: Vec<Field>,
) -> Result<DataType> {
    match code {
        type_code::NULL => Ok(DataType::Null),
        type_code::BOOL => Ok(DataType::Bool),
        type_code::INT => {
            let bit_width = table.i32(type_table::INT_BIT_WIDTH, 0)?;
            let signed = table.bool(type_table::INT_IS_SIGNED, false)?;
            match (bit_width, signed) {
                (8, true) => Ok(DataType::Int8),
                (16, true) => Ok(DataType::Int16),
                (32, true) => Ok(DataType::Int32),
                (64, true) => Ok(DataType::Int64),
                (8, false) => Ok(DataType::UInt8),
                (16, false) => Ok(DataType::UInt16),
                (32, false) => Ok(DataType::UInt32),
                (64, false) => Ok(DataType::UInt64),
                _ => Err(IpcError::UnsupportedTypeParameters {
                    family: "Int",
                    field: name.to_owned(),
                    detail: format!(
                        "{bit_width}-bit {}",
                        if signed { "signed" } else { "unsigned" }
                    ),
                }),
            }
        }
        type_code::FLOATING_POINT => {
            let value = table.i16(type_table::FLOAT_PRECISION, precision::HALF)?;
            match value {
                precision::HALF => Ok(DataType::Float16),
                precision::SINGLE => Ok(DataType::Float32),
                precision::DOUBLE => Ok(DataType::Float64),
                other => Err(IpcError::UnsupportedTypeParameters {
                    family: "FloatingPoint",
                    field: name.to_owned(),
                    detail: format!("precision code {other}"),
                }),
            }
        }
        type_code::BINARY => Ok(DataType::Binary),
        type_code::LARGE_BINARY => Ok(DataType::LargeBinary),
        type_code::UTF8 => Ok(DataType::Utf8),
        type_code::LARGE_UTF8 => Ok(DataType::LargeUtf8),
        type_code::FIXED_SIZE_BINARY => {
            let width = table.i32(type_table::FIXED_SIZE_BINARY_BYTE_WIDTH, 0)?;
            if width <= 0 {
                return Err(IpcError::UnsupportedTypeParameters {
                    family: "FixedSizeBinary",
                    field: name.to_owned(),
                    detail: format!("byte width {width}"),
                });
            }
            Ok(DataType::FixedSizeBinary(width))
        }
        type_code::FIXED_SIZE_LIST => {
            let size = table.i32(type_table::FIXED_SIZE_LIST_LIST_SIZE, 0)?;
            if size <= 0 {
                return Err(IpcError::UnsupportedTypeParameters {
                    family: "FixedSizeList",
                    field: name.to_owned(),
                    detail: format!("list size {size}"),
                });
            }
            let child = take_only_child(name, "FixedSizeList", &mut children)?;
            Ok(DataType::fixed_size_list(child, size))
        }
        type_code::LIST => {
            let child = take_only_child(name, "List", &mut children)?;
            Ok(DataType::list(child))
        }
        type_code::STRUCT => Ok(DataType::Struct(children)),
        type_code::TIMESTAMP => {
            let unit = table.i16(type_table::TIMESTAMP_UNIT, time_unit::SECOND)?;
            if unit != time_unit::NANOSECOND {
                return Err(IpcError::UnsupportedTypeParameters {
                    family: "Timestamp",
                    field: name.to_owned(),
                    detail: format!("{} resolution", time_unit::name(unit)),
                });
            }
            match table.string(type_table::TIMESTAMP_TIMEZONE)? {
                None | Some("") => Ok(DataType::Timestamp),
                Some(zone) => Err(IpcError::UnsupportedTypeParameters {
                    family: "Timestamp",
                    field: name.to_owned(),
                    detail: format!("time zone {zone:?}"),
                }),
            }
        }
        type_code::DURATION => {
            let unit = table.i16(type_table::DURATION_UNIT, time_unit::MILLISECOND)?;
            if unit != time_unit::NANOSECOND {
                return Err(IpcError::UnsupportedTypeParameters {
                    family: "Duration",
                    field: name.to_owned(),
                    detail: format!("{} resolution", time_unit::name(unit)),
                });
            }
            Ok(DataType::Duration)
        }
        other => Err(IpcError::UnsupportedType {
            code: other,
            name: type_code::name(other),
            field: name.to_owned(),
        }),
    }
}

/// Takes the single child a `List`/`FixedSizeList` must declare.
fn take_only_child(name: &str, family: &'static str, children: &mut Vec<Field>) -> Result<Field> {
    if children.len() != 1 {
        return Err(IpcError::ChildCountMismatch {
            field: name.to_owned(),
            family,
            expected: 1,
            actual: children.len(),
        });
    }
    children.pop().ok_or(IpcError::ChildCountMismatch {
        field: name.to_owned(),
        family,
        expected: 1,
        actual: 0,
    })
}

/// Writes a `[KeyValue]` vector, or `None` when the map is empty.
///
/// Entries are written in the map's own (sorted) order, so two runs over the
/// same schema produce byte-identical metadata.
pub fn encode_metadata(
    builder: &mut FbBuilder,
    metadata: &BTreeMap<String, String>,
) -> Option<WipOffset> {
    if metadata.is_empty() {
        return None;
    }
    let mut entries = Vec::with_capacity(metadata.len());
    for (key, value) in metadata {
        let key = builder.create_string(key);
        let value = builder.create_string(value);
        let table = builder.start_table();
        builder.push_slot_offset(key_value::KEY, key);
        builder.push_slot_offset(key_value::VALUE, value);
        entries.push(builder.end_table(table));
    }
    Some(builder.create_offset_vector(&entries))
}

/// Reads a `[KeyValue]` vector into a sorted map.
///
/// # Errors
///
/// [`IpcError::MalformedFlatbuffer`] when an entry is not a readable table.
pub fn decode_metadata(table: &Table<'_>, slot: usize) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    let Some(vector) = table.vector(slot, SIZE_UOFFSET)? else {
        return Ok(map);
    };
    for index in 0..vector.len() {
        let entry = vector.table(index)?;
        let key = entry.string(key_value::KEY)?.unwrap_or_default();
        let value = entry.string(key_value::VALUE)?.unwrap_or_default();
        map.insert(key.to_owned(), value.to_owned());
    }
    Ok(map)
}

/// An empty table — the encoding of every parameter-less Arrow type.
fn empty_table(builder: &mut FbBuilder) -> WipOffset {
    let table = builder.start_table();
    builder.end_table(table)
}

/// `table Int { bitWidth: int; is_signed: bool; }`
fn int_table(builder: &mut FbBuilder, bit_width: i32, signed: bool) -> WipOffset {
    let table = builder.start_table();
    builder.push_slot_i32(type_table::INT_BIT_WIDTH, bit_width, 0);
    builder.push_slot_bool(type_table::INT_IS_SIGNED, signed, false);
    builder.end_table(table)
}

/// `table FloatingPoint { precision: Precision; }`
fn float_table(builder: &mut FbBuilder, value: i16) -> WipOffset {
    let table = builder.start_table();
    builder.push_slot_i16(type_table::FLOAT_PRECISION, value, precision::HALF);
    builder.end_table(table)
}

/// A one-slot table holding an `int`.
fn scalar_i32_table(builder: &mut FbBuilder, slot: usize, value: i32) -> WipOffset {
    let table = builder.start_table();
    builder.push_slot_i32(slot, value, 0);
    builder.end_table(table)
}

/// A one-slot table holding a `short` with a schema default.
fn scalar_i16_table(builder: &mut FbBuilder, slot: usize, value: i16, default: i16) -> WipOffset {
    let table = builder.start_table();
    builder.push_slot_i16(slot, value, default);
    builder.end_table(table)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn round_trip(schema: &Schema) -> Schema {
        let bytes = encode_schema_bytes(schema).expect("encode");
        decode_schema_bytes(&bytes).expect("decode")
    }

    fn every_type() -> Vec<DataType> {
        let item = Field::new("item", DataType::Int32, true);
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
            DataType::FixedSizeBinary(11),
            DataType::fixed_size_list(item.clone(), 3),
            DataType::list(item.clone()),
            DataType::strukt([
                Field::new("a", DataType::Int8, false),
                Field::new("b", DataType::Utf8, true),
            ]),
            DataType::Timestamp,
            DataType::Duration,
            // Deeply nested: List<Struct<FixedSizeList<Utf8, 2>>>
            DataType::list(Field::new(
                "item",
                DataType::strukt([Field::new(
                    "grid",
                    DataType::fixed_size_list(Field::new("item", DataType::Utf8, true), 2),
                    false,
                )]),
                true,
            )),
        ]
    }

    #[test]
    fn every_type_round_trips() {
        for data_type in every_type() {
            let schema = Schema::new(vec![Field::new("f", data_type.clone(), true)]);
            let decoded = round_trip(&schema);
            assert_eq!(decoded, schema, "{data_type}");
        }
    }

    #[test]
    fn nullability_and_names_round_trip() {
        let schema = Schema::new(vec![
            Field::new("required", DataType::Int32, false),
            Field::new("optional", DataType::Int32, true),
            Field::new("速度 🦀", DataType::Float64, true),
            Field::new("", DataType::Bool, false),
        ]);
        assert_eq!(round_trip(&schema), schema);
    }

    #[test]
    fn metadata_round_trips_sorted() {
        let schema = Schema::new(vec![Field::new("data", DataType::UInt8, false)])
            .with_metadata_entry("z", "last")
            .with_metadata_entry("a", "first")
            .with_metadata_entry("empty", "");
        let decoded = round_trip(&schema);
        assert_eq!(decoded, schema);
        assert_eq!(decoded.metadata_value("empty"), Some(""));
        let keys: Vec<&str> = decoded.metadata().keys().map(String::as_str).collect();
        assert_eq!(keys, ["a", "empty", "z"]);
    }

    #[test]
    fn empty_schema_round_trips() {
        let schema = Schema::new(Vec::new());
        let decoded = round_trip(&schema);
        assert_eq!(decoded.len(), 0);
        assert!(decoded.metadata().is_empty());
    }

    #[test]
    fn nested_child_names_and_nullability_survive() {
        let inner = Field::new("element", DataType::Float32, false);
        let schema = Schema::new(vec![Field::new(
            "xyz",
            DataType::fixed_size_list(inner, 3),
            true,
        )]);
        let decoded = round_trip(&schema);
        assert_eq!(decoded, schema);
        let Some(DataType::FixedSizeList(child, size)) =
            decoded.field(0).map(|f| f.data_type().clone())
        else {
            panic!("expected a fixed size list");
        };
        assert_eq!(size, 3);
        assert_eq!(child.name(), "element");
        assert!(!child.is_nullable());
    }

    #[test]
    fn unsupported_types_are_named() {
        // Hand-build a Field whose type is `Map` (17), which AstRS rejects.
        let mut builder = FbBuilder::new();
        let name = builder.create_string("labels");
        let children = builder.create_offset_vector(&[]);
        let type_offset = empty_table(&mut builder);
        let table = builder.start_table();
        builder.push_slot_offset(field::NAME, name);
        builder.push_slot_u8(field::TYPE_TYPE, type_code::MAP, 0);
        builder.push_slot_offset(field::TYPE, type_offset);
        builder.push_slot_offset(field::CHILDREN, children);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();

        let table = root_table(&bytes).unwrap();
        let err = decode_field(&table, 1).unwrap_err();
        match err {
            IpcError::UnsupportedType { code, name, field } => {
                assert_eq!(code, type_code::MAP);
                assert_eq!(name, "Map");
                assert_eq!(field, "labels");
            }
            other => panic!("unexpected {other}"),
        }
    }

    #[test]
    fn unsupported_int_widths_are_rejected() {
        let mut builder = FbBuilder::new();
        let name = builder.create_string("weird");
        let children = builder.create_offset_vector(&[]);
        let type_offset = int_table(&mut builder, 96, true);
        let table = builder.start_table();
        builder.push_slot_offset(field::NAME, name);
        builder.push_slot_u8(field::TYPE_TYPE, type_code::INT, 0);
        builder.push_slot_offset(field::TYPE, type_offset);
        builder.push_slot_offset(field::CHILDREN, children);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();

        let err = decode_field(&root_table(&bytes).unwrap(), 1).unwrap_err();
        assert!(err.is_unsupported(), "{err}");
        assert!(err.to_string().contains("96-bit signed"), "{err}");
    }

    #[test]
    fn non_nanosecond_temporals_are_rejected() {
        for (code, slot, unit, family) in [
            (
                type_code::TIMESTAMP,
                type_table::TIMESTAMP_UNIT,
                time_unit::MICROSECOND,
                "Timestamp",
            ),
            (
                type_code::DURATION,
                type_table::DURATION_UNIT,
                time_unit::SECOND,
                "Duration",
            ),
        ] {
            let mut builder = FbBuilder::new();
            // Both defaults differ from the value written, so the slot lands.
            let type_offset = scalar_i16_table(&mut builder, slot, unit, -1);
            builder.finish(type_offset);
            let bytes = builder.finished_bytes().to_vec();
            let table = root_table(&bytes).unwrap();
            let err = decode_type(code, &table, "stamp", Vec::new()).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(family), "{text}");
            assert!(text.contains(time_unit::name(unit)), "{text}");
        }
    }

    #[test]
    fn timestamps_with_a_time_zone_are_rejected() {
        let mut builder = FbBuilder::new();
        let zone = builder.create_string("UTC");
        let table = builder.start_table();
        builder.push_slot_i16(
            type_table::TIMESTAMP_UNIT,
            time_unit::NANOSECOND,
            time_unit::SECOND,
        );
        builder.push_slot_offset(type_table::TIMESTAMP_TIMEZONE, zone);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();
        let table = root_table(&bytes).unwrap();
        let err = decode_type(type_code::TIMESTAMP, &table, "stamp", Vec::new()).unwrap_err();
        assert!(err.to_string().contains("UTC"), "{err}");
    }

    #[test]
    fn wrong_child_counts_are_rejected() {
        let mut builder = FbBuilder::new();
        let table = empty_table(&mut builder);
        builder.finish(table);
        let bytes = builder.finished_bytes().to_vec();
        let table = root_table(&bytes).unwrap();

        let err = decode_type(type_code::LIST, &table, "l", Vec::new()).unwrap_err();
        assert!(matches!(err, IpcError::ChildCountMismatch { .. }), "{err}");
        let two = vec![
            Field::new("a", DataType::Int8, true),
            Field::new("b", DataType::Int8, true),
        ];
        let err = decode_type(type_code::LIST, &table, "l", two).unwrap_err();
        assert!(err.to_string().contains("2 child field(s)"), "{err}");
    }

    #[test]
    fn non_positive_fixed_sizes_are_rejected() {
        let mut builder = FbBuilder::new();
        // A zero byte width is the flatbuffer default, so the slot is elided
        // and the reader falls back to 0 — which must still be rejected.
        let table = empty_table(&mut builder);
        builder.finish(table);
        let bytes = builder.finished_bytes().to_vec();
        let table = root_table(&bytes).unwrap();
        for code in [type_code::FIXED_SIZE_BINARY, type_code::FIXED_SIZE_LIST] {
            let children = vec![Field::new("item", DataType::Int8, true)];
            let err = decode_type(code, &table, "f", children).unwrap_err();
            assert!(err.is_unsupported(), "{err}");
        }
    }

    #[test]
    fn dictionary_encoded_fields_are_rejected() {
        let mut builder = FbBuilder::new();
        let name = builder.create_string("dict");
        let children = builder.create_offset_vector(&[]);
        let type_offset = empty_table(&mut builder);
        let dictionary = empty_table(&mut builder);
        let table = builder.start_table();
        builder.push_slot_offset(field::NAME, name);
        builder.push_slot_u8(field::TYPE_TYPE, type_code::UTF8, 0);
        builder.push_slot_offset(field::TYPE, type_offset);
        builder.push_slot_offset(field::DICTIONARY, dictionary);
        builder.push_slot_offset(field::CHILDREN, children);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();

        let err = decode_field(&root_table(&bytes).unwrap(), 1).unwrap_err();
        assert!(matches!(err, IpcError::UnsupportedDictionary), "{err}");
    }

    #[test]
    fn big_endian_schemas_are_rejected() {
        let mut builder = FbBuilder::new();
        let fields = builder.create_offset_vector(&[]);
        let table = builder.start_table();
        builder.push_slot_i16(schema_slot::ENDIANNESS, endianness::BIG, endianness::LITTLE);
        builder.push_slot_offset(schema_slot::FIELDS, fields);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();

        let err = decode_schema(&root_table(&bytes).unwrap()).unwrap_err();
        assert!(
            matches!(err, IpcError::UnsupportedEndianness { endianness: 1 }),
            "{err}"
        );
    }

    #[test]
    fn a_schema_without_fields_is_malformed() {
        let mut builder = FbBuilder::new();
        let table = builder.start_table();
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();
        let err = decode_schema(&root_table(&bytes).unwrap()).unwrap_err();
        assert!(matches!(err, IpcError::MissingField { .. }), "{err}");
    }

    #[test]
    fn a_field_without_a_type_is_malformed() {
        let mut builder = FbBuilder::new();
        let name = builder.create_string("x");
        let table = builder.start_table();
        builder.push_slot_offset(field::NAME, name);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();
        let err = decode_field(&root_table(&bytes).unwrap(), 1).unwrap_err();
        assert!(matches!(err, IpcError::MissingField { .. }), "{err}");
    }

    #[test]
    fn deep_nesting_is_rejected_on_both_sides() {
        let mut data_type = DataType::Int32;
        for _ in 0..MAX_NESTING_DEPTH {
            data_type = DataType::list(Field::new("item", data_type, true));
        }
        let schema = Schema::new(vec![Field::new("deep", data_type, true)]);
        let err = encode_schema_bytes(&schema).unwrap_err();
        assert!(matches!(err, IpcError::NestingTooDeep { .. }), "{err}");
    }

    #[test]
    fn truncated_schema_buffers_never_panic() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new(
                "b",
                DataType::list(Field::new("item", DataType::Utf8, true)),
                true,
            ),
        ]);
        let bytes = encode_schema_bytes(&schema).expect("encode");
        for cut in 0..bytes.len() {
            let _ = decode_schema_bytes(&bytes[..cut]);
        }
        assert_eq!(decode_schema_bytes(&bytes).expect("full"), schema);
    }

    #[test]
    fn duplicate_field_names_are_accepted() {
        let mut builder = FbBuilder::new();
        let mut offsets = Vec::new();
        for _ in 0..2 {
            offsets.push(
                encode_field(&mut builder, &Field::new("data", DataType::Int8, true), 1)
                    .expect("field"),
            );
        }
        let fields = builder.create_offset_vector(&offsets);
        let table = builder.start_table();
        builder.push_slot_offset(schema_slot::FIELDS, fields);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();

        let decoded = decode_schema(&root_table(&bytes).unwrap()).expect("decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.index_of("data"), Some(0));
    }

    #[test]
    fn field_metadata_is_dropped_not_rejected() {
        let mut builder = FbBuilder::new();
        let name = builder.create_string("m");
        let children = builder.create_offset_vector(&[]);
        let type_offset = empty_table(&mut builder);
        let mut map = BTreeMap::new();
        map.insert("k".to_owned(), "v".to_owned());
        let metadata = encode_metadata(&mut builder, &map).expect("metadata");
        let table = builder.start_table();
        builder.push_slot_offset(field::NAME, name);
        builder.push_slot_u8(field::TYPE_TYPE, type_code::BOOL, 0);
        builder.push_slot_offset(field::TYPE, type_offset);
        builder.push_slot_offset(field::CHILDREN, children);
        builder.push_slot_offset(field::CUSTOM_METADATA, metadata);
        let root = builder.end_table(table);
        builder.finish(root);
        let bytes = builder.finished_bytes().to_vec();

        let decoded = decode_field(&root_table(&bytes).unwrap(), 1).expect("decode");
        assert_eq!(decoded, Field::new("m", DataType::Bool, false));
    }

    #[test]
    fn empty_metadata_is_not_encoded() {
        let mut builder = FbBuilder::new();
        assert!(encode_metadata(&mut builder, &BTreeMap::new()).is_none());
    }
}
