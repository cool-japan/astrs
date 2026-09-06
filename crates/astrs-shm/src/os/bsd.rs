//! The macOS / BSD half of the platform layer: pipes for doorbells, `kqueue`
//! `EVFILT_PROC` for producer liveness, and named `shm_open` segments.
//!
//! Blueprint §23 risk #7 ("macOS SHM limits: name length, no `/dev/shm`") is
//! answered here and in [`crate::key`]: there is no `memfd`, so every segment
//! is named, and every name is the 30-byte hash form that fits Darwin's
//! `PSHMNAMLEN`.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use rustix::event::kqueue::{Event, EventFilter, EventFlags, ProcessEvents, kevent, kqueue};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::pipe::pipe;

use crate::config::Backing;
use crate::error::{ShmError, ShmResult};
use crate::os::unix::{
    checked_pid, process_alive, read_nonblocking, set_nonblocking, wait_readable, write_doorbell,
};

/// The backing [`crate::Segment`] creation resolves [`Backing::Auto`] to.
///
/// There is no `memfd_create` outside Linux, so every segment is a named
/// POSIX shared-memory object.
pub(crate) const AUTO_BACKING: Backing = Backing::Named;

/// Anonymous segments do not exist on this platform.
///
/// # Errors
///
/// Always [`ShmError::Unsupported`]. Callers reach this only by asking for
/// [`Backing::Memfd`] explicitly; [`Backing::Auto`] resolves to
/// [`Backing::Named`] here.
pub(crate) fn create_anonymous(_label: &str) -> ShmResult<OwnedFd> {
    Err(ShmError::unsupported("memfd_create"))
}

/// No-op: POSIX shared memory has no sealing analogue.
///
/// A named object *can* be truncated by anyone who can open it, which is why
/// the segment name is mode `0600` and unguessable (a 120-bit hash).
pub(crate) fn seal_anonymous(_fd: BorrowedFd<'_>) {}

/// Create a doorbell: a non-blocking pipe.
///
/// A pipe rather than an `eventfd` (which Darwin lacks). The payload is a
/// single byte per ring; the count matters only in that "nonzero" means
/// "something happened", and a full pipe already conveys that.
///
/// Darwin has no `pipe2`, so the close-on-exec and non-blocking flags are
/// applied afterwards with `fcntl` rather than at creation. The window in
/// between matters only if another thread `exec`s concurrently — which the
/// daemon's spawner never does while a route is being built (§6.3 creates the
/// segment and its doorbells before any child exists).
pub(crate) fn doorbell_pair() -> ShmResult<(OwnedFd, OwnedFd)> {
    let (read_end, write_end) = pipe().map_err(|errno| ShmError::os("pipe", errno))?;
    for end in [read_end.as_fd(), write_end.as_fd()] {
        fcntl_setfd(end, FdFlags::CLOEXEC)
            .map_err(|errno| ShmError::os("fcntl(F_SETFD)", errno))?;
        set_nonblocking(end)?;
    }
    Ok((read_end, write_end))
}

/// Ring a doorbell.
///
/// Returns `false` when the pipe is full or the reader is gone — neither is
/// an error: a full pipe already carries the wakeup, and a gone reader has
/// nothing to wake.
pub(crate) fn doorbell_ring(fd: BorrowedFd<'_>) -> ShmResult<bool> {
    write_doorbell(fd, &[1u8])
}

/// Drain a doorbell, returning how many bytes were consumed.
///
/// Drains in 64-byte gulps until the pipe is empty, so a burst of commits
/// costs one wakeup rather than one wakeup per commit.
pub(crate) fn doorbell_drain(fd: BorrowedFd<'_>) -> ShmResult<u64> {
    let mut total = 0u64;
    let mut buffer = [0u8; 64];
    loop {
        let read = read_nonblocking(fd, &mut buffer)?;
        if read == 0 {
            return Ok(total);
        }
        total += read as u64;
        if read < buffer.len() {
            return Ok(total);
        }
    }
}

/// A handle that becomes readable when a process exits.
///
/// Backed by a `kqueue` registered for `EVFILT_PROC` / `NOTE_EXIT`. Darwin
/// only allows that for processes the caller may signal, so registration can
/// fail (`ESRCH`, `EPERM`); when it does, the handle degrades to interval
/// polling of `kill(pid, 0)` — the portable fallback the task calls for, and
/// the reason [`ProcessWatchImpl::is_event_driven`] exists as an observable
/// property rather than an assumption.
#[derive(Debug)]
pub(crate) struct ProcessWatchImpl {
    pid: i64,
    fd: Option<OwnedFd>,
}

impl ProcessWatchImpl {
    /// Start watching `pid`.
    pub(crate) fn new(pid: i64) -> Self {
        Self {
            pid,
            fd: register_proc_exit(pid),
        }
    }

    /// The watched pid.
    pub(crate) const fn pid(&self) -> i64 {
        self.pid
    }

    /// The pollable descriptor, when registration succeeded.
    pub(crate) fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        self.fd.as_ref().map(AsFd::as_fd)
    }

    /// Whether the watch is backed by a kernel notification rather than
    /// polling.
    pub(crate) const fn is_event_driven(&self) -> bool {
        self.fd.is_some()
    }

    /// Wait until the process exits, or the timeout expires.
    ///
    /// Returns `true` when the process is gone. A `kqueue` descriptor is
    /// itself pollable, so the wait is a plain `poll` either way — which
    /// keeps the daemon's supervision loop identical on both platforms.
    pub(crate) fn wait(
        &self,
        timeout: Option<Duration>,
        poll_interval: Duration,
    ) -> ShmResult<bool> {
        match self.as_fd() {
            Some(fd) => wait_readable(fd, timeout),
            None => Ok(crate::liveness::poll_until_gone(
                self.pid,
                timeout,
                poll_interval,
            )),
        }
    }

    /// A non-blocking liveness check.
    pub(crate) fn is_alive(&self) -> bool {
        process_alive(self.pid)
    }
}

/// Register a `NOTE_EXIT` watch, returning the `kqueue` descriptor.
///
/// `None` means the caller must fall back to polling.
fn register_proc_exit(pid: i64) -> Option<OwnedFd> {
    let pid = checked_pid(pid)?;
    let queue = kqueue().ok()?;
    let change = Event::new(
        EventFilter::Proc {
            pid,
            flags: ProcessEvents::EXIT,
        },
        // ONESHOT: the process exits once; leaving the filter armed would
        // keep reporting an event that has already been consumed and turn
        // every subsequent `poll` into a busy loop.
        EventFlags::ADD | EventFlags::ENABLE | EventFlags::ONESHOT,
        std::ptr::null_mut(),
    );
    let mut sink: [std::mem::MaybeUninit<Event>; 1] = [std::mem::MaybeUninit::uninit()];
    // SAFETY: `change` is a well-formed event built by rustix's constructor,
    // `sink` is a correctly sized uninitialised buffer, and a zero timeout
    // makes the call non-blocking.
    let registered = unsafe {
        kevent(
            &queue,
            std::slice::from_ref(&change),
            &mut sink,
            Some(Duration::ZERO),
        )
    };
    match registered {
        // A returned event here means the registration itself reported an
        // error (EV_ERROR) — typically the process is already gone. Fall back
        // to polling, which will observe the same thing immediately.
        Ok((reported, _)) if !reported.is_empty() => {
            if reported[0].flags().contains(EventFlags::ERROR) {
                None
            } else {
                Some(queue)
            }
        }
        Ok(_) => Some(queue),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::os::unix::current_pid;

    #[test]
    fn anonymous_backing_is_refused_with_a_typed_error() {
        assert!(matches!(
            create_anonymous("x"),
            Err(ShmError::Unsupported { .. })
        ));
    }

    #[test]
    fn the_doorbell_counts_rings() {
        let (reader, writer) = doorbell_pair().expect("pipe");
        assert_eq!(doorbell_drain(reader.as_fd()).unwrap(), 0);
        for _ in 0..3 {
            assert!(doorbell_ring(writer.as_fd()).unwrap());
        }
        assert_eq!(doorbell_drain(reader.as_fd()).unwrap(), 3);
        assert_eq!(doorbell_drain(reader.as_fd()).unwrap(), 0);
    }

    #[test]
    fn a_full_doorbell_is_not_an_error() {
        let (reader, writer) = doorbell_pair().expect("pipe");
        // Fill the pipe buffer; the ring must degrade to `false`, never fail.
        let mut rang = 0u64;
        for _ in 0..1_000_000 {
            match doorbell_ring(writer.as_fd()) {
                Ok(true) => rang += 1,
                Ok(false) => break,
                Err(err) => panic!("ringing a full doorbell must not fail: {err}"),
            }
        }
        assert!(rang > 0);
        assert!(doorbell_drain(reader.as_fd()).unwrap() > 0);
    }

    #[test]
    fn watching_our_own_process_never_reports_an_exit() {
        let watch = ProcessWatchImpl::new(current_pid());
        assert_eq!(watch.pid(), current_pid());
        assert!(watch.is_alive());
        assert!(
            !watch
                .wait(Some(Duration::from_millis(20)), Duration::from_millis(5))
                .unwrap()
        );
    }

    #[test]
    fn watching_a_nonexistent_pid_degrades_to_polling_and_reports_gone() {
        // Pid 1 exists; a pid far above the system maximum does not.
        let watch = ProcessWatchImpl::new(i64::from(i32::MAX) - 1);
        assert!(!watch.is_alive());
        assert!(
            watch
                .wait(Some(Duration::from_millis(50)), Duration::from_millis(5))
                .unwrap()
        );
    }
}
