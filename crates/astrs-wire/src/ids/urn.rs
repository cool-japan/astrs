//! [`TypeUrn`] — the port type identifier that rides the control plane.
//!
//! Blueprint §24.3 defines the URN grammar as
//! `<namespace>/<category>/v<n>/<Type>[k=v,…]`, for example
//! `std/media/v1/Image[pixel=rgb8]` or `std/geometry/v1/Pose`. Ports declare
//! them (`input_types` / `output_types`, §8.3), the graph type-checks edges
//! against them at `validate` (§3.7), and `astrs-data` documents the
//! normative columnar layout each one denotes.
//!
//! This crate owns none of that semantics. It owns the *transport* of the
//! string: a validated newtype that cannot carry a control character or an
//! unbounded blob onto the wire, plus cheap structural accessors so a daemon
//! can log `Image` without pulling in the graph crate. Compatibility rules
//! (`type_rules: [{from, to}]`, §8.2) live in `astrs-graph`.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::TypeUrn;
//!
//! let urn: TypeUrn = "std/media/v1/Image[pixel=rgb8,width=640]".parse()?;
//! assert_eq!(urn.namespace(), Some("std"));
//! assert_eq!(urn.category(), Some("media"));
//! assert_eq!(urn.version(), Some(1));
//! assert_eq!(urn.type_name(), Some("Image"));
//! assert_eq!(urn.parameter("pixel"), Some("rgb8"));
//!
//! // The explicit escape hatch for untyped ports (`type: any`).
//! assert!(TypeUrn::any().is_any());
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;
use core::str::FromStr;

use oxicode::de::{Decode, Decoder};
use oxicode::enc::{Encode, Encoder};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{IdError, IdKind, codec_invalid, preview};

/// The maximum length of a type URN in bytes.
///
/// Generous enough for a point-cloud URN listing a dozen fields, small enough
/// that a map of them cannot be used to exhaust memory.
pub const MAX_TYPE_URN_LEN: usize = 512;

/// The URN denoting an explicitly untyped port (manifest `type: any`).
pub const ANY_TYPE_URN: &str = "any";

/// Whether `ch` may appear in a type URN.
///
/// The set is printable ASCII minus whitespace: alphanumerics and
/// `/ . _ - [ ] = , : + * ( ) < > @ ~ ^ $ % ! ? # & ' " ; | \ { } ` are all
/// admitted in principle, but control characters, spaces and non-ASCII are
/// not — those are the ones that break log lines, shell quoting and JSON
/// round-trips.
///
/// # Examples
///
/// ```
/// use astrs_wire::ids::is_type_urn_char;
///
/// assert!(is_type_urn_char('/'));
/// assert!(is_type_urn_char('['));
/// assert!(!is_type_urn_char(' '));
/// assert!(!is_type_urn_char('\n'));
/// assert!(!is_type_urn_char('é'));
/// ```
#[must_use]
pub const fn is_type_urn_char(ch: char) -> bool {
    ch.is_ascii_graphic()
}

/// A validated port type URN.
///
/// # Examples
///
/// ```
/// use astrs_wire::TypeUrn;
///
/// let urn = TypeUrn::new("std/sensor/v1/PointCloud[fields=xyzi]")?;
/// assert_eq!(urn.base(), "std/sensor/v1/PointCloud");
/// assert!(urn.is_structured());
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TypeUrn(String);

impl TypeUrn {
    /// The maximum length in bytes.
    pub const MAX_LEN: usize = MAX_TYPE_URN_LEN;

    /// Validates `value` and wraps it as a type URN.
    ///
    /// # Errors
    ///
    /// - [`IdError::Empty`] for the empty string.
    /// - [`IdError::TooLong`] beyond [`MAX_TYPE_URN_LEN`] bytes.
    /// - [`IdError::InvalidChar`] for a character outside printable ASCII.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// assert!(TypeUrn::new("std/core/v1/Bool").is_ok());
    /// assert!(TypeUrn::new("std/core/v1/Bool ").is_err());
    /// assert!(TypeUrn::new("").is_err());
    /// ```
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdError::Empty {
                kind: IdKind::TypeUrn,
            });
        }
        if value.len() > Self::MAX_LEN {
            return Err(IdError::TooLong {
                kind: IdKind::TypeUrn,
                len: value.len(),
                max: Self::MAX_LEN,
            });
        }
        for (index, ch) in value.char_indices() {
            if !is_type_urn_char(ch) {
                return Err(IdError::InvalidChar {
                    kind: IdKind::TypeUrn,
                    value: preview(&value),
                    ch,
                    index,
                });
            }
        }
        Ok(Self(value))
    }

    /// The URN denoting an explicitly untyped port (manifest `type: any`).
    ///
    /// An associated function rather than a `const` because the inner
    /// [`String`] cannot be allocated at compile time.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// assert_eq!(TypeUrn::any().as_str(), "any");
    /// assert!(TypeUrn::any().is_any());
    /// ```
    #[must_use]
    pub fn any() -> Self {
        Self(ANY_TYPE_URN.to_owned())
    }

    /// The URN as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the URN and returns the inner [`String`].
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// Whether this is the untyped escape hatch (`any`).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// assert!(TypeUrn::new("any")?.is_any());
    /// assert!(!TypeUrn::new("std/core/v1/Bool")?.is_any());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn is_any(&self) -> bool {
        self.0 == ANY_TYPE_URN
    }

    /// The URN without its bracketed parameter list.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// let urn = TypeUrn::new("std/media/v1/Image[pixel=rgb8]")?;
    /// assert_eq!(urn.base(), "std/media/v1/Image");
    ///
    /// let plain = TypeUrn::new("std/geometry/v1/Pose")?;
    /// assert_eq!(plain.base(), "std/geometry/v1/Pose");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn base(&self) -> &str {
        match self.0.find('[') {
            Some(index) => &self.0[..index],
            None => &self.0,
        }
    }

    /// The bracketed parameter list, without the brackets.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// let urn = TypeUrn::new("std/media/v1/Image[pixel=rgb8,width=640]")?;
    /// assert_eq!(urn.parameter_list(), Some("pixel=rgb8,width=640"));
    /// assert_eq!(TypeUrn::new("std/core/v1/Bool")?.parameter_list(), None);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn parameter_list(&self) -> Option<&str> {
        let open = self.0.find('[')?;
        let close = self.0.rfind(']')?;
        if close <= open + 1 {
            return None;
        }
        self.0.get(open + 1..close)
    }

    /// The `k=v` pairs of the parameter list, in the order they appear.
    ///
    /// Entries without an `=` yield an empty value, so a bare flag such as
    /// `[compressed]` is still visible.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// let urn = TypeUrn::new("std/media/v1/Image[pixel=rgb8,width=640]")?;
    /// let pairs: Vec<_> = urn.parameters().collect();
    /// assert_eq!(pairs, vec![("pixel", "rgb8"), ("width", "640")]);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    pub fn parameters(&self) -> impl Iterator<Item = (&str, &str)> {
        self.parameter_list()
            .unwrap_or_default()
            .split(',')
            .filter(|part| !part.is_empty())
            .map(|part| match part.split_once('=') {
                Some((key, value)) => (key.trim(), value.trim()),
                None => (part.trim(), ""),
            })
    }

    /// Looks up one parameter by name.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// let urn = TypeUrn::new("std/sensor/v1/PointCloud[fields=xyzi]")?;
    /// assert_eq!(urn.parameter("fields"), Some("xyzi"));
    /// assert_eq!(urn.parameter("absent"), None);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }

    /// The `/`-separated segments of [`TypeUrn::base`].
    fn segments(&self) -> impl Iterator<Item = &str> {
        self.base().split('/')
    }

    /// The first segment: the registry namespace (`std` for the built-in set).
    ///
    /// Returns `None` for a URN that is not in `ns/cat/vN/Type` shape.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// assert_eq!(TypeUrn::new("std/core/v1/Bool")?.namespace(), Some("std"));
    /// assert_eq!(TypeUrn::new("any")?.namespace(), None);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.structured_segment(0)
    }

    /// The second segment: the category (`media`, `vision`, `geometry`, …).
    #[must_use]
    pub fn category(&self) -> Option<&str> {
        self.structured_segment(1)
    }

    /// The third segment's numeric part: the `N` of `vN`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// assert_eq!(TypeUrn::new("std/nav/v2/Path")?.version(), Some(2));
    /// assert_eq!(TypeUrn::new("std/nav/x2/Path")?.version(), None);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn version(&self) -> Option<u16> {
        self.structured_segment(2)?.strip_prefix('v')?.parse().ok()
    }

    /// The fourth segment: the type name (`Image`, `Pose`, …).
    #[must_use]
    pub fn type_name(&self) -> Option<&str> {
        self.structured_segment(3)
    }

    /// Whether the URN follows the four-segment `ns/cat/vN/Type` shape with a
    /// parseable version.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TypeUrn;
    ///
    /// assert!(TypeUrn::new("std/core/v1/Int32")?.is_structured());
    /// assert!(!TypeUrn::new("any")?.is_structured());
    /// assert!(!TypeUrn::new("a/b/c/d/e")?.is_structured());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn is_structured(&self) -> bool {
        self.segments().count() == 4 && self.version().is_some()
    }

    /// The `index`-th segment, but only when the URN has exactly four.
    fn structured_segment(&self, index: usize) -> Option<&str> {
        let mut segments = self.base().split('/');
        let collected: [&str; 4] = [
            segments.next()?,
            segments.next()?,
            segments.next()?,
            segments.next()?,
        ];
        if segments.next().is_some() {
            return None;
        }
        collected.get(index).copied()
    }
}

impl fmt::Display for TypeUrn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for TypeUrn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TypeUrn({:?})", self.0)
    }
}

impl FromStr for TypeUrn {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for TypeUrn {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for TypeUrn {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl AsRef<str> for TypeUrn {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TypeUrn {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl Encode for TypeUrn {
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), oxicode::error::Error> {
        self.0.encode(encoder)
    }
}

impl<Context> Decode<Context> for TypeUrn {
    fn decode<D: Decoder<Context = Context>>(
        decoder: &mut D,
    ) -> Result<Self, oxicode::error::Error> {
        let raw = String::decode(decoder)?;
        Self::new(raw).map_err(|err| codec_invalid(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn accepts_the_registry_examples() {
        for urn in [
            "std/core/v1/Bool",
            "std/core/v1/Float64",
            "std/media/v1/Image[pixel=rgb8]",
            "std/media/v1/AudioFrame[sample=s16le]",
            "std/vision/v1/Detections",
            "std/geometry/v1/Pose",
            "std/sensor/v1/PointCloud[fields=xyzirt]",
            "std/nav/v1/OccupancyGrid",
            "std/time/v1/Duration",
            "any",
        ] {
            assert!(TypeUrn::new(urn).is_ok(), "{urn} should be valid");
        }
    }

    #[test]
    fn rejects_whitespace_control_and_non_ascii() {
        for bad in ["a b", "a\tb", "a\nb", "café", "a\u{0}b", "\u{200b}"] {
            assert!(TypeUrn::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_empty_and_oversize() {
        assert!(matches!(
            TypeUrn::new(""),
            Err(IdError::Empty {
                kind: IdKind::TypeUrn
            })
        ));
        let at_limit = "a".repeat(MAX_TYPE_URN_LEN);
        assert!(TypeUrn::new(&at_limit).is_ok());
        assert!(matches!(
            TypeUrn::new("a".repeat(MAX_TYPE_URN_LEN + 1)),
            Err(IdError::TooLong { .. })
        ));
    }

    #[test]
    fn structural_accessors_decompose_a_full_urn() {
        let urn = TypeUrn::new("std/media/v1/Image[pixel=rgb8,width=640,height=480]").unwrap();
        assert_eq!(urn.namespace(), Some("std"));
        assert_eq!(urn.category(), Some("media"));
        assert_eq!(urn.version(), Some(1));
        assert_eq!(urn.type_name(), Some("Image"));
        assert_eq!(urn.base(), "std/media/v1/Image");
        assert_eq!(
            urn.parameter_list(),
            Some("pixel=rgb8,width=640,height=480")
        );
        assert_eq!(urn.parameter("width"), Some("640"));
        assert_eq!(urn.parameter("depth"), None);
        assert!(urn.is_structured());
    }

    #[test]
    fn accessors_are_none_for_unstructured_urns() {
        for text in ["any", "a/b", "a/b/c", "a/b/c/d/e"] {
            let urn = TypeUrn::new(text).unwrap();
            assert!(!urn.is_structured(), "{text} should be unstructured");
            assert_eq!(urn.type_name(), None, "{text}");
        }
    }

    #[test]
    fn version_requires_a_v_prefix_and_digits() {
        assert_eq!(TypeUrn::new("std/a/v0/T").unwrap().version(), Some(0));
        assert_eq!(
            TypeUrn::new("std/a/v65535/T").unwrap().version(),
            Some(65_535)
        );
        assert_eq!(TypeUrn::new("std/a/v65536/T").unwrap().version(), None);
        assert_eq!(TypeUrn::new("std/a/1/T").unwrap().version(), None);
        assert_eq!(TypeUrn::new("std/a/vx/T").unwrap().version(), None);
    }

    #[test]
    fn parameters_handle_edge_shapes() {
        let empty = TypeUrn::new("std/a/v1/T[]").unwrap();
        assert_eq!(empty.parameter_list(), None);
        assert_eq!(empty.parameters().count(), 0);
        assert_eq!(empty.base(), "std/a/v1/T");

        let flag = TypeUrn::new("std/a/v1/T[compressed]").unwrap();
        assert_eq!(flag.parameter("compressed"), Some(""));

        let trailing = TypeUrn::new("std/a/v1/T[a=1,]").unwrap();
        let pairs: Vec<_> = trailing.parameters().collect();
        assert_eq!(pairs, vec![("a", "1")]);
    }

    #[test]
    fn any_is_recognised() {
        let any = TypeUrn::any();
        assert!(any.is_any());
        assert_eq!(any.as_str(), ANY_TYPE_URN);
        assert_eq!(any.to_string(), "any");
        assert_eq!(any, TypeUrn::new("any").unwrap());
        assert!(!TypeUrn::new("std/core/v1/Bool").unwrap().is_any());
        assert!(!any.is_structured());
    }

    #[test]
    fn codec_round_trips_and_validates_on_decode() {
        let urn = TypeUrn::new("std/vision/v1/Keypoints").unwrap();
        let bytes = urn.encode_to_vec().unwrap();
        assert_eq!(TypeUrn::decode_exact(&bytes).unwrap(), urn);

        let hostile = "has space".to_owned().encode_to_vec().unwrap();
        assert!(TypeUrn::decode_exact(&hostile).is_err());

        let oversize = "a".repeat(MAX_TYPE_URN_LEN + 1).encode_to_vec().unwrap();
        assert!(TypeUrn::decode_exact(&oversize).is_err());
    }

    #[test]
    fn serde_round_trips_as_a_bare_string() {
        let urn = TypeUrn::new("std/core/v1/Bytes").unwrap();
        let json = serde_json::to_string(&urn).unwrap();
        assert_eq!(json, "\"std/core/v1/Bytes\"");
        assert_eq!(serde_json::from_str::<TypeUrn>(&json).unwrap(), urn);
        assert!(serde_json::from_str::<TypeUrn>("\"bad urn\"").is_err());
    }

    #[test]
    fn conversions_and_formatting() {
        let urn = TypeUrn::try_from("std/core/v1/Empty").unwrap();
        assert_eq!(
            TypeUrn::try_from("std/core/v1/Empty".to_owned()).unwrap(),
            urn
        );
        assert_eq!("std/core/v1/Empty".parse::<TypeUrn>().unwrap(), urn);
        assert_eq!(urn.as_ref(), "std/core/v1/Empty");
        assert_eq!(urn.to_string(), "std/core/v1/Empty");
        assert_eq!(format!("{urn:?}"), "TypeUrn(\"std/core/v1/Empty\")");
        assert_eq!(urn.clone().into_string(), "std/core/v1/Empty");
    }
}
