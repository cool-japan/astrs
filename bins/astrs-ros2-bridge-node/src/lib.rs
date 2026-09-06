//! The AstRS ROS 2 bridge node.
//!
//! An ordinary manifest node that a `ros2:` block configures declaratively
//! (blueprint §10.5): it joins the DDS domain through `astrs-rtps`,
//! subscribes or publishes the configured topics, services and actions with
//! the requested QoS, and converts CDR ⇄ columnar through the `astrs-idl`
//! generated types — preserving ROS header timestamps into HLC metadata.
//!
//! ```yaml
//!   - id: lidar-in
//!     ros2:
//!       compat: humble
//!       topic: /scan
//!       message_type: sensor_msgs/msg/LaserScan
//!       direction: to_astrs
//!       qos: { reliable: true, keep_last: 10 }
//!     outputs: [scan]
//! ```
//!
//! # Layer map
//!
//! ```text
//!   run       the event loop: Stop, restarts, clean participant teardown
//!   topic     ┐
//!   service   ├ one task per endpoint, both directions
//!   action    ┘
//!   resolve   `message_type:` → a codec, or a typed startup error
//!   codec     CDR ⇄ columnar, dispatched on a runtime type name
//!   plan      the `ros2:` block + declared ports → endpoints (pure)
//!   config    the spawn handshake → the `ros2:` block and the env knobs
//!   error     the taxonomy every layer reports through
//! ```
//!
//! Everything above `codec` is pure and unit-tested without a socket;
//! everything from `topic` up needs a participant, and is tested against a
//! second in-process one over loopback UDP (`tests/loopback_bridge.rs`),
//! which is the same shape §10.2's in-repo interoperability proof uses.
//!
//! # What the daemon hands this process
//!
//! Nothing special. `astrs-daemon` picks this binary because the node's
//! source is [`astrs_wire::NodeSource::Ros2Bridge`], and the `ros2:` block
//! rides inside that variant as JSON — see [`config`] for the contract and
//! for the two environment knobs (`ROS_DOMAIN_ID`, `AMENT_PREFIX_PATH`) that
//! are deployment facts rather than graph facts.
//!
//! # Example
//!
//! ```no_run
//! # fn main() -> Result<(), astrs_ros2_bridge_node::BridgeError> {
//! // What `fn main` does, in one call: join the dataflow, read the `ros2:`
//! // block out of the handshake, create every endpoint and pump until Stop.
//! astrs_ros2_bridge_node::run()?;
//! # Ok(())
//! # }
//! ```

pub mod action;
pub mod codec;
pub mod config;
pub mod error;
pub mod plan;
pub mod resolve;
pub mod run;
pub mod service;
pub mod topic;

pub use codec::{Decoded, MessageCodec};
pub use config::{BridgeSettings, bridge_config};
pub use error::{BridgeError, BridgeResult, CodecError, ConfigError, PlanError, ResolveError};
pub use plan::{ActionBridge, BridgePlan, ServiceBridge, TopicBridge, plan};
pub use resolve::{ResolvedPlan, resolve_plan};
pub use run::{Bridge, run, run_with};
