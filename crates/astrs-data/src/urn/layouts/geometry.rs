//! `std/geometry/v1` layouts — rigid-body quantities as `Struct` compositions
//! of `Float64` scalars.
//!
//! Every type here mirrors the corresponding `geometry_msgs` shape (field
//! names included), so the eventual ROS 2 bridge (`astrs-ros2`, blueprint
//! §10.4) maps columns mechanically instead of renaming them. None of the
//! six types' layouts depend on their accepted parameters — `frame` and
//! `child_frame` are graph-level bookkeeping (which coordinate frame a value
//! is expressed in), not part of the byte shape — so every constructor here
//! is a plain `fn() -> DataType`, wired into the registry as
//! [`LayoutRule::Fixed`](crate::urn::LayoutRule::Fixed).
//!
//! ```
//! use astrs_data::urn::layouts::geometry::{quaternion_layout, vector3_layout};
//!
//! let v = vector3_layout();
//! assert_eq!(v.children().len(), 3);
//! assert_eq!(quaternion_layout().children().len(), 4);
//! ```

use crate::datatype::{DataType, Field, Schema};

/// `std/geometry/v1/Vector3` — `{x, y, z}`, all `Float64`, none nullable.
///
/// ```
/// use astrs_data::urn::layouts::geometry::vector3_layout;
///
/// let layout = vector3_layout();
/// assert_eq!(layout.to_string(), "Struct{x: Float64, y: Float64, z: Float64}");
/// ```
#[must_use]
pub fn vector3_layout() -> DataType {
    DataType::strukt([
        Field::required("x", DataType::Float64),
        Field::required("y", DataType::Float64),
        Field::required("z", DataType::Float64),
    ])
}

/// The single-column [`Schema`] for a `std/geometry/v1/Vector3` payload.
#[must_use]
pub fn vector3_schema() -> Schema {
    Schema::payload(vector3_layout(), false)
}

/// `std/geometry/v1/Quaternion` — `{x, y, z, w}`, all `Float64`.
///
/// Stored `xyzw` (scalar last), the ROS 2 and Eigen convention.
///
/// ```
/// use astrs_data::urn::layouts::geometry::quaternion_layout;
/// use astrs_data::DataType;
///
/// let layout = quaternion_layout();
/// let DataType::Struct(fields) = layout else { unreachable!() };
/// let names: Vec<&str> = fields.iter().map(|f| f.name()).collect();
/// assert_eq!(names, ["x", "y", "z", "w"]);
/// ```
#[must_use]
pub fn quaternion_layout() -> DataType {
    DataType::strukt([
        Field::required("x", DataType::Float64),
        Field::required("y", DataType::Float64),
        Field::required("z", DataType::Float64),
        Field::required("w", DataType::Float64),
    ])
}

/// The single-column [`Schema`] for a `std/geometry/v1/Quaternion` payload.
#[must_use]
pub fn quaternion_schema() -> Schema {
    Schema::payload(quaternion_layout(), false)
}

/// `std/geometry/v1/Pose` — `{position: Vector3, orientation: Quaternion}`.
///
/// `position` uses the [`vector3_layout`] shape; ROS 2's `geometry_msgs/Point`
/// is byte-identical to `Vector3` (both are three `float64`s), so AstRS does
/// not carry a separate `Point` type — one shape, two names, per context.
#[must_use]
pub fn pose_layout() -> DataType {
    DataType::strukt([
        Field::required("position", vector3_layout()),
        Field::required("orientation", quaternion_layout()),
    ])
}

/// The single-column [`Schema`] for a `std/geometry/v1/Pose` payload.
#[must_use]
pub fn pose_schema() -> Schema {
    Schema::payload(pose_layout(), false)
}

/// `std/geometry/v1/Transform` — `{translation: Vector3, rotation: Quaternion}`.
///
/// Same shape as [`pose_layout`] with different field names — `Transform`
/// answers "how do I get from `frame` to `child_frame`", `Pose` answers
/// "where is this, expressed in `frame`". `astrs-tf`'s transform tree
/// (blueprint §10.6) is built on this layout.
#[must_use]
pub fn transform_layout() -> DataType {
    DataType::strukt([
        Field::required("translation", vector3_layout()),
        Field::required("rotation", quaternion_layout()),
    ])
}

/// The single-column [`Schema`] for a `std/geometry/v1/Transform` payload.
#[must_use]
pub fn transform_schema() -> Schema {
    Schema::payload(transform_layout(), false)
}

/// `std/geometry/v1/Twist` — `{linear: Vector3, angular: Vector3}`, a
/// velocity.
#[must_use]
pub fn twist_layout() -> DataType {
    DataType::strukt([
        Field::required("linear", vector3_layout()),
        Field::required("angular", vector3_layout()),
    ])
}

/// The single-column [`Schema`] for a `std/geometry/v1/Twist` payload.
#[must_use]
pub fn twist_schema() -> Schema {
    Schema::payload(twist_layout(), false)
}

/// `std/geometry/v1/Accel` — `{linear: Vector3, angular: Vector3}`, an
/// acceleration.
///
/// **Byte-identical to [`twist_layout`]** — ROS 2's `geometry_msgs/Accel` is
/// the same two-`Vector3` shape as `Twist`, under different field
/// meanings but the same field *names* (`linear`/`angular`). The type URN is
/// what tells a velocity from an acceleration; the columns cannot.
/// [`crate::urn::urn_for_data_type`] knows this and does not attempt a
/// reverse lookup for compound layouts at all, precisely because pairs like
/// this one have no unique inverse.
#[must_use]
pub fn accel_layout() -> DataType {
    DataType::strukt([
        Field::required("linear", vector3_layout()),
        Field::required("angular", vector3_layout()),
    ])
}

/// The single-column [`Schema`] for a `std/geometry/v1/Accel` payload.
#[must_use]
pub fn accel_schema() -> Schema {
    Schema::payload(accel_layout(), false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn vector3_has_three_required_float64_fields() {
        let layout = vector3_layout();
        assert_eq!(layout.children().len(), 3);
        for field in layout.children() {
            assert_eq!(field.data_type(), &DataType::Float64);
            assert!(!field.is_nullable());
        }
    }

    #[test]
    fn quaternion_orders_xyzw() {
        let DataType::Struct(fields) = quaternion_layout() else {
            panic!("expected a struct");
        };
        let names: Vec<&str> = fields.iter().map(Field::name).collect();
        assert_eq!(names, ["x", "y", "z", "w"]);
    }

    #[test]
    fn pose_nests_position_and_orientation() {
        let DataType::Struct(fields) = pose_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name(), "position");
        assert_eq!(fields[0].data_type(), &vector3_layout());
        assert_eq!(fields[1].name(), "orientation");
        assert_eq!(fields[1].data_type(), &quaternion_layout());
    }

    #[test]
    fn transform_and_pose_share_a_shape_under_different_names() {
        assert!(transform_layout().layout_eq(&pose_layout()));
        assert_ne!(transform_layout(), pose_layout());
    }

    #[test]
    fn twist_and_accel_are_byte_identical() {
        // Deliberate: see `accel_layout`'s doc for why this is not a bug.
        assert_eq!(twist_layout(), accel_layout());
    }

    #[test]
    fn schemas_wrap_the_layout_in_the_single_data_column() {
        for (schema, layout) in [
            (vector3_schema(), vector3_layout()),
            (quaternion_schema(), quaternion_layout()),
            (pose_schema(), pose_layout()),
            (transform_schema(), transform_layout()),
            (twist_schema(), twist_layout()),
            (accel_schema(), accel_layout()),
        ] {
            assert_eq!(schema.len(), 1);
            assert_eq!(schema.field(0).map(Field::data_type), Some(&layout));
            assert_eq!(schema.field(0).map(|f| f.name()), Some(crate::DATA_COLUMN));
        }
    }
}
