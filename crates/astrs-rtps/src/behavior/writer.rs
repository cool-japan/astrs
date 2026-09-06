//! The RTPS writer: one implementation, two behaviours.
//!
//! §8.4.7 and §8.4.9 describe a stateless writer and a stateful writer as
//! separate machines. They are not, quite: the stateless one is the stateful
//! one with the reliability half switched off. This module implements the
//! stateful machine and lets [`WriterQos::reliability`] decide whether the
//! HEARTBEAT/ACKNACK cycle runs, which means best-effort and reliable share
//! one history cache, one matching path and one fragmentation path — and the
//! best-effort case cannot rot, because every test of the common path covers
//! it.
//!
//! # No sockets, no clock
//!
//! Every method here is synchronous and every one that emits takes `now` as
//! an argument. [`RtpsWriter::produce`] returns [`Outbound`] values for the
//! participant to send. There is no interior mutability, no task, no
//! `Instant::now()` outside a `Default`. The whole reliability protocol is
//! therefore a pure function of (state, input, time), and its tests need no
//! runtime.
//!
//! # What a `produce` call decides, in order
//!
//! 1. **Repairs first.** Anything a reader explicitly nacked goes before
//!    anything new. A reader stuck on sample 3 is not helped by sample 40.
//! 2. **Then new samples**, oldest first, for readers that are behind.
//! 3. **Then GAPs**, for sequence numbers the history no longer holds — a
//!    reader asking for an evicted or expired sample must be told it is
//!    never coming, or it will ask forever.
//! 4. **Then a HEARTBEAT**, when the cadence is due or a reader is behind.
//!    Best-effort writers skip this step entirely.
//!
//! # Fragmentation
//!
//! A sample above [`FRAGMENTATION_THRESHOLD`] is sent as `DATA_FRAG`
//! submessages, one datagram's worth at a time. The threshold and the
//! fragment size are configuration, not constants, so a test can force
//! fragmentation on a small payload and a real deployment can match its MTU.

use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use crate::behavior::cache::{CacheChange, ChangeKind, HistoryCache, InstanceHandle};
use crate::behavior::endpoint::{Outbound, TopicKey};
use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::behavior::fragment::{
    DEFAULT_FRAGMENT_SIZE, FRAGMENTATION_THRESHOLD, FragmentPlan, fragment_sample,
};
use crate::behavior::proxy::ReaderProxy;
use crate::discovery::matching::WriterQos;
use crate::security::EndpointSecurity;
use astrs_cdr::{Endianness, ParameterId, ParameterList};

use crate::messages::{
    Data, DataPayload, Gap, Header, Heartbeat, InfoDestination, InfoTimestamp, Message,
    MessageBuilder, NackFrag, SerializedPayload, Submessage, inline_qos_encoding,
};
use crate::structure::{
    EntityId, Guid, Locator, MAX_SET_BITS, SequenceNumber, SequenceNumberSet, Time,
};

/// How often a reliable writer heartbeats when nothing else prompts it.
pub const DEFAULT_HEARTBEAT_PERIOD: StdDuration = StdDuration::from_millis(200);

/// How long a writer waits before answering a NACK, to coalesce a burst.
///
/// Zero here, deliberately. The delay exists to avoid a NACK storm from many
/// readers on a lossy link; on the loopback path the deterministic tests use
/// it would only add latency, and a caller that wants it can set it.
pub const DEFAULT_NACK_RESPONSE_DELAY: StdDuration = StdDuration::ZERO;

/// Octets a `DATA` costs beyond its payload, at the worst case this writer
/// emits.
///
/// The RTPS header (20), an `INFO_DST` (16), an `INFO_TS` (12) and the `DATA`
/// submessage header and prelude (24). A sample larger than
/// `datagram_budget - DATAGRAM_OVERHEAD` cannot be sent whole, whatever
/// [`WriterConfig::fragmentation_threshold`] says, which is why
/// [`WriterConfig::effective_threshold`] takes the smaller of the two.
pub const DATAGRAM_OVERHEAD: usize = 20 + 16 + 12 + 24;

/// Octets a `DATA_FRAG` costs beyond its fragment payload, at the worst case
/// this writer emits.
///
/// The RTPS header (20), an `INFO_DST` (16), an `INFO_TS` (12) and the
/// `DATA_FRAG` submessage header and prelude (4 + 36). The `DATA` form of the
/// same sum is [`DATAGRAM_OVERHEAD`]; a fragment's prelude is longer because
/// it also carries the fragment number, the fragment size and the sample
/// size.
pub const FRAGMENT_DATAGRAM_OVERHEAD: usize =
    20 + 16 + 12 + (4 + crate::messages::DATA_FRAG_PRELUDE_LEN);

/// The smallest fragment a protected writer will cut a sample into.
///
/// A floor, so that a pathologically small datagram budget produces a
/// fragmentation plan that is merely inefficient rather than one with a
/// zero-octet fragment size.
pub const MIN_PROTECTED_FRAGMENT_SIZE: u16 = 64;

/// Most samples one `produce` call will put on the wire per reader.
///
/// Bounds the work a single call does when a reader has been away for a long
/// time; the rest goes out on the next call.
pub const MAX_SAMPLES_PER_PRODUCE: usize = 256;

/// How a writer is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterConfig {
    /// The writer's GUID.
    pub guid: Guid,
    /// The topic and type it publishes.
    pub topic: TopicKey,
    /// The QoS it offers.
    pub qos: WriterQos,
    /// How often to heartbeat when nothing else prompts one.
    pub heartbeat_period: StdDuration,
    /// How long to wait before answering a NACK.
    pub nack_response_delay: StdDuration,
    /// Octets per fragment when fragmenting.
    pub fragment_size: u16,
    /// Octets one datagram may occupy.
    pub datagram_budget: usize,
    /// Samples above this size are fragmented.
    pub fragmentation_threshold: usize,
    /// The pre-shared key and protection level this writer's submessages
    /// travel under.
    ///
    /// [`EndpointSecurity::none`] by default, which is byte-for-byte the
    /// behaviour of a build without [`crate::security`]. Setting it does two
    /// things: the participant registers the derived key, and
    /// [`effective_threshold`](Self::effective_threshold) shrinks by the
    /// per-submessage cost of the transform, so a sample that would have been
    /// sent whole and then failed to fit once wrapped is fragmented instead.
    pub security: EndpointSecurity,
}

impl WriterConfig {
    /// A writer on `topic` with default QoS and cadence.
    #[must_use]
    pub fn new(guid: Guid, topic: TopicKey) -> Self {
        Self {
            guid,
            topic,
            qos: WriterQos::default(),
            heartbeat_period: DEFAULT_HEARTBEAT_PERIOD,
            nack_response_delay: DEFAULT_NACK_RESPONSE_DELAY,
            fragment_size: DEFAULT_FRAGMENT_SIZE,
            datagram_budget: crate::messages::MAX_UDP_PAYLOAD,
            fragmentation_threshold: FRAGMENTATION_THRESHOLD,
            security: EndpointSecurity::none(),
        }
    }

    /// Replace the security settings.
    #[must_use]
    pub fn with_security(mut self, security: EndpointSecurity) -> Self {
        self.security = security;
        self
    }

    /// Replace the QoS.
    #[must_use]
    pub const fn with_qos(mut self, qos: WriterQos) -> Self {
        self.qos = qos;
        self
    }

    /// Replace the heartbeat cadence.
    #[must_use]
    pub const fn with_heartbeat_period(mut self, period: StdDuration) -> Self {
        self.heartbeat_period = period;
        self
    }

    /// Replace the fragmentation settings.
    #[must_use]
    pub const fn with_fragmentation(mut self, threshold: usize, fragment_size: u16) -> Self {
        self.fragmentation_threshold = threshold;
        self.fragment_size = fragment_size;
        self
    }

    /// Replace the per-datagram budget.
    #[must_use]
    pub const fn with_datagram_budget(mut self, budget: usize) -> Self {
        self.datagram_budget = budget;
        self
    }

    /// Octets one datagram may occupy *before* submessage protection.
    ///
    /// A protected submessage grows by [`EndpointSecurity::overhead`], and it
    /// grows after this writer has finished packing — so the packing has to
    /// leave the room. Without this a single `DATA_FRAG` sized to fill the
    /// budget exactly would be refused by the transform, and refused is what
    /// it must be: sending it in the clear would be the downgrade the whole
    /// module exists to prevent.
    ///
    /// Identical to [`datagram_budget`](Self::datagram_budget) for an
    /// unprotected writer.
    #[must_use]
    pub const fn effective_budget(&self) -> usize {
        self.datagram_budget
            .saturating_sub(self.security.overhead())
    }

    /// Octets of payload one `DATA_FRAG` carries.
    ///
    /// [`fragment_size`](Self::fragment_size) for an unprotected writer —
    /// byte for byte the pre-security plan, so an unprotected deployment
    /// fragments exactly as it always did. A protected one caps it at what a
    /// datagram can still hold once the fragment has grown by
    /// [`EndpointSecurity::overhead`], because a `DATA_FRAG` is alone in its
    /// datagram and there is nothing to split off it.
    #[must_use]
    pub fn effective_fragment_size(&self) -> u16 {
        if !self.security.protection.is_protecting() {
            return self.fragment_size;
        }
        let room = self
            .effective_budget()
            .saturating_sub(FRAGMENT_DATAGRAM_OVERHEAD);
        let capped = u16::try_from(room)
            .unwrap_or(u16::MAX)
            .max(MIN_PROTECTED_FRAGMENT_SIZE);
        self.fragment_size.min(capped)
    }

    /// The size above which this writer actually fragments.
    ///
    /// The smaller of the configured threshold and what one datagram can
    /// hold. The two are different questions and both matter: the threshold
    /// is a policy — blueprint §10.2 fixes it at 64 KiB — and the budget is a
    /// physical limit. A sample of 64 KiB does not fit a 1400-octet datagram
    /// however permissive the policy is, and an implementation that trusted
    /// the policy alone would hand the kernel a datagram it refuses.
    ///
    /// A protected writer subtracts a third thing:
    /// [`EndpointSecurity::overhead`], what wrapping one submessage costs. A
    /// datagram carrying several submessages can be split once it is
    /// protected, but a datagram carrying *one* large `DATA` cannot — one
    /// submessage is one datagram at minimum — so the room the transform will
    /// need has to come out of the threshold rather than out of the split.
    #[must_use]
    pub const fn effective_threshold(&self) -> usize {
        let room = self.effective_budget().saturating_sub(DATAGRAM_OVERHEAD);
        if room < self.fragmentation_threshold {
            room
        } else {
            self.fragmentation_threshold
        }
    }
}

/// An RTPS writer: history, matched readers, and the cadence that serves
/// them.
#[derive(Debug, Clone)]
pub struct RtpsWriter {
    config: WriterConfig,
    cache: HistoryCache,
    last_change: SequenceNumber,
    heartbeat_count: i32,
    matched: BTreeMap<Guid, ReaderProxy>,
    last_heartbeat_at: Option<Instant>,
}

impl RtpsWriter {
    /// Build a writer from its configuration.
    #[must_use]
    pub fn new(config: WriterConfig) -> Self {
        let cache = HistoryCache::new(config.qos.history)
            .with_limits(config.qos.resource_limits)
            .with_lifespan(config.qos.lifespan());
        Self {
            config,
            cache,
            last_change: SequenceNumber::ZERO,
            heartbeat_count: 0,
            matched: BTreeMap::new(),
            last_heartbeat_at: None,
        }
    }

    /// The writer's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.config.guid
    }

    /// The topic and type it publishes.
    #[must_use]
    pub const fn topic(&self) -> &TopicKey {
        &self.config.topic
    }

    /// Write a disposal: the octets that say an instance is gone.
    ///
    /// The key is what identifies the instance — for a builtin discovery
    /// topic, the departing entity's GUID — wrapped in a `CDR_LE`
    /// encapsulation header so a receiver can find it at a fixed offset.
    ///
    /// # Errors
    ///
    /// As [`write`](Self::write).
    pub fn dispose(&mut self, key: Guid, now: Instant) -> BehaviorResult<SequenceNumber> {
        let mut payload = Vec::with_capacity(astrs_cdr::ENCAPSULATION_HEADER_LEN + 16);
        payload.extend_from_slice(
            &astrs_cdr::EncapsulationHeader::new(astrs_cdr::EncapsulationKind::CdrLe).to_bytes(),
        );
        payload.extend_from_slice(&key.to_bytes());
        self.write_change(
            payload,
            None,
            ChangeKind::NotAliveDisposed,
            InstanceHandle::NIL,
            now,
        )
    }

    /// The QoS it offers.
    #[must_use]
    pub const fn qos(&self) -> &WriterQos {
        &self.config.qos
    }

    /// The configuration it was built with.
    #[must_use]
    pub const fn config(&self) -> &WriterConfig {
        &self.config
    }

    /// True when the reliability half of the machine is running.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        self.config.qos.is_reliable()
    }

    /// The history cache, for inspection.
    #[must_use]
    pub const fn cache(&self) -> &HistoryCache {
        &self.cache
    }

    /// The highest sequence number ever written.
    #[must_use]
    pub const fn last_change(&self) -> SequenceNumber {
        self.last_change
    }

    /// The lowest sequence number still held, or one past `last_change` when
    /// the history is empty.
    ///
    /// This is a HEARTBEAT's `firstSN`, and §8.3.7.5.3 requires
    /// `firstSN >= 1`, so an empty history reports `lastSN + 1` rather than
    /// zero — the standard "I hold nothing" encoding.
    #[must_use]
    pub fn first_available(&self) -> SequenceNumber {
        self.cache
            .min_sequence_number()
            .unwrap_or_else(|| self.last_change.next())
    }

    /// How many readers are matched.
    #[must_use]
    pub fn matched_reader_count(&self) -> usize {
        self.matched.len()
    }

    /// True when `guid` is a matched reader.
    #[must_use]
    pub fn is_matched(&self, guid: Guid) -> bool {
        self.matched.contains_key(&guid)
    }

    /// The matched readers.
    pub fn matched_readers(&self) -> impl Iterator<Item = &ReaderProxy> {
        self.matched.values()
    }

    /// Add or replace a matched reader.
    ///
    /// Idempotent: matching a reader that is already matched refreshes its
    /// locators without disturbing its acknowledgement state, which is what a
    /// repeated SEDP announcement should do.
    ///
    /// # Who gets the history
    ///
    /// A late joiner is replayed the writer's history only when *both* sides
    /// asked for it: this writer's `DURABILITY` must be `TRANSIENT_LOCAL`, so
    /// the samples are still there, and the reader's must be too, so it
    /// actually wants them. Either one saying `VOLATILE` starts the proxy at
    /// [`last_change`](Self::last_change), which is the RTPS way of saying
    /// "you begin at the present" (§8.4.9.1, and DDS 1.4 §2.2.3.4, where the
    /// `DataReader`'s `DURABILITY` is what decides whether pre-existing
    /// samples are delivered to *it*).
    ///
    /// The two are genuinely different questions. A `TRANSIENT_LOCAL` writer
    /// with one `VOLATILE` reader still keeps its history, because the next
    /// reader to appear may want it.
    pub fn match_reader(&mut self, proxy: ReaderProxy) {
        let guid = proxy.guid();
        match self.matched.get_mut(&guid) {
            Some(existing) => {
                existing.set_locators(proxy.locators(), Vec::new());
            }
            None => {
                let mut proxy = proxy;
                if !self.replays_history_to(&proxy) {
                    // VOLATILE on either side: the late joiner starts where
                    // the writer is now, not at sample one. See
                    // `ReaderProxy::skip_history_through`.
                    proxy.skip_history_through(self.last_change);
                }
                self.matched.insert(guid, proxy);
            }
        }
    }

    /// True when `proxy` is entitled to the samples written before it matched.
    ///
    /// The conjunction [`match_reader`](Self::match_reader) documents.
    #[must_use]
    pub const fn replays_history_to(&self, proxy: &ReaderProxy) -> bool {
        self.config.qos.replays_history() && proxy.wants_history()
    }

    /// Remove a matched reader.
    pub fn unmatch_reader(&mut self, guid: Guid) -> bool {
        self.matched.remove(&guid).is_some()
    }

    /// Remove every matched reader belonging to one participant.
    pub fn unmatch_participant(&mut self, participant: Guid) -> usize {
        let doomed: Vec<Guid> = self
            .matched
            .keys()
            .filter(|guid| guid.prefix == participant.prefix)
            .copied()
            .collect();
        for guid in &doomed {
            self.matched.remove(guid);
        }
        doomed.len()
    }

    /// Write a sample and return the sequence number it was given.
    ///
    /// The sample enters the history immediately; nothing goes on the wire
    /// until [`produce`](Self::produce) is called. Anything the history
    /// policy pushed out is remembered so a `GAP` can name it.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::HistoryFull`] when `KEEP_ALL` has reached its
    /// resource limit, and [`BehaviorError::SampleTooLarge`] when the payload
    /// is bigger than fragmentation can carry.
    pub fn write(
        &mut self,
        payload: impl Into<Vec<u8>>,
        source_timestamp: Option<Time>,
        now: Instant,
    ) -> BehaviorResult<SequenceNumber> {
        self.write_change(
            payload,
            source_timestamp,
            ChangeKind::Alive,
            InstanceHandle::NIL,
            now,
        )
    }

    /// Write a sample with an explicit lifecycle kind and instance.
    ///
    /// # Errors
    ///
    /// As [`write`](Self::write).
    pub fn write_change(
        &mut self,
        payload: impl Into<Vec<u8>>,
        source_timestamp: Option<Time>,
        kind: ChangeKind,
        instance: InstanceHandle,
        now: Instant,
    ) -> BehaviorResult<SequenceNumber> {
        let payload = payload.into();
        if payload.len() > crate::behavior::fragment::MAX_REASSEMBLY_SAMPLE as usize {
            return Err(BehaviorError::SampleTooLarge {
                len: payload.len(),
                limit: crate::behavior::fragment::MAX_REASSEMBLY_SAMPLE as usize,
            });
        }
        let sequence_number = self.last_change.next();
        let change = CacheChange::at(sequence_number, payload, now)
            .with_kind(kind)
            .with_instance(instance);
        let change = match source_timestamp {
            Some(timestamp) => change.with_source_timestamp(timestamp),
            None => change,
        };
        let _evicted = self.cache.insert(change)?;
        self.last_change = sequence_number;
        Ok(sequence_number)
    }

    /// Drop one sample from the history, whatever the policies say.
    ///
    /// The application-level `dispose` path, and the hook a test uses to
    /// create a hole the writer must `GAP` rather than resend. Returns
    /// whether the sample was there.
    pub fn forget(&mut self, sequence_number: SequenceNumber) -> bool {
        self.cache.remove(sequence_number).is_some()
    }

    /// Drop everything the lifespan has aged out.
    ///
    /// Returns the sequence numbers that expired, which the next
    /// [`produce`](Self::produce) will `GAP`.
    pub fn expire(&mut self, now: Instant) -> Vec<SequenceNumber> {
        self.cache
            .expire(now)
            .iter()
            .map(|removal| removal.sequence_number())
            .collect()
    }

    /// Drop history every matched reader has acknowledged.
    ///
    /// Only meaningful under `KEEP_ALL` **and** `VOLATILE`, and both
    /// exclusions are load-bearing:
    ///
    /// - A `KEEP_LAST` cache is already bounded by its depth, so there is
    ///   nothing here to win, and dropping acknowledged samples out of it
    ///   would leave a `TRANSIENT_LOCAL` writer with less than its depth to
    ///   replay to the *next* reader.
    /// - A `TRANSIENT_LOCAL` writer must not drop a sample merely because
    ///   every reader that exists *today* has acknowledged it. The whole
    ///   promise of the policy is to the reader that has not appeared yet.
    ///   `KEEP_ALL` + `TRANSIENT_LOCAL` therefore grows until
    ///   `RESOURCE_LIMITS` stops it — which is the DDS contract, not a leak:
    ///   a writer that wants a bound states one, in the depth or in the
    ///   limits.
    ///
    /// Returns how many samples were dropped, which is zero in both excluded
    /// cases.
    pub fn reclaim(&mut self) -> usize {
        if self.config.qos.history.retained().is_some() || self.config.qos.replays_history() {
            return 0;
        }
        let Some(watermark) = self.acked_by_all() else {
            return 0;
        };
        self.cache.drop_through(watermark)
    }

    /// The highest sequence number every matched reliable reader has
    /// acknowledged, or `None` when there is no reliable reader.
    #[must_use]
    pub fn acked_by_all(&self) -> Option<SequenceNumber> {
        self.matched
            .values()
            .filter(|proxy| proxy.is_reliable() && proxy.is_active())
            .map(ReaderProxy::acked_through)
            .min()
    }

    /// True when every matched reliable reader has acknowledged everything
    /// written.
    #[must_use]
    pub fn is_acknowledged(&self) -> bool {
        self.matched
            .values()
            .filter(|proxy| proxy.is_reliable() && proxy.is_active())
            .all(|proxy| proxy.acked_through() >= self.last_change)
    }

    /// Apply an `ACKNACK` from a matched reader.
    ///
    /// Returns `true` when the submessage was fresh and applied. The repairs
    /// it asks for go out on the next [`produce`](Self::produce).
    pub fn on_acknack(&mut self, reader: Guid, acknack: &crate::messages::AckNack) -> bool {
        let first_available = self.first_available();
        match self.matched.get_mut(&reader) {
            None => false,
            Some(proxy) => {
                let applied = proxy.accept_acknack(acknack);
                if applied {
                    proxy.drop_requested_below(first_available);
                }
                applied
            }
        }
    }

    /// Apply a `NACK_FRAG` from a matched reader.
    ///
    /// Fragment-level repair is served by resending the whole sample: a
    /// writer that still holds the sample can always rebuild any fragment of
    /// it, and the cost of a few extra fragments is far below the cost of a
    /// per-fragment retransmission index that would have to be kept per
    /// reader.
    pub fn on_nack_frag(&mut self, reader: Guid, nack: &NackFrag) -> bool {
        let held = self.cache.contains(nack.writer_sn);
        match self.matched.get_mut(&reader) {
            None => false,
            Some(proxy) if held => {
                proxy.request_resend(nack.writer_sn);
                true
            }
            Some(_) => false,
        }
    }

    /// Everything this writer wants to put on the wire right now.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`] when a submessage will not encode, which
    /// would mean a bug in this crate rather than in a peer.
    pub fn produce(&mut self, now: Instant) -> BehaviorResult<Vec<Outbound>> {
        let mut outbound = Vec::new();
        let reader_guids: Vec<Guid> = self.matched.keys().copied().collect();
        for reader in reader_guids {
            outbound.extend(self.produce_for(reader, now)?);
        }
        if self.should_heartbeat(now) {
            outbound.extend(self.produce_heartbeat(now, false)?);
        }
        Ok(outbound)
    }

    /// A HEARTBEAT to every matched reliable reader, cadence or not.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`].
    pub fn force_heartbeat(&mut self, now: Instant) -> BehaviorResult<Vec<Outbound>> {
        self.produce_heartbeat(now, false)
    }

    /// A HEARTBEAT with the `L` flag: the writer is alive, whatever its
    /// history says.
    ///
    /// This is the `MANUAL_BY_TOPIC` liveliness assertion of §8.7.2.2.3.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`].
    pub fn assert_liveliness(&mut self, now: Instant) -> BehaviorResult<Vec<Outbound>> {
        self.produce_heartbeat(now, true)
    }

    /// Build a `DATA` for one held sample, addressed to arbitrary locators.
    ///
    /// The path SPDP uses: an announcement goes to the multicast group and to
    /// every initial peer, none of which is a matched reader yet.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`] when the message will not encode.
    pub fn announce(
        &self,
        sequence_number: SequenceNumber,
        reader_id: EntityId,
        targets: Vec<Locator>,
    ) -> BehaviorResult<Option<Outbound>> {
        let Some(change) = self.cache.get(sequence_number) else {
            return Ok(None);
        };
        if targets.is_empty() {
            return Ok(None);
        }
        let mut message = Message::new(self.header());
        if let Some(timestamp) = change.source_timestamp {
            message.push(InfoTimestamp::at(timestamp));
        }
        message.push(self.data_for(change, reader_id));
        Ok(Some(Outbound::new(targets, message.encode()?)))
    }

    /// The header every message from this writer's participant carries.
    #[must_use]
    fn header(&self) -> Header {
        Header::new(self.config.guid.prefix)
    }

    /// Whether the cadence says a heartbeat is due.
    fn should_heartbeat(&self, now: Instant) -> bool {
        if !self.is_reliable() || self.matched.is_empty() {
            return false;
        }
        match self.last_heartbeat_at {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.config.heartbeat_period,
        }
    }

    /// Everything for one reader.
    fn produce_for(&mut self, reader: Guid, now: Instant) -> BehaviorResult<Vec<Outbound>> {
        let Some(proxy) = self.matched.get(&reader) else {
            return Ok(Vec::new());
        };
        if !proxy.is_active() {
            return Ok(Vec::new());
        }
        let locators = proxy.locators();
        if locators.is_empty() {
            return Ok(Vec::new());
        }
        let reader_id = proxy.guid().entity_id;
        let reader_prefix = proxy.guid().prefix;
        let reliable = proxy.is_reliable();

        // Repairs first, then anything the reader has not been sent. A
        // sample at or below the reader's acknowledged watermark is never
        // sent again, whatever `highest_sent` says: an ACKNACK that
        // acknowledges through 2 proves the reader has 1 and 2, even if this
        // writer never sent them (a second writer, or a replay, may have).
        let floor = proxy.highest_sent().max(proxy.acked_through());
        let mut wanted: Vec<SequenceNumber> = proxy
            .requested()
            .filter(|number| *number > proxy.acked_through())
            .collect();
        let start = floor.next();
        let mut number = start;
        while number <= self.last_change && wanted.len() < MAX_SAMPLES_PER_PRODUCE {
            wanted.push(number);
            number = number.next();
        }
        wanted.sort_unstable();
        wanted.dedup();

        let mut missing = Vec::new();
        let mut sendable = Vec::new();
        for number in wanted.into_iter().take(MAX_SAMPLES_PER_PRODUCE) {
            if self.cache.contains(number) {
                sendable.push(number);
            } else if number <= self.last_change {
                missing.push(number);
            }
        }

        let mut outbound = Vec::new();
        outbound.extend(self.datagrams_for(&sendable, reader_id, reader_prefix, &locators)?);
        if reliable && !missing.is_empty() {
            outbound.push(self.gap_for(&missing, reader_id, reader_prefix, &locators)?);
        }

        if let Some(proxy) = self.matched.get_mut(&reader) {
            for number in &sendable {
                proxy.record_sent(*number);
            }
            for number in &missing {
                proxy.forget_requested(*number);
            }
            if !reliable {
                // Nobody will ever acknowledge, so the watermark is whatever
                // has been sent. Without this a KEEP_ALL best-effort writer
                // would never release a sample.
                proxy.assume_acked_through(proxy.highest_sent());
            }
        }
        let _ = now;
        Ok(outbound)
    }

    /// Pack the given samples into datagrams for one reader.
    fn datagrams_for(
        &self,
        numbers: &[SequenceNumber],
        reader_id: EntityId,
        reader_prefix: crate::structure::GuidPrefix,
        locators: &[Locator],
    ) -> BehaviorResult<Vec<Outbound>> {
        let mut outbound = Vec::new();
        let mut builder = self.new_builder(reader_prefix);
        let mut last_timestamp: Option<Time> = None;

        for number in numbers {
            let Some(change) = self.cache.get(*number) else {
                continue;
            };
            if change.len() > self.config.effective_threshold() {
                // Flush whatever is queued: fragments own their datagrams.
                if let Some(message) = Self::flush(&mut builder, self.header(), reader_prefix) {
                    outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
                }
                last_timestamp = None;
                outbound.extend(self.fragment_datagrams(
                    change,
                    reader_id,
                    reader_prefix,
                    locators,
                )?);
                continue;
            }

            if change.source_timestamp != last_timestamp
                && let Some(timestamp) = change.source_timestamp
            {
                if !builder.try_push(InfoTimestamp::at(timestamp)) {
                    if let Some(message) = Self::flush(&mut builder, self.header(), reader_prefix) {
                        outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
                    }
                    builder.push(InfoTimestamp::at(timestamp));
                }
                last_timestamp = Some(timestamp);
            }

            let data = self.data_for(change, reader_id);
            // §8.3.3: every submessage header starts on a four-octet
            // boundary, so a body whose length is not a multiple of four can
            // only be the *last* submessage of a datagram. A CDR payload is
            // padded and never trips this, but a caller writing raw octets
            // may, and the writer must not turn that into an encode failure.
            let must_end_the_datagram = !data.body_len().is_multiple_of(4);

            if !builder.try_push(data.clone()) {
                if let Some(message) = Self::flush(&mut builder, self.header(), reader_prefix) {
                    outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
                }
                last_timestamp = None;
                if let Some(timestamp) = change.source_timestamp {
                    builder.push(InfoTimestamp::at(timestamp));
                    last_timestamp = Some(timestamp);
                }
                builder.push(data);
            }

            if must_end_the_datagram
                && let Some(message) = Self::flush(&mut builder, self.header(), reader_prefix)
            {
                outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
                last_timestamp = None;
            }
        }

        if let Some(message) = Self::flush(&mut builder, self.header(), reader_prefix) {
            outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
        }
        Ok(outbound)
    }

    /// One `DATA_FRAG` series, one datagram each.
    fn fragment_datagrams(
        &self,
        change: &CacheChange,
        reader_id: EntityId,
        reader_prefix: crate::structure::GuidPrefix,
        locators: &[Locator],
    ) -> BehaviorResult<Vec<Outbound>> {
        let fragments = fragment_sample(
            reader_id,
            self.config.guid.entity_id,
            change.sequence_number,
            &change.payload,
            self.config.effective_fragment_size(),
            self.config.effective_budget(),
        )?;
        let mut outbound = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            let mut message = Message::new(self.header());
            message.push(InfoDestination::new(reader_prefix));
            if let Some(timestamp) = change.source_timestamp {
                message.push(InfoTimestamp::at(timestamp));
            }
            message.push(fragment);
            outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
        }
        Ok(outbound)
    }

    /// The `GAP` that tells a reader the listed numbers are never coming.
    ///
    /// The set must name **exactly** the numbers that are gone. A `GAP` is
    /// binding: every number it covers is irrelevant from that moment on, and
    /// a reader will never ask for one again. Widening a scattered list into
    /// one contiguous run — the obvious simplification — would declare the
    /// samples *between* the holes irrelevant too, and on a reliable channel
    /// that is silent data loss.
    ///
    /// §8.3.7.4 gives exactly the shape needed: the irrelevant set is
    /// `[gapStart, gapList.bitmapBase)` together with the bits set in
    /// `gapList`. So `gapStart` is the first missing number, `gapList` is
    /// based one past it, and the rest of the list is bits.
    ///
    /// Numbers more than [`MAX_SET_BITS`](crate::structure::MAX_SET_BITS)
    /// past the base do not fit the bitmap and are left for the next
    /// `produce`: once this GAP lands the reader's watermark moves, its next
    /// ACKNACK names a higher base, and the remainder is covered then.
    fn gap_for(
        &self,
        numbers: &[SequenceNumber],
        reader_id: EntityId,
        reader_prefix: crate::structure::GuidPrefix,
        locators: &[Locator],
    ) -> BehaviorResult<Outbound> {
        let start = numbers.first().copied().unwrap_or(SequenceNumber::FIRST);
        let base = start.next();
        let mut gap_list = SequenceNumberSet::new(base);
        for number in numbers.iter().skip(1) {
            if number.value().saturating_sub(base.value()) >= i64::from(MAX_SET_BITS) {
                break;
            }
            gap_list.insert(*number)?;
        }
        let gap = Gap::new(reader_id, self.config.guid.entity_id, start, gap_list);
        gap.validate()?;
        let mut message = Message::new(self.header());
        message.push(InfoDestination::new(reader_prefix));
        message.push(gap);
        Ok(Outbound::new(locators.to_vec(), message.encode()?))
    }

    /// A HEARTBEAT to every matched reliable reader.
    fn produce_heartbeat(
        &mut self,
        now: Instant,
        liveliness: bool,
    ) -> BehaviorResult<Vec<Outbound>> {
        if !self.is_reliable() {
            return Ok(Vec::new());
        }
        self.heartbeat_count = self.heartbeat_count.saturating_add(1);
        self.last_heartbeat_at = Some(now);

        let first = self.first_available();
        let last = self.last_change;
        let mut outbound = Vec::new();
        for proxy in self.matched.values() {
            if !proxy.is_reliable() || !proxy.is_active() {
                continue;
            }
            let locators = proxy.locators();
            if locators.is_empty() {
                continue;
            }
            let mut heartbeat = Heartbeat::new(
                proxy.guid().entity_id,
                self.config.guid.entity_id,
                first,
                last,
                self.heartbeat_count,
            );
            if liveliness {
                heartbeat = heartbeat.asserting_liveliness().finalized();
            } else if proxy.acked_through() >= last {
                heartbeat = heartbeat.finalized();
            }
            let mut message = Message::new(self.header());
            message.push(InfoDestination::new(proxy.guid().prefix));
            message.push(heartbeat);
            outbound.push(Outbound::new(locators, message.encode()?));
        }
        Ok(outbound)
    }

    /// The `DATA` submessage for one cache change.
    ///
    /// A change that is not `Alive` goes out as a **key**, not a sample: the
    /// `K` flag says "these octets identify the instance, they are not its
    /// value", and `PID_STATUS_INFO` in the inline QoS says whether the
    /// instance was disposed of or merely unregistered (§9.6.3.9). Both are
    /// needed — the flag alone cannot tell the two apart, and a receiver that
    /// reads only the flag would treat an unregistration as a disposal.
    fn data_for<'a>(&self, change: &'a CacheChange, reader_id: EntityId) -> Data<'a> {
        let payload = if change.kind.carries_data() {
            DataPayload::Data(SerializedPayload::new(change.payload.as_slice()))
        } else if change.payload.is_empty() {
            DataPayload::None
        } else {
            DataPayload::Key(SerializedPayload::new(change.payload.as_slice()))
        };
        let data = Data::new(
            reader_id,
            self.config.guid.entity_id,
            change.sequence_number,
            payload,
        );
        if change.kind.carries_data() {
            return data;
        }
        match status_info_qos(change.kind) {
            None => data,
            Some(list) => data.with_inline_qos(list),
        }
    }

    /// A builder primed with the destination interpreter submessage.
    fn new_builder(&self, reader_prefix: crate::structure::GuidPrefix) -> MessageBuilder<'_> {
        let mut builder =
            MessageBuilder::with_budget(self.header(), self.config.effective_budget());
        builder.push(InfoDestination::new(reader_prefix));
        builder
    }

    /// Take the builder's message when it holds more than the destination
    /// preamble, and start a fresh one.
    fn flush<'a>(
        builder: &mut MessageBuilder<'a>,
        header: Header,
        reader_prefix: crate::structure::GuidPrefix,
    ) -> Option<Message<'a>> {
        if builder.len() <= 1 {
            return None;
        }
        let mut fresh = MessageBuilder::with_budget(header, builder.remaining() + builder.used());
        fresh.push(InfoDestination::new(reader_prefix));
        let finished = core::mem::replace(builder, fresh);
        Some(finished.build())
    }

    /// The fragmentation plan this writer would use for a sample of `len`
    /// octets, or `None` when it would not fragment.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::SampleTooLarge`].
    pub fn fragment_plan(&self, len: usize) -> BehaviorResult<Option<FragmentPlan>> {
        if len <= self.config.effective_threshold() {
            return Ok(None);
        }
        Ok(Some(FragmentPlan::new(len, self.config.fragment_size)?))
    }
}

/// The inline QoS a disposal or unregistration carries.
///
/// One parameter: `PID_STATUS_INFO`, four octets, the last of which holds the
/// disposed and unregistered bits. `None` for a live sample, which needs no
/// status at all.
fn status_info_qos(kind: ChangeKind) -> Option<ParameterList<'static>> {
    if kind.carries_data() {
        return None;
    }
    let mut list = ParameterList::new(inline_qos_encoding(Endianness::Little));
    list.push_octets(
        ParameterId::new(astrs_cdr::pid::STATUS_INFO),
        kind.status_info().to_vec(),
    )
    .ok()?;
    Some(list)
}

/// Everything a writer emits, flattened for a caller that only wants the
/// submessages.
///
/// Used by tests and by `astrs-ros2`'s introspection; the participant sends
/// [`Outbound`] values directly.
///
/// # Errors
///
/// [`BehaviorError::Wire`] when a datagram will not decode, which cannot
/// happen for octets this crate produced.
pub fn decode_outbound(outbound: &Outbound) -> BehaviorResult<Vec<Submessage<'_>>> {
    let message = Message::decode(&outbound.datagram)?;
    Ok(message.iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::discovery::qos::{HistoryQos, LifespanQos, ReliabilityQos};
    use crate::structure::{EntityKind, GuidPrefix, VendorId};
    use std::net::Ipv4Addr;

    fn writer_guid() -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [1; 10]),
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    fn reader_guid() -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [2; 10]),
            EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
        )
    }

    fn topic() -> TopicKey {
        TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_").expect("valid names")
    }

    fn reliable_writer() -> RtpsWriter {
        RtpsWriter::new(
            WriterConfig::new(writer_guid(), topic())
                .with_qos(WriterQos::services_default())
                .with_datagram_budget(1_400),
        )
    }

    fn best_effort_writer() -> RtpsWriter {
        RtpsWriter::new(
            WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::sensor_data()),
        )
    }

    fn proxy(reliable: bool) -> ReaderProxy {
        ReaderProxy::new(
            reader_guid(),
            vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 45_001)],
            reliable,
        )
    }

    fn submessages(outbound: &[Outbound]) -> Vec<Submessage<'_>> {
        outbound
            .iter()
            .flat_map(|item| decode_outbound(item).expect("decode"))
            .collect()
    }

    fn count_data(outbound: &[Outbound]) -> usize {
        submessages(outbound)
            .iter()
            .filter(|submessage| submessage.as_data().is_some())
            .count()
    }

    #[test]
    fn a_writer_with_no_readers_emits_nothing() {
        let mut writer = reliable_writer();
        let now = Instant::now();
        writer.write(vec![1, 2, 3, 4], None, now).unwrap();
        assert!(writer.produce(now).unwrap().is_empty());
        assert_eq!(writer.last_change(), SequenceNumber::FIRST);
    }

    #[test]
    fn sequence_numbers_start_at_one_and_increase() {
        let mut writer = reliable_writer();
        let now = Instant::now();
        assert_eq!(
            writer.write(vec![1], None, now).unwrap(),
            SequenceNumber::new(1)
        );
        assert_eq!(
            writer.write(vec![2], None, now).unwrap(),
            SequenceNumber::new(2)
        );
        assert_eq!(writer.last_change(), SequenceNumber::new(2));
    }

    #[test]
    fn an_empty_history_reports_a_first_sn_above_its_last_sn() {
        let writer = reliable_writer();
        assert_eq!(writer.first_available(), SequenceNumber::FIRST);
        assert_eq!(writer.last_change(), SequenceNumber::ZERO);
        assert!(
            writer.first_available() > writer.last_change(),
            "the standard \"I hold nothing\" heartbeat"
        );
    }

    #[test]
    fn a_matched_reader_receives_every_sample_once() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        for value in 1..=3_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        let outbound = writer.produce(now).unwrap();
        assert_eq!(count_data(&outbound), 3);

        // A second produce sends nothing new.
        let again = writer.produce(now).unwrap();
        assert_eq!(count_data(&again), 0);
    }

    #[test]
    fn every_datagram_names_its_destination() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer.write(vec![7], None, now).unwrap();
        let outbound = writer.produce(now).unwrap();
        let found = submessages(&outbound);
        assert!(
            found
                .iter()
                .any(|submessage| matches!(submessage, Submessage::InfoDestination(_))),
            "a unicast datagram must carry INFO_DST"
        );
    }

    #[test]
    fn a_source_timestamp_becomes_an_info_ts() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer
            .write(vec![1], Some(Time::new(1_700_000_000, 0)), now)
            .unwrap();
        let outbound = writer.produce(now).unwrap();
        let found = submessages(&outbound);
        assert!(
            found
                .iter()
                .any(|submessage| matches!(submessage, Submessage::InfoTimestamp(_)))
        );
    }

    #[test]
    fn a_reliable_writer_heartbeats_and_a_best_effort_one_does_not() {
        let now = Instant::now();

        let mut reliable = reliable_writer();
        reliable.match_reader(proxy(true));
        reliable.write(vec![1], None, now).unwrap();
        let outbound = reliable.produce(now).unwrap();
        assert!(
            submessages(&outbound)
                .iter()
                .any(|submessage| matches!(submessage, Submessage::Heartbeat(_)))
        );

        let mut best_effort = best_effort_writer();
        best_effort.match_reader(proxy(false));
        best_effort.write(vec![1], None, now).unwrap();
        let outbound = best_effort.produce(now).unwrap();
        assert!(
            !submessages(&outbound)
                .iter()
                .any(|submessage| matches!(submessage, Submessage::Heartbeat(_))),
            "a best-effort writer has no reliability protocol to run"
        );
        assert_eq!(count_data(&outbound), 1, "but it does send the sample");
    }

    #[test]
    fn the_heartbeat_cadence_is_respected() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let start = Instant::now();
        writer.write(vec![1], None, start).unwrap();

        let first = writer.produce(start).unwrap();
        let heartbeats = submessages(&first)
            .iter()
            .filter(|submessage| matches!(submessage, Submessage::Heartbeat(_)))
            .count();
        assert_eq!(heartbeats, 1);

        let soon = start + StdDuration::from_millis(10);
        let second = writer.produce(soon).unwrap();
        assert!(
            !submessages(&second)
                .iter()
                .any(|submessage| matches!(submessage, Submessage::Heartbeat(_))),
            "the period has not elapsed"
        );

        let later = start + DEFAULT_HEARTBEAT_PERIOD + StdDuration::from_millis(1);
        let third = writer.produce(later).unwrap();
        assert!(
            submessages(&third)
                .iter()
                .any(|submessage| matches!(submessage, Submessage::Heartbeat(_)))
        );
    }

    #[test]
    fn the_heartbeat_announces_the_real_window() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        for value in 1..=4_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        let outbound = writer.force_heartbeat(now).unwrap();
        let found = submessages(&outbound);
        let heartbeat = found
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::Heartbeat(heartbeat) => Some(heartbeat),
                _ => None,
            })
            .expect("a heartbeat");
        assert_eq!(heartbeat.first_sn, SequenceNumber::new(1));
        assert_eq!(heartbeat.last_sn, SequenceNumber::new(4));
        assert!(!heartbeat.is_final, "the reader has acknowledged nothing");
    }

    #[test]
    fn a_caught_up_reader_gets_a_final_heartbeat() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer.write(vec![1], None, now).unwrap();
        writer.produce(now).unwrap();

        let acknack = crate::messages::AckNack::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumberSet::new(SequenceNumber::new(2)),
            1,
        );
        assert!(writer.on_acknack(reader_guid(), &acknack));
        assert!(writer.is_acknowledged());

        let outbound = writer.force_heartbeat(now).unwrap();
        let found = submessages(&outbound);
        let heartbeat = found
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::Heartbeat(heartbeat) => Some(heartbeat),
                _ => None,
            })
            .expect("a heartbeat");
        assert!(heartbeat.is_final);
    }

    #[test]
    fn a_nacked_sample_is_retransmitted() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        for value in 1..=3_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        writer.produce(now).unwrap();
        assert_eq!(count_data(&writer.produce(now).unwrap()), 0);

        // "I have 1; 2 and 3 are missing."
        let set = SequenceNumberSet::from_numbers(
            SequenceNumber::new(2),
            [SequenceNumber::new(2), SequenceNumber::new(3)],
        )
        .unwrap();
        let acknack =
            crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, 1);
        assert!(writer.on_acknack(reader_guid(), &acknack));

        let repairs = writer.produce(now).unwrap();
        assert_eq!(count_data(&repairs), 2, "both missing samples come back");
    }

    #[test]
    fn a_stale_acknack_is_ignored() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer.write(vec![1], None, now).unwrap();
        writer.produce(now).unwrap();

        let ack = crate::messages::AckNack::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumberSet::new(SequenceNumber::new(2)),
            5,
        );
        assert!(writer.on_acknack(reader_guid(), &ack));
        let stale = crate::messages::AckNack::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumberSet::from_numbers(SequenceNumber::FIRST, [SequenceNumber::FIRST])
                .unwrap(),
            5,
        );
        assert!(!writer.on_acknack(reader_guid(), &stale));
        assert_eq!(count_data(&writer.produce(now).unwrap()), 0);
    }

    #[test]
    fn an_acknack_from_an_unmatched_reader_is_ignored() {
        let mut writer = reliable_writer();
        let acknack = crate::messages::AckNack::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumberSet::new(SequenceNumber::FIRST),
            1,
        );
        assert!(!writer.on_acknack(reader_guid(), &acknack));
    }

    #[test]
    fn an_evicted_sample_becomes_a_gap() {
        let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
            WriterQos {
                reliability: ReliabilityQos::reliable(),
                history: HistoryQos::keep_last(2),
                ..WriterQos::default()
            },
        ));
        writer.match_reader(proxy(true));
        let now = Instant::now();
        for value in 1..=4_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        // The reader asks for sample 1, which KEEP_LAST 2 has evicted.
        let set = SequenceNumberSet::from_numbers(SequenceNumber::FIRST, [SequenceNumber::FIRST])
            .unwrap();
        let acknack =
            crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, 1);
        writer.on_acknack(reader_guid(), &acknack);

        let outbound = writer.produce(now).unwrap();
        let found = submessages(&outbound);
        let gap = found
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::Gap(gap) => Some(gap),
                _ => None,
            })
            .expect("a GAP for the evicted sample");
        assert!(gap.covers(SequenceNumber::FIRST));
    }

    #[test]
    fn an_expired_sample_is_dropped_and_reported() {
        let origin = Instant::now();
        let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
            WriterQos {
                history: HistoryQos::keep_all(),
                lifespan: LifespanQos::from_millis(50),
                ..WriterQos::default()
            },
        ));
        writer.write(vec![1], None, origin).unwrap();
        writer
            .write(vec![2], None, origin + StdDuration::from_millis(40))
            .unwrap();

        let expired = writer.expire(origin + StdDuration::from_millis(60));
        assert_eq!(expired, vec![SequenceNumber::FIRST]);
        assert_eq!(writer.cache().len(), 1);
    }

    #[test]
    fn keep_all_reclaims_what_every_reader_acknowledged() {
        let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
            WriterQos {
                history: HistoryQos::keep_all(),
                ..WriterQos::default()
            },
        ));
        writer.match_reader(proxy(true));
        let now = Instant::now();
        for value in 1..=5_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        assert_eq!(writer.reclaim(), 0, "nothing acknowledged yet");

        let acknack = crate::messages::AckNack::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumberSet::new(SequenceNumber::new(4)),
            1,
        );
        writer.on_acknack(reader_guid(), &acknack);
        assert_eq!(writer.acked_by_all(), Some(SequenceNumber::new(3)));
        assert_eq!(writer.reclaim(), 3);
        assert_eq!(
            writer.cache().min_sequence_number(),
            Some(SequenceNumber::new(4))
        );
    }

    #[test]
    fn keep_last_never_reclaims() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer.write(vec![1], None, now).unwrap();
        let acknack = crate::messages::AckNack::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumberSet::new(SequenceNumber::new(2)),
            1,
        );
        writer.on_acknack(reader_guid(), &acknack);
        assert_eq!(
            writer.reclaim(),
            0,
            "a KEEP_LAST cache must keep its history for the next late joiner"
        );
    }

    #[test]
    fn a_large_sample_is_fragmented() {
        let mut writer = RtpsWriter::new(
            WriterConfig::new(writer_guid(), topic())
                .with_qos(WriterQos::services_default())
                .with_fragmentation(1_000, 500)
                .with_datagram_budget(1_400),
        );
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer.write(vec![0xab_u8; 4_000], None, now).unwrap();

        let outbound = writer.produce(now).unwrap();
        let found = submessages(&outbound);
        let fragments: Vec<_> = found
            .iter()
            .filter_map(|submessage| submessage.as_data_frag())
            .collect();
        let carried: u32 = fragments
            .iter()
            .map(|fragment| u32::from(fragment.fragments_in_submessage))
            .sum();
        assert_eq!(carried, 8, "4000 octets at 500 per fragment");
        assert_eq!(
            fragments.len(),
            4,
            "a 1400-octet datagram holds two 500-octet fragments"
        );
        assert_eq!(fragments[0].sample_size, 4_000);
        assert_eq!(fragments[0].fragment_size, 500);
        assert!(
            found
                .iter()
                .all(|submessage| submessage.as_data().is_none()),
            "a fragmented sample never also goes as a plain DATA"
        );
    }

    #[test]
    fn a_small_sample_is_not_fragmented() {
        let writer = reliable_writer();
        assert!(writer.fragment_plan(100).unwrap().is_none());
        let plan = writer
            .fragment_plan(FRAGMENTATION_THRESHOLD + 1)
            .unwrap()
            .expect("above the threshold");
        assert!(plan.is_needed());
    }

    #[test]
    fn the_datagram_budget_lowers_the_threshold_below_the_policy() {
        // The policy says 64 KiB; a 1400-octet datagram says otherwise, and
        // the physical limit wins.
        let tight = WriterConfig::new(writer_guid(), topic()).with_datagram_budget(1_400);
        assert_eq!(tight.fragmentation_threshold, FRAGMENTATION_THRESHOLD);
        assert_eq!(tight.effective_threshold(), 1_400 - DATAGRAM_OVERHEAD);

        let writer = RtpsWriter::new(tight);
        assert!(writer.fragment_plan(1_000).unwrap().is_none());
        assert!(writer.fragment_plan(2_000).unwrap().is_some());

        // With room to spare the policy is what binds.
        let roomy = WriterConfig::new(writer_guid(), topic())
            .with_datagram_budget(crate::messages::MAX_UDP_PAYLOAD)
            .with_fragmentation(1_000, 500);
        assert_eq!(roomy.effective_threshold(), 1_000);
    }

    #[test]
    fn matching_the_same_reader_twice_keeps_its_state() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer.write(vec![1], None, now).unwrap();
        writer.produce(now).unwrap();

        writer.match_reader(proxy(true));
        assert_eq!(writer.matched_reader_count(), 1);
        assert_eq!(
            count_data(&writer.produce(now).unwrap()),
            0,
            "re-matching must not resend everything"
        );
    }

    #[test]
    fn unmatching_stops_delivery() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        assert!(writer.is_matched(reader_guid()));
        assert!(writer.unmatch_reader(reader_guid()));
        assert!(!writer.unmatch_reader(reader_guid()));

        let now = Instant::now();
        writer.write(vec![1], None, now).unwrap();
        assert!(writer.produce(now).unwrap().is_empty());
    }

    #[test]
    fn unmatching_a_participant_removes_all_of_its_readers() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        writer.match_reader(ReaderProxy::new(
            Guid::new(
                reader_guid().prefix,
                EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY),
            ),
            vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 45_002)],
            true,
        ));
        assert_eq!(writer.matched_reader_count(), 2);
        assert_eq!(writer.unmatch_participant(reader_guid()), 2);
        assert_eq!(writer.matched_reader_count(), 0);
    }

    #[test]
    fn a_reader_with_no_locators_is_skipped_rather_than_failing() {
        let mut writer = reliable_writer();
        writer.match_reader(ReaderProxy::new(reader_guid(), Vec::new(), true));
        let now = Instant::now();
        writer.write(vec![1], None, now).unwrap();
        assert!(writer.produce(now).unwrap().is_empty());
    }

    #[test]
    fn a_deactivated_reader_receives_nothing() {
        let mut writer = reliable_writer();
        let mut idle = proxy(true);
        idle.deactivate();
        writer.match_reader(idle);
        let now = Instant::now();
        writer.write(vec![1], None, now).unwrap();
        // `match_reader` on a fresh GUID stores the proxy as given.
        assert_eq!(count_data(&writer.produce(now).unwrap()), 0);
    }

    #[test]
    fn a_best_effort_writer_releases_its_keep_all_history() {
        let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
            WriterQos {
                reliability: ReliabilityQos::best_effort(),
                history: HistoryQos::keep_all(),
                ..WriterQos::default()
            },
        ));
        writer.match_reader(proxy(false));
        let now = Instant::now();
        for value in 1..=3_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        writer.produce(now).unwrap();
        assert_eq!(
            writer.acked_by_all(),
            None,
            "there is no reliable reader to wait for"
        );
    }

    #[test]
    fn an_announcement_addresses_arbitrary_locators() {
        let mut writer = reliable_writer();
        let now = Instant::now();
        let number = writer.write(vec![1, 2, 3, 4], None, now).unwrap();
        let outbound = writer
            .announce(
                number,
                crate::structure::ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
                vec![Locator::udpv4(Ipv4Addr::new(239, 255, 0, 1), 7_400)],
            )
            .unwrap()
            .expect("the sample is held");
        assert!(outbound.is_deliverable());
        let found = decode_outbound(&outbound).unwrap();
        let data = found.iter().find_map(Submessage::as_data).expect("a DATA");
        assert_eq!(
            data.reader_id,
            crate::structure::ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER
        );
        assert_eq!(data.writer_sn, number);
    }

    #[test]
    fn announcing_a_sample_that_is_gone_yields_nothing() {
        let writer = reliable_writer();
        assert!(
            writer
                .announce(
                    SequenceNumber::new(99),
                    EntityId::UNKNOWN,
                    vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 1)]
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn announcing_to_nowhere_yields_nothing() {
        let mut writer = reliable_writer();
        let now = Instant::now();
        let number = writer.write(vec![1], None, now).unwrap();
        assert!(
            writer
                .announce(number, EntityId::UNKNOWN, Vec::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_liveliness_assertion_is_a_final_heartbeat_with_the_l_flag() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        let outbound = writer.assert_liveliness(now).unwrap();
        let found = submessages(&outbound);
        let heartbeat = found
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::Heartbeat(heartbeat) => Some(heartbeat),
                _ => None,
            })
            .expect("a heartbeat");
        assert!(heartbeat.liveliness);
        assert!(heartbeat.is_final);
    }

    #[test]
    fn a_nack_frag_requests_the_whole_sample_again() {
        let mut writer = RtpsWriter::new(
            WriterConfig::new(writer_guid(), topic())
                .with_qos(WriterQos::services_default())
                .with_fragmentation(1_000, 500)
                .with_datagram_budget(1_400),
        );
        writer.match_reader(proxy(true));
        let now = Instant::now();
        let number = writer.write(vec![0xcd_u8; 4_000], None, now).unwrap();
        writer.produce(now).unwrap();
        assert!(writer.produce(now).unwrap().is_empty());

        let missing = crate::structure::FragmentNumberSet::from_numbers(
            crate::structure::FragmentNumber::new(3),
            [crate::structure::FragmentNumber::new(3)],
        )
        .unwrap();
        let nack = NackFrag::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            number,
            missing,
            1,
        );
        assert!(writer.on_nack_frag(reader_guid(), &nack));

        let repairs = writer.produce(now).unwrap();
        let carried: u32 = submessages(&repairs)
            .iter()
            .filter_map(|submessage| submessage.as_data_frag())
            .map(|fragment| u32::from(fragment.fragments_in_submessage))
            .sum();
        assert_eq!(carried, 8, "the whole sample is resent");
    }

    #[test]
    fn a_nack_frag_for_a_sample_that_is_gone_is_ignored() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let missing =
            crate::structure::FragmentNumberSet::new(crate::structure::FragmentNumber::FIRST);
        let nack = NackFrag::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumber::new(42),
            missing,
            1,
        );
        assert!(!writer.on_nack_frag(reader_guid(), &nack));
    }

    #[test]
    fn every_datagram_stays_within_the_budget() {
        let mut writer = RtpsWriter::new(
            WriterConfig::new(writer_guid(), topic())
                .with_qos(WriterQos {
                    history: HistoryQos::keep_all(),
                    ..WriterQos::services_default()
                })
                .with_datagram_budget(600),
        );
        writer.match_reader(proxy(true));
        let now = Instant::now();
        for _ in 0..20 {
            writer.write(vec![0x5a_u8; 100], None, now).unwrap();
        }
        let outbound = writer.produce(now).unwrap();
        assert!(
            outbound.len() > 1,
            "twenty samples cannot fit in one datagram"
        );
        for item in &outbound {
            assert!(
                item.len() <= 600,
                "a datagram of {} octets exceeds the budget",
                item.len()
            );
        }
        assert_eq!(count_data(&outbound), 20, "and every sample still went");
    }

    #[test]
    fn a_volatile_writer_gives_a_late_joiner_nothing() {
        let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
            WriterQos {
                reliability: ReliabilityQos::reliable(),
                durability: crate::discovery::qos::DurabilityQos::volatile(),
                history: HistoryQos::keep_last(10),
                ..WriterQos::default()
            },
        ));
        let now = Instant::now();
        for value in 1..=3_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        // The reader arrives after the fact.
        writer.match_reader(proxy(true));
        assert_eq!(
            count_data(&writer.produce(now).unwrap()),
            0,
            "VOLATILE means the late joiner missed them"
        );
        // …but everything written from now on does arrive.
        writer.write(vec![4; 4], None, now).unwrap();
        assert_eq!(count_data(&writer.produce(now).unwrap()), 1);
    }

    #[test]
    fn a_transient_local_writer_replays_its_history_to_a_late_joiner() {
        let mut writer = RtpsWriter::new(
            WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::latched(10)),
        );
        let now = Instant::now();
        for value in 1..=3_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        writer.match_reader(proxy(true));
        assert_eq!(
            count_data(&writer.produce(now).unwrap()),
            3,
            "TRANSIENT_LOCAL replays everything the history still holds"
        );
    }

    #[test]
    fn a_scattered_gap_names_only_the_numbers_that_are_gone() {
        let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
            WriterQos {
                reliability: ReliabilityQos::reliable(),
                history: HistoryQos::keep_all(),
                ..WriterQos::default()
            },
        ));
        writer.match_reader(proxy(true));
        let now = Instant::now();
        for value in 1..=9_u8 {
            writer.write(vec![value; 4], None, now).unwrap();
        }
        // Drop 3 and 9 from the history, keeping 4..=8.
        assert!(writer.forget(SequenceNumber::new(3)));
        assert!(writer.forget(SequenceNumber::new(9)));

        // The reader has 1 and 2 and asks for everything from 3.
        let set = SequenceNumberSet::from_numbers(
            SequenceNumber::new(3),
            (3..=9).map(SequenceNumber::new),
        )
        .unwrap();
        let acknack =
            crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, 1);
        writer.on_acknack(reader_guid(), &acknack);

        let outbound = writer.produce(now).unwrap();
        let found = submessages(&outbound);
        let gap = found
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::Gap(gap) => Some(gap),
                _ => None,
            })
            .expect("a GAP");
        let irrelevant: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
        assert_eq!(
            irrelevant,
            vec![3, 9],
            "the samples between the holes are still held and must not be gapped"
        );
        assert!(!gap.covers(SequenceNumber::new(5)));
        assert_eq!(count_data(&outbound), 5, "4..=8 are still there and go out");
    }

    #[test]
    fn a_disposal_goes_out_as_a_key_with_a_status_info() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        let gone = Guid::new(
            reader_guid().prefix,
            EntityId::user_defined(9, EntityKind::USER_READER_NO_KEY),
        );
        writer.dispose(gone, now).expect("dispose");

        let outbound = writer.produce(now).unwrap();
        let found = submessages(&outbound);
        let data = found
            .iter()
            .find_map(Submessage::as_data)
            .expect("a DATA carrying the disposal");

        assert!(
            matches!(data.payload, DataPayload::Key(_)),
            "a disposal names the instance, it does not carry a value"
        );
        assert!(data.flags().has(crate::messages::flags::KEY));
        let qos = data.inline_qos.as_ref().expect("PID_STATUS_INFO");
        let status = qos
            .get_by_base(astrs_cdr::pid::STATUS_INFO)
            .expect("the status parameter");
        assert_eq!(
            ChangeKind::from_status_info([
                status.value[0],
                status.value[1],
                status.value[2],
                status.value[3],
            ]),
            ChangeKind::NotAliveDisposed
        );

        // …and the key octets are the GUID, at the offset a receiver reads.
        let key = data.payload.payload().expect("key octets").as_slice();
        assert_eq!(
            &key[astrs_cdr::ENCAPSULATION_HEADER_LEN..],
            &gone.to_bytes()
        );
    }

    #[test]
    fn a_live_sample_carries_no_status_info() {
        let mut writer = reliable_writer();
        writer.match_reader(proxy(true));
        let now = Instant::now();
        writer.write(vec![1, 2, 3, 4], None, now).unwrap();
        let outbound = writer.produce(now).unwrap();
        let found = submessages(&outbound);
        let data = found.iter().find_map(Submessage::as_data).expect("a DATA");
        assert!(data.inline_qos.is_none());
    }

    #[test]
    fn the_writer_reports_its_own_configuration() {
        let writer = reliable_writer();
        assert_eq!(writer.guid(), writer_guid());
        assert_eq!(writer.topic().topic_name, "rt/chatter");
        assert!(writer.qos().is_reliable());
        assert!(writer.is_reliable());
        assert_eq!(writer.config().datagram_budget, 1_400);
        assert_eq!(writer.matched_readers().count(), 0);
    }
}
