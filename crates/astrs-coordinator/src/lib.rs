//! The AstRS coordinator — one per cluster.
//!
//! Owns cluster-wide truth and hands it out over the single wire protocol
//! (blueprint §4.2):
//!
//! - The daemon registry: registration, heartbeats, capability negotiation
//!   and eviction.
//! - The dataflow finite state machine, from `build` through `start`, `stop`
//!   and `destroy`, aggregating per-node exit causes into a typed
//!   `DataflowResult`.
//! - Build orchestration and artifact serving to daemons.
//! - Log and topic fan-out to CLI subscribers over the standard framing.
//! - The parameter store API over `astrs-store`, and sequence-numbered
//!   `StateCatchUp` for daemons that reconnect after a partition.
//!
//! # High availability (the `ha` feature)
//!
//! One coordinator is one point of failure for the whole control plane. With
//! the `ha` feature on, an odd-sized set of coordinators replicates the
//! durable registry through Raft ([`ha`]): mutating control requests are
//! accepted only by the leader and only once a majority has them, reads are
//! served locally under a leader lease, and a request that reaches a follower
//! is answered with the ordinary structured error carrying a
//! `leader: <address>` hint. Off by default — a single-machine deployment
//! pays nothing for it, and every path below behaves exactly as it always has.

pub mod auth;
mod catchup;
mod config;
mod coordinator;
mod error;
mod graph_bridge;
#[cfg(feature = "ha")]
pub mod ha;
mod handlers;
pub mod hub_index;
mod param_scope;
mod placement;
mod registry;
mod restore;
mod server;
mod session;
mod trace;

pub use config::{
    CoordinatorConfig, DEFAULT_COORDINATOR_PORT, DEFAULT_HANDSHAKE_TIMEOUT,
    DEFAULT_HEARTBEAT_INTERVAL, DEFAULT_MISSED_HEARTBEAT_LIMIT, ENV_COORDINATOR_PORT,
};
pub use coordinator::Coordinator;
pub use error::{CoordinatorError, Result};
pub use graph_bridge::{expand_node, expand_node_fragment};
pub use param_scope::{
    GLOBAL_SCOPE_DATAFLOW, delete_all_for_dataflow, delete_param, get_param, json_to_parameter,
    list_params, list_with_inheritance, lookup_with_inheritance, parameter_to_json, set_param,
};
pub use restore::rebuild_graph;
pub use server::{CoordinatorServer, ServerHandle};
