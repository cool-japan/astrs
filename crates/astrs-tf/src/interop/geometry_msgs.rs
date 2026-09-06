//! `geometry_msgs` interop: conversions between this crate's native SE(3)
//! types ([`crate::math`]) and `astrs-idl`'s pre-generated
//! `geometry_msgs`/`std_msgs`/`builtin_interfaces` wire types (blueprint
//! §10.3), plus the `do_transform_*` family — the same computational core
//! ROS 2's `tf2_geometry_msgs` package provides, applying a looked-up
//! transform to a point, vector, quaternion or pose.
//!
//! # Two conversion layers
//!
//! - **Field-level** (`vector3_to_ros`/`vector3_from_ros`,
//!   `point_to_ros`/`point_from_ros`, `quaternion_to_ros`/`quaternion_from_ros`,
//!   `transform_to_isometry`/`isometry_to_transform`): plain functions, not
//!   `From`/`Into` impls. The orphan rule allows `From<geometry_msgs::Vector3>
//!   for Vector3` (the target type is local) but not the reverse
//!   (`geometry_msgs::Vector3` is foreign) — rather than one direction
//!   reading as a trait impl and the other as a function, both directions
//!   are plain functions throughout, which is one shape to remember instead
//!   of two.
//! - **Message-level** ([`StampedTransform`]'s [`From`]/[`TryFrom`] pair
//!   with `astrs_idl::generated::geometry_msgs::TransformStamped`): ROS to
//!   native is infallible ([`crate::time::TfStamp::from_ros_time`] always
//!   widens exactly); native to ROS is fallible
//!   ([`crate::time::TfStamp::to_ros_time`] can exceed
//!   `builtin_interfaces/Time`'s `i32`-second range), so this pair uses
//!   `From` and `TryFrom` respectively rather than forcing both through one
//!   shape.

use astrs_idl::generated::geometry_msgs::{
    Point, PointStamped, Pose, PoseStamped, Quaternion as RosQuaternion, QuaternionStamped,
    Transform as RosTransform, TransformStamped, Vector3 as RosVector3, Vector3Stamped,
};
use astrs_idl::generated::std_msgs::Header;

use crate::error::TfError;
use crate::math::{Isometry3, Quaternion, Vector3};
use crate::time::TfStamp;

// ---------------------------------------------------------------------
// Field-level conversions
// ---------------------------------------------------------------------

/// Native [`Vector3`] to `geometry_msgs/msg/Vector3`.
#[must_use]
pub fn vector3_to_ros(v: Vector3) -> RosVector3 {
    RosVector3 {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

/// `geometry_msgs/msg/Vector3` to native [`Vector3`].
#[must_use]
pub fn vector3_from_ros(v: &RosVector3) -> Vector3 {
    Vector3::new(v.x, v.y, v.z)
}

/// Native [`Vector3`] to `geometry_msgs/msg/Point`.
///
/// `Point` and `Vector3` are byte-identical `{x, y, z}: float64` shapes —
/// ROS 2 gives them different names for a *position* versus a *free
/// vector*, the same distinction [`Isometry3::transform_point`] and
/// [`Isometry3::transform_vector`] draw natively.
#[must_use]
pub fn point_to_ros(v: Vector3) -> Point {
    Point {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

/// `geometry_msgs/msg/Point` to native [`Vector3`].
#[must_use]
pub fn point_from_ros(p: &Point) -> Vector3 {
    Vector3::new(p.x, p.y, p.z)
}

/// Native [`Quaternion`] to `geometry_msgs/msg/Quaternion`.
#[must_use]
pub fn quaternion_to_ros(q: Quaternion) -> RosQuaternion {
    RosQuaternion {
        x: q.x,
        y: q.y,
        z: q.z,
        w: q.w,
    }
}

/// `geometry_msgs/msg/Quaternion` to native [`Quaternion`].
///
/// Does **not** normalize — a bare field-level conversion should not
/// silently rewrite its input; [`crate::buffer::TransformBuffer::set_transform`]
/// is where normalization belongs (it has a frame name to attach
/// [`TfError::DegenerateQuaternion`] to if normalization fails).
#[must_use]
pub fn quaternion_from_ros(q: &RosQuaternion) -> Quaternion {
    Quaternion::new(q.x, q.y, q.z, q.w)
}

/// Native [`Isometry3`] to `geometry_msgs/msg/Transform`.
#[must_use]
pub fn isometry_to_transform(t: Isometry3) -> RosTransform {
    RosTransform {
        translation: vector3_to_ros(t.translation),
        rotation: quaternion_to_ros(t.rotation),
    }
}

/// `geometry_msgs/msg/Transform` to native [`Isometry3`].
#[must_use]
pub fn transform_to_isometry(t: &RosTransform) -> Isometry3 {
    Isometry3::new(
        vector3_from_ros(&t.translation),
        quaternion_from_ros(&t.rotation),
    )
}

// ---------------------------------------------------------------------
// Message-level: StampedTransform <-> TransformStamped
// ---------------------------------------------------------------------

/// The buffer's native currency for one timestamped edge: everything
/// [`crate::buffer::TransformBuffer::set_transform`] needs, bundled as one
/// value — the native counterpart to
/// `astrs_idl::generated::geometry_msgs::TransformStamped`.
///
/// `frame`/`child_frame` are plain `String`s here rather than interned
/// [`crate::frame::FrameId`]s: this type crosses the wire (via
/// [`StampedTransform::try_into_transform_stamped`]/`From<&TransformStamped>`)
/// and is meaningful without any particular [`crate::buffer::TransformBuffer`]
/// in scope to intern into.
///
/// ```
/// use astrs_tf::TfStamp;
/// use astrs_tf::interop::geometry_msgs::StampedTransform;
/// use astrs_tf::math::Isometry3;
///
/// # fn main() -> Result<(), astrs_tf::TfError> {
/// let stamped = StampedTransform::new("map", "odom", Isometry3::IDENTITY, TfStamp::from_nanos(0));
/// let wire = stamped.try_into_transform_stamped()?;
/// assert_eq!(wire.header.frame_id, "map");
/// assert_eq!(wire.child_frame_id, "odom");
///
/// // ROS-to-native is infallible; the value round-trips exactly.
/// assert_eq!(StampedTransform::from(&wire), stamped);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct StampedTransform {
    /// The parent (reference) frame — `TransformStamped.header.frame_id`.
    pub parent_frame: String,
    /// The child frame — `TransformStamped.child_frame_id`.
    pub child_frame: String,
    /// When this transform was valid.
    pub stamp: TfStamp,
    /// The transform itself: parent ← child (see
    /// [`Isometry3`]'s "Convention" docs).
    pub transform: Isometry3,
}

impl StampedTransform {
    /// Builds a `StampedTransform` from its parts.
    #[must_use]
    pub fn new(
        parent_frame: impl Into<String>,
        child_frame: impl Into<String>,
        transform: Isometry3,
        stamp: TfStamp,
    ) -> Self {
        Self {
            parent_frame: parent_frame.into(),
            child_frame: child_frame.into(),
            stamp,
            transform,
        }
    }

    /// Converts to the wire type.
    ///
    /// # Errors
    ///
    /// [`TfError::RosTimeRangeExceeded`] when `self.stamp` does not fit
    /// `builtin_interfaces/Time`'s `i32`-second range (see
    /// [`TfStamp::to_ros_time`]).
    pub fn try_into_transform_stamped(&self) -> Result<TransformStamped, TfError> {
        Ok(TransformStamped {
            header: Header {
                stamp: self.stamp.to_ros_time()?,
                frame_id: self.parent_frame.clone(),
            },
            child_frame_id: self.child_frame.clone(),
            transform: isometry_to_transform(self.transform),
        })
    }
}

impl From<&TransformStamped> for StampedTransform {
    /// Always succeeds: [`TfStamp::from_ros_time`] never fails to widen.
    fn from(value: &TransformStamped) -> Self {
        Self {
            parent_frame: value.header.frame_id.clone(),
            child_frame: value.child_frame_id.clone(),
            stamp: TfStamp::from_ros_time(&value.header.stamp),
            transform: transform_to_isometry(&value.transform),
        }
    }
}

impl TryFrom<&StampedTransform> for TransformStamped {
    type Error = TfError;

    fn try_from(value: &StampedTransform) -> Result<Self, Self::Error> {
        value.try_into_transform_stamped()
    }
}

// ---------------------------------------------------------------------
// do_transform_* — tf2_geometry_msgs's computational core
// ---------------------------------------------------------------------

/// Applies `transform` to a `geometry_msgs/msg/Point`: rotates and
/// translates (a point has a position, so both apply).
#[must_use]
pub fn do_transform_point(transform: &Isometry3, point: &Point) -> Point {
    point_to_ros(transform.transform_point(point_from_ros(point)))
}

/// Applies `transform` to a `geometry_msgs/msg/Vector3`: rotates only — a
/// free vector (direction or velocity) has no position to translate.
#[must_use]
pub fn do_transform_vector3(transform: &Isometry3, vector: &RosVector3) -> RosVector3 {
    vector3_to_ros(transform.transform_vector(vector3_from_ros(vector)))
}

/// Applies `transform` to a `geometry_msgs/msg/Quaternion`: composes the
/// rotations (`transform.rotation ⊗ quaternion`).
#[must_use]
pub fn do_transform_quaternion(transform: &Isometry3, quaternion: &RosQuaternion) -> RosQuaternion {
    quaternion_to_ros(transform.rotation * quaternion_from_ros(quaternion))
}

/// Applies `transform` to a `geometry_msgs/msg/Pose`: the position
/// transforms as a point, the orientation as a quaternion.
#[must_use]
pub fn do_transform_pose(transform: &Isometry3, pose: &Pose) -> Pose {
    Pose {
        position: do_transform_point(transform, &pose.position),
        orientation: do_transform_quaternion(transform, &pose.orientation),
    }
}

/// Applies `transform` to a `geometry_msgs/msg/PointStamped`, re-stamping
/// the result with `transform`'s own parent frame and stamp — tf2's own
/// `doTransform(PointStamped, TransformStamped)` convention.
///
/// # Errors
///
/// [`TfError::RosTimeRangeExceeded`] from [`TfStamp::to_ros_time`].
pub fn do_transform_point_stamped(
    transform: &StampedTransform,
    point: &PointStamped,
) -> Result<PointStamped, TfError> {
    Ok(PointStamped {
        header: restamped_header(transform)?,
        point: do_transform_point(&transform.transform, &point.point),
    })
}

/// The `Vector3Stamped` counterpart of [`do_transform_point_stamped`].
///
/// # Errors
///
/// [`TfError::RosTimeRangeExceeded`] from [`TfStamp::to_ros_time`].
pub fn do_transform_vector3_stamped(
    transform: &StampedTransform,
    vector: &Vector3Stamped,
) -> Result<Vector3Stamped, TfError> {
    Ok(Vector3Stamped {
        header: restamped_header(transform)?,
        vector: do_transform_vector3(&transform.transform, &vector.vector),
    })
}

/// The `QuaternionStamped` counterpart of [`do_transform_point_stamped`].
///
/// # Errors
///
/// [`TfError::RosTimeRangeExceeded`] from [`TfStamp::to_ros_time`].
pub fn do_transform_quaternion_stamped(
    transform: &StampedTransform,
    quaternion: &QuaternionStamped,
) -> Result<QuaternionStamped, TfError> {
    Ok(QuaternionStamped {
        header: restamped_header(transform)?,
        quaternion: do_transform_quaternion(&transform.transform, &quaternion.quaternion),
    })
}

/// The `PoseStamped` counterpart of [`do_transform_point_stamped`].
///
/// # Errors
///
/// [`TfError::RosTimeRangeExceeded`] from [`TfStamp::to_ros_time`].
pub fn do_transform_pose_stamped(
    transform: &StampedTransform,
    pose: &PoseStamped,
) -> Result<PoseStamped, TfError> {
    Ok(PoseStamped {
        header: restamped_header(transform)?,
        pose: do_transform_pose(&transform.transform, &pose.pose),
    })
}

fn restamped_header(transform: &StampedTransform) -> Result<Header, TfError> {
    Ok(Header {
        stamp: transform.stamp.to_ros_time()?,
        frame_id: transform.parent_frame.clone(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::math::Vector3 as NativeVector3;
    use astrs_idl::generated::builtin_interfaces::Time;
    use std::f64::consts::FRAC_PI_2;

    fn quarter_turn_about_z() -> Isometry3 {
        Isometry3::new(
            NativeVector3::new(1.0, 0.0, 0.0),
            Quaternion::from_axis_angle(NativeVector3::UNIT_Z, FRAC_PI_2),
        )
    }

    #[test]
    fn vector3_field_conversion_round_trips() {
        let v = NativeVector3::new(1.0, 2.0, 3.0);
        assert_eq!(vector3_from_ros(&vector3_to_ros(v)), v);
    }

    #[test]
    fn point_field_conversion_round_trips() {
        let v = NativeVector3::new(4.0, 5.0, 6.0);
        assert_eq!(point_from_ros(&point_to_ros(v)), v);
    }

    #[test]
    fn quaternion_field_conversion_round_trips() {
        let q = Quaternion::from_axis_angle(NativeVector3::UNIT_X, 0.5);
        assert_eq!(quaternion_from_ros(&quaternion_to_ros(q)), q);
    }

    #[test]
    fn transform_field_conversion_round_trips() {
        let t = quarter_turn_about_z();
        let ros = isometry_to_transform(t);
        assert_eq!(transform_to_isometry(&ros), t);
    }

    #[test]
    fn stamped_transform_round_trips_through_transform_stamped() {
        let native = StampedTransform::new(
            "map",
            "odom",
            quarter_turn_about_z(),
            TfStamp::from_nanos(1_771_286_400_123_456_789),
        );
        let wire = native.try_into_transform_stamped().unwrap();
        assert_eq!(wire.header.frame_id, "map");
        assert_eq!(wire.child_frame_id, "odom");
        let back = StampedTransform::from(&wire);
        assert_eq!(back, native);
    }

    #[test]
    fn stamped_transform_conversion_rejects_a_time_range_overflow() {
        let native = StampedTransform::new("map", "odom", Isometry3::IDENTITY, TfStamp::MAX);
        assert!(matches!(
            native.try_into_transform_stamped(),
            Err(TfError::RosTimeRangeExceeded { .. })
        ));
        assert!(matches!(
            TransformStamped::try_from(&native),
            Err(TfError::RosTimeRangeExceeded { .. })
        ));
    }

    #[test]
    fn do_transform_point_rotates_and_translates() {
        let transform = quarter_turn_about_z(); // rotate 90 about Z, then +1 on X.
        let point = Point {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        };
        let result = do_transform_point(&transform, &point);
        // rotate (1,0,0) by 90 about Z -> (0,1,0); then translate +1 on X -> (1,1,0).
        assert!((result.x - 1.0).abs() < 1e-9);
        assert!((result.y - 1.0).abs() < 1e-9);
        assert!((result.z - 0.0).abs() < 1e-9);
    }

    #[test]
    fn do_transform_vector3_ignores_translation() {
        let transform = quarter_turn_about_z();
        let vector = RosVector3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        };
        let result = do_transform_vector3(&transform, &vector);
        assert!((result.x - 0.0).abs() < 1e-9);
        assert!((result.y - 1.0).abs() < 1e-9);
    }

    #[test]
    fn do_transform_pose_transforms_position_and_orientation() {
        let transform = quarter_turn_about_z();
        let pose = Pose {
            position: Point {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
            orientation: RosQuaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
        };
        let result = do_transform_pose(&transform, &pose);
        assert!((result.position.x - 1.0).abs() < 1e-9);
        assert!((result.position.y - 1.0).abs() < 1e-9);
        // The orientation picks up the transform's own 90-degree rotation.
        let rotated = quaternion_from_ros(&result.orientation);
        let (_, angle) = rotated.to_axis_angle();
        assert!((angle - FRAC_PI_2).abs() < 1e-9);
    }

    #[test]
    fn do_transform_point_stamped_restamps_with_the_transforms_own_header() {
        let transform = StampedTransform::new(
            "map",
            "odom",
            quarter_turn_about_z(),
            TfStamp::from_nanos(5_000_000_000),
        );
        let input = PointStamped {
            header: Header {
                stamp: Time { sec: 0, nanosec: 0 },
                frame_id: "odom".to_owned(),
            },
            point: Point {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
        };
        let output = do_transform_point_stamped(&transform, &input).unwrap();
        assert_eq!(output.header.frame_id, "map");
        assert_eq!(
            output.header.stamp,
            TfStamp::from_nanos(5_000_000_000).to_ros_time().unwrap()
        );
        assert!((output.point.y - 1.0).abs() < 1e-9);
    }

    #[test]
    fn do_transform_vector3_stamped_and_quaternion_stamped_restamp_too() {
        let transform = StampedTransform::new(
            "map",
            "odom",
            quarter_turn_about_z(),
            TfStamp::from_nanos(1_000),
        );
        let vector_in = Vector3Stamped {
            header: Header {
                stamp: Time { sec: 0, nanosec: 0 },
                frame_id: "odom".to_owned(),
            },
            vector: RosVector3 {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
        };
        let vector_out = do_transform_vector3_stamped(&transform, &vector_in).unwrap();
        assert_eq!(vector_out.header.frame_id, "map");

        let quat_in = QuaternionStamped {
            header: Header {
                stamp: Time { sec: 0, nanosec: 0 },
                frame_id: "odom".to_owned(),
            },
            quaternion: RosQuaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
        };
        let quat_out = do_transform_quaternion_stamped(&transform, &quat_in).unwrap();
        assert_eq!(quat_out.header.frame_id, "map");
    }

    #[test]
    fn do_transform_pose_stamped_propagates_a_time_range_error() {
        let transform = StampedTransform::new("map", "odom", Isometry3::IDENTITY, TfStamp::MAX);
        let pose = PoseStamped {
            header: Header {
                stamp: Time { sec: 0, nanosec: 0 },
                frame_id: "odom".to_owned(),
            },
            pose: Pose {
                position: Point {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                orientation: RosQuaternion {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 1.0,
                },
            },
        };
        assert!(matches!(
            do_transform_pose_stamped(&transform, &pose),
            Err(TfError::RosTimeRangeExceeded { .. })
        ));
    }
}
