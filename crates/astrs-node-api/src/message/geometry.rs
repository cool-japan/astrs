//! `std/geometry/v1` — rigid-body quantities (§24.3).
//!
//! Six types, all `Struct` compositions of `Float64` scalars, all mirroring
//! their `geometry_msgs` counterpart field for field so `astrs-ros2` bridges
//! them by column name rather than by translation table.
//!
//! | Type | Layout |
//! |---|---|
//! | [`Vector3`] | `{x, y, z}` |
//! | [`Quaternion`] | `{x, y, z, w}` — scalar last, the ROS 2 and Eigen order |
//! | [`Pose`] | `{position: Vector3, orientation: Quaternion}` |
//! | [`Transform`] | `{translation: Vector3, rotation: Quaternion}` |
//! | [`Twist`] | `{linear: Vector3, angular: Vector3}` |
//! | [`Accel`] | `{linear: Vector3, angular: Vector3}` |
//!
//! [`Twist`] and [`Accel`] are byte-identical: a velocity and an acceleration
//! have the same shape, and only the URN says which one a port carries. That
//! is deliberate — it is also why `astrs-data`'s reverse lookup refuses to
//! guess a URN from a compound layout.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{AstrsMessage, Pose, Quaternion, Vector3};
//!
//! let pose = Pose::new(Vector3::new(1.0, 2.0, 3.0), Quaternion::identity());
//! let batch = pose.to_record_batch()?;
//! assert_eq!(Pose::from_record_batch(&batch)?, pose);
//! assert!(pose.orientation.is_normalized(1e-9));
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::{AstrsMessage, DataType, Field, RecordBatch, Result};

use super::{build, read, single_row_column};

/// A three-component vector — `std/geometry/v1/Vector3`.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd)]
pub struct Vector3 {
    /// The x component.
    pub x: f64,
    /// The y component.
    pub y: f64,
    /// The z component.
    pub z: f64,
}

impl Vector3 {
    /// The zero vector.
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// A vector from its components.
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// The vector's Euclidean length.
    #[must_use]
    pub fn norm(self) -> f64 {
        self.x.hypot(self.y).hypot(self.z)
    }

    /// The components as an array, in `xyz` order.
    #[must_use]
    pub const fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }

    /// A vector from an `xyz` array.
    #[must_use]
    pub const fn from_array(values: [f64; 3]) -> Self {
        Self::new(values[0], values[1], values[2])
    }

    /// The columnar layout of this type, shared with every composite that
    /// embeds it.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("x", DataType::Float64),
            Field::required("y", DataType::Float64),
            Field::required("z", DataType::Float64),
        ])
    }

    /// Builds the child column for `values`.
    ///
    /// # Errors
    ///
    /// [`astrs_data::DataError`] when the columns cannot be assembled.
    pub fn column(values: &[Self]) -> Result<astrs_data::ArrayRef> {
        let x: Vec<f64> = values.iter().map(|value| value.x).collect();
        let y: Vec<f64> = values.iter().map(|value| value.y).collect();
        let z: Vec<f64> = values.iter().map(|value| value.z).collect();
        build::structure(vec![
            ("x", build::primitive::<f64>(&x)),
            ("y", build::primitive::<f64>(&y)),
            ("z", build::primitive::<f64>(&z)),
        ])
    }

    /// Reads one row of a `Vector3` column.
    ///
    /// # Errors
    ///
    /// [`astrs_data::DataError`] for a layout mismatch or an out-of-range row.
    pub fn at(column: &astrs_data::ArrayRef, row: usize) -> Result<Self> {
        let strukt = read::structure(column)?;
        Ok(Self {
            x: read::f64_at(read::field(strukt, "x")?, row)?,
            y: read::f64_at(read::field(strukt, "y")?, row)?,
            z: read::f64_at(read::field(strukt, "z")?, row)?,
        })
    }
}

impl From<[f64; 3]> for Vector3 {
    fn from(values: [f64; 3]) -> Self {
        Self::from_array(values)
    }
}

impl AstrsMessage for Vector3 {
    const URN: &'static str = "std/geometry/v1/Vector3";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(Self::column(
            core::slice::from_ref(self),
        )?))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Self::at(single_row_column(batch)?, 0)
    }
}

super::impl_from_payload!(Vector3);

/// A rotation — `std/geometry/v1/Quaternion`, stored `xyzw` (scalar last).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Quaternion {
    /// The x component of the vector part.
    pub x: f64,
    /// The y component of the vector part.
    pub y: f64,
    /// The z component of the vector part.
    pub z: f64,
    /// The scalar part.
    pub w: f64,
}

impl Quaternion {
    /// The identity rotation.
    pub const IDENTITY: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        w: 1.0,
    };

    /// A quaternion from its components, in `xyzw` order.
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64, w: f64) -> Self {
        Self { x, y, z, w }
    }

    /// The identity rotation.
    #[must_use]
    pub const fn identity() -> Self {
        Self::IDENTITY
    }

    /// The quaternion's norm.
    #[must_use]
    pub fn norm(self) -> f64 {
        self.x.hypot(self.y).hypot(self.z).hypot(self.w)
    }

    /// Whether the quaternion is a unit rotation within `tolerance`.
    #[must_use]
    pub fn is_normalized(self, tolerance: f64) -> bool {
        (self.norm() - 1.0).abs() <= tolerance
    }

    /// The quaternion scaled to unit length, or [`Quaternion::IDENTITY`] when
    /// it has no length to scale.
    #[must_use]
    pub fn normalized(self) -> Self {
        let norm = self.norm();
        if norm == 0.0 || !norm.is_finite() {
            return Self::IDENTITY;
        }
        Self::new(self.x / norm, self.y / norm, self.z / norm, self.w / norm)
    }

    /// The components as an array, in `xyzw` order.
    #[must_use]
    pub const fn to_array(self) -> [f64; 4] {
        [self.x, self.y, self.z, self.w]
    }

    /// The columnar layout of this type.
    #[must_use]
    pub fn layout() -> DataType {
        DataType::strukt([
            Field::required("x", DataType::Float64),
            Field::required("y", DataType::Float64),
            Field::required("z", DataType::Float64),
            Field::required("w", DataType::Float64),
        ])
    }

    /// Builds the child column for `values`.
    ///
    /// # Errors
    ///
    /// [`astrs_data::DataError`] when the columns cannot be assembled.
    pub fn column(values: &[Self]) -> Result<astrs_data::ArrayRef> {
        let x: Vec<f64> = values.iter().map(|value| value.x).collect();
        let y: Vec<f64> = values.iter().map(|value| value.y).collect();
        let z: Vec<f64> = values.iter().map(|value| value.z).collect();
        let w: Vec<f64> = values.iter().map(|value| value.w).collect();
        build::structure(vec![
            ("x", build::primitive::<f64>(&x)),
            ("y", build::primitive::<f64>(&y)),
            ("z", build::primitive::<f64>(&z)),
            ("w", build::primitive::<f64>(&w)),
        ])
    }

    /// Reads one row of a `Quaternion` column.
    ///
    /// # Errors
    ///
    /// [`astrs_data::DataError`] for a layout mismatch or an out-of-range row.
    pub fn at(column: &astrs_data::ArrayRef, row: usize) -> Result<Self> {
        let strukt = read::structure(column)?;
        Ok(Self {
            x: read::f64_at(read::field(strukt, "x")?, row)?,
            y: read::f64_at(read::field(strukt, "y")?, row)?,
            z: read::f64_at(read::field(strukt, "z")?, row)?,
            w: read::f64_at(read::field(strukt, "w")?, row)?,
        })
    }
}

impl Default for Quaternion {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl AstrsMessage for Quaternion {
    const URN: &'static str = "std/geometry/v1/Quaternion";

    fn data_type() -> DataType {
        Self::layout()
    }

    fn to_record_batch(&self) -> Result<RecordBatch> {
        Ok(RecordBatch::from_payload(Self::column(
            core::slice::from_ref(self),
        )?))
    }

    fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
        Self::at(single_row_column(batch)?, 0)
    }
}

super::impl_from_payload!(Quaternion);

/// Writes a two-`Vector3`/`Quaternion` composite: `Pose`, `Transform`,
/// `Twist` and `Accel` differ only in their field names and URN.
macro_rules! composite {
    (
        $(#[$meta:meta])*
        $name:ident, $urn:literal,
        $first:ident : $first_ty:ty = $first_name:literal,
        $second:ident : $second_ty:ty = $second_name:literal
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd)]
        pub struct $name {
            #[doc = concat!("The `", $first_name, "` component.")]
            pub $first: $first_ty,
            #[doc = concat!("The `", $second_name, "` component.")]
            pub $second: $second_ty,
        }

        impl $name {
            /// A value from its two components.
            #[must_use]
            pub const fn new($first: $first_ty, $second: $second_ty) -> Self {
                Self { $first, $second }
            }

            /// The columnar layout of this type.
            #[must_use]
            pub fn layout() -> DataType {
                DataType::strukt([
                    Field::required($first_name, <$first_ty>::layout()),
                    Field::required($second_name, <$second_ty>::layout()),
                ])
            }

            /// Builds the child column for `values`.
            ///
            /// # Errors
            ///
            /// [`astrs_data::DataError`] when the columns cannot be assembled.
            pub fn column(values: &[Self]) -> Result<astrs_data::ArrayRef> {
                let first: Vec<$first_ty> = values.iter().map(|value| value.$first).collect();
                let second: Vec<$second_ty> = values.iter().map(|value| value.$second).collect();
                build::structure(vec![
                    ($first_name, <$first_ty>::column(&first)?),
                    ($second_name, <$second_ty>::column(&second)?),
                ])
            }

            /// Reads one row of a column of this type.
            ///
            /// # Errors
            ///
            /// [`astrs_data::DataError`] for a layout mismatch or an
            /// out-of-range row.
            pub fn at(column: &astrs_data::ArrayRef, row: usize) -> Result<Self> {
                let strukt = read::structure(column)?;
                Ok(Self {
                    $first: <$first_ty>::at(read::field(strukt, $first_name)?, row)?,
                    $second: <$second_ty>::at(read::field(strukt, $second_name)?, row)?,
                })
            }
        }

        impl AstrsMessage for $name {
            const URN: &'static str = $urn;

            fn data_type() -> DataType {
                Self::layout()
            }

            fn to_record_batch(&self) -> Result<RecordBatch> {
                Ok(RecordBatch::from_payload(Self::column(
                    core::slice::from_ref(self),
                )?))
            }

            fn from_record_batch(batch: &RecordBatch) -> Result<Self> {
                Self::at(single_row_column(batch)?, 0)
            }
        }

        super::impl_from_payload!($name);
    };
}

composite!(
    /// A position and an orientation — `std/geometry/v1/Pose`.
    Pose,
    "std/geometry/v1/Pose",
    position: Vector3 = "position",
    orientation: Quaternion = "orientation"
);

composite!(
    /// A rigid-body transform — `std/geometry/v1/Transform`.
    ///
    /// The same shape as [`Pose`] under different names: a `Transform`
    /// answers "how do I get from `frame` to `child_frame`", a `Pose`
    /// answers "where is this, expressed in `frame`".
    Transform,
    "std/geometry/v1/Transform",
    translation: Vector3 = "translation",
    rotation: Quaternion = "rotation"
);

composite!(
    /// A velocity — `std/geometry/v1/Twist`.
    Twist,
    "std/geometry/v1/Twist",
    linear: Vector3 = "linear",
    angular: Vector3 = "angular"
);

composite!(
    /// An acceleration — `std/geometry/v1/Accel`.
    ///
    /// Byte-identical to [`Twist`]; only the URN distinguishes them.
    Accel,
    "std/geometry/v1/Accel",
    linear: Vector3 = "linear",
    angular: Vector3 = "angular"
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::assert_registry_layout;

    #[test]
    fn every_geometry_type_conforms_to_the_registry() {
        assert_registry_layout::<Vector3>().unwrap();
        assert_registry_layout::<Quaternion>().unwrap();
        assert_registry_layout::<Pose>().unwrap();
        assert_registry_layout::<Transform>().unwrap();
        assert_registry_layout::<Twist>().unwrap();
        assert_registry_layout::<Accel>().unwrap();
    }

    #[test]
    fn vectors_round_trip() {
        let value = Vector3::new(1.5, -2.5, 3.5);
        let batch = value.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(Vector3::from_record_batch(&batch).unwrap(), value);
        assert_eq!(value.to_array(), [1.5, -2.5, 3.5]);
        assert_eq!(Vector3::from([1.5, -2.5, 3.5]), value);
        assert_eq!(Vector3::ZERO.norm(), 0.0);
        assert_eq!(Vector3::new(3.0, 4.0, 0.0).norm(), 5.0);
        assert_eq!(Vector3::default(), Vector3::ZERO);
    }

    #[test]
    fn quaternions_round_trip_and_normalise() {
        let half = core::f64::consts::FRAC_1_SQRT_2;
        let value = Quaternion::new(0.0, 0.0, half, half);
        let batch = value.to_record_batch().unwrap();
        assert_eq!(Quaternion::from_record_batch(&batch).unwrap(), value);
        assert!(value.is_normalized(1e-12));
        assert_eq!(Quaternion::default(), Quaternion::IDENTITY);
        assert_eq!(Quaternion::identity().to_array(), [0.0, 0.0, 0.0, 1.0]);

        let scaled = Quaternion::new(0.0, 0.0, 0.0, 4.0);
        assert!(!scaled.is_normalized(1e-9));
        assert!(scaled.normalized().is_normalized(1e-12));

        // A degenerate quaternion cannot be normalised; the identity is the
        // only answer that stays a rotation.
        let zero = Quaternion::new(0.0, 0.0, 0.0, 0.0);
        assert_eq!(zero.normalized(), Quaternion::IDENTITY);
        let nan = Quaternion::new(f64::NAN, 0.0, 0.0, 0.0);
        assert_eq!(nan.normalized(), Quaternion::IDENTITY);
    }

    #[test]
    fn composites_round_trip() {
        let pose = Pose::new(Vector3::new(1.0, 2.0, 3.0), Quaternion::identity());
        assert_eq!(
            Pose::from_record_batch(&pose.to_record_batch().unwrap()).unwrap(),
            pose
        );

        let transform = Transform::new(Vector3::new(-1.0, 0.0, 0.5), Quaternion::identity());
        assert_eq!(
            Transform::from_record_batch(&transform.to_record_batch().unwrap()).unwrap(),
            transform
        );

        let twist = Twist::new(Vector3::new(1.0, 0.0, 0.0), Vector3::new(0.0, 0.0, 0.2));
        assert_eq!(
            Twist::from_record_batch(&twist.to_record_batch().unwrap()).unwrap(),
            twist
        );

        let accel = Accel::new(Vector3::new(0.0, 0.0, -9.81), Vector3::ZERO);
        assert_eq!(
            Accel::from_record_batch(&accel.to_record_batch().unwrap()).unwrap(),
            accel
        );
    }

    #[test]
    fn twist_and_accel_share_a_layout_but_not_a_urn() {
        assert_eq!(Twist::data_type(), Accel::data_type());
        assert_ne!(<Twist as AstrsMessage>::URN, <Accel as AstrsMessage>::URN);
    }

    #[test]
    fn multi_row_columns_hold_every_value() {
        let poses = [
            Pose::new(Vector3::new(0.0, 0.0, 0.0), Quaternion::identity()),
            Pose::new(Vector3::new(1.0, 1.0, 1.0), Quaternion::identity()),
            Pose::new(Vector3::new(2.0, 4.0, 8.0), Quaternion::identity()),
        ];
        let column = Pose::column(&poses).unwrap();
        assert_eq!(column.len(), 3);
        for (row, expected) in poses.iter().enumerate() {
            assert_eq!(&Pose::at(&column, row).unwrap(), expected);
        }
        assert!(Pose::at(&column, 3).is_err());
    }

    #[test]
    fn a_wrong_layout_is_refused_rather_than_reinterpreted() {
        let vector = Vector3::new(1.0, 2.0, 3.0).to_record_batch().unwrap();
        assert!(Quaternion::from_record_batch(&vector).is_err());
        assert!(Pose::from_record_batch(&vector).is_err());

        let scalar = crate::message::Scalar::from(1.0_f64)
            .to_record_batch()
            .unwrap();
        assert!(Vector3::from_record_batch(&scalar).is_err());
    }
}
