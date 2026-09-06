//! `OperatorInbox` — one hosted operator's event queue.
//!
//! Blueprint §9.3: *"each operator runs on its own thread over a bounded
//! channel"*; §11.2's queue discipline (`queue_size`/`queue_policy`,
//! eviction immunity) is specified at the *input* granularity, and an
//! operator can declare several inputs, each with its own manifest
//! settings (`astrs_manifest::Input`). A single bounded channel with one
//! capacity cannot honor that — it has no notion of "this input drops the
//! oldest message, that one buffers to ten times its size before
//! dropping". `OperatorInbox` is this crate's per-operator inbox built
//! instead from the exact primitives `astrs-node-api`'s own per-*node*
//! inbox ([`astrs_node_api::events::EventSource`]) is built from —
//! [`astrs_scheduler::EventMux`] gives every named input its own
//! policy-and-immunity-aware [`astrs_scheduler::InputQueue`] plus fair
//! round-robin delivery, and a separate unbounded control deque carries
//! [`astrs_operator_api::OpEvent::Stop`] /
//! [`astrs_operator_api::OpEvent::Reload`] /
//! [`astrs_operator_api::OpEvent::ParamUpdate`] ahead of any data backlog
//! (blueprint §11.3: "the control lane pre-empts the data lane"), exactly
//! mirroring `EventSource`'s own two-lanes-one-source shape.
//!
//! This is the "bounded channel" blueprint §9.3 asks for: a
//! [`std::sync::mpsc`] or `tokio::sync::mpsc` in front of it would only
//! add a second, policy-blind layer of buffering, so none sits there — see
//! this crate's top-level docs for the fuller justification.
//!
//! Delivery is blocking (via [`astrs_node_api::Signal`], the same
//! ticket-before-check-before-wait primitive `EventStream::recv` uses),
//! never tokio-async: every hosted operator runs on a plain
//! [`std::thread`], not inside a runtime, so the wakeup this inbox offers
//! has to work without one.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use astrs_node_api::{Signal, WaitOutcome};
use astrs_operator_api::OpEvent;
use astrs_scheduler::{Envelope, EventMux, InputHandle, PushReport, QueueSnapshot};
use astrs_wire::messages::control::types::ParamScope;
use astrs_wire::metadata::Parameter;
use astrs_wire::{
    DataId, DurationMs, Metadata, ParamKey, PortRef, PriorityLane, QueuePolicy, RouteCloseReason,
    StopCause,
};

/// One data-lane item queued for one of an operator's named inputs.
///
/// The payload analogue of [`astrs_node_api::events::QueuedEvent`] — same
/// shape, but carrying already-materialized `Vec<u8>` (matching
/// [`OpEvent::Input`]'s own frozen field, rather than node-api's lazy
/// [`astrs_node_api::Payload`]).
#[derive(Debug, Clone)]
enum QueuedInput {
    /// A message.
    Message {
        /// The producer port it came from (the *original* external
        /// producer for an externally-fed input, or the sibling operator's
        /// synthetic port for an intra-runtime one).
        source: PortRef,
        /// The payload bytes.
        payload: Vec<u8>,
    },
    /// The input will receive nothing further.
    Closed {
        /// The producer port that stopped.
        source: PortRef,
        /// Why it closed.
        reason: RouteCloseReason,
    },
}

/// Wraps one [`QueuedInput`] for [`astrs_scheduler::InputQueue`], attaching
/// the eviction-immunity bit `Closed` needs "by hand": generic
/// [`Envelope::stop`] is a plain field, not derived from the payload's own
/// shape, so — unlike `astrs-node-api`'s bespoke `MetadataView` impl for its
/// own `QueuedEvent` — the immunity has to be set explicitly at
/// construction rather than computed from the variant.
fn envelope_for(item: QueuedInput, metadata: Option<Metadata>) -> Envelope<QueuedInput> {
    let stop = matches!(item, QueuedInput::Closed { .. });
    Envelope {
        payload: item,
        metadata,
        stop,
    }
}

/// One control-lane item: everything an operator's inbox delivers ahead of
/// any data backlog.
#[derive(Debug, Clone)]
enum ControlItem {
    /// Wind the operator down (blueprint §9.3).
    Stop {
        cause: StopCause,
        grace: Option<DurationMs>,
    },
    /// Reset the operator's own state (blueprint §9.3).
    Reload,
    /// A parameter this operator's host node reads was written.
    ParamUpdate {
        scope: ParamScope,
        key: ParamKey,
        value: Parameter,
    },
}

impl ControlItem {
    fn into_op_event(self) -> OpEvent {
        match self {
            Self::Stop { cause, grace } => OpEvent::Stop { cause, grace },
            Self::Reload => OpEvent::Reload,
            Self::ParamUpdate { scope, key, value } => OpEvent::ParamUpdate { scope, key, value },
        }
    }
}

/// One hosted operator's inbox: a fair, policy-aware, priority-preempting
/// queue that hands out [`OpEvent`]s.
///
/// Outlives any one operator *incarnation* — a restarted operator keeps the
/// same inbox, so a message queued while the previous incarnation was
/// panicking is not lost to the rebuild (see [`crate::worker`]'s module
/// docs).
pub(crate) struct OperatorInbox {
    /// One [`astrs_scheduler::InputQueue`] per named input, registered from
    /// that input's own manifest `queue_size`/`queue_policy`.
    mux: EventMux<Envelope<QueuedInput>>,
    /// Producer-side handles, one per registered input — `EventMux` hands
    /// one back only at registration time, so this is what lets
    /// [`OperatorInbox::push_message`] reach an input's queue by
    /// [`DataId`] afterward (mirroring
    /// [`astrs_node_api::events::EventSource`]'s own `inputs` map).
    handles: Mutex<HashMap<DataId, InputHandle<Envelope<QueuedInput>>>>,
    /// The unbounded control lane.
    control: Mutex<VecDeque<ControlItem>>,
    /// Wakes a thread blocked in [`OperatorInbox::recv_blocking`].
    signal: Signal,
    /// Set once no further events will ever arrive (the whole runtime is
    /// winding down and this operator's final `Stop` was already queued, or
    /// every producer of every registered input has permanently closed).
    closed: AtomicBool,
}

impl core::fmt::Debug for OperatorInbox {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OperatorInbox")
            .field("mux", &self.mux)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl OperatorInbox {
    /// An inbox with no inputs registered yet.
    pub(crate) fn new() -> Self {
        Self {
            mux: EventMux::new(),
            handles: Mutex::new(HashMap::new()),
            control: Mutex::new(VecDeque::new()),
            signal: Signal::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Registers one named input with the queue bound this operator's
    /// manifest entry declares for it.
    ///
    /// # Errors
    ///
    /// [`astrs_scheduler::SchedulerError`] on a zero capacity or a duplicate
    /// name.
    pub(crate) fn register_input(
        &self,
        id: DataId,
        capacity: u32,
        policy: QueuePolicy,
    ) -> astrs_scheduler::Result<()> {
        let handle = self
            .mux
            .register_input(id.clone(), capacity, policy, PriorityLane::Data)?;
        let _previous = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, handle);
        Ok(())
    }

    /// A point-in-time reading of one input's queue counters.
    #[must_use]
    pub(crate) fn queue_snapshot(&self, id: &DataId) -> Option<QueueSnapshot> {
        self.mux.queue_snapshot(id)
    }

    /// Buffers a message for `input`, applying that input's queue policy,
    /// and wakes a blocked reader.
    ///
    /// Returns the queue's own [`PushReport`] so a caller can log a drop
    /// signal (blueprint §11.2) — this type has no logging dependency of
    /// its own, matching `astrs-scheduler`'s own stance.
    pub(crate) fn push_message(
        &self,
        input: &DataId,
        source: PortRef,
        metadata: Metadata,
        payload: Vec<u8>,
    ) -> Option<PushReport> {
        let handle = self.handle_for(input)?;
        let envelope = envelope_for(QueuedInput::Message { source, payload }, Some(metadata));
        let report = handle.push(envelope);
        self.signal.notify();
        Some(report)
    }

    /// Marks `input` closed, immune to eviction like every wind-down signal.
    pub(crate) fn push_closed(
        &self,
        input: &DataId,
        source: PortRef,
        reason: RouteCloseReason,
    ) -> Option<PushReport> {
        let handle = self.handle_for(input)?;
        let envelope = envelope_for(QueuedInput::Closed { source, reason }, None);
        let report = handle.push(envelope);
        self.signal.notify();
        Some(report)
    }

    /// The producer-side handle for `input`, if it was registered.
    fn handle_for(&self, input: &DataId) -> Option<InputHandle<Envelope<QueuedInput>>> {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(input)
            .cloned()
    }

    /// Buffers a control-lane item, delivered ahead of any queued input.
    pub(crate) fn push_stop(&self, cause: StopCause, grace: Option<DurationMs>) {
        self.push_control(ControlItem::Stop { cause, grace });
    }

    /// As [`OperatorInbox::push_stop`], for a reload.
    pub(crate) fn push_reload(&self) {
        self.push_control(ControlItem::Reload);
    }

    /// As [`OperatorInbox::push_stop`], for a parameter update.
    pub(crate) fn push_param_update(&self, scope: ParamScope, key: ParamKey, value: Parameter) {
        self.push_control(ControlItem::ParamUpdate { scope, key, value });
    }

    fn push_control(&self, item: ControlItem) {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(item);
        self.signal.notify();
    }

    /// Marks the inbox permanently closed and wakes every waiter.
    ///
    /// Idempotent. A worker still drains whatever is already queued after
    /// this — closing only means "nothing further will arrive", the same
    /// contract [`astrs_node_api::events::EventSource::close`] documents.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.signal.close();
    }

    /// Whether [`OperatorInbox::close`] has run.
    #[must_use]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Takes the next event, control lane first, without waiting.
    fn try_next(&self) -> Option<OpEvent> {
        if let Some(item) = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
        {
            return Some(item.into_op_event());
        }
        let (id, envelope) = self.mux.try_recv()?;
        Some(into_op_event(id, envelope))
    }

    /// Blocks until an event is ready, the inbox closes, or `timeout`
    /// expires (`None` waits indefinitely).
    ///
    /// Returns `None` only once the inbox is closed *and* drained — the
    /// same "close does not discard the tail" contract
    /// [`astrs_node_api::EventStream::recv`] documents for its own queue.
    pub(crate) fn recv_blocking(&self, timeout: Option<Duration>) -> Option<OpEvent> {
        let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
        loop {
            let ticket = self.signal.ticket();
            if let Some(event) = self.try_next() {
                return Some(event);
            }
            if self.is_closed() {
                return None;
            }
            let remaining = match deadline {
                None => None,
                Some(deadline) => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    Some(deadline - now)
                }
            };
            match self.signal.wait_blocking(ticket, remaining) {
                WaitOutcome::TimedOut => return None,
                WaitOutcome::Closed => return self.try_next(),
                // `Signalled`, or a future variant this build does not
                // know how to interpret: the conservative choice is to
                // loop around and re-check the queue rather than assume
                // either "timed out" or "closed".
                _ => {}
            }
        }
    }
}

/// Logs a blueprint §11.2 "must-log" queue signal, if `report` carried one,
/// alongside that input's own queue snapshot for context.
///
/// Shared by every call site that pushes into an [`OperatorInbox`] and
/// would otherwise just discard the [`PushReport`] — the demux loop
/// routing external inputs, and [`crate::routing::Forwarder`] routing
/// intra-runtime sibling edges.
pub(crate) fn log_queue_signal(inbox: &OperatorInbox, input: &DataId, report: Option<PushReport>) {
    let Some(signal) = report.and_then(|report| report.signal) else {
        return;
    };
    let snapshot = inbox.queue_snapshot(input);
    tracing::warn!(input = %input, ?signal, ?snapshot, "operator input queue signal");
}

/// Converts one delivered `(input id, envelope)` pair into the [`OpEvent`]
/// an operator's `on_event` sees.
fn into_op_event(id: DataId, envelope: Envelope<QueuedInput>) -> OpEvent {
    match envelope.payload {
        QueuedInput::Message { source, payload } => OpEvent::Input {
            id,
            source,
            metadata: envelope.metadata.unwrap_or_default(),
            payload,
        },
        QueuedInput::Closed { source, reason } => OpEvent::InputClosed { id, source, reason },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn port() -> PortRef {
        PortRef::from_parts("camera", "image").unwrap()
    }

    fn meta() -> Metadata {
        Metadata::new(HlcTimestamp::EPOCH)
    }

    #[test]
    fn a_registered_input_delivers_messages_in_order() {
        let inbox = OperatorInbox::new();
        let id = DataId::new("frames").unwrap();
        inbox
            .register_input(id.clone(), 4, QueuePolicy::DropOldest)
            .unwrap();

        for n in 0..3u8 {
            inbox.push_message(&id, port(), meta(), vec![n]).unwrap();
        }
        for n in 0..3u8 {
            let event = inbox.recv_blocking(Some(Duration::from_secs(1))).unwrap();
            let OpEvent::Input { payload, .. } = event else {
                panic!("expected an input");
            };
            assert_eq!(payload, vec![n]);
        }
        assert!(
            inbox
                .recv_blocking(Some(Duration::from_millis(10)))
                .is_none()
        );
    }

    #[test]
    fn control_events_preempt_a_full_data_backlog() {
        let inbox = OperatorInbox::new();
        let id = DataId::new("frames").unwrap();
        inbox
            .register_input(id.clone(), 16, QueuePolicy::DropOldest)
            .unwrap();
        for n in 0..10u8 {
            inbox.push_message(&id, port(), meta(), vec![n]).unwrap();
        }
        inbox.push_stop(StopCause::Requested, None);

        let event = inbox.recv_blocking(Some(Duration::from_secs(1))).unwrap();
        assert!(matches!(event, OpEvent::Stop { .. }), "{event:?}");
    }

    #[test]
    fn reload_and_param_update_are_control_lane_too() {
        let inbox = OperatorInbox::new();
        inbox.push_reload();
        let event = inbox.recv_blocking(None).unwrap();
        assert!(matches!(event, OpEvent::Reload));

        inbox.push_param_update(
            ParamScope::Global,
            ParamKey::new("gain").unwrap(),
            Parameter::Float(1.5),
        );
        let event = inbox.recv_blocking(None).unwrap();
        assert!(matches!(event, OpEvent::ParamUpdate { .. }));
    }

    #[test]
    fn a_message_for_an_unregistered_input_is_silently_absent() {
        let inbox = OperatorInbox::new();
        assert!(
            inbox
                .push_message(&DataId::new("nope").unwrap(), port(), meta(), vec![1])
                .is_none()
        );
        assert!(
            inbox
                .recv_blocking(Some(Duration::from_millis(10)))
                .is_none()
        );
    }

    #[test]
    fn drop_oldest_evicts_under_load_but_never_a_closed_signal() {
        let inbox = OperatorInbox::new();
        let id = DataId::new("frames").unwrap();
        inbox
            .register_input(id.clone(), 2, QueuePolicy::DropOldest)
            .unwrap();
        for n in 0..5u8 {
            inbox.push_message(&id, port(), meta(), vec![n]).unwrap();
        }
        inbox.push_closed(&id, port(), RouteCloseReason::ProducerFinished);

        let mut payloads = Vec::new();
        let mut saw_closed = false;
        while let Some(event) = inbox.recv_blocking(Some(Duration::from_millis(50))) {
            match event {
                OpEvent::Input { payload, .. } => payloads.push(payload),
                OpEvent::InputClosed { .. } => saw_closed = true,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(saw_closed, "the close signal must survive eviction");
        assert!(payloads.len() <= 2, "{payloads:?}");
    }

    #[test]
    fn closing_lets_the_tail_drain_then_ends() {
        let inbox = OperatorInbox::new();
        let id = DataId::new("frames").unwrap();
        inbox
            .register_input(id.clone(), 4, QueuePolicy::DropOldest)
            .unwrap();
        inbox.push_message(&id, port(), meta(), vec![9]).unwrap();
        inbox.close();

        assert!(inbox.is_closed());
        let event = inbox.recv_blocking(Some(Duration::from_secs(1))).unwrap();
        assert!(
            matches!(event, OpEvent::Input { .. }),
            "the tail is drained"
        );
        assert!(inbox.recv_blocking(Some(Duration::from_secs(1))).is_none());
    }

    #[test]
    fn queue_snapshot_reports_depth() {
        let inbox = OperatorInbox::new();
        let id = DataId::new("frames").unwrap();
        inbox
            .register_input(id.clone(), 4, QueuePolicy::DropOldest)
            .unwrap();
        inbox.push_message(&id, port(), meta(), vec![1]).unwrap();
        assert_eq!(inbox.queue_snapshot(&id).unwrap().depth, 1);
        assert!(
            inbox
                .queue_snapshot(&DataId::new("nope").unwrap())
                .is_none()
        );
    }

    #[test]
    fn debug_rendering_does_not_panic() {
        let inbox = OperatorInbox::new();
        assert!(format!("{inbox:?}").contains("OperatorInbox"));
    }
}
