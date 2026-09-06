//! [`NodeMailbox`] — one node's inbound queue set (§11.2).
//!
//! A thin, daemon-shaped wrapper over [`astrs_scheduler::EventMux`]: one queue
//! per subscribed input, each with the manifest's `queue_size`,
//! `queue_policy`, and priority lane, and the control lane strictly
//! pre-empting the data lane so a `Stop` never waits behind a backlog of
//! camera frames.
//!
//! The wrapper exists for three reasons the scheduler deliberately does not
//! cover (see its crate docs: *"this crate does not log"*):
//!
//! 1. **The event type.** The mux is generic; the daemon's is
//!    [`astrs_wire::NodeEvent`], and `astrs-scheduler` already implements
//!    [`astrs_scheduler::MetadataView`] for exactly that type — so eviction
//!    immunity for `Stop` and for correlated request/goal messages
//!    (§9.4, §11.2) is inherited, not reimplemented.
//! 2. **Signals become observations.** A
//!    [`astrs_scheduler::QueueSignal`] returned from a push is turned into a
//!    [`DeliveryReport`] the router hands upward, where it becomes a log line
//!    and a metric.
//! 3. **Unsubscribed inputs.** A node may receive an event for an input it
//!    never subscribed to (a route added by a topology mutation, a virtual
//!    source the daemon offers unprompted). The mailbox registers lazily
//!    rather than dropping it on the floor.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::local::NodeMailbox;
//! use astrs_wire::{DataId, InputSpec, NodeEvent, PortRef, StopCause};
//!
//! let mut mailbox = NodeMailbox::new();
//! let spec = InputSpec::new(DataId::new("frames")?, PortRef::from_parts("camera", "image")?);
//! mailbox.register(&spec)?;
//!
//! let report = mailbox.push(&spec.id, NodeEvent::AllInputsClosed);
//! assert!(report.accepted());
//! assert_eq!(mailbox.depth(&spec.id), Some(1));
//!
//! let (input, event) = mailbox.try_recv().expect("queued");
//! assert_eq!(input, spec.id);
//! assert!(matches!(event, NodeEvent::AllInputsClosed));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use astrs_scheduler::{EventMux, InputHandle, PushOutcome, QueueSignal, QueueSnapshot};
use astrs_wire::{DataId, InputSpec, NodeEvent, PriorityLane, QueuePolicy};

use crate::error::{DaemonError, DaemonResult};

/// What one delivery did, and whether it is worth reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryReport {
    /// What the queue did with the message.
    pub outcome: PushOutcome,
    /// A condition the daemon should log or meter, if this push raised one.
    pub signal: Option<QueueSignal>,
}

impl DeliveryReport {
    /// Whether the message was queued.
    #[must_use]
    pub const fn accepted(&self) -> bool {
        matches!(
            self.outcome,
            PushOutcome::Enqueued | PushOutcome::EnqueuedEvicting
        )
    }

    /// Whether a message was lost — the incoming one, or an evicted older one.
    #[must_use]
    pub const fn dropped_something(&self) -> bool {
        matches!(
            self.outcome,
            PushOutcome::DroppedIncoming | PushOutcome::EnqueuedEvicting
        )
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self.outcome {
            PushOutcome::Enqueued => "enqueued",
            PushOutcome::EnqueuedEvicting => "enqueued_evicting",
            PushOutcome::DroppedIncoming => "dropped_incoming",
        }
    }
}

/// One node's inbound queues.
#[derive(Debug)]
pub struct NodeMailbox {
    /// The prioritized, fair mux over every registered input.
    mux: EventMux<NodeEvent>,
    /// The push handles, one per registered input.
    handles: std::collections::BTreeMap<DataId, InputHandle<NodeEvent>>,
    /// An end-of-stream notice parked until the queues behind it are empty.
    ///
    /// See [`NodeMailbox::try_recv`]. `Mutex` rather than a plain field
    /// because the taking side is `&self`; the critical section is a single
    /// `Option` move, so recovering from poisoning is sound (it cannot leave
    /// a torn intermediate state).
    held_end_of_stream: std::sync::Mutex<Option<(DataId, NodeEvent)>>,
    /// Payload bytes accepted into each input's queue since this mailbox was
    /// created (§13's bandwidth half).
    ///
    /// Counted here rather than in [`astrs_scheduler::InputQueue`] because
    /// that type is generic over its message and has no way to ask how large
    /// one is; this one knows it is carrying [`NodeEvent`]s and can. Counted
    /// on *acceptance*, so a message the queue policy refused or evicted is
    /// absent — which is what makes "the link is saturated" distinguishable
    /// from "the consumer is too slow" when read beside
    /// [`astrs_scheduler::QueueSnapshot::dropped`].
    received_bytes: std::collections::BTreeMap<DataId, u64>,
}

/// The payload bytes one event carries, for the ingress ledger (§13).
///
/// Only [`NodeEvent::Input`] carries a payload; a control event
/// (`InputClosed`, `Stop`, `Restarted`, …) is bookkeeping, not bandwidth, and
/// counting its frame would make an idle graph look busy.
fn payload_bytes(event: &NodeEvent) -> u64 {
    match event {
        NodeEvent::Input { payload, .. } => payload.len() as u64,
        _ => 0,
    }
}

impl NodeMailbox {
    /// An empty mailbox.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mux: EventMux::new(),
            handles: std::collections::BTreeMap::new(),
            held_end_of_stream: std::sync::Mutex::new(None),
            received_bytes: std::collections::BTreeMap::new(),
        }
    }

    /// Registers an input from its manifest-resolved specification.
    ///
    /// Registering the same input twice is a no-op rather than an error: a
    /// node that reconnects and re-subscribes must keep whatever is already
    /// queued for it, not lose it to a fresh queue.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Manifest`] if the specification asks for a zero-capacity
    /// queue, which the manifest validator already refuses and which is
    /// therefore surfaced rather than assumed away.
    pub fn register(&mut self, spec: &InputSpec) -> DaemonResult<()> {
        if self.handles.contains_key(&spec.id) {
            return Ok(());
        }
        let handle = self
            .mux
            .register_from_spec(spec)
            .map_err(|error| DaemonError::Manifest(format!("input {}: {error}", spec.id)))?;
        self.handles.insert(spec.id.clone(), handle);
        Ok(())
    }

    /// Registers an input with explicit parameters.
    ///
    /// # Errors
    ///
    /// As [`NodeMailbox::register`].
    pub fn register_input(
        &mut self,
        id: DataId,
        capacity: u32,
        policy: QueuePolicy,
        lane: PriorityLane,
    ) -> DaemonResult<()> {
        if self.handles.contains_key(&id) {
            return Ok(());
        }
        let handle = self
            .mux
            .register_input(id.clone(), capacity, policy, lane)
            .map_err(|error| DaemonError::Manifest(format!("input {id}: {error}")))?;
        self.handles.insert(id, handle);
        Ok(())
    }

    /// Whether `input` is registered.
    #[must_use]
    pub fn is_registered(&self, input: &DataId) -> bool {
        self.handles.contains_key(input)
    }

    /// How many inputs are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// The registered inputs, in order.
    pub fn inputs(&self) -> impl Iterator<Item = &DataId> {
        self.handles.keys()
    }

    /// Delivers `event` to `input`.
    ///
    /// An unregistered input silently registers itself with the §24.2 default
    /// queue (size 10, drop-oldest, data lane) rather than losing the message.
    /// The event's own class decides its immunity, not the queue's
    /// configuration, so a `Stop` arriving on an unregistered input is still
    /// eviction-immune.
    pub fn push(&mut self, input: &DataId, event: NodeEvent) -> DeliveryReport {
        if !self.handles.contains_key(input) {
            // The reserved status port (§8.4) carries every lifecycle event,
            // so its lane is a property of the *port*, not of whichever event
            // happened to arrive first. Deciding it from the event would let a
            // `Restarted` or `ExtDropped` — neither terminal nor a fault —
            // register it on the data lane, silently costing every later
            // `Stop` on that node the pre-emption it depends on.
            let lane =
                if *input == crate::local::status_port() || event.is_terminal() || event.is_fault()
                {
                    PriorityLane::Control
                } else {
                    PriorityLane::Data
                };
            // A default-capacity registration cannot fail; if it somehow did,
            // dropping the message is still better than a panic, and the
            // report says so.
            if self
                .register_input(
                    input.clone(),
                    astrs_wire::DEFAULT_QUEUE_SIZE,
                    QueuePolicy::DropOldest,
                    lane,
                )
                .is_err()
            {
                return DeliveryReport {
                    outcome: PushOutcome::DroppedIncoming,
                    signal: None,
                };
            }
        }

        // Measured before the push, because the push moves the event.
        let bytes = payload_bytes(&event);
        match self.handles.get(input) {
            Some(handle) => {
                let report = handle.push(event);
                // `DroppedIncoming` means the queue refused *this* message, so
                // it never arrived; every other outcome accepted it, including
                // the ones that evicted an older message to make room — that
                // older one's bytes did arrive, and are already counted.
                if report.outcome != PushOutcome::DroppedIncoming {
                    *self.received_bytes.entry(input.clone()).or_default() += bytes;
                }
                DeliveryReport {
                    outcome: report.outcome,
                    signal: report.signal,
                }
            }
            None => DeliveryReport {
                outcome: PushOutcome::DroppedIncoming,
                signal: None,
            },
        }
    }

    /// Payload bytes accepted on each input since this mailbox was created.
    #[must_use]
    pub const fn received_bytes(&self) -> &std::collections::BTreeMap<DataId, u64> {
        &self.received_bytes
    }

    /// Whether an event asserts that nothing further will arrive.
    ///
    /// [`NodeEvent::AllInputsClosed`] and `Stop` are both *terminal*
    /// ([`NodeEvent::is_terminal`]) and share the control lane, but they mean
    /// opposite things about the queue behind them. `Stop` is an interrupt —
    /// "stop now, whatever is queued" — and pre-empting a backlog is the
    /// point of it. `AllInputsClosed` is an end-of-stream marker: it asserts
    /// the node has *already been given* everything it will ever get, which
    /// is false while its own queues still hold messages. Only the latter is
    /// gated below.
    const fn is_end_of_stream(event: &NodeEvent) -> bool {
        matches!(event, NodeEvent::AllInputsClosed)
    }

    /// Locks the parked-notice slot, recovering from poisoning.
    fn held(&self) -> std::sync::MutexGuard<'_, Option<(DataId, NodeEvent)>> {
        self.held_end_of_stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes the next event, control lane first, holding an end-of-stream
    /// notice back until the queues it speaks for are empty.
    ///
    /// # Why this gate exists
    ///
    /// `AllInputsClosed` is delivered on the synthetic `astrs.status` port,
    /// which is a *control-lane* queue because the event is terminal — and
    /// the control lane strictly pre-empts the data lane. Without this gate a
    /// node is told "all your inputs are closed" while its own queues still
    /// hold undelivered messages, and a node that (correctly) treats that
    /// notice as its cue to finish drops them. Observed directly as
    /// `input, AllInputsClosed, input`.
    ///
    /// So the notice is parked and re-offered once the queues drain. It is
    /// held rather than declined: refusing the push would lose the only
    /// notice the daemon ever sends (`claim_inputs_closed_notice` is
    /// once-per-incarnation) and turn a mis-ordering into a node that waits
    /// forever.
    ///
    /// Starvation is not a concern and must not be "fixed" with a timeout:
    /// the daemon only sends this once *every* input is closed, so no
    /// producer is still feeding the queues that hold it back. The gate is
    /// also independent of *why* the notice arrived early — control-lane
    /// pre-emption here, or any future change to how the mux picks between
    /// queues — because it re-checks emptiness rather than trusting an order.
    #[must_use]
    pub fn try_recv(&self) -> Option<(DataId, NodeEvent)> {
        loop {
            match self.mux.try_recv() {
                Some(event) if Self::is_end_of_stream(&event.1) => {
                    // Park it and keep pulling: whatever is behind it in the
                    // queues is owed to the node first. A second notice would
                    // be indistinguishable from the first, so the parked one
                    // stands.
                    let mut held = self.held();
                    if held.is_none() {
                        *held = Some(event);
                    }
                }
                // A real event — data, or a `Stop` that is meant to jump the
                // queue — outranks a parked notice.
                Some(event) => return Some(event),
                // The queues are empty, so the notice is now true.
                None => return self.held().take(),
            }
        }
    }

    /// Waits for the next event, control lane first, under the same
    /// end-of-stream gate as [`NodeMailbox::try_recv`].
    pub async fn recv(&self) -> (DataId, NodeEvent) {
        loop {
            if let Some(event) = self.try_recv() {
                return event;
            }
            // Nothing ready and nothing parked, so block on the mux. The
            // event it wakes with still has to pass the gate: a notice can
            // arrive here ahead of data pushed a moment earlier, which is
            // exactly the race `try_recv` exists to close.
            let event = self.mux.recv().await;
            if Self::is_end_of_stream(&event.1) {
                let mut held = self.held();
                if held.is_none() {
                    *held = Some(event);
                }
                continue;
            }
            return event;
        }
    }

    /// Drains up to `max` events, control lane first.
    ///
    /// The shape a node's `NextEvent { max_batch }` request wants: one lock
    /// acquisition per event, no allocation beyond the returned vector, and a
    /// bounded amount of work per call. Goes through
    /// [`NodeMailbox::try_recv`], so a batch that ends a stream carries the
    /// remaining messages *followed by* the end-of-stream notice.
    #[must_use]
    pub fn drain(&self, max: usize) -> Vec<(DataId, NodeEvent)> {
        let mut batch = Vec::with_capacity(max.min(64));
        while batch.len() < max {
            match self.try_recv() {
                Some(event) => batch.push(event),
                None => break,
            }
        }
        batch
    }

    /// The queue depth for one input.
    #[must_use]
    pub fn depth(&self, input: &DataId) -> Option<u64> {
        self.mux
            .queue_snapshot(input)
            .map(|snapshot| snapshot.depth)
    }

    /// Every input's counters, for the metrics sampler (§13).
    #[must_use]
    pub fn snapshot_all(&self) -> Vec<(DataId, QueueSnapshot)> {
        self.mux.snapshot_all()
    }

    /// The total depth across every input.
    #[must_use]
    pub fn total_depth(&self) -> u64 {
        self.mux
            .snapshot_all()
            .into_iter()
            .map(|(_, snapshot)| snapshot.depth)
            .sum()
    }

    /// Removes an input's queue, discarding anything still in it.
    pub fn unregister(&mut self, input: &DataId) -> bool {
        self.handles.remove(input);
        self.mux.unregister_input(input)
    }

    /// Removes every queue — what a node's incarnation ending does.
    pub fn clear(&mut self) {
        let inputs: Vec<DataId> = self.handles.keys().cloned().collect();
        for input in inputs {
            self.unregister(&input);
        }
    }
}

impl Default for NodeMailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{Metadata, PortRef, StopCause};

    use super::*;

    fn data(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    fn spec(name: &str, size: u32, policy: QueuePolicy) -> InputSpec {
        InputSpec::new(data(name), PortRef::from_parts("camera", "image").unwrap())
            .with_queue(size, policy)
    }

    fn input_event(name: &str, payload: Vec<u8>) -> NodeEvent {
        NodeEvent::Input {
            id: data(name),
            source: PortRef::from_parts("camera", "image").unwrap(),
            metadata: Metadata::default(),
            payload,
        }
    }

    /// The §24.1 ordering invariant: "all your inputs are closed" is an
    /// assertion about the queues, so it may not overtake what is still in
    /// them — even though it rides the pre-empting control lane.
    #[test]
    fn an_end_of_stream_notice_waits_for_the_queue_it_speaks_for() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register_input(
                data("frames"),
                8,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();

        for i in 0..3u8 {
            assert!(
                mailbox
                    .push(&data("frames"), input_event("frames", vec![i]))
                    .accepted()
            );
        }
        // Pushed last, on the control lane, exactly as the daemon sends it.
        assert!(
            mailbox
                .push(&crate::local::status_port(), NodeEvent::AllInputsClosed)
                .accepted()
        );

        let batch = mailbox.drain(16);
        let payloads: Vec<Vec<u8>> = batch
            .iter()
            .filter_map(|(_, event)| match event {
                NodeEvent::Input { payload, .. } => Some(payload.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            payloads,
            vec![vec![0], vec![1], vec![2]],
            "every queued message must be delivered: {batch:?}"
        );
        assert!(
            matches!(batch.last(), Some((_, NodeEvent::AllInputsClosed))),
            "the notice must come last, not first: {batch:?}"
        );
        assert_eq!(batch.len(), 4, "{batch:?}");
        assert!(mailbox.try_recv().is_none(), "the notice is delivered once");
    }

    /// The other half of the same rule: `Stop` is an *interrupt*, so it keeps
    /// the pre-emption that `AllInputsClosed` gives up.
    #[test]
    fn a_stop_still_pre_empts_a_queued_backlog() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register_input(
                data("frames"),
                8,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();
        for i in 0..3u8 {
            mailbox.push(&data("frames"), input_event("frames", vec![i]));
        }
        mailbox.push(
            &crate::local::status_port(),
            NodeEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            },
        );

        let (_, first) = mailbox.try_recv().expect("something is queued");
        assert!(
            matches!(first, NodeEvent::Stop { .. }),
            "a stop must jump the queue: {first:?}"
        );
    }

    /// The status port's lane is a property of the port, not of whichever
    /// event registered it: a non-terminal, non-fault event arriving first
    /// must not demote it to the data lane and cost a later `Stop` its
    /// pre-emption.
    #[test]
    fn the_status_port_takes_the_control_lane_whatever_registers_it() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register_input(
                data("frames"),
                8,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();
        // Neither terminal nor a fault — the case that used to pick `Data`.
        mailbox.push(
            &crate::local::status_port(),
            NodeEvent::Restarted {
                peer: astrs_wire::NodeId::new("camera").unwrap(),
                generation: 1,
            },
        );
        assert!(mailbox.try_recv().is_some());

        for i in 0..3u8 {
            mailbox.push(&data("frames"), input_event("frames", vec![i]));
        }
        mailbox.push(
            &crate::local::status_port(),
            NodeEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            },
        );
        let (_, first) = mailbox.try_recv().expect("queued");
        assert!(
            matches!(first, NodeEvent::Stop { .. }),
            "the status port must have kept the control lane: {first:?}"
        );
    }

    #[test]
    fn an_empty_mailbox_has_nothing() {
        let mailbox = NodeMailbox::new();
        assert!(mailbox.is_empty());
        assert_eq!(mailbox.len(), 0);
        assert!(mailbox.try_recv().is_none());
        assert!(mailbox.drain(10).is_empty());
        assert_eq!(mailbox.total_depth(), 0);
        assert!(mailbox.depth(&data("nothing")).is_none());
    }

    #[test]
    fn registering_then_pushing_queues_the_event() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 4, QueuePolicy::DropOldest))
            .unwrap();
        assert!(mailbox.is_registered(&data("frames")));

        let report = mailbox.push(&data("frames"), input_event("frames", vec![1]));
        assert!(report.accepted());
        assert!(!report.dropped_something());
        assert_eq!(report.kind_name(), "enqueued");
        assert_eq!(mailbox.depth(&data("frames")), Some(1));
        assert_eq!(mailbox.total_depth(), 1);
    }

    #[test]
    fn registering_twice_keeps_what_is_queued() {
        let mut mailbox = NodeMailbox::new();
        let spec = spec("frames", 4, QueuePolicy::DropOldest);
        mailbox.register(&spec).unwrap();
        mailbox.push(&spec.id, input_event("frames", vec![1]));
        mailbox.register(&spec).unwrap();
        assert_eq!(mailbox.depth(&spec.id), Some(1), "the queue survived");
        assert_eq!(mailbox.len(), 1);
    }

    #[test]
    fn a_zero_capacity_specification_is_refused_rather_than_assumed_away() {
        let mut mailbox = NodeMailbox::new();
        let spec = spec("frames", 0, QueuePolicy::DropOldest);
        let error = mailbox.register(&spec).unwrap_err();
        assert!(matches!(error, DaemonError::Manifest(_)), "{error}");
    }

    #[test]
    fn an_unregistered_input_registers_itself_rather_than_losing_the_message() {
        let mut mailbox = NodeMailbox::new();
        let report = mailbox.push(&data("surprise"), input_event("surprise", vec![7]));
        assert!(report.accepted());
        assert!(mailbox.is_registered(&data("surprise")));
        assert_eq!(mailbox.depth(&data("surprise")), Some(1));
    }

    #[test]
    fn the_control_lane_pre_empts_the_data_lane() {
        let mut mailbox = NodeMailbox::new();
        let mut frames = spec("frames", 8, QueuePolicy::DropOldest);
        frames.priority_lane = PriorityLane::Data;
        let mut control = spec("control", 8, QueuePolicy::DropOldest);
        control.priority_lane = PriorityLane::Control;
        mailbox.register(&frames).unwrap();
        mailbox.register(&control).unwrap();

        // Data first, control second — the control message still wins.
        mailbox.push(&data("frames"), input_event("frames", vec![1]));
        mailbox.push(
            &data("control"),
            NodeEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            },
        );

        let (input, event) = mailbox.try_recv().expect("something is queued");
        assert_eq!(input, data("control"));
        assert!(event.is_terminal());
    }

    #[test]
    fn a_terminal_event_on_an_unknown_input_takes_the_control_lane() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 8, QueuePolicy::DropOldest))
            .unwrap();
        mailbox.push(&data("frames"), input_event("frames", vec![1]));
        mailbox.push(
            &data("astrs.status"),
            NodeEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            },
        );
        let (input, _) = mailbox.try_recv().expect("queued");
        assert_eq!(input, data("astrs.status"), "the stop jumped the queue");
    }

    #[test]
    fn drop_oldest_evicts_and_reports_it() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 2, QueuePolicy::DropOldest))
            .unwrap();
        for index in 0..2u8 {
            assert_eq!(
                mailbox
                    .push(&data("frames"), input_event("frames", vec![index]))
                    .outcome,
                PushOutcome::Enqueued
            );
        }
        let report = mailbox.push(&data("frames"), input_event("frames", vec![2]));
        assert_eq!(report.outcome, PushOutcome::EnqueuedEvicting);
        assert!(report.accepted());
        assert!(report.dropped_something());
        assert_eq!(report.kind_name(), "enqueued_evicting");
        assert_eq!(mailbox.depth(&data("frames")), Some(2));
    }

    #[test]
    fn backpressure_eventually_refuses_and_signals() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 1, QueuePolicy::Backpressure))
            .unwrap();
        // Ten times `queue_size` is the buffer; the eleventh is refused.
        let mut refused = None;
        for index in 0..20u8 {
            let report = mailbox.push(&data("frames"), input_event("frames", vec![index]));
            if !report.accepted() {
                refused = Some(report);
                break;
            }
        }
        let report = refused.expect("backpressure eventually refuses");
        assert_eq!(report.outcome, PushOutcome::DroppedIncoming);
        assert!(matches!(
            report.signal,
            Some(QueueSignal::BackpressureExhausted { .. })
        ));
    }

    #[test]
    fn a_stop_is_never_evicted() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 1, QueuePolicy::DropOldest))
            .unwrap();
        mailbox.push(
            &data("frames"),
            NodeEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            },
        );
        for index in 0..5u8 {
            mailbox.push(&data("frames"), input_event("frames", vec![index]));
        }
        let events = mailbox.drain(16);
        assert!(
            events.iter().any(|(_, event)| event.is_terminal()),
            "the stop survived the flood"
        );
    }

    #[test]
    fn draining_is_bounded_by_the_requested_batch() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 16, QueuePolicy::DropOldest))
            .unwrap();
        for index in 0..8u8 {
            mailbox.push(&data("frames"), input_event("frames", vec![index]));
        }
        assert_eq!(mailbox.drain(3).len(), 3);
        assert_eq!(mailbox.drain(100).len(), 5);
        assert!(mailbox.drain(4).is_empty());
    }

    #[tokio::test]
    async fn recv_waits_for_a_message() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 4, QueuePolicy::DropOldest))
            .unwrap();
        mailbox.push(&data("frames"), input_event("frames", vec![1]));
        let (input, _) = mailbox.recv().await;
        assert_eq!(input, data("frames"));
    }

    #[test]
    fn snapshots_cover_every_input() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("a", 4, QueuePolicy::DropOldest))
            .unwrap();
        mailbox
            .register(&spec("b", 4, QueuePolicy::DropOldest))
            .unwrap();
        mailbox.push(&data("a"), input_event("a", vec![1]));

        let snapshots = mailbox.snapshot_all();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(mailbox.total_depth(), 1);
        assert_eq!(mailbox.inputs().count(), 2);
    }

    #[test]
    fn unregistering_removes_the_queue() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("frames", 4, QueuePolicy::DropOldest))
            .unwrap();
        mailbox.push(&data("frames"), input_event("frames", vec![1]));
        assert!(mailbox.unregister(&data("frames")));
        assert!(!mailbox.is_registered(&data("frames")));
        assert!(mailbox.try_recv().is_none());
        assert!(!mailbox.unregister(&data("frames")));
    }

    #[test]
    fn clearing_removes_everything() {
        let mut mailbox = NodeMailbox::new();
        mailbox
            .register(&spec("a", 4, QueuePolicy::DropOldest))
            .unwrap();
        mailbox
            .register(&spec("b", 4, QueuePolicy::DropOldest))
            .unwrap();
        mailbox.clear();
        assert!(mailbox.is_empty());
        assert!(mailbox.try_recv().is_none());
    }
}
