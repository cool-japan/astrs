//! The RTPS reader: one implementation, two behaviours.
//!
//! As with [`writer`](crate::behavior::writer), §8.4.8's stateless reader and
//! §8.4.10's stateful reader are the same machine with the reliability half
//! switched on or off. Which one runs is decided by
//! [`ReaderQos::reliability`], per matched writer, so one reader can be
//! reliable towards a reliable writer and best-effort towards a best-effort
//! one without the participant having to keep two objects.
//!
//! # The acceptance rule differs by reliability
//!
//! This is the difference that matters, and it is the one implementations get
//! wrong:
//!
//! - **Best-effort** (§8.4.10.4): accept a sample only when its sequence
//!   number is *greater* than the highest already accepted. An out-of-order
//!   arrival is dropped, not reordered — there is no repair protocol to make
//!   the gap go away, so holding sample 7 while waiting for 5 would stall
//!   forever.
//! - **Reliable** (§8.4.10.5): accept any sequence number not already held.
//!   Out-of-order arrivals wait above the watermark, and the ACKNACK asks for
//!   the hole.
//!
//! [`RtpsReader::on_data`] applies whichever rule the matched writer's proxy
//! says, and the two paths are tested against each other.
//!
//! # No sockets, no clock
//!
//! Every method is synchronous; the ones that emit take `now`.
//! [`RtpsReader::produce`] returns [`Outbound`] values the participant sends.
//!
//! # History
//!
//! Accepted samples queue up until the application takes them, bounded by the
//! reader's own `HISTORY` policy — `KEEP_LAST n` drops the oldest, `KEEP_ALL`
//! is bounded only by `RESOURCE_LIMITS`. A dropped sample is *still*
//! acknowledged: it arrived, the protocol delivered it, and the application
//! chose a depth that could not hold it. Nacking it would ask the writer to
//! send something that would be dropped again.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration as StdDuration, Instant};

use crate::behavior::cache::{ChangeKind, InstanceHandle};
use crate::behavior::endpoint::{Outbound, Sample, TopicKey};
use crate::behavior::error::BehaviorResult;
use crate::behavior::fragment::Reassembler;
use crate::behavior::proxy::WriterProxy;
use crate::discovery::matching::ReaderQos;
use crate::messages::{
    Data, DataFrag, Gap, Header, Heartbeat, InfoDestination, Message, NackFrag, SerializedPayload,
};
use crate::security::EndpointSecurity;
use crate::structure::{Guid, GuidPrefix, SequenceNumber, Time};

/// How long a reliable reader waits before answering a HEARTBEAT.
///
/// Zero, deliberately: the delay exists to stop many readers answering one
/// multicast HEARTBEAT in the same microsecond, and the deterministic
/// loopback path has one reader. A caller that needs it can set it.
pub const DEFAULT_HEARTBEAT_RESPONSE_DELAY: StdDuration = StdDuration::ZERO;

/// Samples a reader will hold when `KEEP_ALL` sets no other bound.
///
/// A reader whose application has stopped taking must not grow without limit;
/// at this point the oldest are dropped, which is the same thing DDS's
/// `RESOURCE_LIMITS` would do.
pub const MAX_UNTAKEN_SAMPLES: usize = 8_192;

/// How a reader is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderConfig {
    /// The reader's GUID.
    pub guid: Guid,
    /// The topic and type it subscribes to.
    pub topic: TopicKey,
    /// The QoS it requests.
    pub qos: ReaderQos,
    /// How long to wait before answering a HEARTBEAT.
    pub heartbeat_response_delay: StdDuration,
    /// True when the reader wants inline QoS on every sample.
    pub expects_inline_qos: bool,
    /// Run as a **stateless** reader (§8.4.10.2).
    ///
    /// A stateless reader keeps no `WriterProxy` state that governs
    /// acceptance. Two consequences, and both are needed by exactly one
    /// endpoint:
    ///
    /// 1. **It accepts from writers nobody matched.** The whole point of an
    ///    SPDP announcement is that it arrives from a participant nobody has
    ///    heard of; a matching requirement would make discovery unable to
    ///    start.
    /// 2. **It does not filter duplicates.** The SPDP writer holds one
    ///    change and *resends* it every announce period — same sequence
    ///    number, every time. A stateful reader would take the first and
    ///    discard the rest as duplicates, and the peer's lease would never be
    ///    renewed: it would be discovered once and then declared dead.
    ///
    /// False for every application reader, where both behaviours would be
    /// bugs: an unmatched sample is one whose QoS was never checked, and a
    /// duplicate delivered twice is a sample the application sees twice.
    pub stateless: bool,
    /// The pre-shared key and protection level this reader requires.
    ///
    /// [`EndpointSecurity::none`] by default. When it is set, the reader's
    /// own `ACKNACK`s and `NACK_FRAG`s are protected on the way out, and a
    /// plaintext `DATA` addressed to this reader is refused on the way in —
    /// see [`crate::security`], which is where both happen.
    pub security: EndpointSecurity,
}

impl ReaderConfig {
    /// A reader on `topic` with default QoS.
    #[must_use]
    pub fn new(guid: Guid, topic: TopicKey) -> Self {
        Self {
            guid,
            topic,
            qos: ReaderQos::default(),
            heartbeat_response_delay: DEFAULT_HEARTBEAT_RESPONSE_DELAY,
            expects_inline_qos: false,
            stateless: false,
            security: EndpointSecurity::none(),
        }
    }

    /// Replace the security settings.
    #[must_use]
    pub fn with_security(mut self, security: EndpointSecurity) -> Self {
        self.security = security;
        self
    }

    /// Run as a stateless reader.
    ///
    /// Only the SPDP participant reader should — see
    /// [`stateless`](Self::stateless).
    #[must_use]
    pub const fn stateless(mut self) -> Self {
        self.stateless = true;
        self
    }

    /// Replace the QoS.
    #[must_use]
    pub const fn with_qos(mut self, qos: ReaderQos) -> Self {
        self.qos = qos;
        self
    }

    /// Replace the heartbeat response delay.
    #[must_use]
    pub const fn with_heartbeat_response_delay(mut self, delay: StdDuration) -> Self {
        self.heartbeat_response_delay = delay;
        self
    }
}

/// What a matched writer's deadline monitor is tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineMiss {
    /// The writer that went quiet.
    pub writer: Guid,
    /// How long it has been since its last sample.
    pub since: StdDuration,
    /// The period the reader requested.
    pub period: StdDuration,
}

/// An RTPS reader: matched writers, reassembly, and the samples waiting to be
/// taken.
#[derive(Debug)]
pub struct RtpsReader {
    config: ReaderConfig,
    matched: BTreeMap<Guid, WriterProxy>,
    reassembler: Reassembler,
    ready: VecDeque<Sample>,
    last_sample_at: BTreeMap<Guid, Instant>,
    fragment_progress: BTreeMap<(Guid, SequenceNumber), u32>,
    dropped: u64,
}

impl RtpsReader {
    /// Build a reader from its configuration.
    #[must_use]
    pub fn new(config: ReaderConfig) -> Self {
        Self {
            config,
            matched: BTreeMap::new(),
            reassembler: Reassembler::new(),
            ready: VecDeque::new(),
            last_sample_at: BTreeMap::new(),
            fragment_progress: BTreeMap::new(),
            dropped: 0,
        }
    }

    /// The reader's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.config.guid
    }

    /// The topic and type it subscribes to.
    #[must_use]
    pub const fn topic(&self) -> &TopicKey {
        &self.config.topic
    }

    /// The QoS it requests.
    #[must_use]
    pub const fn qos(&self) -> &ReaderQos {
        &self.config.qos
    }

    /// The configuration it was built with.
    #[must_use]
    pub const fn config(&self) -> &ReaderConfig {
        &self.config
    }

    /// True when the reader asked for repairs.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        self.config.qos.is_reliable()
    }

    /// How many writers are matched.
    #[must_use]
    pub fn matched_writer_count(&self) -> usize {
        self.matched.len()
    }

    /// True when `guid` is a matched writer.
    #[must_use]
    pub fn is_matched(&self, guid: Guid) -> bool {
        self.matched.contains_key(&guid)
    }

    /// The matched writers.
    pub fn matched_writers(&self) -> impl Iterator<Item = &WriterProxy> {
        self.matched.values()
    }

    /// How many samples have been dropped because the history was full.
    #[must_use]
    pub const fn dropped_count(&self) -> u64 {
        self.dropped
    }

    /// Add or replace a matched writer.
    ///
    /// Idempotent: re-matching refreshes locators without disturbing
    /// reception state, which is what a repeated SEDP announcement should do.
    pub fn match_writer(&mut self, proxy: WriterProxy) {
        let guid = proxy.guid();
        match self.matched.get_mut(&guid) {
            Some(existing) => existing.set_locators(proxy.locators(), Vec::new()),
            None => {
                self.matched.insert(guid, proxy);
            }
        }
    }

    /// Remove a matched writer, and everything it had part-assembled.
    pub fn unmatch_writer(&mut self, guid: Guid) -> bool {
        self.reassembler.discard_writer(guid);
        self.last_sample_at.remove(&guid);
        self.fragment_progress.retain(|(held, _), _| *held != guid);
        self.matched.remove(&guid).is_some()
    }

    /// Remove every matched writer belonging to one participant.
    pub fn unmatch_participant(&mut self, participant: Guid) -> usize {
        let doomed: Vec<Guid> = self
            .matched
            .keys()
            .filter(|guid| guid.prefix == participant.prefix)
            .copied()
            .collect();
        for guid in &doomed {
            self.unmatch_writer(*guid);
        }
        doomed.len()
    }

    /// How many samples are waiting to be taken.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ready.len()
    }

    /// True when nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    /// Take the oldest sample waiting.
    pub fn take(&mut self) -> Option<Sample> {
        self.ready.pop_front()
    }

    /// Take every sample waiting.
    pub fn take_all(&mut self) -> Vec<Sample> {
        self.ready.drain(..).collect()
    }

    /// Look at the oldest sample without taking it.
    #[must_use]
    pub fn peek(&self) -> Option<&Sample> {
        self.ready.front()
    }

    /// Accept a `DATA` from `source`.
    ///
    /// Returns `true` when the sample was new and has been queued.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`](crate::behavior::BehaviorError::Wire) when the
    /// submessage fails its §8.3.7 validity clauses.
    pub fn on_data(
        &mut self,
        source: GuidPrefix,
        data: &Data<'_>,
        timestamp: Option<Time>,
        now: Instant,
    ) -> BehaviorResult<bool> {
        data.validate()?;
        if !self.addresses_me(data.reader_id) {
            return Ok(false);
        }
        let writer = Guid::new(source, data.writer_id);
        self.admit(writer);
        let Some(proxy) = self.matched.get_mut(&writer) else {
            return Ok(false);
        };
        // A stateless reader takes every DATA. See `ReaderConfig::stateless`.
        let accepted = proxy.accept_data(data.writer_sn);
        if !accepted && !self.config.stateless {
            return Ok(false);
        }
        let kind = payload_kind(data);
        let payload = data
            .payload
            .payload()
            .map(SerializedPayload::as_slice)
            .unwrap_or_default()
            .to_vec();
        self.deliver(writer, data.writer_sn, payload, timestamp, kind, now);
        Ok(true)
    }

    /// Accept a `DATA_FRAG` from `source`.
    ///
    /// Returns `true` when the fragment completed a sample.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`](crate::behavior::BehaviorError::Wire) for an
    /// invalid submessage, or
    /// [`BehaviorError::Reassembly`](crate::behavior::BehaviorError::Reassembly)
    /// when the series contradicts itself.
    pub fn on_data_frag(
        &mut self,
        source: GuidPrefix,
        fragment: &DataFrag<'_>,
        timestamp: Option<Time>,
        now: Instant,
    ) -> BehaviorResult<bool> {
        fragment.validate()?;
        if !self.addresses_me(fragment.reader_id) {
            return Ok(false);
        }
        let writer = Guid::new(source, fragment.writer_id);
        self.admit(writer);
        let Some(proxy) = self.matched.get(&writer) else {
            return Ok(false);
        };
        if proxy.is_satisfied(fragment.writer_sn) {
            return Ok(false);
        }
        let Some(payload) = self.reassembler.accept(writer, fragment)? else {
            return Ok(false);
        };
        let Some(proxy) = self.matched.get_mut(&writer) else {
            return Ok(false);
        };
        if !proxy.accept_data(fragment.writer_sn) {
            return Ok(false);
        }
        self.deliver(
            writer,
            fragment.writer_sn,
            payload,
            timestamp,
            ChangeKind::Alive,
            now,
        );
        Ok(true)
    }

    /// Accept a `HEARTBEAT` from `source`.
    ///
    /// Returns `true` when it was fresh and applied; the `ACKNACK` it
    /// prompts goes out on the next [`produce`](Self::produce).
    pub fn on_heartbeat(&mut self, source: GuidPrefix, heartbeat: &Heartbeat) -> bool {
        if !self.addresses_me(heartbeat.reader_id) {
            return false;
        }
        let writer = Guid::new(source, heartbeat.writer_id);
        match self.matched.get_mut(&writer) {
            None => false,
            Some(proxy) if !proxy.is_reliable() => {
                // A best-effort reader still learns the window, so that its
                // "greater than the highest seen" rule does not stall after
                // the writer restarts its history — but it will never answer,
                // so the pending flag must not latch.
                let applied = proxy.accept_heartbeat(heartbeat);
                proxy.clear_heartbeat_pending();
                applied
            }
            Some(proxy) => proxy.accept_heartbeat(heartbeat),
        }
    }

    /// Accept a `GAP` from `source`.
    ///
    /// Returns how many sequence numbers newly became irrelevant.
    pub fn on_gap(&mut self, source: GuidPrefix, gap: &Gap) -> usize {
        if !self.addresses_me(gap.reader_id) {
            return 0;
        }
        let writer = Guid::new(source, gap.writer_id);
        let Some(proxy) = self.matched.get_mut(&writer) else {
            return 0;
        };
        let added = proxy.accept_gap(gap);
        for number in gap.irrelevant() {
            self.reassembler.discard(writer, number);
        }
        added
    }

    /// Everything this reader wants to put on the wire right now.
    ///
    /// One `ACKNACK` per matched reliable writer that has heartbeated without
    /// being answered.
    ///
    /// `NACK_FRAG` is deliberately **not** produced here. A fragment repair
    /// asked for on every arriving fragment is a request per fragment, each
    /// of which provokes a retransmission — quadratic traffic on a sample
    /// that is arriving perfectly well. Fragment repair belongs on the
    /// cadence, where [`produce_nack_frags`](Self::produce_nack_frags) only
    /// asks about a series that has stopped making progress.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`](crate::behavior::BehaviorError::Wire) when a
    /// message will not encode.
    pub fn produce(&mut self, now: Instant) -> BehaviorResult<Vec<Outbound>> {
        let _ = now;
        let mut outbound = Vec::new();
        let writers: Vec<Guid> = self.matched.keys().copied().collect();
        for writer in writers {
            let Some(proxy) = self.matched.get_mut(&writer) else {
                continue;
            };
            if !proxy.is_reliable() || !proxy.heartbeat_pending() {
                continue;
            }
            let locators = proxy.locators();
            if locators.is_empty() {
                continue;
            }
            let acknack = proxy.take_acknack(self.config.guid.entity_id);
            let mut message = Message::new(self.header());
            message.push(InfoDestination::new(writer.prefix));
            message.push(acknack);
            outbound.push(Outbound::new(locators, message.encode()?));
        }
        Ok(outbound)
    }

    /// A `NACK_FRAG` for every fragment series that has **stalled**.
    ///
    /// Called from the participant's cadence, not from
    /// [`produce`](Self::produce). A series is stalled when no new fragment
    /// has arrived since the previous call: while fragments are still coming
    /// in, asking for the ones that have not arrived yet is asking the writer
    /// to resend what is already in flight.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Wire`](crate::behavior::BehaviorError::Wire).
    pub fn produce_nack_frags(&mut self) -> BehaviorResult<Vec<Outbound>> {
        let mut outbound = Vec::new();
        let writers: Vec<Guid> = self.matched.keys().copied().collect();
        let mut still_pending: Vec<(Guid, SequenceNumber, u32)> = Vec::new();
        for writer in writers {
            let Some(proxy) = self.matched.get(&writer) else {
                continue;
            };
            if !proxy.is_reliable() {
                continue;
            }
            let locators = proxy.locators();
            if locators.is_empty() {
                continue;
            }
            let pending: Vec<SequenceNumber> = self.reassembler.in_flight(writer).collect();
            for number in pending {
                let received = self
                    .reassembler
                    .assembly(writer, number)
                    .map_or(0, crate::behavior::fragment::Assembly::received_count);
                let previous = self.fragment_progress.get(&(writer, number)).copied();
                still_pending.push((writer, number, received));
                if previous != Some(received) {
                    // Progress since the last check: the series is arriving.
                    continue;
                }
                let Some(missing) = self.reassembler.missing(writer, number) else {
                    continue;
                };
                if missing.is_empty() {
                    continue;
                }
                let Some(proxy) = self.matched.get_mut(&writer) else {
                    continue;
                };
                let nack = NackFrag::new(
                    self.config.guid.entity_id,
                    writer.entity_id,
                    number,
                    missing,
                    proxy.next_acknack_count(),
                );
                let mut message = Message::new(self.header());
                message.push(InfoDestination::new(writer.prefix));
                message.push(nack);
                outbound.push(Outbound::new(locators.clone(), message.encode()?));
            }
        }
        self.fragment_progress.clear();
        for (writer, number, received) in still_pending {
            self.fragment_progress.insert((writer, number), received);
        }
        Ok(outbound)
    }

    /// Which matched writers have missed the reader's `DEADLINE`.
    ///
    /// An infinite deadline — the default — never reports anything, and a
    /// writer that has never sent is not yet late: the clock starts at the
    /// first sample, as DDS's `requested_deadline_missed` does.
    #[must_use]
    pub fn missed_deadlines(&self, now: Instant) -> Vec<DeadlineMiss> {
        let Some(period) = self.config.qos.deadline.period.to_std() else {
            return Vec::new();
        };
        if self.config.qos.deadline.is_infinite() {
            return Vec::new();
        }
        self.last_sample_at
            .iter()
            .filter_map(|(writer, last)| {
                let since = now.saturating_duration_since(*last);
                (since > period).then_some(DeadlineMiss {
                    writer: *writer,
                    since,
                    period,
                })
            })
            .collect()
    }

    /// When the reader last accepted a sample from `writer`.
    #[must_use]
    pub fn last_sample_at(&self, writer: Guid) -> Option<Instant> {
        self.last_sample_at.get(&writer).copied()
    }

    /// The reassembler, for inspection.
    #[must_use]
    pub const fn reassembler(&self) -> &Reassembler {
        &self.reassembler
    }

    /// The header every message from this reader's participant carries.
    fn header(&self) -> Header {
        Header::new(self.config.guid.prefix)
    }

    /// True when a submessage addressed to `reader_id` is for this reader.
    fn addresses_me(&self, reader_id: crate::structure::EntityId) -> bool {
        reader_id.is_unknown() || reader_id == self.config.guid.entity_id
    }

    /// Create a proxy for an unmatched writer, when this reader is stateless.
    ///
    /// The proxy is best-effort: SPDP has no reliability protocol, and a
    /// reliable proxy would start ACKNACKing a writer that has no
    /// `ReaderProxy` for this reader and would ignore it.
    fn admit(&mut self, writer: Guid) {
        if !self.config.stateless || self.matched.contains_key(&writer) {
            return;
        }
        self.matched
            .insert(writer, WriterProxy::new(writer, Vec::new(), false));
    }

    /// Queue a sample, applying the reader's own history bound.
    fn deliver(
        &mut self,
        writer: Guid,
        sequence_number: SequenceNumber,
        payload: Vec<u8>,
        timestamp: Option<Time>,
        kind: ChangeKind,
        now: Instant,
    ) {
        self.last_sample_at.insert(writer, now);
        self.ready.push_back(Sample {
            writer,
            sequence_number,
            payload,
            source_timestamp: timestamp,
            received_at: now,
            kind,
            instance: InstanceHandle::NIL,
        });
        let requested = self
            .config
            .qos
            .history
            .retained()
            .unwrap_or(MAX_UNTAKEN_SAMPLES);
        let depth = requested.clamp(1, MAX_UNTAKEN_SAMPLES);
        while self.ready.len() > depth {
            self.ready.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
    }
}

/// Read the lifecycle kind out of a `DATA`'s inline QoS and payload shape.
fn payload_kind(data: &Data<'_>) -> ChangeKind {
    if let Some(list) = &data.inline_qos
        && let Some(parameter) = list.get_by_base(astrs_cdr::pid::STATUS_INFO)
        && let Some(octets) = parameter.value.get(..4)
    {
        let mut status = [0_u8; 4];
        status.copy_from_slice(octets);
        return ChangeKind::from_status_info(status);
    }
    match &data.payload {
        crate::messages::DataPayload::Data(_) => ChangeKind::Alive,
        _ => ChangeKind::NotAliveDisposed,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::behavior::fragment::fragment_sample;
    use crate::discovery::qos::HistoryQos;
    use crate::messages::{DataPayload, Submessage};
    use crate::structure::{EntityId, EntityKind, Locator, SequenceNumberSet, VendorId};
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

    fn reader(qos: ReaderQos) -> RtpsReader {
        RtpsReader::new(ReaderConfig::new(reader_guid(), topic()).with_qos(qos))
    }

    fn proxy(reliable: bool) -> WriterProxy {
        WriterProxy::new(
            writer_guid(),
            vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 45_000)],
            reliable,
        )
    }

    fn data(number: i64, payload: &[u8]) -> Data<'_> {
        Data::new(
            EntityId::UNKNOWN,
            writer_guid().entity_id,
            SequenceNumber::new(number),
            DataPayload::Data(SerializedPayload::new(payload)),
        )
    }

    fn heartbeat(first: i64, last: i64, count: i32) -> Heartbeat {
        Heartbeat::new(
            EntityId::UNKNOWN,
            writer_guid().entity_id,
            SequenceNumber::new(first),
            SequenceNumber::new(last),
            count,
        )
    }

    #[test]
    fn a_sample_from_an_unmatched_writer_is_ignored() {
        let mut reader = reader(ReaderQos::reliable(10));
        let now = Instant::now();
        assert!(
            !reader
                .on_data(writer_guid().prefix, &data(1, b"hi\0\0"), None, now)
                .unwrap()
        );
        assert!(reader.is_empty());
    }

    #[test]
    fn a_matched_writers_sample_is_queued() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"hi\0\0"), None, now)
                .unwrap()
        );
        assert_eq!(reader.len(), 1);
        let sample = reader.take().expect("a sample");
        assert_eq!(sample.as_slice(), b"hi\0\0");
        assert_eq!(sample.writer, writer_guid());
        assert_eq!(sample.sequence_number, SequenceNumber::FIRST);
        assert!(sample.is_alive());
        assert!(reader.is_empty());
    }

    #[test]
    fn a_duplicate_is_not_queued_twice() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        assert!(
            !reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        assert_eq!(reader.len(), 1);
    }

    #[test]
    fn a_reliable_reader_holds_out_of_order_arrivals() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(3, b"cccc"), None, now)
                .unwrap(),
            "a reliable reader takes sample 3 while 2 is missing"
        );
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(2, b"bbbb"), None, now)
                .unwrap()
        );
        assert_eq!(reader.len(), 3);
    }

    #[test]
    fn a_best_effort_reader_drops_what_arrives_late() {
        let mut reader = reader(ReaderQos::sensor_data());
        reader.match_writer(proxy(false));
        let now = Instant::now();
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(3, b"cccc"), None, now)
                .unwrap()
        );
        assert!(
            !reader
                .on_data(writer_guid().prefix, &data(2, b"bbbb"), None, now)
                .unwrap(),
            "sample 2 arrives after 3 and is dropped: there is no repair"
        );
        assert_eq!(reader.len(), 2);
    }

    #[test]
    fn a_sample_addressed_to_another_reader_is_ignored() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        let mut targeted = data(1, b"aaaa");
        targeted.reader_id = EntityId::user_defined(99, EntityKind::USER_READER_NO_KEY);
        assert!(
            !reader
                .on_data(writer_guid().prefix, &targeted, None, now)
                .unwrap()
        );
    }

    #[test]
    fn a_sample_addressed_to_this_reader_by_name_is_taken() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        let mut targeted = data(1, b"aaaa");
        targeted.reader_id = reader_guid().entity_id;
        assert!(
            reader
                .on_data(writer_guid().prefix, &targeted, None, now)
                .unwrap()
        );
    }

    #[test]
    fn a_heartbeat_prompts_an_acknack() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        reader
            .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
            .unwrap();
        assert!(reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 3, 1)));

        let outbound = reader.produce(now).unwrap();
        assert_eq!(outbound.len(), 1);
        let message = Message::decode(&outbound[0].datagram).unwrap();
        let acknack = message
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::AckNack(acknack) => Some(acknack),
                _ => None,
            })
            .expect("an ACKNACK");
        assert_eq!(acknack.reader_sn_state.base(), SequenceNumber::new(2));
        assert_eq!(
            acknack
                .missing()
                .map(SequenceNumber::value)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(!acknack.is_final);

        assert!(
            reader.produce(now).unwrap().is_empty(),
            "one heartbeat, one answer"
        );
    }

    #[test]
    fn a_best_effort_reader_never_acknacks() {
        let mut reader = reader(ReaderQos::sensor_data());
        reader.match_writer(proxy(false));
        let now = Instant::now();
        reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 5, 1));
        assert!(reader.produce(now).unwrap().is_empty());
    }

    #[test]
    fn a_stale_heartbeat_is_ignored() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        assert!(reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 5, 3)));
        assert!(!reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 5, 3)));
        assert!(!reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 5, 1)));
    }

    #[test]
    fn a_gap_closes_the_hole_without_a_sample() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        reader
            .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
            .unwrap();
        reader
            .on_data(writer_guid().prefix, &data(4, b"dddd"), None, now)
            .unwrap();
        reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 4, 1));

        let gap = Gap::contiguous(
            EntityId::UNKNOWN,
            writer_guid().entity_id,
            SequenceNumber::new(2),
            SequenceNumber::new(3),
        );
        assert_eq!(reader.on_gap(writer_guid().prefix, &gap), 2);

        let outbound = reader.produce(now).unwrap();
        let message = Message::decode(&outbound[0].datagram).unwrap();
        let acknack = message
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::AckNack(acknack) => Some(acknack),
                _ => None,
            })
            .expect("an ACKNACK");
        assert_eq!(
            acknack.reader_sn_state.base(),
            SequenceNumber::new(5),
            "everything through 4 is satisfied"
        );
        assert!(acknack.is_final);
    }

    #[test]
    fn keep_last_bounds_the_untaken_queue() {
        let mut reader = reader(ReaderQos {
            history: HistoryQos::keep_last(2),
            ..ReaderQos::reliable(2)
        });
        reader.match_writer(proxy(true));
        let now = Instant::now();
        for value in 1..=5_i64 {
            reader
                .on_data(writer_guid().prefix, &data(value, b"aaaa"), None, now)
                .unwrap();
        }
        assert_eq!(reader.len(), 2);
        assert_eq!(reader.dropped_count(), 3);
        let oldest = reader.peek().expect("a sample");
        assert_eq!(oldest.sequence_number, SequenceNumber::new(4));
    }

    #[test]
    fn a_dropped_sample_is_still_acknowledged() {
        let mut reader = reader(ReaderQos {
            history: HistoryQos::keep_last(1),
            ..ReaderQos::reliable(1)
        });
        reader.match_writer(proxy(true));
        let now = Instant::now();
        for value in 1..=3_i64 {
            reader
                .on_data(writer_guid().prefix, &data(value, b"aaaa"), None, now)
                .unwrap();
        }
        reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 3, 1));
        let outbound = reader.produce(now).unwrap();
        let message = Message::decode(&outbound[0].datagram).unwrap();
        let acknack = message
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::AckNack(acknack) => Some(acknack),
                _ => None,
            })
            .expect("an ACKNACK");
        assert_eq!(acknack.reader_sn_state.base(), SequenceNumber::new(4));
        assert!(acknack.is_final, "the reader is not asking for them again");
    }

    #[test]
    fn a_fragmented_sample_is_reassembled_and_delivered_once() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        let payload: Vec<u8> = (0..3_000).map(|index| (index % 251) as u8).collect();
        let fragments = fragment_sample(
            EntityId::UNKNOWN,
            writer_guid().entity_id,
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut delivered = 0;
        for fragment in &fragments {
            if reader
                .on_data_frag(writer_guid().prefix, fragment, None, now)
                .unwrap()
            {
                delivered += 1;
            }
        }
        assert_eq!(delivered, 1, "one sample, however many fragments");
        assert_eq!(
            reader.take().expect("a sample").as_slice(),
            payload.as_slice()
        );
    }

    #[test]
    fn a_missing_fragment_produces_a_nack_frag() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        let payload = vec![0x7e_u8; 3_000];
        let fragments = fragment_sample(
            EntityId::UNKNOWN,
            writer_guid().entity_id,
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        reader
            .on_data_frag(writer_guid().prefix, &fragments[0], None, now)
            .unwrap();

        assert!(
            reader.produce(now).unwrap().is_empty(),
            "a fragment repair is never asked for on the arrival path"
        );
        assert!(
            reader.produce_nack_frags().unwrap().is_empty(),
            "the first cadence pass only records progress"
        );
        let outbound = reader.produce_nack_frags().unwrap();
        let message = Message::decode(&outbound[0].datagram).unwrap();
        let nack = message
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::NackFrag(nack) => Some(nack),
                _ => None,
            })
            .expect("a NACK_FRAG");
        assert_eq!(nack.writer_sn, SequenceNumber::FIRST);
        assert!(!nack.fragment_number_state.is_empty());
    }

    #[test]
    fn unmatching_a_writer_forgets_its_fragments() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        let payload = vec![1_u8; 3_000];
        let fragments = fragment_sample(
            EntityId::UNKNOWN,
            writer_guid().entity_id,
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();
        reader
            .on_data_frag(writer_guid().prefix, &fragments[0], None, now)
            .unwrap();
        assert_eq!(reader.reassembler().len(), 1);

        assert!(reader.unmatch_writer(writer_guid()));
        assert_eq!(reader.reassembler().len(), 0);
        assert_eq!(reader.matched_writer_count(), 0);
    }

    #[test]
    fn unmatching_a_participant_removes_all_of_its_writers() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        reader.match_writer(WriterProxy::new(
            Guid::new(
                writer_guid().prefix,
                EntityId::user_defined(2, EntityKind::USER_WRITER_NO_KEY),
            ),
            Vec::new(),
            true,
        ));
        assert_eq!(reader.matched_writer_count(), 2);
        assert_eq!(reader.unmatch_participant(writer_guid()), 2);
    }

    #[test]
    fn an_infinite_deadline_never_reports_a_miss() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        reader
            .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
            .unwrap();
        assert!(
            reader
                .missed_deadlines(now + StdDuration::from_secs(3_600))
                .is_empty()
        );
    }

    #[test]
    fn a_finite_deadline_reports_a_writer_that_went_quiet() {
        let mut reader = reader(ReaderQos {
            deadline: crate::discovery::qos::DeadlineQos::from_millis(100),
            ..ReaderQos::reliable(10)
        });
        reader.match_writer(proxy(true));
        let now = Instant::now();
        reader
            .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
            .unwrap();
        assert!(
            reader
                .missed_deadlines(now + StdDuration::from_millis(50))
                .is_empty()
        );

        let missed = reader.missed_deadlines(now + StdDuration::from_millis(150));
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0].writer, writer_guid());
        assert_eq!(missed[0].period, StdDuration::from_millis(100));
        assert_eq!(reader.last_sample_at(writer_guid()), Some(now));
    }

    #[test]
    fn a_writer_that_has_never_sent_is_not_yet_late() {
        let reader = reader(ReaderQos {
            deadline: crate::discovery::qos::DeadlineQos::from_millis(10),
            ..ReaderQos::reliable(10)
        });
        assert!(
            reader
                .missed_deadlines(Instant::now() + StdDuration::from_secs(1))
                .is_empty()
        );
    }

    #[test]
    fn a_source_timestamp_reaches_the_sample() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        let stamp = Time::new(1_700_000_000, 0);
        reader
            .on_data(writer_guid().prefix, &data(1, b"aaaa"), Some(stamp), now)
            .unwrap();
        assert_eq!(
            reader.take().expect("a sample").source_timestamp,
            Some(stamp)
        );
    }

    #[test]
    fn a_key_only_data_is_delivered_as_not_alive() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        let disposal = Data::new(
            EntityId::UNKNOWN,
            writer_guid().entity_id,
            SequenceNumber::FIRST,
            DataPayload::Key(SerializedPayload::new(&b"key\0"[..])),
        );
        assert!(
            reader
                .on_data(writer_guid().prefix, &disposal, None, now)
                .unwrap()
        );
        let sample = reader.take().expect("a sample");
        assert!(!sample.is_alive());
        assert_eq!(sample.kind, ChangeKind::NotAliveDisposed);
    }

    #[test]
    fn take_all_drains_the_queue() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        for value in 1..=3_i64 {
            reader
                .on_data(writer_guid().prefix, &data(value, b"aaaa"), None, now)
                .unwrap();
        }
        assert_eq!(reader.take_all().len(), 3);
        assert!(reader.is_empty());
        assert!(reader.take().is_none());
    }

    #[test]
    fn a_stateless_reader_takes_from_strangers_and_takes_repeats() {
        let mut reader = RtpsReader::new(
            ReaderConfig::new(reader_guid(), topic())
                .with_qos(ReaderQos::builtin_spdp())
                .stateless(),
        );
        let now = Instant::now();

        // No proxy exists: a stateful reader would drop this, and discovery
        // would never start.
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        assert_eq!(reader.matched_writer_count(), 1, "a proxy is admitted");

        // The same sequence number again: an SPDP writer resends one change
        // forever, and every resend must be delivered so the lease is renewed.
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        // All three were *accepted*; the SPDP reader's KEEP_LAST 1 history
        // keeps only the newest, which is exactly right — the participant
        // drains it after every datagram, and an older copy of the same
        // announcement is worth nothing.
        assert_eq!(reader.len(), 1);
        assert_eq!(reader.dropped_count(), 2);
    }

    #[test]
    fn a_stateful_reader_does_neither() {
        let mut reader = reader(ReaderQos::reliable(10));
        let now = Instant::now();
        assert!(
            !reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap(),
            "an unmatched writer is not admitted"
        );
        reader.match_writer(proxy(true));
        assert!(
            reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap()
        );
        assert!(
            !reader
                .on_data(writer_guid().prefix, &data(1, b"aaaa"), None, now)
                .unwrap(),
            "and a repeat is a duplicate"
        );
        assert_eq!(reader.len(), 1);
    }

    #[test]
    fn the_reader_reports_its_own_configuration() {
        let reader = reader(ReaderQos::reliable(4));
        assert_eq!(reader.guid(), reader_guid());
        assert_eq!(reader.topic().topic_name, "rt/chatter");
        assert!(reader.is_reliable());
        assert!(reader.qos().is_reliable());
        assert_eq!(reader.config().heartbeat_response_delay, StdDuration::ZERO);
        assert_eq!(reader.matched_writers().count(), 0);
        assert!(!reader.is_matched(writer_guid()));
    }

    #[test]
    fn an_acknack_is_addressed_to_the_writers_participant() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 1, 1));
        let outbound = reader.produce(now).unwrap();
        let message = Message::decode(&outbound[0].datagram).unwrap();
        let destination = message
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::InfoDestination(destination) => Some(destination),
                _ => None,
            })
            .expect("an INFO_DST");
        assert_eq!(destination.guid_prefix, writer_guid().prefix);
    }

    #[test]
    fn the_first_acknack_after_nothing_arrives_asks_for_everything() {
        let mut reader = reader(ReaderQos::reliable(10));
        reader.match_writer(proxy(true));
        let now = Instant::now();
        reader.on_heartbeat(writer_guid().prefix, &heartbeat(1, 2, 1));
        let outbound = reader.produce(now).unwrap();
        let message = Message::decode(&outbound[0].datagram).unwrap();
        let acknack = message
            .iter()
            .find_map(|submessage| match submessage {
                Submessage::AckNack(acknack) => Some(acknack),
                _ => None,
            })
            .expect("an ACKNACK");
        assert_eq!(acknack.reader_sn_state.base(), SequenceNumber::FIRST);
        assert_eq!(
            acknack
                .missing()
                .map(SequenceNumber::value)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            acknack.reader_sn_state,
            SequenceNumberSet::from_numbers(
                SequenceNumber::FIRST,
                [SequenceNumber::new(1), SequenceNumber::new(2)]
            )
            .unwrap()
        );
    }
}
