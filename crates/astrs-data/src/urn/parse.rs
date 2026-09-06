//! [`TypeUrn`] — the port type identifier, and its grammar.
//!
//! # Grammar
//!
//! ```text
//!   urn        := "std" "/" category "/" "v" version "/" name params?
//!   category   := [a-z] [a-z0-9_]*
//!   version    := "0" | [1-9] [0-9]{0,4}          ; a u16, no leading zeros
//!   name       := [A-Z] [A-Za-z0-9]*
//!   params     := "[" param ("," param)* "]"
//!   param      := key "=" value
//!   key        := [a-z] [a-z0-9_]*
//!   value      := [A-Za-z0-9_.:+*-]+
//! ```
//!
//! Nothing is optional except the parameter list, and nothing is
//! whitespace-tolerant: a URN comes from a manifest or from the wire, and
//! silently accepting `" std/core/v1/Bool"` would let two spellings of one
//! type differ under `==`.
//!
//! # Canonical form
//!
//! Parsing **normalises**: parameters are stored in a [`BTreeMap`] and are
//! therefore re-emitted sorted by key. The round-trip that always holds is
//!
//! ```text
//!   parse(format(urn)) == urn
//! ```
//!
//! and *not* `format(parse(text)) == text`, which fails for any input whose
//! parameters were written out of order. [`TypeUrn::as_str`] returns the
//! canonical text, so equality, ordering and hashing are all one string
//! comparison over an already-normalised representation.
//!
//! ```
//! use astrs_data::urn::TypeUrn;
//!
//! let urn = TypeUrn::parse("std/media/v1/Image[width=640,pixel=rgb8]")?;
//! assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8,width=640]");
//! assert_eq!(urn.category(), "media");
//! assert_eq!(urn.version(), 1);
//! assert_eq!(urn.name(), "Image");
//! assert_eq!(urn.param("pixel"), Some("rgb8"));
//! # Ok::<(), astrs_data::TypeUrnError>(())
//! ```
//!
//! # Representation
//!
//! A `TypeUrn` owns one boxed string — the canonical text — plus byte spans
//! into it for the category and the name, so every accessor is a borrow and
//! none allocates. Parameters are additionally kept as an owned map because
//! lookups by key are the common operation and re-scanning the text for each
//! one would be quadratic in a graph validation pass.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::urn::error::TypeUrnError;

/// A byte range into [`TypeUrn::repr`].
///
/// `u16` bounds are sound because the whole string is capped at
/// [`TypeUrn::MAX_LEN`], which is far below `u16::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Span {
    start: u16,
    end: u16,
}

impl Span {
    /// Builds a span, saturating rather than wrapping if the impossible
    /// happens (the caller has already enforced [`TypeUrn::MAX_LEN`]).
    fn new(start: usize, end: usize) -> Self {
        Self {
            start: u16::try_from(start).unwrap_or(u16::MAX),
            end: u16::try_from(end).unwrap_or(u16::MAX),
        }
    }

    /// Slices `text` with this span, or returns `""` if the span is somehow
    /// out of range — keeping the accessors total and panic-free.
    fn slice(self, text: &str) -> &str {
        text.get(usize::from(self.start)..usize::from(self.end))
            .unwrap_or_default()
    }
}

/// The type identifier a port carries (blueprint §3.7, §24.3).
///
/// `std/<category>/v<version>/<Name>[key=value,...]`.
///
/// Cheap to compare and to hash — both go through the canonical string — and
/// cheap to clone only in the sense that a URN is small; a clone does copy the
/// text and the parameter map. Store one in an `Arc` if a graph pins millions.
///
/// ```
/// use astrs_data::urn::TypeUrn;
/// use std::collections::HashSet;
///
/// let a = TypeUrn::parse("std/core/v1/Float32")?;
/// let b = TypeUrn::try_new("core", 1, "Float32")?;
/// assert_eq!(a, b);
///
/// let mut set = HashSet::new();
/// set.insert(a);
/// assert!(set.contains(&b));
/// # Ok::<(), astrs_data::TypeUrnError>(())
/// ```
#[derive(Clone)]
pub struct TypeUrn {
    /// The canonical text, exactly what [`fmt::Display`] emits.
    repr: Box<str>,
    /// Byte span of the category within `repr`.
    category: Span,
    /// Byte span of the type name within `repr`.
    name: Span,
    /// The parsed version.
    version: u16,
    /// Parameters, sorted by key — the order `repr` uses.
    params: BTreeMap<Box<str>, Box<str>>,
}

impl TypeUrn {
    /// The only namespace 0.1.0 recognises.
    pub const NAMESPACE: &'static str = "std";

    /// Hard cap on the length of a URN, in bytes.
    ///
    /// URNs arrive from manifests and from the wire. Bounding the input up
    /// front means a hostile 10 MB "type name" costs one comparison, not an
    /// allocation.
    pub const MAX_LEN: usize = 512;

    /// Hard cap on the number of parameters one URN may carry.
    pub const MAX_PARAMS: usize = 16;

    /// Hard cap on the length of any one segment, in bytes.
    pub const MAX_SEGMENT_LEN: usize = 64;

    /// Parses a URN from its canonical (or an equivalent) textual form.
    ///
    /// # Errors
    ///
    /// One of the syntax variants of [`TypeUrnError`]; see the module docs for
    /// the grammar each one enforces.
    ///
    /// ```
    /// use astrs_data::urn::{TypeUrn, TypeUrnError};
    ///
    /// assert!(TypeUrn::parse("std/sensor/v1/PointCloud[fields=x:y:z]").is_ok());
    /// assert_eq!(TypeUrn::parse(""), Err(TypeUrnError::Empty));
    /// assert_eq!(
    ///     TypeUrn::parse("ros/core/v1/Bool"),
    ///     Err(TypeUrnError::UnsupportedNamespace { namespace: "ros".to_owned() })
    /// );
    /// assert_eq!(
    ///     TypeUrn::parse("std/core/v1"),
    ///     Err(TypeUrnError::SegmentCount { found: 3 })
    /// );
    /// ```
    pub fn parse(input: &str) -> Result<Self, TypeUrnError> {
        if input.len() > Self::MAX_LEN {
            return Err(TypeUrnError::TooLong {
                len: input.len(),
                max: Self::MAX_LEN,
            });
        }
        if input.trim().is_empty() {
            return Err(TypeUrnError::Empty);
        }

        let (head, params_text) = split_params(input)?;
        let segments: Vec<&str> = head.split('/').collect();
        let [namespace, category, version, name] = segments.as_slice() else {
            return Err(TypeUrnError::SegmentCount {
                found: segments.len(),
            });
        };

        if *namespace != Self::NAMESPACE {
            return Err(TypeUrnError::UnsupportedNamespace {
                namespace: (*namespace).to_owned(),
            });
        }
        if !is_identifier(category) {
            return Err(TypeUrnError::InvalidCategory {
                category: (*category).to_owned(),
            });
        }
        if !is_type_name(name) {
            return Err(TypeUrnError::InvalidName {
                name: (*name).to_owned(),
            });
        }
        let version = parse_version(version)?;
        let params = match params_text {
            Some(text) => parse_params(text)?,
            None => BTreeMap::new(),
        };

        Self::assemble(category, version, name, params)
    }

    /// Builds a parameter-free URN from its parts.
    ///
    /// # Errors
    ///
    /// [`TypeUrnError::InvalidCategory`], [`TypeUrnError::InvalidName`] or
    /// [`TypeUrnError::TooLong`].
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let urn = TypeUrn::try_new("geometry", 1, "Pose")?;
    /// assert_eq!(urn.as_str(), "std/geometry/v1/Pose");
    /// assert!(TypeUrn::try_new("Geometry", 1, "Pose").is_err());
    /// assert!(TypeUrn::try_new("geometry", 1, "pose").is_err());
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    pub fn try_new(category: &str, version: u16, name: &str) -> Result<Self, TypeUrnError> {
        Self::try_with_params::<&str, &str>(category, version, name, [])
    }

    /// Builds a URN from its parts, including parameters.
    ///
    /// Parameters are normalised exactly as [`TypeUrn::parse`] normalises
    /// them: validated, de-duplicated and sorted.
    ///
    /// # Errors
    ///
    /// Any of the syntax variants of [`TypeUrnError`].
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let urn = TypeUrn::try_with_params("media", 1, "Image", [("pixel", "rgb8")])?;
    /// assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8]");
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    pub fn try_with_params<K, V>(
        category: &str,
        version: u16,
        name: &str,
        params: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self, TypeUrnError>
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        if !is_identifier(category) {
            return Err(TypeUrnError::InvalidCategory {
                category: category.to_owned(),
            });
        }
        if !is_type_name(name) {
            return Err(TypeUrnError::InvalidName {
                name: name.to_owned(),
            });
        }

        let mut map = BTreeMap::new();
        for (key, value) in params {
            let (key, value) = (key.as_ref(), value.as_ref());
            check_param(key, value)?;
            if map.insert(Box::from(key), Box::from(value)).is_some() {
                return Err(TypeUrnError::DuplicateParam {
                    key: key.to_owned(),
                });
            }
            if map.len() > Self::MAX_PARAMS {
                return Err(TypeUrnError::TooManyParams {
                    found: map.len(),
                    max: Self::MAX_PARAMS,
                });
            }
        }

        Self::assemble(category, version, name, map)
    }

    /// Renders the canonical text and records the spans into it.
    ///
    /// The single construction point: every public constructor funnels here
    /// once its inputs are validated, so the canonical form has exactly one
    /// implementation.
    fn assemble(
        category: &str,
        version: u16,
        name: &str,
        params: BTreeMap<Box<str>, Box<str>>,
    ) -> Result<Self, TypeUrnError> {
        let params_len: usize = params
            .iter()
            .map(|(key, value)| key.len() + value.len() + 2)
            .sum();
        let mut repr = String::with_capacity(
            Self::NAMESPACE.len() + category.len() + name.len() + 9 + params_len,
        );

        repr.push_str(Self::NAMESPACE);
        repr.push('/');
        let category_start = repr.len();
        repr.push_str(category);
        let category_span = Span::new(category_start, repr.len());

        repr.push_str("/v");
        push_u16(&mut repr, version);
        repr.push('/');
        let name_start = repr.len();
        repr.push_str(name);
        let name_span = Span::new(name_start, repr.len());

        if !params.is_empty() {
            repr.push('[');
            for (index, (key, value)) in params.iter().enumerate() {
                if index > 0 {
                    repr.push(',');
                }
                repr.push_str(key);
                repr.push('=');
                repr.push_str(value);
            }
            repr.push(']');
        }

        if repr.len() > Self::MAX_LEN {
            return Err(TypeUrnError::TooLong {
                len: repr.len(),
                max: Self::MAX_LEN,
            });
        }

        Ok(Self {
            repr: repr.into_boxed_str(),
            category: category_span,
            name: name_span,
            version,
            params,
        })
    }

    /// The canonical text — what `Display`, `Serialize` and `as_str` all use.
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.repr
    }

    /// The category segment, e.g. `"sensor"`.
    #[inline]
    #[must_use]
    pub fn category(&self) -> &str {
        self.category.slice(&self.repr)
    }

    /// The version number, e.g. `1` for `v1`.
    #[inline]
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// The type name, e.g. `"PointCloud"`.
    #[inline]
    #[must_use]
    pub fn name(&self) -> &str {
        self.name.slice(&self.repr)
    }

    /// Every parameter, sorted by key.
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let urn = TypeUrn::parse("std/media/v1/AudioFrame[rate=48000,sample=f32]")?;
    /// let keys: Vec<&str> = urn.params().keys().map(|k| &**k).collect();
    /// assert_eq!(keys, vec!["rate", "sample"]);
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    #[inline]
    #[must_use]
    pub const fn params(&self) -> &BTreeMap<Box<str>, Box<str>> {
        &self.params
    }

    /// One parameter's value, or `None`.
    #[inline]
    #[must_use]
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(Borrow::borrow)
    }

    /// Number of parameters.
    #[inline]
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.params.len()
    }

    /// Returns `true` when the URN carries at least one parameter.
    #[inline]
    #[must_use]
    pub fn has_params(&self) -> bool {
        !self.params.is_empty()
    }

    /// The URN text with the parameter list stripped.
    ///
    /// This is the registry key: `std/media/v1/Image[pixel=rgb8]` and
    /// `std/media/v1/Image[pixel=mono8]` resolve to the same entry.
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let urn = TypeUrn::parse("std/media/v1/Image[pixel=rgb8]")?;
    /// assert_eq!(urn.base_str(), "std/media/v1/Image");
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn base_str(&self) -> &str {
        let end = usize::from(self.name.end);
        self.repr.get(..end).unwrap_or(&self.repr)
    }

    /// The same type with every parameter removed.
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let urn = TypeUrn::parse("std/media/v1/Image[pixel=rgb8]")?;
    /// assert_eq!(urn.base().as_str(), "std/media/v1/Image");
    /// assert!(!urn.base().has_params());
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    #[must_use]
    pub fn base(&self) -> Self {
        if !self.has_params() {
            return self.clone();
        }
        let base = self.base_str();
        Self {
            repr: Box::from(base),
            category: self.category,
            name: self.name,
            version: self.version,
            params: BTreeMap::new(),
        }
    }

    /// Returns `true` when both URNs name the same type, whatever their
    /// parameters.
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let rgb = TypeUrn::parse("std/media/v1/Image[pixel=rgb8]")?;
    /// let mono = TypeUrn::parse("std/media/v1/Image[pixel=mono8]")?;
    /// assert!(rgb.base_eq(&mono));
    /// assert_ne!(rgb, mono);
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn base_eq(&self, other: &Self) -> bool {
        self.base_str() == other.base_str()
    }

    /// A copy carrying one more parameter, replacing any previous value.
    ///
    /// # Errors
    ///
    /// [`TypeUrnError::InvalidParamKey`], [`TypeUrnError::InvalidParamValue`],
    /// [`TypeUrnError::EmptyParamValue`], [`TypeUrnError::TooManyParams`] or
    /// [`TypeUrnError::TooLong`].
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let urn = TypeUrn::try_new("media", 1, "Image")?.with_param("pixel", "rgb8")?;
    /// assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8]");
    ///
    /// // Replacing keeps the canonical single-entry form.
    /// let urn = urn.with_param("pixel", "mono8")?;
    /// assert_eq!(urn.param("pixel"), Some("mono8"));
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    pub fn with_param(self, key: &str, value: &str) -> Result<Self, TypeUrnError> {
        check_param(key, value)?;
        let category = self.category().to_owned();
        let name = self.name().to_owned();
        let mut params = self.params;
        params.insert(Box::from(key), Box::from(value));
        if params.len() > Self::MAX_PARAMS {
            return Err(TypeUrnError::TooManyParams {
                found: params.len(),
                max: Self::MAX_PARAMS,
            });
        }
        Self::assemble(&category, self.version, &name, params)
    }

    /// A copy with one parameter removed. Removing an absent key is a no-op.
    ///
    /// # Errors
    ///
    /// [`TypeUrnError::TooLong`] cannot occur here in practice — the result is
    /// never longer than the input — but the constructor is fallible and the
    /// error is propagated rather than swallowed.
    pub fn without_param(self, key: &str) -> Result<Self, TypeUrnError> {
        if !self.params.contains_key(key) {
            return Ok(self);
        }
        let category = self.category().to_owned();
        let name = self.name().to_owned();
        let mut params = self.params;
        params.remove(key);
        Self::assemble(&category, self.version, &name, params)
    }
}

impl fmt::Display for TypeUrn {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.repr)
    }
}

impl fmt::Debug for TypeUrn {
    /// Prints the canonical text, so a `{:?}` in a graph diagnostic reads as
    /// the URN rather than as five struct fields.
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TypeUrn({})", self.repr)
    }
}

impl PartialEq for TypeUrn {
    /// Canonical-text equality. The spans and the parameter map are derived
    /// from the text, so comparing the text compares everything.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.repr == other.repr
    }
}

impl Eq for TypeUrn {}

impl std::hash::Hash for TypeUrn {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.repr.hash(state);
    }
}

impl PartialOrd for TypeUrn {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TypeUrn {
    /// Lexicographic over the canonical text, which groups a category's types
    /// together — the order `astrs types list` prints.
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.repr.cmp(&other.repr)
    }
}

impl AsRef<str> for TypeUrn {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.repr
    }
}

impl Borrow<str> for TypeUrn {
    #[inline]
    fn borrow(&self) -> &str {
        &self.repr
    }
}

impl FromStr for TypeUrn {
    type Err = TypeUrnError;

    #[inline]
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl TryFrom<&str> for TypeUrn {
    type Error = TypeUrnError;

    #[inline]
    fn try_from(text: &str) -> Result<Self, Self::Error> {
        Self::parse(text)
    }
}

impl serde::Serialize for TypeUrn {
    /// Serialises as the canonical string, so a manifest round-trips through
    /// YAML or JSON unchanged.
    ///
    /// ```
    /// use astrs_data::urn::TypeUrn;
    ///
    /// let urn = TypeUrn::parse("std/core/v1/Float32")?;
    /// assert_eq!(
    ///     serde_json::to_string(&urn).ok().as_deref(),
    ///     Some("\"std/core/v1/Float32\"")
    /// );
    /// # Ok::<(), astrs_data::TypeUrnError>(())
    /// ```
    #[inline]
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.repr)
    }
}

impl<'de> serde::Deserialize<'de> for TypeUrn {
    /// Deserialises from a string, running the full parser — a malformed URN
    /// in a manifest is a deserialisation error, not a value to fix later.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(TypeUrnVisitor)
    }
}

/// Accepts both borrowed and owned strings from any self-describing format.
struct TypeUrnVisitor;

impl serde::de::Visitor<'_> for TypeUrnVisitor {
    type Value = TypeUrn;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a type URN of the form \"std/<category>/v<n>/<Name>[k=v,...]\"")
    }

    fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
        TypeUrn::parse(text).map_err(E::custom)
    }

    fn visit_string<E: serde::de::Error>(self, text: String) -> Result<Self::Value, E> {
        TypeUrn::parse(&text).map_err(E::custom)
    }
}

/// Splits the parameter list off the end of the input.
///
/// Returns the head (everything before `[`) and the bracket contents.
fn split_params(input: &str) -> Result<(&str, Option<&str>), TypeUrnError> {
    let Some(open) = input.find('[') else {
        return Ok((input, None));
    };
    let Some(close) = input.rfind(']') else {
        return Err(TypeUrnError::UnterminatedParams);
    };
    if close < open {
        return Err(TypeUrnError::UnterminatedParams);
    }
    if close + 1 != input.len() {
        return Err(TypeUrnError::TrailingData {
            rest: input.get(close + 1..).unwrap_or_default().to_owned(),
        });
    }
    let head = input.get(..open).unwrap_or_default();
    // `open < close` and both are ASCII byte positions, so this range is a
    // valid slice; `get` keeps the function total regardless.
    let inner = input.get(open + 1..close).unwrap_or_default();
    Ok((head, Some(inner)))
}

/// Parses the contents of a `[...]` list into the normalised parameter map.
fn parse_params(inner: &str) -> Result<BTreeMap<Box<str>, Box<str>>, TypeUrnError> {
    if inner.is_empty() {
        return Err(TypeUrnError::EmptyParams);
    }
    let mut map: BTreeMap<Box<str>, Box<str>> = BTreeMap::new();
    for entry in inner.split(',') {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(TypeUrnError::MalformedParam {
                param: entry.to_owned(),
            });
        };
        check_param(key, value)?;
        if map.insert(Box::from(key), Box::from(value)).is_some() {
            return Err(TypeUrnError::DuplicateParam {
                key: key.to_owned(),
            });
        }
        if map.len() > TypeUrn::MAX_PARAMS {
            return Err(TypeUrnError::TooManyParams {
                found: map.len(),
                max: TypeUrn::MAX_PARAMS,
            });
        }
    }
    Ok(map)
}

/// Validates one `key=value` pair against the grammar.
fn check_param(key: &str, value: &str) -> Result<(), TypeUrnError> {
    if key.is_empty() {
        return Err(TypeUrnError::EmptyParamKey);
    }
    if !is_identifier(key) {
        return Err(TypeUrnError::InvalidParamKey {
            key: key.to_owned(),
        });
    }
    if value.is_empty() {
        return Err(TypeUrnError::EmptyParamValue {
            key: key.to_owned(),
        });
    }
    if !is_param_value(value) {
        return Err(TypeUrnError::InvalidParamValue {
            key: key.to_owned(),
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Parses the `v<u16>` segment.
fn parse_version(text: &str) -> Result<u16, TypeUrnError> {
    let reject = || TypeUrnError::InvalidVersion {
        version: text.to_owned(),
    };
    let digits = text.strip_prefix('v').ok_or_else(reject)?;
    if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(reject());
    }
    // Reject `v01`: two spellings of one version would break the canonical
    // form, and with it equality and the registry key.
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(reject());
    }
    digits.parse::<u16>().map_err(|_| reject())
}

/// `[a-z][a-z0-9_]*`, bounded by [`TypeUrn::MAX_SEGMENT_LEN`].
fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= TypeUrn::MAX_SEGMENT_LEN
        && matches!(text.bytes().next(), Some(byte) if byte.is_ascii_lowercase())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// `[A-Z][A-Za-z0-9]*`, bounded by [`TypeUrn::MAX_SEGMENT_LEN`].
fn is_type_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= TypeUrn::MAX_SEGMENT_LEN
        && matches!(text.bytes().next(), Some(byte) if byte.is_ascii_uppercase())
        && text.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// `[A-Za-z0-9_.:+*-]+`, bounded by [`TypeUrn::MAX_SEGMENT_LEN`].
///
/// Deliberately narrow. A parameter value ends up in a manifest, in a log
/// line, in a mermaid label and in a URL query, and an allow-list that
/// survives all four unquoted is worth more than the freedom to write
/// `pixel="rgb 8"`. `:` is the inner separator for multi-valued parameters
/// (`fields=x:y:z`), since `,` already separates parameters.
fn is_param_value(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= TypeUrn::MAX_SEGMENT_LEN
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'+' | b'*' | b'-')
        })
}

/// Appends a `u16` in decimal without allocating.
///
/// The registry parses and re-renders every standard URN at start-up; a
/// `to_string()` per row would be pure waste on a path that runs before the
/// first message is ever sent.
fn push_u16(out: &mut String, value: u16) {
    // `u16::MAX` is 65535 — five digits, never more.
    let mut digits = [0u8; 5];
    let mut index = digits.len();
    let mut rest = value;
    loop {
        index = index.saturating_sub(1);
        let digit = u8::try_from(rest % 10).unwrap_or(0);
        if let Some(slot) = digits.get_mut(index) {
            *slot = b'0' + digit;
        }
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    debug_assert!(index < digits.len(), "at least one digit was written");
    for &byte in digits.get(index..).unwrap_or_default() {
        out.push(char::from(byte));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn parses_the_minimal_form() {
        let urn = TypeUrn::parse("std/core/v1/Bool").unwrap();
        assert_eq!(urn.as_str(), "std/core/v1/Bool");
        assert_eq!(urn.category(), "core");
        assert_eq!(urn.version(), 1);
        assert_eq!(urn.name(), "Bool");
        assert!(!urn.has_params());
        assert_eq!(urn.param_count(), 0);
        assert_eq!(urn.base_str(), "std/core/v1/Bool");
    }

    #[test]
    fn parses_parameters_and_normalises_their_order() {
        let urn = TypeUrn::parse("std/media/v1/Image[width=640,pixel=rgb8,height=480]").unwrap();
        assert_eq!(
            urn.as_str(),
            "std/media/v1/Image[height=480,pixel=rgb8,width=640]"
        );
        assert_eq!(urn.param("pixel"), Some("rgb8"));
        assert_eq!(urn.param("height"), Some("480"));
        assert_eq!(urn.param("missing"), None);
        assert_eq!(urn.param_count(), 3);
        assert!(urn.has_params());
    }

    #[test]
    fn canonical_round_trip_holds() {
        for text in [
            "std/core/v1/Int64",
            "std/time/v1/Duration",
            "std/media/v1/Image[pixel=rgb8]",
            "std/sensor/v1/PointCloud[fields=x:y:z,frame=base_link]",
            "std/core/v0/Empty",
            "std/core/v65535/Bytes",
        ] {
            let urn = TypeUrn::parse(text).unwrap();
            assert_eq!(urn.as_str(), text, "already canonical");
            let again = TypeUrn::parse(urn.as_str()).unwrap();
            assert_eq!(urn, again);
        }
    }

    #[test]
    fn every_version_digit_count_renders() {
        for version in [0u16, 1, 9, 10, 99, 100, 999, 1000, 9999, 10000, 65535] {
            let urn = TypeUrn::try_new("core", version, "Bool").unwrap();
            assert_eq!(urn.as_str(), format!("std/core/v{version}/Bool"));
            assert_eq!(urn.version(), version);
        }
    }

    #[test]
    fn empty_and_blank_are_rejected() {
        assert_eq!(TypeUrn::parse(""), Err(TypeUrnError::Empty));
        assert_eq!(TypeUrn::parse("   "), Err(TypeUrnError::Empty));
        assert_eq!(TypeUrn::parse("\t\n"), Err(TypeUrnError::Empty));
    }

    #[test]
    fn over_long_input_is_rejected_before_any_work() {
        let text = "x".repeat(TypeUrn::MAX_LEN + 1);
        assert_eq!(
            TypeUrn::parse(&text),
            Err(TypeUrnError::TooLong {
                len: TypeUrn::MAX_LEN + 1,
                max: TypeUrn::MAX_LEN,
            })
        );
    }

    #[test]
    fn segment_count_is_exact() {
        assert_eq!(
            TypeUrn::parse("std/core/v1"),
            Err(TypeUrnError::SegmentCount { found: 3 })
        );
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool/extra"),
            Err(TypeUrnError::SegmentCount { found: 5 })
        );
        assert_eq!(
            TypeUrn::parse("Bool"),
            Err(TypeUrnError::SegmentCount { found: 1 })
        );
    }

    #[test]
    fn namespace_is_closed() {
        assert_eq!(
            TypeUrn::parse("ros/core/v1/Bool"),
            Err(TypeUrnError::UnsupportedNamespace {
                namespace: "ros".to_owned()
            })
        );
        assert_eq!(
            TypeUrn::parse("STD/core/v1/Bool"),
            Err(TypeUrnError::UnsupportedNamespace {
                namespace: "STD".to_owned()
            })
        );
    }

    #[test]
    fn whitespace_is_not_tolerated() {
        assert!(TypeUrn::parse(" std/core/v1/Bool").is_err());
        assert!(TypeUrn::parse("std/core/v1/Bool ").is_err());
        assert!(TypeUrn::parse("std/ core/v1/Bool").is_err());
        assert!(TypeUrn::parse("std/core/v1/Bo ol").is_err());
    }

    #[test]
    fn category_must_be_a_lowercase_identifier() {
        for bad in ["", "Core", "1core", "core-x", "core.x", "cör"] {
            assert!(
                matches!(
                    TypeUrn::parse(&format!("std/{bad}/v1/Bool")),
                    Err(TypeUrnError::InvalidCategory { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
        assert!(TypeUrn::parse("std/point_cloud/v1/Bool").is_ok());
        assert!(TypeUrn::parse("std/core2/v1/Bool").is_ok());
    }

    #[test]
    fn name_must_be_upper_camel() {
        for bad in ["", "bool", "_Bool", "1Bool", "Bo-ol", "Bo_ol"] {
            assert!(
                matches!(
                    TypeUrn::parse(&format!("std/core/v1/{bad}")),
                    Err(TypeUrnError::InvalidName { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
        assert!(TypeUrn::parse("std/core/v1/UInt64").is_ok());
        assert!(TypeUrn::parse("std/core/v1/Float16").is_ok());
    }

    #[test]
    fn version_grammar_is_strict() {
        for bad in [
            "1", "V1", "v", "v-1", "v1x", "v01", "v001", "v65536", "v123456",
        ] {
            assert!(
                matches!(
                    TypeUrn::parse(&format!("std/core/{bad}/Bool")),
                    Err(TypeUrnError::InvalidVersion { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
        assert_eq!(TypeUrn::parse("std/core/v0/Bool").unwrap().version(), 0);
        assert_eq!(
            TypeUrn::parse("std/core/v65535/Bool").unwrap().version(),
            65535
        );
    }

    #[test]
    fn parameter_brackets_must_be_well_formed() {
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[a=b"),
            Err(TypeUrnError::UnterminatedParams)
        );
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[a=b]x"),
            Err(TypeUrnError::TrailingData {
                rest: "x".to_owned()
            })
        );
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[]"),
            Err(TypeUrnError::EmptyParams)
        );
        // A stray `]` with no `[` falls through to the name check.
        assert!(matches!(
            TypeUrn::parse("std/core/v1/Bool]"),
            Err(TypeUrnError::InvalidName { .. })
        ));
    }

    #[test]
    fn parameter_entries_must_be_key_value_pairs() {
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[novalue]"),
            Err(TypeUrnError::MalformedParam {
                param: "novalue".to_owned()
            })
        );
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[a=b,,c=d]"),
            Err(TypeUrnError::MalformedParam {
                param: String::new()
            })
        );
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[=v]"),
            Err(TypeUrnError::EmptyParamKey)
        );
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[k=]"),
            Err(TypeUrnError::EmptyParamValue {
                key: "k".to_owned()
            })
        );
    }

    #[test]
    fn parameter_charsets_are_enforced() {
        assert!(matches!(
            TypeUrn::parse("std/core/v1/Bool[Key=v]"),
            Err(TypeUrnError::InvalidParamKey { .. })
        ));
        assert!(matches!(
            TypeUrn::parse("std/core/v1/Bool[k-1=v]"),
            Err(TypeUrnError::InvalidParamKey { .. })
        ));
        // `=` inside a value is a value-charset failure, not a parse ambiguity.
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[k=a=b]"),
            Err(TypeUrnError::InvalidParamValue {
                key: "k".to_owned(),
                value: "a=b".to_owned(),
            })
        );
        assert!(matches!(
            TypeUrn::parse("std/core/v1/Bool[k=a b]"),
            Err(TypeUrnError::InvalidParamValue { .. })
        ));
        assert!(matches!(
            TypeUrn::parse("std/core/v1/Bool[k=a/b]"),
            Err(TypeUrnError::InvalidParamValue { .. })
        ));
        // `:` is the documented inner separator and must survive.
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[k=x:y:z]")
                .unwrap()
                .param("k"),
            Some("x:y:z")
        );
    }

    #[test]
    fn duplicate_parameters_are_rejected() {
        assert_eq!(
            TypeUrn::parse("std/core/v1/Bool[k=1,k=2]"),
            Err(TypeUrnError::DuplicateParam {
                key: "k".to_owned()
            })
        );
    }

    #[test]
    fn parameter_count_is_capped() {
        let params: Vec<String> = (0..=TypeUrn::MAX_PARAMS)
            .map(|index| format!("k{index}=v"))
            .collect();
        let text = format!("std/core/v1/Bool[{}]", params.join(","));
        assert_eq!(
            TypeUrn::parse(&text),
            Err(TypeUrnError::TooManyParams {
                found: TypeUrn::MAX_PARAMS + 1,
                max: TypeUrn::MAX_PARAMS,
            })
        );
    }

    #[test]
    fn segment_lengths_are_capped() {
        let long = "a".repeat(TypeUrn::MAX_SEGMENT_LEN + 1);
        assert!(matches!(
            TypeUrn::parse(&format!("std/{long}/v1/Bool")),
            Err(TypeUrnError::InvalidCategory { .. })
        ));
        let long_name = format!("A{}", "b".repeat(TypeUrn::MAX_SEGMENT_LEN));
        assert!(matches!(
            TypeUrn::parse(&format!("std/core/v1/{long_name}")),
            Err(TypeUrnError::InvalidName { .. })
        ));
    }

    #[test]
    fn try_new_matches_parse() {
        let built = TypeUrn::try_new("sensor", 1, "Imu").unwrap();
        let parsed = TypeUrn::parse("std/sensor/v1/Imu").unwrap();
        assert_eq!(built, parsed);
        assert_eq!(built.category(), parsed.category());
        assert_eq!(built.name(), parsed.name());
    }

    #[test]
    fn try_with_params_validates_and_sorts() {
        let urn =
            TypeUrn::try_with_params("media", 1, "Image", [("width", "8"), ("pixel", "rgb8")])
                .unwrap();
        assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8,width=8]");

        assert!(TypeUrn::try_with_params("media", 1, "Image", [("Pixel", "x")]).is_err());
        assert!(TypeUrn::try_with_params("media", 1, "image", [("pixel", "x")]).is_err());
        assert!(TypeUrn::try_with_params("Media", 1, "Image", [("pixel", "x")]).is_err());
        assert_eq!(
            TypeUrn::try_with_params("media", 1, "Image", [("k", "1"), ("k", "2")]),
            Err(TypeUrnError::DuplicateParam {
                key: "k".to_owned()
            })
        );
    }

    #[test]
    fn base_strips_parameters() {
        let urn = TypeUrn::parse("std/media/v1/Image[pixel=rgb8,width=8]").unwrap();
        let base = urn.base();
        assert_eq!(base.as_str(), "std/media/v1/Image");
        assert_eq!(base.category(), "media");
        assert_eq!(base.name(), "Image");
        assert_eq!(base.version(), 1);
        assert!(!base.has_params());
        assert_eq!(base.base(), base);
        assert_eq!(base, TypeUrn::parse("std/media/v1/Image").unwrap());
    }

    #[test]
    fn base_eq_ignores_parameters() {
        let rgb = TypeUrn::parse("std/media/v1/Image[pixel=rgb8]").unwrap();
        let mono = TypeUrn::parse("std/media/v1/Image[pixel=mono8]").unwrap();
        let other = TypeUrn::parse("std/media/v1/CompressedImage[format=jpeg]").unwrap();
        assert!(rgb.base_eq(&mono));
        assert!(!rgb.base_eq(&other));
        assert_ne!(rgb, mono);
    }

    #[test]
    fn with_param_adds_and_replaces() {
        let urn = TypeUrn::try_new("media", 1, "Image").unwrap();
        let urn = urn.with_param("pixel", "rgb8").unwrap();
        assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8]");
        let urn = urn.with_param("width", "8").unwrap();
        assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8,width=8]");
        let urn = urn.with_param("pixel", "mono8").unwrap();
        assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=mono8,width=8]");
        assert_eq!(urn.param_count(), 2);

        assert!(urn.clone().with_param("BAD", "x").is_err());
        assert!(urn.with_param("ok", "with space").is_err());
    }

    #[test]
    fn without_param_removes_and_is_idempotent() {
        let urn = TypeUrn::parse("std/media/v1/Image[pixel=rgb8,width=8]").unwrap();
        let urn = urn.without_param("width").unwrap();
        assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8]");
        let urn = urn.without_param("absent").unwrap();
        assert_eq!(urn.as_str(), "std/media/v1/Image[pixel=rgb8]");
        let urn = urn.without_param("pixel").unwrap();
        assert_eq!(urn.as_str(), "std/media/v1/Image");
        assert!(!urn.has_params());
    }

    #[test]
    fn with_param_respects_the_parameter_cap() {
        let mut urn = TypeUrn::try_new("core", 1, "Bool").unwrap();
        for index in 0..TypeUrn::MAX_PARAMS {
            urn = urn.with_param(&format!("k{index}"), "v").unwrap();
        }
        assert_eq!(urn.param_count(), TypeUrn::MAX_PARAMS);
        assert_eq!(
            urn.with_param("overflow", "v"),
            Err(TypeUrnError::TooManyParams {
                found: TypeUrn::MAX_PARAMS + 1,
                max: TypeUrn::MAX_PARAMS,
            })
        );
    }

    #[test]
    fn assembled_urns_respect_the_length_cap() {
        let key_count = TypeUrn::MAX_PARAMS;
        let long_value = "v".repeat(TypeUrn::MAX_SEGMENT_LEN);
        let params: Vec<(String, String)> = (0..key_count)
            .map(|index| (format!("k{index}"), long_value.clone()))
            .collect();
        assert!(matches!(
            TypeUrn::try_with_params("core", 1, "Bool", params),
            Err(TypeUrnError::TooLong { max, .. }) if max == TypeUrn::MAX_LEN
        ));
    }

    #[test]
    fn ordering_groups_by_category() {
        let mut urns = [
            TypeUrn::parse("std/sensor/v1/Imu").unwrap(),
            TypeUrn::parse("std/core/v1/Bool").unwrap(),
            TypeUrn::parse("std/core/v1/Int8").unwrap(),
        ];
        urns.sort();
        let texts: Vec<&str> = urns.iter().map(TypeUrn::as_str).collect();
        assert_eq!(
            texts,
            vec!["std/core/v1/Bool", "std/core/v1/Int8", "std/sensor/v1/Imu"]
        );
    }

    #[test]
    fn conversions_are_available() {
        let urn: TypeUrn = "std/core/v1/Bool".parse().unwrap();
        assert_eq!(urn.as_str(), "std/core/v1/Bool");

        let urn = TypeUrn::try_from("std/core/v1/Int32").unwrap();
        assert_eq!(AsRef::<str>::as_ref(&urn), "std/core/v1/Int32");

        let borrowed: &str = urn.borrow();
        assert_eq!(borrowed, "std/core/v1/Int32");
    }

    #[test]
    fn debug_and_display_show_the_canonical_text() {
        let urn = TypeUrn::parse("std/core/v1/Bool").unwrap();
        assert_eq!(urn.to_string(), "std/core/v1/Bool");
        assert_eq!(format!("{urn:?}"), "TypeUrn(std/core/v1/Bool)");
    }

    #[test]
    fn hashing_matches_equality() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(TypeUrn::parse("std/media/v1/Image[b=2,a=1]").unwrap());
        assert!(set.contains(&TypeUrn::parse("std/media/v1/Image[a=1,b=2]").unwrap()));
        assert!(!set.contains(&TypeUrn::parse("std/media/v1/Image[a=1]").unwrap()));
    }

    #[test]
    fn serde_round_trips_through_json() {
        let urn = TypeUrn::parse("std/sensor/v1/PointCloud[fields=x:y:z]").unwrap();
        let json = serde_json::to_string(&urn).unwrap();
        assert_eq!(json, "\"std/sensor/v1/PointCloud[fields=x:y:z]\"");
        let back: TypeUrn = serde_json::from_str(&json).unwrap();
        assert_eq!(back, urn);
    }

    #[test]
    fn serde_rejects_malformed_text() {
        let err = serde_json::from_str::<TypeUrn>("\"nope\"").unwrap_err();
        assert!(
            err.to_string().contains("4 '/'-separated segments"),
            "{err}"
        );
        assert!(serde_json::from_str::<TypeUrn>("7").is_err());
    }

    #[test]
    fn push_u16_covers_the_range() {
        for value in [0u16, 7, 42, 500, 6000, 65535] {
            let mut out = String::new();
            push_u16(&mut out, value);
            assert_eq!(out, value.to_string());
        }
    }

    #[test]
    fn spans_survive_every_shape() {
        for text in [
            "std/a/v0/A",
            "std/core/v1/Bool",
            "std/very_long_category_name/v65535/SomeLongTypeName[k=v]",
        ] {
            let urn = TypeUrn::parse(text).unwrap();
            assert!(text.contains(urn.category()));
            assert!(text.contains(urn.name()));
            assert!(text.starts_with(urn.base_str()));
        }
    }
}
