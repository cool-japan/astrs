//! Shared types and pure logic for `ros2-native-listener` (blueprint
//! §10.4, §5.4).
//!
//! ```text
//!   [talker] ──/chatter (std_msgs/msg/String, real DDS/RTPS)──► [listener]
//! ```
//!
//! Both binaries construct their own [`astrs_ros2::node::Ros2Context`] and
//! [`astrs_ros2::node::Ros2Node`] directly — the rcl-equivalent surface
//! blueprint §10.4 describes — and talk to each other over real RTPS on
//! loopback, entirely independent of the AstRS graph's own data plane
//! (neither node declares any `inputs:`/`outputs:` in `dataflow.yml`).
//! [`chat_text`] is the one piece of logic worth sharing and testing on its
//! own: the deterministic message body both binaries and this crate's own
//! test agree on.

use serde::{Deserialize, Serialize};

/// The ROS 2 topic name both nodes talk over.
pub const TOPIC: &str = "/chatter";
/// The ROS 2 message type carried on [`TOPIC`].
pub const MESSAGE_TYPE: &str = "std_msgs/msg/String";

/// The DDS domain both participants join.
///
/// Deliberately not the ROS 2 default (`0`), so this pair never collides
/// with a real ROS 2 stack that happens to be running on the same host.
pub const DOMAIN_ID: u32 = 92;

/// Environment variable overriding how many messages `native-talker` sends.
pub const ENV_TALKER_MESSAGES: &str = "NATIVE_TALKER_MESSAGES";
/// Environment variable overriding how many messages `native-listener`
/// waits for before finishing.
pub const ENV_LISTENER_MESSAGES: &str = "NATIVE_LISTENER_MESSAGES";
/// How many messages each side budgets by default.
pub const DEFAULT_MESSAGES: u64 = 15;

/// How often `native-talker` publishes.
pub const PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// The absolute deadline `native-listener` gives up at, even if fewer than
/// its budgeted messages ever arrive — the safety net against a discovery
/// failure (see this crate's own README) hanging the node forever.
pub const LISTEN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Environment variable naming the JSON file `native-listener` writes its
/// [`ListenerReport`] to.
pub const ENV_REPORT_PATH: &str = "NATIVE_LISTENER_REPORT";

/// How many messages this run should budget, for either side.
///
/// A value the manifest cannot express falls back to [`DEFAULT_MESSAGES`]
/// rather than failing the node — including zero, which would make
/// `exit_when_nodes_finish` fire before either side had done anything.
#[must_use]
pub fn message_budget(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_MESSAGES)
}

/// Where `native-listener` writes its report when the manifest names no
/// path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-ros2-native-listener-report.json")
}

/// The file this run's listener writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// The `std_msgs/msg/String` body `native-talker` sends for message
/// `index`.
///
/// A pure function of the index, deterministic, so both binaries and this
/// crate's own test agree on exactly what crosses the wire.
#[must_use]
pub fn chat_text(index: u64) -> String {
    format!("hello from native-talker, message {index}")
}

/// What `native-listener` observed, written as JSON when the run ends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListenerReport {
    /// How many messages arrived before the budget or the deadline.
    pub received: u64,
    /// Every message body, in arrival order — decoded, not merely
    /// counted, so a reader can see the subscription really carried
    /// `native-talker`'s own text.
    pub texts: Vec<String>,
    /// Whether [`Self::received`] met the configured budget, as opposed to
    /// giving up at [`LISTEN_DEADLINE`] with fewer.
    pub met_budget: bool,
}

impl ListenerReport {
    /// Renders the report as pretty JSON.
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
        assert_eq!(message_budget(Some("4")), 4);
        for raw in [None, Some(""), Some("lots"), Some("0"), Some("-1")] {
            assert_eq!(message_budget(raw), DEFAULT_MESSAGES, "{raw:?}");
        }
    }

    /// Chat text is deterministic and distinct per index.
    #[test]
    fn chat_text_is_deterministic() {
        assert_eq!(chat_text(0), "hello from native-talker, message 0");
        assert_eq!(chat_text(0), chat_text(0));
        assert_ne!(chat_text(0), chat_text(1));
    }

    /// The report round-trips as JSON.
    #[test]
    fn a_report_round_trips_as_json() {
        let report = ListenerReport {
            received: 3,
            texts: vec![chat_text(0), chat_text(1), chat_text(2)],
            met_budget: false,
        };
        let json = report.to_json().unwrap();
        let parsed: ListenerReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    /// The report path defaults under the temporary directory.
    #[test]
    fn the_report_path_defaults_under_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The central claim, proved for real: two independent `Ros2Node`s —
    /// unicast-wired on loopback rather than relying on multicast, exactly
    /// as `bins/astrs-ros2-bridge-node`'s own loopback tests do — round
    /// trip `std_msgs/msg/String` over a typed `Publisher`/`Subscription`
    /// pair with no bridge process and no `ros2:` block anywhere. This is
    /// what `native-talker`/`native-listener` do as two separate OS
    /// processes joining the same DDS domain; this test proves the same
    /// `Ros2Node` calls work without depending on multicast having been
    /// permitted in whatever sandbox runs the suite.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn native_pub_sub_round_trips_over_real_rtps() {
        use std::sync::Arc;
        use std::time::Duration;

        use astrs_ros2::msg::std_msgs::String as ChatMessage;
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

        let listener_peer = talker_context
            .metatraffic_locator()
            .socket_addr()
            .expect("a bound participant has a dialable locator");
        let listener_context =
            Ros2Context::new(ContextOptions::loopback().with_peer(listener_peer.into()))
                .await
                .expect("the listener's participant binds");
        let listener = Ros2Node::new(
            Arc::clone(&listener_context),
            "test_listener",
            NodeOptions::default().with_parameter_services(false),
        )
        .await
        .expect("the listener node starts");

        let publisher = talker
            .create_publisher::<ChatMessage>(TOPIC, QosProfile::default())
            .await
            .expect("the publisher is created");
        let subscription = listener
            .create_subscription::<ChatMessage>(TOPIC, QosProfile::default())
            .await
            .expect("the subscription is created");
        publisher
            .wait_for_subscriptions(1, Duration::from_secs(10))
            .await
            .expect("the two participants must discover each other");

        let sent = ChatMessage { data: chat_text(0) };
        publisher
            .publish(&sent)
            .await
            .expect("the talker publishes");

        let (received, _info) = subscription
            .recv()
            .await
            .expect("the listener must receive the message");
        assert_eq!(received.data, sent.data, "the chat text crossed unchanged");

        talker_context.shutdown().await;
        listener_context.shutdown().await;
    }

    /// The committed manifest parses, validates and names this dataflow —
    /// neither node declares an `inputs:`/`outputs:` edge (see this
    /// crate's docs for why), so this is what actually stands in for the
    /// "typed edges" check every other example's estate coverage performs.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("ros2-native-listener"));
        assert_eq!(manifest.nodes.len(), 2);
    }
}
