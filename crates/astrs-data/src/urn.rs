//! Type URNs — the identifier a port carries (blueprint §3.7, §24.3).
//!
//! An AstRS edge is type-checked before the first message flows, and the thing
//! that gets checked is a **type URN**:
//!
//! ```text
//!   std/sensor/v1/PointCloud[fields=x:y:z,frame=base_link]
//!   ^^^ ^^^^^^ ^^ ^^^^^^^^^^ ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
//!    |    |    |      |       parameters, sorted, optional
//!    |    |    |      type name (upper camel)
//!    |    |    version (u16, no leading zeros)
//!    |    category
//!    namespace — `std` and only `std` in 0.1.0
//! ```
//!
//! Three questions, three answers, three error groups:
//!
//! | Question | Answer | Failure |
//! |---|---|---|
//! | Is this text a URN? | [`TypeUrn::parse`] | the syntax group of [`TypeUrnError`] |
//! | Is this type known, with these parameters? | [`validate`] | the resolution group |
//! | What columnar layout is it? | [`layout_of`] | [`TypeUrnError::LayoutUnavailable`] |
//!
//! Keeping them apart is what lets a graph type-check an edge whose byte
//! layout nobody has frozen yet: `std/nav/v1/Path` validates today and gets a
//! [`DataType`](crate::DataType) later, without either side changing.
//!
//! # Layout
//!
//! ```text
//!   urn::layout     layout_of · validate · urn_for_data_type · accepts_layout
//!   urn::registry   TypeRegistry · TypeEntry · LayoutRule · the std table
//!   urn::parse      TypeUrn — the grammar, the canonical form
//!   urn::error      TypeUrnError
//! ```
//!
//! # Tour
//!
//! ```
//! use astrs_data::urn::{layout_of, validate, TypeUrn};
//! use astrs_data::DataType;
//!
//! // Parse — and normalise: parameters come back sorted by key.
//! let urn = TypeUrn::parse("std/media/v1/Image[width=640,pixel=rgb8]")?;
//! assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8,width=640]");
//!
//! // Resolve against the standard registry — every std/v1 type maps onto a
//! // columnar layout in this build (core/time scalars directly in
//! // `registry`; the other five categories via `layouts`).
//! validate(&urn)?;
//! assert!(layout_of(&urn).is_ok());
//!
//! // A core scalar resolves all the way to a columnar type too.
//! let scalar = TypeUrn::parse("std/core/v1/Float32")?;
//! assert_eq!(layout_of(&scalar), Ok(DataType::Float32));
//! # Ok::<(), astrs_data::TypeUrnError>(())
//! ```
//!
//! # Extension points
//!
//! Two, both deliberate:
//!
//! * **Namespaces.** `std` is the only one 0.1.0 accepts, and anything else is
//!   rejected with [`TypeUrnError::UnsupportedNamespace`] rather than silently
//!   admitted. Vendor namespaces open by extending the parser's namespace
//!   check; nothing downstream of it assumes `std`.
//! * **Layouts.** [`TypeRegistry`] is a mutable, cloneable table. Every
//!   `std/v1` type already maps onto a [`DataType`](crate::datatype::DataType) (scalars in
//!   [`registry`], the five compound categories in [`layouts`]), but the
//!   mechanism a future, append-only revision would use to stage a type
//!   before its layout is ready is exactly the one that got the compound
//!   categories here in the first place: register it with
//!   [`LayoutRule::Deferred`], then replace that entry with a
//!   [`LayoutRule::Fixed`] or a [`LayoutRule::Resolver`] once the layout is
//!   decided — no change to the grammar, the errors or the callers.

pub mod error;
pub mod layout;
pub mod layouts;
pub mod parse;
pub mod registry;

pub use crate::urn::error::TypeUrnError;
pub use crate::urn::layout::{
    accepts_layout, is_registered, layout_of, layout_of_str, urn_for_data_type, validate,
};
pub use crate::urn::parse::TypeUrn;
pub use crate::urn::registry::{
    CATEGORY_CORE, CATEGORY_GEOMETRY, CATEGORY_MEDIA, CATEGORY_NAV, CATEGORY_SENSOR, CATEGORY_TIME,
    CATEGORY_VISION, LayoutResolver, LayoutRule, STD_CORE_BOOL, STD_CORE_BYTES, STD_CORE_EMPTY,
    STD_CORE_FLOAT16, STD_CORE_FLOAT32, STD_CORE_FLOAT64, STD_CORE_INT8, STD_CORE_INT16,
    STD_CORE_INT32, STD_CORE_INT64, STD_CORE_STRING, STD_CORE_UINT8, STD_CORE_UINT16,
    STD_CORE_UINT32, STD_CORE_UINT64, STD_GEOMETRY_ACCEL, STD_GEOMETRY_POSE,
    STD_GEOMETRY_QUATERNION, STD_GEOMETRY_TRANSFORM, STD_GEOMETRY_TWIST, STD_GEOMETRY_VECTOR3,
    STD_MEDIA_AUDIO_FRAME, STD_MEDIA_COMPRESSED_IMAGE, STD_MEDIA_IMAGE, STD_NAV_OCCUPANCY_GRID,
    STD_NAV_ODOMETRY, STD_NAV_PATH, STD_SENSOR_IMU, STD_SENSOR_LASER_SCAN, STD_SENSOR_NAV_SAT_FIX,
    STD_SENSOR_POINT_CLOUD, STD_SENSOR_RANGE, STD_TIME_DURATION, STD_TIME_TIMESTAMP, STD_TYPE_URNS,
    STD_VERSION, STD_VISION_DETECTIONS, STD_VISION_KEYPOINTS, STD_VISION_MASK, TypeEntry,
    TypeRegistry, std_registry,
};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::DataType;

    #[test]
    fn the_three_questions_are_independent() {
        // 1. Syntax only.
        let malformed = TypeUrn::parse("std/core/V1/Bool").unwrap_err();
        assert!(malformed.is_syntax());

        // 2. Syntax fine, registry disagrees.
        let ghost = TypeUrn::parse("std/core/v1/Ghost").unwrap();
        assert!(validate(&ghost).unwrap_err().is_resolution());

        // 3. The staging error itself: registered, valid parameters, layout
        // not mapped yet. No `std/v1` type produces this any longer (every
        // one of them resolves — see `urn::layouts`), so it is demonstrated
        // directly on the error rather than through a registry lookup; the
        // registry-level mechanics are covered in `layout`'s own tests.
        let staged = TypeUrnError::LayoutUnavailable {
            urn: "std/future/v1/Ghost".to_owned(),
        };
        assert!(staged.is_layout_unavailable());
        assert!(!staged.is_syntax() && !staged.is_resolution());

        // ...and two real URNs that answer all three questions today: a
        // scalar and a compound type.
        let scalar = TypeUrn::parse("std/core/v1/Int64").unwrap();
        assert!(validate(&scalar).is_ok());
        assert_eq!(layout_of(&scalar), Ok(DataType::Int64));

        let pose = TypeUrn::parse("std/geometry/v1/Pose").unwrap();
        assert!(validate(&pose).is_ok());
        assert!(layout_of(&pose).is_ok_and(|data_type| data_type.is_nested()));
    }

    #[test]
    fn re_exports_cover_the_public_surface() {
        let registry: TypeRegistry = TypeRegistry::std();
        assert_eq!(registry.len(), STD_TYPE_URNS.len());
        assert!(std_registry().contains(&TypeUrn::parse(STD_CORE_BOOL).unwrap()));

        let entry: &TypeEntry = registry
            .entry(&TypeUrn::parse(STD_TIME_TIMESTAMP).unwrap())
            .unwrap();
        assert!(matches!(entry.layout_rule(), LayoutRule::Fixed(_)));

        let resolver: LayoutResolver = |_| Ok(DataType::Null);
        assert_eq!(
            resolver(&TypeUrn::parse(STD_CORE_EMPTY).unwrap()),
            Ok(DataType::Null)
        );

        assert!(is_registered(&TypeUrn::parse(STD_NAV_PATH).unwrap()));
        assert!(accepts_layout(
            &TypeUrn::parse(STD_CORE_STRING).unwrap(),
            &DataType::Utf8
        ));
        assert_eq!(layout_of_str(STD_CORE_BYTES), Ok(DataType::Binary));
        assert_eq!(
            urn_for_data_type(&DataType::Bool).map(|u| u.as_str().to_owned()),
            Some(STD_CORE_BOOL.to_owned())
        );
        assert_eq!(STD_VERSION, 1);
    }

    #[test]
    fn category_constants_are_the_ones_the_table_uses() {
        for (category, expected) in [
            (CATEGORY_CORE, 15),
            (CATEGORY_TIME, 2),
            (CATEGORY_MEDIA, 3),
            (CATEGORY_VISION, 3),
            (CATEGORY_GEOMETRY, 6),
            (CATEGORY_SENSOR, 5),
            (CATEGORY_NAV, 3),
        ] {
            assert_eq!(
                std_registry().category(category).count(),
                expected,
                "{category}"
            );
        }
    }

    #[test]
    fn every_named_constant_is_in_the_table() {
        for text in [
            STD_CORE_BOOL,
            STD_CORE_INT8,
            STD_CORE_INT16,
            STD_CORE_INT32,
            STD_CORE_INT64,
            STD_CORE_UINT8,
            STD_CORE_UINT16,
            STD_CORE_UINT32,
            STD_CORE_UINT64,
            STD_CORE_FLOAT16,
            STD_CORE_FLOAT32,
            STD_CORE_FLOAT64,
            STD_CORE_STRING,
            STD_CORE_BYTES,
            STD_CORE_EMPTY,
            STD_TIME_TIMESTAMP,
            STD_TIME_DURATION,
            STD_MEDIA_IMAGE,
            STD_MEDIA_AUDIO_FRAME,
            STD_MEDIA_COMPRESSED_IMAGE,
            STD_VISION_DETECTIONS,
            STD_VISION_KEYPOINTS,
            STD_VISION_MASK,
            STD_GEOMETRY_POSE,
            STD_GEOMETRY_TRANSFORM,
            STD_GEOMETRY_TWIST,
            STD_GEOMETRY_ACCEL,
            STD_GEOMETRY_QUATERNION,
            STD_GEOMETRY_VECTOR3,
            STD_SENSOR_LASER_SCAN,
            STD_SENSOR_POINT_CLOUD,
            STD_SENSOR_IMU,
            STD_SENSOR_NAV_SAT_FIX,
            STD_SENSOR_RANGE,
            STD_NAV_ODOMETRY,
            STD_NAV_PATH,
            STD_NAV_OCCUPANCY_GRID,
        ] {
            assert!(
                STD_TYPE_URNS.contains(&text),
                "{text} missing from the table"
            );
            assert!(is_registered(&TypeUrn::parse(text).unwrap()), "{text}");
        }
    }

    #[test]
    fn urns_flow_into_the_crate_error_type() {
        let err: crate::DataError = TypeUrn::parse("nope").unwrap_err().into();
        assert!(matches!(err, crate::DataError::TypeUrn(_)));
    }
}
