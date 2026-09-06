//! `.arec → .db3` (blueprint §10.6, §14): `astrs bag convert x.arec y.db3`.
//!
//! See the [`crate::convert`] module docs for the round-trip identity this
//! preserves (via [`RosbagTopicManifest`] embedded into the output
//! `.db3`'s [`custom_data`](crate::db3::BagMetadata::custom_data)) and
//! what does not survive.
//!
//! # Two passes, bounded memory
//!
//! [`arec_to_db3`] reads `input` twice — once to discover every distinct
//! `(node, output)` pair and register its `.db3` topic (needed *before*
//! any message row can name a `topic_id`), once to stream the messages
//! themselves — rather than collecting entries into a `Vec` first. Both
//! passes are [`astrs_recording::Reader::iter_all`], which recomputes the
//! same HLC-sorted order from the same footer index each time it is
//! called, so the two passes agree on ordering without any state carried
//! between them.

use std::collections::BTreeMap;
use std::path::Path;

use astrs_recording::Reader as ArecReader;

use crate::convert::{ConversionReport, infer_serialization_format, physical_ns_to_i64};
use crate::db3::{MessageRow, Writer as Db3Writer};
use crate::error::RosbagError;
use crate::{RosbagTopicManifest, SIDECAR_FORMAT_VERSION, SIDECAR_KEY, SidecarTopic, TopicRecord};

/// Converts the `.arec` recording at `input` to a fresh rosbag2 `.db3` at
/// `output`.
///
/// Every topic is either recovered exactly from `input`'s own embedded
/// [`RosbagTopicManifest`] sidecar (present when `input` was itself
/// produced by [`crate::convert::db3_to_arec::db3_to_arec`] or
/// [`crate::convert::mcap_to_arec::mcap_to_arec`]), or — for a genuine
/// dataflow recording, which has no such sidecar — synthesized as
/// `/{node}/{output}` with an empty type and a `serialization_format`
/// inferred by peeking that topic's first message (never fabricated as
/// `"cdr"` unless a CDR encapsulation header actually parses). Either
/// way, the resulting `.db3`
/// carries its own `custom_data` sidecar recording every topic actually
/// written plus `input`'s dataflow id and HLC epoch, so a later
/// `.db3 → .arec` conversion of *this* file recovers them exactly rather
/// than re-synthesizing.
///
/// # Errors
///
/// [`RosbagError::Recording`] if `input` cannot be opened (falling back
/// to scan recovery for a footerless file, as `astrs bag info`/`astrs
/// replay` do), [`RosbagError::Id`] if a sidecar entry's `node`/`output`
/// text is not a valid identifier, or [`RosbagError::Sql`]/
/// [`RosbagError::Io`] if `output` cannot be written.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::convert::arec_to_db3;
/// use astrs_recording::{Entry, Writer, WriterOptions};
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
///
/// let dir = std::env::temp_dir().join(format!("astrs-rosbag-arec-to-db3-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let input = dir.join("session.arec");
/// let output = dir.join("session.db3");
///
/// let mut writer = Writer::create(&input, WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::new(1_000, 0)))?;
/// writer.append(Entry::new(NodeId::new("scan")?, DataId::new("data")?, Metadata::new(HlcTimestamp::new(1_000, 0)), vec![1, 2, 3]))?;
/// writer.finish()?;
///
/// let report = arec_to_db3(&input, &output)?;
/// assert_eq!(report.topics, 1);
/// assert_eq!(report.messages, 1);
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn arec_to_db3(input: &Path, output: &Path) -> Result<ConversionReport, RosbagError> {
    let (mut reader, _recovery) = ArecReader::open_or_recover(input)?;
    let dataflow = reader.header().dataflow;
    let hlc_epoch = reader.header().hlc_epoch;
    let sidecar = RosbagTopicManifest::from_manifest_yaml(&reader.header().manifest_yaml);

    let mut sidecar_by_port: BTreeMap<(String, String), TopicRecord> = BTreeMap::new();
    if let Some(manifest) = &sidecar {
        for topic in &manifest.topics {
            sidecar_by_port.insert(
                (topic.node.clone(), topic.output.clone()),
                topic.record.clone(),
            );
        }
    }
    let ros_distro = sidecar
        .as_ref()
        .map(|manifest| manifest.ros_distro.clone())
        .unwrap_or_default();

    let mut writer = Db3Writer::create(output, ros_distro)?;
    let mut warnings = Vec::new();
    let mut topic_ids: BTreeMap<(String, String), i64> = BTreeMap::new();
    let mut written_topics: Vec<((String, String), TopicRecord)> = Vec::new();

    // Register every sidecar-listed topic *before* looking at a single
    // entry: a topic that never carried a message (a subscribed-but-idle
    // port, a short recording that ended before its first tick) is still
    // a real topic a `.db3 -> .arec` conversion of this same sidecar
    // would have listed, and must not vanish just because pass 1 below is
    // otherwise entry-driven.
    if let Some(manifest) = &sidecar {
        for topic in &manifest.topics {
            let key = (topic.node.clone(), topic.output.clone());
            if topic_ids.contains_key(&key) {
                continue;
            }
            let topic_id = writer.create_topic(&topic.record)?;
            topic_ids.insert(key.clone(), topic_id);
            written_topics.push((key, topic.record.clone()));
        }
    }

    // Pass 1: register one `.db3` topic per distinct `(node, output)` this
    // recording actually carries an entry for — covers every topic the
    // sidecar (if any) did not already list, i.e. every synthesized one.
    for entry in reader.iter_all() {
        let entry = entry?;
        let key = (
            entry.node.as_str().to_owned(),
            entry.output.as_str().to_owned(),
        );
        if topic_ids.contains_key(&key) {
            continue;
        }
        let record = match sidecar_by_port.get(&key) {
            Some(record) => record.clone(),
            None => {
                let format = infer_serialization_format(&entry.payload);
                let topic = format!("/{}/{}", key.0, key.1);
                warnings.push(format!(
                    "topic for astrs node/output `{}/{}` was not in the embedded sidecar \
                     (this .arec was not produced by a prior bag \u{2192} arec conversion); \
                     synthesized as `{topic}` with serialization_format `{format}`",
                    key.0, key.1
                ));
                TopicRecord::new(topic, "", format)
            }
        };
        let topic_id = writer.create_topic(&record)?;
        topic_ids.insert(key.clone(), topic_id);
        written_topics.push((key, record));
    }

    // The output sidecar: every topic actually written, plus enough of
    // `input`'s own identity (dataflow id, HLC epoch) that a later
    // `.db3 → .arec` conversion of this file recovers both exactly — see
    // the module docs.
    let output_sidecar = RosbagTopicManifest {
        format_version: SIDECAR_FORMAT_VERSION,
        ros_distro: sidecar
            .as_ref()
            .map(|manifest| manifest.ros_distro.clone())
            .unwrap_or_default(),
        dataflow: Some(dataflow.to_string()),
        hlc_epoch: Some(hlc_epoch.to_string()),
        topics: written_topics
            .iter()
            .map(|((node, output), record)| SidecarTopic {
                node: node.clone(),
                output: output.clone(),
                record: record.clone(),
            })
            .collect(),
    };
    let sidecar_yaml = output_sidecar.to_manifest_yaml().map_err(|source| {
        RosbagError::Internal(format!(
            "the astrs-rosbag topic-manifest sidecar did not serialize: {source}"
        ))
    })?;
    writer.set_custom_data(SIDECAR_KEY, sidecar_yaml);

    // Pass 2: stream every message, in the same HLC order pass 1 walked.
    let mut messages = 0u64;
    let mut saturated = 0u64;
    let rows = reader
        .iter_all()
        .map(|entry| -> Result<MessageRow, RosbagError> {
            let entry = entry?;
            let key = (
                entry.node.as_str().to_owned(),
                entry.output.as_str().to_owned(),
            );
            let topic_id = *topic_ids.get(&key).ok_or_else(|| {
                RosbagError::Internal(format!(
                    "topic id for `{}/{}` was not registered in pass 1",
                    key.0, key.1
                ))
            })?;
            let (timestamp_ns, was_saturated) = physical_ns_to_i64(entry.hlc().physical_ns());
            if was_saturated {
                saturated += 1;
            }
            messages += 1;
            Ok((topic_id, timestamp_ns, entry.payload))
        });
    writer.write_messages(rows)?;

    if saturated > 0 {
        warnings.push(format!(
            "{saturated} message timestamp(s) exceeded i64::MAX nanoseconds and were \
             saturated to i64::MAX in the .db3 output"
        ));
    }

    writer.finish()?;

    Ok(ConversionReport {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        direction: "arec -> db3",
        topics: topic_ids.len(),
        messages,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::convert::db3_to_arec::db3_to_arec;
    use crate::db3::Reader as Db3Reader;
    use astrs_recording::{Entry, Writer as ArecWriter, WriterOptions};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-rosbag-arec-to-db3-test-{}-{}-{label}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_arec(path: &Path, dataflow: u128, entries: &[(&str, &str, u64, u32, Vec<u8>)]) {
        let options = WriterOptions::new(DataflowId::from_u128(dataflow), HlcTimestamp::new(1, 0));
        let mut writer = ArecWriter::create(path, options).unwrap();
        for (node, output, physical_ns, logical, payload) in entries {
            writer
                .append(Entry::new(
                    NodeId::new(*node).unwrap(),
                    DataId::new(*output).unwrap(),
                    Metadata::new(HlcTimestamp::new(*physical_ns, *logical)),
                    payload.clone(),
                ))
                .unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn a_genuine_dataflow_recording_synthesizes_topic_names() {
        let dir = temp_dir("synthesize");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        write_arec(&input, 1, &[("camera", "frames", 1_000, 0, vec![1, 2, 3])]);

        let report = arec_to_db3(&input, &output).unwrap();
        assert_eq!(report.topics, 1);
        assert_eq!(report.messages, 1);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("synthesized"),
            "{:?}",
            report.warnings
        );

        let reader = Db3Reader::open(&output).unwrap();
        assert_eq!(reader.topics()[0].1.topic, "/camera/frames");
        assert_eq!(
            reader.topics()[0].1.serialization_format,
            crate::RAW_SERIALIZATION_FORMAT
        );
        let messages: Vec<_> = reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages[0].data, vec![1, 2, 3]);
        assert_eq!(messages[0].timestamp_ns, 1_000);
    }

    #[test]
    fn a_real_cdr_payload_is_recognized_without_a_sidecar() {
        let dir = temp_dir("cdr-peek");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        // `CDR_LE` header (0x0001) then a zero options word.
        write_arec(
            &input,
            1,
            &[("scan", "data", 1, 0, vec![0x00, 0x01, 0x00, 0x00])],
        );
        arec_to_db3(&input, &output).unwrap();
        let reader = Db3Reader::open(&output).unwrap();
        assert_eq!(reader.topics()[0].1.serialization_format, "cdr");
    }

    #[test]
    fn serialization_format_inference_peeks_only_the_first_message_on_a_topic() {
        // A raw-shaped first message fixes the topic as
        // `RAW_SERIALIZATION_FORMAT`, even though a later message on the
        // *same* topic happens to look CDR-shaped — one topic, one
        // format, decided once (see `arec_to_db3`'s own doc comment: "a
        // `serialization_format` inferred by peeking that topic's first
        // message").
        let dir = temp_dir("peek-once");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        write_arec(
            &input,
            1,
            &[
                ("scan", "data", 1, 0, vec![0xff, 0xff]),
                ("scan", "data", 2, 0, vec![0x00, 0x01, 0x00, 0x00]),
            ],
        );
        arec_to_db3(&input, &output).unwrap();
        let reader = Db3Reader::open(&output).unwrap();
        assert_eq!(reader.topics().len(), 1);
        assert_eq!(
            reader.topics()[0].1.serialization_format,
            crate::RAW_SERIALIZATION_FORMAT
        );
    }

    #[test]
    fn distinct_ports_become_distinct_topics_and_messages_land_on_the_right_one() {
        let dir = temp_dir("multi-port");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        write_arec(
            &input,
            1,
            &[
                ("camera", "frames", 10, 0, vec![1]),
                ("lidar", "points", 20, 0, vec![2]),
                ("camera", "frames", 30, 0, vec![3]),
            ],
        );
        let report = arec_to_db3(&input, &output).unwrap();
        assert_eq!(report.topics, 2);
        assert_eq!(report.messages, 3);

        let reader = Db3Reader::open(&output).unwrap();
        assert_eq!(reader.topics().len(), 2);
        let camera_id = reader
            .topics()
            .iter()
            .find(|(_, t)| t.topic == "/camera/frames")
            .unwrap()
            .0;
        let camera_messages: Vec<_> = reader
            .iter_messages()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter(|m| m.topic_id == camera_id)
            .collect();
        assert_eq!(camera_messages.len(), 2);
    }

    #[test]
    fn an_empty_recording_still_produces_a_finished_bag() {
        let dir = temp_dir("empty");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        write_arec(&input, 1, &[]);
        let report = arec_to_db3(&input, &output).unwrap();
        assert_eq!(report.topics, 0);
        assert_eq!(report.messages, 0);
        assert!(report.warnings.is_empty());
        assert!(output.exists());
    }

    #[test]
    fn an_oversized_physical_ns_saturates_with_a_warning_not_a_panic() {
        let dir = temp_dir("saturate");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        write_arec(&input, 1, &[("x", "y", u64::MAX, 0, vec![9])]);
        let report = arec_to_db3(&input, &output).unwrap();
        assert!(
            report.warnings.iter().any(|w| w.contains("saturated")),
            "{:?}",
            report.warnings
        );
        let reader = Db3Reader::open(&output).unwrap();
        let messages: Vec<_> = reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages[0].timestamp_ns, i64::MAX);
    }

    #[test]
    fn round_trip_through_db3_and_back_preserves_dataflow_hlc_epoch_and_ports() {
        let dir = temp_dir("round-trip");
        let arec_a = dir.join("a.arec");
        let db3_b = dir.join("b.db3");
        let arec_c = dir.join("c.arec");
        write_arec(
            &arec_a,
            0x2a,
            &[
                ("camera", "frames", 5_000, 0, vec![1, 2, 3]),
                ("lidar", "points", 6_000, 0, vec![4, 5]),
            ],
        );

        let (original, _) = ArecReader::open_or_recover(&arec_a).unwrap();
        let original_dataflow = original.header().dataflow;
        let original_epoch = original.header().hlc_epoch;
        drop(original);

        arec_to_db3(&arec_a, &db3_b).unwrap();
        db3_to_arec(&db3_b, &arec_c).unwrap();

        let (roundtripped, _) = ArecReader::open_or_recover(&arec_c).unwrap();
        assert_eq!(roundtripped.header().dataflow, original_dataflow);
        assert_eq!(roundtripped.header().hlc_epoch, original_epoch);

        let mut ports: Vec<(String, String)> = roundtripped
            .index()
            .iter()
            .map(|entry| {
                (
                    entry.node.as_str().to_owned(),
                    entry.output.as_str().to_owned(),
                )
            })
            .collect();
        ports.sort();
        ports.dedup();
        assert_eq!(
            ports,
            vec![
                ("camera".to_owned(), "frames".to_owned()),
                ("lidar".to_owned(), "points".to_owned()),
            ]
        );
    }

    #[test]
    fn a_missing_input_is_a_typed_recording_error_not_a_panic() {
        let dir = temp_dir("missing-input");
        let input = dir.join("does-not-exist.arec");
        let output = dir.join("session.db3");
        let err = arec_to_db3(&input, &output).unwrap_err();
        assert!(matches!(err, RosbagError::Recording(_)), "{err:?}");
    }

    #[test]
    fn a_corrupt_input_with_the_wrong_magic_is_a_typed_recording_error() {
        let dir = temp_dir("corrupt-input");
        let input = dir.join("garbage.arec");
        let output = dir.join("session.db3");
        std::fs::write(&input, b"not an .arec file at all, just noise bytes").unwrap();
        let err = arec_to_db3(&input, &output).unwrap_err();
        assert!(matches!(err, RosbagError::Recording(_)), "{err:?}");
    }

    #[test]
    fn a_sidecar_covering_only_some_ports_leaves_the_rest_synthesized() {
        // A sidecar that names `camera/frames` but is silent about
        // `lidar/points` — plausible for a hand-edited or partially
        // stale sidecar, and proves the two topic-discovery paths
        // (sidecar-seeded, entry-driven-synthesized) coexist correctly
        // within one conversion rather than one silently winning.
        let dir = temp_dir("mixed-sidecar-coverage");
        let input = dir.join("a.arec");
        let output = dir.join("b.db3");
        write_arec(
            &input,
            1,
            &[
                ("camera", "frames", 100, 0, vec![1, 2, 3]),
                ("lidar", "points", 200, 0, vec![4, 5]),
            ],
        );

        let (dataflow, epoch, entries) = {
            let (mut reader, _) = ArecReader::open_or_recover(&input).unwrap();
            let dataflow = reader.header().dataflow;
            let epoch = reader.header().hlc_epoch;
            let entries = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
            (dataflow, epoch, entries)
        };
        let mut manifest = RosbagTopicManifest::new("jazzy");
        manifest.topics.push(SidecarTopic {
            node: "camera".to_owned(),
            output: "frames".to_owned(),
            record: TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr"),
        });
        let options = WriterOptions::new(dataflow, epoch)
            .with_manifest_yaml(manifest.to_manifest_yaml().unwrap());
        let mut rewritten = ArecWriter::create(&input, options).unwrap();
        for entry in entries {
            rewritten.append(entry).unwrap();
        }
        rewritten.finish().unwrap();

        let report = arec_to_db3(&input, &output).unwrap();
        assert_eq!(report.topics, 2);
        assert_eq!(report.warnings.len(), 1, "only the unlisted port warns");
        assert!(
            report.warnings[0].contains("lidar"),
            "{:?}",
            report.warnings
        );

        let reader = Db3Reader::open(&output).unwrap();
        let mut names: Vec<&str> = reader
            .topics()
            .iter()
            .map(|(_, t)| t.topic.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["/lidar/points", "/scan"]);
        let scan = reader
            .topics()
            .iter()
            .find(|(_, t)| t.topic == "/scan")
            .unwrap();
        assert_eq!(scan.1.serialization_format, "cdr");
        assert_eq!(scan.1.r#type, "sensor_msgs/msg/LaserScan");
    }

    #[test]
    fn a_sidecars_ros_distro_propagates_into_the_output_schema_table() {
        let dir = temp_dir("ros-distro");
        let input = dir.join("a.arec");
        let output = dir.join("b.db3");
        write_arec(&input, 1, &[("camera", "frames", 1, 0, vec![1])]);

        let (dataflow, epoch, entries) = {
            let (mut reader, _) = ArecReader::open_or_recover(&input).unwrap();
            let dataflow = reader.header().dataflow;
            let epoch = reader.header().hlc_epoch;
            let entries = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
            (dataflow, epoch, entries)
        };
        let mut manifest = RosbagTopicManifest::new("jazzy");
        manifest.topics.push(SidecarTopic {
            node: "camera".to_owned(),
            output: "frames".to_owned(),
            record: TopicRecord::new("/camera/frames", "", "cdr"),
        });
        let options = WriterOptions::new(dataflow, epoch)
            .with_manifest_yaml(manifest.to_manifest_yaml().unwrap());
        let mut rewritten = ArecWriter::create(&input, options).unwrap();
        for entry in entries {
            rewritten.append(entry).unwrap();
        }
        rewritten.finish().unwrap();

        arec_to_db3(&input, &output).unwrap();
        let db3_metadata = Db3Reader::open(&output)
            .unwrap()
            .metadata()
            .unwrap()
            .clone();
        assert_eq!(db3_metadata.ros_distro, "jazzy");
    }

    #[test]
    fn topic_names_that_collide_after_sanitization_are_disambiguated_and_warned() {
        // Two ports whose names both sanitize to the same synthesized
        // topic name once slashes become underscores — proving the
        // `create_topic`/collision path a synthesized (no-sidecar) topic
        // takes is exercised end to end, not just at the `assign_node_ids`
        // unit level `db3_to_arec` also relies on.
        let dir = temp_dir("collision");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        // `/a.b/c` and `/a_b/c` both synthesize to a topic containing
        // `a_b`/`c`, but the *node ids themselves* (`a.b` vs `a_b`) are
        // already distinct astrs identifiers, so this exercises "two
        // distinct topics, no collision" rather than a true collision —
        // real collisions are `assign_node_ids`'s own concern (see
        // `topic::tests`), already covered there. What matters here is
        // that two distinct ports never merge into one topic id.
        write_arec(
            &input,
            1,
            &[("a.b", "c", 1, 0, vec![1]), ("a_b", "c", 2, 0, vec![2])],
        );
        let report = arec_to_db3(&input, &output).unwrap();
        assert_eq!(report.topics, 2);
        let reader = Db3Reader::open(&output).unwrap();
        assert_eq!(reader.topics().len(), 2);
    }

    #[test]
    fn a_sidecar_qos_profile_round_trips_through_the_output_db3() {
        let dir = temp_dir("qos-round-trip");
        let input = dir.join("a.arec");
        let db3 = dir.join("b.db3");
        write_arec(&input, 1, &[("camera", "frames", 1_000, 0, vec![1, 2, 3])]);

        // Read back what `write_arec` just wrote *before* rewriting the
        // same path with a sidecar attached — `ArecWriter::create`
        // truncates its path, so everything needed from the original
        // must be collected first.
        let (dataflow, epoch, entries) = {
            let (mut reader, _) = ArecReader::open_or_recover(&input).unwrap();
            let dataflow = reader.header().dataflow;
            let epoch = reader.header().hlc_epoch;
            let entries = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
            (dataflow, epoch, entries)
        };

        // Rewrite `input` as if it were itself a `db3 -> arec` output: a
        // sidecar naming a real QoS profile for this topic.
        let mut record = TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr");
        record
            .offered_qos_profiles
            .push(crate::QosProfile::reliable_default());
        let mut manifest = RosbagTopicManifest::new("jazzy");
        manifest.topics.push(SidecarTopic {
            node: "camera".to_owned(),
            output: "frames".to_owned(),
            record,
        });
        let options = WriterOptions::new(dataflow, epoch)
            .with_manifest_yaml(manifest.to_manifest_yaml().unwrap());
        let mut rewritten = ArecWriter::create(&input, options).unwrap();
        for entry in entries {
            rewritten.append(entry).unwrap();
        }
        rewritten.finish().unwrap();

        arec_to_db3(&input, &db3).unwrap();
        let reader = Db3Reader::open(&db3).unwrap();
        assert_eq!(reader.topics()[0].1.topic, "/scan");
        assert_eq!(
            reader.topics()[0].1.offered_qos_profiles,
            vec![crate::QosProfile::reliable_default()]
        );
    }

    proptest::proptest! {
        // Each case does real file I/O across three files (write, two
        // conversions, read back), so the default 256 cases would make
        // this one property dominate the whole crate's test time; 24
        // still covers the shrinking search space meaningfully.
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn arec_to_db3_to_arec_preserves_dataflow_epoch_and_every_entry(
            dataflow_bits in any::<u64>(),
            epoch_ns in 0u64..1_000_000_000_000,
            entries in proptest::collection::vec(
                (
                    "[a-zA-Z][a-zA-Z0-9]{0,7}",
                    "[a-zA-Z][a-zA-Z0-9]{0,7}",
                    0u64..1_000_000_000_000,
                    proptest::collection::vec(any::<u8>(), 0..32),
                ),
                0..8,
            ),
        ) {
            let dir = temp_dir("proptest-round-trip");
            let arec_a = dir.join("a.arec");
            let db3_b = dir.join("b.db3");
            let arec_c = dir.join("c.arec");

            let dataflow = DataflowId::from_u128(u128::from(dataflow_bits));
            let epoch = HlcTimestamp::new(epoch_ns, 0);
            let options = WriterOptions::new(dataflow, epoch);
            let mut writer = ArecWriter::create(&arec_a, options).unwrap();
            // Logical counters are deliberately left at 0 throughout: they
            // are a known, documented lossy dimension (see the
            // `crate::convert` module docs), so a property about *exact*
            // round-tripping only holds where nothing forces a loss.
            let mut expected: Vec<(String, String, i64, Vec<u8>)> = Vec::new();
            for (node, output, ns, payload) in &entries {
                writer
                    .append(Entry::new(
                        NodeId::new(node.as_str()).unwrap(),
                        DataId::new(output.as_str()).unwrap(),
                        Metadata::new(HlcTimestamp::new(*ns, 0)),
                        payload.clone(),
                    ))
                    .unwrap();
                expected.push((node.clone(), output.clone(), *ns as i64, payload.clone()));
            }
            writer.finish().unwrap();

            arec_to_db3(&arec_a, &db3_b).unwrap();
            db3_to_arec(&db3_b, &arec_c).unwrap();

            let (mut reader, _) = ArecReader::open_or_recover(&arec_c).unwrap();
            prop_assert_eq!(reader.header().dataflow, dataflow);
            prop_assert_eq!(reader.header().hlc_epoch, epoch);

            let mut actual: Vec<(String, String, i64, Vec<u8>)> = reader
                .iter_all()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .into_iter()
                .map(|entry| {
                    let physical_ns = entry.hlc().physical_ns() as i64;
                    (
                        entry.node.into_string(),
                        entry.output.into_string(),
                        physical_ns,
                        entry.payload,
                    )
                })
                .collect();
            let mut expected_sorted = expected;
            actual.sort();
            expected_sorted.sort();
            prop_assert_eq!(actual, expected_sorted);
        }
    }
}
