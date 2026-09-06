//! The URDF object model: [`Robot`], [`Link`], [`Joint`] and their nested
//! elements — blueprint §5.3's "robot/link (inertial, visual, collision,
//! geometry box/cylinder/sphere/mesh-path, material)/joint (revolute/
//! continuous/prismatic/fixed/floating/planar, origin, axis, limits,
//! dynamics, mimic, safety-controller)" in full.
//!
//! This module owns the *data shape* and its structural validation
//! ([`Robot::validate`]); [`crate::parse`] is what actually builds a
//! [`Robot`] from XML, and [`crate::kinematics`] is what walks an
//! already-[`Robot::validate`]d one.

mod geometry;
mod joint;
mod link;
mod material;
mod robot;
mod validate;

pub use geometry::Geometry;
pub use joint::{Joint, JointDynamics, JointLimit, JointMimic, JointSafetyController};
pub use link::{Collision, InertiaTensor, Inertial, Link, Visual};
pub use material::{Color, Material};
pub use robot::Robot;
