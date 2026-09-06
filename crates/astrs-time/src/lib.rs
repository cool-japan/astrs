//! Time and causality primitives for AstRS.
//!
//! A first-party hybrid logical clock (the `uhlc` replacement — see the
//! blueprint's dependency policy, §18.1, which bans `uhlc` outright) plus
//! the surrounding time vocabulary the rest of the stack shares
//! (blueprint §4.3):
//!
//! - [`HlcTimestamp`] — a compact, totally-ordered hybrid logical clock
//!   timestamp: a 64-bit physical nanosecond count paired with a 32-bit
//!   logical tie-breaking counter. `Clone`/`Copy`/`Eq`/`Ord`/`Hash`,
//!   `serde`, `oxicode`, and a `Display`/`FromStr` string form.
//! - [`HlcClock`] — issues [`HlcTimestamp`]s that are monotone even when
//!   the underlying wall clock is not: [`HlcClock::now`] (the HLC "send"
//!   rule) and [`HlcClock::update_with`] (the "receive" rule, with
//!   configurable maximum clock drift rejection).
//! - [`Stamped<T>`] — the universal event wrapper every AstRS merged event
//!   loop uses (§4.3): a value paired with the [`HlcTimestamp`] it
//!   occurred at.
//! - [`Clock`], [`SystemClock`], [`ManualClock`] — a monotonic/wall clock
//!   abstraction, with a fully controllable clock for deterministic tests
//!   and AstRS's `--deterministic` replay mode (§14).
//! - [`Deadline`] — a monotonic-clock deadline with checked arithmetic.
//! - [`parse_duration`]/[`format_duration`]/[`HumanDuration`] — human
//!   duration parsing and formatting (`"250ms"`, `"5s"`, `"1.5h"`, ...),
//!   plus `serde` helpers ([`duration::serde_human`]) for manifest fields.
//! - [`TimerInterval`] — drift-free periodic scheduling for the
//!   `astrs/timer/*` virtual sources (§8.4).
//!
//! # Example: an event pipeline
//!
//! ```
//! use astrs_time::{HlcClock, ManualClock};
//!
//! let clock = HlcClock::new(ManualClock::new(1_000));
//! let event = clock.stamp("camera-frame-042");
//! assert_eq!(event.inner, "camera-frame-042");
//!
//! let next_event = clock.stamp("camera-frame-043");
//! assert!(next_event.ts > event.ts);
//! ```

// `missing_docs` and the clippy `unwrap_used`/`expect_used`/`panic`/
// `dbg_macro`/`todo`/`unimplemented` denials come from `[lints] workspace =
// true` in this crate's Cargo.toml (workspace policy), so they are not
// repeated here as source attributes.

pub mod clock;
pub mod deadline;
pub mod duration;
pub mod hlc;
pub mod interval;
pub mod stamped;
pub mod timestamp;

pub use clock::{Clock, ManualClock, SystemClock};
pub use deadline::Deadline;
pub use duration::{DurationParseError, HumanDuration, format_duration, parse_duration};
pub use hlc::{DEFAULT_MAX_CLOCK_DRIFT, HlcClock, HlcDriftError, HlcError};
pub use interval::{TimerInterval, TimerIntervalError, TimerSourcePathError};
pub use stamped::Stamped;
pub use timestamp::{HlcTimestamp, HlcTimestampParseError};
