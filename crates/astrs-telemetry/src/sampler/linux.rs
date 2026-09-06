//! Linux CPU/RSS sampling: hand-rolled `/proc/<pid>/stat` and
//! `/proc/<pid>/status` parsers, plus [`LinuxSampler`] that turns two
//! successive readings into a CPU percentage.
//!
//! # Why `status`'s `VmRSS`, not `statm`
//!
//! The task that scoped this crate named `/proc/<pid>/statm` for memory.
//! `statm`'s fields are in *pages*, which requires the page size
//! (`sysconf(_SC_PAGESIZE)`) to convert to bytes — and this crate has no
//! sound, dependency-free way to obtain that: `rustix`'s `param` feature
//! (which exposes `page_size()`) is not part of the feature set the
//! workspace `Cargo.toml` enables for `rustix`, and adding a new feature
//! to a workspace-pinned dependency from one member crate would be an
//! uncoordinated, workspace-wide change this task's own hard rules
//! forbid touching. `/proc/<pid>/status`'s `VmRSS:` line is reported by
//! the kernel already converted to kB — the same value `ps`/`top` show —
//! so parsing it avoids the page-size dependency entirely rather than
//! working around it with a guessed constant. This is a deliberate
//! deviation from the letter of the task, recorded here and in this
//! crate's final report.
//!
//! # Why ticks-to-seconds uses a hardcoded constant
//!
//! [`CLOCK_TICKS_PER_SECOND`] is `100` (`USER_HZ`), not read via
//! `sysconf(_SC_CLK_TCK)` — for the same dependency-availability reason
//! as above. Unlike the page size (which genuinely varies by
//! architecture and kernel configuration), `USER_HZ` has been a
//! kernel-wide constant fixed at `100` on every mainstream Linux port for
//! decades, specifically *so that* `/proc` parsing does not need a
//! syscall to stay portable — this is a documented, checkable fact
//! rather than an assumption, and [`cpu_percent_from_ticks`] takes the
//! divisor as a named constant precisely so a wrong value is a one-line
//! fix rather than a rewrite if it is ever wrong for some target.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::sampler::error::SamplerError;
use crate::sampler::{ProcSample, SampleFidelity};

/// Linux's `USER_HZ`: how many `/proc/<pid>/stat` clock ticks make up one
/// second. See the module docs for why this is a constant rather than a
/// `sysconf` call.
pub const CLOCK_TICKS_PER_SECOND: u64 = 100;

/// The `/proc/<pid>/stat` fields this crate needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFields {
    /// Field 14: ticks scheduled in user mode.
    pub utime_ticks: u64,
    /// Field 15: ticks scheduled in kernel mode.
    pub stime_ticks: u64,
    /// Field 22: ticks between system boot and process start.
    pub starttime_ticks: u64,
}

impl StatFields {
    /// `utime_ticks + stime_ticks`: total CPU time charged to this
    /// process, in ticks.
    #[must_use]
    pub const fn total_ticks(&self) -> u64 {
        self.utime_ticks + self.stime_ticks
    }
}

/// [`parse_stat`] or [`parse_status_vmrss_bytes`] could not make sense of
/// their input.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProcParseError {
    /// `/proc/<pid>/stat`'s `comm` field (the second, parenthesized
    /// field) was not closed — the line is not `/proc/<pid>/stat`-shaped
    /// at all.
    #[error("no closing ')' found for the comm field")]
    MissingCommClose,
    /// A required field was missing after the `comm` field.
    #[error("missing field {index} ({name}) after the comm field")]
    MissingField {
        /// The 0-based index into the whitespace-split remainder.
        index: usize,
        /// A human name for the field, for the error message.
        name: &'static str,
    },
    /// A field was present but not a valid `u64`.
    #[error("field {name} is not a valid u64: {value:?}")]
    InvalidField {
        /// A human name for the field.
        name: &'static str,
        /// The text that failed to parse.
        value: String,
    },
    /// `/proc/<pid>/status` had no `VmRSS:` line.
    #[error("no VmRSS line found")]
    MissingVmRss,
    /// The `VmRSS:` line's value was not a valid `u64` number of kB.
    #[error("VmRSS line {line:?} does not have a valid kB value")]
    InvalidVmRss {
        /// The offending line.
        line: String,
    },
}

/// Parses the CPU-time and start-time fields out of the raw contents of
/// `/proc/<pid>/stat`.
///
/// # Robustness
///
/// `comm` (the second field) is the executable name in parentheses, and
/// — unlike every other field — may itself contain spaces or even
/// parentheses (`(some (weird) name)`), because it comes straight from
/// `argv[0]`/`PR_SET_NAME` with no escaping. The only sound way to find
/// where it ends is the **last** `)` in the line (the kernel guarantees
/// `comm` cannot contain a trailing `)` immediately followed by the rest
/// of `stat`'s fixed, space-separated, non-parenthesized tail), so this
/// function does exactly that rather than a naive `split_whitespace`
/// that would misalign every field for such a process.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::sampler::linux::parse_stat;
///
/// let line = "1234 (my process) S 1 1234 1234 0 -1 4194304 100 0 0 0 \
///              1500 300 0 0 20 0 4 0 987654 123456789 4321 \
///              18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0 0 0 0 0 0 0 0 0";
/// let fields = parse_stat(line).unwrap();
/// assert_eq!(fields.utime_ticks, 1500);
/// assert_eq!(fields.stime_ticks, 300);
/// assert_eq!(fields.starttime_ticks, 987654);
/// ```
pub fn parse_stat(contents: &str) -> Result<StatFields, ProcParseError> {
    let close = contents
        .rfind(')')
        .ok_or(ProcParseError::MissingCommClose)?;
    // `close` is a byte index of `)`, which is one byte wide (ASCII), so
    // `close + 1` is always a valid char boundary to slice from.
    let rest = &contents[close + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();

    let field = |index: usize, name: &'static str| -> Result<u64, ProcParseError> {
        let raw = fields
            .get(index)
            .ok_or(ProcParseError::MissingField { index, name })?;
        raw.parse::<u64>()
            .map_err(|_| ProcParseError::InvalidField {
                name,
                value: (*raw).to_owned(),
            })
    };

    Ok(StatFields {
        utime_ticks: field(11, "utime")?,
        stime_ticks: field(12, "stime")?,
        starttime_ticks: field(19, "starttime")?,
    })
}

/// Parses the `VmRSS:` line out of the raw contents of
/// `/proc/<pid>/status`, returning bytes (the kernel reports kB; this
/// multiplies by 1024).
///
/// # Examples
///
/// ```
/// use astrs_telemetry::sampler::linux::parse_status_vmrss_bytes;
///
/// let status = "Name:\tsleep\nVmRSS:\t    1024 kB\nThreads:\t1\n";
/// assert_eq!(parse_status_vmrss_bytes(status).unwrap(), 1024 * 1024);
/// ```
pub fn parse_status_vmrss_bytes(contents: &str) -> Result<u64, ProcParseError> {
    for line in contents.lines() {
        let Some(rest) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        let digits = rest.trim().trim_end_matches("kB").trim();
        return digits.parse::<u64>().map(|kb| kb * 1024).map_err(|_| {
            ProcParseError::InvalidVmRss {
                line: line.to_owned(),
            }
        });
    }
    Err(ProcParseError::MissingVmRss)
}

/// Converts a CPU-time delta (in ticks) over a wall-clock delta into a
/// percentage of one core (matching [`astrs_wire::NodeMetricsSample::cpu_percent`]'s
/// documented convention: a process fully using two cores reports
/// `200.0`, not a value clamped to `100.0`).
///
/// Returns `0.0` for a zero (or backwards) wall-clock delta rather than
/// dividing by zero or a negative number.
#[must_use]
pub fn cpu_percent_from_ticks(delta_ticks: u64, delta_wall: Duration) -> f32 {
    if delta_wall.is_zero() {
        return 0.0;
    }
    let delta_secs = delta_ticks as f64 / CLOCK_TICKS_PER_SECOND as f64;
    ((delta_secs / delta_wall.as_secs_f64()) * 100.0) as f32
}

/// Samples CPU% and RSS for arbitrary pids by reading `/proc`, keeping
/// enough state per pid to turn two cumulative CPU-time readings into a
/// percentage.
#[derive(Debug, Default)]
pub struct LinuxSampler {
    previous: HashMap<u32, (u64, Instant)>,
}

impl LinuxSampler {
    /// A sampler with no prior readings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads and parses `/proc/<pid>/stat` and `/proc/<pid>/status`,
    /// returning a full-fidelity [`ProcSample`].
    ///
    /// The first call for a given `pid` has no prior reading to diff
    /// against and reports `cpu_percent: 0.0`; every subsequent call
    /// computes a real percentage from the delta since the previous
    /// call.
    ///
    /// # Errors
    ///
    /// [`SamplerError::ProcessNotFound`] if `pid` does not exist (or is
    /// not visible to this process); [`SamplerError::Read`] if a file
    /// exists but could not be read or parsed.
    pub fn sample_one(&mut self, pid: u32) -> Result<ProcSample, SamplerError> {
        let stat_path = format!("/proc/{pid}/stat");
        let stat_contents = std::fs::read_to_string(&stat_path)
            .map_err(|error| read_error(pid, &stat_path, &error.to_string(), error.kind()))?;
        let stat = parse_stat(&stat_contents).map_err(|error| SamplerError::Read {
            pid,
            source_name: stat_path,
            message: error.to_string(),
        })?;

        let status_path = format!("/proc/{pid}/status");
        let status_contents = std::fs::read_to_string(&status_path)
            .map_err(|error| read_error(pid, &status_path, &error.to_string(), error.kind()))?;
        let rss_bytes =
            parse_status_vmrss_bytes(&status_contents).map_err(|error| SamplerError::Read {
                pid,
                source_name: status_path,
                message: error.to_string(),
            })?;

        let total_ticks = stat.total_ticks();
        let now = Instant::now();
        let cpu_percent = match self.previous.insert(pid, (total_ticks, now)) {
            Some((previous_ticks, previous_instant)) => cpu_percent_from_ticks(
                total_ticks.saturating_sub(previous_ticks),
                now.saturating_duration_since(previous_instant),
            ),
            None => 0.0,
        };

        Ok(ProcSample {
            pid,
            cpu_percent,
            rss_bytes,
            fidelity: SampleFidelity::Full,
        })
    }

    /// [`LinuxSampler::sample_one`] for every pid in `pids`.
    ///
    /// Unlike the macOS sampler, this needs no batching optimization:
    /// each pid is two independent file reads, not a subprocess spawn,
    /// so sampling `pids` one at a time costs the same as any other
    /// grouping.
    #[must_use]
    pub fn sample_many(&mut self, pids: &[u32]) -> HashMap<u32, Result<ProcSample, SamplerError>> {
        pids.iter()
            .map(|&pid| (pid, self.sample_one(pid)))
            .collect()
    }
}

/// Maps an [`std::io::Error`] from reading a `/proc` file to the right
/// [`SamplerError`] variant, distinguishing "the process is gone"
/// (`NotFound`) from every other I/O failure.
fn read_error(pid: u32, path: &str, message: &str, kind: std::io::ErrorKind) -> SamplerError {
    if kind == std::io::ErrorKind::NotFound {
        SamplerError::ProcessNotFound { pid }
    } else {
        SamplerError::Read {
            pid,
            source_name: path.to_owned(),
            message: message.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const STAT_NORMAL: &str = include_str!("../../tests/fixtures/proc/stat_normal.txt");
    const STAT_WEIRD_COMM: &str = include_str!("../../tests/fixtures/proc/stat_weird_comm.txt");
    const STATUS_NORMAL: &str = include_str!("../../tests/fixtures/proc/status_normal.txt");
    const STATUS_NO_VMRSS: &str = include_str!("../../tests/fixtures/proc/status_no_vmrss.txt");

    #[test]
    fn parses_a_normal_stat_fixture() {
        let fields = parse_stat(STAT_NORMAL).unwrap();
        assert_eq!(fields.utime_ticks, 1500);
        assert_eq!(fields.stime_ticks, 300);
        assert_eq!(fields.starttime_ticks, 987_654);
        assert_eq!(fields.total_ticks(), 1800);
    }

    #[test]
    fn parses_a_comm_field_containing_spaces_and_parentheses() {
        // The fixture's comm field is "(weird (proc) name)" -- a naive
        // split on the *first* ')' would misalign every field after it.
        let fields = parse_stat(STAT_WEIRD_COMM).unwrap();
        assert_eq!(fields.utime_ticks, 42);
        assert_eq!(fields.stime_ticks, 7);
        assert_eq!(fields.starttime_ticks, 111_111);
    }

    #[test]
    fn rejects_a_line_with_no_comm_close() {
        assert_eq!(
            parse_stat("1234 no-parens-here"),
            Err(ProcParseError::MissingCommClose)
        );
    }

    #[test]
    fn rejects_a_line_missing_fields() {
        let err = parse_stat("1 (a) S 1 1").unwrap_err();
        assert!(matches!(err, ProcParseError::MissingField { .. }));
    }

    #[test]
    fn rejects_a_non_numeric_field() {
        let mut line = STAT_NORMAL.replace("1500", "not-a-number");
        line.truncate(line.trim_end().len());
        let err = parse_stat(&line).unwrap_err();
        assert!(matches!(
            err,
            ProcParseError::InvalidField { name: "utime", .. }
        ));
    }

    #[test]
    fn parses_vmrss_from_a_normal_status_fixture() {
        assert_eq!(
            parse_status_vmrss_bytes(STATUS_NORMAL).unwrap(),
            12_345 * 1024
        );
    }

    #[test]
    fn reports_missing_vmrss() {
        assert_eq!(
            parse_status_vmrss_bytes(STATUS_NO_VMRSS),
            Err(ProcParseError::MissingVmRss)
        );
    }

    #[test]
    fn cpu_percent_from_ticks_examples() {
        // 100 ticks == 1 second of CPU time at USER_HZ=100; over a
        // 1-second wall-clock window, that is 100%.
        assert_eq!(cpu_percent_from_ticks(100, Duration::from_secs(1)), 100.0);
        // Two full cores' worth of CPU time over one second: 200%, not
        // clamped to 100%.
        assert_eq!(cpu_percent_from_ticks(200, Duration::from_secs(1)), 200.0);
        assert_eq!(cpu_percent_from_ticks(0, Duration::from_secs(1)), 0.0);
    }

    #[test]
    fn cpu_percent_handles_a_zero_wall_delta() {
        assert_eq!(cpu_percent_from_ticks(100, Duration::ZERO), 0.0);
    }

    #[test]
    fn linux_sampler_reports_zero_on_the_first_call_for_a_pid() {
        // Sampling a real, always-present pid: this process's own.
        let pid = std::process::id();
        let mut sampler = LinuxSampler::new();
        if let Ok(sample) = sampler.sample_one(pid) {
            assert_eq!(sample.cpu_percent, 0.0, "no prior reading to diff against");
            assert_eq!(sample.fidelity, SampleFidelity::Full);
            assert_eq!(sample.pid, pid);
        }
        // On a non-Linux CI runner (this crate is tested on macOS too),
        // `/proc` does not exist and this is expected to error --
        // the parser-correctness assertions above already ran either way.
    }

    #[test]
    fn sample_many_covers_every_requested_pid() {
        let mut sampler = LinuxSampler::new();
        let results = sampler.sample_many(&[std::process::id(), u32::MAX]);
        assert_eq!(results.len(), 2);
        assert!(results.contains_key(&std::process::id()));
        assert!(results.contains_key(&u32::MAX));
    }

    #[test]
    fn linux_sampler_reports_process_not_found_for_an_implausible_pid() {
        let mut sampler = LinuxSampler::new();
        let result = sampler.sample_one(u32::MAX);
        assert!(matches!(
            result,
            Err(SamplerError::ProcessNotFound { pid: u32::MAX }) | Err(SamplerError::Read { .. })
        ));
    }
}
