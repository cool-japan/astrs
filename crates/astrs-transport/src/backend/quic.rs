//! The QUIC backend over `oxiquic` (blueprint §6.4) — **feature `quic`**.
//!
//! # What `oxiquic` 0.2.1 actually provides
//!
//! This module was written against the crate's real API surface, not the
//! blueprint's assumption about it. The findings, because they decide the
//! design:
//!
//! | Capability | Status in `oxiquic` 0.2.1 |
//! |---|---|
//! | Client endpoint | **Yes** — `connect`, `connect_with_alpn`, `connect_0rtt` |
//! | Server endpoint | **Present, but not callable from here** — see below |
//! | Bidirectional streams | **Yes** — `DrivenConnection::open_bidi_stream` returns `AsyncWrite`/`AsyncRead` handles |
//! | Unidirectional streams | **Yes** — `open_uni_stream` / `accept_uni_stream` |
//! | Datagrams (RFC 9221) | **Yes on `QuicConnection`**, but not on `DrivenConnection`; and off by default (`TransportConfig::max_datagram_frame_size` defaults to 0) |
//! | ALPN | **Yes** — `connect_with_alpn`, `listen_with_alpn`, `negotiated_alpn` |
//! | PSK / pre-shared keys | **No** — the TLS layer is certificate-based `rustls` |
//!
//! ## The server-endpoint gap — an *argument-type* gap, not a missing feature
//!
//! `oxiquic` 0.2.1 **can** accept connections. The facade exports
//! `oxiquic::listen` and `oxiquic::listen_with_alpn`, and the endpoint they
//! return has a working `ServerEndpoint::accept() -> QuicConnection` (it
//! spawns a demux task over the shared UDP socket on first call) alongside
//! `ServerEndpoint::bind` and `local_addr`. Nothing about the server half is
//! unimplemented upstream.
//!
//! What is missing is a way to *name the arguments*. Every server constructor
//! spells its parameters in `rustls`:
//!
//! | Constructor | Parameters this crate cannot name |
//! |---|---|
//! | `oxiquic::listen` | `Vec<rustls::pki_types::CertificateDer<'static>>`, `rustls::pki_types::PrivateKeyDer<'static>` |
//! | `oxiquic::listen_with_alpn` | the same, plus `protocols` |
//! | `oxiquic_transport::ServerEndpoint::bind` | `Arc<rustls::ServerConfig>` |
//!
//! …and `oxiquic` re-exports none of `rustls`, `rustls::pki_types`, or a
//! certificate generator. (Its own tests build one with
//! `oxitls_rcgen::generate_self_signed_ed25519` — a **dev**-dependency of
//! `oxiquic`, so it is not in a downstream crate's graph at all.) Naming them
//! here would mean adding `rustls` and `oxitls-rcgen` to this crate's
//! `Cargo.toml`, and blueprint §18.1's retained-crate list is closed.
//!
//! `rustls` 0.23 is already *in* `Cargo.lock`, arriving through `oxiquic`
//! itself — so the gap is genuinely about what may be *named*, not about what
//! is compiled.
//!
//! ### What would close it
//!
//! One constructor whose signature mentions only `oxiquic`'s own types:
//!
//! ```text
//! pub async fn listen_self_signed(
//!     addr: std::net::SocketAddr,
//!     server_name: &str,
//!     protocols: &[&[u8]],
//! ) -> Result<ServerEndpoint, OxiQuicError>;
//! ```
//!
//! …which is exactly what `oxiquic`'s own test setup already does, moved from
//! `#[cfg(test)]` into the crate and given `oxitls-rcgen` as a real
//! dependency. A cluster's channel identity comes from its own `Hello` token
//! either way (see the PSK note below), so an ephemeral self-signed
//! certificate is the *right* default here rather than a compromise.
//!
//! A useful second best, for a host that has its own certificate bytes:
//! re-export the argument types, e.g. `pub mod pki { pub use
//! rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}; }`.
//!
//! Everything after the endpoint — the stream handles, the framing, the
//! handshake, the mux — is implemented here and is exercised by the same code
//! paths the UDS and TCP backends use. [`serve_connection`] is the seam: a
//! host that *can* build a `ServerEndpoint` (because it has `rustls` in its
//! own dependency set) passes the accepted `QuicConnection` in and gets a
//! fully-driven AstRS connection back.
//!
//! ## The PSK gap, and the §23 risk-5 degradation
//!
//! The blueprint's §6.4 asks for "oxiquic's built-in handshake with cluster PSK
//! derived from the auth token". TLS 1.3 external PSKs are not exposed by this
//! stack. The documented degradation is already in place and needs no new code:
//! **the QUIC handshake authenticates the channel, and the AstRS `Hello`
//! authenticates the cluster.** `astrs-wire`'s `Acceptor` compares the 32-byte
//! cluster token in constant time and answers a mismatch with
//! `RefusalReason::BadAuth` before any route can open (§7.2, §16). A
//! self-signed or ephemeral certificate therefore costs channel *identity*, not
//! cluster *authorisation* — which is exactly the trade the risk register
//! anticipated.
//!
//! ## The datagram gap
//!
//! Native datagrams live on `QuicConnection`, which must be consumed by
//! `into_driven()` to get concurrent stream handles. There is no way to have
//! both. This backend chooses concurrent streams — a robot's data plane needs
//! them far more than it needs unreliability — and reports
//! `native_datagrams: false`, so the mux's transparent emulation carries the
//! datagram channel (§23, risk 5). The emulation preserves what a datagram
//! user depends on (no head-of-line blocking behind route data, a bounded
//! queue that drops rather than grows) and merely makes delivery *more*
//! reliable than promised, which is safe.
//!
//! # A note on unnameable types
//!
//! `oxiquic` 0.2.1 re-exports `QuicConnection` and the endpoints, but **not**
//! `DrivenConnection`, `SendStreamHandle` or `RecvStreamHandle` — they live in
//! `oxiquic-transport`, which is not a workspace dependency. This module
//! therefore never writes those type names down: the values flow through
//! inference into [`FramedDuplex::from_halves`] and
//! [`StreamConnection::connect_duplex`], both of which are generic over any
//! `AsyncRead`/`AsyncWrite` pair. The code is identical to what it would be
//! with the names available; only the signatures are inferred rather than
//! spelled out.
//!
//! # What runs here
//!
//! One QUIC bidirectional stream carries the whole `ASTRS-MUX/1` protocol
//! ([`crate::mux`]), exactly as it does over TCP. Mapping each route onto its
//! own native QUIC stream is a drop-in replacement for the driver once a
//! server endpoint can be built in-process and the mapping can be tested end
//! to end; shipping an untestable second code path would be worse than shipping
//! one that every UDS and TCP test already covers.

use std::net::SocketAddr;
use std::sync::Arc;

use astrs_wire::{Acceptor, SessionAssignment};
use oxiquic::{QuicConnection, TransportConfig as QuicTransportConfig};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::addr::TransportAddr;
use crate::config::TransportConfig;
use crate::conn::{Capabilities, Connection, KeepAlive, StreamConnection};
use crate::error::{TransportError, TransportResult};
use crate::framed::FramedDuplex;
use crate::handshake::HandshakeParams;
use crate::mux::MuxChannels;
use crate::stats::ConnectionCounters;

/// The ALPN identifier AstRS advertises on QUIC.
///
/// Versioned with the frame layout, not the crate: a peer speaking
/// `astrs/1` speaks the frame format of §7.1, whatever release it is from.
pub const ASTRS_ALPN: &[u8] = b"astrs/1";

/// The server name a dial presents when the caller does not choose one.
///
/// A cluster peer is usually named by address, not by hostname, and the
/// certificate that matches is issued for the cluster rather than for a DNS
/// entry.
pub const DEFAULT_SERVER_NAME: &str = "astrs";

/// Dials `addr` over QUIC and completes the AstRS handshake.
///
/// Uses the system WebPKI trust store, so the peer must present a certificate
/// chaining to a public CA. A cluster with its own CA should use
/// [`connect_with_server_name`] with the name the certificate was issued for.
///
/// # Errors
///
/// - [`TransportError::Quic`] if the QUIC handshake fails.
/// - [`TransportError::Timeout`] if the dial exceeds
///   [`TransportConfig::connect_timeout`].
/// - Anything [`crate::initiate`] can return.
pub async fn connect(
    addr: SocketAddr,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    connect_with_server_name(addr, DEFAULT_SERVER_NAME, config, params).await
}

/// Dials `addr`, presenting `server_name` for certificate validation.
///
/// # Errors
///
/// As [`connect`].
pub async fn connect_with_server_name(
    addr: SocketAddr,
    server_name: &str,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    let dial = oxiquic::connect_with_alpn(addr, server_name, &[ASTRS_ALPN]);
    let connection = match tokio::time::timeout(config.connect_timeout, dial).await {
        Ok(result) => result.map_err(TransportError::quic)?,
        Err(_) => {
            return Err(TransportError::Timeout {
                operation: "quic connect",
                timeout: config.connect_timeout,
            });
        }
    };
    open_connection(connection, addr, config, params).await
}

/// Drives an already-established client-side [`QuicConnection`].
///
/// The seam for a host that built its own [`oxiquic::ClientEndpoint`] — with a
/// cluster CA, a client certificate, or 0-RTT — and wants AstRS to take it from
/// there.
///
/// # Errors
///
/// As [`connect`].
pub async fn open_connection(
    connection: QuicConnection,
    addr: SocketAddr,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    let capabilities_hint = observe(&connection);
    let driven = connection.into_driven();
    let (send, recv) = driven
        .open_bidi_stream()
        .await
        .map_err(TransportError::quic)?;

    let counters = ConnectionCounters::shared();
    let framed = frame_halves(recv, send, config, Arc::clone(&counters));
    let (connection, channels) = StreamConnection::connect_duplex(
        framed,
        TransportAddr::Quic(addr),
        config,
        params,
        counters,
    )
    .await?;
    // The driver tasks own the stream handles, but `oxiquic` aborts its
    // background I/O task when the last `DrivenConnection` clone drops — which
    // would kill a connection whose handles are still perfectly alive. Park it
    // with the connection so the two die together.
    let connection = connection.with_keepalive(KeepAlive::new(driven));
    Ok((apply_capabilities(connection, capabilities_hint), channels))
}

/// Runs the acceptor's half over a `QuicConnection` the host accepted.
///
/// This is the server seam described in the module documentation. A host that
/// can construct an [`oxiquic::ServerEndpoint`] — because it has `rustls` in
/// its own dependency set — accepts a connection and hands it here:
///
/// ```ignore
/// let server = oxiquic::listen_with_alpn(addr, cert_chain, key, &[ASTRS_ALPN]).await?;
/// loop {
///     let connection = server.accept().await?;
///     let (peer, channels) = serve_connection(
///         connection,
///         peer_addr,
///         &config,
///         acceptor.clone(),
///         SessionAssignment::Fresh(next_session_id()),
///     )
///     .await?;
/// }
/// ```
///
/// # Errors
///
/// - [`TransportError::Quic`] if the peer never opens its control stream.
/// - Anything [`crate::accept`] can return.
pub async fn serve_connection(
    connection: QuicConnection,
    addr: SocketAddr,
    config: &TransportConfig,
    acceptor: Acceptor,
    assignment: SessionAssignment,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    let capabilities_hint = observe(&connection);
    let driven = connection.into_driven();
    let (send, recv) = driven
        .accept_bidi_stream()
        .await
        .map_err(TransportError::quic)?;

    let counters = ConnectionCounters::shared();
    let framed = frame_halves(recv, send, config, Arc::clone(&counters));
    let (connection, channels) = StreamConnection::accept_duplex(
        framed,
        TransportAddr::Quic(addr),
        config,
        acceptor,
        assignment,
        counters,
    )
    .await?;
    let connection = connection.with_keepalive(KeepAlive::new(driven));
    Ok((apply_capabilities(connection, capabilities_hint), channels))
}

/// What a QUIC connection reported about itself before it was driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct QuicObservations {
    /// The datagram ceiling the peer advertised, if it enabled datagrams.
    pub max_datagram_bytes: Option<usize>,
    /// Whether the peer negotiated the AstRS ALPN.
    pub alpn_matched: bool,
}

/// Reads what a connection can tell us before `into_driven` consumes it.
///
/// Everything here is unavailable afterwards, which is why it is captured in
/// one place rather than asked for later.
#[must_use]
pub fn observe(connection: &QuicConnection) -> QuicObservations {
    QuicObservations {
        max_datagram_bytes: connection.max_datagram_size(),
        alpn_matched: connection
            .negotiated_alpn()
            .is_some_and(|alpn| alpn == ASTRS_ALPN),
    }
}

/// The transport parameters an AstRS QUIC endpoint should be built with.
///
/// Offered for a host that constructs its own endpoint: it keeps the datagram
/// frame size non-zero (it defaults to zero, which disables RFC 9221 outright)
/// so that a future build can use native datagrams without a second
/// negotiation.
///
/// # Examples
///
/// ```ignore
/// let endpoint = ServerEndpoint::bind(addr, server_config, quic_transport_config()).await?;
/// ```
#[must_use]
pub fn quic_transport_config(config: &TransportConfig) -> QuicTransportConfig {
    let datagram_ceiling = config.limits.max_payload_bytes.min(u64::from(u16::MAX));
    QuicTransportConfig::default().max_datagram_frame_size(datagram_ceiling)
}

/// Wraps a QUIC stream pair in the AstRS frame codec.
fn frame_halves<R, W>(
    recv: R,
    send: W,
    config: &TransportConfig,
    counters: Arc<ConnectionCounters>,
) -> FramedDuplex<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // QUIC is a network leg: the checksum is mandatory (§7.1), whatever the
    // caller's policy says, unless it explicitly overrode it.
    let limits = config.proposed_limits(true).to_frame_limits();
    FramedDuplex::from_halves(recv, send, limits, config.read_buffer_bytes, counters)
}

/// Applies what the QUIC layer told us to the connection's reported
/// capabilities.
///
/// Native streams are reported as *unavailable* even though `oxiquic` has
/// them: this backend runs the whole mux over one bidirectional stream, so a
/// caller optimising for "each route is independently ordered" would be
/// optimising for something that is not true today. Reporting a capability the
/// implementation does not use would be a lie that costs a debugging session.
fn apply_capabilities(
    connection: StreamConnection,
    observations: QuicObservations,
) -> StreamConnection {
    let base = connection.capabilities();
    let _ = observations.max_datagram_bytes;
    connection.with_capabilities(
        Capabilities::stream(base.compression, base.max_payload_bytes, base.max_routes)
            .with_route_mux(base.route_mux)
            .with_native_streams(false),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_alpn_is_the_frame_layout_version() {
        assert_eq!(ASTRS_ALPN, b"astrs/1");
        assert_eq!(
            u32::from(astrs_wire::FRAME_VERSION),
            1,
            "the ALPN version must track the frame layout version"
        );
    }

    #[test]
    fn the_transport_config_enables_datagram_frames() {
        let config = quic_transport_config(&TransportConfig::new());
        assert!(
            config.get_max_datagram_frame_size() > 0,
            "oxiquic defaults datagrams off; an AstRS endpoint must enable them"
        );
        assert!(config.get_max_datagram_frame_size() <= u64::from(u16::MAX));
    }

    #[test]
    fn observations_default_to_nothing_negotiated() {
        let observations = QuicObservations::default();
        assert_eq!(observations.max_datagram_bytes, None);
        assert!(!observations.alpn_matched);
    }

    #[tokio::test]
    async fn a_dial_to_a_dead_port_fails_rather_than_hanging() {
        // Nothing is listening on this UDP port, so the QUIC handshake must
        // time out or fail — never block forever.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let config =
            TransportConfig::new().with_connect_timeout(std::time::Duration::from_millis(250));
        let params = HandshakeParams::from_config(
            &config,
            crate::config::LocalIdentity::new(astrs_wire::Role::Peer),
            astrs_wire::AuthToken::ZERO,
            true,
        );
        let err = connect(addr, &config, &params).await.unwrap_err();
        assert!(
            matches!(
                err,
                TransportError::Quic(_) | TransportError::Timeout { .. }
            ),
            "unexpected error: {err:?}"
        );
        assert!(err.is_retryable());
    }
}
