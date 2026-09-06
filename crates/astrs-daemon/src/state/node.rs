//! [`NodeState`] — everything the daemon knows about one node.
//!
//! One record per manifest node, not per incarnation: a restart mutates this
//! record (new generation, new handle, `restart_count` up) rather than
//! replacing it, because the *edges* of the graph outlive any one process and
//! the subscriptions built from them must not be rebuilt from scratch every
//! time a node bounces.
//!
//! ```text
//!   Pending ──spawn──► Spawning ──Register──► Running
//!      ▲                   │                     │
//!      │                   │ spawn deadline      │ exit
//!      │                   ▼                     ▼
//!      └──backoff──── Restarting ◄──restart── (decision)
//!                                                │
//!                          Stopping ◄─Stop───────┤
//!                             │                  │
//!                             ▼                  ▼
//!                          Finished / Failed  (terminal)
//! ```
//!
//! The state machine is deliberately permissive about *ordering* — a node may
//! exit before it registers, register twice, or close outputs it never opened —
//! because a daemon that assumes a well-behaved node is a daemon the
//! conformance zoo (§20.3) breaks in the first minute. What it is strict about
//! is *generations*: every incarnation-scoped fact ([`NodeState::handle`], the
//! session, the finish watchdog) is checked against
//! [`NodeState::generation`] before it is acted on.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::state::NodeState;
//! use astrs_wire::{DataflowId, NodeExitCause, NodeId, NodeRunState, NodeSource, NodeSpawnSpec};
//!
//! let spec = NodeSpawnSpec::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     0,
//!     NodeSource::Executable { path: "./camera".into() },
//! );
//! let mut state = NodeState::new(spec);
//! assert_eq!(state.run_state(), NodeRunState::Pending);
//!
//! state.mark_spawning(4_242);
//! assert_eq!(state.run_state(), NodeRunState::Spawning);
//! assert_eq!(state.pid(), Some(4_242));
//!
//! state.mark_registered();
//! assert_eq!(state.run_state(), NodeRunState::Running);
//!
//! state.mark_exited(NodeExitCause::Success);
//! assert_eq!(state.run_state(), NodeRunState::Finished);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeSet;
use std::time::Duration;

use astrs_time::HlcTimestamp;
use astrs_wire::{
    DataId, DataflowId, NodeExitCause, NodeId, NodeInfo, NodeRunState, NodeSpawnSpec, SessionId,
    StopCause, TypeUrn,
};

use crate::spawn::{ProcessHandle, next_generation};
use crate::supervise::{ExitIntent, RestartHistory};

/// One node's daemon-side record.
#[derive(Debug)]
pub struct NodeState {
    /// The current incarnation's specification.
    spec: NodeSpawnSpec,
    /// Where the node is in its lifecycle.
    run_state: NodeRunState,
    /// The live signalling handle, when a process is running.
    handle: Option<ProcessHandle>,
    /// The session the node's connection belongs to, once it registers.
    session: Option<SessionId>,
    /// When the current incarnation started.
    started_at: Option<HlcTimestamp>,
    /// How the last incarnation ended.
    exit_cause: Option<NodeExitCause>,
    /// The restart budget window.
    history: RestartHistory,
    /// The inputs the node has subscribed to.
    subscribed: BTreeSet<DataId>,
    /// The outputs the node still publishes on.
    open_outputs: BTreeSet<DataId>,
    /// The inputs that have been reported closed.
    closed_inputs: BTreeSet<DataId>,
    /// Why the node was asked to stop, if it was.
    stop_cause: Option<StopCause>,
    /// What the daemon was expecting when the process next exits.
    intent: ExitIntent,
    /// Whether the node has a *live* registration right now.
    ///
    /// Cleared by [`NodeState::mark_exited`]: the session is gone, so nothing
    /// may be pushed down it. This is emphatically **not** the readiness
    /// barrier's question — see [`NodeState::registered_this_incarnation`].
    registered: bool,
    /// Whether the node registered at least once in this incarnation,
    /// latching past its exit.
    ///
    /// The `AllNodesReady` barrier (§7.3, §24.1) asks a *historical* question
    /// — "did every node this daemon was told to spawn come up?" — and
    /// [`crate::Daemon::report_lifecycle`] re-derives it from this table on a
    /// periodic pass. Reading [`NodeState::registered`] for that made the
    /// answer depend on when the pass happened to run: a dataflow whose nodes
    /// register and exit *between* two passes was never once observed with
    /// every node registered, so `AllNodesReady` was never reported, the
    /// coordinator's `PendingSpawn` never resolved, and a non-detached
    /// `Start` (`astrs start --attach`) hung until its caller timed out while
    /// the dataflow itself ran to completion. This flag is cleared only when a
    /// new incarnation begins, so the barrier's answer no longer depends on
    /// the sampling instant.
    registered_this_incarnation: bool,
    /// Whether this incarnation's declared outputs have been opened yet.
    ///
    /// A spawned node opens them at [`NodeState::mark_spawning`]; a
    /// `path: dynamic` node, which nobody spawns, opens them when it
    /// registers. The flag is what keeps the second path from *re*-opening an
    /// output the node has already closed.
    outputs_opened: bool,
    /// Whether this incarnation has already been told
    /// [`astrs_wire::NodeEvent::AllInputsClosed`].
    ///
    /// Input closure is discovered from several independent places (a
    /// producer's explicit `OutputDone`, a producer process exiting, a peer
    /// link dropping, a shared-memory plane retiring a ring), and every one
    /// of them re-runs the "is anybody finished now?" sweep. Without this
    /// latch that sweep re-announces the same conclusion once per trigger,
    /// so a node whose single producer both closes its output *and* exits
    /// is told twice — see [`NodeState::claim_inputs_closed_notice`].
    inputs_closed_notified: bool,
}

impl NodeState {
    /// A record for a node that has not been spawned yet.
    #[must_use]
    pub fn new(spec: NodeSpawnSpec) -> Self {
        let window = spec.restart.restart_window.to_duration();
        Self {
            spec,
            run_state: NodeRunState::Pending,
            handle: None,
            session: None,
            started_at: None,
            exit_cause: None,
            history: RestartHistory::new(window),
            subscribed: BTreeSet::new(),
            open_outputs: BTreeSet::new(),
            closed_inputs: BTreeSet::new(),
            stop_cause: None,
            intent: ExitIntent::Running,
            registered: false,
            registered_this_incarnation: false,
            outputs_opened: false,
            inputs_closed_notified: false,
        }
    }

    /// The node's identifier.
    #[must_use]
    pub const fn id(&self) -> &NodeId {
        &self.spec.node
    }

    /// The dataflow it belongs to.
    #[must_use]
    pub const fn dataflow(&self) -> DataflowId {
        self.spec.dataflow
    }

    /// The current incarnation's specification.
    #[must_use]
    pub const fn spec(&self) -> &NodeSpawnSpec {
        &self.spec
    }

    /// The current incarnation counter.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.spec.generation
    }

    /// Where the node is in its lifecycle.
    #[must_use]
    pub const fn run_state(&self) -> NodeRunState {
        self.run_state
    }

    /// The live signalling handle, if a process is running.
    #[must_use]
    pub const fn handle(&self) -> Option<&ProcessHandle> {
        self.handle.as_ref()
    }

    /// The current process id, if there is one.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.handle.as_ref().map(ProcessHandle::pid)
    }

    /// The session the node is connected on, if it has registered.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        self.session
    }

    /// When the current incarnation started.
    #[must_use]
    pub const fn started_at(&self) -> Option<HlcTimestamp> {
        self.started_at
    }

    /// How the last incarnation ended.
    #[must_use]
    pub const fn exit_cause(&self) -> Option<&NodeExitCause> {
        self.exit_cause.as_ref()
    }

    /// The restart budget window, mutably — [`crate::supervise::decide`] needs
    /// it that way.
    pub const fn history_mut(&mut self) -> &mut RestartHistory {
        &mut self.history
    }

    /// The restart budget window.
    #[must_use]
    pub const fn history(&self) -> &RestartHistory {
        &self.history
    }

    /// How many times the node has been restarted.
    #[must_use]
    pub const fn restart_count(&self) -> u32 {
        self.history.total()
    }

    /// The inputs the node has subscribed to.
    #[must_use]
    pub const fn subscribed(&self) -> &BTreeSet<DataId> {
        &self.subscribed
    }

    /// Whether the node is subscribed to `input`.
    #[must_use]
    pub fn is_subscribed(&self, input: &DataId) -> bool {
        self.subscribed.contains(input)
    }

    /// The outputs the node still publishes on.
    #[must_use]
    pub const fn open_outputs(&self) -> &BTreeSet<DataId> {
        &self.open_outputs
    }

    /// Whether every declared output has been closed.
    #[must_use]
    pub fn outputs_done(&self) -> bool {
        self.open_outputs.is_empty()
    }

    /// Whether every declared input has been reported closed.
    ///
    /// A node with no inputs at all never reaches this: a source node has no
    /// inputs to close, and telling it `AllInputsClosed` at startup would stop
    /// a camera before it took a frame.
    #[must_use]
    pub fn all_inputs_closed(&self) -> bool {
        !self.spec.inputs.is_empty()
            && self
                .spec
                .inputs
                .iter()
                .all(|input| self.closed_inputs.contains(&input.id))
    }

    /// Whether `input` has been reported closed.
    #[must_use]
    pub fn is_input_closed(&self, input: &DataId) -> bool {
        self.closed_inputs.contains(input)
    }

    /// Claims the single [`astrs_wire::NodeEvent::AllInputsClosed`] notice
    /// this incarnation is owed, returning whether the caller should send it.
    ///
    /// `true` exactly once per incarnation, and only while
    /// [`NodeState::all_inputs_closed`] holds. Every later sweep over the
    /// same still-closed inputs answers `false`, so a node hears
    /// "you are done" once however many independent closure triggers fire —
    /// a producer's `OutputDone` and that same producer's process exit are
    /// two of them for one edge, which is the common `astrs run` case.
    ///
    /// [`NodeState::reopen_input`] clears the latch: an input that comes
    /// back (§12 `InputRecovered`) may legitimately close again later, and
    /// that later closure is a new fact, not a repeat of the old one.
    pub fn claim_inputs_closed_notice(&mut self) -> bool {
        if self.inputs_closed_notified || !self.all_inputs_closed() {
            return false;
        }
        self.inputs_closed_notified = true;
        true
    }

    /// Why the node was asked to stop, if it was.
    #[must_use]
    pub const fn stop_cause(&self) -> Option<&StopCause> {
        self.stop_cause.as_ref()
    }

    /// What the daemon expects the next exit to mean.
    #[must_use]
    pub const fn intent(&self) -> ExitIntent {
        self.intent
    }

    /// Whether the node has a live registration right now.
    #[must_use]
    pub const fn is_registered(&self) -> bool {
        self.registered
    }

    /// Whether the node registered at least once in this incarnation, even if
    /// it has since exited.
    ///
    /// The readiness barrier's question, as opposed to
    /// [`NodeState::is_registered`]'s "is it connected *now*". A node that
    /// ran, produced and exited satisfies the barrier it already passed; only
    /// a new incarnation clears the answer.
    #[must_use]
    pub const fn registered_this_incarnation(&self) -> bool {
        self.registered_this_incarnation
    }

    /// Whether the node is in a state where it can receive events.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(
            self.run_state,
            NodeRunState::Running | NodeRunState::Stopping
        )
    }

    /// Whether the node has finished for good.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.run_state.is_terminal()
    }

    /// Whether the node is a `path: dynamic` attach-in node the daemon never
    /// spawns (§8.3).
    #[must_use]
    pub const fn is_dynamic(&self) -> bool {
        !self.spec.source.is_spawned()
    }

    /// The finish grace period this node's specification asks for.
    #[must_use]
    pub fn finish_grace(&self) -> Option<Duration> {
        self.spec
            .finish_grace
            .map(astrs_wire::DurationMs::to_duration)
    }

    /// The spawn deadline this node's specification asks for.
    #[must_use]
    pub fn spawn_deadline(&self) -> Option<Duration> {
        self.spec
            .spawn_deadline
            .map(astrs_wire::DurationMs::to_duration)
    }

    /// Records that a process was started, as `pid`.
    pub fn mark_spawning(&mut self, pid: u32) {
        self.handle = Some(ProcessHandle::new(
            self.spec.dataflow,
            self.spec.node.clone(),
            self.spec.generation,
            pid,
        ));
        self.run_state = NodeRunState::Spawning;
        self.registered = false;
        self.registered_this_incarnation = false;
        self.intent = ExitIntent::Running;
        self.exit_cause = None;
        self.subscribed.clear();
        self.closed_inputs.clear();
        self.inputs_closed_notified = false;
        self.outputs_opened = false;
        self.open_declared_outputs();
    }

    /// Records that a dynamic node is being waited for rather than spawned.
    pub fn mark_awaiting_attach(&mut self) {
        self.run_state = NodeRunState::Spawning;
        self.registered = false;
        self.registered_this_incarnation = false;
        self.intent = ExitIntent::Running;
    }

    /// Records the node's registration on `session`, at `at`.
    ///
    /// A `path: dynamic` node reaches `Running` here without ever passing
    /// through [`NodeState::mark_spawning`] — nobody spawned it — so its
    /// declared outputs are opened here too. Without that, its first
    /// `OutputDone` would close nothing and its consumers would never be told
    /// their input had ended.
    pub fn mark_registered_on(&mut self, session: SessionId, at: HlcTimestamp) {
        self.session = Some(session);
        self.started_at = Some(at);
        self.mark_registered();
    }

    /// Records the node's registration without a session — a test path.
    pub fn mark_registered(&mut self) {
        self.registered = true;
        self.registered_this_incarnation = true;
        self.open_declared_outputs();
        if !self.is_terminal() {
            self.run_state = NodeRunState::Running;
        }
    }

    /// Opens every declared output, once per incarnation.
    fn open_declared_outputs(&mut self) {
        if self.outputs_opened {
            return;
        }
        self.outputs_opened = true;
        self.open_outputs = self
            .spec
            .outputs
            .iter()
            .map(|output| output.id.clone())
            .collect();
    }

    /// Records the inputs a node subscribed to.
    ///
    /// An empty list means "everything the specification declares", which is
    /// what a node built from its own `NodeConfig` sends.
    pub fn subscribe(&mut self, inputs: &[DataId]) {
        if inputs.is_empty() {
            self.subscribed = self
                .spec
                .inputs
                .iter()
                .map(|input| input.id.clone())
                .collect();
        } else {
            self.subscribed.extend(inputs.iter().cloned());
        }
    }

    /// Records that `output` will produce nothing further.
    ///
    /// Returns whether this closed something that was open.
    pub fn close_output(&mut self, output: &DataId) -> bool {
        self.open_outputs.remove(output)
    }

    /// Records that every output is done.
    pub fn close_all_outputs(&mut self) -> Vec<DataId> {
        let closed: Vec<DataId> = self.open_outputs.iter().cloned().collect();
        self.open_outputs.clear();
        closed
    }

    /// Records that `input` will receive nothing further.
    ///
    /// Returns whether this closed something that was open.
    pub fn close_input(&mut self, input: &DataId) -> bool {
        self.closed_inputs.insert(input.clone())
    }

    /// Records that `input` is live again, because its producer came back
    /// (§12 `InputRecovered`).
    ///
    /// The inverse of [`NodeState::close_input`], and the reason it exists
    /// separately from [`NodeState::subscribe`]: a peer partition closes an
    /// input without the node having unsubscribed, so the repair must reopen
    /// exactly that one input without touching the subscription set.
    ///
    /// Returns whether this reopened something that was closed.
    pub fn reopen_input(&mut self, input: &DataId) -> bool {
        let reopened = self.closed_inputs.remove(input);
        if reopened {
            // The node is no longer finished, so the one notice it is owed
            // is owed again should every input close a second time.
            self.inputs_closed_notified = false;
        }
        reopened
    }

    /// Records that a stop was requested.
    pub fn mark_stopping(&mut self, cause: StopCause) {
        self.stop_cause = Some(cause);
        self.intent = ExitIntent::StopRequested;
        if !self.is_terminal() {
            self.run_state = NodeRunState::Stopping;
        }
    }

    /// Records that the finish watchdog escalated.
    pub const fn mark_escalated(&mut self, intent: ExitIntent) {
        self.intent = intent;
    }

    /// Records that the daemon itself is going away.
    pub fn mark_daemon_shutdown(&mut self) {
        self.intent = ExitIntent::DaemonShutdown;
        if !self.is_terminal() {
            self.run_state = NodeRunState::Stopping;
        }
    }

    /// Records that the process exited with `cause`.
    pub fn mark_exited(&mut self, cause: NodeExitCause) {
        if let Some(handle) = &self.handle {
            handle.mark_reaped();
        }
        self.handle = None;
        self.session = None;
        // `registered_this_incarnation` deliberately survives: the readiness
        // barrier records that this node *did* come up, and an exit does not
        // un-happen a registration. Only `begin_next_generation` clears it.
        self.registered = false;
        self.run_state = if cause.is_failure() {
            NodeRunState::Failed
        } else {
            NodeRunState::Finished
        };
        self.exit_cause = Some(cause);
    }

    /// Records that the node is waiting out a restart backoff.
    pub fn mark_restarting(&mut self) {
        self.run_state = NodeRunState::Restarting;
    }

    /// Advances to the next incarnation, returning the new generation.
    ///
    /// Everything incarnation-scoped is reset here, and *only* here: the
    /// handle, the session, the subscriptions, the open outputs. The restart
    /// history deliberately survives — it is what makes the next backoff
    /// longer than the last.
    pub fn begin_next_generation(&mut self) -> u64 {
        self.spec.generation = next_generation(self.spec.generation);
        self.handle = None;
        self.session = None;
        self.registered = false;
        self.registered_this_incarnation = false;
        self.stop_cause = None;
        self.intent = ExitIntent::Running;
        self.subscribed.clear();
        self.closed_inputs.clear();
        self.inputs_closed_notified = false;
        self.open_outputs.clear();
        self.outputs_opened = false;
        self.run_state = NodeRunState::Pending;
        self.spec.generation
    }

    /// Adopts an incarnation counter chosen elsewhere.
    ///
    /// The coordinator owns the generation counter *across* the cluster (§7.3
    /// `CoordinatorEvent::RestartNode{generation}`): a restart it ordered must
    /// land on its number, or that restart and one a local restart policy
    /// ordered could mint the same generation for two different incarnations —
    /// and the generation stamp is exactly what makes a stale shared-memory
    /// mapping or a stale peer message detectable (§6.2, §12).
    ///
    /// Only ever moves *forward*: a number at or below the current one is
    /// ignored, because a counter that went backwards would make a live
    /// incarnation's own messages look stale to itself. Returns the generation
    /// in force afterwards.
    ///
    /// Call [`NodeState::begin_next_generation`] first — this sets the
    /// counter, it does not reset the incarnation-scoped state around it.
    pub fn set_generation(&mut self, generation: u64) -> u64 {
        if generation > self.spec.generation {
            self.spec.generation = generation;
        }
        self.spec.generation
    }

    /// Replaces the specification — a dynamic node whose ports the daemon only
    /// learns at registration, or a `ReplaceNode` topology mutation (§17).
    pub fn replace_spec(&mut self, spec: NodeSpawnSpec) {
        self.history
            .set_window(spec.restart.restart_window.to_duration());
        self.spec = spec;
    }

    /// The `NodeInfo` this record reports to the coordinator and the CLI.
    #[must_use]
    pub fn info(&self, daemon: astrs_wire::DaemonId) -> NodeInfo {
        NodeInfo {
            dataflow: self.spec.dataflow,
            node: self.spec.node.clone(),
            daemon,
            state: self.run_state,
            pid: self.pid(),
            generation: self.spec.generation,
            restart_count: self.restart_count(),
            inputs: self
                .spec
                .inputs
                .iter()
                .map(|input| (input.id.clone(), input.type_urn.clone()))
                .collect::<std::collections::BTreeMap<DataId, Option<TypeUrn>>>(),
            outputs: self
                .spec
                .outputs
                .iter()
                .map(|output| (output.id.clone(), output.type_urn.clone()))
                .collect(),
            started_at: self.started_at,
            exit_cause: self.exit_cause.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{DaemonId, InputSpec, NodeSource, OutputSpec, PortRef};

    use super::*;

    fn spec() -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("detect").unwrap(),
            0,
            NodeSource::Executable {
                path: "./detect".into(),
            },
        )
        .with_input(InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        ))
        .with_input(InputSpec::new(
            DataId::new("tick").unwrap(),
            PortRef::from_parts("astrs", "timer").unwrap(),
        ))
        .with_output(OutputSpec::new(DataId::new("detections").unwrap()))
    }

    fn state() -> NodeState {
        NodeState::new(spec())
    }

    fn data(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    #[test]
    fn a_fresh_record_is_pending_and_knows_nothing() {
        let state = state();
        assert_eq!(state.run_state(), NodeRunState::Pending);
        assert!(state.handle().is_none());
        assert!(state.pid().is_none());
        assert!(state.session().is_none());
        assert!(state.exit_cause().is_none());
        assert_eq!(state.generation(), 0);
        assert_eq!(state.restart_count(), 0);
        assert!(!state.is_registered());
        assert!(!state.is_live());
        assert!(!state.is_terminal());
        assert!(!state.is_dynamic());
    }

    #[test]
    fn spawning_arms_the_handle_and_opens_the_outputs() {
        let mut state = state();
        state.mark_spawning(4_242);
        assert_eq!(state.run_state(), NodeRunState::Spawning);
        assert_eq!(state.pid(), Some(4_242));
        assert_eq!(
            state.handle().map(ProcessHandle::generation),
            Some(state.generation())
        );
        assert_eq!(state.open_outputs().len(), 1);
        assert!(!state.outputs_done());
    }

    #[test]
    fn registering_moves_to_running_and_records_the_session() {
        let mut state = state();
        state.mark_spawning(1);
        let session = SessionId::from_u128(9);
        state.mark_registered_on(session, HlcTimestamp::new(5, 0));
        assert_eq!(state.run_state(), NodeRunState::Running);
        assert_eq!(state.session(), Some(session));
        assert_eq!(state.started_at(), Some(HlcTimestamp::new(5, 0)));
        assert!(state.is_registered());
        assert!(state.is_live());
    }

    #[test]
    fn an_empty_subscribe_means_every_declared_input() {
        let mut state = state();
        state.subscribe(&[]);
        assert_eq!(state.subscribed().len(), 2);
        assert!(state.is_subscribed(&data("frames")));
        assert!(state.is_subscribed(&data("tick")));
    }

    #[test]
    fn an_explicit_subscribe_takes_only_what_was_named() {
        let mut state = state();
        state.subscribe(&[data("frames")]);
        assert_eq!(state.subscribed().len(), 1);
        assert!(!state.is_subscribed(&data("tick")));
    }

    #[test]
    fn closing_outputs_is_idempotent_and_reported() {
        let mut state = state();
        state.mark_spawning(1);
        assert!(state.close_output(&data("detections")));
        assert!(!state.close_output(&data("detections")));
        assert!(state.outputs_done());
    }

    #[test]
    fn closing_every_output_at_once_reports_what_it_closed() {
        let mut state = state();
        state.mark_spawning(1);
        assert_eq!(state.close_all_outputs(), [data("detections")]);
        assert!(state.close_all_outputs().is_empty());
    }

    #[test]
    fn all_inputs_closed_needs_every_declared_input() {
        let mut state = state();
        assert!(!state.all_inputs_closed());
        assert!(state.close_input(&data("frames")));
        assert!(!state.all_inputs_closed());
        assert!(state.close_input(&data("tick")));
        assert!(state.all_inputs_closed());
        assert!(state.is_input_closed(&data("tick")));
    }

    #[test]
    fn the_all_inputs_closed_notice_is_claimable_exactly_once() {
        let mut state = state();
        assert!(!state.claim_inputs_closed_notice(), "nothing closed yet");
        state.close_input(&data("frames"));
        assert!(!state.claim_inputs_closed_notice(), "only half closed");
        state.close_input(&data("tick"));
        assert!(state.claim_inputs_closed_notice(), "the one notice");
        assert!(
            !state.claim_inputs_closed_notice(),
            "a second closure sweep must stay silent"
        );
    }

    #[test]
    fn a_recovered_input_makes_the_notice_claimable_again() {
        let mut state = state();
        state.close_input(&data("frames"));
        state.close_input(&data("tick"));
        assert!(state.claim_inputs_closed_notice());

        assert!(state.reopen_input(&data("frames")));
        assert!(!state.claim_inputs_closed_notice(), "no longer finished");
        state.close_input(&data("frames"));
        assert!(
            state.claim_inputs_closed_notice(),
            "closing again is a new fact, not a repeat"
        );
    }

    #[test]
    fn a_restart_owes_the_notice_afresh() {
        let mut state = state();
        state.close_input(&data("frames"));
        state.close_input(&data("tick"));
        assert!(state.claim_inputs_closed_notice());

        state.begin_next_generation();
        state.mark_spawning(7);
        state.close_input(&data("frames"));
        state.close_input(&data("tick"));
        assert!(
            state.claim_inputs_closed_notice(),
            "a fresh incarnation is owed its own notice"
        );
    }

    #[test]
    fn a_recovered_input_reopens_exactly_one_input() {
        let mut state = state();
        state.close_input(&data("frames"));
        state.close_input(&data("tick"));
        assert!(state.all_inputs_closed());

        assert!(state.reopen_input(&data("frames")));
        assert!(!state.reopen_input(&data("frames")), "already open");
        assert!(!state.is_input_closed(&data("frames")));
        assert!(
            state.is_input_closed(&data("tick")),
            "the other input is untouched"
        );
        assert!(!state.all_inputs_closed());
    }

    #[test]
    fn a_source_node_never_reports_all_inputs_closed() {
        let mut spec = spec();
        spec.inputs.clear();
        let state = NodeState::new(spec);
        assert!(
            !state.all_inputs_closed(),
            "a camera has no inputs and must not be stopped for it"
        );
    }

    #[test]
    fn stopping_records_the_cause_and_the_intent() {
        let mut state = state();
        state.mark_spawning(1);
        state.mark_registered();
        state.mark_stopping(StopCause::Requested);
        assert_eq!(state.run_state(), NodeRunState::Stopping);
        assert_eq!(state.stop_cause(), Some(&StopCause::Requested));
        assert_eq!(state.intent(), ExitIntent::StopRequested);
        assert!(state.is_live(), "a stopping node still receives events");
    }

    #[test]
    fn a_clean_exit_is_finished_and_a_failure_is_failed() {
        let mut clean = state();
        clean.mark_spawning(1);
        clean.mark_exited(NodeExitCause::Success);
        assert_eq!(clean.run_state(), NodeRunState::Finished);
        assert!(clean.is_terminal());
        assert!(clean.handle().is_none(), "the handle is released");

        let mut failed = state();
        failed.mark_spawning(1);
        failed.mark_exited(NodeExitCause::ExitCode { code: 3 });
        assert_eq!(failed.run_state(), NodeRunState::Failed);
        assert_eq!(
            failed.exit_cause(),
            Some(&NodeExitCause::ExitCode { code: 3 })
        );
    }

    #[test]
    fn a_cancelled_exit_is_finished_not_failed() {
        let mut state = state();
        state.mark_spawning(1);
        state.mark_stopping(StopCause::Requested);
        state.mark_exited(NodeExitCause::Cancelled);
        assert_eq!(state.run_state(), NodeRunState::Finished);
    }

    #[test]
    fn exiting_marks_the_handle_reaped_so_stale_signals_stop() {
        let mut state = state();
        state.mark_spawning(1);
        let handle = state.handle().cloned().unwrap();
        state.mark_exited(NodeExitCause::Success);
        assert!(handle.is_reaped());
    }

    #[test]
    fn a_new_generation_resets_incarnation_state_but_not_history() {
        let mut state = state();
        state.mark_spawning(1);
        state.subscribe(&[]);
        state.close_input(&data("frames"));
        state.mark_stopping(StopCause::Requested);
        state.history_mut().record(std::time::Instant::now());
        state.mark_exited(NodeExitCause::ExitCode { code: 1 });

        let generation = state.begin_next_generation();
        assert_eq!(generation, 1);
        assert_eq!(state.generation(), 1);
        assert_eq!(state.run_state(), NodeRunState::Pending);
        assert!(state.subscribed().is_empty());
        assert!(!state.is_input_closed(&data("frames")));
        assert!(state.stop_cause().is_none());
        assert_eq!(state.intent(), ExitIntent::Running);
        assert_eq!(state.restart_count(), 1, "the backoff exponent survives");
    }

    #[test]
    fn restarting_is_its_own_state() {
        let mut state = state();
        state.mark_restarting();
        assert_eq!(state.run_state(), NodeRunState::Restarting);
        assert!(!state.is_terminal());
    }

    #[test]
    fn registering_opens_the_declared_outputs_for_a_node_nobody_spawned() {
        let mut spec = spec();
        spec.source = NodeSource::Dynamic;
        let mut state = NodeState::new(spec);
        assert!(state.outputs_done(), "nothing is open before registration");

        state.mark_registered_on(SessionId::from_u128(1), HlcTimestamp::new(1, 0));
        assert!(
            !state.outputs_done(),
            "a dynamic node's outputs open when it registers"
        );
        assert!(state.close_output(&data("detections")));
        assert!(state.outputs_done());
    }

    #[test]
    fn registering_does_not_reopen_an_output_a_spawned_node_already_closed() {
        let mut state = state();
        state.mark_spawning(1);
        assert!(state.close_output(&data("detections")));
        state.mark_registered();
        assert!(
            state.outputs_done(),
            "a closed output stays closed across a late registration"
        );
    }

    #[test]
    fn a_dynamic_node_is_recognized_and_awaits_attachment() {
        let mut spec = spec();
        spec.source = NodeSource::Dynamic;
        let mut state = NodeState::new(spec);
        assert!(state.is_dynamic());
        state.mark_awaiting_attach();
        assert_eq!(state.run_state(), NodeRunState::Spawning);
        assert!(state.pid().is_none(), "nobody spawned it");
    }

    #[test]
    fn a_daemon_shutdown_moves_a_live_node_to_stopping() {
        let mut state = state();
        state.mark_spawning(1);
        state.mark_registered();
        state.mark_daemon_shutdown();
        assert_eq!(state.intent(), ExitIntent::DaemonShutdown);
        assert_eq!(state.run_state(), NodeRunState::Stopping);
    }

    #[test]
    fn a_shutdown_does_not_resurrect_a_terminal_node() {
        let mut state = state();
        state.mark_exited(NodeExitCause::Success);
        state.mark_daemon_shutdown();
        assert_eq!(state.run_state(), NodeRunState::Finished);
    }

    #[test]
    fn escalation_changes_only_the_intent() {
        let mut state = state();
        state.mark_spawning(1);
        state.mark_stopping(StopCause::Requested);
        state.mark_escalated(ExitIntent::KilledByWatchdog);
        assert_eq!(state.intent(), ExitIntent::KilledByWatchdog);
        assert_eq!(state.run_state(), NodeRunState::Stopping);
    }

    #[test]
    fn grace_and_deadline_come_from_the_specification() {
        let mut spec = spec();
        spec.finish_grace = Some(astrs_wire::DurationMs::from_secs(3));
        spec.spawn_deadline = Some(astrs_wire::DurationMs::from_secs(7));
        let state = NodeState::new(spec);
        assert_eq!(state.finish_grace(), Some(Duration::from_secs(3)));
        assert_eq!(state.spawn_deadline(), Some(Duration::from_secs(7)));

        let bare = self::state();
        assert!(bare.finish_grace().is_none());
        assert!(bare.spawn_deadline().is_none());
    }

    #[test]
    fn replacing_the_specification_retunes_the_restart_window() {
        let mut state = state();
        let mut spec = spec();
        spec.restart.restart_window = astrs_wire::DurationMs::from_secs(5);
        state.replace_spec(spec);
        assert_eq!(state.history().window(), Duration::from_secs(5));
    }

    #[test]
    fn the_reported_info_matches_the_record() {
        let mut state = state();
        state.mark_spawning(77);
        state.mark_registered_on(SessionId::from_u128(2), HlcTimestamp::new(1, 0));
        let daemon = DaemonId::generate(None);
        let info = state.info(daemon.clone());

        assert_eq!(info.node, *state.id());
        assert_eq!(info.daemon, daemon);
        assert_eq!(info.pid, Some(77));
        assert_eq!(info.state, NodeRunState::Running);
        assert_eq!(info.generation, 0);
        assert_eq!(info.inputs.len(), 2);
        assert_eq!(info.outputs.len(), 1);
        assert_eq!(info.started_at, Some(HlcTimestamp::new(1, 0)));
    }
}
