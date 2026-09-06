//! One framed byte stream, shared by every backend.
//!
//! UDS, TCP and QUIC differ in how bytes get from one process to another and
//! in nothing else: all three carry the *same* `astrs-wire` frames —
//! `magic "AS" | ver | flags | kind | len:u32 | payload | crc32c` (§7.1) —
//! encoded by the same codec. [`FramedStream`] is that shared layer, and it is
//! generic over `tokio::io::AsyncRead + AsyncWrite`, which is exactly what all
//! three backends can produce:
//!
//! | Backend | Read half | Write half |
//! |---|---|---|
//! | UDS | `tokio::net::unix::OwnedReadHalf` | `OwnedWriteHalf` |
//! | TCP | `tokio::net::tcp::OwnedReadHalf` | `OwnedWriteHalf` |
//! | QUIC | `oxiquic` `RecvStreamHandle` | `SendStreamHandle` |
//!
//! # Why the halves are split
//!
//! A connection reads and writes concurrently: the driver task must be able to
//! poll for an inbound frame while another task is blocked writing an outbound
//! one. If a single value owned the whole stream, every send would serialise
//! behind a read. [`FramedStream::into_halves`] therefore hands out an
//! independent [`FramedReader`] and [`FramedWriter`], each owning one half of
//! a `tokio::io::split`, so the two directions never contend.
//!
//! # Limits are enforced in both directions
//!
//! `astrs-wire`'s reader refuses an inbound frame that declares more than the
//! policy allows, which protects this process. This module adds the mirror
//! check on the *send* path, so that a bug here cannot emit a frame the peer
//! is obliged to refuse. The handshake sets one [`FrameLimits`] and both
//! directions honour it (§7.2).
//!
//! # Examples
//!
//! ```
//! use astrs_transport::{ConnectionCounters, FramedStream};
//! use astrs_wire::{ControlRequest, FrameKind, FrameLimits};
//!
//! # tokio_test_block(async {
//! let (client, server) = tokio::io::duplex(64 * 1024);
//! let limits = FrameLimits::uds();
//!
//! let mut client = FramedStream::new(client, limits, ConnectionCounters::shared());
//! let mut server = FramedStream::new(server, limits, ConnectionCounters::shared());
//!
//! client.send_message(&ControlRequest::List { all: true }).await?;
//! let frame = server.recv_frame().await?.expect("a frame");
//! assert_eq!(frame.kind(), FrameKind::Control);
//! # Ok::<(), astrs_transport::TransportError>(())
//! # });
//! # fn tokio_test_block<F: std::future::Future<Output = Result<(), astrs_transport::TransportError>>>(f: F) {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f).unwrap();
//! # }
//! ```

use std::future::Future;
use std::sync::Arc;

use astrs_wire::{
    AsyncFrameReader, AsyncFrameWriter, Compression, Frame, FrameFlags, FrameKind, FrameLimits,
    WireMessage,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};

use crate::error::{TransportError, TransportResult};
use crate::stats::ConnectionCounters;

/// The read half of a framed connection.
///
/// Everything a caller needs to pull complete, checked frames off a byte
/// stream: the buffer, the policy, and the counters.
#[derive(Debug)]
pub struct FramedReader<R> {
    /// The `astrs-wire` reader that owns the reassembly buffer.
    inner: AsyncFrameReader<R>,
    /// Shared connection counters.
    counters: Arc<ConnectionCounters>,
}

impl<R: AsyncRead + Unpin> FramedReader<R> {
    /// A reader over `inner`, checked against `limits`.
    #[must_use]
    pub fn new(inner: R, limits: FrameLimits, counters: Arc<ConnectionCounters>) -> Self {
        Self {
            inner: AsyncFrameReader::new(inner, limits),
            counters,
        }
    }

    /// A reader with a chosen initial buffer capacity.
    #[must_use]
    pub fn with_capacity(
        inner: R,
        limits: FrameLimits,
        capacity: usize,
        counters: Arc<ConnectionCounters>,
    ) -> Self {
        Self {
            inner: AsyncFrameReader::with_capacity(inner, limits, capacity),
            counters,
        }
    }

    /// The policy inbound frames are checked against.
    #[must_use]
    pub const fn limits(&self) -> &FrameLimits {
        self.inner.limits()
    }

    /// Widens (or narrows) the policy, as the handshake negotiates it.
    ///
    /// A connection opens with a deliberately small ceiling — a peer that has
    /// not said who it is may not make this process allocate — and widens to
    /// the negotiated budget once the greeting completes (§7.2).
    pub const fn set_limits(&mut self, limits: FrameLimits) {
        self.inner.set_limits(limits);
    }

    /// The counters this reader updates.
    #[must_use]
    pub fn counters(&self) -> &Arc<ConnectionCounters> {
        &self.counters
    }

    /// How many complete frames have been read.
    #[must_use]
    pub const fn frames_read(&self) -> u64 {
        self.inner.frames_read()
    }

    /// Reads the next complete frame, or [`None`] at a clean end of stream.
    ///
    /// # Errors
    ///
    /// [`TransportError::Wire`] for a malformed or oversized frame, a checksum
    /// mismatch, or a stream that ended mid-frame;
    /// [`TransportError::Io`] for a socket failure.
    pub async fn recv_frame(&mut self) -> TransportResult<Option<Frame>> {
        match self.inner.read_frame().await {
            Ok(Some(frame)) => {
                self.counters.record_frame_received(
                    frame.header().total_len() as u64,
                    frame.payload().len() as u64,
                );
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                self.counters.record_error();
                Err(TransportError::Wire(err))
            }
        }
    }

    /// Reads the next frame, failing if the stream ended instead.
    ///
    /// # Errors
    ///
    /// As [`FramedReader::recv_frame`], plus [`TransportError::UnexpectedEof`]
    /// when the peer closed with a frame still owed.
    pub async fn expect_frame(&mut self, expected: FrameKind) -> TransportResult<Frame> {
        self.recv_frame()
            .await?
            .ok_or(TransportError::UnexpectedEof { expected })
    }

    /// Reads the next frame and decodes it as `T`.
    ///
    /// # Errors
    ///
    /// As [`FramedReader::expect_frame`], plus
    /// [`TransportError::UnexpectedFrame`] if the family does not match and
    /// [`TransportError::Wire`] if the payload does not decode.
    pub async fn expect_message<T: WireMessage>(
        &mut self,
        expected: FrameKind,
    ) -> TransportResult<T> {
        let frame = self.expect_frame(expected).await?;
        if frame.kind() != expected {
            return Err(TransportError::UnexpectedFrame {
                expected,
                found: frame.kind(),
            });
        }
        T::from_frame(&frame.as_view()).map_err(TransportError::Wire)
    }

    /// Consumes the reader and returns the stream, discarding any buffered
    /// bytes.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

/// The write half of a framed connection.
#[derive(Debug)]
pub struct FramedWriter<W> {
    /// The `astrs-wire` writer that owns the pending buffer.
    inner: AsyncFrameWriter<W>,
    /// Shared connection counters.
    counters: Arc<ConnectionCounters>,
    /// The flags every frame carries before compression is applied.
    ///
    /// Exactly `CRC` or `EMPTY`, decided by the policy: a network leg always
    /// checksums, a Unix socket may not bother (§7.1).
    base_flags: FrameFlags,
}

impl<W: AsyncWrite + Unpin> FramedWriter<W> {
    /// A writer over `inner`, obeying `limits`.
    #[must_use]
    pub fn new(inner: W, limits: FrameLimits, counters: Arc<ConnectionCounters>) -> Self {
        let base_flags = base_flags_for(&limits);
        Self {
            inner: AsyncFrameWriter::new(inner, limits),
            counters,
            base_flags,
        }
    }

    /// The policy outbound frames are checked against.
    #[must_use]
    pub const fn limits(&self) -> &FrameLimits {
        self.inner.limits()
    }

    /// Widens (or narrows) the policy, as the handshake negotiates it.
    ///
    /// The checksum policy follows the limits, so a connection that negotiated
    /// mandatory checksums starts emitting them from the next frame.
    pub fn set_limits(&mut self, limits: FrameLimits) {
        self.base_flags = base_flags_for(&limits);
        self.inner.set_limits(limits);
    }

    /// The counters this writer updates.
    #[must_use]
    pub fn counters(&self) -> &Arc<ConnectionCounters> {
        &self.counters
    }

    /// How many frames have been queued.
    #[must_use]
    pub const fn frames_written(&self) -> u64 {
        self.inner.frames_written()
    }

    /// The flags a frame with `compression` will be written with.
    #[must_use]
    pub const fn flags_for(&self, compression: Compression) -> FrameFlags {
        self.base_flags.with_compression(compression)
    }

    /// Queues a payload without flushing.
    ///
    /// Queueing several frames and flushing once is how the driver amortises
    /// syscalls across a burst; a caller with one frame to send should use
    /// [`FramedWriter::send_raw`].
    ///
    /// # Errors
    ///
    /// [`TransportError::FrameTooLarge`] if the payload exceeds the negotiated
    /// ceiling — the mirror of the check the peer's reader applies.
    pub fn queue_raw(
        &mut self,
        kind: FrameKind,
        compression: Compression,
        payload: &[u8],
    ) -> TransportResult<usize> {
        self.check_payload_len(payload.len())?;
        let flags = self.flags_for(compression);
        let written = self
            .inner
            .queue_frame(kind, flags, payload)
            .map_err(TransportError::Wire)?;
        Ok(written)
    }

    /// Queues an already-built frame, keeping its flags.
    ///
    /// Used by the mux, which builds frames whose compression bits were
    /// decided when the payload was encoded.
    ///
    /// # Errors
    ///
    /// As [`FramedWriter::queue_raw`].
    pub fn queue_frame(&mut self, frame: &Frame) -> TransportResult<usize> {
        self.check_payload_len(frame.payload().len())?;
        // Preserve the frame's compression bits, but let the connection policy
        // decide the checksum: the frame may have been built before the
        // handshake settled whether this leg checksums.
        let flags = self
            .base_flags
            .with_compression(frame.flags().compression());
        self.inner
            .queue_frame(frame.kind(), flags, frame.payload())
            .map_err(TransportError::Wire)
    }

    /// Queues and flushes one payload.
    ///
    /// # Errors
    ///
    /// As [`FramedWriter::queue_raw`], plus [`TransportError::Io`].
    pub async fn send_raw(
        &mut self,
        kind: FrameKind,
        compression: Compression,
        payload: &[u8],
    ) -> TransportResult<()> {
        let wire_bytes = self.queue_raw(kind, compression, payload)?;
        self.flush().await?;
        self.counters
            .record_frame_sent(wire_bytes as u64, payload.len() as u64);
        Ok(())
    }

    /// Queues and flushes one frame.
    ///
    /// # Errors
    ///
    /// As [`FramedWriter::queue_frame`], plus [`TransportError::Io`].
    pub async fn send_frame(&mut self, frame: &Frame) -> TransportResult<()> {
        let wire_bytes = self.queue_frame(frame)?;
        self.flush().await?;
        self.counters
            .record_frame_sent(wire_bytes as u64, frame.payload().len() as u64);
        Ok(())
    }

    /// Encodes and flushes one protocol message.
    ///
    /// # Errors
    ///
    /// [`TransportError::Wire`] if the message does not encode or exceeds the
    /// ceiling, [`TransportError::Io`] if the socket fails.
    pub async fn send_message<T: WireMessage>(&mut self, message: &T) -> TransportResult<()> {
        // `queue` uses the writer's own flags, which never include compression:
        // a protocol message is small by construction and is never a codec
        // candidate.
        let wire_bytes = self.inner.queue(message).map_err(TransportError::Wire)?;
        self.flush().await?;
        self.counters.record_frame_sent(wire_bytes as u64, 0);
        Ok(())
    }

    /// Writes everything queued to the socket.
    ///
    /// # Errors
    ///
    /// [`TransportError::Io`] if the socket fails.
    pub async fn flush(&mut self) -> TransportResult<()> {
        match self.inner.flush().await {
            Ok(()) => Ok(()),
            Err(err) => {
                self.counters.record_error();
                Err(TransportError::Wire(err))
            }
        }
    }

    /// Flushes and shuts the write half down.
    ///
    /// # Errors
    ///
    /// [`TransportError::Io`] if the socket fails.
    pub async fn shutdown(&mut self) -> TransportResult<()> {
        self.inner.shutdown().await.map_err(TransportError::Wire)
    }

    /// Records a frame that was queued earlier and has now been flushed.
    ///
    /// The batching path queues several frames before one flush, so the
    /// per-frame accounting cannot live inside `flush`.
    pub fn record_sent(&self, wire_bytes: u64, payload_bytes: u64) {
        self.counters.record_frame_sent(wire_bytes, payload_bytes);
    }

    /// Consumes the writer and returns the stream, discarding anything queued.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner.into_inner()
    }

    /// The symmetric send-side ceiling check.
    fn check_payload_len(&self, len: usize) -> TransportResult<()> {
        let limit = self.inner.limits().max_payload_bytes();
        if len > limit {
            return Err(TransportError::FrameTooLarge { actual: len, limit });
        }
        Ok(())
    }
}

/// The flags every frame on a connection carries before compression.
const fn base_flags_for(limits: &FrameLimits) -> FrameFlags {
    if limits.require_crc() {
        FrameFlags::CRC
    } else {
        FrameFlags::EMPTY
    }
}

/// A full-duplex framed connection over an independent read and write half.
///
/// Owns both halves until [`FramedDuplex::into_halves`] separates them. The
/// combined form is what the handshake uses — a strict request/response
/// exchange with no concurrency — and the split form is what the driver tasks
/// use afterwards.
///
/// The two halves are separate type parameters because not every backend has
/// one value that is both: `oxiquic` hands out a `SendStreamHandle` and a
/// `RecvStreamHandle` that were never a single stream. [`FramedStream`] is the
/// alias for the common case where they came from splitting one.
#[derive(Debug)]
pub struct FramedDuplex<R, W> {
    /// The read half.
    reader: FramedReader<R>,
    /// The write half.
    writer: FramedWriter<W>,
}

/// A framed connection over one byte stream, split internally.
pub type FramedStream<S> = FramedDuplex<ReadHalf<S>, WriteHalf<S>>;

impl<S: AsyncRead + AsyncWrite + Unpin> FramedDuplex<ReadHalf<S>, WriteHalf<S>> {
    /// Splits `stream` and wraps both halves in the frame codec.
    #[must_use]
    pub fn new(stream: S, limits: FrameLimits, counters: Arc<ConnectionCounters>) -> Self {
        Self::with_capacity(
            stream,
            limits,
            astrs_wire::DEFAULT_BUFFER_CAPACITY,
            counters,
        )
    }

    /// Splits `stream` with a chosen read-buffer capacity.
    #[must_use]
    pub fn with_capacity(
        stream: S,
        limits: FrameLimits,
        capacity: usize,
        counters: Arc<ConnectionCounters>,
    ) -> Self {
        let (read_half, write_half) = tokio::io::split(stream);
        Self::from_halves(read_half, write_half, limits, capacity, counters)
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> FramedDuplex<R, W> {
    /// Wraps two already-separate halves in the frame codec.
    ///
    /// This is the constructor a backend uses when its read and write halves
    /// were never one value — a QUIC bidirectional stream, or a pair of pipes.
    #[must_use]
    pub fn from_halves(
        read: R,
        write: W,
        limits: FrameLimits,
        capacity: usize,
        counters: Arc<ConnectionCounters>,
    ) -> Self {
        Self {
            reader: FramedReader::with_capacity(read, limits, capacity, Arc::clone(&counters)),
            writer: FramedWriter::new(write, limits, counters),
        }
    }

    /// The read half.
    pub const fn reader_mut(&mut self) -> &mut FramedReader<R> {
        &mut self.reader
    }

    /// The write half.
    pub const fn writer_mut(&mut self) -> &mut FramedWriter<W> {
        &mut self.writer
    }

    /// The policy both directions are checked against.
    #[must_use]
    pub const fn limits(&self) -> &FrameLimits {
        self.reader.limits()
    }

    /// Applies a policy to both directions at once.
    ///
    /// Symmetry is the point: §7.2 negotiates one budget, and a connection
    /// that enforced it on receive but not on send would emit frames its peer
    /// must refuse.
    pub fn set_limits(&mut self, limits: FrameLimits) {
        self.reader.set_limits(limits);
        self.writer.set_limits(limits);
    }

    /// Reads the next frame.
    ///
    /// # Errors
    ///
    /// As [`FramedReader::recv_frame`].
    pub async fn recv_frame(&mut self) -> TransportResult<Option<Frame>> {
        self.reader.recv_frame().await
    }

    /// Reads the next frame and decodes it as `T`.
    ///
    /// # Errors
    ///
    /// As [`FramedReader::expect_message`].
    pub async fn expect_message<T: WireMessage>(
        &mut self,
        expected: FrameKind,
    ) -> TransportResult<T> {
        self.reader.expect_message(expected).await
    }

    /// Encodes and sends one protocol message.
    ///
    /// # Errors
    ///
    /// As [`FramedWriter::send_message`].
    pub async fn send_message<T: WireMessage>(&mut self, message: &T) -> TransportResult<()> {
        self.writer.send_message(message).await
    }

    /// Sends one payload.
    ///
    /// # Errors
    ///
    /// As [`FramedWriter::send_raw`].
    pub async fn send_raw(
        &mut self,
        kind: FrameKind,
        compression: Compression,
        payload: &[u8],
    ) -> TransportResult<()> {
        self.writer.send_raw(kind, compression, payload).await
    }

    /// Separates the two halves so they can be driven concurrently.
    #[must_use]
    pub fn into_halves(self) -> (FramedReader<R>, FramedWriter<W>) {
        (self.reader, self.writer)
    }
}

/// The outbound half of a framed link, as the driver task sees it.
///
/// Abstracting the driver over this trait rather than over a concrete stream
/// is what lets one scheduler serve all three backends: QUIC hands out
/// `AsyncWrite` stream handles that are not `WriteHalf<S>` for any `S` this
/// crate owns.
pub trait FrameSink: Send + 'static {
    /// Queues a frame without flushing.
    ///
    /// # Errors
    ///
    /// Implementation-defined; typically [`TransportError::FrameTooLarge`].
    fn queue(&mut self, frame: &Frame) -> TransportResult<usize>;

    /// Flushes everything queued.
    ///
    /// # Errors
    ///
    /// Implementation-defined; typically [`TransportError::Io`].
    fn flush(&mut self) -> impl Future<Output = TransportResult<()>> + Send;

    /// Flushes and closes the write direction.
    ///
    /// # Errors
    ///
    /// Implementation-defined; typically [`TransportError::Io`].
    fn shutdown(&mut self) -> impl Future<Output = TransportResult<()>> + Send;

    /// Applies a negotiated policy.
    fn set_limits(&mut self, limits: FrameLimits);

    /// The current policy.
    fn limits(&self) -> FrameLimits;

    /// Records a flushed frame against the connection counters.
    fn record_sent(&self, wire_bytes: u64, payload_bytes: u64);
}

/// The inbound half of a framed link, as the driver task sees it.
pub trait FrameSource: Send + 'static {
    /// Reads the next frame, or [`None`] at a clean end of stream.
    ///
    /// # Errors
    ///
    /// Implementation-defined; typically [`TransportError::Wire`].
    fn recv(&mut self) -> impl Future<Output = TransportResult<Option<Frame>>> + Send;

    /// Applies a negotiated policy.
    fn set_limits(&mut self, limits: FrameLimits);

    /// The current policy.
    fn limits(&self) -> FrameLimits;
}

impl<W: AsyncWrite + Unpin + Send + 'static> FrameSink for FramedWriter<W> {
    fn queue(&mut self, frame: &Frame) -> TransportResult<usize> {
        FramedWriter::queue_frame(self, frame)
    }

    async fn flush(&mut self) -> TransportResult<()> {
        FramedWriter::flush(self).await
    }

    async fn shutdown(&mut self) -> TransportResult<()> {
        FramedWriter::shutdown(self).await
    }

    fn set_limits(&mut self, limits: FrameLimits) {
        FramedWriter::set_limits(self, limits);
    }

    fn limits(&self) -> FrameLimits {
        *FramedWriter::limits(self)
    }

    fn record_sent(&self, wire_bytes: u64, payload_bytes: u64) {
        FramedWriter::record_sent(self, wire_bytes, payload_bytes);
    }
}

impl<R: AsyncRead + Unpin + Send + 'static> FrameSource for FramedReader<R> {
    async fn recv(&mut self) -> TransportResult<Option<Frame>> {
        FramedReader::recv_frame(self).await
    }

    fn set_limits(&mut self, limits: FrameLimits) {
        FramedReader::set_limits(self, limits);
    }

    fn limits(&self) -> FrameLimits {
        *FramedReader::limits(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{ControlReply, ControlRequest, PeerEvent};
    use tokio::io::AsyncWriteExt;

    fn counters() -> Arc<ConnectionCounters> {
        ConnectionCounters::shared()
    }

    fn pair(
        limits: FrameLimits,
    ) -> (
        FramedStream<tokio::io::DuplexStream>,
        FramedStream<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(1 << 20);
        (
            FramedStream::new(a, limits, counters()),
            FramedStream::new(b, limits, counters()),
        )
    }

    #[tokio::test]
    async fn a_message_survives_the_round_trip() {
        let (mut client, mut server) = pair(FrameLimits::uds());
        client
            .send_message(&ControlRequest::List { all: true })
            .await
            .unwrap();
        let request: ControlRequest = server.expect_message(FrameKind::Control).await.unwrap();
        assert_eq!(request, ControlRequest::List { all: true });

        server.send_message(&ControlReply::Ok).await.unwrap();
        let reply: ControlReply = client
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();
        assert_eq!(reply, ControlReply::Ok);
    }

    #[tokio::test]
    async fn a_raw_payload_keeps_its_compression_flag() {
        let (mut client, mut server) = pair(FrameLimits::network());
        client
            .send_raw(
                FrameKind::PeerEvent,
                Compression::Zstd,
                b"pretend-codec-bytes",
            )
            .await
            .unwrap();
        let frame = server.recv_frame().await.unwrap().expect("a frame");
        assert_eq!(frame.kind(), FrameKind::PeerEvent);
        assert_eq!(frame.flags().compression(), Compression::Zstd);
        assert!(frame.flags().has_crc(), "network legs always checksum");
        assert_eq!(frame.payload(), b"pretend-codec-bytes");
    }

    #[tokio::test]
    async fn a_uds_leg_may_skip_the_checksum() {
        let (mut client, mut server) = pair(FrameLimits::uds());
        client
            .send_raw(FrameKind::Data, Compression::None, b"payload")
            .await
            .unwrap();
        let frame = server.recv_frame().await.unwrap().expect("a frame");
        assert!(!frame.flags().has_crc());
    }

    #[tokio::test]
    async fn an_oversize_send_is_refused_before_it_reaches_the_socket() {
        let limits = FrameLimits::uds().with_max_payload_bytes(64);
        let (mut client, mut server) = pair(limits);

        let err = client
            .send_raw(FrameKind::Data, Compression::None, &[0u8; 65])
            .await
            .unwrap_err();
        match err {
            TransportError::FrameTooLarge { actual, limit } => {
                assert_eq!(actual, 65);
                assert_eq!(limit, 64);
            }
            other => panic!("expected a size refusal, got {other:?}"),
        }

        // The connection is untouched: a legal frame still gets through.
        client
            .send_raw(FrameKind::Data, Compression::None, &[0u8; 64])
            .await
            .unwrap();
        assert_eq!(
            server
                .recv_frame()
                .await
                .unwrap()
                .expect("a frame")
                .payload()
                .len(),
            64
        );
    }

    #[tokio::test]
    async fn an_oversize_receive_is_refused_by_the_reader() {
        // The writer is generous, the reader is strict: exactly the asymmetry
        // a peer that ignores the negotiated ceiling would create.
        let (a, b) = tokio::io::duplex(1 << 20);
        let mut sender = FramedStream::new(a, FrameLimits::uds(), counters());
        let mut receiver =
            FramedStream::new(b, FrameLimits::uds().with_max_payload_bytes(64), counters());

        sender
            .send_raw(FrameKind::Data, Compression::None, &[0u8; 4_096])
            .await
            .unwrap();
        let err = receiver.recv_frame().await.unwrap_err();
        assert!(err.is_fatal(), "an oversize frame must end the connection");
    }

    #[tokio::test]
    async fn a_corrupted_frame_is_a_crc_error_not_a_panic() {
        let limits = FrameLimits::network();
        let (mut raw_sender, receiver_half) = tokio::io::duplex(1 << 20);
        let mut receiver = FramedStream::new(receiver_half, limits, counters());

        // Build a valid frame, then flip a payload bit.
        let mut bytes = astrs_wire::encode_frame(
            FrameKind::Data,
            FrameFlags::CRC,
            b"the quick brown fox",
            &limits,
        )
        .unwrap();
        let midpoint = bytes.len() / 2;
        bytes[midpoint] ^= 0xff;
        raw_sender.write_all(&bytes).await.unwrap();
        raw_sender.flush().await.unwrap();

        let err = receiver.recv_frame().await.unwrap_err();
        match err {
            TransportError::Wire(astrs_wire::WireError::CrcMismatch { .. }) => {}
            other => panic!("expected a checksum failure, got {other:?}"),
        }
        assert!(err.is_fatal());
        assert!(err.is_peer_fault());
    }

    #[tokio::test]
    async fn a_clean_close_reads_as_end_of_stream() {
        let (mut client, mut server) = pair(FrameLimits::uds());
        client.writer_mut().shutdown().await.unwrap();
        assert!(server.recv_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_close_mid_frame_is_a_truncation_not_a_short_read() {
        let limits = FrameLimits::uds();
        let (mut raw_sender, receiver_half) = tokio::io::duplex(1 << 20);
        let mut receiver = FramedStream::new(receiver_half, limits, counters());

        let bytes =
            astrs_wire::encode_frame(FrameKind::Data, FrameFlags::EMPTY, &[7u8; 512], &limits)
                .unwrap();
        raw_sender
            .write_all(&bytes[..bytes.len() / 2])
            .await
            .unwrap();
        raw_sender.shutdown().await.unwrap();
        drop(raw_sender);

        let err = receiver.recv_frame().await.unwrap_err();
        match err {
            TransportError::Wire(astrs_wire::WireError::Truncated { .. }) => {}
            other => panic!("expected a truncation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn expecting_a_message_rejects_the_wrong_family() {
        let (mut client, mut server) = pair(FrameLimits::uds());
        client.send_message(&ControlReply::Ok).await.unwrap();
        let err = server
            .expect_message::<ControlRequest>(FrameKind::Control)
            .await
            .unwrap_err();
        match err {
            TransportError::UnexpectedFrame { expected, found } => {
                assert_eq!(expected, FrameKind::Control);
                assert_eq!(found, FrameKind::ControlReply);
            }
            other => panic!("expected a family mismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn expecting_a_message_on_a_closed_stream_is_an_eof_error() {
        let (client, mut server) = pair(FrameLimits::uds());
        drop(client);
        let err = server
            .expect_message::<ControlRequest>(FrameKind::Control)
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::UnexpectedEof { .. }));
    }

    #[tokio::test]
    async fn limits_widen_symmetrically_after_a_handshake() {
        let narrow = FrameLimits::uds().with_max_payload_bytes(64);
        let (mut client, mut server) = pair(narrow);

        assert!(
            client
                .send_raw(FrameKind::Data, Compression::None, &[0u8; 256])
                .await
                .is_err()
        );

        let wide = FrameLimits::uds().with_max_payload_bytes(4_096);
        client.set_limits(wide);
        server.set_limits(wide);
        assert_eq!(client.limits().max_payload_bytes(), 4_096);

        client
            .send_raw(FrameKind::Data, Compression::None, &[0u8; 256])
            .await
            .unwrap();
        assert_eq!(
            server
                .recv_frame()
                .await
                .unwrap()
                .expect("a frame")
                .payload()
                .len(),
            256
        );
    }

    #[tokio::test]
    async fn widening_the_limits_turns_the_checksum_on() {
        let (mut client, mut server) = pair(FrameLimits::uds());
        client.set_limits(FrameLimits::network());
        client
            .send_raw(FrameKind::Data, Compression::None, b"x")
            .await
            .unwrap();
        // The receiver must widen too, or it would refuse the checksummed
        // frame it now gets — which is exactly why `set_limits` is symmetric.
        server.set_limits(FrameLimits::network());
        let frame = server.recv_frame().await.unwrap().expect("a frame");
        assert!(frame.flags().has_crc());
    }

    #[tokio::test]
    async fn counters_track_both_directions() {
        let counters_a = counters();
        let counters_b = counters();
        let (a, b) = tokio::io::duplex(1 << 20);
        let mut client = FramedStream::new(a, FrameLimits::uds(), Arc::clone(&counters_a));
        let mut server = FramedStream::new(b, FrameLimits::uds(), Arc::clone(&counters_b));

        for _ in 0..4 {
            client
                .send_raw(FrameKind::Data, Compression::None, &[0u8; 100])
                .await
                .unwrap();
        }
        for _ in 0..4 {
            server.recv_frame().await.unwrap().expect("a frame");
        }

        assert_eq!(counters_a.snapshot().frames_sent, 4);
        assert_eq!(counters_b.snapshot().frames_received, 4);
        assert!(counters_a.snapshot().wire_bytes_sent >= 400);
        assert_eq!(counters_b.snapshot().payload_bytes_received, 400);
    }

    #[tokio::test]
    async fn the_halves_run_concurrently() {
        let (a, b) = tokio::io::duplex(1 << 20);
        let client = FramedStream::new(a, FrameLimits::uds(), counters());
        let server = FramedStream::new(b, FrameLimits::uds(), counters());

        let (mut client_reader, mut client_writer) = client.into_halves();
        let (mut server_reader, mut server_writer) = server.into_halves();

        let writer = tokio::spawn(async move {
            for index in 0..64u8 {
                client_writer
                    .send_raw(FrameKind::Data, Compression::None, &[index; 8])
                    .await
                    .unwrap();
            }
            client_writer
        });
        let echoer = tokio::spawn(async move {
            for _ in 0..64 {
                let frame = server_reader.recv_frame().await.unwrap().expect("a frame");
                server_writer.send_frame(&frame).await.unwrap();
            }
        });

        for index in 0..64u8 {
            let frame = client_reader.recv_frame().await.unwrap().expect("a frame");
            assert_eq!(frame.payload(), &[index; 8]);
        }
        writer.await.unwrap();
        echoer.await.unwrap();
    }

    #[tokio::test]
    async fn queued_frames_flush_in_one_batch() {
        let (a, b) = tokio::io::duplex(1 << 20);
        let mut writer = FramedWriter::new(a, FrameLimits::uds(), counters());
        let mut reader = FramedReader::new(b, FrameLimits::uds(), counters());

        for index in 0..8u8 {
            writer
                .queue_raw(FrameKind::Log, Compression::None, &[index])
                .unwrap();
        }
        assert_eq!(writer.frames_written(), 8);
        writer.flush().await.unwrap();

        for index in 0..8u8 {
            let frame = reader.recv_frame().await.unwrap().expect("a frame");
            assert_eq!(frame.payload(), &[index]);
        }
        assert_eq!(reader.frames_read(), 8);
    }

    #[tokio::test]
    async fn a_frame_is_requeued_with_the_connection_checksum_policy() {
        // A frame built on a UDS leg (no checksum) forwarded onto a network
        // leg must acquire one, and keep its compression bits.
        let (a, b) = tokio::io::duplex(1 << 20);
        let mut writer = FramedWriter::new(a, FrameLimits::network(), counters());
        let mut reader = FramedReader::new(b, FrameLimits::network(), counters());

        let built = Frame::new(FrameKind::Data, FrameFlags::LZ4, b"body".to_vec()).unwrap();
        assert!(!built.flags().has_crc());
        writer.send_frame(&built).await.unwrap();

        let received = reader.recv_frame().await.unwrap().expect("a frame");
        assert!(received.flags().has_crc());
        assert_eq!(received.flags().compression(), Compression::Lz4);
        assert_eq!(received.payload(), b"body");
    }

    #[tokio::test]
    async fn the_sink_and_source_traits_drive_the_same_stream() {
        let (a, b) = tokio::io::duplex(1 << 20);
        let mut sink = FramedWriter::new(a, FrameLimits::network(), counters());
        let mut source = FramedReader::new(b, FrameLimits::network(), counters());

        async fn round_trip(
            sink: &mut impl FrameSink,
            source: &mut impl FrameSource,
        ) -> TransportResult<Frame> {
            let frame = Frame::new(FrameKind::PeerEvent, FrameFlags::EMPTY, b"ping".to_vec())
                .map_err(TransportError::Wire)?;
            let bytes = sink.queue(&frame)?;
            sink.flush().await?;
            sink.record_sent(bytes as u64, frame.payload().len() as u64);
            source.recv().await?.ok_or(TransportError::NotConnected)
        }

        let echoed = round_trip(&mut sink, &mut source).await.unwrap();
        assert_eq!(echoed.payload(), b"ping");
        assert_eq!(FrameSink::limits(&sink).max_payload_bytes(), 64 << 20);
        assert_eq!(FrameSource::limits(&source).max_payload_bytes(), 64 << 20);

        FrameSink::set_limits(&mut sink, FrameLimits::uds());
        FrameSource::set_limits(&mut source, FrameLimits::uds());
        assert!(!FrameSink::limits(&sink).require_crc());
        sink.shutdown().await.unwrap();
        assert!(source.recv().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_peer_event_travels_as_a_typed_message() {
        let (mut client, mut server) = pair(FrameLimits::network());
        let ping = PeerEvent::ping(42, astrs_time::HlcTimestamp::new(7, 0));
        client.send_message(&ping).await.unwrap();
        let received: PeerEvent = server.expect_message(FrameKind::PeerEvent).await.unwrap();
        assert_eq!(received, ping);
    }
}
