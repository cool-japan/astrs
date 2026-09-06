//! [`Isometry3`] — a rigid-body transform: a rotation plus a translation.

use std::ops::Mul;

use astrs_data::array::{ArrayExt, ArrayRef, IntoArrayRef, StructArray};
use astrs_data::urn::layouts::geometry::transform_layout;
use astrs_data::{AstrsMessage, DataError, DataType, RecordBatch, Result as DataResult};

use crate::math::quaternion::Quaternion;
use crate::math::vector3::Vector3;

/// A rigid-body transform (an element of `SE(3)`): a rotation composed with
/// a translation.
///
/// Field names and order (`translation` then `rotation`) match
/// `geometry_msgs/msg/Transform` exactly, and [`AstrsMessage`] reports
/// `std/geometry/v1/Transform` — `astrs_data`'s own
/// `urn::layouts::geometry::transform_layout` doc names this crate's
/// transform tree as the type that layout is built for (blueprint §24.3).
/// See [`crate::interop::geometry_msgs`] for the CDR-side counterpart
/// (`astrs_idl::generated::geometry_msgs::Transform`, used when bridging
/// `/tf` rather than AstRS's own columnar wire).
///
/// # Convention
///
/// `set_transform(parent, child, isometry)` stores an `Isometry3` such that
/// `isometry.transform_point(p)` maps a point expressed in `child`'s frame
/// to the same point expressed in `parent`'s frame — "the pose of `child`
/// in `parent`," tf2's own convention for what a stored
/// `TransformStamped.transform` means. [`Isometry3::compose`] and
/// [`Isometry3::inverse`] are defined consistently with this: composing the
/// parent→child transform with the child→grandchild transform gives the
/// parent→grandchild transform, exactly the chain
/// [`crate::buffer::TransformBuffer::lookup_transform`]'s frame walk builds.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Isometry3 {
    /// The translation component.
    pub translation: Vector3,
    /// The rotation component.
    pub rotation: Quaternion,
}

impl Isometry3 {
    /// The identity transform: no translation, no rotation.
    ///
    /// Safe to derive via [`Isometry3::default`] (unlike
    /// [`Quaternion::default`] alone) precisely because both field types'
    /// own `Default` impls are already the correct identity element
    /// (`Vector3::ZERO`, `Quaternion::IDENTITY`) — see
    /// [`Quaternion::IDENTITY`]'s docs for why that one had to be
    /// hand-written.
    pub const IDENTITY: Self = Self::new(Vector3::ZERO, Quaternion::IDENTITY);

    /// Builds a transform from its translation and rotation.
    #[must_use]
    pub const fn new(translation: Vector3, rotation: Quaternion) -> Self {
        Self {
            translation,
            rotation,
        }
    }

    /// A pure translation, no rotation.
    #[must_use]
    pub const fn from_translation(translation: Vector3) -> Self {
        Self::new(translation, Quaternion::IDENTITY)
    }

    /// A pure rotation about the origin, no translation.
    #[must_use]
    pub const fn from_rotation(rotation: Quaternion) -> Self {
        Self::new(Vector3::ZERO, rotation)
    }

    /// Composes `self` with `other`: `self ∘ other`.
    ///
    /// Applying the result to a point equals applying `other` first, then
    /// `self`: `self.compose(other).transform_point(p) ==
    /// self.transform_point(other.transform_point(p))` (property-tested
    /// below, alongside associativity). This is also [`Mul`]'s
    /// implementation.
    ///
    /// ```
    /// use astrs_tf::math::{Isometry3, Vector3};
    ///
    /// // map->odom (10m on X) composed with odom->base_link (2m on X)
    /// // gives map->base_link (12m on X).
    /// let map_from_odom = Isometry3::from_translation(Vector3::new(10.0, 0.0, 0.0));
    /// let odom_from_base_link = Isometry3::from_translation(Vector3::new(2.0, 0.0, 0.0));
    /// let map_from_base_link = map_from_odom.compose(odom_from_base_link);
    /// assert_eq!(map_from_base_link.translation, Vector3::new(12.0, 0.0, 0.0));
    /// ```
    #[must_use]
    pub fn compose(self, other: Self) -> Self {
        let rotation = self.rotation * other.rotation;
        let translation = self.translation + self.rotation.rotate_vector(other.translation);
        Self::new(translation, rotation)
    }

    /// The inverse transform, undoing `self`.
    ///
    /// Returns `None` only when [`Quaternion::inverse`] does — a degenerate
    /// (zero-norm or non-finite) rotation, which
    /// [`crate::buffer::TransformBuffer::set_transform`] never allows into
    /// the buffer in the first place (see [`crate::error::TfError::DegenerateQuaternion`]),
    /// so this is effectively infallible for any `Isometry3` the buffer
    /// itself produced.
    #[must_use]
    pub fn inverse(self) -> Option<Self> {
        let inv_rotation = self.rotation.inverse()?;
        let inv_translation = -inv_rotation.rotate_vector(self.translation);
        Some(Self::new(inv_translation, inv_rotation))
    }

    /// The inverse transform, assuming the rotation is already unit.
    ///
    /// Uses [`Quaternion::conjugate`] rather than the fallible
    /// [`Quaternion::inverse`], so this never fails — the companion to
    /// [`Isometry3::inverse`] the same way [`Quaternion::conjugate`] is
    /// [`Quaternion::inverse`]'s companion. [`crate::buffer::TransformBuffer`]
    /// uses this rather than [`Isometry3::inverse`] for the transforms its
    /// own lookup composes, since every rotation the buffer stores was
    /// already normalized by [`crate::buffer::TransformBuffer::set_transform`]
    /// (and a Hamilton product of unit quaternions is unit again, so a
    /// composed chain of them stays unit) — calling the fallible general
    /// form there would need a [`crate::error::TfError`] variant for a
    /// case that is a buffer-invariant violation, not a normal error
    /// condition a caller can act on.
    ///
    /// ```
    /// use astrs_tf::math::{Isometry3, Vector3};
    ///
    /// let map_from_odom = Isometry3::from_translation(Vector3::new(10.0, 0.0, 0.0));
    /// let odom_from_map = map_from_odom.inverse_unit();
    /// assert_eq!(odom_from_map.translation, Vector3::new(-10.0, 0.0, 0.0));
    /// ```
    #[must_use]
    pub fn inverse_unit(self) -> Self {
        let inv_rotation = self.rotation.conjugate();
        let inv_translation = -inv_rotation.rotate_vector(self.translation);
        Self::new(inv_translation, inv_rotation)
    }

    /// Maps a point expressed in this transform's source frame into its
    /// target frame: rotates, then translates.
    ///
    /// ```
    /// use astrs_tf::math::{Isometry3, Quaternion, Vector3};
    /// use std::f64::consts::FRAC_PI_2;
    ///
    /// // Rotate 90 degrees about Z, then translate +1 on X.
    /// let transform = Isometry3::new(
    ///     Vector3::new(1.0, 0.0, 0.0),
    ///     Quaternion::from_axis_angle(Vector3::UNIT_Z, FRAC_PI_2),
    /// );
    /// let result = transform.transform_point(Vector3::new(1.0, 0.0, 0.0));
    /// assert!((result - Vector3::new(1.0, 1.0, 0.0)).norm() < 1e-9);
    /// ```
    #[must_use]
    pub fn transform_point(self, point: Vector3) -> Vector3 {
        self.rotation.rotate_vector(point) + self.translation
    }

    /// Maps a free vector (a direction or a velocity, not a position) —
    /// rotation only, no translation, since a free vector has no origin to
    /// carry along.
    #[must_use]
    pub fn transform_vector(self, vector: Vector3) -> Vector3 {
        self.rotation.rotate_vector(vector)
    }

    /// Interpolates between `self` (`t = 0`) and `other` (`t = 1`):
    /// [`Vector3::lerp`] on the translation, [`Quaternion::slerp`] on the
    /// rotation. The combination blueprint §10.6 asks for by name.
    ///
    /// ```
    /// use astrs_tf::math::{Isometry3, Vector3};
    ///
    /// let start = Isometry3::from_translation(Vector3::new(0.0, 0.0, 0.0));
    /// let end = Isometry3::from_translation(Vector3::new(10.0, 0.0, 0.0));
    /// let halfway = start.interpolate(end, 0.5);
    /// assert_eq!(halfway.translation, Vector3::new(5.0, 0.0, 0.0));
    /// ```
    #[must_use]
    pub fn interpolate(self, other: Self, t: f64) -> Self {
        Self::new(
            self.translation.lerp(other.translation, t),
            self.rotation.slerp(other.rotation, t),
        )
    }

    /// `true` when the translation is finite and the rotation is finite —
    /// does **not** check the rotation is unit length; see
    /// [`Quaternion::normalize`] for that.
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.translation.is_finite() && self.rotation.is_finite()
    }
}

impl Mul for Isometry3 {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self {
        self.compose(rhs)
    }
}

impl AstrsMessage for Isometry3 {
    const URN: &'static str = "std/geometry/v1/Transform";

    fn data_type() -> DataType {
        transform_layout()
    }

    fn to_record_batch(&self) -> DataResult<RecordBatch> {
        let DataType::Struct(fields) = Self::data_type() else {
            return Err(DataError::type_mismatch(Self::data_type(), DataType::Null));
        };
        let columns: Vec<ArrayRef> = vec![
            self.translation.to_array_ref()?,
            self.rotation.to_array_ref()?,
        ];
        let strukt = StructArray::try_new_with_len(fields, columns, 1, None)?;
        Ok(RecordBatch::from_payload(strukt.into_array_ref()))
    }

    fn from_record_batch(batch: &RecordBatch) -> DataResult<Self> {
        if batch.num_rows() != 1 {
            return Err(DataError::MessageRowCount {
                actual: batch.num_rows(),
            });
        }
        let column = batch
            .payload_column()
            .ok_or(DataError::ColumnCountMismatch {
                fields: 2,
                columns: 0,
            })?;
        let strukt = column.try_downcast::<StructArray>()?;
        let columns = strukt.columns();
        let translation_column = columns.first().ok_or(DataError::ColumnCountMismatch {
            fields: 2,
            columns: columns.len(),
        })?;
        let rotation_column = columns.get(1).ok_or(DataError::ColumnCountMismatch {
            fields: 2,
            columns: columns.len(),
        })?;
        Ok(Self {
            translation: Vector3::from_array_ref(translation_column)?,
            rotation: Quaternion::from_array_ref(rotation_column)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use proptest::prelude::*;

    const EPS: f64 = 1e-9;

    fn assert_vec_close(a: Vector3, b: Vector3, eps: f64) {
        assert!((a - b).norm() < eps, "{a:?} vs {b:?}");
    }

    #[test]
    fn identity_is_the_default() {
        assert_eq!(Isometry3::default(), Isometry3::IDENTITY);
        assert_eq!(Isometry3::IDENTITY.translation, Vector3::ZERO);
        assert_eq!(Isometry3::IDENTITY.rotation, Quaternion::IDENTITY);
    }

    #[test]
    fn identity_transform_point_is_a_no_op() {
        let p = Vector3::new(1.0, 2.0, 3.0);
        assert_eq!(Isometry3::IDENTITY.transform_point(p), p);
    }

    #[test]
    fn pure_translation_only_shifts() {
        let t = Isometry3::from_translation(Vector3::new(1.0, 0.0, 0.0));
        assert_eq!(
            t.transform_point(Vector3::ZERO),
            Vector3::new(1.0, 0.0, 0.0)
        );
        // A free vector is unaffected by a pure translation.
        assert_eq!(t.transform_vector(Vector3::UNIT_X), Vector3::UNIT_X);
    }

    #[test]
    fn pure_rotation_only_rotates() {
        let r = Isometry3::from_rotation(Quaternion::from_axis_angle(
            Vector3::UNIT_Z,
            std::f64::consts::FRAC_PI_2,
        ));
        assert_vec_close(r.transform_point(Vector3::UNIT_X), Vector3::UNIT_Y, EPS);
    }

    #[test]
    fn compose_matches_sequential_application() {
        let a = Isometry3::new(
            Vector3::new(1.0, 0.0, 0.0),
            Quaternion::from_axis_angle(Vector3::UNIT_Z, 0.5),
        );
        let b = Isometry3::new(
            Vector3::new(0.0, 2.0, 0.0),
            Quaternion::from_axis_angle(Vector3::UNIT_X, 0.8),
        );
        let p = Vector3::new(3.0, -1.0, 2.0);
        let composed = a.compose(b).transform_point(p);
        let sequential = a.transform_point(b.transform_point(p));
        assert_vec_close(composed, sequential, EPS);
    }

    #[test]
    fn inverse_undoes_the_transform() {
        let t = Isometry3::new(
            Vector3::new(5.0, -3.0, 2.0),
            Quaternion::from_axis_angle(Vector3::new(1.0, 1.0, 1.0), 1.2),
        );
        let inv = t.inverse().unwrap();
        let p = Vector3::new(4.0, 4.0, 4.0);
        assert_vec_close(inv.transform_point(t.transform_point(p)), p, EPS);
        assert_vec_close(t.transform_point(inv.transform_point(p)), p, EPS);
    }

    #[test]
    fn inverse_unit_matches_the_general_inverse_for_a_unit_rotation() {
        let t = Isometry3::new(
            Vector3::new(5.0, -3.0, 2.0),
            Quaternion::from_axis_angle(Vector3::new(1.0, 1.0, 1.0), 1.2),
        );
        let general = t.inverse().unwrap();
        let fast = t.inverse_unit();
        assert_vec_close(fast.translation, general.translation, EPS);
        assert!((fast.rotation.dot(general.rotation).abs() - 1.0).abs() < EPS);
    }

    #[test]
    fn interpolate_at_the_endpoints_returns_the_endpoints() {
        let a = Isometry3::IDENTITY;
        let b = Isometry3::new(
            Vector3::new(2.0, 0.0, 0.0),
            Quaternion::from_axis_angle(Vector3::UNIT_Z, 1.0),
        );
        assert_eq!(a.interpolate(b, 0.0).translation, a.translation);
        assert_eq!(a.interpolate(b, 1.0).translation, b.translation);
    }

    #[test]
    fn round_trips_through_a_record_batch() {
        let t = Isometry3::new(
            Vector3::new(1.0, 2.0, 3.0),
            Quaternion::from_axis_angle(Vector3::UNIT_Y, 0.75),
        );
        let batch = t.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        let back = Isometry3::from_record_batch(&batch).unwrap();
        assert_vec_close(back.translation, t.translation, EPS);
        assert!((back.rotation.dot(t.rotation).abs() - 1.0).abs() < EPS);
    }

    #[test]
    fn data_type_matches_the_curated_geometry_layout() {
        assert_eq!(Isometry3::data_type(), transform_layout());
        assert_eq!(Isometry3::URN, "std/geometry/v1/Transform");
    }

    /// Builds an isometry from raw proptest components, `prop_assume!`-ing
    /// away the one degenerate case ([`Quaternion::from_axis_angle`]'s own
    /// "zero axis" fallback to identity) so every generated case exercises
    /// an actual non-trivial rotation rather than silently degrading to a
    /// pure translation. Must be called from inside a `proptest!` body (it
    /// expands to a `prop_assume!`).
    macro_rules! isometry {
        ($tx:expr, $ty:expr, $tz:expr, $ax:expr, $ay:expr, $az:expr, $angle:expr) => {{
            let axis = Vector3::new($ax, $ay, $az);
            prop_assume!(axis.norm() > 1e-6);
            Isometry3::new(
                Vector3::new($tx, $ty, $tz),
                Quaternion::from_axis_angle(axis, $angle),
            )
        }};
    }

    proptest! {
        /// Transform composition associativity within tolerance — one of
        /// this crate's five mandated property tests:
        /// `(a ∘ b) ∘ c == a ∘ (b ∘ c)` when applied to any point.
        #[test]
        fn composition_is_associative(
            atx in -10f64..10.0, aty in -10f64..10.0, atz in -10f64..10.0,
            aax in -1f64..1.0, aay in -1f64..1.0, aaz in -1f64..1.0, aangle in -6.0f64..6.0,
            btx in -10f64..10.0, bty in -10f64..10.0, btz in -10f64..10.0,
            bax in -1f64..1.0, bay in -1f64..1.0, baz in -1f64..1.0, bangle in -6.0f64..6.0,
            ctx in -10f64..10.0, cty in -10f64..10.0, ctz in -10f64..10.0,
            cax in -1f64..1.0, cay in -1f64..1.0, caz in -1f64..1.0, cangle in -6.0f64..6.0,
            px in -10f64..10.0, py in -10f64..10.0, pz in -10f64..10.0,
        ) {
            let a = isometry!(atx, aty, atz, aax, aay, aaz, aangle);
            let b = isometry!(btx, bty, btz, bax, bay, baz, bangle);
            let c = isometry!(ctx, cty, ctz, cax, cay, caz, cangle);
            let p = Vector3::new(px, py, pz);

            let left = a.compose(b).compose(c).transform_point(p);
            let right = a.compose(b.compose(c)).transform_point(p);
            prop_assert!((left - right).norm() < 1e-6, "{left:?} vs {right:?}");
        }

        /// `interpolate` never leaves the translation's bracketing interval
        /// (componentwise) and always returns a finite, well-formed
        /// transform — the buffer-lookup-independent half of "interpolation
        /// stays inside its bracketing interval."
        #[test]
        fn interpolate_translation_stays_within_the_bracketing_interval(
            atx in -100f64..100.0, aty in -100f64..100.0, atz in -100f64..100.0,
            btx in -100f64..100.0, bty in -100f64..100.0, btz in -100f64..100.0,
            t in 0f64..=1.0,
        ) {
            let a = Isometry3::from_translation(Vector3::new(atx, aty, atz));
            let b = Isometry3::from_translation(Vector3::new(btx, bty, btz));
            let mid = a.interpolate(b, t).translation;
            let within = |m: f64, x: f64, y: f64| {
                let (lo, hi) = (x.min(y), x.max(y));
                m >= lo - 1e-9 && m <= hi + 1e-9
            };
            prop_assert!(within(mid.x, atx, btx));
            prop_assert!(within(mid.y, aty, bty));
            prop_assert!(within(mid.z, atz, btz));
            prop_assert!(mid.is_finite());
        }

        #[test]
        fn round_trip_holds_through_a_record_batch(
            tx in -100f64..100.0, ty in -100f64..100.0, tz in -100f64..100.0,
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle in -6.0f64..6.0,
        ) {
            let t = isometry!(tx, ty, tz, ax, ay, az, angle);
            let batch = t.to_record_batch().unwrap();
            let back = Isometry3::from_record_batch(&batch).unwrap();
            prop_assert!((back.translation - t.translation).norm() < 1e-9);
            prop_assert!((back.rotation.dot(t.rotation).abs() - 1.0).abs() < 1e-9);
        }
    }
}
