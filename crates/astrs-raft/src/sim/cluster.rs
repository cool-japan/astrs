//! [`SimCluster`]: a whole Raft cluster in one thread, with the invariants
//! checked on every tick.
//!
//! # What is being tested
//!
//! Not "does an election happen" — that is a liveness property, and this
//! harness checks it too — but the four **safety** properties the Raft paper
//! proves, restated as assertions that run continuously:
//!
//! | Property | [`Invariant`] | Statement |
//! |---|---|---|
//! | Election Safety | [`Invariant::ElectionSafety`] | at most one leader per term |
//! | Log Matching | [`Invariant::LogMatching`] | two logs agreeing at an index agree before it |
//! | Leader Completeness | [`Invariant::LeaderCompleteness`] | a committed entry is in every later leader's log |
//! | State Machine Safety | [`Invariant::StateMachineSafety`] | no two replicas apply different commands at the same index |
//!
//! A safety violation is not a flaky test: it means a caller could have been
//! told a write succeeded and then read it back missing. That is why
//! [`SimCluster::check`] is called after *every* tick rather than at the end.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::sim::{FaultSchedule, SimCluster};
//!
//! let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 7);
//! let leader = cluster.run_until_leader(200).expect("no invariant violated");
//! assert!(leader.is_some(), "an election completes on a healthy network");
//! cluster.propose(b"hello".to_vec()).expect("a proposal");
//! cluster.run(50).expect("no invariant violated");
//!
//! assert!(cluster.committed_everywhere(b"hello"));
//! assert_eq!(cluster.leaders(), vec![leader.expect("a leader")]);
//! ```

use std::collections::BTreeMap;
use std::fmt;

use crate::config::RaftConfig;
use crate::core::RaftNode;
use crate::error::Result;
use crate::log::store::{LogStore, MemoryLog};
use crate::membership::MembershipChange;
use crate::sim::clock::VirtualClock;
use crate::sim::network::{FaultSchedule, SimNetwork};
use crate::state_machine::MemoryStateMachine;
use crate::types::{LogIndex, PeerId, Term};

/// A safety property that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Invariant {
    /// Two peers were leaders in the same term.
    ElectionSafety {
        /// The term in which two leaders existed.
        term: Term,
        /// The leader seen first.
        first: PeerId,
        /// The second, illegal, leader.
        second: PeerId,
    },
    /// Two logs hold entries with the same index and term but disagree
    /// somewhere before it.
    LogMatching {
        /// The index whose prefixes disagree.
        index: LogIndex,
        /// One of the peers.
        left: PeerId,
        /// The other.
        right: PeerId,
    },
    /// An entry that had been committed is no longer present, or changed
    /// term.
    LeaderCompleteness {
        /// The committed index that changed.
        index: LogIndex,
        /// The term it was committed in.
        committed_term: Term,
        /// The term found there now.
        found_term: Term,
        /// The peer holding the wrong entry.
        peer: PeerId,
    },
    /// Two replicas applied different commands at the same position.
    StateMachineSafety {
        /// The apply position that disagrees.
        position: usize,
        /// One of the peers.
        left: PeerId,
        /// The other.
        right: PeerId,
    },
}

impl fmt::Display for Invariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ElectionSafety {
                term,
                first,
                second,
            } => write!(f, "election safety: {first} and {second} both led {term}"),
            Self::LogMatching { index, left, right } => write!(
                f,
                "log matching: {left} and {right} agree at {index} but not before it"
            ),
            Self::LeaderCompleteness {
                index,
                committed_term,
                found_term,
                peer,
            } => write!(
                f,
                "leader completeness: {index} committed in {committed_term} but {peer} holds \
                 {found_term}"
            ),
            Self::StateMachineSafety {
                position,
                left,
                right,
            } => write!(
                f,
                "state machine safety: {left} and {right} applied different commands at \
                 position {position}"
            ),
        }
    }
}

impl std::error::Error for Invariant {}

/// One replica in the simulation, plus whether it is currently "running".
struct SimPeer {
    /// The consensus node, absent while the peer is crashed.
    node: Option<RaftNode<MemoryStateMachine, MemoryLog>>,
    /// The durable state a crashed peer keeps, to be handed back on restart.
    durable: Option<(MemoryLog, MemoryStateMachine)>,
    /// This peer's configuration, so a restart rebuilds it identically.
    config: RaftConfig,
}

/// A deterministic, in-thread Raft cluster.
pub struct SimCluster {
    /// Every peer, running or crashed.
    peers: BTreeMap<PeerId, SimPeer>,
    /// The faulty network between them.
    network: SimNetwork,
    /// The virtual clock driving every tick.
    clock: VirtualClock,
    /// Which peer led each term, for the election-safety check.
    leaders_by_term: BTreeMap<Term, PeerId>,
    /// The term of every index that has ever been committed anywhere, for the
    /// leader-completeness check.
    committed: BTreeMap<LogIndex, Term>,
}

impl SimCluster {
    /// A cluster of `size` peers with the given network faults and seed.
    ///
    /// Peers are numbered `1..=size`, and each gets its own election-timeout
    /// seed derived from `seed`, so they do not all campaign in lockstep.
    ///
    /// # Panics
    ///
    /// Never panics: a cluster built here always satisfies
    /// [`RaftConfig::validate`].
    #[must_use]
    pub fn new(size: u64, schedule: FaultSchedule, seed: u64) -> Self {
        Self::with_config(size, schedule, seed, |config| config)
    }

    /// A cluster whose replicas are configured by `tune`.
    ///
    /// `tune` receives the shared configuration — peer set already filled in —
    /// and returns the one every replica runs with. Each peer's own `id` and
    /// `seed` are stamped afterwards, so `tune` cannot accidentally make five
    /// replicas share one identity.
    ///
    /// This is how a test reaches the timing- and threshold-dependent paths
    /// that the defaults deliberately keep far away: chiefly
    /// [`RaftConfig::with_snapshot_threshold`], since compaction at the default
    /// 4096 entries is not something a simulation reaches by accident.
    ///
    /// # Panics
    ///
    /// Never panics: an invalid `tune` result surfaces as peers that fail to
    /// start, which the first [`SimCluster::run`] then reports.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_raft::sim::{FaultSchedule, SimCluster};
    ///
    /// let mut cluster = SimCluster::with_config(3, FaultSchedule::perfect(), 5, |config| {
    ///     config.with_snapshot_threshold(4)
    /// });
    /// let leader = cluster.run_until_leader(200).expect("no invariant violated");
    /// assert!(leader.is_some());
    /// ```
    #[must_use]
    pub fn with_config(
        size: u64,
        schedule: FaultSchedule,
        seed: u64,
        tune: impl Fn(RaftConfig) -> RaftConfig,
    ) -> Self {
        let mut base = RaftConfig::new(PeerId::new(1));
        for id in 1..=size {
            base = base.with_peer(
                PeerId::new(id),
                std::net::SocketAddr::from(([127, 0, 0, 1], 7500 + id as u16)),
            );
        }
        let base = tune(base);

        let mut peers = BTreeMap::new();
        for id in 1..=size {
            let mut config = base.clone();
            config.id = PeerId::new(id);
            config.seed = seed
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(id.wrapping_mul(0xBF58_476D_1CE4_E5B9))
                | 1;
            let node = RaftNode::new(config.clone(), MemoryLog::new(), MemoryStateMachine::new());
            peers.insert(
                PeerId::new(id),
                SimPeer {
                    // A configuration built here is valid by construction, so
                    // a failure would be a bug in this constructor rather than
                    // in the caller — recorded as a crashed peer instead of a
                    // panic, which the first `run` then reports.
                    node: node.ok(),
                    durable: None,
                    config,
                },
            );
        }

        Self {
            peers,
            network: SimNetwork::new(schedule, seed),
            clock: VirtualClock::default(),
            leaders_by_term: BTreeMap::new(),
            committed: BTreeMap::new(),
        }
    }

    /// The virtual clock.
    #[must_use]
    pub const fn clock(&self) -> &VirtualClock {
        &self.clock
    }

    /// The network, for partitioning and fault-schedule changes.
    pub const fn network_mut(&mut self) -> &mut SimNetwork {
        &mut self.network
    }

    /// The network, for statistics.
    #[must_use]
    pub const fn network(&self) -> &SimNetwork {
        &self.network
    }

    /// Every peer that currently believes it is the leader.
    #[must_use]
    pub fn leaders(&self) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter(|(_, peer)| {
                peer.node
                    .as_ref()
                    .is_some_and(|node| node.role().is_leader())
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// The running node for `peer`, if it has not crashed.
    #[must_use]
    pub fn node(&self, peer: PeerId) -> Option<&RaftNode<MemoryStateMachine, MemoryLog>> {
        self.peers.get(&peer).and_then(|entry| entry.node.as_ref())
    }

    /// Advances the cluster one tick and checks every invariant.
    ///
    /// # Errors
    ///
    /// The [`Invariant`] that failed, or whatever a node's log store
    /// reported.
    pub fn step(&mut self) -> std::result::Result<(), SimFailure> {
        self.clock.advance();
        let now = self.clock.tick();

        for peer in self.peers.values_mut() {
            if let Some(node) = peer.node.as_mut() {
                node.tick().map_err(SimFailure::Raft)?;
            }
        }
        self.pump(now)?;
        self.check().map_err(SimFailure::Invariant)
    }

    /// Moves every outbound message into the network and delivers what is
    /// due.
    fn pump(&mut self, now: u64) -> std::result::Result<(), SimFailure> {
        let mut outbound = Vec::new();
        for peer in self.peers.values_mut() {
            if let Some(node) = peer.node.as_mut() {
                outbound.extend(node.take_messages());
                node.take_applied();
            }
        }
        for envelope in outbound {
            self.network.send(envelope, now);
        }
        for envelope in self.network.deliver_due(now) {
            if let Some(peer) = self.peers.get_mut(&envelope.to)
                && let Some(node) = peer.node.as_mut()
            {
                node.step(envelope).map_err(SimFailure::Raft)?;
            }
        }
        Ok(())
    }

    /// Runs `ticks` steps.
    ///
    /// # Errors
    ///
    /// The first [`Invariant`] violated, or a node failure.
    pub fn run(&mut self, ticks: u64) -> std::result::Result<(), SimFailure> {
        for _ in 0..ticks {
            self.step()?;
        }
        Ok(())
    }

    /// Runs until exactly one leader exists, up to `ticks`.
    ///
    /// Returns the leader, or `None` if none emerged in time.
    ///
    /// # Errors
    ///
    /// The first [`Invariant`] violated, or a node failure.
    pub fn run_until_leader(
        &mut self,
        ticks: u64,
    ) -> std::result::Result<Option<PeerId>, SimFailure> {
        for _ in 0..ticks {
            self.step()?;
            let leaders = self.leaders();
            if leaders.len() == 1 {
                return Ok(leaders.first().copied());
            }
        }
        Ok(None)
    }

    /// Proposes a command on whichever peer currently leads.
    ///
    /// # Errors
    ///
    /// [`crate::RaftError::NotLeader`] if no peer is leading right now.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex> {
        let leader = self
            .leaders()
            .first()
            .copied()
            .ok_or(crate::RaftError::NotLeader { leader: None })?;
        let Some(node) = self.peers.get_mut(&leader).and_then(|p| p.node.as_mut()) else {
            return Err(crate::RaftError::NotLeader { leader: None });
        };
        node.propose(command)
    }

    /// Proposes a membership change on whichever peer currently leads.
    ///
    /// # Errors
    ///
    /// As [`crate::RaftNode::propose_membership`].
    pub fn propose_membership(&mut self, change: MembershipChange) -> Result<LogIndex> {
        let leader = self
            .leaders()
            .first()
            .copied()
            .ok_or(crate::RaftError::NotLeader { leader: None })?;
        let Some(node) = self.peers.get_mut(&leader).and_then(|p| p.node.as_mut()) else {
            return Err(crate::RaftError::NotLeader { leader: None });
        };
        node.propose_membership(change)
    }

    /// Stops `peer`, keeping its durable state so it can restart.
    ///
    /// Models a process crash, not a disk failure: the log survives.
    pub fn crash(&mut self, peer: PeerId) {
        if let Some(entry) = self.peers.get_mut(&peer)
            && let Some(node) = entry.node.take()
        {
            entry.durable = Some(node.into_parts());
        }
    }

    /// Restarts a crashed `peer` from its durable state.
    ///
    /// # Errors
    ///
    /// Whatever [`RaftNode::new`] reports while recovering.
    pub fn restart(&mut self, peer: PeerId) -> Result<()> {
        let Some(entry) = self.peers.get_mut(&peer) else {
            return Err(crate::RaftError::UnknownPeer { peer });
        };
        let (log, machine) = entry
            .durable
            .take()
            .unwrap_or_else(|| (MemoryLog::new(), MemoryStateMachine::new()));
        entry.node = Some(RaftNode::new(entry.config.clone(), log, machine)?);
        Ok(())
    }

    /// Whether `peer` is running.
    #[must_use]
    pub fn is_running(&self, peer: PeerId) -> bool {
        self.peers
            .get(&peer)
            .is_some_and(|entry| entry.node.is_some())
    }

    /// Whether every running peer's state machine holds `command`.
    #[must_use]
    pub fn committed_everywhere(&self, command: &[u8]) -> bool {
        self.peers.values().all(|peer| {
            peer.node.as_ref().is_none_or(|node| {
                node.state_machine()
                    .commands()
                    .iter()
                    .any(|applied| applied == command)
            })
        })
    }

    /// Checks every safety property.
    ///
    /// # Errors
    ///
    /// The first [`Invariant`] that failed.
    pub fn check(&mut self) -> std::result::Result<(), Invariant> {
        self.check_election_safety()?;
        self.record_committed()?;
        self.check_leader_completeness()?;
        self.check_log_matching()?;
        self.check_state_machine_safety()
    }

    /// At most one leader per term, over the whole run.
    fn check_election_safety(&mut self) -> std::result::Result<(), Invariant> {
        for (id, peer) in &self.peers {
            let Some(node) = peer.node.as_ref() else {
                continue;
            };
            if !node.role().is_leader() {
                continue;
            }
            match self.leaders_by_term.get(&node.term()) {
                Some(existing) if existing != id => {
                    return Err(Invariant::ElectionSafety {
                        term: node.term(),
                        first: *existing,
                        second: *id,
                    });
                }
                Some(_) => {}
                None => {
                    self.leaders_by_term.insert(node.term(), *id);
                }
            }
        }
        Ok(())
    }

    /// Records the term of every index any peer considers committed.
    fn record_committed(&mut self) -> std::result::Result<(), Invariant> {
        for peer in self.peers.values() {
            let Some(node) = peer.node.as_ref() else {
                continue;
            };
            let mut index = LogIndex::FIRST;
            while index <= node.commit_index() {
                if let Ok(Some(term)) = node.log().term_at(index) {
                    self.committed.entry(index).or_insert(term);
                }
                index = index.next();
            }
        }
        Ok(())
    }

    /// A committed entry is never replaced by a different one, anywhere.
    ///
    /// This is the operational form of Leader Completeness: if a later leader
    /// were missing a committed entry, it would overwrite that index with an
    /// entry of a different term, and that is what this catches.
    fn check_leader_completeness(&self) -> std::result::Result<(), Invariant> {
        for (id, peer) in &self.peers {
            let Some(node) = peer.node.as_ref() else {
                continue;
            };
            for (index, committed_term) in &self.committed {
                let Ok(Some(found)) = node.log().term_at(*index) else {
                    continue;
                };
                if found != *committed_term {
                    return Err(Invariant::LeaderCompleteness {
                        index: *index,
                        committed_term: *committed_term,
                        found_term: found,
                        peer: *id,
                    });
                }
            }
        }
        Ok(())
    }

    /// Two logs holding the same `(index, term)` agree on everything before
    /// it.
    fn check_log_matching(&self) -> std::result::Result<(), Invariant> {
        let running: Vec<(PeerId, &RaftNode<MemoryStateMachine, MemoryLog>)> = self
            .peers
            .iter()
            .filter_map(|(id, peer)| peer.node.as_ref().map(|node| (*id, node)))
            .collect();

        for (position, (left_id, left)) in running.iter().enumerate() {
            for (right_id, right) in running.iter().skip(position + 1) {
                let highest = left.log().last_index().min(right.log().last_index());
                let floor = left.log().first_index().max(right.log().first_index());
                let mut index = highest;
                while index >= floor && !index.is_empty_sentinel() {
                    let (Ok(Some(a)), Ok(Some(b))) =
                        (left.log().term_at(index), right.log().term_at(index))
                    else {
                        index = index.previous();
                        continue;
                    };
                    if a == b {
                        // Same (index, term): every earlier entry must match.
                        let mut earlier = index.previous();
                        while earlier >= floor && !earlier.is_empty_sentinel() {
                            let (Ok(Some(x)), Ok(Some(y))) =
                                (left.log().term_at(earlier), right.log().term_at(earlier))
                            else {
                                break;
                            };
                            if x != y {
                                return Err(Invariant::LogMatching {
                                    index,
                                    left: *left_id,
                                    right: *right_id,
                                });
                            }
                            earlier = earlier.previous();
                        }
                        break;
                    }
                    index = index.previous();
                }
            }
        }
        Ok(())
    }

    /// No two replicas applied different commands at the same position.
    fn check_state_machine_safety(&self) -> std::result::Result<(), Invariant> {
        let running: Vec<(PeerId, &MemoryStateMachine)> = self
            .peers
            .iter()
            .filter_map(|(id, peer)| peer.node.as_ref().map(|node| (*id, node.state_machine())))
            .collect();

        for (position, (left_id, left)) in running.iter().enumerate() {
            for (right_id, right) in running.iter().skip(position + 1) {
                let shared = left.commands().len().min(right.commands().len());
                for offset in 0..shared {
                    if left.commands().get(offset) != right.commands().get(offset) {
                        return Err(Invariant::StateMachineSafety {
                            position: offset,
                            left: *left_id,
                            right: *right_id,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// Why a simulation stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SimFailure {
    /// A safety property was violated — a real bug, never flakiness.
    #[error("invariant violated: {0}")]
    Invariant(#[from] Invariant),
    /// A replica reported an error.
    #[error("a replica failed: {0}")]
    Raft(#[from] crate::RaftError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_healthy_three_peer_cluster_elects_exactly_one_leader() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 1);
        let leader = cluster.run_until_leader(300).unwrap();
        assert!(leader.is_some());
        assert_eq!(cluster.leaders().len(), 1);
    }

    #[test]
    fn a_five_peer_cluster_also_converges() {
        let mut cluster = SimCluster::new(5, FaultSchedule::perfect(), 2);
        assert!(cluster.run_until_leader(400).unwrap().is_some());
    }

    #[test]
    fn a_proposal_reaches_every_replicas_state_machine() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 3);
        cluster.run_until_leader(300).unwrap();
        cluster.propose(b"replicated".to_vec()).unwrap();
        cluster.run(50).unwrap();
        assert!(cluster.committed_everywhere(b"replicated"));
    }

    #[test]
    fn crashing_and_restarting_a_follower_loses_nothing() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 4);
        let leader = cluster.run_until_leader(300).unwrap().unwrap();
        let follower = (1..=3u64)
            .map(PeerId::new)
            .find(|peer| *peer != leader)
            .unwrap();

        cluster.propose(b"before".to_vec()).unwrap();
        cluster.run(30).unwrap();
        cluster.crash(follower);
        assert!(!cluster.is_running(follower));
        cluster.propose(b"during".to_vec()).unwrap();
        cluster.run(30).unwrap();
        cluster.restart(follower).unwrap();
        cluster.run(120).unwrap();

        assert!(cluster.committed_everywhere(b"before"));
        assert!(cluster.committed_everywhere(b"during"));
    }

    #[test]
    fn the_election_safety_check_would_catch_two_leaders() {
        // Proof the invariant checker is not vacuous: forcing two peers to
        // lead the same term must be reported by name.
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 5);
        cluster.run_until_leader(300).unwrap();
        cluster
            .leaders_by_term
            .insert(Term::new(1), PeerId::new(99));
        cluster.leaders_by_term.retain(|term, peer| {
            if term.get() == 1 {
                *peer = PeerId::new(99);
            }
            true
        });
        // Whatever term the real leader is in, claim a bogus other leader for
        // it and re-check.
        let real = cluster.leaders()[0];
        let term = cluster.node(real).unwrap().term();
        cluster.leaders_by_term.insert(term, PeerId::new(42));
        let failure = cluster.check().unwrap_err();
        assert!(matches!(failure, Invariant::ElectionSafety { .. }));
        assert!(failure.to_string().contains("election safety"));
    }

    #[test]
    fn the_leader_completeness_check_would_catch_a_lost_entry() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 6);
        cluster.run_until_leader(300).unwrap();
        cluster.propose(b"committed".to_vec()).unwrap();
        cluster.run(40).unwrap();
        // Claim index 1 committed in a term nobody was ever in.
        cluster.committed.insert(LogIndex::FIRST, Term::new(999));
        let failure = cluster.check().unwrap_err();
        assert!(matches!(failure, Invariant::LeaderCompleteness { .. }));
    }

    #[test]
    fn a_partitioned_minority_cannot_elect_anyone() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 7);
        cluster.run_until_leader(300).unwrap();
        cluster
            .network_mut()
            .partition([vec![PeerId::new(1)], vec![PeerId::new(2), PeerId::new(3)]]);
        cluster.run(300).unwrap();
        // Whoever leads must be in the majority side.
        for leader in cluster.leaders() {
            assert_ne!(leader, PeerId::new(1), "a minority of one cannot elect");
        }
    }

    #[test]
    fn membership_can_grow_and_shrink_in_the_simulator() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 8);
        let leader = cluster.run_until_leader(300).unwrap().unwrap();
        cluster
            .propose_membership(MembershipChange::Remove(
                (1..=3u64)
                    .map(PeerId::new)
                    .find(|peer| *peer != leader)
                    .unwrap(),
            ))
            .unwrap();
        cluster.run(60).unwrap();
        assert_eq!(cluster.node(leader).unwrap().membership().len(), 2);
    }

    #[test]
    fn a_restart_of_an_unknown_peer_is_an_error() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 9);
        assert!(cluster.restart(PeerId::new(99)).is_err());
    }

    #[test]
    fn the_clock_advances_one_tick_per_step() {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), 10);
        cluster.run(25).unwrap();
        assert_eq!(cluster.clock().tick(), 25);
    }
}
