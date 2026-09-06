//! Structured logging for AstRS dataflows.
//!
//! The log plumbing behind `astrs logs -f` and the `astrs/logs/*` virtual
//! inputs (blueprint §13, §8.4):
//!
//! - [`HlcTimestamp`] — re-exported from `astrs-time` (blueprint §5.2
//!   assigns the hybrid logical clock to that crate; §4.3: "every event is
//!   `Stamped<T>` — an HLC timestamp from `astrs-time`"). Every
//!   [`LogRecord`] carries one, so a caller that also depends on
//!   `astrs-time` directly (every layer above this one eventually will)
//!   never has two distinct types to reconcile — there is exactly one
//!   `HlcTimestamp` in the whole tree.
//! - [`LogRecord`] — the structured log entry and its JSON wire encoding
//!   (the payload format on `astrs/logs/*`), plus [`LogRecord::from_tracing_event`]
//!   for the `tracing` bridge that `astrs-telemetry` (W2) builds on top of
//!   this crate, and [`LogRecord::from_captured_output`] for wrapping a
//!   spawned node's captured stdout/stderr (capture itself is
//!   `astrs-daemon`'s job).
//! - [`LogLevel`] — severity, with `FromStr`/`Display` and a total,
//!   direction-flipped conversion to/from [`tracing::Level`].
//! - [`RotatingWriter`] — a size-rotating, retention-limited, `Mutex`-guarded
//!   line-oriented JSON file writer for on-disk daemon logs.
//! - [`LogFileReader`] — reads a rotated log file back into [`LogRecord`]s.
//! - [`mod@format`] — deterministic human-text and JSON rendering.
//! - [`LogMerger`] / [`merge_logs`] — an HLC-ordered k-way merge of
//!   multiple `LogRecord` streams, the core of cross-machine `astrs logs -f`.
//! - [`LogFilter`] — parses `astrs/logs[/level[/node]]` virtual-input
//!   paths (blueprint §8.4) into a level+node predicate.
//! - [`EnvFilterLite`] — a small `RUST_LOG`-style directive parser
//!   (`"info,astrs_daemon=debug"`) for target-scoped filtering.

mod env_filter;
mod error;
mod filter;
pub mod format;
mod level;
mod merge;
mod reader;
mod record;
mod rotate;

#[cfg(test)]
mod test_util;

pub use astrs_time::HlcTimestamp;
pub use env_filter::{DirectiveError, EnvFilterLite};
pub use error::{LogError, Result};
pub use filter::{FilterPathError, LogFilter};
pub use level::{LevelParseError, LogLevel};
pub use merge::{LogMerger, merge_logs};
pub use reader::LogFileReader;
pub use record::{LogRecord, SeqCounter, StdioStream};
pub use rotate::{FsyncPolicy, RotatingWriter, RotationConfig, rotated_path};
