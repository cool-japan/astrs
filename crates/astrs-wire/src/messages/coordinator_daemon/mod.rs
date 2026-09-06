//! `coordinator ↔ daemon`: [`CoordinatorEvent`] and [`DaemonEvent`] (§7.3).
//!
//! | Module | Contents |
//! |---|---|
//! | [`coordinator`] | [`CoordinatorEvent`] — the §24.1 instructions, plus the §6.4 peer-route directive |
//! | [`daemon`] | [`DaemonEvent`] — the twelve §24.1 reports |
//! | [`types`] | [`DaemonRegistration`], [`BuildStep`], [`BuildOutcome`], [`SpawnOutcome`], [`PeerRouteDirective`], [`StateEntry`], [`StateEntryKind`] |
//!
//! Unlike the CLI leg, this one is **not** request/response: both directions
//! push events continuously, and a daemon keeps running its nodes when the link
//! drops. That is why the family carries a heartbeat in each direction and a
//! numbered state-catch-up log — the two ends have to be able to disagree for a
//! while and then re-converge without stopping the robot (§12).
//!
//! ```text
//! coordinator                                  daemon
//!      │◄──────────── Register ──────────────────┤
//!      ├──────────── StateCatchUp ──────────────►│
//!      │◄─────────── StateCatchUpAck ────────────┤
//!      ├──────────── Build / Spawn ─────────────►│
//!      │◄────────── BuildResult / SpawnResult ───┤
//!      ├──────────── AllNodesReady ─────────────►│
//!      │◄─────── Heartbeat / NodeMetrics / Log ──┤
//!      ├──────────── StopDataflow ──────────────►│
//!      │◄────────── NodeStopped / Exit ──────────┤
//! ```
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{
//!     CoordinatorEvent, DaemonEvent, DaemonId, DaemonRegistration, FrameFlags, FrameLimits,
//!     SessionId, WireMessage,
//! };
//!
//! let limits = FrameLimits::network();
//! let register = DaemonEvent::Register(DaemonRegistration::new(
//!     DaemonId::generate(None),
//!     "quic://10.0.0.4:7407",
//!     SessionId::from_u128(1),
//! ));
//! let bytes = register.to_frame(FrameFlags::CRC, &limits)?;
//! assert!(DaemonEvent::from_bytes(&bytes, &limits)?.dataflow().is_none());
//!
//! let catch_up = CoordinatorEvent::StateCatchUp {
//!     seq: 1,
//!     entries: Vec::new(),
//!     final_batch: true,
//! };
//! assert_eq!(catch_up.catch_up_high_water(), Some(1));
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

pub mod coordinator;
pub mod daemon;
pub mod types;

pub use coordinator::CoordinatorEvent;
pub use daemon::DaemonEvent;
pub use types::{
    BuildOutcome, BuildStep, DaemonRegistration, PeerRouteDirective, SpawnOutcome, StateEntry,
    StateEntryKind,
};
