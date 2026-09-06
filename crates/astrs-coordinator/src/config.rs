//! Coordinator configuration: the listening address, the auth token, and
//! the connection/liveness policy (blueprint §4.2, §7.3, §12, §16).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use astrs_wire::AuthToken;

/// The coordinator's default TCP port (blueprint §4.2: *"7407 honors Atom's
/// birthday 2003-04-07"*).
pub const DEFAULT_COORDINATOR_PORT: u16 = 7407;

/// The environment variable overriding [`DEFAULT_COORDINATOR_PORT`]
/// (blueprint §24.2).
pub const ENV_COORDINATOR_PORT: &str = "ASTRS_COORDINATOR_PORT";

/// The default heartbeat interval the coordinator sends daemons on
/// (blueprint §24.2: 5 s).
pub const DEFAULT_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// The default number of consecutive missed heartbeats before a daemon is
/// marked degraded, then lost (task brief: *"heartbeat watchdog (mark
/// degraded/lost on 3 missed)"*).
pub const DEFAULT_MISSED_HEARTBEAT_LIMIT: u32 = 3;

/// The default handshake deadline.
pub const DEFAULT_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The coordinator's tunable policy.
///
/// # Examples
///
/// ```
/// use astrs_coordinator::CoordinatorConfig;
/// use astrs_wire::AuthToken;
///
/// let config = CoordinatorConfig::new(AuthToken::generate().unwrap()).with_port(0);
/// assert_eq!(config.bind_addr().port(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// The address to listen on. Port `0` (the default in tests) asks the OS
    /// for an ephemeral free port.
    bind_addr: SocketAddr,
    /// The cluster auth token every `Hello` is checked against (blueprint
    /// §16).
    pub token: AuthToken,
    /// How often the coordinator sends a daemon a heartbeat.
    pub heartbeat_interval: std::time::Duration,
    /// How many consecutive missed heartbeats mark a daemon degraded, and
    /// then (at twice this many) lost.
    pub missed_heartbeat_limit: u32,
    /// How long a `Hello` has to complete before the connection is dropped.
    pub handshake_timeout: std::time::Duration,
    /// The maximum number of concurrent connections this coordinator
    /// accepts, symmetric with the negotiated per-connection limits
    /// (blueprint §7.3: *"limits in config, enforced symmetrically"`).
    /// `None` means unbounded.
    pub connection_limit: Option<u32>,
    /// The maximum number of connections accepted from a single remote IP
    /// address. `None` means unbounded.
    pub per_ip_connection_limit: Option<u32>,
    /// The frame-size and timing budget this coordinator proposes and
    /// enforces on every connection.
    pub limits: astrs_wire::NegotiatedLimits,
    /// The maximum number of log records returned by one `Logs` request or
    /// forwarded through one `LogSubscribe` push before it is split.
    pub max_catch_up_page: usize,
}

impl CoordinatorConfig {
    /// A configuration listening on [`DEFAULT_COORDINATOR_PORT`] on every
    /// interface, with the blueprint's default policy.
    #[must_use]
    pub fn new(token: AuthToken) -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_COORDINATOR_PORT),
            token,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            missed_heartbeat_limit: DEFAULT_MISSED_HEARTBEAT_LIMIT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            connection_limit: None,
            per_ip_connection_limit: None,
            limits: astrs_wire::NegotiatedLimits::network(),
            max_catch_up_page: astrs_store::MAX_CATCH_UP_PAGE,
        }
    }

    /// Reads the port from [`ENV_COORDINATOR_PORT`], falling back to
    /// [`DEFAULT_COORDINATOR_PORT`] when unset or unparsable.
    #[must_use]
    pub fn from_env(token: AuthToken) -> Self {
        let mut config = Self::new(token);
        if let Ok(text) = std::env::var(ENV_COORDINATOR_PORT)
            && let Ok(port) = text.parse::<u16>()
        {
            config.bind_addr.set_port(port);
        }
        config
    }

    /// Overrides the bind address entirely.
    #[must_use]
    pub const fn with_bind_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    /// Overrides only the port, keeping the configured interface.
    #[must_use]
    pub const fn with_port(mut self, port: u16) -> Self {
        self.bind_addr.set_port(port);
        self
    }

    /// Sets the connection ceiling.
    #[must_use]
    pub const fn with_connection_limit(mut self, limit: Option<u32>) -> Self {
        self.connection_limit = limit;
        self
    }

    /// Sets the per-IP connection ceiling.
    #[must_use]
    pub const fn with_per_ip_connection_limit(mut self, limit: Option<u32>) -> Self {
        self.per_ip_connection_limit = limit;
        self
    }

    /// Sets the heartbeat interval and missed-heartbeat threshold together,
    /// since the daemon-liveness watchdog reasons about both at once.
    #[must_use]
    pub const fn with_heartbeat(
        mut self,
        interval: std::time::Duration,
        missed_limit: u32,
    ) -> Self {
        self.heartbeat_interval = interval;
        self.missed_heartbeat_limit = missed_limit;
        self
    }

    /// The address this coordinator listens on.
    #[must_use]
    pub const fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    /// The frame-level policy this coordinator enforces once a handshake
    /// completes.
    #[must_use]
    pub const fn frame_limits(&self) -> astrs_wire::FrameLimits {
        self.limits.to_frame_limits()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn token() -> AuthToken {
        AuthToken::from_bytes([7; 32])
    }

    #[test]
    fn defaults_match_the_blueprint() {
        let config = CoordinatorConfig::new(token());
        assert_eq!(config.bind_addr().port(), DEFAULT_COORDINATOR_PORT);
        assert_eq!(config.missed_heartbeat_limit, 3);
        assert_eq!(config.heartbeat_interval, std::time::Duration::from_secs(5));
    }

    #[test]
    fn with_port_keeps_the_configured_interface() {
        let addr: SocketAddr = "10.0.0.4:0".parse().unwrap();
        let config = CoordinatorConfig::new(token())
            .with_bind_addr(addr)
            .with_port(9999);
        assert_eq!(config.bind_addr().ip().to_string(), "10.0.0.4");
        assert_eq!(config.bind_addr().port(), 9999);
    }

    #[test]
    fn env_override_replaces_the_default_port() {
        // SAFETY-equivalent: `std::env::set_var` is not literally `unsafe`
        // here (pre-2024-semantics crate, single-threaded test), but tests
        // sharing an env var must not run concurrently with each other; this
        // test owns a variable name no other test in this crate touches.
        unsafe {
            std::env::set_var(ENV_COORDINATOR_PORT, "9001");
        }
        let config = CoordinatorConfig::from_env(token());
        assert_eq!(config.bind_addr().port(), 9001);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_PORT);
        }
    }

    #[test]
    fn a_malformed_env_port_falls_back_to_the_default() {
        unsafe {
            std::env::set_var(ENV_COORDINATOR_PORT, "not-a-port");
        }
        let config = CoordinatorConfig::from_env(token());
        assert_eq!(config.bind_addr().port(), DEFAULT_COORDINATOR_PORT);
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_PORT);
        }
    }

    #[test]
    fn builders_reach_every_field() {
        let config = CoordinatorConfig::new(token())
            .with_connection_limit(Some(64))
            .with_per_ip_connection_limit(Some(8))
            .with_heartbeat(std::time::Duration::from_secs(1), 5);
        assert_eq!(config.connection_limit, Some(64));
        assert_eq!(config.per_ip_connection_limit, Some(8));
        assert_eq!(config.missed_heartbeat_limit, 5);
    }
}
