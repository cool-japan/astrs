//! The shared-memory plane, brokered (§6.2, §6.3).
//!
//! > *The daemon holds every segment fd … it issues `RouteUpgrade` to the
//! > producer, which switches to direct SHM publishing. Any consumer
//! > crash/downgrade flips the route back. This removes dora's ack-window
//! > heuristics — the daemon **knows** attachment state because it brokers the
//! > segment fds.*
//!
//! `astrs-shm` owns the ring: the layout, the slot protocol, the drop-token
//! reclamation, the descriptor passing. This module owns the *decision*: which
//! routes deserve a ring, when every consumer has arrived, what to tell the
//! producer, and what to do when any of it comes apart.
//!
//! | Module | Concern |
//! |---|---|
//! | [`keys`] | [`OutputKey`]: the `(dataflow, node, output)` a ring belongs to |
//! | [`policy`] | [`ShmPolicy`]: which consumers may attach, and the typed reason when they may not |
//! | [`segments`] | [`SegmentRegistry`]: the broker, the segments, and the daemon's map onto them |
//! | [`attach`] | [`AttachmentLedger`]: expected versus observed consumers |
//! | [`upgrade`] | [`UpgradeTable`]: the reliable → offered → upgraded state machine |
//! | [`bridge`] | [`SegmentBridge`]: the daemon reading a ring, during a transition |
//! | [`plane`] | [`ShmPlane`]: the five above as one thing the event loop holds |
//!
//! # The route's life
//!
//! ```text
//!   Subscribe ──► plan_output ──► segment created, consumers expected
//!                                          │
//!                     consumer maps the ring (broker hands out the fd)
//!                                          │
//!   tick ──► poll ──► every expected consumer observed ──► RouteUpgrade ──┐
//!                                                                         │
//!            producer answers RouteUpgradeAck{accepted:true} ◄────────────┘
//!                                          │
//!                            the daemon steps out of the path
//!                                          │
//!   consumer detaches · crashes · producer dies · pool exhausted
//!                                          │
//!                          RouteDowngrade ──► reliable path resumes
//! ```
//!
//! # Nothing here spawns a task
//!
//! Every transition above is a method the merged event loop calls from its own
//! task ([`crate::server::core`]), for the reason `astrs-shm`'s own broker
//! documentation gives: the daemon already owns a supervision loop, and a
//! second timer would be a second source of truth. The one background thread
//! involved is the broker's `accept(2)` loop, which is about descriptors
//! rather than time.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::shm::{ConsumerFacts, ShmPolicy, ShmVerdict};
//!
//! // A dynamic consumer never joins a ring: nobody spawned it, and its
//! // lifetime is not the daemon's to reason about.
//! let policy = ShmPolicy::new();
//! assert!(!policy.verdict(&ConsumerFacts::same_host().dynamic()).is_eligible());
//! assert_eq!(policy.verdict(&ConsumerFacts::same_host()), ShmVerdict::Eligible);
//! ```

pub mod attach;
pub mod bridge;
pub mod keys;
pub mod plane;
pub mod policy;
pub mod segments;
pub mod upgrade;

pub use attach::{AttachmentDelta, AttachmentLedger};
pub use bridge::{BridgedMessage, DEFAULT_DRAIN_BATCH, SegmentBridge};
pub use keys::OutputKey;
pub use plane::{PlaneOutcome, ShmPlane};
pub use policy::{ConsumerFacts, DEFAULT_SEGMENT_BUDGET, ShmPolicy, ShmRefusal, ShmVerdict};
pub use segments::{CONSUMER_STALE_AFTER, SegmentRecord, SegmentRegistry};
pub use upgrade::{
    InputRouteKey, InputRouteTable, UPGRADE_ACK_TIMEOUT, UpgradeAction, UpgradeState, UpgradeTable,
};
