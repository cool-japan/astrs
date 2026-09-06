//! Coordinate frames the tf2 way.
//!
//! The transform contract robotics code is written against, implemented in
//! pure Rust (blueprint §10.6):
//!
//! - SE(3) types: translations, unit quaternions, transform composition and
//!   inversion ([`math`]).
//! - The frame tree: static and dynamic frames, parent/child validation and
//!   multi-publisher conflict detection ([`buffer`], [`frame`]).
//! - Time-travel lookup with interpolation and configurable buffer windows
//!   ([`buffer`], [`time`]).
//! - `/tf` and `/tf_static` topic bridging in both directions ([`bridge`]).
//!
//! # Module map
//!
//! ```text
//!   math      SE(3) types: Vector3, Quaternion, Isometry3   no crate deps
//!   time      TfStamp, TimePoint                            no crate deps
//!   error     TfError — the crate-wide error taxonomy        time
//!   frame     FrameId, FrameRegistry — the string interner    —
//!   buffer    TransformBuffer — static/dynamic frames,        math, time,
//!             time-travel lookup, cycle/connectivity checks   error, frame
//!   interop   geometry_msgs <-> Isometry3 conversions,        math, buffer,
//!             do_transform_* helpers                          astrs-idl
//!   bridge    TfMessage, StaticTransformAccumulator,          interop,
//!             /tf and /tf_static conventions                  buffer
//! ```
//!
//! # Scope
//!
//! This crate implements the tf2 *contract*: the frame tree, its math, and
//! wire interop. It deliberately does not implement a URDF parser or
//! kinematic-chain solver (`astrs-urdf`, a P1 stretch crate, blueprint
//! §5.3) — those consume this crate's [`buffer::TransformBuffer`], they are
//! not part of it.
//!
//! # Quick start
//!
//! Build a small frame tree — a static `map -> odom` offset and a `odom ->
//! base_link` frame that moves over one second — then look up the
//! composed transform, interpolated, at the halfway point:
//!
//! ```
//! use astrs_tf::{TfStamp, TimePoint, TransformBuffer};
//! use astrs_tf::math::{Isometry3, Vector3};
//!
//! # fn main() -> Result<(), astrs_tf::TfError> {
//! let mut buffer = TransformBuffer::new();
//!
//! // A fixed 10m offset from `map` to `odom`, valid at any query time.
//! buffer.set_transform(
//!     "map", "odom",
//!     Isometry3::from_translation(Vector3::new(10.0, 0.0, 0.0)),
//!     TfStamp::from_nanos(0),
//!     true, // static
//! )?;
//!
//! // `base_link` drives from odom's origin to +2m on X over one second.
//! buffer.set_transform(
//!     "odom", "base_link",
//!     Isometry3::from_translation(Vector3::new(0.0, 0.0, 0.0)),
//!     TfStamp::from_nanos(0),
//!     false, // dynamic
//! )?;
//! buffer.set_transform(
//!     "odom", "base_link",
//!     Isometry3::from_translation(Vector3::new(2.0, 0.0, 0.0)),
//!     TfStamp::from_nanos(1_000_000_000),
//!     false,
//! )?;
//!
//! // Halfway through that second, base_link has moved 1m — composed with
//! // the static 10m map->odom offset, "map"->"base_link" is 11m on X.
//! let halfway = TimePoint::At(TfStamp::from_nanos(500_000_000));
//! let map_from_base_link = buffer.lookup_transform("map", "base_link", halfway)?;
//! assert!((map_from_base_link.translation.x - 11.0).abs() < 1e-9);
//! # Ok(())
//! # }
//! ```

pub mod bridge;
pub mod buffer;
pub mod error;
pub mod frame;
pub mod interop;
pub mod math;
pub mod time;

pub use buffer::TransformBuffer;
pub use error::TfError;
pub use frame::{FrameId, FrameRegistry};
pub use math::{Isometry3, Quaternion, Vector3};
pub use time::{TfStamp, TimePoint};
