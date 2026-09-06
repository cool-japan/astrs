//! Normative columnar layouts for the `std` categories beyond `core`/`time`
//! (blueprint §24.3), expressed entirely in the closed [`DataType`](crate::datatype::DataType) set.
//!
//! Stage 1 gave `std/core/v1` and `std/time/v1` their layouts directly in
//! [`crate::urn::registry`] — eighteen `fn(): DataType` one-liners needed no
//! module of their own. The five categories here are compound: every layout
//! is a `Struct`/`List`/`FixedSizeList` composition, several nest another
//! type from this module (`Pose` is a `Vector3` plus a `Quaternion`;
//! `Odometry` is a `Pose` plus a `Twist`), and three compute their layout
//! from a URN parameter (`Image[pixel=…]`, `AudioFrame[sample=…]`,
//! `PointCloud[fields=…]`) — enough structure to earn a module per category.
//!
//! | Module | Category | Types |
//! |---|---|---|
//! | [`geometry`] | `std/geometry/v1` | `Vector3`, `Quaternion`, `Pose`, `Transform`, `Twist`, `Accel` |
//! | [`media`] | `std/media/v1` | `Image[pixel=…]`, `AudioFrame[sample=…]`, `CompressedImage[format=…]` |
//! | [`vision`] | `std/vision/v1` | `Detections`, `Keypoints`, `Mask` |
//! | [`sensor`] | `std/sensor/v1` | `LaserScan`, `PointCloud[fields=…]`, `Imu`, `NavSatFix`, `Range` |
//! | [`nav`] | `std/nav/v1` | `Odometry`, `Path`, `OccupancyGrid` |
//!
//! Each type has three things: a `..._layout() -> DataType` (or, for the
//! three parameterised types, `..._layout(param) -> Result<DataType,
//! TypeUrnError>`), a `..._schema()` that wraps it in the single-column
//! [`Schema`](crate::Schema) every AstRS payload uses, and — for the
//! parameterised three — a private `..._resolver` matching
//! [`LayoutResolver`](crate::urn::LayoutResolver), which is what
//! the crate-internal `std_rows` table wires into the registry. The registry
//! lookup itself does not live here: call [`crate::urn::layout_of`] or
//! [`crate::urn::TypeRegistry::layout`] as usual — this module is where the
//! answer comes from, not a second place to ask the question.
//!
//! ```
//! use astrs_data::urn::{layout_of, TypeUrn};
//! use astrs_data::urn::layouts::geometry::pose_layout;
//!
//! let urn = TypeUrn::parse("std/geometry/v1/Pose")?;
//! assert_eq!(layout_of(&urn), Ok(pose_layout()));
//! # Ok::<(), astrs_data::TypeUrnError>(())
//! ```

pub mod geometry;
pub mod media;
pub mod nav;
pub mod sensor;
pub mod vision;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use crate::datatype::DataType;
    use crate::urn::layout_of;
    use crate::urn::parse::TypeUrn;

    /// Every std URN this module ultimately backs resolves through the
    /// public [`layout_of`] entry point — not just through this module's own
    /// `..._layout()` functions — so the registry wiring itself is checked,
    /// not only the layout math.
    #[test]
    fn every_compound_type_resolves_through_the_registry() {
        let cases: &[(&str, DataType)] = &[
            ("std/geometry/v1/Vector3", super::geometry::vector3_layout()),
            (
                "std/geometry/v1/Quaternion",
                super::geometry::quaternion_layout(),
            ),
            ("std/geometry/v1/Pose", super::geometry::pose_layout()),
            (
                "std/geometry/v1/Transform",
                super::geometry::transform_layout(),
            ),
            ("std/geometry/v1/Twist", super::geometry::twist_layout()),
            ("std/geometry/v1/Accel", super::geometry::accel_layout()),
            (
                "std/vision/v1/Detections",
                super::vision::detections_layout(),
            ),
            ("std/vision/v1/Keypoints", super::vision::keypoints_layout()),
            ("std/vision/v1/Mask", super::vision::mask_layout()),
            (
                "std/sensor/v1/LaserScan",
                super::sensor::laser_scan_layout(),
            ),
            ("std/sensor/v1/Imu", super::sensor::imu_layout()),
            (
                "std/sensor/v1/NavSatFix",
                super::sensor::nav_sat_fix_layout(),
            ),
            ("std/sensor/v1/Range", super::sensor::range_layout()),
            ("std/nav/v1/Odometry", super::nav::odometry_layout()),
            ("std/nav/v1/Path", super::nav::path_layout()),
            (
                "std/nav/v1/OccupancyGrid",
                super::nav::occupancy_grid_layout(),
            ),
            (
                "std/media/v1/CompressedImage[format=jpeg]",
                super::media::compressed_image_layout(),
            ),
        ];
        for (text, expected) in cases {
            let urn = TypeUrn::parse(text).unwrap();
            assert_eq!(layout_of(&urn).as_ref(), Ok(expected), "{text}");
        }

        assert_eq!(
            layout_of(&TypeUrn::parse("std/media/v1/Image[pixel=rgb8]").unwrap()),
            super::media::image_layout("rgb8")
        );
        assert_eq!(
            layout_of(&TypeUrn::parse("std/media/v1/AudioFrame[sample=s16]").unwrap()),
            super::media::audio_frame_layout("s16")
        );
        assert_eq!(
            layout_of(&TypeUrn::parse("std/sensor/v1/PointCloud[fields=x:y:z]").unwrap()),
            super::sensor::point_cloud_layout("x:y:z")
        );
    }

    #[test]
    fn every_compound_layout_is_expressible_in_the_closed_type_set() {
        // Every layout in this module is Struct/List/FixedSizeList over the
        // primitive set — no Dictionary/Union/Map, which the closed P0 set
        // (blueprint §6.1) does not have variants for in the first place, so
        // this is really asserting that the types below compile and nest.
        fn assert_closed(data_type: &DataType) {
            match data_type {
                DataType::Struct(fields) => {
                    for field in fields {
                        assert_closed(field.data_type());
                    }
                }
                DataType::List(field) | DataType::FixedSizeList(field, _) => {
                    assert_closed(field.data_type());
                }
                _ => {}
            }
        }

        for data_type in [
            super::geometry::pose_layout(),
            super::sensor::imu_layout(),
            super::nav::odometry_layout(),
            super::vision::detections_layout(),
            super::media::image_layout("rgba32f").unwrap(),
            super::sensor::point_cloud_layout("x:y:z:intensity").unwrap(),
        ] {
            assert_closed(&data_type);
        }
    }
}
