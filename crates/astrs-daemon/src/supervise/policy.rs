//! The restart decision (§12) — one pure function, one enum.
//!
//! > *Restart policies per node: supervised respawn with exponential backoff
//! > (`restart_delay`×2^n capped by `max_restart_delay`), budget
//! > `max_restarts` within `restart_window`. New incarnation ⇒ new
//! > generation; stale SHM/messages are rejected by stamp.*
//!
//! [`decide`] takes the policy, the exit cause, the history and the clock, and
//! returns what to do. It touches nothing else: no process, no channel, no
//! logging. That is what makes the timing testable against
//! [`astrs_time::ManualClock`]-derived instants rather than against a
//! stopwatch, and what makes a `--deterministic` replay (§14) reproduce the
//! same restart schedule.
//!
//! The backoff and budget arithmetic is [`astrs_wire::RestartConfig`]'s
//! ([`RestartConfig::backoff_for`], [`RestartConfig::budget_exhausted`]) — the
//! same functions the manifest layer documents and the wire type exposes, not
//! a second implementation that could drift from them.
//!
//! # Decision table
//!
//! | Condition | Decision |
//! |---|---|
//! | Policy is `Never` | [`RestartDecision::Stop`] with the exit cause as-is |
//! | Policy is `OnFailure` and the node succeeded | [`RestartDecision::Stop`] |
//! | The daemon asked for the stop | [`RestartDecision::Stop`] — a restart would fight the operator |
//! | Budget spent inside the window | [`RestartDecision::Stop`] with [`NodeExitCause::RestartBudgetExhausted`] |
//! | Otherwise | [`RestartDecision::Restart`] after `restart_delay × 2^total` |
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::supervise::{RestartHistory, decide, RestartDecision};
//! use astrs_wire::{DurationMs, NodeExitCause, RestartConfig, RestartPolicy};
//!
//! let config = RestartConfig {
//!     policy: RestartPolicy::OnFailure,
//!     max_restarts: Some(2),
//!     restart_delay: DurationMs::new(100),
//!     max_restart_delay: DurationMs::from_secs(30),
//!     restart_window: DurationMs::from_secs(60),
//! };
//! let now = Instant::now();
//! let mut history = RestartHistory::new(config.restart_window.to_duration());
//!
//! // A clean exit under `on_failure` is the end of the node.
//! assert!(matches!(
//!     decide(&config, &NodeExitCause::Success, &mut history, false, now),
//!     RestartDecision::Stop { .. },
//! ));
//!
//! // A failure restarts after the base delay.
//! let failure = NodeExitCause::ExitCode { code: 1 };
//! assert_eq!(
//!     decide(&config, &failure, &mut history, false, now),
//!     RestartDecision::Restart { delay: Duration::from_millis(100) },
//! );
//! ```

use std::time::{Duration, Instant};

use astrs_wire::{DurationMs, NodeExitCause, RestartConfig, RestartPolicy};

use crate::supervise::history::RestartHistory;

/// What the supervisor should do after a node exits.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestartDecision {
    /// Respawn after waiting `delay`.
    Restart {
        /// The backoff before the next incarnation starts.
        delay: Duration,
    },
    /// Leave the node down, and record `cause` in the `DataflowResult`.
    Stop {
        /// The cause to record.
        cause: NodeExitCause,
    },
}

impl RestartDecision {
    /// Whether the node will come back.
    #[must_use]
    pub const fn is_restart(&self) -> bool {
        matches!(self, Self::Restart { .. })
    }

    /// The backoff, when there is one.
    #[must_use]
    pub const fn delay(&self) -> Option<Duration> {
        match self {
            Self::Restart { delay } => Some(*delay),
            Self::Stop { .. } => None,
        }
    }

    /// The recorded cause, when the node is staying down.
    #[must_use]
    pub const fn cause(&self) -> Option<&NodeExitCause> {
        match self {
            Self::Restart { .. } => None,
            Self::Stop { cause } => Some(cause),
        }
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Restart { .. } => "restart",
            Self::Stop { .. } => "stop",
        }
    }
}

/// Decides what to do about a node that just exited.
///
/// `daemon_initiated` is true when the daemon asked for this exit — a `Stop`
/// event, the finish-straggler watchdog, or the daemon shutting down. Such an
/// exit never restarts, whatever the policy says: an `always` node that fought
/// `astrs stop` by respawning would be unstoppable.
///
/// `history` is updated in place when the decision is to restart, so the next
/// call sees the new backoff exponent and budget count.
pub fn decide(
    config: &RestartConfig,
    cause: &NodeExitCause,
    history: &mut RestartHistory,
    daemon_initiated: bool,
    now: Instant,
) -> RestartDecision {
    if daemon_initiated {
        return RestartDecision::Stop {
            cause: cause.clone(),
        };
    }
    if !config.policy.should_restart(cause.is_success()) {
        return RestartDecision::Stop {
            cause: cause.clone(),
        };
    }
    let in_window = history.in_window(now);
    if config.budget_exhausted(in_window) {
        return RestartDecision::Stop {
            cause: NodeExitCause::RestartBudgetExhausted {
                restarts: in_window,
                window: config.restart_window,
            },
        };
    }

    let delay = config.backoff_for(history.total()).to_duration();
    history.record(now);
    RestartDecision::Restart { delay }
}

/// The delay the next restart would use, without deciding or recording.
///
/// For a diagnostic (`astrs info` showing "next restart in …") and for the
/// tests that assert the ladder.
#[must_use]
pub fn next_backoff(config: &RestartConfig, history: &RestartHistory) -> Duration {
    config.backoff_for(history.total()).to_duration()
}

/// The backoff ladder a configuration produces for the first `steps` restarts.
///
/// A diagnostic helper: `astrs doctor` prints it so an operator can see what
/// `restart_delay: 0.1` and `max_restart_delay: 30` actually mean before a
/// robot demonstrates it.
#[must_use]
pub fn backoff_ladder(config: &RestartConfig, steps: u32) -> Vec<Duration> {
    (0..steps)
        .map(|step| config.backoff_for(step).to_duration())
        .collect()
}

/// A restart configuration with the given policy and the §24.2 timings.
///
/// The shape a manifest that names only `restart_policy:` produces.
#[must_use]
pub fn config_for(policy: RestartPolicy) -> RestartConfig {
    RestartConfig {
        policy,
        max_restarts: None,
        restart_delay: DurationMs::new(100),
        max_restart_delay: DurationMs::from_secs(30),
        restart_window: DurationMs::from_secs(60),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn config(policy: RestartPolicy, max_restarts: Option<u32>) -> RestartConfig {
        RestartConfig {
            policy,
            max_restarts,
            restart_delay: DurationMs::new(100),
            max_restart_delay: DurationMs::from_secs(30),
            restart_window: DurationMs::from_secs(60),
        }
    }

    fn history(config: &RestartConfig) -> RestartHistory {
        RestartHistory::new(config.restart_window.to_duration())
    }

    fn failure() -> NodeExitCause {
        NodeExitCause::ExitCode { code: 1 }
    }

    #[test]
    fn never_means_never() {
        let config = config(RestartPolicy::Never, None);
        let mut history = history(&config);
        let now = Instant::now();
        for cause in [NodeExitCause::Success, failure()] {
            assert!(matches!(
                decide(&config, &cause, &mut history, false, now),
                RestartDecision::Stop { .. }
            ));
        }
        assert_eq!(history.total(), 0);
    }

    #[test]
    fn on_failure_restarts_only_failures() {
        let config = config(RestartPolicy::OnFailure, None);
        let mut history = history(&config);
        let now = Instant::now();

        assert!(
            decide(&config, &NodeExitCause::Success, &mut history, false, now)
                .cause()
                .is_some()
        );
        assert!(
            decide(&config, &failure(), &mut history, false, now).is_restart(),
            "a non-zero exit restarts"
        );
    }

    #[test]
    fn always_restarts_a_clean_exit_too() {
        let config = config(RestartPolicy::Always, None);
        let mut history = history(&config);
        let now = Instant::now();
        assert!(decide(&config, &NodeExitCause::Success, &mut history, false, now).is_restart());
    }

    #[test]
    fn a_daemon_initiated_stop_never_restarts() {
        let config = config(RestartPolicy::Always, None);
        let mut history = history(&config);
        let now = Instant::now();
        let decision = decide(&config, &NodeExitCause::Cancelled, &mut history, true, now);
        assert_eq!(
            decision,
            RestartDecision::Stop {
                cause: NodeExitCause::Cancelled
            }
        );
        assert_eq!(history.total(), 0, "a stop is not a restart");
    }

    #[test]
    fn the_backoff_doubles_and_then_caps() {
        let config = config(RestartPolicy::Always, None);
        let mut history = history(&config);
        let now = Instant::now();

        let expected = [100u64, 200, 400, 800, 1_600, 3_200, 6_400, 12_800, 25_600];
        for (step, millis) in expected.into_iter().enumerate() {
            let decision = decide(&config, &failure(), &mut history, false, now);
            assert_eq!(
                decision,
                RestartDecision::Restart {
                    delay: Duration::from_millis(millis)
                },
                "step {step}"
            );
        }
        // The tenth would be 51.2 s, over the 30 s cap.
        assert_eq!(
            decide(&config, &failure(), &mut history, false, now),
            RestartDecision::Restart {
                delay: Duration::from_secs(30)
            }
        );
    }

    #[test]
    fn the_budget_is_counted_over_the_window() {
        let config = config(RestartPolicy::Always, Some(3));
        let mut history = history(&config);
        let start = Instant::now();

        for step in 0..3 {
            let now = start + Duration::from_secs(step);
            assert!(
                decide(&config, &failure(), &mut history, false, now).is_restart(),
                "restart {step} is inside the budget"
            );
        }

        let decision = decide(
            &config,
            &failure(),
            &mut history,
            false,
            start + Duration::from_secs(4),
        );
        match decision.cause() {
            Some(NodeExitCause::RestartBudgetExhausted { restarts, window }) => {
                assert_eq!(*restarts, 3);
                assert_eq!(*window, config.restart_window);
            }
            other => panic!("expected an exhausted budget, got {other:?}"),
        }
    }

    #[test]
    fn the_budget_recovers_once_the_window_slides_past() {
        let config = config(RestartPolicy::Always, Some(2));
        let mut history = history(&config);
        let start = Instant::now();

        assert!(decide(&config, &failure(), &mut history, false, start).is_restart());
        assert!(decide(&config, &failure(), &mut history, false, start).is_restart());
        assert!(!decide(&config, &failure(), &mut history, false, start).is_restart());

        // A minute later the window is empty again.
        let later = start + Duration::from_secs(61);
        assert!(decide(&config, &failure(), &mut history, false, later).is_restart());
    }

    #[test]
    fn an_unlimited_budget_never_exhausts() {
        let config = config(RestartPolicy::Always, None);
        let mut history = history(&config);
        let now = Instant::now();
        for _ in 0..100 {
            assert!(decide(&config, &failure(), &mut history, false, now).is_restart());
        }
        assert_eq!(history.total(), 100);
    }

    #[test]
    fn a_zero_budget_stops_immediately() {
        let config = config(RestartPolicy::Always, Some(0));
        let mut history = history(&config);
        assert!(
            !decide(&config, &failure(), &mut history, false, Instant::now()).is_restart(),
            "max_restarts: 0 means do not restart"
        );
    }

    #[test]
    fn the_next_backoff_is_readable_without_deciding() {
        let config = config(RestartPolicy::Always, None);
        let mut history = history(&config);
        assert_eq!(next_backoff(&config, &history), Duration::from_millis(100));
        history.record(Instant::now());
        assert_eq!(next_backoff(&config, &history), Duration::from_millis(200));
    }

    #[test]
    fn the_ladder_is_printable() {
        let ladder = backoff_ladder(&config(RestartPolicy::Always, None), 4);
        assert_eq!(
            ladder,
            [
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(800),
            ]
        );
        assert!(backoff_ladder(&config(RestartPolicy::Always, None), 0).is_empty());
    }

    #[test]
    fn decisions_classify_and_name_themselves() {
        let restart = RestartDecision::Restart {
            delay: Duration::from_millis(1),
        };
        assert!(restart.is_restart());
        assert_eq!(restart.delay(), Some(Duration::from_millis(1)));
        assert!(restart.cause().is_none());
        assert_eq!(restart.kind_name(), "restart");

        let stop = RestartDecision::Stop {
            cause: NodeExitCause::Success,
        };
        assert!(!stop.is_restart());
        assert!(stop.delay().is_none());
        assert_eq!(stop.cause(), Some(&NodeExitCause::Success));
        assert_eq!(stop.kind_name(), "stop");
    }

    #[test]
    fn the_default_configuration_follows_the_appendix() {
        let config = config_for(RestartPolicy::OnFailure);
        assert_eq!(config.policy, RestartPolicy::OnFailure);
        assert_eq!(config.restart_delay, DurationMs::new(100));
        assert_eq!(config.max_restart_delay, DurationMs::from_secs(30));
        assert_eq!(config.restart_window, DurationMs::from_secs(60));
        assert_eq!(config.max_restarts, None);
    }

    #[test]
    fn a_spawn_failure_is_still_a_failure_worth_retrying() {
        let config = config(RestartPolicy::OnFailure, None);
        let mut history = history(&config);
        let cause = NodeExitCause::SpawnFailed {
            message: "no such file".into(),
        };
        assert!(decide(&config, &cause, &mut history, false, Instant::now()).is_restart());
    }

    #[test]
    fn a_health_check_timeout_restarts_under_on_failure() {
        let config = config(RestartPolicy::OnFailure, None);
        let mut history = history(&config);
        let cause = NodeExitCause::HealthCheckTimeout {
            after: DurationMs::from_secs(5),
        };
        assert!(decide(&config, &cause, &mut history, false, Instant::now()).is_restart());
    }
}
