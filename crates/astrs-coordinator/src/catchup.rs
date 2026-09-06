//! Translating `astrs-store`'s mutation log into
//! [`astrs_wire::StateCatchUp`](astrs_wire::CoordinatorEvent::StateCatchUp)
//! batches, and pushing them at a (re)connecting daemon (blueprint §12,
//! §24.1).
//!
//! # Why some mutations translate to nothing
//!
//! [`astrs_store::record::MutationOp`] and [`astrs_wire::StateEntryKind`]
//! are shaped for different audiences: the store's log is the
//! coordinator's own durable bookkeeping (ten-plus variants, several
//! carrying full records), while the wire's catch-up entries are only what
//! a *daemon* needs to resynchronise (eight variants, each a small
//! projection). [`mutation_to_state_entry`] returns `None` for a store
//! mutation with nothing for a daemon to act on — a build-cache change
//! (coordinator-local artefact reuse bookkeeping) or a delete of a
//! dataflow's/node's durable *record* (the daemon already knows its own
//! processes; the coordinator's own control messages, not the catch-up
//! log, are what tell it to stop them).
//!
//! Skipping an entry does not lose the daemon's place in the log:
//! [`push_catch_up`] always advances by the store's own
//! [`astrs_store::record::CatchUpBatch::next_seq`], which accounts for
//! every raw record in a page whether or not it produced a translated
//! entry.

use astrs_store::record::{MutationOp, MutationRecord};
use astrs_wire::{CoordinatorEvent, DaemonId, ParamScope, StateEntry, StateEntryKind};
use tokio::sync::mpsc;

use crate::coordinator::Coordinator;
use crate::error::Result;
use crate::param_scope::{GLOBAL_SCOPE_DATAFLOW, json_to_parameter};

/// Translates one mutation-log record into a [`StateEntry`], if it has a
/// wire-level equivalent — see this module's top-level docs for the
/// entries that do not.
#[must_use]
pub fn mutation_to_state_entry(record: &MutationRecord) -> Option<StateEntry> {
    let kind = match &record.op {
        MutationOp::DataflowMetaPut { record } => StateEntryKind::DataflowStatus {
            dataflow: record.id,
            status: record.status,
            name: record.name.clone(),
        },
        MutationOp::NodeStatusPut { record } => StateEntryKind::NodeState {
            dataflow: record.info.dataflow,
            node: record.info.node.clone(),
            generation: record.info.generation,
            state: record.info.state,
        },
        MutationOp::ParamPut {
            dataflow,
            key,
            record,
        } => {
            let value = json_to_parameter(record.value().ok()?).ok()?;
            StateEntryKind::ParamSet {
                scope: dataflow_param_scope(*dataflow),
                key: key.clone(),
                value,
            }
        }
        MutationOp::ParamDelete { dataflow, key } => StateEntryKind::ParamDeleted {
            scope: dataflow_param_scope(*dataflow),
            key: key.clone(),
        },
        MutationOp::NodeParamPut {
            dataflow,
            node,
            key,
            record,
        } => {
            let value = json_to_parameter(record.value().ok()?).ok()?;
            StateEntryKind::ParamSet {
                scope: ParamScope::node(*dataflow, node.clone()),
                key: key.clone(),
                value,
            }
        }
        MutationOp::NodeParamDelete {
            dataflow,
            node,
            key,
        } => StateEntryKind::ParamDeleted {
            scope: ParamScope::node(*dataflow, node.clone()),
            key: key.clone(),
        },
        MutationOp::DaemonPut { record } => StateEntryKind::DaemonPresence {
            daemon: record.info.id.clone(),
            connected: record.info.reachable,
        },
        MutationOp::DaemonDelete { daemon } => StateEntryKind::DaemonPresence {
            daemon: daemon.clone(),
            connected: false,
        },
        MutationOp::DataflowMetaDelete { .. }
        | MutationOp::NodeStatusDelete { .. }
        | MutationOp::BuildCachePut { .. }
        | MutationOp::BuildCacheDelete { .. } => return None,
        // `MutationOp` is `#[non_exhaustive]` (mirroring `astrs-wire`'s own
        // append-only discipline, per that type's docs); a variant added
        // in a later store revision has no catch-up mapping decided for
        // it yet, so it is skipped exactly like the bucket-local variants
        // above rather than guessed at.
        _ => return None,
    };
    Some(StateEntry::new(record.seq.get(), record.ts, kind))
}

/// The [`ParamScope`] a dataflow-scoped store mutation belongs to —
/// [`ParamScope::Global`] for the [`GLOBAL_SCOPE_DATAFLOW`] sentinel,
/// [`ParamScope::Dataflow`] otherwise (see `crate::param_scope`'s module
/// docs for the sentinel convention).
fn dataflow_param_scope(dataflow: astrs_wire::DataflowId) -> ParamScope {
    if dataflow == GLOBAL_SCOPE_DATAFLOW {
        ParamScope::Global
    } else {
        ParamScope::dataflow_scope(dataflow)
    }
}

/// Pushes every mutation logged after `after` at a (re)connecting daemon,
/// as one or more `StateCatchUp` batches, ending with one whose
/// `final_batch` is `true`.
///
/// Always sends at least one batch — even an empty one — so the daemon's
/// "waiting for catch-up" state always resolves, even when nothing has
/// changed since `after` at all.
///
/// # Errors
///
/// [`crate::CoordinatorError::Store`] if the mutation log cannot be read.
/// [`crate::CoordinatorError::DaemonNotConnected`] if `sender`'s receiving
/// half has already been dropped (the connection ended mid-catch-up).
pub async fn push_catch_up(
    coordinator: &Coordinator,
    daemon: &DaemonId,
    sender: &mpsc::Sender<CoordinatorEvent>,
    after: astrs_store::record::MutationSeq,
) -> Result<()> {
    let mut cursor = after;
    loop {
        let batch = coordinator
            .store
            .mutations_since(cursor, coordinator.config.max_catch_up_page)
            .await?;
        let entries: Vec<StateEntry> = batch
            .entries
            .iter()
            .filter_map(mutation_to_state_entry)
            .collect();
        let seq = entries
            .first()
            .map_or_else(|| batch.next_seq.get(), |entry| entry.seq);
        let final_batch = batch.caught_up;
        let event = CoordinatorEvent::StateCatchUp {
            seq,
            entries,
            final_batch,
        };
        sender
            .send(event)
            .await
            .map_err(|_| crate::error::CoordinatorError::DaemonNotConnected(daemon.clone()))?;
        cursor = batch.next_seq;
        if final_batch {
            break;
        }
    }
    Ok(())
}

/// The fallback for [`push_catch_up`] when the daemon's requested cursor
/// names a sequence older than [`astrs_store::CoordinatorStore::compact`]
/// has retained (`after < compacted_before` inside
/// [`astrs_store::CoordinatorStore::mutations_since`], surfaced as
/// [`astrs_store::Error::MutationHistoryCompacted`]): there is no delta to
/// replay that far back, so this sends the coordinator's **entire current
/// state** instead — every registered dataflow's status, every node's run
/// state, and every dataflow-scoped parameter — as one `StateCatchUp` batch.
///
/// # Why this reuses `StateEntry` rather than a new message
///
/// A full snapshot is expressed with exactly the [`StateEntryKind`]
/// variants an ordinary delta batch already carries
/// ([`StateEntryKind::DataflowStatus`], [`StateEntryKind::NodeState`],
/// [`StateEntryKind::ParamSet`], [`StateEntryKind::DaemonPresence`]) —
/// reusing the wire contract [`push_catch_up`] already has, rather than
/// widening it or appending a variant blueprint §7.2's append-only rule
/// would otherwise require regenerating `astrs-wire`'s frozen-protocol
/// snapshot for. A snapshot and a delta both resolve to "the daemon's view
/// of the world, as of `seq`" — only how each one got there differs, and the
/// daemon-side consumer (`astrs-daemon`'s
/// `coordinator::apply::Daemon::apply_state_catch_up`, a crate this one does
/// not depend on and so cannot link to) does not need to know which kind it
/// received.
///
/// [`StateEntryKind::Route`] and [`StateEntryKind::Subscription`] are
/// deliberately absent: both describe daemon-local or CLI-local state a
/// reconnecting *daemon* already owns and never derives from the
/// coordinator (that same `apply_state_catch_up`'s own doc: "it is the
/// *executor*... every fact it needs to act on arrives as its own
/// instruction"), so there is nothing for a snapshot to reconstruct there
/// that the delta path would have sent either.
///
/// # Why every entry — and the batch itself — is stamped with one `seq`
///
/// A reconnecting daemon acks the highest `seq` it sees and remembers it as
/// its next resume point (blueprint §12; `astrs-daemon`'s
/// `apply_state_catch_up` folds every entry's `seq` into a running
/// high-water mark and hands that back as `DaemonEvent::StateCatchUpAck`).
/// Reading
/// [`astrs_store::CoordinatorStore::last_seq`] once, up front, and stamping
/// every entry and the batch's own `seq` with it is what makes the daemon's
/// cursor land on a real, current log position — anything else (a
/// per-record historical `seq`, or `0`) would leave the daemon's next
/// reconnect asking for history the log has *also* since compacted away,
/// reproducing the very failure this function exists to recover from.
///
/// # Errors
///
/// [`crate::CoordinatorError::Store`] if the store cannot be read.
/// [`crate::CoordinatorError::DaemonNotConnected`] if `sender`'s receiving
/// half has already been dropped (the connection ended before the snapshot
/// could be delivered).
pub async fn push_full_snapshot(
    coordinator: &Coordinator,
    daemon: &DaemonId,
    sender: &mpsc::Sender<CoordinatorEvent>,
) -> Result<()> {
    let seq = coordinator.store.last_seq().await?.get();
    let mut entries = Vec::new();

    for record in coordinator.store.list_daemons().await? {
        entries.push(StateEntry::new(
            seq,
            record.last_heartbeat,
            StateEntryKind::DaemonPresence {
                daemon: record.info.id.clone(),
                connected: record.info.reachable,
            },
        ));
    }

    for meta in coordinator.store.list_dataflows().await? {
        let dataflow = meta.id;
        entries.push(StateEntry::new(
            seq,
            meta.updated_at,
            StateEntryKind::DataflowStatus {
                dataflow,
                status: meta.status,
                name: meta.name.clone(),
            },
        ));

        // One consistent read of this dataflow's meta *and* every node's
        // status — `CoordinatorStore::dataflow_snapshot`'s own reason for
        // existing (blueprint §12) — rather than this function's own
        // `list_dataflows` pass and a second, separately-timed
        // `list_node_status` potentially disagreeing under a concurrent
        // writer.
        if let Some(snapshot) = coordinator.store.dataflow_snapshot(dataflow).await? {
            for status in snapshot.nodes.values() {
                entries.push(StateEntry::new(
                    seq,
                    status.updated_at,
                    StateEntryKind::NodeState {
                        dataflow: status.info.dataflow,
                        node: status.info.node.clone(),
                        generation: status.info.generation,
                        state: status.info.state,
                    },
                ));
            }
        }

        for (key, record) in coordinator.store.list_params(dataflow).await? {
            // Mirrors `mutation_to_state_entry`'s own `ParamPut` arm:
            // unparseable is unreachable for a value this coordinator wrote
            // itself, and skipping (rather than failing the whole snapshot)
            // matches the delta path's own tolerance for it.
            if let Ok(json) = record.value()
                && let Ok(value) = json_to_parameter(json)
            {
                entries.push(StateEntry::new(
                    seq,
                    record.updated_at,
                    StateEntryKind::ParamSet {
                        scope: dataflow_param_scope(dataflow),
                        key: key.clone(),
                        value,
                    },
                ));
            }
        }
    }

    sender
        .send(CoordinatorEvent::StateCatchUp {
            seq,
            entries,
            final_batch: true,
        })
        .await
        .map_err(|_| crate::error::CoordinatorError::DaemonNotConnected(daemon.clone()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_wire::{
        AuthToken, DaemonInfo, DataflowId, DataflowStatus, NodeId, NodeInfo, NodeRunState, ParamKey,
    };

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([2; 32])).with_port(0),
        )
        .unwrap()
    }

    #[test]
    fn dataflow_status_and_param_mutations_translate() {
        let dataflow = DataflowId::from_u128(1);
        let key = ParamKey::new("gain").unwrap();

        let meta = astrs_store::record::DataflowMeta {
            id: dataflow,
            name: Some("demo".into()),
            manifest_json: "{}".into(),
            status: DataflowStatus::Running,
            daemons: vec![],
            node_count: 1,
            started_at: None,
            revision: 1,
            updated_at: astrs_time::HlcTimestamp::EPOCH,
        };
        let record = MutationRecord {
            seq: astrs_store::record::MutationSeq::new(1),
            ts: astrs_time::HlcTimestamp::new(5, 0),
            op: MutationOp::DataflowMetaPut { record: meta },
        };
        let entry = mutation_to_state_entry(&record).unwrap();
        assert_eq!(entry.seq, 1);
        match entry.kind {
            StateEntryKind::DataflowStatus {
                dataflow: got,
                status,
                name,
            } => {
                assert_eq!(got, dataflow);
                assert_eq!(status, DataflowStatus::Running);
                assert_eq!(name, Some("demo".to_owned()));
            }
            other => panic!("unexpected {other:?}"),
        }

        let param_record = astrs_store::record::ParamRecord {
            value_json: serde_json::to_string(&serde_json::json!({"integer": 3})).unwrap(),
            revision: 1,
            created_at: astrs_time::HlcTimestamp::EPOCH,
            updated_at: astrs_time::HlcTimestamp::EPOCH,
        };
        let param_put = MutationRecord {
            seq: astrs_store::record::MutationSeq::new(2),
            ts: astrs_time::HlcTimestamp::EPOCH,
            op: MutationOp::ParamPut {
                dataflow,
                key: key.clone(),
                record: param_record,
            },
        };
        let entry = mutation_to_state_entry(&param_put).unwrap();
        match entry.kind {
            StateEntryKind::ParamSet {
                scope,
                key: got_key,
                value,
            } => {
                assert_eq!(scope, ParamScope::dataflow_scope(dataflow));
                assert_eq!(got_key, key);
                assert_eq!(value, astrs_wire::Parameter::Integer(3));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn global_scope_params_translate_using_the_sentinel() {
        let record = MutationRecord {
            seq: astrs_store::record::MutationSeq::new(1),
            ts: astrs_time::HlcTimestamp::EPOCH,
            op: MutationOp::ParamDelete {
                dataflow: GLOBAL_SCOPE_DATAFLOW,
                key: ParamKey::new("gain").unwrap(),
            },
        };
        let entry = mutation_to_state_entry(&record).unwrap();
        match entry.kind {
            StateEntryKind::ParamDeleted { scope, .. } => assert_eq!(scope, ParamScope::Global),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn node_scoped_params_translate_with_the_node_in_scope() {
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("camera").unwrap();
        let record = MutationRecord {
            seq: astrs_store::record::MutationSeq::new(1),
            ts: astrs_time::HlcTimestamp::EPOCH,
            op: MutationOp::NodeParamDelete {
                dataflow,
                node: node.clone(),
                key: ParamKey::new("exposure").unwrap(),
            },
        };
        let entry = mutation_to_state_entry(&record).unwrap();
        match entry.kind {
            StateEntryKind::ParamDeleted { scope, .. } => {
                assert_eq!(scope, ParamScope::node(dataflow, node));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn build_cache_and_delete_mutations_have_no_wire_equivalent() {
        let record = MutationRecord {
            seq: astrs_store::record::MutationSeq::new(1),
            ts: astrs_time::HlcTimestamp::EPOCH,
            op: MutationOp::BuildCacheDelete {
                hash: astrs_store::record::BuildCacheKey::new(vec![1]),
            },
        };
        assert!(mutation_to_state_entry(&record).is_none());

        let record = MutationRecord {
            seq: astrs_store::record::MutationSeq::new(2),
            ts: astrs_time::HlcTimestamp::EPOCH,
            op: MutationOp::DataflowMetaDelete {
                dataflow: DataflowId::from_u128(1),
            },
        };
        assert!(mutation_to_state_entry(&record).is_none());
    }

    #[tokio::test]
    async fn push_catch_up_sends_one_final_batch_when_nothing_changed() {
        let coordinator = coordinator();
        let (tx, mut rx) = mpsc::channel(8);
        push_catch_up(
            &coordinator,
            &DaemonId::generate(None),
            &tx,
            astrs_store::record::MutationSeq::ZERO,
        )
        .await
        .unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            CoordinatorEvent::StateCatchUp {
                entries,
                final_batch,
                ..
            } => {
                assert!(entries.is_empty());
                assert!(final_batch);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "exactly one batch");
    }

    /// A store-ready JSON value for an integer parameter — mirroring what
    /// the real `SetParam` handler stores (`param_scope`'s
    /// `parameter_to_json`, never the bare JSON number a caller's `astrs
    /// param set` argument might look like), since [`mutation_to_state_entry`]
    /// decodes a `Parameter` back out of exactly that shape.
    fn param_value(n: i64) -> serde_json::Value {
        crate::param_scope::parameter_to_json(&astrs_wire::Parameter::Integer(n)).unwrap()
    }

    #[tokio::test]
    async fn push_catch_up_delivers_every_translatable_mutation() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator
            .store
            .sync()
            .set_param(dataflow, ParamKey::new("a").unwrap(), param_value(1))
            .unwrap();
        coordinator
            .store
            .sync()
            .set_param(dataflow, ParamKey::new("b").unwrap(), param_value(2))
            .unwrap();

        let (tx, mut rx) = mpsc::channel(8);
        push_catch_up(
            &coordinator,
            &DaemonId::generate(None),
            &tx,
            astrs_store::record::MutationSeq::ZERO,
        )
        .await
        .unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            CoordinatorEvent::StateCatchUp {
                entries,
                final_batch,
                ..
            } => {
                assert_eq!(entries.len(), 2);
                assert!(final_batch);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_catch_up_pages_across_a_small_max_catch_up_page() {
        let mut config = CoordinatorConfig::new(AuthToken::from_bytes([3; 32])).with_port(0);
        config.max_catch_up_page = 1;
        let coordinator = Coordinator::open_in_memory(config).unwrap();
        let dataflow = DataflowId::generate();
        coordinator
            .store
            .sync()
            .set_param(dataflow, ParamKey::new("a").unwrap(), param_value(1))
            .unwrap();
        coordinator
            .store
            .sync()
            .set_param(dataflow, ParamKey::new("b").unwrap(), param_value(2))
            .unwrap();

        let (tx, mut rx) = mpsc::channel(8);
        push_catch_up(
            &coordinator,
            &DaemonId::generate(None),
            &tx,
            astrs_store::record::MutationSeq::ZERO,
        )
        .await
        .unwrap();

        let first = rx.try_recv().unwrap();
        assert!(matches!(
            first,
            CoordinatorEvent::StateCatchUp {
                final_batch: false,
                ..
            }
        ));
        let second = rx.try_recv().unwrap();
        assert!(matches!(
            second,
            CoordinatorEvent::StateCatchUp {
                final_batch: true,
                ..
            }
        ));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn push_catch_up_reports_a_dropped_receiver_as_daemon_not_connected() {
        let coordinator = coordinator();
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        let daemon = DaemonId::generate(None);
        let err = push_catch_up(
            &coordinator,
            &daemon,
            &tx,
            astrs_store::record::MutationSeq::ZERO,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, crate::error::CoordinatorError::DaemonNotConnected(got) if got == daemon)
        );
    }

    #[tokio::test]
    async fn resuming_after_a_seq_only_sends_later_mutations() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator
            .store
            .sync()
            .set_param(dataflow, ParamKey::new("a").unwrap(), param_value(1))
            .unwrap();
        let after = coordinator.store.sync().last_seq().unwrap();
        coordinator
            .store
            .sync()
            .set_param(dataflow, ParamKey::new("b").unwrap(), param_value(2))
            .unwrap();

        let (tx, mut rx) = mpsc::channel(8);
        push_catch_up(&coordinator, &DaemonId::generate(None), &tx, after)
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            CoordinatorEvent::StateCatchUp { entries, .. } => assert_eq!(entries.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn node_info(dataflow: DataflowId, node: &str) -> NodeInfo {
        NodeInfo {
            dataflow,
            node: NodeId::new(node).unwrap(),
            daemon: DaemonId::generate(None),
            state: NodeRunState::Running,
            pid: None,
            generation: 1,
            restart_count: 0,
            inputs: std::collections::BTreeMap::new(),
            outputs: std::collections::BTreeMap::new(),
            started_at: None,
            exit_cause: None,
        }
    }

    fn daemon_info(id: DaemonId) -> DaemonInfo {
        DaemonInfo {
            id,
            version: astrs_wire::AstrsVersion::default(),
            address: "uds:///tmp/astrs.sock".to_owned(),
            connected_at: astrs_time::HlcTimestamp::EPOCH,
            node_count: 1,
            labels: std::collections::BTreeMap::new(),
            reachable: true,
        }
    }

    /// The scenario item 4's fix exists for: a daemon reconnects asking to
    /// resume from a sequence the mutation log no longer has a delta for.
    /// `push_catch_up` reports exactly that
    /// ([`astrs_store::Error::MutationHistoryCompacted`], wrapped as
    /// [`crate::error::CoordinatorError::Store`]), and
    /// [`push_full_snapshot`] recovers every current fact — the dataflow's
    /// status, its node's run state, its parameter, and the connected
    /// daemon — that the compacted history could no longer diff against.
    #[tokio::test]
    async fn push_full_snapshot_recovers_current_state_after_a_compaction() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let daemon = DaemonId::generate(None);

        coordinator
            .store
            .sync()
            .upsert_dataflow(dataflow, Some("demo".into()), "{}".into(), 1)
            .unwrap();
        coordinator
            .store
            .sync()
            .set_node_status(node_info(dataflow, "camera"))
            .unwrap();
        coordinator
            .store
            .sync()
            .set_param(dataflow, ParamKey::new("gain").unwrap(), param_value(7))
            .unwrap();
        coordinator
            .store
            .sync()
            .upsert_daemon(daemon_info(daemon.clone()))
            .unwrap();

        // Compact away every mutation logged so far: the delta path has
        // nothing left to diff from `MutationSeq::ZERO` (or from anywhere
        // at or before the watermark).
        let watermark = coordinator.store.sync().last_seq().unwrap();
        coordinator.store.sync().compact(watermark).unwrap();

        let (tx, mut rx) = mpsc::channel(8);
        let compacted = push_catch_up(
            &coordinator,
            &daemon,
            &tx,
            astrs_store::record::MutationSeq::ZERO,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                compacted,
                crate::error::CoordinatorError::Store(
                    astrs_store::Error::MutationHistoryCompacted { .. }
                )
            ),
            "{compacted}"
        );
        assert!(
            rx.try_recv().is_err(),
            "a failed push must not have queued a partial batch"
        );

        push_full_snapshot(&coordinator, &daemon, &tx)
            .await
            .unwrap();

        let event = rx.try_recv().unwrap();
        let CoordinatorEvent::StateCatchUp {
            seq,
            entries,
            final_batch,
        } = event
        else {
            panic!("unexpected {event:?}");
        };
        assert!(final_batch, "a snapshot is always complete in one batch");
        assert!(rx.try_recv().is_err(), "exactly one batch");

        // The cursor a reconnecting daemon would remember from this batch
        // must not itself already be compacted — otherwise the very next
        // reconnect reproduces the failure this function exists to recover
        // from.
        assert!(
            seq >= watermark.get(),
            "seq {seq} must be at or beyond the compaction watermark {watermark:?}"
        );
        assert!(
            coordinator
                .store
                .sync()
                .mutations_since(astrs_store::record::MutationSeq::new(seq), 10)
                .is_ok(),
            "the returned seq must itself be resumable"
        );

        assert!(entries.iter().any(|entry| matches!(
            &entry.kind,
            StateEntryKind::DataflowStatus { dataflow: got, status, .. }
                if *got == dataflow && *status == DataflowStatus::Pending
        )));
        assert!(entries.iter().any(|entry| matches!(
            &entry.kind,
            StateEntryKind::NodeState { dataflow: got, node, state, .. }
                if *got == dataflow && node.as_str() == "camera" && *state == NodeRunState::Running
        )));
        assert!(entries.iter().any(|entry| matches!(
            &entry.kind,
            StateEntryKind::ParamSet { key, value, .. }
                if key.as_str() == "gain" && *value == astrs_wire::Parameter::Integer(7)
        )));
        assert!(entries.iter().any(|entry| matches!(
            &entry.kind,
            StateEntryKind::DaemonPresence { daemon: got, connected: true } if *got == daemon
        )));
    }

    #[tokio::test]
    async fn push_full_snapshot_still_sends_one_empty_batch_when_nothing_is_registered() {
        let coordinator = coordinator();
        let (tx, mut rx) = mpsc::channel(8);
        push_full_snapshot(&coordinator, &DaemonId::generate(None), &tx)
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            CoordinatorEvent::StateCatchUp {
                entries,
                final_batch,
                seq,
            } => {
                assert!(entries.is_empty());
                assert!(final_batch);
                assert_eq!(seq, 0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_full_snapshot_reports_a_dropped_receiver_as_daemon_not_connected() {
        let coordinator = coordinator();
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        let daemon = DaemonId::generate(None);
        let err = push_full_snapshot(&coordinator, &daemon, &tx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::CoordinatorError::DaemonNotConnected(got) if got == daemon)
        );
    }
}
