//! [`UplinkConfig`] — everything the daemon↔coordinator leg needs to know
//! before it dials (blueprint §4.2, §12, §16, §24.2).
//!
//! One struct, built once by whoever starts the daemon (`astrs daemon
//! --coordinator …`, `astrs up`, a test harness), and read by the uplink task
//! on every reconnect. Nothing here is discovered: the address comes from the
//! operator, the token from §16's cluster secret, the machine name and labels
//! from the deployment, and the peer address from
//! [`crate::peer::PeerManager::bind`] — which is why it is set *after*
//! construction and before the first dial.
//!
//! # The reconnect ladder (§12)
//!
//! > *reconnects with backoff, resyncs via sequence-numbered `StateCatchUp`.*
//!
//! [`UplinkConfig::backoff_for`] is that ladder: `initial_backoff × 2ⁿ`,
//! capped at `max_backoff`, reset to the first rung by every successful
//! registration. It is deliberately *not* jittered — a cluster's daemons
//! are told apart by their own start times, and a deterministic ladder is one
//! fewer thing to explain when reading a log next to a wall clock.
//!
//! # Examples
//!
//! ```
//! use std::net::SocketAddr;
//! use std::time::Duration;
//! use astrs_daemon::coordinator::UplinkConfig;
//! use astrs_wire::AuthToken;
//!
//! let addr: SocketAddr = "127.0.0.1:7407".parse()?;
//! let config = UplinkConfig::new(addr, AuthToken::from_bytes([3; 32]))
//!     .with_label("zone", "front")
//!     .with_peer_address("tcp:10.0.0.4:7409");
//!
//! assert_eq!(config.label("zone"), Some("front"));
//! assert_eq!(config.backoff_for(0), UplinkConfig::DEFAULT_INITIAL_BACKOFF);
//! assert!(config.backoff_for(30) <= UplinkConfig::DEFAULT_MAX_BACKOFF);
//! # Ok::<(), std::net::AddrParseError>(())
//! ```

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use astrs_wire::{AuthToken, FrameLimits, MachineName};

/// The coordinator's default control port (§24.2: 7407, Atom's birthday).
pub const DEFAULT_COORDINATOR_PORT: u16 = 7407;

/// The environment variable that overrides it (§24.2).
pub const ENV_COORDINATOR_PORT: &str = "ASTRS_COORDINATOR_PORT";

/// The environment variable naming the coordinator's whole address.
///
/// Accepts `host:port` and a bare `host` (which takes
/// [`DEFAULT_COORDINATOR_PORT`]). Present so a containerised daemon needs no
/// command line at all.
pub const ENV_COORDINATOR_ADDR: &str = "ASTRS_COORDINATOR";

/// How the daemon reaches its coordinator.
#[derive(Debug, Clone)]
pub struct UplinkConfig {
    /// Where the coordinator listens (§4.2: TCP, daemons dial out).
    address: SocketAddr,
    /// The cluster token presented in the greeting (§16).
    auth: AuthToken,
    /// The machine this daemon claims, for `deploy.machine` placement (§8.3).
    machine: Option<MachineName>,
    /// Free-form placement labels (`zone: front`, `gpu: yes`).
    labels: BTreeMap<String, String>,
    /// The address *peers* dial this daemon on, announced in the
    /// registration. Set once [`crate::peer::PeerManager::bind`] knows it.
    peer_address: Option<String>,
    /// How long the greeting may take.
    handshake_timeout: Duration,
    /// How long a dial may take.
    dial_timeout: Duration,
    /// The first reconnect rung.
    initial_backoff: Duration,
    /// The ceiling every later rung is capped at.
    max_backoff: Duration,
    /// How many events may be buffered while the link is down (§12).
    buffer_capacity: usize,
    /// The frame budget the greeting proposes.
    limits: FrameLimits,
}

impl UplinkConfig {
    /// The first reconnect rung.
    pub const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(250);

    /// The ceiling every later rung is capped at.
    ///
    /// Five seconds is one heartbeat interval (§24.2): a coordinator that
    /// comes back is noticed within the same window a live link would have
    /// reported in, and a coordinator that stays down costs one dial per
    /// heartbeat rather than a busy loop.
    pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(5);

    /// How long the greeting may take before the dial is abandoned.
    pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    /// How many events are buffered while the link is down (§12).
    ///
    /// Bounded on purpose: a coordinator that has been gone for an hour must
    /// cost this daemon a fixed amount of memory, not an unbounded one. The
    /// eviction policy is in [`crate::coordinator::UplinkSink`] — liveness
    /// and telemetry are shed first, lifecycle facts last.
    pub const DEFAULT_BUFFER_CAPACITY: usize = 4096;

    /// A configuration dialling `address` with `auth`.
    #[must_use]
    pub fn new(address: SocketAddr, auth: AuthToken) -> Self {
        Self {
            address,
            auth,
            machine: None,
            labels: BTreeMap::new(),
            peer_address: None,
            handshake_timeout: Self::DEFAULT_HANDSHAKE_TIMEOUT,
            dial_timeout: Self::DEFAULT_HANDSHAKE_TIMEOUT,
            initial_backoff: Self::DEFAULT_INITIAL_BACKOFF,
            max_backoff: Self::DEFAULT_MAX_BACKOFF,
            buffer_capacity: Self::DEFAULT_BUFFER_CAPACITY,
            limits: FrameLimits::network(),
        }
    }

    /// Names the machine this daemon claims (§8.3 `deploy.machine`).
    #[must_use]
    pub fn with_machine(mut self, machine: MachineName) -> Self {
        self.machine = Some(machine);
        self
    }

    /// Adds one placement label.
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Replaces every placement label.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// Announces the address peers should dial this daemon on.
    #[must_use]
    pub fn with_peer_address(mut self, address: impl Into<String>) -> Self {
        self.peer_address = Some(address.into());
        self
    }

    /// Sets the greeting timeout.
    #[must_use]
    pub const fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Sets the dial timeout.
    #[must_use]
    pub const fn with_dial_timeout(mut self, timeout: Duration) -> Self {
        self.dial_timeout = timeout;
        self
    }

    /// Sets the reconnect ladder's first rung and ceiling.
    #[must_use]
    pub const fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max;
        self
    }

    /// Sets how many events may be buffered while the link is down.
    #[must_use]
    pub const fn with_buffer_capacity(mut self, capacity: usize) -> Self {
        self.buffer_capacity = capacity;
        self
    }

    /// Sets the frame budget the greeting proposes.
    #[must_use]
    pub const fn with_limits(mut self, limits: FrameLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Where the coordinator listens.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The cluster token (§16).
    #[must_use]
    pub const fn auth(&self) -> &AuthToken {
        &self.auth
    }

    /// The machine this daemon claims.
    #[must_use]
    pub const fn machine(&self) -> Option<&MachineName> {
        self.machine.as_ref()
    }

    /// Every placement label.
    #[must_use]
    pub const fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    /// One placement label.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// The address peers dial this daemon on, when one is known.
    #[must_use]
    pub fn peer_address(&self) -> Option<&str> {
        self.peer_address.as_deref()
    }

    /// The greeting timeout.
    #[must_use]
    pub const fn handshake_timeout(&self) -> Duration {
        self.handshake_timeout
    }

    /// The dial timeout.
    #[must_use]
    pub const fn dial_timeout(&self) -> Duration {
        self.dial_timeout
    }

    /// How many events may be buffered while the link is down.
    #[must_use]
    pub const fn buffer_capacity(&self) -> usize {
        self.buffer_capacity
    }

    /// The frame budget the greeting proposes.
    #[must_use]
    pub const fn limits(&self) -> FrameLimits {
        self.limits
    }

    /// The backoff after `attempt` consecutive failures (§12).
    ///
    /// `attempt` counts from zero, so the first retry waits
    /// `initial_backoff`. Saturating rather than wrapping: an uplink that has
    /// been failing for days must keep waiting `max_backoff`, not suddenly
    /// spin.
    #[must_use]
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        self.initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// Parses a coordinator address written as `host:port` or bare `host`.
///
/// A bare host takes [`DEFAULT_COORDINATOR_PORT`], honouring
/// [`ENV_COORDINATOR_PORT`] when it names a valid port. Resolution is
/// blocking, so this is meant for start-up rather than for the reconnect
/// path.
///
/// # Errors
///
/// [`crate::DaemonError::Configuration`] when the text resolves to no
/// address at all.
pub fn resolve_coordinator_addr(text: &str) -> crate::error::DaemonResult<SocketAddr> {
    use std::net::ToSocketAddrs as _;

    let default_port = std::env::var(ENV_COORDINATOR_PORT)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_COORDINATOR_PORT);
    let candidate = if text.contains(':') {
        text.to_owned()
    } else {
        format!("{text}:{default_port}")
    };
    candidate
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .ok_or_else(|| {
            crate::error::DaemonError::Configuration(format!(
                "no address resolves for coordinator {text:?}"
            ))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:7407".parse().expect("a literal address")
    }

    fn config() -> UplinkConfig {
        UplinkConfig::new(addr(), AuthToken::from_bytes([1; 32]))
    }

    #[test]
    fn a_fresh_config_announces_nothing_it_has_not_been_told() {
        let config = config();
        assert_eq!(config.address(), addr());
        assert!(config.machine().is_none());
        assert!(config.labels().is_empty());
        assert!(config.peer_address().is_none());
        assert_eq!(
            config.buffer_capacity(),
            UplinkConfig::DEFAULT_BUFFER_CAPACITY
        );
    }

    #[test]
    fn the_backoff_ladder_doubles_and_then_holds() {
        let config = config().with_backoff(Duration::from_millis(10), Duration::from_millis(80));
        assert_eq!(config.backoff_for(0), Duration::from_millis(10));
        assert_eq!(config.backoff_for(1), Duration::from_millis(20));
        assert_eq!(config.backoff_for(2), Duration::from_millis(40));
        assert_eq!(config.backoff_for(3), Duration::from_millis(80));
        assert_eq!(config.backoff_for(4), Duration::from_millis(80));
        assert_eq!(
            config.backoff_for(u32::MAX),
            Duration::from_millis(80),
            "a long outage never wraps into a busy loop"
        );
    }

    #[test]
    fn labels_and_machine_survive_the_builders() {
        let config = config()
            .with_machine(MachineName::new("robot-01").unwrap())
            .with_label("zone", "front")
            .with_peer_address("tcp:10.0.0.4:7409");
        assert_eq!(config.machine().map(MachineName::as_str), Some("robot-01"));
        assert_eq!(config.label("zone"), Some("front"));
        assert_eq!(config.label("gpu"), None);
        assert_eq!(config.peer_address(), Some("tcp:10.0.0.4:7409"));
    }

    #[test]
    fn replacing_the_label_set_replaces_it_wholesale() {
        let mut labels = BTreeMap::new();
        labels.insert("gpu".to_owned(), "yes".to_owned());
        let config = config().with_label("zone", "front").with_labels(labels);
        assert_eq!(config.label("gpu"), Some("yes"));
        assert_eq!(config.label("zone"), None);
    }

    #[test]
    fn a_host_and_port_resolves() {
        let resolved = resolve_coordinator_addr("127.0.0.1:7407").unwrap();
        assert_eq!(resolved, addr());
    }

    #[test]
    fn a_bare_host_takes_the_default_port() {
        let resolved = resolve_coordinator_addr("127.0.0.1").unwrap();
        assert_eq!(resolved.port(), DEFAULT_COORDINATOR_PORT);
    }

    #[test]
    fn an_unresolvable_address_is_a_typed_error() {
        let err = resolve_coordinator_addr("this.host.does.not.exist.invalid:1").unwrap_err();
        assert!(matches!(err, crate::error::DaemonError::Configuration(_)));
    }
}
