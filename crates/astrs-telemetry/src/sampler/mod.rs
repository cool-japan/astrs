//! `CpuMemSampler` — per-process CPU% and RSS sampling for self and child
//! pids (blueprint §13: "Daemon samples per-node CPU/RSS/queue-depth/
//! SHM-stats every 2 s").
//!
//! | Module | Contents |
//! |---|---|
//! | [`error`] | [`SamplerError`] |
//! | `linux` (Linux only) | `/proc/<pid>/stat` + `/proc/<pid>/status` parsing |
//! | `macos` (macOS only) | `getrusage(2)` for self, a `ps` fallback for other pids |
//!
//! # Fidelity is reported, not hidden
//!
//! Every [`ProcSample`] carries a [`SampleFidelity`] because the two
//! supported platforms genuinely do not offer the same quality of
//! measurement:
//!
//! - **Linux**: [`SampleFidelity::Full`] for every pid, always — `/proc`
//!   gives real, current, per-pid CPU-time deltas and current RSS.
//! - **macOS, this process**: [`SampleFidelity::ProcessRusagePeakRss`] —
//!   `getrusage(2)` gives exact CPU time, but only the process's
//!   lifetime-*peak* RSS, not its current RSS (macOS has no per-pid
//!   syscall for "current" without `proc_pidinfo`, which is not wrapped
//!   by `rustix`).
//! - **macOS, any other pid**: [`SampleFidelity::PsFallback`] — shelling
//!   out to `ps`, whose `%CPU` is its own short-window average rather
//!   than the delta-since-last-sample this crate computes everywhere
//!   else, at the cost of a subprocess spawn per sampling round.
//!
//! A caller that only cares about "roughly how busy is this thing" can
//! ignore [`ProcSample::fidelity`] entirely; one comparing numbers across
//! platforms, or deciding how much to trust a reading, should not.
//!
//! # Examples
//!
//! ```
//! use astrs_telemetry::sampler::CpuMemSampler;
//!
//! let mut sampler = CpuMemSampler::new();
//! let pid = astrs_telemetry::sampler::self_pid();
//! let samples = sampler.sample_many(&[pid]);
//! assert!(samples.contains_key(&pid));
//! ```

pub mod error;

// `linux` is intentionally *not* `#[cfg(target_os = "linux")]`-gated: its
// `parse_stat`/`parse_status_vmrss_bytes` are pure string parsers with no
// OS-specific syscalls (only `LinuxSampler`'s `std::fs::read_to_string`
// calls are Linux-specific in practice, and they simply fail gracefully
// with `SamplerError::ProcessNotFound`/`Read` on a host with no `/proc`).
// Compiling it everywhere means its fixture-file tests actually run in
// CI regardless of the runner's OS, instead of silently never executing
// on a macOS or Windows runner. `macos`, by contrast, *is* gated: its
// hand-written `getrusage` FFI declares a struct layout matching
// Darwin's specific ABI, which would be unsound to link against on any
// other OS.
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

pub use error::SamplerError;

use crate::metrics::MetricRegistry;

/// How trustworthy a [`ProcSample`] is, given what the host OS actually
/// let this crate measure. See the module docs for what each variant
/// means in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleFidelity {
    /// A real, current, per-pid reading (Linux `/proc`).
    Full,
    /// This process's own `getrusage(2)` reading: exact CPU time, but a
    /// lifetime-peak (not current) RSS.
    ProcessRusagePeakRss,
    /// Read via a `ps` subprocess: a coarser sampling cadence and a
    /// kernel/`ps`-internal CPU-percent average rather than a delta this
    /// crate computed itself.
    PsFallback,
}

/// One process's CPU and memory reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcSample {
    /// The sampled pid.
    pub pid: u32,
    /// CPU usage as a percentage of one core: a process fully using two
    /// cores reports `200.0`, not a value clamped to `100.0` (matching
    /// [`astrs_wire::NodeMetricsSample::cpu_percent`]'s convention).
    pub cpu_percent: f32,
    /// Resident memory, in bytes. See [`ProcSample::fidelity`] for
    /// whether this is a current or peak reading.
    pub rss_bytes: u64,
    /// How this sample was obtained.
    pub fidelity: SampleFidelity,
}

/// This process's own pid, via `rustix` (blueprint §3.1: OS syscalls go
/// through `rustix`).
#[must_use]
pub fn self_pid() -> u32 {
    u32::try_from(rustix::process::getpid().as_raw_pid()).unwrap_or(0)
}

/// A portable `kill(pid, 0)`-equivalent liveness probe: whether `pid`
/// names a process this one has permission to signal (which implies it
/// exists), without actually sending a signal.
///
/// Useful before spending an OS-specific sampling call on a pid a caller
/// suspects has already exited (e.g. a daemon that has not yet reaped a
/// recently-stopped child).
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    match rustix::process::Pid::from_raw(raw) {
        Some(rustix_pid) => rustix::process::test_kill_process(rustix_pid).is_ok(),
        None => false,
    }
}

/// Samples CPU% and RSS for self and arbitrary child pids, dispatching
/// to the right OS-specific backend.
///
/// Stateful: successive calls for the same pid compute a real CPU
/// percentage from the delta since the previous call (see each backend's
/// docs for the first-call convention).
#[derive(Debug, Default)]
pub struct CpuMemSampler {
    #[cfg(target_os = "linux")]
    inner: linux::LinuxSampler,
    #[cfg(target_os = "macos")]
    inner: macos::MacSampler,
}

impl CpuMemSampler {
    /// A sampler with no prior readings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Samples every pid in `pids`, one entry per requested pid.
    ///
    /// On a platform with no implemented backend, every pid resolves to
    /// [`SamplerError::UnsupportedPlatform`] rather than failing to
    /// compile or panicking — the blueprint targets Linux and macOS for
    /// 0.1.0 (§1.3), and a caller built for a third platform should see
    /// a clear, catchable error rather than a missing symbol.
    #[must_use]
    pub fn sample_many(&mut self, pids: &[u32]) -> BTreeMap<u32, Result<ProcSample, SamplerError>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.inner.sample_many(pids).into_iter().collect()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            pids.iter()
                .map(|&pid| (pid, Err(SamplerError::UnsupportedPlatform)))
                .collect()
        }
    }

    /// Samples a single pid. Equivalent to calling
    /// [`CpuMemSampler::sample_many`] with a one-element slice.
    pub fn sample(&mut self, pid: u32) -> Result<ProcSample, SamplerError> {
        self.sample_many(&[pid])
            .remove(&pid)
            .unwrap_or(Err(SamplerError::UnsupportedPlatform))
    }
}

/// The bounded cardinality cap used by [`spawn_periodic_sampling`]'s
/// per-pid gauge families — generous enough for a large single-machine
/// dataflow's worth of node processes without letting a
/// pid-recycling-over-a-long-uptime process grow the registry forever.
pub const DEFAULT_SAMPLED_PID_CAPACITY: u32 = 128;

/// Spawns a background task that samples whatever pids `pids()` returns
/// every `interval`, recording each successful reading into two gauge
/// families (`process_cpu_percent`, `process_rss_bytes`, both labelled
/// `pid`) on `registry` — blueprint §13: "Emits into the registry every
/// N seconds (task-based, configurable)."
///
/// `pids` is called fresh on every tick rather than captured once, so
/// the sampled set tracks nodes as they are spawned or exit; a failed
/// sample for one pid (e.g. it exited between the caller listing it and
/// this task sampling it) is silently skipped for that tick rather than
/// aborting the whole round — the next tick will simply stop reporting a
/// pid that is gone.
///
/// Returns the [`JoinHandle`] for the spawned task; aborting or awaiting
/// it is the caller's responsibility (this crate does not assume a
/// particular shutdown protocol for a periodic sampler the way
/// [`crate::export::OtlpExporter`] does for its own background task).
///
/// # Examples
///
/// ```
/// use astrs_telemetry::metrics::MetricRegistry;
/// use astrs_telemetry::sampler::{self, spawn_periodic_sampling};
/// use std::sync::Arc;
/// use std::time::Duration;
///
/// # async fn run() {
/// let registry = Arc::new(MetricRegistry::new());
/// let pid = sampler::self_pid();
/// let handle = spawn_periodic_sampling(Arc::clone(&registry), move || vec![pid], Duration::from_millis(10));
/// tokio::time::sleep(Duration::from_millis(50)).await;
/// handle.abort();
/// # }
/// ```
pub fn spawn_periodic_sampling(
    registry: Arc<MetricRegistry>,
    pids: impl Fn() -> Vec<u32> + Send + 'static,
    interval: Duration,
) -> JoinHandle<()> {
    let cpu_family = registry.register_gauge_family(
        "process_cpu_percent",
        &["pid"],
        DEFAULT_SAMPLED_PID_CAPACITY,
    );
    let rss_family =
        registry.register_gauge_family("process_rss_bytes", &["pid"], DEFAULT_SAMPLED_PID_CAPACITY);
    tokio::spawn(async move {
        let mut sampler = CpuMemSampler::new();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let current_pids = pids();
            for (pid, result) in sampler.sample_many(&current_pids) {
                let Ok(sample) = result else { continue };
                let pid_label = pid.to_string();
                cpu_family
                    .get_or_create(&[&pid_label])
                    .set(f64::from(sample.cpu_percent));
                rss_family
                    .get_or_create(&[&pid_label])
                    .set(sample.rss_bytes as f64);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn self_pid_matches_the_standard_library() {
        assert_eq!(self_pid(), std::process::id());
    }

    #[test]
    fn this_process_is_alive() {
        assert!(is_process_alive(self_pid()));
    }

    #[test]
    fn pid_zero_is_not_reported_alive() {
        // Not a real "process 0" on Unix (0 means "every process in the
        // caller's group" to `kill(2)`), so `Pid::from_raw` rejects it
        // and this returns `false` rather than a false positive.
        assert!(!is_process_alive(0));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sample_covers_this_process_on_a_supported_platform() {
        let mut sampler = CpuMemSampler::new();
        let sample = sampler.sample(self_pid()).unwrap();
        assert_eq!(sample.pid, self_pid());
    }

    #[test]
    fn sample_many_returns_one_entry_per_requested_pid() {
        let mut sampler = CpuMemSampler::new();
        let results = sampler.sample_many(&[self_pid(), u32::MAX]);
        assert_eq!(results.len(), 2);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn periodic_sampling_populates_the_registry() {
        let registry = Arc::new(MetricRegistry::new());
        let pid = self_pid();
        let handle = spawn_periodic_sampling(
            Arc::clone(&registry),
            move || vec![pid],
            Duration::from_millis(10),
        );

        // Several ticks' worth of real time: this environment has no
        // `tokio::time::pause` (the `test-util` feature is not part of
        // this workspace's pinned tokio feature set), so the test waits
        // out real ticks rather than a virtual clock.
        tokio::time::sleep(Duration::from_millis(150)).await;
        handle.abort();

        let batch = registry.snapshot(astrs_time::HlcTimestamp::EPOCH, "test");
        assert!(
            batch.points.iter().any(|p| p.name == "process_cpu_percent"
                && p.label("pid") == Some(pid.to_string().as_str()))
        );
        assert!(
            batch.points.iter().any(|p| p.name == "process_rss_bytes"
                && p.label("pid") == Some(pid.to_string().as_str()))
        );
    }
}
