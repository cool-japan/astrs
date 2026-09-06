//! [`DataflowState`] — one running graph's daemon-side record.
//!
//! Holds the nodes ([`crate::state::NodeState`]), the routes
//! ([`crate::state::RouteTable`]), the extension table
//! ([`crate::extensions::ExtensionTable`]) and the phase the dataflow is in.
//! The FSM that drives it lives in [`crate::dataflow`]; this is the data it
//! drives.
//!
//! ```text
//!   Pending ─build─► Building ─ok─► Ready ─start─► Starting
//!                        │                            │ every node registered
//!                        │ failure                    ▼
//!                        └──────────────────────►  Running
//!                                                     │
//!                            exit_when_nodes_finish   │ stop / destroy
//!                                       ▼             ▼
//!                                    Finished ◄── Stopping ──► Failed
//! ```
//!
//! # The barrier
//!
//! `Starting` becomes `Running` when every spawnable node has registered —
//! the `AllNodesReady` barrier (§24.1). A `path: dynamic` node is *not*
//! waited for: it attaches whenever its operator starts it, and a graph that
//! blocked on one would never start. [`DataflowState::ready_barrier_met`] is
//! that rule, in one place.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::state::DataflowState;
//! use astrs_wire::{DataflowId, DataflowStatus, NodeId, NodeSource, NodeSpawnSpec};
//! use astrs_time::HlcTimestamp;
//!
//! let dataflow = DataflowId::from_u128(1);
//! let mut state = DataflowState::new(dataflow, HlcTimestamp::new(1, 0));
//! state.add_node(NodeSpawnSpec::new(
//!     dataflow,
//!     NodeId::new("camera")?,
//!     0,
//!     NodeSource::Executable { path: "./camera".into() },
//! ));
//!
//! assert_eq!(state.status(), DataflowStatus::Pending);
//! assert!(!state.ready_barrier_met(), "nothing has registered yet");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use astrs_wire::{
    DataflowId, DataflowResult, DataflowStatus, DataflowSummary, NodeExitCause, NodeId, NodeInfo,
    NodeSpawnSpec, SessionId,
};

use crate::extensions::ExtensionTable;
use crate::state::node::NodeState;
use crate::state::routes::RouteTable;

/// One dataflow, as the daemon sees it.
#[derive(Debug)]
pub struct DataflowState {
    /// The dataflow's identifier.
    id: DataflowId,
    /// Its human name, from the manifest.
    name: Option<String>,
    /// Where it is in its lifecycle.
    status: DataflowStatus,
    /// The nodes this daemon hosts, in manifest order by id.
    nodes: BTreeMap<NodeId, NodeState>,
    /// Every edge whose consumer this daemon hosts.
    routes: RouteTable,
    /// The dataflow-scoped extension table (§2.1).
    extensions: ExtensionTable,
    /// The per-node exit causes, accumulating into the final result.
    result: DataflowResult,
    /// Whether the dataflow stops itself when every node finishes (§8.2).
    exit_when_nodes_finish: bool,
    /// Sessions that have registered, so a session drop finds its node in O(1).
    sessions: BTreeMap<SessionId, NodeId>,
}

impl DataflowState {
    /// An empty record for `id`, started at `now`.
    #[must_use]
    pub fn new(id: DataflowId, now: HlcTimestamp) -> Self {
        Self {
            id,
            name: None,
            status: DataflowStatus::Pending,
            nodes: BTreeMap::new(),
            routes: RouteTable::new(),
            extensions: ExtensionTable::new(),
            result: DataflowResult::new(id, now),
            // The manifest's own default (§8.2: long-running unless the
            // graph opts in). `DataflowState` is built before the plan is
            // applied, so starting anywhere else would make the two disagree
            // for the window in between.
            exit_when_nodes_finish: false,
            sessions: BTreeMap::new(),
        }
    }

    /// Names the dataflow.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets whether the dataflow stops itself when every node finishes.
    #[must_use]
    pub const fn with_exit_when_nodes_finish(mut self, exit: bool) -> Self {
        self.exit_when_nodes_finish = exit;
        self
    }

    /// The dataflow's identifier.
    #[must_use]
    pub const fn id(&self) -> DataflowId {
        self.id
    }

    /// Its human name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Where it is in its lifecycle.
    #[must_use]
    pub const fn status(&self) -> DataflowStatus {
        self.status
    }

    /// Moves it to `status`.
    pub const fn set_status(&mut self, status: DataflowStatus) {
        self.status = status;
    }

    /// Whether it stops itself when every node finishes.
    #[must_use]
    pub const fn exit_when_nodes_finish(&self) -> bool {
        self.exit_when_nodes_finish
    }

    /// The routes.
    #[must_use]
    pub const fn routes(&self) -> &RouteTable {
        &self.routes
    }

    /// The routes, mutably.
    pub const fn routes_mut(&mut self) -> &mut RouteTable {
        &mut self.routes
    }

    /// The extension table.
    #[must_use]
    pub const fn extensions(&self) -> &ExtensionTable {
        &self.extensions
    }

    /// The extension table, mutably.
    pub const fn extensions_mut(&mut self) -> &mut ExtensionTable {
        &mut self.extensions
    }

    /// The accumulated result.
    #[must_use]
    pub const fn result(&self) -> &DataflowResult {
        &self.result
    }

    /// Adds a node, replacing any record already under that id.
    pub fn add_node(&mut self, spec: NodeSpawnSpec) {
        let node = spec.node.clone();
        for input in &spec.inputs {
            self.routes.insert(node.clone(), input.clone());
        }
        self.nodes.insert(node, NodeState::new(spec));
    }

    /// The node record for `id`.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&NodeState> {
        self.nodes.get(id)
    }

    /// The node record for `id`, mutably.
    pub fn node_mut(&mut self, id: &NodeId) -> Option<&mut NodeState> {
        self.nodes.get_mut(id)
    }

    /// Every node record.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeState> {
        self.nodes.values()
    }

    /// Every node record, mutably.
    pub fn nodes_mut(&mut self) -> impl Iterator<Item = &mut NodeState> {
        self.nodes.values_mut()
    }

    /// The node ids, in order.
    pub fn node_ids(&self) -> impl Iterator<Item = &NodeId> {
        self.nodes.keys()
    }

    /// How many nodes the dataflow has.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// How many nodes are running.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.nodes.values().filter(|node| node.is_live()).count()
    }

    /// Binds a session to the node that registered on it.
    pub fn bind_session(&mut self, session: SessionId, node: NodeId) {
        self.sessions.insert(session, node);
    }

    /// The node a session belongs to.
    #[must_use]
    pub fn session_node(&self, session: SessionId) -> Option<&NodeId> {
        self.sessions.get(&session)
    }

    /// Unbinds a session, returning the node it belonged to.
    pub fn unbind_session(&mut self, session: SessionId) -> Option<NodeId> {
        self.sessions.remove(&session)
    }

    /// Whether every node the daemon spawns has *settled* — the
    /// `AllNodesReady` barrier (§24.1).
    ///
    /// Dynamic nodes are excluded: nobody spawned them, so nobody can wait for
    /// them. A dataflow made *entirely* of dynamic nodes is ready at once,
    /// which is the right answer — there is nothing to wait for.
    ///
    /// # Why "settled" and not "registered"
    ///
    /// A node satisfies the barrier two ways, and both are answers rather than
    /// waits:
    ///
    /// | Node | Satisfies | Reported in `AllNodesReady{nodes}` |
    /// |---|---|---|
    /// | registered this incarnation (running, or ran and exited) | yes | yes |
    /// | terminal without ever registering (spawn failed, crashed first) | yes | **no** |
    /// | spawning, or waiting out a restart backoff | no | — |
    ///
    /// The first row is what [`NodeState::registered_this_incarnation`]
    /// exists for: the barrier is a historical question, re-derived on a
    /// periodic pass, and a node that came up and exited between two passes
    /// has still come up. Reading the live registration instead made a fast
    /// dataflow's barrier unobservable and hung every non-detached `Start`.
    ///
    /// The second row is what keeps a node that will *never* register from
    /// blocking the barrier forever. It is reported as absent from
    /// `AllNodesReady{nodes}`, which is exactly the gap the coordinator's
    /// `PendingSpawn::record` reads as "this node failed to spawn" — the
    /// wire carries no failure list, so absence is the vocabulary. A node
    /// merely `Restarting` is *not* terminal and does hold the barrier, which
    /// is right: its next incarnation may well register.
    #[must_use]
    pub fn ready_barrier_met(&self) -> bool {
        self.nodes
            .values()
            .filter(|node| !node.is_dynamic())
            .all(|node| node.registered_this_incarnation() || node.is_terminal())
    }

    /// The nodes the barrier is still waiting for.
    ///
    /// The complement of [`DataflowState::ready_barrier_met`]'s predicate, so
    /// "why is this not ready" always names exactly the nodes that answer it.
    #[must_use]
    pub fn pending_registrations(&self) -> Vec<&NodeId> {
        self.nodes
            .values()
            .filter(|node| {
                !node.is_dynamic() && !node.registered_this_incarnation() && !node.is_terminal()
            })
            .map(NodeState::id)
            .collect()
    }

    /// The nodes to name in [`astrs_wire::DaemonEvent::AllNodesReady`] — every
    /// node that did *not* fail to come up.
    ///
    /// The wire carries no failure list, so the coordinator's
    /// `PendingSpawn::record` reads the gap between what it dispatched to this
    /// daemon and what comes back here as "these failed to spawn". The set is
    /// therefore the complement of that claim, and it has two members:
    ///
    /// - nodes that registered in this incarnation, whether or not they have
    ///   since exited;
    /// - `path: dynamic` nodes, always. Nobody spawned one, so a spawn of one
    ///   cannot have failed; the coordinator dispatched it and must not be
    ///   told it failed merely because whoever attaches to it has not turned
    ///   up yet. This mirrors their exclusion from
    ///   [`DataflowState::ready_barrier_met`].
    #[must_use]
    pub fn ready_node_ids(&self) -> Vec<NodeId> {
        self.nodes
            .values()
            .filter(|node| node.registered_this_incarnation() || node.is_dynamic())
            .map(|node| node.id().clone())
            .collect()
    }

    /// Whether every node has reached a terminal state.
    #[must_use]
    pub fn all_nodes_finished(&self) -> bool {
        self.nodes.values().all(NodeState::is_terminal)
    }

    /// Whether any node is still expected to do something.
    #[must_use]
    pub fn any_node_active(&self) -> bool {
        !self.all_nodes_finished()
    }

    /// Records a node's exit cause into the result.
    pub fn record_exit(&mut self, node: &NodeId, cause: NodeExitCause) {
        self.result.record(node.clone(), cause);
    }

    /// Finishes the result at `now`, setting the terminal status.
    ///
    /// [`DataflowResult::finish`] computes the status from the recorded
    /// causes; this mirrors it onto the state so a subsequent `list` shows the
    /// same thing the result does.
    pub fn finish(&mut self, now: HlcTimestamp) -> DataflowResult {
        self.result.finish(now);
        self.status = self.result.status;
        self.result.clone()
    }

    /// The summary reported to `astrs list` (§17).
    #[must_use]
    pub fn summary(&self, daemon: astrs_wire::DaemonId) -> DataflowSummary {
        DataflowSummary {
            id: self.id,
            name: self.name.clone(),
            status: self.status,
            daemons: vec![daemon],
            node_count: u32::try_from(self.nodes.len()).unwrap_or(u32::MAX),
            running_nodes: u32::try_from(self.running_count()).unwrap_or(u32::MAX),
            started_at: Some(self.result.started_at),
        }
    }

    /// The per-node info reported to `astrs info` (§17).
    #[must_use]
    pub fn node_infos(&self, daemon: &astrs_wire::DaemonId) -> Vec<NodeInfo> {
        self.nodes
            .values()
            .map(|node| node.info(daemon.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{DaemonId, DataId, InputSpec, NodeSource, OutputSpec, PortRef};

    use super::*;

    fn spec(name: &str) -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new(name).unwrap(),
            0,
            NodeSource::Executable {
                path: format!("./{name}"),
            },
        )
    }

    fn state() -> DataflowState {
        let mut state = DataflowState::new(DataflowId::from_u128(1), HlcTimestamp::new(1, 0))
            .with_name("pipeline");
        state.add_node(spec("camera").with_output(OutputSpec::new(DataId::new("image").unwrap())));
        state.add_node(spec("detect").with_input(InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        )));
        state
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    #[test]
    fn a_fresh_dataflow_is_pending_and_empty_of_results() {
        let state = state();
        assert_eq!(state.status(), DataflowStatus::Pending);
        assert_eq!(state.id(), DataflowId::from_u128(1));
        assert_eq!(state.name(), Some("pipeline"));
        assert_eq!(state.node_count(), 2);
        assert_eq!(state.running_count(), 0);
        assert!(state.result().node_results.is_empty());
        assert!(
            !state.exit_when_nodes_finish(),
            "long-running unless the manifest opts in (§8.2)"
        );
    }

    #[test]
    fn adding_a_node_registers_its_inputs_as_routes() {
        let state = state();
        assert_eq!(state.routes().len(), 1);
        let consumers = state
            .routes()
            .consumers(&PortRef::from_parts("camera", "image").unwrap());
        assert_eq!(consumers.len(), 1);
        assert_eq!(consumers[0].node, node("detect"));
    }

    #[test]
    fn node_records_are_reachable_and_mutable() {
        let mut state = state();
        assert!(state.node(&node("camera")).is_some());
        assert!(state.node(&node("missing")).is_none());

        state
            .node_mut(&node("camera"))
            .expect("present")
            .mark_spawning(11);
        assert_eq!(
            state.node(&node("camera")).and_then(NodeState::pid),
            Some(11)
        );
        assert_eq!(state.node_ids().count(), 2);
        assert_eq!(state.nodes().count(), 2);
    }

    #[test]
    fn the_ready_barrier_waits_for_every_spawnable_node() {
        let mut state = state();
        assert!(!state.ready_barrier_met());
        assert_eq!(state.pending_registrations().len(), 2);

        state.node_mut(&node("camera")).unwrap().mark_registered();
        assert!(!state.ready_barrier_met());
        assert_eq!(state.pending_registrations(), [&node("detect")]);

        state.node_mut(&node("detect")).unwrap().mark_registered();
        assert!(state.ready_barrier_met());
        assert!(state.pending_registrations().is_empty());
        assert_eq!(state.running_count(), 2);
    }

    #[test]
    fn the_barrier_does_not_wait_for_a_dynamic_node() {
        let mut state = state();
        let mut dynamic = spec("attached");
        dynamic.source = NodeSource::Dynamic;
        state.add_node(dynamic);

        state.node_mut(&node("camera")).unwrap().mark_registered();
        state.node_mut(&node("detect")).unwrap().mark_registered();
        assert!(
            state.ready_barrier_met(),
            "nobody spawned the dynamic node, so nobody waits for it"
        );
        assert_eq!(
            state.ready_node_ids(),
            vec![node("attached"), node("camera"), node("detect")],
            "a dynamic node is reported ready even before it attaches: the \
             coordinator reads an absent id as a *spawn* failure, and nobody \
             spawned this one"
        );
    }

    #[test]
    fn the_barrier_stays_met_after_a_node_that_registered_exits() {
        // The `astrs start --attach` regression. `report_lifecycle` re-derives
        // the barrier on a periodic pass, so a dataflow whose nodes register
        // and exit *between* two passes must still be observed as having met
        // it — otherwise `AllNodesReady` is never reported, the coordinator's
        // `PendingSpawn` never resolves, and a non-detached `Start` hangs
        // while the dataflow it started runs to completion.
        let mut state = state();
        state.node_mut(&node("camera")).unwrap().mark_registered();
        state.node_mut(&node("detect")).unwrap().mark_registered();
        assert!(state.ready_barrier_met());

        state
            .node_mut(&node("camera"))
            .unwrap()
            .mark_exited(NodeExitCause::Success);
        assert!(
            state.ready_barrier_met(),
            "a node that came up and exited has still come up"
        );
        assert!(state.pending_registrations().is_empty());

        state
            .node_mut(&node("detect"))
            .unwrap()
            .mark_exited(NodeExitCause::Success);
        assert!(
            state.ready_barrier_met(),
            "and the whole dataflow finishing does not un-meet the barrier"
        );
        assert_eq!(
            state.ready_node_ids(),
            vec![node("camera"), node("detect")],
            "both are reported ready: both registered"
        );
    }

    #[test]
    fn the_barrier_stops_waiting_for_a_node_that_died_before_registering() {
        // The other way a barrier used to hang forever: a node whose process
        // failed to start never registers, so "every node registered" was
        // never true. It is terminal, which settles it — and it is absent
        // from the ready list, which is the only vocabulary the wire has for
        // "this one failed to spawn".
        let mut state = state();
        state.node_mut(&node("camera")).unwrap().mark_registered();
        assert!(!state.ready_barrier_met());
        assert_eq!(state.pending_registrations(), [&node("detect")]);

        state
            .node_mut(&node("detect"))
            .unwrap()
            .mark_exited(NodeExitCause::SpawnFailed {
                message: "no such file".to_owned(),
            });
        assert!(state.ready_barrier_met());
        assert!(state.pending_registrations().is_empty());
        assert_eq!(
            state.ready_node_ids(),
            vec![node("camera")],
            "the node that never registered is not reported ready"
        );
    }

    #[test]
    fn a_node_waiting_out_a_restart_backoff_still_holds_the_barrier() {
        let mut state = state();
        state.node_mut(&node("camera")).unwrap().mark_registered();
        state.node_mut(&node("detect")).unwrap().mark_restarting();
        assert!(
            !state.ready_barrier_met(),
            "restarting is not terminal: the next incarnation may yet register"
        );
        assert_eq!(state.pending_registrations(), [&node("detect")]);
    }

    #[test]
    fn a_new_incarnation_clears_the_registration_the_barrier_read() {
        let mut state = state();
        state.node_mut(&node("camera")).unwrap().mark_registered();
        state.node_mut(&node("detect")).unwrap().mark_registered();
        state
            .node_mut(&node("camera"))
            .unwrap()
            .mark_exited(NodeExitCause::ExitCode { code: 1 });
        assert!(state.ready_barrier_met());

        state
            .node_mut(&node("camera"))
            .unwrap()
            .begin_next_generation();
        assert!(
            !state.ready_barrier_met(),
            "the restarted incarnation has to register on its own account"
        );
        assert_eq!(state.pending_registrations(), [&node("camera")]);
    }

    #[test]
    fn a_dataflow_of_only_dynamic_nodes_is_ready_at_once() {
        let mut state = DataflowState::new(DataflowId::from_u128(2), HlcTimestamp::new(1, 0));
        let mut dynamic = spec("attached");
        dynamic.source = NodeSource::Dynamic;
        state.add_node(dynamic);
        assert!(state.ready_barrier_met());
    }

    #[test]
    fn sessions_map_back_to_their_nodes() {
        let mut state = state();
        let session = SessionId::from_u128(4);
        state.bind_session(session, node("camera"));
        assert_eq!(state.session_node(session), Some(&node("camera")));
        assert_eq!(state.unbind_session(session), Some(node("camera")));
        assert!(state.session_node(session).is_none());
        assert!(state.unbind_session(session).is_none());
    }

    #[test]
    fn finishing_aggregates_the_causes_and_mirrors_the_status() {
        let mut state = state();
        state.record_exit(&node("camera"), NodeExitCause::Success);
        state.record_exit(&node("detect"), NodeExitCause::ExitCode { code: 2 });
        let result = state.finish(HlcTimestamp::new(9, 0));

        assert_eq!(result.status, DataflowStatus::Failed);
        assert_eq!(state.status(), DataflowStatus::Failed);
        assert_eq!(result.node_results.len(), 2);
        assert_eq!(result.failed_nodes().count(), 1);
        assert_eq!(result.finished_at, Some(HlcTimestamp::new(9, 0)));
    }

    #[test]
    fn a_clean_run_finishes_as_finished() {
        let mut state = state();
        state.record_exit(&node("camera"), NodeExitCause::Success);
        state.record_exit(&node("detect"), NodeExitCause::Success);
        assert_eq!(
            state.finish(HlcTimestamp::new(9, 0)).status,
            DataflowStatus::Finished
        );
    }

    #[test]
    fn all_nodes_finished_needs_every_node_terminal() {
        let mut state = state();
        assert!(!state.all_nodes_finished());
        assert!(state.any_node_active());

        for name in ["camera", "detect"] {
            state
                .node_mut(&node(name))
                .unwrap()
                .mark_exited(NodeExitCause::Success);
        }
        assert!(state.all_nodes_finished());
        assert!(!state.any_node_active());
    }

    #[test]
    fn the_summary_reports_what_list_shows() {
        let mut state = state();
        state.node_mut(&node("camera")).unwrap().mark_registered();
        let daemon = DaemonId::generate(None);
        let summary = state.summary(daemon.clone());

        assert_eq!(summary.id, state.id());
        assert_eq!(summary.name.as_deref(), Some("pipeline"));
        assert_eq!(summary.node_count, 2);
        assert_eq!(summary.running_nodes, 1);
        assert_eq!(summary.daemons, [daemon]);
        assert_eq!(summary.display_name(), "pipeline");
    }

    #[test]
    fn node_infos_cover_every_node() {
        let state = state();
        let daemon = DaemonId::generate(None);
        let infos = state.node_infos(&daemon);
        assert_eq!(infos.len(), 2);
        assert!(infos.iter().all(|info| info.daemon == daemon));
    }

    #[test]
    fn the_extension_table_is_dataflow_scoped() {
        let mut state = state();
        let key = astrs_wire::ExtensionKey::user("pool").unwrap();
        state
            .extensions_mut()
            .store(&node("camera"), key.clone(), vec![1], None)
            .unwrap();
        assert_eq!(state.extensions().len(), 1);
        assert_eq!(state.extensions().peek(&key), Some([1].as_slice()));
    }

    #[test]
    fn exit_when_nodes_finish_is_configurable() {
        let state = DataflowState::new(DataflowId::from_u128(3), HlcTimestamp::new(1, 0))
            .with_exit_when_nodes_finish(true);
        assert!(state.exit_when_nodes_finish());
    }

    #[test]
    fn adding_a_node_twice_replaces_the_record() {
        let mut state = state();
        state.add_node(spec("camera"));
        assert_eq!(state.node_count(), 2);
        assert_eq!(
            state.node(&node("camera")).map(NodeState::run_state),
            Some(astrs_wire::NodeRunState::Pending)
        );
    }
}
