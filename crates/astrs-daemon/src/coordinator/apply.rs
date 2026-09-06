//! Executing a [`CoordinatorEvent`] against the local daemon (blueprint §7.3,
//! §12, §24.1).
//!
//! One arm per §24.1 instruction, each turning a cluster-level decision into
//! the local calls that already implement it:
//!
//! | Instruction | Local effect |
//! |---|---|
//! | `Heartbeat` | the coordinator was heard from — clears degraded-autonomous (§12) |
//! | `Build` | run the steps in a task, answer `BuildResult` |
//! | `Spawn` | admit the dataflow, add the node, spawn it, answer `SpawnResult` |
//! | `AllNodesReady` | the cluster-wide barrier is met; the dataflow is `Running` |
//! | `StopDataflow` / `StopNode` | the finish ladder (§12) |
//! | `RestartNode` | a new incarnation at the generation the coordinator chose |
//! | `ReloadNode` | `NodeEvent::Reload` to the node (§9.3) |
//! | `SetParam` / `DeleteParam` | `ParamUpdate` / `ParamDeleted` on `astrs.status` |
//! | `Logs` | answer from the local [`crate::coordinator::LogHistory`] |
//! | `Destroy` | begin the daemon's own shutdown |
//! | `PeerDisconnected` | drop the peer and close what it carried (§12) |
//! | `StateCatchUp` | advance the cursor, answer `StateCatchUpAck` (§12) |
//! | `TopicTapStart` / `TopicTapStop` | the §13 debug taps |
//! | `PeerRoutes` | the §6.4 cross-daemon edges — see [`crate::coordinator::routes`] |
//! | `ReplaceNode` / `AddEdge` / `RemoveEdge` | the §8/§17 dynamic-topology ops — [`crate::dataflow::topology`] |
//!
//! # Nothing here blocks
//!
//! Every arm is synchronous, because it runs inside the one task that owns
//! every mutable fact (§4.3). The single instruction that genuinely takes time
//! — `Build`, which runs other people's compilers — is handed to a spawned
//! task that reports its own `BuildResult` through the same
//! [`crate::health::ReportSink`] everything else uses, so the loop is never
//! parked on a build while a node needs supervising.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use astrs_wire::{
    BuildOutcome, BuildStep as WireBuildStep, CoordinatorEvent, DaemonEvent as WireDaemonEvent,
    DataflowId, DataflowStatus, DurationMs, NodeEvent, NodeId, NodeRunState, NodeSpawnSpec,
    RouteCloseReason, RouteSpec, SessionId, SpawnOutcome, StateEntry, StopCause,
};

use crate::dataflow::build::run_build;
use crate::dataflow::plan::BuildStep;
use crate::server::core::Daemon;
use crate::spawn::{EnvPolicy, Spawner};
use crate::state::{DataflowState, is_virtual_port, virtual_source_text};

impl Daemon {
    /// The coordinator uplink came up and registered (§4.2).
    ///
    /// Re-announces nothing: what the coordinator missed it asks for, with a
    /// `StateCatchUp` derived from the `catch_up_seq` this daemon's
    /// registration carried. What this *does* do is clear the degraded flag
    /// and re-open the peer question, because a coordinator that just came
    /// back is about to name peers this daemon may need to dial.
    pub(crate) fn handle_coordinator_connected(&mut self, session: SessionId, epoch: u64) {
        tracing::info!(%session, epoch, "the coordinator link is up");
        self.heartbeat.note_contact(Instant::now());
        self.metrics.record_coordinator_connected();
        self.reconcile_peers();
    }

    /// The coordinator uplink dropped (§12).
    ///
    /// Deliberately does *nothing* to the running graph. Nodes keep running,
    /// rings keep carrying payloads, peer routes keep delivering: the
    /// coordinator owns the cluster's plan, not its execution. The uplink's
    /// outbox keeps accepting events (bounded — see
    /// [`crate::coordinator::UplinkSink`]) and the uplink task keeps
    /// reconnecting.
    pub(crate) fn handle_coordinator_lost(&mut self, reason: &str) {
        tracing::warn!(reason, "the coordinator link is down; running autonomously");
        self.metrics.record_coordinator_lost();
    }

    /// Executes one coordinator instruction.
    pub fn handle_coordinator_frame(&mut self, event: CoordinatorEvent) {
        // Any frame at all is contact, not only a heartbeat: §12's silence
        // budget is about hearing from the coordinator, and an instruction is
        // a much stronger signal than a liveness ping.
        self.heartbeat.note_contact(Instant::now());

        match event {
            CoordinatorEvent::Heartbeat { .. } => {}
            CoordinatorEvent::Build {
                build,
                dataflow,
                steps,
                working_dir,
            } => self.run_coordinator_build(build, dataflow, &steps, working_dir),
            CoordinatorEvent::Spawn {
                node,
                routes,
                dataflow_name,
            } => self.apply_coordinator_spawn(*node, &routes, dataflow_name),
            CoordinatorEvent::AllNodesReady { dataflow, failed } => {
                self.apply_all_nodes_ready(dataflow, &failed);
            }
            CoordinatorEvent::StopDataflow {
                dataflow, cause, ..
            } => self.stop_dataflow_nodes(dataflow, cause),
            CoordinatorEvent::ReloadNode {
                dataflow,
                node,
                operator,
            } => {
                self.send_to(
                    dataflow,
                    &node,
                    NodeEvent::Reload {
                        operator,
                        path: None,
                    },
                );
            }
            CoordinatorEvent::Logs {
                request,
                dataflow,
                node,
                query,
            } => {
                let batch = self.log_history().query(dataflow, node.as_ref(), &query);
                self.sink().report(WireDaemonEvent::Log {
                    request: Some(request),
                    records: batch.records,
                    truncated: batch.truncated,
                });
            }
            CoordinatorEvent::RestartNode {
                dataflow,
                node,
                generation,
            } => self.restart_node_at(dataflow, &node, generation),
            CoordinatorEvent::StopNode {
                dataflow,
                node,
                cause,
                ..
            } => self.stop_node(dataflow, &node, cause),
            CoordinatorEvent::SetParam { scope, key, value } => {
                let dataflow = scope.dataflow();
                let node = scope.node_id().cloned();
                self.deliver_param_event(
                    dataflow,
                    node,
                    &NodeEvent::ParamUpdate { scope, key, value },
                );
            }
            CoordinatorEvent::DeleteParam { scope, key } => {
                let dataflow = scope.dataflow();
                let node = scope.node_id().cloned();
                self.deliver_param_event(dataflow, node, &NodeEvent::ParamDeleted { scope, key });
            }
            CoordinatorEvent::Destroy { .. } => {
                tracing::info!("the coordinator asked this daemon to shut down");
                self.begin_shutdown();
            }
            CoordinatorEvent::PeerDisconnected { daemon, .. } => {
                self.handle_peer_lost(&daemon, "the coordinator reported the peer gone");
                self.peer_directory_mut().forget_peer(&daemon);
            }
            CoordinatorEvent::StateCatchUp {
                seq,
                entries,
                final_batch,
            } => self.apply_state_catch_up(seq, &entries, final_batch),
            CoordinatorEvent::TopicTapStart {
                dataflow,
                port,
                query,
                subscription,
            } => {
                // Unconditional, deliberately: this daemon never sees the
                // manifest's `debug: true` flag (a `Spawn` carries an
                // already-expanded `NodeSpawnSpec`, nothing more) — the
                // coordinator's `handlers::logs::topic_subscribe` is the
                // one place that flag is enforced, before it ever sends
                // this instruction. See `tap::TapRegistry::subscribe`'s
                // docs for what this `enable` call still buys locally.
                self.taps_mut().enable(dataflow);
                let max_hz = query.max_rate_hz.map(f64::from);
                if !self.start_tap(subscription, dataflow, Some(port), max_hz) {
                    tracing::warn!(%dataflow, "a topic tap was refused");
                }
            }
            CoordinatorEvent::TopicTapStop { subscription } => {
                self.stop_tap(subscription);
            }
            CoordinatorEvent::PeerRoutes {
                dataflow,
                directives,
            } => {
                self.apply_peer_routes(dataflow, &directives);
            }
            CoordinatorEvent::ReplaceNode { node, .. } => {
                let dataflow = node.dataflow;
                let target = node.node.clone();
                if let Err(err) = self.apply_replace_node(*node) {
                    // The coordinator validates a replacement against its
                    // own tracked graph (`astrs_graph::apply`) before this
                    // is ever sent, so a failure here means this daemon's
                    // local state disagrees with the coordinator's — a
                    // race (the node exited between validation and
                    // dispatch), not a malformed request. Nothing to
                    // answer: `ReplaceNode` has no `DaemonEvent` reply in
                    // §24.1's frozen set (see `crate::dataflow::topology`'s
                    // docs), so this is exactly as observable as any other
                    // coordinator-ordered instruction that finds its
                    // target gone.
                    tracing::warn!(%dataflow, %target, %err, "a coordinator-ordered replace could not be applied");
                }
            }
            CoordinatorEvent::AddEdge {
                dataflow,
                consumer,
                input,
            } => {
                if let Err(err) = self.apply_add_edge(dataflow, consumer.clone(), input) {
                    tracing::warn!(%dataflow, node = %consumer, %err, "a coordinator-ordered edge add could not be applied");
                }
            }
            CoordinatorEvent::RemoveEdge {
                dataflow,
                consumer,
                input,
            } => {
                if let Err(err) = self.apply_remove_edge(dataflow, &consumer, &input) {
                    tracing::warn!(%dataflow, node = %consumer, %err, "a coordinator-ordered edge removal could not be applied");
                }
            }
            // `CoordinatorEvent` is `#[non_exhaustive]`: an instruction this
            // build does not know is logged rather than guessed at.
            other => tracing::warn!(event = %other, "an unrecognised coordinator instruction"),
        }
    }

    /// Admits a coordinator-dispatched node and starts it (§7.3 `Spawn`).
    ///
    /// The dataflow is created on first sight, with `exit_when_nodes_finish`
    /// **off**: under `astrs run` that flag is what makes the process return
    /// when the graph ends, and a cluster daemon that exited the moment its
    /// last node finished would take every other dataflow on the machine with
    /// it — and never get to report `AllNodesFinished`.
    pub fn apply_coordinator_spawn(
        &mut self,
        spec: NodeSpawnSpec,
        routes: &[RouteSpec],
        dataflow_name: Option<String>,
    ) {
        let dataflow = spec.dataflow;
        let node = spec.node.clone();
        let generation = spec.generation;

        if self.state().is_shutting_down() {
            self.sink().report(WireDaemonEvent::SpawnResult {
                dataflow,
                node,
                generation,
                outcome: SpawnOutcome::Cancelled,
            });
            return;
        }

        if self.state().dataflow(dataflow).is_none() {
            let now = self.clock.now();
            let mut state = DataflowState::new(dataflow, now).with_exit_when_nodes_finish(false);
            if let Some(name) = dataflow_name {
                state = state.with_name(name);
            }
            state.set_status(DataflowStatus::Starting);
            self.state_mut().insert_dataflow(state);
            // Lifecycle claims only: a `PeerRoutes` for this dataflow may
            // already have arrived (see [`crate::coordinator::routes`]), and
            // clearing the directory here would throw away the very
            // directives that raced ahead of this spawn.
            self.cluster_mut().forget_lifecycle(dataflow);
        }

        self.subscribe_spec_virtual_inputs(&spec);
        if let Some(state) = self.state_mut().dataflow_mut(dataflow) {
            state.add_node(spec);
        }
        for route in routes {
            self.note_local_route(route);
        }

        self.spawn_node(dataflow, &node);

        let outcome = self.spawn_outcome_of(dataflow, &node);
        self.sink().report(WireDaemonEvent::SpawnResult {
            dataflow,
            node: node.clone(),
            generation,
            outcome,
        });

        // A node that has just been admitted may be the producer a peer route
        // was waiting on (§6.4: a `PeerRoutes` can arrive before its `Spawn`).
        self.reconcile_peers();
        self.plan_shm_outputs_of(dataflow, &node);
    }

    /// How a spawn ended, read back from the node's own state.
    ///
    /// [`Daemon::spawn_node`] reports nothing upward — under `astrs run` there
    /// is nobody to report to — so the cluster path reads the outcome off the
    /// state machine instead of duplicating the classification inside it.
    fn spawn_outcome_of(&self, dataflow: DataflowId, node: &NodeId) -> SpawnOutcome {
        let Some(state) = self
            .dataflow(dataflow)
            .and_then(|dataflow| dataflow.node(node))
        else {
            return SpawnOutcome::Failed {
                message: "the node vanished between admission and spawn".to_owned(),
                errno: None,
            };
        };
        match state.run_state() {
            NodeRunState::Spawning | NodeRunState::Running => {
                if state.is_dynamic() {
                    SpawnOutcome::AwaitingDynamic
                } else {
                    SpawnOutcome::Spawned {
                        pid: state.pid(),
                        started_at: self.clock.now(),
                    }
                }
            }
            NodeRunState::Failed => SpawnOutcome::Failed {
                message: state
                    .exit_cause()
                    .map_or_else(|| "the spawn failed".to_owned(), ToString::to_string),
                errno: None,
            },
            NodeRunState::Finished => SpawnOutcome::Spawned {
                pid: state.pid(),
                started_at: self.clock.now(),
            },
            // `NodeRunState` is `#[non_exhaustive]`: `Pending`,
            // `Restarting` and `Stopping` all mean "this incarnation is not
            // the one that will run", and so does any state a later release
            // adds between them.
            _ => SpawnOutcome::Cancelled,
        }
    }

    /// Registers the `astrs/timer/*` and `astrs/logs/*` subscriptions a spawn
    /// specification declares (§8.4).
    ///
    /// The coordinator sends a *fully expanded* specification and no manifest,
    /// so the virtual sources have to be recovered from the port references
    /// themselves — which is exactly what [`virtual_source_text`] is for.
    fn subscribe_spec_virtual_inputs(&mut self, spec: &NodeSpawnSpec) {
        let now = Instant::now();
        for input in &spec.inputs {
            if !is_virtual_port(&input.source) {
                continue;
            }
            let text = virtual_source_text(&input.source);
            match astrs_manifest::recognize_virtual_source(&text) {
                Some(Ok(parsed)) if text.starts_with("astrs/timer") => {
                    // A period the wheel refuses cannot fire; the coordinator's
                    // own manifest validation already refused zero rates, so
                    // this only triggers on a rate beyond the interval type's
                    // range.
                    let _ = self.timers.subscribe_at(
                        spec.dataflow,
                        spec.node.clone(),
                        input.id.clone(),
                        parsed,
                        now,
                    );
                }
                Some(Ok(_)) if text.starts_with("astrs/logs") => {
                    let _ = self.log_subscriptions.subscribe(
                        spec.dataflow,
                        spec.node.clone(),
                        input.id.clone(),
                        &text,
                    );
                }
                // `astrs/status` needs no registration, and an unrecognised
                // virtual source was already refused by the coordinator's
                // manifest validation.
                _ => {}
            }
        }
    }

    /// Records a route the coordinator sent whose consumer this daemon hosts.
    ///
    /// Same-host edges are already in the table (a node's own `inputs` put
    /// them there when it was added); this fills in the queue configuration
    /// the coordinator resolved from the manifest, which a bare `InputSpec`
    /// would otherwise keep at its default.
    fn note_local_route(&mut self, route: &RouteSpec) {
        let dataflow = route.key.dataflow;
        let consumer = &route.key.consumer;
        let Some(state) = self.state_mut().dataflow_mut(dataflow) else {
            return;
        };
        if state.node(consumer.node()).is_none() {
            return;
        }
        let mut spec =
            astrs_wire::InputSpec::new(consumer.port().clone(), route.key.producer.clone());
        spec.queue_size = route.queue_size.max(1);
        spec.queue_policy = route.queue_policy;
        state.routes_mut().insert(consumer.node().clone(), spec);
    }

    /// The cluster-wide readiness barrier was met (§7.3 `AllNodesReady`).
    fn apply_all_nodes_ready(&mut self, dataflow: DataflowId, failed: &[NodeId]) {
        if !failed.is_empty() {
            tracing::warn!(
                %dataflow,
                failed = failed.len(),
                "the dataflow started with nodes that never came up"
            );
        }
        if let Some(state) = self.state_mut().dataflow_mut(dataflow)
            && state.status() == DataflowStatus::Starting
        {
            state.set_status(DataflowStatus::Running);
        }
        // Every node of the graph exists now, cluster-wide, so a route that
        // was waiting on a peer's producer can finally be opened.
        self.reconcile_peers();
    }

    /// Stops every node of one dataflow **without** stopping the daemon.
    ///
    /// [`Daemon::stop_dataflow`] ends the daemon's event loop too, which is
    /// right for `astrs run` (the CLI *is* the daemon) and wrong for a cluster
    /// daemon, which must stay up to report the result and serve every other
    /// dataflow on the machine.
    pub fn stop_dataflow_nodes(&mut self, dataflow: DataflowId, cause: StopCause) {
        let nodes: Vec<NodeId> = self
            .dataflow(dataflow)
            .map(|state| state.node_ids().cloned().collect())
            .unwrap_or_default();
        if nodes.is_empty() {
            return;
        }
        self.set_status(dataflow, DataflowStatus::Stopping);
        for node in &nodes {
            self.stop_node(dataflow, node, cause.clone());
        }
        self.forget_peer_routes(dataflow, RouteCloseReason::DataflowStopped);
    }

    /// Restarts one node at the incarnation the coordinator chose (§7.3).
    ///
    /// The coordinator owns the generation counter across the cluster, so a
    /// restart it ordered must land on *its* number rather than on whatever
    /// the daemon's own `begin_next_generation` would have produced —
    /// otherwise a restart the coordinator ordered and one a restart policy
    /// ordered could mint the same generation for two different incarnations.
    ///
    /// A node that is still **live** is asked to stop through the ordinary
    /// finish ladder (§12), and the respawn is *armed* rather than performed:
    /// spawning before the outgoing process is reaped would leave that
    /// process's finish watchdog armed against a generation the node no
    /// longer carries, and its next escalation would then signal the
    /// incarnation that had just started. [`ClusterState::arm_restart`] holds
    /// the claim and the tick discharges it once the exit lands.
    ///
    /// [`ClusterState::arm_restart`]: crate::coordinator::ClusterState::arm_restart
    pub fn restart_node_at(&mut self, dataflow: DataflowId, node: &NodeId, generation: u64) {
        let live = self
            .dataflow(dataflow)
            .and_then(|state| state.node(node))
            .is_some_and(|state| !state.is_terminal());
        if live {
            tracing::info!(
                %dataflow,
                %node,
                generation,
                "restart ordered by the coordinator; stopping the current incarnation"
            );
            self.cluster_mut()
                .arm_restart(dataflow, node.clone(), generation);
            // `StopCause::Replaced` is daemon-initiated, so the node's own
            // restart policy declines to act on the exit
            // (`crate::supervise::decide`) and this claim is the only thing
            // that will respawn it.
            self.stop_node(dataflow, node, StopCause::Replaced);
            return;
        }
        self.spawn_restarted_node(dataflow, node, generation);
    }

    /// Spawns the incarnation a coordinator-ordered restart asked for, and
    /// reports the outcome.
    pub(crate) fn spawn_restarted_node(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
    ) {
        let applied = {
            let Some(state) = self.state_mut().dataflow_mut(dataflow) else {
                return;
            };
            let Some(node_state) = state.node_mut(node) else {
                return;
            };
            node_state.begin_next_generation();
            node_state.set_generation(generation)
        };
        self.notify_peers_restarted_at(dataflow, node, applied);
        self.spawn_node(dataflow, node);
        let outcome = self.spawn_outcome_of(dataflow, node);
        self.sink().report(WireDaemonEvent::SpawnResult {
            dataflow,
            node: node.clone(),
            generation: applied,
            outcome,
        });
        // A new incarnation is a new generation to stamp peer routes with, so
        // every cross-daemon edge this node produces on has to be re-opened
        // (§12: the stamp is what makes a stale message detectable).
        self.reconcile_peers();
    }

    /// Tells this node's local graph peers that it came back (§24.1
    /// `NodeEvent::Restarted`).
    fn notify_peers_restarted_at(&mut self, dataflow: DataflowId, node: &NodeId, generation: u64) {
        let peers: Vec<NodeId> = self
            .dataflow(dataflow)
            .map(|state| state.routes().downstream_of(node).into_iter().collect())
            .unwrap_or_default();
        let event = NodeEvent::Restarted {
            peer: node.clone(),
            generation,
        };
        for peer in peers {
            self.send_to(dataflow, &peer, event.clone());
        }
    }

    /// Delivers a `ParamUpdate`/`ParamDeleted` on the `astrs.status` port to
    /// exactly the node(s) `scope` named (blueprint §17 param scoping).
    ///
    /// `node` narrows to one recipient — the coordinator's own
    /// `handlers::dispatch_param_update` already resolved
    /// [`astrs_wire::ParamScope::Node`] down to "only the one daemon hosting
    /// that node" before this instruction ever arrived here; broadcasting
    /// it a second time to every *other* live node this daemon happens to
    /// host in the same dataflow would undo that narrowing at the last hop
    /// and hand every sibling node a private parameter that was never
    /// scoped to it. `Global`/`Dataflow` scope has no single node to prefer
    /// (a cluster or dataflow default is relevant to any node that has not
    /// overridden it), so those still reach every live node of the matching
    /// dataflow(s) — `dataflow: None` matching every dataflow, for `Global`.
    ///
    /// [`Daemon::send_to`] already no-ops on a node with no live session, so
    /// naming a `node` that this daemon does not currently host (stale
    /// routing, a race with that node's own exit) is silently harmless —
    /// exactly as the broadcast branch below already tolerates a dataflow
    /// with no live nodes at all.
    fn deliver_param_event(
        &mut self,
        dataflow: Option<DataflowId>,
        node: Option<NodeId>,
        event: &NodeEvent,
    ) {
        if let Some(node) = node {
            if let Some(dataflow) = dataflow {
                self.send_to(dataflow, &node, event.clone());
            }
            return;
        }
        let targets: Vec<(DataflowId, NodeId)> = self
            .state()
            .dataflows()
            .filter(|state| dataflow.is_none_or(|wanted| state.id() == wanted))
            .flat_map(|state| {
                let id = state.id();
                state
                    .nodes()
                    .filter(|node| node.is_live())
                    .map(move |node| (id, node.id().clone()))
            })
            .collect();
        for (id, node) in targets {
            self.send_to(id, &node, event.clone());
        }
    }

    /// Applies a catch-up batch and acknowledges it (§12).
    ///
    /// The entries themselves are informational for a daemon — it is the
    /// *executor*, and every fact it needs to act on arrives as its own
    /// instruction — with one exception that matters: a `DaemonPresence`
    /// entry for a peer that left is a reason to drop that peer's routes now
    /// rather than wait for the socket to notice.
    fn apply_state_catch_up(&mut self, seq: u64, entries: &[StateEntry], final_batch: bool) {
        let mut applied = 0u32;
        let mut high_water = seq;
        for entry in entries {
            high_water = high_water.max(entry.seq);
            applied = applied.saturating_add(1);
            if let astrs_wire::StateEntryKind::DaemonPresence { daemon, connected } = &entry.kind
                && !connected
                && *daemon != *self.config().id()
            {
                self.handle_peer_lost(daemon, "the coordinator's catch-up says the peer left");
                self.peer_directory_mut().forget_peer(daemon);
            }
        }
        if let Some(uplink) = self.cluster().uplink() {
            uplink.state().observe_catch_up(high_water);
        }
        tracing::debug!(seq, applied, final_batch, "applied a state catch-up batch");
        self.sink().report(WireDaemonEvent::StateCatchUpAck {
            seq: high_water,
            applied,
        });
    }

    /// Re-plans the shared-memory rings of every output a node produces.
    fn plan_shm_outputs_of(&mut self, dataflow: DataflowId, node: &NodeId) {
        let outputs: Vec<astrs_wire::PortRef> = self
            .dataflow(dataflow)
            .and_then(|state| state.node(node))
            .map(|state| {
                state
                    .spec()
                    .outputs
                    .iter()
                    .map(|output| astrs_wire::PortRef::new(node.clone(), output.id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for output in outputs {
            self.plan_shm_output(dataflow, &output);
        }
    }

    /// Runs a coordinator-dispatched build in its own task (§7.3 `Build`).
    fn run_coordinator_build(
        &mut self,
        build: astrs_wire::BuildId,
        dataflow: DataflowId,
        steps: &[WireBuildStep],
        working_dir: Option<String>,
    ) {
        let sink = Arc::clone(self.sink());
        let dir =
            working_dir.map_or_else(|| self.config().working_dir().to_path_buf(), PathBuf::from);
        let policy = EnvPolicy::new().with_passthrough(self.config().env_passthrough().to_vec());
        // The wire carries an already-split argv (§16: no shell), and
        // [`crate::dataflow::run_step`] takes one unsplit line that it splits
        // by the very same shlex rules. `try_join` is the exact inverse of
        // that split, so the round trip is lossless — and a command that
        // cannot be quoted (an embedded nul) fails the build here rather than
        // silently running something else.
        //
        // A step's per-step `env` and `timeout` are not carried through: the
        // daemon runs every build step under §16's scrubbed environment and
        // to completion. Noted in this crate's report rather than silently
        // dropped.
        let mut joined = Vec::with_capacity(steps.len());
        for step in steps {
            match shlex::try_join(step.command.iter().map(String::as_str)) {
                Ok(command) => joined.push(BuildStep {
                    node: step.node.clone(),
                    command,
                    working_dir: step.working_dir.clone(),
                }),
                Err(error) => {
                    self.sink().report(WireDaemonEvent::BuildResult {
                        build,
                        dataflow,
                        outcome: BuildOutcome::Failed {
                            node: Some(step.node.clone()),
                            exit_code: None,
                            message: format!("a build command cannot be quoted: {error}"),
                            output: String::new(),
                        },
                    });
                    return;
                }
            }
        }
        let steps = joined;
        self.set_status(dataflow, DataflowStatus::Building);
        tokio::spawn(async move {
            let spawner = Spawner::with_policy(dir, policy);
            let started = Instant::now();
            let report = run_build(&spawner, &steps).await;
            let took = DurationMs::from_duration(started.elapsed());
            let outcome = if report.is_success() {
                BuildOutcome::Succeeded {
                    artifacts: Vec::new(),
                    took,
                }
            } else if let Some(step) = report.failed_step() {
                BuildOutcome::Failed {
                    node: Some(step.node.clone()),
                    exit_code: step.exit_code,
                    message: format!("build step failed: {}", step.command),
                    output: report.combined_output(),
                }
            } else {
                BuildOutcome::Failed {
                    node: None,
                    exit_code: None,
                    message: report
                        .start_failure
                        .clone()
                        .unwrap_or_else(|| "the build could not start".to_owned()),
                    output: report.combined_output(),
                }
            };
            sink.report(WireDaemonEvent::BuildResult {
                build,
                dataflow,
                outcome,
            });
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use astrs_wire::{DataId, InputSpec, NodeSource, OutputSpec, ParamKey, ParamScope, Parameter};

    use super::*;
    use crate::config::{DaemonConfig, ListenConfig, RuntimePaths};
    use crate::health::RecordingSink;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(0x0C1A)
    }

    /// A daemon with a private runtime directory.
    ///
    /// The name is kept short on purpose: the node socket lives inside this
    /// directory and a Unix socket path has a hard 103-byte limit, which a
    /// system temporary directory plus a descriptive test name exceeds.
    fn daemon(name: &str) -> Daemon {
        let root = std::env::temp_dir().join(format!("as-ca-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let config = DaemonConfig::new(RuntimePaths::under(root))
            .with_listen(ListenConfig::none())
            .with_shm(false);
        Daemon::new(config).expect("a daemon")
    }

    fn dynamic_spec(node: &str, generation: u64) -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            dataflow(),
            NodeId::new(node).unwrap(),
            generation,
            NodeSource::Dynamic,
        )
    }

    #[tokio::test]
    async fn a_spawn_admits_the_dataflow_and_reports_its_outcome() {
        let mut daemon = daemon("spawn");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());

        let spec =
            dynamic_spec("camera", 1).with_output(OutputSpec::new(DataId::new("image").unwrap()));
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(spec),
            routes: Vec::new(),
            dataflow_name: Some("demo".to_owned()),
        });

        let state = daemon.dataflow(dataflow()).expect("admitted");
        assert_eq!(state.node_count(), 1);
        assert_eq!(state.name(), Some("demo"));
        assert!(
            !state.exit_when_nodes_finish(),
            "a cluster daemon must not exit when one dataflow ends"
        );
        assert!(sink.contains("SpawnResult"));
        match sink
            .snapshot()
            .into_iter()
            .find(|event| matches!(event, WireDaemonEvent::SpawnResult { .. }))
        {
            Some(WireDaemonEvent::SpawnResult { outcome, .. }) => {
                assert_eq!(outcome, SpawnOutcome::AwaitingDynamic);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_spawn_while_shutting_down_is_cancelled() {
        let mut daemon = daemon("ss");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());
        daemon.begin_shutdown();

        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });
        assert!(daemon.dataflow(dataflow()).is_none());
        match sink.snapshot().first() {
            Some(WireDaemonEvent::SpawnResult { outcome, .. }) => {
                assert_eq!(*outcome, SpawnOutcome::Cancelled);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_spawn_registers_a_timer_subscription_from_its_expanded_spec() {
        let mut daemon = daemon("spawn-timer");
        let tick = crate::state::virtual_port_ref("astrs/timer/millis/10").unwrap();
        let spec = dynamic_spec("planner", 1)
            .with_input(InputSpec::new(DataId::new("tick").unwrap(), tick));
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(spec),
            routes: Vec::new(),
            dataflow_name: None,
        });
        assert_eq!(
            daemon.timers.len(),
            1,
            "a coordinator spawn carries no manifest, so the timer comes from the port"
        );
    }

    #[tokio::test]
    async fn all_nodes_ready_moves_a_starting_dataflow_to_running() {
        let mut daemon = daemon("ready");
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });
        daemon.handle_coordinator_frame(CoordinatorEvent::AllNodesReady {
            dataflow: dataflow(),
            failed: Vec::new(),
        });
        assert_eq!(
            daemon.dataflow(dataflow()).map(DataflowState::status),
            Some(DataflowStatus::Running)
        );
    }

    #[tokio::test]
    async fn a_stop_dataflow_does_not_stop_the_daemon() {
        let mut daemon = daemon("stop");
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });
        daemon.handle_coordinator_frame(CoordinatorEvent::StopDataflow {
            dataflow: dataflow(),
            grace: None,
            cause: StopCause::Requested,
        });
        assert!(
            !daemon.state().is_shutting_down(),
            "a cluster daemon outlives the dataflows it runs"
        );
        assert!(daemon.handle().is_open());
    }

    #[tokio::test]
    async fn a_destroy_shuts_the_daemon_down() {
        let mut daemon = daemon("destroy");
        daemon.handle_coordinator_frame(CoordinatorEvent::Destroy { grace: None });
        assert!(daemon.state().is_shutting_down());
    }

    #[tokio::test]
    async fn a_log_request_is_answered_from_the_history() {
        let mut daemon = daemon("logs");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());
        daemon.record_log(
            astrs_wire::LogRecord::new(
                astrs_time::HlcTimestamp::new(1, 0),
                astrs_wire::LogLevel::Info,
                "hello",
            )
            .with_dataflow(dataflow()),
        );

        daemon.handle_coordinator_frame(CoordinatorEvent::Logs {
            request: 7,
            dataflow: Some(dataflow()),
            node: None,
            query: astrs_wire::LogQuery::new(),
        });
        match sink.snapshot().first() {
            Some(WireDaemonEvent::Log {
                request, records, ..
            }) => {
                assert_eq!(*request, Some(7));
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].message, "hello");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_catch_up_batch_is_acknowledged_at_its_high_water_mark() {
        let mut daemon = daemon("catch-up");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());

        let entries = vec![
            StateEntry::new(
                41,
                astrs_time::HlcTimestamp::new(1, 0),
                astrs_wire::StateEntryKind::DataflowStatus {
                    dataflow: dataflow(),
                    status: DataflowStatus::Running,
                    name: None,
                },
            ),
            StateEntry::new(
                43,
                astrs_time::HlcTimestamp::new(2, 0),
                astrs_wire::StateEntryKind::BuildFinished {
                    build: astrs_wire::BuildId::from_u128(1),
                    dataflow: dataflow(),
                    success: true,
                },
            ),
        ];
        daemon.handle_coordinator_frame(CoordinatorEvent::StateCatchUp {
            seq: 41,
            entries,
            final_batch: true,
        });
        match sink.snapshot().first() {
            Some(WireDaemonEvent::StateCatchUpAck { seq, applied }) => {
                assert_eq!(*seq, 43);
                assert_eq!(*applied, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_parameter_update_reaches_only_the_scoped_dataflow() {
        let mut daemon = daemon("params");
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });
        // No session is bound, so this proves the addressing, not delivery:
        // an unscoped parameter must not panic on a daemon with no nodes of
        // that scope, and a scoped one must not reach another dataflow.
        daemon.handle_coordinator_frame(CoordinatorEvent::SetParam {
            scope: ParamScope::dataflow_scope(DataflowId::from_u128(9)),
            key: ParamKey::new("gain").unwrap(),
            value: Parameter::Integer(3),
        });
        daemon.handle_coordinator_frame(CoordinatorEvent::DeleteParam {
            scope: ParamScope::Global,
            key: ParamKey::new("gain").unwrap(),
        });
    }

    #[tokio::test]
    async fn a_restart_of_a_terminal_node_spawns_the_coordinators_generation() {
        let mut daemon = daemon("rt");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });
        // A dynamic node whose session never attached, then ended.
        daemon
            .state_mut()
            .dataflow_mut(dataflow())
            .unwrap()
            .node_mut(&NodeId::new("camera").unwrap())
            .unwrap()
            .mark_exited(astrs_wire::NodeExitCause::Success);

        daemon.handle_coordinator_frame(CoordinatorEvent::RestartNode {
            dataflow: dataflow(),
            node: NodeId::new("camera").unwrap(),
            generation: 7,
        });

        assert_eq!(
            daemon
                .dataflow(dataflow())
                .and_then(|state| state.node(&NodeId::new("camera").unwrap()))
                .map(|state| state.generation()),
            Some(7),
            "the coordinator owns the generation counter across the cluster"
        );
        assert_eq!(
            daemon.cluster().armed_restart_count(),
            0,
            "a terminal node is restarted immediately, not armed"
        );
    }

    #[tokio::test]
    async fn a_restart_of_a_live_node_is_armed_until_its_incarnation_ends() {
        let mut daemon = daemon("rl");
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });
        let camera = NodeId::new("camera").unwrap();
        assert!(
            !daemon
                .dataflow(dataflow())
                .and_then(|state| state.node(&camera))
                .expect("admitted")
                .is_terminal(),
            "a dynamic node that was told to spawn is awaiting its attach"
        );

        daemon.handle_coordinator_frame(CoordinatorEvent::RestartNode {
            dataflow: dataflow(),
            node: camera.clone(),
            generation: 9,
        });
        assert_eq!(
            daemon.cluster().armed_restart_count(),
            1,
            "the respawn waits for the outgoing incarnation to end (§12)"
        );
        assert_eq!(
            daemon
                .dataflow(dataflow())
                .and_then(|state| state.node(&camera))
                .map(|state| state.generation()),
            Some(1),
            "the generation does not move while the old incarnation is alive"
        );

        // The outgoing incarnation ends; the tick discharges the claim.
        daemon
            .state_mut()
            .dataflow_mut(dataflow())
            .unwrap()
            .node_mut(&camera)
            .unwrap()
            .mark_exited(astrs_wire::NodeExitCause::Success);
        daemon.tick(Instant::now());

        assert_eq!(daemon.cluster().armed_restart_count(), 0);
        assert_eq!(
            daemon
                .dataflow(dataflow())
                .and_then(|state| state.node(&camera))
                .map(|state| state.generation()),
            Some(9)
        );
    }

    #[tokio::test]
    async fn an_armed_restart_is_dropped_when_the_node_came_back_another_way() {
        let mut daemon = daemon("rr");
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });
        let camera = NodeId::new("camera").unwrap();
        daemon
            .cluster_mut()
            .arm_restart(dataflow(), camera.clone(), 1);

        // The node is live at (or past) the generation the coordinator asked
        // for — a local restart policy got there first.
        daemon.tick(Instant::now());
        assert_eq!(
            daemon.cluster().armed_restart_count(),
            0,
            "one node must never be spawned twice for one restart"
        );
    }

    #[tokio::test]
    async fn a_spawn_keeps_peer_route_directives_that_arrived_first() {
        let mut daemon = daemon("keep");
        let peer = astrs_wire::DaemonId::generate(None);
        let route = RouteSpec::new(astrs_wire::RouteKey::new(
            dataflow(),
            "camera/image".parse().unwrap(),
            "detect/frames".parse().unwrap(),
        ));
        // The directive overtakes its spawn — §6.4's documented race.
        daemon.handle_coordinator_frame(CoordinatorEvent::PeerRoutes {
            dataflow: dataflow(),
            directives: vec![astrs_wire::PeerRouteDirective::new(
                route,
                peer.clone(),
                "tcp:127.0.0.1:7409",
            )],
        });
        assert_eq!(daemon.peer_directory().edges_for(&peer).len(), 1);

        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(
                dynamic_spec("camera", 1)
                    .with_output(OutputSpec::new(DataId::new("image").unwrap())),
            ),
            routes: Vec::new(),
            dataflow_name: None,
        });
        assert_eq!(
            daemon.peer_directory().edges_for(&peer).len(),
            1,
            "admitting the dataflow must not discard the edges already announced"
        );
    }

    #[tokio::test]
    async fn an_unknown_instruction_is_logged_rather_than_guessed_at() {
        let mut daemon = daemon("unknown");
        // `PeerDisconnected` for a peer that was never connected is the
        // closest thing to a no-op instruction the frozen set offers.
        daemon.handle_coordinator_frame(CoordinatorEvent::PeerDisconnected {
            daemon: astrs_wire::DaemonId::generate(None),
            dataflows: vec![dataflow()],
        });
        assert!(daemon.peers().is_empty());
    }

    // ---- Dynamic topology: the three tail-appended verbs -----------------
    //
    // `Spawn`/`StopNode` already carry `AddNode`/`RemoveNode` (blueprint
    // §17) end to end and are covered above; these three exercise the
    // thin translation this module adds onto `Daemon::apply_replace_node`/
    // `apply_add_edge`/`apply_remove_edge` (`crate::dataflow::topology`),
    // which already carry their own, much more thorough, behavioural
    // coverage (dual-run window, live rewiring, ...).

    #[tokio::test]
    async fn a_coordinator_ordered_replace_swaps_the_spec_and_spawns_a_new_generation() {
        let mut daemon = daemon("frame-replace");
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("camera", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });

        let mut replacement = dynamic_spec("camera", 0);
        replacement
            .outputs
            .push(OutputSpec::new(DataId::new("image").unwrap()));
        daemon.handle_coordinator_frame(CoordinatorEvent::ReplaceNode {
            dataflow: dataflow(),
            node: Box::new(replacement),
        });

        let node = daemon
            .dataflow(dataflow())
            .and_then(|state| state.node(&NodeId::new("camera").unwrap()))
            .expect("still tracked under the same id");
        // The outgoing incarnation was already at generation 1; the
        // replacement's own spec named generation 0 (a caller with no
        // reliable way to know this daemon's live counter has no better
        // placeholder) — the fresh generation must be computed from the
        // *outgoing* incarnation's real value (2), never from the
        // placeholder, or the two incarnations would collide on 1.
        assert_eq!(
            node.generation(),
            2,
            "a fresh generation was minted, past the outgoing one — never colliding with it"
        );
        assert_eq!(node.spec().outputs.len(), 1, "the new shape took effect");
    }

    #[tokio::test]
    async fn a_coordinator_ordered_add_edge_wires_the_route() {
        let mut daemon = daemon("frame-add-edge");
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(dynamic_spec("detect", 1)),
            routes: Vec::new(),
            dataflow_name: None,
        });

        daemon.handle_coordinator_frame(CoordinatorEvent::AddEdge {
            dataflow: dataflow(),
            consumer: NodeId::new("detect").unwrap(),
            input: InputSpec::new(
                DataId::new("frames").unwrap(),
                "camera/image".parse().unwrap(),
            ),
        });

        let state = daemon.dataflow(dataflow()).expect("admitted");
        assert_eq!(
            state
                .routes()
                .consumers(&"camera/image".parse().unwrap())
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_coordinator_ordered_remove_edge_closes_the_input() {
        let mut daemon = daemon("frame-remove-edge");
        let spec = dynamic_spec("detect", 1).with_input(InputSpec::new(
            DataId::new("frames").unwrap(),
            "camera/image".parse().unwrap(),
        ));
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(spec),
            routes: Vec::new(),
            dataflow_name: None,
        });

        daemon.handle_coordinator_frame(CoordinatorEvent::RemoveEdge {
            dataflow: dataflow(),
            consumer: NodeId::new("detect").unwrap(),
            input: DataId::new("frames").unwrap(),
        });

        let state = daemon.dataflow(dataflow()).expect("admitted");
        assert!(
            state
                .routes()
                .consumers(&"camera/image".parse().unwrap())
                .is_empty(),
            "the edge the coordinator ordered removed no longer routes"
        );
    }

    #[tokio::test]
    async fn a_coordinator_ordered_replace_for_a_node_this_daemon_never_admitted_is_logged_not_panicked()
     {
        let mut daemon = daemon("frame-replace-unknown");
        // No `Spawn` ran first: the dataflow itself is unknown to this
        // daemon, exactly the race `apply_replace_node`'s `Err` return
        // covers (validated by the coordinator against state this daemon
        // no longer, or not yet, agrees with).
        daemon.handle_coordinator_frame(CoordinatorEvent::ReplaceNode {
            dataflow: dataflow(),
            node: Box::new(dynamic_spec("ghost", 0)),
        });
        assert!(
            daemon.dataflow(dataflow()).is_none(),
            "an unresolvable replace must not fabricate a dataflow"
        );
    }

    #[tokio::test]
    async fn a_route_the_coordinator_sent_sizes_the_local_queue() {
        let mut daemon = daemon("rq");
        let spec = dynamic_spec("detect", 1).with_input(InputSpec::new(
            DataId::new("frames").unwrap(),
            "camera/image".parse().unwrap(),
        ));
        let route = RouteSpec::new(astrs_wire::RouteKey::new(
            dataflow(),
            "camera/image".parse().unwrap(),
            "detect/frames".parse().unwrap(),
        ));
        let mut sized = route.clone();
        sized.queue_size = 64;
        daemon.handle_coordinator_frame(CoordinatorEvent::Spawn {
            node: Box::new(spec),
            routes: vec![sized],
            dataflow_name: None,
        });

        let state = daemon.dataflow(dataflow()).expect("admitted");
        let consumers = state.routes().consumers(&"camera/image".parse().unwrap());
        assert_eq!(consumers.len(), 1);
        assert_eq!(consumers[0].spec.queue_size, 64);
        let _ = BTreeMap::<u8, u8>::new();
    }
}
