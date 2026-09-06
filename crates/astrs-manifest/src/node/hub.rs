//! The `hub:` source kind: a node or operator fetched from the AstRS package
//! index by name (blueprint §22's "hub/package index").
//!
//! A `git:` source names a repository, a ref selector and a build command; a
//! `hub:` source names a package the index already knows how to fetch and
//! resolve. That is the whole difference at the manifest level — everything
//! about *how* a named package becomes an executable belongs to the hub
//! client, not to this crate.
//!
//! # Two spellings, one type
//!
//! Both of these parse to the same [`HubSource`]:
//!
//! ```yaml
//! hub: yolo-detector          # latest
//! hub: yolo-detector@v0.3.1   # pinned
//! ```
//!
//! ```yaml
//! hub:
//!   name: yolo-detector
//!   rev: v0.3.1
//! ```
//!
//! The string form is what anyone writing a manifest by hand actually types,
//! and it is what this crate re-emits whenever it round-trips (see
//! [`HubSource::is_short_form`]) — the same short-form-preserving rule
//! [`crate::Input`] follows for `node/output`. The structured form exists
//! because a generated or programmatically-edited manifest should not have to
//! do string surgery to change a pinned revision, and because it leaves room
//! for hub fields a later revision may add without breaking the `@`-separated
//! grammar.
//!
//! # `@` splits at the *first* one
//!
//! A package name may not contain `@` (the charset is `[a-z0-9-]`), but a
//! revision may — an OCI-style `sha256@...` digest, for instance. Splitting
//! at the first `@` therefore reads `pkg@sha256@abc` as `pkg` at revision
//! `sha256@abc`: everything before the first separator is the name, and
//! everything after it is the revision, whatever it contains. Splitting at
//! the last one instead would hand the name a character its own charset
//! forbids.

use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// The separator between a hub package name and its revision in the short
/// string form (`name@rev`).
pub const HUB_REV_SEPARATOR: char = '@';

/// A node or operator source fetched from the AstRS package index by name.
///
/// Deserializes from either form the manifest allows — see this module's docs:
///
/// - **Short form** — `"name"` or `"name@rev"`.
/// - **Long form** — a map with a required `name` and an optional `rev`.
///
/// Round-trips back to the short form whenever it can be spelled that way
/// (see [`HubSource::is_short_form`]), keeping `to_yaml` output as close to
/// hand-written as possible.
///
/// The `name` charset (`[a-z0-9-]`, non-empty) is checked by
/// [`crate::Manifest::validate`], not by `Deserialize`: this crate reports
/// every structural problem in one pass rather than failing at the first (see
/// the validation pass's own module docs). `rev` is deliberately unconstrained —
/// a tag, a branch, a commit hash and a content digest are all legitimate,
/// and the hub client is the only thing that can say which of them resolves.
///
/// # Examples
///
/// ```
/// use astrs_manifest::{HubSource, Manifest};
///
/// let yaml = "\
/// nodes:
///   - id: detector
///     hub: yolo-detector@v0.3.1
///     outputs: [detections]
/// ";
/// let manifest = Manifest::from_yaml_str(yaml)?;
/// manifest.validate()?;
///
/// let hub = manifest.nodes[0].hub.as_ref().ok_or("expected a hub source")?;
/// assert_eq!(hub, &HubSource::pinned("yolo-detector", "v0.3.1"));
/// assert_eq!(hub.name, "yolo-detector");
/// assert_eq!(hub.rev.as_deref(), Some("v0.3.1"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubSource {
    /// The package name in the index. Charset `[a-z0-9-]`, non-empty
    /// (checked by [`crate::Manifest::validate`]).
    pub name: String,
    /// The revision to fetch — a tag, branch, commit or digest, opaque to
    /// this crate. [`None`] means "whatever the index calls latest".
    pub rev: Option<String>,
}

impl HubSource {
    /// A hub source at the index's latest revision.
    #[must_use]
    pub fn latest(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rev: None,
        }
    }

    /// A hub source pinned to an explicit revision.
    #[must_use]
    pub fn pinned(name: impl Into<String>, rev: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rev: Some(rev.into()),
        }
    }

    /// Parse the short `name[@rev]` string form.
    ///
    /// Never fails: a name that is empty or carries illegal characters still
    /// parses here and is rejected by [`crate::Manifest::validate`] with a
    /// document path pointing at it, exactly like every other structural
    /// problem in this crate. An empty revision (`"pkg@"`) parses as
    /// [`None`], since `@` with nothing after it says no more than omitting
    /// it does.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::HubSource;
    ///
    /// assert_eq!(HubSource::parse("yolo"), HubSource::latest("yolo"));
    /// assert_eq!(HubSource::parse("yolo@v1"), HubSource::pinned("yolo", "v1"));
    /// assert_eq!(HubSource::parse("yolo@"), HubSource::latest("yolo"));
    /// // Split at the FIRST `@`: a revision may contain one, a name may not.
    /// assert_eq!(
    ///     HubSource::parse("yolo@sha256@abc"),
    ///     HubSource::pinned("yolo", "sha256@abc")
    /// );
    /// ```
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.split_once(HUB_REV_SEPARATOR) {
            Some((name, rev)) if !rev.is_empty() => Self::pinned(name, rev),
            Some((name, _)) => Self::latest(name),
            None => Self::latest(value),
        }
    }

    /// Whether this source round-trips through the short `name[@rev]` string
    /// form.
    ///
    /// `false` only when the revision itself contains the separator, which
    /// [`HubSource::parse`] would read back correctly but which reads far more
    /// clearly as the long form.
    #[must_use]
    pub fn is_short_form(&self) -> bool {
        match &self.rev {
            Some(rev) => !rev.contains(HUB_REV_SEPARATOR),
            None => true,
        }
    }

    /// This source rendered in the short `name[@rev]` form.
    #[must_use]
    pub fn to_short_form(&self) -> String {
        match &self.rev {
            Some(rev) => format!("{}{HUB_REV_SEPARATOR}{rev}", self.name),
            None => self.name.clone(),
        }
    }

    /// Whether `name` satisfies the hub package charset: non-empty, and only
    /// lowercase ASCII letters, digits and `-`.
    ///
    /// The rule [`crate::Manifest::validate`] applies, exposed so a hub
    /// client can apply the identical check without re-deriving it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::HubSource;
    ///
    /// assert!(HubSource::is_valid_name("yolo-detector"));
    /// assert!(!HubSource::is_valid_name("Yolo"));       // no uppercase
    /// assert!(!HubSource::is_valid_name("yolo_det"));   // no underscores
    /// assert!(!HubSource::is_valid_name(""));           // never empty
    /// ```
    #[must_use]
    pub fn is_valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    /// Whether this source's own [`HubSource::name`] satisfies
    /// [`HubSource::is_valid_name`].
    #[must_use]
    pub fn has_valid_name(&self) -> bool {
        Self::is_valid_name(&self.name)
    }
}

impl fmt::Display for HubSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_short_form())
    }
}

/// The long-form shape of [`HubSource`], used both as the `visit_map` target
/// for [`HubSource`]'s hand-written `Deserialize` and as the source of its
/// long-form `Serialize`/`JsonSchema` representation. `deny_unknown_fields`
/// here is what enforces the "no stray keys" contract on the long form —
/// [`HubSource`] itself cannot derive that attribute because it has a
/// hand-written `Deserialize`. (The identical split [`crate::Input`] uses for
/// its own two-form shape.)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HubLongForm {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
}

impl From<HubLongForm> for HubSource {
    fn from(long: HubLongForm) -> Self {
        Self {
            name: long.name,
            rev: long.rev,
        }
    }
}

impl From<&HubSource> for HubLongForm {
    fn from(source: &HubSource) -> Self {
        Self {
            name: source.name.clone(),
            rev: source.rev.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for HubSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HubVisitor;

        impl<'de> Visitor<'de> for HubVisitor {
            type Value = HubSource;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a hub source given as a `name` or `name@rev` string, or a map with a \
                     required `name` field and an optional `rev`",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<HubSource, E> {
                Ok(HubSource::parse(v))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<HubSource, E> {
                Ok(HubSource::parse(&v))
            }

            fn visit_map<A>(self, map: A) -> Result<HubSource, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                // Delegate to `HubLongForm`'s derived `Deserialize` so the
                // long form gets `deny_unknown_fields` and serde's own crisp
                // "missing field `name`" / "unknown field `x`" messages.
                let long = HubLongForm::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(long.into())
            }
        }

        deserializer.deserialize_any(HubVisitor)
    }
}

impl Serialize for HubSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.is_short_form() {
            serializer.serialize_str(&self.to_short_form())
        } else {
            HubLongForm::from(self).serialize(serializer)
        }
    }
}

impl JsonSchema for HubSource {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "HubSource".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let short = generator.subschema_for::<String>();
        let long = generator.subschema_for::<HubLongForm>();
        json_schema!({
            "description": "An AstRS package-index source, given as a `name` or `name@rev` string, or as an object with `name` plus optional `rev`.",
            "oneOf": [short, long],
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn the_short_form_parses_with_and_without_a_revision() {
        let latest: HubSource = astrs_yaml::from_str("yolo-detector").unwrap();
        assert_eq!(latest, HubSource::latest("yolo-detector"));

        let pinned: HubSource = astrs_yaml::from_str("yolo-detector@v0.3.1").unwrap();
        assert_eq!(pinned, HubSource::pinned("yolo-detector", "v0.3.1"));
    }

    #[test]
    fn the_long_form_parses_with_and_without_a_revision() {
        let pinned: HubSource = astrs_yaml::from_str("name: yolo\nrev: v1\n").unwrap();
        assert_eq!(pinned, HubSource::pinned("yolo", "v1"));

        let latest: HubSource = astrs_yaml::from_str("name: yolo\n").unwrap();
        assert_eq!(latest, HubSource::latest("yolo"));
    }

    #[test]
    fn the_two_forms_are_the_same_value() {
        let short: HubSource = astrs_yaml::from_str("yolo@v1").unwrap();
        let long: HubSource = astrs_yaml::from_str("name: yolo\nrev: v1\n").unwrap();
        assert_eq!(short, long);
    }

    #[test]
    fn the_separator_splits_at_the_first_occurrence() {
        // A name may not contain `@`; a digest-style revision may. Splitting
        // at the last one would hand `name` a character its charset forbids.
        assert_eq!(
            HubSource::parse("yolo@sha256@abc"),
            HubSource::pinned("yolo", "sha256@abc")
        );
        assert!(HubSource::parse("yolo@sha256@abc").has_valid_name());
    }

    #[test]
    fn an_empty_revision_is_the_same_as_none() {
        assert_eq!(HubSource::parse("yolo@"), HubSource::latest("yolo"));
    }

    #[test]
    fn the_long_form_requires_a_name_and_rejects_unknown_fields() {
        let missing = astrs_yaml::from_str::<HubSource>("rev: v1\n").unwrap_err();
        assert!(missing.to_string().contains("name"), "error: {missing}");

        let stray = astrs_yaml::from_str::<HubSource>("name: yolo\nregistry: x\n").unwrap_err();
        assert!(stray.to_string().contains("registry"), "error: {stray}");
    }

    #[test]
    fn rejects_non_string_non_map_scalars() {
        assert!(astrs_yaml::from_str::<HubSource>("42").is_err());
        assert!(astrs_yaml::from_str::<HubSource>("true").is_err());
    }

    #[test]
    fn round_trips_as_the_short_form_whenever_it_can() {
        for value in [HubSource::latest("yolo"), HubSource::pinned("yolo", "v1")] {
            assert!(value.is_short_form(), "{value:?}");
            let yaml = astrs_yaml::to_string(&value).unwrap();
            assert!(!yaml.contains("name:"), "yaml was: {yaml}");
            assert_eq!(astrs_yaml::from_str::<HubSource>(&yaml).unwrap(), value);
        }
    }

    #[test]
    fn a_revision_containing_the_separator_round_trips_as_the_long_form() {
        let value = HubSource::pinned("yolo", "sha256@abc");
        assert!(!value.is_short_form());
        let yaml = astrs_yaml::to_string(&value).unwrap();
        assert!(yaml.contains("name: yolo"), "yaml was: {yaml}");
        assert!(yaml.contains("rev: sha256@abc"), "yaml was: {yaml}");
        assert_eq!(astrs_yaml::from_str::<HubSource>(&yaml).unwrap(), value);
    }

    #[test]
    fn the_name_charset_is_lowercase_alphanumeric_and_dashes() {
        for good in ["yolo", "yolo-detector", "y2", "a-b-c-1"] {
            assert!(HubSource::is_valid_name(good), "{good}");
        }
        for bad in [
            "",
            "Yolo",
            "yolo_detector",
            "yolo.detector",
            "yolo/det",
            "yolo ",
        ] {
            assert!(!HubSource::is_valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn has_valid_name_delegates_to_the_charset_rule() {
        assert!(HubSource::latest("yolo-detector").has_valid_name());
        assert!(!HubSource::latest("Yolo").has_valid_name());
    }

    #[test]
    fn display_is_the_short_form() {
        assert_eq!(HubSource::latest("yolo").to_string(), "yolo");
        assert_eq!(HubSource::pinned("yolo", "v1").to_string(), "yolo@v1");
    }
}
