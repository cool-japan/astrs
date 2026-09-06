//! Async framed I/O over [`tokio::io::AsyncRead`] / [`tokio::io::AsyncWrite`].
//!
//! This is the flavour the daemon, the coordinator and `astrs-transport` use:
//! every real connection in AstRS is a tokio stream (UDS, TCP, or a QUIC stream
//! from oxiquic), and every one of them is framed by this reader and writer.
//!
//! The reassembly engine is the same [`FrameBuffer`] the blocking flavour uses,
//! so the two cannot disagree about what a frame is — the async half only adds
//! `.await` points around the reads and writes.
//!
//! # Cancellation
//!
//! [`AsyncFrameReader::next_frame`] is **cancel-safe at the frame boundary**:
//! bytes already read stay in the buffer, so a `select!` that drops the future
//! part-way through a frame loses nothing and the next call resumes where it
//! left off. [`AsyncFrameWriter::flush`] is not cancel-safe — a dropped flush
//! may have written part of a frame, and the connection has to be considered
//! broken, which is exactly what a half-written frame means anyway.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{AsyncFrameReader, AsyncFrameWriter, FrameLimits, PeerEvent};
//!
//! # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
//! let limits = FrameLimits::uds();
//! let mut wire: Vec<u8> = Vec::new();
//!
//! let mut writer = AsyncFrameWriter::new(&mut wire, limits);
//! writer.send(&PeerEvent::ping(7, Default::default())).await?;
//!
//! let mut reader = AsyncFrameReader::new(wire.as_slice(), limits);
//! let event: PeerEvent = reader.read_message().await?.expect("one event");
//! assert!(matches!(event, PeerEvent::Ping { nonce: 7, .. }));
//! # Ok::<(), astrs_wire::WireError>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::codec::WireEncode;
use crate::error::{WireError, WireResult};
use crate::frame::{Frame, FrameFlags, FrameKind, FrameLimits, FrameView, write_frame};
use crate::io::buffer::{DEFAULT_BUFFER_CAPACITY, FrameBuffer};
use crate::messages::WireMessage;

/// Reads frames from an async byte stream.
///
/// # Examples
///
/// ```
/// use astrs_wire::{AsyncFrameReader, FrameFlags, FrameKind, FrameLimits, encode_frame};
///
/// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
/// let limits = FrameLimits::uds();
/// let bytes = encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"line", &limits)?;
///
/// let mut reader = AsyncFrameReader::new(bytes.as_slice(), limits);
/// let frame = reader.read_frame().await?.expect("one frame");
/// assert_eq!(frame.payload(), b"line");
/// # Ok::<(), astrs_wire::WireError>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct AsyncFrameReader<R> {
    /// The underlying stream.
    inner: R,
    /// The reassembly engine.
    buffer: FrameBuffer,
    /// Whether the stream has reported end-of-file.
    eof: bool,
    /// How many frames have been handed out.
    frames_read: u64,
    /// How many bytes have been pulled off the stream.
    bytes_read: u64,
}

impl<R: AsyncRead + Unpin> AsyncFrameReader<R> {
    /// A reader over `inner` with the default buffer capacity.
    #[must_use]
    pub fn new(inner: R, limits: FrameLimits) -> Self {
        Self::with_capacity(inner, limits, DEFAULT_BUFFER_CAPACITY)
    }

    /// A reader with a chosen initial buffer capacity.
    #[must_use]
    pub fn with_capacity(inner: R, limits: FrameLimits, capacity: usize) -> Self {
        Self {
            inner,
            buffer: FrameBuffer::with_capacity(limits, capacity),
            eof: false,
            frames_read: 0,
            bytes_read: 0,
        }
    }

    /// The policy frames are checked against.
    #[must_use]
    pub const fn limits(&self) -> &FrameLimits {
        self.buffer.limits()
    }

    /// Replaces the policy, as the handshake negotiates it (§7.2).
    pub const fn set_limits(&mut self, limits: FrameLimits) {
        self.buffer.set_limits(limits);
    }

    /// How many complete frames have been read.
    #[must_use]
    pub const fn frames_read(&self) -> u64 {
        self.frames_read
    }

    /// How many bytes have been pulled off the stream.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// How many bytes are buffered but not yet consumed.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.buffer.buffered()
    }

    /// A reference to the underlying stream.
    #[must_use]
    pub const fn get_ref(&self) -> &R {
        &self.inner
    }

    /// A mutable reference to the underlying stream.
    pub const fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Consumes the reader and returns the stream, **discarding** anything
    /// still buffered.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Reads the next frame, borrowed from the reader's own buffer.
    ///
    /// Returns `None` at a clean end of stream.
    ///
    /// # Errors
    ///
    /// - [`WireError::Io`] from the underlying stream.
    /// - [`WireError::Truncated`] if the stream ends part-way through a frame.
    /// - Everything the frame decoder can raise (bad magic, oversize, CRC).
    pub async fn next_frame(&mut self) -> WireResult<Option<FrameView<'_>>> {
        self.buffer.advance();
        while !self.buffer.frame_ready()? {
            if !self.fill().await? {
                if self.buffer.buffered() == 0 {
                    return Ok(None);
                }
                let buffered = self.buffer.buffered();
                return Err(WireError::Truncated {
                    expected: buffered + self.buffer.needed()?,
                    found: buffered,
                });
            }
        }
        self.frames_read += 1;
        self.buffer.next_frame()
    }

    /// Reads the next frame into an owned [`Frame`].
    ///
    /// # Errors
    ///
    /// As [`AsyncFrameReader::next_frame`].
    pub async fn read_frame(&mut self) -> WireResult<Option<Frame>> {
        Ok(self.next_frame().await?.map(|view| view.to_owned_frame()))
    }

    /// Reads and decodes the next frame as `T`, checking the family.
    ///
    /// # Errors
    ///
    /// As [`AsyncFrameReader::next_frame`], plus
    /// [`WireError::KindMismatch`] if the frame belongs to another family.
    pub async fn read_message<T: WireMessage>(&mut self) -> WireResult<Option<T>> {
        match self.next_frame().await? {
            Some(view) => Ok(Some(T::from_frame(&view)?)),
            None => Ok(None),
        }
    }

    /// Pulls one chunk off the stream, returning `false` at end of file.
    ///
    /// # Errors
    ///
    /// [`WireError::Io`] from the underlying stream.
    pub async fn fill(&mut self) -> WireResult<bool> {
        if self.eof {
            return Ok(false);
        }
        let needed = self.buffer.needed()?;
        let spare = self.buffer.spare_mut(needed);
        let read = match self.inner.read(spare).await {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(true),
            Err(err) => return Err(WireError::Io(err)),
        };
        if read == 0 {
            self.eof = true;
            return Ok(false);
        }
        self.buffer.commit(read);
        self.bytes_read += read as u64;
        Ok(true)
    }
}

/// Writes frames to an async byte stream.
///
/// # Examples
///
/// ```
/// use astrs_wire::{AsyncFrameWriter, FrameLimits, PeerEvent};
///
/// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
/// let mut wire: Vec<u8> = Vec::new();
/// let mut writer = AsyncFrameWriter::new(&mut wire, FrameLimits::uds());
///
/// writer.queue(&PeerEvent::ping(1, Default::default()))?;
/// writer.queue(&PeerEvent::ping(2, Default::default()))?;
/// writer.flush().await?;
/// assert_eq!(writer.frames_written(), 2);
/// # Ok::<(), astrs_wire::WireError>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct AsyncFrameWriter<W> {
    /// The underlying stream.
    inner: W,
    /// Frames encoded but not yet written.
    pending: Vec<u8>,
    /// The size policy outgoing frames are checked against.
    limits: FrameLimits,
    /// The flags every frame is written with.
    flags: FrameFlags,
    /// How many frames have been encoded.
    frames_written: u64,
    /// How many bytes have reached the stream.
    bytes_written: u64,
}

impl<W: AsyncWrite + Unpin> AsyncFrameWriter<W> {
    /// A writer over `inner`.
    ///
    /// As with the blocking writer, the flags follow the policy: a connection
    /// that requires a checksum gets one on every frame (§7.1).
    #[must_use]
    pub fn new(inner: W, limits: FrameLimits) -> Self {
        let flags = if limits.require_crc() {
            FrameFlags::CRC
        } else {
            FrameFlags::EMPTY
        };
        Self {
            inner,
            pending: Vec::with_capacity(DEFAULT_BUFFER_CAPACITY),
            limits,
            flags,
            frames_written: 0,
            bytes_written: 0,
        }
    }

    /// Overrides the flags every frame is written with.
    ///
    /// # Errors
    ///
    /// [`WireError::CompressedPayload`] if the flags claim a compression this
    /// crate does not apply (§6.4).
    pub fn set_flags(&mut self, flags: FrameFlags) -> WireResult<()> {
        let compression = flags.compression();
        if compression.is_enabled() {
            return Err(WireError::CompressedPayload { compression });
        }
        self.flags = flags;
        Ok(())
    }

    /// The flags frames are written with.
    #[must_use]
    pub const fn flags(&self) -> FrameFlags {
        self.flags
    }

    /// The policy outgoing frames are checked against.
    #[must_use]
    pub const fn limits(&self) -> &FrameLimits {
        &self.limits
    }

    /// Replaces the policy, as the handshake negotiates it (§7.2).
    pub const fn set_limits(&mut self, limits: FrameLimits) {
        self.limits = limits;
    }

    /// How many frames have been encoded.
    #[must_use]
    pub const fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// How many bytes have reached the stream.
    #[must_use]
    pub const fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// How many bytes are queued but not yet flushed.
    #[must_use]
    pub const fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    /// A reference to the underlying stream.
    #[must_use]
    pub const fn get_ref(&self) -> &W {
        &self.inner
    }

    /// A mutable reference to the underlying stream.
    pub const fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Consumes the writer and returns the stream, **discarding** anything
    /// still queued. Flush first.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Encodes a message into the queue without touching the stream.
    ///
    /// Synchronous on purpose: encoding is CPU work with no await point, and
    /// making it `async` would only invite callers to hold a borrow across one.
    ///
    /// # Errors
    ///
    /// As [`crate::write_message`].
    pub fn queue<T: WireMessage>(&mut self, message: &T) -> WireResult<usize> {
        let written = message.write_frame_into(&mut self.pending, self.flags, &self.limits)?;
        self.frames_written += 1;
        Ok(written)
    }

    /// Encodes an already-serialised payload into the queue.
    ///
    /// # Errors
    ///
    /// As [`write_frame`].
    pub fn queue_frame(
        &mut self,
        kind: FrameKind,
        flags: FrameFlags,
        payload: &[u8],
    ) -> WireResult<usize> {
        let written = write_frame(&mut self.pending, kind, flags, payload, &self.limits)?;
        self.frames_written += 1;
        Ok(written)
    }

    /// Encodes a raw payload with a family, using the writer's own flags.
    ///
    /// # Errors
    ///
    /// As [`crate::write_message`].
    pub fn queue_payload<T: WireEncode>(
        &mut self,
        kind: FrameKind,
        message: &T,
    ) -> WireResult<usize> {
        let written = crate::frame::write_message(
            &mut self.pending,
            kind,
            self.flags,
            message,
            &self.limits,
        )?;
        self.frames_written += 1;
        Ok(written)
    }

    /// Encodes a message and flushes it to the stream.
    ///
    /// # Errors
    ///
    /// As [`AsyncFrameWriter::queue`] and [`AsyncFrameWriter::flush`].
    pub async fn send<T: WireMessage>(&mut self, message: &T) -> WireResult<usize> {
        let written = self.queue(message)?;
        self.flush().await?;
        Ok(written)
    }

    /// Writes everything queued and flushes the underlying stream.
    ///
    /// # Errors
    ///
    /// [`WireError::Io`] from the underlying stream.
    pub async fn flush(&mut self) -> WireResult<()> {
        if self.pending.is_empty() {
            return self.inner.flush().await.map_err(WireError::Io);
        }
        let result = self.inner.write_all(&self.pending).await;
        let queued = self.pending.len();
        self.pending.clear();
        match result {
            Ok(()) => self.bytes_written += queued as u64,
            Err(err) => return Err(WireError::Io(err)),
        }
        self.inner.flush().await.map_err(WireError::Io)
    }

    /// Flushes and then shuts the write half of the stream down.
    ///
    /// # Errors
    ///
    /// [`WireError::Io`] from the underlying stream.
    pub async fn shutdown(&mut self) -> WireResult<()> {
        self.flush().await?;
        self.inner.shutdown().await.map_err(WireError::Io)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::*;
    use crate::frame::encode_frame;
    use crate::messages::samples;
    use crate::messages::{ControlRequest, PeerEvent};

    /// A stream that yields at most `chunk` bytes per poll and reports
    /// `Pending` exactly once, to exercise both the partial-read and the
    /// wake-up paths.
    struct ChunkedStream {
        data: Vec<u8>,
        position: usize,
        chunk: usize,
    }

    impl AsyncRead for ChunkedStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            out: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let remaining = self.data.len() - self.position;
            let take = remaining.min(out.remaining()).min(self.chunk);
            let position = self.position;
            let slice = self
                .data
                .get(position..position + take)
                .unwrap_or_default()
                .to_vec();
            out.put_slice(&slice);
            self.position += take;
            Poll::Ready(Ok(()))
        }
    }

    fn limits() -> FrameLimits {
        FrameLimits::uds()
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        runtime.block_on(future)
    }

    #[test]
    fn a_round_trip_through_an_async_pipe_preserves_every_family() {
        block_on(async {
            let mut wire = Vec::new();
            {
                let mut writer = AsyncFrameWriter::new(&mut wire, limits());
                for event in samples::peer_events().unwrap() {
                    writer.queue(&event).unwrap();
                }
                writer.flush().await.unwrap();
                assert_eq!(writer.frames_written(), 6);
            }

            let mut reader = AsyncFrameReader::new(wire.as_slice(), limits());
            for expected in samples::peer_events().unwrap() {
                let event: PeerEvent = reader.read_message().await.unwrap().expect("a frame");
                assert!(event.bitwise_eq(&expected));
            }
            assert!(reader.read_message::<PeerEvent>().await.unwrap().is_none());
            assert_eq!(reader.frames_read(), 6);
            assert!(reader.bytes_read() > 0);
        });
    }

    #[test]
    fn the_async_and_blocking_writers_produce_identical_bytes() {
        let message = ControlRequest::List { all: true };
        let mut sync_wire = Vec::new();
        let mut sync_writer = crate::io::sync_io::FrameWriter::new(&mut sync_wire, limits());
        sync_writer.send(&message).unwrap();

        let mut async_wire = Vec::new();
        block_on(async {
            let mut writer = AsyncFrameWriter::new(&mut async_wire, limits());
            writer.send(&message).await.unwrap();
        });

        assert_eq!(sync_wire, async_wire);
    }

    #[test]
    fn a_stream_that_dribbles_still_reassembles() {
        block_on(async {
            let mut wire = Vec::new();
            {
                let mut writer = AsyncFrameWriter::new(&mut wire, limits());
                writer
                    .send(&PeerEvent::ping(1, Default::default()))
                    .await
                    .unwrap();
                writer
                    .send(&PeerEvent::ping(2, Default::default()))
                    .await
                    .unwrap();
            }

            let mut reader = AsyncFrameReader::new(
                ChunkedStream {
                    data: wire,
                    position: 0,
                    chunk: 1,
                },
                limits(),
            );
            let first: PeerEvent = reader.read_message().await.unwrap().unwrap();
            let second: PeerEvent = reader.read_message().await.unwrap().unwrap();
            assert!(matches!(first, PeerEvent::Ping { nonce: 1, .. }));
            assert!(matches!(second, PeerEvent::Ping { nonce: 2, .. }));
            assert!(reader.read_message::<PeerEvent>().await.unwrap().is_none());
        });
    }

    #[test]
    fn a_clean_end_of_stream_is_none_and_a_truncated_one_is_an_error() {
        block_on(async {
            let bytes =
                encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"payload", &limits()).unwrap();

            let mut clean = AsyncFrameReader::new(bytes.as_slice(), limits());
            assert!(clean.read_frame().await.unwrap().is_some());
            assert!(clean.read_frame().await.unwrap().is_none());

            let mut truncated = AsyncFrameReader::new(&bytes[..bytes.len() - 2], limits());
            assert!(matches!(
                truncated.read_frame().await,
                Err(WireError::Truncated { .. })
            ));
        });
    }

    #[test]
    fn a_borrowed_frame_avoids_the_copy() {
        block_on(async {
            let bytes =
                encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"zero-copy", &limits()).unwrap();
            let mut reader = AsyncFrameReader::new(bytes.as_slice(), limits());
            {
                let view = reader.next_frame().await.unwrap().expect("a frame");
                assert_eq!(view.payload(), b"zero-copy");
            }
            assert!(reader.next_frame().await.unwrap().is_none());
        });
    }

    #[test]
    fn buffered_bytes_survive_a_dropped_read_future() {
        block_on(async {
            let bytes =
                encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"resumed", &limits()).unwrap();
            let mut reader = AsyncFrameReader::new(
                ChunkedStream {
                    data: bytes,
                    position: 0,
                    chunk: 4,
                },
                limits(),
            );
            // Drive a single fill, then drop it — the bytes it read stay put.
            assert!(reader.fill().await.unwrap());
            let buffered = reader.buffered();
            assert!(buffered > 0);

            let frame = reader.read_frame().await.unwrap().expect("a frame");
            assert_eq!(frame.payload(), b"resumed");
        });
    }

    #[test]
    fn a_corrupt_frame_stops_the_reader_with_a_typed_error() {
        block_on(async {
            let mut bytes =
                encode_frame(FrameKind::Data, FrameFlags::CRC, b"integrity", &limits()).unwrap();
            bytes[12] ^= 0xFF;
            let mut reader = AsyncFrameReader::new(bytes.as_slice(), limits());
            let err = reader.read_frame().await.unwrap_err();
            assert!(err.is_protocol_violation());
        });
    }

    #[test]
    fn a_family_mismatch_is_reported() {
        block_on(async {
            let mut wire = Vec::new();
            {
                let mut writer = AsyncFrameWriter::new(&mut wire, limits());
                writer
                    .send(&PeerEvent::ping(1, Default::default()))
                    .await
                    .unwrap();
            }
            let mut reader = AsyncFrameReader::new(wire.as_slice(), limits());
            assert!(matches!(
                reader.read_message::<ControlRequest>().await,
                Err(WireError::KindMismatch { .. })
            ));
        });
    }

    #[test]
    fn the_writer_refuses_to_claim_a_compression_it_does_not_apply() {
        let mut wire = Vec::new();
        let mut writer = AsyncFrameWriter::new(&mut wire, limits());
        let flags = FrameFlags::EMPTY.with_compression(crate::frame::Compression::Zstd);
        assert!(matches!(
            writer.set_flags(flags),
            Err(WireError::CompressedPayload { .. })
        ));
        assert!(
            writer
                .queue_frame(FrameKind::Data, flags, b"opaque")
                .is_ok()
        );
        assert_eq!(writer.frames_written(), 1);
    }

    #[test]
    fn shutdown_flushes_first() {
        block_on(async {
            let mut wire = Vec::new();
            {
                let mut writer = AsyncFrameWriter::new(&mut wire, limits());
                writer
                    .queue(&PeerEvent::ping(1, Default::default()))
                    .unwrap();
                assert!(writer.pending_bytes() > 0);
                writer.shutdown().await.unwrap();
                assert_eq!(writer.pending_bytes(), 0);
            }
            let mut reader = AsyncFrameReader::new(wire.as_slice(), limits());
            assert!(reader.read_frame().await.unwrap().is_some());
        });
    }

    #[test]
    fn limits_and_accessors_behave_like_the_blocking_flavour() {
        block_on(async {
            let mut wire = Vec::new();
            let mut writer = AsyncFrameWriter::new(&mut wire, FrameLimits::network());
            assert_eq!(writer.flags(), FrameFlags::CRC);
            writer.set_limits(limits().with_max_payload_bytes(4));
            assert_eq!(writer.limits().max_payload_bytes(), 4);
            assert!(writer.get_ref().is_empty());
            writer.get_mut().reserve(1);
            assert!(writer.into_inner().is_empty());

            let mut reader = AsyncFrameReader::new(b"".as_slice(), FrameLimits::default());
            reader.set_limits(limits().with_max_payload_bytes(4));
            assert_eq!(reader.limits().max_payload_bytes(), 4);
            assert_eq!(reader.get_ref().len(), 0);
            let _ = reader.get_mut();
            assert_eq!(reader.into_inner().len(), 0);
        });
    }

    #[test]
    fn a_payload_over_the_limit_is_refused_before_it_is_queued() {
        let tight = limits().with_max_payload_bytes(4);
        let mut wire = Vec::new();
        let mut writer = AsyncFrameWriter::new(&mut wire, tight);
        assert!(matches!(
            writer.queue_frame(FrameKind::Data, FrameFlags::EMPTY, b"far too long"),
            Err(WireError::FrameTooLarge { .. })
        ));
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn raw_payloads_can_be_framed_with_the_writers_own_flags() {
        block_on(async {
            let mut wire = Vec::new();
            {
                let mut writer = AsyncFrameWriter::new(&mut wire, limits());
                writer
                    .queue_payload(FrameKind::Data, &vec![1u8, 2, 3])
                    .unwrap();
                writer.flush().await.unwrap();
            }
            let mut reader = AsyncFrameReader::new(wire.as_slice(), limits());
            let frame = reader.read_frame().await.unwrap().expect("a frame");
            assert_eq!(frame.kind(), FrameKind::Data);
            assert_eq!(frame.decode::<Vec<u8>>().unwrap(), vec![1, 2, 3]);
        });
    }
}
