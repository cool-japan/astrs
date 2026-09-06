//! Shared types and pure logic for `rosbag-reader` (blueprint §10.6, §5.4).
//!
//! ```text
//!   astrs/timer/millis/50 ──► [feeder] ──feed──► [sink] ──► ROSBAG_READER_REPORT (JSON)
//!            (writes+reads a .db3 fixture in code)
//! ```
//!
//! `feeder` writes a tiny fixture `.db3` bag with [`astrs_rosbag::db3::Writer`]
//! — no checked-in binary fixture, no external ROS install — reads it
//! straight back with [`astrs_rosbag::db3::Reader`], and republishes each
//! decoded message on the typed port `feed`, one per timer tick. `sink`
//! tallies what arrives into a [`BagSummary`].
//!
//! The bag carries real `std_msgs/msg/String` messages, CDR-encoded exactly
//! as a real rosbag2 recording would: [`write_fixture_bag`] and
//! [`message_text`] are the pure halves both binaries and this crate's own
//! tests share, so a test can predict a fixture's exact contents without
//! reading the file back.

use std::path::{Path, PathBuf};

use astrs_cdr::Encoding;
use astrs_idl::generated::std_msgs::String as StdString;
use astrs_rosbag::TopicRecord;
use astrs_rosbag::db3::Writer;
use astrs_rosbag::error::RosbagError;
use serde::{Deserialize, Serialize};

/// `feeder`'s tick input, as named in `dataflow.yml`.
pub const TICK_PORT: &str = "tick";
/// The port `feeder` republishes decoded bag messages on, and `sink`'s
/// input.
pub const FEED_PORT: &str = "feed";

/// The ROS 2 topic name the fixture bag's one topic carries.
pub const TOPIC: &str = "/chatter";
/// The ROS 2 message type the fixture bag's one topic carries.
pub const MESSAGE_TYPE: &str = "std_msgs/msg/String";
/// rosbag2's own serialization-format spelling for CDR.
pub const SERIALIZATION_FORMAT: &str = "cdr";

/// The first fixture message's rosbag2 timestamp, nanoseconds since the
/// Unix epoch — an arbitrary but fixed and recognisable instant.
pub const FIXTURE_START_NS: i64 = 1_700_000_000_000_000_000;
/// The gap between two consecutive fixture messages' timestamps.
pub const FIXTURE_STEP_NS: i64 = 100_000_000;

/// Environment variable overriding how many messages the fixture bag holds.
pub const ENV_MESSAGES: &str = "ROSBAG_READER_MESSAGES";
/// How many messages the fixture bag holds by default.
pub const DEFAULT_MESSAGES: u64 = 8;

/// Environment variable naming where `feeder` writes (and re-reads) the
/// fixture bag.
pub const ENV_BAG_PATH: &str = "ROSBAG_READER_BAG_PATH";
/// Environment variable naming the JSON file `sink` writes its
/// [`BagSummary`] to.
pub const ENV_REPORT_PATH: &str = "ROSBAG_READER_REPORT";

/// How many messages this run's fixture bag should hold.
///
/// A value the manifest cannot express falls back to [`DEFAULT_MESSAGES`]
/// rather than failing the node — including zero, which would make
/// `exit_when_nodes_finish` fire before the graph had done anything.
#[must_use]
pub fn message_budget(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_MESSAGES)
}

/// Where `feeder` writes the fixture bag when the manifest names no path.
///
/// Includes the process id so two runs on the same machine never collide —
/// `feeder` truncates whatever it finds there regardless (`Writer::create`'s
/// own contract), but a reader inspecting the bag afterward with `astrs bag
/// info` wants the file that *this* run actually wrote.
#[must_use]
pub fn default_bag_path() -> PathBuf {
    std::env::temp_dir().join(format!("astrs-rosbag-reader-{}.db3", std::process::id()))
}

/// The `.db3` path this run reads and writes.
#[must_use]
pub fn bag_path() -> PathBuf {
    std::env::var(ENV_BAG_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_bag_path, PathBuf::from)
}

/// Where `sink` writes its report when the manifest names no path.
#[must_use]
pub fn default_report_path() -> PathBuf {
    std::env::temp_dir().join("astrs-rosbag-reader-report.json")
}

/// The file this run's sink writes its report to.
#[must_use]
pub fn report_path() -> PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, PathBuf::from)
}

/// The `std_msgs/msg/String` body of fixture message `index`.
///
/// A pure function of the index, deterministic, so both `feeder`'s writer
/// and this crate's own tests agree on exactly what a fixture bag holds.
#[must_use]
pub fn message_text(index: u64) -> String {
    format!("rosbag-reader message {index}")
}

/// The rosbag2 timestamp of fixture message `index`, nanoseconds since the
/// Unix epoch.
#[must_use]
pub fn message_timestamp_ns(index: u64) -> i64 {
    FIXTURE_START_NS + FIXTURE_STEP_NS * i64::try_from(index).unwrap_or(i64::MAX)
}

/// Writes a fresh `.db3` at `path` with one topic ([`TOPIC`]) carrying
/// `count` CDR-encoded `std_msgs/msg/String` messages, exactly the shape
/// [`message_text`]/[`message_timestamp_ns`] predict.
///
/// # Errors
///
/// [`RosbagError`] if the bag cannot be created or written — see
/// [`astrs_rosbag::db3::Writer`]'s own errors.
pub fn write_fixture_bag(path: &Path, count: u64) -> Result<(), RosbagError> {
    let mut writer = Writer::create(path, "jazzy")?;
    let topic_id =
        writer.create_topic(&TopicRecord::new(TOPIC, MESSAGE_TYPE, SERIALIZATION_FORMAT))?;

    let rows = (0..count).map(|index| {
        let body = StdString {
            data: message_text(index),
        };
        let bytes = astrs_cdr::to_vec(&body, Encoding::ROS2).map_err(|source| {
            RosbagError::Internal(format!(
                "fixture message {index} did not CDR-encode: {source}"
            ))
        })?;
        Ok((topic_id, message_timestamp_ns(index), bytes))
    });
    writer.write_messages(rows)?;
    writer.finish()?;
    Ok(())
}

/// What `sink` observed, written as JSON when the run ends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BagSummary {
    /// How many messages `feed` delivered.
    pub messages: u64,
    /// Every message body, in arrival order — decoded, not merely counted,
    /// so a reader can see the pipeline actually carried the bag's own
    /// content.
    pub texts: Vec<String>,
}

impl BagSummary {
    /// Renders the summary as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field
    /// types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A value the manifest cannot express falls back to the default.
    #[test]
    fn the_message_budget_falls_back_to_the_default() {
        assert_eq!(message_budget(Some("3")), 3);
        for raw in [None, Some(""), Some("lots"), Some("0"), Some("-1")] {
            assert_eq!(message_budget(raw), DEFAULT_MESSAGES, "{raw:?}");
        }
    }

    /// Fixture message text and timestamps are deterministic and strictly
    /// increasing.
    #[test]
    fn fixture_messages_are_deterministic_and_ordered() {
        assert_eq!(message_text(0), "rosbag-reader message 0");
        assert_eq!(message_text(3), "rosbag-reader message 3");
        assert!(message_timestamp_ns(1) > message_timestamp_ns(0));
        assert_eq!(
            message_timestamp_ns(2) - message_timestamp_ns(1),
            FIXTURE_STEP_NS
        );
    }

    /// Writing a fixture bag and reading it straight back yields exactly
    /// the messages [`message_text`]/[`message_timestamp_ns`] predict, CDR
    /// round trip included — the central claim this whole example proves.
    #[test]
    fn a_written_fixture_reads_back_exactly() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-rosbag-reader-lib-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.db3");

        write_fixture_bag(&path, 5).unwrap();

        let reader = astrs_rosbag::db3::Reader::open(&path).unwrap();
        assert_eq!(reader.topics().len(), 1);
        assert_eq!(reader.topics()[0].1.topic, TOPIC);
        assert_eq!(reader.topics()[0].1.r#type, MESSAGE_TYPE);

        let messages: Vec<_> = reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 5);
        for (index, message) in messages.iter().enumerate() {
            let index = index as u64;
            assert_eq!(message.timestamp_ns, message_timestamp_ns(index));
            let decoded: StdString = astrs_cdr::from_bytes_tolerant(&message.data).unwrap();
            assert_eq!(decoded.data, message_text(index));
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The report round-trips as JSON.
    #[test]
    fn a_summary_round_trips_as_json() {
        let summary = BagSummary {
            messages: 3,
            texts: vec![message_text(0), message_text(1), message_text(2)],
        };
        let json = summary.to_json().unwrap();
        let parsed: BagSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, summary);
    }

    /// The bag and report paths both default under the temporary directory.
    #[test]
    fn the_paths_default_under_the_temp_dir() {
        assert!(default_bag_path().starts_with(std::env::temp_dir()));
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, wires the typed edge and
    /// names this dataflow.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("std/ros2/v1/StdMsgsString"), "{text}");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("rosbag-reader"));
        assert_eq!(manifest.nodes.len(), 2);
    }
}
