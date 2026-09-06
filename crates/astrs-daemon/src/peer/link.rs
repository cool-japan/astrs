//! [`PeerLink`] — one established daemon↔daemon connection (§6.4).
//!
//! > *One connection per peer pair, control messages on stream 0, each
//! > high-bandwidth route on its own unidirectional stream (no head-of-line
//! > coupling between a camera topic and heartbeats).*
//!
//! A link is that connection plus the senders the daemon writes through. It
//! does **not** own the receiving side: a socket that must be read continuously
//! cannot live inside a `&mut self` the event loop only touches between
//! events, so every inbound frame is pumped by a task that reports it as
//! [`crate::session::DaemonEvent::PeerFrame`] — the same discipline every
//! other blocking thing in this crate follows.
//!
//! ```text
//!   ┌──────────────── PeerLink (owned by the event loop) ───────────────┐
//!   │  ControlSender          stream 0: RouteSetup/Accept/Teardown/Ping │
//!   │  RouteSender × N        one per route: Output                     │
//!   └───────────────────────────────┬───────────────────────────────────┘
//!                                   │ Arc<StreamConnection>
//!   ┌───────────────────────────────┴───────────────────────────────────┐
//!   │  pump task: control · accepts · datagrams ──► DaemonEvent::PeerFrame│
//!   └───────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Which stream a frame takes
//!
//! `Output` rides the route's own stream and everything else rides stream 0.
//! That is what keeps a 4 K camera frame from delaying a `Ping`, and it is why
//! a route's stream is opened at *setup* time rather than lazily on the first
//! payload — a stream that has to be created under load is a stall under load.
//!
//! **`OutputClosed` is the one exception**, and it rides the route's stream
//! too. Stream 0's independence is exactly the problem for it: a closure
//! written there can overtake the payloads it terminates, and did — a consumer
//! on another machine saw its input close with the tail of the stream still in
//! flight and reported one message fewer than was published. On the route's
//! own stream it is ordered behind them by construction. A `RouteTeardown`
//! deliberately keeps stream 0: it abandons a route rather than finishing one,
//! and must arrive even when the route stream is already gone.
//!
//! # Backpressure
//!
//! Sends are non-blocking ([`astrs_transport::RouteSender::try_send`]). The
//! event loop must never await a socket: a peer that stops reading would
//! otherwise stop this daemon's node supervision too. A full queue is reported
//! as [`astrs_transport::TransportError::SendQueueFull`], which the caller
//! accounts as a drop — the same outcome a full input queue produces (§11.2),
//! for the same reason.

use std::collections::BTreeMap;
use std::sync::Arc;

use astrs_transport::{
    Connection, ControlSender, MuxChannels, RouteSender, RouteStream, StreamConnection,
    TransportAddr,
};
use astrs_wire::{DaemonId, FrameKind, PeerEvent, RouteId, WireEncode};

use crate::error::{DaemonError, DaemonResult};
use crate::session::{DaemonEvent, DaemonHandle};

/// One connection to one peer daemon.
#[derive(Debug)]
pub struct PeerLink {
    /// The peer at the other end.
    daemon: DaemonId,
    /// Where it is.
    addr: TransportAddr,
    /// The connection, shared with the pump task.
    connection: Arc<StreamConnection>,
    /// Stream 0.
    control: ControlSender,
    /// One sender per route this daemon opened.
    routes: BTreeMap<RouteId, RouteSender>,
    /// How many frames have been written.
    frames_sent: u64,
    /// How many payload bytes have been written.
    bytes_sent: u64,
    /// How many sends were refused by a full queue.
    dropped: u64,
}

impl PeerLink {
    /// Takes ownership of an established connection and starts pumping it.
    ///
    /// The returned link is inert until the event loop installs it: it holds
    /// senders, and the pump task holds the receivers, so nothing is lost in
    /// the window between establishment and installation — inbound frames
    /// queue in the internal event channel exactly like every other event.
    #[must_use]
    pub fn establish(
        daemon: DaemonId,
        connection: StreamConnection,
        channels: MuxChannels,
        handle: DaemonHandle,
    ) -> Self {
        let connection = Arc::new(connection);
        let control = connection.open_control();
        let addr = connection.peer().addr.clone();
        tokio::spawn(pump(daemon.clone(), channels, handle));
        Self {
            daemon,
            addr,
            connection,
            control,
            routes: BTreeMap::new(),
            frames_sent: 0,
            bytes_sent: 0,
            dropped: 0,
        }
    }

    /// The peer at the other end.
    #[must_use]
    pub const fn daemon(&self) -> &DaemonId {
        &self.daemon
    }

    /// Where the peer is.
    #[must_use]
    pub const fn addr(&self) -> &TransportAddr {
        &self.addr
    }

    /// The underlying connection, for statistics and closure.
    #[must_use]
    pub fn connection(&self) -> &Arc<StreamConnection> {
        &self.connection
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.connection.is_closed()
    }

    /// The connection's incarnation counter, which a reconnect increments.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.connection.epoch()
    }

    /// How many frames this daemon has written to the peer.
    #[must_use]
    pub const fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// How many payload bytes this daemon has written to the peer.
    #[must_use]
    pub const fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    /// How many sends a full queue refused.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// How many routes have a stream open on this link.
    #[must_use]
    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    /// Whether one route has a stream open.
    #[must_use]
    pub fn has_route(&self, route: RouteId) -> bool {
        self.routes.contains_key(&route)
    }

    /// Opens a logical stream for one route (§6.4).
    ///
    /// # The stream id is the transport's to mint, not this daemon's
    ///
    /// `route` is a **daemon-level** handle: it is minted per peer by
    /// [`crate::peer::PeerRouteTable::allocate`], travels *inside* every
    /// [`astrs_wire::PeerEvent`], and is what the far end looks the route up
    /// by. A mux stream id is a different thing entirely — it is owned by one
    /// side of the connection, and `astrs-transport` splits that ownership by
    /// parity (the initiator's ids are odd, the acceptor's even) so the two
    /// ends can open streams concurrently without agreeing first.
    ///
    /// Passing the daemon's handle straight through as the stream id broke
    /// that rule, because both daemons mint their first route as `1`: with two
    /// routes between the same pair in opposite directions — a sensor on A
    /// feeding a planner on B, whose commands come back to a node on A — the
    /// acceptor's stream `1` collided with the initiator's, and the payloads
    /// of the *return* route were delivered into the local end's own
    /// drain-and-discard receiver instead of being reported. One direction
    /// worked perfectly and the other silently carried nothing.
    ///
    /// So the transport mints the stream, and the mapping from this daemon's
    /// handle to that stream stays local. Nothing on the wire depends on the
    /// two being equal.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Transport`] if the connection has ended or the
    /// negotiated route ceiling is reached.
    pub fn open_route(&mut self, route: RouteId, descriptor: &[u8]) -> DaemonResult<()> {
        if self.routes.contains_key(&route) {
            return Ok(());
        }
        let stream: RouteStream = self.connection.open_route(descriptor)?;
        let (sender, mut receiver) = stream.into_halves();
        // A producer-side route receives nothing, but an undrained receiver
        // holds mux credit the peer is waiting on, so it is drained and
        // discarded rather than dropped on the floor.
        tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        self.routes.insert(route, sender);
        Ok(())
    }

    /// Closes one route's stream.
    pub fn close_route(&mut self, route: RouteId) -> bool {
        match self.routes.remove(&route) {
            Some(sender) => {
                sender.close();
                true
            }
            None => false,
        }
    }

    /// Writes one event to the peer, choosing its stream (§6.4).
    ///
    /// # Errors
    ///
    /// - [`DaemonError::Wire`] if the event does not encode.
    /// - [`DaemonError::Transport`] if the queue is full or the connection has
    ///   ended.
    /// - [`DaemonError::UnknownRoute`] for a payload on a route with no
    ///   stream, which is a caller bug rather than a network condition.
    pub fn send(&mut self, event: &PeerEvent) -> DaemonResult<()> {
        let payload = event.encode_to_vec().map_err(DaemonError::Wire)?;
        let result = if event.is_payload() {
            let route = event.route_id().unwrap_or(RouteId::NONE);
            let Some(sender) = self.routes.get(&route) else {
                return Err(DaemonError::UnknownRoute {
                    daemon: self.daemon.clone(),
                    route,
                });
            };
            sender.try_send(FrameKind::PeerEvent, &payload)
        } else if let Some(sender) = closure_stream(event).and_then(|route| self.routes.get(&route))
        {
            // A closure must not overtake the payloads it terminates. Stream 0
            // is *deliberately* independent of every route stream — that is
            // what keeps a `Ping` from queueing behind a 4 K frame — so an
            // `OutputClosed` written there could, and did, arrive before the
            // producer's last message: a consumer on another machine saw its
            // input close with the tail of the stream still in flight, and
            // reported one message fewer than was published. Written on the
            // route's own stream it is ordered behind them by construction,
            // which is the only ordering guarantee a mux offers and the only
            // one this needs.
            sender.try_send(FrameKind::PeerEvent, &payload)
        } else {
            self.control.try_send(FrameKind::PeerEvent, &payload)
        };

        match result {
            Ok(()) => {
                self.frames_sent = self.frames_sent.saturating_add(1);
                self.bytes_sent = self.bytes_sent.saturating_add(event.payload_len() as u64);
                Ok(())
            }
            Err(error) => {
                self.dropped = self.dropped.saturating_add(1);
                Err(DaemonError::Transport(error))
            }
        }
    }

    /// Closes the connection, telling the peer why.
    pub async fn close(&mut self, reason: astrs_transport::CloseReason) {
        for (_, sender) in std::mem::take(&mut self.routes) {
            sender.close();
        }
        let _ = self.connection.close(reason).await;
    }
}

/// Reads everything the peer sends and reports it to the event loop.
///
/// Ends when the connection does, with one
/// [`crate::session::DaemonEvent::PeerLost`] — exactly one, whichever channel
/// notices first, because the loop must reclaim the peer's routes once and not
/// three times.
async fn pump(daemon: DaemonId, mut channels: MuxChannels, handle: DaemonHandle) {
    loop {
        tokio::select! {
            frame = channels.control.recv() => match frame {
                Some(frame) => {
                    if !report(&daemon, &handle, frame.payload()) {
                        return;
                    }
                }
                None => break,
            },
            frame = channels.datagrams.recv() => match frame {
                Some(frame) => {
                    if !report(&daemon, &handle, frame.payload()) {
                        return;
                    }
                }
                // A connection without native datagrams closes this channel
                // immediately; that is not the end of the connection, so the
                // arm is disabled rather than treated as a disconnect.
                None => std::future::pending().await,
            },
            route = channels.accepts.accept() => match route {
                Some(stream) => {
                    tokio::spawn(pump_route(daemon.clone(), stream, handle.clone()));
                }
                None => break,
            },
        }
    }
    handle.send(DaemonEvent::PeerLost {
        daemon,
        reason: "the peer connection closed".into(),
    });
}

/// The route whose stream an event must be ordered on, if any.
///
/// Only [`PeerEvent::OutputClosed`]: it is the one non-payload event whose
/// meaning depends on arriving *after* the payloads it follows. A
/// `RouteTeardown` deliberately does not qualify — it abandons a route rather
/// than finishing it, and must reach the peer even when the route stream is
/// already gone.
const fn closure_stream(event: &PeerEvent) -> Option<RouteId> {
    match event {
        PeerEvent::OutputClosed { route_id, .. } => Some(*route_id),
        _ => None,
    }
}

/// Reads one route's stream until the peer closes it.
async fn pump_route(daemon: DaemonId, stream: RouteStream, handle: DaemonHandle) {
    let (_sender, mut receiver) = stream.into_halves();
    while let Some(frame) = receiver.recv().await {
        if !report(&daemon, &handle, frame.payload()) {
            return;
        }
    }
}

/// Decodes one payload and hands it to the loop.
///
/// Returns whether the loop is still listening. A payload that will not decode
/// is dropped with a trace rather than killing the connection: one malformed
/// frame from a peer running a different build must not take down a route that
/// is otherwise working, and [`astrs_wire::PeerEvent`] is `#[non_exhaustive]`
/// precisely so a newer peer's tail variant is a decode error rather than a
/// misinterpretation.
fn report(daemon: &DaemonId, handle: &DaemonHandle, payload: &[u8]) -> bool {
    use astrs_wire::WireMessage;
    match PeerEvent::from_payload(payload) {
        Ok(event) => handle.send(DaemonEvent::PeerFrame {
            daemon: daemon.clone(),
            event: Box::new(event),
        }),
        Err(error) => {
            tracing::warn!(%daemon, %error, "undecodable peer frame dropped");
            handle.is_open()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::{RouteCloseReason, WireMessage};

    use super::*;

    #[test]
    fn a_ping_is_control_and_an_output_is_payload() {
        let ping = PeerEvent::ping(1, HlcTimestamp::EPOCH);
        assert!(!ping.is_payload());
        assert!(ping.route_id().is_none());

        let output = PeerEvent::Output {
            route_id: RouteId::FIRST,
            seq: 0,
            metadata: Default::default(),
            payload: vec![0; 16],
        };
        assert!(output.is_payload());
        assert_eq!(output.route_id(), Some(RouteId::FIRST));
    }

    #[test]
    fn a_peer_event_round_trips_through_the_payload_encoding_the_link_uses() {
        let event = PeerEvent::OutputClosed {
            route_id: RouteId::new(4),
            final_seq: 9,
            reason: RouteCloseReason::ProducerFinished,
        };
        let payload = event.encode_to_vec().expect("encodes");
        assert_eq!(PeerEvent::from_payload(&payload).expect("decodes"), event);
    }

    #[tokio::test]
    async fn an_undecodable_payload_is_dropped_rather_than_forwarded() {
        let (handle, mut events) = crate::session::event_channel();
        let daemon = DaemonId::generate(None);
        assert!(report(&daemon, &handle, b"not a peer event"));
        assert!(events.try_recv().is_none());
    }

    #[tokio::test]
    async fn a_decodable_payload_reaches_the_loop() {
        let (handle, mut events) = crate::session::event_channel();
        let daemon = DaemonId::generate(None);
        let event = PeerEvent::ping(7, HlcTimestamp::EPOCH);
        assert!(report(
            &daemon,
            &handle,
            &event.encode_to_vec().expect("encodes")
        ));

        match events.recv().await.expect("sent") {
            DaemonEvent::PeerFrame {
                daemon: from,
                event,
            } => {
                assert_eq!(from, daemon);
                assert!(matches!(*event, PeerEvent::Ping { nonce: 7, .. }));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn reporting_stops_once_the_loop_is_gone() {
        let (handle, events) = crate::session::event_channel();
        drop(events);
        let daemon = DaemonId::generate(None);
        let event = PeerEvent::ping(1, HlcTimestamp::EPOCH);
        assert!(!report(
            &daemon,
            &handle,
            &event.encode_to_vec().expect("encodes")
        ));
    }
}
