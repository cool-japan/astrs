//! Submessage identifiers: the first octet of every submessage header.
//!
//! OMG DDSI-RTPS 2.3 §9.4.5.1.1 assigns thirteen values and splits the range
//! in two: `0x00`–`0x7f` belongs to the protocol, `0x80`–`0xff` to vendors.
//! A receiver that meets an id it does not know **must skip the submessage
//! and keep parsing** (§8.3.4.1) — the forward-compatibility rule that lets a
//! 2.3 participant talk to a 2.5 one — and that is why
//! [`SubmessageId`] models unknown ids rather than rejecting them.
//!
//! Five further ids, `0x30`–`0x34`, are assigned out of the same protocol
//! range by DDS-Security 1.1 §7.3.6. They are named here — a dropped datagram
//! should say `SEC_PREFIX`, not `UNKNOWN(0x31)` — and
//! [`SubmessageId::is_security`] separates them from the thirteen of
//! DDSI-RTPS proper. Naming them changes nothing about how the message model
//! treats them: their bodies stay undecoded octets, which is exactly what a
//! participant with no keys should do with them.
//!
//! ```
//! use astrs_rtps::messages::SubmessageId;
//!
//! assert_eq!(SubmessageId::from_raw(0x15), SubmessageId::Data);
//! assert_eq!(SubmessageId::Data.raw(), 0x15);
//!
//! let vendor = SubmessageId::from_raw(0x81);
//! assert!(vendor.is_vendor_specific());
//! assert!(!vendor.is_known());
//! assert_eq!(vendor.name(), "VENDOR(0x81)");
//! ```

use core::fmt;

/// The `submessageId` octet (§9.4.5.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SubmessageId {
    /// `PAD` (`0x01`) — filler that adjusts alignment and nothing else.
    Pad,
    /// `ACKNACK` (`0x06`) — a reader's positive and negative acknowledgement.
    AckNack,
    /// `HEARTBEAT` (`0x07`) — a writer's announcement of what it holds.
    Heartbeat,
    /// `GAP` (`0x08`) — sequence numbers that will never be delivered.
    Gap,
    /// `INFO_TS` (`0x09`) — a timestamp for the submessages that follow.
    InfoTimestamp,
    /// `INFO_SRC` (`0x0c`) — restates the source participant mid-message.
    InfoSource,
    /// `INFO_REPLY_IP4` (`0x0d`) — the compact IPv4 form of `INFO_REPLY`.
    InfoReplyIp4,
    /// `INFO_DST` (`0x0e`) — the participant the following submessages are
    /// addressed to.
    InfoDestination,
    /// `INFO_REPLY` (`0x0f`) — where replies should be sent.
    InfoReply,
    /// `NACK_FRAG` (`0x12`) — fragment-level negative acknowledgement.
    NackFrag,
    /// `HEARTBEAT_FRAG` (`0x13`) — fragment-level heartbeat.
    HeartbeatFrag,
    /// `DATA` (`0x15`) — a sample, or a key, or neither.
    Data,
    /// `DATA_FRAG` (`0x16`) — one or more fragments of a sample.
    DataFrag,
    /// `SEC_BODY` (`0x30`) — the ciphertext of a protected submessage.
    SecureBody,
    /// `SEC_PREFIX` (`0x31`) — the crypto header that introduces a protected
    /// submessage.
    SecurePrefix,
    /// `SEC_POSTFIX` (`0x32`) — the crypto footer that closes one.
    SecurePostfix,
    /// `SRTPS_PREFIX` (`0x33`) — the crypto header of a whole protected
    /// message.
    SecureRtpsPrefix,
    /// `SRTPS_POSTFIX` (`0x34`) — the crypto footer of one.
    SecureRtpsPostfix,
    /// An id outside the eighteen this crate assigns.
    ///
    /// Kept rather than rejected so a message that carries one still parses,
    /// and so `decode(encode(message)) == message` holds for it.
    Unknown(u8),
}

impl SubmessageId {
    /// `PAD`.
    pub const PAD: u8 = 0x01;
    /// `ACKNACK`.
    pub const ACKNACK: u8 = 0x06;
    /// `HEARTBEAT`.
    pub const HEARTBEAT: u8 = 0x07;
    /// `GAP`.
    pub const GAP: u8 = 0x08;
    /// `INFO_TS`.
    pub const INFO_TS: u8 = 0x09;
    /// `INFO_SRC`.
    pub const INFO_SRC: u8 = 0x0c;
    /// `INFO_REPLY_IP4`.
    pub const INFO_REPLY_IP4: u8 = 0x0d;
    /// `INFO_DST`.
    pub const INFO_DST: u8 = 0x0e;
    /// `INFO_REPLY`.
    pub const INFO_REPLY: u8 = 0x0f;
    /// `NACK_FRAG`.
    pub const NACK_FRAG: u8 = 0x12;
    /// `HEARTBEAT_FRAG`.
    pub const HEARTBEAT_FRAG: u8 = 0x13;
    /// `DATA`.
    pub const DATA: u8 = 0x15;
    /// `DATA_FRAG`.
    pub const DATA_FRAG: u8 = 0x16;
    /// `SEC_BODY` (DDS-Security 1.1 §7.3.6.2).
    pub const SEC_BODY: u8 = 0x30;
    /// `SEC_PREFIX` (DDS-Security 1.1 §7.3.6.3).
    pub const SEC_PREFIX: u8 = 0x31;
    /// `SEC_POSTFIX` (DDS-Security 1.1 §7.3.6.4).
    pub const SEC_POSTFIX: u8 = 0x32;
    /// `SRTPS_PREFIX` (DDS-Security 1.1 §7.3.6.5).
    pub const SRTPS_PREFIX: u8 = 0x33;
    /// `SRTPS_POSTFIX` (DDS-Security 1.1 §7.3.6.6).
    pub const SRTPS_POSTFIX: u8 = 0x34;

    /// The first id reserved for vendor extensions (§9.4.5.1.1).
    pub const VENDOR_RANGE_START: u8 = 0x80;

    /// Every id this crate models, in wire order.
    ///
    /// Eighteen, not the thirteen of §9.4.5.1.1: the last five are the
    /// DDS-Security 1.1 §7.3.6 ids, assigned out of the same protocol range
    /// by the companion specification. They are *named* here so a log line
    /// can say `SEC_PREFIX` rather than `UNKNOWN(0x31)`; their bodies are
    /// still not decoded by the message model — see
    /// [`crate::security`] for the half that does.
    pub const ALL: [Self; 18] = [
        Self::Pad,
        Self::AckNack,
        Self::Heartbeat,
        Self::Gap,
        Self::InfoTimestamp,
        Self::InfoSource,
        Self::InfoReplyIp4,
        Self::InfoDestination,
        Self::InfoReply,
        Self::NackFrag,
        Self::HeartbeatFrag,
        Self::Data,
        Self::DataFrag,
        Self::SecureBody,
        Self::SecurePrefix,
        Self::SecurePostfix,
        Self::SecureRtpsPrefix,
        Self::SecureRtpsPostfix,
    ];

    /// Classify a raw id octet.
    #[must_use]
    pub const fn from_raw(id: u8) -> Self {
        match id {
            Self::PAD => Self::Pad,
            Self::ACKNACK => Self::AckNack,
            Self::HEARTBEAT => Self::Heartbeat,
            Self::GAP => Self::Gap,
            Self::INFO_TS => Self::InfoTimestamp,
            Self::INFO_SRC => Self::InfoSource,
            Self::INFO_REPLY_IP4 => Self::InfoReplyIp4,
            Self::INFO_DST => Self::InfoDestination,
            Self::INFO_REPLY => Self::InfoReply,
            Self::NACK_FRAG => Self::NackFrag,
            Self::HEARTBEAT_FRAG => Self::HeartbeatFrag,
            Self::DATA => Self::Data,
            Self::DATA_FRAG => Self::DataFrag,
            Self::SEC_BODY => Self::SecureBody,
            Self::SEC_PREFIX => Self::SecurePrefix,
            Self::SEC_POSTFIX => Self::SecurePostfix,
            Self::SRTPS_PREFIX => Self::SecureRtpsPrefix,
            Self::SRTPS_POSTFIX => Self::SecureRtpsPostfix,
            other => Self::Unknown(other),
        }
    }

    /// The raw id octet.
    #[must_use]
    pub const fn raw(self) -> u8 {
        match self {
            Self::Pad => Self::PAD,
            Self::AckNack => Self::ACKNACK,
            Self::Heartbeat => Self::HEARTBEAT,
            Self::Gap => Self::GAP,
            Self::InfoTimestamp => Self::INFO_TS,
            Self::InfoSource => Self::INFO_SRC,
            Self::InfoReplyIp4 => Self::INFO_REPLY_IP4,
            Self::InfoDestination => Self::INFO_DST,
            Self::InfoReply => Self::INFO_REPLY,
            Self::NackFrag => Self::NACK_FRAG,
            Self::HeartbeatFrag => Self::HEARTBEAT_FRAG,
            Self::Data => Self::DATA,
            Self::DataFrag => Self::DATA_FRAG,
            Self::SecureBody => Self::SEC_BODY,
            Self::SecurePrefix => Self::SEC_PREFIX,
            Self::SecurePostfix => Self::SEC_POSTFIX,
            Self::SecureRtpsPrefix => Self::SRTPS_PREFIX,
            Self::SecureRtpsPostfix => Self::SRTPS_POSTFIX,
            Self::Unknown(raw) => raw,
        }
    }

    /// True for the eighteen ids [`ALL`](Self::ALL) names.
    #[must_use]
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// True for the five DDS-Security 1.1 §7.3.6 ids.
    ///
    /// A participant with no security configuration ignores these exactly as
    /// it ignores an unassigned id — the octets are kept in an
    /// [`Opaque`](crate::messages::Opaque) and never interpreted — so a
    /// secured peer talking to an unsecured one loses the traffic rather than
    /// mis-reads it.
    #[must_use]
    pub const fn is_security(self) -> bool {
        matches!(
            self,
            Self::SecureBody
                | Self::SecurePrefix
                | Self::SecurePostfix
                | Self::SecureRtpsPrefix
                | Self::SecureRtpsPostfix
        )
    }

    /// True for an id in the `0x80`–`0xff` vendor range.
    #[must_use]
    pub const fn is_vendor_specific(self) -> bool {
        self.raw() >= Self::VENDOR_RANGE_START
    }

    /// True for the two ids whose body may legitimately be empty, and for
    /// which `octetsToNextHeader == 0` therefore means "zero octets" rather
    /// than "to the end of the message" (§8.3.3.2.3).
    #[must_use]
    pub const fn allows_empty_body(self) -> bool {
        matches!(self, Self::Pad | Self::InfoTimestamp)
    }

    /// True for the submessages that carry entity ids and therefore address a
    /// specific reader and writer.
    ///
    /// The `INFO_*` submessages and `PAD` do not; they change the state of
    /// the *receiver* for everything that follows them in the message.
    #[must_use]
    pub const fn is_entity_submessage(self) -> bool {
        matches!(
            self,
            Self::Data
                | Self::DataFrag
                | Self::Heartbeat
                | Self::HeartbeatFrag
                | Self::AckNack
                | Self::NackFrag
                | Self::Gap
        )
    }

    /// True for a submessage that changes the receiver's interpretation of
    /// the ones after it (§8.3.7): the `INFO_*` family.
    #[must_use]
    pub const fn is_interpreter_submessage(self) -> bool {
        matches!(
            self,
            Self::InfoTimestamp
                | Self::InfoSource
                | Self::InfoReplyIp4
                | Self::InfoDestination
                | Self::InfoReply
        )
    }

    /// The specification's name, or `VENDOR(0x..)` / `UNKNOWN(0x..)`.
    ///
    /// Returns an owned `String` for the unknown cases, which is why it is
    /// not a `&'static str`; log lines are the only caller.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Pad => "PAD".to_owned(),
            Self::AckNack => "ACKNACK".to_owned(),
            Self::Heartbeat => "HEARTBEAT".to_owned(),
            Self::Gap => "GAP".to_owned(),
            Self::InfoTimestamp => "INFO_TS".to_owned(),
            Self::InfoSource => "INFO_SRC".to_owned(),
            Self::InfoReplyIp4 => "INFO_REPLY_IP4".to_owned(),
            Self::InfoDestination => "INFO_DST".to_owned(),
            Self::InfoReply => "INFO_REPLY".to_owned(),
            Self::NackFrag => "NACK_FRAG".to_owned(),
            Self::HeartbeatFrag => "HEARTBEAT_FRAG".to_owned(),
            Self::Data => "DATA".to_owned(),
            Self::DataFrag => "DATA_FRAG".to_owned(),
            Self::SecureBody => "SEC_BODY".to_owned(),
            Self::SecurePrefix => "SEC_PREFIX".to_owned(),
            Self::SecurePostfix => "SEC_POSTFIX".to_owned(),
            Self::SecureRtpsPrefix => "SRTPS_PREFIX".to_owned(),
            Self::SecureRtpsPostfix => "SRTPS_POSTFIX".to_owned(),
            Self::Unknown(raw) if raw >= Self::VENDOR_RANGE_START => {
                format!("VENDOR(0x{raw:02x})")
            }
            Self::Unknown(raw) => format!("UNKNOWN(0x{raw:02x})"),
        }
    }
}

impl fmt::Display for SubmessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

impl From<u8> for SubmessageId {
    fn from(id: u8) -> Self {
        Self::from_raw(id)
    }
}

impl From<SubmessageId> for u8 {
    fn from(id: SubmessageId) -> Self {
        id.raw()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn every_assigned_id_matches_the_table() {
        // §9.4.5.1.1, Table 9.4, then DDS-Security 1.1 §7.3.6.
        let table = [
            (0x01_u8, SubmessageId::Pad, "PAD", false),
            (0x06, SubmessageId::AckNack, "ACKNACK", false),
            (0x07, SubmessageId::Heartbeat, "HEARTBEAT", false),
            (0x08, SubmessageId::Gap, "GAP", false),
            (0x09, SubmessageId::InfoTimestamp, "INFO_TS", false),
            (0x0c, SubmessageId::InfoSource, "INFO_SRC", false),
            (0x0d, SubmessageId::InfoReplyIp4, "INFO_REPLY_IP4", false),
            (0x0e, SubmessageId::InfoDestination, "INFO_DST", false),
            (0x0f, SubmessageId::InfoReply, "INFO_REPLY", false),
            (0x12, SubmessageId::NackFrag, "NACK_FRAG", false),
            (0x13, SubmessageId::HeartbeatFrag, "HEARTBEAT_FRAG", false),
            (0x15, SubmessageId::Data, "DATA", false),
            (0x16, SubmessageId::DataFrag, "DATA_FRAG", false),
            (0x30, SubmessageId::SecureBody, "SEC_BODY", true),
            (0x31, SubmessageId::SecurePrefix, "SEC_PREFIX", true),
            (0x32, SubmessageId::SecurePostfix, "SEC_POSTFIX", true),
            (0x33, SubmessageId::SecureRtpsPrefix, "SRTPS_PREFIX", true),
            (0x34, SubmessageId::SecureRtpsPostfix, "SRTPS_POSTFIX", true),
        ];
        assert_eq!(table.len(), SubmessageId::ALL.len());
        for (raw, id, name, security) in table {
            assert_eq!(SubmessageId::from_raw(raw), id);
            assert_eq!(id.raw(), raw);
            assert_eq!(id.name(), name);
            assert_eq!(id.to_string(), name);
            assert!(id.is_known());
            assert!(!id.is_vendor_specific());
            assert_eq!(id.is_security(), security, "{name}");
            assert!(SubmessageId::ALL.contains(&id));
        }
    }

    #[test]
    fn the_security_ids_are_five_and_sit_in_the_protocol_range() {
        let security: Vec<SubmessageId> = SubmessageId::ALL
            .iter()
            .copied()
            .filter(|id| id.is_security())
            .collect();
        assert_eq!(security.len(), 5);
        for id in security {
            assert!(
                (0x30..=0x34).contains(&id.raw()),
                "{id} is outside the DDS-Security block"
            );
            assert!(
                !id.is_vendor_specific(),
                "{id} is a protocol id, not a vendor one"
            );
        }
        assert!(!SubmessageId::from_raw(0x35).is_security());
        assert!(!SubmessageId::Data.is_security());
    }

    #[test]
    fn every_unassigned_id_round_trips_as_unknown() {
        for raw in 0_u8..=255 {
            let id = SubmessageId::from_raw(raw);
            assert_eq!(id.raw(), raw, "0x{raw:02x} did not round-trip");
            assert_eq!(u8::from(SubmessageId::from(raw)), raw);
            if !SubmessageId::ALL.iter().any(|known| known.raw() == raw) {
                assert!(matches!(id, SubmessageId::Unknown(_)));
                assert!(!id.is_known());
            }
        }
    }

    #[test]
    fn the_vendor_range_starts_at_eighty() {
        assert!(!SubmessageId::from_raw(0x7f).is_vendor_specific());
        assert!(SubmessageId::from_raw(0x80).is_vendor_specific());
        assert!(SubmessageId::from_raw(0xff).is_vendor_specific());
        assert_eq!(SubmessageId::from_raw(0x80).name(), "VENDOR(0x80)");
        assert_eq!(SubmessageId::from_raw(0x7f).name(), "UNKNOWN(0x7f)");
    }

    #[test]
    fn only_pad_and_info_ts_may_have_an_empty_body() {
        for id in SubmessageId::ALL {
            let expected = matches!(id, SubmessageId::Pad | SubmessageId::InfoTimestamp);
            assert_eq!(id.allows_empty_body(), expected, "{id}");
        }
        assert!(!SubmessageId::from_raw(0x80).allows_empty_body());
    }

    #[test]
    fn the_entity_and_interpreter_families_partition_the_known_ids() {
        // Three buckets, not two: `PAD` is in neither family, and neither are
        // the five DDS-Security ids — a `SEC_PREFIX` addresses no endpoint
        // and reinterprets nothing, it wraps.
        for id in SubmessageId::ALL {
            let entity = id.is_entity_submessage();
            let interpreter = id.is_interpreter_submessage();
            assert!(!(entity && interpreter), "{id} cannot be both");
            if id != SubmessageId::Pad && !id.is_security() {
                assert!(entity || interpreter, "{id} belongs to neither family");
            } else {
                assert!(!entity, "{id} addresses no endpoint");
                assert!(!interpreter, "{id} reinterprets nothing");
            }
        }
        assert!(!SubmessageId::Pad.is_entity_submessage());
        assert!(!SubmessageId::Pad.is_interpreter_submessage());
        assert_eq!(
            SubmessageId::ALL
                .iter()
                .filter(|id| id.is_entity_submessage())
                .count(),
            7
        );
        assert_eq!(
            SubmessageId::ALL
                .iter()
                .filter(|id| id.is_interpreter_submessage())
                .count(),
            5
        );
    }
}
