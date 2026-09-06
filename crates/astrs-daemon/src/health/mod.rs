//! Health, metrics sampling and the coordinator leg (§12, §13).
//!
//! | Module | Concern |
//! |---|---|
//! | [`liveness`] | [`HealthTable`]: post-registration liveness deadlines, and why a parked node is a live node |
//! | [`sampler`] | [`NodeMetricsCollector`]: per-node CPU/RSS/queue sampling every 2 s |
//! | [`io`] | [`NodeIoLedger`]: per-node, per-output message and byte totals (§13's bandwidth half) |
//! | [`heartbeat`] | [`HeartbeatProducer`]: the 5 s beat, its sequence number, and degraded-autonomous mode |
//! | [`sink`] | [`ReportSink`]: where everything the daemon reports upward goes |
//!
//! The first three are pure state machines over [`std::time::Instant`]: none
//! spawns a task, none reads a clock of its own, and each is driven from the
//! merged event loop's single `tick`. That is the same discipline
//! [`crate::supervise`] follows, for the same reason — a deadline that fires
//! from its own thread fires while the state it is about to act on is being
//! changed by another.
//!
//! ```text
//!             Daemon::tick(now)
//!                    │
//!   ┌────────────────┼──────────────────┬──────────────────┐
//!   ▼                ▼                  ▼                  ▼
//! HealthTable   NodeMetricsCollector  HeartbeatProducer   (spawn deadlines,
//! ::expired      ::due → ::sample      ::due → ::emit       watchdogs)
//!   │                │                  │
//!   │ HealthExpiry   │ NodeMetricsSample│ DaemonEvent::Heartbeat
//!   ▼                ▼                  ▼
//! fail_node      ReportSink ◄───────────┘
//! ```
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::health::{HealthTable, RecordingSink, ReportSink};
//! use astrs_wire::{DataflowId, NodeId};
//!
//! let start = Instant::now();
//! let mut health = HealthTable::with_default_timeout(Duration::from_secs(5));
//! health.arm(DataflowId::from_u128(1), NodeId::new("planner")?, 0, None, start);
//!
//! let sink = RecordingSink::new();
//! for expiry in health.expired(start + Duration::from_secs(6)) {
//!     sink.report(astrs_wire::DaemonEvent::NodeStopped {
//!         dataflow: expiry.dataflow,
//!         node: expiry.node.clone(),
//!         generation: expiry.generation,
//!         cause: expiry.cause(),
//!         restarting: false,
//!     });
//! }
//! assert!(sink.contains("NodeStopped"));
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

pub mod heartbeat;
pub mod io;
pub mod liveness;
pub mod sampler;
pub mod sink;

pub use heartbeat::{DEGRADED_AFTER, HeartbeatProducer, LinkHealth};
pub use io::{NodeIoLedger, PortTraffic};
pub use liveness::{HealthExpiry, HealthTable, LivenessState, NodeKey};
pub use sampler::{NodeMetricsCollector, NodeSampleRequest};
pub use sink::{ChannelSink, NullSink, RECORDING_CAPACITY, RecordingSink, ReportSink};
