//! End-to-end conformance tests exercising `astrs-tf`'s public API the way
//! an external consumer would: build a frame tree, bridge it through the
//! `/tf` wire format (CDR bytes, not just in-memory structs), and confirm
//! the far side's lookups agree with the near side's.
//!
//! Unit tests inside each module (`src/**/*.rs`) cover one layer at a time;
//! this file is deliberately the one place that exercises the whole stack
//! together — buffer, interop and bridge — the same shape a real
//! `astrs-ros2` `/tf` bridge node would use them in.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_cdr::{from_bytes, to_vec_ros2};
use astrs_idl::generated::geometry_msgs::{Point, Pose, Quaternion as RosQuaternion};
use astrs_tf::bridge::{
    StaticTransformAccumulator, TF_STATIC_TOPIC, TF_TOPIC, TfMessage, ingest_tf_message,
};
use astrs_tf::error::{ExtrapolationDirection, FrameKind};
use astrs_tf::interop::geometry_msgs::{StampedTransform, do_transform_pose};
use astrs_tf::math::{Isometry3, Quaternion, Vector3};
use astrs_tf::{TfError, TfStamp, TimePoint, TransformBuffer};

fn translate(x: f64, y: f64, z: f64) -> Isometry3 {
    Isometry3::from_translation(Vector3::new(x, y, z))
}

fn nanos(n: i64) -> TfStamp {
    TfStamp::from_nanos(n)
}

fn at(n: i64) -> TimePoint {
    TimePoint::At(nanos(n))
}

/// A small but realistic tree: `map` (world origin, static) -> `odom`
/// (localization origin, static) -> `base_link` (the robot, dynamic,
/// moving along X over one second) -> `sensor` (rigidly mounted, static).
fn build_reference_buffer() -> TransformBuffer {
    let mut buffer = TransformBuffer::new();
    buffer
        .set_transform("map", "odom", translate(10.0, 0.0, 0.0), nanos(0), true)
        .expect("static map->odom");
    buffer
        .set_transform(
            "odom",
            "base_link",
            translate(0.0, 0.0, 0.0),
            nanos(0),
            false,
        )
        .expect("dynamic odom->base_link @ t=0");
    buffer
        .set_transform(
            "odom",
            "base_link",
            translate(2.0, 0.0, 0.0),
            nanos(1_000_000_000),
            false,
        )
        .expect("dynamic odom->base_link @ t=1s");
    buffer
        .set_transform(
            "base_link",
            "sensor",
            Isometry3::new(Vector3::new(0.0, 0.0, 0.5), Quaternion::IDENTITY),
            nanos(0),
            true,
        )
        .expect("static base_link->sensor");
    buffer
}

#[test]
fn topic_names_match_ros_2_tf2_conventions() {
    assert_eq!(TF_TOPIC, "/tf");
    assert_eq!(TF_STATIC_TOPIC, "/tf_static");
}

#[test]
fn reference_buffer_multi_hop_lookup_at_several_stamps() {
    let buffer = build_reference_buffer();

    // At t=0: base_link is at odom's origin, so map->sensor is
    // map->odom (10) + odom->base_link (0) + base_link->sensor (0,0,0.5).
    let at_start = buffer.lookup_transform("map", "sensor", at(0)).unwrap();
    assert!((at_start.translation.x - 10.0).abs() < 1e-9);
    assert!((at_start.translation.z - 0.5).abs() < 1e-9);

    // At t=1s: base_link has moved +2 on X.
    let at_end = buffer
        .lookup_transform("map", "sensor", at(1_000_000_000))
        .unwrap();
    assert!((at_end.translation.x - 12.0).abs() < 1e-9);

    // At t=0.5s: interpolated halfway (+1 on X).
    let at_mid = buffer
        .lookup_transform("map", "sensor", at(500_000_000))
        .unwrap();
    assert!((at_mid.translation.x - 11.0).abs() < 1e-9);
}

/// The mandated end-to-end claim: `/tf` round trip through CDR bytes back
/// into a *fresh* buffer produces identical lookups to the original.
#[test]
fn tf_message_round_trips_through_cdr_bytes_and_reproduces_lookups() {
    let reference = build_reference_buffer();

    // Snapshot every dynamic sample of odom->base_link as a TfMessage the
    // way a `/tf` publisher would (one message per broadcast cycle here,
    // both samples together for a compact test).
    let dynamic_wire = TfMessage::from_stamped_transforms(&[
        StampedTransform::new("odom", "base_link", translate(0.0, 0.0, 0.0), nanos(0)),
        StampedTransform::new(
            "odom",
            "base_link",
            translate(2.0, 0.0, 0.0),
            nanos(1_000_000_000),
        ),
    ])
    .unwrap();
    let dynamic_bytes = to_vec_ros2(&dynamic_wire).unwrap();
    let dynamic_decoded: TfMessage = from_bytes(&dynamic_bytes).unwrap();
    assert_eq!(dynamic_decoded, dynamic_wire);

    // Snapshot the static edges through the accumulator, the way
    // `tf2_ros::StaticTransformBroadcaster` would for `/tf_static`.
    let mut accumulator = StaticTransformAccumulator::new();
    accumulator.set(StampedTransform::new(
        "map",
        "odom",
        translate(10.0, 0.0, 0.0),
        nanos(0),
    ));
    accumulator.set(StampedTransform::new(
        "base_link",
        "sensor",
        Isometry3::new(Vector3::new(0.0, 0.0, 0.5), Quaternion::IDENTITY),
        nanos(0),
    ));
    let static_wire = accumulator.snapshot().unwrap();
    let static_bytes = to_vec_ros2(&static_wire).unwrap();
    let static_decoded: TfMessage = from_bytes(&static_bytes).unwrap();
    assert_eq!(static_decoded, static_wire);

    // Ingest both, in either order, into a brand new buffer — never
    // touching `reference` again from here on.
    let mut rebuilt = TransformBuffer::new();
    ingest_tf_message(&mut rebuilt, &static_decoded, true).unwrap();
    ingest_tf_message(&mut rebuilt, &dynamic_decoded, false).unwrap();

    for stamp in [0, 250_000_000, 500_000_000, 750_000_000, 1_000_000_000] {
        let expected = reference
            .lookup_transform("map", "sensor", at(stamp))
            .unwrap();
        let actual = rebuilt
            .lookup_transform("map", "sensor", at(stamp))
            .unwrap();
        assert!(
            (expected.translation - actual.translation).norm() < 1e-9,
            "at t={stamp}: expected {expected:?}, got {actual:?}"
        );
        assert!(
            (expected.rotation.dot(actual.rotation).abs() - 1.0).abs() < 1e-9,
            "at t={stamp}: rotation mismatch {expected:?} vs {actual:?}"
        );
    }

    // The frame kinds also carried through correctly: static stayed
    // static, dynamic stayed dynamic.
    assert_eq!(rebuilt.frame_kind("odom"), Some(FrameKind::Static));
    assert_eq!(rebuilt.frame_kind("base_link"), Some(FrameKind::Dynamic));
    assert_eq!(rebuilt.frame_kind("sensor"), Some(FrameKind::Static));
}

#[test]
fn static_transform_survives_any_query_time_after_ingestion() {
    let message = TfMessage::from_stamped_transforms(&[StampedTransform::new(
        "map",
        "odom",
        translate(3.0, 0.0, 0.0),
        nanos(5_000),
    )])
    .unwrap();
    let bytes = to_vec_ros2(&message).unwrap();
    let decoded: TfMessage = from_bytes(&bytes).unwrap();

    let mut buffer = TransformBuffer::new();
    ingest_tf_message(&mut buffer, &decoded, true).unwrap();

    for stamp in [0, 5_000, i64::MAX] {
        let result = buffer.lookup_transform("map", "odom", at(stamp)).unwrap();
        assert!((result.translation.x - 3.0).abs() < 1e-9);
    }
}

#[test]
fn dynamic_lookup_outside_the_buffered_range_reports_which_bound_and_direction() {
    let mut buffer = TransformBuffer::with_max_history(Duration::from_secs(10));
    buffer
        .set_transform(
            "odom",
            "base_link",
            translate(0.0, 0.0, 0.0),
            nanos(1_000),
            false,
        )
        .unwrap();
    buffer
        .set_transform(
            "odom",
            "base_link",
            translate(1.0, 0.0, 0.0),
            nanos(2_000),
            false,
        )
        .unwrap();

    let past = buffer
        .lookup_transform("odom", "base_link", at(0))
        .unwrap_err();
    assert_eq!(
        past,
        TfError::Extrapolation {
            frame: "base_link".to_owned(),
            requested: nanos(0),
            bound: nanos(1_000),
            direction: ExtrapolationDirection::Past,
        }
    );

    let future = buffer
        .lookup_transform("odom", "base_link", at(3_000))
        .unwrap_err();
    assert_eq!(
        future,
        TfError::Extrapolation {
            frame: "base_link".to_owned(),
            requested: nanos(3_000),
            bound: nanos(2_000),
            direction: ExtrapolationDirection::Future,
        }
    );
}

#[test]
fn disconnected_forest_reports_a_typed_error_end_to_end() {
    let mut buffer = TransformBuffer::new();
    buffer
        .set_transform("map", "odom", translate(0.0, 0.0, 0.0), nanos(0), true)
        .unwrap();
    buffer
        .set_transform(
            "other_world",
            "drone",
            translate(0.0, 0.0, 0.0),
            nanos(0),
            true,
        )
        .unwrap();

    assert!(!buffer.can_transform("odom", "drone", TimePoint::Latest));
    assert_eq!(
        buffer.lookup_transform("odom", "drone", TimePoint::Latest),
        Err(TfError::Disconnected {
            target_frame: "odom".to_owned(),
            source_frame: "drone".to_owned(),
        })
    );
    // The graph itself is still valid (no cycle) — disconnection is not a
    // structural defect, just something a lookup between the two roots
    // cannot answer.
    assert_eq!(buffer.validate(), Ok(()));
}

#[test]
fn a_cycle_introduced_via_the_public_api_is_rejected() {
    let mut buffer = TransformBuffer::new();
    buffer
        .set_transform("a", "b", translate(1.0, 0.0, 0.0), nanos(0), true)
        .unwrap();
    buffer
        .set_transform("b", "c", translate(1.0, 0.0, 0.0), nanos(0), true)
        .unwrap();
    let err = buffer
        .set_transform("c", "a", translate(1.0, 0.0, 0.0), nanos(0), true)
        .unwrap_err();
    assert!(matches!(err, TfError::Cycle { .. }));
    // The rejected edge never took effect: "a" is still rootless (it was
    // never given a parent), not "c".
    assert_eq!(buffer.parent_of("a"), None);
}

#[test]
fn frame_kind_conflict_ingesting_the_same_child_as_both_static_and_dynamic() {
    let mut buffer = TransformBuffer::new();
    let message = TfMessage::from_stamped_transforms(&[StampedTransform::new(
        "map",
        "odom",
        translate(0.0, 0.0, 0.0),
        nanos(0),
    )])
    .unwrap();
    ingest_tf_message(&mut buffer, &message, true).unwrap(); // via /tf_static
    let err = ingest_tf_message(&mut buffer, &message, false).unwrap_err(); // then /tf
    assert!(matches!(err, TfError::FrameKindConflict { .. }));
}

#[test]
fn do_transform_pose_end_to_end_through_a_looked_up_buffer_transform() {
    let mut buffer = TransformBuffer::new();
    buffer
        .set_transform(
            "map",
            "odom",
            Isometry3::new(
                Vector3::new(1.0, 0.0, 0.0),
                Quaternion::from_axis_angle(Vector3::UNIT_Z, std::f64::consts::FRAC_PI_2),
            ),
            nanos(0),
            true,
        )
        .unwrap();

    let map_from_odom = buffer
        .lookup_transform("map", "odom", TimePoint::Latest)
        .unwrap();

    let pose_in_odom = Pose {
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
    let pose_in_map = do_transform_pose(&map_from_odom, &pose_in_odom);

    // Rotate (1,0,0) by 90 about Z -> (0,1,0), then translate +1 on X -> (1,1,0).
    assert!((pose_in_map.position.x - 1.0).abs() < 1e-9);
    assert!((pose_in_map.position.y - 1.0).abs() < 1e-9);
}

#[test]
fn remove_frame_then_rekind_round_trips_through_the_buffer_again() {
    let mut buffer = TransformBuffer::new();
    buffer
        .set_transform("map", "odom", translate(0.0, 0.0, 0.0), nanos(0), true)
        .unwrap();
    assert_eq!(buffer.frame_kind("odom"), Some(FrameKind::Static));

    assert!(buffer.remove_frame("odom"));
    buffer
        .set_transform("map", "odom", translate(5.0, 0.0, 0.0), nanos(0), false)
        .unwrap();
    assert_eq!(buffer.frame_kind("odom"), Some(FrameKind::Dynamic));
    let result = buffer
        .lookup_transform("map", "odom", TimePoint::Latest)
        .unwrap();
    assert!((result.translation.x - 5.0).abs() < 1e-9);
}
