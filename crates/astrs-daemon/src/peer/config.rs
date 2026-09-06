//! [`PeerConfig`] — how this daemon talks to other daemons (§6.4).
//!
//! > *QUIC (oxiquic) is the default daemon↔daemon transport … TCP fallback
//! > (same framing) for QUIC-hostile networks … Compression: per-route
//! > optional lz4/zstd (oxiarc) negotiated in route setup, applied to payloads
//! > ≥ 16 KiB.*
//!
//! Everything above is a decision, and every decision lives here rather than
//! in the code that dials: the peer port, the cluster token that authenticates
//! the greeting (§16), the codec offered at route setup, and the budgets the
//! handshake proposes.
//!
//! # QUIC
//!
//! The `quic` feature is off by default in `astrs-transport` because
//! `oxiquic`'s *server* constructors need `rustls` types this workspace does
//! not carry. A `quic:` peer address is therefore dialled over TCP with
//! byte-identical framing — the documented §23 risk-5 degradation — and
//! [`astrs_transport::backend::effective_plane`] reports which plane a dial
//! will really use. Nothing in this module changes when the feature is turned
//! on; the address simply resolves to a different backend.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::peer::PeerConfig;
//! use astrs_wire::{AuthToken, Compression};
//!
//! let config = PeerConfig::new(AuthToken::from_bytes([7; 32]))
//!     .with_compression(Compression::Lz4);
//!
//! assert_eq!(config.compression(), Compression::Lz4);
//! assert!(config.transport().compression.threshold_bytes >= 16 * 1024);
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use astrs_transport::{LocalIdentity, TransportConfig};
use astrs_wire::{AuthToken, Compression, DaemonId, NegotiatedLimits, Role, RoleSet};

/// The port a daemon listens on for other daemons.
///
/// The blueprint fixes the coordinator at 7407 and the node listener at 7408
/// (§24.2); the peer leg takes the next number so an operator reading a
/// firewall rule can tell the three apart at a glance.
pub const DEFAULT_PEER_PORT: u16 = 7409;

/// The environment variable that overrides it.
pub const ENV_PEER_PORT: &str = "ASTRS_PEER_PORT";

/// How long a dial may take before it is abandoned.
pub const DEFAULT_DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a peer may go unheard from before its routes are declared closed
/// (§12: "routes to an unreachable daemon flip to `InputClosed` after
/// `timeout`, recover with `InputRecovered`").
pub const DEFAULT_PEER_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the daemon probes a quiet peer (§24.1 `PeerEvent::Ping`).
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(5);

/// How this daemon reaches, and is reached by, its peers.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// The cluster token presented and demanded in every greeting (§16).
    auth: AuthToken,
    /// The transport policy: timeouts, compression thresholds, mux windows.
    transport: TransportConfig,
    /// The codec offered at route setup.
    compression: Compression,
    /// Where to listen for peers.
    listen: Option<SocketAddr>,
    /// How long a dial may take.
    dial_timeout: Duration,
    /// How long a silent peer's routes stay open.
    peer_timeout: Duration,
    /// How often a quiet peer is probed.
    ping_interval: Duration,
}

impl PeerConfig {
    /// A configuration authenticating with `auth`, listening nowhere.
    ///
    /// Listening is opt-in because most daemons are dialled *to* by exactly
    /// the peers the coordinator tells them about; a daemon that binds a port
    /// nobody was told about has opened a hole for nothing.
    #[must_use]
    pub fn new(auth: AuthToken) -> Self {
        Self {
            auth,
            transport: TransportConfig::new(),
            compression: Compression::None,
            listen: None,
            dial_timeout: DEFAULT_DIAL_TIMEOUT,
            peer_timeout: DEFAULT_PEER_TIMEOUT,
            ping_interval: DEFAULT_PING_INTERVAL,
        }
    }

    /// Listens on the loopback interface at `port`.
    ///
    /// Port `0` asks the operating system for a free one, which is what a test
    /// wants and what [`crate::peer::PeerManager::bind`] reports back.
    #[must_use]
    pub fn with_loopback(mut self, port: u16) -> Self {
        self.listen = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
        self
    }

    /// Listens on `addr`.
    #[must_use]
    pub const fn with_listen(mut self, addr: SocketAddr) -> Self {
        self.listen = Some(addr);
        self
    }

    /// Listens nowhere: this daemon only dials out.
    #[must_use]
    pub const fn without_listen(mut self) -> Self {
        self.listen = None;
        self
    }

    /// Replaces the transport policy.
    #[must_use]
    pub fn with_transport(mut self, transport: TransportConfig) -> Self {
        self.transport = transport;
        self
    }

    /// Offers `compression` at route setup (§6.4).
    #[must_use]
    pub const fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Sets the dial budget.
    #[must_use]
    pub const fn with_dial_timeout(mut self, timeout: Duration) -> Self {
        self.dial_timeout = timeout;
        self
    }

    /// Sets how long a silent peer's routes stay open (§12).
    #[must_use]
    pub const fn with_peer_timeout(mut self, timeout: Duration) -> Self {
        self.peer_timeout = timeout;
        self
    }

    /// Sets how often a quiet peer is probed.
    #[must_use]
    pub const fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// The cluster token.
    #[must_use]
    pub const fn auth(&self) -> &AuthToken {
        &self.auth
    }

    /// The transport policy.
    #[must_use]
    pub const fn transport(&self) -> &TransportConfig {
        &self.transport
    }

    /// The codec offered at route setup.
    #[must_use]
    pub const fn compression(&self) -> Compression {
        self.compression
    }

    /// Where this daemon listens for peers, if it does.
    #[must_use]
    pub const fn listen(&self) -> Option<SocketAddr> {
        self.listen
    }

    /// The dial budget.
    #[must_use]
    pub const fn dial_timeout(&self) -> Duration {
        self.dial_timeout
    }

    /// How long a silent peer's routes stay open.
    #[must_use]
    pub const fn peer_timeout(&self) -> Duration {
        self.peer_timeout
    }

    /// How often a quiet peer is probed.
    #[must_use]
    pub const fn ping_interval(&self) -> Duration {
        self.ping_interval
    }

    /// The limits this daemon proposes on a peer connection.
    ///
    /// A peer leg is a network leg, so the checksum is mandatory (§7.1) —
    /// [`astrs_wire::Plane::needs_crc`] answers `true` for TCP, and the proposal
    /// reflects it rather than leaving the two to disagree.
    #[must_use]
    pub fn limits(&self) -> NegotiatedLimits {
        self.transport.proposed_limits(true)
    }

    /// The identity this daemon presents to a peer (§7.2).
    ///
    /// The label carries the [`DaemonId`] in its text form, which is how the
    /// acceptor learns *which* daemon dialled it: the greeting has a role and
    /// a label and nothing else, and a peer table keyed by address rather than
    /// identity cannot survive a daemon moving.
    #[must_use]
    pub fn identity(&self, local: &DaemonId) -> LocalIdentity {
        LocalIdentity::new(Role::Peer).with_label(local.to_string())
    }

    /// The roles this daemon's peer port serves.
    #[must_use]
    pub const fn accepted_roles() -> RoleSet {
        RoleSet::EMPTY.with(Role::Peer)
    }
}

impl Default for PeerConfig {
    /// A configuration with no token, for a single-machine run where no peer
    /// leg is ever dialled.
    fn default() -> Self {
        Self::new(AuthToken::ZERO)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let config = PeerConfig::default();
        assert_eq!(config.compression(), Compression::None);
        assert_eq!(config.dial_timeout(), DEFAULT_DIAL_TIMEOUT);
        assert_eq!(config.peer_timeout(), DEFAULT_PEER_TIMEOUT);
        assert_eq!(config.ping_interval(), DEFAULT_PING_INTERVAL);
        assert!(config.listen().is_none(), "listening is opt-in");
        assert_eq!(DEFAULT_PEER_PORT, 7409);
    }

    #[test]
    fn a_loopback_listener_can_ask_for_any_port() {
        let config = PeerConfig::default().with_loopback(0);
        let addr = config.listen().expect("listening");
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn an_explicit_address_is_kept_and_can_be_removed() {
        let addr: SocketAddr = "10.0.0.4:7409".parse().unwrap();
        let config = PeerConfig::default().with_listen(addr);
        assert_eq!(config.listen(), Some(addr));
        assert!(config.without_listen().listen().is_none());
    }

    #[test]
    fn a_peer_leg_always_proposes_a_checksum() {
        let config = PeerConfig::default();
        assert!(
            config.limits().require_crc,
            "§7.1: the checksum is mandatory on a network leg"
        );
    }

    #[test]
    fn the_identity_carries_the_daemon_id() {
        let local = DaemonId::generate(None);
        let identity = PeerConfig::default().identity(&local);
        assert_eq!(identity.role, Role::Peer);
        assert_eq!(identity.label.as_deref(), Some(local.to_string().as_str()));
        assert_eq!(
            identity
                .label
                .and_then(|label| label.parse::<DaemonId>().ok()),
            Some(local)
        );
    }

    #[test]
    fn a_peer_port_serves_peers_and_nobody_else() {
        let roles = PeerConfig::accepted_roles();
        assert!(roles.contains(Role::Peer));
        assert!(!roles.contains(Role::Node));
        assert!(!roles.contains(Role::Cli));
    }

    #[test]
    fn the_builders_replace_what_they_name() {
        let config = PeerConfig::new(AuthToken::from_bytes([3; 32]))
            .with_compression(Compression::Zstd)
            .with_dial_timeout(Duration::from_millis(500))
            .with_peer_timeout(Duration::from_secs(2))
            .with_ping_interval(Duration::from_millis(100))
            .with_transport(TransportConfig::new());

        assert_eq!(config.auth(), &AuthToken::from_bytes([3; 32]));
        assert_eq!(config.compression(), Compression::Zstd);
        assert_eq!(config.dial_timeout(), Duration::from_millis(500));
        assert_eq!(config.peer_timeout(), Duration::from_secs(2));
        assert_eq!(config.ping_interval(), Duration::from_millis(100));
    }

    #[test]
    fn compression_below_the_threshold_is_never_applied() {
        let config = PeerConfig::default().with_compression(Compression::Zstd);
        assert_eq!(config.transport().compression.threshold_bytes, 16 * 1024);
    }
}
