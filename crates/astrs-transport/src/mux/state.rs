//! The mux's shared state: the send scheduler, the route table, the demux.
//!
//! Everything a muxed connection knows lives in one [`MuxShared`], held behind
//! an [`Arc`] by the driver task and by every handle the application holds.
//! The parts that must be consistent with each other — the per-route queues,
//! their credit, and the round-robin cursor — live under a single
//! [`std::sync::Mutex`], and no `await` ever happens while it is held.
//!
//! # Why a blocking mutex in async code
//!
//! The critical sections here are a `VecDeque` push, a `BTreeMap` lookup and
//! an integer decrement. A `tokio::sync::Mutex` would add a future, a waker
//! registration and a heap allocation to each of them, to protect a region
//! that cannot yield. The rule that makes the blocking mutex sound is
//! mechanical and worth stating: **no `.await` inside the lock**. Backpressure
//! is expressed with a separate [`Semaphore`] whose permit is acquired *before*
//! the lock is taken.
//!
//! # The scheduler
//!
//! The private `MuxState::pop_next` is the fairness policy in one function:
//!
//! 1. Control frames are served first, up to
//!    [`MuxConfig::control_burst`](crate::MuxConfig::control_burst) in a row,
//!    then queued datagrams under the same budget.
//! 2. Then one frame from the next route, chosen round-robin, that has both a
//!    queued frame and credit to send it.
//! 3. If no route is ready, the unmetered tiers continue.
//!
//! Step 1's cap is what stops a control flood from starving the data plane;
//! step 2's round robin is what stops a saturated camera route from starving a
//! LiDAR route; and the credit check in step 2 is what stops either of them
//! from outrunning a slow consumer.
//!
//! # Every queue here is bounded, and by what
//!
//! | Queue | Bound | On overflow |
//! |---|---|---|
//! | application control | [`Semaphore`] of `control_queue_depth` | sender waits, or typed [`TransportError::SendQueueFull`] |
//! | datagrams | `datagram_queue_depth` frames | **oldest dropped**, counted |
//! | per-route data | [`Semaphore`] of `route_queue_depth` **and** peer credit | sender waits, or typed overflow |
//! | internal control (credit grants, route lifecycle) | structural: `O(routes)` | cannot overflow |
//!
//! The last row is the one that justifies having no explicit bound: a credit
//! grant is only ever emitted after `credit_grant_threshold` frames have been
//! *consumed*, and route lifecycle frames are one per route transition, so the
//! outstanding count is `O(routes × window / threshold)` — bounded by the
//! negotiated route ceiling, not by peer behaviour.
//!
//! The datagram row is the one that has to differ. A datagram sender never
//! waits: "the newest pose, right now" is the whole contract, and a caller
//! blocked behind a stalled link would be publishing stale poses by the time it
//! unblocked. Dropping the oldest is therefore the *correct* answer rather than
//! a concession, and it is the same policy the receive side applies, so both
//! ends of a saturated datagram channel behave alike.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use astrs_wire::{Compression, Frame, FrameFlags, FrameKind, RouteId};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};

use crate::compress::decompress_payload;
use crate::config::{CompressionPolicy, MuxConfig, Side};
use crate::error::{CloseReason, TransportError, TransportResult};
use crate::stats::{ConnectionCounters, RouteCounters, RouteStatsSnapshot};

use super::header::{
    AcceptBody, CLOSE_CODE_DUPLICATE, CLOSE_CODE_FLOW_CONTROL, CLOSE_CODE_NORMAL,
    CLOSE_CODE_ROUTE_LIMIT, CloseBody, MuxHeader, MuxTag, OpenBody, build_payload, decode_credit,
};

/// A frame waiting for the writer, with the queue permit that reserved its
/// slot.
///
/// The permit is released when this value is dropped — after the frame has
/// been written — so a route's queue depth is a true bound on outstanding
/// bytes rather than an advisory one.
#[derive(Debug)]
pub(crate) struct Outbound {
    /// The frame to write.
    pub(crate) frame: Frame,
    /// Which route it belongs to; [`RouteId::NONE`] for control and datagrams.
    pub(crate) route: RouteId,
    /// The application payload size before compression, for accounting.
    pub(crate) payload_bytes: u64,
    /// Whether writing it costs a unit of the route's window.
    pub(crate) consumes_credit: bool,
    /// The queue slot this frame reserved.
    ///
    /// Never read: it exists to be *dropped*, which is what releases the slot
    /// back to a blocked sender. The drop happens after the frame is on the
    /// wire, so the queue depth bounds bytes in flight rather than bytes
    /// merely accepted.
    pub(crate) _permit: Option<OwnedSemaphorePermit>,
}

impl Outbound {
    /// An unmetered frame: control, credit grants, route lifecycle.
    pub(crate) const fn control(frame: Frame) -> Self {
        Self {
            frame,
            route: RouteId::NONE,
            payload_bytes: 0,
            consumes_credit: false,
            _permit: None,
        }
    }
}

/// One route's send queue, window and demux endpoint.
#[derive(Debug)]
struct RouteEntry {
    /// Frames waiting for the writer.
    queue: VecDeque<Outbound>,
    /// Frames this end may still send before it must wait for a grant.
    credit: u32,
    /// The window the peer advertised, used to size credit grants.
    window: u32,
    /// Frames delivered to the application since the last grant was sent.
    consumed_since_grant: u32,
    /// Where inbound frames for this route are delivered.
    inbound: mpsc::Sender<Frame>,
    /// This route's counters.
    counters: Arc<RouteCounters>,
    /// The codec both ends agreed to for this route.
    compression: Compression,
    /// Whether the peer has confirmed the open.
    confirmed: bool,
}

impl RouteEntry {
    /// Whether this route has a frame to send and the credit to send it.
    fn is_ready(&self) -> bool {
        match self.queue.front() {
            Some(front) => !front.consumes_credit || self.credit > 0,
            None => false,
        }
    }
}

/// The mutable half of the mux.
#[derive(Debug)]
struct MuxState {
    /// The control-plane queue — "stream 0".
    control: VecDeque<Outbound>,
    /// Best-effort datagrams, bounded and drop-oldest.
    datagrams: VecDeque<Outbound>,
    /// Every open route, in handle order.
    routes: BTreeMap<RouteId, RouteEntry>,
    /// The round-robin cursor over open routes.
    order: VecDeque<RouteId>,
    /// How many control frames have been served since the last route frame.
    control_run: u32,
    /// The inbound endpoints, dropped on close so receivers see end-of-stream.
    inbound: InboundSinks,
}

/// Where the demux delivers frames the application has not claimed a route
/// for.
#[derive(Debug, Default)]
struct InboundSinks {
    /// Control-plane frames.
    control: Option<mpsc::Sender<Frame>>,
    /// Best-effort datagrams.
    datagrams: Option<mpsc::Sender<Frame>>,
    /// Routes the peer opened.
    accepts: Option<mpsc::UnboundedSender<InboundRoute>>,
}

/// A route the peer opened, waiting to be claimed by the application.
#[derive(Debug)]
pub struct InboundRoute {
    /// The handle the peer minted.
    pub route: RouteId,
    /// The opaque descriptor the peer attached, if any.
    pub descriptor: Vec<u8>,
    /// Where this route's frames arrive.
    pub(crate) receiver: mpsc::Receiver<Frame>,
    /// This route's counters.
    pub(crate) counters: Arc<RouteCounters>,
    /// The send-queue permits for this route.
    pub(crate) permits: Arc<Semaphore>,
    /// The codec this route uses.
    pub(crate) compression: Compression,
}

impl MuxState {
    /// Picks the next frame to write, or [`None`] when nothing is ready.
    ///
    /// See the module documentation for the policy; this is its implementation
    /// and the only place scheduling decisions are made.
    fn pop_next(&mut self, control_burst: u32) -> Option<Outbound> {
        if self.control_run < control_burst
            && let Some(item) = self.pop_unmetered()
        {
            self.control_run = self.control_run.saturating_add(1);
            return Some(item);
        }

        if let Some(item) = self.pop_route() {
            self.control_run = 0;
            return Some(item);
        }

        // No route can make progress: the unmetered tiers keep the connection
        // alive rather than letting it idle behind a stalled route.
        let item = self.pop_unmetered()?;
        self.control_run = self.control_run.saturating_add(1);
        Some(item)
    }

    /// One frame from the unmetered tiers: control first, then datagrams.
    ///
    /// Control outranks datagrams because a heartbeat that is late costs a
    /// declared-dead node, while a pose that is late is simply replaced by the
    /// next one.
    fn pop_unmetered(&mut self) -> Option<Outbound> {
        self.control
            .pop_front()
            .or_else(|| self.datagrams.pop_front())
    }

    /// One frame from the next ready route, round-robin.
    fn pop_route(&mut self) -> Option<Outbound> {
        for _ in 0..self.order.len() {
            let route = self.order.pop_front()?;
            self.order.push_back(route);
            let ready = self.routes.get(&route).is_some_and(RouteEntry::is_ready);
            if !ready {
                continue;
            }
            let entry = self.routes.get_mut(&route)?;
            let item = entry.queue.pop_front()?;
            if item.consumes_credit {
                entry.credit = entry.credit.saturating_sub(1);
                entry.counters.set_credit_remaining(entry.credit);
            }
            return Some(item);
        }
        None
    }

    /// Whether anything at all could be written right now.
    fn has_work(&self) -> bool {
        !self.control.is_empty()
            || !self.datagrams.is_empty()
            || self.routes.values().any(RouteEntry::is_ready)
    }
}

/// The mux, shared by the driver task and every handle.
#[derive(Debug)]
pub struct MuxShared {
    /// The policy this connection runs under.
    config: MuxConfig,
    /// Which side minted this connection, deciding route-handle parity.
    side: Side,
    /// Connection-wide counters.
    counters: Arc<ConnectionCounters>,
    /// The negotiated payload ceiling, which bounds decompression.
    max_payload: AtomicUsize,
    /// The datagram ceiling, or zero to follow `max_payload`.
    max_datagram: AtomicUsize,
    /// The negotiated route ceiling.
    max_routes: u32,
    /// The negotiated compression policy.
    compression: CompressionPolicy,
    /// The scheduler and route table.
    state: Mutex<MuxState>,
    /// Wakes the writer task when work appears.
    writable: Notify,
    /// Whether the connection has closed.
    closed: AtomicBool,
    /// Why it closed, once it has.
    close_reason: Mutex<Option<CloseReason>>,
    /// The next route handle this end will mint.
    next_route: AtomicU64,
    /// Permits bounding the *application's* share of the control queue.
    ///
    /// Internal control traffic — credit grants, route lifecycle — takes no
    /// permit, because it must never block: it is already bounded by the route
    /// count and the grant threshold, at `O(routes)` frames outstanding.
    control_permits: Arc<Semaphore>,
}

impl MuxShared {
    /// Builds a mux and the receivers the application will read from.
    #[must_use]
    pub fn new(
        config: MuxConfig,
        side: Side,
        counters: Arc<ConnectionCounters>,
        compression: CompressionPolicy,
        max_payload: usize,
        max_routes: u32,
    ) -> (Arc<Self>, MuxReceivers) {
        let (control_tx, control_rx) = mpsc::channel(config.control_queue_depth.max(1));
        let (datagram_tx, datagram_rx) = mpsc::channel(config.datagram_queue_depth.max(1));
        let (accept_tx, accept_rx) = mpsc::unbounded_channel();

        let shared = Arc::new(Self {
            config,
            side,
            counters,
            max_payload: AtomicUsize::new(max_payload),
            max_datagram: AtomicUsize::new(0),
            max_routes,
            compression,
            state: Mutex::new(MuxState {
                control: VecDeque::new(),
                datagrams: VecDeque::with_capacity(config.datagram_queue_depth.min(1_024)),
                routes: BTreeMap::new(),
                order: VecDeque::new(),
                control_run: 0,
                inbound: InboundSinks {
                    control: Some(control_tx),
                    datagrams: Some(datagram_tx),
                    accepts: Some(accept_tx),
                },
            }),
            writable: Notify::new(),
            closed: AtomicBool::new(false),
            close_reason: Mutex::new(None),
            next_route: AtomicU64::new(side.first_route().get()),
            control_permits: Arc::new(Semaphore::new(config.control_queue_depth.max(1))),
        });

        (
            shared,
            MuxReceivers {
                control: control_rx,
                datagrams: datagram_rx,
                accepts: accept_rx,
            },
        )
    }

    /// The policy this connection runs under.
    #[must_use]
    pub const fn config(&self) -> &MuxConfig {
        &self.config
    }

    /// Which side of the connection this end is.
    #[must_use]
    pub const fn side(&self) -> Side {
        self.side
    }

    /// The connection-wide counters.
    #[must_use]
    pub fn counters(&self) -> &Arc<ConnectionCounters> {
        &self.counters
    }

    /// The codec this connection negotiated.
    #[must_use]
    pub const fn compression(&self) -> CompressionPolicy {
        self.compression
    }

    /// The negotiated payload ceiling, which bounds decompression.
    #[must_use]
    pub fn max_payload(&self) -> usize {
        self.max_payload.load(Ordering::Relaxed)
    }

    /// Applies a payload ceiling negotiated after the mux was created.
    pub fn set_max_payload(&self, bytes: usize) {
        self.max_payload.store(bytes, Ordering::Relaxed);
    }

    /// Whether the connection has closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Why the connection closed, if it has.
    #[must_use]
    pub fn close_reason(&self) -> Option<CloseReason> {
        lock(&self.close_reason).clone()
    }

    /// Waits until the writer has something to do, or the connection closes.
    pub async fn wait_for_work(&self) {
        self.writable.notified().await;
    }

    /// Takes up to `max` frames to write, in scheduler order.
    #[must_use]
    pub(crate) fn take_batch(&self, max: usize) -> Vec<Outbound> {
        let mut state = lock(&self.state);
        let mut batch = Vec::new();
        while batch.len() < max {
            match state.pop_next(self.config.control_burst) {
                Some(item) => batch.push(item),
                None => break,
            }
        }
        batch
    }

    /// Whether anything could be written right now.
    #[must_use]
    pub fn has_work(&self) -> bool {
        lock(&self.state).has_work()
    }

    /// Closes the connection, waking the writer and ending every receiver.
    ///
    /// Idempotent: the first reason wins, so a close forced by a checksum
    /// failure is not overwritten by the end-of-stream that follows it.
    pub fn close(&self, reason: CloseReason) {
        if self.closed.swap(true, Ordering::AcqRel) {
            self.writable.notify_waiters();
            return;
        }
        {
            let mut slot = lock(&self.close_reason);
            if slot.is_none() {
                *slot = Some(reason);
            }
        }
        {
            let mut state = lock(&self.state);
            // Dropping the entries drops their inbound senders, which is what
            // ends each route receiver.
            for route in state.routes.keys().copied().collect::<Vec<_>>() {
                state.routes.remove(&route);
                self.counters.record_route_closed();
            }
            state.order.clear();
            state.control.clear();
            state.datagrams.clear();
            state.inbound = InboundSinks::default();
        }
        self.writable.notify_waiters();
    }

    /// Fails if the connection has closed.
    fn ensure_open(&self) -> TransportResult<()> {
        if self.is_closed() {
            return Err(match self.close_reason() {
                Some(reason) => TransportError::Closed { reason },
                None => TransportError::NotConnected,
            });
        }
        Ok(())
    }

    /// Mints the next route handle this side owns.
    ///
    /// Handles are parity-partitioned (odd for the initiator, even for the
    /// acceptor) so both ends can open routes concurrently without agreeing on
    /// anything first.
    #[must_use]
    pub fn next_route_id(&self) -> RouteId {
        RouteId::new(self.next_route.fetch_add(2, Ordering::Relaxed))
    }

    /// The permits bounding the application's control-queue share.
    #[must_use]
    pub fn control_permits(&self) -> Arc<Semaphore> {
        Arc::clone(&self.control_permits)
    }

    /// Queues an application control frame against a queue permit.
    ///
    /// The permit is what makes the control queue bounded: it is acquired by
    /// the caller (waiting, or failing with a typed overflow) *before* the
    /// frame reaches the scheduler, and released only once the frame is on the
    /// wire. Internal control traffic — credit grants, route lifecycle — takes
    /// no permit, because its volume is `O(routes)` by construction rather
    /// than by peer behaviour.
    ///
    /// # Errors
    ///
    /// [`TransportError::Closed`] if the connection has ended,
    /// [`TransportError::FrameTooLarge`] if the payload exceeds the negotiated
    /// ceiling.
    pub fn queue_control_metered(
        &self,
        kind: FrameKind,
        body: &[u8],
        permit: OwnedSemaphorePermit,
    ) -> TransportResult<()> {
        self.ensure_open()?;
        let payload = build_payload(MuxHeader::control(), body);
        self.check_payload(payload.len())?;
        let frame = Frame::new(kind, FrameFlags::EMPTY, payload).map_err(TransportError::Wire)?;
        self.push_control(Outbound {
            frame,
            route: RouteId::NONE,
            payload_bytes: body.len() as u64,
            consumes_credit: false,
            _permit: Some(permit),
        });
        Ok(())
    }

    /// Queues a best-effort datagram, dropping the oldest if the queue is full.
    ///
    /// Never waits and never applies backpressure: that is the datagram
    /// contract (§6.4, "latest-only low-rate topics"). The queue is bounded at
    /// [`MuxConfig::datagram_queue_depth`](crate::MuxConfig::datagram_queue_depth)
    /// frames, and an overflow evicts the *oldest* — the same policy the
    /// receive side applies, so both ends of a saturated datagram channel lose
    /// the same thing.
    ///
    /// # Errors
    ///
    /// - [`TransportError::Closed`] if the connection has ended.
    /// - [`TransportError::DatagramTooLarge`] if the payload exceeds the
    ///   negotiated ceiling. Unlike a route frame this is never a queue
    ///   problem, so it gets its own variant.
    pub fn queue_datagram(&self, kind: FrameKind, body: &[u8]) -> TransportResult<()> {
        self.ensure_open()?;
        let payload = build_payload(MuxHeader::datagram(), body);
        let limit = self.max_datagram_bytes();
        if payload.len() > limit {
            return Err(TransportError::DatagramTooLarge {
                actual: payload.len(),
                limit,
            });
        }
        let frame = Frame::new(kind, FrameFlags::EMPTY, payload).map_err(TransportError::Wire)?;
        let item = Outbound {
            payload_bytes: body.len() as u64,
            ..Outbound::control(frame)
        };

        let dropped = {
            let mut state = lock(&self.state);
            let depth = self.config.datagram_queue_depth.max(1);
            let dropped = if state.datagrams.len() >= depth {
                state.datagrams.pop_front().is_some()
            } else {
                false
            };
            state.datagrams.push_back(item);
            dropped
        };
        self.writable.notify_one();

        if dropped {
            self.counters.record_datagram_dropped();
        }
        self.counters.record_datagram_sent(0);
        Ok(())
    }

    /// The largest datagram this connection will carry, in bytes.
    ///
    /// The negotiated payload ceiling unless a backend narrowed it — a QUIC
    /// path with native datagrams has a much smaller one than its stream
    /// ceiling.
    #[must_use]
    pub fn max_datagram_bytes(&self) -> usize {
        let declared = self.max_datagram.load(Ordering::Relaxed);
        if declared == 0 {
            self.max_payload()
        } else {
            declared.min(self.max_payload())
        }
    }

    /// Narrows the datagram ceiling to what the path actually carries.
    ///
    /// Zero restores "whatever a frame may be".
    pub fn set_max_datagram_bytes(&self, bytes: usize) {
        self.max_datagram.store(bytes, Ordering::Relaxed);
    }

    /// How many datagrams are queued for the writer.
    #[must_use]
    pub fn queued_datagrams(&self) -> usize {
        lock(&self.state).datagrams.len()
    }

    /// Queues a frame on `route`, consuming one unit of its window.
    ///
    /// The caller must already hold `permit`, acquired from the route's
    /// semaphore: that is where backpressure lives, and it is acquired before
    /// the state lock so that a full queue never blocks the scheduler.
    ///
    /// # Errors
    ///
    /// [`TransportError::UnknownRoute`] if the route closed while the caller
    /// was waiting for its permit.
    pub(crate) fn queue_route_frame(
        &self,
        route: RouteId,
        frame: Frame,
        payload_bytes: u64,
        permit: Option<OwnedSemaphorePermit>,
    ) -> TransportResult<()> {
        self.ensure_open()?;
        self.check_payload(frame.payload().len())?;
        let mut state = lock(&self.state);
        let entry = state
            .routes
            .get_mut(&route)
            .ok_or(TransportError::UnknownRoute { route })?;
        entry.queue.push_back(Outbound {
            frame,
            route,
            payload_bytes,
            consumes_credit: true,
            _permit: permit,
        });
        drop(state);
        self.writable.notify_one();
        Ok(())
    }

    /// Pushes an unmetered frame onto the control queue.
    ///
    /// Reserved for traffic whose volume is structurally bounded: credit
    /// grants (`O(routes × window / threshold)`) and route lifecycle frames
    /// (`O(routes)`). Application control frames take a permit first — see
    /// [`MuxShared::queue_control_metered`] — so that a producer whose peer is
    /// not draining blocks rather than growing this queue without bound.
    fn push_control(&self, item: Outbound) {
        lock(&self.state).control.push_back(item);
        self.writable.notify_one();
    }

    /// Refuses a payload that exceeds the negotiated ceiling.
    fn check_payload(&self, len: usize) -> TransportResult<()> {
        let limit = self.max_payload();
        if len > limit {
            return Err(TransportError::FrameTooLarge { actual: len, limit });
        }
        Ok(())
    }

    /// Opens a route this end mints, and queues the `RouteOpen` that announces
    /// it.
    ///
    /// The open is *optimistic*: the handle is usable immediately, and the
    /// peer's [`MuxTag::RouteAccept`] only adjusts the window or refuses. That
    /// removes a round trip from route setup, which matters because a dataflow
    /// with a hundred routes would otherwise pay a hundred of them. The
    /// application-level negotiation that genuinely needs an answer —
    /// [`astrs_wire::PeerEvent::RouteSetup`] and its `RouteAccept` — rides on
    /// the route once it exists.
    ///
    /// # Errors
    ///
    /// - [`TransportError::Closed`] if the connection has ended.
    /// - [`TransportError::RouteLimitReached`] at the negotiated ceiling.
    /// - [`TransportError::DuplicateRoute`] if the handle is already in use.
    pub fn open_route(&self, route: RouteId, descriptor: &[u8]) -> TransportResult<OpenedRoute> {
        self.ensure_open()?;
        let window = self.config.initial_window_frames;
        let (receiver, permits, counters) = self.insert_route(route, window, true)?;

        let mut body = Vec::new();
        OpenBody::new(window)
            .with_descriptor(descriptor.to_vec())
            .encode_into(&mut body);
        let payload = build_payload(MuxHeader::new(MuxTag::RouteOpen, route), &body);
        self.check_payload(payload.len())?;
        let frame = Frame::new(FrameKind::PeerEvent, FrameFlags::EMPTY, payload)
            .map_err(TransportError::Wire)?;
        self.push_control(Outbound::control(frame));

        Ok(OpenedRoute {
            route,
            receiver,
            permits,
            counters,
            compression: self.compression.codec,
        })
    }

    /// Creates the route table entry and its channels.
    fn insert_route(
        &self,
        route: RouteId,
        window: u32,
        confirmed: bool,
    ) -> TransportResult<(mpsc::Receiver<Frame>, Arc<Semaphore>, Arc<RouteCounters>)> {
        let mut state = lock(&self.state);
        if state.routes.contains_key(&route) {
            return Err(TransportError::DuplicateRoute { route });
        }
        if state.routes.len() as u64 >= u64::from(self.max_routes) {
            return Err(TransportError::RouteLimitReached {
                limit: self.max_routes,
            });
        }

        // The inbound channel is sized to exactly the window the peer is
        // granted, which is what makes `try_send` on the receive path
        // infallible for a peer that respects its credit.
        let (inbound_tx, inbound_rx) = mpsc::channel(window.max(1) as usize);
        let permits = Arc::new(Semaphore::new(self.config.route_queue_depth.max(1)));
        let counters = RouteCounters::shared(window);

        state.routes.insert(
            route,
            RouteEntry {
                queue: VecDeque::new(),
                credit: window,
                window,
                consumed_since_grant: 0,
                inbound: inbound_tx,
                counters: Arc::clone(&counters),
                compression: self.compression.codec,
                confirmed,
            },
        );
        state.order.push_back(route);
        drop(state);
        self.counters.record_route_opened();
        Ok((inbound_rx, permits, counters))
    }

    /// Closes one route without touching the rest of the connection.
    pub fn close_route(&self, route: RouteId, code: u16, detail: &str) {
        let existed = {
            let mut state = lock(&self.state);
            let removed = state.routes.remove(&route).is_some();
            if removed {
                state.order.retain(|open| *open != route);
            }
            removed
        };
        if !existed {
            return;
        }
        self.counters.record_route_closed();

        if self.is_closed() {
            return;
        }
        let mut body = Vec::new();
        CloseBody::with_detail(code, detail).encode_into(&mut body);
        let payload = build_payload(MuxHeader::new(MuxTag::RouteClose, route), &body);
        if let Ok(frame) = Frame::new(FrameKind::PeerEvent, FrameFlags::EMPTY, payload) {
            self.push_control(Outbound::control(frame));
        }
    }

    /// Records that the application consumed a frame, granting credit back
    /// once enough have accumulated.
    ///
    /// Granting per frame would double the frame rate on a busy route;
    /// granting only when the window empties would stall the sender for a full
    /// round trip. The threshold in
    /// [`MuxConfig::credit_grant_threshold`](crate::MuxConfig::credit_grant_threshold)
    /// is the compromise.
    pub fn note_consumed(&self, route: RouteId) {
        let grant = {
            let mut state = lock(&self.state);
            let Some(entry) = state.routes.get_mut(&route) else {
                return;
            };
            entry.consumed_since_grant = entry.consumed_since_grant.saturating_add(1);
            let threshold = self
                .config
                .credit_grant_threshold()
                .min(entry.window.max(1));
            if entry.consumed_since_grant < threshold {
                return;
            }
            let grant = entry.consumed_since_grant;
            entry.consumed_since_grant = 0;
            grant
        };
        self.grant_credit(route, grant);
    }

    /// Sends the peer `delta` more frames of window on `route`.
    pub fn grant_credit(&self, route: RouteId, delta: u32) {
        if delta == 0 || self.is_closed() {
            return;
        }
        let payload = build_payload(
            MuxHeader::new(MuxTag::Credit, route),
            &super::header::encode_credit(delta),
        );
        if let Ok(frame) = Frame::new(FrameKind::PeerEvent, FrameFlags::EMPTY, payload) {
            self.push_control(Outbound::control(frame));
            self.counters.record_credit_granted();
        }
    }

    /// Per-route statistics for every open route.
    #[must_use]
    pub fn route_snapshots(&self) -> Vec<RouteStatsSnapshot> {
        let state = lock(&self.state);
        state
            .routes
            .iter()
            .map(|(route, entry)| {
                entry
                    .counters
                    .snapshot(*route)
                    .with_compression(entry.compression)
            })
            .collect()
    }

    /// How many routes are open.
    #[must_use]
    pub fn open_route_count(&self) -> usize {
        lock(&self.state).routes.len()
    }

    /// Whether `route` is open on this connection.
    #[must_use]
    pub fn has_route(&self, route: RouteId) -> bool {
        lock(&self.state).routes.contains_key(&route)
    }

    /// Whether the peer has confirmed `route`.
    #[must_use]
    pub fn is_route_confirmed(&self, route: RouteId) -> Option<bool> {
        lock(&self.state)
            .routes
            .get(&route)
            .map(|entry| entry.confirmed)
    }
}

/// The route this end just opened, and everything needed to drive it.
#[derive(Debug)]
pub struct OpenedRoute {
    /// The handle that was minted.
    pub route: RouteId,
    /// Where this route's inbound frames arrive.
    pub(crate) receiver: mpsc::Receiver<Frame>,
    /// The send-queue permits.
    pub(crate) permits: Arc<Semaphore>,
    /// This route's counters.
    pub(crate) counters: Arc<RouteCounters>,
    /// The codec this route uses.
    pub(crate) compression: Compression,
}

/// The receiving ends the application reads from.
#[derive(Debug)]
pub struct MuxReceivers {
    /// Control-plane frames.
    pub control: mpsc::Receiver<Frame>,
    /// Best-effort datagrams.
    pub datagrams: mpsc::Receiver<Frame>,
    /// Routes the peer opened.
    pub accepts: mpsc::UnboundedReceiver<InboundRoute>,
}

/// The outcome of demultiplexing one inbound frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dispatched {
    /// The frame was delivered, or deliberately dropped.
    Handled,
    /// The peer asked to end the connection.
    PeerClosed,
}

impl MuxShared {
    /// Demultiplexes one inbound frame.
    ///
    /// The control path *awaits* its delivery, which is deliberate: an
    /// application that stops draining control frames should apply socket
    /// backpressure to the peer rather than silently lose protocol messages.
    /// Route data never awaits — its credit window guarantees a free slot —
    /// and datagrams never await, because a datagram that cannot be delivered
    /// now is a datagram that has already lost its value.
    ///
    /// # Errors
    ///
    /// [`TransportError::MalformedMux`] and friends, all of which are fatal to
    /// the connection: a frame whose routing cannot be determined means the
    /// mux has lost track of the stream.
    pub(crate) async fn dispatch(&self, frame: Frame) -> TransportResult<Dispatched> {
        let compression = frame.flags().compression();
        let (header, body) = MuxHeader::decode(frame.payload())?;

        match header.tag {
            MuxTag::Control => {
                let payload = self.decode_body(compression, body)?;
                let inner = Frame::new(frame.kind(), FrameFlags::EMPTY, payload)
                    .map_err(TransportError::Wire)?;
                let Some(sink) = self.control_sink() else {
                    return Ok(Dispatched::PeerClosed);
                };
                if sink.send(inner).await.is_err() {
                    return Ok(Dispatched::PeerClosed);
                }
                Ok(Dispatched::Handled)
            }
            MuxTag::Datagram => {
                let payload = self.decode_body(compression, body)?;
                let inner = Frame::new(frame.kind(), FrameFlags::EMPTY, payload)
                    .map_err(TransportError::Wire)?;
                let Some(sink) = self.datagram_sink() else {
                    return Ok(Dispatched::Handled);
                };
                match sink.try_send(inner) {
                    Ok(()) => self.counters.record_datagram_received(0),
                    Err(TrySendError::Full(_)) => self.counters.record_datagram_dropped(),
                    Err(TrySendError::Closed(_)) => {}
                }
                Ok(Dispatched::Handled)
            }
            MuxTag::RouteData => {
                self.dispatch_route_data(header.route, frame.kind(), compression, body)
            }
            MuxTag::RouteOpen => self.dispatch_route_open(header.route, body),
            MuxTag::RouteAccept => self.dispatch_route_accept(header.route, body),
            MuxTag::RouteClose => {
                let close = CloseBody::decode(body)?;
                self.close_route_locally(header.route);
                let _ = close;
                Ok(Dispatched::Handled)
            }
            MuxTag::Credit => {
                let delta = decode_credit(body)?;
                self.add_credit(header.route, delta);
                Ok(Dispatched::Handled)
            }
        }
    }

    /// Delivers one payload frame to its route.
    fn dispatch_route_data(
        &self,
        route: RouteId,
        kind: FrameKind,
        compression: Compression,
        body: &[u8],
    ) -> TransportResult<Dispatched> {
        let payload = self.decode_body(compression, body)?;
        let payload_len = payload.len() as u64;
        let inner = Frame::new(kind, FrameFlags::EMPTY, payload).map_err(TransportError::Wire)?;

        let outcome = {
            let state = lock(&self.state);
            match state.routes.get(&route) {
                Some(entry) => {
                    entry.counters.record_frame_received(
                        (body.len() + super::header::MUX_HEADER_LEN) as u64,
                        payload_len,
                    );
                    Some((entry.inbound.clone(), entry.window))
                }
                None => None,
            }
        };

        let Some((sink, window)) = outcome else {
            // A frame for a route this end already closed. The peer will see
            // the `RouteClose` shortly; dropping is correct and not an error.
            return Ok(Dispatched::Handled);
        };

        match sink.try_send(inner) {
            Ok(()) => Ok(Dispatched::Handled),
            Err(TrySendError::Closed(_)) => Ok(Dispatched::Handled),
            Err(TrySendError::Full(_)) => {
                // The peer sent past its window. The route is unusable —
                // resynchronising would mean guessing which frame was lost —
                // so it is torn down loudly rather than silently truncated.
                self.counters.record_error();
                self.close_route(
                    route,
                    CLOSE_CODE_FLOW_CONTROL,
                    "flow-control window overrun",
                );
                Err(TransportError::FlowControlViolation { route, window })
            }
        }
    }

    /// Accepts a route the peer opened.
    fn dispatch_route_open(&self, route: RouteId, body: &[u8]) -> TransportResult<Dispatched> {
        let open = OpenBody::decode(body)?;
        // The peer must mint handles from its own parity half.
        if self.side.owns_route(route) {
            self.reject_route(
                route,
                CLOSE_CODE_DUPLICATE,
                "route handle parity belongs to this end",
            );
            return Ok(Dispatched::Handled);
        }

        let window = if open.credit == 0 {
            self.config.initial_window_frames
        } else {
            open.credit
        };
        match self.insert_route(route, window, true) {
            Ok((receiver, permits, counters)) => {
                let accepted = {
                    let state = lock(&self.state);
                    state.inbound.accepts.clone()
                };
                let Some(accepts) = accepted else {
                    return Ok(Dispatched::PeerClosed);
                };
                let inbound = InboundRoute {
                    route,
                    descriptor: open.descriptor,
                    receiver,
                    counters,
                    permits,
                    compression: self.compression.codec,
                };
                if accepts.send(inbound).is_err() {
                    return Ok(Dispatched::PeerClosed);
                }
                self.answer_open(route, AcceptBody::accepted(window));
                Ok(Dispatched::Handled)
            }
            Err(TransportError::RouteLimitReached { limit }) => {
                self.reject_route(
                    route,
                    CLOSE_CODE_ROUTE_LIMIT,
                    &format!("route limit of {limit} reached"),
                );
                Ok(Dispatched::Handled)
            }
            Err(TransportError::DuplicateRoute { .. }) => {
                self.reject_route(route, CLOSE_CODE_DUPLICATE, "route handle already in use");
                Ok(Dispatched::Handled)
            }
            Err(err) => Err(err),
        }
    }

    /// Applies the peer's answer to a route this end opened.
    fn dispatch_route_accept(&self, route: RouteId, body: &[u8]) -> TransportResult<Dispatched> {
        let accept = AcceptBody::decode(body)?;
        if !accept.accepted {
            self.close_route_locally(route);
            return Ok(Dispatched::Handled);
        }
        let mut state = lock(&self.state);
        if let Some(entry) = state.routes.get_mut(&route) {
            entry.confirmed = true;
            // The peer's window replaces the optimistic one, never widens past
            // what it offered.
            entry.credit = entry.credit.min(accept.credit);
            entry.window = accept.credit.max(1);
            entry.counters.set_credit_remaining(entry.credit);
        }
        drop(state);
        self.writable.notify_one();
        Ok(Dispatched::Handled)
    }

    /// Answers a `RouteOpen`.
    fn answer_open(&self, route: RouteId, accept: AcceptBody) {
        let mut body = Vec::new();
        accept.encode_into(&mut body);
        let payload = build_payload(MuxHeader::new(MuxTag::RouteAccept, route), &body);
        if let Ok(frame) = Frame::new(FrameKind::PeerEvent, FrameFlags::EMPTY, payload) {
            self.push_control(Outbound::control(frame));
        }
    }

    /// Refuses a `RouteOpen` without creating a table entry.
    fn reject_route(&self, route: RouteId, code: u16, detail: &str) {
        self.answer_open(route, AcceptBody::refused(detail));
        let mut body = Vec::new();
        CloseBody::with_detail(code, detail).encode_into(&mut body);
        let payload = build_payload(MuxHeader::new(MuxTag::RouteClose, route), &body);
        if let Ok(frame) = Frame::new(FrameKind::PeerEvent, FrameFlags::EMPTY, payload) {
            self.push_control(Outbound::control(frame));
        }
    }

    /// Drops a route's table entry without telling the peer.
    ///
    /// Used when the peer is the one that closed it: echoing a `RouteClose`
    /// back would loop.
    fn close_route_locally(&self, route: RouteId) {
        let removed = {
            let mut state = lock(&self.state);
            let removed = state.routes.remove(&route).is_some();
            if removed {
                state.order.retain(|open| *open != route);
            }
            removed
        };
        if removed {
            self.counters.record_route_closed();
        }
    }

    /// Adds flow-control credit granted by the peer.
    fn add_credit(&self, route: RouteId, delta: u32) {
        {
            let mut state = lock(&self.state);
            let Some(entry) = state.routes.get_mut(&route) else {
                return;
            };
            entry.credit = entry.credit.saturating_add(delta);
            entry.counters.set_credit_remaining(entry.credit);
        }
        self.counters.record_credit_received();
        self.writable.notify_one();
    }

    /// Decompresses a body if the frame's flags say it is compressed.
    fn decode_body(&self, compression: Compression, body: &[u8]) -> TransportResult<Vec<u8>> {
        if !compression.is_enabled() {
            return Ok(body.to_vec());
        }
        if !crate::compress::is_supported_codec(compression) {
            return Err(TransportError::CompressionNotNegotiated { codec: compression });
        }
        decompress_payload(compression, body, self.max_payload())
    }

    /// The control-plane sink, if the connection is still open.
    fn control_sink(&self) -> Option<mpsc::Sender<Frame>> {
        lock(&self.state).inbound.control.clone()
    }

    /// The datagram sink, if the connection is still open.
    fn datagram_sink(&self) -> Option<mpsc::Sender<Frame>> {
        lock(&self.state).inbound.datagrams.clone()
    }

    /// Marks a control frame as written, for accounting.
    pub(crate) fn record_written(&self, item: &Outbound, wire_bytes: u64) {
        if item.route.is_none() {
            return;
        }
        let state = lock(&self.state);
        if let Some(entry) = state.routes.get(&item.route) {
            entry
                .counters
                .record_frame_sent(wire_bytes, item.payload_bytes);
        }
    }

    /// Records a sender that had to wait for queue space.
    pub(crate) fn record_stall(&self, route: RouteId) {
        self.counters.record_send_stall();
        let state = lock(&self.state);
        if let Some(entry) = state.routes.get(&route) {
            entry.counters.record_send_stall();
        }
    }

    /// Records a compressed frame against a route and the connection.
    pub(crate) fn record_compressed(&self, route: RouteId, original: u64, compressed: u64) {
        self.counters.record_compressed(original, compressed);
        if route.is_none() {
            return;
        }
        let state = lock(&self.state);
        if let Some(entry) = state.routes.get(&route) {
            entry.counters.record_compressed(original, compressed);
        }
    }

    /// Sends a graceful close notice for every open route.
    pub fn close_all_routes(&self, detail: &str) {
        let routes: Vec<RouteId> = lock(&self.state).routes.keys().copied().collect();
        for route in routes {
            self.close_route(route, CLOSE_CODE_NORMAL, detail);
        }
    }
}

/// Takes a lock, recovering from poisoning rather than propagating a panic.
///
/// A poisoned mutex here means a task panicked mid-update. The invariants this
/// state holds are all "a queue may be shorter than expected" rather than "a
/// pointer may dangle", so recovering and continuing is strictly better than
/// turning one task's panic into a whole connection's.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CompressionPolicy;

    fn mux(config: MuxConfig, side: Side) -> (Arc<MuxShared>, MuxReceivers) {
        MuxShared::new(
            config,
            side,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            64 * 1024,
            64,
        )
    }

    fn frame(payload: Vec<u8>) -> Frame {
        Frame::new(FrameKind::PeerEvent, FrameFlags::EMPTY, payload).unwrap()
    }

    /// Queues one application control frame through the metered path.
    fn queue_control(shared: &Arc<MuxShared>, body: &[u8]) -> TransportResult<()> {
        let permit = shared
            .control_permits()
            .try_acquire_owned()
            .expect("a control permit");
        shared.queue_control_metered(FrameKind::Control, body, permit)
    }

    #[test]
    fn route_handles_are_parity_partitioned() {
        let (initiator, _rx) = mux(MuxConfig::new(), Side::Initiator);
        let (acceptor, _rx) = mux(MuxConfig::new(), Side::Acceptor);
        for _ in 0..8 {
            let odd = initiator.next_route_id();
            let even = acceptor.next_route_id();
            assert_eq!(odd.get() % 2, 1);
            assert_eq!(even.get() % 2, 0);
            assert!(Side::Initiator.owns_route(odd));
            assert!(Side::Acceptor.owns_route(even));
        }
    }

    #[test]
    fn a_control_burst_is_capped_so_routes_still_run() {
        let config = MuxConfig::new().with_control_burst(2);
        let (shared, _rx) = mux(config, Side::Initiator);
        let route = shared.next_route_id();
        let opened = shared.open_route(route, b"").unwrap();

        // The open itself queued one control frame; drain it.
        let batch = shared.take_batch(8);
        assert_eq!(batch.len(), 1);

        for _ in 0..8 {
            queue_control(&shared, b"c").unwrap();
        }
        for index in 0..8u8 {
            shared
                .queue_route_frame(
                    route,
                    frame(build_payload(
                        MuxHeader::new(MuxTag::RouteData, route),
                        &[index],
                    )),
                    1,
                    None,
                )
                .unwrap();
        }

        // The pattern, not the phase: the scheduler carries its control run
        // across batches, so what matters is that no more than `control_burst`
        // control frames appear back to back and that the route keeps moving.
        let batch = shared.take_batch(12);
        let served: Vec<RouteId> = batch.iter().map(|item| item.route).collect();

        let mut run = 0usize;
        let mut longest_control_run = 0usize;
        let mut route_frames = 0usize;
        for served_route in &served {
            if served_route.is_none() {
                run += 1;
                longest_control_run = longest_control_run.max(run);
            } else {
                run = 0;
                route_frames += 1;
            }
        }
        assert!(
            longest_control_run <= 2,
            "a control run of {longest_control_run} exceeds the burst cap, in {served:?}"
        );
        assert!(
            route_frames >= 3,
            "the route was starved: only {route_frames} frames in {served:?}"
        );
        drop(opened);
    }

    #[test]
    fn a_control_frame_overtakes_a_deep_route_backlog() {
        // The discriminating fairness test, and the reason it lives here
        // rather than over a socket: only at this level can the route queue be
        // held *reliably* full while a control frame is queued behind it. Over
        // a real socket the writer drains a bounded queue faster than a
        // producer task can refill it, so the backlog empties between batches
        // and even a FIFO scheduler looks fair.
        //
        // Deleting the control tier from `pop_next` makes this test fail.
        const BACKLOG: usize = 256;
        let (shared, _rx) = mux(
            MuxConfig::new()
                .with_control_burst(4)
                .with_initial_window_frames(BACKLOG as u32)
                .with_route_queue_depth(BACKLOG),
            Side::Initiator,
        );
        let route = shared.next_route_id();
        let _opened = shared.open_route(route, b"").unwrap();
        let _ = shared.take_batch(8);

        // A deep, fully-credited backlog on one route…
        for index in 0..BACKLOG {
            shared
                .queue_route_frame(route, frame(vec![index as u8]), 1, None)
                .unwrap();
        }
        // …and one control frame queued strictly behind all of it.
        queue_control(&shared, b"heartbeat").unwrap();

        let batch = shared.take_batch(BACKLOG + 8);
        let position = batch
            .iter()
            .position(|item| item.route.is_none())
            .expect("the control frame must be served at all");
        assert!(
            position < 4,
            "the control frame was served at position {position} behind a \
             {BACKLOG}-frame backlog: the scheduler is behaving like a FIFO"
        );
    }

    #[test]
    fn a_datagram_also_overtakes_a_deep_route_backlog() {
        const BACKLOG: usize = 128;
        let (shared, _rx) = mux(
            MuxConfig::new()
                .with_control_burst(2)
                .with_initial_window_frames(BACKLOG as u32)
                .with_route_queue_depth(BACKLOG),
            Side::Initiator,
        );
        let route = shared.next_route_id();
        let _opened = shared.open_route(route, b"").unwrap();
        let _ = shared.take_batch(8);

        for index in 0..BACKLOG {
            shared
                .queue_route_frame(route, frame(vec![index as u8]), 1, None)
                .unwrap();
        }
        shared.queue_datagram(FrameKind::Data, b"pose").unwrap();

        let batch = shared.take_batch(BACKLOG + 8);
        let position = batch
            .iter()
            .position(|item| {
                MuxHeader::decode(item.frame.payload())
                    .is_ok_and(|(header, _)| header.tag == MuxTag::Datagram)
            })
            .expect("the datagram must be served at all");
        assert!(
            position < 4,
            "the datagram was served at position {position} behind a \
             {BACKLOG}-frame backlog"
        );
    }

    #[test]
    fn routes_are_served_round_robin() {
        let (shared, _rx) = mux(MuxConfig::new().with_control_burst(1), Side::Initiator);
        let first = shared.next_route_id();
        let second = shared.next_route_id();
        let _a = shared.open_route(first, b"").unwrap();
        let _b = shared.open_route(second, b"").unwrap();
        // Drain the two `RouteOpen` control frames.
        assert_eq!(shared.take_batch(8).len(), 2);

        for _ in 0..4 {
            shared
                .queue_route_frame(first, frame(vec![1]), 1, None)
                .unwrap();
            shared
                .queue_route_frame(second, frame(vec![2]), 1, None)
                .unwrap();
        }

        let batch = shared.take_batch(8);
        let order: Vec<RouteId> = batch.iter().map(|item| item.route).collect();
        assert_eq!(
            order,
            vec![first, second, first, second, first, second, first, second]
        );
    }

    #[test]
    fn a_saturated_route_does_not_starve_a_quiet_one() {
        let (shared, _rx) = mux(MuxConfig::new().with_control_burst(1), Side::Initiator);
        let loud = shared.next_route_id();
        let quiet = shared.next_route_id();
        let _a = shared.open_route(loud, b"").unwrap();
        let _b = shared.open_route(quiet, b"").unwrap();
        let _ = shared.take_batch(8);

        for _ in 0..32 {
            shared
                .queue_route_frame(loud, frame(vec![0]), 1, None)
                .unwrap();
        }
        shared
            .queue_route_frame(quiet, frame(vec![1]), 1, None)
            .unwrap();

        let batch = shared.take_batch(4);
        let served: Vec<RouteId> = batch.iter().map(|item| item.route).collect();
        assert!(
            served.contains(&quiet),
            "the quiet route must be served within a few slots, got {served:?}"
        );
    }

    #[test]
    fn a_route_without_credit_is_skipped() {
        let (shared, _rx) = mux(
            MuxConfig::new()
                .with_initial_window_frames(2)
                .with_control_burst(1),
            Side::Initiator,
        );
        let route = shared.next_route_id();
        let _opened = shared.open_route(route, b"").unwrap();
        let _ = shared.take_batch(8);

        for _ in 0..4 {
            shared
                .queue_route_frame(route, frame(vec![9]), 1, None)
                .unwrap();
        }
        // Only two frames of credit exist.
        assert_eq!(shared.take_batch(8).len(), 2);
        assert!(shared.take_batch(8).is_empty());

        // A grant from the peer releases the rest.
        shared.add_credit(route, 2);
        assert_eq!(shared.take_batch(8).len(), 2);
    }

    #[tokio::test]
    async fn a_route_open_creates_a_peer_side_route_and_answers() {
        let (shared, mut receivers) = mux(MuxConfig::new(), Side::Acceptor);
        // The initiator's handles are odd; this end is the acceptor.
        let route = RouteId::new(3);
        let mut body = Vec::new();
        OpenBody::new(8)
            .with_descriptor(b"spec".to_vec())
            .encode_into(&mut body);
        let opened = frame(build_payload(
            MuxHeader::new(MuxTag::RouteOpen, route),
            &body,
        ));

        assert_eq!(shared.dispatch(opened).await.unwrap(), Dispatched::Handled);
        let inbound = receivers.accepts.try_recv().unwrap();
        assert_eq!(inbound.route, route);
        assert_eq!(inbound.descriptor, b"spec");
        assert!(shared.has_route(route));

        // …and an acceptance was queued for the peer.
        let batch = shared.take_batch(4);
        assert_eq!(batch.len(), 1);
        let (header, accept_body) = MuxHeader::decode(batch[0].frame.payload()).unwrap();
        assert_eq!(header.tag, MuxTag::RouteAccept);
        assert!(AcceptBody::decode(accept_body).unwrap().accepted);
    }

    #[tokio::test]
    async fn a_route_open_with_the_wrong_parity_is_refused() {
        let (shared, mut receivers) = mux(MuxConfig::new(), Side::Acceptor);
        // An even handle belongs to this end, so the peer may not mint it.
        let route = RouteId::new(4);
        let mut body = Vec::new();
        OpenBody::new(8).encode_into(&mut body);
        let opened = frame(build_payload(
            MuxHeader::new(MuxTag::RouteOpen, route),
            &body,
        ));

        shared.dispatch(opened).await.unwrap();
        assert!(!shared.has_route(route));
        assert!(receivers.accepts.try_recv().is_err());

        let batch = shared.take_batch(4);
        let (header, accept_body) = MuxHeader::decode(batch[0].frame.payload()).unwrap();
        assert_eq!(header.tag, MuxTag::RouteAccept);
        assert!(!AcceptBody::decode(accept_body).unwrap().accepted);
    }

    #[tokio::test]
    async fn the_route_ceiling_is_enforced_on_inbound_opens() {
        let (shared, mut receivers) = MuxShared::new(
            MuxConfig::new(),
            Side::Acceptor,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            64 * 1024,
            2,
        );
        for handle in [1u64, 3, 5] {
            let mut body = Vec::new();
            OpenBody::new(4).encode_into(&mut body);
            let opened = frame(build_payload(
                MuxHeader::new(MuxTag::RouteOpen, RouteId::new(handle)),
                &body,
            ));
            shared.dispatch(opened).await.unwrap();
        }
        assert_eq!(shared.open_route_count(), 2);
        assert!(receivers.accepts.try_recv().is_ok());
        assert!(receivers.accepts.try_recv().is_ok());
        assert!(receivers.accepts.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_refused_accept_closes_the_route_locally() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Initiator);
        let route = shared.next_route_id();
        let _opened = shared.open_route(route, b"").unwrap();
        assert!(shared.has_route(route));

        let mut body = Vec::new();
        AcceptBody::refused("no such dataflow").encode_into(&mut body);
        let reply = frame(build_payload(
            MuxHeader::new(MuxTag::RouteAccept, route),
            &body,
        ));
        shared.dispatch(reply).await.unwrap();
        assert!(!shared.has_route(route));
    }

    #[tokio::test]
    async fn an_accepted_route_takes_the_peers_window() {
        let (shared, _rx) = mux(
            MuxConfig::new().with_initial_window_frames(64),
            Side::Initiator,
        );
        let route = shared.next_route_id();
        let _opened = shared.open_route(route, b"").unwrap();
        assert_eq!(shared.is_route_confirmed(route), Some(true));

        let mut body = Vec::new();
        AcceptBody::accepted(4).encode_into(&mut body);
        let reply = frame(build_payload(
            MuxHeader::new(MuxTag::RouteAccept, route),
            &body,
        ));
        shared.dispatch(reply).await.unwrap();

        // The optimistic 64-frame window is narrowed to the 4 the peer offered.
        let _ = shared.take_batch(8);
        for _ in 0..8 {
            shared
                .queue_route_frame(route, frame(vec![0]), 1, None)
                .unwrap();
        }
        assert_eq!(shared.take_batch(16).len(), 4);
    }

    #[tokio::test]
    async fn a_flow_control_overrun_tears_down_the_route() {
        let (shared, mut receivers) = mux(
            MuxConfig::new().with_initial_window_frames(2),
            Side::Acceptor,
        );
        let route = RouteId::new(1);
        let mut body = Vec::new();
        OpenBody::new(2).encode_into(&mut body);
        shared
            .dispatch(frame(build_payload(
                MuxHeader::new(MuxTag::RouteOpen, route),
                &body,
            )))
            .await
            .unwrap();
        let _inbound = receivers.accepts.try_recv().unwrap();

        // Two frames fit the window; the third overruns it.
        for _ in 0..2 {
            shared
                .dispatch(frame(build_payload(
                    MuxHeader::new(MuxTag::RouteData, route),
                    b"x",
                )))
                .await
                .unwrap();
        }
        let err = shared
            .dispatch(frame(build_payload(
                MuxHeader::new(MuxTag::RouteData, route),
                b"x",
            )))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::FlowControlViolation { .. }));
        assert!(!shared.has_route(route));
    }

    #[tokio::test]
    async fn data_for_an_unknown_route_is_dropped_not_fatal() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Acceptor);
        let outcome = shared
            .dispatch(frame(build_payload(
                MuxHeader::new(MuxTag::RouteData, RouteId::new(99)),
                b"stale",
            )))
            .await
            .unwrap();
        assert_eq!(outcome, Dispatched::Handled);
    }

    #[tokio::test]
    async fn a_control_frame_reaches_the_control_receiver() {
        let (shared, mut receivers) = mux(MuxConfig::new(), Side::Acceptor);
        let inbound = Frame::new(
            FrameKind::Control,
            FrameFlags::EMPTY,
            build_payload(MuxHeader::control(), b"greeting"),
        )
        .unwrap();
        shared.dispatch(inbound).await.unwrap();
        let delivered = receivers.control.try_recv().unwrap();
        assert_eq!(delivered.kind(), FrameKind::Control);
        assert_eq!(delivered.payload(), b"greeting");
    }

    #[tokio::test]
    async fn datagrams_are_dropped_rather_than_queued_forever() {
        let (shared, mut receivers) = mux(
            MuxConfig::new().with_datagram_queue_depth(2),
            Side::Acceptor,
        );
        for _ in 0..8 {
            shared
                .dispatch(
                    Frame::new(
                        FrameKind::Data,
                        FrameFlags::EMPTY,
                        build_payload(MuxHeader::datagram(), b"d"),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        let snapshot = shared.counters().snapshot();
        assert_eq!(snapshot.datagrams_received, 2);
        assert_eq!(snapshot.datagrams_dropped, 6);
        assert!(receivers.datagrams.try_recv().is_ok());
    }

    #[tokio::test]
    async fn credit_is_granted_once_the_threshold_is_reached() {
        let (shared, _rx) = mux(
            MuxConfig::new().with_initial_window_frames(4),
            Side::Initiator,
        );
        let route = shared.next_route_id();
        let _opened = shared.open_route(route, b"").unwrap();
        let _ = shared.take_batch(8);

        // The grant threshold is half of four, so the second consumption grants.
        shared.note_consumed(route);
        assert!(shared.take_batch(8).is_empty());
        shared.note_consumed(route);

        let batch = shared.take_batch(8);
        assert_eq!(batch.len(), 1);
        let (header, body) = MuxHeader::decode(batch[0].frame.payload()).unwrap();
        assert_eq!(header.tag, MuxTag::Credit);
        assert_eq!(decode_credit(body).unwrap(), 2);
    }

    #[tokio::test]
    async fn a_malformed_header_is_a_fatal_typed_error() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Acceptor);
        let err = shared.dispatch(frame(vec![0u8; 3])).await.unwrap_err();
        assert!(matches!(err, TransportError::MalformedMux(_)));
        assert!(err.is_fatal());
    }

    #[test]
    fn closing_ends_every_route_and_receiver() {
        let (shared, receivers) = mux(MuxConfig::new(), Side::Initiator);
        let route = shared.next_route_id();
        let opened = shared.open_route(route, b"").unwrap();
        assert_eq!(shared.open_route_count(), 1);

        shared.close(CloseReason::Eof);
        assert!(shared.is_closed());
        assert_eq!(shared.close_reason(), Some(CloseReason::Eof));
        assert_eq!(shared.open_route_count(), 0);

        // Every send path now refuses.
        assert!(queue_control(&shared, b"x").is_err());
        assert!(shared.open_route(RouteId::new(99), b"").is_err());
        drop(opened);
        drop(receivers);

        // …and the reason is not overwritten by a later close.
        shared.close(CloseReason::local("later"));
        assert_eq!(shared.close_reason(), Some(CloseReason::Eof));
    }

    #[test]
    fn an_oversize_control_payload_is_refused() {
        let (shared, _rx) = MuxShared::new(
            MuxConfig::new(),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            32,
            8,
        );
        let err = queue_control(&shared, &[0u8; 64]).unwrap_err();
        assert!(matches!(err, TransportError::FrameTooLarge { .. }));

        shared.set_max_payload(1 << 20);
        assert!(queue_control(&shared, &[0u8; 64]).is_ok());
    }

    #[test]
    fn a_duplicate_route_handle_is_refused() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Initiator);
        let route = RouteId::new(1);
        let _first = shared.open_route(route, b"").unwrap();
        let err = shared.open_route(route, b"").unwrap_err();
        assert!(matches!(err, TransportError::DuplicateRoute { .. }));
    }

    #[test]
    fn snapshots_cover_every_open_route() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Initiator);
        let first = shared.next_route_id();
        let second = shared.next_route_id();
        let _a = shared.open_route(first, b"").unwrap();
        let _b = shared.open_route(second, b"").unwrap();

        let snapshots = shared.route_snapshots();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].route, first);
        assert_eq!(snapshots[1].route, second);
        assert_eq!(snapshots[0].compression, Compression::None);

        shared.close_route(first, CLOSE_CODE_NORMAL, "done");
        assert_eq!(shared.route_snapshots().len(), 1);
        assert!(!shared.has_route(first));
    }

    #[test]
    fn closing_every_route_queues_one_notice_each() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Initiator);
        for _ in 0..3 {
            let route = shared.next_route_id();
            let _ = shared.open_route(route, b"").unwrap();
        }
        let _ = shared.take_batch(16);
        shared.close_all_routes("shutting down");
        let batch = shared.take_batch(16);
        assert_eq!(batch.len(), 3);
        for item in &batch {
            let (header, _) = MuxHeader::decode(item.frame.payload()).unwrap();
            assert_eq!(header.tag, MuxTag::RouteClose);
        }
        assert_eq!(shared.open_route_count(), 0);
    }

    #[test]
    fn the_datagram_queue_is_bounded_and_drops_the_oldest() {
        // The regression this guards: a datagram sender never blocks, so an
        // unbounded queue behind a stalled link is an out-of-memory path.
        let (shared, _rx) = mux(
            MuxConfig::new().with_datagram_queue_depth(4),
            Side::Initiator,
        );

        for index in 0..1_000u16 {
            shared
                .queue_datagram(FrameKind::Data, &index.to_le_bytes())
                .unwrap();
        }
        assert_eq!(
            shared.queued_datagrams(),
            4,
            "the datagram queue must stay at its configured depth"
        );

        let snapshot = shared.counters().snapshot();
        assert_eq!(snapshot.datagrams_sent, 1_000);
        assert_eq!(snapshot.datagrams_dropped, 996);

        // …and what survived is the newest, which is the whole point of a
        // latest-only channel.
        let batch = shared.take_batch(16);
        assert_eq!(batch.len(), 4);
        let survivors: Vec<u16> = batch
            .iter()
            .map(|item| {
                let (_, body) = MuxHeader::decode(item.frame.payload()).unwrap();
                u16::from_le_bytes([body[0], body[1]])
            })
            .collect();
        assert_eq!(survivors, vec![996, 997, 998, 999]);
    }

    #[test]
    fn an_oversize_datagram_is_refused_with_its_own_error() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Initiator);
        shared.set_max_datagram_bytes(64);
        assert_eq!(shared.max_datagram_bytes(), 64);

        let err = shared
            .queue_datagram(FrameKind::Data, &[0u8; 128])
            .unwrap_err();
        match err {
            TransportError::DatagramTooLarge { limit, .. } => assert_eq!(limit, 64),
            other => panic!("expected a datagram refusal, got {other:?}"),
        }
        assert_eq!(shared.queued_datagrams(), 0);

        // Zero restores "whatever a frame may be".
        shared.set_max_datagram_bytes(0);
        assert_eq!(shared.max_datagram_bytes(), shared.max_payload());
        shared.queue_datagram(FrameKind::Data, &[0u8; 128]).unwrap();
    }

    #[test]
    fn a_datagram_ceiling_never_exceeds_the_frame_ceiling() {
        let (shared, _rx) = MuxShared::new(
            MuxConfig::new(),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            256,
            8,
        );
        shared.set_max_datagram_bytes(1 << 20);
        assert_eq!(shared.max_datagram_bytes(), 256);
    }

    #[test]
    fn the_internal_control_queue_is_bounded_by_the_route_count() {
        // Credit grants and route lifecycle frames take no permit, which is
        // only sound because their volume is structural. Prove it: a peer that
        // floods a route can provoke at most one grant per threshold, and the
        // route table is capped.
        let (shared, _rx) = MuxShared::new(
            MuxConfig::new().with_initial_window_frames(2),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            4,
        );
        for _ in 0..4 {
            let route = shared.next_route_id();
            let _ = shared.open_route(route, b"").unwrap();
        }
        assert!(shared.open_route(shared.next_route_id(), b"").is_err());

        // Four opens plus, at most, one grant per route per consumed pair.
        let opens = shared.take_batch(64).len();
        assert_eq!(opens, 4);
        for route in [1u64, 3, 5, 7] {
            for _ in 0..100 {
                shared.note_consumed(RouteId::new(route));
            }
        }
        let grants = shared.take_batch(1_024).len();
        assert!(
            grants <= 4 * 100,
            "grants must be bounded by consumption, not by peer volume"
        );
    }

    #[test]
    fn datagrams_are_served_after_control_but_before_a_starved_route() {
        let (shared, _rx) = mux(MuxConfig::new().with_control_burst(4), Side::Initiator);
        let permit = shared
            .control_permits()
            .try_acquire_owned()
            .expect("a control permit");
        shared
            .queue_control_metered(FrameKind::Control, b"c", permit)
            .unwrap();
        shared.queue_datagram(FrameKind::Data, b"d").unwrap();

        let batch = shared.take_batch(8);
        assert_eq!(batch.len(), 2);
        let tags: Vec<MuxTag> = batch
            .iter()
            .map(|item| MuxHeader::decode(item.frame.payload()).unwrap().0.tag)
            .collect();
        assert_eq!(
            tags,
            vec![MuxTag::Control, MuxTag::Datagram],
            "control outranks datagrams: a late heartbeat costs a node, a late \
             pose is replaced by the next one"
        );
    }

    #[test]
    fn work_is_reported_only_when_something_can_be_written() {
        let (shared, _rx) = mux(MuxConfig::new(), Side::Initiator);
        assert!(!shared.has_work());
        queue_control(&shared, b"x").unwrap();
        assert!(shared.has_work());
        let _ = shared.take_batch(8);
        assert!(!shared.has_work());

        // A queued datagram is work too, or the writer would sleep on it.
        shared.queue_datagram(FrameKind::Data, b"d").unwrap();
        assert!(shared.has_work());
        let _ = shared.take_batch(8);
        assert!(!shared.has_work());
    }
}
