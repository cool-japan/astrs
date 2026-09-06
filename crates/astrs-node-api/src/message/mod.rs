//! The `std` URN message types (blueprint §9.2, §24.3).
//!
//! [`AstrsMessage`] itself lives in `astrs-data` — it has to, since
//! `astrs-idl`'s ROS 2 codegen and `astrs-operator-macros`' derive both
//! implement it and neither may depend upward on the node API (§4.1). This
//! module is where the **hand-written implementations** live: one Rust type
//! per entry of the §24.3 registry, each producing exactly the normative
//! columnar layout that entry declares.
//!
//! ```text
//!   scalar     bool · i8..i64 · u8..u64 · f32/f64 · String · Bytes · Empty · Timestamp
//!   geometry   Vector3 · Quaternion · Pose · Transform · Twist · Accel
//!   sensor     LaserScan · Imu · NavSatFix · Range · PointCloud
//!   vision     Detections · Keypoints · Mask
//!   nav        Odometry · Path · OccupancyGrid
//!   media      Image · AudioFrame · CompressedImage
//! ```
//!
//! # Two traits, on purpose
//!
//! | Trait | Direction | Why separate |
//! |---|---|---|
//! | [`FromPayload`] | payload → value | Some types can only be *read* — [`astrs_data::tensor::ImageView`] is a checked view over somebody else's columns, with no way to construct one from scratch |
//! | [`AstrsMessage`] | both | Everything a node can also *send* |
//!
//! There is deliberately **no** blanket `impl<T: AstrsMessage> FromPayload for
//! T`: the pair is written together for each type, which keeps a read-only
//! implementation such as `ImageView`'s legal instead of overlapping with a
//! blanket one. [`crate::Payload::view`] is generic over [`FromPayload`], so
//! the blueprint's `let img: ImageView = data.view()?;` resolves.
//!
//! # The layouts are normative, not incidental
//!
//! Every implementation here is checked against
//! [`astrs_data::TypeRegistry::std()`] by [`assert_registry_layout`] in its
//! own test. Round-tripping an encoder against its own decoder proves nothing
//! about §24.3 conformance; comparing the produced [`DataType`] against the
//! registry's does.
//!
//! # Parameterised types
//!
//! `Image[pixel=…]`, `AudioFrame[sample=…]` and `PointCloud[fields=…]` have a
//! layout that *depends* on a URN parameter, which a `const URN: &'static str`
//! cannot express. Those three carry the parameter as a Rust value instead
//! ([`PixelFormat`], [`SampleFormat`], [`PointCloud::field_names`]) and expose
//! `to_record_batch`/`from_record_batch` as inherent methods with the same
//! shape as the trait's, plus a `urn()` that renders the parameterised URN a
//! port should declare. `CompressedImage`'s layout does *not* vary with its
//! `format`, so it implements the trait proper.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{AstrsMessage, Pose, Vector3};
//!
//! let batch = Vector3::new(1.0, 2.0, 3.0).to_record_batch()?;
//! assert_eq!(batch.num_rows(), 1);
//! assert_eq!(Vector3::from_record_batch(&batch)?.y, 2.0);
//! assert_eq!(Pose::URN, "std/geometry/v1/Pose");
//! # Ok::<(), astrs_data::DataError>(())
//! ```

pub mod build;
pub mod geometry;
pub mod media;
pub mod nav;
pub mod read;
pub mod scalar;
pub mod sensor;
pub mod vision;

use astrs_data::urn::layout_of_str;
use astrs_data::{ArrayRef, DataError, DataType, RecordBatch, Result};

pub use astrs_data::AstrsMessage;
pub use geometry::{Accel, Pose, Quaternion, Transform, Twist, Vector3};
pub use media::{AudioFrame, CompressedImage, Image, ImageSamples, PixelFormat, SampleFormat};
pub use nav::{OccupancyGrid, Odometry, Path, StampedPose};
pub use scalar::{
    Bytes, BytesRun, Duration, Empty, Flag, FlagRun, Scalar, ScalarRun, ScalarType, Text, TextRun,
    Timestamp, TimestampRun,
};
pub use sensor::{Imu, LaserScan, NavSatFix, PointCloud, Range};
pub use vision::{BoundingBox, Detections, Keypoint, Keypoints, Mask};

/// Reading a typed value out of a decoded payload.
///
/// Implemented by every [`AstrsMessage`] in this module, and separately by the
/// read-only view types that cannot be constructed from scratch.
pub trait FromPayload: Sized {
    /// Reads the value from a decoded payload batch.
    ///
    /// # Errors
    ///
    /// [`DataError`] when the batch's layout is not the one this type
    /// expects, or when it holds no rows.
    fn from_batch(batch: &RecordBatch) -> Result<Self>;
}

/// The single top-level payload column of `batch` (§6.1).
///
/// # Errors
///
/// [`DataError::ColumnCountMismatch`] when the batch carries no column.
///
/// # Examples
///
/// ```
/// use astrs_data::prelude::*;
/// use astrs_node_api::message::payload_column;
///
/// let batch = RecordBatch::from_payload(Float64Array::from_values([1.0]).into_array_ref());
/// assert_eq!(payload_column(&batch)?.len(), 1);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn payload_column(batch: &RecordBatch) -> Result<&ArrayRef> {
    batch
        .payload_column()
        .or_else(|| batch.column(0))
        .ok_or(DataError::ColumnCountMismatch {
            fields: 1,
            columns: 0,
        })
}

/// The single payload column of a batch that must hold exactly one row.
///
/// # Errors
///
/// [`DataError::MessageRowCount`] when the batch does not hold exactly one
/// row, or whatever [`payload_column`] reports.
pub fn single_row_column(batch: &RecordBatch) -> Result<&ArrayRef> {
    if batch.num_rows() != 1 {
        return Err(DataError::MessageRowCount {
            actual: batch.num_rows(),
        });
    }
    payload_column(batch)
}

/// Checks a type's layout against the `std` registry (§24.3).
///
/// This is the conformance gate every implementation in this module runs in
/// its own test: it is the only check that can catch "my encoder and my
/// decoder agree with each other, and both disagree with the spec".
///
/// # Errors
///
/// [`DataError::TypeUrn`] when the URN is unknown or its parameters do not
/// validate, and [`DataError::TypeMismatch`] when the registry's layout is
/// not the one the type produces.
///
/// # Examples
///
/// ```
/// use astrs_node_api::message::{Pose, assert_registry_layout};
///
/// assert_registry_layout::<Pose>()?;
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn assert_registry_layout<T: AstrsMessage>() -> Result<()> {
    assert_layout_matches(T::URN, &T::data_type())
}

/// [`assert_registry_layout`] for a type whose registry entry requires a
/// parameter the `const URN` cannot carry.
///
/// # Errors
///
/// As [`assert_registry_layout`].
///
/// # Examples
///
/// ```
/// use astrs_node_api::message::{CompressedImage, assert_registry_layout_for};
///
/// assert_registry_layout_for::<CompressedImage>("std/media/v1/CompressedImage[format=jpeg]")?;
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn assert_registry_layout_for<T: AstrsMessage>(urn: &str) -> Result<()> {
    assert_layout_matches(urn, &T::data_type())
}

/// Compares a layout against the registry's entry for `urn`.
///
/// # Errors
///
/// As [`assert_registry_layout`].
pub fn assert_layout_matches(urn: &str, actual: &DataType) -> Result<()> {
    let expected = layout_of_str(urn)?;
    if &expected == actual {
        return Ok(());
    }
    Err(DataError::type_mismatch(expected, actual.clone()))
}

/// Writes the [`FromPayload`] half of an implementation, reading row zero.
///
/// Every implementation in this module pairs with one, since there is no
/// blanket impl (see the module documentation).
macro_rules! impl_from_payload {
    ($ty:ty) => {
        impl $crate::message::FromPayload for $ty {
            fn from_batch(batch: &::astrs_data::RecordBatch) -> ::astrs_data::Result<Self> {
                <Self as ::astrs_data::AstrsMessage>::from_record_batch(batch)
            }
        }
    };
}

pub(crate) use impl_from_payload;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_data::array::{Float64Array, IntoArrayRef};

    #[test]
    fn the_payload_column_is_found_by_name_and_by_position() {
        let batch = RecordBatch::from_payload(Float64Array::from_values([1.0]).into_array_ref());
        assert_eq!(payload_column(&batch).unwrap().len(), 1);
        assert_eq!(single_row_column(&batch).unwrap().len(), 1);
    }

    #[test]
    fn a_batch_without_columns_is_reported() {
        let batch =
            RecordBatch::try_new_empty(std::sync::Arc::new(astrs_data::Schema::new(Vec::new())))
                .unwrap();
        assert!(matches!(
            payload_column(&batch),
            Err(DataError::ColumnCountMismatch { .. })
        ));
    }

    #[test]
    fn a_multi_row_batch_is_refused_where_one_row_is_required() {
        let batch =
            RecordBatch::from_payload(Float64Array::from_values([1.0, 2.0]).into_array_ref());
        assert!(matches!(
            single_row_column(&batch),
            Err(DataError::MessageRowCount { actual: 2 })
        ));
    }

    #[test]
    fn conformance_reports_a_layout_that_drifted() {
        assert!(assert_registry_layout::<Vector3>().is_ok());
        let error =
            assert_layout_matches("std/geometry/v1/Vector3", &DataType::Float64).unwrap_err();
        assert!(matches!(error, DataError::TypeMismatch { .. }), "{error}");
    }

    #[test]
    fn an_unknown_urn_is_reported_by_the_registry() {
        let error = assert_layout_matches("std/core/v1/Nonexistent", &DataType::Null).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn schemas_carry_the_data_column() {
        let schema = Vector3::schema();
        assert_eq!(schema.fields().len(), 1);
        assert_eq!(schema.field(0).map(astrs_data::Field::name), Some("data"));
        assert_eq!(
            schema.field(0).map(|field| field.data_type().clone()),
            Some(Vector3::data_type())
        );
    }
}
