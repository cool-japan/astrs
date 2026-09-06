//! Proves the pre-generated `common_interfaces` set is not merely
//! *syntactically* valid Rust (`generated_matches_source.rs`'s concern) but
//! *semantically* correct: every generated type round-trips through both
//! `astrs_cdr::CdrSerde` (the wire) and `astrs_data::AstrsMessage` (the
//! columnar payload), and at least one worked example is checked against a
//! byte-exact CDR vector derived by hand from the OMG CDR alignment rules —
//! the same standard `astrs-cdr`'s own golden vectors hold themselves to
//! (blueprint §10.1), applied here to codegen's *output* rather than to the
//! serializer itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only.

use astrs_data::AstrsMessage;
use astrs_idl::generated::builtin_interfaces::{Duration, Time};
use astrs_idl::generated::geometry_msgs::{Point, Quaternion, Vector3};

fn cdr_round_trip<T>(value: &T)
where
    T: astrs_cdr::CdrSerde + PartialEq + std::fmt::Debug,
{
    let bytes = astrs_cdr::to_vec_ros2(value).expect("encode");
    let decoded: T = astrs_cdr::from_bytes(&bytes).expect("decode");
    assert_eq!(&decoded, value, "CDR round trip changed the value");
}

fn columnar_round_trip<T>(value: &T)
where
    T: AstrsMessage + PartialEq + std::fmt::Debug,
{
    let batch = value.to_record_batch().expect("encode to columnar");
    assert_eq!(
        batch.num_rows(),
        1,
        "one message is one row (blueprint §6.1)"
    );
    let decoded = T::from_record_batch(&batch).expect("decode from columnar");
    assert_eq!(&decoded, value, "columnar round trip changed the value");
}

#[test]
fn every_worked_type_round_trips_through_cdr_and_columnar() {
    let time = Time {
        sec: 1_700_000_000,
        nanosec: 123_456_789,
    };
    cdr_round_trip(&time);
    columnar_round_trip(&time);

    let duration = Duration {
        sec: -5,
        nanosec: 250_000_000,
    };
    cdr_round_trip(&duration);
    columnar_round_trip(&duration);

    let point = Point {
        x: 1.5,
        y: -2.25,
        z: 0.0,
    };
    cdr_round_trip(&point);
    columnar_round_trip(&point);

    let vector = Vector3 {
        x: 0.0,
        y: 9.81,
        z: -1.0,
    };
    cdr_round_trip(&vector);
    columnar_round_trip(&vector);

    let quaternion = Quaternion {
        x: 0.1,
        y: 0.2,
        z: 0.3,
        w: 0.9,
    };
    cdr_round_trip(&quaternion);
    columnar_round_trip(&quaternion);
}

#[test]
fn quaternion_default_is_the_identity_rotation() {
    // `geometry_msgs/msg/Quaternion.msg` declares `x 0`, `y 0`, `z 0`, `w 1`
    // — the identity quaternion, not the all-zero IDL default `w` would get
    // without an explicit default. Proves codegen's explicit-default path,
    // not just its no-default path.
    let identity = Quaternion::default();
    assert_eq!(
        identity,
        Quaternion {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0
        }
    );
    assert_eq!(
        <Quaternion as astrs_cdr::CdrDefault>::cdr_default(),
        identity
    );
}

#[test]
fn time_and_duration_default_to_the_idl_zero_value() {
    assert_eq!(Time::default(), Time { sec: 0, nanosec: 0 });
    assert_eq!(Duration::default(), Duration { sec: 0, nanosec: 0 });
}

#[test]
fn point_cdr_encoding_matches_a_hand_derived_golden_vector() {
    // `geometry_msgs/msg/Point`: three `float64` members, no padding
    // possible between 8-byte-aligned fields starting right after the
    // 4-byte ROS 2 (`CDR_LE`) encapsulation header. Derivation:
    //   offset 0..4   encapsulation header: 00 01 00 00 (CDR_LE, no options)
    //   offset 4..12  x = 1.0   -> f64 LE bytes of 1.0f64
    //   offset 12..20 y = 2.0   -> f64 LE bytes of 2.0f64
    //   offset 20..28 z = -3.5  -> f64 LE bytes of -3.5f64
    let point = Point {
        x: 1.0,
        y: 2.0,
        z: -3.5,
    };
    let bytes = astrs_cdr::to_vec_ros2(&point).expect("encode");

    let mut expected = vec![0x00, 0x01, 0x00, 0x00];
    expected.extend_from_slice(&1.0f64.to_le_bytes());
    expected.extend_from_slice(&2.0f64.to_le_bytes());
    expected.extend_from_slice(&(-3.5f64).to_le_bytes());

    assert_eq!(bytes, expected);
    assert_eq!(bytes.len(), 4 + 24);
    assert_eq!(
        astrs_cdr::from_bytes::<Point>(&bytes).expect("decode"),
        point
    );
}

#[test]
fn time_cdr_encoding_matches_a_hand_derived_golden_vector() {
    // `builtin_interfaces/msg/Time`: `int32 sec` then `uint32 nanosec`, both
    // 4-byte aligned with no padding between two consecutive 4-byte fields.
    let time = Time {
        sec: -1,
        nanosec: 42,
    };
    let bytes = astrs_cdr::to_vec_ros2(&time).expect("encode");
    let mut expected = vec![0x00, 0x01, 0x00, 0x00];
    expected.extend_from_slice(&(-1i32).to_le_bytes());
    expected.extend_from_slice(&42u32.to_le_bytes());
    assert_eq!(bytes, expected);
}

#[test]
fn ros_and_dds_type_names_follow_the_rosidl_convention() {
    assert_eq!(Point::ROS_TYPE_NAME, "geometry_msgs/msg/Point");
    assert_eq!(Point::DDS_TYPE_NAME, "geometry_msgs::msg::dds_::Point_");
    assert_eq!(Time::ROS_TYPE_NAME, "builtin_interfaces/msg/Time");
    assert_eq!(Time::DDS_TYPE_NAME, "builtin_interfaces::msg::dds_::Time_");
}

#[test]
fn urns_are_well_formed_and_resolve_through_type_urn_parse() {
    for urn in [
        Point::URN,
        Vector3::URN,
        Quaternion::URN,
        Time::URN,
        Duration::URN,
    ] {
        let parsed = astrs_data::TypeUrn::parse(urn).unwrap_or_else(|e| panic!("{urn}: {e}"));
        assert_eq!(parsed.as_str(), urn);
        assert_eq!(parsed.category(), "ros2");
    }
}

#[test]
fn every_worked_type_reports_the_correct_columnar_shape() {
    use astrs_data::{DataType, Field};

    assert_eq!(
        Point::data_type(),
        DataType::strukt([
            Field::required("x", DataType::Float64),
            Field::required("y", DataType::Float64),
            Field::required("z", DataType::Float64),
        ])
    );
    assert_eq!(
        Time::data_type(),
        DataType::strukt([
            Field::required("sec", DataType::Int32),
            Field::required("nanosec", DataType::UInt32)
        ])
    );
}
