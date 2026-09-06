//! [`Spawner`] — turning a [`astrs_wire::NodeSpawnSpec`] into a running
//! process.
//!
//! The assembly point for everything else in this module: the environment
//! ([`crate::spawn::env`]), the argument split ([`crate::spawn::argv`]), the
//! handshake blob ([`crate::spawn::blob`]) and the signalling handle
//! ([`crate::spawn::handle`]) all meet here, in one fixed order, and the child
//! comes out the other side.
//!
//! ```text
//!   NodeSpawnSpec + NodeConfig
//!            │
//!            ├── env:  scrub → expand → deny-filter → daemon-owned last
//!            ├── argv: shlex split, no shell
//!            ├── cwd:  spec.working_dir, else the dataflow working dir
//!            ├── pgid: its own process group, so children die with it
//!            └── io:   piped, so `astrs/logs/*` has something to fan out
//!            ▼
//!      SpawnedProcess { handle, child, stdout, stderr }
//! ```
//!
//! `env_clear()` is called unconditionally before the built environment is
//! applied: a variable reaches a node because this module put it there, or it
//! does not reach it at all. That single line is what makes the §16 allowlist
//! an allowlist rather than a suggestion.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::spawn::{Spawner, SpawnRequest, StdioMode};
//! use astrs_wire::{AuthToken, DaemonId, DataflowId, NodeConfig, NodeId, NodeSource, NodeSpawnSpec};
//!
//! let spec = NodeSpawnSpec::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     0,
//!     NodeSource::Executable { path: "/usr/bin/env".into() },
//! );
//! let config = NodeConfig::new(spec.clone(), DaemonId::generate(None), AuthToken::ZERO);
//!
//! let spawner = Spawner::new(std::env::temp_dir());
//! let request = SpawnRequest::new(&spec, &config)?.with_stdio(StdioMode::Capture);
//! let mut spawned = spawner.spawn(request)?;
//! let status = spawned.child_mut().wait().await?;
//! assert!(status.success());
//! # Ok(()) }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use astrs_manifest::{EnvValue, RtConfig};
use astrs_wire::{DataflowId, NodeConfig, NodeId, NodeSource, NodeSpawnSpec};
use tokio::process::{Child, ChildStderr, ChildStdout};

use crate::error::{DaemonError, DaemonResult};
use crate::metrics::DaemonMetrics;
use crate::spawn::affinity::{self, CpuAffinityOutcome};
use crate::spawn::argv::CommandLine;
use crate::spawn::blob::DaemonOwnedVars;
use crate::spawn::env::{BuiltEnv, EnvPolicy};
use crate::spawn::handle::ProcessHandle;
use crate::spawn::rt::{self, RtOutcome};

/// What to do with a child's standard streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum StdioMode {
    /// Pipe both, so the daemon can turn them into log records (§13).
    #[default]
    Capture,
    /// Let the child share the daemon's own streams.
    ///
    /// Useful for `astrs run` in a terminal, where a node's `println!` going
    /// straight to the console is what the user expects.
    Inherit,
    /// Discard both.
    Null,
}

impl StdioMode {
    /// The [`Stdio`] this mode configures.
    fn to_stdio(self) -> Stdio {
        match self {
            Self::Capture => Stdio::piped(),
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
        }
    }

    /// Whether the daemon will receive the child's output.
    #[must_use]
    pub const fn is_captured(self) -> bool {
        matches!(self, Self::Capture)
    }
}

/// One request to start one node incarnation.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// The dataflow the node belongs to.
    dataflow: DataflowId,
    /// The node.
    node: NodeId,
    /// The incarnation being started.
    generation: u64,
    /// The program and arguments.
    command: CommandLine,
    /// Manifest values that still need `$VAR` expansion.
    manifest_env: BTreeMap<String, EnvValue>,
    /// Manifest values that are already literal — expansion happened upstream.
    literal_env: BTreeMap<String, String>,
    /// The daemon-owned variables.
    owned: DaemonOwnedVars,
    /// The working directory, if the spec names one.
    working_dir: Option<PathBuf>,
    /// What to do with the child's streams.
    stdio: StdioMode,
    /// CPU cores this incarnation is pinned to (§11.3); empty means unpinned.
    cpu_affinity: Vec<u16>,
    /// The real-time scheduling reservation this incarnation should be
    /// spawned under (§11.3, §22 hard-RT reservations); `None` means "let
    /// [`Spawner::spawn`] fall back to whatever
    /// [`Spawner::register_rt`](super::Spawner::register_rt) holds for this
    /// `(dataflow, node)` pair" (itself [`RtConfig::default`] — the ordinary
    /// time-sharing class — when nothing was registered either).
    ///
    /// `Option`, not a bare [`RtConfig`], specifically so [`Self::with_rt`]
    /// can be told apart from a caller that never touched `rt` at all: both
    /// [`RtConfig::default`] and a genuine reservation are legal *values* to
    /// set explicitly (a caller pinning a node back to `SCHED_OTHER` on
    /// purpose, overriding whatever the registry holds, must be able to say
    /// so) — collapsing that case onto "nothing was set" would make an
    /// explicit override of a registered reservation impossible to express
    /// at all, not merely awkward. See [`Self::rt`] for the resolved
    /// value a caller reads back.
    ///
    /// Unlike [`Self::cpu_affinity`], this is **not** derived from
    /// [`NodeSpawnSpec`] in [`SpawnRequest::new`] — see that constructor's
    /// own docs for why (`astrs_wire::NodeSpawnSpec` is a byte-frozen wire
    /// type this crate cannot add a field to).
    rt: Option<RtConfig>,
}

impl SpawnRequest {
    /// A request for `spec`, carrying `config` as its handshake blob.
    ///
    /// The command line comes from [`NodeSpawnSpec::source`]: an
    /// [`NodeSource::Executable`] contributes its path (which may itself carry
    /// arguments, e.g. `python3 -m mynode`), and the spec's `args` follow.
    ///
    /// [`Self::rt`] resolves to [`Spawner::registered_rt`](super::Spawner::registered_rt)'s
    /// answer rather than being read from `spec`: unlike `cpu_affinity`,
    /// `rt` is not a [`NodeSpawnSpec`] field at all —
    /// `astrs_wire::NodeSpawnSpec` is embedded, non-terminally, inside
    /// `ControlRequest::AddNode` / `CoordinatorEvent::Spawn`, both
    /// byte-frozen by `crates/astrs-wire/tests/golden/protocol.frozen.snap`
    /// (blueprint §7.2), so adding a field there shifts every trailing byte
    /// of both messages and fails that frozen-prefix guard — confirmed
    /// directly before this design was chosen, not assumed. A caller that
    /// wants a specific reservation *regardless* of the registry —
    /// overriding it, or pinning `SCHED_OTHER` deliberately — calls
    /// [`Self::with_rt`] itself; see this crate's `spawn::rt` module docs
    /// for the full rationale and the wire-side follow-up this leaves for a
    /// wave authorized to change `astrs-wire`.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::BadArgv`] if the path cannot be split.
    /// - [`DaemonError::Wire`] if the handshake blob cannot be encoded.
    /// - [`DaemonError::BadState`] for a source that spawns no process
    ///   ([`NodeSource::Dynamic`]).
    pub fn new(spec: &NodeSpawnSpec, config: &NodeConfig) -> DaemonResult<Self> {
        let command = command_for(spec)?;
        let owned = DaemonOwnedVars::new(config)?;
        Ok(Self {
            dataflow: spec.dataflow,
            node: spec.node.clone(),
            generation: spec.generation,
            command,
            // `NodeSpawnSpec::env` is already expanded and filtered
            // (`astrs_manifest::expand_map` ran against the scrubbed base
            // before the spec was built), so the values are literal. Wrapping
            // them as `EnvValue::String` would re-enter `$VAR` expansion and
            // reject a legitimate literal `$` — see `SpawnRequest::literal_env`.
            manifest_env: BTreeMap::new(),
            literal_env: spec.env.clone(),
            owned,
            working_dir: spec.working_dir.as_ref().map(PathBuf::from),
            stdio: StdioMode::default(),
            cpu_affinity: spec.cpu_affinity.clone(),
            rt: None,
        })
    }

    /// A request built from parts, for a caller that is not starting from a
    /// [`NodeSpawnSpec`] — a build step, a test helper.
    #[must_use]
    pub fn from_parts(
        dataflow: DataflowId,
        node: NodeId,
        generation: u64,
        command: CommandLine,
        owned: DaemonOwnedVars,
    ) -> Self {
        Self {
            dataflow,
            node,
            generation,
            command,
            manifest_env: BTreeMap::new(),
            literal_env: BTreeMap::new(),
            owned,
            working_dir: None,
            stdio: StdioMode::default(),
            cpu_affinity: Vec::new(),
            rt: None,
        }
    }

    /// Replaces the manifest environment with values that still need `$VAR`
    /// expansion against the scrubbed base.
    #[must_use]
    pub fn with_manifest_env(mut self, env: BTreeMap<String, EnvValue>) -> Self {
        self.manifest_env = env;
        self
    }

    /// Replaces the already-expanded manifest environment.
    ///
    /// These values are copied through verbatim: a literal `$` in one of them
    /// is a `$`, not the start of a reference. They still pass the §16
    /// deny-filter — expansion having happened upstream does not make a
    /// `LD_PRELOAD` acceptable.
    #[must_use]
    pub fn with_literal_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.literal_env = env;
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Sets the stdio mode.
    #[must_use]
    pub const fn with_stdio(mut self, stdio: StdioMode) -> Self {
        self.stdio = stdio;
        self
    }

    /// Arms the orphan guard on the daemon-owned variables.
    #[must_use]
    pub fn with_run_parent_pid(mut self, pid: Option<u32>) -> Self {
        self.owned = self.owned.with_run_parent_pid(pid);
        self
    }

    /// Sets the CPU cores this incarnation is pinned to (§11.3).
    #[must_use]
    pub fn with_cpu_affinity(mut self, cores: Vec<u16>) -> Self {
        self.cpu_affinity = cores;
        self
    }

    /// The CPU cores this request is pinned to; empty means unpinned.
    #[must_use]
    pub fn cpu_affinity(&self) -> &[u16] {
        &self.cpu_affinity
    }

    /// Sets the real-time scheduling reservation this incarnation should be
    /// spawned under (§11.3, §22 hard-RT reservations), regardless of what
    /// [`Spawner::register_rt`](super::Spawner::register_rt) may hold for
    /// this `(dataflow, node)` pair — an explicit call here always wins over
    /// the registry (see [`Self::new`]'s own docs for why this crate cannot
    /// resolve `rt` from [`NodeSpawnSpec`] itself, and why the registry
    /// exists at all).
    ///
    /// Setting [`RtConfig::default`] explicitly is meaningfully different
    /// from never calling this at all: the former pins a node to
    /// `SCHED_OTHER` on purpose (overriding a registered real-time policy,
    /// say); the latter defers entirely to whatever the registry holds.
    /// [`Self::rt`] cannot distinguish the two by itself — it always reports
    /// the *effective* value, defaulting when unset exactly as a registry
    /// miss would — so a caller that needs to know which one actually
    /// happened is asking the wrong object: that question belongs to
    /// whichever [`Spawner`] this request is eventually spawned through.
    #[must_use]
    pub const fn with_rt(mut self, rt: RtConfig) -> Self {
        self.rt = Some(rt);
        self
    }

    /// The real-time scheduling reservation this request carries, resolved
    /// against nothing but itself: [`RtConfig::default`] (the ordinary
    /// time-sharing class) unless [`Self::with_rt`] was called explicitly.
    ///
    /// This is **not** the value a spawn will actually apply when nothing
    /// was set explicitly — that additionally depends on
    /// [`Spawner::registered_rt`](super::Spawner::registered_rt) for this
    /// request's `(dataflow, node)` pair, which a bare [`SpawnRequest`] has
    /// no way to consult (it holds no reference to a [`Spawner`] at all).
    /// Precise enough for a test asserting "nothing was set explicitly
    /// here"; not the right tool for previewing a real spawn's outcome —
    /// use [`Spawner::registered_rt`](super::Spawner::registered_rt) for that.
    #[must_use]
    pub fn rt(&self) -> RtConfig {
        self.rt.unwrap_or_default()
    }

    /// The command line that will be executed.
    #[must_use]
    pub const fn command(&self) -> &CommandLine {
        &self.command
    }

    /// The node this request starts.
    #[must_use]
    pub const fn node(&self) -> &NodeId {
        &self.node
    }

    /// The incarnation this request starts.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// The command line a spec's source describes.
///
/// A [`NodeSource::Recorder`]'s destination `.arec` path is not one of
/// the manifest's own `args:` — it comes from the source itself, either
/// `record:` sugar's `{node id}.arec` default (§14) or the path an
/// `astrs record start` request supplied — so it is threaded through as
/// `astrs-record-node`'s first positional argument, ahead of whatever
/// `args:` the manifest declared. Every other source kind's `args:` are
/// untouched.
///
/// # Errors
///
/// As [`SpawnRequest::new`].
pub fn command_for(spec: &NodeSpawnSpec) -> DaemonResult<CommandLine> {
    let path = match &spec.source {
        NodeSource::Executable { path } => path.clone(),
        NodeSource::Runtime { .. } => "astrs-runtime".to_string(),
        NodeSource::Ros2Bridge { .. } => "astrs-ros2-bridge-node".to_string(),
        NodeSource::Recorder { .. } => "astrs-record-node".to_string(),
        NodeSource::Dynamic => {
            return Err(DaemonError::BadState {
                dataflow: spec.dataflow,
                state: "dynamic",
                operation: "spawn",
            });
        }
        // The wire enum is `#[non_exhaustive]`; a source this build does not
        // know how to launch is a configuration error, not a panic.
        other => {
            return Err(DaemonError::Manifest(format!(
                "node {} has an unsupported source kind {}",
                spec.node,
                other.kind_name()
            )));
        }
    };

    let mut args: Vec<String> = Vec::with_capacity(spec.args.len() + 1);
    if let NodeSource::Recorder { path: destination } = &spec.source {
        args.push(destination.clone());
    }
    args.extend(spec.args.iter().cloned());

    CommandLine::from_path_and_args(&path, args).map_err(|_| DaemonError::BadArgv {
        node: spec.node.clone(),
        input: path,
    })
}

/// A started child, with everything the daemon needs to supervise it.
#[derive(Debug)]
pub struct SpawnedProcess {
    /// The generation-stamped signalling handle.
    handle: ProcessHandle,
    /// The child, owned by whoever waits on it.
    child: Child,
    /// The child's stdout, when captured.
    stdout: Option<ChildStdout>,
    /// The child's stderr, when captured.
    stderr: Option<ChildStderr>,
    /// The environment the child received, for diagnostics.
    env: BuiltEnv,
    /// What happened when the request's `cpu_affinity` was applied (§11.3).
    cpu_affinity: CpuAffinityOutcome,
    /// What happened when the request's `rt` reservation was applied
    /// (§11.3, §22 hard-RT reservations).
    rt: RtOutcome,
}

impl SpawnedProcess {
    /// The signalling handle.
    #[must_use]
    pub const fn handle(&self) -> &ProcessHandle {
        &self.handle
    }

    /// What happened when the request's `cpu_affinity` was applied (§11.3):
    /// nothing was requested, it was pinned before the child's program ran,
    /// or this platform has no API to pin it with.
    #[must_use]
    pub const fn cpu_affinity(&self) -> CpuAffinityOutcome {
        self.cpu_affinity
    }

    /// What happened when the request's `rt` reservation was applied
    /// (§11.3, §22 hard-RT reservations): nothing was requested, it was
    /// applied before the child's own program ran, or this platform (or
    /// Linux architecture) has no application implemented here.
    #[must_use]
    pub const fn rt(&self) -> RtOutcome {
        self.rt
    }

    /// The process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.handle.pid()
    }

    /// The child, mutably — for a caller that waits on it in place.
    pub const fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// The environment the child received.
    #[must_use]
    pub const fn env(&self) -> &BuiltEnv {
        &self.env
    }

    /// Takes the captured stdout, if there is one.
    #[must_use]
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Takes the captured stderr, if there is one.
    #[must_use]
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// Splits into the handle and the parts a supervisor task needs.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        ProcessHandle,
        Child,
        Option<ChildStdout>,
        Option<ChildStderr>,
    ) {
        (self.handle, self.child, self.stdout, self.stderr)
    }
}

/// Starts node processes under one environment policy.
#[derive(Debug, Clone)]
pub struct Spawner {
    /// The §16 environment policy.
    policy: EnvPolicy,
    /// The dataflow working directory, used when a spec names none.
    working_dir: PathBuf,
    /// The daemon's own environment, scrubbed once at construction.
    ///
    /// Captured rather than read per spawn so a daemon that mutates its own
    /// environment mid-run (nothing should, but `set_var` exists) cannot
    /// change what an already-planned node inherits.
    inherited: BTreeMap<String, String>,
    /// Where a `cpu_affinity` request this platform cannot honor (§11.3) is
    /// counted. `None` still gets the WARN log — only the counter is
    /// skipped — so a caller that has not wired metrics in yet (a test, a
    /// `build:` line preview) never silently loses the log.
    metrics: Option<DaemonMetrics>,
    /// Per-`(dataflow, node)` `rt:` reservations (§11.3, §22 hard-RT
    /// reservations), consulted automatically by [`Spawner::spawn`].
    ///
    /// See this field's own module (`crate::spawn::rt`)'s top-level docs for
    /// why this table exists at all rather than a [`NodeSpawnSpec`] field:
    /// `astrs_wire::NodeSpawnSpec` is a byte-frozen wire type this crate
    /// cannot add a field to.
    ///
    /// `Arc<Mutex<_>>` rather than plain `BTreeMap`: [`Spawner`] derives
    /// [`Clone`] and every existing clone site
    /// (`dataflow::build`/`dataflow::fsm`/`coordinator::apply`) expects a
    /// cheap, shallow copy that keeps behaving like a builder — a plain
    /// `BTreeMap` field would make [`Spawner::register_rt`] observable only
    /// through the exact clone it was called on, silently splitting one
    /// logical spawner into two independent tables. `Arc<Mutex<_>>` rather
    /// than `Rc<RefCell<_>>`: [`crate::coordinator::apply`]'s own
    /// `build:`-line driver constructs a fresh [`Spawner`] and moves it
    /// into a `tokio::spawn`ed task, which requires the whole struct — this
    /// field included — to be `Send`. Contention is a non-issue: a
    /// `register_rt`/`spawn` pair are microseconds apart on the same
    /// dataflow's admit-then-run sequence, never a hot loop.
    rt_reservations: Arc<Mutex<BTreeMap<(DataflowId, NodeId), RtConfig>>>,
}

impl Spawner {
    /// A spawner with the blueprint policy, rooted at `working_dir`.
    #[must_use]
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self::with_policy(working_dir, EnvPolicy::new())
    }

    /// A spawner with an explicit policy.
    #[must_use]
    pub fn with_policy(working_dir: impl Into<PathBuf>, policy: EnvPolicy) -> Self {
        let inherited = crate::spawn::env::process_env();
        Self {
            policy,
            working_dir: working_dir.into(),
            inherited,
            metrics: None,
            rt_reservations: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Replaces the inherited environment — the seam a test uses to assert the
    /// scrub without touching the real process environment.
    #[must_use]
    pub fn with_inherited(mut self, inherited: BTreeMap<String, String>) -> Self {
        self.inherited = inherited;
        self
    }

    /// Wires in the registry a `cpu_affinity` request this platform cannot
    /// honor (§11.3) is counted against.
    ///
    /// Optional: without it, an unsupported request still logs its WARN (see
    /// [`crate::spawn::affinity`]) — only the counter is skipped.
    ///
    /// Takes `DaemonMetrics` by value rather than behind an `Arc`: every
    /// series it holds is already `Arc`-backed internally, so cloning it
    /// (what a caller reaching for `Arc::new` here would really want to
    /// avoid) is already cheap — see [`Daemon::metrics`](crate::server::core::Daemon::metrics).
    #[must_use]
    pub fn with_metrics(mut self, metrics: DaemonMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Registers `node`'s manifest-resolved `rt:` reservation (§11.3, §22
    /// hard-RT reservations), so every future [`Spawner::spawn`] for that
    /// `(dataflow, node)` pair applies it automatically — a caller building
    /// a [`SpawnRequest`] no longer needs to call [`SpawnRequest::with_rt`]
    /// itself.
    ///
    /// The intended caller is the plan → admit path, once for each node
    /// that carries a manifest `rt:` block, before the dataflow's nodes are
    /// first spawned (mirroring how `cpu_affinity` already reaches
    /// [`SpawnRequest::new`] straight from [`NodeSpawnSpec`] with no
    /// registration step at all — `rt` needs one only because it cannot
    /// ride the wire type; see `crate::spawn::rt`'s module docs). `&self`,
    /// not `&mut self`: every existing clone of this [`Spawner`] observes
    /// the registration (this struct's own private `rt_reservations` field
    /// docs say why), so a caller holding any clone — the build-time
    /// preview in
    /// [`crate::dataflow::build`], the admit-time one in
    /// [`crate::server::core::Daemon`] — can register without needing
    /// exclusive access.
    ///
    /// [`RtConfig::default`] is never worth registering explicitly — every
    /// `(dataflow, node)` pair with no entry already behaves identically
    /// (the ordinary time-sharing class) — so a caller reacting to a
    /// manifest node whose `rt` is `None` should call
    /// [`Self::forget_rt`] instead, or simply not call either: an
    /// unregistered pair and one explicitly registered at the default are
    /// indistinguishable at spawn time.
    pub fn register_rt(&self, dataflow: DataflowId, node: NodeId, rt: RtConfig) {
        self.rt_reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((dataflow, node), rt);
    }

    /// Removes a registered `rt:` reservation, if any — the counterpart of
    /// [`Self::register_rt`], for a node removed from a dataflow
    /// (`astrs node remove`, §5.2 dynamic topology) or replaced by one with
    /// no `rt:` block of its own (`astrs node replace`).
    ///
    /// A no-op, not an error, when nothing was registered for this pair —
    /// removing what was never there and removing what already ran once are
    /// both "there is now nothing registered", indistinguishably.
    pub fn forget_rt(&self, dataflow: DataflowId, node: &NodeId) {
        self.rt_reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(dataflow, node.clone()));
    }

    /// The `rt:` reservation currently registered for `(dataflow, node)`,
    /// or [`RtConfig::default`] (unpinned) if none was ever registered —
    /// exactly the value [`Spawner::spawn`] would apply for that pair right
    /// now. Public so a test, or an embedder previewing what a spawn would
    /// do, can inspect the table without spawning anything.
    #[must_use]
    pub fn registered_rt(&self, dataflow: DataflowId, node: &NodeId) -> RtConfig {
        self.rt_reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(dataflow, node.clone()))
            .copied()
            .unwrap_or_default()
    }

    /// The environment policy in force.
    #[must_use]
    pub const fn policy(&self) -> &EnvPolicy {
        &self.policy
    }

    /// The dataflow working directory.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Builds the environment a request's child would receive, without
    /// spawning anything.
    ///
    /// # Errors
    ///
    /// [`DaemonError::BadEnv`] if a manifest value references a variable the
    /// scrubbed base does not have.
    pub fn build_env(&self, request: &SpawnRequest) -> DaemonResult<BuiltEnv> {
        let mut env = self
            .policy
            .build(&self.inherited, &request.manifest_env)
            .map_err(|error| DaemonError::BadEnv {
                node: request.node.clone(),
                reason: error.to_string(),
            })?;
        env.merge_literal(&request.literal_env);
        request.owned.apply(&mut env);
        Ok(env)
    }

    /// The directory a request's child would run in.
    #[must_use]
    pub fn resolve_working_dir(&self, request: &SpawnRequest) -> PathBuf {
        match &request.working_dir {
            Some(dir) if dir.is_absolute() => dir.clone(),
            Some(dir) => self.working_dir.join(dir),
            None => self.working_dir.clone(),
        }
    }

    /// Builds the `Command` a request describes, without running it.
    ///
    /// Public so a test can assert on the assembled command, and so the build
    /// driver ([`crate::dataflow::build`]) can reuse the same assembly for a
    /// `build:` line.
    ///
    /// A thin wrapper over this type's own private `build_command_with_affinity`
    /// that drops the [`CpuAffinityOutcome`]/[`RtOutcome`] halves — this
    /// signature predates §11.3 and stays exactly as it was so a caller
    /// compiled against the old two-tuple keeps working.
    ///
    /// # Errors
    ///
    /// As [`Spawner::build_env`].
    pub fn build_command(
        &self,
        request: &SpawnRequest,
    ) -> DaemonResult<(tokio::process::Command, BuiltEnv)> {
        let (command, env, _cpu_affinity, _rt, _effective_rt) =
            self.build_command_with_affinity(request)?;
        Ok((command, env))
    }

    /// As [`Spawner::build_command`], plus what happened when `request`'s
    /// `cpu_affinity` (§11.3) and `rt` reservation (§11.3, §22) were each
    /// armed on the not-yet-spawned command. Both are independent
    /// `pre_exec` hooks on the same `command` — `std::process::Command`
    /// keeps a `Vec` of them, run in registration order, so arming one never
    /// displaces the other (see `spawn::rt::linux::arm`'s docs).
    ///
    /// Not `pub`: [`Spawner::spawn`] is the only caller that needs either
    /// outcome (to carry them on the resulting [`SpawnedProcess`]);
    /// everything else goes through [`Spawner::build_command`].
    ///
    /// # Errors
    ///
    /// As [`Spawner::build_env`].
    fn build_command_with_affinity(
        &self,
        request: &SpawnRequest,
    ) -> DaemonResult<(
        tokio::process::Command,
        BuiltEnv,
        CpuAffinityOutcome,
        RtOutcome,
        RtConfig,
    )> {
        let env = self.build_env(request)?;
        let working_dir = self.resolve_working_dir(request);
        let program = request.command.resolved_program(&working_dir);
        // Absolutized against *this* process's directory before the child is
        // told to `chdir` into `working_dir`. Both halves are needed and they
        // fight otherwise: `resolved_program` joins a relative `path:` onto a
        // relative working dir (`examples/demo` + `../../target/debug/node`),
        // and `current_dir` then makes the child resolve that same relative
        // result a *second* time, against the directory it just moved into —
        // so `astrs run examples/demo/dataflow.yml` from a repository root
        // failed with `ENOENT` while the identical manifest under an absolute
        // `--working-dir` worked. `std::path::absolute` is purely lexical (no
        // filesystem access, no symlink resolution), which is what makes it
        // safe to apply to a path that may not exist yet.
        //
        // A bare `cargo`/`sh` is left exactly as written: that is a `PATH`
        // lookup the operating system performs, and prefixing a directory
        // would turn it into a file that does not exist.
        let program = if request.command.is_path_lookup() {
            program
        } else {
            std::path::absolute(&program).unwrap_or(program)
        };

        let mut command = std::process::Command::new(program);
        command.args(request.command.args());
        command.current_dir(&working_dir);
        // The whole point of the allowlist: nothing survives that this
        // module did not put there.
        command.env_clear();
        command.envs(env.vars());
        command.stdin(Stdio::null());
        command.stdout(request.stdio.to_stdio());
        command.stderr(request.stdio.to_stdio());
        // Its own process group, so a node's own children die with it (§12).
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        // Armed here, on the `std::process::Command`, before it is wrapped as
        // a `tokio::process::Command` below — `pre_exec` (the race-free point
        // `affinity::arm`'s docs explain) is `std`'s own API (§11.3).
        let cpu_affinity = affinity::arm(
            &mut command,
            &request.cpu_affinity,
            request.dataflow,
            &request.node,
            self.metrics.as_ref(),
        );
        // A second, independent `pre_exec` hook on the same command (§11.3,
        // §22) — see this method's own docs for why arming both never
        // displaces either.
        //
        // `request.rt` (the `Option` field, not the always-defaulting
        // `SpawnRequest::rt()` accessor) wins whenever a caller set it
        // explicitly via `SpawnRequest::with_rt` — `Some(_)`, whatever value
        // it holds, including a deliberate `RtConfig::default()` overriding
        // a registered policy back to `SCHED_OTHER`. The registered table
        // ([`Self::register_rt`]) is consulted only as the `None` fallback,
        // for a request that never touched `rt` at all — see
        // `SpawnRequest::rt`'s own docs for why `Option` is what makes this
        // distinction expressible in the first place.
        let effective_rt = request
            .rt
            .unwrap_or_else(|| self.registered_rt(request.dataflow, &request.node));
        let rt = rt::arm(
            &mut command,
            effective_rt,
            request.dataflow,
            &request.node,
            self.metrics.as_ref(),
        );

        Ok((
            tokio::process::Command::from(command),
            env,
            cpu_affinity,
            rt,
            effective_rt,
        ))
    }

    /// Starts the process.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::BadEnv`] as [`Spawner::build_env`].
    /// - [`DaemonError::Spawn`] if the process cannot be started — a missing
    ///   binary, a working directory that does not exist, a permission
    ///   problem. RT was promised (§11.3, §22): a `rt:` reservation refused
    ///   with `EPERM` is one of these, never a silent fall-back to
    ///   `SCHED_OTHER` — see this crate's `spawn::rt` module's private
    ///   `enrich_spawn_error` function for the actionable
    ///   `CAP_SYS_NICE`/`RLIMIT_RTPRIO` message this wraps such a failure
    ///   with.
    pub fn spawn(&self, request: SpawnRequest) -> DaemonResult<SpawnedProcess> {
        let (mut command, env, cpu_affinity, rt, effective_rt) =
            self.build_command_with_affinity(&request)?;
        let mut child = command.spawn().map_err(|source| DaemonError::Spawn {
            node: request.node.clone(),
            program: request.command.program().to_string(),
            // `effective_rt` — not `request.rt` — because `rt::arm` above
            // was armed against whichever one actually won (an explicit
            // `SpawnRequest::with_rt`, or a `Spawner::register_rt` fallback:
            // see `build_command_with_affinity`'s own docs on that
            // precedence). Using `request.rt` here instead would silently
            // fail to enrich an `EPERM` that came from a registry-sourced
            // reservation the request itself never named.
            //
            // `rt::enrich_spawn_error` only rewrites `source` when it is
            // both `ErrorKind::PermissionDenied` and `effective_rt` actually
            // named a real-time policy `rt::arm` would have registered a
            // `pre_exec` hook for — every other failure (a missing binary,
            // `affinity::arm`'s own out-of-range-core `io::Error`, a bad
            // working directory) passes through completely unchanged. See
            // that function's own docs for why the message cannot be built
            // inside the `pre_exec` closure itself and must be attached
            // here instead, in the parent, after the real failure is known.
            source: rt::enrich_spawn_error(effective_rt, source),
        })?;
        // Recorded only once `command.spawn()` has actually returned `Ok`:
        // `rt::arm` only *arms* the `pre_exec` hook, and every hook on this
        // command already ran (in the child, before `exec()`) by the time
        // `spawn()` returns at all — if `sched_setscheduler` had failed
        // (`EPERM`), that error would have surfaced here as `DaemonError::Spawn`
        // instead, and this line would never run. So reaching this point
        // with `rt.was_applied()` true is not optimism, it is the
        // already-confirmed outcome.
        if rt.was_applied()
            && let Some(metrics) = self.metrics.as_ref()
        {
            metrics.record_rt_applied();
        }

        let pid = child.id().unwrap_or_default();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Ok(SpawnedProcess {
            handle: ProcessHandle::new(
                request.dataflow,
                request.node.clone(),
                request.generation,
                pid,
            ),
            child,
            stdout,
            stderr,
            env,
            cpu_affinity,
            rt,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{AuthToken, DaemonId};
    use tokio::io::AsyncReadExt;

    use super::*;

    fn spec(path: &str) -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("probe").unwrap(),
            0,
            NodeSource::Executable {
                path: path.to_string(),
            },
        )
    }

    fn config(spec: &NodeSpawnSpec) -> NodeConfig {
        NodeConfig::new(spec.clone(), DaemonId::generate(None), AuthToken::ZERO)
    }

    fn request(path: &str) -> SpawnRequest {
        let spec = spec(path);
        let config = config(&spec);
        SpawnRequest::new(&spec, &config).unwrap()
    }

    fn spawner() -> Spawner {
        Spawner::new(std::env::temp_dir()).with_inherited(BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("SECRET".to_string(), "leaked".to_string()),
        ]))
    }

    /// A relative `path:` under a relative working directory resolves once,
    /// not twice.
    ///
    /// The regression this guards: `build_command` both joins the program
    /// onto the working directory *and* asks the child to `chdir` into it, so
    /// a relative result was resolved a second time against the directory the
    /// child had just moved into. `astrs run examples/demo/dataflow.yml` from
    /// a repository root — the exact command every example's README prints —
    /// failed with `ENOENT` while an absolute `--working-dir` worked.
    #[test]
    fn a_relative_program_under_a_relative_working_dir_is_absolutised() {
        let spawner = Spawner::new(std::path::PathBuf::from("examples/demo"));
        let request = request("../../target/debug/node");
        let (command, _env) = spawner.build_command(&request).unwrap();
        let program = std::path::Path::new(command.as_std().get_program());
        assert!(program.is_absolute(), "{}", program.display());
        assert!(
            program.ends_with("target/debug/node"),
            "{}",
            program.display()
        );
    }

    /// A bare program name stays a `PATH` lookup, absolutisation or not.
    #[test]
    fn a_path_lookup_is_left_alone() {
        let spawner = Spawner::new(std::path::PathBuf::from("examples/demo"));
        let request = request("astrs-runtime");
        let (command, _env) = spawner.build_command(&request).unwrap();
        assert_eq!(command.as_std().get_program(), "astrs-runtime");
    }

    #[test]
    fn a_dynamic_node_is_not_spawnable() {
        let mut spec = spec("./x");
        spec.source = NodeSource::Dynamic;
        let config = config(&spec);
        assert!(matches!(
            SpawnRequest::new(&spec, &config),
            Err(DaemonError::BadState { .. })
        ));
    }

    #[test]
    fn the_command_comes_from_the_source_and_the_args() {
        let mut spec = spec("python3 -m mynode");
        spec.args = vec!["--fast".into()];
        let line = command_for(&spec).unwrap();
        assert_eq!(line.program(), "python3");
        assert_eq!(line.args(), ["-m", "mynode", "--fast"]);
    }

    #[test]
    fn an_unsplittable_path_is_refused() {
        let spec = spec(r#"./bin "unterminated"#);
        assert!(matches!(
            command_for(&spec),
            Err(DaemonError::BadArgv { .. })
        ));
    }

    #[test]
    fn runtime_and_bridge_sources_map_to_their_host_binaries() {
        let mut spec = spec("./x");
        spec.source = NodeSource::Runtime {
            operators: Vec::new(),
        };
        assert_eq!(command_for(&spec).unwrap().program(), "astrs-runtime");

        spec.source = NodeSource::Recorder {
            path: "out.arec".into(),
        };
        assert_eq!(command_for(&spec).unwrap().program(), "astrs-record-node");

        spec.source = NodeSource::Ros2Bridge {
            config: "{}".into(),
        };
        assert_eq!(
            command_for(&spec).unwrap().program(),
            "astrs-ros2-bridge-node"
        );
    }

    #[test]
    fn a_recorders_destination_path_is_the_first_positional_argument() {
        let mut spec = spec("./x");
        spec.args = vec!["--extra".into()];
        spec.source = NodeSource::Recorder {
            path: "sessions/out.arec".into(),
        };
        let line = command_for(&spec).unwrap();
        assert_eq!(line.program(), "astrs-record-node");
        assert_eq!(line.args(), ["sessions/out.arec", "--extra"]);
    }

    #[test]
    fn a_non_recorder_source_never_gains_an_extra_argument() {
        let mut spec = spec("./x");
        spec.args = vec!["--fast".into()];
        let line = command_for(&spec).unwrap();
        assert_eq!(line.args(), ["--fast"]);
    }

    #[test]
    fn the_built_environment_is_scrubbed_and_carries_the_blob() {
        let env = spawner().build_env(&request("/usr/bin/env")).unwrap();
        assert_eq!(env.get("PATH"), Some("/usr/bin:/bin"));
        assert!(!env.contains("SECRET"), "the scrub removed it");
        assert!(env.contains(astrs_wire::ENV_NODE_CONFIG));
    }

    #[test]
    fn the_working_directory_falls_back_to_the_dataflow_root() {
        let spawner = Spawner::new("/workspace");
        let plain = request("./x");
        assert_eq!(
            spawner.resolve_working_dir(&plain),
            PathBuf::from("/workspace")
        );

        let relative = request("./x").with_working_dir("sub");
        assert_eq!(
            spawner.resolve_working_dir(&relative),
            PathBuf::from("/workspace/sub")
        );

        let absolute = request("./x").with_working_dir("/elsewhere");
        assert_eq!(
            spawner.resolve_working_dir(&absolute),
            PathBuf::from("/elsewhere")
        );
    }

    #[test]
    fn stdio_modes_classify_themselves() {
        assert!(StdioMode::Capture.is_captured());
        assert!(!StdioMode::Inherit.is_captured());
        assert!(!StdioMode::Null.is_captured());
        assert_eq!(StdioMode::default(), StdioMode::Capture);
    }

    #[tokio::test]
    async fn a_spawned_child_sees_exactly_the_built_environment() {
        let request = request("/usr/bin/env").with_stdio(StdioMode::Capture);
        let spawner = spawner();
        let expected = spawner.build_env(&request).unwrap();
        let mut spawned = spawner.spawn(request).unwrap();

        let mut stdout = spawned.take_stdout().expect("captured");
        let mut text = String::new();
        stdout.read_to_string(&mut text).await.unwrap();
        let status = spawned.child_mut().wait().await.unwrap();
        assert!(status.success());

        let seen: BTreeMap<&str, &str> = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        assert_eq!(seen.get("PATH"), Some(&"/usr/bin:/bin"));
        assert!(!seen.contains_key("SECRET"), "{text}");
        assert!(!seen.contains_key("LD_PRELOAD"), "{text}");
        assert_eq!(
            seen.get(astrs_wire::ENV_NODE_CONFIG).copied(),
            expected.get(astrs_wire::ENV_NODE_CONFIG)
        );
    }

    #[tokio::test]
    async fn a_missing_binary_reports_the_program_it_tried() {
        let request = request("/nonexistent/astrs-probe");
        let error = spawner().spawn(request).unwrap_err();
        match &error {
            DaemonError::Spawn { program, node, .. } => {
                assert_eq!(program, "/nonexistent/astrs-probe");
                assert_eq!(node.as_str(), "probe");
            }
            other => panic!("expected a spawn error, got {other}"),
        }
        assert!(!error.is_client_error());
    }

    #[tokio::test]
    async fn a_child_runs_in_its_own_process_group() {
        let request = request("/bin/sh").with_stdio(StdioMode::Null);
        // `/bin/sh` with no arguments reads stdin, which is /dev/null, and
        // exits — long enough to look at its process group.
        let mut spawned = spawner().spawn(request).unwrap();
        let pid = spawned.pid();
        assert_ne!(pid, 0);
        let _ = spawned.child_mut().wait().await;
        // The group id equals the child's own pid when `process_group(0)`
        // took effect. Reading it after the wait is racy, so assert on the
        // spawn path having asked for it instead: the handle addresses the
        // group, and killing a reaped handle is a no-op.
        assert!(spawned.handle().pid() == pid);
    }

    #[tokio::test]
    async fn the_handle_addresses_the_generation_that_was_spawned() {
        let mut spec = spec("/usr/bin/true");
        spec.generation = 7;
        let config = config(&spec);
        let request = SpawnRequest::new(&spec, &config)
            .unwrap()
            .with_stdio(StdioMode::Null);
        let mut spawned = spawner().spawn(request).unwrap();
        assert_eq!(spawned.handle().generation(), 7);
        let _ = spawned.child_mut().wait().await;
    }

    #[tokio::test]
    async fn into_parts_hands_over_the_child_and_its_streams() {
        let request = request("/usr/bin/true").with_stdio(StdioMode::Capture);
        let spawned = spawner().spawn(request).unwrap();
        let (handle, mut child, stdout, stderr) = spawned.into_parts();
        assert!(stdout.is_some());
        assert!(stderr.is_some());
        assert!(handle.pid() > 0);
        let _ = child.wait().await;
    }

    #[test]
    fn a_request_built_from_parts_carries_no_manifest_environment() {
        let spec = spec("/usr/bin/true");
        let owned = DaemonOwnedVars::new(&config(&spec)).unwrap();
        let request = SpawnRequest::from_parts(
            spec.dataflow,
            spec.node.clone(),
            3,
            CommandLine::new("/usr/bin/true", Vec::<String>::new()),
            owned,
        );
        assert_eq!(request.generation(), 3);
        assert_eq!(request.node().as_str(), "probe");
        assert_eq!(request.command().program(), "/usr/bin/true");
    }

    #[test]
    fn manifest_environment_reaches_the_built_environment() {
        let request = request("/usr/bin/env").with_manifest_env(BTreeMap::from([
            ("CAMERA".to_string(), EnvValue::Int(3)),
            ("EVIL".to_string(), EnvValue::String("x".into())),
        ]));
        let env = spawner().build_env(&request).unwrap();
        assert_eq!(env.get("CAMERA"), Some("3"));
        assert_eq!(env.get("EVIL"), Some("x"));
    }

    #[test]
    fn an_unresolvable_manifest_reference_is_a_bad_env_error() {
        let request = request("/usr/bin/env").with_manifest_env(BTreeMap::from([(
            "LEAK".to_string(),
            EnvValue::String("$SECRET".into()),
        )]));
        let error = spawner().build_env(&request).unwrap_err();
        assert!(matches!(error, DaemonError::BadEnv { .. }), "{error}");
        assert!(error.is_client_error());
    }

    // ------------------------------------------------------ cpu_affinity (§11.3)

    #[test]
    fn a_spawn_request_carries_the_specs_cpu_affinity() {
        let mut spec = spec("/usr/bin/true");
        spec.cpu_affinity = vec![0, 2];
        let config = config(&spec);
        let request = SpawnRequest::new(&spec, &config).unwrap();
        assert_eq!(request.cpu_affinity(), &[0, 2]);
    }

    #[test]
    fn from_parts_and_the_builder_start_unpinned_then_can_be_set() {
        let owned = DaemonOwnedVars::new(&config(&spec("/usr/bin/true"))).unwrap();
        let request = SpawnRequest::from_parts(
            DataflowId::from_u128(1),
            NodeId::new("probe").unwrap(),
            0,
            CommandLine::new("/usr/bin/true", Vec::<String>::new()),
            owned,
        );
        assert!(request.cpu_affinity().is_empty());
        let pinned = request.with_cpu_affinity(vec![1]);
        assert_eq!(pinned.cpu_affinity(), &[1]);
    }

    #[tokio::test]
    async fn no_cpu_affinity_requested_is_reported_as_such() {
        let request = request("/usr/bin/true").with_stdio(StdioMode::Null);
        let mut spawned = spawner().spawn(request).unwrap();
        assert_eq!(spawned.cpu_affinity(), CpuAffinityOutcome::NotRequested);
        let _ = spawned.child_mut().wait().await;
    }

    /// The Linux path applies the pin *to the spawned child itself* — this is
    /// the only test in this module that observes the effect from outside
    /// the `pre_exec` closure (which runs in a process this test cannot
    /// otherwise inspect).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_spawn_applies_the_requested_affinity_to_the_child() {
        // A core this test process is actually allowed to run on, so the
        // request cannot fail with `EINVAL` on a sandboxed/cgroup-limited CI
        // runner that does not have core 0.
        let allowed = rustix::thread::sched_getaffinity(None).unwrap();
        let Some(core) = (0..rustix::thread::CpuSet::MAX_CPU).find(|&c| allowed.is_set(c)) else {
            panic!("this process's own affinity mask names no CPU at all");
        };
        let core = u16::try_from(core).expect("a real core index fits u16");

        // `sleep 5` outlives the window this test needs to read its affinity
        // back before killing it — a short-lived child could already be
        // reaped by the time `sched_getaffinity(Some(pid))` runs.
        let owned = DaemonOwnedVars::new(&config(&spec("/bin/sleep"))).unwrap();
        let request = SpawnRequest::from_parts(
            DataflowId::from_u128(1),
            NodeId::new("probe").unwrap(),
            0,
            CommandLine::new("/bin/sleep", vec!["5".to_string()]),
            owned,
        )
        .with_cpu_affinity(vec![core])
        .with_stdio(StdioMode::Null);

        let mut spawned = spawner().spawn(request).unwrap();
        assert_eq!(spawned.cpu_affinity(), CpuAffinityOutcome::Applied);

        let pid = rustix::process::Pid::from_raw(i32::try_from(spawned.pid()).unwrap())
            .expect("a freshly spawned pid is never zero");
        let observed = rustix::thread::sched_getaffinity(Some(pid))
            .expect("the child is still alive: it is sleeping, and nothing has reaped it yet");
        assert!(observed.is_set(core as usize), "{observed:?}");
        assert_eq!(observed.count(), 1, "only the requested core is set");

        let _ = spawned.handle().kill();
        let _ = spawned.child_mut().wait().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_out_of_range_cpu_core_fails_the_spawn_cleanly_not_a_panic() {
        let request = request("/usr/bin/true")
            .with_stdio(StdioMode::Null)
            .with_cpu_affinity(vec![u16::MAX]);
        // A `CpuSet::MAX_CPU`-sized bitmask cannot possibly hold index
        // 65535: this must fail as an ordinary `DaemonError::Spawn` (the
        // `pre_exec` hook's own `io::Error`, relayed by `Command::spawn()`),
        // never panic mid-fork — reaching this `assert` at all is half the
        // proof; the `matches!` is the other half.
        let error = spawner().spawn(request).unwrap_err();
        assert!(matches!(error, DaemonError::Spawn { .. }), "{error}");
    }

    // ---------------------------------------------------- rt (§11.3, §22 hard-RT reservations)

    /// `SpawnRequest::new` always starts unpinned, regardless of anything on
    /// `spec` — unlike `cpu_affinity`, `rt` is not read from
    /// [`NodeSpawnSpec`] at all (see [`SpawnRequest::new`]'s own docs for
    /// why: `astrs_wire::NodeSpawnSpec` is a byte-frozen wire type this
    /// crate cannot add a field to). A caller with a manifest-resolved
    /// [`RtConfig`] must call [`SpawnRequest::with_rt`] itself.
    #[test]
    fn a_spawn_request_from_a_spec_always_starts_unpinned() {
        let request = request("/usr/bin/true");
        assert_eq!(request.rt(), RtConfig::default());
    }

    #[test]
    fn from_parts_and_the_builder_start_with_no_rt_then_can_be_set() {
        let owned = DaemonOwnedVars::new(&config(&spec("/usr/bin/true"))).unwrap();
        let request = SpawnRequest::from_parts(
            DataflowId::from_u128(1),
            NodeId::new("probe").unwrap(),
            0,
            CommandLine::new("/usr/bin/true", Vec::<String>::new()),
            owned,
        );
        assert_eq!(request.rt(), RtConfig::default());
        let rt = RtConfig {
            policy: astrs_manifest::RtPolicy::Rr,
            priority: Some(10),
        };
        let pinned = request.with_rt(rt);
        assert_eq!(pinned.rt(), rt);
    }

    #[test]
    fn an_unregistered_pair_reads_back_as_the_default() {
        let spawner = spawner();
        assert_eq!(
            spawner.registered_rt(DataflowId::from_u128(1), &NodeId::new("a").unwrap()),
            RtConfig::default()
        );
    }

    #[test]
    fn register_rt_is_readable_through_registered_rt() {
        let spawner = spawner();
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("control-loop").unwrap();
        let rt = RtConfig {
            policy: astrs_manifest::RtPolicy::Fifo,
            priority: Some(90),
        };
        spawner.register_rt(dataflow, node.clone(), rt);
        assert_eq!(spawner.registered_rt(dataflow, &node), rt);

        // A different node, or a different dataflow with the same node id,
        // is unaffected — the table keys on the full pair, not either half.
        assert_eq!(
            spawner.registered_rt(dataflow, &NodeId::new("other").unwrap()),
            RtConfig::default()
        );
        assert_eq!(
            spawner.registered_rt(DataflowId::from_u128(2), &node),
            RtConfig::default()
        );
    }

    #[test]
    fn forget_rt_clears_a_registration_and_is_a_no_op_on_an_absent_one() {
        let spawner = spawner();
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("control-loop").unwrap();
        spawner.register_rt(
            dataflow,
            node.clone(),
            RtConfig {
                policy: astrs_manifest::RtPolicy::Rr,
                priority: Some(50),
            },
        );
        spawner.forget_rt(dataflow, &node);
        assert_eq!(spawner.registered_rt(dataflow, &node), RtConfig::default());

        // Forgetting again — nothing registered — does not panic.
        spawner.forget_rt(dataflow, &node);
    }

    #[test]
    fn every_clone_of_a_spawner_shares_one_rt_table() {
        // The whole reason `rt_reservations` is `Arc<Mutex<_>>` rather than
        // a plain `BTreeMap` field: `Spawner` derives `Clone`, and every
        // production call site (`dataflow::build`, `dataflow::fsm`,
        // `coordinator::apply`) expects a shallow copy that still shares
        // registration state, not an independent table each clone forgets.
        let original = spawner();
        let clone = original.clone();
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("control-loop").unwrap();
        let rt = RtConfig {
            policy: astrs_manifest::RtPolicy::Fifo,
            priority: Some(30),
        };

        clone.register_rt(dataflow, node.clone(), rt);
        assert_eq!(
            original.registered_rt(dataflow, &node),
            rt,
            "a clone's registration must be visible through the original"
        );
    }

    /// A request that never called `with_rt` at all picks up whatever the
    /// registry holds for its `(dataflow, node)` pair — the whole point of
    /// [`Spawner::register_rt`]: a caller that only knows how to build an
    /// ordinary [`SpawnRequest`] (the ordinary `spawn_node` path, once
    /// wired) still gets the manifest's `rt:` applied.
    ///
    /// Handles all three reachable spawn outcomes explicitly, the same way
    /// `rt_metrics_are_recorded_through_a_real_spawn` does — which one is
    /// real depends on this process's own platform/capabilities, and every
    /// branch is still proof the registry (not `request.rt`, which stayed
    /// at its default the whole time) is what `rt::arm` actually saw.
    #[tokio::test]
    async fn a_registered_reservation_applies_without_an_explicit_with_rt_call() {
        let spawner = spawner();
        let dataflow = DataflowId::from_u128(1);
        spawner.register_rt(
            dataflow,
            NodeId::new("probe").unwrap(),
            RtConfig {
                policy: astrs_manifest::RtPolicy::Fifo,
                priority: Some(10),
            },
        );

        let mut spec = spec("/usr/bin/true");
        spec.dataflow = dataflow;
        let config = config(&spec);
        let request = SpawnRequest::new(&spec, &config)
            .unwrap()
            .with_stdio(StdioMode::Null);
        // Confirms the request itself never named anything explicit — the
        // outcome below can only have come from the registry.
        assert_eq!(request.rt(), RtConfig::default());

        match spawner.spawn(request) {
            Ok(mut spawned) => {
                assert_ne!(
                    spawned.rt(),
                    RtOutcome::NotRequested,
                    "a registered rt: must be seen even though this request never \
                     called with_rt"
                );
                let _ = spawned.handle().kill();
                let _ = spawned.child_mut().wait().await;
            }
            Err(error) => {
                // `EPERM`, on a platform that does implement `sched_setscheduler`
                // but this process lacks `CAP_SYS_NICE`/a permissive
                // `RLIMIT_RTPRIO` — still proof the registry was consulted:
                // `rt::arm` only ever arms a real `pre_exec` hook (and can
                // therefore only ever produce this failure) when it received
                // something other than `RtConfig::default()`, and this
                // request's own `rt` never left that default.
                assert!(matches!(error, DaemonError::Spawn { .. }), "{error}");
            }
        }
    }

    /// An explicit [`SpawnRequest::with_rt`] always wins over whatever is
    /// registered for the same pair — see `Spawner::build_command_with_affinity`'s
    /// own docs on this precedence.
    #[tokio::test]
    async fn an_explicit_with_rt_call_overrides_the_registry() {
        let spawner = spawner();
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("probe").unwrap();
        // Registered: a real-time policy this (likely unprivileged) test
        // process cannot actually apply.
        spawner.register_rt(
            dataflow,
            node.clone(),
            RtConfig {
                policy: astrs_manifest::RtPolicy::Fifo,
                priority: Some(10),
            },
        );

        let mut spec = spec("/usr/bin/true");
        spec.dataflow = dataflow;
        let config = config(&spec);
        // Explicit `RtConfig::default()` — a deliberate "pin this node back
        // to SCHED_OTHER" call, not the same thing as never calling
        // `with_rt` at all (see `SpawnRequest::rt`'s own docs for why the
        // field is `Option<RtConfig>` specifically so this distinction is
        // expressible). This is the exact case the earlier, `RtConfig`-only
        // design got wrong: comparing the request's resolved value against
        // `RtConfig::default()` could never tell an explicit override
        // apart from an unset field, so it silently fell through to the
        // registry here instead of honoring the override.
        let request = SpawnRequest::new(&spec, &config)
            .unwrap()
            .with_stdio(StdioMode::Null)
            .with_rt(RtConfig::default());
        let mut spawned = spawner.spawn(request).unwrap();
        assert_eq!(
            spawned.rt(),
            RtOutcome::NotRequested,
            "an explicit default must still win over a registered real-time policy"
        );
        let _ = spawned.child_mut().wait().await;
    }

    #[tokio::test]
    async fn no_rt_requested_is_reported_as_such() {
        let request = request("/usr/bin/true").with_stdio(StdioMode::Null);
        let mut spawned = spawner().spawn(request).unwrap();
        assert_eq!(spawned.rt(), RtOutcome::NotRequested);
        let _ = spawned.child_mut().wait().await;
    }

    /// Wires a real [`DaemonMetrics`] registry through [`Spawner::spawn`]
    /// and checks that exactly the counter matching the real outcome moved —
    /// `rt::arm` itself is exercised with `metrics: None` by `spawn::rt`'s
    /// own tests (which is where the WARN/outcome logic is proven); this is
    /// the one place [`Spawner::with_metrics`] itself is exercised for `rt`
    /// at all, so a broken wire (the metrics handle never reaching
    /// `rt::arm`, or the post-spawn `record_rt_applied` call being lost)
    /// would otherwise pass every other test in this file silently.
    ///
    /// All three reachable outcomes are handled explicitly rather than
    /// picking one: which one is real depends on this process's own
    /// capabilities (Linux x86_64/aarch64 with `CAP_SYS_NICE`/a permissive
    /// `RLIMIT_RTPRIO` applies; the same platform without either fails with
    /// `EPERM`; every other platform reports unsupported) — see
    /// `spawn::rt::arm`'s own docs for why. Whichever branch runs, it is
    /// deterministic for a given environment, not a flaky race between them.
    #[tokio::test]
    async fn rt_metrics_are_recorded_through_a_real_spawn() {
        let metrics = DaemonMetrics::new();
        let request = request("/usr/bin/true")
            .with_stdio(StdioMode::Null)
            .with_rt(RtConfig {
                policy: astrs_manifest::RtPolicy::Fifo,
                priority: Some(10),
            });
        let outcome = spawner().with_metrics(metrics.clone()).spawn(request);

        let batch = metrics.snapshot(astrs_time::HlcTimestamp::new(1, 0));
        let value = |name: &str| {
            batch
                .points
                .iter()
                .find(|point| point.name == name)
                .map(|point| point.value.as_f64())
        };
        let applied = value(crate::metrics::names::RT_APPLIED_TOTAL);
        let unsupported = value(crate::metrics::names::RT_UNSUPPORTED_TOTAL);

        match outcome {
            Ok(mut spawned) => {
                match spawned.rt() {
                    RtOutcome::Applied => {
                        assert_eq!(applied, Some(1.0));
                        assert_eq!(unsupported, Some(0.0));
                    }
                    RtOutcome::UnsupportedPlatform => {
                        assert_eq!(applied, Some(0.0));
                        assert_eq!(unsupported, Some(1.0));
                    }
                    RtOutcome::NotRequested => panic!("this request named a real-time policy"),
                }
                let _ = spawned.handle().kill();
                let _ = spawned.child_mut().wait().await;
            }
            Err(error) => {
                // `EPERM`: `Spawner::spawn` returns before its post-success
                // metrics line ever runs, and `rt::arm` on this (supported)
                // platform never touches `rt_unsupported_total` either.
                assert!(matches!(error, DaemonError::Spawn { .. }), "{error}");
                assert_eq!(applied, Some(0.0));
                assert_eq!(unsupported, Some(0.0));
            }
        }
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[tokio::test]
    async fn an_invalid_rt_priority_is_defensively_unapplied_not_a_spawn_failure() {
        // `Manifest::validate` never lets this shape reach a real spawn (see
        // `spawn::rt::effective_priority`'s docs) — this proves the
        // defense-in-depth path all the way through `Spawner::spawn`, not
        // just `rt::arm` in isolation.
        let request = request("/usr/bin/true")
            .with_stdio(StdioMode::Null)
            .with_rt(RtConfig {
                policy: astrs_manifest::RtPolicy::Fifo,
                priority: None,
            });
        let mut spawned = spawner().spawn(request).unwrap();
        assert_eq!(spawned.rt(), RtOutcome::NotRequested);
        let _ = spawned.child_mut().wait().await;
    }

    /// The manifest → spawn config link, proven from literal YAML text
    /// rather than a hand-built [`RtConfig`] — nothing else in this crate
    /// starts from manifest text on the daemon side. Cross-platform: no
    /// `cfg` gate, because reading a parsed `rt:` block into a
    /// [`SpawnRequest`] never touches a syscall — only
    /// [`crate::spawn::rt::arm`] (exercised separately, per-platform) does.
    #[test]
    fn manifest_yaml_text_survives_into_a_spawn_requests_rt() {
        let manifest = astrs_manifest::Manifest::from_yaml_str(
            "nodes:\n  - id: control-loop\n    path: /usr/bin/true\n    rt:\n      policy: fifo\n      priority: 80\n",
        )
        .unwrap();
        manifest.validate().unwrap();
        let rt = manifest.nodes[0]
            .rt
            .expect("the manifest above names an rt: block");

        let request = request("/usr/bin/true").with_rt(rt);
        assert_eq!(
            request.rt(),
            RtConfig {
                policy: astrs_manifest::RtPolicy::Fifo,
                priority: Some(80),
            }
        );
    }

    /// As the test above, for a manifest node that names no `rt:` block at
    /// all — the common case, and the one [`SpawnRequest::new`] itself
    /// already covers without any [`SpawnRequest::with_rt`] call.
    #[test]
    fn a_manifest_node_with_no_rt_block_parses_as_none() {
        let manifest =
            astrs_manifest::Manifest::from_yaml_str("nodes:\n  - id: a\n    path: /usr/bin/true\n")
                .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.nodes[0].rt, None);
    }
}
