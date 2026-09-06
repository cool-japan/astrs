//! The AstRS daemon — one per machine (blueprint §4.2, §12, §16).
//!
//! The control-plane actor and reliability path for every node on its host.
//! One daemon owns every node process on its machine: it spawns them with a
//! scrubbed environment, supervises them under their restart policies, routes
//! messages between them until the shared-memory plane takes over, holds their
//! extension table, and stops them — really stops them — when the dataflow
//! ends.
//!
//! ```text
//!         ┌──────────────────── astrs-daemon ────────────────────┐
//!  CLI ──►│  dataflow::fsm    plan → build → spawn → run → stop  │
//!         │        │                                             │
//!         │  server::core     one merged event loop, one owner   │
//!         │        │            of every mutable fact (§4.3)     │
//!         │  ┌─────┴──────┬──────────────┬───────────────────┐   │
//!         │  spawn/      supervise/     local/          extensions/│
//!         │  env scrub   restart        routing         (ns,key)  │
//!         │  argv        watchdogs      queues          → bytes   │
//!         │  generations exit causes    virtual srcs    reclaim   │
//!         │  └──────────────────────────┬──────────────────────┘  │
//!         │              session/ : one task per connected node   │
//!         └───────────────────────────┬──────────────────────────┘
//!                                     ▼
//!                          node processes (§16 hygiene)
//! ```
//!
//! # Modules
//!
//! | Module | Concern |
//! |---|---|
//! | [`config`] | Machine identity, runtime paths, listener addresses, budgets (§24.2) |
//! | [`spawn`] | Process creation: the §16 environment pipeline, argv splitting, the `ASTRS_NODE_CONFIG` blob, generation-stamped handles |
//! | [`supervise`] | Restart policies with exponential backoff and budgets, spawn deadlines, the `SIGTERM`→`SIGKILL` finish ladder, exit-cause taxonomy (§12) |
//! | [`local`] | The daemon-mediated reliable path: per-node queues, fan-out, `astrs/timer/*` and `astrs/logs/*` (§6.3, §8.4, §11.2) |
//! | [`extensions`] | The dataflow-scoped `(namespace, key) → bytes` table with crash reclamation (§2.1) |
//! | [`state`] | What the daemon knows: nodes, routes, dataflows, sessions |
//! | [`session`] | The daemon↔node leg: the conversation state machine, the socket actor, the internal event channel (§7.3) |
//! | [`server`] | The merged event loop and the node listeners (§4.3, §4.2) |
//! | [`dataflow`] | Planning, building, and the lifecycle — including [`run_dataflow_with`] |
//! | [`peer`] | The daemon↔daemon leg: cross-host routes over `astrs-transport` (§6.4) |
//! | [`shm`] | The same-host zero-copy plane and its slow-start handshake (§6.2, §6.3) |
//! | [`coordinator`] | The daemon↔coordinator uplink: register, heartbeat, execute, reconnect (§4.2, §12) |
//!
//! # Two ways to drive the same daemon
//!
//! [`run_dataflow_with`] hands the loop a plan and reads the result back
//! in-process — that is `astrs run`. [`Daemon::connect_coordinator`] instead
//! dials a coordinator and lets it drive: the same event loop, the same state,
//! the same planes, with `CoordinatorEvent`s arriving over the wire in place
//! of function calls. Nothing below the seam knows which one it is running
//! under.
//!
//! # Running a graph in this process
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::{RunOptions, run_dataflow_with};
//! use astrs_manifest::Manifest;
//!
//! let manifest = Manifest::from_yaml_file("dataflow.yml")?;
//! let result = run_dataflow_with(&manifest, RunOptions::default()).await?;
//!
//! for (node, cause) in result.failed_nodes() {
//!     eprintln!("{node} failed: {cause}");
//! }
//! # Ok(()) }
//! ```
//!
//! # Running a daemon
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::{Daemon, DaemonConfig};
//!
//! let mut daemon = Daemon::new(DaemonConfig::from_env())?;
//! daemon.bind().await?;
//! let results = daemon.run().await;
//! println!("{} dataflow(s) finished", results.len());
//! # Ok(()) }
//! ```

// `missing_docs` and the clippy `unwrap_used`/`expect_used`/`panic`/
// `dbg_macro`/`todo`/`unimplemented` denials come from `[lints] workspace =
// true` in this crate's Cargo.toml (workspace policy), so they are not
// repeated here as source attributes.

pub mod config;
pub mod coordinator;
pub mod dataflow;
pub mod error;
pub mod extensions;
pub mod health;
pub mod local;
pub mod metrics;
pub mod peer;
pub mod server;
pub mod session;
pub mod shm;
pub mod spawn;
pub mod state;
pub mod supervise;
pub mod tap;

pub use config::{DaemonConfig, ListenConfig, RuntimePaths};
pub use coordinator::{
    ClusterState, LogHistory, PeerDirectory, UplinkConfig, UplinkHandle, UplinkSink, UplinkState,
};
pub use dataflow::{
    BuildReport, BuildStep, DataflowPlan, PlannedVirtualInput, RunOptions, plan_dataflow,
    run_dataflow_with,
};
pub use error::{DaemonError, DaemonResult};
pub use extensions::{DropReason, DroppedEntry, ExtensionTable, StoreOutcome};
pub use health::{HealthTable, HeartbeatProducer, NodeMetricsCollector, ReportSink};
pub use local::{LocalRouter, NodeMailbox};
pub use metrics::{DaemonMetrics, FtStats};
pub use peer::{PeerConfig, PeerLink, PeerManager, PeerReaction};
pub use server::{Daemon, NodeListeners, SessionMinter};
pub use session::{DaemonEvent, DaemonHandle, SessionAction, SessionProtocol};
pub use shm::{OutputKey, ShmPlane, ShmPolicy, UpgradeState};
pub use spawn::{BuiltEnv, DenyReason, EnvPolicy, ProcessHandle, SpawnRequest, Spawner};
pub use state::{DaemonState, DataflowState, NodeState, RouteTable};
pub use supervise::{ExitIntent, RestartDecision, RestartHistory};
pub use tap::{TapRegistry, TapSubscription};
