//! The dataflow-registry bucket's record types.
//!
//! A dataflow's state is split across two independently keyed record types
//! rather than one record nesting a per-node map:
//!
//! - [`DataflowMeta`] — one row per dataflow: the manifest snapshot, FSM
//!   status, and summary fields. Changes rarely (start/stop/status
//!   transitions).
//! - [`NodeStatusRecord`] — one row per `(dataflow, node)`: a single node's
//!   [`NodeInfo`]. Changes often (every spawn, heartbeat-driven state
//!   change, restart, exit).
//!
//! Nesting the per-node map inside `DataflowMeta` would mean that updating
//! *one* node's status in an N-node dataflow rewrites, re-serializes, and
//! re-logs all N nodes' records — an O(N) cost paid on every single-node
//! event, which for a busy dataflow's mutation log dwarfs the O(1) row this
//! split makes it instead. [`crate::CoordinatorStore::dataflow_snapshot`]
//! composes both into a read-only [`DataflowSnapshot`] for callers that want
//! the combined view.

use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use astrs_wire::{
    DaemonId, DataflowId, DataflowStatus, DataflowSummary, NodeId, NodeInfo, NodeRunState,
};
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// One dataflow's registration: manifest snapshot, FSM status and summary
/// fields — everything about a dataflow except per-node detail (see the
/// module docs for why that is split out into [`NodeStatusRecord`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DataflowMeta {
    /// The dataflow's id.
    pub id: DataflowId,
    /// Its manifest `name:`, if it declared one.
    pub name: Option<String>,
    /// A snapshot of the manifest that started this dataflow, as JSON text.
    ///
    /// `astrs-store` does not depend on `astrs-manifest` (blueprint layering:
    /// this is a Layer 1 substrate crate), so the caller — the coordinator —
    /// is responsible for serializing whatever manifest representation it
    /// holds to JSON before calling
    /// [`crate::CoordinatorStore::upsert_dataflow`].
    pub manifest_json: String,
    /// The dataflow's current lifecycle state.
    pub status: DataflowStatus,
    /// The daemons hosting at least one of this dataflow's nodes.
    pub daemons: Vec<DaemonId>,
    /// How many nodes the manifest declares.
    pub node_count: u32,
    /// When the dataflow started, if it has.
    pub started_at: Option<HlcTimestamp>,
    /// Monotonically increasing per-dataflow version counter, incremented on
    /// every [`crate::CoordinatorStore::upsert_dataflow`] /
    /// [`crate::CoordinatorStore::set_dataflow_status`] call.
    pub revision: u64,
    /// The hybrid logical clock timestamp of the last write.
    pub updated_at: HlcTimestamp,
}

/// One node's last-known status within a dataflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeStatusRecord {
    /// The node's identity and run state, reusing [`astrs_wire`]'s own
    /// wire-level type rather than duplicating its fields (blueprint
    /// instruction: build on Wave-1 crates, never re-shape their public
    /// types).
    pub info: NodeInfo,
    /// Monotonically increasing per-node version counter, incremented on
    /// every [`crate::CoordinatorStore::set_node_status`] call for this
    /// `(dataflow, node)` pair.
    pub revision: u64,
    /// The hybrid logical clock timestamp of the last write.
    pub updated_at: HlcTimestamp,
}

/// A read-only composed view: one dataflow's metadata plus every node's
/// last-known status.
///
/// Returned by [`crate::CoordinatorStore::dataflow_snapshot`]; never itself
/// the unit of storage or of a mutation-log record — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataflowSnapshot {
    /// The dataflow's registration record.
    pub meta: DataflowMeta,
    /// Every node this store currently has status for, keyed by node id.
    pub nodes: BTreeMap<NodeId, NodeStatusRecord>,
}

impl DataflowSnapshot {
    /// Reduces this snapshot to an [`astrs_wire::DataflowSummary`] row — the
    /// shape `astrs list` / `ControlReply::DataflowList` sends over the
    /// wire.
    ///
    /// [`DataflowSummary::running_nodes`] counts only nodes in
    /// [`DataflowSnapshot::nodes`] whose [`NodeRunState`] is exactly
    /// [`NodeRunState::Running`] — which can be fewer than
    /// [`DataflowMeta::node_count`] simply because not every manifest node
    /// has spawned yet, not necessarily because any of them crashed. A
    /// caller must not treat `running_nodes < node_count` alone as a
    /// failure signal.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_wire::NodeRunState;
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// let dataflow = astrs_wire::DataflowId::generate();
    /// store.upsert_dataflow(dataflow, None, "{}".into(), 2)?;
    ///
    /// let snapshot = store.dataflow_snapshot(dataflow)?.expect("just registered");
    /// let summary = snapshot.to_summary();
    /// assert_eq!(summary.node_count, 2, "declared by the manifest");
    /// assert_eq!(summary.running_nodes, 0, "none have spawned yet — not a failure");
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    #[must_use]
    pub fn to_summary(&self) -> DataflowSummary {
        let running_nodes = self
            .nodes
            .values()
            .filter(|node| node.info.state == NodeRunState::Running)
            .count() as u32;
        DataflowSummary {
            id: self.meta.id,
            name: self.meta.name.clone(),
            status: self.meta.status,
            daemons: self.meta.daemons.clone(),
            node_count: self.meta.node_count,
            running_nodes,
            started_at: self.meta.started_at,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{NodeExitCause, WireDecode, WireEncode};

    fn ts(n: u64) -> HlcTimestamp {
        HlcTimestamp::new(n, 0)
    }

    fn sample_node_info() -> NodeInfo {
        NodeInfo {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new("camera").unwrap(),
            daemon: DaemonId::generate(None),
            state: NodeRunState::Running,
            pid: Some(4242),
            generation: 1,
            restart_count: 0,
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            started_at: Some(ts(5)),
            exit_cause: None::<NodeExitCause>,
        }
    }

    #[test]
    fn dataflow_meta_survives_both_codecs() {
        let meta = DataflowMeta {
            id: DataflowId::from_u128(1),
            name: Some("demo".to_owned()),
            manifest_json: "{}".to_owned(),
            status: DataflowStatus::Running,
            daemons: vec![DaemonId::generate(None)],
            node_count: 3,
            started_at: Some(ts(1)),
            revision: 4,
            updated_at: ts(9),
        };
        let bytes = meta.encode_to_vec().unwrap();
        assert_eq!(DataflowMeta::decode_exact(&bytes).unwrap(), meta);

        let json = serde_json::to_vec(&meta).unwrap();
        let back: DataflowMeta = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, meta);
    }

    #[test]
    fn node_status_record_survives_both_codecs() {
        let record = NodeStatusRecord {
            info: sample_node_info(),
            revision: 2,
            updated_at: ts(7),
        };
        let bytes = record.encode_to_vec().unwrap();
        assert_eq!(NodeStatusRecord::decode_exact(&bytes).unwrap(), record);

        let json = serde_json::to_vec(&record).unwrap();
        let back: NodeStatusRecord = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn snapshot_composes_meta_and_nodes() {
        let meta = DataflowMeta {
            id: DataflowId::from_u128(1),
            name: None,
            manifest_json: "{}".to_owned(),
            status: DataflowStatus::Pending,
            daemons: Vec::new(),
            node_count: 1,
            started_at: None,
            revision: 1,
            updated_at: ts(1),
        };
        let mut nodes = BTreeMap::new();
        nodes.insert(
            NodeId::new("camera").unwrap(),
            NodeStatusRecord {
                info: sample_node_info(),
                revision: 1,
                updated_at: ts(1),
            },
        );
        let snapshot = DataflowSnapshot {
            meta: meta.clone(),
            nodes: nodes.clone(),
        };
        assert_eq!(snapshot.meta, meta);
        assert_eq!(snapshot.nodes, nodes);
    }

    #[test]
    fn to_summary_counts_only_running_nodes() {
        let meta = DataflowMeta {
            id: DataflowId::from_u128(1),
            name: Some("demo".to_owned()),
            manifest_json: "{}".to_owned(),
            status: DataflowStatus::Running,
            daemons: vec![DaemonId::generate(None)],
            // The manifest declares three nodes, but this snapshot only
            // has status for two of them (the third has not spawned yet).
            node_count: 3,
            started_at: Some(ts(1)),
            revision: 1,
            updated_at: ts(1),
        };
        let mut nodes = BTreeMap::new();
        nodes.insert(
            NodeId::new("camera").unwrap(),
            NodeStatusRecord {
                info: sample_node_info(),
                revision: 1,
                updated_at: ts(1),
            },
        );
        let mut spawning_info = sample_node_info();
        spawning_info.node = NodeId::new("detector").unwrap();
        spawning_info.state = NodeRunState::Spawning;
        nodes.insert(
            NodeId::new("detector").unwrap(),
            NodeStatusRecord {
                info: spawning_info,
                revision: 1,
                updated_at: ts(1),
            },
        );
        let snapshot = DataflowSnapshot { meta, nodes };

        let summary = snapshot.to_summary();
        assert_eq!(summary.id, DataflowId::from_u128(1));
        assert_eq!(summary.name.as_deref(), Some("demo"));
        assert_eq!(summary.status, DataflowStatus::Running);
        assert_eq!(summary.node_count, 3, "declared by the manifest");
        assert_eq!(
            summary.running_nodes, 1,
            "only `camera` is Running; `detector` is Spawning and the third \
             node has no status row at all — neither counts, and neither is \
             a failure signal"
        );
    }

    #[test]
    fn to_summary_of_an_empty_snapshot_has_zero_running_nodes() {
        let meta = DataflowMeta {
            id: DataflowId::from_u128(2),
            name: None,
            manifest_json: "{}".to_owned(),
            status: DataflowStatus::Pending,
            daemons: Vec::new(),
            node_count: 5,
            started_at: None,
            revision: 1,
            updated_at: ts(1),
        };
        let snapshot = DataflowSnapshot {
            meta,
            nodes: BTreeMap::new(),
        };
        let summary = snapshot.to_summary();
        assert_eq!(summary.running_nodes, 0);
        assert_eq!(summary.node_count, 5);
    }
}
