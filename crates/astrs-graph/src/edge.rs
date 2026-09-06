//! Dataflow edges: one wired input, and what feeds it.
//!
//! # Why an edge's key is `(consumer node, consumer input)`
//!
//! [`Node::inputs`](astrs_manifest::Node::inputs) is a
//! `BTreeMap<String, Input>` and [`Input`](astrs_manifest::Input) carries
//! exactly one `source` — the manifest schema makes fan-in
//! (two producers feeding one input) structurally unrepresentable. That
//! means a consumer's `(node, input)` pair is already a unique key for
//! "the thing that feeds this input," so [`EdgeKey`] uses it directly
//! instead of inventing a separate synthetic edge id. This is also what
//! makes [`crate::diff::diff`] correct as a pair of independent map diffs
//! (nodes by [`crate::NodeId`], edges by [`EdgeKey`]) rather than needing
//! graph-isomorphism matching.

use std::fmt;

use astrs_manifest::{DurationSecs, QueuePolicy};
use serde::{Deserialize, Serialize};

use crate::ids::{NodeId, PortName};

/// The stable identity of one edge: the consumer node and input it feeds.
///
/// See this module's top-level docs for why this pair is a valid primary
/// key rather than an arbitrary counter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EdgeKey {
    /// The node whose input this edge feeds.
    pub consumer: NodeId,
    /// The input name on `consumer` this edge feeds.
    pub input: PortName,
}

impl EdgeKey {
    /// Build an edge key from its consumer node and input name.
    #[must_use]
    pub fn new(consumer: NodeId, input: PortName) -> Self {
        Self { consumer, input }
    }
}

impl fmt::Display for EdgeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.consumer, self.input)
    }
}

/// Where an edge's data comes from: an ordinary producer output, or one of
/// the synthetic `astrs/...` virtual sources (blueprint §8.4).
///
/// Virtual sources are stored as their raw source string rather than a
/// parsed structure: this crate never schedules a timer or filters a log
/// stream (that is `astrs-scheduler`'s and `astrs-log`'s job per the crate
/// catalog, §5.2), so the only things it ever needs from a virtual source
/// are "what string do I show in a diagnostic/diagram" and "this edge has
/// no producer node, so it is always daemon-local, never a cross-machine
/// route" (§11.1: one timer wheel per daemon) — both are true of the raw
/// string as-is. Call [`astrs_manifest::recognize_virtual_source`] on the
/// string if a caller needs the parsed form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeSource {
    /// An ordinary `node/output` producer.
    NodeOutput {
        /// The producing node.
        node: NodeId,
        /// The output name on `node`.
        output: PortName,
    },
    /// A synthetic `astrs/...` virtual source, stored verbatim.
    Virtual(String),
}

impl EdgeSource {
    /// The producing node id, if this edge comes from an ordinary
    /// producer output rather than a virtual source.
    #[must_use]
    pub fn producer_node(&self) -> Option<&NodeId> {
        match self {
            Self::NodeOutput { node, .. } => Some(node),
            Self::Virtual(_) => None,
        }
    }

    /// Whether this edge is sourced from a virtual source rather than a
    /// declared node output.
    #[must_use]
    pub fn is_virtual(&self) -> bool {
        matches!(self, Self::Virtual(_))
    }
}

impl fmt::Display for EdgeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeOutput { node, output } => write!(f, "{node}.{output}"),
            Self::Virtual(source) => f.write_str(source),
        }
    }
}

/// Per-input queueing behavior, mirroring the fields
/// [`Input`](astrs_manifest::Input) bundles with its `source` (blueprint
/// §11.2) — kept on the edge rather than on [`crate::GraphNode`] because
/// the manifest declares them together with the wiring, not independently
/// the way `input_types`/`output_types` are declared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueConfig {
    /// The bounded queue depth for this input.
    pub size: u32,
    /// What happens when the queue is full.
    pub policy: QueuePolicy,
    /// An optional per-input delivery timeout.
    pub timeout: Option<DurationSecs>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            size: astrs_manifest::default_queue_size(),
            policy: QueuePolicy::default(),
            timeout: None,
        }
    }
}

/// One dataflow edge: a consumer input, what feeds it, and how it queues.
///
/// Stored in [`crate::DataflowGraph`] keyed by [`EdgeKey`]; see this
/// module's top-level docs for why that key is the consumer `(node,
/// input)` pair rather than a separate id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// What feeds this edge's consumer input.
    pub from: EdgeSource,
    /// This input's queueing behavior.
    pub queue: QueueConfig,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn edge_key_displays_dotted() {
        let key = EdgeKey::new(NodeId::new("detector"), PortName::new("frames"));
        assert_eq!(key.to_string(), "detector.frames");
    }

    #[test]
    fn edge_key_orders_by_consumer_then_input() {
        let a = EdgeKey::new(NodeId::new("a"), PortName::new("z"));
        let b = EdgeKey::new(NodeId::new("b"), PortName::new("a"));
        assert!(a < b);
    }

    #[test]
    fn node_output_producer_node_is_some() {
        let source = EdgeSource::NodeOutput {
            node: NodeId::new("camera"),
            output: PortName::new("frames"),
        };
        assert_eq!(source.producer_node(), Some(&NodeId::new("camera")));
        assert!(!source.is_virtual());
        assert_eq!(source.to_string(), "camera.frames");
    }

    #[test]
    fn virtual_source_has_no_producer_node() {
        let source = EdgeSource::Virtual("astrs/timer/hz/50".to_string());
        assert_eq!(source.producer_node(), None);
        assert!(source.is_virtual());
        assert_eq!(source.to_string(), "astrs/timer/hz/50");
    }

    #[test]
    fn queue_config_default_matches_manifest_default() {
        let q = QueueConfig::default();
        assert_eq!(q.size, 10);
        assert_eq!(q.policy, QueuePolicy::DropOldest);
        assert!(q.timeout.is_none());
    }

    #[test]
    fn edge_is_cloneable_and_comparable() {
        let e1 = Edge {
            from: EdgeSource::Virtual("astrs/status".to_string()),
            queue: QueueConfig::default(),
        };
        let e2 = e1.clone();
        assert_eq!(e1, e2);
    }
}
