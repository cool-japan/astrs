//! Shared fixtures for the loopback self-interop tests.
//!
//! Everything here builds on one decision: **the deterministic path is
//! unicast, over explicit initial peers, on ephemeral loopback ports.**
//! A sandboxed macOS host refuses `IP_ADD_MEMBERSHIP`, so a test that needs
//! multicast is a test that reports nothing useful on a developer's machine.
//! Multicast is implemented and probed — [`multicast_probe`] answers the
//! capability question loudly — but no protocol assertion depends on it.
//!
//! Three rules every test in this directory follows:
//!
//! 1. **Every socket binds port 0.** The kernel picks; the announcement
//!    carries what the kernel picked. Two tests running in parallel under
//!    `cargo nextest` cannot collide.
//! 2. **No sleep is ever used to wait for a thing to happen.** Waiting for
//!    something is [`await_matched`] / [`ReaderHandle::take_within`], which
//!    park on a condition and are bounded by [`PATIENCE`]; a fixed sleep
//!    there would be flaky or slow, and usually both. Two sleeps do appear
//!    and are honest ones: [`await_condition`]'s one-millisecond poll
//!    interval, because a state a participant reaches internally has no
//!    channel to await, and a handful of `sleep(TICK * n)` calls in the
//!    *negative* tests, because "these two must never match" cannot be
//!    awaited — the only way to test a non-event is to give it time and then
//!    check it did not happen.
//! 3. **Loss is injected at the socket**, through [`LossySocket`], not by
//!    reaching into the state machine. The participant above cannot tell the
//!    difference between a dropped datagram and a lost one, which is the
//!    whole point.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use astrs_rtps::behavior::endpoint::TopicKey;
use astrs_rtps::behavior::transport::{
    DatagramSocket, MulticastCapability, SocketFuture, UdpTransport, probe_default_multicast,
};
use astrs_rtps::behavior::{Participant, ParticipantConfig, ReaderHandle, WriterHandle};
use astrs_rtps::discovery::{ReaderQos, RosCompat, SpdpConfig, WriterQos};
use astrs_rtps::structure::{GuidPrefix, Locator, VendorId};

/// How long any wait may take before the test fails.
///
/// Generous on purpose: this bounds a *failure*, not a success. A working
/// exchange over loopback completes in single-digit milliseconds; five
/// seconds is what a loaded CI machine running twenty tests in parallel might
/// need, and a test that takes five seconds to fail is still a test that
/// fails rather than hangs.
pub const PATIENCE: Duration = Duration::from_secs(5);

/// The tick period the fixtures run their participants at.
///
/// Twenty milliseconds. The heartbeat cadence rides on it, and repair latency
/// is one heartbeat, so this is what bounds how fast a lost sample comes
/// back.
pub const TICK: Duration = Duration::from_millis(20);

/// The topic every fixture uses.
pub const TOPIC: &str = "rt/chatter";

/// The type every fixture uses.
pub const TYPE_NAME: &str = "std_msgs::msg::dds_::String_";

/// The fixture's topic key.
///
/// # Panics
///
/// Never: the names are constants.
#[must_use]
pub fn topic() -> TopicKey {
    TopicKey::new(TOPIC, TYPE_NAME).expect("the fixture's names are valid")
}

/// A distinct GUID prefix, vendor-scoped as §9.3.1.5 requires.
#[must_use]
pub fn prefix(seed: u8) -> GuidPrefix {
    GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
}

/// A CDR-shaped payload: an encapsulation header and four octets, so the
/// submessage body stays four-octet aligned.
#[must_use]
pub fn payload(tag: u8) -> Vec<u8> {
    vec![0x00, 0x01, 0x00, 0x00, tag, tag, tag, tag]
}

/// A payload of `len` octets with a recognisable pattern.
#[must_use]
pub fn large_payload(len: usize) -> Vec<u8> {
    let mut octets = vec![0x00, 0x01, 0x00, 0x00];
    octets.extend((0..len.saturating_sub(4)).map(|index| (index % 251) as u8));
    octets
}

/// The configuration a fixture participant is built from.
///
/// Multicast off, one initial peer, ephemeral loopback ports.
///
/// # Panics
///
/// When domain 0 is somehow unusable, which cannot happen.
#[must_use]
pub fn config(seed: u8, peer: Option<Locator>, compat: RosCompat) -> ParticipantConfig {
    let mut spdp = SpdpConfig::new(0, 0, prefix(seed))
        .expect("domain 0 is usable")
        .with_multicast(false)
        .with_compat(compat)
        .with_announce_period(TICK);
    if let Some(peer) = peer {
        spdp = spdp.with_initial_peer(peer);
    }
    ParticipantConfig::from_spdp(spdp)
        .with_heartbeat_period(TICK)
        .with_tick_period(TICK)
}

/// Two participants, each other's initial peer, both running.
///
/// The order matters and is the reason this is a fixture rather than two
/// calls: the first participant must exist before the second can be told
/// where to find it, and the first must then be told about the second, which
/// only its announcement can do. Giving the second the first's locator is
/// enough — the first learns the second's from the announcement it receives,
/// and re-announces to it from then on.
pub struct Pair {
    /// The participant created first.
    pub left: Participant,
    /// The participant created second, holding `left` as its initial peer.
    pub right: Participant,
    tasks: Vec<tokio::task::JoinHandle<astrs_rtps::behavior::BehaviorResult<()>>>,
}

impl Pair {
    /// Build and start a pair of participants on the Jazzy conventions.
    ///
    /// # Panics
    ///
    /// When a socket will not bind.
    pub async fn new() -> Self {
        Self::with_compat(RosCompat::Jazzy, RosCompat::Jazzy).await
    }

    /// Build and start a pair, each on its own ROS 2 distribution.
    ///
    /// # Panics
    ///
    /// When a socket will not bind.
    pub async fn with_compat(left_compat: RosCompat, right_compat: RosCompat) -> Self {
        let left = Participant::new(config(1, None, left_compat))
            .await
            .expect("the left participant must bind");
        let right = Participant::new(config(2, Some(left.metatraffic_locator()), right_compat))
            .await
            .expect("the right participant must bind");
        Self::start(left, right)
    }

    /// Start two participants that were built elsewhere.
    #[must_use]
    pub fn start(left: Participant, right: Participant) -> Self {
        let tasks = vec![left.spawn(TICK), right.spawn(TICK)];
        Self { left, right, tasks }
    }

    /// Wait until each participant has discovered the other.
    ///
    /// # Panics
    ///
    /// When discovery does not complete within [`PATIENCE`].
    pub async fn await_discovery(&self) {
        let left = self.left.clone();
        let right = self.right.clone();
        let left_guid = left.guid();
        let right_guid = right.guid();
        await_condition("mutual SPDP discovery", || {
            let left = left.clone();
            let right = right.clone();
            async move { left.knows(right_guid).await && right.knows(left_guid).await }
        })
        .await;
    }

    /// Stop both participants and join their tasks.
    pub async fn shutdown(self) {
        self.left.shutdown().await;
        self.right.shutdown().await;
        for task in self.tasks {
            let _ = tokio::time::timeout(PATIENCE, task).await;
        }
    }
}

/// Poll `condition` until it holds, failing the test after [`PATIENCE`].
///
/// This is the one place the fixtures busy-wait, and it is deliberate: the
/// alternative — an event channel per condition — would need a channel for
/// every predicate a test wants to express. The poll interval is one
/// millisecond, so a condition that becomes true takes a millisecond to be
/// noticed, and the `PATIENCE` bound turns a hang into a named failure.
///
/// # Panics
///
/// When `condition` has not held within [`PATIENCE`].
pub async fn await_condition<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if condition().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{what} did not happen within {PATIENCE:?}");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Wait until `writer` has matched at least one reader.
///
/// # Panics
///
/// When no match happens within [`PATIENCE`].
pub async fn await_matched(writer: &WriterHandle) {
    let writer = writer.clone();
    await_condition("endpoint matching", || {
        let writer = writer.clone();
        async move { writer.matched_readers().await > 0 }
    })
    .await;
}

/// Create one writer and one reader on the fixture topic and wait for them to
/// match.
///
/// # Panics
///
/// When either endpoint cannot be created, or the two never match.
pub async fn wire(
    pair: &Pair,
    writer_qos: WriterQos,
    reader_qos: ReaderQos,
) -> (WriterHandle, ReaderHandle) {
    let reader = pair
        .right
        .create_reader(topic(), reader_qos)
        .await
        .expect("the reader must be created");
    let writer = pair
        .left
        .create_writer(topic(), writer_qos)
        .await
        .expect("the writer must be created");
    await_matched(&writer).await;
    (writer, reader)
}

/// Ask the host whether it can join the SPDP multicast group.
///
/// Returns the answer; it is never a reason to skip a test.
///
/// # Panics
///
/// When even an ephemeral bind fails, which would mean the host has no
/// network stack at all.
pub async fn multicast_probe() -> MulticastCapability {
    probe_default_multicast()
        .await
        .expect("probing must produce an answer")
}

/// A socket that drops datagrams on demand.
///
/// The loss-injection seam. Wraps a real [`UdpTransport`] and decides, per
/// outgoing datagram, whether to hand it to the kernel or to say it was sent
/// and throw it away. The participant above cannot tell the difference — that
/// is what makes this an honest test of retransmission rather than a
/// simulation of one.
#[derive(Debug)]
pub struct LossySocket {
    inner: UdpTransport,
    drop_every: AtomicU64,
    sent: AtomicU64,
    dropped: AtomicU64,
    blackhole: AtomicBool,
}

impl LossySocket {
    /// Wrap `inner`, dropping every `drop_every`-th datagram.
    ///
    /// Zero means drop nothing.
    #[must_use]
    pub const fn new(inner: UdpTransport, drop_every: u64) -> Self {
        Self {
            inner,
            drop_every: AtomicU64::new(drop_every),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            blackhole: AtomicBool::new(false),
        }
    }

    /// Bind an ephemeral loopback socket and wrap it.
    ///
    /// # Panics
    ///
    /// When the socket will not bind.
    pub async fn bind_loopback(drop_every: u64) -> Arc<Self> {
        let inner = UdpTransport::bind_loopback()
            .await
            .expect("an ephemeral loopback socket must bind");
        Arc::new(Self::new(inner, drop_every))
    }

    /// Change the drop rate.
    pub fn set_drop_every(&self, drop_every: u64) {
        self.drop_every.store(drop_every, Ordering::Relaxed);
    }

    /// Drop everything, or stop doing so.
    pub fn set_blackhole(&self, enabled: bool) {
        self.blackhole.store(enabled, Ordering::Relaxed);
    }

    /// How many datagrams reached the kernel.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// How many datagrams were thrown away.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Whether the next datagram should be dropped.
    fn should_drop(&self) -> bool {
        if self.blackhole.load(Ordering::Relaxed) {
            return true;
        }
        let every = self.drop_every.load(Ordering::Relaxed);
        if every == 0 {
            return false;
        }
        let index = self
            .sent
            .load(Ordering::Relaxed)
            .saturating_add(self.dropped.load(Ordering::Relaxed));
        index.wrapping_add(1).is_multiple_of(every)
    }
}

impl DatagramSocket for LossySocket {
    fn send_to<'a>(&'a self, datagram: &'a [u8], target: SocketAddr) -> SocketFuture<'a, usize> {
        if self.should_drop() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            let len = datagram.len();
            return Box::pin(async move { Ok(len) });
        }
        self.sent.fetch_add(1, Ordering::Relaxed);
        Box::pin(self.inner.send_to(datagram, target))
    }

    fn recv_from<'a>(&'a self, buffer: &'a mut [u8]) -> SocketFuture<'a, (usize, SocketAddr)> {
        self.inner.recv_from(buffer)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}
