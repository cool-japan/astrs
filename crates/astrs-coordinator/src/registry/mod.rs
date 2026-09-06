//! Live, in-memory coordinator state — the part that does not survive a
//! restart because it only makes sense while sockets are open (blueprint
//! §5.2).
//!
//! Durable state (dataflow metadata, node status, parameters, the daemon
//! registry's persisted half) lives in `astrs-store` and is read back on
//! demand; see [`crate::coordinator::Coordinator`] for how the two are
//! composed.

pub mod daemon;
pub mod dataflow;
pub mod pending_log;
pub mod subscription;

pub use daemon::{DaemonHandle, DaemonRegistry};
pub use dataflow::{
    BuildOutcomeSummary, DataflowRegistry, LiveDataflow, PendingBuild, PendingSpawn,
    SpawnOutcomeSummary,
};
pub use pending_log::{PendingLogFetch, PendingLogFetches};
pub use subscription::{SubscriberHandle, SubscriptionKind, SubscriptionRegistry};
