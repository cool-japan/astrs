//! Nodes, and the context they live on.
//!
//! ROS 2 splits what a naive design would merge:
//!
//! | Type | Owns | There is one per |
//! |---|---|---|
//! | [`Ros2Context`] | the RTPS participant, the clock, the graph announcer | process, usually |
//! | [`Ros2Node`] | a name, a remapping table, parameters, a set of endpoints | logical node |
//!
//! A component container hosts a dozen nodes in one participant, which is
//! why `ros_discovery_info` exists at all — DDS discovery announces
//! endpoints, not nodes. [`Ros2Node::standalone`] is the one-node-per-
//! process convenience; [`Ros2Node::new`] is the general form.

pub mod context;
#[allow(clippy::module_inception)] // `node::node::Ros2Node`, re-exported here as `node::Ros2Node`.
pub mod node;
pub mod options;

pub use context::{LocalEndpoint, Ros2Context};
pub use node::Ros2Node;
pub use options::{ContextOptions, NodeOptions};
