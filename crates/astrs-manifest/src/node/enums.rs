//! Small closed-set enums used directly on [`crate::Node`] (blueprint §8.3).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The service/action wiring pattern a node participates in.
///
/// Written in the manifest with hyphens (`service-server`), matching
/// blueprint §8.3's literal spelling — unlike [`crate::RestartPolicy`] and
/// [`crate::QueuePolicy`], which use underscores (§8.1's
/// `restart_policy: on_failure`, `queue_policy: drop_oldest`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Pattern {
    /// This node answers request/response service calls.
    ServiceServer,
    /// This node issues request/response service calls.
    ServiceClient,
    /// This node executes long-running goals with progress feedback.
    ActionServer,
    /// This node submits long-running goals and tracks their progress.
    ActionClient,
}

/// The minimum severity a node's log records must meet to be kept.
///
/// Mirrors `tracing`'s level names; used for `min_log_level` on
/// [`crate::Node`] and for the optional `/level` segment of the
/// `astrs/logs[/level[/node]]` virtual source (§8.4).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Fine-grained diagnostic detail, off by default in production.
    Trace,
    /// Diagnostic detail useful during development.
    Debug,
    /// Routine operational messages.
    Info,
    /// Recoverable problems worth operator attention.
    Warn,
    /// Failures that likely require operator intervention.
    Error,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn pattern_uses_kebab_case_on_wire() {
        assert_eq!(
            astrs_yaml::to_string(&Pattern::ServiceServer)
                .unwrap()
                .trim(),
            "service-server"
        );
        assert_eq!(
            astrs_yaml::to_string(&Pattern::ActionClient)
                .unwrap()
                .trim(),
            "action-client"
        );
        let p: Pattern = astrs_yaml::from_str("service-client").unwrap();
        assert_eq!(p, Pattern::ServiceClient);
    }

    #[test]
    fn log_level_uses_snake_case_on_wire() {
        assert_eq!(
            astrs_yaml::to_string(&LogLevel::Warn).unwrap().trim(),
            "warn"
        );
        let l: LogLevel = astrs_yaml::from_str("error").unwrap();
        assert_eq!(l, LogLevel::Error);
    }

    #[test]
    fn log_level_orders_by_severity() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn pattern_rejects_unknown_variant() {
        assert!(astrs_yaml::from_str::<Pattern>("bogus").is_err());
    }
}
