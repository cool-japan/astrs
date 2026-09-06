//! Regenerates the boundary and hand-picked "interesting case" files under
//! `corpus/<surface>/` (blueprint §15).
//!
//! Not part of the default lane — deliberately `#[ignore]`d, because every
//! function here **writes to the repository**, which no ordinary `cargo
//! test` run may ever do as a side effect (see `astrs-fuzz`'s own crate
//! docs). Run explicitly, after reviewing the diff it produces:
//!
//! ```text
//! cargo test -p astrs-fuzz --test generate_corpus -- --ignored
//! ```
//!
//! Every case is checked (via `catch_unwind`, exactly as the real harness
//! checks a fuzz case) *before* it is written, so this generator can never
//! silently commit a crashing input — a case that panics here is a
//! genuine finding, reported as a normal test failure instead of a quiet
//! file write. Re-running is additive: nothing here deletes an existing
//! file, so a case added by hand (e.g. a minimized crash from the
//! task's own validation pass) survives a re-run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use astrs_fuzz::surfaces::{cdr, data, rtps, wire};

/// Writes `bytes` to `<dir>/<name>`, first checking that `check` does not
/// panic on them — see the module docs for why this is not optional.
fn commit(dir: &str, name: &str, bytes: &[u8], check: fn(&[u8])) {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| check(bytes)));
    assert!(
        outcome.is_ok(),
        "refusing to commit a crashing case ({dir}/{name}, {} bytes) -- fix the underlying bug \
         first, then commit the minimized reproduction instead",
        bytes.len()
    );
    std::fs::create_dir_all(dir).expect("create corpus dir");
    std::fs::write(Path::new(dir).join(name), bytes).expect("write corpus case");
}

/// Truncates `sample` at every offset in `offsets`, skipping any at or past
/// `sample.len()` (a boundary that does not exist for this particular
/// sample is simply not written).
fn truncations(sample: &[u8], offsets: &[(&str, usize)]) -> Vec<(String, Vec<u8>)> {
    offsets
        .iter()
        .filter(|(_, at)| *at < sample.len())
        .map(|(label, at)| (format!("truncated_{label}.bin"), sample[..*at].to_vec()))
        .collect()
}

#[test]
#[ignore = "writes to the repository -- run deliberately, see module docs"]
fn generate_wire_corpus() {
    let dir = wire::CORPUS_DIR;
    commit(dir, "empty.bin", &[], wire::check);

    let limits = astrs_wire::FrameLimits::new();
    let sample = astrs_wire::encode_frame(
        astrs_wire::FrameKind::Data,
        astrs_wire::FrameFlags::CRC,
        &[0xAB, 0xCD, 0xEF, 0x01],
        &limits,
    )
    .expect("encode a reference sample");
    // Field boundaries of the ten-octet header (magic:2, version:1,
    // flags:1, kind:2, len:4 -- astrs-wire's `frame::header` module),
    // then the payload and CRC trailer of the reference sample above.
    let boundaries: [(&str, usize); 10] = [
        ("mid_magic", 1),
        ("after_magic", 2),
        ("after_version", 3),
        ("after_flags", 4),
        ("mid_kind", 5),
        ("after_kind", 6),
        ("mid_len", 8),
        ("after_header", astrs_wire::HEADER_LEN),
        ("mid_payload", astrs_wire::HEADER_LEN + 2),
        ("before_crc", sample.len() - 4),
    ];
    for (name, bytes) in truncations(&sample, &boundaries) {
        commit(dir, &name, &bytes, wire::check);
    }

    let max_length = astrs_wire::encode_frame(
        astrs_wire::FrameKind::Data,
        astrs_wire::FrameFlags::CRC,
        &vec![0x5Au8; wire::MAX_INPUT_LEN - astrs_wire::HEADER_LEN - 4],
        &limits,
    )
    .expect("encode a max-length sample");
    commit(dir, "max_length.bin", &max_length, wire::check);

    // A CRC-bearing frame with the last trailer byte flipped.
    let mut crc_mismatch = sample.clone();
    if let Some(last) = crc_mismatch.last_mut() {
        *last ^= 0xFF;
    }
    commit(dir, "crc_mismatch.bin", &crc_mismatch, wire::check);

    // A header claiming a 4 GiB payload with nothing behind it -- the
    // "refused before allocation" case astrs-wire's own unit tests cover.
    let huge_header = astrs_wire::FrameHeader::new(
        astrs_wire::FrameKind::Data,
        astrs_wire::FrameFlags::EMPTY,
        u32::MAX,
    );
    commit(
        dir,
        "oversize_declared_length.bin",
        &huge_header.to_bytes(),
        wire::check,
    );

    // A compression-flagged frame: the payload is opaque at this layer, so
    // this is still a structurally valid frame.
    let compressed = astrs_wire::encode_frame(
        astrs_wire::FrameKind::Data,
        astrs_wire::FrameFlags::CRC.with_compression(astrs_wire::Compression::Zstd),
        b"opaque",
        &limits,
    )
    .expect("encode a compressed-flag sample");
    commit(dir, "compressed_payload_flag.bin", &compressed, wire::check);
}

#[test]
#[ignore = "writes to the repository -- run deliberately, see module docs"]
fn generate_data_corpus() {
    use astrs_data::ipc::{WriteOptions, decode_payload, encode_payload, to_ipc_bytes_with};
    use astrs_data::prelude::*;
    use std::sync::Arc;

    let dir = data::CORPUS_DIR;
    commit(dir, "empty.bin", &[], data::check);

    let batch = RecordBatch::from_payload(Int64Array::from_values([1, 2, 3]).into_array_ref());
    let payload = encode_payload(&batch).expect("encode a reference sample");
    assert!(
        decode_payload(payload.as_slice()).is_ok(),
        "reference sample must decode"
    );

    // Truncated right after the continuation marker + declared metadata
    // length -- the exact shape `astrs-data`'s own `ipc::error` module docs
    // use to demonstrate a truncation.
    commit(
        dir,
        "truncated_after_length_prefix.bin",
        &[0xFF, 0xFF, 0xFF, 0xFF, 0x10],
        data::check,
    );
    for at in [1usize, 4, 8, payload.len() / 2, payload.len() - 1] {
        if at < payload.len() {
            commit(
                dir,
                &format!("truncated_at_{at}.bin"),
                &payload.as_slice()[..at],
                data::check,
            );
        }
    }

    let max_length_column = UInt8Array::from_values(vec![0x5Au8; data::MAX_INPUT_LEN - 512]);
    let max_length_batch = RecordBatch::from_payload(max_length_column.into_array_ref());
    if let Ok(max_length) = encode_payload(&max_length_batch) {
        commit(dir, "max_length.bin", max_length.as_slice(), data::check);
    }

    // Two batches in one stream: legal for `IpcStreamReader`, deliberately
    // rejected by `decode_payload`'s "exactly one batch" rule.
    let schema = batch.schema_ref();
    if let Ok(two_batches) = astrs_data::ipc::to_ipc_bytes(&[batch.clone(), batch.clone()]) {
        commit(
            dir,
            "two_batch_stream.bin",
            two_batches.as_slice(),
            data::check,
        );
    }

    // A schema with no batches at all.
    if let Ok(schema_only) = to_ipc_bytes_with(Arc::clone(&schema), &[], WriteOptions::new()) {
        commit(
            dir,
            "schema_only_stream.bin",
            schema_only.as_slice(),
            data::check,
        );
    }

    // A `NullArray` column: a handful of flatbuffer bytes legitimately
    // declaring a large row count (blueprint's own P0 array set) -- kept as
    // a named regression anchor precisely because it looks alarming without
    // being wrong; see this module's own doc comment.
    let large_null_batch = RecordBatch::from_payload(NullArray::new(10_000_000).into_array_ref());
    if let Ok(large_null) = encode_payload(&large_null_batch) {
        commit(
            dir,
            "large_null_array.bin",
            large_null.as_slice(),
            data::check,
        );
    }
}

#[test]
#[ignore = "writes to the repository -- run deliberately, see module docs"]
fn generate_cdr_corpus() {
    let dir = cdr::CORPUS_DIR;
    commit(dir, "empty.bin", &[], cdr::check);

    // The exact case astrs-cdr's own
    // `a_hostile_sequence_length_costs_one_comparison` test derives: a
    // declared count of 0xffff_ffff eight-octet elements, four octets
    // actually present.
    let hostile_sequence: [u8; 12] = [
        0x00, 0x01, 0x00, 0x00, // CDR_LE
        0xff, 0xff, 0xff, 0xff, // count = 0xffff_ffff
        0x00, 0x00, 0x00, 0x00, // four octets of payload
    ];
    commit(
        dir,
        "hostile_sequence_length.bin",
        &hostile_sequence,
        cdr::check,
    );

    // astrs-cdr's own `a_hostile_parameter_list_cannot_grow_without_bound`
    // shape: 64 minimal parameter entries under a `PL_CDR` header.
    let mut hostile_parameter_list = vec![0x00, 0x03, 0x00, 0x00];
    for _ in 0..64 {
        hostile_parameter_list.extend_from_slice(&[0x70, 0x00, 0x00, 0x00]);
    }
    hostile_parameter_list.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
    commit(
        dir,
        "hostile_parameter_list.bin",
        &hostile_parameter_list,
        cdr::check,
    );

    // An encapsulation identifier none of the ten XTypes Table 47 entries
    // define.
    commit(
        dir,
        "unknown_encapsulation.bin",
        &[0xAB, 0xCD, 0x00, 0x00],
        cdr::check,
    );

    // A max-length-ish octet sequence under the crate's ROS2 encoding.
    let max_length = vec![0xC3u8; cdr::MAX_INPUT_LEN];
    commit(dir, "max_length.bin", &max_length, cdr::check);

    // A truncation series off one valid `Mixed` encoding under CDR_LE: the
    // 4-octet encapsulation header boundary, then two cuts partway into the
    // struct body. Not a full per-field walk -- `Mixed` mixes 1/2/4/8-octet
    // alignment classes, so hand-stating each field's exact offset here
    // would be easy to get subtly wrong; these three offsets land inside
    // three different members regardless of the exact padding either XCDR
    // revision inserts.
    use astrs_cdr::{EncapsulationKind, Encoding, to_vec};
    let mixed_sample = cdr::Mixed {
        flag: true,
        tag: 7,
        small: -1,
        medium: 42,
        large: 99,
        name: "sample".to_owned(),
        samples: vec![1.0, 2.5],
    };
    let mixed_bytes = to_vec(&mixed_sample, Encoding::new(EncapsulationKind::CdrLe))
        .expect("encode a reference sample");
    let body_boundaries: [(&str, usize); 3] = [("at_4", 4), ("at_8", 8), ("at_12", 12)];
    for (name, bytes) in truncations(&mixed_bytes, &body_boundaries) {
        commit(dir, &name, &bytes, cdr::check);
    }
}

#[test]
#[ignore = "writes to the repository -- run deliberately, see module docs"]
fn generate_rtps_corpus() {
    use astrs_rtps::messages::{Header, MAX_SUBMESSAGES, Message, Pad};
    use astrs_rtps::structure::GuidPrefix;

    let dir = rtps::CORPUS_DIR;
    commit(dir, "empty.bin", &[], rtps::check);

    let prefix = GuidPrefix::new([0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    let header_bytes = Header::new(prefix).to_bytes();
    commit(dir, "bare_header.bin", &header_bytes, rtps::check);

    // A PAD declaring a two-octet body, so the next header would start at
    // an offset that is not four-octet aligned -- the exact shape
    // astrs-rtps's own `a_misaligned_next_header_stops_the_walk` test uses.
    let mut misaligned = header_bytes.to_vec();
    misaligned.extend_from_slice(&[0x01, 0x01, 0x02, 0x00, 0xaa, 0xbb]);
    misaligned.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);
    commit(dir, "misaligned_submessage.bin", &misaligned, rtps::check);

    // A submessage declaring 255 octets of body with only 8 present.
    let mut overrun = header_bytes.to_vec();
    overrun.extend_from_slice(&[0x07, 0x01, 0xff, 0x00]);
    overrun.extend_from_slice(&[0u8; 8]);
    commit(dir, "submessage_overrun.bin", &overrun, rtps::check);

    // A trailing partial submessage header -- three of the required four
    // octets.
    let mut partial_header = header_bytes.to_vec();
    partial_header.extend_from_slice(&[0x01, 0x01, 0x00]);
    commit(
        dir,
        "truncated_submessage_header.bin",
        &partial_header,
        rtps::check,
    );

    // A truncation series off one full valid message (the twenty-octet RTPS
    // header this crate's own `messages::header` module documents --
    // protocol:4, version:2, vendorId:2, guidPrefix:12 -- followed by one
    // DATA submessage), plus one cut partway into the submessage that
    // follows the header.
    use astrs_rtps::messages::{Data, DataPayload, SerializedPayload};
    use astrs_rtps::structure::{EntityId, EntityKind, SequenceNumber};
    let full_message = Message::from_participant(prefix)
        .with(Data::new(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::new(1),
            DataPayload::Data(
                SerializedPayload::from_cdr(&7u32).expect("encode a u32 reference payload"),
            ),
        ))
        .encode()
        .expect("encode a reference sample");
    let header_boundaries: [(&str, usize); 6] = [
        ("mid_protocol", 2),
        ("after_protocol", 4),
        ("after_version", 6),
        ("after_vendor_id", 8),
        ("mid_guid_prefix", 14),
        ("after_header", header_bytes.len()),
    ];
    for (name, bytes) in truncations(&full_message, &header_boundaries) {
        commit(dir, &name, &bytes, rtps::check);
    }
    if header_bytes.len() + 6 < full_message.len() {
        commit(
            dir,
            "truncated_mid_submessage.bin",
            &full_message[..header_bytes.len() + 6],
            rtps::check,
        );
    }

    // MAX_SUBMESSAGES + 1 empty PADs: the ceiling astrs-rtps's own
    // `a_crafted_datagram_cannot_produce_an_unbounded_submessage_vector`
    // test crosses on purpose.
    let mut message = Message::from_participant(prefix);
    for _ in 0..=MAX_SUBMESSAGES {
        message.push(Pad::empty());
    }
    let over_the_submessage_ceiling = message.encode().expect("PADs always encode");
    commit(
        dir,
        "over_the_submessage_ceiling.bin",
        &over_the_submessage_ceiling,
        rtps::check,
    );
}
