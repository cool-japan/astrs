//! The normative columnar layout of the `std` type set.
//!
//! Blueprint §24.3 requires every `std` type to name a columnar layout. This
//! module is where a [`TypeUrn`] becomes a [`DataType`], through the process
//! [`std_registry`], and where the mapping is written down.
//!
//! # `std/core/v1` — scalar channels
//!
//! | URN | [`DataType`] | Notes |
//! |---|---|---|
//! | `Bool` | `Bool` | one bit per row, LSB-numbered |
//! | `Int8` … `Int64` | `Int8` … `Int64` | two's complement, little-endian |
//! | `UInt8` … `UInt64` | `UInt8` … `UInt64` | little-endian |
//! | `Float16` | `Float16` | IEEE 754 binary16, see [`F16`](crate::F16) |
//! | `Float32` | `Float32` | IEEE 754 binary32 |
//! | `Float64` | `Float64` | IEEE 754 binary64 |
//! | `String` | `Utf8` | 32-bit offsets; use a `Large` column only by explicit schema |
//! | `Bytes` | `Binary` | 32-bit offsets |
//! | `Empty` | `Null` | no buffers at all — the shape of a pure trigger |
//!
//! # `std/time/v1`
//!
//! | URN | [`DataType`] | Notes |
//! |---|---|---|
//! | `Timestamp` | `Timestamp` | nanoseconds since the Unix epoch, timezone-less |
//! | `Duration` | `Duration` | a nanosecond count |
//!
//! Both are `i64` columns; the distinct [`DataType`] variants exist so that a
//! stamp is never silently added to an interval.
//!
//! # The other five categories
//!
//! `media`, `vision`, `geometry`, `sensor` and `nav` carry a normative layout
//! too, defined in [`crate::urn::layouts`] and expressed entirely in the
//! closed [`DataType`] set — `Struct`/`List`/`FixedSizeList` compositions
//! over the same scalars `core` uses (blueprint §24.3). `Image`,
//! `AudioFrame` and `PointCloud` compute their layout from a parameter
//! (`pixel`, `sample`, `fields`); the rest are fixed regardless of their
//! accepted parameters — `frame` and friends are graph-level metadata, not
//! shape.
//!
//! [`TypeUrnError::LayoutUnavailable`] is still a real, tested outcome of
//! [`LayoutRule::Deferred`](crate::urn::LayoutRule::Deferred) — nothing about
//! it was removed, it just has no `std/v1` occupant any longer. It stays
//! available for whatever a future, append-only revision registers before
//! its layout is ready (blueprint §3.4); see `urn::registry`'s tests for it
//! exercised directly.
//!
//! ```
//! use astrs_data::urn::{layout_of, TypeUrn};
//! use astrs_data::DataType;
//!
//! assert_eq!(layout_of(&TypeUrn::parse("std/core/v1/Float32")?), Ok(DataType::Float32));
//! assert!(layout_of(&TypeUrn::parse("std/geometry/v1/Pose")?)?.is_nested());
//! # Ok::<(), astrs_data::TypeUrnError>(())
//! ```

use crate::datatype::DataType;
use crate::urn::error::TypeUrnError;
use crate::urn::parse::TypeUrn;
use crate::urn::registry::std_registry;

/// The columnar layout `urn` maps onto, resolved against the process
/// [`std_registry`].
///
/// # Errors
///
/// * [`TypeUrnError::UnknownType`] — nothing is registered under the URN.
/// * [`TypeUrnError::MissingParameter`] / [`TypeUrnError::UnknownParameter`] —
///   the parameters do not satisfy the type's contract.
/// * [`TypeUrnError::LayoutUnavailable`] — the type is known but this build
///   does not map its layout.
///
/// ```
/// use astrs_data::urn::{layout_of, TypeUrn};
/// use astrs_data::DataType;
///
/// assert_eq!(layout_of(&TypeUrn::parse("std/core/v1/Bytes")?), Ok(DataType::Binary));
/// assert_eq!(layout_of(&TypeUrn::parse("std/time/v1/Duration")?), Ok(DataType::Duration));
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
pub fn layout_of(urn: &TypeUrn) -> Result<DataType, TypeUrnError> {
    std_registry().layout(urn)
}

/// Parses `text` and resolves its layout in one step.
///
/// The shape a manifest loader wants: it holds a string, it wants a type.
///
/// # Errors
///
/// A syntax variant from the parser, or whatever [`layout_of`] reports.
///
/// ```
/// use astrs_data::urn::layout_of_str;
/// use astrs_data::DataType;
///
/// assert_eq!(layout_of_str("std/core/v1/UInt16"), Ok(DataType::UInt16));
/// assert!(layout_of_str("not a urn").is_err());
/// ```
pub fn layout_of_str(text: &str) -> Result<DataType, TypeUrnError> {
    layout_of(&TypeUrn::parse(text)?)
}

/// Checks that `urn` names a registered type and satisfies its parameter
/// contract, without asking for a layout.
///
/// This is the check `astrs validate` runs on every port: an edge between two
/// `std/nav/v1/Path` ports is valid even though neither side can name a
/// [`DataType`] for it yet.
///
/// # Errors
///
/// [`TypeUrnError::UnknownType`], [`TypeUrnError::UnknownParameter`] or
/// [`TypeUrnError::MissingParameter`].
///
/// ```
/// use astrs_data::urn::{validate, TypeUrn};
///
/// assert!(validate(&TypeUrn::parse("std/nav/v1/Path")?).is_ok());
/// assert!(validate(&TypeUrn::parse("std/nav/v1/Nope")?).is_err());
/// assert!(validate(&TypeUrn::parse("std/media/v1/Image")?).is_err());
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
pub fn validate(urn: &TypeUrn) -> Result<(), TypeUrnError> {
    std_registry().validate(urn)
}

/// Returns `true` when the process registry knows `urn`'s type.
///
/// ```
/// use astrs_data::urn::{is_registered, TypeUrn};
///
/// assert!(is_registered(&TypeUrn::parse("std/sensor/v1/Imu")?));
/// assert!(!is_registered(&TypeUrn::parse("std/sensor/v1/Sonar")?));
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
#[must_use]
pub fn is_registered(urn: &TypeUrn) -> bool {
    std_registry().contains(urn)
}

/// The `std` URN whose fixed layout is exactly `data_type`, if there is one.
///
/// The inverse of [`layout_of`] over the scalar set. Used when a decoder has
/// a concrete column and needs a URN to stamp on the port it arrived at.
///
/// `LargeBinary`, `LargeUtf8` and every nested type return `None`: the `std`
/// set has no scalar name for them, and inventing one here would let two
/// producers disagree about what `std/core/v1/Bytes` means.
///
/// ```
/// use astrs_data::urn::urn_for_data_type;
/// use astrs_data::{DataType, Field};
///
/// assert_eq!(
///     urn_for_data_type(&DataType::Timestamp).map(|u| u.as_str().to_owned()),
///     Some("std/time/v1/Timestamp".to_owned())
/// );
/// assert!(urn_for_data_type(&DataType::LargeBinary).is_none());
/// assert!(urn_for_data_type(&DataType::list(Field::nullable("item", DataType::Int8))).is_none());
/// ```
#[must_use]
pub fn urn_for_data_type(data_type: &DataType) -> Option<TypeUrn> {
    std_registry().urn_for_data_type(data_type).cloned()
}

/// Returns `true` when a column of `actual` can be delivered to a port
/// declared as `urn`.
///
/// Layout compatibility, not type identity: producer and consumer must agree
/// on bytes, not on labels (see [`DataType::layout_eq`]). A URN whose layout
/// this build cannot resolve — currently only possible for a
/// [`LayoutRule::Deferred`](crate::urn::LayoutRule::Deferred) entry a future
/// revision registers before its layout lands, since every `std/v1` type
/// resolves today — is treated as *compatible with anything*, since refusing
/// traffic over a staging gap would be worse than letting it through: the
/// graph type-checker has already established that both ends name the same
/// URN.
///
/// ```
/// use astrs_data::urn::{accepts_layout, TypeUrn};
/// use astrs_data::urn::layouts::geometry::pose_layout;
/// use astrs_data::{DataType, Field};
///
/// let urn = TypeUrn::parse("std/core/v1/Float32")?;
/// assert!(accepts_layout(&urn, &DataType::Float32));
/// assert!(!accepts_layout(&urn, &DataType::Float64));
///
/// // Nested layouts compare structurally, including across categories.
/// let pose = TypeUrn::parse("std/geometry/v1/Pose")?;
/// assert!(accepts_layout(&pose, &pose_layout()));
/// assert!(!accepts_layout(&pose, &DataType::Float64));
///
/// let list = DataType::list(Field::new("item", DataType::Int32, true));
/// let renamed = DataType::list(Field::new("element", DataType::Int32, false));
/// assert!(DataType::layout_eq(&list, &renamed));
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
#[must_use]
pub fn accepts_layout(urn: &TypeUrn, actual: &DataType) -> bool {
    match layout_of(urn) {
        Ok(expected) => expected.layout_eq(actual),
        Err(err) if err.is_layout_unavailable() => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::datatype::Field;
    use crate::urn::registry::{CATEGORY_CORE, CATEGORY_TIME, STD_TYPE_URNS};

    fn urn(text: &str) -> TypeUrn {
        TypeUrn::parse(text).expect("a valid URN")
    }

    #[test]
    fn core_scalars_resolve() {
        assert_eq!(layout_of(&urn("std/core/v1/Bool")), Ok(DataType::Bool));
        assert_eq!(layout_of(&urn("std/core/v1/Int8")), Ok(DataType::Int8));
        assert_eq!(layout_of(&urn("std/core/v1/Int16")), Ok(DataType::Int16));
        assert_eq!(layout_of(&urn("std/core/v1/Int32")), Ok(DataType::Int32));
        assert_eq!(layout_of(&urn("std/core/v1/Int64")), Ok(DataType::Int64));
        assert_eq!(layout_of(&urn("std/core/v1/UInt8")), Ok(DataType::UInt8));
        assert_eq!(layout_of(&urn("std/core/v1/UInt16")), Ok(DataType::UInt16));
        assert_eq!(layout_of(&urn("std/core/v1/UInt32")), Ok(DataType::UInt32));
        assert_eq!(layout_of(&urn("std/core/v1/UInt64")), Ok(DataType::UInt64));
        assert_eq!(
            layout_of(&urn("std/core/v1/Float16")),
            Ok(DataType::Float16)
        );
        assert_eq!(
            layout_of(&urn("std/core/v1/Float32")),
            Ok(DataType::Float32)
        );
        assert_eq!(
            layout_of(&urn("std/core/v1/Float64")),
            Ok(DataType::Float64)
        );
        assert_eq!(layout_of(&urn("std/core/v1/String")), Ok(DataType::Utf8));
        assert_eq!(layout_of(&urn("std/core/v1/Bytes")), Ok(DataType::Binary));
        assert_eq!(layout_of(&urn("std/core/v1/Empty")), Ok(DataType::Null));
    }

    #[test]
    fn time_types_are_distinct() {
        assert_eq!(
            layout_of(&urn("std/time/v1/Timestamp")),
            Ok(DataType::Timestamp)
        );
        assert_eq!(
            layout_of(&urn("std/time/v1/Duration")),
            Ok(DataType::Duration)
        );
        assert_ne!(DataType::Timestamp, DataType::Duration);
    }

    #[test]
    fn every_std_urn_resolves_with_valid_parameters() {
        // Every `std/v1` type maps onto a layout in this build (registry.rs,
        // urn::layouts). A type whose required parameter is value-sensitive
        // needs a value its resolver actually accepts; a type whose
        // parameters are graph metadata (`frame`, …) accepts a placeholder.
        fn valid_value(key: &str) -> &'static str {
            match key {
                "pixel" => "rgb8",
                "sample" => "s16",
                "fields" => "x:y:z",
                "format" => "jpeg",
                _ => "x",
            }
        }

        for text in STD_TYPE_URNS {
            let mut candidate = urn(text);
            let entry = std_registry().entry(&candidate).expect("registered");
            for key in entry.required_params() {
                candidate = candidate.with_param(key, valid_value(key)).unwrap();
            }
            let data_type = layout_of(&candidate).unwrap_or_else(|err| panic!("{text}: {err}"));
            let is_scalar = matches!(candidate.category(), CATEGORY_CORE | CATEGORY_TIME);
            assert_eq!(
                !data_type.is_nested(),
                is_scalar,
                "{text} should be a scalar iff it is core/time"
            );
        }
    }

    #[test]
    fn string_helper_parses_then_resolves() {
        assert_eq!(layout_of_str("std/core/v1/UInt16"), Ok(DataType::UInt16));
        assert!(layout_of_str("nope").unwrap_err().is_syntax());
        assert!(
            layout_of_str("std/core/v1/Nope")
                .unwrap_err()
                .is_resolution()
        );
    }

    #[test]
    fn validation_is_independent_of_layout_availability() {
        assert!(validate(&urn("std/nav/v1/Path")).is_ok());
        assert!(validate(&urn("std/nav/v1/Path[frame=map]")).is_ok());
        assert!(validate(&urn("std/nav/v1/Path[bogus=1]")).is_err());
        assert!(validate(&urn("std/nav/v1/Ghost")).is_err());
        assert!(validate(&urn("std/media/v1/Image")).is_err());
        assert!(validate(&urn("std/media/v1/Image[pixel=rgb8]")).is_ok());
    }

    #[test]
    fn registration_lookup_is_cheap_and_correct() {
        assert!(is_registered(&urn("std/sensor/v1/Imu")));
        assert!(is_registered(&urn(
            "std/sensor/v1/PointCloud[fields=x:y:z]"
        )));
        assert!(!is_registered(&urn("std/sensor/v1/Sonar")));
        assert!(!is_registered(&urn("std/core/v2/Bool")));
    }

    #[test]
    fn reverse_lookup_is_the_inverse_over_the_scalar_set() {
        for text in STD_TYPE_URNS {
            let candidate = urn(text);
            let Ok(data_type) = layout_of(&candidate) else {
                continue;
            };
            if data_type.is_nested() {
                // Compound layouts have no unique inverse (Twist/Accel are
                // byte-identical) — `urn_for_data_type` does not try.
                assert!(urn_for_data_type(&data_type).is_none(), "{text}");
                continue;
            }
            let back = urn_for_data_type(&data_type).expect("a scalar type maps back");
            assert_eq!(back.as_str(), *text, "{text} did not round-trip");
        }
    }

    #[test]
    fn reverse_lookup_declines_types_with_no_std_name() {
        assert!(urn_for_data_type(&DataType::LargeUtf8).is_none());
        assert!(urn_for_data_type(&DataType::LargeBinary).is_none());
        assert!(urn_for_data_type(&DataType::FixedSizeBinary(4)).is_none());
        assert!(urn_for_data_type(&DataType::strukt([])).is_none());
        assert!(
            urn_for_data_type(&DataType::list(Field::nullable("item", DataType::Int8))).is_none()
        );
    }

    #[test]
    fn layout_acceptance_compares_bytes() {
        let float = urn("std/core/v1/Float32");
        assert!(accepts_layout(&float, &DataType::Float32));
        assert!(!accepts_layout(&float, &DataType::Float64));
        assert!(!accepts_layout(&float, &DataType::Int32));

        let text = urn("std/core/v1/String");
        assert!(accepts_layout(&text, &DataType::Utf8));
        assert!(!accepts_layout(&text, &DataType::LargeUtf8));
    }

    // `accepts_layout`'s permissive branch for `LayoutUnavailable` (see its
    // doc) has no std/v1 URN left to drive it through a real lookup — every
    // one resolves now. `urn::registry`'s
    // `a_deferred_entry_reports_layout_unavailable_not_unknown` exercises the
    // same error at the `TypeRegistry` level instead.

    #[test]
    fn unknown_types_block_delivery() {
        let ghost = urn("std/core/v1/Ghost");
        assert!(!accepts_layout(&ghost, &DataType::Float64));

        // A parameter contract failure blocks it too.
        let image = urn("std/media/v1/Image");
        assert!(!accepts_layout(&image, &DataType::UInt8));
    }

    #[test]
    fn nested_layouts_compare_structurally() {
        // A registry entry with a nested fixed layout accepts a child field
        // that only differs by label.
        use crate::urn::registry::{LayoutRule, TypeEntry, TypeRegistry};

        let mut registry = TypeRegistry::new();
        let vector = urn("std/geometry/v1/Vector3");
        let declared = DataType::fixed_size_list(Field::new("item", DataType::Float64, false), 3);
        registry.insert(TypeEntry::new(vector.clone(), LayoutRule::Fixed(declared)));

        let actual = DataType::fixed_size_list(Field::new("element", DataType::Float64, true), 3);
        let resolved = registry.layout(&vector).unwrap();
        assert!(resolved.layout_eq(&actual));
        assert_ne!(resolved, actual);
    }
}
