//! The handles an application holds onto a muxed connection.
//!
//! Every handle here is a thin, cheap value over the shared mux
//! ([`MuxShared`]): a [`RouteSender`] is an `Arc`, a route handle and a
//! semaphore. None of them owns a socket, so all of them are `Send + Sync`,
//! can be cloned across tasks, and stay valid while the driver task does the
//! actual I/O.
//!
//! | Handle | Direction | Backpressure |
//! |---|---|---|
//! | [`ControlSender`] | out, "stream 0" | bounded queue, `await` or typed overflow |
//! | [`ControlReceiver`] | in | bounded queue; not draining stalls the peer |
//! | [`RouteSender`] | out, one route | bounded queue **and** peer credit |
//! | [`RouteReceiver`] | in, one route | credit is granted as frames are consumed |
//! | [`DatagramSender`] | out, best effort | unreliable, never blocks |
//! | [`DatagramReceiver`] | in, best effort | drops when the queue is full |
//!
//! # Compression lives here
//!
//! A [`RouteSender`] compresses the payload before it ever reaches the queue
//! (§6.4): the codec runs on the *caller's* task, not the driver's, so a slow
//! zstd pass on one route cannot stall the writer that serves all of them.

use std::borrow::Cow;
use std::sync::Arc;

use astrs_wire::{Compression, Frame, FrameFlags, FrameKind, RouteId};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::compress::compress_payload;
use crate::config::CompressionPolicy;
use crate::error::{TransportError, TransportResult};
use crate::stats::{RouteCounters, RouteStatsSnapshot};

use super::header::{MuxHeader, MuxTag, build_payload};
use super::state::{InboundRoute, MuxShared, OpenedRoute};

/// One logical stream: a sender, a receiver, and the handle that names it.
#[derive(Debug)]
pub struct RouteStream {
    /// The outbound half.
    sender: RouteSender,
    /// The inbound half.
    receiver: RouteReceiver,
    /// The opaque descriptor the peer attached, for an accepted route.
    descriptor: Vec<u8>,
}

impl RouteStream {
    /// Builds a stream over a route this end opened.
    pub(crate) fn from_opened(shared: Arc<MuxShared>, opened: OpenedRoute) -> Self {
        let OpenedRoute {
            route,
            receiver,
            permits,
            counters,
            compression,
        } = opened;
        Self {
            sender: RouteSender {
                shared: Arc::clone(&shared),
                route,
                permits,
                counters: Arc::clone(&counters),
                compression: shared.compression().negotiate_codec(compression),
            },
            receiver: RouteReceiver {
                shared,
                route,
                receiver,
                counters,
            },
            descriptor: Vec::new(),
        }
    }

    /// Builds a stream over a route the peer opened.
    pub(crate) fn from_inbound(shared: Arc<MuxShared>, inbound: InboundRoute) -> Self {
        let InboundRoute {
            route,
            descriptor,
            receiver,
            counters,
            permits,
            compression,
        } = inbound;
        Self {
            sender: RouteSender {
                shared: Arc::clone(&shared),
                route,
                permits,
                counters: Arc::clone(&counters),
                compression: shared.compression().negotiate_codec(compression),
            },
            receiver: RouteReceiver {
                shared,
                route,
                receiver,
                counters,
            },
            descriptor,
        }
    }

    /// The handle that names this route on this connection.
    #[must_use]
    pub const fn route(&self) -> RouteId {
        self.sender.route
    }

    /// The opaque descriptor the peer attached to its open, if any.
    #[must_use]
    pub fn descriptor(&self) -> &[u8] {
        &self.descriptor
    }

    /// The outbound half.
    #[must_use]
    pub const fn sender(&self) -> &RouteSender {
        &self.sender
    }

    /// The inbound half.
    pub const fn receiver_mut(&mut self) -> &mut RouteReceiver {
        &mut self.receiver
    }

    /// Splits the stream so both directions can be driven by different tasks.
    #[must_use]
    pub fn into_halves(self) -> (RouteSender, RouteReceiver) {
        (self.sender, self.receiver)
    }

    /// This route's counters.
    #[must_use]
    pub fn stats(&self) -> RouteStatsSnapshot {
        self.sender.stats()
    }

    /// Closes this route without touching the rest of the connection.
    pub fn close(&self) {
        self.sender.close();
    }
}

/// The outbound half of one route.
///
/// Cloneable: several producers may feed one route, and the queue permits keep
/// them collectively bounded.
#[derive(Debug, Clone)]
pub struct RouteSender {
    /// The connection this route belongs to.
    shared: Arc<MuxShared>,
    /// Which route this is.
    route: RouteId,
    /// The send-queue permits — the local half of backpressure.
    permits: Arc<Semaphore>,
    /// This route's counters.
    counters: Arc<RouteCounters>,
    /// The codec this route uses.
    compression: CompressionPolicy,
}

impl RouteSender {
    /// The handle that names this route.
    #[must_use]
    pub const fn route(&self) -> RouteId {
        self.route
    }

    /// The codec this route compresses with.
    #[must_use]
    pub const fn compression(&self) -> Compression {
        self.compression.codec
    }

    /// Whether the connection is still usable.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.shared.is_closed() && self.shared.has_route(self.route)
    }

    /// Sends one payload, waiting for queue space if the route is backed up.
    ///
    /// Two independent limits apply, and both must clear:
    ///
    /// - the *local* queue depth, which this call awaits;
    /// - the *peer's* flow-control window, which the scheduler respects when it
    ///   dequeues. A route with no credit simply stops being selected, so its
    ///   queue fills and this call starts to block. That is the backpressure
    ///   chain working end to end.
    ///
    /// # Errors
    ///
    /// - [`TransportError::UnknownRoute`] if the route closed while waiting.
    /// - [`TransportError::FrameTooLarge`] if the payload exceeds the
    ///   negotiated ceiling.
    /// - [`TransportError::Codec`] if compression failed.
    pub async fn send(&self, kind: FrameKind, payload: &[u8]) -> TransportResult<()> {
        let permit = match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.shared.record_stall(self.route);
                Arc::clone(&self.permits)
                    .acquire_owned()
                    .await
                    .map_err(|_| TransportError::UnknownRoute { route: self.route })?
            }
        };
        self.enqueue(kind, payload, permit)
    }

    /// Sends one payload, failing rather than waiting.
    ///
    /// # Errors
    ///
    /// [`TransportError::SendQueueFull`] when the route's queue is full, plus
    /// everything [`RouteSender::send`] can return.
    pub fn try_send(&self, kind: FrameKind, payload: &[u8]) -> TransportResult<()> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().map_err(|_| {
            self.shared.record_stall(self.route);
            TransportError::SendQueueFull {
                route: self.route,
                capacity: self.shared.config().route_queue_depth,
            }
        })?;
        self.enqueue(kind, payload, permit)
    }

    /// Compresses, frames and queues one payload.
    fn enqueue(
        &self,
        kind: FrameKind,
        payload: &[u8],
        permit: OwnedSemaphorePermit,
    ) -> TransportResult<()> {
        let (body, codec) = self.encode_body(payload)?;
        let muxed = build_payload(MuxHeader::new(MuxTag::RouteData, self.route), &body);
        let frame = Frame::new(kind, FrameFlags::EMPTY.with_compression(codec), muxed)
            .map_err(TransportError::Wire)?;
        self.shared
            .queue_route_frame(self.route, frame, payload.len() as u64, Some(permit))
    }

    /// Applies the route's codec, measuring rather than assuming.
    fn encode_body<'a>(&self, payload: &'a [u8]) -> TransportResult<(Cow<'a, [u8]>, Compression)> {
        if !self.compression.codec.is_enabled() {
            return Ok((Cow::Borrowed(payload), Compression::None));
        }
        let (bytes, codec) = compress_payload(&self.compression, payload)?;
        if codec.is_enabled() {
            self.shared
                .record_compressed(self.route, payload.len() as u64, bytes.len() as u64);
            Ok((Cow::Owned(bytes), codec))
        } else {
            self.shared.counters().record_compression_skipped();
            Ok((Cow::Borrowed(payload), Compression::None))
        }
    }

    /// This route's counters.
    #[must_use]
    pub fn stats(&self) -> RouteStatsSnapshot {
        self.counters
            .snapshot(self.route)
            .with_compression(self.compression.codec)
    }

    /// Closes this route, telling the peer.
    pub fn close(&self) {
        self.shared.close_route(
            self.route,
            super::header::CLOSE_CODE_NORMAL,
            "route closed by application",
        );
    }
}

/// The inbound half of one route.
#[derive(Debug)]
pub struct RouteReceiver {
    /// The connection this route belongs to.
    shared: Arc<MuxShared>,
    /// Which route this is.
    route: RouteId,
    /// Where the demux delivers this route's frames.
    receiver: mpsc::Receiver<Frame>,
    /// This route's counters.
    counters: Arc<RouteCounters>,
}

impl RouteReceiver {
    /// The handle that names this route.
    #[must_use]
    pub const fn route(&self) -> RouteId {
        self.route
    }

    /// Receives the next frame, granting the peer credit as frames are
    /// consumed.
    ///
    /// Returns [`None`] once the route or the connection has closed and every
    /// buffered frame has been drained.
    pub async fn recv(&mut self) -> Option<Frame> {
        let frame = self.receiver.recv().await?;
        self.shared.note_consumed(self.route);
        Some(frame)
    }

    /// Receives the next frame if one is already buffered.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<Frame> {
        let frame = self.receiver.try_recv().ok()?;
        self.shared.note_consumed(self.route);
        Some(frame)
    }

    /// This route's counters.
    #[must_use]
    pub fn stats(&self) -> RouteStatsSnapshot {
        self.counters.snapshot(self.route)
    }

    /// Closes this route, telling the peer.
    pub fn close(&self) {
        self.shared.close_route(
            self.route,
            super::header::CLOSE_CODE_NORMAL,
            "route closed by application",
        );
    }
}

/// The outbound control plane — "stream 0" (§6.4).
#[derive(Debug, Clone)]
pub struct ControlSender {
    /// The connection this channel belongs to.
    shared: Arc<MuxShared>,
    /// The control queue's permits.
    permits: Arc<Semaphore>,
}

impl ControlSender {
    /// Builds a sender over `shared`.
    pub(crate) fn new(shared: Arc<MuxShared>) -> Self {
        let permits = shared.control_permits();
        Self { shared, permits }
    }

    /// Sends one control frame, waiting for queue space.
    ///
    /// # Errors
    ///
    /// [`TransportError::Closed`] if the connection ended, or
    /// [`TransportError::FrameTooLarge`] if the payload exceeds the ceiling.
    pub async fn send(&self, kind: FrameKind, payload: &[u8]) -> TransportResult<()> {
        let permit = match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.shared.counters().record_send_stall();
                Arc::clone(&self.permits)
                    .acquire_owned()
                    .await
                    .map_err(|_| TransportError::NotConnected)?
            }
        };
        self.shared.queue_control_metered(kind, payload, permit)
    }

    /// Sends one control message, encoded by `astrs-wire`.
    ///
    /// # Errors
    ///
    /// As [`ControlSender::send`], plus [`TransportError::Wire`] if the message
    /// does not encode.
    pub async fn send_message<T: astrs_wire::WireMessage>(
        &self,
        message: &T,
    ) -> TransportResult<()> {
        let payload = message.encode_to_vec().map_err(TransportError::Wire)?;
        self.send(T::KIND, &payload).await
    }

    /// Sends one control frame, failing rather than waiting.
    ///
    /// # Errors
    ///
    /// [`TransportError::SendQueueFull`] when the control queue is full, plus
    /// everything [`ControlSender::send`] can return.
    pub fn try_send(&self, kind: FrameKind, payload: &[u8]) -> TransportResult<()> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().map_err(|_| {
            self.shared.counters().record_send_stall();
            TransportError::SendQueueFull {
                route: RouteId::NONE,
                capacity: self.shared.config().control_queue_depth,
            }
        })?;
        self.shared.queue_control_metered(kind, payload, permit)
    }

    /// Whether the connection is still usable.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.shared.is_closed()
    }
}

/// The inbound control plane.
#[derive(Debug)]
pub struct ControlReceiver {
    /// Where the demux delivers control frames.
    receiver: mpsc::Receiver<Frame>,
}

impl ControlReceiver {
    /// Builds a receiver over a demux endpoint.
    pub(crate) const fn new(receiver: mpsc::Receiver<Frame>) -> Self {
        Self { receiver }
    }

    /// Receives the next control frame, or [`None`] once the connection has
    /// closed and the queue has drained.
    ///
    /// A consumer that stops calling this applies backpressure all the way to
    /// the peer's socket: the demux awaits delivery rather than dropping
    /// control frames, because a lost protocol message is worse than a stalled
    /// connection.
    pub async fn recv(&mut self) -> Option<Frame> {
        self.receiver.recv().await
    }

    /// Receives the next control frame if one is already buffered.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<Frame> {
        self.receiver.try_recv().ok()
    }

    /// Receives and decodes the next control message.
    ///
    /// # Errors
    ///
    /// [`TransportError::UnexpectedFrame`] if the family does not match,
    /// [`TransportError::Wire`] if the payload does not decode.
    pub async fn recv_message<T: astrs_wire::WireMessage>(&mut self) -> TransportResult<Option<T>> {
        let Some(frame) = self.recv().await else {
            return Ok(None);
        };
        if frame.kind() != T::KIND {
            return Err(TransportError::UnexpectedFrame {
                expected: T::KIND,
                found: frame.kind(),
            });
        }
        T::from_frame(&frame.as_view())
            .map(Some)
            .map_err(TransportError::Wire)
    }
}

/// The outbound datagram channel.
///
/// Datagrams are the "latest value wins" path of §6.4: a pose at 200 Hz whose
/// consumer only ever wants the newest one. On QUIC they map to native
/// `DATAGRAM` frames when the peer enabled them; everywhere else — and on a
/// QUIC connection whose peer advertised `max_datagram_frame_size = 0` — they
/// degrade to a tagged frame on the shared stream, which is the transparent
/// degradation the blueprint's risk register calls for (§23, risk 5).
///
/// The degradation preserves the *semantics* a datagram user depends on:
/// no retransmission wait, no head-of-line blocking behind route data, and a
/// bounded receive queue that drops rather than grows. It does not preserve
/// unreliability — a stream-carried datagram will not be lost in transit —
/// which is a strictly weaker promise and therefore safe.
#[derive(Debug, Clone)]
pub struct DatagramSender {
    /// The connection this channel belongs to.
    shared: Arc<MuxShared>,
}

impl DatagramSender {
    /// Builds a sender over `shared`.
    pub(crate) const fn new(shared: Arc<MuxShared>) -> Self {
        Self { shared }
    }

    /// Sends one datagram, best effort.
    ///
    /// Never waits: a datagram that cannot go out now has already missed its
    /// moment.
    ///
    /// # Errors
    ///
    /// [`TransportError::Closed`] if the connection ended, or
    /// [`TransportError::DatagramTooLarge`] if the payload exceeds the
    /// datagram ceiling.
    pub fn send(&self, kind: FrameKind, payload: &[u8]) -> TransportResult<()> {
        self.shared.queue_datagram(kind, payload)
    }

    /// Whether the connection is still usable.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.shared.is_closed()
    }
}

/// The inbound datagram channel.
#[derive(Debug)]
pub struct DatagramReceiver {
    /// Where the demux delivers datagrams.
    receiver: mpsc::Receiver<Frame>,
}

impl DatagramReceiver {
    /// Builds a receiver over a demux endpoint.
    pub(crate) const fn new(receiver: mpsc::Receiver<Frame>) -> Self {
        Self { receiver }
    }

    /// Receives the next datagram, or [`None`] once the connection has closed.
    pub async fn recv(&mut self) -> Option<Frame> {
        self.receiver.recv().await
    }

    /// Receives the next datagram if one is already buffered.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<Frame> {
        self.receiver.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::{MuxConfig, Side};
    use crate::mux::header::MuxHeader;
    use crate::stats::ConnectionCounters;
    use astrs_wire::Compression;

    fn mux(config: MuxConfig, compression: CompressionPolicy) -> Arc<MuxShared> {
        MuxShared::new(
            config,
            Side::Initiator,
            ConnectionCounters::shared(),
            compression,
            1 << 20,
            64,
        )
        .0
    }

    fn open(shared: &Arc<MuxShared>) -> RouteStream {
        let route = shared.next_route_id();
        let opened = shared.open_route(route, b"descriptor").unwrap();
        RouteStream::from_opened(Arc::clone(shared), opened)
    }

    #[tokio::test]
    async fn a_route_send_queues_a_tagged_frame() {
        let shared = mux(MuxConfig::new(), CompressionPolicy::disabled());
        let stream = open(&shared);
        // Drain the `RouteOpen`.
        let _ = shared.take_batch(4);

        stream
            .sender()
            .send(FrameKind::PeerEvent, b"an arrow batch")
            .await
            .unwrap();
        let batch = shared.take_batch(4);
        assert_eq!(batch.len(), 1);
        let (header, body) = MuxHeader::decode(batch[0].frame.payload()).unwrap();
        assert_eq!(header.tag, MuxTag::RouteData);
        assert_eq!(header.route, stream.route());
        assert_eq!(body, b"an arrow batch");
        assert_eq!(batch[0].frame.flags().compression(), Compression::None);
    }

    #[tokio::test]
    async fn a_large_payload_is_compressed_and_flagged() {
        let policy = CompressionPolicy::codec(Compression::Lz4).with_threshold_bytes(64);
        let shared = mux(MuxConfig::new(), policy);
        let stream = open(&shared);
        let _ = shared.take_batch(4);

        let payload: Vec<u8> = (0..8_192).map(|index| (index % 5) as u8).collect();
        stream
            .sender()
            .send(FrameKind::Data, &payload)
            .await
            .unwrap();

        let batch = shared.take_batch(4);
        assert_eq!(batch[0].frame.flags().compression(), Compression::Lz4);
        let (_, body) = MuxHeader::decode(batch[0].frame.payload()).unwrap();
        assert!(body.len() < payload.len());
        assert_eq!(shared.counters().snapshot().frames_compressed, 1);
    }

    #[tokio::test]
    async fn a_small_payload_skips_the_codec() {
        let policy = CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(4_096);
        let shared = mux(MuxConfig::new(), policy);
        let stream = open(&shared);
        let _ = shared.take_batch(4);

        stream
            .sender()
            .send(FrameKind::Data, b"tiny")
            .await
            .unwrap();
        let batch = shared.take_batch(4);
        assert_eq!(batch[0].frame.flags().compression(), Compression::None);
        assert_eq!(shared.counters().snapshot().frames_compressed, 0);
    }

    #[tokio::test]
    async fn a_full_route_queue_is_a_typed_overflow_not_a_wait() {
        let shared = mux(
            MuxConfig::new()
                .with_route_queue_depth(2)
                .with_initial_window_frames(1),
            CompressionPolicy::disabled(),
        );
        let stream = open(&shared);
        let _ = shared.take_batch(8);

        stream.sender().try_send(FrameKind::Data, b"a").unwrap();
        stream.sender().try_send(FrameKind::Data, b"b").unwrap();
        let err = stream.sender().try_send(FrameKind::Data, b"c").unwrap_err();
        match err {
            TransportError::SendQueueFull { route, capacity } => {
                assert_eq!(route, stream.route());
                assert_eq!(capacity, 2);
            }
            other => panic!("expected an overflow, got {other:?}"),
        }
        assert_eq!(shared.counters().snapshot().send_stalls, 1);
    }

    #[tokio::test]
    async fn a_permit_is_released_once_the_frame_is_written() {
        let shared = mux(
            MuxConfig::new().with_route_queue_depth(1),
            CompressionPolicy::disabled(),
        );
        let stream = open(&shared);
        let _ = shared.take_batch(8);

        stream.sender().try_send(FrameKind::Data, b"a").unwrap();
        assert!(stream.sender().try_send(FrameKind::Data, b"b").is_err());

        // Draining the batch drops the permit with the `Outbound`.
        drop(shared.take_batch(8));
        stream.sender().try_send(FrameKind::Data, b"b").unwrap();
    }

    #[tokio::test]
    async fn receiving_grants_credit_back() {
        let shared = mux(
            MuxConfig::new().with_initial_window_frames(2),
            CompressionPolicy::disabled(),
        );
        let mut stream = open(&shared);
        let _ = shared.take_batch(8);

        let route = stream.route();
        shared
            .dispatch(
                Frame::new(
                    FrameKind::Data,
                    FrameFlags::EMPTY,
                    build_payload(MuxHeader::new(MuxTag::RouteData, route), b"one"),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let frame = stream.receiver_mut().recv().await.unwrap();
        assert_eq!(frame.payload(), b"one");

        // The threshold for a window of two is one frame, so a grant is queued.
        let batch = shared.take_batch(8);
        assert_eq!(batch.len(), 1);
        let (header, _) = MuxHeader::decode(batch[0].frame.payload()).unwrap();
        assert_eq!(header.tag, MuxTag::Credit);
    }

    #[tokio::test]
    async fn a_closed_route_ends_its_receiver() {
        let shared = mux(MuxConfig::new(), CompressionPolicy::disabled());
        let mut stream = open(&shared);
        assert!(stream.sender().is_open());
        stream.close();
        assert!(!stream.sender().is_open());
        assert!(stream.receiver_mut().recv().await.is_none());
    }

    #[tokio::test]
    async fn the_halves_can_be_driven_separately() {
        let shared = mux(MuxConfig::new(), CompressionPolicy::disabled());
        let stream = open(&shared);
        let route = stream.route();
        let (sender, mut receiver) = stream.into_halves();
        assert_eq!(sender.route(), route);
        assert_eq!(receiver.route(), route);
        assert_eq!(sender.compression(), Compression::None);
        assert_eq!(sender.stats().route, route);
        assert_eq!(receiver.stats().route, route);
        assert!(receiver.try_recv().is_none());
        receiver.close();
        assert!(!sender.is_open());
    }

    #[tokio::test]
    async fn the_control_channel_round_trips_a_message() {
        let (shared, mut receivers) = MuxShared::new(
            MuxConfig::new(),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        );
        let sender = ControlSender::new(Arc::clone(&shared));
        assert!(sender.is_open());
        sender
            .send_message(&astrs_wire::ControlRequest::List { all: true })
            .await
            .unwrap();

        let batch = shared.take_batch(4);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].frame.kind(), FrameKind::Control);

        // Feed it back through the demux and read it from the receiver.
        shared.dispatch(batch[0].frame.clone()).await.unwrap();
        let mut control = ControlReceiver::new(receivers.control);
        let request: astrs_wire::ControlRequest = control.recv_message().await.unwrap().unwrap();
        assert_eq!(request, astrs_wire::ControlRequest::List { all: true });
        receivers.datagrams.close();
    }

    #[tokio::test]
    async fn the_control_queue_overflows_with_a_typed_error() {
        let shared = mux(
            MuxConfig::new().with_control_queue_depth(2),
            CompressionPolicy::disabled(),
        );
        let sender = ControlSender::new(Arc::clone(&shared));
        sender.try_send(FrameKind::Control, b"a").unwrap();
        sender.try_send(FrameKind::Control, b"b").unwrap();
        let err = sender.try_send(FrameKind::Control, b"c").unwrap_err();
        match err {
            TransportError::SendQueueFull { route, capacity } => {
                assert_eq!(route, RouteId::NONE);
                assert_eq!(capacity, 2);
            }
            other => panic!("expected an overflow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_control_message_of_the_wrong_family_is_rejected() {
        let (shared, receivers) = MuxShared::new(
            MuxConfig::new(),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        );
        let mut control = ControlReceiver::new(receivers.control);
        shared
            .dispatch(
                Frame::new(
                    FrameKind::Log,
                    FrameFlags::EMPTY,
                    build_payload(MuxHeader::control(), b"x"),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let err = control
            .recv_message::<astrs_wire::ControlRequest>()
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::UnexpectedFrame { .. }));
    }

    #[tokio::test]
    async fn a_closed_control_channel_ends_cleanly() {
        let (shared, receivers) = MuxShared::new(
            MuxConfig::new(),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        );
        let mut control = ControlReceiver::new(receivers.control);
        shared.close(crate::error::CloseReason::Eof);
        assert!(control.recv().await.is_none());
        assert!(
            control
                .recv_message::<astrs_wire::ControlRequest>()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn datagrams_never_block_and_are_dropped_when_late() {
        let (shared, receivers) = MuxShared::new(
            MuxConfig::new().with_datagram_queue_depth(1),
            Side::Initiator,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        );
        let sender = DatagramSender::new(Arc::clone(&shared));
        assert!(sender.is_open());
        for _ in 0..4 {
            sender.send(FrameKind::Data, b"pose").unwrap();
        }
        assert_eq!(shared.counters().snapshot().datagrams_sent, 4);

        // Round-trip two through the demux; the second is dropped.
        let batch = shared.take_batch(8);
        for item in &batch {
            shared.dispatch(item.frame.clone()).await.unwrap();
        }
        let mut receiver = DatagramReceiver::new(receivers.datagrams);
        assert!(receiver.try_recv().is_some());
        assert!(receiver.try_recv().is_none());
        assert!(shared.counters().snapshot().datagrams_dropped >= 1);
    }

    #[tokio::test]
    async fn an_inbound_route_carries_its_descriptor() {
        let (shared, mut receivers) = MuxShared::new(
            MuxConfig::new(),
            Side::Acceptor,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        );
        let mut body = Vec::new();
        super::super::header::OpenBody::new(4)
            .with_descriptor(b"route-spec".to_vec())
            .encode_into(&mut body);
        shared
            .dispatch(
                Frame::new(
                    FrameKind::PeerEvent,
                    FrameFlags::EMPTY,
                    build_payload(MuxHeader::new(MuxTag::RouteOpen, RouteId::new(1)), &body),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let inbound = receivers.accepts.try_recv().unwrap();
        let stream = RouteStream::from_inbound(Arc::clone(&shared), inbound);
        assert_eq!(stream.route(), RouteId::new(1));
        assert_eq!(stream.descriptor(), b"route-spec");
        assert_eq!(stream.stats().route, RouteId::new(1));
    }
}
