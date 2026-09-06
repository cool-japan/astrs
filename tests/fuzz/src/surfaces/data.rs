//! Deep-fuzz harness for `astrs-data`'s Arrow IPC reader (blueprint §6.1,
//! §15).
//!
//! Attack surface: [`decode_payload`] (the §6.1 one-batch payload
//! convention) and [`IpcStreamReader`] (the general multi-batch stream
//! reader it is built on). Both must turn arbitrary octets into a value or a
//! typed [`IpcError`], never a panic.
//!
//! One deliberate omission, not an oversight: `check` does **not** assert
//! any relationship between a decoded batch's row count and the input's
//! byte length. `Null` is in the closed P0 array set (blueprint §5.1) and a
//! `NullArray` column stores only a length, so a handful of flatbuffer bytes
//! can legitimately declare an enormous row count — asserting otherwise
//! would turn a correct decode into a false "finding".

use astrs_data::datatype::{DataType, Field, Schema};
use astrs_data::ipc::{
    IpcStreamReader, WriteOptions, decode_payload, encode_payload, to_ipc_bytes, to_ipc_bytes_with,
};
use astrs_data::prelude::*;
use std::sync::Arc;

use crate::support::mutate;
use crate::support::rng::Rng;

/// Per-iteration cap on generated input length — enough room for a batch
/// with a handful of columns, small enough that millions of nightly-lane
/// iterations stay fast.
pub const MAX_INPUT_LEN: usize = 32 * 1024;

/// Where the committed regression corpus for this surface lives.
pub const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/data");

/// A hard ceiling on how many batches [`check`] will pull from one stream.
///
/// Purely a harness runtime guard, not a correctness assertion: each batch a
/// well-formed stream yields costs at least a few metadata bytes, so this
/// cannot fire on a genuinely valid stream within [`MAX_INPUT_LEN`]. It
/// exists only so a hypothetical decoder bug cannot turn one fuzz case into
/// an unbounded loop.
const MAX_BATCHES_PER_STREAM: usize = 10_000;

/// A handful of single-`data`-column batches spanning the shapes worth
/// mutating: nulls, floats, a wide byte column, and a nested struct.
fn sample_batches() -> Vec<RecordBatch> {
    let int_with_nulls = Int64Array::from_opt_iter([Some(1), None, Some(3), None, Some(5)]);
    let floats = Float32Array::from_values([1.0, -2.5, f32::NAN, f32::INFINITY, 0.0]);
    let wide_bytes = UInt8Array::from_values((0..=255u16).map(|v| v as u8).collect::<Vec<_>>());
    let strings = StringArray::from_values(["", "a", "astrs", "fuzz seed string"]);
    let bools = BooleanArray::from_opt_iter([Some(true), Some(false), None]);

    let struct_fields = vec![
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
    ];
    let struct_columns = vec![
        Float32Array::from_values([1.0, 2.0]).into_array_ref(),
        Float32Array::from_values([3.0, 4.0]).into_array_ref(),
    ];

    let mut batches = vec![
        RecordBatch::from_payload(int_with_nulls.into_array_ref()),
        RecordBatch::from_payload(floats.into_array_ref()),
        RecordBatch::from_payload(wide_bytes.into_array_ref()),
        RecordBatch::from_payload(strings.into_array_ref()),
        RecordBatch::from_payload(bools.into_array_ref()),
        RecordBatch::from_payload(NullArray::new(7).into_array_ref()),
    ];
    if let Ok(strukt) = StructArray::try_new(struct_fields, struct_columns, None) {
        batches.push(RecordBatch::from_payload(strukt.into_array_ref()));
    }
    batches
}

/// Valid Arrow IPC streams from [`encode_payload`] and [`to_ipc_bytes`]:
/// single-batch payloads (what [`decode_payload`] expects), a two-batch
/// stream and a schema-only stream (both legal for [`IpcStreamReader`], both
/// deliberately rejected by [`decode_payload`]'s "exactly one batch" rule).
#[must_use]
pub fn seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    let batches = sample_batches();
    for batch in &batches {
        if let Ok(payload) = encode_payload(batch) {
            seeds.push(payload.to_vec());
        }
    }
    if let (Some(first), Some(second)) = (batches.first(), batches.get(1))
        && let Ok(stream) = to_ipc_bytes(&[first.clone(), second.clone()])
    {
        seeds.push(stream.to_vec());
    }
    if let Some(first) = batches.first()
        && let Ok(stream) = to_ipc_bytes_with(first.schema_ref(), &[], WriteOptions::new())
    {
        seeds.push(stream.to_vec());
    }
    let empty_schema = Arc::new(Schema::new(Vec::new()));
    if let Ok(stream) = to_ipc_bytes_with(empty_schema, &[], WriteOptions::new()) {
        seeds.push(stream.to_vec());
    }
    seeds
}

/// Unstructured bytes about a quarter of the time; a structure-aware
/// mutation of a randomly chosen seed otherwise.
#[must_use]
pub fn generate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    if rng.one_in(4) {
        return mutate::random_bytes(rng, MAX_INPUT_LEN);
    }
    match rng.pick(seeds) {
        Some(seed) => mutate::mutate(rng, seed, MAX_INPUT_LEN),
        None => mutate::random_bytes(rng, MAX_INPUT_LEN),
    }
}

/// The invariant: [`decode_payload`] and [`IpcStreamReader`] return a value
/// or a typed [`IpcError`] for whatever `bytes` are — never a panic.
pub fn check(bytes: &[u8]) {
    let _ = decode_payload(bytes);
    if let Ok(mut reader) = IpcStreamReader::from_slice(bytes) {
        let mut batches = 0usize;
        while let Ok(Some(_batch)) = reader.next_batch() {
            batches += 1;
            if batches >= MAX_BATCHES_PER_STREAM {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_single_batch_seed_actually_decodes() {
        let batches = sample_batches();
        assert!(!batches.is_empty());
        for batch in &batches {
            let payload = encode_payload(batch).expect("encode");
            assert_eq!(
                decode_payload(payload.as_slice()).expect("decode"),
                *batch,
                "a data seed did not round-trip through decode_payload"
            );
        }
    }

    #[test]
    fn seeds_is_non_empty_and_every_seed_is_readable_by_at_least_one_entry_point() {
        let seeds = seeds();
        assert!(!seeds.is_empty());
        for seed in &seeds {
            let single = decode_payload(seed).is_ok();
            let stream = IpcStreamReader::from_slice(seed).is_ok();
            assert!(
                single || stream,
                "a data seed was rejected by both decode_payload and IpcStreamReader: {} bytes",
                seed.len()
            );
        }
    }

    #[test]
    fn generate_never_exceeds_the_cap() {
        let seeds = seeds();
        let mut rng = Rng::new(456);
        for _ in 0..500 {
            assert!(generate(&mut rng, &seeds).len() <= MAX_INPUT_LEN);
        }
    }

    #[test]
    fn check_never_panics_on_a_handful_of_hand_picked_edge_cases() {
        for case in [Vec::new(), vec![0u8; 1], vec![0xFFu8; 8], vec![0xFFu8; 64]] {
            check(&case);
        }
    }
}
