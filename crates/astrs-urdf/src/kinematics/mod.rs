//! Kinematic chains and forward kinematics over an already-parsed
//! [`crate::model::Robot`] — blueprint §5.3's "chain extraction, forward
//! kinematics (joint-position map -> per-link transforms; own small 3D
//! transform math consistent with astrs-tf conventions), conversion of the
//! fixed-frame skeleton into astrs-tf static transform messages."
//!
//! # Module map
//!
//! - [`Topology`] — the shared parent/child index [`Chain`] and
//!   [`forward_kinematics`] both build on, which also re-derives (rather
//!   than trusts) the tree-shape invariants [`crate::model::Robot::validate`]
//!   already checked (see that type's own "Trust boundary" docs).
//! - [`Chain`] — the classical base-to-tip joint sequence a manipulator's
//!   kinematics is usually described along.
//! - [`JointPosition`] — one joint's configuration, shaped per
//!   [`crate::JointKind::degrees_of_freedom`].
//! - [`forward_kinematics`] — the joint-position-map -> per-link-transform
//!   solver, including `<mimic>` resolution.
//! - [`populate_static_transforms`]/[`populate_home_pose_transforms`] — the
//!   bridge into `astrs_tf::buffer::TransformBuffer`.

mod chain;
mod forward;
mod position;
mod tf_bridge;
mod topology;

pub use chain::Chain;
pub use forward::forward_kinematics;
pub use position::JointPosition;
pub use tf_bridge::{populate_home_pose_transforms, populate_static_transforms};
pub use topology::Topology;
