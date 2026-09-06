//! `ASTRS-MUX/1`: per-route logical streams over one peer connection.
//!
//! Blueprint §6.4 asks for one connection per peer pair, control messages on
//! stream 0, and *each high-bandwidth route on its own stream* — so that a
//! camera topic never head-of-line blocks a heartbeat. QUIC gives that for
//! free. TCP and Unix sockets do not: they are single ordered byte streams, and
//! a 4 MiB point cloud in front of a 40-byte heartbeat delays it by however
//! long the cloud takes to drain.
//!
//! This module is the missing piece. It is a small, precisely specified
//! multiplexer that gives TCP and UDS the same non-blocking, per-route
//! behaviour QUIC has natively, using nothing but the frame format
//! `astrs-wire` already defines.
//!
//! # The mini-protocol
//!
//! ## Framing
//!
//! Every frame on a muxed connection is an ordinary `astrs-wire` frame —
//! `magic "AS" | ver | flags | kind | len:u32 | payload | crc32c` (§7.1),
//! byte for byte, produced by the same codec. The mux adds **nine bytes at the
//! front of the payload**:
//!
//! ```text
//! ┌────────────────────────── astrs-wire frame ───────────────────────────┐
//! │ "AS" │ ver │ flags │ kind │ len │        payload        │  crc32c     │
//! └──────────────────────────────────────────┬────────────────────────────┘
//!                                            │
//!                    ┌───────────────────────┴──────────────────────┐
//!                    │ tag:u8 │ route:u64 LE │ body                 │
//!                    └──────────────────────────────────────────────┘
//!                        0        1..9          9..
//! ```
//!
//! The header is *inside* the payload, never beside it, for a reason that is
//! not stylistic: [`astrs_wire::FrameKind`] is a closed, snapshot-frozen enum
//! (§7.2 — `xtask snapshot-protocol` fails CI on any reorder), so there is no
//! mux frame kind to add and no spare flag bit to claim. The payload, by
//! contrast, is already this crate's to define: the compression container in
//! [`crate::compress`] is the same kind of convention. The frame's `kind` field
//! therefore keeps its normal meaning — a router can still dispatch on
//! `PeerEvent` versus `Data` without decoding — and `len` and `crc32c` still
//! cover everything.
//!
//! ## Tags
//!
//! | Tag | Route | Body | Meaning |
//! |---|---|---|---|
//! | `0` [`MuxTag::Control`] | must be `0` | the real payload | stream 0 |
//! | `1` [`MuxTag::RouteData`] | non-zero | the real payload | one route's data |
//! | `2` [`MuxTag::RouteOpen`] | non-zero | `credit:u32 LE` ‖ descriptor | open a logical stream |
//! | `3` [`MuxTag::RouteAccept`] | non-zero | `credit:u32 LE` ‖ `status:u8` ‖ detail | answer an open |
//! | `4` [`MuxTag::RouteClose`] | non-zero | `code:u16 LE` ‖ detail | close a logical stream |
//! | `5` [`MuxTag::Credit`] | non-zero | `delta:u32 LE` | grant window |
//! | `6` [`MuxTag::Datagram`] | must be `0` | the real payload | best effort |
//!
//! A route-scoped tag carrying route `0`, or a connection-scoped tag carrying a
//! route, is a protocol violation and closes the connection: the mux cannot
//! guess, and guessing wrong would deliver one route's bytes to another.
//!
//! ## Route handles
//!
//! Both ends open routes concurrently, so handles are **parity-partitioned**:
//! the initiator mints odd handles, the acceptor even ones, and `0` is reserved
//! (`RouteId::NONE`). No agreement is needed and no collision is possible. A
//! `RouteOpen` bearing the receiver's own parity is refused.
//!
//! ## Opening is optimistic
//!
//! [`RouteMux::open_route`] returns a usable handle immediately and queues the
//! `RouteOpen` behind it; the peer's `RouteAccept` only narrows the window or
//! refuses. A dataflow with a hundred routes therefore pays zero round trips to
//! set them up. The negotiation that genuinely needs an answer — the
//! application's [`astrs_wire::PeerEvent::RouteSetup`] and its `RouteAccept`,
//! carrying `RouteSpec`, generation and type URN — rides *on* the route once it
//! exists. The two layers are not redundant: this one creates a demux slot and
//! a flow-control window, that one agrees what the route means.
//!
//! ## Flow control
//!
//! Each route has a window measured in **frames**, not bytes, because the
//! per-frame payload ceiling is already negotiated at the handshake and a frame
//! count is what a queue depth is naturally expressed in.
//!
//! - A route opens with `initial_window_frames` of credit.
//! - Sending one `RouteData` frame costs one credit. At zero, the scheduler
//!   stops selecting the route; its send queue fills; its senders block. No
//!   other route is affected.
//! - The receiver grants credit back as the *application* consumes frames —
//!   not as they arrive — after `credit_grant_threshold` of them, which is
//!   half a window by default.
//!
//! The invariant this maintains is what makes the receive path allocation-free
//! and infallible: a route's inbound channel is sized to exactly its window, a
//! well-behaved peer can never have more than a window in flight, and so the
//! demux never has to block or drop. A peer that *does* overrun has broken the
//! protocol; its route is torn down with `CLOSE_CODE_FLOW_CONTROL` rather than
//! silently truncated.
//!
//! ## Fairness
//!
//! The scheduler serves control frames first, capped at `control_burst` in a
//! row, then one frame from the next ready route round-robin. A saturated
//! route can therefore delay a heartbeat by at most one frame's write, and a
//! control flood can delay route data by at most `control_burst`.
//!
//! # On QUIC
//!
//! The same mux runs unchanged over a QUIC bidirectional stream. That is a
//! deliberate choice rather than an oversight: one code path, exercised by
//! every UDS and TCP test, beats a second QUIC-only path that this build cannot
//! test end to end (see the crate-level "QUIC status" note). Native per-stream
//! QUIC mapping — `RouteOpen` becoming `open_uni_stream`, credit becoming QUIC
//! flow control — is a drop-in replacement for [`crate::mux::driver`] once
//! `oxiquic` can be exercised in-process.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::{CompressionPolicy, ConnectionCounters, MuxConfig, RouteMux, Side};
//! use astrs_wire::FrameKind;
//!
//! # fn main() {
//! # tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
//! let (mux, _receivers) = RouteMux::new(
//!     MuxConfig::new(),
//!     Side::Initiator,
//!     ConnectionCounters::shared(),
//!     CompressionPolicy::disabled(),
//!     1 << 20,
//!     64,
//! );
//!
//! let route = mux.open_route(b"camera->detector").expect("a route");
//! route.sender().try_send(FrameKind::Data, b"frame 0").expect("queued");
//! assert_eq!(mux.open_route_count(), 1);
//! # });
//! # }
//! ```

pub mod driver;
pub mod handle;
pub mod header;
pub mod state;

use std::sync::Arc;

use astrs_wire::RouteId;

use crate::config::{CompressionPolicy, MuxConfig, Side};
use crate::error::{CloseReason, TransportResult};
use crate::stats::{ConnectionCounters, ConnectionStatsSnapshot, TransportSnapshot};

pub use driver::{WRITE_BATCH, run_reader, run_writer};
pub use handle::{
    ControlReceiver, ControlSender, DatagramReceiver, DatagramSender, RouteReceiver, RouteSender,
    RouteStream,
};
pub use header::{
    ACCEPT_STATUS_OK, ACCEPT_STATUS_REFUSED, AcceptBody, CLOSE_CODE_DUPLICATE,
    CLOSE_CODE_FLOW_CONTROL, CLOSE_CODE_NORMAL, CLOSE_CODE_REFUSED, CLOSE_CODE_ROUTE_LIMIT,
    CLOSE_CODE_SHUTDOWN, CloseBody, MUX_HEADER_LEN, MUX_PROTOCOL, MuxHeader, MuxTag, OpenBody,
    build_payload, build_payload_into, decode_credit, encode_credit,
};
pub use state::{InboundRoute, MuxShared};

/// The application-facing face of a muxed connection.
///
/// Cheap to clone: every clone addresses the same connection.
#[derive(Debug, Clone)]
pub struct RouteMux {
    /// The shared state the driver tasks operate on.
    shared: Arc<MuxShared>,
}

/// The receiving halves a muxed connection hands to its owner.
#[derive(Debug)]
#[non_exhaustive]
pub struct MuxChannels {
    /// Inbound control-plane frames — stream 0.
    pub control: ControlReceiver,
    /// Inbound best-effort datagrams.
    pub datagrams: DatagramReceiver,
    /// Routes the peer opened, waiting to be claimed.
    pub accepts: RouteAcceptor,
}

/// The queue of routes the peer has opened.
#[derive(Debug)]
pub struct RouteAcceptor {
    /// The connection these routes belong to.
    shared: Arc<MuxShared>,
    /// The demux's accept queue.
    receiver: tokio::sync::mpsc::UnboundedReceiver<InboundRoute>,
}

impl RouteAcceptor {
    /// Waits for the next route the peer opens.
    ///
    /// Returns [`None`] once the connection has closed.
    pub async fn accept(&mut self) -> Option<RouteStream> {
        let inbound = self.receiver.recv().await?;
        Some(RouteStream::from_inbound(Arc::clone(&self.shared), inbound))
    }

    /// Takes the next route the peer opened, if one is already waiting.
    #[must_use]
    pub fn try_accept(&mut self) -> Option<RouteStream> {
        let inbound = self.receiver.try_recv().ok()?;
        Some(RouteStream::from_inbound(Arc::clone(&self.shared), inbound))
    }
}

impl RouteMux {
    /// Builds a mux and the channels its owner reads from.
    ///
    /// The caller is responsible for spawning [`run_writer`] and
    /// [`run_reader`] over the framed halves; [`crate::conn`] does that.
    #[must_use]
    pub fn new(
        config: MuxConfig,
        side: Side,
        counters: Arc<ConnectionCounters>,
        compression: CompressionPolicy,
        max_payload: usize,
        max_routes: u32,
    ) -> (Self, MuxChannels) {
        let (shared, receivers) =
            MuxShared::new(config, side, counters, compression, max_payload, max_routes);
        let channels = MuxChannels {
            control: ControlReceiver::new(receivers.control),
            datagrams: DatagramReceiver::new(receivers.datagrams),
            accepts: RouteAcceptor {
                shared: Arc::clone(&shared),
                receiver: receivers.accepts,
            },
        };
        (Self { shared }, channels)
    }

    /// Wraps an existing shared mux.
    #[must_use]
    pub const fn from_shared(shared: Arc<MuxShared>) -> Self {
        Self { shared }
    }

    /// The shared state, for a backend that must spawn the driver tasks.
    #[must_use]
    pub fn shared(&self) -> &Arc<MuxShared> {
        &self.shared
    }

    /// The outbound control plane — stream 0.
    #[must_use]
    pub fn control(&self) -> ControlSender {
        ControlSender::new(Arc::clone(&self.shared))
    }

    /// The outbound datagram channel.
    #[must_use]
    pub fn datagrams(&self) -> DatagramSender {
        DatagramSender::new(Arc::clone(&self.shared))
    }

    /// Opens a route with a freshly minted handle.
    ///
    /// # Errors
    ///
    /// As [`RouteMux::open_route_with_id`].
    pub fn open_route(&self, descriptor: &[u8]) -> TransportResult<RouteStream> {
        let route = self.shared.next_route_id();
        self.open_route_with_id(route, descriptor)
    }

    /// Opens a route with a caller-chosen handle.
    ///
    /// The handle must belong to this end's parity half; the peer refuses one
    /// that does not.
    ///
    /// # Errors
    ///
    /// - [`crate::TransportError::Closed`] if the connection has ended.
    /// - [`crate::TransportError::RouteLimitReached`] at the negotiated ceiling.
    /// - [`crate::TransportError::DuplicateRoute`] if the handle is in use.
    pub fn open_route_with_id(
        &self,
        route: RouteId,
        descriptor: &[u8],
    ) -> TransportResult<RouteStream> {
        let opened = self.shared.open_route(route, descriptor)?;
        Ok(RouteStream::from_opened(Arc::clone(&self.shared), opened))
    }

    /// How many routes are open.
    #[must_use]
    pub fn open_route_count(&self) -> usize {
        self.shared.open_route_count()
    }

    /// Whether the connection is still usable.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    /// Why the connection closed, if it has.
    #[must_use]
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.shared.close_reason()
    }

    /// Closes the connection and every route on it.
    pub fn close(&self, reason: CloseReason) {
        self.shared.close_all_routes("connection closing");
        self.shared.close(reason);
    }

    /// The connection's counters.
    #[must_use]
    pub fn connection_stats(&self) -> ConnectionStatsSnapshot {
        self.shared.counters().snapshot()
    }

    /// Connection and per-route counters in one value.
    #[must_use]
    pub fn snapshot(&self) -> TransportSnapshot {
        TransportSnapshot::new(self.connection_stats()).with_routes(self.shared.route_snapshots())
    }

    /// Applies a payload ceiling negotiated after the mux was built.
    pub fn set_max_payload(&self, bytes: usize) {
        self.shared.set_max_payload(bytes);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{Compression, FrameKind};

    fn mux() -> (RouteMux, MuxChannels) {
        RouteMux::new(
            MuxConfig::new(),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        )
    }

    #[test]
    fn opening_mints_handles_from_this_sides_parity() {
        let (mux, _channels) = mux();
        for _ in 0..4 {
            let stream = mux.open_route(b"").unwrap();
            assert!(Side::Initiator.owns_route(stream.route()));
        }
        assert_eq!(mux.open_route_count(), 4);
    }

    #[test]
    fn the_route_ceiling_is_enforced_on_outbound_opens() {
        let (mux, _channels) = mux();
        for _ in 0..8 {
            mux.open_route(b"").unwrap();
        }
        let err = mux.open_route(b"").unwrap_err();
        assert!(matches!(
            err,
            crate::TransportError::RouteLimitReached { limit: 8 }
        ));
    }

    #[test]
    fn a_chosen_handle_can_collide_exactly_once() {
        let (mux, _channels) = mux();
        mux.open_route_with_id(RouteId::new(11), b"").unwrap();
        let err = mux.open_route_with_id(RouteId::new(11), b"").unwrap_err();
        assert!(matches!(err, crate::TransportError::DuplicateRoute { .. }));
    }

    #[test]
    fn a_snapshot_covers_the_connection_and_every_route() {
        let (mux, _channels) = mux();
        let first = mux.open_route(b"").unwrap();
        let second = mux.open_route(b"").unwrap();
        first.sender().try_send(FrameKind::Data, b"aaaa").unwrap();

        let snapshot = mux.snapshot();
        assert_eq!(snapshot.routes.len(), 2);
        assert!(snapshot.routes.contains_key(&first.route()));
        assert!(snapshot.routes.contains_key(&second.route()));
        assert_eq!(snapshot.connection.routes_opened, 2);
        assert_eq!(snapshot.connection.routes_active, 2);
        assert_eq!(
            snapshot.routes[&first.route()].compression,
            Compression::None
        );
    }

    #[test]
    fn closing_the_mux_ends_every_route_and_reports_the_reason() {
        let (mux, _channels) = mux();
        let stream = mux.open_route(b"").unwrap();
        assert!(!mux.is_closed());

        mux.close(CloseReason::local("shutting down"));
        assert!(mux.is_closed());
        assert!(matches!(
            mux.close_reason(),
            Some(CloseReason::Local { .. })
        ));
        assert_eq!(mux.open_route_count(), 0);
        assert!(!stream.sender().is_open());
        assert!(mux.open_route(b"").is_err());
    }

    #[test]
    fn the_payload_ceiling_can_be_widened_after_the_handshake() {
        let (mux, _channels) = RouteMux::new(
            MuxConfig::new(),
            Side::Acceptor,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            64,
            8,
        );
        let stream = mux.open_route(b"").unwrap();
        assert!(
            stream
                .sender()
                .try_send(FrameKind::Data, &[0u8; 128])
                .is_err()
        );
        mux.set_max_payload(1 << 20);
        stream
            .sender()
            .try_send(FrameKind::Data, &[0u8; 128])
            .unwrap();
    }

    #[tokio::test]
    async fn the_acceptor_hands_out_streams_the_peer_opened() {
        let (mux, mut channels) = RouteMux::new(
            MuxConfig::new(),
            Side::Acceptor,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        );
        assert!(channels.accepts.try_accept().is_none());

        let mut body = Vec::new();
        OpenBody::new(4)
            .with_descriptor(b"spec".to_vec())
            .encode_into(&mut body);
        let frame = astrs_wire::Frame::new(
            FrameKind::PeerEvent,
            astrs_wire::FrameFlags::EMPTY,
            build_payload(MuxHeader::new(MuxTag::RouteOpen, RouteId::new(7)), &body),
        )
        .unwrap();
        mux.shared().dispatch(frame).await.unwrap();

        let stream = channels.accepts.accept().await.expect("an inbound route");
        assert_eq!(stream.route(), RouteId::new(7));
        assert_eq!(stream.descriptor(), b"spec");
    }

    #[tokio::test]
    async fn the_acceptor_ends_when_the_connection_does() {
        let (mux, mut channels) = mux();
        mux.close(CloseReason::Eof);
        assert!(channels.accepts.accept().await.is_none());
    }

    #[test]
    fn a_mux_can_be_rebuilt_from_its_shared_state() {
        let (mux, _channels) = mux();
        let clone = RouteMux::from_shared(Arc::clone(mux.shared()));
        let stream = clone.open_route(b"").unwrap();
        assert_eq!(mux.open_route_count(), 1);
        assert!(mux.shared().has_route(stream.route()));
    }

    #[test]
    fn the_control_and_datagram_senders_address_the_same_connection() {
        let (mux, _channels) = mux();
        assert!(mux.control().is_open());
        assert!(mux.datagrams().is_open());
        mux.close(CloseReason::Dropped);
        assert!(!mux.control().is_open());
        assert!(!mux.datagrams().is_open());
    }
}
