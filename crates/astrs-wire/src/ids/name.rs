//! Name-shaped identifiers: `[A-Za-z0-9_.-]+`.
//!
//! Node ids, port (data) ids, operator ids, machine labels and parameter keys
//! all share one grammar. Keeping it narrow is deliberate: these strings end
//! up in shared-memory segment names (`{dataflow_id}/{node_id}/{generation}`,
//! §6.2), file paths, metrics labels, log lines and CLI arguments, and every
//! character outside the set is a quoting hazard in at least one of those.
//!
//! **Validation applies on every construction path.** `FromStr`, `TryFrom`,
//! `serde::Deserialize` and `oxicode::Decode` all funnel through the same
//! validator, so a hostile peer cannot inject an unvalidated identifier by
//! encoding one directly into a frame — and the byte cap enforced during
//! decoding means a forged length prefix cannot allocate without bound even
//! inside an otherwise legal frame.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DataId, NodeId};
//!
//! let node: NodeId = "camera-01".parse()?;
//! let port: DataId = "image".parse()?;
//! assert_eq!(node.as_str(), "camera-01");
//! assert!("bad id".parse::<NodeId>().is_err());
//! assert!("".parse::<DataId>().is_err());
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;
use core::str::FromStr;

use oxicode::de::{Decode, Decoder};
use oxicode::enc::{Encode, Encoder};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{IdError, IdKind, codec_invalid, preview};

/// The maximum length, in bytes, of a name-shaped identifier.
///
/// 255 keeps every identifier inside a single length byte's worth of range and
/// leaves room for the composite names built from them (a POSIX shared-memory
/// object name is limited to 255 bytes on Linux and 31 on macOS, so composites
/// are bounded separately by `astrs-shm`).
pub const MAX_NAME_LEN: usize = 255;

/// Whether `ch` is admissible inside a name-shaped identifier.
///
/// The set is exactly `[A-Za-z0-9_.-]`.
///
/// # Examples
///
/// ```
/// use astrs_wire::ids::is_name_char;
///
/// assert!(is_name_char('a'));
/// assert!(is_name_char('_'));
/// assert!(is_name_char('-'));
/// assert!(is_name_char('.'));
/// assert!(!is_name_char('/'));
/// assert!(!is_name_char(' '));
/// ```
#[must_use]
pub const fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')
}

/// Validates a name-shaped identifier.
///
/// # Errors
///
/// - [`IdError::Empty`] for the empty string.
/// - [`IdError::TooLong`] beyond `max` bytes.
/// - [`IdError::InvalidChar`] for the first character outside the grammar.
///
/// # Examples
///
/// ```
/// use astrs_wire::error::IdKind;
/// use astrs_wire::ids::{MAX_NAME_LEN, validate_name};
///
/// assert!(validate_name(IdKind::Node, "camera.left-01", MAX_NAME_LEN).is_ok());
/// assert!(validate_name(IdKind::Node, "camera/left", MAX_NAME_LEN).is_err());
/// ```
pub fn validate_name(kind: IdKind, value: &str, max: usize) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if value.len() > max {
        return Err(IdError::TooLong {
            kind,
            len: value.len(),
            max,
        });
    }
    for (index, ch) in value.char_indices() {
        if !is_name_char(ch) {
            return Err(IdError::InvalidChar {
                kind,
                value: preview(value),
                ch,
                index,
            });
        }
    }
    Ok(())
}

/// Defines a validated, name-shaped identifier newtype.
///
/// Every generated type gets the identical construction, comparison,
/// formatting, `serde` and `oxicode` behaviour, so there is exactly one
/// validator and one wire encoding to reason about. The macro exists to keep
/// that guarantee mechanical rather than to save typing.
macro_rules! define_name_id {
    (
        $(#[$meta:meta])*
        $name:ident, $kind:expr, $noun:literal
    ) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// The identifier family this type belongs to.
            pub const KIND: IdKind = $kind;

            /// The maximum length in bytes.
            pub const MAX_LEN: usize = MAX_NAME_LEN;

            #[doc = concat!("Validates `value` and wraps it as a ", $noun, ".")]
            ///
            /// # Errors
            ///
            /// [`IdError`] if the value is empty, over
            #[doc = concat!("[`MAX_NAME_LEN`] bytes, or outside `[A-Za-z0-9_.-]`.")]
            ///
            /// # Examples
            ///
            /// ```
            #[doc = concat!("use astrs_wire::", stringify!($name), ";")]
            ///
            #[doc = concat!("assert!(", stringify!($name), "::new(\"valid-01\").is_ok());")]
            #[doc = concat!("assert!(", stringify!($name), "::new(\"in valid\").is_err());")]
            /// ```
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_name(Self::KIND, &value, Self::MAX_LEN)?;
                Ok(Self(value))
            }

            #[doc = concat!("Wraps `value` as a ", $noun, ", repairing it if it does not fit the grammar.")]
            ///
            /// A **total** constructor, for callers that must produce an
            /// identifier and have no error path to take: a reserved literal
            /// this crate itself defines, a metrics label derived from
            /// operator-supplied text, a fallback in a `Display` impl.
            /// Characters outside `[A-Za-z0-9_.-]` become `_`, the result is
            /// truncated to [`MAX_NAME_LEN`] bytes on a character boundary,
            /// and an empty result becomes `"_"` — so the returned value
            /// always satisfies the same invariant [`Self::new`] enforces.
            ///
            /// Prefer [`Self::new`] wherever a malformed name is a real error
            /// worth reporting; silent repair is the right answer only when
            /// there is nobody to report it to.
            ///
            /// # Examples
            ///
            /// ```
            #[doc = concat!("use astrs_wire::", stringify!($name), ";")]
            ///
            #[doc = concat!("assert_eq!(", stringify!($name), "::sanitized(\"camera.image\").as_str(), \"camera.image\");")]
            #[doc = concat!("assert_eq!(", stringify!($name), "::sanitized(\"in valid\").as_str(), \"in_valid\");")]
            #[doc = concat!("assert_eq!(", stringify!($name), "::sanitized(\"\").as_str(), \"_\");")]
            /// ```
            #[must_use]
            pub fn sanitized(value: &str) -> Self {
                let mut repaired = String::with_capacity(value.len().min(Self::MAX_LEN));
                for ch in value.chars() {
                    let ch = if is_name_char(ch) { ch } else { '_' };
                    if repaired.len() + ch.len_utf8() > Self::MAX_LEN {
                        break;
                    }
                    repaired.push(ch);
                }
                if repaired.is_empty() {
                    repaired.push('_');
                }
                Self(repaired)
            }

            /// The identifier as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the identifier and returns the inner [`String`].
            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }

            /// The identifier's length in bytes.
            #[must_use]
            pub fn len(&self) -> usize {
                self.0.len()
            }

            /// Always `false` — a validated identifier is never empty.
            ///
            /// Provided because clippy (rightly) expects any type with `len`
            /// to offer `is_empty`; the constant answer documents the
            /// invariant.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                false
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl core::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }

        impl Encode for $name {
            fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), oxicode::error::Error> {
                self.0.encode(encoder)
            }
        }

        impl<Context> Decode<Context> for $name {
            fn decode<D: Decoder<Context = Context>>(
                decoder: &mut D,
            ) -> Result<Self, oxicode::error::Error> {
                let raw = String::decode(decoder)?;
                Self::new(raw).map_err(|err| codec_invalid(err.to_string()))
            }
        }
    };
}

define_name_id! {
    /// The id of a node in a dataflow — the `id:` field of a manifest node
    /// (blueprint §8.3).
    ///
    /// Module expansion produces dotted composites (`parent.child`, §8.5),
    /// which is why `.` is part of the grammar.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::NodeId;
    ///
    /// let id: NodeId = "perception.detector".parse()?;
    /// assert_eq!(id.as_str(), "perception.detector");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    NodeId, IdKind::Node, "node id"
}

define_name_id! {
    /// The id of an input or output port — a manifest `outputs:` entry, or
    /// the port half of an `inputs:` entry.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DataId;
    ///
    /// let id: DataId = "detections".parse()?;
    /// assert_eq!(id.to_string(), "detections");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    DataId, IdKind::Data, "data id"
}

define_name_id! {
    /// The id of an operator hosted inside a runtime node (blueprint §9.3).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::OperatorId;
    ///
    /// let id: OperatorId = "nms".parse()?;
    /// assert_eq!(id.as_str(), "nms");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    OperatorId, IdKind::Operator, "operator id"
}

define_name_id! {
    /// A machine label: the `deploy.machine` of a manifest node and the
    /// human-readable half of a [`crate::DaemonId`].
    ///
    /// Hostnames routinely contain hyphens and dots (`robot-arm-01.lab`),
    /// both of which the grammar admits.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::MachineName;
    ///
    /// let machine: MachineName = "robot-arm-01.lab".parse()?;
    /// assert_eq!(machine.as_str(), "robot-arm-01.lab");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    MachineName, IdKind::Machine, "machine name"
}

define_name_id! {
    /// A key in a [`crate::Metadata`] parameter map (blueprint §6.1).
    ///
    /// Keys beginning with `_` are AstRS-internal and stripped before user
    /// delivery — see [`crate::ParamKey::is_internal`] and
    /// [`crate::Metadata::strip_internal`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::ParamKey;
    ///
    /// let key: ParamKey = "request_id".parse()?;
    /// assert!(!key.is_internal());
    /// assert!(ParamKey::new("_schema_hash")?.is_internal());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    ParamKey, IdKind::Param, "parameter key"
}

impl ParamKey {
    /// Whether this key is AstRS-internal.
    ///
    /// Internal keys start with `_`; they carry plumbing (the schema hash,
    /// trace context) and are removed by [`crate::Metadata::strip_internal`]
    /// before an event reaches user code.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::ParamKey;
    ///
    /// assert!(ParamKey::new("_schema_hash")?.is_internal());
    /// assert!(!ParamKey::new("seq")?.is_internal());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn is_internal(&self) -> bool {
        self.as_str().starts_with('_')
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn accepts_the_full_grammar() {
        let all = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.-";
        assert!(NodeId::new(all).is_ok());
    }

    #[test]
    fn rejects_the_empty_string() {
        for result in [
            NodeId::new("").err(),
            DataId::new("").err(),
            OperatorId::new("").err(),
            MachineName::new("").err(),
            ParamKey::new("").err(),
        ] {
            assert!(matches!(result, Some(IdError::Empty { .. })));
        }
    }

    #[test]
    fn rejects_characters_outside_the_grammar() {
        for bad in ["a/b", "a b", "a:b", "a\u{0}b", "café", "a\nb", "a+b", "a*b"] {
            match NodeId::new(bad) {
                Err(IdError::InvalidChar { kind, .. }) => assert_eq!(kind, IdKind::Node),
                other => panic!("{bad:?} should be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn reports_the_first_offending_character_and_offset() {
        match DataId::new("ok.then/bad") {
            Err(IdError::InvalidChar { ch, index, .. }) => {
                assert_eq!(ch, '/');
                assert_eq!(index, 7);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn enforces_the_length_cap() {
        let at_limit = "a".repeat(MAX_NAME_LEN);
        assert!(NodeId::new(&at_limit).is_ok());

        let over = "a".repeat(MAX_NAME_LEN + 1);
        match NodeId::new(&over) {
            Err(IdError::TooLong { len, max, .. }) => {
                assert_eq!(len, MAX_NAME_LEN + 1);
                assert_eq!(max, MAX_NAME_LEN);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn error_messages_do_not_echo_unbounded_input() {
        let hostile = format!("{}/", "z".repeat(10_000));
        let err = NodeId::new(&hostile).unwrap_err();
        assert!(err.to_string().len() < 200, "message: {err}");
    }

    #[test]
    fn accessors_agree() {
        let id = NodeId::new("camera").unwrap();
        assert_eq!(id.as_str(), "camera");
        assert_eq!(id.as_ref(), "camera");
        assert_eq!(id.len(), 6);
        assert!(!id.is_empty());
        assert_eq!(id.to_string(), "camera");
        assert_eq!(id, "camera");
        assert_eq!(id.clone().into_string(), "camera");
        assert_eq!(format!("{id:?}"), "NodeId(\"camera\")");
    }

    #[test]
    fn borrow_enables_str_lookup_in_maps() {
        use std::collections::BTreeMap;

        let mut map = BTreeMap::new();
        map.insert(NodeId::new("camera").unwrap(), 1);
        assert_eq!(map.get("camera"), Some(&1));
    }

    #[test]
    fn ordering_is_lexicographic() {
        let mut ids = [
            NodeId::new("zeta").unwrap(),
            NodeId::new("alpha").unwrap(),
            NodeId::new("Beta").unwrap(),
        ];
        ids.sort();
        assert_eq!(ids[0].as_str(), "Beta");
        assert_eq!(ids[1].as_str(), "alpha");
        assert_eq!(ids[2].as_str(), "zeta");
    }

    #[test]
    fn codec_round_trips() {
        let id = DataId::new("image.raw-0").unwrap();
        let bytes = id.encode_to_vec().unwrap();
        assert_eq!(DataId::decode_exact(&bytes).unwrap(), id);
    }

    #[test]
    fn decoding_validates_just_like_parsing() {
        // A peer that encodes a raw String where a NodeId belongs must be
        // rejected: the validator runs on the decode path too.
        let hostile = "not a valid id".to_owned().encode_to_vec().unwrap();
        let err = NodeId::decode_exact(&hostile).unwrap_err();
        assert!(matches!(err, crate::WireError::Codec(_)), "{err:?}");
    }

    #[test]
    fn decoding_enforces_the_length_cap() {
        let hostile = "a".repeat(MAX_NAME_LEN + 1).encode_to_vec().unwrap();
        assert!(NodeId::decode_exact(&hostile).is_err());
    }

    #[test]
    fn decoding_rejects_the_empty_string() {
        let empty = String::new().encode_to_vec().unwrap();
        assert!(DataId::decode_exact(&empty).is_err());
    }

    #[test]
    fn serde_round_trips_as_a_bare_string() {
        let id = NodeId::new("planner").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"planner\"");
        assert_eq!(serde_json::from_str::<NodeId>(&json).unwrap(), id);
    }

    #[test]
    fn serde_deserialization_validates() {
        assert!(serde_json::from_str::<NodeId>("\"has space\"").is_err());
        assert!(serde_json::from_str::<NodeId>("\"\"").is_err());
    }

    #[test]
    fn try_from_variants_are_equivalent() {
        assert_eq!(
            NodeId::try_from("x").unwrap(),
            NodeId::try_from("x".to_owned()).unwrap()
        );
        assert_eq!("x".parse::<NodeId>().unwrap(), NodeId::new("x").unwrap());
    }

    #[test]
    fn each_type_reports_its_own_kind() {
        assert_eq!(NodeId::KIND, IdKind::Node);
        assert_eq!(DataId::KIND, IdKind::Data);
        assert_eq!(OperatorId::KIND, IdKind::Operator);
        assert_eq!(MachineName::KIND, IdKind::Machine);
        assert_eq!(ParamKey::KIND, IdKind::Param);

        let err = DataId::new("!").unwrap_err();
        assert_eq!(err.kind(), IdKind::Data);
    }

    #[test]
    fn param_keys_flag_internal_names() {
        assert!(ParamKey::new("_schema_hash").unwrap().is_internal());
        assert!(ParamKey::new("_trace").unwrap().is_internal());
        assert!(!ParamKey::new("seq").unwrap().is_internal());
        assert!(!ParamKey::new("goal.id").unwrap().is_internal());
    }

    #[test]
    fn name_char_predicate_matches_the_validator() {
        for byte in 0u8..=127 {
            let ch = char::from(byte);
            let mut buffer = String::from("a");
            buffer.push(ch);
            let accepted = NodeId::new(&buffer).is_ok();
            assert_eq!(accepted, is_name_char(ch), "disagreement for {ch:?}");
        }
    }

    #[test]
    fn sanitized_keeps_a_legal_name_unchanged() {
        for value in ["camera", "astrs.status", "node-01", "a_b.c-d"] {
            assert_eq!(NodeId::sanitized(value).as_str(), value);
            assert_eq!(DataId::sanitized(value).as_str(), value);
        }
    }

    #[test]
    fn sanitized_repairs_illegal_characters() {
        assert_eq!(NodeId::sanitized("in valid").as_str(), "in_valid");
        assert_eq!(NodeId::sanitized("a/b").as_str(), "a_b");
        assert_eq!(NodeId::sanitized("a=b\0c").as_str(), "a_b_c");
    }

    #[test]
    fn sanitized_never_returns_an_empty_name() {
        assert_eq!(NodeId::sanitized("").as_str(), "_");
        assert_eq!(DataId::sanitized("").as_str(), "_");
    }

    #[test]
    fn sanitized_truncates_on_a_character_boundary() {
        let long = "\u{00e9}".repeat(MAX_NAME_LEN);
        let repaired = NodeId::sanitized(&long);
        assert!(repaired.len() <= MAX_NAME_LEN);
        // Every source character is multi-byte and therefore illegal, so each
        // becomes a single-byte '_'; the result fills the budget exactly.
        assert_eq!(repaired.as_str(), "_".repeat(MAX_NAME_LEN));
    }

    #[test]
    fn sanitized_output_always_passes_the_validator() {
        for value in [
            "",
            "  ",
            "a/b/c",
            "\u{1f600}",
            &"x".repeat(MAX_NAME_LEN * 3),
        ] {
            let repaired = NodeId::sanitized(value);
            assert!(
                NodeId::new(repaired.as_str()).is_ok(),
                "{value:?} repaired to {repaired:?}, which is still invalid"
            );
        }
    }
}
