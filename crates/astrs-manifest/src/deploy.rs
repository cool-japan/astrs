//! Placement configuration, shared by the manifest root and each node
//! (blueprint §8.2 root `deploy`, §8.3 node `deploy: {machine, labels,
//! working_dir}`).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where (and with what metadata) a node should run.
///
/// Appears at the manifest root as the graph-wide default placement, and
/// on each [`crate::Node`] to override it. This crate does not merge the
/// two (that is `astrs-graph`'s placement-planner concern, per the crate
/// catalog, §5.2) — it only models the shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    /// The target machine id, matched against a daemon's registered name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Free-form placement labels (for future label-selector placement).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// A working directory override for this placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_machine_only() {
        let d: Deploy = astrs_yaml::from_str("machine: robot-1\n").unwrap();
        assert_eq!(d.machine.as_deref(), Some("robot-1"));
        assert!(d.labels.is_empty());
    }

    #[test]
    fn empty_deploy_round_trips_to_empty_map() {
        let d = Deploy::default();
        let yaml = astrs_yaml::to_string(&d).unwrap();
        assert_eq!(yaml.trim(), "{}");
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(astrs_yaml::from_str::<Deploy>("machine: x\nbogus: 1\n").is_err());
    }
}
