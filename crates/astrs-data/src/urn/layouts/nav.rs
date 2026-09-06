//! `std/nav/v1` layouts — navigation products built on `std/geometry/v1`.
//!
//! None of the three types' layouts depend on their accepted parameters
//! (`frame`/`child_frame` are, as in `geometry`, graph bookkeeping rather
//! than shape), so every constructor here is a plain `fn() -> DataType`.

use crate::datatype::{DataType, Field, Schema};
use crate::urn::layouts::geometry::{pose_layout, twist_layout};

/// A `Float64` covariance matrix of `n * n` entries, row-major.
fn covariance(n: i32) -> DataType {
    DataType::fixed_size_list(Field::required("v", DataType::Float64), n * n)
}

/// `std/nav/v1/Odometry` — pose and twist with covariance.
///
/// `{pose: Pose, pose_covariance: Float64[36], twist: Twist,
/// twist_covariance: Float64[36]}` — `nav_msgs/Odometry`'s
/// `PoseWithCovariance`/`TwistWithCovariance` wrappers flattened onto the
/// pose/twist plus their `float64[36]` (6x6) covariance, since AstRS has no
/// separate "with covariance" wrapper type.
#[must_use]
pub fn odometry_layout() -> DataType {
    DataType::strukt([
        Field::required("pose", pose_layout()),
        Field::required("pose_covariance", covariance(6)),
        Field::required("twist", twist_layout()),
        Field::required("twist_covariance", covariance(6)),
    ])
}

/// The single-column [`Schema`] for a `std/nav/v1/Odometry` payload.
#[must_use]
pub fn odometry_schema() -> Schema {
    Schema::payload(odometry_layout(), false)
}

/// `std/nav/v1/Path` — an ordered pose sequence.
///
/// `List<Struct{stamp: Timestamp, pose: Pose}>` — a top-level list rather
/// than a struct wrapping a list, since a path *is* the sequence (matching
/// `nav_msgs/Path`'s `poses: PoseStamped[]`, minus the header AstRS carries
/// as message metadata instead).
#[must_use]
pub fn path_layout() -> DataType {
    DataType::list(Field::required(
        "pose",
        DataType::strukt([
            Field::required("stamp", DataType::Timestamp),
            Field::required("pose", pose_layout()),
        ]),
    ))
}

/// The single-column [`Schema`] for a `std/nav/v1/Path` payload.
#[must_use]
pub fn path_schema() -> Schema {
    Schema::payload(path_layout(), false)
}

/// `std/nav/v1/OccupancyGrid` — a 2-D occupancy costmap.
///
/// `{resolution: Float32, width: UInt32, height: UInt32, origin: Pose,
/// cells: List<Int8>}`, row-major, `cells.len() == width * height` — the
/// `nav_msgs/OccupancyGrid` `MapMetaData` fields plus its `int8[]` data,
/// flattened the same way [`crate::urn::layouts::vision::mask_layout`]
/// flattens a label plane.
#[must_use]
pub fn occupancy_grid_layout() -> DataType {
    DataType::strukt([
        Field::required("resolution", DataType::Float32),
        Field::required("width", DataType::UInt32),
        Field::required("height", DataType::UInt32),
        Field::required("origin", pose_layout()),
        Field::required(
            "cells",
            DataType::list(Field::required("cell", DataType::Int8)),
        ),
    ])
}

/// The single-column [`Schema`] for a `std/nav/v1/OccupancyGrid` payload.
#[must_use]
pub fn occupancy_grid_schema() -> Schema {
    Schema::payload(occupancy_grid_layout(), false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int8Array, IntoArrayRef, ListArray};

    #[test]
    fn odometry_pairs_pose_and_twist_with_36_wide_covariances() {
        let DataType::Struct(fields) = odometry_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0].data_type(), &pose_layout());
        assert_eq!(
            fields[1].data_type(),
            &DataType::fixed_size_list(Field::required("v", DataType::Float64), 36)
        );
        assert_eq!(fields[2].data_type(), &twist_layout());
    }

    #[test]
    fn path_is_a_list_of_stamped_poses() {
        let DataType::List(item) = path_layout() else {
            panic!("expected a list");
        };
        let DataType::Struct(fields) = item.data_type() else {
            panic!("expected the item to be a struct");
        };
        assert_eq!(fields[0].name(), "stamp");
        assert_eq!(fields[0].data_type(), &DataType::Timestamp);
        assert_eq!(fields[1].data_type(), &pose_layout());
    }

    #[test]
    fn occupancy_grid_pairs_dimensions_with_a_flat_cell_list() {
        let DataType::Struct(fields) = occupancy_grid_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(fields[1].name(), "width");
        assert_eq!(fields[2].name(), "height");
        assert!(matches!(fields[4].data_type(), DataType::List(_)));
    }

    #[test]
    fn an_occupancy_grid_row_builds_end_to_end() {
        use crate::array::{Array, Float32Array, Float64Array, StructArray, UInt32Array};

        fn vector3_row(x: f64, y: f64, z: f64) -> StructArray {
            let DataType::Struct(fields) = crate::urn::layouts::geometry::vector3_layout() else {
                unreachable!()
            };
            StructArray::try_new(
                fields,
                vec![
                    Float64Array::from_values([x]).into_array_ref(),
                    Float64Array::from_values([y]).into_array_ref(),
                    Float64Array::from_values([z]).into_array_ref(),
                ],
                None,
            )
            .unwrap()
        }

        fn quaternion_row() -> StructArray {
            let DataType::Struct(fields) = crate::urn::layouts::geometry::quaternion_layout()
            else {
                unreachable!()
            };
            StructArray::try_new(
                fields,
                vec![
                    Float64Array::from_values([0.0]).into_array_ref(),
                    Float64Array::from_values([0.0]).into_array_ref(),
                    Float64Array::from_values([0.0]).into_array_ref(),
                    Float64Array::from_values([1.0]).into_array_ref(),
                ],
                None,
            )
            .unwrap()
        }

        let DataType::Struct(pose_fields) = pose_layout() else {
            unreachable!()
        };
        let origin = StructArray::try_new(
            pose_fields,
            vec![
                vector3_row(0.0, 0.0, 0.0).into_array_ref(),
                quaternion_row().into_array_ref(),
            ],
            None,
        )
        .unwrap();

        let cells = ListArray::try_from_lengths(
            Field::required("cell", DataType::Int8),
            [4],
            Int8Array::from_values([0i8, 100, 100, 0]).into_array_ref(),
        )
        .unwrap();

        let DataType::Struct(grid_fields) = occupancy_grid_layout() else {
            unreachable!()
        };
        let row = StructArray::try_new(
            grid_fields,
            vec![
                Float32Array::from_values([0.05]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                origin.into_array_ref(),
                cells.into_array_ref(),
            ],
            None,
        )
        .unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row.data_type(), &occupancy_grid_layout());
    }
}
