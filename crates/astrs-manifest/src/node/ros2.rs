//! The declarative ROS 2 bridge node block (blueprint §10.5).
//!
//! This module is **pure data**: it models the `ros2:` YAML block exactly
//! as written, with `deny_unknown_fields` at every level. It does not
//! validate cross-field semantics (e.g. "exactly one of `topic`/`service`/
//! `action`") beyond what [`crate::Manifest::validate`] performs — per the
//! task brief, "pure data here; semantics live in W5" (the `astrs-rtps` /
//! `astrs-ros2` / bridge-node crates built in wave 5).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::DurationSecs;

/// Which ROS 2 distribution's wire-level quirks to speak (§10.2).
///
/// Humble uses a 24-byte participant GID; Jazzy uses 16 bytes — the two
/// distributions are not wire-compatible with each other, so every bridge
/// must pick one explicitly (there is no sensible default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RosCompat {
    /// ROS 2 Humble Hawksbill (24-byte GID, `rmw_dds_common` graph).
    Humble,
    /// ROS 2 Jazzy Jalisco (16-byte GID).
    Jazzy,
}

/// Which way a bridged topic/service/action crosses the AstRS/ROS 2 boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BridgeDirection {
    /// ROS 2 → AstRS: the bridge subscribes on the ROS 2 side and
    /// publishes an AstRS output.
    ToAstrs,
    /// AstRS → ROS 2: the bridge subscribes on the AstRS side (an input)
    /// and publishes on the ROS 2 side.
    FromAstrs,
}

/// Whether a bridged `service:`/`action:` block acts as the server or the
/// client side of the exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ros2Role {
    /// This bridge answers requests/goals.
    Server,
    /// This bridge issues requests/goals.
    Client,
}

/// DDS durability QoS (§10.2): whether late-joining subscribers receive
/// history from before they attached.
///
/// Written in the manifest with a hyphen (`transient-local`), matching the
/// blueprint's own prose spelling in §10.2 ("durability
/// (volatile/transient-local)") — unlike the underscore-separated
/// `restart_policy`/`queue_policy` values, which the blueprint shows in an
/// actual YAML example (§8.1) rather than prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Durability {
    /// No history is kept for late joiners (the DDS default).
    Volatile,
    /// The last N samples (per `keep_last`/`keep_all`) are delivered to
    /// late-joining subscribers.
    TransientLocal,
}

/// The QoS block of a `ros2:` bridge (§10.5).
///
/// Every field is optional; an absent field means "let `astrs-rtps` pick
/// its documented default" (this crate does not encode RTPS defaults —
/// that is §10.2's concern, not the manifest's).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Qos {
    /// Request the RELIABLE reliability kind rather than BEST_EFFORT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reliable: Option<bool>,
    /// Durability: `volatile` or `transient-local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<Durability>,
    /// KEEP_LAST history depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_last: Option<u32>,
    /// Request KEEP_ALL history instead of KEEP_LAST.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_all: Option<bool>,
    /// Deadline QoS: the maximum expected period between samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DurationSecs>,
    /// Liveliness lease duration (§10.2's WLP liveliness).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_duration: Option<DurationSecs>,
}

/// One entry of a `ros2: { topics: [...] }` bulk bridge list (§10.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ros2Topic {
    /// The ROS 2 topic name, e.g. `/scan`.
    pub topic: String,
    /// The ROS 2 message type, e.g. `sensor_msgs/msg/LaserScan`.
    pub message_type: String,
    /// Which way this topic crosses the bridge.
    pub direction: BridgeDirection,
}

/// The full `ros2:` bridge configuration block (§10.5).
///
/// Covers the single-topic form (`topic`/`message_type`/`direction`), the
/// bulk `topics: [...]` form, and the `service:`/`action:` + `role:` form,
/// all as one struct — a node's `ros2:` block is expected to use exactly
/// one of these shapes, which [`crate::Manifest::validate`] does not
/// currently enforce (see this module's top-level docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ros2Config {
    /// Which ROS 2 distribution's wire format to speak.
    pub compat: RosCompat,

    /// The single-topic form: a ROS 2 topic name, e.g. `/scan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The single-topic form: the ROS 2 message type, e.g.
    /// `sensor_msgs/msg/LaserScan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_type: Option<String>,
    /// The single-topic form: which way the topic crosses the bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<BridgeDirection>,

    /// The bulk form: multiple topics bridged by one node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<Ros2Topic>,

    /// The service-bridge form: a ROS 2 service name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The action-bridge form: a ROS 2 action name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Server or client side of `service`/`action`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Ros2Role>,

    /// DDS QoS overrides for this bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos: Option<Qos>,
    /// A ROS 2 namespace to mount the bridged entity under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The participant name this bridge presents on the ROS 2 graph
    /// (visible to `ros2 node list` on a stock ROS box, per §10.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_blueprint_lidar_example() {
        let yaml = "\
compat: humble
topic: /scan
message_type: sensor_msgs/msg/LaserScan
direction: to_astrs
qos: { reliable: true, keep_last: 10 }
";
        let cfg: Ros2Config = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.compat, RosCompat::Humble);
        assert_eq!(cfg.topic.as_deref(), Some("/scan"));
        assert_eq!(
            cfg.message_type.as_deref(),
            Some("sensor_msgs/msg/LaserScan")
        );
        assert_eq!(cfg.direction, Some(BridgeDirection::ToAstrs));
        let qos = cfg.qos.unwrap();
        assert_eq!(qos.reliable, Some(true));
        assert_eq!(qos.keep_last, Some(10));
    }

    #[test]
    fn parses_bulk_topics_form() {
        let yaml = "\
compat: jazzy
topics:
  - topic: /scan
    message_type: sensor_msgs/msg/LaserScan
    direction: to_astrs
  - topic: /cmd_vel
    message_type: geometry_msgs/msg/Twist
    direction: from_astrs
";
        let cfg: Ros2Config = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.topics.len(), 2);
        assert_eq!(cfg.topics[1].direction, BridgeDirection::FromAstrs);
    }

    #[test]
    fn parses_service_role_form() {
        let yaml = "\
compat: humble
service: /add_two_ints
role: server
";
        let cfg: Ros2Config = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.service.as_deref(), Some("/add_two_ints"));
        assert_eq!(cfg.role, Some(Ros2Role::Server));
    }

    #[test]
    fn durability_uses_kebab_case_on_wire() {
        assert_eq!(
            astrs_yaml::to_string(&Durability::TransientLocal)
                .unwrap()
                .trim(),
            "transient-local"
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = "compat: humble\ntopic: /scan\nbogus: true\n";
        assert!(astrs_yaml::from_str::<Ros2Config>(yaml).is_err());
    }

    #[test]
    fn requires_compat() {
        let yaml = "topic: /scan\n";
        assert!(astrs_yaml::from_str::<Ros2Config>(yaml).is_err());
    }
}
