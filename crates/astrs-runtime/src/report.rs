//! [`RuntimeReport`] — what [`crate::RuntimeHost::run`] hands back once the
//! host has wound down.
//!
//! One entry per hosted operator, in manifest declaration order, so a
//! caller (today: a test; eventually: the daemon's own health reporting)
//! can tell *which* operator did what without re-deriving it from log
//! lines.

use std::time::Duration;

/// A single caught failure inside one operator incarnation.
///
/// Recorded whether the failure was a panic (caught at the
/// [`crate::RuntimeHost`] boundary) or an ordinary
/// [`astrs_operator_api::OpError`] returned from a lifecycle hook — both are
/// "this incarnation cannot continue", and a caller deciding whether a
/// restart loop is healthy needs both kinds counted the same way.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorFailure {
    /// Which lifecycle hook was running when this incarnation ended.
    pub hook: FailedHook,
    /// A human-readable description: the panic payload downcast to a
    /// string, or the [`astrs_operator_api::OpError`]'s own `Display`.
    pub message: String,
    /// Whether this was a caught panic rather than an `Err` return.
    pub panicked: bool,
    /// This operator's restart counter at the moment of the failure (0 for
    /// the first incarnation).
    pub incarnation: u32,
}

/// Which [`astrs_operator_api::Operator`] method was running when an
/// incarnation ended in failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailedHook {
    /// [`astrs_operator_api::Operator::configure`].
    Configure,
    /// [`astrs_operator_api::Operator::on_start`].
    OnStart,
    /// [`astrs_operator_api::Operator::on_event`].
    OnEvent,
    /// [`astrs_operator_api::Operator::on_reload`].
    OnReload,
    /// [`astrs_operator_api::Operator::on_stop`].
    OnStop,
    /// Building the operator itself (`Default::default()`, via the
    /// registry's constructor) panicked or the registry had no such name.
    Construct,
}

impl core::fmt::Display for FailedHook {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Configure => "configure",
            Self::OnStart => "on_start",
            Self::OnEvent => "on_event",
            Self::OnReload => "on_reload",
            Self::OnStop => "on_stop",
            Self::Construct => "construct",
        })
    }
}

/// How one hosted operator's life inside this run ended.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum OperatorOutcome {
    /// The operator wound down cleanly: [`astrs_operator_api::Status::Finished`]
    /// or an [`astrs_operator_api::OpEvent::Stop`] was delivered, `on_stop`
    /// ran, and the restart policy did not call for another incarnation.
    Finished {
        /// How many times this operator was restarted before this, final,
        /// incarnation (0 if it never failed).
        restarts: u32,
    },
    /// Every incarnation failed and the restart policy gave up — either
    /// [`astrs_wire::RestartPolicy::Never`] on the first failure, or the
    /// [`astrs_wire::RestartConfig`] budget was exhausted.
    Failed {
        /// Every caught failure, oldest first.
        failures: Vec<OperatorFailure>,
        /// Whether the restart budget itself was the reason this operator
        /// stopped being restarted (`false` means the policy was
        /// [`astrs_wire::RestartPolicy::Never`]).
        budget_exhausted: bool,
    },
    /// The operator's own worker thread could not be joined cleanly — a bug
    /// in this host, not in the operator (every operator-level panic is
    /// already caught inside the loop [`OperatorOutcome::Failed`] reports).
    HostPanicked {
        /// The panic payload, downcast to a string where possible.
        message: String,
    },
}

impl OperatorOutcome {
    /// Whether this operator ended its run in a state a supervisor should
    /// treat as healthy.
    #[must_use]
    pub const fn is_healthy(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }

    /// How many times this operator was restarted, for every outcome that
    /// tracks a count.
    #[must_use]
    pub fn restarts(&self) -> u32 {
        match self {
            Self::Finished { restarts } => *restarts,
            Self::Failed { failures, .. } => failures.len().saturating_sub(1) as u32,
            Self::HostPanicked { .. } => 0,
        }
    }
}

/// One hosted operator's identity alongside how it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorReport {
    /// This operator's id (`operators[].id` in the manifest).
    pub id: String,
    /// The registry name it was constructed from.
    pub registry_name: String,
    /// How its run ended.
    pub outcome: OperatorOutcome,
}

/// What [`crate::RuntimeHost::run`] returns once every hosted operator has
/// wound down and the node's outputs are closed.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeReport {
    /// One entry per hosted operator, in manifest declaration order.
    pub operators: Vec<OperatorReport>,
    /// How long the run took, from the first event read to the last output
    /// closed.
    pub elapsed: Duration,
}

impl RuntimeReport {
    /// Whether every hosted operator ended healthily.
    #[must_use]
    pub fn all_healthy(&self) -> bool {
        self.operators
            .iter()
            .all(|report| report.outcome.is_healthy())
    }

    /// The operators that did not end healthily.
    #[must_use]
    pub fn unhealthy(&self) -> Vec<&OperatorReport> {
        self.operators
            .iter()
            .filter(|report| !report.outcome.is_healthy())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn finished(id: &str, restarts: u32) -> OperatorReport {
        OperatorReport {
            id: id.to_owned(),
            registry_name: "Echo".to_owned(),
            outcome: OperatorOutcome::Finished { restarts },
        }
    }

    #[test]
    fn all_healthy_is_true_only_when_every_operator_finished() {
        let report = RuntimeReport {
            operators: vec![finished("a", 0), finished("b", 2)],
            elapsed: Duration::from_millis(5),
        };
        assert!(report.all_healthy());
        assert!(report.unhealthy().is_empty());
    }

    #[test]
    fn unhealthy_lists_everything_that_did_not_finish() {
        let failed = OperatorReport {
            id: "c".to_owned(),
            registry_name: "Nms".to_owned(),
            outcome: OperatorOutcome::Failed {
                failures: vec![OperatorFailure {
                    hook: FailedHook::OnEvent,
                    message: "boom".to_owned(),
                    panicked: true,
                    incarnation: 0,
                }],
                budget_exhausted: false,
            },
        };
        let host_panicked = OperatorReport {
            id: "d".to_owned(),
            registry_name: "Bad".to_owned(),
            outcome: OperatorOutcome::HostPanicked {
                message: "join failed".to_owned(),
            },
        };
        let report = RuntimeReport {
            operators: vec![finished("a", 0), failed, host_panicked],
            elapsed: Duration::ZERO,
        };
        assert!(!report.all_healthy());
        assert_eq!(report.unhealthy().len(), 2);
    }

    #[test]
    fn restarts_counts_from_failure_history_or_the_finished_counter() {
        assert_eq!(OperatorOutcome::Finished { restarts: 3 }.restarts(), 3);
        assert_eq!(
            OperatorOutcome::HostPanicked {
                message: String::new()
            }
            .restarts(),
            0
        );
        let failed = OperatorOutcome::Failed {
            failures: vec![
                OperatorFailure {
                    hook: FailedHook::OnStart,
                    message: "a".to_owned(),
                    panicked: false,
                    incarnation: 0,
                },
                OperatorFailure {
                    hook: FailedHook::OnStart,
                    message: "b".to_owned(),
                    panicked: false,
                    incarnation: 1,
                },
            ],
            budget_exhausted: true,
        };
        assert_eq!(failed.restarts(), 1);
    }

    #[test]
    fn failed_hook_renders_every_variant() {
        for hook in [
            FailedHook::Configure,
            FailedHook::OnStart,
            FailedHook::OnEvent,
            FailedHook::OnReload,
            FailedHook::OnStop,
            FailedHook::Construct,
        ] {
            assert!(!hook.to_string().is_empty());
        }
    }
}
