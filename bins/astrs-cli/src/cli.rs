//! The `astrs` clap derive tree (blueprint §17): the full verb set, with
//! a real argument schema on every subcommand — so `--help`, argument
//! validation and shell completion (`astrs completion`) describe the whole
//! surface, including the handful of verbs whose implementation belongs to
//! a later wave and which report
//! [`crate::error::CliError::NotImplementedYet`] when invoked (see
//! [`crate::command::stub`]).
//!
//! `--json` and `--color` are global flags (available on, and inherited
//! by, every subcommand — blueprint §17: "All output has `--json` for
//! scripting") rather than repeated on each leaf args struct.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::command::completion::CompletionShell;
use crate::command::graph::GraphFormat;
use crate::command::new::{NewKind, NewLang};

/// When to colorize human-readable output with plain ANSI SGR codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ColorMode {
    /// Colorize only when the output sink is a terminal.
    #[default]
    Auto,
    /// Always colorize.
    Always,
    /// Never colorize.
    Never,
}

/// The `astrs` command line: run, build, monitor, migrate and bridge
/// robotic dataflows (blueprint §17).
#[derive(Debug, Parser)]
#[command(name = "astrs", version, about, long_about = None)]
pub struct Cli {
    /// Emit machine-readable JSON instead of human-readable text, on
    /// every subcommand that produces a report.
    #[arg(long, global = true)]
    pub json: bool,
    /// When to colorize human-readable output.
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,
    /// The verb to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The full `astrs` verb set (blueprint §17).
#[derive(Debug, Subcommand)]
pub enum Command {
    // ---- Lifecycle ----------------------------------------------------
    /// Run a dataflow in single-process mode (embeds an in-process
    /// daemon; no coordinator).
    Run(RunArgs),
    /// Start a dataflow cluster (coordinator + daemons already running).
    Up(UpArgs),
    /// Tear down a running dataflow cluster.
    Down(DownArgs),
    /// Build every node in a manifest (resolves `git:` sources via the
    /// system `git` binary, then runs each node's `build:` command).
    Build(BuildArgs),
    /// Start a dataflow on an already-running cluster.
    Start(StartArgs),
    /// Stop a running dataflow, leaving the cluster up.
    Stop(StopArgs),
    /// Stop then restart a running dataflow.
    Restart(RestartArgs),
    /// Stop a dataflow and release every resource it held.
    Destroy(DestroyArgs),
    /// Reclaim the resources of dataflows that have already finished.
    Clean(CleanArgs),

    // ---- Monitoring -----------------------------------------------------
    /// List known dataflows.
    List(ListArgs),
    /// Tail (or dump) a dataflow's logs.
    Logs(LogsArgs),
    /// Launch the live TUI monitor.
    Top(TopArgs),
    /// Inspect a running topic.
    #[command(subcommand)]
    Topic(TopicCommand),
    /// Show a dataflow's (or node's) current status.
    Status(StatusArgs),
    /// Print a causal (HLC-ordered) event trace.
    Trace(TraceArgs),

    // ---- Graph ops ------------------------------------------------------
    /// Parse, expand and type-check a manifest, reporting every
    /// diagnostic found.
    Validate(ValidateCliArgs),
    /// Print a manifest with every `module:`-sourced node flattened.
    Expand(ExpandCliArgs),
    /// Visualize a manifest's dataflow graph.
    Graph(GraphCliArgs),
    /// Add, remove or replace a node in a running dataflow.
    #[command(subcommand)]
    Node(NodeCommand),
    /// Get, set, list or delete a running dataflow's parameters.
    #[command(subcommand)]
    Param(ParamCommand),

    // ---- Data -----------------------------------------------------------
    /// Start or stop recording a dataflow's outputs to a `.arec` file.
    #[command(subcommand)]
    Record(RecordCommand),
    /// Replay a `.arec` recording as live sources.
    Replay(ReplayArgs),
    /// Convert or inspect a rosbag2 (`.db3`/`.mcap`) file.
    #[command(subcommand)]
    Bag(BagCommand),

    // ---- ROS 2 ------------------------------------------------------------
    /// ROS 2 interop diagnostics.
    #[command(subcommand)]
    Ros2(Ros2Command),

    // ---- Dev --------------------------------------------------------------
    /// Scaffold a new node, operator, or graph manifest.
    #[command(subcommand)]
    New(NewCommand),
    /// Migrate a manifest from another dataflow framework.
    #[command(subcommand)]
    Migrate(MigrateCommand),
    /// Check the local environment for AstRS-readiness.
    Doctor(DoctorCliArgs),
    /// Generate a shell completion script.
    Completion(CompletionCliArgs),
    /// Emit the manifest JSON schema.
    Schema(SchemaCliArgs),
    /// Mint a scoped cluster credential from the root token (blueprint §16,
    /// §22).
    #[command(subcommand)]
    Token(TokenCommand),
    /// Manage the local AstRS package index (blueprint §22): update,
    /// search, inspect, or scaffold one for self-hosting.
    #[command(subcommand)]
    Hub(HubCommand),

    // ---- Hidden internals -------------------------------------------------
    /// (internal) Run the per-machine daemon process.
    #[command(hide = true)]
    Daemon(DaemonArgs),
    /// (internal) Run the cluster coordinator process.
    #[command(hide = true)]
    Coordinator(CoordinatorArgs),
    /// (internal) Run an operator-hosting runtime process.
    #[command(hide = true)]
    Runtime(RuntimeArgs),
}

// =========================================================================
// Lifecycle
// =========================================================================

/// Where a client verb finds the coordinator, and how it authenticates —
/// flattened into every verb that talks to one so the four flags are
/// spelled, parsed and documented exactly once (blueprint §16, §24.2).
#[derive(Debug, Args, Clone, Default)]
pub struct ConnectArgs {
    /// The coordinator address (`host[:port]`, or a bare port); defaults to
    /// `ASTRS_COORDINATOR_ADDR`, then loopback on `ASTRS_COORDINATOR_PORT`
    /// (7407).
    #[arg(long)]
    pub coordinator: Option<String>,
    /// The 64-hex cluster token (§16). Prefer `--token-file`: an argument
    /// is visible in `ps`.
    #[arg(long)]
    pub token: Option<String>,
    /// Read the cluster token from this file instead of
    /// `<working-dir>/.astrs-token`.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// The directory holding `.astrs-token`; defaults to the process's
    /// current directory (§16).
    #[arg(long)]
    pub working_dir: Option<PathBuf>,
}

/// `astrs run` arguments.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The manifest to run.
    pub manifest: PathBuf,
    /// Drive the timer wheel from a recorded HLC stream instead of the
    /// wall clock, for byte-identical replay (blueprint §14). Needs
    /// `--from-recording`; refused with
    /// [`crate::error::CliError::DeterministicNeedsRecording`] without it.
    #[arg(long)]
    pub deterministic: bool,
    /// The `.arec` recording `--deterministic` replays as its clock source.
    /// Every node the recording produced for is stood down (`path:
    /// dynamic`) and fed from it instead of spawned.
    #[arg(long, value_name = "PATH")]
    pub from_recording: Option<PathBuf>,
    /// Paces `--deterministic`'s wall-clock walk through the recording:
    /// `1.0` reproduces the original spacing, `2.0` halves it. Omit to
    /// replay as fast as the loop can — pacing never changes which
    /// messages are delivered or their stamps, only how long the run takes.
    #[arg(long, value_name = "FACTOR")]
    pub speed: Option<f64>,
    /// Exit once every node with a finite input set has finished, whatever
    /// the manifest's own `exit_when_nodes_finish:` says.
    #[arg(long)]
    pub exit_when_nodes_finish: bool,
    /// Skip the manifest's `build:` lines and go straight to spawning.
    #[arg(long)]
    pub skip_build: bool,
    /// Hide streamed node output below this level
    /// (`trace|debug|info|warn|error`).
    #[arg(long, value_name = "LEVEL")]
    pub level: Option<String>,
    /// The directory node paths, `build:` lines and relative `env:` values
    /// resolve against; defaults to the manifest's own directory.
    #[arg(long)]
    pub working_dir: Option<PathBuf>,
    /// Where the embedded daemon puts its socket and captured logs;
    /// defaults to `$ASTRS_RUNTIME_DIR`/`$XDG_RUNTIME_DIR/astrs`.
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,
    /// Stop the whole graph after this many seconds, however it is doing.
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<f64>,
    /// How long a stopping node has to finish before `SIGTERM` (§12).
    #[arg(long, value_name = "SECONDS")]
    pub grace: Option<f64>,
}

/// `astrs up` arguments.
///
/// Brings up the *cluster* — a coordinator and this machine's daemon — not
/// a dataflow; `astrs start` runs dataflows on it (blueprint §17).
#[derive(Debug, Args)]
pub struct UpArgs {
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// The port the coordinator should listen on; `0` binds a free one.
    #[arg(long)]
    pub port: Option<u16>,
    /// Where pidfiles and sockets live; defaults to
    /// `$ASTRS_RUNTIME_DIR`/`$XDG_RUNTIME_DIR/astrs` (§24.2).
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,
    /// Delete the coordinator's parameter/state store before starting, so
    /// the cluster comes up with nothing remembered (§4.2).
    #[arg(long)]
    pub recreate_store: bool,
    /// Start the coordinator only, leaving this machine without a daemon.
    #[arg(long)]
    pub no_daemon: bool,
}

/// `astrs down` arguments.
#[derive(Debug, Args)]
pub struct DownArgs {
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Where the pidfiles to read live (§24.2).
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,
    /// Tear the cluster down even while dataflows are still running.
    #[arg(long)]
    pub force: bool,
}

/// `astrs build` arguments.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// The manifest whose nodes should be built.
    pub manifest: PathBuf,
    /// Append `--release` to every `build:` line — the common
    /// `cargo build` case. A build line that does not accept the flag will
    /// fail, so spell the profile out in the manifest when in doubt.
    #[arg(long)]
    pub release: bool,
    /// Only build this one node id (default: every node).
    #[arg(long)]
    pub node: Option<String>,
    /// The directory `build:` lines run in; defaults to the manifest's own
    /// directory.
    #[arg(long)]
    pub working_dir: Option<PathBuf>,
    /// Where per-node build logs are written; defaults to
    /// `$ASTRS_RUNTIME_DIR`/`$XDG_RUNTIME_DIR/astrs` (§24.2).
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,
}

/// `astrs start` arguments.
#[derive(Debug, Args)]
pub struct StartArgs {
    /// The manifest to start on the already-running cluster.
    pub manifest: PathBuf,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// A name for the run, for `astrs list` and `astrs stop <name>`.
    #[arg(long)]
    pub name: Option<String>,
    /// Override where nodes run (§8.3 `deploy.machine`), repeatable.
    ///
    /// `--machine robot-1` places every node the manifest left unplaced;
    /// `--machine planner=robot-2` pins one node, overriding whatever the
    /// manifest said about it.
    #[arg(long = "machine", value_name = "[NODE=]MACHINE")]
    pub machines: Vec<String>,
    /// Stream the dataflow's logs until it finishes, then exit with its
    /// severity — rather than returning as soon as it is started.
    #[arg(long)]
    pub attach: bool,
    /// Hide streamed output below this level, with `--attach`.
    #[arg(long, value_name = "LEVEL")]
    pub level: Option<String>,
}

/// `astrs stop` arguments.
#[derive(Debug, Args)]
pub struct StopArgs {
    /// The dataflow id (or name) to stop.
    pub dataflow: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// How long nodes get to finish before they are killed (§12).
    #[arg(long, value_name = "SECONDS")]
    pub grace: Option<f64>,
}

/// `astrs restart` arguments.
#[derive(Debug, Args)]
pub struct RestartArgs {
    /// The dataflow id (or name) to restart.
    pub dataflow: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Re-run the manifest's `build:` lines before starting again.
    #[arg(long)]
    pub rebuild: bool,
}

/// `astrs destroy` arguments.
#[derive(Debug, Args)]
pub struct DestroyArgs {
    /// The dataflow id (or name) to destroy; every dataflow when omitted.
    pub dataflow: Option<String>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Destroy even while dataflows are still running.
    #[arg(long)]
    pub force: bool,
}

/// `astrs clean` arguments.
#[derive(Debug, Args)]
pub struct CleanArgs {
    /// The finished dataflow to clean; every finished one when omitted.
    pub dataflow: Option<String>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Also delete build artefacts.
    #[arg(long)]
    pub artifacts: bool,
    /// Also delete captured logs.
    #[arg(long)]
    pub logs: bool,
}

// =========================================================================
// Monitoring
// =========================================================================

/// `astrs list` arguments.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Include dataflows that have already finished.
    #[arg(short, long)]
    pub all: bool,
}

/// `astrs logs` arguments.
#[derive(Debug, Args)]
pub struct LogsArgs {
    /// The dataflow id (or name) to show logs for; every dataflow if
    /// omitted.
    pub dataflow: Option<String>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Keep streaming new log records instead of dumping and exiting.
    #[arg(short, long)]
    pub follow: bool,
    /// Only show records at or above this level.
    #[arg(long)]
    pub level: Option<String>,
    /// Only show records from this node id.
    #[arg(long)]
    pub node: Option<String>,
    /// At most this many records in a non-following dump.
    #[arg(long)]
    pub limit: Option<u32>,
}

/// `astrs top` arguments.
#[derive(Debug, Args)]
pub struct TopArgs {
    /// How to reach (and authenticate to) the coordinator. Ignored when
    /// `--replay` is given — a replay session opens no connection at all.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Render an `.arec` recording instead of a live cluster (blueprint
    /// §14). The coordinator is never dialled in this mode.
    #[arg(long)]
    pub replay: Option<PathBuf>,
}

/// `astrs topic` subcommands.
#[derive(Debug, Subcommand)]
pub enum TopicCommand {
    /// Print every message on a topic as it arrives.
    Echo(TopicEchoArgs),
    /// Report a topic's observed message rate.
    Hz(TopicHzArgs),
    /// Show a topic's type, publisher(s) and subscriber(s).
    Info(TopicInfoArgs),
    /// Publish one message onto a topic.
    Pub(TopicPubArgs),
}

/// `astrs topic echo` arguments.
#[derive(Debug, Args)]
pub struct TopicEchoArgs {
    /// The topic to echo, as `node/output`.
    pub topic: String,
    /// The dataflow it belongs to; probes the cluster's sole running
    /// dataflow if omitted.
    #[arg(long)]
    pub dataflow: Option<String>,
    /// Stop after this many messages instead of streaming until
    /// interrupted (`ros2 topic echo --once` for `--count 1`).
    #[arg(long)]
    pub count: Option<u32>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

/// `astrs topic hz` arguments.
#[derive(Debug, Args)]
pub struct TopicHzArgs {
    /// The topic to measure, as `node/output`.
    pub topic: String,
    /// The dataflow it belongs to; probes the cluster's sole running
    /// dataflow if omitted.
    #[arg(long)]
    pub dataflow: Option<String>,
    /// The trailing window the rate is computed over, in seconds.
    #[arg(long, default_value_t = 5.0)]
    pub window: f64,
    /// Stop after this many messages instead of streaming until
    /// interrupted.
    #[arg(long)]
    pub count: Option<u32>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

/// `astrs topic info` arguments.
#[derive(Debug, Args)]
pub struct TopicInfoArgs {
    /// The topic to inspect, as `node/output`.
    pub topic: String,
    /// The dataflow it belongs to; probes the cluster's sole running
    /// dataflow if omitted.
    #[arg(long)]
    pub dataflow: Option<String>,
    /// A manifest on disk to resolve the subscriber list from (the
    /// coordinator does not yet answer with a running dataflow's graph —
    /// see this crate's own docs).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

/// `astrs topic pub` arguments.
#[derive(Debug, Args)]
pub struct TopicPubArgs {
    /// The topic to publish onto, as `node/output` — the *producer* port a
    /// `path: dynamic` node in the manifest declares, not a consumer's
    /// input (this crate's own docs explain why).
    pub topic: String,
    /// The message payload, as JSON.
    pub message: String,
    /// The dataflow it belongs to; probes the cluster's sole running
    /// dataflow if omitted.
    #[arg(long)]
    pub dataflow: Option<String>,
    /// Repeat the publish at this rate (Hz) instead of once.
    #[arg(long)]
    pub rate: Option<f64>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

/// `astrs status` arguments.
///
/// With no dataflow named this probes the local *cluster* — the
/// coordinator and this machine's daemon — which is what tells `astrs up`
/// apart from "nothing is running" (blueprint §17).
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// The dataflow id (or name) to show; probes the cluster if omitted.
    pub dataflow: Option<String>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Where the pidfiles to read live (§24.2).
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,
}

/// `astrs trace` arguments.
#[derive(Debug, Args)]
pub struct TraceArgs {
    /// The dataflow id (or name) to trace; every dataflow if omitted.
    pub dataflow: Option<String>,
    /// Only trace events on this node id.
    #[arg(long)]
    pub node: Option<String>,
    /// Stop after this many spans.
    #[arg(long)]
    pub limit: Option<u32>,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

// =========================================================================
// Graph ops
// =========================================================================

/// `astrs validate` arguments.
#[derive(Debug, Args)]
pub struct ValidateCliArgs {
    /// The manifest to validate.
    pub manifest: PathBuf,
    /// Prove the graph's obligations — deadlock freedom, queue
    /// boundedness, rate consistency, latency budgets and type-rule
    /// consistency (blueprint §15) — through the SMT solver, printing a
    /// counterexample for any that fails.
    #[arg(long)]
    pub prove: bool,
    /// A verification profile supplying what the manifest cannot state:
    /// per-node service times and named end-to-end latency budgets. Only
    /// meaningful with `--prove`; every other obligation is discharged
    /// from the manifest alone.
    #[arg(long, value_name = "FILE", requires = "prove")]
    pub profile: Option<PathBuf>,
}

/// `astrs expand` arguments.
#[derive(Debug, Args)]
pub struct ExpandCliArgs {
    /// The manifest to expand.
    pub manifest: PathBuf,
}

/// `astrs graph` arguments.
#[derive(Debug, Args)]
pub struct GraphCliArgs {
    /// The manifest to visualize.
    pub manifest: PathBuf,
    /// The output format.
    #[arg(long, value_enum, default_value_t = GraphFormatArg::Mermaid)]
    pub format: GraphFormatArg,
}

/// The `--format` values `astrs graph` accepts (a thin `clap::ValueEnum`
/// wrapper over [`GraphFormat`], which has no `clap` dependency of its
/// own — keeping `astrs-graph`'s presentation-neutral emitters decoupled
/// from this crate's argument-parsing choices).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum GraphFormatArg {
    /// Mermaid flowchart source.
    Mermaid,
    /// Graphviz DOT source.
    Dot,
    /// A self-contained HTML page embedding the mermaid source.
    Html,
}

impl From<GraphFormatArg> for GraphFormat {
    fn from(value: GraphFormatArg) -> Self {
        match value {
            GraphFormatArg::Mermaid => Self::Mermaid,
            GraphFormatArg::Dot => Self::Dot,
            GraphFormatArg::Html => Self::Html,
        }
    }
}

/// `astrs node` subcommands (blueprint §8, §17 dynamic topology).
#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Add a node to a running dataflow.
    Add(NodeAddArgs),
    /// Remove a node from a running dataflow.
    Remove(NodeRemoveArgs),
    /// Replace a node in a running dataflow with a new definition.
    Replace(NodeReplaceArgs),
    /// Connect (or rewire) one input edge on a live node.
    Connect(NodeConnectArgs),
    /// Disconnect one input edge from a live node.
    Disconnect(NodeDisconnectArgs),
}

/// `astrs node add` arguments.
#[derive(Debug, Args)]
pub struct NodeAddArgs {
    /// The dataflow id (or name) to add the node to.
    pub dataflow: String,
    /// A manifest fragment (a single node's YAML, with no `nodes:`
    /// wrapper) describing the new node.
    pub node_manifest: PathBuf,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Register the node without spawning it yet.
    #[arg(long)]
    pub no_start: bool,
    /// Emit JSON rather than a human line.
    #[arg(long)]
    pub json: bool,
}

/// `astrs node remove` arguments.
#[derive(Debug, Args)]
pub struct NodeRemoveArgs {
    /// The dataflow id (or name) to remove the node from.
    pub dataflow: String,
    /// The node id to remove.
    pub node_id: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// How long the node gets to finish before it is killed (§12).
    #[arg(long, value_name = "SECONDS")]
    pub grace: Option<f64>,
    /// Emit JSON rather than a human line.
    #[arg(long)]
    pub json: bool,
}

/// `astrs node replace` arguments.
#[derive(Debug, Args)]
pub struct NodeReplaceArgs {
    /// The dataflow id (or name) whose node should be replaced.
    pub dataflow: String,
    /// The node id to replace.
    pub node_id: String,
    /// A manifest fragment (a single node's YAML, with no `nodes:`
    /// wrapper) describing the replacement.
    pub node_manifest: PathBuf,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Let the outgoing incarnation drain its inputs before the cutover.
    ///
    /// Accepted but not yet enforced server-side: the daemon-side cutover
    /// (`astrs-daemon`'s dual-run window) always runs regardless — see
    /// `astrs-coordinator`'s own report for this deviation.
    #[arg(long)]
    pub drain: bool,
    /// Emit JSON rather than a human line.
    #[arg(long)]
    pub json: bool,
}

/// `astrs node connect` arguments.
#[derive(Debug, Args)]
pub struct NodeConnectArgs {
    /// The dataflow id (or name) to edit.
    pub dataflow: String,
    /// The node whose input is being connected (or rewired).
    pub node_id: String,
    /// The input's name.
    pub input: String,
    /// The producer port to read from, as `node/output`.
    pub source: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// How many messages to buffer (default 10, blueprint §24.2).
    #[arg(long)]
    pub queue_size: Option<u32>,
    /// What to do when the queue is full.
    #[arg(long, value_enum)]
    pub queue_policy: Option<QueuePolicyArg>,
    /// Emit JSON rather than a human line.
    #[arg(long)]
    pub json: bool,
}

/// `astrs node disconnect` arguments.
#[derive(Debug, Args)]
pub struct NodeDisconnectArgs {
    /// The dataflow id (or name) to edit.
    pub dataflow: String,
    /// The node whose input is being disconnected.
    pub node_id: String,
    /// The input's name.
    pub input: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Emit JSON rather than a human line.
    #[arg(long)]
    pub json: bool,
}

/// The queue eviction policy for a connected input (`astrs node connect
/// --queue-policy`) — this crate's own copy of
/// [`astrs_wire::QueuePolicy`] (blueprint §11.2), which sits below `clap`
/// in the layer stack and so cannot derive [`ValueEnum`] itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum QueuePolicyArg {
    /// Evict the oldest queued message to make room. The default.
    DropOldest,
    /// Buffer up to ten times the queue size, then drop with an error.
    Backpressure,
}

/// `astrs param` subcommands.
#[derive(Debug, Subcommand)]
pub enum ParamCommand {
    /// Get one parameter's value.
    Get(ParamGetArgs),
    /// Set one parameter's value.
    Set(ParamSetArgs),
    /// List every parameter (optionally scoped to one node).
    List(ParamListArgs),
    /// Delete one parameter.
    Delete(ParamDeleteArgs),
}

/// `astrs param get` arguments.
#[derive(Debug, Args)]
pub struct ParamGetArgs {
    /// The dataflow id (or name), or the word `global` for the
    /// cluster-wide scope.
    pub dataflow: String,
    /// The parameter's key.
    pub key: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Read the node-scoped value instead of the dataflow-scoped one.
    #[arg(long)]
    pub node: Option<String>,
    /// Report only a value set in this exact scope, rather than falling
    /// back to the dataflow's (and then the cluster's) value.
    #[arg(long)]
    pub exact: bool,
    /// Keep polling and print every value this key transitions to,
    /// instead of reading it once (there is no push subscription for
    /// parameters — see `command::param`'s own docs).
    #[arg(long)]
    pub watch: bool,
    /// With `--watch`, stop after this many observed values (the initial
    /// read counts as one) instead of watching until interrupted.
    #[arg(long)]
    pub count: Option<u32>,
}

/// `astrs param set` arguments.
#[derive(Debug, Args)]
pub struct ParamSetArgs {
    /// The dataflow id (or name), or the word `global` for the
    /// cluster-wide scope.
    pub dataflow: String,
    /// The parameter's key.
    pub key: String,
    /// The value, as plain JSON: `30`, `1.5`, `true`, `"left"`, `[1,2,3]`
    /// — or the tagged form (`{"float": 30}`) to pin a type.
    pub value: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Set the node-scoped value instead of the dataflow-scoped one.
    #[arg(long)]
    pub node: Option<String>,
    /// Fail rather than overwrite a value that is already set.
    #[arg(long)]
    pub create_only: bool,
}

/// `astrs param list` arguments.
#[derive(Debug, Args)]
pub struct ParamListArgs {
    /// The dataflow id (or name), or the word `global` for the
    /// cluster-wide scope.
    pub dataflow: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Only list parameters on this node id.
    #[arg(long)]
    pub node: Option<String>,
    /// Only list keys starting with this prefix.
    #[arg(long)]
    pub prefix: Option<String>,
    /// Include values inherited from parent scopes.
    #[arg(long)]
    pub inherited: bool,
}

/// `astrs param delete` arguments.
#[derive(Debug, Args)]
pub struct ParamDeleteArgs {
    /// The dataflow id (or name), or the word `global` for the
    /// cluster-wide scope.
    pub dataflow: String,
    /// The parameter's key.
    pub key: String,
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Delete the node-scoped value instead of the dataflow-scoped one.
    #[arg(long)]
    pub node: Option<String>,
}

// =========================================================================
// Data
// =========================================================================

/// `astrs record` subcommands.
#[derive(Debug, Subcommand)]
pub enum RecordCommand {
    /// Start recording a dataflow's outputs.
    Start(RecordStartArgs),
    /// Stop an in-progress recording.
    Stop(RecordStopArgs),
}

/// `astrs record start` arguments.
#[derive(Debug, Args)]
pub struct RecordStartArgs {
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// The dataflow id (or name) to record.
    pub dataflow: String,
    /// The `.arec` file to write.
    pub output: PathBuf,
    /// Restrict recording to these `node/output` ports (repeatable);
    /// every declared output of every node in the dataflow, if none are
    /// given.
    #[arg(long = "only")]
    pub only: Vec<String>,
    /// Overwrite an already-recording session at the same path rather
    /// than refusing.
    #[arg(long)]
    pub overwrite: bool,
}

/// `astrs record stop` arguments.
#[derive(Debug, Args)]
pub struct RecordStopArgs {
    /// How to reach (and authenticate to) the coordinator.
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// The dataflow id (or name) whose recording should stop.
    pub dataflow: String,
}

/// How `astrs replay`'s generated `astrs-replay-node` instances pace
/// their re-emitted entries.
///
/// Spells the exact same three modes (and the same kebab-case flag
/// values) as `astrs_replay_node::TimingMode` — kept as this crate's own
/// type rather than a dependency on that binary's library purely to
/// parse three fixed strings, matching this codebase's own convention
/// of small, independently-testable per-module helpers over sharing one
/// two-line type across a crate boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ReplayTimingMode {
    /// No pacing at all.
    AsFastAsPossible,
    /// Recorded HLC deltas, scaled by `--speed`.
    #[default]
    RealTime,
    /// A constant period, from `--rate`.
    FixedRate,
}

impl ReplayTimingMode {
    /// The flag spelling `astrs-replay-node --mode` expects.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AsFastAsPossible => "as-fast-as-possible",
            Self::RealTime => "real-time",
            Self::FixedRate => "fixed-rate",
        }
    }
}

/// `astrs replay` arguments.
///
/// Two forms, and exactly one of them must be chosen (blueprint §14):
///
/// ```text
///   astrs replay s.arec --into graph.yml    offline: print the rewritten manifest
///   astrs replay s.arec perception          live:    cut the running graph over
/// ```
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// The `.arec` file to replay.
    pub input: PathBuf,
    /// A *running* dataflow to cut over, by id or by name.
    ///
    /// Each chosen node is replaced in place by an `astrs-replay-node`
    /// reading the recording, keeping its id and therefore its edges — the
    /// live twin of `--into`'s manifest rewrite.
    pub dataflow: Option<String>,
    /// The manifest file to rewrite, replacing chosen source nodes with
    /// `astrs-replay-node` instances in place.
    ///
    /// The offline form: the rewritten manifest is printed and nothing is
    /// started. Mutually exclusive with the positional dataflow.
    #[arg(long, conflicts_with = "dataflow")]
    pub into: Option<String>,
    /// The replay speed multiplier (`1.0` is real-time), passed through
    /// to `--mode real-time`'s `--speed`.
    #[arg(long)]
    pub speed: Option<f64>,
    /// The timing mode the generated replay node paces entries with.
    #[arg(long, value_enum, default_value = "real-time")]
    pub mode: ReplayTimingMode,
    /// The fixed emission rate, in Hz, for `--mode fixed-rate`.
    #[arg(long)]
    pub rate: Option<f64>,
    /// Restart each generated replay node from the beginning once its
    /// recording is exhausted.
    #[arg(long)]
    pub r#loop: bool,
    /// Replace only these node ids (repeatable); every node the
    /// recording covers, if none are given.
    #[arg(long = "replace")]
    pub replace: Vec<String>,
    /// Let each replaced node drain its inputs before it is stopped.
    ///
    /// The live form only: the offline form starts nothing, so there is
    /// nothing to drain.
    #[arg(long, requires = "dataflow")]
    pub drain: bool,
    /// How to reach the coordinator — the live form only.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

/// `astrs bag` subcommands.
#[derive(Debug, Subcommand)]
pub enum BagCommand {
    /// Convert between `.arec` and rosbag2 (`.db3`/`.mcap`).
    Convert(BagConvertArgs),
    /// Show a bag file's summary (topics, types, message counts).
    Info(BagInfoArgs),
}

/// `astrs bag convert` arguments.
#[derive(Debug, Args)]
pub struct BagConvertArgs {
    /// The input file (`.arec`, `.db3`, or `.mcap` — inferred from the
    /// extension).
    pub input: PathBuf,
    /// The output file (extension selects the target format).
    pub output: PathBuf,
}

/// `astrs bag info` arguments.
#[derive(Debug, Args)]
pub struct BagInfoArgs {
    /// The bag file to inspect.
    pub input: PathBuf,
}

// =========================================================================
// ROS 2
// =========================================================================

/// `astrs ros2` subcommands.
#[derive(Debug, Subcommand)]
pub enum Ros2Command {
    /// Probe ROS 2 discovery (RTPS participants, QoS, domain id).
    Doctor(Ros2DoctorArgs),
    /// List the live ROS 2 topic graph as AstRS's bridge sees it.
    Topics(Ros2TopicsArgs),
}

/// `astrs ros2 doctor` arguments.
#[derive(Debug, Args)]
pub struct Ros2DoctorArgs {
    /// The ROS domain id to probe (defaults to `ROS_DOMAIN_ID`, or `0`).
    #[arg(long)]
    pub domain_id: Option<u32>,
    /// How many milliseconds to listen for participants.
    #[arg(long, value_name = "MS")]
    pub timeout: Option<u64>,
    /// Include hidden (`_`-prefixed) names.
    #[arg(long)]
    pub hidden: bool,
}

/// `astrs ros2 topics` arguments.
#[derive(Debug, Args)]
pub struct Ros2TopicsArgs {
    /// The ROS domain id to probe (defaults to `ROS_DOMAIN_ID`, or `0`).
    #[arg(long)]
    pub domain_id: Option<u32>,
    /// How many milliseconds to listen for participants.
    #[arg(long, value_name = "MS")]
    pub timeout: Option<u64>,
    /// Include hidden (`_`-prefixed) names.
    #[arg(long)]
    pub hidden: bool,
}

// =========================================================================
// Dev
// =========================================================================

/// `astrs new` subcommands.
#[derive(Debug, Subcommand)]
pub enum NewCommand {
    /// Scaffold a new node crate.
    Node(NewNodeArgs),
    /// Scaffold a new runtime-hosted operator crate.
    Operator(NewOperatorArgs),
    /// Scaffold a new dylib-hosted operator crate (a `cdylib`, `dlopen`ed
    /// at spawn time -- blueprint §9.3, §22).
    OperatorDylib(NewOperatorDylibArgs),
    /// Scaffold a new starter graph manifest.
    Graph(NewGraphArgs),
}

/// `astrs new node` arguments.
#[derive(Debug, Args)]
pub struct NewNodeArgs {
    /// The new node's name (also its directory name).
    pub name: String,
    /// The language to scaffold in.
    #[arg(long, value_enum, default_value_t = NewLang::Rust)]
    pub lang: NewLang,
    /// The directory to scaffold into.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Overwrite files that already exist at the target location.
    #[arg(long)]
    pub force: bool,
}

/// `astrs new operator` arguments.
#[derive(Debug, Args)]
pub struct NewOperatorArgs {
    /// The new operator's name (also its directory name).
    pub name: String,
    /// The language to scaffold in.
    #[arg(long, value_enum, default_value_t = NewLang::Rust)]
    pub lang: NewLang,
    /// The directory to scaffold into.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Overwrite files that already exist at the target location.
    #[arg(long)]
    pub force: bool,
}

/// `astrs new operator-dylib` arguments.
#[derive(Debug, Args)]
pub struct NewOperatorDylibArgs {
    /// The new operator's name (also its directory name).
    pub name: String,
    /// The language to scaffold in.
    #[arg(long, value_enum, default_value_t = NewLang::Rust)]
    pub lang: NewLang,
    /// The directory to scaffold into.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Overwrite files that already exist at the target location.
    #[arg(long)]
    pub force: bool,
}

/// `astrs new graph` arguments.
#[derive(Debug, Args)]
pub struct NewGraphArgs {
    /// The new graph's name (its manifest file's stem).
    pub name: String,
    /// The directory to scaffold into.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Overwrite the target file if it already exists.
    #[arg(long)]
    pub force: bool,
}

impl NewCommand {
    /// Flatten this sub-subcommand into
    /// [`crate::command::new::NewArgs`]'s kind-agnostic shape.
    ///
    /// `json` is threaded through separately rather than read off any one
    /// leaf args struct because it is the global `--json` flag
    /// (blueprint §17: "All output has `--json` for scripting"), not
    /// something `clap` ever parses onto [`NewNodeArgs`]/
    /// [`NewOperatorArgs`]/[`NewOperatorDylibArgs`]/[`NewGraphArgs`]
    /// themselves.
    #[must_use]
    pub fn into_new_args(self, json: bool) -> crate::command::new::NewArgs {
        use crate::command::new::NewArgs;
        match self {
            Self::Node(a) => NewArgs {
                kind: NewKind::Node,
                name: a.name,
                lang: a.lang,
                dir: a.dir,
                force: a.force,
                json,
            },
            Self::Operator(a) => NewArgs {
                kind: NewKind::Operator,
                name: a.name,
                lang: a.lang,
                dir: a.dir,
                force: a.force,
                json,
            },
            Self::OperatorDylib(a) => NewArgs {
                kind: NewKind::OperatorDylib,
                name: a.name,
                lang: a.lang,
                dir: a.dir,
                force: a.force,
                json,
            },
            Self::Graph(a) => NewArgs {
                kind: NewKind::Graph,
                name: a.name,
                lang: NewLang::Rust,
                dir: a.dir,
                force: a.force,
                json,
            },
        }
    }
}

/// `astrs migrate` subcommands.
#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// Migrate a dora-rs dataflow descriptor.
    FromDora(FromDoraCliArgs),
    /// Skim a ROS 2 launch file into a bridge manifest scaffold.
    FromRos2(FromRos2CliArgs),
}

/// `astrs migrate from-dora` arguments.
#[derive(Debug, Args)]
pub struct FromDoraCliArgs {
    /// The dora dataflow descriptor to migrate.
    pub input: PathBuf,
    /// Write the migrated manifest here instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// `astrs migrate from-ros2` arguments.
#[derive(Debug, Args)]
pub struct FromRos2CliArgs {
    /// The ROS 2 launch file to skim (`.xml`/`.launch.xml`, or `.py` for
    /// a best-effort, unverified Python skim -- dispatched by extension).
    pub input: PathBuf,
    /// Write the migrated manifest here instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// `astrs doctor` arguments.
#[derive(Debug, Args)]
pub struct DoctorCliArgs {
    /// The coordinator port to probe.
    #[arg(long, default_value_t = crate::command::doctor::DEFAULT_COORDINATOR_PORT)]
    pub coordinator_port: u16,
    /// The local daemon port to probe.
    #[arg(long, default_value_t = crate::command::doctor::DEFAULT_DAEMON_PORT)]
    pub daemon_port: u16,
}

/// `astrs completion` arguments.
#[derive(Debug, Args)]
pub struct CompletionCliArgs {
    /// The shell to generate a completion script for.
    #[arg(value_enum)]
    pub shell: CompletionShell,
}

/// `astrs schema` arguments.
#[derive(Debug, Args)]
pub struct SchemaCliArgs {
    /// Write the schema to this file instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

// =========================================================================
// Token scopes (blueprint §16, §22)
// =========================================================================

/// `astrs token` subcommands.
#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Derive (or reveal) the credential for one scope of the cluster's
    /// root token.
    Mint(TokenMintArgs),
}

/// `astrs token mint` arguments.
///
/// Unlike every other verb group in this file, minting never dials a
/// coordinator: it only needs the root secret already on this machine (the
/// same `--token`/`--token-file`/`--working-dir` resolution every
/// coordinator-facing verb uses, via [`crate::runtime_dir::find_token`]),
/// and the scope derivation is a pure function of it
/// (`astrs_coordinator::auth::derive_read_token`) — so it takes no
/// [`ConnectArgs`].
#[derive(Debug, Args)]
pub struct TokenMintArgs {
    /// The scope to mint a credential for.
    #[arg(long, value_enum)]
    pub scope: TokenScopeArg,
    /// An explicit root token, instead of resolving one (§16). Prefer
    /// `--token-file`: an argument is visible in `ps`.
    #[arg(long)]
    pub token: Option<String>,
    /// Read the root token from this file instead of
    /// `<working-dir>/.astrs-token`.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// The directory `.astrs-token` is looked up in; defaults to the
    /// process's current directory (§16).
    #[arg(long)]
    pub working_dir: Option<PathBuf>,
    /// Write the minted credential to this file (mode `0600`) instead of
    /// printing it to stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// The scope `astrs token mint --scope` accepts — this crate's own copy of
/// [`astrs_wire::RequestScope`] (blueprint §16), which sits below `clap` in
/// the layer stack and so cannot derive [`ValueEnum`] itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum TokenScopeArg {
    /// Observes the cluster; cannot start, stop, edit or destroy anything.
    Read,
    /// Full access — the root token itself (blueprint §16 back-compat: every
    /// token minted before 0.2 is this scope).
    Mutate,
}

// =========================================================================
// Package hub (blueprint §22)
// =========================================================================

/// `astrs hub` subcommands.
#[derive(Debug, Subcommand)]
pub enum HubCommand {
    /// Clone (or fast-forward pull) the package index into the local
    /// cache.
    Update(HubUpdateArgs),
    /// List packages in the local index whose name or description matches
    /// a term.
    Search(HubSearchArgs),
    /// Show one package's description and every published version.
    Info(HubInfoArgs),
    /// Scaffold a valid, self-hostable index repo layout.
    Init(HubInitArgs),
}

/// `astrs hub update` arguments.
///
/// Like `astrs token mint`, this never dials a coordinator: it clones or
/// pulls a git repository directly (blueprint §2.2: the system `git`
/// binary, never `git2`/`libgit2`).
#[derive(Debug, Args)]
pub struct HubUpdateArgs {
    /// Use this index URL instead of resolving one from
    /// `ASTRS_HUB_INDEX`, the config file, or the built-in default.
    #[arg(long)]
    pub index: Option<String>,
}

/// `astrs hub search` arguments.
#[derive(Debug, Args)]
pub struct HubSearchArgs {
    /// Matched case-insensitively against a package's name or
    /// description.
    pub term: String,
}

/// `astrs hub info` arguments.
#[derive(Debug, Args)]
pub struct HubInfoArgs {
    /// The package name to show.
    pub name: String,
}

/// `astrs hub init` arguments.
#[derive(Debug, Args)]
pub struct HubInitArgs {
    /// The directory to scaffold the index repo layout into.
    pub dir: PathBuf,
    /// Scaffold into a directory that already has other files in it.
    #[arg(long)]
    pub force: bool,
}

// =========================================================================
// Hidden internals
// =========================================================================

/// `astrs daemon` arguments (internal — spawned by `astrs up`, never run
/// by hand in ordinary use).
#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// The coordinator address to register with.
    #[arg(long)]
    pub coordinator: Option<String>,
    /// This daemon's machine name (defaults to the local hostname).
    #[arg(long)]
    pub machine: Option<String>,
    /// The loopback TCP port node processes may dial as a UDS fallback
    /// (§4.2); `0` binds a free one, and omitting it opens no TCP port.
    #[arg(long)]
    pub port: Option<u16>,
    /// The TCP port other *daemons* dial for cross-machine data routes
    /// (§6.4); `0` binds a free one. Only opened when `--coordinator` is
    /// given, because a daemon nobody was told about has opened a hole for
    /// nothing.
    #[arg(long, default_value_t = astrs_daemon::peer::DEFAULT_PEER_PORT)]
    pub peer_port: u16,
    /// The interface the peer listener binds; defaults to loopback, exactly
    /// as the coordinator's own `--bind` does and for the same §16 reason.
    #[arg(long)]
    pub peer_bind: Option<String>,
    /// A placement label, repeatable: `--label zone=front --label gpu=yes`
    /// (§8.3 `deploy`).
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,
    /// Where the node socket and captured logs live (§24.2).
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,
    /// The directory node paths resolve against.
    #[arg(long)]
    pub working_dir: Option<PathBuf>,
    /// The 64-hex cluster token every node must present (§16).
    #[arg(long)]
    pub token: Option<String>,
    /// Read that token from this file instead of
    /// `<working-dir>/.astrs-token`.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// Write this process's pid here once the listeners are bound, so
    /// `astrs down` and `astrs status` can find it.
    #[arg(long)]
    pub pidfile: Option<PathBuf>,
    /// Print the bound addresses as one JSON line on stdout once ready —
    /// how `astrs up` learns a `--port 0` daemon's real port.
    #[arg(long)]
    pub announce: bool,
}

/// `astrs coordinator` arguments (internal — the one process `astrs up`
/// starts first).
#[derive(Debug, Args)]
pub struct CoordinatorArgs {
    /// The TCP port to listen on; `0` binds a free one.
    #[arg(long, default_value_t = crate::command::doctor::DEFAULT_COORDINATOR_PORT)]
    pub port: u16,
    /// The address to bind; defaults to loopback.
    #[arg(long)]
    pub bind: Option<String>,
    /// The 64-hex cluster token every peer must present (§16).
    #[arg(long)]
    pub token: Option<String>,
    /// Read that token from this file instead of
    /// `<working-dir>/.astrs-token`.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// The directory holding `.astrs-token` (§16).
    #[arg(long)]
    pub working_dir: Option<PathBuf>,
    /// The parameter/state store's directory; an in-memory store is used
    /// when omitted.
    #[arg(long)]
    pub store: Option<PathBuf>,
    /// Delete `--store` before opening it.
    #[arg(long)]
    pub recreate_store: bool,
    /// Write this process's pid here once the listener is bound.
    #[arg(long)]
    pub pidfile: Option<PathBuf>,
    /// Print the bound address as one JSON line on stdout once ready —
    /// how `astrs up` learns a `--port 0` coordinator's real port.
    #[arg(long)]
    pub announce: bool,
    /// This coordinator's Raft peer id within a replicated set (§22).
    /// Requires at least one `--ha-peer`, and the id must appear in it.
    #[arg(long)]
    pub ha_node_id: Option<u64>,
    /// One coordinator in the replicated set, as `id=host:port` (§22).
    /// Repeat once per coordinator, including this one; every coordinator in
    /// the set is given the identical list.
    #[arg(long = "ha-peer")]
    pub ha_peers: Vec<String>,
}

/// `astrs runtime` arguments (internal — spawned by a daemon for a
/// manifest node whose source is `operators:`).
#[derive(Debug, Args)]
pub struct RuntimeArgs {
    /// The node id (within its dataflow) this runtime process hosts
    /// operators for. Advisory: the authoritative id arrives in the
    /// `ASTRS_NODE_CONFIG` handshake blob, and a mismatch is refused.
    #[arg(long)]
    pub node_id: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_tree_is_valid_per_clap_debug_assert() {
        // `clap::Command::debug_assert` runs every internal consistency
        // check clap has (duplicate arg ids, conflicting short flags,
        // ...) without needing to actually parse anything.
        Cli::command().debug_assert();
    }

    #[test]
    fn the_coordinator_takes_a_repeated_ha_peer_flag() {
        // Every coordinator in a replicated set is given the identical peer
        // list; only `--ha-node-id` differs per machine (blueprint §22).
        let cli = Cli::try_parse_from([
            "astrs",
            "coordinator",
            "--ha-node-id",
            "2",
            "--ha-peer",
            "1=10.0.0.1:7601",
            "--ha-peer",
            "2=10.0.0.2:7601",
            "--ha-peer",
            "3=10.0.0.3:7601",
        ])
        .unwrap();
        let Command::Coordinator(args) = cli.command else {
            panic!("expected `coordinator`");
        };
        assert_eq!(args.ha_node_id, Some(2));
        assert_eq!(args.ha_peers.len(), 3);
        assert_eq!(args.ha_peers[0], "1=10.0.0.1:7601");
    }

    #[test]
    fn the_coordinator_still_parses_without_any_ha_flags() {
        let cli = Cli::try_parse_from(["astrs", "coordinator"]).unwrap();
        let Command::Coordinator(args) = cli.command else {
            panic!("expected `coordinator`");
        };
        assert!(args.ha_node_id.is_none());
        assert!(args.ha_peers.is_empty());
    }

    #[test]
    fn binary_name_is_astrs() {
        assert_eq!(Cli::command().get_name(), "astrs");
    }

    #[test]
    fn json_and_color_are_global_flags() {
        let cli = Cli::try_parse_from(["astrs", "--json", "schema"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["astrs", "schema", "--json"]).unwrap();
        assert!(cli.json, "global flags must also work after the subcommand");
    }

    #[test]
    fn token_mint_parses_its_scope() {
        let cli = Cli::try_parse_from(["astrs", "token", "mint", "--scope", "read"]).unwrap();
        let Command::Token(TokenCommand::Mint(args)) = cli.command else {
            panic!("expected `token mint`");
        };
        assert_eq!(args.scope, TokenScopeArg::Read);

        let cli = Cli::try_parse_from(["astrs", "token", "mint", "--scope", "mutate"]).unwrap();
        let Command::Token(TokenCommand::Mint(args)) = cli.command else {
            panic!("expected `token mint`");
        };
        assert_eq!(args.scope, TokenScopeArg::Mutate);
    }

    #[test]
    fn token_mint_requires_a_scope() {
        assert!(Cli::try_parse_from(["astrs", "token", "mint"]).is_err());
    }

    #[test]
    fn validate_rejects_a_profile_without_prove() {
        assert!(
            Cli::try_parse_from(["astrs", "validate", "--profile", "v.yaml", "dataflow.yml"])
                .is_err(),
            "`--profile` only means something alongside `--prove`"
        );
    }

    #[test]
    fn validate_parses_the_profile_flag() {
        let cli = Cli::try_parse_from([
            "astrs",
            "validate",
            "--prove",
            "--profile",
            "verify.yaml",
            "dataflow.yml",
        ])
        .unwrap();
        match cli.command {
            Command::Validate(args) => {
                assert!(args.prove);
                assert_eq!(
                    args.profile.as_deref(),
                    Some(std::path::Path::new("verify.yaml"))
                );
            }
            other => panic!("expected validate, got {other:?}"),
        }
    }

    #[test]
    fn validate_parses_prove_flag() {
        let cli = Cli::try_parse_from(["astrs", "validate", "--prove", "dataflow.yml"]).unwrap();
        match cli.command {
            Command::Validate(args) => {
                assert!(args.prove);
                assert_eq!(args.manifest, PathBuf::from("dataflow.yml"));
            }
            other => panic!("expected Validate, got {other:?}"),
        }
    }

    #[test]
    fn new_node_parses_with_defaults() {
        let cli = Cli::try_parse_from(["astrs", "new", "node", "my-node"]).unwrap();
        match cli.command {
            Command::New(NewCommand::Node(args)) => {
                assert_eq!(args.name, "my-node");
                assert_eq!(args.lang, NewLang::Rust);
                assert_eq!(args.dir, PathBuf::from("."));
                assert!(!args.force);
            }
            other => panic!("expected New(Node), got {other:?}"),
        }
    }

    #[test]
    fn new_operator_dylib_parses_as_the_kebab_case_subcommand() {
        // `NewCommand`'s multi-word variants get clap's own default
        // kebab-case rendering (no `rename_all` override on the enum
        // itself — see `astrs migrate from-dora`'s identical precedent),
        // so `OperatorDylib` must parse as `operator-dylib`, not
        // `operatordylib` or `operator_dylib`.
        let cli = Cli::try_parse_from(["astrs", "new", "operator-dylib", "my-dylib-op"]).unwrap();
        match cli.command {
            Command::New(NewCommand::OperatorDylib(args)) => {
                assert_eq!(args.name, "my-dylib-op");
                assert_eq!(args.lang, NewLang::Rust);
                assert_eq!(args.dir, PathBuf::from("."));
                assert!(!args.force);
            }
            other => panic!("expected New(OperatorDylib), got {other:?}"),
        }
    }

    #[test]
    fn migrate_from_dora_parses() {
        let cli = Cli::try_parse_from([
            "astrs",
            "migrate",
            "from-dora",
            "in.yml",
            "--output",
            "out.yml",
        ])
        .unwrap();
        match cli.command {
            Command::Migrate(MigrateCommand::FromDora(args)) => {
                assert_eq!(args.input, PathBuf::from("in.yml"));
                assert_eq!(args.output, Some(PathBuf::from("out.yml")));
            }
            other => panic!("expected Migrate(FromDora), got {other:?}"),
        }
    }

    #[test]
    fn migrate_from_ros2_parses() {
        let cli = Cli::try_parse_from([
            "astrs",
            "migrate",
            "from-ros2",
            "in.launch.xml",
            "--output",
            "out.yml",
        ])
        .unwrap();
        match cli.command {
            Command::Migrate(MigrateCommand::FromRos2(args)) => {
                assert_eq!(args.input, PathBuf::from("in.launch.xml"));
                assert_eq!(args.output, Some(PathBuf::from("out.yml")));
            }
            other => panic!("expected Migrate(FromRos2), got {other:?}"),
        }
    }

    #[test]
    fn every_daemon_needing_verb_still_parses() {
        // "present with real arg schemas" -- every verb blueprint §17 lists
        // must parse, whether it is implemented here or scheduled for a
        // later wave (see `command::stub`).
        let lines: &[&[&str]] = &[
            &["astrs", "run", "d.yml"],
            &["astrs", "up"],
            &["astrs", "down"],
            &["astrs", "build", "d.yml"],
            &["astrs", "start", "d.yml"],
            &["astrs", "stop", "my-flow"],
            &["astrs", "restart", "my-flow"],
            &["astrs", "destroy", "my-flow"],
            &["astrs", "clean"],
            &["astrs", "list"],
            &["astrs", "logs"],
            &["astrs", "top"],
            &["astrs", "topic", "echo", "cam/frames"],
            &["astrs", "topic", "hz", "cam/frames"],
            &["astrs", "topic", "info", "cam/frames"],
            &["astrs", "topic", "pub", "cam/frames", "{}"],
            &["astrs", "status"],
            &["astrs", "trace"],
            &["astrs", "node", "remove", "my-flow", "cam"],
            &["astrs", "param", "list", "my-flow"],
            &["astrs", "record", "stop", "my-flow"],
            &["astrs", "replay", "r.arec"],
            &["astrs", "bag", "info", "b.mcap"],
            &["astrs", "ros2", "doctor"],
            &["astrs", "ros2", "topics"],
            &["astrs", "daemon"],
            &["astrs", "coordinator"],
            &["astrs", "runtime", "--node-id", "cam"],
        ];
        for argv in lines {
            Cli::try_parse_from(*argv).unwrap_or_else(|e| panic!("{argv:?} failed to parse: {e}"));
        }
    }

    #[test]
    fn every_client_verb_takes_the_same_connect_flags() {
        // `ConnectArgs` is flattened rather than repeated, so this asserts
        // the flattening actually reached each verb — a `#[command(flatten)]`
        // left off one of them would not fail to compile.
        for verb in [
            &["astrs", "list"][..],
            &["astrs", "logs"][..],
            &["astrs", "stop", "f"][..],
            &["astrs", "restart", "f"][..],
            &["astrs", "destroy"][..],
            &["astrs", "clean"][..],
            &["astrs", "start", "d.yml"][..],
            &["astrs", "status"][..],
            &["astrs", "up"][..],
            &["astrs", "down"][..],
            &["astrs", "top"][..],
        ] {
            let mut argv: Vec<&str> = verb.to_vec();
            argv.extend(["--coordinator", "127.0.0.1:7407", "--token", "ab"]);
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?} failed to parse: {e}"));
        }
    }

    #[test]
    fn top_parses_the_replay_flag() {
        let cli = Cli::try_parse_from(["astrs", "top", "--replay", "session.arec"]).unwrap();
        match cli.command {
            Command::Top(args) => {
                assert_eq!(args.replay, Some(PathBuf::from("session.arec")));
            }
            other => panic!("expected Command::Top, got {other:?}"),
        }
    }

    #[test]
    fn top_without_replay_defaults_to_live() {
        let cli = Cli::try_parse_from(["astrs", "top"]).unwrap();
        match cli.command {
            Command::Top(args) => assert_eq!(args.replay, None),
            other => panic!("expected Command::Top, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_its_stage_two_flags() {
        let cli = Cli::try_parse_from([
            "astrs",
            "run",
            "d.yml",
            "--skip-build",
            "--level",
            "warn",
            "--timeout",
            "2.5",
            "--exit-when-nodes-finish",
        ])
        .unwrap();
        match cli.command {
            Command::Run(args) => {
                assert!(args.skip_build);
                assert!(args.exit_when_nodes_finish);
                assert_eq!(args.level.as_deref(), Some("warn"));
                assert_eq!(args.timeout, Some(2.5));
                assert!(!args.deterministic);
                assert_eq!(args.from_recording, None);
                assert_eq!(args.speed, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_deterministic_replay_flags() {
        let cli = Cli::try_parse_from([
            "astrs",
            "run",
            "d.yml",
            "--deterministic",
            "--from-recording",
            "session.arec",
            "--speed",
            "2.0",
        ])
        .unwrap();
        match cli.command {
            Command::Run(args) => {
                assert!(args.deterministic);
                assert_eq!(args.from_recording, Some(PathBuf::from("session.arec")));
                assert_eq!(args.speed, Some(2.0));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn up_takes_no_manifest_because_it_brings_up_a_cluster() {
        // Blueprint §17 groups `up`/`down` under *cluster* lifecycle;
        // dataflows are started with `astrs start`.
        assert!(Cli::try_parse_from(["astrs", "up", "d.yml"]).is_err());
        let cli = Cli::try_parse_from(["astrs", "up", "--recreate-store", "--port", "0"]).unwrap();
        match cli.command {
            Command::Up(args) => {
                assert!(args.recreate_store);
                assert_eq!(args.port, Some(0));
                assert!(!args.no_daemon);
            }
            other => panic!("expected Up, got {other:?}"),
        }
    }

    #[test]
    fn hidden_commands_are_marked_hidden() {
        let command = Cli::command();
        for name in ["daemon", "coordinator", "runtime"] {
            let sub = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("missing subcommand {name}"));
            assert!(sub.is_hide_set(), "`{name}` should be hidden");
        }
        // A visible command, as a control.
        assert!(!command.find_subcommand("validate").unwrap().is_hide_set());
    }

    #[test]
    fn graph_format_arg_maps_onto_graph_format() {
        assert_eq!(
            GraphFormat::from(GraphFormatArg::Mermaid),
            GraphFormat::Mermaid
        );
        assert_eq!(GraphFormat::from(GraphFormatArg::Dot), GraphFormat::Dot);
        assert_eq!(GraphFormat::from(GraphFormatArg::Html), GraphFormat::Html);
    }

    #[test]
    fn new_command_flattens_into_new_args() {
        let cli = Cli::try_parse_from(["astrs", "new", "graph", "perception"]).unwrap();
        let Command::New(new_command) = cli.command else {
            panic!("expected New");
        };
        let args = new_command.into_new_args(false);
        assert_eq!(args.kind, NewKind::Graph);
        assert_eq!(args.name, "perception");
        assert!(!args.json);
    }

    #[test]
    fn new_command_threads_the_global_json_flag_through() {
        let cli = Cli::try_parse_from(["astrs", "--json", "new", "node", "cam"]).unwrap();
        let Command::New(new_command) = cli.command else {
            panic!("expected New");
        };
        let args = new_command.into_new_args(cli.json);
        assert!(args.json);
    }
}
