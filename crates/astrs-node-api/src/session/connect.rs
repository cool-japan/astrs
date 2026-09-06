//! Dialling the daemon (blueprint §4.2, §7.2).
//!
//! > Local node listener on **TCP 7408** (loopback) and a Unix domain socket
//! > (`$XDG_RUNTIME_DIR/astrs/daemon.sock`) — UDS preferred, TCP fallback.
//!
//! The node tries every endpoint its configuration names, in order, and keeps
//! every failure: "connection refused on the Unix socket" and "no route to
//! host on the TCP fallback" are two different diagnoses, and an error that
//! reports only the last one sends an operator to the wrong place.
//!
//! # What this module produces
//!
//! A [`NodeLink`]: a framed duplex whose halves are **boxed trait objects**,
//! so a Unix socket, a TCP socket and the testing harness's
//! `tokio::io::duplex` pair are all the same type from here on. That is what
//! lets [`crate::Node`] be one concrete type instead of being generic over
//! its transport, and it is what makes the in-process mock daemon speak the
//! *real* protocol rather than a stub of it.

use std::sync::Arc;
use std::time::Duration;

use astrs_transport::{
    ConnectionCounters, FramedDuplex, FramedReader, FramedWriter, HandshakeParams, LocalIdentity,
    TransportAddr, TransportConfig, initiate,
};
use astrs_wire::{AuthToken, FrameLimits, NegotiatedLimits, NegotiatedSession, Role};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::env::DEFAULT_DAEMON_PORT;
use crate::error::{NodeError, Result};

/// The read half of a node's daemon link, type-erased.
pub type LinkReader = FramedReader<Box<dyn AsyncRead + Send + Unpin>>;

/// The write half of a node's daemon link, type-erased.
pub type LinkWriter = FramedWriter<Box<dyn AsyncWrite + Send + Unpin>>;

/// A framed duplex over a type-erased transport.
pub type LinkDuplex =
    FramedDuplex<Box<dyn AsyncRead + Send + Unpin>, Box<dyn AsyncWrite + Send + Unpin>>;

/// The read-buffer capacity a node's link starts with.
///
/// Larger than the transport default because the daemon path carries whole
/// payloads until a route is upgraded (§6.3): 64 KiB absorbs a typical
/// pre-upgrade burst without a second read syscall per frame.
pub const LINK_BUFFER_CAPACITY: usize = 64 * 1024;

/// A live, greeted connection to the daemon.
pub struct NodeLink {
    /// The framed duplex.
    pub duplex: LinkDuplex,
    /// What the greeting agreed to (§7.2).
    pub session: NegotiatedSession,
    /// The endpoint that answered.
    pub endpoint: String,
}

impl NodeLink {
    /// Splits the link into the halves the reader and writer tasks own.
    #[must_use]
    pub fn into_halves(self) -> (LinkReader, LinkWriter, NegotiatedSession) {
        let (reader, writer) = self.duplex.into_halves();
        (reader, writer, self.session)
    }
}

impl core::fmt::Debug for NodeLink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NodeLink")
            .field("endpoint", &self.endpoint)
            .field("session", &self.session.session_id)
            .field("protocol", &self.session.protocol)
            .finish_non_exhaustive()
    }
}

/// Wraps an already-open byte stream in the node link's frame codec.
///
/// The seam the testing harness uses: hand it one end of a
/// `tokio::io::duplex` pair and the node cannot tell it from a socket.
#[must_use]
pub fn wrap_stream<S>(stream: S, limits: FrameLimits) -> LinkDuplex
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (read, write) = tokio::io::split(stream);
    let read: Box<dyn AsyncRead + Send + Unpin> = Box::new(read);
    let write: Box<dyn AsyncWrite + Send + Unpin> = Box::new(write);
    FramedDuplex::from_halves(
        read,
        write,
        limits,
        LINK_BUFFER_CAPACITY,
        ConnectionCounters::shared(),
    )
}

/// The handshake parameters a node presents (§7.2).
#[must_use]
pub fn node_handshake_params(auth: AuthToken, label: Option<String>) -> HandshakeParams {
    let identity = match label {
        Some(label) => LocalIdentity::new(Role::Node).with_label(label),
        None => LocalIdentity::new(Role::Node),
    };
    HandshakeParams::new(identity, auth).with_limits(NegotiatedLimits::uds())
}

/// Dials the first endpoint that answers, then greets it.
///
/// # Errors
///
/// [`NodeError::Connect`] listing every attempt when none answered, and
/// [`NodeError::Handshake`] when one answered but refused the greeting.
pub async fn dial(
    endpoints: &[String],
    auth: &AuthToken,
    label: Option<String>,
    timeout: Duration,
) -> Result<NodeLink> {
    if endpoints.is_empty() {
        return Err(NodeError::Connect {
            attempts: Vec::new(),
        });
    }
    let mut attempts = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        match dial_one(endpoint, auth, label.clone(), timeout).await {
            Ok(link) => return Ok(link),
            Err(NodeError::Handshake(reason)) => {
                // A refused greeting is not "try the next address": the token
                // or the protocol is wrong, and every other endpoint of the
                // same daemon will refuse identically.
                return Err(NodeError::Handshake(reason));
            }
            Err(error) => attempts.push((endpoint.clone(), error.to_string())),
        }
    }
    Err(NodeError::Connect { attempts })
}

/// Dials exactly one endpoint.
///
/// # Errors
///
/// [`NodeError::Transport`] when the address is unusable or the connect
/// fails, and [`NodeError::Handshake`] when the greeting is refused.
pub async fn dial_one(
    endpoint: &str,
    auth: &AuthToken,
    label: Option<String>,
    timeout: Duration,
) -> Result<NodeLink> {
    let addr =
        TransportAddr::parse_with_default_port(endpoint, DEFAULT_DAEMON_PORT).map_err(|error| {
            NodeError::BadEnv {
                name: "endpoint",
                value: endpoint.to_owned(),
                reason: error.to_string(),
            }
        })?;
    let config = TransportConfig::uds();
    let limits = FrameLimits::uds();

    let mut duplex = match &addr {
        TransportAddr::Uds(path) => {
            let stream = tokio::time::timeout(timeout, tokio::net::UnixStream::connect(path))
                .await
                .map_err(|_| NodeError::Timeout {
                    operation: "daemon connect",
                    millis: as_millis(timeout),
                })?
                .map_err(|error| NodeError::Transport(error.into()))?;
            wrap_stream(stream, limits)
        }
        TransportAddr::Tcp(socket) => {
            let stream = astrs_transport::backend::tcp::dial(*socket, &config).await?;
            wrap_stream(stream, FrameLimits::network())
        }
        other => {
            return Err(NodeError::BadEnv {
                name: "endpoint",
                value: endpoint.to_owned(),
                reason: format!("a node cannot dial a {} endpoint", other.scheme()),
            });
        }
    };

    let params = node_handshake_params(auth.clone(), label);
    let greeting = initiate(&mut duplex, &params, timeout)
        .await
        .map_err(|error| NodeError::Handshake(error.to_string()))?;
    Ok(NodeLink {
        duplex,
        session: greeting.session,
        endpoint: endpoint.to_owned(),
    })
}

/// Milliseconds, saturating rather than wrapping.
fn as_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The counters a freshly wrapped link starts with, exposed so a caller can
/// share one set across a reconnect.
#[must_use]
pub fn fresh_counters() -> Arc<ConnectionCounters> {
    ConnectionCounters::shared()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{Acceptor, RoleSet, SessionAssignment, SessionId};

    #[test]
    fn an_empty_endpoint_list_is_reported_as_such() {
        let runtime = crate::runtime::NodeRuntime::owned().unwrap();
        let error = runtime
            .block_on("dial", "dial", async {
                dial(&[], &AuthToken::ZERO, None, Duration::from_millis(50)).await
            })
            .unwrap()
            .unwrap_err();
        let NodeError::Connect { attempts } = error else {
            panic!("expected a connect error");
        };
        assert!(attempts.is_empty());
    }

    #[test]
    fn an_unreachable_endpoint_is_reported_with_its_reason() {
        let runtime = crate::runtime::NodeRuntime::owned().unwrap();
        let missing = std::env::temp_dir().join("astrs-node-api-no-such.sock");
        let endpoint = format!("uds://{}", missing.display());
        let error = runtime
            .block_on("dial", "dial", async {
                dial(
                    std::slice::from_ref(&endpoint),
                    &AuthToken::ZERO,
                    None,
                    Duration::from_millis(200),
                )
                .await
            })
            .unwrap()
            .unwrap_err();
        let NodeError::Connect { attempts } = error else {
            panic!("expected a connect error");
        };
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].0, endpoint);
        assert!(!attempts[0].1.is_empty());
    }

    #[test]
    fn a_malformed_endpoint_is_named() {
        let runtime = crate::runtime::NodeRuntime::owned().unwrap();
        let error = runtime
            .block_on("dial", "dial", async {
                dial_one(
                    "nonsense://x",
                    &AuthToken::ZERO,
                    None,
                    Duration::from_millis(50),
                )
                .await
            })
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, NodeError::BadEnv { .. }), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrapped_duplex_completes_the_real_greeting() {
        let token = AuthToken::from_bytes([3; 32]);
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let mut client = wrap_stream(client_io, FrameLimits::uds());
        let mut server = wrap_stream(server_io, FrameLimits::uds());

        let acceptor = Acceptor::new(token.clone()).with_accepted_roles(RoleSet::NODES);
        let server_task = tokio::spawn(async move {
            astrs_transport::accept(
                &mut server,
                &acceptor,
                SessionAssignment::Fresh(SessionId::from_u128(9)),
                Duration::from_secs(5),
            )
            .await
            .map(|accepted| accepted.session.session_id)
        });

        let params = node_handshake_params(token, Some("camera".to_owned()));
        let greeting = initiate(&mut client, &params, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(greeting.session.role, Role::Node);
        assert_eq!(
            server_task.await.unwrap().unwrap(),
            greeting.session.session_id
        );
    }

    #[test]
    fn the_link_buffer_is_sized_for_the_pre_upgrade_path() {
        assert_eq!(LINK_BUFFER_CAPACITY, 64 * 1024);
        assert_eq!(fresh_counters().snapshot().frames_sent, 0);
    }
}
