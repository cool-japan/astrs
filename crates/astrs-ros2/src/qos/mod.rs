//! ROS 2 QoS profiles, and how they map onto RTPS QoS.
//!
//! ROS 2 does not expose DDS's twenty-odd policies. It exposes seven, in one
//! flat [`QosProfile`], plus a set of named presets that cover almost every
//! real deployment. This module is that vocabulary and the two conversions
//! either side of it:
//!
//! ```text
//!   astrs_manifest::node::ros2::Qos  →  QosProfile  →  WriterQos / ReaderQos
//!            (the `ros2:` block)         (this crate)      (astrs-rtps)
//! ```
//!
//! # The asymmetry this module exists to hide
//!
//! `astrs-rtps` follows DDS exactly, and DDS gives a `DataWriter` and a
//! `DataReader` *different* defaults for `RELIABILITY`: a writer defaults to
//! `RELIABLE`, a reader to `BEST_EFFORT`. So a program that says nothing at
//! all gets a reliable publisher and a best-effort subscription, which pair
//! successfully and then behave like neither.
//!
//! ROS 2 does not have that asymmetry: `rmw_qos_profile_default` is
//! `RELIABLE`, `KEEP_LAST(10)`, `VOLATILE` on **both** sides.
//! [`QosProfile::default`] is that profile, and both
//! [`to_writer_qos`](QosProfile::to_writer_qos) and
//! [`to_reader_qos`](QosProfile::to_reader_qos) write every field
//! explicitly rather than leaning on `WriterQos::default()`/
//! `ReaderQos::default()`, so the DDS asymmetry can never leak through a
//! `..Default::default()`.
//!
//! # `SystemDefault`
//!
//! Three policies have a `SystemDefault` variant, matching
//! `RMW_QOS_POLICY_*_SYSTEM_DEFAULT`: it means "whatever the middleware
//! chooses". AstRS is the middleware, so it resolves to the same value
//! [`QosProfile::default`] uses — but it is a distinct variant rather than a
//! synonym, because `ros2 topic info --verbose` prints it distinctly and a
//! bridge that rewrites a peer's profile must not silently promote it.
//!
//! # Example
//!
//! ```
//! use astrs_ros2::qos::{QosProfile, Reliability};
//!
//! let sensor = QosProfile::sensor_data();
//! assert_eq!(sensor.reliability, Reliability::BestEffort);
//! assert_eq!(sensor.depth(), Some(5));
//!
//! // A reliable subscription cannot be served by a best-effort publisher.
//! let strict = QosProfile::default();
//! assert!(!strict.can_be_served_by(&sensor));
//! assert!(sensor.can_be_served_by(&strict));
//! ```

pub mod manifest;
pub mod profile;

pub use manifest::{from_manifest_qos, to_manifest_qos};
pub use profile::{
    Durability, History, Liveliness, QosProfile, Reliability, compatible, incompatible_policy,
};
