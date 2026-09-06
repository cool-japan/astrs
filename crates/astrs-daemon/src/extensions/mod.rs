//! The extension table — dataflow-scoped `(namespace, key) → bytes` (§2.1).
//!
//! > *The extension table: dataflow-scoped `(namespace, key) → bytes` with
//! > crash reclamation. Kept — it is the clean seam for GPU pools and
//! > third-party transports.*
//!
//! A node stores an opaque handle (a CUDA IPC handle, a third-party
//! transport's endpoint descriptor, a pinned-buffer registration) under a key
//! its peers already know, and the peers load it. The daemon never interprets
//! the bytes. What the daemon *does* provide is the part a node cannot: an
//! owner, and a reclamation path for when that owner dies without saying
//! goodbye (§3, principle 5).
//!
//! ```text
//!   camera  ──ExtStore(gpu_handle/frame_pool, 128 B)──►  daemon
//!   detect  ──ExtLoad (gpu_handle/frame_pool)────────►   daemon
//!           ◄─ExtValue(Some(128 B))──────────────────
//!   camera  ✗ crashes
//!   detect  ◄─ExtDropped{key, "owner camera exited"}──   daemon
//! ```
//!
//! # Who is told
//!
//! [`ExtensionTable::reclaim_owner`] returns the *interested parties* for
//! every dropped key: the owner is gone by definition, so the notification
//! goes to everyone who stored **or read** the key — the readers are exactly
//! the nodes holding a handle that has just become invalid, and telling only
//! the owner would leave them dereferencing freed memory.
//!
//! # Namespaces
//!
//! [`astrs_wire::ExtensionNamespace::User`] is writable by nodes; the rest
//! (`PinnedMemory`, `GpuHandle`, `Internal`) are reserved for the daemon and a
//! node's `ExtStore` into one is refused
//! ([`crate::DaemonError::ReservedNamespace`]). The daemon writes them on a
//! node's behalf once the corresponding subsystem exists; until then they are
//! reachable through [`ExtensionTable::store_as_daemon`], which is how the SHM
//! broker (stage 2) will publish segment descriptors.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::extensions::{ExtensionTable, StoreOutcome};
//! use astrs_wire::{ExtensionKey, ExtensionNamespace, NodeId};
//!
//! let mut table = ExtensionTable::new();
//! let camera = NodeId::new("camera")?;
//! let detect = NodeId::new("detect")?;
//! let key = ExtensionKey::user("frame_pool")?;
//!
//! assert_eq!(table.store(&camera, key.clone(), vec![1, 2, 3], None)?, StoreOutcome::Created);
//! assert_eq!(table.load(&detect, &key).map(<[u8]>::to_vec), Some(vec![1, 2, 3]));
//!
//! // The owner crashes: every party that touched the key is told.
//! let dropped = table.reclaim_owner(&camera);
//! assert_eq!(dropped.len(), 1);
//! assert!(dropped[0].interested.contains(&detect));
//! assert!(table.load(&detect, &key).is_none());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use astrs_wire::{ExtensionKey, ExtensionNamespace, NodeId};

use crate::error::{DaemonError, DaemonResult};

/// What a store did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreOutcome {
    /// The key did not exist.
    Created,
    /// The key existed and the owner replaced its value.
    Replaced,
    /// The key existed under a different owner, which is now the previous
    /// owner: the last writer owns the entry.
    ///
    /// Deliberately allowed rather than refused — a handoff (a node restarts
    /// and re-publishes its pool) is a normal thing to do, and the readers are
    /// told through the ordinary `ExtDropped`/re-read path. It is reported so
    /// the daemon can log it, because a *contended* key is usually a bug.
    OwnerChanged {
        /// Who owned it before.
        previous: NodeId,
    },
}

impl StoreOutcome {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Replaced => "replaced",
            Self::OwnerChanged { .. } => "owner_changed",
        }
    }
}

/// Why an entry went away.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropReason {
    /// A node dropped it explicitly.
    Requested {
        /// Who asked.
        by: NodeId,
    },
    /// Its owner's session ended (crash or clean exit).
    OwnerGone {
        /// The owner that went away.
        owner: NodeId,
    },
    /// Its time-to-live expired.
    Expired {
        /// The time-to-live it was stored with.
        ttl: Duration,
    },
    /// The dataflow it belonged to was destroyed.
    DataflowDestroyed,
}

impl core::fmt::Display for DropReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Requested { by } => write!(f, "dropped by {by}"),
            Self::OwnerGone { owner } => write!(f, "owner {owner} exited"),
            Self::Expired { ttl } => write!(f, "time-to-live of {} ms expired", ttl.as_millis()),
            Self::DataflowDestroyed => f.write_str("the dataflow was destroyed"),
        }
    }
}

/// One entry that went away, and who needs to hear about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedEntry {
    /// The key that is gone.
    pub key: ExtensionKey,
    /// Why.
    pub reason: DropReason,
    /// Every node that stored or read it and is therefore holding a handle
    /// that just became invalid.
    pub interested: BTreeSet<NodeId>,
}

impl DroppedEntry {
    /// The `reason` string carried in [`astrs_wire::NodeEvent::ExtDropped`].
    #[must_use]
    pub fn reason_text(&self) -> String {
        self.reason.to_string()
    }
}

/// One stored value.
#[derive(Debug, Clone)]
struct Entry {
    /// The bytes, uninterpreted.
    value: Vec<u8>,
    /// Who owns the entry — the last node to store it.
    owner: NodeId,
    /// Everyone who has stored or read it.
    interested: BTreeSet<NodeId>,
    /// When it was stored.
    stored_at: Instant,
    /// Its time-to-live, if any.
    ttl: Option<Duration>,
}

impl Entry {
    /// Whether the entry has outlived its time-to-live at `now`.
    fn is_expired(&self, now: Instant) -> bool {
        match self.ttl {
            Some(ttl) => now.duration_since(self.stored_at) >= ttl,
            None => false,
        }
    }
}

/// The per-dataflow extension table.
#[derive(Debug, Default)]
pub struct ExtensionTable {
    /// The entries, ordered so iteration and diagnostics are deterministic.
    entries: BTreeMap<ExtensionKey, Entry>,
}

impl ExtensionTable {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// How many entries the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many bytes the table holds, for the memory gauge (§13).
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.entries.values().map(|entry| entry.value.len()).sum()
    }

    /// The keys currently stored.
    pub fn keys(&self) -> impl Iterator<Item = &ExtensionKey> {
        self.entries.keys()
    }

    /// The owner of a key, if it exists.
    #[must_use]
    pub fn owner(&self, key: &ExtensionKey) -> Option<&NodeId> {
        self.entries.get(key).map(|entry| &entry.owner)
    }

    /// Stores a value on a node's behalf, honouring the reserved namespaces.
    ///
    /// # Errors
    ///
    /// [`DaemonError::ReservedNamespace`] if `key` is in a namespace nodes may
    /// not write.
    pub fn store(
        &mut self,
        owner: &NodeId,
        key: ExtensionKey,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> DaemonResult<StoreOutcome> {
        if !key.is_writable_by_node() {
            return Err(DaemonError::ReservedNamespace {
                namespace: key.namespace,
            });
        }
        Ok(self.store_as_daemon(owner, key, value, ttl))
    }

    /// Stores a value without the namespace check.
    ///
    /// The daemon's own path: the SHM broker publishing a segment descriptor
    /// under [`ExtensionNamespace::Internal`] is not a node writing a reserved
    /// namespace, it is the owner of that namespace using it.
    pub fn store_as_daemon(
        &mut self,
        owner: &NodeId,
        key: ExtensionKey,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> StoreOutcome {
        self.store_at(owner, key, value, ttl, Instant::now())
    }

    /// [`ExtensionTable::store_as_daemon`] with an explicit clock reading.
    pub fn store_at(
        &mut self,
        owner: &NodeId,
        key: ExtensionKey,
        value: Vec<u8>,
        ttl: Option<Duration>,
        now: Instant,
    ) -> StoreOutcome {
        match self.entries.get_mut(&key) {
            Some(entry) => {
                let outcome = if entry.owner == *owner {
                    StoreOutcome::Replaced
                } else {
                    StoreOutcome::OwnerChanged {
                        previous: entry.owner.clone(),
                    }
                };
                entry.value = value;
                entry.owner = owner.clone();
                entry.interested.insert(owner.clone());
                entry.stored_at = now;
                entry.ttl = ttl;
                outcome
            }
            None => {
                let mut interested = BTreeSet::new();
                interested.insert(owner.clone());
                self.entries.insert(
                    key,
                    Entry {
                        value,
                        owner: owner.clone(),
                        interested,
                        stored_at: now,
                        ttl,
                    },
                );
                StoreOutcome::Created
            }
        }
    }

    /// Reads a value, recording `reader` as an interested party.
    ///
    /// Recording the read is what makes crash reclamation useful: a reader
    /// that never stored anything still holds a handle, and must be told when
    /// it dies.
    pub fn load(&mut self, reader: &NodeId, key: &ExtensionKey) -> Option<&[u8]> {
        let entry = self.entries.get_mut(key)?;
        entry.interested.insert(reader.clone());
        Some(&entry.value)
    }

    /// Reads a value without recording interest — a diagnostic path.
    #[must_use]
    pub fn peek(&self, key: &ExtensionKey) -> Option<&[u8]> {
        self.entries.get(key).map(|entry| entry.value.as_slice())
    }

    /// Drops a key at a node's request.
    ///
    /// Returns the dropped entry when the key existed *and* `by` was allowed
    /// to drop it — only the owner may. A non-owner's request is ignored
    /// rather than refused, because the common cause is a race with a handoff
    /// and there is nothing for the caller to do about it.
    pub fn drop_key(&mut self, by: &NodeId, key: &ExtensionKey) -> Option<DroppedEntry> {
        let entry = self.entries.get(key)?;
        if entry.owner != *by {
            return None;
        }
        let entry = self.entries.remove(key)?;
        Some(DroppedEntry {
            key: key.clone(),
            reason: DropReason::Requested { by: by.clone() },
            interested: entry.interested,
        })
    }

    /// Reclaims everything `owner` owned — the crash path.
    ///
    /// Called when a node's session ends, whatever the reason: a clean exit
    /// and a `SIGSEGV` leave exactly the same dangling handles behind.
    pub fn reclaim_owner(&mut self, owner: &NodeId) -> Vec<DroppedEntry> {
        let keys: Vec<ExtensionKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.owner == *owner)
            .map(|(key, _)| key.clone())
            .collect();

        keys.into_iter()
            .filter_map(|key| {
                let entry = self.entries.remove(&key)?;
                Some(DroppedEntry {
                    key,
                    reason: DropReason::OwnerGone {
                        owner: owner.clone(),
                    },
                    interested: entry.interested,
                })
            })
            .collect()
    }

    /// Drops every entry whose time-to-live has expired at `now`.
    pub fn expire(&mut self, now: Instant) -> Vec<DroppedEntry> {
        let keys: Vec<ExtensionKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.is_expired(now))
            .map(|(key, _)| key.clone())
            .collect();

        keys.into_iter()
            .filter_map(|key| {
                let entry = self.entries.remove(&key)?;
                let ttl = entry.ttl.unwrap_or_default();
                Some(DroppedEntry {
                    key,
                    reason: DropReason::Expired { ttl },
                    interested: entry.interested,
                })
            })
            .collect()
    }

    /// Drops everything — the dataflow is being destroyed.
    pub fn clear(&mut self) -> Vec<DroppedEntry> {
        let entries = std::mem::take(&mut self.entries);
        entries
            .into_iter()
            .map(|(key, entry)| DroppedEntry {
                key,
                reason: DropReason::DataflowDestroyed,
                interested: entry.interested,
            })
            .collect()
    }

    /// A per-namespace count, for the diagnostics table.
    #[must_use]
    pub fn counts_by_namespace(&self) -> BTreeMap<ExtensionNamespace, usize> {
        let mut counts = BTreeMap::new();
        for key in self.entries.keys() {
            *counts.entry(key.namespace).or_insert(0) += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn key(name: &str) -> ExtensionKey {
        ExtensionKey::user(name).unwrap()
    }

    fn internal(name: &str) -> ExtensionKey {
        ExtensionKey::new(ExtensionNamespace::Internal, name).unwrap()
    }

    #[test]
    fn an_empty_table_reports_itself_empty() {
        let table = ExtensionTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.total_bytes(), 0);
        assert!(table.peek(&key("nothing")).is_none());
    }

    #[test]
    fn storing_and_loading_round_trips() {
        let mut table = ExtensionTable::new();
        assert_eq!(
            table
                .store(&node("a"), key("k"), vec![1, 2, 3], None)
                .unwrap(),
            StoreOutcome::Created
        );
        assert_eq!(table.load(&node("b"), &key("k")).unwrap(), &[1, 2, 3]);
        assert_eq!(table.len(), 1);
        assert_eq!(table.total_bytes(), 3);
        assert_eq!(table.owner(&key("k")), Some(&node("a")));
    }

    #[test]
    fn a_node_may_not_write_a_reserved_namespace() {
        let mut table = ExtensionTable::new();
        let error = table
            .store(&node("a"), internal("k"), vec![1], None)
            .unwrap_err();
        assert!(
            matches!(error, DaemonError::ReservedNamespace { .. }),
            "{error}"
        );
        assert!(table.is_empty());
    }

    #[test]
    fn the_daemon_may_write_a_reserved_namespace() {
        let mut table = ExtensionTable::new();
        assert_eq!(
            table.store_as_daemon(&node("a"), internal("k"), vec![1], None),
            StoreOutcome::Created
        );
        assert_eq!(table.peek(&internal("k")), Some([1].as_slice()));
        assert_eq!(
            table
                .counts_by_namespace()
                .get(&ExtensionNamespace::Internal),
            Some(&1)
        );
    }

    #[test]
    fn a_second_store_by_the_owner_replaces() {
        let mut table = ExtensionTable::new();
        table.store(&node("a"), key("k"), vec![1], None).unwrap();
        assert_eq!(
            table.store(&node("a"), key("k"), vec![2], None).unwrap(),
            StoreOutcome::Replaced
        );
        assert_eq!(table.peek(&key("k")), Some([2].as_slice()));
    }

    #[test]
    fn a_store_by_another_node_hands_ownership_over_and_says_so() {
        let mut table = ExtensionTable::new();
        table.store(&node("a"), key("k"), vec![1], None).unwrap();
        assert_eq!(
            table.store(&node("b"), key("k"), vec![2], None).unwrap(),
            StoreOutcome::OwnerChanged {
                previous: node("a")
            }
        );
        assert_eq!(table.owner(&key("k")), Some(&node("b")));
    }

    #[test]
    fn only_the_owner_may_drop() {
        let mut table = ExtensionTable::new();
        table.store(&node("a"), key("k"), vec![1], None).unwrap();
        assert!(table.drop_key(&node("b"), &key("k")).is_none());
        assert_eq!(table.len(), 1);

        let dropped = table.drop_key(&node("a"), &key("k")).unwrap();
        assert_eq!(dropped.reason, DropReason::Requested { by: node("a") });
        assert!(table.is_empty());
    }

    #[test]
    fn dropping_an_absent_key_is_not_an_error() {
        let mut table = ExtensionTable::new();
        assert!(table.drop_key(&node("a"), &key("missing")).is_none());
    }

    #[test]
    fn reclamation_tells_readers_as_well_as_the_owner() {
        let mut table = ExtensionTable::new();
        table
            .store(&node("camera"), key("pool"), vec![7], None)
            .unwrap();
        table.load(&node("detect"), &key("pool"));
        table.load(&node("record"), &key("pool"));

        let dropped = table.reclaim_owner(&node("camera"));
        assert_eq!(dropped.len(), 1);
        let entry = &dropped[0];
        assert_eq!(entry.key, key("pool"));
        assert_eq!(
            entry.reason,
            DropReason::OwnerGone {
                owner: node("camera")
            }
        );
        assert_eq!(
            entry.interested,
            BTreeSet::from([node("camera"), node("detect"), node("record")])
        );
        assert!(entry.reason_text().contains("camera"));
        assert!(table.is_empty());
    }

    #[test]
    fn reclamation_leaves_other_owners_alone() {
        let mut table = ExtensionTable::new();
        table.store(&node("a"), key("ka"), vec![1], None).unwrap();
        table.store(&node("b"), key("kb"), vec![2], None).unwrap();

        let dropped = table.reclaim_owner(&node("a"));
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].key, key("ka"));
        assert_eq!(table.len(), 1);
        assert_eq!(table.peek(&key("kb")), Some([2].as_slice()));
    }

    #[test]
    fn reclaiming_a_node_that_owns_nothing_drops_nothing() {
        let mut table = ExtensionTable::new();
        table.store(&node("a"), key("k"), vec![1], None).unwrap();
        table.load(&node("b"), &key("k"));
        assert!(table.reclaim_owner(&node("b")).is_empty());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn a_time_to_live_expires_the_entry() {
        let mut table = ExtensionTable::new();
        let start = Instant::now();
        table.store_at(
            &node("a"),
            key("short"),
            vec![1],
            Some(Duration::from_millis(50)),
            start,
        );
        table.store_at(&node("a"), key("forever"), vec![2], None, start);

        assert!(table.expire(start).is_empty(), "not yet");
        let dropped = table.expire(start + Duration::from_millis(50));
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].key, key("short"));
        assert_eq!(
            dropped[0].reason,
            DropReason::Expired {
                ttl: Duration::from_millis(50)
            }
        );
        assert_eq!(table.len(), 1, "the entry with no ttl survives");
    }

    #[test]
    fn re_storing_restarts_the_time_to_live() {
        let mut table = ExtensionTable::new();
        let start = Instant::now();
        let ttl = Duration::from_millis(50);
        table.store_at(&node("a"), key("k"), vec![1], Some(ttl), start);
        table.store_at(
            &node("a"),
            key("k"),
            vec![2],
            Some(ttl),
            start + Duration::from_millis(40),
        );
        assert!(
            table.expire(start + Duration::from_millis(60)).is_empty(),
            "the clock restarted at the second store"
        );
        assert_eq!(table.expire(start + Duration::from_millis(95)).len(), 1);
    }

    #[test]
    fn clearing_reports_every_entry_as_destroyed() {
        let mut table = ExtensionTable::new();
        table.store(&node("a"), key("k1"), vec![1], None).unwrap();
        table.store(&node("b"), key("k2"), vec![2], None).unwrap();
        let dropped = table.clear();
        assert_eq!(dropped.len(), 2);
        for entry in &dropped {
            assert_eq!(entry.reason, DropReason::DataflowDestroyed);
        }
        assert!(table.is_empty());
    }

    #[test]
    fn keys_iterate_in_a_deterministic_order() {
        let mut table = ExtensionTable::new();
        for name in ["c", "a", "b"] {
            table.store(&node("n"), key(name), vec![0], None).unwrap();
        }
        let names: Vec<&str> = table.keys().map(|key| key.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn outcomes_have_stable_labels() {
        assert_eq!(StoreOutcome::Created.kind_name(), "created");
        assert_eq!(StoreOutcome::Replaced.kind_name(), "replaced");
        assert_eq!(
            StoreOutcome::OwnerChanged {
                previous: node("a")
            }
            .kind_name(),
            "owner_changed"
        );
    }

    #[test]
    fn drop_reasons_render_readably() {
        for reason in [
            DropReason::Requested { by: node("a") },
            DropReason::OwnerGone { owner: node("a") },
            DropReason::Expired {
                ttl: Duration::from_millis(5),
            },
            DropReason::DataflowDestroyed,
        ] {
            assert!(!reason.to_string().is_empty(), "{reason:?}");
        }
    }
}
