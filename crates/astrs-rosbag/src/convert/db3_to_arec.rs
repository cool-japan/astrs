//! `.db3 → .arec` (blueprint §10.6, §14): `astrs bag convert x.db3 y.arec`.
//!
//! See the [`crate::convert`] module docs for the round-trip identity this
//! recovers when `input` was itself produced by
//! [`crate::convert::arec_to_db3::arec_to_db3`], and what does not survive
//! either way.

use std::collections::BTreeMap;
use std::path::Path;

use astrs_recording::{Writer as ArecWriter, WriterOptions};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, DataflowId, Metadata, NodeId};

use crate::convert::{ConversionReport, i64_to_physical_ns};
use crate::db3::Reader as Db3Reader;
use crate::error::RosbagError;
use crate::{
    OUTPUT_NAME, RosbagTopicManifest, SIDECAR_FORMAT_VERSION, SIDECAR_KEY, SidecarTopic,
    assign_node_ids,
};

/// Converts the rosbag2 `.db3` at `input` to a fresh `.arec` at `output`.
///
/// Every topic's astrs node/output ids are either recovered exactly from
/// `input`'s own `custom_data["astrs_rosbag_topics"]` sidecar entry
/// (present when `input` was itself produced by
/// [`crate::convert::arec_to_db3::arec_to_db3`] — see
/// [`crate::db3::Reader::metadata`]'s fallback for where that entry can
/// live), or — for a genuine external bag, which carries no such entry —
/// assigned fresh via [`assign_node_ids`], with every topic writing to
/// [`OUTPUT_NAME`]. The resulting `.arec`'s header dataflow id and HLC
/// epoch are recovered the same way, falling back to
/// [`DataflowId::generate`] and the earliest message's own timestamp
/// respectively.
///
/// # Errors
///
/// [`RosbagError::Sql`]/[`RosbagError::Io`]/[`RosbagError::MissingTable`]/
/// [`RosbagError::ColumnTypeMismatch`] if `input` cannot be opened or
/// read, or [`RosbagError::Recording`] if `output` cannot be written.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::convert::db3_to_arec;
/// use astrs_rosbag::TopicRecord;
/// use astrs_rosbag::db3::Writer;
///
/// let dir = std::env::temp_dir().join(format!("astrs-rosbag-db3-to-arec-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let input = dir.join("session.db3");
/// let output = dir.join("session.arec");
///
/// let mut writer = Writer::create(&input, "jazzy")?;
/// let topic_id = writer.create_topic(&TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr"))?;
/// writer.write_messages(std::iter::once(Ok((topic_id, 1_000, vec![1, 2, 3]))))?;
/// writer.finish()?;
///
/// let report = db3_to_arec(&input, &output)?;
/// assert_eq!(report.topics, 1);
/// assert_eq!(report.messages, 1);
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn db3_to_arec(input: &Path, output: &Path) -> Result<ConversionReport, RosbagError> {
    let reader = Db3Reader::open(input)?;
    let mut warnings: Vec<String> = reader.warnings().to_vec();

    let sidecar = reader
        .metadata()
        .and_then(|metadata| metadata.custom_data.get(SIDECAR_KEY))
        .and_then(|yaml| RosbagTopicManifest::from_manifest_yaml(yaml));
    let sidecar_by_topic_name: BTreeMap<&str, &SidecarTopic> = sidecar
        .as_ref()
        .map(|manifest| {
            manifest
                .topics
                .iter()
                .map(|topic| (topic.record.topic.as_str(), topic))
                .collect()
        })
        .unwrap_or_default();

    // Every topic not covered by the sidecar gets a freshly assigned node
    // id, computed in one batch (so collisions are disambiguated
    // deterministically, exactly as a `bag → arec` conversion with no
    // sidecar at all always has).
    let topic_names: Vec<&str> = reader
        .topics()
        .iter()
        .map(|(_, record)| record.topic.as_str())
        .collect();
    let mut assign_warnings = Vec::new();
    let fresh_ids = assign_node_ids(topic_names.into_iter(), &mut assign_warnings);
    warnings.extend(assign_warnings);

    let mut topic_ports: BTreeMap<i64, (NodeId, DataId)> = BTreeMap::new();
    for (index, (topic_id, record)) in reader.topics().iter().enumerate() {
        let port = match sidecar_by_topic_name.get(record.topic.as_str()) {
            Some(sidecar_topic) => (
                NodeId::new(sidecar_topic.node.clone())?,
                DataId::new(sidecar_topic.output.clone())?,
            ),
            None => (fresh_ids[index].clone(), DataId::new(OUTPUT_NAME)?),
        };
        topic_ports.insert(*topic_id, port);
    }

    let dataflow = sidecar
        .as_ref()
        .and_then(|manifest| manifest.dataflow.as_deref())
        .and_then(|text| text.parse::<DataflowId>().ok())
        .unwrap_or_else(DataflowId::generate);

    let hlc_epoch = match sidecar
        .as_ref()
        .and_then(|manifest| manifest.hlc_epoch.as_deref())
        .and_then(|text| text.parse::<HlcTimestamp>().ok())
    {
        Some(epoch) => epoch,
        None => earliest_message_epoch(&reader)?,
    };

    let ros_distro = sidecar
        .as_ref()
        .map(|manifest| manifest.ros_distro.clone())
        .filter(|distro| !distro.is_empty())
        .or_else(|| {
            reader
                .metadata()
                .map(|metadata| metadata.ros_distro.clone())
        })
        .unwrap_or_default();

    let output_sidecar = RosbagTopicManifest {
        format_version: SIDECAR_FORMAT_VERSION,
        ros_distro,
        dataflow: Some(dataflow.to_string()),
        hlc_epoch: Some(hlc_epoch.to_string()),
        topics: reader
            .topics()
            .iter()
            .map(|(topic_id, record)| {
                let (node, output) = &topic_ports[topic_id];
                SidecarTopic {
                    node: node.as_str().to_owned(),
                    output: output.as_str().to_owned(),
                    record: record.clone(),
                }
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

    let mut messages = 0u64;
    let mut saturated = 0u64;
    for message in reader.iter_messages() {
        let message = message?;
        let (node, data_id) = topic_ports.get(&message.topic_id).cloned().ok_or_else(|| {
            RosbagError::Internal(format!(
                "message references topic id {} with no registered topic",
                message.topic_id
            ))
        })?;
        let (physical_ns, was_saturated) = i64_to_physical_ns(message.timestamp_ns);
        if was_saturated {
            saturated += 1;
        }
        writer.append_parts(
            node,
            data_id,
            Metadata::new(HlcTimestamp::new(physical_ns, 0)),
            message.data,
        )?;
        messages += 1;
    }
    if saturated > 0 {
        warnings.push(format!(
            "{saturated} message timestamp(s) were pre-epoch (negative) and were saturated \
             to physical nanosecond 0 in the .arec output"
        ));
    }
    writer.finish()?;

    Ok(ConversionReport {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        direction: "db3 -> arec",
        topics: reader.topics().len(),
        messages,
        warnings,
    })
}

/// The earliest message's timestamp as an [`HlcTimestamp`] (logical `0`),
/// or [`HlcTimestamp::EPOCH`] for a bag with no messages — [`db3_to_arec`]'s
/// fallback header epoch when no sidecar supplied one directly.
fn earliest_message_epoch(reader: &Db3Reader) -> Result<HlcTimestamp, RosbagError> {
    let mut min_ns: Option<i64> = None;
    for message in reader.iter_messages() {
        let message = message?;
        min_ns = Some(min_ns.map_or(message.timestamp_ns, |min| min.min(message.timestamp_ns)));
    }
    Ok(match min_ns {
        Some(timestamp_ns) => {
            let (physical_ns, _saturated) = i64_to_physical_ns(timestamp_ns);
            HlcTimestamp::new(physical_ns, 0)
        }
        None => HlcTimestamp::EPOCH,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::TopicRecord;
    use crate::db3::Writer as Db3Writer;
    use astrs_recording::Reader as ArecReader;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-rosbag-db3-to-arec-test-{}-{}-{label}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_genuine_bag_assigns_fresh_node_ids_and_a_fresh_dataflow() {
        let dir = temp_dir("fresh");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");

        let mut writer = Db3Writer::create(&input, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "sensor_msgs/msg/LaserScan",
                "cdr",
            ))
            .unwrap();
        writer
            .write_messages(std::iter::once(Ok((topic_id, 5_000, vec![1, 2, 3]))))
            .unwrap();
        writer.finish().unwrap();

        let report = db3_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 1);
        assert_eq!(report.messages, 1);

        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        assert_ne!(reader.header().dataflow, DataflowId::NIL);
        let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node.as_str(), "_scan");
        assert_eq!(entries[0].output.as_str(), OUTPUT_NAME);
        assert_eq!(entries[0].payload, vec![1, 2, 3]);
        assert_eq!(entries[0].hlc(), HlcTimestamp::new(5_000, 0));
    }

    #[test]
    fn type_description_hash_round_trips_through_db3_and_back() {
        let dir = temp_dir("type-hash");
        let db3_a = dir.join("a.db3");
        let arec_b = dir.join("b.arec");
        let db3_c = dir.join("c.db3");

        let mut writer = Db3Writer::create(&db3_a, "jazzy").unwrap();
        let mut record = TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr");
        record.type_description_hash = "RIHS01_deadbeefcafef00d".to_owned();
        let topic_id = writer.create_topic(&record).unwrap();
        writer
            .write_messages(std::iter::once(Ok((topic_id, 1, vec![1]))))
            .unwrap();
        writer.finish().unwrap();

        db3_to_arec(&db3_a, &arec_b).unwrap();
        crate::convert::arec_to_db3::arec_to_db3(&arec_b, &db3_c).unwrap();

        let reader = Db3Reader::open(&db3_c).unwrap();
        assert_eq!(
            reader.topics()[0].1.type_description_hash,
            "RIHS01_deadbeefcafef00d"
        );
    }

    #[test]
    fn ros_distro_falls_back_to_the_bags_own_metadata_when_no_sidecar_names_one() {
        // No `custom_data` sidecar at all (a genuine external bag), but
        // `Writer::create`'s own `ros_distro` argument still reaches the
        // output `.arec`'s sidecar via `BagMetadata::ros_distro` — the
        // fallback this function's `ros_distro` computation falls through
        // to when `sidecar` is `None` entirely (distinct from the
        // sidecar-present-but-empty case
        // `a_genuine_bag_assigns_fresh_node_ids_and_a_fresh_dataflow`
        // already covers via `Writer::create(&input, "jazzy")` too, but
        // without asserting on `ros_distro` specifically).
        let dir = temp_dir("ros-distro-fallback");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");
        let mut writer = Db3Writer::create(&input, "humble").unwrap();
        writer
            .create_topic(&TopicRecord::new("/x", "", "cdr"))
            .unwrap();
        writer.finish().unwrap();

        db3_to_arec(&input, &output).unwrap();
        let (reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let sidecar =
            RosbagTopicManifest::from_manifest_yaml(&reader.header().manifest_yaml).unwrap();
        assert_eq!(sidecar.ros_distro, "humble");
    }

    #[test]
    fn hlc_epoch_falls_back_to_the_earliest_message_when_no_sidecar_is_present() {
        let dir = temp_dir("epoch-fallback");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");

        let mut writer = Db3Writer::create(&input, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new("/x", "", "cdr"))
            .unwrap();
        writer
            .write_messages(
                vec![
                    Ok((topic_id, 30_000, vec![1])),
                    Ok((topic_id, 10_000, vec![2])),
                    Ok((topic_id, 20_000, vec![3])),
                ]
                .into_iter(),
            )
            .unwrap();
        writer.finish().unwrap();

        db3_to_arec(&input, &output).unwrap();
        let (reader, _) = ArecReader::open_or_recover(&output).unwrap();
        assert_eq!(reader.header().hlc_epoch, HlcTimestamp::new(10_000, 0));
    }

    #[test]
    fn a_pre_epoch_timestamp_saturates_with_a_warning_not_a_panic() {
        let dir = temp_dir("pre-epoch");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");

        let mut writer = Db3Writer::create(&input, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new("/x", "", "cdr"))
            .unwrap();
        writer
            .write_messages(std::iter::once(Ok((topic_id, -1, vec![9]))))
            .unwrap();
        writer.finish().unwrap();

        let report = db3_to_arec(&input, &output).unwrap();
        assert!(
            report.warnings.iter().any(|w| w.contains("saturated")),
            "{:?}",
            report.warnings
        );
        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        assert_eq!(entries[0].hlc(), HlcTimestamp::new(0, 0));
    }

    #[test]
    fn an_empty_bag_still_produces_a_finished_recording() {
        let dir = temp_dir("empty");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");
        Db3Writer::create(&input, "jazzy")
            .unwrap()
            .finish()
            .unwrap();

        let report = db3_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 0);
        assert_eq!(report.messages, 0);
        let (reader, _) = ArecReader::open_or_recover(&output).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.header().hlc_epoch, HlcTimestamp::EPOCH);
    }

    #[test]
    fn a_missing_input_is_a_typed_error_not_a_panic() {
        let dir = temp_dir("missing-input");
        let input = dir.join("does-not-exist.db3");
        let output = dir.join("session.arec");
        let err = db3_to_arec(&input, &output).unwrap_err();
        assert!(matches!(err, RosbagError::Io { .. }), "{err:?}");
    }

    #[test]
    fn topic_names_colliding_after_sanitization_are_disambiguated_and_warned() {
        // The exact pair `topic::tests::assign_node_ids_disambiguates_a_
        // three_way_collision` (minus the third entry) uses at the unit
        // level — here the same collision is exercised through the full
        // `.db3 -> .arec` pipeline, proving `db3_to_arec` surfaces the
        // warning and that both topics still convert (merged onto
        // distinct, disambiguated node ids, never silently dropped).
        let dir = temp_dir("collision");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");

        let mut writer = Db3Writer::create(&input, "jazzy").unwrap();
        let a = writer
            .create_topic(&TopicRecord::new("/a/b", "", "cdr"))
            .unwrap();
        let b = writer
            .create_topic(&TopicRecord::new("/a_b", "", "cdr"))
            .unwrap();
        writer
            .write_messages(vec![Ok((a, 1, vec![1])), Ok((b, 2, vec![2]))].into_iter())
            .unwrap();
        writer.finish().unwrap();

        let report = db3_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 2);
        assert!(
            report.warnings.iter().any(|w| w.contains("sanitize")),
            "{:?}",
            report.warnings
        );

        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let mut node_ids: Vec<String> = reader
            .iter_all()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .map(|entry| entry.node.into_string())
            .collect();
        node_ids.sort();
        assert_eq!(node_ids, vec!["_a_b".to_owned(), "_a_b.2".to_owned()]);
    }

    #[test]
    fn a_schema_version_one_bag_with_no_qos_or_hash_columns_converts_cleanly() {
        // The oldest rosbag2 schema `db3::Reader` tolerates (see
        // `db3::schema`'s module docs) — proving a v1 bag converts through
        // the full pipeline, not just that `db3::Reader` can open one.
        let dir = temp_dir("schema-v1");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");

        let conn =
            oxisql_sqlite_compat::blocking::SqliteConnectionBlocking::open(input.to_str().unwrap())
                .unwrap();
        conn.execute(
            "CREATE TABLE topics(id INTEGER PRIMARY KEY, name TEXT NOT NULL, type TEXT NOT NULL, \
             serialization_format TEXT NOT NULL)",
            &[],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, topic_id INTEGER NOT NULL, \
             timestamp INTEGER NOT NULL, data BLOB NOT NULL)",
            &[],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topics (name, type, serialization_format) VALUES ($1, $2, $3)",
            &[&"/legacy", &"std_msgs/msg/String", &"cdr"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (topic_id, timestamp, data) VALUES ($1, $2, $3)",
            &[&1i64, &42i64, &vec![7u8, 8, 9]],
        )
        .unwrap();
        drop(conn);

        let report = db3_to_arec(&input, &output).unwrap();
        assert_eq!(report.topics, 1);
        assert_eq!(report.messages, 1);

        let (mut reader, _) = ArecReader::open_or_recover(&output).unwrap();
        let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        assert_eq!(entries[0].node.as_str(), "_legacy");
        assert_eq!(entries[0].payload, vec![7, 8, 9]);
        assert_eq!(entries[0].hlc(), HlcTimestamp::new(42, 0));
    }

    proptest::proptest! {
        // Real file I/O per case — see the identical note in
        // `arec_to_db3`'s own property test.
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn db3_to_arec_to_db3_preserves_every_topic_and_message(
            topics in proptest::collection::vec(
                "[a-zA-Z][a-zA-Z0-9]{0,7}",
                1..5,
            ),
            messages in proptest::collection::vec(
                (0usize..5, 0i64..1_000_000_000, proptest::collection::vec(any::<u8>(), 0..32)),
                0..10,
            ),
        ) {
            let dir = temp_dir("proptest-round-trip");
            let db3_a = dir.join("a.db3");
            let arec_b = dir.join("b.arec");
            let db3_c = dir.join("c.db3");

            let mut writer = Db3Writer::create(&db3_a, "jazzy").unwrap();
            let topic_names: Vec<String> = topics.iter().map(|t| format!("/{t}")).collect();
            let topic_ids: Vec<i64> = topic_names
                .iter()
                .map(|name| writer.create_topic(&TopicRecord::new(name.clone(), "", "cdr")).unwrap())
                .collect();
            let mut expected: BTreeMap<String, Vec<(i64, Vec<u8>)>> = BTreeMap::new();
            let mut rows = Vec::new();
            for (topic_index, timestamp_ns, payload) in &messages {
                let index = topic_index % topic_ids.len().max(1);
                if topic_ids.is_empty() {
                    continue;
                }
                rows.push(Ok((topic_ids[index], *timestamp_ns, payload.clone())));
                expected
                    .entry(topic_names[index].clone())
                    .or_default()
                    .push((*timestamp_ns, payload.clone()));
            }
            writer.write_messages(rows.into_iter()).unwrap();
            writer.finish().unwrap();

            db3_to_arec(&db3_a, &arec_b).unwrap();
            crate::convert::arec_to_db3::arec_to_db3(&arec_b, &db3_c).unwrap();

            let reader = Db3Reader::open(&db3_c).unwrap();
            // `create_topic` is idempotent on name (first write wins), so
            // a duplicate in the generated `topics` list collapses to one
            // real topic — compare against the distinct count, not the
            // raw generated length.
            let distinct_topic_count = topic_names.iter().collect::<std::collections::BTreeSet<_>>().len();
            prop_assert_eq!(reader.topics().len(), distinct_topic_count);
            let mut actual: BTreeMap<String, Vec<(i64, Vec<u8>)>> = BTreeMap::new();
            let messages_out: Vec<_> = reader.iter_messages().collect::<Result<Vec<_>, _>>().unwrap();
            for message in messages_out {
                let name = reader.topic_by_id(message.topic_id).unwrap().topic.clone();
                actual.entry(name).or_default().push((message.timestamp_ns, message.data));
            }
            for values in expected.values_mut().chain(actual.values_mut()) {
                values.sort();
            }
            prop_assert_eq!(actual, expected);
        }
    }

    #[test]
    fn a_malformed_sidecar_node_id_surfaces_a_typed_id_error() {
        // A hand-corrupted `custom_data` sidecar (as if written by a
        // non-astrs-rosbag tool, or by a future bug) naming an astrs node
        // id outside `NodeId`'s grammar — proving `db3_to_arec` reports
        // `RosbagError::Id` via `?` rather than panicking on the
        // `NodeId::new` it recovers a sidecar entry's `node` field
        // through, exercised nowhere else in this module's test suite.
        let dir = temp_dir("bad-sidecar-id");
        let input = dir.join("session.db3");
        let output = dir.join("session.arec");

        let mut writer = Db3Writer::create(&input, "jazzy").unwrap();
        let record = TopicRecord::new("/scan", "", "cdr");
        writer.create_topic(&record).unwrap();
        let manifest = RosbagTopicManifest {
            format_version: SIDECAR_FORMAT_VERSION,
            ros_distro: String::new(),
            dataflow: None,
            hlc_epoch: None,
            topics: vec![SidecarTopic {
                node: "not a valid node id".to_owned(),
                output: OUTPUT_NAME.to_owned(),
                record,
            }],
        };
        writer.set_custom_data(SIDECAR_KEY, manifest.to_manifest_yaml().unwrap());
        writer.finish().unwrap();

        let err = db3_to_arec(&input, &output).unwrap_err();
        assert!(matches!(err, RosbagError::Id(_)), "{err:?}");
    }
}
