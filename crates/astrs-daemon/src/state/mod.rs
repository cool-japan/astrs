//! What the daemon knows — nodes, routes, dataflows.
//!
//! | Module | Contents |
//! |---|---|
//! | [`node`] | [`NodeState`]: one node's lifecycle, generation, subscriptions and restart history |
//! | [`routes`] | [`RouteTable`]: producer port → consumers, with the plane each route uses |
//! | [`dataflow`] | [`DataflowState`]: the nodes, the routes, the extension table and the phase |
//! | [`registry`] | [`DaemonState`]: every dataflow this daemon hosts, plus the session index |
//!
//! Everything here is plain data with plain methods: no locks, no channels, no
//! tasks, no clock reads. The event loop ([`crate::server`]) owns one
//! [`DaemonState`] and mutates it from a single task, which is why none of it
//! needs to be `Sync` and why every transition is trivially testable.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::state::DaemonState;
//! use astrs_time::HlcTimestamp;
//! use astrs_wire::{DataflowId, NodeId, NodeSource, NodeSpawnSpec};
//!
//! let mut state = DaemonState::new();
//! let dataflow = DataflowId::from_u128(1);
//! state.insert_dataflow(
//!     astrs_daemon::state::DataflowState::new(dataflow, HlcTimestamp::new(1, 0)),
//! );
//!
//! state
//!     .dataflow_mut(dataflow)
//!     .expect("just inserted")
//!     .add_node(NodeSpawnSpec::new(
//!         dataflow,
//!         NodeId::new("camera")?,
//!         0,
//!         NodeSource::Executable { path: "./camera".into() },
//!     ));
//!
//! assert_eq!(state.dataflow_count(), 1);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

pub mod dataflow;
pub mod node;
pub mod registry;
pub mod routes;

pub use dataflow::DataflowState;
pub use node::NodeState;
pub use registry::{DaemonState, SessionBinding};
pub use routes::{
    Consumer, DeliveryPlane, RouteTable, VIRTUAL_NODE, is_virtual_port, virtual_port_ref,
    virtual_source_text,
};
