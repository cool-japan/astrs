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
//! return `false` for a stale one, and neither mutates when it does.
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

use std::collections::BTreeSet;

use crate::messages::{AckNack, Gap, Heartbeat};
use crate::structure::{Guid, Locator, MAX_SET_BITS, SequenceNumber, SequenceNumberSet};

/// Most sequence numbers a proxy will remember as explicitly requested.
///
/// An `ACKNACK` cannot name more than 256 in one submessage, and a reader
/// that keeps asking for more than a few hundred is one this writer cannot
/// help anyway.
pub const MAX_REQUESTED: usize = 4_096;

/// Most out-of-order sequence numbers a reader will hold per writer.
pub const MAX_TRACKED: usize = 65_536;

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

    /// The highest sequence number this writer has sent to the reader.
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
    /// than the last accepted — changes nothing at all.
    pub fn accept_acknack(&mut self, acknack: &AckNack) -> bool {
        if acknack.count <= self.last_acknack_count {
            return false;
        }
        self.last_acknack_count = acknack.count;
        self.active = true;

        let watermark = acknack.acknowledged_through();
        if watermark > self.acked_through {
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
    pub fn skip_history_through(&mut self, number: SequenceNumber) {
        if number > self.highest_sent {
            self.highest_sent = number;
        }
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
    irrelevant: BTreeSet<SequenceNumber>,
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
            irrelevant: BTreeSet::new(),
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
            || self.irrelevant.contains(&number)
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
        self.irrelevant.contains(&number)
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
            if self.acked_through < heartbeat.first_sn.previous() {
                self.acked_through = heartbeat.first_sn.previous();
                self.received.retain(|number| *number > self.acked_through);
                self.irrelevant
                    .retain(|number| *number > self.acked_through);
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
    /// Returns how many numbers newly became irrelevant.
    pub fn accept_gap(&mut self, gap: &Gap) -> usize {
        self.active = true;
        let mut added = 0_usize;
        for number in gap.irrelevant() {
            if number <= self.acked_through || self.irrelevant.contains(&number) {
                continue;
            }
            if self.irrelevant.len() >= MAX_TRACKED {
                break;
            }
            self.irrelevant.insert(number);
            if number > self.last_available {
                self.last_available = number;
            }
            added = added.saturating_add(1);
        }
        self.advance();
        added
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

    /// Walk the watermark up over everything satisfied.
    fn advance(&mut self) {
        loop {
            let next = self.acked_through.next();
            if self.received.remove(&next) || self.irrelevant.remove(&next) {
                self.acked_through = next;
            } else {
                break;
            }
        }
    }
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
