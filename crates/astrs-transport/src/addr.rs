//! Transport addresses: one text form for all three planes.
//!
//! A daemon's peer list, a manifest's `deploy:` block and an environment
//! override all name endpoints as text. [`TransportAddr`] is the parsed form,
//! and it round-trips: `addr.to_string().parse()` is always the same address.
//!
//! | Scheme | Example | Plane |
//! |---|---|---|
//! | `uds:` | `uds:/run/astrs/daemon.sock` | [`Plane::Uds`] |
//! | `tcp:` | `tcp:10.0.0.4:7407` | [`Plane::Tcp`] |
//! | `quic:` | `quic:10.0.0.4:7407` | [`Plane::Quic`] |
//!
//! A bare `host:port` with no scheme parses as TCP, because that is what an
//! operator means when they type `--coordinator 10.0.0.4:7407`. A bare path
//! (anything starting with `/` or `.`) parses as UDS for the same reason.
//!
//! The examples and tests in this module are the one place in the crate where
//! absolute paths appear as literals. They are the parser's *input*, not a
//! filesystem location: nothing here opens, creates or removes a file, and
//! recognising an absolute path is precisely the behaviour under test. Every
//! test that touches a real socket builds its path from
//! [`std::env::temp_dir`].
//!
//! # Examples
//!
//! ```
//! use astrs_transport::TransportAddr;
//! use astrs_wire::Plane;
//!
//! let quic: TransportAddr = "quic:127.0.0.1:7407".parse()?;
//! assert_eq!(quic.plane(), Plane::Quic);
//! assert_eq!(quic.to_string(), "quic:127.0.0.1:7407");
//!
//! // Scheme-less shorthands, for the command line.
//! assert_eq!("127.0.0.1:7407".parse::<TransportAddr>()?.plane(), Plane::Tcp);
//! assert_eq!("/tmp/astrs.sock".parse::<TransportAddr>()?.plane(), Plane::Uds);
//! # Ok::<(), astrs_transport::AddressError>(())
//! ```

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use astrs_wire::Plane;

use crate::error::AddressError;

/// The `uds:` scheme prefix.
pub const UDS_SCHEME: &str = "uds";
/// The `tcp:` scheme prefix.
pub const TCP_SCHEME: &str = "tcp";
/// The `quic:` scheme prefix.
pub const QUIC_SCHEME: &str = "quic";

/// The default coordinator port (blueprint §24.2).
pub const DEFAULT_COORDINATOR_PORT: u16 = 7407;
/// The default daemon node port (blueprint §24.2).
pub const DEFAULT_DAEMON_PORT: u16 = 7408;

/// Where a transport connection goes, and over which plane.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum TransportAddr {
    /// A Unix domain socket path — node↔daemon.
    Uds(PathBuf),
    /// A TCP socket address — the fallback everywhere.
    Tcp(SocketAddr),
    /// A QUIC (UDP) socket address — daemon↔daemon and daemon↔coordinator.
    Quic(SocketAddr),
}

impl TransportAddr {
    /// A UDS address for `path`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::TransportAddr;
    ///
    /// let addr = TransportAddr::uds("/tmp/astrs-daemon.sock");
    /// assert!(addr.is_same_host());
    /// ```
    #[must_use]
    pub fn uds(path: impl Into<PathBuf>) -> Self {
        Self::Uds(path.into())
    }

    /// A TCP address.
    #[must_use]
    pub const fn tcp(addr: SocketAddr) -> Self {
        Self::Tcp(addr)
    }

    /// A QUIC address.
    #[must_use]
    pub const fn quic(addr: SocketAddr) -> Self {
        Self::Quic(addr)
    }

    /// The wire-level plane this address selects.
    #[must_use]
    pub const fn plane(&self) -> Plane {
        match self {
            Self::Uds(_) => Plane::Uds,
            Self::Tcp(_) => Plane::Tcp,
            Self::Quic(_) => Plane::Quic,
        }
    }

    /// The scheme prefix this address renders with.
    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        match self {
            Self::Uds(_) => UDS_SCHEME,
            Self::Tcp(_) => TCP_SCHEME,
            Self::Quic(_) => QUIC_SCHEME,
        }
    }

    /// The socket path, for a UDS address.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Uds(path) => Some(path.as_path()),
            _ => None,
        }
    }

    /// The socket address, for a network address.
    #[must_use]
    pub const fn socket_addr(&self) -> Option<SocketAddr> {
        match self {
            Self::Tcp(addr) | Self::Quic(addr) => Some(*addr),
            Self::Uds(_) => None,
        }
    }

    /// Whether this address can only reach a peer on this machine.
    ///
    /// Loopback network addresses are still *network* addresses: they cross a
    /// socket API that a firewall or a namespace can interpose on, so only UDS
    /// answers `true`.
    #[must_use]
    pub const fn is_same_host(&self) -> bool {
        matches!(self, Self::Uds(_))
    }

    /// Whether a checksum is mandatory on this plane (§7.1).
    ///
    /// CRC is mandatory on network legs and optional on UDS, where the kernel
    /// already guarantees the bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::TransportAddr;
    ///
    /// assert!(!TransportAddr::uds("/tmp/s").requires_crc());
    /// assert!("tcp:127.0.0.1:1".parse::<TransportAddr>()?.requires_crc());
    /// # Ok::<(), astrs_transport::AddressError>(())
    /// ```
    #[must_use]
    pub const fn requires_crc(&self) -> bool {
        !self.is_same_host()
    }

    /// The same endpoint reached over TCP instead of QUIC.
    ///
    /// This is the §23 risk-5 fallback in one call: when a QUIC dial fails on a
    /// QUIC-hostile network, the daemon retries the identical `host:port` over
    /// TCP, where the framing is byte-for-byte the same.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::TransportAddr;
    /// use astrs_wire::Plane;
    ///
    /// let quic: TransportAddr = "quic:10.0.0.1:7407".parse()?;
    /// assert_eq!(quic.fallback_to_tcp().map(|a| a.plane()), Some(Plane::Tcp));
    /// # Ok::<(), astrs_transport::AddressError>(())
    /// ```
    #[must_use]
    pub const fn fallback_to_tcp(&self) -> Option<Self> {
        match self {
            Self::Quic(addr) => Some(Self::Tcp(*addr)),
            _ => None,
        }
    }

    /// Parses an address, defaulting a missing port to `default_port`.
    ///
    /// Used by the CLI, where `--coordinator 10.0.0.4` should mean port 7407.
    ///
    /// # Errors
    ///
    /// [`AddressError`] as [`FromStr`], plus a malformed host.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::{TransportAddr, DEFAULT_COORDINATOR_PORT};
    ///
    /// let addr = TransportAddr::parse_with_default_port("127.0.0.1", DEFAULT_COORDINATOR_PORT)?;
    /// assert_eq!(addr.to_string(), "tcp:127.0.0.1:7407");
    /// # Ok::<(), astrs_transport::AddressError>(())
    /// ```
    pub fn parse_with_default_port(text: &str, default_port: u16) -> Result<Self, AddressError> {
        match text.parse::<Self>() {
            Ok(addr) => Ok(addr),
            Err(original) => {
                let (scheme, body) = split_scheme(text);
                // Only retry the network schemes: a UDS path never has a port.
                if matches!(scheme, Some(UDS_SCHEME)) || looks_like_path(body) {
                    return Err(original);
                }
                let with_port = format!("{body}:{default_port}");
                let addr =
                    with_port
                        .parse::<SocketAddr>()
                        .map_err(|err| AddressError::Malformed {
                            scheme: scheme.unwrap_or(TCP_SCHEME),
                            body: text.to_string(),
                            detail: err.to_string(),
                        })?;
                Ok(match scheme {
                    Some(QUIC_SCHEME) => Self::Quic(addr),
                    _ => Self::Tcp(addr),
                })
            }
        }
    }
}

impl fmt::Display for TransportAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uds(path) => write!(f, "{UDS_SCHEME}:{}", path.display()),
            Self::Tcp(addr) => write!(f, "{TCP_SCHEME}:{addr}"),
            Self::Quic(addr) => write!(f, "{QUIC_SCHEME}:{addr}"),
        }
    }
}

/// Splits `scheme:body`, returning `(None, text)` when there is no known
/// scheme prefix.
///
/// A Windows-style drive letter or an IPv6 literal both contain colons, so the
/// prefix only counts when it is one of the three schemes this crate defines.
fn split_scheme(text: &str) -> (Option<&'static str>, &str) {
    for scheme in [UDS_SCHEME, TCP_SCHEME, QUIC_SCHEME] {
        if let Some(rest) = text.strip_prefix(scheme)
            && let Some(body) = rest.strip_prefix(':')
        {
            // Tolerate the URL-ish `scheme://body` spelling operators type.
            let body = body.strip_prefix("//").unwrap_or(body);
            return (Some(scheme), body);
        }
    }
    (None, text)
}

/// Whether scheme-less text should be read as a filesystem path.
fn looks_like_path(text: &str) -> bool {
    text.starts_with('/') || text.starts_with('.') || text.starts_with('~')
}

impl FromStr for TransportAddr {
    type Err = AddressError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(AddressError::Empty);
        }

        let (scheme, body) = split_scheme(trimmed);
        match scheme {
            Some(UDS_SCHEME) => {
                if body.is_empty() {
                    return Err(AddressError::Malformed {
                        scheme: UDS_SCHEME,
                        body: trimmed.to_string(),
                        detail: "socket path is empty".into(),
                    });
                }
                Ok(Self::Uds(PathBuf::from(body)))
            }
            Some(scheme @ (TCP_SCHEME | QUIC_SCHEME)) => {
                let addr = body
                    .parse::<SocketAddr>()
                    .map_err(|err| AddressError::Malformed {
                        scheme,
                        body: body.to_string(),
                        detail: err.to_string(),
                    })?;
                Ok(if scheme == QUIC_SCHEME {
                    Self::Quic(addr)
                } else {
                    Self::Tcp(addr)
                })
            }
            Some(other) => Err(AddressError::UnknownScheme {
                scheme: other.to_string(),
            }),
            None => {
                if looks_like_path(body) {
                    return Ok(Self::Uds(PathBuf::from(body)));
                }
                if let Ok(addr) = body.parse::<SocketAddr>() {
                    return Ok(Self::Tcp(addr));
                }
                // Anything left with a scheme-looking prefix names a plane we
                // do not implement; report that rather than a parse failure.
                if let Some((maybe_scheme, _)) = body.split_once(':')
                    && !maybe_scheme.is_empty()
                    && maybe_scheme.chars().all(|ch| ch.is_ascii_alphabetic())
                    && body.matches(':').count() == 1
                    && body
                        .rsplit(':')
                        .next()
                        .is_some_and(|port| port.parse::<u16>().is_err())
                {
                    return Err(AddressError::UnknownScheme {
                        scheme: maybe_scheme.to_string(),
                    });
                }
                Err(AddressError::Malformed {
                    scheme: TCP_SCHEME,
                    body: body.to_string(),
                    detail: "expected host:port, a socket path, or a scheme prefix".into(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_scheme_round_trips() {
        for text in [
            "uds:/run/astrs/daemon.sock",
            "tcp:127.0.0.1:7407",
            "quic:127.0.0.1:7407",
            "tcp:[::1]:7407",
            "quic:[2001:db8::1]:9",
        ] {
            let addr: TransportAddr = text.parse().unwrap();
            assert_eq!(addr.to_string(), text, "round trip failed for {text}");
            assert_eq!(addr.to_string().parse::<TransportAddr>().unwrap(), addr);
        }
    }

    #[test]
    fn the_url_ish_spelling_is_tolerated() {
        let addr: TransportAddr = "tcp://127.0.0.1:7407".parse().unwrap();
        assert_eq!(addr, TransportAddr::Tcp("127.0.0.1:7407".parse().unwrap()));
        // …and normalises to the canonical form.
        assert_eq!(addr.to_string(), "tcp:127.0.0.1:7407");
    }

    #[test]
    fn scheme_less_shorthands_pick_the_obvious_plane() {
        assert_eq!(
            "127.0.0.1:7407".parse::<TransportAddr>().unwrap().plane(),
            Plane::Tcp
        );
        assert_eq!(
            "/tmp/astrs.sock".parse::<TransportAddr>().unwrap().plane(),
            Plane::Uds
        );
        assert_eq!(
            "./astrs.sock".parse::<TransportAddr>().unwrap().plane(),
            Plane::Uds
        );
    }

    #[test]
    fn an_empty_address_is_rejected() {
        assert_eq!("".parse::<TransportAddr>(), Err(AddressError::Empty));
        assert_eq!("   ".parse::<TransportAddr>(), Err(AddressError::Empty));
    }

    #[test]
    fn an_unknown_scheme_says_so() {
        let err = "ftp:host".parse::<TransportAddr>().unwrap_err();
        assert!(matches!(err, AddressError::UnknownScheme { .. }));
    }

    #[test]
    fn an_empty_uds_path_is_rejected() {
        let err = "uds:".parse::<TransportAddr>().unwrap_err();
        assert!(matches!(err, AddressError::Malformed { .. }));
    }

    #[test]
    fn a_bad_socket_address_is_rejected() {
        let err = "tcp:not-an-address".parse::<TransportAddr>().unwrap_err();
        assert!(matches!(err, AddressError::Malformed { .. }));
        let err = "quic:127.0.0.1".parse::<TransportAddr>().unwrap_err();
        assert!(matches!(err, AddressError::Malformed { .. }));
    }

    #[test]
    fn a_missing_port_can_be_defaulted() {
        let addr =
            TransportAddr::parse_with_default_port("10.0.0.4", DEFAULT_COORDINATOR_PORT).unwrap();
        assert_eq!(addr.to_string(), "tcp:10.0.0.4:7407");

        let quic =
            TransportAddr::parse_with_default_port("quic:10.0.0.4", DEFAULT_DAEMON_PORT).unwrap();
        assert_eq!(quic.to_string(), "quic:10.0.0.4:7408");

        // An address that already has a port keeps it.
        let explicit =
            TransportAddr::parse_with_default_port("10.0.0.4:9", DEFAULT_COORDINATOR_PORT).unwrap();
        assert_eq!(explicit.to_string(), "tcp:10.0.0.4:9");
    }

    #[test]
    fn defaulting_a_port_never_rewrites_a_uds_path() {
        let err =
            TransportAddr::parse_with_default_port("uds:", DEFAULT_COORDINATOR_PORT).unwrap_err();
        assert!(matches!(err, AddressError::Malformed { .. }));
    }

    #[test]
    fn crc_is_mandatory_off_host_only() {
        assert!(!TransportAddr::uds("/tmp/x").requires_crc());
        assert!(TransportAddr::Tcp("127.0.0.1:1".parse().unwrap()).requires_crc());
        assert!(TransportAddr::Quic("127.0.0.1:1".parse().unwrap()).requires_crc());
    }

    #[test]
    fn quic_falls_back_to_tcp_at_the_same_endpoint() {
        let quic: TransportAddr = "quic:10.0.0.1:7407".parse().unwrap();
        let tcp = quic.fallback_to_tcp().unwrap();
        assert_eq!(tcp.to_string(), "tcp:10.0.0.1:7407");
        assert_eq!(tcp.fallback_to_tcp(), None);
        assert_eq!(TransportAddr::uds("/tmp/x").fallback_to_tcp(), None);
    }

    #[test]
    fn accessors_answer_for_their_own_variant_only() {
        let uds = TransportAddr::uds("/tmp/x");
        assert_eq!(uds.path(), Some(Path::new("/tmp/x")));
        assert_eq!(uds.socket_addr(), None);
        assert_eq!(uds.scheme(), "uds");

        let tcp = TransportAddr::Tcp("127.0.0.1:1".parse().unwrap());
        assert_eq!(tcp.path(), None);
        assert!(tcp.socket_addr().is_some());
        assert_eq!(tcp.scheme(), "tcp");
        assert_eq!(
            TransportAddr::Quic("127.0.0.1:1".parse().unwrap()).scheme(),
            "quic"
        );
    }

    #[test]
    fn addresses_sort_and_hash_for_use_as_map_keys() {
        use std::collections::BTreeSet;

        let mut set = BTreeSet::new();
        set.insert(TransportAddr::uds("/tmp/a"));
        set.insert(TransportAddr::uds("/tmp/a"));
        set.insert(TransportAddr::Tcp("127.0.0.1:1".parse().unwrap()));
        assert_eq!(set.len(), 2);
    }
}
