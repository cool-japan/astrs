//! `serde::Deserializer` over [`Value`], in two flavours.
//!
//! - `Value` **by value** consumes the tree, so a `String` field moves the
//!   parsed `String` straight into the target with no copy.
//! - `&'de Value` **by reference** hands out `&'de str` through
//!   [`visit_borrowed_str`](serde::de::Visitor::visit_borrowed_str), so a
//!   type with borrowed fields (`struct View<'a> { id: &'a str }`) can be
//!   deserialized against a `Value` that outlives it — no allocation at all
//!   for its string fields.
//!
//! Both are the same shape; only the string and collection ownership
//! differ. [`crate::from_str`] uses the owning one (it owns the freshly
//! parsed tree anyway), [`crate::from_borrowed_value`] the borrowing one.
//!
//! # Enums
//!
//! Three spellings deserialize into a Rust enum, in the order a document is
//! likely to use them:
//!
//! | YAML | Rust |
//! |---|---|
//! | `on_failure` | a unit variant |
//! | `!Circle 3` | `Circle(3)` — the tagged form this crate's serializer writes |
//! | `{Circle: 3}` | `Circle(3)` — the single-entry-mapping form other emitters use |
//!
//! # Where the error points
//!
//! A *syntax* error carries a [`Span`](crate::Span) from the parser. A
//! `serde` **type** error — "invalid type: string, expected u32" — is raised
//! by the target type long after the source text is gone, so it carries no
//! span; instead each mapping level prefixes it with the key it was reading,
//! turning it into `queue_size: invalid type: string "2", expected u16`.

use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};

use crate::error::{Error, ErrorKind, Result};
use crate::mapping::Mapping;
use crate::value::Value;

/// Deserialize `T` from an owned [`Value`], consuming it.
pub(crate) fn from_value<T: de::DeserializeOwned>(value: Value) -> Result<T> {
    T::deserialize(value)
}

/// Deserialize `T` from a borrowed [`Value`], letting `T` borrow its strings.
pub(crate) fn from_borrowed_value<'de, T: de::Deserialize<'de>>(value: &'de Value) -> Result<T> {
    T::deserialize(value)
}

/// How a value is described in a `serde` "invalid type" message.
fn unexpected(value: &Value) -> de::Unexpected<'_> {
    match value {
        Value::Null => de::Unexpected::Unit,
        Value::Bool(inner) => de::Unexpected::Bool(*inner),
        Value::Number(number) => {
            if let Some(inner) = number.as_u64() {
                de::Unexpected::Unsigned(inner)
            } else if let Some(inner) = number.as_i64() {
                de::Unexpected::Signed(inner)
            } else {
                de::Unexpected::Float(number.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(inner) => de::Unexpected::Str(inner),
        Value::Sequence(_) => de::Unexpected::Seq,
        Value::Mapping(_) => de::Unexpected::Map,
        Value::Tagged(_) => de::Unexpected::Other("tagged value"),
    }
}

/// Prefix a location-less `serde` error with the mapping key it came from.
fn under_key(error: Error, key: &str) -> Error {
    match (error.kind(), error.span()) {
        (ErrorKind::Message(message), None) => {
            Error::new(ErrorKind::Message(format!("{key}: {message}")))
        }
        _ => error,
    }
}

/// The key a mapping entry was read under, rendered for a message.
fn key_label(key: &Value) -> String {
    match key {
        Value::String(text) => text.clone(),
        other => crate::emit::inline_scalar(other),
    }
}

/// Send a scalar to `visitor`, or report that the value was not one.
macro_rules! visit_scalar {
    ($value:expr, $visitor:expr, $string:ident) => {{
        match $value {
            Value::Null => $visitor.visit_unit(),
            Value::Bool(inner) => $visitor.visit_bool(inner),
            Value::Number(number) => {
                if let Some(inner) = number.as_u64() {
                    $visitor.visit_u64(inner)
                } else if let Some(inner) = number.as_i64() {
                    $visitor.visit_i64(inner)
                } else {
                    $visitor.visit_f64(number.as_f64().unwrap_or(f64::NAN))
                }
            }
            other => $string(other, $visitor),
        }
    }};
}

/// Forward the listed hints to `deserialize_any`.
macro_rules! forward_to_any {
    ($($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
                self.deserialize_any(visitor)
            }
        )*
    };
}

// ---------------------------------------------------------------- owned

impl<'de> Deserializer<'de> for Value {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        fn strings<'de, V: Visitor<'de>>(value: Value, visitor: V) -> Result<V::Value> {
            match value {
                Value::String(text) => visitor.visit_string(text),
                Value::Sequence(items) => visitor.visit_seq(OwnedSeq::new(items)),
                Value::Mapping(mapping) => visitor.visit_map(OwnedMap::new(mapping)),
                Value::Tagged(tagged) => visitor.visit_enum(OwnedEnum {
                    variant: tagged.tag.as_str().to_owned(),
                    value: Some(tagged.value),
                }),
                // The macro handles every remaining case before calling us.
                other => Err(de::Error::invalid_type(unexpected(&other), &visitor)),
            }
        }
        visit_scalar!(self, visitor, strings)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self {
            Value::Null => visitor.visit_none(),
            other => visitor.visit_some(other),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self {
            Value::String(variant) => visitor.visit_enum(OwnedEnum {
                variant,
                value: None,
            }),
            Value::Tagged(tagged) => visitor.visit_enum(OwnedEnum {
                variant: tagged.tag.as_str().to_owned(),
                value: Some(tagged.value),
            }),
            Value::Mapping(mapping) if mapping.len() == 1 => {
                let mut entries = mapping.into_iter();
                let Some((key, value)) = entries.next() else {
                    return Err(de::Error::custom("a one-entry mapping lost its entry"));
                };
                visitor.visit_enum(OwnedEnum {
                    variant: key_label(&key),
                    value: Some(value),
                })
            }
            other => Err(de::Error::invalid_type(unexpected(&other), &visitor)),
        }
    }

    forward_to_any!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
        deserialize_ignored_any,
    );

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }
}

impl<'de> IntoDeserializer<'de, Error> for Value {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

struct OwnedSeq {
    items: std::vec::IntoIter<Value>,
}

impl OwnedSeq {
    fn new(items: Vec<Value>) -> Self {
        Self {
            items: items.into_iter(),
        }
    }
}

impl<'de> SeqAccess<'de> for OwnedSeq {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        match self.items.next() {
            Some(item) => seed.deserialize(item).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

struct OwnedMap {
    entries: std::vec::IntoIter<(Value, Value)>,
    value: Option<Value>,
    label: String,
}

impl OwnedMap {
    fn new(mapping: Mapping) -> Self {
        Self {
            entries: mapping.into_iter(),
            value: None,
            label: String::new(),
        }
    }
}

impl<'de> MapAccess<'de> for OwnedMap {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        match self.entries.next() {
            Some((key, value)) => {
                self.label = key_label(&key);
                self.value = Some(value);
                seed.deserialize(key).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let Some(value) = self.value.take() else {
            return Err(de::Error::custom("a mapping value was read out of order"));
        };
        seed.deserialize(value)
            .map_err(|error| under_key(error, &self.label))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

struct OwnedEnum {
    variant: String,
    value: Option<Value>,
}

impl<'de> EnumAccess<'de> for OwnedEnum {
    type Error = Error;
    type Variant = OwnedVariant;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        let variant = seed.deserialize(Value::String(self.variant))?;
        Ok((variant, OwnedVariant { value: self.value }))
    }
}

struct OwnedVariant {
    value: Option<Value>,
}

impl<'de> VariantAccess<'de> for OwnedVariant {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        match self.value {
            None | Some(Value::Null) => Ok(()),
            Some(other) => Err(de::Error::invalid_type(
                unexpected(&other),
                &"a unit variant, with no value",
            )),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        match self.value {
            Some(value) => seed.deserialize(value),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"a newtype variant",
            )),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        match self.value {
            Some(Value::Sequence(items)) => visitor.visit_seq(OwnedSeq::new(items)),
            Some(other) => Err(de::Error::invalid_type(unexpected(&other), &visitor)),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &visitor,
            )),
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.value {
            Some(Value::Mapping(mapping)) => visitor.visit_map(OwnedMap::new(mapping)),
            Some(other) => Err(de::Error::invalid_type(unexpected(&other), &visitor)),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &visitor,
            )),
        }
    }
}

// ------------------------------------------------------------- borrowed

impl<'de> Deserializer<'de> for &'de Value {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self {
            Value::Null => visitor.visit_unit(),
            Value::Bool(inner) => visitor.visit_bool(*inner),
            Value::Number(number) => {
                if let Some(inner) = number.as_u64() {
                    visitor.visit_u64(inner)
                } else if let Some(inner) = number.as_i64() {
                    visitor.visit_i64(inner)
                } else {
                    visitor.visit_f64(number.as_f64().unwrap_or(f64::NAN))
                }
            }
            // The whole point of the borrowed path: the visitor gets a
            // `&'de str` that outlives this call, so `#[derive(Deserialize)]`
            // on a borrowing struct never allocates.
            Value::String(text) => visitor.visit_borrowed_str(text),
            Value::Sequence(items) => visitor.visit_seq(BorrowedSeq {
                items: items.iter(),
            }),
            Value::Mapping(mapping) => visitor.visit_map(BorrowedMap::new(mapping)),
            Value::Tagged(tagged) => visitor.visit_enum(BorrowedEnum {
                variant: tagged.tag.as_str(),
                value: Some(&tagged.value),
            }),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self {
            Value::Null => visitor.visit_none(),
            other => visitor.visit_some(other),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self {
            Value::String(variant) => visitor.visit_enum(BorrowedEnum {
                variant,
                value: None,
            }),
            Value::Tagged(tagged) => visitor.visit_enum(BorrowedEnum {
                variant: tagged.tag.as_str(),
                value: Some(&tagged.value),
            }),
            Value::Mapping(mapping) if mapping.len() == 1 => {
                let Some((key, value)) = mapping.get_index(0) else {
                    return Err(de::Error::custom("a one-entry mapping lost its entry"));
                };
                let Value::String(variant) = key else {
                    return Err(de::Error::invalid_type(unexpected(key), &visitor));
                };
                visitor.visit_enum(BorrowedEnum {
                    variant,
                    value: Some(value),
                })
            }
            other => Err(de::Error::invalid_type(unexpected(other), &visitor)),
        }
    }

    forward_to_any!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
        deserialize_ignored_any,
    );

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }
}

impl<'de> IntoDeserializer<'de, Error> for &'de Value {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

struct BorrowedSeq<'de> {
    items: std::slice::Iter<'de, Value>,
}

impl<'de> SeqAccess<'de> for BorrowedSeq<'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        match self.items.next() {
            Some(item) => seed.deserialize(item).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

struct BorrowedMap<'de> {
    mapping: &'de Mapping,
    index: usize,
    label: String,
}

impl<'de> BorrowedMap<'de> {
    fn new(mapping: &'de Mapping) -> Self {
        Self {
            mapping,
            index: 0,
            label: String::new(),
        }
    }
}

impl<'de> MapAccess<'de> for BorrowedMap<'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        match self.mapping.get_index(self.index) {
            Some((key, _)) => {
                self.label = key_label(key);
                seed.deserialize(key).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let Some((_, value)) = self.mapping.get_index(self.index) else {
            return Err(de::Error::custom("a mapping value was read out of order"));
        };
        self.index += 1;
        seed.deserialize(value)
            .map_err(|error| under_key(error, &self.label))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.mapping.len() - self.index)
    }
}

struct BorrowedEnum<'de> {
    variant: &'de str,
    value: Option<&'de Value>,
}

impl<'de> EnumAccess<'de> for BorrowedEnum<'de> {
    type Error = Error;
    type Variant = BorrowedVariant<'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        let variant = seed.deserialize(BorrowedStr(self.variant))?;
        Ok((variant, BorrowedVariant { value: self.value }))
    }
}

/// A deserializer for a borrowed variant name.
struct BorrowedStr<'de>(&'de str);

impl<'de> Deserializer<'de> for BorrowedStr<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_str(self.0)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

struct BorrowedVariant<'de> {
    value: Option<&'de Value>,
}

impl<'de> VariantAccess<'de> for BorrowedVariant<'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        match self.value {
            None | Some(Value::Null) => Ok(()),
            Some(other) => Err(de::Error::invalid_type(
                unexpected(other),
                &"a unit variant, with no value",
            )),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        match self.value {
            Some(value) => seed.deserialize(value),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"a newtype variant",
            )),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        match self.value {
            Some(Value::Sequence(items)) => visitor.visit_seq(BorrowedSeq {
                items: items.iter(),
            }),
            Some(other) => Err(de::Error::invalid_type(unexpected(other), &visitor)),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &visitor,
            )),
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.value {
            Some(Value::Mapping(mapping)) => visitor.visit_map(BorrowedMap::new(mapping)),
            Some(other) => Err(de::Error::invalid_type(unexpected(other), &visitor)),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &visitor,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::BTreeMap;

    use serde::Deserialize;

    use super::*;
    use crate::value::{Tag, TaggedValue};

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Node {
        id: String,
        #[serde(default)]
        queue_size: Option<u16>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    enum Shape {
        Point,
        Circle(u32),
        Segment(u32, u32),
        Rect { w: u32, h: u32 },
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        let mut mapping = Mapping::new();
        for (key, value) in pairs {
            mapping.insert(Value::from(*key), value.clone());
        }
        Value::Mapping(mapping)
    }

    #[test]
    fn a_struct_deserializes_from_a_mapping() {
        let value = map(&[
            ("id", Value::from("camera")),
            ("queue_size", Value::from(2u8)),
        ]);
        let node: Node = from_value(value).expect("node");
        assert_eq!(
            node,
            Node {
                id: "camera".to_owned(),
                queue_size: Some(2),
            }
        );
    }

    #[test]
    fn a_null_field_is_none_and_a_missing_field_defaults() {
        let explicit: Node = from_value(map(&[
            ("id", Value::from("a")),
            ("queue_size", Value::Null),
        ]))
        .expect("null");
        assert_eq!(explicit.queue_size, None);
        let missing: Node = from_value(map(&[("id", Value::from("a"))])).expect("missing");
        assert_eq!(missing.queue_size, None);
    }

    #[test]
    fn a_type_error_names_the_key_it_came_from() {
        let error = from_value::<Node>(map(&[
            ("id", Value::from("a")),
            ("queue_size", Value::from("two")),
        ]))
        .expect_err("type error");
        assert!(
            error.to_string().starts_with("queue_size: invalid type"),
            "{error}"
        );
        assert_eq!(error.span(), None);
    }

    #[test]
    fn deny_unknown_fields_still_bites() {
        let error = from_value::<Node>(map(&[("id", Value::from("a")), ("bogus", Value::Null)]))
            .expect_err("unknown field");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn every_enum_spelling_deserializes() {
        assert_eq!(
            from_value::<Shape>(Value::from("Point")).expect("unit"),
            Shape::Point
        );
        assert_eq!(
            from_value::<Shape>(Value::from(TaggedValue {
                tag: Tag::new("Circle"),
                value: Value::from(3u8),
            }))
            .expect("newtype"),
            Shape::Circle(3)
        );
        assert_eq!(
            from_value::<Shape>(Value::from(TaggedValue {
                tag: Tag::new("Segment"),
                value: Value::Sequence(vec![Value::from(1u8), Value::from(2u8)]),
            }))
            .expect("tuple"),
            Shape::Segment(1, 2)
        );
        assert_eq!(
            from_value::<Shape>(Value::from(TaggedValue {
                tag: Tag::new("Rect"),
                value: map(&[("w", Value::from(1u8)), ("h", Value::from(2u8))]),
            }))
            .expect("struct"),
            Shape::Rect { w: 1, h: 2 }
        );
        // The single-entry mapping form other emitters produce.
        assert_eq!(
            from_value::<Shape>(map(&[("Circle", Value::from(3u8))])).expect("mapping"),
            Shape::Circle(3)
        );
    }

    #[test]
    fn the_borrowed_path_hands_out_borrowed_strs() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct View<'a> {
            id: &'a str,
        }

        let value = map(&[("id", Value::from("camera"))]);
        let view: View<'_> = from_borrowed_value(&value).expect("view");
        assert_eq!(view.id, "camera");
        // Genuinely borrowed: the &str points into the Value, not a copy.
        let stored = value.get("id").and_then(Value::as_str).expect("stored");
        assert!(std::ptr::eq(view.id.as_ptr(), stored.as_ptr()));
    }

    #[test]
    fn the_borrowed_path_handles_every_kind() {
        let value = Value::Sequence(vec![
            Value::Null,
            Value::from(true),
            Value::from(-1i8),
            Value::from(1.5),
            Value::from(u64::MAX),
            map(&[("a", Value::from("b"))]),
        ]);
        let echoed: Value = from_borrowed_value(&value).expect("echo");
        assert_eq!(echoed, value);

        let owned: Value = from_value(value.clone()).expect("echo owned");
        assert_eq!(owned, value);
    }

    #[test]
    fn borrowed_enums_work_in_all_three_spellings() {
        for (value, expected) in [
            (Value::from("Point"), Shape::Point),
            (map(&[("Circle", Value::from(3u8))]), Shape::Circle(3)),
            (
                Value::from(TaggedValue {
                    tag: Tag::new("Rect"),
                    value: map(&[("w", Value::from(1u8)), ("h", Value::from(2u8))]),
                }),
                Shape::Rect { w: 1, h: 2 },
            ),
        ] {
            let shape: Shape = from_borrowed_value(&value).expect("shape");
            assert_eq!(shape, expected);
        }
    }

    #[test]
    fn collections_of_every_shape_deserialize() {
        let value = Value::Sequence(vec![Value::from(1u8), Value::from(2u8)]);
        assert_eq!(
            from_value::<Vec<u8>>(value.clone()).expect("vec"),
            vec![1, 2]
        );
        assert_eq!(
            from_borrowed_value::<Vec<u8>>(&value).expect("vec"),
            vec![1, 2]
        );
        assert_eq!(
            from_value::<(u8, u8)>(value.clone()).expect("tuple"),
            (1, 2)
        );

        let mut mapping = Mapping::new();
        mapping.insert(Value::from(1u8), Value::from("a"));
        let value = Value::Mapping(mapping);
        let decoded: BTreeMap<u8, String> = from_value(value.clone()).expect("map");
        assert_eq!(decoded.get(&1).map(String::as_str), Some("a"));
        let borrowed: BTreeMap<u8, String> = from_borrowed_value(&value).expect("map");
        assert_eq!(borrowed, decoded);
    }

    #[test]
    fn a_wrong_variant_shape_is_reported_not_ignored() {
        let error = from_value::<Shape>(Value::from(TaggedValue {
            tag: Tag::new("Point"),
            value: Value::from(1u8),
        }))
        .expect_err("unit variant with a payload");
        assert!(error.to_string().contains("invalid type"), "{error}");

        let error = from_value::<Shape>(Value::from(1u8)).expect_err("not an enum");
        assert!(error.to_string().contains("invalid type"), "{error}");

        let error =
            from_borrowed_value::<Shape>(&Value::Sequence(vec![])).expect_err("not an enum");
        assert!(error.to_string().contains("invalid type"), "{error}");
    }

    #[test]
    fn a_flattened_catch_all_collects_the_unmodelled_keys() {
        // The shape `astrs-migrate`'s dora importer uses: named fields plus
        // a `#[serde(flatten)]` map that swallows everything else. `serde`
        // implements it by buffering into its own `Content` type, so it
        // exercises `deserialize_any` on every value *and* on every key.
        #[derive(Debug, Deserialize, PartialEq)]
        struct Descriptor {
            name: String,
            #[serde(default)]
            nodes: Vec<String>,
            #[serde(flatten)]
            extra: std::collections::BTreeMap<String, Value>,
        }

        let parsed: Descriptor =
            crate::from_str("name: demo\nnodes: [a]\nunknown: 1\nnested:\n  k: [1, true, null]\n")
                .expect("flattened");
        assert_eq!(parsed.name, "demo");
        assert_eq!(parsed.nodes, vec!["a".to_owned()]);
        assert_eq!(parsed.extra.get("unknown"), Some(&Value::from(1u64)));
        assert_eq!(
            parsed
                .extra
                .get("nested")
                .and_then(|nested| nested.get("k")),
            Some(&Value::Sequence(vec![
                Value::from(1u64),
                Value::from(true),
                Value::Null,
            ]))
        );
        // And the same document through the borrowed path.
        let value = crate::parse_str("name: demo\nunknown: 1\n").expect("parse");
        let borrowed: Descriptor = from_borrowed_value(&value).expect("flattened");
        assert_eq!(borrowed.extra.get("unknown"), Some(&Value::from(1u64)));
    }

    #[test]
    fn a_flattened_map_survives_a_round_trip_through_text() {
        #[derive(Debug, Deserialize, serde::Serialize, PartialEq)]
        struct Descriptor {
            name: String,
            #[serde(flatten)]
            extra: std::collections::BTreeMap<String, Value>,
        }

        let original = Descriptor {
            name: "demo".to_owned(),
            extra: [
                ("count".to_owned(), Value::from(2u64)),
                ("flag".to_owned(), Value::from(true)),
                (
                    "list".to_owned(),
                    Value::Sequence(vec![Value::from("x"), Value::Null]),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let text = crate::to_string(&original).expect("emit");
        let back: Descriptor = crate::from_str(&text).expect("re-read");
        assert_eq!(back, original, "{text}");
    }

    #[test]
    fn newtype_structs_are_transparent() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Meters(f64);

        assert_eq!(
            from_value::<Meters>(Value::from(1.5)).expect("newtype"),
            Meters(1.5)
        );
        assert_eq!(
            from_borrowed_value::<Meters>(&Value::from(1.5)).expect("newtype"),
            Meters(1.5)
        );
    }

    #[test]
    fn untagged_enums_resolve_through_deserialize_any() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(untagged)]
        enum EnvValue {
            Bool(bool),
            Int(i64),
            Text(String),
        }

        assert_eq!(
            from_value::<EnvValue>(Value::from(true)).expect("bool"),
            EnvValue::Bool(true)
        );
        assert_eq!(
            from_value::<EnvValue>(Value::from(3u8)).expect("int"),
            EnvValue::Int(3)
        );
        assert_eq!(
            from_value::<EnvValue>(Value::from("x")).expect("text"),
            EnvValue::Text("x".to_owned())
        );
        assert_eq!(
            from_borrowed_value::<EnvValue>(&Value::from("x")).expect("text"),
            EnvValue::Text("x".to_owned())
        );
    }

    #[test]
    fn into_deserializer_is_available_for_both_flavours() {
        let value = Value::from(7u8);
        let owned: u8 = u8::deserialize(value.clone().into_deserializer()).expect("owned");
        let borrowed: u8 = u8::deserialize((&value).into_deserializer()).expect("borrowed");
        assert_eq!((owned, borrowed), (7, 7));
    }
}
