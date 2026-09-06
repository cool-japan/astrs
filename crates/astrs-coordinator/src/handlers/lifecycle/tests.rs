//! Unit tests for the coordinator lifecycle handlers.
//!
//! Split out of `lifecycle.rs` to keep every file under the 2000-line
//! limit; `use super::*` still resolves to the handler module, so these
//! are the same tests against the same private items.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::CoordinatorConfig;
use crate::registry::DaemonHandle;
use astrs_time::HlcTimestamp;
use astrs_wire::{AuthToken, NodeExitCause, SessionId};
use tokio::sync::mpsc;

fn coordinator() -> Coordinator {
    Coordinator::open_in_memory(CoordinatorConfig::new(AuthToken::from_bytes([9; 32])).with_port(0))
        .unwrap()
}

fn connect_daemon(coordinator: &Coordinator) -> (DaemonId, mpsc::Receiver<CoordinatorEvent>) {
    let id = DaemonId::generate(None);
    let (tx, rx) = mpsc::channel(32);
    coordinator.daemons().insert(DaemonHandle::new(
        id.clone(),
        None,
        SessionId::generate(),
        tx,
        HlcTimestamp::EPOCH,
    ));
    (id, rx)
}

fn manifest_source(yaml: &str) -> DataflowSource {
    DataflowSource::from_manifest(yaml)
}

#[tokio::test]
async fn build_with_no_build_lines_goes_straight_to_ready() {
    let coordinator = coordinator();
    let (_daemon, _rx) = connect_daemon(&coordinator);
    let reply = build(
        &coordinator,
        "nodes:\n  - id: a\n    path: ./a\n".into(),
        None,
        None,
        false,
    )
    .await;
    let build_id = match reply {
        ControlReply::BuildStarted { build } => build,
        other => panic!("unexpected {other:?}"),
    };
    let dataflow = coordinator.dataflows().id_for_build(build_id).unwrap();
    let meta = coordinator
        .store
        .get_dataflow_meta(dataflow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.status, DataflowStatus::Ready);
}

#[tokio::test]
async fn build_dispatches_steps_and_wait_for_build_blocks_until_they_answer() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let reply = build(
        &coordinator,
        "nodes:\n  - id: a\n    path: ./a\n    build: cargo build\n".into(),
        None,
        None,
        false,
    )
    .await;
    let build_id = match reply {
        ControlReply::BuildStarted { build } => build,
        other => panic!("unexpected {other:?}"),
    };
    let dataflow = coordinator.dataflows().id_for_build(build_id).unwrap();
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Building
    );

    let dispatched = rx.try_recv().unwrap();
    assert!(matches!(dispatched, CoordinatorEvent::Build { .. }));

    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { wait_for_build(&coordinator, build_id, None).await }
    });
    // Give the waiter a moment to register before resolving the build.
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle_build_result(
        &coordinator,
        daemon_id,
        build_id,
        dataflow,
        BuildOutcome::Succeeded {
            artifacts: vec![],
            took: DurationMs::new(1),
        },
    )
    .await;
    assert_eq!(waiter.await.unwrap(), ControlReply::Ok);
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Ready
    );
}

#[tokio::test]
async fn wait_for_build_after_the_fact_still_answers_from_the_retained_outcome() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let reply = build(
        &coordinator,
        "nodes:\n  - id: a\n    path: ./a\n    build: cargo build\n".into(),
        None,
        None,
        false,
    )
    .await;
    let build_id = match reply {
        ControlReply::BuildStarted { build } => build,
        other => panic!("unexpected {other:?}"),
    };
    let dataflow = coordinator.dataflows().id_for_build(build_id).unwrap();
    let _ = rx.try_recv();
    handle_build_result(
        &coordinator,
        daemon_id,
        build_id,
        dataflow,
        BuildOutcome::Failed {
            node: None,
            exit_code: Some(1),
            message: "boom".into(),
            output: String::new(),
        },
    )
    .await;

    let reply = wait_for_build(&coordinator, build_id, None).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::BuildFailed));
}

#[tokio::test]
async fn wait_for_build_on_an_unknown_build_is_not_found() {
    let coordinator = coordinator();
    let reply = wait_for_build(&coordinator, BuildId::generate(), None).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::NotFound));
}

#[tokio::test]
async fn start_from_a_manifest_dispatches_spawn_and_waits_for_all_nodes_ready() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");

    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { start(&coordinator, source, Some("demo".into()), false).await }
    });
    let spawn = loop {
        if let Ok(event) = rx.try_recv() {
            break event;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let dataflow = match spawn {
        CoordinatorEvent::Spawn { node, .. } => node.dataflow,
        other => panic!("unexpected {other:?}"),
    };
    handle_all_nodes_ready(
        &coordinator,
        daemon_id,
        dataflow,
        vec![WireNodeId::new("a").unwrap()],
    )
    .await;

    let reply = waiter.await.unwrap();
    match reply {
        ControlReply::Started { name, .. } => assert_eq!(name, Some("demo".into())),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Running
    );
}

#[tokio::test]
async fn start_detached_returns_immediately() {
    let coordinator = coordinator();
    let (_daemon, mut rx) = connect_daemon(&coordinator);
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");
    let reply = start(&coordinator, source, None, true).await;
    assert!(matches!(reply, ControlReply::Started { .. }));
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::Spawn { .. }
    ));
}

#[tokio::test]
async fn start_with_no_daemons_connected_fails_cleanly() {
    let coordinator = coordinator();
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");
    let reply = start(&coordinator, source, None, true).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::Unavailable));
}

#[tokio::test]
async fn spawn_failures_are_reported_through_wait_for_spawn() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");
    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { start(&coordinator, source, None, false).await }
    });
    let dataflow = loop {
        if let Ok(CoordinatorEvent::Spawn { node, .. }) = rx.try_recv() {
            break node.dataflow;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    // Nothing reported ready: "a" (the only node dispatched) is
    // inferred failed.
    handle_all_nodes_ready(&coordinator, daemon_id, dataflow, vec![]).await;
    let reply = waiter.await.unwrap();
    assert_eq!(reply.error_code(), Some(ErrorCode::FailedPrecondition));
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Failed
    );
}

#[tokio::test]
async fn stop_refuses_a_dataflow_that_never_started() {
    let coordinator = coordinator();
    let (_daemon, _rx) = connect_daemon(&coordinator);
    let reply = build(
        &coordinator,
        "nodes:\n  - id: a\n    path: ./a\n".into(),
        None,
        None,
        false,
    )
    .await;
    let build_id = match reply {
        ControlReply::BuildStarted { build } => build,
        other => panic!("unexpected {other:?}"),
    };
    let dataflow = coordinator.dataflows().id_for_build(build_id).unwrap();
    let reply = stop(&coordinator, dataflow, None).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::FailedPrecondition));
}

#[tokio::test]
async fn stop_on_a_running_dataflow_dispatches_and_marks_stopping() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");
    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { start(&coordinator, source, None, false).await }
    });
    let dataflow = loop {
        if let Ok(CoordinatorEvent::Spawn { node, .. }) = rx.try_recv() {
            break node.dataflow;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    handle_all_nodes_ready(
        &coordinator,
        daemon_id,
        dataflow,
        vec![WireNodeId::new("a").unwrap()],
    )
    .await;
    assert_eq!(
        waiter.await.unwrap(),
        ControlReply::Started {
            dataflow,
            name: None
        }
    );
    // `handle_all_nodes_ready` also fanned the aggregate back out to
    // every hosting daemon (blueprint §4.3) — drain it before looking
    // for the `Stop` dispatch below.
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::AllNodesReady { .. }
    ));

    let reply = stop(&coordinator, dataflow, None).await;
    assert_eq!(reply, ControlReply::Ok);
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::StopDataflow { .. }
    ));
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Stopping
    );
}

#[tokio::test]
async fn stop_by_name_resolves_the_name_first() {
    let coordinator = coordinator();
    let reply = stop_by_name(&coordinator, "ghost".into(), None).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::NotFound));
}

/// Starts a named, single-node dataflow through to `Running` and drains
/// the `AllNodesReady` echo [`handle_all_nodes_ready`] fans back out —
/// the shared setup every `StopByName`/`Restart`/`RestartByName` happy-path
/// test below needs before it can exercise its own verb.
async fn start_named_dataflow(
    coordinator: &Coordinator,
    daemon_id: DaemonId,
    rx: &mut mpsc::Receiver<CoordinatorEvent>,
    name: &str,
) -> DataflowId {
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");
    let name = name.to_owned();
    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        let name = name.clone();
        async move { start(&coordinator, source, Some(name), false).await }
    });
    let dataflow = loop {
        if let Ok(CoordinatorEvent::Spawn { node, .. }) = rx.try_recv() {
            break node.dataflow;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    handle_all_nodes_ready(
        coordinator,
        daemon_id,
        dataflow,
        vec![WireNodeId::new("a").unwrap()],
    )
    .await;
    assert_eq!(
        waiter.await.unwrap(),
        ControlReply::Started {
            dataflow,
            name: Some(name)
        }
    );
    // `handle_all_nodes_ready` also fans the aggregate back out to every
    // hosting daemon (blueprint §4.3) — drain it so a caller's very next
    // `rx.try_recv()` sees whatever *its own* verb dispatches, not this
    // echo.
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::AllNodesReady { .. }
    ));
    dataflow
}

#[tokio::test]
async fn stop_by_name_stops_the_resolved_dataflow() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let dataflow = start_named_dataflow(&coordinator, daemon_id, &mut rx, "demo").await;

    let reply = stop_by_name(&coordinator, "demo".into(), None).await;
    assert_eq!(reply, ControlReply::Ok);
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::StopDataflow { .. }
    ));
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Stopping
    );
}

#[tokio::test]
async fn restart_on_an_unknown_dataflow_is_not_found() {
    let coordinator = coordinator();
    let reply = restart(&coordinator, DataflowId::generate(), false).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::NotFound));
}

#[tokio::test]
async fn restart_by_name_on_an_unknown_name_is_not_found() {
    let coordinator = coordinator();
    let reply = restart_by_name(&coordinator, "ghost".into(), false).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::NotFound));
}

#[tokio::test]
async fn restart_stops_then_respawns_with_a_bumped_generation() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let dataflow = start_named_dataflow(&coordinator, daemon_id, &mut rx, "demo").await;

    let reply = restart(&coordinator, dataflow, false).await;
    assert_eq!(reply, ControlReply::Ok);

    // `stop_inner`'s `StopDataflow` and `dispatch_spawn`'s fresh `Spawn`
    // ride the same per-daemon channel in that order (this function's
    // own doc comment) — no rebuild requested, so nothing else queues
    // between them.
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::StopDataflow { .. }
    ));
    match rx.try_recv().unwrap() {
        CoordinatorEvent::Spawn { node, .. } => {
            assert_eq!(node.generation, 2, "a second spawn advances past the first");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Starting
    );
}

#[tokio::test]
async fn restart_with_rebuild_dispatches_a_fresh_build_before_the_respawn() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    // `start_named_dataflow`'s own manifest has no `build:` line (`Start`
    // from a raw manifest never dispatches one either way — only
    // `rebuild` on an already-running dataflow does); this test needs
    // one, so it builds its own `LiveDataflow` directly rather than
    // reusing the shared helper.
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n    build: cargo build\n");
    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { start(&coordinator, source, Some("demo".into()), false).await }
    });
    let dataflow = loop {
        if let Ok(CoordinatorEvent::Spawn { node, .. }) = rx.try_recv() {
            break node.dataflow;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    handle_all_nodes_ready(
        &coordinator,
        daemon_id,
        dataflow,
        vec![WireNodeId::new("a").unwrap()],
    )
    .await;
    waiter.await.unwrap();
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::AllNodesReady { .. }
    ));

    let reply = restart(&coordinator, dataflow, true).await;
    assert_eq!(reply, ControlReply::Ok);

    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::StopDataflow { .. }
    ));
    assert!(
        matches!(rx.try_recv().unwrap(), CoordinatorEvent::Build { .. }),
        "rebuild dispatches a fresh Build before the respawn"
    );
    match rx.try_recv().unwrap() {
        CoordinatorEvent::Spawn { node, .. } => assert_eq!(node.generation, 2),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn restart_by_name_resolves_the_name_and_respawns() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let _dataflow = start_named_dataflow(&coordinator, daemon_id, &mut rx, "demo").await;

    let reply = restart_by_name(&coordinator, "demo".into(), false).await;
    assert_eq!(reply, ControlReply::Ok);
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::StopDataflow { .. }
    ));
    assert!(matches!(
        rx.try_recv().unwrap(),
        CoordinatorEvent::Spawn { .. }
    ));
}

#[tokio::test]
async fn destroy_without_force_refuses_while_something_is_active() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");
    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { start(&coordinator, source, None, false).await }
    });
    let dataflow = loop {
        if let Ok(CoordinatorEvent::Spawn { node, .. }) = rx.try_recv() {
            break node.dataflow;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    handle_all_nodes_ready(
        &coordinator,
        daemon_id,
        dataflow,
        vec![WireNodeId::new("a").unwrap()],
    )
    .await;
    waiter.await.unwrap();

    let reply = destroy(&coordinator, false).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::InvalidArgument));
    let reply = destroy(&coordinator, true).await;
    assert_eq!(reply, ControlReply::Ok);
    assert!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn clean_removes_only_terminal_dataflows() {
    let coordinator = coordinator();
    let active = DataflowId::generate();
    let finished = DataflowId::generate();
    coordinator
        .store
        .upsert_dataflow(active, None, "{}".into(), 0)
        .await
        .unwrap();
    coordinator
        .store
        .set_dataflow_status(active, DataflowStatus::Running, None)
        .await
        .unwrap();
    coordinator
        .store
        .upsert_dataflow(finished, None, "{}".into(), 0)
        .await
        .unwrap();
    coordinator
        .store
        .set_dataflow_status(finished, DataflowStatus::Finished, None)
        .await
        .unwrap();

    let reply = clean(&coordinator, None, false, false).await;
    assert_eq!(reply, ControlReply::Ok);
    assert!(
        coordinator
            .store
            .get_dataflow_meta(active)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        coordinator
            .store
            .get_dataflow_meta(finished)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn clean_on_a_named_active_dataflow_is_a_precondition_failure() {
    let coordinator = coordinator();
    let id = DataflowId::generate();
    coordinator
        .store
        .upsert_dataflow(id, None, "{}".into(), 0)
        .await
        .unwrap();
    coordinator
        .store
        .set_dataflow_status(id, DataflowStatus::Running, None)
        .await
        .unwrap();
    let reply = clean(&coordinator, Some(id), false, false).await;
    assert_eq!(reply.error_code(), Some(ErrorCode::FailedPrecondition));
}

#[tokio::test]
async fn a_node_crash_mid_run_is_visible_through_check_after_all_nodes_finished() {
    // Exercises the FSM matrix's "node crash mid-run" case end to end
    // via the store, the same path `handlers::info::check` reads.
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
    coordinator
        .store
        .set_node_status(astrs_wire::NodeInfo {
            dataflow: id,
            node: WireNodeId::new("a").unwrap(),
            daemon: DaemonId::generate(None),
            state: astrs_wire::NodeRunState::Failed,
            pid: None,
            generation: 1,
            restart_count: 0,
            inputs: Default::default(),
            outputs: Default::default(),
            started_at: None,
            exit_cause: Some(NodeExitCause::Signal {
                signal: 11,
                name: "SIGSEGV".into(),
            }),
        })
        .await
        .unwrap();
    coordinator
        .store
        .set_dataflow_status(id, DataflowStatus::Failed, None)
        .await
        .unwrap();

    let snapshot = coordinator
        .store
        .dataflow_snapshot(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.meta.status, DataflowStatus::Failed);
    assert_eq!(snapshot.nodes.len(), 1);
}

#[tokio::test]
async fn handle_spawn_result_persists_a_spawned_node() {
    let coordinator = coordinator();
    let dataflow = DataflowId::generate();
    let daemon = DaemonId::generate(None);
    handle_spawn_result(
        &coordinator,
        daemon.clone(),
        dataflow,
        WireNodeId::new("a").unwrap(),
        1,
        astrs_wire::SpawnOutcome::Spawned {
            pid: Some(4242),
            started_at: HlcTimestamp::new(1, 0),
        },
    )
    .await;
    let record = coordinator
        .store
        .get_node_status(dataflow, WireNodeId::new("a").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.info.pid, Some(4242));
    assert_eq!(record.info.state, astrs_wire::NodeRunState::Spawning);
    assert_eq!(record.info.daemon, daemon);
}

#[tokio::test]
async fn handle_spawn_result_records_a_typed_failure() {
    let coordinator = coordinator();
    let dataflow = DataflowId::generate();
    handle_spawn_result(
        &coordinator,
        DaemonId::generate(None),
        dataflow,
        WireNodeId::new("a").unwrap(),
        1,
        astrs_wire::SpawnOutcome::Failed {
            message: "no such file".into(),
            errno: Some(2),
        },
    )
    .await;
    let record = coordinator
        .store
        .get_node_status(dataflow, WireNodeId::new("a").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.info.state, astrs_wire::NodeRunState::Failed);
    assert!(matches!(
        record.info.exit_cause,
        Some(NodeExitCause::SpawnFailed { .. })
    ));
}

#[tokio::test]
async fn handle_node_stopped_bumps_restart_count_only_when_restarting() {
    let coordinator = coordinator();
    let dataflow = DataflowId::generate();
    let daemon = DaemonId::generate(None);
    handle_node_stopped(
        &coordinator,
        daemon.clone(),
        dataflow,
        WireNodeId::new("a").unwrap(),
        1,
        NodeExitCause::ExitCode { code: 1 },
        true,
    )
    .await;
    let record = coordinator
        .store
        .get_node_status(dataflow, WireNodeId::new("a").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.info.restart_count, 1);
    assert_eq!(record.info.state, astrs_wire::NodeRunState::Restarting);

    handle_node_stopped(
        &coordinator,
        daemon,
        dataflow,
        WireNodeId::new("a").unwrap(),
        2,
        NodeExitCause::Success,
        false,
    )
    .await;
    let record = coordinator
        .store
        .get_node_status(dataflow, WireNodeId::new("a").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.info.restart_count, 1,
        "a clean stop does not count as a restart"
    );
    assert_eq!(record.info.state, astrs_wire::NodeRunState::Finished);
}

#[tokio::test]
async fn all_nodes_finished_waits_for_every_hosting_daemon_before_finalizing() {
    let coordinator = coordinator();
    let dataflow = DataflowId::generate();
    let a = DaemonId::generate(None);
    let b = DaemonId::generate(None);
    coordinator
        .store
        .upsert_dataflow(dataflow, None, "{}".into(), 2)
        .await
        .unwrap();
    coordinator
        .store
        .set_dataflow_status(dataflow, DataflowStatus::Running, None)
        .await
        .unwrap();
    {
        let mut dataflows = coordinator.dataflows();
        let manifest = astrs_manifest::Manifest::from_yaml_str(
            "nodes:\n  - id: x\n    path: ./x\n  - id: y\n    path: ./y\n",
        )
        .unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        let mut live = LiveDataflow::new(dataflow, None, manifest, None, graph);
        live.placement
            .node_daemon
            .insert(WireNodeId::new("x").unwrap(), a.clone());
        live.placement
            .node_daemon
            .insert(WireNodeId::new("y").unwrap(), b.clone());
        dataflows.insert(live);
    }

    handle_all_nodes_finished(
        &coordinator,
        a,
        dataflow,
        BTreeMap::from([(WireNodeId::new("x").unwrap(), NodeExitCause::Success)]),
    )
    .await;
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Running,
        "one of two daemons has reported"
    );

    handle_all_nodes_finished(
        &coordinator,
        b,
        dataflow,
        BTreeMap::from([(
            WireNodeId::new("y").unwrap(),
            NodeExitCause::ExitCode { code: 1 },
        )]),
    )
    .await;
    assert_eq!(
        coordinator
            .store
            .get_dataflow_meta(dataflow)
            .await
            .unwrap()
            .unwrap()
            .status,
        DataflowStatus::Failed,
        "y's non-zero exit fails the whole run"
    );
}

#[tokio::test]
async fn a_daemon_disconnect_releases_a_pending_wait_for_build_as_a_failure() {
    let coordinator = coordinator();
    let (daemon_id, _rx) = connect_daemon(&coordinator);
    let reply = build(
        &coordinator,
        "nodes:\n  - id: a\n    path: ./a\n    build: cargo build\n".into(),
        None,
        None,
        false,
    )
    .await;
    let build_id = match reply {
        ControlReply::BuildStarted { build } => build,
        other => panic!("unexpected {other:?}"),
    };

    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { wait_for_build(&coordinator, build_id, None).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle_daemon_disconnected(&coordinator, daemon_id).await;
    let reply = waiter.await.unwrap();
    assert_eq!(reply.error_code(), Some(ErrorCode::BuildFailed));
}

#[tokio::test]
async fn a_daemon_disconnect_releases_a_pending_wait_for_spawn_as_a_failure() {
    let coordinator = coordinator();
    let (daemon_id, mut rx) = connect_daemon(&coordinator);
    let source = manifest_source("nodes:\n  - id: a\n    path: ./a\n");
    let waiter = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { start(&coordinator, source, None, false).await }
    });
    loop {
        if let Ok(CoordinatorEvent::Spawn { .. }) = rx.try_recv() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    handle_daemon_disconnected(&coordinator, daemon_id).await;
    let reply = waiter.await.unwrap();
    assert_eq!(reply.error_code(), Some(ErrorCode::FailedPrecondition));
}

#[tokio::test]
async fn a_disconnect_with_nothing_pending_is_a_harmless_no_op() {
    let coordinator = coordinator();
    // No dataflow at all — must not panic on an empty registry.
    handle_daemon_disconnected(&coordinator, DaemonId::generate(None)).await;
}
