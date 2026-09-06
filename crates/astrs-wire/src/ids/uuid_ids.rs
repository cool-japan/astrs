//! UUID-shaped identifiers: dataflows, sessions and builds.
//!
//! Each is a distinct newtype rather than a bare `Uuid` so the compiler
//! catches the "passed a session id where a dataflow id belonged" class of
//! bug, which is otherwise invisible: both are sixteen bytes.
//!
//! # Wire encoding
//!
//! All three encode as **sixteen raw bytes** — `oxicode` writes a `[u8; 16]`
//! without a length prefix, so a UUID costs exactly sixteen bytes on the wire,
//! not the twenty-two a varint `u128` would take (and not the thirty-seven a
//! string form would).
//!
//! # Generation
//!
//! Sessions and builds are minted with **UUID v7** (blueprint: time-ordered
//! ids), so a lexical sort of ids is a chronological sort — which is what makes
//! `astrs list` and the coordinator's build cache index cheap to page through.
//! Dataflow ids use the same generator for the same reason.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DataflowId, SessionId};
//!
//! let dataflow = DataflowId::from_u128(1);
//! assert_eq!(dataflow.to_string(), "00000000-0000-0000-0000-000000000001");
//!
//! let a = SessionId::generate();
//! let b = SessionId::generate();
//! assert_ne!(a, b);
//! ```

use core::fmt;
use core::str::FromStr;

use oxicode::de::Decoder;
use oxicode::enc::Encoder;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The number of bytes a UUID occupies on the wire.
pub const UUID_WIRE_LEN: usize = 16;

/// Defines a UUID-backed identifier newtype.
macro_rules! define_uuid_id {
    (
        $(#[$meta:meta])*
        $name:ident, $noun:literal
    ) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// The all-zero identifier.
            ///
            /// A sentinel for "not yet assigned"; never returned by
            #[doc = concat!("[`", stringify!($name), "::generate`].")]
            pub const NIL: Self = Self(Uuid::nil());

            #[doc = concat!("Mints a fresh ", $noun, " using UUID v7 (time-ordered).")]
            ///
            /// # Examples
            ///
            /// ```
            #[doc = concat!("use astrs_wire::", stringify!($name), ";")]
            ///
            #[doc = concat!("let id = ", stringify!($name), "::generate();")]
            #[doc = concat!("assert_ne!(id, ", stringify!($name), "::NIL);")]
            /// ```
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            #[doc = concat!("Wraps an existing [`Uuid`] as a ", $noun, ".")]
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying [`Uuid`].
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            /// Consumes the identifier and returns the underlying [`Uuid`].
            #[must_use]
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }

            /// Builds an identifier from a 128-bit integer.
            ///
            /// Deterministic — used by tests and by the protocol snapshot,
            /// which must produce byte-identical samples on every run.
            #[must_use]
            pub const fn from_u128(value: u128) -> Self {
                Self(Uuid::from_u128(value))
            }

            /// The identifier as a 128-bit integer.
            #[must_use]
            pub const fn as_u128(&self) -> u128 {
                self.0.as_u128()
            }

            /// Builds an identifier from its sixteen wire bytes.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; UUID_WIRE_LEN]) -> Self {
                Self(Uuid::from_bytes(bytes))
            }

            /// The identifier's sixteen wire bytes.
            #[must_use]
            pub const fn to_bytes(&self) -> [u8; UUID_WIRE_LEN] {
                *self.0.as_bytes()
            }

            /// Whether this is the all-zero sentinel.
            #[must_use]
            pub fn is_nil(&self) -> bool {
                self.0.is_nil()
            }
        }

        impl fmt::Display for $name {
            /// The canonical hyphenated lower-case form, e.g.
            /// `067e6162-3b6f-7c4e-9b4a-1f9d6b2c8a01`.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(value)?))
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl Encode for $name {
            fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), oxicode::error::Error> {
                self.0.as_bytes().encode(encoder)
            }
        }

        impl<Context> Decode<Context> for $name {
            fn decode<D: Decoder<Context = Context>>(
                decoder: &mut D,
            ) -> Result<Self, oxicode::error::Error> {
                let bytes = <[u8; UUID_WIRE_LEN]>::decode(decoder)?;
                Ok(Self(Uuid::from_bytes(bytes)))
            }
        }
    };
}

define_uuid_id! {
    /// The identity of a running dataflow.
    ///
    /// Minted by the coordinator at `astrs start` and carried by every
    /// subsequent control message about that dataflow. It also names the
    /// shared-memory segments of its nodes (`{dataflow_id}/{node_id}/
    /// {generation}`, blueprint §6.2), which is why a stale segment is always
    /// attributable.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DataflowId;
    ///
    /// let id = DataflowId::generate();
    /// let text = id.to_string();
    /// assert_eq!(text.parse::<DataflowId>()?, id);
    /// # Ok::<(), uuid::Error>(())
    /// ```
    DataflowId, "dataflow id"
}

define_uuid_id! {
    /// The identity of one connection session.
    ///
    /// Assigned by the accepting side in `Welcome` and echoed in
    /// reconnect handshakes so a daemon returning from a partition
    /// (blueprint §12, *degraded-autonomous* mode) can be recognised as the
    /// same participant and resynchronised with `StateCatchUp` instead of
    /// re-registered from scratch.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::SessionId;
    ///
    /// let session = SessionId::generate();
    /// assert!(!session.is_nil());
    /// ```
    SessionId, "session id"
}

define_uuid_id! {
    /// The identity of one build of a dataflow.
    ///
    /// `astrs build` returns one; `astrs start --build <id>` consumes it, and
    /// the coordinator's build cache index is keyed by it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::BuildId;
    ///
    /// let build = BuildId::from_u128(7);
    /// assert_eq!(build.as_u128(), 7);
    /// ```
    BuildId, "build id"
}

/// The identity of a log, topic or telemetry subscription.
///
/// Blueprint §7.3: log and topic fan-out to the CLI rides the ordinary framing
/// with a `SubscriptionId` in the payload, rather than a bespoke binary-prefix
/// side channel. Subscriptions are per-connection, so a monotonically
/// increasing 64-bit counter is enough — no UUID needed, and the smaller
/// encoding matters on a high-rate `astrs topic echo`.
///
/// # Examples
///
/// ```
/// use astrs_wire::SubscriptionId;
///
/// let first = SubscriptionId::FIRST;
/// let second = first.next();
/// assert_eq!(first.get(), 1);
/// assert_eq!(second.get(), 2);
/// assert!(second > first);
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(transparent)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    /// The reserved "no subscription" value.
    pub const NONE: Self = Self(0);

    /// The first id an allocator hands out.
    pub const FIRST: Self = Self(1);

    /// Wraps a raw counter value.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::SubscriptionId;
    ///
    /// assert_eq!(SubscriptionId::new(42).get(), 42);
    /// ```
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw counter value.
    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0
    }

    /// Whether this is the reserved "no subscription" value.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        self.0 == 0
    }

    /// The next id in sequence.
    ///
    /// Saturates at [`u64::MAX`] rather than wrapping: reusing a live
    /// subscription id would misroute a stream, which is worse than refusing
    /// to allocate after 2^64 subscriptions on one connection.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::SubscriptionId;
    ///
    /// assert_eq!(SubscriptionId::new(1).next().get(), 2);
    /// assert_eq!(SubscriptionId::new(u64::MAX).next().get(), u64::MAX);
    /// ```
    #[must_use]
    pub const fn next(&self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<u64> for SubscriptionId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<SubscriptionId> for u64 {
    fn from(id: SubscriptionId) -> Self {
        id.0
    }
}

impl FromStr for SubscriptionId {
    type Err = core::num::ParseIntError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.parse()?))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn uuids_encode_as_sixteen_raw_bytes() {
        let id = DataflowId::from_u128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10);
        let bytes = id.encode_to_vec().unwrap();
        assert_eq!(bytes.len(), UUID_WIRE_LEN);
        assert_eq!(
            bytes,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert_eq!(id.encode_size_hint().unwrap(), UUID_WIRE_LEN);
    }

    #[test]
    fn uuid_ids_round_trip_through_the_codec() {
        let dataflow = DataflowId::generate();
        let session = SessionId::generate();
        let build = BuildId::generate();

        assert_eq!(
            DataflowId::decode_exact(&dataflow.encode_to_vec().unwrap()).unwrap(),
            dataflow
        );
        assert_eq!(
            SessionId::decode_exact(&session.encode_to_vec().unwrap()).unwrap(),
            session
        );
        assert_eq!(
            BuildId::decode_exact(&build.encode_to_vec().unwrap()).unwrap(),
            build
        );
    }

    #[test]
    fn generated_ids_are_version_seven_and_distinct() {
        let first = SessionId::generate();
        let second = SessionId::generate();
        assert_ne!(first, second);
        assert_eq!(first.as_uuid().get_version_num(), 7);
        assert!(!first.is_nil());
    }

    #[test]
    fn version_seven_ids_sort_chronologically() {
        let mut ids: Vec<BuildId> = (0..8).map(|_| BuildId::generate()).collect();
        let generated = ids.clone();
        ids.sort();
        assert_eq!(ids, generated, "v7 ids must already be in creation order");
    }

    #[test]
    fn nil_is_the_zero_value() {
        assert!(DataflowId::NIL.is_nil());
        assert_eq!(DataflowId::NIL.as_u128(), 0);
        assert_eq!(DataflowId::NIL.to_bytes(), [0u8; UUID_WIRE_LEN]);
    }

    #[test]
    fn text_form_round_trips() {
        let id = DataflowId::from_u128(0xDEAD_BEEF_CAFE_F00D_0011_2233_4455_6677);
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(text.parse::<DataflowId>().unwrap(), id);
        assert!(text.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn parsing_a_non_uuid_fails() {
        assert!("not-a-uuid".parse::<DataflowId>().is_err());
        assert!("".parse::<SessionId>().is_err());
    }

    #[test]
    fn byte_and_integer_forms_agree() {
        let id = BuildId::from_u128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10);
        assert_eq!(BuildId::from_bytes(id.to_bytes()), id);
        assert_eq!(BuildId::from_uuid(id.into_uuid()), id);
        assert_eq!(Uuid::from(id), *id.as_uuid());
        assert_eq!(BuildId::from(Uuid::from(id)), id);
    }

    #[test]
    fn serde_uses_the_hyphenated_text_form() {
        let id = SessionId::from_u128(1);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00000000-0000-0000-0000-000000000001\"");
        assert_eq!(serde_json::from_str::<SessionId>(&json).unwrap(), id);
    }

    #[test]
    fn debug_names_the_type() {
        let id = SessionId::from_u128(1);
        assert!(format!("{id:?}").starts_with("SessionId("));
    }

    #[test]
    fn subscription_ids_count_and_saturate() {
        assert!(SubscriptionId::NONE.is_none());
        assert!(!SubscriptionId::FIRST.is_none());
        assert_eq!(SubscriptionId::default(), SubscriptionId::NONE);
        assert_eq!(SubscriptionId::FIRST.next().get(), 2);
        assert_eq!(SubscriptionId::new(u64::MAX).next().get(), u64::MAX);
        assert_eq!(SubscriptionId::from(5u64).get(), 5);
        assert_eq!(u64::from(SubscriptionId::new(5)), 5);
        assert_eq!("77".parse::<SubscriptionId>().unwrap().get(), 77);
        assert!("x".parse::<SubscriptionId>().is_err());
        assert_eq!(SubscriptionId::new(9).to_string(), "9");
    }

    #[test]
    fn subscription_ids_round_trip_and_stay_compact() {
        let id = SubscriptionId::new(1);
        let bytes = id.encode_to_vec().unwrap();
        assert_eq!(bytes.len(), 1, "small ids must cost one varint byte");
        assert_eq!(SubscriptionId::decode_exact(&bytes).unwrap(), id);

        let big = SubscriptionId::new(u64::MAX);
        assert_eq!(
            SubscriptionId::decode_exact(&big.encode_to_vec().unwrap()).unwrap(),
            big
        );
    }

    #[test]
    fn a_truncated_uuid_payload_is_rejected() {
        let id = DataflowId::from_u128(1);
        let bytes = id.encode_to_vec().unwrap();
        assert!(DataflowId::decode_exact(&bytes[..15]).is_err());
    }

    #[test]
    fn trailing_bytes_after_a_uuid_are_rejected() {
        let mut bytes = DataflowId::from_u128(1).encode_to_vec().unwrap();
        bytes.push(0);
        assert!(matches!(
            DataflowId::decode_exact(&bytes),
            Err(crate::WireError::TrailingBytes { .. })
        ));
    }
}
