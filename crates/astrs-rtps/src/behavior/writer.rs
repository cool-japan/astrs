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
//! 2. **Then what the reader has never been served**, oldest first, from its
//!    served frontier: the samples the history holds, and a `GAP` for the
//!    numbers between them it does not — a reader waiting on an evicted,
//!    expired or swept number must be told it is never coming, or it will
//!    ask forever. The frontier then moves past both, so nothing is pushed
//!    twice; what a reader lost on the way, it nacks.
//! 3. **Then a prompt**, when a repair stopped short of what the reader had
//!    already been served: one non-final `HEARTBEAT` to that reader alone, so
//!    it names what else it lacks now rather than a heartbeat period later.
//! 4. **Then the retirements every reader now has are swept** — see below.
//! 5. **Then a HEARTBEAT**, when the cadence is due. Best-effort writers
//!    skip this step and step 3 entirely.
//!
//! # Holes: one `GAP` per run, however long the run
//!
//! A history is not a contiguous range. `KEEP_LAST` evicts, `LIFESPAN`
//! expires, and a retirement leaves with its instance's older changes, so a
//! long-lived `TRANSIENT_LOCAL` writer holds a few changes scattered across
//! everything it ever wrote: the SEDP writer of a node that has created and
//! deleted endpoints for a week holds its live endpoints' announcements and
//! a hundred thousand numbers of nothing between them. So the writer walks
//! the numbers it *holds*, never the numbers between, and names each run of
//! holes with one `GAP` whose contiguous part — `gapStart` through
//! `gapList.bitmapBase - 1`, which §8.3.7.4 puts no length limit on — is the
//! whole run. A late joiner is served such a history in one call, and
//! [`MAX_SAMPLES_PER_PRODUCE`] bounds the samples, never the holes.
//!
//! Repairs follow the same rule. A reader that lost that `GAP` can only nack
//! the first 256 numbers of the run — an `ACKNACK`'s bitmap is no wider —
//! and is answered with the whole run, through the next change the history
//! holds, rather than with 256 numbers per round trip.
//!
//! # Retirements: the one change that leaves once it is delivered
//!
//! [`RtpsWriter::dispose`] writes what deleting an entity writes on its
//! builtin discovery topic: the instance is disposed *and* unregistered —
//! gone, and never to be written again. Every other change leaves the
//! history only when `KEEP_LAST` or `LIFESPAN` pushes it out, which for a
//! retirement would be never: its instance is never written again, and
//! entity keys are never reused. So a retirement is held only until every
//! matched reader has it, then dropped with whatever older change of its
//! instance is still held; a reader that matches afterwards is `GAP`ped past
//! it, and learns — correctly — nothing about an entity that was gone before
//! it arrived. The writer checks whenever the answer can change: when an
//! `ACKNACK` moves a reader's watermark; in every
//! [`produce`](RtpsWriter::produce), which is when a best-effort reader's
//! copy goes out and which the cadence runs after a reader is unmatched; and
//! before the next change is written or the next reader is matched.
//!
//! One kind of unmatched reader still counts: one whose participant's lease
//! ran out here, which may be alive and still hold the entity. The builtin
//! SEDP writers remember such a reader as *lapsed*, with what it had
//! acknowledged, and hold every retirement since for it until it is matched
//! again — the new proxy is replayed them — or until the participant says
//! when to stop owing it.
//!
//! # Fragmentation
//!
//! A sample above [`FRAGMENTATION_THRESHOLD`] is sent as `DATA_FRAG`
//! submessages, one datagram's worth at a time. The threshold and the
//! fragment size are configuration, not constants, so a test can force
//! fragmentation on a small payload and a real deployment can match its MTU.

use std::collections::{BTreeMap, BTreeSet};
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
/// time; the rest goes out on the next call. Holes do not count against it:
/// a run of numbers the history no longer holds is one `GAP` whatever its
/// length, so two held changes a hundred thousand numbers apart are served
/// in one call.
pub const MAX_SAMPLES_PER_PRODUCE: usize = 256;

/// Most lapsed readers one writer remembers.
///
/// One per peer participant for a builtin SEDP writer, so the same bound the
/// discovery database puts on participants. See
/// [`RtpsWriter::lapse_participant`].
const MAX_LAPSED_READERS: usize = crate::discovery::db::MAX_PARTICIPANTS;

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
    /// The retirements [`dispose`](Self::dispose) wrote that the history
    /// still holds.
    ///
    /// Beside the cache rather than in it, because a retirement says two
    /// things a [`ChangeKind`] cannot say at once — disposed *and*
    /// unregistered — and because it is the one change this writer drops
    /// once every matched reader has it. See
    /// [`sweep_retirements`](Self::sweep_retirements).
    retired: BTreeSet<SequenceNumber>,
    /// Readers whose participant's lease ran out while they were matched,
    /// kept for the retirements they may still be owed. See
    /// [`lapse_participant`](Self::lapse_participant).
    lapsed: BTreeMap<Guid, Lapse>,
}

/// A reader this writer unmatched because its participant's lease ran out.
///
/// A lease that runs out here says nothing about the reader: its participant
/// may be alive and still hold every endpoint this one announced, having
/// heard from this participant all along. What such a reader is still owed
/// is the retirement of every endpoint deleted since — and a retirement is
/// swept as soon as every *matched* reader has it, which with the reader
/// unmatched could be at once. The record holds those retirements back until
/// the reader is matched again, which replays them, or until the record
/// expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lapse {
    /// What the reader had acknowledged when it lapsed; every retirement
    /// above it is held for the reader.
    acked_through: SequenceNumber,
    /// When the record is dropped if the reader has not been matched again.
    until: Instant,
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
            retired: BTreeSet::new(),
            lapsed: BTreeMap::new(),
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

    /// Retire an instance: write the change that says an entity is gone.
    ///
    /// What deleting an entity writes on its builtin discovery topic — the
    /// departing participant's GUID on SPDP, the deleted endpoint's on SEDP.
    /// The key is that GUID, wrapped in a `CDR_LE` encapsulation header so a
    /// receiver can find it at a fixed offset. The change is filed under the
    /// GUID's own instance (a GUID is its own key hash, §9.6.3.8), so under
    /// `KEEP_LAST` it replaces that instance's announcement and leaves every
    /// other instance's alone.
    ///
    /// # Disposed *and* unregistered
    ///
    /// The `DATA` carries `PID_STATUS_INFO` with both flags set (§9.6.3.9),
    /// which is what DDS sends for a deleted entity: the instance is
    /// disposed, and this writer will never write it again. The history files
    /// the change as [`ChangeKind::NotAliveDisposed`], the kind a receiver
    /// reads either form back as, and the writer remembers the
    /// unregistration beside it.
    ///
    /// The unregistration is what bounds the history. A disposal alone is
    /// state a `TRANSIENT_LOCAL` writer owes every late joiner, and one kept
    /// for every entity ever deleted would grow for the whole life of the
    /// participant: entity keys are never reused, so nothing would ever
    /// replace it. An unregistered instance is owed to nobody who arrives
    /// later — its announcement is already gone, so a late joiner told
    /// nothing about it knows exactly what it should. The change is therefore
    /// held only until every matched reader has it — acknowledged by a
    /// reliable one, sent to a best-effort one — and every lapsed one too
    /// (see the module docs), and then dropped, together with any older
    /// change still held for the instance. A reader matched after that is
    /// `GAP`ped past it.
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
        let number = self.write_change(
            payload,
            None,
            ChangeKind::NotAliveDisposed,
            crate::discovery::sedp::guid_instance(key),
            now,
        )?;
        self.retired.insert(number);
        Ok(number)
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
    ///
    /// # A reader that lapsed
    ///
    /// Matching a reader recorded as lapsed — unmatched because its
    /// participant's lease ran out — ends the record: the new proxy starts like any late
    /// joiner's, and the retirements the record held back are replayed to
    /// it with the rest of the history. That is how a peer this writer's
    /// participant had given up on, but which never gave up on it, learns
    /// which endpoints were deleted while it was away.
    ///
    /// A reader that is still matched is left alone, whatever it may have
    /// forgotten: a repeated SPDP or SEDP announcement says nothing about
    /// that. What does is the reader's own next `ACKNACK` — see
    /// [`ReaderProxy::accept_acknack`].
    pub fn match_reader(&mut self, proxy: ReaderProxy) {
        let guid = proxy.guid();
        match self.matched.get_mut(&guid) {
            Some(existing) => {
                existing.set_locators(proxy.locators(), Vec::new());
            }
            None => {
                // Before the newcomer counts: a retirement every reader
                // already matched has is swept first, so a late joiner is
                // never replayed the end of an entity that was gone before it
                // arrived. See `sweep_retirements`.
                self.sweep_retirements();
                let mut proxy = proxy;
                if !self.replays_history_to(&proxy) {
                    // VOLATILE on either side: the late joiner starts where
                    // the writer is now, not at sample one. See
                    // `ReaderProxy::skip_history_through`.
                    proxy.skip_history_through(self.last_change);
                }
                self.matched.insert(guid, proxy);
                // A reader that lapsed is back. Its new proxy is replayed
                // the history, the retirements held for it included, and
                // holds them itself from now on.
                self.lapsed.remove(&guid);
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
    ///
    /// The reader is gone for good, so a record of it having lapsed goes
    /// too.
    pub fn unmatch_reader(&mut self, guid: Guid) -> bool {
        if self.lapsed.remove(&guid).is_some() {
            self.sweep_retirements();
        }
        self.matched.remove(&guid).is_some()
    }

    /// Remove every matched reader belonging to one participant.
    ///
    /// Leaves any record of the participant's readers having lapsed alone:
    /// when the participant's lease runs out, its readers of the builtin
    /// SEDP writers are recorded as lapsed first, and the rest of it is then
    /// unwired through here.
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

    /// Unmatch every reader of `participant`, whose lease ran out, and
    /// remember each as lapsed until `until`.
    ///
    /// [`unmatch_participant`](Self::unmatch_participant), except that each
    /// active reader's acknowledged watermark is kept in a [`Lapse`] record,
    /// and every retirement above it is held — owed to the reader, which may
    /// be alive and still hold the endpoint the retirement ends — until the
    /// reader is matched again, which replays it, or until `until`. At most
    /// [`MAX_LAPSED_READERS`] are remembered; beyond that, of the records
    /// already held, the one that would expire soonest makes room.
    ///
    /// Only the participant decides what `until` is, and only for the
    /// builtin writers whose retirements say an endpoint is gone. Returns
    /// how many readers were unmatched.
    pub(crate) fn lapse_participant(&mut self, participant: Guid, until: Instant) -> usize {
        let lapsing: Vec<Guid> = self
            .matched
            .keys()
            .filter(|guid| guid.prefix == participant.prefix)
            .copied()
            .collect();
        for guid in &lapsing {
            let Some(proxy) = self.matched.remove(guid) else {
                continue;
            };
            if !proxy.is_active() {
                // Owed nothing while it was matched either: see
                // `settled_through`.
                continue;
            }
            if !self.lapsed.contains_key(guid) && self.lapsed.len() >= MAX_LAPSED_READERS {
                let soonest = self
                    .lapsed
                    .iter()
                    .min_by_key(|(_, lapse)| lapse.until)
                    .map(|(held, _)| *held);
                if let Some(soonest) = soonest {
                    self.lapsed.remove(&soonest);
                }
            }
            self.lapsed.insert(
                *guid,
                Lapse {
                    acked_through: proxy.acked_through(),
                    until,
                },
            );
        }
        if !lapsing.is_empty() {
            // A record dropped to make room may have been the last thing
            // holding a retirement.
            self.sweep_retirements();
        }
        lapsing.len()
    }

    /// Forget every lapsed reader of `participant`, and sweep what only they
    /// held.
    ///
    /// For a participant that announced its departure: it is gone, and owed
    /// nothing. Returns how many records went.
    pub(crate) fn forget_lapsed_participant(&mut self, participant: Guid) -> usize {
        let before = self.lapsed.len();
        self.lapsed
            .retain(|guid, _| guid.prefix != participant.prefix);
        self.forgot_lapses(before)
    }

    /// Forget every lapsed reader whose record expires at or before `now`,
    /// and sweep what only they held. Returns how many records went.
    pub(crate) fn forget_lapses_due(&mut self, now: Instant) -> usize {
        let before = self.lapsed.len();
        self.lapsed.retain(|_, lapse| lapse.until > now);
        self.forgot_lapses(before)
    }

    /// How many lapse records went since there were `before`, sweeping when
    /// any did.
    fn forgot_lapses(&mut self, before: usize) -> usize {
        let forgotten = before.saturating_sub(self.lapsed.len());
        if forgotten > 0 {
            self.sweep_retirements();
        }
        forgotten
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
        // Before the new change, never after it. A retirement every matched
        // reader already has goes now; the one `dispose` is about to write
        // must survive until it has been sent, because the SPDP departure
        // announces it straight out of the history.
        self.sweep_retirements();
        let sequence_number = self.last_change.next();
        let change = CacheChange::at(sequence_number, payload, now)
            .with_kind(kind)
            .with_instance(instance);
        let change = match source_timestamp {
            Some(timestamp) => change.with_source_timestamp(timestamp),
            None => change,
        };
        let evicted = self.cache.insert(change)?;
        for removal in evicted {
            self.retired.remove(&removal.sequence_number());
        }
        self.last_change = sequence_number;
        Ok(sequence_number)
    }

    /// Drop one sample from the history, whatever the policies say.
    ///
    /// The application-level `dispose` path, and the hook a test uses to
    /// create a hole the writer must `GAP` rather than resend. Returns
    /// whether the sample was there.
    pub fn forget(&mut self, sequence_number: SequenceNumber) -> bool {
        self.retired.remove(&sequence_number);
        self.cache.remove(sequence_number).is_some()
    }

    /// Drop everything the lifespan has aged out.
    ///
    /// Returns the sequence numbers that expired, which the next
    /// [`produce`](Self::produce) will `GAP`.
    pub fn expire(&mut self, now: Instant) -> Vec<SequenceNumber> {
        let expired: Vec<SequenceNumber> = self
            .cache
            .expire(now)
            .iter()
            .map(|removal| removal.sequence_number())
            .collect();
        for number in &expired {
            self.retired.remove(number);
        }
        expired
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
    /// The one change that promise does not cover is a retirement, which
    /// [`dispose`](Self::dispose) writes: it ends its instance, and a reader
    /// that has not appeared yet is owed nothing about an instance that was
    /// gone before it arrived. The writer drops those itself, under every
    /// history and durability, as soon as every matched reader has them;
    /// this method neither waits for nor counts them.
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
        self.retired.retain(|number| *number > watermark);
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

    /// The highest sequence number every active matched reader, and every
    /// lapsed one, is finished with, or `None` when there is neither.
    ///
    /// Wider than [`acked_by_all`](Self::acked_by_all), which counts the
    /// reliable readers only. A best-effort reader never acknowledges, but it
    /// is still owed one copy of a retirement, and its watermark moves only
    /// once [`produce`](Self::produce) has put that copy on the wire. A
    /// lapsed reader counts with what it had acknowledged when it lapsed: it
    /// is owed every retirement since (see [`Lapse`]).
    fn settled_through(&self) -> Option<SequenceNumber> {
        let matched = self
            .matched
            .values()
            .filter(|proxy| proxy.is_active())
            .map(ReaderProxy::acked_through);
        let lapsed = self.lapsed.values().map(|lapse| lapse.acked_through);
        matched.chain(lapsed).min()
    }

    /// Drop every retirement each matched reader already has, together with
    /// any older change of its instance the history still holds.
    ///
    /// "Has" is [`settled_through`](Self::settled_through): acknowledged by a
    /// reliable reader, sent to a best-effort one, and acknowledged before it
    /// lapsed by a lapsed one. With no reader matched and none lapsed, nobody
    /// is owed anything and every retirement goes. A retirement some matched
    /// or lapsed reader is still missing stays, whatever else happens:
    /// dropping it would `GAP` that reader past the one change that says the
    /// entity is gone, and the reader would keep the entity for as long as
    /// this participant lives.
    ///
    /// The older changes go too because a `KEEP_LAST` depth above one keeps
    /// an instance's last announcements beside its retirement, and they are
    /// owed to nobody either. Each is below the retirement, so below the
    /// watermark: every reader matched now already has it.
    ///
    /// Returns how many changes were dropped.
    fn sweep_retirements(&mut self) -> usize {
        let Some(&oldest) = self.retired.first() else {
            return 0;
        };
        let watermark = self.settled_through();
        if watermark.is_some_and(|through| oldest > through) {
            return 0;
        }
        // The newest settled retirement of each instance. The set pops in
        // ascending order, so the last one filed for an instance is it.
        let mut settled: BTreeMap<InstanceHandle, SequenceNumber> = BTreeMap::new();
        while let Some(&number) = self.retired.first() {
            if watermark.is_some_and(|through| number > through) {
                break;
            }
            self.retired.pop_first();
            if let Some(change) = self.cache.get(number) {
                settled.insert(change.instance, number);
            }
        }
        if settled.is_empty() {
            return 0;
        }
        let doomed: Vec<SequenceNumber> = self
            .cache
            .iter()
            .filter(|change| {
                settled
                    .get(&change.instance)
                    .is_some_and(|through| change.sequence_number <= *through)
            })
            .map(|change| change.sequence_number)
            .collect();
        for number in &doomed {
            self.cache.remove(*number);
        }
        doomed.len()
    }

    /// Apply an `ACKNACK` from a matched reader.
    ///
    /// Returns `true` when the submessage was fresh and applied. The repairs
    /// it asks for go out on the next [`produce`](Self::produce).
    pub fn on_acknack(&mut self, reader: Guid, acknack: &crate::messages::AckNack) -> bool {
        let first_available = self.first_available();
        let applied = match self.matched.get_mut(&reader) {
            None => false,
            Some(proxy) => {
                let applied = proxy.accept_acknack(acknack);
                if applied {
                    proxy.drop_requested_below(first_available);
                }
                applied
            }
        };
        if applied {
            // The watermark this reader just moved may be the lowest one.
            self.sweep_retirements();
        }
        applied
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
        // After the readers, because a best-effort reader's watermark moves
        // only once its copy is on the wire; before the heartbeat, so the
        // window it announces is the history as it now stands.
        self.sweep_retirements();
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

    /// Everything for one reader: [`plan_service`](Self::plan_service)'s
    /// plan, on the wire.
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

        // A sample at or below the reader's acknowledged watermark is never
        // sent again, whatever the frontier says: an ACKNACK that
        // acknowledges through 2 proves the reader has 1 and 2, even if this
        // writer never sent them (a second writer, or a replay, may have).
        let acked = proxy.acked_through();
        let floor = proxy.highest_sent().max(acked);
        let repairs: Vec<SequenceNumber> = proxy
            .requested()
            .filter(|number| *number > acked)
            .take(MAX_SAMPLES_PER_PRODUCE)
            .collect();
        let service = self.plan_service(floor, &repairs);

        let mut outbound =
            self.datagrams_for(&service.samples, reader_id, reader_prefix, &locators)?;
        if reliable {
            // A best-effort reader is never sent a GAP, nor prompted: it
            // cannot ask for anything, so there is nothing to answer.
            outbound.extend(self.gap_datagrams(
                &service.holes,
                reader_id,
                reader_prefix,
                &locators,
            )?);
            if service
                .repaired_through
                .is_some_and(|through| through < floor)
            {
                // The repair stopped short of what this reader had already
                // been served, and what lies between may have been lost with
                // it: the reader could not ask, because an ACKNACK's bitmap
                // reaches no further than 256 numbers past its base. Ask it
                // now, rather than a heartbeat period from now — a late
                // joiner whose first replay went nowhere is repaired at the
                // pace of round trips.
                outbound.push(self.heartbeat_to(reader_id, reader_prefix, &locators)?);
            }
        }

        if let Some(proxy) = self.matched.get_mut(&reader) {
            for number in &service.samples {
                proxy.forget_requested(*number);
            }
            for (first, last) in &service.holes {
                proxy.forget_requested_within(*first, *last);
            }
            proxy.serve_through(service.served_through);
            if !reliable {
                // Nobody will ever acknowledge, so the watermark is the
                // frontier: everything below it went out once, as a sample
                // or as a hole a best-effort reader is simply not told
                // about. Without this a KEEP_ALL best-effort writer would
                // never release a sample, and a best-effort late joiner
                // would hold back every retirement the writer is waiting to
                // sweep.
                proxy.assume_acked_through(proxy.highest_sent());
            }
        }
        let _ = now;
        Ok(outbound)
    }

    /// Decide what one `produce` serves a reader whose frontier is `floor`
    /// and who asked for `repairs` again.
    ///
    /// **Repairs** first. A held one goes again. One the history no longer
    /// holds is answered with the whole run of holes it starts, through the
    /// change after it or [`last_change`](Self::last_change): the reader
    /// asking for the start of a run lost the `GAP` for all of it, and an
    /// answer as narrow as its bitmap would cost it one round trip per 256
    /// numbers. A number above `last_change` has not been written, and is
    /// never `GAP`ped.
    ///
    /// **Then everything above `floor`**: the held changes, oldest first,
    /// while the [`MAX_SAMPLES_PER_PRODUCE`] budget lasts, each preceded by
    /// the run of holes before it; and, once the history has nothing held
    /// above the last change taken, the run from there through
    /// `last_change`.
    ///
    /// The frontier moves to the last number that walk covered and no
    /// further. A repair above it is sent but does not move it, or the
    /// numbers between the two would never be pushed at all.
    fn plan_service(&self, floor: SequenceNumber, repairs: &[SequenceNumber]) -> Service {
        let mut samples: BTreeSet<SequenceNumber> = BTreeSet::new();
        let mut holes: Vec<(SequenceNumber, SequenceNumber)> = Vec::new();
        let mut repaired_through: Option<SequenceNumber> = None;
        for &number in repairs {
            if number > self.last_change {
                continue;
            }
            let named = if self.cache.contains(number) {
                samples.insert(number);
                number
            } else {
                let last = self.end_of_hole(number);
                holes.push((number, last));
                last
            };
            repaired_through = Some(repaired_through.map_or(named, |through| through.max(named)));
        }

        let mut served_through = floor;
        let mut held = self.cache.held_after(floor).peekable();
        while let Some(&number) = held.peek() {
            if samples.len() >= MAX_SAMPLES_PER_PRODUCE && !samples.contains(&number) {
                break;
            }
            if number > served_through.next() {
                holes.push((served_through.next(), number.previous()));
            }
            samples.insert(number);
            served_through = number;
            held.next();
        }
        if held.peek().is_none() && served_through < self.last_change {
            holes.push((served_through.next(), self.last_change));
            served_through = self.last_change;
        }

        Service {
            samples: samples.into_iter().collect(),
            holes: merge_runs(holes),
            served_through,
            repaired_through,
        }
    }

    /// The last number of the run of holes that `number` — which the history
    /// no longer holds — belongs to: one below the next change the history
    /// holds, or [`last_change`](Self::last_change) when it holds none after
    /// it.
    fn end_of_hole(&self, number: SequenceNumber) -> SequenceNumber {
        self.cache
            .next_held_after(number)
            .map_or(self.last_change, SequenceNumber::previous)
            .min(self.last_change)
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

    /// The `GAP`s that tell a reader `holes` are never coming, packed into
    /// as few datagrams as the budget allows.
    ///
    /// [`gaps_naming`] decides what each `GAP` says; this only puts them on
    /// the wire.
    fn gap_datagrams(
        &self,
        holes: &[(SequenceNumber, SequenceNumber)],
        reader_id: EntityId,
        reader_prefix: crate::structure::GuidPrefix,
        locators: &[Locator],
    ) -> BehaviorResult<Vec<Outbound>> {
        let mut outbound = Vec::new();
        if holes.is_empty() {
            return Ok(outbound);
        }
        let mut builder = self.new_builder(reader_prefix);
        for gap in gaps_naming(holes, reader_id, self.config.guid.entity_id)? {
            gap.validate()?;
            let gap = Submessage::from(gap);
            if !builder.fits(&gap)
                && let Some(message) = Self::flush(&mut builder, self.header(), reader_prefix)
            {
                outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
            }
            builder.push(gap);
        }
        if let Some(message) = Self::flush(&mut builder, self.header(), reader_prefix) {
            outbound.push(Outbound::new(locators.to_vec(), message.encode()?));
        }
        Ok(outbound)
    }

    /// A non-final `HEARTBEAT` to one reader, outside the cadence.
    ///
    /// The prompt [`produce_for`](Self::produce_for) sends after a repair
    /// that stopped short of what the reader had already been served. It
    /// spends a `Count_t` like any other heartbeat and leaves the cadence
    /// alone: every other reader is still owed its heartbeat on time.
    fn heartbeat_to(
        &mut self,
        reader_id: EntityId,
        reader_prefix: crate::structure::GuidPrefix,
        locators: &[Locator],
    ) -> BehaviorResult<Outbound> {
        self.heartbeat_count = self.heartbeat_count.saturating_add(1);
        let heartbeat = Heartbeat::new(
            reader_id,
            self.config.guid.entity_id,
            self.first_available(),
            self.last_change,
            self.heartbeat_count,
        );
        let mut message = Message::new(self.header());
        message.push(InfoDestination::new(reader_prefix));
        message.push(heartbeat);
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
        let header = self.header();
        let writer_id = self.config.guid.entity_id;
        let count = self.heartbeat_count;
        let mut outbound = Vec::new();
        for proxy in self.matched.values_mut() {
            if !proxy.is_reliable() || !proxy.is_active() {
                continue;
            }
            let locators = proxy.locators();
            if locators.is_empty() {
                continue;
            }
            let mut heartbeat =
                Heartbeat::new(proxy.guid().entity_id, writer_id, first, last, count);
            if liveliness {
                heartbeat = heartbeat.asserting_liveliness().finalized();
            } else {
                if proxy.acked_through() >= last {
                    heartbeat = heartbeat.finalized();
                }
                // The periodic heartbeat is the writer's clock for how long
                // a reader has been silent. See `ReaderProxy::accept_acknack`.
                proxy.note_heartbeat();
            }
            let mut message = Message::new(header);
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
    /// instance was disposed of, merely unregistered, or both (§9.6.3.9).
    /// Both are needed — the flag alone cannot tell them apart, and a
    /// receiver that reads only the flag would treat an unregistration as a
    /// disposal.
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
        match status_info_qos(self.status_info(change)) {
            None => data,
            Some(list) => data.with_inline_qos(list),
        }
    }

    /// The `PID_STATUS_INFO` octets a change that is not `Alive` goes out
    /// with.
    ///
    /// Its kind's flags, plus the unregistered flag when the change is a
    /// retirement: [`dispose`](Self::dispose) disposes of an instance *and*
    /// unregisters it, and a [`ChangeKind`] can say only the first. A
    /// receiver that reads the octets back with
    /// [`ChangeKind::from_status_info`] sees a disposal either way.
    fn status_info(&self, change: &CacheChange) -> [u8; 4] {
        let [first, second, third, flags] = change.kind.status_info();
        if self.retired.contains(&change.sequence_number) {
            let [.., unregistered] = ChangeKind::NotAliveUnregistered.status_info();
            [first, second, third, flags | unregistered]
        } else {
            [first, second, third, flags]
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

/// What one `produce` serves one reader: the plan
/// [`RtpsWriter::plan_service`] makes and [`RtpsWriter::produce_for`]
/// carries out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Service {
    /// The held changes to send, ascending.
    samples: Vec<SequenceNumber>,
    /// The runs of numbers the history no longer holds, each `(first, last)`
    /// inclusive: ascending, disjoint and never adjacent.
    holes: Vec<(SequenceNumber, SequenceNumber)>,
    /// The reader's served frontier once this is on the wire.
    served_through: SequenceNumber,
    /// The highest number the answer to the reader's repair requests names,
    /// when it asked for any this writer could answer.
    repaired_through: Option<SequenceNumber>,
}

/// `runs` sorted, with every two that overlap or touch merged into one.
fn merge_runs(
    mut runs: Vec<(SequenceNumber, SequenceNumber)>,
) -> Vec<(SequenceNumber, SequenceNumber)> {
    runs.sort_unstable();
    let mut merged: Vec<(SequenceNumber, SequenceNumber)> = Vec::with_capacity(runs.len());
    for (first, last) in runs {
        match merged.last_mut() {
            Some((_, merged_last)) if first <= merged_last.next() => {
                if last > *merged_last {
                    *merged_last = last;
                }
            }
            _ => merged.push((first, last)),
        }
    }
    merged
}

/// The `GAP`s that name exactly `holes`: every number in them, and no other.
///
/// Exactly, because a `GAP` is binding. Every number it covers is irrelevant
/// from that moment on and the reader never asks for it again, so a `GAP`
/// that covered a held change — widening scattered holes into one run is the
/// obvious simplification that does it — would be silent data loss on a
/// reliable channel.
///
/// `holes` is ascending, disjoint and never adjacent, each `(first, last)`
/// inclusive. A `GAP` names two sets (§8.3.7.4): the run `gapStart` through
/// `gapList.bitmapBase - 1`, of any length, and the bits of a bitmap at most
/// [`MAX_SET_BITS`] wide after it. Each `GAP` here spends its run on one hole
/// — the base is that hole's end plus one, which the history holds or has
/// not written, so the run never reaches a held change — and its bitmap on
/// the holes that start within its reach. A hole the bitmap cannot finish is
/// where the next `GAP` starts, and that one's run takes the rest.
///
/// # Errors
///
/// [`BehaviorError::Wire`] if a bit fell outside its bitmap, which the
/// construction rules out.
fn gaps_naming(
    holes: &[(SequenceNumber, SequenceNumber)],
    reader_id: EntityId,
    writer_id: EntityId,
) -> BehaviorResult<Vec<Gap>> {
    let reach = i64::from(MAX_SET_BITS).saturating_sub(1);
    let mut gaps = Vec::new();
    let mut index = 0_usize;
    let mut resume: Option<SequenceNumber> = None;
    while let Some(&(first, last)) = holes.get(index) {
        let start = resume.take().unwrap_or(first);
        let base = last.next();
        let window_last = base.saturating_add(reach);
        let mut gap_list = SequenceNumberSet::new(base);
        index = index.saturating_add(1);
        while let Some(&(next_first, next_last)) = holes.get(index) {
            if next_first > window_last {
                break;
            }
            // `next_first > base`, because holes never touch, and `stop` is
            // at most `window_last`: every bit lands inside the bitmap, and
            // the loop ends even at `SequenceNumber::MAX`, where `next`
            // saturates.
            let stop = next_last.min(window_last);
            let mut number = next_first;
            loop {
                gap_list.insert(number)?;
                if number >= stop {
                    break;
                }
                number = number.next();
            }
            if next_last > window_last {
                resume = Some(window_last.next());
                break;
            }
            index = index.saturating_add(1);
        }
        gaps.push(Gap::new(reader_id, writer_id, start, gap_list));
    }
    Ok(gaps)
}

/// The inline QoS a disposal or unregistration carries.
///
/// One parameter: `PID_STATUS_INFO`, the four octets given, the last of which
/// holds the disposed and unregistered bits. A live sample needs no status at
/// all, and `RtpsWriter::data_for` never asks for one.
fn status_info_qos(status_info: [u8; 4]) -> Option<ParameterList<'static>> {
    let mut list = ParameterList::new(inline_qos_encoding(Endianness::Little));
    list.push_octets(
        ParameterId::new(astrs_cdr::pid::STATUS_INFO),
        status_info.to_vec(),
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
mod tests;
