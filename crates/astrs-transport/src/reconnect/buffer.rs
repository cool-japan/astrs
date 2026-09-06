//! The bounded in-flight buffer that survives a reconnect (blueprint §12).
//!
//! A daemon that loses its coordinator keeps producing heartbeats, node
//! metrics and spawn results. Dropping them all is wrong — the coordinator will
//! come back in a second and needs to know what happened — and keeping them all
//! is worse, because a coordinator that stays down for an hour must not take
//! the daemon's memory with it.
//!
//! [`ReconnectBuffer`] is the middle: a fixed number of frames, and an explicit
//! policy for what happens at the boundary.
//!
//! | Policy | Behaviour at the boundary | Right for |
//! |---|---|---|
//! | [`OverflowPolicy::Reject`] | refuse the new frame, typed error | commands whose loss must be visible |
//! | [`OverflowPolicy::DropOldest`] | evict the oldest, accept the new | heartbeats and metrics — the newest is the truth |
//! | [`OverflowPolicy::DropNewest`] | silently discard the new one | logs, where order matters more than recency |
//!
//! Every policy counts what it dropped, so a reconnect that lost data says so
//! rather than pretending it did not.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::{OverflowPolicy, ReconnectBuffer};
//! use astrs_wire::{Frame, FrameFlags, FrameKind};
//!
//! let mut buffer = ReconnectBuffer::new(2, OverflowPolicy::DropOldest);
//! for index in 0..3u8 {
//!     let frame = Frame::new(FrameKind::DaemonEvent, FrameFlags::EMPTY, vec![index])?;
//!     buffer.push(frame).expect("a lossy policy never rejects");
//! }
//!
//! assert_eq!(buffer.len(), 2);
//! assert_eq!(buffer.dropped(), 1);
//! // The oldest went; the newest survived.
//! assert_eq!(buffer.pop().map(|frame| frame.payload().to_vec()), Some(vec![1]));
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

use std::collections::VecDeque;

use astrs_wire::Frame;

use crate::error::{TransportError, TransportResult};

/// What to do when the buffer is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OverflowPolicy {
    /// Refuse the new frame with [`TransportError::ReconnectBufferOverflow`].
    ///
    /// The default, because a caller that has not thought about it should find
    /// out that frames are being lost rather than discover it in a postmortem.
    #[default]
    Reject,
    /// Evict the oldest frame to make room.
    DropOldest,
    /// Discard the new frame and report success.
    DropNewest,
}

impl OverflowPolicy {
    /// A stable label for logs and metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::DropOldest => "drop_oldest",
            Self::DropNewest => "drop_newest",
        }
    }

    /// Whether this policy ever loses a frame silently.
    #[must_use]
    pub const fn is_lossy(self) -> bool {
        !matches!(self, Self::Reject)
    }
}

/// Frames queued while a connection is down.
#[derive(Debug)]
pub struct ReconnectBuffer {
    /// The queued frames, oldest first.
    frames: VecDeque<Frame>,
    /// How many frames fit.
    capacity: usize,
    /// What to do when it is full.
    policy: OverflowPolicy,
    /// How many frames have been lost, ever.
    dropped: usize,
    /// How many bytes are queued right now.
    queued_bytes: usize,
}

impl ReconnectBuffer {
    /// A buffer holding `capacity` frames under `policy`.
    ///
    /// A capacity of zero is legal and means "buffer nothing": every push is
    /// an overflow, which is the right configuration for a link whose messages
    /// are worthless once stale.
    #[must_use]
    pub fn new(capacity: usize, policy: OverflowPolicy) -> Self {
        Self {
            frames: VecDeque::with_capacity(capacity.min(1_024)),
            capacity,
            policy,
            dropped: 0,
            queued_bytes: 0,
        }
    }

    /// A buffer sized by a [`crate::BackoffConfig`].
    #[must_use]
    pub fn from_config(config: &crate::config::BackoffConfig, policy: OverflowPolicy) -> Self {
        Self::new(config.buffer_frames, policy)
    }

    /// How many frames fit.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// What happens at the boundary.
    #[must_use]
    pub const fn policy(&self) -> OverflowPolicy {
        self.policy
    }

    /// How many frames are queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether anything is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Whether another frame would overflow.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.frames.len() >= self.capacity
    }

    /// How many bytes of payload are queued.
    ///
    /// A frame count bounds the queue; a byte count is what an operator wants
    /// when the queue is full of point clouds.
    #[must_use]
    pub const fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// How many frames have been lost since this buffer was created.
    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }

    /// Queues a frame.
    ///
    /// # Errors
    ///
    /// [`TransportError::ReconnectBufferOverflow`] under
    /// [`OverflowPolicy::Reject`] when the buffer is full. The lossy policies
    /// never fail; they count instead.
    pub fn push(&mut self, frame: Frame) -> TransportResult<()> {
        if self.is_full() {
            match self.policy {
                OverflowPolicy::Reject => {
                    self.dropped = self.dropped.saturating_add(1);
                    return Err(TransportError::ReconnectBufferOverflow {
                        dropped: 1,
                        capacity: self.capacity,
                    });
                }
                OverflowPolicy::DropNewest => {
                    self.dropped = self.dropped.saturating_add(1);
                    return Ok(());
                }
                OverflowPolicy::DropOldest => {
                    // A capacity of zero has nothing to evict, so the new
                    // frame is what goes.
                    match self.frames.pop_front() {
                        Some(evicted) => {
                            self.queued_bytes =
                                self.queued_bytes.saturating_sub(evicted.payload().len());
                        }
                        None => {
                            self.dropped = self.dropped.saturating_add(1);
                            return Ok(());
                        }
                    }
                    self.dropped = self.dropped.saturating_add(1);
                }
            }
        }
        self.queued_bytes = self.queued_bytes.saturating_add(frame.payload().len());
        self.frames.push_back(frame);
        Ok(())
    }

    /// Takes the oldest queued frame.
    #[must_use]
    pub fn pop(&mut self) -> Option<Frame> {
        let frame = self.frames.pop_front()?;
        self.queued_bytes = self.queued_bytes.saturating_sub(frame.payload().len());
        Some(frame)
    }

    /// Takes every queued frame, oldest first, leaving the buffer empty.
    ///
    /// This is what a reconnect does the instant a new connection comes up.
    #[must_use]
    pub fn drain(&mut self) -> Vec<Frame> {
        self.queued_bytes = 0;
        self.frames.drain(..).collect()
    }

    /// Discards everything queued, counting it as dropped.
    ///
    /// Used when a reconnect gives up: the frames will never be delivered, and
    /// saying so is better than holding them until the process exits.
    pub fn clear(&mut self) {
        self.dropped = self.dropped.saturating_add(self.frames.len());
        self.frames.clear();
        self.queued_bytes = 0;
    }

    /// Resets the drop counter, after it has been reported.
    pub const fn reset_dropped(&mut self) -> usize {
        let dropped = self.dropped;
        self.dropped = 0;
        dropped
    }

    /// A borrowed view of the queued frames, oldest first.
    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter()
    }
}

impl Default for ReconnectBuffer {
    fn default() -> Self {
        Self::new(
            crate::config::DEFAULT_RECONNECT_BUFFER_FRAMES,
            OverflowPolicy::default(),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{FrameFlags, FrameKind};

    fn frame(tag: u8) -> Frame {
        Frame::new(FrameKind::DaemonEvent, FrameFlags::EMPTY, vec![tag; 4]).unwrap()
    }

    #[test]
    fn frames_come_back_in_the_order_they_went_in() {
        let mut buffer = ReconnectBuffer::new(8, OverflowPolicy::Reject);
        assert!(buffer.is_empty());
        for index in 0..4u8 {
            buffer.push(frame(index)).unwrap();
        }
        assert_eq!(buffer.len(), 4);
        assert_eq!(buffer.queued_bytes(), 16);

        for index in 0..4u8 {
            assert_eq!(buffer.pop().unwrap().payload(), &[index; 4]);
        }
        assert!(buffer.is_empty());
        assert_eq!(buffer.queued_bytes(), 0);
        assert!(buffer.pop().is_none());
    }

    #[test]
    fn rejecting_is_a_typed_error_that_names_the_capacity() {
        let mut buffer = ReconnectBuffer::new(2, OverflowPolicy::Reject);
        buffer.push(frame(0)).unwrap();
        buffer.push(frame(1)).unwrap();
        assert!(buffer.is_full());

        let err = buffer.push(frame(2)).unwrap_err();
        match err {
            TransportError::ReconnectBufferOverflow { dropped, capacity } => {
                assert_eq!(dropped, 1);
                assert_eq!(capacity, 2);
            }
            other => panic!("expected an overflow, got {other:?}"),
        }
        assert_eq!(buffer.dropped(), 1);
        assert_eq!(buffer.len(), 2, "the queued frames are untouched");
    }

    #[test]
    fn dropping_the_oldest_keeps_the_freshest_frames() {
        let mut buffer = ReconnectBuffer::new(3, OverflowPolicy::DropOldest);
        for index in 0..6u8 {
            buffer.push(frame(index)).unwrap();
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.dropped(), 3);
        let survivors: Vec<u8> = buffer.drain().iter().map(|f| f.payload()[0]).collect();
        assert_eq!(survivors, vec![3, 4, 5]);
    }

    #[test]
    fn dropping_the_newest_keeps_the_oldest_frames() {
        let mut buffer = ReconnectBuffer::new(3, OverflowPolicy::DropNewest);
        for index in 0..6u8 {
            buffer.push(frame(index)).unwrap();
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.dropped(), 3);
        let survivors: Vec<u8> = buffer.drain().iter().map(|f| f.payload()[0]).collect();
        assert_eq!(survivors, vec![0, 1, 2]);
    }

    #[test]
    fn a_zero_capacity_buffer_never_holds_anything() {
        for policy in [
            OverflowPolicy::Reject,
            OverflowPolicy::DropOldest,
            OverflowPolicy::DropNewest,
        ] {
            let mut buffer = ReconnectBuffer::new(0, policy);
            assert!(buffer.is_full());
            let result = buffer.push(frame(0));
            if policy == OverflowPolicy::Reject {
                assert!(result.is_err(), "{policy:?}");
            } else {
                assert!(result.is_ok(), "{policy:?}");
            }
            assert!(buffer.is_empty(), "{policy:?}");
            assert_eq!(buffer.dropped(), 1, "{policy:?}");
        }
    }

    #[test]
    fn draining_empties_the_buffer_and_the_byte_count() {
        let mut buffer = ReconnectBuffer::new(8, OverflowPolicy::Reject);
        for index in 0..4u8 {
            buffer.push(frame(index)).unwrap();
        }
        let drained = buffer.drain();
        assert_eq!(drained.len(), 4);
        assert!(buffer.is_empty());
        assert_eq!(buffer.queued_bytes(), 0);
        assert!(buffer.drain().is_empty());
    }

    #[test]
    fn clearing_counts_what_it_threw_away() {
        let mut buffer = ReconnectBuffer::new(8, OverflowPolicy::Reject);
        for index in 0..3u8 {
            buffer.push(frame(index)).unwrap();
        }
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.dropped(), 3);
        assert_eq!(buffer.queued_bytes(), 0);
    }

    #[test]
    fn the_drop_counter_can_be_taken_and_reset() {
        let mut buffer = ReconnectBuffer::new(1, OverflowPolicy::DropOldest);
        buffer.push(frame(0)).unwrap();
        buffer.push(frame(1)).unwrap();
        assert_eq!(buffer.reset_dropped(), 1);
        assert_eq!(buffer.dropped(), 0);
        assert_eq!(buffer.reset_dropped(), 0);
    }

    #[test]
    fn queued_bytes_track_evictions() {
        let mut buffer = ReconnectBuffer::new(2, OverflowPolicy::DropOldest);
        buffer.push(frame(0)).unwrap();
        buffer.push(frame(1)).unwrap();
        assert_eq!(buffer.queued_bytes(), 8);
        buffer.push(frame(2)).unwrap();
        assert_eq!(buffer.queued_bytes(), 8, "one in, one out");
    }

    #[test]
    fn frames_can_be_inspected_without_draining() {
        let mut buffer = ReconnectBuffer::new(4, OverflowPolicy::Reject);
        for index in 0..3u8 {
            buffer.push(frame(index)).unwrap();
        }
        let tags: Vec<u8> = buffer.frames().map(|frame| frame.payload()[0]).collect();
        assert_eq!(tags, vec![0, 1, 2]);
        assert_eq!(buffer.len(), 3, "inspection must not consume");
    }

    #[test]
    fn policies_describe_themselves() {
        assert_eq!(OverflowPolicy::default(), OverflowPolicy::Reject);
        assert_eq!(OverflowPolicy::Reject.label(), "reject");
        assert_eq!(OverflowPolicy::DropOldest.label(), "drop_oldest");
        assert_eq!(OverflowPolicy::DropNewest.label(), "drop_newest");
        assert!(!OverflowPolicy::Reject.is_lossy());
        assert!(OverflowPolicy::DropOldest.is_lossy());
        assert!(OverflowPolicy::DropNewest.is_lossy());
    }

    #[test]
    fn a_buffer_can_be_sized_from_the_backoff_policy() {
        let config = crate::config::BackoffConfig::new().with_buffer_frames(7);
        let buffer = ReconnectBuffer::from_config(&config, OverflowPolicy::DropOldest);
        assert_eq!(buffer.capacity(), 7);
        assert_eq!(buffer.policy(), OverflowPolicy::DropOldest);

        let default = ReconnectBuffer::default();
        assert_eq!(
            default.capacity(),
            crate::config::DEFAULT_RECONNECT_BUFFER_FRAMES
        );
    }

    #[test]
    fn a_huge_capacity_does_not_preallocate_it() {
        // The buffer must be cheap to create even when the policy is generous.
        let buffer = ReconnectBuffer::new(usize::MAX, OverflowPolicy::Reject);
        assert_eq!(buffer.capacity(), usize::MAX);
        assert!(buffer.is_empty());
        assert!(!buffer.is_full());
    }
}
