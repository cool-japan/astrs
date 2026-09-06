//! The history cache: the samples an endpoint is holding, and the QoS that
//! decides how long.
//!
//! RTPS calls it `HistoryCache` and gives it one job (§8.2.2): hold
//! `CacheChange`s, ordered by sequence number, so a writer can retransmit and
//! a reader can reorder. The QoS policies that bound it — `HISTORY`,
//! `RESOURCE_LIMITS`, `LIFESPAN` — are DDS's, and applying them is what turns
//! an unbounded `Vec` into something a robot can run for a week.
//!
//! # Instances
//!
//! `KEEP_LAST n` retains `n` samples *per instance*, not `n` in total. An
//! instance is identified by the key hash of the sample's key fields; a
//! keyless topic — which is every ROS 2 topic — has exactly one instance,
//! [`InstanceHandle::NIL`]. Implementing the per-instance rule even though
//! ROS never exercises it costs one `BTreeMap` lookup and means the cache is
//! correct the day a keyed topic arrives.
//!
//! # What eviction is, and is not
//!
//! Three different things remove a change, and they are not
//! interchangeable:
//!
//! | Cause | Policy | Effect on a reliable reader |
//! |---|---|---|
//! | Depth exceeded | `KEEP_LAST` | The sample is gone; the writer GAPs it |
//! | Lifespan elapsed | `LIFESPAN` | The same |
//! | Acknowledged by everyone | — | Nothing; it was already delivered |
//!
//! The first two produce a sequence number a reader may still be asking for,
//! which is exactly what `GAP` exists to answer. [`HistoryCache::insert`]
//! and [`HistoryCache::expire`] therefore *return* what they dropped rather
//! than dropping it quietly, and the writer turns that list into a `GAP`.
//! A `KEEP_ALL` cache never evicts — it refuses the write instead, with
//! [`BehaviorError::HistoryFull`], which is the DDS `RETCODE_TIMEOUT`
//! contract in a shape a Rust caller can match on.

use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::discovery::qos::{HistoryKind, HistoryQos, ResourceLimitsQos};
use crate::structure::{SequenceNumber, Time};

/// Octets in a DDS key hash (`PID_KEY_HASH`, an MD5 or the key itself).
pub const INSTANCE_HANDLE_LEN: usize = 16;

/// Identifies which instance of a keyed topic a sample belongs to.
///
/// A keyless topic — every ROS 2 topic — uses [`InstanceHandle::NIL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct InstanceHandle([u8; INSTANCE_HANDLE_LEN]);

impl InstanceHandle {
    /// The single instance a keyless topic has.
    pub const NIL: Self = Self([0; INSTANCE_HANDLE_LEN]);

    /// Wrap a sixteen-octet key hash.
    #[must_use]
    pub const fn new(octets: [u8; INSTANCE_HANDLE_LEN]) -> Self {
        Self(octets)
    }

    /// The octets, as `PID_KEY_HASH` carries them.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; INSTANCE_HANDLE_LEN] {
        &self.0
    }

    /// True when this is the keyless topic's single instance.
    #[must_use]
    pub const fn is_nil(&self) -> bool {
        let mut index = 0;
        while index < INSTANCE_HANDLE_LEN {
            if self.0[index] != 0 {
                return false;
            }
            index += 1;
        }
        true
    }
}

/// What a sample says about its instance's lifecycle (§8.7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChangeKind {
    /// A normal sample carrying data.
    #[default]
    Alive,
    /// The writer disposed of the instance.
    NotAliveDisposed,
    /// The writer no longer writes the instance but did not dispose of it.
    NotAliveUnregistered,
}

impl ChangeKind {
    /// True when the change carries a serialized sample.
    #[must_use]
    pub const fn carries_data(self) -> bool {
        matches!(self, Self::Alive)
    }

    /// The `PID_STATUS_INFO` octets a `DATA` announces this kind with.
    ///
    /// Bit 0 is "disposed", bit 1 is "unregistered", both in the last octet
    /// of a four-octet big-endian field (§9.6.3.9).
    #[must_use]
    pub const fn status_info(self) -> [u8; 4] {
        match self {
            Self::Alive => [0, 0, 0, 0],
            Self::NotAliveDisposed => [0, 0, 0, 1],
            Self::NotAliveUnregistered => [0, 0, 0, 2],
        }
    }

    /// Read a kind back from `PID_STATUS_INFO`.
    #[must_use]
    pub const fn from_status_info(octets: [u8; 4]) -> Self {
        let flags = octets[3];
        if flags & 0x01 != 0 {
            Self::NotAliveDisposed
        } else if flags & 0x02 != 0 {
            Self::NotAliveUnregistered
        } else {
            Self::Alive
        }
    }
}

/// One sample in a history cache.
///
/// The payload is owned. A writer's cache must outlive the buffer the
/// application wrote from, and a reader's must outlive the datagram, so
/// borrowing here would only move the copy somewhere less obvious.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheChange {
    /// The writer's sequence number for this sample.
    pub sequence_number: SequenceNumber,
    /// Whether the sample carries data or announces a disposal.
    pub kind: ChangeKind,
    /// Which instance of the topic it belongs to.
    pub instance: InstanceHandle,
    /// The serialized payload, encapsulation header included.
    pub payload: Vec<u8>,
    /// The writer's `INFO_TS`, when it sent one.
    pub source_timestamp: Option<Time>,
    /// When this endpoint stored the change — the clock `LIFESPAN` runs on.
    pub written_at: Instant,
}

impl CacheChange {
    /// A live sample with no key and no source timestamp.
    #[must_use]
    pub fn new(sequence_number: SequenceNumber, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            sequence_number,
            kind: ChangeKind::Alive,
            instance: InstanceHandle::NIL,
            payload: payload.into(),
            source_timestamp: None,
            written_at: Instant::now(),
        }
    }

    /// The same, stamped at an explicit moment.
    ///
    /// What a test uses so that `LIFESPAN` can be exercised without waiting.
    #[must_use]
    pub fn at(
        sequence_number: SequenceNumber,
        payload: impl Into<Vec<u8>>,
        written_at: Instant,
    ) -> Self {
        Self {
            written_at,
            ..Self::new(sequence_number, payload)
        }
    }

    /// Attach a source timestamp.
    #[must_use]
    pub const fn with_source_timestamp(mut self, timestamp: Time) -> Self {
        self.source_timestamp = Some(timestamp);
        self
    }

    /// Attach an instance handle.
    #[must_use]
    pub const fn with_instance(mut self, instance: InstanceHandle) -> Self {
        self.instance = instance;
        self
    }

    /// Set the lifecycle kind.
    #[must_use]
    pub const fn with_kind(mut self, kind: ChangeKind) -> Self {
        self.kind = kind;
        self
    }

    /// Octets in the serialized payload.
    #[must_use]
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// True when the payload is empty — a disposal, normally.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    /// True when `lifespan` has elapsed at `now`.
    #[must_use]
    pub fn has_expired(&self, lifespan: StdDuration, now: Instant) -> bool {
        now.saturating_duration_since(self.written_at) >= lifespan
    }
}

/// What removing a change was caused by.
///
/// A writer turns [`Evicted`](Removal::Evicted) and
/// [`Expired`](Removal::Expired) into a `GAP`; it says nothing about an
/// acknowledged change, because the reader already has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// `KEEP_LAST`'s depth pushed the oldest sample out.
    Evicted(SequenceNumber),
    /// `LIFESPAN` elapsed.
    Expired(SequenceNumber),
}

impl Removal {
    /// The sequence number that is gone.
    #[must_use]
    pub const fn sequence_number(self) -> SequenceNumber {
        match self {
            Self::Evicted(number) | Self::Expired(number) => number,
        }
    }

    /// True when a reader may still be asking for it, so the writer owes a
    /// `GAP`.
    ///
    /// Both variants qualify; the distinction exists for logs and metrics,
    /// not for protocol behaviour.
    #[must_use]
    pub const fn needs_gap(self) -> bool {
        true
    }
}

/// The samples one endpoint is holding.
///
/// Ordered by sequence number, bounded by `HISTORY` and `RESOURCE_LIMITS`,
/// aged out by `LIFESPAN`.
#[derive(Debug, Clone)]
pub struct HistoryCache {
    changes: BTreeMap<SequenceNumber, CacheChange>,
    history: HistoryQos,
    limits: ResourceLimitsQos,
    lifespan: Option<StdDuration>,
}

impl HistoryCache {
    /// A cache governed by `history`, with no other limits.
    #[must_use]
    pub fn new(history: HistoryQos) -> Self {
        Self {
            changes: BTreeMap::new(),
            history,
            limits: ResourceLimitsQos::unlimited(),
            lifespan: None,
        }
    }

    /// Add `RESOURCE_LIMITS` on top of the history policy.
    #[must_use]
    pub const fn with_limits(mut self, limits: ResourceLimitsQos) -> Self {
        self.limits = limits;
        self
    }

    /// Add a `LIFESPAN`; `None` means samples never expire.
    #[must_use]
    pub const fn with_lifespan(mut self, lifespan: Option<StdDuration>) -> Self {
        self.lifespan = lifespan;
        self
    }

    /// The history policy in force.
    #[must_use]
    pub const fn history(&self) -> HistoryQos {
        self.history
    }

    /// The lifespan in force.
    #[must_use]
    pub const fn lifespan(&self) -> Option<StdDuration> {
        self.lifespan
    }

    /// How many changes are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// True when nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The lowest sequence number held.
    #[must_use]
    pub fn min_sequence_number(&self) -> Option<SequenceNumber> {
        self.changes.keys().next().copied()
    }

    /// The highest sequence number held.
    #[must_use]
    pub fn max_sequence_number(&self) -> Option<SequenceNumber> {
        self.changes.keys().next_back().copied()
    }

    /// The change with this sequence number, if it is still held.
    #[must_use]
    pub fn get(&self, sequence_number: SequenceNumber) -> Option<&CacheChange> {
        self.changes.get(&sequence_number)
    }

    /// True when this sequence number is still held.
    #[must_use]
    pub fn contains(&self, sequence_number: SequenceNumber) -> bool {
        self.changes.contains_key(&sequence_number)
    }

    /// Every change, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &CacheChange> {
        self.changes.values()
    }

    /// Every sequence number held, ascending.
    pub fn sequence_numbers(&self) -> impl Iterator<Item = SequenceNumber> + '_ {
        self.changes.keys().copied()
    }

    /// Store `change`, applying `HISTORY` and `RESOURCE_LIMITS`.
    ///
    /// Returns whatever the policies pushed out, which the caller owes a
    /// `GAP` for. Re-inserting a sequence number already held replaces it and
    /// evicts nothing — that is a writer resending, not a new sample.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::HistoryFull`] when `KEEP_ALL` is in force and the
    /// resource limit is reached. `KEEP_LAST` never returns this.
    pub fn insert(&mut self, change: CacheChange) -> BehaviorResult<Vec<Removal>> {
        let sequence_number = change.sequence_number;
        if let std::collections::btree_map::Entry::Occupied(mut held) =
            self.changes.entry(sequence_number)
        {
            // A writer resending, not a new sample: replace and evict
            // nothing, or the history would shrink every time a datagram was
            // repeated.
            held.insert(change);
            return Ok(Vec::new());
        }

        let mut removed = Vec::new();
        match self.history.kind {
            HistoryKind::KeepLast => {
                let depth = self.history.retained().unwrap_or(1);
                let instance = change.instance;
                self.changes.insert(sequence_number, change);
                while self.instance_len(instance) > depth {
                    match self.oldest_of(instance) {
                        None => break,
                        Some(oldest) => {
                            self.changes.remove(&oldest);
                            removed.push(Removal::Evicted(oldest));
                        }
                    }
                }
            }
            HistoryKind::KeepAll => {
                if let Some(ceiling) = self.limits.sample_ceiling()
                    && self.changes.len() >= ceiling
                {
                    return Err(BehaviorError::HistoryFull {
                        depth: self.changes.len(),
                    });
                }
                self.changes.insert(sequence_number, change);
            }
        }

        // The total-sample ceiling applies to KEEP_LAST too, and there it
        // evicts rather than refuses — a bounded cache that starts rejecting
        // writes would turn a resource policy into a reliability one.
        if let Some(ceiling) = self.limits.sample_ceiling() {
            while self.changes.len() > ceiling {
                match self.changes.keys().next().copied() {
                    None => break,
                    Some(oldest) => {
                        self.changes.remove(&oldest);
                        removed.push(Removal::Evicted(oldest));
                    }
                }
            }
        }

        Ok(removed)
    }

    /// Remove one change by sequence number, whatever the policies say.
    pub fn remove(&mut self, sequence_number: SequenceNumber) -> Option<CacheChange> {
        self.changes.remove(&sequence_number)
    }

    /// Drop everything at or below `through`.
    ///
    /// What a `KEEP_ALL` writer does once every matched reader has
    /// acknowledged. Returns how many changes went.
    pub fn drop_through(&mut self, through: SequenceNumber) -> usize {
        let doomed: Vec<SequenceNumber> = self
            .changes
            .range(..=through)
            .map(|(number, _)| *number)
            .collect();
        for number in &doomed {
            self.changes.remove(number);
        }
        doomed.len()
    }

    /// Drop everything whose lifespan has elapsed at `now`.
    ///
    /// Returns the sequence numbers that expired, which the caller owes a
    /// `GAP` for.
    pub fn expire(&mut self, now: Instant) -> Vec<Removal> {
        let Some(lifespan) = self.lifespan else {
            return Vec::new();
        };
        let doomed: Vec<SequenceNumber> = self
            .changes
            .iter()
            .filter(|(_, change)| change.has_expired(lifespan, now))
            .map(|(number, _)| *number)
            .collect();
        let mut removed = Vec::with_capacity(doomed.len());
        for number in doomed {
            self.changes.remove(&number);
            removed.push(Removal::Expired(number));
        }
        removed
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.changes.clear();
    }

    /// The changes a late-joining `TRANSIENT_LOCAL` reader should be replayed,
    /// oldest first.
    ///
    /// Exactly the cache contents: `VOLATILE` writers do not call this, and a
    /// `TRANSIENT_LOCAL` one has already bounded its history with `KEEP_LAST`.
    pub fn replay(&self) -> impl Iterator<Item = &CacheChange> {
        self.changes.values()
    }

    /// How many distinct instances are held.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        let mut instances: Vec<InstanceHandle> = self
            .changes
            .values()
            .map(|change| change.instance)
            .collect();
        instances.sort_unstable();
        instances.dedup();
        instances.len()
    }

    /// Changes held for one instance.
    fn instance_len(&self, instance: InstanceHandle) -> usize {
        self.changes
            .values()
            .filter(|change| change.instance == instance)
            .count()
    }

    /// The lowest sequence number held for one instance.
    fn oldest_of(&self, instance: InstanceHandle) -> Option<SequenceNumber> {
        self.changes
            .iter()
            .find(|(_, change)| change.instance == instance)
            .map(|(number, _)| *number)
    }
}

impl Default for HistoryCache {
    fn default() -> Self {
        Self::new(HistoryQos::default())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn number(value: i64) -> SequenceNumber {
        SequenceNumber::new(value)
    }

    fn change(value: i64) -> CacheChange {
        CacheChange::new(number(value), vec![value as u8])
    }

    #[test]
    fn a_keep_last_cache_evicts_the_oldest() {
        let mut cache = HistoryCache::new(HistoryQos::keep_last(3));
        for value in 1..=3 {
            assert!(cache.insert(change(value)).unwrap().is_empty());
        }
        assert_eq!(cache.len(), 3);

        let removed = cache.insert(change(4)).unwrap();
        assert_eq!(removed, vec![Removal::Evicted(number(1))]);
        assert_eq!(cache.min_sequence_number(), Some(number(2)));
        assert_eq!(cache.max_sequence_number(), Some(number(4)));
        assert!(!cache.contains(number(1)));
    }

    #[test]
    fn an_eviction_always_needs_a_gap() {
        let removal = Removal::Evicted(number(7));
        assert!(removal.needs_gap());
        assert_eq!(removal.sequence_number(), number(7));
        assert!(Removal::Expired(number(8)).needs_gap());
    }

    #[test]
    fn keep_last_counts_per_instance() {
        let mut cache = HistoryCache::new(HistoryQos::keep_last(1));
        let left = InstanceHandle::new([1; INSTANCE_HANDLE_LEN]);
        let right = InstanceHandle::new([2; INSTANCE_HANDLE_LEN]);

        cache.insert(change(1).with_instance(left)).expect("insert");
        cache
            .insert(change(2).with_instance(right))
            .expect("insert");
        assert_eq!(cache.len(), 2, "two instances, one sample each");
        assert_eq!(cache.instance_count(), 2);

        let removed = cache.insert(change(3).with_instance(left)).expect("insert");
        assert_eq!(removed, vec![Removal::Evicted(number(1))]);
        assert!(cache.contains(number(2)), "the other instance is untouched");
    }

    #[test]
    fn a_keep_all_cache_never_evicts() {
        let mut cache = HistoryCache::new(HistoryQos::keep_all());
        for value in 1..=100 {
            assert!(cache.insert(change(value)).unwrap().is_empty());
        }
        assert_eq!(cache.len(), 100);
        assert_eq!(cache.min_sequence_number(), Some(number(1)));
    }

    #[test]
    fn a_keep_all_cache_refuses_when_the_resource_limit_is_reached() {
        let mut cache = HistoryCache::new(HistoryQos::keep_all()).with_limits(ResourceLimitsQos {
            max_samples: 2,
            ..ResourceLimitsQos::unlimited()
        });
        cache.insert(change(1)).expect("first fits");
        cache.insert(change(2)).expect("second fits");
        let error = cache.insert(change(3)).expect_err("third does not");
        assert_eq!(error, BehaviorError::HistoryFull { depth: 2 });
        assert_eq!(cache.len(), 2, "the refused sample is not stored");
    }

    #[test]
    fn a_keep_last_cache_evicts_rather_than_refusing_at_the_resource_limit() {
        let mut cache =
            HistoryCache::new(HistoryQos::keep_last(100)).with_limits(ResourceLimitsQos {
                max_samples: 2,
                ..ResourceLimitsQos::unlimited()
            });
        cache.insert(change(1)).expect("insert");
        cache.insert(change(2)).expect("insert");
        let removed = cache.insert(change(3)).expect("insert");
        assert_eq!(removed, vec![Removal::Evicted(number(1))]);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn re_inserting_a_held_sequence_number_replaces_without_evicting() {
        let mut cache = HistoryCache::new(HistoryQos::keep_last(2));
        cache.insert(change(1)).expect("insert");
        cache.insert(change(2)).expect("insert");
        let removed = cache
            .insert(CacheChange::new(number(2), vec![0xff]))
            .expect("insert");
        assert!(removed.is_empty());
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.get(number(2)).map(|c| c.payload.clone()),
            Some(vec![0xff])
        );
    }

    #[test]
    fn lifespan_expires_the_old_and_keeps_the_new() {
        let origin = Instant::now();
        let mut cache = HistoryCache::new(HistoryQos::keep_all())
            .with_lifespan(Some(StdDuration::from_millis(100)));

        cache
            .insert(CacheChange::at(number(1), vec![1], origin))
            .expect("insert");
        cache
            .insert(CacheChange::at(
                number(2),
                vec![2],
                origin + StdDuration::from_millis(90),
            ))
            .expect("insert");

        let expired = cache.expire(origin + StdDuration::from_millis(120));
        assert_eq!(expired, vec![Removal::Expired(number(1))]);
        assert!(cache.contains(number(2)));
        assert_eq!(cache.lifespan(), Some(StdDuration::from_millis(100)));
    }

    #[test]
    fn a_cache_without_a_lifespan_never_expires() {
        let mut cache = HistoryCache::new(HistoryQos::keep_all());
        cache.insert(change(1)).expect("insert");
        assert!(
            cache
                .expire(Instant::now() + StdDuration::from_secs(3_600))
                .is_empty()
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn dropping_through_removes_the_prefix() {
        let mut cache = HistoryCache::new(HistoryQos::keep_all());
        for value in 1..=5 {
            cache.insert(change(value)).expect("insert");
        }
        assert_eq!(cache.drop_through(number(3)), 3);
        assert_eq!(cache.min_sequence_number(), Some(number(4)));
        assert_eq!(cache.drop_through(number(0)), 0);
    }

    #[test]
    fn replay_yields_everything_oldest_first() {
        let mut cache = HistoryCache::new(HistoryQos::keep_last(4));
        for value in 1..=4 {
            cache.insert(change(value)).expect("insert");
        }
        let replayed: Vec<i64> = cache
            .replay()
            .map(|change| change.sequence_number.value())
            .collect();
        assert_eq!(replayed, vec![1, 2, 3, 4]);
    }

    #[test]
    fn an_empty_cache_reports_no_bounds() {
        let cache = HistoryCache::default();
        assert!(cache.is_empty());
        assert_eq!(cache.min_sequence_number(), None);
        assert_eq!(cache.max_sequence_number(), None);
        assert_eq!(cache.instance_count(), 0);
        assert_eq!(cache.history().depth, 1);
    }

    #[test]
    fn clearing_empties_the_cache() {
        let mut cache = HistoryCache::new(HistoryQos::keep_all());
        cache.insert(change(1)).expect("insert");
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn status_info_round_trips_for_every_kind() {
        for kind in [
            ChangeKind::Alive,
            ChangeKind::NotAliveDisposed,
            ChangeKind::NotAliveUnregistered,
        ] {
            assert_eq!(ChangeKind::from_status_info(kind.status_info()), kind);
        }
        assert!(ChangeKind::Alive.carries_data());
        assert!(!ChangeKind::NotAliveDisposed.carries_data());
    }

    #[test]
    fn an_unknown_status_info_bit_reads_as_alive() {
        assert_eq!(
            ChangeKind::from_status_info([0, 0, 0, 0x80]),
            ChangeKind::Alive
        );
    }

    #[test]
    fn the_nil_instance_is_the_keyless_one() {
        assert!(InstanceHandle::NIL.is_nil());
        assert!(!InstanceHandle::new([1; INSTANCE_HANDLE_LEN]).is_nil());
        assert_eq!(InstanceHandle::default(), InstanceHandle::NIL);
        assert_eq!(InstanceHandle::NIL.as_bytes(), &[0; INSTANCE_HANDLE_LEN]);
    }

    #[test]
    fn a_change_reports_its_size_and_expiry() {
        let origin = Instant::now();
        let change = CacheChange::at(number(1), vec![0; 32], origin)
            .with_source_timestamp(Time::new(5, 0))
            .with_kind(ChangeKind::NotAliveDisposed);
        assert_eq!(change.len(), 32);
        assert!(!change.is_empty());
        assert_eq!(change.source_timestamp, Some(Time::new(5, 0)));
        assert!(!change.has_expired(StdDuration::from_secs(1), origin));
        assert!(change.has_expired(
            StdDuration::from_secs(1),
            origin + StdDuration::from_secs(2)
        ));
    }

    #[test]
    fn sequence_numbers_come_out_ascending() {
        let mut cache = HistoryCache::new(HistoryQos::keep_all());
        for value in [5, 1, 3, 2, 4] {
            cache.insert(change(value)).expect("insert");
        }
        let ordered: Vec<i64> = cache
            .sequence_numbers()
            .map(SequenceNumber::value)
            .collect();
        assert_eq!(ordered, vec![1, 2, 3, 4, 5]);
    }
}
