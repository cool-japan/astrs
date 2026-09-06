//! Reading the bridge's own configuration out of the spawn handshake.
//!
//! # Where the `ros2:` block comes from
//!
//! The bridge is an ordinary manifest node, so it learns everything the same
//! way every other spawned node does — from `ASTRS_NODE_CONFIG`, the
//! oxicode+base64 handshake blob `astrs-daemon`'s `DaemonOwnedVars` writes
//! into the child's environment. [`astrs_node_api::Node::init_from_env`]
//! decodes it; [`astrs_node_api::Node::descriptor`] hands back the
//! [`NodeSpawnSpec`] inside, and the `ros2:` block rides in that spec's
//! [`NodeSource::Ros2Bridge`] variant as a JSON string.
//!
//! ```text
//!   manifest `ros2:` ──serde_json──▶ NodeSource::Ros2Bridge { config }
//!                                          │
//!                          NodeSpawnSpec ──┤ oxicode + base64
//!                                          ▼
//!                                  ASTRS_NODE_CONFIG ──▶ this process
//! ```
//!
//! JSON rather than the original YAML because that is the shape
//! `astrs-coordinator`'s `graph_bridge::node_source_for` already chose, and
//! one shape is worth more than a preference: `astrs-daemon`'s own
//! single-process planner (`dataflow::plan`) was carrying only the node id
//! there, which is why this crate's landing needed that one-line contract
//! fix (see the report).
//!
//! # The knobs that are *not* in the block
//!
//! The DDS domain, the interface search path and where the participant's
//! sockets bind are deployment facts, not graph facts, so §10.5's schema has
//! no field for any of them and this module reads them from the node's
//! environment instead — the manifest's ordinary `env:` block, which the
//! daemon applies to the child (§16's allowlist scrubs the *inherited*
//! environment, so a bridge that wants a non-zero domain says so in its
//! manifest rather than relying on the operator's shell):
//!
//! ```yaml
//!   - id: lidar-in
//!     env:
//!       ROS_DOMAIN_ID: "7"
//!       AMENT_PREFIX_PATH: /opt/ros/humble:/opt/my_msgs
//!     ros2:
//!       compat: humble
//!       topic: /scan
//!       message_type: sensor_msgs/msg/LaserScan
//!       direction: to_astrs
//! ```

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use astrs_manifest::Ros2Config;
use astrs_rtps::behavior::BindPolicy;
use astrs_wire::{NodeSource, NodeSpawnSpec};

use crate::error::ConfigError;

/// The DDS domain to join, when nothing says otherwise.
pub const DEFAULT_DOMAIN_ID: u32 = 0;

/// The environment variable naming the DDS domain — ROS 2's own spelling.
pub const ENV_DOMAIN_ID: &str = "ROS_DOMAIN_ID";

/// The environment variable naming the ament install prefixes to search for
/// `.msg`/`.srv`/`.action` definitions — ROS 2's own spelling.
pub const ENV_AMENT_PREFIX_PATH: &str = "AMENT_PREFIX_PATH";

/// The environment variable naming *source* trees (packages that have a
/// `package.xml` and a `msg/` directory but no `share/` install layout).
///
/// Not a ROS 2 variable: `AMENT_PREFIX_PATH` points at install prefixes, and
/// a developer bridging a type from a package they have merely checked out
/// has no install prefix to point at. Kept separate rather than overloading
/// `AMENT_PREFIX_PATH` because the two are searched differently — see
/// [`crate::resolve`].
pub const ENV_INTERFACE_PATH: &str = "ASTRS_ROS2_INTERFACE_PATH";

/// The environment variable that turns the `ros_discovery_info` graph
/// announcement off.
///
/// On by default: a bridge that publishes `/scan` should appear in `ros2
/// node list` on a stock ROS box, which is §10.4's whole point. A bridge
/// deliberately invisible to the ROS graph sets this to `0`/`false`.
pub const ENV_ANNOUNCE_GRAPH: &str = "ASTRS_ROS2_ANNOUNCE_GRAPH";

/// The environment variable that turns the SPDP multicast join off.
///
/// On by default, and *probed* rather than assumed (§10.2's addendum): a
/// refused join is reported, not fatal. Setting this to `0` skips the
/// attempt entirely, which is what a sandboxed CI host wants.
pub const ENV_MULTICAST: &str = "ASTRS_ROS2_MULTICAST";

/// The environment variable choosing where the participant's sockets bind.
///
/// Three spellings, matching [`BindPolicy`]:
///
/// | Value | Policy | When |
/// |---|---|---|
/// | `loopback` (default) | `EphemeralLoopback` | a ROS 2 stack on **this host** |
/// | `any:<address>` | `EphemeralAny` | another host, kernel-chosen ports |
/// | `standard:<address>` | `Standard` | another host, §9.6.1.1 ports |
///
/// The default is the same-host case on purpose: it needs no address, it
/// cannot collide with another participant's ports, and it is what a bridge
/// beside a robot's own ROS 2 stack wants. A bridge that must be reachable
/// from another machine has to say which address to announce, and no
/// default can guess that.
pub const ENV_BIND: &str = "ASTRS_ROS2_BIND";

/// The environment variable listing unicast peers to announce to directly,
/// as a `,`-separated list of `host:port`.
///
/// §10.2's addendum makes unicast initial peers a first-class discovery
/// path, not a fallback; this is how a bridge on a host with no multicast
/// permission still finds its peers.
pub const ENV_INITIAL_PEERS: &str = "ASTRS_ROS2_INITIAL_PEERS";

/// Everything the bridge needs that is not in the `ros2:` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSettings {
    /// The DDS domain to join.
    pub domain_id: u32,
    /// Ament install prefixes to search for interface definitions.
    pub ament_prefixes: Vec<PathBuf>,
    /// Source trees to search for interface definitions.
    pub source_trees: Vec<PathBuf>,
    /// Whether to announce `ros_discovery_info`.
    pub announce_graph: bool,
    /// Whether to attempt the SPDP multicast join.
    pub multicast: bool,
    /// Unicast peers to announce to, as written (`host:port`).
    pub initial_peers: Vec<String>,
    /// Where the participant's sockets bind, and what they announce.
    pub bind: BindPolicy,
}

impl Default for BridgeSettings {
    fn default() -> Self {
        Self {
            domain_id: DEFAULT_DOMAIN_ID,
            ament_prefixes: Vec::new(),
            source_trees: Vec::new(),
            announce_graph: true,
            multicast: true,
            initial_peers: Vec::new(),
            bind: BindPolicy::EphemeralLoopback,
        }
    }
}

impl BridgeSettings {
    /// Read the settings out of one variable map.
    ///
    /// Pure: the map is the whole input, which is what lets a test drive
    /// this without touching the process environment. [`Self::from_spec`]
    /// is the impure wrapper the binary uses.
    ///
    /// # Errors
    ///
    /// [`ConfigError::BadEnv`] when a variable is set to something that
    /// does not parse. A *missing* variable is never an error — every one
    /// of them has a documented default.
    pub fn from_env_map(env: &BTreeMap<String, String>) -> Result<Self, ConfigError> {
        let mut settings = Self::default();

        if let Some(raw) = env.get(ENV_DOMAIN_ID) {
            settings.domain_id = raw.trim().parse::<u32>().map_err(|_| ConfigError::BadEnv {
                name: ENV_DOMAIN_ID,
                expected: "non-negative integer domain id",
                value: raw.clone(),
            })?;
        }

        if let Some(raw) = env.get(ENV_AMENT_PREFIX_PATH) {
            settings.ament_prefixes = split_path_list(raw);
        }
        if let Some(raw) = env.get(ENV_INTERFACE_PATH) {
            settings.source_trees = split_path_list(raw);
        }

        if let Some(raw) = env.get(ENV_ANNOUNCE_GRAPH) {
            settings.announce_graph = parse_flag(raw, ENV_ANNOUNCE_GRAPH)?;
        }
        if let Some(raw) = env.get(ENV_MULTICAST) {
            settings.multicast = parse_flag(raw, ENV_MULTICAST)?;
        }
        if let Some(raw) = env.get(ENV_BIND) {
            settings.bind = parse_bind(raw)?;
        }

        if let Some(raw) = env.get(ENV_INITIAL_PEERS) {
            settings.initial_peers = raw
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(ToOwned::to_owned)
                .collect();
        }

        Ok(settings)
    }

    /// Read the settings for a spawned node.
    ///
    /// The spec's own `env:` map wins over the process environment: it is
    /// what the manifest asked for, and it is what the daemon *would* have
    /// applied. Consulting it directly rather than only `std::env` means a
    /// [`astrs_node_api::testing::MockDaemon`]-driven test can configure a
    /// bridge without mutating a process-global.
    ///
    /// # Errors
    ///
    /// As [`Self::from_env_map`].
    pub fn from_spec(spec: &NodeSpawnSpec) -> Result<Self, ConfigError> {
        let mut merged = BTreeMap::new();
        for name in [
            ENV_DOMAIN_ID,
            ENV_AMENT_PREFIX_PATH,
            ENV_INTERFACE_PATH,
            ENV_ANNOUNCE_GRAPH,
            ENV_MULTICAST,
            ENV_INITIAL_PEERS,
            ENV_BIND,
        ] {
            if let Ok(value) = std::env::var(name) {
                merged.insert(name.to_owned(), value);
            }
            if let Some(value) = spec.env.get(name) {
                merged.insert(name.to_owned(), value.clone());
            }
        }
        Self::from_env_map(&merged)
    }

    /// Whether any interface search path at all is configured.
    #[must_use]
    pub fn has_search_path(&self) -> bool {
        !self.ament_prefixes.is_empty() || !self.source_trees.is_empty()
    }

    /// The search path rendered for an error message.
    ///
    /// Reads as the second half of "…and {this}", so it is a clause rather
    /// than a list: the reader of a failed startup wants to know whether
    /// anything was searched at all before they are shown paths.
    #[must_use]
    pub fn describe_search_path(&self) -> String {
        if !self.has_search_path() {
            return format!(
                "no interface search path is configured (set {ENV_AMENT_PREFIX_PATH} or \
                 {ENV_INTERFACE_PATH} in the node's `env:` block)"
            );
        }
        let mut rendered = String::from("it was not found under ");
        let mut first = true;
        for path in self.ament_prefixes.iter().chain(&self.source_trees) {
            if !first {
                rendered.push_str(", ");
            }
            first = false;
            rendered.push('`');
            rendered.push_str(&path.display().to_string());
            rendered.push('`');
        }
        rendered
    }
}

/// Split a `:`-separated (`;` on Windows) path list, dropping empties.
fn split_path_list(raw: &str) -> Vec<PathBuf> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    raw.split(separator)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Parse the [`ENV_BIND`] spelling into a policy.
fn parse_bind(raw: &str) -> Result<BindPolicy, ConfigError> {
    let trimmed = raw.trim();
    let bad = || ConfigError::BadEnv {
        name: ENV_BIND,
        expected: "`loopback`, `any:<ipv4>` or `standard:<ipv4>`",
        value: raw.to_owned(),
    };
    if trimmed.eq_ignore_ascii_case("loopback") {
        return Ok(BindPolicy::EphemeralLoopback);
    }
    let (kind, address) = trimmed.split_once(':').ok_or_else(bad)?;
    let address: Ipv4Addr = address.trim().parse().map_err(|_| bad())?;
    match kind.trim().to_ascii_lowercase().as_str() {
        "any" => Ok(BindPolicy::EphemeralAny(address)),
        "standard" => Ok(BindPolicy::Standard(address)),
        _ => Err(bad()),
    }
}

/// Parse a boolean-ish environment variable.
fn parse_flag(raw: &str, name: &'static str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::BadEnv {
            name,
            expected: "boolean (`1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`)",
            value: raw.to_owned(),
        }),
    }
}

/// Read the `ros2:` block out of a spawn specification.
///
/// # Errors
///
/// [`ConfigError::NotABridge`] when the spec's source is not a bridge, and
/// [`ConfigError::Malformed`] when the carried string is not a serialized
/// [`Ros2Config`] — which is what an older daemon forwarding only the node
/// id produces, and what the message says.
pub fn bridge_config(spec: &NodeSpawnSpec) -> Result<Ros2Config, ConfigError> {
    match &spec.source {
        NodeSource::Ros2Bridge { config } => Ok(serde_json::from_str(config)?),
        other => Err(ConfigError::NotABridge {
            kind: other.kind_name(),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_manifest::{BridgeDirection, RosCompat};
    use astrs_wire::{DataflowId, NodeId};

    use super::*;

    fn spec_with(source: NodeSource) -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("lidar-in").unwrap(),
            0,
            source,
        )
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_bridge_source_round_trips_the_block() {
        let yaml = "\
compat: humble
topic: /scan
message_type: sensor_msgs/msg/LaserScan
direction: to_astrs
qos: { reliable: true, keep_last: 10 }
";
        let original: Ros2Config = astrs_yaml::from_str(yaml).unwrap();
        let spec = spec_with(NodeSource::Ros2Bridge {
            config: serde_json::to_string(&original).unwrap(),
        });

        let read = bridge_config(&spec).unwrap();
        assert_eq!(read.compat, RosCompat::Humble);
        assert_eq!(read.topic.as_deref(), Some("/scan"));
        assert_eq!(read.direction, Some(BridgeDirection::ToAstrs));
        assert_eq!(read, original);
    }

    #[test]
    fn a_non_bridge_source_names_the_kind_it_found() {
        let spec = spec_with(NodeSource::Executable {
            path: "./camera".into(),
        });
        let error = bridge_config(&spec).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::NotABridge { kind: "executable" }
        ));
    }

    /// The regression this crate's daemon fix exists for: a bare node id in
    /// the config slot must be a typed error naming the contract, not a
    /// silent "bridge nothing".
    #[test]
    fn a_bare_node_id_in_the_config_slot_is_a_typed_error() {
        let spec = spec_with(NodeSource::Ros2Bridge {
            config: "lidar-in".into(),
        });
        let error = bridge_config(&spec).unwrap_err();
        assert!(matches!(error, ConfigError::Malformed { .. }));
        assert!(
            error.to_string().contains("forward the manifest"),
            "{error}"
        );
    }

    #[test]
    fn settings_default_to_domain_zero_and_no_search_path() {
        let settings = BridgeSettings::from_env_map(&BTreeMap::new()).unwrap();
        assert_eq!(settings, BridgeSettings::default());
        assert_eq!(settings.domain_id, 0);
        assert!(!settings.has_search_path());
        assert!(settings.announce_graph);
        assert!(settings.multicast);
    }

    #[test]
    fn the_domain_id_is_read_from_the_ros_variable() {
        let settings = BridgeSettings::from_env_map(&env(&[(ENV_DOMAIN_ID, " 7 ")])).unwrap();
        assert_eq!(settings.domain_id, 7);
    }

    #[test]
    fn a_non_numeric_domain_id_is_refused_by_name() {
        let error = BridgeSettings::from_env_map(&env(&[(ENV_DOMAIN_ID, "seven")])).unwrap_err();
        match error {
            ConfigError::BadEnv { name, value, .. } => {
                assert_eq!(name, ENV_DOMAIN_ID);
                assert_eq!(value, "seven");
            }
            other => panic!("expected BadEnv, got {other}"),
        }
    }

    #[test]
    fn the_search_paths_split_on_the_platform_separator() {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let raw = format!("/opt/ros/humble{separator}{separator}/opt/my_msgs");
        let settings =
            BridgeSettings::from_env_map(&env(&[(ENV_AMENT_PREFIX_PATH, &raw)])).unwrap();
        assert_eq!(
            settings.ament_prefixes,
            vec![
                PathBuf::from("/opt/ros/humble"),
                PathBuf::from("/opt/my_msgs")
            ],
            "empty entries are dropped, not turned into `.`"
        );
        assert!(settings.has_search_path());
    }

    #[test]
    fn the_flags_accept_every_documented_spelling() {
        for (raw, expected) in [
            ("1", true),
            ("TRUE", true),
            ("yes", true),
            ("On", true),
            ("0", false),
            ("false", false),
            ("no", false),
            ("OFF", false),
        ] {
            let settings = BridgeSettings::from_env_map(&env(&[(ENV_MULTICAST, raw)])).unwrap();
            assert_eq!(settings.multicast, expected, "{raw}");
        }
    }

    #[test]
    fn a_nonsense_flag_is_refused_by_name() {
        let error =
            BridgeSettings::from_env_map(&env(&[(ENV_ANNOUNCE_GRAPH, "maybe")])).unwrap_err();
        assert!(error.to_string().contains(ENV_ANNOUNCE_GRAPH), "{error}");
    }

    #[test]
    fn the_bind_policy_defaults_to_loopback_and_reads_every_spelling() {
        assert_eq!(
            BridgeSettings::default().bind,
            BindPolicy::EphemeralLoopback
        );
        for (raw, expected) in [
            ("loopback", BindPolicy::EphemeralLoopback),
            ("LOOPBACK", BindPolicy::EphemeralLoopback),
            (
                "any:0.0.0.0",
                BindPolicy::EphemeralAny(Ipv4Addr::UNSPECIFIED),
            ),
            (
                "standard:192.168.1.10",
                BindPolicy::Standard(Ipv4Addr::new(192, 168, 1, 10)),
            ),
        ] {
            let settings = BridgeSettings::from_env_map(&env(&[(ENV_BIND, raw)])).unwrap();
            assert_eq!(settings.bind, expected, "{raw}");
        }
    }

    #[test]
    fn a_nonsense_bind_policy_is_refused_by_name() {
        for raw in ["everywhere", "any:", "any:not-an-address", "bogus:1.2.3.4"] {
            let error = BridgeSettings::from_env_map(&env(&[(ENV_BIND, raw)])).unwrap_err();
            assert!(error.to_string().contains(ENV_BIND), "{raw}: {error}");
        }
    }

    #[test]
    fn initial_peers_split_on_commas_and_drop_blanks() {
        let settings = BridgeSettings::from_env_map(&env(&[(
            ENV_INITIAL_PEERS,
            "127.0.0.1:7400, ,127.0.0.1:7410",
        )]))
        .unwrap();
        assert_eq!(settings.initial_peers, ["127.0.0.1:7400", "127.0.0.1:7410"]);
    }

    #[test]
    fn the_spec_env_is_consulted() {
        let mut spec = spec_with(NodeSource::Ros2Bridge {
            config: "{}".into(),
        });
        spec.env.insert(ENV_DOMAIN_ID.to_owned(), "42".to_owned());
        let settings = BridgeSettings::from_spec(&spec).unwrap();
        assert_eq!(settings.domain_id, 42);
    }

    #[test]
    fn an_absent_search_path_says_so_rather_than_listing_nothing() {
        let settings = BridgeSettings::default();
        let described = settings.describe_search_path();
        assert!(
            described.contains("no interface search path"),
            "{described}"
        );
        assert!(described.contains(ENV_AMENT_PREFIX_PATH), "{described}");
    }

    #[test]
    fn a_configured_search_path_is_listed_in_the_message() {
        let settings = BridgeSettings {
            ament_prefixes: vec![PathBuf::from("/opt/my_msgs")],
            ..BridgeSettings::default()
        };
        let described = settings.describe_search_path();
        assert!(described.contains("/opt/my_msgs"), "{described}");
    }
}
