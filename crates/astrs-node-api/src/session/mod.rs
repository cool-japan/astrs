//! The daemon session: one connection, two tasks, one shared state
//! (blueprint §7.3).
//!
//! ```text
//!   Node ──► outgoing mpsc ──► writer task ──► FramedWriter ──► daemon
//!                                                                 │
//!   EventStream ◄── EventSource ◄── reader task ◄── FramedReader ◄─┘
//!                        ▲
//!                        └── RouteTable (§6.3), extension replies, HLC
//! ```
//!
//! Everything a node calls goes into the bounded `outgoing` channel and
//! returns immediately — a `send` never waits on a socket. Everything the
//! daemon says is decoded by the reader task and *dispatched*: data onto the
//! per-input queues, control onto the control lane, route changes into the
//! [`RouteTable`], extension answers to whoever asked.
//!
//! # Why the outgoing channel is bounded
//!
//! An unbounded one turns a wedged daemon into unbounded memory growth in
//! every node simultaneously — the failure mode that takes a robot down
//! rather than one process. [`SessionShared::try_send`] reports
//! [`NodeError::Backpressure`] instead, and the blocking and async senders
//! wait for room.
//!
//! # Shutdown
//!
//! [`SessionShared::close`] closes the outgoing channel, which ends the
//! writer task, which closes the socket, which ends the reader task, which
//! closes the [`EventSource`] and wakes every waiter. One direction, no
//! cross-task signalling, no ordering questions.

pub mod connect;
pub mod inputs;
pub mod pump;
pub mod routes;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use astrs_time::{HlcClock, HlcTimestamp};
use astrs_wire::common::stream::LogFrame;
use astrs_wire::{
    DataId, DataflowId, ExtensionKey, FrameLimits, LogRecord, NodeId, NodeRequest, NodeSpawnSpec,
    SessionId, SubscriptionId,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::{NodeError, Result};
use crate::events::EventSource;
use crate::runtime::NodeRuntime;

pub use connect::{LinkDuplex, LinkReader, LinkWriter, NodeLink};
pub use inputs::{InputPlaneStats, InputRouteTable, MAX_CONSECUTIVE_LAGS, RECV_SLICE};
pub use routes::{RoutePlane, RouteSlot, RouteTable, RouteUpdate};

/// How many requests may queue for the writer task before a caller waits.
///
/// Sized for a burst, not a backlog: a node that is 256 requests behind its
/// daemon has a problem the channel cannot fix.
pub const OUTGOING_CAPACITY: usize = 256;

/// How long a reply-carrying request waits by default.
pub const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// The answer an `ExtLoad` is waiting for.
pub type ExtReply = oneshot::Sender<Option<Vec<u8>>>;

/// Every outstanding `ExtLoad`, keyed by its rendered [`ExtensionKey`].
type ExtWaiters = HashMap<String, Vec<ExtReply>>;

/// One thing to write to the daemon.
#[derive(Debug)]
pub enum Outgoing {
    /// A protocol request (`kind = NodeRequest`).
    Request(Box<NodeRequest>),
    /// A log record (`kind = Log`, §7.3's "same framing").
    Log(Box<LogFrame>),
    /// End the writer once everything queued ahead of this has been flushed.
    ///
    /// A sentinel rather than an abort, because a node that closes its
    /// outputs and exits must have that `CloseOutputs` actually reach the
    /// daemon — otherwise its consumers wait for a producer that has already
    /// gone.
    Shutdown,
}

impl Outgoing {
    /// A request.
    #[must_use]
    pub fn request(request: NodeRequest) -> Self {
        Self::Request(Box::new(request))
    }

    /// A log record.
    #[must_use]
    pub fn log(subscription: SubscriptionId, record: LogRecord) -> Self {
        Self::Log(Box::new(LogFrame::new(subscription, record)))
    }
}

/// Counters a node can read off its live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SessionStats {
    /// Requests handed to the writer.
    pub requests_sent: u64,
    /// Events decoded by the reader.
    pub events_received: u64,
    /// Messages published on the daemon path.
    pub sends_inline: u64,
    /// Messages published straight into shared memory (§6.3).
    pub sends_zero_copy: u64,
    /// Times a zero-copy send fell back to the daemon path (§6.2's
    /// `shm_fallback_total`).
    pub shm_fallbacks: u64,
    /// Log records emitted over the wire.
    pub logs_sent: u64,
}

/// The state both the node and its session tasks share.
pub struct SessionShared {
    /// The dataflow this node belongs to.
    pub dataflow: DataflowId,
    /// The node's id.
    pub node: NodeId,
    /// The incarnation the daemon assigned.
    pub generation: u64,
    /// The greeting's session id (§7.2).
    pub session: SessionId,
    /// The node's effective specification.
    pub spec: Arc<NodeSpawnSpec>,
    /// The writer's inbox.
    outgoing: mpsc::Sender<Outgoing>,
    /// The event inbox the reader fills.
    pub source: Arc<EventSource>,
    /// This node's hybrid logical clock (§4.3).
    hlc: Mutex<HlcClock>,
    /// Set once the session has ended.
    closed: AtomicBool,
    /// Whoever is waiting for an `ExtValue` reply, keyed by rendered key.
    ext_waiters: Mutex<ExtWaiters>,
    /// Per-output route state (§6.3, producer side).
    pub routes: RouteTable,
    /// Per-input route state (§6.3, consumer side): which inputs read from a
    /// ring, and the reader threads behind them.
    pub inputs: InputRouteTable,
    /// The §24.2 zero-copy threshold in bytes.
    pub zero_copy_threshold: u64,
    /// The daemon's segment-broker socket, when it published one
    /// ([`astrs_wire::NodeConfig::shm_broker`]).
    ///
    /// How a `RouteUpgrade` turns into a mapping (§6.2: the daemon brokers the
    /// segment fds). `None` leaves [`crate::session::routes::open_producer`]
    /// to open the segment by name, which only a named backing supports.
    pub shm_broker: Option<std::path::PathBuf>,
    /// The negotiated frame budget.
    pub limits: FrameLimits,
    /// The runtime background work runs on.
    pub runtime: NodeRuntime,
    /// The log subscription node-emitted records carry.
    pub log_subscription: SubscriptionId,
    /// Monotone log sequence numbers.
    log_seq: AtomicU64,
    /// Counters.
    requests_sent: AtomicU64,
    events_received: AtomicU64,
    sends_inline: AtomicU64,
    sends_zero_copy: AtomicU64,
    shm_fallbacks: AtomicU64,
    logs_sent: AtomicU64,
}

impl core::fmt::Debug for SessionShared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionShared")
            .field("dataflow", &self.dataflow)
            .field("node", &self.node)
            .field("generation", &self.generation)
            .field("session", &self.session)
            .field("closed", &self.is_closed())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl SessionShared {
    /// Builds the shared state around an established link.
    #[must_use]
    pub fn new(
        spec: Arc<NodeSpawnSpec>,
        session: SessionId,
        outgoing: mpsc::Sender<Outgoing>,
        source: Arc<EventSource>,
        runtime: NodeRuntime,
        zero_copy_threshold: u64,
        limits: FrameLimits,
    ) -> Self {
        Self {
            dataflow: spec.dataflow,
            node: spec.node.clone(),
            generation: spec.generation,
            session,
            spec,
            outgoing,
            source,
            hlc: Mutex::new(HlcClock::system()),
            closed: AtomicBool::new(false),
            ext_waiters: Mutex::new(HashMap::new()),
            routes: RouteTable::new(),
            inputs: InputRouteTable::new(),
            zero_copy_threshold,
            shm_broker: None,
            limits,
            runtime,
            log_subscription: SubscriptionId::new(0),
            log_seq: AtomicU64::new(0),
            requests_sent: AtomicU64::new(0),
            events_received: AtomicU64::new(0),
            sends_inline: AtomicU64::new(0),
            sends_zero_copy: AtomicU64::new(0),
            shm_fallbacks: AtomicU64::new(0),
            logs_sent: AtomicU64::new(0),
        }
    }

    /// Records the daemon's segment-broker socket (§6.2).
    ///
    /// A builder rather than a constructor parameter so an existing caller
    /// keeps compiling: a session without a broker is a session that opens
    /// segments by name, exactly as before.
    #[must_use]
    pub fn with_shm_broker(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.shm_broker = path;
        self
    }

    /// Whether the session has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Queues the writer's shutdown sentinel, so everything already queued is
    /// flushed before the connection ends.
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`] when the session has already ended.
    pub fn request_drain(&self) -> Result<()> {
        self.try_send(Outgoing::Shutdown)
    }

    /// Marks the session ended and wakes every waiter.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.source.close();
        // Every ring reader is stopped and waited for *here*, before the
        // session is declared over: a reader still holding a sample holds a
        // ring slot, and a process that walks away from its consumer-table
        // entry leaves the daemon to reclaim it on a timeout (§6.2).
        self.inputs.close();
        // Anyone still waiting for an extension reply gets `None` rather than
        // a hang: the answer can no longer arrive.
        let waiters = std::mem::take(&mut *lock(&self.ext_waiters));
        for (_key, senders) in waiters {
            for sender in senders {
                let _ = sender.send(None);
            }
        }
    }

    /// The next HLC reading (§4.3).
    #[must_use]
    pub fn hlc_now(&self) -> HlcTimestamp {
        lock(&self.hlc).now()
    }

    /// Folds a peer's timestamp into this node's clock.
    ///
    /// # Errors
    ///
    /// Nothing: an out-of-drift remote reading is ignored rather than
    /// propagated, because a peer with a wild clock must not be able to drag
    /// this node's forward. The rejection is reported by the return value.
    pub fn observe_remote(&self, remote: HlcTimestamp) -> bool {
        lock(&self.hlc).update_with(remote).is_ok()
    }

    /// A snapshot of the session's counters.
    #[must_use]
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            requests_sent: self.requests_sent.load(Ordering::Relaxed),
            events_received: self.events_received.load(Ordering::Relaxed),
            sends_inline: self.sends_inline.load(Ordering::Relaxed),
            sends_zero_copy: self.sends_zero_copy.load(Ordering::Relaxed),
            shm_fallbacks: self.shm_fallbacks.load(Ordering::Relaxed),
            logs_sent: self.logs_sent.load(Ordering::Relaxed),
        }
    }

    /// Records one inline publish.
    pub fn count_inline_send(&self) {
        self.sends_inline.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one zero-copy publish.
    pub fn count_zero_copy_send(&self) {
        self.sends_zero_copy.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one fall back from the zero-copy plane to the daemon path
    /// (§6.2: *never sleep-retry*).
    pub fn count_shm_fallback(&self) {
        self.shm_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one decoded event.
    pub fn count_event(&self) {
        self.events_received.fetch_add(1, Ordering::Relaxed);
    }

    /// The next log sequence number.
    pub fn next_log_seq(&self) -> u64 {
        self.log_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Queues a message without waiting for room.
    ///
    /// # Errors
    ///
    /// [`NodeError::Backpressure`] when the writer is behind, and
    /// [`NodeError::DaemonGone`] once the session has ended.
    pub fn try_send(&self, message: Outgoing) -> Result<()> {
        if self.is_closed() {
            return Err(NodeError::DaemonGone);
        }
        let is_log = matches!(message, Outgoing::Log(_));
        match self.outgoing.try_send(message) {
            Ok(()) => {
                self.count_outgoing(is_log);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(NodeError::Backpressure {
                depth: OUTGOING_CAPACITY,
            }),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(NodeError::DaemonGone),
        }
    }

    /// Queues a message, awaiting room if the writer is behind.
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`] once the session has ended.
    pub async fn send_async(&self, message: Outgoing) -> Result<()> {
        if self.is_closed() {
            return Err(NodeError::DaemonGone);
        }
        let is_log = matches!(message, Outgoing::Log(_));
        self.outgoing
            .send(message)
            .await
            .map_err(|_| NodeError::DaemonGone)?;
        self.count_outgoing(is_log);
        Ok(())
    }

    /// Queues a message, blocking only if the writer is behind.
    ///
    /// The synchronous path a node's own loop uses. The fast path is a plain
    /// `try_send`; only a genuinely full channel touches the runtime, and the
    /// message is recovered from the failed attempt rather than rebuilt.
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`], or [`NodeError::BlockingInAsync`] when the
    /// channel is full and this thread may not block.
    pub fn send_blocking(&self, message: Outgoing) -> Result<()> {
        if self.is_closed() {
            return Err(NodeError::DaemonGone);
        }
        let is_log = matches!(message, Outgoing::Log(_));
        let pending = match self.outgoing.try_send(message) {
            Ok(()) => {
                self.count_outgoing(is_log);
                return Ok(());
            }
            Err(mpsc::error::TrySendError::Full(message)) => message,
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(NodeError::DaemonGone),
        };
        let sender = self.outgoing.clone();
        self.runtime
            .block_on("Node::send", "Node::send_async", async move {
                sender.send(pending).await
            })?
            .map_err(|_| NodeError::DaemonGone)?;
        self.count_outgoing(is_log);
        Ok(())
    }

    /// Sends one protocol request (§7.3).
    ///
    /// A successfully-sent [`NodeRequest::SendMessage`] also closes every
    /// input-to-output latency measurement this session has open (§11.3) —
    /// see [`EventSource::finish_deadlines`]. This is the one place both
    /// publish paths ([`crate::output::RawOutput::send_slice`] and
    /// [`crate::output::OutputSample::send`]) actually converge, which is
    /// what lets the deadline check live here instead of being duplicated
    /// in each of them.
    ///
    /// # Errors
    ///
    /// As [`SessionShared::send_blocking`].
    pub fn send_request(&self, request: NodeRequest) -> Result<()> {
        let is_publish = matches!(request, NodeRequest::SendMessage { .. });
        self.send_blocking(Outgoing::request(request))?;
        if is_publish {
            self.source.finish_deadlines(Instant::now());
        }
        Ok(())
    }

    /// Sends one protocol request, awaiting room.
    ///
    /// As [`SessionShared::send_request`], a successful
    /// [`NodeRequest::SendMessage`] also closes every open deadline
    /// measurement (§11.3).
    ///
    /// # Errors
    ///
    /// As [`SessionShared::send_async`].
    pub async fn send_request_async(&self, request: NodeRequest) -> Result<()> {
        let is_publish = matches!(request, NodeRequest::SendMessage { .. });
        self.send_async(Outgoing::request(request)).await?;
        if is_publish {
            self.source.finish_deadlines(Instant::now());
        }
        Ok(())
    }

    /// Counts an outgoing message by kind.
    fn count_outgoing(&self, is_log: bool) {
        if is_log {
            self.logs_sent.fetch_add(1, Ordering::Relaxed);
        } else {
            self.requests_sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Registers interest in the next `ExtValue` for `key`.
    #[must_use]
    pub fn watch_extension(&self, key: &ExtensionKey) -> oneshot::Receiver<Option<Vec<u8>>> {
        let (sender, receiver) = oneshot::channel();
        lock(&self.ext_waiters)
            .entry(key.to_string())
            .or_default()
            .push(sender);
        receiver
    }

    /// Delivers an `ExtValue` to whoever asked for it.
    ///
    /// Returns whether anybody was waiting; an unasked-for value is not an
    /// error (a daemon may push one), it just has nowhere to go.
    pub fn resolve_extension(&self, key: &ExtensionKey, value: Option<Vec<u8>>) -> bool {
        let mut waiters = lock(&self.ext_waiters);
        let Some(senders) = waiters.get_mut(&key.to_string()) else {
            return false;
        };
        if senders.is_empty() {
            return false;
        }
        let sender = senders.remove(0);
        if senders.is_empty() {
            let _ = waiters.remove(&key.to_string());
        }
        drop(waiters);
        sender.send(value).is_ok()
    }

    /// How many extension replies are outstanding.
    #[must_use]
    pub fn pending_extensions(&self) -> usize {
        lock(&self.ext_waiters).values().map(Vec::len).sum()
    }

    /// The producer port behind one of this node's inputs.
    #[must_use]
    pub fn input_source(&self, input: &DataId) -> Option<astrs_wire::PortRef> {
        self.spec.input(input).map(|spec| spec.source.clone())
    }

    /// Whether `output` is one this node declares.
    #[must_use]
    pub fn declares_output(&self, output: &DataId) -> bool {
        self.spec.output(output).is_some()
    }
}

/// Locks a mutex, recovering from a poisoning panic elsewhere.
///
/// Each guarded value is a clock or a small map with no multi-step invariant;
/// see `astrs-scheduler`'s `sync_util` for the full reasoning.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{LogLevel, NodeSource};

    fn spec() -> Arc<NodeSpawnSpec> {
        Arc::new(NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("camera").unwrap(),
            2,
            NodeSource::Dynamic,
        ))
    }

    fn shared() -> (Arc<SessionShared>, mpsc::Receiver<Outgoing>) {
        let (sender, receiver) = mpsc::channel(OUTGOING_CAPACITY);
        let shared = Arc::new(SessionShared::new(
            spec(),
            SessionId::from_u128(7),
            sender,
            Arc::new(EventSource::new()),
            NodeRuntime::acquire().unwrap(),
            4096,
            FrameLimits::uds(),
        ));
        (shared, receiver)
    }

    #[test]
    fn requests_queue_and_are_counted() {
        let (shared, mut receiver) = shared();
        shared
            .send_request(NodeRequest::EventStreamDropped)
            .unwrap();
        assert_eq!(shared.stats().requests_sent, 1);
        let queued = receiver.try_recv().unwrap();
        assert!(matches!(queued, Outgoing::Request(_)));
    }

    #[test]
    fn a_full_channel_reports_backpressure_rather_than_growing() {
        // `try_send` is the non-waiting face; `send_blocking` deliberately
        // waits for room instead, which the drained test below covers.
        let (shared, _receiver) = shared();
        let mut sent = 0;
        loop {
            match shared.try_send(Outgoing::request(NodeRequest::EventStreamDropped)) {
                Ok(()) => sent += 1,
                Err(NodeError::Backpressure { depth }) => {
                    assert_eq!(depth, OUTGOING_CAPACITY);
                    break;
                }
                Err(error) => panic!("unexpected {error}"),
            }
            assert!(sent <= OUTGOING_CAPACITY + 1, "the channel is unbounded");
        }
        assert_eq!(sent, OUTGOING_CAPACITY);
    }

    #[test]
    fn a_blocking_send_waits_for_room_rather_than_dropping_the_message() {
        let (shared, mut receiver) = shared();
        for _ in 0..OUTGOING_CAPACITY {
            shared
                .try_send(Outgoing::request(NodeRequest::EventStreamDropped))
                .unwrap();
        }
        // A reader that frees one slot shortly lets the blocking send land.
        let drainer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let mut drained = 0;
            while receiver.blocking_recv().is_some() {
                drained += 1;
                if drained == OUTGOING_CAPACITY + 1 {
                    break;
                }
            }
            drained
        });
        shared
            .send_request(NodeRequest::EventStreamDropped)
            .unwrap();
        drop(shared);
        assert!(drainer.join().unwrap() >= 1);
    }

    #[test]
    fn a_closed_session_refuses_further_requests() {
        let (shared, _receiver) = shared();
        shared.close();
        assert!(shared.is_closed());
        assert!(matches!(
            shared.send_request(NodeRequest::EventStreamDropped),
            Err(NodeError::DaemonGone)
        ));
        assert!(shared.source.is_closed());
    }

    #[test]
    fn extension_waiters_are_matched_by_key() {
        let (shared, _receiver) = shared();
        let key = ExtensionKey::user("calibration").unwrap();
        let receiver = shared.watch_extension(&key);
        assert_eq!(shared.pending_extensions(), 1);

        assert!(shared.resolve_extension(&key, Some(vec![1, 2, 3])));
        assert_eq!(shared.pending_extensions(), 0);
        assert_eq!(receiver.blocking_recv().unwrap(), Some(vec![1, 2, 3]));

        // A value nobody asked for is not an error.
        assert!(!shared.resolve_extension(&key, Some(vec![9])));
    }

    #[test]
    fn closing_releases_extension_waiters() {
        let (shared, _receiver) = shared();
        let key = ExtensionKey::user("calibration").unwrap();
        let receiver = shared.watch_extension(&key);
        shared.close();
        assert_eq!(receiver.blocking_recv().unwrap(), None);
        assert_eq!(shared.pending_extensions(), 0);
    }

    #[test]
    fn the_clock_advances_and_absorbs_remote_readings() {
        let (shared, _receiver) = shared();
        let first = shared.hlc_now();
        let second = shared.hlc_now();
        assert!(second >= first);
        assert!(shared.observe_remote(second));
    }

    #[test]
    fn log_frames_are_counted_separately() {
        let (shared, mut receiver) = shared();
        let record = LogRecord::new(shared.hlc_now(), LogLevel::Info, "hello");
        shared
            .try_send(Outgoing::log(shared.log_subscription, record))
            .unwrap();
        assert!(matches!(receiver.try_recv().unwrap(), Outgoing::Log(_)));
        assert_eq!(shared.stats().logs_sent, 1);
        assert_eq!(shared.stats().requests_sent, 0, "logs are not requests");
    }

    #[test]
    fn counters_track_the_two_send_planes() {
        let (shared, _receiver) = shared();
        shared.count_inline_send();
        shared.count_zero_copy_send();
        shared.count_zero_copy_send();
        shared.count_shm_fallback();
        shared.count_event();
        let stats = shared.stats();
        assert_eq!(stats.sends_inline, 1);
        assert_eq!(stats.sends_zero_copy, 2);
        assert_eq!(stats.shm_fallbacks, 1);
        assert_eq!(stats.events_received, 1);
    }

    #[test]
    fn the_specification_answers_wiring_questions() {
        let (shared, _receiver) = shared();
        assert!(!shared.declares_output(&DataId::new("image").unwrap()));
        assert!(
            shared
                .input_source(&DataId::new("frames").unwrap())
                .is_none()
        );
        assert_eq!(shared.generation, 2);
        assert!(format!("{shared:?}").contains("camera"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_async_sender_waits_for_room() {
        let (shared, mut receiver) = shared();
        shared
            .send_async(Outgoing::request(NodeRequest::EventStreamDropped))
            .await
            .unwrap();
        assert!(receiver.recv().await.is_some());
    }
}
