//! Positive expansion tests: real `#[derive(AstrsMessage)]` uses, each
//! round-tripped through `to_record_batch`/`from_record_batch` and, where
//! the URN is one of `astrs-data`'s registered `std` types, cross-checked
//! against the registry's own layout (see [`assert_layout_matches_registry_if_known`]
//! and this crate's `lib.rs` docs on why that check is best-effort rather
//! than unconditional).
//!
//! Six representative structs (the task's own list, `Vector3`/`Quaternion`
//! counted as one "nested" pair): scalars plus a nullable field
//! ([`Vector3Msg`]), a struct nesting two other `AstrsMessage` types
//! ([`PoseMsg`], nesting [`Vector3Msg`] and [`QuaternionMsg`]),
//! `Vec<numeric>` and `Vec<[f32; 4]>` — blueprint §9.1's own `Detections`
//! example ([`DetectionsMsg`]), bare fixed arrays alongside more nesting
//! ([`ImuMsg`]), and `String`/`Vec<String>`/`bool` ([`TagSetMsg`] — the one
//! struct whose shape has no counterpart in the current closed `std`
//! registry, exercised precisely to prove the registry cross-check
//! degrades gracefully rather than failing on an unrelated shape).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_data::urn::layout_of_str;
use astrs_data::{AstrsMessage, DataType, TypeUrnError};
use astrs_operator_macros::AstrsMessage as AstrsMessageDerive;

/// Asserts `derived` agrees with the registry's own layout for `urn`, when
/// `urn` is registered — an unregistered URN (any shape not yet in the
/// closed `std` catalog) is not a failure here, since the derive's contract
/// is with the `std/v1` layout table only when the target type actually
/// has an entry in it (blueprint §3.4's append-only registry is a runtime
/// structure this proc-macro crate cannot consult from inside a
/// `#[proc_macro_derive]` — see `lib.rs`'s docs).
fn assert_layout_matches_registry_if_known(urn: &str, derived: &DataType) {
    match layout_of_str(urn) {
        Ok(registered) => assert!(
            derived.layout_eq(&registered),
            "derived layout for {urn} does not match the registry.\nderived:    {derived}\nregistered: {registered}"
        ),
        Err(TypeUrnError::UnknownType { .. }) => {
            // Not (yet) in the closed `std` registry: nothing to compare
            // against, and that is expected for this struct.
        }
        Err(other) => panic!("urn {urn} failed to resolve for an unexpected reason: {other}"),
    }
}

// ---------------------------------------------------------------------
// 1. Scalars, plus a nullable field.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, AstrsMessageDerive)]
#[astrs(urn = "std/geometry/v1/Vector3")]
struct Vector3Msg {
    x: f64,
    y: Option<f64>,
    z: f64,
}

#[test]
fn vector3_round_trips_and_matches_the_registry() {
    for value in [
        Vector3Msg {
            x: 1.0,
            y: Some(2.0),
            z: 3.0,
        },
        Vector3Msg {
            x: -1.5,
            y: None,
            z: 0.0,
        },
    ] {
        let batch = value.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(Vector3Msg::from_record_batch(&batch).unwrap(), value);
    }
    assert_layout_matches_registry_if_known(Vector3Msg::URN, &Vector3Msg::data_type());
}

// ---------------------------------------------------------------------
// 2 & 3. A struct nesting two other `AstrsMessage` types.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, AstrsMessageDerive)]
#[astrs(urn = "std/geometry/v1/Quaternion")]
struct QuaternionMsg {
    x: f64,
    y: f64,
    z: f64,
    w: f64,
}

#[derive(Debug, Clone, PartialEq, AstrsMessageDerive)]
#[astrs(urn = "std/geometry/v1/Pose")]
struct PoseMsg {
    position: Vector3Msg,
    orientation: QuaternionMsg,
}

#[test]
fn quaternion_round_trips_and_matches_the_registry() {
    let value = QuaternionMsg {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        w: 1.0,
    };
    let batch = value.to_record_batch().unwrap();
    assert_eq!(QuaternionMsg::from_record_batch(&batch).unwrap(), value);
    assert_layout_matches_registry_if_known(QuaternionMsg::URN, &QuaternionMsg::data_type());
}

#[test]
fn pose_nests_two_message_types_and_round_trips() {
    let value = PoseMsg {
        position: Vector3Msg {
            x: 1.0,
            y: Some(2.0),
            z: 3.0,
        },
        orientation: QuaternionMsg {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        },
    };
    let batch = value.to_record_batch().unwrap();
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(PoseMsg::from_record_batch(&batch).unwrap(), value);
    assert_layout_matches_registry_if_known(PoseMsg::URN, &PoseMsg::data_type());
}

// ---------------------------------------------------------------------
// 4. `Vec<numeric>` and `Vec<[f32; 4]>` — blueprint §9.1's own example.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, AstrsMessageDerive)]
#[astrs(urn = "std/vision/v1/Detections")]
struct DetectionsMsg {
    boxes: Vec<[f32; 4]>,
    scores: Vec<f32>,
    labels: Vec<u32>,
}

#[test]
fn detections_round_trips_and_matches_the_registry() {
    let value = DetectionsMsg {
        boxes: vec![[0.0, 0.0, 10.0, 20.0], [5.0, 5.0, 8.0, 8.0]],
        scores: vec![0.9, 0.5],
        labels: vec![1, 7],
    };
    let batch = value.to_record_batch().unwrap();
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(DetectionsMsg::from_record_batch(&batch).unwrap(), value);
    assert_layout_matches_registry_if_known(DetectionsMsg::URN, &DetectionsMsg::data_type());
}

#[test]
fn detections_round_trips_with_empty_vectors() {
    let value = DetectionsMsg {
        boxes: vec![],
        scores: vec![],
        labels: vec![],
    };
    let batch = value.to_record_batch().unwrap();
    assert_eq!(DetectionsMsg::from_record_batch(&batch).unwrap(), value);
}

// ---------------------------------------------------------------------
// 5. Bare fixed arrays, plus more nesting (three covariance matrices).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, AstrsMessageDerive)]
#[astrs(urn = "std/sensor/v1/Imu")]
struct ImuMsg {
    orientation: QuaternionMsg,
    orientation_covariance: [f64; 9],
    angular_velocity: Vector3Msg,
    angular_velocity_covariance: [f64; 9],
    linear_acceleration: Vector3Msg,
    linear_acceleration_covariance: [f64; 9],
}

#[test]
fn imu_round_trips_and_matches_the_registry() {
    let value = ImuMsg {
        orientation: QuaternionMsg {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        },
        orientation_covariance: [0.0; 9],
        angular_velocity: Vector3Msg {
            x: 0.1,
            y: None,
            z: 0.0,
        },
        angular_velocity_covariance: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        linear_acceleration: Vector3Msg {
            x: 0.0,
            y: Some(0.0),
            z: 9.81,
        },
        linear_acceleration_covariance: [0.0; 9],
    };
    let batch = value.to_record_batch().unwrap();
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(ImuMsg::from_record_batch(&batch).unwrap(), value);
    assert_layout_matches_registry_if_known(ImuMsg::URN, &ImuMsg::data_type());
}

// ---------------------------------------------------------------------
// 6. `String`, `Vec<String>` and `bool` — no counterpart in the current
//    registry, so this is the graceful-skip case.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, AstrsMessageDerive)]
#[astrs(urn = "std/core/v1/TagSet")]
struct TagSetMsg {
    name: String,
    active: bool,
    tags: Vec<String>,
}

#[test]
fn tag_set_round_trips_string_bool_and_string_list_fields() {
    let value = TagSetMsg {
        name: "camera-0".to_owned(),
        active: true,
        tags: vec![
            "front".to_owned(),
            "rgb".to_owned(),
            "calibrated".to_owned(),
        ],
    };
    let batch = value.to_record_batch().unwrap();
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(TagSetMsg::from_record_batch(&batch).unwrap(), value);
}

#[test]
fn tag_set_urn_is_not_in_the_closed_registry_yet() {
    // Documents *why* this struct's layout is not cross-checked above:
    // `std/core/v1/TagSet` is a syntactically well-formed, but currently
    // unregistered, URN.
    assert!(matches!(
        layout_of_str(TagSetMsg::URN),
        Err(TypeUrnError::UnknownType { .. })
    ));
}

#[test]
fn tag_set_round_trips_an_empty_tag_list_and_false_bool() {
    let value = TagSetMsg {
        name: String::new(),
        active: false,
        tags: vec![],
    };
    let batch = value.to_record_batch().unwrap();
    assert_eq!(TagSetMsg::from_record_batch(&batch).unwrap(), value);
}

// ---------------------------------------------------------------------
// Cross-cutting: every message is exactly one row, and the schema is the
// single `DATA_COLUMN` payload convention (blueprint §6.1).
// ---------------------------------------------------------------------

#[test]
fn every_message_type_uses_the_single_payload_column_convention() {
    let batch = Vector3Msg {
        x: 1.0,
        y: None,
        z: 1.0,
    }
    .to_record_batch()
    .unwrap();
    assert_eq!(batch.num_columns(), 1);
    assert!(batch.payload_column().is_some());
    assert_eq!(
        batch.schema().field(0).map(|f| f.name()),
        Some(astrs_data::DATA_COLUMN)
    );
}

#[test]
fn decoding_the_wrong_row_count_is_a_typed_error() {
    use astrs_data::array::{Float64Array, IntoArrayRef, StructArray};
    use astrs_data::{DataError, RecordBatch};

    // A two-row batch of the same layout `Vector3Msg` uses — decoding it as
    // a single message must fail with `MessageRowCount`, not silently read
    // row 0 or panic.
    let DataType::Struct(fields) = Vector3Msg::data_type() else {
        panic!("expected a struct layout")
    };
    let columns = vec![
        Float64Array::from_values([1.0, 2.0]).into_array_ref(),
        Float64Array::from_values([1.0, 2.0]).into_array_ref(),
        Float64Array::from_values([1.0, 2.0]).into_array_ref(),
    ];
    let strukt = StructArray::try_new(fields, columns, None).unwrap();
    let batch = RecordBatch::from_payload(strukt.into_array_ref());

    match Vector3Msg::from_record_batch(&batch) {
        Err(DataError::MessageRowCount { actual: 2 }) => {}
        other => panic!("expected MessageRowCount {{ actual: 2 }}, got {other:?}"),
    }
}
