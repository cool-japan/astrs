//! Live topology ops on an already-running dataflow (blueprint §8, §17:
//! `astrs node add/remove/replace/connect/disconnect`).
//!
//! Every method here is a **domain-typed** `impl Daemon` method —
//! `DataflowId`/`NodeId`/`NodeSpawnSpec`/[`astrs_wire::InputSpec`], never a
//! [`astrs_wire::CoordinatorEvent`] — deliberately, so a caller can reach
//! them directly (this crate's own tests do, over a real UDS/TCP session,
//! exactly as [`crate::dataflow::run_dataflow_with`]'s single-process mode
//! would need to) without a coordinator in the loop at all. The
//! coordinator↔daemon wire carrier for these
//! ([`astrs_wire::CoordinatorEvent::ReplaceNode`]/`AddEdge`/`RemoveEdge`,
//! and the pre-existing `Spawn`/`StopNode` for add/remove) is exactly that
//! thin translation: `crate::coordinator::apply`'s `handle_coordinator_frame`
//! dispatches each one straight onto the matching method below.
//!
//! # Add
//!
//! [`Daemon::apply_add_node`] is [`crate::state::DataflowState::add_node`]
//! plus an optional [`Daemon::spawn_node`] — both already fully general
//! (nothing about them assumes "at dataflow start"), so a node added long
//! after `Running` goes through the exact same registration, SHM
//! slow-start (§6.3) and health-arming path as one the manifest declared.
//!
//! Not covered: a dynamically added node with an `astrs/timer/*` or
//! `astrs/logs/*` input. Those are registered with the daemon's timer
//! wheel / log filters only by [`crate::server::core::Daemon::admit`]'s
//! own `subscribe_virtual_inputs` pass over a whole
//! [`crate::dataflow::DataflowPlan`] — a dynamic add has no plan, only a
//! [`astrs_wire::NodeSpawnSpec`], and re-deriving one virtual
//! subscription's parsed period from a bare `InputSpec` is future work,
//! tracked rather than silently guessed at here.
//!
//! # Remove
//!
//! [`Daemon::apply_remove_node`] asks the node to stop exactly as
//! [`Daemon::stop_node`] already does (cooperative ask, then the
//! finish-straggler ladder) — the *cleanly* in "tears routes down
//! cleanly" is `Daemon::after_exit`'s existing `close_outputs_of`,
//! unchanged, run whenever the process actually ends and reading exactly
//! the [`crate::state::RouteTable`] entries this method leaves alone.
//! Nothing here scrubs them up front: `close_outputs_of` needs them
//! *present* to know who to tell, and a node that stops for good today —
//! an ordinary crash with no restart budget left, an ordinary `StopNode`
//! — already leaves its own consumer-side entries in the table forever
//! with no ill effect, which is the same shape a removed node ends up in.
//! The coordinator's own tracked graph, separately, already drops the
//! node (and every edge that named it) the moment `RemoveNode` validates
//! — see `crate::graph_bridge::topology_ops_for_remove_node` — so a
//! future edge op naming it is rejected there, before ever reaching here.
//!
//! # Replace — the dual-run window
//!
//! [`Daemon::apply_replace_node`] is the one op this state model cannot
//! represent as a single-generation swap: [`crate::state::NodeState`]
//! holds exactly one incarnation, but "spawn the new generation, *then*
//! stop the old one" needs both alive at once, briefly. The design:
//!
//! 1. Every output the node currently produces is forced off the
//!    shared-memory plane and its segment is released
//!    ([`crate::shm::ShmPlane::release`]) *before* anything else — two
//!    live producers racing to publish into the same ring would corrupt
//!    it, and skipping this node's ordinary exit path (step 4 explains
//!    why) means nothing else will ever reclaim the outgoing generation's
//!    segment (§6.2's crash-safety promise, upheld even though this is
//!    not a crash).
//! 2. The outgoing incarnation's [`crate::spawn::ProcessHandle`] and
//!    [`astrs_wire::SessionId`] are copied into `Daemon::superseded` —
//!    not moved out of [`crate::state::NodeState`], which will be
//!    overwritten in step 3 — and told, cooperatively, to stop
//!    ([`astrs_wire::NodeEvent::Stop`] with
//!    [`astrs_wire::StopCause::Replaced`]).
//! 3. [`crate::state::NodeState::begin_next_generation`], *then*
//!    [`crate::state::NodeState::replace_spec`], then
//!    [`crate::state::NodeState::set_generation`] — in that order, so the
//!    bump is computed from the outgoing incarnation's own real
//!    generation rather than from whatever placeholder the caller (which
//!    has no reliable way to know this daemon's live counter) put in the
//!    replacement's `NodeSpawnSpec::generation`; see
//!    [`Daemon::apply_replace_node`]'s own inline comment for the
//!    collision calling these in the more natural "swap the spec, then
//!    bump" order would let two incarnations land on the same generation
//!    number. [`Daemon::spawn_node`] then starts the new one — using the
//!    *ordinary* spawn/register/health-arm path, no different from any
//!    other node's first spawn.
//! 4. Nothing here calls `Daemon::after_exit` for the outgoing
//!    incarnation, deliberately: `Daemon::handle_process_exit`'s
//!    existing generation-stamp guard already discards a report whose
//!    generation no longer matches what [`crate::state::NodeState`]
//!    tracks, and by step 3 that is always the *new* one. The outgoing
//!    process's outputs are therefore never explicitly closed — which is
//!    the entire point: [`crate::state::RouteTable`] is keyed by producer
//!    port, not by generation, so either incarnation publishing into it
//!    reaches the same consumers with no `InputClosed`/`InputRecovered`
//!    blip, and neither incarnation's in-flight messages are lost. This
//!    is the brief dual-run window the task brief names — real at the OS
//!    process level, bounded by `Daemon::fire_replace_supersessions`
//!    below, and never represented as two live entries in one
//!    [`crate::state::NodeState`].
//! 5. Both incarnations stay bound in
//!    [`crate::state::registry::DaemonState`] for as long as the window
//!    lasts — deliberately: `Daemon::apply_publish` resolves a
//!    publisher by its own exact session id
//!    ([`crate::state::registry::DaemonState::session`], never
//!    ambiguous), and the outgoing incarnation needs that lookup to keep
//!    succeeding for as long as it keeps publishing. What *does* need
//!    disambiguating is the other direction — a node-id lookup like
//!    `Daemon::send_to`'s, which the new incarnation's own
//!    [`astrs_wire::NodeEvent::Registered`] rides — and
//!    [`crate::state::registry::DaemonState::session_of`] is where that
//!    is fixed: it prefers whichever bound session's recorded generation
//!    matches [`crate::state::NodeState::generation`]'s current value,
//!    falling back to any match only when nothing does (a dataflow this
//!    crate's own low-level state tests build directly, with no
//!    [`crate::state::NodeState`] wired up at all). The outgoing
//!    session's own eventual close then runs the ordinary
//!    `Daemon::handle_session_closed` path — reclaiming *its own*
//!    extension entries and discharging *its own* pending exit, which is
//!    correct precisely because nothing here pointed either at the wrong
//!    incarnation.
//!
//! What this deliberately does not attempt: resurrecting the outgoing
//! incarnation if the new one fails to register. `Daemon::after_exit`
//! for the *new* generation's failure runs
//! `Daemon::resolve_superseded`, which accelerates the outgoing
//! incarnation's own kill ladder rather than leaving it running
//! un-managed forever — a failed replace falls back to the new spec's
//! own restart policy, the same recovery path a node that crash-loops on
//! an ordinary restart already has, not a rollback to the pre-replace
//! code.
//!
//! ## Continuity guarantee, by queue policy
//!
//! The dual-run window above is what makes *no message loss* possible
//! across a cutover, but whether it actually holds depends on the
//! consumer's own `queue_policy` (blueprint §11.2) — this module changes
//! nothing about that, deliberately:
//!
//! - The default (reliable) queue never evicts on its own, so every
//!   message from both incarnations that fits ahead of the consumer's own
//!   drain rate arrives, in order, with nothing this window does costing
//!   it anything.
//! - A `queue_policy: drop_oldest` input keeps its own eviction contract
//!   unchanged: at its configured `queue_size` ceiling it evicts the
//!   oldest *non-immune* queued message to make room for a new one,
//!   whichever incarnation either came from — the replace introduces no
//!   *additional* loss beyond what publishing that fast into that small a
//!   queue already implies. A message correlated via
//!   `request_id`/`goal_id`/`goal_status` is exempt from eviction
//!   (`astrs_scheduler::queue`'s own contract) and this holds across the
//!   cutover exactly as it does within one incarnation — the queue has no
//!   notion of "which generation produced this," only of immunity.
//!
//! See `crates/astrs-daemon/tests/dynamic_topology.rs`'s
//! `replace_under_load_with_drop_oldest_bounds_ordinary_loss_but_spares_correlated_messages`
//! for this worked out against the real mailbox path, including the exact
//! surviving sequence a two-eviction scenario produces.
//!
//! # Edge ops
//!
//! [`Daemon::apply_add_edge`]/[`Daemon::apply_remove_edge`] are thin: the
//! coordinator has already validated the input against the graph
//! (`astrs_graph::apply`'s referential-integrity check), so by the time
//! either reaches here the input id is one the consumer genuinely
//! declared. [`crate::state::RouteTable::insert`] already *replaces*
//! rather than duplicates an entry for the same `(node, input)` pair,
//! which is exactly [`astrs_graph::diff`]'s own "rewire" semantics (one
//! `AddEdge`, never a `RemoveEdge`/`AddEdge` pair, when only a producer
//! changed) — so add and rewire are the same call, and neither sends the
//! consumer any signal: its named input keeps receiving, just from a
//! different producer, which is the whole point of a live rewire.
//! [`Daemon::apply_remove_edge`] is the one edge op that *does* signal —
//! [`astrs_wire::NodeEvent::InputClosed`] with
//! [`astrs_wire::RouteCloseReason::Disconnected`], since the consumer's
//! input genuinely stops receiving anything until (if ever) reconnected.

use std::time::{Duration, Instant};

use astrs_wire::{
    DataId, DataflowId, DurationMs, InputSpec, NodeEvent, NodeId, NodeSpawnSpec, PortRef,
    RouteCloseReason, StopCause,
};

use crate::error::{DaemonError, DaemonResult};
use crate::server::core::Daemon;
use crate::shm::OutputKey;
use crate::spawn::ProcessHandle;
use crate::supervise::EscalationStep;

impl Daemon {
    /// Adds a fully-specified node to an already-running dataflow, wiring
    /// its declared inputs as routes and, if `start`, spawning it.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownDataflow`] if `spec.dataflow` is not one this
    /// daemon hosts.
    pub fn apply_add_node(&mut self, spec: NodeSpawnSpec, start: bool) -> DaemonResult<()> {
        let dataflow = spec.dataflow;
        let node = spec.node.clone();
        let Some(state) = self.state_mut().dataflow_mut(dataflow) else {
            return Err(DaemonError::UnknownDataflow { dataflow });
        };
        state.add_node(spec);
        if start {
            self.spawn_node(dataflow, &node);
        }
        Ok(())
    }

    /// Removes a node from a running dataflow: asks it to stop, exactly as
    /// [`Daemon::stop_node`] does for any other stop.
    ///
    /// Deliberately does not also scrub [`crate::state::RouteTable`] up
    /// front: the consumers of whatever this node produced see
    /// [`astrs_wire::NodeEvent::InputClosed`] only once it actually exits
    /// — via `Daemon::after_exit`'s existing `close_outputs_of`, which
    /// reads the very entries an eager scrub would have already erased.
    /// A node that stops for good today (a crash with no restart left in
    /// its budget, an ordinary `StopNode`) already leaves its own
    /// consumer-side route entries in the table forever, on exactly the
    /// same reasoning `close_outputs_of` itself documents — this does not
    /// make a removed node's bookkeeping behave any differently from
    /// that existing, already-accepted shape.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownDataflow`]/[`DaemonError::UnknownNode`] if
    /// either is not one this daemon hosts.
    pub fn apply_remove_node(&mut self, dataflow: DataflowId, node: &NodeId) -> DaemonResult<()> {
        {
            let Some(state) = self.state().dataflow(dataflow) else {
                return Err(DaemonError::UnknownDataflow { dataflow });
            };
            if state.node(node).is_none() {
                return Err(DaemonError::UnknownNode {
                    dataflow,
                    node: node.clone(),
                });
            }
        }
        self.stop_node(dataflow, node, StopCause::Requested);
        Ok(())
    }

    /// Swaps a live node's declared shape for `new_spec`, as a new
    /// generation that starts before the outgoing incarnation is asked to
    /// stop — see this module's top-level docs for the whole sequence and
    /// why it is safe.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownDataflow`]/[`DaemonError::UnknownNode`] if
    /// either is not one this daemon hosts.
    pub fn apply_replace_node(&mut self, new_spec: NodeSpawnSpec) -> DaemonResult<()> {
        let dataflow = new_spec.dataflow;
        let node = new_spec.node.clone();

        let (old_handle, old_session, produced) = {
            let Some(state) = self.state().dataflow(dataflow) else {
                return Err(DaemonError::UnknownDataflow { dataflow });
            };
            let Some(node_state) = state.node(&node) else {
                return Err(DaemonError::UnknownNode { dataflow, node });
            };
            let produced: Vec<PortRef> = state
                .routes()
                .produced_by(&node)
                .into_iter()
                .cloned()
                .collect();
            (node_state.handle().cloned(), node_state.session(), produced)
        };

        // Step 1: off the ring, segment released, before anything else.
        for source in &produced {
            let key = OutputKey::from_port(dataflow, source);
            if let Some(action) = self.shm.release(&key) {
                self.apply_upgrade_actions(vec![action]);
            }
        }

        // Step 2: remember the outgoing incarnation, if it has a real
        // process to eventually `SIGTERM`/`SIGKILL` — a node that had
        // neither registered nor ever been spawned, or a `path: dynamic`
        // node (§8.3), which nobody spawned and so nobody can OS-signal
        // either, has nothing for the ladder in
        // `Daemon::fire_replace_supersessions` to do.
        if let Some(handle) = old_handle {
            let grace = self.config.finish_grace();
            self.superseded.insert(
                (dataflow, node.clone()),
                Superseded::new(Some(handle), grace),
            );
        }
        if let Some(session) = old_session
            && let Some(sink) = self.sinks.get(&session)
        {
            // Deliberately does *not* unbind this session from
            // `crate::state::registry::DaemonState`: `Daemon::apply_publish`
            // looks a publisher up by its own exact session id
            // (`DaemonState::session`, an unambiguous map lookup), and the
            // outgoing incarnation needs that to keep working for exactly
            // as long as the dual-run window lasts — unbinding here would
            // silently drop every message it still sends after this. The
            // *other* half of the problem this window creates — a
            // node-id lookup like `Daemon::send_to`'s finding this now-old
            // binding instead of the new one — is fixed at the read side,
            // in `DaemonState::session_of`, which prefers the binding
            // whose generation matches `crate::state::NodeState`'s
            // current one; see that method's own docs.
            let _ = sink.try_send(NodeEvent::Stop {
                cause: StopCause::Replaced,
                grace: Some(DurationMs::from_duration(self.config.finish_grace())),
            });
        }

        // Step 3: swap the spec, bump the generation, spawn the new one.
        {
            let Some(state) = self.state_mut().dataflow_mut(dataflow) else {
                return Err(DaemonError::UnknownDataflow { dataflow });
            };
            let Some(node_state) = state.node_mut(&node) else {
                return Err(DaemonError::UnknownNode { dataflow, node });
            };
            // `begin_next_generation` runs *before* `replace_spec`,
            // against the outgoing incarnation's own current generation
            // — never against whatever `new_spec.generation` the caller
            // filled in. A caller with no reason to track this daemon's
            // live counter (§17's `astrs node replace`, which knows only
            // the node's declared shape) has no better placeholder than
            // `0`, and computing the bump from that instead of from the
            // real value would collide with a generation already in use
            // the moment this node has been spawned, restarted or
            // replaced even once — `0 + 1 = 1` is indistinguishable from
            // a first spawn's own generation 1 (blueprint §6.2's staleness
            // guarantee depends on generations never repeating for one
            // node). `replace_spec` then swaps in the new declared shape
            // (necessarily overwriting `generation` too, being a wholesale
            // assignment — see that method's own docs), and
            // `set_generation` reinstates the value just computed, exactly
            // the "`begin_next_generation` first... `set_generation`
            // after" sequence that method's own docs describe for a
            // coordinator-supplied value — here the just-computed one.
            let next = node_state.begin_next_generation();
            node_state.replace_spec(new_spec);
            node_state.set_generation(next);
        }
        self.spawn_node(dataflow, &node);
        Ok(())
    }

    /// Adds — or, for an already-wired input, rewires — one input edge on
    /// a live node.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownDataflow`]/[`DaemonError::UnknownNode`] if
    /// either is not one this daemon hosts.
    pub fn apply_add_edge(
        &mut self,
        dataflow: DataflowId,
        consumer: NodeId,
        input: InputSpec,
    ) -> DaemonResult<()> {
        {
            let Some(state) = self.state().dataflow(dataflow) else {
                return Err(DaemonError::UnknownDataflow { dataflow });
            };
            if state.node(&consumer).is_none() {
                return Err(DaemonError::UnknownNode {
                    dataflow,
                    node: consumer,
                });
            }
        }
        let source = input.source.clone();
        let Some(state) = self.state_mut().dataflow_mut(dataflow) else {
            return Err(DaemonError::UnknownDataflow { dataflow });
        };
        // `RouteTable` is keyed by *producer* port, for the producer→
        // consumers lookup the local fan-out needs — so unlike
        // `astrs_graph::DataflowGraph::edges` (keyed by the edge itself,
        // where overwriting a key's value is all a rewire needs),
        // `RouteTable::insert`'s own "replace" only reaches an existing
        // entry under the *same* producer key. Rewiring this input to a
        // different producer needs its old entry, if any, removed from
        // wherever it actually lives first — a no-op for a genuinely new
        // input, which has no old entry to remove.
        state.routes_mut().remove_consumer(&consumer, &input.id);
        state.routes_mut().insert(consumer.clone(), input.clone());

        let mailbox = self.mailbox(dataflow, &consumer);
        let _ = mailbox.register(&input);
        // A new (or newly-live) consumer changes who belongs on the
        // producer's ring (§6.3) — same re-evaluation
        // `apply_subscribe`/`plan_shm_output`'s other call sites already
        // run whenever the answer can change.
        self.plan_shm_output(dataflow, &source);
        Ok(())
    }

    /// Removes one input edge from a live node, telling it the input is
    /// closed.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownDataflow`]/[`DaemonError::UnknownNode`] if
    /// either is not one this daemon hosts.
    pub fn apply_remove_edge(
        &mut self,
        dataflow: DataflowId,
        consumer: &NodeId,
        input: &DataId,
    ) -> DaemonResult<()> {
        let source = {
            let Some(state) = self.state().dataflow(dataflow) else {
                return Err(DaemonError::UnknownDataflow { dataflow });
            };
            if state.node(consumer).is_none() {
                return Err(DaemonError::UnknownNode {
                    dataflow,
                    node: consumer.clone(),
                });
            }
            state
                .routes()
                .all()
                .find(|(_, c)| &c.node == consumer && c.input() == input)
                .map(|(source, _)| source.clone())
        };

        if let Some(state) = self.state_mut().dataflow_mut(dataflow) {
            state.routes_mut().remove_consumer(consumer, input);
            if let Some(node_state) = state.node_mut(consumer) {
                node_state.close_input(input);
            }
        }
        if let Some(source) = source {
            self.mailbox(dataflow, consumer).push(
                input,
                NodeEvent::InputClosed {
                    id: input.clone(),
                    source,
                    reason: RouteCloseReason::Disconnected,
                },
            );
            self.push_to_node(dataflow, consumer);
        }
        self.propagate_input_closure(dataflow);
        Ok(())
    }

    /// Escalates every superseded incarnation whose grace has run out:
    /// `SIGTERM`, then (after another grace period) `SIGKILL` — the same
    /// two-rung ladder [`crate::supervise::FinishWatchdogSet`] runs for an
    /// ordinary stop, kept independent of it because
    /// [`crate::supervise::FinishWatchdogSet`] is keyed by node id alone
    /// and the *new* incarnation may need its own ladder armed for a
    /// reason of its own at the same time.
    ///
    /// A superseded `path: dynamic` node (§8.3) has no
    /// [`crate::spawn::ProcessHandle`] to signal — nobody spawned it — so
    /// its ladder runs the same two steps with neither actually signalling
    /// anything, purely so the entry (and the `Daemon::take_superseded_session`
    /// guard it backs) does not linger forever if that node never
    /// disconnects on its own in response to the cooperative
    /// [`astrs_wire::NodeEvent::Stop`] [`Daemon::apply_replace_node`]
    /// already sent it.
    ///
    /// Called from [`Daemon::tick`] every loop iteration, exactly like
    /// [`Daemon::fire_watchdogs`].
    pub(crate) fn fire_replace_supersessions(&mut self, now: Instant) {
        let due: Vec<(DataflowId, NodeId)> = self
            .superseded
            .iter()
            .filter(|(_, entry)| entry.next_signal_at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            let Some(entry) = self.superseded.get_mut(&key) else {
                continue;
            };
            match entry.step {
                EscalationStep::Terminate => {
                    if let Some(handle) = &entry.handle {
                        handle.terminate();
                    }
                    entry.step = EscalationStep::Kill;
                    entry.next_signal_at = now + entry.kill_grace;
                }
                EscalationStep::Kill => {
                    if let Some(handle) = &entry.handle {
                        handle.kill();
                    }
                    self.superseded.remove(&key);
                }
            }
        }
    }

    /// Accelerates (never removes) any superseded incarnation's kill
    /// ladder for `(dataflow, node)` — the shared hook
    /// [`Daemon::apply_register`] calls on a successful cutover and
    /// [`Daemon::after_exit`] calls when the *new* incarnation itself
    /// fails (see this module's top-level docs on why a failed replace
    /// does not resurrect the old one).
    ///
    /// Deliberately does **not** remove the [`Daemon::superseded`] entry:
    /// [`Daemon::take_superseded_session`] must keep answering `true` for
    /// that outgoing session until it actually closes, or
    /// [`Daemon::handle_session_closed`]'s guard would stop applying the
    /// moment this fires — before the outgoing session has necessarily
    /// closed at all — and reclaim the *new* incarnation's state on the
    /// old session's eventual, now-unguarded close. Removal happens in
    /// exactly two places: that guard (the session closed, so there is
    /// nothing left to signal) and [`Daemon::fire_replace_supersessions`]'s
    /// `SIGKILL` rung (it did not close on its own in time).
    ///
    /// A no-op when nothing is superseded for the key, which is the common
    /// case for every node that has never been replaced.
    pub(crate) fn resolve_superseded(&mut self, dataflow: DataflowId, node: &NodeId) {
        let Some(entry) = self.superseded.get_mut(&(dataflow, node.clone())) else {
            return;
        };
        // Already asked to leave (`apply_replace_node` sent the
        // cooperative `Stop` when it armed this entry); this only moves
        // the escalation up to "now" rather than waiting out whatever was
        // left of the original grace period unnecessarily — the outcome
        // (cutover succeeded, or the replace failed outright) is already
        // known by the time either caller reaches this.
        entry.next_signal_at = Instant::now();
    }
}

/// One incarnation [`Daemon::apply_replace_node`] superseded, kept alive
/// only long enough to be told (then, if needed, made) to stop.
///
/// See `crate::server::core::Daemon::superseded`'s own docs for why this
/// exists outside [`crate::state::NodeState`] at all.
#[derive(Debug, Clone)]
pub struct Superseded {
    /// The outgoing incarnation's signalling handle — `None` for a `path:
    /// dynamic` node (§8.3), which nobody spawned and so nobody can
    /// `SIGTERM`/`SIGKILL` either.
    ///
    /// Deliberately carries no session: unlike this field, the outgoing
    /// session is *not* unbound from
    /// [`crate::state::registry::DaemonState`] when this value is
    /// constructed — see this module's top-level docs, step 5, for why it
    /// deliberately stays bound for as long as the dual-run window lasts.
    /// The cooperative [`astrs_wire::NodeEvent::Stop`] this incarnation
    /// gets is sent exactly once, synchronously, at the same
    /// [`Daemon::apply_replace_node`] call site that constructs this value
    /// — using its own local lookup of the session — so there is no later
    /// need for this type to address that session again, and storing a
    /// second copy of it here would only invite one. The session's actual
    /// unbinding happens later and elsewhere, once it actually closes, via
    /// the ordinary `Daemon::handle_session_closed` path.
    handle: Option<ProcessHandle>,
    /// When the next escalation step fires.
    next_signal_at: Instant,
    /// Which step is armed.
    step: EscalationStep,
    /// The grace between `SIGTERM` and `SIGKILL` (the same value both
    /// rungs use — one node-specific grace, not two).
    kill_grace: Duration,
}

impl Superseded {
    /// Arms the ladder's first rung (`SIGTERM`) `grace` from now.
    #[must_use]
    fn new(handle: Option<ProcessHandle>, grace: Duration) -> Self {
        Self {
            handle,
            next_signal_at: Instant::now() + grace,
            step: EscalationStep::Terminate,
            kill_grace: grace,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use astrs_wire::{NodeSource, OutputSpec};

    use super::*;
    use crate::config::{DaemonConfig, ListenConfig, RuntimePaths};

    fn config(tag: &str) -> DaemonConfig {
        let root = std::env::temp_dir().join(format!(
            "astrs-daemon-topology-{}-{tag}",
            std::process::id()
        ));
        DaemonConfig::new(RuntimePaths::under(root)).with_listen(ListenConfig::none())
    }

    fn spec(dataflow: DataflowId, id: &str) -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            dataflow,
            NodeId::new(id).unwrap(),
            0,
            NodeSource::Executable {
                path: format!("./{id}"),
            },
        )
    }

    fn admitted(tag: &str) -> (Daemon, DataflowId) {
        let mut daemon = Daemon::new(config(tag)).unwrap();
        let dataflow = DataflowId::from_u128(1);
        daemon
            .state_mut()
            .insert_dataflow(crate::state::DataflowState::new(
                dataflow,
                astrs_time::HlcTimestamp::EPOCH,
            ));
        (daemon, dataflow)
    }

    #[test]
    fn add_node_wires_its_inputs_as_routes_without_starting_it() {
        let (mut daemon, dataflow) = admitted("add");
        let mut node = spec(dataflow, "extra");
        node.inputs.push(InputSpec::new(
            DataId::new("in").unwrap(),
            PortRef::from_parts("producer", "out").unwrap(),
        ));
        daemon.apply_add_node(node, false).unwrap();

        let state = daemon.dataflow(dataflow).unwrap();
        assert!(state.node(&NodeId::new("extra").unwrap()).is_some());
        assert_eq!(
            state
                .routes()
                .consumers(&PortRef::from_parts("producer", "out").unwrap())
                .len(),
            1
        );
    }

    #[test]
    fn add_node_on_an_unknown_dataflow_is_reported() {
        let mut daemon = Daemon::new(config("add-unknown")).unwrap();
        let err = daemon
            .apply_add_node(spec(DataflowId::from_u128(9), "extra"), false)
            .unwrap_err();
        assert!(matches!(err, DaemonError::UnknownDataflow { .. }));
    }

    #[test]
    fn remove_node_on_an_unknown_node_is_reported() {
        let (mut daemon, dataflow) = admitted("remove-unknown");
        let err = daemon
            .apply_remove_node(dataflow, &NodeId::new("ghost").unwrap())
            .unwrap_err();
        assert!(matches!(err, DaemonError::UnknownNode { .. }));
    }

    #[test]
    fn remove_node_asks_it_to_stop_without_eagerly_scrubbing_its_routes() {
        // A route entry survives until the node actually exits and
        // `Daemon::after_exit`'s `close_outputs_of` runs — never-spawned
        // here, so that never happens in this test, and the routes must
        // still be exactly as they were. See `apply_remove_node`'s own
        // docs for why an eager scrub is the wrong fix.
        let (mut daemon, dataflow) = admitted("remove-routes");
        let mut producer = spec(dataflow, "producer");
        producer
            .outputs
            .push(OutputSpec::new(DataId::new("out").unwrap()));
        daemon.apply_add_node(producer, false).unwrap();
        let mut consumer = spec(dataflow, "consumer");
        consumer.inputs.push(InputSpec::new(
            DataId::new("in").unwrap(),
            PortRef::from_parts("producer", "out").unwrap(),
        ));
        daemon.apply_add_node(consumer, false).unwrap();
        assert_eq!(
            daemon
                .dataflow(dataflow)
                .unwrap()
                .routes()
                .consumers(&PortRef::from_parts("producer", "out").unwrap())
                .len(),
            1
        );

        let id = NodeId::new("consumer").unwrap();
        daemon.apply_remove_node(dataflow, &id).unwrap();
        assert_eq!(
            daemon
                .dataflow(dataflow)
                .unwrap()
                .routes()
                .consumers(&PortRef::from_parts("producer", "out").unwrap())
                .len(),
            1,
            "nothing has exited yet, so the route entry is untouched"
        );
        assert_eq!(
            daemon
                .dataflow(dataflow)
                .unwrap()
                .node(&id)
                .unwrap()
                .run_state(),
            astrs_wire::NodeRunState::Stopping,
            "the node was asked to stop"
        );
    }

    #[test]
    fn add_edge_wires_a_new_route_and_registers_the_mailbox() {
        let (mut daemon, dataflow) = admitted("add-edge");
        daemon
            .apply_add_node(spec(dataflow, "consumer"), false)
            .unwrap();

        daemon
            .apply_add_edge(
                dataflow,
                NodeId::new("consumer").unwrap(),
                InputSpec::new(
                    DataId::new("frames").unwrap(),
                    PortRef::from_parts("camera", "image").unwrap(),
                ),
            )
            .unwrap();

        let state = daemon.dataflow(dataflow).unwrap();
        assert_eq!(
            state
                .routes()
                .consumers(&PortRef::from_parts("camera", "image").unwrap())
                .len(),
            1
        );
    }

    #[test]
    fn add_edge_replaces_rather_than_duplicates_an_existing_input() {
        let (mut daemon, dataflow) = admitted("add-edge-rw");
        daemon
            .apply_add_node(spec(dataflow, "consumer"), false)
            .unwrap();
        let consumer = NodeId::new("consumer").unwrap();
        daemon
            .apply_add_edge(
                dataflow,
                consumer.clone(),
                InputSpec::new(
                    DataId::new("frames").unwrap(),
                    PortRef::from_parts("camera", "image").unwrap(),
                ),
            )
            .unwrap();
        daemon
            .apply_add_edge(
                dataflow,
                consumer,
                InputSpec::new(
                    DataId::new("frames").unwrap(),
                    PortRef::from_parts("camera2", "image").unwrap(),
                ),
            )
            .unwrap();

        let state = daemon.dataflow(dataflow).unwrap();
        assert!(
            state
                .routes()
                .consumers(&PortRef::from_parts("camera", "image").unwrap())
                .is_empty(),
            "the old source no longer feeds this input"
        );
        assert_eq!(
            state
                .routes()
                .consumers(&PortRef::from_parts("camera2", "image").unwrap())
                .len(),
            1
        );
    }

    #[test]
    fn remove_edge_closes_only_the_named_input() {
        let (mut daemon, dataflow) = admitted("remove-edge");
        let mut consumer = spec(dataflow, "consumer");
        consumer.inputs.push(InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        ));
        consumer.inputs.push(InputSpec::new(
            DataId::new("tick").unwrap(),
            PortRef::from_parts("astrs", "timer.hz.1").unwrap(),
        ));
        daemon.apply_add_node(consumer, false).unwrap();
        let id = NodeId::new("consumer").unwrap();

        daemon
            .apply_remove_edge(dataflow, &id, &DataId::new("frames").unwrap())
            .unwrap();

        let state = daemon.dataflow(dataflow).unwrap();
        assert!(
            state
                .routes()
                .consumers(&PortRef::from_parts("camera", "image").unwrap())
                .is_empty()
        );
        assert!(
            !state
                .routes()
                .consumers(&PortRef::from_parts("astrs", "timer.hz.1").unwrap())
                .is_empty(),
            "the other input is untouched"
        );
        assert!(
            state
                .node(&id)
                .unwrap()
                .is_input_closed(&DataId::new("frames").unwrap())
        );
        let queued = daemon.mailbox(dataflow, &id).snapshot_all();
        assert!(
            queued
                .iter()
                .any(|(input, snapshot)| input.as_str() == "frames" && snapshot.depth > 0),
            "the consumer was told its input closed"
        );
    }

    #[test]
    fn replace_node_on_a_never_spawned_node_just_swaps_the_spec() {
        let (mut daemon, dataflow) = admitted("replace-fresh");
        daemon.apply_add_node(spec(dataflow, "a"), false).unwrap();

        let mut replacement = spec(dataflow, "a");
        replacement
            .outputs
            .push(OutputSpec::new(DataId::new("out").unwrap()));
        daemon.apply_replace_node(replacement).unwrap();

        let state = daemon.dataflow(dataflow).unwrap();
        let node = state.node(&NodeId::new("a").unwrap()).unwrap();
        assert_eq!(node.spec().outputs.len(), 1);
        assert_eq!(
            node.generation(),
            1,
            "a fresh generation, even with nothing to supersede"
        );
        assert!(
            daemon.superseded.is_empty(),
            "nothing was running to supersede"
        );
    }

    #[test]
    fn a_second_replace_never_collides_with_the_generation_the_first_one_minted() {
        // `spec(dataflow, id)` always names generation 0 — the same
        // placeholder a caller with no reliable way to know this daemon's
        // live counter (§17's `astrs node replace`) would send. The bump
        // must be computed from *this node's own* current generation, not
        // from that placeholder, or a second replace immediately after a
        // first would collide with the generation the first one just
        // minted (both would land on `next_generation(0) == 1`).
        let (mut daemon, dataflow) = admitted("replace-twice");
        daemon.apply_add_node(spec(dataflow, "a"), false).unwrap();

        daemon.apply_replace_node(spec(dataflow, "a")).unwrap();
        let first_generation = daemon
            .dataflow(dataflow)
            .unwrap()
            .node(&NodeId::new("a").unwrap())
            .unwrap()
            .generation();
        assert_eq!(first_generation, 1);

        daemon.apply_replace_node(spec(dataflow, "a")).unwrap();
        let second_generation = daemon
            .dataflow(dataflow)
            .unwrap()
            .node(&NodeId::new("a").unwrap())
            .unwrap()
            .generation();
        assert_eq!(
            second_generation, 2,
            "the second replace's bump must be computed from the first \
             replace's real generation (1), not from the placeholder (0) \
             the caller sent — otherwise both would mint 1"
        );
        assert_ne!(
            first_generation, second_generation,
            "two distinct incarnations must never share a generation number"
        );
    }

    #[test]
    fn replace_node_on_an_unknown_node_is_reported() {
        let (mut daemon, dataflow) = admitted("replace-unk");
        let err = daemon
            .apply_replace_node(spec(dataflow, "ghost"))
            .unwrap_err();
        assert!(matches!(err, DaemonError::UnknownNode { .. }));
    }

    #[test]
    fn a_superseded_incarnation_escalates_term_then_kill() {
        let (mut daemon, dataflow) = admitted("super-ladder");
        let node = NodeId::new("a").unwrap();
        let handle = ProcessHandle::new(dataflow, node.clone(), 0, u32::MAX);
        let grace = Duration::from_millis(5);
        daemon.superseded.insert(
            (dataflow, node.clone()),
            Superseded::new(Some(handle), grace),
        );

        let now = Instant::now();
        daemon.fire_replace_supersessions(now);
        assert!(
            daemon.superseded.contains_key(&(dataflow, node.clone())),
            "the grace has not elapsed yet"
        );

        daemon.fire_replace_supersessions(now + grace + Duration::from_millis(1));
        assert!(
            daemon.superseded.contains_key(&(dataflow, node.clone())),
            "terminate fired; the kill rung is now armed"
        );

        daemon.fire_replace_supersessions(now + grace * 3);
        assert!(
            !daemon.superseded.contains_key(&(dataflow, node)),
            "kill fired; the ladder ends"
        );
    }

    #[test]
    fn resolving_a_superseded_incarnation_that_was_never_armed_is_a_no_op() {
        let (mut daemon, dataflow) = admitted("resolve-nop");
        daemon.resolve_superseded(dataflow, &NodeId::new("nobody").unwrap());
        assert!(daemon.superseded.is_empty());
    }

    #[test]
    fn replacing_a_never_registered_node_supersedes_nothing() {
        // Neither a handle (nobody spawned a `path: dynamic` node) nor a
        // session (it never registered) to track: `apply_replace_node`
        // must not manufacture a `Superseded` entry with nothing in it.
        let (mut daemon, dataflow) = admitted("replace-none");
        daemon
            .apply_add_node(
                NodeSpawnSpec::new(dataflow, NodeId::new("a").unwrap(), 0, NodeSource::Dynamic),
                false,
            )
            .unwrap();

        daemon
            .apply_replace_node(NodeSpawnSpec::new(
                dataflow,
                NodeId::new("a").unwrap(),
                0,
                NodeSource::Dynamic,
            ))
            .unwrap();
        assert!(daemon.superseded.is_empty());
    }
}
