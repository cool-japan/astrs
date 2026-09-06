//! [`SimNetwork`]: a network that misbehaves on a seeded schedule.
//!
//! Raft's correctness claims are claims about an *adversarial* network: one
//! that drops, reorders, duplicates and delays messages, and that can split
//! the cluster in two. A test running over a perfect in-process channel proves
//! none of them. This module supplies the adversary, and makes it reproducible
//! — every decision comes from a seeded [`crate::Rng`], so a failing run is
//! replayed exactly by re-using its seed.
//!
//! # What can go wrong, and what cannot
//!
//! | Fault | Modelled |
//! |---|---|
//! | drop | yes — [`FaultSchedule::drop_percent`] |
//! | duplicate | yes — [`FaultSchedule::duplicate_percent`] |
//! | delay / reorder | yes — [`FaultSchedule::max_delay_ticks`] |
//! | partition | yes — [`SimNetwork::partition`] |
//! | **corruption** | **no**, deliberately |
//!
//! Corruption is out of scope on purpose: the frame layer already checksums
//! every message ([`astrs_wire::crc32c`]), so a corrupted Raft message is
//! detected and discarded before consensus ever sees it — which is a *drop*,
//! already modelled above.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::sim::{FaultSchedule, SimNetwork};
//! use astrs_raft::{Envelope, PeerId, RaftMessage, Term};
//!
//! let mut network = SimNetwork::new(FaultSchedule::perfect(), 7);
//! network.send(
//!     Envelope::new(PeerId::new(1), PeerId::new(2), RaftMessage::TimeoutNow { term: Term::new(1) }),
//!     0,
//! );
//! assert_eq!(network.deliver_due(0).len(), 1);
//! ```

use std::collections::BTreeMap;

use crate::message::Envelope;
use crate::types::{PeerId, Rng};

/// How badly the simulated network behaves.
///
/// # Examples
///
/// ```
/// use astrs_raft::sim::FaultSchedule;
///
/// assert!(FaultSchedule::perfect().is_perfect());
/// assert!(!FaultSchedule::lossy(10).is_perfect());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultSchedule {
    /// Percentage of messages discarded outright.
    pub drop_percent: u64,
    /// Percentage of messages delivered twice.
    pub duplicate_percent: u64,
    /// The largest delivery delay, in ticks. `0` delivers everything on the
    /// tick it was sent.
    pub max_delay_ticks: u64,
}

impl FaultSchedule {
    /// A network that loses nothing and delays nothing.
    #[must_use]
    pub const fn perfect() -> Self {
        Self {
            drop_percent: 0,
            duplicate_percent: 0,
            max_delay_ticks: 0,
        }
    }

    /// A network that drops `percent` of messages and nothing else.
    #[must_use]
    pub const fn lossy(percent: u64) -> Self {
        Self {
            drop_percent: percent,
            duplicate_percent: 0,
            max_delay_ticks: 0,
        }
    }

    /// A thoroughly unpleasant network: drops, duplicates and delays.
    #[must_use]
    pub const fn chaotic() -> Self {
        Self {
            drop_percent: 15,
            duplicate_percent: 10,
            max_delay_ticks: 4,
        }
    }

    /// Whether this schedule introduces no faults at all.
    #[must_use]
    pub const fn is_perfect(&self) -> bool {
        self.drop_percent == 0 && self.duplicate_percent == 0 && self.max_delay_ticks == 0
    }
}

/// One message waiting to be delivered.
#[derive(Debug, Clone)]
struct InFlight {
    /// The tick it becomes deliverable on.
    due: u64,
    /// The message.
    envelope: Envelope,
}

/// A message-passing network with faults and partitions.
#[derive(Debug)]
pub struct SimNetwork {
    /// The fault schedule in force.
    schedule: FaultSchedule,
    /// The seeded randomness every decision comes from.
    rng: Rng,
    /// Messages not yet delivered.
    in_flight: Vec<InFlight>,
    /// Which partition group each peer is in; peers in different groups
    /// cannot reach each other. Empty means one connected cluster.
    groups: BTreeMap<PeerId, u32>,
    /// How many messages were dropped, for assertions about coverage.
    dropped: u64,
    /// How many messages were duplicated.
    duplicated: u64,
    /// How many messages were delivered.
    delivered: u64,
    /// Deliveries per [`crate::message::RaftMessage::variant_name`], so a test
    /// can prove *which* mechanism carried a replica forward rather than only
    /// that it converged. Catch-up by `InstallSnapshot` and catch-up by
    /// `AppendEntries` look identical in the state machine and are not
    /// remotely the same code path.
    delivered_by_kind: BTreeMap<&'static str, u64>,
}

impl SimNetwork {
    /// A network with `schedule` and a fixed `seed`.
    #[must_use]
    pub fn new(schedule: FaultSchedule, seed: u64) -> Self {
        Self {
            schedule,
            rng: Rng::new(seed),
            in_flight: Vec::new(),
            groups: BTreeMap::new(),
            dropped: 0,
            duplicated: 0,
            delivered: 0,
            delivered_by_kind: BTreeMap::new(),
        }
    }

    /// Replaces the fault schedule mid-run — how a test heals a lossy link.
    pub const fn set_schedule(&mut self, schedule: FaultSchedule) {
        self.schedule = schedule;
    }

    /// The schedule in force.
    #[must_use]
    pub const fn schedule(&self) -> FaultSchedule {
        self.schedule
    }

    /// Splits the cluster: peers listed in different groups cannot exchange
    /// messages.
    ///
    /// Messages already in flight across the new boundary are discarded, which
    /// is what a real partition does to packets in a switch's queue.
    pub fn partition(&mut self, groups: impl IntoIterator<Item = Vec<PeerId>>) {
        self.groups.clear();
        for (index, group) in groups.into_iter().enumerate() {
            for peer in group {
                self.groups.insert(peer, index as u32);
            }
        }
        let queued = std::mem::take(&mut self.in_flight);
        self.in_flight = queued
            .into_iter()
            .filter(|message| self.can_reach(message.envelope.from, message.envelope.to))
            .collect();
    }

    /// Removes every partition.
    pub fn heal(&mut self) {
        self.groups.clear();
    }

    /// Whether `from` can currently reach `to`.
    #[must_use]
    pub fn can_reach(&self, from: PeerId, to: PeerId) -> bool {
        match (self.groups.get(&from), self.groups.get(&to)) {
            (Some(a), Some(b)) => a == b,
            // A peer nobody assigned to a group is reachable from everyone —
            // a partition names the split, not the whole cluster.
            _ => true,
        }
    }

    /// Offers one message to the network.
    ///
    /// It may be discarded, delayed, or queued twice, according to the
    /// schedule and this network's seed.
    pub fn send(&mut self, envelope: Envelope, now: u64) {
        if !self.can_reach(envelope.from, envelope.to) {
            self.dropped += 1;
            return;
        }
        if self.rng.in_range(0, 100) < self.schedule.drop_percent {
            self.dropped += 1;
            return;
        }
        let delay = if self.schedule.max_delay_ticks == 0 {
            0
        } else {
            self.rng.in_range(0, self.schedule.max_delay_ticks + 1)
        };
        self.in_flight.push(InFlight {
            due: now.saturating_add(delay),
            envelope: envelope.clone(),
        });
        if self.rng.in_range(0, 100) < self.schedule.duplicate_percent {
            self.duplicated += 1;
            let extra = if self.schedule.max_delay_ticks == 0 {
                0
            } else {
                self.rng.in_range(0, self.schedule.max_delay_ticks + 1)
            };
            self.in_flight.push(InFlight {
                due: now.saturating_add(extra),
                envelope,
            });
        }
    }

    /// Takes every message due at or before `now`, in queue order.
    ///
    /// Delayed messages therefore arrive interleaved with newer ones, which is
    /// the reordering Raft must tolerate.
    pub fn deliver_due(&mut self, now: u64) -> Vec<Envelope> {
        let mut due = Vec::new();
        let queued = std::mem::take(&mut self.in_flight);
        let mut still_waiting = Vec::with_capacity(queued.len());
        for message in queued {
            if message.due <= now && self.can_reach(message.envelope.from, message.envelope.to) {
                due.push(message.envelope);
            } else if message.due <= now {
                // The partition arrived while it was queued.
                self.dropped += 1;
            } else {
                still_waiting.push(message);
            }
        }
        self.in_flight = still_waiting;
        self.delivered += due.len() as u64;
        for envelope in &due {
            *self
                .delivered_by_kind
                .entry(envelope.message.variant_name())
                .or_insert(0) += 1;
        }
        due
    }

    /// How many messages are still queued.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// How many messages this network has discarded.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// How many messages this network has duplicated.
    #[must_use]
    pub const fn duplicated(&self) -> u64 {
        self.duplicated
    }

    /// How many messages this network has delivered.
    #[must_use]
    pub const fn delivered(&self) -> u64 {
        self.delivered
    }

    /// How many deliveries carried the named message kind.
    ///
    /// `kind` is a [`crate::message::RaftMessage::variant_name`] —
    /// `"InstallSnapshot"`, `"AppendEntries"`, and so on. An unseen kind
    /// reports `0` rather than failing, so a test can assert an absence as
    /// easily as a presence.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_raft::sim::{FaultSchedule, SimNetwork};
    ///
    /// let network = SimNetwork::new(FaultSchedule::perfect(), 1);
    /// assert_eq!(network.delivered_of("InstallSnapshot"), 0);
    /// ```
    #[must_use]
    pub fn delivered_of(&self, kind: &str) -> u64 {
        self.delivered_by_kind.get(kind).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::message::RaftMessage;
    use crate::types::Term;

    fn envelope(from: u64, to: u64) -> Envelope {
        Envelope::new(
            PeerId::new(from),
            PeerId::new(to),
            RaftMessage::TimeoutNow { term: Term::new(1) },
        )
    }

    #[test]
    fn a_perfect_network_delivers_everything_at_once() {
        let mut network = SimNetwork::new(FaultSchedule::perfect(), 1);
        for peer in 2..=4u64 {
            network.send(envelope(1, peer), 0);
        }
        assert_eq!(network.deliver_due(0).len(), 3);
        assert_eq!(network.in_flight(), 0);
        assert_eq!(network.dropped(), 0);
    }

    #[test]
    fn drops_happen_at_roughly_the_configured_rate() {
        let mut network = SimNetwork::new(FaultSchedule::lossy(50), 42);
        for _ in 0..2_000 {
            network.send(envelope(1, 2), 0);
        }
        let dropped = network.dropped();
        assert!(
            (700..1_300).contains(&dropped),
            "50% of 2000 should be near 1000, got {dropped}"
        );
    }

    #[test]
    fn the_same_seed_produces_the_same_run() {
        // The whole reason the network is seeded: a failure replays exactly.
        let counts: Vec<(u64, u64)> = (0..2)
            .map(|_| {
                let mut network = SimNetwork::new(FaultSchedule::chaotic(), 99);
                for tick in 0..500u64 {
                    network.send(envelope(1, 2), tick);
                    network.deliver_due(tick);
                }
                (network.dropped(), network.duplicated())
            })
            .collect();
        assert_eq!(counts[0], counts[1]);
    }

    #[test]
    fn different_seeds_produce_different_runs() {
        let run = |seed| {
            let mut network = SimNetwork::new(FaultSchedule::chaotic(), seed);
            for tick in 0..500u64 {
                network.send(envelope(1, 2), tick);
                network.deliver_due(tick);
            }
            network.dropped()
        };
        assert_ne!(run(1), run(2));
    }

    #[test]
    fn delayed_messages_arrive_later_and_out_of_order() {
        let mut network = SimNetwork::new(
            FaultSchedule {
                drop_percent: 0,
                duplicate_percent: 0,
                max_delay_ticks: 5,
            },
            7,
        );
        for _ in 0..64 {
            network.send(envelope(1, 2), 0);
        }
        let immediate = network.deliver_due(0).len();
        assert!(immediate < 64, "some messages must have been delayed");
        assert!(network.in_flight() > 0);
        // Everything is eventually delivered.
        let later = network.deliver_due(10).len();
        assert_eq!(immediate + later, 64);
    }

    #[test]
    fn duplicates_are_delivered_twice() {
        let mut network = SimNetwork::new(
            FaultSchedule {
                drop_percent: 0,
                duplicate_percent: 100,
                max_delay_ticks: 0,
            },
            3,
        );
        network.send(envelope(1, 2), 0);
        assert_eq!(network.deliver_due(0).len(), 2);
        assert_eq!(network.duplicated(), 1);
    }

    #[test]
    fn a_partition_isolates_the_groups_from_each_other() {
        let mut network = SimNetwork::new(FaultSchedule::perfect(), 5);
        network.partition([vec![PeerId::new(1)], vec![PeerId::new(2), PeerId::new(3)]]);
        assert!(!network.can_reach(PeerId::new(1), PeerId::new(2)));
        assert!(network.can_reach(PeerId::new(2), PeerId::new(3)));

        network.send(envelope(1, 2), 0);
        network.send(envelope(2, 3), 0);
        let delivered = network.deliver_due(0);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].from, PeerId::new(2));
    }

    #[test]
    fn a_partition_discards_what_was_already_in_flight_across_it() {
        let mut network = SimNetwork::new(
            FaultSchedule {
                drop_percent: 0,
                duplicate_percent: 0,
                max_delay_ticks: 4,
            },
            11,
        );
        for _ in 0..32 {
            network.send(envelope(1, 2), 0);
        }
        network.partition([vec![PeerId::new(1)], vec![PeerId::new(2)]]);
        assert_eq!(network.deliver_due(100).len(), 0);
    }

    #[test]
    fn healing_restores_reachability() {
        let mut network = SimNetwork::new(FaultSchedule::perfect(), 5);
        network.partition([vec![PeerId::new(1)], vec![PeerId::new(2)]]);
        network.heal();
        assert!(network.can_reach(PeerId::new(1), PeerId::new(2)));
        network.send(envelope(1, 2), 0);
        assert_eq!(network.deliver_due(0).len(), 1);
    }

    #[test]
    fn the_schedule_can_be_swapped_mid_run() {
        let mut network = SimNetwork::new(FaultSchedule::lossy(100), 1);
        network.send(envelope(1, 2), 0);
        assert_eq!(network.deliver_due(0).len(), 0);
        network.set_schedule(FaultSchedule::perfect());
        assert!(network.schedule().is_perfect());
        network.send(envelope(1, 2), 1);
        assert_eq!(network.deliver_due(1).len(), 1);
        assert_eq!(network.delivered(), 1);
    }
}
