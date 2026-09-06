//! [`ProcessHandle`] — a signalling handle stamped with its incarnation.
//!
//! Every signal the daemon sends is addressed to a *(process group,
//! generation)* pair, never to a bare pid. That is the whole point of the
//! type, and it exists because of one race:
//!
//! ```text
//!   t0  node `detect` (generation 4) is asked to stop; a finish-grace
//!       watchdog is armed for t0 + 15 s
//!   t1  the node exits on its own; the restart policy respawns it as
//!       generation 5, which the kernel is entirely free to give the same pid
//!   t2  the watchdog fires — and without a generation check, SIGKILLs a
//!       healthy new incarnation
//! ```
//!
//! [`ProcessHandle::signal_if_current`] closes it: the watchdog carries the
//! generation it armed for, the handle carries the generation it belongs to,
//! and a mismatch is a no-op that reports itself
//! ([`SignalOutcome::StaleGeneration`]) rather than a silent miss.
//!
//! # Process groups
//!
//! Each node is spawned into its own process group (`setpgid(0, 0)` via
//! `Command::process_group(0)`), so a node that forks children — a Python node
//! under a launcher, a build step that shells out — is killable as a unit.
//! [`ProcessHandle::signal_group`] addresses the group; [`ProcessHandle::signal`]
//! addresses only the leader, for the rare case where that is what is wanted.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::spawn::{ProcessHandle, SignalOutcome};
//! use astrs_wire::{DataflowId, NodeId};
//!
//! // A handle for a pid that is certainly not running.
//! let handle = ProcessHandle::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("detect")?,
//!     4,
//!     u32::MAX,
//! );
//! assert_eq!(handle.generation(), 4);
//! assert_eq!(
//!     handle.terminate_if_current(5),
//!     SignalOutcome::StaleGeneration { current: 4, expected: 5 },
//! );
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use astrs_wire::{DataflowId, NodeId};
use rustix::process::{Pid, Signal};

/// What a signal attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalOutcome {
    /// The signal was delivered.
    Delivered,
    /// The process is already gone; nothing to signal.
    ///
    /// Not an error: a node that exited between the decision and the signal is
    /// the normal case, not a fault.
    AlreadyGone,
    /// The handle belongs to a different incarnation, so nothing was signalled.
    StaleGeneration {
        /// The generation this handle belongs to.
        current: u64,
        /// The generation the caller armed for.
        expected: u64,
    },
    /// The signal could not be delivered.
    Failed {
        /// The raw `errno`.
        errno: i32,
    },
}

impl SignalOutcome {
    /// Whether the signal actually reached a process.
    #[must_use]
    pub const fn is_delivered(self) -> bool {
        matches!(self, Self::Delivered)
    }

    /// Whether the attempt was skipped rather than attempted and failed.
    #[must_use]
    pub const fn is_skipped(self) -> bool {
        matches!(self, Self::AlreadyGone | Self::StaleGeneration { .. })
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::AlreadyGone => "already_gone",
            Self::StaleGeneration { .. } => "stale_generation",
            Self::Failed { .. } => "failed",
        }
    }
}

impl core::fmt::Display for SignalOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delivered => f.write_str("delivered"),
            Self::AlreadyGone => f.write_str("the process is already gone"),
            Self::StaleGeneration { current, expected } => write!(
                f,
                "handle is generation {current}, caller armed for {expected}"
            ),
            Self::Failed { errno } => write!(f, "signal failed with errno {errno}"),
        }
    }
}

/// A handle to one spawned node incarnation.
///
/// Cheap to clone: every clone addresses the same process and shares the
/// "reaped" flag, so a handle held by a watchdog stops signalling as soon as
/// the waiter task has seen the child exit.
#[derive(Debug, Clone)]
pub struct ProcessHandle {
    /// The dataflow the node belongs to.
    dataflow: DataflowId,
    /// The node.
    node: NodeId,
    /// The incarnation this handle addresses (§12).
    generation: u64,
    /// The process id.
    pid: u32,
    /// Whether the child has been reaped, shared with the waiter task.
    reaped: Arc<AtomicBool>,
}

impl ProcessHandle {
    /// A handle for `pid`, belonging to `generation`.
    #[must_use]
    pub fn new(dataflow: DataflowId, node: NodeId, generation: u64, pid: u32) -> Self {
        Self {
            dataflow,
            node,
            generation,
            pid,
            reaped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The dataflow this process belongs to.
    #[must_use]
    pub const fn dataflow(&self) -> DataflowId {
        self.dataflow
    }

    /// The node this process is.
    #[must_use]
    pub const fn node(&self) -> &NodeId {
        &self.node
    }

    /// The incarnation this handle addresses.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether this handle addresses `generation`.
    #[must_use]
    pub const fn is_generation(&self, generation: u64) -> bool {
        self.generation == generation
    }

    /// Marks the child reaped; further signals become
    /// [`SignalOutcome::AlreadyGone`].
    pub fn mark_reaped(&self) {
        self.reaped.store(true, Ordering::Release);
    }

    /// Whether the waiter task has reaped this child.
    #[must_use]
    pub fn is_reaped(&self) -> bool {
        self.reaped.load(Ordering::Acquire)
    }

    /// Whether the process still exists, as far as `kill(pid, 0)` can tell.
    ///
    /// A reaped child is reported gone without a syscall — after a `wait`, the
    /// pid may already belong to somebody else, and asking about it is exactly
    /// the mistake this type exists to prevent.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        if self.is_reaped() {
            return false;
        }
        match self.checked_pid() {
            Some(pid) => rustix::process::test_kill_process(pid).is_ok(),
            None => false,
        }
    }

    /// Sends `signal` to the process group, unconditionally.
    pub fn signal_group(&self, signal: Signal) -> SignalOutcome {
        self.deliver(signal, true)
    }

    /// Sends `signal` to the leader only, unconditionally.
    pub fn signal(&self, signal: Signal) -> SignalOutcome {
        self.deliver(signal, false)
    }

    /// Sends `signal` to the process group only if this handle is `generation`.
    ///
    /// The generation-guarded form every timer-driven escalation must use.
    pub fn signal_if_current(&self, generation: u64, signal: Signal) -> SignalOutcome {
        if !self.is_generation(generation) {
            return SignalOutcome::StaleGeneration {
                current: self.generation,
                expected: generation,
            };
        }
        self.signal_group(signal)
    }

    /// `SIGTERM` to the process group.
    pub fn terminate(&self) -> SignalOutcome {
        self.signal_group(Signal::TERM)
    }

    /// `SIGKILL` to the process group.
    pub fn kill(&self) -> SignalOutcome {
        self.signal_group(Signal::KILL)
    }

    /// Generation-guarded `SIGTERM`.
    pub fn terminate_if_current(&self, generation: u64) -> SignalOutcome {
        self.signal_if_current(generation, Signal::TERM)
    }

    /// Generation-guarded `SIGKILL`.
    pub fn kill_if_current(&self, generation: u64) -> SignalOutcome {
        self.signal_if_current(generation, Signal::KILL)
    }

    /// The pid as a `rustix` value, or `None` if it is not a signalable pid.
    fn checked_pid(&self) -> Option<Pid> {
        let raw = i32::try_from(self.pid).ok()?;
        Pid::from_raw(raw)
    }

    /// The shared signal path.
    fn deliver(&self, signal: Signal, group: bool) -> SignalOutcome {
        if self.is_reaped() {
            return SignalOutcome::AlreadyGone;
        }
        let Some(pid) = self.checked_pid() else {
            return SignalOutcome::AlreadyGone;
        };
        let result = if group {
            rustix::process::kill_process_group(pid, signal)
        } else {
            rustix::process::kill_process(pid, signal)
        };
        match result {
            Ok(()) => SignalOutcome::Delivered,
            Err(errno) if errno == rustix::io::Errno::SRCH => SignalOutcome::AlreadyGone,
            Err(errno) => SignalOutcome::Failed {
                errno: errno.raw_os_error(),
            },
        }
    }
}

impl core::fmt::Display for ProcessHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}/{} generation {} (pid {})",
            self.dataflow, self.node, self.generation, self.pid
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn handle(generation: u64, pid: u32) -> ProcessHandle {
        ProcessHandle::new(
            DataflowId::from_u128(1),
            NodeId::new("detect").unwrap(),
            generation,
            pid,
        )
    }

    #[test]
    fn a_handle_reports_what_it_addresses() {
        let handle = handle(4, 1234);
        assert_eq!(handle.generation(), 4);
        assert_eq!(handle.pid(), 1234);
        assert_eq!(handle.node().as_str(), "detect");
        assert_eq!(handle.dataflow(), DataflowId::from_u128(1));
        assert!(handle.is_generation(4));
        assert!(!handle.is_generation(5));
    }

    #[test]
    fn a_stale_generation_signal_is_a_reported_no_op() {
        // pid 1 is init; if the guard failed we would learn about it loudly.
        let handle = handle(4, 1);
        assert_eq!(
            handle.terminate_if_current(5),
            SignalOutcome::StaleGeneration {
                current: 4,
                expected: 5
            }
        );
        assert_eq!(
            handle.kill_if_current(99),
            SignalOutcome::StaleGeneration {
                current: 4,
                expected: 99
            }
        );
    }

    #[test]
    fn a_reaped_handle_never_signals_again() {
        let handle = handle(1, 1);
        handle.mark_reaped();
        assert!(handle.is_reaped());
        assert!(!handle.is_alive());
        assert_eq!(handle.terminate(), SignalOutcome::AlreadyGone);
        assert_eq!(handle.kill(), SignalOutcome::AlreadyGone);
        assert_eq!(handle.terminate_if_current(1), SignalOutcome::AlreadyGone);
    }

    #[test]
    fn clones_share_the_reaped_flag() {
        let handle = handle(1, 1);
        let clone = handle.clone();
        handle.mark_reaped();
        assert!(clone.is_reaped(), "the watchdog's copy must stop too");
    }

    #[test]
    fn an_unsignalable_pid_reports_gone_rather_than_failing() {
        let handle = handle(1, u32::MAX);
        assert!(!handle.is_alive());
        assert_eq!(handle.terminate(), SignalOutcome::AlreadyGone);
    }

    #[test]
    fn this_process_is_alive_and_signalable() {
        let handle = handle(1, std::process::id());
        assert!(handle.is_alive());
        // Signal 0 semantics: `test_kill_process` already proved reachability;
        // sending a real signal to ourselves is not something a test should do.
    }

    #[test]
    fn outcomes_classify_and_render() {
        assert!(SignalOutcome::Delivered.is_delivered());
        assert!(!SignalOutcome::Delivered.is_skipped());
        assert!(SignalOutcome::AlreadyGone.is_skipped());
        assert!(
            SignalOutcome::StaleGeneration {
                current: 1,
                expected: 2
            }
            .is_skipped()
        );
        assert!(!SignalOutcome::Failed { errno: 1 }.is_skipped());

        for outcome in [
            SignalOutcome::Delivered,
            SignalOutcome::AlreadyGone,
            SignalOutcome::StaleGeneration {
                current: 1,
                expected: 2,
            },
            SignalOutcome::Failed { errno: 13 },
        ] {
            assert!(!outcome.to_string().is_empty());
            assert!(!outcome.kind_name().is_empty());
        }
        assert_eq!(
            SignalOutcome::StaleGeneration {
                current: 1,
                expected: 2
            }
            .kind_name(),
            "stale_generation"
        );
    }

    #[test]
    fn display_names_the_incarnation() {
        let rendered = handle(4, 1234).to_string();
        assert!(rendered.contains("detect"), "{rendered}");
        assert!(rendered.contains("generation 4"), "{rendered}");
        assert!(rendered.contains("pid 1234"), "{rendered}");
    }
}
