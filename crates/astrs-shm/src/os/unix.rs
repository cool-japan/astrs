//! The portable Unix half of the platform layer.
//!
//! `shm_open`/`shm_unlink`, `ftruncate`, `mmap`/`munmap`, `poll`, `kill(pid,
//! 0)` and the non-blocking flag are identical on Linux and the BSDs, so they
//! live here once. Only segment creation (memfd vs. shm), the doorbell
//! primitive (eventfd vs. pipe) and the process watch (pidfd vs. kqueue)
//! diverge, and those live in `linux.rs` / `bsd.rs`.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags, ftruncate};
use rustix::io::Errno;
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::shm;

use crate::error::{ShmError, ShmResult};
use crate::key::SegmentName;

/// Open (creating) a named POSIX shared-memory object.
///
/// A leftover object with the same name is a stale segment from a process
/// that died without unlinking. Because segment names embed the generation
/// (§6.2), a name collision can only mean *this exact incarnation* left
/// something behind, so the stale object is unlinked and the create retried
/// once. Retrying more than once would be a spin against another process
/// creating the same name, which cannot legitimately happen.
pub(crate) fn create_named(name: &SegmentName) -> ShmResult<OwnedFd> {
    let flags = shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR;
    let mode = Mode::RUSR | Mode::WUSR;
    match shm::open(name.as_str(), flags, mode) {
        Ok(fd) => Ok(fd),
        Err(Errno::EXIST) => {
            // Best-effort cleanup; if the unlink fails the retry reports the
            // original condition.
            let _ = shm::unlink(name.as_str());
            shm::open(name.as_str(), flags, mode).map_err(|errno| ShmError::os("shm_open", errno))
        }
        Err(errno) => Err(ShmError::os("shm_open", errno)),
    }
}

/// Open an existing named POSIX shared-memory object for read-write mapping.
pub(crate) fn open_named(name: &SegmentName) -> ShmResult<OwnedFd> {
    shm::open(name.as_str(), shm::OFlags::RDWR, Mode::empty())
        .map_err(|errno| ShmError::os("shm_open", errno))
}

/// Remove a named POSIX shared-memory object.
///
/// Existing mappings survive; the object simply loses its name, which is
/// exactly the "unlink after drain" behaviour §6.2 asks the daemon for.
pub(crate) fn unlink_named(name: &SegmentName) -> ShmResult<()> {
    match shm::unlink(name.as_str()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(errno) => Err(ShmError::os("shm_unlink", errno)),
    }
}

/// Size a segment's backing object.
///
/// Darwin allows `ftruncate` on a POSIX shared-memory object exactly once —
/// a second call fails with `EINVAL` — so this is only ever called at
/// creation, never to resize a live segment.
pub(crate) fn set_len(fd: BorrowedFd<'_>, len: u64) -> ShmResult<()> {
    ftruncate(fd, len).map_err(|errno| ShmError::os("ftruncate", errno))
}

/// The real size of a segment's backing object.
///
/// Consulted on every attach, because `mmap` happily maps *past* the end of
/// an object: the mapping succeeds and the first access beyond the object's
/// size raises `SIGBUS`. Comparing the header's declared geometry against the
/// object's actual size turns that latent crash into a typed
/// [`ShmError::CorruptHeader`] at attach time.
pub(crate) fn object_len(fd: BorrowedFd<'_>) -> ShmResult<u64> {
    let stat = rustix::fs::fstat(fd).map_err(|errno| ShmError::os("fstat", errno))?;
    u64::try_from(stat.st_size).map_err(|_| {
        ShmError::invalid_config(format!(
            "segment object reports a negative size {}",
            stat.st_size
        ))
    })
}

/// Map a segment shared and read-write.
///
/// See the module docs of [`crate::os`] for why consumers map read-write too.
///
/// # Errors
///
/// [`ShmError::Os`] if `mmap` fails, or [`ShmError::InvalidConfig`] if `len`
/// does not fit `usize` (a 32-bit host asked to map a >4 GiB segment).
pub(crate) fn map_shared(fd: BorrowedFd<'_>, len: u64) -> ShmResult<NonNull<u8>> {
    let len = usize::try_from(len).map_err(|_| {
        ShmError::invalid_config(format!(
            "segment of {len} bytes does not fit this address space"
        ))
    })?;
    // SAFETY: a fresh mapping at an address chosen by the kernel; it overlaps
    // nothing this process owns. The returned pointer is only used through
    // the validated layout, and is unmapped exactly once by `unmap`.
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            fd,
            0,
        )
    }
    .map_err(|errno| ShmError::os("mmap", errno))?;
    NonNull::new(ptr.cast::<u8>()).ok_or_else(|| ShmError::Os {
        operation: "mmap",
        source: std::io::Error::other("mmap returned a null address"),
    })
}

/// Unmap a segment.
///
/// # Safety
///
/// `ptr` and `len` must come from a matching [`map_shared`] call, and no
/// reference into the mapping may outlive this call.
pub(crate) unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
    // SAFETY: the caller guarantees the pointer/length pair names exactly one
    // live mapping produced by `map_shared`.
    let _ = unsafe { munmap(ptr.as_ptr().cast(), len) };
}

/// Put a descriptor into non-blocking mode.
pub(crate) fn set_nonblocking(fd: BorrowedFd<'_>) -> ShmResult<()> {
    let flags =
        rustix::fs::fcntl_getfl(fd).map_err(|errno| ShmError::os("fcntl(F_GETFL)", errno))?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)
        .map_err(|errno| ShmError::os("fcntl(F_SETFL)", errno))
}

/// Wait until `fd` is readable, or the timeout expires.
///
/// Returns `true` when the descriptor became readable (or reported an error
/// condition the caller should observe by reading), `false` on timeout.
/// `EINTR` is retried, so a signal does not shorten a wait.
pub(crate) fn wait_readable(fd: BorrowedFd<'_>, timeout: Option<Duration>) -> ShmResult<bool> {
    // A *deadline*, not a per-iteration budget. Restarting `poll` with the
    // original duration after every `EINTR` would make the wait unbounded in
    // a signal-heavy process — a supervising daemon reaping children is
    // exactly that — and callers that pass a timeout are entitled to have it
    // mean what it says.
    let deadline = timeout.map(|budget| Instant::now() + budget);
    loop {
        let remaining = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(false);
                }
                Some(deadline - now)
            }
            None => None,
        };
        let spec = remaining.map(to_timespec);
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, spec.as_ref()) {
            Ok(0) => return Ok(false),
            Ok(_) => {
                let revents = fds[0].revents();
                // HUP/ERR mean the peer is gone; report readable so the
                // caller performs its own drain and notices the condition.
                return Ok(revents.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR));
            }
            // Retry against the same deadline.
            Err(Errno::INTR) => {}
            Err(errno) => return Err(ShmError::os("poll", errno)),
        }
    }
}

/// This process's id.
#[must_use]
pub(crate) fn current_pid() -> i64 {
    i64::from(rustix::process::getpid().as_raw_nonzero().get())
}

/// Whether a process with this id exists.
///
/// `kill(pid, 0)`: `ESRCH` means gone, `EPERM` means alive but owned by
/// another user (which, for a dataflow's own children, means alive). Any
/// other error is treated as "alive" so a transient failure never causes a
/// spurious eviction — the conservative direction, since a false "dead" would
/// let the producer overwrite a live reader's cursor slot.
#[must_use]
pub(crate) fn process_alive(pid: i64) -> bool {
    let Some(pid) = checked_pid(pid) else {
        return false;
    };
    !matches!(rustix::process::test_kill_process(pid), Err(Errno::SRCH))
}

/// Narrow an `i64` pid to a `rustix::process::Pid`, rejecting the values that
/// are not process ids at all.
///
/// `Pid::from_raw` debug-asserts `raw >= 0` and rejects `0`, so every caller
/// must filter first — a negative pid means a process *group* in `kill(2)`
/// terms, and `0` means "this group"; neither is something this crate ever
/// watches, and passing one through would abort a debug build.
#[must_use]
pub(crate) fn checked_pid(pid: i64) -> Option<rustix::process::Pid> {
    let raw = i32::try_from(pid).ok()?;
    if raw <= 0 {
        return None;
    }
    rustix::process::Pid::from_raw(raw)
}

/// Convert a [`Duration`] into a `poll`-compatible [`Timespec`], saturating
/// rather than overflowing.
pub(crate) fn to_timespec(duration: Duration) -> Timespec {
    let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    Timespec {
        tv_sec: secs,
        tv_nsec: duration.subsec_nanos().into(),
    }
}

/// Read from a descriptor, mapping `EAGAIN` to `Ok(0)`.
pub(crate) fn read_nonblocking(fd: BorrowedFd<'_>, buffer: &mut [u8]) -> ShmResult<usize> {
    match rustix::io::read(fd, &mut *buffer) {
        Ok(read) => Ok(read),
        // `EAGAIN` and `EWOULDBLOCK` are the same value on every platform
        // this crate targets, so one arm covers both.
        Err(Errno::AGAIN) | Err(Errno::INTR) => Ok(0),
        Err(errno) => Err(ShmError::os("read", errno)),
    }
}

/// Write to a descriptor, treating a full buffer as success.
///
/// A doorbell whose buffer is full has already delivered the only signal it
/// carries — "there is something new" — so `EAGAIN` is not an error.
pub(crate) fn write_doorbell(fd: BorrowedFd<'_>, bytes: &[u8]) -> ShmResult<bool> {
    match rustix::io::write(fd, bytes) {
        Ok(_) => Ok(true),
        // `EAGAIN`/`EWOULDBLOCK`: the buffer is full, which already carries
        // the "something happened" signal. `EPIPE`: the reader end is gone —
        // the consumer detached or died, and there is nothing to wake.
        Err(Errno::AGAIN) | Err(Errno::INTR) | Err(Errno::PIPE) => Ok(false),
        Err(errno) => Err(ShmError::os("write", errno)),
    }
}

/// Duplicate a descriptor with `FD_CLOEXEC` set.
pub(crate) fn dup_cloexec(fd: BorrowedFd<'_>) -> ShmResult<OwnedFd> {
    let duplicate = rustix::io::dup(fd).map_err(|errno| ShmError::os("dup", errno))?;
    // `dup` does not carry `FD_CLOEXEC` across, and a duplicated segment or
    // doorbell descriptor leaking into an `exec`'d node would keep a segment
    // alive after every legitimate holder had dropped it.
    rustix::io::fcntl_setfd(duplicate.as_fd(), rustix::io::FdFlags::CLOEXEC)
        .map_err(|errno| ShmError::os("fcntl(F_SETFD)", errno))?;
    Ok(duplicate)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn timespec_conversion_is_exact_and_saturating() {
        let spec = to_timespec(Duration::from_millis(1500));
        assert_eq!(spec.tv_sec, 1);
        // `Nsecs` is `i64` on both P0 platforms but `c_long` in general, so
        // widen to `i128` to compare without assuming which.
        assert_eq!(i128::from(spec.tv_nsec), 500_000_000);

        let spec = to_timespec(Duration::from_secs(u64::MAX));
        assert_eq!(spec.tv_sec, i64::MAX);
    }

    #[test]
    fn the_current_process_is_alive_and_pid_zero_style_garbage_is_not() {
        let pid = current_pid();
        assert!(pid > 0);
        assert!(process_alive(pid));
        assert!(!process_alive(-1), "a negative pid is not a process");
        assert!(
            !process_alive(i64::from(i32::MAX) + 1),
            "a pid outside i32 cannot exist"
        );
    }

    #[test]
    fn named_segments_round_trip_through_the_filesystem_namespace() {
        // A name derived from this process's pid keeps concurrent test
        // binaries from colliding.
        let name = SegmentName::from_digest(u128::from(current_pid() as u64) ^ 0x5a5a_5a5a);
        let _ = unlink_named(&name);

        let fd = create_named(&name).expect("create");
        set_len(fd.as_fd(), 4096).expect("ftruncate");
        let mapping = map_shared(fd.as_fd(), 4096).expect("mmap");

        let reopened = open_named(&name).expect("reopen");
        let second = map_shared(reopened.as_fd(), 4096).expect("second mmap");

        // The two mappings alias the same object.
        // SAFETY: both mappings are 4096 bytes long and live.
        unsafe {
            mapping.as_ptr().write(0xab);
            assert_eq!(second.as_ptr().read(), 0xab);
            unmap(mapping, 4096);
            unmap(second, 4096);
        }

        unlink_named(&name).expect("unlink");
        assert!(
            open_named(&name).is_err(),
            "an unlinked name must not reopen"
        );
        // Unlinking twice is a no-op, not an error.
        unlink_named(&name).expect("second unlink");
    }

    #[test]
    fn creating_over_a_stale_name_succeeds() {
        let name = SegmentName::from_digest(u128::from(current_pid() as u64) ^ 0xa5a5_a5a5);
        let _ = unlink_named(&name);
        let first = create_named(&name).expect("create");
        set_len(first.as_fd(), 128).expect("ftruncate");
        // Simulate a crashed producer that never unlinked: create again.
        let second = create_named(&name).expect("create over a stale name");
        set_len(second.as_fd(), 128).expect("ftruncate");
        unlink_named(&name).expect("unlink");
    }

    #[test]
    fn wait_readable_times_out_on_a_quiet_descriptor() {
        let (reader, writer) = crate::os::doorbell_pair().expect("doorbell");
        let elapsed = std::time::Instant::now();
        assert!(!wait_readable(reader.as_fd(), Some(Duration::from_millis(30))).unwrap());
        assert!(elapsed.elapsed() >= Duration::from_millis(20));

        crate::os::doorbell_ring(writer.as_fd()).expect("ring");
        assert!(wait_readable(reader.as_fd(), Some(Duration::from_millis(500))).unwrap());
    }

    #[test]
    fn duplicated_descriptors_alias_the_same_doorbell() {
        let (reader, writer) = crate::os::doorbell_pair().expect("doorbell");
        let cloned = dup_cloexec(writer.as_fd()).expect("dup");
        crate::os::doorbell_ring(cloned.as_fd()).expect("ring through the clone");
        assert!(wait_readable(reader.as_fd(), Some(Duration::from_millis(500))).unwrap());
        assert!(crate::os::doorbell_drain(reader.as_fd()).expect("drain") > 0);
    }
}
