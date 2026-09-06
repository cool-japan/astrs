//! WLP: the Writer Liveliness Protocol.
//!
//! A `MANUAL_BY_PARTICIPANT` or `MANUAL_BY_TOPIC` writer proves it is alive by
//! writing to the builtin topic `DCPSParticipantMessage`
//! (OMG DDSI-RTPS 2.3 §8.4.13, §9.6.2.1). One sample from a participant
//! renews the lease of every one of its writers that asserts liveliness that
//! way, which is why it is a *participant* message rather than a per-writer
//! one.
//!
//! `AUTOMATIC` writers — the DDS default — need none of this: their
//! participant's SPDP announcement is the assertion, and
//! [`LivelinessTracker`] is told about it through
//! [`assert_automatic`](LivelinessTracker::assert_automatic).
//!
//! # The payload is plain CDR
//!
//! This is the trap, and it is the opposite of every other builtin topic.
//! SPDP and SEDP samples are `PL_CDR` parameter lists;
//! [`ParticipantMessageData`] is a plain `CDR_LE` struct:
//!
//! ```text
//! struct ParticipantMessageData {
//!     GuidPrefix_t   participantGuidPrefix;   // 12 octets, no byte order
//!     octet[4]       kind;                    // 4 octets, big-endian-looking
//!     sequence<octet> data;                   // u32 length, then octets
//! };
//! ```
//!
//! The `kind` is an *octet array*, not an integer, so it reads the same in
//! either byte order: `{0,0,0,1}` is automatic and `{0,0,0,2}` is manual,
//! whatever the encapsulation says. A vendor may define its own by setting
//! the first octet.
//!
//! # Leases
//!
//! [`LivelinessTracker`] is the timer half. It holds one deadline per remote
//! participant per kind, renews it on each assertion, and reports which have
//! run out. It takes `now` rather than reading a clock, so the expiry logic
//! is testable without waiting.

use core::fmt;
use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter, Encoding};

use crate::behavior::error::BehaviorResult;
use crate::messages::SerializedPayload;
use crate::structure::{GUID_PREFIX_LEN, Guid, GuidPrefix};

/// The DDS topic name of the WLP builtin topic.
pub const WLP_TOPIC_NAME: &str = "DCPSParticipantMessage";

/// The DDS type name of the WLP builtin topic.
pub const WLP_TYPE_NAME: &str = "ParticipantMessageData";

/// Octets the fixed part of a [`ParticipantMessageData`] occupies: the
/// prefix, the kind, and the sequence length.
pub const PARTICIPANT_MESSAGE_FIXED_LEN: usize = GUID_PREFIX_LEN + 4 + 4;

/// What a participant message asserts.
///
/// An octet array on the wire, not an integer — see the [module
/// documentation](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ParticipantMessageKind {
    /// `PARTICIPANT_MESSAGE_DATA_KIND_UNKNOWN`, `{0, 0, 0, 0}`.
    #[default]
    Unknown,
    /// `…_AUTOMATIC_LIVELINESS_UPDATE`, `{0, 0, 0, 1}`.
    ///
    /// Renews every `AUTOMATIC` writer of the announcing participant.
    Automatic,
    /// `…_MANUAL_LIVELINESS_UPDATE`, `{0, 0, 0, 2}`.
    ///
    /// Renews every `MANUAL_BY_PARTICIPANT` writer of the announcing
    /// participant.
    Manual,
    /// A kind this build does not name, preserved verbatim.
    Vendor([u8; 4]),
}

impl ParticipantMessageKind {
    /// The four octets this kind is written as.
    #[must_use]
    pub const fn to_octets(self) -> [u8; 4] {
        match self {
            Self::Unknown => [0, 0, 0, 0],
            Self::Automatic => [0, 0, 0, 1],
            Self::Manual => [0, 0, 0, 2],
            Self::Vendor(octets) => octets,
        }
    }

    /// Read a kind from its four octets.
    #[must_use]
    pub const fn from_octets(octets: [u8; 4]) -> Self {
        match octets {
            [0, 0, 0, 0] => Self::Unknown,
            [0, 0, 0, 1] => Self::Automatic,
            [0, 0, 0, 2] => Self::Manual,
            other => Self::Vendor(other),
        }
    }

    /// True when the kind renews a liveliness lease.
    #[must_use]
    pub const fn asserts_liveliness(self) -> bool {
        matches!(self, Self::Automatic | Self::Manual)
    }

    /// True when the first octet is set, which §9.6.2.1 reserves for vendors.
    #[must_use]
    pub const fn is_vendor_specific(self) -> bool {
        self.to_octets()[0] != 0
    }

    /// The specification's name, for a log line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Automatic => "AUTOMATIC_LIVELINESS_UPDATE",
            Self::Manual => "MANUAL_LIVELINESS_UPDATE",
            Self::Vendor(_) => "VENDOR_SPECIFIC",
        }
    }
}

impl fmt::Display for ParticipantMessageKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vendor(octets) => write!(formatter, "VENDOR_SPECIFIC{octets:02x?}"),
            other => formatter.write_str(other.name()),
        }
    }
}

/// One sample on the WLP builtin topic.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParticipantMessageData {
    /// The participant asserting.
    pub participant_guid_prefix: GuidPrefix,
    /// What it is asserting.
    pub kind: ParticipantMessageKind,
    /// Opaque payload; empty for a plain liveliness assertion.
    pub data: Vec<u8>,
}

impl ParticipantMessageData {
    /// An `AUTOMATIC` assertion from `prefix`.
    #[must_use]
    pub fn automatic(prefix: GuidPrefix) -> Self {
        Self {
            participant_guid_prefix: prefix,
            kind: ParticipantMessageKind::Automatic,
            data: Vec::new(),
        }
    }

    /// A `MANUAL_BY_PARTICIPANT` assertion from `prefix`.
    #[must_use]
    pub fn manual(prefix: GuidPrefix) -> Self {
        Self {
            participant_guid_prefix: prefix,
            kind: ParticipantMessageKind::Manual,
            data: Vec::new(),
        }
    }

    /// Attach opaque data.
    #[must_use]
    pub fn with_data(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.data = data.into();
        self
    }

    /// The participant this message is about.
    #[must_use]
    pub const fn participant(&self) -> Guid {
        self.participant_guid_prefix
            .with_entity(crate::structure::ENTITYID_PARTICIPANT)
    }

    /// Octets this sample serializes to, encapsulation header excluded.
    #[must_use]
    pub fn body_len(&self) -> usize {
        PARTICIPANT_MESSAGE_FIXED_LEN + self.data.len()
    }

    /// Encode into the payload a `DATA` carries.
    ///
    /// Plain `CDR_LE`, not `PL_CDR_LE` — see the [module documentation](self).
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Cdr`](crate::behavior::BehaviorError::Cdr).
    pub fn to_payload(&self) -> BehaviorResult<SerializedPayload<'static>> {
        Ok(SerializedPayload::from_cdr_with(self, Encoding::ROS2)?)
    }

    /// Decode from the payload a `DATA` carried.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Cdr`](crate::behavior::BehaviorError::Cdr) when the
    /// octets are not a well-formed sample.
    pub fn from_payload(payload: &SerializedPayload<'_>) -> BehaviorResult<Self> {
        Ok(payload.decode()?)
    }
}

impl fmt::Display for ParticipantMessageData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} from {}",
            self.kind, self.participant_guid_prefix
        )
    }
}

impl CdrType for ParticipantMessageData {
    const MIN_SERIALIZED_SIZE: usize = PARTICIPANT_MESSAGE_FIXED_LEN;
}

impl CdrSerialize for ParticipantMessageData {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(self.participant_guid_prefix.as_bytes());
        writer.write_octets(&self.kind.to_octets());
        writer.write_octet_sequence(&self.data)
    }
}

impl<'de> CdrDeserialize<'de> for ParticipantMessageData {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let prefix_octets = reader.read_octets(GUID_PREFIX_LEN)?;
        let mut prefix = [0_u8; GUID_PREFIX_LEN];
        prefix.copy_from_slice(prefix_octets);
        let kind_octets = reader.read_octets(4)?;
        let mut kind = [0_u8; 4];
        kind.copy_from_slice(kind_octets);
        let data = reader.read_octet_sequence()?;
        Ok(Self {
            participant_guid_prefix: GuidPrefix::new(prefix),
            kind: ParticipantMessageKind::from_octets(kind),
            data: data.to_vec(),
        })
    }
}

/// One remote participant's liveliness, as this participant sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivelinessState {
    /// When the last assertion arrived.
    pub asserted_at: Instant,
    /// How long that assertion is good for.
    pub lease: StdDuration,
    /// How many assertions have arrived in total.
    pub count: u64,
}

impl LivelinessState {
    /// True when the lease has run out at `now`.
    #[must_use]
    pub fn has_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.asserted_at) > self.lease
    }

    /// How long is left, or zero once it has run out.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> StdDuration {
        self.lease
            .saturating_sub(now.saturating_duration_since(self.asserted_at))
    }
}

/// Which participants have gone quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivelinessLost {
    /// The participant whose lease ran out.
    pub participant: Guid,
    /// The kind of assertion that lapsed.
    pub kind: ParticipantMessageKind,
    /// How long it had been.
    pub since: StdDuration,
}

/// Tracks the liveliness leases of every remote participant.
///
/// One deadline per `(participant, kind)`: a participant may assert
/// `AUTOMATIC` from its SPDP announcements and `MANUAL` from the WLP topic,
/// and the two leases run independently because they cover different writers.
#[derive(Debug, Clone, Default)]
pub struct LivelinessTracker {
    states: BTreeMap<(Guid, ParticipantMessageKind), LivelinessState>,
}

impl LivelinessTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: BTreeMap::new(),
        }
    }

    /// How many leases are being tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// True when nothing is being tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Renew a lease from an SPDP announcement.
    ///
    /// An SPDP announcement is the `AUTOMATIC` assertion (§8.4.13.1), so the
    /// discovery path calls this on every announcement it accepts.
    pub fn assert_automatic(&mut self, participant: Guid, lease: StdDuration, now: Instant) {
        self.assert_kind(participant, ParticipantMessageKind::Automatic, lease, now);
    }

    /// Renew a lease from a WLP sample.
    ///
    /// The sample's own `kind` decides which lease is renewed. A kind that
    /// asserts nothing — `UNKNOWN`, or a vendor's own — is recorded so that
    /// introspection can see it, but never renews a lease.
    ///
    /// Returns whether a lease was renewed.
    pub fn assert_message(
        &mut self,
        message: &ParticipantMessageData,
        lease: StdDuration,
        now: Instant,
    ) -> bool {
        if !message.kind.asserts_liveliness() {
            return false;
        }
        self.assert_kind(message.participant(), message.kind, lease, now);
        true
    }

    /// Renew one specific lease.
    pub fn assert_kind(
        &mut self,
        participant: Guid,
        kind: ParticipantMessageKind,
        lease: StdDuration,
        now: Instant,
    ) {
        let entry = self
            .states
            .entry((participant, kind))
            .or_insert(LivelinessState {
                asserted_at: now,
                lease,
                count: 0,
            });
        entry.asserted_at = now;
        entry.lease = lease;
        entry.count = entry.count.saturating_add(1);
    }

    /// The state of one lease.
    #[must_use]
    pub fn state(
        &self,
        participant: Guid,
        kind: ParticipantMessageKind,
    ) -> Option<LivelinessState> {
        self.states.get(&(participant, kind)).copied()
    }

    /// True when `participant` has at least one lease still running.
    #[must_use]
    pub fn is_alive(&self, participant: Guid, now: Instant) -> bool {
        self.states
            .iter()
            .any(|((held, _), state)| *held == participant && !state.has_expired(now))
    }

    /// Remove and report every lease that has run out at `now`.
    pub fn reap(&mut self, now: Instant) -> Vec<LivelinessLost> {
        let expired: Vec<((Guid, ParticipantMessageKind), LivelinessState)> = self
            .states
            .iter()
            .filter(|(_, state)| state.has_expired(now))
            .map(|(key, state)| (*key, *state))
            .collect();
        let mut lost = Vec::with_capacity(expired.len());
        for ((participant, kind), state) in expired {
            self.states.remove(&(participant, kind));
            lost.push(LivelinessLost {
                participant,
                kind,
                since: now.saturating_duration_since(state.asserted_at),
            });
        }
        lost
    }

    /// Forget every lease of one participant — it announced its own
    /// departure.
    pub fn forget(&mut self, participant: Guid) -> usize {
        let doomed: Vec<(Guid, ParticipantMessageKind)> = self
            .states
            .keys()
            .filter(|(held, _)| *held == participant)
            .copied()
            .collect();
        for key in &doomed {
            self.states.remove(key);
        }
        doomed.len()
    }

    /// The soonest a lease will run out, for a caller scheduling a wake-up.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<StdDuration> {
        self.states.values().map(|state| state.remaining(now)).min()
    }

    /// Every participant with a running lease.
    pub fn live_participants(&self, now: Instant) -> impl Iterator<Item = Guid> + '_ {
        let mut seen: Vec<Guid> = self
            .states
            .iter()
            .filter(|(_, state)| !state.has_expired(now))
            .map(|((participant, _), _)| *participant)
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen.into_iter()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{ENTITYID_PARTICIPANT, VendorId};

    fn prefix(seed: u8) -> GuidPrefix {
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
    }

    fn participant(seed: u8) -> Guid {
        Guid::new(prefix(seed), ENTITYID_PARTICIPANT)
    }

    #[test]
    fn the_kind_octets_are_the_ones_the_specification_fixes() {
        assert_eq!(ParticipantMessageKind::Unknown.to_octets(), [0, 0, 0, 0]);
        assert_eq!(ParticipantMessageKind::Automatic.to_octets(), [0, 0, 0, 1]);
        assert_eq!(ParticipantMessageKind::Manual.to_octets(), [0, 0, 0, 2]);
        for kind in [
            ParticipantMessageKind::Unknown,
            ParticipantMessageKind::Automatic,
            ParticipantMessageKind::Manual,
            ParticipantMessageKind::Vendor([0x41, 0x53, 0, 1]),
        ] {
            assert_eq!(
                ParticipantMessageKind::from_octets(kind.to_octets()),
                kind,
                "{kind} did not round-trip"
            );
        }
    }

    #[test]
    fn only_the_two_liveliness_kinds_assert() {
        assert!(ParticipantMessageKind::Automatic.asserts_liveliness());
        assert!(ParticipantMessageKind::Manual.asserts_liveliness());
        assert!(!ParticipantMessageKind::Unknown.asserts_liveliness());
        assert!(!ParticipantMessageKind::Vendor([0x41, 0, 0, 0]).asserts_liveliness());
        assert!(ParticipantMessageKind::Vendor([0x41, 0, 0, 0]).is_vendor_specific());
        assert!(!ParticipantMessageKind::Automatic.is_vendor_specific());
    }

    #[test]
    fn the_payload_is_plain_cdr_not_a_parameter_list() {
        let payload = ParticipantMessageData::automatic(prefix(1))
            .to_payload()
            .unwrap();
        assert!(
            !payload.is_parameter_list(),
            "WLP is the one builtin topic that is not PL_CDR"
        );
        assert_eq!(
            payload.encapsulation(),
            Some(astrs_cdr::EncapsulationKind::CdrLe)
        );
        assert_eq!(&payload.as_slice()[..4], &[0x00, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn the_body_is_prefix_then_kind_then_sequence() {
        let payload = ParticipantMessageData::manual(prefix(2))
            .to_payload()
            .unwrap();
        let body = payload.body().expect("a body");
        assert_eq!(body.len(), PARTICIPANT_MESSAGE_FIXED_LEN);
        assert_eq!(&body[..GUID_PREFIX_LEN], prefix(2).as_bytes());
        assert_eq!(&body[GUID_PREFIX_LEN..GUID_PREFIX_LEN + 4], &[0, 0, 0, 2]);
        assert_eq!(
            &body[GUID_PREFIX_LEN + 4..],
            &[0, 0, 0, 0],
            "empty sequence"
        );
    }

    #[test]
    fn a_message_round_trips() {
        let original = ParticipantMessageData::manual(prefix(3)).with_data(b"why".to_vec());
        let payload = original.to_payload().unwrap();
        let decoded = ParticipantMessageData::from_payload(&payload).unwrap();
        assert_eq!(decoded, original);
        assert_eq!(decoded.participant(), participant(3));
        assert_eq!(original.body_len(), PARTICIPANT_MESSAGE_FIXED_LEN + 3);
    }

    #[test]
    fn a_vendor_kind_survives_the_round_trip() {
        let mut original = ParticipantMessageData::automatic(prefix(4));
        original.kind = ParticipantMessageKind::Vendor([0x41, 0x53, 0x00, 0x07]);
        let decoded =
            ParticipantMessageData::from_payload(&original.to_payload().unwrap()).unwrap();
        assert_eq!(decoded.kind, original.kind);
        assert!(decoded.to_string().contains("VENDOR_SPECIFIC"));
    }

    #[test]
    fn an_assertion_starts_a_lease() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        assert!(tracker.is_empty());
        tracker.assert_automatic(participant(1), StdDuration::from_millis(100), now);
        assert_eq!(tracker.len(), 1);
        assert!(tracker.is_alive(participant(1), now));
        assert!(tracker.is_alive(participant(1), now + StdDuration::from_millis(99)));
        assert!(!tracker.is_alive(participant(1), now + StdDuration::from_millis(101)));
    }

    #[test]
    fn a_renewal_pushes_the_deadline_out() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        tracker.assert_automatic(participant(1), StdDuration::from_millis(100), now);
        tracker.assert_automatic(
            participant(1),
            StdDuration::from_millis(100),
            now + StdDuration::from_millis(80),
        );
        assert!(tracker.is_alive(participant(1), now + StdDuration::from_millis(150)));
        let state = tracker
            .state(participant(1), ParticipantMessageKind::Automatic)
            .expect("a lease");
        assert_eq!(state.count, 2);
        assert_eq!(
            state.remaining(now + StdDuration::from_millis(80)),
            state.lease
        );
    }

    #[test]
    fn reaping_reports_and_removes() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        tracker.assert_automatic(participant(1), StdDuration::from_millis(50), now);
        tracker.assert_automatic(participant(2), StdDuration::from_secs(10), now);

        assert!(tracker.reap(now).is_empty());
        let lost = tracker.reap(now + StdDuration::from_millis(60));
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].participant, participant(1));
        assert_eq!(lost[0].kind, ParticipantMessageKind::Automatic);
        assert_eq!(tracker.len(), 1);
        assert!(tracker.reap(now + StdDuration::from_millis(60)).is_empty());
    }

    #[test]
    fn the_two_kinds_have_independent_leases() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        tracker.assert_automatic(participant(1), StdDuration::from_millis(50), now);
        tracker.assert_kind(
            participant(1),
            ParticipantMessageKind::Manual,
            StdDuration::from_secs(10),
            now,
        );
        assert_eq!(tracker.len(), 2);

        let lost = tracker.reap(now + StdDuration::from_millis(60));
        assert_eq!(lost.len(), 1, "only the automatic lease lapsed");
        assert!(tracker.is_alive(participant(1), now + StdDuration::from_millis(60)));
    }

    #[test]
    fn a_wlp_sample_renews_the_lease_its_kind_names() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        let message = ParticipantMessageData::manual(prefix(5));
        assert!(tracker.assert_message(&message, StdDuration::from_secs(1), now));
        assert!(
            tracker
                .state(participant(5), ParticipantMessageKind::Manual)
                .is_some()
        );
        assert!(
            tracker
                .state(participant(5), ParticipantMessageKind::Automatic)
                .is_none()
        );
    }

    #[test]
    fn a_sample_that_asserts_nothing_renews_nothing() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        let mut message = ParticipantMessageData::automatic(prefix(6));
        message.kind = ParticipantMessageKind::Unknown;
        assert!(!tracker.assert_message(&message, StdDuration::from_secs(1), now));
        assert!(tracker.is_empty());
    }

    #[test]
    fn forgetting_a_participant_drops_every_lease_it_had() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        tracker.assert_automatic(participant(1), StdDuration::from_secs(1), now);
        tracker.assert_kind(
            participant(1),
            ParticipantMessageKind::Manual,
            StdDuration::from_secs(1),
            now,
        );
        tracker.assert_automatic(participant(2), StdDuration::from_secs(1), now);
        assert_eq!(tracker.forget(participant(1)), 2);
        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.forget(participant(1)), 0);
    }

    #[test]
    fn the_next_deadline_is_the_soonest_one() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        assert_eq!(tracker.next_deadline(now), None);
        tracker.assert_automatic(participant(1), StdDuration::from_secs(10), now);
        tracker.assert_automatic(participant(2), StdDuration::from_secs(2), now);
        assert_eq!(tracker.next_deadline(now), Some(StdDuration::from_secs(2)));
    }

    #[test]
    fn live_participants_are_listed_once_each() {
        let mut tracker = LivelinessTracker::new();
        let now = Instant::now();
        tracker.assert_automatic(participant(1), StdDuration::from_secs(1), now);
        tracker.assert_kind(
            participant(1),
            ParticipantMessageKind::Manual,
            StdDuration::from_secs(1),
            now,
        );
        tracker.assert_automatic(participant(2), StdDuration::from_millis(1), now);
        let live: Vec<Guid> = tracker
            .live_participants(now + StdDuration::from_millis(10))
            .collect();
        assert_eq!(live, vec![participant(1)]);
    }

    #[test]
    fn the_topic_names_are_the_builtin_ones() {
        assert_eq!(WLP_TOPIC_NAME, "DCPSParticipantMessage");
        assert_eq!(WLP_TYPE_NAME, "ParticipantMessageData");
    }
}
