//! [`Daemon`] — the merged event loop (§4.3).
//!
//! > *Every process runs one **merged event loop** (tokio `select!` over typed
//! > streams — no `futures-concurrency` dependency): peer connections, local
//! > node messages, timers, and internal channels. Every event is
//! > `Stamped<T>` — an HLC timestamp from `astrs-time`.*
//!
//! This is that loop. One task, one `select!`, four arms, one owner of every
//! mutable fact:
//!
//! ```text
//!   ┌──────────────────── select! ────────────────────┐
//!   │ listeners.accept()   → a node connected         │
//!   │ events.recv()        → a request, exit, log,    │
//!   │                        restart, or shutdown     │
//!   │ timers.next_deadline → an astrs/timer/* tick    │
//!   │ watchdogs/deadlines  → an escalation is due     │
//!   └───────────────────────┬─────────────────────────┘
//!                           ▼
//!            clock.stamp(event)  →  Stamped<T>, HLC-ordered
//!                           ▼
//!             one &mut DaemonState, one task, no locks
//! ```
//!
//! # Why one task owns everything
//!
//! Because the alternative is a lock around [`crate::state::DaemonState`], and
//! every interesting operation touches three parts of it at once: a node
//! exiting must consult the restart policy, close its outputs, notify its
//! graph peers and reclaim its extension entries — atomically, or a peer sees
//! `InputClosed` for a node that the very next event says is running again.
//! Holding a lock across all of that is the same thing as a single-threaded
//! loop, minus the guarantee. So: one loop, and everything that must block
//! (sockets, `wait(2)`, pipes, backoffs) lives in a task that reports back
//! through [`crate::session::DaemonHandle`].
//!
//! # HLC stamping
//!
//! Every event is stamped on *receipt*, not on production: the producing task
//! has no clock of its own, and stamping at the single point of serialization
//! is what makes the resulting order a causal order (§4.3, §14).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use astrs_time::{HlcClock, Stamped, SystemClock};
use astrs_wire::{
    DataId, DataflowId, DataflowResult, DurationMs, NodeEvent, NodeExitCause, NodeId, NodeRunState,
    RouteCloseReason, SessionId, StopCause,
};

use crate::config::DaemonConfig;
use crate::dataflow::plan::DataflowPlan;
use crate::error::{DaemonError, DaemonResult};
use crate::health::{
    HealthTable, HeartbeatProducer, NodeIoLedger, NodeMetricsCollector, NullSink, ReportSink,
};
use crate::local::{LogSubscriptions, NodeMailbox, ReplaySource, TimerRegistry, status_port};
use crate::metrics::DaemonMetrics;
use crate::peer::PeerManager;
use crate::server::listener::{
    AcceptedNode, ConnectionOrigin, NodeListeners, SessionMinter, credentials_acceptable,
    daemon_uid,
};
use crate::session::{
    DaemonEvent, DaemonEvents, DaemonHandle, SessionActor, SessionSink, event_channel,
};
use crate::shm::{ShmPlane, ShmPolicy};
use crate::spawn::{EnvPolicy, Spawner};
use crate::state::{DaemonState, DataflowState};
use crate::supervise::{DeadlineTable, FinishWatchdogSet};
use crate::tap::TapRegistry;

/// How long the loop sleeps when it has no deadline at all.
///
/// Not a poll interval — the loop is fully event-driven — but a ceiling, so a
/// daemon with nothing scheduled still wakes often enough to notice a clock
/// jump or a configuration change.
pub const IDLE_TICK: Duration = Duration::from_millis(250);

/// How long a stopping node has between `SIGTERM` and `SIGKILL`.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// How long an exited node's session has to finish delivering what it already
/// sent, before its outputs are closed anyway.
///
/// The bounded half of the rendezvous in `Daemon::handle_process_exit` (an
/// internal method, so this names it rather than linking to it). A
/// session actor reports `SessionClosed` on every exit path, so this expiring
/// means something is wrong (a wedged actor, a socket that never reached EOF)
/// — and a graph that never finishes would be a worse failure than the lost
/// tail this wait exists to prevent. Generous relative to the work: the
/// frames are already in the socket buffer, so the wait is a scheduling
/// hiccup, never a transfer.
pub const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How long a teardown closure waits for the reap that names its reason,
/// before consumers are told the optimistic answer anyway (§12).
///
/// The other half of [`DRAIN_GRACE`]'s rendezvous. A node's `CloseOutputs` is
/// the protocol's "what a node does as it exits" (§7.3), so the process that
/// sent it is expected within a scheduling hiccup — `Node::shutdown` queues
/// the frame and ends the session in the same breath. Waiting is what lets
/// the closure carry `ProducerCrashed` when the exit turns out to be
/// non-zero; this bounds the wait for the case the protocol does not promise,
/// a node that announces its exit and then keeps running. On expiry the
/// closure is released as `ProducerFinished`, which is the truth as far as it
/// is known: a process that is still alive has not crashed.
pub const TEARDOWN_CLOSE_GRACE: Duration = Duration::from_secs(2);

/// How long a shutting-down daemon spends trying to get its
/// `DaemonEvent::Exit` onto the coordinator socket.
///
/// Short on purpose. The goodbye is a courtesy — the coordinator's own
/// heartbeat watchdog declares a silent daemon lost either way (§12) — so a
/// coordinator that has already gone must not add a visible pause to every
/// `astrs down`.
pub const GOODBYE_GRACE: Duration = Duration::from_millis(250);

/// How often the goodbye drain re-checks the outbox.
const GOODBYE_POLL: Duration = Duration::from_millis(1);

/// The daemon's local core.
pub struct Daemon {
    /// The configuration everything reads from.
    pub(crate) config: DaemonConfig,
    /// Everything the daemon knows.
    pub(crate) state: DaemonState,
    /// The hybrid logical clock every event is stamped with (§4.3).
    pub(crate) clock: HlcClock<SystemClock>,
    /// The process spawner.
    pub(crate) spawner: Spawner,
    /// One mailbox per registered node, keyed by dataflow and node.
    pub(crate) mailboxes: BTreeMap<(DataflowId, NodeId), NodeMailbox>,
    /// One outbound sink per connected session.
    pub(crate) sinks: BTreeMap<SessionId, SessionSink>,
    /// The shared `astrs/timer/*` wheel (§11.1).
    pub(crate) timers: TimerRegistry,
    /// The `astrs/logs/*` filters (§8.4).
    pub(crate) log_subscriptions: LogSubscriptions,
    /// Spawn deadlines, keyed by node (§12: dora's gap, closed).
    pub(crate) spawn_deadlines: DeadlineTable<(DataflowId, NodeId)>,
    /// The finish-straggler ladders (§12).
    pub(crate) watchdogs: FinishWatchdogSet,
    /// The internal event channel every background task reports through.
    pub(crate) handle: DaemonHandle,
    /// The receiving end of it.
    pub(crate) events: DaemonEvents,
    /// Session ids, shared with the accept tasks.
    pub(crate) sessions: SessionMinter,
    /// The listeners, once bound.
    pub(crate) listeners: Option<NodeListeners>,
    /// How many events the loop has processed, for diagnostics.
    processed: u64,
    /// One conversation state per connected session.
    pub(crate) protocols: BTreeMap<SessionId, crate::session::SessionProtocol>,
    /// The most recent refusal, for a caller inspecting a failed exchange.
    pub(crate) last_refusal: Option<(SessionId, DaemonError)>,
    /// Sessions whose `NextEvent` found an empty queue, and the batch size
    /// each asked for.
    pub(crate) pending_delivery: BTreeMap<SessionId, u32>,
    /// The shared-memory plane and its slow-start handshake (§6.2, §6.3).
    pub(crate) shm: ShmPlane,
    /// Peer daemons and their routes (§6.4).
    pub(crate) peers: PeerManager,
    /// Post-registration liveness deadlines (§12).
    pub(crate) health: HealthTable,
    /// Debug taps (§13).
    pub(crate) taps: TapRegistry,
    /// The daemon-level counters (§13).
    pub(crate) metrics: DaemonMetrics,
    /// Where everything reported upward goes (§7.3).
    pub(crate) sink: Arc<dyn ReportSink>,
    /// The coordinator heartbeat (§12).
    pub(crate) heartbeat: HeartbeatProducer,
    /// The per-node metric sampler (§13).
    pub(crate) samples: NodeMetricsCollector,
    /// Per-node, per-output message and byte totals — §13's bandwidth half
    /// (see [`crate::health::NodeIoLedger`]).
    pub(crate) io: NodeIoLedger,
    /// Exits whose consequences are waiting for the node's session to drain.
    ///
    /// See [`Daemon::handle_process_exit`]: a node's last publishes can still
    /// be unread in its socket when the waiter task reports the process gone,
    /// so closing its outputs on the exit alone loses them.
    pub(crate) draining_exits: BTreeMap<(DataflowId, NodeId), PendingExit>,
    /// The bounded wait for each of those (§24.2).
    pub(crate) drain_deadlines: DeadlineTable<(DataflowId, NodeId)>,
    /// Teardown closures whose *reason* is waiting for the reap (§12).
    ///
    /// See [`Daemon::apply_close_outputs`]: a producer that closes its
    /// outputs on the way out and then exits non-zero has crashed, and its
    /// consumers must be told so — but at the instant the closure arrives the
    /// exit status does not exist yet.
    pub(crate) deferred_closures: BTreeMap<(DataflowId, NodeId), PendingClosure>,
    /// The bounded wait for each of those ([`TEARDOWN_CLOSE_GRACE`]).
    pub(crate) closure_deadlines: DeadlineTable<(DataflowId, NodeId)>,
    /// Incarnations a dynamic-topology `ReplaceNode` (blueprint §8, §17)
    /// superseded, kept alive here only long enough to be told to stop.
    ///
    /// [`crate::state::NodeState`] holds exactly one incarnation per node:
    /// [`Daemon::apply_replace_node`] spawns the replacement *before*
    /// asking the outgoing process to leave (the brief dual-run window
    /// that lets a reliable-path consumer see no gap in delivery), which
    /// means the outgoing [`crate::spawn::ProcessHandle`] and its
    /// [`SessionId`] have already been evicted from `NodeState` by the
    /// time anything needs to signal them. This is where they wait — see
    /// [`crate::dataflow::topology`] for the whole lifecycle.
    pub(crate) superseded: BTreeMap<(DataflowId, NodeId), crate::dataflow::topology::Superseded>,
    /// Everything this daemon holds *because* it belongs to a cluster: the
    /// coordinator uplink, the peers it was told about, the log ring `astrs
    /// logs` reads (§4.2, §6.4, §12).
    ///
    /// One field rather than five, because a daemon under `astrs run` needs
    /// none of it — see [`crate::coordinator::ClusterState`].
    pub(crate) cluster: crate::coordinator::ClusterState,
    /// The recorded clock a deterministic run drives the timer wheel from
    /// (§14). `None` for an ordinary wall-clock run — see
    /// [`crate::server::replay`] for every place this is read.
    pub(crate) replay: Option<ReplaySource>,
}

/// A node's exit, held until its session has been read to the end.
#[derive(Debug, Clone)]
pub(crate) struct PendingExit {
    /// The incarnation that exited, so a restart cannot inherit this.
    pub(crate) generation: u64,
    /// Why it exited, replayed into `after_exit` unchanged.
    pub(crate) cause: NodeExitCause,
}

/// A teardown closure, held until the reap says what to call it (§12).
///
/// See [`crate::server::closure`] for the rule this implements.
#[derive(Debug, Clone)]
pub struct PendingClosure {
    /// The incarnation that closed its outputs, so a restart cannot inherit
    /// this — the same generation discipline [`PendingExit`] keeps.
    pub(crate) generation: u64,
    /// The producer ports the node closed, in the order it named them.
    pub(crate) sources: Vec<astrs_wire::PortRef>,
}

impl core::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Daemon")
            .field("id", self.config.id())
            .field("dataflows", &self.state.dataflow_count())
            .field("nodes", &self.state.node_count())
            .field("sessions", &self.sinks.len())
            .field("processed", &self.processed)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Builds a daemon from `config`, with no listeners bound yet.
    ///
    /// # Errors
    ///
    /// Anything [`DaemonConfig::prepare`] can return.
    /// [`DaemonError::Configuration`] if [`DaemonConfig::replay_recording`]
    /// names a recording that cannot be opened (§14) — a deterministic run
    /// refuses before anything else about it is built.
    pub fn new(config: DaemonConfig) -> DaemonResult<Self> {
        config.prepare()?;
        let policy = EnvPolicy::new().with_passthrough(config.env_passthrough().to_vec());
        // Built ahead of `spawner` (rather than inline in the struct literal
        // below, where it used to live) so a `cpu_affinity` request this
        // platform cannot honor (§11.3) has somewhere to register its
        // counter — `DaemonMetrics` is cheap to clone (every series it holds
        // is already `Arc`-backed), so the daemon's own copy below and the
        // spawner's are the same registry, not two.
        let metrics = DaemonMetrics::new();
        let spawner = Spawner::with_policy(config.working_dir().to_path_buf(), policy)
            .with_metrics(metrics.clone());
        let (handle, events) = event_channel();
        // Shared with `timers` below: the wheel's own epoch and the replay
        // timeline's origin must be the same instant, or `virtual_now()`
        // (frozen here until the recording is released) could sit *before*
        // the wheel's epoch and panic the first `Instant` subtraction that
        // compares them.
        let now = Instant::now();
        let replay = match config.replay_recording() {
            Some((path, speed)) => {
                Some(ReplaySource::open(path, speed, now).map_err(|source| {
                    DaemonError::Configuration(format!(
                        "--from-recording {}: {source}",
                        path.display()
                    ))
                })?)
            }
            None => None,
        };
        let shm = if config.shm_enabled() {
            ShmPlane::bind_or_disabled(
                config.paths().shm_socket_path(),
                ShmPolicy::new().with_zero_copy_threshold(config.zero_copy_threshold()),
            )
        } else {
            ShmPlane::disabled()
        };
        let peers = PeerManager::new(config.id().clone(), config.peer().clone());
        let health = match config.default_health_timeout() {
            Some(timeout) => HealthTable::with_default_timeout(timeout),
            None => HealthTable::new(),
        };
        let heartbeat = HeartbeatProducer::new(config.heartbeat_interval(), now);
        let samples = NodeMetricsCollector::new(config.metrics_interval(), now);
        Ok(Self {
            state: DaemonState::new(),
            io: NodeIoLedger::new(),
            clock: HlcClock::new(SystemClock),
            spawner,
            mailboxes: BTreeMap::new(),
            sinks: BTreeMap::new(),
            timers: TimerRegistry::new(now),
            log_subscriptions: LogSubscriptions::new(),
            spawn_deadlines: DeadlineTable::new(),
            draining_exits: BTreeMap::new(),
            deferred_closures: BTreeMap::new(),
            closure_deadlines: DeadlineTable::new(),
            drain_deadlines: DeadlineTable::new(),
            superseded: BTreeMap::new(),
            watchdogs: FinishWatchdogSet::new(config.finish_grace(), KILL_GRACE),
            handle,
            events,
            sessions: SessionMinter::new(),
            listeners: None,
            config,
            processed: 0,
            protocols: BTreeMap::new(),
            last_refusal: None,
            pending_delivery: BTreeMap::new(),
            shm,
            peers,
            health,
            taps: TapRegistry::new(),
            metrics,
            sink: Arc::new(NullSink::new()),
            heartbeat,
            samples,
            cluster: crate::coordinator::ClusterState::new(),
            replay,
        })
    }

    /// Binds the configured listeners.
    ///
    /// # Errors
    ///
    /// Anything [`NodeListeners::bind`] can return.
    pub async fn bind(&mut self) -> DaemonResult<()> {
        let transport = astrs_transport::TransportConfig::uds();
        let listeners =
            NodeListeners::bind(self.config.listen(), transport, self.sessions.clone()).await?;
        if let Some(addr) = listeners.tcp_addr() {
            self.config.listen_mut().set_bound_tcp(addr);
        }
        self.listeners = Some(listeners);
        Ok(())
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &DaemonConfig {
        &self.config
    }

    /// Everything the daemon knows.
    #[must_use]
    pub const fn state(&self) -> &DaemonState {
        &self.state
    }

    /// Everything the daemon knows, mutably.
    ///
    /// The seam the dataflow FSM ([`crate::dataflow::fsm`]) writes phase
    /// transitions through. Nothing else should reach for it: every other
    /// change to a dataflow's state is a consequence of an event the loop
    /// already handled.
    pub const fn state_mut(&mut self) -> &mut DaemonState {
        &mut self.state
    }

    /// A handle background tasks and embedders report through.
    #[must_use]
    pub fn handle(&self) -> DaemonHandle {
        self.handle.clone()
    }

    /// The session minter, for an embedder wiring an in-process node.
    #[must_use]
    pub fn sessions(&self) -> SessionMinter {
        self.sessions.clone()
    }

    /// How many events the loop has processed.
    #[must_use]
    pub const fn processed(&self) -> u64 {
        self.processed
    }

    /// The daemon-level counters (§13).
    #[must_use]
    pub const fn metrics(&self) -> &DaemonMetrics {
        &self.metrics
    }

    /// The shared-memory plane (§6.2).
    #[must_use]
    pub const fn shm(&self) -> &ShmPlane {
        &self.shm
    }

    /// The shared-memory plane, mutably.
    ///
    /// The seam an embedder edits plane bookkeeping through — and the one a
    /// test uses to put the consumer-side ledger
    /// ([`crate::shm::InputRouteTable`]) into a state that would otherwise
    /// take a live node and a real segment to reach.
    pub const fn shm_mut(&mut self) -> &mut ShmPlane {
        &mut self.shm
    }

    /// The peer table (§6.4).
    #[must_use]
    pub const fn peers(&self) -> &PeerManager {
        &self.peers
    }

    /// The peer table, mutably — the seam an embedder dials a peer through.
    pub const fn peers_mut(&mut self) -> &mut PeerManager {
        &mut self.peers
    }

    /// The liveness table (§12).
    #[must_use]
    pub const fn health(&self) -> &HealthTable {
        &self.health
    }

    /// The debug taps (§13).
    #[must_use]
    pub const fn taps(&self) -> &TapRegistry {
        &self.taps
    }

    /// The debug taps, mutably — where `astrs topic echo` subscribes.
    pub const fn taps_mut(&mut self) -> &mut TapRegistry {
        &mut self.taps
    }

    /// Where everything reported upward goes (§7.3).
    #[must_use]
    pub fn sink(&self) -> &Arc<dyn ReportSink> {
        &self.sink
    }

    /// Replaces the report sink.
    ///
    /// Wave 4 installs a coordinator-backed sink here; a test installs a
    /// [`crate::health::RecordingSink`]; `astrs run` leaves the default
    /// [`NullSink`] alone.
    pub fn set_sink(&mut self, sink: Arc<dyn ReportSink>) {
        self.sink = sink;
    }

    /// The TCP address actually bound, if one is.
    #[must_use]
    pub fn tcp_addr(&self) -> Option<std::net::SocketAddr> {
        self.listeners.as_ref().and_then(NodeListeners::tcp_addr)
    }

    /// Registers a planned dataflow, without starting it.
    ///
    /// # Errors
    ///
    /// [`DaemonError::ShuttingDown`] if the daemon is winding down.
    pub fn admit(&mut self, plan: &DataflowPlan) -> DaemonResult<()> {
        if self.state.is_shutting_down() {
            return Err(DaemonError::ShuttingDown);
        }
        let now = self.clock.now();
        let mut dataflow = DataflowState::new(plan.dataflow, now)
            .with_exit_when_nodes_finish(plan.exit_when_nodes_finish);
        if let Some(name) = &plan.name {
            dataflow = dataflow.with_name(name.clone());
        }
        for spec in &plan.specs {
            dataflow.add_node(spec.clone());
        }
        self.state.insert_dataflow(dataflow);
        self.subscribe_virtual_inputs(plan);
        Ok(())
    }

    /// Registers the plan's `astrs/...` subscriptions with the daemon's
    /// timer wheel and log filters (§8.4).
    ///
    /// Arms every `astrs/timer/*` subscription on [`Daemon::virtual_now`]
    /// rather than the wall clock directly: for an ordinary run the two are
    /// the same instant, and for a deterministic one every subscription is
    /// thereby armed at the recording's first point, whatever real instant
    /// admission happens to land on (§14).
    fn subscribe_virtual_inputs(&mut self, plan: &DataflowPlan) {
        let now = self.virtual_now();
        for planned in &plan.virtual_inputs {
            if planned.is_timer() {
                // A timer whose period the wheel refuses cannot fire; the
                // manifest validator already refused zero rates, so this only
                // triggers on a rate beyond the interval type's range, which
                // is reported through the ordinary log path by the caller.
                let _ = self.timers.subscribe_at(
                    plan.dataflow,
                    planned.node.clone(),
                    planned.input.clone(),
                    planned.parsed.clone(),
                    now,
                );
            } else if planned.is_logs() {
                let _ = self.log_subscriptions.subscribe(
                    plan.dataflow,
                    planned.node.clone(),
                    planned.input.clone(),
                    &planned.source,
                );
            }
            // `astrs/status` needs no registration: it is delivered by the
            // supervisor directly, through the route table the plan already
            // built.
        }
    }

    /// Attaches an already-accepted connection.
    ///
    /// The seam an embedded daemon (`astrs run`) uses: it hands its node an
    /// in-process duplex rather than a socket, and the daemon does not care.
    ///
    /// A connection that arrived over a real socket
    /// ([`ConnectionOrigin::Uds`]/[`ConnectionOrigin::Tcp`]) must still greet
    /// (§7.2) before the ordinary conversation begins — see
    /// [`crate::session::SessionActor`]'s own docs for exactly why that
    /// matters and why [`ConnectionOrigin::InProcess`] is the one origin
    /// exempted from it.
    ///
    /// # Peer credentials (§16)
    ///
    /// A [`ConnectionOrigin::Uds`] connection additionally carries what the
    /// kernel vouches for about its peer
    /// ([`crate::server::listener::AcceptedNode::credentials`]). This is
    /// checked *before* the §7.2 handshake even starts: a peer running as
    /// another user has no business in this daemon's dataflow regardless of
    /// what token it presents, so refusing it here means the socket is
    /// closed (the actor is never spawned, no session sink is registered)
    /// rather than merely failing an auth check a moment later. A platform
    /// that cannot report credentials at all
    /// ([`crate::server::listener::credentials_acceptable`]'s `None` case)
    /// is unaffected — the §7.2 token remains the authoritative check for
    /// it, exactly as it is for every [`ConnectionOrigin::Tcp`] connection.
    pub fn attach(&mut self, accepted: AcceptedNode) {
        let session = accepted.session;
        if matches!(accepted.origin, ConnectionOrigin::Uds { .. })
            && !credentials_acceptable(accepted.credentials.as_ref(), daemon_uid())
        {
            self.metrics.record_uds_credential_rejection();
            tracing::warn!(
                %session,
                origin = %accepted.origin,
                credentials = ?accepted.credentials,
                "refusing a UDS connection: peer credentials are neither this daemon's own uid \
                 nor root (§16)"
            );
            return;
        }
        let (actor, sink) = match accepted.origin {
            ConnectionOrigin::InProcess => SessionActor::new_trusted(session, self.handle.clone()),
            // Real sockets always greet. Exhaustive on purpose (no wildcard):
            // `ConnectionOrigin` is defined in this crate, so a variant added
            // to it later will not compile here until someone has decided
            // whether it, too, needs the §7.2 handshake — the same
            // "exhaustive match forces classification" property
            // `ControlRequest::scope` relies on in `astrs-wire`.
            ConnectionOrigin::Uds { .. } | ConnectionOrigin::Tcp { .. } => {
                SessionActor::new(session, self.handle.clone(), self.config.auth().clone())
            }
        };
        let (reader, writer) = accepted.stream.into_split();
        tokio::spawn(actor.serve(reader, writer));
        self.sinks.insert(session, sink);
    }

    /// Runs the loop until a shutdown is requested or every dataflow finishes.
    ///
    /// Returns the results of the dataflows that completed, in id order.
    pub async fn run(&mut self) -> Vec<DataflowResult> {
        while let Some(event) = self.next_event().await {
            let stamped = self.clock.stamp(event);
            self.processed = self.processed.saturating_add(1);
            self.handle_event(stamped);
            // Any event may have filled a queue a node is already waiting on.
            self.flush_pending();
        }
        // The loop has ended, so the peer connections can be closed with a
        // goodbye rather than an EOF — the one part of the shutdown that needs
        // an `await` and therefore cannot live in `begin_shutdown`.
        self.close_peers().await;
        self.disconnect_coordinator().await;
        self.finish_all()
    }

    /// Says goodbye to the coordinator and stops the uplink task.
    ///
    /// Separate from [`Daemon::begin_shutdown`] for exactly the reason
    /// [`Daemon::close_peers`] is: telling the coordinator this daemon is
    /// leaving means writing a frame, and the shutdown path is driven from a
    /// synchronous handler. A daemon that never had a coordinator does
    /// nothing here.
    pub async fn disconnect_coordinator(&mut self) {
        let Some(uplink) = self.cluster.take_uplink() else {
            return;
        };
        // Reported through the sink the uplink still owns, so the writer task
        // gets one last frame out before it is asked to stop.
        self.sink.report(astrs_wire::DaemonEvent::Exit {
            graceful: true,
            message: "the daemon is shutting down".to_owned(),
        });
        // Give the writer a moment to drain that goodbye. Deadline-polled
        // rather than a fixed sleep: on a healthy link this returns almost
        // immediately, and on a dead one it must not add a second to every
        // shutdown.
        let deadline = Instant::now() + GOODBYE_GRACE;
        while uplink.buffered() > 0 && uplink.is_connected() && Instant::now() < deadline {
            tokio::time::sleep(GOODBYE_POLL).await;
        }
        uplink.stop().await;
        self.sink = Arc::new(NullSink::new());
    }

    /// Whether the loop has nothing left to do.
    ///
    /// Two ways to be done: a shutdown was requested and every node has since
    /// ended, or every admitted dataflow asked to `exit_when_nodes_finish`
    /// (§8.2) and every one of them has. The second is what makes
    /// [`crate::run_dataflow_with`] a function that *returns* rather than a
    /// service that must be told to stop.
    fn should_stop(&self) -> bool {
        if self.state.is_empty() {
            // A daemon with nothing admitted is idle, not finished: it is
            // waiting for a coordinator (or an embedder) to give it work.
            return self.state.is_shutting_down();
        }
        // An exit still draining its session is terminal but not yet
        // accounted for — see [`Self::is_accounted_for`]. Stopping the loop
        // here would end the run with that node's result missing from the
        // `DataflowResult` and its consumers never told their inputs closed.
        if !self.draining_exits.is_empty() {
            return false;
        }
        let all_finished = self
            .state
            .dataflows()
            .all(DataflowState::all_nodes_finished);
        if self.state.is_shutting_down() {
            return all_finished;
        }
        all_finished
            && self
                .state
                .dataflows()
                .all(DataflowState::exit_when_nodes_finish)
    }

    /// The next event from any source.
    ///
    /// Accepts and timer expiries are handled *inside* this call rather than
    /// being returned: neither is an event the state machine reasons about,
    /// and returning a synthetic one would make [`Daemon::processed`] count
    /// wall-clock ticks rather than work done. Returns [`None`] only when
    /// every handle is gone, which ends the loop.
    async fn next_event(&mut self) -> Option<DaemonEvent> {
        loop {
            if self.should_stop() {
                return None;
            }
            let sleep_for = self.next_deadline().map_or(IDLE_TICK, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(IDLE_TICK)
            });

            let accepted = match self.listeners.as_mut() {
                Some(listeners) => {
                    tokio::select! {
                        biased;
                        event = self.events.recv() => return event,
                        accepted = listeners.accept() => accepted,
                        () = tokio::time::sleep(sleep_for) => None,
                    }
                }
                None => {
                    tokio::select! {
                        biased;
                        event = self.events.recv() => return event,
                        () = tokio::time::sleep(sleep_for) => None,
                    }
                }
            };

            if let Some(accepted) = accepted {
                self.attach(accepted);
            }
            self.tick(Instant::now());
            // A timer tick fills a queue too.
            self.flush_pending();
        }
    }

    /// The earliest thing the loop is waiting for.
    fn next_deadline(&self) -> Option<Instant> {
        [
            self.timers.next_deadline(),
            self.spawn_deadlines.next_deadline(),
            self.drain_deadlines.next_deadline(),
            self.closure_deadlines.next_deadline(),
            self.watchdogs.next_deadline(),
            self.health.next_deadline(),
            self.shm.next_deadline(),
            Some(self.heartbeat.next_deadline()),
            Some(self.samples.next_deadline()),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Runs everything that is due at `now`: timer ticks, spawn deadlines and
    /// watchdog escalations.
    pub fn tick(&mut self, now: Instant) {
        self.sweep_deliveries();
        // The recorded clock, if this run has one, steps at most one point
        // here and hands back the instant the timer wheel should see —
        // `now` unchanged for an ordinary run (§14).
        let virtual_now = self.advance_replay(now);
        self.fire_timers(virtual_now);
        self.fire_spawn_deadlines(now);
        self.fire_drain_deadlines(now);
        self.fire_closure_deadlines(now);
        self.fire_replace_supersessions(now);
        self.fire_watchdogs(now);
        self.fire_health(now);
        self.poll_planes(now);
        self.sample_nodes(now);
        self.emit_heartbeat(now);
        self.poll_cluster(now);
    }

    /// Hands every connected session whatever is still queued for it.
    ///
    /// The liveness backstop under the targeted pushes. Every path that
    /// queues an event also pushes it (a publish through
    /// [`Self::push_to_waiting_consumers`], a closure through
    /// [`Self::close_source`], a tick and a log record through
    /// [`Self::push_to_node`]) — but a push moves only what the node's outbox
    /// had room for at that instant, and `pending_delivery` covers only the
    /// sessions parked in an explicit `NextEvent`. §9.1's canonical loop
    /// sends none, so without this sweep a message that arrived while a
    /// node's outbox was briefly full would sit in its mailbox until the next
    /// message happened to arrive — and after the last producer exits, no
    /// next message ever does.
    ///
    /// Runs from [`Daemon::tick`], which the loop reaches when its `select!`
    /// times out or accepts rather than on every event, so the cost is one
    /// drain attempt per session per idle tick (§24.2's `IDLE_TICK`, 250 ms)
    /// and not one per message.
    fn sweep_deliveries(&mut self) {
        let sessions: Vec<astrs_wire::SessionId> = self.sinks.keys().copied().collect();
        for session in sessions {
            if self.deliver_now(session, astrs_wire::DEFAULT_EVENT_BATCH) > 0 {
                self.pending_delivery.remove(&session);
            }
        }
    }

    /// Delivers due `astrs/timer/*` ticks (§8.4, §11.1).
    fn fire_timers(&mut self, now: Instant) {
        let mut ticked: BTreeSet<(DataflowId, NodeId)> = BTreeSet::new();
        for (subscriber, _fired) in self.timers.advance(now) {
            // A deterministic run stamps from the recorded point the wheel
            // was just advanced to, not the live HLC clock: `self.clock`'s
            // logical counter is bumped by every caller (a log record, a
            // status event, ...), which is not reproducible run to run.
            // Several ticks landing at the same recorded point still get
            // distinct, ordered stamps — see
            // [`crate::local::ReplaySource::tick_stamp`].
            let stamp = match self.replay.as_mut() {
                Some(replay) => replay.tick_stamp(),
                None => self.clock.now(),
            };
            let event = NodeEvent::Input {
                id: subscriber.input.clone(),
                source: crate::state::virtual_port_ref("astrs/timer")
                    .unwrap_or_else(|_| self.status_source()),
                metadata: astrs_wire::Metadata::new(stamp),
                payload: Vec::new(),
            };
            if let Some(mailbox) = self
                .mailboxes
                .get_mut(&(subscriber.dataflow, subscriber.node.clone()))
            {
                mailbox.push(&subscriber.input, event);
                ticked.insert((subscriber.dataflow, subscriber.node.clone()));
            }
        }

        // Queueing a tick is not delivering it. A published message reaches
        // its consumer because `apply_publish` pushes to that consumer's
        // session (`push_to_waiting_consumers`); a tick had no equivalent, so
        // it sat in the mailbox until the node happened to ask for events with
        // an explicit `NextEvent` — which §9.1's canonical loop
        // (`while let Some(event) = events.recv()`) never sends. A node whose
        // only input was `astrs/timer/*` therefore waited forever. §8.4 makes
        // a timer an input like any other, so it is pushed like any other.
        for (dataflow, node) in ticked {
            self.push_to_node(dataflow, &node);
        }
    }

    /// Fails nodes that were spawned but never registered (§12).
    fn fire_spawn_deadlines(&mut self, now: Instant) {
        for expiry in self.spawn_deadlines.expired(now) {
            let (dataflow, node) = expiry.key;
            let after = astrs_wire::DurationMs::from_duration(self.config.spawn_deadline());
            self.fail_node(
                dataflow,
                &node,
                expiry.generation,
                NodeExitCause::SpawnDeadlineExceeded { after },
            );
        }
    }

    /// Escalates stopping nodes that have run out of grace (§12).
    fn fire_watchdogs(&mut self, now: Instant) {
        for escalation in self.watchdogs.expired(now) {
            let Some(dataflow) = self.dataflow_of(&escalation.node) else {
                continue;
            };
            let Some(state) = self.state.dataflow_mut(dataflow) else {
                continue;
            };
            let Some(node) = state.node_mut(&escalation.node) else {
                continue;
            };
            node.mark_escalated(escalation.step.intent());
            if let Some(handle) = node.handle() {
                handle.signal_if_current(escalation.generation, escalation.step.signal());
            }
        }
    }

    /// Handles one stamped event.
    fn handle_event(&mut self, event: Stamped<DaemonEvent>) {
        match event.inner {
            DaemonEvent::Request { session, request } => {
                self.handle_request(session, *request);
            }
            DaemonEvent::NodeLog { session, frame } => {
                self.handle_node_log(session, &frame.record);
            }
            DaemonEvent::SessionClosed { session } => self.handle_session_closed(session),
            DaemonEvent::SessionFailed { .. } => {}
            DaemonEvent::ProcessExited {
                dataflow,
                node,
                generation,
                status,
            } => self.handle_process_exit(dataflow, &node, generation, status),
            DaemonEvent::NodeOutput {
                dataflow,
                node,
                stream,
                line,
            } => self.handle_node_output(dataflow, &node, stream, &line),
            DaemonEvent::NodeOutputClosed { .. } => {}
            DaemonEvent::RestartDue {
                dataflow,
                node,
                generation,
            } => self.handle_restart_due(dataflow, &node, generation),
            DaemonEvent::PeerAttached { link } => {
                let daemon = link.daemon().clone();
                self.handle_peer_attached(*link);
                // The dial that produced this link (if this daemon was the
                // side that dials, §6.4) has finished either way, and a fresh
                // link is exactly when the routes waiting on it can open.
                self.peer_directory_mut().dial_finished(&daemon);
                self.flush_peer_routes(&daemon);
            }
            DaemonEvent::PeerFrame { daemon, event } => self.handle_peer_frame(&daemon, *event),
            DaemonEvent::PeerLost { daemon, reason } => {
                self.handle_peer_lost(&daemon, &reason);
                // Releases the claim so the reconciliation backstop can dial
                // again; the peer's edges stay in the directory, because a
                // partition is not a reason to forget the graph (§12).
                self.peer_directory_mut().dial_finished(&daemon);
            }
            DaemonEvent::CoordinatorConnected { session, epoch } => {
                self.handle_coordinator_connected(session, epoch);
            }
            DaemonEvent::CoordinatorFrame { event } => self.handle_coordinator_frame(*event),
            DaemonEvent::CoordinatorLost { reason } => self.handle_coordinator_lost(&reason),
            DaemonEvent::Shutdown => self.begin_shutdown(),
        }
    }

    /// The dataflow a node belongs to, searching every registered one.
    pub(crate) fn dataflow_of(&self, node: &NodeId) -> Option<DataflowId> {
        self.state
            .dataflows()
            .find(|state| state.node(node).is_some())
            .map(DataflowState::id)
    }

    /// The reserved status port, as a producer reference.
    fn status_source(&self) -> astrs_wire::PortRef {
        astrs_wire::PortRef::new(NodeId::sanitized("astrs"), status_port())
    }

    /// Begins a graceful shutdown: every live node is asked to stop.
    pub fn begin_shutdown(&mut self) {
        if self.state.is_shutting_down() {
            return;
        }
        self.state.begin_shutdown();
        let now = Instant::now();
        let targets: Vec<(DataflowId, NodeId, u64)> = self
            .state
            .dataflows()
            .flat_map(|state| {
                let id = state.id();
                state
                    .nodes()
                    .filter(|node| !node.is_terminal())
                    .map(move |node| (id, node.id().clone(), node.generation()))
            })
            .collect();

        for (dataflow, node, generation) in targets {
            self.stop_node_at(dataflow, &node, StopCause::DaemonShutdown, now);
            let _ = generation;
        }
        // Peers are told before the rings go, so a remote consumer sees an
        // expected closure rather than a socket that stopped answering (§12).
        self.teardown_peer_routes();
        self.shm.shutdown();
    }

    /// Processes events for at most `budget`, returning how many it handled.
    ///
    /// The seam a test — or an embedder driving two daemons in one task —
    /// uses to advance a loop that has no reason to finish. [`Daemon::run`]
    /// returns when its dataflows do; a daemon acting purely as a peer never
    /// has any, so `pump` gives a caller a bounded slice of the same loop.
    pub async fn pump(&mut self, budget: Duration) -> usize {
        let deadline = Instant::now() + budget;
        let mut handled = 0;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let step = remaining.min(IDLE_TICK);
            let accepted = match self.listeners.as_mut() {
                Some(listeners) => {
                    tokio::select! {
                        biased;
                        event = self.events.recv() => {
                            if let Some(event) = event {
                                let stamped = self.clock.stamp(event);
                                self.processed = self.processed.saturating_add(1);
                                self.handle_event(stamped);
                                self.flush_pending();
                                handled += 1;
                            }
                            None
                        }
                        accepted = listeners.accept() => accepted,
                        () = tokio::time::sleep(step) => None,
                    }
                }
                None => {
                    tokio::select! {
                        biased;
                        event = self.events.recv() => {
                            if let Some(event) = event {
                                let stamped = self.clock.stamp(event);
                                self.processed = self.processed.saturating_add(1);
                                self.handle_event(stamped);
                                self.flush_pending();
                                handled += 1;
                            }
                            None
                        }
                        () = tokio::time::sleep(step) => None,
                    }
                }
            };
            if let Some(accepted) = accepted {
                self.attach(accepted);
            }
            self.tick(Instant::now());
            self.flush_pending();
        }
        handled
    }

    /// Collects the results of every dataflow.
    fn finish_all(&mut self) -> Vec<DataflowResult> {
        let now = self.clock.now();
        let ids: Vec<DataflowId> = self.state.dataflow_ids().collect();
        ids.into_iter()
            .filter_map(|id| self.state.dataflow_mut(id).map(|state| state.finish(now)))
            .collect()
    }

    /// The mailbox for one node, created on demand.
    pub(crate) fn mailbox(&mut self, dataflow: DataflowId, node: &NodeId) -> &mut NodeMailbox {
        self.mailboxes.entry((dataflow, node.clone())).or_default()
    }

    /// Sends one event to a node's socket, if it has one.
    pub(crate) fn send_to(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        event: NodeEvent,
    ) -> bool {
        let Some(session) = self.state.session_of(dataflow, node) else {
            return false;
        };
        match self.sinks.get(&session) {
            Some(sink) => sink.try_send(event).is_ok(),
            None => false,
        }
    }

    /// Marks a node failed, records the cause and applies the restart policy.
    pub(crate) fn fail_node(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
        cause: NodeExitCause,
    ) {
        let Some(state) = self.state.dataflow_mut(dataflow) else {
            return;
        };
        let Some(node_state) = state.node_mut(node) else {
            return;
        };
        if node_state.generation() != generation || node_state.is_terminal() {
            // A stale report about an incarnation that has already been
            // replaced: exactly what the generation stamp exists to catch.
            return;
        }
        if let Some(handle) = node_state.handle() {
            handle.kill_if_current(generation);
        }
        node_state.mark_exited(cause.clone());
        self.after_exit(dataflow, node, generation, cause);
    }

    /// Everything that follows a node's exit: outputs closed, peers told, the
    /// restart policy consulted, extensions reclaimed.
    pub(crate) fn after_exit(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
        cause: NodeExitCause,
    ) {
        self.watchdogs.disarm_generation(node, generation);
        self.spawn_deadlines
            .disarm_generation(&(dataflow, node.clone()), generation);
        self.health.disarm_generation(dataflow, node, generation);
        // The failure half of a `ReplaceNode` cutover (§8, §17): this
        // generation is always the *new* one by construction (the
        // superseded generation's own exit never reaches this far — see
        // `crate::dataflow::topology`'s module docs), so if it never
        // registered before ending, whatever it was meant to replace is
        // cleaned up here rather than left running un-managed forever. A
        // no-op for every node that was never replaced.
        self.resolve_superseded(dataflow, node);

        // Its rings go, and every ring it consumed from downgrades (§6.2).
        let orphaned = self.shm.on_producer_exit(dataflow, node);
        let downgrades = self.shm.on_consumer_exit(dataflow, node);
        self.apply_upgrade_actions(downgrades);
        for key in orphaned {
            tracing::debug!(segment = %key, "the producer's ring was closed");
        }

        // The reap has happened, so a teardown closure this incarnation left
        // pending now has an answer (§12). Dropping the entry is all it takes:
        // `close_outputs_of` below closes *every* port the node produced, so
        // the held ports are covered by the same pass — with the reason the
        // exit status justifies rather than the optimistic one the closure
        // frame carried no evidence for.
        let deferred = self.take_deferred_closure(dataflow, node, generation);
        let reason = if cause.is_failure() {
            RouteCloseReason::ProducerCrashed { generation }
        } else {
            RouteCloseReason::ProducerFinished
        };
        if let Some(pending) = &deferred {
            tracing::debug!(
                %dataflow,
                %node,
                generation,
                ports = pending.sources.len(),
                %reason,
                "a teardown closure was held until the exit status named it"
            );
        }
        self.close_outputs_of(dataflow, node, reason);
        self.reclaim_extensions(dataflow, node);

        if crate::supervise::warrants_peer_notification(&cause) {
            self.notify_peers_failed(dataflow, node, &cause);
        }

        self.record_and_decide(dataflow, node, cause.clone());
        let restarting = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(node))
            .is_some_and(|state| state.run_state() == NodeRunState::Restarting);
        self.report_node_stopped(dataflow, node, generation, &cause, restarting);
        self.publish_gauges();
        self.check_dataflow_completion(dataflow);
    }

    /// Records the exit cause and applies the restart policy (§12).
    fn record_and_decide(&mut self, dataflow: DataflowId, node: &NodeId, cause: NodeExitCause) {
        let now = Instant::now();
        let Some(state) = self.state.dataflow_mut(dataflow) else {
            return;
        };
        let Some(node_state) = state.node_mut(node) else {
            return;
        };
        let daemon_initiated = node_state.intent().is_daemon_initiated();
        let config = node_state.spec().restart;
        let decision = crate::supervise::decide(
            &config,
            &cause,
            node_state.history_mut(),
            daemon_initiated,
            now,
        );

        match decision {
            crate::supervise::RestartDecision::Restart { delay } => {
                node_state.mark_restarting();
                let generation = node_state.generation();
                crate::supervise::spawn_restart_timer(
                    self.handle.clone(),
                    dataflow,
                    node.clone(),
                    generation,
                    delay,
                );
            }
            crate::supervise::RestartDecision::Stop { cause } => {
                state.record_exit(node, cause);
            }
        }
    }

    /// Closes every output a node produced.
    fn close_outputs_of(&mut self, dataflow: DataflowId, node: &NodeId, reason: RouteCloseReason) {
        let Some(state) = self.state.dataflow(dataflow) else {
            return;
        };
        let sources: Vec<astrs_wire::PortRef> = state
            .routes()
            .produced_by(node)
            .into_iter()
            .cloned()
            .collect();
        let routes = state.routes().clone();
        let mut local: BTreeMap<NodeId, NodeMailbox> = BTreeMap::new();
        let mut keys = Vec::new();
        for source in &sources {
            for consumer in routes.consumers(source) {
                let key = (dataflow, consumer.node.clone());
                if let Some(mailbox) = self.mailboxes.remove(&key) {
                    local.insert(consumer.node.clone(), mailbox);
                    keys.push(key);
                }
            }
        }
        for source in &sources {
            crate::local::LocalRouter::close(&routes, &mut local, source, reason.clone());
        }
        for source in &sources {
            self.close_remote_outputs(dataflow, source, reason.clone());
        }
        for (node_id, mailbox) in local {
            self.mailboxes.insert((dataflow, node_id), mailbox);
        }
        // The closures are queued; push them, or a consumer in `recv()` waits
        // for a producer that has already gone (see `close_source`).
        for source in &sources {
            self.push_to_waiting_consumers(dataflow, &routes, source);
        }
        for key in keys {
            self.mailboxes.entry(key).or_default();
        }
        self.propagate_input_closure(dataflow);
    }

    /// Tells nodes whose every input is closed that they are done (§24.1).
    ///
    /// Exactly once per incarnation per node: several independent triggers
    /// (a producer's `OutputDone`, that producer's process exiting, a peer
    /// link dropping, a plane retiring a ring) each re-run this sweep, and
    /// [`crate::state::NodeState::claim_inputs_closed_notice`] is what keeps
    /// the second and later sweeps from re-announcing the same conclusion.
    pub(crate) fn propagate_input_closure(&mut self, dataflow: DataflowId) {
        let Some(state) = self.state.dataflow_mut(dataflow) else {
            return;
        };
        let finished: Vec<NodeId> = state
            .nodes_mut()
            .filter_map(|node| {
                (node.is_live() && node.claim_inputs_closed_notice()).then(|| node.id().clone())
            })
            .collect();
        for node in finished {
            self.send_to(dataflow, &node, NodeEvent::AllInputsClosed);
        }
    }

    /// Reclaims a node's extension entries and notifies interested parties.
    pub(crate) fn reclaim_extensions(&mut self, dataflow: DataflowId, node: &NodeId) {
        let Some(state) = self.state.dataflow_mut(dataflow) else {
            return;
        };
        let dropped = state.extensions_mut().reclaim_owner(node);
        for entry in dropped {
            let event = NodeEvent::ExtDropped {
                key: entry.key.clone(),
                reason: entry.reason_text(),
            };
            for interested in &entry.interested {
                if interested != node {
                    self.send_to(dataflow, interested, event.clone());
                }
            }
        }
    }

    /// Tells graph peers a node failed (§12).
    pub(crate) fn notify_peers_failed(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        cause: &NodeExitCause,
    ) {
        let Some(state) = self.state.dataflow(dataflow) else {
            return;
        };
        let peers: Vec<NodeId> = state.routes().downstream_of(node).into_iter().collect();
        let event = NodeEvent::NodeFailed {
            peer: node.clone(),
            cause: cause.clone(),
        };
        for peer in peers {
            self.send_to(dataflow, &peer, event.clone());
        }
    }

    /// Relays `node`'s own measured deadline (§11.3) violation to its graph
    /// peers on `astrs/status` and counts it — [`Self::notify_peers_failed`]'s
    /// pattern, for [`astrs_wire::NodeRequest::ReportDeadlineViolation`].
    ///
    /// The audience is the same as a failure's: `node`'s downstream
    /// consumers are exactly who has a stake in one of their upstream
    /// producers running late (a request/response peer that is not a direct
    /// producer is not affected the same way a missed deadline affects data
    /// freshness, so it is not included — unlike [`Self::notify_peers_failed`],
    /// which does include it because the peer is gone entirely, not merely
    /// slow).
    pub(crate) fn notify_peers_deadline_violated(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        input: DataId,
        budget: DurationMs,
        latency: DurationMs,
    ) {
        self.metrics.record_deadline_violation();
        let Some(state) = self.state.dataflow(dataflow) else {
            return;
        };
        let peers: Vec<NodeId> = state.routes().downstream_of(node).into_iter().collect();
        let event = NodeEvent::DeadlineViolated {
            peer: node.clone(),
            input,
            budget,
            latency,
        };
        for peer in peers {
            self.send_to(dataflow, &peer, event.clone());
        }
    }

    /// Finishes a dataflow whose nodes have all ended.
    /// Whether every exit in `dataflow` has had its consequences applied.
    ///
    /// A node whose exit is still draining its session is *terminal* but not
    /// yet *accounted for*: [`Self::handle_process_exit`] marks it exited
    /// immediately and defers everything else, so its result has not been
    /// recorded and its consumers have not been told their inputs closed.
    /// Treating `all_nodes_finished` as "done" inside that window ends a run
    /// with a `DataflowResult` missing a node — observed as a three-node
    /// graph reporting two node results, its third node cut off mid-work.
    fn is_accounted_for(&self, dataflow: DataflowId) -> bool {
        !self
            .draining_exits
            .keys()
            .any(|(draining, _)| *draining == dataflow)
    }

    fn check_dataflow_completion(&mut self, dataflow: DataflowId) {
        if !self.is_accounted_for(dataflow) {
            return;
        }
        let Some(state) = self.state.dataflow(dataflow) else {
            return;
        };
        if !state.all_nodes_finished() {
            return;
        }
        let now = self.clock.now();
        if let Some(state) = self.state.dataflow_mut(dataflow) {
            state.finish(now);
        }
    }

    /// Asks a node to stop, arming its finish watchdog.
    pub fn stop_node(&mut self, dataflow: DataflowId, node: &NodeId, cause: StopCause) {
        self.stop_node_at(dataflow, node, cause, Instant::now());
    }

    /// [`Daemon::stop_node`] with an explicit clock reading.
    fn stop_node_at(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        cause: StopCause,
        now: Instant,
    ) {
        let (generation, grace, is_live) = {
            let Some(state) = self.state.dataflow_mut(dataflow) else {
                return;
            };
            let Some(node_state) = state.node_mut(node) else {
                return;
            };
            if node_state.is_terminal() {
                return;
            }
            if matches!(cause, StopCause::DaemonShutdown) {
                node_state.mark_daemon_shutdown();
            } else {
                node_state.mark_stopping(cause.clone());
            }
            (
                node_state.generation(),
                node_state
                    .finish_grace()
                    .unwrap_or(self.config.finish_grace()),
                node_state.run_state() != NodeRunState::Pending,
            )
        };

        if is_live {
            self.send_to(
                dataflow,
                node,
                NodeEvent::Stop {
                    cause,
                    grace: Some(astrs_wire::DurationMs::from_duration(grace)),
                },
            );
            self.watchdogs
                .arm_with_grace(node.clone(), generation, now, grace);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_manifest::Manifest;
    use astrs_wire::{DataId, DataflowStatus};

    use super::*;
    use crate::config::{ListenConfig, RuntimePaths};
    use crate::dataflow::plan_dataflow;

    fn config() -> DaemonConfig {
        let root = std::env::temp_dir().join(format!(
            "astrs-daemon-core-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        DaemonConfig::new(RuntimePaths::under(root)).with_listen(ListenConfig::none())
    }

    fn plan(yaml: &str) -> DataflowPlan {
        let manifest = Manifest::from_yaml_str(yaml).expect("valid yaml");
        plan_dataflow(DataflowId::from_u128(1), &manifest, &BTreeMap::new()).expect("valid plan")
    }

    const PIPELINE: &str = "\
nodes:
  - id: camera
    path: /usr/bin/true
    outputs: [image]
  - id: detect
    path: /usr/bin/true
    inputs:
      frames: camera/image
";

    #[tokio::test]
    async fn a_new_daemon_hosts_nothing() {
        let daemon = Daemon::new(config()).unwrap();
        assert_eq!(daemon.state().dataflow_count(), 0);
        assert_eq!(daemon.processed(), 0);
        assert!(daemon.tcp_addr().is_none());
        assert!(!daemon.state().is_shutting_down());
    }

    #[tokio::test]
    async fn admitting_a_plan_registers_its_nodes_and_routes() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();

        let state = daemon
            .state()
            .dataflow(DataflowId::from_u128(1))
            .expect("admitted");
        assert_eq!(state.node_count(), 2);
        assert_eq!(state.routes().len(), 1);
        assert_eq!(state.status(), DataflowStatus::Pending);
    }

    #[tokio::test]
    async fn admitting_registers_timer_subscriptions() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon
            .admit(&plan(
                "nodes:\n  - id: p\n    path: /usr/bin/true\n    inputs:\n      tick: astrs/timer/hz/100\n",
            ))
            .unwrap();
        assert_eq!(daemon.timers.len(), 1);
        assert!(daemon.next_deadline().is_some());
    }

    #[tokio::test]
    async fn admitting_registers_log_subscriptions() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon
            .admit(&plan(
                "nodes:\n  - id: w\n    path: /usr/bin/true\n    inputs:\n      logs: astrs/logs/error\n",
            ))
            .unwrap();
        assert_eq!(daemon.log_subscriptions.len(), 1);
    }

    #[tokio::test]
    async fn a_shutting_down_daemon_refuses_new_work() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.begin_shutdown();
        assert!(matches!(
            daemon.admit(&plan(PIPELINE)),
            Err(DaemonError::ShuttingDown)
        ));
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.begin_shutdown();
        daemon.begin_shutdown();
        assert!(daemon.state().is_shutting_down());
    }

    #[tokio::test]
    async fn a_deadline_violation_increments_the_metric_regardless_of_delivery() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();
        let dataflow = DataflowId::from_u128(1);
        let camera = NodeId::new("camera").unwrap();

        // No session is bound for `detect` here: `send_to`'s delivery half
        // is a no-op (nothing to find), but the metric is not conditioned
        // on delivery succeeding — a violation happened whether or not
        // anyone is currently connected to hear about it. This mirrors
        // `record_deadline_violation`'s own contract in
        // `crate::metrics::DaemonMetrics`.
        daemon.notify_peers_deadline_violated(
            dataflow,
            &camera,
            DataId::new("frames").unwrap(),
            astrs_wire::DurationMs::new(50),
            astrs_wire::DurationMs::new(80),
        );

        let batch = daemon.metrics().snapshot(astrs_time::HlcTimestamp::EPOCH);
        let value = batch
            .points
            .iter()
            .find(|point| point.name == crate::metrics::names::DEADLINE_VIOLATIONS_TOTAL)
            .map(|point| point.value.as_f64());
        assert_eq!(value, Some(1.0));
    }

    #[tokio::test]
    async fn a_quiet_daemon_reports_zero_deadline_violations() {
        let daemon = Daemon::new(config()).unwrap();
        let batch = daemon.metrics().snapshot(astrs_time::HlcTimestamp::EPOCH);
        let value = batch
            .points
            .iter()
            .find(|point| point.name == crate::metrics::names::DEADLINE_VIOLATIONS_TOTAL)
            .map(|point| point.value.as_f64());
        assert_eq!(value, Some(0.0), "the quiet path stays silent");
    }

    #[tokio::test]
    async fn a_violation_on_an_unknown_dataflow_still_counts_but_delivers_nothing() {
        // Cannot happen through the real `NodeRequest` path (the daemon
        // resolves `dataflow`/`node` from the reporting session's own
        // binding, never from attacker-controlled fields), but the guard
        // clause this exercises — `self.state.dataflow(dataflow)` returning
        // `None` — is real code with its own early return, worth pinning
        // directly rather than only through a session that can never reach it.
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.notify_peers_deadline_violated(
            DataflowId::from_u128(999),
            &NodeId::new("ghost").unwrap(),
            DataId::new("frames").unwrap(),
            astrs_wire::DurationMs::new(50),
            astrs_wire::DurationMs::new(80),
        );
        let batch = daemon.metrics().snapshot(astrs_time::HlcTimestamp::EPOCH);
        let value = batch
            .points
            .iter()
            .find(|point| point.name == crate::metrics::names::DEADLINE_VIOLATIONS_TOTAL)
            .map(|point| point.value.as_f64());
        assert_eq!(value, Some(1.0), "the report itself is still counted");
    }

    #[tokio::test]
    async fn a_stop_moves_a_registered_node_to_stopping_and_arms_the_watchdog() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();
        let dataflow = DataflowId::from_u128(1);
        let camera = NodeId::new("camera").unwrap();

        daemon
            .state
            .dataflow_mut(dataflow)
            .unwrap()
            .node_mut(&camera)
            .unwrap()
            .mark_spawning(1);
        daemon.stop_node(dataflow, &camera, StopCause::Requested);

        let node = daemon
            .state()
            .dataflow(dataflow)
            .and_then(|state| state.node(&camera))
            .expect("present");
        assert_eq!(node.run_state(), NodeRunState::Stopping);
        assert!(daemon.watchdogs.is_armed(&camera));
    }

    #[tokio::test]
    async fn a_stop_on_a_terminal_node_does_nothing() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();
        let dataflow = DataflowId::from_u128(1);
        let camera = NodeId::new("camera").unwrap();
        daemon
            .state
            .dataflow_mut(dataflow)
            .unwrap()
            .node_mut(&camera)
            .unwrap()
            .mark_exited(NodeExitCause::Success);

        daemon.stop_node(dataflow, &camera, StopCause::Requested);
        assert!(!daemon.watchdogs.is_armed(&camera));
    }

    #[tokio::test]
    async fn a_spawn_deadline_fails_a_node_that_never_registered() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();
        let dataflow = DataflowId::from_u128(1);
        let camera = NodeId::new("camera").unwrap();
        let now = Instant::now();

        daemon
            .state
            .dataflow_mut(dataflow)
            .unwrap()
            .node_mut(&camera)
            .unwrap()
            .mark_spawning(u32::MAX);
        daemon
            .spawn_deadlines
            .arm_in((dataflow, camera.clone()), 0, now, Duration::from_millis(1));

        daemon.tick(now + Duration::from_millis(2));
        let node = daemon
            .state()
            .dataflow(dataflow)
            .and_then(|state| state.node(&camera))
            .expect("present");
        assert_eq!(node.run_state(), NodeRunState::Failed);
        assert!(matches!(
            node.exit_cause(),
            Some(NodeExitCause::SpawnDeadlineExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn a_stale_spawn_deadline_is_ignored() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();
        let dataflow = DataflowId::from_u128(1);
        let camera = NodeId::new("camera").unwrap();
        let now = Instant::now();

        {
            let node = daemon
                .state
                .dataflow_mut(dataflow)
                .unwrap()
                .node_mut(&camera)
                .unwrap();
            node.begin_next_generation();
            node.mark_spawning(u32::MAX);
        }
        // Armed for generation 0; the node is on generation 1.
        daemon
            .spawn_deadlines
            .arm_in((dataflow, camera.clone()), 0, now, Duration::from_millis(1));

        daemon.tick(now + Duration::from_millis(2));
        let node = daemon
            .state()
            .dataflow(dataflow)
            .and_then(|state| state.node(&camera))
            .expect("present");
        assert_eq!(
            node.run_state(),
            NodeRunState::Spawning,
            "the deadline belonged to a replaced incarnation"
        );
    }

    #[tokio::test]
    async fn a_timer_tick_reaches_a_registered_mailbox() {
        let mut daemon = Daemon::new(config()).unwrap();
        let plan = plan(
            "nodes:\n  - id: p\n    path: /usr/bin/true\n    inputs:\n      tick: astrs/timer/millis/10\n",
        );
        daemon.admit(&plan).unwrap();
        let dataflow = DataflowId::from_u128(1);
        let planner = NodeId::new("p").unwrap();
        let tick = DataId::new("tick").unwrap();
        daemon.mailbox(dataflow, &planner);

        let start = Instant::now();
        daemon.tick(start + Duration::from_millis(50));

        let mailbox = daemon
            .mailboxes
            .get(&(dataflow, planner))
            .expect("registered");
        assert!(
            mailbox.depth(&tick).unwrap_or(0) > 0,
            "a tick was delivered"
        );
    }

    #[tokio::test]
    async fn the_debug_form_summarizes_rather_than_dumps() {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();
        let rendered = format!("{daemon:?}");
        assert!(rendered.starts_with("Daemon"), "{rendered}");
        assert!(rendered.contains("dataflows: 1"), "{rendered}");
        assert!(rendered.contains("nodes: 2"), "{rendered}");
    }

    #[tokio::test]
    async fn a_handle_and_a_minter_are_shareable() {
        let daemon = Daemon::new(config()).unwrap();
        assert!(daemon.handle().is_open());
        assert_ne!(daemon.sessions().mint(), daemon.sessions().mint());
    }

    #[tokio::test]
    async fn an_in_process_attachment_registers_a_sink() {
        let mut daemon = Daemon::new(config()).unwrap();
        let (accepted, _node_side) = NodeListeners::in_process(daemon.sessions().mint());
        let session = accepted.session;
        daemon.attach(accepted);
        assert!(daemon.sinks.contains_key(&session));
    }

    /// §16: a UDS connection whose kernel-reported peer credentials name
    /// neither this daemon's own uid nor root must never reach the session
    /// machinery at all — no actor spawned, no sink registered — and the
    /// refusal must be visible on the metric an operator would alert on.
    /// Constructed credentials, exactly like [`super::credentials_acceptable`]'s
    /// own unit tests: no real socket is needed to prove the loop-level
    /// wiring, only [`AcceptedNode`]'s fields.
    #[tokio::test]
    async fn a_uds_connection_from_a_foreign_uid_is_refused() {
        let mut daemon = Daemon::new(config()).unwrap();
        let session = daemon.sessions().mint();
        let (daemon_side, _node_side) = tokio::io::duplex(4096);
        let real_uid = daemon_uid();
        let foreign_uid = if real_uid == 12345 { 54321 } else { 12345 };

        let accepted = AcceptedNode {
            session,
            stream: crate::server::listener::NodeStream::Duplex(daemon_side),
            credentials: Some(astrs_transport::PeerCredentials::new(
                foreign_uid,
                foreign_uid,
            )),
            origin: ConnectionOrigin::Uds {
                path: std::path::PathBuf::from("/run/astrs/daemon.sock"),
            },
        };
        daemon.attach(accepted);

        assert!(
            !daemon.sinks.contains_key(&session),
            "a refused connection must not register a sink"
        );

        let batch = daemon
            .metrics()
            .snapshot(astrs_time::HlcTimestamp::new(1, 0));
        let rejections = batch
            .points
            .iter()
            .find(|point| point.name == crate::metrics::names::UDS_CREDENTIAL_REJECTIONS_TOTAL)
            .map(|point| point.value.as_f64());
        assert_eq!(rejections, Some(1.0));
    }

    /// The same-uid case that would have refused nothing before this fix
    /// existed still attaches normally — the check is a refusal, not a
    /// blanket requirement that every UDS connection carry credentials.
    #[tokio::test]
    async fn a_uds_connection_from_the_daemons_own_uid_is_admitted() {
        let mut daemon = Daemon::new(config()).unwrap();
        let session = daemon.sessions().mint();
        let (daemon_side, _node_side) = tokio::io::duplex(4096);

        let accepted = AcceptedNode {
            session,
            stream: crate::server::listener::NodeStream::Duplex(daemon_side),
            credentials: Some(astrs_transport::PeerCredentials::new(daemon_uid(), 0)),
            origin: ConnectionOrigin::Uds {
                path: std::path::PathBuf::from("/run/astrs/daemon.sock"),
            },
        };
        daemon.attach(accepted);

        // A `Duplex` stream fed through the real (`Handshake`) admission
        // path never completes a §7.2 greeting, so the actor exits almost
        // immediately — what this asserts is only that `attach` chose to
        // spawn it at all, which is exactly what the credential check
        // gates. `sinks` is populated synchronously within `attach` itself,
        // before the spawned actor gets a chance to run.
        assert!(
            daemon.sinks.contains_key(&session),
            "a same-uid connection must still be attached"
        );
    }
}
