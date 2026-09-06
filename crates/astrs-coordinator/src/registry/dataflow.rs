//! Live dataflow tracking: the expanded manifest, graph and placement a
//! running dataflow was started from, and the in-flight aggregation state
//! for `Build`/`Start` (blueprint §5.2, §7.3).
//!
//! Everything a CLI can *list* or *inspect* about a dataflow
//! (`DataflowMeta`, `NodeStatusRecord`) is durable, in `astrs-store`, and
//! read back on demand — see [`crate::handlers::info`]. What lives here is
//! the part that has no meaning once every connection closes: the parsed
//! [`DataflowGraph`] and [`PlacementPlan`] (rebuilding either from the
//! stored manifest snapshot on every query would be wasteful, not
//! incorrect, but the resolved node→daemon map genuinely has no other
//! home), and the waiters for `WaitForBuild`/`WaitForSpawn` blocked on an
//! outcome that has not arrived yet.

use std::collections::{BTreeMap, BTreeSet};

use astrs_graph::DataflowGraph;
use astrs_manifest::Manifest;
use astrs_wire::{
    BuildId, BuildOutcome, DaemonId, DataflowId, NodeId as WireNodeId, NodeIoSample,
    NodeMetricsSample, NodeSpawnSpec,
};
use tokio::sync::oneshot;

use crate::placement::ResolvedPlacement;

/// The outcome [`PendingBuild`]'s waiters are resolved with.
#[derive(Debug, Clone, Default)]
pub struct BuildOutcomeSummary {
    /// Whether every daemon that was asked to build reported success.
    pub succeeded: bool,
    /// One entry per daemon whose build did not succeed
    /// (`"<daemon>: <outcome>"`), for the aggregate error message.
    pub failures: Vec<String>,
}

/// One in-flight `Build`, aggregating each daemon's
/// [`astrs_wire::DaemonEvent::BuildResult`] until every daemon that was
/// asked has answered.
pub struct PendingBuild {
    /// This build's id.
    pub build: BuildId,
    /// Daemons that have not yet reported a [`BuildOutcome`].
    pub awaiting: BTreeSet<DaemonId>,
    /// Every outcome received so far.
    pub outcomes: Vec<(DaemonId, BuildOutcome)>,
    /// Callers blocked in `WaitForBuild`, resolved (all at once) the moment
    /// [`PendingBuild::awaiting`] empties.
    pub waiters: Vec<oneshot::Sender<BuildOutcomeSummary>>,
}

impl PendingBuild {
    /// A pending build expecting an answer from every daemon in
    /// `daemons`.
    #[must_use]
    pub fn new(build: BuildId, daemons: impl IntoIterator<Item = DaemonId>) -> Self {
        Self {
            build,
            awaiting: daemons.into_iter().collect(),
            outcomes: Vec::new(),
            waiters: Vec::new(),
        }
    }

    /// Records one daemon's outcome.
    ///
    /// Returns the summary if this was the last daemon awaited (the
    /// caller is then responsible for draining and resolving
    /// [`PendingBuild::waiters`], since doing so here would need this
    /// method to consume `self`, which callers checking "is this build
    /// still pending" for `RestartByName` and friends need not to happen).
    pub fn record(
        &mut self,
        daemon: DaemonId,
        outcome: BuildOutcome,
    ) -> Option<BuildOutcomeSummary> {
        if !outcome.is_success() {
            self.outcomes
                .push((daemon.clone(), outcome_for_summary(&outcome)));
        }
        self.awaiting.remove(&daemon);
        if self.awaiting.is_empty() {
            Some(self.summary())
        } else {
            None
        }
    }

    fn summary(&self) -> BuildOutcomeSummary {
        BuildOutcomeSummary {
            succeeded: self.outcomes.is_empty(),
            failures: self
                .outcomes
                .iter()
                .map(|(daemon, outcome)| format!("{daemon}: {outcome}"))
                .collect(),
        }
    }
}

/// Recorded verbatim; kept as its own function so [`PendingBuild::record`]
/// reads as "push the failing outcome," not "push a clone."
fn outcome_for_summary(outcome: &BuildOutcome) -> BuildOutcome {
    outcome.clone()
}

/// The outcome [`PendingSpawn`]'s waiters are resolved with.
#[derive(Debug, Clone, Default)]
pub struct SpawnOutcomeSummary {
    /// Nodes that failed to spawn anywhere.
    pub failed_nodes: Vec<WireNodeId>,
}

impl SpawnOutcomeSummary {
    /// Whether every node came up.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.failed_nodes.is_empty()
    }
}

/// One in-flight `Start`, aggregating each hosting daemon's
/// [`astrs_wire::DaemonEvent::AllNodesReady`] until every daemon has
/// answered.
///
/// [`astrs_wire::DaemonEvent::AllNodesReady`] reports the nodes on that
/// daemon that *did* register (`nodes: Vec<NodeId>`) — there is no
/// "failed" list on the wire for the coordinator to read directly. Working
/// out which of a daemon's dispatched nodes never came up is this type's
/// job: [`PendingSpawn::new`] records what was dispatched to each daemon,
/// and [`PendingSpawn::record`] diffs that against what the daemon
/// actually reported.
pub struct PendingSpawn {
    /// The nodes dispatched to each daemon, as of the `Spawn` messages
    /// this pending spawn was opened for.
    dispatched: BTreeMap<DaemonId, Vec<WireNodeId>>,
    /// Daemons that have not yet reported `AllNodesReady`.
    pub awaiting: BTreeSet<DaemonId>,
    /// Nodes inferred failed (dispatched but never reported ready) by any
    /// daemon so far.
    pub failed_nodes: Vec<WireNodeId>,
    /// Callers blocked in `WaitForSpawn`.
    pub waiters: Vec<oneshot::Sender<SpawnOutcomeSummary>>,
}

impl PendingSpawn {
    /// A pending spawn expecting an answer from every daemon that was
    /// dispatched at least one node.
    #[must_use]
    pub fn new(dispatched: BTreeMap<DaemonId, Vec<WireNodeId>>) -> Self {
        Self {
            awaiting: dispatched.keys().cloned().collect(),
            dispatched,
            failed_nodes: Vec::new(),
            waiters: Vec::new(),
        }
    }

    /// Records one daemon's `AllNodesReady` report: every node dispatched
    /// to `daemon` that is not in `reported_ready` is inferred failed.
    ///
    /// Returns the summary once every awaited daemon has reported.
    pub fn record(
        &mut self,
        daemon: &DaemonId,
        reported_ready: &[WireNodeId],
    ) -> Option<SpawnOutcomeSummary> {
        if let Some(expected) = self.dispatched.get(daemon) {
            for node in expected {
                if !reported_ready.contains(node) {
                    self.failed_nodes.push(node.clone());
                }
            }
        }
        self.awaiting.remove(daemon);
        if self.awaiting.is_empty() {
            Some(SpawnOutcomeSummary {
                failed_nodes: self.failed_nodes.clone(),
            })
        } else {
            None
        }
    }
}

/// A dataflow the coordinator is currently building, starting, running or
/// tearing down.
pub struct LiveDataflow {
    /// This dataflow's id.
    pub id: DataflowId,
    /// The name it was started/built under, if any.
    pub name: Option<String>,
    /// The fully expanded manifest (no unresolved `module:` references).
    pub manifest: Manifest,
    /// The directory the manifest's relative paths resolve against.
    pub working_dir: Option<String>,
    /// The graph model built from `manifest`.
    pub graph: DataflowGraph,
    /// The resolved node→daemon placement (empty until `Start`).
    pub placement: ResolvedPlacement,
    /// The current incarnation counter per node, keyed by the node's wire
    /// id — bumped on every restart (blueprint §3.5/§6.2).
    pub generations: BTreeMap<WireNodeId, u64>,
    /// Nodes added dynamically after `Start` (`AddNode`) that were never
    /// part of the original manifest/graph — tracked separately since
    /// `graph`/`placement` describe only the manifest's own nodes.
    pub dynamic_nodes: BTreeMap<WireNodeId, NodeSpawnSpec>,
    /// Daemons that have reported `AllNodesFinished` for this run.
    /// Finalizing a [`astrs_wire::DataflowResult`] waits for every hosting
    /// daemon to be in this set, mirroring how [`PendingSpawn`] waits for
    /// every hosting daemon's `AllNodesReady`.
    pub finished_daemons: std::collections::BTreeSet<DaemonId>,
    /// Every node's exit cause reported so far this run, accumulated
    /// across every daemon's `AllNodesFinished`.
    pub node_results: BTreeMap<WireNodeId, astrs_wire::NodeExitCause>,
    /// The build this dataflow is currently building under, if any.
    pub pending_build: Option<PendingBuild>,
    /// The spawn this dataflow is currently starting under, if any.
    pub pending_spawn: Option<PendingSpawn>,
    /// The most recently resolved build outcome for [`LiveDataflow::build_id`],
    /// so a `WaitForBuild` that arrives after the build already finished
    /// still gets a real answer instead of racing
    /// [`LiveDataflow::pending_build`] being cleared.
    pub last_build_outcome: Option<BuildOutcomeSummary>,
    /// As [`LiveDataflow::last_build_outcome`], for the dataflow's most
    /// recent `Start`.
    pub last_spawn_outcome: Option<SpawnOutcomeSummary>,
    /// The synthetic recorder node id opened by `RecordStart`, if
    /// recording is active.
    pub recording_node: Option<WireNodeId>,
    /// The build this dataflow was registered under, if it went through
    /// `Build` rather than starting straight from an inline manifest —
    /// kept for the lifetime of the dataflow (not just while the build is
    /// pending) so `Start { source: DataflowSource::Build { build } }`
    /// can resolve `build` back to this dataflow after the build has
    /// long since finished.
    pub build_id: Option<BuildId>,
    /// Each node's latest reported resource/queue sample (blueprint §13),
    /// keyed by node id. The coordinator's own latest-known reading, not a
    /// history — the daemon re-samples every two seconds, so a poll
    /// (`GetNodeMetrics`, `astrs top`) is never staler than that. Belongs
    /// here rather than in the durable store for the same reason
    /// `placement` does (this module's docs): it has no meaning once every
    /// connection closes, and rebuilding it from scratch on reconnect costs
    /// nothing since the daemon simply samples again.
    pub latest_metrics: BTreeMap<WireNodeId, NodeMetricsSample>,
    /// The bandwidth half of the same reading (§6.2, §13), from
    /// [`astrs_wire::DaemonEvent::NodeIoMetrics`].
    ///
    /// A separate map because it arrives on a separate variant — see
    /// [`astrs_wire::NodeIoSample`] for why the two could not be one message
    /// — and kept beside `latest_metrics` under the same ephemerality rule.
    pub latest_io: BTreeMap<WireNodeId, NodeIoSample>,
}

impl LiveDataflow {
    /// A freshly registered dataflow with no placement resolved yet.
    #[must_use]
    pub fn new(
        id: DataflowId,
        name: Option<String>,
        manifest: Manifest,
        working_dir: Option<String>,
        graph: DataflowGraph,
    ) -> Self {
        Self {
            id,
            name,
            manifest,
            working_dir,
            graph,
            placement: ResolvedPlacement::default(),
            generations: BTreeMap::new(),
            dynamic_nodes: BTreeMap::new(),
            finished_daemons: std::collections::BTreeSet::new(),
            node_results: BTreeMap::new(),
            pending_build: None,
            pending_spawn: None,
            last_build_outcome: None,
            last_spawn_outcome: None,
            recording_node: None,
            build_id: None,
            latest_metrics: BTreeMap::new(),
            latest_io: BTreeMap::new(),
        }
    }

    /// The generation a node is currently on, `0` if it has never been
    /// spawned.
    #[must_use]
    pub fn generation(&self, node: &WireNodeId) -> u64 {
        self.generations.get(node).copied().unwrap_or(0)
    }

    /// Advances a node to its next generation (`1` on a first spawn),
    /// returning the new value.
    pub fn advance_generation(&mut self, node: &WireNodeId) -> u64 {
        let next = self.generation(node) + 1;
        self.generations.insert(node.clone(), next);
        next
    }

    /// Every daemon currently hosting at least one of this dataflow's
    /// nodes (manifest-declared or dynamically added).
    #[must_use]
    pub fn hosting_daemons(&self) -> BTreeSet<DaemonId> {
        let mut daemons: BTreeSet<_> = self.placement.node_daemon.values().cloned().collect();
        daemons.extend(self.placement.dynamic_daemons.values().cloned());
        daemons
    }

    /// The daemon hosting `node`, whether declared in the manifest or
    /// added dynamically.
    #[must_use]
    pub fn daemon_for_node(&self, node: &WireNodeId) -> Option<&DaemonId> {
        self.placement
            .dynamic_daemons
            .get(node)
            .or_else(|| self.placement.node_daemon.get(node))
    }
}

/// The live dataflow registry.
#[derive(Default)]
pub struct DataflowRegistry {
    dataflows: BTreeMap<DataflowId, LiveDataflow>,
    names: BTreeMap<String, DataflowId>,
}

impl DataflowRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a dataflow, indexing its name if it has one.
    ///
    /// A second registration under the same name replaces the name index
    /// entry (matching `astrs start` letting a finished run's name be
    /// reused).
    pub fn insert(&mut self, dataflow: LiveDataflow) {
        if let Some(name) = &dataflow.name {
            self.names.insert(name.clone(), dataflow.id);
        }
        self.dataflows.insert(dataflow.id, dataflow);
    }

    /// Removes a dataflow entirely (`Destroy`/`Clean`).
    pub fn remove(&mut self, id: DataflowId) -> Option<LiveDataflow> {
        let removed = self.dataflows.remove(&id)?;
        if let Some(name) = &removed.name
            && self.names.get(name) == Some(&id)
        {
            self.names.remove(name);
        }
        Some(removed)
    }

    /// Looks a dataflow up by id.
    #[must_use]
    pub fn get(&self, id: DataflowId) -> Option<&LiveDataflow> {
        self.dataflows.get(&id)
    }

    /// Looks a dataflow up by id, mutably.
    pub fn get_mut(&mut self, id: DataflowId) -> Option<&mut LiveDataflow> {
        self.dataflows.get_mut(&id)
    }

    /// Resolves a run's name to its id.
    #[must_use]
    pub fn id_for_name(&self, name: &str) -> Option<DataflowId> {
        self.names.get(name).copied()
    }

    /// Resolves a build to the dataflow it was for.
    ///
    /// A linear scan: the coordinator tracks a handful of dataflows at
    /// once, and adding a second index for a lookup used only by
    /// `Start { source: DataflowSource::Build }` is not worth the
    /// bookkeeping it would take to keep in sync with
    /// [`DataflowRegistry::remove`].
    #[must_use]
    pub fn id_for_build(&self, build: BuildId) -> Option<DataflowId> {
        self.dataflows
            .values()
            .find(|dataflow| dataflow.build_id == Some(build))
            .map(|dataflow| dataflow.id)
    }

    /// Every tracked dataflow, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &LiveDataflow> {
        self.dataflows.values()
    }

    /// How many dataflows are tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dataflows.len()
    }

    /// Whether no dataflow is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dataflows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn manifest_and_graph() -> (Manifest, DataflowGraph) {
        let manifest = Manifest::from_yaml_str("nodes:\n  - id: a\n    path: ./a\n").unwrap();
        manifest.validate().unwrap();
        let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty());
        (manifest, graph)
    }

    fn dataflow(name: Option<&str>) -> LiveDataflow {
        let (manifest, graph) = manifest_and_graph();
        LiveDataflow::new(
            DataflowId::generate(),
            name.map(str::to_owned),
            manifest,
            None,
            graph,
        )
    }

    #[test]
    fn generations_start_at_zero_and_advance_by_one() {
        let mut df = dataflow(None);
        let node = WireNodeId::new("a").unwrap();
        assert_eq!(df.generation(&node), 0);
        assert_eq!(df.advance_generation(&node), 1);
        assert_eq!(df.advance_generation(&node), 2);
        assert_eq!(df.generation(&node), 2);
    }

    #[test]
    fn registry_indexes_by_id_and_by_name() {
        let mut registry = DataflowRegistry::new();
        let df = dataflow(Some("demo"));
        let id = df.id;
        registry.insert(df);

        assert!(registry.get(id).is_some());
        assert_eq!(registry.id_for_name("demo"), Some(id));
        assert_eq!(registry.id_for_name("missing"), None);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn removing_a_dataflow_clears_its_name_index_entry() {
        let mut registry = DataflowRegistry::new();
        let df = dataflow(Some("demo"));
        let id = df.id;
        registry.insert(df);
        assert!(registry.remove(id).is_some());
        assert_eq!(registry.id_for_name("demo"), None);
        assert!(registry.is_empty());
    }

    #[test]
    fn reusing_a_name_repoints_the_index_without_touching_the_old_entry() {
        let mut registry = DataflowRegistry::new();
        let first = dataflow(Some("demo"));
        let first_id = first.id;
        registry.insert(first);

        let second = dataflow(Some("demo"));
        let second_id = second.id;
        registry.insert(second);

        assert_eq!(registry.id_for_name("demo"), Some(second_id));
        // The first registration is untouched (a real coordinator would
        // have already `remove`d it before reusing the name; this only
        // proves the index itself does not corrupt on a raw double-insert).
        assert!(registry.get(first_id).is_some());
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn pending_build_resolves_only_once_every_daemon_answers() {
        let a = DaemonId::generate(None);
        let b = DaemonId::generate(None);
        let mut pending = PendingBuild::new(BuildId::generate(), [a.clone(), b.clone()]);

        assert!(
            pending
                .record(
                    a,
                    BuildOutcome::Succeeded {
                        artifacts: vec![],
                        took: astrs_wire::DurationMs::new(1)
                    }
                )
                .is_none(),
            "one of two daemons has answered"
        );
        let summary = pending
            .record(
                b,
                BuildOutcome::Failed {
                    node: None,
                    exit_code: Some(1),
                    message: "boom".into(),
                    output: String::new(),
                },
            )
            .expect("both daemons have now answered");
        assert!(!summary.succeeded);
        assert_eq!(summary.failures.len(), 1);
    }

    #[test]
    fn pending_build_succeeds_when_every_outcome_does() {
        let a = DaemonId::generate(None);
        let mut pending = PendingBuild::new(BuildId::generate(), [a.clone()]);
        let summary = pending
            .record(
                a,
                BuildOutcome::Succeeded {
                    artifacts: vec![],
                    took: astrs_wire::DurationMs::new(1),
                },
            )
            .unwrap();
        assert!(summary.succeeded);
        assert!(summary.failures.is_empty());
    }

    #[test]
    fn pending_spawn_infers_failure_from_what_a_daemon_never_reports_ready() {
        let a = DaemonId::generate(None);
        let b = DaemonId::generate(None);
        let dispatched = BTreeMap::from([
            (a.clone(), vec![WireNodeId::new("x").unwrap()]),
            (b.clone(), vec![WireNodeId::new("y").unwrap()]),
        ]);
        let mut pending = PendingSpawn::new(dispatched);

        // `a` dispatched `x` but reports nothing ready: `x` is inferred
        // failed.
        assert!(pending.record(&a, &[]).is_none(), "b has not answered yet");
        // `b` dispatched `y` and reports it ready.
        let summary = pending
            .record(&b, &[WireNodeId::new("y").unwrap()])
            .unwrap();
        assert!(!summary.succeeded());
        assert_eq!(summary.failed_nodes, vec![WireNodeId::new("x").unwrap()]);
    }

    #[test]
    fn pending_spawn_succeeds_when_every_dispatched_node_is_reported_ready() {
        let a = DaemonId::generate(None);
        let dispatched = BTreeMap::from([(a.clone(), vec![WireNodeId::new("x").unwrap()])]);
        let mut pending = PendingSpawn::new(dispatched);
        let summary = pending
            .record(&a, &[WireNodeId::new("x").unwrap()])
            .unwrap();
        assert!(summary.succeeded());
    }

    #[test]
    fn id_for_build_resolves_and_survives_the_build_finishing() {
        let mut registry = DataflowRegistry::new();
        let mut df = dataflow(None);
        let id = df.id;
        let build = BuildId::generate();
        df.build_id = Some(build);
        registry.insert(df);

        assert_eq!(registry.id_for_build(build), Some(id));
        assert_eq!(registry.id_for_build(BuildId::generate()), None);

        // Clearing `pending_build` (as the build handler does once every
        // daemon has answered) must not disturb the permanent `build_id`.
        registry.get_mut(id).unwrap().pending_build = None;
        assert_eq!(registry.id_for_build(build), Some(id));
    }

    #[test]
    fn hosting_daemons_merges_static_and_dynamic_placement() {
        let mut df = dataflow(None);
        let static_daemon = DaemonId::generate(None);
        let dynamic_daemon = DaemonId::generate(None);
        df.placement
            .node_daemon
            .insert(WireNodeId::new("a").unwrap(), static_daemon.clone());
        df.placement
            .dynamic_daemons
            .insert(WireNodeId::new("extra").unwrap(), dynamic_daemon.clone());

        let hosting = df.hosting_daemons();
        assert!(hosting.contains(&static_daemon));
        assert!(hosting.contains(&dynamic_daemon));
        assert_eq!(
            df.daemon_for_node(&WireNodeId::new("a").unwrap()),
            Some(&static_daemon)
        );
        assert_eq!(
            df.daemon_for_node(&WireNodeId::new("extra").unwrap()),
            Some(&dynamic_daemon)
        );
    }
}
