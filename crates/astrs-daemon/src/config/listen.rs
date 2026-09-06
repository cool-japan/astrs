//! [`ListenConfig`] — where nodes reach the daemon (§4.2, §24.2).
//!
//! > *Local node listener on **TCP 7408** (loopback) and a Unix domain socket
//! > (`$XDG_RUNTIME_DIR/astrs/daemon.sock`) — UDS preferred, TCP fallback.*
//!
//! Both may be enabled at once, and at least one must be. The order matters
//! twice over: it is the order the addresses are written into a spawned node's
//! [`astrs_wire::NodeConfig::endpoints`] (so the node dials the cheap one
//! first), and it is the order the daemon's own diagnostics list them in.
//!
//! The TCP listener binds loopback only. A daemon is a *local* node listener;
//! cross-host traffic goes daemon↔daemon over the peer plane, never
//! node↔remote-daemon, so a listener on `0.0.0.0` would be an attack surface
//! with no user.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::config::{ListenConfig, RuntimePaths};
//!
//! let listen = ListenConfig::defaults(&RuntimePaths::under("/run/astrs"));
//! assert_eq!(listen.tcp_port(), Some(7408));
//! assert!(listen.uds_path().is_some());
//!
//! // A test binds an ephemeral port instead.
//! let ephemeral = ListenConfig::loopback_tcp(0);
//! assert_eq!(ephemeral.tcp_port(), Some(0));
//! assert!(ephemeral.uds_path().is_none());
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use crate::config::paths::{RuntimePaths, check_socket_path_len};
use crate::error::{DaemonError, DaemonResult};

/// The default daemon node port (§24.2).
pub const DEFAULT_DAEMON_PORT: u16 = astrs_transport::DEFAULT_DAEMON_PORT;

/// The environment variable overriding it (§24.2).
pub const ENV_DAEMON_PORT: &str = "ASTRS_DAEMON_PORT";

/// Which local listeners the daemon opens.
///
/// [`Default`] is [`ListenConfig::none`] — an explicit, empty configuration
/// rather than a guess about the runtime directory. [`ListenConfig::defaults`]
/// is the one that applies the blueprint's §24.2 values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListenConfig {
    /// The Unix socket to bind, if any.
    uds: Option<PathBuf>,
    /// The loopback TCP address to bind, if any.
    tcp: Option<SocketAddr>,
}

impl ListenConfig {
    /// No listeners — the configuration an in-process `astrs run` uses, where
    /// the CLI hosts the daemon and every node is spawned by it (§4.2).
    ///
    /// A daemon with no listener still supervises processes; it simply has no
    /// socket for a *dynamic* node to attach to.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            uds: None,
            tcp: None,
        }
    }

    /// Both listeners, with the blueprint defaults.
    #[must_use]
    pub fn defaults(paths: &RuntimePaths) -> Self {
        Self {
            uds: Some(paths.socket_path()),
            tcp: Some(loopback(port_from_env())),
        }
    }

    /// A Unix socket only.
    #[must_use]
    pub fn uds(path: impl Into<PathBuf>) -> Self {
        Self {
            uds: Some(path.into()),
            tcp: None,
        }
    }

    /// A loopback TCP listener only, on `port` (`0` binds an ephemeral port).
    #[must_use]
    pub fn loopback_tcp(port: u16) -> Self {
        Self {
            uds: None,
            tcp: Some(loopback(port)),
        }
    }

    /// Adds a Unix socket to this configuration.
    #[must_use]
    pub fn with_uds(mut self, path: impl Into<PathBuf>) -> Self {
        self.uds = Some(path.into());
        self
    }

    /// Adds a loopback TCP listener on `port`.
    #[must_use]
    pub fn with_tcp_port(mut self, port: u16) -> Self {
        self.tcp = Some(loopback(port));
        self
    }

    /// Adds a TCP listener at an explicit address.
    ///
    /// Only loopback addresses are accepted — see the module documentation.
    ///
    /// # Errors
    ///
    /// [`DaemonError::BadPath`] if `addr` is not a loopback address.
    pub fn with_tcp_addr(mut self, addr: SocketAddr) -> DaemonResult<Self> {
        if !addr.ip().is_loopback() {
            return Err(DaemonError::BadPath {
                what: "node listener",
                path: PathBuf::from(addr.to_string()),
                reason: "the node listener binds loopback only; \
                         cross-host traffic uses the peer plane"
                    .into(),
            });
        }
        self.tcp = Some(addr);
        Ok(self)
    }

    /// Removes the Unix socket.
    #[must_use]
    pub fn without_uds(mut self) -> Self {
        self.uds = None;
        self
    }

    /// Removes the TCP listener.
    #[must_use]
    pub fn without_tcp(mut self) -> Self {
        self.tcp = None;
        self
    }

    /// The Unix socket, if one is configured.
    #[must_use]
    pub fn uds_path(&self) -> Option<&Path> {
        self.uds.as_deref()
    }

    /// The TCP address, if one is configured.
    #[must_use]
    pub const fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp
    }

    /// The TCP port, if one is configured.
    #[must_use]
    pub fn tcp_port(&self) -> Option<u16> {
        self.tcp.map(|addr| addr.port())
    }

    /// Whether any listener at all is configured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.uds.is_none() && self.tcp.is_none()
    }

    /// Checks the configuration is usable.
    ///
    /// # Errors
    ///
    /// [`DaemonError::BadPath`] if the socket path cannot fit in a
    /// `sockaddr_un`. An empty configuration is *not* an error here — see
    /// [`ListenConfig::none`] — but [`ListenConfig::require_listener`] refuses
    /// it for a caller that needs one.
    pub fn validate(&self) -> DaemonResult<()> {
        match &self.uds {
            Some(path) => check_socket_path_len(path),
            None => Ok(()),
        }
    }

    /// Refuses an empty configuration.
    ///
    /// # Errors
    ///
    /// [`DaemonError::NoListener`] if neither listener is configured.
    pub fn require_listener(&self) -> DaemonResult<()> {
        if self.is_empty() {
            return Err(DaemonError::NoListener);
        }
        self.validate()
    }

    /// The dial addresses a spawned node should try, cheapest first.
    ///
    /// This is the list that goes into [`astrs_wire::NodeConfig::endpoints`];
    /// the strings are `astrs_transport::TransportAddr`'s text form.
    #[must_use]
    pub fn endpoints(&self) -> Vec<String> {
        let mut endpoints = Vec::with_capacity(2);
        if let Some(path) = &self.uds {
            endpoints.push(astrs_transport::TransportAddr::uds(path.clone()).to_string());
        }
        if let Some(addr) = self.tcp {
            endpoints.push(astrs_transport::TransportAddr::tcp(addr).to_string());
        }
        endpoints
    }

    /// Replaces the TCP address with the one actually bound.
    ///
    /// A configuration asking for port `0` becomes a configuration naming the
    /// port the kernel chose, so the endpoints handed to nodes are dialable.
    pub fn set_bound_tcp(&mut self, addr: SocketAddr) {
        self.tcp = Some(addr);
    }
}

/// The loopback address for `port`.
#[must_use]
pub const fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// The port `ASTRS_DAEMON_PORT` selects, or the §24.2 default.
///
/// An unparseable value falls back to the default rather than failing the
/// daemon's start: the variable is an operator convenience, and a typo in it
/// should not take a robot down. It is worth a log line, which the caller
/// emits — this crate's configuration layer does not log.
#[must_use]
pub fn port_from_env() -> u16 {
    std::env::var(ENV_DAEMON_PORT)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_DAEMON_PORT)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_default_port_is_the_documented_one() {
        assert_eq!(DEFAULT_DAEMON_PORT, 7408);
        assert_eq!(ENV_DAEMON_PORT, "ASTRS_DAEMON_PORT");
    }

    #[test]
    fn defaults_open_both_listeners() {
        let paths = RuntimePaths::under("/run/astrs");
        let listen = ListenConfig::defaults(&paths);
        assert_eq!(listen.uds_path(), Some(Path::new("/run/astrs/daemon.sock")));
        assert_eq!(listen.tcp_port(), Some(DEFAULT_DAEMON_PORT));
        assert!(!listen.is_empty());
    }

    #[test]
    fn an_empty_configuration_is_allowed_but_not_when_a_listener_is_required() {
        let listen = ListenConfig::none();
        assert!(listen.is_empty());
        listen.validate().unwrap();
        assert!(matches!(
            listen.require_listener(),
            Err(DaemonError::NoListener)
        ));
    }

    #[test]
    fn endpoints_are_listed_uds_first() {
        let listen = ListenConfig::uds("/run/astrs/daemon.sock").with_tcp_port(7408);
        let endpoints = listen.endpoints();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints[0].starts_with("uds:"), "{:?}", endpoints[0]);
        assert!(endpoints[1].starts_with("tcp:"), "{:?}", endpoints[1]);
    }

    #[test]
    fn endpoints_round_trip_through_the_transport_address_parser() {
        let listen = ListenConfig::uds("/run/astrs/daemon.sock").with_tcp_port(7408);
        for endpoint in listen.endpoints() {
            let parsed: astrs_transport::TransportAddr = endpoint.parse().unwrap_or_else(|error| {
                panic!("{endpoint} should parse as a transport address: {error}")
            });
            assert_eq!(parsed.to_string(), endpoint);
        }
    }

    #[test]
    fn a_non_loopback_tcp_address_is_refused() {
        let addr: SocketAddr = "0.0.0.0:7408".parse().unwrap();
        let error = ListenConfig::none().with_tcp_addr(addr).unwrap_err();
        assert!(error.to_string().contains("loopback"), "{error}");
    }

    #[test]
    fn an_explicit_loopback_address_is_accepted() {
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let listen = ListenConfig::none().with_tcp_addr(addr).unwrap();
        assert_eq!(listen.tcp_addr(), Some(addr));
    }

    #[test]
    fn removing_listeners_works_both_ways() {
        let both = ListenConfig::uds("/a.sock").with_tcp_port(1);
        assert!(both.clone().without_uds().uds_path().is_none());
        assert!(both.clone().without_tcp().tcp_addr().is_none());
        assert!(both.without_uds().without_tcp().is_empty());
    }

    #[test]
    fn the_bound_port_replaces_the_requested_one() {
        let mut listen = ListenConfig::loopback_tcp(0);
        assert_eq!(listen.tcp_port(), Some(0));
        listen.set_bound_tcp(loopback(45_123));
        assert_eq!(listen.tcp_port(), Some(45_123));
        assert!(listen.endpoints()[0].contains("45123"));
    }

    #[test]
    fn an_over_long_socket_path_fails_validation() {
        let listen = ListenConfig::uds(format!("/tmp/{}/d.sock", "x".repeat(200)));
        assert!(listen.validate().is_err());
    }
}
