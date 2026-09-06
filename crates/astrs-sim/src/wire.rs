//! Conversions between this crate's pure simulation types and the curated
//! `std` wire message types (`astrs_node_api::message`) — the boundary
//! `astrs-sim-node`, `astrs-sim-teleop` and `astrs-sim-logger` (this
//! crate's own node binaries) cross through and nothing else does. Every
//! other module in this crate (`kinematics`, `grid`, `lidar`, `odometry`,
//! `world`) works entirely in plain `f64`/[`Pose2D`]/[`Velocity2D`] terms,
//! with no `astrs_data`/`astrs_node_api` dependency of its own — matching
//! `astrs_urdf::math`'s own house rule (see that module's docs) of keeping
//! the hot simulation path free of the wire-encoding machinery and paying
//! that conversion cost exactly once, at the edge.
//!
//! # 2-D in, 3-D wire types out
//!
//! [`Pose2D`]/[`Velocity2D`] only ever describe motion confined to the
//! ground plane (`z = 0`, rotation about `Z` only) — see those types' own
//! docs. Every conversion here embeds that plane into the full 3-D wire
//! shape by construction: `z` is always `0.0`, and a rotation is always
//! pure-yaw (built by [`yaw_to_quaternion`], which never has a roll or
//! pitch component to set in the first place, not merely "happens to
//! produce one that is zero").

use astrs_node_api::message::{LaserScan, Odometry, Pose, Quaternion, Transform, Twist, Vector3};

use crate::kinematics::{Pose2D, Velocity2D};
use crate::lidar::LidarScan;

/// The quaternion for a pure yaw rotation of `theta` radians about `+Z`.
///
/// # Examples
///
/// ```
/// use astrs_sim::wire::{quaternion_to_yaw, yaw_to_quaternion};
///
/// let q = yaw_to_quaternion(1.2);
/// assert!((q.x, q.y) == (0.0, 0.0)); // pure yaw: no roll/pitch component
/// assert!((quaternion_to_yaw(q) - 1.2).abs() < 1e-12);
/// ```
#[must_use]
pub fn yaw_to_quaternion(theta: f64) -> Quaternion {
    let half = theta * 0.5;
    Quaternion::new(0.0, 0.0, half.sin(), half.cos())
}

/// The yaw angle a pure-yaw [`Quaternion`] (as [`yaw_to_quaternion`]
/// builds) represents.
///
/// Not a general roll/pitch/yaw decomposition — it assumes `x == y ==
/// 0.0`, which is exactly and only what a value this crate ever produces
/// itself satisfies. A `Quaternion` from elsewhere carrying roll/pitch
/// would have its out-of-plane components silently ignored rather than
/// reported as an error, since this crate has no 3-D pose concept to
/// report the discrepancy *as* — a caller decoding a genuinely arbitrary
/// 3-D orientation needs `astrs_tf::math::Quaternion`'s own machinery, not
/// this crate's 2-D-only shortcut.
#[must_use]
pub fn quaternion_to_yaw(rotation: Quaternion) -> f64 {
    2.0 * rotation.z.atan2(rotation.w)
}

/// [`Twist`] (`cmd_vel`'s wire shape) to [`Velocity2D`]: `linear.x` and
/// `angular.z` are this crate's whole model (see [module docs](self));
/// `linear.y`/`linear.z`/`angular.x`/`angular.y` are silently dropped, not
/// validated as zero — a `Twist` publisher is free to fill them
/// meaninglessly and this crate simply never looks.
#[must_use]
pub fn twist_to_velocity(twist: &Twist) -> Velocity2D {
    Velocity2D::new(twist.linear.x, twist.angular.z)
}

/// [`Velocity2D`] to [`Twist`] — the inverse embedding
/// [`twist_to_velocity`] projects out of; used by `astrs-sim-teleop` (this
/// crate's own `cmd_vel` source node) to publish a commanded velocity.
#[must_use]
pub fn velocity_to_twist(velocity: Velocity2D) -> Twist {
    Twist::new(
        Vector3::new(velocity.linear, 0.0, 0.0),
        Vector3::new(0.0, 0.0, velocity.angular),
    )
}

/// [`Pose2D`] to the wire [`Pose`] shape (position + orientation).
#[must_use]
pub fn pose2d_to_pose(pose: Pose2D) -> Pose {
    Pose::new(
        Vector3::new(pose.x, pose.y, 0.0),
        yaw_to_quaternion(pose.theta),
    )
}

/// [`Pose2D`] to the wire [`Transform`] shape (translation + rotation) —
/// what `astrs-sim-node` publishes on its `tf` output (the dynamic
/// `odom -> base_link` edge; see [`crate::World`]'s own docs on why that
/// is the one transform this crate's node publishes).
#[must_use]
pub fn pose2d_to_transform(pose: Pose2D) -> Transform {
    Transform::new(
        Vector3::new(pose.x, pose.y, 0.0),
        yaw_to_quaternion(pose.theta),
    )
}

/// Builds an [`Odometry`] message from an estimated pose and velocity,
/// with all-zero covariances — [`Odometry::from_estimate`]'s own
/// "unknown covariance" convention (see that function's docs), which is
/// an honest answer here: this crate's [`crate::odometry::OdometryModel`]
/// reports a noise *scale*, not a running covariance estimate (that would
/// mean this crate owning an actual Kalman-filter-shaped uncertainty
/// propagation, well beyond "kinematic playground"), so there is no
/// principled non-zero number to put in this message's covariance fields.
#[must_use]
pub fn odometry_message(pose: Pose2D, velocity: Velocity2D) -> Odometry {
    Odometry::from_estimate(pose2d_to_pose(pose), velocity_to_twist(velocity))
}

/// Converts a [`LidarScan`] to the wire [`LaserScan`] shape.
///
/// `scan_time_secs` is the wall/sim time the whole sweep is considered to
/// have taken — `astrs-sim-node` passes its own tick duration
/// ([`crate::Timestep::as_secs_f64`]), the same modeling choice a real
/// single-shot lidar driver publishing once per control tick would make.
/// `time_increment` is always `0.0`: this simulator's rays are all cast at
/// the same simulated instant (no per-beam rotation delay to model),
/// unlike a real spinning lidar.
#[must_use]
pub fn laser_scan_message(scan: &LidarScan, scan_time_secs: f32) -> LaserScan {
    let config = scan.config;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "std/sensor/v1/LaserScan's own wire contract stores every angle/range field as f32"
    )]
    LaserScan {
        angle_min: config.angle_min() as f32,
        angle_max: config.angle_max() as f32,
        angle_increment: config.angle_increment() as f32,
        time_increment: 0.0,
        scan_time: scan_time_secs,
        range_min: config.range_min() as f32,
        range_max: config.range_max() as f32,
        ranges: scan.ranges.clone(),
        intensities: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::grid::Grid;
    use crate::lidar::{LidarConfig, cast_scan};

    #[test]
    fn yaw_round_trips_through_the_quaternion_for_a_range_of_angles() {
        for theta in [0.0, 0.5, -0.5, 1.0, -3.0, std::f64::consts::PI - 0.01] {
            let q = yaw_to_quaternion(theta);
            assert!((q.x, q.y) == (0.0, 0.0));
            assert!(q.is_normalized(1e-12), "{theta}");
            assert!((quaternion_to_yaw(q) - theta).abs() < 1e-9, "theta={theta}");
        }
    }

    #[test]
    fn zero_yaw_is_the_identity_quaternion() {
        assert_eq!(yaw_to_quaternion(0.0), Quaternion::IDENTITY);
        assert_eq!(quaternion_to_yaw(Quaternion::IDENTITY), 0.0);
    }

    #[test]
    fn twist_and_velocity_round_trip_through_the_two_meaningful_components() {
        let velocity = Velocity2D::new(0.75, -1.5);
        let twist = velocity_to_twist(velocity);
        assert_eq!(twist.linear, Vector3::new(0.75, 0.0, 0.0));
        assert_eq!(twist.angular, Vector3::new(0.0, 0.0, -1.5));
        assert_eq!(twist_to_velocity(&twist), velocity);
    }

    #[test]
    fn twist_to_velocity_ignores_the_other_four_components() {
        let twist = Twist::new(Vector3::new(1.0, 99.0, 99.0), Vector3::new(99.0, 99.0, 2.0));
        assert_eq!(twist_to_velocity(&twist), Velocity2D::new(1.0, 2.0));
    }

    #[test]
    fn pose2d_conversions_place_everything_in_the_z_equals_zero_plane() {
        let pose = Pose2D::new(1.0, -2.0, 0.4);
        let wire_pose = pose2d_to_pose(pose);
        assert_eq!(wire_pose.position, Vector3::new(1.0, -2.0, 0.0));
        assert_eq!(wire_pose.orientation, yaw_to_quaternion(0.4));

        let transform = pose2d_to_transform(pose);
        assert_eq!(transform.translation, Vector3::new(1.0, -2.0, 0.0));
        assert_eq!(transform.rotation, yaw_to_quaternion(0.4));
    }

    #[test]
    fn odometry_message_carries_the_pose_and_twist_with_zero_covariance() {
        let odom = odometry_message(Pose2D::new(1.0, 2.0, 0.0), Velocity2D::new(0.5, 0.1));
        assert_eq!(odom.pose.position, Vector3::new(1.0, 2.0, 0.0));
        assert_eq!(odom.twist.linear, Vector3::new(0.5, 0.0, 0.0));
        assert_eq!(odom.twist.angular, Vector3::new(0.0, 0.0, 0.1));
        assert!(odom.pose_covariance.iter().all(|&v| v == 0.0));
        assert!(odom.twist_covariance.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn laser_scan_message_carries_the_configuration_and_ranges_through() {
        let grid = Grid::from_text("5 5 1.0\n#####\n#...#\n#...#\n#...#\n#####\n").unwrap();
        let config = LidarConfig::new(4, 0.0, 1.0, 0.1, 10.0).unwrap();
        let scan = cast_scan(Pose2D::new(2.5, 2.5, 0.0), &grid, &config);

        let message = laser_scan_message(&scan, 0.05);
        assert_eq!(message.angle_min, 0.0);
        assert_eq!(message.angle_max, 1.0);
        assert_eq!(message.range_min, 0.1);
        assert_eq!(message.range_max, 10.0);
        assert_eq!(message.scan_time, 0.05);
        assert_eq!(message.time_increment, 0.0);
        assert_eq!(message.ranges, scan.ranges);
        assert!(message.intensities.is_empty());
        assert_eq!(message.beam_count(), 4);
    }
}
