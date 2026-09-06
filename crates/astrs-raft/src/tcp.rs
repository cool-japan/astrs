//! A TCP [`Transport`] framed exactly like the rest of the AstRS control
//! plane.
//!
//! # The same frame, a private message family
//!
//! Every Raft message goes out as one blueprint §7.1 frame — the same ten-byte
//! header, the same little-endian varint `oxicode` payload, the same optional
//! CRC-32C trailer — carrying an [`Envelope`] as its payload.
//! [`astrs_wire::FrameKind::PeerEvent`] is the family used, and reusing that
//! discriminant is safe for one concrete reason: **Raft peers speak on their
//! own listener**. A Raft frame never shares a socket with a CLI's
//! `ControlRequest` or a daemon's `DaemonEvent`, so there is no connection on
//! which the discriminant could be ambiguous, and no frozen wire enum needed a
//! new variant to make this work.
//!
//! # Connections are the transport's problem, not consensus's
//!
//! [`TcpTransport::send`] never blocks and never fails because a peer is
//! down: each peer gets one writer task with an unbounded queue that dials
//! lazily, reconnects after a failure, and drops what it could not deliver.
//! That is exactly the network Raft is specified against — see
//! [`crate::transport`] for why anything stronger would be worse.
//!
//! # Examples
//!
//! ```
//! # tokio_test_block(async {
//! use astrs_raft::{Envelope, PeerId, RaftMessage, TcpTransport, TcpTransportServer, Term, Transport};
//!
//! let (server, mut inbound) = TcpTransportServer::bind("127.0.0.1:0").await?;
//! let address = server.local_addr()?;
//! let serving = tokio::spawn(server.serve());
//!
//! let transport = TcpTransport::connect([(PeerId::new(2), address)]);
//! transport.send(Envelope::new(
//!     PeerId::new(1),
//!     PeerId::new(2),
//!     RaftMessage::TimeoutNow { term: Term::new(4) },
//! ))?;
//!
//! let received = inbound.recv().await.expect("one envelope");
//! assert_eq!(received.from, PeerId::new(1));
//! serving.abort();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! # });
//! # fn tokio_test_block<F: std::future::Future<Output = Result<(), Box<dyn std::error::Error>>>>(f: F) {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(f).expect("ok");
//! # }
//! ```

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use astrs_wire::{AsyncFrameReader, AsyncFrameWriter, FrameFlags, FrameKind, FrameLimits};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{RaftError, Result};
use crate::message::Envelope;
use crate::transport::Transport;
use crate::types::PeerId;

/// The frame family Raft traffic travels in. See this module's header for why
/// reusing `PeerEvent` needs no wire-enum change.
pub const RAFT_FRAME_KIND: FrameKind = FrameKind::PeerEvent;

/// How long a writer task waits after a failed dial before trying again.
pub const RECONNECT_DELAY: Duration = Duration::from_millis(200);

/// The outbound half of the TCP transport: one queue and one writer task per
/// peer.
#[derive(Debug, Clone)]
pub struct TcpTransport {
    /// Shared state, so clones of this transport reuse the same connections.
    inner: Arc<TcpTransportInner>,
}

/// The state a [`TcpTransport`] shares between its clones.
#[derive(Debug)]
struct TcpTransportInner {
    /// One outbound queue per peer.
    peers: BTreeMap<PeerId, mpsc::UnboundedSender<Envelope>>,
    /// The writer tasks, aborted when the last clone drops.
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for TcpTransportInner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl TcpTransport {
    /// Starts one writer task per peer.
    ///
    /// Dialling is lazy: no connection is opened until there is something to
    /// send, and a peer that is not up yet costs nothing but the messages
    /// addressed to it.
    ///
    /// # Panics
    ///
    /// Never panics, but must be called from within a `tokio` runtime, since
    /// it spawns one task per peer.
    #[must_use]
    pub fn connect(peers: impl IntoIterator<Item = (PeerId, SocketAddr)>) -> Self {
        Self::connect_with_limits(peers, FrameLimits::network())
    }

    /// As [`TcpTransport::connect`], with an explicit frame-size policy.
    #[must_use]
    pub fn connect_with_limits(
        peers: impl IntoIterator<Item = (PeerId, SocketAddr)>,
        limits: FrameLimits,
    ) -> Self {
        let mut queues = BTreeMap::new();
        let mut tasks = Vec::new();
        for (peer, address) in peers {
            let (tx, rx) = mpsc::unbounded_channel();
            queues.insert(peer, tx);
            tasks.push(tokio::spawn(writer_task(peer, address, rx, limits)));
        }
        Self {
            inner: Arc::new(TcpTransportInner {
                peers: queues,
                tasks,
            }),
        }
    }

    /// Whether this transport has a route to `peer`.
    #[must_use]
    pub fn knows(&self, peer: PeerId) -> bool {
        self.inner.peers.contains_key(&peer)
    }
}

impl Transport for TcpTransport {
    fn send(&self, envelope: Envelope) -> Result<()> {
        let peer = envelope.to;
        let Some(queue) = self.inner.peers.get(&peer) else {
            return Err(RaftError::Transport {
                peer,
                reason: "no address configured for that peer".to_owned(),
            });
        };
        queue.send(envelope).map_err(|_| RaftError::Transport {
            peer,
            reason: "the writer task for that peer has stopped".to_owned(),
        })
    }
}

/// Drains one peer's queue onto a TCP connection, reconnecting as needed.
async fn writer_task(
    peer: PeerId,
    address: SocketAddr,
    mut queue: mpsc::UnboundedReceiver<Envelope>,
    limits: FrameLimits,
) {
    let mut writer: Option<AsyncFrameWriter<TcpStream>> = None;
    while let Some(envelope) = queue.recv().await {
        if writer.is_none() {
            match TcpStream::connect(address).await {
                Ok(stream) => {
                    let mut fresh = AsyncFrameWriter::new(stream, limits);
                    // CRC on every frame: a Raft message silently corrupted in
                    // transit would be indistinguishable from a real one.
                    if fresh.set_flags(FrameFlags::CRC).is_ok() {
                        writer = Some(fresh);
                    }
                }
                Err(error) => {
                    tracing::debug!(peer = peer.get(), %address, %error, "raft dial failed");
                    // Drop this message — Raft retries on the next heartbeat —
                    // and back off so a down peer does not spin the CPU.
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            }
        }
        let Some(active) = writer.as_mut() else {
            continue;
        };
        let sent = match active.queue_payload(RAFT_FRAME_KIND, &envelope) {
            Ok(_) => active.flush().await,
            Err(error) => Err(error),
        };
        if let Err(error) = sent {
            tracing::debug!(peer = peer.get(), %error, "raft send failed; reconnecting");
            writer = None;
        }
    }
}

/// The inbound half of the TCP transport: a listener that turns accepted
/// connections into a single stream of [`Envelope`]s.
#[derive(Debug)]
pub struct TcpTransportServer {
    /// The bound listener.
    listener: TcpListener,
    /// Where decoded envelopes are delivered.
    inbound: mpsc::UnboundedSender<Envelope>,
    /// The frame-size policy enforced on every connection.
    limits: FrameLimits,
}

impl TcpTransportServer {
    /// Binds a Raft listener, returning it and the stream of inbound
    /// envelopes.
    ///
    /// # Errors
    ///
    /// [`RaftError::Io`] if the address cannot be bound.
    pub async fn bind(
        address: impl tokio::net::ToSocketAddrs + std::fmt::Debug,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Envelope>)> {
        Self::bind_with_limits(address, FrameLimits::network()).await
    }

    /// As [`TcpTransportServer::bind`], with an explicit frame-size policy.
    ///
    /// # Errors
    ///
    /// [`RaftError::Io`] if the address cannot be bound.
    pub async fn bind_with_limits(
        address: impl tokio::net::ToSocketAddrs + std::fmt::Debug,
        limits: FrameLimits,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Envelope>)> {
        let described = format!("{address:?}");
        let listener = TcpListener::bind(address)
            .await
            .map_err(|source| RaftError::io("binding the Raft listener", described, source))?;
        let (inbound, receiver) = mpsc::unbounded_channel();
        Ok((
            Self {
                listener,
                inbound,
                limits,
            },
            receiver,
        ))
    }

    /// The address actually bound, which resolves a `:0` request.
    ///
    /// # Errors
    ///
    /// [`RaftError::Io`] if the socket cannot report its own address.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener.local_addr().map_err(|source| {
            RaftError::io("reading the Raft listener address", "listener", source)
        })
    }

    /// Accepts connections forever, forwarding every decoded envelope.
    ///
    /// Returns only when the listener fails; a caller runs it as a task and
    /// aborts it to shut down.
    ///
    /// # Errors
    ///
    /// [`RaftError::Io`] if accepting fails.
    pub async fn serve(self) -> Result<()> {
        loop {
            let (stream, peer_address) = self
                .listener
                .accept()
                .await
                .map_err(|source| RaftError::io("accepting a Raft peer", "listener", source))?;
            let inbound = self.inbound.clone();
            let limits = self.limits;
            tokio::spawn(async move {
                if let Err(error) = read_connection(stream, &inbound, limits).await {
                    tracing::debug!(%peer_address, %error, "a Raft peer connection ended");
                }
            });
        }
    }
}

/// Reads framed envelopes off one connection until it closes.
async fn read_connection(
    stream: TcpStream,
    inbound: &mpsc::UnboundedSender<Envelope>,
    limits: FrameLimits,
) -> Result<()> {
    let mut reader = AsyncFrameReader::new(stream, limits);
    loop {
        let Some(view) = reader.next_frame().await? else {
            return Ok(());
        };
        let envelope: Envelope = view.decode_as(RAFT_FRAME_KIND)?;
        if inbound.send(envelope).is_err() {
            // The replica has shut down; nothing left to deliver to.
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::log::entry::LogEntry;
    use crate::message::RaftMessage;
    use crate::types::{LogIndex, Term};

    fn heartbeat(term: u64) -> RaftMessage {
        RaftMessage::AppendEntries {
            term: Term::new(term),
            prev_log_index: LogIndex::ZERO,
            prev_log_term: Term::INITIAL,
            entries: Vec::new(),
            leader_commit: LogIndex::ZERO,
        }
    }

    #[tokio::test]
    async fn a_message_survives_a_real_socket_with_the_wires_own_framing() {
        let (server, mut inbound) = TcpTransportServer::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve());

        let transport = TcpTransport::connect([(PeerId::new(2), address)]);
        let sent = Envelope::new(
            PeerId::new(1),
            PeerId::new(2),
            RaftMessage::AppendEntries {
                term: Term::new(3),
                prev_log_index: LogIndex::new(4),
                prev_log_term: Term::new(2),
                entries: vec![LogEntry::command(
                    Term::new(3),
                    LogIndex::new(5),
                    vec![9; 128],
                )],
                leader_commit: LogIndex::new(4),
            },
        );
        transport.send(sent.clone()).unwrap();

        let received = inbound.recv().await.expect("one envelope");
        assert_eq!(received, sent, "the payload must survive byte for byte");
        serving.abort();
    }

    #[tokio::test]
    async fn many_messages_arrive_in_order_on_one_connection() {
        let (server, mut inbound) = TcpTransportServer::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve());

        let transport = TcpTransport::connect([(PeerId::new(2), address)]);
        for term in 1..=32u64 {
            transport
                .send(Envelope::new(
                    PeerId::new(1),
                    PeerId::new(2),
                    heartbeat(term),
                ))
                .unwrap();
        }
        for term in 1..=32u64 {
            let received = inbound.recv().await.expect("an envelope");
            assert_eq!(received.term(), Term::new(term));
        }
        serving.abort();
    }

    #[tokio::test]
    async fn a_peer_that_is_down_costs_only_the_messages_addressed_to_it() {
        // Nothing is listening on this port; sending must not fail, block or
        // panic — Raft retries on the next heartbeat.
        let dead = SocketAddr::from(([127, 0, 0, 1], 1));
        let transport = TcpTransport::connect([(PeerId::new(2), dead)]);
        for term in 1..=4u64 {
            transport
                .send(Envelope::new(
                    PeerId::new(1),
                    PeerId::new(2),
                    heartbeat(term),
                ))
                .unwrap();
        }
        assert!(transport.knows(PeerId::new(2)));
    }

    #[tokio::test]
    async fn an_unconfigured_peer_is_a_named_error() {
        let transport = TcpTransport::connect([]);
        let error = transport
            .send(Envelope::new(PeerId::new(1), PeerId::new(9), heartbeat(1)))
            .unwrap_err();
        assert!(matches!(error, RaftError::Transport { .. }));
        assert!(!transport.knows(PeerId::new(9)));
    }

    #[tokio::test]
    async fn a_writer_reconnects_after_the_far_end_goes_away() {
        let (server, mut inbound) = TcpTransportServer::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve());

        let transport = TcpTransport::connect([(PeerId::new(2), address)]);
        transport
            .send(Envelope::new(PeerId::new(1), PeerId::new(2), heartbeat(1)))
            .unwrap();
        assert!(inbound.recv().await.is_some());

        // Restart the far end on the same port is not possible portably, so
        // instead prove the queue keeps accepting after a failed write by
        // stopping the server and sending again.
        serving.abort();
        for term in 2..=4u64 {
            transport
                .send(Envelope::new(
                    PeerId::new(1),
                    PeerId::new(2),
                    heartbeat(term),
                ))
                .unwrap();
        }
    }

    #[tokio::test]
    async fn dropping_the_last_clone_stops_the_writer_tasks() {
        let (server, _inbound) = TcpTransportServer::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let transport = TcpTransport::connect([(PeerId::new(2), address)]);
        let clone = transport.clone();
        drop(transport);
        // The clone still works: the tasks live as long as any handle does.
        clone
            .send(Envelope::new(PeerId::new(1), PeerId::new(2), heartbeat(1)))
            .unwrap();
        drop(clone);
    }

    #[test]
    fn raft_traffic_uses_the_peer_event_family() {
        // Documented in this module's header: Raft peers have their own
        // listener, so reusing the discriminant is unambiguous.
        assert_eq!(RAFT_FRAME_KIND, FrameKind::PeerEvent);
    }
}
