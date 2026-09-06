//! [`RaftConfig`]: this replica's identity, its peers, and its timing.
//!
//! # Time is counted in ticks, not durations
//!
//! Every timeout in [`crate::RaftNode`] is expressed as a number of *ticks*,
//! and the node advances only when someone calls
//! [`crate::RaftNode::tick`]. The mapping from a tick to wall-clock time is
//! [`RaftConfig::tick_interval`], and it is the driver's business alone
//! ([`crate::driver`]) — the consensus code never reads a clock. That is what
//! makes the deterministic simulator possible: [`crate::sim`] advances the
//! same tick counter a thousand times faster than real time and gets
//! byte-identical behaviour.
//!
//! # The timing constraint that matters
//!
//! Raft needs `broadcastTime ≪ electionTimeout ≪ MTBF`. The half this crate
//! can check is that the heartbeat interval is comfortably shorter than the
//! election timeout — a leader whose heartbeats arrive no faster than
//! followers time out will be deposed constantly. [`RaftConfig::validate`]
//! refuses a configuration that gets that backwards.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{PeerId, RaftConfig};
//!
//! let config = RaftConfig::new(PeerId::new(1))
//!     .with_peer(PeerId::new(1), "127.0.0.1:7501".parse()?)
//!     .with_peer(PeerId::new(2), "127.0.0.1:7502".parse()?)
//!     .with_peer(PeerId::new(3), "127.0.0.1:7503".parse()?);
//!
//! config.validate()?;
//! assert_eq!(config.initial_membership().len(), 3);
//! assert!(config.pre_vote);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use crate::error::{RaftError, Result};
use crate::membership::Membership;
use crate::types::PeerId;

/// The default wall-clock length of one tick.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(50);

/// The default election timeout floor, in ticks (500 ms at the default tick).
pub const DEFAULT_ELECTION_TIMEOUT_TICKS: u64 = 10;

/// The default jitter added to the election timeout, in ticks.
///
/// Randomizing the timeout is what breaks split votes: without it, peers that
/// time out together campaign together, forever.
pub const DEFAULT_ELECTION_JITTER_TICKS: u64 = 10;

/// The default heartbeat interval, in ticks (100 ms at the default tick).
pub const DEFAULT_HEARTBEAT_TICKS: u64 = 2;

/// The default cap on entries in one `AppendEntries`.
pub const DEFAULT_MAX_ENTRIES_PER_APPEND: usize = 256;

/// The default number of applied entries beyond the last snapshot before the
/// replica compacts.
pub const DEFAULT_SNAPSHOT_THRESHOLD: u64 = 4096;

/// One Raft replica's identity, peer set and timing policy.
#[derive(Debug, Clone)]
pub struct RaftConfig {
    /// This replica's id.
    pub id: PeerId,
    /// Every peer in the initial cluster, including this one, and where to
    /// reach it.
    ///
    /// The address is *routing* information, not identity: a peer that moves
    /// keeps its id, and rewriting this map on restart invalidates no durable
    /// record. It is also what a server built on this crate turns into a
    /// client-visible leader hint.
    pub peers: BTreeMap<PeerId, SocketAddr>,
    /// The shortest election timeout, in ticks.
    pub election_timeout_ticks: u64,
    /// The random extra added to the election timeout, in ticks.
    pub election_jitter_ticks: u64,
    /// How often a leader sends heartbeats, in ticks.
    pub heartbeat_ticks: u64,
    /// The wall-clock length of one tick, for the driver.
    pub tick_interval: Duration,
    /// The most entries one `AppendEntries` may carry.
    pub max_entries_per_append: usize,
    /// How many entries may accumulate past the last snapshot before the
    /// replica compacts. `0` disables automatic compaction.
    pub snapshot_threshold: u64,
    /// Whether to run the pre-vote round before campaigning.
    ///
    /// On by default. A partitioned peer without pre-vote returns with an
    /// inflated term and deposes a perfectly healthy leader; with it, that
    /// peer discovers it would lose and stays quiet.
    pub pre_vote: bool,
    /// The seed for this replica's election-timeout randomness.
    ///
    /// Fixed rather than drawn from the OS so a simulation seed reproduces a
    /// failure exactly. A real deployment should vary it per replica — which
    /// [`RaftConfig::new`] does, by deriving it from the peer id.
    pub seed: u64,
}

impl RaftConfig {
    /// A configuration for `id` with the default timing and no peers yet.
    #[must_use]
    pub fn new(id: PeerId) -> Self {
        Self {
            id,
            peers: BTreeMap::new(),
            election_timeout_ticks: DEFAULT_ELECTION_TIMEOUT_TICKS,
            election_jitter_ticks: DEFAULT_ELECTION_JITTER_TICKS,
            heartbeat_ticks: DEFAULT_HEARTBEAT_TICKS,
            tick_interval: DEFAULT_TICK_INTERVAL,
            max_entries_per_append: DEFAULT_MAX_ENTRIES_PER_APPEND,
            snapshot_threshold: DEFAULT_SNAPSHOT_THRESHOLD,
            pre_vote: true,
            // Distinct per replica so three peers started from the same
            // template do not draw identical election timeouts and split the
            // vote forever.
            seed: id.get().wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    /// Adds (or replaces) one peer's address.
    #[must_use]
    pub fn with_peer(mut self, id: PeerId, address: SocketAddr) -> Self {
        self.peers.insert(id, address);
        self
    }

    /// Replaces the whole peer map.
    #[must_use]
    pub fn with_peers(mut self, peers: impl IntoIterator<Item = (PeerId, SocketAddr)>) -> Self {
        self.peers = peers.into_iter().collect();
        self
    }

    /// Sets the election timeout floor and its jitter.
    #[must_use]
    pub const fn with_election_timeout(mut self, ticks: u64, jitter: u64) -> Self {
        self.election_timeout_ticks = ticks;
        self.election_jitter_ticks = jitter;
        self
    }

    /// Sets the heartbeat interval.
    #[must_use]
    pub const fn with_heartbeat_ticks(mut self, ticks: u64) -> Self {
        self.heartbeat_ticks = ticks;
        self
    }

    /// Sets the wall-clock length of one tick.
    #[must_use]
    pub const fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Sets the automatic-compaction threshold; `0` disables it.
    #[must_use]
    pub const fn with_snapshot_threshold(mut self, entries: u64) -> Self {
        self.snapshot_threshold = entries;
        self
    }

    /// Turns the pre-vote round on or off.
    #[must_use]
    pub const fn with_pre_vote(mut self, enabled: bool) -> Self {
        self.pre_vote = enabled;
        self
    }

    /// Sets the election-randomness seed.
    #[must_use]
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// The membership implied by [`RaftConfig::peers`] — the configuration a
    /// brand-new cluster bootstraps with.
    #[must_use]
    pub fn initial_membership(&self) -> Membership {
        Membership::new(self.peers.keys().copied())
    }

    /// Where `peer` can be reached, if this replica knows.
    #[must_use]
    pub fn address_of(&self, peer: PeerId) -> Option<SocketAddr> {
        self.peers.get(&peer).copied()
    }

    /// The leader-lease length, in ticks.
    ///
    /// A leader may serve a read locally for this long after the last round
    /// of heartbeat responses that reached a quorum. It is the **election
    /// timeout floor**: no follower can start an election before its own
    /// timeout elapses, so within one election timeout of a confirmed quorum
    /// heartbeat no other leader can exist. The lease is deliberately not
    /// longer than that floor even though the average timeout is longer,
    /// because the *floor* is what an adversarial schedule will pick.
    #[must_use]
    pub const fn lease_ticks(&self) -> u64 {
        self.election_timeout_ticks
    }

    /// Refuses a configuration that cannot work.
    ///
    /// # Errors
    ///
    /// [`RaftError::InvalidConfig`] when this replica is not in its own peer
    /// set, when the peer set is empty, when the heartbeat interval is not
    /// shorter than the election timeout, or when a timeout is zero.
    pub fn validate(&self) -> Result<()> {
        if self.peers.is_empty() {
            return Err(RaftError::InvalidConfig {
                reason: "a Raft replica needs at least one peer, itself included",
            });
        }
        if !self.peers.contains_key(&self.id) {
            return Err(RaftError::InvalidConfig {
                reason: "this replica's own id must appear in its peer set",
            });
        }
        if self.election_timeout_ticks == 0 {
            return Err(RaftError::InvalidConfig {
                reason: "the election timeout must be at least one tick",
            });
        }
        if self.heartbeat_ticks == 0 {
            return Err(RaftError::InvalidConfig {
                reason: "the heartbeat interval must be at least one tick",
            });
        }
        if self.heartbeat_ticks >= self.election_timeout_ticks {
            return Err(RaftError::InvalidConfig {
                reason: "heartbeats must be sent well inside the election timeout",
            });
        }
        if self.tick_interval.is_zero() {
            return Err(RaftError::InvalidConfig {
                reason: "a tick must have a non-zero duration",
            });
        }
        if self.max_entries_per_append == 0 {
            return Err(RaftError::InvalidConfig {
                reason: "an AppendEntries must be allowed to carry at least one entry",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn address(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn three_peers() -> RaftConfig {
        RaftConfig::new(PeerId::new(1))
            .with_peer(PeerId::new(1), address(7501))
            .with_peer(PeerId::new(2), address(7502))
            .with_peer(PeerId::new(3), address(7503))
    }

    #[test]
    fn a_three_peer_configuration_validates() {
        let config = three_peers();
        config.validate().unwrap();
        assert_eq!(config.initial_membership().len(), 3);
        assert_eq!(config.address_of(PeerId::new(2)), Some(address(7502)));
        assert_eq!(config.address_of(PeerId::new(9)), None);
    }

    #[test]
    fn a_replica_missing_from_its_own_peer_set_is_refused() {
        let config = RaftConfig::new(PeerId::new(4))
            .with_peer(PeerId::new(1), address(7501))
            .with_peer(PeerId::new(2), address(7502));
        assert!(matches!(
            config.validate().unwrap_err(),
            RaftError::InvalidConfig { .. }
        ));
    }

    #[test]
    fn an_empty_peer_set_is_refused() {
        assert!(matches!(
            RaftConfig::new(PeerId::new(1)).validate().unwrap_err(),
            RaftError::InvalidConfig { .. }
        ));
    }

    #[test]
    fn heartbeats_must_fit_inside_the_election_timeout() {
        // The whole point of the constraint: a leader whose heartbeats are
        // no faster than the followers' timeout is deposed constantly.
        let config = three_peers()
            .with_election_timeout(4, 0)
            .with_heartbeat_ticks(4);
        assert!(matches!(
            config.validate().unwrap_err(),
            RaftError::InvalidConfig { .. }
        ));
        let ok = three_peers()
            .with_election_timeout(10, 5)
            .with_heartbeat_ticks(2);
        ok.validate().unwrap();
    }

    #[test]
    fn zero_timings_are_refused() {
        assert!(
            three_peers()
                .with_election_timeout(0, 0)
                .validate()
                .is_err()
        );
        assert!(three_peers().with_heartbeat_ticks(0).validate().is_err());
        assert!(
            three_peers()
                .with_tick_interval(Duration::ZERO)
                .validate()
                .is_err()
        );
        let mut zero_batch = three_peers();
        zero_batch.max_entries_per_append = 0;
        assert!(zero_batch.validate().is_err());
    }

    #[test]
    fn replicas_built_from_the_same_template_get_different_seeds() {
        // Identical seeds would draw identical election timeouts and split
        // the vote indefinitely.
        let seeds: std::collections::BTreeSet<u64> = (1..=5)
            .map(|id| RaftConfig::new(PeerId::new(id)).seed)
            .collect();
        assert_eq!(seeds.len(), 5);
        assert!(seeds.iter().all(|&seed| seed != 0));
    }

    #[test]
    fn the_lease_is_the_election_timeout_floor() {
        let config = three_peers().with_election_timeout(10, 50);
        // Not the floor plus the jitter: an adversarial follower will pick
        // the floor, and the lease must be safe against that one.
        assert_eq!(config.lease_ticks(), 10);
    }

    #[test]
    fn builders_reach_every_knob() {
        let config = three_peers()
            .with_peers([(PeerId::new(1), address(1)), (PeerId::new(2), address(2))])
            .with_snapshot_threshold(16)
            .with_pre_vote(false)
            .with_seed(99)
            .with_tick_interval(Duration::from_millis(5));
        assert_eq!(config.peers.len(), 2);
        assert_eq!(config.snapshot_threshold, 16);
        assert!(!config.pre_vote);
        assert_eq!(config.seed, 99);
        assert_eq!(config.tick_interval, Duration::from_millis(5));
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let config = RaftConfig::new(PeerId::new(1));
        assert_eq!(
            config.election_timeout_ticks,
            DEFAULT_ELECTION_TIMEOUT_TICKS
        );
        assert_eq!(config.election_jitter_ticks, DEFAULT_ELECTION_JITTER_TICKS);
        assert_eq!(config.heartbeat_ticks, DEFAULT_HEARTBEAT_TICKS);
        assert_eq!(config.tick_interval, DEFAULT_TICK_INTERVAL);
        assert_eq!(config.snapshot_threshold, DEFAULT_SNAPSHOT_THRESHOLD);
        assert_eq!(
            config.max_entries_per_append,
            DEFAULT_MAX_ENTRIES_PER_APPEND
        );
        assert!(config.pre_vote);
    }
}
