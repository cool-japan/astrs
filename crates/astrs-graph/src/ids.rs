//! Identifier newtypes shared across the graph model.
//!
//! [`NodeId`] and [`PortName`] wrap the plain `String`s a [`Manifest`] uses
//! for node ids and input/output names so the rest of this crate cannot
//! accidentally pass a port name where a node id is expected (or vice
//! versa) — a mistake the compiler catches instead of a runtime lookup
//! silently missing. [`MachineId`] resolves the manifest's two-level
//! `deploy: {machine}` override chain (graph-wide default, then per-node)
//! into one closed representation with an explicit "unset" state, per
//! blueprint §5.2's placement-planner responsibility (the manifest crate
//! deliberately does not merge these two levels — see [`Deploy`]'s docs).
//!
//! [`Manifest`]: astrs_manifest::Manifest
//! [`Deploy`]: astrs_manifest::Deploy

use std::fmt;

use astrs_manifest::Deploy;
use serde::{Deserialize, Serialize};

/// A dataflow node's identity, taken verbatim from [`Node::id`](astrs_manifest::Node::id).
///
/// A validated manifest guarantees node ids are unique and match
/// `[a-zA-Z0-9_.-]+` (see `astrs_manifest::Manifest::validate`); this type
/// does not re-check that — it is a typed wrapper, not a second validator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    /// Wrap a node id string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a plain string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for NodeId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A port name: an input or output name local to one [`crate::GraphNode`].
///
/// Port names are opaque, arbitrary strings in the manifest (unlike node
/// ids, they are not charset-restricted by `astrs-manifest`) — this type
/// exists purely to keep port names and node ids from being mixed up at
/// call sites, not to add validation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortName(String);

impl PortName {
    /// Wrap a port name string.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The port name as a plain string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PortName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for PortName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for PortName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for PortName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A resolved placement target: either the coordinator's own local daemon
/// (the manifest-wide default per blueprint §4.2 — `astrs run`'s
/// single-process mode has exactly one, in-process daemon) or a named
/// machine matched against a daemon's registered name at cluster time.
///
/// Built by [`MachineId::resolve`] from the two-level `deploy:` override
/// chain (graph-wide [`Manifest::deploy`](astrs_manifest::Manifest::deploy),
/// then [`Node::deploy`](astrs_manifest::Node::deploy)) — modeled as a
/// closed enum rather than `Option<String>` so "no machine configured
/// anywhere" is a distinct, named state ([`MachineId::CoordinatorLocal`])
/// instead of a magic sentinel string threaded through the planner and the
/// visualizers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MachineId {
    /// No `deploy: { machine: ... }` was set at either level; the node
    /// runs wherever the coordinator itself runs (blueprint §4.2's
    /// "default machine = coordinator-local").
    CoordinatorLocal,
    /// An explicit machine name, matched against a daemon's registered
    /// name by the coordinator at cluster time.
    Named(String),
}

impl MachineId {
    /// Resolve a node's effective machine placement.
    ///
    /// `node_deploy` (the node's own `deploy:` block) takes precedence
    /// over `graph_deploy` (the manifest root's `deploy:` block) on a
    /// field-by-field basis — mirroring
    /// [`Node::effective_env`](astrs_manifest::Node::effective_env)'s
    /// node-wins-on-conflict precedence for the same two-level override
    /// shape. Absent from both resolves to
    /// [`MachineId::CoordinatorLocal`].
    #[must_use]
    pub fn resolve(graph_deploy: Option<&Deploy>, node_deploy: Option<&Deploy>) -> Self {
        let machine = node_deploy
            .and_then(|d| d.machine.as_deref())
            .or_else(|| graph_deploy.and_then(|d| d.machine.as_deref()));
        match machine {
            Some(name) => Self::Named(name.to_string()),
            None => Self::CoordinatorLocal,
        }
    }

    /// Whether this is the coordinator-local placement (no explicit
    /// `machine:` anywhere in the override chain).
    #[must_use]
    pub fn is_coordinator_local(&self) -> bool {
        matches!(self, Self::CoordinatorLocal)
    }
}

impl fmt::Display for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoordinatorLocal => f.write_str("coordinator"),
            Self::Named(name) => f.write_str(name),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn node_id_displays_as_its_string() {
        let id = NodeId::new("camera");
        assert_eq!(id.to_string(), "camera");
        assert_eq!(id.as_str(), "camera");
    }

    #[test]
    fn node_id_orders_lexicographically() {
        assert!(NodeId::new("a") < NodeId::new("b"));
    }

    #[test]
    fn port_name_wraps_and_displays() {
        let name = PortName::new("frames");
        assert_eq!(name.as_str(), "frames");
        assert_eq!(name.to_string(), "frames");
    }

    #[test]
    fn machine_id_resolves_to_coordinator_local_when_unset() {
        assert_eq!(MachineId::resolve(None, None), MachineId::CoordinatorLocal);
        assert!(MachineId::resolve(None, None).is_coordinator_local());
    }

    #[test]
    fn machine_id_resolves_graph_default_when_node_unset() {
        let graph = Deploy {
            machine: Some("robot-1".to_string()),
            ..Deploy::default()
        };
        assert_eq!(
            MachineId::resolve(Some(&graph), None),
            MachineId::Named("robot-1".to_string())
        );
    }

    #[test]
    fn machine_id_node_override_wins() {
        let graph = Deploy {
            machine: Some("robot-1".to_string()),
            ..Deploy::default()
        };
        let node = Deploy {
            machine: Some("robot-2".to_string()),
            ..Deploy::default()
        };
        assert_eq!(
            MachineId::resolve(Some(&graph), Some(&node)),
            MachineId::Named("robot-2".to_string())
        );
    }

    #[test]
    fn machine_id_node_deploy_present_but_machine_unset_falls_back_to_graph() {
        let graph = Deploy {
            machine: Some("robot-1".to_string()),
            ..Deploy::default()
        };
        // A node `deploy:` block that only sets `working_dir`/`labels`
        // should not shadow the graph-wide machine with `None`.
        let node = Deploy {
            working_dir: Some("/opt/node".to_string()),
            ..Deploy::default()
        };
        assert_eq!(
            MachineId::resolve(Some(&graph), Some(&node)),
            MachineId::Named("robot-1".to_string())
        );
    }

    #[test]
    fn machine_id_display_matches_variant() {
        assert_eq!(MachineId::CoordinatorLocal.to_string(), "coordinator");
        assert_eq!(
            MachineId::Named("robot-1".to_string()).to_string(),
            "robot-1"
        );
    }
}
