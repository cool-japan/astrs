//! The one place this binary decides what "stop" means when it arrives as a
//! signal rather than as an argument.
//!
//! ```text
//!   SIGINT  ─┐
//!            ├─► Signals::next() ─► the verb's own graceful path
//!   SIGTERM ─┘                       (cancel a run, unsubscribe a tail,
//!                                     shut a server's listener down)
//! ```
//!
//! # Why `SIGTERM` and not only `SIGINT`
//!
//! Ctrl-C in a terminal sends `SIGINT`, and that is the case every user
//! meets. But every long-running verb in this binary also runs *under a
//! supervisor* — `astrs run` inside a CI step or a container, `astrs
//! coordinator`/`astrs daemon` under systemd or under `astrs up`'s own
//! `astrs down` — and a supervisor terminates rather than interrupts. A
//! process that handled only `SIGINT` would be killed outright by its
//! supervisor's default `SIGTERM` disposition, leaving the graph it was
//! supervising to be reaped by the orphan guard instead of stopped
//! gracefully. Both signals therefore mean exactly one thing here: begin the
//! same graceful stop.
//!
//! # Degrading rather than failing
//!
//! Installing a handler can fail (a process may have exhausted its signal
//! resources, or run somewhere the syscall is refused). A verb that refused
//! to start because a handler could not be installed would be strictly worse
//! than one that runs and cannot be interrupted — the graph is what the user
//! came for. [`Signals::install`] therefore keeps whichever handler it got
//! and, if it got none, simply never resolves.

/// The signals that mean "stop what you are doing, gracefully".
#[derive(Debug)]
pub struct Signals {
    /// The `SIGINT` stream, absent if it could not be installed.
    interrupt: Option<tokio::signal::unix::Signal>,
    /// The `SIGTERM` stream, absent if it could not be installed.
    terminate: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    /// Installs both handlers, degrading to "no handler" rather than
    /// failing the caller.
    ///
    /// Must be called from inside a tokio runtime (every caller in this
    /// crate is already inside its verb's own `block_on`).
    #[must_use]
    pub fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        Self {
            interrupt: signal(SignalKind::interrupt()).ok(),
            terminate: signal(SignalKind::terminate()).ok(),
        }
    }

    /// Whether any handler at all is installed — false only when the
    /// process could not have one, in which case [`Signals::next`] never
    /// resolves.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.interrupt.is_some() || self.terminate.is_some()
    }

    /// Resolves on the next signal of either kind.
    ///
    /// Cancel-safe: both underlying streams are, so this is safe to use as a
    /// `tokio::select!` arm that loses the race.
    pub async fn next(&mut self) {
        match (self.interrupt.as_mut(), self.terminate.as_mut()) {
            (Some(interrupt), Some(terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
            }
            (Some(one), None) | (None, Some(one)) => {
                let _ = one.recv().await;
            }
            (None, None) => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn installing_twice_is_harmless_and_both_handlers_arm() {
        let first = Signals::install();
        let second = Signals::install();
        assert!(first.is_armed());
        assert!(second.is_armed());
    }

    #[tokio::test]
    async fn a_real_signal_resolves_the_wait() {
        let mut signals = Signals::install();
        // Signalling this very process is the only honest test of a signal
        // handler; `SIGINT`'s default disposition would end the test binary,
        // which is exactly why the handler being installed first matters.
        let pid = rustix::process::Pid::from_raw(std::process::id().cast_signed())
            .expect("this process has a pid");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
        });
        tokio::time::timeout(Duration::from_secs(5), signals.next())
            .await
            .expect("the handler must observe SIGINT");
    }

    #[tokio::test]
    async fn a_wait_with_no_handlers_never_resolves() {
        let mut signals = Signals {
            interrupt: None,
            terminate: None,
        };
        assert!(!signals.is_armed());
        let timed_out = tokio::time::timeout(Duration::from_millis(30), signals.next())
            .await
            .is_err();
        assert!(timed_out, "an unarmed wait must never resolve");
    }
}
