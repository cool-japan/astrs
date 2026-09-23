//! The protocol state a participant's one lock covers.
//!
//! Everything a participant knows and decides, with no socket and no clock in
//! sight: the discovery half ([`Spdp`], [`DiscoveryDb`],
//! [`LivelinessTracker`]), the endpoints ([`RtpsWriter`], [`RtpsReader`]), and
//! the synchronous logic that turns one arriving datagram into a list of
//! datagrams to send, a list of samples to deliver and a list of discovery
//! events to publish.
//!
//! This is the file [`participant`](crate::behavior::participant) does *not*
//! contain, and the split is the same one the crate makes twice already:
//! sockets and the loop on one side, decisions on the other. Every method here
//! takes `now` as an argument and returns what to do rather than doing it, so
//! the whole of discovery, matching and dispatch can be driven from a test
//! without a runtime.

use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use crate::behavior::cache::ChangeKind;
use crate::behavior::endpoint::{EntityIdAllocator, Outbound, Sample, TopicKey};
use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::behavior::liveliness::{
    LivelinessTracker, ParticipantMessageData, ParticipantMessageKind,
};
use crate::behavior::participant::{Channel, Dispatch, channel_for};
use crate::behavior::reader::{ReaderConfig, RtpsReader};
use crate::behavior::writer::{RtpsWriter, WriterConfig};
use crate::discovery::builtin::builtin_pairs;
use crate::discovery::db::{DiscoveryDb, DiscoveryEvent};
use crate::discovery::endpoint_data::{DiscoveredReaderData, DiscoveredWriterData};
use crate::discovery::matching::{ReaderQos, WriterQos};
use crate::discovery::participant_data::ParticipantData;
use crate::discovery::sedp;
use crate::discovery::spdp::Spdp;
use crate::messages::{Message, SerializedPayload, Submessage};
use crate::security::{EndpointSecurity, SecurityContext};
use crate::structure::{
    ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER,
    ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
    ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER, ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
    ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER, EntityId, Guid, GuidPrefix, Locator, SequenceNumber,
    Time,
};

/// How many of this participant's own leases a peer whose lease ran out is
/// owed the retirements of the endpoints deleted meanwhile. See
/// [`State::lapse_deadline`].
const LAPSE_RETENTION_LEASES: u32 = 2;

/// How long a peer whose lease ran out is owed those retirements when this
/// participant's own lease is infinite: twice the default lease.
const LAPSE_RETENTION_WITHOUT_LEASE: StdDuration = StdDuration::from_secs(200);

/// The protocol state one lock covers.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) spdp: Spdp,
    pub(crate) db: DiscoveryDb,
    pub(crate) liveliness: LivelinessTracker,
    pub(crate) writers: BTreeMap<EntityId, RtpsWriter>,
    pub(crate) readers: BTreeMap<EntityId, RtpsReader>,
    pub(crate) entity_ids: EntityIdAllocator,
    pub(crate) spdp_sample: Option<SequenceNumber>,
    pub(crate) user_locator: Locator,
    pub(crate) heartbeat_period: StdDuration,
    pub(crate) datagram_budget: usize,
    pub(crate) fragment_size: u16,
    pub(crate) fragmentation_threshold: usize,
    pub(crate) shutting_down: bool,
    /// Every key this participant protects and verifies user traffic with.
    ///
    /// Empty unless an endpoint was configured with one, and
    /// [`SecurityContext::is_active`] is checked before any datagram is
    /// touched — so an unsecured participant does not pay a decode, a
    /// re-encode or a copy for the feature existing.
    pub(crate) security: SecurityContext,
    /// The default settings a user endpoint is created with.
    pub(crate) endpoint_security: EndpointSecurity,
}

impl State {
    /// Create the eight builtin endpoints §8.5 requires.
    pub(crate) fn create_builtin_endpoints(&mut self) -> BehaviorResult<()> {
        let prefix = self.spdp.guid().prefix;
        let participant_topic = sedp::participant_topic()?;
        let publications_topic = sedp::publications_topic()?;
        let subscriptions_topic = sedp::subscriptions_topic()?;
        let message_topic = sedp::participant_message_topic()?;

        self.add_builtin_writer(
            prefix,
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
            participant_topic.clone(),
            WriterQos::builtin_spdp(),
        );
        self.add_stateless_reader(
            prefix,
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
            participant_topic,
            ReaderQos::builtin_spdp(),
        );
        self.add_builtin_writer(
            prefix,
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
            publications_topic.clone(),
            WriterQos::builtin_sedp(),
        );
        self.add_builtin_reader(
            prefix,
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
            publications_topic,
            ReaderQos::builtin_sedp(),
        );
        self.add_builtin_writer(
            prefix,
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER,
            subscriptions_topic.clone(),
            WriterQos::builtin_sedp(),
        );
        self.add_builtin_reader(
            prefix,
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
            subscriptions_topic,
            ReaderQos::builtin_sedp(),
        );
        self.add_builtin_writer(
            prefix,
            ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER,
            message_topic.clone(),
            WriterQos::builtin_sedp(),
        );
        self.add_builtin_reader(
            prefix,
            ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER,
            message_topic,
            ReaderQos::builtin_sedp(),
        );
        Ok(())
    }

    fn add_builtin_writer(
        &mut self,
        prefix: GuidPrefix,
        entity_id: EntityId,
        topic: TopicKey,
        qos: WriterQos,
    ) {
        let guid = Guid::new(prefix, entity_id);
        let mut config = self.writer_config(guid, topic);
        config.qos = qos;
        self.writers.insert(entity_id, RtpsWriter::new(config));
    }

    fn add_builtin_reader(
        &mut self,
        prefix: GuidPrefix,
        entity_id: EntityId,
        topic: TopicKey,
        qos: ReaderQos,
    ) {
        let guid = Guid::new(prefix, entity_id);
        self.readers.insert(
            entity_id,
            RtpsReader::new(ReaderConfig::new(guid, topic).with_qos(qos)),
        );
    }

    /// The SPDP participant reader, which accepts from strangers.
    fn add_stateless_reader(
        &mut self,
        prefix: GuidPrefix,
        entity_id: EntityId,
        topic: TopicKey,
        qos: ReaderQos,
    ) {
        let guid = Guid::new(prefix, entity_id);
        self.readers.insert(
            entity_id,
            RtpsReader::new(ReaderConfig::new(guid, topic).with_qos(qos).stateless()),
        );
    }

    /// The writer configuration this participant's settings imply.
    pub(crate) fn writer_config(&self, guid: Guid, topic: TopicKey) -> WriterConfig {
        WriterConfig::new(guid, topic)
            .with_heartbeat_period(self.heartbeat_period)
            .with_datagram_budget(self.datagram_budget)
            .with_fragmentation(self.fragmentation_threshold, self.fragment_size)
    }

    /// The security context string two peers must agree on for one topic.
    ///
    /// The topic and type name, separated by a NUL so no pair of names can
    /// collide by concatenation. Both sides compute it from the same SEDP
    /// vocabulary, which is what lets the derived key id be the lookup key on
    /// the receiving side.
    pub(crate) fn security_context(topic: &TopicKey) -> String {
        format!("{}\0{}", topic.topic_name, topic.type_name)
    }

    /// Register one user endpoint's key material.
    pub(crate) fn register_security(
        &mut self,
        entity_id: EntityId,
        topic: &TopicKey,
        security: &EndpointSecurity,
    ) -> BehaviorResult<()> {
        let context = Self::security_context(topic);
        self.security.register(entity_id, security, &context)?;
        Ok(())
    }

    /// Apply submessage protection to everything a batch is about to send.
    ///
    /// **Called from exactly one place**, `Participant::send_all`, because
    /// that is the only funnel every outbound datagram passes through. An
    /// earlier arrangement protected inside `cadence` and `collect_outbound`
    /// and looked complete; the capture test in `tests/security.rs` found the
    /// third path (`Participant::write`, which produces and sends without
    /// going through either) by reading the octets off the socket. Protecting
    /// at the funnel makes a fourth path impossible rather than unlikely.
    ///
    /// One protected message may become several: wrapping adds octets, and a
    /// datagram that was at the budget before the transform is over it after.
    /// A submessage that will not fit even alone is dropped with a log line
    /// rather than sent in the clear — the alternative is exactly the
    /// downgrade the transform exists to prevent.
    pub(crate) fn protect(
        &mut self,
        outbound: Vec<(Channel, Outbound)>,
    ) -> Vec<(Channel, Outbound)> {
        if !self.security.is_active() {
            return outbound;
        }
        let mut protected = Vec::with_capacity(outbound.len());
        for (channel, item) in outbound {
            match self.security.protect_datagram(&item.datagram) {
                Ok(datagrams) => {
                    for datagram in datagrams {
                        protected.push((channel, Outbound::new(item.locators.clone(), datagram)));
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "dropping a datagram that could not be protected");
                }
            }
        }
        protected
    }

    /// Write (or rewrite) the SPDP sample this participant repeats.
    ///
    /// Filed under the participant's own instance — its GUID is the
    /// `DCPSParticipant` key — because that is where
    /// [`dispose_participant`](Self::dispose_participant) files the disposal.
    /// On one instance the disposal replaces this sample; on two, the sample
    /// would outlive the disposal in the history and be announced again.
    pub(crate) fn refresh_spdp_sample(&mut self, now: Instant) -> BehaviorResult<()> {
        let payload = self.spdp.local().to_payload()?;
        let guid = self.spdp.guid();
        let writer = self
            .writers
            .get_mut(&ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER)
            .ok_or(BehaviorError::UnknownWriter {
                guid: Guid::new(guid.prefix, ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER),
            })?;
        let number = writer.write_change(
            payload.into_cow().into_owned(),
            None,
            ChangeKind::Alive,
            sedp::guid_instance(guid),
            now,
        )?;
        self.spdp_sample = Some(number);
        Ok(())
    }

    /// The SPDP announcement, addressed to the group and to every peer.
    pub(crate) fn spdp_announcement(
        &mut self,
        now: Instant,
    ) -> BehaviorResult<Vec<(Channel, Outbound)>> {
        let targets = self.spdp.announce_targets(&self.db);
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let Some(number) = self.spdp_sample else {
            return Ok(Vec::new());
        };
        let writer = self
            .writers
            .get(&ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER)
            .ok_or(BehaviorError::UnknownWriter {
                guid: Guid::new(
                    self.spdp.guid().prefix,
                    ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
                ),
            })?;
        let announcement =
            writer.announce(number, ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER, targets)?;
        self.spdp.mark_announced(now);
        Ok(announcement
            .into_iter()
            .map(|item| (Channel::Metatraffic, item))
            .collect())
    }

    /// Announce one local endpoint over SEDP.
    ///
    /// The announcement is filed under the endpoint's own instance, its GUID.
    /// The builtin writer keeps `KEEP_LAST 1` of each instance, so a peer
    /// that discovers this participant late is replayed every endpoint's
    /// announcement rather than only the newest one, and
    /// [`dispose_endpoint`](Self::dispose_endpoint), which files its disposal
    /// under the same GUID, replaces exactly this announcement.
    pub(crate) fn announce_endpoint(
        &mut self,
        entity_id: EntityId,
        is_writer: bool,
        now: Instant,
    ) -> BehaviorResult<Vec<(Channel, Outbound)>> {
        let compat = self.spdp.config().compat;
        let instance = sedp::guid_instance(Guid::new(self.spdp.guid().prefix, entity_id));
        let payload = if is_writer {
            let Some(writer) = self.writers.get(&entity_id) else {
                return Ok(Vec::new());
            };
            sedp::publication_for(writer, vec![self.user_locator], compat)?.to_payload()?
        } else {
            let Some(reader) = self.readers.get(&entity_id) else {
                return Ok(Vec::new());
            };
            sedp::subscription_for(reader, vec![self.user_locator], compat)?.to_payload()?
        };
        let announcer = if is_writer {
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER
        } else {
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER
        };
        let Some(writer) = self.writers.get_mut(&announcer) else {
            return Ok(Vec::new());
        };
        writer.write_change(
            payload.into_cow().into_owned(),
            None,
            ChangeKind::Alive,
            instance,
            now,
        )?;
        Ok(writer
            .produce(now)?
            .into_iter()
            .map(|item| (Channel::Metatraffic, item))
            .collect())
    }

    /// Announce that this participant is leaving (§8.5.3.1).
    ///
    /// A disposal on the SPDP topic keyed by the participant GUID. Without
    /// it a peer only learns of the departure when the lease runs out —
    /// a hundred seconds by default — and spends that whole time sending
    /// samples into a void. The receive side of this already existed;
    /// this is what makes it reachable between two AstRS participants.
    ///
    /// Written by [`RtpsWriter::dispose`], like an endpoint's: disposed and
    /// unregistered, and announced straight out of the history — which is
    /// why the writer never sweeps a retirement before the next write or
    /// `produce`. The SPDP writer's only matched readers are the peers'
    /// best-effort SPDP readers, so the next `produce` sends each its copy
    /// and drops it; one with no reader matched drops it at once.
    pub(crate) fn dispose_participant(
        &mut self,
        now: Instant,
    ) -> BehaviorResult<Vec<(Channel, Outbound)>> {
        let targets = self.spdp.announce_targets(&self.db);
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let guid = self.spdp.guid();
        let Some(writer) = self
            .writers
            .get_mut(&ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER)
        else {
            return Ok(Vec::new());
        };
        let number = writer.dispose(guid, now)?;
        let announcement =
            writer.announce(number, ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER, targets)?;
        Ok(announcement
            .into_iter()
            .map(|item| (Channel::Metatraffic, item))
            .collect())
    }

    /// Announce that one local endpoint is gone, and unwire it.
    ///
    /// The SEDP counterpart of [`dispose_participant`](Self::dispose_participant):
    /// a disposal on the publications or subscriptions topic keyed by the
    /// endpoint GUID.
    pub(crate) fn dispose_endpoint(
        &mut self,
        entity_id: EntityId,
        is_writer: bool,
        now: Instant,
    ) -> BehaviorResult<(bool, Vec<(Channel, Outbound)>)> {
        let existed = if is_writer {
            self.writers.remove(&entity_id).is_some()
        } else {
            self.readers.remove(&entity_id).is_some()
        };
        if !existed {
            return Ok((false, Vec::new()));
        }
        // The key goes with the endpoint. Leaving it registered would let a
        // peer keep verifying against a topic nobody is subscribed to any
        // more, and would keep the replay windows alive for it.
        self.security.forget(entity_id);
        let guid = Guid::new(self.spdp.guid().prefix, entity_id);
        let announcer = if is_writer {
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER
        } else {
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER
        };
        let Some(writer) = self.writers.get_mut(&announcer) else {
            return Ok((true, Vec::new()));
        };
        // Filed under `guid`, the instance `announce_endpoint` filed the
        // announcement under: the disposal replaces that one and no other.
        // It is a retirement, so it leaves the history once every matched
        // peer has it — deleting endpoints for the whole life of the
        // participant never grows this writer. See `RtpsWriter::dispose`.
        writer.dispose(guid, now)?;
        let outbound = writer
            .produce(now)?
            .into_iter()
            .map(|item| (Channel::Metatraffic, item))
            .collect();
        Ok((true, outbound))
    }

    /// Which matched writers have missed their reader's `DEADLINE`.
    ///
    /// Computed live rather than stored: a miss is a *condition*, not an
    /// event, and it stops being true the moment a sample arrives.
    pub(crate) fn missed_deadlines(
        &self,
        now: Instant,
    ) -> Vec<crate::behavior::reader::DeadlineMiss> {
        self.readers
            .values()
            .flat_map(|reader| reader.missed_deadlines(now))
            .collect()
    }

    /// A WLP assertion.
    pub(crate) fn assert_liveliness(
        &mut self,
        kind: ParticipantMessageKind,
        now: Instant,
    ) -> BehaviorResult<Vec<(Channel, Outbound)>> {
        let mut message = ParticipantMessageData::automatic(self.spdp.guid().prefix);
        message.kind = kind;
        let payload = message.to_payload()?;
        if matches!(kind, ParticipantMessageKind::Manual) {
            self.spdp.bump_manual_liveliness();
        }
        let mut outbound = Vec::new();
        if let Some(writer) = self
            .writers
            .get_mut(&ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER)
        {
            writer.write(payload.into_cow().into_owned(), None, now)?;
            outbound.extend(
                writer
                    .produce(now)?
                    .into_iter()
                    .map(|item| (Channel::Metatraffic, item)),
            );
        }
        // A manual assertion also changes the SPDP sample, so re-write it.
        if matches!(kind, ParticipantMessageKind::Manual) {
            self.refresh_spdp_sample(now)?;
        }
        Ok(outbound)
    }

    /// The periodic work.
    pub(crate) fn cadence(&mut self, now: Instant) -> BehaviorResult<Vec<(Channel, Outbound)>> {
        let mut outbound = Vec::new();
        if self.spdp.should_announce(now) {
            outbound.extend(self.spdp_announcement(now)?);
        }
        let writer_ids: Vec<EntityId> = self.writers.keys().copied().collect();
        for entity_id in writer_ids {
            let channel = channel_for(entity_id);
            if let Some(writer) = self.writers.get_mut(&entity_id) {
                writer.expire(now);
                outbound.extend(writer.produce(now)?.into_iter().map(|item| (channel, item)));
            }
        }
        let reader_ids: Vec<EntityId> = self.readers.keys().copied().collect();
        for entity_id in reader_ids {
            let channel = channel_for(entity_id);
            if let Some(reader) = self.readers.get_mut(&entity_id) {
                outbound.extend(reader.produce(now)?.into_iter().map(|item| (channel, item)));
                outbound.extend(
                    reader
                        .produce_nack_frags()?
                        .into_iter()
                        .map(|item| (channel, item)),
                );
            }
        }
        // `DEADLINE` is the one QoS whose violation nothing else surfaces:
        // lifespan expires samples, liveliness reaps leases, and a missed
        // deadline is only visible if something asks. This is where it is
        // asked, once per cadence.
        for miss in self.missed_deadlines(now) {
            tracing::warn!(
                writer = %miss.writer,
                since_ms = miss.since.as_millis(),
                period_ms = miss.period.as_millis(),
                "requested deadline missed"
            );
        }
        Ok(outbound)
    }

    /// Drop remote participants whose lease has run out, and unwire them.
    ///
    /// # A lease that ran out here says nothing about the peer
    ///
    /// The peer may be alive, and may never have stopped hearing this
    /// participant: its announcements were lost on the way here, or it was
    /// slow, while this participant's reached it. Then it still holds every
    /// endpoint announced here, and the SEDP writers still owe it the
    /// retirement of each one deleted from now on — a retirement they would
    /// otherwise sweep at once, with nobody matched to be owed it, leaving
    /// the peer with a ghost endpoint for as long as this participant lives.
    /// So the SEDP writers record the peer's readers as *lapsed* rather than
    /// dropping them, and hold those retirements until the peer is
    /// rediscovered — its readers are matched again and replayed them — or
    /// until [`lapse_deadline`](Self::lapse_deadline) passes.
    ///
    /// The other half of the same event is the peer's: it may have expired
    /// this participant while this participant kept it, and then it forgot
    /// what the SEDP writers here had delivered to it. That is the writer's
    /// business, and it sees it in the peer's next `ACKNACK` — see
    /// [`ReaderProxy::accept_acknack`](crate::behavior::proxy::ReaderProxy::accept_acknack).
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<DiscoveryEvent> {
        self.forget_lapses_due(now);
        let events = self.db.expire(now);
        let until = self.lapse_deadline(now);
        for event in &events {
            if let (DiscoveryEvent::ParticipantLost(participant), Some(until)) = (event, until) {
                // Before `apply_departure` unwires the participant, which
                // would drop the readers' watermarks with them.
                self.lapse_participant(*participant, until);
            }
            self.apply_departure(*event);
        }
        for lost in self.liveliness.reap(now) {
            tracing::debug!(participant = %lost.participant, kind = %lost.kind, "liveliness lost");
        }
        events
    }

    /// Decode one datagram and run every submessage through the endpoints it
    /// concerns.
    ///
    /// A submessage a peer got wrong is logged and skipped; the rest of the
    /// datagram is still processed, because §8.3.4 says a receiver interprets
    /// submessages in order and one bad one is not a reason to discard the
    /// good ones that preceded it.
    pub(crate) fn handle_message(
        &mut self,
        datagram: &[u8],
        now: Instant,
    ) -> BehaviorResult<Dispatch> {
        let message = Message::decode(datagram)?;
        let local_prefix = self.spdp.guid().prefix;
        if message.header.guid_prefix == local_prefix {
            // Our own announcement, looped back by multicast.
            return Ok(Dispatch::default());
        }

        // Verify and recover before anything is interpreted. A submessage
        // that does not verify never reaches a reader, and neither does a
        // plaintext one addressed to an endpoint that requires protection —
        // both are the same rejection, and both are logged at the level
        // `SecurityError::is_remote_fault` chooses, because a peer with the
        // wrong key is traffic and a local misconfiguration is not.
        let message = if self.security.is_active() {
            match self.security.unprotect(&message) {
                Ok(clear) => clear,
                Err(error) => {
                    if error.is_remote_fault() {
                        tracing::debug!(%error, "rejecting a datagram that did not verify");
                    } else {
                        tracing::error!(%error, "the local security configuration is unusable");
                    }
                    return Ok(Dispatch::default());
                }
            }
        } else {
            message
        };

        let mut source = message.header.guid_prefix;
        let mut timestamp: Option<Time> = None;
        let mut addressed = true;

        for submessage in message.iter() {
            match submessage {
                Submessage::InfoTimestamp(info) => timestamp = info.timestamp,
                Submessage::InfoSource(info) => {
                    source = info.guid_prefix;
                    timestamp = None;
                }
                Submessage::InfoDestination(info) => {
                    addressed = info.guid_prefix.is_unknown() || info.guid_prefix == local_prefix;
                }
                _ if !addressed => {}
                Submessage::Data(data) => {
                    for reader in self.readers.values_mut() {
                        if let Err(error) = reader.on_data(source, data, timestamp, now) {
                            tracing::debug!(%error, "rejecting a DATA");
                        }
                    }
                }
                Submessage::DataFrag(fragment) => {
                    for reader in self.readers.values_mut() {
                        if let Err(error) = reader.on_data_frag(source, fragment, timestamp, now) {
                            tracing::debug!(%error, "rejecting a DATA_FRAG");
                        }
                    }
                }
                Submessage::Heartbeat(heartbeat) => {
                    for reader in self.readers.values_mut() {
                        reader.on_heartbeat(source, heartbeat);
                    }
                }
                Submessage::Gap(gap) => {
                    for reader in self.readers.values_mut() {
                        reader.on_gap(source, gap);
                    }
                }
                Submessage::AckNack(acknack) => {
                    let remote = Guid::new(source, acknack.reader_id);
                    if let Some(writer) = self.writers.get_mut(&acknack.writer_id) {
                        writer.on_acknack(remote, acknack);
                    }
                }
                Submessage::NackFrag(nack) => {
                    let remote = Guid::new(source, nack.reader_id);
                    if let Some(writer) = self.writers.get_mut(&nack.writer_id) {
                        writer.on_nack_frag(remote, nack);
                    }
                }
                _ => {}
            }
        }

        let mut dispatch = Dispatch::default();
        self.drain_readers(&mut dispatch, now);
        self.collect_outbound(&mut dispatch, now)?;
        Ok(dispatch)
    }

    /// Take everything the readers accepted: builtin samples drive discovery,
    /// user samples go to the application.
    fn drain_readers(&mut self, dispatch: &mut Dispatch, now: Instant) {
        let reader_ids: Vec<EntityId> = self.readers.keys().copied().collect();
        for entity_id in reader_ids {
            let Some(reader) = self.readers.get_mut(&entity_id) else {
                continue;
            };
            let samples = reader.take_all();
            if samples.is_empty() {
                continue;
            }
            if entity_id.is_builtin() {
                for sample in samples {
                    self.absorb_builtin(entity_id, &sample, dispatch, now);
                }
            } else {
                dispatch.samples.push((entity_id, samples));
            }
        }
    }

    /// Apply one builtin sample to the discovery state.
    fn absorb_builtin(
        &mut self,
        reader: EntityId,
        sample: &Sample,
        dispatch: &mut Dispatch,
        now: Instant,
    ) {
        let payload = SerializedPayload::new(sample.payload.as_slice());
        let disposed = !matches!(sample.kind, ChangeKind::Alive);

        if reader == ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER {
            if disposed {
                let participant = sample.writer.participant_guid();
                // It announced its departure, so it is owed nothing — even
                // when its lease had already run out here and there is
                // nothing left to forget below.
                self.forget_lapsed_participant(participant);
                let events = self.db.forget_participant(participant);
                for event in &events {
                    self.apply_departure(*event);
                }
                dispatch.events.extend(events);
                return;
            }
            match ParticipantData::from_payload(&payload) {
                Err(error) => tracing::debug!(%error, "rejecting an SPDP sample"),
                Ok(data) => {
                    if !self.spdp.accepts(&data) {
                        return;
                    }
                    let lease = data
                        .lease_duration
                        .to_std()
                        .unwrap_or(StdDuration::from_secs(100));
                    self.wire_builtin(&data);
                    self.liveliness
                        .assert_automatic(data.guid.participant_guid(), lease, now);
                    if let Some(event) = self.db.observe_participant(data, now) {
                        dispatch.events.push(event);
                    }
                    self.rematch_all();
                }
            }
        } else if reader == ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER {
            if disposed {
                if let Some(event) = self.forget_disposed(sample, |db, guid| db.forget_writer(guid))
                {
                    self.apply_departure(event);
                    dispatch.events.push(event);
                }
                return;
            }
            match DiscoveredWriterData::from_payload(&payload) {
                Err(error) => tracing::debug!(%error, "rejecting an SEDP publication"),
                Ok(data) => {
                    if let Some(event) = self.db.observe_writer(data) {
                        dispatch.events.push(event);
                    }
                    self.rematch_all();
                }
            }
        } else if reader == ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER {
            if disposed {
                if let Some(event) = self.forget_disposed(sample, |db, guid| db.forget_reader(guid))
                {
                    self.apply_departure(event);
                    dispatch.events.push(event);
                }
                return;
            }
            match DiscoveredReaderData::from_payload(&payload) {
                Err(error) => tracing::debug!(%error, "rejecting an SEDP subscription"),
                Ok(data) => {
                    if let Some(event) = self.db.observe_reader(data) {
                        dispatch.events.push(event);
                    }
                    self.rematch_all();
                }
            }
        } else if reader == ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER {
            match ParticipantMessageData::from_payload(&payload) {
                Err(error) => tracing::debug!(%error, "rejecting a WLP sample"),
                Ok(message) => {
                    let lease = self
                        .db
                        .participant(message.participant())
                        .and_then(|remote| remote.lease())
                        .unwrap_or(StdDuration::from_secs(100));
                    self.liveliness.assert_message(&message, lease, now);
                }
            }
        }
    }

    /// Forget the remote endpoint an SEDP disposal names, with `forget`.
    ///
    /// The endpoint is the GUID the reader filed the disposal under, which
    /// it read from `PID_KEY_HASH`, from a `PL_CDR` key's
    /// `PID_ENDPOINT_GUID` or from a plain-CDR key — whichever the peer sent
    /// (see [`sedp::builtin_instance`]); on the SEDP topics a key hash *is*
    /// the endpoint's GUID. Reading the plain-CDR key alone, as this once
    /// did, ignored a disposal that carries only a key hash, which the spec
    /// allows, and misread a `PL_CDR` key as a GUID of garbage: either way
    /// the endpoint stayed known here for as long as its participant lived.
    /// The plain-CDR reading is kept as the fallback when the instance names
    /// no endpoint this participant knows.
    fn forget_disposed(
        &mut self,
        sample: &Sample,
        forget: impl Fn(&mut DiscoveryDb, Guid) -> Option<DiscoveryEvent>,
    ) -> Option<DiscoveryEvent> {
        let named = sedp::guid_of_instance(sample.instance);
        let keyed = sedp::guid_from_key(&sample.payload);
        [named, keyed]
            .into_iter()
            .flatten()
            .find_map(|guid| forget(&mut self.db, guid))
    }

    /// The builtin writers whose retirements say an endpoint is gone, and
    /// which therefore keep lapsed readers: see [`expire`](Self::expire).
    const ENDPOINT_ANNOUNCERS: [EntityId; 2] = [
        ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
        ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER,
    ];

    /// Record every reader `participant` has on the SEDP writers as lapsed,
    /// until `until`.
    fn lapse_participant(&mut self, participant: Guid, until: Instant) {
        for entity_id in Self::ENDPOINT_ANNOUNCERS {
            if let Some(writer) = self.writers.get_mut(&entity_id) {
                writer.lapse_participant(participant, until);
            }
        }
    }

    /// Forget every lapsed reader `participant` has on the SEDP writers.
    fn forget_lapsed_participant(&mut self, participant: Guid) {
        for entity_id in Self::ENDPOINT_ANNOUNCERS {
            if let Some(writer) = self.writers.get_mut(&entity_id) {
                writer.forget_lapsed_participant(participant);
            }
        }
    }

    /// Forget every lapsed reader on the SEDP writers whose time is up.
    fn forget_lapses_due(&mut self, now: Instant) {
        for entity_id in Self::ENDPOINT_ANNOUNCERS {
            if let Some(writer) = self.writers.get_mut(&entity_id) {
                writer.forget_lapses_due(now);
            }
        }
    }

    /// Until when a peer whose lease ran out at `now` is owed the
    /// retirements of the endpoints deleted here meanwhile.
    ///
    /// Twice this participant's own lease. A peer that cannot hear this
    /// participant either gives up on it within one lease of the last
    /// announcement it heard, which came before the peer's own went silent
    /// here, and forgets every endpoint with it: nothing is owed after that.
    /// The second lease is for the peer's next announcement to arrive here
    /// once the outage ends. A peer that heard this participant all along
    /// never gives up on it, and one that stays unheard for longer keeps a
    /// ghost of an endpoint deleted meanwhile; nothing bounded can be owed to
    /// a peer that may be gone for good. An infinite lease holds for
    /// [`LAPSE_RETENTION_WITHOUT_LEASE`]. `None` only when the deadline would
    /// overflow the clock.
    fn lapse_deadline(&self, now: Instant) -> Option<Instant> {
        let retention = self
            .spdp
            .config()
            .lease_duration
            .to_std()
            .map_or(LAPSE_RETENTION_WITHOUT_LEASE, |lease| {
                lease.saturating_mul(LAPSE_RETENTION_LEASES)
            });
        now.checked_add(retention)
            .or_else(|| now.checked_add(LAPSE_RETENTION_WITHOUT_LEASE))
    }

    /// Everything the endpoints want to send after a datagram was absorbed.
    fn collect_outbound(&mut self, dispatch: &mut Dispatch, now: Instant) -> BehaviorResult<()> {
        let writer_ids: Vec<EntityId> = self.writers.keys().copied().collect();
        for entity_id in writer_ids {
            let channel = channel_for(entity_id);
            if let Some(writer) = self.writers.get_mut(&entity_id) {
                dispatch
                    .outbound
                    .extend(writer.produce(now)?.into_iter().map(|item| (channel, item)));
            }
        }
        let reader_ids: Vec<EntityId> = self.readers.keys().copied().collect();
        for entity_id in reader_ids {
            let channel = channel_for(entity_id);
            if let Some(reader) = self.readers.get_mut(&entity_id) {
                dispatch
                    .outbound
                    .extend(reader.produce(now)?.into_iter().map(|item| (channel, item)));
            }
        }
        Ok(())
    }

    /// Wire this participant's builtin endpoints to a peer's, from the
    /// `PID_BUILTIN_ENDPOINT_SET` it announced.
    ///
    /// SEDP cannot announce the SEDP endpoints — it *is* them — so the
    /// builtin wiring comes from the SPDP mask and nothing else.
    pub(crate) fn wire_builtin(&mut self, remote: &ParticipantData) {
        let local_prefix = self.spdp.guid().prefix;
        let locators = remote.metatraffic_locators();
        if locators.is_empty() {
            tracing::debug!(participant = %remote.guid, "peer announced no metatraffic locator");
            return;
        }
        let pairs = builtin_pairs(
            local_prefix,
            self.spdp.config().builtin_endpoints,
            remote.guid.prefix,
            remote.available_builtin_endpoints,
        );
        for pair in pairs {
            if pair.writer.prefix == local_prefix {
                if let Some(writer) = self.writers.get_mut(&pair.writer.entity_id) {
                    writer.match_reader(sedp::builtin_reader_proxy(pair, locators.clone()));
                }
            } else if pair.reader.prefix == local_prefix
                && let Some(reader) = self.readers.get_mut(&pair.reader.entity_id)
            {
                reader.match_writer(sedp::builtin_writer_proxy(pair, locators.clone()));
            }
        }
    }

    /// Re-run request-versus-offered for every user endpoint.
    pub(crate) fn rematch_all(&mut self) {
        let writer_ids: Vec<EntityId> = self
            .writers
            .keys()
            .copied()
            .filter(|id| !id.is_builtin())
            .collect();
        for entity_id in writer_ids {
            self.rematch_writer(entity_id);
        }
        let reader_ids: Vec<EntityId> = self
            .readers
            .keys()
            .copied()
            .filter(|id| !id.is_builtin())
            .collect();
        for entity_id in reader_ids {
            self.rematch_reader(entity_id);
        }
    }

    /// Match one local writer against every known remote reader.
    pub(crate) fn rematch_writer(&mut self, entity_id: EntityId) {
        let Some(writer) = self.writers.get(&entity_id) else {
            return;
        };
        let topic = writer.topic().clone();
        let qos = *writer.qos();
        let mut wanted: Vec<(Guid, crate::behavior::proxy::ReaderProxy)> = Vec::new();
        let mut unwanted: Vec<Guid> = Vec::new();
        for (remote, outcome) in self.db.readers_for(&topic, &qos) {
            let guid = remote.guid();
            if outcome.is_matched() {
                let locators = remote
                    .identity
                    .resolve_locators(&self.default_locators_of(guid));
                if locators.is_empty() {
                    continue;
                }
                wanted.push((guid, sedp::reader_proxy_for(remote, &qos, locators)));
            } else {
                unwanted.push(guid);
            }
        }
        let Some(writer) = self.writers.get_mut(&entity_id) else {
            return;
        };
        for guid in unwanted {
            writer.unmatch_reader(guid);
        }
        for (_, proxy) in wanted {
            writer.match_reader(proxy);
        }
    }

    /// Match one local reader against every known remote writer.
    pub(crate) fn rematch_reader(&mut self, entity_id: EntityId) {
        let Some(reader) = self.readers.get(&entity_id) else {
            return;
        };
        let topic = reader.topic().clone();
        let qos = *reader.qos();
        let mut wanted: Vec<crate::behavior::proxy::WriterProxy> = Vec::new();
        let mut unwanted: Vec<Guid> = Vec::new();
        for (remote, outcome) in self.db.writers_for(&topic, &qos) {
            let guid = remote.guid();
            if outcome.is_matched() {
                let locators = remote
                    .identity
                    .resolve_locators(&self.default_locators_of(guid));
                if locators.is_empty() {
                    continue;
                }
                wanted.push(sedp::writer_proxy_for(remote, &qos, locators));
            } else {
                unwanted.push(guid);
            }
        }
        let Some(reader) = self.readers.get_mut(&entity_id) else {
            return;
        };
        for guid in unwanted {
            reader.unmatch_writer(guid);
        }
        for proxy in wanted {
            reader.match_writer(proxy);
        }
    }

    /// The user-traffic locators of the participant that owns `endpoint`.
    fn default_locators_of(&self, endpoint: Guid) -> Vec<Locator> {
        self.db
            .participant(endpoint)
            .map(crate::discovery::db::RemoteParticipant::default_locators)
            .unwrap_or_default()
    }

    /// Unwire whatever a departure event named.
    pub(crate) fn apply_departure(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::WriterLost(guid) => {
                for reader in self.readers.values_mut() {
                    reader.unmatch_writer(guid);
                }
            }
            DiscoveryEvent::ReaderLost(guid) => {
                for writer in self.writers.values_mut() {
                    writer.unmatch_reader(guid);
                }
            }
            DiscoveryEvent::ParticipantLost(guid) => {
                for reader in self.readers.values_mut() {
                    reader.unmatch_participant(guid);
                }
                for writer in self.writers.values_mut() {
                    writer.unmatch_participant(guid);
                }
                self.liveliness.forget(guid);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
