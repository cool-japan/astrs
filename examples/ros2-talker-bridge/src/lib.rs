//! Shared types and pure logic for `ros2-talker-bridge` (blueprint §10.5,
//! §5.4).
//!
//! ```text
//!   [ros2-talker] ──/scan (real DDS/RTPS)──► [lidar-in: ros2:] ──scan──► [ros2-scan-sink]
//!   \_____________ dataflow.yml ____________/ \_______________ bridge.yml _______________/
//! ```
//!
//! `ros2-talker` is a self-hosted loopback RTPS participant — no external
//! ROS 2 install needed — publishing `sensor_msgs/msg/LaserScan` on
//! `/scan`. It is the *only* node in the committed `dataflow.yml`, which is
//! what lets that file set `exit_when_nodes_finish: true` and actually
//! finish on its own (see that file's header comment for why the bridge
//! itself cannot live there too).
//!
//! `bridge.yml`, this directory's companion manifest, declares the other
//! half exactly as blueprint §10.5 writes it: a `ros2:` block (resolved by
//! `astrs-daemon` straight to `bins/astrs-ros2-bridge-node`, §10.5's own
//! bridge) feeding a typed AstRS output that `ros2-scan-sink` decodes and
//! tallies.
//!
//! [`scan_for`] is the one piece of logic both binaries and this crate's
//! own tests share: the deterministic scan `ros2-talker` publishes, and
//! the exact value `ros2-scan-sink` (and this crate's own decode test)
//! expects to receive.

use astrs_ros2::msg::builtin_interfaces::Time;
use astrs_ros2::msg::sensor_msgs::LaserScan;
use astrs_ros2::msg::std_msgs::Header;
use serde::{Deserialize, Serialize};

/// The ROS 2 topic `ros2-talker` publishes on and `lidar-in` bridges.
pub const TOPIC: &str = "/scan";
/// The ROS 2 message type carried on [`TOPIC`].
pub const MESSAGE_TYPE: &str = "sensor_msgs/msg/LaserScan";
/// The `frame_id` every published scan carries.
pub const FRAME_ID: &str = "laser";

/// The DDS domain `ros2-talker` and `lidar-in` (via `bridge.yml`'s
/// `ROS_DOMAIN_ID`) both join.
///
/// Deliberately not the ROS 2 default (`0`) and distinct from
/// `ros2-native-listener`'s own domain, so neither pair collides with a
/// real ROS 2 stack, or with each other, on the same host.
pub const DOMAIN_ID: u32 = 93;

/// Environment variable overriding how many scans `ros2-talker` publishes.
pub const ENV_SCANS: &str = "ROS2_TALKER_SCANS";
/// How many scans `ros2-talker` publishes by default.
pub const DEFAULT_SCANS: u64 = 15;
/// How often `ros2-talker` publishes.
pub const PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// Environment variable naming the JSON file `ros2-scan-sink` writes its
/// [`SinkSummary`] to.
pub const ENV_REPORT_PATH: &str = "ROS2_SCAN_SINK_REPORT";

/// How many scans this run should publish.
///
/// A value the manifest cannot express falls back to [`DEFAULT_SCANS`]
/// rather than failing the node — including zero, which would make
/// `exit_when_nodes_finish` fire before the graph had done anything.
#[must_use]
pub fn scan_budget(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_SCANS)
}

/// Where `ros2-scan-sink` writes its report when the manifest names no
/// path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-ros2-talker-bridge-report.json")
}

/// The file this run's sink writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// The deterministic `sensor_msgs/msg/LaserScan` `ros2-talker` publishes
/// for scan `index`.
///
/// A pure function of the index: both `ros2-talker`'s publisher and
/// `ros2-scan-sink`'s expectations (and this crate's own tests) agree on
/// exactly what crosses the bridge.
#[must_use]
pub fn scan_for(index: u64) -> LaserScan {
    let offset = index as f32;
    LaserScan {
        header: Header {
            stamp: Time {
                sec: 1_700_000_000 + index as i32,
                nanosec: 0,
            },
            frame_id: FRAME_ID.to_owned(),
        },
        angle_min: -1.5,
        angle_max: 1.5,
        angle_increment: 0.25,
        time_increment: 0.0,
        scan_time: 0.05,
        range_min: 0.1,
        range_max: 30.0,
        ranges: vec![1.0 + offset, 2.0 + offset, 3.0 + offset],
        intensities: vec![10.0 + offset, 20.0 + offset, 30.0 + offset],
    }
}

/// What `ros2-scan-sink` observed, written as JSON when the run ends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SinkSummary {
    /// How many scans arrived on the bridged `scan` port.
    pub scans: u64,
    /// The last received scan's ranges, so a reader can see the pipeline
    /// carried real, decodable content rather than merely counting bytes.
    pub last_ranges: Vec<f32>,
}

impl SinkSummary {
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
    use astrs_data::AstrsMessage;

    /// A value the manifest cannot express falls back to the default.
    #[test]
    fn the_scan_budget_falls_back_to_the_default() {
        assert_eq!(scan_budget(Some("4")), 4);
        for raw in [None, Some(""), Some("lots"), Some("0"), Some("-1")] {
            assert_eq!(scan_budget(raw), DEFAULT_SCANS, "{raw:?}");
        }
    }

    /// Scans are deterministic and distinct per index.
    #[test]
    fn scan_for_is_deterministic() {
        let first = scan_for(0);
        assert_eq!(first, scan_for(0));
        assert_ne!(first, scan_for(1));
        assert_eq!(first.header.frame_id, FRAME_ID);
        assert_eq!(scan_for(5).ranges, vec![6.0, 7.0, 8.0]);
    }

    /// The report round-trips as JSON.
    #[test]
    fn a_summary_round_trips_as_json() {
        let summary = SinkSummary {
            scans: 4,
            last_ranges: scan_for(3).ranges,
        };
        let json = summary.to_json().unwrap();
        let parsed: SinkSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, summary);
    }

    /// The report path defaults under the temporary directory.
    #[test]
    fn the_report_path_defaults_under_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// `ros2-scan-sink`'s own decode path: exactly what `Payload::batch()`
    /// plus `AstrsMessage::from_record_batch` does to a bridged sample —
    /// proving the columnar round trip independently of any live bridge.
    #[test]
    fn a_scan_round_trips_through_a_record_batch() {
        let sent = scan_for(7);
        let batch = sent.to_record_batch().unwrap();
        let decoded = LaserScan::from_record_batch(&batch).unwrap();
        assert_eq!(decoded, sent);
    }

    /// `dataflow.yml` is exactly the talker, alone: no `ros2:` block, one
    /// node, a name matching this directory, and a manifest that actually
    /// validates — everything `bridge.yml`'s own header comment claims
    /// about why the two are split.
    #[test]
    fn the_committed_dataflow_is_just_the_talker() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("ros2-talker-bridge"));
        assert_eq!(manifest.nodes.len(), 1);
        assert!(manifest.nodes[0].ros2.is_none());
        assert!(manifest.nodes[0].path.is_some());
    }

    /// `bridge.yml` parses and validates, and its `ros2:` block is exactly
    /// blueprint §10.5's own shape, applied to [`TOPIC`]/[`MESSAGE_TYPE`].
    #[test]
    fn the_committed_bridge_manifest_declares_the_ros2_block() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bridge.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.nodes.len(), 2);

        let bridge = manifest
            .nodes
            .iter()
            .find(|node| node.id == "lidar-in")
            .expect("bridge.yml declares lidar-in");
        assert!(bridge.path.is_none(), "a ros2: source has no path:");
        let ros2 = bridge
            .ros2
            .as_ref()
            .expect("lidar-in declares a ros2: block");
        assert_eq!(ros2.compat, astrs_manifest::RosCompat::Humble);
        assert_eq!(ros2.topic.as_deref(), Some(TOPIC));
        assert_eq!(ros2.message_type.as_deref(), Some(MESSAGE_TYPE));
        assert_eq!(
            ros2.direction,
            Some(astrs_manifest::BridgeDirection::ToAstrs)
        );

        let sink = manifest
            .nodes
            .iter()
            .find(|node| node.id == "sink")
            .expect("bridge.yml declares sink");
        assert!(sink.path.is_some());
        assert!(sink.build.is_some());
    }

    /// The central claim, proved for real: `ros2-talker`'s own
    /// sample-construction and CDR encoding produce a
    /// `sensor_msgs/msg/LaserScan` a plain ROS 2 subscriber decodes
    /// unchanged — unicast-wired on loopback rather than relying on
    /// multicast, exactly as `bins/astrs-ros2-bridge-node`'s own loopback
    /// tests do, so this passes regardless of whether this sandbox permits
    /// a multicast join.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_talker_publishes_a_scan_a_real_subscriber_decodes() {
        use std::sync::Arc;
        use std::time::Duration;

        use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Context, Ros2Node};
        use astrs_ros2::qos::QosProfile;

        let talker_context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("the talker's participant binds");
        let talker = Ros2Node::new(
            Arc::clone(&talker_context),
            "test_talker",
            NodeOptions::default().with_parameter_services(false),
        )
        .await
        .expect("the talker node starts");

        let peer_addr = talker_context
            .metatraffic_locator()
            .socket_addr()
            .expect("a bound participant has a dialable locator");
        let peer_context = Ros2Context::new(ContextOptions::loopback().with_peer(peer_addr.into()))
            .await
            .expect("the peer participant binds");
        let peer = Ros2Node::new(
            Arc::clone(&peer_context),
            "test_peer",
            NodeOptions::default().with_parameter_services(false),
        )
        .await
        .expect("the peer node starts");

        let publisher = talker
            .create_publisher::<LaserScan>(TOPIC, QosProfile::default())
            .await
            .expect("the publisher is created");
        let subscription = peer
            .create_subscription::<LaserScan>(TOPIC, QosProfile::default())
            .await
            .expect("the subscription is created");
        publisher
            .wait_for_subscriptions(1, Duration::from_secs(10))
            .await
            .expect("the two participants must discover each other");

        let sent = scan_for(0);
        publisher
            .publish(&sent)
            .await
            .expect("the talker publishes");

        let (received, _info) = subscription
            .recv()
            .await
            .expect("the peer must receive the scan");
        assert_eq!(received, sent, "the scan crossed real RTPS unchanged");

        talker_context.shutdown().await;
        peer_context.shutdown().await;
    }
}
