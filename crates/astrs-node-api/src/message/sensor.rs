//! `std/sensor/v1` — raw sensor frames (§24.3).
//!
//! | Type | Layout |
//! |---|---|
//! | [`LaserScan`] | seven `Float32` scalars plus `ranges`/`intensities` as `List<Float32>` |
//! | [`Imu`] | orientation, angular velocity and linear acceleration, each with a `Float64[9]` covariance |
//! | [`NavSatFix`] | a GNSS fix with a `Float64[9]` position covariance |
//! | [`Range`] | a single-beam distance reading |
//! | [`PointCloud`] | `Struct{<name>: List<Float32>, …}`, structure-of-arrays over `fields=` |
//!
//! [`PointCloud`]'s layout depends on its URN parameter, so it carries the
//! field names as data and exposes [`PointCloud::urn`] rather than a `const
//! URN`; the other four implement [`AstrsMessage`] directly.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{AstrsMessage, Imu, PointCloud, Vector3};
//!
//! let imu = Imu::from_readings(
//!     Default::default(),
//!     Vector3::new(0.0, 0.0, 0.1),
//!     Vector3::new(0.0, 0.0, -9.81),
//! );
//! assert_eq!(Imu::from_record_batch(&imu.to_record_batch()?)?, imu);
//!
//! let cloud = PointCloud::from_columns(vec![
//!     ("x".to_owned(), vec![1.0, 2.0]),
//!     ("y".to_owned(), vec![0.0, 0.5]),
//! ])?;
//! assert_eq!(cloud.urn(), "std/sensor/v1/PointCloud[fields=x:y]");
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::{AstrsMessage, DataError, DataType, Field, RecordBatch, Result};

use super::geometry::{Quaternion, Vector3};
use super::{build, read, single_row_column};

/// The width of a 3x3 covariance matrix, flattened row-major.
pub const COVARIANCE_3X3: usize = 9;

/// A planar range sweep — `std/sensor/v1/LaserScan`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaserScan {
    /// The angle of the first beam, in radians.
    pub angle_min: f32,
    /// The angle of the last beam, in radians.
    pub angle_max: f32,
    /// The angular step between beams, in radians.
    pub angle_increment: f32,
    /// The time between beams, in seconds.
    pub time_increment: f32,
    /// The time the whole sweep took, in seconds.
    pub scan_time: f32,
    /// The smallest range the device can report, in metres.
    pub range_min: f32,
    /// The largest range the device can report, in metres.
    pub range_max: f32,
    /// One range per beam, in metres.
    pub ranges: Vec<f32>,
    /// One intensity per beam; empty when the device reports none.
    pub intensities: Vec<f32>,
}

impl LaserScan {
    /// The number of beams the sweep reports.
    #[must_use]
    pub fn beam_count(&self) -> usize {
        self.ranges.len()
    }

    /// The angle of beam `index`, in radians.
    #[must_use]
    pub fn angle_of(&self, index: usize) -> Option<f32> {
        if index >= self.ranges.len() {
            return None;
        }
        // `index` is bounded by the beam count, which a scan of any plausible
        // size keeps well inside `f32`'s exact-integer range.
        let step = u32::try_from(index).unwrap_or(u32::MAX);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a beam index is far below f32's 2^24 exact-integer ceiling"
        )]
        Some(self.angle_min + self.angle_increment * (step as f32))
    }

    /// Whether the sweep carries an intensity per beam.
    #[must_use]
    pub fn has_intensities(&self) -> bool {
        !self.intensities.is_empty() && self.intensities.len() == self.ranges.len()
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
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
}

impl AstrsMessage for LaserScan {
    const URN: &'static str = "std/sensor/v1/LaserScan";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let column = build::structure(vec![
            ("angle_min", build::primitive::<f32>(&[self.angle_min])),
            ("angle_max", build::primitive::<f32>(&[self.angle_max])),
            (
                "angle_increment",
                build::primitive::<f32>(&[self.angle_increment]),
            ),
            (
                "time_increment",
                build::primitive::<f32>(&[self.time_increment]),
            ),
            ("scan_time", build::primitive::<f32>(&[self.scan_time])),
            ("range_min", build::primitive::<f32>(&[self.range_min])),
            ("range_max", build::primitive::<f32>(&[self.range_max])),
            (
                "ranges",
                build::primitive_lists::<f32>("range", &[&self.ranges])?,
            ),
            (
                "intensities",
                build::primitive_lists::<f32>("intensity", &[&self.intensities])?,
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        Ok(Self {
            angle_min: read::f32_at(read::child(column, "angle_min")?, 0)?,
            angle_max: read::f32_at(read::child(column, "angle_max")?, 0)?,
            angle_increment: read::f32_at(read::child(column, "angle_increment")?, 0)?,
            time_increment: read::f32_at(read::child(column, "time_increment")?, 0)?,
            scan_time: read::f32_at(read::child(column, "scan_time")?, 0)?,
            range_min: read::f32_at(read::child(column, "range_min")?, 0)?,
            range_max: read::f32_at(read::child(column, "range_max")?, 0)?,
            ranges: read::primitive_list_at::<f32>(read::child(column, "ranges")?, 0)?,
            intensities: read::primitive_list_at::<f32>(read::child(column, "intensities")?, 0)?,
        })
    }
}

super::impl_from_payload!(LaserScan);

/// An inertial reading — `std/sensor/v1/Imu`.
#[derive(Debug, Clone, PartialEq)]
pub struct Imu {
    /// The estimated orientation.
    pub orientation: Quaternion,
    /// Its 3x3 covariance, row-major.
    pub orientation_covariance: [f64; COVARIANCE_3X3],
    /// The angular velocity, in radians per second.
    pub angular_velocity: Vector3,
    /// Its 3x3 covariance, row-major.
    pub angular_velocity_covariance: [f64; COVARIANCE_3X3],
    /// The linear acceleration, in metres per second squared.
    pub linear_acceleration: Vector3,
    /// Its 3x3 covariance, row-major.
    pub linear_acceleration_covariance: [f64; COVARIANCE_3X3],
}

impl Imu {
    /// A reading with unknown (all-zero) covariances, which is what
    /// `sensor_msgs/Imu` means by an unreported one.
    #[must_use]
    pub const fn from_readings(
        orientation: Quaternion,
        angular_velocity: Vector3,
        linear_acceleration: Vector3,
    ) -> Self {
        Self {
            orientation,
            orientation_covariance: [0.0; COVARIANCE_3X3],
            angular_velocity,
            angular_velocity_covariance: [0.0; COVARIANCE_3X3],
            linear_acceleration,
            linear_acceleration_covariance: [0.0; COVARIANCE_3X3],
        }
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("orientation", Quaternion::layout()),
            Field::required("orientation_covariance", covariance_layout(3)),
            Field::required("angular_velocity", Vector3::layout()),
            Field::required("angular_velocity_covariance", covariance_layout(3)),
            Field::required("linear_acceleration", Vector3::layout()),
            Field::required("linear_acceleration_covariance", covariance_layout(3)),
        ])
    }
}

impl Default for Imu {
    fn default() -> Self {
        Self::from_readings(Quaternion::IDENTITY, Vector3::ZERO, Vector3::ZERO)
    }
}

impl AstrsMessage for Imu {
    const URN: &'static str = "std/sensor/v1/Imu";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let column = build::structure(vec![
            (
                "orientation",
                Quaternion::column(core::slice::from_ref(&self.orientation))?,
            ),
            (
                "orientation_covariance",
                build::fixed_tuples::<f64>("v", 9, &self.orientation_covariance)?,
            ),
            (
                "angular_velocity",
                Vector3::column(core::slice::from_ref(&self.angular_velocity))?,
            ),
            (
                "angular_velocity_covariance",
                build::fixed_tuples::<f64>("v", 9, &self.angular_velocity_covariance)?,
            ),
            (
                "linear_acceleration",
                Vector3::column(core::slice::from_ref(&self.linear_acceleration))?,
            ),
            (
                "linear_acceleration_covariance",
                build::fixed_tuples::<f64>("v", 9, &self.linear_acceleration_covariance)?,
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        Ok(Self {
            orientation: Quaternion::at(read::child(column, "orientation")?, 0)?,
            orientation_covariance: covariance_at(
                read::child(column, "orientation_covariance")?,
                "orientation_covariance",
            )?,
            angular_velocity: Vector3::at(read::child(column, "angular_velocity")?, 0)?,
            angular_velocity_covariance: covariance_at(
                read::child(column, "angular_velocity_covariance")?,
                "angular_velocity_covariance",
            )?,
            linear_acceleration: Vector3::at(read::child(column, "linear_acceleration")?, 0)?,
            linear_acceleration_covariance: covariance_at(
                read::child(column, "linear_acceleration_covariance")?,
                "linear_acceleration_covariance",
            )?,
        })
    }
}

super::impl_from_payload!(Imu);

/// A GNSS fix — `std/sensor/v1/NavSatFix`.
#[derive(Debug, Clone, PartialEq)]
pub struct NavSatFix {
    /// The fix status, matching `sensor_msgs/NavSatStatus::status`.
    pub status: i8,
    /// The constellations used, matching `sensor_msgs/NavSatStatus::service`.
    pub service: u16,
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
    /// Altitude in metres above the WGS 84 ellipsoid.
    pub altitude: f64,
    /// The 3x3 position covariance, row-major.
    pub position_covariance: [f64; COVARIANCE_3X3],
    /// How the covariance was obtained, matching `sensor_msgs/NavSatFix`.
    pub position_covariance_type: u8,
}

impl NavSatFix {
    /// A fix at a coordinate, with no covariance reported.
    #[must_use]
    pub const fn at_coordinate(latitude: f64, longitude: f64, altitude: f64) -> Self {
        Self {
            status: 0,
            service: 1,
            latitude,
            longitude,
            altitude,
            position_covariance: [0.0; COVARIANCE_3X3],
            position_covariance_type: 0,
        }
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("status", DataType::Int8),
            Field::required("service", DataType::UInt16),
            Field::required("latitude", DataType::Float64),
            Field::required("longitude", DataType::Float64),
            Field::required("altitude", DataType::Float64),
            Field::required("position_covariance", covariance_layout(3)),
            Field::required("position_covariance_type", DataType::UInt8),
        ])
    }
}

impl Default for NavSatFix {
    fn default() -> Self {
        Self::at_coordinate(0.0, 0.0, 0.0)
    }
}

impl AstrsMessage for NavSatFix {
    const URN: &'static str = "std/sensor/v1/NavSatFix";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let column = build::structure(vec![
            ("status", build::primitive::<i8>(&[self.status])),
            ("service", build::primitive::<u16>(&[self.service])),
            ("latitude", build::primitive::<f64>(&[self.latitude])),
            ("longitude", build::primitive::<f64>(&[self.longitude])),
            ("altitude", build::primitive::<f64>(&[self.altitude])),
            (
                "position_covariance",
                build::fixed_tuples::<f64>("v", 9, &self.position_covariance)?,
            ),
            (
                "position_covariance_type",
                build::primitive::<u8>(&[self.position_covariance_type]),
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        Ok(Self {
            status: read::primitive_at::<i8>(read::child(column, "status")?, 0)?,
            service: read::primitive_at::<u16>(read::child(column, "service")?, 0)?,
            latitude: read::f64_at(read::child(column, "latitude")?, 0)?,
            longitude: read::f64_at(read::child(column, "longitude")?, 0)?,
            altitude: read::f64_at(read::child(column, "altitude")?, 0)?,
            position_covariance: covariance_at(
                read::child(column, "position_covariance")?,
                "position_covariance",
            )?,
            position_covariance_type: read::primitive_at::<u8>(
                read::child(column, "position_covariance_type")?,
                0,
            )?,
        })
    }
}

super::impl_from_payload!(NavSatFix);

/// A single-beam distance reading — `std/sensor/v1/Range`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Range {
    /// The radiation type, matching `sensor_msgs/Range` (`0` ultrasound,
    /// `1` infrared).
    pub radiation_type: u8,
    /// The beam's field of view, in radians.
    pub field_of_view: f32,
    /// The smallest range the device can report, in metres.
    pub min_range: f32,
    /// The largest range the device can report, in metres.
    pub max_range: f32,
    /// The reading, in metres.
    pub range: f32,
}

impl Range {
    /// Whether the reading falls inside the device's reportable window.
    #[must_use]
    pub fn is_in_range(&self) -> bool {
        self.range >= self.min_range && self.range <= self.max_range
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("radiation_type", DataType::UInt8),
            Field::required("field_of_view", DataType::Float32),
            Field::required("min_range", DataType::Float32),
            Field::required("max_range", DataType::Float32),
            Field::required("range", DataType::Float32),
        ])
    }
}

impl AstrsMessage for Range {
    const URN: &'static str = "std/sensor/v1/Range";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let column = build::structure(vec![
            (
                "radiation_type",
                build::primitive::<u8>(&[self.radiation_type]),
            ),
            (
                "field_of_view",
                build::primitive::<f32>(&[self.field_of_view]),
            ),
            ("min_range", build::primitive::<f32>(&[self.min_range])),
            ("max_range", build::primitive::<f32>(&[self.max_range])),
            ("range", build::primitive::<f32>(&[self.range])),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        Ok(Self {
            radiation_type: read::primitive_at::<u8>(read::child(column, "radiation_type")?, 0)?,
            field_of_view: read::f32_at(read::child(column, "field_of_view")?, 0)?,
            min_range: read::f32_at(read::child(column, "min_range")?, 0)?,
            max_range: read::f32_at(read::child(column, "max_range")?, 0)?,
            range: read::f32_at(read::child(column, "range")?, 0)?,
        })
    }
}

super::impl_from_payload!(Range);

/// A point set with named per-point fields —
/// `std/sensor/v1/PointCloud[fields=…]`, structure-of-arrays.
///
/// Deliberately not `sensor_msgs/PointCloud2`'s packed byte blob: each field
/// keeps its own typed, SIMD-ready buffer, so nothing has to unpack a
/// row-major blob to read `z`.
///
/// The layout depends on the URN's `fields` parameter, which a `const URN`
/// cannot express — this type therefore carries the names and renders its own
/// [`PointCloud::urn`] rather than implementing [`AstrsMessage`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointCloud {
    /// One `(name, values)` pair per field, in declaration order.
    columns: Vec<(String, Vec<f32>)>,
}

impl PointCloud {
    /// A cloud from its named columns.
    ///
    /// # Errors
    ///
    /// [`DataError::DuplicateFieldName`] when a name repeats, and
    /// [`DataError::ColumnLengthMismatch`] when the columns disagree on how
    /// many points they describe.
    pub fn from_columns(columns: Vec<(String, Vec<f32>)>) -> Result<Self> {
        let value = Self { columns };
        value.check()?;
        Ok(value)
    }

    /// An `x:y:z` cloud from three same-length coordinate columns.
    ///
    /// # Errors
    ///
    /// As [`PointCloud::from_columns`].
    pub fn xyz(x: Vec<f32>, y: Vec<f32>, z: Vec<f32>) -> Result<Self> {
        Self::from_columns(vec![
            ("x".to_owned(), x),
            ("y".to_owned(), y),
            ("z".to_owned(), z),
        ])
    }

    /// The field names, in declaration order.
    #[must_use]
    pub fn field_names(&self) -> Vec<&str> {
        self.columns.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// One field's values.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&[f32]> {
        self.columns
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, values)| values.as_slice())
    }

    /// How many points the cloud holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.columns.first().map_or(0, |(_, values)| values.len())
    }

    /// Whether the cloud holds no points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The parameterised URN a port carrying this cloud declares.
    #[must_use]
    pub fn urn(&self) -> String {
        format!(
            "std/sensor/v1/PointCloud[fields={}]",
            self.field_names().join(":")
        )
    }

    /// The columnar layout this cloud's fields produce.
    #[must_use]
    pub fn layout(&self) -> DataType {
        DataType::strukt(self.columns.iter().map(|(name, _)| {
            Field::required(
                name,
                DataType::list(Field::required("v", DataType::Float32)),
            )
        }))
    }

    /// Checks the invariants a well-formed cloud holds.
    ///
    /// # Errors
    ///
    /// As [`PointCloud::from_columns`].
    pub fn check(&self) -> Result<()> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.columns.len());
        let expected = self.len();
        for (index, (name, values)) in self.columns.iter().enumerate() {
            if seen.contains(&name.as_str()) {
                return Err(DataError::DuplicateFieldName { name: name.clone() });
            }
            seen.push(name);
            if values.len() != expected {
                return Err(DataError::ColumnLengthMismatch {
                    index,
                    name: name.clone(),
                    expected,
                    actual: values.len(),
                });
            }
        }
        Ok(())
    }

    /// Encodes the cloud as a one-row payload batch.
    ///
    /// The inherent twin of [`AstrsMessage::to_record_batch`]; see the type's
    /// documentation for why the trait itself does not apply.
    ///
    /// # Errors
    ///
    /// [`DataError`] when the cloud is malformed or the columns cannot be
    /// assembled.
    pub fn to_record_batch(&self) -> Result<RecordBatch> {
        self.check()?;
        let mut children: Vec<(&str, astrs_data::ArrayRef)> =
            Vec::with_capacity(self.columns.len());
        for (name, values) in &self.columns {
            children.push((
                name.as_str(),
                build::primitive_lists::<f32>("v", &[values])?,
            ));
        }
        Ok(RecordBatch::from_payload(build::structure(children)?))
    }

    /// Decodes a cloud from a one-row payload batch.
    ///
    /// # Errors
    ///
    /// [`DataError`] when the batch is not a `PointCloud` layout.
    pub fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let strukt = read::structure(column)?;
        let mut columns = Vec::with_capacity(strukt.fields().len());
        for field in strukt.fields() {
            let name = field.name().to_owned();
            let child = read::field(strukt, &name)?;
            columns.push((name, read::primitive_list_at::<f32>(child, 0)?));
        }
        Self::from_columns(columns)
    }
}

impl super::FromPayload for PointCloud {
    fn from_batch(batch: &RecordBatch) -> Result<Self> {
        Self::from_record_batch(batch)
    }
}

/// The layout of an `n * n` row-major covariance matrix.
#[must_use]
fn covariance_layout(n: i32) -> DataType {
    DataType::fixed_size_list(Field::required("v", DataType::Float64), n * n)
}

/// Reads a 3x3 covariance out of a `FixedSizeList<Float64, 9>` column.
fn covariance_at(column: &astrs_data::ArrayRef, field: &str) -> Result<[f64; COVARIANCE_3X3]> {
    let values = read::fixed_tuple_at::<f64>(column, 0, COVARIANCE_3X3)?;
    <[f64; COVARIANCE_3X3]>::try_from(values.as_slice()).map_err(|_| {
        DataError::MessageFixedArrayLength {
            field: field.to_owned(),
            expected: COVARIANCE_3X3,
            actual: values.len(),
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::{assert_layout_matches, assert_registry_layout};

    #[test]
    fn every_fixed_sensor_type_conforms_to_the_registry() {
        assert_registry_layout::<LaserScan>().unwrap();
        assert_registry_layout::<Imu>().unwrap();
        assert_registry_layout::<NavSatFix>().unwrap();
        assert_registry_layout::<Range>().unwrap();
    }

    #[test]
    fn laser_scans_round_trip() {
        let scan = LaserScan {
            angle_min: -1.5,
            angle_max: 1.5,
            angle_increment: 0.5,
            time_increment: 0.001,
            scan_time: 0.1,
            range_min: 0.05,
            range_max: 30.0,
            ranges: vec![1.0, 2.0, 3.0],
            intensities: vec![10.0, 20.0, 30.0],
        };
        let batch = scan.to_record_batch().unwrap();
        assert_eq!(LaserScan::from_record_batch(&batch).unwrap(), scan);
        assert_eq!(scan.beam_count(), 3);
        assert!(scan.has_intensities());
        assert_eq!(scan.angle_of(0), Some(-1.5));
        assert_eq!(scan.angle_of(2), Some(-0.5));
        assert_eq!(scan.angle_of(3), None);

        let bare = LaserScan {
            intensities: Vec::new(),
            ..scan
        };
        assert!(!bare.has_intensities());
        assert_eq!(
            LaserScan::from_record_batch(&bare.to_record_batch().unwrap()).unwrap(),
            bare
        );
        assert_eq!(LaserScan::default().beam_count(), 0);
    }

    #[test]
    fn imu_readings_round_trip_with_their_covariances() {
        let mut imu = Imu::from_readings(
            Quaternion::identity(),
            Vector3::new(0.0, 0.0, 0.1),
            Vector3::new(0.0, 0.0, -9.81),
        );
        imu.orientation_covariance = [1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 3.0];
        let batch = imu.to_record_batch().unwrap();
        let decoded = Imu::from_record_batch(&batch).unwrap();
        assert_eq!(decoded, imu);
        assert_eq!(decoded.orientation_covariance[4], 2.0);
        assert_eq!(Imu::default().linear_acceleration, Vector3::ZERO);
    }

    #[test]
    fn gnss_fixes_round_trip() {
        let fix = NavSatFix::at_coordinate(35.681_2, 139.767_1, 40.0);
        let batch = fix.to_record_batch().unwrap();
        assert_eq!(NavSatFix::from_record_batch(&batch).unwrap(), fix);
        assert_eq!(NavSatFix::default().latitude, 0.0);
    }

    #[test]
    fn ranges_round_trip_and_report_their_window() {
        let reading = Range {
            radiation_type: 1,
            field_of_view: 0.26,
            min_range: 0.1,
            max_range: 4.0,
            range: 1.2,
        };
        let batch = reading.to_record_batch().unwrap();
        assert_eq!(Range::from_record_batch(&batch).unwrap(), reading);
        assert!(reading.is_in_range());
        assert!(
            !Range {
                range: 9.0,
                ..reading
            }
            .is_in_range()
        );
    }

    #[test]
    fn point_clouds_round_trip_and_render_their_urn() {
        let cloud = PointCloud::xyz(vec![1.0, 2.0], vec![0.0, 0.5], vec![-1.0, -2.0]).unwrap();
        assert_eq!(cloud.urn(), "std/sensor/v1/PointCloud[fields=x:y:z]");
        assert_layout_matches(&cloud.urn(), &cloud.layout()).unwrap();

        let batch = cloud.to_record_batch().unwrap();
        let decoded = PointCloud::from_record_batch(&batch).unwrap();
        assert_eq!(decoded, cloud);
        assert_eq!(decoded.len(), 2);
        assert!(!decoded.is_empty());
        assert_eq!(decoded.field_names(), vec!["x", "y", "z"]);
        assert_eq!(decoded.column("y"), Some(&[0.0, 0.5][..]));
        assert_eq!(decoded.column("intensity"), None);
    }

    #[test]
    fn a_malformed_point_cloud_is_refused() {
        let error = PointCloud::from_columns(vec![
            ("x".to_owned(), vec![1.0, 2.0]),
            ("y".to_owned(), vec![1.0]),
        ])
        .unwrap_err();
        assert!(matches!(error, DataError::ColumnLengthMismatch { .. }));

        let error = PointCloud::from_columns(vec![
            ("x".to_owned(), vec![1.0]),
            ("x".to_owned(), vec![2.0]),
        ])
        .unwrap_err();
        assert!(matches!(error, DataError::DuplicateFieldName { .. }));

        assert!(PointCloud::default().is_empty());
        assert_eq!(PointCloud::default().len(), 0);
    }

    #[test]
    fn a_wrong_layout_is_refused() {
        let range = Range::default().to_record_batch().unwrap();
        assert!(Imu::from_record_batch(&range).is_err());
        assert!(LaserScan::from_record_batch(&range).is_err());
        assert!(NavSatFix::from_record_batch(&range).is_err());

        let imu = Imu::default().to_record_batch().unwrap();
        assert!(Range::from_record_batch(&imu).is_err());
        assert!(PointCloud::from_record_batch(&imu).is_err());
    }
}
