//! The per-CLI connection actor (blueprint §7.3).
//!
//! A CLI connection is a strict request/response loop for
//! [`ControlRequest`]/[`ControlReply`], plus whatever
//! [`crate::session::CliOutbound::Log`]/[`crate::session::CliOutbound::Data`]
//! pushes an open subscription queues — see [`crate::session::outbound`]
//! for why both ride the same queue, drained by one writer task, rather
//! than two tasks racing to write the same socket.

use astrs_transport::{FramedReader, FramedWriter};
use astrs_wire::{ControlRequest, RequestScope, SubscriptionId};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::coordinator::Coordinator;
use crate::handlers;
use crate::session::CliOutbound;

/// The outbound queue depth for one CLI connection — see
/// [`crate::session::daemon::OUTBOUND_QUEUE_DEPTH`] for the reasoning
/// (bounded, so a stalled CLI cannot grow this coordinator's memory
/// without limit; a full queue drops a subscription push rather than
/// blocking the daemon-event loop that feeds it — see
/// [`crate::registry::SubscriptionRegistry::deliver_data`]).
pub const OUTBOUND_QUEUE_DEPTH: usize = 256;

/// Runs one CLI connection to completion.
///
/// `reader`/`writer` are already past the handshake — [`astrs_transport::handshake::accept`]
/// widens their limits from the narrow pre-handshake ceiling to the
/// negotiated budget itself, so this takes the very halves the acceptor
/// produced rather than raw streams it would have to re-wrap (and
/// mis-size) from scratch.
///
/// `scope` is what the presented token was classified as (blueprint §16,
/// §22; see [`crate::auth`]) — carried once, for the whole connection,
/// rather than re-derived per request: it is what `crate::server`'s
/// connection acceptor already decided when it chose which credential the
/// handshake itself accepted against, and every request on this connection
/// is bound by that one decision until it closes.
pub async fn run<R, W>(
    coordinator: Coordinator,
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    scope: RequestScope,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<CliOutbound>(OUTBOUND_QUEUE_DEPTH);

    let writer_task = tokio::spawn(async move {
        while let Some(item) = outbound_rx.recv().await {
            let result = match &item {
                CliOutbound::Reply(reply) => writer.send_message(reply).await,
                CliOutbound::Log(frame) => writer.send_message(frame.as_ref()).await,
                CliOutbound::Data(frame) => writer.send_message(frame.as_ref()).await,
            };
            if result.is_err() {
                break;
            }
        }
    });

    loop {
        match reader.recv_frame().await {
            Ok(Some(frame)) => match frame.decode::<ControlRequest>() {
                Ok(request) => {
                    let reply =
                        handlers::dispatch(&coordinator, &outbound_tx, request, scope).await;
                    if outbound_tx.send(CliOutbound::Reply(reply)).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "malformed control request; closing connection");
                    break;
                }
            },
            Ok(None) => break,
            Err(err) => {
                tracing::warn!(%err, "CLI connection error; closing");
                break;
            }
        }
    }

    writer_task.abort();
    close_subscriptions(&coordinator, &outbound_tx).await;
}

/// Closes every subscription this connection opened, telling the owning
/// daemon to stop any topic tap (see
/// [`crate::handlers::logs::topic_unsubscribe`]).
async fn close_subscriptions(coordinator: &Coordinator, sender: &mpsc::Sender<CliOutbound>) {
    let ids: Vec<SubscriptionId> = coordinator.subscriptions().ids_for_sender(sender);
    for id in ids {
        let _ = handlers::logs::topic_unsubscribe(coordinator, id).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_wire::{AuthToken, ControlReply, LogQuery};

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([12; 32])).with_port(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn close_subscriptions_removes_only_this_connections_subscriptions() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(4);
        let (other_tx, _other_rx) = mpsc::channel(4);
        coordinator
            .subscriptions()
            .insert(crate::registry::SubscriberHandle {
                id: SubscriptionId::new(1),
                kind: crate::registry::SubscriptionKind::Log(
                    crate::registry::subscription::LogSubscription {
                        dataflow: None,
                        node: None,
                        query: LogQuery::new(),
                    },
                ),
                sender: tx.clone(),
                dropped: 0,
            });
        coordinator
            .subscriptions()
            .insert(crate::registry::SubscriberHandle {
                id: SubscriptionId::new(2),
                kind: crate::registry::SubscriptionKind::Log(
                    crate::registry::subscription::LogSubscription {
                        dataflow: None,
                        node: None,
                        query: LogQuery::new(),
                    },
                ),
                sender: other_tx,
                dropped: 0,
            });

        close_subscriptions(&coordinator, &tx).await;
        assert!(!coordinator.subscriptions().contains(SubscriptionId::new(1)));
        assert!(coordinator.subscriptions().contains(SubscriptionId::new(2)));
    }

    #[tokio::test]
    async fn run_answers_a_request_over_a_split_duplex_stream() {
        let coordinator = coordinator();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let limits = astrs_wire::FrameLimits::uds();
        let (server_read, server_write) = tokio::io::split(server);
        let counters = astrs_transport::ConnectionCounters::shared();
        let server_reader =
            astrs_transport::FramedReader::new(server_read, limits, counters.clone());
        let server_writer =
            astrs_transport::FramedWriter::new(server_write, limits, counters.clone());
        let server_task = tokio::spawn(run(
            coordinator,
            server_reader,
            server_writer,
            RequestScope::Mutate,
        ));

        let (client_read, client_write) = tokio::io::split(client);
        let mut client_writer =
            astrs_transport::FramedWriter::new(client_write, limits, counters.clone());
        let mut client_reader = astrs_transport::FramedReader::new(client_read, limits, counters);

        client_writer
            .send_message(&ControlRequest::List { all: true })
            .await
            .unwrap();
        let reply: ControlReply = client_reader
            .expect_message(astrs_wire::FrameKind::ControlReply)
            .await
            .unwrap();
        assert!(matches!(reply, ControlReply::DataflowList { .. }));

        drop(client_writer);
        drop(client_reader);
        let _ = server_task.await;
    }
}
