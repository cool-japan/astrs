//! `RecordStart`/`RecordStop`/`GetTraces` (blueprint §14, §13, §17).

use astrs_time::HlcTimestamp;
use astrs_wire::{
    ControlReply, CoordinatorEvent, DataId, DataflowId, InputSpec, NodeId, NodeSource,
    NodeSpawnSpec, PortRef, StopCause,
};

use crate::coordinator::Coordinator;
use crate::error::CoordinatorError;
use crate::placement;

/// `RecordStart`: synthesizes a [`NodeSource::Recorder`] node wired to the
/// requested ports (every producer output in the dataflow when `ports` is
/// empty, per the request's own doc), and spawns it dynamically — reusing
/// exactly the mechanism [`super::topology::add_node`] uses for any other
/// dynamically added node.
pub async fn record_start(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    path: String,
    ports: Vec<PortRef>,
    overwrite: bool,
) -> ControlReply {
    let already_recording = {
        let dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        live.recording_node.clone()
    };
    if already_recording.is_some() && !overwrite {
        return CoordinatorError::AlreadyExists {
            kind: "recording",
            name: dataflow.to_string(),
        }
        .into_reply();
    }

    let daemon_id = {
        let daemons = coordinator.daemons();
        match placement::resolve_machine(&astrs_graph::MachineId::CoordinatorLocal, &daemons) {
            Ok(id) => id,
            Err(err) => return err.into_reply(),
        }
    };

    let recorder_id = match NodeId::new(format!("__record_{}", record_slug(dataflow))) {
        Ok(id) => id,
        Err(err) => return CoordinatorError::from(err).into_reply(),
    };

    let ports = {
        let dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        if ports.is_empty() {
            all_producer_ports(live)
        } else {
            ports
        }
    };
    let inputs = match ports_to_inputs(&ports) {
        Ok(inputs) => inputs,
        Err(err) => return err.into_reply(),
    };

    let mut spec = NodeSpawnSpec::new(
        dataflow,
        recorder_id.clone(),
        1,
        NodeSource::Recorder { path },
    );
    spec.inputs = inputs;

    {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        live.placement
            .dynamic_daemons
            .insert(recorder_id.clone(), daemon_id.clone());
        live.recording_node = Some(recorder_id);
    }

    let name = coordinator
        .store
        .get_dataflow_meta(dataflow)
        .await
        .ok()
        .flatten()
        .and_then(|meta| meta.name);
    let event = CoordinatorEvent::Spawn {
        node: Box::new(spec),
        routes: Vec::new(),
        dataflow_name: name,
    };
    let daemons = coordinator.daemons();
    match daemons.get(&daemon_id) {
        Some(handle) => match handle.send(event) {
            Ok(()) => ControlReply::Ok,
            Err(err) => err.into_reply(),
        },
        None => CoordinatorError::DaemonNotConnected(daemon_id).into_reply(),
    }
}

/// `RecordStop`: stops the recorder node [`record_start`] opened, if any.
pub async fn record_stop(coordinator: &Coordinator, dataflow: DataflowId) -> ControlReply {
    let (daemon_id, recorder_id) = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        let Some(recorder_id) = live.recording_node.take() else {
            return CoordinatorError::invalid(format!("dataflow {dataflow} is not recording"))
                .into_reply();
        };
        let Some(daemon_id) = live.daemon_for_node(&recorder_id).cloned() else {
            return CoordinatorError::NoSuchNode {
                dataflow,
                node: recorder_id,
            }
            .into_reply();
        };
        (daemon_id, recorder_id)
    };
    let event = CoordinatorEvent::StopNode {
        dataflow,
        node: recorder_id,
        grace: None,
        cause: StopCause::Requested,
    };
    let daemons = coordinator.daemons();
    match daemons.get(&daemon_id) {
        Some(handle) => match handle.send(event) {
            Ok(()) => ControlReply::Ok,
            Err(err) => err.into_reply(),
        },
        None => CoordinatorError::DaemonNotConnected(daemon_id).into_reply(),
    }
}

/// `GetTraces`.
///
/// Answers from [`Coordinator::traces`] — this coordinator's own buffer of
/// finished control-request spans (see [`crate::trace`] for what is, and
/// is not, in there). No `DaemonEvent`/`CoordinatorEvent` pair exists yet
/// for pulling collected spans out of a daemon, so a node-side or
/// cross-process causal chain is not part of what this answers; a
/// dataflow/node/since/limit filter over this coordinator's own recent
/// work is.
pub async fn get_traces(
    coordinator: &Coordinator,
    dataflow: Option<DataflowId>,
    node: Option<NodeId>,
    since: Option<HlcTimestamp>,
    limit: Option<u32>,
) -> ControlReply {
    let traces = coordinator
        .traces()
        .query(dataflow, node.as_ref(), since, limit);
    ControlReply::TraceData { traces }
}

/// A path-safe, deterministic recorder node id for `dataflow`.
fn record_slug(dataflow: DataflowId) -> String {
    dataflow.as_u128().to_string()
}

/// Builds one positionally-named [`InputSpec`] per port.
fn ports_to_inputs(ports: &[PortRef]) -> crate::error::Result<Vec<InputSpec>> {
    ports
        .iter()
        .enumerate()
        .map(|(index, port)| {
            let id = DataId::new(format!("in{index}"))?;
            Ok(InputSpec::new(id, port.clone()))
        })
        .collect()
}

/// Every declared output of every manifest node in `live`, as `node/output`
/// port references — the "record every port" default.
fn all_producer_ports(live: &crate::registry::LiveDataflow) -> Vec<PortRef> {
    let mut ports = Vec::new();
    for node in &live.manifest.nodes {
        let Ok(node_id) = NodeId::new(&node.id) else {
            continue;
        };
        for output in &node.outputs {
            if let Ok(data_id) = DataId::new(output) {
                ports.push(PortRef::new(node_id.clone(), data_id));
            }
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::registry::{DaemonHandle, LiveDataflow};
    use astrs_wire::{AuthToken, SessionId};
    use tokio::sync::mpsc;

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([8; 32])).with_port(0),
        )
        .unwrap()
    }

    fn live_dataflow(id: DataflowId) -> LiveDataflow {
        let manifest = astrs_manifest::Manifest::from_yaml_str(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n",
        )
        .unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        LiveDataflow::new(id, None, manifest, None, graph)
    }

    fn connect_daemon(coordinator: &Coordinator) -> mpsc::Receiver<CoordinatorEvent> {
        let id = astrs_wire::DaemonId::generate(None);
        let (tx, rx) = mpsc::channel(8);
        coordinator.daemons().insert(DaemonHandle::new(
            id,
            None,
            SessionId::generate(),
            tx,
            astrs_time::HlcTimestamp::EPOCH,
        ));
        rx
    }

    #[tokio::test]
    async fn record_start_with_no_ports_records_every_declared_output() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let mut rx = connect_daemon(&coordinator);
        coordinator.dataflows().insert(live_dataflow(dataflow));

        let reply = record_start(&coordinator, dataflow, "out.arec".into(), vec![], false).await;
        assert_eq!(reply, ControlReply::Ok);
        match rx.try_recv().unwrap() {
            CoordinatorEvent::Spawn { node, .. } => {
                assert!(matches!(node.source, NodeSource::Recorder { .. }));
                assert_eq!(node.inputs.len(), 1);
                assert_eq!(node.inputs[0].source.to_string(), "camera/frames");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            coordinator
                .dataflows()
                .get(dataflow)
                .unwrap()
                .recording_node
                .is_some()
        );
    }

    #[tokio::test]
    async fn record_start_twice_without_overwrite_is_rejected() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let _rx = connect_daemon(&coordinator);
        coordinator.dataflows().insert(live_dataflow(dataflow));

        record_start(&coordinator, dataflow, "out.arec".into(), vec![], false).await;
        let reply = record_start(&coordinator, dataflow, "out.arec".into(), vec![], false).await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::AlreadyExists)
        );
    }

    #[tokio::test]
    async fn record_stop_without_an_active_recording_is_invalid() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.dataflows().insert(live_dataflow(dataflow));
        let reply = record_stop(&coordinator, dataflow).await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::InvalidArgument)
        );
    }

    #[tokio::test]
    async fn record_stop_after_start_dispatches_a_stop_node() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let mut rx = connect_daemon(&coordinator);
        coordinator.dataflows().insert(live_dataflow(dataflow));
        record_start(&coordinator, dataflow, "out.arec".into(), vec![], false).await;
        let _ = rx.try_recv(); // drain the Spawn

        let reply = record_stop(&coordinator, dataflow).await;
        assert_eq!(reply, ControlReply::Ok);
        assert!(matches!(
            rx.try_recv().unwrap(),
            CoordinatorEvent::StopNode { .. }
        ));
        assert!(
            coordinator
                .dataflows()
                .get(dataflow)
                .unwrap()
                .recording_node
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_traces_on_a_fresh_coordinator_is_empty_rather_than_an_error() {
        let coordinator = coordinator();
        let reply = get_traces(&coordinator, None, None, None, None).await;
        match reply {
            ControlReply::TraceData { traces } => assert!(traces.spans.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_traces_answers_from_the_coordinators_own_span_buffer() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.traces().push(crate::trace::control_span(
            "SetParam",
            Some(dataflow),
            1,
            astrs_time::HlcTimestamp::EPOCH,
            astrs_time::HlcTimestamp::EPOCH,
            astrs_wire::SpanStatus::Ok,
        ));
        coordinator.traces().push(crate::trace::control_span(
            "GetParam",
            None,
            2,
            astrs_time::HlcTimestamp::EPOCH,
            astrs_time::HlcTimestamp::EPOCH,
            astrs_wire::SpanStatus::Ok,
        ));

        let reply = get_traces(&coordinator, Some(dataflow), None, None, None).await;
        match reply {
            ControlReply::TraceData { traces } => {
                assert_eq!(traces.spans.len(), 1);
                assert_eq!(traces.spans[0].name, "SetParam");
            }
            other => panic!("unexpected {other:?}"),
        }

        let reply = get_traces(&coordinator, None, None, None, None).await;
        match reply {
            ControlReply::TraceData { traces } => assert_eq!(traces.spans.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
    }
}
