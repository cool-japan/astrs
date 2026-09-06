//! The mutation log: sequence numbers, records and catch-up batches.
//!
//! Blueprint §12 / §2.2: a daemon that drops off the coordinator's heartbeat
//! for a while re-enters *degraded-autonomous* mode and, on reconnect, must
//! be resynchronised without re-sending the coordinator's entire state.
//! [`MutationRecord`] is the unit that makes that possible: every mutating
//! call on [`crate::CoordinatorStore`] appends exactly one, in the same
//! backend transaction as the bucket write it describes, so sequence numbers
//! can never gap or duplicate (see `crate::store` for how the transaction is
//! built). [`crate::CoordinatorStore::mutations_since`] hands a reconnecting
//! daemon everything it missed, in order, as a bounded [`CatchUpBatch`].

use std::fmt;

use astrs_time::HlcTimestamp;
use astrs_wire::{DaemonId, DataflowId, NodeId, ParamKey};
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::record::{
    BuildCacheEntry, BuildCacheKey, DaemonRecord, DataflowMeta, NodeStatusRecord, ParamRecord,
};

/// A mutation log position.
///
/// `0` is reserved to mean "nothing observed yet" — a daemon that has never
/// connected before asks for [`MutationSeq::ZERO`] and gets the log from its
/// very first entry, since real entries start numbering at `1`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(transparent)]
pub struct MutationSeq(u64);

impl MutationSeq {
    /// The reserved "nothing observed yet" value.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw sequence number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Adds `delta`, saturating at [`u64::MAX`] rather than wrapping.
    ///
    /// Used to compute bounded catch-up page windows; saturating means a
    /// pathological `max_entries` near [`usize::MAX`] near the end of the
    /// sequence space narrows the page instead of wrapping back to `0` and
    /// returning the wrong entries.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::record::MutationSeq;
    ///
    /// assert_eq!(MutationSeq::new(1).saturating_add(2), MutationSeq::new(3));
    /// assert_eq!(MutationSeq::new(u64::MAX).saturating_add(2), MutationSeq::new(u64::MAX));
    /// ```
    #[must_use]
    pub const fn saturating_add(self, delta: u64) -> Self {
        Self(self.0.saturating_add(delta))
    }
}

impl fmt::Display for MutationSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// The domain-level change one mutating [`crate::CoordinatorStore`] call
/// made, as logged by [`MutationRecord`].
///
/// Each `*Put` variant carries the **complete** new record rather than a
/// delta from its previous value, so replaying a record (see
/// [`crate::CoordinatorStore::apply_replayed`]) is always a plain overwrite:
/// no earlier state needs to be known or reconstructed first, which is what
/// lets a reconnecting daemon (or this crate's own tests) fold an arbitrary
/// batch of records into a bucket in one pass.
///
/// A variant only repeats a key field the wrapped record does not already
/// carry: `ParamRecord` has no id of its own so `ParamPut` states
/// `dataflow`/`key` explicitly, while `DataflowMeta` already carries `id`,
/// so `DataflowMetaPut` does not repeat it. Every `*Delete` variant states
/// its key explicitly since there is no record to derive one from.
///
/// `#[non_exhaustive]` and explicit `#[oxicode(variant = N)]` tags on every
/// variant mirror `astrs-wire`'s append-only discipline (blueprint design
/// principle 4): this log is durable state that must stay decodable across
/// a coordinator binary upgrade, so its variant indices are frozen exactly
/// like a wire enum's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[non_exhaustive]
pub enum MutationOp {
    /// A parameter was created or updated.
    #[oxicode(variant = 0)]
    ParamPut {
        /// The owning dataflow.
        dataflow: DataflowId,
        /// The parameter key.
        key: ParamKey,
        /// The new record.
        record: ParamRecord,
    },
    /// A parameter was deleted.
    #[oxicode(variant = 1)]
    ParamDelete {
        /// The owning dataflow.
        dataflow: DataflowId,
        /// The parameter key.
        key: ParamKey,
    },
    /// A dataflow was registered, or its metadata changed.
    #[oxicode(variant = 2)]
    DataflowMetaPut {
        /// The new record (`record.id` is the key).
        record: DataflowMeta,
    },
    /// A dataflow's registration was removed.
    #[oxicode(variant = 3)]
    DataflowMetaDelete {
        /// The dataflow removed.
        dataflow: DataflowId,
    },
    /// A node's status was created or updated.
    #[oxicode(variant = 4)]
    NodeStatusPut {
        /// The new record (`record.info.dataflow`/`record.info.node` are
        /// the key).
        record: NodeStatusRecord,
    },
    /// A node's status was removed.
    #[oxicode(variant = 5)]
    NodeStatusDelete {
        /// The owning dataflow.
        dataflow: DataflowId,
        /// The node removed.
        node: NodeId,
    },
    /// A daemon was registered, or its info changed (including a
    /// heartbeat).
    #[oxicode(variant = 6)]
    DaemonPut {
        /// The new record (`record.info.id` is the key).
        record: DaemonRecord,
    },
    /// A daemon's registration was removed.
    #[oxicode(variant = 7)]
    DaemonDelete {
        /// The daemon removed.
        daemon: DaemonId,
    },
    /// A build cache entry was created or updated.
    #[oxicode(variant = 8)]
    BuildCachePut {
        /// The new record (`record.hash` is the key).
        record: BuildCacheEntry,
    },
    /// A build cache entry was removed.
    #[oxicode(variant = 9)]
    BuildCacheDelete {
        /// The hash removed.
        hash: BuildCacheKey,
    },
    /// A node-scoped parameter was created or updated (§17 `param set
    /// --node`).
    ///
    /// A tail append beyond the original ten variants: the params bucket
    /// started dataflow-scoped only (`ParamPut`/`ParamDelete` above), and
    /// node scoping was added additively once the coordinator's `ParamScope`
    /// (`Global | Dataflow | Node`, `astrs-wire`) needed a third level that
    /// the two-component `(dataflow, key)` bucket key cannot express — see
    /// `crate::keys`'s `node_param_*` functions.
    #[oxicode(variant = 10)]
    NodeParamPut {
        /// The owning dataflow.
        dataflow: DataflowId,
        /// The owning node.
        node: NodeId,
        /// The parameter key.
        key: ParamKey,
        /// The new record.
        record: ParamRecord,
    },
    /// A node-scoped parameter was deleted.
    #[oxicode(variant = 11)]
    NodeParamDelete {
        /// The owning dataflow.
        dataflow: DataflowId,
        /// The owning node.
        node: NodeId,
        /// The parameter key.
        key: ParamKey,
    },
    /// A dynamic-topology op (blueprint §8, §17 — `astrs node
    /// add/remove/replace/connect/disconnect`) was applied to a running
    /// dataflow's tracked graph.
    ///
    /// Another tail append beyond the original ten variants, for the same
    /// reason `NodeParamPut`/`NodeParamDelete` above are: recorded so a
    /// coordinator restart can rebuild its in-memory tracked graph exactly
    /// (base manifest, replayed forward through every op logged after it)
    /// rather than losing every dynamic change once the process holding
    /// them exits.
    ///
    /// `op_json` carries the op pre-serialized as JSON rather than as a
    /// native field: this crate is Layer 1 (blueprint §4.1, "Substrate")
    /// and must not depend on `astrs-graph` (Layer 2, "Domain libraries")
    /// merely to name its `TopologyOp` type — that would be an upward
    /// dependency the layer rule forbids. `astrs-coordinator`, which
    /// depends on both, decodes `op_json` back into a
    /// `astrs_graph::TopologyOp` (via that type's own `Deserialize`) to
    /// replay it through `astrs_graph::apply` — see that crate's
    /// `rehydrate` module.
    #[oxicode(variant = 12)]
    TopologyOpApplied {
        /// The dataflow whose tracked graph changed.
        dataflow: DataflowId,
        /// The JSON-serialized `astrs_graph::TopologyOp`.
        op_json: String,
    },
}

impl MutationOp {
    /// A short, stable label for the bucket this op targets — for error
    /// messages and metrics, not for parsing.
    #[must_use]
    pub const fn bucket_name(&self) -> &'static str {
        match self {
            Self::ParamPut { .. } | Self::ParamDelete { .. } => "params",
            Self::DataflowMetaPut { .. } | Self::DataflowMetaDelete { .. } => "dataflow_meta",
            Self::NodeStatusPut { .. } | Self::NodeStatusDelete { .. } => "node_status",
            Self::DaemonPut { .. } | Self::DaemonDelete { .. } => "daemon",
            Self::BuildCachePut { .. } | Self::BuildCacheDelete { .. } => "build_cache",
            Self::NodeParamPut { .. } | Self::NodeParamDelete { .. } => "node_params",
            Self::TopologyOpApplied { .. } => "topology_ops",
        }
    }
}

/// One entry in the mutation log: a sequence number, the hybrid logical
/// clock timestamp the change was applied at, and the change itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct MutationRecord {
    /// This record's position in the log. Strictly increasing, with no
    /// gaps, across the log's lifetime (blueprint §12).
    pub seq: MutationSeq,
    /// When the change was applied.
    pub ts: HlcTimestamp,
    /// What changed.
    pub op: MutationOp,
}

/// A bounded page of the mutation log, as returned by
/// [`crate::CoordinatorStore::mutations_since`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpBatch {
    /// The records in this page, in ascending sequence order.
    pub entries: Vec<MutationRecord>,
    /// The sequence number the caller should pass to the next
    /// `mutations_since` call to continue paging: the last entry's `seq` if
    /// `entries` is non-empty, otherwise the `after` value the caller
    /// passed in.
    pub next_seq: MutationSeq,
    /// Whether `entries` contains everything currently logged after the
    /// requested position. `false` means more entries exist and the caller
    /// should call `mutations_since(next_seq, ...)` again.
    pub caught_up: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{WireDecode, WireEncode};

    fn ts(n: u64) -> HlcTimestamp {
        HlcTimestamp::new(n, 0)
    }

    #[test]
    fn seq_zero_is_the_default_and_the_reserved_sentinel() {
        assert_eq!(MutationSeq::default(), MutationSeq::ZERO);
        assert_eq!(MutationSeq::ZERO.get(), 0);
    }

    #[test]
    fn seq_orders_numerically() {
        assert!(MutationSeq::new(1) < MutationSeq::new(2));
        assert!(MutationSeq::new(9) < MutationSeq::new(10));
    }

    #[test]
    fn seq_saturates_instead_of_wrapping() {
        assert_eq!(
            MutationSeq::new(u64::MAX).saturating_add(5),
            MutationSeq::new(u64::MAX)
        );
    }

    #[test]
    fn seq_displays_as_a_plain_integer() {
        assert_eq!(MutationSeq::new(42).to_string(), "42");
    }

    #[test]
    fn every_op_variant_reports_its_bucket_name() {
        let dataflow = DataflowId::from_u128(1);
        let key = ParamKey::new("gain").unwrap();
        assert_eq!(
            MutationOp::ParamDelete {
                dataflow,
                key: key.clone()
            }
            .bucket_name(),
            "params"
        );
        assert_eq!(
            MutationOp::DataflowMetaDelete { dataflow }.bucket_name(),
            "dataflow_meta"
        );
        assert_eq!(
            MutationOp::NodeStatusDelete {
                dataflow,
                node: NodeId::new("n").unwrap(),
            }
            .bucket_name(),
            "node_status"
        );
        assert_eq!(
            MutationOp::DaemonDelete {
                daemon: DaemonId::generate(None),
            }
            .bucket_name(),
            "daemon"
        );
        assert_eq!(
            MutationOp::BuildCacheDelete {
                hash: BuildCacheKey::new(vec![1]),
            }
            .bucket_name(),
            "build_cache"
        );
        assert_eq!(
            MutationOp::NodeParamDelete {
                dataflow,
                node: NodeId::new("camera").unwrap(),
                key: key.clone(),
            }
            .bucket_name(),
            "node_params"
        );
    }

    #[test]
    fn topology_op_applied_is_frozen_at_the_tail_and_reports_its_bucket() {
        let dataflow = DataflowId::from_u128(1);
        let op = MutationOp::TopologyOpApplied {
            dataflow,
            op_json: "{\"AddNode\":{\"id\":\"extra\",\"node\":{}}}".to_owned(),
        };
        assert_eq!(op.bucket_name(), "topology_ops");
        let bytes = op.encode_to_vec().unwrap();
        assert_eq!(usize::from(bytes[0]), 12);
        assert_eq!(MutationOp::decode_exact(&bytes).unwrap(), op);
    }

    #[test]
    fn node_param_variants_are_frozen_at_the_tail() {
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        let record = ParamRecord {
            value_json: "1".to_owned(),
            revision: 1,
            created_at: ts(1),
            updated_at: ts(1),
        };

        let put = MutationOp::NodeParamPut {
            dataflow,
            node: node.clone(),
            key: key.clone(),
            record,
        };
        assert_eq!(usize::from(put.encode_to_vec().unwrap()[0]), 10);
        assert_eq!(
            MutationOp::decode_exact(&put.encode_to_vec().unwrap()).unwrap(),
            put
        );

        let delete = MutationOp::NodeParamDelete {
            dataflow,
            node,
            key,
        };
        assert_eq!(usize::from(delete.encode_to_vec().unwrap()[0]), 11);
        assert_eq!(
            MutationOp::decode_exact(&delete.encode_to_vec().unwrap()).unwrap(),
            delete
        );
    }

    #[test]
    fn mutation_record_survives_the_oxicode_codec() {
        let record = MutationRecord {
            seq: MutationSeq::new(1),
            ts: ts(100),
            op: MutationOp::ParamPut {
                dataflow: DataflowId::from_u128(1),
                key: ParamKey::new("gain").unwrap(),
                record: ParamRecord {
                    value_json: "1".to_owned(),
                    revision: 1,
                    created_at: ts(100),
                    updated_at: ts(100),
                },
            },
        };
        let bytes = record.encode_to_vec().unwrap();
        assert_eq!(MutationRecord::decode_exact(&bytes).unwrap(), record);
    }

    #[test]
    fn mutation_record_survives_the_json_codec() {
        let record = MutationRecord {
            seq: MutationSeq::new(2),
            ts: ts(1),
            op: MutationOp::DaemonDelete {
                daemon: DaemonId::generate(None),
            },
        };
        let json = serde_json::to_vec(&record).unwrap();
        let back: MutationRecord = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn catch_up_batch_carries_pagination_state() {
        let batch = CatchUpBatch {
            entries: Vec::new(),
            next_seq: MutationSeq::ZERO,
            caught_up: true,
        };
        assert!(batch.caught_up);
        assert!(batch.entries.is_empty());
    }
}
