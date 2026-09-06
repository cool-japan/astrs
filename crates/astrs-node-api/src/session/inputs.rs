//! The slow-start route handshake, **consumer** side (blueprint §6.2, §6.3).
//!
//! [`super::routes`] is the producer's half: a `RouteUpgrade` arrives, a
//! [`astrs_shm::Producer`] is opened, and every later publish writes into a
//! ring slot. This module is the other half, and it is the one that makes the
//! plane actually engage: a route is only upgraded once *"all static same-host
//! consumers attached to the ring"* (§6.3), so until a consumer attaches by
//! itself, nothing ever moves off the daemon path.
//!
//! ```text
//!   InputRouteUpgrade{input, source, segment}
//!            │
//!            ▼
//!   SegmentClient::attach ──► astrs_shm::Consumer ──► reader thread
//!            │                                             │
//!            │  doorbell registered with the broker        │ Payload::zero_copy
//!            ▼                                             ▼
//!   the daemon observes the pid in the consumer table   the node's input queue
//!            │
//!            ▼
//!   RouteUpgrade to the producer — §6.3 completes
//! ```
//!
//! # Why attaching early is safe
//!
//! The daemon sends `InputRouteUpgrade` *before* the producer is offered its
//! own upgrade, because the producer's offer is gated on observing this
//! consumer in the segment's consumer table. So a consumer always attaches
//! while the producer is still publishing through the daemon — and the ring is
//! therefore empty. [`astrs_shm::StartPosition::Latest`] puts the cursor at the
//! *next* write, so the transition can neither duplicate a message the daemon
//! already delivered nor skip the first one the ring carries.
//!
//! # What still arrives on the daemon path
//!
//! An upgraded route is not a ring *only*. §6.2 keeps the threshold rule — *"a
//! heap payload ≥ threshold is copied once into a slot; below threshold it
//! rides the UDS control channel"* — so a producer on the shared-memory plane
//! still publishes its small messages through the daemon, and so does one
//! whose ring was momentarily full (§6.2: *never sleep-retry*). Both arrive
//! here as ordinary [`astrs_wire::NodeEvent::Input`] frames on the same input
//! queue. A node sees one stream; [`crate::Payload::is_zero_copy`] is the only
//! thing that differs.
//!
//! The consequence worth stating plainly: on an output that publishes payloads
//! on *both* sides of the threshold, the two carriers have different latencies
//! and the relative order of a large and a small message is not guaranteed.
//! An output whose payloads are uniformly sized — the normal case, since a
//! port carries one type — stays on one carrier and stays in order.
//!
//! # Falling back is a detach
//!
//! Every fatal condition ends the same way: the [`astrs_shm::Consumer`] is
//! dropped, which releases its consumer-table entry, which the daemon's next
//! plane pass observes as a detachment — and a detached consumer downgrades
//! the producer, which resumes publishing through the daemon (§6.3). The node
//! keeps reading the same event stream throughout. Nothing has to be
//! negotiated for the fallback to be correct; it falls out of the handshake.
//!
//! # Holding a sample holds a slot, and that is fine
//!
//! A queued [`crate::Payload`] on the zero-copy plane pins the ring slot it
//! points at, so a node that reads slowly holds slots its producer cannot
//! refill. That sounds like a way to stall a producer, and it is worth being
//! explicit about why it is not.
//!
//! Two bounds close it. The consumer's queue policy (§11.2) caps how many
//! events can be outstanding at once — a `queue_size: 4` input pins at most
//! four slots, whatever the node does — and the producer's reaction to a ring
//! with no free slot is [`astrs_shm::Producer::try_allocate`] failing, which
//! the node API turns into §6.2's documented fallback: publish inline, count
//! `shm_fallback_total`, carry on. Nothing waits, so nothing stalls.
//!
//! An earlier version of this module copied payloads out of their slots once
//! an input's queue passed a threshold, to "protect" the producer. It bought
//! nothing the two bounds above do not already give, and it cost the thing the
//! plane exists for: a consumer that fell briefly behind started copying every
//! frame, which made it slower, which kept it behind. The copy is still
//! available — [`crate::Payload::detach`] — to the node, which knows whether
//! it is about to stash the payload and is the right place for the decision.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use astrs_shm::{
    AttachOptions, Consumer, RecvError, Sample, Segment, SegmentClient, SegmentKey, StartPosition,
};
use astrs_wire::{DataId, DataflowId, Metadata, PortRef, ShmSegmentSpec, WireDecode};

use crate::error::{NodeError, Result};
use crate::events::{Event, QueuedEvent};
use crate::payload::Payload;
use crate::session::SessionShared;
use crate::session::routes::RoutePlane;

/// How long one blocking receive parks on the doorbell before the reader
/// looks at its stop flag again.
///
/// It bounds how long a detach takes to take effect, not how long a message
/// waits: the doorbell wakes the reader the moment the producer commits.
pub const RECV_SLICE: Duration = Duration::from_millis(25);

/// How many consecutive [`RecvError::Lagged`] reports end the attachment.
///
/// A lag means the producer overwrote messages this consumer had not read —
/// possible only under [`astrs_shm::OverflowPolicy::Overwrite`], since the
/// default blocks instead. One is a hiccup and the cursor recovers by itself.
/// This many in a row is a consumer that cannot keep up with the ring at all,
/// and the honest answer is to give the route back to the daemon, whose queue
/// policy (§11.2) is the node's *declared* answer to being behind.
pub const MAX_CONSECUTIVE_LAGS: u32 = 8;

/// Counters describing what the consumer-side plane has done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct InputPlaneStats {
    /// Inputs successfully attached to a ring.
    pub attaches: u64,
    /// Attachments that ended, for any reason.
    pub detaches: u64,
    /// Upgrades that could not be taken up, so the route stayed on the daemon
    /// path (§6.2: never sleep-retry).
    pub refusals: u64,
    /// Samples delivered from a ring.
    pub samples: u64,
    /// Messages the producer overwrote before this consumer read them.
    pub lagged: u64,
}

/// One input's place on the plane.
#[derive(Debug)]
struct InputRoute {
    /// Which plane it reads from right now.
    plane: RoutePlane,
    /// The segment it is attached to, when it is.
    segment: Option<ShmSegmentSpec>,
    /// The live reader, if any.
    reader: Option<ReaderHandle>,
    /// Readers that were asked to stop and have not been joined yet.
    ///
    /// Joining is deferred rather than skipped: a reader must be *gone* before
    /// another attaches to the same segment, or two cursors would deliver the
    /// same message twice. Doing it here rather than in [`InputRouteTable::detach`]
    /// keeps the session's frame loop off a thread join.
    retiring: Vec<ReaderHandle>,
    /// Which incarnation of this input's reader is current.
    ///
    /// A reader that exits stamps the table only if it is still the current
    /// one, so a slow exit cannot clobber a re-attachment that overtook it.
    epoch: u64,
}

impl InputRoute {
    /// A route on the daemon path with no reader.
    const fn daemon() -> Self {
        Self {
            plane: RoutePlane::Daemon,
            segment: None,
            reader: None,
            retiring: Vec::new(),
            epoch: 0,
        }
    }
}

/// What happened to a closure offered to a reader.
///
/// Three outcomes, not two, and the third is the one that matters: a daemon
/// closes a source from more than one place — the producer's `OutputDone` and
/// its session ending — so the same `InputClosed` can arrive twice. Treating
/// the duplicate like an ordinary "the reader would not take it" and queueing
/// it directly puts a closure *in front of* the frames the first one is still
/// waiting behind, which is precisely the loss this hand-off exists to
/// prevent. A duplicate carries no information, so it is discarded.
#[derive(Debug)]
enum ClosureOutcome {
    /// The reader has it and will push it once it has drained.
    Taken,
    /// A closure for this input is already queued for delivery; this one is a
    /// duplicate and is dropped.
    AlreadyPending,
    /// The reader has already exited, so its drain is complete: the caller
    /// should queue the closure itself.
    Finished(QueuedEvent),
}

/// The hand-off of an input's closure to the reader that must deliver it last.
///
/// One mutex covers both fields on purpose. The reader marks itself finished
/// and takes any pending closure in the same critical section, so a caller
/// either wins the race and hands the event over, or loses it and is told to
/// deliver the event itself — never a window where both believe the other
/// will, which would lose a closure and hang a node forever.
#[derive(Debug, Default)]
struct ClosureSlot {
    /// The event to push once the drain finishes.
    pending: Option<QueuedEvent>,
    /// Set by the reader as it exits, under this same lock.
    finished: bool,
}

/// A reader thread and the two ways to end it.
///
/// The distinction is the last message of a stream. A producer that finishes
/// commits its final frame into the ring and *then* tells the daemon, which
/// tells this consumer its input is closed — over the control connection,
/// which can easily overtake a sample still travelling on the ring. A reader
/// that stopped on the closure would drop that frame.
#[derive(Debug)]
struct ReaderHandle {
    /// Set to end the reader without reading anything further.
    stop: Arc<AtomicBool>,
    /// Set to make the reader drain what is resident and *then* end.
    drain: Arc<AtomicBool>,
    /// The input's closure, delivered behind the messages it closes rather
    /// than in front of them.
    closing: Arc<Mutex<ClosureSlot>>,
    /// The thread itself.
    join: JoinHandle<()>,
}

impl ReaderHandle {
    /// Asks the reader to stop at once, without waiting for it.
    ///
    /// For a ring that is gone or replaced: there is nothing left to drain,
    /// and reading a segment the daemon has reclaimed is the one thing §6.2's
    /// generation stamp exists to prevent.
    fn abort(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Asks the reader to drain what is resident and then stop.
    fn signal_drain(&self) {
        self.drain.store(true, Ordering::Release);
    }

    /// Hands the reader the event to push once it has drained.
    fn take_closure(&self, event: QueuedEvent) -> ClosureOutcome {
        let mut slot = lock(&self.closing);
        if slot.finished {
            return ClosureOutcome::Finished(event);
        }
        if slot.pending.is_some() {
            return ClosureOutcome::AlreadyPending;
        }
        slot.pending = Some(event);
        self.signal_drain();
        ClosureOutcome::Taken
    }

    /// Waits for the reader to exit, which is also when its
    /// [`astrs_shm::Consumer`] is dropped and its consumer-table entry freed.
    fn join(self) {
        self.abort();
        // A reader that panicked has still released its entry, because the
        // `Consumer` is dropped by the unwind. There is nothing to report and
        // nothing to retry.
        let _ = self.join.join();
    }
}

/// Every input's plane state, and the reader threads behind the attached ones.
#[derive(Debug, Default)]
pub struct InputRouteTable {
    /// One entry per input the daemon has said anything about.
    routes: Mutex<HashMap<DataId, InputRoute>>,
    /// Counters.
    attaches: AtomicU64,
    detaches: AtomicU64,
    refusals: AtomicU64,
    samples: AtomicU64,
    lagged: AtomicU64,
}

impl InputRouteTable {
    /// An empty table: every input on the daemon path.
    #[must_use]
    pub fn new() -> Self {
        Self {
            routes: Mutex::new(HashMap::new()),
            attaches: AtomicU64::new(0),
            detaches: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            lagged: AtomicU64::new(0),
        }
    }

    /// The plane one input reads from.
    #[must_use]
    pub fn plane(&self, input: &DataId) -> RoutePlane {
        lock(&self.routes)
            .get(input)
            .map_or(RoutePlane::Daemon, |route| route.plane)
    }

    /// The plane every input the daemon has spoken about reads from, sorted.
    #[must_use]
    pub fn planes(&self) -> Vec<(DataId, RoutePlane)> {
        let mut planes: Vec<(DataId, RoutePlane)> = lock(&self.routes)
            .iter()
            .map(|(id, route)| (id.clone(), route.plane))
            .collect();
        planes.sort_by(|left, right| left.0.cmp(&right.0));
        planes
    }

    /// The segment one input is attached to.
    #[must_use]
    pub fn segment(&self, input: &DataId) -> Option<ShmSegmentSpec> {
        lock(&self.routes)
            .get(input)
            .and_then(|route| route.segment.clone())
    }

    /// How many inputs read from a ring right now.
    #[must_use]
    pub fn attached_count(&self) -> usize {
        lock(&self.routes)
            .values()
            .filter(|route| route.plane.is_zero_copy())
            .count()
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> InputPlaneStats {
        InputPlaneStats {
            attaches: self.attaches.load(Ordering::Relaxed),
            detaches: self.detaches.load(Ordering::Relaxed),
            refusals: self.refusals.load(Ordering::Relaxed),
            samples: self.samples.load(Ordering::Relaxed),
            lagged: self.lagged.load(Ordering::Relaxed),
        }
    }

    /// Records one upgrade this node could not take up.
    pub fn count_refusal(&self) {
        self.refusals.fetch_add(1, Ordering::Relaxed);
    }

    /// Attaches `input` to `segment` and starts reading it (§6.3).
    ///
    /// Idempotent for the generation already attached: a repeated offer for
    /// the same incarnation is answered by leaving the working reader alone. A
    /// *different* generation replaces it, which is the producer-restart case
    /// — the old ring is gone and its reader has nothing left to read.
    ///
    /// # Errors
    ///
    /// [`NodeError::Shm`] when the segment cannot be opened, verified or read,
    /// and [`NodeError::Pattern`] when the daemon's segment name does not
    /// describe the producer the event names. Either leaves the input on the
    /// daemon path, which is the honest outcome (§6.2: never sleep-retry).
    pub fn attach(
        &self,
        shared: &Arc<SessionShared>,
        input: &DataId,
        source: &PortRef,
        segment: &ShmSegmentSpec,
    ) -> Result<()> {
        // A repeat of what is already running costs nothing and changes
        // nothing. Checked before the segment is opened, because opening is
        // the expensive half.
        let retiring = {
            let mut routes = lock(&self.routes);
            let route = routes
                .entry(input.clone())
                .or_insert_with(InputRoute::daemon);
            if route.plane.is_zero_copy()
                && route.segment.as_ref().is_some_and(|open| {
                    open.generation == segment.generation && open.name == segment.name
                })
            {
                return Ok(());
            }
            // Anything already running for this input is superseded, and its
            // ring is a previous incarnation's: aborted, not drained.
            if let Some(reader) = route.reader.take() {
                reader.abort();
                route.retiring.push(reader);
            }
            route.plane = RoutePlane::Daemon;
            route.segment = None;
            core::mem::take(&mut route.retiring)
        };
        // Joined outside the lock: an exiting reader takes it to stamp itself
        // detached, and joining while holding it would deadlock.
        for reader in retiring {
            reader.join();
        }

        let (mapped, consumer) = open_consumer(
            shared.dataflow,
            source,
            segment,
            shared.shm_broker.as_deref(),
        )?;
        let stop = Arc::new(AtomicBool::new(false));
        let drain = Arc::new(AtomicBool::new(false));
        let closing: Arc<Mutex<ClosureSlot>> = Arc::new(Mutex::new(ClosureSlot::default()));

        let epoch = {
            let mut routes = lock(&self.routes);
            let route = routes
                .entry(input.clone())
                .or_insert_with(InputRoute::daemon);
            route.epoch = route.epoch.wrapping_add(1);
            route.epoch
        };

        let reader = ReaderContext {
            input: input.clone(),
            source: source.clone(),
            session: Arc::downgrade(shared),
            stop: Arc::clone(&stop),
            drain: Arc::clone(&drain),
            closing: Arc::clone(&closing),
            epoch,
        };
        let join = std::thread::Builder::new()
            .name(format!("astrs-shm-{input}"))
            .spawn(move || reader.run(consumer))
            .map_err(|error| {
                NodeError::Pattern(format!(
                    "could not start the ring reader for `{input}`: {error}"
                ))
            })?;

        let mut routes = lock(&self.routes);
        let route = routes
            .entry(input.clone())
            .or_insert_with(InputRoute::daemon);
        route.plane = RoutePlane::Shm;
        route.segment = Some(segment.clone());
        route.reader = Some(ReaderHandle {
            stop,
            drain,
            closing,
            join,
        });
        // The mapping itself is kept alive by the `Consumer` the reader owns;
        // this handle has done its job.
        drop(mapped);
        self.attaches.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Takes `input` off the ring, if it was on one.
    ///
    /// Returns whether anything was attached. The reader **drains what is
    /// resident** before it exits — §6.2's rule for a producer that goes away
    /// is that the daemon *"marks `closed`, lets consumers drain, and unlinks"*,
    /// and a consumer that stopped reading the instant it was told to detach
    /// would throw away messages the producer had already committed.
    ///
    /// The input is on the daemon path from this instant, so anything the
    /// daemon sends next is delivered inline and nothing arrives twice: the
    /// ring holds only what was written before the detach.
    pub fn detach(&self, input: &DataId) -> bool {
        let mut routes = lock(&self.routes);
        let Some(route) = routes.get_mut(input) else {
            return false;
        };
        let was_attached = route.plane.is_zero_copy();
        route.plane = RoutePlane::Daemon;
        route.segment = None;
        route.epoch = route.epoch.wrapping_add(1);
        if let Some(reader) = route.reader.take() {
            reader.signal_drain();
            route.retiring.push(reader);
        }
        if was_attached {
            self.detaches.fetch_add(1, Ordering::Relaxed);
        }
        was_attached
    }

    /// Delivers `input`'s closure *behind* whatever its ring still holds.
    ///
    /// Returns [`None`] when a reader took the event, and gives it back —
    /// never drops it — when the input is not on a ring and the caller should
    /// queue it itself.
    ///
    /// # Why the closure cannot simply be queued
    ///
    /// A producer that finishes commits its last frame into the ring and only
    /// then tells the daemon, which tells this consumer. That message travels
    /// on the control connection, which routinely overtakes a sample still
    /// being read from the ring — so queueing the closure directly would put
    /// "this input is finished" in front of the message that finished it, and
    /// a node that stops reading on the closure (every node) would lose the
    /// last frame of every stream.
    ///
    /// Handing it to the reader instead makes the ordering structural: the
    /// reader drains, pushes the samples, pushes the closure, and exits.
    pub fn close_input(&self, input: &DataId, event: QueuedEvent) -> Option<QueuedEvent> {
        let mut routes = lock(&self.routes);
        let Some(route) = routes.get_mut(input) else {
            return Some(event);
        };
        if !route.plane.is_zero_copy() {
            // Already off the ring — but perhaps only just, with a reader
            // still draining what the producer committed before it went.
            return Self::hand_to_retiring(route, event);
        }
        let Some(reader) = route.reader.take() else {
            return Some(event);
        };
        let outcome = reader.take_closure(event);
        route.plane = RoutePlane::Daemon;
        route.segment = None;
        route.epoch = route.epoch.wrapping_add(1);
        route.retiring.push(reader);
        self.detaches.fetch_add(1, Ordering::Relaxed);
        match outcome {
            ClosureOutcome::Taken | ClosureOutcome::AlreadyPending => None,
            ClosureOutcome::Finished(event) => Some(event),
        }
    }

    /// Hands `input`'s closure to a reader that is still draining, if there is
    /// one.
    ///
    /// The ordering this exists for: a producer that dies makes the daemon
    /// send *both* an `InputRouteDowngrade` (the ring is going away) and an
    /// `InputClosed` (the stream ended), and the downgrade arrives first — so
    /// by the time the closure gets here the input is already off the ring and
    /// its reader is in [`ReaderHandle`]'s retiring list, draining the last
    /// frames the producer committed. Queueing the closure directly would put
    /// it in front of them.
    ///
    /// Returns [`None`] when a draining reader took it.
    fn hand_to_retiring(route: &mut InputRoute, event: QueuedEvent) -> Option<QueuedEvent> {
        let mut event = event;
        // Most recent first: an older reader is draining an older ring, and
        // this closure belongs behind the newest thing that will be delivered.
        for reader in route.retiring.iter().rev() {
            match reader.take_closure(event) {
                ClosureOutcome::Taken | ClosureOutcome::AlreadyPending => return None,
                ClosureOutcome::Finished(returned) => event = returned,
            }
        }
        Some(event)
    }

    /// Finishes every ring reader, so nothing is still in flight.
    ///
    /// The [`InputRouteTable::close_input`] hand-off orders one input's
    /// closure behind its own ring. [`astrs_wire::NodeEvent::AllInputsClosed`]
    /// has no input to hang off: the daemon sends it straight to the session,
    /// so it arrives on the node's *control* lane, which is served before any
    /// data — and it is terminal
    /// ([`astrs_wire::NodeEvent::is_terminal`]), so a node that winds down on
    /// it (`astrs-record-node` and `examples/rust-pipeline`'s recorder both do)
    /// would drop whatever a ring was still draining.
    ///
    /// So the session drains first and pushes it second. This *does* wait —
    /// bounded by [`RECV_SLICE`] per reader — which is acceptable here and
    /// nowhere else: every input has just been declared finished, so there is
    /// nothing left for the session to be responsive to.
    ///
    /// Returns how many readers were waited for.
    pub fn drain_all(&self) -> usize {
        let readers: Vec<ReaderHandle> = {
            let mut routes = lock(&self.routes);
            let mut readers = Vec::new();
            for route in routes.values_mut() {
                if route.plane.is_zero_copy() {
                    self.detaches.fetch_add(1, Ordering::Relaxed);
                }
                route.plane = RoutePlane::Daemon;
                route.segment = None;
                route.epoch = route.epoch.wrapping_add(1);
                if let Some(reader) = route.reader.take() {
                    readers.push(reader);
                }
                readers.append(&mut route.retiring);
            }
            for reader in &readers {
                reader.signal_drain();
            }
            readers
        };
        let waited = readers.len();
        // Joined outside the lock: an exiting reader takes it to stamp itself
        // detached, and joining while holding it would deadlock. `join` also
        // sets the abort flag, which a reader that has finished draining has
        // already passed — it ends the wait for one that is parked with an
        // empty ring rather than cutting a drain short.
        for reader in readers {
            reader.join();
        }
        waited
    }

    /// Detaches every input and waits for every reader to exit.
    ///
    /// Called when the session ends. Waiting matters: a reader still holding a
    /// [`astrs_shm::Sample`] holds a ring slot, and a process that exits
    /// without releasing its consumer-table entry leaves the daemon to reclaim
    /// it on a timeout instead of immediately.
    pub fn close(&self) {
        let readers: Vec<ReaderHandle> = {
            let mut routes = lock(&self.routes);
            let mut readers = Vec::new();
            for route in routes.values_mut() {
                if route.plane.is_zero_copy() {
                    self.detaches.fetch_add(1, Ordering::Relaxed);
                }
                route.plane = RoutePlane::Daemon;
                route.segment = None;
                route.epoch = route.epoch.wrapping_add(1);
                if let Some(reader) = route.reader.take() {
                    readers.push(reader);
                }
                readers.append(&mut route.retiring);
            }
            for reader in &readers {
                reader.abort();
            }
            readers
        };
        for reader in readers {
            reader.join();
        }
    }

    /// Marks one input back on the daemon path from its own reader thread.
    ///
    /// Ignored when `epoch` is not the current one: a reader that took a while
    /// to notice its stop flag must not undo the attachment that replaced it.
    fn mark_detached(&self, input: &DataId, epoch: u64) {
        let mut routes = lock(&self.routes);
        let Some(route) = routes.get_mut(input) else {
            return;
        };
        if route.epoch != epoch {
            return;
        }
        if route.plane.is_zero_copy() {
            self.detaches.fetch_add(1, Ordering::Relaxed);
        }
        route.plane = RoutePlane::Daemon;
        route.segment = None;
    }

    /// Records one sample delivered from a ring.
    fn count_sample(&self) {
        self.samples.fetch_add(1, Ordering::Relaxed);
    }

    /// Records messages the producer overwrote before they were read.
    fn count_lagged(&self, missed: u64) {
        self.lagged.fetch_add(missed, Ordering::Relaxed);
    }
}

/// Everything one reader thread needs, so the thread body is a method and not
/// a seven-argument function.
struct ReaderContext {
    /// The input being read.
    input: DataId,
    /// The producer port behind it, which every delivered event names.
    source: PortRef,
    /// The session to deliver into.
    ///
    /// Weak because the table this reader is registered in lives *inside* the
    /// session: a strong reference would keep it alive forever.
    session: Weak<SessionShared>,
    /// Set when the reader should end at once, reading nothing further.
    stop: Arc<AtomicBool>,
    /// Set when the reader should drain what is resident and then end.
    drain: Arc<AtomicBool>,
    /// The input's closure, to push once the drain finishes.
    closing: Arc<Mutex<ClosureSlot>>,
    /// Which incarnation of this input's reader this is.
    epoch: u64,
}

impl ReaderContext {
    /// Reads until told to stop, the session ends, or the ring does.
    fn run(self, mut consumer: Consumer) {
        let mut consecutive_lags = 0u32;
        while !self.stop.load(Ordering::Acquire) {
            let Some(session) = self.session.upgrade() else {
                break;
            };
            if session.is_closed() {
                break;
            }
            if self.drain.load(Ordering::Acquire) {
                self.finish(&session, &mut consumer);
                break;
            }
            match consumer.next_blocking(RECV_SLICE) {
                Ok(sample) => {
                    consecutive_lags = 0;
                    self.deliver(&session, sample);
                }
                Err(RecvError::Empty) => {}
                Err(RecvError::Lagged(missed)) => {
                    session.inputs.count_lagged(missed);
                    consecutive_lags += 1;
                    if consecutive_lags >= MAX_CONSECUTIVE_LAGS {
                        session.source.push_control(Event::Error(format!(
                            "input `{}` fell behind its ring {MAX_CONSECUTIVE_LAGS} times in a \
                             row; reading it through the daemon instead",
                            self.input
                        )));
                        break;
                    }
                }
                // The producer finished, or its segment was reclaimed: both
                // are the daemon's business to report, and it does — as
                // `InputClosed` or `InputRouteDowngrade` on the control
                // connection. Ending quietly here avoids saying it twice.
                Err(RecvError::Closed) => break,
                // `RecvError::Shm` and — since the enum is `#[non_exhaustive]`
                // — any outcome a future `astrs-shm` adds: a condition this
                // build cannot classify is treated as fatal for the
                // attachment, which costs the plane and keeps the data.
                Err(error) => {
                    session.source.push_control(Event::Error(format!(
                        "input `{}` left the shared-memory plane: {error}",
                        self.input
                    )));
                    break;
                }
            }
        }

        // Dropping the consumer releases its consumer-table entry, which is
        // what the daemon observes as a detachment — and a detached consumer
        // downgrades the producer back onto the daemon path (§6.3). The
        // fallback needs no message of its own.
        drop(consumer);
        // Marked finished and drained of any pending closure in one critical
        // section, so a `close_input` racing this exit is told to deliver the
        // closure itself rather than handing it to a thread that is gone.
        let closure = {
            let mut slot = lock(&self.closing);
            slot.finished = true;
            slot.pending.take()
        };
        if let Some(session) = self.session.upgrade() {
            if let Some(closure) = closure {
                session.source.push_input(&self.input, closure);
            }
            session.inputs.mark_detached(&self.input, self.epoch);
        }
    }

    /// Delivers everything still resident in the ring, then the closure.
    ///
    /// Bounded by construction: a ring holds at most `slot_count` messages and
    /// nothing is being written into it any more, so the loop ends at the
    /// first `Empty`. A lag report is not a reason to stop draining — the
    /// messages behind the gap are still there and still wanted.
    ///
    /// The closure itself is pushed by [`ReaderContext::run`]'s exit path,
    /// which is where the hand-off is settled against a concurrent
    /// [`InputRouteTable::close_input`].
    fn finish(&self, session: &Arc<SessionShared>, consumer: &mut Consumer) {
        loop {
            match consumer.try_next() {
                Ok(sample) => self.deliver(session, sample),
                Err(RecvError::Lagged(missed)) => session.inputs.count_lagged(missed),
                // `Empty`, `Closed`, or a fault: in every case the ring has
                // nothing more to give this consumer.
                Err(_) => break,
            }
        }
    }

    /// Turns one sample into a queued input event.
    fn deliver(&self, session: &Arc<SessionShared>, sample: Sample) {
        let metadata = slot_metadata(sample.metadata(), session);
        // Fold the producer's clock reading into ours, exactly as the daemon
        // path does, so causal order survives the hop (§4.3).
        let _accepted = session.observe_remote(metadata.timestamp);

        // Queued as it lies: the slot stays pinned until the node drops the
        // payload, which is what makes reading it free. See the module
        // documentation for why that cannot stall the producer.
        let payload = Payload::zero_copy(sample);
        session.source.push_input(
            &self.input,
            QueuedEvent::Input {
                source: self.source.clone(),
                metadata,
                payload,
            },
        );
        session.inputs.count_sample();
    }
}

/// The metadata a slot carried, or a fresh stamp when it carried none.
///
/// The producer commits its [`Metadata`] beside the payload (§6.1, §6.2) and it
/// is not replaceable: `request_id`, `goal_id`, `seq` and `_schema_hash` are
/// what make a service exchange correlate (§9.4) and a schema drift detectable.
/// Re-stamping would deliver the bytes and lose the message — so it happens
/// only when there is nothing to lose.
fn slot_metadata(bytes: &[u8], session: &Arc<SessionShared>) -> Metadata {
    if bytes.is_empty() {
        return Metadata::new(session.hlc_now());
    }
    Metadata::decode_exact(bytes).unwrap_or_else(|_| Metadata::new(session.hlc_now()))
}

/// Opens the segment an
/// [`InputRouteUpgrade`](astrs_wire::NodeEvent::InputRouteUpgrade) names and
/// attaches a reader to it.
///
/// The mirror of [`crate::session::routes::open_producer`], and it checks the
/// same two things: the name the daemon sent must be the one the *producer's*
/// `(dataflow, node, output, generation)` describes, and the mapped header's
/// digest and generation must match that key. A stale ring from a previous
/// incarnation therefore cannot be read, which is §6.2's crash-safety property
/// from the consumer's side.
///
/// # The doorbell
///
/// A consumer in a different process from its producer cannot be woken by the
/// producer directly — the two share a mapping, not a descriptor table. The
/// broker is the go-between: [`astrs_shm::SegmentClient::register_doorbell`]
/// passes this consumer's ringer over the same Unix socket that carried the
/// segment, and the producer picks it up from the shared registry on its next
/// commit. Without it a receive still works, but only by waiting out
/// `astrs-shm`'s growing fallback slice — correct, and needlessly slow.
///
/// # Errors
///
/// [`NodeError::Shm`] when the segment cannot be opened, verified or attached
/// to, and [`NodeError::Pattern`] when the daemon's name does not describe the
/// producer port the event names.
pub fn open_consumer(
    dataflow: DataflowId,
    source: &PortRef,
    segment: &ShmSegmentSpec,
    broker: Option<&Path>,
) -> Result<(Arc<Segment>, Consumer)> {
    let key = SegmentKey::new(
        dataflow,
        source.node().clone(),
        source.port().clone(),
        segment.generation,
    );
    let canonical = key.canonical();
    if canonical != segment.name {
        return Err(NodeError::Pattern(format!(
            "input route upgrade names segment `{}`, but `{source}` at generation {} is \
             `{canonical}`",
            segment.name, segment.generation,
        )));
    }

    let mut client = match broker {
        Some(path) => Some(SegmentClient::connect(path)?),
        None => None,
    };
    let mapped = match client.as_mut() {
        Some(client) => client.attach(&key)?,
        // No broker: the in-process harness and any embedder that disabled the
        // plane socket. Only a named backing can be opened this way, which is
        // exactly what those two use.
        None => Segment::open_named(&key.os_name(), Some(&key))?,
    }
    .shared();

    let consumer = Consumer::attach(
        Arc::clone(&mapped),
        AttachOptions::new()
            // The next write, not the oldest resident one: everything already
            // in the ring — if anything is — was published before this route
            // moved, and the daemon delivered it inline.
            .with_start(StartPosition::Latest)
            .with_doorbell(true)
            .expecting(key.clone()),
    )?;

    if let Some(client) = client.as_mut()
        && let Some(doorbell) = consumer.doorbell()
    {
        let ringer = doorbell.ringer()?;
        // A broker that will not take the ringer costs latency, never
        // correctness: `next_blocking` falls back to a bounded wait slice.
        if let Err(error) =
            client.register_doorbell(&key, consumer.index(), consumer.token(), ringer)
        {
            tracing::debug!(%error, segment = %canonical, "the broker refused a doorbell");
        }
    }

    Ok((mapped, consumer))
}

/// Locks a mutex, recovering from a poisoning panic elsewhere.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{DataflowId, NodeId};

    fn input() -> DataId {
        DataId::new("frames").unwrap()
    }

    fn source() -> PortRef {
        PortRef::from_parts("camera", "image").unwrap()
    }

    #[test]
    fn a_fresh_input_is_on_the_daemon_path() {
        let table = InputRouteTable::new();
        assert_eq!(table.plane(&input()), RoutePlane::Daemon);
        assert_eq!(table.segment(&input()), None);
        assert_eq!(table.attached_count(), 0);
        assert!(table.planes().is_empty());
        assert_eq!(table.stats(), InputPlaneStats::default());
    }

    #[test]
    fn detaching_an_unknown_input_is_a_no_op() {
        let table = InputRouteTable::new();
        assert!(!table.detach(&input()));
        table.close();
        assert_eq!(table.stats().detaches, 0);
    }

    #[test]
    fn refusals_are_counted() {
        let table = InputRouteTable::new();
        table.count_refusal();
        assert_eq!(table.stats().refusals, 1);
    }

    #[test]
    fn a_segment_name_that_does_not_match_the_producer_is_refused() {
        let spec = ShmSegmentSpec::new("astrs-not-this-segment", 3, 8, 4096);
        let error = open_consumer(DataflowId::from_u128(1), &source(), &spec, None).unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
        assert!(error.to_string().contains("camera/image"));
    }

    #[test]
    fn the_canonical_producer_name_is_what_the_daemon_sends() {
        let dataflow = DataflowId::from_u128(0x9001);
        let key = SegmentKey::new(
            dataflow,
            NodeId::new("camera").unwrap(),
            DataId::new("image").unwrap(),
            5,
        );
        assert!(key.canonical().ends_with("/camera/image/5"));
        // The hashed `shm_open` form never travels, so it is refused.
        let hashed = ShmSegmentSpec::new(key.os_name().as_str(), 5, 8, 4096);
        assert!(open_consumer(dataflow, &source(), &hashed, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_real_segment_is_opened_verified_and_read_from() {
        use astrs_shm::{Backing, Producer, SegmentConfig};

        let dataflow = DataflowId::from_u128((u128::from(std::process::id()) << 32) | 0x9002);
        let key = SegmentKey::new(
            dataflow,
            NodeId::new("camera").unwrap(),
            DataId::new("image").unwrap(),
            1,
        );
        let config = SegmentConfig::new(4, 4096)
            .unwrap()
            .with_backing(Backing::Named);
        let Ok(segment) = Segment::create(key.clone(), config) else {
            // A sandbox without a usable shared-memory backing is not a test
            // failure; the plane is an optimisation (§6.2).
            return;
        };
        let segment = segment.shared();
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();

        let spec = ShmSegmentSpec::new(key.canonical(), 1, 4, 4096);
        let (mapped, mut consumer) =
            open_consumer(dataflow, &source(), &spec, None).expect("the segment opens");
        assert_eq!(mapped.layout().slot_count(), 4);

        // `Latest` means the next write, so a message published *after* the
        // attach is the first one seen.
        producer.send(b"after", b"").unwrap();
        let sample = consumer.next_blocking(Duration::from_secs(2)).unwrap();
        assert_eq!(sample.payload(), b"after");

        // A generation this node does not believe in is refused before any
        // mapping happens.
        let stale = ShmSegmentSpec::new(key.canonical(), 2, 4, 4096);
        assert!(open_consumer(dataflow, &source(), &stale, None).is_err());

        drop(sample);
        drop(consumer);
        let _ = segment.unlink();
    }

    /// A session with one input, `frames`, fed by `camera/image`.
    #[cfg(unix)]
    fn session(
        dataflow: DataflowId,
    ) -> (
        Arc<SessionShared>,
        tokio::sync::mpsc::Receiver<crate::session::Outgoing>,
    ) {
        use astrs_wire::{FrameLimits, InputSpec, NodeSource, NodeSpawnSpec, SessionId};

        let spec = Arc::new(
            NodeSpawnSpec::new(
                dataflow,
                NodeId::new("detect").unwrap(),
                0,
                NodeSource::Dynamic,
            )
            .with_input(InputSpec::new(input(), source())),
        );
        let events = Arc::new(crate::events::EventSource::new());
        for declared in &spec.inputs {
            events.register_input(declared).unwrap();
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(crate::session::OUTGOING_CAPACITY);
        let shared = Arc::new(SessionShared::new(
            spec,
            SessionId::from_u128(11),
            sender,
            events,
            crate::runtime::NodeRuntime::owned().unwrap(),
            4096,
            FrameLimits::uds(),
        ));
        (shared, receiver)
    }

    /// A named segment for `camera/image` at `generation`, plus its producer.
    #[cfg(unix)]
    fn ring(
        dataflow: DataflowId,
        generation: u64,
    ) -> Option<(
        SegmentKey,
        Arc<Segment>,
        astrs_shm::Producer,
        ShmSegmentSpec,
    )> {
        use astrs_shm::{Backing, Producer, SegmentConfig};

        let key = SegmentKey::new(
            dataflow,
            NodeId::new("camera").unwrap(),
            DataId::new("image").unwrap(),
            generation,
        );
        let config = SegmentConfig::new(4, 4096)
            .unwrap()
            .with_backing(Backing::Named);
        let segment = Segment::create(key.clone(), config).ok()?.shared();
        let producer = Producer::new(Arc::clone(&segment)).ok()?;
        let spec = ShmSegmentSpec::new(key.canonical(), generation, 4, 4096);
        Some((key, segment, producer, spec))
    }

    #[cfg(unix)]
    #[test]
    fn a_new_generation_replaces_an_attachment_rather_than_being_ignored() {
        // The producer-restart case, which an "already attached, ignore" early
        // return would wedge permanently: the node would keep reading a ring
        // whose producer has gone and never see the one that replaced it.
        let dataflow = DataflowId::from_u128((u128::from(std::process::id()) << 32) | 0x9004);
        let Some((_key_one, first, _producer_one, spec_one)) = ring(dataflow, 1) else {
            return;
        };
        let Some((_key_two, second, mut producer_two, spec_two)) = ring(dataflow, 2) else {
            let _ = first.unlink();
            return;
        };
        let (shared, _outgoing) = session(dataflow);

        shared
            .inputs
            .attach(&shared, &input(), &source(), &spec_one)
            .expect("the first ring attaches");
        assert_eq!(shared.inputs.plane(&input()), RoutePlane::Shm);
        assert_eq!(
            shared.inputs.segment(&input()).map(|s| s.generation),
            Some(1)
        );
        assert_eq!(shared.inputs.stats().attaches, 1);

        // The same offer again is silence, not churn: re-attaching would drop
        // the reader mid-message for nothing.
        shared
            .inputs
            .attach(&shared, &input(), &source(), &spec_one)
            .expect("a repeated offer is accepted");
        assert_eq!(shared.inputs.stats().attaches, 1, "no second attachment");

        // A different generation is a genuinely different ring.
        shared
            .inputs
            .attach(&shared, &input(), &source(), &spec_two)
            .expect("the second ring attaches");
        assert_eq!(shared.inputs.stats().attaches, 2);
        assert_eq!(
            shared.inputs.segment(&input()).map(|s| s.generation),
            Some(2)
        );
        assert_eq!(
            shared.inputs.planes(),
            vec![(input(), RoutePlane::Shm)],
            "the plane is reported per input"
        );

        // And it is the *new* ring being read.
        producer_two.send(b"second-incarnation", b"").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut delivered = None;
        while std::time::Instant::now() < deadline && delivered.is_none() {
            if let Some(crate::events::Event::Input { id, data, .. }) = shared.source.try_next() {
                assert_eq!(id, input());
                delivered = Some(data);
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let payload = delivered.expect("a sample from the second ring");
        assert!(payload.is_zero_copy());
        assert_eq!(payload.bytes(), b"second-incarnation");
        let slot = payload.slot().expect("a ring slot");
        assert!(
            slot.is_inside_mapping() && slot.is_at_layout_offset(),
            "{slot:?}"
        );
        drop(payload);

        assert!(shared.inputs.detach(&input()));
        assert_eq!(shared.inputs.plane(&input()), RoutePlane::Daemon);
        assert!(!shared.inputs.detach(&input()), "a detach happens once");

        shared.inputs.close();
        let _ = first.unlink();
        let _ = second.unlink();
    }

    #[cfg(unix)]
    #[test]
    fn latest_skips_what_the_daemon_already_delivered() {
        use astrs_shm::{Backing, Producer, SegmentConfig};

        let dataflow = DataflowId::from_u128((u128::from(std::process::id()) << 32) | 0x9003);
        let key = SegmentKey::new(
            dataflow,
            NodeId::new("camera").unwrap(),
            DataId::new("image").unwrap(),
            1,
        );
        let config = SegmentConfig::new(4, 4096)
            .unwrap()
            .with_backing(Backing::Named);
        let Ok(segment) = Segment::create(key.clone(), config) else {
            return;
        };
        let segment = segment.shared();
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        producer.send(b"before", b"").unwrap();

        let spec = ShmSegmentSpec::new(key.canonical(), 1, 4, 4096);
        let (_mapped, mut consumer) = open_consumer(dataflow, &source(), &spec, None).unwrap();
        assert!(
            matches!(consumer.try_next(), Err(RecvError::Empty)),
            "a message published before the attach is not replayed"
        );
        producer.send(b"after", b"").unwrap();
        let sample = consumer.next_blocking(Duration::from_secs(2)).unwrap();
        assert_eq!(sample.payload(), b"after");

        drop(sample);
        drop(consumer);
        let _ = segment.unlink();
    }
}
