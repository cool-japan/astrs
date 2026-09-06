//! Producer and consumer liveness: watching a process die, portably.
//!
//! Blueprint §6.2: *"The daemon holds every segment fd; on producer death
//! (pidfd on Linux, kqueue `EVFILT_PROC` on macOS) it marks `closed`, lets
//! consumers drain, and unlinks."* This module is the watch half of that
//! sentence; [`crate::SegmentBroker`] is the acting half.
//!
//! # Three mechanisms, one interface
//!
//! | Platform | Mechanism | Cost while idle |
//! |---|---|---|
//! | Linux | `pidfd_open` + `poll` | none — the kernel wakes us |
//! | macOS / BSD | `kqueue` `EVFILT_PROC`/`NOTE_EXIT` + `poll` | none |
//! | either, when registration is refused | `kill(pid, 0)` at an interval | one cheap syscall per interval |
//!
//! The fallback matters: `pidfd_open` needs Linux 5.3 and the right to signal
//! the target, and Darwin's `EVFILT_PROC` refuses processes the caller cannot
//! signal. Neither is guaranteed for a daemon supervising a node it did not
//! spawn (an externally started node attaching to a dataflow, §9.1). Rather
//! than fail the route, the watch degrades to polling and says so through
//! [`ProcessWatch::is_event_driven`], so `astrs doctor` can report the
//! difference instead of a supervisor silently going blind.
//!
//! # Examples
//!
//! ```
//! # #[cfg(unix)] {
//! use astrs_shm::ProcessWatch;
//! use std::time::Duration;
//!
//! // Watching ourselves never fires.
//! let watch = ProcessWatch::current();
//! assert!(watch.is_alive());
//! assert!(!watch.wait(Some(Duration::from_millis(10)))?);
//! # }
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```

use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use crate::error::ShmResult;
use crate::os;

/// How often the polling fallback re-checks `kill(pid, 0)`.
///
/// 25 ms bounds the detection latency well below the 5 s heartbeat interval
/// of §24.2 while costing one syscall per interval per watched process — at
/// the scale of a daemon supervising a few dozen nodes, unmeasurable.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// A watch on one process.
#[derive(Debug)]
pub struct ProcessWatch {
    inner: os::ProcessWatchImpl,
    poll_interval: Duration,
}

impl ProcessWatch {
    /// Start watching `pid`.
    ///
    /// Never fails: a process that cannot be watched by kernel notification
    /// is watched by polling.
    #[must_use]
    pub fn new(pid: i64) -> Self {
        Self {
            inner: os::ProcessWatchImpl::new(pid),
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// Watch this process — useful in tests and as a null watch.
    #[must_use]
    pub fn current() -> Self {
        Self::new(os::current_pid())
    }

    /// Watch the producer recorded in a segment's header.
    #[must_use]
    pub fn for_segment(segment: &crate::Segment) -> Self {
        Self::new(segment.header().producer_pid())
    }

    /// Override the polling fallback's interval.
    #[must_use]
    pub const fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// The watched pid.
    #[must_use]
    pub const fn pid(&self) -> i64 {
        self.inner.pid()
    }

    /// Whether the process still exists, without blocking.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    /// Whether the watch is backed by a kernel notification rather than
    /// polling.
    ///
    /// Surfaced so `astrs doctor` can report a degraded supervision path
    /// instead of hiding it.
    #[must_use]
    pub const fn is_event_driven(&self) -> bool {
        self.inner.is_event_driven()
    }

    /// The pollable descriptor, for callers folding this into their own event
    /// loop. `None` when the watch is polling-based.
    #[must_use]
    pub fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        self.inner.as_fd()
    }

    /// Wait for the process to exit.
    ///
    /// Returns `true` when it is gone, `false` on timeout. `None` waits
    /// indefinitely.
    ///
    /// # Errors
    ///
    /// [`crate::ShmError::Os`] if the underlying wait fails.
    /// A `pidfd` becomes readable, and a `kqueue` `NOTE_EXIT` fires, only when
    /// the process has actually terminated — so an event-driven readiness is
    /// authoritative and is **not** re-confirmed with `kill(pid, 0)`. That
    /// re-confirmation would be actively wrong: a terminated child stays
    /// reapable as a zombie, and `kill(pid, 0)` reports a zombie as alive, so
    /// checking again would report "still running" for a process that has
    /// demonstrably exited.
    pub fn wait(&self, timeout: Option<Duration>) -> ShmResult<bool> {
        self.inner.wait(timeout, self.poll_interval)
    }

    /// Block until the process exits.
    ///
    /// # Errors
    ///
    /// As [`ProcessWatch::wait`].
    pub fn wait_exit(&self) -> ShmResult<()> {
        loop {
            if self.wait(None)? {
                return Ok(());
            }
        }
    }
}

/// The portable fallback: poll `kill(pid, 0)` until the process is gone or
/// the budget runs out.
///
/// Returns `true` when the process disappeared.
pub(crate) fn poll_until_gone(pid: i64, timeout: Option<Duration>, interval: Duration) -> bool {
    let deadline = timeout.map(|budget| Instant::now() + budget);
    let interval = interval.max(Duration::from_millis(1));
    loop {
        if !os::process_alive(pid) {
            return true;
        }
        match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                std::thread::sleep(interval.min(deadline - now));
            }
            None => std::thread::sleep(interval),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn watching_ourselves_never_reports_an_exit() {
        let watch = ProcessWatch::current();
        assert_eq!(watch.pid(), os::current_pid());
        assert!(watch.is_alive());
        assert!(!watch.wait(Some(Duration::from_millis(20))).unwrap());
    }

    #[test]
    fn an_impossible_pid_is_immediately_gone() {
        let watch =
            ProcessWatch::new(i64::from(i32::MAX) - 1).with_poll_interval(Duration::from_millis(2));
        assert!(!watch.is_alive());
        assert!(watch.wait(Some(Duration::from_millis(200))).unwrap());
        watch.wait_exit().unwrap();
    }

    #[test]
    fn a_negative_pid_is_never_alive() {
        let watch = ProcessWatch::new(-42);
        assert!(!watch.is_alive());
        assert!(!watch.is_event_driven());
        assert!(watch.as_fd().is_none());
    }

    #[test]
    fn a_real_child_exit_is_observed() {
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 0.05")
            .spawn();
        let Ok(mut child) = child else {
            // A sandbox without `sh` still has to pass the rest of the suite.
            return;
        };
        let watch = ProcessWatch::new(i64::from(child.id()));
        assert!(watch.is_alive());
        assert!(
            watch.wait(Some(Duration::from_secs(5))).unwrap(),
            "the child must be observed exiting"
        );
        // `kill(pid, 0)` reports a *zombie* as alive, so the liveness check
        // only agrees once the child has been reaped — which is exactly why
        // `wait` does not re-confirm an event-driven exit.
        let _ = child.wait();
        assert!(!watch.is_alive());
    }

    #[test]
    fn the_polling_fallback_respects_its_deadline() {
        let started = Instant::now();
        let gone = poll_until_gone(
            os::current_pid(),
            Some(Duration::from_millis(40)),
            Duration::from_millis(5),
        );
        assert!(!gone);
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn watching_a_segment_targets_its_recorded_producer() {
        use crate::{Segment, SegmentConfig, SegmentKey};
        use astrs_wire::DataflowId;

        let key = SegmentKey::from_parts(DataflowId::generate(), "watched", "out", 1).unwrap();
        let segment = Segment::create(key, SegmentConfig::new(2, 128).unwrap()).unwrap();
        let watch = ProcessWatch::for_segment(&segment);
        assert_eq!(watch.pid(), os::current_pid());
        assert!(watch.is_alive());
    }
}
