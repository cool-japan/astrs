//! [`DaemonId`] — one daemon's identity in the cluster.
//!
//! A daemon is identified by a UUID, optionally prefixed with the human
//! machine label from the manifest's `deploy.machine` (blueprint §8.3). The
//! label is what makes `astrs list` and the TUI readable ("`robot-arm-01`",
//! not a bare UUID); the UUID is what makes the identity unique across
//! restarts and across two daemons that claim the same label.
//!
//! # The text form, and why parsing runs right-to-left
//!
//! ```text
//! robot-arm-01-067e6162-3b6f-7c4e-9b4a-1f9d6b2c8a01
//! └────┬─────┘ └───────────────┬───────────────────┘
//!   machine                  uuid
//! ```
//!
//! Machine labels routinely contain hyphens, so splitting on the *first*
//! hyphen would tear `robot-arm-01` apart. A UUID's canonical form is always
//! exactly 36 characters, though, so the split point is unambiguous when
//! measured from the **right**: the last 36 characters are the UUID, the
//! character before them is the separator, and everything before that is the
//! label. That makes `Display` and `FromStr` exact inverses for every label
//! the grammar admits — including labels that themselves end in a hyphen, or
//! that look like a UUID.

use core::fmt;
use core::str::FromStr;

use oxicode::de::{Decode, Decoder};
use oxicode::enc::{Encode, Encoder};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::error::{IdError, IdKind, codec_invalid, preview};
use crate::ids::name::MachineName;
use crate::ids::uuid_ids::UUID_WIRE_LEN;

/// Length of a UUID in its canonical hyphenated text form.
pub const UUID_TEXT_LEN: usize = 36;

/// The character separating a machine label from the UUID.
pub const DAEMON_ID_SEPARATOR: char = '-';

/// The identity of one daemon.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DaemonId, MachineName};
/// use uuid::Uuid;
///
/// let uuid = Uuid::from_u128(0x067e_6162_3b6f_7c4e_9b4a_1f9d_6b2c_8a01);
/// let id = DaemonId::new(Some(MachineName::new("robot-arm-01")?), uuid);
///
/// let text = id.to_string();
/// assert!(text.starts_with("robot-arm-01-"));
/// assert_eq!(text.parse::<DaemonId>()?, id);
///
/// // Anonymous daemons render as a bare UUID.
/// let anonymous = DaemonId::new(None, uuid);
/// assert_eq!(anonymous.to_string().len(), 36);
/// assert_eq!(anonymous.to_string().parse::<DaemonId>()?, anonymous);
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DaemonId {
    /// The human-readable machine label, when the daemon was configured with
    /// one.
    machine: Option<MachineName>,
    /// The unique identity.
    uuid: Uuid,
}

impl DaemonId {
    /// Builds a daemon id from its parts.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DaemonId;
    /// use uuid::Uuid;
    ///
    /// let id = DaemonId::new(None, Uuid::from_u128(1));
    /// assert!(id.machine().is_none());
    /// ```
    #[must_use]
    pub const fn new(machine: Option<MachineName>, uuid: Uuid) -> Self {
        Self { machine, uuid }
    }

    /// Builds a daemon id from a label that has not been validated yet.
    ///
    /// # Errors
    ///
    /// [`IdError`] if `machine` is not a valid [`MachineName`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DaemonId;
    /// use uuid::Uuid;
    ///
    /// let id = DaemonId::from_parts(Some("lab-02"), Uuid::from_u128(1))?;
    /// assert_eq!(id.machine(), Some("lab-02"));
    /// assert!(DaemonId::from_parts(Some("bad name"), Uuid::from_u128(1)).is_err());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    pub fn from_parts(machine: Option<&str>, uuid: Uuid) -> Result<Self, IdError> {
        let machine = machine.map(MachineName::new).transpose()?;
        Ok(Self { machine, uuid })
    }

    /// Mints a fresh daemon id for `machine` using UUID v7.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{DaemonId, MachineName};
    ///
    /// let id = DaemonId::generate(Some(MachineName::new("lab-02")?));
    /// assert_eq!(id.machine(), Some("lab-02"));
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn generate(machine: Option<MachineName>) -> Self {
        Self {
            machine,
            uuid: Uuid::now_v7(),
        }
    }

    /// The machine label, if any.
    #[must_use]
    pub fn machine(&self) -> Option<&str> {
        self.machine.as_ref().map(MachineName::as_str)
    }

    /// The machine label as its validated newtype, if any.
    #[must_use]
    pub const fn machine_name(&self) -> Option<&MachineName> {
        self.machine.as_ref()
    }

    /// The unique identity.
    #[must_use]
    pub const fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// A label suitable for logs and metrics: the machine name when present,
    /// otherwise a short UUID prefix.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DaemonId;
    /// use uuid::Uuid;
    ///
    /// let named = DaemonId::from_parts(Some("lab-02"), Uuid::from_u128(1))?;
    /// assert_eq!(named.label(), "lab-02");
    ///
    /// let anonymous = DaemonId::new(None, Uuid::from_u128(1));
    /// assert_eq!(anonymous.label(), "00000000");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn label(&self) -> String {
        match self.machine() {
            Some(machine) => machine.to_owned(),
            None => self.uuid.to_string().chars().take(8).collect(),
        }
    }
}

impl fmt::Display for DaemonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.machine {
            Some(machine) => write!(f, "{machine}{DAEMON_ID_SEPARATOR}{}", self.uuid),
            None => fmt::Display::fmt(&self.uuid, f),
        }
    }
}

impl fmt::Debug for DaemonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DaemonId({self})")
    }
}

impl FromStr for DaemonId {
    type Err = IdError;

    /// Parses the text form, splitting the fixed-width UUID off the right.
    ///
    /// # Errors
    ///
    /// [`IdError::Malformed`] when the string is too short to contain a UUID,
    /// when the trailing 36 characters do not parse as one, or when the
    /// separator is missing. [`IdError`] from [`MachineName`] validation when
    /// the prefix is not a legal label.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let malformed = |reason: &'static str| IdError::Malformed {
            kind: IdKind::Machine,
            value: preview(text),
            reason,
        };

        // `len()` is a byte count, but the canonical UUID text form and the
        // separator are both ASCII, so byte offsets from the right are safe
        // as long as the tail slices land on char boundaries — which
        // `get()` verifies, returning `None` otherwise.
        let len = text.len();
        if len < UUID_TEXT_LEN {
            return Err(malformed("shorter than a uuid"));
        }

        let uuid_text = text
            .get(len - UUID_TEXT_LEN..)
            .ok_or_else(|| malformed("uuid suffix is not on a character boundary"))?;
        let uuid = Uuid::parse_str(uuid_text)
            .map_err(|_| malformed("trailing 36 characters are not a uuid"))?;

        if len == UUID_TEXT_LEN {
            return Ok(Self {
                machine: None,
                uuid,
            });
        }

        let prefix = text
            .get(..len - UUID_TEXT_LEN)
            .ok_or_else(|| malformed("machine prefix is not on a character boundary"))?;
        let machine_text = prefix
            .strip_suffix(DAEMON_ID_SEPARATOR)
            .ok_or_else(|| malformed("missing '-' between machine name and uuid"))?;
        if machine_text.is_empty() {
            return Err(malformed("empty machine name before the uuid"));
        }

        Ok(Self {
            machine: Some(MachineName::new(machine_text)?),
            uuid,
        })
    }
}

impl Serialize for DaemonId {
    /// Serialises as the canonical text form.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DaemonId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

impl Encode for DaemonId {
    /// Encodes as `Option<MachineName>` followed by sixteen raw UUID bytes.
    ///
    /// The parts are encoded separately rather than as the joined text form,
    /// so the wire cost is 17 bytes for an anonymous daemon instead of 37,
    /// and no re-parsing is needed on receipt.
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), oxicode::error::Error> {
        self.machine.encode(encoder)?;
        self.uuid.as_bytes().encode(encoder)
    }
}

impl<Context> Decode<Context> for DaemonId {
    fn decode<D: Decoder<Context = Context>>(
        decoder: &mut D,
    ) -> Result<Self, oxicode::error::Error> {
        let machine = Option::<MachineName>::decode(decoder)?;
        let bytes = <[u8; UUID_WIRE_LEN]>::decode(decoder)?;
        let uuid = Uuid::from_bytes(bytes);
        if let Some(machine) = &machine
            && machine.is_empty()
        {
            return Err(codec_invalid("daemon id carries an empty machine name"));
        }
        Ok(Self { machine, uuid })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    fn uuid() -> Uuid {
        Uuid::from_u128(0x067e_6162_3b6f_7c4e_9b4a_1f9d_6b2c_8a01)
    }

    #[test]
    fn text_form_round_trips_for_plain_labels() {
        let id = DaemonId::from_parts(Some("robot"), uuid()).unwrap();
        let text = id.to_string();
        assert_eq!(text, format!("robot-{}", uuid()));
        assert_eq!(text.parse::<DaemonId>().unwrap(), id);
    }

    #[test]
    fn text_form_round_trips_for_hyphenated_hostnames() {
        for machine in [
            "robot-arm-01",
            "a-b-c-d-e-f",
            "lab.rack-3.node-11",
            "x-1",
            "-leading-is-illegal-so-not-here",
        ] {
            if MachineName::new(machine).is_err() {
                continue;
            }
            let id = DaemonId::from_parts(Some(machine), uuid()).unwrap();
            let parsed: DaemonId = id.to_string().parse().unwrap();
            assert_eq!(parsed, id, "failed for {machine}");
            assert_eq!(parsed.machine(), Some(machine));
        }
    }

    #[test]
    fn text_form_round_trips_when_the_label_ends_in_a_hyphen() {
        // The pathological case: the label's own trailing hyphen sits right
        // next to the separator.
        let id = DaemonId::from_parts(Some("robot-"), uuid()).unwrap();
        let text = id.to_string();
        assert_eq!(text, format!("robot--{}", uuid()));
        let parsed: DaemonId = text.parse().unwrap();
        assert_eq!(parsed, id);
        assert_eq!(parsed.machine(), Some("robot-"));
    }

    #[test]
    fn text_form_round_trips_when_the_label_looks_like_a_uuid() {
        let decoy = Uuid::from_u128(0xFFFF).to_string();
        let id = DaemonId::from_parts(Some(&decoy), uuid()).unwrap();
        let parsed: DaemonId = id.to_string().parse().unwrap();
        assert_eq!(parsed, id);
        assert_eq!(parsed.machine(), Some(decoy.as_str()));
        assert_eq!(parsed.uuid(), uuid());
    }

    #[test]
    fn text_form_round_trips_for_anonymous_daemons() {
        let id = DaemonId::new(None, uuid());
        let text = id.to_string();
        assert_eq!(text.len(), UUID_TEXT_LEN);
        let parsed: DaemonId = text.parse().unwrap();
        assert_eq!(parsed, id);
        assert!(parsed.machine().is_none());
    }

    #[test]
    fn text_form_round_trips_for_labels_of_every_length() {
        for len in 1..40usize {
            let machine = "m".repeat(len);
            let id = DaemonId::from_parts(Some(&machine), uuid()).unwrap();
            assert_eq!(id.to_string().parse::<DaemonId>().unwrap(), id);
        }
    }

    #[test]
    fn malformed_text_is_rejected() {
        let cases = [
            "",
            "short",
            "not-a-uuid-at-all-not-a-uuid-at-all-x",
            "robot-00000000-0000-0000-0000-00000000000",
        ];
        for case in cases {
            assert!(
                case.parse::<DaemonId>().is_err(),
                "{case:?} should not parse"
            );
        }
    }

    #[test]
    fn a_missing_separator_is_rejected() {
        let text = format!("robot{}", uuid());
        match text.parse::<DaemonId>() {
            Err(IdError::Malformed { reason, .. }) => assert!(reason.contains('-')),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_empty_machine_prefix_is_rejected() {
        let text = format!("-{}", uuid());
        assert!(text.parse::<DaemonId>().is_err());
    }

    #[test]
    fn an_illegal_machine_prefix_is_rejected() {
        let text = format!("has space-{}", uuid());
        match text.parse::<DaemonId>() {
            Err(IdError::InvalidChar { ch, .. }) => assert_eq!(ch, ' '),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn multibyte_input_does_not_panic() {
        // A hostile peer sending non-ASCII must produce an error, never a
        // slice-on-non-char-boundary panic.
        for text in [
            "日本語のホスト名です-これはとても長い文字列です",
            "🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂",
        ] {
            assert!(text.parse::<DaemonId>().is_err());
        }
    }

    #[test]
    fn codec_round_trips_both_shapes() {
        for machine in [None, Some("robot-arm-01")] {
            let id = DaemonId::from_parts(machine, uuid()).unwrap();
            let bytes = id.encode_to_vec().unwrap();
            assert_eq!(DaemonId::decode_exact(&bytes).unwrap(), id);
        }
    }

    #[test]
    fn anonymous_daemon_ids_are_compact_on_the_wire() {
        let id = DaemonId::new(None, uuid());
        // One byte for `Option::None`, sixteen for the uuid.
        assert_eq!(id.encode_to_vec().unwrap().len(), 1 + UUID_WIRE_LEN);
    }

    #[test]
    fn decoding_validates_the_machine_name() {
        // Forge a payload whose machine name is not a legal label.
        let forged = (Some("bad name".to_owned()), [0u8; UUID_WIRE_LEN])
            .encode_to_vec()
            .unwrap();
        assert!(DaemonId::decode_exact(&forged).is_err());
    }

    #[test]
    fn serde_uses_the_text_form() {
        let id = DaemonId::from_parts(Some("lab-02"), uuid()).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"lab-02-{}\"", uuid()));
        assert_eq!(serde_json::from_str::<DaemonId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<DaemonId>("\"nope\"").is_err());
    }

    #[test]
    fn label_prefers_the_machine_name() {
        assert_eq!(
            DaemonId::from_parts(Some("lab-02"), uuid())
                .unwrap()
                .label(),
            "lab-02"
        );
        assert_eq!(DaemonId::new(None, uuid()).label(), "067e6162");
    }

    #[test]
    fn accessors_and_debug() {
        let id = DaemonId::from_parts(Some("m1"), uuid()).unwrap();
        assert_eq!(id.uuid(), uuid());
        assert_eq!(id.machine_name().map(MachineName::as_str), Some("m1"));
        assert_eq!(format!("{id:?}"), format!("DaemonId({id})"));
    }

    #[test]
    fn generate_produces_distinct_ids() {
        let a = DaemonId::generate(Some(MachineName::new("m").unwrap()));
        let b = DaemonId::generate(Some(MachineName::new("m").unwrap()));
        assert_ne!(a, b);
        assert_eq!(a.machine(), Some("m"));
    }
}
