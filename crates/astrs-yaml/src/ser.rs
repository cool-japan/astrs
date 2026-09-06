//! `serde::Serializer` over [`Value`].
//!
//! Serialization is deliberately two-stage — build a [`Value`], then hand it
//! to the emitter — rather than writing YAML directly from the `Serialize`
//! calls. YAML's layout decisions are not local: whether a mapping's value
//! goes on the same line depends on whether it turns out to be empty, and a
//! non-scalar mapping key has to be written with `?` before its value is
//! known. A streaming emitter would need to buffer for exactly those cases,
//! so building the tree first costs no more and keeps the emitter a pure
//! function of the value.
//!
//! # How Rust shapes land in YAML
//!
//! | Rust | YAML |
//! |---|---|
//! | `None`, `()`, unit struct | `null` |
//! | `Some(x)`, newtype struct | whatever `x` is |
//! | unit enum variant | the variant's name, as a string |
//! | `E::V(x)` / `E::V(a, b)` / `E::V { a }` | `!V x` / `!V [a, b]` / `!V {a: …}` |
//! | `&[u8]` | a sequence of integers |
//! | `char` | a one-character string |
//!
//! These are `serde_yaml`'s conventions, not inventions: an externally
//! tagged enum has to round-trip through YAML the same way it did before the
//! switchover, or every `astrs-migrate` fixture changes shape.

use serde::ser::{self, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::mapping::Mapping;
use crate::number::Number;
use crate::value::{Tag, TaggedValue, Value};

/// Convert any [`Serialize`] value into a [`Value`].
pub(crate) fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value> {
    value.serialize(ValueSerializer)
}

/// The serializer that builds a [`Value`].
pub(crate) struct ValueSerializer;

/// `i128`/`u128` outside the `i64`/`u64` range cannot be represented.
fn too_wide(literal: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::IntegerOutOfRange {
        literal: literal.to_string(),
    })
}

impl ser::Serializer for ValueSerializer {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = SerializeSequence;
    type SerializeTuple = SerializeSequence;
    type SerializeTupleStruct = SerializeSequence;
    type SerializeTupleVariant = SerializeTaggedSequence;
    type SerializeMap = SerializeMapping;
    type SerializeStruct = SerializeMapping;
    type SerializeStructVariant = SerializeTaggedMapping;

    fn serialize_bool(self, value: bool) -> Result<Value> {
        Ok(Value::Bool(value))
    }

    fn serialize_i8(self, value: i8) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_i16(self, value: i16) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_i32(self, value: i32) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_i64(self, value: i64) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_i128(self, value: i128) -> Result<Value> {
        if let Ok(narrowed) = u64::try_from(value) {
            return Ok(Value::Number(Number::from(narrowed)));
        }
        match i64::try_from(value) {
            Ok(narrowed) => Ok(Value::Number(Number::from(narrowed))),
            Err(_) => Err(too_wide(value)),
        }
    }

    fn serialize_u8(self, value: u8) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_u16(self, value: u16) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_u32(self, value: u32) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_u64(self, value: u64) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_u128(self, value: u128) -> Result<Value> {
        match u64::try_from(value) {
            Ok(narrowed) => Ok(Value::Number(Number::from(narrowed))),
            Err(_) => Err(too_wide(value)),
        }
    }

    fn serialize_f32(self, value: f32) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_f64(self, value: f64) -> Result<Value> {
        Ok(Value::Number(Number::from(value)))
    }

    fn serialize_char(self, value: char) -> Result<Value> {
        Ok(Value::String(value.to_string()))
    }

    fn serialize_str(self, value: &str) -> Result<Value> {
        Ok(Value::String(value.to_owned()))
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<Value> {
        Ok(Value::Sequence(
            value
                .iter()
                .map(|byte| Value::Number(Number::from(*byte)))
                .collect(),
        ))
    }

    fn serialize_none(self) -> Result<Value> {
        Ok(Value::Null)
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Value> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Value> {
        Ok(Value::Null)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value> {
        Ok(Value::Null)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Value> {
        Ok(Value::String(variant.to_owned()))
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Value> {
        let inner = value.serialize(self)?;
        if name != crate::value::TAGGED_NEWTYPE_STRUCT {
            return Ok(inner);
        }
        // A `Value::Tagged` on its way back to itself: the wrapper carries a
        // one-entry mapping whose key is the tag. Anything else under this
        // name is passed through untouched rather than mis-folded.
        let Value::Mapping(mapping) = inner else {
            return Ok(inner);
        };
        if mapping.len() != 1 {
            return Ok(Value::Mapping(mapping));
        }
        let mut entries = mapping.into_iter();
        let Some((key, payload)) = entries.next() else {
            return Ok(Value::Mapping(Mapping::new()));
        };
        match key {
            Value::String(tag) if tag.starts_with('!') => Ok(Value::from(TaggedValue {
                tag: Tag::new(tag),
                value: payload,
            })),
            other => {
                let mut restored = Mapping::with_capacity(1);
                restored.insert(other, payload);
                Ok(Value::Mapping(restored))
            }
        }
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value> {
        Ok(Value::from(TaggedValue {
            tag: Tag::new(variant),
            value: value.serialize(self)?,
        }))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<SerializeSequence> {
        Ok(SerializeSequence {
            items: Vec::with_capacity(len.unwrap_or(0)),
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<SerializeSequence> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SerializeSequence> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<SerializeTaggedSequence> {
        Ok(SerializeTaggedSequence {
            tag: Tag::new(variant),
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<SerializeMapping> {
        Ok(SerializeMapping {
            mapping: Mapping::with_capacity(len.unwrap_or(0)),
            key: None,
        })
    }

    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<SerializeMapping> {
        self.serialize_map(Some(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<SerializeTaggedMapping> {
        Ok(SerializeTaggedMapping {
            tag: Tag::new(variant),
            mapping: Mapping::with_capacity(len),
        })
    }
}

/// Accumulates a sequence, a tuple, or a tuple struct.
pub(crate) struct SerializeSequence {
    items: Vec<Value>,
}

impl ser::SerializeSeq for SerializeSequence {
    type Ok = Value;
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.items.push(value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(Value::Sequence(self.items))
    }
}

impl ser::SerializeTuple for SerializeSequence {
    type Ok = Value;
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Value> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for SerializeSequence {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Value> {
        ser::SerializeSeq::end(self)
    }
}

/// Accumulates `E::V(a, b)`, which becomes `!V [a, b]`.
pub(crate) struct SerializeTaggedSequence {
    tag: Tag,
    items: Vec<Value>,
}

impl ser::SerializeTupleVariant for SerializeTaggedSequence {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.items.push(value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(Value::from(TaggedValue {
            tag: self.tag,
            value: Value::Sequence(self.items),
        }))
    }
}

/// Accumulates a map or a struct.
pub(crate) struct SerializeMapping {
    mapping: Mapping,
    key: Option<Value>,
}

impl ser::SerializeMap for SerializeMapping {
    type Ok = Value;
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<()> {
        self.key = Some(key.serialize(ValueSerializer)?);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let Some(key) = self.key.take() else {
            return Err(Error::new(ErrorKind::Message(
                "serialize_value called before serialize_key".to_owned(),
            )));
        };
        self.mapping.insert(key, value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(Value::Mapping(self.mapping))
    }
}

impl ser::SerializeStruct for SerializeMapping {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        name: &'static str,
        value: &T,
    ) -> Result<()> {
        self.mapping.insert(
            Value::String(name.to_owned()),
            value.serialize(ValueSerializer)?,
        );
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(Value::Mapping(self.mapping))
    }
}

/// Accumulates `E::V { a }`, which becomes `!V {a: …}`.
pub(crate) struct SerializeTaggedMapping {
    tag: Tag,
    mapping: Mapping,
}

impl ser::SerializeStructVariant for SerializeTaggedMapping {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        name: &'static str,
        value: &T,
    ) -> Result<()> {
        self.mapping.insert(
            Value::String(name.to_owned()),
            value.serialize(ValueSerializer)?,
        );
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(Value::from(TaggedValue {
            tag: self.tag,
            value: Value::Mapping(self.mapping),
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::BTreeMap;

    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    enum Shape {
        Point,
        Circle(u32),
        Segment(u32, u32),
        Rect { w: u32, h: u32 },
    }

    #[test]
    fn scalars_map_onto_their_yaml_kinds() {
        assert_eq!(to_value(&true).expect("bool"), Value::Bool(true));
        assert_eq!(to_value(&1u8).expect("u8"), Value::from(1u64));
        assert_eq!(to_value(&-1i16).expect("i16"), Value::from(-1i64));
        assert_eq!(to_value(&1.5f32).expect("f32"), Value::from(1.5));
        assert_eq!(to_value(&'x').expect("char"), Value::from("x"));
        assert_eq!(to_value("s").expect("str"), Value::from("s"));
        assert_eq!(to_value(&()).expect("unit"), Value::Null);
        assert_eq!(to_value(&None::<u8>).expect("none"), Value::Null);
        assert_eq!(to_value(&Some(2u8)).expect("some"), Value::from(2u64));
    }

    #[test]
    fn wide_integers_are_accepted_only_inside_the_yaml_range() {
        assert_eq!(
            to_value(&(u64::MAX as u128)).expect("u64 max"),
            Value::from(u64::MAX)
        );
        assert_eq!(to_value(&(-1i128)).expect("small"), Value::from(-1i64));
        assert_eq!(
            to_value(&(i64::MAX as i128 + 1)).expect("still a u64"),
            Value::from(i64::MAX as u64 + 1)
        );
        let too_big = to_value(&(u64::MAX as u128 + 1)).expect_err("out of range");
        assert!(matches!(
            too_big.kind(),
            ErrorKind::IntegerOutOfRange { .. }
        ));
        let too_small = to_value(&(i64::MIN as i128 - 1)).expect_err("out of range");
        assert!(matches!(
            too_small.kind(),
            ErrorKind::IntegerOutOfRange { .. }
        ));
    }

    #[test]
    fn bytes_become_a_sequence_of_integers() {
        #[derive(Serialize)]
        struct Blob(#[serde(with = "serde_bytes_shim")] Vec<u8>);

        mod serde_bytes_shim {
            pub(super) fn serialize<S: serde::Serializer>(
                bytes: &[u8],
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                serializer.serialize_bytes(bytes)
            }
        }

        assert_eq!(
            to_value(&Blob(vec![1, 2])).expect("bytes"),
            Value::Sequence(vec![Value::from(1u64), Value::from(2u64)])
        );
    }

    #[test]
    fn enum_variants_use_the_tagged_representation() {
        assert_eq!(to_value(&Shape::Point).expect("unit"), Value::from("Point"));

        let circle = to_value(&Shape::Circle(3)).expect("newtype");
        assert_eq!(circle.tag().map(Tag::as_str), Some("Circle"));
        assert_eq!(circle.untagged(), &Value::from(3u64));

        let segment = to_value(&Shape::Segment(1, 2)).expect("tuple");
        assert_eq!(segment.tag().map(Tag::as_str), Some("Segment"));
        assert_eq!(
            segment.untagged(),
            &Value::Sequence(vec![Value::from(1u64), Value::from(2u64)])
        );

        let rect = to_value(&Shape::Rect { w: 1, h: 2 }).expect("struct");
        assert_eq!(rect.tag().map(Tag::as_str), Some("Rect"));
        assert_eq!(rect.untagged().get("w"), Some(&Value::from(1u64)));
    }

    #[test]
    fn maps_and_structs_keep_their_insertion_order() {
        #[derive(Serialize)]
        struct Node {
            id: &'static str,
            outputs: Vec<&'static str>,
        }

        let value = to_value(&Node {
            id: "camera",
            outputs: vec!["frames"],
        })
        .expect("struct");
        let mapping = value.as_mapping().expect("mapping");
        let keys: Vec<&str> = mapping.keys().filter_map(Value::as_str).collect();
        assert_eq!(keys, vec!["id", "outputs"]);

        let map: BTreeMap<u8, &str> = BTreeMap::from([(2, "b"), (1, "a")]);
        let value = to_value(&map).expect("map");
        let mapping = value.as_mapping().expect("mapping");
        assert_eq!(mapping.get(&Value::from(1u64)), Some(&Value::from("a")));
    }

    #[test]
    fn nested_containers_survive_intact() {
        let value = to_value(&vec![vec![1u8, 2], vec![3]]).expect("nested");
        assert_eq!(
            value,
            Value::Sequence(vec![
                Value::Sequence(vec![Value::from(1u64), Value::from(2u64)]),
                Value::Sequence(vec![Value::from(3u64)]),
            ])
        );
        assert_eq!(to_value(&(1u8, "a")).expect("tuple"), value_of_tuple());
    }

    fn value_of_tuple() -> Value {
        Value::Sequence(vec![Value::from(1u64), Value::from("a")])
    }
}
