//! `Logs`/`LogSubscribe`/`TopicSubscribe`/`TopicUnsubscribe`/`TopicPublish`
//! (blueprint §4.2, §7.3, §17): fan-out log/topic delivery to CLI
//! subscribers, and the one-shot historical fetches that do not subscribe
//! to anything.

use std::time::Duration;

use astrs_wire::{
    ControlReply, CoordinatorEvent, DataflowId, LogQuery, Metadata, NodeId, PortRef,
    SubscriptionId, TopicQuery,
};
use tokio::sync::{mpsc, oneshot};

use crate::coordinator::Coordinator;
use crate::error::CoordinatorError;
use crate::registry::subscription::{LogSubscription, TopicSubscription};
use crate::registry::{PendingLogFetch, SubscriberHandle, SubscriptionKind};
use crate::session::CliOutbound;

/// How long a one-shot `Logs` fan-out waits for every hosting daemon to
/// answer before giving up.
const LOG_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// `Logs`: a bounded, one-shot fetch across every daemon hosting
/// `dataflow`.
pub async fn logs(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: Option<NodeId>,
    query: LogQuery,
) -> ControlReply {
    let hosting: Vec<_> = {
        let dataflows = coordinator.dataflows();
        match dataflows.get(dataflow) {
            Some(live) => live.hosting_daemons().into_iter().collect(),
            None => return CoordinatorError::NoSuchDataflow(dataflow).into_reply(),
        }
    };
    if hosting.is_empty() {
        return ControlReply::Logs {
            records: Vec::new(),
            truncated: false,
        };
    }

    let request_id = coordinator.next_request_id();
    let (waiter_tx, waiter_rx) = oneshot::channel();
    {
        let mut fetch = PendingLogFetch::new(hosting.clone());
        fetch.waiters.push(waiter_tx);
        coordinator.pending_logs().insert(request_id, fetch);
    }

    let event = CoordinatorEvent::Logs {
        request: request_id,
        dataflow: Some(dataflow),
        node,
        query,
    };
    {
        let daemons = coordinator.daemons();
        for id in &hosting {
            if let Some(handle) = daemons.get(id) {
                let _ = handle.send(event.clone());
            }
        }
    }

    match tokio::time::timeout(LOG_FETCH_TIMEOUT, waiter_rx).await {
        Ok(Ok(mut result)) => {
            result.records.sort_by_key(|record| record.timestamp);
            ControlReply::Logs {
                records: result.records,
                truncated: result.truncated,
            }
        }
        _ => {
            coordinator.pending_logs().remove(&request_id);
            CoordinatorError::Timeout("log fetch").into_reply()
        }
    }
}

/// `LogSubscribe`: opens a live, push-fed log tail — see
/// [`crate::registry::subscription`] for how a spontaneous
/// `DaemonEvent::Log` push gets matched against every open subscription's
/// filter.
pub async fn log_subscribe(
    coordinator: &Coordinator,
    outbound: &mpsc::Sender<CliOutbound>,
    dataflow: Option<DataflowId>,
    node: Option<NodeId>,
    query: LogQuery,
    subscription: SubscriptionId,
) -> ControlReply {
    coordinator.subscriptions().insert(SubscriberHandle {
        id: subscription,
        kind: SubscriptionKind::Log(LogSubscription {
            dataflow,
            node,
            query,
        }),
        sender: outbound.clone(),
        dropped: 0,
    });
    ControlReply::Ok
}

/// `TopicSubscribe`: opens a tap and asks the producer's daemon to start
/// streaming it.
pub async fn topic_subscribe(
    coordinator: &Coordinator,
    outbound: &mpsc::Sender<CliOutbound>,
    dataflow: DataflowId,
    port: PortRef,
    query: TopicQuery,
    subscription: SubscriptionId,
) -> ControlReply {
    let daemon_id = {
        let dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        // Blueprint §13: taps are "explicitly enabled per dataflow with
        // `debug: true`" — checked here, once, against the manifest this
        // coordinator already holds, rather than round-tripping to the
        // daemon to find out. This is also the *only* place that check is
        // made: a cluster daemon never sees the manifest at all (a
        // coordinator `Spawn` carries a fully expanded `NodeSpawnSpec`, no
        // `debug` field anywhere on the wire — see
        // `astrs_daemon::coordinator::apply`'s own docs on
        // `subscribe_spec_virtual_inputs`), so it has no way to verify this
        // independently. `astrs-daemon`'s `tap::TapRegistry::subscribe` does
        // have a real `is_enabled` gate, but the daemon's only caller
        // (`CoordinatorEvent::TopicTapStart`'s handler) calls
        // `TapRegistry::enable` unconditionally immediately before
        // subscribing, so that gate always passes for whatever this
        // function already approved — it is bookkeeping for
        // `TapRegistry::taps_output`'s SHM-route-upgrade refusal, not a
        // second, independent policy check. A dataflow that never opted in
        // gets a plain refusal here instead of a tap that would silently
        // see nothing.
        if !live.manifest.debug {
            return CoordinatorError::invalid(format!(
                "dataflow {dataflow} does not have `debug: true`; topic taps \
                 (`astrs topic echo/hz/info`) are refused until its manifest opts in"
            ))
            .into_reply();
        }
        live.daemon_for_node(port.node()).cloned()
    };
    let Some(daemon_id) = daemon_id else {
        return CoordinatorError::NoSuchNode {
            dataflow,
            node: port.node().clone(),
        }
        .into_reply();
    };

    coordinator.subscriptions().insert(SubscriberHandle {
        id: subscription,
        kind: SubscriptionKind::Topic(TopicSubscription {
            dataflow,
            port: port.clone(),
            query,
        }),
        sender: outbound.clone(),
        dropped: 0,
    });

    let event = CoordinatorEvent::TopicTapStart {
        dataflow,
        port,
        query,
        subscription,
    };
    let sent = coordinator
        .daemons()
        .get(&daemon_id)
        .map(|handle| handle.send(event));
    match sent {
        Some(Ok(())) => ControlReply::Ok,
        Some(Err(err)) => err.into_reply(),
        None => CoordinatorError::DaemonNotConnected(daemon_id).into_reply(),
    }
}

/// `TopicUnsubscribe`: closes a log or topic subscription, telling the
/// daemon to stop tapping if it was a topic tap.
pub async fn topic_unsubscribe(
    coordinator: &Coordinator,
    subscription: SubscriptionId,
) -> ControlReply {
    let removed = coordinator.subscriptions().remove(subscription);
    let Some(handle) = removed else {
        return CoordinatorError::invalid(format!("no such subscription: {subscription}"))
            .into_reply();
    };
    if let SubscriptionKind::Topic(topic) = &handle.kind {
        let daemon_id = {
            let dataflows = coordinator.dataflows();
            dataflows
                .get(topic.dataflow)
                .and_then(|live| live.daemon_for_node(topic.port.node()).cloned())
        };
        if let Some(daemon_id) = daemon_id {
            let daemons = coordinator.daemons();
            if let Some(handle) = daemons.get(&daemon_id) {
                let _ = handle.send(CoordinatorEvent::TopicTapStop { subscription });
            }
        }
    }
    ControlReply::Ok
}

/// `TopicPublish`.
///
/// No `CoordinatorEvent` verb exists yet to inject a caller-supplied
/// message into a running dataflow's port — §24.1 lists no such variant,
/// and adding one is a wire-protocol change outside this crate's scope
/// (this coordinator dispatches against the frozen set; see the crate's
/// final report for the gap). The dataflow itself is still validated, so
/// the difference between "not implemented" and "no such dataflow" stays
/// visible to the caller.
pub async fn topic_publish(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    _port: PortRef,
    _metadata: Metadata,
    _payload: Vec<u8>,
) -> ControlReply {
    match coordinator.store.get_dataflow_meta(dataflow).await {
        Ok(Some(_)) => CoordinatorError::NotYetSupported(
            "TopicPublish",
            "no CoordinatorEvent verb exists yet to inject a message into a running dataflow",
        )
        .into_reply(),
        Ok(None) => CoordinatorError::NoSuchDataflow(dataflow).into_reply(),
        Err(err) => CoordinatorError::from(err).into_reply(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_wire::AuthToken;

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([6; 32])).with_port(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn logs_on_a_dataflow_with_no_hosting_daemons_is_an_empty_success() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator
            .dataflows()
            .insert(sample_live_dataflow(dataflow));
        let reply = logs(&coordinator, dataflow, None, LogQuery::new()).await;
        assert_eq!(
            reply,
            ControlReply::Logs {
                records: Vec::new(),
                truncated: false
            }
        );
    }

    #[tokio::test]
    async fn logs_on_an_unregistered_dataflow_is_not_found() {
        let coordinator = coordinator();
        let reply = logs(&coordinator, DataflowId::generate(), None, LogQuery::new()).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn log_subscribe_registers_a_subscription() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(8);
        let id = SubscriptionId::new(1);
        let reply = log_subscribe(&coordinator, &tx, None, None, LogQuery::new(), id).await;
        assert_eq!(reply, ControlReply::Ok);
        assert!(coordinator.subscriptions().contains(id));
    }

    #[tokio::test]
    async fn topic_subscribe_fails_cleanly_without_a_hosting_daemon() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(8);
        let reply = topic_subscribe(
            &coordinator,
            &tx,
            DataflowId::generate(),
            "camera/frames".parse().unwrap(),
            TopicQuery::new(),
            SubscriptionId::new(1),
        )
        .await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
        assert!(
            coordinator.subscriptions().is_empty(),
            "no partial registration"
        );
    }

    #[tokio::test]
    async fn topic_subscribe_refuses_a_dataflow_without_debug_true() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(8);
        let dataflow = DataflowId::generate();
        coordinator
            .dataflows()
            .insert(sample_live_dataflow(dataflow));

        let reply = topic_subscribe(
            &coordinator,
            &tx,
            dataflow,
            "camera/frames".parse().unwrap(),
            TopicQuery::new(),
            SubscriptionId::new(1),
        )
        .await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::InvalidArgument)
        );
        assert!(
            coordinator.subscriptions().is_empty(),
            "no partial registration"
        );
    }

    #[tokio::test]
    async fn topic_subscribe_succeeds_once_debug_is_enabled_and_a_daemon_hosts_the_node() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(8);
        let dataflow = DataflowId::generate();
        let daemon_id = astrs_wire::DaemonId::generate(None);
        let (daemon_tx, mut daemon_rx) = mpsc::channel(4);
        coordinator
            .daemons()
            .insert(crate::registry::DaemonHandle::new(
                daemon_id.clone(),
                None,
                astrs_wire::SessionId::generate(),
                daemon_tx,
                astrs_time::HlcTimestamp::EPOCH,
            ));
        let mut live = sample_live_dataflow_with_debug(dataflow, true);
        live.placement
            .node_daemon
            .insert(astrs_wire::NodeId::new("camera").unwrap(), daemon_id);
        coordinator.dataflows().insert(live);

        let reply = topic_subscribe(
            &coordinator,
            &tx,
            dataflow,
            "camera/frames".parse().unwrap(),
            TopicQuery::new(),
            SubscriptionId::new(1),
        )
        .await;
        assert_eq!(reply, ControlReply::Ok);
        assert!(coordinator.subscriptions().contains(SubscriptionId::new(1)));
        assert!(matches!(
            daemon_rx.try_recv().unwrap(),
            CoordinatorEvent::TopicTapStart { .. }
        ));
    }

    #[tokio::test]
    async fn topic_unsubscribe_on_an_unknown_id_is_invalid_argument() {
        let coordinator = coordinator();
        let reply = topic_unsubscribe(&coordinator, SubscriptionId::new(99)).await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::InvalidArgument)
        );
    }

    #[tokio::test]
    async fn topic_unsubscribe_closes_a_log_subscription_too() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(8);
        let id = SubscriptionId::new(1);
        log_subscribe(&coordinator, &tx, None, None, LogQuery::new(), id).await;
        let reply = topic_unsubscribe(&coordinator, id).await;
        assert_eq!(reply, ControlReply::Ok);
        assert!(!coordinator.subscriptions().contains(id));
    }

    #[tokio::test]
    async fn topic_publish_on_a_real_dataflow_is_not_yet_supported() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator
            .store
            .upsert_dataflow(dataflow, None, "{}".into(), 0)
            .await
            .unwrap();
        let reply = topic_publish(
            &coordinator,
            dataflow,
            "a/o".parse().unwrap(),
            Metadata::new(astrs_time::HlcTimestamp::EPOCH),
            vec![],
        )
        .await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::Unsupported));
    }

    #[tokio::test]
    async fn topic_publish_on_a_missing_dataflow_is_not_found() {
        let coordinator = coordinator();
        let reply = topic_publish(
            &coordinator,
            DataflowId::generate(),
            "a/o".parse().unwrap(),
            Metadata::new(astrs_time::HlcTimestamp::EPOCH),
            vec![],
        )
        .await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    fn sample_live_dataflow(id: DataflowId) -> crate::registry::LiveDataflow {
        let manifest =
            astrs_manifest::Manifest::from_yaml_str("nodes:\n  - id: a\n    path: ./a\n").unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        crate::registry::LiveDataflow::new(id, None, manifest, None, graph)
    }

    /// A live dataflow with one `camera` node declaring a `frames` output,
    /// and `debug:` set as asked — for the `topic_subscribe` gate tests.
    fn sample_live_dataflow_with_debug(
        id: DataflowId,
        debug: bool,
    ) -> crate::registry::LiveDataflow {
        let yaml = format!(
            "debug: {debug}\nnodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n"
        );
        let manifest = astrs_manifest::Manifest::from_yaml_str(&yaml).unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        crate::registry::LiveDataflow::new(id, None, manifest, None, graph)
    }
}
