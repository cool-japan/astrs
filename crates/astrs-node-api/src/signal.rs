//! [`Signal`] — one wakeup that both a blocking thread and an async task can
//! wait on.
//!
//! `EventStream` has to serve two callers with one queue: the ordinary node
//! `while let Some(event) = events.recv()` loop, which is a plain blocking
//! thread, and `events.recv_async().await` inside somebody's tokio task
//! (blueprint §9.1 requires both). A [`tokio::sync::Notify`] cannot wake a
//! blocking thread and a [`Condvar`] cannot wake a task, so the producer side
//! signals *both* and each waiter uses the face that fits.
//!
//! # The ticket
//!
//! A condition variable has no memory: a notification delivered before a
//! waiter sleeps is lost. The counter in [`Signal::ticket`] is that memory —
//! a waiter reads it *before* it checks the queue, and
//! [`Signal::wait_blocking`] sleeps only while the counter still equals what
//! was read. A `notify` that lands in between bumps the counter, so the sleep
//! is skipped rather than missed. The async side gets the same guarantee from
//! `Notify`'s documented "create the future before checking the condition"
//! pattern, which [`Signal::notified`] exposes.
//!
//! # Closing
//!
//! [`Signal::close`] is terminal and idempotent: it wakes everybody and every
//! later wait returns immediately. That is what makes an event stream fuse
//! after `Stop` (§9.1) without a separate flag that could disagree with the
//! queue.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::signal::{Signal, WaitOutcome};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let signal = Arc::new(Signal::new());
//! let ticket = signal.ticket();
//!
//! let producer = Arc::clone(&signal);
//! std::thread::spawn(move || producer.notify());
//!
//! assert_eq!(
//!     signal.wait_blocking(ticket, Some(Duration::from_secs(5))),
//!     WaitOutcome::Signalled
//! );
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::sync::futures::Notified;

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WaitOutcome {
    /// The signal fired (or had already fired since the ticket was taken).
    Signalled,
    /// The signal was closed; no further wakeups will ever come.
    Closed,
    /// The deadline passed with no signal.
    TimedOut,
}

impl WaitOutcome {
    /// Whether waiting again could still produce something.
    #[must_use]
    pub const fn may_continue(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

/// The mutex-guarded half: a monotone counter the condition variable waits on.
///
/// Kept behind a `Mutex` (rather than only in the atomic) because
/// `Condvar::wait_timeout` requires a guard, and the guard is what closes the
/// lost-wakeup window between "check the counter" and "go to sleep".
#[derive(Debug, Default)]
struct Ticket {
    /// Bumped by every [`Signal::notify`].
    value: u64,
}

/// A wakeup with a blocking face and an async face.
#[derive(Debug)]
pub struct Signal {
    /// The counter the blocking waiters compare against.
    ticket: Mutex<Ticket>,
    /// Wakes the blocking waiters.
    condvar: Condvar,
    /// Wakes the async waiters.
    notify: Notify,
    /// A lock-free mirror of the counter, so a waiter can take a ticket
    /// without contending with a notifier that is mid-broadcast.
    ticket_gauge: AtomicU64,
    /// Terminal once set.
    closed: AtomicBool,
}

impl Default for Signal {
    fn default() -> Self {
        Self::new()
    }
}

impl Signal {
    /// A fresh, open signal.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ticket: Mutex::new(Ticket::default()),
            condvar: Condvar::new(),
            notify: Notify::new(),
            ticket_gauge: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    /// The current ticket.
    ///
    /// Read this *before* checking whatever condition the signal guards, and
    /// pass it to [`Signal::wait_blocking`].
    #[must_use]
    pub fn ticket(&self) -> u64 {
        self.ticket_gauge.load(Ordering::Acquire)
    }

    /// Whether the signal has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Wakes every waiter, blocking and async alike.
    ///
    /// Cheap enough to call per delivered event: it takes an uncontended
    /// mutex and does one atomic store plus two wakeups.
    pub fn notify(&self) {
        {
            let mut ticket = lock(&self.ticket);
            ticket.value = ticket.value.wrapping_add(1);
            // Published under the lock so a waiter that observed the old
            // value and is about to sleep cannot miss this increment.
            self.ticket_gauge.store(ticket.value, Ordering::Release);
        }
        self.condvar.notify_all();
        self.notify.notify_waiters();
    }

    /// Closes the signal: wakes everybody, permanently.
    ///
    /// Idempotent. A second call is a no-op beyond another (harmless)
    /// broadcast.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify();
    }

    /// The async wakeup future.
    ///
    /// Create it *before* checking the guarded condition, then `await` it if
    /// the condition was not met — the pattern `Notify` documents, and the
    /// one that makes a wakeup racing the check impossible to lose.
    pub fn notified(&self) -> Notified<'_> {
        self.notify.notified()
    }

    /// Blocks until the ticket moves past `since`, the signal closes, or the
    /// optional timeout expires.
    ///
    /// A `None` timeout waits indefinitely.
    #[must_use]
    pub fn wait_blocking(&self, since: u64, timeout: Option<Duration>) -> WaitOutcome {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut ticket = lock(&self.ticket);
        loop {
            if self.closed.load(Ordering::Acquire) {
                return WaitOutcome::Closed;
            }
            if ticket.value != since {
                return WaitOutcome::Signalled;
            }
            match deadline {
                None => {
                    ticket = self
                        .condvar
                        .wait(ticket)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                Some(deadline) => {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        return WaitOutcome::TimedOut;
                    };
                    let (guard, result) = self
                        .condvar
                        .wait_timeout(ticket, remaining)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    ticket = guard;
                    if result.timed_out()
                        && ticket.value == since
                        && !self.closed.load(Ordering::Acquire)
                    {
                        return WaitOutcome::TimedOut;
                    }
                }
            }
        }
    }

    /// Awaits the next wakeup, or returns immediately if the signal is closed.
    ///
    /// The ticket is not needed here: the caller creates the future with
    /// [`Signal::notified`] before checking its condition, which is the
    /// equivalent guarantee.
    pub async fn wait_async(&self) -> WaitOutcome {
        if self.is_closed() {
            return WaitOutcome::Closed;
        }
        let notified = self.notified();
        if self.is_closed() {
            return WaitOutcome::Closed;
        }
        notified.await;
        if self.is_closed() {
            WaitOutcome::Closed
        } else {
            WaitOutcome::Signalled
        }
    }
}

/// Locks a mutex, recovering from a poisoning panic elsewhere.
///
/// The guarded value is a single `u64`; a panic cannot leave it torn, so
/// discarding the whole signal because an unrelated thread unwound while
/// holding this lock would turn one bug into a wedged event loop.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_notification_before_the_wait_is_not_lost() {
        let signal = Signal::new();
        let ticket = signal.ticket();
        signal.notify();
        assert_eq!(
            signal.wait_blocking(ticket, Some(Duration::from_millis(50))),
            WaitOutcome::Signalled,
            "the ticket remembers the notification"
        );
    }

    #[test]
    fn a_wait_times_out_when_nothing_happens() {
        let signal = Signal::new();
        let ticket = signal.ticket();
        let start = Instant::now();
        assert_eq!(
            signal.wait_blocking(ticket, Some(Duration::from_millis(20))),
            WaitOutcome::TimedOut
        );
        assert!(start.elapsed() >= Duration::from_millis(15));
    }

    #[test]
    fn a_blocking_waiter_is_woken_by_another_thread() {
        let signal = Arc::new(Signal::new());
        let ticket = signal.ticket();
        let producer = Arc::clone(&signal);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            producer.notify();
        });
        assert_eq!(
            signal.wait_blocking(ticket, Some(Duration::from_secs(5))),
            WaitOutcome::Signalled
        );
        handle.join().unwrap();
    }

    #[test]
    fn closing_wakes_everybody_and_is_terminal() {
        let signal = Arc::new(Signal::new());
        assert!(!signal.is_closed());
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let signal = Arc::clone(&signal);
                let ticket = signal.ticket();
                std::thread::spawn(move || {
                    signal.wait_blocking(ticket, Some(Duration::from_secs(5)))
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(10));
        signal.close();
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap(), WaitOutcome::Closed);
        }
        assert!(signal.is_closed());

        // Terminal: a later wait returns at once, and a second close is a
        // no-op.
        signal.close();
        assert_eq!(
            signal.wait_blocking(signal.ticket(), None),
            WaitOutcome::Closed
        );
    }

    #[test]
    fn the_ticket_advances_once_per_notification() {
        let signal = Signal::new();
        let first = signal.ticket();
        signal.notify();
        let second = signal.ticket();
        signal.notify();
        assert_eq!(second, first.wrapping_add(1));
        assert_eq!(signal.ticket(), first.wrapping_add(2));
    }

    #[test]
    fn wait_outcomes_classify_continuation() {
        assert!(WaitOutcome::Signalled.may_continue());
        assert!(WaitOutcome::TimedOut.may_continue());
        assert!(!WaitOutcome::Closed.may_continue());
    }

    #[test]
    fn a_poisoned_lock_does_not_wedge_the_signal() {
        let signal = Arc::new(Signal::new());
        let poisoner = Arc::clone(&signal);
        let handle = std::thread::spawn(move || {
            let _guard = lock(&poisoner.ticket);
            panic!("simulated panic while holding the ticket lock");
        });
        assert!(handle.join().is_err());
        // The signal still works: `lock` recovers the inner value.
        let ticket = signal.ticket();
        signal.notify();
        assert_eq!(
            signal.wait_blocking(ticket, Some(Duration::from_millis(50))),
            WaitOutcome::Signalled
        );
    }

    #[tokio::test]
    async fn an_async_waiter_is_woken() {
        let signal = Arc::new(Signal::new());
        let producer = Arc::clone(&signal);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            producer.notify();
        });
        assert_eq!(signal.wait_async().await, WaitOutcome::Signalled);
    }

    #[tokio::test]
    async fn an_async_waiter_sees_a_close() {
        let signal = Arc::new(Signal::new());
        let producer = Arc::clone(&signal);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            producer.close();
        });
        assert_eq!(signal.wait_async().await, WaitOutcome::Closed);
        assert_eq!(signal.wait_async().await, WaitOutcome::Closed);
    }

    #[tokio::test]
    async fn the_notified_future_is_created_before_the_check() {
        // The documented pattern: a notification that lands between creating
        // the future and awaiting it is still delivered.
        let signal = Signal::new();
        let notified = signal.notified();
        signal.notify();
        notified.await;
    }
}
