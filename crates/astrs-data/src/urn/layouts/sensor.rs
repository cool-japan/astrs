//! `std/sensor/v1` layouts — raw sensor frames.
//!
//! Four of the five types (`LaserScan`, `Imu`, `NavSatFix`, `Range`) mirror
//! their `sensor_msgs` counterpart field-for-field and take no shape from
//! their parameters. `PointCloud` is the one resolver in this module: its
//! `fields` parameter (`PointCloud[fields=x:y:z]`) names the per-point
//! columns, so its layout is computed rather than fixed — see
//! [`point_cloud_layout`].

use crate::datatype::{DataType, Field, Schema};
use crate::urn::error::TypeUrnError;
use crate::urn::layouts::geometry::{quaternion_layout, vector3_layout};
use crate::urn::parse::TypeUrn;
use crate::urn::registry::STD_SENSOR_POINT_CLOUD;

/// A `Float64` covariance matrix of `n * n` entries, row-major — the shape
/// `sensor_msgs` spells `float64[36]` (a 6x6 pose+twist covariance) or
/// `float64[9]` (a 3x3 orientation/acceleration covariance).
fn covariance(n: i32) -> DataType {
    DataType::fixed_size_list(Field::required("v", DataType::Float64), n * n)
}

/// `std/sensor/v1/LaserScan` — a planar range sweep.
///
/// `{angle_min, angle_max, angle_increment, time_increment, scan_time,
/// range_min, range_max: Float32, ranges: List<Float32>, intensities:
/// List<Float32>}` — the `sensor_msgs/LaserScan` fields exactly, with
/// `ranges`/`intensities` as `List` because the beam count is a device
/// property, not a URN parameter.
#[must_use]
pub fn laser_scan_layout() -> DataType {
    DataType::strukt([
        Field::required("angle_min", DataType::Float32),
        Field::required("angle_max", DataType::Float32),
        Field::required("angle_increment", DataType::Float32),
        Field::required("time_increment", DataType::Float32),
        Field::required("scan_time", DataType::Float32),
        Field::required("range_min", DataType::Float32),
        Field::required("range_max", DataType::Float32),
        Field::required(
            "ranges",
            DataType::list(Field::required("range", DataType::Float32)),
        ),
        Field::required(
            "intensities",
            DataType::list(Field::required("intensity", DataType::Float32)),
        ),
    ])
}

/// The single-column [`Schema`] for a `std/sensor/v1/LaserScan` payload.
#[must_use]
pub fn laser_scan_schema() -> Schema {
    Schema::payload(laser_scan_layout(), false)
}

/// `std/sensor/v1/Imu` — angular rate, acceleration and orientation.
///
/// `{orientation: Quaternion, orientation_covariance: Float64[9],
/// angular_velocity: Vector3, angular_velocity_covariance: Float64[9],
/// linear_acceleration: Vector3, linear_acceleration_covariance:
/// Float64[9]}`, matching `sensor_msgs/Imu` field for field.
#[must_use]
pub fn imu_layout() -> DataType {
    DataType::strukt([
        Field::required("orientation", quaternion_layout()),
        Field::required("orientation_covariance", covariance(3)),
        Field::required("angular_velocity", vector3_layout()),
        Field::required("angular_velocity_covariance", covariance(3)),
        Field::required("linear_acceleration", vector3_layout()),
        Field::required("linear_acceleration_covariance", covariance(3)),
    ])
}

/// The single-column [`Schema`] for a `std/sensor/v1/Imu` payload.
#[must_use]
pub fn imu_schema() -> Schema {
    Schema::payload(imu_layout(), false)
}

/// `std/sensor/v1/NavSatFix` — a GNSS fix.
///
/// `{status: Int8, service: UInt16, latitude, longitude, altitude: Float64,
/// position_covariance: Float64[9], position_covariance_type: UInt8}`,
/// matching `sensor_msgs/NavSatFix` (its nested `NavSatStatus` is flattened
/// into `status`/`service`, since AstRS carries no sub-message metadata
/// beyond the columnar layout itself).
#[must_use]
pub fn nav_sat_fix_layout() -> DataType {
    DataType::strukt([
        Field::required("status", DataType::Int8),
        Field::required("service", DataType::UInt16),
        Field::required("latitude", DataType::Float64),
        Field::required("longitude", DataType::Float64),
        Field::required("altitude", DataType::Float64),
        Field::required("position_covariance", covariance(3)),
        Field::required("position_covariance_type", DataType::UInt8),
    ])
}

/// The single-column [`Schema`] for a `std/sensor/v1/NavSatFix` payload.
#[must_use]
pub fn nav_sat_fix_schema() -> Schema {
    Schema::payload(nav_sat_fix_layout(), false)
}

/// `std/sensor/v1/Range` — a single-beam distance reading.
///
/// `{radiation_type: UInt8, field_of_view, min_range, max_range, range:
/// Float32}`, matching `sensor_msgs/Range`.
#[must_use]
pub fn range_layout() -> DataType {
    DataType::strukt([
        Field::required("radiation_type", DataType::UInt8),
        Field::required("field_of_view", DataType::Float32),
        Field::required("min_range", DataType::Float32),
        Field::required("max_range", DataType::Float32),
        Field::required("range", DataType::Float32),
    ])
}

/// The single-column [`Schema`] for a `std/sensor/v1/Range` payload.
#[must_use]
pub fn range_schema() -> Schema {
    Schema::payload(range_layout(), false)
}

/// Splits a `PointCloud` `fields` parameter value into its ordered, unique
/// field names.
///
/// # Errors
///
/// [`TypeUrnError::UnsupportedParameterValue`] when `fields` is empty,
/// contains an empty name, or repeats a name.
fn split_point_fields(fields: &str) -> Result<Vec<&str>, TypeUrnError> {
    let reject = |value: &str| {
        Err(TypeUrnError::UnsupportedParameterValue {
            urn: STD_SENSOR_POINT_CLOUD.to_owned(),
            key: "fields".to_owned(),
            value: value.to_owned(),
        })
    };
    if fields.is_empty() {
        return reject(fields);
    }
    let mut names = Vec::new();
    for name in fields.split(':') {
        if name.is_empty() || names.contains(&name) {
            return reject(fields);
        }
        names.push(name);
    }
    Ok(names)
}

/// `std/sensor/v1/PointCloud[fields=…]` — a point set with named per-point
/// fields, laid out **structure-of-arrays**: `Struct{<name>: List<Float32>,
/// …}`, one same-length `List<Float32>` column per colon-separated name in
/// `fields` (`fields=x:y:z` — see the registry docs), in the order given.
///
/// This is deliberately not `sensor_msgs/PointCloud2`'s packed-byte-blob
/// layout: AstRS's columnar model already gives every field its own typed,
/// SIMD-ready buffer, so there is no reason to re-pack them into row-major
/// bytes only to unpack them again downstream. [`crate::tensor`] and the
/// compute kernels operate directly on the per-field `List<Float32>`
/// columns this produces.
///
/// # Errors
///
/// [`TypeUrnError::UnsupportedParameterValue`] when `fields` is empty,
/// contains an empty name, or repeats a name.
///
/// ```
/// use astrs_data::urn::layouts::sensor::point_cloud_layout;
/// use astrs_data::DataType;
///
/// let layout = point_cloud_layout("x:y:z")?;
/// let DataType::Struct(columns) = layout else { unreachable!() };
/// let names: Vec<&str> = columns.iter().map(|f| f.name()).collect();
/// assert_eq!(names, ["x", "y", "z"]);
/// assert!(point_cloud_layout("").is_err());
/// assert!(point_cloud_layout("x:x").is_err());
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
pub fn point_cloud_layout(fields: &str) -> Result<DataType, TypeUrnError> {
    let names = split_point_fields(fields)?;
    Ok(DataType::strukt(names.into_iter().map(|name| {
        Field::required(
            name,
            DataType::list(Field::required("v", DataType::Float32)),
        )
    })))
}

/// The single-column [`Schema`] for a `std/sensor/v1/PointCloud[fields=…]`
/// payload.
///
/// # Errors
///
/// Whatever [`point_cloud_layout`] reports.
pub fn point_cloud_schema(fields: &str) -> Result<Schema, TypeUrnError> {
    Ok(Schema::payload(point_cloud_layout(fields)?, false))
}

/// [`LayoutResolver`](crate::urn::LayoutResolver) for `PointCloud`, wired
/// into the registry.
///
/// # Errors
///
/// [`TypeUrnError::MissingParameter`] if called before the registry's
/// required-parameter check (defensive; the registry never does this in
/// practice), or whatever [`point_cloud_layout`] reports.
pub(crate) fn point_cloud_resolver(urn: &TypeUrn) -> Result<DataType, TypeUrnError> {
    let fields = urn
        .param("fields")
        .ok_or_else(|| TypeUrnError::MissingParameter {
            urn: STD_SENSOR_POINT_CLOUD.to_owned(),
            key: "fields".to_owned(),
        })?;
    point_cloud_layout(fields)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn laser_scan_has_seven_scalars_and_two_lists() {
        let DataType::Struct(fields) = laser_scan_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(fields.len(), 9);
        assert!(matches!(fields[7].data_type(), DataType::List(_)));
        assert!(matches!(fields[8].data_type(), DataType::List(_)));
    }

    #[test]
    fn imu_covariances_are_nine_wide() {
        let DataType::Struct(fields) = imu_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(
            fields[1].data_type(),
            &DataType::fixed_size_list(Field::required("v", DataType::Float64), 9)
        );
    }

    #[test]
    fn nav_sat_fix_flattens_the_status_submessage() {
        let DataType::Struct(fields) = nav_sat_fix_layout() else {
            panic!("expected a struct");
        };
        assert_eq!(fields[0].name(), "status");
        assert_eq!(fields[1].name(), "service");
    }

    #[test]
    fn range_has_five_float_or_byte_scalars() {
        assert_eq!(range_layout().children().len(), 5);
    }

    #[test]
    fn point_cloud_layout_orders_and_types_the_named_fields() {
        let layout = point_cloud_layout("x:y:z:intensity").unwrap();
        let DataType::Struct(fields) = &layout else {
            panic!("expected a struct");
        };
        let names: Vec<&str> = fields.iter().map(Field::name).collect();
        assert_eq!(names, ["x", "y", "z", "intensity"]);
        for field in fields {
            assert_eq!(
                field.data_type(),
                &DataType::list(Field::required("v", DataType::Float32))
            );
        }
    }

    #[test]
    fn point_cloud_layout_rejects_malformed_fields() {
        assert!(point_cloud_layout("").is_err());
        assert!(point_cloud_layout("x::y").is_err());
        assert!(point_cloud_layout("x:x").is_err());
        assert!(matches!(
            point_cloud_layout("x:x"),
            Err(TypeUrnError::UnsupportedParameterValue { .. })
        ));
    }

    #[test]
    fn point_cloud_resolver_reads_the_urn_parameter() {
        let urn = TypeUrn::parse("std/sensor/v1/PointCloud[fields=x:y]").unwrap();
        assert_eq!(point_cloud_resolver(&urn), point_cloud_layout("x:y"));

        let bare = TypeUrn::parse("std/sensor/v1/PointCloud").unwrap();
        assert!(matches!(
            point_cloud_resolver(&bare),
            Err(TypeUrnError::MissingParameter { .. })
        ));
    }

    #[test]
    fn point_cloud_schema_wraps_the_layout() {
        let schema = point_cloud_schema("x:y:z").unwrap();
        assert_eq!(
            schema.field(0).map(|f| f.data_type()),
            Some(&point_cloud_layout("x:y:z").unwrap())
        );
        assert!(point_cloud_schema("").is_err());
    }
}
