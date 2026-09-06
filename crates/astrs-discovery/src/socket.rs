//! [`DiscoverySocket`] — the async UDP transport seam behind every beacon
//! send and receive.
//!
//! Both [`crate::sender::BeaconSender`] and [`crate::watcher::BeaconWatcher`]
//! are generic over `S: DiscoverySocket` rather than hard-wiring
//! `tokio::net::UdpSocket`, so their loop logic (jittered retry, per-peer
//! dedup, timeout sweeps) can be exercised with a fake, in-process
//! implementation of this trait — no real socket, no real network stack —
//! while [`TokioSocket`] remains the one production implementation.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::error::{DiscoveryError, DiscoveryResult};

/// An async UDP-shaped transport: send a datagram, receive a datagram.
///
/// Implementations are expected to be cheap to clone-by-reference (used
/// through `&self`, never `&mut self`) so one socket can be shared between
/// a sender and a watcher task via [`std::sync::Arc`] — see the blanket
/// [`DiscoverySocket`] impl for `Arc<S>` below, which is exactly how a real
/// daemon shares one bound-and-joined socket between both roles instead of
/// contending for the same well-known port twice.
pub trait DiscoverySocket: Send + Sync + 'static {
    /// Sends `buf` as one datagram to `target`, returning the number of
    /// bytes sent.
    fn send_to<'a>(
        &'a self,
        buf: &'a [u8],
        target: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send + 'a;

    /// Receives one datagram into `buf`, returning its length and the
    /// address it arrived from.
    ///
    /// A datagram larger than `buf` is truncated by the OS with no error
    /// indication (ordinary UDP `recvfrom` semantics) — callers size `buf`
    /// generously (see [`crate::defaults::RECV_BUFFER_LEN`]) and treat a
    /// return length at or above that size as suspicious rather than
    /// trusting it decodes correctly.
    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send + 'a;

    /// The local address this socket is bound to.
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

impl<S: DiscoverySocket + ?Sized> DiscoverySocket for Arc<S> {
    fn send_to<'a>(
        &'a self,
        buf: &'a [u8],
        target: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send + 'a {
        (**self).send_to(buf, target)
    }

    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send + 'a {
        (**self).recv_from(buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        (**self).local_addr()
    }
}

/// Whether a [`DiscoverySocket`] successfully joined its multicast group.
///
/// Binding the local UDP port is a purely local operation that either
/// succeeds or is a hard [`DiscoveryError::SocketBind`] (there is no
/// meaningful degraded fallback for "the requested address could not be
/// bound at all" — see [`bind`]'s docs). Joining a multicast group,
/// however, additionally requires kernel/network multicast support that is
/// routinely absent in sandboxes, containers, and CI runners; failing to
/// join degrades to unicast-only operation instead of treating the whole
/// socket as unusable.
///
/// # Examples
///
/// ```
/// use astrs_discovery::socket::MulticastStatus;
///
/// assert!(MulticastStatus::Joined.is_joined());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MulticastStatus {
    /// The socket joined its configured multicast group; both sending and
    /// receiving multicast beacons work normally.
    Joined,
    /// The multicast group could not be joined (or, for a sender, a send to
    /// the group address itself failed). The socket remains fully usable
    /// for unicast send/receive — including the sender's own unicast
    /// fallback list and [`crate::peer_book::PeerBook`]-directed traffic.
    Degraded {
        /// Why multicast is unavailable.
        reason: DegradedReason,
    },
}

impl MulticastStatus {
    /// Shorthand for `matches!(self, MulticastStatus::Joined)`.
    #[must_use]
    pub const fn is_joined(&self) -> bool {
        matches!(self, Self::Joined)
    }
}

/// Why a [`DiscoverySocket`] is running in [`MulticastStatus::Degraded`]
/// mode.
///
/// Carries a rendered message (rather than the original [`std::io::Error`])
/// so this type stays [`Clone`] + [`PartialEq`] for tests and status
/// snapshots; the original error is logged via `tracing` at the point of
/// failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DegradedReason {
    /// `join_multicast_v4` failed on an otherwise successfully bound
    /// socket.
    JoinFailed(Arc<str>),
    /// A send to the multicast group address itself failed (distinct from
    /// a unicast send failing, which is reported per-call, not as a status
    /// change).
    SendFailed(Arc<str>),
}

impl fmt::Display for DegradedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JoinFailed(reason) => write!(f, "multicast join failed: {reason}"),
            Self::SendFailed(reason) => write!(f, "multicast send failed: {reason}"),
        }
    }
}

/// The production [`DiscoverySocket`]: a bound `tokio::net::UdpSocket`.
pub struct TokioSocket(UdpSocket);

impl fmt::Debug for TokioSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokioSocket")
            .field("local_addr", &self.0.local_addr().ok())
            .finish()
    }
}

impl DiscoverySocket for TokioSocket {
    // Written as an explicit `-> impl Future<..> + Send + 'a` rather than
    // plain `async fn` on purpose: `async fn` in a trait impl does not by
    // itself give the returned opaque future a `Send` bound, and generic
    // callers (`BeaconWatcher<S>::spawn`/`BeaconSender<S>::spawn`) need
    // `tokio::spawn` to see one without knowing the concrete `S`.
    #[allow(clippy::manual_async_fn)]
    fn send_to<'a>(
        &'a self,
        buf: &'a [u8],
        target: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send + 'a {
        async move { self.0.send_to(buf, target).await }
    }

    #[allow(clippy::manual_async_fn)]
    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send + 'a {
        async move { self.0.recv_from(buf).await }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

/// Binds the discovery socket and attempts to join `group` on the default
/// interface.
///
/// `bind_addr` is typically `0.0.0.0:<`[`crate::defaults::DEFAULT_MULTICAST_PORT`]`>`
/// — receiving a multicast datagram requires the listening socket's *own*
/// bound port to match the packet's destination port, so a socket that
/// also wants to receive beacons cannot use an ephemeral port here. A
/// send-only caller that will never call [`DiscoverySocket::recv_from`] may
/// still bind an ephemeral port (`bind_addr`'s port `0`); multicast *sending*
/// does not require group membership at all, only receiving does.
///
/// # Errors
///
/// [`DiscoveryError::SocketBind`] if `bind_addr` itself could not be
/// bound — a genuine misconfiguration (port already in use by another
/// process, insufficient privilege, an address not owned by any local
/// interface) with no reasonable silent fallback.
///
/// A failure to join the multicast group is **not** an error: it is
/// reported as `Ok((socket, MulticastStatus::Degraded { .. }))`, and is
/// logged via `tracing::warn!` at the point of failure.
pub async fn bind(
    bind_addr: SocketAddr,
    group: Ipv4Addr,
) -> DiscoveryResult<(TokioSocket, MulticastStatus)> {
    let socket = UdpSocket::bind(bind_addr)
        .await
        .map_err(|source| DiscoveryError::SocketBind {
            addr: bind_addr,
            source,
        })?;

    // Best-effort: local single-host clusters (dev, `astrs run`, this
    // crate's own loopback tests) still want to see their own beacon loop
    // back. Most platforms already default `IP_MULTICAST_LOOP` to on; a
    // failure here is not itself grounds for `Degraded`.
    if let Err(err) = socket.set_multicast_loop_v4(true) {
        tracing::debug!(error = %err, "set_multicast_loop_v4 failed; continuing with the platform default");
    }

    let interface = match bind_addr.ip() {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
    };

    let status = match socket.join_multicast_v4(group, interface) {
        Ok(()) => MulticastStatus::Joined,
        Err(err) => {
            tracing::warn!(
                error = %err,
                %group,
                %bind_addr,
                "multicast join failed; continuing in unicast-only degraded mode"
            );
            MulticastStatus::Degraded {
                reason: DegradedReason::JoinFailed(Arc::from(err.to_string())),
            }
        }
    };

    Ok((TokioSocket(socket), status))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binding_an_ephemeral_loopback_port_succeeds() {
        let (socket, _status) = bind(
            "127.0.0.1:0".parse().unwrap(),
            Ipv4Addr::new(239, 255, 74, 7),
        )
        .await
        .unwrap();
        let addr = socket.local_addr().unwrap();
        assert_eq!(addr.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0, "the OS must have assigned a real port");
    }

    #[tokio::test]
    async fn two_sockets_can_send_and_receive_unicast_loopback() {
        let (a, _) = bind(
            "127.0.0.1:0".parse().unwrap(),
            Ipv4Addr::new(239, 255, 74, 7),
        )
        .await
        .unwrap();
        let (b, _) = bind(
            "127.0.0.1:0".parse().unwrap(),
            Ipv4Addr::new(239, 255, 74, 7),
        )
        .await
        .unwrap();
        let b_addr = b.local_addr().unwrap();

        a.send_to(b"hello", b_addr).await.unwrap();
        let mut buf = [0u8; 16];
        let (len, from) = b.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"hello");
        assert_eq!(from, a.local_addr().unwrap());
    }

    #[tokio::test]
    async fn binding_the_same_fixed_port_twice_is_a_hard_bind_error() {
        let (first, _) = bind(
            "127.0.0.1:0".parse().unwrap(),
            Ipv4Addr::new(239, 255, 74, 7),
        )
        .await
        .unwrap();
        let taken_port = first.local_addr().unwrap().port();

        let err = bind(
            format!("127.0.0.1:{taken_port}").parse().unwrap(),
            Ipv4Addr::new(239, 255, 74, 7),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DiscoveryError::SocketBind { .. }));
    }

    #[test]
    fn multicast_status_is_joined_helper() {
        assert!(MulticastStatus::Joined.is_joined());
        assert!(
            !MulticastStatus::Degraded {
                reason: DegradedReason::JoinFailed(Arc::from("nope"))
            }
            .is_joined()
        );
    }

    #[test]
    fn degraded_reason_display_mentions_the_cause() {
        let reason = DegradedReason::JoinFailed(Arc::from("no multicast route"));
        assert!(reason.to_string().contains("no multicast route"));
        let reason = DegradedReason::SendFailed(Arc::from("network unreachable"));
        assert!(reason.to_string().contains("network unreachable"));
    }

    /// An `Arc<TokioSocket>` must itself satisfy `DiscoverySocket`, so one
    /// bound-and-joined socket can be shared between a sender task and a
    /// watcher task without either needing ownership.
    #[tokio::test]
    async fn arc_wrapped_socket_implements_discovery_socket() {
        let (socket, _status) = bind(
            "127.0.0.1:0".parse().unwrap(),
            Ipv4Addr::new(239, 255, 74, 7),
        )
        .await
        .unwrap();
        let shared: Arc<TokioSocket> = Arc::new(socket);
        let sender_handle = Arc::clone(&shared);
        let watcher_handle = Arc::clone(&shared);

        let watcher_addr = watcher_handle.local_addr().unwrap();
        sender_handle.send_to(b"ping", watcher_addr).await.unwrap();
        let mut buf = [0u8; 8];
        let (len, _from) = watcher_handle.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"ping");
    }
}
