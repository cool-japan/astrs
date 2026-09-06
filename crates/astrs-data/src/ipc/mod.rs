//! Arrow IPC **streaming format**, encoded and decoded natively.
//!
//! Every payload on every AstRS leg is a self-describing Arrow IPC stream —
//! the exact on-wire encoding arrow-rs, pyarrow, arrow-cpp and the dora tools
//! read (blueprint §6.1). This module is that encoding, implemented from the
//! byte level up with no `arrow` and no `flatbuffers` dependency.
//!
//! # The format in one screen
//!
//! ```text
//! stream  := schema-message  record-batch-message*  end-of-stream
//!
//! message := 0xFFFFFFFF          continuation marker (4 bytes)
//!            metadata_length     int32, padding included
//!            Message flatbuffer  Schema | RecordBatch
//!            padding             up to the alignment (AstRS writes 64)
//!            body                Message.bodyLength bytes, itself padded
//!
//! end-of-stream := 0xFFFFFFFF 0x00000000
//! ```
//!
//! A `RecordBatch` message describes its body with two flat vectors — one
//! `FieldNode` per array and one `Buffer` per physical buffer, both
//! depth-first over the schema — and [`layout`] is the single table both the
//! writer and the reader derive those counts from.
//!
//! # What AstRS writes
//!
//! * Metadata version **V5**, little-endian, always with the continuation
//!   marker. V4 and the pre-0.15 marker-less framing are **read**, never
//!   written.
//! * **64-byte** message and buffer alignment rather than the specification's
//!   8-byte minimum, so a body buffer inside a mapped payload starts on a
//!   cache line and is directly usable as a SIMD source.
//! * A **zero-length validity buffer** for a column without nulls, which is
//!   what arrow-cpp and pyarrow write too (arrow-rs materialises an all-ones
//!   bitmap instead — both are legal, and both are read here).
//! * No compression: AstRS negotiates that per route (§6.4), not per payload.
//!   A compressed body is rejected with [`IpcError::UnsupportedCompression`].
//!
//! # What AstRS reads
//!
//! The closed P0 type set of §6.1 and nothing else. Dictionary encoding,
//! unions, maps, decimals, intervals, the view types, non-nanosecond
//! timestamps and time zones are all rejected by name
//! ([`IpcError::UnsupportedType`], [`IpcError::UnsupportedTypeParameters`]),
//! because silently reinterpreting them would corrupt data rather than fail.
//!
//! # Layer map
//!
//! ```text
//!   payload    encode_payload · decode_payload      §6.1 one-batch convention
//!   writer     IpcStreamWriter · write_ipc_stream   framing, message order
//!   reader     IpcStreamReader · read_ipc_stream
//!   encode     BatchLayout                          arrays  -> nodes/buffers/body
//!   decode     BatchDecoder                         body    -> arrays
//!   schema     Schema/Field tables                  DataType <-> Arrow Type
//!   message    encapsulated framing                 marker, length, padding
//!   layout     node/buffer counts                   the shared source of truth
//!   format     every Arrow constant                 Message.fbs · Schema.fbs
//!   fb         FlatBuffers codec                    tables, vtables, vectors
//! ```
//!
//! # Conformance
//!
//! The compatibility gate is byte-level, not eyeballed (risk register §23 #1):
//! `tests/golden_arrow.rs` decodes every stream under `tests/golden/arrow/`
//! — written by arrow-rs 59 across alignments 8/32/64, metadata versions V4
//! and V5, and every type in the set — and compares a canonical rendering of
//! the result against the `.expect` file arrow-rs itself produced.
//!
//! ```
//! use astrs_data::array::{Float32Array, IntoArrayRef};
//! use astrs_data::ipc::{decode_payload, encode_payload};
//! use astrs_data::RecordBatch;
//!
//! let batch = RecordBatch::from_payload(
//!     Float32Array::from_opt_iter([Some(0.5), None, Some(1.5)]).into_array_ref(),
//! );
//!
//! let payload = encode_payload(&batch)?;
//! assert_eq!(decode_payload(payload.as_slice())?, batch);
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

pub mod decode;
pub mod encode;
pub mod error;
pub mod fb;
pub mod format;
pub mod layout;
pub mod message;
pub mod payload;
pub mod reader;
pub mod schema;
pub mod writer;

pub use crate::ipc::decode::BatchDecoder;
pub use crate::ipc::encode::BatchLayout;
pub use crate::ipc::error::{IpcError, Result};
pub use crate::ipc::layout::{BufferRole, MAX_NESTING_DEPTH};
pub use crate::ipc::message::MessageLimits;
pub use crate::ipc::payload::{
    PAYLOAD_ALIGNMENT, decode_payload, decode_payload_buffer, encode_payload, encode_payload_with,
};
pub use crate::ipc::reader::{
    DEFAULT_MAX_STREAM_BYTES, IpcStreamReader, ReadOptions, read_ipc_stream, read_ipc_stream_from,
};
pub use crate::ipc::schema::{decode_schema_bytes, encode_schema_bytes};
pub use crate::ipc::writer::{
    IpcStreamWriter, WriteOptions, to_ipc_bytes, to_ipc_bytes_with, write_ipc_stream,
    write_ipc_stream_with,
};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use crate::array::{ArrayRef, IntoArrayRef};
    use crate::datatype::{DataType, Field, Schema};
    use crate::ipc::{IpcStreamReader, decode_payload, encode_payload, to_ipc_bytes};
    use crate::record_batch::RecordBatch;
    use std::sync::Arc;

    /// One array of every type in the closed set, three rows each, with a null
    /// in the middle wherever the type allows one.
    fn every_type_columns() -> Vec<(Field, ArrayRef)> {
        use crate::array::{
            BinaryArray, BooleanArray, DurationArray, FixedSizeBinaryArray, FixedSizeListArray,
            Float16Array, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array,
            Int64Array, LargeBinaryArray, LargeStringArray, ListArray, NullArray, StringArray,
            StructArray, TimestampArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
        };
        use crate::datatype::F16;

        let mask = [Some(0usize), None, Some(2)];
        let opt = |values: [Option<i64>; 3]| values;
        let struct_fields = vec![
            Field::new("a", DataType::Int16, true),
            Field::new("b", DataType::Binary, true),
        ];
        let inner = StructArray::try_new(
            struct_fields.clone(),
            vec![
                Int16Array::from_opt_iter([Some(1), Some(2), None]).into_array_ref(),
                BinaryArray::from_opt_iter([Some(&b"q"[..]), None, Some(&b"rr"[..])])
                    .into_array_ref(),
            ],
            Some([true, true, false].into_iter().collect()),
        )
        .expect("struct");

        vec![
            (
                Field::new("null", DataType::Null, true),
                NullArray::new(3).into_array_ref(),
            ),
            (
                Field::new("bool", DataType::Bool, true),
                BooleanArray::from_opt_iter([Some(true), None, Some(false)]).into_array_ref(),
            ),
            (
                Field::new("i8", DataType::Int8, true),
                Int8Array::from_opt_iter([Some(-128), None, Some(127)]).into_array_ref(),
            ),
            (
                Field::new("i16", DataType::Int16, true),
                Int16Array::from_opt_iter([Some(i16::MIN), None, Some(i16::MAX)]).into_array_ref(),
            ),
            (
                Field::new("i32", DataType::Int32, true),
                Int32Array::from_opt_iter([Some(i32::MIN), None, Some(i32::MAX)]).into_array_ref(),
            ),
            (
                Field::new("i64", DataType::Int64, true),
                Int64Array::from_opt_iter(opt([Some(i64::MIN), None, Some(i64::MAX)]))
                    .into_array_ref(),
            ),
            (
                Field::new("u8", DataType::UInt8, true),
                UInt8Array::from_opt_iter([Some(0), None, Some(255)]).into_array_ref(),
            ),
            (
                Field::new("u16", DataType::UInt16, true),
                UInt16Array::from_opt_iter([Some(0), None, Some(u16::MAX)]).into_array_ref(),
            ),
            (
                Field::new("u32", DataType::UInt32, true),
                UInt32Array::from_opt_iter([Some(0), None, Some(u32::MAX)]).into_array_ref(),
            ),
            (
                Field::new("u64", DataType::UInt64, true),
                UInt64Array::from_opt_iter([Some(0), None, Some(u64::MAX)]).into_array_ref(),
            ),
            (
                Field::new("f16", DataType::Float16, true),
                Float16Array::from_opt_iter([
                    Some(F16::from_f32(1.0)),
                    None,
                    Some(F16::from_f32(-2.5)),
                ])
                .into_array_ref(),
            ),
            (
                Field::new("f32", DataType::Float32, true),
                Float32Array::from_opt_iter([Some(f32::NAN), None, Some(-0.0)]).into_array_ref(),
            ),
            (
                Field::new("f64", DataType::Float64, true),
                Float64Array::from_opt_iter([Some(f64::INFINITY), None, Some(-2.5)])
                    .into_array_ref(),
            ),
            (
                Field::new("bin", DataType::Binary, true),
                BinaryArray::from_opt_iter([Some(&b"ab"[..]), None, Some(&b""[..])])
                    .into_array_ref(),
            ),
            (
                Field::new("lbin", DataType::LargeBinary, true),
                LargeBinaryArray::from_opt_iter([Some(&b""[..]), None, Some(&b"cd"[..])])
                    .into_array_ref(),
            ),
            (
                Field::new("utf8", DataType::Utf8, true),
                StringArray::from_opt_iter([Some("hi"), None, Some("")]).into_array_ref(),
            ),
            (
                Field::new("lutf8", DataType::LargeUtf8, true),
                LargeStringArray::from_opt_iter([Some(""), None, Some("速度")]).into_array_ref(),
            ),
            (
                Field::new("fsb", DataType::FixedSizeBinary(3), true),
                FixedSizeBinaryArray::try_from_opt_iter(
                    3,
                    [Some([1u8, 2, 3]), None, Some([9, 8, 7])],
                )
                .expect("fsb")
                .into_array_ref(),
            ),
            (
                Field::new(
                    "fsl",
                    DataType::fixed_size_list(Field::new("item", DataType::Int8, false), 2),
                    true,
                ),
                FixedSizeListArray::try_new(
                    Field::new("item", DataType::Int8, false),
                    2,
                    Int8Array::from_values([1, -2, 3, -4, 5, -6]).into_array_ref(),
                    Some(mask.iter().map(Option::is_some).collect()),
                )
                .expect("fsl")
                .into_array_ref(),
            ),
            (
                Field::new(
                    "list",
                    DataType::list(Field::new("item", DataType::Float64, true)),
                    true,
                ),
                ListArray::try_new(
                    Field::new("item", DataType::Float64, true),
                    crate::buffer::ScalarBuffer::from_slice(&[0i32, 2, 2, 3]),
                    Float64Array::from_opt_iter([Some(1.5), None, Some(-0.25)]).into_array_ref(),
                    Some(mask.iter().map(Option::is_some).collect()),
                )
                .expect("list")
                .into_array_ref(),
            ),
            (
                Field::new("struct", DataType::Struct(struct_fields), true),
                inner.into_array_ref(),
            ),
            (
                Field::new("ts", DataType::Timestamp, true),
                TimestampArray::from_opt_nanos([Some(0), None, Some(1_700_000_000_123_456_789)])
                    .into_array_ref(),
            ),
            (
                Field::new("dur", DataType::Duration, true),
                DurationArray::from_opt_nanos([Some(-1), None, Some(1_000_000_000)])
                    .into_array_ref(),
            ),
        ]
    }

    fn every_type_batch() -> RecordBatch {
        let (fields, columns): (Vec<Field>, Vec<ArrayRef>) =
            every_type_columns().into_iter().unzip();
        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).expect("batch")
    }

    #[test]
    fn every_type_survives_a_stream_round_trip() {
        let batch = every_type_batch();
        let bytes = to_ipc_bytes(std::slice::from_ref(&batch)).expect("write");
        let mut reader = IpcStreamReader::from_slice(bytes.as_slice()).expect("reader");
        let decoded = reader.next_batch().expect("decode").expect("batch");
        assert_eq!(decoded.schema(), batch.schema());
        assert_eq!(decoded, batch);
    }

    #[test]
    fn every_type_survives_a_payload_round_trip_column_by_column() {
        for (field, column) in every_type_columns() {
            let batch = RecordBatch::try_new(
                Arc::new(Schema::new(vec![field.clone()])),
                vec![Arc::clone(&column)],
            )
            .expect("batch");
            let payload = encode_payload(&batch).expect("encode");
            let decoded = decode_payload(payload.as_slice()).expect("decode");
            assert_eq!(decoded.num_rows(), 3, "{}", field.name());
            assert_eq!(
                decoded.column(0).map(|c| c.data_type().clone()),
                Some(field.data_type().clone()),
                "{}",
                field.name()
            );
            assert_eq!(decoded, batch, "{}", field.name());
        }
    }
}
