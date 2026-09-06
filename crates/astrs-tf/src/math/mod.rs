//! `SE(3)` math, implemented in-crate (blueprint §10.6: "no external math
//! crate unless already on the §18.1 list" — none of `nalgebra`, `glam` or
//! `cgmath` are, so every rotation and rigid-body-transform operation this
//! crate needs is written here from first principles).
//!
//! | Type | Represents | `AstrsMessage` URN |
//! |---|---|---|
//! | [`vector3::Vector3`] | A translation, direction, or velocity | `std/geometry/v1/Vector3` |
//! | [`quaternion::Quaternion`] | An `SO(3)` rotation | `std/geometry/v1/Quaternion` |
//! | [`isometry::Isometry3`] | A rigid-body transform (`SE(3)`) | `std/geometry/v1/Transform` |
//!
//! All three implement `astrs_data::AstrsMessage` against the curated
//! `std/geometry/v1/*` layouts `astrs_data::urn::layouts::geometry` defines
//! — that module's own docs name this crate's transform tree as what
//! `transform_layout()` is built for. Deliberately *not* implemented here:
//! `Pose`/`Twist`/`Accel` (the rest of that curated set) — a `Pose` is
//! conceptually an `Isometry3` under different field names
//! (`position`/`orientation` rather than `translation`/`rotation`), which
//! Rust's coherence rules do not allow as a second `AstrsMessage` impl for
//! the same type, and `Twist`/`Accel` (velocities/accelerations) are
//! outside tf2's own contract entirely. See [`crate`]'s module docs for the
//! full scope boundary.

pub mod isometry;
pub mod quaternion;
pub mod vector3;

pub use isometry::Isometry3;
pub use quaternion::Quaternion;
pub use vector3::Vector3;
