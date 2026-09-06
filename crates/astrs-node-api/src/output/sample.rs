//! [`OutputSample`] — a write buffer that is a ring slot when it can be.
//!
//! Blueprint §9.1 promises `allocate(len) → SampleMut`, and §6.2 says what
//! that means: *"`node.allocate(len)` hands the producer a `SampleMut`
//! pointing into the next FREE slot; `send` flips WRITING→READY. No copy ever
//! happens for the fast path."*
//!
//! This type is that buffer, plus the one thing a bare `SampleMut` cannot do:
//! fall back. A node writes the same code whether or not its route has been
//! upgraded yet, because [`RawOutput::allocate`](super::RawOutput::allocate)
//! hands back the same type either way —
//! [`SampleBuffer::Shm`] when the ring is available and
//! [`SampleBuffer::Heap`] when it is not.
//!
//! # Committing is consuming
//!
//! [`OutputSample::send`] takes `self`, so a window cannot be published twice
//! and cannot be written to after publication. Dropping one without sending
//! aborts it, which returns the slot to the pool *without consuming its
//! sequence number* — an aborted write leaves no gap for consumers to wait on.

use std::sync::Arc;

use astrs_shm::SampleMut;
use astrs_wire::{DataId, Metadata, NodeRequest, OutputPayload};

use crate::error::Result;
use crate::session::SessionShared;

/// Where an [`OutputSample`]'s bytes live.
pub enum SampleBuffer<'a> {
    /// A shared-memory ring slot: writing here copies nothing (§6.2).
    Shm(SampleMut<'a>),
    /// An owned heap buffer, published inline on the daemon path.
    Heap(Vec<u8>),
}

impl core::fmt::Debug for SampleBuffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Shm(window) => f
                .debug_struct("Shm")
                .field("slot", &window.slot_index())
                .field("seq", &window.seq())
                .field("len", &window.len())
                .finish(),
            Self::Heap(bytes) => f.debug_struct("Heap").field("len", &bytes.len()).finish(),
        }
    }
}

/// A reserved write buffer for one message.
#[derive(Debug)]
pub struct OutputSample<'a> {
    /// The bytes.
    buffer: SampleBuffer<'a>,
    /// The session this publishes through.
    shared: Arc<SessionShared>,
    /// The output this belongs to.
    output: DataId,
    /// The segment name and generation, when the buffer is a ring slot.
    segment: Option<(String, u64)>,
}

impl<'a> OutputSample<'a> {
    /// A sample backed by a ring slot.
    #[must_use]
    pub fn shm(
        window: SampleMut<'a>,
        shared: Arc<SessionShared>,
        output: DataId,
        segment: Option<(String, u64)>,
    ) -> Self {
        Self {
            buffer: SampleBuffer::Shm(window),
            shared,
            output,
            segment,
        }
    }

    /// A sample backed by an owned heap buffer.
    #[must_use]
    pub fn heap(bytes: Vec<u8>, shared: Arc<SessionShared>, output: DataId) -> Self {
        Self {
            buffer: SampleBuffer::Heap(bytes),
            shared,
            output,
            segment: None,
        }
    }

    /// Whether writing here copies nothing.
    #[must_use]
    pub const fn is_zero_copy(&self) -> bool {
        matches!(self.buffer, SampleBuffer::Shm(_))
    }

    /// The buffer's length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.buffer {
            SampleBuffer::Shm(window) => window.len(),
            SampleBuffer::Heap(bytes) => bytes.len(),
        }
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The writable bytes.
    ///
    /// On the zero-copy plane the slice base is 128-byte aligned (§6.1), so an
    /// Arrow IPC message serialised straight into it is directly usable as a
    /// SIMD source by every consumer.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        match &mut self.buffer {
            SampleBuffer::Shm(window) => window.as_mut_slice(),
            SampleBuffer::Heap(bytes) => bytes.as_mut_slice(),
        }
    }

    /// The bytes written so far.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        match &self.buffer {
            SampleBuffer::Shm(window) => window.as_slice(),
            SampleBuffer::Heap(bytes) => bytes.as_slice(),
        }
    }

    /// The output this sample belongs to.
    #[must_use]
    pub const fn output(&self) -> &DataId {
        &self.output
    }

    /// The sequence number this window will publish, on the zero-copy plane.
    #[must_use]
    pub fn sequence(&self) -> Option<u64> {
        match &self.buffer {
            SampleBuffer::Shm(window) => Some(window.seq()),
            SampleBuffer::Heap(_) => None,
        }
    }

    /// Shortens the buffer to `len` bytes.
    ///
    /// The encoder that reserved a generous window and produced less keeps the
    /// zero-copy path rather than falling back to a copy.
    ///
    /// # Errors
    ///
    /// [`crate::NodeError::Shm`] when `len` is longer than the window.
    pub fn truncate(&mut self, len: usize) -> Result<()> {
        match &mut self.buffer {
            SampleBuffer::Shm(window) => Ok(window.truncate(len)?),
            SampleBuffer::Heap(bytes) => {
                if len <= bytes.len() {
                    bytes.truncate(len);
                }
                Ok(())
            }
        }
    }

    /// Publishes the message.
    ///
    /// # Errors
    ///
    /// [`crate::NodeError::DaemonGone`] once the session has ended, and
    /// [`crate::NodeError::Wire`] when the metadata cannot be encoded.
    pub fn send(self, metadata: Metadata) -> Result<()> {
        let Self {
            buffer,
            shared,
            output,
            segment,
        } = self;
        match buffer {
            SampleBuffer::Shm(window) => {
                let slot = window.slot_index();
                let len = window.len() as u64;
                let meta_bytes = super::encode_metadata(&metadata)?;
                match window.commit(&meta_bytes) {
                    Ok(_seq) => {
                        let (name, generation) = segment.unwrap_or_default();
                        shared.count_zero_copy_send();
                        shared.send_request(NodeRequest::SendMessage {
                            output,
                            metadata,
                            payload: OutputPayload::Shm {
                                segment: name,
                                slot,
                                len,
                                generation,
                            },
                        })
                    }
                    Err(error) => {
                        // The commit could only fail because the segment
                        // closed underneath us; §6.2 says fall back rather
                        // than retry, but the bytes are gone with the slot,
                        // so the honest answer is the error.
                        shared.count_shm_fallback();
                        Err(error.into())
                    }
                }
            }
            SampleBuffer::Heap(bytes) => {
                shared.count_inline_send();
                shared.send_request(NodeRequest::SendMessage {
                    output,
                    metadata,
                    payload: OutputPayload::inline(bytes),
                })
            }
        }
    }

    /// Discards the reservation.
    ///
    /// A ring slot returns to the pool without consuming its sequence number.
    pub fn abort(self) {
        if let SampleBuffer::Shm(window) = self.buffer {
            window.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::events::EventSource;
    use crate::runtime::NodeRuntime;
    use crate::session::{OUTGOING_CAPACITY, Outgoing};
    use astrs_wire::{DataflowId, FrameLimits, NodeId, NodeSource, NodeSpawnSpec, SessionId};
    use tokio::sync::mpsc;

    fn session() -> (Arc<SessionShared>, mpsc::Receiver<Outgoing>) {
        let (sender, receiver) = mpsc::channel(OUTGOING_CAPACITY);
        let spec = Arc::new(NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("camera").unwrap(),
            0,
            NodeSource::Dynamic,
        ));
        let shared = Arc::new(SessionShared::new(
            spec,
            SessionId::from_u128(1),
            sender,
            Arc::new(EventSource::new()),
            NodeRuntime::acquire().unwrap(),
            4096,
            FrameLimits::uds(),
        ));
        (shared, receiver)
    }

    #[test]
    fn a_heap_sample_publishes_inline() {
        let (shared, mut receiver) = session();
        let output = DataId::new("image").unwrap();
        let mut sample = OutputSample::heap(vec![0; 4], Arc::clone(&shared), output.clone());
        assert!(!sample.is_zero_copy());
        assert_eq!(sample.len(), 4);
        assert!(!sample.is_empty());
        assert_eq!(sample.output(), &output);
        assert_eq!(sample.sequence(), None);
        sample.as_mut_slice().copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(sample.as_slice(), &[1, 2, 3, 4]);

        sample.send(Metadata::default()).unwrap();
        let Ok(Outgoing::Request(request)) = receiver.try_recv() else {
            panic!("expected a send");
        };
        let NodeRequest::SendMessage { payload, .. } = *request else {
            panic!("expected a SendMessage");
        };
        assert_eq!(payload.bytes(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(shared.stats().sends_inline, 1);
    }

    #[test]
    fn truncating_a_heap_sample_shortens_the_payload() {
        let (shared, mut receiver) = session();
        let mut sample = OutputSample::heap(
            vec![9; 8],
            Arc::clone(&shared),
            DataId::new("image").unwrap(),
        );
        sample.truncate(3).unwrap();
        assert_eq!(sample.len(), 3);
        sample.truncate(99).unwrap();
        assert_eq!(sample.len(), 3, "a longer truncate is a no-op");
        sample.send(Metadata::default()).unwrap();

        let Ok(Outgoing::Request(request)) = receiver.try_recv() else {
            panic!("expected a send");
        };
        let NodeRequest::SendMessage { payload, .. } = *request else {
            panic!("expected a SendMessage");
        };
        assert_eq!(payload.len(), 3);
    }

    #[test]
    fn aborting_a_heap_sample_publishes_nothing() {
        let (shared, mut receiver) = session();
        let sample = OutputSample::heap(
            vec![0; 4],
            Arc::clone(&shared),
            DataId::new("image").unwrap(),
        );
        sample.abort();
        assert!(receiver.try_recv().is_err());
        assert_eq!(shared.stats().sends_inline, 0);
    }

    #[test]
    fn a_zero_length_sample_is_empty() {
        let (shared, _receiver) = session();
        let sample = OutputSample::heap(Vec::new(), shared, DataId::new("image").unwrap());
        assert!(sample.is_empty());
        assert_eq!(sample.as_slice(), &[] as &[u8]);
        assert!(format!("{sample:?}").contains("Heap"));
    }
}
