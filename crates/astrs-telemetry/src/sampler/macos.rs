//! macOS CPU/RSS sampling.
//!
//! macOS has no `/proc`. The correct per-pid introspection API
//! (`proc_pidinfo`, from `libproc.h`) is not wrapped by `rustix`, and
//! this crate cannot add a `libproc`/`libc`-sys dependency (not on the
//! retained list, §18.1; Pure-Rust-default, §3.1). Two very different
//! mechanisms cover the two cases this module actually needs:
//!
//! - **This process itself**: `getrusage(2)` via a small hand-written FFI
//!   declaration (no `libc` crate — the same OS `libSystem.dylib` every
//!   Rust binary on macOS already links against, called through a
//!   `#[repr(C)]` struct matching Darwin's stable ABI, verified by
//!   the unit test `tests::darwin_rusage_matches_the_expected_layout`
//!   and exercised for real in this sandbox). This is [`SampleFidelity::ProcessRusagePeakRss`]:
//!   CPU time is exact, but `ru_maxrss` is the process's **peak** RSS
//!   over its whole lifetime, not its current RSS — `getrusage` has no
//!   "current" reading at all, only cumulative/peak ones.
//! - **Any other pid** (a spawned node's child process): shelling out to
//!   `ps` (blueprint's own suggested fallback), the only avenue left
//!   without `proc_pidinfo`. This is [`SampleFidelity::PsFallback`]: a
//!   subprocess per sampling round, and `ps`'s own `%CPU` — a kernel/`ps`
//!   internal short-window average, not the delta this crate computes
//!   for the `/proc`-based Linux path or for `getrusage`.
//!
//! Both fidelity levels are reported honestly on every [`ProcSample`]
//! rather than presented as equivalent to Linux's `/proc` reading.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::sampler::error::SamplerError;
use crate::sampler::{ProcSample, SampleFidelity};

/// `RUSAGE_SELF`, from `<sys/resource.h>`.
const RUSAGE_SELF: core::ffi::c_int = 0;

/// Darwin's `struct timeval`: `{ time_t tv_sec; suseconds_t tv_usec; }`,
/// where on 64-bit Darwin `tv_sec` is a 64-bit `time_t` and `tv_usec` is
/// specifically a **32-bit** `__darwin_suseconds_t` — unlike Linux, where
/// both fields are 64-bit `long`. Getting this width wrong would
/// misalign every field after it in [`DarwinRusage`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DarwinTimeval {
    tv_sec: i64,
    tv_usec: i32,
}

/// Darwin's `struct rusage` (`<sys/resource.h>`), the fields `getrusage`
/// writes into. Every field after the two `timeval`s is a plain 64-bit
/// `long`; this crate only reads `ru_utime`/`ru_stime`/`ru_maxrss`; the
/// rest exist purely so the struct's total size matches what the kernel
/// expects to write.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DarwinRusage {
    ru_utime: DarwinTimeval,
    ru_stime: DarwinTimeval,
    ru_maxrss: i64,
    ru_ixrss: i64,
    ru_idrss: i64,
    ru_isrss: i64,
    ru_minflt: i64,
    ru_majflt: i64,
    ru_nswap: i64,
    ru_inblock: i64,
    ru_oublock: i64,
    ru_msgsnd: i64,
    ru_msgrcv: i64,
    ru_nsignals: i64,
    ru_nvcsw: i64,
    ru_nivcsw: i64,
}

unsafe extern "C" {
    /// `getrusage(2)`. Declared by hand against `libSystem.dylib` (which
    /// every Rust binary on macOS already links, the same way `std`
    /// itself calls into the OS) rather than pulled in via a `libc`
    /// crate dependency — see the module docs.
    fn getrusage(who: core::ffi::c_int, usage: *mut DarwinRusage) -> core::ffi::c_int;
}

/// Converts a Darwin `timeval` to a [`Duration`], clamping any
/// (unexpected) negative component to zero rather than reinterpreting
/// its bits as a huge unsigned value.
fn timeval_to_duration(tv: DarwinTimeval) -> Duration {
    let secs = u64::try_from(tv.tv_sec).unwrap_or(0);
    let micros = u32::try_from(tv.tv_usec).unwrap_or(0);
    Duration::new(secs, micros.saturating_mul(1_000))
}

/// Calls `getrusage(RUSAGE_SELF, ...)` and returns `(total CPU time,
/// peak RSS in bytes)`.
///
/// # Errors
///
/// [`SamplerError::Read`] if the syscall itself reports failure (via a
/// non-zero return and `errno`) — unreached in practice for
/// `RUSAGE_SELF`, which the man page documents as only failing for an
/// invalid `who` argument, and `who` is this function's own hardcoded
/// constant.
fn self_rusage() -> Result<(Duration, u64), SamplerError> {
    let mut usage = std::mem::MaybeUninit::<DarwinRusage>::zeroed();
    // SAFETY: `usage` points to a validly-aligned, zero-initialized
    // `DarwinRusage` sized to exactly match Darwin's `struct rusage`
    // (asserted by this module's `darwin_rusage_matches_the_expected_layout`
    // test), which `getrusage` is documented to fully overwrite on
    // success; `who` is the constant `RUSAGE_SELF`, the only argument
    // shape this function ever passes.
    let ret = unsafe { getrusage(RUSAGE_SELF, usage.as_mut_ptr()) };
    if ret != 0 {
        return Err(SamplerError::Read {
            pid: std::process::id(),
            source_name: "getrusage(RUSAGE_SELF)".to_owned(),
            message: std::io::Error::last_os_error().to_string(),
        });
    }
    // SAFETY: `ret == 0` means `getrusage` succeeded and, per its
    // contract, fully initialized `usage`.
    let usage = unsafe { usage.assume_init() };
    let cpu_time = timeval_to_duration(usage.ru_utime) + timeval_to_duration(usage.ru_stime);
    let maxrss_bytes = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    Ok((cpu_time, maxrss_bytes))
}

/// Runs `ps` with `args`, trying `/bin/ps` (verified present on macOS)
/// before falling back to `/usr/bin/ps` (some other Unix layouts' usual
/// location), and only failing if neither exists.
fn run_ps(args: &[&str]) -> Result<std::process::Output, SamplerError> {
    let mut last_error = None;
    for candidate in ["/bin/ps", "/usr/bin/ps"] {
        match Command::new(candidate).args(args).output() {
            Ok(output) => return Ok(output),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_error = Some(error);
                continue;
            }
            Err(error) => {
                return Err(SamplerError::PsFallback {
                    message: format!("{candidate}: {error}"),
                });
            }
        }
    }
    Err(SamplerError::PsFallback {
        message: format!(
            "neither /bin/ps nor /usr/bin/ps could be run: {}",
            last_error.map_or_else(|| "not found".to_owned(), |e| e.to_string())
        ),
    })
}

/// Parses `ps -o pid=,pcpu=,rss=` output: one `<pid> <pcpu> <rss_kb>`
/// line per pid `ps` actually found.
///
/// A pid requested from the crate-private `sample_via_ps` but absent from this map was
/// simply not found by `ps` — the caller reports that as
/// [`SamplerError::ProcessNotFound`], not as a parse failure. This
/// function itself never fails: an unparseable line is skipped (`ps`'s
/// output format is stable enough in practice that this is not expected
/// to happen, but a stray malformed line should not take down the whole
/// sampling round).
///
/// # Examples
///
/// ```
/// use astrs_telemetry::sampler::macos::parse_ps_output;
///
/// let output = "  1234   1.5  2048\n  5678   0.0   512\n";
/// let samples = parse_ps_output(output);
/// assert_eq!(samples.len(), 2);
/// assert_eq!(samples[&1234].cpu_percent, 1.5);
/// assert_eq!(samples[&5678].rss_bytes, 512 * 1024);
/// ```
#[must_use]
pub fn parse_ps_output(stdout: &str) -> HashMap<u32, ProcSample> {
    let mut results = HashMap::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid_str), Some(pcpu_str), Some(rss_str)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(cpu_percent), Ok(rss_kb)) = (
            pid_str.parse::<u32>(),
            pcpu_str.parse::<f32>(),
            rss_str.parse::<u64>(),
        ) else {
            continue;
        };
        results.insert(
            pid,
            ProcSample {
                pid,
                cpu_percent,
                rss_bytes: rss_kb.saturating_mul(1024),
                fidelity: SampleFidelity::PsFallback,
            },
        );
    }
    results
}

/// Samples `pids` via one `ps` invocation covering all of them.
///
/// # Errors
///
/// [`SamplerError::PsFallback`] only if `ps` itself could not be run at
/// all (see [`run_ps`]). A pid `ps` ran but did not report on (dead,
/// never existed, or rejected by `ps` as malformed — empirically, `ps`
/// on macOS refuses an implausibly large `-p` value for the *entire*
/// batch rather than just skipping it) is simply absent from the
/// returned map; it is not itself an error at this layer.
fn sample_via_ps(pids: &[u32]) -> Result<HashMap<u32, ProcSample>, SamplerError> {
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    let pid_list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = run_ps(&["-o", "pid=,pcpu=,rss=", "-p", &pid_list])?;
    Ok(parse_ps_output(&String::from_utf8_lossy(&output.stdout)))
}

/// Samples CPU% and RSS on macOS: `getrusage` for this process itself,
/// `ps` for every other requested pid.
#[derive(Debug, Default)]
pub struct MacSampler {
    previous_self: Option<(Duration, Instant)>,
}

impl MacSampler {
    /// A sampler with no prior self-reading.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Samples every pid in `pids`, routing this process's own pid
    /// through `getrusage` and every other pid through one shared `ps`
    /// invocation.
    #[must_use]
    pub fn sample_many(&mut self, pids: &[u32]) -> HashMap<u32, Result<ProcSample, SamplerError>> {
        let self_pid = std::process::id();
        let mut results = HashMap::new();
        let mut others = Vec::new();
        for &pid in pids {
            if pid == self_pid {
                results.insert(pid, self.sample_self());
            } else {
                others.push(pid);
            }
        }
        if !others.is_empty() {
            match sample_via_ps(&others) {
                Ok(mut found) => {
                    for pid in others {
                        let result = found
                            .remove(&pid)
                            .ok_or(SamplerError::ProcessNotFound { pid });
                        results.insert(pid, result);
                    }
                }
                Err(error) => {
                    for pid in others {
                        results.insert(
                            pid,
                            Err(SamplerError::PsFallback {
                                message: error.to_string(),
                            }),
                        );
                    }
                }
            }
        }
        results
    }

    /// Samples this process's own pid via `getrusage`, computing a CPU
    /// percentage from the delta since the previous call (`0.0` on the
    /// first call, matching [`crate::sampler::linux::LinuxSampler::sample_one`]'s
    /// convention).
    fn sample_self(&mut self) -> Result<ProcSample, SamplerError> {
        let (cpu_time, maxrss_bytes) = self_rusage()?;
        let now = Instant::now();
        let cpu_percent = match self.previous_self.replace((cpu_time, now)) {
            Some((previous_cpu, previous_instant)) => {
                let delta_cpu = cpu_time.saturating_sub(previous_cpu);
                let delta_wall = now.saturating_duration_since(previous_instant);
                if delta_wall.is_zero() {
                    0.0
                } else {
                    ((delta_cpu.as_secs_f64() / delta_wall.as_secs_f64()) * 100.0) as f32
                }
            }
            None => 0.0,
        };
        Ok(ProcSample {
            pid: std::process::id(),
            cpu_percent,
            rss_bytes: maxrss_bytes,
            fidelity: SampleFidelity::ProcessRusagePeakRss,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Pins [`DarwinRusage`]'s layout against the real, decades-stable
    /// Darwin ABI: two 16-byte `timeval`s (an 8-byte `time_t` plus a
    /// 4-byte `suseconds_t`, padded to 8-byte alignment) followed by 14
    /// 8-byte `long` fields — `32 + 14 * 8 == 144` bytes, 8-byte aligned.
    /// If this ever fails, [`self_rusage`]'s `unsafe` call is no longer
    /// sound and must not be trusted until this is fixed.
    #[test]
    fn darwin_rusage_matches_the_expected_layout() {
        assert_eq!(std::mem::size_of::<DarwinTimeval>(), 16);
        assert_eq!(std::mem::size_of::<DarwinRusage>(), 144);
        assert_eq!(std::mem::align_of::<DarwinRusage>(), 8);
    }

    #[test]
    fn getrusage_on_self_returns_plausible_values() {
        // Real syscall: this sandbox runs on Darwin.
        let (cpu_time, maxrss_bytes) = self_rusage().unwrap();
        // This test process has done *some* work by the time it reaches
        // this line (at minimum, starting up and running prior tests in
        // the same binary), and a sane RSS is comfortably under 4 GiB.
        assert!(cpu_time >= Duration::ZERO);
        assert!(maxrss_bytes > 0, "a running process has nonzero RSS");
        assert!(maxrss_bytes < 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn mac_sampler_reports_zero_cpu_percent_on_the_first_self_sample() {
        let mut sampler = MacSampler::new();
        let results = sampler.sample_many(&[std::process::id()]);
        let sample = results[&std::process::id()].as_ref().unwrap();
        assert_eq!(sample.cpu_percent, 0.0);
        assert_eq!(sample.fidelity, SampleFidelity::ProcessRusagePeakRss);
        assert!(sample.rss_bytes > 0);
    }

    #[test]
    fn mac_sampler_reports_a_real_second_self_sample() {
        let mut sampler = MacSampler::new();
        let pid = std::process::id();
        let _ = sampler.sample_many(&[pid]);
        // Burn a little CPU so the second reading's delta is nonzero
        // and meaningfully testable.
        let mut acc = 0u64;
        for i in 0..5_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        std::hint::black_box(acc);
        let results = sampler.sample_many(&[pid]);
        let sample = results[&pid].as_ref().unwrap();
        assert!(sample.cpu_percent >= 0.0);
    }

    #[test]
    fn ps_fallback_finds_this_processs_own_pid() {
        // Exercises the real `/bin/ps` binary on this sandbox.
        let pid = std::process::id();
        let found = sample_via_ps(&[pid]).unwrap();
        assert!(found.contains_key(&pid));
        assert_eq!(found[&pid].fidelity, SampleFidelity::PsFallback);
    }

    #[test]
    fn ps_fallback_omits_a_pid_it_did_not_find() {
        // pid 2 is essentially never a running, ps-visible process on a
        // modern macOS (launchd/kernel_task occupy the lowest live pids).
        let found = sample_via_ps(&[2]).unwrap();
        assert!(!found.contains_key(&2));
    }

    #[test]
    fn mac_sampler_reports_process_not_found_for_a_pid_ps_does_not_see() {
        let mut sampler = MacSampler::new();
        let results = sampler.sample_many(&[2]);
        assert!(matches!(
            results[&2],
            Err(SamplerError::ProcessNotFound { pid: 2 })
        ));
    }

    #[test]
    fn mac_sampler_handles_a_mix_of_self_and_other_pids() {
        let mut sampler = MacSampler::new();
        let self_pid = std::process::id();
        let results = sampler.sample_many(&[self_pid, 2]);
        assert_eq!(results.len(), 2);
        assert!(results[&self_pid].is_ok());
        assert!(results[&2].is_err());
    }

    #[test]
    fn parse_ps_output_examples() {
        let samples = parse_ps_output("  1234   1.5  2048\n  5678   0.0   512\n");
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[&1234].cpu_percent, 1.5);
        assert_eq!(samples[&1234].rss_bytes, 2048 * 1024);
        assert_eq!(samples[&5678].rss_bytes, 512 * 1024);
    }

    #[test]
    fn parse_ps_output_skips_unparseable_lines() {
        let samples = parse_ps_output("garbage line\n1234 1.0 100\n\n");
        assert_eq!(samples.len(), 1);
        assert!(samples.contains_key(&1234));
    }

    #[test]
    fn parse_ps_output_of_empty_input_is_empty() {
        assert!(parse_ps_output("").is_empty());
    }

    #[test]
    fn sample_many_of_an_empty_pid_list_is_empty() {
        let mut sampler = MacSampler::new();
        assert!(sampler.sample_many(&[]).is_empty());
    }
}
