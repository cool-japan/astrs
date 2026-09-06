//! The AstRS cross-host data and control plane.
//!
//! One connection abstraction over three substrates, all speaking the
//! `astrs-wire` framing (blueprint §6.4):
//!
//! | Plane | Backend | Used for | Checksum (§7.1) |
//! |---|---|---|---|
//! | UDS | [`backend::uds`] | node ↔ daemon | optional |
//! | TCP | [`backend::tcp`] | fallback everywhere | mandatory |
//! | QUIC | `backend::quic` (feature `quic`) | daemon ↔ daemon, daemon ↔ coordinator | mandatory |
//!
//! Everything above the socket is shared: the frame codec, the greeting, the
//! route multiplexer, the compression container, the counters. A backend
//! contributes a byte stream and a [`PeerIdentity`], and nothing else. That is
//! what makes "the TCP fallback has identical framing" (§23, risk 5) a property
//! of the code rather than a promise in a document.
//!
//! # The layers
//!
//! ```text
//!   application  ──▶ Connection ─┬─▶ ControlSender      "stream 0"      §6.4
//!                                ├─▶ RouteStream ×N     one per route   §6.4
//!                                └─▶ DatagramSender     latest-only     §6.4
//!                                          │
//!                                    RouteMux           ASTRS-MUX/1     §6.4
//!                                          │
//!                          compress ──▶ FramedDuplex ──▶ astrs-wire     §7.1
//!                                          │
//!                                   UDS │ TCP │ QUIC
//! ```
//!
//! | Module | Contents |
//! |---|---|
//! | [`addr`] | [`TransportAddr`] — one text form for all three planes |
//! | [`config`] | timeouts, compression policy, mux windows, backoff |
//! | [`error`] | [`TransportError`], classified by fatal / retryable / peer-fault |
//! | [`framed`] | [`FramedDuplex`] — the `astrs-wire` codec over any byte stream |
//! | [`compress`] | the lz4/zstd container, with a decompression-bomb guard |
//! | [`handshake`] | driving `Hello` → `Welcome` / `Refused` over a socket (§7.2) |
//! | [`mux`] | `ASTRS-MUX/1`: per-route logical streams and flow control |
//! | [`conn`] | the [`Connection`] trait and [`StreamConnection`] |
//! | [`backend`] | the three substrates |
//! | [`reconnect`] | backoff, epochs, in-flight buffering (§12) |
//! | [`stats`] | per-connection and per-route counters (§13) |
//!
//! # Three things worth knowing before reading the code
//!
//! **The frame format is `astrs-wire`'s, unmodified.** This crate adds two
//! *payload* conventions on top of it — the nine-byte mux header ([`mux`]) and
//! the four-byte compression container ([`compress`]) — because
//! [`astrs_wire::FrameKind`] is a snapshot-frozen enum with no room to grow and
//! the flags byte has no spare bits. The frame's `magic`, `ver`, `flags`,
//! `kind`, `len` and `crc32c` all keep their normative meanings, and a router
//! can still dispatch on `kind` without decoding.
//!
//! **Backpressure is a chain, and every link is bounded.** A route's sender
//! blocks on a queue permit; the queue drains only when the scheduler picks the
//! route; the scheduler skips a route with no credit; credit is granted only as
//! the *peer's application* consumes frames. There is no unbounded buffer
//! anywhere on the path, and the one place a caller may choose to lose data
//! rather than wait — the reconnect buffer — says so with a typed error.
//!
//! **Failures are classified, not merely reported.** Every [`TransportError`]
//! answers whether the byte stream is still trustworthy
//! ([`TransportError::is_fatal`]), whether redialling could help
//! ([`TransportError::is_retryable`]), and whose fault it was
//! ([`TransportError::is_peer_fault`]). The reconnect supervisor and the
//! daemon's metrics read those answers rather than matching on variants.
//!
//! # QUIC status
//!
//! The `quic` feature is **off by default**, and the reason is a dependency
//! gap rather than a design one. `oxiquic` 0.2.1 can dial (`connect`,
//! `connect_with_alpn`, `connect_0rtt`) but its *server* constructors —
//! `listen`, `ServerEndpoint::bind` — take `rustls` types (`CertificateDer`,
//! `PrivateKeyDer`, `Arc<ServerConfig>`) that the facade does not re-export,
//! and `rustls` is not in this workspace's dependency set. A QUIC listener
//! therefore cannot be constructed from inside this crate.
//!
//! Everything downstream of the endpoint is implemented and shares its code
//! path with UDS and TCP; `backend::quic::serve_connection` is the seam a host
//! with `rustls` of its own passes an accepted connection through. Until then a
//! `quic:` address is dialled over TCP with byte-identical framing — the
//! documented §23 risk-5 degradation — and [`backend::effective_plane`] reports
//! which plane a dial will really use. The full findings, including the PSK and
//! datagram gaps and their degradations, are in the `backend::quic` module
//! documentation.
//!
//! # Examples
//!
//! ```no_run
//! use astrs_transport::{
//!     Connection, HandshakeParams, LocalIdentity, TransportAddr, TransportConfig,
//! };
//! use astrs_wire::{AuthToken, FrameKind, Role};
//!
//! # async fn example() -> Result<(), astrs_transport::TransportError> {
//! let addr: TransportAddr = "tcp:10.0.0.4:7407".parse()?;
//! let config = TransportConfig::new();
//! let params = HandshakeParams::from_config(
//!     &config,
//!     LocalIdentity::new(Role::Peer).with_label("daemon-a"),
//!     AuthToken::from_bytes([7; 32]),
//!     addr.requires_crc(),
//! );
//!
//! let (peer, mut channels) = astrs_transport::backend::connect(&addr, &config, &params).await?;
//!
//! // Control messages ride "stream 0"…
//! peer.open_control().send(FrameKind::PeerEvent, b"hello").await?;
//!
//! // …and each high-bandwidth route gets a stream of its own.
//! let camera = peer.open_route(b"camera->detector")?;
//! camera.sender().send(FrameKind::Data, b"an arrow batch").await?;
//!
//! while let Some(frame) = channels.control.recv().await {
//!     println!("{:?}: {} B", frame.kind(), frame.payload().len());
//! }
//! # Ok(())
//! # }
//! ```

pub mod addr;
pub mod backend;
pub mod compress;
pub mod config;
pub mod conn;
pub mod error;
pub mod framed;
pub mod handshake;
pub mod mux;
pub mod reconnect;
pub mod stats;

pub use addr::{
    DEFAULT_COORDINATOR_PORT, DEFAULT_DAEMON_PORT, QUIC_SCHEME, TCP_SCHEME, TransportAddr,
    UDS_SCHEME,
};
pub use compress::{
    CONTAINER_HEADER_LEN, MAX_CONTAINER_PAYLOAD_BYTES, compress_payload, compress_payload_into,
    declared_len, decompress_payload, is_supported_codec,
};
pub use config::{
    BackoffConfig, CompressionPolicy, DEFAULT_BACKOFF_BASE, DEFAULT_BACKOFF_CAP,
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_CONTROL_BURST, DEFAULT_CONTROL_QUEUE_DEPTH,
    DEFAULT_DATAGRAM_QUEUE_DEPTH, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_RECONNECT_BUFFER_FRAMES,
    DEFAULT_ROUTE_QUEUE_DEPTH, DEFAULT_ROUTE_WINDOW_FRAMES, DEFAULT_ZSTD_LEVEL, LocalIdentity,
    MAX_ZSTD_LEVEL, MIN_ZSTD_LEVEL, MuxConfig, PRE_HANDSHAKE_MAX_PAYLOAD_BYTES, Side,
    TransportConfig,
};
pub use conn::{
    BoxFuture, Capabilities, Connection, EstablishedConnection, KeepAlive, PeerCredentials,
    PeerIdentity, StreamConnection,
};
pub use error::{AddressError, CloseReason, MuxHeaderError, TransportError, TransportResult};
pub use framed::{FrameSink, FrameSource, FramedDuplex, FramedReader, FramedStream, FramedWriter};
pub use handshake::{
    AcceptedHandshake, HandshakeParams, InitiatedHandshake, accept, accept_with,
    acceptor_from_config, default_accepted_roles, initiate, pre_handshake_limits,
};
pub use mux::{
    ControlReceiver, ControlSender, DatagramReceiver, DatagramSender, MUX_HEADER_LEN, MUX_PROTOCOL,
    MuxChannels, MuxHeader, MuxTag, RouteAcceptor, RouteMux, RouteReceiver, RouteSender,
    RouteStream,
};
pub use reconnect::{
    AcceptedRoute, Backoff, ConnectionEvent, ConnectionFactory, EVENT_CHANNEL_CAPACITY, LinkState,
    OverflowPolicy, ReconnectBuffer, ReconnectChannels, ReconnectingConnection,
};
pub use stats::{
    ConnectionCounters, ConnectionStatsSnapshot, PathStats, RouteCounters, RouteStatsSnapshot,
    TransportSnapshot,
};
