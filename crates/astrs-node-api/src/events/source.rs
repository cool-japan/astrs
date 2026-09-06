//! [`EventSource`] — the shared state between the session's reader task and
//! the node's [`EventStream`](super::EventStream).
//!
//! One `Arc<EventSource>` sits between them. The reader pushes; the stream
//! pops; neither knows about the other. That separation is what lets the
//! testing harness feed a stream without a socket, and what lets a node drop
//! its stream (which the session notices and reports to the daemon as
//! [`astrs_wire::NodeRequest::EventStreamDropped`], §7.3) without racing the
//! reader.
//!
//! # Priority and fairness
//!
//! [`EventSource::try_next`] drains the control deque first and only then
//! asks the [`EventMux`] for data — blueprint §11.3's "control lane
//! pre-empts data lane", applied at the one point where both lanes meet.
//! Within the data lane the mux's own round robin keeps a hot input from
//! starving a quiet one.
//!
//! # Counters
//!
//! Every drop is counted, and every drop *signal* from the queue is turned
//! into an [`Event::Error`] on the control lane, because blueprint §11.2 says
//! a backpressure exhaustion is an ERROR-log-and-metric event rather than a
//! silent loss. It is also written to the process log as a
//! `tracing::error!` — the control-lane `Event::Error` only reaches a node
//! that is reading its own event stream, while the process log reaches
//! whatever is watching `astrs/logs/*` or stderr — throttled to once per
//! input per `DROP_LOG_WINDOW` rather than once per dropped message: see
//! `EventSource::log_drop_signal`, this module's own (private) rate limiter.
//!
//! # Deadlines (§11.3)
//!
//! An input whose [`InputSpec::deadline`] is set gets an
//! [`astrs_scheduler::DeadlineMonitor`] entry alongside its queue
//! registration. Every *data* event [`EventSource::try_next`] hands to the
//! node opens a measurement for that input, superseding (not queuing behind)
//! any measurement already open for the same input — the newest arrival is
//! what a node's next publish is judged against, the same "the newest state
//! is what matters" reasoning [`QueuePolicy::DropOldest`] applies to the
//! queue itself, and what keeps the open set bounded by the node's own input
//! count rather than by how long it has been running. [`EventSource::finish_deadlines`]
//! is the other half: called once per publish (see
//! [`crate::session::SessionShared::send_request`]), it closes every open
//! measurement against `now` and turns a [`astrs_scheduler::DeadlineOutcome::Violated`]
//! into an `Event::Error` on the control lane — `describe_signal`'s §11.2
//! pattern, mirrored for §11.3.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use astrs_scheduler::{
    DeadlineMonitor, DeadlineOutcome, DeadlineToken, EventMux, InputHandle, PushOutcome,
    QueueSignal, QueueSnapshot,
};
use astrs_wire::{DataId, InputSpec, PriorityLane, QueuePolicy};

use crate::error::Result;
use crate::events::{Event, QueuedEvent};
use crate::signal::Signal;

/// How often, per input, a queue drop signal is allowed to reach the process
/// log as a `tracing::error!` (§11.2).
///
/// The in-process [`Event::Error`] pushed alongside it is unthrottled — a
/// node's own event stream is a bounded channel the node already reads at
/// its own pace, and every entry on it is one already-accounted-for drop,
/// not a new one. The process log is different: a full stack trace of
/// `ERROR` lines, one per dropped message, during a sustained overload would
/// itself become an unbounded-write problem sitting right behind the one it
/// was reporting.
const DROP_LOG_WINDOW: Duration = Duration::from_secs(5);

/// Counters a node (and `astrs list`) can read off a live stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct StreamStats {
    /// Events delivered to the node.
    pub delivered: u64,
    /// Data-lane events dropped by a queue policy.
    pub dropped: u64,
    /// Control-lane events currently waiting.
    pub control_depth: u64,
    /// Times a queue reported a condition worth logging (§11.2).
    pub queue_signals: u64,
    /// Times an input's `InputSpec::deadline` was exceeded (§11.3).
    pub deadline_violations: u64,
}

/// The shared inbox behind an [`EventStream`](super::EventStream).
pub struct EventSource {
    /// The per-input bounded queues, priority-and-fairness multiplexed.
    mux: EventMux<QueuedEvent>,
    /// Producer-side handles, one per registered input.
    inputs: Mutex<HashMap<DataId, InputHandle<QueuedEvent>>>,
    /// The unbounded control lane.
    control: Mutex<VecDeque<Event>>,
    /// Wakes both blocking and async waiters.
    signal: Signal,
    /// Set when the session ends; no further events can arrive.
    closed: AtomicBool,
    /// Set when the node dropped its stream.
    abandoned: AtomicBool,
    /// Delivered counter.
    delivered: AtomicU64,
    /// Dropped counter.
    dropped: AtomicU64,
    /// Queue-signal counter.
    signals: AtomicU64,
    /// Run once when the node drops its stream (§7.3 `EventStreamDropped`).
    abandon_hook: Mutex<Option<AbandonHook>>,
    /// Per-input latency budgets declared via [`InputSpec::deadline`]
    /// (§11.3). Registration is a no-op for an input with none — see
    /// [`astrs_scheduler::DeadlineMonitor::register_from_spec`].
    deadlines: DeadlineMonitor<DataId>,
    /// The measurement currently open for each input that has one, keyed by
    /// input — see this module's docs on why a later arrival supersedes
    /// rather than queues behind an earlier one.
    open_deadlines: Mutex<HashMap<DataId, DeadlineToken<DataId>>>,
    /// Deadline-violation counter.
    deadline_violations: AtomicU64,
    /// The last instant each input's queue-drop signal actually reached the
    /// process log, for [`DROP_LOG_WINDOW`] throttling — see
    /// [`EventSource::log_drop_signal`].
    last_drop_log: Mutex<HashMap<DataId, Instant>>,
    /// Run once per violation [`EventSource::finish_deadlines`] finds
    /// (§11.3), so the session can relay it to the daemon as
    /// [`astrs_wire::NodeRequest::ReportDeadlineViolation`] — the
    /// [`AbandonHook`] pattern, for the daemon-relay half of a deadline
    /// violation rather than its local [`Event::Error`], which
    /// `finish_deadlines` still pushes unconditionally.
    deadline_violation_hook: Mutex<Option<DeadlineViolationHook>>,
}

/// What runs when a node drops its event stream.
///
/// The session installs one that sends
/// [`astrs_wire::NodeRequest::EventStreamDropped`], which is what stops the
/// daemon growing a queue for a node that stopped reading — dora's lesson,
/// kept (§7.3).
pub type AbandonHook = Box<dyn Fn() + Send + Sync>;

/// What runs when [`EventSource::finish_deadlines`] finds a violation
/// (§11.3): the input it was on, the budget, and the measured latency —
/// everything [`astrs_wire::NodeRequest::ReportDeadlineViolation`] needs.
///
/// Not installed by default: without a hook, a violation is still counted
/// ([`EventSource::stats`]) and still reported on the node's own control
/// lane via [`Event::Error`] — this hook is *only* the daemon-relay half
/// (§11.3's `astrs/status` fan-out and registered metric), which needs a live
/// session to actually send anything over.
pub type DeadlineViolationHook = Box<dyn Fn(DataId, Duration, Duration) + Send + Sync>;

// Hand-written, and deliberately a summary: a payload can be megabytes, so a
// `Debug` that tried to render queued events would be unusable exactly when a
// node is being debugged. What a caller needs is which inputs exist, what the
// counters say, and whether the session is still live.
impl core::fmt::Debug for EventSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EventSource")
            .field("inputs", &self.input_ids())
            .field("stats", &self.stats())
            .field("closed", &self.is_closed())
            .field("abandoned", &self.is_abandoned())
            .finish_non_exhaustive()
    }
}

impl Default for EventSource {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSource {
    /// An empty source with no inputs registered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mux: EventMux::new(),
            inputs: Mutex::new(HashMap::new()),
            control: Mutex::new(VecDeque::new()),
            signal: Signal::new(),
            closed: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            signals: AtomicU64::new(0),
            abandon_hook: Mutex::new(None),
            deadlines: DeadlineMonitor::new(),
            open_deadlines: Mutex::new(HashMap::new()),
            deadline_violations: AtomicU64::new(0),
            last_drop_log: Mutex::new(HashMap::new()),
            deadline_violation_hook: Mutex::new(None),
        }
    }

    /// Installs the hook that runs when the node drops its stream.
    ///
    /// Replaces any previous one; the session sets exactly one.
    pub fn set_abandon_hook(&self, hook: AbandonHook) {
        *lock(&self.abandon_hook) = Some(hook);
    }

    /// Installs the hook that runs once per deadline violation
    /// [`EventSource::finish_deadlines`] finds (§11.3).
    ///
    /// Replaces any previous one; the session sets exactly one — see
    /// [`DeadlineViolationHook`].
    pub fn set_deadline_violation_hook(&self, hook: DeadlineViolationHook) {
        *lock(&self.deadline_violation_hook) = Some(hook);
    }

    /// The wakeup both faces of the stream wait on.
    #[must_use]
    pub const fn signal(&self) -> &Signal {
        &self.signal
    }

    /// Registers an input with the queue size, policy and lane its manifest
    /// declares (§11.2, §11.3).
    ///
    /// Also registers `spec`'s input-to-output latency budget, when it
    /// declares one ([`InputSpec::deadline`]) — a no-op, harmlessly, for the
    /// common case of an input with none.
    ///
    /// # Errors
    ///
    /// [`crate::NodeError::Scheduler`] when the input is already registered or the
    /// declared capacity is zero.
    pub fn register_input(&self, spec: &InputSpec) -> Result<()> {
        let handle = self.mux.register_from_spec(spec)?;
        let _previous = lock(&self.inputs).insert(spec.id.clone(), handle);
        let _has_deadline = self.deadlines.register_from_spec(spec.id.clone(), spec);
        Ok(())
    }

    /// Registers an input explicitly, for a dynamic node with no manifest
    /// specification to read from.
    ///
    /// # Errors
    ///
    /// As [`EventSource::register_input`].
    pub fn register_raw(
        &self,
        id: DataId,
        capacity: u32,
        policy: QueuePolicy,
        lane: PriorityLane,
    ) -> Result<()> {
        let handle = self
            .mux
            .register_input(id.clone(), capacity, policy, lane)?;
        let _previous = lock(&self.inputs).insert(id, handle);
        Ok(())
    }

    /// Whether `id` is a registered input.
    #[must_use]
    pub fn has_input(&self, id: &DataId) -> bool {
        lock(&self.inputs).contains_key(id)
    }

    /// The registered input ids.
    #[must_use]
    pub fn input_ids(&self) -> Vec<DataId> {
        let mut ids: Vec<DataId> = lock(&self.inputs).keys().cloned().collect();
        ids.sort_unstable();
        ids
    }

    /// A point-in-time reading of one input's queue.
    #[must_use]
    pub fn queue_snapshot(&self, id: &DataId) -> Option<QueueSnapshot> {
        self.mux.queue_snapshot(id)
    }

    /// Pushes a data-lane event onto `id`'s queue, applying its policy.
    ///
    /// An unregistered input is reported on the control lane rather than
    /// silently dropped: it means the daemon and the node disagree about the
    /// wiring, which an operator needs to see.
    pub fn push_input(&self, id: &DataId, event: QueuedEvent) {
        let handle = lock(&self.inputs).get(id).cloned();
        let Some(handle) = handle else {
            self.push_control(Event::Error(format!(
                "message for `{id}`, which this node does not declare as an input"
            )));
            return;
        };
        let report = handle.push(event);
        if report.outcome == PushOutcome::DroppedIncoming
            || report.outcome == PushOutcome::EnqueuedEvicting
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(signal) = report.signal {
            self.signals.fetch_add(1, Ordering::Relaxed);
            self.push_control(Event::Error(describe_signal(id, signal)));
            self.log_drop_signal(id, signal, Instant::now());
            return;
        }
        self.signal.notify();
    }

    /// Writes `signal` to the process log as a `tracing::error!` (blueprint
    /// §11.2: "a backpressure exhaustion is an ERROR-log-and-metric event
    /// rather than a silent loss"), at most once per `id` per
    /// [`DROP_LOG_WINDOW`].
    ///
    /// Rate-limited by input, not skipped after the first: a queue that goes
    /// quiet for a while and then starts dropping again is a *new*
    /// occurrence an operator needs to see, not a continuation of the first
    /// one — throttling is only about not writing one line per message
    /// during a single sustained overload.
    fn log_drop_signal(&self, id: &DataId, signal: QueueSignal, now: Instant) {
        {
            let mut last = lock(&self.last_drop_log);
            let elapsed_enough = match last.get(id) {
                Some(&previous) => {
                    now.checked_duration_since(previous)
                        .unwrap_or(Duration::ZERO)
                        >= DROP_LOG_WINDOW
                }
                None => true,
            };
            if !elapsed_enough {
                return;
            }
            last.insert(id.clone(), now);
        }
        let message = describe_signal(id, signal);
        tracing::error!(input = %id, "{message}");
    }

    /// Pushes a control-lane event, which is delivered before any data.
    pub fn push_control(&self, event: Event) {
        lock(&self.control).push_back(event);
        self.signal.notify();
    }

    /// Marks the source finished: no further events will arrive, and waiting
    /// consumers wake immediately.
    ///
    /// Idempotent.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.signal.close();
    }

    /// Whether the source has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Records that the node dropped its event stream (§7.3).
    ///
    /// Idempotent: the hook runs once, however many times a stream is
    /// abandoned.
    pub fn abandon(&self) {
        if self.abandoned.swap(true, Ordering::AcqRel) {
            return;
        }
        self.signal.notify();
        let hook = lock(&self.abandon_hook).take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Whether the node dropped its event stream.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    /// Takes the next event, control lane first, without waiting.
    #[must_use]
    pub fn try_next(&self) -> Option<Event> {
        if let Some(event) = lock(&self.control).pop_front() {
            self.delivered.fetch_add(1, Ordering::Relaxed);
            return Some(event);
        }
        let (id, queued) = self.mux.try_recv()?;
        self.delivered.fetch_add(1, Ordering::Relaxed);
        // Only actual data starts an input-to-output measurement (§11.3) —
        // `Closed`/`Recovered` are wind-down signals, not something a node
        // is expected to answer with a publish.
        if queued.is_message() {
            self.start_deadline(&id, Instant::now());
        }
        Some(queued.into_event(id))
    }

    /// Opens an input-to-output latency measurement for `id`, if it has a
    /// declared budget — a no-op otherwise. Supersedes (drops) whatever
    /// measurement was already open for `id`; see this module's docs.
    fn start_deadline(&self, id: &DataId, now: Instant) {
        let Some(token) = self.deadlines.start(id, now) else {
            return;
        };
        let _superseded = lock(&self.open_deadlines).insert(id.clone(), token);
    }

    /// Closes every open input-to-output measurement against `now` (§11.3),
    /// counting and reporting each one that ran over budget.
    ///
    /// Called once per publish — see [`crate::session::SessionShared::send_request`]
    /// — so the deadline check runs at the same cadence the daemon's own
    /// `sends_inline`/`sends_zero_copy` counters do. A node with nothing open
    /// (no deadline-tracked input has delivered since the last publish, or
    /// none is configured at all) does nothing here.
    pub fn finish_deadlines(&self, now: Instant) {
        let open: Vec<DeadlineToken<DataId>> = {
            let mut guard = lock(&self.open_deadlines);
            guard.drain().map(|(_, token)| token).collect()
        };
        for token in open {
            let id = token.key().clone();
            if let DeadlineOutcome::Violated {
                latency,
                budget,
                over_by,
                ..
            } = self.deadlines.finish(token, now)
            {
                self.deadline_violations.fetch_add(1, Ordering::Relaxed);
                self.push_control(Event::Error(describe_deadline_violation(
                    &id, budget, latency, over_by,
                )));
                if let Some(hook) = lock(&self.deadline_violation_hook).as_ref() {
                    hook(id, budget, latency);
                }
            }
        }
    }

    /// Whether anything is waiting to be delivered right now.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        lock(&self.control).is_empty() && self.mux.snapshot_all().iter().all(|(_, q)| q.depth == 0)
    }

    /// A snapshot of the stream's counters.
    #[must_use]
    pub fn stats(&self) -> StreamStats {
        StreamStats {
            delivered: self.delivered.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            control_depth: lock(&self.control).len() as u64,
            queue_signals: self.signals.load(Ordering::Relaxed),
            deadline_violations: self.deadline_violations.load(Ordering::Relaxed),
        }
    }
}

/// Renders a queue signal as the ERROR-lane message §11.2 asks for.
fn describe_signal(id: &DataId, signal: QueueSignal) -> String {
    match signal {
        QueueSignal::BackpressureExhausted {
            queue_size,
            buffered,
        } => format!(
            "input `{id}`: backpressure buffer exhausted at {buffered} message(s) \
             (queue_size {queue_size}); dropping"
        ),
        QueueSignal::ImmuneOverflow {
            immune_count,
            capacity,
        } => format!(
            "input `{id}`: {immune_count} eviction-immune message(s) over a capacity of \
             {capacity}; the peer is not consuming its correlated replies"
        ),
    }
}

/// Renders a deadline violation as the ERROR-lane message §11.3 asks for —
/// [`describe_signal`]'s §11.2 pattern, mirrored.
fn describe_deadline_violation(
    id: &DataId,
    budget: std::time::Duration,
    latency: std::time::Duration,
    over_by: std::time::Duration,
) -> String {
    format!(
        "input `{id}`: exceeded its {}ms input-to-output deadline — took {}ms ({}ms over)",
        budget.as_millis(),
        latency.as_millis(),
        over_by.as_millis(),
    )
}

/// Locks a mutex, recovering from a poisoning panic elsewhere.
///
/// Both guarded values are small collections with no multi-step invariant, so
/// discarding them because an unrelated thread unwound would turn one bug into
/// a wedged event loop — the same reasoning `astrs-scheduler` documents for
/// its own locks.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use super::*;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DurationMs, Metadata, PortRef, RouteCloseReason, StopCause};

    fn spec_with_deadline(name: &str, deadline_ms: Option<u64>) -> InputSpec {
        let mut spec = InputSpec::new(
            DataId::new(name).unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        );
        spec.deadline = deadline_ms.map(DurationMs::new);
        spec
    }

    fn source_with_input(capacity: u32, policy: QueuePolicy) -> EventSource {
        let source = EventSource::new();
        source
            .register_raw(
                DataId::new("frames").unwrap(),
                capacity,
                policy,
                PriorityLane::Data,
            )
            .unwrap();
        source
    }

    fn message(seq: i64) -> QueuedEvent {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.set_seq(seq);
        QueuedEvent::Input {
            source: PortRef::from_parts("camera", "image").unwrap(),
            metadata,
            payload: Payload::inline(vec![0; 4]),
        }
    }

    use crate::payload::Payload;

    #[test]
    fn events_come_out_in_order() {
        let source = source_with_input(8, QueuePolicy::DropOldest);
        let id = DataId::new("frames").unwrap();
        for seq in 0..3 {
            source.push_input(&id, message(seq));
        }
        for seq in 0..3 {
            let event = source.try_next().unwrap();
            assert_eq!(event.metadata().and_then(Metadata::seq), Some(seq));
        }
        assert!(source.try_next().is_none());
        assert!(source.is_empty());
        assert_eq!(source.stats().delivered, 3);
    }

    #[test]
    fn the_control_lane_pre_empts_a_full_data_queue() {
        let source = source_with_input(16, QueuePolicy::DropOldest);
        let id = DataId::new("frames").unwrap();
        for seq in 0..10 {
            source.push_input(&id, message(seq));
        }
        source.push_control(Event::Stop(StopCause::Requested));
        let event = source.try_next().unwrap();
        assert!(event.is_stop(), "control first, whatever the backlog");
    }

    #[test]
    fn drop_oldest_keeps_the_newest_message() {
        let source = source_with_input(2, QueuePolicy::DropOldest);
        let id = DataId::new("frames").unwrap();
        for seq in 0..5 {
            source.push_input(&id, message(seq));
        }
        let seqs: Vec<i64> = std::iter::from_fn(|| source.try_next())
            .filter_map(|event| event.metadata().and_then(Metadata::seq))
            .collect();
        assert_eq!(seqs, vec![3, 4], "the two newest survive");
        assert!(source.stats().dropped >= 3);
    }

    #[test]
    fn correlated_messages_are_never_the_ones_dropped() {
        let source = source_with_input(2, QueuePolicy::DropOldest);
        let id = DataId::new("frames").unwrap();

        let mut correlated = Metadata::new(HlcTimestamp::new(1, 0));
        correlated.set_request_id("req-1");
        correlated.set_seq(99);
        source.push_input(
            &id,
            QueuedEvent::Input {
                source: PortRef::from_parts("camera", "image").unwrap(),
                metadata: correlated,
                payload: Payload::empty(),
            },
        );
        for seq in 0..6 {
            source.push_input(&id, message(seq));
        }
        let seqs: Vec<i64> = std::iter::from_fn(|| source.try_next())
            .filter_map(|event| event.metadata().and_then(Metadata::seq))
            .collect();
        assert!(
            seqs.contains(&99),
            "the correlated message survived: {seqs:?}"
        );
    }

    #[test]
    fn backpressure_exhaustion_is_reported_on_the_control_lane() {
        let source = source_with_input(1, QueuePolicy::Backpressure);
        let id = DataId::new("frames").unwrap();
        // Ten times `queue_size` is the buffer; the eleventh push drops.
        for seq in 0..12 {
            source.push_input(&id, message(seq));
        }
        let mut errors = 0;
        while let Some(event) = source.try_next() {
            if matches!(event, Event::Error(_)) {
                errors += 1;
            }
        }
        assert!(errors > 0, "an exhausted buffer is an ERROR event");
        assert!(source.stats().queue_signals > 0);
    }

    // A minimal `tracing::Subscriber` that records every event's level and
    // rendered message — enough to assert a `tracing::error!` actually fired
    // (and how many times), without a dev-dependency this crate does not
    // otherwise need.
    #[derive(Default)]
    struct CaptureSubscriber {
        events: Mutex<Vec<(tracing::Level, String)>>,
    }

    impl CaptureSubscriber {
        fn events(&self) -> Vec<(tracing::Level, String)> {
            lock(&self.events).clone()
        }

        fn error_count(&self) -> usize {
            lock(&self.events)
                .iter()
                .filter(|(level, _)| *level == tracing::Level::ERROR)
                .count()
        }
    }

    #[derive(Default)]
    struct MessageVisitor(String);

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            lock(&self.events).push((*event.metadata().level(), visitor.0));
        }

        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[test]
    fn a_backpressure_signal_logs_an_error_once_per_window_not_per_message() {
        let source = source_with_input(1, QueuePolicy::Backpressure);
        let id = DataId::new("frames").unwrap();
        let subscriber = std::sync::Arc::new(CaptureSubscriber::default());
        tracing::subscriber::with_default(subscriber.clone(), || {
            // Far more than the ten-times-`queue_size` buffer, so many drops
            // happen in this one burst — all within `DROP_LOG_WINDOW`.
            for seq in 0..30 {
                source.push_input(&id, message(seq));
            }
        });
        assert_eq!(
            subscriber.error_count(),
            1,
            "one ERROR per window, not one per dropped message: {:?}",
            subscriber.events()
        );
        let (_, message) = &subscriber.events()[0];
        assert!(message.contains("frames"), "{message}");
    }

    #[test]
    fn an_input_within_budget_never_logs_an_error() {
        let source = source_with_input(8, QueuePolicy::DropOldest);
        let id = DataId::new("frames").unwrap();
        let subscriber = std::sync::Arc::new(CaptureSubscriber::default());
        tracing::subscriber::with_default(subscriber.clone(), || {
            source.push_input(&id, message(0));
        });
        assert_eq!(subscriber.error_count(), 0, "{:?}", subscriber.events());
    }

    #[test]
    fn the_drop_log_is_rate_limited_per_input_independently() {
        let a = DataId::new("a").unwrap();
        let b = DataId::new("b").unwrap();
        let source = EventSource::new();
        let signal = QueueSignal::BackpressureExhausted {
            queue_size: 1,
            buffered: 10,
        };
        let subscriber = std::sync::Arc::new(CaptureSubscriber::default());
        tracing::subscriber::with_default(subscriber.clone(), || {
            let t0 = Instant::now();
            source.log_drop_signal(&a, signal, t0);
            source.log_drop_signal(&a, signal, t0); // rate-limited: same input, same instant
            source.log_drop_signal(&b, signal, t0); // a different input has its own window
        });
        assert_eq!(subscriber.error_count(), 2, "{:?}", subscriber.events());
    }

    #[test]
    fn the_drop_log_fires_again_once_the_window_elapses() {
        let id = DataId::new("frames").unwrap();
        let source = EventSource::new();
        let signal = QueueSignal::BackpressureExhausted {
            queue_size: 1,
            buffered: 10,
        };
        let subscriber = std::sync::Arc::new(CaptureSubscriber::default());
        tracing::subscriber::with_default(subscriber.clone(), || {
            let t0 = Instant::now();
            source.log_drop_signal(&id, signal, t0);
            // Still inside the window: rate-limited.
            source.log_drop_signal(&id, signal, t0 + DROP_LOG_WINDOW - Duration::from_millis(1));
            // The window has fully elapsed: a fresh occurrence, logged again.
            source.log_drop_signal(&id, signal, t0 + DROP_LOG_WINDOW);
        });
        assert_eq!(subscriber.error_count(), 2, "{:?}", subscriber.events());
    }

    #[test]
    fn a_message_for_an_unknown_input_is_reported_not_dropped() {
        let source = EventSource::new();
        source.push_input(&DataId::new("nope").unwrap(), message(0));
        let event = source.try_next().unwrap();
        let Event::Error(message) = event else {
            panic!("expected an error event");
        };
        assert!(message.contains("nope"), "{message}");
    }

    #[test]
    fn wind_down_events_keep_their_place_behind_queued_frames() {
        let source = source_with_input(8, QueuePolicy::DropOldest);
        let id = DataId::new("frames").unwrap();
        source.push_input(&id, message(1));
        source.push_input(
            &id,
            QueuedEvent::Closed {
                source: PortRef::from_parts("camera", "image").unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            },
        );
        assert!(
            source.try_next().unwrap().is_input(),
            "the frame comes first"
        );
        assert!(matches!(
            source.try_next().unwrap(),
            Event::InputClosed { .. }
        ));
    }

    #[test]
    fn registration_is_reported_and_queryable() {
        let source = EventSource::new();
        let spec = InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        );
        source.register_input(&spec).unwrap();
        assert!(source.has_input(&spec.id));
        assert_eq!(source.input_ids(), vec![spec.id.clone()]);
        assert!(source.queue_snapshot(&spec.id).is_some());
        assert!(
            source.register_input(&spec).is_err(),
            "no double registration"
        );
        assert!(!source.has_input(&DataId::new("other").unwrap()));
    }

    #[test]
    fn the_abandon_hook_runs_exactly_once() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let source = EventSource::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        source.set_abandon_hook(Box::new(move || {
            let _ = counter.fetch_add(1, Ordering::Relaxed);
        }));
        source.abandon();
        source.abandon();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn closing_and_abandoning_are_observable_and_idempotent() {
        let source = EventSource::new();
        assert!(!source.is_closed());
        assert!(!source.is_abandoned());
        source.abandon();
        assert!(source.is_abandoned());
        source.close();
        assert!(source.is_closed());
        source.close();
        assert!(source.is_closed());
        assert!(source.signal().is_closed());
    }

    // ------------------------------------------------------ deadlines (§11.3)

    #[test]
    fn an_input_with_no_declared_deadline_never_reports_a_violation() {
        let source = EventSource::new();
        let spec = spec_with_deadline("frames", None);
        source.register_input(&spec).unwrap();
        source.push_input(&spec.id, message(0));
        assert!(source.try_next().unwrap().is_input());

        // Even an absurdly-late "publish" has no budget to have missed.
        source.finish_deadlines(Instant::now() + Duration::from_secs(3600));
        assert_eq!(source.stats().deadline_violations, 0);
        assert!(source.try_next().is_none(), "nothing was queued");
    }

    #[test]
    fn a_publish_comfortably_inside_the_budget_is_silent() {
        let source = EventSource::new();
        let spec = spec_with_deadline("frames", Some(50));
        source.register_input(&spec).unwrap();
        source.push_input(&spec.id, message(0));
        assert!(source.try_next().unwrap().is_input());

        source.finish_deadlines(Instant::now() + Duration::from_millis(5));
        assert_eq!(source.stats().deadline_violations, 0);
        assert!(source.try_next().is_none());
    }

    #[test]
    fn a_publish_past_the_budget_is_counted_and_reported() {
        let source = EventSource::new();
        let spec = spec_with_deadline("frames", Some(10));
        source.register_input(&spec).unwrap();
        source.push_input(&spec.id, message(0));
        assert!(source.try_next().unwrap().is_input());

        source.finish_deadlines(Instant::now() + Duration::from_secs(1));
        assert_eq!(source.stats().deadline_violations, 1);

        let event = source.try_next().expect("the violation was queued");
        let Event::Error(message) = event else {
            panic!("expected an error event, got {event:?}");
        };
        assert!(message.contains("frames"), "{message}");
        assert!(message.contains("deadline"), "{message}");
    }

    #[test]
    fn a_violation_runs_the_relay_hook_with_the_input_budget_and_latency() {
        let source = EventSource::new();
        let spec = spec_with_deadline("frames", Some(10));
        source.register_input(&spec).unwrap();

        let calls: std::sync::Arc<Mutex<Vec<(DataId, Duration, Duration)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        source.set_deadline_violation_hook(Box::new(move |input, budget, latency| {
            lock(&recorded).push((input, budget, latency));
        }));

        source.push_input(&spec.id, message(0));
        assert!(source.try_next().unwrap().is_input());
        source.finish_deadlines(Instant::now() + Duration::from_millis(80));

        let calls = lock(&calls);
        assert_eq!(calls.len(), 1, "{calls:?}");
        let (input, budget, latency) = &calls[0];
        assert_eq!(input, &spec.id);
        assert_eq!(*budget, Duration::from_millis(10));
        assert!(*latency >= Duration::from_millis(80), "{latency:?}");
    }

    #[test]
    fn a_publish_inside_the_budget_never_runs_the_relay_hook() {
        let source = EventSource::new();
        let spec = spec_with_deadline("frames", Some(50));
        source.register_input(&spec).unwrap();

        let calls: std::sync::Arc<Mutex<Vec<(DataId, Duration, Duration)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        source.set_deadline_violation_hook(Box::new(move |input, budget, latency| {
            lock(&recorded).push((input, budget, latency));
        }));

        source.push_input(&spec.id, message(0));
        assert!(source.try_next().unwrap().is_input());
        source.finish_deadlines(Instant::now() + Duration::from_millis(5));

        assert!(
            lock(&calls).is_empty(),
            "the quiet path must not run the relay hook either"
        );
    }

    #[test]
    fn a_second_delivery_supersedes_the_first_open_measurement() {
        let source = EventSource::new();
        let spec = spec_with_deadline("frames", Some(10));
        source.register_input(&spec).unwrap();

        source.push_input(&spec.id, message(0));
        assert!(source.try_next().unwrap().is_input());
        // A second delivery on the same input, before anything published,
        // replaces the first open measurement rather than queuing behind
        // it — see this module's docs.
        source.push_input(&spec.id, message(1));
        assert!(source.try_next().unwrap().is_input());

        source.finish_deadlines(Instant::now() + Duration::from_secs(1));
        assert_eq!(
            source.stats().deadline_violations,
            1,
            "one measurement was open, not two"
        );
    }

    #[test]
    fn finishing_deadlines_with_nothing_open_does_nothing() {
        let source = EventSource::new();
        source.finish_deadlines(Instant::now());
        assert_eq!(source.stats().deadline_violations, 0);
        assert!(source.try_next().is_none());
    }

    #[test]
    fn a_wind_down_signal_does_not_open_a_deadline_measurement() {
        let source = EventSource::new();
        let spec = spec_with_deadline("frames", Some(10));
        source.register_input(&spec).unwrap();
        source.push_input(
            &spec.id,
            QueuedEvent::Closed {
                source: PortRef::from_parts("camera", "image").unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            },
        );
        assert!(matches!(
            source.try_next().unwrap(),
            Event::InputClosed { .. }
        ));

        source.finish_deadlines(Instant::now() + Duration::from_secs(1));
        assert_eq!(
            source.stats().deadline_violations,
            0,
            "a wind-down signal is not data the node must answer with output"
        );
    }
}
