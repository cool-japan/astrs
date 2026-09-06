//! The owned document tree: [`Value`] and the tag wrapper it can carry.
//!
//! [`Value`] is what the parser produces and what the emitter consumes. It
//! is deliberately the same shape as `serde_yaml`'s `Value` — the same six
//! core-schema cases plus a tagged case — because the switchover from
//! `serde_yaml` to this crate has to be a dependency edit and an import
//! rename, not a rewrite of `astrs-migrate`'s `BTreeMap<String, Value>`
//! escape hatches.
//!
//! # Tags
//!
//! Core-schema tags (`!!str`, `!!int`, `!!float`, `!!bool`, `!!null`,
//! `!!seq`, `!!map`) are *resolved during parsing* and never reach
//! [`Value`]: `!!str 1` is a [`Value::String`], not a tagged integer. Only
//! **local** tags — `!Variant`, `!my-thing` — survive as
//! [`Value::Tagged`], which is exactly the representation `serde` needs for
//! an externally tagged enum (`!Circle {radius: 1}`).

use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::mapping::Mapping;
use crate::number::Number;

/// Discriminant byte mixed into [`Value`]'s hash for the string case.
///
/// Shared with [`Mapping::get_str`](crate::Mapping::get_str), which hashes a
/// bare `&str` and must land in the same bucket as the `Value::String` that
/// holds it.
const STRING_DISCRIMINANT: u8 = 3;

/// A YAML node.
///
/// # Examples
///
/// ```
/// use astrs_yaml::Value;
///
/// let value: Value = astrs_yaml::from_str("nodes: [camera, planner]")?;
/// let nodes = value.get("nodes").and_then(Value::as_sequence).expect("nodes");
/// assert_eq!(nodes.len(), 2);
/// assert_eq!(nodes[0].as_str(), Some("camera"));
/// # Ok::<(), astrs_yaml::Error>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Value {
    /// `null`, `~`, or an empty node.
    #[default]
    Null,
    /// `true` or `false` (and their `True`/`TRUE` spellings).
    Bool(bool),
    /// An integer or a float — see [`Number`].
    Number(Number),
    /// A string scalar, in any of the four scalar styles.
    String(String),
    /// A block or flow sequence.
    Sequence(Vec<Value>),
    /// A block or flow mapping, in document order.
    Mapping(Mapping),
    /// A node carrying a local tag, such as `!Variant`.
    Tagged(Box<TaggedValue>),
}

impl Value {
    /// A short name for this node's kind, for error messages.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Number(number) => {
                if number.is_f64() {
                    "float"
                } else {
                    "integer"
                }
            }
            Self::String(_) => "string",
            Self::Sequence(_) => "sequence",
            Self::Mapping(_) => "mapping",
            Self::Tagged(_) => "tagged value",
        }
    }

    /// True for [`Value::Null`].
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The boolean this node holds, if it is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The string this node holds, if it is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// The number this node holds, if it is one.
    #[must_use]
    pub fn as_number(&self) -> Option<Number> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// This node as a [`u64`].
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        self.as_number().and_then(|number| number.as_u64())
    }

    /// This node as an [`i64`].
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        self.as_number().and_then(|number| number.as_i64())
    }

    /// This node as an [`f64`], widening an integer when necessary.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        self.as_number().and_then(|number| number.as_f64())
    }

    /// The sequence this node holds, if it is one.
    #[must_use]
    pub fn as_sequence(&self) -> Option<&Vec<Value>> {
        match self {
            Self::Sequence(items) => Some(items),
            _ => None,
        }
    }

    /// The mapping this node holds, if it is one.
    #[must_use]
    pub fn as_mapping(&self) -> Option<&Mapping> {
        match self {
            Self::Mapping(mapping) => Some(mapping),
            _ => None,
        }
    }

    /// A mutable reference to the mapping this node holds, if it is one.
    pub fn as_mapping_mut(&mut self) -> Option<&mut Mapping> {
        match self {
            Self::Mapping(mapping) => Some(mapping),
            _ => None,
        }
    }

    /// The value stored under the string key `key`, when this node is a
    /// mapping that has one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_mapping().and_then(|mapping| mapping.get_str(key))
    }

    /// The `index`-th element, when this node is a sequence long enough.
    #[must_use]
    pub fn get_index(&self, index: usize) -> Option<&Value> {
        self.as_sequence().and_then(|items| items.get(index))
    }

    /// This node's local tag, when it carries one.
    #[must_use]
    pub fn tag(&self) -> Option<&Tag> {
        match self {
            Self::Tagged(tagged) => Some(&tagged.tag),
            _ => None,
        }
    }

    /// This node with any local tag stripped.
    ///
    /// Tags nest at most one level deep in practice, but the loop is written
    /// to terminate for any depth.
    #[must_use]
    pub fn untagged(&self) -> &Value {
        let mut current = self;
        while let Self::Tagged(tagged) = current {
            current = &tagged.value;
        }
        current
    }

    /// How many nodes this subtree contains, counting itself.
    ///
    /// The parser charges alias expansion against
    /// [`Limits::max_alias_nodes`](crate::Limits::max_alias_nodes) using
    /// this count, which is what makes the billion-laughs guard bound *work*
    /// rather than merely bounding declared nesting.
    #[must_use]
    pub fn node_count(&self) -> usize {
        match self {
            Self::Sequence(items) => 1 + items.iter().map(Value::node_count).sum::<usize>(),
            Self::Mapping(mapping) => {
                1 + mapping
                    .iter()
                    .map(|(key, value)| key.node_count() + value.node_count())
                    .sum::<usize>()
            }
            Self::Tagged(tagged) => 1 + tagged.value.node_count(),
            _ => 1,
        }
    }

    /// Hash a bare `&str` exactly as `Value::String(key)` hashes.
    pub(crate) fn hash_str_key<H: Hasher>(key: &str, state: &mut H) {
        STRING_DISCRIMINANT.hash(state);
        key.hash(state);
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Null => 0u8.hash(state),
            Self::Bool(value) => {
                1u8.hash(state);
                value.hash(state);
            }
            Self::Number(value) => {
                2u8.hash(state);
                value.hash(state);
            }
            Self::String(value) => Self::hash_str_key(value, state),
            Self::Sequence(items) => {
                4u8.hash(state);
                items.hash(state);
            }
            Self::Mapping(mapping) => {
                5u8.hash(state);
                mapping.hash(state);
            }
            Self::Tagged(tagged) => {
                6u8.hash(state);
                tagged.hash(state);
            }
        }
    }
}

/// A [`Value`] carrying a local tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaggedValue {
    /// The tag, without its leading `!`.
    pub tag: Tag,
    /// The node the tag applies to.
    pub value: Value,
}

/// A local YAML tag such as `!Circle`.
///
/// Stored without the leading `!`, and rendered *with* it — the same
/// convention `serde_yaml::value::Tag` uses, so `tag.to_string()` produces
/// text that can be pasted back into a document.
///
/// # Examples
///
/// ```
/// use astrs_yaml::Tag;
///
/// let tag = Tag::new("Circle");
/// assert_eq!(tag.as_str(), "Circle");
/// assert_eq!(tag.to_string(), "!Circle");
/// // A leading `!` is accepted and normalized away.
/// assert_eq!(Tag::new("!Circle"), tag);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag(String);

impl Tag {
    /// Build a tag from `name`, dropping one leading `!` if present.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        match name.strip_prefix('!') {
            Some(rest) => Self(rest.to_owned()),
            None => Self(name),
        }
    }

    /// The tag name without its leading `!`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "!{}", self.0)
    }
}

impl PartialEq<str> for Tag {
    fn eq(&self, other: &str) -> bool {
        self.0 == other || (other.starts_with('!') && self.0 == other[1..])
    }
}

macro_rules! from_into_number {
    ($($ty:ty),* $(,)?) => {
        $(
            impl From<$ty> for Value {
                fn from(value: $ty) -> Self {
                    Self::Number(Number::from(value))
                }
            }
        )*
    };
}

from_into_number!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

impl From<Number> for Value {
    fn from(value: Number) -> Self {
        Self::Number(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Self {
        Self::Sequence(value)
    }
}

impl From<Mapping> for Value {
    fn from(value: Mapping) -> Self {
        Self::Mapping(value)
    }
}

impl From<TaggedValue> for Value {
    fn from(value: TaggedValue) -> Self {
        Self::Tagged(Box::new(value))
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        match value {
            Some(inner) => inner.into(),
            None => Self::Null,
        }
    }
}

impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Sequence(items) => items.serialize(serializer),
            Self::Mapping(mapping) => mapping.serialize(serializer),
            Self::Tagged(tagged) => {
                serializer.serialize_newtype_struct(TAGGED_NEWTYPE_STRUCT, tagged.as_ref())
            }
        }
    }
}

/// The `serialize_newtype_struct` name this crate's own serializer watches
/// for so a [`Value::Tagged`] survives `to_value` intact.
///
/// `Serializer::serialize_newtype_variant` would be the natural fit, but it
/// demands a `&'static str` variant name and a tag is document data — so the
/// tag travels as the key of the one-entry mapping inside, and the marker
/// name is what tells this crate's serializer to fold that mapping back into
/// a tagged value. Any other serializer sees a transparent newtype wrapping
/// `{"!Tag": value}`, which is lossless and needs no cooperation.
pub(crate) const TAGGED_NEWTYPE_STRUCT: &str = "$astrs_yaml::tagged";

impl Serialize for TaggedValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(&self.tag.to_string(), &self.value)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> serde::de::Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any YAML value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        Value::deserialize(deserializer)
    }

    fn visit_newtype_struct<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Value, D::Error> {
        Value::deserialize(deserializer)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut items = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(item) = access.next_element()? {
            items.push(item);
        }
        Ok(Value::Sequence(items))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut mapping = Mapping::with_capacity(access.size_hint().unwrap_or(0));
        while let Some((key, value)) = access.next_entry()? {
            mapping.insert(key, value);
        }
        Ok(Value::Mapping(mapping))
    }

    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, access: A) -> Result<Value, A::Error> {
        use serde::de::VariantAccess as _;
        let (tag, variant): (String, _) = access.variant()?;
        let value = variant.newtype_variant()?;
        Ok(Value::Tagged(Box::new(TaggedValue {
            tag: Tag::new(tag),
            value,
        })))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::hash_map::DefaultHasher;

    use super::*;

    fn hash_of<T: Hash>(value: &T) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn accessors_return_none_for_the_wrong_kind() {
        let text = Value::from("x");
        assert_eq!(text.as_str(), Some("x"));
        assert_eq!(text.as_bool(), None);
        assert_eq!(text.as_u64(), None);
        assert_eq!(text.as_i64(), None);
        assert_eq!(text.as_f64(), None);
        assert_eq!(text.as_sequence(), None);
        assert_eq!(text.as_mapping(), None);
        assert_eq!(text.as_number(), None);
        assert!(!text.is_null());
        assert!(Value::Null.is_null());
        assert_eq!(Value::default(), Value::Null);
    }

    #[test]
    fn numbers_widen_but_do_not_narrow() {
        let integer = Value::from(3u8);
        assert_eq!(integer.as_u64(), Some(3));
        assert_eq!(integer.as_i64(), Some(3));
        assert_eq!(integer.as_f64(), Some(3.0));
        let float = Value::from(3.5f64);
        assert_eq!(float.as_u64(), None);
        assert_eq!(float.as_f64(), Some(3.5));
        assert_eq!(Value::from(-1i32).as_i64(), Some(-1));
        assert_eq!(Value::from(Number::from(1u8)), Value::from(1u8));
    }

    #[test]
    fn type_names_cover_every_case() {
        assert_eq!(Value::Null.type_name(), "null");
        assert_eq!(Value::from(true).type_name(), "boolean");
        assert_eq!(Value::from(1u8).type_name(), "integer");
        assert_eq!(Value::from(1.0).type_name(), "float");
        assert_eq!(Value::from("x").type_name(), "string");
        assert_eq!(Value::Sequence(vec![]).type_name(), "sequence");
        assert_eq!(Value::Mapping(Mapping::new()).type_name(), "mapping");
        assert_eq!(
            Value::from(TaggedValue {
                tag: Tag::new("T"),
                value: Value::Null,
            })
            .type_name(),
            "tagged value"
        );
    }

    #[test]
    fn navigation_helpers_reach_nested_nodes() {
        let mut inner = Mapping::new();
        inner.insert(Value::from("id"), Value::from("camera"));
        let value = Value::Sequence(vec![Value::Mapping(inner)]);
        assert_eq!(
            value.get_index(0).and_then(|node| node.get("id")),
            Some(&Value::from("camera"))
        );
        assert_eq!(value.get("id"), None);
        assert_eq!(value.get_index(9), None);
    }

    #[test]
    fn node_count_walks_keys_and_values() {
        assert_eq!(Value::Null.node_count(), 1);
        assert_eq!(
            Value::Sequence(vec![Value::Null, Value::Null]).node_count(),
            3
        );
        let mut mapping = Mapping::new();
        mapping.insert(Value::from("a"), Value::Sequence(vec![Value::Null]));
        // mapping + key + sequence + element
        assert_eq!(Value::Mapping(mapping).node_count(), 4);
        assert_eq!(
            Value::from(TaggedValue {
                tag: Tag::new("T"),
                value: Value::Null,
            })
            .node_count(),
            2
        );
    }

    #[test]
    fn tags_normalize_their_bang_and_render_it_back() {
        let tag = Tag::new("Circle");
        assert_eq!(Tag::new("!Circle"), tag);
        assert_eq!(tag.to_string(), "!Circle");
        assert_eq!(tag.as_str(), "Circle");
        assert!(tag == *"Circle");
        assert!(tag == *"!Circle");
        assert!(tag != *"Square");
    }

    #[test]
    fn untagged_strips_every_layer() {
        let inner = Value::from(TaggedValue {
            tag: Tag::new("A"),
            value: Value::from(1u8),
        });
        let outer = Value::from(TaggedValue {
            tag: Tag::new("B"),
            value: inner,
        });
        assert_eq!(outer.untagged(), &Value::from(1u8));
        assert_eq!(outer.tag(), Some(&Tag::new("B")));
        assert_eq!(Value::Null.tag(), None);
        assert_eq!(Value::Null.untagged(), &Value::Null);
    }

    #[test]
    fn a_string_value_hashes_like_its_bare_str() {
        let mut direct = DefaultHasher::new();
        Value::hash_str_key("frames", &mut direct);
        assert_eq!(direct.finish(), hash_of(&Value::from("frames")));
        assert_ne!(hash_of(&Value::from("a")), hash_of(&Value::from("b")));
        assert_ne!(hash_of(&Value::Null), hash_of(&Value::from(0u8)));
    }

    #[test]
    fn option_converts_to_null_or_the_inner_value() {
        assert_eq!(Value::from(None::<u8>), Value::Null);
        assert_eq!(Value::from(Some(2u8)), Value::from(2u8));
        assert_eq!(Value::from(String::from("s")), Value::from("s"));
        assert_eq!(
            Value::from(vec![Value::Null]),
            Value::Sequence(vec![Value::Null])
        );
        assert_eq!(Value::from(Mapping::new()), Value::Mapping(Mapping::new()));
    }

    #[test]
    fn mutable_mapping_access_is_available() {
        let mut value = Value::Mapping(Mapping::new());
        value
            .as_mapping_mut()
            .expect("mapping")
            .insert(Value::from("a"), Value::from(1u8));
        assert_eq!(value.get("a"), Some(&Value::from(1u8)));
        assert!(Value::Null.as_mapping_mut().is_none());
    }
}
