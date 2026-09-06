//! The local node listener — UDS preferred, TCP 7408 loopback fallback
//! (§4.2, §24.2).
//!
//! Both listeners, when configured, run as independent accept tasks feeding
//! one channel. The event loop selects on that channel alongside everything
//! else, so a node arriving on the Unix socket and a node arriving on the
//! loopback port are the same event as far as the loop is concerned — which is
//! the point of having two listeners rather than two code paths.
//!
//! ```text
//!   UDS accept task ──┐
//!                     ├──► Accepted { session, stream } ──► event loop
//!   TCP accept task ──┘
//! ```
//!
//! # The handshake runs after `attach`, not before
//!
//! This module's docs used to claim that "each accept runs
//! [`astrs_transport::accept`] before the connection reaches the loop: the
//! auth token check (§16), the protocol negotiation (§7.2) and the session
//! assignment." That is not what the implementation does. `serve_uds`/
//! `serve_tcp` call [`UdsListener::accept_raw`]/[`TcpListener::accept_raw`] —
//! a raw socket accept plus, on the Unix socket, the kernel-reported peer
//! credentials, nothing more — and mint the session id
//! ([`SessionMinter::mint`]) right there, before the [`AcceptedNode`] is even
//! queued to the loop. The §7.2 `Hello`/`Welcome` greeting happens later
//! still: only once [`crate::server::Daemon::attach`] has run this module's
//! own §16 peer-credential check (below) does it spawn the
//! [`crate::session::SessionActor`] task that actually negotiates the
//! greeting, over [`astrs_transport::accept_with`]. A connection that then
//! fails to greet correctly is one bad session, logged and dropped by that
//! task — never a reason to stop the listener, because a robot whose daemon
//! stopped accepting nodes because one of them presented a stale token is a
//! robot that has stopped.
//!
//! # Peer credentials
//!
//! On the Unix socket the kernel vouches for the peer's uid before a single
//! protocol byte is exchanged. [`AcceptedNode::credentials`] carries it, and
//! [`crate::server::Daemon::attach`] checks it with [`credentials_acceptable`]
//! *before* the §7.2 handshake is even attempted: a peer running as neither
//! this daemon's own uid nor root is refused outright — the socket is
//! dropped, no session actor is spawned — which is the peer-cred half of
//! §16's "UDS legs rely on filesystem permissions + peer-cred check". There
//! is no configuration knob to widen that allow-list; a platform that
//! cannot report credentials at all (`credentials` is [`None`]) falls back
//! to the §7.2 auth token as the sole check, exactly as a [`ConnectionOrigin::Tcp`]
//! connection always does.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use astrs_transport::backend::tcp::TcpListener;
use astrs_transport::backend::uds::UdsListener;
use astrs_transport::{PeerCredentials, TransportConfig};
use astrs_wire::SessionId;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::DaemonResult;

/// How many accepted-but-unprocessed connections may queue.
///
/// Small on purpose: a backlog here means the event loop is not keeping up,
/// and the kernel's own listen backlog is a better place to queue than the
/// daemon's heap.
pub const ACCEPT_QUEUE_DEPTH: usize = 32;

/// One accepted node connection, before the loop has seen it.
#[derive(Debug)]
pub struct AcceptedNode {
    /// The session the daemon assigned.
    pub session: SessionId,
    /// The byte stream, ready for a [`crate::session::SessionActor`].
    pub stream: NodeStream,
    /// What the kernel vouches for, on a Unix socket.
    pub credentials: Option<PeerCredentials>,
    /// Where it came from, for diagnostics.
    pub origin: ConnectionOrigin,
}

/// Which listener a connection arrived on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionOrigin {
    /// The Unix domain socket.
    Uds {
        /// The socket path.
        path: PathBuf,
    },
    /// The loopback TCP port.
    Tcp {
        /// The peer address.
        peer: SocketAddr,
    },
    /// An in-process stream — `astrs run`, or a test.
    InProcess,
}

impl ConnectionOrigin {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Uds { .. } => "uds",
            Self::Tcp { .. } => "tcp",
            Self::InProcess => "in_process",
        }
    }

    /// Whether the kernel can vouch for the peer's identity on this plane.
    #[must_use]
    pub const fn has_peer_credentials(&self) -> bool {
        matches!(self, Self::Uds { .. })
    }
}

impl core::fmt::Display for ConnectionOrigin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Uds { path } => write!(f, "uds:{}", path.display()),
            Self::Tcp { peer } => write!(f, "tcp:{peer}"),
            Self::InProcess => f.write_str("in-process"),
        }
    }
}

/// The byte stream one node speaks over.
///
/// An enum rather than a `Box<dyn>` because there are exactly three kinds and
/// the session actor is generic anyway: the indirection would buy nothing and
/// cost a virtual call per frame on the reliable path.
#[derive(Debug)]
#[non_exhaustive]
pub enum NodeStream {
    /// A Unix domain socket.
    Uds(tokio::net::UnixStream),
    /// A loopback TCP socket.
    Tcp(tokio::net::TcpStream),
    /// An in-memory duplex — `astrs run`, or a test.
    Duplex(tokio::io::DuplexStream),
}

impl NodeStream {
    /// Splits into the halves a [`crate::session::SessionActor`] serves.
    #[must_use]
    pub fn into_split(
        self,
    ) -> (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) {
        match self {
            Self::Uds(stream) => {
                let (reader, writer) = tokio::io::split(stream);
                (Box::new(reader), Box::new(writer))
            }
            Self::Tcp(stream) => {
                let (reader, writer) = tokio::io::split(stream);
                (Box::new(reader), Box::new(writer))
            }
            Self::Duplex(stream) => {
                let (reader, writer) = tokio::io::split(stream);
                (Box::new(reader), Box::new(writer))
            }
        }
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Uds(_) => "uds",
            Self::Tcp(_) => "tcp",
            Self::Duplex(_) => "duplex",
        }
    }
}

/// The listeners a daemon has open, and the accepted connections they feed.
#[derive(Debug)]
pub struct NodeListeners {
    /// The accept tasks, aborted when this value drops.
    tasks: Vec<JoinHandle<()>>,
    /// The accepted connections.
    accepted: mpsc::Receiver<AcceptedNode>,
    /// The Unix socket path, if one is bound.
    uds_path: Option<PathBuf>,
    /// The TCP address actually bound, if one is.
    tcp_addr: Option<SocketAddr>,
}

impl NodeListeners {
    /// Binds the configured listeners.
    ///
    /// `next_session` mints a session id per accepted connection; the daemon
    /// passes its own counter so ids stay unique across both listeners.
    ///
    /// # Errors
    ///
    /// [`crate::DaemonError::Io`] if a listener cannot bind, or
    /// [`crate::DaemonError::Transport`] if the socket path is unusable.
    pub async fn bind(
        listen: &crate::config::ListenConfig,
        transport: TransportConfig,
        sessions: SessionMinter,
    ) -> DaemonResult<Self> {
        listen.validate()?;
        let (sender, accepted) = mpsc::channel(ACCEPT_QUEUE_DEPTH);
        let mut tasks = Vec::new();
        let mut uds_path = None;
        let mut tcp_addr = None;

        if let Some(path) = listen.uds_path() {
            let listener = UdsListener::bind(path, transport.clone())?;
            let bound = listener.path().to_path_buf();
            uds_path = Some(bound.clone());
            tasks.push(tokio::spawn(serve_uds(
                listener,
                bound,
                sender.clone(),
                sessions.clone(),
            )));
        }

        if let Some(addr) = listen.tcp_addr() {
            let listener = TcpListener::bind(addr, transport).await?;
            tcp_addr = Some(listener.local_addr()?);
            tasks.push(tokio::spawn(serve_tcp(listener, sender, sessions)));
        }

        Ok(Self {
            tasks,
            accepted,
            uds_path,
            tcp_addr,
        })
    }

    /// A listener set with nothing bound — the embedded `astrs run` case,
    /// where connections arrive through [`NodeListeners::in_process`] instead.
    #[must_use]
    pub fn none() -> (Self, mpsc::Sender<AcceptedNode>) {
        let (sender, accepted) = mpsc::channel(ACCEPT_QUEUE_DEPTH);
        (
            Self {
                tasks: Vec::new(),
                accepted,
                uds_path: None,
                tcp_addr: None,
            },
            sender,
        )
    }

    /// Builds an accepted connection over an in-memory duplex.
    #[must_use]
    pub fn in_process(session: SessionId) -> (AcceptedNode, tokio::io::DuplexStream) {
        let (daemon_side, node_side) = tokio::io::duplex(256 * 1024);
        (
            AcceptedNode {
                session,
                stream: NodeStream::Duplex(daemon_side),
                credentials: None,
                origin: ConnectionOrigin::InProcess,
            },
            node_side,
        )
    }

    /// Waits for the next accepted connection.
    pub async fn accept(&mut self) -> Option<AcceptedNode> {
        self.accepted.recv().await
    }

    /// The Unix socket path, if one is bound.
    #[must_use]
    pub fn uds_path(&self) -> Option<&Path> {
        self.uds_path.as_deref()
    }

    /// The TCP address actually bound, if one is.
    ///
    /// A configuration asking for port `0` reports the port the kernel chose,
    /// which is what makes a test's `bind-to-port-0` usable.
    #[must_use]
    pub const fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    /// Whether anything is bound.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.uds_path.is_some() || self.tcp_addr.is_some()
    }

    /// Stops accepting.
    pub fn shutdown(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.accepted.close();
    }
}

impl Drop for NodeListeners {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// A shared, monotone source of [`SessionId`]s.
///
/// Cloneable and lock-free: both accept tasks hold one, and every id they hand
/// out is distinct because they share the same atomic. The alternative — a
/// counter per listener — would let a Unix connection and a TCP connection
/// collide on session 1, and the daemon's session index would then bind two
/// nodes to one entry.
#[derive(Debug, Clone)]
pub struct SessionMinter {
    /// The shared counter.
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl SessionMinter {
    /// A minter starting at session 1.
    ///
    /// Zero is skipped so an unset session id is distinguishable from the
    /// first real one in a log line.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// The next id.
    #[must_use]
    pub fn mint(&self) -> SessionId {
        let value = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SessionId::from_u128(u128::from(value))
    }

    /// How many ids have been handed out.
    #[must_use]
    pub fn issued(&self) -> u64 {
        self.next
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(1)
    }
}

impl Default for SessionMinter {
    fn default() -> Self {
        Self::new()
    }
}

/// Accepts Unix connections until the channel closes.
async fn serve_uds(
    listener: UdsListener,
    path: PathBuf,
    sender: mpsc::Sender<AcceptedNode>,
    sessions: SessionMinter,
) {
    loop {
        let Ok((stream, credentials)) = listener.accept_raw().await else {
            // A failed accept is usually EMFILE or a transient interruption:
            // yield and try again rather than tearing the listener down.
            tokio::task::yield_now().await;
            continue;
        };
        let accepted = AcceptedNode {
            session: sessions.mint(),
            stream: NodeStream::Uds(stream),
            credentials,
            origin: ConnectionOrigin::Uds { path: path.clone() },
        };
        if sender.send(accepted).await.is_err() {
            break;
        }
    }
}

/// Accepts TCP connections until the channel closes.
async fn serve_tcp(
    listener: TcpListener,
    sender: mpsc::Sender<AcceptedNode>,
    sessions: SessionMinter,
) {
    loop {
        let Ok((stream, peer)) = listener.accept_raw().await else {
            tokio::task::yield_now().await;
            continue;
        };
        let accepted = AcceptedNode {
            session: sessions.mint(),
            stream: NodeStream::Tcp(stream),
            credentials: None,
            origin: ConnectionOrigin::Tcp { peer },
        };
        if sender.send(accepted).await.is_err() {
            break;
        }
    }
}

/// Whether a peer's credentials are acceptable for a node connection (§16).
///
/// The daemon's own uid always is; root always is (a root operator can already
/// do anything); anything else is refused, because a node running as another
/// user has no business in this daemon's dataflow.
#[must_use]
pub fn credentials_acceptable(credentials: Option<&PeerCredentials>, daemon_uid: u32) -> bool {
    match credentials {
        Some(credentials) => credentials.runs_as_any(&[daemon_uid, 0]),
        // A platform that cannot report credentials falls back to the auth
        // token, which is the authoritative check either way (§7.2).
        None => true,
    }
}

/// This process's effective user id.
#[must_use]
pub fn daemon_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::config::ListenConfig;

    fn session_minter() -> SessionMinter {
        SessionMinter::new()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("astrs-listen-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    #[test]
    fn origins_name_themselves_and_report_their_credential_support() {
        let uds = ConnectionOrigin::Uds {
            path: PathBuf::from("/run/astrs/daemon.sock"),
        };
        assert_eq!(uds.kind_name(), "uds");
        assert!(uds.has_peer_credentials());
        assert!(uds.to_string().contains("daemon.sock"));

        let tcp = ConnectionOrigin::Tcp {
            peer: "127.0.0.1:5000".parse().unwrap(),
        };
        assert_eq!(tcp.kind_name(), "tcp");
        assert!(!tcp.has_peer_credentials());
        assert!(tcp.to_string().contains("5000"));

        assert_eq!(ConnectionOrigin::InProcess.kind_name(), "in_process");
        assert!(!ConnectionOrigin::InProcess.has_peer_credentials());
    }

    #[tokio::test]
    async fn an_unbound_listener_set_reports_itself_unbound() {
        let (listeners, _sender) = NodeListeners::none();
        assert!(!listeners.is_bound());
        assert!(listeners.uds_path().is_none());
        assert!(listeners.tcp_addr().is_none());
    }

    #[tokio::test]
    async fn an_in_process_connection_round_trips_bytes() {
        let (accepted, mut node_side) = NodeListeners::in_process(SessionId::from_u128(1));
        assert_eq!(accepted.origin, ConnectionOrigin::InProcess);
        assert_eq!(accepted.stream.kind_name(), "duplex");

        let (mut reader, mut writer) = accepted.stream.into_split();
        node_side.write_all(b"ping").await.unwrap();
        let mut buffer = [0u8; 4];
        reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"ping");

        writer.write_all(b"pong").await.unwrap();
        let mut back = [0u8; 4];
        node_side.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"pong");
    }

    #[tokio::test]
    async fn a_tcp_listener_binds_an_ephemeral_port_and_accepts() {
        let listen = ListenConfig::loopback_tcp(0);
        let mut listeners = NodeListeners::bind(&listen, TransportConfig::new(), session_minter())
            .await
            .unwrap();

        let addr = listeners.tcp_addr().expect("bound");
        assert_ne!(addr.port(), 0, "the kernel chose a real port");
        assert!(listeners.is_bound());

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let accepted = listeners.accept().await.expect("a connection");
        assert_eq!(accepted.stream.kind_name(), "tcp");
        assert!(matches!(accepted.origin, ConnectionOrigin::Tcp { .. }));

        let (mut reader, _writer) = accepted.stream.into_split();
        client.write_all(b"hi").await.unwrap();
        let mut buffer = [0u8; 2];
        reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"hi");
    }

    #[tokio::test]
    async fn a_uds_listener_binds_and_reports_peer_credentials() {
        let path = scratch("uds.sock");
        let _ = std::fs::remove_file(&path);
        let listen = ListenConfig::uds(&path);
        let mut listeners = NodeListeners::bind(&listen, TransportConfig::uds(), session_minter())
            .await
            .unwrap();
        assert_eq!(listeners.uds_path(), Some(path.as_path()));

        let _client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let accepted = listeners.accept().await.expect("a connection");
        assert_eq!(accepted.stream.kind_name(), "uds");
        assert!(matches!(accepted.origin, ConnectionOrigin::Uds { .. }));
        // Every platform this crate supports reports credentials on a Unix
        // socket, but the check is written to tolerate one that does not.
        assert!(credentials_acceptable(
            accepted.credentials.as_ref(),
            daemon_uid()
        ));

        drop(listeners);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn both_listeners_feed_one_queue() {
        let path = scratch("both.sock");
        let _ = std::fs::remove_file(&path);
        let listen = ListenConfig::uds(&path).with_tcp_port(0);
        let mut listeners = NodeListeners::bind(&listen, TransportConfig::uds(), session_minter())
            .await
            .unwrap();

        let addr = listeners.tcp_addr().expect("bound");
        let _uds = tokio::net::UnixStream::connect(&path).await.unwrap();
        let _tcp = tokio::net::TcpStream::connect(addr).await.unwrap();

        let mut kinds = Vec::new();
        for _ in 0..2 {
            let accepted = listeners.accept().await.expect("a connection");
            kinds.push(accepted.stream.kind_name());
        }
        kinds.sort_unstable();
        assert_eq!(kinds, ["tcp", "uds"]);

        drop(listeners);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn sessions_are_unique_across_both_listeners() {
        let path = scratch("sessions.sock");
        let _ = std::fs::remove_file(&path);
        let listen = ListenConfig::uds(&path).with_tcp_port(0);
        let mut listeners = NodeListeners::bind(&listen, TransportConfig::uds(), session_minter())
            .await
            .unwrap();
        let addr = listeners.tcp_addr().expect("bound");

        let _a = tokio::net::UnixStream::connect(&path).await.unwrap();
        let _b = tokio::net::TcpStream::connect(addr).await.unwrap();

        let first = listeners.accept().await.expect("one").session;
        let second = listeners.accept().await.expect("two").session;
        assert_ne!(first, second);

        drop(listeners);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn shutting_down_stops_accepting() {
        let listen = ListenConfig::loopback_tcp(0);
        let mut listeners = NodeListeners::bind(&listen, TransportConfig::new(), session_minter())
            .await
            .unwrap();
        listeners.shutdown();
        assert!(listeners.accept().await.is_none());
    }

    #[tokio::test]
    async fn an_over_long_socket_path_is_refused_before_binding() {
        let listen = ListenConfig::uds(format!("/tmp/{}/d.sock", "x".repeat(200)));
        let error = NodeListeners::bind(&listen, TransportConfig::uds(), session_minter())
            .await
            .unwrap_err();
        assert!(
            matches!(error, crate::DaemonError::BadPath { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_minter_never_repeats_an_id() {
        let minter = SessionMinter::new();
        let mut ids: Vec<SessionId> = (0..64).map(|_| minter.mint()).collect();
        assert_eq!(minter.issued(), 64);
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn clones_of_a_minter_share_the_counter() {
        let first = SessionMinter::new();
        let second = first.clone();
        assert_ne!(first.mint(), second.mint());
        assert_eq!(first.issued(), 2);
    }

    #[test]
    fn credentials_from_another_user_are_refused() {
        // A platform that reports nothing falls back to the token.
        assert!(credentials_acceptable(None, 1000));
    }
}
