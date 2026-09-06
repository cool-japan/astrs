//! Typed send and receive over the real wire, for the `std` URN registry
//! (blueprint §9.2, §24.3).
//!
//! Each test publishes a typed value from one node and decodes it in another,
//! through the frame codec and the Arrow IPC payload encoding. Round-tripping
//! a type against itself in memory proves the encoder and the decoder agree;
//! this proves they agree *across the wire*, which is the property a dataflow
//! actually depends on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_node_api::message::{
    Accel, AstrsMessage, BoundingBox, Bytes, CompressedImage, Detections, Duration as TimeDuration,
    Flag, Imu, Keypoint, Keypoints, LaserScan, Mask, NavSatFix, OccupancyGrid, Odometry, Path,
    Pose, Quaternion, Range, Scalar, ScalarRun, StampedPose, Text, Timestamp, Transform, Twist,
    Vector3, assert_registry_layout,
};
use astrs_node_api::testing::TestHarness;

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

/// Publishes `value` from one node and decodes it in another.
fn round_trip<T>(value: T) -> T
where
    T: AstrsMessage + astrs_node_api::message::FromPayload + PartialEq + core::fmt::Debug,
{
    let (mut producer, mut consumer) =
        TestHarness::pair("sender", "port", "receiver", "port").unwrap();
    let mut output = producer.node.output::<T>("port").unwrap();
    output.publish(value).unwrap();

    let event = consumer
        .events
        .recv_timeout(WAIT)
        .unwrap()
        .expect("the typed message");
    let (_, _, payload) = event.into_input().expect("an input");
    payload.view::<T>().expect("a decodable payload")
}

#[test]
fn geometry_types_survive_the_wire() {
    let pose = Pose::new(Vector3::new(1.0, 2.0, 3.0), Quaternion::identity());
    assert_eq!(round_trip(pose), pose);

    let transform = Transform::new(Vector3::new(-1.0, 0.0, 0.5), Quaternion::identity());
    assert_eq!(round_trip(transform), transform);

    let twist = Twist::new(Vector3::new(1.0, 0.0, 0.0), Vector3::new(0.0, 0.0, 0.2));
    assert_eq!(round_trip(twist), twist);

    let accel = Accel::new(Vector3::new(0.0, 0.0, -9.81), Vector3::ZERO);
    assert_eq!(round_trip(accel), accel);

    let vector = Vector3::new(4.0, 5.0, 6.0);
    assert_eq!(round_trip(vector), vector);

    let rotation = Quaternion::new(0.0, 0.0, 0.0, 1.0);
    assert_eq!(round_trip(rotation), rotation);
}

#[test]
fn vision_types_survive_the_wire() {
    let detections = Detections::new(
        vec![
            BoundingBox::new(0.0, 0.0, 10.0, 20.0),
            BoundingBox::new(30.0, 40.0, 5.0, 5.0),
        ],
        vec![0.93, 0.41],
        vec![7, 12],
    )
    .unwrap();
    assert_eq!(round_trip(detections.clone()), detections);

    let keypoints = Keypoints::new(
        vec![Keypoint::new(1.0, 2.0), Keypoint::new(3.0, 4.0)],
        vec![0.5, 0.25],
        vec![0, 1],
    )
    .unwrap();
    assert_eq!(round_trip(keypoints.clone()), keypoints);

    let mask = Mask::new(4, 2, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    assert_eq!(round_trip(mask.clone()), mask);
}

#[test]
fn sensor_types_survive_the_wire() {
    let imu = Imu::from_readings(
        Quaternion::identity(),
        Vector3::new(0.0, 0.0, 0.1),
        Vector3::new(0.0, 0.0, -9.81),
    );
    assert_eq!(round_trip(imu.clone()), imu);

    let scan = LaserScan {
        angle_min: -1.5,
        angle_max: 1.5,
        angle_increment: 0.5,
        time_increment: 0.001,
        scan_time: 0.1,
        range_min: 0.05,
        range_max: 30.0,
        ranges: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
        intensities: Vec::new(),
    };
    assert_eq!(round_trip(scan.clone()), scan);

    let fix = NavSatFix::at_coordinate(35.681_2, 139.767_1, 40.0);
    assert_eq!(round_trip(fix.clone()), fix);

    let range = Range {
        radiation_type: 1,
        field_of_view: 0.26,
        min_range: 0.1,
        max_range: 4.0,
        range: 1.2,
    };
    assert_eq!(round_trip(range), range);
}

#[test]
fn navigation_types_survive_the_wire() {
    let odometry = Odometry::from_estimate(
        Pose::new(Vector3::new(1.0, 2.0, 0.0), Quaternion::identity()),
        Twist::new(Vector3::new(0.5, 0.0, 0.0), Vector3::new(0.0, 0.0, 0.1)),
    );
    assert_eq!(round_trip(odometry.clone()), odometry);

    let path = Path::new(vec![
        StampedPose::new(Timestamp::from_nanos(1), Pose::default()),
        StampedPose::new(Timestamp::from_nanos(2), Pose::default()),
    ]);
    assert_eq!(round_trip(path.clone()), path);

    let grid =
        OccupancyGrid::new(0.05, 3, 2, Pose::default(), vec![-1, 0, 50, 100, 0, -1]).unwrap();
    assert_eq!(round_trip(grid.clone()), grid);
}

#[test]
fn media_and_scalar_types_survive_the_wire() {
    let frame = CompressedImage::new("jpeg", vec![0xFF, 0xD8, 0xFF, 0xE0]);
    assert_eq!(round_trip(frame.clone()), frame);

    assert_eq!(round_trip(Scalar::from(2.5_f64)).into_inner(), 2.5);
    assert_eq!(round_trip(Scalar::from(-7_i32)).into_inner(), -7);
    assert_eq!(
        round_trip(ScalarRun::from(vec![1.0_f32, 2.0, 3.0])).as_slice(),
        &[1.0, 2.0, 3.0]
    );
    assert_eq!(round_trip(Text::from("hello")).as_str(), "hello");
    assert_eq!(round_trip(Bytes::new(vec![1, 2, 3])).as_slice(), &[1, 2, 3]);
    assert!(round_trip(Flag::from(true)).get());
    assert_eq!(
        round_trip(Timestamp::from_nanos(1_700_000_000)).nanos(),
        1_700_000_000
    );
    assert_eq!(
        round_trip(TimeDuration::from_millis(-250)).nanos(),
        -250_000_000
    );
}

#[test]
fn every_type_this_crate_ships_conforms_to_the_registry() {
    // The one check that catches "my encoder and my decoder agree with each
    // other, and both disagree with §24.3".
    assert_registry_layout::<Vector3>().unwrap();
    assert_registry_layout::<Quaternion>().unwrap();
    assert_registry_layout::<Pose>().unwrap();
    assert_registry_layout::<Transform>().unwrap();
    assert_registry_layout::<Twist>().unwrap();
    assert_registry_layout::<Accel>().unwrap();
    assert_registry_layout::<Detections>().unwrap();
    assert_registry_layout::<Keypoints>().unwrap();
    assert_registry_layout::<Mask>().unwrap();
    assert_registry_layout::<Imu>().unwrap();
    assert_registry_layout::<LaserScan>().unwrap();
    assert_registry_layout::<NavSatFix>().unwrap();
    assert_registry_layout::<Range>().unwrap();
    assert_registry_layout::<Odometry>().unwrap();
    assert_registry_layout::<Path>().unwrap();
    assert_registry_layout::<OccupancyGrid>().unwrap();
    assert_registry_layout::<Scalar<f64>>().unwrap();
    assert_registry_layout::<Text>().unwrap();
    assert_registry_layout::<Bytes>().unwrap();
    assert_registry_layout::<Flag>().unwrap();
    assert_registry_layout::<Timestamp>().unwrap();
    assert_registry_layout::<TimeDuration>().unwrap();
}

#[test]
fn a_typed_handle_reports_the_urn_it_publishes() {
    let mut harness = TestHarness::start().unwrap();
    let output = harness
        .node
        .output::<Odometry>(TestHarness::DEFAULT_OUTPUT)
        .unwrap();
    assert_eq!(output.type_urn(), "std/nav/v1/Odometry");
}

#[test]
fn a_payload_of_the_wrong_type_is_refused_rather_than_reinterpreted() {
    let (mut producer, mut consumer) =
        TestHarness::pair("sender", "port", "receiver", "port").unwrap();
    let mut output = producer.node.output::<Vector3>("port").unwrap();
    output.publish(Vector3::new(1.0, 2.0, 3.0)).unwrap();

    let event = consumer
        .events
        .recv_timeout(WAIT)
        .unwrap()
        .expect("a message");
    let (_, _, payload) = event.into_input().unwrap();
    assert!(payload.view::<Vector3>().is_ok());
    assert!(
        payload.view::<Imu>().is_err(),
        "a Vector3 is not an Imu, however much they are both structs of floats"
    );
}
