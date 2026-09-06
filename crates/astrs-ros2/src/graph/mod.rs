//! ROS graph introspection: what nodes, topics, services and actions exist.
//!
//! Three modules, in the order a query passes through them:
//!
//! 1. [`wire`] — `rmw_dds_common`'s three graph types, hand-written because
//!    their `Gid` member changes width with the ROS distribution.
//! 2. [`announcer`] — this participant's own `ros_discovery_info` sample:
//!    which nodes it hosts and which endpoints each owns.
//! 3. [`cache`] — the join of every participant's sample with the RTPS
//!    discovery database, which is what `ros2 node list`,
//!    `ros2 topic list -t`, `ros2 service list` and `ros2 action list`
//!    read.

pub mod announcer;
pub mod cache;
pub mod wire;

pub use announcer::GraphAnnouncer;
pub use cache::{EndpointInfo, GraphCache, GraphQuery, NodeInfo};
pub use wire::{NodeEntitiesInfo, ParticipantEntitiesInfo};
