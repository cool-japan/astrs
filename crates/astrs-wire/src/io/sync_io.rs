//! Blocking framed I/O over [`std::io::Read`] / [`std::io::Write`].
//!
//! The synchronous flavour exists for the places an async runtime is the wrong
//! tool: a CLI that sends one request and waits, a test harness, a recording
//! being replayed from a file, a node that has no runtime of its own. It shares
//! its reassembly engine ([`FrameBuffer`]) with the async flavour, so the two
//! cannot drift apart.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{ControlRequest, FrameLimits, FrameReader, FrameWriter};
//!
//! let limits = FrameLimits::uds();
//! let mut wire: Vec<u8> = Vec::new();
//!
//! let mut writer = FrameWriter::new(&mut wire, limits);
//! writer.send(&ControlRequest::List { all: true })?;
//! writer.flush()?;
//!
//! let mut reader = FrameReader::new(wire.as_slice(), limits);
//! let request: Option<ControlRequest> = reader.read_message()?;
//! assert_eq!(request, Some(ControlRequest::List { all: true }));
//! assert_eq!(reader.read_message::<ControlRequest>()?, None, "clean end of stream");
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

use std::io::{Read, Write};

use crate::codec::WireEncode;
use crate::error::{WireError, WireResult};
use crate::frame::{Frame, FrameFlags, FrameKind, FrameLimits, FrameView, write_frame};
use crate::io::buffer::{DEFAULT_BUFFER_CAPACITY, FrameBuffer};
use crate::messages::WireMessage;

/// Reads frames from a blocking byte stream.
///
/// # End of stream
///
/// A `None` return means the peer closed the connection **between** frames,
/// which is orderly. A close *inside* a frame is
/// [`WireError::Truncated`] — the distinction matters, because the first is a
/// normal shutdown and the second is a fault worth logging.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, FrameReader, encode_frame};
///
/// let limits = FrameLimits::uds();
/// let bytes = encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"line", &limits)?;
///
/// let mut reader = FrameReader::new(bytes.as_slice(), limits);
/// let frame = reader.read_frame()?.expect("one frame");
/// assert_eq!(frame.payload(), b"line");
/// assert_eq!(reader.frames_read(), 1);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
#[derive(Debug)]
pub struct FrameReader<R> {
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

impl<R: Read> FrameReader<R> {
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
    ///
    /// Check [`FrameReader::buffered`] first if that would lose data.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Reads the next frame, borrowed from the reader's own buffer.
    ///
    /// Returns `None` at a clean end of stream. The frame borrows `self`, so it
    /// must be dropped before the next call — which is exactly the property
    /// that makes it copy-free.
    ///
    /// # Errors
    ///
    /// - [`WireError::Io`] from the underlying stream.
    /// - [`WireError::Truncated`] if the stream ends part-way through a frame.
    /// - Everything the frame decoder can raise (bad magic, oversize, CRC).
    pub fn next_frame(&mut self) -> WireResult<Option<FrameView<'_>>> {
        // Release the previous frame, then read until one is complete. The
        // borrow is taken exactly once, after the loop, so the view can outlive
        // the decision that produced it.
        self.buffer.advance();
        while !self.buffer.frame_ready()? {
            if !self.fill()? {
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
    /// As [`FrameReader::next_frame`].
    pub fn read_frame(&mut self) -> WireResult<Option<Frame>> {
        Ok(self.next_frame()?.map(|view| view.to_owned_frame()))
    }

    /// Reads and decodes the next frame as `T`, checking the family.
    ///
    /// # Errors
    ///
    /// As [`FrameReader::next_frame`], plus
    /// [`WireError::KindMismatch`] if the frame belongs to another family and
    /// the codec errors if the payload does not decode.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameLimits, FrameReader, FrameWriter, PeerEvent};
    ///
    /// let limits = FrameLimits::uds();
    /// let mut wire = Vec::new();
    /// let mut writer = FrameWriter::new(&mut wire, limits);
    /// writer.send(&PeerEvent::ping(7, Default::default()))?;
    /// writer.flush()?;
    ///
    /// let mut reader = FrameReader::new(wire.as_slice(), limits);
    /// let event: PeerEvent = reader.read_message()?.expect("one event");
    /// assert!(matches!(event, PeerEvent::Ping { nonce: 7, .. }));
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn read_message<T: WireMessage>(&mut self) -> WireResult<Option<T>> {
        match self.next_frame()? {
            Some(view) => Ok(Some(T::from_frame(&view)?)),
            None => Ok(None),
        }
    }

    /// Pulls one chunk off the stream, returning `false` at end of file.
    ///
    /// Exposed because a caller draining a socket by hand — a transport that
    /// multiplexes several readers, say — needs to drive the fill itself.
    ///
    /// # Errors
    ///
    /// [`WireError::Io`] from the underlying stream.
    pub fn fill(&mut self) -> WireResult<bool> {
        if self.eof {
            return Ok(false);
        }
        let needed = self.buffer.needed()?;
        let spare = self.buffer.spare_mut(needed);
        let read = match self.inner.read(spare) {
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

/// Writes frames to a blocking byte stream.
///
/// Frames are encoded into an internal buffer and written out on
/// [`FrameWriter::flush`], so a caller can batch several messages into one
/// syscall with [`FrameWriter::queue`] — or send one at a time with
/// [`FrameWriter::send`], which flushes for you.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameLimits, FrameWriter, PeerEvent};
///
/// let limits = FrameLimits::uds();
/// let mut wire = Vec::new();
/// let mut writer = FrameWriter::new(&mut wire, limits);
///
/// writer.queue(&PeerEvent::ping(1, Default::default()))?;
/// writer.queue(&PeerEvent::ping(2, Default::default()))?;
/// assert_eq!(writer.queued(), writer.pending_bytes());
/// writer.flush()?;
///
/// assert_eq!(writer.frames_written(), 2);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
#[derive(Debug)]
pub struct FrameWriter<W> {
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
    /// How many bytes have been written to the stream.
    bytes_written: u64,
}

impl<W: Write> FrameWriter<W> {
    /// A writer over `inner`.
    ///
    /// The frame flags follow the policy: a connection that requires a
    /// checksum gets one on every frame (§7.1).
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
    /// crate does not apply — compression belongs to `astrs-transport` (§6.4),
    /// and a writer that stamped the flag without compressing would produce
    /// frames no peer could read.
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

    /// How many frames are queued but not yet flushed.
    #[must_use]
    pub const fn queued(&self) -> usize {
        self.pending.len()
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

    /// Encodes a message into the queue without writing to the stream.
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
    /// This is what a transport uses for a payload it compressed itself: pass
    /// the compression flag here rather than through
    /// [`FrameWriter::set_flags`], since only this frame is compressed.
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
    /// As [`FrameWriter::queue`] and [`FrameWriter::flush`].
    pub fn send<T: WireMessage>(&mut self, message: &T) -> WireResult<usize> {
        let written = self.queue(message)?;
        self.flush()?;
        Ok(written)
    }

    /// Writes everything queued and flushes the underlying stream.
    ///
    /// # Errors
    ///
    /// [`WireError::Io`] from the underlying stream. Bytes that were written
    /// before the failure are dropped from the queue, so a retry does not
    /// duplicate them.
    pub fn flush(&mut self) -> WireResult<()> {
        if self.pending.is_empty() {
            return self.inner.flush().map_err(WireError::Io);
        }
        match self.inner.write_all(&self.pending) {
            Ok(()) => {
                self.bytes_written += self.pending.len() as u64;
                self.pending.clear();
            }
            Err(err) => {
                // `write_all` gives no partial count, so the queue is dropped
                // wholesale: a half-written frame cannot be repaired by
                // resending it, and the connection is finished either way.
                self.pending.clear();
                return Err(WireError::Io(err));
            }
        }
        self.inner.flush().map_err(WireError::Io)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io::{self, ErrorKind};

    use super::*;
    use crate::frame::encode_frame;
    use crate::messages::PeerEvent;
    use crate::messages::samples;

    /// A reader that yields at most `chunk` bytes per call, to exercise the
    /// reassembly path a real socket forces on us.
    struct ChunkedReader {
        data: Vec<u8>,
        position: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let remaining = self.data.len() - self.position;
            let take = remaining.min(out.len()).min(self.chunk);
            out[..take].copy_from_slice(&self.data[self.position..self.position + take]);
            self.position += take;
            Ok(take)
        }
    }

    /// A writer that fails after `budget` bytes.
    struct FailingWriter {
        budget: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if data.len() > self.budget {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "no more room"));
            }
            self.budget -= data.len();
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn limits() -> FrameLimits {
        FrameLimits::uds()
    }

    #[test]
    fn a_round_trip_through_a_pipe_preserves_every_family() {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, limits());
        for event in samples::peer_events().unwrap() {
            writer.queue(&event).unwrap();
        }
        writer.flush().unwrap();
        assert_eq!(writer.frames_written(), 6);
        assert!(writer.bytes_written() > 0);

        let mut reader = FrameReader::new(wire.as_slice(), limits());
        for expected in samples::peer_events().unwrap() {
            let event: PeerEvent = reader.read_message().unwrap().expect("a frame");
            assert!(event.bitwise_eq(&expected));
        }
        assert_eq!(reader.read_message::<PeerEvent>().unwrap(), None);
        assert_eq!(reader.frames_read(), 6);
    }

    #[test]
    fn a_stream_that_dribbles_one_byte_at_a_time_still_reassembles() {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, limits());
        writer
            .send(&PeerEvent::ping(1, Default::default()))
            .unwrap();
        writer
            .send(&PeerEvent::ping(2, Default::default()))
            .unwrap();

        let mut reader = FrameReader::new(
            ChunkedReader {
                data: wire,
                position: 0,
                chunk: 1,
            },
            limits(),
        );
        let first: PeerEvent = reader.read_message().unwrap().unwrap();
        let second: PeerEvent = reader.read_message().unwrap().unwrap();
        assert!(matches!(first, PeerEvent::Ping { nonce: 1, .. }));
        assert!(matches!(second, PeerEvent::Ping { nonce: 2, .. }));
        assert!(reader.read_message::<PeerEvent>().unwrap().is_none());
    }

    #[test]
    fn a_clean_end_of_stream_is_none_and_a_truncated_one_is_an_error() {
        let bytes =
            encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"payload", &limits()).unwrap();

        let mut clean = FrameReader::new(bytes.as_slice(), limits());
        assert!(clean.read_frame().unwrap().is_some());
        assert!(clean.read_frame().unwrap().is_none());

        let mut truncated = FrameReader::new(&bytes[..bytes.len() - 2], limits());
        assert!(matches!(
            truncated.read_frame(),
            Err(WireError::Truncated { .. })
        ));
    }

    #[test]
    fn a_borrowed_frame_avoids_the_copy() {
        let bytes =
            encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"zero-copy", &limits()).unwrap();
        let mut reader = FrameReader::new(bytes.as_slice(), limits());
        {
            let view = reader.next_frame().unwrap().expect("a frame");
            assert_eq!(view.payload(), b"zero-copy");
        }
        assert!(reader.next_frame().unwrap().is_none());
    }

    #[test]
    fn a_family_mismatch_is_reported() {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, limits());
        writer
            .send(&PeerEvent::ping(1, Default::default()))
            .unwrap();

        let mut reader = FrameReader::new(wire.as_slice(), limits());
        assert!(matches!(
            reader.read_message::<crate::messages::ControlRequest>(),
            Err(WireError::KindMismatch { .. })
        ));
    }

    #[test]
    fn a_corrupt_frame_stops_the_reader_with_a_typed_error() {
        let mut bytes =
            encode_frame(FrameKind::Data, FrameFlags::CRC, b"integrity", &limits()).unwrap();
        bytes[12] ^= 0xFF;
        let mut reader = FrameReader::new(bytes.as_slice(), limits());
        let err = reader.read_frame().unwrap_err();
        assert!(err.is_protocol_violation());
    }

    #[test]
    fn garbage_is_refused_rather_than_resynchronised() {
        // Deliberate: a stream that lost sync cannot be trusted, so the reader
        // reports the error instead of hunting for the next plausible magic.
        let mut reader = FrameReader::new(b"not a frame at all".as_slice(), limits());
        assert!(matches!(
            reader.read_frame(),
            Err(WireError::BadMagic { .. })
        ));
    }

    #[test]
    fn the_writer_stamps_a_checksum_when_the_policy_requires_one() {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, FrameLimits::network());
        assert_eq!(writer.flags(), FrameFlags::CRC);
        writer
            .send(&PeerEvent::ping(1, Default::default()))
            .unwrap();

        let mut reader = FrameReader::new(wire.as_slice(), FrameLimits::network());
        assert!(reader.read_frame().unwrap().is_some());
    }

    #[test]
    fn the_writer_refuses_to_claim_a_compression_it_does_not_apply() {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, limits());
        let flags = FrameFlags::EMPTY.with_compression(crate::frame::Compression::Zstd);
        assert!(matches!(
            writer.set_flags(flags),
            Err(WireError::CompressedPayload { .. })
        ));
        assert_eq!(writer.flags(), FrameFlags::EMPTY);
        // But a pre-compressed payload may be framed explicitly.
        assert!(
            writer
                .queue_frame(FrameKind::Data, flags, b"opaque")
                .is_ok()
        );
    }

    #[test]
    fn queueing_batches_several_frames_into_one_write() {
        let mut wire = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut wire, limits());
            writer
                .queue(&PeerEvent::ping(1, Default::default()))
                .unwrap();
            writer
                .queue(&PeerEvent::ping(2, Default::default()))
                .unwrap();
            assert!(writer.pending_bytes() > 0);
            assert_eq!(writer.bytes_written(), 0, "nothing is written until flush");
            writer.flush().unwrap();
            assert_eq!(writer.pending_bytes(), 0);
        }
        let mut reader = FrameReader::new(wire.as_slice(), limits());
        assert!(reader.read_frame().unwrap().is_some());
        assert!(reader.read_frame().unwrap().is_some());
    }

    #[test]
    fn a_write_failure_surfaces_and_clears_the_queue() {
        let mut writer = FrameWriter::new(FailingWriter { budget: 4 }, limits());
        writer
            .queue(&PeerEvent::ping(1, Default::default()))
            .unwrap();
        assert!(matches!(writer.flush(), Err(WireError::Io(_))));
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn a_payload_over_the_limit_is_refused_before_it_is_queued() {
        let tight = limits().with_max_payload_bytes(4);
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, tight);
        let err = writer
            .queue_frame(FrameKind::Data, FrameFlags::EMPTY, b"far too long")
            .unwrap_err();
        assert!(matches!(err, WireError::FrameTooLarge { .. }));
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn limits_can_be_tightened_after_the_handshake_on_both_halves() {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, FrameLimits::default());
        writer.set_limits(limits().with_max_payload_bytes(4));
        assert_eq!(writer.limits().max_payload_bytes(), 4);

        let mut reader = FrameReader::new(b"".as_slice(), FrameLimits::default());
        reader.set_limits(limits().with_max_payload_bytes(4));
        assert_eq!(reader.limits().max_payload_bytes(), 4);
    }

    #[test]
    fn accessors_reach_the_underlying_streams() {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, limits());
        writer
            .send(&PeerEvent::ping(1, Default::default()))
            .unwrap();
        assert!(!writer.get_ref().is_empty());
        writer.get_mut().push(0);
        let recovered = writer.into_inner();
        assert_eq!(recovered.last(), Some(&0));

        let reader = FrameReader::new(b"abc".as_slice(), limits());
        assert_eq!(reader.get_ref().len(), 3);
        assert_eq!(reader.buffered(), 0);
        assert_eq!(reader.bytes_read(), 0);
        assert_eq!(reader.into_inner().len(), 3);
    }

    #[test]
    fn an_interrupted_read_is_retried_not_reported() {
        struct Interrupting {
            interrupted: bool,
            data: Vec<u8>,
            position: usize,
        }

        impl Read for Interrupting {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::new(ErrorKind::Interrupted, "signal"));
                }
                let take = (self.data.len() - self.position).min(out.len());
                out[..take].copy_from_slice(&self.data[self.position..self.position + take]);
                self.position += take;
                Ok(take)
            }
        }

        let bytes = encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"ok", &limits()).unwrap();
        let mut reader = FrameReader::new(
            Interrupting {
                interrupted: false,
                data: bytes,
                position: 0,
            },
            limits(),
        );
        assert_eq!(
            reader
                .read_frame()
                .unwrap()
                .map(|frame| frame.payload().to_vec()),
            Some(b"ok".to_vec())
        );
    }
}
