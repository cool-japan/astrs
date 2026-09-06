//! Real-time scheduling for a node's process (`rt:`, blueprint §11.3).
//!
//! §11.3's "real-time accommodations" ship `cpu_affinity` and
//! `priority_lane` on [`crate::Node`] already; this block adds the third
//! piece — which OS scheduling *class* the spawned process runs under, and at
//! what priority within it.
//!
//! # Why `priority` is required for `fifo`/`rr` and rejected for `normal`
//!
//! `SCHED_FIFO` and `SCHED_RR` are genuinely real-time classes: a runaway
//! thread in either can starve every other process on the machine, including
//! the daemon supervising it. POSIX gives them a priority range (`1..=99` on
//! Linux) with no defensible default — a wrong guess is either "silently not
//! actually real-time" or "silently pre-empts your control loop" — so the
//! manifest must say. `SCHED_OTHER` (`normal`) has no priority axis at all:
//! its `sched_priority` must be `0`, and a nice value is a different field
//! entirely. A `priority` alongside `policy: normal` is therefore not a
//! harmless extra but a statement the system cannot honor, and blueprint
//! §2.2's "config never silently lies about what it does" makes that a
//! rejection rather than an ignored key.
//!
//! Both rules are enforced by [`crate::Manifest::validate`], not by
//! `Deserialize`: the crate reports every structural problem in one pass
//! rather than failing at the first (see the validation pass's own module docs),
//! and a `serde` shape cannot express "required only for two of three enum
//! variants" without an untagged enum that would wreck the error messages.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The lowest `sched_priority` a real-time policy accepts (POSIX/Linux).
pub const RT_PRIORITY_MIN: u8 = 1;

/// The highest `sched_priority` a real-time policy accepts (POSIX/Linux).
pub const RT_PRIORITY_MAX: u8 = 99;

/// Which OS scheduling class a node's process runs under (§11.3).
///
/// Spelled in the manifest exactly as the POSIX policy names are commonly
/// written — `normal`, `fifo`, `rr` — rather than as their `SCHED_*`
/// constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RtPolicy {
    /// `SCHED_OTHER`: the ordinary time-sharing class every process gets by
    /// default. Carries no `sched_priority` axis — see this module's docs for
    /// why a `priority` alongside it is rejected rather than ignored.
    #[default]
    Normal,
    /// `SCHED_FIFO`: run until the process blocks, yields, or is pre-empted
    /// by a strictly higher-priority real-time thread. No timeslice.
    Fifo,
    /// `SCHED_RR`: `SCHED_FIFO` plus a timeslice, so equal-priority real-time
    /// threads round-robin against each other instead of the first one
    /// holding the CPU.
    Rr,
}

impl RtPolicy {
    /// Whether this is a real-time class (`fifo`/`rr`) — the two that
    /// require a `priority`, and the two that can starve the rest of the
    /// machine if set carelessly.
    #[must_use]
    pub const fn is_realtime(self) -> bool {
        matches!(self, Self::Fifo | Self::Rr)
    }

    /// This policy's manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Fifo => "fifo",
            Self::Rr => "rr",
        }
    }
}

impl std::fmt::Display for RtPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A node's `rt:` block: the OS scheduling class its process is spawned
/// under, and its priority within that class (blueprint §11.3).
///
/// ```yaml
/// nodes:
///   - id: control-loop
///     path: ./control-loop
///     rt:
///       policy: fifo
///       priority: 80
/// ```
///
/// [`RtConfig::priority`] is `Option` in the *shape* but not in the
/// *contract*: [`crate::Manifest::validate`] requires it (in
/// [`RT_PRIORITY_MIN`]`..=`[`RT_PRIORITY_MAX`]) whenever
/// [`RtPolicy::is_realtime`] holds, and rejects it outright for
/// [`RtPolicy::Normal`]. See this module's docs for why that split lives in
/// the validation pass rather than in `Deserialize`.
///
/// # Examples
///
/// ```
/// use astrs_manifest::{Manifest, RtPolicy};
///
/// let yaml = "\
/// nodes:
///   - id: control-loop
///     path: ./control-loop
///     rt:
///       policy: fifo
///       priority: 80
/// ";
/// let manifest = Manifest::from_yaml_str(yaml)?;
/// manifest.validate()?;
///
/// let rt = manifest.nodes[0].rt.as_ref().ok_or("expected an rt block")?;
/// assert_eq!(rt.policy, RtPolicy::Fifo);
/// assert_eq!(rt.priority, Some(80));
/// assert!(rt.policy.is_realtime());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RtConfig {
    /// The scheduling class. Defaults to [`RtPolicy::Normal`] when the key is
    /// omitted, so `rt: { priority: 80 }` is a rejected manifest (a priority
    /// with no real-time policy to apply it to), not a silent promotion.
    #[serde(default, skip_serializing_if = "is_default_policy")]
    pub policy: RtPolicy,
    /// The `sched_priority` within the class:
    /// [`RT_PRIORITY_MIN`]`..=`[`RT_PRIORITY_MAX`], required for
    /// `fifo`/`rr` and rejected for `normal` (both checked by
    /// [`crate::Manifest::validate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

fn is_default_policy(policy: &RtPolicy) -> bool {
    *policy == RtPolicy::default()
}

impl RtConfig {
    /// An `rt:` block for a real-time policy at an explicit priority.
    ///
    /// No validation happens here — [`crate::Manifest::validate`] is the one
    /// place the priority range and the policy/priority pairing are checked,
    /// so a hand-built config and a parsed one are held to exactly the same
    /// rule by exactly the same code.
    #[must_use]
    pub const fn new(policy: RtPolicy, priority: u8) -> Self {
        Self {
            policy,
            priority: Some(priority),
        }
    }

    /// Whether `priority` falls in the range a real-time policy accepts.
    ///
    /// `false` when no priority is set at all — a real-time policy without
    /// one is exactly the case [`crate::Manifest::validate`] rejects.
    #[must_use]
    pub const fn has_valid_realtime_priority(&self) -> bool {
        match self.priority {
            Some(priority) => priority >= RT_PRIORITY_MIN && priority <= RT_PRIORITY_MAX,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn policy_defaults_to_normal_and_is_not_realtime() {
        assert_eq!(RtPolicy::default(), RtPolicy::Normal);
        assert!(!RtPolicy::Normal.is_realtime());
        assert!(RtPolicy::Fifo.is_realtime());
        assert!(RtPolicy::Rr.is_realtime());
    }

    #[test]
    fn policy_uses_snake_case_on_wire() {
        assert_eq!(
            astrs_yaml::to_string(&RtPolicy::Fifo).unwrap().trim(),
            "fifo"
        );
        assert_eq!(astrs_yaml::to_string(&RtPolicy::Rr).unwrap().trim(), "rr");
        assert_eq!(
            astrs_yaml::to_string(&RtPolicy::Normal).unwrap().trim(),
            "normal"
        );
        let parsed: RtPolicy = astrs_yaml::from_str("rr").unwrap();
        assert_eq!(parsed, RtPolicy::Rr);
    }

    #[test]
    fn policy_rejects_unknown_values_and_sched_constant_spellings() {
        assert!(astrs_yaml::from_str::<RtPolicy>("deadline").is_err());
        assert!(astrs_yaml::from_str::<RtPolicy>("SCHED_FIFO").is_err());
        assert!(astrs_yaml::from_str::<RtPolicy>("FIFO").is_err());
    }

    #[test]
    fn parses_the_full_block() {
        let rt: RtConfig = astrs_yaml::from_str("policy: fifo\npriority: 80\n").unwrap();
        assert_eq!(rt, RtConfig::new(RtPolicy::Fifo, 80));
    }

    #[test]
    fn an_omitted_policy_defaults_to_normal() {
        let rt: RtConfig = astrs_yaml::from_str("priority: 80\n").unwrap();
        assert_eq!(rt.policy, RtPolicy::Normal);
        // Parsing accepts this shape; `Manifest::validate` is what rejects
        // it (a priority with no real-time policy to carry it).
        assert_eq!(rt.priority, Some(80));
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(astrs_yaml::from_str::<RtConfig>("policy: fifo\nnice: -5\n").is_err());
    }

    #[test]
    fn rejects_a_priority_outside_a_u8() {
        assert!(astrs_yaml::from_str::<RtConfig>("policy: fifo\npriority: 300\n").is_err());
        assert!(astrs_yaml::from_str::<RtConfig>("policy: fifo\npriority: -1\n").is_err());
    }

    #[test]
    fn round_trips_and_omits_the_default_policy() {
        let rt = RtConfig {
            policy: RtPolicy::Normal,
            priority: None,
        };
        let yaml = astrs_yaml::to_string(&rt).unwrap();
        assert!(!yaml.contains("policy"), "yaml was: {yaml}");
        assert!(!yaml.contains("priority"), "yaml was: {yaml}");
        assert_eq!(astrs_yaml::from_str::<RtConfig>(&yaml).unwrap(), rt);

        let realtime = RtConfig::new(RtPolicy::Rr, 42);
        let yaml = astrs_yaml::to_string(&realtime).unwrap();
        assert!(yaml.contains("policy: rr"), "yaml was: {yaml}");
        assert!(yaml.contains("priority: 42"), "yaml was: {yaml}");
        assert_eq!(astrs_yaml::from_str::<RtConfig>(&yaml).unwrap(), realtime);
    }

    #[test]
    fn the_realtime_priority_range_is_one_to_ninety_nine() {
        assert_eq!(RT_PRIORITY_MIN, 1);
        assert_eq!(RT_PRIORITY_MAX, 99);
        for priority in [RT_PRIORITY_MIN, 50, RT_PRIORITY_MAX] {
            assert!(
                RtConfig::new(RtPolicy::Fifo, priority).has_valid_realtime_priority(),
                "{priority}"
            );
        }
        assert!(!RtConfig::new(RtPolicy::Fifo, 0).has_valid_realtime_priority());
        assert!(!RtConfig::new(RtPolicy::Fifo, 100).has_valid_realtime_priority());
    }

    #[test]
    fn a_missing_priority_is_never_a_valid_realtime_priority() {
        let rt = RtConfig {
            policy: RtPolicy::Fifo,
            priority: None,
        };
        assert!(!rt.has_valid_realtime_priority());
    }

    #[test]
    fn display_is_the_manifest_spelling() {
        assert_eq!(RtPolicy::Normal.to_string(), "normal");
        assert_eq!(RtPolicy::Fifo.to_string(), "fifo");
        assert_eq!(RtPolicy::Rr.to_string(), "rr");
    }
}
