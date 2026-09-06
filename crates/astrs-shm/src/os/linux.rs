//! Linux-only syscalls: `memfd_create`, `eventfd`, `pidfd_open`.
//!
//! Everything here has a portable counterpart in `bsd.rs`; the two files are
//! kept deliberately thin so the platform that CI cannot always exercise has
//! the smallest possible surface to rot.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use rustix::event::{EventfdFlags, eventfd};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
use rustix::io::Errno;
use rustix::process::{PidfdFlags, pidfd_open};

use crate::config::Backing;
use crate::error::{ShmError, ShmResult};
use crate::os::unix::{
    checked_pid, dup_cloexec, process_alive, read_nonblocking, set_nonblocking, wait_readable,
    write_doorbell,
};

/// The backing [`crate::Segment`] creation resolves [`Backing::Auto`] to.
///
/// `memfd` on Linux: an anonymous object has no filesystem presence to leak
/// when every process in a dataflow dies at once, and the daemon brokers the
/// descriptors anyway (§6.3).
pub(crate) const AUTO_BACKING: Backing = Backing::Memfd;

/// Create an anonymous shared-memory object.
///
/// The label is purely diagnostic — it shows up as `/memfd:<label>` in
/// `/proc/<pid>/maps`, which is what makes a leaked mapping traceable back to
/// its dataflow.
///
/// Shrink and grow seals are applied so no peer holding the descriptor can
/// resize the object under a live mapping; without them, a truncation would
/// turn every consumer's next read into a `SIGBUS`.
pub(crate) fn create_anonymous(label: &str) -> ShmResult<OwnedFd> {
    let fd = memfd_create(label, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
        .map_err(|errno| ShmError::os("memfd_create", errno))?;
    Ok(fd)
}

/// Seal an anonymous segment against resizing, after it has been sized.
///
/// Best-effort: a kernel or filesystem that refuses sealing costs hardening,
/// not correctness, so the failure is swallowed rather than aborting a route.
pub(crate) fn seal_anonymous(fd: BorrowedFd<'_>) {
    let _ = fcntl_add_seals(fd, SealFlags::SHRINK | SealFlags::GROW);
}

/// Create a doorbell: an `eventfd` and a duplicate of it.
///
/// A single counting `eventfd` is both ends, so the "write end" handed to a
/// producer is just a `dup`. Non-blocking on both ends: a producer must never
/// stall in `write` because a consumer stopped draining, and a consumer's
/// drain must never block when the counter is already zero.
pub(crate) fn doorbell_pair() -> ShmResult<(OwnedFd, OwnedFd)> {
    let read_end = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
        .map_err(|errno| ShmError::os("eventfd", errno))?;
    let write_end = dup_cloexec(read_end.as_fd())?;
    set_nonblocking(write_end.as_fd())?;
    Ok((read_end, write_end))
}

/// Ring a doorbell.
///
/// Returns `false` when the counter is saturated — which already means "there
/// is something new", so the signal is not lost.
pub(crate) fn doorbell_ring(fd: BorrowedFd<'_>) -> ShmResult<bool> {
    write_doorbell(fd, &1u64.to_ne_bytes())
}

/// Drain a doorbell, returning how many rings were consumed.
pub(crate) fn doorbell_drain(fd: BorrowedFd<'_>) -> ShmResult<u64> {
    let mut buffer = [0u8; 8];
    let read = read_nonblocking(fd, &mut buffer)?;
    if read == 8 {
        Ok(u64::from_ne_bytes(buffer))
    } else {
        Ok(0)
    }
}

/// A handle that becomes readable when a process exits.
///
/// Backed by `pidfd_open` where the kernel provides it (5.3+, and the caller
/// must be able to signal the target). Where it does not, the handle degrades
/// to interval polling of `kill(pid, 0)` — see [`ProcessWatchImpl::wait`].
#[derive(Debug)]
pub(crate) struct ProcessWatchImpl {
    pid: i64,
    fd: Option<OwnedFd>,
}

impl ProcessWatchImpl {
    /// Start watching `pid`.
    ///
    /// Never fails: a process that cannot be watched with a `pidfd` is
    /// watched by polling instead, because "the daemon cannot supervise this
    /// producer" is not an acceptable outcome for a route.
    pub(crate) fn new(pid: i64) -> Self {
        let fd = checked_pid(pid).and_then(|pid| pidfd_open(pid, PidfdFlags::NONBLOCK).ok());
        Self { pid, fd }
    }

    /// The watched pid.
    pub(crate) const fn pid(&self) -> i64 {
        self.pid
    }

    /// The pollable descriptor, when the platform provided one.
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
    /// Returns `true` when the process is gone.
    pub(crate) fn wait(
        &self,
        timeout: Option<Duration>,
        poll_interval: Duration,
    ) -> ShmResult<bool> {
        match self.as_fd() {
            Some(fd) => match wait_readable(fd, timeout) {
                Ok(ready) => Ok(ready),
                // A pidfd for an already-reaped process can report this;
                // treat it as "gone" rather than propagating.
                Err(ShmError::Os { source, .. })
                    if source.raw_os_error() == Some(Errno::BADF.raw_os_error()) =>
                {
                    Ok(true)
                }
                Err(err) => Err(err),
            },
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::os::unix::current_pid;

    #[test]
    fn anonymous_segments_are_creatable_and_sealable() {
        let fd = create_anonymous("astrs-shm-test").expect("memfd_create");
        crate::os::unix::set_len(fd.as_fd(), 8192).expect("ftruncate");
        seal_anonymous(fd.as_fd());
        let mapping = crate::os::unix::map_shared(fd.as_fd(), 8192).expect("mmap");
        // SAFETY: the mapping is 8192 bytes and live.
        unsafe { crate::os::unix::unmap(mapping, 8192) };
    }

    #[test]
    fn the_doorbell_counts_rings() {
        let (reader, writer) = doorbell_pair().expect("eventfd");
        assert_eq!(doorbell_drain(reader.as_fd()).unwrap(), 0);
        for _ in 0..3 {
            assert!(doorbell_ring(writer.as_fd()).unwrap());
        }
        assert_eq!(doorbell_drain(reader.as_fd()).unwrap(), 3);
        assert_eq!(doorbell_drain(reader.as_fd()).unwrap(), 0);
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
}
