//! [`ContextOptions`] and [`NodeOptions`]: everything `--ros-args` can set.

use std::net::Ipv4Addr;
use std::time::Duration as StdDuration;

use astrs_rtps::behavior::BindPolicy;
use astrs_rtps::discovery::RosCompat;
use astrs_rtps::structure::{GuidPrefix, Locator, VendorId};

use crate::names::RemapRules;
use crate::qos::QosProfile;

/// The default DDS domain, and what `ROS_DOMAIN_ID` defaults to.
pub const DEFAULT_DOMAIN_ID: u32 = 0;

/// How often the participant's background loop runs its cadence.
pub const DEFAULT_TICK_PERIOD: StdDuration = StdDuration::from_millis(20);

/// The `PID_USER_DATA` a ROS 2 participant announces when no enclave is set.
pub const DEFAULT_ENCLAVE: &str = "/";

/// How a [`Ros2Context`](crate::node::Ros2Context) builds its participant.
///
/// One participant per context, many nodes per participant — which is what
/// a ROS 2 component container is, and why the node options and the context
/// options are separate types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOptions {
    /// The DDS domain.
    pub domain_id: u32,
    /// The participant id inside the domain, used only under
    /// [`BindPolicy::Standard`].
    pub participant_id: u32,
    /// Which ROS 2 distribution's conventions to speak.
    pub compat: RosCompat,
    /// Where the sockets bind.
    pub bind: BindPolicy,
    /// Whether to attempt the SPDP multicast join.
    pub multicast: bool,
    /// Peers to announce to directly, whether or not multicast works.
    pub initial_peers: Vec<Locator>,
    /// The security enclave, announced in `PID_USER_DATA` as `enclave=…;`.
    pub enclave: String,
    /// How often the participant's background loop runs.
    pub tick_period: StdDuration,
    /// A fixed GUID prefix, for a test that needs a reproducible identity.
    pub guid_prefix: Option<GuidPrefix>,
    /// Whether to announce `ros_discovery_info`.
    ///
    /// On by default: it is what makes `ros2 node list` see AstRS nodes. A
    /// bridge that is deliberately invisible to the ROS graph turns it off.
    pub announce_graph: bool,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            domain_id: DEFAULT_DOMAIN_ID,
            participant_id: 0,
            compat: RosCompat::default(),
            bind: BindPolicy::default(),
            multicast: true,
            initial_peers: Vec::new(),
            enclave: DEFAULT_ENCLAVE.to_owned(),
            tick_period: DEFAULT_TICK_PERIOD,
            guid_prefix: None,
            announce_graph: true,
        }
    }
}

impl ContextOptions {
    /// Options for `domain_id`.
    #[must_use]
    pub fn new(domain_id: u32) -> Self {
        Self {
            domain_id,
            ..Self::default()
        }
    }

    /// Options a test wants: loopback, ephemeral ports, no multicast.
    ///
    /// Two contexts built this way in one process cannot collide on a port,
    /// and neither one needs the kernel's permission to join a multicast
    /// group — which a sandboxed host refuses. Discovery then runs over
    /// [`with_peer`](Self::with_peer).
    #[must_use]
    pub fn loopback() -> Self {
        Self {
            bind: BindPolicy::EphemeralLoopback,
            multicast: false,
            ..Self::default()
        }
    }

    /// Bind the §9.6.1.1 ports on `address` rather than ephemeral ones.
    ///
    /// What a deployment does: a peer that has never met this participant
    /// finds it by computing the port from the domain id.
    #[must_use]
    pub const fn with_standard_ports(mut self, address: Ipv4Addr) -> Self {
        self.bind = BindPolicy::Standard(address);
        self
    }

    /// Replace the distribution.
    #[must_use]
    pub const fn with_compat(mut self, compat: RosCompat) -> Self {
        self.compat = compat;
        self
    }

    /// Turn the multicast join on or off.
    #[must_use]
    pub const fn with_multicast(mut self, enabled: bool) -> Self {
        self.multicast = enabled;
        self
    }

    /// Add a unicast peer to announce to.
    #[must_use]
    pub fn with_peer(mut self, peer: Locator) -> Self {
        self.initial_peers.push(peer);
        self
    }

    /// Replace the enclave.
    #[must_use]
    pub fn with_enclave(mut self, enclave: impl Into<String>) -> Self {
        self.enclave = enclave.into();
        self
    }

    /// Replace the participant id.
    #[must_use]
    pub const fn with_participant_id(mut self, participant_id: u32) -> Self {
        self.participant_id = participant_id;
        self
    }

    /// Replace the background loop's cadence.
    #[must_use]
    pub const fn with_tick_period(mut self, period: StdDuration) -> Self {
        self.tick_period = period;
        self
    }

    /// Pin the GUID prefix.
    #[must_use]
    pub const fn with_guid_prefix(mut self, prefix: GuidPrefix) -> Self {
        self.guid_prefix = Some(prefix);
        self
    }

    /// Turn the `ros_discovery_info` announcement on or off.
    #[must_use]
    pub const fn with_graph_announcement(mut self, enabled: bool) -> Self {
        self.announce_graph = enabled;
        self
    }

    /// The `PID_USER_DATA` octets this context announces.
    ///
    /// ROS 2 puts `enclave=<path>;` there, and `ros2 node list --enclaves`
    /// is what reads it back.
    #[must_use]
    pub fn user_data(&self) -> Vec<u8> {
        format!("enclave={};", self.enclave).into_bytes()
    }

    /// The GUID prefix this context will use, generating one if none was
    /// pinned.
    ///
    /// Every AstRS prefix starts with [`VendorId::ASTRS`]
    /// ([`GuidPrefix::vendor_scoped`]), which is what makes an AstRS
    /// participant identifiable on a shared domain.
    #[must_use]
    pub fn resolved_guid_prefix(&self) -> GuidPrefix {
        self.guid_prefix
            .unwrap_or_else(|| GuidPrefix::vendor_scoped(VendorId::ASTRS, random_seed()))
    }
}

/// Ten octets of process-unique entropy for a GUID prefix.
///
/// Not a cryptographic identifier — a GUID prefix only has to be unique
/// across the participants that can hear each other. The process id and a
/// per-process counter give that, and the nanosecond clock reading
/// distinguishes two runs of the same process id after a fast restart.
fn random_seed() -> [u8; 10] {
    use std::sync::atomic::{AtomicU16, Ordering};
    static COUNTER: AtomicU16 = AtomicU16::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_u64, |since| since.as_nanos() as u64);
    let pid = u32::from(std::process::id() as u16);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut seed = [0_u8; 10];
    seed[..4].copy_from_slice(&pid.to_be_bytes());
    seed[4..6].copy_from_slice(&counter.to_be_bytes());
    seed[6..].copy_from_slice(&(nanos as u32).to_be_bytes());
    seed
}

/// How a [`Ros2Node`](crate::node::Ros2Node) is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeOptions {
    /// The node's namespace, absolute. Defaults to the root.
    pub namespace: String,
    /// `--ros-args -r from:=to` rules.
    pub remap: RemapRules,
    /// Whether to start the six parameter services and the
    /// `/parameter_events` publisher.
    pub start_parameter_services: bool,
    /// Whether `use_sim_time` starts set.
    pub use_sim_time: bool,
    /// Whether a `set_parameter` for an undeclared name declares it instead
    /// of failing.
    pub allow_undeclared_parameters: bool,
    /// The QoS the `/parameter_events` publisher uses.
    pub parameter_events_qos: QosProfile,
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            namespace: crate::names::ROOT_NAMESPACE.to_owned(),
            remap: RemapRules::new(),
            start_parameter_services: true,
            use_sim_time: false,
            allow_undeclared_parameters: false,
            parameter_events_qos: QosProfile::parameter_events(),
        }
    }
}

impl NodeOptions {
    /// Options for a node in `namespace`.
    #[must_use]
    pub fn in_namespace(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            ..Self::default()
        }
    }

    /// Replace the namespace.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// Replace the remapping table.
    #[must_use]
    pub fn with_remap(mut self, remap: RemapRules) -> Self {
        self.remap = remap;
        self
    }

    /// Turn the parameter services on or off.
    #[must_use]
    pub const fn with_parameter_services(mut self, enabled: bool) -> Self {
        self.start_parameter_services = enabled;
        self
    }

    /// Set `use_sim_time` at construction.
    #[must_use]
    pub const fn with_sim_time(mut self, enabled: bool) -> Self {
        self.use_sim_time = enabled;
        self
    }

    /// Allow setting a parameter that was never declared.
    #[must_use]
    pub const fn with_undeclared_parameters(mut self, allowed: bool) -> Self {
        self.allow_undeclared_parameters = allowed;
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_defaults_are_the_ros_2_defaults() {
        let options = ContextOptions::default();
        assert_eq!(options.domain_id, 0);
        assert_eq!(options.compat, RosCompat::Jazzy);
        assert!(options.multicast);
        assert!(options.announce_graph);
        assert_eq!(options.enclave, "/");
    }

    #[test]
    fn the_loopback_preset_is_what_a_parallel_test_needs() {
        let options = ContextOptions::loopback();
        assert_eq!(options.bind, BindPolicy::EphemeralLoopback);
        assert!(
            !options.multicast,
            "a sandboxed host refuses IP_ADD_MEMBERSHIP; the deterministic path is unicast"
        );
        assert!(options.initial_peers.is_empty());
    }

    #[test]
    fn the_enclave_renders_the_way_ros_announces_it() {
        assert_eq!(ContextOptions::default().user_data(), b"enclave=/;");
        assert_eq!(
            ContextOptions::default()
                .with_enclave("/robot/arm")
                .user_data(),
            b"enclave=/robot/arm;"
        );
    }

    #[test]
    fn a_pinned_guid_prefix_is_used_verbatim() {
        let prefix = GuidPrefix::vendor_scoped(VendorId::ASTRS, [7; 10]);
        let options = ContextOptions::default().with_guid_prefix(prefix);
        assert_eq!(options.resolved_guid_prefix(), prefix);
    }

    #[test]
    fn a_generated_prefix_is_vendor_scoped_and_process_unique() {
        let options = ContextOptions::default();
        let first = options.resolved_guid_prefix();
        let second = options.resolved_guid_prefix();
        assert_ne!(first, second, "the counter makes each call distinct");
        assert_eq!(
            first.to_bytes()[..2],
            VendorId::ASTRS.to_bytes(),
            "every AstRS prefix opens with the vendor id"
        );
    }

    #[test]
    fn the_builders_compose() {
        let options = ContextOptions::new(7)
            .with_compat(RosCompat::Humble)
            .with_multicast(false)
            .with_peer(Locator::udpv4(Ipv4Addr::LOCALHOST, 7_410))
            .with_participant_id(3)
            .with_tick_period(StdDuration::from_millis(5))
            .with_graph_announcement(false)
            .with_standard_ports(Ipv4Addr::LOCALHOST);
        assert_eq!(options.domain_id, 7);
        assert_eq!(options.compat, RosCompat::Humble);
        assert!(!options.multicast);
        assert_eq!(options.initial_peers.len(), 1);
        assert_eq!(options.participant_id, 3);
        assert_eq!(options.tick_period, StdDuration::from_millis(5));
        assert!(!options.announce_graph);
        assert_eq!(options.bind, BindPolicy::Standard(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn node_options_default_to_the_root_namespace_with_parameters_on() {
        let options = NodeOptions::default();
        assert_eq!(options.namespace, "/");
        assert!(options.start_parameter_services);
        assert!(!options.use_sim_time);
        assert!(!options.allow_undeclared_parameters);
        assert!(options.remap.is_empty());
        assert_eq!(options.parameter_events_qos, QosProfile::parameter_events());
    }

    #[test]
    fn node_option_builders_compose() {
        let rules = RemapRules::new()
            .with(crate::names::RemapRule::new("scan", "/lidar/scan").expect("a valid rule"));
        let options = NodeOptions::in_namespace("/robot")
            .with_remap(rules.clone())
            .with_parameter_services(false)
            .with_sim_time(true)
            .with_undeclared_parameters(true)
            .with_namespace("/robot/arm");
        assert_eq!(options.namespace, "/robot/arm");
        assert_eq!(options.remap, rules);
        assert!(!options.start_parameter_services);
        assert!(options.use_sim_time);
        assert!(options.allow_undeclared_parameters);
    }
}
