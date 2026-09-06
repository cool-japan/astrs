//! Event scheduling and delivery policy for AstRS (blueprint §11).
//!
//! This is the layer that decides what a node's merged event loop
//! (blueprint §4.3) sees next, and when. `astrs-daemon` and `astrs-node-api`
//! are this crate's two intended consumers — the daemon multiplexing many
//! nodes' routes, the node API multiplexing one node's own inputs — and
//! both are meant to reuse the exact same mechanism, which is why every
//! piece here is generic over the message payload rather than tied to
//! either crate's own event type.
//!
//! Five independent pieces, composed by the caller (see the example
//! below), not by this crate:
//!
//! - [`InputQueue`] — a bounded, policy-driven, eviction-immunity-aware
//!   queue for one input (§11.2): `queue_size`,
//!   [`astrs_wire::QueuePolicy::DropOldest`] or
//!   [`astrs_wire::QueuePolicy::Backpressure`], and immunity for Stop-class control
//!   events and metadata-correlated messages (`request_id`/`goal_id`/
//!   `goal_status`). [`InputQueue::from_spec`] builds one directly from an
//!   [`astrs_wire::InputSpec`].
//! - [`EventMux`] — the prioritized, fair, multi-input merged event loop
//!   (§11.3): the control lane strictly pre-empts the data lane; within a
//!   lane, round robin guarantees no input starves.
//!   [`EventMux::register_from_spec`] is the mux-level equivalent of
//!   `InputQueue::from_spec`, and [`EventMux::snapshot_all`] is the one
//!   call a metrics sampler needs for every registered input at once.
//! - [`TimerWheel`] (+ [`TimerWheelDriver`]) — the hierarchical timing
//!   wheel serving every `astrs/timer/*` subscription (§11.1) with
//!   drift-free absolute scheduling and a configurable
//!   [`MissedTickPolicy`]. A [`TimerSpec::tag`] rides along on every
//!   [`TimerFired`] it produces, so a daemon multiplexing many nodes'
//!   timers through one wheel can tell which node/input a tick belongs to
//!   without a side table keyed by the wheel's own opaque [`TimerId`].
//! - [`DeadlineMonitor`] — per-input input-to-output latency budgets
//!   (§11.3), token-based so pipelined measurements never clobber each
//!   other. [`DeadlineMonitor::register_from_spec`] reads the budget
//!   straight off an [`astrs_wire::InputSpec::deadline`].
//! - [`IdleWatchdog`] — per-input silence detection (§8.3, §12): "declare
//!   the input closed if nothing arrives for this long", the one question
//!   [`DeadlineMonitor`] does not answer (it measures a span's *duration*,
//!   not a route's *silence*). Edge-triggered — [`IdleWatchdog::poll_all`]
//!   reports a key exactly once per crossing into or out of silence, never
//!   once per health-check tick spent silent.
//!   [`IdleWatchdog::register_from_spec`] reads the budget straight off an
//!   [`astrs_wire::InputSpec::timeout`].
//!
//! # This crate does not log
//!
//! Nothing here writes a log line, increments a named metric, or emits an
//! `astrs/status` event on its own — this crate has no `astrs-log` or
//! `astrs-telemetry` dependency by design. Every condition worth a
//! caller's attention (a backpressure-exhausted drop, an eviction-immunity
//! escalation, a deadline violation, a coalesced timer tick) is instead
//! surfaced as an ordinary return value —
//! [`QueueSignal`] on [`PushReport`], [`DeadlineOutcome`], [`TimerFired`] —
//! for the daemon or node API to turn into whatever its own observability
//! stack expects. This keeps the scheduling mechanism testable in
//! isolation (every test in this crate is a plain, synchronous assertion
//! on a returned value, no log capture required) and keeps one crate's
//! logging opinions out of another's hot path.
//!
//! # Reusing `astrs-wire`'s vocabulary
//!
//! [`QueuePolicy`](astrs_wire::QueuePolicy), [`PriorityLane`](astrs_wire::PriorityLane),
//! and [`astrs_wire::InputSpec`] (which already carries `queue_size`,
//! `queue_policy`, `priority_lane`, `deadline`, and `timeout` per
//! manifest-resolved input — every field this crate's five pieces need is
//! read straight off it, via `from_spec`/`register_from_spec` on each one)
//! are consumed directly from `astrs-wire`, not redefined here —
//! the blueprint's "one spec per concern" principle (§3) means the wire
//! format for a queue policy and the runtime enum a scheduler matches on
//! are the same type, so a daemon parsing a manifest's `queue_policy:
//! backpressure` and a node API applying it are provably talking about the
//! same thing. [`MetadataView`] is the one seam this crate does introduce:
//! it reads [`astrs_wire::Metadata`] to decide eviction immunity without
//! needing to know a caller's full event-enum shape — and this crate
//! implements it directly on [`astrs_wire::NodeEvent`] itself (the daemon →
//! node wire event, and `astrs-node-api`'s eventual event-loop type), not
//! only on the [`Envelope`] stand-in the example below uses for brevity.
//!
//! # Example: wiring one node's merged event loop
//!
//! A sketch of how the five pieces come together. Real callers typically
//! implement [`MetadataView`] directly on their own richer event enum (see
//! that trait's docs for the pattern); this uses [`Envelope`] to keep the
//! wiring itself visible.
//!
//! ```
//! use astrs_scheduler::{
//!     DeadlineMonitor, Envelope, EventMux, IdleWatchdog, MissedTickPolicy, TimerSpec, TimerWheel,
//! };
//! use astrs_time::TimerInterval;
//! use astrs_wire::{DataId, PriorityLane, QueuePolicy};
//! use std::time::{Duration, Instant};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // One mux per node; one input per manifest `inputs:` entry.
//! let mux: EventMux<Envelope<u32>> = EventMux::new();
//! let frames = mux.register_input(
//!     DataId::new("frames")?, 10, QueuePolicy::DropOldest, PriorityLane::Data,
//! )?;
//! let status = mux.register_input(
//!     DataId::new("status")?, 4, QueuePolicy::DropOldest, PriorityLane::Control,
//! )?;
//!
//! frames.push(Envelope::new(42)); // an ordinary data message
//! status.push(Envelope { payload: 0, metadata: None, stop: true }); // Stop-class
//!
//! // The control lane is served first, regardless of arrival order or how
//! // much data-lane backlog exists.
//! let (first, event) = mux.try_recv().expect("something is queued");
//! assert_eq!(first, DataId::new("status")?);
//! assert!(event.stop);
//!
//! // One timer wheel per daemon serves every `astrs/timer/*` subscription.
//! let mut wheel = TimerWheel::new(Instant::now());
//! let spec = TimerSpec::new(TimerInterval::from_hz(50.0)?, MissedTickPolicy::Skip)
//!     .with_tag(7); // e.g. an index into the daemon's own node/input table
//! wheel.insert(spec, Instant::now());
//! // In production a `TimerWheelDriver` calls `wheel.advance(now)` once per
//! // real millisecond; a deterministic test or a §14 replay instead calls
//! // it directly against `astrs_time::ManualClock`-sourced instants (see
//! // `astrs_time::Clock::now_instant`).
//!
//! // One deadline monitor per daemon, one budget per monitored input.
//! let deadlines: DeadlineMonitor<DataId> = DeadlineMonitor::new();
//! deadlines.register(DataId::new("frames")?, Duration::from_millis(50));
//! let token = deadlines.start(&DataId::new("frames")?, Instant::now()).expect("registered");
//! assert!(!deadlines.finish_now(token).is_violated());
//!
//! // One idle watchdog per daemon, one silence budget per monitored input —
//! // "declare the input closed if nothing arrives for this long" (§8.3),
//! // a distinct question from the deadline monitor's "was this span slow".
//! let idle: IdleWatchdog<DataId> = IdleWatchdog::new();
//! let now = Instant::now();
//! idle.register(DataId::new("frames")?, Duration::from_secs(5), now);
//! idle.touch(&DataId::new("frames")?, now); // a message just arrived
//! assert!(idle.poll_all(now + Duration::from_secs(1)).is_empty(), "well within budget");
//! # Ok(())
//! # }
//! ```

// `missing_docs` and the clippy `unwrap_used`/`expect_used`/`panic`/
// `dbg_macro`/`todo`/`unimplemented` denials come from `[lints] workspace =
// true` in this crate's Cargo.toml (workspace policy), so they are not
// repeated here as source attributes.

mod deadline;
mod error;
mod idle;
mod metadata_view;
mod mux;
mod queue;
mod sync_util;
mod timer;

pub use deadline::{DeadlineMonitor, DeadlineOutcome, DeadlineSnapshot, DeadlineToken};
pub use error::{Result, SchedulerError};
pub use idle::{IdleSnapshot, IdleViolation, IdleWatchdog};
pub use metadata_view::{Envelope, MetadataView};
pub use mux::{EventMux, InputHandle};
pub use queue::{InputQueue, PushOutcome, PushReport, QueueSignal, QueueSnapshot};
pub use timer::{
    DEFAULT_BURST_CAP, JitterStats, MissedTickPolicy, TimerFired, TimerId, TimerSpec, TimerWheel,
    TimerWheelDriver, TimerWheelHandle,
};
