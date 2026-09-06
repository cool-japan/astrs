//! The per-daemon connection actor (blueprint §4.2, §4.3, §12).
//!
//! One [`run`] per accepted daemon connection: a reader loop that decodes
//! [`DaemonEvent`]s and folds each into coordinator state (mostly by
//! delegating to [`crate::handlers::lifecycle`]), and a writer task that
//! drains the daemon's outbound [`CoordinatorEvent`] queue — the same
//! queue [`crate::registry::DaemonHandle::send`] feeds from every other
//! part of the coordinator. The two run concurrently so a slow write never
//! stalls reading this daemon's next event, and vice versa.

use astrs_store::record::MutationSeq;
use astrs_time::HlcTimestamp;
use astrs_transport::{FramedReader, FramedWriter};
use astrs_wire::{
    CoordinatorEvent, DaemonEvent, DaemonId, DaemonInfo, DaemonRegistration, FrameKind,
    MachineName, SessionId,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::catchup;
use crate::coordinator::Coordinator;
use crate::handlers::lifecycle;
use crate::registry::DaemonHandle;

/// The outbound queue depth for one daemon connection.
///
/// Bounded rather than unbounded: a daemon that stops reading (a hung
/// process, a saturated network path) must eventually make its own sends
/// fail rather than let this coordinator's memory grow without limit —
/// [`crate::registry::DaemonHandle::send`] already treats a full queue the
/// same as a closed one (a typed [`crate::CoordinatorError::DaemonNotConnected`],
/// never a panic or a block).
pub const OUTBOUND_QUEUE_DEPTH: usize = 512;

/// Runs one daemon connection to completion: registration, state
/// catch-up, then the event loop, then cleanup on disconnect.
///
/// `reader`/`writer` are already past the handshake — see
/// [`crate::session::cli::run`]'s doc for why this takes the very halves
/// the acceptor produced (with limits already widened to the negotiated
/// budget) rather than raw streams it would have to re-wrap. `session_id`
/// comes from that same handshake's [`astrs_wire::NegotiatedSession`].
pub async fn run<R, W>(
    coordinator: Coordinator,
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    session_id: SessionId,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let registration = match reader
        .expect_message::<DaemonEvent>(FrameKind::DaemonEvent)
        .await
    {
        Ok(DaemonEvent::Register(registration)) => registration,
        Ok(_) => {
            tracing::warn!("daemon connection's first frame was not Register; closing");
            return;
        }
        Err(err) => {
            tracing::warn!(%err, "daemon connection failed before registering");
            return;
        }
    };

    let daemon_id = registration.daemon.clone();
    let now = coordinator.clock.now();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<CoordinatorEvent>(OUTBOUND_QUEUE_DEPTH);

    register(
        &coordinator,
        &registration,
        session_id,
        now,
        outbound_tx.clone(),
    )
    .await;

    let after = MutationSeq::new(registration.catch_up_seq);
    if let Err(err) = resynchronise(&coordinator, &daemon_id, &outbound_tx, after).await {
        if matches!(
            err,
            crate::error::CoordinatorError::Store(
                astrs_store::Error::MutationHistoryCompacted { .. }
            )
        ) {
            // The daemon asked to resume from a sequence this coordinator no
            // longer has a delta for (blueprint §12: the mutation log was
            // compacted past it, typically after a very long disconnect).
            // There is no diff to send, so send the whole current picture
            // instead — still `StateCatchUp`, still acked the same way — so
            // this daemon actually resynchronises rather than staying
            // connected on stale state forever.
            tracing::warn!(
                %daemon_id,
                %err,
                "the daemon's catch-up cursor predates this coordinator's retained history; \
                 falling back to a full state snapshot"
            );
            if let Err(err) =
                catchup::push_full_snapshot(&coordinator, &daemon_id, &outbound_tx).await
            {
                tracing::warn!(%daemon_id, %err, "full state snapshot also failed");
            }
        } else {
            tracing::warn!(%daemon_id, %err, "initial state catch-up failed");
        }
    }

    let writer_task = tokio::spawn(async move {
        while let Some(event) = outbound_rx.recv().await {
            if let Err(err) = writer.send_message(&event).await {
                tracing::warn!(%err, "daemon writer stopped");
                break;
            }
        }
    });

    loop {
        match reader.recv_frame().await {
            Ok(Some(frame)) => match frame.decode::<DaemonEvent>() {
                Ok(event) => {
                    // The heartbeat watchdog (`crate::server`) can declare
                    // this daemon lost — and run its own disconnect
                    // cascade — while this loop is still blocked waiting
                    // for its next frame. Bound to a local first: a
                    // `MutexGuard` held across the `.await` below would
                    // fail the async-fn Send analysis (see
                    // `crate::handlers::lifecycle::stop_by_name`'s comment
                    // for the same pattern). A daemon that was only slow,
                    // not gone, must re-register on a fresh connection
                    // rather than have this now-stale one keep folding
                    // events into coordinator state after its own
                    // disconnect cascade already ran — otherwise a late
                    // `BuildResult`/`AllNodesFinished`/log record could
                    // resurrect state the cascade just tore down.
                    let still_registered = coordinator.daemons().is_connected(&daemon_id);
                    if !still_registered {
                        tracing::info!(
                            %daemon_id,
                            "connection no longer registered (declared lost elsewhere); closing"
                        );
                        break;
                    }
                    handle_event(&coordinator, &daemon_id, event).await;
                }
                Err(err) => {
                    tracing::warn!(%daemon_id, %err, "malformed daemon event; closing connection");
                    break;
                }
            },
            Ok(None) => break,
            Err(err) => {
                tracing::warn!(%daemon_id, %err, "daemon connection error; closing");
                break;
            }
        }
    }

    writer_task.abort();
    coordinator.daemons().remove(&daemon_id);
    let _ = coordinator
        .store
        .set_daemon_reachable(daemon_id.clone(), false)
        .await;
    lifecycle::handle_daemon_disconnected(&coordinator, daemon_id).await;
}

async fn register(
    coordinator: &Coordinator,
    registration: &DaemonRegistration,
    session_id: SessionId,
    now: HlcTimestamp,
    outbound: mpsc::Sender<CoordinatorEvent>,
) {
    let daemon_id = registration.daemon.clone();
    let machine: Option<MachineName> = registration.machine.clone();

    coordinator.daemons().insert(
        DaemonHandle::new(daemon_id.clone(), machine, session_id, outbound, now)
            // The address the *peer* leg dials (§6.4), kept in the live
            // registry so `dispatch_spawn` can build a `PeerRoutes` directive
            // without awaiting a store read inside a registry borrow.
            .with_peer_address(registration.address.clone()),
    );

    let info = DaemonInfo {
        id: daemon_id,
        version: registration.version.clone(),
        address: registration.address.clone(),
        connected_at: now,
        node_count: registration.running_nodes,
        labels: registration.labels.clone(),
        reachable: true,
    };
    let _ = coordinator.store.upsert_daemon(info).await;
}

async fn handle_event(coordinator: &Coordinator, daemon_id: &DaemonId, event: DaemonEvent) {
    coordinator
        .daemons()
        .record_seen(daemon_id, coordinator.clock.now());

    match event {
        DaemonEvent::Register(_) => {
            // A second `Register` on an already-open connection is not
            // part of the normal protocol; ignore rather than re-running
            // registration side effects mid-session.
        }
        DaemonEvent::Heartbeat { .. } => {
            let _ = coordinator.store.record_heartbeat(daemon_id.clone()).await;
        }
        DaemonEvent::BuildResult {
            build,
            dataflow,
            outcome,
        } => {
            lifecycle::handle_build_result(
                coordinator,
                daemon_id.clone(),
                build,
                dataflow,
                outcome,
            )
            .await;
        }
        DaemonEvent::SpawnResult {
            dataflow,
            node,
            generation,
            outcome,
        } => {
            lifecycle::handle_spawn_result(
                coordinator,
                daemon_id.clone(),
                dataflow,
                node,
                generation,
                outcome,
            )
            .await;
        }
        DaemonEvent::AllNodesReady { dataflow, nodes } => {
            lifecycle::handle_all_nodes_ready(coordinator, daemon_id.clone(), dataflow, nodes)
                .await;
        }
        DaemonEvent::AllNodesFinished { dataflow, results } => {
            lifecycle::handle_all_nodes_finished(coordinator, daemon_id.clone(), dataflow, results)
                .await;
        }
        DaemonEvent::NodeStopped {
            dataflow,
            node,
            generation,
            cause,
            restarting,
        } => {
            lifecycle::handle_node_stopped(
                coordinator,
                daemon_id.clone(),
                dataflow,
                node,
                generation,
                cause,
                restarting,
            )
            .await;
        }
        DaemonEvent::NodeMetrics { dataflow, samples } => {
            // Per-node CPU/RSS/queue-depth samples (blueprint §13),
            // recorded as this dataflow's latest-known reading per node
            // — read back by `astrs top` via
            // `ControlRequest::GetNodeMetrics` (see
            // `crate::handlers::info::get_node_metrics`). A dataflow this
            // coordinator has already forgotten (finished and cleaned, or
            // never registered here) simply has nowhere to record the
            // sample; that is not an error worth logging on every
            // two-second heartbeat.
            if let Some(live) = coordinator.dataflows().get_mut(dataflow) {
                for sample in samples {
                    live.latest_metrics.insert(sample.node.clone(), sample);
                }
            }
        }
        DaemonEvent::NodeIoMetrics { dataflow, samples } => {
            // The bandwidth half of the round above, recorded under the same
            // rule and read back by `ControlRequest::GetNodeIoMetrics`.
            if let Some(live) = coordinator.dataflows().get_mut(dataflow) {
                for sample in samples {
                    live.latest_io.insert(sample.node.clone(), sample);
                }
            }
        }
        DaemonEvent::Log {
            request,
            records,
            truncated,
        } => {
            handle_log(coordinator, daemon_id, request, records, truncated).await;
        }
        DaemonEvent::TopicTapData { frame, .. } => {
            coordinator.subscriptions().deliver_data(*frame);
        }
        DaemonEvent::StateCatchUpAck { .. } => {
            // Informational: this coordinator's push loop already drives
            // catch-up to completion by its own bookkeeping (see
            // `crate::catchup::push_catch_up`) rather than waiting on
            // this ack.
        }
        DaemonEvent::Exit { graceful, message } => {
            tracing::info!(%daemon_id, graceful, %message, "daemon reported exit");
        }
        // `DaemonEvent` is `#[non_exhaustive]`.
        _ => {}
    }
}

async fn handle_log(
    coordinator: &Coordinator,
    daemon_id: &DaemonId,
    request: Option<u64>,
    records: Vec<astrs_wire::LogRecord>,
    truncated: bool,
) {
    match request {
        Some(request_id) => {
            let outcome = {
                let mut pending = coordinator.pending_logs();
                let resolved = pending
                    .get_mut(&request_id)
                    .and_then(|fetch| fetch.record(daemon_id, records, truncated));
                resolved.map(|result| {
                    let waiters = pending
                        .remove(&request_id)
                        .map(|fetch| fetch.waiters)
                        .unwrap_or_default();
                    (result, waiters)
                })
            };
            if let Some((result, waiters)) = outcome {
                for waiter in waiters {
                    let _ = waiter.send(result.clone());
                }
            }
        }
        None => {
            let mut subscriptions = coordinator.subscriptions();
            for record in &records {
                subscriptions.deliver_log(record.dataflow, record.node.as_ref(), record);
            }
        }
    }
}

/// Sends a reconnecting daemon whatever it missed.
///
/// Ordinarily a *delta* from the daemon's own cursor
/// ([`catchup::push_catch_up`]), which is what the sequence-numbered mutation
/// log exists for (blueprint §12).
///
/// With the `ha` feature on and this coordinator part of a replicated set,
/// a **full snapshot** instead. Under replication the store's local sequence
/// counter is no longer the cluster's ordering authority — the Raft log index
/// is, and a replicated write is applied to the buckets without allocating a
/// local sequence number (see [`crate::ha::machine`]). A delta computed from
/// that counter would therefore be silently incomplete after a failover,
/// which is worse than the cost of a full resend: the daemon would believe it
/// was caught up. A full snapshot is exactly the fallback the store already
/// defines for history it cannot serve as a delta.
#[cfg(feature = "ha")]
async fn resynchronise(
    coordinator: &Coordinator,
    daemon_id: &DaemonId,
    outbound: &mpsc::Sender<CoordinatorEvent>,
    after: MutationSeq,
) -> crate::error::Result<()> {
    if coordinator.ha().is_some() {
        return catchup::push_full_snapshot(coordinator, daemon_id, outbound).await;
    }
    catchup::push_catch_up(coordinator, daemon_id, outbound, after).await
}

/// Sends a reconnecting daemon the delta from its own cursor (blueprint §12).
#[cfg(not(feature = "ha"))]
async fn resynchronise(
    coordinator: &Coordinator,
    daemon_id: &DaemonId,
    outbound: &mpsc::Sender<CoordinatorEvent>,
    after: MutationSeq,
) -> crate::error::Result<()> {
    catchup::push_catch_up(coordinator, daemon_id, outbound, after).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_wire::AuthToken;

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([11; 32])).with_port(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_spontaneous_log_push_fans_out_to_a_matching_subscription() {
        let coordinator = coordinator();
        let (tx, mut rx) = mpsc::channel(8);
        coordinator
            .subscriptions()
            .insert(crate::registry::SubscriberHandle {
                id: astrs_wire::SubscriptionId::new(1),
                kind: crate::registry::SubscriptionKind::Log(
                    crate::registry::subscription::LogSubscription {
                        dataflow: None,
                        node: None,
                        query: astrs_wire::LogQuery::new(),
                    },
                ),
                sender: tx,
                dropped: 0,
            });

        let record =
            astrs_wire::LogRecord::new(HlcTimestamp::EPOCH, astrs_wire::LogLevel::Info, "hi");
        handle_log(
            &coordinator,
            &DaemonId::generate(None),
            None,
            vec![record],
            false,
        )
        .await;
        assert!(matches!(
            rx.try_recv().unwrap(),
            crate::session::CliOutbound::Log(_)
        ));
    }

    #[tokio::test]
    async fn a_requested_log_fetch_resolves_its_waiter() {
        let coordinator = coordinator();
        let daemon = DaemonId::generate(None);
        let (waiter_tx, waiter_rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = coordinator.pending_logs();
            let mut fetch = crate::registry::PendingLogFetch::new([daemon.clone()]);
            fetch.waiters.push(waiter_tx);
            pending.insert(7, fetch);
        }
        let record =
            astrs_wire::LogRecord::new(HlcTimestamp::EPOCH, astrs_wire::LogLevel::Info, "hi");
        handle_log(&coordinator, &daemon, Some(7), vec![record], false).await;
        let result = waiter_rx.await.unwrap();
        assert_eq!(result.records.len(), 1);
        assert!(!coordinator.pending_logs().contains_key(&7));
    }

    #[tokio::test]
    async fn a_connection_deregistered_out_from_under_it_closes_instead_of_processing_the_next_frame()
     {
        let coordinator = coordinator();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let limits = astrs_wire::FrameLimits::uds();
        let (server_read, server_write) = tokio::io::split(server);
        let counters = astrs_transport::ConnectionCounters::shared();
        let server_reader = FramedReader::new(server_read, limits, counters.clone());
        let server_writer = FramedWriter::new(server_write, limits, counters.clone());
        let session_id = SessionId::generate();
        let server_task = tokio::spawn(run(
            coordinator.clone(),
            server_reader,
            server_writer,
            session_id,
        ));

        let (client_read, client_write) = tokio::io::split(client);
        let mut client_writer = FramedWriter::new(client_write, limits, counters.clone());
        let client_reader = FramedReader::new(client_read, limits, counters);

        let daemon_id = DaemonId::generate(None);
        client_writer
            .send_message(&DaemonEvent::Register(DaemonRegistration::new(
                daemon_id.clone(),
                "127.0.0.1:7408",
                session_id,
            )))
            .await
            .unwrap();

        // Wait for the session task to have actually processed the
        // registration before pulling it back out from under it.
        for _ in 0..200 {
            if coordinator.daemons().is_connected(&daemon_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(coordinator.daemons().is_connected(&daemon_id));

        // The heartbeat watchdog declaring this daemon lost looks exactly
        // like this: gone from the registry while its socket is still, as
        // far as this connection's own reader loop knows, perfectly fine.
        coordinator.daemons().remove(&daemon_id);

        // A daemon that was merely slow, not actually dead, sends another
        // frame right after. It must not be folded into coordinator state.
        client_writer
            .send_message(&DaemonEvent::Log {
                request: None,
                records: Vec::new(),
                truncated: false,
            })
            .await
            .unwrap();

        // The server task returning at all (rather than the 2 s timeout
        // firing) is the actual proof this closes the connection instead
        // of hanging on it forever; a `client_reader.recv_frame()` check
        // for EOF here would be a false negative unrelated to that fix —
        // the initial `StateCatchUp` batch `run` pushes right after
        // `Register` is still sitting, unread, in the duplex's buffer
        // ahead of the close, and `drop(client_writer)` below is what
        // actually lets the duplex's other end observe EOF.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;
        assert!(
            outcome.is_ok(),
            "the session task must close this now-stale connection rather than hang processing its events"
        );
        drop(client_writer);
        drop(client_reader);
    }

    /// End to end, at the store+session level: a daemon that registers
    /// asking to resume from before this coordinator's retained mutation
    /// history (blueprint §12's `MutationHistoryCompacted`) must still come
    /// away connected *and resynchronised* — not merely warned about and
    /// left stale, which was item 4's gap.
    #[tokio::test]
    async fn a_stale_catch_up_cursor_past_compaction_falls_back_to_a_full_snapshot() {
        let coordinator = coordinator();
        let dataflow = astrs_wire::DataflowId::generate();
        coordinator
            .store
            .sync()
            .upsert_dataflow(dataflow, Some("demo".into()), "{}".into(), 0)
            .unwrap();
        coordinator
            .store
            .sync()
            .set_param(
                dataflow,
                astrs_wire::ParamKey::new("gain").unwrap(),
                crate::param_scope::parameter_to_json(&astrs_wire::Parameter::Integer(3)).unwrap(),
            )
            .unwrap();
        // Compact away every mutation logged so far — a `catch_up_seq` of
        // `0` (a cold-start daemon's default) is now older than what this
        // coordinator can diff from.
        let watermark = coordinator.store.sync().last_seq().unwrap();
        coordinator.store.sync().compact(watermark).unwrap();

        let (client, server) = tokio::io::duplex(64 * 1024);
        let limits = astrs_wire::FrameLimits::uds();
        let (server_read, server_write) = tokio::io::split(server);
        let counters = astrs_transport::ConnectionCounters::shared();
        let server_reader = FramedReader::new(server_read, limits, counters.clone());
        let server_writer = FramedWriter::new(server_write, limits, counters.clone());
        let session_id = SessionId::generate();
        let server_task = tokio::spawn(run(
            coordinator.clone(),
            server_reader,
            server_writer,
            session_id,
        ));

        let (client_read, client_write) = tokio::io::split(client);
        let mut client_writer = FramedWriter::new(client_write, limits, counters.clone());
        let mut client_reader = FramedReader::new(client_read, limits, counters);

        let daemon_id = DaemonId::generate(None);
        client_writer
            .send_message(&DaemonEvent::Register(DaemonRegistration::new(
                daemon_id.clone(),
                "127.0.0.1:7408",
                session_id,
            )))
            .await
            .unwrap();

        // The first frame this connection ever receives is the catch-up
        // batch `run` pushes right after registering it.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            client_reader
                .recv_frame()
                .await
                .expect("a readable frame")
                .expect("the connection stayed open")
        })
        .await
        .expect("a frame arrived");
        let event: CoordinatorEvent = frame.decode().expect("a decodable CoordinatorEvent");
        let CoordinatorEvent::StateCatchUp {
            seq,
            entries,
            final_batch,
        } = event
        else {
            panic!("unexpected {event:?}");
        };

        assert!(
            final_batch,
            "a full-snapshot fallback is complete in one batch"
        );
        assert!(
            seq >= watermark.get(),
            "a daemon that acks this seq must not land back before the watermark"
        );
        assert!(
            !entries.is_empty(),
            "the compacted delta was empty; the fallback snapshot must not also be"
        );
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.kind,
                astrs_wire::StateEntryKind::DataflowStatus { dataflow: got, .. }
                    if *got == dataflow
            )),
            "the dataflow that survived compaction must reach the daemon via the snapshot: \
             {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.kind,
                astrs_wire::StateEntryKind::ParamSet { key, .. } if key.as_str() == "gain"
            )),
            "the param that survived compaction must reach the daemon via the snapshot: \
             {entries:?}"
        );

        drop(client_writer);
        drop(client_reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;
    }

    #[tokio::test]
    async fn heartbeat_and_metrics_events_do_not_panic_on_an_unregistered_daemon() {
        let coordinator = coordinator();
        let daemon_id = DaemonId::generate(None);
        handle_event(
            &coordinator,
            &daemon_id,
            DaemonEvent::Heartbeat {
                seq: 1,
                sent_at: HlcTimestamp::EPOCH,
                stats: astrs_wire::DaemonStats {
                    uptime: astrs_wire::DurationMs::new(1),
                    node_count: 0,
                    dataflow_count: 0,
                    cpu_percent: 0.0,
                    rss_bytes: 0,
                    shm_bytes_mapped: 0,
                    shm_fallback_total: 0,
                    frames_sent: 0,
                    frames_received: 0,
                    bytes_sent: 0,
                    bytes_received: 0,
                },
            },
        )
        .await;
        handle_event(
            &coordinator,
            &daemon_id,
            DaemonEvent::Exit {
                graceful: true,
                message: "bye".into(),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn node_metrics_are_recorded_against_the_live_dataflow() {
        let coordinator = coordinator();
        let daemon_id = DaemonId::generate(None);
        let dataflow = astrs_wire::DataflowId::generate();
        let manifest =
            astrs_manifest::Manifest::from_yaml_str("nodes:\n  - id: camera\n    path: ./c\n")
                .unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        coordinator
            .dataflows()
            .insert(crate::registry::LiveDataflow::new(
                dataflow, None, manifest, None, graph,
            ));

        let camera = astrs_wire::NodeId::new("camera").unwrap();
        let mut sample = astrs_wire::NodeMetricsSample::new(camera.clone(), HlcTimestamp::EPOCH);
        sample.cpu_percent = 42.0;
        handle_event(
            &coordinator,
            &daemon_id,
            DaemonEvent::NodeMetrics {
                dataflow,
                samples: vec![sample],
            },
        )
        .await;

        let registry = coordinator.dataflows();
        let live = registry.get(dataflow).unwrap();
        assert_eq!(live.latest_metrics.len(), 1);
        assert_eq!(live.latest_metrics[&camera].cpu_percent, 42.0);
    }

    #[tokio::test]
    async fn node_metrics_for_an_unregistered_dataflow_do_not_panic() {
        let coordinator = coordinator();
        let daemon_id = DaemonId::generate(None);
        handle_event(
            &coordinator,
            &daemon_id,
            DaemonEvent::NodeMetrics {
                dataflow: astrs_wire::DataflowId::generate(),
                samples: vec![astrs_wire::NodeMetricsSample::new(
                    astrs_wire::NodeId::new("camera").unwrap(),
                    HlcTimestamp::EPOCH,
                )],
            },
        )
        .await;
    }
}
