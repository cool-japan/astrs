//! ROS time ⇄ the AstRS hybrid logical clock.
//!
//! Three clocks meet in a bridged system and the blueprint (§10.4) asks this
//! crate to reconcile them:
//!
//! | Clock | Type | Zero | Monotonic |
//! |---|---|---|---|
//! | ROS time | [`RosTime`] | the Unix epoch, or a simulator's zero | no |
//! | RTPS source timestamp | [`astrs_rtps::structure::Time`] | the Unix epoch | no |
//! | AstRS HLC | [`astrs_time::HlcTimestamp`] | the Unix epoch, plus a logical counter | yes |
//!
//! # The one lossy edge, stated plainly
//!
//! An [`HlcTimestamp`](astrs_time::HlcTimestamp) is a physical nanosecond
//! count *and* a logical counter that breaks ties when two events share a
//! nanosecond. ROS time has no logical component, so
//! [`RosTime::from_hlc`] **drops it**: two HLC stamps that differ only in
//! their logical counter render as the same `builtin_interfaces/Time`.
//!
//! That is the correct trade rather than a defect — a ROS consumer has no
//! way to interpret a logical counter, and inventing nanoseconds to encode
//! one would make the timestamp lie about when the sample was taken. The
//! reverse direction, [`RosTime::to_hlc`], sets the logical counter to zero,
//! which is the identity for the physical ordering. What matters is that
//! the round trip `hlc → ros → hlc` is only lossless for a stamp whose
//! logical counter is already zero, and
//! [`RosTime::round_trips_losslessly`] answers that question rather than
//! leaving a caller to guess.
//!
//! # Negative time
//!
//! `builtin_interfaces/Time` is `int32 sec` + `uint32 nanosec`, so it can
//! express times before the epoch; an HLC timestamp is an unsigned
//! nanosecond count and cannot. [`RosTime::to_hlc`] therefore returns
//! `None` for a negative time rather than saturating to zero, because a
//! simulator that publishes `/clock` starting below zero is a real thing
//! and silently clamping it would put every sample at the epoch.
//!
//! # Example
//!
//! ```
//! use astrs_ros2::time::RosTime;
//! use astrs_time::HlcTimestamp;
//!
//! let stamp = RosTime::from_nanos(1_700_000_000_500_000_000);
//! assert_eq!(stamp.sec, 1_700_000_000);
//! assert_eq!(stamp.nanosec, 500_000_000);
//!
//! let hlc = stamp.to_hlc().expect("a positive time");
//! assert_eq!(RosTime::from_hlc(hlc), stamp);
//! assert!(RosTime::round_trips_losslessly(hlc));
//!
//! let ticked = HlcTimestamp::new(hlc.physical_ns(), 1);
//! assert_eq!(RosTime::from_hlc(ticked), stamp, "the logical counter is dropped");
//! assert!(!RosTime::round_trips_losslessly(ticked));
//! ```

pub mod clock;
pub mod stamp;

pub use clock::{ClockType, Ros2Clock};
pub use stamp::{NANOS_PER_SEC, RosDuration, RosTime};
