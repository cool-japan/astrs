//! [`AttachmentLedger`] — who has mapped which ring (§6.3).
//!
//! > *This removes dora's ack-window heuristics — the daemon **knows**
//! > attachment state because it brokers the segment fds.*
//!
//! Knowing is this module. For every output with a segment, the ledger holds
//! the set of consumers that are *expected* to attach (decided by
//! [`crate::shm::ShmPolicy`]) and the set that actually *has* (observed from
//! the segment's own consumer table). The upgrade condition — every expected
//! consumer attached — is then a set comparison rather than a timer.
//!
//! # Observation, not notification
//!
//! There is no "I attached" message, and there does not need to be. A consumer
//! that maps a segment claims an entry in its consumer table
//! ([`astrs_shm::ConsumerEntry`]) and writes its pid into it, in shared memory
//! the daemon already has mapped. [`AttachmentLedger::sync`] takes the pid set
//! the daemon read out of that table, translated to node ids, and reports what
//! changed since last time.
//!
//! That is strictly better than a message: a consumer that dies takes its
//! entry with it (the producer's eviction sweep clears it), so a *detach* is
//! observed on exactly the same path as an attach, with no crash-time
//! goodbye to miss.
//!
//! # Why the expected set is stored rather than recomputed
//!
//! A consumer can be removed from the graph (`astrs graph remove-edge`, a
//! dynamic node leaving) between one sync and the next. Keeping the expected
//! set here means "all attached" is answered against the set the *offer* was
//! made for, so a route cannot be upgraded on the strength of a consumer that
//! is no longer part of the dataflow.
//!
//! # Examples
//!
//! ```
//! use std::collections::BTreeMap;
//! use astrs_daemon::shm::{AttachmentLedger, OutputKey};
//! use astrs_wire::{DataId, DataflowId, NodeId};
//!
//! let key = OutputKey::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     DataId::new("image")?,
//! );
//! let detect = NodeId::new("detect")?;
//! let record = NodeId::new("record")?;
//!
//! let mut ledger = AttachmentLedger::new();
//! ledger.expect(key.clone(), [detect.clone(), record.clone()]);
//! assert!(!ledger.all_attached(&key));
//!
//! ledger.sync(&key, BTreeMap::from([(detect.clone(), 10)]));
//! assert_eq!(ledger.missing(&key), vec![record.clone()]);
//!
//! let delta = ledger.sync(&key, BTreeMap::from([(detect, 10), (record, 11)]));
//! assert_eq!(delta.attached.len(), 1);
//! assert!(ledger.all_attached(&key));
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};

use astrs_wire::{DataflowId, NodeId};

use crate::shm::keys::OutputKey;

/// What changed between two observations of one segment's consumer table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentDelta {
    /// Consumers that appeared.
    pub attached: Vec<NodeId>,
    /// Consumers that disappeared.
    pub detached: Vec<NodeId>,
}

impl AttachmentDelta {
    /// Whether anything changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attached.is_empty() && self.detached.is_empty()
    }

    /// Whether a consumer went away — the edge that forces a downgrade (§6.3).
    #[must_use]
    pub fn lost_a_consumer(&self) -> bool {
        !self.detached.is_empty()
    }
}

/// The expected and observed consumers of one output's ring.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OutputAttachments {
    /// Consumers the policy says should attach.
    expected: BTreeSet<NodeId>,
    /// Consumers observed in the segment's table, with their pids.
    attached: BTreeMap<NodeId, u32>,
}

impl OutputAttachments {
    /// Whether every expected consumer has attached.
    ///
    /// An empty expected set answers `false`: an output nobody consumes has no
    /// reason to be upgraded, and answering `true` would upgrade every
    /// unconnected output in the graph.
    fn all_attached(&self) -> bool {
        !self.expected.is_empty()
            && self
                .expected
                .iter()
                .all(|node| self.attached.contains_key(node))
    }
}

/// Attachment state for every brokered output.
#[derive(Debug, Clone, Default)]
pub struct AttachmentLedger {
    /// One entry per output with a segment.
    entries: BTreeMap<OutputKey, OutputAttachments>,
}

impl AttachmentLedger {
    /// An empty ledger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Replaces the set of consumers expected to attach to `key`'s ring.
    pub fn expect<I>(&mut self, key: OutputKey, consumers: I)
    where
        I: IntoIterator<Item = NodeId>,
    {
        let entry = self.entries.entry(key).or_default();
        entry.expected = consumers.into_iter().collect();
        // A consumer that is no longer expected is no longer *tracked*, but a
        // consumer that is expected and already attached keeps its record: the
        // expected set changing must not look like a mass detach.
        entry
            .attached
            .retain(|node, _| entry.expected.contains(node));
    }

    /// Adds one consumer to the expected set.
    pub fn add_expected(&mut self, key: OutputKey, consumer: NodeId) -> bool {
        self.entries
            .entry(key)
            .or_default()
            .expected
            .insert(consumer)
    }

    /// Removes one consumer from the expected set and from the observations.
    pub fn remove_expected(&mut self, key: &OutputKey, consumer: &NodeId) -> bool {
        match self.entries.get_mut(key) {
            Some(entry) => {
                entry.attached.remove(consumer);
                entry.expected.remove(consumer)
            }
            None => false,
        }
    }

    /// The consumers expected to attach to `key`'s ring.
    #[must_use]
    pub fn expected(&self, key: &OutputKey) -> Vec<NodeId> {
        self.entries
            .get(key)
            .map(|entry| entry.expected.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The consumers observed attached to `key`'s ring.
    #[must_use]
    pub fn attached(&self, key: &OutputKey) -> Vec<NodeId> {
        self.entries
            .get(key)
            .map(|entry| entry.attached.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The expected consumers that have not attached yet.
    #[must_use]
    pub fn missing(&self, key: &OutputKey) -> Vec<NodeId> {
        self.entries
            .get(key)
            .map(|entry| {
                entry
                    .expected
                    .iter()
                    .filter(|node| !entry.attached.contains_key(*node))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The pid one attached consumer mapped from.
    #[must_use]
    pub fn pid_of(&self, key: &OutputKey, consumer: &NodeId) -> Option<u32> {
        self.entries
            .get(key)
            .and_then(|entry| entry.attached.get(consumer))
            .copied()
    }

    /// Whether every expected consumer of `key` has attached (§6.3).
    #[must_use]
    pub fn all_attached(&self, key: &OutputKey) -> bool {
        self.entries
            .get(key)
            .is_some_and(OutputAttachments::all_attached)
    }

    /// Folds one observation of `key`'s consumer table into the ledger.
    ///
    /// `present` is node → pid for every consumer currently in the table.
    /// Returns what changed, which is what the upgrade state machine reacts
    /// to: an addition may complete the set, a removal always breaks it.
    pub fn sync(&mut self, key: &OutputKey, present: BTreeMap<NodeId, u32>) -> AttachmentDelta {
        let Some(entry) = self.entries.get_mut(key) else {
            return AttachmentDelta::default();
        };
        let mut delta = AttachmentDelta::default();
        for (node, pid) in &present {
            if !entry.expected.contains(node) {
                // Somebody attached that the policy never expected — a
                // recording node, a debugger. It is not tracked, and it is not
                // an error: the ring is capability-brokered, so anything with
                // a descriptor was given one deliberately.
                continue;
            }
            if entry.attached.insert(node.clone(), *pid).is_none() {
                delta.attached.push(node.clone());
            }
        }
        let gone: Vec<NodeId> = entry
            .attached
            .keys()
            .filter(|node| !present.contains_key(*node))
            .cloned()
            .collect();
        for node in gone {
            entry.attached.remove(&node);
            delta.detached.push(node);
        }
        delta
    }

    /// Forgets everything about one output.
    pub fn forget(&mut self, key: &OutputKey) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Forgets every output of one dataflow.
    pub fn forget_dataflow(&mut self, dataflow: DataflowId) -> usize {
        let before = self.entries.len();
        self.entries.retain(|key, _| key.dataflow != dataflow);
        before - self.entries.len()
    }

    /// Forgets every output one node produces.
    pub fn forget_producer(&mut self, dataflow: DataflowId, node: &NodeId) -> Vec<OutputKey> {
        let keys: Vec<OutputKey> = self
            .entries
            .keys()
            .filter(|key| key.dataflow == dataflow && key.node == *node)
            .cloned()
            .collect();
        for key in &keys {
            self.entries.remove(key);
        }
        keys
    }

    /// Drops one consumer from every output it took part in.
    ///
    /// The path a consumer's crash takes: it is no longer expected, so a route
    /// whose only remaining consumers have attached can still be upgraded, and
    /// one that was already upgraded is downgraded by the caller acting on the
    /// returned keys.
    pub fn forget_consumer(&mut self, dataflow: DataflowId, consumer: &NodeId) -> Vec<OutputKey> {
        let mut touched = Vec::new();
        for (key, entry) in &mut self.entries {
            if key.dataflow != dataflow {
                continue;
            }
            let was_attached = entry.attached.remove(consumer).is_some();
            let was_expected = entry.expected.remove(consumer);
            if was_attached || was_expected {
                touched.push(key.clone());
            }
        }
        touched
    }

    /// How many outputs the ledger tracks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether an output is tracked at all.
    #[must_use]
    pub fn tracks(&self, key: &OutputKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Every tracked output, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &OutputKey> {
        self.entries.keys()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::DataId;

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    fn key(output: &str) -> OutputKey {
        OutputKey::new(
            dataflow(),
            NodeId::new("camera").unwrap(),
            DataId::new(output).unwrap(),
        )
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn ledger() -> (AttachmentLedger, OutputKey) {
        let mut ledger = AttachmentLedger::new();
        let key = key("image");
        ledger.expect(key.clone(), [node("detect"), node("record")]);
        (ledger, key)
    }

    #[test]
    fn an_empty_ledger_tracks_nothing() {
        let ledger = AttachmentLedger::new();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
        assert!(!ledger.tracks(&key("image")));
        assert!(!ledger.all_attached(&key("image")));
        assert!(ledger.expected(&key("image")).is_empty());
        assert!(ledger.attached(&key("image")).is_empty());
        assert!(ledger.missing(&key("image")).is_empty());
    }

    #[test]
    fn an_output_nobody_consumes_is_never_fully_attached() {
        let mut ledger = AttachmentLedger::new();
        ledger.expect(key("image"), []);
        assert!(ledger.tracks(&key("image")));
        assert!(!ledger.all_attached(&key("image")));
    }

    #[test]
    fn the_upgrade_condition_is_every_expected_consumer() {
        let (mut ledger, key) = ledger();
        assert_eq!(ledger.expected(&key), vec![node("detect"), node("record")]);
        assert!(!ledger.all_attached(&key));

        ledger.sync(&key, BTreeMap::from([(node("detect"), 10)]));
        assert!(!ledger.all_attached(&key));
        assert_eq!(ledger.missing(&key), vec![node("record")]);

        ledger.sync(
            &key,
            BTreeMap::from([(node("detect"), 10), (node("record"), 11)]),
        );
        assert!(ledger.all_attached(&key));
        assert!(ledger.missing(&key).is_empty());
    }

    #[test]
    fn a_sync_reports_only_what_changed() {
        let (mut ledger, key) = ledger();
        let first = ledger.sync(&key, BTreeMap::from([(node("detect"), 10)]));
        assert_eq!(first.attached, vec![node("detect")]);
        assert!(first.detached.is_empty());
        assert!(!first.is_empty());

        let second = ledger.sync(&key, BTreeMap::from([(node("detect"), 10)]));
        assert!(second.is_empty(), "nothing changed");
    }

    #[test]
    fn a_disappearing_consumer_is_a_detach() {
        let (mut ledger, key) = ledger();
        ledger.sync(
            &key,
            BTreeMap::from([(node("detect"), 10), (node("record"), 11)]),
        );
        let delta = ledger.sync(&key, BTreeMap::from([(node("detect"), 10)]));

        assert_eq!(delta.detached, vec![node("record")]);
        assert!(delta.lost_a_consumer());
        assert!(!ledger.all_attached(&key));
    }

    #[test]
    fn an_unexpected_attacher_is_ignored_rather_than_tracked() {
        let (mut ledger, key) = ledger();
        let delta = ledger.sync(
            &key,
            BTreeMap::from([(node("detect"), 10), (node("recorder"), 99)]),
        );
        assert_eq!(delta.attached, vec![node("detect")]);
        assert_eq!(ledger.attached(&key), vec![node("detect")]);
    }

    #[test]
    fn a_sync_for_an_untracked_output_changes_nothing() {
        let mut ledger = AttachmentLedger::new();
        let delta = ledger.sync(&key("image"), BTreeMap::from([(node("detect"), 10)]));
        assert!(delta.is_empty());
        assert!(ledger.is_empty());
    }

    #[test]
    fn pids_are_remembered_per_consumer() {
        let (mut ledger, key) = ledger();
        ledger.sync(&key, BTreeMap::from([(node("detect"), 4242)]));
        assert_eq!(ledger.pid_of(&key, &node("detect")), Some(4242));
        assert_eq!(ledger.pid_of(&key, &node("record")), None);
    }

    #[test]
    fn re_expecting_keeps_the_attachments_that_still_apply() {
        let (mut ledger, key) = ledger();
        ledger.sync(
            &key,
            BTreeMap::from([(node("detect"), 10), (node("record"), 11)]),
        );
        ledger.expect(key.clone(), [node("detect")]);

        assert_eq!(ledger.attached(&key), vec![node("detect")]);
        assert!(
            ledger.all_attached(&key),
            "narrowing the expected set completes it rather than resetting it"
        );
    }

    #[test]
    fn expectations_can_be_added_and_removed_one_at_a_time() {
        let mut ledger = AttachmentLedger::new();
        let key = key("image");
        assert!(ledger.add_expected(key.clone(), node("detect")));
        assert!(!ledger.add_expected(key.clone(), node("detect")));
        ledger.sync(&key, BTreeMap::from([(node("detect"), 10)]));
        assert!(ledger.all_attached(&key));

        assert!(ledger.remove_expected(&key, &node("detect")));
        assert!(!ledger.remove_expected(&key, &node("detect")));
        assert!(ledger.attached(&key).is_empty());
        assert!(!ledger.all_attached(&key));
    }

    #[test]
    fn removing_an_expectation_from_an_untracked_output_is_a_no_op() {
        let mut ledger = AttachmentLedger::new();
        assert!(!ledger.remove_expected(&key("image"), &node("detect")));
    }

    #[test]
    fn a_consumer_crash_drops_it_from_every_output() {
        let mut ledger = AttachmentLedger::new();
        ledger.expect(key("image"), [node("detect")]);
        ledger.expect(key("depth"), [node("detect"), node("record")]);
        ledger.sync(&key("image"), BTreeMap::from([(node("detect"), 10)]));

        let touched = ledger.forget_consumer(dataflow(), &node("detect"));
        assert_eq!(touched.len(), 2);
        assert!(ledger.expected(&key("image")).is_empty());
        assert_eq!(ledger.expected(&key("depth")), vec![node("record")]);
    }

    #[test]
    fn a_consumer_crash_in_another_dataflow_touches_nothing() {
        let (mut ledger, _) = ledger();
        assert!(
            ledger
                .forget_consumer(DataflowId::from_u128(99), &node("detect"))
                .is_empty()
        );
    }

    #[test]
    fn a_producer_crash_forgets_all_of_its_outputs() {
        let mut ledger = AttachmentLedger::new();
        ledger.expect(key("image"), [node("detect")]);
        ledger.expect(key("depth"), [node("detect")]);
        ledger.expect(
            OutputKey::new(dataflow(), node("lidar"), DataId::new("points").unwrap()),
            [node("detect")],
        );

        let forgotten = ledger.forget_producer(dataflow(), &node("camera"));
        assert_eq!(forgotten.len(), 2);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn forgetting_removes_one_output() {
        let (mut ledger, key) = ledger();
        assert!(ledger.forget(&key));
        assert!(!ledger.forget(&key));
        assert!(ledger.is_empty());
    }

    #[test]
    fn forgetting_a_dataflow_leaves_the_others() {
        let mut ledger = AttachmentLedger::new();
        ledger.expect(key("image"), [node("detect")]);
        ledger.expect(
            OutputKey::new(
                DataflowId::from_u128(2),
                node("camera"),
                DataId::new("image").unwrap(),
            ),
            [node("detect")],
        );
        assert_eq!(ledger.forget_dataflow(dataflow()), 1);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.keys().count(), 1);
    }
}
