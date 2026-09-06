//! Exit-cause taxonomy — turning an `ExitStatus` into a typed cause (§12).
//!
//! > *Error propagation: a failing node emits `NodeFailed` to graph peers, the
//! > dataflow FSM aggregates per-node exit causes into a `DataflowResult` with
//! > typed causes (not strings).*
//!
//! "Not strings" is the whole design constraint. `wait(2)` gives back an
//! integer; what the operator needs to see is *why*, and what the restart
//! policy needs to decide on is *whether that counts as a failure*. This
//! module is the one place where the integer becomes an
//! [`astrs_wire::NodeExitCause`], so every consumer downstream — the restart
//! policy, the `DataflowResult`, the `NodeFailed` fan-out, the metrics label —
//! agrees.
//!
//! # The stop-aware reading
//!
//! The same `SIGTERM` means two different things depending on whether the
//! daemon sent it. [`classify_with_intent`] takes that context: a node the
//! daemon asked to stop and which then died of `SIGTERM` exited
//! [`NodeExitCause::Cancelled`] (an operator got what they asked for), while
//! the identical status with no stop pending is
//! [`NodeExitCause::Signal`] (something killed the node).
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::supervise::{ExitIntent, classify_with_intent, signal_name};
//! use astrs_wire::NodeExitCause;
//!
//! assert_eq!(signal_name(9), "SIGKILL");
//!
//! # #[cfg(unix)] {
//! use std::os::unix::process::ExitStatusExt;
//! let terminated = std::process::ExitStatus::from_raw(15); // killed by SIGTERM
//!
//! assert!(matches!(
//!     classify_with_intent(terminated, ExitIntent::Running),
//!     NodeExitCause::Signal { signal: 15, .. },
//! ));
//! assert_eq!(
//!     classify_with_intent(terminated, ExitIntent::StopRequested),
//!     NodeExitCause::Cancelled,
//! );
//! # }
//! ```

use std::process::ExitStatus;

use astrs_wire::NodeExitCause;

/// What the daemon was expecting when the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExitIntent {
    /// Nothing was pending: the node ended on its own.
    #[default]
    Running,
    /// A `Stop` was delivered and the node was expected to wind down.
    StopRequested,
    /// The finish-straggler watchdog escalated to `SIGTERM`.
    TerminatedByWatchdog,
    /// The finish-straggler watchdog escalated to `SIGKILL`.
    KilledByWatchdog,
    /// The daemon itself is going away.
    DaemonShutdown,
}

impl ExitIntent {
    /// Whether the daemon caused this exit.
    #[must_use]
    pub const fn is_daemon_initiated(self) -> bool {
        !matches!(self, Self::Running)
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::StopRequested => "stop_requested",
            Self::TerminatedByWatchdog => "terminated_by_watchdog",
            Self::KilledByWatchdog => "killed_by_watchdog",
            Self::DaemonShutdown => "daemon_shutdown",
        }
    }
}

/// The conventional name of a POSIX signal number.
///
/// Unknown numbers (real-time signals, platform extensions) render as
/// `SIG<number>` rather than being dropped — the number is still the useful
/// part, and an operator reading `SIG42` learns more than reading `unknown`.
#[must_use]
pub fn signal_name(signal: i32) -> String {
    let name = match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        17 => "SIGCHLD",
        18 => "SIGCONT",
        19 => "SIGSTOP",
        20 => "SIGTSTP",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        26 => "SIGVTALRM",
        27 => "SIGPROF",
        31 => "SIGSYS",
        _ => return format!("SIG{signal}"),
    };
    name.to_string()
}

/// Whether a signal number is one a crash produces rather than a shutdown.
///
/// Used to decide whether an exit deserves the `NodeFailed` fan-out even when
/// a stop was pending: a node that segfaults *while* shutting down still
/// segfaulted.
#[must_use]
pub const fn is_crash_signal(signal: i32) -> bool {
    matches!(signal, 4 | 6 | 7 | 8 | 11 | 31)
}

/// Classifies a raw exit status with no stop context.
#[must_use]
pub fn classify(status: ExitStatus) -> NodeExitCause {
    classify_with_intent(status, ExitIntent::Running)
}

/// Classifies a raw exit status against what the daemon was expecting.
#[must_use]
pub fn classify_with_intent(status: ExitStatus, intent: ExitIntent) -> NodeExitCause {
    if let Some(signal) = signal_of(status) {
        return classify_signal(signal, intent);
    }
    let code = status.code().unwrap_or(-1);
    if code == 0 {
        return NodeExitCause::Success;
    }
    match intent {
        // A node that was told to stop and chose a non-zero code on the way
        // out is still a node that failed; only a *signal* the daemon sent is
        // attributable to the daemon.
        ExitIntent::DaemonShutdown => NodeExitCause::DaemonShutdown,
        _ => NodeExitCause::ExitCode { code },
    }
}

/// Classifies a signal death.
fn classify_signal(signal: i32, intent: ExitIntent) -> NodeExitCause {
    // A crash is a crash no matter what the daemon was doing at the time.
    if is_crash_signal(signal) {
        return NodeExitCause::Signal {
            signal,
            name: signal_name(signal),
        };
    }
    match intent {
        ExitIntent::StopRequested | ExitIntent::TerminatedByWatchdog => NodeExitCause::Cancelled,
        ExitIntent::KilledByWatchdog => NodeExitCause::Killed {
            reason: format!(
                "did not exit within the finish grace period; escalated to {}",
                signal_name(signal)
            ),
        },
        ExitIntent::DaemonShutdown => NodeExitCause::DaemonShutdown,
        ExitIntent::Running => NodeExitCause::Signal {
            signal,
            name: signal_name(signal),
        },
    }
}

/// The signal that killed a process, if one did.
#[must_use]
pub fn signal_of(status: ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(&status)
}

/// Whether a cause should produce a `NodeFailed` fan-out to graph peers (§12).
///
/// A deliberate stop is not news; a crash is. `DaemonShutdown` is excluded
/// because every peer is being stopped anyway.
#[must_use]
pub fn warrants_peer_notification(cause: &NodeExitCause) -> bool {
    cause.is_failure()
}

/// A short human phrase for a log line.
#[must_use]
pub fn describe(cause: &NodeExitCause) -> String {
    cause.to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// A status for a process that exited with `code`.
    fn exited(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    /// A status for a process killed by `signal`.
    fn signalled(signal: i32) -> ExitStatus {
        ExitStatus::from_raw(signal)
    }

    #[test]
    fn a_zero_exit_is_a_success() {
        assert_eq!(classify(exited(0)), NodeExitCause::Success);
        assert!(classify(exited(0)).is_success());
    }

    #[test]
    fn a_non_zero_exit_keeps_its_code() {
        assert_eq!(classify(exited(3)), NodeExitCause::ExitCode { code: 3 });
        assert!(classify(exited(3)).is_failure());
    }

    #[test]
    fn a_signal_death_keeps_its_number_and_name() {
        match classify(signalled(11)) {
            NodeExitCause::Signal { signal, name } => {
                assert_eq!(signal, 11);
                assert_eq!(name, "SIGSEGV");
            }
            other => panic!("expected a signal cause, got {other}"),
        }
    }

    #[test]
    fn a_requested_stop_turns_sigterm_into_a_cancellation() {
        assert_eq!(
            classify_with_intent(signalled(15), ExitIntent::StopRequested),
            NodeExitCause::Cancelled
        );
        assert_eq!(
            classify_with_intent(signalled(15), ExitIntent::TerminatedByWatchdog),
            NodeExitCause::Cancelled
        );
        assert!(!NodeExitCause::Cancelled.is_failure());
    }

    #[test]
    fn a_watchdog_kill_says_so() {
        match classify_with_intent(signalled(9), ExitIntent::KilledByWatchdog) {
            NodeExitCause::Killed { reason } => {
                assert!(reason.contains("finish grace"), "{reason}");
                assert!(reason.contains("SIGKILL"), "{reason}");
            }
            other => panic!("expected a killed cause, got {other}"),
        }
    }

    #[test]
    fn a_crash_is_a_crash_even_during_a_stop() {
        for signal in [4, 6, 7, 8, 11, 31] {
            for intent in [
                ExitIntent::StopRequested,
                ExitIntent::TerminatedByWatchdog,
                ExitIntent::KilledByWatchdog,
                ExitIntent::DaemonShutdown,
            ] {
                let cause = classify_with_intent(signalled(signal), intent);
                assert!(
                    matches!(cause, NodeExitCause::Signal { .. }),
                    "signal {signal} under {intent:?} became {cause}"
                );
            }
        }
    }

    #[test]
    fn a_daemon_shutdown_absorbs_ordinary_exits() {
        assert_eq!(
            classify_with_intent(signalled(15), ExitIntent::DaemonShutdown),
            NodeExitCause::DaemonShutdown
        );
        assert_eq!(
            classify_with_intent(exited(1), ExitIntent::DaemonShutdown),
            NodeExitCause::DaemonShutdown
        );
        assert_eq!(
            classify_with_intent(exited(0), ExitIntent::DaemonShutdown),
            NodeExitCause::Success,
            "a clean exit is a clean exit"
        );
    }

    #[test]
    fn a_stopped_node_that_exits_non_zero_still_failed() {
        assert_eq!(
            classify_with_intent(exited(2), ExitIntent::StopRequested),
            NodeExitCause::ExitCode { code: 2 }
        );
    }

    #[test]
    fn signal_names_cover_the_common_set_and_degrade_gracefully() {
        let expected = [
            (1, "SIGHUP"),
            (2, "SIGINT"),
            (6, "SIGABRT"),
            (9, "SIGKILL"),
            (11, "SIGSEGV"),
            (13, "SIGPIPE"),
            (15, "SIGTERM"),
        ];
        for (signal, name) in expected {
            assert_eq!(signal_name(signal), name);
        }
        assert_eq!(signal_name(42), "SIG42");
        assert_eq!(signal_name(-1), "SIG-1");
    }

    #[test]
    fn crash_signals_are_the_fault_ones() {
        assert!(is_crash_signal(11));
        assert!(is_crash_signal(6));
        assert!(!is_crash_signal(15));
        assert!(!is_crash_signal(2));
        assert!(!is_crash_signal(9));
    }

    #[test]
    fn peer_notification_follows_the_failure_predicate() {
        assert!(warrants_peer_notification(&NodeExitCause::ExitCode {
            code: 1
        }));
        assert!(warrants_peer_notification(&NodeExitCause::Signal {
            signal: 11,
            name: "SIGSEGV".into()
        }));
        assert!(!warrants_peer_notification(&NodeExitCause::Success));
        assert!(!warrants_peer_notification(&NodeExitCause::Cancelled));
        assert!(!warrants_peer_notification(&NodeExitCause::DaemonShutdown));
    }

    #[test]
    fn intents_classify_and_name_themselves() {
        assert!(!ExitIntent::Running.is_daemon_initiated());
        for intent in [
            ExitIntent::StopRequested,
            ExitIntent::TerminatedByWatchdog,
            ExitIntent::KilledByWatchdog,
            ExitIntent::DaemonShutdown,
        ] {
            assert!(intent.is_daemon_initiated(), "{intent:?}");
            assert!(!intent.kind_name().is_empty());
        }
        assert_eq!(ExitIntent::default(), ExitIntent::Running);
    }

    #[test]
    fn descriptions_are_never_empty() {
        for cause in [
            NodeExitCause::Success,
            NodeExitCause::ExitCode { code: 1 },
            NodeExitCause::Cancelled,
            NodeExitCause::DaemonShutdown,
        ] {
            assert!(!describe(&cause).is_empty());
        }
    }

    #[test]
    fn the_signal_accessor_agrees_with_the_classifier() {
        assert_eq!(signal_of(signalled(9)), Some(9));
        assert_eq!(signal_of(exited(3)), None);
    }
}
