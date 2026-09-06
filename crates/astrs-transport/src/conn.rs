//! The connection abstraction every backend implements.
//!
//! One trait, three substrates. [`Connection`] is what the daemon, the
//! coordinator and the node API program against; whether the bytes underneath
//! travel over a Unix socket, a TCP socket or a QUIC stream changes the
//! [`PeerIdentity`] and the [`Capabilities`], and nothing else.
//!
//! # What a connection offers
//!
//! | Method | Blueprint |
//! |---|---|
//! | [`Connection::open_control`] | stream 0: control messages (§6.4) |
//! | [`Connection::open_route_stream`] | one logical stream per route (§6.4) |
//! | [`RouteAcceptor`](crate::RouteAcceptor) | the peer's side of the same |
//! | [`Connection::datagrams`] | latest-only topics (§6.4) |
//! | [`Connection::peer`] | who is on the other end (§16) |
//! | [`Connection::close`] | shutdown with a stated reason |
//! | [`Connection::stats`] | bandwidth accounting (§13) |
//!
//! # Dyn-safety, deliberately
//!
//! A daemon holds a heterogeneous set of peers — one UDS connection per node,
//! one QUIC connection per daemon, a TCP fallback for the one machine behind a
//! hostile middlebox — in a single table. That requires `Box<dyn Connection>`,
//! which in turn requires every method to be object-safe. Only
//! [`Connection::close`] genuinely needs to be async, so it is the only one
//! that returns a boxed future; everything else is a synchronous call into the
//! mux, which is where the work already happens. The result is one allocation
//! per *close*, and none per send.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::{Connection, ConnectionCounters, StreamConnection, TransportConfig};
//! use astrs_transport::{LocalIdentity, Side, TransportAddr};
//! use astrs_wire::{AuthToken, FrameKind, Role};
//!
//! # fn main() {
//! # tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
//! let token = AuthToken::from_bytes([4; 32]);
//! let (client_io, server_io) = tokio::io::duplex(1 << 20);
//! let config = TransportConfig::uds();
//!
//! let server = tokio::spawn({
//!     let config = config.clone();
//!     let token = token.clone();
//!     async move {
//!         StreamConnection::accept(
//!             server_io,
//!             TransportAddr::uds(std::env::temp_dir().join("astrs-example.sock")),
//!             &config,
//!             astrs_transport::acceptor_from_config(
//!                 &config,
//!                 token,
//!                 astrs_wire::RoleSet::PEERS,
//!                 false,
//!             ),
//!             astrs_wire::SessionAssignment::Fresh(astrs_wire::SessionId::from_u128(1)),
//!         )
//!         .await
//!     }
//! });
//!
//! let (client, _channels) = StreamConnection::connect(
//!     client_io,
//!     TransportAddr::uds(std::env::temp_dir().join("astrs-example.sock")),
//!     &config,
//!     &astrs_transport::HandshakeParams::from_config(
//!         &config,
//!         LocalIdentity::new(Role::Peer),
//!         token,
//!         false,
//!     ),
//! )
//! .await
//! .expect("connect");
//! let (_server, mut server_channels) = server.await.unwrap().expect("accept");
//!
//! client.open_control().send(FrameKind::PeerEvent, b"hi").await.unwrap();
//! let frame = server_channels.control.recv().await.expect("a frame");
//! assert_eq!(frame.payload(), b"hi");
//! assert_eq!(client.side(), Side::Initiator);
//! # });
//! # }
//! ```

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use astrs_wire::{
    Acceptor, AstrsVersion, Compression, NegotiatedSession, Plane, Role, RouteId, SessionAssignment,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;

use crate::addr::TransportAddr;
use crate::config::{Side, TransportConfig};
use crate::error::{CloseReason, TransportResult};
use crate::framed::{FramedDuplex, FramedStream};
use crate::handshake::{HandshakeParams, accept, initiate};
use crate::mux::{
    ControlSender, DatagramSender, MuxChannels, RouteMux, RouteStream, driver::run_reader,
    driver::run_writer,
};
use crate::stats::{ConnectionCounters, TransportSnapshot};

/// A boxed future, for the one method that must be async and dyn-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A backend-owned value kept alive for exactly as long as the connection is.
///
/// Some substrates hand out I/O handles that are only valid while a *third*
/// value lives. `oxiquic`'s `DrivenConnection` is the motivating case: it owns
/// the background task that pumps the QUIC connection, and dropping the last
/// clone aborts that task — silently killing a connection whose stream handles
/// are still in use. The backend parks it here, and it is dropped when the
/// connection is.
pub struct KeepAlive {
    /// The parked value. Never read: it exists to be *dropped* at the right
    /// moment, which is the whole point.
    _guard: Box<dyn std::any::Any + Send + Sync>,
}

impl KeepAlive {
    /// Parks `value` for the connection's lifetime.
    #[must_use]
    pub fn new(value: impl std::any::Any + Send + Sync) -> Self {
        Self {
            _guard: Box::new(value),
        }
    }
}

impl fmt::Debug for KeepAlive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeepAlive(..)")
    }
}

/// The credentials the kernel vouches for on a Unix socket.
///
/// Blueprint §16 requires the daemon to check that a node connecting over UDS
/// really is the process it spawned. These are the values that check runs
/// against, and they cannot be forged by the peer: the kernel fills them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PeerCredentials {
    /// The peer's effective user id.
    pub uid: u32,
    /// The peer's effective group id.
    pub gid: u32,
    /// The peer's process id, where the platform reports it.
    pub pid: Option<i32>,
}

impl PeerCredentials {
    /// Credentials for `uid`/`gid` with no process id.
    #[must_use]
    pub const fn new(uid: u32, gid: u32) -> Self {
        Self {
            uid,
            gid,
            pid: None,
        }
    }

    /// Attaches a process id.
    #[must_use]
    pub const fn with_pid(mut self, pid: Option<i32>) -> Self {
        self.pid = pid;
        self
    }

    /// Whether the peer runs as `uid`.
    ///
    /// The cheap half of the §16 check: a daemon compares this against its own
    /// effective uid and refuses a node that is not even the same user, before
    /// anything more expensive runs. The uid to compare against is the
    /// caller's to supply — reading it needs a syscall, and this crate does not
    /// take a syscall dependency to answer a question its callers
    /// (`astrs-daemon`, which already has `rustix`) can answer for themselves.
    #[must_use]
    pub const fn runs_as(&self, uid: u32) -> bool {
        self.uid == uid
    }

    /// Whether the peer runs as one of `uids`.
    #[must_use]
    pub fn runs_as_any(&self, uids: &[u32]) -> bool {
        uids.contains(&self.uid)
    }
}

/// Who is on the other end of a connection.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PeerIdentity {
    /// The role this connection was opened as (§7.2).
    ///
    /// This is the *initiating* end's role, and both ends see the same value:
    /// `Hello` carries it and `Welcome` echoes it, so "this is a node
    /// connection" is a fact about the link rather than about one side of it.
    /// A daemon's peer table keys on it to know which message family will
    /// arrive.
    pub role: Role,
    /// The label the peer attached, for logs.
    pub label: Option<String>,
    /// The address this connection reaches the peer at.
    pub addr: TransportAddr,
    /// The plane the bytes actually travel over.
    pub plane: Plane,
    /// The peer's AstRS release.
    pub version: AstrsVersion,
    /// The kernel-vouched credentials, on a Unix socket.
    pub credentials: Option<PeerCredentials>,
}

impl PeerIdentity {
    /// An identity for a peer at `addr` over a connection opened as `role`.
    #[must_use]
    pub fn new(role: Role, addr: TransportAddr) -> Self {
        let plane = addr.plane();
        Self {
            role,
            label: None,
            addr,
            plane,
            version: AstrsVersion::current(),
            credentials: None,
        }
    }

    /// Attaches the peer's self-declared label.
    #[must_use]
    pub fn with_label(mut self, label: Option<String>) -> Self {
        self.label = label;
        self
    }

    /// Attaches the peer's AstRS release.
    #[must_use]
    pub fn with_version(mut self, version: AstrsVersion) -> Self {
        self.version = version;
        self
    }

    /// Attaches kernel-vouched credentials.
    #[must_use]
    pub const fn with_credentials(mut self, credentials: Option<PeerCredentials>) -> Self {
        self.credentials = credentials;
        self
    }

    /// A short description for logs: `daemon-b (peer) at quic:10.0.0.4:7407`.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.label {
            Some(label) => format!("{label} ({}) at {}", self.role, self.addr),
            None => format!("{} at {}", self.role, self.addr),
        }
    }
}

impl fmt::Display for PeerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// What a connection can actually do, after negotiation.
///
/// A caller consults this rather than the plane: a QUIC connection whose peer
/// advertised `max_datagram_frame_size = 0` has no native datagrams, and a
/// caller that branched on "is this QUIC?" would get it wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// Whether the backend has real unreliable datagrams underneath.
    ///
    /// `false` does **not** mean [`Connection::datagrams`] is unusable: the mux
    /// emulates datagrams over the shared stream (§23, risk 5). It means a
    /// caller optimising for "this will not be retransmitted" should not.
    pub native_datagrams: bool,
    /// Whether the backend has real per-route streams underneath.
    pub native_streams: bool,
    /// Whether the route mux is running on this connection.
    pub route_mux: bool,
    /// The codec both ends negotiated.
    pub compression: Compression,
    /// The negotiated payload ceiling, in bytes.
    pub max_payload_bytes: usize,
    /// The negotiated route ceiling.
    pub max_routes: u32,
    /// The largest datagram this path carries, where the backend knows.
    pub max_datagram_bytes: Option<usize>,
}

impl Capabilities {
    /// Capabilities for a stream backend with no native datagrams or streams.
    #[must_use]
    pub const fn stream(
        compression: Compression,
        max_payload_bytes: usize,
        max_routes: u32,
    ) -> Self {
        Self {
            native_datagrams: false,
            native_streams: false,
            route_mux: true,
            compression,
            max_payload_bytes,
            max_routes,
            max_datagram_bytes: None,
        }
    }

    /// Declares that the backend has native unreliable datagrams.
    #[must_use]
    pub const fn with_native_datagrams(mut self, max_bytes: Option<usize>) -> Self {
        self.native_datagrams = true;
        self.max_datagram_bytes = max_bytes;
        self
    }

    /// Declares that the backend has native per-route streams.
    #[must_use]
    pub const fn with_native_streams(mut self, native: bool) -> Self {
        self.native_streams = native;
        self
    }

    /// Declares whether the route mux is running.
    #[must_use]
    pub const fn with_route_mux(mut self, enabled: bool) -> Self {
        self.route_mux = enabled;
        self
    }
}

/// One connection to one peer.
///
/// See the module documentation for the shape of this trait and why only
/// [`Connection::close`] is async.
pub trait Connection: Send + Sync + fmt::Debug {
    /// Who is on the other end.
    fn peer(&self) -> &PeerIdentity;

    /// What the handshake agreed to (§7.2).
    fn session(&self) -> &NegotiatedSession;

    /// What this connection can do, after negotiation.
    fn capabilities(&self) -> Capabilities;

    /// Which side of the connection this end is.
    fn side(&self) -> Side;

    /// The connection's incarnation counter.
    ///
    /// Zero for a connection that has never been re-established;
    /// [`crate::ReconnectingConnection`] increments it on every successful
    /// redial, so a consumer that saw epoch `n` knows anything it queued
    /// before belongs to an older link.
    fn epoch(&self) -> u64;

    /// Whether the connection has ended.
    fn is_closed(&self) -> bool;

    /// Why it ended, if it has.
    fn close_reason(&self) -> Option<CloseReason>;

    /// The control plane — stream 0 (§6.4).
    fn open_control(&self) -> ControlSender;

    /// The datagram channel, native or emulated (§6.4, §23 risk 5).
    fn datagrams(&self) -> DatagramSender;

    /// Opens a route with a freshly minted handle.
    ///
    /// # Errors
    ///
    /// As [`Connection::open_route_stream`].
    fn open_route(&self, descriptor: &[u8]) -> TransportResult<RouteStream>;

    /// Opens a route with a caller-chosen handle.
    ///
    /// # Errors
    ///
    /// - [`crate::TransportError::Closed`] if the connection has ended.
    /// - [`crate::TransportError::RouteLimitReached`] at the negotiated ceiling.
    /// - [`crate::TransportError::DuplicateRoute`] if the handle is in use.
    fn open_route_stream(&self, route: RouteId, descriptor: &[u8]) -> TransportResult<RouteStream>;

    /// How many routes are open right now.
    fn open_route_count(&self) -> usize;

    /// Connection and per-route counters (§13).
    fn stats(&self) -> TransportSnapshot;

    /// Closes the connection, telling the peer why.
    fn close(&self, reason: CloseReason) -> BoxFuture<'_, TransportResult<()>>;
}

/// A connection over any framed byte stream: UDS, TCP, or a QUIC stream pair.
///
/// This is the only [`Connection`] implementation the crate needs. A backend's
/// job is to produce the byte stream and the [`PeerIdentity`]; everything after
/// that — the handshake, the mux, the driver tasks — is the same code.
#[derive(Debug)]
pub struct StreamConnection {
    /// Who is on the other end.
    peer: PeerIdentity,
    /// What the handshake agreed to.
    session: NegotiatedSession,
    /// The route mux.
    mux: RouteMux,
    /// What this connection can do.
    capabilities: Capabilities,
    /// Which side this end is.
    side: Side,
    /// The connection's incarnation.
    epoch: u64,
    /// The writer and reader tasks, aborted when this handle drops.
    tasks: Vec<JoinHandle<()>>,
    /// The shared counters.
    counters: Arc<ConnectionCounters>,
    /// A backend-owned value that must outlive the stream handles.
    keepalive: Option<KeepAlive>,
}

impl StreamConnection {
    /// Dials: runs the initiator's handshake, then starts the mux.
    ///
    /// # Errors
    ///
    /// Anything [`initiate`] can return.
    pub async fn connect<S>(
        stream: S,
        addr: TransportAddr,
        config: &TransportConfig,
        params: &HandshakeParams,
    ) -> TransportResult<(Self, MuxChannels)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let counters = ConnectionCounters::shared();
        let limits = config
            .proposed_limits(addr.requires_crc())
            .to_frame_limits();
        let framed = FramedStream::with_capacity(
            stream,
            limits,
            config.read_buffer_bytes,
            Arc::clone(&counters),
        );
        Self::connect_duplex(framed, addr, config, params, counters).await
    }

    /// Dials over two already-separate halves.
    ///
    /// The QUIC backend uses this: a bidirectional stream arrives as a send
    /// handle and a receive handle that were never one value.
    ///
    /// # Errors
    ///
    /// Anything [`initiate`] can return.
    pub async fn connect_duplex<R, W>(
        mut framed: FramedDuplex<R, W>,
        addr: TransportAddr,
        config: &TransportConfig,
        params: &HandshakeParams,
        counters: Arc<ConnectionCounters>,
    ) -> TransportResult<(Self, MuxChannels)>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let outcome = initiate(&mut framed, params, config.handshake_timeout).await?;
        let peer = PeerIdentity::new(outcome.welcome.peer_role, addr)
            .with_version(outcome.welcome.astrs_version.clone());

        Ok(Self::start(
            framed,
            peer,
            outcome.session,
            config,
            Side::Initiator,
            counters,
        ))
    }

    /// Accepts: runs the acceptor's handshake, then starts the mux.
    ///
    /// # Errors
    ///
    /// Anything [`accept`] can return.
    pub async fn accept<S>(
        stream: S,
        addr: TransportAddr,
        config: &TransportConfig,
        acceptor: Acceptor,
        assignment: SessionAssignment,
    ) -> TransportResult<(Self, MuxChannels)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let counters = ConnectionCounters::shared();
        let limits = config
            .proposed_limits(addr.requires_crc())
            .to_frame_limits();
        let framed = FramedStream::with_capacity(
            stream,
            limits,
            config.read_buffer_bytes,
            Arc::clone(&counters),
        );
        Self::accept_duplex(framed, addr, config, acceptor, assignment, counters).await
    }

    /// Accepts, choosing the session once the greeting has been read.
    ///
    /// This is the path a coordinator uses to honour a `resume` request: it
    /// cannot decide whether a session is resumable until it sees which one
    /// the peer asked for (§7.2, §12).
    ///
    /// # Errors
    ///
    /// Anything [`crate::accept_with`] can return.
    pub async fn accept_with_session<S, F>(
        stream: S,
        addr: TransportAddr,
        config: &TransportConfig,
        acceptor: Acceptor,
        assign: F,
    ) -> TransportResult<(Self, MuxChannels)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        F: FnOnce(&astrs_wire::Hello) -> SessionAssignment,
    {
        let counters = ConnectionCounters::shared();
        let limits = config
            .proposed_limits(addr.requires_crc())
            .to_frame_limits();
        let mut framed = FramedStream::with_capacity(
            stream,
            limits,
            config.read_buffer_bytes,
            Arc::clone(&counters),
        );

        let outcome =
            crate::handshake::accept_with(&mut framed, &acceptor, config.handshake_timeout, assign)
                .await?;
        let peer = PeerIdentity::new(outcome.hello.role, addr)
            .with_label(outcome.hello.label.clone())
            .with_version(outcome.hello.astrs_version.clone());

        Ok(Self::start(
            framed,
            peer,
            outcome.session,
            config,
            Side::Acceptor,
            counters,
        ))
    }

    /// Accepts over two already-separate halves.
    ///
    /// # Errors
    ///
    /// Anything [`accept`] can return.
    pub async fn accept_duplex<R, W>(
        mut framed: FramedDuplex<R, W>,
        addr: TransportAddr,
        config: &TransportConfig,
        acceptor: Acceptor,
        assignment: SessionAssignment,
        counters: Arc<ConnectionCounters>,
    ) -> TransportResult<(Self, MuxChannels)>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let outcome = accept(&mut framed, &acceptor, assignment, config.handshake_timeout).await?;
        let peer = PeerIdentity::new(outcome.hello.role, addr)
            .with_label(outcome.hello.label.clone())
            .with_version(outcome.hello.astrs_version.clone());

        Ok(Self::start(
            framed,
            peer,
            outcome.session,
            config,
            Side::Acceptor,
            counters,
        ))
    }

    /// Starts the mux over an already-handshaken stream.
    ///
    /// Public so a backend that establishes the byte stream by other means —
    /// a QUIC bidirectional stream, a test harness — can reuse the whole
    /// machine.
    #[must_use]
    pub fn start<R, W>(
        framed: FramedDuplex<R, W>,
        peer: PeerIdentity,
        session: NegotiatedSession,
        config: &TransportConfig,
        side: Side,
        counters: Arc<ConnectionCounters>,
    ) -> (Self, MuxChannels)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let compression = config.compression.negotiate(session.features);
        let max_payload = session.limits.max_payload_bytes.min(usize::MAX as u64) as usize;
        let max_routes = session.limits.max_routes;

        let (mux, channels) = RouteMux::new(
            config.mux,
            side,
            Arc::clone(&counters),
            compression,
            max_payload,
            max_routes,
        );

        let (reader, writer) = framed.into_halves();
        let shared = Arc::clone(mux.shared());
        let tasks = vec![
            tokio::spawn(run_writer(Arc::clone(&shared), writer)),
            tokio::spawn(run_reader(shared, reader)),
        ];

        let capabilities = Capabilities::stream(compression.codec, max_payload, max_routes)
            .with_route_mux(config.mux.enabled);

        (
            Self {
                peer,
                session,
                mux,
                capabilities,
                side,
                epoch: counters.epoch(),
                tasks,
                counters,
                keepalive: None,
            },
            channels,
        )
    }

    /// Overrides the reported capabilities.
    ///
    /// The QUIC backend uses this to declare native datagrams once it knows
    /// what the peer's transport parameters allowed. A narrower datagram
    /// ceiling is pushed down to the mux, so an oversize datagram is refused
    /// with [`TransportError::DatagramTooLarge`](crate::TransportError::DatagramTooLarge)
    /// rather than silently framed as a stream payload.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self.mux
            .shared()
            .set_max_datagram_bytes(capabilities.max_datagram_bytes.unwrap_or(0));
        self
    }

    /// Stamps this connection with a reconnect epoch.
    ///
    /// Also writes it into the counters, so that
    /// [`Connection::stats`]`().connection.epoch` and [`Connection::epoch`]
    /// agree — a metrics consumer that read only the snapshot would otherwise
    /// see zero for every incarnation.
    #[must_use]
    pub fn with_epoch(mut self, epoch: u64) -> Self {
        self.epoch = epoch;
        self.counters.set_epoch(epoch);
        self
    }

    /// Attaches kernel-vouched peer credentials (UDS only).
    #[must_use]
    pub fn with_credentials(mut self, credentials: Option<PeerCredentials>) -> Self {
        self.peer.credentials = credentials;
        self
    }

    /// Parks a backend-owned value for this connection's lifetime.
    ///
    /// See [`KeepAlive`] for why a backend would need this.
    #[must_use]
    pub fn with_keepalive(mut self, keepalive: KeepAlive) -> Self {
        self.keepalive = Some(keepalive);
        self
    }

    /// The route mux, for a caller that needs it directly.
    #[must_use]
    pub const fn mux(&self) -> &RouteMux {
        &self.mux
    }

    /// The shared counters.
    #[must_use]
    pub fn counters(&self) -> &Arc<ConnectionCounters> {
        &self.counters
    }

    /// Waits for both driver tasks to finish.
    ///
    /// Used by the reconnect loop, which must know the old connection is fully
    /// wound down before it dials again.
    pub async fn wait_for_shutdown(&mut self) {
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for StreamConnection {
    fn drop(&mut self) {
        // A dropped handle means nobody can read from this connection any more,
        // so leaving the driver tasks polling a socket would leak a task and a
        // file descriptor per dropped connection.
        self.mux.close(CloseReason::Dropped);
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Connection for StreamConnection {
    fn peer(&self) -> &PeerIdentity {
        &self.peer
    }

    fn session(&self) -> &NegotiatedSession {
        &self.session
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn side(&self) -> Side {
        self.side
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn is_closed(&self) -> bool {
        self.mux.is_closed()
    }

    fn close_reason(&self) -> Option<CloseReason> {
        self.mux.close_reason()
    }

    fn open_control(&self) -> ControlSender {
        self.mux.control()
    }

    fn datagrams(&self) -> DatagramSender {
        self.mux.datagrams()
    }

    fn open_route(&self, descriptor: &[u8]) -> TransportResult<RouteStream> {
        self.mux.open_route(descriptor)
    }

    fn open_route_stream(&self, route: RouteId, descriptor: &[u8]) -> TransportResult<RouteStream> {
        self.mux.open_route_with_id(route, descriptor)
    }

    fn open_route_count(&self) -> usize {
        self.mux.open_route_count()
    }

    fn stats(&self) -> TransportSnapshot {
        self.mux.snapshot()
    }

    fn close(&self, reason: CloseReason) -> BoxFuture<'_, TransportResult<()>> {
        Box::pin(async move {
            self.mux.close(reason);
            Ok(())
        })
    }
}

/// A connection plus the receiving halves its owner reads from.
///
/// Backends return this rather than a bare connection because the two are
/// useless apart: a connection with no [`MuxChannels`] can send but never
/// receive.
#[derive(Debug)]
#[non_exhaustive]
pub struct EstablishedConnection {
    /// The connection.
    pub connection: StreamConnection,
    /// Its inbound channels.
    pub channels: MuxChannels,
}

impl EstablishedConnection {
    /// Pairs a connection with its channels.
    #[must_use]
    pub const fn new(connection: StreamConnection, channels: MuxChannels) -> Self {
        Self {
            connection,
            channels,
        }
    }

    /// Splits the pair.
    #[must_use]
    pub fn into_parts(self) -> (StreamConnection, MuxChannels) {
        (self.connection, self.channels)
    }
}

/// Asserts at compile time that [`Connection`] can be made into an object.
///
/// A daemon holds `Box<dyn Connection>` in its peer table; if a method were
/// ever added that broke object safety, this would fail to compile rather than
/// failing at the daemon's call site.
const fn _assert_object_safe(_: &dyn Connection) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::{CompressionPolicy, LocalIdentity, MuxConfig};
    use crate::handshake::acceptor_from_config;
    use astrs_wire::{AuthToken, FrameKind, RoleSet, SessionId};
    use std::time::Duration;

    fn token() -> AuthToken {
        AuthToken::from_bytes([0x5a; 32])
    }

    fn addr() -> TransportAddr {
        TransportAddr::uds(std::env::temp_dir().join("astrs-conn-test.sock"))
    }

    /// Establishes a connected pair over an in-memory duplex.
    async fn pair(
        config: TransportConfig,
    ) -> (
        (StreamConnection, MuxChannels),
        (StreamConnection, MuxChannels),
    ) {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server_config = config.clone();
        let server = tokio::spawn(async move {
            let acceptor = acceptor_from_config(&server_config, token(), RoleSet::ALL, false);
            StreamConnection::accept(
                server_io,
                addr(),
                &server_config,
                acceptor,
                SessionAssignment::Fresh(SessionId::from_u128(42)),
            )
            .await
        });

        let params = HandshakeParams::from_config(
            &config,
            LocalIdentity::new(Role::Peer).with_label("daemon-a"),
            token(),
            false,
        );
        let client = StreamConnection::connect(client_io, addr(), &config, &params)
            .await
            .expect("connect");
        let server = server.await.unwrap().expect("accept");
        (client, server)
    }

    #[tokio::test]
    async fn a_connected_pair_agrees_on_the_session() {
        let ((client, _cc), (server, _sc)) = pair(TransportConfig::uds()).await;
        assert_eq!(client.session().session_id, server.session().session_id);
        assert_eq!(client.side(), Side::Initiator);
        assert_eq!(server.side(), Side::Acceptor);
        assert_eq!(client.peer().role, Role::Peer);
        assert_eq!(server.peer().label.as_deref(), Some("daemon-a"));
        assert!(!client.is_closed());
        assert_eq!(client.close_reason(), None);
        assert_eq!(client.epoch(), 0);
    }

    #[tokio::test]
    async fn control_frames_cross_in_both_directions() {
        let ((client, mut cc), (server, mut sc)) = pair(TransportConfig::uds()).await;

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
        let frame = tokio::time::timeout(Duration::from_secs(5), cc.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"pong");
    }

    #[tokio::test]
    async fn a_route_opens_and_carries_data() {
        let ((client, _cc), (_server, mut sc)) = pair(TransportConfig::uds()).await;

        let stream = client.open_route(b"camera->detector").unwrap();
        let mut accepted = tokio::time::timeout(Duration::from_secs(5), sc.accepts.accept())
            .await
            .unwrap()
            .expect("an inbound route");
        assert_eq!(accepted.descriptor(), b"camera->detector");
        assert_eq!(accepted.route(), stream.route());

        stream
            .sender()
            .send(FrameKind::Data, b"a batch")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), accepted.receiver_mut().recv())
            .await
            .unwrap()
            .expect("a route frame");
        assert_eq!(frame.payload(), b"a batch");
        assert_eq!(client.open_route_count(), 1);
    }

    #[tokio::test]
    async fn datagrams_are_emulated_on_a_stream_backend() {
        let ((client, _cc), (_server, mut sc)) = pair(TransportConfig::uds()).await;
        assert!(!client.capabilities().native_datagrams);

        client.datagrams().send(FrameKind::Data, b"pose").unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), sc.datagrams.recv())
            .await
            .unwrap()
            .expect("a datagram");
        assert_eq!(frame.payload(), b"pose");
    }

    #[tokio::test]
    async fn capabilities_report_the_negotiated_terms() {
        let config =
            TransportConfig::uds().with_compression(CompressionPolicy::codec(Compression::Lz4));
        let ((client, _cc), (server, _sc)) = pair(config).await;

        let caps = client.capabilities();
        assert!(caps.route_mux);
        assert!(!caps.native_streams);
        assert_eq!(caps.compression, Compression::Lz4);
        assert_eq!(
            caps.max_payload_bytes as u64,
            client.session().limits.max_payload_bytes
        );
        assert_eq!(caps.max_routes, client.session().limits.max_routes);
        assert_eq!(server.capabilities().compression, Compression::Lz4);
    }

    #[tokio::test]
    async fn closing_one_end_closes_the_other() {
        let ((client, _cc), (server, _sc)) = pair(TransportConfig::uds()).await;
        client.close(CloseReason::local("done")).await.unwrap();
        assert!(client.is_closed());

        for _ in 0..200 {
            if server.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(server.is_closed());
        assert_eq!(server.close_reason(), Some(CloseReason::Eof));
    }

    #[tokio::test]
    async fn dropping_a_connection_stops_its_tasks() {
        let ((client, _cc), (server, _sc)) = pair(TransportConfig::uds()).await;
        drop(client);
        for _ in 0..200 {
            if server.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(server.is_closed());
    }

    #[tokio::test]
    async fn a_connection_is_usable_behind_a_trait_object() {
        let ((client, _cc), (_server, mut sc)) = pair(TransportConfig::uds()).await;
        let boxed: Box<dyn Connection> = Box::new(client);
        _assert_object_safe(boxed.as_ref());

        boxed
            .open_control()
            .send(FrameKind::PeerEvent, b"via dyn")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), sc.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"via dyn");

        // The greeting counts too, so the control frame is not the first.
        assert!(boxed.stats().connection.frames_sent >= 2);
        boxed.close(CloseReason::local("bye")).await.unwrap();
    }

    #[tokio::test]
    async fn stats_cover_the_connection_and_its_routes() {
        let ((client, _cc), (_server, mut sc)) = pair(TransportConfig::uds()).await;
        let stream = client.open_route(b"").unwrap();
        let mut accepted = sc.accepts.accept().await.expect("an inbound route");
        for _ in 0..4 {
            stream
                .sender()
                .send(FrameKind::Data, &[7u8; 128])
                .await
                .unwrap();
        }
        for _ in 0..4 {
            accepted.receiver_mut().recv().await.expect("a route frame");
        }

        let snapshot = client.stats();
        assert_eq!(snapshot.routes.len(), 1);
        assert_eq!(snapshot.routes[&stream.route()].frames_sent, 4);
        assert!(snapshot.connection.frames_sent >= 4);
        assert_eq!(snapshot.connection.routes_active, 1);
    }

    #[tokio::test]
    async fn a_mux_less_connection_still_carries_control() {
        let config = TransportConfig::uds().with_mux(MuxConfig::disabled());
        let ((client, _cc), (_server, mut sc)) = pair(config).await;
        assert!(!client.capabilities().route_mux);

        client
            .open_control()
            .send(FrameKind::PeerEvent, b"still works")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), sc.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"still works");
    }

    #[test]
    fn a_peer_identity_describes_itself() {
        let identity = PeerIdentity::new(Role::Daemon, addr()).with_label(Some("daemon-b".into()));
        assert!(identity.to_string().contains("daemon-b"));
        assert!(identity.describe().contains("uds:"));
        assert_eq!(identity.plane, Plane::Uds);

        let bare = PeerIdentity::new(Role::Node, addr());
        assert!(!bare.to_string().contains('('));
        assert_eq!(bare.credentials, None);
    }

    #[test]
    fn credentials_carry_what_the_kernel_vouched_for() {
        let creds = PeerCredentials::new(501, 20).with_pid(Some(4_242));
        assert_eq!(creds.uid, 501);
        assert_eq!(creds.gid, 20);
        assert_eq!(creds.pid, Some(4_242));
        assert!(creds.runs_as(501));
        assert!(!creds.runs_as(0));
        assert!(creds.runs_as_any(&[0, 501]));
        assert!(!creds.runs_as_any(&[0, 1]));

        let identity = PeerIdentity::new(Role::Node, addr()).with_credentials(Some(creds));
        assert_eq!(identity.credentials, Some(creds));
    }

    #[test]
    fn capability_builders_describe_each_backend() {
        let stream = Capabilities::stream(Compression::None, 1 << 20, 64);
        assert!(!stream.native_datagrams);
        assert!(!stream.native_streams);
        assert!(stream.route_mux);
        assert_eq!(stream.max_datagram_bytes, None);

        let quic = stream
            .with_native_datagrams(Some(1_200))
            .with_native_streams(true);
        assert!(quic.native_datagrams);
        assert!(quic.native_streams);
        assert_eq!(quic.max_datagram_bytes, Some(1_200));

        assert!(!stream.with_route_mux(false).route_mux);
    }
}
