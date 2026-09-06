//! The TCP backend: the fallback that works everywhere (blueprint §6.4).
//!
//! QUIC is the default for daemon↔daemon and daemon↔coordinator links, and TCP
//! is what runs when QUIC cannot: a middlebox that drops UDP, a container
//! network that only forwards TCP, a platform where the QUIC stack is not
//! available. Because the framing is *identical* — same magic, same header,
//! same checksum, same `oxicode` payloads (§7.1) — the fallback is a change of
//! socket and nothing else. [`TransportAddr::fallback_to_tcp`] makes it one
//! call.
//!
//! # `TCP_NODELAY`, always
//!
//! AstRS writes small control frames — heartbeats, route setups, acks — that
//! must not wait for Nagle's coalescing timer. Every socket this module
//! creates has `TCP_NODELAY` set, on both the dialling and the accepting side,
//! unless [`TransportConfig::tcp_nodelay`] is explicitly turned off. Batching
//! is the driver's job (it queues a burst and flushes once), not the kernel's.
//!
//! # Checksums are mandatory here
//!
//! Unlike the Unix-socket leg, a TCP connection crosses hardware that can and
//! does corrupt bytes past TCP's own 16-bit checksum. CRC32C is therefore
//! mandatory on this plane (§7.1), and [`crate::TransportAddr::requires_crc`]
//! says so for every network address.
//!
//! # Examples
//!
//! ```no_run
//! use astrs_transport::backend::tcp::{TcpListener as AstrsTcpListener, connect};
//! use astrs_transport::{HandshakeParams, LocalIdentity, TransportConfig};
//! use astrs_wire::{AuthToken, Role};
//!
//! # async fn example() -> Result<(), astrs_transport::TransportError> {
//! let config = TransportConfig::new();
//! let listener = AstrsTcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone()).await?;
//! let params = HandshakeParams::from_config(
//!     &config,
//!     LocalIdentity::new(Role::Peer),
//!     AuthToken::from_bytes([2; 32]),
//!     true,
//! );
//! let (peer, _channels) = connect(listener.local_addr()?, &config, &params).await?;
//! # let _ = peer;
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use astrs_wire::{Acceptor, SessionAssignment};
use tokio::net::{TcpSocket, TcpStream};

use crate::addr::TransportAddr;
use crate::config::TransportConfig;
use crate::conn::StreamConnection;
use crate::error::{TransportError, TransportResult};
use crate::handshake::HandshakeParams;
use crate::mux::MuxChannels;

/// Dials `addr` over TCP and completes the handshake.
///
/// # Errors
///
/// - [`TransportError::Io`] if the connect fails.
/// - [`TransportError::Timeout`] if it exceeds
///   [`TransportConfig::connect_timeout`].
/// - Anything [`crate::initiate`] can return.
pub async fn connect(
    addr: SocketAddr,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    let stream = dial(addr, config).await?;
    StreamConnection::connect(stream, TransportAddr::Tcp(addr), config, params).await
}

/// Dials `addr` and hands back the raw socket, options applied.
///
/// # Errors
///
/// As [`connect`], minus the handshake.
pub async fn dial(addr: SocketAddr, config: &TransportConfig) -> TransportResult<TcpStream> {
    let connect = TcpStream::connect(addr);
    let stream = match tokio::time::timeout(config.connect_timeout, connect).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(TransportError::Timeout {
                operation: "tcp connect",
                timeout: config.connect_timeout,
            });
        }
    };
    apply_options(&stream, config)?;
    Ok(stream)
}

/// Dials `addr` from a chosen local address.
///
/// A multi-homed robot may need to pin the interface a peer link uses — the
/// wired one for the perception bus, the wireless one for telemetry.
///
/// # Errors
///
/// As [`dial`], plus [`TransportError::Io`] if `local` cannot be bound.
pub async fn dial_from(
    addr: SocketAddr,
    local: SocketAddr,
    config: &TransportConfig,
) -> TransportResult<TcpStream> {
    let socket = match local {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.bind(local)?;
    let stream = match tokio::time::timeout(config.connect_timeout, socket.connect(addr)).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(TransportError::Timeout {
                operation: "tcp connect",
                timeout: config.connect_timeout,
            });
        }
    };
    apply_options(&stream, config)?;
    Ok(stream)
}

/// Applies the socket options every AstRS TCP connection wants.
///
/// # Errors
///
/// [`TransportError::Io`] if the option cannot be set.
pub fn apply_options(stream: &TcpStream, config: &TransportConfig) -> TransportResult<()> {
    stream.set_nodelay(config.tcp_nodelay)?;
    Ok(())
}

/// A listening TCP socket.
#[derive(Debug)]
pub struct TcpListener {
    /// The bound socket.
    listener: tokio::net::TcpListener,
    /// The policy connections accepted here run under.
    config: TransportConfig,
}

impl TcpListener {
    /// Binds `addr`.
    ///
    /// Passing port 0 asks the kernel for an ephemeral port, which
    /// [`TcpListener::local_addr`] then reports — the pattern every test in
    /// this crate uses instead of guessing a free port.
    ///
    /// # Errors
    ///
    /// [`TransportError::Io`] if the bind fails.
    pub async fn bind(addr: SocketAddr, config: TransportConfig) -> TransportResult<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(Self { listener, config })
    }

    /// Wraps an already-bound listener.
    #[must_use]
    pub const fn from_listener(listener: tokio::net::TcpListener, config: TransportConfig) -> Self {
        Self { listener, config }
    }

    /// The address this listener is bound to.
    ///
    /// # Errors
    ///
    /// [`TransportError::Io`] if the socket cannot report it.
    pub fn local_addr(&self) -> TransportResult<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// The transport address peers reach this listener at.
    ///
    /// # Errors
    ///
    /// As [`TcpListener::local_addr`].
    pub fn addr(&self) -> TransportResult<TransportAddr> {
        Ok(TransportAddr::Tcp(self.local_addr()?))
    }

    /// The policy connections accepted here run under.
    #[must_use]
    pub const fn config(&self) -> &TransportConfig {
        &self.config
    }

    /// Accepts one connection and completes its handshake.
    ///
    /// A failed handshake is one bad connection, not a reason to stop serving.
    ///
    /// # Errors
    ///
    /// - [`TransportError::Io`] if the accept fails.
    /// - Anything [`crate::accept`] can return.
    pub async fn accept(
        &self,
        acceptor: &Acceptor,
        assignment: SessionAssignment,
    ) -> TransportResult<(StreamConnection, MuxChannels)> {
        let (stream, peer) = self.listener.accept().await?;
        apply_options(&stream, &self.config)?;
        StreamConnection::accept(
            stream,
            TransportAddr::Tcp(peer),
            &self.config,
            acceptor.clone(),
            assignment,
        )
        .await
    }

    /// Accepts one connection, choosing its session once the greeting arrives.
    ///
    /// The path a coordinator uses to honour a `resume` request (§7.2, §12).
    ///
    /// # Errors
    ///
    /// As [`TcpListener::accept`].
    pub async fn accept_with<F>(
        &self,
        acceptor: &Acceptor,
        assign: F,
    ) -> TransportResult<(StreamConnection, MuxChannels)>
    where
        F: FnOnce(&astrs_wire::Hello) -> SessionAssignment,
    {
        let (stream, peer) = self.listener.accept().await?;
        apply_options(&stream, &self.config)?;
        StreamConnection::accept_with_session(
            stream,
            TransportAddr::Tcp(peer),
            &self.config,
            acceptor.clone(),
            assign,
        )
        .await
    }

    /// Accepts the raw socket without running a handshake.
    ///
    /// # Errors
    ///
    /// [`TransportError::Io`] if the accept fails.
    pub async fn accept_raw(&self) -> TransportResult<(TcpStream, SocketAddr)> {
        let (stream, peer) = self.listener.accept().await?;
        apply_options(&stream, &self.config)?;
        Ok((stream, peer))
    }
}

/// Dials `addr`, falling back from QUIC to TCP at the same endpoint.
///
/// This is the §23 risk-5 mitigation as a single call: try the QUIC address,
/// and on failure retry the identical `host:port` over TCP, where the framing
/// is byte-for-byte the same. Without the `quic` feature there is no QUIC leg
/// to try, so a `quic:` address goes straight to TCP — which is the correct
/// behaviour, not a silent downgrade: the two carry the same protocol.
///
/// # Errors
///
/// The TCP error, if the fallback also fails.
pub async fn connect_with_fallback(
    addr: &TransportAddr,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    let socket_addr = addr
        .socket_addr()
        .ok_or_else(|| TransportError::Configuration(format!("{addr} is not a network address")))?;

    #[cfg(feature = "quic")]
    if matches!(addr, TransportAddr::Quic(_)) {
        match crate::backend::quic::connect(socket_addr, config, params).await {
            Ok(established) => return Ok(established),
            Err(err) => {
                tracing::warn!(
                    peer = %addr,
                    error = %err,
                    "quic dial failed, falling back to tcp with identical framing"
                );
            }
        }
    }

    connect(socket_addr, config, params).await
}

/// How long a caller should wait before deciding a TCP dial has stalled.
///
/// Exposed so a supervisor's own deadline and the transport's agree.
#[must_use]
pub const fn dial_deadline(config: &TransportConfig) -> Duration {
    config.connect_timeout
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::{CompressionPolicy, LocalIdentity};
    use crate::conn::Connection;
    use crate::error::CloseReason;
    use crate::handshake::acceptor_from_config;
    use astrs_wire::{AuthToken, Compression, FrameKind, Role, RoleSet, SessionId};

    fn token() -> AuthToken {
        AuthToken::from_bytes([0x77; 32])
    }

    fn loopback() -> SocketAddr {
        // Port 0: the kernel picks a free one, which is the only race-free way
        // to run many of these tests at once.
        "127.0.0.1:0".parse().expect("a valid loopback address")
    }

    fn params(config: &TransportConfig, role: Role) -> HandshakeParams {
        HandshakeParams::from_config(config, LocalIdentity::new(role), token(), true)
    }

    /// Binds, dials and handshakes a pair over loopback TCP.
    async fn pair(
        config: TransportConfig,
    ) -> (
        (StreamConnection, MuxChannels),
        (StreamConnection, MuxChannels),
    ) {
        let listener = TcpListener::bind(loopback(), config.clone()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

        let server = tokio::spawn(async move {
            listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(5)))
                .await
        });
        let client = connect(addr, &config, &params(&config, Role::Peer))
            .await
            .unwrap();
        let server = server.await.unwrap().unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn a_loopback_pair_echoes_control_frames() {
        let ((client, mut cc), (server, mut sc)) = pair(TransportConfig::new()).await;

        client
            .open_control()
            .send(FrameKind::PeerEvent, b"ping")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), sc.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"ping");

        server
            .open_control()
            .send(FrameKind::PeerEvent, b"pong")
            .await
            .unwrap();
        let echoed = tokio::time::timeout(Duration::from_secs(5), cc.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(echoed.payload(), b"pong");
    }

    #[tokio::test]
    async fn a_network_leg_always_checksums() {
        let ((client, _cc), (server, _sc)) = pair(TransportConfig::new()).await;
        assert!(client.session().limits.require_crc);
        assert!(server.session().limits.require_crc);
        assert_eq!(client.peer().plane, astrs_wire::Plane::Tcp);
    }

    #[tokio::test]
    async fn nodelay_is_on_by_default() {
        let listener = TcpListener::bind(loopback(), TransportConfig::new())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { listener.accept_raw().await });

        let stream = dial(addr, &TransportConfig::new()).await.unwrap();
        assert!(stream.nodelay().unwrap());
        let (accepted, peer) = server.await.unwrap().unwrap();
        assert!(accepted.nodelay().unwrap());
        assert_eq!(peer.ip(), stream.local_addr().unwrap().ip());
    }

    #[tokio::test]
    async fn nodelay_can_be_turned_off() {
        let config = TransportConfig::new().with_tcp_nodelay(false);
        let listener = TcpListener::bind(loopback(), config.clone()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { listener.accept_raw().await });

        let stream = dial(addr, &config).await.unwrap();
        assert!(!stream.nodelay().unwrap());
        let _ = server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_refused_dial_is_a_retryable_io_error() {
        // Bind and immediately drop, so the port is almost certainly free.
        let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TransportConfig::new();
        let err = connect(addr, &config, &params(&config, Role::Peer))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(_) | TransportError::Timeout { .. }
        ));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn a_large_compressed_route_crosses_the_socket() {
        let config = TransportConfig::new().with_compression(
            CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(1_024),
        );
        let ((client, _cc), (server, mut sc)) = pair(config).await;
        assert_eq!(client.capabilities().compression, Compression::Zstd);

        let stream = client.open_route(b"lidar").unwrap();
        let mut accepted = tokio::time::timeout(Duration::from_secs(5), sc.accepts.accept())
            .await
            .unwrap()
            .expect("an inbound route");

        let payload: Vec<u8> = (0..256_000).map(|index| (index % 13) as u8).collect();
        stream
            .sender()
            .send(FrameKind::Data, &payload)
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(20), accepted.receiver_mut().recv())
            .await
            .unwrap()
            .expect("a route frame");
        assert_eq!(frame.payload(), payload.as_slice());

        let stats = client.stats();
        assert_eq!(stats.connection.frames_compressed, 1);
        assert!(stats.connection.compression_saved_bytes > 0);
        assert!(server.stats().connection.frames_received > 0);
    }

    #[tokio::test]
    async fn a_dial_from_a_chosen_local_address_works() {
        let listener = TcpListener::bind(loopback(), TransportConfig::new())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { listener.accept_raw().await });

        let stream = dial_from(addr, loopback(), &TransportConfig::new())
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), addr);
        let _ = server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_quic_address_falls_back_to_tcp_with_identical_framing() {
        let config = TransportConfig::new();
        let listener = TcpListener::bind(loopback(), config.clone()).await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let server = tokio::spawn(async move {
            listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
        });

        // Ask for QUIC at an endpoint where only TCP is listening.
        let quic = TransportAddr::Quic(tcp_addr);
        let (client, _channels) =
            connect_with_fallback(&quic, &config, &params(&config, Role::Peer))
                .await
                .unwrap();
        let (server_conn, _sc) = server.await.unwrap().unwrap();

        assert_eq!(client.peer().plane, astrs_wire::Plane::Tcp);
        assert_eq!(
            server_conn.session().session_id,
            client.session().session_id
        );
        client.close(CloseReason::local("done")).await.unwrap();
    }

    #[tokio::test]
    async fn a_uds_address_has_no_network_fallback() {
        let config = TransportConfig::new();
        let err = connect_with_fallback(
            &TransportAddr::uds(std::env::temp_dir().join("astrs-nope.sock")),
            &config,
            &params(&config, Role::Peer),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TransportError::Configuration(_)));
    }

    #[tokio::test]
    async fn a_wrapped_listener_keeps_its_config() {
        let raw = tokio::net::TcpListener::bind(loopback()).await.unwrap();
        let expected = raw.local_addr().unwrap();
        let listener = TcpListener::from_listener(raw, TransportConfig::new());
        assert_eq!(listener.local_addr().unwrap(), expected);
        assert_eq!(listener.addr().unwrap(), TransportAddr::Tcp(expected));
        assert!(listener.config().tcp_nodelay);
        assert_eq!(
            dial_deadline(listener.config()),
            TransportConfig::new().connect_timeout
        );
    }

    #[tokio::test]
    async fn a_hundred_routes_survive_a_real_socket() {
        let config = TransportConfig::new().with_mux(
            crate::config::MuxConfig::new()
                .with_initial_window_frames(8)
                .with_route_queue_depth(8),
        );
        let ((client, _cc), (_server, mut sc)) = pair(config).await;

        let mut streams = Vec::new();
        for _ in 0..100 {
            streams.push(client.open_route(b"").unwrap());
        }
        let mut accepted = Vec::new();
        for _ in 0..100 {
            accepted.push(
                tokio::time::timeout(Duration::from_secs(20), sc.accepts.accept())
                    .await
                    .unwrap()
                    .expect("an inbound route"),
            );
        }
        assert_eq!(client.open_route_count(), 100);

        let mut writers = Vec::new();
        for (index, stream) in streams.iter().enumerate() {
            let sender = stream.sender().clone();
            writers.push(tokio::spawn(async move {
                for sequence in 0..8u8 {
                    sender
                        .send(FrameKind::Data, &[index as u8, sequence])
                        .await
                        .unwrap();
                }
            }));
        }
        let mut readers = Vec::new();
        for mut stream in accepted {
            readers.push(tokio::spawn(async move {
                let mut seen = 0;
                while seen < 8 {
                    let frame =
                        tokio::time::timeout(Duration::from_secs(60), stream.receiver_mut().recv())
                            .await
                            .expect("a route must not stall")
                            .expect("a route frame");
                    assert_eq!(frame.payload().len(), 2);
                    seen += 1;
                }
            }));
        }
        for writer in writers {
            writer.await.unwrap();
        }
        for reader in readers {
            reader.await.unwrap();
        }
    }
}
