//! Cross-checks `arrow-interop` against the same golden corpus
//! `tests/golden_arrow.rs` uses to prove the native Arrow IPC codec
//! byte-compatible with arrow-rs 59.2.0 (blueprint risk register §23 #1).
//!
//! # Why this is "both readers", given the workspace bans `arrow-ipc`
//!
//! The task this suite was written for asks to "decode one of the committed
//! golden IPC files with both readers and assert identical logical content
//! through the conversion layer." A literal second *IPC-stream* reader is not
//! available in-workspace: `arrow-ipc` (the arrow-rs crate that parses
//! `.arrows` bytes) is not among the four crates `arrow-interop` may depend
//! on (`astrs-data/Cargo.toml`; deny.toml's arrow ban entries have no
//! `wrappers` exception for it), and depending on it would mean shipping a
//! second FlatBuffers parser next to the hand-rolled one blueprint §6.1
//! explicitly asks for instead.
//!
//! What *is* available, and what this file does instead, is the stronger
//! form of the same claim: decode with astrs-data's own native reader
//! (`astrs_data::ipc::IpcStreamReader` — the "first reader"), then run the
//! result through `to_arrow_record_batch` *and back* through
//! `from_arrow_record_batch` (the "second reader": arrow-rs's own
//! `ArrayData`/`RecordBatch` machinery, reconstructing from what the first
//! reader produced). Both the once-decoded batch and the round-tripped one
//! are rendered with the exact independent grammar (`tests/common/render.rs`)
//! that `tests/golden_arrow.rs` already checks byte-for-byte against
//! arrow-rs's own reading of the same file — so a bug in either interop
//! direction shows up as a diff against ground truth arrow-rs itself
//! produced, not merely as "these two things I wrote agree with each other".
//!
//! Gated on the `arrow-interop` feature for the same reason
//! `tests/interop_roundtrip.rs` is — see that file's module docs.

#![cfg(feature = "arrow-interop")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/render.rs"]
mod render;

use std::path::{Path, PathBuf};

use astrs_data::interop::{from_arrow_record_batch, to_arrow_record_batch};
use astrs_data::ipc::IpcStreamReader;
use astrs_data::{RecordBatch, Schema};

/// The committed corpus directory (shared with `tests/golden_arrow.rs`).
fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/arrow")
}

/// Every `.arrows` file, sorted, with its `.expect` sibling — the same
/// listing `tests/golden_arrow.rs` builds, reimplemented here rather than
/// shared, because a `tests/*.rs` file is its own binary and cannot `mod` a
/// sibling one (only `tests/common/` is shareable).
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

/// Decodes one golden stream with astrs-data's own native reader.
fn decode(path: &Path) -> (Schema, Vec<RecordBatch>) {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let mut reader = IpcStreamReader::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("{}: schema: {err}", path.display()));
    let batches = reader
        .read_all()
        .unwrap_or_else(|err| panic!("{}: batches: {err}", path.display()));
    (reader.schema().as_ref().clone(), batches)
}

/// For every case in the golden corpus: decode natively, convert to arrow-rs
/// and back, and assert the round trip changed nothing a reader could
/// observe — checked the same way `tests/golden_arrow.rs` checks the native
/// decoder against arrow-rs ground truth: a byte-for-byte rendering
/// comparison against the committed `.expect` file, not merely
/// `RecordBatch::eq` (which would not catch, for example, a schema-metadata
/// key silently dropped in one direction).
#[test]
fn every_golden_case_survives_a_round_trip_through_arrow_rs() {
    let cases = golden_cases();
    assert!(
        cases.len() >= 28,
        "the committed corpus shrank to {} case(s)",
        cases.len()
    );

    for (name, arrows_path, expect_path) in &cases {
        let expected = std::fs::read_to_string(expect_path)
            .unwrap_or_else(|err| panic!("{name}: {}: {err}", expect_path.display()));

        let (schema, batches) = decode(arrows_path);
        let native_rendering = render::render_stream(&schema, &batches);
        assert_eq!(
            native_rendering,
            expected,
            "{name}: the native decoder itself disagrees with the golden \
             .expect file (tests/golden_arrow.rs should already have caught \
             this — checked again here so a failure below is unambiguously \
             this file's own conversion, not a corpus regression): {}",
            render::first_difference(&expected, &native_rendering)
        );

        // astrs -> arrow -> astrs, through the public interop surface.
        let mut round_tripped = Vec::with_capacity(batches.len());
        for (index, batch) in batches.iter().enumerate() {
            let arrow_batch = to_arrow_record_batch(batch)
                .unwrap_or_else(|e| panic!("{name}[{index}]: to_arrow_record_batch: {e}"));
            let back = from_arrow_record_batch(&arrow_batch)
                .unwrap_or_else(|e| panic!("{name}[{index}]: from_arrow_record_batch: {e}"));
            round_tripped.push(back);
        }

        let round_tripped_rendering = render::render_stream(&schema, &round_tripped);
        assert_eq!(
            round_tripped_rendering,
            expected,
            "{name}: round-tripping through arrow-rs changed what a reader \
             observes, relative to what arrow-rs itself produced for this \
             exact stream: {}",
            render::first_difference(&expected, &round_tripped_rendering)
        );
    }
}

/// The `all_types` case specifically (every P0 type in one batch, per the
/// corpus manifest) gets its own named test so a failure there is not lost
/// inside the loop above's aggregate — and additionally checks that the
/// *schema itself* (not just the rendered values) survives the round trip,
/// since `render_stream` intentionally does not print field-level type
/// tokens for every nested case the way the schema comparison below does.
#[test]
fn all_types_batch_keeps_its_schema_shape_through_arrow_rs() {
    let (name, arrows_path, _expect_path) = golden_cases()
        .into_iter()
        .find(|(name, _, _)| name == "all_types")
        .expect("the `all_types` case is part of the committed corpus");
    assert_eq!(name, "all_types");

    let (schema, batches) = decode(&arrows_path);
    assert_eq!(batches.len(), 1, "all_types is a single-batch case");
    let batch = &batches[0];

    let arrow_batch = to_arrow_record_batch(batch).unwrap();
    let back = from_arrow_record_batch(&arrow_batch).unwrap();

    assert_eq!(back.schema().as_ref(), &schema);
    assert_eq!(&back, batch);
    assert_eq!(back.num_columns(), schema.len());
    assert_eq!(back.num_rows(), batch.num_rows());
}
