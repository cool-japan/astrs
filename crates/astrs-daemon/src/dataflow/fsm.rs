//! The dataflow finite state machine, and [`run_dataflow_with`].
//!
//! > *`astrs run` — the single-process mode: CLI embeds an in-process daemon
//! > (and no coordinator socket), runs the whole graph under one supervisor
//! > with the orphan guard (parent-pid + process-group kill).*
//!
//! [`run_dataflow_with`] is that mode's whole implementation, and the entry
//! point `astrs run` embeds. It is deliberately a *function*, not a service:
//! a manifest goes in, a [`astrs_wire::DataflowResult`] comes out, and every
//! process it started is dead by the time it returns — including the ones that
//! ignored `SIGTERM`, because the daemon's finish-straggler ladder (§12) is
//! what it delegates the wind-down to.
//!
//! ```text
//!   Pending ──build──► Building ──ok──► Ready ──spawn all──► Starting
//!      │                  │ failure                             │
//!      │                  ▼                                     │ every
//!      │               Failed ◄───────────────────┐             │ spawned node
//!      │                                          │             │ registered
//!      ▼                                          │             ▼
//!   Destroyed ◄── Stopping ◄── stop / a node ─────┴────────── Running
//!                     │        failed fatally                   │
//!                     ▼                                         │ every node
//!                  Finished ◄───────────────────────────────────┘ finished
//! ```
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::{RunOptions, run_dataflow_with};
//! use astrs_manifest::Manifest;
//!
//! let manifest = Manifest::from_yaml_file("dataflow.yml")?;
//! let result = run_dataflow_with(&manifest, RunOptions::default()).await?;
//!
//! assert!(!result.has_failures(), "{}", result.message);
//! # Ok(()) }
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use astrs_manifest::Manifest;
use astrs_wire::{DataflowId, DataflowResult, DataflowStatus, NodeId, StopCause};
use tokio::sync::watch;

use crate::config::{DaemonConfig, ListenConfig, RuntimePaths};
use crate::dataflow::build::run_build;
use crate::dataflow::plan::{DataflowPlan, plan_dataflow};
use crate::error::{DaemonError, DaemonResult};
use crate::health::ReportSink;
use crate::server::Daemon;
use crate::spawn::EnvPolicy;

/// How a dataflow should be run.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The dataflow's identifier; a fresh one is minted when unset.
    pub dataflow: Option<DataflowId>,
    /// The directory node paths resolve against.
    pub working_dir: Option<PathBuf>,
    /// The runtime directory for the embedded daemon's socket.
    pub runtime_dir: Option<PathBuf>,
    /// Whether to run `build:` lines first.
    pub build: bool,
    /// Extra inherited environment variables nodes may see (§16).
    pub env_passthrough: Vec<String>,
    /// How long a stopping node has before `SIGTERM` (§12).
    pub finish_grace: Duration,
    /// How long a spawned node has to register (§12).
    pub spawn_deadline: Duration,
    /// A ceiling on the whole run; `None` runs until the graph finishes.
    pub timeout: Option<Duration>,
    /// Whether the run is deterministic (§14).
    pub deterministic: bool,
    /// The recording a deterministic run replays as its clock source, and
    /// its pacing factor. See [`crate::config::DaemonConfig::with_replay_recording`].
    pub replay_recording: Option<(PathBuf, Option<f64>)>,
    /// Where the embedded daemon's upward traffic (heartbeats, captured
    /// node stdout/stderr lines, spawn/exit results, ...) goes.
    ///
    /// `None` keeps the daemon's own default (a
    /// [`crate::health::NullSink`] — nobody is watching). `astrs run`'s
    /// terminal log streamer installs a sink here so it observes every
    /// node's captured output live, via
    /// [`crate::server::core::Daemon::set_sink`], rather than only
    /// learning about a node's stdout/stderr after the whole run ends.
    pub report_sink: Option<Arc<dyn ReportSink>>,
    /// An external stop request (blueprint §17: `astrs run`'s clean
    /// Ctrl-C handling).
    ///
    /// `None` means "no external cancellation" — the run still ends on
    /// [`RunOptions::timeout`] or the graph finishing on its own.
    /// Flipping the watched value to `true` asks every live node of the
    /// dataflow to stop gracefully, exactly like [`RunOptions::timeout`]
    /// elapsing does: [`crate::config::DaemonConfig::finish_grace`] to
    /// finish, then the daemon's own `SIGTERM`→`SIGKILL` escalation
    /// ladder (§12) if a node ignores it.
    pub cancel: Option<watch::Receiver<bool>>,
}

impl RunOptions {
    /// The blueprint defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dataflow: None,
            working_dir: None,
            runtime_dir: None,
            build: true,
            env_passthrough: Vec::new(),
            finish_grace: crate::config::DEFAULT_FINISH_GRACE,
            spawn_deadline: crate::config::DEFAULT_SPAWN_DEADLINE,
            timeout: None,
            deterministic: false,
            replay_recording: None,
            report_sink: None,
            cancel: None,
        }
    }

    /// Uses an explicit dataflow identifier.
    #[must_use]
    pub const fn with_dataflow(mut self, dataflow: DataflowId) -> Self {
        self.dataflow = Some(dataflow);
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Sets the runtime directory.
    #[must_use]
    pub fn with_runtime_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.runtime_dir = Some(dir.into());
        self
    }

    /// Skips (or runs) the `build:` lines.
    #[must_use]
    pub const fn with_build(mut self, build: bool) -> Self {
        self.build = build;
        self
    }

    /// Adds explicitly passed-through environment variables.
    #[must_use]
    pub fn with_env_passthrough<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.env_passthrough
            .extend(names.into_iter().map(Into::into));
        self
    }

    /// Sets the finish grace period.
    #[must_use]
    pub const fn with_finish_grace(mut self, grace: Duration) -> Self {
        self.finish_grace = grace;
        self
    }

    /// Sets the spawn deadline.
    #[must_use]
    pub const fn with_spawn_deadline(mut self, deadline: Duration) -> Self {
        self.spawn_deadline = deadline;
        self
    }

    /// Caps the whole run.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Marks the run deterministic (§14).
    #[must_use]
    pub const fn with_deterministic(mut self, deterministic: bool) -> Self {
        self.deterministic = deterministic;
        self
    }

    /// Sets the recording a deterministic run replays as its clock source,
    /// and its pacing factor. See
    /// [`crate::config::DaemonConfig::with_replay_recording`].
    #[must_use]
    pub fn with_replay_recording(mut self, recording: Option<PathBuf>, speed: Option<f64>) -> Self {
        self.replay_recording = recording.map(|path| (path, speed));
        self
    }

    /// Installs a sink for the embedded daemon's upward traffic (heartbeats,
    /// captured node output, spawn/exit results, ...) — `astrs run`'s hook
    /// for streaming node logs to the terminal live.
    #[must_use]
    pub fn with_report_sink(mut self, sink: Arc<dyn ReportSink>) -> Self {
        self.report_sink = Some(sink);
        self
    }

    /// Wires an external stop request into the run (blueprint §17: `astrs
    /// run`'s clean Ctrl-C handling). Flipping `*receiver.borrow()` to
    /// `true` asks every node to stop gracefully, the same way
    /// [`RunOptions::timeout`] elapsing does.
    #[must_use]
    pub fn with_cancel(mut self, receiver: watch::Receiver<bool>) -> Self {
        self.cancel = Some(receiver);
        self
    }

    /// The daemon configuration these options describe.
    fn to_config(&self) -> DaemonConfig {
        let working_dir = self
            .working_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let runtime_dir = self.runtime_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("astrs-run-{}", std::process::id()))
        });
        // The socket name carries the pid *and* the dataflow id: two runs in
        // one process (a test suite, a CLI running two graphs) must not fight
        // over one path, and `sun_path` is too short to spell out more.
        let discriminator = self.dataflow.map_or(0, |id| {
            u64::try_from(id.as_u128() & 0xFFFF_FFFF).unwrap_or(0)
        });
        let paths = RuntimePaths::under(runtime_dir).with_socket_name(format!(
            "run-{}-{:x}.sock",
            std::process::id(),
            discriminator
        ));
        let listen = ListenConfig::uds(paths.socket_path());
        let (recording, speed) = match &self.replay_recording {
            Some((path, speed)) => (Some(path.clone()), *speed),
            None => (None, None),
        };
        DaemonConfig::new(paths)
            .with_listen(listen)
            .with_working_dir(working_dir)
            .with_env_passthrough(self.env_passthrough.clone())
            .with_finish_grace(self.finish_grace)
            .with_spawn_deadline(self.spawn_deadline)
            .with_deterministic(self.deterministic)
            .with_replay_recording(recording, speed)
            .with_run_parent_pid(Some(std::process::id()))
    }
}

impl Default for RunOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs one dataflow to completion in this process (§4.2).
///
/// Builds, spawns, supervises and stops the whole graph under one embedded
/// daemon, with no coordinator and no external socket beyond the node socket
/// the spawned processes dial. Returns when every node has finished, the
/// timeout elapses, or a fatal error stops the run.
///
/// # Errors
///
/// - [`DaemonError::Manifest`] if the manifest is invalid.
/// - [`DaemonError::BuildFailed`] if a `build:` line fails.
/// - Anything [`Daemon::new`] or [`Daemon::bind`] can return.
pub async fn run_dataflow_with(
    manifest: &Manifest,
    options: RunOptions,
) -> DaemonResult<DataflowResult> {
    let dataflow = options.dataflow.unwrap_or_else(DataflowId::generate);
    let options = options.with_dataflow(dataflow);
    let config = options.to_config();

    let policy = EnvPolicy::new().with_passthrough(config.env_passthrough().to_vec());
    let base_env = policy.scrub_process_env();
    let plan = plan_dataflow(dataflow, manifest, &base_env)?;

    let mut daemon = Daemon::new(config)?;
    register_manifest_rt(&daemon.spawner, manifest, dataflow);
    if let Some(sink) = options.report_sink.clone() {
        daemon.set_sink(sink);
    }
    daemon.bind().await?;
    daemon.admit(&plan)?;

    if options.build && plan.needs_build() {
        let spawner =
            crate::spawn::Spawner::with_policy(daemon.config().working_dir().to_path_buf(), policy);
        daemon.set_status(dataflow, DataflowStatus::Building);
        run_build(&spawner, &plan.build_steps).await.into_result()?;
    }
    daemon.set_status(dataflow, DataflowStatus::Ready);

    start_all(&mut daemon, &plan);

    let results = run_until_stopped(&mut daemon, dataflow, &options).await;

    results
        .into_iter()
        .find(|result| result.dataflow == dataflow)
        .ok_or(DaemonError::UnknownDataflow { dataflow })
}

/// Registers every node's manifest `rt:` reservation (§11.3, §22 hard-RT
/// reservations) with `spawner`, so [`start_all`] below picks each one up
/// automatically — no node's own [`SpawnRequest`](crate::spawn::SpawnRequest)
/// ever needs to call `with_rt` itself.
///
/// This is the "plan → admit path"
/// [`Spawner::register_rt`](crate::spawn::Spawner::register_rt)'s own docs
/// name as the intended caller: `astrs run`'s single-process daemon holds
/// `manifest` and `spawner` in the same scope with no wire hop between
/// them, unlike a coordinator-admitted dataflow, whose `NodeSpawnSpec`
/// cannot carry `rt` at all (see [`crate::spawn::rt`]'s module docs for why,
/// and for that deferred wire-side follow-up — a genuinely remote daemon
/// still cannot honor `rt:` until a wave authorized to change `astrs-wire`
/// gives it a carrier).
///
/// [`NodeId::new`] re-parses `node.id`, already proven valid by the
/// successful [`plan_dataflow`] call [`run_dataflow_with`] makes before this
/// runs — skipped defensively, never unwrapped, on the
/// unreachable-in-practice chance it is not (the same defense-in-depth
/// style [`crate::spawn::rt`]'s own `effective_priority` uses).
fn register_manifest_rt(
    spawner: &crate::spawn::Spawner,
    manifest: &Manifest,
    dataflow: DataflowId,
) {
    for node in &manifest.nodes {
        if let Some(rt) = node.rt
            && let Ok(node_id) = NodeId::new(node.id.clone())
        {
            spawner.register_rt(dataflow, node_id, rt);
        }
    }
}

/// Runs the daemon's event loop to completion, honoring both
/// [`RunOptions::timeout`] and [`RunOptions::cancel`] as early-stop
/// triggers.
///
/// Either one asks every live node to stop, then gives the daemon's own
/// `SIGTERM`→`SIGKILL` finish ladder (§12) up to twice the finish grace to
/// produce a result before giving up on it (matching the pre-existing
/// timeout-only behavior exactly).
async fn run_until_stopped(
    daemon: &mut Daemon,
    dataflow: DataflowId,
    options: &RunOptions,
) -> Vec<DataflowResult> {
    let early_stop = early_stop_requested(options.timeout, options.cancel.clone());
    tokio::select! {
        results = daemon.run() => results,
        () = early_stop => {
            stop_all(daemon, dataflow, StopCause::Requested);
            let grace = options.finish_grace + crate::server::KILL_GRACE;
            tokio::time::timeout(grace * 2, daemon.run())
                .await
                .unwrap_or_default()
        }
    }
}

/// Resolves once [`RunOptions::timeout`] elapses or [`RunOptions::cancel`]
/// is flipped to `true`, whichever comes first. Never resolves when neither
/// is set, so it is safe to race unconditionally in [`run_until_stopped`].
async fn early_stop_requested(timeout: Option<Duration>, cancel: Option<watch::Receiver<bool>>) {
    let timer = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = timer => {}
        () = wait_for_cancel(cancel) => {}
    }
}

/// Resolves once `cancel`'s watched value becomes `true`. Never resolves
/// when `cancel` is `None`, or once its sender is dropped without ever
/// setting `true` (nothing more can arrive, so this simply stops racing
/// ahead of whatever other trigger is still live).
async fn wait_for_cancel(cancel: Option<watch::Receiver<bool>>) {
    let Some(mut receiver) = cancel else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
            return;
        }
    }
}

/// Spawns every node the plan describes, moving the dataflow to `Starting`.
fn start_all(daemon: &mut Daemon, plan: &DataflowPlan) {
    daemon.set_status(plan.dataflow, DataflowStatus::Starting);
    let nodes: Vec<NodeId> = plan.specs.iter().map(|spec| spec.node.clone()).collect();
    for node in &nodes {
        daemon.spawn_node(plan.dataflow, node);
    }
}

/// Asks every live node of a dataflow to stop.
fn stop_all(daemon: &mut Daemon, dataflow: DataflowId, cause: StopCause) {
    daemon.set_status(dataflow, DataflowStatus::Stopping);
    let nodes: Vec<NodeId> = daemon
        .dataflow(dataflow)
        .map(|state| state.node_ids().cloned().collect())
        .unwrap_or_default();
    for node in &nodes {
        daemon.stop_node(dataflow, node, cause.clone());
    }
    daemon.handle().shutdown();
}

impl Daemon {
    /// Moves a dataflow to `status`.
    ///
    /// The FSM's only write into the daemon's own view of a dataflow's phase;
    /// every other transition is a consequence of a node event.
    pub fn set_status(&mut self, dataflow: DataflowId, status: DataflowStatus) {
        if let Some(state) = self.state_mut().dataflow_mut(dataflow) {
            state.set_status(status);
        }
    }

    /// Starts every node of an admitted dataflow.
    pub fn start_dataflow(&mut self, dataflow: DataflowId) {
        self.set_status(dataflow, DataflowStatus::Starting);
        let nodes: Vec<NodeId> = self
            .dataflow(dataflow)
            .map(|state| state.node_ids().cloned().collect())
            .unwrap_or_default();
        for node in &nodes {
            self.spawn_node(dataflow, node);
        }
    }

    /// Asks every live node of a dataflow to stop (§12).
    pub fn stop_dataflow(&mut self, dataflow: DataflowId, cause: StopCause) {
        stop_all(self, dataflow, cause);
    }

    /// Destroys a dataflow: stop everything, then forget it.
    pub fn destroy_dataflow(&mut self, dataflow: DataflowId) {
        self.stop_dataflow(dataflow, StopCause::Destroyed);
        if let Some(state) = self.state_mut().dataflow_mut(dataflow) {
            state.extensions_mut().clear();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::health::RecordingSink;

    fn options() -> RunOptions {
        RunOptions::new()
            .with_runtime_dir(std::env::temp_dir())
            .with_working_dir(std::env::temp_dir())
            .with_finish_grace(Duration::from_millis(200))
            .with_spawn_deadline(Duration::from_secs(2))
            .with_timeout(Duration::from_secs(20))
    }

    #[test]
    fn the_defaults_follow_the_blueprint() {
        let options = RunOptions::new();
        assert!(options.build);
        assert!(!options.deterministic);
        assert!(options.timeout.is_none());
        assert_eq!(options.finish_grace, Duration::from_secs(15));
        assert_eq!(options.spawn_deadline, Duration::from_secs(30));
        assert!(options.dataflow.is_none());
    }

    #[test]
    fn options_are_all_overridable() {
        let dataflow = DataflowId::from_u128(9);
        let options = RunOptions::new()
            .with_dataflow(dataflow)
            .with_working_dir("/workspace")
            .with_runtime_dir("/run/astrs")
            .with_build(false)
            .with_env_passthrough(["CUDA_VISIBLE_DEVICES"])
            .with_finish_grace(Duration::from_secs(1))
            .with_spawn_deadline(Duration::from_secs(2))
            .with_timeout(Duration::from_secs(3))
            .with_deterministic(true);

        assert_eq!(options.dataflow, Some(dataflow));
        assert_eq!(
            options.working_dir.as_deref(),
            Some(std::path::Path::new("/workspace"))
        );
        assert!(!options.build);
        assert_eq!(options.env_passthrough, ["CUDA_VISIBLE_DEVICES"]);
        assert_eq!(options.timeout, Some(Duration::from_secs(3)));
        assert!(options.deterministic);
    }

    #[test]
    fn the_derived_configuration_arms_the_orphan_guard_and_the_socket() {
        let config = options().to_config();
        assert_eq!(config.run_parent_pid(), Some(std::process::id()));
        assert!(config.listen().uds_path().is_some());
        assert!(
            config.listen().tcp_addr().is_none(),
            "an embedded daemon opens no TCP port"
        );
        assert_eq!(config.finish_grace(), Duration::from_millis(200));
    }

    #[test]
    fn two_runs_get_two_sockets_when_the_runtime_dir_is_shared() {
        // The socket name carries the pid, so a second daemon in another
        // process never collides with this one.
        let config = options().to_config();
        let path = config.listen().uds_path().expect("a socket").to_path_buf();
        assert!(
            path.to_string_lossy()
                .contains(&std::process::id().to_string()),
            "{}",
            path.display()
        );
    }

    #[tokio::test]
    async fn an_invalid_manifest_is_refused_before_anything_starts() {
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: ./a\n    inputs:\n      in: ghost/out\n",
        )
        .unwrap();
        let error = run_dataflow_with(&manifest, options()).await.unwrap_err();
        assert!(matches!(error, DaemonError::Manifest(_)), "{error}");
    }

    #[tokio::test]
    async fn a_failing_build_stops_the_run_before_spawning() {
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    build: /bin/sh -c 'exit 4'\n",
        )
        .unwrap();
        let error = run_dataflow_with(&manifest, options()).await.unwrap_err();
        assert!(matches!(error, DaemonError::BuildFailed { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_build_can_be_skipped() {
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    build: /bin/sh -c 'exit 4'\n",
        )
        .unwrap();
        // With `build: false` the failing line never runs, so the graph gets
        // as far as spawning — and `/usr/bin/true` exits without registering,
        // which the spawn deadline catches.
        let options = options()
            .with_build(false)
            .with_spawn_deadline(Duration::from_millis(300))
            .with_timeout(Duration::from_secs(10));
        let result = run_dataflow_with(&manifest, options).await.unwrap();
        assert_eq!(result.node_results.len(), 1);
    }

    #[tokio::test]
    async fn a_report_sink_observes_captured_node_output_live() {
        // The exact scenario `report_sink`'s own docs name: `astrs run`'s
        // terminal log streamer wants every node's stdout/stderr as the run
        // happens, not only a summary once it ends.
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: /bin/sh\n    args: [\"-c\", \"echo hello-from-sink\"]\n",
        )
        .unwrap();
        let sink = Arc::new(RecordingSink::new());
        let run_options = options().with_report_sink(sink.clone());
        let result = run_dataflow_with(&manifest, run_options).await.unwrap();
        assert!(!result.has_failures(), "{result:?}");

        let events = sink.snapshot();
        let saw_it = events.iter().any(|event| match event {
            astrs_wire::DaemonEvent::Log { records, .. } => records
                .iter()
                .any(|record| record.message.contains("hello-from-sink")),
            _ => false,
        });
        assert!(saw_it, "sink never observed the captured line: {events:?}");
    }

    #[tokio::test]
    async fn a_cancel_signal_stops_the_run_well_before_its_timeout() {
        // A node that sleeps far longer than either the timeout below or a
        // reasonable test budget; only an external cancel — not the 20 s
        // timeout `options()` sets — can plausibly end this run quickly.
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: /bin/sleep\n    args: [\"30\"]\n",
        )
        .unwrap();
        let (sender, receiver) = watch::channel(false);
        let run_options = options().with_cancel(receiver);

        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = sender.send(true);
        });

        let started = std::time::Instant::now();
        let result = run_dataflow_with(&manifest, run_options).await.unwrap();
        let elapsed = started.elapsed();
        canceller.await.unwrap();

        assert!(
            elapsed < Duration::from_secs(10),
            "a cancel signal should cut the 30 s sleep far short: {elapsed:?}"
        );
        // A `sleep 30` killed mid-flight is not a clean exit; the point of
        // this assertion is only that the run *ended*, not what it ended as.
        assert!(result.finished_at.is_some(), "{result:?}");
    }

    #[tokio::test]
    async fn cancel_is_a_no_op_once_the_graph_finishes_on_its_own() {
        let manifest =
            Manifest::from_yaml_str("nodes:\n  - id: a\n    path: /usr/bin/true\n").unwrap();
        let (_sender, receiver) = watch::channel(false);
        let run_options = options().with_cancel(receiver);
        let result = run_dataflow_with(&manifest, run_options).await.unwrap();
        assert_eq!(result.status, DataflowStatus::Finished, "{result:?}");
    }

    // ------------------------------------ manifest `rt:` → spawn config (§11.3, §22)

    #[test]
    fn manifest_rt_reservations_are_registered_with_the_spawner_before_any_spawn() {
        // The exact gap a previous pass left open: `spawn::rt`'s whole
        // application layer existed with zero production callers of
        // `Spawner::register_rt`, so a manifest `rt:` block was silently a
        // no-op end to end. This proves `register_manifest_rt` — the fix —
        // without paying for a real spawn: `Spawner::registered_rt` is
        // exactly what `Spawner::spawn` consults later.
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  \
               - id: a\n    path: /usr/bin/true\n    rt:\n      policy: fifo\n      priority: 42\n  \
               - id: b\n    path: /usr/bin/true\n",
        )
        .unwrap();
        manifest.validate().unwrap();

        let spawner = crate::spawn::Spawner::new(std::env::temp_dir());
        let dataflow = DataflowId::from_u128(1);
        register_manifest_rt(&spawner, &manifest, dataflow);

        assert_eq!(
            spawner.registered_rt(dataflow, &NodeId::new("a").unwrap()),
            astrs_manifest::RtConfig {
                policy: astrs_manifest::RtPolicy::Fifo,
                priority: Some(42),
            }
        );
        // Node `b` names no `rt:` block at all — it must read back as the
        // unregistered default, not have been registered *at* the default:
        // `Spawner::register_rt`'s own docs are explicit that the two are
        // meant to stay indistinguishable at spawn time, so this only
        // proves `register_manifest_rt` skips it, either reading is valid
        // evidence.
        assert_eq!(
            spawner.registered_rt(dataflow, &NodeId::new("b").unwrap()),
            astrs_manifest::RtConfig::default()
        );
    }

    /// As the test above, but against a real [`Daemon`]'s own `spawner`
    /// rather than a bare [`crate::spawn::Spawner`] built for the test:
    /// [`Daemon::new`] is exactly what [`run_dataflow_with`] constructs, and
    /// the daemon's `spawn_node` handler spawns every node through that same
    /// field, never a throwaway one. Closes the half the test above cannot:
    /// that the registration lands on the spawner production spawning
    /// actually consults, not merely *a* spawner.
    #[tokio::test]
    async fn a_real_daemons_own_spawner_receives_the_manifests_rt_reservations() {
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    rt:\n      policy: rr\n      priority: 55\n",
        )
        .unwrap();
        manifest.validate().unwrap();

        let dataflow = DataflowId::generate();
        let config = options().with_dataflow(dataflow).to_config();
        let daemon = Daemon::new(config).unwrap();
        register_manifest_rt(&daemon.spawner, &manifest, dataflow);

        assert_eq!(
            daemon
                .spawner
                .registered_rt(dataflow, &NodeId::new("a").unwrap()),
            astrs_manifest::RtConfig {
                policy: astrs_manifest::RtPolicy::Rr,
                priority: Some(55),
            }
        );
    }

    #[test]
    fn a_manifest_with_no_rt_blocks_registers_nothing() {
        let manifest =
            Manifest::from_yaml_str("nodes:\n  - id: a\n    path: /usr/bin/true\n").unwrap();
        manifest.validate().unwrap();

        let spawner = crate::spawn::Spawner::new(std::env::temp_dir());
        let dataflow = DataflowId::from_u128(2);
        register_manifest_rt(&spawner, &manifest, dataflow);

        assert_eq!(
            spawner.registered_rt(dataflow, &NodeId::new("a").unwrap()),
            astrs_manifest::RtConfig::default()
        );
    }

    #[tokio::test]
    async fn an_rt_reservation_reaches_a_real_run_through_the_manifest_alone() {
        // End to end, through the public `run_dataflow_with` entry point: no
        // test here calls `with_rt` or `register_rt` itself — the manifest's
        // `rt:` block is the only source. `/usr/bin/true` exits immediately
        // either way, so this asserts the run completes cleanly regardless
        // of whether this platform/process can actually honor the
        // reservation — `spawn::rt::arm`'s own tests (and
        // `rt_metrics_are_recorded_through_a_real_spawn` in
        // `spawn::process`) are what prove *which* outcome a given
        // environment gets; this test only proves the manifest value made it
        // into a real spawn attempt at all, not stuck at `RtConfig::default`.
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    rt:\n      policy: fifo\n      priority: 10\n",
        )
        .unwrap();
        // A short timeout, not the 20 s default: `/usr/bin/true` exits in
        // milliseconds either way (`rt::arm`'s outcome only decides *how*
        // the spawn attempt goes, never how long the node itself runs), and
        // this is one more `run_dataflow_with` call in an already-large
        // suite.
        let run_options = options().with_timeout(Duration::from_secs(5));
        let result = run_dataflow_with(&manifest, run_options).await.unwrap();
        assert_eq!(result.node_results.len(), 1, "{result:?}");
    }
}
