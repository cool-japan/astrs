//! The UDP seam: one trait, one tokio implementation, one capability probe.
//!
//! Everything above this module talks to the network through
//! [`DatagramSocket`]. That is not abstraction for its own sake — it is what
//! makes the reliability protocol testable. A reliable writer is only
//! interesting when datagrams go missing, and the honest way to lose a
//! datagram is to wrap the socket rather than to reach inside the state
//! machine and pretend. A test defines its own `DatagramSocket` that drops
//! whatever it likes; the participant above cannot tell the difference.
//!
//! # Why the futures are boxed
//!
//! `async fn` in a trait is not `dyn`-compatible, and a participant needs to
//! hold `Arc<dyn DatagramSocket>` — the whole point is that it does not know
//! which socket it has. So the two asynchronous methods return
//! [`SocketFuture`], a pinned boxed future. One allocation per datagram, in
//! exchange for a seam that costs nothing to reason about.
//!
//! # Multicast is a capability, not an assumption
//!
//! SPDP's default rendezvous is the multicast group `239.255.0.1`, and a
//! sandboxed host — macOS under a test harness is the case that matters here
//! — will refuse `IP_ADD_MEMBERSHIP` outright. This crate treats that as a
//! *capability question with an answer*, never as a reason to skip a test:
//! [`probe_multicast`] returns what the kernel said, the participant carries
//! the answer in [`MulticastCapability`], and every deterministic protocol
//! assertion runs over the unicast initial-peers path regardless.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::behavior::transport::{DatagramSocket, UdpTransport, probe_multicast};
//! use astrs_rtps::structure::port::DEFAULT_MULTICAST_GROUP;
//! use std::net::Ipv4Addr;
//!
//! let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
//! runtime.block_on(async {
//!     let socket = UdpTransport::bind_loopback().await?;
//!
//!     // Port 0 was requested; the locator a participant announces must carry
//!     // what the kernel actually chose, never the port that was asked for.
//!     if let Ok(bound) = socket.local_addr() {
//!         assert_ne!(bound.port(), 0);
//!     }
//!
//!     // Multicast is a question with an answer, never an assumption.
//!     let capability =
//!         probe_multicast(&socket, DEFAULT_MULTICAST_GROUP, Ipv4Addr::UNSPECIFIED)?;
//!     assert!(capability.is_joined() || capability.is_refused());
//!     Ok::<(), astrs_rtps::behavior::BehaviorError>(())
//! })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::behavior::error::{BehaviorError, BehaviorResult, IoFailure};
use crate::messages::{MAX_UDP_PAYLOAD, Message};
use crate::structure::Locator;
use crate::structure::port::DEFAULT_MULTICAST_GROUP;

/// The return type of [`DatagramSocket`]'s asynchronous methods.
///
/// A pinned, boxed, `Send` future — the shape that keeps the trait
/// `dyn`-compatible. See the [module documentation](self) for why.
pub type SocketFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// The largest datagram this transport will send or accept.
///
/// The IPv4 UDP payload ceiling, re-exported from the message model so the
/// behavior half and the wire half cannot drift apart.
pub const MAX_DATAGRAM_LEN: usize = MAX_UDP_PAYLOAD;

/// A UDP socket, as the RTPS behavior half needs it.
///
/// Four operations, three of which every socket has and one — multicast
/// membership — that a socket is allowed to refuse. Implementors are `Send +
/// Sync` because a participant shares one socket between its receive loop and
/// every writer that sends on it.
///
/// # Implementing a test double
///
/// The only required methods are the three a plain socket always supports.
/// [`join_multicast_v4`](DatagramSocket::join_multicast_v4) and
/// [`leave_multicast_v4`](DatagramSocket::leave_multicast_v4) default to
/// [`io::ErrorKind::Unsupported`], which is the truthful answer for a socket
/// that has no such thing.
pub trait DatagramSocket: fmt::Debug + Send + Sync {
    /// Send `datagram` to `target`.
    fn send_to<'a>(&'a self, datagram: &'a [u8], target: SocketAddr) -> SocketFuture<'a, usize>;

    /// Wait for a datagram, copy it into `buffer`, and report its length and
    /// sender.
    fn recv_from<'a>(&'a self, buffer: &'a mut [u8]) -> SocketFuture<'a, (usize, SocketAddr)>;

    /// The address the socket is actually bound to.
    ///
    /// After binding port 0 this is how a participant learns the port it must
    /// announce. Announcing the *computed* §9.6.1.1 port after binding an
    /// ephemeral one is the classic silent-failure mode: peers dutifully send
    /// to a port nothing is listening on.
    ///
    /// # Errors
    ///
    /// Whatever the operating system reports.
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Join an IPv4 multicast group.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::Unsupported`] by default; a real socket reports what
    /// the kernel said, which on a sandboxed host is often
    /// [`io::ErrorKind::PermissionDenied`].
    fn join_multicast_v4(&self, group: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        let _ = (group, interface);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this socket does not implement multicast membership",
        ))
    }

    /// Leave an IPv4 multicast group.
    ///
    /// # Errors
    ///
    /// As [`join_multicast_v4`](DatagramSocket::join_multicast_v4).
    fn leave_multicast_v4(&self, group: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        let _ = (group, interface);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this socket does not implement multicast membership",
        ))
    }
}

/// A tokio UDP socket.
///
/// The production implementation of [`DatagramSocket`], and the only place in
/// this crate that touches `tokio::net`.
#[derive(Debug)]
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Bind a socket to `address`.
    ///
    /// Pass port 0 to let the operating system choose — then read the choice
    /// back with [`local_addr`](DatagramSocket::local_addr) and announce
    /// *that*.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Bind`] when the address is unavailable.
    pub async fn bind(address: SocketAddr) -> BehaviorResult<Self> {
        let socket = UdpSocket::bind(address)
            .await
            .map_err(|error| BehaviorError::Bind {
                address,
                source: IoFailure::new(&error),
            })?;
        Ok(Self { socket })
    }

    /// Bind a socket on the loopback interface with an ephemeral port.
    ///
    /// The deterministic test path: no fixed port to collide with a
    /// concurrently running test, no traffic that leaves the host.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Bind`].
    pub async fn bind_loopback() -> BehaviorResult<Self> {
        Self::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await
    }

    /// Bind a socket on every interface at `port`.
    ///
    /// What a participant does for its multicast reception socket, and for
    /// the §9.6.1.1 fixed-port unicast sockets when the configuration asks
    /// for real-world addressing.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Bind`].
    pub async fn bind_any(port: u16) -> BehaviorResult<Self> {
        Self::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)).await
    }

    /// Ask the kernel to loop multicast sends back to this host.
    ///
    /// Two participants in one process only see each other's multicast
    /// traffic when this is on, which is the interesting case for a
    /// single-host test.
    ///
    /// # Errors
    ///
    /// Whatever `setsockopt` reports.
    pub fn set_multicast_loop(&self, enabled: bool) -> io::Result<()> {
        self.socket.set_multicast_loop_v4(enabled)
    }

    /// Set the time-to-live on multicast sends.
    ///
    /// One — the default — keeps SPDP announcements on the local link.
    ///
    /// # Errors
    ///
    /// Whatever `setsockopt` reports.
    pub fn set_multicast_ttl(&self, ttl: u32) -> io::Result<()> {
        self.socket.set_multicast_ttl_v4(ttl)
    }

    /// The underlying tokio socket.
    #[must_use]
    pub const fn as_socket(&self) -> &UdpSocket {
        &self.socket
    }
}

impl DatagramSocket for UdpTransport {
    fn send_to<'a>(&'a self, datagram: &'a [u8], target: SocketAddr) -> SocketFuture<'a, usize> {
        Box::pin(self.socket.send_to(datagram, target))
    }

    fn recv_from<'a>(&'a self, buffer: &'a mut [u8]) -> SocketFuture<'a, (usize, SocketAddr)> {
        Box::pin(self.socket.recv_from(buffer))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn join_multicast_v4(&self, group: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        self.socket.join_multicast_v4(group, interface)
    }

    fn leave_multicast_v4(&self, group: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        self.socket.leave_multicast_v4(group, interface)
    }
}

/// What the kernel said when a multicast join was attempted.
///
/// Never a boolean. A test that asserts "multicast works or is denied" must be
/// able to say *which*, and to print the reason when it is neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MulticastCapability {
    /// The join succeeded; the socket is a member of the group.
    Joined {
        /// The group that was joined.
        group: Ipv4Addr,
    },
    /// The kernel refused on policy grounds — a sandbox, a missing route, an
    /// interface without multicast. Discovery falls back to unicast initial
    /// peers, which is the deterministic path anyway.
    Refused {
        /// The group that was requested.
        group: Ipv4Addr,
        /// Exactly what the operating system said.
        reason: IoFailure,
    },
    /// The participant was configured with multicast off; no join was tried.
    Disabled,
}

impl MulticastCapability {
    /// True only when the socket actually joined.
    #[must_use]
    pub const fn is_joined(&self) -> bool {
        matches!(self, Self::Joined { .. })
    }

    /// True when a join was attempted and the kernel said no.
    #[must_use]
    pub const fn is_refused(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }

    /// The group involved, when there was one.
    #[must_use]
    pub const fn group(&self) -> Option<Ipv4Addr> {
        match self {
            Self::Joined { group } | Self::Refused { group, .. } => Some(*group),
            Self::Disabled => None,
        }
    }

    /// The refusal, as a [`BehaviorError`], for a caller that wants to
    /// propagate rather than adapt.
    #[must_use]
    pub fn as_error(&self) -> Option<BehaviorError> {
        match self {
            Self::Refused { group, reason } => Some(BehaviorError::MulticastJoin {
                group: *group,
                interface: Ipv4Addr::UNSPECIFIED,
                source: reason.clone(),
            }),
            _ => None,
        }
    }
}

impl fmt::Display for MulticastCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Joined { group } => write!(formatter, "joined {group}"),
            Self::Refused { group, reason } => write!(formatter, "refused {group}: {reason}"),
            Self::Disabled => formatter.write_str("disabled by configuration"),
        }
    }
}

/// Attempt a multicast join on `socket` and report the outcome.
///
/// Never returns `Err` for a refusal — a refusal *is* the answer. It returns
/// `Err` only when the socket cannot report its own address, which would mean
/// the socket is broken rather than the capability missing.
///
/// # Errors
///
/// [`BehaviorError::Receive`] when `local_addr` fails.
pub fn probe_multicast(
    socket: &dyn DatagramSocket,
    group: Ipv4Addr,
    interface: Ipv4Addr,
) -> BehaviorResult<MulticastCapability> {
    let local = socket
        .local_addr()
        .map_err(|error| BehaviorError::Receive {
            local: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            source: IoFailure::new(&error),
        })?;
    let _ = local;
    match socket.join_multicast_v4(group, interface) {
        Ok(()) => Ok(MulticastCapability::Joined { group }),
        Err(error) => Ok(MulticastCapability::Refused {
            group,
            reason: IoFailure::new(&error),
        }),
    }
}

/// Bind an ephemeral socket and probe the SPDP group on it.
///
/// The one-call form a test uses to answer "can this host do multicast at
/// all?" without disturbing a participant.
///
/// # Errors
///
/// [`BehaviorError::Bind`] when even the ephemeral bind fails.
pub async fn probe_default_multicast() -> BehaviorResult<MulticastCapability> {
    let socket = UdpTransport::bind_any(0).await?;
    probe_multicast(&socket, DEFAULT_MULTICAST_GROUP, Ipv4Addr::UNSPECIFIED)
}

/// A socket a participant sends and receives RTPS messages on.
///
/// Wraps an [`Arc<dyn DatagramSocket>`](DatagramSocket) with the two
/// operations the protocol actually performs — put a [`Message`] on the wire,
/// take one off — plus the address bookkeeping every announcement depends on.
///
/// The bound address is captured once at construction, because the whole
/// point of binding port 0 is that the *bound* address, not the requested
/// one, is what peers must be told about.
#[derive(Clone)]
pub struct RtpsSocket {
    socket: Arc<dyn DatagramSocket>,
    bound: SocketAddr,
    multicast: MulticastCapability,
}

impl fmt::Debug for RtpsSocket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RtpsSocket")
            .field("bound", &self.bound)
            .field("multicast", &self.multicast)
            .finish_non_exhaustive()
    }
}

impl RtpsSocket {
    /// Wrap `socket`, reading its bound address back from the kernel.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Receive`] when the socket cannot report its address.
    pub fn new(socket: Arc<dyn DatagramSocket>) -> BehaviorResult<Self> {
        let bound = socket
            .local_addr()
            .map_err(|error| BehaviorError::Receive {
                local: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                source: IoFailure::new(&error),
            })?;
        Ok(Self {
            socket,
            bound,
            multicast: MulticastCapability::Disabled,
        })
    }

    /// Wrap `socket` and attempt to join `group`.
    ///
    /// The join outcome is recorded, not raised: see
    /// [`MulticastCapability`].
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Receive`] when the socket cannot report its address.
    pub fn with_multicast(
        socket: Arc<dyn DatagramSocket>,
        group: Ipv4Addr,
        interface: Ipv4Addr,
    ) -> BehaviorResult<Self> {
        let mut wrapped = Self::new(socket)?;
        wrapped.multicast = probe_multicast(wrapped.socket.as_ref(), group, interface)?;
        Ok(wrapped)
    }

    /// The address the socket is bound to — the one to announce.
    #[must_use]
    pub const fn bound(&self) -> SocketAddr {
        self.bound
    }

    /// The port the socket is bound to.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.bound.port()
    }

    /// What happened when this socket tried to join a multicast group.
    #[must_use]
    pub const fn multicast(&self) -> &MulticastCapability {
        &self.multicast
    }

    /// The wrapped socket, for a caller that needs the raw operations.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn DatagramSocket> {
        &self.socket
    }

    /// A locator naming this socket, with the address forced to `address`.
    ///
    /// A socket bound to `0.0.0.0` has no address a peer can use, so the
    /// participant substitutes the interface address it wants to be reached
    /// on — loopback for a single-host test.
    #[must_use]
    pub fn locator_via(&self, address: Ipv4Addr) -> Locator {
        Locator::udpv4(address, self.bound.port())
    }

    /// A locator naming this socket as it is bound.
    ///
    /// Falls back to loopback when the socket is bound to the unspecified
    /// address, because `0.0.0.0:p` is not something a peer can send to.
    #[must_use]
    pub fn locator(&self) -> Locator {
        match self.bound.ip() {
            IpAddr::V4(address) if address.is_unspecified() => {
                Locator::udpv4(Ipv4Addr::LOCALHOST, self.bound.port())
            }
            IpAddr::V4(address) => Locator::udpv4(address, self.bound.port()),
            IpAddr::V6(address) => Locator::udpv6(address, self.bound.port()),
        }
    }

    /// Encode `message` and send it to `target`.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`] when the message will not encode,
    /// [`BehaviorError::DatagramTooLarge`] when it is bigger than UDP allows,
    /// and [`BehaviorError::Send`] when the operating system rejects it.
    pub async fn send_message(
        &self,
        message: &Message<'_>,
        target: SocketAddr,
    ) -> BehaviorResult<usize> {
        let datagram = message.encode()?;
        self.send_datagram(&datagram, target).await
    }

    /// Send an already-encoded datagram to `target`.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::DatagramTooLarge`] or [`BehaviorError::Send`].
    pub async fn send_datagram(
        &self,
        datagram: &[u8],
        target: SocketAddr,
    ) -> BehaviorResult<usize> {
        if datagram.len() > MAX_DATAGRAM_LEN {
            return Err(BehaviorError::DatagramTooLarge {
                len: datagram.len(),
                limit: MAX_DATAGRAM_LEN,
            });
        }
        self.socket
            .send_to(datagram, target)
            .await
            .map_err(|error| BehaviorError::Send {
                target,
                len: datagram.len(),
                source: IoFailure::new(&error),
            })
    }

    /// Send an encoded datagram to every locator in `targets`.
    ///
    /// Locators that name an unusable transport are skipped rather than
    /// fatal: a peer may legitimately announce a UDPv6 locator this
    /// participant cannot reach alongside one it can. The count of successful
    /// sends is returned so a caller can tell "nobody was reachable" from
    /// "everybody was".
    ///
    /// # Errors
    ///
    /// The first [`BehaviorError::Send`] from a locator that *was* usable.
    pub async fn send_datagram_to_locators(
        &self,
        datagram: &[u8],
        targets: &[Locator],
    ) -> BehaviorResult<usize> {
        let mut sent = 0_usize;
        for locator in targets {
            let Ok(address) = locator.socket_addr() else {
                continue;
            };
            self.send_datagram(datagram, address).await?;
            sent = sent.saturating_add(1);
        }
        Ok(sent)
    }

    /// Wait for one datagram and copy it into `buffer`.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Receive`].
    pub async fn recv(&self, buffer: &mut [u8]) -> BehaviorResult<(usize, SocketAddr)> {
        self.socket
            .recv_from(buffer)
            .await
            .map_err(|error| BehaviorError::Receive {
                local: self.bound,
                source: IoFailure::new(&error),
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::Mutex;

    /// A socket that records what it was asked to send and never receives.
    #[derive(Debug, Default)]
    struct Recorder {
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    impl DatagramSocket for Recorder {
        fn send_to<'a>(
            &'a self,
            datagram: &'a [u8],
            target: SocketAddr,
        ) -> SocketFuture<'a, usize> {
            let octets = datagram.to_vec();
            Box::pin(async move {
                let len = octets.len();
                match self.sent.lock() {
                    Ok(mut log) => log.push((octets, target)),
                    Err(_) => return Err(io::Error::other("recorder poisoned")),
                }
                Ok(len)
            })
        }

        fn recv_from<'a>(&'a self, _buffer: &'a mut [u8]) -> SocketFuture<'a, (usize, SocketAddr)> {
            Box::pin(async { Err(io::Error::from(io::ErrorKind::WouldBlock)) })
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7411))
        }
    }

    fn any_address(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn default_multicast_methods_report_unsupported() {
        let recorder = Recorder::default();
        let error = recorder
            .join_multicast_v4(DEFAULT_MULTICAST_GROUP, Ipv4Addr::UNSPECIFIED)
            .expect_err("the default implementation must refuse");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn probe_reports_refusal_rather_than_failing() {
        let recorder = Recorder::default();
        let capability = probe_multicast(&recorder, DEFAULT_MULTICAST_GROUP, Ipv4Addr::UNSPECIFIED)
            .expect("probing must not fail");
        assert!(capability.is_refused());
        assert!(!capability.is_joined());
        assert_eq!(capability.group(), Some(DEFAULT_MULTICAST_GROUP));
        assert!(capability.as_error().is_some());
    }

    #[test]
    fn disabled_capability_has_no_group_and_no_error() {
        let capability = MulticastCapability::Disabled;
        assert_eq!(capability.group(), None);
        assert!(capability.as_error().is_none());
        assert_eq!(capability.to_string(), "disabled by configuration");
    }

    #[tokio::test]
    async fn rtps_socket_reads_its_bound_port_back() {
        let socket = RtpsSocket::new(Arc::new(Recorder::default())).expect("wrap");
        assert_eq!(socket.port(), 7411);
        assert_eq!(socket.locator(), Locator::udpv4(Ipv4Addr::LOCALHOST, 7411));
    }

    #[tokio::test]
    async fn oversized_datagrams_are_rejected_before_the_syscall() {
        let socket = RtpsSocket::new(Arc::new(Recorder::default())).expect("wrap");
        let datagram = vec![0_u8; MAX_DATAGRAM_LEN + 1];
        let error = socket
            .send_datagram(&datagram, any_address(1))
            .await
            .expect_err("must refuse");
        assert!(matches!(error, BehaviorError::DatagramTooLarge { .. }));
    }

    #[tokio::test]
    async fn unusable_locators_are_skipped_not_fatal() {
        let socket = RtpsSocket::new(Arc::new(Recorder::default())).expect("wrap");
        let targets = [
            Locator::INVALID,
            Locator::udpv4(Ipv4Addr::LOCALHOST, 7400),
            Locator::from_raw(99, 7400, [0; 16]),
        ];
        let sent = socket
            .send_datagram_to_locators(b"RTPS", &targets)
            .await
            .expect("send");
        assert_eq!(sent, 1, "only the UDPv4 locator is addressable");
    }

    #[tokio::test]
    async fn a_real_socket_binds_an_ephemeral_port_and_reports_it() {
        let transport = UdpTransport::bind_loopback().await.expect("bind");
        let bound = transport.local_addr().expect("local_addr");
        assert_ne!(bound.port(), 0, "the kernel must have chosen a port");
        assert!(bound.ip().is_loopback());
    }

    #[tokio::test]
    async fn a_real_socket_round_trips_a_datagram() {
        let left = UdpTransport::bind_loopback().await.expect("bind left");
        let right = UdpTransport::bind_loopback().await.expect("bind right");
        let right_address = right.local_addr().expect("addr");

        let sent = left
            .send_to(b"RTPS\x02\x03AS", right_address)
            .await
            .expect("send");
        assert_eq!(sent, 8);

        let mut buffer = [0_u8; 64];
        let (len, from) = right.recv_from(&mut buffer).await.expect("recv");
        assert_eq!(&buffer[..len], b"RTPS\x02\x03AS");
        assert_eq!(from, left.local_addr().expect("addr"));
    }

    #[tokio::test]
    async fn probing_the_default_group_answers_one_way_or_the_other() {
        let capability = probe_default_multicast().await.expect("probe");
        assert!(
            capability.is_joined() || capability.is_refused(),
            "a probe must produce an answer, got {capability}"
        );
    }
}
