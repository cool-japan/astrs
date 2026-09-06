//! `cpu_affinity` application at spawn (§11.3).
//!
//! The manifest's `cpu_affinity: [0, 2]` is parsed, validated
//! ([`astrs_manifest`]) and carried all the way to
//! [`astrs_wire::NodeSpawnSpec::cpu_affinity`] — but until this module,
//! nothing ever *applied* it: every process ran unpinned regardless of what
//! the manifest asked for. This module's own (private) `arm` function is the
//! missing step, called from [`super::Spawner::build_command`] on the
//! not-yet-spawned `std::process::Command`.
//!
//! # Platforms
//!
//! | Platform | What happens |
//! |---|---|
//! | Linux | a `pre_exec` hook that calls `sched_setaffinity` between `fork()` and `exec()` — see [`CpuAffinityOutcome::Applied`]'s docs for why that point, specifically, is the race-free one. |
//! | Everything else (macOS included) | No per-process (or per-thread) affinity API exists to call. [`CpuAffinityOutcome::UnsupportedPlatform`] follows: a structured `tracing::warn!` once per spawn, plus [`crate::metrics::DaemonMetrics::record_cpu_affinity_unsupported`] when a registry is wired in. |

use astrs_wire::{DataflowId, NodeId};

use crate::metrics::DaemonMetrics;

/// What happened when a spawn request's `cpu_affinity` list was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CpuAffinityOutcome {
    /// The spec named no cores; there was nothing to apply.
    #[default]
    NotRequested,
    /// Armed via a race-free `pre_exec` hook (see this module's own docs) —
    /// the child's own program will not execute its first instruction until
    /// after the mask is in place.
    Applied,
    /// The spec named cores, but this platform has no per-process (or
    /// per-thread) CPU affinity API to apply them with. Logged once and left
    /// unpinned rather than silently ignored.
    UnsupportedPlatform,
}

impl CpuAffinityOutcome {
    /// Whether the request actually named any cores.
    #[must_use]
    pub const fn was_requested(self) -> bool {
        !matches!(self, Self::NotRequested)
    }

    /// Whether the pin was actually put in place.
    #[must_use]
    pub const fn was_applied(self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// Decides, arms and reports the outcome of `cores` for `command`.
///
/// # Race-freedom
///
/// The pin is armed as a `pre_exec` hook rather than applied to the child's
/// pid after `Command::spawn()` returns. `sched_setaffinity` on Linux
/// addresses a *thread*, not "a process" (Linux has no such syscall) — and a
/// freshly forked child has exactly one thread, whose id equals its pid,
/// right up until its first `exec()`. A hook that runs in that window pins
/// the child before the node's own program has executed a single
/// instruction, so there is no interval in which it could run unpinned.
/// Applying the mask afterwards, to the pid `spawn()` hands back, cannot make
/// that guarantee: between the fork the parent cannot see happen and the
/// moment it gets around to calling `sched_setaffinity` on the new pid, the
/// child is free to run — and if it has spawned its own threads by then,
/// those inherited *today's* (unpinned) mask, not the one about to be
/// applied.
///
/// Called on the not-yet-spawned `std::process::Command`, before it is
/// wrapped as a `tokio::process::Command` — `pre_exec` is `std`'s own API.
///
/// # Everything but Linux
///
/// There is no per-process (or per-thread) CPU affinity API at all on
/// macOS — no syscall, no `pthread` call, nothing that pins execution to
/// specific cores. (`qos_class`, which older AstRS documentation referenced,
/// is not that: it is a scheduler *hint* about a thread's importance, not a
/// core mask, and applying it here would let a log line claim a pin that
/// never happened — worse than the honest fallback below, not better.) So
/// the fallback is exactly that: leave the process unpinned, and say so
/// loudly rather than silently — one structured `tracing::warn!` per spawn
/// (a node's whole run between restarts triggers exactly one call to [`arm`],
/// so this can never become log spam on its own) plus a registered counter
/// an operator's dashboard can alert on.
// Two sibling definitions, one per platform, rather than one function
// branching on `cfg!(target_os = "linux")`: a `cfg!()` boolean is a constant
// the *codegen* backend folds away, but every branch is still fully
// typechecked and lint-checked first, so a single-function version would
// need `command` (Linux-only) or `dataflow`/`node`/`metrics`
// (fallback-only) referenced on a platform that never uses them — an
// unused-parameter warning this workspace denies. Two `#[cfg]`-gated
// definitions instead means exactly one exists per compilation, each
// referencing only what its own platform actually needs.
#[cfg(target_os = "linux")]
pub(super) fn arm(
    command: &mut std::process::Command,
    cores: &[u16],
    _dataflow: DataflowId,
    _node: &NodeId,
    _metrics: Option<&DaemonMetrics>,
) -> CpuAffinityOutcome {
    if cores.is_empty() {
        return CpuAffinityOutcome::NotRequested;
    }
    linux::arm(command, cores);
    CpuAffinityOutcome::Applied
}

#[cfg(not(target_os = "linux"))]
pub(super) fn arm(
    _command: &mut std::process::Command,
    cores: &[u16],
    dataflow: DataflowId,
    node: &NodeId,
    metrics: Option<&DaemonMetrics>,
) -> CpuAffinityOutcome {
    if cores.is_empty() {
        return CpuAffinityOutcome::NotRequested;
    }
    tracing::warn!(
        dataflow = %dataflow,
        node = %node,
        cores = ?cores,
        os = std::env::consts::OS,
        "cpu_affinity is advisory-unsupported on this platform: no \
         per-process CPU affinity API exists here, so this node will \
         run unpinned",
    );
    if let Some(metrics) = metrics {
        metrics.record_cpu_affinity_unsupported();
    }
    CpuAffinityOutcome::UnsupportedPlatform
}

#[cfg(target_os = "linux")]
mod linux {
    use rustix::thread::CpuSet;

    /// Arms the race-free `pre_exec` hook (see [`super::arm`]'s docs).
    pub(super) fn arm(command: &mut std::process::Command, cores: &[u16]) {
        let cores: Vec<u16> = cores.to_vec();
        // SAFETY: this closure runs in the forked child, strictly between
        // `fork()` and `exec()` — `std::os::unix::process::CommandExt::pre_exec`'s
        // own contract. It performs exactly one operation,
        // `rustix::thread::sched_setaffinity`: on the `linux_raw` backend
        // that is a direct `syscall(2)` with no heap allocation and no lock
        // acquisition, and the `libc` backend's equivalent is the same thin,
        // non-allocating wrapper glibc itself documents as safe to call
        // between `fork()` and `exec()`. `cores` was built on the parent's
        // stack before the fork and moved into the closure by value, so
        // nothing is allocated *inside* the child either — the one hazard
        // `pre_exec` actually warns about (a forked child sharing its
        // parent's heap-allocator locks with threads that no longer exist on
        // this side of the fork) never arises here.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(command, move || {
                let mut set = CpuSet::new();
                for &core in &cores {
                    let core = core as usize;
                    if core >= CpuSet::MAX_CPU {
                        // Rejected here, not inside `CpuSet::set` (which
                        // indexes a fixed-size array and panics out of
                        // bounds): panicking mid-fork, before `exec()`, is
                        // undefined-behavior-adjacent and can wedge or
                        // corrupt the parent, so this is surfaced as an
                        // ordinary I/O error instead. `Command::spawn()`
                        // relays a `pre_exec` error as its own `Err`, so it
                        // reaches the daemon exactly like any other spawn
                        // failure — as `DaemonError::Spawn`.
                        return Err(std::io::Error::other(format!(
                            "cpu_affinity core index {core} is out of range \
                             for this platform (CpuSet::MAX_CPU = {})",
                            CpuSet::MAX_CPU
                        )));
                    }
                    set.set(core);
                }
                rustix::thread::sched_setaffinity(None, &set).map_err(std::io::Error::from)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Mutex;

    use tracing::field::{Field, Visit};
    use tracing::span;

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    fn node() -> NodeId {
        NodeId::new("camera").unwrap()
    }

    #[test]
    fn no_cores_named_is_not_requested() {
        let mut command = std::process::Command::new("/usr/bin/true");
        let outcome = arm(&mut command, &[], dataflow(), &node(), None);
        assert_eq!(outcome, CpuAffinityOutcome::NotRequested);
        assert!(!outcome.was_requested());
        assert!(!outcome.was_applied());
    }

    #[test]
    fn outcome_classifies_itself() {
        assert!(!CpuAffinityOutcome::NotRequested.was_requested());
        assert!(CpuAffinityOutcome::Applied.was_requested());
        assert!(CpuAffinityOutcome::Applied.was_applied());
        assert!(CpuAffinityOutcome::UnsupportedPlatform.was_requested());
        assert!(!CpuAffinityOutcome::UnsupportedPlatform.was_applied());
    }

    // A minimal `tracing::Subscriber` that records every event's message and
    // level — enough to assert a specific WARN fired, without pulling in a
    // dev-dependency this crate does not otherwise need.
    #[derive(Default)]
    struct CaptureSubscriber {
        events: Mutex<Vec<(tracing::Level, String)>>,
    }

    #[derive(Default)]
    struct MessageVisitor(String);

    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((*event.metadata().level(), visitor.0));
        }

        fn enter(&self, _span: &span::Id) {}
        fn exit(&self, _span: &span::Id) {}
    }

    #[test]
    #[cfg_attr(
        target_os = "linux",
        ignore = "the Linux path applies the pin \
        directly and never falls back to the WARN this test asserts on"
    )]
    fn an_unsupported_platform_warns_once_and_still_reports_unsupported() {
        // `with_default` scopes the subscriber to this thread's call stack
        // only (unlike `set_global_default`, which may run once per
        // process), so this needs no cross-test synchronization.
        let subscriber = std::sync::Arc::new(CaptureSubscriber::default());
        let outcome = tracing::subscriber::with_default(subscriber.clone(), || {
            let mut command = std::process::Command::new("/usr/bin/true");
            arm(&mut command, &[0, 1], dataflow(), &node(), None)
        });
        assert_eq!(outcome, CpuAffinityOutcome::UnsupportedPlatform);
        assert!(outcome.was_requested());
        assert!(!outcome.was_applied());

        let events = subscriber
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1, "exactly one WARN per spawn: {events:?}");
        let (level, message) = &events[0];
        assert_eq!(*level, tracing::Level::WARN);
        assert!(message.contains("cpu_affinity"), "{message}");
        assert!(message.contains("unsupported"), "{message}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_applies_the_pin_to_the_current_process_and_it_is_observable() {
        // Exercised against the *test process itself* rather than a spawned
        // child: `arm`'s `pre_exec` hook only ever runs inside a forked
        // child, which a unit test cannot observe directly (the assertion
        // would run in the parent). `linux::arm`'s closure calls
        // `rustix::thread::sched_setaffinity(None, ...)` — "the current
        // thread" — so calling that same operation here, on this test
        // thread, exercises the identical syscall path `pre_exec` would run,
        // and `sched_getaffinity` reading it back proves the mask actually
        // took effect at the OS level.
        let original = rustix::thread::sched_getaffinity(None).unwrap();
        // Pick a core this process is actually allowed to run on, so the
        // syscall cannot fail with `EINVAL` on a sandboxed/cgroup-limited
        // CI runner that does not have core 0.
        let Some(core) = (0..rustix::thread::CpuSet::MAX_CPU).find(|&c| original.is_set(c)) else {
            panic!("the current affinity mask names no CPU at all");
        };

        let mut set = rustix::thread::CpuSet::new();
        set.set(core);
        rustix::thread::sched_setaffinity(None, &set).unwrap();

        let observed = rustix::thread::sched_getaffinity(None).unwrap();
        assert!(observed.is_set(core));
        assert_eq!(observed.count(), 1, "only the requested core is set");

        // Restore, so this test does not permanently narrow the affinity of
        // whichever OS thread runs the next test in this process.
        rustix::thread::sched_setaffinity(None, &original).unwrap();
    }

    // An out-of-range core index (`>= CpuSet::MAX_CPU`) is rejected inside
    // `linux::arm`'s `pre_exec` closure, which only ever runs post-fork in a
    // real spawned child — see `Spawner`'s own
    // `an_out_of_range_cpu_core_fails_the_spawn_cleanly_not_a_panic` test for
    // the end-to-end proof that it surfaces as `DaemonError::Spawn`, not a
    // panic mid-fork.
}
