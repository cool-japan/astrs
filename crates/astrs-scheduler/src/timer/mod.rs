//! The hierarchical timer wheel subsystem (blueprint §11.1).
//!
//! - [`TimerWheel`] — the synchronous, clock-agnostic wheel itself. See its
//!   module docs ([`wheel`]) for the cascade structure and correctness
//!   guarantees.
//! - [`TimerWheelDriver`] — an async task that ticks a [`TimerWheel`] once
//!   per real millisecond and forwards fired ticks over a channel, for
//!   production use.
//! - For deterministic tests and §14 replay, drive a [`TimerWheel`]
//!   directly with [`TimerWheel::advance`], feeding it [`std::time::Instant`]
//!   values sourced from an [`astrs_time::ManualClock`] — no driver task,
//!   no real waiting, exact and reproducible.

mod driver;
mod p2;
mod wheel;

pub use driver::{TimerWheelDriver, TimerWheelHandle};
pub use p2::JitterStats;
pub use wheel::{DEFAULT_BURST_CAP, MissedTickPolicy, TimerFired, TimerId, TimerSpec, TimerWheel};
