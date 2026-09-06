//! Raft consensus over [`astrs_wire`]: coordinator high availability.
//!
//! A single coordinator is a single point of failure for the entire control
//! plane (blueprint §22 lists coordinator HA as the missing piece). This
//! crate replicates the coordinator's authoritative state — the dataflow
//! registry, placement decisions and daemon membership — as a Raft log across
//! an odd-sized peer set, so losing a coordinator costs one election timeout
//! instead of the fleet.
//!
//! # What is here
//!
//! | Module | What it holds |
//! |---|---|
//! | [`types`] | [`Term`], [`LogIndex`], [`PeerId`], [`Role`] |
//! | [`log`] | [`LogEntry`], the [`LogStore`] contract, [`MemoryLog`], [`WalLog`] |
//! | [`membership`] | [`Membership`] and single-server [`MembershipChange`] |
//! | [`message`] | the Raft RPC enum and its envelope |
//! | [`state_machine`] | the [`StateMachine`] a caller plugs in |
//! | [`core`] | [`RaftNode`]: Figure 2 plus pre-vote, snapshots and membership |
//! | [`transport`] | the [`Transport`] contract and an in-process implementation |
//! | [`tcp`] | a TCP transport framed exactly like the rest of AstRS |
//! | [`driver`] | the `tokio` task that drives a [`RaftNode`] in a real process |
//! | [`sim`] | a deterministic simulator: virtual clock, faulty network |
//!
//! # Why Raft rides the existing wire
//!
//! Leader election, `AppendEntries` replication, snapshot install and
//! membership changes are all ordinary request/response exchanges. They
//! travel over the same [`astrs_wire`] framing and the same `oxicode`
//! encoding every other AstRS control message uses, rather than a second
//! transport with its own bugs, its own metrics and its own version skew. A
//! peer that can talk to a coordinator can already talk Raft to it.
//!
//! # The core is a pure state machine
//!
//! [`RaftNode`] performs no I/O of its own: it consumes ticks and inbound
//! messages, and produces outbound messages a caller drains. Every scheduling
//! decision it makes is a function of its inputs and a seeded
//! [`Rng`]. That is what lets [`sim`] run the *same* consensus code as a
//! production replica against a virtual clock and an adversarial network, and
//! replay any failing seed exactly.
//!
//! # Terms are the safety currency
//!
//! Every Raft invariant is ultimately expressed against the monotonic term
//! counter: a peer that observes a term higher than its own steps down to
//! follower before doing anything else, and a message from a lower term is
//! rejected outright. [`Term`] is that counter, with those two comparisons
//! spelled as named methods so no call site has to re-derive which direction
//! of `<` means "stale".
//!
//! ```
//! use astrs_raft::Term;
//!
//! let mut current = Term::new(4);
//!
//! // A peer at a higher term wins: this node must step down.
//! assert!(current.is_stale_against(Term::new(5)));
//!
//! // Standing for election bumps the term first.
//! current = current.next();
//! assert_eq!(current, Term::new(5));
//! assert!(!current.is_stale_against(Term::new(5)));
//! ```

pub mod config;
pub mod core;
pub mod driver;
pub mod error;
pub mod log;
pub mod membership;
pub mod message;
pub mod sim;
pub mod state_machine;
pub mod tcp;
pub mod transport;
pub mod types;

pub use config::RaftConfig;
pub use core::{Applied, RaftNode, RaftStatus};
pub use driver::{RaftHandle, RaftReplica};
pub use error::{RaftError, Result};
pub use log::{
    EntryPayload, FsyncPolicy, HardState, LogEntry, LogStore, MemoryLog, RecoveryOutcome, Snapshot,
    SnapshotMeta, WalLog,
};
pub use membership::{Membership, MembershipChange};
pub use message::{Envelope, RaftMessage};
pub use state_machine::{MemoryStateMachine, StateMachine};
pub use tcp::{TcpTransport, TcpTransportServer};
pub use transport::{ChannelTransport, Transport};
pub use types::{LogIndex, PeerId, Rng, Role, Term};
