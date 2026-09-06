//! [`ClusterState`] — everything a daemon holds *because* it belongs to a
//! cluster (blueprint §4.2, §12).
//!
//! A daemon under `astrs run` has none of this: no uplink, no peers to dial,
//! no lifecycle facts to report to anybody. So it all lives in one struct
//! behind one field on [`Daemon`], default-constructed and inert until
//! [`Daemon::connect_coordinator`] fills it in — rather than five more fields
//! on the event loop's own struct that four out of five daemons would never
//! read.
//!
//! | Held | Why the loop needs it |
//! |---|---|
//! | [`UplinkHandle`] | the link's status, and the cursors a re-registration carries |
//! | [`PeerDirectory`] | which peers exist, where, and which edges cross to them (§6.4) |
//! | [`LogHistory`] | so `astrs logs` can be answered without a file (§17) |
//! | reported-lifecycle sets | so `AllNodesReady`/`AllNodesFinished` are each sent once |
//!
//! # The one timer
//!
//! [`ClusterState::reconcile_due`] paces the peer-reconciliation backstop
//! described in [`crate::coordinator::routes`]. Everything else in this module
//! is edge-triggered; this is the only thing that runs because time passed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use astrs_wire::{
    DaemonEvent as WireDaemonEvent, DataflowId, NodeExitCause, NodeId, RouteCloseReason,
};

use crate::coordinator::config::UplinkConfig;
use crate::coordinator::link::{UplinkHandle, spawn_uplink};
use crate::coordinator::logs::LogHistory;
use crate::coordinator::routes::{PEER_RECONCILE_INTERVAL, PeerDirectory};
use crate::error::DaemonResult;
use crate::server::core::Daemon;

/// The cluster half of a daemon's state.
#[derive(Debug)]
pub struct ClusterState {
    /// The coordinator link, once one has been asked for.
    uplink: Option<UplinkHandle>,
    /// The peers the coordinator has named (§6.4).
    directory: PeerDirectory,
    /// The recent log records `astrs logs` reads (§17).
    history: LogHistory,
    /// Dataflows whose `AllNodesReady` has already been reported.
    ready_reported: BTreeSet<DataflowId>,
    /// Dataflows whose `AllNodesFinished` has already been reported.
    finished_reported: BTreeSet<DataflowId>,
    /// When the peer-reconciliation backstop next runs.
    reconcile_at: Option<Instant>,
    /// Coordinator-ordered restarts whose outgoing incarnation has been
    /// asked to stop and has not ended yet (§7.3 `RestartNode`).
    pending_restarts: BTreeMap<(DataflowId, NodeId), u64>,
}

impl Default for ClusterState {
    fn default() -> Self {
        Self::new()
    }
}

impl ClusterState {
    /// A daemon that belongs to no cluster yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            uplink: None,
            directory: PeerDirectory::new(),
            history: LogHistory::default(),
            ready_reported: BTreeSet::new(),
            finished_reported: BTreeSet::new(),
            reconcile_at: None,
            pending_restarts: BTreeMap::new(),
        }
    }

    /// Records that `node` must be respawned as `generation` once its
    /// current incarnation ends (§7.3 `CoordinatorEvent::RestartNode`).
    ///
    /// Deferred rather than immediate because a live node has a *process*: a
    /// respawn issued before that process is reaped leaves the finish
    /// watchdog armed against a generation the node no longer has, and the
    /// next escalation then signals the incarnation that just started.
    pub fn arm_restart(&mut self, dataflow: DataflowId, node: NodeId, generation: u64) {
        self.pending_restarts.insert((dataflow, node), generation);
    }

    /// Every armed restart, as `(dataflow, node, generation)`.
    #[must_use]
    pub fn armed_restarts(&self) -> Vec<(DataflowId, NodeId, u64)> {
        self.pending_restarts
            .iter()
            .map(|((dataflow, node), generation)| (*dataflow, node.clone(), *generation))
            .collect()
    }

    /// Forgets one armed restart, returning the generation it carried.
    pub fn disarm_restart(&mut self, dataflow: DataflowId, node: &NodeId) -> Option<u64> {
        self.pending_restarts.remove(&(dataflow, node.clone()))
    }

    /// How many restarts are waiting for their incarnation to end.
    #[must_use]
    pub fn armed_restart_count(&self) -> usize {
        self.pending_restarts.len()
    }

    /// The coordinator link, if one was asked for.
    #[must_use]
    pub const fn uplink(&self) -> Option<&UplinkHandle> {
        self.uplink.as_ref()
    }

    /// Whether a coordinator connection is up right now.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.uplink.as_ref().is_some_and(UplinkHandle::is_connected)
    }

    /// Whether the daemon is running without a coordinator it was told to
    /// have — §12's degraded-autonomous mode.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.uplink.is_some() && !self.is_connected()
    }

    /// The peers the coordinator has named.
    #[must_use]
    pub const fn directory(&self) -> &PeerDirectory {
        &self.directory
    }

    /// The peers the coordinator has named, mutably.
    pub const fn directory_mut(&mut self) -> &mut PeerDirectory {
        &mut self.directory
    }

    /// The recent log records.
    #[must_use]
    pub const fn history(&self) -> &LogHistory {
        &self.history
    }

    /// The recent log records, mutably.
    pub const fn history_mut(&mut self) -> &mut LogHistory {
        &mut self.history
    }

    /// Installs an uplink, replacing (and stopping) any previous one.
    pub fn set_uplink(&mut self, uplink: UplinkHandle) {
        if let Some(previous) = self.uplink.replace(uplink) {
            previous.shutdown();
        }
    }

    /// Takes the uplink out, leaving the daemon coordinator-less.
    #[must_use]
    pub fn take_uplink(&mut self) -> Option<UplinkHandle> {
        self.uplink.take()
    }

    /// Whether the peer-reconciliation backstop is due at `now`, advancing it
    /// when it is.
    pub fn reconcile_due(&mut self, now: Instant) -> bool {
        match self.reconcile_at {
            Some(at) if now < at => false,
            _ => {
                self.reconcile_at = now.checked_add(PEER_RECONCILE_INTERVAL);
                true
            }
        }
    }

    /// Makes the peer-reconciliation backstop due at the next tick.
    ///
    /// Used when something already known has made a previously impossible
    /// route possible — a peer's transient refusal, which the route table has
    /// already forgotten. Waiting out [`PEER_RECONCILE_INTERVAL`] would be
    /// correct but far too slow: several dataflows in this workspace finish in
    /// less time than one sweep.
    pub const fn reconcile_soon(&mut self) {
        self.reconcile_at = None;
    }

    /// Claims the right to report `dataflow`'s `AllNodesReady`, once.
    pub fn claim_ready(&mut self, dataflow: DataflowId) -> bool {
        self.ready_reported.insert(dataflow)
    }

    /// Claims the right to report `dataflow`'s `AllNodesFinished`, once.
    pub fn claim_finished(&mut self, dataflow: DataflowId) -> bool {
        self.finished_reported.insert(dataflow)
    }

    /// Forgets a dataflow's lifecycle claims and cross-daemon edges, so a
    /// restart of the same id is reported again.
    pub fn forget_dataflow(&mut self, dataflow: DataflowId) {
        self.forget_lifecycle(dataflow);
        self.directory.forget_dataflow(dataflow);
    }

    /// Releases a dataflow's lifecycle claims **without** touching its
    /// cross-daemon edges.
    ///
    /// The two are forgotten together when a dataflow *ends*, and separately
    /// when one *starts*: a `PeerRoutes` can arrive before the `Spawn` that
    /// creates the dataflow (see [`crate::coordinator::routes`]), so clearing
    /// the directory on first sight of a dataflow would throw away the very
    /// directives that raced ahead of it.
    pub fn forget_lifecycle(&mut self, dataflow: DataflowId) {
        self.ready_reported.remove(&dataflow);
        self.finished_reported.remove(&dataflow);
        self.pending_restarts
            .retain(|(armed, _), _| *armed != dataflow);
    }
}

impl Daemon {
    /// The cluster half of this daemon's state.
    #[must_use]
    pub const fn cluster(&self) -> &ClusterState {
        &self.cluster
    }

    /// The cluster half of this daemon's state, mutably.
    pub const fn cluster_mut(&mut self) -> &mut ClusterState {
        &mut self.cluster
    }

    /// The peers the coordinator has named (§6.4).
    #[must_use]
    pub const fn peer_directory(&self) -> &PeerDirectory {
        self.cluster.directory()
    }

    /// The peers the coordinator has named, mutably.
    pub const fn peer_directory_mut(&mut self) -> &mut PeerDirectory {
        self.cluster.directory_mut()
    }

    /// The recent log records `astrs logs` reads (§13, §17).
    #[must_use]
    pub const fn log_history(&self) -> &LogHistory {
        self.cluster.history()
    }

    /// Records one log record for the pull path.
    pub fn record_log(&mut self, record: astrs_wire::LogRecord) {
        self.cluster.history_mut().record(record);
    }

    /// Whether a coordinator connection is up right now.
    #[must_use]
    pub fn is_coordinator_connected(&self) -> bool {
        self.cluster.is_connected()
    }

    /// Joins a cluster: binds the peer listener, dials the coordinator, and
    /// installs the uplink's outbox as this daemon's report sink (§4.2, §12).
    ///
    /// Order matters. The peer listener is bound **first** so the registration
    /// can announce the address peers must dial — with a configured port of
    /// `0` (what a test wants) the address does not even exist until the bind
    /// returns, and registering before it would advertise `:0` and make every
    /// peer dial fail.
    ///
    /// Returns immediately after the uplink task is started: the first dial
    /// happens in that task, so a daemon whose coordinator is not up yet still
    /// starts, runs autonomously, and joins when it can (§12).
    ///
    /// # Errors
    ///
    /// [`crate::DaemonError::Transport`] if the peer listener cannot be bound.
    /// A peer configuration that listens nowhere is *not* an error: such a
    /// daemon can still consume from peers that dial it and produce over links
    /// it opens itself, and it registers the coordinator's own address so the
    /// registration is still well formed.
    pub async fn connect_coordinator(&mut self, config: UplinkConfig) -> DaemonResult<()> {
        let config = match self.bind_peer_listener().await? {
            Some(addr) => config.with_peer_address(format!("tcp:{addr}")),
            None => config,
        };
        let uplink = spawn_uplink(self.config().id().clone(), config, self.handle());
        self.set_sink(Arc::clone(uplink.sink()) as Arc<dyn crate::health::ReportSink>);
        self.cluster.set_uplink(uplink);
        Ok(())
    }

    /// Binds the peer listener and starts serving it, if one is configured.
    ///
    /// Returns the address actually bound, which for a configured port of `0`
    /// is the one the operating system chose.
    ///
    /// # Errors
    ///
    /// [`crate::DaemonError::Transport`] if the port cannot be bound.
    pub async fn bind_peer_listener(&mut self) -> DaemonResult<Option<std::net::SocketAddr>> {
        if self.config().peer().listen().is_none() {
            return Ok(None);
        }
        if let Some(addr) = self.peers().listen_addr() {
            return Ok(Some(addr));
        }
        let handle = self.handle();
        let addr = self.peers_mut().bind().await?;
        self.peers().spawn_accept_loop(handle);
        Ok(Some(addr))
    }

    /// The cluster-side maintenance pass: reconcile peers, report lifecycle.
    ///
    /// Called from [`Daemon::tick`], so it runs on the same schedule every
    /// other deadline does and from the one task that owns the state it reads.
    pub(crate) fn poll_cluster(&mut self, now: Instant) {
        self.fire_pending_restarts();
        self.report_lifecycle();
        if self.cluster.reconcile_due(now) {
            self.reconcile_peers();
        }
        if let Some(uplink) = self.cluster.uplink() {
            let running = u32::try_from(self.state().running_node_count()).unwrap_or(u32::MAX);
            uplink.state().set_running_nodes(running);
        }
    }

    /// Respawns every coordinator-ordered restart whose outgoing incarnation
    /// has ended (§7.3 `RestartNode`).
    ///
    /// Driven from the tick rather than from the exit path so it needs no
    /// hook inside [`crate::server::handlers`], and so an incarnation that
    /// came back some *other* way — a restart policy that fired first, an
    /// `AddNode` that replaced it — simply disarms the claim instead of
    /// spawning a second process for the same node.
    fn fire_pending_restarts(&mut self) {
        let armed = self.cluster.armed_restarts();
        if armed.is_empty() {
            return;
        }
        for (dataflow, node, generation) in armed {
            let state = self
                .dataflow(dataflow)
                .and_then(|dataflow| dataflow.node(&node));
            let Some(state) = state else {
                // The dataflow or the node is gone; there is nothing to
                // restart and nothing to wait for.
                self.cluster.disarm_restart(dataflow, &node);
                continue;
            };
            if state.is_terminal() {
                self.cluster.disarm_restart(dataflow, &node);
                self.spawn_restarted_node(dataflow, &node, generation);
            } else if state.generation() >= generation {
                // Somebody else already advanced this node past the
                // generation the coordinator asked for: the restart happened.
                self.cluster.disarm_restart(dataflow, &node);
            }
        }
    }

    /// Reports each dataflow's `AllNodesReady` and `AllNodesFinished` exactly
    /// once (§7.3).
    ///
    /// Derived from the node table on every pass rather than emitted from the
    /// registration and exit paths: those are in
    /// [`crate::server::handlers`], reached by several routes each, and a
    /// missed emission is a dataflow the coordinator never marks running. A
    /// claim set makes the derivation idempotent, which is what lets it be
    /// re-derived as often as it likes.
    fn report_lifecycle(&mut self) {
        // A daemon with no coordinator has nobody to report to — that is
        // `astrs run`, whose caller reads the `DataflowResult` directly. A
        // daemon whose coordinator is merely *down* still reports: the
        // outbox buffers it (§12), and losing a lifecycle fact to a
        // reconnect is exactly what that buffer exists to prevent.
        if self.cluster.uplink().is_none() {
            return;
        }
        // `AllNodesReady{nodes}` carries the nodes that *registered*, not
        // every node this daemon hosts: the wire has no failure list, so the
        // coordinator's `PendingSpawn::record` infers a failed spawn from the
        // gap between what it dispatched here and what comes back. Reporting
        // every id — including one that died before it ever registered —
        // would report a failed start as a clean one.
        let ready: Vec<(DataflowId, Vec<NodeId>)> = self
            .state()
            .dataflows()
            .filter(|state| state.node_count() > 0 && state.ready_barrier_met())
            .map(|state| (state.id(), state.ready_node_ids()))
            .collect();
        for (dataflow, nodes) in ready {
            if self.cluster.claim_ready(dataflow) {
                self.sink()
                    .report(WireDaemonEvent::AllNodesReady { dataflow, nodes });
            }
        }

        let finished: Vec<(DataflowId, BTreeMap<NodeId, NodeExitCause>)> = self
            .state()
            .dataflows()
            .filter(|state| state.node_count() > 0 && state.all_nodes_finished())
            .map(|state| {
                let results = state
                    .nodes()
                    .map(|node| {
                        (
                            node.id().clone(),
                            node.exit_cause().cloned().unwrap_or(NodeExitCause::Success),
                        )
                    })
                    .collect();
                (state.id(), results)
            })
            .collect();
        for (dataflow, results) in finished {
            if self.cluster.claim_finished(dataflow) {
                self.sink()
                    .report(WireDaemonEvent::AllNodesFinished { dataflow, results });
                self.forget_peer_routes(dataflow, RouteCloseReason::DataflowStopped);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use super::*;

    #[test]
    fn a_fresh_cluster_state_belongs_to_no_cluster() {
        let cluster = ClusterState::new();
        assert!(cluster.uplink().is_none());
        assert!(!cluster.is_connected());
        assert!(
            !cluster.is_degraded(),
            "a daemon that was never given a coordinator is not degraded, it is local"
        );
        assert!(cluster.directory().is_empty());
        assert!(cluster.history().is_empty());
    }

    #[test]
    fn a_lifecycle_claim_is_granted_once() {
        let mut cluster = ClusterState::new();
        let dataflow = DataflowId::from_u128(1);
        assert!(cluster.claim_ready(dataflow));
        assert!(!cluster.claim_ready(dataflow));
        assert!(cluster.claim_finished(dataflow));
        assert!(!cluster.claim_finished(dataflow));
    }

    #[test]
    fn forgetting_a_dataflow_releases_its_claims() {
        let mut cluster = ClusterState::new();
        let dataflow = DataflowId::from_u128(1);
        assert!(cluster.claim_ready(dataflow));
        cluster.forget_dataflow(dataflow);
        assert!(
            cluster.claim_ready(dataflow),
            "a dataflow started again is reported again"
        );
    }

    #[test]
    fn the_reconcile_backstop_paces_itself() {
        let mut cluster = ClusterState::new();
        let start = Instant::now();
        assert!(cluster.reconcile_due(start), "the first pass always runs");
        assert!(!cluster.reconcile_due(start));
        assert!(!cluster.reconcile_due(start + PEER_RECONCILE_INTERVAL - Duration::from_millis(1)));
        assert!(cluster.reconcile_due(start + PEER_RECONCILE_INTERVAL));
    }

    #[test]
    fn a_default_state_matches_a_new_one() {
        let cluster = ClusterState::default();
        assert!(cluster.directory().is_empty());
        assert_eq!(
            cluster.history().capacity(),
            crate::coordinator::DEFAULT_LOG_HISTORY
        );
    }
}
