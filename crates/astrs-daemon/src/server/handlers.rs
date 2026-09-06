//! What the event loop does about each event.
//!
//! The other half of [`crate::server::core::Daemon`], split out because the
//! loop and its handlers together would exceed the 2000-line file ceiling and
//! because the split is a real seam: [`core`](crate::server::core) decides
//! *when* something is handled (the `select!`, the HLC stamp, the deadline
//! arithmetic), and this module decides *what happens*.
//!
//! Every method here is a `&mut self` on the one task that owns the state, so
//! none of them can interleave with another. That is what lets a node's exit
//! close its outputs, tell its peers, reclaim its extensions and consult its
//! restart policy as one indivisible step — see [`core`](crate::server::core)'s
//! own documentation for why that indivisibility is not optional.

use std::process::ExitStatus;
use std::time::Instant;

use astrs_log::{LogRecord, StdioStream};
use astrs_wire::{
    DaemonEvent, DataId, DataflowId, DurationMs, ExtensionKey, LogLevel,
    LogRecord as WireLogRecord, Metadata, NodeEvent, NodeExitCause, NodeHandshake, NodeId,
    NodeRequest, OutputPayload, PortRef, SessionId,
};

use crate::error::DaemonError;
use crate::local::{LocalRouter, NodeMailbox, PayloadOrigin, status_port};
use crate::server::closure::{ClosureOrigin, ClosureRelease, ProcessLiveness};
use crate::server::core::Daemon;
use crate::session::{RefusalReason, SessionAction, SessionProtocol};
use crate::spawn::{SpawnRequest, StdioMode};
use crate::state::DataflowState;
use crate::supervise::{classify_with_intent, spawn_log_pump, spawn_waiter};

/// The `astrs-log` twin of a wire severity.
///
/// The reverse of the mapping [`Daemon::report_captured_log`] applies, kept
/// as a free function because both directions are needed and neither belongs
/// to either crate: `astrs-log` deliberately has no `astrs-wire` dependency.
const fn wire_level_to_log(level: LogLevel) -> astrs_log::LogLevel {
    match level {
        LogLevel::Error => astrs_log::LogLevel::Error,
        LogLevel::Warn => astrs_log::LogLevel::Warn,
        LogLevel::Info => astrs_log::LogLevel::Info,
        LogLevel::Debug => astrs_log::LogLevel::Debug,
        LogLevel::Trace => astrs_log::LogLevel::Trace,
        // `LogLevel` is `#[non_exhaustive]`: a severity minted by a newer
        // peer is carried at the level nearest to "something happened"
        // rather than dropped, because losing the record entirely would be
        // the worse answer for an operator reading logs.
        _ => astrs_log::LogLevel::Info,
    }
}

impl Daemon {
    /// Handles one request from a node.
    pub(crate) fn handle_request(&mut self, session: SessionId, request: NodeRequest) {
        // Any request at all is proof the node's event loop is running (§12).
        self.touch_health(session, Instant::now());
        let mut protocol = self
            .protocols
            .remove(&session)
            .unwrap_or_else(|| SessionProtocol::new(session));

        // Re-establish the protocol's view of who is registered from the
        // daemon's own state, which is the single source of truth: a protocol
        // that outlived a reconnect would otherwise disagree with it.
        if let Some(binding) = self.state.session(session) {
            protocol.confirm_registration(
                binding.dataflow,
                binding.node.clone(),
                binding.generation,
            );
        }

        for action in protocol.handle(request) {
            self.apply_action(session, &mut protocol, action);
        }
        self.protocols.insert(session, protocol);
    }

    /// Applies one protocol action.
    fn apply_action(
        &mut self,
        session: SessionId,
        protocol: &mut SessionProtocol,
        action: SessionAction,
    ) {
        match action {
            SessionAction::Register { handshake } => {
                self.apply_register(session, protocol, *handshake);
            }
            SessionAction::Subscribe { inputs } => self.apply_subscribe(session, &inputs),
            SessionAction::Publish {
                output,
                metadata,
                payload,
            } => self.apply_publish(session, &output, *metadata, payload),
            SessionAction::CloseOutput { output } => {
                self.apply_close_outputs(session, &[output], ClosureOrigin::Explicit);
            }
            SessionAction::CloseOutputs { outputs } => {
                self.apply_close_outputs(session, &outputs, ClosureOrigin::Teardown);
            }
            SessionAction::Deliver { max_batch, .. } => self.apply_deliver(session, max_batch),
            SessionAction::ExtStore { key, value, ttl } => {
                self.apply_ext_store(session, key, value, ttl);
            }
            SessionAction::ExtLoad { key } => self.apply_ext_load(session, key),
            SessionAction::ExtDrop { key } => self.apply_ext_drop(session, &key),
            SessionAction::RouteUpgradeAck {
                output,
                accepted,
                reason,
            } => self.apply_upgrade_ack(session, &output, accepted, reason),
            SessionAction::Disconnect => self.handle_session_closed(session),
            SessionAction::Refuse { reason } => self.apply_refusal(session, &reason),
            SessionAction::ReportDeadlineViolation {
                input,
                budget,
                latency,
            } => self.apply_deadline_violation(session, input, budget, latency),
        }
    }

    /// Accepts (or refuses) a node's registration.
    fn apply_register(
        &mut self,
        session: SessionId,
        protocol: &mut SessionProtocol,
        handshake: NodeHandshake,
    ) {
        let dataflow = handshake.dataflow;
        let node = handshake.node.clone();
        let now = self.clock.now();

        let Some(state) = self.state.dataflow_mut(dataflow) else {
            self.refuse(session, DaemonError::UnknownDataflow { dataflow });
            return;
        };
        let Some(node_state) = state.node_mut(&node) else {
            self.refuse(session, DaemonError::UnknownNode { dataflow, node });
            return;
        };

        // A dynamic node was never spawned, so it has no generation to match:
        // the daemon adopts whatever it presents. A spawned node must present
        // the generation the daemon handed it in `ASTRS_NODE_CONFIG`, which is
        // what keeps a zombie from a previous incarnation out (§12).
        let expected = node_state.generation();
        if !node_state.is_dynamic() && handshake.generation != expected {
            let error = DaemonError::StaleGeneration {
                dataflow,
                node,
                presented: handshake.generation,
                current: expected,
            };
            self.refuse(session, error);
            return;
        }

        node_state.mark_registered_on(session, now);
        let spec = node_state.spec().clone();
        let generation = node_state.generation();
        let inputs = spec.inputs.clone();

        self.state
            .bind_session_at(session, dataflow, node.clone(), generation);
        // §13's totals are "since the node registered"; this is that instant.
        self.io.begin_incarnation(dataflow, &node, generation);
        protocol.confirm_registration(dataflow, node.clone(), generation);
        self.spawn_deadlines
            .disarm_generation(&(dataflow, node.clone()), generation);

        // Give the node a mailbox with its manifest queue configuration before
        // anything can be published to it.
        let mailbox = self.mailbox(dataflow, &node);
        for input in &inputs {
            let _ = mailbox.register(input);
        }

        self.send_to(
            dataflow,
            &node,
            NodeEvent::Registered {
                spec: Box::new(spec),
                session,
            },
        );
        // A producer faster than this node's own connect/handshake round
        // trip (the common case for two freshly spawned OS processes, one
        // of which has nothing to do before its first `SendMessage`) may
        // already have published into this node's mailbox before it had a
        // session to push to at all — [`Self::push_to_waiting_consumers`]
        // skips exactly that node on each of those earlier publishes (no
        // session yet), and nothing else revisits them once one exists.
        // Draining now, right after `Registered`, is the symmetric other
        // half of that push-on-publish path: whatever is already queued is
        // sent immediately, in the same order it was queued, and after
        // `Registered` on the wire so a node's init handshake (which reads
        // that one event directly, before its ordinary dispatch loop even
        // starts) is never handed an `Input` in its place.
        self.deliver_now(session, astrs_wire::DEFAULT_EVENT_BATCH);
        self.arm_health(dataflow, &node, Instant::now());
        self.notify_peers_restarted(dataflow, &node, generation);
        self.replan_planes(dataflow, &node);
        self.check_ready_barrier(dataflow);
        self.publish_gauges();
        // The successful half of a `ReplaceNode` cutover (§8, §17):
        // whatever incarnation this generation superseded is told to stop
        // now that its replacement has proven itself by registering. A
        // no-op for every ordinary registration, which supersedes nothing.
        self.resolve_superseded(dataflow, &node);
    }

    /// Re-evaluates the shared-memory plane for every output `node` takes part
    /// in, as producer or consumer (§6.3).
    fn replan_planes(&mut self, dataflow: DataflowId, node: &NodeId) {
        let sources: Vec<PortRef> = match self.state.dataflow(dataflow) {
            Some(state) => state
                .routes()
                .produced_by(node)
                .into_iter()
                .chain(state.routes().consumed_by(node))
                .cloned()
                .collect(),
            None => return,
        };
        for source in sources {
            self.plan_shm_output(dataflow, &source);
        }
    }

    /// Records a node's input subscriptions.
    fn apply_subscribe(&mut self, session: SessionId, inputs: &[DataId]) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        let specs = {
            let Some(state) = self.state.dataflow_mut(binding.dataflow) else {
                return;
            };
            let Some(node_state) = state.node_mut(&binding.node) else {
                return;
            };
            node_state.subscribe(inputs);
            node_state
                .spec()
                .inputs
                .iter()
                .filter(|input| node_state.is_subscribed(&input.id))
                .cloned()
                .collect::<Vec<_>>()
        };
        let mailbox = self.mailbox(binding.dataflow, &binding.node);
        for spec in &specs {
            let _ = mailbox.register(spec);
        }
        // A new subscriber changes who is expected on the ring (§6.3).
        for spec in &specs {
            self.plan_shm_output(binding.dataflow, &spec.source);
        }
        self.publish_gauges();
    }

    /// Fans a published message out to its local consumers.
    fn apply_publish(
        &mut self,
        session: SessionId,
        output: &DataId,
        metadata: Metadata,
        payload: OutputPayload,
    ) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        let dataflow = binding.dataflow;
        let declared = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(&binding.node))
            .is_some_and(|node| node.spec().output(output).is_some());
        if !declared {
            self.refuse(
                session,
                DaemonError::UnknownOutput {
                    dataflow,
                    node: binding.node.clone(),
                    output: output.clone(),
                },
            );
            return;
        }

        let source = PortRef::new(binding.node.clone(), output.clone());
        match payload {
            // Bytes on the control channel, in no ring: §6.2's below-threshold
            // rule, or a fallback after a full one. Every consumer needs them,
            // *including* one whose route is otherwise on the shared-memory
            // plane — it is reading a ring these bytes never entered.
            OutputPayload::Inline { bytes } => {
                self.fan_out(dataflow, &source, metadata, bytes, PayloadOrigin::Inline);
            }
            // A slot reference: the consumers on the ring already have the
            // bytes, and anybody still on the daemon path — a tap, a remote
            // consumer, a downgrade in flight — is served by draining the
            // daemon's own reader (§6.3).
            OutputPayload::Shm { generation, .. } => {
                self.drain_bridged(dataflow, &binding.node, output, generation);
            }
            // `OutputPayload` is `#[non_exhaustive]`: a carrier this build
            // does not know is accounted as delivered to nobody rather than
            // silently mis-delivered.
            _ => {}
        }
    }

    /// Fans one message out on every plane that carries it.
    ///
    /// The order is deliberate: the tap and the remote legs see the payload
    /// before it is moved into the last local consumer's event, so the local
    /// fan-out keeps its zero-clone fast path for the common single-consumer
    /// case.
    ///
    /// This is also where §13's egress accounting happens, because it is the
    /// one point *both* data planes pass through: a below-threshold or
    /// fallback publish arrives here with its inline bytes, and a
    /// shared-memory publish arrives here with the bytes the daemon's own
    /// bridge reader drained out of the ring (§6.2, §6.3). See
    /// [`crate::health::NodeIoLedger`].
    pub(crate) fn fan_out(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        metadata: Metadata,
        bytes: Vec<u8>,
        origin: PayloadOrigin,
    ) {
        // Charged once per publish, before the fan-out: what left the producer,
        // not what the topology multiplied it into. A virtual source (§8.4's
        // `astrs/timer/*`, `astrs/logs/*`) is charged to its reserved producer
        // id and simply never matches a real node when the sample is taken.
        self.io
            .record_sent(dataflow, source.node(), source.port(), bytes.len() as u64);

        self.capture_tap(dataflow, source, &metadata, &bytes);
        self.forward_remote(dataflow, source, &metadata, &bytes);

        let routes = match self.state.dataflow(dataflow) {
            Some(state) => state.routes().clone(),
            None => return,
        };
        let mut local = self.take_mailboxes(dataflow, routes.consumers(source));
        let outcome = LocalRouter::deliver(&routes, &mut local, source, metadata, bytes, origin);
        self.restore_mailboxes(dataflow, local);
        self.push_to_waiting_consumers(dataflow, &routes, source);

        // A message is "lost" whether the queue refused the incoming one
        // (backpressure) or evicted an older one to make room (`drop_oldest`).
        // Both are a message the consumer will never see, and §11.2's counter
        // is about that, not about which end of the queue it happened at.
        let evicted = outcome
            .signals
            .iter()
            .filter(|(_, _, report)| report.dropped_something())
            .count();
        let refused = outcome
            .signals
            .iter()
            .filter(|(_, _, report)| !report.accepted() && !report.dropped_something())
            .count();
        self.metrics.record_queue_drops((evicted + refused) as u64);
    }

    /// Pushes to every consumer of `source`, so a consumer that never issued
    /// an explicit `NextEvent` long-poll still sees a message without asking.
    ///
    /// `astrs_node_api::Node::request_events`'s own contract is *"the node
    /// API is push-based: a node that simply reads its stream never needs
    /// this, and the daemon delivers as messages arrive"* — before this
    /// method existed nothing upheld that on this leg: [`Self::deliver_now`]
    /// only ever ran from [`Self::apply_deliver`] (a live `NextEvent`) or
    /// [`Self::flush_pending`] (a *previously* outstanding one), so a
    /// consumer that had never sent either received nothing, ever, no
    /// matter how long it waited — every node in this workspace reads its
    /// stream the plain way, and none sends `NextEvent`, so this path was
    /// exercised by nothing until `astrs-cli`'s end-to-end test tried an
    /// actual two-process publish/subscribe.
    ///
    /// Deliberately iterates `routes.consumers(source)` rather than the
    /// [`crate::local::FanOutOutcome`] `LocalRouter::deliver` returned: that outcome's
    /// `signals` field is *"the per-consumer reports worth logging or
    /// metering"* — an ordinary accepted delivery, with nothing noteworthy
    /// to meter, never appears there at all, so keying off it here would
    /// silently skip the common case (exactly what this method exists to
    /// fix).
    ///
    /// A session already mid-long-poll is unaffected: [`Self::deliver_now`]
    /// drains whatever is queued regardless of who asked for it, so being
    /// served from here first just means [`Self::flush_pending`]'s own pass
    /// (run immediately after, from the same event) finds nothing left and
    /// leaves the (already-satisfied) bookkeeping to the natural next
    /// message.
    ///
    /// # Why the consumer's plane is not consulted
    ///
    /// It used to be: a consumer on [`crate::state::DeliveryPlane::Shm`] was
    /// skipped, on the reasoning that it reads its ring and has nothing queued
    /// here. Two kinds of event disprove that. §6.2's below-threshold rule
    /// keeps small payloads on the control channel even for an upgraded route,
    /// and every *closure* — `InputClosed`, `InputRecovered`, the
    /// route-change pair — is queued here whatever plane the data takes
    /// ([`crate::local::LocalRouter::close`] has never filtered by plane).
    /// Skipping the push left both sitting in the mailbox until some later
    /// message happened to wake the session; a single small message with
    /// nothing behind it waited forever.
    ///
    /// So this is a wake-up, not a delivery decision: a consumer with nothing
    /// queued costs one `deliver_now` that drains nothing and returns 0.
    pub(crate) fn push_to_waiting_consumers(
        &mut self,
        dataflow: DataflowId,
        routes: &crate::state::RouteTable,
        source: &PortRef,
    ) {
        let mut served = std::collections::BTreeSet::new();
        for consumer in routes.consumers(source) {
            if !served.insert(consumer.node.clone()) {
                continue;
            }
            let Some(session) = self
                .state
                .dataflow(dataflow)
                .and_then(|state| state.node(&consumer.node))
                .and_then(|node_state| node_state.session())
            else {
                continue;
            };
            if self.deliver_now(session, astrs_wire::DEFAULT_EVENT_BATCH) > 0 {
                self.pending_delivery.remove(&session);
            }
        }
    }

    /// Closes one producer port on every plane that carries it.
    pub(crate) fn close_source(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        reason: astrs_wire::RouteCloseReason,
    ) {
        self.close_remote_outputs(dataflow, source, reason.clone());
        let routes = match self.state.dataflow(dataflow) {
            Some(state) => state.routes().clone(),
            None => return,
        };
        let mut local = self.take_mailboxes(dataflow, routes.consumers(source));
        LocalRouter::close(&routes, &mut local, source, reason);
        self.restore_mailboxes(dataflow, local);
        // Queueing the closure is not delivering it. A consumer parked in
        // `events.recv()` — §9.1's canonical loop, which sends no
        // `NextEvent` — would otherwise wait for a producer that has already
        // finished, and the graph would never end. Exactly the push
        // `fan_out` performs for an ordinary message.
        self.push_to_waiting_consumers(dataflow, &routes, source);
        self.mark_inputs_closed(dataflow, source);
        self.propagate_input_closure(dataflow);
    }

    /// Closes one or more of a node's outputs.
    ///
    /// `origin` decides *when* the consumers hear about it, which is the whole
    /// of §12's truthful-failure rule — see [`ClosureOrigin`].
    fn apply_close_outputs(
        &mut self,
        session: SessionId,
        outputs: &[DataId],
        origin: ClosureOrigin,
    ) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        let dataflow = binding.dataflow;
        let (closed, generation, alive) = {
            let Some(state) = self.state.dataflow_mut(dataflow) else {
                return;
            };
            let Some(node_state) = state.node_mut(&binding.node) else {
                return;
            };
            // A closure from a *superseded* incarnation closes nothing (§8,
            // §17 `ReplaceNode`). `NodeState` holds exactly one incarnation,
            // so during a cutover's dual-run window the outgoing session is
            // still bound while `node_state` already describes the incoming
            // one — and the outgoing node's teardown would otherwise close the
            // ports its *replacement* had just opened, which is precisely the
            // "no gap in delivery" the cutover is built to provide. The same
            // comparison `handle_session_closed` and `handle_process_exit`
            // already make, for the same reason; an ordinary close (every one
            // that is not a replacement) always finds the two equal.
            //
            // `<` rather than `!=` because only a *stale* binding is silenced.
            // A binding ahead of the node cannot arise today — a binding is
            // created at registration, which is what sets the generation — and
            // if one ever did it would be the node's own current incarnation
            // reporting, not a superseded one, so it must still be heard.
            if binding.generation < node_state.generation() {
                tracing::debug!(
                    %dataflow,
                    node = %binding.node,
                    closing = binding.generation,
                    current = node_state.generation(),
                    "a superseded incarnation closed its outputs; its replacement keeps them"
                );
                return;
            }
            let closed: Vec<DataId> = if outputs.is_empty() {
                node_state.close_all_outputs()
            } else {
                outputs
                    .iter()
                    .filter(|output| node_state.close_output(output))
                    .cloned()
                    .collect()
            };
            (
                closed,
                node_state.generation(),
                ProcessLiveness {
                    // A daemon-owned process this daemon has not reaped: an
                    // exit status is genuinely on its way.
                    unreaped: node_state.handle().is_some(),
                    // Already terminal — the exit has been seen, and either
                    // its consequences have run or they are queued below.
                    terminal: node_state.is_terminal(),
                },
            )
        };
        if closed.is_empty() {
            return;
        }

        let sources: Vec<PortRef> = closed
            .into_iter()
            .map(|output| PortRef::new(binding.node.clone(), output))
            .collect();

        match self.release_for(dataflow, &binding.node, generation, origin, alive) {
            ClosureRelease::AwaitExit => {
                self.defer_closure(dataflow, &binding.node, generation, sources);
                return;
            }
            // The exit already closed these ports with the reason its status
            // justified. Repeating it now would tell the consumer a second,
            // softer story about the same producer.
            ClosureRelease::AlreadySaid => return,
            ClosureRelease::Now => {}
        }

        for source in &sources {
            self.close_source(
                dataflow,
                source,
                astrs_wire::RouteCloseReason::ProducerFinished,
            );
        }
        self.propagate_input_closure(dataflow);
    }

    /// Records, on each consumer, that one of its inputs is closed.
    fn mark_inputs_closed(&mut self, dataflow: DataflowId, source: &PortRef) {
        let pairs: Vec<(NodeId, DataId)> = match self.state.dataflow(dataflow) {
            Some(state) => state
                .routes()
                .consumers(source)
                .iter()
                .map(|consumer| (consumer.node.clone(), consumer.spec.id.clone()))
                .collect(),
            None => return,
        };
        let Some(state) = self.state.dataflow_mut(dataflow) else {
            return;
        };
        for (node, input) in pairs {
            if let Some(node_state) = state.node_mut(&node) {
                node_state.close_input(&input);
            }
        }
    }

    /// Delivers up to `max_batch` queued events to a node.
    ///
    /// A request that finds an empty queue is *remembered* rather than
    /// answered with nothing: [`Daemon::flush_pending`] runs after every
    /// event, so the node is served the moment something arrives. Without
    /// that, a node whose `NextEvent` raced ahead of its producer's
    /// `SendMessage` — which is the common case, since the two arrive from
    /// different tasks — would have to poll, and a polling node is a node
    /// that either spins or adds latency.
    fn apply_deliver(&mut self, session: SessionId, max_batch: u32) {
        let delivered = self.deliver_now(session, max_batch);
        if delivered == 0 {
            // Parked in `NextEvent`: the node reached the top of its loop and
            // the silence that follows is the daemon's, not the node's (§12).
            self.park_health(session, Instant::now());
            self.pending_delivery.insert(session, max_batch);
        } else {
            self.pending_delivery.remove(&session);
        }
    }

    /// Drains up to `max_batch` events into one session's sink.
    ///
    /// Returns how many were sent.
    ///
    /// Never drains more than the outbox can accept right now.
    /// [`crate::local::NodeMailbox::drain`] *removes* what it returns, so an
    /// event drained into a full outbox would be lost outright — with none
    /// of the per-input policy §11.2 asks for applied to the loss, and
    /// nothing counted. Leaving it queued instead keeps a node that has
    /// stopped reading its socket behind exactly the bounded, policy-aware,
    /// metered queue it is supposed to be behind: the mailbox is where
    /// `queue_size`/`queue_policy` live, and the outbox is not a second,
    /// silent one.
    pub(crate) fn deliver_now(&mut self, session: SessionId, max_batch: u32) -> usize {
        let Some(binding) = self.state.session(session).cloned() else {
            return 0;
        };
        // Looked up before the drain for the same reason: a session that has
        // already ended must not consume the events it can no longer carry.
        let Some(sink) = self.sinks.get(&session) else {
            return 0;
        };
        let room = sink.capacity().min(max_batch as usize);
        if room == 0 {
            return 0;
        }
        let batch = match self
            .mailboxes
            .get(&(binding.dataflow, binding.node.clone()))
        {
            Some(mailbox) => mailbox.drain(room),
            None => return 0,
        };
        let mut sent = 0;
        for (_, event) in batch {
            if sink.try_send(event).is_err() {
                break;
            }
            sent += 1;
        }
        sent
    }

    /// Serves every session whose `NextEvent` is still outstanding.
    ///
    /// Called once after each handled event, which is the only point at which
    /// a mailbox can have gained anything.
    pub(crate) fn flush_pending(&mut self) {
        let waiting: Vec<(SessionId, u32)> = self
            .pending_delivery
            .iter()
            .map(|(session, batch)| (*session, *batch))
            .collect();
        let now = Instant::now();
        for (session, max_batch) in waiting {
            if self.deliver_now(session, max_batch) > 0 {
                self.pending_delivery.remove(&session);
                // Served: the node's own deadline starts again from here.
                if let Some(binding) = self.state.session(session).cloned() {
                    self.health.unpark(binding.dataflow, &binding.node, now);
                }
            }
        }
    }

    /// Relays a node's own measured deadline (§11.3) violation onto
    /// `astrs/status` and counts it — [`crate::server::core::Daemon::notify_peers_deadline_violated`],
    /// resolved from the reporting session's identity.
    fn apply_deadline_violation(
        &mut self,
        session: SessionId,
        input: DataId,
        budget: DurationMs,
        latency: DurationMs,
    ) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        self.notify_peers_deadline_violated(
            binding.dataflow,
            &binding.node,
            input,
            budget,
            latency,
        );
    }

    /// Stores a value in the dataflow's extension table (§2.1).
    fn apply_ext_store(
        &mut self,
        session: SessionId,
        key: ExtensionKey,
        value: Vec<u8>,
        ttl: Option<std::time::Duration>,
    ) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        let Some(state) = self.state.dataflow_mut(binding.dataflow) else {
            return;
        };
        if let Err(error) = state.extensions_mut().store(&binding.node, key, value, ttl) {
            self.refuse(session, error);
        }
    }

    /// Reads a value and answers with [`NodeEvent::ExtValue`].
    fn apply_ext_load(&mut self, session: SessionId, key: ExtensionKey) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        let value = self.state.dataflow_mut(binding.dataflow).and_then(|state| {
            state
                .extensions_mut()
                .load(&binding.node, &key)
                .map(<[u8]>::to_vec)
        });
        self.send_to(
            binding.dataflow,
            &binding.node,
            NodeEvent::ExtValue { key, value },
        );
    }

    /// Drops a value the node owns, telling everyone who read it.
    fn apply_ext_drop(&mut self, session: SessionId, key: &ExtensionKey) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        let dropped = self
            .state
            .dataflow_mut(binding.dataflow)
            .and_then(|state| state.extensions_mut().drop_key(&binding.node, key));
        if let Some(entry) = dropped {
            let event = NodeEvent::ExtDropped {
                key: entry.key.clone(),
                reason: entry.reason_text(),
            };
            for interested in &entry.interested {
                if interested != &binding.node {
                    self.send_to(binding.dataflow, interested, event.clone());
                }
            }
        }
    }

    /// Turns a protocol refusal into a daemon error.
    fn apply_refusal(&mut self, session: SessionId, reason: &RefusalReason) {
        let dataflow = self
            .state
            .session(session)
            .map_or(DataflowId::from_u128(0), |binding| binding.dataflow);
        let error = reason.to_error(session, dataflow);
        self.refuse(session, error);
    }

    /// Records a refusal.
    ///
    /// A refusal is a *log line*, not a wire message: the daemon↔node protocol
    /// has no error reply, deliberately — a node that asks for something
    /// impossible has a bug, and the operator needs to see it, while the node
    /// itself has no sensible recovery beyond the one it already has (its
    /// request had no effect).
    fn refuse(&mut self, session: SessionId, error: DaemonError) {
        self.last_refusal = Some((session, error));
    }

    /// Handles a node's connection ending — clean exit or crash, identically.
    ///
    /// A no-op past unbinding the session itself for a *superseded*
    /// incarnation's connection closing (§8, §17
    /// [`Daemon::apply_replace_node`]): [`crate::state::NodeState`] holds
    /// exactly one incarnation, so by the time an outgoing incarnation's
    /// session actually closes, [`crate::state::NodeState::generation`]
    /// already reports the *new* one — reclaiming extensions or
    /// discharging a pending exit here would act on the new incarnation's
    /// state under the pretext of the old one leaving. Comparing the
    /// closing session's own recorded generation
    /// ([`crate::state::registry::SessionBinding::generation`]) against
    /// the node's current one is what tells the two apart; an ordinary
    /// session close (the overwhelming majority — nothing before this
    /// feature could produce any other kind) always finds them equal.
    pub(crate) fn handle_session_closed(&mut self, session: SessionId) {
        self.sinks.remove(&session);
        self.protocols.remove(&session);
        self.pending_delivery.remove(&session);
        let Some(binding) = self.state.unbind_session(session) else {
            return;
        };
        let dataflow = binding.dataflow;
        let node = binding.node;

        let current_generation = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(&node))
            .map(crate::state::NodeState::generation);
        if current_generation.is_some_and(|current| current != binding.generation) {
            return;
        }

        // The extension entries go now, whatever happens to the process: a
        // handle whose owner's socket is gone is a dangling handle (§2.1).
        self.reclaim_extensions(dataflow, &node);

        // This is the point the exit was waiting for. Everything the node
        // sent has been read and routed — the session actor forwards it all
        // before reporting itself closed — so its outputs can now be closed
        // without losing the tail of any stream. See
        // [`Self::handle_process_exit`].
        self.discharge_pending_exit(dataflow, &node);

        // A node whose process is still alive may simply have dropped its
        // event stream; the process exit is what drives the lifecycle, and it
        // will arrive on its own. Only a node with no live process is finished
        // here — a dynamic node, which nobody spawned and nobody will reap.
        let has_process = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(&node))
            .is_some_and(|state| state.handle().is_some());
        if has_process {
            return;
        }

        let is_dynamic = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(&node))
            .is_some_and(crate::state::NodeState::is_dynamic);
        if is_dynamic
            && let Some(state) = self.state.dataflow_mut(dataflow)
            && let Some(node_state) = state.node_mut(&node)
            && !node_state.is_terminal()
        {
            let generation = node_state.generation();
            node_state.mark_exited(NodeExitCause::Success);
            self.after_exit(dataflow, &node, generation, NodeExitCause::Success);
        }
    }

    /// Handles a reaped child.
    pub(crate) fn handle_process_exit(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
        status: Result<ExitStatus, String>,
    ) {
        let intent = {
            let Some(state) = self.state.dataflow(dataflow) else {
                return;
            };
            let Some(node_state) = state.node(node) else {
                return;
            };
            if node_state.generation() != generation {
                // A report about an incarnation that has already been
                // replaced. The generation stamp is what makes ignoring it
                // safe rather than merely convenient.
                return;
            }
            node_state.intent()
        };

        let cause = match status {
            Ok(status) => classify_with_intent(status, intent),
            Err(message) => NodeExitCause::SpawnFailed { message },
        };

        if let Some(state) = self.state.dataflow_mut(dataflow)
            && let Some(node_state) = state.node_mut(node)
        {
            node_state.mark_exited(cause.clone());
        }

        // A process exit and the node's own frames reach the loop from two
        // different tasks: the waiter that reaped the child, and the session
        // actor still reading its socket. The child can therefore be reported
        // gone while its *last publishes are still unread*, and closing its
        // outputs here would tell every consumer the stream ended before
        // those messages were routed — losing the tail of it. That is the M1
        // pipeline defect exactly: a 12-frame run recording 11.
        //
        // So the consequences wait for `SessionClosed`, which the session
        // actor emits *after* forwarding everything it read, on the same
        // channel and therefore ordered behind it. `DRAIN_GRACE` bounds the
        // wait: a rendezvous that cannot complete would be a worse failure
        // than the lost tail it exists to prevent.
        //
        // A node with no bound session — a spawn failure, or a deadline that
        // fired before it ever registered — has nothing to wait for and is
        // finished here, as before.
        if self.state.session_of(dataflow, node).is_some() {
            self.draining_exits.insert(
                (dataflow, node.clone()),
                crate::server::core::PendingExit {
                    generation,
                    cause: cause.clone(),
                },
            );
            self.drain_deadlines.arm_in(
                (dataflow, node.clone()),
                generation,
                Instant::now(),
                crate::server::core::DRAIN_GRACE,
            );
            return;
        }

        self.after_exit(dataflow, node, generation, cause);
    }

    /// Runs the consequences of an exit that was waiting for its session.
    ///
    /// A no-op unless [`Self::handle_process_exit`] deferred one, and then
    /// only for the incarnation that actually exited: a deferral belongs to
    /// its generation, so a node that has already been restarted keeps its
    /// new incarnation's outputs open.
    pub(crate) fn discharge_pending_exit(&mut self, dataflow: DataflowId, node: &NodeId) {
        let key = (dataflow, node.clone());
        let Some(pending) = self.draining_exits.remove(&key) else {
            return;
        };
        self.drain_deadlines
            .disarm_generation(&key, pending.generation);
        self.after_exit(dataflow, node, pending.generation, pending.cause);
    }

    /// Discharges every drain wait that ran out at `now`.
    pub(crate) fn fire_drain_deadlines(&mut self, now: Instant) {
        for expiry in self.drain_deadlines.expired(now) {
            let (dataflow, node) = expiry.key.clone();
            let stale = self
                .draining_exits
                .get(&(dataflow, node.clone()))
                .is_none_or(|pending| pending.generation != expiry.generation);
            if stale {
                continue;
            }
            tracing::warn!(
                %dataflow,
                %node,
                "a node's session did not close after its process exited; \
                 closing its outputs without it"
            );
            self.discharge_pending_exit(dataflow, &node);
        }
    }

    /// Handles a log record a node emitted through the §9.1 logging API.
    ///
    /// The wire twin of [`Self::handle_node_output`]: a captured stdout line
    /// and a `Node::log_info` call are the same thing to everything
    /// downstream — the `astrs/logs/*` virtual inputs (§8.4) and whatever
    /// [`crate::health::ReportSink`] the embedder installed (`astrs run`'s
    /// terminal streamer) — so both end in the same two places. What differs
    /// is only where the record was built: this one arrived already
    /// structured, with fields, a level the node chose and the node's own HLC
    /// reading, none of which a captured line can carry.
    ///
    /// The frame's `subscription` is deliberately ignored. A node stamps its
    /// *own* [`astrs_wire::SubscriptionId`] on the records it emits (it has
    /// no other), while the daemon's subscription table is keyed for
    /// `astrs/logs/*` consumers; looking one up against the other would match
    /// nothing and silently drop every record a node ever logged.
    pub(crate) fn handle_node_log(&mut self, session: SessionId, record: &WireLogRecord) {
        let Some(binding) = self.state.session(session).cloned() else {
            // A record from a session the daemon has already unbound: the
            // node is gone and there is nobody left to attribute it to.
            return;
        };
        // Speaking is the liveness signal (§12), and a node that logs has
        // spoken — exactly as it has when it sends a request.
        self.touch_health(session, Instant::now());

        // Attributed by the daemon, not by the record: the session binding is
        // who the sender *is*, and a record that arrived with no node (or
        // with somebody else's) must not be reported as either. The node's
        // own `Node::log_*` fills these in identically, so this is a
        // confirmation on the happy path and a correction otherwise.
        let mut attributed = record.clone();
        attributed.node = Some(binding.node.clone());
        attributed.dataflow = Some(binding.dataflow);
        // Both log paths (§13): the push, which a live `astrs logs -f`
        // subscriber reads through the coordinator, and the bounded ring a
        // later `astrs logs` pull is answered from.
        self.record_log(attributed.clone());
        self.sink.report(DaemonEvent::Log {
            request: None,
            records: vec![attributed],
            truncated: false,
        });

        let mut local = astrs_log::LogRecord::new(
            record.timestamp,
            wire_level_to_log(record.level),
            record.target.clone(),
            record.message.clone(),
        )
        .with_node(binding.node.as_str());
        for (key, value) in &record.fields {
            local = local.with_field(key.clone(), value.clone());
        }
        self.fan_out_log(binding.dataflow, &local);
    }

    /// Handles a captured output line (§13, §8.4).
    pub(crate) fn handle_node_output(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        stream: StdioStream,
        line: &str,
    ) {
        let record = LogRecord::from_captured_output(self.clock.now(), node.as_str(), stream, line);
        self.fan_out_log(dataflow, &record);
        let wire_record = self.report_captured_log(node, &record);
        // The pull path (§17 `astrs logs`) needs the dataflow the push path
        // does not carry here — the record was attributed by node alone.
        self.record_log(wire_record.with_dataflow(dataflow));
    }

    /// Forwards a captured line to [`crate::health::ReportSink`] as a wire
    /// [`DaemonEvent::Log`] push (`request: None`, one record, never
    /// truncated), so an embedder that installed a non-null sink — chiefly
    /// `astrs run`'s terminal log streamer (blueprint §17: "stream logs to
    /// the terminal") — observes every node's stdout/stderr live, not only
    /// the in-graph `astrs/logs/*` virtual-input fan-out
    /// [`Self::fan_out_log`] already provides.
    ///
    /// A no-op beyond [`crate::health::ReportSink::dropped`]'s own counter
    /// under the default [`crate::health::NullSink`] — every cluster
    /// daemon today, and `astrs run` unless it calls
    /// [`Daemon::set_sink`](crate::server::core::Daemon::set_sink) first.
    ///
    /// [`astrs_log::LogRecord`] and [`astrs_wire::LogRecord`] are
    /// deliberately distinct types (this crate's own module docs: the
    /// former has no `astrs-wire` dependency at all), so this is a small,
    /// lossy-only-in-`fields` bridge between them — a captured stdout/
    /// stderr line never carries structured fields in the first place, so
    /// nothing this call site ever produces actually has any to lose.
    ///
    /// Returns the wire record it reported, so the caller — which knows the
    /// dataflow this one does not — can file the same record in the pull-path
    /// ring without building it twice.
    fn report_captured_log(&self, node: &NodeId, record: &LogRecord) -> WireLogRecord {
        let level = match record.level {
            astrs_log::LogLevel::Error => LogLevel::Error,
            astrs_log::LogLevel::Warn => LogLevel::Warn,
            astrs_log::LogLevel::Info => LogLevel::Info,
            astrs_log::LogLevel::Debug => LogLevel::Debug,
            astrs_log::LogLevel::Trace => LogLevel::Trace,
        };
        let wire_record = WireLogRecord::new(record.hlc, level, record.message.clone())
            .with_target(record.target.clone())
            .with_node(node.clone());
        self.sink.report(DaemonEvent::Log {
            request: None,
            records: vec![wire_record.clone()],
            truncated: false,
        });
        wire_record
    }

    /// Delivers one log record to every `astrs/logs/*` subscriber (§8.4).
    pub(crate) fn fan_out_log(&mut self, dataflow: DataflowId, record: &LogRecord) {
        let subscribers: Vec<(NodeId, DataId)> = self
            .log_subscriptions
            .matching(dataflow, record)
            .map(|subscriber| (subscriber.node.clone(), subscriber.input.clone()))
            .collect();
        if subscribers.is_empty() {
            return;
        }
        let Ok(json) = astrs_log::format::json(record) else {
            // A record that will not serialize cannot be fanned out; the
            // only field that can fail is a caller-supplied structured value,
            // and dropping one record beats stalling the loop.
            return;
        };
        let payload = json.into_bytes();
        let source = PortRef::new(NodeId::sanitized("astrs"), crate::local::logs_port());
        for (node, input) in &subscribers {
            let event = NodeEvent::Input {
                id: input.clone(),
                source: source.clone(),
                metadata: Metadata::new(record.hlc),
                payload: payload.clone(),
            };
            self.mailbox(dataflow, node).push(input, event);
        }
        // As every other queueing path: a subscriber that is simply reading
        // its event stream must not have to ask for what it subscribed to.
        for (node, _) in subscribers {
            self.push_to_node(dataflow, &node);
        }
    }

    /// Delivers whatever is queued for one node, if it has a live session.
    ///
    /// The single-node twin of [`Self::push_to_waiting_consumers`], for the
    /// paths that queue by node rather than by route (`astrs/logs/*`,
    /// `astrs/timer/*`).
    pub(crate) fn push_to_node(&mut self, dataflow: DataflowId, node: &NodeId) {
        let Some(session) = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(node))
            .and_then(crate::state::NodeState::session)
        else {
            return;
        };
        if self.deliver_now(session, astrs_wire::DEFAULT_EVENT_BATCH) > 0 {
            self.pending_delivery.remove(&session);
        }
    }

    /// Respawns a node whose backoff has elapsed (§12).
    pub(crate) fn handle_restart_due(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
    ) {
        let ready = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(node))
            .is_some_and(|state| {
                state.generation() == generation
                    && state.run_state() == astrs_wire::NodeRunState::Restarting
            });
        if !ready {
            return;
        }
        let next = {
            let Some(state) = self.state.dataflow_mut(dataflow) else {
                return;
            };
            let Some(node_state) = state.node_mut(node) else {
                return;
            };
            node_state.begin_next_generation()
        };
        self.notify_peers_restarted(dataflow, node, next);
        self.spawn_node(dataflow, node);
    }

    /// Spawns one node's current incarnation.
    pub fn spawn_node(&mut self, dataflow: DataflowId, node: &NodeId) {
        let spec = {
            let Some(state) = self.state.dataflow(dataflow) else {
                return;
            };
            let Some(node_state) = state.node(node) else {
                return;
            };
            if node_state.is_dynamic() {
                if let Some(state) = self.state.dataflow_mut(dataflow)
                    && let Some(node_state) = state.node_mut(node)
                {
                    node_state.mark_awaiting_attach();
                }
                return;
            }
            node_state.spec().clone()
        };

        let config = self.node_config(&spec);
        let request = match SpawnRequest::new(&spec, &config) {
            Ok(request) => request
                .with_literal_env(spec.env.clone())
                .with_stdio(StdioMode::Capture)
                .with_run_parent_pid(self.config.run_parent_pid()),
            Err(error) => {
                self.fail_spawn(dataflow, node, spec.generation, &error);
                return;
            }
        };

        let spawned = match self.spawner.spawn(request) {
            Ok(spawned) => spawned,
            Err(error) => {
                self.fail_spawn(dataflow, node, spec.generation, &error);
                return;
            }
        };

        let pid = spawned.pid();
        if let Some(state) = self.state.dataflow_mut(dataflow)
            && let Some(node_state) = state.node_mut(node)
        {
            node_state.mark_spawning(pid);
        }

        let deadline = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(node))
            .and_then(crate::state::NodeState::spawn_deadline)
            .unwrap_or(self.config.spawn_deadline());
        self.spawn_deadlines.arm_in(
            (dataflow, node.clone()),
            spec.generation,
            Instant::now(),
            deadline,
        );

        let (handle, child, stdout, stderr) = spawned.into_parts();
        if let Some(stdout) = stdout {
            spawn_log_pump(
                self.handle.clone(),
                dataflow,
                node.clone(),
                StdioStream::Stdout,
                stdout,
            );
        }
        if let Some(stderr) = stderr {
            spawn_log_pump(
                self.handle.clone(),
                dataflow,
                node.clone(),
                StdioStream::Stderr,
                stderr,
            );
        }
        spawn_waiter(self.handle.clone(), handle, child);
    }

    /// Records a spawn that never started.
    fn fail_spawn(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
        error: &DaemonError,
    ) {
        let cause = NodeExitCause::SpawnFailed {
            message: error.to_string(),
        };
        if let Some(state) = self.state.dataflow_mut(dataflow)
            && let Some(node_state) = state.node_mut(node)
        {
            node_state.mark_exited(cause.clone());
        }
        self.after_exit(dataflow, node, generation, cause);
    }

    /// The handshake blob one node is spawned with (§24.2).
    fn node_config(&self, spec: &astrs_wire::NodeSpawnSpec) -> astrs_wire::NodeConfig {
        let mut config = astrs_wire::NodeConfig::new(
            spec.clone(),
            self.config.id().clone(),
            self.config.auth().clone(),
        )
        .with_endpoints(self.config.listen().endpoints())
        .with_limits(self.config.limits())
        .with_zero_copy_threshold(self.config.zero_copy_threshold())
        .with_deterministic(self.config.deterministic());
        config.working_dir = Some(self.config.working_dir().display().to_string());
        // Where the node dials to attach a ring (§6.2, §24.2). Absent when the
        // plane is off or unsupported, which is exactly what tells the node to
        // stay on the reliable daemon path.
        if let Some(socket) = self.shm.socket_path() {
            config = config.with_shm_broker(socket.display().to_string());
        }
        config
    }

    /// Tells graph peers a node came back (§12).
    fn notify_peers_restarted(&mut self, dataflow: DataflowId, node: &NodeId, generation: u64) {
        if generation == 0 {
            return;
        }
        let peers: Vec<NodeId> = match self.state.dataflow(dataflow) {
            Some(state) => state.routes().downstream_of(node).into_iter().collect(),
            None => return,
        };
        let event = NodeEvent::Restarted {
            peer: node.clone(),
            generation,
        };
        for peer in peers {
            self.send_to(dataflow, &peer, event.clone());
        }
        self.recover_inputs_of(dataflow, node, generation);
    }

    /// Tells consumers of a restarted node's outputs that they are live again.
    fn recover_inputs_of(&mut self, dataflow: DataflowId, node: &NodeId, generation: u64) {
        let routes = match self.state.dataflow(dataflow) {
            Some(state) => state.routes().clone(),
            None => return,
        };
        let sources: Vec<PortRef> = routes.produced_by(node).into_iter().cloned().collect();
        for source in &sources {
            let mut local = self.take_mailboxes(dataflow, routes.consumers(source));
            LocalRouter::recover(&routes, &mut local, source, generation);
            self.restore_mailboxes(dataflow, local);
        }
    }

    /// Moves the ready barrier forward when every spawned node has registered.
    fn check_ready_barrier(&mut self, dataflow: DataflowId) {
        let Some(state) = self.state.dataflow_mut(dataflow) else {
            return;
        };
        if state.status() == astrs_wire::DataflowStatus::Starting && state.ready_barrier_met() {
            state.set_status(astrs_wire::DataflowStatus::Running);
        }
    }

    /// Temporarily removes the mailboxes a fan-out will touch.
    ///
    /// The borrow checker will not let the router hold `&RouteTable` from
    /// `self.state` and `&mut self.mailboxes` at once; moving the affected
    /// mailboxes out and back is cheaper than cloning the queues and keeps the
    /// router's signature honest about what it mutates.
    fn take_mailboxes(
        &mut self,
        dataflow: DataflowId,
        consumers: &[crate::state::Consumer],
    ) -> std::collections::BTreeMap<NodeId, NodeMailbox> {
        let mut local = std::collections::BTreeMap::new();
        for consumer in consumers {
            // A consumer on [`crate::state::DeliveryPlane::Remote`] is a node
            // on another machine (§6.4): it has no process here, so it has no
            // mailbox here, and creating one would queue payloads nobody ever
            // drains. Its copy travels over the peer leg
            // (`Daemon::forward_remote`, run before this fan-out) and its
            // closures arrive as `PeerEvent::OutputClosed`.
            if !consumer.plane.is_local() {
                continue;
            }
            let key = (dataflow, consumer.node.clone());
            let mailbox = self.mailboxes.remove(&key).unwrap_or_default();
            local.insert(consumer.node.clone(), mailbox);
        }
        local
    }

    /// Puts them back.
    fn restore_mailboxes(
        &mut self,
        dataflow: DataflowId,
        local: std::collections::BTreeMap<NodeId, NodeMailbox>,
    ) {
        for (node, mailbox) in local {
            self.mailboxes.insert((dataflow, node), mailbox);
        }
    }

    /// The status port every lifecycle event is delivered on.
    #[must_use]
    pub fn status_port() -> DataId {
        status_port()
    }

    /// The dataflow record for `id`, for a caller inspecting a finished run.
    #[must_use]
    pub fn dataflow(&self, id: DataflowId) -> Option<&DataflowState> {
        self.state.dataflow(id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use astrs_wire::WireMessage;

    use super::*;
    use crate::config::{DaemonConfig, ListenConfig, RuntimePaths};
    use crate::health::RecordingSink;

    fn config() -> DaemonConfig {
        let root = std::env::temp_dir().join(format!(
            "astrs-daemon-handlers-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        DaemonConfig::new(RuntimePaths::under(root)).with_listen(ListenConfig::none())
    }

    /// [`Daemon::handle_node_output`] must forward to a non-null
    /// [`crate::health::ReportSink`] as a wire [`DaemonEvent::Log`] push —
    /// the seam `astrs run`'s terminal log streamer relies on (blueprint
    /// §17), proven here without a socket or a real child process.
    #[tokio::test]
    async fn captured_output_reaches_a_non_null_sink_as_a_log_push() {
        let mut daemon = Daemon::new(config()).unwrap();
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("camera").unwrap();

        daemon.handle_node_output(dataflow, &node, StdioStream::Stdout, "hello from camera");

        let events = sink.take();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            DaemonEvent::Log {
                request,
                records,
                truncated,
            } => {
                assert!(request.is_none(), "a push, not an answer to a request");
                assert!(!truncated);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].message, "hello from camera");
                assert_eq!(records[0].node.as_ref(), Some(&node));
                assert_eq!(records[0].level, LogLevel::Info);
            }
            other => panic!("expected Log, got {} ({other:?})", other.variant_name()),
        }
    }

    /// The default [`crate::health::NullSink`] — every daemon that never
    /// calls [`Daemon::set_sink`] — must keep working exactly as before
    /// this forwarding was added: no panic, and the drop counter moves.
    #[tokio::test]
    async fn captured_output_is_harmless_under_the_default_null_sink() {
        let mut daemon = Daemon::new(config()).unwrap();
        let dropped_before = daemon.sink().dropped();
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("camera").unwrap();

        daemon.handle_node_output(dataflow, &node, StdioStream::Stderr, "still fine");

        assert!(daemon.sink().dropped() > dropped_before);
    }

    /// Every [`astrs_log::LogLevel`] maps onto its
    /// [`astrs_wire::LogLevel`] namesake, not just the default `Info` the
    /// two tests above exercise.
    #[tokio::test]
    async fn every_captured_level_maps_onto_its_wire_namesake() {
        let mut daemon = Daemon::new(config()).unwrap();
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());
        let node = NodeId::new("camera").unwrap();
        let record = LogRecord::new(
            daemon.clock.now(),
            astrs_log::LogLevel::Trace,
            "target",
            "m",
        );

        daemon.report_captured_log(&node, &record);

        let events = sink.take();
        match &events[0] {
            DaemonEvent::Log { records, .. } => {
                assert_eq!(records[0].level, LogLevel::Trace);
                assert_eq!(records[0].target, "target");
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }
}
