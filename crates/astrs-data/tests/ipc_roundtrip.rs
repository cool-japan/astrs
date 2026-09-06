//! The AstRS side of the Arrow IPC gate: batches AstRS builds, encoded by the
//! AstRS writer, read back by the AstRS reader — value for value, at every
//! alignment, for every shape of the closed §6.1 type set.
//!
//! `tests/golden_arrow.rs` proves the *read* direction against bytes arrow-rs
//! produced. This file proves the *write* direction is lossless, and pins the
//! two physical properties the shared-memory plane depends on (blueprint
//! §6.1):
//!
//! * every body buffer starts on a 64-byte boundary inside the message body,
//!   so a mapped tensor column is directly usable as a SIMD source;
//! * a whole payload is padded to 128 bytes, so payloads pack back to back in
//!   a slot without any of them losing that property.
//!
//! `tests/cross_validate.rs` exports the same corpus for the out-of-workspace
//! arrow-rs harness, which closes the loop: what this file calls "lossless"
//! is confirmed by the other implementation, not only by ours.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/cases.rs"]
mod cases;
#[path = "common/render.rs"]
mod render;

use std::sync::Arc;

use astrs_data::array::{
    Array, ArrayExt, FixedSizeListArray, Float32Array, Int64Array, IntoArrayRef, ListArray,
    StringArray, StructArray,
};
use astrs_data::buffer::{AlignedBuf, Buffer};
use astrs_data::ipc::format::{message_header, record_batch};
use astrs_data::ipc::message::{MessageLimits, decode_message, scan_message};
use astrs_data::ipc::{
    IpcError, IpcStreamReader, IpcStreamWriter, PAYLOAD_ALIGNMENT, ReadOptions, WriteOptions,
    decode_payload, decode_payload_buffer, encode_payload, to_ipc_bytes, to_ipc_bytes_with,
    write_ipc_stream,
};
use astrs_data::tensor::TensorView;
use astrs_data::{ALIGNMENT, DATA_COLUMN, DataType, Field, MAX_PAYLOAD_BYTES, RecordBatch, Schema};

use crate::cases::{Case, case, cases};

/// Encodes a case at the default framing.
fn encode(case: &Case) -> AlignedBuf {
    to_ipc_bytes_with(Arc::clone(&case.schema), &case.batches, WriteOptions::new())
        .unwrap_or_else(|err| panic!("{}: write: {err}", case.name))
}

/// Decodes a stream, returning the schema it declares and its batches.
fn decode(name: &str, bytes: &[u8]) -> (Arc<Schema>, Vec<RecordBatch>) {
    let mut reader =
        IpcStreamReader::from_slice(bytes).unwrap_or_else(|err| panic!("{name}: open: {err}"));
    let batches = reader
        .read_all()
        .unwrap_or_else(|err| panic!("{name}: batches: {err}"));
    (reader.schema_ref(), batches)
}

/// Asserts that `bytes` decodes to exactly the case that produced it.
fn assert_round_trip(case: &Case, bytes: &[u8], context: &str) {
    let (schema, batches) = decode(case.name, bytes);
    assert_eq!(
        schema.as_ref(),
        case.schema.as_ref(),
        "{}: {context}: schema",
        case.name
    );
    assert_eq!(
        batches.len(),
        case.batches.len(),
        "{}: {context}: batch count",
        case.name
    );
    let expected = render::render_stream(&case.schema, &case.batches);
    let actual = render::render_stream(&schema, &batches);
    assert_eq!(
        actual,
        expected,
        "{}: {context}: {}",
        case.name,
        render::first_difference(&expected, &actual)
    );
    // The rendering is the logical oracle; array equality is the structural
    // one, and both have to hold.
    for (index, (left, right)) in batches.iter().zip(case.batches.iter()).enumerate() {
        assert_eq!(left, right, "{}: {context}: batch {index}", case.name);
    }
}

/// Every `(body_start, buffers)` pair of every record batch message in a
/// stream, read back out of the flatbuffer headers rather than inferred.
fn batch_buffer_layouts(bytes: &[u8]) -> Vec<(usize, Vec<(i64, i64)>)> {
    let limits = MessageLimits::default();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(message) = scan_message(bytes, pos, &limits).expect("scan") {
        let info = decode_message(&bytes[message.metadata.clone()]).expect("header");
        if info.header_type == message_header::RECORD_BATCH {
            let vector = info
                .header
                .vector(record_batch::BUFFERS, 16)
                .expect("buffers")
                .expect("a buffer vector");
            let buffers = (0..vector.len())
                .map(|index| vector.struct_pair(index).expect("buffer entry"))
                .collect();
            out.push((message.body.start, buffers));
        }
        pos = message.next;
    }
    out
}

#[test]
fn the_corpus_covers_every_type_of_the_closed_set() {
    // A guard against the corpus quietly losing a type: every §6.1 type must
    // appear somewhere in it, nested types included.
    fn collect(data_type: &DataType, out: &mut Vec<String>) {
        out.push(render::render_type(data_type));
        for child in data_type.children() {
            collect(child.data_type(), out);
        }
    }

    let mut seen = Vec::new();
    for case in cases() {
        for field in case.schema.fields() {
            collect(field.data_type(), &mut seen);
        }
    }
    for required in [
        "null",
        "bool",
        "i8",
        "i16",
        "i32",
        "i64",
        "u8",
        "u16",
        "u32",
        "u64",
        "f16",
        "f32",
        "f64",
        "binary",
        "largebinary",
        "utf8",
        "largeutf8",
        "timestamp(ns)",
        "duration(ns)",
    ] {
        assert!(
            seen.iter().any(|entry| entry == required),
            "the corpus lost type {required}"
        );
    }
    for prefix in ["fsb(", "fsl(", "list(", "struct("] {
        assert!(
            seen.iter().any(|entry| entry.starts_with(prefix)),
            "the corpus lost type {prefix}..)"
        );
    }
}

#[test]
fn every_case_round_trips_through_the_stream_writer() {
    for case in cases() {
        let bytes = encode(&case);
        assert_round_trip(&case, bytes.as_slice(), "default framing");
    }
}

#[test]
fn every_case_round_trips_at_every_alignment() {
    // Only the framing changes with the alignment; the values must not.
    for case in cases() {
        for alignment in [8usize, 16, 32, 64, 256] {
            let bytes = to_ipc_bytes_with(
                Arc::clone(&case.schema),
                &case.batches,
                WriteOptions::new().with_alignment(alignment),
            )
            .unwrap_or_else(|err| panic!("{}: write at {alignment}: {err}", case.name));
            assert_round_trip(&case, bytes.as_slice(), &format!("alignment {alignment}"));

            let limits = MessageLimits::default();
            let mut pos = 0usize;
            while let Some(message) = scan_message(bytes.as_slice(), pos, &limits).expect("scan") {
                assert_eq!(pos % alignment, 0, "{}: message at {pos}", case.name);
                assert_eq!(
                    message.body.start % alignment,
                    0,
                    "{}: body at {pos}",
                    case.name
                );
                pos = message.next;
            }
        }
    }
}

#[test]
fn every_case_round_trips_through_a_reader_over_a_std_read() {
    // The `Read` entry point buffers the whole stream itself; it must agree
    // with the in-memory one byte for byte.
    for case in cases() {
        let bytes = encode(&case);
        let (schema, batches) = astrs_data::ipc::read_ipc_stream_from(bytes.as_slice())
            .unwrap_or_else(|err| panic!("{}: read_ipc_stream_from: {err}", case.name));
        let expected = render::render_stream(&case.schema, &case.batches);
        let actual = render::render_stream(&schema, &batches);
        assert_eq!(actual, expected, "{}", case.name);
    }
}

#[test]
fn every_case_survives_an_incremental_writer() {
    // Writing batch by batch must produce exactly the bytes the one-shot
    // helper produces.
    for case in cases() {
        let one_shot = encode(&case);
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = IpcStreamWriter::try_new(&mut sink, Arc::clone(&case.schema))
                .unwrap_or_else(|err| panic!("{}: open: {err}", case.name));
            for batch in &case.batches {
                writer
                    .write(batch)
                    .unwrap_or_else(|err| panic!("{}: write: {err}", case.name));
            }
            writer
                .finish()
                .unwrap_or_else(|err| panic!("{}: finish: {err}", case.name));
            assert_eq!(
                writer.batches_written(),
                case.batches.len(),
                "{}",
                case.name
            );
            assert_eq!(writer.bytes_written(), sink.len(), "{}", case.name);
        }
        assert_eq!(sink, one_shot.as_slice(), "{}", case.name);
    }
}

#[test]
fn every_single_batch_case_round_trips_as_a_payload() {
    for case in cases() {
        if case.batches.len() != 1 {
            continue;
        }
        let batch = &case.batches[0];
        let payload = encode_payload(batch)
            .unwrap_or_else(|err| panic!("{}: encode_payload: {err}", case.name));
        let decoded = decode_payload(payload.as_slice())
            .unwrap_or_else(|err| panic!("{}: decode_payload: {err}", case.name));
        assert_eq!(&decoded, batch, "{}", case.name);
        assert_eq!(
            decoded.schema().as_ref(),
            batch.schema().as_ref(),
            "{}: schema",
            case.name
        );
    }
}

#[test]
fn a_payload_is_128_byte_padded_and_64_byte_based() {
    // Probe (d) of the alignment contract: the payload as a whole.
    for case in cases() {
        if case.batches.len() != 1 {
            continue;
        }
        let payload = encode_payload(&case.batches[0]).expect("payload");
        assert_eq!(
            payload.len() % PAYLOAD_ALIGNMENT,
            0,
            "{}: payload length is not slot-aligned",
            case.name
        );
        assert_eq!(
            payload.as_ptr() as usize % ALIGNMENT,
            0,
            "{}: payload base is not buffer-aligned",
            case.name
        );
        // The padding sits past the end-of-stream marker, so the payload is
        // still exactly one stream.
        let (_, batches) = decode(case.name, payload.as_slice());
        assert_eq!(batches.len(), 1, "{}", case.name);
    }
}

#[test]
fn every_body_buffer_starts_on_a_64_byte_boundary() {
    // Probe (d) of the alignment contract: the buffers inside the body. The
    // offsets are read back out of the flatbuffer header, not inferred from
    // what the writer intended.
    for case in cases() {
        let bytes = encode(&case);
        let layouts = batch_buffer_layouts(bytes.as_slice());
        assert_eq!(layouts.len(), case.batches.len(), "{}", case.name);
        for (body_start, buffers) in layouts {
            assert_eq!(body_start % ALIGNMENT, 0, "{}: body start", case.name);
            for (index, (offset, length)) in buffers.iter().enumerate() {
                assert!(
                    *offset >= 0 && *length >= 0,
                    "{}: buffer {index}",
                    case.name
                );
                assert_eq!(
                    offset % ALIGNMENT as i64,
                    0,
                    "{}: buffer {index} at offset {offset} is not 64-byte aligned",
                    case.name
                );
                // And the absolute address inside the aligned stream buffer is
                // aligned too, which is what SIMD code actually needs.
                let address = bytes.as_ptr() as usize + body_start + *offset as usize;
                assert_eq!(
                    address % ALIGNMENT,
                    0,
                    "{}: buffer {index} lands at an unaligned address",
                    case.name
                );
            }
        }
    }
}

#[test]
fn a_multi_batch_stream_keeps_its_batch_boundaries() {
    let case = case("multi_batch");
    let bytes = encode(&case);
    let (schema, batches) = decode(case.name, bytes.as_slice());
    assert_eq!(schema.as_ref(), case.schema.as_ref());
    assert_eq!(batches.len(), 4);
    assert_eq!(
        batches
            .iter()
            .map(RecordBatch::num_rows)
            .collect::<Vec<_>>(),
        vec![3, 0, 1, 5],
        "row counts must not be merged or reordered"
    );

    // The reader is an iterator too, and it must yield the same batches.
    let mut reader = IpcStreamReader::from_slice(bytes.as_slice()).expect("reader");
    let mut streamed = Vec::new();
    while let Some(batch) = reader.next_batch().expect("next") {
        streamed.push(batch);
    }
    assert_eq!(streamed, batches);
}

#[test]
fn a_schema_only_stream_carries_a_schema_and_no_batches() {
    let case = case("schema_only");
    let bytes = encode(&case);
    let (schema, batches) = decode(case.name, bytes.as_slice());
    assert_eq!(schema.as_ref(), case.schema.as_ref());
    assert!(batches.is_empty());
    // A payload needs exactly one batch, so this stream is not a payload.
    assert!(matches!(
        decode_payload(bytes.as_slice()),
        Err(IpcError::NotASinglePayload { count: 0 })
    ));
}

#[test]
fn a_zero_column_batch_keeps_its_row_count() {
    let case = case("zero_columns");
    let bytes = encode(&case);
    let (schema, batches) = decode(case.name, bytes.as_slice());
    assert_eq!(schema.len(), 0);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_columns(), 0);
    assert_eq!(
        batches[0].num_rows(),
        5,
        "a column-less batch still declares its rows"
    );
}

#[test]
fn an_empty_batch_and_an_all_null_batch_both_survive() {
    for name in [
        "primitives_empty",
        "bytes_empty",
        "nested_empty",
        "primitives_all_null",
        "nested_all_null",
    ] {
        let case = case(name);
        let bytes = encode(&case);
        assert_round_trip(&case, bytes.as_slice(), "degenerate shape");

        let (_, batches) = decode(name, bytes.as_slice());
        for column in batches[0].columns() {
            if name.ends_with("all_null") {
                assert_eq!(
                    column.null_count(),
                    column.len(),
                    "{name}: every row should be null"
                );
            } else {
                assert_eq!(column.len(), 0, "{name}: no rows expected");
            }
        }
    }
}

#[test]
fn a_list_of_structs_keeps_every_nested_value() {
    let case = case("list_of_struct");
    let bytes = encode(&case);
    let (_, batches) = decode(case.name, bytes.as_slice());
    let column = batches[0].column(0).expect("column");
    let lists = column.downcast::<ListArray>().expect("list");

    assert_eq!(lists.len(), 3);
    assert!(!lists.is_null(0));
    assert!(!lists.is_null(1));
    assert!(lists.is_null(2), "row 2 is a null list");
    assert_eq!(lists.value_length(0), Some(2));
    assert_eq!(lists.value_length(1), Some(0), "row 1 is the empty list");

    let row0 = lists.value(0).expect("row 0");
    let structs = row0.downcast::<StructArray>().expect("struct");
    assert_eq!(structs.len(), 2);
    let ids = structs
        .column_by_name("id")
        .expect("id")
        .downcast::<astrs_data::array::UInt32Array>()
        .expect("u32");
    assert_eq!(ids.value(0), Some(10));
    assert_eq!(ids.value(1), Some(11));
    let scores = structs
        .column_by_name("score")
        .expect("score")
        .downcast::<Float32Array>()
        .expect("f32");
    assert_eq!(scores.value(0), Some(0.5));
    assert!(scores.is_null(1), "the second score is null");
    let labels = structs
        .column_by_name("label")
        .expect("label")
        .downcast::<StringArray>()
        .expect("utf8");
    assert_eq!(labels.value(0), Some("a"));
    assert_eq!(labels.value(1), Some("b"));
}

#[test]
fn a_fixed_size_list_payload_reads_back_as_a_tensor() {
    // The §6.1 tensor convention: one `data` column of `FixedSizeList<f32,4>`
    // is a `rows x 4` tensor, and it has to survive the wire as one.
    let case = case("dora_payload");
    let payload = encode_payload(&case.batches[0]).expect("payload");
    let decoded = decode_payload(payload.as_slice()).expect("decode");

    let column = decoded.column_by_name(DATA_COLUMN).expect("data column");
    let tensor_column = column.downcast::<FixedSizeListArray>().expect("fsl");
    assert_eq!(tensor_column.len(), 1024);

    let values = tensor_column
        .values()
        .downcast::<Float32Array>()
        .expect("f32 child");
    let view = TensorView::from_primitive(values, vec![1024, 4]).expect("tensor view");
    assert_eq!(view.shape(), &[1024, 4]);
    assert_eq!(view.get(&[0, 0]), Ok(0.0));
    assert_eq!(view.get(&[0, 3]), Ok(1.5));
    assert_eq!(view.get(&[1023, 3]), Ok(4095.0 * 0.5));
    assert!(
        view.is_contiguous(),
        "a decoded tensor column must stay contiguous"
    );
}

#[test]
fn sliced_columns_are_rebased_by_the_writer() {
    // A column sliced away from offset zero must be written as if it had been
    // built that way: the body must not carry the rows in front of it.
    let case = case("sliced_columns");
    let bytes = encode(&case);
    assert_round_trip(&case, bytes.as_slice(), "sliced");

    let (_, batches) = decode(case.name, bytes.as_slice());
    let decoded = &batches[0];
    let strings = decoded
        .column_by_name("strings")
        .expect("strings")
        .downcast::<StringArray>()
        .expect("utf8");
    assert_eq!(strings.len(), 5);
    assert_eq!(strings.value(0), Some("row-3"));
    assert!(strings.is_null(1), "the sliced null must stay at row 1");
    assert_eq!(strings.value(4), Some("row-7"));

    // Re-basing also means the encoded body is proportional to the slice, not
    // to the array it came from: five rows of five columns is well under a
    // kilobyte of values.
    let layouts = batch_buffer_layouts(bytes.as_slice());
    let (_, buffers) = &layouts[0];
    let payload: i64 = buffers.iter().map(|(_, length)| *length).sum();
    assert!(
        payload < 1024,
        "the writer carried the whole backing buffer ({payload} bytes)"
    );
}

#[test]
fn a_decoded_payload_shares_the_buffer_it_was_decoded_from() {
    // The zero-copy path: decoding an owned, aligned buffer hands out array
    // buffers that are windows into that same allocation.
    let batch = RecordBatch::from_payload(
        Int64Array::from_values((0..512).map(i64::from)).into_array_ref(),
    );
    let payload = encode_payload(&batch).expect("payload");
    let buffer = Buffer::from(payload);
    let start = buffer.as_ptr() as usize;
    let end = start + buffer.as_slice().len();

    let decoded = decode_payload_buffer(buffer.clone()).expect("decode");
    let column = decoded
        .column(0)
        .expect("column")
        .downcast::<Int64Array>()
        .expect("i64");
    let address = column.values().as_ptr() as usize;
    assert!(
        (start..end).contains(&address),
        "the decoded column should point into the input buffer"
    );
    assert_eq!(column.values()[7], 7);
    assert!(
        buffer.share_count() > 1,
        "the decoded batch should hold a share of the input buffer"
    );
}

#[test]
fn decoding_a_borrowed_slice_copies_once() {
    // The documented copy path: a `&[u8]` is copied into an aligned buffer
    // exactly once, and the decoded arrays point into the copy — never into
    // the caller's slice, whose alignment and lifetime we do not own.
    let batch = RecordBatch::from_payload(
        Int64Array::from_values((0..512).map(i64::from)).into_array_ref(),
    );
    let payload = encode_payload(&batch).expect("payload");
    let bytes = payload.as_slice().to_vec();
    let start = bytes.as_ptr() as usize;
    let end = start + bytes.len();

    let decoded = decode_payload(&bytes).expect("decode");
    let column = decoded
        .column(0)
        .expect("column")
        .downcast::<Int64Array>()
        .expect("i64");
    let address = column.values().as_ptr() as usize;
    assert!(
        !(start..end).contains(&address),
        "a borrowed slice must be copied, not aliased"
    );
    assert_eq!(decoded, batch);
}

#[test]
fn a_misaligned_stream_still_decodes() {
    // Not every producer hands us a 64-byte aligned buffer. The reader must
    // fall back to copying the buffers it cannot window, and still be right.
    let case = case("primitives_nulls");
    let bytes = encode(&case);
    let mut shifted = vec![0u8; bytes.len() + 1];
    shifted[1..].copy_from_slice(bytes.as_slice());
    assert_round_trip(&case, &shifted[1..], "misaligned input");
}

#[test]
fn the_payload_cap_is_enforced_before_anything_is_read() {
    // §6.1's 256 MB ceiling. The allocation below is virtual: the pages are
    // never touched, because the length check happens before the first read.
    let oversized = vec![0u8; MAX_PAYLOAD_BYTES + 1];
    assert!(matches!(
        decode_payload(&oversized),
        Err(IpcError::TooLarge {
            what: "payload",
            cap,
            ..
        }) if cap == MAX_PAYLOAD_BYTES as u64
    ));
    assert!(matches!(
        decode_payload_buffer(Buffer::zeroed(MAX_PAYLOAD_BYTES + 1)),
        Err(IpcError::TooLarge {
            what: "payload",
            ..
        })
    ));
    // And the message limits a payload decode runs under are the §6.1 cap,
    // not the reader's more generous stream default.
    assert_eq!(
        MessageLimits::default().max_body_bytes,
        MAX_PAYLOAD_BYTES,
        "the default body cap is the payload cap"
    );
}

#[test]
fn a_body_larger_than_the_limit_is_refused_without_allocating() {
    // The same ceiling from the other side: a stream whose declared body
    // exceeds the caller's limit is rejected on the header alone.
    let case = case("wide_rows");
    let bytes = encode(&case);
    let options = ReadOptions::new().with_limits(MessageLimits::default().with_max_body_bytes(64));
    let buffer = Buffer::from(AlignedBuf::from_slice(bytes.as_slice()));
    let mut reader = IpcStreamReader::with_options(buffer, options).expect("schema fits");
    assert!(matches!(
        reader.next_batch(),
        Err(IpcError::TooLarge { what: "body", .. })
    ));
}

#[test]
fn a_stream_longer_than_the_limit_is_refused() {
    // `max_stream_bytes` bounds what is slurped out of an unbounded source;
    // an in-memory buffer is already bounded by its own length, and is capped
    // by `decode_payload`'s §6.1 check instead.
    let case = case("wide_rows");
    let bytes = encode(&case);
    let options = ReadOptions::new().with_max_stream_bytes(128);
    assert!(matches!(
        IpcStreamReader::from_reader_with_options(bytes.as_slice(), options),
        Err(IpcError::TooLarge { what: "stream", .. })
    ));
    // The same source under the default cap reads fine.
    let mut reader = IpcStreamReader::from_reader(bytes.as_slice()).expect("default cap");
    assert_eq!(reader.read_all().expect("batches").len(), 1);
}

#[test]
fn the_writer_refuses_a_batch_from_another_schema() {
    let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, true)]));
    let other = RecordBatch::from_payload(Int64Array::from_values([1, 2]).into_array_ref());
    let mut sink: Vec<u8> = Vec::new();
    let mut writer = IpcStreamWriter::try_new(&mut sink, schema).expect("open");
    assert!(matches!(
        writer.write(&other),
        Err(IpcError::SchemaMismatch { index: 0 })
    ));
}

#[test]
fn a_stream_without_batches_has_no_schema_to_infer() {
    let empty: Vec<RecordBatch> = Vec::new();
    assert!(matches!(
        write_ipc_stream(&mut Vec::new(), &empty),
        Err(IpcError::MissingSchema)
    ));
    assert!(matches!(to_ipc_bytes(&empty), Err(IpcError::MissingSchema)));
}

#[test]
fn truncating_one_of_our_own_streams_never_panics() {
    // Every prefix of every case must be rejected or decode to a prefix of
    // the batches — never panic, never hang.
    for case in cases() {
        let bytes = encode(&case);
        let step = (bytes.len() / 48).max(1);
        let mut cut = 0usize;
        while cut < bytes.len() {
            if let Ok(mut reader) = IpcStreamReader::from_slice(&bytes.as_slice()[..cut]) {
                let _ = reader.read_all();
            }
            cut += step;
        }
    }
}

#[test]
fn corrupting_one_of_our_own_streams_never_panics() {
    for name in ["all_types_stand_in", "list_of_struct", "sliced_columns"] {
        let case = if name == "all_types_stand_in" {
            case("primitives_nulls")
        } else {
            case(name)
        };
        let bytes = encode(&case);
        for index in (0..bytes.len()).step_by(11) {
            for flip in [0x01u8, 0x40, 0xff] {
                let mut corrupted = bytes.as_slice().to_vec();
                corrupted[index] ^= flip;
                if let Ok(mut reader) = IpcStreamReader::from_slice(&corrupted) {
                    let _ = reader.read_all();
                }
                let _ = decode_payload(&corrupted);
            }
        }
    }
}
