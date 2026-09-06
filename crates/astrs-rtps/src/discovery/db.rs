//! The discovery database: who is out there, and what they publish.
//!
//! One structure per participant, holding every remote participant SPDP has
//! announced and every remote endpoint SEDP has. It is the only place that
//! decides *whether* two endpoints should be wired together; the participant
//! decides *when*, and the writer and reader do the wiring.
//!
//! # Everything is an event
//!
//! Every mutating method returns [`DiscoveryEvent`]s rather than mutating
//! silently. A participant turns those into `match_reader` / `unmatch_writer`
//! calls and into notifications an application awaits, and a test can drive
//! the database directly and assert the events without a socket in sight.
//!
//! # Leases
//!
//! A remote participant is kept for its announced `PID_PARTICIPANT_LEASE_
//! DURATION` past its last announcement (§8.5.3.3). When the lease runs out
//! the participant goes, and *every endpoint it owned goes with it* — that
//! cascade is the whole reason endpoints are keyed by GUID rather than held
//! inside the participant record, because an SEDP sample can arrive before
//! the SPDP announcement that explains who sent it.
//!
//! # Ordering
//!
//! SPDP and SEDP are separate reliable streams and there is no ordering
//! between them. A `DiscoveredWriterData` for a participant this database has
//! never heard of is therefore *stored*, not rejected: the SPDP announcement
//! will arrive, and when it does the endpoint is already there to be matched.
//! [`DiscoveryDb::orphan_count`] reports how many are waiting.

use core::fmt;
use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use crate::behavior::endpoint::TopicKey;
use crate::discovery::endpoint_data::{DiscoveredReaderData, DiscoveredWriterData};
use crate::discovery::matching::{MatchOutcome, ReaderQos, WriterQos, match_endpoints};
use crate::discovery::participant_data::ParticipantData;
use crate::structure::{Guid, Locator};

/// Most remote participants one database will hold.
///
/// A domain with more than this many participants is a misconfiguration or an
/// attack; either way the memory has to stop somewhere.
pub const MAX_PARTICIPANTS: usize = 4_096;

/// Most remote endpoints of one kind one database will hold.
pub const MAX_ENDPOINTS: usize = 65_536;

/// What changed in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DiscoveryEvent {
    /// A participant was heard from for the first time.
    ParticipantDiscovered(Guid),
    /// A participant re-announced with different contents.
    ParticipantUpdated(Guid),
    /// A participant's lease ran out, or it announced its departure.
    ParticipantLost(Guid),
    /// A remote writer was announced for the first time.
    WriterDiscovered(Guid),
    /// A remote writer re-announced with different contents.
    WriterUpdated(Guid),
    /// A remote writer was disposed of, or its participant went.
    WriterLost(Guid),
    /// A remote reader was announced for the first time.
    ReaderDiscovered(Guid),
    /// A remote reader re-announced with different contents.
    ReaderUpdated(Guid),
    /// A remote reader was disposed of, or its participant went.
    ReaderLost(Guid),
}

impl DiscoveryEvent {
    /// The GUID the event is about.
    #[must_use]
    pub const fn guid(self) -> Guid {
        match self {
            Self::ParticipantDiscovered(guid)
            | Self::ParticipantUpdated(guid)
            | Self::ParticipantLost(guid)
            | Self::WriterDiscovered(guid)
            | Self::WriterUpdated(guid)
            | Self::WriterLost(guid)
            | Self::ReaderDiscovered(guid)
            | Self::ReaderUpdated(guid)
            | Self::ReaderLost(guid) => guid,
        }
    }

    /// True when something arrived rather than departed.
    #[must_use]
    pub const fn is_arrival(self) -> bool {
        matches!(
            self,
            Self::ParticipantDiscovered(_) | Self::WriterDiscovered(_) | Self::ReaderDiscovered(_)
        )
    }

    /// True when something departed.
    #[must_use]
    pub const fn is_departure(self) -> bool {
        matches!(
            self,
            Self::ParticipantLost(_) | Self::WriterLost(_) | Self::ReaderLost(_)
        )
    }
}

impl fmt::Display for DiscoveryEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (verb, guid) = match self {
            Self::ParticipantDiscovered(guid) => ("participant discovered", guid),
            Self::ParticipantUpdated(guid) => ("participant updated", guid),
            Self::ParticipantLost(guid) => ("participant lost", guid),
            Self::WriterDiscovered(guid) => ("writer discovered", guid),
            Self::WriterUpdated(guid) => ("writer updated", guid),
            Self::WriterLost(guid) => ("writer lost", guid),
            Self::ReaderDiscovered(guid) => ("reader discovered", guid),
            Self::ReaderUpdated(guid) => ("reader updated", guid),
            Self::ReaderLost(guid) => ("reader lost", guid),
        };
        write!(formatter, "{verb}: {guid}")
    }
}

/// One remote participant, with the timing that decides how long it stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteParticipant {
    /// What it announced.
    pub data: ParticipantData,
    /// When it was first heard from.
    pub discovered_at: Instant,
    /// When it was last heard from.
    pub last_seen: Instant,
    /// How many announcements have arrived.
    pub announcements: u64,
}

impl RemoteParticipant {
    /// The lease it asked for, as a `std::time::Duration`.
    ///
    /// An infinite lease reads as `None`; such a participant is only removed
    /// when it announces its own departure.
    #[must_use]
    pub fn lease(&self) -> Option<StdDuration> {
        if self.data.lease_duration.is_infinite() {
            None
        } else {
            self.data.lease_duration.to_std()
        }
    }

    /// True when the lease has run out at `now`.
    #[must_use]
    pub fn has_expired(&self, now: Instant) -> bool {
        match self.lease() {
            None => false,
            Some(lease) => now.saturating_duration_since(self.last_seen) > lease,
        }
    }

    /// Where to send this participant's discovery traffic.
    #[must_use]
    pub fn metatraffic_locators(&self) -> Vec<Locator> {
        self.data.metatraffic_locators()
    }

    /// Where to send this participant's user traffic when an endpoint names
    /// no locator of its own.
    #[must_use]
    pub fn default_locators(&self) -> Vec<Locator> {
        self.data.default_locators()
    }
}

/// Everything this participant knows about the rest of the domain.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryDb {
    participants: BTreeMap<Guid, RemoteParticipant>,
    writers: BTreeMap<Guid, DiscoveredWriterData>,
    readers: BTreeMap<Guid, DiscoveredReaderData>,
}

impl DiscoveryDb {
    /// An empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many remote participants are known.
    #[must_use]
    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    /// How many remote writers are known.
    #[must_use]
    pub fn writer_count(&self) -> usize {
        self.writers.len()
    }

    /// How many remote readers are known.
    #[must_use]
    pub fn reader_count(&self) -> usize {
        self.readers.len()
    }

    /// True when nothing at all is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty() && self.writers.is_empty() && self.readers.is_empty()
    }

    /// How many endpoints belong to a participant that has not announced
    /// itself yet.
    ///
    /// SEDP and SPDP are unordered; a nonzero count here is normal for a few
    /// milliseconds and a symptom if it stays that way.
    #[must_use]
    pub fn orphan_count(&self) -> usize {
        let writers = self
            .writers
            .keys()
            .filter(|guid| !self.participants.contains_key(&guid.participant_guid()))
            .count();
        let readers = self
            .readers
            .keys()
            .filter(|guid| !self.participants.contains_key(&guid.participant_guid()))
            .count();
        writers.saturating_add(readers)
    }

    /// One remote participant.
    #[must_use]
    pub fn participant(&self, guid: Guid) -> Option<&RemoteParticipant> {
        self.participants.get(&guid.participant_guid())
    }

    /// Every remote participant.
    pub fn participants(&self) -> impl Iterator<Item = &RemoteParticipant> {
        self.participants.values()
    }

    /// One remote writer.
    #[must_use]
    pub fn writer(&self, guid: Guid) -> Option<&DiscoveredWriterData> {
        self.writers.get(&guid)
    }

    /// One remote reader.
    #[must_use]
    pub fn reader(&self, guid: Guid) -> Option<&DiscoveredReaderData> {
        self.readers.get(&guid)
    }

    /// Every remote writer.
    pub fn writers(&self) -> impl Iterator<Item = &DiscoveredWriterData> {
        self.writers.values()
    }

    /// Every remote reader.
    pub fn readers(&self) -> impl Iterator<Item = &DiscoveredReaderData> {
        self.readers.values()
    }

    /// True when `participant` is known.
    #[must_use]
    pub fn knows(&self, participant: Guid) -> bool {
        self.participants
            .contains_key(&participant.participant_guid())
    }

    /// Record an SPDP announcement.
    ///
    /// Returns `Discovered` the first time, `Updated` when the contents
    /// changed, and nothing when it is a repeat — a participant announcing
    /// every three seconds must not generate an event every three seconds.
    pub fn observe_participant(
        &mut self,
        data: ParticipantData,
        now: Instant,
    ) -> Option<DiscoveryEvent> {
        let guid = data.guid.participant_guid();
        match self.participants.get_mut(&guid) {
            Some(existing) => {
                existing.last_seen = now;
                existing.announcements = existing.announcements.saturating_add(1);
                if existing.data == data {
                    None
                } else {
                    existing.data = data;
                    Some(DiscoveryEvent::ParticipantUpdated(guid))
                }
            }
            None => {
                if self.participants.len() >= MAX_PARTICIPANTS {
                    return None;
                }
                self.participants.insert(
                    guid,
                    RemoteParticipant {
                        data,
                        discovered_at: now,
                        last_seen: now,
                        announcements: 1,
                    },
                );
                Some(DiscoveryEvent::ParticipantDiscovered(guid))
            }
        }
    }

    /// Record an SEDP publication announcement.
    pub fn observe_writer(&mut self, data: DiscoveredWriterData) -> Option<DiscoveryEvent> {
        let guid = data.guid();
        match self.writers.get_mut(&guid) {
            Some(existing) if *existing == data => None,
            Some(existing) => {
                *existing = data;
                Some(DiscoveryEvent::WriterUpdated(guid))
            }
            None => {
                if self.writers.len() >= MAX_ENDPOINTS {
                    return None;
                }
                self.writers.insert(guid, data);
                Some(DiscoveryEvent::WriterDiscovered(guid))
            }
        }
    }

    /// Record an SEDP subscription announcement.
    pub fn observe_reader(&mut self, data: DiscoveredReaderData) -> Option<DiscoveryEvent> {
        let guid = data.guid();
        match self.readers.get_mut(&guid) {
            Some(existing) if *existing == data => None,
            Some(existing) => {
                *existing = data;
                Some(DiscoveryEvent::ReaderUpdated(guid))
            }
            None => {
                if self.readers.len() >= MAX_ENDPOINTS {
                    return None;
                }
                self.readers.insert(guid, data);
                Some(DiscoveryEvent::ReaderDiscovered(guid))
            }
        }
    }

    /// Forget one remote writer — it was disposed of.
    pub fn forget_writer(&mut self, guid: Guid) -> Option<DiscoveryEvent> {
        self.writers
            .remove(&guid)
            .map(|_| DiscoveryEvent::WriterLost(guid))
    }

    /// Forget one remote reader — it was disposed of.
    pub fn forget_reader(&mut self, guid: Guid) -> Option<DiscoveryEvent> {
        self.readers
            .remove(&guid)
            .map(|_| DiscoveryEvent::ReaderLost(guid))
    }

    /// Forget a participant and everything it owned.
    ///
    /// The endpoint events come first, so a caller applying them in order
    /// unmatches the endpoints before it forgets where to send to them.
    pub fn forget_participant(&mut self, participant: Guid) -> Vec<DiscoveryEvent> {
        let participant = participant.participant_guid();
        let mut events = Vec::new();

        let doomed_writers: Vec<Guid> = self
            .writers
            .keys()
            .filter(|guid| guid.prefix == participant.prefix)
            .copied()
            .collect();
        for guid in doomed_writers {
            self.writers.remove(&guid);
            events.push(DiscoveryEvent::WriterLost(guid));
        }

        let doomed_readers: Vec<Guid> = self
            .readers
            .keys()
            .filter(|guid| guid.prefix == participant.prefix)
            .copied()
            .collect();
        for guid in doomed_readers {
            self.readers.remove(&guid);
            events.push(DiscoveryEvent::ReaderLost(guid));
        }

        if self.participants.remove(&participant).is_some() {
            events.push(DiscoveryEvent::ParticipantLost(participant));
        }
        events
    }

    /// Forget every participant whose lease has run out at `now`.
    pub fn expire(&mut self, now: Instant) -> Vec<DiscoveryEvent> {
        let expired: Vec<Guid> = self
            .participants
            .iter()
            .filter(|(_, remote)| remote.has_expired(now))
            .map(|(guid, _)| *guid)
            .collect();
        let mut events = Vec::new();
        for guid in expired {
            events.extend(self.forget_participant(guid));
        }
        events
    }

    /// When the soonest lease runs out, for a caller scheduling a wake-up.
    #[must_use]
    pub fn next_lease_deadline(&self, now: Instant) -> Option<StdDuration> {
        self.participants
            .values()
            .filter_map(|remote| {
                let lease = remote.lease()?;
                Some(lease.saturating_sub(now.saturating_duration_since(remote.last_seen)))
            })
            .min()
    }

    /// Every remote writer a local reader on `topic` with `qos` should match.
    ///
    /// Unrelated endpoints are omitted entirely; incompatible ones are
    /// reported with the policy that failed, because a DDS application wants
    /// to be told "your reader is RELIABLE and that writer is not" rather
    /// than watching nothing happen.
    #[must_use]
    pub fn writers_for(
        &self,
        topic: &TopicKey,
        qos: &ReaderQos,
    ) -> Vec<(&DiscoveredWriterData, MatchOutcome)> {
        self.writers
            .values()
            .filter_map(|writer| {
                let outcome = match_endpoints(
                    &topic.topic_name,
                    &topic.type_name,
                    qos,
                    &writer.identity.topic_name,
                    &writer.identity.type_name,
                    &writer.qos,
                );
                match outcome {
                    MatchOutcome::Unrelated => None,
                    other => Some((writer, other)),
                }
            })
            .collect()
    }

    /// Every remote reader a local writer on `topic` with `qos` should match.
    #[must_use]
    pub fn readers_for(
        &self,
        topic: &TopicKey,
        qos: &WriterQos,
    ) -> Vec<(&DiscoveredReaderData, MatchOutcome)> {
        self.readers
            .values()
            .filter_map(|reader| {
                let outcome = match_endpoints(
                    &reader.identity.topic_name,
                    &reader.identity.type_name,
                    &reader.qos,
                    &topic.topic_name,
                    &topic.type_name,
                    qos,
                );
                match outcome {
                    MatchOutcome::Unrelated => None,
                    other => Some((reader, other)),
                }
            })
            .collect()
    }

    /// The locators a remote writer's traffic should be sent to.
    ///
    /// The endpoint's own when it announced any, and the owning
    /// participant's user-traffic defaults otherwise — which is the ROS 2
    /// case, since an `rmw` node has one user socket per participant.
    #[must_use]
    pub fn writer_locators(&self, guid: Guid) -> Vec<Locator> {
        let Some(writer) = self.writers.get(&guid) else {
            return Vec::new();
        };
        let defaults = self
            .participant(guid)
            .map(RemoteParticipant::default_locators)
            .unwrap_or_default();
        writer.identity.resolve_locators(&defaults)
    }

    /// The locators a remote reader's traffic should be sent to.
    #[must_use]
    pub fn reader_locators(&self, guid: Guid) -> Vec<Locator> {
        let Some(reader) = self.readers.get(&guid) else {
            return Vec::new();
        };
        let defaults = self
            .participant(guid)
            .map(RemoteParticipant::default_locators)
            .unwrap_or_default();
        reader.identity.resolve_locators(&defaults)
    }

    /// The metatraffic locators of every known participant.
    ///
    /// Where an SPDP re-announcement or an SEDP sample goes when multicast is
    /// unavailable.
    #[must_use]
    pub fn all_metatraffic_locators(&self) -> Vec<Locator> {
        let mut locators = Vec::new();
        for remote in self.participants.values() {
            for locator in remote.metatraffic_locators() {
                if !locators.contains(&locator) {
                    locators.push(locator);
                }
            }
        }
        locators
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.participants.clear();
        self.writers.clear();
        self.readers.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::behavior::error::QosPolicyId;
    use crate::structure::{
        Duration, ENTITYID_PARTICIPANT, EntityId, EntityKind, GuidPrefix, VendorId,
    };
    use std::net::Ipv4Addr;

    const TOPIC: &str = "rt/chatter";
    const TYPE: &str = "std_msgs::msg::dds_::String_";

    fn prefix(seed: u8) -> GuidPrefix {
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
    }

    fn participant(seed: u8) -> Guid {
        Guid::new(prefix(seed), ENTITYID_PARTICIPANT)
    }

    fn announcement(seed: u8, lease_secs: i32) -> ParticipantData {
        ParticipantData::new(participant(seed))
            .with_lease(Duration::from_secs(lease_secs))
            .with_metatraffic_unicast(Locator::udpv4(
                Ipv4Addr::LOCALHOST,
                45_000 + u16::from(seed),
            ))
            .with_default_unicast(Locator::udpv4(
                Ipv4Addr::LOCALHOST,
                46_000 + u16::from(seed),
            ))
    }

    fn writer_data(seed: u8, counter: u32, qos: WriterQos) -> DiscoveredWriterData {
        DiscoveredWriterData::new(
            Guid::new(
                prefix(seed),
                EntityId::user_defined(counter, EntityKind::USER_WRITER_NO_KEY),
            ),
            TOPIC,
            TYPE,
        )
        .unwrap()
        .with_qos(qos)
    }

    fn reader_data(seed: u8, counter: u32, qos: ReaderQos) -> DiscoveredReaderData {
        DiscoveredReaderData::new(
            Guid::new(
                prefix(seed),
                EntityId::user_defined(counter, EntityKind::USER_READER_NO_KEY),
            ),
            TOPIC,
            TYPE,
        )
        .unwrap()
        .with_qos(qos)
    }

    fn topic() -> TopicKey {
        TopicKey::new(TOPIC, TYPE).unwrap()
    }

    #[test]
    fn a_first_announcement_is_a_discovery() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        assert!(db.is_empty());
        assert_eq!(
            db.observe_participant(announcement(1, 10), now),
            Some(DiscoveryEvent::ParticipantDiscovered(participant(1)))
        );
        assert_eq!(db.participant_count(), 1);
        assert!(db.knows(participant(1)));
    }

    #[test]
    fn a_repeat_announcement_is_silent_but_renews_the_lease() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        db.observe_participant(announcement(1, 10), now);
        assert_eq!(
            db.observe_participant(announcement(1, 10), now + StdDuration::from_secs(5)),
            None,
            "an unchanged re-announcement is not news"
        );
        let remote = db.participant(participant(1)).expect("known");
        assert_eq!(remote.announcements, 2);
        assert_eq!(remote.last_seen, now + StdDuration::from_secs(5));
        assert!(!remote.has_expired(now + StdDuration::from_secs(14)));
    }

    #[test]
    fn a_changed_announcement_is_an_update() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        db.observe_participant(announcement(1, 10), now);
        let moved = announcement(1, 10).with_entity_name("renamed");
        assert_eq!(
            db.observe_participant(moved, now),
            Some(DiscoveryEvent::ParticipantUpdated(participant(1)))
        );
    }

    #[test]
    fn a_lease_that_runs_out_takes_the_endpoints_with_it() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        db.observe_participant(announcement(1, 1), now);
        db.observe_writer(writer_data(1, 1, WriterQos::default()));
        db.observe_reader(reader_data(1, 2, ReaderQos::default()));
        assert_eq!(db.writer_count(), 1);
        assert_eq!(db.reader_count(), 1);

        assert!(db.expire(now).is_empty(), "the lease is still running");
        let events = db.expire(now + StdDuration::from_secs(2));
        assert_eq!(events.len(), 3);
        assert!(events[0].is_departure());
        assert_eq!(
            events.last().copied(),
            Some(DiscoveryEvent::ParticipantLost(participant(1))),
            "the participant goes last, after its endpoints"
        );
        assert!(db.is_empty());
    }

    #[test]
    fn an_infinite_lease_never_expires() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        let mut data = announcement(1, 10);
        data.lease_duration = Duration::INFINITE;
        db.observe_participant(data, now);
        assert!(
            db.expire(now + StdDuration::from_secs(86_400)).is_empty(),
            "an infinite lease means \"until I say otherwise\""
        );
        assert_eq!(db.next_lease_deadline(now), None);
    }

    #[test]
    fn the_next_deadline_is_the_soonest_lease() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        db.observe_participant(announcement(1, 30), now);
        db.observe_participant(announcement(2, 5), now);
        assert_eq!(db.next_lease_deadline(now), Some(StdDuration::from_secs(5)));
    }

    #[test]
    fn an_endpoint_may_arrive_before_its_participant() {
        let mut db = DiscoveryDb::new();
        assert_eq!(
            db.observe_writer(writer_data(3, 1, WriterQos::default())),
            Some(DiscoveryEvent::WriterDiscovered(Guid::new(
                prefix(3),
                EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
            )))
        );
        assert_eq!(db.orphan_count(), 1, "SEDP outran SPDP");

        db.observe_participant(announcement(3, 10), Instant::now());
        assert_eq!(db.orphan_count(), 0);
    }

    #[test]
    fn a_repeat_endpoint_announcement_is_silent() {
        let mut db = DiscoveryDb::new();
        let data = writer_data(1, 1, WriterQos::default());
        assert!(db.observe_writer(data.clone()).is_some());
        assert_eq!(db.observe_writer(data), None);
    }

    #[test]
    fn a_changed_endpoint_announcement_is_an_update() {
        let mut db = DiscoveryDb::new();
        db.observe_writer(writer_data(1, 1, WriterQos::default()));
        let event = db.observe_writer(writer_data(1, 1, WriterQos::latched(5)));
        assert!(matches!(event, Some(DiscoveryEvent::WriterUpdated(_))));
    }

    #[test]
    fn forgetting_an_endpoint_is_reported_once() {
        let mut db = DiscoveryDb::new();
        let data = reader_data(1, 1, ReaderQos::default());
        let guid = data.guid();
        db.observe_reader(data);
        assert_eq!(
            db.forget_reader(guid),
            Some(DiscoveryEvent::ReaderLost(guid))
        );
        assert_eq!(db.forget_reader(guid), None);
    }

    #[test]
    fn matching_reports_compatible_and_incompatible_but_not_unrelated() {
        let mut db = DiscoveryDb::new();
        db.observe_writer(writer_data(1, 1, WriterQos::services_default()));
        db.observe_writer(writer_data(1, 2, WriterQos::sensor_data()));
        db.observe_writer(
            DiscoveredWriterData::new(
                Guid::new(
                    prefix(1),
                    EntityId::user_defined(3, EntityKind::USER_WRITER_NO_KEY),
                ),
                "rt/elsewhere",
                TYPE,
            )
            .unwrap(),
        );

        let candidates = db.writers_for(&topic(), &ReaderQos::reliable(10));
        assert_eq!(
            candidates.len(),
            2,
            "the other topic is not reported at all"
        );
        let matched = candidates
            .iter()
            .filter(|(_, outcome)| outcome.is_matched())
            .count();
        assert_eq!(matched, 1);
        let incompatible = candidates
            .iter()
            .find_map(|(_, outcome)| outcome.incompatible_policy())
            .expect("one is incompatible");
        assert_eq!(incompatible, QosPolicyId::Reliability);
    }

    #[test]
    fn a_local_writer_finds_the_readers_that_want_it() {
        let mut db = DiscoveryDb::new();
        db.observe_reader(reader_data(2, 1, ReaderQos::reliable(1)));
        db.observe_reader(reader_data(2, 2, ReaderQos::latched(1)));

        let candidates = db.readers_for(&topic(), &WriterQos::services_default());
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates
                .iter()
                .filter(|(_, outcome)| outcome.is_matched())
                .count(),
            1,
            "the TRANSIENT_LOCAL reader cannot be served by a VOLATILE writer"
        );
    }

    #[test]
    fn endpoint_locators_fall_back_to_the_participants() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        db.observe_participant(announcement(1, 10), now);
        let data = writer_data(1, 1, WriterQos::default());
        let guid = data.guid();
        db.observe_writer(data);
        assert_eq!(
            db.writer_locators(guid),
            vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 46_001)],
            "the endpoint announced none, so the participant's defaults apply"
        );
        assert!(db.writer_locators(participant(9)).is_empty());
    }

    #[test]
    fn metatraffic_locators_are_deduplicated() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        db.observe_participant(announcement(1, 10), now);
        db.observe_participant(announcement(2, 10), now);
        db.observe_participant(announcement(1, 10), now);
        assert_eq!(db.all_metatraffic_locators().len(), 2);
    }

    #[test]
    fn forgetting_a_participant_cascades_to_its_endpoints_only() {
        let mut db = DiscoveryDb::new();
        let now = Instant::now();
        db.observe_participant(announcement(1, 10), now);
        db.observe_participant(announcement(2, 10), now);
        db.observe_writer(writer_data(1, 1, WriterQos::default()));
        db.observe_writer(writer_data(2, 1, WriterQos::default()));

        let events = db.forget_participant(participant(1));
        assert_eq!(events.len(), 2);
        assert_eq!(db.participant_count(), 1);
        assert_eq!(db.writer_count(), 1, "the other participant is untouched");
    }

    #[test]
    fn events_render_and_classify() {
        let discovered = DiscoveryEvent::ParticipantDiscovered(participant(1));
        assert!(discovered.is_arrival());
        assert!(!discovered.is_departure());
        assert_eq!(discovered.guid(), participant(1));
        assert!(discovered.to_string().starts_with("participant discovered"));

        let lost = DiscoveryEvent::WriterLost(participant(1));
        assert!(lost.is_departure());
        assert!(!lost.is_arrival());

        let updated = DiscoveryEvent::ReaderUpdated(participant(1));
        assert!(!updated.is_arrival() && !updated.is_departure());
    }

    #[test]
    fn clearing_empties_the_database() {
        let mut db = DiscoveryDb::new();
        db.observe_participant(announcement(1, 10), Instant::now());
        db.observe_writer(writer_data(1, 1, WriterQos::default()));
        db.clear();
        assert!(db.is_empty());
        assert_eq!(db.participants().count(), 0);
        assert_eq!(db.writers().count(), 0);
        assert_eq!(db.readers().count(), 0);
    }
}
