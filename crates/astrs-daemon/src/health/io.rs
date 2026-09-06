//! [`NodeIoLedger`] — per-node **egress** accounting: how many messages and
//! how many bytes each output has actually produced (§6.2, §6.3, §13).
//!
//! # Why the daemon can count bytes it never copies
//!
//! §6.3's slow-start handshake means one output's traffic can be on either of
//! two planes, and the daemon sees the payload on both:
//!
//! | Plane | How the bytes reach the daemon |
//! |---|---|
//! | daemon-mediated (§6.3 stage 1, or a below-threshold payload) | `NodeRequest::SendMessage{Inline}` carries them |
//! | shared memory (§6.2, stage 2) | the daemon's own bridge reader drains the ring, because a tap, a remote consumer or a not-yet-upgraded consumer still needs them |
//!
//! Both paths converge on `Daemon::fan_out`, which is where this ledger is
//! written. One place, both planes, and no second copy of anything: the byte
//! count is taken from a slice the caller already owns.
//!
//! The one publish this cannot see is a shared-memory publish on an output
//! whose ring the daemon's bridge could not attach to — and that case already
//! has its own visible counter, `shm_fallback_total` (§6.2 makes pool
//! exhaustion *visible* on purpose), which travels beside these figures in
//! [`astrs_wire::NodeIoSample`].
//!
//! # Ingress lives elsewhere, deliberately
//!
//! Received bytes are counted by [`crate::local::NodeMailbox`], at the moment
//! a queue *accepts* a message. Counting them here instead would count what
//! was sent to a consumer rather than what reached it, and the difference
//! between those two numbers is exactly the queue-policy loss §11.2 exists to
//! make visible.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::health::NodeIoLedger;
//! use astrs_wire::{DataId, DataflowId, NodeId};
//!
//! let dataflow = DataflowId::from_u128(1);
//! let camera = NodeId::new("camera")?;
//! let image = DataId::new("image")?;
//!
//! let mut ledger = NodeIoLedger::new();
//! ledger.record_sent(dataflow, &camera, &image, 4_096);
//! ledger.record_sent(dataflow, &camera, &image, 2_048);
//!
//! let traffic = ledger.egress(dataflow, &camera).expect("recorded");
//! assert_eq!(traffic[&image].messages, 2);
//! assert_eq!(traffic[&image].bytes, 6_144);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;

use astrs_wire::{DataId, DataflowId, NodeId};

/// What one port has carried since the node's current incarnation started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortTraffic {
    /// Messages published on (or delivered to) the port.
    pub messages: u64,
    /// Payload bytes those messages carried.
    pub bytes: u64,
}

impl PortTraffic {
    /// Adds one message of `bytes` bytes.
    ///
    /// Saturating, not wrapping: a counter that has run for long enough to
    /// overflow `u64` should stick at the maximum rather than silently
    /// restart and make a rate calculation produce nonsense.
    pub const fn record(&mut self, bytes: u64) {
        self.messages = self.messages.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

/// One node's egress, and the incarnation it belongs to.
#[derive(Debug, Default)]
struct NodeEgress {
    /// The incarnation these totals were accumulated for.
    generation: u64,
    /// `output → traffic`.
    ports: BTreeMap<DataId, PortTraffic>,
}

/// Per-node, per-output egress totals.
///
/// Keyed by `(dataflow, node)` and reset per incarnation, so a restarted node
/// starts from zero — the same rule [`astrs_wire::NodeMetricsSample`]'s own
/// totals follow ("since the node registered"), and the one that keeps a rate
/// derived from two samples from spanning a restart and reading as a huge
/// negative-then-positive spike.
#[derive(Debug, Default)]
pub struct NodeIoLedger {
    /// `(dataflow, node) → that node's current incarnation's egress`.
    egress: BTreeMap<(DataflowId, NodeId), NodeEgress>,
}

impl NodeIoLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self {
            egress: BTreeMap::new(),
        }
    }

    /// Records one publish of `bytes` bytes on `output`.
    pub fn record_sent(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        output: &DataId,
        bytes: u64,
    ) {
        self.egress
            .entry((dataflow, node.clone()))
            .or_default()
            .ports
            .entry(output.clone())
            .or_default()
            .record(bytes);
    }

    /// Starts `generation`'s totals from zero, if they are not already.
    ///
    /// Called once per incarnation, from the node's registration — which is
    /// the epoch every total in §13 is measured from, so this is where the
    /// epoch is set rather than a second place that has to be kept in step
    /// with it. Idempotent: a node that re-registers on the same generation
    /// (a reconnect, not a restart) keeps what it has published.
    pub fn begin_incarnation(&mut self, dataflow: DataflowId, node: &NodeId, generation: u64) {
        let entry = self.egress.entry((dataflow, node.clone())).or_default();
        if entry.generation != generation {
            entry.generation = generation;
            entry.ports.clear();
        }
    }

    /// One node's egress, or `None` if it has published nothing.
    #[must_use]
    pub fn egress(
        &self,
        dataflow: DataflowId,
        node: &NodeId,
    ) -> Option<&BTreeMap<DataId, PortTraffic>> {
        self.egress
            .get(&(dataflow, node.clone()))
            .map(|entry| &entry.ports)
    }

    /// Message counts per output, in the shape
    /// [`astrs_wire::NodeMetricsSample::sent_total`] wants.
    #[must_use]
    pub fn sent_messages(&self, dataflow: DataflowId, node: &NodeId) -> BTreeMap<DataId, u64> {
        self.egress(dataflow, node)
            .map(|ports| {
                ports
                    .iter()
                    .map(|(id, traffic)| (id.clone(), traffic.messages))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Byte counts per output, in the shape
    /// [`astrs_wire::NodeIoSample::sent_bytes_total`] wants.
    #[must_use]
    pub fn sent_bytes(&self, dataflow: DataflowId, node: &NodeId) -> BTreeMap<DataId, u64> {
        self.egress(dataflow, node)
            .map(|ports| {
                ports
                    .iter()
                    .map(|(id, traffic)| (id.clone(), traffic.bytes))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forgets one node's totals, because a new incarnation is starting.
    pub fn forget_node(&mut self, dataflow: DataflowId, node: &NodeId) {
        self.egress.remove(&(dataflow, node.clone()));
    }

    /// Forgets every node of one dataflow.
    pub fn forget_dataflow(&mut self, dataflow: DataflowId) {
        self.egress.retain(|(id, _), _| *id != dataflow);
    }

    /// How many `(dataflow, node)` pairs have published at least once.
    #[must_use]
    pub fn len(&self) -> usize {
        self.egress.len()
    }

    /// Whether nothing has been published at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.egress.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ids() -> (DataflowId, NodeId, DataId) {
        (
            DataflowId::from_u128(1),
            NodeId::new("camera").unwrap(),
            DataId::new("image").unwrap(),
        )
    }

    #[test]
    fn a_fresh_ledger_knows_nothing() {
        let ledger = NodeIoLedger::new();
        let (dataflow, camera, _) = ids();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
        assert!(ledger.egress(dataflow, &camera).is_none());
        assert!(ledger.sent_messages(dataflow, &camera).is_empty());
        assert!(ledger.sent_bytes(dataflow, &camera).is_empty());
    }

    #[test]
    fn publishes_accumulate_per_port() {
        let (dataflow, camera, image) = ids();
        let mut ledger = NodeIoLedger::new();
        ledger.record_sent(dataflow, &camera, &image, 100);
        ledger.record_sent(dataflow, &camera, &image, 250);

        assert_eq!(ledger.sent_messages(dataflow, &camera)[&image], 2);
        assert_eq!(ledger.sent_bytes(dataflow, &camera)[&image], 350);
    }

    #[test]
    fn ports_of_one_node_are_counted_apart() {
        let (dataflow, camera, image) = ids();
        let thumbs = DataId::new("thumbnails").unwrap();
        let mut ledger = NodeIoLedger::new();
        ledger.record_sent(dataflow, &camera, &image, 4_096);
        ledger.record_sent(dataflow, &camera, &thumbs, 64);

        let bytes = ledger.sent_bytes(dataflow, &camera);
        assert_eq!(bytes[&image], 4_096);
        assert_eq!(bytes[&thumbs], 64);
        assert_eq!(ledger.len(), 1, "one node, two ports");
    }

    #[test]
    fn a_zero_byte_publish_still_counts_as_a_message() {
        // An empty payload is a real event — a heartbeat, a tick, a sentinel —
        // and a graph that sends nothing but those must not look idle.
        let (dataflow, camera, image) = ids();
        let mut ledger = NodeIoLedger::new();
        ledger.record_sent(dataflow, &camera, &image, 0);
        assert_eq!(ledger.sent_messages(dataflow, &camera)[&image], 1);
        assert_eq!(ledger.sent_bytes(dataflow, &camera)[&image], 0);
    }

    #[test]
    fn a_new_incarnation_starts_from_zero() {
        let (dataflow, camera, image) = ids();
        let mut ledger = NodeIoLedger::new();
        ledger.record_sent(dataflow, &camera, &image, 500);
        ledger.forget_node(dataflow, &camera);
        assert!(
            ledger.egress(dataflow, &camera).is_none(),
            "a restarted node's totals must not be inherited: a rate taken \
             across the restart would otherwise be meaningless"
        );
    }

    #[test]
    fn a_registration_on_a_new_generation_resets_the_totals() {
        let (dataflow, camera, image) = ids();
        let mut ledger = NodeIoLedger::new();
        ledger.begin_incarnation(dataflow, &camera, 0);
        ledger.record_sent(dataflow, &camera, &image, 900);

        ledger.begin_incarnation(dataflow, &camera, 1);
        assert!(
            ledger.sent_bytes(dataflow, &camera).is_empty(),
            "the restarted incarnation publishes on its own account"
        );
    }

    #[test]
    fn a_reconnect_on_the_same_generation_keeps_the_totals() {
        // A node that loses its socket and re-registers has not restarted:
        // its generation is unchanged, and zeroing here would make its
        // throughput graph drop to nothing for no reason the operator can see.
        let (dataflow, camera, image) = ids();
        let mut ledger = NodeIoLedger::new();
        ledger.begin_incarnation(dataflow, &camera, 3);
        ledger.record_sent(dataflow, &camera, &image, 900);
        ledger.begin_incarnation(dataflow, &camera, 3);
        assert_eq!(ledger.sent_bytes(dataflow, &camera)[&image], 900);
    }

    #[test]
    fn forgetting_a_dataflow_leaves_the_others_alone() {
        let (dataflow, camera, image) = ids();
        let other = DataflowId::from_u128(2);
        let mut ledger = NodeIoLedger::new();
        ledger.record_sent(dataflow, &camera, &image, 1);
        ledger.record_sent(other, &camera, &image, 2);

        ledger.forget_dataflow(dataflow);
        assert!(ledger.egress(dataflow, &camera).is_none());
        assert_eq!(ledger.sent_bytes(other, &camera)[&image], 2);
    }

    #[test]
    fn totals_saturate_rather_than_wrap() {
        let mut traffic = PortTraffic {
            messages: u64::MAX,
            bytes: u64::MAX,
        };
        traffic.record(10);
        assert_eq!(traffic.messages, u64::MAX);
        assert_eq!(traffic.bytes, u64::MAX);
    }
}
