//! The shared "one topic" vocabulary between `.db3`, `.mcap`, and the
//! `.arec` topic-manifest sidecar (blueprint §10.6).
//!
//! [`TopicRecord`] is the format-agnostic shape every reader/writer in
//! this crate converges on. [`SidecarTopic`] adds the two fields only the
//! `.arec` conversion needs (which `astrs_wire::NodeId`/`DataId` an
//! `.arec`'s entries for this topic were written under), and
//! [`RosbagTopicManifest`] is the small YAML document [`crate::convert`]
//! embeds in [`astrs_recording::Header::manifest_yaml`] so a later
//! `arec -> bag` conversion recovers every topic's type, serialization
//! format and QoS losslessly rather than reconstructing them from the
//! entries alone.

use std::collections::BTreeMap;

use astrs_wire::NodeId;
use serde::{Deserialize, Serialize};

/// The `serialization_format` / mcap `message_encoding` this crate writes
/// when a payload's first four bytes did not decode as a CDR encapsulation
/// header ([`astrs_cdr::EncapsulationHeader::from_bytes`]).
///
/// Never fabricated as `"cdr"`: a rosbag2/mcap consumer trusts that field
/// to choose a deserializer, and mislabeling an opaque payload (an
/// `.arec` entry from a non-ROS2 dataflow, an empty payload, a truncated
/// capture) would hand it a stream it cannot decode instead of an honest
/// "unknown".
pub const RAW_SERIALIZATION_FORMAT: &str = "astrs-raw";

/// One topic's rosbag2/mcap metadata — the fields every format in this
/// crate can express, independent of which one it came from.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::TopicRecord;
///
/// let topic = TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr");
/// assert_eq!(topic.topic, "/scan");
/// assert!(topic.offered_qos_profiles.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicRecord {
    /// The original ROS topic name, e.g. `/scan` — never sanitized, never
    /// truncated.
    pub topic: String,
    /// The ROS message type, e.g. `sensor_msgs/msg/LaserScan`. Empty when
    /// unknown — a topic synthesized from `.arec` entries with no sidecar,
    /// or an mcap channel with `schema_id == 0`.
    #[serde(default)]
    pub r#type: String,
    /// rosbag2's `topics.serialization_format` column / mcap's
    /// `Channel.message_encoding` field. `"cdr"` for real ROS 2 topics;
    /// [`RAW_SERIALIZATION_FORMAT`] when this crate could not confirm it.
    pub serialization_format: String,
    /// QoS profiles offered by publishers on this topic, in rosbag2's own
    /// order. Almost every real recording has exactly one entry; more than
    /// one means multiple publishers offered different QoS.
    #[serde(default)]
    pub offered_qos_profiles: Vec<QosProfile>,
    /// rosbag2 schema v4's `type_description_hash` (topics table / message
    /// definitions table). Left empty when the source format has no
    /// equivalent (mcap has no directly corresponding field) — fabricating
    /// a hash under this name would be worse than an honest blank, since a
    /// real rosbag2 reader treats it as a content-addressed cache key.
    #[serde(default)]
    pub type_description_hash: String,
}

impl TopicRecord {
    /// A topic record with no QoS profiles and no type-description hash.
    #[must_use]
    pub fn new(
        topic: impl Into<String>,
        r#type: impl Into<String>,
        serialization_format: impl Into<String>,
    ) -> Self {
        Self {
            topic: topic.into(),
            r#type: r#type.into(),
            serialization_format: serialization_format.into(),
            offered_qos_profiles: Vec::new(),
            type_description_hash: String::new(),
        }
    }
}

/// A `sec`/`nsec` duration, exactly as `rclcpp::Duration` round-trips
/// through rosbag2's `metadata.yaml` (`yaml.hpp`'s
/// `convert<rclcpp::Duration>`) and the mcap ROS 2 profile's QoS block
/// (mcap registry §"Profiles" → ROS2 → Channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QosDuration {
    /// Whole seconds.
    pub sec: i32,
    /// The remaining nanoseconds (always `0..1_000_000_000` when produced
    /// by a well-behaved writer; read as-is otherwise, never validated).
    pub nsec: u32,
}

/// One QoS profile offered by a publisher on a topic.
///
/// The four enum-shaped fields (`history`, `reliability`, `durability`,
/// `liveliness`) are carried as raw strings rather than a closed Rust
/// enum: the permitted value set has evolved across ROS 2 distros (the
/// mcap registry's own worked example lists only two `history` values
/// against four for the others), and this crate's job is lossless
/// round-trip, not policy validation — a value this crate has never seen
/// still comes back out byte-for-byte instead of being coerced into an
/// `Unknown` bucket or rejected outright.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::{QosDuration, QosProfile};
///
/// let qos = QosProfile::best_effort_volatile();
/// assert_eq!(qos.reliability, "best_effort");
/// assert_eq!(qos.deadline, QosDuration::default());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QosProfile {
    /// `"keep_last"`, `"keep_all"`, `"system_default"`, or `"unknown"`.
    pub history: String,
    /// The queue depth `history: keep_last` retains. Meaningless (but
    /// still round-tripped) for `keep_all`.
    pub depth: i64,
    /// `"reliable"`, `"best_effort"`, `"system_default"`, or `"unknown"`.
    pub reliability: String,
    /// `"volatile"`, `"transient_local"`, `"system_default"`, or
    /// `"unknown"`.
    pub durability: String,
    /// How long between messages a subscriber expects, at most.
    #[serde(default)]
    pub deadline: QosDuration,
    /// How long a message stays valid after publication.
    #[serde(default)]
    pub lifespan: QosDuration,
    /// `"automatic"`, `"manual_by_topic"`, `"system_default"`, or
    /// `"unknown"`.
    pub liveliness: String,
    /// How long a liveliness assertion remains valid.
    #[serde(default)]
    pub liveliness_lease_duration: QosDuration,
    /// Whether ROS's `rt`/`rq`/`rr` topic-name mangling was skipped for
    /// this publisher.
    #[serde(default)]
    pub avoid_ros_namespace_conventions: bool,
}

impl QosProfile {
    /// `RELIABLE` / `VOLATILE` / `KEEP_LAST(10)` — rclcpp's
    /// `rmw_qos_profile_default` shape, the profile almost every ROS 2
    /// publisher uses unless it asks for something else.
    #[must_use]
    pub fn reliable_default() -> Self {
        Self {
            history: "keep_last".to_owned(),
            depth: 10,
            reliability: "reliable".to_owned(),
            durability: "volatile".to_owned(),
            deadline: QosDuration::default(),
            lifespan: QosDuration::default(),
            liveliness: "system_default".to_owned(),
            liveliness_lease_duration: QosDuration::default(),
            avoid_ros_namespace_conventions: false,
        }
    }

    /// `BEST_EFFORT` / `VOLATILE` / `KEEP_LAST(10)` — the sensor-data QoS
    /// shape (`rclcpp::SensorDataQoS`) most camera/lidar topics use.
    #[must_use]
    pub fn best_effort_volatile() -> Self {
        Self {
            reliability: "best_effort".to_owned(),
            ..Self::reliable_default()
        }
    }
}

/// The `.arec` output id every topic this crate converts writes its
/// entries under. One output per topic (rather than per-message-type
/// fan-out) keeps the node/output pair a stable, minimal key: the
/// message's ROS type already lives in [`TopicRecord::type`].
pub const OUTPUT_NAME: &str = "data";

/// Parses a YAML sequence of [`QosProfile`] — the shape [`crate::db3`]'s
/// `topics.offered_qos_profiles` column and [`crate::mcap`]'s
/// `Channel.metadata["offered_qos_profiles"]` both use.
///
/// This crate's own [`crate::db3::Writer`] always writes that exact
/// shape, and it is the mcap ROS 2 profile's own documented convention.
/// A real `rosbag2_storage_sqlite3`-written `.db3` may instead carry
/// `rosbag2_storage::serialize_rclcpp_qos_vector`'s own text encoding — a
/// detail this crate has not independently verified against C++ source
/// (astrs.md §18 forbids C++-derived fixtures in-repo to begin with) — so
/// a value that does not parse as the expected shape degrades to an
/// empty QoS list rather than either a hard error or a guessed
/// reinterpretation. An empty (or whitespace-only) string is a valid,
/// non-degraded "no QoS profiles recorded" — `Some(Vec::new())`, not
/// `None`.
#[must_use]
pub(crate) fn parse_qos_profiles_yaml(text: &str) -> Option<Vec<QosProfile>> {
    if text.trim().is_empty() {
        return Some(Vec::new());
    }
    astrs_yaml::from_str(text).ok()
}

/// One topic's entry in the `.arec` topic-manifest sidecar
/// ([`RosbagTopicManifest`]): a [`TopicRecord`] plus which
/// [`astrs_wire::NodeId`]/[`astrs_wire::DataId`] pair its entries were
/// written under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidecarTopic {
    /// The `.arec` producer node id this topic's entries carry
    /// ([`astrs_recording::Entry::node`]).
    pub node: String,
    /// The `.arec` producer output id this topic's entries carry
    /// ([`astrs_recording::Entry::output`]) — [`OUTPUT_NAME`] for every
    /// topic this crate writes; kept as a field (rather than assumed)
    /// so a sidecar this crate reads back, but did not itself write, is
    /// never misread.
    pub output: String,
    /// The topic's rosbag2/mcap metadata.
    #[serde(flatten)]
    pub record: TopicRecord,
}

/// The top-level YAML key [`RosbagTopicManifest`] is embedded under
/// inside [`astrs_recording::Header::manifest_yaml`].
///
/// Deliberately **not** shaped like a real dataflow manifest (blueprint
/// §8.2 requires a top-level `nodes:` sequence, parsed
/// `deny_unknown_fields`): a bag-converted `.arec` never ran as an AstRS
/// dataflow, so claiming one would be dishonest. `astrs-tui`'s replay view
/// already treats an unparseable or foreign `manifest_yaml` as "no Graph
/// tab content" rather than an error
/// (`astrs-tui/src/view/mod.rs::parse_manifest`), so this key being
/// unrecognized there is the intended, already-handled outcome, not a
/// compatibility risk.
pub const SIDECAR_KEY: &str = "astrs_rosbag_topics";

/// The `.arec` topic-manifest sidecar's format version this crate writes.
pub const SIDECAR_FORMAT_VERSION: u32 = 1;

/// The `.arec` topic-manifest sidecar: every topic a `bag -> arec`
/// conversion saw, serialized as YAML under [`SIDECAR_KEY`] into
/// [`astrs_recording::Header::manifest_yaml`] so a later `arec -> bag`
/// conversion recovers each one's real name, type, serialization format
/// and QoS instead of reconstructing them from the entries alone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RosbagTopicManifest {
    /// This sidecar document's own format version. `1` for everything this
    /// crate writes; a future incompatible change bumps it rather than
    /// silently reinterpreting an old field.
    pub format_version: u32,
    /// The ROS distro the source bag declared (rosbag2 `metadata.yaml`'s
    /// `ros_distro`, or the mcap `Header.library` string) — empty when
    /// unknown.
    #[serde(default)]
    pub ros_distro: String,
    /// The source `.arec`'s [`astrs_wire::DataflowId`], as
    /// [`ToString`]/[`std::str::FromStr`] render it — present only when
    /// this sidecar was written by [`crate::convert::arec_to_db3()`] (a
    /// `bag → .arec` conversion of a bag that was never an `.arec` has no
    /// dataflow id to recover, so it is `None` there and a fresh one is
    /// minted instead). Recovering it rather than always minting a fresh
    /// one is what makes an `.arec → bag → .arec` round trip preserve the
    /// dataflow's identity, not just its content.
    #[serde(default)]
    pub dataflow: Option<String>,
    /// The source `.arec`'s [`astrs_time::HlcTimestamp`] header epoch, in
    /// its canonical `"<physical_ns>-<logical>"` form — present under the
    /// same condition as [`Self::dataflow`], and for the same round-trip
    /// reason.
    #[serde(default)]
    pub hlc_epoch: Option<String>,
    /// One entry per topic, in first-seen order.
    pub topics: Vec<SidecarTopic>,
}

impl RosbagTopicManifest {
    /// A fresh, empty sidecar at [`SIDECAR_FORMAT_VERSION`], with no
    /// recovered dataflow identity (see [`Self::dataflow`]).
    #[must_use]
    pub fn new(ros_distro: impl Into<String>) -> Self {
        Self {
            format_version: SIDECAR_FORMAT_VERSION,
            ros_distro: ros_distro.into(),
            dataflow: None,
            hlc_epoch: None,
            topics: Vec::new(),
        }
    }

    /// Serializes this sidecar as the complete `manifest_yaml` string a
    /// `.arec` [`astrs_recording::Header`] should carry: a single-key YAML
    /// document, `{` [`SIDECAR_KEY`] `: <this sidecar> }`.
    ///
    /// # Errors
    ///
    /// [`astrs_yaml::Error`] only on an allocation failure — every field
    /// here is representable as YAML by construction.
    pub fn to_manifest_yaml(&self) -> Result<String, astrs_yaml::Error> {
        let mut wrapper = BTreeMap::new();
        wrapper.insert(SIDECAR_KEY, self);
        astrs_yaml::to_string(&wrapper)
    }

    /// Recovers a sidecar previously embedded by [`Self::to_manifest_yaml`].
    ///
    /// Returns `None` — never an error — for YAML that does not parse, or
    /// that parses but has no [`SIDECAR_KEY`] top-level entry: both mean
    /// "no sidecar", the same graceful-fallback signal an empty
    /// `manifest_yaml` gives, so [`crate::convert::arec_to_db3()`] can always
    /// fall back to reconstructing topics from the entries alone,
    /// uniformly.
    #[must_use]
    pub fn from_manifest_yaml(yaml: &str) -> Option<Self> {
        if yaml.trim().is_empty() {
            return None;
        }
        let mut wrapper: BTreeMap<String, Self> = astrs_yaml::from_str(yaml).ok()?;
        wrapper.remove(SIDECAR_KEY)
    }
}

/// Assigns each topic a stable [`astrs_wire::NodeId`]
/// ([`NodeId::sanitized`] of its name), disambiguating collisions rather
/// than silently merging two distinct topics into one node.
///
/// [`NodeId::sanitized`] is not injective — `/a/b` and `/a_b` both
/// sanitize to `_a_b` — so a topic landing on an id already assigned gets
/// `.2`, `.3`, … appended (in the order `topic_names` iterates), and a
/// warning naming both topics and the disambiguated id actually used is
/// pushed to `warnings`, so a caller printing a conversion report never
/// silently loses a topic to an unannounced merge.
///
/// # Examples
///
/// ```
/// use astrs_rosbag::assign_node_ids;
///
/// let mut warnings = Vec::new();
/// let assigned = assign_node_ids(["/a/b", "/a_b"].into_iter(), &mut warnings);
/// assert_eq!(assigned[0].as_str(), "_a_b");
/// assert_eq!(assigned[1].as_str(), "_a_b.2");
/// assert_eq!(warnings.len(), 1);
/// ```
pub fn assign_node_ids<'a>(
    topic_names: impl Iterator<Item = &'a str>,
    warnings: &mut Vec<String>,
) -> Vec<NodeId> {
    let mut seen: BTreeMap<String, (String, u32)> = BTreeMap::new();
    let mut assigned = Vec::new();
    for name in topic_names {
        let base = NodeId::sanitized(name).into_string();
        let id_string = match seen.get_mut(&base) {
            None => {
                seen.insert(base.clone(), (name.to_owned(), 1));
                base
            }
            Some((first_topic, count)) => {
                *count += 1;
                let disambiguated = format!("{base}.{count}");
                warnings.push(format!(
                    "topics `{first_topic}` and `{name}` both sanitize to astrs node id \
                     `{base}`; `{name}` was assigned `{disambiguated}` instead"
                ));
                disambiguated
            }
        };
        // `base` is already grammar-valid (`NodeId::sanitized`'s own
        // postcondition), and `.`/digits are in-grammar, so the only way
        // this second `sanitized` call can still change anything is by
        // re-truncating at `NodeId::MAX_LEN` for a pathologically long
        // topic name — handled the same total, non-panicking way
        // `sanitized` always handles it.
        assigned.push(NodeId::sanitized(&id_string));
    }
    assigned
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn topic_record_round_trips_through_yaml() {
        let mut topic = TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr");
        topic
            .offered_qos_profiles
            .push(QosProfile::reliable_default());
        topic.type_description_hash = "RIHS01_deadbeef".to_owned();
        let yaml = astrs_yaml::to_string(&topic).unwrap();
        let back: TopicRecord = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, topic);
    }

    #[test]
    fn qos_profile_round_trips_and_preserves_unknown_enum_text() {
        let mut qos = QosProfile::best_effort_volatile();
        // A value this crate never enumerates: round-trip must still be
        // exact, not coerced to a nearest-known bucket.
        qos.history = "some_future_policy".to_owned();
        let yaml = astrs_yaml::to_string(&qos).unwrap();
        let back: QosProfile = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.history, "some_future_policy");
        assert_eq!(back, qos);
    }

    #[test]
    fn sidecar_manifest_round_trips_through_its_embedded_form() {
        let mut manifest = RosbagTopicManifest::new("jazzy");
        manifest.topics.push(SidecarTopic {
            node: "_scan".to_owned(),
            output: OUTPUT_NAME.to_owned(),
            record: TopicRecord::new("/scan", "sensor_msgs/msg/LaserScan", "cdr"),
        });
        let yaml = manifest.to_manifest_yaml().unwrap();
        assert!(yaml.contains(SIDECAR_KEY));
        let back = RosbagTopicManifest::from_manifest_yaml(&yaml).unwrap();
        assert_eq!(back, manifest);
    }

    #[test]
    fn dataflow_and_hlc_epoch_round_trip_when_present() {
        let mut manifest = RosbagTopicManifest::new("jazzy");
        manifest.dataflow = Some("00000000-0000-0000-0000-00000000002a".to_owned());
        manifest.hlc_epoch = Some("1000-3".to_owned());
        let yaml = manifest.to_manifest_yaml().unwrap();
        let back = RosbagTopicManifest::from_manifest_yaml(&yaml).unwrap();
        assert_eq!(back.dataflow, manifest.dataflow);
        assert_eq!(back.hlc_epoch, manifest.hlc_epoch);
    }

    #[test]
    fn a_sidecar_written_before_dataflow_and_hlc_epoch_existed_still_parses() {
        // `#[serde(default)]` must keep an older, two-field sidecar
        // (`format_version`/`ros_distro`/`topics` only, no `dataflow` or
        // `hlc_epoch` keys) readable — forward compatibility for a sidecar
        // this crate wrote before those fields were added.
        let yaml =
            format!("{SIDECAR_KEY}:\n  format_version: 1\n  ros_distro: jazzy\n  topics: []\n");
        let manifest = RosbagTopicManifest::from_manifest_yaml(&yaml).unwrap();
        assert_eq!(manifest.dataflow, None);
        assert_eq!(manifest.hlc_epoch, None);
    }

    #[test]
    fn from_manifest_yaml_is_none_for_empty_or_foreign_yaml() {
        assert!(RosbagTopicManifest::from_manifest_yaml("").is_none());
        assert!(RosbagTopicManifest::from_manifest_yaml("   \n").is_none());
        // Garbage that is not even valid YAML.
        assert!(RosbagTopicManifest::from_manifest_yaml(": : :").is_none());
        // A real dataflow manifest's own top-level shape (blueprint §8.2):
        // present, parses as YAML, but has no `SIDECAR_KEY` entry.
        assert!(
            RosbagTopicManifest::from_manifest_yaml("nodes:\n  - id: camera\n    path: ./camera\n")
                .is_none()
        );
    }

    #[test]
    fn assign_node_ids_is_identity_when_every_name_is_already_distinct() {
        let mut warnings = Vec::new();
        let assigned = assign_node_ids(["/scan", "/camera/image"].into_iter(), &mut warnings);
        assert_eq!(assigned[0].as_str(), "_scan");
        assert_eq!(assigned[1].as_str(), "_camera_image");
        assert!(warnings.is_empty());
    }

    #[test]
    fn assign_node_ids_disambiguates_a_three_way_collision() {
        let mut warnings = Vec::new();
        // Three distinct topic names that all sanitize to `_a_b`: both
        // `/` and the literal space are outside `NodeId`'s grammar and
        // become `_`.
        let assigned = assign_node_ids(["/a/b", "/a_b", " a b"].into_iter(), &mut warnings);
        assert_eq!(assigned[0].as_str(), "_a_b");
        assert_eq!(assigned[1].as_str(), "_a_b.2");
        assert_eq!(assigned[2].as_str(), "_a_b.3");
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn assign_node_ids_handles_an_empty_input() {
        let mut warnings = Vec::new();
        let assigned = assign_node_ids(std::iter::empty(), &mut warnings);
        assert!(assigned.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn raw_serialization_format_is_never_the_string_cdr() {
        assert_ne!(RAW_SERIALIZATION_FORMAT, "cdr");
    }

    #[test]
    fn parse_qos_profiles_yaml_accepts_an_empty_string_as_zero_profiles() {
        assert_eq!(parse_qos_profiles_yaml(""), Some(Vec::new()));
        assert_eq!(parse_qos_profiles_yaml("   \n"), Some(Vec::new()));
    }

    #[test]
    fn parse_qos_profiles_yaml_accepts_a_real_sequence() {
        let yaml = astrs_yaml::to_string(&vec![QosProfile::reliable_default()]).unwrap();
        let parsed = parse_qos_profiles_yaml(&yaml).unwrap();
        assert_eq!(parsed, vec![QosProfile::reliable_default()]);
    }

    #[test]
    fn parse_qos_profiles_yaml_degrades_the_exact_string_mcaps_own_channel_metadata_test_uses() {
        // Byte-for-byte the same fixture `mcap::records::data`'s
        // `channel_parses_the_spec_byte_layout_including_its_metadata_map`
        // test embeds in a `Channel.metadata["offered_qos_profiles"]`
        // value — proving the reuse this function enables (db3's column
        // and mcap's channel metadata sharing one parser) degrades that
        // fixture identically here: it names only `history`/`depth`, so it
        // is not a complete `QosProfile` (`reliability`/`durability`/
        // `liveliness` are required, not `#[serde(default)]`), and a
        // partial-looking real-world value degrades to `None`, not a
        // panic or a half-populated profile.
        let yaml = "- history: keep_last\n  depth: 10\n";
        assert_eq!(parse_qos_profiles_yaml(yaml), None);
    }

    #[test]
    fn parse_qos_profiles_yaml_degrades_unparseable_text_to_none() {
        assert_eq!(
            parse_qos_profiles_yaml("not: [valid, for, our, shape"),
            None
        );
    }
}
