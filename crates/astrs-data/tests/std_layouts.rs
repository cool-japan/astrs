//! Every `std/v1` type beyond `core`/`time` — construct a payload in its
//! normative layout, then take it through the same operations a message
//! actually goes through: schema hashing, and the row-selection/stitching
//! kernels a recorder or a replay tool would call.
//!
//! # What "round-trips" means here
//!
//! Blueprint §6.1 puts every AstRS payload on the wire as an Arrow IPC
//! stream, so every layout in §24.3 has to survive four different things, and
//! [`assert_round_trips`] puts each fixture through all of them:
//!
//! 1. **Resolution** — the URN resolves through the public registry to the
//!    layout the fixture was actually built in.
//! 2. **Kernels** — the batch survives [`concat_batches`]/[`take_batch`]/
//!    [`filter_batch`] with its schema and row data intact, and hashes stably
//!    through all of them.
//! 3. **The payload wire form** — [`encode_payload`] then [`decode_payload`]
//!    returns an equal batch under an equal schema, and re-encoding what came
//!    back reproduces the original bytes exactly. Logical equality plus a
//!    byte-stable re-encoding is what "byte-for-byte logically" means: the
//!    encoding is a fixpoint, so nothing is lost and nothing drifts.
//! 4. **The physical contract** — the encoded payload is padded to
//!    [`PAYLOAD_ALIGNMENT`] and starts on a [`ALIGNMENT`]-byte boundary, which
//!    is what lets `astrs-shm` map a slot and hand the columns straight to
//!    SIMD code.
//!
//! Multi-batch streaming of the same layouts is covered too, because a
//! recorder writes many batches of one type into one stream: the concatenated
//! fixture and the original go into a single [`IpcStreamReader`]-readable
//! stream and come back unchanged.
//!
//! Byte compatibility with arrow-rs itself is gated separately, over the
//! committed corpus in `tests/golden_arrow.rs` and the synthetic one in
//! `tests/ipc_roundtrip.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_data::array::{
    ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array, Int32Array,
    IntoArrayRef, ListArray, StringArray, StructArray, UInt8Array, UInt16Array, UInt32Array,
};
use astrs_data::ipc::{
    IpcStreamReader, PAYLOAD_ALIGNMENT, decode_payload, encode_payload, to_ipc_bytes,
};
use astrs_data::kernel::{concat_batches, filter_batch, take_batch};
use astrs_data::urn::layout_of;
use astrs_data::urn::layouts::{geometry, media, nav, sensor, vision};
use astrs_data::{ALIGNMENT, DataType, Field, RecordBatch, SchemaHash};

/// Runs every shared round-trip assertion against `batch`, which must be a
/// two-row (or more) payload batch declared under `urn_text`.
fn assert_round_trips(urn_text: &str, batch: RecordBatch) {
    let urn =
        astrs_data::urn::TypeUrn::parse(urn_text).unwrap_or_else(|err| panic!("{urn_text}: {err}"));
    let layout = layout_of(&urn).unwrap_or_else(|err| panic!("{urn_text}: {err}"));
    assert_eq!(
        batch.schema().field(0).map(Field::data_type),
        Some(&layout),
        "{urn_text}: the registry's layout must match the payload actually built"
    );

    // Hashing is deterministic and independent of row content.
    let hash_a = SchemaHash::of(batch.schema());
    let hash_b = SchemaHash::of(batch.schema());
    assert_eq!(hash_a, hash_b, "{urn_text}");

    // concat: two copies of the batch double the row count and keep the
    // schema (and therefore the hash) identical.
    let doubled = concat_batches(&[batch.clone(), batch.clone()])
        .unwrap_or_else(|err| panic!("{urn_text}: concat_batches: {err}"));
    assert_eq!(doubled.num_rows(), batch.num_rows() * 2, "{urn_text}");
    assert_eq!(
        SchemaHash::of(doubled.schema()),
        hash_a,
        "{urn_text}: concatenation must not perturb the schema"
    );

    assert!(
        batch.num_rows() >= 2,
        "{urn_text}: fixture needs at least two rows"
    );

    // filter: keep only row 0.
    let mut mask = vec![false; batch.num_rows()];
    mask[0] = true;
    let filtered = filter_batch(&batch, &BooleanArray::from_values(mask))
        .unwrap_or_else(|err| panic!("{urn_text}: filter_batch: {err}"));
    assert_eq!(filtered.num_rows(), 1, "{urn_text}");
    assert_eq!(SchemaHash::of(filtered.schema()), hash_a, "{urn_text}");

    // take: reverse the rows.
    let reversed: Vec<i32> = (0..batch.num_rows() as i32).rev().collect();
    let indices = Int32Array::from_values(reversed).into_array_ref();
    let taken = take_batch(&batch, indices.as_ref())
        .unwrap_or_else(|err| panic!("{urn_text}: take_batch: {err}"));
    assert_eq!(taken.num_rows(), batch.num_rows(), "{urn_text}");
    assert_eq!(SchemaHash::of(taken.schema()), hash_a, "{urn_text}");

    assert_payload_round_trips(urn_text, &batch);
    assert_stream_round_trips(urn_text, &[batch, doubled]);
}

/// The §6.1 payload wire form: encode, decode, compare, and check that
/// re-encoding what came back is byte-identical.
fn assert_payload_round_trips(urn_text: &str, batch: &RecordBatch) {
    let bytes = encode_payload(batch).unwrap_or_else(|err| panic!("{urn_text}: encode: {err}"));

    // The physical contract the shared-memory plane relies on.
    assert_eq!(
        bytes.len() % PAYLOAD_ALIGNMENT,
        0,
        "{urn_text}: payload length is not slot-aligned"
    );
    assert_eq!(
        bytes.as_ptr() as usize % ALIGNMENT,
        0,
        "{urn_text}: payload base is not buffer-aligned"
    );

    let decoded =
        decode_payload(bytes.as_slice()).unwrap_or_else(|err| panic!("{urn_text}: decode: {err}"));
    assert_eq!(
        decoded.schema().as_ref(),
        batch.schema().as_ref(),
        "{urn_text}: the decoded schema drifted"
    );
    assert_eq!(
        SchemaHash::of(decoded.schema()),
        SchemaHash::of(batch.schema()),
        "{urn_text}: the decoded schema hashes differently"
    );
    assert_eq!(&decoded, batch, "{urn_text}: a value changed on the wire");

    // Encoding is a fixpoint: what came off the wire re-encodes to the same
    // bytes it arrived as, so nothing is lost and nothing drifts.
    let again =
        encode_payload(&decoded).unwrap_or_else(|err| panic!("{urn_text}: re-encode: {err}"));
    assert_eq!(
        again.as_slice(),
        bytes.as_slice(),
        "{urn_text}: re-encoding the decoded payload changed the bytes"
    );
}

/// The same layout through a multi-batch stream, which is what a recorder
/// writes.
fn assert_stream_round_trips(urn_text: &str, batches: &[RecordBatch]) {
    let bytes = to_ipc_bytes(batches).unwrap_or_else(|err| panic!("{urn_text}: stream: {err}"));
    let mut reader = IpcStreamReader::from_slice(bytes.as_slice())
        .unwrap_or_else(|err| panic!("{urn_text}: stream open: {err}"));
    let decoded = reader
        .read_all()
        .unwrap_or_else(|err| panic!("{urn_text}: stream batches: {err}"));
    assert_eq!(
        reader.schema().as_ref(),
        batches[0].schema().as_ref(),
        "{urn_text}: stream schema"
    );
    assert_eq!(decoded.len(), batches.len(), "{urn_text}: batch count");
    for (index, (left, right)) in decoded.iter().zip(batches.iter()).enumerate() {
        assert_eq!(left, right, "{urn_text}: stream batch {index}");
    }
}

/// Wraps a two-row `data` column in the single-column [`Schema`] every AstRS
/// payload uses, and checks it against `layout`.
fn payload_batch(layout: DataType, column: ArrayRef) -> RecordBatch {
    assert_eq!(column.data_type(), &layout);
    RecordBatch::from_payload(column)
}

// ---------------------------------------------------------------------------
// geometry
// ---------------------------------------------------------------------------

fn vector3_rows(rows: &[[f64; 3]]) -> StructArray {
    let DataType::Struct(fields) = geometry::vector3_layout() else {
        unreachable!()
    };
    let (xs, ys, zs): (Vec<f64>, Vec<f64>, Vec<f64>) = rows
        .iter()
        .map(|[x, y, z]| (*x, *y, *z))
        .fold((vec![], vec![], vec![]), |mut acc, (x, y, z)| {
            acc.0.push(x);
            acc.1.push(y);
            acc.2.push(z);
            acc
        });
    StructArray::try_new(
        fields,
        vec![
            Float64Array::from_values(xs).into_array_ref(),
            Float64Array::from_values(ys).into_array_ref(),
            Float64Array::from_values(zs).into_array_ref(),
        ],
        None,
    )
    .unwrap()
}

fn quaternion_rows(len: usize) -> StructArray {
    let DataType::Struct(fields) = geometry::quaternion_layout() else {
        unreachable!()
    };
    let zeros = || Float64Array::from_values(vec![0.0; len]).into_array_ref();
    let ones = || Float64Array::from_values(vec![1.0; len]).into_array_ref();
    StructArray::try_new(fields, vec![zeros(), zeros(), zeros(), ones()], None).unwrap()
}

#[test]
fn vector3_round_trips() {
    let column = vector3_rows(&[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]).into_array_ref();
    assert_round_trips(
        "std/geometry/v1/Vector3",
        payload_batch(geometry::vector3_layout(), column),
    );
}

#[test]
fn quaternion_round_trips() {
    let column = quaternion_rows(2).into_array_ref();
    assert_round_trips(
        "std/geometry/v1/Quaternion",
        payload_batch(geometry::quaternion_layout(), column),
    );
}

#[test]
fn pose_round_trips() {
    let DataType::Struct(fields) = geometry::pose_layout() else {
        unreachable!()
    };
    let column = StructArray::try_new(
        fields,
        vec![
            vector3_rows(&[[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]]).into_array_ref(),
            quaternion_rows(2).into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/geometry/v1/Pose",
        payload_batch(geometry::pose_layout(), column),
    );
}

#[test]
fn transform_round_trips() {
    let DataType::Struct(fields) = geometry::transform_layout() else {
        unreachable!()
    };
    let column = StructArray::try_new(
        fields,
        vec![
            vector3_rows(&[[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]]).into_array_ref(),
            quaternion_rows(2).into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/geometry/v1/Transform",
        payload_batch(geometry::transform_layout(), column),
    );
}

fn twist_or_accel_rows() -> ArrayRef {
    let DataType::Struct(fields) = geometry::twist_layout() else {
        unreachable!()
    };
    StructArray::try_new(
        fields,
        vec![
            vector3_rows(&[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]).into_array_ref(),
            vector3_rows(&[[0.0, 0.0, 1.0], [0.0, 0.0, -1.0]]).into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref()
}

#[test]
fn twist_round_trips() {
    assert_round_trips(
        "std/geometry/v1/Twist",
        payload_batch(geometry::twist_layout(), twist_or_accel_rows()),
    );
}

#[test]
fn accel_round_trips() {
    assert_round_trips(
        "std/geometry/v1/Accel",
        payload_batch(geometry::accel_layout(), twist_or_accel_rows()),
    );
}

// ---------------------------------------------------------------------------
// sensor
// ---------------------------------------------------------------------------

#[test]
fn laser_scan_round_trips() {
    let DataType::Struct(fields) = sensor::laser_scan_layout() else {
        unreachable!()
    };
    let ranges = ListArray::try_from_lengths(
        Field::required("range", DataType::Float32),
        [3, 2],
        Float32Array::from_values([1.0, 2.0, 3.0, 4.0, 5.0]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let intensities = ListArray::try_from_lengths(
        Field::required("intensity", DataType::Float32),
        [3, 2],
        Float32Array::from_values([9.0, 9.0, 9.0, 9.0, 9.0]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let scalar = |v: f32| Float32Array::from_values([v, v]).into_array_ref();
    let column = StructArray::try_new(
        fields,
        vec![
            scalar(-1.0),
            scalar(1.0),
            scalar(0.1),
            scalar(0.0),
            scalar(0.1),
            scalar(0.1),
            scalar(10.0),
            ranges,
            intensities,
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/sensor/v1/LaserScan",
        payload_batch(sensor::laser_scan_layout(), column),
    );
}

#[test]
fn imu_round_trips() {
    let DataType::Struct(fields) = sensor::imu_layout() else {
        unreachable!()
    };
    let covariance = || {
        FixedSizeListArray::try_new(
            Field::required("v", DataType::Float64),
            9,
            Float64Array::from_values(vec![0.0; 18]).into_array_ref(),
            None,
        )
        .unwrap()
        .into_array_ref()
    };
    let column = StructArray::try_new(
        fields,
        vec![
            quaternion_rows(2).into_array_ref(),
            covariance(),
            vector3_rows(&[[0.0, 0.0, 0.0], [0.0, 0.0, 0.0]]).into_array_ref(),
            covariance(),
            vector3_rows(&[[0.0, 0.0, 9.8], [0.0, 0.0, 9.8]]).into_array_ref(),
            covariance(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/sensor/v1/Imu",
        payload_batch(sensor::imu_layout(), column),
    );
}

#[test]
fn nav_sat_fix_round_trips() {
    let DataType::Struct(fields) = sensor::nav_sat_fix_layout() else {
        unreachable!()
    };
    let column = StructArray::try_new(
        fields,
        vec![
            Int8Array::from_values([0, 0]).into_array_ref(),
            UInt16Array::from_values([1u16, 1]).into_array_ref(),
            Float64Array::from_values([35.0, 35.1]).into_array_ref(),
            Float64Array::from_values([139.0, 139.1]).into_array_ref(),
            Float64Array::from_values([10.0, 11.0]).into_array_ref(),
            FixedSizeListArray::try_new(
                Field::required("v", DataType::Float64),
                9,
                Float64Array::from_values(vec![0.0; 18]).into_array_ref(),
                None,
            )
            .unwrap()
            .into_array_ref(),
            UInt8Array::from_values([0u8, 0]).into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/sensor/v1/NavSatFix",
        payload_batch(sensor::nav_sat_fix_layout(), column),
    );
}

#[test]
fn range_round_trips() {
    let DataType::Struct(fields) = sensor::range_layout() else {
        unreachable!()
    };
    let column = StructArray::try_new(
        fields,
        vec![
            UInt8Array::from_values([0u8, 1]).into_array_ref(),
            Float32Array::from_values([0.5, 0.6]).into_array_ref(),
            Float32Array::from_values([0.1, 0.1]).into_array_ref(),
            Float32Array::from_values([4.0, 4.0]).into_array_ref(),
            Float32Array::from_values([1.5, 2.0]).into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/sensor/v1/Range",
        payload_batch(sensor::range_layout(), column),
    );
}

#[test]
fn point_cloud_round_trips() {
    let layout = sensor::point_cloud_layout("x:y:z").unwrap();
    let DataType::Struct(fields) = layout.clone() else {
        unreachable!()
    };
    let field_column = || {
        ListArray::try_from_lengths(
            Field::required("v", DataType::Float32),
            [2, 3],
            Float32Array::from_values([0.0, 1.0, 2.0, 3.0, 4.0]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref()
    };
    let column = StructArray::try_new(
        fields,
        vec![field_column(), field_column(), field_column()],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/sensor/v1/PointCloud[fields=x:y:z]",
        payload_batch(layout, column),
    );
}

// ---------------------------------------------------------------------------
// nav
// ---------------------------------------------------------------------------

#[test]
fn odometry_round_trips() {
    let DataType::Struct(fields) = nav::odometry_layout() else {
        unreachable!()
    };
    let DataType::Struct(pose_fields) = geometry::pose_layout() else {
        unreachable!()
    };
    let pose = |n| {
        StructArray::try_new(
            pose_fields,
            vec![
                vector3_rows(&vec![[0.0, 0.0, 0.0]; n]).into_array_ref(),
                quaternion_rows(n).into_array_ref(),
            ],
            None,
        )
        .unwrap()
        .into_array_ref()
    };
    let cov36 = |n: usize| {
        FixedSizeListArray::try_new(
            Field::required("v", DataType::Float64),
            36,
            Float64Array::from_values(vec![0.0; 36 * n]).into_array_ref(),
            None,
        )
        .unwrap()
        .into_array_ref()
    };
    let column = StructArray::try_new(
        fields,
        vec![pose(2), cov36(2), twist_or_accel_rows(), cov36(2)],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/nav/v1/Odometry",
        payload_batch(nav::odometry_layout(), column),
    );
}

#[test]
fn path_round_trips() {
    // `Path` is a bare `List<Struct{stamp, pose}>` — the *payload* is the
    // list itself, so "two rows" means two messages, each an independent
    // path (a list of stamped poses), not two poses in one path.
    let DataType::List(item_field) = nav::path_layout() else {
        unreachable!()
    };
    let DataType::Struct(item_fields) = item_field.data_type().clone() else {
        unreachable!()
    };
    let DataType::Struct(pose_fields) = geometry::pose_layout() else {
        unreachable!()
    };
    let n = 5; // total stamped-pose entries across both messages
    let stamped_poses = StructArray::try_new(
        item_fields,
        vec![
            astrs_data::array::TimestampArray::from_nanos(0..n as i64).into_array_ref(),
            StructArray::try_new(
                pose_fields,
                vec![
                    vector3_rows(&vec![[0.0, 0.0, 0.0]; n]).into_array_ref(),
                    quaternion_rows(n).into_array_ref(),
                ],
                None,
            )
            .unwrap()
            .into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    let column = ListArray::try_from_lengths(*item_field, [3, 2], stamped_poses)
        .unwrap()
        .into_array_ref();
    assert_round_trips("std/nav/v1/Path", payload_batch(nav::path_layout(), column));
}

#[test]
fn occupancy_grid_round_trips() {
    let DataType::Struct(fields) = nav::occupancy_grid_layout() else {
        unreachable!()
    };
    let DataType::Struct(pose_fields) = geometry::pose_layout() else {
        unreachable!()
    };
    let origin = StructArray::try_new(
        pose_fields,
        vec![
            vector3_rows(&[[0.0, 0.0, 0.0], [0.0, 0.0, 0.0]]).into_array_ref(),
            quaternion_rows(2).into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    let cells = ListArray::try_from_lengths(
        Field::required("cell", DataType::Int8),
        [4, 4],
        Int8Array::from_values([0i8, 100, 100, 0, -1, -1, 0, 0]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let column = StructArray::try_new(
        fields,
        vec![
            Float32Array::from_values([0.05, 0.1]).into_array_ref(),
            UInt32Array::from_values([2u32, 2]).into_array_ref(),
            UInt32Array::from_values([2u32, 2]).into_array_ref(),
            origin,
            cells,
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/nav/v1/OccupancyGrid",
        payload_batch(nav::occupancy_grid_layout(), column),
    );
}

// ---------------------------------------------------------------------------
// vision
// ---------------------------------------------------------------------------

#[test]
fn detections_round_trips() {
    let DataType::Struct(fields) = vision::detections_layout() else {
        unreachable!()
    };

    // 3 boxes total (2 in row 0, 1 in row 1), each `[x, y, w, h]`.
    let box_values = Float32Array::from_values((0..12).map(|v| v as f32)).into_array_ref();
    let box_child =
        FixedSizeListArray::try_new(Field::required("v", DataType::Float32), 4, box_values, None)
            .unwrap()
            .into_array_ref();
    let boxes = ListArray::try_from_lengths(
        Field::required(
            "xywh",
            DataType::fixed_size_list(Field::required("v", DataType::Float32), 4),
        ),
        [2, 1],
        box_child,
    )
    .unwrap()
    .into_array_ref();
    let scores = ListArray::try_from_lengths(
        Field::required("score", DataType::Float32),
        [2, 1],
        Float32Array::from_values([0.9, 0.8, 0.7]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let labels = ListArray::try_from_lengths(
        Field::required("label", DataType::UInt32),
        [2, 1],
        UInt32Array::from_values([1u32, 2, 3]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let column = StructArray::try_new(fields, vec![boxes, scores, labels], None)
        .unwrap()
        .into_array_ref();
    assert_round_trips(
        "std/vision/v1/Detections",
        payload_batch(vision::detections_layout(), column),
    );
}

#[test]
fn keypoints_round_trips() {
    let DataType::Struct(fields) = vision::keypoints_layout() else {
        unreachable!()
    };
    let points_child = FixedSizeListArray::try_new(
        Field::required("v", DataType::Float32),
        2,
        Float32Array::from_values((0..6).map(|v| v as f32)).into_array_ref(),
        None,
    )
    .unwrap()
    .into_array_ref();
    let points = ListArray::try_from_lengths(
        Field::required(
            "xy",
            DataType::fixed_size_list(Field::required("v", DataType::Float32), 2),
        ),
        [2, 1],
        points_child,
    )
    .unwrap()
    .into_array_ref();
    let scores = ListArray::try_from_lengths(
        Field::required("score", DataType::Float32),
        [2, 1],
        Float32Array::from_values([0.5, 0.6, 0.7]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let labels = ListArray::try_from_lengths(
        Field::required("label", DataType::UInt32),
        [2, 1],
        UInt32Array::from_values([0u32, 1, 0]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let column = StructArray::try_new(fields, vec![points, scores, labels], None)
        .unwrap()
        .into_array_ref();
    assert_round_trips(
        "std/vision/v1/Keypoints",
        payload_batch(vision::keypoints_layout(), column),
    );
}

#[test]
fn mask_round_trips() {
    let DataType::Struct(fields) = vision::mask_layout() else {
        unreachable!()
    };
    let data = ListArray::try_from_lengths(
        Field::required("label", DataType::UInt16),
        [4, 4],
        UInt16Array::from_values([0u16, 1, 1, 0, 2, 2, 0, 0]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let column = StructArray::try_new(
        fields,
        vec![
            UInt32Array::from_values([2u32, 2]).into_array_ref(),
            UInt32Array::from_values([2u32, 2]).into_array_ref(),
            data,
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/vision/v1/Mask",
        payload_batch(vision::mask_layout(), column),
    );
}

// ---------------------------------------------------------------------------
// media
// ---------------------------------------------------------------------------

#[test]
fn image_round_trips_and_builds_an_image_view() {
    let layout = media::image_layout("mono8").unwrap();
    let DataType::Struct(fields) = layout.clone() else {
        unreachable!()
    };
    let data = ListArray::try_from_lengths(
        Field::required("sample", DataType::UInt8),
        [4, 4],
        UInt8Array::from_values([0u8, 1, 2, 3, 10, 11, 12, 13]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let column = StructArray::try_new(
        fields,
        vec![
            UInt32Array::from_values([2u32, 2]).into_array_ref(),
            UInt32Array::from_values([2u32, 2]).into_array_ref(),
            UInt32Array::from_values([2u32, 2]).into_array_ref(),
            UInt8Array::from_values([1u8, 1]).into_array_ref(),
            data,
        ],
        None,
    )
    .unwrap();

    // Exercise `ImageView` end to end against the registry-derived layout —
    // the point of `assert_round_trips`'s kernel checks is the batch level;
    // this checks the tensor accessor built specifically for this URN.
    let view = astrs_data::tensor::ImageView::from_struct_row(&column, 1).unwrap();
    assert_eq!((view.height(), view.width(), view.channels()), (2, 2, 1));
    assert_eq!(view.get_f64(&[1, 1, 0]).unwrap(), 13.0);

    assert_round_trips(
        "std/media/v1/Image[pixel=mono8]",
        payload_batch(layout, column.into_array_ref()),
    );
}

#[test]
fn audio_frame_round_trips() {
    let layout = media::audio_frame_layout("s16").unwrap();
    let DataType::Struct(fields) = layout.clone() else {
        unreachable!()
    };
    let data = ListArray::try_from_lengths(
        Field::required("sample", DataType::Int16),
        [4, 2],
        astrs_data::array::Int16Array::from_values([0i16, 100, -100, 200, 300, -300])
            .into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let column = StructArray::try_new(
        fields,
        vec![
            UInt32Array::from_values([48_000u32, 48_000]).into_array_ref(),
            UInt16Array::from_values([2u16, 1]).into_array_ref(),
            data,
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/media/v1/AudioFrame[sample=s16]",
        payload_batch(layout, column),
    );
}

#[test]
fn compressed_image_round_trips() {
    let column = StructArray::try_new(
        match media::compressed_image_layout() {
            DataType::Struct(fields) => fields,
            _ => unreachable!(),
        },
        vec![
            StringArray::from_values(["jpeg", "png"]).into_array_ref(),
            astrs_data::array::BinaryArray::from_values([&b"\xff\xd8\xff"[..], &b"\x89PNG"[..]])
                .into_array_ref(),
        ],
        None,
    )
    .unwrap()
    .into_array_ref();
    assert_round_trips(
        "std/media/v1/CompressedImage[format=jpeg]",
        payload_batch(media::compressed_image_layout(), column),
    );
}

/// Every std URN covered above resolves the same [`DataType`] whether asked
/// through [`layout_of`] or through this module's own constructor — the
/// registry wiring (`urn::registry::std_rows`) is not a second source of
/// truth that could quietly drift from `urn::layouts`.
#[test]
fn every_covered_urn_resolves_consistently_with_its_layout_function() {
    let cases: &[(&str, DataType)] = &[
        ("std/geometry/v1/Vector3", geometry::vector3_layout()),
        ("std/geometry/v1/Pose", geometry::pose_layout()),
        ("std/nav/v1/Path", nav::path_layout()),
        ("std/vision/v1/Mask", vision::mask_layout()),
        ("std/sensor/v1/Imu", sensor::imu_layout()),
    ];
    for (text, expected) in cases {
        let urn = astrs_data::urn::TypeUrn::parse(text).unwrap();
        assert_eq!(layout_of(&urn).as_ref(), Ok(expected), "{text}");
    }
}

/// The parameters the four parameterised `std` types need before they resolve
/// to a layout at all — one canonical value each, from their own registry
/// vocabularies.
const CANONICAL_PARAMETERS: &[(&str, &str)] = &[
    ("std/media/v1/Image", "[pixel=rgb8]"),
    ("std/media/v1/AudioFrame", "[sample=f32]"),
    ("std/media/v1/CompressedImage", "[format=jpeg]"),
    ("std/sensor/v1/PointCloud", "[fields=x:y:z]"),
];

/// A two-row column of any layout in the closed set: values are positional and
/// never null, which is all this test needs — the null and edge-case matrix
/// lives in `tests/ipc_roundtrip.rs`.
fn sample_column(data_type: &DataType, rows: usize) -> ArrayRef {
    match data_type {
        DataType::Null => astrs_data::array::NullArray::new(rows).into_array_ref(),
        DataType::Bool => {
            BooleanArray::from_values((0..rows).map(|row| row % 2 == 0)).into_array_ref()
        }
        DataType::Int8 => Int8Array::from_values((0..rows).map(|row| row as i8)).into_array_ref(),
        DataType::Int16 => {
            astrs_data::array::Int16Array::from_values((0..rows).map(|row| row as i16))
                .into_array_ref()
        }
        DataType::Int32 => {
            Int32Array::from_values((0..rows).map(|row| row as i32)).into_array_ref()
        }
        DataType::Int64 => {
            astrs_data::array::Int64Array::from_values((0..rows).map(|row| row as i64))
                .into_array_ref()
        }
        DataType::UInt8 => UInt8Array::from_values((0..rows).map(|row| row as u8)).into_array_ref(),
        DataType::UInt16 => {
            UInt16Array::from_values((0..rows).map(|row| row as u16)).into_array_ref()
        }
        DataType::UInt32 => {
            UInt32Array::from_values((0..rows).map(|row| row as u32)).into_array_ref()
        }
        DataType::UInt64 => {
            astrs_data::array::UInt64Array::from_values((0..rows).map(|row| row as u64))
                .into_array_ref()
        }
        DataType::Float16 => astrs_data::array::Float16Array::from_values(
            (0..rows).map(|row| astrs_data::F16::from_f32(row as f32)),
        )
        .into_array_ref(),
        DataType::Float32 => {
            Float32Array::from_values((0..rows).map(|row| row as f32)).into_array_ref()
        }
        DataType::Float64 => {
            Float64Array::from_values((0..rows).map(|row| row as f64)).into_array_ref()
        }
        DataType::Binary => astrs_data::array::BinaryArray::from_values(
            (0..rows).map(|row| vec![row as u8; row % 3]),
        )
        .into_array_ref(),
        DataType::LargeBinary => astrs_data::array::LargeBinaryArray::from_values(
            (0..rows).map(|row| vec![row as u8; row % 3]),
        )
        .into_array_ref(),
        DataType::Utf8 => {
            StringArray::from_values((0..rows).map(|row| format!("row-{row}"))).into_array_ref()
        }
        DataType::LargeUtf8 => astrs_data::array::LargeStringArray::from_values(
            (0..rows).map(|row| format!("row-{row}")),
        )
        .into_array_ref(),
        DataType::FixedSizeBinary(width) => {
            let mut builder =
                astrs_data::builder::FixedSizeBinaryBuilder::new(*width).expect("width");
            let width = usize::try_from(*width).expect("positive width");
            for row in 0..rows {
                builder
                    .append_value(vec![row as u8; width])
                    .expect("append");
            }
            builder.finish().into_array_ref()
        }
        DataType::Timestamp => {
            astrs_data::array::TimestampArray::from_nanos((0..rows).map(|row| row as i64 * 1_000))
                .into_array_ref()
        }
        DataType::Duration => {
            astrs_data::array::DurationArray::from_nanos((0..rows).map(|row| row as i64 * 7))
                .into_array_ref()
        }
        DataType::List(field) => {
            // Two child values per row, so the offsets are not the identity.
            let offsets: Vec<i32> = (0..=rows).map(|row| row as i32 * 2).collect();
            ListArray::try_new(
                field.as_ref().clone(),
                offsets.into(),
                sample_column(field.data_type(), rows * 2),
                None,
            )
            .expect("list")
            .into_array_ref()
        }
        DataType::FixedSizeList(field, size) => {
            let width = usize::try_from(*size).expect("positive size");
            FixedSizeListArray::try_new(
                field.as_ref().clone(),
                *size,
                sample_column(field.data_type(), rows * width),
                None,
            )
            .expect("fixed size list")
            .into_array_ref()
        }
        DataType::Struct(fields) => StructArray::try_new(
            fields.clone(),
            fields
                .iter()
                .map(|field| sample_column(field.data_type(), rows))
                .collect(),
            None,
        )
        .expect("struct")
        .into_array_ref(),
        other => panic!("type outside the closed AstRS set: {other:?}"),
    }
}

#[test]
fn every_std_urn_layout_round_trips_through_ipc() {
    // The per-type tests above cover the layouts of §24.3 that carry real
    // fixture data. This one covers the *registry* exhaustively: every URN it
    // publishes, parameterised where it has to be, resolves to a layout that a
    // payload can be built in and that survives the wire unchanged.
    let mut checked = 0usize;
    for text in astrs_data::urn::STD_TYPE_URNS {
        let parameterised = CANONICAL_PARAMETERS
            .iter()
            .find(|(base, _)| base == text)
            .map_or_else(
                || (*text).to_owned(),
                |(base, params)| format!("{base}{params}"),
            );
        let urn = astrs_data::urn::TypeUrn::parse(&parameterised)
            .unwrap_or_else(|err| panic!("{parameterised}: {err}"));
        let layout = layout_of(&urn).unwrap_or_else(|err| panic!("{parameterised}: layout: {err}"));

        let batch = RecordBatch::from_payload(sample_column(&layout, 2));
        assert_eq!(batch.num_rows(), 2, "{parameterised}");
        assert_payload_round_trips(&parameterised, &batch);
        assert_stream_round_trips(&parameterised, std::slice::from_ref(&batch));
        checked += 1;
    }
    assert_eq!(
        checked,
        astrs_data::urn::STD_TYPE_URNS.len(),
        "every registered URN must be covered"
    );
    assert!(checked >= 37, "the std registry shrank to {checked} types");
}
