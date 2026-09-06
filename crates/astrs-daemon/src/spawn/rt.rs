//! `rt:` application at spawn — hard-RT executor reservations (§11.3, §22).
//!
//! The manifest's `rt: { policy: fifo, priority: 80 }` block
//! ([`astrs_manifest::RtConfig`], frozen by the ROADMAP PULL-FORWARD setup
//! wave) is parsed and validated by [`astrs_manifest::Manifest::validate`] —
//! but until this module, nothing ever *applied* it: every process ran under
//! the ordinary `SCHED_OTHER` class regardless of what the manifest asked
//! for. This module's own (private) `arm` function is the missing step,
//! called from [`super::Spawner::build_command`] on the not-yet-spawned
//! `std::process::Command`, mirroring exactly how [`super::affinity`]
//! applies `cpu_affinity` at the same point.
//!
//! # Why this never touches `astrs_wire::NodeSpawnSpec`
//!
//! `cpu_affinity` (the sibling this module mirrors) rides on
//! [`astrs_wire::NodeSpawnSpec::cpu_affinity`], a wire field. `rt` cannot
//! follow that precedent: `NodeSpawnSpec` is embedded inside
//! `ControlRequest::AddNode`, one of the *already-frozen* lines
//! `crates/astrs-wire/tests/golden/protocol.frozen.snap` pins byte-for-byte
//! forever (blueprint §7.2's "frozen prefix" contract — see that test
//! module's own docs). Adding a field there — even a `Default`-valued,
//! append-only-looking one — changes `NodeSpawnSpec`'s serialized byte
//! length, which the frozen-prefix guard treats as exactly the wire break
//! its own module docs warn about, regardless of whether `oxicode`'s wire
//! *format* could tolerate it. Confirmed empirically before this design was
//! chosen: adding `rt` to `NodeSpawnSpec` moved the committed `AddNode`
//! sample's bytes and failed
//! `the_frozen_prefix_of_every_family_is_unchanged`. `astrs-wire` is also
//! outside this task's owned files and no wire change was authorized.
//!
//! So the fix is architectural, not cosmetic: [`SpawnRequest::with_rt`](super::SpawnRequest::with_rt)
//! is a plain builder method on [`super::SpawnRequest`] — parallel to
//! [`SpawnRequest::with_cpu_affinity`](super::SpawnRequest::with_cpu_affinity),
//! but *not* derived from a [`NodeSpawnSpec`](astrs_wire::NodeSpawnSpec)
//! field inside [`SpawnRequest::new`](super::SpawnRequest::new) the way
//! `cpu_affinity` is (see that constructor's own docs). A caller holding a
//! manifest-resolved [`RtConfig`] for a node — the daemon's own
//! `NodeState`/`spawn_node` path, once wired, is the intended one — calls
//! [`with_rt`](super::SpawnRequest::with_rt) itself before spawning; a
//! caller that does not know or care about `rt:` gets
//! [`RtConfig::default`] (unpinned, `SCHED_OTHER`) automatically and pays
//! nothing for the feature it never touches.
//!
//! # Platforms
//!
//! | Platform | What happens |
//! |---|---|
//! | Linux, x86_64/aarch64 | a `pre_exec` hook that calls `sched_setscheduler` between `fork()` and `exec()` — race-free for the same reason [`CpuAffinityOutcome`](super::CpuAffinityOutcome)'s docs give for `cpu_affinity`. `EPERM` is a spawn **failure** (never a silent downgrade to `SCHED_OTHER`) with an actionable error naming `CAP_SYS_NICE` and `RLIMIT_RTPRIO` — RT was promised, and a manifest that asked for it and silently didn't get it would be exactly the "config that lies about what it does" this project's whole validation posture exists to prevent. |
//! | Everything else (macOS, other Unix, Windows, and any other Linux architecture) | No portable, implemented-here way to apply the request. [`RtOutcome::UnsupportedPlatform`] follows: a structured `tracing::warn!` once per spawn, plus [`crate::metrics::DaemonMetrics::record_rt_unsupported`] when a registry is wired in — the exact shape [`super::affinity`]'s own fallback uses. |
//!
//! # Why the `EPERM` message is built in the parent, not the child
//!
//! A `pre_exec` closure's `Err` does not reach the parent's
//! `Command::spawn()` as the `io::Error` the closure actually constructed:
//! std relays a failing closure across the fork through a `CLOEXEC`-flagged
//! pipe carrying a 4-byte big-endian `errno` plus a fixed 4-byte footer (see
//! `library/std/src/sys/pal/unix/process/process_unix.rs`'s `spawn`, which
//! reads exactly `[u8; 8]` and reconstructs the error with
//! `Error::from_raw_os_error`), and the parent-side `Error` is resynthesized
//! from that bare `errno` alone — any string a closure built (`io::Error::other(String)`,
//! for instance) is dropped, never serialized across the pipe. Verified
//! directly with a standalone `pre_exec` probe before this design was
//! chosen: a closure returning `io::Error::other("MY MARKER")` surfaces in
//! the parent as a plain `Error { code: 22, kind: InvalidInput, message:
//! "Invalid argument" }` with no trace of the marker. So the `arm` function's
//! `pre_exec` closure returns only the bare OS error, and `enrich_spawn_error`
//! — run in the parent, *after* `Command::spawn()` has already returned — is
//! what builds the actionable `CAP_SYS_NICE`/`RLIMIT_RTPRIO` message, reading
//! this process's own `RLIMIT_RTPRIO` (a `getrlimit` call, which is exactly
//! as meaningful read in the still-unforked parent as it would be read from
//! inside the child: `RLIMIT_RTPRIO` is inherited unchanged across `fork()`).
//!
//! # Why a raw syscall on Linux
//!
//! `rustix` 1.1.4's `thread::sched` module exposes
//! `sched_setaffinity`/`sched_getaffinity`/`sched_getcpu`/`sched_yield`
//! only — there is no `sched_setscheduler`. The alternative to a raw
//! syscall would be adding `linux-raw-sys` as a new direct dependency (its
//! generated `__NR_sched_setscheduler` constants are exactly what a raw
//! syscall needs), but that means a root-workspace `Cargo.toml` edit, which
//! sits outside this task's owned-files scope while other agents are
//! concurrently editing that same shared file. The `linux` submodule's own
//! docs cite the exact source the two hand-copied syscall numbers below
//! were checked against instead.

use astrs_manifest::{RT_PRIORITY_MAX, RT_PRIORITY_MIN, RtConfig};
// `RtPolicy` is only ever *named* (rather than reached through a `.policy`
// field/method) on the platforms that actually implement `sched_setscheduler`
// below — everywhere else it would be an unused import in non-test code.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use astrs_manifest::RtPolicy;
use astrs_wire::{DataflowId, NodeId};

use crate::metrics::DaemonMetrics;

/// What happened when a spawn request's `rt` reservation was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RtOutcome {
    /// The spec named `RtPolicy::Normal` (or no valid priority accompanied a
    /// real-time policy — see this module's own private `effective_priority`
    /// helper); there was nothing to apply.
    #[default]
    NotRequested,
    /// Armed via a race-free `pre_exec` hook — the child's own program will
    /// not execute its first instruction until after the scheduling class
    /// is in place.
    Applied,
    /// The spec named a real-time policy, but this platform (or this Linux
    /// architecture) has no application implemented here. Logged once and
    /// left on the ordinary time-sharing class rather than silently
    /// ignored.
    UnsupportedPlatform,
}

impl RtOutcome {
    /// Whether the request actually named a real-time policy with a valid
    /// priority.
    #[must_use]
    pub const fn was_requested(self) -> bool {
        !matches!(self, Self::NotRequested)
    }

    /// Whether the reservation was actually put in place.
    #[must_use]
    pub const fn was_applied(self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// The priority to apply, or `None` if there is nothing valid to apply.
///
/// `None` covers two cases identically: `RtPolicy::Normal` (nothing to
/// apply, by design — it carries no `sched_priority` axis at all), and a
/// real-time policy whose priority is missing or outside
/// [`RT_PRIORITY_MIN`]`..=`[`RT_PRIORITY_MAX`]. `astrs_manifest::Manifest::validate`
/// already rejects the latter shape on the normal manifest → plan → spawn
/// path (see `astrs_manifest::node::rt`'s docs for why the pairing is
/// enforced there rather than at parse time), so reaching this function with
/// it can only happen through a hand-built [`RtConfig`] that bypassed
/// validation — a test, an embedder. This is that defense in depth, not the
/// primary guard: it never trusts the pairing blindly, but it also never
/// panics on a shape it does not expect.
#[must_use]
fn effective_priority(rt: RtConfig) -> Option<u8> {
    if !rt.policy.is_realtime() {
        return None;
    }
    rt.priority
        .filter(|priority| (RT_PRIORITY_MIN..=RT_PRIORITY_MAX).contains(priority))
}

// Two sibling definitions, one per supported platform, rather than one
// function branching on `cfg!()` — see `affinity`'s own module docs for why:
// a `cfg!()` boolean is a constant the *codegen* backend folds away, but
// every branch is still fully typechecked and lint-checked first, so a
// single-function version would need `command`/`linux::*` (Linux-only) or
// `dataflow`/`node`/`metrics` (fallback-only) referenced on a platform that
// never uses them — an unused-parameter warning this workspace denies.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) fn arm(
    command: &mut std::process::Command,
    rt: RtConfig,
    _dataflow: DataflowId,
    _node: &NodeId,
    _metrics: Option<&DaemonMetrics>,
) -> RtOutcome {
    let Some(priority) = effective_priority(rt) else {
        return RtOutcome::NotRequested;
    };
    let policy_num = match rt.policy {
        RtPolicy::Fifo => linux::SCHED_FIFO,
        RtPolicy::Rr => linux::SCHED_RR,
        // `effective_priority` only returns `Some` for a real-time policy —
        // `Normal` already returned above. Kept as a reachable arm (rather
        // than `unreachable!()`) so an impossible-today shape degrades to
        // "nothing to apply" instead of panicking mid-spawn; `RtPolicy` is a
        // plain (non-`#[non_exhaustive]`) enum with exactly these three
        // variants, so this arm is presently unreachable in practice, not
        // structurally required — kept anyway as the same defensive style
        // the rest of this module uses.
        RtPolicy::Normal => return RtOutcome::NotRequested,
    };
    linux::arm(command, policy_num, priority);
    RtOutcome::Applied
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub(super) fn arm(
    _command: &mut std::process::Command,
    rt: RtConfig,
    dataflow: DataflowId,
    node: &NodeId,
    metrics: Option<&DaemonMetrics>,
) -> RtOutcome {
    let Some(priority) = effective_priority(rt) else {
        return RtOutcome::NotRequested;
    };
    tracing::warn!(
        dataflow = %dataflow,
        node = %node,
        policy = rt.policy.as_str(),
        priority,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "rt is advisory-unsupported on this platform: no SCHED_FIFO/SCHED_RR \
         application is implemented here, so this node will run under the \
         ordinary time-sharing class",
    );
    if let Some(metrics) = metrics {
        metrics.record_rt_unsupported();
    }
    RtOutcome::UnsupportedPlatform
}

/// Enriches a spawn failure with an actionable message when it is very
/// likely `EPERM` from this module's own `sched_setscheduler` `pre_exec`
/// hook (see this module's private `arm` function and its own top-level
/// docs for why the message cannot be built inside the closure itself) —
/// naming
/// `CAP_SYS_NICE` and `RLIMIT_RTPRIO`, and reading this process's current
/// `RLIMIT_RTPRIO` soft limit, so an operator sees exactly what to grant
/// rather than a bare "Operation not permitted".
///
/// Called from [`super::Spawner::spawn`]'s error path, after
/// `Command::spawn()` has already returned `Err` — the first point a message
/// built here is actually observable by the caller.
///
/// # Why this is a heuristic, not a certainty
///
/// `Command::spawn()`'s `Err` does not say *which* `pre_exec` closure
/// failed, or whether `execve()` itself did. Restricting the rewrite to
/// `ErrorKind::PermissionDenied` (`EPERM` specifically — not `EACCES`, which
/// is what an unreadable or non-executable binary reports) and to the case
/// where `effective_priority` confirms this module's own `arm` would
/// actually have registered the hook narrows this a great deal:
/// `sched_setaffinity(None, ..)` — this project's other `pre_exec` hook —
/// reports `EINVAL` for a bad mask, not `EPERM`, and `execve` itself
/// reporting bare `EPERM` (rather than `EACCES`) needs an LSM/seccomp policy
/// rare enough that this attribution is a defensible engineering judgment in
/// practice, not a formal proof.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) fn enrich_spawn_error(rt: RtConfig, error: std::io::Error) -> std::io::Error {
    if error.kind() != std::io::ErrorKind::PermissionDenied {
        return error;
    }
    let Some(priority) = effective_priority(rt) else {
        return error;
    };
    std::io::Error::other(linux::eperm_message(rt.policy, priority))
}

/// As the Linux x86_64/aarch64 `enrich_spawn_error` above: this platform's
/// `arm` never registers a scheduling `pre_exec` hook at all (see this
/// module's top-level docs), so there is nothing to attribute an `EPERM` to
/// — `error` is returned exactly as `Command::spawn()` produced it.
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub(super) fn enrich_spawn_error(_rt: RtConfig, error: std::io::Error) -> std::io::Error {
    error
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod linux {
    use super::RtPolicy;

    /// `SCHED_FIFO` (`<sched.h>`) — architecture-independent on Linux,
    /// unlike the syscall numbers below.
    pub(super) const SCHED_FIFO: i32 = 1;
    /// `SCHED_RR` (`<sched.h>`) — architecture-independent on Linux.
    pub(super) const SCHED_RR: i32 = 2;

    /// The POSIX `struct sched_param`: a stable, single-`int` kernel ABI
    /// struct, identical across every Linux architecture, so it is defined
    /// by hand here rather than pulled from a header-generated crate.
    #[repr(C)]
    struct SchedParam {
        sched_priority: i32,
    }

    /// Builds the message [`super::enrich_spawn_error`] wraps a real `EPERM`
    /// in — naming `CAP_SYS_NICE` and `RLIMIT_RTPRIO` and reading the
    /// latter's current value, so the operator sees exactly what to grant
    /// rather than a bare "Operation not permitted". Runs in the parent,
    /// well after any fork — see this module's parent docs for why that
    /// matters.
    pub(super) fn eperm_message(policy: RtPolicy, priority: u8) -> String {
        let limit = rustix::process::getrlimit(rustix::process::Resource::Rtprio);
        let soft = limit
            .current
            .map_or_else(|| "unlimited".to_string(), |value| value.to_string());
        format!(
            "cannot apply rt: {{ policy: {policy}, priority: {priority} }} — \
             sched_setscheduler failed with EPERM. This process has neither \
             CAP_SYS_NICE (which alone is sufficient, regardless of the limit \
             below) nor an RLIMIT_RTPRIO soft limit large enough to cover \
             priority {priority} (currently {soft}). Grant CAP_SYS_NICE to the \
             daemon binary (e.g. `setcap cap_sys_nice+ep <path-to-astrs-daemon>`) \
             or raise RLIMIT_RTPRIO for the user running it (e.g. an `rtprio` \
             line in /etc/security/limits.conf), then restart the daemon."
        )
    }

    /// Arms the race-free `pre_exec` hook that calls `sched_setscheduler`.
    ///
    /// Race-freedom is the same argument `affinity::linux::arm`'s docs make
    /// for `sched_setaffinity`: `pre_exec` runs strictly between `fork()`
    /// and `exec()`, in the child's one and only thread at that point, so
    /// there is no window in which the child could run — or spawn its own
    /// threads — under the wrong scheduling class.
    ///
    /// `pre_exec` composes rather than overwrites: `std`'s own `Command`
    /// keeps a `Vec` of registered closures (run in registration order), so
    /// this coexists with `affinity::arm`'s own `pre_exec` call on the same
    /// `command` without either one displacing the other.
    ///
    /// # Why a raw syscall
    ///
    /// See this module's parent docs for why `rustix` cannot be used here
    /// at all. The two syscall numbers below are hand-copied and verified
    /// against `linux-raw-sys` 0.12.1's own generated tables — the exact
    /// crate and version already resolved transitively through `rustix` in
    /// this workspace's `Cargo.lock` — specifically
    /// `src/x86_64/general.rs`'s `pub const __NR_sched_setscheduler: u32 =
    /// 144` and `src/aarch64/general.rs`'s `= 119`. They are POSIX/Linux
    /// ABI constants (stable for the lifetime of the syscall, not
    /// implementation details), and hand-copying them with a citation is
    /// exactly how `rustix` itself sources the syscall number
    /// `affinity::linux::arm` already relies on for `sched_setaffinity` —
    /// this is the same technique, applied to one more syscall.
    ///
    /// Only x86_64 and aarch64 are implemented — the two Linux targets this
    /// workspace's toolchain actually installs and cross-checks (see this
    /// crate's own verification notes). Any other Linux architecture takes
    /// the module's top-level `#[cfg(not(...))]` branch instead of this one,
    /// falling back to the same honest [`super::RtOutcome::UnsupportedPlatform`]
    /// path as macOS, rather than failing to compile.
    pub(super) fn arm(command: &mut std::process::Command, policy_num: i32, priority: u8) {
        // The `unsafe` block below covers the whole closure body — including
        // the `syscall3` call inside it — because an `unsafe` block's
        // permission is lexical (it follows where a closure is *written*,
        // not where or when it is later *called*), so a second nested
        // `unsafe { .. }` immediately inside would be flagged as the
        // redundant block it is, not extra safety.
        //
        // SAFETY: this closure runs in the forked child, strictly between
        // `fork()` and `exec()` — `std::os::unix::process::CommandExt::pre_exec`'s
        // own contract. It performs exactly one syscall
        // (`sched_setscheduler`, invoked directly via `asm!`: no libc, no
        // heap allocation, no lock acquisition) and constructs no `String`
        // or other heap value on any path — the message
        // `super::enrich_spawn_error` builds lives entirely in the parent,
        // after this closure has already returned (see this module's
        // top-level docs for why: a `String` built here would never survive
        // the fork/exec boundary anyway). `policy_num`/`priority` are `Copy`
        // and captured by value, both computed on the parent's stack before
        // the fork — the same hazard `affinity::linux::arm` documents (a
        // forked child sharing its parent's heap-allocator locks with
        // threads that no longer exist on this side of the fork) never
        // arises here at all.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(command, move || {
                let param = SchedParam {
                    sched_priority: i32::from(priority),
                };
                // `sched_setscheduler(0, policy, &param)` — pid `0` means
                // "the calling process" (POSIX), i.e. this freshly forked
                // child, exactly like `sched_setaffinity`'s `None` in
                // `affinity::linux::arm`. `param` is a valid, live,
                // correctly-shaped `sched_param` on this closure's own stack
                // for the entire duration of the call.
                let ret = syscall3(
                    SYS_SCHED_SETSCHEDULER,
                    0,
                    policy_num as usize,
                    std::ptr::from_ref(&param) as usize,
                );
                if ret < 0 {
                    // Raw Linux syscalls return `-errno` in the return
                    // register, not a separate errno variable — recovering
                    // it this way is what lets `std::io::Error` classify it
                    // exactly as it would any libc-mediated OS error
                    // (`ErrorKind::PermissionDenied` for `EPERM`), with no
                    // hand-maintained errno table of our own. The resulting
                    // bare `io::Error` (no message attached — see this
                    // module's top-level docs for why one built here would
                    // be lost anyway) is exactly what `Command::spawn()`
                    // relays to the parent, and exactly what
                    // `super::enrich_spawn_error` expects to receive there.
                    let errno = i32::try_from(ret.unsigned_abs()).unwrap_or(i32::MAX);
                    return Err(std::io::Error::from_raw_os_error(errno));
                }
                Ok(())
            });
        }
    }

    /// Syscall numbers (`<asm/unistd.h>`), verified against `linux-raw-sys`
    /// 0.12.1's generated per-architecture tables — see this submodule's own
    /// `arm` function's docs.
    #[cfg(target_arch = "x86_64")]
    const SYS_SCHED_SETSCHEDULER: usize = 144;
    #[cfg(target_arch = "aarch64")]
    const SYS_SCHED_SETSCHEDULER: usize = 119;

    /// `sched_getscheduler(pid)`, for tests only — reads back the class
    /// `arm` applied. Not used by production code: nothing here needs to
    /// *read* the scheduling class back, only set it.
    #[cfg(test)]
    #[cfg(target_arch = "x86_64")]
    const SYS_SCHED_GETSCHEDULER: usize = 145;
    #[cfg(test)]
    #[cfg(target_arch = "aarch64")]
    const SYS_SCHED_GETSCHEDULER: usize = 120;

    /// Reads back the `SCHED_*` class of `pid` via a raw `sched_getscheduler`
    /// call — test-only verification for `arm`'s effect, the same role
    /// `rustix::thread::sched_getaffinity` plays in `affinity`'s own tests
    /// (`rustix` has no equivalent for the scheduling *class*, only the CPU
    /// mask, which is exactly this module's parent docs' reason for existing
    /// at all).
    #[cfg(test)]
    pub(super) fn getscheduler(pid: i32) -> std::io::Result<i32> {
        // SAFETY: `sched_getscheduler(pid)` takes and mutates nothing but
        // its one integer argument; the trailing two register slots
        // `syscall3` reserves are simply unused by this two-argument-arity
        // syscall (extra unused argument registers are never read by the
        // kernel's handler), and it has no memory-safety preconditions
        // beyond a live `pid`.
        let ret = unsafe { syscall3(SYS_SCHED_GETSCHEDULER, pid as usize, 0, 0) };
        if ret < 0 {
            let errno = i32::try_from(ret.unsigned_abs()).unwrap_or(i32::MAX);
            return Err(std::io::Error::from_raw_os_error(errno));
        }
        i32::try_from(ret)
            .map_err(|_| std::io::Error::other("sched_getscheduler: policy out of range"))
    }

    /// A 3-argument Linux syscall, invoked directly — no libc (Pure Rust
    /// Absolute). Register conventions mirror `rustix` 1.1.4's own
    /// `backend::linux_raw::arch::{x86_64,aarch64}::syscall3` exactly (this
    /// workspace already depends on `rustix`, and its `linux_raw` backend —
    /// the same one `affinity::linux::arm` runs through — is the reference
    /// implementation these two blocks were checked against): `syscall`
    /// with the number in `rax` and `rcx`/`r11` clobbered on x86_64 (the
    /// `syscall` instruction's own use of those registers to stash the
    /// return address and flags), `svc 0` with the number in `x8` and the
    /// return in `x0` on aarch64.
    ///
    /// # Safety
    ///
    /// `nr` must name a syscall whose first three arguments accept `a0`,
    /// `a1`, `a2` as given (a pointer argument packed into a `usize` must
    /// point at valid, correctly-shaped, live memory for the syscall's
    /// duration) — the same precondition any raw syscall invocation carries.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    unsafe fn syscall3(nr: usize, a0: usize, a1: usize, a2: usize) -> isize {
        let ret: usize;
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") nr => ret,
                in("rdi") a0,
                in("rsi") a1,
                in("rdx") a2,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack, preserves_flags),
            );
        }
        ret as isize
    }

    /// As the x86_64 [`syscall3`] above, via aarch64's `svc 0`.
    ///
    /// # Safety
    ///
    /// As the x86_64 overload.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    unsafe fn syscall3(nr: usize, a0: usize, a1: usize, a2: usize) -> isize {
        let ret: usize;
        unsafe {
            core::arch::asm!(
                "svc 0",
                in("x8") nr,
                inlateout("x0") a0 => ret,
                in("x1") a1,
                in("x2") a2,
                options(nostack, preserves_flags),
            );
        }
        ret as isize
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Mutex;

    use astrs_manifest::RtPolicy;
    use tracing::field::{Field, Visit};
    use tracing::span;

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    fn node() -> NodeId {
        NodeId::new("control-loop").unwrap()
    }

    #[test]
    fn no_rt_requested_is_not_requested() {
        let mut command = std::process::Command::new("/usr/bin/true");
        let outcome = arm(&mut command, RtConfig::default(), dataflow(), &node(), None);
        assert_eq!(outcome, RtOutcome::NotRequested);
        assert!(!outcome.was_requested());
        assert!(!outcome.was_applied());
    }

    #[test]
    fn a_realtime_policy_with_no_valid_priority_is_defensively_not_requested() {
        // `Manifest::validate` never lets this shape reach a real spawn —
        // this is the defense-in-depth branch `effective_priority` docs
        // describe, exercised directly against a hand-built `RtConfig`.
        let missing = RtConfig {
            policy: RtPolicy::Fifo,
            priority: None,
        };
        let out_of_range = RtConfig {
            policy: RtPolicy::Rr,
            priority: Some(0),
        };
        for rt in [missing, out_of_range] {
            let mut command = std::process::Command::new("/usr/bin/true");
            let outcome = arm(&mut command, rt, dataflow(), &node(), None);
            assert_eq!(outcome, RtOutcome::NotRequested, "{rt:?}");
        }
    }

    #[test]
    fn outcome_classifies_itself() {
        assert!(!RtOutcome::NotRequested.was_requested());
        assert!(RtOutcome::Applied.was_requested());
        assert!(RtOutcome::Applied.was_applied());
        assert!(RtOutcome::UnsupportedPlatform.was_requested());
        assert!(!RtOutcome::UnsupportedPlatform.was_applied());
    }

    #[test]
    fn effective_priority_accepts_only_a_realtime_policy_in_range() {
        assert_eq!(
            effective_priority(RtConfig {
                policy: RtPolicy::Normal,
                priority: None,
            }),
            None
        );
        assert_eq!(
            effective_priority(RtConfig {
                policy: RtPolicy::Fifo,
                priority: Some(80),
            }),
            Some(80)
        );
        assert_eq!(
            effective_priority(RtConfig {
                policy: RtPolicy::Rr,
                priority: Some(100),
            }),
            None,
            "100 is outside RT_PRIORITY_MAX"
        );
    }

    #[test]
    fn enrich_spawn_error_only_rewrites_a_real_permission_denied_with_a_pending_request() {
        // A non-EPERM error is passed through untouched regardless of `rt`.
        let unrelated = std::io::Error::from(std::io::ErrorKind::NotFound);
        let rt = RtConfig {
            policy: RtPolicy::Fifo,
            priority: Some(50),
        };
        let passed_through = enrich_spawn_error(rt, unrelated);
        assert_eq!(passed_through.kind(), std::io::ErrorKind::NotFound);

        // A `PermissionDenied` with nothing real-time requested is also
        // passed through untouched — nothing here could be the rt hook's
        // fault.
        let permission_denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let unrequested = enrich_spawn_error(RtConfig::default(), permission_denied);
        assert_eq!(unrequested.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            !unrequested.to_string().contains("CAP_SYS_NICE"),
            "{unrequested}"
        );
    }

    // A minimal `tracing::Subscriber` that records every event's message and
    // level — enough to assert a specific WARN fired, without pulling in a
    // dev-dependency this crate does not otherwise need. Identical in shape
    // to `affinity`'s own (each module keeps its own copy rather than
    // sharing one across a private module boundary).
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
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        ignore = "the real Linux x86_64/aarch64 path applies the scheduler \
        directly and never falls back to the WARN this test asserts on"
    )]
    fn an_unsupported_platform_warns_once_and_still_reports_unsupported() {
        // `with_default` scopes the subscriber to this thread's call stack
        // only (unlike `set_global_default`, which may run once per
        // process), so this needs no cross-test synchronization.
        let subscriber = std::sync::Arc::new(CaptureSubscriber::default());
        let rt = RtConfig {
            policy: RtPolicy::Fifo,
            priority: Some(50),
        };
        let outcome = tracing::subscriber::with_default(subscriber.clone(), || {
            let mut command = std::process::Command::new("/usr/bin/true");
            arm(&mut command, rt, dataflow(), &node(), None)
        });
        assert_eq!(outcome, RtOutcome::UnsupportedPlatform);
        assert!(outcome.was_requested());
        assert!(!outcome.was_applied());

        let events = subscriber
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1, "exactly one WARN per spawn: {events:?}");
        let (level, message) = &events[0];
        assert_eq!(*level, tracing::Level::WARN);
        assert!(message.contains("rt"), "{message}");
        assert!(message.contains("unsupported"), "{message}");
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn linux_applies_scheduling_or_fails_with_an_actionable_eperm() {
        // Exercised against a real spawned child (`sleep`, so it is still
        // alive long enough to read its scheduling class back), the same
        // way `process.rs`'s own
        // `linux_spawn_applies_the_requested_affinity_to_the_child` reads
        // affinity back from a real child rather than the unobservable
        // `pre_exec` closure directly.
        //
        // Both outcomes are asserted, matching whichever this process
        // actually has: `EPERM` (no `CAP_SYS_NICE`, and an `RLIMIT_RTPRIO`
        // soft limit of 0 — the default for an unprivileged CI runner) is
        // asserted for its actionable content once run through
        // `enrich_spawn_error` — exactly what `Spawner::spawn` does — since
        // the raw `Command::spawn()` error carries no message of its own
        // (see this module's top-level docs); success (running as root, or
        // under a raised `RLIMIT_RTPRIO`) is asserted by reading the applied
        // class back. Either branch is deterministic for a given
        // environment — this is not a flaky race between the two.
        let mut command = std::process::Command::new("/bin/sleep");
        command.arg("5");
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());

        let rt = RtConfig {
            policy: RtPolicy::Fifo,
            priority: Some(10),
        };
        let outcome = arm(&mut command, rt, dataflow(), &node(), None);
        assert_eq!(outcome, RtOutcome::Applied);

        match command.spawn() {
            Ok(mut child) => {
                let pid = i32::try_from(child.id()).expect("a freshly spawned pid fits i32");
                let policy = linux::getscheduler(pid).expect(
                    "the child is still alive: it is sleeping, and nothing has reaped it yet",
                );
                assert_eq!(policy, linux::SCHED_FIFO);
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(error) => {
                let enriched = enrich_spawn_error(rt, error);
                let message = enriched.to_string();
                assert!(
                    message.contains("CAP_SYS_NICE"),
                    "an EPERM spawn failure must name the capability: {message}"
                );
                assert!(
                    message.contains("RLIMIT_RTPRIO"),
                    "an EPERM spawn failure must name the rlimit: {message}"
                );
            }
        }
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn the_eperm_message_names_the_capability_and_the_rlimit() {
        let message = linux::eperm_message(RtPolicy::Fifo, 80);
        assert!(message.contains("CAP_SYS_NICE"), "{message}");
        assert!(message.contains("RLIMIT_RTPRIO"), "{message}");
        assert!(message.contains("80"), "{message}");
    }
}
