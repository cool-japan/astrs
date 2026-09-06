//! [`OutputKey`] — the thing a shared-memory ring belongs to (§6.2).
//!
//! > *One ring per (producer, output); consumers attach read-only.*
//!
//! Everything in [`crate::shm`] is filed under this key: the segment, the set
//! of consumers waiting to attach, and the upgrade state machine. It is a
//! struct rather than a tuple because three of its four uses want to name the
//! parts, and because `(DataflowId, NodeId, DataId)` read at a call site is
//! indistinguishable from `(DataflowId, NodeId, NodeId)`.
//!
//! The generation is deliberately *not* part of the key. A restarted producer
//! keeps the same output — the ring behind it is replaced, and the
//! [`astrs_shm::SegmentKey`] that names the ring does carry the generation, so
//! stale mappings stay detectable (§6.2) — but the daemon's bookkeeping for
//! "camera's `image` output" survives the restart, which is what lets a
//! downgrade-then-upgrade cycle be a state transition rather than a fresh
//! entry.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::shm::OutputKey;
//! use astrs_wire::{DataflowId, PortRef};
//!
//! let port: PortRef = "camera/image".parse()?;
//! let key = OutputKey::from_port(DataflowId::from_u128(1), &port);
//!
//! assert_eq!(key.node.as_str(), "camera");
//! assert_eq!(key.output.as_str(), "image");
//! assert_eq!(key.port(), port);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use core::fmt;

use astrs_shm::SegmentKey;
use astrs_wire::{DataId, DataflowId, NodeId, PortRef};

/// One producer output, within one dataflow.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputKey {
    /// The dataflow the producer belongs to.
    pub dataflow: DataflowId,
    /// The producing node.
    pub node: NodeId,
    /// The output port.
    pub output: DataId,
}

impl OutputKey {
    /// The key for `node`'s `output` in `dataflow`.
    #[must_use]
    pub const fn new(dataflow: DataflowId, node: NodeId, output: DataId) -> Self {
        Self {
            dataflow,
            node,
            output,
        }
    }

    /// The key a producer port names.
    #[must_use]
    pub fn from_port(dataflow: DataflowId, port: &PortRef) -> Self {
        Self {
            dataflow,
            node: port.node().clone(),
            output: port.port().clone(),
        }
    }

    /// The producer port this key names.
    #[must_use]
    pub fn port(&self) -> PortRef {
        PortRef::new(self.node.clone(), self.output.clone())
    }

    /// The segment key for one incarnation of this output (§6.2).
    #[must_use]
    pub fn segment_key(&self, generation: u64) -> SegmentKey {
        SegmentKey::new(
            self.dataflow,
            self.node.clone(),
            self.output.clone(),
            generation,
        )
    }

    /// Whether this key belongs to `dataflow`.
    #[must_use]
    pub fn in_dataflow(&self, dataflow: DataflowId) -> bool {
        self.dataflow == dataflow
    }

    /// Whether `node` produces this output.
    #[must_use]
    pub fn produced_by(&self, node: &NodeId) -> bool {
        self.node == *node
    }
}

impl fmt::Display for OutputKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.dataflow, self.node, self.output)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn key(node: &str, output: &str) -> OutputKey {
        OutputKey::new(
            DataflowId::from_u128(1),
            NodeId::new(node).unwrap(),
            DataId::new(output).unwrap(),
        )
    }

    #[test]
    fn a_key_round_trips_through_its_port() {
        let port = PortRef::from_parts("camera", "image").unwrap();
        let built = OutputKey::from_port(DataflowId::from_u128(1), &port);
        assert_eq!(built, key("camera", "image"));
        assert_eq!(built.port(), port);
    }

    #[test]
    fn keys_order_by_dataflow_then_node_then_output() {
        let mut keys = [
            key("camera", "image"),
            key("camera", "depth"),
            key("astar", "path"),
        ];
        keys.sort();
        assert_eq!(keys[0].node.as_str(), "astar");
        assert_eq!(keys[1].output.as_str(), "depth");
        assert_eq!(keys[2].output.as_str(), "image");
    }

    #[test]
    fn the_generation_lives_in_the_segment_key_not_the_output_key() {
        let key = key("camera", "image");
        let first = key.segment_key(1);
        let second = key.segment_key(2);
        assert_ne!(first.digest(), second.digest());
        assert_eq!(first.node(), second.node());
        assert!(first.canonical().ends_with("/camera/image/1"));
    }

    #[test]
    fn predicates_answer_about_membership() {
        let key = key("camera", "image");
        assert!(key.in_dataflow(DataflowId::from_u128(1)));
        assert!(!key.in_dataflow(DataflowId::from_u128(2)));
        assert!(key.produced_by(&NodeId::new("camera").unwrap()));
        assert!(!key.produced_by(&NodeId::new("detect").unwrap()));
    }

    #[test]
    fn the_display_form_names_all_three_parts() {
        let rendered = key("camera", "image").to_string();
        assert!(rendered.ends_with("/camera/image"), "{rendered}");
    }
}
