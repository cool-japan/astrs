//! A private helper for recovering from mutex poisoning.
//!
//! Every lock in this crate guards a small, panic-safe invariant: a
//! [`VecDeque`](std::collections::VecDeque) plus a handful of counters, or a
//! fixed-size array of `Vec`s. None of the code that runs while holding one
//! of these locks can leave the guarded data in a state that violates its
//! own invariants if it panics partway through — there is no multi-step
//! protocol with an observable "torn" middle state. So if some *unrelated*
//! panic elsewhere in the process happens to unwind through a thread that
//! was holding one of these locks (the only way `std::sync::Mutex` ever
//! poisons), discarding the guarded scheduler state along with it would
//! turn one bug into a second, unrelated outage — every other input's
//! queue, or every other registered timer, stops working too. Recovering
//! the inner value and carrying on is the sound choice here specifically
//! *because* the critical sections are this small and this simple; it would
//! not be sound for a lock guarding, say, a multi-step transaction.

use std::sync::{Mutex, MutexGuard};

/// Locks `mutex`, recovering the inner guard even if a previous holder
/// panicked while holding it (see the module docs for why that is sound for
/// every lock this crate takes).
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_returns_the_guarded_value() {
        let mutex = Mutex::new(41);
        *lock(&mutex) += 1;
        assert_eq!(*lock(&mutex), 42);
    }

    #[test]
    fn lock_recovers_after_a_panic_while_held() {
        let mutex = Arc::new(Mutex::new(Vec::<i32>::new()));
        let clone = Arc::clone(&mutex);
        let handle = std::thread::spawn(move || {
            let mut guard = lock(&clone);
            guard.push(1);
            panic!("simulated panic while holding the lock");
        });
        assert!(handle.join().is_err());
        assert!(mutex.is_poisoned());

        // The value pushed before the panic is still there; the crate keeps
        // operating on it rather than losing the whole structure.
        let mut guard = lock(&mutex);
        assert_eq!(*guard, vec![1]);
        guard.push(2);
        drop(guard);
        assert_eq!(*lock(&mutex), vec![1, 2]);
    }
}
