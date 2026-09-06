//! The three substrates AstRS runs on (blueprint §6.4).
//!
//! | Module | Plane | Used for | Checksum |
//! |---|---|---|---|
//! | [`uds`] | [`Plane::Uds`] | node ↔ daemon | optional (§7.1) |
//! | [`tcp`] | [`Plane::Tcp`] | fallback everywhere | mandatory |
//! | `quic` (feature `quic`) | [`Plane::Quic`] | daemon ↔ daemon, daemon ↔ coordinator | mandatory |
//!
//! Each module contributes exactly two things: a way to get a byte stream, and
//! the [`PeerIdentity`](crate::PeerIdentity) that goes with it. Everything
//! after that — the greeting, the frame codec, the route mux, the counters —
//! is shared, which is what makes "the TCP fallback has identical framing"
//! (§23, risk 5) a property of the code rather than a promise in a document.
//!
//! # Choosing one
//!
//! [`connect`] dispatches on the address, so a caller with a
//! [`TransportAddr`] from a manifest or an environment variable never has to
//! match on the plane itself.
//!
//! # Examples
//!
//! ```no_run
//! use astrs_transport::backend::connect;
//! use astrs_transport::{HandshakeParams, LocalIdentity, TransportAddr, TransportConfig};
//! use astrs_wire::{AuthToken, Role};
//!
//! # async fn example() -> Result<(), astrs_transport::TransportError> {
//! let addr: TransportAddr = "tcp:10.0.0.4:7407".parse()?;
//! let config = TransportConfig::new();
//! let params = HandshakeParams::from_config(
//!     &config,
//!     LocalIdentity::new(Role::Peer),
//!     AuthToken::from_bytes([1; 32]),
//!     addr.requires_crc(),
//! );
//! let (peer, _channels) = connect(&addr, &config, &params).await?;
//! # let _ = peer;
//! # Ok(())
//! # }
//! ```

pub mod tcp;
pub mod uds;

#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
#[cfg(feature = "quic")]
pub mod quic;

use astrs_wire::Plane;

use crate::addr::TransportAddr;
use crate::config::TransportConfig;
use crate::conn::StreamConnection;
use crate::error::{TransportError, TransportResult};
use crate::handshake::HandshakeParams;
use crate::mux::MuxChannels;

/// Dials `addr` over whichever plane it names.
///
/// A `quic:` address without the `quic` feature compiled in is dialled over
/// TCP at the same endpoint. That is not a silent downgrade: the two planes
/// carry byte-identical framing (§7.1), and the alternative — refusing to start
/// a robot because a build flag is off — is worse. The choice is logged.
///
/// # Errors
///
/// Whatever the chosen backend returns.
pub async fn connect(
    addr: &TransportAddr,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    match addr {
        TransportAddr::Uds(path) => uds::connect(path, config, params).await,
        TransportAddr::Tcp(socket) => tcp::connect(*socket, config, params).await,
        TransportAddr::Quic(socket) => {
            #[cfg(feature = "quic")]
            {
                quic::connect(*socket, config, params).await
            }
            #[cfg(not(feature = "quic"))]
            {
                tracing::info!(
                    peer = %addr,
                    "quic feature is not compiled in; dialling tcp with identical framing"
                );
                tcp::connect(*socket, config, params).await
            }
        }
    }
}

/// Dials `addr`, retrying over TCP if the QUIC leg fails (§23, risk 5).
///
/// # Errors
///
/// The TCP error, if the fallback also fails.
pub async fn connect_with_fallback(
    addr: &TransportAddr,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    match addr {
        TransportAddr::Uds(path) => uds::connect(path, config, params).await,
        _ => tcp::connect_with_fallback(addr, config, params).await,
    }
}

/// Whether this build can dial `plane` natively.
///
/// `Plane::Quic` answers `false` without the `quic` feature — the dial still
/// succeeds, over TCP, but a caller reporting capabilities to an operator
/// should say which.
///
/// # Examples
///
/// ```
/// use astrs_transport::backend::supports_natively;
/// use astrs_wire::Plane;
///
/// assert!(supports_natively(Plane::Uds));
/// assert!(supports_natively(Plane::Tcp));
/// assert_eq!(supports_natively(Plane::Quic), cfg!(feature = "quic"));
/// ```
#[must_use]
pub const fn supports_natively(plane: Plane) -> bool {
    match plane {
        Plane::Uds | Plane::Tcp => true,
        Plane::Quic => cfg!(feature = "quic"),
        // `Plane` is `#[non_exhaustive]`: a plane added later is not supported
        // until a backend exists for it, which fails closed.
        _ => false,
    }
}

/// The plane a dial to `addr` will actually use in this build.
///
/// # Examples
///
/// ```
/// use astrs_transport::backend::effective_plane;
/// use astrs_transport::TransportAddr;
/// use astrs_wire::Plane;
///
/// let quic: TransportAddr = "quic:127.0.0.1:1".parse()?;
/// let expected = if cfg!(feature = "quic") { Plane::Quic } else { Plane::Tcp };
/// assert_eq!(effective_plane(&quic), expected);
/// # Ok::<(), astrs_transport::AddressError>(())
/// ```
#[must_use]
pub fn effective_plane(addr: &TransportAddr) -> Plane {
    let requested = addr.plane();
    if supports_natively(requested) {
        requested
    } else {
        Plane::Tcp
    }
}

/// Refuses an address whose plane this build cannot reach at all.
///
/// Every plane is reachable today (QUIC degrades to TCP), so this only fires
/// for a plane from a future release. It exists so the failure is a typed
/// error at dial time rather than a confusing timeout.
///
/// # Errors
///
/// [`TransportError::Unsupported`] for an unreachable plane.
pub fn ensure_reachable(addr: &TransportAddr) -> TransportResult<()> {
    match addr.plane() {
        Plane::Uds | Plane::Tcp | Plane::Quic => Ok(()),
        _ => Err(TransportError::Unsupported {
            capability: "transport plane",
            detail: "this build has no backend for that plane",
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::LocalIdentity;
    use crate::conn::Connection;
    use crate::handshake::acceptor_from_config;
    use astrs_wire::{AuthToken, FrameKind, Role, RoleSet, SessionAssignment, SessionId};
    use std::time::Duration;

    fn token() -> AuthToken {
        AuthToken::from_bytes([0x11; 32])
    }

    #[test]
    fn the_stream_planes_are_always_native() {
        assert!(supports_natively(Plane::Uds));
        assert!(supports_natively(Plane::Tcp));
        assert_eq!(supports_natively(Plane::Quic), cfg!(feature = "quic"));
    }

    #[test]
    fn the_effective_plane_reports_the_degradation() {
        let uds = TransportAddr::uds(std::env::temp_dir().join("astrs-x.sock"));
        assert_eq!(effective_plane(&uds), Plane::Uds);

        let tcp: TransportAddr = "tcp:127.0.0.1:1".parse().unwrap();
        assert_eq!(effective_plane(&tcp), Plane::Tcp);

        let quic: TransportAddr = "quic:127.0.0.1:1".parse().unwrap();
        let expected = if cfg!(feature = "quic") {
            Plane::Quic
        } else {
            Plane::Tcp
        };
        assert_eq!(effective_plane(&quic), expected);
    }

    #[test]
    fn every_current_plane_is_reachable() {
        for addr in [
            TransportAddr::uds(std::env::temp_dir().join("astrs-x.sock")),
            "tcp:127.0.0.1:1".parse().unwrap(),
            "quic:127.0.0.1:1".parse().unwrap(),
        ] {
            ensure_reachable(&addr).unwrap();
        }
    }

    #[tokio::test]
    async fn the_dispatcher_reaches_a_tcp_listener() {
        let config = TransportConfig::new();
        let listener = tcp::TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.addr().unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let server = tokio::spawn(async move {
            listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
        });

        let params =
            HandshakeParams::from_config(&config, LocalIdentity::new(Role::Peer), token(), true);
        let (client, _cc) = connect(&addr, &config, &params).await.unwrap();
        let (_server, mut sc) = server.await.unwrap().unwrap();

        client
            .open_control()
            .send(FrameKind::PeerEvent, b"dispatched")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), sc.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"dispatched");
    }

    #[tokio::test]
    async fn the_dispatcher_reaches_a_unix_socket() {
        let path = std::env::temp_dir().join(format!("astrs-disp-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let config = TransportConfig::uds();
        let listener = uds::UdsListener::bind(&path, config.clone()).unwrap();
        let addr = listener.addr();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, false);
        let server = tokio::spawn(async move {
            let outcome = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await;
            (outcome, listener)
        });

        let params =
            HandshakeParams::from_config(&config, LocalIdentity::new(Role::Node), token(), false);
        let (client, _cc) = connect(&addr, &config, &params).await.unwrap();
        let ((_server, mut sc), _listener) = {
            let (outcome, listener) = server.await.unwrap();
            (outcome.unwrap(), listener)
        };

        client
            .open_control()
            .send(FrameKind::NodeRequest, b"dispatched")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), sc.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"dispatched");
        assert_eq!(client.peer().plane, Plane::Uds);
    }

    #[tokio::test]
    async fn a_quic_address_reaches_a_tcp_listener_when_quic_is_absent() {
        // Without the `quic` feature this is the documented degradation; with
        // it, the QUIC dial fails and `connect_with_fallback` retries on TCP.
        // Either way the connection must come up with identical framing.
        let config = TransportConfig::new();
        let listener = tcp::TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let socket = listener.local_addr().unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let server = tokio::spawn(async move {
            listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
        });

        let params =
            HandshakeParams::from_config(&config, LocalIdentity::new(Role::Peer), token(), true);
        let (client, _cc) = connect_with_fallback(&TransportAddr::Quic(socket), &config, &params)
            .await
            .unwrap();
        let (_server, _sc) = server.await.unwrap().unwrap();
        assert_eq!(client.peer().plane, Plane::Tcp);
    }

    #[tokio::test]
    async fn the_fallback_dispatcher_still_handles_unix_sockets() {
        let config = TransportConfig::uds();
        let params =
            HandshakeParams::from_config(&config, LocalIdentity::new(Role::Node), token(), false);
        let missing = TransportAddr::uds(std::env::temp_dir().join("astrs-nope.sock"));
        let err = connect_with_fallback(&missing, &config, &params)
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Io(_)));
    }
}
