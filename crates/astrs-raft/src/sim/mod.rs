//! A deterministic simulator for the consensus code in [`crate::core`].
//!
//! # Why simulate rather than integration-test
//!
//! Every interesting Raft failure needs three things a normal test cannot
//! arrange: a specific interleaving of messages, a specific set of drops, and
//! enough simulated time for timeouts to fire. This module supplies all three
//! and makes them **reproducible** — [`VirtualClock`] removes real time,
//! [`SimNetwork`] draws every fault from a seeded generator, and
//! [`SimCluster`] runs the same [`crate::RaftNode`] a production replica runs.
//! A failure found here is replayed by re-using its seed, not chased.
//!
//! # The safety properties are checked continuously
//!
//! [`SimCluster::check`] runs after every tick, not at the end, and reports
//! the named [`Invariant`] that broke. A violation means a caller could have
//! been told a write succeeded and then read it back missing — which is why it
//! stops the run immediately.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::sim::{FaultSchedule, SimCluster};
//!
//! // Three peers on a network that drops, duplicates and delays.
//! let mut cluster = SimCluster::new(3, FaultSchedule::chaotic(), 12345);
//! assert!(cluster.run_until_leader(2_000)?.is_some());
//! # Ok::<(), astrs_raft::sim::SimFailure>(())
//! ```

pub mod clock;
pub mod cluster;
pub mod network;

pub use clock::VirtualClock;
pub use cluster::{Invariant, SimCluster, SimFailure};
pub use network::{FaultSchedule, SimNetwork};
