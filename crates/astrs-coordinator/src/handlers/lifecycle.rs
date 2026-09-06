//! `Build`/`WaitForBuild`/`Start`/`WaitForSpawn`/`Stop`/`StopByName`/
//! `Restart`/`RestartByName`/`Destroy`/`Clean` — the dataflow finite state
//! machine (blueprint §4.2, §7.3, §12).
//!
//! This module also owns the two functions [`crate::session::daemon`]
//! calls when a `DaemonEvent::BuildResult`/`AllNodesReady` arrives:
//! [`handle_build_result`] and [`handle_all_nodes_ready`]. Keeping the
//! aggregation logic beside the code that starts the aggregation (rather
//! than splitting it into the daemon-session module) is what makes
//! [`crate::registry::PendingBuild`]/[`crate::registry::PendingSpawn`]'s
//! invariant — "resolved exactly once, and only from here" — checkable by
//! reading one file.

use std::collections::BTreeMap;
use std::time::Duration;

use astrs_graph::DataflowGraph;
use astrs_manifest::Manifest;
use astrs_wire::{
    BuildId, BuildOutcome, BuildStep, ControlReply, CoordinatorEvent, DaemonId, DataflowId,
    DataflowSource, DataflowStatus, DurationMs, ErrorCode, NodeId as WireNodeId, StopCause,
};
use tokio::sync::oneshot;

use crate::coordinator::Coordinator;
use crate::error::{CoordinatorError, Result};
use crate::graph_bridge;
use crate::placement::{self, ResolvedPlacement};
use crate::registry::{
    BuildOutcomeSummary, LiveDataflow, PendingBuild, PendingSpawn, SpawnOutcomeSummary,
};

/// How long a blocking `Start`/`WaitForBuild`/`WaitForSpawn` waits when the
/// caller gave no explicit timeout.
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------

/// `Build`.
pub async fn build(
    coordinator: &Coordinator,
    manifest_text: String,
    working_dir: Option<String>,
    name: Option<String>,
    _force: bool,
) -> ControlReply {
    let manifest = match parse_and_expand(&manifest_text, working_dir.as_deref()) {
        Ok(manifest) => manifest,
        Err(err) => return err.into_reply(),
    };
    let graph = match build_graph(&manifest) {
        Ok(graph) => graph,
        Err(err) => return err.into_reply(),
    };

    let dataflow_id = DataflowId::generate();
    let build_id = BuildId::generate();
    if let Err(err) = register_dataflow(coordinator, dataflow_id, name.clone(), &manifest).await {
        return err.into_reply();
    }

    let mut live = LiveDataflow::new(dataflow_id, name, manifest, working_dir, graph);
    live.build_id = Some(build_id);

    // Resolved on its own, `daemons()`-guard-and-all, *before* the
    // `match` below: a `std::sync::MutexGuard` kept alive by this
    // function's own locals up to (even past, for a plain `let` binding
    // whose scope encloses it) a later `.await` makes the whole `async
    // fn`'s future non-`Send` — see `send_spawn_events`'s docs for the
    // same limitation hit once already in this module.
    let resolve_result = {
        let daemons = coordinator.daemons();
        placement::resolve_placement(&live.graph, &daemons)
    };
    let resolved = match resolve_result {
        Ok(resolved) => resolved,
        Err(err) => {
            let _ = coordinator
                .store
                .set_dataflow_status(dataflow_id, DataflowStatus::Failed, None)
                .await;
            return err.into_reply();
        }
    };
    live.placement = resolved.clone();

    let steps_by_daemon = match build_steps_by_daemon(&live, &resolved) {
        Ok(steps) => steps,
        Err(err) => return err.into_reply(),
    };

    if steps_by_daemon.is_empty() {
        coordinator.dataflows().insert(live);
        let _ = coordinator
            .store
            .set_dataflow_status(dataflow_id, DataflowStatus::Ready, None)
            .await;
        return ControlReply::BuildStarted { build: build_id };
    }

    live.pending_build = Some(PendingBuild::new(build_id, steps_by_daemon.keys().cloned()));
    coordinator.dataflows().insert(live);
    let _ = coordinator
        .store
        .set_dataflow_status(dataflow_id, DataflowStatus::Building, None)
        .await;

    dispatch_build_steps(
        coordinator,
        build_id,
        dataflow_id,
        steps_by_daemon,
        working_dir_of(coordinator, dataflow_id).await,
    );
    ControlReply::BuildStarted { build: build_id }
}

async fn working_dir_of(coordinator: &Coordinator, dataflow: DataflowId) -> Option<String> {
    coordinator
        .dataflows()
        .get(dataflow)
        .and_then(|live| live.working_dir.clone())
}

fn dispatch_build_steps(
    coordinator: &Coordinator,
    build_id: BuildId,
    dataflow_id: DataflowId,
    steps_by_daemon: BTreeMap<DaemonId, Vec<BuildStep>>,
    working_dir: Option<String>,
) {
    let daemons = coordinator.daemons();
    for (daemon_id, steps) in steps_by_daemon {
        if let Some(handle) = daemons.get(&daemon_id) {
            let _ = handle.send(CoordinatorEvent::Build {
                build: build_id,
                dataflow: dataflow_id,
                steps,
                working_dir: working_dir.clone(),
            });
        }
    }
}

/// `WaitForBuild`.
pub async fn wait_for_build(
    coordinator: &Coordinator,
    build: BuildId,
    timeout: Option<DurationMs>,
) -> ControlReply {
    let Some(dataflow_id) = coordinator.dataflows().id_for_build(build) else {
        return CoordinatorError::NoSuchBuild(build).into_reply();
    };

    let pending_receiver = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow_id) else {
            return CoordinatorError::NoSuchBuild(build).into_reply();
        };
        if live.build_id != Some(build) {
            return CoordinatorError::NoSuchBuild(build).into_reply();
        }
        match live.pending_build.as_mut() {
            Some(pending) if pending.build == build => {
                let (tx, rx) = oneshot::channel();
                pending.waiters.push(tx);
                Some(rx)
            }
            _ => None,
        }
    };

    let summary = match pending_receiver {
        Some(receiver) => {
            let deadline = timeout.map_or(DEFAULT_WAIT_TIMEOUT, DurationMs::to_duration);
            match tokio::time::timeout(deadline, receiver).await {
                Ok(Ok(summary)) => summary,
                _ => return CoordinatorError::Timeout("build").into_reply(),
            }
        }
        None => coordinator
            .dataflows()
            .get(dataflow_id)
            .and_then(|live| live.last_build_outcome.clone())
            .unwrap_or_default(),
    };

    reply_for_build(&summary)
}

fn reply_for_build(summary: &BuildOutcomeSummary) -> ControlReply {
    if summary.succeeded {
        ControlReply::Ok
    } else {
        ControlReply::error_with_context(
            ErrorCode::BuildFailed,
            "build failed",
            summary.failures.clone(),
        )
    }
}

/// Resolves one pending build's outcome once every daemon that was asked
/// has answered — called from [`crate::session::daemon`] on every
/// `DaemonEvent::BuildResult`.
pub async fn handle_build_result(
    coordinator: &Coordinator,
    daemon: DaemonId,
    build: BuildId,
    dataflow: DataflowId,
    outcome: BuildOutcome,
) {
    let resolved = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return;
        };
        let matches = live
            .pending_build
            .as_ref()
            .is_some_and(|pending| pending.build == build);
        if !matches {
            return;
        }
        let summary = live
            .pending_build
            .as_mut()
            .and_then(|pending| pending.record(daemon, outcome));
        summary.map(|summary| {
            let waiters = std::mem::take(&mut live.pending_build)
                .map(|pending| pending.waiters)
                .unwrap_or_default();
            live.last_build_outcome = Some(summary.clone());
            (summary, waiters)
        })
    };
    let Some((summary, waiters)) = resolved else {
        return;
    };
    let status = if summary.succeeded {
        DataflowStatus::Ready
    } else {
        DataflowStatus::Failed
    };
    let _ = coordinator
        .store
        .set_dataflow_status(dataflow, status, None)
        .await;
    for waiter in waiters {
        let _ = waiter.send(summary.clone());
    }
}

// ---------------------------------------------------------------------
// Start / WaitForSpawn
// ---------------------------------------------------------------------

/// `Start`.
pub async fn start(
    coordinator: &Coordinator,
    source: DataflowSource,
    name: Option<String>,
    detach: bool,
) -> ControlReply {
    let dataflow_id = match resolve_source(coordinator, source, name.clone()).await {
        Ok(id) => id,
        Err(reply) => return reply,
    };
    if let Err(reply) = dispatch_spawn(coordinator, dataflow_id, name.clone()).await {
        return reply;
    }
    if detach {
        return ControlReply::Started {
            dataflow: dataflow_id,
            name,
        };
    }
    match wait_for_spawn_inner(coordinator, dataflow_id, None).await {
        Ok(summary) if summary.succeeded() => ControlReply::Started {
            dataflow: dataflow_id,
            name,
        },
        Ok(summary) => reply_for_spawn(&summary),
        Err(reply) => reply,
    }
}

async fn resolve_source(
    coordinator: &Coordinator,
    source: DataflowSource,
    name: Option<String>,
) -> std::result::Result<DataflowId, ControlReply> {
    match source {
        DataflowSource::Manifest { yaml, working_dir } => {
            let manifest = parse_and_expand(&yaml, working_dir.as_deref())
                .map_err(CoordinatorError::into_reply)?;
            let graph = build_graph(&manifest).map_err(CoordinatorError::into_reply)?;
            let dataflow_id = DataflowId::generate();
            register_dataflow(coordinator, dataflow_id, name.clone(), &manifest)
                .await
                .map_err(CoordinatorError::into_reply)?;
            let live = LiveDataflow::new(dataflow_id, name, manifest, working_dir, graph);
            coordinator.dataflows().insert(live);
            Ok(dataflow_id)
        }
        DataflowSource::Build { build } => coordinator
            .dataflows()
            .id_for_build(build)
            .ok_or_else(|| CoordinatorError::NoSuchBuild(build).into_reply()),
        // `DataflowSource` is `#[non_exhaustive]`; a source kind added in a
        // later protocol revision has no resolution decided for it yet.
        _ => Err(CoordinatorError::invalid("unrecognised dataflow source").into_reply()),
    }
}

async fn dispatch_spawn(
    coordinator: &Coordinator,
    dataflow_id: DataflowId,
    name: Option<String>,
) -> std::result::Result<(), ControlReply> {
    let per_daemon = {
        let daemons = coordinator.daemons();
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow_id) else {
            return Err(CoordinatorError::NoSuchDataflow(dataflow_id).into_reply());
        };
        let resolved = placement::resolve_placement(&live.graph, &daemons)
            .map_err(CoordinatorError::into_reply)?;
        live.placement = resolved.clone();

        let mut per_daemon: BTreeMap<DaemonId, Vec<CoordinatorEvent>> = BTreeMap::new();
        let mut dispatched: BTreeMap<DaemonId, Vec<WireNodeId>> = BTreeMap::new();
        // The full node list is cloned up front, once, rather than
        // re-searched by id inside the loop: `advance_generation` below
        // needs a mutable borrow of `live`, which cannot coexist with an
        // immutable borrow into `live.manifest.nodes` staying alive across
        // it, and re-deriving each node from its id would need a fallible
        // lookup for a case that cannot actually happen (every id in this
        // list came from this same `live.manifest.nodes`).
        let nodes: Vec<_> = live.manifest.nodes.clone();
        for node in nodes {
            let graph_id = astrs_graph::NodeId::new(node.id.clone());
            let Some(graph_node) = live.graph.node(&graph_id) else {
                continue;
            };
            let graph_node = graph_node.clone();
            let wire_id = WireNodeId::new(&node.id)
                .map_err(|err| CoordinatorError::from(err).into_reply())?;
            let generation = live.advance_generation(&wire_id);
            let spec = graph_bridge::build_node_spawn_spec(
                dataflow_id,
                generation,
                &live.manifest,
                &node,
                &live.graph,
                &graph_node,
                live.working_dir.as_deref(),
            )
            .map_err(CoordinatorError::into_reply)?;
            let routes = placement::routes_for_node(dataflow_id, &graph_id, &live.graph, &resolved)
                .map_err(CoordinatorError::into_reply)?;
            let Some(daemon_id) = resolved.node_daemon.get(&wire_id).cloned() else {
                continue;
            };
            dispatched
                .entry(daemon_id.clone())
                .or_default()
                .push(wire_id);
            per_daemon
                .entry(daemon_id)
                .or_default()
                .push(CoordinatorEvent::Spawn {
                    node: Box::new(spec),
                    routes,
                    dataflow_name: name.clone(),
                });
        }
        live.pending_spawn = Some(PendingSpawn::new(dispatched));

        // The cross-daemon edges (§6.4), queued *after* this daemon's
        // `Spawn`s and never before them: a daemon stamps a peer route with
        // the producing node's generation, and a node it has not been told to
        // spawn yet has no generation to stamp with (§12). Same queue, same
        // order, same connection — so "after" here is "after" on the wire.
        let directives =
            placement::peer_route_directives(dataflow_id, &live.graph, &resolved, &daemons)
                .map_err(CoordinatorError::into_reply)?;
        for (daemon_id, directives) in directives {
            per_daemon
                .entry(daemon_id)
                .or_default()
                .push(CoordinatorEvent::PeerRoutes {
                    dataflow: dataflow_id,
                    directives,
                });
        }
        per_daemon
    };

    send_spawn_events(coordinator, per_daemon);

    coordinator
        .store
        .set_dataflow_status(dataflow_id, DataflowStatus::Starting, None)
        .await
        .map_err(|err| CoordinatorError::from(err).into_reply())?;
    Ok(())
}

/// Sends every dispatched `Spawn` to its daemon.
///
/// Deliberately a plain (non-`async`) function, called without `.await`:
/// [`std::sync::MutexGuard`] is not `Send`, and while it never needs to be
/// held across an actual suspend point here, an `async fn`'s Send-ness
/// analysis is a conservative approximation of MIR liveness that can still
/// flag a guard used inside a loop/`if let` as "maybe live" at a later
/// `.await` in the *same* async function, even past an explicit `drop`.
/// Moving the loop into its own ordinary function call sidesteps the
/// analysis entirely — nothing inside a plain function call is ever part
/// of the caller's generated future.
fn send_spawn_events(
    coordinator: &Coordinator,
    per_daemon: BTreeMap<DaemonId, Vec<CoordinatorEvent>>,
) {
    let daemons = coordinator.daemons();
    for (daemon_id, events) in per_daemon {
        if let Some(handle) = daemons.get(&daemon_id) {
            for event in events {
                let _ = handle.send(event);
            }
        }
    }
}

/// `WaitForSpawn`.
pub async fn wait_for_spawn(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    timeout: Option<DurationMs>,
) -> ControlReply {
    match wait_for_spawn_inner(coordinator, dataflow, timeout).await {
        Ok(summary) => reply_for_spawn(&summary),
        Err(reply) => reply,
    }
}

async fn wait_for_spawn_inner(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    timeout: Option<DurationMs>,
) -> std::result::Result<SpawnOutcomeSummary, ControlReply> {
    let pending_receiver = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return Err(CoordinatorError::NoSuchDataflow(dataflow).into_reply());
        };
        match live.pending_spawn.as_mut() {
            Some(pending) => {
                let (tx, rx) = oneshot::channel();
                pending.waiters.push(tx);
                Some(rx)
            }
            None => None,
        }
    };
    match pending_receiver {
        Some(receiver) => {
            let deadline = timeout.map_or(DEFAULT_WAIT_TIMEOUT, DurationMs::to_duration);
            match tokio::time::timeout(deadline, receiver).await {
                Ok(Ok(summary)) => Ok(summary),
                _ => Err(CoordinatorError::Timeout("spawn").into_reply()),
            }
        }
        None => Ok(coordinator
            .dataflows()
            .get(dataflow)
            .and_then(|live| live.last_spawn_outcome.clone())
            .unwrap_or_default()),
    }
}

fn reply_for_spawn(summary: &SpawnOutcomeSummary) -> ControlReply {
    if summary.succeeded() {
        ControlReply::Ok
    } else {
        ControlReply::error_with_context(
            ErrorCode::FailedPrecondition,
            "one or more nodes failed to spawn",
            summary
                .failed_nodes
                .iter()
                .map(std::string::ToString::to_string),
        )
    }
}

/// Resolves one pending spawn's outcome once every hosting daemon has
/// reported `AllNodesReady`, and fans the aggregate back out to every one
/// of them (blueprint §4.3: a producer must not deliver before every
/// consumer exists). Called from [`crate::session::daemon`].
///
/// `reported_ready` is [`astrs_wire::DaemonEvent::AllNodesReady::nodes`]
/// verbatim — the nodes *this daemon* confirms are up, not a failure list
/// (the wire carries none); [`crate::registry::PendingSpawn::record`] is
/// what infers failure from the gap between what was dispatched to this
/// daemon and what it reports back.
pub async fn handle_all_nodes_ready(
    coordinator: &Coordinator,
    daemon: DaemonId,
    dataflow: DataflowId,
    reported_ready: Vec<WireNodeId>,
) {
    let resolved = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return;
        };
        if live.pending_spawn.is_none() {
            return;
        }
        let summary = live
            .pending_spawn
            .as_mut()
            .and_then(|pending| pending.record(&daemon, &reported_ready));
        summary.map(|summary| {
            let waiters = std::mem::take(&mut live.pending_spawn)
                .map(|pending| pending.waiters)
                .unwrap_or_default();
            live.last_spawn_outcome = Some(summary.clone());
            (summary, waiters)
        })
    };
    let Some((summary, waiters)) = resolved else {
        return;
    };

    let hosting: Vec<_> = coordinator
        .dataflows()
        .get(dataflow)
        .map(|live| live.hosting_daemons().into_iter().collect())
        .unwrap_or_default();
    let event = CoordinatorEvent::AllNodesReady {
        dataflow,
        failed: summary.failed_nodes.clone(),
    };
    {
        let daemons = coordinator.daemons();
        for id in &hosting {
            if let Some(handle) = daemons.get(id) {
                let _ = handle.send(event.clone());
            }
        }
    }

    let status = if summary.succeeded() {
        DataflowStatus::Running
    } else {
        DataflowStatus::Failed
    };
    let _ = coordinator
        .store
        .set_dataflow_status(dataflow, status, Some(coordinator.clock.now()))
        .await;
    for waiter in waiters {
        let _ = waiter.send(summary.clone());
    }
}

// ---------------------------------------------------------------------
// Per-node daemon events: SpawnResult, NodeStopped, AllNodesFinished
// ---------------------------------------------------------------------

/// Persists one node's `SpawnResult`, preserving whatever port
/// declarations and restart count a previous record already carried
/// (`SpawnResult` itself says nothing about ports). Called from
/// [`crate::session::daemon`].
pub async fn handle_spawn_result(
    coordinator: &Coordinator,
    daemon: DaemonId,
    dataflow: DataflowId,
    node: WireNodeId,
    generation: u64,
    outcome: astrs_wire::SpawnOutcome,
) {
    use astrs_wire::{NodeExitCause, NodeRunState, SpawnOutcome};

    let existing = coordinator
        .store
        .get_node_status(dataflow, node.clone())
        .await
        .ok()
        .flatten();
    let (inputs, outputs, restart_count) = existing
        .map(|record| {
            (
                record.info.inputs,
                record.info.outputs,
                record.info.restart_count,
            )
        })
        .unwrap_or_default();

    let (state, pid, started_at, exit_cause) = match &outcome {
        SpawnOutcome::Spawned { pid, started_at } => {
            (NodeRunState::Spawning, *pid, Some(*started_at), None)
        }
        SpawnOutcome::AwaitingDynamic => (NodeRunState::Spawning, None, None, None),
        SpawnOutcome::Failed { message, .. } => (
            NodeRunState::Failed,
            None,
            None,
            Some(NodeExitCause::SpawnFailed {
                message: message.clone(),
            }),
        ),
        SpawnOutcome::Cancelled => (
            NodeRunState::Failed,
            None,
            None,
            Some(NodeExitCause::Cancelled),
        ),
        // `SpawnOutcome` is `#[non_exhaustive]`.
        _ => (NodeRunState::Pending, None, None, None),
    };

    let info = astrs_wire::NodeInfo {
        dataflow,
        node,
        daemon,
        state,
        pid,
        generation,
        restart_count,
        inputs,
        outputs,
        started_at,
        exit_cause,
    };
    let _ = coordinator.store.set_node_status(info).await;
}

/// Persists one node's `NodeStopped`. Called from [`crate::session::daemon`].
pub async fn handle_node_stopped(
    coordinator: &Coordinator,
    daemon: DaemonId,
    dataflow: DataflowId,
    node: WireNodeId,
    generation: u64,
    cause: astrs_wire::NodeExitCause,
    restarting: bool,
) {
    use astrs_wire::NodeRunState;

    let existing = coordinator
        .store
        .get_node_status(dataflow, node.clone())
        .await
        .ok()
        .flatten();
    let (inputs, outputs, mut restart_count, pid, started_at) = existing
        .map(|record| {
            (
                record.info.inputs,
                record.info.outputs,
                record.info.restart_count,
                record.info.pid,
                record.info.started_at,
            )
        })
        .unwrap_or_default();
    if restarting {
        restart_count += 1;
    }
    let state = if restarting {
        NodeRunState::Restarting
    } else if cause.is_success() {
        NodeRunState::Finished
    } else {
        NodeRunState::Failed
    };

    let info = astrs_wire::NodeInfo {
        dataflow,
        node,
        daemon,
        state,
        pid: if restarting { pid } else { None },
        generation,
        restart_count,
        inputs,
        outputs,
        started_at,
        exit_cause: Some(cause),
    };
    let _ = coordinator.store.set_node_status(info).await;
}

/// Persists every node's `AllNodesFinished` result and, once every
/// hosting daemon has reported, finalizes the dataflow's terminal status
/// (`Finished` if nothing failed, `Failed` otherwise). Called from
/// [`crate::session::daemon`].
///
/// It also closes any spawn this daemon is still being awaited for. A daemon
/// that has reported `AllNodesFinished` will send no `AllNodesReady` for this
/// run — it reports each exactly once per dataflow — so a `Start` that was
/// still waiting for one would wait out its whole timeout for an answer that
/// can no longer come. This is the backstop under
/// [`crate::handlers::lifecycle::handle_all_nodes_ready`], not a replacement
/// for it: in a healthy run the barrier resolves first and the code below
/// finds nothing pending.
pub async fn handle_all_nodes_finished(
    coordinator: &Coordinator,
    daemon: DaemonId,
    dataflow: DataflowId,
    results: BTreeMap<WireNodeId, astrs_wire::NodeExitCause>,
) {
    for (node, cause) in &results {
        persist_finished_node(coordinator, dataflow, node.clone(), &daemon, cause.clone()).await;
    }

    // A node that reached a terminal state under any cause other than
    // `SpawnFailed` did come up, so it is truthfully "ready" for the purpose
    // of the spawn barrier. Draining `PendingSpawn::failed_nodes` instead
    // would report every node this daemon had not yet confirmed as a spawn
    // failure — for a dataflow that simply finished quickly, all of them.
    let came_up: Vec<WireNodeId> = results
        .iter()
        .filter(|(_, cause)| !matches!(cause, astrs_wire::NodeExitCause::SpawnFailed { .. }))
        .map(|(node, _)| node.clone())
        .collect();

    let (should_finalize, resolved_spawn) = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return;
        };
        live.node_results.extend(results);
        live.finished_daemons.insert(daemon.clone());
        let resolved_spawn = resolve_pending_spawn(live, &daemon, &came_up);
        let hosting = live.hosting_daemons();
        let finalize =
            !hosting.is_empty() && hosting.iter().all(|id| live.finished_daemons.contains(id));
        (finalize, resolved_spawn)
    };
    if let Some((summary, waiters)) = resolved_spawn {
        for waiter in waiters {
            let _ = waiter.send(summary.clone());
        }
    }
    if !should_finalize {
        return;
    }
    let any_failed = coordinator.dataflows().get(dataflow).is_some_and(|live| {
        live.node_results
            .values()
            .any(astrs_wire::NodeExitCause::is_failure)
    });
    let status = if any_failed {
        DataflowStatus::Failed
    } else {
        DataflowStatus::Finished
    };
    let _ = coordinator
        .store
        .set_dataflow_status(dataflow, status, None)
        .await;
}

/// Records `daemon`'s implicit spawn answer on `live`'s pending spawn, taking
/// the waiters out when that answer completes the set.
///
/// Split out of [`handle_all_nodes_finished`] so the whole borrow of `live`
/// (and the registry lock behind it) ends before anything is awaited — the
/// same reason [`send_spawn_events`] is a plain function.
fn resolve_pending_spawn(
    live: &mut LiveDataflow,
    daemon: &DaemonId,
    came_up: &[WireNodeId],
) -> Option<(
    SpawnOutcomeSummary,
    Vec<oneshot::Sender<SpawnOutcomeSummary>>,
)> {
    let summary = live
        .pending_spawn
        .as_mut()
        .and_then(|pending| pending.record(daemon, came_up))?;
    let waiters = std::mem::take(&mut live.pending_spawn)
        .map(|pending| pending.waiters)
        .unwrap_or_default();
    live.last_spawn_outcome = Some(summary.clone());
    Some((summary, waiters))
}

async fn persist_finished_node(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: WireNodeId,
    daemon: &DaemonId,
    cause: astrs_wire::NodeExitCause,
) {
    use astrs_wire::NodeRunState;

    let existing = coordinator
        .store
        .get_node_status(dataflow, node.clone())
        .await
        .ok()
        .flatten();
    let (daemon, generation, inputs, outputs, restart_count, pid, started_at) = match existing {
        Some(record) => (
            record.info.daemon,
            record.info.generation,
            record.info.inputs,
            record.info.outputs,
            record.info.restart_count,
            record.info.pid,
            record.info.started_at,
        ),
        None => (
            daemon.clone(),
            1,
            Default::default(),
            Default::default(),
            0,
            None,
            None,
        ),
    };
    let state = if cause.is_failure() {
        NodeRunState::Failed
    } else {
        NodeRunState::Finished
    };
    let info = astrs_wire::NodeInfo {
        dataflow,
        node,
        daemon,
        state,
        pid,
        generation,
        restart_count,
        inputs,
        outputs,
        started_at,
        exit_cause: Some(cause),
    };
    let _ = coordinator.store.set_node_status(info).await;
}

/// Releases every pending build/spawn that was still waiting on `daemon`
/// when its connection ended, so a `WaitForBuild`/`WaitForSpawn`/`Start`
/// caller is answered a failure rather than left hanging until its own
/// deadline. Called from [`crate::session::daemon`] on disconnect.
pub async fn handle_daemon_disconnected(coordinator: &Coordinator, daemon: DaemonId) {
    let dataflow_ids: Vec<DataflowId> =
        coordinator.dataflows().iter().map(|live| live.id).collect();
    for id in dataflow_ids {
        release_pending_build(coordinator, id, &daemon).await;
        release_pending_spawn(coordinator, id, &daemon).await;
    }
}

async fn release_pending_build(coordinator: &Coordinator, dataflow: DataflowId, daemon: &DaemonId) {
    let resolved = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return;
        };
        let awaits = live
            .pending_build
            .as_ref()
            .is_some_and(|pending| pending.awaiting.contains(daemon));
        if !awaits {
            return;
        }
        let outcome = BuildOutcome::Failed {
            node: None,
            exit_code: None,
            message: "daemon disconnected before the build finished".to_owned(),
            output: String::new(),
        };
        let summary = live
            .pending_build
            .as_mut()
            .and_then(|pending| pending.record(daemon.clone(), outcome));
        summary.map(|summary| {
            let waiters = std::mem::take(&mut live.pending_build)
                .map(|pending| pending.waiters)
                .unwrap_or_default();
            live.last_build_outcome = Some(summary.clone());
            (summary, waiters)
        })
    };
    let Some((summary, waiters)) = resolved else {
        return;
    };
    let _ = coordinator
        .store
        .set_dataflow_status(dataflow, DataflowStatus::Failed, None)
        .await;
    for waiter in waiters {
        let _ = waiter.send(summary.clone());
    }
}

async fn release_pending_spawn(coordinator: &Coordinator, dataflow: DataflowId, daemon: &DaemonId) {
    let resolved = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return;
        };
        let awaits = live
            .pending_spawn
            .as_ref()
            .is_some_and(|pending| pending.awaiting.contains(daemon));
        if !awaits {
            return;
        }
        // No node dispatched to a now-gone daemon can possibly be
        // reported ready.
        let summary = live
            .pending_spawn
            .as_mut()
            .and_then(|pending| pending.record(daemon, &[]));
        summary.map(|summary| {
            let waiters = std::mem::take(&mut live.pending_spawn)
                .map(|pending| pending.waiters)
                .unwrap_or_default();
            live.last_spawn_outcome = Some(summary.clone());
            (summary, waiters)
        })
    };
    let Some((summary, waiters)) = resolved else {
        return;
    };
    let status = if summary.succeeded() {
        DataflowStatus::Running
    } else {
        DataflowStatus::Failed
    };
    let _ = coordinator
        .store
        .set_dataflow_status(dataflow, status, None)
        .await;
    for waiter in waiters {
        let _ = waiter.send(summary.clone());
    }
}

// ---------------------------------------------------------------------
// Stop / Restart / Destroy / Clean
// ---------------------------------------------------------------------

/// `Stop`.
pub async fn stop(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    grace: Option<DurationMs>,
) -> ControlReply {
    match stop_inner(coordinator, dataflow, grace).await {
        Ok(()) => ControlReply::Ok,
        Err(reply) => reply,
    }
}

async fn stop_inner(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    grace: Option<DurationMs>,
) -> std::result::Result<(), ControlReply> {
    let meta = coordinator
        .store
        .get_dataflow_meta(dataflow)
        .await
        .map_err(|err| CoordinatorError::from(err).into_reply())?
        .ok_or_else(|| CoordinatorError::NoSuchDataflow(dataflow).into_reply())?;
    if !meta.status.is_active() {
        return Err(CoordinatorError::WrongDataflowState {
            dataflow,
            status: meta.status,
            action: "stop",
        }
        .into_reply());
    }
    coordinator
        .store
        .set_dataflow_status(dataflow, DataflowStatus::Stopping, None)
        .await
        .map_err(|err| CoordinatorError::from(err).into_reply())?;

    let hosting: Vec<_> = coordinator
        .dataflows()
        .get(dataflow)
        .map(|live| live.hosting_daemons().into_iter().collect())
        .unwrap_or_default();
    let event = CoordinatorEvent::StopDataflow {
        dataflow,
        grace,
        cause: StopCause::Requested,
    };
    let daemons = coordinator.daemons();
    for id in &hosting {
        if let Some(handle) = daemons.get(id) {
            let _ = handle.send(event.clone());
        }
    }
    Ok(())
}

/// `StopByName`.
pub async fn stop_by_name(
    coordinator: &Coordinator,
    name: String,
    grace: Option<DurationMs>,
) -> ControlReply {
    // Bound to a local first, not matched on directly: a temporary
    // `MutexGuard` created in a `match`'s scrutinee lives for the whole
    // `match` (every arm), which would keep it "alive" across the
    // `.await` below in the compiler's async-fn Send analysis.
    let id = coordinator.dataflows().id_for_name(&name);
    match id {
        Some(id) => stop(coordinator, id, grace).await,
        None => CoordinatorError::NoSuchDataflowName(name).into_reply(),
    }
}

/// `Restart`.
///
/// Stops the dataflow (best-effort — a dataflow that already finished on
/// its own is not an error to restart) and re-dispatches every node with
/// a fresh generation. `rebuild` re-dispatches the build steps first, but
/// — unlike the explicit `Build` → `WaitForBuild` → `Start` sequence a
/// caller can run by hand — does **not** wait for the rebuild to finish
/// before spawning: both dispatches ride the same per-daemon channel, so
/// a real daemon that processes messages in order will still build before
/// it spawns, but this call does not confirm that before returning. A
/// caller that needs the confirmed ordering should use the explicit
/// sequence instead.
pub async fn restart(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    rebuild: bool,
) -> ControlReply {
    let (name, exists) = {
        let dataflows = coordinator.dataflows();
        match dataflows.get(dataflow) {
            Some(live) => (live.name.clone(), true),
            None => (None, false),
        }
    };
    if !exists {
        return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
    }

    let _ = stop_inner(coordinator, dataflow, None).await;

    if rebuild {
        let steps_by_daemon = {
            let daemons = coordinator.daemons();
            let dataflows = coordinator.dataflows();
            let Some(live) = dataflows.get(dataflow) else {
                return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
            };
            let resolved = match placement::resolve_placement(&live.graph, &daemons) {
                Ok(resolved) => resolved,
                Err(err) => return err.into_reply(),
            };
            match build_steps_by_daemon(live, &resolved) {
                Ok(steps) => steps,
                Err(err) => return err.into_reply(),
            }
        };
        if !steps_by_daemon.is_empty() {
            dispatch_build_steps(
                coordinator,
                BuildId::generate(),
                dataflow,
                steps_by_daemon,
                working_dir_of(coordinator, dataflow).await,
            );
        }
    }

    match dispatch_spawn(coordinator, dataflow, name).await {
        Ok(()) => ControlReply::Ok,
        Err(reply) => reply,
    }
}

/// `RestartByName`.
pub async fn restart_by_name(
    coordinator: &Coordinator,
    name: String,
    rebuild: bool,
) -> ControlReply {
    // See `stop_by_name`'s comment on why this is bound before matching.
    let id = coordinator.dataflows().id_for_name(&name);
    match id {
        Some(id) => restart(coordinator, id, rebuild).await,
        None => CoordinatorError::NoSuchDataflowName(name).into_reply(),
    }
}

/// `Destroy`.
pub async fn destroy(coordinator: &Coordinator, force: bool) -> ControlReply {
    let metas = match coordinator.store.list_dataflows().await {
        Ok(metas) => metas,
        Err(err) => return CoordinatorError::from(err).into_reply(),
    };
    if !force && metas.iter().any(|meta| meta.status.is_active()) {
        return CoordinatorError::invalid(
            "active dataflows exist; pass force to destroy the cluster anyway",
        )
        .into_reply();
    }

    let daemon_ids: Vec<_> = coordinator.daemons().ids().cloned().collect();
    {
        let daemons = coordinator.daemons();
        for id in &daemon_ids {
            if let Some(handle) = daemons.get(id) {
                let _ = handle.send(CoordinatorEvent::Destroy { grace: None });
            }
        }
    }
    for meta in &metas {
        let _ = coordinator.store.remove_dataflow(meta.id).await;
        coordinator.dataflows().remove(meta.id);
    }
    ControlReply::Ok
}

/// `Clean`.
pub async fn clean(
    coordinator: &Coordinator,
    dataflow: Option<DataflowId>,
    _artifacts: bool,
    _logs: bool,
) -> ControlReply {
    let targets = match dataflow {
        Some(id) => vec![id],
        None => match coordinator.store.list_dataflows().await {
            Ok(metas) => metas
                .into_iter()
                .filter(|meta| meta.status.is_terminal())
                .map(|meta| meta.id)
                .collect(),
            Err(err) => return CoordinatorError::from(err).into_reply(),
        },
    };

    for id in targets {
        let meta = match coordinator.store.get_dataflow_meta(id).await {
            Ok(Some(meta)) => meta,
            Ok(None) => {
                if dataflow.is_some() {
                    return CoordinatorError::NoSuchDataflow(id).into_reply();
                }
                continue;
            }
            Err(err) => return CoordinatorError::from(err).into_reply(),
        };
        if !meta.status.is_terminal() {
            if dataflow.is_some() {
                return CoordinatorError::WrongDataflowState {
                    dataflow: id,
                    status: meta.status,
                    action: "clean",
                }
                .into_reply();
            }
            continue;
        }
        if let Ok(nodes) = coordinator.store.list_node_status(id).await {
            for node in nodes {
                let _ = coordinator
                    .store
                    .remove_node_status(id, node.info.node)
                    .await;
            }
        }
        let _ = coordinator.store.remove_dataflow(id).await;
        coordinator.dataflows().remove(id);
    }
    ControlReply::Ok
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

/// Parses, validates, expands and re-validates a manifest — the full
/// pipeline blueprint §8's two-stage error model plus §8.5 expansion
/// implies before a manifest is fit to build a graph from.
fn parse_and_expand(yaml: &str, working_dir: Option<&str>) -> Result<Manifest> {
    let manifest = Manifest::from_yaml_str(yaml)?;
    manifest.validate()?;
    let base_dir = std::path::Path::new(working_dir.unwrap_or("."));
    let loader = astrs_manifest::expand::FsModuleLoader;
    let expanded = manifest.expand(base_dir, &loader)?;
    expanded.validate()?;
    Ok(expanded)
}

/// Builds the graph model, rejecting a manifest whose construction-time
/// diagnostics already include an error (blueprint §3.7: typed by default).
fn build_graph(manifest: &Manifest) -> Result<DataflowGraph> {
    let (graph, diagnostics) = DataflowGraph::from_manifest(manifest)?;
    reject_error_diagnostics(&diagnostics)?;
    reject_error_diagnostics(&graph.diagnostics())?;
    Ok(graph)
}

fn reject_error_diagnostics(diagnostics: &[astrs_graph::Diagnostic]) -> Result<()> {
    let errors: Vec<String> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == astrs_graph::Severity::Error)
        .map(std::string::ToString::to_string)
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CoordinatorError::invalid(format!(
            "graph has {} error(s): {}",
            errors.len(),
            errors.join("; ")
        )))
    }
}

/// Registers (or re-registers) a dataflow's durable metadata.
async fn register_dataflow(
    coordinator: &Coordinator,
    id: DataflowId,
    name: Option<String>,
    manifest: &Manifest,
) -> Result<()> {
    let manifest_json = serde_json::to_string(manifest).map_err(|source| {
        CoordinatorError::invalid(format!("manifest failed to serialize: {source}"))
    })?;
    let node_count = u32::try_from(manifest.nodes.len()).unwrap_or(u32::MAX);
    coordinator
        .store
        .upsert_dataflow(id, name, manifest_json, node_count)
        .await?;
    Ok(())
}

/// Groups every node's build steps (blueprint §16: `git clone` + `build:`,
/// never through a shell) by the daemon that will run them.
fn build_steps_by_daemon(
    live: &LiveDataflow,
    resolved: &ResolvedPlacement,
) -> Result<BTreeMap<DaemonId, Vec<BuildStep>>> {
    let mut steps_by_daemon: BTreeMap<DaemonId, Vec<BuildStep>> = BTreeMap::new();
    for node in &live.manifest.nodes {
        if node.build.is_none() && node.git.is_none() {
            continue;
        }
        let wire_id = WireNodeId::new(&node.id)?;
        let Some(daemon_id) = resolved.node_daemon.get(&wire_id).cloned() else {
            continue;
        };
        let steps = graph_bridge::build_steps_for_node(node, live.working_dir.as_deref())?;
        steps_by_daemon.entry(daemon_id).or_default().extend(steps);
    }
    Ok(steps_by_daemon)
}

#[cfg(test)]
mod tests;
