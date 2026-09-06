//! [`FrameBuffer`]: the stream-reassembly engine both framed readers share.
//!
//! A stream transport hands over arbitrary byte runs: half a frame, three
//! frames, a frame split across four reads. Turning that back into frames is
//! the same problem for a blocking socket, a tokio socket and a QUIC stream, so
//! it is solved once here and the readers are thin shells around it.
//!
//! # What makes it fast
//!
//! - **Exact sizing.** [`FrameBuffer::needed`] reads the header's `len` field
//!   and says precisely how many bytes are still missing, so a reader never
//!   over-allocates and never has to guess a chunk size.
//! - **No re-decoding.** A frame is parsed once; [`FrameBuffer::next_frame`]
//!   hands out a borrowed [`FrameView`] into the buffer rather than a copy.
//! - **Compaction, not shifting.** Consumed bytes are dropped by advancing a
//!   cursor; the buffer is only memmoved when the tail actually needs the room.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{FrameBuffer, FrameFlags, FrameKind, FrameLimits, encode_frame};
//!
//! let limits = FrameLimits::uds();
//! let bytes = encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"hello", &limits)?;
//!
//! let mut buffer = FrameBuffer::new(limits);
//! // Feed it one byte at a time: nothing decodes until the frame is complete.
//! for (index, byte) in bytes.iter().enumerate() {
//!     buffer.push(&[*byte]);
//!     let complete = buffer.next_frame()?.is_some();
//!     assert_eq!(complete, index + 1 == bytes.len());
//! }
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

use crate::error::{WireError, WireResult};
use crate::frame::{Frame, FrameHeader, FrameLimits, FrameView, HEADER_LEN, decode_frame_prefix};

/// The default read buffer size: enough for a burst of control frames without
/// reserving space for a payload most connections never send.
pub const DEFAULT_BUFFER_CAPACITY: usize = 64 * 1024;

/// A growable buffer that turns a byte stream back into frames.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameBuffer, FrameFlags, FrameKind, FrameLimits, encode_frame};
///
/// let limits = FrameLimits::uds();
/// let mut buffer = FrameBuffer::new(limits);
/// buffer.push(&encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"one", &limits)?);
/// buffer.push(&encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"two", &limits)?);
///
/// assert_eq!(buffer.next_frame()?.map(|frame| frame.payload().to_vec()), Some(b"one".to_vec()));
/// assert_eq!(buffer.next_frame()?.map(|frame| frame.payload().to_vec()), Some(b"two".to_vec()));
/// assert!(buffer.next_frame()?.is_none());
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
#[derive(Debug)]
pub struct FrameBuffer {
    /// The storage. Always fully initialised — `len() == capacity` — so a
    /// reader can borrow the spare region as `&mut [u8]` without `unsafe` and
    /// without re-zeroing it on every read.
    storage: Vec<u8>,
    /// The first unconsumed byte.
    start: usize,
    /// One past the last byte received.
    end: usize,
    /// The length of the frame handed out by the last [`FrameBuffer::next_frame`],
    /// consumed at the start of the next call.
    pending: usize,
    /// The size and integrity policy frames are checked against.
    limits: FrameLimits,
}

impl FrameBuffer {
    /// An empty buffer with the default capacity.
    #[must_use]
    pub fn new(limits: FrameLimits) -> Self {
        Self::with_capacity(limits, DEFAULT_BUFFER_CAPACITY)
    }

    /// An empty buffer with a chosen capacity.
    ///
    /// The capacity is a starting point, not a cap: a frame larger than it
    /// grows the buffer (bounded by `limits`), and a connection that only ever
    /// sends small frames never pays for the growth.
    #[must_use]
    pub fn with_capacity(limits: FrameLimits, capacity: usize) -> Self {
        Self {
            storage: vec![0; capacity.max(HEADER_LEN)],
            start: 0,
            end: 0,
            pending: 0,
            limits,
        }
    }

    /// The policy frames are checked against.
    #[must_use]
    pub const fn limits(&self) -> &FrameLimits {
        &self.limits
    }

    /// Replaces the policy — what a connection does once the handshake has
    /// negotiated its limits (§7.2).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameBuffer, FrameLimits, NegotiatedLimits};
    ///
    /// let mut buffer = FrameBuffer::new(FrameLimits::default());
    /// let agreed = NegotiatedLimits::network().with_max_payload_bytes(1 << 20);
    /// buffer.set_limits(agreed.to_frame_limits());
    /// assert_eq!(buffer.limits().max_payload_bytes(), 1 << 20);
    /// ```
    pub const fn set_limits(&mut self, limits: FrameLimits) {
        self.limits = limits;
    }

    /// How many unconsumed bytes are buffered.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.end - self.start
    }

    /// Whether nothing is buffered.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buffered() == 0
    }

    /// The buffer's current capacity in bytes.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    /// Drops everything buffered.
    pub const fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
        self.pending = 0;
    }

    /// The unconsumed bytes.
    #[must_use]
    pub fn filled(&self) -> &[u8] {
        self.storage.get(self.start..self.end).unwrap_or(&[])
    }

    /// How many more bytes are needed to complete the next frame.
    ///
    /// Returns at least one: a reader that has a complete frame buffered
    /// should decode it rather than read more, and this is only consulted when
    /// [`FrameBuffer::next_frame`] returned `None`.
    ///
    /// # Errors
    ///
    /// [`WireError::FrameTooLarge`] and the header errors, when the header is
    /// already complete and invalid.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameBuffer, FrameFlags, FrameKind, FrameLimits, encode_frame};
    ///
    /// let limits = FrameLimits::uds();
    /// let bytes = encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"payload", &limits)?;
    ///
    /// let mut buffer = FrameBuffer::new(limits);
    /// assert_eq!(buffer.needed()?, 10, "an empty buffer needs a header first");
    ///
    /// buffer.push(&bytes[..10]);
    /// assert_eq!(buffer.needed()?, 7, "the header says how much payload follows");
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn needed(&self) -> WireResult<usize> {
        let buffered = self.buffered();
        if buffered < HEADER_LEN {
            return Ok(HEADER_LEN - buffered);
        }
        let header = FrameHeader::parse(self.filled())?;
        let payload_len = header.payload_len_usize()?;
        self.limits.check_payload_len(payload_len)?;
        Ok(header.total_len().saturating_sub(buffered).max(1))
    }

    /// Appends received bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameBuffer, FrameLimits};
    ///
    /// let mut buffer = FrameBuffer::new(FrameLimits::uds());
    /// buffer.push(b"partial");
    /// assert_eq!(buffer.buffered(), 7);
    /// ```
    pub fn push(&mut self, bytes: &[u8]) {
        self.reserve(bytes.len());
        let end = self.end;
        if let Some(slot) = self.storage.get_mut(end..end + bytes.len()) {
            slot.copy_from_slice(bytes);
            self.end += bytes.len();
        }
    }

    /// Makes room for at least `additional` more bytes, compacting first and
    /// growing only if compaction is not enough.
    pub fn reserve(&mut self, additional: usize) {
        if self.storage.len() - self.end >= additional {
            return;
        }
        self.compact();
        let required = self.end + additional;
        if self.storage.len() < required {
            // Grow geometrically so a stream of large frames does not
            // reallocate on every one.
            let target = required.max(self.storage.len().saturating_mul(2));
            self.storage.resize(target, 0);
        }
    }

    /// The writable region a reader fills, sized for the next frame.
    ///
    /// The slice is at least `at_least` bytes long, and the reader reports how
    /// much it actually wrote with [`FrameBuffer::commit`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameBuffer, FrameLimits};
    ///
    /// let mut buffer = FrameBuffer::new(FrameLimits::uds());
    /// let spare = buffer.spare_mut(4);
    /// spare[..4].copy_from_slice(b"data");
    /// buffer.commit(4);
    /// assert_eq!(buffer.filled(), b"data");
    /// ```
    pub fn spare_mut(&mut self, at_least: usize) -> &mut [u8] {
        self.reserve(at_least.max(1));
        let end = self.end;
        self.storage.get_mut(end..).unwrap_or(&mut [])
    }

    /// Records that `written` bytes were placed in the region returned by
    /// [`FrameBuffer::spare_mut`].
    ///
    /// Silently clamps to the buffer's capacity rather than panicking, so a
    /// transport that miscounts corrupts nothing.
    pub const fn commit(&mut self, written: usize) {
        let available = self.storage.len() - self.end;
        self.end += if written > available {
            available
        } else {
            written
        };
    }

    /// Releases the frame handed out by the previous
    /// [`FrameBuffer::next_frame`], so the bytes behind it can be reused.
    ///
    /// [`FrameBuffer::next_frame`] does this for you; it is public because a
    /// reader that has to decide *whether* to read more —
    /// [`FrameBuffer::frame_ready`] — must release the previous frame first,
    /// and doing so through a borrow of the view would be impossible.
    pub const fn advance(&mut self) {
        if self.pending == 0 {
            return;
        }
        self.start += self.pending;
        self.pending = 0;
        if self.start == self.end {
            // The common case: the buffer drained exactly. Reset both cursors
            // so the next frame starts at offset zero and never needs a memmove.
            self.start = 0;
            self.end = 0;
        }
    }

    /// Whether a complete, well-formed frame is buffered — without borrowing
    /// it.
    ///
    /// Call [`FrameBuffer::advance`] first if a previous frame is still
    /// outstanding.
    ///
    /// # Errors
    ///
    /// The header errors, [`WireError::FrameTooLarge`] and
    /// [`WireError::MissingCrc`] — everything that can be decided before the
    /// payload has fully arrived, so a reader learns about a malformed frame
    /// instead of waiting forever for bytes that will never be valid.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameBuffer, FrameFlags, FrameKind, FrameLimits, encode_frame};
    ///
    /// let limits = FrameLimits::uds();
    /// let bytes = encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"x", &limits)?;
    /// let mut buffer = FrameBuffer::new(limits);
    ///
    /// buffer.push(&bytes[..5]);
    /// assert!(!buffer.frame_ready()?);
    /// buffer.push(&bytes[5..]);
    /// assert!(buffer.frame_ready()?);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn frame_ready(&self) -> WireResult<bool> {
        let buffered = self.buffered();
        if buffered < HEADER_LEN {
            return Ok(false);
        }
        let header = FrameHeader::parse(self.filled())?;
        let payload_len = header.payload_len_usize()?;
        self.limits.check_payload_len(payload_len)?;
        if self.limits.require_crc() && !header.has_crc() {
            return Err(WireError::MissingCrc);
        }
        Ok(buffered >= header.total_len())
    }

    /// The next complete frame, or `None` when more bytes are needed.
    ///
    /// The frame borrows the buffer; it is released at the start of the
    /// *following* call (or by [`FrameBuffer::advance`]), which is what lets a
    /// reader hand out a payload with no copy at all.
    ///
    /// # Errors
    ///
    /// Everything [`decode_frame_prefix`] can raise except
    /// [`WireError::Truncated`], which is reported as `None` — an incomplete
    /// frame is the normal state of a stream reader, not an error.
    pub fn next_frame(&mut self) -> WireResult<Option<FrameView<'_>>> {
        self.advance();
        if !self.frame_ready()? {
            return Ok(None);
        }
        let buffered = self.buffered();
        let total = FrameHeader::parse(self.filled())?.total_len();
        self.pending = total;
        let bytes =
            self.storage
                .get(self.start..self.start + total)
                .ok_or(WireError::Truncated {
                    expected: total,
                    found: buffered,
                })?;
        decode_frame_prefix(bytes, &self.limits).map(Some)
    }

    /// The next complete frame, copied out of the buffer.
    ///
    /// For callers that need to keep the frame past the next read — a queue, a
    /// recording — where the borrowed form of [`FrameBuffer::next_frame`] would
    /// not live long enough.
    ///
    /// # Errors
    ///
    /// As [`FrameBuffer::next_frame`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameBuffer, FrameFlags, FrameKind, FrameLimits, encode_frame};
    ///
    /// let limits = FrameLimits::uds();
    /// let mut buffer = FrameBuffer::new(limits);
    /// buffer.push(&encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"x", &limits)?);
    ///
    /// let frame = buffer.next_owned_frame()?.expect("one complete frame");
    /// assert_eq!(frame.payload(), b"x");
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn next_owned_frame(&mut self) -> WireResult<Option<Frame>> {
        Ok(self.next_frame()?.map(|view| view.to_owned_frame()))
    }

    /// Moves the unconsumed bytes to the front of the storage.
    fn compact(&mut self) {
        if self.start == 0 {
            return;
        }
        let buffered = self.buffered();
        if buffered > 0 {
            self.storage.copy_within(self.start..self.end, 0);
        }
        self.start = 0;
        self.end = buffered;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::frame::{FrameFlags, FrameKind, encode_frame};

    fn frame(payload: &[u8]) -> Vec<u8> {
        encode_frame(
            FrameKind::Data,
            FrameFlags::CRC,
            payload,
            &FrameLimits::uds(),
        )
        .unwrap()
    }

    #[test]
    fn an_empty_buffer_yields_nothing() {
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        assert!(buffer.is_empty());
        assert!(buffer.next_frame().unwrap().is_none());
        assert_eq!(buffer.needed().unwrap(), HEADER_LEN);
    }

    #[test]
    fn a_frame_split_byte_by_byte_reassembles() {
        let bytes = frame(b"hello world");
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        for (index, byte) in bytes.iter().enumerate() {
            buffer.push(&[*byte]);
            let complete = buffer.next_frame().unwrap().is_some();
            assert_eq!(complete, index + 1 == bytes.len(), "at byte {index}");
        }
    }

    #[test]
    fn needed_shrinks_as_bytes_arrive() {
        let bytes = frame(b"payload");
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        assert_eq!(buffer.needed().unwrap(), HEADER_LEN);

        buffer.push(&bytes[..HEADER_LEN]);
        assert_eq!(buffer.needed().unwrap(), bytes.len() - HEADER_LEN);

        buffer.push(&bytes[HEADER_LEN..bytes.len() - 1]);
        assert_eq!(buffer.needed().unwrap(), 1);
    }

    #[test]
    fn several_frames_in_one_push_come_out_in_order() {
        let mut stream = Vec::new();
        for payload in [b"one".as_slice(), b"two", b"three"] {
            stream.extend_from_slice(&frame(payload));
        }
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        buffer.push(&stream);

        for expected in [b"one".as_slice(), b"two", b"three"] {
            let view = buffer.next_frame().unwrap().expect("a complete frame");
            assert_eq!(view.payload(), expected);
        }
        assert!(buffer.next_frame().unwrap().is_none());
        assert!(buffer.is_empty(), "a drained buffer resets its cursors");
    }

    #[test]
    fn owned_frames_outlive_the_next_read() {
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        buffer.push(&frame(b"first"));
        let owned = buffer.next_owned_frame().unwrap().expect("complete");
        buffer.push(&frame(b"second"));
        let second = buffer.next_owned_frame().unwrap().expect("complete");
        assert_eq!(owned.payload(), b"first");
        assert_eq!(second.payload(), b"second");
    }

    #[test]
    fn the_spare_region_is_writable_and_committed() {
        let bytes = frame(b"direct");
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        let spare = buffer.spare_mut(bytes.len());
        assert!(spare.len() >= bytes.len());
        spare[..bytes.len()].copy_from_slice(&bytes);
        buffer.commit(bytes.len());
        assert_eq!(
            buffer
                .next_frame()
                .unwrap()
                .map(|view| view.payload().to_vec()),
            Some(b"direct".to_vec())
        );
    }

    #[test]
    fn committing_more_than_the_capacity_is_clamped_not_fatal() {
        let mut buffer = FrameBuffer::with_capacity(FrameLimits::uds(), 16);
        buffer.commit(1_000_000);
        assert!(buffer.buffered() <= buffer.capacity());
    }

    #[test]
    fn the_buffer_grows_for_a_frame_larger_than_its_capacity() {
        let payload = vec![0x5A; 100_000];
        let bytes = frame(&payload);
        let mut buffer = FrameBuffer::with_capacity(FrameLimits::uds(), 64);
        buffer.push(&bytes);
        let view = buffer.next_frame().unwrap().expect("complete");
        assert_eq!(view.payload().len(), payload.len());
        assert!(buffer.capacity() >= bytes.len());
    }

    #[test]
    fn compaction_keeps_a_partial_frame_intact() {
        // Fill the buffer with one frame plus a partial second, then force a
        // compaction by reserving beyond the remaining tail space.
        let first = frame(b"first frame");
        let second = frame(b"second frame");
        let mut buffer = FrameBuffer::with_capacity(FrameLimits::uds(), first.len() + 4);
        buffer.push(&first);
        buffer.push(&second[..4]);
        assert!(buffer.next_frame().unwrap().is_some());
        buffer.push(&second[4..]);
        let view = buffer.next_frame().unwrap().expect("complete");
        assert_eq!(view.payload(), b"second frame");
    }

    #[test]
    fn a_corrupt_header_is_an_error_not_a_stall() {
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        buffer.push(b"NOTASTRSFRAME");
        assert!(matches!(
            buffer.next_frame(),
            Err(WireError::BadMagic { .. })
        ));
    }

    #[test]
    fn an_oversize_declared_length_is_refused_before_it_is_buffered() {
        let tight = FrameLimits::uds().with_max_payload_bytes(8);
        let header = crate::frame::FrameHeader::new(FrameKind::Data, FrameFlags::EMPTY, u32::MAX);
        let mut buffer = FrameBuffer::new(tight);
        buffer.push(&header.to_bytes());
        assert!(matches!(
            buffer.next_frame(),
            Err(WireError::FrameTooLarge { .. })
        ));
        assert!(matches!(
            buffer.needed(),
            Err(WireError::FrameTooLarge { .. })
        ));
        assert!(
            buffer.capacity() < 1 << 20,
            "the buffer must not have grown to the forged length"
        );
    }

    #[test]
    fn a_missing_checksum_is_refused_when_the_policy_requires_one() {
        let bytes = encode_frame(
            FrameKind::Data,
            FrameFlags::EMPTY,
            b"x",
            &FrameLimits::uds(),
        )
        .unwrap();
        let mut buffer = FrameBuffer::new(FrameLimits::network());
        buffer.push(&bytes);
        assert!(matches!(buffer.next_frame(), Err(WireError::MissingCrc)));
    }

    #[test]
    fn a_corrupt_payload_is_caught_by_the_checksum() {
        let mut bytes = frame(b"integrity");
        let last = bytes.len() - 5;
        bytes[last] ^= 0xFF;
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        buffer.push(&bytes);
        assert!(matches!(
            buffer.next_frame(),
            Err(WireError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn limits_can_be_tightened_after_the_handshake() {
        let mut buffer = FrameBuffer::new(FrameLimits::default());
        buffer.set_limits(FrameLimits::uds().with_max_payload_bytes(4));
        assert_eq!(buffer.limits().max_payload_bytes(), 4);
        buffer.push(&frame(b"too long"));
        assert!(matches!(
            buffer.next_frame(),
            Err(WireError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn clearing_drops_everything() {
        let mut buffer = FrameBuffer::new(FrameLimits::uds());
        buffer.push(&frame(b"x"));
        buffer.clear();
        assert!(buffer.is_empty());
        assert!(buffer.next_frame().unwrap().is_none());
    }
}
