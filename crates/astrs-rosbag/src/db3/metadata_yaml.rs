//! rosbag2's `metadata.yaml` sidecar — the file every `.db3` bag carries
//! alongside its database (`rosbag2_storage::BagMetadata`,
//! `rosbag2_storage/metadata_io.cpp` + `yaml.hpp`), reproduced here as
//! plain `serde`/`astrs_yaml` rather than the C++ `yaml-cpp` conversions
//! it mirrors.
//!
//! # Single-file bags only
//!
//! rosbag2 bags are, in general, a *directory* holding `metadata.yaml`
//! plus one or more numbered `.db3` files (split bags). This crate's
//! [`Reader`](crate::db3::Reader)/[`Writer`](crate::db3::Writer) operate on
//! exactly one `.db3` file, matching `astrs bag convert`'s file-to-file
//! CLI shape — the overwhelmingly common case (`ros2 bag record` with no
//! explicit size/duration split). [`BagMetadata::single_file`] builds the
//! one-element `relative_file_paths`/`files` a single-file bag's
//! `metadata.yaml` carries.
//!
//! # Read tolerance vs. write target
//!
//! [`BagMetadata::version`] [`CURRENT_METADATA_VERSION`] (9) is what
//! [`Writer`](crate::db3::Writer) always writes: `offered_qos_profiles` as
//! a structured YAML sequence of QoS blocks (`yaml.hpp`'s
//! `convert<std::vector<rclcpp::QoS>>`, version ≥ 9). Reading tolerates
//! every older version's *other* fields via `#[serde(default)]` (a field
//! introduced at version 6 simply defaults when absent from an older
//! file, without this module replicating each version threshold's exact
//! C++ conditional by hand) — but versions below
//! [`STRUCTURED_QOS_MIN_VERSION`] serialized `offered_qos_profiles` as an
//! **opaque string** (`rosbag2_storage::serialize_rclcpp_qos_vector`'s
//! own text encoding, itself parsed by a C++ subsystem this crate does not
//! reimplement). This module's `deserialize_qos_profiles` recognizes that shape and
//! degrades to an empty QoS list rather than failing the whole document —
//! an honest, stated limit (every other field, including the topic's
//! name/type/serialization_format, still reads correctly) rather than a
//! guessed reimplementation of an unverified legacy text format.

use serde::{Deserialize, Serialize};

use crate::topic::{QosProfile, TopicRecord};

/// `BagMetadata::version`'s current value
/// (`rosbag2_storage/bag_metadata.hpp`: `int version = 9;`).
/// [`Writer`](crate::db3::Writer) always writes this version.
pub const CURRENT_METADATA_VERSION: i64 = 9;

/// The first metadata.yaml version whose `offered_qos_profiles` is a
/// structured YAML sequence of QoS blocks rather than an opaque string —
/// see the module docs' read-tolerance note.
pub const STRUCTURED_QOS_MIN_VERSION: i64 = 9;

/// `rosbag2_storage`'s fixed storage identifier for the SQLite backend —
/// `BagMetadata::storage_identifier`'s value for every bag this crate
/// writes.
pub const STORAGE_IDENTIFIER: &str = "sqlite3";

/// `{ nanoseconds_since_epoch: <u64> }` — `yaml.hpp`'s
/// `convert<std::chrono::time_point<std::chrono::high_resolution_clock>>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StartingTime {
    /// Nanoseconds since the Unix epoch.
    pub nanoseconds_since_epoch: u64,
}

/// `{ nanoseconds: <u64> }` — `yaml.hpp`'s
/// `convert<std::chrono::nanoseconds>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DurationNanos {
    /// The duration, in nanoseconds.
    pub nanoseconds: u64,
}

/// Accepts a YAML sequence of [`QosProfile`] (metadata.yaml version ≥
/// [`STRUCTURED_QOS_MIN_VERSION`]) and degrades any other shape — in
/// particular the opaque string older versions used — to an empty list,
/// per the module docs' read-tolerance note.
fn deserialize_qos_profiles<'de, D>(deserializer: D) -> Result<Vec<QosProfile>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = astrs_yaml::Value::deserialize(deserializer)?;
    match value {
        astrs_yaml::Value::Sequence(_) => {
            astrs_yaml::from_value(value).map_err(serde::de::Error::custom)
        }
        _ => Ok(Vec::new()),
    }
}

/// The exact YAML shape of one `topics_with_message_count[].topic_metadata`
/// entry (`rosbag2_storage::TopicMetadata` via `yaml.hpp`).
///
/// Field names match the real rosbag2 keys byte-for-byte — in particular
/// `name`, not [`TopicRecord::topic`] — because this **is** the wire
/// format real `ros2 bag info`/`rosbag2_py` reads; [`crate::TopicRecord`]
/// is this crate's own internal, format-agnostic shape, converted to and
/// from this one at the metadata.yaml boundary only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YamlTopicMetadata {
    /// The ROS topic name — [`TopicRecord::topic`].
    pub name: String,
    /// The ROS message type — [`TopicRecord::type`].
    #[serde(default)]
    pub r#type: String,
    /// [`TopicRecord::serialization_format`].
    pub serialization_format: String,
    /// [`TopicRecord::offered_qos_profiles`].
    #[serde(default, deserialize_with = "deserialize_qos_profiles")]
    pub offered_qos_profiles: Vec<QosProfile>,
    /// [`TopicRecord::type_description_hash`].
    #[serde(default)]
    pub type_description_hash: String,
}

impl From<&TopicRecord> for YamlTopicMetadata {
    fn from(topic: &TopicRecord) -> Self {
        Self {
            name: topic.topic.clone(),
            r#type: topic.r#type.clone(),
            serialization_format: topic.serialization_format.clone(),
            offered_qos_profiles: topic.offered_qos_profiles.clone(),
            type_description_hash: topic.type_description_hash.clone(),
        }
    }
}

impl From<YamlTopicMetadata> for TopicRecord {
    fn from(yaml: YamlTopicMetadata) -> Self {
        Self {
            topic: yaml.name,
            r#type: yaml.r#type,
            serialization_format: yaml.serialization_format,
            offered_qos_profiles: yaml.offered_qos_profiles,
            type_description_hash: yaml.type_description_hash,
        }
    }
}

/// One topic's message count within the bag — `TopicInformation`
/// (`rosbag2_storage/bag_metadata.hpp`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicInformation {
    /// The topic's own metadata.
    pub topic_metadata: YamlTopicMetadata,
    /// How many messages on this topic the bag holds.
    pub message_count: u64,
}

/// One physical `.db3` file within the bag — `FileInformation`
/// (`rosbag2_storage/bag_metadata.hpp`), present from metadata.yaml
/// version 5 onward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInformation {
    /// The file's path, relative to the bag directory (`metadata.yaml`'s
    /// own directory) — a bare filename for the single-file bags this
    /// crate produces.
    pub path: String,
    /// The earliest message's log time in this file.
    pub starting_time: StartingTime,
    /// This file's message time span.
    pub duration: DurationNanos,
    /// How many messages this file holds.
    pub message_count: u64,
}

/// `rosbag2_storage::BagMetadata` — the complete content of one
/// `metadata.yaml`.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::db3::metadata_yaml::{BagMetadata, StartingTime, DurationNanos};
/// use astrs_rosbag::TopicRecord;
///
/// let metadata = BagMetadata::single_file(
///     "session_0.db3",
///     StartingTime { nanoseconds_since_epoch: 1_000 },
///     DurationNanos { nanoseconds: 5_000 },
///     "jazzy",
///     vec![(TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr"), 3)],
/// );
/// let yaml = metadata.to_yaml().unwrap();
/// let back = BagMetadata::from_yaml_str(&yaml).unwrap();
/// assert_eq!(back, metadata);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BagMetadata {
    /// The metadata.yaml format version.
    pub version: i64,
    /// The storage plugin — [`STORAGE_IDENTIFIER`] for every bag this
    /// crate writes.
    pub storage_identifier: String,
    /// The bag's total message time span.
    pub duration: DurationNanos,
    /// The earliest message's log time across the whole bag.
    pub starting_time: StartingTime,
    /// Total message count across every topic.
    pub message_count: u64,
    /// Per-topic message counts and metadata.
    pub topics_with_message_count: Vec<TopicInformation>,
    /// The compression algorithm applied to message data (`""` — this
    /// crate never compresses `.db3` message blobs; SQLite page
    /// compression is out of scope, and rosbag2's own `data`-blob
    /// compression is a `rosbag2_compression` feature this crate does not
    /// implement).
    #[serde(default)]
    pub compression_format: String,
    /// The compression granularity (`""`, `"file"`, or `"message"` in real
    /// rosbag2; always `""` here, see [`Self::compression_format`]).
    #[serde(default)]
    pub compression_mode: String,
    /// The `.db3` file name(s), relative to this `metadata.yaml`'s own
    /// directory.
    pub relative_file_paths: Vec<String>,
    /// Per-file statistics (version ≥ 5).
    #[serde(default)]
    pub files: Vec<FileInformation>,
    /// Free-form key/value data (version ≥ 6). Always empty here; kept so
    /// a bag this crate reads and re-writes does not silently drop a
    /// producer's custom data.
    #[serde(default)]
    pub custom_data: std::collections::BTreeMap<String, String>,
    /// The ROS distro that wrote the bag (version ≥ 8) — empty when
    /// unknown.
    #[serde(default)]
    pub ros_distro: String,
}

impl BagMetadata {
    /// Builds the `metadata.yaml` content for a single-file bag at
    /// [`CURRENT_METADATA_VERSION`].
    #[must_use]
    pub fn single_file(
        file_name: impl Into<String>,
        starting_time: StartingTime,
        duration: DurationNanos,
        ros_distro: impl Into<String>,
        topics: Vec<(TopicRecord, u64)>,
    ) -> Self {
        let file_name = file_name.into();
        let message_count = topics.iter().map(|(_, count)| *count).sum();
        let topics_with_message_count = topics
            .iter()
            .map(|(topic, count)| TopicInformation {
                topic_metadata: YamlTopicMetadata::from(topic),
                message_count: *count,
            })
            .collect();
        Self {
            version: CURRENT_METADATA_VERSION,
            storage_identifier: STORAGE_IDENTIFIER.to_owned(),
            duration,
            starting_time,
            message_count,
            topics_with_message_count,
            compression_format: String::new(),
            compression_mode: String::new(),
            relative_file_paths: vec![file_name.clone()],
            files: vec![FileInformation {
                path: file_name,
                starting_time,
                duration,
                message_count,
            }],
            custom_data: std::collections::BTreeMap::new(),
            ros_distro: ros_distro.into(),
        }
    }

    /// Serializes as the exact content `metadata.yaml` should hold: a
    /// single top-level `rosbag2_bagfile_information:` key wrapping this
    /// document, matching `MetadataIo::write_metadata`.
    ///
    /// # Errors
    ///
    /// [`astrs_yaml::Error`] only on an allocation failure.
    pub fn to_yaml(&self) -> Result<String, astrs_yaml::Error> {
        let mut wrapper = std::collections::BTreeMap::new();
        wrapper.insert("rosbag2_bagfile_information", self);
        astrs_yaml::to_string(&wrapper)
    }

    /// Parses a `metadata.yaml` file's content.
    ///
    /// # Errors
    ///
    /// [`astrs_yaml::Error`] if the document is not valid YAML, has no
    /// `rosbag2_bagfile_information` top-level key, or a required field is
    /// both absent and not `#[serde(default)]`-eligible (every field
    /// introduced after version 1 is).
    pub fn from_yaml_str(yaml: &str) -> Result<Self, astrs_yaml::Error> {
        let wrapper: std::collections::BTreeMap<String, Self> = astrs_yaml::from_str(yaml)?;
        wrapper
            .into_iter()
            .find_map(|(key, value)| (key == "rosbag2_bagfile_information").then_some(value))
            .ok_or_else(|| serde::de::Error::custom("missing `rosbag2_bagfile_information` key"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sample() -> BagMetadata {
        BagMetadata::single_file(
            "session_0.db3",
            StartingTime {
                nanoseconds_since_epoch: 1_000,
            },
            DurationNanos { nanoseconds: 5_000 },
            "jazzy",
            vec![
                (
                    TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr"),
                    3,
                ),
                (TopicRecord::new("/tf", "tf2_msgs/msg/TFMessage", "cdr"), 7),
            ],
        )
    }

    #[test]
    fn bag_metadata_round_trips_through_yaml() {
        let metadata = sample();
        let yaml = metadata.to_yaml().unwrap();
        assert!(yaml.contains("rosbag2_bagfile_information"));
        let back = BagMetadata::from_yaml_str(&yaml).unwrap();
        assert_eq!(back, metadata);
    }

    #[test]
    fn single_file_sums_message_counts_and_names_the_one_file_twice() {
        let metadata = sample();
        assert_eq!(metadata.message_count, 10);
        assert_eq!(metadata.relative_file_paths, vec!["session_0.db3"]);
        assert_eq!(metadata.files.len(), 1);
        assert_eq!(metadata.files[0].path, "session_0.db3");
        assert_eq!(metadata.files[0].message_count, 10);
    }

    #[test]
    fn topic_metadata_uses_the_real_rosbag2_field_name_not_the_rust_one() {
        let metadata = sample();
        let yaml = metadata.to_yaml().unwrap();
        assert!(yaml.contains("name: /scan"), "{yaml}");
        assert!(!yaml.contains("topic: /scan"), "{yaml}");
    }

    #[test]
    fn a_legacy_string_qos_profile_degrades_to_an_empty_list_not_an_error() {
        // A hand-built document shaped like a pre-version-9 metadata.yaml:
        // `offered_qos_profiles` is a scalar string, not a sequence. A raw
        // string literal, so YAML's own indentation survives verbatim
        // (a normal string's `\` line-continuation would eat it).
        let yaml = r#"rosbag2_bagfile_information:
  version: 4
  storage_identifier: sqlite3
  duration: {nanoseconds: 0}
  starting_time: {nanoseconds_since_epoch: 0}
  message_count: 1
  topics_with_message_count:
    - topic_metadata:
        name: /scan
        type: sensor_msgs/msg/LaserScan
        serialization_format: cdr
        offered_qos_profiles: "- history: keep_last\n  depth: 10"
        type_description_hash: ''
      message_count: 1
  relative_file_paths: [session_0.db3]
"#;
        let metadata = BagMetadata::from_yaml_str(yaml).unwrap();
        assert_eq!(metadata.version, 4);
        assert_eq!(metadata.topics_with_message_count.len(), 1);
        assert!(
            metadata.topics_with_message_count[0]
                .topic_metadata
                .offered_qos_profiles
                .is_empty()
        );
        assert_eq!(
            metadata.topics_with_message_count[0].topic_metadata.name,
            "/scan"
        );
    }

    #[test]
    fn missing_rosbag2_key_is_a_typed_error_not_a_panic() {
        assert!(BagMetadata::from_yaml_str("some_other_key: 1\n").is_err());
        assert!(BagMetadata::from_yaml_str("not: [valid").is_err());
    }

    #[test]
    fn topic_record_round_trips_through_the_yaml_shape() {
        let topic = TopicRecord::new("/imu", "sensor_msgs/msg/Imu", "cdr");
        let yaml_shape = YamlTopicMetadata::from(&topic);
        assert_eq!(yaml_shape.name, "/imu");
        let back: TopicRecord = yaml_shape.into();
        assert_eq!(back, topic);
    }
}
