//! [`TypeRegistry`] — what a [`TypeUrn`] means.
//!
//! Parsing a URN proves it is *well formed*. The registry decides whether it
//! is *known*, which parameters it accepts, and what columnar layout it maps
//! onto. Those are three different questions and they fail with three
//! different error groups (see [`TypeUrnError`]).
//!
//! # The initial `std` set (blueprint §24.3)
//!
//! | Category | Types |
//! |---|---|
//! | `core` | `Bool`, `Int8..64`, `UInt8..64`, `Float16/32/64`, `String`, `Bytes`, `Empty` |
//! | `time` | `Timestamp`, `Duration` |
//! | `media` | `Image[pixel=…]`, `AudioFrame[sample=…]`, `CompressedImage[format=…]` |
//! | `vision` | `Detections`, `Keypoints`, `Mask` |
//! | `geometry` | `Pose`, `Transform`, `Twist`, `Accel`, `Quaternion`, `Vector3` |
//! | `sensor` | `LaserScan`, `PointCloud[fields=…]`, `Imu`, `NavSatFix`, `Range` |
//! | `nav` | `Odometry`, `Path`, `OccupancyGrid` |
//!
//! `core` and `time` carry their normative columnar layout directly in this
//! file (every one of them is a bare scalar `DataType`, so a one-line
//! [`LayoutRule::Fixed`] per row is all there is to it). The other five
//! categories are compound — `Struct`/`List`/`FixedSizeList` compositions,
//! several nesting another type in the set — and get a module each under
//! [`crate::urn::layouts`]; three of them (`Image`, `AudioFrame`,
//! `PointCloud`) compute their layout from a parameter via
//! [`LayoutRule::Resolver`], the rest are [`LayoutRule::Fixed`] like the
//! scalars. Every `std/v1` type therefore resolves in this build —
//! [`TypeUrnError::LayoutUnavailable`] stays real and tested
//! ([`LayoutRule::Deferred`]'s own tests in this module's `#[cfg(test)]`
//! block exercise it directly), ready for whatever a future, append-only
//! revision registers before its layout lands, but no current type
//! produces it.
//!
//! # Extending it
//!
//! ```
//! use astrs_data::urn::{LayoutRule, TypeEntry, TypeRegistry, TypeUrn};
//! use astrs_data::DataType;
//!
//! let mut registry = TypeRegistry::std();
//! let urn = TypeUrn::parse("std/geometry/v1/Vector3")?;
//!
//! // `insert` always overwrites, so a caller can replace even an already
//! // laid-out entry with its own layout — here, a flattened alternative.
//! registry.insert(
//!     TypeEntry::new(urn.clone(), LayoutRule::Fixed(DataType::Float64))
//!         .with_summary("a 3-vector, flattened"),
//! );
//! assert_eq!(registry.layout(&urn), Ok(DataType::Float64));
//! # Ok::<(), astrs_data::TypeUrnError>(())
//! ```

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::datatype::DataType;
use crate::urn::error::TypeUrnError;
use crate::urn::layouts::{geometry, media, nav, sensor, vision};
use crate::urn::parse::TypeUrn;

// ---------------------------------------------------------------------------
// Canonical URN constants — blueprint §24.3.
// ---------------------------------------------------------------------------

/// The `core` category: scalar channels.
pub const CATEGORY_CORE: &str = "core";
/// The `time` category: stamps and intervals.
pub const CATEGORY_TIME: &str = "time";
/// The `media` category: images and audio.
pub const CATEGORY_MEDIA: &str = "media";
/// The `vision` category: detector outputs.
pub const CATEGORY_VISION: &str = "vision";
/// The `geometry` category: rigid-body quantities.
pub const CATEGORY_GEOMETRY: &str = "geometry";
/// The `sensor` category: raw sensor frames.
pub const CATEGORY_SENSOR: &str = "sensor";
/// The `nav` category: navigation products.
pub const CATEGORY_NAV: &str = "nav";

/// The version every type in the initial `std` set carries.
pub const STD_VERSION: u16 = 1;

/// `std/core/v1/Bool` — one boolean per row.
pub const STD_CORE_BOOL: &str = "std/core/v1/Bool";
/// `std/core/v1/Int8`.
pub const STD_CORE_INT8: &str = "std/core/v1/Int8";
/// `std/core/v1/Int16`.
pub const STD_CORE_INT16: &str = "std/core/v1/Int16";
/// `std/core/v1/Int32`.
pub const STD_CORE_INT32: &str = "std/core/v1/Int32";
/// `std/core/v1/Int64`.
pub const STD_CORE_INT64: &str = "std/core/v1/Int64";
/// `std/core/v1/UInt8`.
pub const STD_CORE_UINT8: &str = "std/core/v1/UInt8";
/// `std/core/v1/UInt16`.
pub const STD_CORE_UINT16: &str = "std/core/v1/UInt16";
/// `std/core/v1/UInt32`.
pub const STD_CORE_UINT32: &str = "std/core/v1/UInt32";
/// `std/core/v1/UInt64`.
pub const STD_CORE_UINT64: &str = "std/core/v1/UInt64";
/// `std/core/v1/Float16`.
pub const STD_CORE_FLOAT16: &str = "std/core/v1/Float16";
/// `std/core/v1/Float32`.
pub const STD_CORE_FLOAT32: &str = "std/core/v1/Float32";
/// `std/core/v1/Float64`.
pub const STD_CORE_FLOAT64: &str = "std/core/v1/Float64";
/// `std/core/v1/String` — UTF-8 text, 32-bit offsets.
pub const STD_CORE_STRING: &str = "std/core/v1/String";
/// `std/core/v1/Bytes` — opaque bytes, 32-bit offsets.
pub const STD_CORE_BYTES: &str = "std/core/v1/Bytes";
/// `std/core/v1/Empty` — a signal with no payload.
pub const STD_CORE_EMPTY: &str = "std/core/v1/Empty";

/// `std/time/v1/Timestamp` — nanoseconds since the Unix epoch.
pub const STD_TIME_TIMESTAMP: &str = "std/time/v1/Timestamp";
/// `std/time/v1/Duration` — a nanosecond interval.
pub const STD_TIME_DURATION: &str = "std/time/v1/Duration";

/// `std/media/v1/Image` — a raw frame; requires `pixel`.
pub const STD_MEDIA_IMAGE: &str = "std/media/v1/Image";
/// `std/media/v1/AudioFrame` — PCM samples; requires `sample`.
pub const STD_MEDIA_AUDIO_FRAME: &str = "std/media/v1/AudioFrame";
/// `std/media/v1/CompressedImage` — an encoded frame; requires `format`.
pub const STD_MEDIA_COMPRESSED_IMAGE: &str = "std/media/v1/CompressedImage";

/// `std/vision/v1/Detections` — boxes with scores and labels.
pub const STD_VISION_DETECTIONS: &str = "std/vision/v1/Detections";
/// `std/vision/v1/Keypoints` — landmark sets.
pub const STD_VISION_KEYPOINTS: &str = "std/vision/v1/Keypoints";
/// `std/vision/v1/Mask` — a per-pixel label plane.
pub const STD_VISION_MASK: &str = "std/vision/v1/Mask";

/// `std/geometry/v1/Pose` — position plus orientation.
pub const STD_GEOMETRY_POSE: &str = "std/geometry/v1/Pose";
/// `std/geometry/v1/Transform` — a frame-to-frame rigid transform.
pub const STD_GEOMETRY_TRANSFORM: &str = "std/geometry/v1/Transform";
/// `std/geometry/v1/Twist` — linear plus angular velocity.
pub const STD_GEOMETRY_TWIST: &str = "std/geometry/v1/Twist";
/// `std/geometry/v1/Accel` — linear plus angular acceleration.
pub const STD_GEOMETRY_ACCEL: &str = "std/geometry/v1/Accel";
/// `std/geometry/v1/Quaternion` — an orientation, `xyzw`.
pub const STD_GEOMETRY_QUATERNION: &str = "std/geometry/v1/Quaternion";
/// `std/geometry/v1/Vector3` — a 3-vector.
pub const STD_GEOMETRY_VECTOR3: &str = "std/geometry/v1/Vector3";

/// `std/sensor/v1/LaserScan` — a planar range sweep.
pub const STD_SENSOR_LASER_SCAN: &str = "std/sensor/v1/LaserScan";
/// `std/sensor/v1/PointCloud` — a point set; requires `fields`.
pub const STD_SENSOR_POINT_CLOUD: &str = "std/sensor/v1/PointCloud";
/// `std/sensor/v1/Imu` — angular rate, acceleration and orientation.
pub const STD_SENSOR_IMU: &str = "std/sensor/v1/Imu";
/// `std/sensor/v1/NavSatFix` — a GNSS fix.
pub const STD_SENSOR_NAV_SAT_FIX: &str = "std/sensor/v1/NavSatFix";
/// `std/sensor/v1/Range` — a single-beam distance reading.
pub const STD_SENSOR_RANGE: &str = "std/sensor/v1/Range";

/// `std/nav/v1/Odometry` — pose and twist with covariance.
pub const STD_NAV_ODOMETRY: &str = "std/nav/v1/Odometry";
/// `std/nav/v1/Path` — an ordered pose sequence.
pub const STD_NAV_PATH: &str = "std/nav/v1/Path";
/// `std/nav/v1/OccupancyGrid` — a 2-D costmap.
pub const STD_NAV_OCCUPANCY_GRID: &str = "std/nav/v1/OccupancyGrid";

/// Every URN in the initial `std` registry, in registration order.
///
/// ```
/// use astrs_data::urn::{STD_TYPE_URNS, TypeUrn};
///
/// assert!(STD_TYPE_URNS.iter().all(|text| TypeUrn::parse(text).is_ok()));
/// assert_eq!(STD_TYPE_URNS.len(), 37);
/// ```
pub const STD_TYPE_URNS: &[&str] = &[
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
];

// ---------------------------------------------------------------------------
// Layout rules
// ---------------------------------------------------------------------------

/// Computes a layout from a URN's parameters.
///
/// A plain `fn` pointer rather than a boxed closure, so a [`TypeRegistry`]
/// stays `Clone + Send + Sync` and can live in a `static`.
pub type LayoutResolver = fn(&TypeUrn) -> Result<DataType, TypeUrnError>;

/// How a registered type maps onto the columnar layout.
#[derive(Clone)]
#[non_exhaustive]
pub enum LayoutRule {
    /// One layout, whatever the parameters say.
    Fixed(DataType),
    /// A layout computed from the parameters.
    Resolver(LayoutResolver),
    /// The type is known and its parameters are checked, but its normative
    /// columnar layout is not mapped in this build.
    ///
    /// Resolving one yields [`TypeUrnError::LayoutUnavailable`], never
    /// [`TypeUrnError::UnknownType`] — the caller can tell a staging gap from
    /// a typo.
    Deferred,
}

impl std::fmt::Debug for LayoutRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fixed(data_type) => write!(f, "Fixed({data_type})"),
            Self::Resolver(_) => f.write_str("Resolver(..)"),
            Self::Deferred => f.write_str("Deferred"),
        }
    }
}

impl LayoutRule {
    /// Returns `true` when the rule can produce a layout in this build.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        !matches!(self, Self::Deferred)
    }
}

// ---------------------------------------------------------------------------
// Registry entries
// ---------------------------------------------------------------------------

/// One registered type: its base URN, its parameter contract and its layout.
///
/// ```
/// use astrs_data::urn::{LayoutRule, TypeEntry, TypeUrn};
/// use astrs_data::DataType;
///
/// let entry = TypeEntry::new(
///     TypeUrn::parse("std/core/v1/Int32")?,
///     LayoutRule::Fixed(DataType::Int32),
/// )
/// .with_summary("a 32-bit signed integer channel");
///
/// assert_eq!(entry.urn().as_str(), "std/core/v1/Int32");
/// assert!(entry.accepted_params().is_empty());
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
#[derive(Debug, Clone)]
pub struct TypeEntry {
    /// The base URN — parameters stripped, since parameters vary per use.
    urn: TypeUrn,
    /// The layout rule.
    layout: LayoutRule,
    /// Parameter keys this type understands.
    accepted_params: Vec<Box<str>>,
    /// Parameter keys this type cannot do without.
    required_params: Vec<Box<str>>,
    /// A one-line human description, used by `astrs types list`.
    summary: Box<str>,
}

impl TypeEntry {
    /// Registers `urn` (with any parameters stripped) under `layout`.
    #[must_use]
    pub fn new(urn: TypeUrn, layout: LayoutRule) -> Self {
        Self {
            urn: urn.base(),
            layout,
            accepted_params: Vec::new(),
            required_params: Vec::new(),
            summary: Box::from(""),
        }
    }

    /// Declares the parameter keys this type understands.
    ///
    /// Required keys are added to the accepted set automatically, so the two
    /// builders can be called in either order.
    #[must_use]
    pub fn with_accepted_params<S: AsRef<str>>(
        mut self,
        keys: impl IntoIterator<Item = S>,
    ) -> Self {
        for key in keys {
            let key: Box<str> = Box::from(key.as_ref());
            if !self.accepted_params.contains(&key) {
                self.accepted_params.push(key);
            }
        }
        self.accepted_params.sort_unstable();
        self
    }

    /// Declares the parameter keys this type cannot do without.
    #[must_use]
    pub fn with_required_params<S: AsRef<str>>(
        mut self,
        keys: impl IntoIterator<Item = S>,
    ) -> Self {
        let keys: Vec<Box<str>> = keys.into_iter().map(|k| Box::from(k.as_ref())).collect();
        for key in &keys {
            if !self.required_params.contains(key) {
                self.required_params.push(key.clone());
            }
        }
        self.required_params.sort_unstable();
        self.with_accepted_params(keys.iter().map(|k| &**k))
    }

    /// Attaches the one-line description.
    #[must_use]
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into().into_boxed_str();
        self
    }

    /// The base URN this entry is keyed by.
    #[inline]
    #[must_use]
    pub const fn urn(&self) -> &TypeUrn {
        &self.urn
    }

    /// The layout rule.
    #[inline]
    #[must_use]
    pub const fn layout_rule(&self) -> &LayoutRule {
        &self.layout
    }

    /// The parameter keys this type understands, sorted.
    #[inline]
    #[must_use]
    pub fn accepted_params(&self) -> &[Box<str>] {
        &self.accepted_params
    }

    /// The parameter keys this type requires, sorted.
    #[inline]
    #[must_use]
    pub fn required_params(&self) -> &[Box<str>] {
        &self.required_params
    }

    /// The one-line description, or `""`.
    #[inline]
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Checks a concrete URN's parameters against this entry's contract.
    ///
    /// # Errors
    ///
    /// [`TypeUrnError::UnknownParameter`] for a key the type does not accept,
    /// [`TypeUrnError::MissingParameter`] for a required key that is absent.
    ///
    /// ```
    /// use astrs_data::urn::{TypeRegistry, TypeUrn};
    ///
    /// let registry = TypeRegistry::std();
    /// let entry = registry.entry(&TypeUrn::parse("std/media/v1/Image")?).ok_or("missing")?;
    ///
    /// assert!(entry.validate(&TypeUrn::parse("std/media/v1/Image[pixel=rgb8]")?).is_ok());
    /// assert!(entry.validate(&TypeUrn::parse("std/media/v1/Image")?).is_err());
    /// assert!(entry.validate(&TypeUrn::parse("std/media/v1/Image[pixel=rgb8,bogus=1]")?).is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn validate(&self, urn: &TypeUrn) -> Result<(), TypeUrnError> {
        for key in urn.params().keys() {
            if !self.accepted_params.contains(key) {
                return Err(TypeUrnError::UnknownParameter {
                    urn: self.urn.as_str().to_owned(),
                    key: key.to_string(),
                });
            }
        }
        for key in &self.required_params {
            if !urn.params().contains_key(key) {
                return Err(TypeUrnError::MissingParameter {
                    urn: self.urn.as_str().to_owned(),
                    key: key.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Validates `urn` and resolves its columnar layout.
    ///
    /// # Errors
    ///
    /// Whatever [`TypeEntry::validate`] reports, or
    /// [`TypeUrnError::LayoutUnavailable`] for a [`LayoutRule::Deferred`]
    /// entry, or whatever a [`LayoutRule::Resolver`] reports.
    pub fn layout(&self, urn: &TypeUrn) -> Result<DataType, TypeUrnError> {
        self.validate(urn)?;
        match &self.layout {
            LayoutRule::Fixed(data_type) => Ok(data_type.clone()),
            LayoutRule::Resolver(resolve) => resolve(urn),
            LayoutRule::Deferred => Err(TypeUrnError::LayoutUnavailable {
                urn: self.urn.as_str().to_owned(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// A set of [`TypeEntry`] values, keyed by base URN.
///
/// Ordered, so iteration and `astrs types list` are deterministic.
#[derive(Debug, Clone, Default)]
pub struct TypeRegistry {
    entries: BTreeMap<Box<str>, TypeEntry>,
}

impl TypeRegistry {
    /// An empty registry.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The initial `std` table (blueprint §24.3).
    ///
    /// ```
    /// use astrs_data::urn::{TypeRegistry, TypeUrn, STD_TYPE_URNS};
    ///
    /// let registry = TypeRegistry::std();
    /// assert_eq!(registry.len(), STD_TYPE_URNS.len());
    /// assert!(registry.contains(&TypeUrn::parse("std/core/v1/Float64")?));
    /// assert!(!registry.contains(&TypeUrn::parse("std/core/v1/Complex")?));
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    #[must_use]
    pub fn std() -> Self {
        let mut registry = Self::new();
        for row in std_rows() {
            // A malformed row is a bug in this file, caught by
            // `std_table_is_wellformed`. Skipping it keeps the constructor
            // infallible and total instead of poisoning every caller.
            if let Ok(entry) = row.into_entry() {
                registry.insert(entry);
            }
        }
        registry
    }

    /// The initial `std` table, reporting a malformed row instead of skipping
    /// it.
    ///
    /// # Errors
    ///
    /// The first [`TypeUrnError`] a row's URN produces.
    pub fn try_std() -> Result<Self, TypeUrnError> {
        let mut registry = Self::new();
        for row in std_rows() {
            registry.insert(row.into_entry()?);
        }
        Ok(registry)
    }

    /// Adds an entry, refusing to shadow one that is already there.
    ///
    /// # Errors
    ///
    /// [`TypeUrnError::DuplicateRegistration`] when the base URN is taken.
    /// Use [`TypeRegistry::insert`] to replace deliberately.
    pub fn register(&mut self, entry: TypeEntry) -> Result<(), TypeUrnError> {
        let key = entry.urn().as_str();
        if self.entries.contains_key(key) {
            return Err(TypeUrnError::DuplicateRegistration {
                urn: key.to_owned(),
            });
        }
        self.entries.insert(Box::from(key), entry);
        Ok(())
    }

    /// Adds an entry, replacing any previous one and returning it.
    pub fn insert(&mut self, entry: TypeEntry) -> Option<TypeEntry> {
        let key: Box<str> = Box::from(entry.urn().as_str());
        self.entries.insert(key, entry)
    }

    /// Removes an entry by base URN, returning it.
    pub fn remove(&mut self, urn: &TypeUrn) -> Option<TypeEntry> {
        self.entries.remove(urn.base_str())
    }

    /// The entry for `urn`, ignoring its parameters.
    #[must_use]
    pub fn entry(&self, urn: &TypeUrn) -> Option<&TypeEntry> {
        self.entries.get(urn.base_str())
    }

    /// The entry for `urn`.
    ///
    /// # Errors
    ///
    /// [`TypeUrnError::UnknownType`] when nothing is registered under it.
    pub fn try_entry(&self, urn: &TypeUrn) -> Result<&TypeEntry, TypeUrnError> {
        self.entry(urn).ok_or_else(|| TypeUrnError::UnknownType {
            urn: urn.base_str().to_owned(),
        })
    }

    /// Returns `true` when `urn`'s type is registered.
    #[inline]
    #[must_use]
    pub fn contains(&self, urn: &TypeUrn) -> bool {
        self.entries.contains_key(urn.base_str())
    }

    /// Resolves `urn` and checks its parameters against the registered
    /// contract.
    ///
    /// # Errors
    ///
    /// [`TypeUrnError::UnknownType`], [`TypeUrnError::UnknownParameter`] or
    /// [`TypeUrnError::MissingParameter`].
    pub fn validate(&self, urn: &TypeUrn) -> Result<(), TypeUrnError> {
        self.try_entry(urn)?.validate(urn)
    }

    /// The columnar layout `urn` maps onto.
    ///
    /// # Errors
    ///
    /// Whatever [`TypeRegistry::validate`] reports, or
    /// [`TypeUrnError::LayoutUnavailable`] for a type whose layout this build
    /// does not map.
    ///
    /// ```
    /// use astrs_data::urn::{TypeRegistry, TypeUrn};
    /// use astrs_data::DataType;
    ///
    /// let registry = TypeRegistry::std();
    /// assert_eq!(
    ///     registry.layout(&TypeUrn::parse("std/core/v1/String")?),
    ///     Ok(DataType::Utf8)
    /// );
    /// // Every std/v1 type resolves in this build (see `urn::layouts`); a
    /// // `LayoutUnavailable` error is reserved for a future, append-only
    /// // addition registered before its layout lands.
    /// assert!(registry.layout(&TypeUrn::parse("std/nav/v1/Path")?).is_ok());
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    pub fn layout(&self, urn: &TypeUrn) -> Result<DataType, TypeUrnError> {
        self.try_entry(urn)?.layout(urn)
    }

    /// The first registered type whose fixed layout is exactly `data_type`.
    ///
    /// The reverse of [`TypeRegistry::layout`], used when a decoder has a
    /// concrete column and wants the URN to stamp on the port. Two kinds of
    /// entry are deliberately excluded, because neither has a unique
    /// inverse:
    ///
    /// * [`LayoutRule::Resolver`] entries — a parameterised layout can be
    ///   produced by more than one parameter value (nothing here even tries
    ///   to invert the resolver function).
    /// * [`LayoutRule::Fixed`] entries whose `DataType` is
    ///   [nested](DataType::is_nested) — `std/geometry/v1/Twist` and
    ///   `std/geometry/v1/Accel` are both `Struct{linear: Vector3, angular:
    ///   Vector3}` (ROS 2's `geometry_msgs/Accel` really is the same shape as
    ///   `Twist`), so a `Struct`/`List`/`FixedSizeList` match can be
    ///   genuinely ambiguous. Only the flat scalar set — `core`'s and
    ///   `time`'s — gets a reverse lookup.
    ///
    /// ```
    /// use astrs_data::urn::TypeRegistry;
    /// use astrs_data::DataType;
    ///
    /// let registry = TypeRegistry::std();
    /// assert_eq!(
    ///     registry.urn_for_data_type(&DataType::Utf8).map(|u| u.as_str().to_owned()),
    ///     Some("std/core/v1/String".to_owned())
    /// );
    /// assert!(registry.urn_for_data_type(&DataType::LargeUtf8).is_none());
    ///
    /// // Compound layouts have no unique inverse, even when they are Fixed.
    /// use astrs_data::urn::layouts::geometry::twist_layout;
    /// assert!(registry.urn_for_data_type(&twist_layout()).is_none());
    /// ```
    #[must_use]
    pub fn urn_for_data_type(&self, data_type: &DataType) -> Option<&TypeUrn> {
        if data_type.is_nested() {
            return None;
        }
        self.entries
            .values()
            .find_map(|entry| match entry.layout_rule() {
                LayoutRule::Fixed(fixed) if fixed == data_type => Some(entry.urn()),
                _ => None,
            })
    }

    /// Number of registered types.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when nothing is registered.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, ordered by base URN.
    pub fn iter(&self) -> impl Iterator<Item = &TypeEntry> {
        self.entries.values()
    }

    /// Every entry in one category, ordered by base URN.
    ///
    /// ```
    /// use astrs_data::urn::TypeRegistry;
    ///
    /// let registry = TypeRegistry::std();
    /// assert_eq!(registry.category("time").count(), 2);
    /// assert_eq!(registry.category("nope").count(), 0);
    /// ```
    pub fn category<'a>(&'a self, category: &'a str) -> impl Iterator<Item = &'a TypeEntry> {
        self.entries
            .values()
            .filter(move |entry| entry.urn().category() == category)
    }
}

impl<'a> IntoIterator for &'a TypeRegistry {
    type Item = &'a TypeEntry;
    type IntoIter = std::collections::btree_map::Values<'a, Box<str>, TypeEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.values()
    }
}

/// The process-wide `std` registry, built once.
///
/// ```
/// use astrs_data::urn::{std_registry, TypeUrn};
///
/// let urn = TypeUrn::parse("std/core/v1/Bool")?;
/// assert!(std_registry().contains(&urn));
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
#[must_use]
pub fn std_registry() -> &'static TypeRegistry {
    static REGISTRY: OnceLock<TypeRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TypeRegistry::std)
}

// ---------------------------------------------------------------------------
// The standard table
// ---------------------------------------------------------------------------

/// One row of the standard table, before it becomes a [`TypeEntry`].
struct StdRow {
    /// Canonical URN text — one of the `STD_*` constants.
    urn: &'static str,
    /// Layout rule.
    layout: LayoutRule,
    /// Optional parameters.
    accepted: &'static [&'static str],
    /// Mandatory parameters.
    required: &'static [&'static str],
    /// One-line description.
    summary: &'static str,
}

impl StdRow {
    /// Parses the row's URN and builds its entry.
    fn into_entry(self) -> Result<TypeEntry, TypeUrnError> {
        Ok(TypeEntry::new(TypeUrn::parse(self.urn)?, self.layout)
            .with_accepted_params(self.accepted.iter().copied())
            .with_required_params(self.required.iter().copied())
            .with_summary(self.summary))
    }
}

/// A scalar row: fixed layout, no parameters.
fn scalar(urn: &'static str, data_type: DataType, summary: &'static str) -> StdRow {
    StdRow {
        urn,
        layout: LayoutRule::Fixed(data_type),
        accepted: &[],
        required: &[],
        summary,
    }
}

/// A row whose layout is fixed but whose type still accepts parameters that
/// are graph metadata rather than shape — `frame`/`child_frame`, or a value
/// that does not change the columnar layout such as `CompressedImage`'s
/// `format`.
fn fixed(
    urn: &'static str,
    data_type: DataType,
    accepted: &'static [&'static str],
    required: &'static [&'static str],
    summary: &'static str,
) -> StdRow {
    StdRow {
        urn,
        layout: LayoutRule::Fixed(data_type),
        accepted,
        required,
        summary,
    }
}

/// A row whose layout is computed from its parameters — `Image[pixel=…]`,
/// `AudioFrame[sample=…]`, `PointCloud[fields=…]` (see
/// [`crate::urn::layouts`]).
fn resolved(
    urn: &'static str,
    resolver: LayoutResolver,
    accepted: &'static [&'static str],
    required: &'static [&'static str],
    summary: &'static str,
) -> StdRow {
    StdRow {
        urn,
        layout: LayoutRule::Resolver(resolver),
        accepted,
        required,
        summary,
    }
}

/// The initial `std` table (blueprint §24.3), in registration order.
fn std_rows() -> Vec<StdRow> {
    vec![
        // -- core: scalar channels -------------------------------------------
        scalar(STD_CORE_BOOL, DataType::Bool, "a boolean channel"),
        scalar(STD_CORE_INT8, DataType::Int8, "an 8-bit signed channel"),
        scalar(STD_CORE_INT16, DataType::Int16, "a 16-bit signed channel"),
        scalar(STD_CORE_INT32, DataType::Int32, "a 32-bit signed channel"),
        scalar(STD_CORE_INT64, DataType::Int64, "a 64-bit signed channel"),
        scalar(STD_CORE_UINT8, DataType::UInt8, "an 8-bit unsigned channel"),
        scalar(
            STD_CORE_UINT16,
            DataType::UInt16,
            "a 16-bit unsigned channel",
        ),
        scalar(
            STD_CORE_UINT32,
            DataType::UInt32,
            "a 32-bit unsigned channel",
        ),
        scalar(
            STD_CORE_UINT64,
            DataType::UInt64,
            "a 64-bit unsigned channel",
        ),
        scalar(
            STD_CORE_FLOAT16,
            DataType::Float16,
            "an IEEE 754 binary16 channel",
        ),
        scalar(
            STD_CORE_FLOAT32,
            DataType::Float32,
            "an IEEE 754 binary32 channel",
        ),
        scalar(
            STD_CORE_FLOAT64,
            DataType::Float64,
            "an IEEE 754 binary64 channel",
        ),
        scalar(
            STD_CORE_STRING,
            DataType::Utf8,
            "UTF-8 text with 32-bit offsets",
        ),
        scalar(
            STD_CORE_BYTES,
            DataType::Binary,
            "opaque bytes with 32-bit offsets",
        ),
        scalar(
            STD_CORE_EMPTY,
            DataType::Null,
            "a signal carrying no payload",
        ),
        // -- time -------------------------------------------------------------
        scalar(
            STD_TIME_TIMESTAMP,
            DataType::Timestamp,
            "nanoseconds since the Unix epoch, timezone-less",
        ),
        scalar(
            STD_TIME_DURATION,
            DataType::Duration,
            "a nanosecond interval",
        ),
        // -- media ------------------------------------------------------------
        resolved(
            STD_MEDIA_IMAGE,
            media::image_resolver,
            &["pixel", "width", "height", "stride"],
            &["pixel"],
            "a raw image frame",
        ),
        resolved(
            STD_MEDIA_AUDIO_FRAME,
            media::audio_frame_resolver,
            &["sample", "rate", "channels"],
            &["sample"],
            "a block of PCM audio samples",
        ),
        fixed(
            STD_MEDIA_COMPRESSED_IMAGE,
            media::compressed_image_layout(),
            &["format"],
            &["format"],
            "an encoded image frame",
        ),
        // -- vision -----------------------------------------------------------
        fixed(
            STD_VISION_DETECTIONS,
            vision::detections_layout(),
            &["model"],
            &[],
            "bounding boxes with scores and labels",
        ),
        fixed(
            STD_VISION_KEYPOINTS,
            vision::keypoints_layout(),
            &["model"],
            &[],
            "landmark sets with per-point confidence",
        ),
        fixed(
            STD_VISION_MASK,
            vision::mask_layout(),
            &["labels"],
            &[],
            "a per-pixel label plane",
        ),
        // -- geometry ---------------------------------------------------------
        fixed(
            STD_GEOMETRY_POSE,
            geometry::pose_layout(),
            &["frame"],
            &[],
            "a position and an orientation",
        ),
        fixed(
            STD_GEOMETRY_TRANSFORM,
            geometry::transform_layout(),
            &["frame", "child_frame"],
            &[],
            "a rigid transform between two frames",
        ),
        fixed(
            STD_GEOMETRY_TWIST,
            geometry::twist_layout(),
            &["frame"],
            &[],
            "linear and angular velocity",
        ),
        fixed(
            STD_GEOMETRY_ACCEL,
            geometry::accel_layout(),
            &["frame"],
            &[],
            "linear and angular acceleration",
        ),
        fixed(
            STD_GEOMETRY_QUATERNION,
            geometry::quaternion_layout(),
            &[],
            &[],
            "an orientation, stored xyzw",
        ),
        fixed(
            STD_GEOMETRY_VECTOR3,
            geometry::vector3_layout(),
            &["frame"],
            &[],
            "a 3-vector",
        ),
        // -- sensor -----------------------------------------------------------
        fixed(
            STD_SENSOR_LASER_SCAN,
            sensor::laser_scan_layout(),
            &["frame"],
            &[],
            "a planar range sweep",
        ),
        resolved(
            STD_SENSOR_POINT_CLOUD,
            sensor::point_cloud_resolver,
            &["fields", "frame"],
            &["fields"],
            "a point set with named per-point fields",
        ),
        fixed(
            STD_SENSOR_IMU,
            sensor::imu_layout(),
            &["frame"],
            &[],
            "angular rate, acceleration and orientation",
        ),
        fixed(
            STD_SENSOR_NAV_SAT_FIX,
            sensor::nav_sat_fix_layout(),
            &[],
            &[],
            "a GNSS fix",
        ),
        fixed(
            STD_SENSOR_RANGE,
            sensor::range_layout(),
            &["frame"],
            &[],
            "a single-beam distance reading",
        ),
        // -- nav ---------------------------------------------------------------
        fixed(
            STD_NAV_ODOMETRY,
            nav::odometry_layout(),
            &["frame", "child_frame"],
            &[],
            "pose and twist with covariance",
        ),
        fixed(
            STD_NAV_PATH,
            nav::path_layout(),
            &["frame"],
            &[],
            "an ordered pose sequence",
        ),
        fixed(
            STD_NAV_OCCUPANCY_GRID,
            nav::occupancy_grid_layout(),
            &["frame"],
            &[],
            "a 2-D occupancy costmap",
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn std_table_is_wellformed() {
        let registry = TypeRegistry::try_std().expect("every standard row parses");
        assert_eq!(registry.len(), STD_TYPE_URNS.len());
        assert_eq!(TypeRegistry::std().len(), registry.len());
    }

    #[test]
    fn constants_and_table_agree() {
        let registry = TypeRegistry::std();
        for text in STD_TYPE_URNS {
            let urn = TypeUrn::parse(text).unwrap();
            assert!(registry.contains(&urn), "{text} is not registered");
            assert_eq!(urn.as_str(), *text, "{text} is not canonical");
        }
        assert_eq!(registry.len(), STD_TYPE_URNS.len());
    }

    #[test]
    fn every_constant_is_unique() {
        let mut sorted: Vec<&str> = STD_TYPE_URNS.to_vec();
        sorted.sort_unstable();
        let count = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), count, "duplicate URN constant");
    }

    #[test]
    fn category_constants_match_the_table() {
        let registry = TypeRegistry::std();
        assert_eq!(registry.category(CATEGORY_CORE).count(), 15);
        assert_eq!(registry.category(CATEGORY_TIME).count(), 2);
        assert_eq!(registry.category(CATEGORY_MEDIA).count(), 3);
        assert_eq!(registry.category(CATEGORY_VISION).count(), 3);
        assert_eq!(registry.category(CATEGORY_GEOMETRY).count(), 6);
        assert_eq!(registry.category(CATEGORY_SENSOR).count(), 5);
        assert_eq!(registry.category(CATEGORY_NAV).count(), 3);
    }

    #[test]
    fn every_std_type_uses_the_std_version() {
        for entry in TypeRegistry::std().iter() {
            assert_eq!(entry.urn().version(), STD_VERSION);
        }
    }

    #[test]
    fn every_std_entry_has_an_available_layout() {
        // Every `std/v1` type resolves in this build: core/time scalars are
        // `Fixed` directly here; the other five categories are `Fixed` or
        // `Resolver`, defined in `urn::layouts`.
        for entry in TypeRegistry::std().iter() {
            assert!(
                entry.layout_rule().is_available(),
                "{} should have a layout",
                entry.urn()
            );
        }
    }

    #[test]
    fn a_deferred_entry_reports_layout_unavailable_not_unknown() {
        // No std/v1 type defers any longer; exercise `LayoutRule::Deferred`
        // directly so the branch that reports `LayoutUnavailable` (rather
        // than `UnknownType`) stays correct for whatever a future,
        // append-only revision registers before its layout lands.
        let mut registry = TypeRegistry::new();
        let urn = TypeUrn::parse("std/future/v1/Ghost").unwrap();
        registry.insert(TypeEntry::new(urn.clone(), LayoutRule::Deferred));

        assert!(registry.contains(&urn), "registered, just not laid out");
        let err = registry.layout(&urn).unwrap_err();
        assert!(err.is_layout_unavailable());
        assert!(!matches!(err, TypeUrnError::UnknownType { .. }));
    }

    #[test]
    fn compound_layouts_have_no_reverse_lookup() {
        // Every category beyond core/time resolves to a nested DataType, and
        // `urn_for_data_type` deliberately does not attempt to invert those —
        // see its doc for the Twist/Accel collision this sidesteps.
        let registry = TypeRegistry::std();
        for category in [
            CATEGORY_MEDIA,
            CATEGORY_VISION,
            CATEGORY_GEOMETRY,
            CATEGORY_SENSOR,
            CATEGORY_NAV,
        ] {
            for entry in registry.category(category) {
                let mut urn = entry.urn().clone();
                for key in entry.required_params() {
                    let value = match key.as_ref() {
                        "pixel" => "rgb8",
                        "sample" => "s16",
                        "fields" => "x:y:z",
                        "format" => "jpeg",
                        _ => "x",
                    };
                    urn = urn.with_param(key, value).unwrap();
                }
                let data_type = registry.layout(&urn).unwrap_or_else(|err| {
                    panic!("{}: {err}", entry.urn());
                });
                assert!(data_type.is_nested(), "{}", entry.urn());
                assert!(
                    registry.urn_for_data_type(&data_type).is_none(),
                    "{}",
                    entry.urn()
                );
            }
        }
    }

    #[test]
    fn twist_and_accel_collide_but_neither_wins() {
        use crate::urn::layouts::geometry::{accel_layout, twist_layout};

        assert_eq!(
            twist_layout(),
            accel_layout(),
            "same shape by ROS 2 convention"
        );
        let registry = TypeRegistry::std();
        assert!(registry.urn_for_data_type(&twist_layout()).is_none());
        assert!(registry.urn_for_data_type(&accel_layout()).is_none());
    }

    #[test]
    fn scalar_layouts_are_exact() {
        let registry = TypeRegistry::std();
        let expected = [
            (STD_CORE_BOOL, DataType::Bool),
            (STD_CORE_INT8, DataType::Int8),
            (STD_CORE_INT16, DataType::Int16),
            (STD_CORE_INT32, DataType::Int32),
            (STD_CORE_INT64, DataType::Int64),
            (STD_CORE_UINT8, DataType::UInt8),
            (STD_CORE_UINT16, DataType::UInt16),
            (STD_CORE_UINT32, DataType::UInt32),
            (STD_CORE_UINT64, DataType::UInt64),
            (STD_CORE_FLOAT16, DataType::Float16),
            (STD_CORE_FLOAT32, DataType::Float32),
            (STD_CORE_FLOAT64, DataType::Float64),
            (STD_CORE_STRING, DataType::Utf8),
            (STD_CORE_BYTES, DataType::Binary),
            (STD_CORE_EMPTY, DataType::Null),
            (STD_TIME_TIMESTAMP, DataType::Timestamp),
            (STD_TIME_DURATION, DataType::Duration),
        ];
        for (text, data_type) in expected {
            let urn = TypeUrn::parse(text).unwrap();
            assert_eq!(registry.layout(&urn), Ok(data_type), "{text}");
        }
    }

    #[test]
    fn unknown_types_are_reported_as_such() {
        let registry = TypeRegistry::std();
        let urn = TypeUrn::parse("std/core/v1/Complex").unwrap();
        assert_eq!(
            registry.layout(&urn),
            Err(TypeUrnError::UnknownType {
                urn: "std/core/v1/Complex".to_owned()
            })
        );
        assert!(registry.entry(&urn).is_none());
        assert!(registry.try_entry(&urn).is_err());
    }

    #[test]
    fn a_different_version_is_a_different_type() {
        let registry = TypeRegistry::std();
        assert!(registry.contains(&TypeUrn::parse("std/core/v1/Bool").unwrap()));
        assert!(!registry.contains(&TypeUrn::parse("std/core/v2/Bool").unwrap()));
    }

    #[test]
    fn parameters_are_checked_against_the_contract() {
        let registry = TypeRegistry::std();

        let image = TypeUrn::parse("std/media/v1/Image[pixel=rgb8,width=640]").unwrap();
        assert_eq!(registry.validate(&image), Ok(()));

        let missing = TypeUrn::parse("std/media/v1/Image[width=640]").unwrap();
        assert_eq!(
            registry.validate(&missing),
            Err(TypeUrnError::MissingParameter {
                urn: STD_MEDIA_IMAGE.to_owned(),
                key: "pixel".to_owned(),
            })
        );

        let unknown = TypeUrn::parse("std/media/v1/Image[pixel=rgb8,bogus=1]").unwrap();
        assert_eq!(
            registry.validate(&unknown),
            Err(TypeUrnError::UnknownParameter {
                urn: STD_MEDIA_IMAGE.to_owned(),
                key: "bogus".to_owned(),
            })
        );
    }

    #[test]
    fn scalar_types_accept_no_parameters() {
        let registry = TypeRegistry::std();
        let urn = TypeUrn::parse("std/core/v1/Int32[width=4]").unwrap();
        assert!(matches!(
            registry.validate(&urn),
            Err(TypeUrnError::UnknownParameter { .. })
        ));
    }

    #[test]
    fn required_params_are_also_accepted_params() {
        let registry = TypeRegistry::std();
        let entry = registry
            .entry(&TypeUrn::parse(STD_SENSOR_POINT_CLOUD).unwrap())
            .unwrap();
        assert!(entry.required_params().iter().any(|k| &**k == "fields"));
        assert!(entry.accepted_params().iter().any(|k| &**k == "fields"));
        assert!(entry.accepted_params().windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn entries_are_keyed_by_base_urn() {
        let registry = TypeRegistry::std();
        let parameterised = TypeUrn::parse("std/media/v1/Image[pixel=mono8]").unwrap();
        let entry = registry.entry(&parameterised).unwrap();
        assert_eq!(entry.urn().as_str(), STD_MEDIA_IMAGE);
        assert!(!entry.urn().has_params());
    }

    #[test]
    fn register_refuses_to_shadow() {
        let mut registry = TypeRegistry::std();
        let urn = TypeUrn::parse(STD_CORE_BOOL).unwrap();
        let entry = TypeEntry::new(urn.clone(), LayoutRule::Fixed(DataType::Int8));
        assert_eq!(
            registry.register(entry),
            Err(TypeUrnError::DuplicateRegistration {
                urn: STD_CORE_BOOL.to_owned()
            })
        );
        // The original survives.
        assert_eq!(registry.layout(&urn), Ok(DataType::Bool));
    }

    #[test]
    fn insert_replaces_and_returns_the_previous_entry() {
        let mut registry = TypeRegistry::std();
        let urn = TypeUrn::parse(STD_GEOMETRY_VECTOR3).unwrap();
        let previous = registry.insert(TypeEntry::new(
            urn.clone(),
            LayoutRule::Fixed(DataType::Float64),
        ));
        assert!(previous.is_some());
        assert_eq!(registry.layout(&urn), Ok(DataType::Float64));
        assert_eq!(registry.len(), STD_TYPE_URNS.len());
    }

    #[test]
    fn remove_drops_the_entry() {
        let mut registry = TypeRegistry::std();
        let urn = TypeUrn::parse(STD_NAV_PATH).unwrap();
        assert!(registry.remove(&urn).is_some());
        assert!(!registry.contains(&urn));
        assert!(registry.remove(&urn).is_none());
        assert_eq!(registry.len(), STD_TYPE_URNS.len() - 1);
    }

    #[test]
    fn a_registered_resolver_computes_from_parameters() {
        fn resolve(urn: &TypeUrn) -> Result<DataType, TypeUrnError> {
            match urn.param("pixel") {
                Some("mono8") => Ok(DataType::UInt8),
                Some("mono16") => Ok(DataType::UInt16),
                Some(other) => Err(TypeUrnError::UnsupportedParameterValue {
                    urn: urn.base_str().to_owned(),
                    key: "pixel".to_owned(),
                    value: other.to_owned(),
                }),
                None => Err(TypeUrnError::MissingParameter {
                    urn: urn.base_str().to_owned(),
                    key: "pixel".to_owned(),
                }),
            }
        }

        let mut registry = TypeRegistry::std();
        let base = TypeUrn::parse(STD_MEDIA_IMAGE).unwrap();
        registry.insert(
            TypeEntry::new(base.clone(), LayoutRule::Resolver(resolve))
                .with_required_params(["pixel"]),
        );

        let mono = base.clone().with_param("pixel", "mono8").unwrap();
        assert_eq!(registry.layout(&mono), Ok(DataType::UInt8));

        let rgb = base.clone().with_param("pixel", "rgb8").unwrap();
        assert_eq!(
            registry.layout(&rgb),
            Err(TypeUrnError::UnsupportedParameterValue {
                urn: STD_MEDIA_IMAGE.to_owned(),
                key: "pixel".to_owned(),
                value: "rgb8".to_owned(),
            })
        );

        // The contract check still runs before the resolver.
        assert!(matches!(
            registry.layout(&base),
            Err(TypeUrnError::MissingParameter { .. })
        ));
    }

    #[test]
    fn reverse_lookup_finds_the_scalar_types() {
        let registry = TypeRegistry::std();
        for (data_type, expected) in [
            (DataType::Bool, STD_CORE_BOOL),
            (DataType::Int64, STD_CORE_INT64),
            (DataType::Float32, STD_CORE_FLOAT32),
            (DataType::Utf8, STD_CORE_STRING),
            (DataType::Binary, STD_CORE_BYTES),
            (DataType::Null, STD_CORE_EMPTY),
            (DataType::Timestamp, STD_TIME_TIMESTAMP),
            (DataType::Duration, STD_TIME_DURATION),
        ] {
            assert_eq!(
                registry.urn_for_data_type(&data_type).map(TypeUrn::as_str),
                Some(expected),
                "{data_type}"
            );
        }
        assert!(registry.urn_for_data_type(&DataType::LargeBinary).is_none());
        assert!(registry.urn_for_data_type(&DataType::LargeUtf8).is_none());
    }

    #[test]
    fn iteration_is_ordered_and_complete() {
        let registry = TypeRegistry::std();
        let urns: Vec<&str> = registry.iter().map(|entry| entry.urn().as_str()).collect();
        assert_eq!(urns.len(), STD_TYPE_URNS.len());
        assert!(urns.windows(2).all(|w| w[0] < w[1]), "not sorted: {urns:?}");

        let by_ref: Vec<&str> = (&registry)
            .into_iter()
            .map(|entry| entry.urn().as_str())
            .collect();
        assert_eq!(by_ref, urns);
    }

    #[test]
    fn an_empty_registry_knows_nothing() {
        let registry = TypeRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.iter().next().is_none());
        let urn = TypeUrn::parse(STD_CORE_BOOL).unwrap();
        assert!(matches!(
            registry.layout(&urn),
            Err(TypeUrnError::UnknownType { .. })
        ));
    }

    #[test]
    fn the_process_registry_is_the_std_table() {
        assert_eq!(std_registry().len(), STD_TYPE_URNS.len());
        // Same instance on every call.
        assert!(std::ptr::eq(std_registry(), std_registry()));
    }

    #[test]
    fn summaries_are_present() {
        for entry in TypeRegistry::std().iter() {
            assert!(!entry.summary().is_empty(), "{}", entry.urn());
        }
    }

    #[test]
    fn entry_builders_deduplicate() {
        let entry = TypeEntry::new(TypeUrn::parse(STD_CORE_BOOL).unwrap(), LayoutRule::Deferred)
            .with_accepted_params(["a", "a", "b"])
            .with_required_params(["b", "b"]);
        assert_eq!(entry.accepted_params().len(), 2);
        assert_eq!(entry.required_params().len(), 1);
    }

    #[test]
    fn layout_rule_debug_is_readable() {
        assert_eq!(
            format!("{:?}", LayoutRule::Fixed(DataType::Int32)),
            "Fixed(Int32)"
        );
        assert_eq!(format!("{:?}", LayoutRule::Deferred), "Deferred");
        let rule: LayoutRule = LayoutRule::Resolver(|_| Ok(DataType::Null));
        assert_eq!(format!("{rule:?}"), "Resolver(..)");
    }
}
