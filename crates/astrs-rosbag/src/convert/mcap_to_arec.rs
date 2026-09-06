//! `.mcap → .arec` (blueprint §10.6, §14): `astrs bag convert x.mcap y.arec`.
//!
//! `.mcap` never had a mechanism to carry an [`astrs_wire::DataflowId`] or
//! an [`astrs_recording`] sidecar — real mcap files were never produced by
//! this crate's own `arec_to_db3` — so unlike
//! [`crate::convert::db3_to_arec::db3_to_arec`], there is nothing to
//! *recover*: [`mcap_to_arec`] always mints a fresh
//! [`DataflowId::generate`] and assigns fresh node ids via
//! [`assign_node_ids`], the same fallback `db3_to_arec` uses for a bag
//! that carries no sidecar either.
//!
//! # Two passes, uniform regardless of indexing
//!
//! [`crate::mcap::Reader`] populates its schema/channel tables (and
//! [`crate::mcap::Reader::is_indexed`]'s answer) directly from the
//! summary section on [`crate::mcap::Reader::open`] *when the file has
//! one* — but for an unindexed file they only fill in as a side effect of
//! walking every message. Rather than branch on which case this file is,
//! [`mcap_to_arec`] always drains [`crate::mcap::Reader::iter_messages`]
//! once up front (a no-op-shaped pass for an indexed file: the tables are
//! already complete, and the pass also finds the earliest `log_time` for
//! the output header's HLC epoch) before a second pass streams the actual
//! conversion — the same two-pass shape
//! [`crate::convert::arec_to_db3::arec_to_db3`] uses, for the same reason
//! (a topic must be registered before any message can reference it).

use std::collections::BTreeMap;
use std::path::Path;

use astrs_recording::{Writer as ArecWriter, WriterOptions};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, DataflowId, Metadata, NodeId};

use crate::convert::ConversionReport;
use crate::error::RosbagError;
use crate::mcap::Reader as McapReader;
use crate::topic::parse_qos_profiles_yaml;
use crate::{
    OUTPUT_NAME, RosbagTopicManifest, SIDECAR_FORMAT_VERSION, SidecarTopic, TopicRecord,
    assign_node_ids,
};

/// Converts the `.mcap` at `input` to a fresh `.arec` at `output`.
///
/// Every channel's `serialization_format` is `Channel.message_encoding`
/// verbatim (mcap already names it directly — no CDR-header peeking
/// needed, unlike [`crate::convert::arec_to_db3::arec_to_db3`]'s
/// synthesized topics), its `type` is its schema's `name` when the
/// channel has one, and its QoS profiles are parsed from
/// `Channel.metadata["offered_qos_profiles"]` (the mcap `"ros2"` profile's
/// own convention) when present. `type_description_hash` is always empty:
/// mcap has no directly corresponding field.
///
/// # Errors
///
/// [`RosbagError::BadMagic`]/[`RosbagError::Truncated`]/
/// [`RosbagError::Malformed`]/[`RosbagError::ChunkTooLarge`]/
/// [`RosbagError::UnknownCompression`]/[`RosbagError::Codec`]/
/// [`RosbagError::ChunkSizeMismatch`]/[`RosbagError::NestedChunk`] if
/// `input` cannot be opened or read, or [`RosbagError::Recording`] if
/// `output` cannot be written.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::convert::mcap_to_arec;
///
/// fn record(opcode: u8, body: &[u8]) -> Vec<u8> {
///     let mut out = vec![opcode];
///     out.extend((body.len() as u64).to_le_bytes());
///     out.extend_from_slice(body);
///     out
/// }
/// fn prefixed(s: &str) -> Vec<u8> {
///     let mut out = (s.len() as u32).to_le_bytes().to_vec();
///     out.extend_from_slice(s.as_bytes());
///     out
/// }
///
/// const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
/// let mut bytes = MAGIC.to_vec();
/// bytes.extend(record(0x01, &[0u32.to_le_bytes(), 0u32.to_le_bytes()].concat())); // Header
/// let mut channel_body = 1u16.to_le_bytes().to_vec();
/// channel_body.extend(0u16.to_le_bytes()); // schema_id = 0
/// channel_body.extend(prefixed("/scan"));
/// channel_body.extend(prefixed("cdr"));
/// channel_body.extend(0u32.to_le_bytes()); // empty metadata map
/// bytes.extend(record(0x04, &channel_body)); // Channel
/// let mut message_body = 1u16.to_le_bytes().to_vec();
/// message_body.extend(0u32.to_le_bytes()); // sequence
/// message_body.extend(1_000u64.to_le_bytes()); // log_time
/// message_body.extend(1_000u64.to_le_bytes()); // publish_time
/// message_body.extend_from_slice(&[1, 2, 3]);
/// bytes.extend(record(0x05, &message_body)); // Message
/// let mut footer_body = 0u64.to_le_bytes().to_vec();
/// footer_body.extend(0u64.to_le_bytes());
/// footer_body.extend(0u32.to_le_bytes());
/// bytes.extend(record(0x02, &footer_body)); // Footer, unindexed
/// bytes.extend(MAGIC);
///
/// let dir = std::env::temp_dir().join(format!("astrs-rosbag-mcap-to-arec-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let input = dir.join("session.mcap");
/// let output = dir.join("session.arec");
/// std::fs::write(&input, &bytes)?;
///
/// let report = mcap_to_arec(&input, &output)?;
/// assert_eq!(report.topics, 1);
/// assert_eq!(report.messages, 1);
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn mcap_to_arec(input: &Path, output: &Path) -> Result<ConversionReport, RosbagError> {
    let mut reader = McapReader::open(input)?;

    // Pass 1: ensure `channels()`/`schemas()` are complete (a no-op walk
    // for an already-indexed file) and find the earliest `log_time` for
    // the output header's HLC epoch.
    let mut min_log_time: Option<u64> = None;
    for message in reader.iter_messages() {
        let message = message?;
        min_log_time = Some(min_log_time.map_or(message.message.log_time, |min| {
            min.min(message.message.log_time)
        }));
    }

    let channel_ids: Vec<u16> = reader.channels().keys().copied().collect();
    let topic_names: Vec<&str> = channel_ids
        .iter()
        .map(|id| reader.channels()[id].topic.as_str())
        .collect();
    let mut warnings = Vec::new();
    let fresh_ids = assign_node_ids(topic_names.into_iter(), &mut warnings);

    let mut topic_ports: BTreeMap<u16, (NodeId, DataId, TopicRecord)> = BTreeMap::new();
    for (index, channel_id) in channel_ids.iter().enumerate() {
        let channel = &reader.channels()[channel_id];
        let type_name = if channel.schema_id == 0 {
            String::new()
        } else {
            reader
                .schemas()
                .get(&channel.schema_id)
                .map(|schema| schema.name.clone())
                .unwrap_or_default()
        };
        let offered_qos_profiles = channel
            .metadata
            .get("offered_qos_profiles")
            .and_then(|text| parse_qos_profiles_yaml(text.as_str()))
            .unwrap_or_default();
        let record = TopicRecord {
            topic: channel.topic.clone(),
            r#type: type_name,
            serialization_format: channel.message_encoding.clone(),
            offered_qos_profiles,
            type_description_hash: String::new(),
        };
        topic_ports.insert(
            *channel_id,
            (fresh_ids[index].clone(), DataId::new(OUTPUT_NAME)?, record),
        );
    }

    let dataflow = DataflowId::generate();
    let hlc_epoch = min_log_time.map_or(HlcTimestamp::EPOCH, |ns| HlcTimestamp::new(ns, 0));

    // mcap carries no field corresponding to rosbag2's `ros_distro`
    // (`Header.profile` is `"ros1"`/`"ros2"`/empty, a wire-format family,
    // not a distro) — left empty rather than guessed from it.
    let output_sidecar = RosbagTopicManifest {
        format_version: SIDECAR_FORMAT_VERSION,
        ros_distro: String::new(),
        dataflow: Some(dataflow.to_string()),
        hlc_epoch: Some(hlc_epoch.to_string()),
        topics: topic_ports
            .values()
            .map(|(node, output, record)| SidecarTopic {
                node: node.as_str().to_owned(),
                output: output.as_str().to_owned(),
                record: record.clone(),
            })
            .collect(),
    };
    let manifest_yaml = output_sidecar.to_manifest_yaml().map_err(|source| {
        RosbagError::Internal(format!(
            "the astrs-rosbag topic-manifest sidecar did not serialize: {source}"
        ))
    })?;

    let options = WriterOptions::new(dataflow, hlc_epoch).with_manifest_yaml(manifest_yaml);
    let mut writer = ArecWriter::create(output, options)?;

    // Pass 2: the real conversion.
    let mut messages = 0u64;
    for message in reader.iter_messages() {
        let message = message?;
        let (node, data_id, _) =
            topic_ports
                .get(&message.channel.id)
                .cloned()
                .ok_or_else(|| {
                    RosbagError::Internal(format!(
                        "message references channel id {} with no registered topic",
                        message.channel.id
                    ))
                })?;
        writer.append_parts(
            node,
            data_id,
            Metadata::new(HlcTimestamp::new(message.message.log_time, 0)),
            message.message.data,
        )?;
        messages += 1;
    }
    writer.finish()?;

    warnings.extend(reader.warnings().iter().cloned());

    Ok(ConversionReport {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        direction: "mcap -> arec",
        topics: topic_ports.len(),
        messages,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_recording::Reader as ArecReader;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "astrs-rosbag-mcap-to-arec-test-{}-{}-{label}.mcap",
            std::process::id(),
            uniq()
        ))
    }

    const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];

    fn record(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![opcode];
        out.extend((payload.len() as u64).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn prefixed_string(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out
    }

    fn prefixed_map(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut inner = Vec::new();
        for (k, v) in entries {
            inner.extend(prefixed_string(k));
            inner.extend(prefixed_string(v));
        }
        let mut out = (inner.len() as u32).to_le_bytes().to_vec();
        out.extend(inner);
        out
    }

    /// A minimal, unindexed, hand-built `.mcap`: one schema, one channel,
    /// two messages — built directly against the specification (astrs.md
    /// §18 forbids C++-derived fixtures in-repo), matching
    /// `mcap::reader::tests::Builder`'s own approach.
    fn minimal_mcap() -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        let header_body = [prefixed_string("ros2"), prefixed_string("astrs-test")].concat();
        bytes.extend(record(0x01, &header_body));

        let mut schema_body = 7u16.to_le_bytes().to_vec();
        schema_body.extend(prefixed_string("sensor_msgs/msg/LaserScan"));
        schema_body.extend(prefixed_string("ros2msg"));
        schema_body.extend(0u32.to_le_bytes());
        bytes.extend(record(0x03, &schema_body));

        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(7u16.to_le_bytes());
        channel_body.extend(prefixed_string("/scan"));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[(
            "offered_qos_profiles",
            "- history: keep_last\n",
        )]));
        bytes.extend(record(0x04, &channel_body));

        for (log_time, payload) in [(200u64, [1u8, 2, 3]), (100u64, [4u8, 5, 6])] {
            let mut message_body = 1u16.to_le_bytes().to_vec();
            message_body.extend(0u32.to_le_bytes());
            message_body.extend(log_time.to_le_bytes());
            message_body.extend(log_time.to_le_bytes());
            message_body.extend_from_slice(&payload);
            bytes.extend(record(0x05, &message_body));
        }

        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);
        bytes
    }

    #[test]
    fn converts_channels_and_messages_assigning_fresh_ids() {
        let input = temp_path("basic");
        std::fs::write(&input, minimal_mcap()).unwrap();
        let output = input.with_extension("arec");

        let report = mcap_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 1);
        assert_eq!(report.messages, 2);

        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        assert_ne!(reader.header().dataflow, DataflowId::NIL);
        // Earliest `log_time` (100) becomes the header's HLC epoch.
        assert_eq!(reader.header().hlc_epoch, HlcTimestamp::new(100, 0));

        let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].node.as_str(), "_scan");
        assert_eq!(entries[0].output.as_str(), OUTPUT_NAME);
        // HLC order: the 100-timestamped message first.
        assert_eq!(entries[0].payload, vec![4, 5, 6]);
        assert_eq!(entries[1].payload, vec![1, 2, 3]);

        let sidecar =
            RosbagTopicManifest::from_manifest_yaml(&reader.header().manifest_yaml).unwrap();
        assert_eq!(sidecar.topics.len(), 1);
        assert_eq!(sidecar.topics[0].record.topic, "/scan");
        assert_eq!(sidecar.topics[0].record.r#type, "sensor_msgs/msg/LaserScan");
        assert_eq!(sidecar.topics[0].record.serialization_format, "cdr");
        assert_eq!(sidecar.ros_distro, "");
    }

    #[test]
    fn a_channel_with_no_schema_gets_an_empty_type() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[prefixed_string(""), prefixed_string("")].concat(),
        ));
        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes()); // schema_id = 0: no schema
        channel_body.extend(prefixed_string("/topic"));
        channel_body.extend(prefixed_string("json"));
        channel_body.extend(prefixed_map(&[]));
        bytes.extend(record(0x04, &channel_body));
        let mut message_body = 1u16.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend_from_slice(b"{}");
        bytes.extend(record(0x05, &message_body));
        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let input = temp_path("no-schema");
        std::fs::write(&input, bytes).unwrap();
        let output = input.with_extension("arec");
        mcap_to_arec(&input, &output).unwrap();

        let (reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let sidecar =
            RosbagTopicManifest::from_manifest_yaml(&reader.header().manifest_yaml).unwrap();
        assert_eq!(sidecar.topics[0].record.r#type, "");
        assert_eq!(sidecar.topics[0].record.serialization_format, "json");
    }

    #[test]
    fn an_mcap_with_no_messages_falls_back_to_the_epoch() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[prefixed_string(""), prefixed_string("")].concat(),
        ));
        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let input = temp_path("empty");
        std::fs::write(&input, bytes).unwrap();
        let output = input.with_extension("arec");
        let report = mcap_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 0);
        assert_eq!(report.messages, 0);

        let (reader, _) = ArecReader::open_or_recover(&output).unwrap();
        assert_eq!(reader.header().hlc_epoch, HlcTimestamp::EPOCH);
    }

    #[test]
    fn a_missing_input_is_a_typed_error_not_a_panic() {
        let input = temp_path("does-not-exist");
        let output = input.with_extension("arec");
        let err = mcap_to_arec(&input, &output).unwrap_err();
        assert!(matches!(err, RosbagError::Io { .. }), "{err:?}");
    }

    #[test]
    fn a_file_with_the_wrong_magic_is_a_typed_bad_magic_error() {
        let input = temp_path("wrong-magic");
        std::fs::write(&input, b"definitely not an mcap file, sixteen+ bytes long").unwrap();
        let output = input.with_extension("arec");
        let err = mcap_to_arec(&input, &output).unwrap_err();
        assert!(matches!(err, RosbagError::BadMagic { .. }), "{err:?}");
    }

    #[test]
    fn a_metadata_record_in_the_data_section_does_not_disturb_the_conversion() {
        // `Metadata` (op=0x0c) carries no `Message` content and is not a
        // `Channel`/`Schema` either — `mcap::reader` already proves it
        // skips such records during a plain read; this proves
        // `mcap_to_arec`'s own two-pass walk (which drains the *same*
        // iterator twice) tolerates one sitting between a channel and its
        // message without affecting the topic or message count either
        // pass.
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[prefixed_string(""), prefixed_string("")].concat(),
        ));
        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes());
        channel_body.extend(prefixed_string("/topic"));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[]));
        bytes.extend(record(0x04, &channel_body));

        let mut metadata_body = prefixed_string("calibration");
        metadata_body.extend(prefixed_map(&[("board", "rev-3")]));
        bytes.extend(record(0x0c, &metadata_body));

        let mut message_body = 1u16.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend_from_slice(&[1, 2]);
        bytes.extend(record(0x05, &message_body));

        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let input = temp_path("with-metadata-record");
        std::fs::write(&input, bytes).unwrap();
        let output = input.with_extension("arec");
        let report = mcap_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 1);
        assert_eq!(report.messages, 1);
    }

    /// Builds a Chunk record (op=0x06) wrapping `inner` (already-assembled
    /// raw records), compressed with `compression` (`""`, `"lz4"`, or
    /// `"zstd"`) — the same shape `mcap::reader::tests::Builder::chunk`
    /// uses, reproduced locally since this module's own fixtures are
    /// self-contained (astrs.md §18: no shared C/C++-derived tooling, and
    /// no cross-module test-only dependency either).
    fn chunk_record(compression: &str, inner: &[u8]) -> Vec<u8> {
        let on_disk = match compression {
            "" => inner.to_vec(),
            "lz4" => oxiarc_lz4::compress(inner).unwrap(),
            "zstd" => oxiarc_zstd::compress(inner).unwrap(),
            other => panic!("test helper does not know compression {other}"),
        };
        let mut body = 0u64.to_le_bytes().to_vec(); // message_start_time
        body.extend(0u64.to_le_bytes()); // message_end_time
        body.extend((inner.len() as u64).to_le_bytes()); // uncompressed_size
        body.extend(0u32.to_le_bytes()); // uncompressed_crc (unchecked)
        body.extend(prefixed_string(compression));
        body.extend((on_disk.len() as u64).to_le_bytes());
        body.extend_from_slice(&on_disk);
        record(0x06, &body)
    }

    /// One `Channel` + one `Message` record, assembled as raw bytes ready
    /// to sit inside a chunk's `records` field.
    fn chunk_inner(channel_id: u16, topic: &str, log_time: u64, payload: &[u8]) -> Vec<u8> {
        let mut channel_body = channel_id.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes()); // schema_id = 0
        channel_body.extend(prefixed_string(topic));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[]));
        let mut inner = record(0x04, &channel_body);

        let mut message_body = channel_id.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(log_time.to_le_bytes());
        message_body.extend(log_time.to_le_bytes());
        message_body.extend_from_slice(payload);
        inner.extend(record(0x05, &message_body));
        inner
    }

    #[test]
    fn an_lz4_compressed_chunk_converts_through_the_full_pipeline() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[prefixed_string("ros2"), prefixed_string("astrs-test")].concat(),
        ));
        let inner = chunk_inner(1, "/chunked", 500, &[9, 9, 9]);
        bytes.extend(chunk_record("lz4", &inner));
        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let input = temp_path("lz4-chunk");
        std::fs::write(&input, bytes).unwrap();
        let output = input.with_extension("arec");
        let report = mcap_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 1);
        assert_eq!(report.messages, 1);

        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        assert_eq!(entries[0].payload, vec![9, 9, 9]);
        assert_eq!(entries[0].node.as_str(), "_chunked");
        assert_eq!(entries[0].hlc(), HlcTimestamp::new(500, 0));
    }

    #[test]
    fn a_zstd_compressed_chunk_with_multiple_channels_converts_every_topic() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[prefixed_string("ros2"), prefixed_string("astrs-test")].concat(),
        ));
        let mut inner = chunk_inner(1, "/a", 200, &[1]);
        inner.extend(chunk_inner(2, "/b", 100, &[2]));
        bytes.extend(chunk_record("zstd", &inner));
        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let input = temp_path("zstd-chunk-multi");
        std::fs::write(&input, bytes).unwrap();
        let output = input.with_extension("arec");
        let report = mcap_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 2);
        assert_eq!(report.messages, 2);

        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        // The earlier-timestamped message (`/b`, log_time 100) becomes the
        // header's HLC epoch and, in HLC order, is delivered first.
        assert_eq!(reader.header().hlc_epoch, HlcTimestamp::new(100, 0));
        let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        assert_eq!(entries[0].node.as_str(), "_b");
        assert_eq!(entries[1].node.as_str(), "_a");
    }

    #[test]
    fn a_well_formed_qos_profile_in_channel_metadata_parses_end_to_end() {
        // `minimal_mcap`'s own embedded QoS text is deliberately partial
        // (see `topic::tests::parse_qos_profiles_yaml_degrades_the_exact_
        // string_mcaps_own_channel_metadata_test_uses`) — this is the
        // complementary case: a *complete* `QosProfile` sequence, proving
        // the happy path also works through the full conversion, not just
        // at the shared parser's own unit-test level.
        let qos_yaml = astrs_yaml::to_string(&vec![crate::QosProfile::reliable_default()]).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[prefixed_string("ros2"), prefixed_string("astrs-test")].concat(),
        ));
        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes());
        channel_body.extend(prefixed_string("/scan"));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[("offered_qos_profiles", &qos_yaml)]));
        bytes.extend(record(0x04, &channel_body));
        let mut message_body = 1u16.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend_from_slice(&[1]);
        bytes.extend(record(0x05, &message_body));
        let footer_body = [
            0u64.to_le_bytes().to_vec(),
            0u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let input = temp_path("full-qos");
        std::fs::write(&input, bytes).unwrap();
        let output = input.with_extension("arec");
        mcap_to_arec(&input, &output).unwrap();

        let (reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let sidecar =
            RosbagTopicManifest::from_manifest_yaml(&reader.header().manifest_yaml).unwrap();
        assert_eq!(
            sidecar.topics[0].record.offered_qos_profiles,
            vec![crate::QosProfile::reliable_default()]
        );
    }

    proptest::proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn mcap_to_arec_preserves_every_channels_messages(
            // Distinct names only: two channels sharing one topic name
            // would make `assign_node_ids` disambiguate the second (its
            // own, separately-tested behavior — see
            // `topic::tests::assign_node_ids_disambiguates_a_three_way_collision`),
            // which this property's node-id-to-topic-name reversal below
            // does not need to also model.
            channels in proptest::collection::hash_set("[a-zA-Z][a-zA-Z0-9]{0,7}", 1..4)
                .prop_map(|set| set.into_iter().collect::<Vec<_>>()),
            messages in proptest::collection::vec(
                (0usize..4, 0u64..1_000_000_000, proptest::collection::vec(any::<u8>(), 0..32)),
                0..8,
            ),
        ) {
            let mut bytes = MAGIC.to_vec();
            bytes.extend(record(
                0x01,
                &[prefixed_string(""), prefixed_string("")].concat(),
            ));
            for (id, name) in channels.iter().enumerate() {
                let mut channel_body = (id as u16 + 1).to_le_bytes().to_vec();
                channel_body.extend(0u16.to_le_bytes());
                channel_body.extend(prefixed_string(&format!("/{name}")));
                channel_body.extend(prefixed_string("cdr"));
                channel_body.extend(prefixed_map(&[]));
                bytes.extend(record(0x04, &channel_body));
            }
            let mut expected: BTreeMap<String, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
            for (channel_index, log_time, payload) in &messages {
                let index = channel_index % channels.len().max(1);
                if channels.is_empty() {
                    continue;
                }
                let channel_id = index as u16 + 1;
                let mut message_body = channel_id.to_le_bytes().to_vec();
                message_body.extend(0u32.to_le_bytes());
                message_body.extend(log_time.to_le_bytes());
                message_body.extend(log_time.to_le_bytes());
                message_body.extend_from_slice(payload);
                bytes.extend(record(0x05, &message_body));
                expected
                    .entry(format!("/{}", channels[index]))
                    .or_default()
                    .push((*log_time, payload.clone()));
            }
            let footer_body = [
                0u64.to_le_bytes().to_vec(),
                0u64.to_le_bytes().to_vec(),
                0u32.to_le_bytes().to_vec(),
            ]
            .concat();
            bytes.extend(record(0x02, &footer_body));
            bytes.extend(MAGIC);

            let input = temp_path("proptest");
            std::fs::write(&input, bytes).unwrap();
            let output = input.with_extension("arec");
            mcap_to_arec(&input, &output).unwrap();

            let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
            let mut actual: BTreeMap<String, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
            for entry in reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap() {
                actual
                    .entry(format!("/{}", &entry.node.as_str()[1..]))
                    .or_default()
                    .push((entry.hlc().physical_ns(), entry.payload));
            }
            for values in expected.values_mut().chain(actual.values_mut()) {
                values.sort();
            }
            prop_assert_eq!(actual, expected);
        }
    }

    #[test]
    fn an_indexed_mcap_converts_identically_to_an_unindexed_one() {
        // The module docs claim the two-pass design behaves uniformly
        // regardless of `Reader::is_indexed` — every other test in this
        // file builds an *unindexed* file (`Footer.summary_start == 0`);
        // this one builds the summary section too, so both branches of
        // that claim are actually exercised, not just the unindexed one.
        let mut bytes = MAGIC.to_vec();
        bytes.extend(record(
            0x01,
            &[prefixed_string("ros2"), prefixed_string("astrs-test")].concat(),
        ));

        let mut schema_body = 9u16.to_le_bytes().to_vec();
        schema_body.extend(prefixed_string("std_msgs/msg/String"));
        schema_body.extend(prefixed_string("ros2msg"));
        schema_body.extend(0u32.to_le_bytes());
        let data_schema = record(0x03, &schema_body);
        bytes.extend(&data_schema);

        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(9u16.to_le_bytes());
        channel_body.extend(prefixed_string("/indexed"));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[]));
        let data_channel = record(0x04, &channel_body);
        bytes.extend(&data_channel);

        let mut message_body = 1u16.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(300u64.to_le_bytes());
        message_body.extend(300u64.to_le_bytes());
        message_body.extend_from_slice(&[4, 5, 6]);
        bytes.extend(record(0x05, &message_body));

        // The summary section: the same Schema/Channel records again
        // (the spec permits this — a reader must not double-count them),
        // plus a `Statistics` record, so `Reader::open`'s fast path finds
        // and uses a real summary rather than treating this as unindexed.
        let summary_start = bytes.len() as u64;
        bytes.extend(&data_schema);
        bytes.extend(&data_channel);
        let mut stats_body = 1u64.to_le_bytes().to_vec(); // message_count
        stats_body.extend(1u16.to_le_bytes()); // schema_count
        stats_body.extend(1u32.to_le_bytes()); // channel_count
        stats_body.extend(0u32.to_le_bytes()); // attachment_count
        stats_body.extend(0u32.to_le_bytes()); // metadata_count
        stats_body.extend(0u32.to_le_bytes()); // chunk_count
        stats_body.extend(300u64.to_le_bytes()); // message_start_time
        stats_body.extend(300u64.to_le_bytes()); // message_end_time
        stats_body.extend(0u32.to_le_bytes()); // empty channel_message_counts map
        bytes.extend(record(0x0b, &stats_body));

        let mut footer_body = summary_start.to_le_bytes().to_vec();
        footer_body.extend(0u64.to_le_bytes()); // summary_offset_start: none
        footer_body.extend(0u32.to_le_bytes());
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let input = temp_path("indexed");
        std::fs::write(&input, bytes).unwrap();
        let output = input.with_extension("arec");

        // Confirm the fixture is genuinely indexed before trusting what
        // the conversion did with it.
        let sanity = McapReader::open(&input).unwrap();
        assert!(sanity.is_indexed());
        drop(sanity);

        let report = mcap_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 1);
        assert_eq!(report.messages, 1);

        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        assert_eq!(entries[0].node.as_str(), "_indexed");
        assert_eq!(entries[0].payload, vec![4, 5, 6]);
        let sidecar =
            RosbagTopicManifest::from_manifest_yaml(&reader.header().manifest_yaml).unwrap();
        assert_eq!(sidecar.topics[0].record.r#type, "std_msgs/msg/String");
    }
}
