//! Doorbells: how a consumer blocks without spinning, and how a producer
//! wakes it.
//!
//! Blueprint §6.2 asks for a "semaphore-free design (futex-less portable
//! seqlock via atomics + eventfd/kqueue wakeups through the UDS channel for
//! blocking receives)". The atomics are [`crate::slot`]; this module is the
//! wakeup half.
//!
//! # One doorbell per consumer, not one per segment
//!
//! A single shared `eventfd` cannot serve an SPMC ring: reading it consumes
//! the count, so exactly one waiter would wake per commit and the rest would
//! sleep through their own messages. So **each consumer owns its own
//! doorbell** and hands the producer a duplicate of the write end:
//!
//! - in-process, [`crate::Consumer::attach`] registers the ringer with the
//!   segment's [`DoorbellRegistry`] automatically;
//! - across processes, the consumer sends the write end to the producer's
//!   daemon over the control socket with `SCM_RIGHTS`
//!   ([`crate::fdpass`]), which registers it in the same registry.
//!
//! Both paths converge on one list, so [`crate::Producer::commit`] rings
//! every registered consumer with one loop and no knowledge of who is local.
//!
//! # Hot-path cost
//!
//! Ringing must not take a lock. The producer keeps a private snapshot of the
//! ringer list and re-reads the shared list only when the registry's
//! generation counter changes — one Relaxed load per commit in the steady
//! state, and a lock only when a consumer attaches or detaches.
//!
//! # Wait / drain ordering
//!
//! The correct sequence is **check the ring → block on the fd → drain the fd
//! → check the ring again**. Draining before the final re-check would lose a
//! ring that arrived between the check and the drain; the extra re-check
//! costs one atomic load and removes the lost-wakeup class entirely.
//!
//! # Examples
//!
//! ```
//! # #[cfg(unix)] {
//! use astrs_shm::Doorbell;
//! use std::time::Duration;
//!
//! let doorbell = Doorbell::new()?;
//! let ringer = doorbell.ringer()?;
//!
//! // Nothing has rung: the wait times out rather than spinning.
//! assert!(!doorbell.wait(Some(Duration::from_millis(10)))?);
//!
//! ringer.ring()?;
//! assert!(doorbell.wait(Some(Duration::from_millis(500)))?);
//! assert!(doorbell.drain()? > 0);
//! # }
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{ShmError, ShmResult};
use crate::os;

/// The read end of a consumer's wakeup channel.
///
/// `eventfd` on Linux, a pipe elsewhere. Always non-blocking, so a drain
/// never stalls and a producer's ring never blocks on a consumer that stopped
/// reading.
#[derive(Debug)]
pub struct Doorbell {
    read: OwnedFd,
    write: OwnedFd,
    async_fd: Mutex<Option<Arc<tokio::io::unix::AsyncFd<OwnedFd>>>>,
}

impl Doorbell {
    /// Create a doorbell.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if the descriptors cannot be created, or
    /// [`ShmError::Unsupported`] on a platform without a doorbell primitive.
    pub fn new() -> ShmResult<Self> {
        let (read, write) = os::doorbell_pair()?;
        Ok(Self {
            read,
            write,
            async_fd: Mutex::new(None),
        })
    }

    /// Duplicate the write end for a producer to keep.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if `dup` fails.
    pub fn ringer(&self) -> ShmResult<DoorbellRinger> {
        Ok(DoorbellRinger {
            fd: os::dup_cloexec(self.write.as_fd())?,
        })
    }

    /// Ring this doorbell from the owning side.
    ///
    /// Used to break a consumer out of a blocking wait at shutdown, without
    /// a separate control channel.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] for an unexpected write failure; a full buffer is
    /// reported as `Ok(false)`, not an error.
    pub fn self_ring(&self) -> ShmResult<bool> {
        os::doorbell_ring(self.write.as_fd())
    }

    /// Consume every pending ring, returning how many were pending.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] for an unexpected read failure.
    pub fn drain(&self) -> ShmResult<u64> {
        os::doorbell_drain(self.read.as_fd())
    }

    /// Block until the doorbell rings, or the timeout expires.
    ///
    /// Returns `true` when a ring is pending. Does **not** drain — the caller
    /// re-checks the ring first, then drains, so a ring that arrives during
    /// the re-check is not swallowed.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if `poll` fails for a reason other than `EINTR`.
    pub fn wait(&self, timeout: Option<Duration>) -> ShmResult<bool> {
        os::wait_readable(self.read.as_fd(), timeout)
    }

    /// Await a ring on a tokio runtime.
    ///
    /// The [`tokio::io::unix::AsyncFd`] is created lazily on first use,
    /// because constructing one requires a running reactor and a
    /// [`Doorbell`] is routinely created on a plain thread.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if the descriptor cannot be registered with the
    /// reactor, or if the readiness wait fails.
    pub async fn wait_async(&self) -> ShmResult<()> {
        let async_fd = self.async_fd()?;
        let mut guard = async_fd
            .readable()
            .await
            .map_err(|err| ShmError::io("AsyncFd::readable", err))?;
        // Clearing readiness here (rather than after the caller's re-check)
        // is correct because the drain that follows is level-triggered
        // against the descriptor itself: if bytes remain, the next
        // `readable()` returns immediately.
        guard.clear_ready();
        Ok(())
    }

    fn async_fd(&self) -> ShmResult<Arc<tokio::io::unix::AsyncFd<OwnedFd>>> {
        let mut slot = self
            .async_fd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(existing));
        }
        let duplicate = os::dup_cloexec(self.read.as_fd())?;
        let registered = tokio::io::unix::AsyncFd::new(duplicate)
            .map_err(|err| ShmError::io("AsyncFd::new", err))?;
        let shared = Arc::new(registered);
        *slot = Some(Arc::clone(&shared));
        Ok(shared)
    }

    /// The pollable read end, for callers integrating with their own event
    /// loop.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }
}

/// The producer's handle to one consumer's doorbell.
#[derive(Debug)]
pub struct DoorbellRinger {
    fd: OwnedFd,
}

impl DoorbellRinger {
    /// Adopt a write end received over a socket.
    #[must_use]
    pub const fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Surrender the descriptor — what a broker sends over `SCM_RIGHTS`.
    #[must_use]
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }

    /// Borrow the descriptor.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Wake the consumer.
    ///
    /// Returns `false` when the wakeup was coalesced into a pending one, or
    /// when the reader is gone — neither is a failure.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] for an unexpected write failure.
    pub fn ring(&self) -> ShmResult<bool> {
        os::doorbell_ring(self.fd.as_fd())
    }
}

/// One registered consumer doorbell.
#[derive(Debug, Clone)]
struct Registered {
    index: u32,
    token: u32,
    ringer: Arc<DoorbellRinger>,
}

/// The set of doorbells a producer must ring on commit.
///
/// Lives in the [`crate::Segment`] (process-locally — descriptors cannot live
/// in shared memory), so in-process consumers register themselves simply by
/// attaching, and cross-process consumers are registered by the daemon when
/// their descriptor arrives.
#[derive(Debug, Default)]
pub struct DoorbellRegistry {
    entries: Mutex<Vec<Registered>>,
    generation: AtomicU64,
}

impl DoorbellRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the doorbell for one consumer-table entry.
    ///
    /// Replacing is the reattach case: a consumer that detached and came back
    /// on the same table index gets a fresh token, and the stale ringer must
    /// not survive.
    pub fn register(&self, index: u32, token: u32, ringer: DoorbellRinger) {
        let mut entries = self.lock();
        entries.retain(|entry| entry.index != index);
        entries.push(Registered {
            index,
            token,
            ringer: Arc::new(ringer),
        });
        drop(entries);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Remove a consumer's doorbell.
    ///
    /// Returns `true` if an entry was removed.
    pub fn unregister(&self, index: u32) -> bool {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|entry| entry.index != index);
        let removed = entries.len() != before;
        drop(entries);
        if removed {
            self.generation.fetch_add(1, Ordering::Release);
        }
        removed
    }

    /// Duplicate every registered ringer.
    ///
    /// The broker replays these to a producer that claims a segment *after*
    /// its consumers have already attached — the ordinary case, since §6.3
    /// has the daemon confirm consumer attachment before it upgrades the
    /// route. Without a replay, every doorbell registered during that window
    /// would be silently lost and the consumers would fall back to polling.
    ///
    /// A descriptor that cannot be duplicated is skipped rather than
    /// failing the replay: the consumer it belonged to has gone, and the
    /// worst case is one fewer wakeup.
    #[must_use]
    pub fn dup_all(&self) -> Vec<(u32, u32, OwnedFd)> {
        self.lock()
            .iter()
            .filter_map(|entry| {
                os::dup_cloexec(entry.ringer.as_fd())
                    .ok()
                    .map(|fd| (entry.index, entry.token, fd))
            })
            .collect()
    }

    /// The drop token a registered doorbell was filed under.
    ///
    /// The daemon uses this to tell "this consumer is still the one I wired"
    /// from "the table entry was recycled and the ringer I hold is stale".
    #[must_use]
    pub fn registered_token(&self, index: u32) -> Option<u32> {
        self.lock()
            .iter()
            .find(|entry| entry.index == index)
            .map(|entry| entry.token)
    }

    /// How many doorbells are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no doorbell is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The registry's change counter.
    ///
    /// Relaxed is enough for the hot-path comparison: a producer that misses
    /// a bump by one commit rings the old set once more, and the consumer it
    /// missed re-checks the ring anyway on its next loop.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Registered>> {
        // A panic inside the registry's critical section cannot corrupt the
        // invariant (it is a plain `Vec` of descriptors), so recovering from
        // poisoning is strictly better than propagating a panic into a
        // producer's commit path.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A producer's private, lock-free view of the doorbell set.
///
/// Refreshed only when [`DoorbellRegistry::generation`] changes.
#[derive(Debug, Default)]
pub struct DoorbellFanout {
    generation: u64,
    ringers: Vec<Arc<DoorbellRinger>>,
    initialized: bool,
}

impl DoorbellFanout {
    /// An empty fanout, guaranteed to refresh on first use.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many doorbells the last refresh saw.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ringers.len()
    }

    /// Whether the last refresh saw no doorbells.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ringers.is_empty()
    }

    /// Ring every registered doorbell, refreshing the snapshot if the
    /// registry changed.
    ///
    /// Returns how many rings were delivered (a coalesced ring counts as
    /// delivered, since the signal it carries is already pending).
    ///
    /// # Errors
    ///
    /// Never fails the commit: a per-descriptor error is swallowed, because
    /// a consumer whose doorbell has gone bad must not be able to stop the
    /// producer. The count reflects only the successful rings.
    pub fn ring_all(&mut self, registry: &DoorbellRegistry) -> usize {
        let generation = registry.generation();
        if !self.initialized || generation != self.generation {
            self.refresh(registry, generation);
        }
        let mut rung = 0;
        for ringer in &self.ringers {
            if ringer.ring().is_ok() {
                rung += 1;
            }
        }
        rung
    }

    fn refresh(&mut self, registry: &DoorbellRegistry, generation: u64) {
        let entries = registry.lock();
        self.ringers.clear();
        self.ringers
            .extend(entries.iter().map(|entry| Arc::clone(&entry.ringer)));
        drop(entries);
        self.generation = generation;
        self.initialized = true;
    }

    /// The consumer-table tokens the snapshot was built from, for tests.
    #[must_use]
    pub fn snapshot_generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_quiet_doorbell_times_out() {
        let doorbell = Doorbell::new().unwrap();
        let started = std::time::Instant::now();
        assert!(!doorbell.wait(Some(Duration::from_millis(25))).unwrap());
        assert!(started.elapsed() >= Duration::from_millis(15));
        assert_eq!(doorbell.drain().unwrap(), 0);
    }

    #[test]
    fn ringing_wakes_the_waiter_and_draining_clears_it() {
        let doorbell = Doorbell::new().unwrap();
        let ringer = doorbell.ringer().unwrap();
        assert!(ringer.ring().unwrap());
        assert!(doorbell.wait(Some(Duration::from_secs(1))).unwrap());
        assert!(doorbell.drain().unwrap() > 0);
        assert!(!doorbell.wait(Some(Duration::from_millis(10))).unwrap());
    }

    #[test]
    fn a_doorbell_can_wake_itself() {
        let doorbell = Doorbell::new().unwrap();
        assert!(doorbell.self_ring().unwrap());
        assert!(doorbell.wait(Some(Duration::from_secs(1))).unwrap());
    }

    #[test]
    fn ringing_from_another_thread_unblocks_a_waiter() {
        let doorbell = Doorbell::new().unwrap();
        let ringer = doorbell.ringer().unwrap();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            ringer.ring()
        });
        assert!(doorbell.wait(Some(Duration::from_secs(2))).unwrap());
        handle.join().expect("ringer thread").expect("ring");
    }

    #[test]
    fn a_ringer_survives_a_round_trip_through_a_raw_descriptor() {
        let doorbell = Doorbell::new().unwrap();
        let ringer = doorbell.ringer().unwrap();
        let raw = ringer.into_fd();
        let adopted = DoorbellRinger::from_fd(raw);
        assert!(adopted.ring().unwrap());
        assert!(doorbell.wait(Some(Duration::from_secs(1))).unwrap());
        let _ = adopted.as_fd();
    }

    #[test]
    fn the_registry_tracks_generations_and_replaces_by_index() {
        let registry = DoorbellRegistry::new();
        assert!(registry.is_empty());
        let first = Doorbell::new().unwrap();
        let second = Doorbell::new().unwrap();

        let generation = registry.generation();
        registry.register(0, 1, first.ringer().unwrap());
        assert_eq!(registry.len(), 1);
        assert!(registry.generation() > generation);

        // Re-registering the same index replaces rather than duplicates.
        registry.register(0, 2, second.ringer().unwrap());
        assert_eq!(registry.len(), 1);

        registry.register(1, 1, first.ringer().unwrap());
        assert_eq!(registry.len(), 2);

        assert!(registry.unregister(1));
        assert!(!registry.unregister(1));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn the_fanout_refreshes_only_when_the_registry_changes() {
        let registry = DoorbellRegistry::new();
        let mut fanout = DoorbellFanout::new();
        assert!(fanout.is_empty());
        assert_eq!(fanout.ring_all(&registry), 0);

        let doorbell = Doorbell::new().unwrap();
        registry.register(0, 1, doorbell.ringer().unwrap());
        assert_eq!(fanout.ring_all(&registry), 1);
        assert_eq!(fanout.len(), 1);
        let generation = fanout.snapshot_generation();

        // No change: the snapshot is reused.
        assert_eq!(fanout.ring_all(&registry), 1);
        assert_eq!(fanout.snapshot_generation(), generation);

        registry.unregister(0);
        assert_eq!(fanout.ring_all(&registry), 0);
        assert!(fanout.snapshot_generation() > generation);
        assert!(doorbell.drain().unwrap() > 0);
    }

    #[test]
    fn a_fanout_reaches_every_registered_consumer() {
        let registry = DoorbellRegistry::new();
        let doorbells: Vec<Doorbell> = (0..4).map(|_| Doorbell::new().unwrap()).collect();
        for (index, doorbell) in doorbells.iter().enumerate() {
            registry.register(u32::try_from(index).unwrap(), 1, doorbell.ringer().unwrap());
        }
        let mut fanout = DoorbellFanout::new();
        assert_eq!(fanout.ring_all(&registry), 4);
        for doorbell in &doorbells {
            assert!(
                doorbell.wait(Some(Duration::from_millis(200))).unwrap(),
                "every registered consumer must be woken"
            );
        }
    }

    #[tokio::test]
    async fn the_async_wait_resolves_when_the_doorbell_rings() {
        let doorbell = Doorbell::new().unwrap();
        let ringer = doorbell.ringer().unwrap();
        let waiter = tokio::time::timeout(Duration::from_secs(2), doorbell.wait_async());
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(20));
            ringer.ring()
        });
        waiter
            .await
            .expect("the async wait must not time out")
            .expect("readiness");
        assert!(doorbell.drain().unwrap() > 0);
    }

    #[tokio::test]
    async fn repeated_async_waits_reuse_one_registration() {
        let doorbell = Doorbell::new().unwrap();
        let ringer = doorbell.ringer().unwrap();
        for _ in 0..3 {
            ringer.ring().unwrap();
            tokio::time::timeout(Duration::from_secs(2), doorbell.wait_async())
                .await
                .expect("no timeout")
                .expect("readiness");
            doorbell.drain().unwrap();
        }
    }
}
