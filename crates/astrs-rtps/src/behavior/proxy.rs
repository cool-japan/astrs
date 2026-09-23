//! What each side remembers about the other: [`ReaderProxy`] and
//! [`WriterProxy`].
//!
//! A stateful writer keeps one [`ReaderProxy`] per matched reader and a
//! stateful reader keeps one [`WriterProxy`] per matched writer (§8.4.7,
//! §8.4.10). Everything the reliability protocol decides — what to
//! retransmit, what to acknowledge, what to declare irrelevant — is a
//! function of these two structures, so they are pure state with no I/O and
//! no clock.
//!
//! # The `Count_t` rule
//!
//! Both `HEARTBEAT` and `ACKNACK` carry a counter that increments with every
//! submessage the sender emits (§9.4.2.10). UDP reorders, so a submessage
//! whose count is not *greater* than the last one accepted is stale and must
//! be discarded — not merged, not applied out of order, discarded. Applying a
//! stale `ACKNACK` would walk a reader's acknowledged watermark backwards and
//! resend samples it already has; applying a stale `HEARTBEAT` would
//! resurrect a sample the writer has since GAPped.
//!
//! [`ReaderProxy::accept_acknack`] and [`WriterProxy::accept_heartbeat`] each
//! return `false` for a stale one, and neither mutates when it does — with
//! one exception, and it is not a reordering: an `ACKNACK` from a reader
//! that forgot what it acknowledged, arriving after the writer has
//! heartbeated that reader for a long silence (see below).
//!
//! # The acknowledged watermark
//!
//! An `ACKNACK`'s `readerSNState.bitmapBase` means "everything strictly below
//! this has arrived". The proxies keep the equivalent
//! `acked_through = base - 1`, because a watermark that names a sequence
//! number that *has* arrived reads more naturally next to `first_sn` and
//! `last_sn`, which are also inclusive.
//!
//! On the reader side the same watermark is computed rather than received:
//! [`WriterProxy::acked_through`] is the largest `n` such that every sequence
//! number from the writer's `first_sn` through `n` has either arrived or been
//! declared irrelevant by a `GAP`. A `GAP`ped number counts as satisfied —
//! that is the whole point of `GAP`, and a reader that kept nacking one would
//! never make progress.
//!
//! # A reader that forgot
//!
//! A reader's watermark never moves backwards on its own: every `ACKNACK` one
//! [`WriterProxy`] sends acknowledges at least what the one before did. So an
//! `ACKNACK` whose base lies *below* what the reader already acknowledged,
//! carrying a count newer than any accepted, can only come from a reader that
//! lost its state — its participant's lease on the writer's ran out and it
//! wired the writer up again with a fresh proxy, while the writer, which never
//! lost the reader, kept its old one. Ignored, that reader never gets back
//! what it forgot: the watermark says it has it, and every number it asks for
//! is below the watermark. [`ReaderProxy::accept_acknack`] therefore takes
//! such an `ACKNACK` at its word — the watermark and the served frontier go
//! back to what it still has, and the writer serves it everything above as
//! it would a late joiner — but only for a reader the writer owes its history
//! to: one matched `VOLATILE`, on either side, starts at the present however
//! often it rejoins, and replaying what it had already been delivered would
//! deliver it twice.
//!
//! A fresh proxy counts from wherever the old one stopped
//! ([`RtpsReader::match_writer`](crate::behavior::reader::RtpsReader::match_writer)
//! resumes the count), so this crate's readers never trip the `Count_t` rule
//! by rejoining. A reader that restarts its count at one — another stack, or
//! a participant restarted under the same GUID prefix — is heard once the
//! writer has sent it ten periodic heartbeats since it last accepted an
//! `ACKNACK` from it: a stale submessage is one UDP reordered, and ordinary
//! reordering does not reach that many heartbeat periods. One that does is
//! taken as a restart, which costs a re-push of what the reader already has
//! and nothing else.
//!
//! # Irrelevant numbers are runs
//!
//! A `GAP`'s contiguous part, `gapStart` through `gapList.bitmapBase - 1`,
//! has no length limit (§8.3.7.4), and a writer whose history lost a hundred
//! thousand numbers between two changes says so in one submessage. The
//! reader keeps what it was told as runs, first to last, so taking that
//! submessage in costs a few map operations — never one per number, and never
//! a partial application that leaves the rest of the run to be nacked again.

use std::collections::{BTreeMap, BTreeSet};

use crate::messages::{AckNack, Gap, Heartbeat};
use crate::structure::{Guid, Locator, MAX_SET_BITS, SequenceNumber, SequenceNumberSet};

/// Most sequence numbers a proxy will remember as explicitly requested.
///
/// An `ACKNACK` cannot name more than 256 in one submessage, and a reader
/// that keeps asking for more than a few hundred is one this writer cannot
/// help anyway.
pub const MAX_REQUESTED: usize = 4_096;

/// Most out-of-order sequence numbers a reader will hold per writer.
///
/// It bounds two things, each on its own: the samples waiting above a hole,
/// and the runs of numbers a `GAP` declared irrelevant above one. A run
/// counts once whatever its length.
pub const MAX_TRACKED: usize = 65_536;

/// Cadence heartbeats a writer sends a reader, with no `ACKNACK` of it
/// accepted in between, before an `ACKNACK` with a stale count that shows the
/// reader forgot what it acknowledged is taken all the same.
///
/// Two seconds at the default 200 ms heartbeat period. The `Count_t` rule
/// exists because UDP reorders, and ordinary reordering does not put a
/// datagram behind one sent that many heartbeat periods later; a reader that
/// restarted its count, on the other hand, is silent from the writer's point
/// of view for as long as its count is below the last one accepted. The
/// module docs, which cannot link here, state the number: keep them in step.
pub(crate) const RESTART_AFTER_HEARTBEATS: u32 = 10;

/// What a stateful writer remembers about one matched reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderProxy {
    guid: Guid,
    unicast: Vec<Locator>,
    multicast: Vec<Locator>,
    reliable: bool,
    expects_inline_qos: bool,
    wants_history: bool,
    acked_through: SequenceNumber,
    highest_sent: SequenceNumber,
    requested: BTreeSet<SequenceNumber>,
    last_acknack_count: i32,
    active: bool,
    /// Whether the writer owes this reader its history: true until
    /// [`skip_history_through`](Self::skip_history_through) starts the
    /// reader at the present. Only a reader owed the history is served it
    /// again when it shows it forgot it — see the module docs.
    owed_history: bool,
    /// Cadence heartbeats sent to this reader since the writer last
    /// accepted one of its `ACKNACK`s. See [`RESTART_AFTER_HEARTBEATS`].
    heartbeats_unanswered: u32,
}

impl ReaderProxy {
    /// A proxy for a reader that has not acknowledged anything yet.
    #[must_use]
    pub fn new(guid: Guid, locators: Vec<Locator>, reliable: bool) -> Self {
        Self {
            guid,
            unicast: locators,
            multicast: Vec::new(),
            reliable,
            expects_inline_qos: false,
            wants_history: true,
            acked_through: SequenceNumber::ZERO,
            highest_sent: SequenceNumber::ZERO,
            requested: BTreeSet::new(),
            last_acknack_count: i32::MIN,
            active: true,
            owed_history: true,
            heartbeats_unanswered: 0,
        }
    }

    /// Add multicast locators.
    #[must_use]
    pub fn with_multicast(mut self, multicast: Vec<Locator>) -> Self {
        self.multicast = multicast;
        self
    }

    /// Record that the reader wants inline QoS on every sample.
    #[must_use]
    pub const fn expecting_inline_qos(mut self, expects: bool) -> Self {
        self.expects_inline_qos = expects;
        self
    }

    /// Record whether the reader asked for the writer's pre-join history.
    ///
    /// This is the reader's half of `DURABILITY`, and it is not the same
    /// question as the writer's. DDS puts the decision on *both* endpoints:
    /// the writer's policy says whether the samples are still there to be
    /// replayed, and the reader's says whether it wants them. A
    /// `TRANSIENT_LOCAL` writer matched with a `VOLATILE` reader keeps its
    /// history — the next reader may want it — but must start *that* reader
    /// at the present, exactly as a `VOLATILE` writer would.
    ///
    /// Defaults to `true`, which is right for every builtin endpoint (SPDP
    /// and SEDP readers are all `TRANSIENT_LOCAL`) and leaves the writer's
    /// own policy as the only gate when a caller says nothing.
    #[must_use]
    pub const fn wanting_history(mut self, wants: bool) -> Self {
        self.wants_history = wants;
        self
    }

    /// The reader's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.guid
    }

    /// True when the reader asked for reliable delivery.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        self.reliable
    }

    /// True when the reader wants inline QoS.
    #[must_use]
    pub const fn expects_inline_qos(&self) -> bool {
        self.expects_inline_qos
    }

    /// True when the reader requested `TRANSIENT_LOCAL` and is therefore
    /// entitled to whatever the writer wrote before it appeared.
    ///
    /// See [`wanting_history`](Self::wanting_history).
    #[must_use]
    pub const fn wants_history(&self) -> bool {
        self.wants_history
    }

    /// True while the reader is believed to be alive.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Mark the reader as gone; a writer stops sending to an inactive proxy.
    pub const fn deactivate(&mut self) {
        self.active = false;
    }

    /// Where to send this reader's traffic, unicast first.
    #[must_use]
    pub fn locators(&self) -> Vec<Locator> {
        let mut locators = self.unicast.clone();
        locators.extend(self.multicast.iter().copied());
        locators
    }

    /// Replace the locators, as a fresh SEDP announcement may.
    pub fn set_locators(&mut self, unicast: Vec<Locator>, multicast: Vec<Locator>) {
        self.unicast = unicast;
        self.multicast = multicast;
    }

    /// The highest sequence number the reader has acknowledged, inclusive.
    #[must_use]
    pub const fn acked_through(&self) -> SequenceNumber {
        self.acked_through
    }

    /// The highest sequence number this writer has served the reader.
    ///
    /// Served means put on the wire once, as a `DATA` or as a `GAP` saying
    /// the number is gone: the writer's frontier for the reader. Everything
    /// between the acknowledged watermark and here has gone out at least
    /// once, so a writer never pushes it again unasked; what the reader lost
    /// on the way it asks for with an `ACKNACK`, and gets as a repair.
    #[must_use]
    pub const fn highest_sent(&self) -> SequenceNumber {
        self.highest_sent
    }

    /// True when `number` is at or below the acknowledged watermark.
    #[must_use]
    pub fn has_acked(&self, number: SequenceNumber) -> bool {
        number <= self.acked_through
    }

    /// Note that `number` has been put on the wire for this reader.
    pub fn record_sent(&mut self, number: SequenceNumber) {
        if number > self.highest_sent {
            self.highest_sent = number;
        }
        self.requested.remove(&number);
    }

    /// The sequence numbers this reader has explicitly asked to have again.
    pub fn requested(&self) -> impl Iterator<Item = SequenceNumber> + '_ {
        self.requested.iter().copied()
    }

    /// How many retransmissions are outstanding.
    #[must_use]
    pub fn requested_count(&self) -> usize {
        self.requested.len()
    }

    /// True when the reader has asked for nothing beyond what it has.
    #[must_use]
    pub fn is_satisfied(&self, through: SequenceNumber) -> bool {
        self.requested.is_empty() && self.acked_through >= through
    }

    /// Apply an `ACKNACK`, unless it is stale.
    ///
    /// Returns `true` when it was applied. A stale one — a count not greater
    /// than the last accepted — changes nothing at all, save the one case the
    /// last paragraph below describes: a reader that forgot, heard after a
    /// long silence.
    ///
    /// # A reader that forgot
    ///
    /// An `ACKNACK` that acknowledges *less* than the reader already had is
    /// a reader that lost its state (see the module docs). When the writer
    /// owes this reader its history, the watermark and the served frontier
    /// go back to the new base, and the writer serves everything above it
    /// again, as it would a late joiner; otherwise nothing it already has is
    /// replayed and the watermark stays where it is. Such an `ACKNACK` is
    /// taken even with a stale count once the writer has sent the reader a
    /// number of periodic heartbeats without accepting one from it — the
    /// module docs say how many — which is how a reader that restarted its
    /// count is heard.
    pub fn accept_acknack(&mut self, acknack: &AckNack) -> bool {
        let watermark = acknack.acknowledged_through();
        let forgot = self.has_forgotten(watermark);
        let fresh = acknack.count > self.last_acknack_count;
        let restarted = forgot && self.heartbeats_unanswered >= RESTART_AFTER_HEARTBEATS;
        if !fresh && !restarted {
            return false;
        }
        self.last_acknack_count = acknack.count;
        self.heartbeats_unanswered = 0;
        self.active = true;

        if forgot {
            // Everything above what it still has is owed to it again. The
            // frontier goes back too, or the writer would wait for the
            // reader to ask for each number instead of pushing them.
            self.acked_through = watermark;
            if self.highest_sent > watermark {
                self.highest_sent = watermark;
            }
        } else if watermark > self.acked_through {
            self.acked_through = watermark;
            self.requested.retain(|number| *number > watermark);
        }
        for number in acknack.missing() {
            if self.requested.len() >= MAX_REQUESTED {
                break;
            }
            if number > self.acked_through {
                self.requested.insert(number);
            }
        }
        true
    }

    /// True when an `ACKNACK` acknowledging through `watermark` shows a
    /// reader that no longer has what it acknowledged before, and is owed it
    /// again.
    ///
    /// Reliable readers only: a best-effort one never acknowledges, and its
    /// watermark is the writer's own bookkeeping. And only a reader owed the
    /// history — see [`accept_acknack`](Self::accept_acknack).
    ///
    /// Never below zero. §8.3.7.1.3 wants the base at one or more, and
    /// decoding does not check it; a watermark wound back to minus one would
    /// have the writer name sequence number zero in a `GAP`, which fails
    /// validation and would take the writer's whole `produce` — the
    /// participant's cadence — down with it. An `ACKNACK` with such a base is
    /// applied as it always was: it moves nothing back.
    fn has_forgotten(&self, watermark: SequenceNumber) -> bool {
        self.reliable
            && self.owed_history
            && watermark >= SequenceNumber::ZERO
            && watermark < self.acked_through
    }

    /// Note that the cadence sent this reader a `HEARTBEAT`.
    ///
    /// Counts towards [`RESTART_AFTER_HEARTBEATS`]. The writer calls it for
    /// the periodic heartbeat only: a prompt follows an `ACKNACK` by a round
    /// trip, and a liveliness assertion comes at whatever rate the
    /// application asserts, so neither measures how long the reader has been
    /// silent.
    pub(crate) const fn note_heartbeat(&mut self) {
        self.heartbeats_unanswered = self.heartbeats_unanswered.saturating_add(1);
    }

    /// Ask for `number` again.
    ///
    /// What a `NACK_FRAG` turns into: fragment-level repair is served by
    /// resending the whole sample, so a fragment request becomes a sample
    /// request. Numbers at or below the watermark are ignored — a reader
    /// that acknowledged a sample cannot then ask for a fragment of it.
    pub fn request_resend(&mut self, number: SequenceNumber) {
        if number > self.acked_through && self.requested.len() < MAX_REQUESTED {
            self.requested.insert(number);
        }
    }

    /// Drop one outstanding retransmission request.
    ///
    /// Called when the writer answers with a `GAP` rather than the sample:
    /// the reader will stop asking, so the writer must stop remembering.
    pub fn forget_requested(&mut self, number: SequenceNumber) {
        self.requested.remove(&number);
    }

    /// Drop every outstanding retransmission request in `first ..= last`.
    ///
    /// What one `GAP` naming that whole run answers. Costs the requests it
    /// drops, never the length of the run.
    pub(crate) fn forget_requested_within(&mut self, first: SequenceNumber, last: SequenceNumber) {
        if first > last {
            // `BTreeSet::range` panics on an inverted range; an empty one
            // answers nothing.
            return;
        }
        let answered: Vec<SequenceNumber> = self.requested.range(first..=last).copied().collect();
        for number in answered {
            self.requested.remove(&number);
        }
    }

    /// Move the served frontier up to `number`; see
    /// [`highest_sent`](Self::highest_sent).
    ///
    /// Only ever forward, and only as far as the writer has served *every*
    /// number below: a frontier that jumped over one it never sent would
    /// leave that one to the reader's next `ACKNACK`, one heartbeat later.
    pub(crate) fn serve_through(&mut self, number: SequenceNumber) {
        if number > self.highest_sent {
            self.highest_sent = number;
        }
    }

    /// Forget every outstanding retransmission request.
    ///
    /// What a writer does after a `GAP` tells the reader those samples are
    /// never coming.
    pub fn clear_requested(&mut self) {
        self.requested.clear();
    }

    /// Drop retransmission requests for numbers the writer no longer holds.
    pub fn drop_requested_below(&mut self, first_available: SequenceNumber) {
        self.requested.retain(|number| *number >= first_available);
    }

    /// Treat everything through `number` as already sent *and* acknowledged.
    ///
    /// What a `VOLATILE` writer does to a reader it has just matched: DDS
    /// says a late joiner gets nothing that came before it, and the way RTPS
    /// expresses that is a proxy whose watermarks start at the writer's
    /// current `lastChangeSequenceNumber` rather than at zero. Without this a
    /// `VOLATILE` writer replays its whole `KEEP_LAST` history to every new
    /// reader and is indistinguishable from a `TRANSIENT_LOCAL` one.
    ///
    /// It also marks the reader as one the writer does not owe its history,
    /// so an `ACKNACK` from a reader that forgot what it was delivered never
    /// winds the watermark back at all — see
    /// [`accept_acknack`](Self::accept_acknack).
    pub fn skip_history_through(&mut self, number: SequenceNumber) {
        if number > self.highest_sent {
            self.highest_sent = number;
        }
        self.owed_history = false;
        self.assume_acked_through(number);
    }

    /// Treat everything through `number` as acknowledged.
    ///
    /// A best-effort reader never sends an `ACKNACK`, so the writer moves the
    /// watermark itself once the sample is on the wire; otherwise a
    /// `KEEP_ALL` history would fill up behind a reader that will never
    /// answer.
    pub fn assume_acked_through(&mut self, number: SequenceNumber) {
        if number > self.acked_through {
            self.acked_through = number;
            self.requested.retain(|held| *held > number);
        }
    }
}

/// What a stateful reader remembers about one matched writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterProxy {
    guid: Guid,
    unicast: Vec<Locator>,
    multicast: Vec<Locator>,
    reliable: bool,
    first_available: SequenceNumber,
    last_available: SequenceNumber,
    received: BTreeSet<SequenceNumber>,
    /// The runs a `GAP` declared irrelevant above the watermark: first to
    /// last, both inclusive, disjoint and never adjacent — a run that
    /// touches another is merged into it. See the module docs.
    irrelevant: BTreeMap<SequenceNumber, SequenceNumber>,
    acked_through: SequenceNumber,
    acknack_count: i32,
    last_heartbeat_count: i32,
    heartbeat_pending: bool,
    active: bool,
}

impl WriterProxy {
    /// A proxy for a writer nothing has been heard from yet.
    #[must_use]
    pub fn new(guid: Guid, locators: Vec<Locator>, reliable: bool) -> Self {
        Self {
            guid,
            unicast: locators,
            multicast: Vec::new(),
            reliable,
            first_available: SequenceNumber::FIRST,
            last_available: SequenceNumber::ZERO,
            received: BTreeSet::new(),
            irrelevant: BTreeMap::new(),
            acked_through: SequenceNumber::ZERO,
            acknack_count: 0,
            last_heartbeat_count: i32::MIN,
            heartbeat_pending: false,
            active: true,
        }
    }

    /// Add multicast locators.
    #[must_use]
    pub fn with_multicast(mut self, multicast: Vec<Locator>) -> Self {
        self.multicast = multicast;
        self
    }

    /// The writer's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.guid
    }

    /// True when the writer offers reliable delivery.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        self.reliable
    }

    /// True while the writer is believed to be alive.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Mark the writer as gone.
    pub const fn deactivate(&mut self) {
        self.active = false;
    }

    /// Where to send this writer's `ACKNACK`s, unicast first.
    #[must_use]
    pub fn locators(&self) -> Vec<Locator> {
        let mut locators = self.unicast.clone();
        locators.extend(self.multicast.iter().copied());
        locators
    }

    /// Replace the locators.
    pub fn set_locators(&mut self, unicast: Vec<Locator>, multicast: Vec<Locator>) {
        self.unicast = unicast;
        self.multicast = multicast;
    }

    /// The lowest sequence number the writer still holds.
    #[must_use]
    pub const fn first_available(&self) -> SequenceNumber {
        self.first_available
    }

    /// The highest sequence number the writer has ever written.
    #[must_use]
    pub const fn last_available(&self) -> SequenceNumber {
        self.last_available
    }

    /// The highest sequence number every one below which has been satisfied.
    #[must_use]
    pub const fn acked_through(&self) -> SequenceNumber {
        self.acked_through
    }

    /// True when `number` has arrived or been declared irrelevant, so the
    /// reader must not ask for it again.
    ///
    /// Below the watermark the two are indistinguishable, and deliberately
    /// so: once a number is behind the watermark the reader has either
    /// delivered it or been told it never existed, and in both cases it is
    /// finished with it. Keeping the distinction would mean holding every
    /// sequence number a writer ever sent, for a query nothing asks.
    #[must_use]
    pub fn is_satisfied(&self, number: SequenceNumber) -> bool {
        number <= self.acked_through
            || self.received.contains(&number)
            || self.is_pending_irrelevant(number)
    }

    /// True when `number` arrived out of order and is waiting above the
    /// watermark for the hole below it to close.
    #[must_use]
    pub fn is_pending(&self, number: SequenceNumber) -> bool {
        self.received.contains(&number)
    }

    /// True when `number` was declared irrelevant and is still above the
    /// watermark.
    #[must_use]
    pub fn is_pending_irrelevant(&self, number: SequenceNumber) -> bool {
        // The one run that can hold `number` is the last one starting at or
        // below it.
        self.irrelevant
            .range(..=number)
            .next_back()
            .is_some_and(|(_, last)| number <= *last)
    }

    /// How many out-of-order arrivals are waiting above the watermark.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.received.len()
    }

    /// True when everything the writer has announced has been satisfied.
    #[must_use]
    pub fn is_caught_up(&self) -> bool {
        self.acked_through >= self.last_available
    }

    /// True when a `HEARTBEAT` has arrived that has not been answered.
    #[must_use]
    pub const fn heartbeat_pending(&self) -> bool {
        self.heartbeat_pending
    }

    /// Record that `number` arrived.
    ///
    /// Returns `true` when it is new — the caller should deliver it — and
    /// `false` when it is a duplicate, out of order on a best-effort link, or
    /// already below the watermark.
    ///
    /// The acceptance rule is the proxy's own reliability, not the caller's
    /// (§8.4.10.4 versus §8.4.10.5). A best-effort proxy takes only what is
    /// newer than everything it has seen and moves its watermark straight to
    /// it, because nothing will ever repair the numbers it skipped; a
    /// reliable proxy takes anything not already satisfied and lets the
    /// watermark catch up when the hole closes.
    pub fn accept_data(&mut self, number: SequenceNumber) -> bool {
        self.active = true;
        if !self.reliable {
            if number <= self.acked_through {
                return false;
            }
            self.acked_through = number;
            self.received.clear();
            self.irrelevant.clear();
            if number > self.last_available {
                self.last_available = number;
            }
            return true;
        }
        if number <= self.acked_through || self.received.contains(&number) {
            return false;
        }
        if self.received.len() >= MAX_TRACKED {
            return false;
        }
        self.received.insert(number);
        if number > self.last_available {
            self.last_available = number;
        }
        self.advance();
        true
    }

    /// Apply a `HEARTBEAT`, unless it is stale.
    ///
    /// Returns `true` when it was applied. A non-final heartbeat that was
    /// applied leaves an `ACKNACK` pending.
    pub fn accept_heartbeat(&mut self, heartbeat: &Heartbeat) -> bool {
        if heartbeat.count <= self.last_heartbeat_count {
            return false;
        }
        self.last_heartbeat_count = heartbeat.count;
        self.active = true;

        if heartbeat.first_sn > self.first_available {
            self.first_available = heartbeat.first_sn;
            // Everything the writer has dropped is irrelevant by definition.
            // `advance` below forgets whatever the new watermark passed.
            if self.acked_through < heartbeat.first_sn.previous() {
                self.acked_through = heartbeat.first_sn.previous();
            }
        }
        if heartbeat.last_sn > self.last_available {
            self.last_available = heartbeat.last_sn;
        }
        self.advance();
        if !heartbeat.is_final || !self.is_caught_up() {
            self.heartbeat_pending = true;
        }
        true
    }

    /// Apply a `GAP`: every number it names is irrelevant from now on.
    ///
    /// Returns how many numbers newly became irrelevant: those above the
    /// watermark that no earlier `GAP` had already named.
    ///
    /// The contiguous run is taken whole, as one run, however long it is —
    /// the cost is a few map operations, the same for three numbers as for
    /// three billion — and so is every stretch of consecutive bits in the
    /// bitmap. A run that starts at the watermark moves it straight to the
    /// run's end; one that starts above a hole waits there, whole, for the
    /// hole to close.
    pub fn accept_gap(&mut self, gap: &Gap) -> usize {
        self.active = true;
        let mut added = 0_u64;
        let mut highest: Option<SequenceNumber> = None;

        let run_last = gap.contiguous_end().previous();
        if gap.gap_start <= run_last {
            added = added.saturating_add(self.mark_irrelevant(gap.gap_start, run_last));
            highest = Some(run_last);
        }

        let mut stretch: Option<(SequenceNumber, SequenceNumber)> = None;
        for number in gap.gap_list.iter() {
            stretch = match stretch {
                Some((first, last)) if number == last.next() => Some((first, number)),
                Some((first, last)) => {
                    added = added.saturating_add(self.mark_irrelevant(first, last));
                    Some((number, number))
                }
                None => Some((number, number)),
            };
            highest = Some(highest.map_or(number, |seen| seen.max(number)));
        }
        if let Some((first, last)) = stretch {
            added = added.saturating_add(self.mark_irrelevant(first, last));
        }

        if let Some(highest) = highest
            && highest > self.last_available
        {
            self.last_available = highest;
        }
        self.advance();
        usize::try_from(added).unwrap_or(usize::MAX)
    }

    /// Record `first ..= last` as irrelevant, merged with every run it
    /// overlaps or touches.
    ///
    /// Numbers at or below the watermark are finished with already and are
    /// left out. Returns how many numbers no run covered before. Refuses —
    /// returning zero, recording nothing — only when the merge would leave
    /// more than [`MAX_TRACKED`] runs; what it refused the reader still
    /// lacks, so it asks for it again and the writer answers again.
    fn mark_irrelevant(&mut self, first: SequenceNumber, last: SequenceNumber) -> u64 {
        let first = first.max(self.acked_through.next());
        if first > last {
            return 0;
        }
        let mut merged_first = first;
        let mut merged_last = last;
        let mut covered = 0_u64;
        let mut absorbed: Vec<SequenceNumber> = Vec::new();

        // The one run that starts below `first` and reaches it or ends right
        // before it.
        if let Some((&start, &end)) = self.irrelevant.range(..first).next_back()
            && end.next() >= first
        {
            absorbed.push(start);
            merged_first = start;
            merged_last = merged_last.max(end);
            covered = covered.saturating_add(overlap(start, end, first, last));
        }
        // Every run that starts inside `first ..= last + 1`. The range is
        // never inverted: `first <= last <= last.next()`.
        for (&start, &end) in self.irrelevant.range(first..=last.next()) {
            absorbed.push(start);
            merged_last = merged_last.max(end);
            covered = covered.saturating_add(overlap(start, end, first, last));
        }

        let runs_after = self
            .irrelevant
            .len()
            .saturating_sub(absorbed.len())
            .saturating_add(1);
        if runs_after > MAX_TRACKED {
            return 0;
        }
        for start in absorbed {
            self.irrelevant.remove(&start);
        }
        self.irrelevant.insert(merged_first, merged_last);
        span(first, last).saturating_sub(covered)
    }

    /// The set an `ACKNACK` should carry: base one past the watermark, bits
    /// for what is still missing.
    ///
    /// Never wider than [`MAX_SET_BITS`], and never naming a number the
    /// writer has not announced.
    #[must_use]
    pub fn acknack_state(&self) -> SequenceNumberSet {
        let base = self.acked_through.next();
        let mut set = SequenceNumberSet::new(base);
        let end = base
            .value()
            .saturating_add(i64::from(MAX_SET_BITS))
            .min(self.last_available.value().saturating_add(1));
        let mut number = base.value();
        while number < end {
            let candidate = SequenceNumber::new(number);
            if !self.is_satisfied(candidate) {
                // Bounded by construction: `number - base < MAX_SET_BITS`.
                let _ = set.insert(candidate);
            }
            number = number.saturating_add(1);
        }
        set
    }

    /// Build the `ACKNACK` to answer the pending heartbeat with, and clear
    /// the pending flag.
    ///
    /// `is_final` is set when nothing is missing, which tells the writer no
    /// repair is being asked for.
    pub fn take_acknack(&mut self, reader_id: crate::structure::EntityId) -> AckNack {
        self.heartbeat_pending = false;
        self.acknack_count = self.acknack_count.saturating_add(1);
        let state = self.acknack_state();
        let acknack = AckNack::new(reader_id, self.guid.entity_id, state, self.acknack_count);
        if state.is_empty() {
            acknack.finalized()
        } else {
            acknack
        }
    }

    /// Forget that a heartbeat is waiting for an answer.
    ///
    /// A best-effort reader never sends an `ACKNACK`, so the flag would latch
    /// forever if it were left set.
    pub const fn clear_heartbeat_pending(&mut self) {
        self.heartbeat_pending = false;
    }

    /// The `Count_t` the next `ACKNACK` will carry.
    #[must_use]
    pub const fn next_acknack_count(&self) -> i32 {
        self.acknack_count.saturating_add(1)
    }

    /// The `Count_t` of the last `ACKNACK` this proxy built, zero before the
    /// first.
    #[must_use]
    pub(crate) const fn acknack_count(&self) -> i32 {
        self.acknack_count
    }

    /// Count on from `after`: the next `ACKNACK` carries a count above it.
    ///
    /// What a reader does to a proxy for a writer it has matched before. The
    /// writer compares counts per reader, not per proxy, and may still hold
    /// the count the old proxy reached; one that started again at one would
    /// be stale to it until it had caught up (the `Count_t` rule in the
    /// module docs). Never lowers the count.
    pub(crate) const fn resume_acknack_count(&mut self, after: i32) {
        if after > self.acknack_count {
            self.acknack_count = after;
        }
    }

    /// Walk the watermark up over everything satisfied, then forget what it
    /// passed.
    ///
    /// A number can be in both structures at once — a sample that arrived
    /// and that a `GAP` then named — so the walk asks each in turn, and the
    /// run it steps onto may have started below the watermark. Whatever the
    /// walk leaves at or below the watermark is dropped from both: it is
    /// finished with, and left behind it would count against
    /// [`MAX_TRACKED`] for as long as the writer lives.
    fn advance(&mut self) {
        loop {
            let next = self.acked_through.next();
            if self.received.remove(&next) {
                self.acked_through = next;
                continue;
            }
            let run = self
                .irrelevant
                .range(..=next)
                .next_back()
                .map(|(first, last)| (*first, *last));
            match run {
                Some((first, last)) if last >= next => {
                    self.irrelevant.remove(&first);
                    self.acked_through = last;
                }
                _ => break,
            }
        }
        let watermark = self.acked_through;
        while self
            .received
            .first()
            .is_some_and(|number| *number <= watermark)
        {
            self.received.pop_first();
        }
        // No run that starts at or below the watermark reaches past it: the
        // walk would have stepped onto it. So every such run is wholly
        // behind, and goes.
        while self
            .irrelevant
            .first_key_value()
            .is_some_and(|(first, _)| *first <= watermark)
        {
            self.irrelevant.pop_first();
        }
    }
}

/// How many numbers `first ..= last` holds, saturating.
fn span(first: SequenceNumber, last: SequenceNumber) -> u64 {
    last.offset_from(first)
        .map_or(0, |difference| difference.saturating_add(1))
}

/// How many numbers `a_first ..= a_last` and `b_first ..= b_last` share.
fn overlap(
    a_first: SequenceNumber,
    a_last: SequenceNumber,
    b_first: SequenceNumber,
    b_last: SequenceNumber,
) -> u64 {
    span(a_first.max(b_first), a_last.min(b_last))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{EntityId, EntityKind, GuidPrefix, VendorId};
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

    fn number(value: i64) -> SequenceNumber {
        SequenceNumber::new(value)
    }

    fn acknack(base: i64, missing: &[i64], count: i32) -> AckNack {
        let set =
            SequenceNumberSet::from_numbers(number(base), missing.iter().copied().map(number))
                .expect("the test's numbers are in window");
        AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, count)
    }

    fn heartbeat(first: i64, last: i64, count: i32) -> Heartbeat {
        Heartbeat::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            number(first),
            number(last),
            count,
        )
    }

    // ── ReaderProxy ───────────────────────────────────────────────────────

    #[test]
    fn a_fresh_reader_proxy_has_acknowledged_nothing() {
        let proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        assert_eq!(proxy.acked_through(), SequenceNumber::ZERO);
        assert!(!proxy.has_acked(number(1)));
        assert!(proxy.is_reliable());
        assert!(proxy.is_active());
        assert_eq!(proxy.requested_count(), 0);
        assert!(
            proxy.wants_history(),
            "a proxy nobody told otherwise leaves the decision to the writer"
        );
    }

    #[test]
    fn a_reader_proxy_remembers_whether_the_reader_asked_for_history() {
        let volatile = ReaderProxy::new(reader_guid(), Vec::new(), true).wanting_history(false);
        assert!(!volatile.wants_history());
        assert!(
            !volatile.expecting_inline_qos(true).wants_history(),
            "the two builders are independent, in either order"
        );

        let durable = ReaderProxy::new(reader_guid(), Vec::new(), true)
            .expecting_inline_qos(true)
            .wanting_history(true);
        assert!(durable.wants_history());
        assert!(durable.expects_inline_qos());
    }

    #[test]
    fn an_acknack_moves_the_watermark_and_records_the_gaps() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        assert!(proxy.accept_acknack(&acknack(5, &[5, 7], 1)));
        assert_eq!(proxy.acked_through(), number(4));
        assert_eq!(
            proxy.requested().collect::<Vec<_>>(),
            vec![number(5), number(7)]
        );
    }

    #[test]
    fn a_stale_acknack_changes_nothing() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        assert!(proxy.accept_acknack(&acknack(9, &[], 7)));
        assert_eq!(proxy.acked_through(), number(8));

        assert!(
            !proxy.accept_acknack(&acknack(2, &[2], 7)),
            "an equal count is stale"
        );
        assert!(
            !proxy.accept_acknack(&acknack(2, &[2], 3)),
            "a lower count is stale"
        );
        assert_eq!(
            proxy.acked_through(),
            number(8),
            "the watermark must not walk backwards"
        );
        assert_eq!(proxy.requested_count(), 0);
    }

    #[test]
    fn a_later_acknack_forgets_requests_it_has_since_received() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        proxy.accept_acknack(&acknack(3, &[3, 4, 5], 1));
        assert_eq!(proxy.requested_count(), 3);
        proxy.accept_acknack(&acknack(5, &[5], 2));
        assert_eq!(
            proxy.requested().collect::<Vec<_>>(),
            vec![number(5)],
            "3 and 4 arrived, so only 5 is still wanted"
        );
    }

    #[test]
    fn sending_a_sample_clears_its_request() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        proxy.accept_acknack(&acknack(1, &[1, 2], 1));
        proxy.record_sent(number(1));
        assert_eq!(proxy.requested().collect::<Vec<_>>(), vec![number(2)]);
        assert_eq!(proxy.highest_sent(), number(1));
    }

    #[test]
    fn a_best_effort_reader_is_assumed_to_have_everything_sent() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), false);
        proxy.assume_acked_through(number(10));
        assert!(proxy.has_acked(number(10)));
        assert!(proxy.is_satisfied(number(10)));
        proxy.assume_acked_through(number(4));
        assert_eq!(proxy.acked_through(), number(10), "never walks back");
    }

    #[test]
    fn requests_below_the_first_available_are_dropped() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        proxy.accept_acknack(&acknack(1, &[1, 2, 3], 1));
        proxy.drop_requested_below(number(3));
        assert_eq!(proxy.requested().collect::<Vec<_>>(), vec![number(3)]);
        proxy.clear_requested();
        assert_eq!(proxy.requested_count(), 0);
    }

    #[test]
    fn locators_put_unicast_before_multicast() {
        let proxy = ReaderProxy::new(
            reader_guid(),
            vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 1)],
            true,
        )
        .with_multicast(vec![Locator::udpv4(Ipv4Addr::new(239, 255, 0, 1), 7400)]);
        let locators = proxy.locators();
        assert_eq!(locators.len(), 2);
        assert!(locators[0].is_loopback());
        assert!(locators[1].is_multicast());
    }

    // ── A reader that forgot ──────────────────────────────────────────────

    /// The numbers one through `last`.
    fn through(last: i64) -> Vec<i64> {
        (1..=last).collect()
    }

    #[test]
    fn a_reader_that_forgot_is_owed_everything_again() {
        // The writer served the reader 1..=10 and it acknowledged them all.
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        proxy.serve_through(number(10));
        assert!(proxy.accept_acknack(&acknack(11, &[], 1)));
        assert_eq!(proxy.acked_through(), number(10));

        // Then the reader's participant gave up on the writer's and wired it
        // up again with a fresh proxy: a newer count, and nothing received.
        assert!(proxy.accept_acknack(&acknack(1, &through(10), 2)));
        assert_eq!(
            proxy.acked_through(),
            SequenceNumber::ZERO,
            "the watermark goes back to what the reader still has"
        );
        assert_eq!(
            proxy.highest_sent(),
            SequenceNumber::ZERO,
            "and the frontier with it, so the writer pushes everything again \
             rather than waiting to be asked for it 256 numbers at a time"
        );
        assert_eq!(
            proxy
                .requested()
                .map(SequenceNumber::value)
                .collect::<Vec<_>>(),
            through(10)
        );
        assert!(!proxy.is_satisfied(number(10)));
    }

    #[test]
    fn a_reader_started_at_the_present_is_never_wound_back() {
        // A VOLATILE reader, or any reader of a VOLATILE writer, is owed
        // nothing written before it matched. Forgetting what it was delivered
        // since does not make it owed that either: replayed, the application
        // would be handed every sample a second time.
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true).wanting_history(false);
        proxy.skip_history_through(number(10));
        assert!(
            proxy.accept_acknack(&acknack(1, &through(10), 1)),
            "a fresh count is applied"
        );
        assert_eq!(proxy.acked_through(), number(10));
        assert_eq!(proxy.highest_sent(), number(10));
        assert_eq!(proxy.requested_count(), 0, "and nothing is asked for again");
    }

    #[test]
    fn a_best_effort_reader_is_never_wound_back() {
        // Its watermark is the writer's own bookkeeping, not an
        // acknowledgement, so no ACKNACK can contradict it.
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), false);
        proxy.serve_through(number(10));
        proxy.assume_acked_through(number(10));
        assert!(proxy.accept_acknack(&acknack(1, &[1], 1)));
        assert_eq!(proxy.acked_through(), number(10));
        assert_eq!(proxy.highest_sent(), number(10));
    }

    #[test]
    fn a_count_that_restarted_is_heard_after_a_silence_and_only_from_a_reader_that_forgot() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        assert!(proxy.accept_acknack(&acknack(11, &[], 40)));

        // A reader counting from one again — another stack, rejoining —
        // looks exactly like a reordered ACKNACK until the writer has
        // heartbeated it long enough without an answer.
        for _ in 1..RESTART_AFTER_HEARTBEATS {
            proxy.note_heartbeat();
            assert!(
                !proxy.accept_acknack(&acknack(1, &[1, 2], 1)),
                "as far as the writer can tell, reordered"
            );
        }
        assert_eq!(proxy.acked_through(), number(10));
        proxy.note_heartbeat();

        // However long the silence, a stale count that does not show a
        // forgotten state is stale: taking it would reopen the reordering
        // hole the rule exists to close.
        assert!(!proxy.accept_acknack(&acknack(11, &[11], 39)));
        assert_eq!(proxy.requested_count(), 0);

        assert!(proxy.accept_acknack(&acknack(1, &[1, 2], 1)));
        assert_eq!(proxy.acked_through(), SequenceNumber::ZERO);
        assert_eq!(
            proxy
                .requested()
                .map(SequenceNumber::value)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        // The restarted count is the one the reader counts on from.
        assert!(proxy.accept_acknack(&acknack(3, &[], 2)));
        assert_eq!(proxy.acked_through(), number(2));
        assert!(!proxy.accept_acknack(&acknack(1, &[1], 2)), "stale again");
    }

    #[test]
    fn an_accepted_acknack_starts_the_silence_again() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        assert!(proxy.accept_acknack(&acknack(11, &[], 40)));
        for _ in 0..RESTART_AFTER_HEARTBEATS {
            proxy.note_heartbeat();
        }
        assert!(
            proxy.accept_acknack(&acknack(11, &[], 41)),
            "the reader answers after all"
        );
        proxy.note_heartbeat();
        assert!(
            !proxy.accept_acknack(&acknack(1, &[1], 1)),
            "one heartbeat of silence is not enough"
        );
        assert_eq!(proxy.acked_through(), number(10));
    }

    #[test]
    fn a_base_below_one_never_winds_the_watermark_back() {
        // §8.3.7.1.3 wants a base of one or more, and decoding does not
        // check it. Taken as a reader that forgot, a base of zero would wind
        // the watermark to minus one and ask for sequence number zero, which
        // no GAP may name. It is applied as it always was: nothing moves.
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        assert!(proxy.accept_acknack(&acknack(11, &[], 1)));
        let invalid = AckNack::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            SequenceNumberSet::new(SequenceNumber::ZERO),
            2,
        );
        assert!(proxy.accept_acknack(&invalid), "a fresh count, as before");
        assert_eq!(proxy.acked_through(), number(10));
        assert_eq!(proxy.highest_sent(), SequenceNumber::ZERO);
        assert_eq!(proxy.requested_count(), 0);
    }

    #[test]
    fn a_deactivated_proxy_says_so() {
        let mut proxy = ReaderProxy::new(reader_guid(), Vec::new(), true);
        proxy.deactivate();
        assert!(!proxy.is_active());
        proxy.accept_acknack(&acknack(1, &[], 1));
        assert!(proxy.is_active(), "an ACKNACK proves it is back");
    }

    // ── WriterProxy ───────────────────────────────────────────────────────

    #[test]
    fn a_fresh_writer_proxy_expects_sample_one() {
        let proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        assert_eq!(proxy.acked_through(), SequenceNumber::ZERO);
        assert_eq!(proxy.first_available(), SequenceNumber::FIRST);
        assert!(proxy.is_caught_up(), "nothing has been announced yet");
        assert!(!proxy.heartbeat_pending());
    }

    #[test]
    fn in_order_data_walks_the_watermark_forward() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        for value in 1..=5 {
            assert!(proxy.accept_data(number(value)));
        }
        assert_eq!(proxy.acked_through(), number(5));
        assert!(proxy.is_caught_up());
    }

    #[test]
    fn a_hole_stops_the_watermark_but_not_reception() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_data(number(1));
        proxy.accept_data(number(3));
        assert_eq!(proxy.acked_through(), number(1));
        assert!(proxy.is_pending(number(3)));
        assert_eq!(proxy.pending_count(), 1);
        assert!(!proxy.is_satisfied(number(2)));

        proxy.accept_data(number(2));
        assert_eq!(proxy.acked_through(), number(3), "the hole closed");
    }

    #[test]
    fn a_duplicate_is_reported_as_not_new() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        assert!(proxy.accept_data(number(1)));
        assert!(!proxy.accept_data(number(1)));
        assert!(!proxy.accept_data(number(1)), "still not new");
    }

    #[test]
    fn a_gap_satisfies_the_numbers_it_names() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_data(number(1));
        let gap = Gap::contiguous(
            reader_guid().entity_id,
            writer_guid().entity_id,
            number(2),
            number(4),
        );
        assert_eq!(proxy.accept_gap(&gap), 3);
        assert_eq!(proxy.acked_through(), number(4));
        assert!(proxy.is_satisfied(number(3)));
        assert!(
            !proxy.is_pending_irrelevant(number(3)),
            "the watermark swallowed it, so nothing is held above"
        );
    }

    #[test]
    fn a_heartbeat_announces_the_window_and_asks_for_an_answer() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        assert!(proxy.accept_heartbeat(&heartbeat(1, 4, 1)));
        assert_eq!(proxy.last_available(), number(4));
        assert!(proxy.heartbeat_pending());
        assert!(!proxy.is_caught_up());
    }

    #[test]
    fn a_stale_heartbeat_changes_nothing() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_heartbeat(&heartbeat(1, 9, 5));
        assert!(!proxy.accept_heartbeat(&heartbeat(1, 3, 5)));
        assert!(!proxy.accept_heartbeat(&heartbeat(1, 3, 1)));
        assert_eq!(
            proxy.last_available(),
            number(9),
            "the announced window must not shrink from a stale heartbeat"
        );
    }

    #[test]
    fn a_heartbeat_that_moved_first_sn_forgets_what_the_writer_dropped() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_heartbeat(&heartbeat(1, 10, 1));
        assert_eq!(proxy.acked_through(), SequenceNumber::ZERO);

        proxy.accept_heartbeat(&heartbeat(6, 10, 2));
        assert_eq!(
            proxy.acked_through(),
            number(5),
            "1..=5 are gone; asking for them forever would wedge the reader"
        );
        assert!(proxy.is_satisfied(number(3)));
        assert!(!proxy.is_satisfied(number(6)));
    }

    #[test]
    fn the_acknack_state_names_exactly_what_is_missing() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_heartbeat(&heartbeat(1, 5, 1));
        proxy.accept_data(number(1));
        proxy.accept_data(number(2));
        proxy.accept_data(number(5));

        let state = proxy.acknack_state();
        assert_eq!(state.base(), number(3));
        let missing: Vec<i64> = state.iter().map(SequenceNumber::value).collect();
        assert_eq!(missing, vec![3, 4]);
    }

    #[test]
    fn a_caught_up_reader_sends_a_final_empty_acknack() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_heartbeat(&heartbeat(1, 2, 1));
        proxy.accept_data(number(1));
        proxy.accept_data(number(2));

        let acknack = proxy.take_acknack(reader_guid().entity_id);
        assert!(acknack.is_final, "nothing is being asked for");
        assert!(acknack.reader_sn_state.is_empty());
        assert_eq!(acknack.reader_sn_state.base(), number(3));
        assert_eq!(acknack.count, 1);
        assert!(!proxy.heartbeat_pending(), "the answer was sent");
    }

    #[test]
    fn a_reader_with_a_hole_sends_a_non_final_acknack() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_heartbeat(&heartbeat(1, 3, 1));
        proxy.accept_data(number(1));
        proxy.accept_data(number(3));

        let acknack = proxy.take_acknack(reader_guid().entity_id);
        assert!(!acknack.is_final, "a repair is being asked for");
        assert_eq!(
            acknack
                .missing()
                .map(SequenceNumber::value)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(proxy.next_acknack_count(), 2);
    }

    #[test]
    fn the_acknack_window_never_exceeds_the_bitmap_width() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_heartbeat(&heartbeat(1, 100_000, 1));
        let state = proxy.acknack_state();
        assert_eq!(state.base(), number(1));
        assert_eq!(state.num_bits(), MAX_SET_BITS);
        assert_eq!(state.len(), MAX_SET_BITS as usize);
        state
            .validate("test")
            .expect("the set a reader emits must always be valid");
    }

    #[test]
    fn a_final_heartbeat_that_finds_a_hole_still_asks() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_data(number(2));
        assert!(proxy.accept_heartbeat(&heartbeat(1, 2, 1).finalized()));
        assert!(
            proxy.heartbeat_pending(),
            "final means \"no answer needed\" only when nothing is missing"
        );
    }

    #[test]
    fn a_final_heartbeat_with_nothing_missing_needs_no_answer() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_data(number(1));
        assert!(proxy.accept_heartbeat(&heartbeat(1, 1, 1).finalized()));
        assert!(!proxy.heartbeat_pending());
    }

    #[test]
    fn locators_can_be_replaced_when_sedp_announces_again() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        assert!(proxy.locators().is_empty());
        proxy.set_locators(vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 5)], Vec::new());
        assert_eq!(proxy.locators().len(), 1);
        proxy.deactivate();
        assert!(!proxy.is_active());
    }

    fn gap(start: i64, run_end: i64, bits: &[i64]) -> Gap {
        // `run_end` is the run's last number; the bitmap is based one past it.
        let set =
            SequenceNumberSet::from_numbers(number(run_end + 1), bits.iter().copied().map(number))
                .expect("the test's bits are in window");
        Gap::new(
            reader_guid().entity_id,
            writer_guid().entity_id,
            number(start),
            set,
        )
    }

    #[test]
    fn a_run_at_the_watermark_moves_it_in_one_step() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_data(number(1));
        assert_eq!(proxy.accept_gap(&gap(2, 1_000_000, &[])), 999_999);
        assert_eq!(proxy.acked_through(), number(1_000_000));
        assert_eq!(proxy.last_available(), number(1_000_000));
        assert!(proxy.is_caught_up());
    }

    #[test]
    fn a_run_above_a_hole_is_remembered_whole_until_the_hole_closes() {
        // The late joiner whose DATA 1 was lost while the GAP for the
        // hundred thousand numbers after it arrived: the run must wait
        // above the hole in one piece, not in the first MAX_TRACKED numbers
        // of it.
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        assert_eq!(proxy.accept_gap(&gap(2, 100_001, &[])), 100_000);
        assert!(proxy.accept_data(number(100_002)));
        assert_eq!(proxy.acked_through(), SequenceNumber::ZERO);
        for pending in [2, MAX_TRACKED as i64 + 2, 100_001] {
            assert!(proxy.is_pending_irrelevant(number(pending)));
            assert!(proxy.is_satisfied(number(pending)));
        }
        assert!(!proxy.is_satisfied(number(1)));
        assert_eq!(
            proxy
                .acknack_state()
                .iter()
                .map(SequenceNumber::value)
                .collect::<Vec<_>>(),
            vec![1],
            "the one number the reader still lacks"
        );

        assert!(proxy.accept_data(number(1)));
        assert_eq!(
            proxy.acked_through(),
            number(100_002),
            "the hole closed, and the run and the sample above it went with it"
        );
        assert!(!proxy.is_pending_irrelevant(number(50_000)));
        assert!(!proxy.is_pending(number(100_002)));
    }

    #[test]
    fn a_run_of_any_length_is_taken_whole() {
        // The contiguous part of a GAP has no length limit, and a GAP naming
        // 2^62 numbers is valid. It must cost what a GAP naming three does.
        let last = (1_i64 << 62) - 1;
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        let added = proxy.accept_gap(&gap(1, last, &[]));
        assert_eq!(added as u64, last as u64);
        assert_eq!(proxy.acked_through(), number(last));

        // Above a hole, too.
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_gap(&gap(2, last, &[]));
        assert!(proxy.is_pending_irrelevant(number(last)));
        proxy.accept_data(number(1));
        assert_eq!(proxy.acked_through(), number(last));
    }

    #[test]
    fn overlapping_gaps_merge_and_count_only_what_is_new() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        // Nothing has arrived, so every run waits above the hole at 1.
        assert_eq!(proxy.accept_gap(&gap(5, 10, &[])), 6);
        assert_eq!(proxy.accept_gap(&gap(8, 15, &[])), 5, "11..=15 are new");
        assert_eq!(
            proxy.accept_gap(&gap(3, 3, &[16, 17, 20])),
            4,
            "3, 16, 17 and 20"
        );
        for (value, pending) in [
            (2, false),
            (3, true),
            (4, false),
            (5, true),
            (15, true),
            (16, true),
            (17, true),
            (18, false),
            (19, false),
            (20, true),
            (21, false),
        ] {
            assert_eq!(
                proxy.is_pending_irrelevant(number(value)),
                pending,
                "sequence number {value}"
            );
        }
        assert_eq!(proxy.accept_gap(&gap(4, 4, &[])), 1, "4 joins 3 to 5..=17");
        assert_eq!(proxy.accept_gap(&gap(6, 12, &[])), 0, "all named already");

        proxy.accept_data(number(1));
        proxy.accept_data(number(2));
        assert_eq!(proxy.acked_through(), number(17));
        proxy.accept_data(number(18));
        proxy.accept_data(number(19));
        assert_eq!(proxy.acked_through(), number(20));
    }

    #[test]
    fn nothing_is_left_behind_the_watermark() {
        // A sample that arrived and that a GAP then named is in both
        // structures, and the watermark can pass it through either: here
        // through the run, in one step, which never visits 5 on its own. It
        // must be forgotten in both: left in either, it would count against
        // MAX_TRACKED for as long as the writer lives.
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_data(number(5));
        assert_eq!(proxy.accept_gap(&gap(3, 10, &[])), 8);
        proxy.accept_data(number(1));
        proxy.accept_data(number(2));
        assert_eq!(proxy.acked_through(), number(10));
        assert!(
            !proxy.is_pending(number(5)),
            "nothing at or below the watermark is still held as received"
        );
        assert!(!proxy.is_pending_irrelevant(number(5)), "nor as irrelevant");
        assert_eq!(proxy.pending_count(), 0);
    }

    #[test]
    fn a_heartbeat_that_moves_first_sn_forgets_the_runs_it_passes() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.accept_gap(&gap(2, 3, &[]));
        proxy.accept_gap(&gap(5, 10, &[]));
        proxy.accept_gap(&gap(20, 30, &[]));
        proxy.accept_data(number(25));
        proxy.accept_heartbeat(&heartbeat(8, 40, 1));
        assert_eq!(
            proxy.acked_through(),
            number(10),
            "7 by the heartbeat, then through the rest of the run it landed in"
        );
        assert!(
            !proxy.is_pending_irrelevant(number(2)),
            "the run the heartbeat jumped over entirely is forgotten too"
        );
        assert!(!proxy.is_pending_irrelevant(number(9)));
        assert!(proxy.is_pending_irrelevant(number(25)));
        proxy.accept_heartbeat(&heartbeat(26, 40, 2));
        assert_eq!(proxy.acked_through(), number(30));
        assert!(!proxy.is_pending_irrelevant(number(30)));
        assert!(
            !proxy.is_pending(number(25)),
            "a sample inside a run the watermark took whole is forgotten with it"
        );
    }

    #[test]
    fn a_flood_of_scattered_gaps_is_bounded_in_runs() {
        // One run per GAP, none touching another, all above a hole at 1:
        // the cap is on runs, and a GAP past it is refused, not half taken.
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        let tracked = MAX_TRACKED as i64;
        for index in 0..tracked {
            let start = 3 + 2 * index;
            assert_eq!(proxy.accept_gap(&gap(start, start, &[])), 1);
        }
        let beyond = 3 + 2 * tracked;
        assert_eq!(proxy.accept_gap(&gap(beyond, beyond + 10, &[])), 0);
        assert!(!proxy.is_pending_irrelevant(number(beyond)));
        assert_eq!(
            proxy.accept_gap(&gap(4, 4, &[])),
            1,
            "a run that merges two leaves fewer runs, and is taken"
        );
        assert!(proxy.is_pending_irrelevant(number(4)));
    }

    #[test]
    fn a_resumed_count_carries_on_above_the_old_one_and_never_goes_back() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        proxy.resume_acknack_count(41);
        assert!(proxy.accept_heartbeat(&heartbeat(1, 1, 1)));
        assert_eq!(proxy.take_acknack(reader_guid().entity_id).count, 42);
        assert_eq!(proxy.acknack_count(), 42);
        proxy.resume_acknack_count(7);
        assert_eq!(proxy.next_acknack_count(), 43, "a count is never lowered");
    }

    #[test]
    fn a_writer_proxy_will_not_track_an_unbounded_number_of_holes() {
        let mut proxy = WriterProxy::new(writer_guid(), Vec::new(), true);
        // Every even number, so nothing ever closes the first hole.
        for value in (2..=(2 * MAX_TRACKED as i64 + 2)).step_by(2) {
            proxy.accept_data(number(value));
        }
        assert!(proxy.acked_through() == SequenceNumber::ZERO);
        assert!(
            proxy.acknack_state().num_bits() <= MAX_SET_BITS,
            "the emitted set stays bounded regardless"
        );
    }
}
