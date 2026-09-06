//! The Unix-domain-socket backend: node ↔ daemon (blueprint §6.4).
//!
//! A node and its daemon live on the same machine, share a kernel, and pass
//! shared-memory file descriptors to each other. A Unix socket is the right
//! substrate for that leg, and it brings two things a TCP loopback does not:
//!
//! - **Peer credentials.** The kernel tells the daemon the uid, gid and pid of
//!   the process on the other end, unforgeable by the peer. That is the
//!   foundation of the §16 check that a connecting node really is one the
//!   daemon spawned.
//! - **No checksum needed.** The kernel already guarantees the bytes, so
//!   [`FrameFlags::CRC`](astrs_wire::FrameFlags::CRC) is optional here and
//!   mandatory on the network legs (§7.1). [`crate::TransportConfig::uds`]
//!   turns it off by default.
//!
//! # The socket file
//!
//! A Unix socket is a filesystem entry, and a daemon that crashed leaves one
//! behind. [`UdsListener::bind`] therefore removes a *stale* socket before
//! binding — one that exists but that nothing is listening on — and refuses to
//! touch a *live* one, which would silently steal another daemon's clients.
//! The distinction is made by trying to connect to it first.
//!
//! # Examples
//!
//! ```no_run
//! use astrs_transport::backend::uds::{UdsListener, connect};
//! use astrs_transport::{LocalIdentity, HandshakeParams, TransportConfig};
//! use astrs_wire::{AuthToken, Role, RoleSet, SessionAssignment, SessionId};
//!
//! # async fn example() -> Result<(), astrs_transport::TransportError> {
//! let path = std::env::temp_dir().join("astrs-example.sock");
//! let config = TransportConfig::uds();
//! let token = AuthToken::from_bytes([1; 32]);
//!
//! let listener = UdsListener::bind(&path, config.clone())?;
//! let params = HandshakeParams::from_config(
//!     &config,
//!     LocalIdentity::new(Role::Node),
//!     token.clone(),
//!     false,
//! );
//! let (node, _channels) = connect(&path, &config, &params).await?;
//! # let _ = (listener, node);
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use astrs_wire::{Acceptor, SessionAssignment};
use tokio::net::{UnixListener, UnixStream};

use crate::addr::TransportAddr;
use crate::config::TransportConfig;
use crate::conn::{PeerCredentials, StreamConnection};
use crate::error::{TransportError, TransportResult};
use crate::handshake::HandshakeParams;
use crate::mux::MuxChannels;

/// Dials the daemon socket at `path` and completes the handshake.
///
/// # Errors
///
/// - [`TransportError::Io`] if the socket is missing or the connect fails.
/// - [`TransportError::Timeout`] if the dial exceeds
///   [`TransportConfig::connect_timeout`].
/// - Anything [`crate::initiate`] can return.
pub async fn connect(
    path: impl AsRef<Path>,
    config: &TransportConfig,
    params: &HandshakeParams,
) -> TransportResult<(StreamConnection, MuxChannels)> {
    let path = path.as_ref();
    let stream = dial(path, config.connect_timeout).await?;
    let credentials = peer_credentials(&stream);
    let addr = TransportAddr::uds(path.to_path_buf());
    let (connection, channels) = StreamConnection::connect(stream, addr, config, params).await?;
    Ok((connection.with_credentials(credentials), channels))
}

/// Dials with a deadline.
async fn dial(path: &Path, timeout: Duration) -> TransportResult<UnixStream> {
    match tokio::time::timeout(timeout, UnixStream::connect(path)).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(TransportError::Timeout {
            operation: "uds connect",
            timeout,
        }),
    }
}

/// Reads the credentials the kernel vouches for, if the platform reports them.
///
/// A platform that cannot answer is not an error: the handshake token is the
/// authoritative check (§7.2), and credentials are the cheap pre-filter.
#[must_use]
pub fn peer_credentials(stream: &UnixStream) -> Option<PeerCredentials> {
    let cred = stream.peer_cred().ok()?;
    Some(PeerCredentials::new(cred.uid(), cred.gid()).with_pid(cred.pid()))
}

/// A listening Unix socket.
///
/// Unlinks its socket file on drop, so a clean shutdown leaves no stale entry
/// for the next daemon to reason about.
#[derive(Debug)]
pub struct UdsListener {
    /// The bound socket.
    listener: UnixListener,
    /// Where it lives, for unlinking and for the peer address.
    path: PathBuf,
    /// The policy connections accepted here run under.
    config: TransportConfig,
    /// Whether to unlink the socket file on drop.
    unlink_on_drop: bool,
}

impl UdsListener {
    /// Binds `path`, removing a stale socket first.
    ///
    /// # Errors
    ///
    /// - [`TransportError::Io`] if the bind fails.
    /// - [`TransportError::Configuration`] if `path` is occupied by a socket
    ///   something is still listening on — taking it over would silently steal
    ///   another daemon's clients.
    pub fn bind(path: impl AsRef<Path>, config: TransportConfig) -> TransportResult<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            if is_live_socket(&path) {
                return Err(TransportError::Configuration(format!(
                    "{} is already in use by a live listener",
                    path.display()
                )));
            }
            std::fs::remove_file(&path)?;
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        Ok(Self {
            listener,
            path,
            config,
            unlink_on_drop: true,
        })
    }

    /// Wraps an already-bound listener, taking no responsibility for the file.
    ///
    /// Used when the socket was created by a supervisor (systemd socket
    /// activation, a test harness) and this process must not unlink it.
    #[must_use]
    pub fn from_listener(listener: UnixListener, path: PathBuf, config: TransportConfig) -> Self {
        Self {
            listener,
            path,
            config,
            unlink_on_drop: false,
        }
    }

    /// The path this listener is bound to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The address peers reach this listener at.
    #[must_use]
    pub fn addr(&self) -> TransportAddr {
        TransportAddr::uds(self.path.clone())
    }

    /// The policy connections accepted here run under.
    #[must_use]
    pub const fn config(&self) -> &TransportConfig {
        &self.config
    }

    /// Accepts one connection and completes its handshake.
    ///
    /// A handshake that fails does **not** end the listener: a node that
    /// presented the wrong token is one bad connection, not a reason to stop
    /// serving the others. The caller sees the error and loops.
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
        let (stream, _) = self.listener.accept().await?;
        self.serve(stream, acceptor, assignment).await
    }

    /// Runs the acceptor's handshake over an already-accepted socket.
    ///
    /// # Errors
    ///
    /// As [`UdsListener::accept`].
    pub async fn serve(
        &self,
        stream: UnixStream,
        acceptor: &Acceptor,
        assignment: SessionAssignment,
    ) -> TransportResult<(StreamConnection, MuxChannels)> {
        let credentials = peer_credentials(&stream);
        let (connection, channels) = StreamConnection::accept(
            stream,
            self.addr(),
            &self.config,
            acceptor.clone(),
            assignment,
        )
        .await?;
        Ok((connection.with_credentials(credentials), channels))
    }

    /// Accepts one connection, choosing its session once the greeting arrives.
    ///
    /// The path a daemon uses to honour a node's `resume` request (§7.2, §12).
    ///
    /// # Errors
    ///
    /// As [`UdsListener::accept`].
    pub async fn accept_with<F>(
        &self,
        acceptor: &Acceptor,
        assign: F,
    ) -> TransportResult<(StreamConnection, MuxChannels)>
    where
        F: FnOnce(&astrs_wire::Hello) -> SessionAssignment,
    {
        let (stream, _) = self.listener.accept().await?;
        let credentials = peer_credentials(&stream);
        let (connection, channels) = StreamConnection::accept_with_session(
            stream,
            self.addr(),
            &self.config,
            acceptor.clone(),
            assign,
        )
        .await?;
        Ok((connection.with_credentials(credentials), channels))
    }

    /// Accepts the raw socket without running a handshake.
    ///
    /// For a caller that must inspect credentials before deciding whether to
    /// spend a handshake on the peer at all.
    ///
    /// # Errors
    ///
    /// [`TransportError::Io`] if the accept fails.
    pub async fn accept_raw(&self) -> TransportResult<(UnixStream, Option<PeerCredentials>)> {
        let (stream, _) = self.listener.accept().await?;
        let credentials = peer_credentials(&stream);
        Ok((stream, credentials))
    }

    /// Gives up responsibility for unlinking the socket file.
    pub const fn leak_socket_file(&mut self) {
        self.unlink_on_drop = false;
    }
}

impl Drop for UdsListener {
    fn drop(&mut self) {
        if self.unlink_on_drop {
            // Best effort: the directory may already be gone in a teardown
            // race, and failing to unlink is not worth a panic in a destructor.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether something is currently listening on `path`.
///
/// The only portable way to ask is to try to connect. A refused connection
/// means the file is a leftover; a successful one means a live listener, whose
/// socket must not be taken over.
fn is_live_socket(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::LocalIdentity;
    use crate::conn::Connection;
    use crate::error::CloseReason;
    use crate::handshake::acceptor_from_config;
    use astrs_wire::{AuthToken, FrameKind, Role, RoleSet, SessionId};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique socket path under the platform temp directory.
    ///
    /// Unix socket paths are length-limited (104 bytes on macOS, 108 on
    /// Linux), so the name stays short.
    fn socket_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("astrs-{tag}-{pid}-{unique}.sock"))
    }

    fn token() -> AuthToken {
        AuthToken::from_bytes([0x33; 32])
    }

    fn params(config: &TransportConfig, role: Role) -> HandshakeParams {
        HandshakeParams::from_config(config, LocalIdentity::new(role), token(), false)
    }

    #[tokio::test]
    async fn a_node_and_a_daemon_exchange_frames_over_a_socket() {
        let path = socket_path("echo");
        let config = TransportConfig::uds();
        let listener = UdsListener::bind(&path, config.clone()).unwrap();
        assert_eq!(listener.path(), path.as_path());
        assert_eq!(listener.addr(), TransportAddr::uds(path.clone()));

        let acceptor = acceptor_from_config(&config, token(), RoleSet::NODES, false);
        let server = tokio::spawn(async move {
            let outcome = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await;
            (outcome, listener)
        });

        let (node, _channels) = connect(&path, &config, &params(&config, Role::Node))
            .await
            .unwrap();
        let ((daemon, mut daemon_channels), _listener) = {
            let (outcome, listener) = server.await.unwrap();
            (outcome.unwrap(), listener)
        };

        node.open_control()
            .send(FrameKind::NodeRequest, b"register")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), daemon_channels.control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"register");
        assert_eq!(daemon.peer().role, Role::Node);
        assert_eq!(node.peer().plane, astrs_wire::Plane::Uds);
    }

    #[tokio::test]
    async fn the_kernel_vouches_for_the_peer() {
        let path = socket_path("cred");
        let config = TransportConfig::uds();
        let listener = UdsListener::bind(&path, config.clone()).unwrap();

        let acceptor = acceptor_from_config(&config, token(), RoleSet::NODES, false);
        let server = tokio::spawn(async move {
            listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
                .map(|(connection, _)| connection)
        });

        let (node, _channels) = connect(&path, &config, &params(&config, Role::Node))
            .await
            .unwrap();
        let daemon = server.await.unwrap().unwrap();

        let creds = daemon
            .peer()
            .credentials
            .expect("the kernel reports credentials on this platform");
        // Both ends are this process, so both see this process's own ids.
        assert!(creds.runs_as(creds.uid));
        assert_eq!(node.peer().credentials.map(|c| c.uid), Some(creds.uid));
    }

    #[tokio::test]
    async fn a_uds_leg_skips_the_checksum_by_default() {
        let path = socket_path("nocrc");
        let config = TransportConfig::uds();
        let listener = UdsListener::bind(&path, config.clone()).unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::NODES, false);
        let server = tokio::spawn(async move {
            listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
                .map(|(connection, _)| connection)
        });

        let (node, _channels) = connect(&path, &config, &params(&config, Role::Node))
            .await
            .unwrap();
        let daemon = server.await.unwrap().unwrap();
        assert!(!node.session().limits.require_crc);
        assert!(!daemon.session().limits.require_crc);
    }

    #[tokio::test]
    async fn a_stale_socket_file_is_replaced() {
        let path = socket_path("stale");
        {
            let listener = UdsListener::bind(&path, TransportConfig::uds()).unwrap();
            assert!(path.exists());
            drop(listener);
        }
        // The drop unlinked it; recreate a leftover by hand.
        std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(path.exists());

        // A leftover with nothing listening is replaced, not refused.
        let listener = UdsListener::bind(&path, TransportConfig::uds()).unwrap();
        assert!(path.exists());
        drop(listener);
        assert!(!path.exists(), "drop must unlink the socket");
    }

    #[tokio::test]
    async fn a_live_socket_is_never_stolen() {
        let path = socket_path("live");
        let first = UdsListener::bind(&path, TransportConfig::uds()).unwrap();
        let err = UdsListener::bind(&path, TransportConfig::uds()).unwrap_err();
        assert!(matches!(err, TransportError::Configuration(_)));
        drop(first);
    }

    #[tokio::test]
    async fn a_missing_socket_is_an_io_error_not_a_hang() {
        let path = socket_path("absent");
        let config = TransportConfig::uds();
        let err = connect(&path, &config, &params(&config, Role::Node))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Io(_)));
        assert!(err.is_retryable(), "the daemon may still be starting");
    }

    #[tokio::test]
    async fn a_bad_token_is_refused_without_ending_the_listener() {
        let path = socket_path("auth");
        let config = TransportConfig::uds();
        let listener = UdsListener::bind(&path, config.clone()).unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::NODES, false);

        let server = tokio::spawn(async move {
            let first = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await;
            let second = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(2)))
                .await;
            (first, second, listener)
        });

        // A node with the wrong token.
        let bad = HandshakeParams::from_config(
            &config,
            LocalIdentity::new(Role::Node),
            AuthToken::from_bytes([0xff; 32]),
            false,
        );
        assert!(connect(&path, &config, &bad).await.is_err());

        // …and a good one right behind it.
        let (node, _channels) = connect(&path, &config, &params(&config, Role::Node))
            .await
            .unwrap();
        let (first, second, _listener) = server.await.unwrap();
        assert!(first.is_err(), "the bad token must be refused");
        assert!(second.is_ok(), "the listener must keep serving");
        // Both ends see the connection's role, which is the one the node
        // opened it as.
        assert_eq!(node.peer().role, Role::Node);
    }

    #[tokio::test]
    async fn a_raw_accept_hands_back_credentials_before_the_handshake() {
        let path = socket_path("raw");
        let config = TransportConfig::uds();
        let listener = UdsListener::bind(&path, config.clone()).unwrap();

        let server = tokio::spawn(async move {
            let outcome = listener.accept_raw().await;
            (outcome, listener)
        });
        let client = UnixStream::connect(&path).await.unwrap();

        let ((stream, credentials), listener) = {
            let (outcome, listener) = server.await.unwrap();
            (outcome.unwrap(), listener)
        };
        assert!(credentials.is_some());
        assert!(listener.config().require_crc == Some(false));
        drop(stream);
        drop(client);
    }

    #[tokio::test]
    async fn a_wrapped_listener_leaves_the_socket_file_alone() {
        let path = socket_path("wrapped");
        let raw = UnixListener::bind(&path).unwrap();
        {
            let listener = UdsListener::from_listener(raw, path.clone(), TransportConfig::uds());
            assert_eq!(listener.path(), path.as_path());
        }
        assert!(path.exists(), "a wrapped listener must not unlink");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn leaking_the_socket_file_survives_the_drop() {
        let path = socket_path("leak");
        {
            let mut listener = UdsListener::bind(&path, TransportConfig::uds()).unwrap();
            listener.leak_socket_file();
        }
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn routes_and_datagrams_work_over_a_socket() {
        let path = socket_path("routes");
        let config = TransportConfig::uds();
        let listener = UdsListener::bind(&path, config.clone()).unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, false);
        let server = tokio::spawn(async move {
            let outcome = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(9)))
                .await;
            (outcome, listener)
        });

        let (node, _channels) = connect(&path, &config, &params(&config, Role::Node))
            .await
            .unwrap();
        let ((daemon, mut daemon_channels), _listener) = {
            let (outcome, listener) = server.await.unwrap();
            (outcome.unwrap(), listener)
        };

        let route = node.open_route(b"camera").unwrap();
        let mut accepted =
            tokio::time::timeout(Duration::from_secs(5), daemon_channels.accepts.accept())
                .await
                .unwrap()
                .expect("an inbound route");
        route
            .sender()
            .send(FrameKind::Data, &[0xab; 4_096])
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), accepted.receiver_mut().recv())
            .await
            .unwrap()
            .expect("a route frame");
        assert_eq!(frame.payload().len(), 4_096);

        node.datagrams().send(FrameKind::Data, b"pose").unwrap();
        let datagram =
            tokio::time::timeout(Duration::from_secs(5), daemon_channels.datagrams.recv())
                .await
                .unwrap()
                .expect("a datagram");
        assert_eq!(datagram.payload(), b"pose");

        node.close(CloseReason::local("done")).await.unwrap();
        assert!(daemon.stats().connection.frames_received > 0);
    }
}
