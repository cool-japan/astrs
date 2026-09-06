//! [`Transport`]: how a replica's outbound messages reach their peers.
//!
//! # Fire and forget, on purpose
//!
//! [`Transport::send`] returns immediately and promises nothing about
//! delivery. That is not a simplification — it is Raft's own network model.
//! Every message is either idempotent (a duplicated `AppendEntries` re-appends
//! entries the follower already has, and the merge rule makes that a no-op) or
//! retried by the next heartbeat. A transport that blocked until a peer
//! acknowledged would couple the leader's progress to its slowest follower,
//! which is exactly what Raft is designed to avoid.
//!
//! A transport that *cannot* send — a peer it has never heard of, a queue that
//! is full — says so with [`crate::RaftError::Transport`], and the caller logs
//! it and moves on. There is no correct way to fail a Raft round because one
//! message did not go out.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{ChannelTransport, Envelope, PeerId, RaftMessage, Term, Transport};
//!
//! let (transport, mut inboxes) = ChannelTransport::mesh([PeerId::new(1), PeerId::new(2)]);
//! transport.send(Envelope::new(
//!     PeerId::new(1),
//!     PeerId::new(2),
//!     RaftMessage::TimeoutNow { term: Term::new(1) },
//! ))?;
//!
//! let mut inbox = inboxes.remove(&PeerId::new(2)).expect("peer 2's inbox");
//! assert_eq!(inbox.try_recv()?.from, PeerId::new(1));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;

use tokio::sync::mpsc;

use crate::error::{RaftError, Result};
use crate::message::Envelope;
use crate::types::PeerId;

/// Delivers Raft messages between replicas.
pub trait Transport: Send + Sync + 'static {
    /// Hands one message to the network, best effort.
    ///
    /// # Errors
    ///
    /// [`RaftError::Transport`] when the message could not even be queued —
    /// an unknown peer, a closed connection, a full outbound buffer. The
    /// caller treats this as a dropped packet, which Raft already tolerates.
    fn send(&self, envelope: Envelope) -> Result<()>;
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn send(&self, envelope: Envelope) -> Result<()> {
        (**self).send(envelope)
    }
}

/// An in-process [`Transport`] over `tokio` channels.
///
/// What the three-replica integration tests run on, and what an embedded
/// single-process cluster would use. Unbounded on purpose: a bounded channel
/// would make "the network is congested" into "this replica blocks", and the
/// whole point of [`Transport::send`] is that it does not block.
#[derive(Debug, Clone, Default)]
pub struct ChannelTransport {
    /// One outbound queue per known peer.
    peers: BTreeMap<PeerId, mpsc::UnboundedSender<Envelope>>,
}

impl ChannelTransport {
    /// A transport that knows no peers yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `peer`'s inbox.
    #[must_use]
    pub fn with_peer(mut self, peer: PeerId, inbox: mpsc::UnboundedSender<Envelope>) -> Self {
        self.peers.insert(peer, inbox);
        self
    }

    /// Adds `peer`'s inbox to an existing transport.
    pub fn insert(&mut self, peer: PeerId, inbox: mpsc::UnboundedSender<Envelope>) {
        self.peers.insert(peer, inbox);
    }

    /// Builds one transport reaching every peer, plus each peer's receiver.
    ///
    /// Every replica in an in-process cluster can share a clone of the same
    /// transport, because an [`Envelope`] already names its recipient.
    #[must_use]
    pub fn mesh(
        peers: impl IntoIterator<Item = PeerId>,
    ) -> (Self, BTreeMap<PeerId, mpsc::UnboundedReceiver<Envelope>>) {
        let mut transport = Self::new();
        let mut inboxes = BTreeMap::new();
        for peer in peers {
            let (tx, rx) = mpsc::unbounded_channel();
            transport.insert(peer, tx);
            inboxes.insert(peer, rx);
        }
        (transport, inboxes)
    }

    /// Whether this transport knows how to reach `peer`.
    #[must_use]
    pub fn knows(&self, peer: PeerId) -> bool {
        self.peers.contains_key(&peer)
    }
}

impl Transport for ChannelTransport {
    fn send(&self, envelope: Envelope) -> Result<()> {
        let peer = envelope.to;
        let Some(inbox) = self.peers.get(&peer) else {
            return Err(RaftError::Transport {
                peer,
                reason: "no route to that peer".to_owned(),
            });
        };
        inbox.send(envelope).map_err(|_| RaftError::Transport {
            peer,
            reason: "the peer's inbox is closed".to_owned(),
        })
    }
}

/// A [`Transport`] that discards everything.
///
/// Used to isolate a replica in a test without unwinding its configuration —
/// the simulator's partitions are built on the same idea.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullTransport;

impl Transport for NullTransport {
    fn send(&self, _envelope: Envelope) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::message::RaftMessage;
    use crate::types::Term;

    fn message(from: u64, to: u64) -> Envelope {
        Envelope::new(
            PeerId::new(from),
            PeerId::new(to),
            RaftMessage::TimeoutNow { term: Term::new(1) },
        )
    }

    #[test]
    fn a_mesh_routes_by_the_envelopes_recipient() {
        let (transport, mut inboxes) =
            ChannelTransport::mesh([PeerId::new(1), PeerId::new(2), PeerId::new(3)]);
        transport.send(message(1, 3)).unwrap();
        transport.send(message(2, 3)).unwrap();

        let third = inboxes.get_mut(&PeerId::new(3)).unwrap();
        assert_eq!(third.try_recv().unwrap().from, PeerId::new(1));
        assert_eq!(third.try_recv().unwrap().from, PeerId::new(2));
        assert!(
            inboxes
                .get_mut(&PeerId::new(1))
                .unwrap()
                .try_recv()
                .is_err()
        );
    }

    #[test]
    fn an_unknown_peer_is_a_named_transport_error() {
        let (transport, _inboxes) = ChannelTransport::mesh([PeerId::new(1)]);
        let error = transport.send(message(1, 9)).unwrap_err();
        assert!(matches!(error, RaftError::Transport { .. }));
        assert!(error.to_string().contains("peer 9"));
        assert!(!transport.knows(PeerId::new(9)));
    }

    #[test]
    fn a_closed_inbox_is_reported_rather_than_panicking() {
        let (transport, inboxes) = ChannelTransport::mesh([PeerId::new(1), PeerId::new(2)]);
        drop(inboxes);
        let error = transport.send(message(1, 2)).unwrap_err();
        assert!(matches!(error, RaftError::Transport { .. }));
    }

    #[test]
    fn the_builder_and_the_setter_agree() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let transport = ChannelTransport::new().with_peer(PeerId::new(4), tx);
        assert!(transport.knows(PeerId::new(4)));
        transport.send(message(1, 4)).unwrap();
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn the_null_transport_swallows_everything_without_failing() {
        assert!(NullTransport.send(message(1, 2)).is_ok());
    }

    #[test]
    fn an_arc_wrapped_transport_is_itself_a_transport() {
        let (transport, mut inboxes) = ChannelTransport::mesh([PeerId::new(1), PeerId::new(2)]);
        let shared: std::sync::Arc<ChannelTransport> = std::sync::Arc::new(transport);
        shared.send(message(1, 2)).unwrap();
        assert!(inboxes.get_mut(&PeerId::new(2)).unwrap().try_recv().is_ok());
    }
}
