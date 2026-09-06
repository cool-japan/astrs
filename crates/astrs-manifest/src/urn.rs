//! Type URNs (`std/<cat>/v<u16>/<Type>[params]`), blueprint §8.3 / §24.3.
//!
//! `input_types` / `output_types` and `type_rules` carry URN strings. This
//! module stores them as a validated newtype **without** depending on
//! `astrs-data` (which owns the actual columnar type registry) — the
//! manifest crate only needs to know a URN is *well-formed*, not resolve it
//! to a concrete type.
//!
//! Deliberately, [`Urn::deserialize`] never fails on malformed syntax: it
//! accepts any string so the [validation pass](crate::validate) can report
//! *all* malformed URNs across a manifest in one pass (per blueprint §8,
//! the validator "returns ALL errors, not first-fail") rather than serde
//! aborting at the first one during parsing.

use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A type URN, stored verbatim and validated on demand.
///
/// Validation is deferred to [`Urn::validate`] rather than happening at
/// deserialize time, so a manifest with several malformed URNs gets every
/// one reported by [`crate::Manifest::validate`] in a single pass instead
/// of failing to parse on the first.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Urn(String);

impl Urn {
    /// Wrap a string as a URN without validating it.
    ///
    /// Use [`Urn::parse`] instead when the caller wants a syntax error
    /// surfaced immediately rather than deferred to [`Urn::validate`].
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Wrap and immediately validate a URN string.
    ///
    /// # Errors
    ///
    /// Returns [`UrnError`] if `raw` does not match the
    /// `<ns>(/<ns>)*/v<digits>/<Type>[params]` grammar.
    pub fn parse(raw: impl Into<String>) -> Result<Self, UrnError> {
        let urn = Self::new(raw);
        urn.validate()?;
        Ok(urn)
    }

    /// The raw URN string, exactly as written in the manifest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Check the URN against the type-URN grammar without allocating a
    /// parsed representation.
    ///
    /// # Errors
    ///
    /// Returns [`UrnError::Malformed`] with a human-readable reason if the
    /// string does not match `<ns>(/<ns>)*/v<digits>/<Type>[k=v,...]`.
    pub fn validate(&self) -> Result<(), UrnError> {
        match urn_regex() {
            Some(re) if re.is_match(&self.0) => Ok(()),
            Some(_) => Err(UrnError::Malformed {
                urn: self.0.clone(),
                reason: MALFORMED_REASON.to_string(),
            }),
            // The pattern is a fixed, unit-tested constant; this branch is
            // unreachable in practice but must be handled without a panic.
            None => Err(UrnError::PatternUnavailable),
        }
    }

    /// Whether this URN is syntactically valid, without constructing an
    /// error value.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// Parse the URN into its structural parts.
    ///
    /// # Errors
    ///
    /// Returns [`UrnError`] under the same conditions as
    /// [`Urn::validate`] — a malformed URN has no parts to extract.
    pub fn parts(&self) -> Result<UrnParts, UrnError> {
        self.validate()?;
        let Some(caps) = urn_regex().and_then(|re| re.captures(&self.0)) else {
            return Err(UrnError::PatternUnavailable);
        };
        // `ns`, `ver`, `type` are mandatory (non-optional) capture groups in
        // `URN_PATTERN`; they always participate once `self.validate()`
        // above has confirmed a full match. `.name(..)` is still used
        // (rather than the panicking `Index` impl) so this stays
        // panic-free even if that invariant is ever violated by a future
        // pattern edit.
        let ns = caps
            .name("ns")
            .ok_or(UrnError::PatternUnavailable)?
            .as_str();
        let ver = caps
            .name("ver")
            .ok_or(UrnError::PatternUnavailable)?
            .as_str();
        let type_name = caps
            .name("type")
            .ok_or(UrnError::PatternUnavailable)?
            .as_str()
            .to_string();

        let namespace: Vec<String> = ns.split('/').map(str::to_string).collect();
        let version: u16 = ver.parse().map_err(|_| UrnError::VersionOutOfRange {
            urn: self.0.clone(),
        })?;
        let params = caps
            .name("params")
            .map(|m| {
                m.as_str()
                    .split(',')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(UrnParts {
            namespace,
            version,
            type_name,
            params,
        })
    }
}

impl fmt::Display for Urn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Urn> for String {
    fn from(urn: Urn) -> Self {
        urn.0
    }
}

impl AsRef<str> for Urn {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The structural parts of a validated [`Urn`]: `<namespace>/v<version>/<type_name>[params]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrnParts {
    /// The namespace path segments before the version, e.g.
    /// `["std", "vision"]` for `std/vision/v1/Detections`.
    pub namespace: Vec<String>,
    /// The version number, e.g. `1` for `.../v1/...`.
    pub version: u16,
    /// The type name, e.g. `Detections`.
    pub type_name: String,
    /// Bracketed `key=value` parameters, e.g. `[("pixel", "rgb8")]` for
    /// `Image[pixel=rgb8]`. Empty when the URN has no bracket suffix.
    pub params: Vec<(String, String)>,
}

/// An error raised while validating or parsing a [`Urn`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UrnError {
    /// The URN does not match the `<ns>(/<ns>)*/v<digits>/<Type>[params]` grammar.
    #[error("malformed type URN `{urn}`: {reason}")]
    Malformed {
        /// The offending URN string.
        urn: String,
        /// A human-readable explanation of the expected grammar.
        reason: String,
    },
    /// The version segment (`v<digits>`) does not fit in a `u16`.
    #[error("type URN `{urn}` has a version number that does not fit in u16")]
    VersionOutOfRange {
        /// The offending URN string.
        urn: String,
    },
    /// The compiled-in URN regex failed to build.
    ///
    /// This can only happen if the hardcoded pattern itself is invalid,
    /// which this crate's `urn_pattern_compiles` test covers — it should
    /// never occur outside a broken build of this crate.
    #[error("internal error: the URN validation pattern failed to compile")]
    PatternUnavailable,
}

const MALFORMED_REASON: &str = "expected `<namespace>/v<version>/<Type>` or `<namespace>/v<version>/<Type>[k=v,...]`, \
     e.g. `std/vision/v1/Detections` or `std/media/v1/Image[pixel=rgb8]`";

/// The URN grammar, as a single fixed pattern:
///
/// ```text
/// <ns>(/<ns>)*/v<digits>/<Type>[k=v(,k=v)*]?
/// ```
///
/// `ns` segments and the type name start with a letter and continue with
/// letters/digits (plus `_`/`-` for namespace segments, to allow
/// hyphenated organization names); param values allow letters, digits,
/// `_.:+-` to cover pixel formats (`rgb8`), semantic tags, and future
/// extension without over-constraining.
const URN_PATTERN: &str = concat!(
    r"^(?P<ns>[A-Za-z][A-Za-z0-9_-]*(?:/[A-Za-z][A-Za-z0-9_-]*)*)",
    r"/v(?P<ver>[0-9]+)",
    r"/(?P<type>[A-Za-z][A-Za-z0-9_]*)",
    r"(?:\[(?P<params>[A-Za-z][A-Za-z0-9_]*=[A-Za-z0-9_.:+-]+(?:,[A-Za-z][A-Za-z0-9_]*=[A-Za-z0-9_.:+-]+)*)\])?$",
);

/// Lazily compile [`URN_PATTERN`] once per process.
///
/// Returns `None` only if the hardcoded pattern is itself invalid regex,
/// which this crate's `urn_pattern_compiles` test guards against — this
/// keeps every caller panic-free (no `.unwrap()`/`.expect()` on
/// `Regex::new`) without threading a `Result` through every URN check.
fn urn_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(URN_PATTERN).ok()).as_ref()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn urn_pattern_compiles() {
        assert!(urn_regex().is_some(), "URN_PATTERN must be valid regex");
    }

    #[test]
    fn accepts_blueprint_examples() {
        for example in [
            "std/vision/v1/Detections",
            "std/media/v1/Image[pixel=rgb8]",
            "std/sensor/v1/PointCloud[fields=xyz]",
            "std/core/v1/Int8",
            "std/core/v1/UInt64",
            "std/core/v1/Float16",
            "std/geometry/v1/Vector3",
            "std/time/v1/Timestamp",
        ] {
            assert!(Urn::new(example).is_valid(), "expected valid: {example}");
        }
    }

    #[test]
    fn rejects_missing_version() {
        assert!(!Urn::new("std/vision/Detections").is_valid());
    }

    #[test]
    fn rejects_missing_namespace() {
        assert!(!Urn::new("v1/Detections").is_valid());
    }

    #[test]
    fn rejects_lowercase_type_leading_digit() {
        assert!(!Urn::new("std/vision/v1/1Detections").is_valid());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(!Urn::new("").is_valid());
    }

    #[test]
    fn rejects_malformed_params() {
        assert!(!Urn::new("std/media/v1/Image[pixel]").is_valid());
        assert!(!Urn::new("std/media/v1/Image[=rgb8]").is_valid());
        assert!(!Urn::new("std/media/v1/Image[pixel=]").is_valid());
    }

    #[test]
    fn parts_extracts_namespace_version_type() {
        let parts = Urn::new("std/vision/v1/Detections").parts().unwrap();
        assert_eq!(
            parts.namespace,
            vec!["std".to_string(), "vision".to_string()]
        );
        assert_eq!(parts.version, 1);
        assert_eq!(parts.type_name, "Detections");
        assert!(parts.params.is_empty());
    }

    #[test]
    fn parts_extracts_multiple_params() {
        let parts = Urn::new("std/sensor/v1/PointCloud[fields=xyz,dtype=f32]")
            .parts()
            .unwrap();
        assert_eq!(
            parts.params,
            vec![
                ("fields".to_string(), "xyz".to_string()),
                ("dtype".to_string(), "f32".to_string()),
            ]
        );
    }

    #[test]
    fn parts_on_malformed_urn_errors() {
        assert!(Urn::new("not-a-urn").parts().is_err());
    }

    #[test]
    fn parse_rejects_malformed_input() {
        assert!(Urn::parse("nope").is_err());
        assert!(Urn::parse("std/vision/v1/Detections").is_ok());
    }

    #[test]
    fn deserialize_never_fails_on_malformed_syntax() {
        // Per the module docs: syntax validation is deferred to the
        // validation pass, so deserialization must accept any string.
        let urn: Urn = astrs_yaml::from_str("\"totally not a urn\"").unwrap();
        assert!(!urn.is_valid());
    }

    #[test]
    fn round_trips_through_yaml() {
        let urn = Urn::new("std/vision/v1/Detections");
        let yaml = astrs_yaml::to_string(&urn).unwrap();
        let back: Urn = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(urn, back);
    }
}
