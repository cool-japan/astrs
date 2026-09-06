//! `std/nav/v1` — navigation products built on `std/geometry/v1` (§24.3).
//!
//! | Type | Layout |
//! |---|---|
//! | [`Odometry`] | `{pose: Pose, pose_covariance: Float64[36], twist: Twist, twist_covariance: Float64[36]}` |
//! | [`Path`] | `List<Struct{stamp: Timestamp, pose: Pose}>` — a *top-level list*, not a struct wrapping one |
//! | [`OccupancyGrid`] | `{resolution: Float32, width: UInt32, height: UInt32, origin: Pose, cells: List<Int8>}` |
//!
//! [`Path`] is the one std type whose payload column is not a `Struct`: a
//! path *is* its sequence, so the layout says so. That makes its
//! encode/decode read a little differently from every other type here, which
//! is a fair price for not inventing a wrapper the spec does not have.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{AstrsMessage, Path, Pose, StampedPose, Timestamp, Vector3};
//!
//! let path = Path::new(vec![StampedPose::new(
//!     Timestamp::from_nanos(1),
//!     Pose::new(Vector3::new(1.0, 0.0, 0.0), Default::default()),
//! )]);
//! assert_eq!(Path::from_record_batch(&path.to_record_batch()?)?.len(), 1);
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::{ArrayRef, AstrsMessage, DataError, DataType, Field, RecordBatch, Result};

use super::geometry::{Pose, Twist};
use super::scalar::Timestamp;
use super::{build, read, single_row_column};

/// The width of a 6x6 covariance matrix, flattened row-major.
pub const COVARIANCE_6X6: usize = 36;

/// A pose and a velocity with covariances — `std/nav/v1/Odometry`.
#[derive(Debug, Clone, PartialEq)]
pub struct Odometry {
    /// The estimated pose.
    pub pose: Pose,
    /// Its 6x6 covariance, row-major.
    pub pose_covariance: [f64; COVARIANCE_6X6],
    /// The estimated velocity.
    pub twist: Twist,
    /// Its 6x6 covariance, row-major.
    pub twist_covariance: [f64; COVARIANCE_6X6],
}

impl Odometry {
    /// An estimate with unknown (all-zero) covariances.
    #[must_use]
    pub const fn from_estimate(pose: Pose, twist: Twist) -> Self {
        Self {
            pose,
            pose_covariance: [0.0; COVARIANCE_6X6],
            twist,
            twist_covariance: [0.0; COVARIANCE_6X6],
        }
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("pose", Pose::layout()),
            Field::required("pose_covariance", covariance_layout(6)),
            Field::required("twist", Twist::layout()),
            Field::required("twist_covariance", covariance_layout(6)),
        ])
    }
}

impl Default for Odometry {
    fn default() -> Self {
        Self::from_estimate(Pose::default(), Twist::default())
    }
}

impl AstrsMessage for Odometry {
    const URN: &'static str = "std/nav/v1/Odometry";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let column = build::structure(vec![
            ("pose", Pose::column(core::slice::from_ref(&self.pose))?),
            (
                "pose_covariance",
                build::fixed_tuples::<f64>("v", 36, &self.pose_covariance)?,
            ),
            ("twist", Twist::column(core::slice::from_ref(&self.twist))?),
            (
                "twist_covariance",
                build::fixed_tuples::<f64>("v", 36, &self.twist_covariance)?,
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        Ok(Self {
            pose: Pose::at(read::child(column, "pose")?, 0)?,
            pose_covariance: covariance_at(read::child(column, "pose_covariance")?, "pose")?,
            twist: Twist::at(read::child(column, "twist")?, 0)?,
            twist_covariance: covariance_at(read::child(column, "twist_covariance")?, "twist")?,
        })
    }
}

super::impl_from_payload!(Odometry);

/// One entry of a [`Path`]: a pose and the time it holds for.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StampedPose {
    /// When the pose holds.
    pub stamp: Timestamp,
    /// The pose.
    pub pose: Pose,
}

impl StampedPose {
    /// An entry from its parts.
    #[must_use]
    pub const fn new(stamp: Timestamp, pose: Pose) -> Self {
        Self { stamp, pose }
    }

    /// The columnar layout of one entry.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("stamp", DataType::Timestamp),
            Field::required("pose", Pose::layout()),
        ])
    }

    /// Builds the child column for `values`.
    ///
    /// # Errors
    ///
    /// [`DataError`] when the columns cannot be assembled.
    pub fn column(values: &[Self]) -> Result<ArrayRef> {
        let stamps: Vec<i64> = values.iter().map(|value| value.stamp.nanos).collect();
        let poses: Vec<Pose> = values.iter().map(|value| value.pose).collect();
        build::structure(vec![
            ("stamp", build::timestamps(&stamps)),
            ("pose", Pose::column(&poses)?),
        ])
    }

    /// Reads one row of an entry column.
    ///
    /// # Errors
    ///
    /// [`DataError`] for a layout mismatch or an out-of-range row.
    pub fn at(column: &ArrayRef, row: usize) -> Result<Self> {
        let strukt = read::structure(column)?;
        Ok(Self {
            stamp: Timestamp::from_nanos(read::timestamp_at(read::field(strukt, "stamp")?, row)?),
            pose: Pose::at(read::field(strukt, "pose")?, row)?,
        })
    }
}

/// An ordered pose sequence — `std/nav/v1/Path`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Path {
    /// The poses, in order.
    pub poses: Vec<StampedPose>,
}

impl Path {
    /// A path from its poses.
    #[must_use]
    pub const fn new(poses: Vec<StampedPose>) -> Self {
        Self { poses }
    }

    /// How many poses the path holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.poses.len()
    }

    /// Whether the path is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.poses.is_empty()
    }

    /// The columnar layout of this type — a top-level `List`, not a `Struct`.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::list(Field::required("pose", StampedPose::layout()))
    }
}

impl AstrsMessage for Path {
    const URN: &'static str = "std/nav/v1/Path";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        let child = StampedPose::column(&self.poses)?;
        let column =
            build::nested_lists("pose", StampedPose::layout(), &[self.poses.len()], child)?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        let (entries, len) = read::list_window(column, 0)?;
        let poses = (0..len)
            .map(|row| StampedPose::at(&entries, row))
            .collect::<Result<Vec<StampedPose>>>()?;
        Ok(Self { poses })
    }
}

super::impl_from_payload!(Path);

/// A 2-D occupancy costmap — `std/nav/v1/OccupancyGrid`, row-major.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OccupancyGrid {
    /// The size of one cell, in metres.
    pub resolution: f32,
    /// The grid's width in cells.
    pub width: u32,
    /// The grid's height in cells.
    pub height: u32,
    /// The pose of cell `(0, 0)`.
    pub origin: Pose,
    /// `width * height` occupancy values, row-major: `-1` unknown, `0..=100`
    /// the occupancy probability as a percentage.
    pub cells: Vec<i8>,
}

impl OccupancyGrid {
    /// A grid whose cell count matches its dimensions.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`] when `cells.len() != width *
    /// height`.
    pub fn new(
        resolution: f32,
        width: u32,
        height: u32,
        origin: Pose,
        cells: Vec<i8>,
    ) -> Result<Self> {
        let value = Self {
            resolution,
            width,
            height,
            origin,
            cells,
        };
        value.check_dimensions()?;
        Ok(value)
    }

    /// The occupancy at `(x, y)`, or `None` outside the grid.
    #[must_use]
    pub fn cell_at(&self, x: u32, y: u32) -> Option<i8> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = usize::try_from(y)
            .ok()?
            .checked_mul(usize::try_from(self.width).ok()?)?
            + usize::try_from(x).ok()?;
        self.cells.get(index).copied()
    }

    /// Checks that the cell count matches the declared dimensions.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`].
    pub fn check_dimensions(&self) -> Result<()> {
        let expected =
            usize::try_from(u64::from(self.width) * u64::from(self.height)).unwrap_or(usize::MAX);
        if self.cells.len() == expected {
            Ok(())
        } else {
            Err(DataError::ChildLengthMismatch {
                expected,
                actual: self.cells.len(),
            })
        }
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("resolution", DataType::Float32),
            Field::required("width", DataType::UInt32),
            Field::required("height", DataType::UInt32),
            Field::required("origin", Pose::layout()),
            Field::required(
                "cells",
                DataType::list(Field::required("cell", DataType::Int8)),
            ),
        ])
    }
}

impl AstrsMessage for OccupancyGrid {
    const URN: &'static str = "std/nav/v1/OccupancyGrid";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        self.check_dimensions()?;
        let column = build::structure(vec![
            ("resolution", build::primitive::<f32>(&[self.resolution])),
            ("width", build::primitive::<u32>(&[self.width])),
            ("height", build::primitive::<u32>(&[self.height])),
            ("origin", Pose::column(core::slice::from_ref(&self.origin))?),
            (
                "cells",
                build::primitive_lists::<i8>("cell", &[&self.cells])?,
            ),
        ])?;
        Ok(RecordBatch::from_payload(column))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        let column = single_row_column(batch)?;
        Self::new(
            read::f32_at(read::child(column, "resolution")?, 0)?,
            read::primitive_at::<u32>(read::child(column, "width")?, 0)?,
            read::primitive_at::<u32>(read::child(column, "height")?, 0)?,
            Pose::at(read::child(column, "origin")?, 0)?,
            read::primitive_list_at::<i8>(read::child(column, "cells")?, 0)?,
        )
    }
}

super::impl_from_payload!(OccupancyGrid);

/// The layout of an `n * n` row-major covariance matrix.
fn covariance_layout(n: i32) -> DataType {
    DataType::fixed_size_list(Field::required("v", DataType::Float64), n * n)
}

/// Reads a 6x6 covariance out of a `FixedSizeList<Float64, 36>` column.
fn covariance_at(column: &ArrayRef, field: &str) -> Result<[f64; COVARIANCE_6X6]> {
    let values = read::fixed_tuple_at::<f64>(column, 0, COVARIANCE_6X6)?;
    <[f64; COVARIANCE_6X6]>::try_from(values.as_slice()).map_err(|_| {
        DataError::MessageFixedArrayLength {
            field: field.to_owned(),
            expected: COVARIANCE_6X6,
            actual: values.len(),
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::assert_registry_layout;
    use crate::message::geometry::{Quaternion, Vector3};

    #[test]
    fn every_nav_type_conforms_to_the_registry() {
        assert_registry_layout::<Odometry>().unwrap();
        assert_registry_layout::<Path>().unwrap();
        assert_registry_layout::<OccupancyGrid>().unwrap();
    }

    #[test]
    fn odometry_round_trips_with_its_covariances() {
        let mut odom = Odometry::from_estimate(
            Pose::new(Vector3::new(1.0, 2.0, 0.0), Quaternion::identity()),
            Twist::new(Vector3::new(0.5, 0.0, 0.0), Vector3::new(0.0, 0.0, 0.1)),
        );
        odom.pose_covariance[0] = 0.25;
        odom.twist_covariance[35] = 0.5;
        let batch = odom.to_record_batch().unwrap();
        let decoded = Odometry::from_record_batch(&batch).unwrap();
        assert_eq!(decoded, odom);
        assert_eq!(decoded.pose_covariance[0], 0.25);
        assert_eq!(decoded.twist_covariance[35], 0.5);
        assert_eq!(Odometry::default().pose, Pose::default());
    }

    #[test]
    fn paths_round_trip_as_a_top_level_list() {
        let path = Path::new(vec![
            StampedPose::new(
                Timestamp::from_nanos(1),
                Pose::new(Vector3::new(0.0, 0.0, 0.0), Quaternion::identity()),
            ),
            StampedPose::new(
                Timestamp::from_nanos(2),
                Pose::new(Vector3::new(1.0, 0.0, 0.0), Quaternion::identity()),
            ),
        ]);
        let batch = path.to_record_batch().unwrap();
        assert!(
            matches!(Path::data_type(), DataType::List(_)),
            "a path is its sequence"
        );
        let decoded = Path::from_record_batch(&batch).unwrap();
        assert_eq!(decoded, path);
        assert_eq!(decoded.len(), 2);
        assert!(!decoded.is_empty());
        assert_eq!(decoded.poses[1].stamp.nanos(), 2);
    }

    #[test]
    fn an_empty_path_round_trips() {
        let path = Path::default();
        assert!(path.is_empty());
        let batch = path.to_record_batch().unwrap();
        assert_eq!(Path::from_record_batch(&batch).unwrap(), path);
    }

    #[test]
    fn occupancy_grids_round_trip_and_check_their_dimensions() {
        let grid =
            OccupancyGrid::new(0.05, 3, 2, Pose::default(), vec![-1, 0, 50, 100, 0, -1]).unwrap();
        let batch = grid.to_record_batch().unwrap();
        assert_eq!(OccupancyGrid::from_record_batch(&batch).unwrap(), grid);
        assert_eq!(grid.cell_at(0, 0), Some(-1));
        assert_eq!(grid.cell_at(2, 1), Some(-1));
        assert_eq!(grid.cell_at(1, 0), Some(0));
        assert_eq!(grid.cell_at(3, 0), None);
        assert_eq!(grid.cell_at(0, 2), None);

        let error = OccupancyGrid::new(0.05, 3, 2, Pose::default(), vec![0, 0]).unwrap_err();
        assert!(matches!(error, DataError::ChildLengthMismatch { .. }));
    }

    #[test]
    fn a_wrong_layout_is_refused() {
        let odom = Odometry::default().to_record_batch().unwrap();
        assert!(Path::from_record_batch(&odom).is_err());
        assert!(OccupancyGrid::from_record_batch(&odom).is_err());

        let path = Path::default().to_record_batch().unwrap();
        assert!(Odometry::from_record_batch(&path).is_err());
    }

    #[test]
    fn stamped_poses_read_back_row_by_row() {
        let entries = [
            StampedPose::new(Timestamp::from_nanos(10), Pose::default()),
            StampedPose::new(Timestamp::from_nanos(20), Pose::default()),
        ];
        let column = StampedPose::column(&entries).unwrap();
        assert_eq!(StampedPose::at(&column, 1).unwrap().stamp.nanos(), 20);
        assert!(StampedPose::at(&column, 2).is_err());
    }
}
