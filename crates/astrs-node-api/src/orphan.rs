//! The orphan guard (blueprint §4.2).
//!
//! > **`astrs run`** — the single-process mode: the CLI embeds an in-process
//! > daemon […] under one supervisor with the orphan guard (parent-pid +
//! > process-group kill).
//!
//! A node spawned by a supervisor is told that supervisor's process id in
//! `ASTRS_RUN_PARENT_PID`. If the supervisor dies without stopping its
//! children — `kill -9` on the CLI, an OOM kill, a laptop lid — those children
//! keep running, keep holding shared-memory segments, and keep their ports
//! bound. The guard is what stops that: a low-frequency watch on the parent
//! that turns its disappearance into an ordinary [`Event::Stop`], so the node
//! winds down through exactly the same path a requested stop takes.
//!
//! # Why polling
//!
//! `pidfd_open` (Linux 5.3+) and `kqueue`'s `EVFILT_PROC` (BSD, macOS) both
//! give an *event* rather than a poll, and `astrs-shm`'s
//! [`ProcessWatch`](astrs_shm::ProcessWatch) already wraps them for the
//! segment plane. The guard deliberately does not use them: it watches a
//! process that is not a child of this one, where `pidfd` needs a capability
//! this process may not have and `kqueue` needs a descriptor it cannot obtain.
//! A `kill(pid, 0)` every two seconds costs one syscall per node per two
//! seconds and works everywhere, which for a liveness check with a
//! human-scale deadline is the right trade.
//!
//! # Pid reuse
//!
//! A pid can be reused, so "the pid still exists" is not quite "my parent is
//! still alive". The window is small (the supervisor would have to die *and*
//! the pid be recycled between two polls) and the consequence is benign (the
//! node keeps running and is stopped by the ordinary path instead). The guard
//! documents this rather than pretending otherwise.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use astrs_wire::StopCause;
use tokio::task::JoinHandle;

use crate::events::{Event, EventSource};
use crate::runtime::NodeRuntime;

/// How often the guard checks its parent, by default.
///
/// Two seconds: fast enough that an orphan does not outlive its supervisor by
/// anything a human notices, slow enough to be invisible in a profile.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Whether a process with this id currently exists.
///
/// Implemented with the null signal (`kill(pid, 0)`), which performs the
/// permission and existence checks without delivering anything. A process that
/// exists but belongs to another user answers `true`: the check is *liveness*,
/// not ownership.
///
/// On a platform without the null-signal check this returns `true`, because
/// "cannot tell" must not be reported as "gone" — a guard that stops a healthy
/// node is worse than one that never fires.
///
/// # Examples
///
/// ```
/// use astrs_node_api::orphan::process_exists;
///
/// assert!(process_exists(std::process::id()), "this process exists");
/// ```
#[must_use]
pub fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        let Some(pid) = rustix::process::Pid::from_raw(raw) else {
            return false;
        };
        match rustix::process::test_kill_process(pid) {
            Ok(()) => true,
            // `EPERM` means "it exists, and you may not signal it" — which is
            // still alive, and is what a node sees when its supervisor runs as
            // another user.
            Err(error) => error == rustix::io::Errno::PERM,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// A running watch on a supervising process.
#[derive(Debug)]
pub struct OrphanGuard {
    /// The pid being watched.
    parent: u32,
    /// Set to stop the watch.
    stop: Arc<AtomicBool>,
    /// Set once the parent was observed to be gone.
    fired: Arc<AtomicBool>,
    /// How many polls the watch has performed.
    polls: Arc<AtomicU64>,
    /// The watch task.
    task: JoinHandle<()>,
}

impl OrphanGuard {
    /// Starts watching `parent`, stopping `source` when it disappears.
    #[must_use]
    pub fn watch(
        parent: u32,
        runtime: &NodeRuntime,
        source: Arc<EventSource>,
        interval: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let polls = Arc::new(AtomicU64::new(0));
        let task = {
            let stop = Arc::clone(&stop);
            let fired = Arc::clone(&fired);
            let polls = Arc::clone(&polls);
            runtime.spawn(async move {
                let interval = interval.max(Duration::from_millis(10));
                loop {
                    tokio::time::sleep(interval).await;
                    if stop.load(Ordering::Acquire) || source.is_closed() {
                        return;
                    }
                    polls.fetch_add(1, Ordering::Relaxed);
                    if process_exists(parent) {
                        continue;
                    }
                    fired.store(true, Ordering::Release);
                    source.push_control(Event::Error(format!(
                        "the supervising process {parent} exited; this node is orphaned"
                    )));
                    source.push_control(Event::Stop(StopCause::DaemonShutdown));
                    return;
                }
            })
        };
        Self {
            parent,
            stop,
            fired,
            polls,
            task,
        }
    }

    /// Starts a guard only if `parent` is set (§4.2: the guard is optional).
    #[must_use]
    pub fn maybe_watch(
        parent: Option<u32>,
        runtime: &NodeRuntime,
        source: Arc<EventSource>,
        interval: Duration,
    ) -> Option<Self> {
        parent.map(|parent| Self::watch(parent, runtime, source, interval))
    }

    /// The pid being watched.
    #[must_use]
    pub const fn parent(&self) -> u32 {
        self.parent
    }

    /// Whether the guard has observed the parent's disappearance.
    #[must_use]
    pub fn has_fired(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    /// How many polls the watch has performed.
    #[must_use]
    pub fn poll_count(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }

    /// Stops the watch.
    ///
    /// Idempotent, and implied by [`Drop`].
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for OrphanGuard {
    fn drop(&mut self) {
        self.stop();
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn this_process_exists_and_an_impossible_pid_does_not() {
        assert!(process_exists(std::process::id()));
        // Pid 0 addresses the caller's process *group* on Unix, and is never
        // a process id; the guard must not read it as a live parent.
        assert!(!process_exists(u32::MAX));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_live_parent_keeps_the_node_running() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let guard = OrphanGuard::watch(
            std::process::id(),
            &runtime,
            Arc::clone(&source),
            Duration::from_millis(10),
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!guard.has_fired());
        assert!(guard.poll_count() >= 1, "the watch is actually polling");
        assert!(source.try_next().is_none());
        assert_eq!(guard.parent(), std::process::id());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_parent_stops_the_node() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let _guard = OrphanGuard::watch(
            u32::MAX,
            &runtime,
            Arc::clone(&source),
            Duration::from_millis(10),
        );
        for _ in 0..200 {
            if let Some(event) = source.try_next() {
                let Event::Error(message) = event else {
                    panic!("expected the orphan report first");
                };
                assert!(message.contains("orphaned"), "{message}");
                let stop = source.try_next().unwrap();
                assert!(stop.is_stop());
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the guard never fired");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_the_guard_ends_the_watch() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let guard = OrphanGuard::watch(
            u32::MAX,
            &runtime,
            Arc::clone(&source),
            Duration::from_millis(500),
        );
        guard.stop();
        drop(guard);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(source.try_next().is_none(), "a stopped guard says nothing");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_parent_means_no_guard() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        assert!(OrphanGuard::maybe_watch(None, &runtime, source, DEFAULT_POLL_INTERVAL).is_none());
    }

    #[test]
    fn the_default_interval_is_documented() {
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_secs(2));
    }
}
