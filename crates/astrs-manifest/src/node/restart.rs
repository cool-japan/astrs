//! Restart policy (blueprint §8.3, §12).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How the daemon supervises a node's process lifecycle (§12).
///
/// Written in the manifest with underscores (`on_failure`), matching §8.1's
/// literal `restart_policy: on_failure`.
///
/// A node with no `restart_policy` field behaves as [`RestartPolicy::Never`]
/// — see [`crate::Node::effective_restart_policy`], which is what the
/// validation pass ([`crate::Manifest::validate`]) checks `max_restarts` against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// The node is never restarted; a crash or exit is terminal.
    #[default]
    Never,
    /// The node is restarted only after a non-zero exit or a crash, not
    /// after a clean (zero) exit.
    OnFailure,
    /// The node is restarted after any exit, clean or not.
    Always,
}

impl RestartPolicy {
    /// Whether this policy ever triggers a restart.
    ///
    /// `false` only for [`RestartPolicy::Never`] — used by the validation
    /// pass to reject a `max_restarts` budget that could never be spent.
    #[must_use]
    pub fn restarts_at_all(self) -> bool {
        !matches!(self, Self::Never)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn default_is_never() {
        assert_eq!(RestartPolicy::default(), RestartPolicy::Never);
        assert!(!RestartPolicy::default().restarts_at_all());
    }

    #[test]
    fn uses_snake_case_on_wire() {
        assert_eq!(
            astrs_yaml::to_string(&RestartPolicy::OnFailure)
                .unwrap()
                .trim(),
            "on_failure"
        );
        let p: RestartPolicy = astrs_yaml::from_str("always").unwrap();
        assert_eq!(p, RestartPolicy::Always);
    }

    #[test]
    fn restarts_at_all_is_true_except_never() {
        assert!(!RestartPolicy::Never.restarts_at_all());
        assert!(RestartPolicy::OnFailure.restarts_at_all());
        assert!(RestartPolicy::Always.restarts_at_all());
    }
}
