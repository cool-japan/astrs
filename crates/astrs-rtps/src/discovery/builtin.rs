//! The builtin endpoint set: which discovery endpoints a participant has.
//!
//! Every participant announces, in `PID_BUILTIN_ENDPOINT_SET`, a bitmask of
//! the discovery endpoints it runs (OMG DDSI-RTPS 2.3 §8.5.3.2). A peer reads
//! it before sending anything: there is no point pushing an SEDP publication
//! sample at a participant that has no publications *detector*, and no point
//! waiting for one from a participant that has no *announcer*.
//!
//! The bits are paired — announcer and detector, writer and reader — and this
//! module keeps that pairing in the type system: [`BuiltinEndpointSet`]
//! answers [`has`](BuiltinEndpointSet::has) for a single bit, and
//! [`builtin_pairs`] turns a local set and a remote set into exactly the list
//! of writer/reader GUID pairs the two participants should wire up.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::discovery::{BuiltinEndpointSet, builtin_pairs};
//! use astrs_rtps::structure::{GuidPrefix, VendorId};
//!
//! let local = GuidPrefix::vendor_scoped(VendorId::ASTRS, [1; 10]);
//! let remote = GuidPrefix::vendor_scoped(VendorId::ASTRS, [2; 10]);
//!
//! // Two full participants wire up SPDP, both SEDP topics and WLP: four
//! // writer→reader pairs in each direction, eight in all.
//! let pairs = builtin_pairs(
//!     local,
//!     BuiltinEndpointSet::ASTRS,
//!     remote,
//!     BuiltinEndpointSet::ASTRS,
//! );
//! assert_eq!(pairs.len(), 8);
//!
//! // A peer that only detects participants gets far less.
//! let listener = BuiltinEndpointSet::new(BuiltinEndpointSet::PARTICIPANT_DETECTOR);
//! let pairs = builtin_pairs(local, BuiltinEndpointSet::ASTRS, remote, listener);
//! assert_eq!(pairs.len(), 1);
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::structure::{
    ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER,
    ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
    ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER, ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
    ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER, EntityId, Guid, GuidPrefix,
};

/// Octets a `PID_BUILTIN_ENDPOINT_SET` value occupies.
pub const BUILTIN_ENDPOINT_SET_LEN: usize = 4;

/// The set of builtin discovery endpoints a participant runs.
///
/// A `u32` of flags, §8.5.3.2. The named constants are the individual bits;
/// [`ASTRS`](BuiltinEndpointSet::ASTRS) is the set this implementation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BuiltinEndpointSet(u32);

impl BuiltinEndpointSet {
    /// `DISC_BUILTIN_ENDPOINT_PARTICIPANT_ANNOUNCER` — the SPDP writer.
    pub const PARTICIPANT_ANNOUNCER: u32 = 0x0000_0001;
    /// `DISC_BUILTIN_ENDPOINT_PARTICIPANT_DETECTOR` — the SPDP reader.
    pub const PARTICIPANT_DETECTOR: u32 = 0x0000_0002;
    /// `DISC_BUILTIN_ENDPOINT_PUBLICATIONS_ANNOUNCER` — the SEDP publications
    /// writer.
    pub const PUBLICATIONS_ANNOUNCER: u32 = 0x0000_0004;
    /// `DISC_BUILTIN_ENDPOINT_PUBLICATIONS_DETECTOR` — the SEDP publications
    /// reader.
    pub const PUBLICATIONS_DETECTOR: u32 = 0x0000_0008;
    /// `DISC_BUILTIN_ENDPOINT_SUBSCRIPTIONS_ANNOUNCER` — the SEDP
    /// subscriptions writer.
    pub const SUBSCRIPTIONS_ANNOUNCER: u32 = 0x0000_0010;
    /// `DISC_BUILTIN_ENDPOINT_SUBSCRIPTIONS_DETECTOR` — the SEDP
    /// subscriptions reader.
    pub const SUBSCRIPTIONS_DETECTOR: u32 = 0x0000_0020;
    /// `BUILTIN_ENDPOINT_PARTICIPANT_MESSAGE_DATA_WRITER` — the WLP writer.
    pub const PARTICIPANT_MESSAGE_WRITER: u32 = 0x0000_0400;
    /// `BUILTIN_ENDPOINT_PARTICIPANT_MESSAGE_DATA_READER` — the WLP reader.
    pub const PARTICIPANT_MESSAGE_READER: u32 = 0x0000_0800;
    /// `DISC_BUILTIN_ENDPOINT_TOPICS_ANNOUNCER` — the SEDP topic writer,
    /// which this crate does not run.
    pub const TOPICS_ANNOUNCER: u32 = 0x0000_1000;
    /// `DISC_BUILTIN_ENDPOINT_TOPICS_DETECTOR` — the SEDP topic reader, which
    /// this crate does not run.
    pub const TOPICS_DETECTOR: u32 = 0x0000_2000;

    /// No builtin endpoints at all.
    pub const NONE: Self = Self(0);

    /// The set an AstRS participant runs: SPDP both ways, both SEDP topics
    /// both ways, and WLP both ways.
    ///
    /// The SEDP *topic* endpoints are deliberately absent — no
    /// interoperating stack requires them, and announcing an endpoint that is
    /// not there is worse than announcing none.
    pub const ASTRS: Self = Self(
        Self::PARTICIPANT_ANNOUNCER
            | Self::PARTICIPANT_DETECTOR
            | Self::PUBLICATIONS_ANNOUNCER
            | Self::PUBLICATIONS_DETECTOR
            | Self::SUBSCRIPTIONS_ANNOUNCER
            | Self::SUBSCRIPTIONS_DETECTOR
            | Self::PARTICIPANT_MESSAGE_WRITER
            | Self::PARTICIPANT_MESSAGE_READER,
    );

    /// The six bits every ROS 2 `rmw` implementation sets.
    pub const ROS2_MINIMUM: Self = Self(
        Self::PARTICIPANT_ANNOUNCER
            | Self::PARTICIPANT_DETECTOR
            | Self::PUBLICATIONS_ANNOUNCER
            | Self::PUBLICATIONS_DETECTOR
            | Self::SUBSCRIPTIONS_ANNOUNCER
            | Self::SUBSCRIPTIONS_DETECTOR,
    );

    /// Wrap a raw bitmask, unknown bits and all.
    #[must_use]
    pub const fn new(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw bitmask, as it goes on the wire.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// True when every bit in `mask` is set.
    #[must_use]
    pub const fn has(self, mask: u32) -> bool {
        self.0 & mask == mask
    }

    /// The set with `mask` added.
    #[must_use]
    pub const fn with(self, mask: u32) -> Self {
        Self(self.0 | mask)
    }

    /// The set with `mask` removed.
    #[must_use]
    pub const fn without(self, mask: u32) -> Self {
        Self(self.0 & !mask)
    }

    /// The bits both sets have.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// True when no bit is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many bits are set.
    #[must_use]
    pub const fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// True when the participant will announce itself over SPDP.
    #[must_use]
    pub const fn announces_participants(self) -> bool {
        self.has(Self::PARTICIPANT_ANNOUNCER)
    }

    /// True when the participant will listen for SPDP announcements.
    #[must_use]
    pub const fn detects_participants(self) -> bool {
        self.has(Self::PARTICIPANT_DETECTOR)
    }

    /// True when the participant runs both halves of SEDP.
    #[must_use]
    pub const fn runs_sedp(self) -> bool {
        self.has(Self::PUBLICATIONS_ANNOUNCER | Self::SUBSCRIPTIONS_DETECTOR)
            || self.has(Self::SUBSCRIPTIONS_ANNOUNCER | Self::PUBLICATIONS_DETECTOR)
    }

    /// True when the participant runs the WLP topic.
    #[must_use]
    pub const fn runs_liveliness(self) -> bool {
        self.has(Self::PARTICIPANT_MESSAGE_WRITER | Self::PARTICIPANT_MESSAGE_READER)
    }

    /// The bits this build knows how to name, for a readable log line.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        const TABLE: [(u32, &str); 10] = [
            (
                BuiltinEndpointSet::PARTICIPANT_ANNOUNCER,
                "PARTICIPANT_ANNOUNCER",
            ),
            (
                BuiltinEndpointSet::PARTICIPANT_DETECTOR,
                "PARTICIPANT_DETECTOR",
            ),
            (
                BuiltinEndpointSet::PUBLICATIONS_ANNOUNCER,
                "PUBLICATIONS_ANNOUNCER",
            ),
            (
                BuiltinEndpointSet::PUBLICATIONS_DETECTOR,
                "PUBLICATIONS_DETECTOR",
            ),
            (
                BuiltinEndpointSet::SUBSCRIPTIONS_ANNOUNCER,
                "SUBSCRIPTIONS_ANNOUNCER",
            ),
            (
                BuiltinEndpointSet::SUBSCRIPTIONS_DETECTOR,
                "SUBSCRIPTIONS_DETECTOR",
            ),
            (
                BuiltinEndpointSet::PARTICIPANT_MESSAGE_WRITER,
                "PARTICIPANT_MESSAGE_WRITER",
            ),
            (
                BuiltinEndpointSet::PARTICIPANT_MESSAGE_READER,
                "PARTICIPANT_MESSAGE_READER",
            ),
            (BuiltinEndpointSet::TOPICS_ANNOUNCER, "TOPICS_ANNOUNCER"),
            (BuiltinEndpointSet::TOPICS_DETECTOR, "TOPICS_DETECTOR"),
        ];
        TABLE
            .iter()
            .filter(|(bit, _)| self.has(*bit))
            .map(|(_, name)| *name)
            .collect()
    }
}

impl fmt::Display for BuiltinEndpointSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self.names();
        if names.is_empty() {
            return write!(formatter, "0x{:08x} (none known)", self.0);
        }
        write!(formatter, "0x{:08x} [{}]", self.0, names.join("|"))
    }
}

impl CdrType for BuiltinEndpointSet {
    const MIN_SERIALIZED_SIZE: usize = BUILTIN_ENDPOINT_SET_LEN;
    const IS_PRIMITIVE: bool = true;
}

impl CdrSerialize for BuiltinEndpointSet {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_u32(self.0)
    }
}

impl<'de> CdrDeserialize<'de> for BuiltinEndpointSet {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self(reader.read_u32()?))
    }
}

/// `PID_BUILTIN_ENDPOINT_QOS` (`0x0077`): per-bit QoS overrides for the
/// builtin endpoints.
///
/// One bit is defined: the participant message reader may be best-effort
/// rather than reliable. Announcing it lets a peer skip the reliability
/// machinery on a topic where it buys nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BuiltinEndpointQos(u32);

impl BuiltinEndpointQos {
    /// `BEST_EFFORT_PARTICIPANT_MESSAGE_DATA_READER`.
    pub const BEST_EFFORT_PARTICIPANT_MESSAGE_READER: u32 = 0x0000_0001;

    /// No overrides — every builtin endpoint uses its default QoS.
    pub const NONE: Self = Self(0);

    /// Wrap a raw bitmask.
    #[must_use]
    pub const fn new(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw bitmask.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// True when the peer's WLP reader is best-effort.
    #[must_use]
    pub const fn best_effort_participant_message_reader(self) -> bool {
        self.0 & Self::BEST_EFFORT_PARTICIPANT_MESSAGE_READER != 0
    }
}

impl CdrType for BuiltinEndpointQos {
    const MIN_SERIALIZED_SIZE: usize = BUILTIN_ENDPOINT_SET_LEN;
    const IS_PRIMITIVE: bool = true;
}

impl CdrSerialize for BuiltinEndpointQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_u32(self.0)
    }
}

impl<'de> CdrDeserialize<'de> for BuiltinEndpointQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self(reader.read_u32()?))
    }
}

/// One builtin writer paired with the reader it talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuiltinPair {
    /// The local writer.
    pub writer: Guid,
    /// The remote reader it should send to.
    pub reader: Guid,
    /// Whether this pairing is reliable. SPDP is not; everything else is.
    pub reliable: bool,
}

/// The complete table of builtin writer/reader pairs, with the bits each side
/// must have set for the pairing to exist.
const BUILTIN_TABLE: [(EntityId, u32, EntityId, u32, bool); 4] = [
    (
        ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
        BuiltinEndpointSet::PARTICIPANT_ANNOUNCER,
        ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
        BuiltinEndpointSet::PARTICIPANT_DETECTOR,
        false,
    ),
    (
        ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
        BuiltinEndpointSet::PUBLICATIONS_ANNOUNCER,
        ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
        BuiltinEndpointSet::PUBLICATIONS_DETECTOR,
        true,
    ),
    (
        ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER,
        BuiltinEndpointSet::SUBSCRIPTIONS_ANNOUNCER,
        ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
        BuiltinEndpointSet::SUBSCRIPTIONS_DETECTOR,
        true,
    ),
    (
        ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER,
        BuiltinEndpointSet::PARTICIPANT_MESSAGE_WRITER,
        ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER,
        BuiltinEndpointSet::PARTICIPANT_MESSAGE_READER,
        true,
    ),
];

/// The builtin writer/reader pairs two participants should wire up.
///
/// Both directions: `local`'s writers to `remote`'s readers, and `remote`'s
/// writers to `local`'s readers. A pair appears only when the announcing side
/// has the writer bit *and* the detecting side has the reader bit, which is
/// the whole point of exchanging the mask.
#[must_use]
pub fn builtin_pairs(
    local_prefix: GuidPrefix,
    local_set: BuiltinEndpointSet,
    remote_prefix: GuidPrefix,
    remote_set: BuiltinEndpointSet,
) -> Vec<BuiltinPair> {
    let mut pairs = Vec::with_capacity(BUILTIN_TABLE.len() * 2);
    for (writer_id, writer_bit, reader_id, reader_bit, reliable) in BUILTIN_TABLE {
        if local_set.has(writer_bit) && remote_set.has(reader_bit) {
            pairs.push(BuiltinPair {
                writer: local_prefix.with_entity(writer_id),
                reader: remote_prefix.with_entity(reader_id),
                reliable,
            });
        }
        if remote_set.has(writer_bit) && local_set.has(reader_bit) {
            pairs.push(BuiltinPair {
                writer: remote_prefix.with_entity(writer_id),
                reader: local_prefix.with_entity(reader_id),
                reliable,
            });
        }
    }
    pairs
}

/// The entity ids of the builtin writers a participant with `set` runs.
#[must_use]
pub fn builtin_writer_ids(set: BuiltinEndpointSet) -> Vec<EntityId> {
    BUILTIN_TABLE
        .iter()
        .filter(|(_, bit, _, _, _)| set.has(*bit))
        .map(|(id, _, _, _, _)| *id)
        .collect()
}

/// The entity ids of the builtin readers a participant with `set` runs.
#[must_use]
pub fn builtin_reader_ids(set: BuiltinEndpointSet) -> Vec<EntityId> {
    BUILTIN_TABLE
        .iter()
        .filter(|(_, _, _, bit, _)| set.has(*bit))
        .map(|(_, _, id, _, _)| *id)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{
        ENTITYID_SEDP_BUILTIN_TOPIC_READER, ENTITYID_SEDP_BUILTIN_TOPIC_WRITER, VendorId,
    };
    use astrs_cdr::Encoding;

    fn prefix(seed: u8) -> GuidPrefix {
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
    }

    #[test]
    fn the_astrs_set_has_eight_bits() {
        assert_eq!(BuiltinEndpointSet::ASTRS.count(), 8);
        assert!(BuiltinEndpointSet::ASTRS.announces_participants());
        assert!(BuiltinEndpointSet::ASTRS.detects_participants());
        assert!(BuiltinEndpointSet::ASTRS.runs_sedp());
        assert!(BuiltinEndpointSet::ASTRS.runs_liveliness());
    }

    #[test]
    fn the_ros2_minimum_has_six_bits_and_no_liveliness() {
        assert_eq!(BuiltinEndpointSet::ROS2_MINIMUM.count(), 6);
        assert!(BuiltinEndpointSet::ROS2_MINIMUM.runs_sedp());
        assert!(!BuiltinEndpointSet::ROS2_MINIMUM.runs_liveliness());
    }

    #[test]
    fn the_topic_endpoints_are_not_announced() {
        assert!(!BuiltinEndpointSet::ASTRS.has(BuiltinEndpointSet::TOPICS_ANNOUNCER));
        assert!(!BuiltinEndpointSet::ASTRS.has(BuiltinEndpointSet::TOPICS_DETECTOR));
        assert!(
            !builtin_writer_ids(BuiltinEndpointSet::ASTRS)
                .contains(&ENTITYID_SEDP_BUILTIN_TOPIC_WRITER)
        );
        assert!(
            !builtin_reader_ids(BuiltinEndpointSet::ASTRS)
                .contains(&ENTITYID_SEDP_BUILTIN_TOPIC_READER)
        );
    }

    #[test]
    fn bits_add_and_remove() {
        let set = BuiltinEndpointSet::NONE
            .with(BuiltinEndpointSet::PARTICIPANT_ANNOUNCER)
            .with(BuiltinEndpointSet::PARTICIPANT_DETECTOR);
        assert_eq!(set.count(), 2);
        let trimmed = set.without(BuiltinEndpointSet::PARTICIPANT_DETECTOR);
        assert!(trimmed.announces_participants());
        assert!(!trimmed.detects_participants());
        assert!(BuiltinEndpointSet::NONE.is_empty());
    }

    #[test]
    fn intersection_keeps_only_shared_bits() {
        let shared = BuiltinEndpointSet::ASTRS.intersection(BuiltinEndpointSet::ROS2_MINIMUM);
        assert_eq!(shared, BuiltinEndpointSet::ROS2_MINIMUM);
    }

    #[test]
    fn two_full_participants_pair_eight_ways() {
        let pairs = builtin_pairs(
            prefix(1),
            BuiltinEndpointSet::ASTRS,
            prefix(2),
            BuiltinEndpointSet::ASTRS,
        );
        assert_eq!(pairs.len(), 8, "four topics, two directions");
        assert_eq!(
            pairs.iter().filter(|pair| pair.reliable).count(),
            6,
            "only SPDP is best-effort"
        );
    }

    #[test]
    fn a_detector_only_peer_receives_but_does_not_send() {
        let listener = BuiltinEndpointSet::new(BuiltinEndpointSet::PARTICIPANT_DETECTOR);
        let pairs = builtin_pairs(prefix(1), BuiltinEndpointSet::ASTRS, prefix(2), listener);
        assert_eq!(pairs.len(), 1);
        let pair = pairs[0];
        assert_eq!(
            pair.writer,
            prefix(1).with_entity(ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER)
        );
        assert_eq!(
            pair.reader,
            prefix(2).with_entity(ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER)
        );
        assert!(!pair.reliable);
    }

    #[test]
    fn an_empty_peer_pairs_with_nothing() {
        let pairs = builtin_pairs(
            prefix(1),
            BuiltinEndpointSet::ASTRS,
            prefix(2),
            BuiltinEndpointSet::NONE,
        );
        assert!(pairs.is_empty());
    }

    #[test]
    fn ros2_peers_get_no_liveliness_pairing() {
        let pairs = builtin_pairs(
            prefix(1),
            BuiltinEndpointSet::ASTRS,
            prefix(2),
            BuiltinEndpointSet::ROS2_MINIMUM,
        );
        assert_eq!(pairs.len(), 6);
        assert!(
            !pairs.iter().any(
                |pair| pair.writer.entity_id == ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER
            )
        );
    }

    #[test]
    fn the_mask_round_trips_through_cdr() {
        let octets =
            astrs_cdr::to_vec_headerless(&BuiltinEndpointSet::ASTRS, Encoding::DISCOVERY.plain())
                .unwrap();
        assert_eq!(octets.len(), BUILTIN_ENDPOINT_SET_LEN);
        let decoded: BuiltinEndpointSet =
            astrs_cdr::from_bytes_headerless(&octets, Encoding::DISCOVERY.plain()).unwrap();
        assert_eq!(decoded, BuiltinEndpointSet::ASTRS);
    }

    #[test]
    fn unknown_bits_survive_a_round_trip() {
        let exotic = BuiltinEndpointSet::new(0x8000_0001);
        let octets = astrs_cdr::to_vec_headerless(&exotic, Encoding::DISCOVERY.plain()).unwrap();
        let decoded: BuiltinEndpointSet =
            astrs_cdr::from_bytes_headerless(&octets, Encoding::DISCOVERY.plain()).unwrap();
        assert_eq!(decoded.bits(), 0x8000_0001);
        assert_eq!(
            decoded.names(),
            vec!["PARTICIPANT_ANNOUNCER"],
            "the unknown high bit has no name"
        );
    }

    #[test]
    fn display_lists_the_named_bits() {
        let rendered = BuiltinEndpointSet::ROS2_MINIMUM.to_string();
        assert!(rendered.contains("PARTICIPANT_ANNOUNCER"), "{rendered}");
        assert!(rendered.contains("SUBSCRIPTIONS_DETECTOR"), "{rendered}");
        assert!(!rendered.contains("PARTICIPANT_MESSAGE"), "{rendered}");
        assert_eq!(
            BuiltinEndpointSet::NONE.to_string(),
            "0x00000000 (none known)"
        );
    }

    #[test]
    fn builtin_endpoint_qos_reads_its_one_bit() {
        let qos =
            BuiltinEndpointQos::new(BuiltinEndpointQos::BEST_EFFORT_PARTICIPANT_MESSAGE_READER);
        assert!(qos.best_effort_participant_message_reader());
        assert!(!BuiltinEndpointQos::NONE.best_effort_participant_message_reader());

        let octets = astrs_cdr::to_vec_headerless(&qos, Encoding::DISCOVERY.plain()).unwrap();
        let decoded: BuiltinEndpointQos =
            astrs_cdr::from_bytes_headerless(&octets, Encoding::DISCOVERY.plain()).unwrap();
        assert_eq!(decoded, qos);
    }
}
