//! The Arrow byte-compatibility gate (blueprint risk register §23 #1).
//!
//! Every `.arrows` file under `tests/golden/arrow/` was written by **arrow-rs
//! 59** in an out-of-workspace generator, together with a `.expect` sibling
//! holding a canonical rendering of what arrow-rs itself read back out of it.
//! This suite decodes each file with `astrs_data::ipc` and compares its own
//! rendering byte for byte. A passing run means the two implementations agree
//! on every type, value, null and framing decision of the corpus — the
//! compatibility gate the blueprint asks for is byte equality, not eyeballs.
//!
//! The corpus spans, deliberately:
//!
//! * every type of the closed §6.1 set (`all_types`), plus the nested
//!   combinations robotics payloads use (`nested_list`, `list_of_list`,
//!   `nested_struct`, `fixed_size_list`);
//! * the degenerate shapes (`*_empty`, `zero_columns`, `schema_only`,
//!   `null_type_empty`, `primitives_all_null`, `primitives_no_null`);
//! * float bit patterns that must survive verbatim (`float_edge_cases`);
//! * arrays with a non-zero offset, which force a writer to re-base
//!   (`sliced_arrays`);
//! * the framing variants: 8/32/64-byte alignment, and a **V4 stream with no
//!   continuation marker and no end-of-stream marker**
//!   (`legacy_no_continuation`);
//! * schema metadata (`schema_metadata`) and the dora single-`data`-column
//!   payload convention (`dora_payload`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/render.rs"]
mod render;

use std::path::{Path, PathBuf};

use astrs_data::ipc::message::{MessageLimits, decode_message, scan_message};
use astrs_data::ipc::{IpcStreamReader, WriteOptions, to_ipc_bytes_with};
use astrs_data::{RecordBatch, Schema};

/// The committed corpus directory.
fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/arrow")
}

/// Every `.arrows` file, sorted, with its `.expect` sibling.
fn golden_cases() -> Vec<(String, PathBuf, PathBuf)> {
    let dir = golden_dir();
    let mut cases: Vec<(String, PathBuf, PathBuf)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("arrows"))
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("utf-8 file name")
                .to_owned();
            let expect = path.with_extension("expect");
            (name, path, expect)
        })
        .collect();
    cases.sort_by(|a, b| a.0.cmp(&b.0));
    cases
}

/// Decodes one golden stream.
fn decode(path: &Path) -> (Schema, Vec<RecordBatch>) {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let mut reader = IpcStreamReader::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("{}: schema: {err}", path.display()));
    let batches = reader
        .read_all()
        .unwrap_or_else(|err| panic!("{}: batches: {err}", path.display()));
    (reader.schema().as_ref().clone(), batches)
}

#[test]
fn the_corpus_is_present_and_complete() {
    let cases = golden_cases();
    assert!(
        cases.len() >= 28,
        "the committed corpus shrank to {} case(s)",
        cases.len()
    );
    for (name, arrows, expect) in &cases {
        assert!(arrows.is_file(), "{name}: missing stream");
        assert!(expect.is_file(), "{name}: missing .expect sibling");
    }
    // The cases the rest of this file reasons about by name must exist.
    let names: Vec<&str> = cases.iter().map(|(name, _, _)| name.as_str()).collect();
    for required in [
        "all_types",
        "align32",
        "align64",
        "bytes_empty",
        "bytes_types",
        "dora_payload",
        "fixed_size_list",
        "float_edge_cases",
        "large_values",
        "legacy_no_continuation",
        "list_of_list",
        "multi_batch",
        "nested_list",
        "nested_struct",
        "null_type",
        "primitives_all_null",
        "primitives_empty",
        "primitives_no_null",
        "primitives_nulls",
        "schema_metadata",
        "schema_only",
        "sliced_arrays",
        "zero_columns",
    ] {
        assert!(names.contains(&required), "missing golden case {required}");
    }
}

#[test]
fn every_golden_stream_decodes_exactly_as_arrow_rs_read_it() {
    let mut checked = 0usize;
    for (name, arrows, expect_path) in golden_cases() {
        let (schema, batches) = decode(&arrows);
        let rendered = render::render_stream(&schema, &batches);
        let expected =
            std::fs::read_to_string(&expect_path).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(
            rendered,
            expected,
            "{name}: {}",
            render::first_difference(&expected, &rendered)
        );
        checked += 1;
    }
    assert!(checked >= 28, "only {checked} case(s) were checked");
}

#[test]
fn re_encoding_a_golden_stream_preserves_every_value() {
    // Decode arrow-rs's bytes, write them back out with the AstRS writer,
    // decode again: the rendering must still match the golden. This is the
    // writer's half of the compatibility gate — the reader alone could agree
    // with arrow-rs while the writer dropped a null mask.
    for (name, arrows, expect_path) in golden_cases() {
        let (schema, batches) = decode(&arrows);
        let schema = std::sync::Arc::new(schema);
        let bytes = to_ipc_bytes_with(
            std::sync::Arc::clone(&schema),
            &batches,
            WriteOptions::new(),
        )
        .unwrap_or_else(|err| panic!("{name}: write: {err}"));

        let mut reader = IpcStreamReader::from_slice(bytes.as_slice())
            .unwrap_or_else(|err| panic!("{name}: re-read: {err}"));
        let round_tripped = reader
            .read_all()
            .unwrap_or_else(|err| panic!("{name}: re-read batches: {err}"));
        assert_eq!(reader.schema().as_ref(), schema.as_ref(), "{name}: schema");

        let rendered = render::render_stream(reader.schema(), &round_tripped);
        let expected =
            std::fs::read_to_string(&expect_path).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(
            rendered,
            expected,
            "{name}: {}",
            render::first_difference(&expected, &rendered)
        );
    }
}

#[test]
fn re_encoding_is_stable_at_every_alignment() {
    // The same batches at 8, 32, 64 and 256 byte alignment must decode to the
    // same values; only the framing changes.
    let (schema, batches) = decode(&golden_dir().join("all_types.arrows"));
    let schema = std::sync::Arc::new(schema);
    let reference = render::render_stream(&schema, &batches);
    for alignment in [8usize, 32, 64, 256] {
        let bytes = to_ipc_bytes_with(
            std::sync::Arc::clone(&schema),
            &batches,
            WriteOptions::new().with_alignment(alignment),
        )
        .expect("write");
        let mut reader = IpcStreamReader::from_slice(bytes.as_slice()).expect("reader");
        let decoded = reader.read_all().expect("batches");
        assert_eq!(
            render::render_stream(reader.schema(), &decoded),
            reference,
            "alignment {alignment}"
        );

        // And the framing really is at that alignment.
        let limits = MessageLimits::default();
        let mut pos = 0usize;
        while let Some(message) = scan_message(bytes.as_slice(), pos, &limits).expect("scan") {
            assert_eq!(pos % alignment, 0, "message at {pos}");
            assert_eq!(message.body.start % alignment, 0, "body at {pos}");
            pos = message.next;
        }
    }
}

#[test]
fn the_legacy_stream_has_neither_marker_nor_terminator() {
    // `legacy_no_continuation.arrows` is arrow-rs's pre-0.15 framing: a bare
    // 4-byte length prefix, metadata version V4, and no end-of-stream marker.
    let path = golden_dir().join("legacy_no_continuation.arrows");
    let bytes = std::fs::read(&path).expect("read");
    let limits = MessageLimits::default();

    let first = scan_message(&bytes, 0, &limits)
        .expect("scan")
        .expect("schema message");
    assert!(!first.continued, "the legacy framing has no marker");
    let info = decode_message(&bytes[first.metadata.clone()]).expect("decode");
    assert_eq!(
        info.version,
        astrs_data::ipc::format::MetadataVersion::V4,
        "the legacy case pins V4"
    );

    let second = scan_message(&bytes, first.next, &limits)
        .expect("scan")
        .expect("batch message");
    // The legacy terminator is a bare 4-byte zero length, with no marker in
    // front of it; anything past it would be a third message.
    assert_eq!(
        bytes.len() - second.next,
        4,
        "only the bare legacy terminator follows the batch"
    );
    assert_eq!(&bytes[second.next..], &[0u8, 0, 0, 0]);
    assert!(
        scan_message(&bytes, second.next, &limits)
            .expect("scan")
            .is_none(),
        "a zero length prefix ends the stream"
    );
    assert!(
        scan_message(&bytes, bytes.len(), &limits)
            .expect("scan")
            .is_none(),
        "a clean end of input is a valid end of stream"
    );

    let (_, batches) = decode(&path);
    assert_eq!(batches.len(), 1);
}

#[test]
fn arrow_rs_writes_all_ones_validity_where_astrs_writes_none() {
    // Documents the one intentional encoding difference (see
    // `ipc::encode`): arrow-rs materialises a validity bitmap even for a
    // column without nulls, AstRS writes a zero-length buffer. Both decode to
    // the same array, which is what the golden comparison above proves; this
    // test pins the difference so it cannot change silently.
    let path = golden_dir().join("primitives_no_null.arrows");
    let bytes = std::fs::read(&path).expect("read");
    let limits = MessageLimits::default();
    let schema_message = scan_message(&bytes, 0, &limits)
        .expect("scan")
        .expect("schema");
    let batch_message = scan_message(&bytes, schema_message.next, &limits)
        .expect("scan")
        .expect("batch");
    let info = decode_message(&bytes[batch_message.metadata.clone()]).expect("decode");
    let buffers = info
        .header
        .vector(astrs_data::ipc::format::record_batch::BUFFERS, 16)
        .expect("buffers")
        .expect("present");
    let first = buffers.struct_pair(0).expect("validity entry");
    assert!(
        first.1 > 0,
        "arrow-rs writes a real validity bitmap for a null-free column"
    );

    let (schema, batches) = decode(&path);
    let ours = to_ipc_bytes_with(std::sync::Arc::new(schema), &batches, WriteOptions::new())
        .expect("write");
    let our_batch = scan_message(
        ours.as_slice(),
        scan_message(ours.as_slice(), 0, &limits)
            .expect("scan")
            .expect("schema")
            .next,
        &limits,
    )
    .expect("scan")
    .expect("batch");
    let our_info = decode_message(&ours.as_slice()[our_batch.metadata.clone()]).expect("decode");
    let our_buffers = our_info
        .header
        .vector(astrs_data::ipc::format::record_batch::BUFFERS, 16)
        .expect("buffers")
        .expect("present");
    assert_eq!(
        our_buffers.struct_pair(0).expect("validity entry").1,
        0,
        "AstRS elides the bitmap of a null-free column"
    );
}

#[test]
fn the_dora_payload_convention_is_recognised() {
    let (schema, batches) = decode(&golden_dir().join("dora_payload.arrows"));
    assert_eq!(schema.len(), 1);
    assert_eq!(
        schema.field(0).map(astrs_data::Field::name),
        Some(astrs_data::DATA_COLUMN)
    );
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_rows(), 1024);
    assert!(batches[0].payload_column().is_some());
}

#[test]
fn schema_metadata_round_trips_from_a_foreign_producer() {
    let (schema, _) = decode(&golden_dir().join("schema_metadata.arrows"));
    assert_eq!(
        schema.metadata_value("std/type"),
        Some("std/core/v1/Float32")
    );
    assert_eq!(
        schema.metadata_value("_schema_hash"),
        Some("0123456789abcdef")
    );
    assert_eq!(schema.metadata_value("empty"), Some(""));
}

#[test]
fn truncating_any_golden_stream_never_panics() {
    // Fuzz-lite: every prefix of every golden file must be rejected or decode
    // to a prefix of the batches, never panic.
    for (_, arrows, _) in golden_cases() {
        let bytes = std::fs::read(&arrows).expect("read");
        let step = (bytes.len() / 64).max(1);
        let mut cut = 0usize;
        while cut < bytes.len() {
            if let Ok(mut reader) = IpcStreamReader::from_slice(&bytes[..cut]) {
                let _ = reader.read_all();
            }
            cut += step;
        }
    }
}

#[test]
fn corrupting_a_byte_of_a_golden_stream_never_panics() {
    let bytes = std::fs::read(golden_dir().join("all_types.arrows")).expect("read");
    for index in (0..bytes.len()).step_by(7) {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut corrupted = bytes.clone();
            corrupted[index] ^= flip;
            if let Ok(mut reader) = IpcStreamReader::from_slice(&corrupted) {
                let _ = reader.read_all();
            }
        }
    }
}
