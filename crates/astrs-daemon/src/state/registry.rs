//! [`DaemonState`] — every dataflow this daemon hosts.
//!
//! Two indexes over the same facts, kept together because they must not drift:
//!
//! - dataflow id → [`crate::state::DataflowState`], the ordinary lookup;
//! - session id → [`SessionBinding`], so a dropped connection finds its node
//!   without scanning every dataflow.
//!
//! The second index is what makes crash handling O(1) at exactly the moment a
//! daemon has least time to spare: a node segfaults, its socket closes, and
//! the daemon must reclaim its extension entries, close its outputs, notify
//! its graph peers and consult its restart policy — all keyed off a session id
//! that arrives with nothing else attached.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::state::{DataflowState, DaemonState};
//! use astrs_time::HlcTimestamp;
//! use astrs_wire::{DataflowId, NodeId, SessionId};
//!
//! let dataflow = DataflowId::from_u128(1);
//! let mut state = DaemonState::new();
//! state.insert_dataflow(DataflowState::new(dataflow, HlcTimestamp::new(1, 0)));
//!
//! let session = SessionId::from_u128(9);
//! state.bind_session(session, dataflow, NodeId::new("camera")?);
//!
//! let binding = state.session(session).expect("bound");
//! assert_eq!(binding.dataflow, dataflow);
//! assert_eq!(binding.node.as_str(), "camera");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;

use astrs_wire::{DataflowId, NodeId, SessionId};

use crate::state::dataflow::DataflowState;

/// What a connected session belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBinding {
    /// The dataflow the node belongs to.
    pub dataflow: DataflowId,
    /// The node on the other end.
    pub node: NodeId,
    /// The incarnation that registered.
    pub generation: u64,
}

/// Every dataflow this daemon hosts.
#[derive(Debug, Default)]
pub struct DaemonState {
    /// The dataflows, in id order.
    dataflows: BTreeMap<DataflowId, DataflowState>,
    /// The session index.
    sessions: BTreeMap<SessionId, SessionBinding>,
    /// Whether the daemon is winding down.
    shutting_down: bool,
}

impl DaemonState {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dataflows: BTreeMap::new(),
            sessions: BTreeMap::new(),
            shutting_down: false,
        }
    }

    /// Adds (or replaces) a dataflow.
    pub fn insert_dataflow(&mut self, dataflow: DataflowState) {
        self.dataflows.insert(dataflow.id(), dataflow);
    }

    /// The record for `id`.
    #[must_use]
    pub fn dataflow(&self, id: DataflowId) -> Option<&DataflowState> {
        self.dataflows.get(&id)
    }

    /// The record for `id`, mutably.
    pub fn dataflow_mut(&mut self, id: DataflowId) -> Option<&mut DataflowState> {
        self.dataflows.get_mut(&id)
    }

    /// Every dataflow.
    pub fn dataflows(&self) -> impl Iterator<Item = &DataflowState> {
        self.dataflows.values()
    }

    /// Every dataflow, mutably.
    pub fn dataflows_mut(&mut self) -> impl Iterator<Item = &mut DataflowState> {
        self.dataflows.values_mut()
    }

    /// The dataflow ids, in order.
    pub fn dataflow_ids(&self) -> impl Iterator<Item = DataflowId> {
        self.dataflows.keys().copied()
    }

    /// How many dataflows the daemon hosts.
    #[must_use]
    pub fn dataflow_count(&self) -> usize {
        self.dataflows.len()
    }

    /// Whether the daemon hosts nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dataflows.is_empty()
    }

    /// Removes a dataflow, along with every session bound to it.
    pub fn remove_dataflow(&mut self, id: DataflowId) -> Option<DataflowState> {
        self.sessions.retain(|_, binding| binding.dataflow != id);
        self.dataflows.remove(&id)
    }

    /// Binds a session to a node.
    pub fn bind_session(&mut self, session: SessionId, dataflow: DataflowId, node: NodeId) {
        let generation = self
            .dataflows
            .get(&dataflow)
            .and_then(|state| state.node(&node))
            .map_or(0, crate::state::node::NodeState::generation);
        self.bind_session_at(session, dataflow, node, generation);
    }

    /// Binds a session to a specific incarnation.
    pub fn bind_session_at(
        &mut self,
        session: SessionId,
        dataflow: DataflowId,
        node: NodeId,
        generation: u64,
    ) {
        if let Some(state) = self.dataflows.get_mut(&dataflow) {
            state.bind_session(session, node.clone());
        }
        self.sessions.insert(
            session,
            SessionBinding {
                dataflow,
                node,
                generation,
            },
        );
    }

    /// What a session belongs to.
    #[must_use]
    pub fn session(&self, session: SessionId) -> Option<&SessionBinding> {
        self.sessions.get(&session)
    }

    /// Unbinds a session, returning what it belonged to.
    pub fn unbind_session(&mut self, session: SessionId) -> Option<SessionBinding> {
        let binding = self.sessions.remove(&session)?;
        if let Some(state) = self.dataflows.get_mut(&binding.dataflow) {
            state.unbind_session(session);
        }
        Some(binding)
    }

    /// How many sessions are bound.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// The session bound to a node, if any.
    ///
    /// A dynamic-topology `ReplaceNode` (blueprint §8, §17) briefly leaves
    /// *two* sessions bound to the same node id — [`crate::state::NodeState`]
    /// holds exactly one incarnation, but the outgoing one keeps its own
    /// session bound for as long as it may still be publishing (see
    /// [`crate::dataflow::topology`]'s module docs) — so a plain "any
    /// match" scan would answer with whichever session happens to sort
    /// first, which is not necessarily the current one. This prefers the
    /// bound session whose recorded generation
    /// ([`SessionBinding::generation`]) matches
    /// [`crate::state::NodeState::generation`]'s current value, falling
    /// back to any match only when this daemon tracks no
    /// [`crate::state::NodeState`] for the pair at all (a shape this
    /// crate's own low-level state tests build directly).
    #[must_use]
    pub fn session_of(&self, dataflow: DataflowId, node: &NodeId) -> Option<SessionId> {
        let current_generation = self
            .dataflows
            .get(&dataflow)
            .and_then(|state| state.node(node))
            .map(crate::state::node::NodeState::generation);

        let mut fallback = None;
        for (session, binding) in &self.sessions {
            if binding.dataflow != dataflow || binding.node != *node {
                continue;
            }
            if current_generation == Some(binding.generation) {
                return Some(*session);
            }
            fallback.get_or_insert(*session);
        }
        fallback
    }

    /// Whether the daemon is winding down.
    #[must_use]
    pub const fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Marks the daemon as winding down; new work is refused from here on.
    pub const fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    /// Every dataflow that has finished and can be reaped.
    #[must_use]
    pub fn finished_dataflows(&self) -> Vec<DataflowId> {
        self.dataflows
            .iter()
            .filter(|(_, state)| state.status().is_terminal())
            .map(|(id, _)| *id)
            .collect()
    }

    /// How many nodes the daemon hosts across every dataflow.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.dataflows.values().map(DataflowState::node_count).sum()
    }

    /// How many nodes are running across every dataflow.
    #[must_use]
    pub fn running_node_count(&self) -> usize {
        self.dataflows
            .values()
            .map(DataflowState::running_count)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataflowStatus, NodeSource, NodeSpawnSpec};

    use super::*;

    fn dataflow(id: u128) -> DataflowState {
        let dataflow = DataflowId::from_u128(id);
        let mut state = DataflowState::new(dataflow, HlcTimestamp::new(1, 0));
        state.add_node(NodeSpawnSpec::new(
            dataflow,
            NodeId::new("camera").unwrap(),
            0,
            NodeSource::Executable {
                path: "./camera".into(),
            },
        ));
        state
    }

    fn state() -> DaemonState {
        let mut state = DaemonState::new();
        state.insert_dataflow(dataflow(1));
        state.insert_dataflow(dataflow(2));
        state
    }

    #[test]
    fn an_empty_registry_hosts_nothing() {
        let state = DaemonState::new();
        assert!(state.is_empty());
        assert_eq!(state.dataflow_count(), 0);
        assert_eq!(state.node_count(), 0);
        assert_eq!(state.session_count(), 0);
        assert!(!state.is_shutting_down());
    }

    #[test]
    fn dataflows_are_found_by_id() {
        let mut state = state();
        assert_eq!(state.dataflow_count(), 2);
        assert!(state.dataflow(DataflowId::from_u128(1)).is_some());
        assert!(state.dataflow(DataflowId::from_u128(9)).is_none());
        assert!(state.dataflow_mut(DataflowId::from_u128(2)).is_some());
        assert_eq!(state.dataflow_ids().count(), 2);
        assert_eq!(state.node_count(), 2);
    }

    #[test]
    fn a_session_binds_to_a_node_and_its_generation() {
        let mut state = state();
        let dataflow = DataflowId::from_u128(1);
        state
            .dataflow_mut(dataflow)
            .unwrap()
            .node_mut(&NodeId::new("camera").unwrap())
            .unwrap()
            .begin_next_generation();

        let session = SessionId::from_u128(4);
        state.bind_session(session, dataflow, NodeId::new("camera").unwrap());

        let binding = state.session(session).expect("bound");
        assert_eq!(binding.dataflow, dataflow);
        assert_eq!(binding.node.as_str(), "camera");
        assert_eq!(binding.generation, 1, "the current incarnation");
        assert_eq!(
            state.session_of(dataflow, &NodeId::new("camera").unwrap()),
            Some(session)
        );
    }

    #[test]
    fn the_dataflow_index_agrees_with_the_registry_index() {
        let mut state = state();
        let dataflow = DataflowId::from_u128(1);
        let session = SessionId::from_u128(4);
        state.bind_session(session, dataflow, NodeId::new("camera").unwrap());

        assert_eq!(
            state
                .dataflow(dataflow)
                .and_then(|state| state.session_node(session)),
            Some(&NodeId::new("camera").unwrap())
        );

        state.unbind_session(session);
        assert!(state.session(session).is_none());
        assert!(
            state
                .dataflow(dataflow)
                .and_then(|state| state.session_node(session))
                .is_none()
        );
    }

    #[test]
    fn removing_a_dataflow_unbinds_its_sessions() {
        let mut state = state();
        let dataflow = DataflowId::from_u128(1);
        state.bind_session(
            SessionId::from_u128(4),
            dataflow,
            NodeId::new("camera").unwrap(),
        );
        state.bind_session(
            SessionId::from_u128(5),
            DataflowId::from_u128(2),
            NodeId::new("camera").unwrap(),
        );

        let removed = state.remove_dataflow(dataflow).expect("present");
        assert_eq!(removed.id(), dataflow);
        assert_eq!(state.dataflow_count(), 1);
        assert_eq!(state.session_count(), 1, "only the other one is left");
        assert!(state.session(SessionId::from_u128(5)).is_some());
    }

    #[test]
    fn binding_a_session_for_an_unknown_dataflow_still_records_it() {
        let mut state = DaemonState::new();
        let session = SessionId::from_u128(1);
        state.bind_session(session, DataflowId::from_u128(9), NodeId::new("x").unwrap());
        assert_eq!(
            state.session(session).map(|binding| binding.generation),
            Some(0)
        );
    }

    #[test]
    fn an_explicit_generation_is_recorded_verbatim() {
        let mut state = state();
        let session = SessionId::from_u128(7);
        state.bind_session_at(
            session,
            DataflowId::from_u128(1),
            NodeId::new("camera").unwrap(),
            42,
        );
        assert_eq!(
            state.session(session).map(|binding| binding.generation),
            Some(42)
        );
    }

    #[test]
    fn shutdown_is_a_one_way_flag() {
        let mut state = state();
        state.begin_shutdown();
        assert!(state.is_shutting_down());
    }

    #[test]
    fn finished_dataflows_are_listed_for_reaping() {
        let mut state = state();
        assert!(state.finished_dataflows().is_empty());
        state
            .dataflow_mut(DataflowId::from_u128(1))
            .unwrap()
            .set_status(DataflowStatus::Finished);
        assert_eq!(state.finished_dataflows(), [DataflowId::from_u128(1)]);
    }

    #[test]
    fn running_counts_aggregate_across_dataflows() {
        let mut state = state();
        for id in [1u128, 2] {
            state
                .dataflow_mut(DataflowId::from_u128(id))
                .unwrap()
                .node_mut(&NodeId::new("camera").unwrap())
                .unwrap()
                .mark_registered();
        }
        assert_eq!(state.running_node_count(), 2);
        assert_eq!(state.dataflows().count(), 2);
        assert_eq!(state.dataflows_mut().count(), 2);
    }

    #[test]
    fn unbinding_an_unknown_session_is_not_an_error() {
        let mut state = state();
        assert!(state.unbind_session(SessionId::from_u128(99)).is_none());
    }
}
