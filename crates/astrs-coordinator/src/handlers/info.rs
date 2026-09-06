//! `List`/`Info`/`ConnectedDaemons`/`GetNodeInfo`/`Check` (blueprint §17):
//! every read verb that answers from durable state rather than live
//! connection bookkeeping.
//!
//! These all read `astrs-store` directly rather than the live
//! [`crate::registry::DataflowRegistry`]/[`crate::registry::DaemonRegistry`]:
//! every lifecycle transition persists before it is observable (see
//! [`crate::handlers::lifecycle`]), so the store is never behind, and
//! reading it means a query never needs to reach into another
//! connection's in-memory state.

use astrs_wire::{
    ControlReply, DaemonInfo, DataflowId, DataflowResult, DataflowStatus, DataflowSummary,
    ErrorCode, NodeId,
};

use crate::coordinator::Coordinator;

/// `List`.
pub async fn list(coordinator: &Coordinator, all: bool) -> ControlReply {
    match coordinator.store.list_dataflows().await {
        Ok(metas) => {
            let dataflows = metas
                .into_iter()
                .filter(|meta| all || !meta.status.is_terminal())
                .map(summary_of)
                .collect();
            ControlReply::DataflowList {
                dataflows,
                nodes: Vec::new(),
            }
        }
        Err(err) => crate::error::CoordinatorError::from(err).into_reply(),
    }
}

/// `Info`.
pub async fn info(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    include_nodes: bool,
) -> ControlReply {
    match coordinator.store.dataflow_snapshot(dataflow).await {
        Ok(Some(snapshot)) => {
            let nodes = if include_nodes {
                snapshot
                    .nodes
                    .values()
                    .map(|record| record.info.clone())
                    .collect()
            } else {
                Vec::new()
            };
            ControlReply::DataflowList {
                dataflows: vec![summary_of(snapshot.meta)],
                nodes,
            }
        }
        Ok(None) => crate::error::CoordinatorError::NoSuchDataflow(dataflow).into_reply(),
        Err(err) => crate::error::CoordinatorError::from(err).into_reply(),
    }
}

/// `ConnectedDaemons`.
pub async fn connected_daemons(
    coordinator: &Coordinator,
    include_unreachable: bool,
) -> ControlReply {
    match coordinator.store.list_daemons().await {
        Ok(records) => {
            let daemons: Vec<DaemonInfo> = records
                .into_iter()
                .filter(|record| include_unreachable || record.info.reachable)
                .map(|record| record.info)
                .collect();
            ControlReply::DaemonList { daemons }
        }
        Err(err) => crate::error::CoordinatorError::from(err).into_reply(),
    }
}

/// `GetNodeInfo`.
pub async fn get_node_info(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: NodeId,
) -> ControlReply {
    match coordinator
        .store
        .get_node_status(dataflow, node.clone())
        .await
    {
        Ok(Some(record)) => ControlReply::NodeInfo {
            nodes: vec![record.info],
        },
        Ok(None) => crate::error::CoordinatorError::NoSuchNode { dataflow, node }.into_reply(),
        Err(err) => crate::error::CoordinatorError::from(err).into_reply(),
    }
}

/// `Check`.
///
/// Names a dataflow: reports its [`DataflowResult`] if it has already
/// finished (success or failure alike — a finished check is exactly the
/// information a health check on a terminal run should surface), or `Ok`
/// while it is still pending/active. Names no dataflow: `Ok` when at least
/// one daemon is connected or none is needed yet (nothing to check against
/// otherwise counts as healthy-by-vacuity), [`astrs_wire::ErrorCode::Unavailable`]
/// when dataflows are registered but no daemon at all is connected to run
/// them.
pub async fn check(coordinator: &Coordinator, dataflow: Option<DataflowId>) -> ControlReply {
    match dataflow {
        Some(id) => check_one(coordinator, id).await,
        None => check_cluster(coordinator).await,
    }
}

async fn check_one(coordinator: &Coordinator, id: DataflowId) -> ControlReply {
    match coordinator.store.dataflow_snapshot(id).await {
        Ok(Some(snapshot)) if snapshot.meta.status.is_terminal() => ControlReply::DataflowResult {
            result: Box::new(result_of(&snapshot)),
        },
        Ok(Some(_)) => ControlReply::Ok,
        Ok(None) => crate::error::CoordinatorError::NoSuchDataflow(id).into_reply(),
        Err(err) => crate::error::CoordinatorError::from(err).into_reply(),
    }
}

/// `GetNodeMetrics` (`astrs top`, blueprint §13).
///
/// Reads the *live* registry's
/// [`crate::registry::LiveDataflow::latest_metrics`] rather than
/// `astrs-store`, unlike every other handler in this module: a metrics
/// sample is intentionally ephemeral (see that field's docs) — there is
/// nothing durable to read, and rebuilding one from the store would mean
/// persisting a value whose entire point is "the coordinator's live,
/// two-second-old reading."
pub async fn get_node_metrics(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: Option<NodeId>,
) -> ControlReply {
    let registry = coordinator.dataflows();
    let Some(live) = registry.get(dataflow) else {
        return crate::error::CoordinatorError::NoSuchDataflow(dataflow).into_reply();
    };
    let samples = match node {
        Some(node) => live
            .latest_metrics
            .get(&node)
            .cloned()
            .into_iter()
            .collect(),
        None => live.latest_metrics.values().cloned().collect(),
    };
    ControlReply::NodeMetrics { samples }
}

/// `GetNodeIoMetrics` (`astrs top`'s throughput column, blueprint §6.2/§13).
///
/// The bandwidth twin of [`get_node_metrics`], reading
/// [`crate::registry::LiveDataflow::latest_io`] under exactly the same
/// ephemerality rule.
pub async fn get_node_io_metrics(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: Option<NodeId>,
) -> ControlReply {
    let registry = coordinator.dataflows();
    let Some(live) = registry.get(dataflow) else {
        return crate::error::CoordinatorError::NoSuchDataflow(dataflow).into_reply();
    };
    let samples = match node {
        Some(node) => live.latest_io.get(&node).cloned().into_iter().collect(),
        None => live.latest_io.values().cloned().collect(),
    };
    ControlReply::NodeIoMetrics { samples }
}

/// `GetManifest` (`astrs top`'s Graph tab, blueprint §5.2).
///
/// Re-serializes [`crate::registry::LiveDataflow::manifest`] — the
/// coordinator's already-expanded, in-memory copy — to YAML, for the same
/// reason [`get_node_metrics`] reads the live registry: nothing else in
/// this coordinator holds a serializable manifest. `astrs-wire` cannot
/// carry the parsed [`astrs_graph::DataflowGraph`] type directly (it sits
/// below that domain crate in the layer stack, blueprint §4.1), so the
/// receiving end parses this YAML itself — exactly as `astrs graph`
/// already does for a manifest file on disk.
pub async fn get_manifest(coordinator: &Coordinator, dataflow: DataflowId) -> ControlReply {
    let registry = coordinator.dataflows();
    let Some(live) = registry.get(dataflow) else {
        return crate::error::CoordinatorError::NoSuchDataflow(dataflow).into_reply();
    };
    match astrs_yaml::to_string(&live.manifest) {
        Ok(yaml) => ControlReply::Manifest {
            yaml,
            working_dir: live.working_dir.clone(),
        },
        // A manifest that reached the live registry already round-tripped
        // through `Manifest::from_yaml_str` once; re-serializing it
        // failing is not a case this build can usefully recover from, but
        // it must still answer rather than panic.
        Err(error) => ControlReply::error(
            ErrorCode::Internal,
            format!("failed to serialize the dataflow's manifest: {error}"),
        ),
    }
}

async fn check_cluster(coordinator: &Coordinator) -> ControlReply {
    let has_dataflows = match coordinator.store.list_dataflows().await {
        Ok(metas) => metas.iter().any(|meta| !meta.status.is_terminal()),
        Err(err) => return crate::error::CoordinatorError::from(err).into_reply(),
    };
    let has_daemons = !coordinator.daemons().is_empty();
    if has_dataflows && !has_daemons {
        return crate::error::CoordinatorError::NoDaemonsConnected.into_reply();
    }
    ControlReply::Ok
}

fn summary_of(meta: astrs_store::record::DataflowMeta) -> DataflowSummary {
    let running_nodes = if meta.status == DataflowStatus::Running {
        meta.node_count
    } else {
        0
    };
    DataflowSummary {
        id: meta.id,
        name: meta.name,
        status: meta.status,
        daemons: meta.daemons,
        node_count: meta.node_count,
        running_nodes,
        started_at: meta.started_at,
    }
}

fn result_of(snapshot: &astrs_store::record::DataflowSnapshot) -> DataflowResult {
    let started_at = snapshot
        .meta
        .started_at
        .unwrap_or(astrs_time::HlcTimestamp::EPOCH);
    let mut result = DataflowResult::new(snapshot.meta.id, started_at);
    for record in snapshot.nodes.values() {
        if let Some(cause) = record.info.exit_cause.clone() {
            result.record(record.info.node.clone(), cause);
        }
    }
    result.finish(snapshot.meta.updated_at);
    result.status = snapshot.meta.status;
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_wire::{AuthToken, NodeExitCause, NodeInfo, NodeRunState};

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([5; 32])).with_port(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn list_excludes_finished_dataflows_unless_all_is_set() {
        let coordinator = coordinator();
        let id = DataflowId::generate();
        coordinator
            .store
            .upsert_dataflow(id, None, "{}".into(), 1)
            .await
            .unwrap();
        coordinator
            .store
            .set_dataflow_status(id, DataflowStatus::Finished, None)
            .await
            .unwrap();

        let reply = list(&coordinator, false).await;
        match reply {
            ControlReply::DataflowList { dataflows, .. } => assert!(dataflows.is_empty()),
            other => panic!("unexpected {other:?}"),
        }

        let reply = list(&coordinator, true).await;
        match reply {
            ControlReply::DataflowList { dataflows, .. } => assert_eq!(dataflows.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn info_on_an_unregistered_dataflow_is_not_found() {
        let coordinator = coordinator();
        let reply = info(&coordinator, DataflowId::generate(), false).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn info_includes_nodes_only_when_asked() {
        let coordinator = coordinator();
        let id = DataflowId::generate();
        coordinator
            .store
            .upsert_dataflow(id, None, "{}".into(), 1)
            .await
            .unwrap();
        coordinator
            .store
            .set_node_status(NodeInfo {
                dataflow: id,
                node: NodeId::new("a").unwrap(),
                daemon: astrs_wire::DaemonId::generate(None),
                state: NodeRunState::Running,
                pid: None,
                generation: 1,
                restart_count: 0,
                inputs: Default::default(),
                outputs: Default::default(),
                started_at: None,
                exit_cause: None,
            })
            .await
            .unwrap();

        let reply = info(&coordinator, id, false).await;
        assert!(matches!(reply, ControlReply::DataflowList { nodes, .. } if nodes.is_empty()));

        let reply = info(&coordinator, id, true).await;
        assert!(matches!(reply, ControlReply::DataflowList { nodes, .. } if nodes.len() == 1));
    }

    #[tokio::test]
    async fn connected_daemons_filters_unreachable_by_default() {
        let coordinator = coordinator();
        let reachable = astrs_wire::DaemonId::generate(None);
        let unreachable = astrs_wire::DaemonId::generate(None);
        for (id, ok) in [(&reachable, true), (&unreachable, false)] {
            coordinator
                .store
                .upsert_daemon(DaemonInfo {
                    id: id.clone(),
                    version: astrs_wire::AstrsVersion::current(),
                    address: "x".into(),
                    connected_at: astrs_time::HlcTimestamp::EPOCH,
                    node_count: 0,
                    labels: Default::default(),
                    reachable: ok,
                })
                .await
                .unwrap();
        }

        let reply = connected_daemons(&coordinator, false).await;
        match reply {
            ControlReply::DaemonList { daemons } => assert_eq!(daemons.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
        let reply = connected_daemons(&coordinator, true).await;
        match reply {
            ControlReply::DaemonList { daemons } => assert_eq!(daemons.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_node_info_reports_not_found_for_a_missing_node() {
        let coordinator = coordinator();
        let reply = get_node_info(
            &coordinator,
            DataflowId::generate(),
            NodeId::new("x").unwrap(),
        )
        .await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn check_reports_ok_for_a_pending_dataflow_and_a_result_once_finished() {
        let coordinator = coordinator();
        let id = DataflowId::generate();
        coordinator
            .store
            .upsert_dataflow(id, None, "{}".into(), 1)
            .await
            .unwrap();
        assert_eq!(check(&coordinator, Some(id)).await, ControlReply::Ok);

        coordinator
            .store
            .set_node_status(NodeInfo {
                dataflow: id,
                node: NodeId::new("a").unwrap(),
                daemon: astrs_wire::DaemonId::generate(None),
                state: NodeRunState::Failed,
                pid: None,
                generation: 1,
                restart_count: 0,
                inputs: Default::default(),
                outputs: Default::default(),
                started_at: None,
                exit_cause: Some(NodeExitCause::ExitCode { code: 1 }),
            })
            .await
            .unwrap();
        coordinator
            .store
            .set_dataflow_status(id, DataflowStatus::Failed, None)
            .await
            .unwrap();

        let reply = check(&coordinator, Some(id)).await;
        match reply {
            ControlReply::DataflowResult { result } => {
                assert_eq!(result.status, DataflowStatus::Failed);
                assert_eq!(result.failed_nodes().count(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_cluster_is_ok_with_no_dataflows_at_all() {
        let coordinator = coordinator();
        assert_eq!(check(&coordinator, None).await, ControlReply::Ok);
    }

    fn live_dataflow(id: DataflowId) -> crate::registry::LiveDataflow {
        let manifest = astrs_manifest::Manifest::from_yaml_str(
            "name: perception\nnodes:\n  - id: camera\n    path: ./camera\n",
        )
        .unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        crate::registry::LiveDataflow::new(id, Some("perception".to_owned()), manifest, None, graph)
    }

    #[tokio::test]
    async fn get_node_metrics_reports_not_found_for_an_unregistered_dataflow() {
        let coordinator = coordinator();
        let reply = get_node_metrics(&coordinator, DataflowId::generate(), None).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn get_node_metrics_returns_every_sample_or_one_by_node() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let mut live = live_dataflow(dataflow);
        let camera = NodeId::new("camera").unwrap();
        let detector = NodeId::new("detector").unwrap();
        live.latest_metrics.insert(
            camera.clone(),
            astrs_wire::NodeMetricsSample::new(camera.clone(), astrs_time::HlcTimestamp::EPOCH),
        );
        live.latest_metrics.insert(
            detector.clone(),
            astrs_wire::NodeMetricsSample::new(detector.clone(), astrs_time::HlcTimestamp::EPOCH),
        );
        coordinator.dataflows().insert(live);

        let reply = get_node_metrics(&coordinator, dataflow, None).await;
        match reply {
            ControlReply::NodeMetrics { samples } => assert_eq!(samples.len(), 2),
            other => panic!("unexpected {other:?}"),
        }

        let reply = get_node_metrics(&coordinator, dataflow, Some(camera.clone())).await;
        match reply {
            ControlReply::NodeMetrics { samples } => {
                assert_eq!(samples.len(), 1);
                assert_eq!(samples[0].node, camera);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_node_metrics_on_a_node_with_no_sample_yet_is_empty_not_an_error() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.dataflows().insert(live_dataflow(dataflow));
        let reply =
            get_node_metrics(&coordinator, dataflow, Some(NodeId::new("camera").unwrap())).await;
        assert!(matches!(reply, ControlReply::NodeMetrics { samples } if samples.is_empty()));
    }

    #[tokio::test]
    async fn get_node_io_metrics_answers_from_the_bandwidth_half_of_the_registry() {
        // The tail-appended twin of `GetNodeMetrics`: a separate verb reading
        // a separate map, because the daemon leg that fills it is a separate
        // (necessarily separate — see `astrs_wire::NodeIoSample`) variant.
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let mut live = live_dataflow(dataflow);
        let camera = NodeId::new("camera").unwrap();
        let detector = NodeId::new("detector").unwrap();
        let mut sample =
            astrs_wire::NodeIoSample::new(camera.clone(), astrs_time::HlcTimestamp::EPOCH);
        sample
            .sent_bytes_total
            .insert(astrs_wire::DataId::new("image").unwrap(), 4_096);
        sample.shm_slots_in_use = 2;
        sample.shm_slots_total = 8;
        live.latest_io.insert(camera.clone(), sample);
        live.latest_io.insert(
            detector.clone(),
            astrs_wire::NodeIoSample::new(detector, astrs_time::HlcTimestamp::EPOCH),
        );
        coordinator.dataflows().insert(live);

        match get_node_io_metrics(&coordinator, dataflow, None).await {
            ControlReply::NodeIoMetrics { samples } => assert_eq!(samples.len(), 2),
            other => panic!("unexpected {other:?}"),
        }

        match get_node_io_metrics(&coordinator, dataflow, Some(camera.clone())).await {
            ControlReply::NodeIoMetrics { samples } => {
                assert_eq!(samples.len(), 1);
                assert_eq!(samples[0].node, camera);
                assert_eq!(samples[0].total_sent_bytes(), 4_096);
                assert_eq!(samples[0].shm_slot_occupancy(), Some(0.25));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_node_io_metrics_reports_not_found_for_an_unregistered_dataflow() {
        let coordinator = coordinator();
        let reply = get_node_io_metrics(&coordinator, DataflowId::generate(), None).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn get_manifest_reports_not_found_for_an_unregistered_dataflow() {
        let coordinator = coordinator();
        let reply = get_manifest(&coordinator, DataflowId::generate()).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn get_manifest_returns_yaml_that_reparses_to_the_same_graph() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.dataflows().insert(live_dataflow(dataflow));

        let reply = get_manifest(&coordinator, dataflow).await;
        let (yaml, working_dir) = match reply {
            ControlReply::Manifest { yaml, working_dir } => (yaml, working_dir),
            other => panic!("unexpected {other:?}"),
        };
        assert!(working_dir.is_none());

        let reparsed = astrs_manifest::Manifest::from_yaml_str(&yaml).unwrap();
        reparsed.validate().unwrap();
        assert_eq!(reparsed.name.as_deref(), Some("perception"));
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&reparsed).unwrap();
        assert_eq!(graph.node_count(), 1);
    }

    #[tokio::test]
    async fn check_cluster_is_unavailable_when_active_work_has_no_daemons() {
        let coordinator = coordinator();
        let id = DataflowId::generate();
        coordinator
            .store
            .upsert_dataflow(id, None, "{}".into(), 1)
            .await
            .unwrap();
        coordinator
            .store
            .set_dataflow_status(id, DataflowStatus::Running, None)
            .await
            .unwrap();
        let reply = check(&coordinator, None).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::Unavailable));
    }
}
