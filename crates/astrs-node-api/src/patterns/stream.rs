//! Chunked transfers over ordinary edges (blueprint §9.4,
//! `session_id`/`segment_id`/`seq`/`fin`/`flush`).
//!
//! ```text
//!   session S ┬─ segment 0 ─ seq 0,1,2 … fin        (one point cloud)
//!             ├─ segment 1 ─ seq 0,1 … fin          (the next one)
//!             └─ segment 2 ─ seq 0 … flush … fin    (a partial delivery)
//! ```
//!
//! Three keys, three jobs: `session_id` says *which conversation*,
//! `segment_id` says *which message within it*, `seq` says *which chunk within
//! that message*. `fin` ends a segment; `flush` asks the receiver to deliver
//! what it has without ending anything.
//!
//! # Why a node needs help here
//!
//! A chunked transfer has three failure modes that a bare metadata key cannot
//! prevent, and [`StreamAssembler`] detects all three:
//!
//! | Failure | How it is caught |
//! |---|---|
//! | A gap (chunk 3 arrives after chunk 1) | The assembler expects `seq` to be contiguous and reports [`crate::NodeError::Pattern`] |
//! | Reordering | Same check; out-of-order is a gap from the assembler's side |
//! | A segment that never ends | [`StreamAssembler::open_segments`] shows it; a node with a deadline can act |
//!
//! Correlated messages are *not* eviction-immune here — a stream chunk carries
//! none of §11.2's correlation keys — which is deliberate: a stream that
//! outruns its consumer should drop, and the assembler will say so at the
//! gap rather than silently reassembling the wrong bytes.

use core::fmt;
use std::collections::BTreeMap;

use astrs_wire::{DataId, Metadata};

use crate::error::{NodeError, Result};
use crate::events::Event;
use crate::node::Node;
use crate::output::RawOutput;
use crate::payload::Payload;

/// One chunk's place in its stream.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkRef {
    /// Which conversation.
    pub session: String,
    /// Which message within it.
    pub segment: i64,
    /// Which chunk within that message.
    pub seq: i64,
    /// Whether this chunk ends the segment.
    pub fin: bool,
    /// Whether the sender asked for an immediate delivery.
    pub flush: bool,
}

impl ChunkRef {
    /// Reads a chunk reference out of metadata, if it carries one.
    #[must_use]
    pub fn from_metadata(metadata: &Metadata) -> Option<Self> {
        Some(Self {
            session: metadata.session_id()?.to_owned(),
            segment: metadata.segment_id().unwrap_or(0),
            seq: metadata.seq().unwrap_or(0),
            fin: metadata.fin().unwrap_or(false),
            flush: metadata.flush().unwrap_or(false),
        })
    }

    /// The `(session, segment)` pair this chunk belongs to.
    #[must_use]
    pub fn segment_key(&self) -> (String, i64) {
        (self.session.clone(), self.segment)
    }
}

impl fmt::Display for ChunkRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.session, self.segment, self.seq)?;
        if self.fin {
            f.write_str(" fin")?;
        }
        if self.flush {
            f.write_str(" flush")?;
        }
        Ok(())
    }
}

/// What [`StreamAssembler::accept_event`] made of an event: a completed
/// segment, a chunk it absorbed (`Ok(None)`), or the event handed back
/// because it was not a stream chunk at all.
pub type AcceptedChunk = core::result::Result<Option<StreamSegment>, Box<Event>>;

/// A completed segment, reassembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSegment {
    /// Which conversation.
    pub session: String,
    /// Which message.
    pub segment: i64,
    /// The concatenated payload.
    pub bytes: Vec<u8>,
    /// How many chunks it took.
    pub chunks: usize,
}

/// The sending side of a stream (§9.4).
///
/// Owns the `(session, segment, seq)` bookkeeping so a node cannot forget to
/// advance the sequence or to end a segment.
#[derive(Debug, Clone)]
pub struct StreamWriter {
    /// The conversation id.
    session: String,
    /// The message being sent.
    segment: i64,
    /// The next chunk's sequence number.
    seq: i64,
    /// How many segments have been finished.
    finished: u64,
}

impl StreamWriter {
    /// A writer over a fresh session.
    #[must_use]
    pub fn new() -> Self {
        Self::with_session(super::fresh_id())
    }

    /// A writer over an existing session id.
    #[must_use]
    pub fn with_session(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            segment: 0,
            seq: 0,
            finished: 0,
        }
    }

    /// The conversation id.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The segment being sent.
    #[must_use]
    pub const fn segment(&self) -> i64 {
        self.segment
    }

    /// The next chunk's sequence number.
    #[must_use]
    pub const fn seq(&self) -> i64 {
        self.seq
    }

    /// How many segments this writer has finished.
    #[must_use]
    pub const fn finished_segments(&self) -> u64 {
        self.finished
    }

    /// The metadata for the next chunk, advancing the sequence.
    ///
    /// `fin` ends the segment: the next chunk starts segment `n + 1` at
    /// sequence zero.
    pub fn next_metadata(&mut self, base: Metadata, fin: bool, flush: bool) -> Metadata {
        let mut metadata = base;
        metadata.set_session_id(self.session.clone());
        metadata.set_segment_id(self.segment);
        metadata.set_seq(self.seq);
        if fin {
            metadata.set_fin(true);
        }
        if flush {
            metadata.set_flush(true);
        }
        if fin {
            self.segment = self.segment.saturating_add(1);
            self.seq = 0;
            self.finished = self.finished.saturating_add(1);
        } else {
            self.seq = self.seq.saturating_add(1);
        }
        metadata
    }

    /// Starts a new segment without finishing the current one.
    ///
    /// For a sender that gave up on a partially sent message; the receiver
    /// sees the abandoned segment in
    /// [`StreamAssembler::open_segments`].
    pub fn abandon_segment(&mut self) {
        self.segment = self.segment.saturating_add(1);
        self.seq = 0;
    }
}

impl Default for StreamWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// The receiving side of a stream (§9.4).
#[derive(Debug, Default)]
pub struct StreamAssembler {
    /// Partially received segments, keyed by `(session, segment)`.
    open: BTreeMap<(String, i64), Partial>,
    /// How many segments have completed.
    completed: u64,
}

/// One partially received segment.
#[derive(Debug, Default)]
struct Partial {
    /// The bytes so far.
    bytes: Vec<u8>,
    /// The sequence number expected next.
    next_seq: i64,
    /// How many chunks have arrived.
    chunks: usize,
}

impl StreamAssembler {
    /// An empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many segments are partially received.
    #[must_use]
    pub fn open_segments(&self) -> Vec<(String, i64)> {
        self.open.keys().cloned().collect()
    }

    /// How many segments have completed.
    #[must_use]
    pub const fn completed(&self) -> u64 {
        self.completed
    }

    /// How many bytes are buffered across every open segment.
    #[must_use]
    pub fn buffered_bytes(&self) -> usize {
        self.open.values().map(|partial| partial.bytes.len()).sum()
    }

    /// Accepts one chunk, returning the segment when it completes.
    ///
    /// # Errors
    ///
    /// [`NodeError::Pattern`] when the chunk's `seq` is not the one expected —
    /// a gap or a reordering, either of which would silently corrupt the
    /// reassembled bytes.
    pub fn accept(&mut self, chunk: &ChunkRef, bytes: &[u8]) -> Result<Option<StreamSegment>> {
        let key = chunk.segment_key();
        let partial = self.open.entry(key.clone()).or_default();
        if chunk.seq != partial.next_seq {
            let expected = partial.next_seq;
            let _abandoned = self.open.remove(&key);
            return Err(NodeError::Pattern(format!(
                "stream {}/{}: expected chunk {expected}, got {}",
                chunk.session, chunk.segment, chunk.seq
            )));
        }
        partial.bytes.extend_from_slice(bytes);
        partial.next_seq = partial.next_seq.saturating_add(1);
        partial.chunks += 1;
        if !chunk.fin {
            return Ok(None);
        }
        let Some(partial) = self.open.remove(&key) else {
            return Ok(None);
        };
        self.completed = self.completed.saturating_add(1);
        Ok(Some(StreamSegment {
            session: chunk.session.clone(),
            segment: chunk.segment,
            bytes: partial.bytes,
            chunks: partial.chunks,
        }))
    }

    /// Accepts one event, returning the segment when it completes.
    ///
    /// An event that is not a stream chunk is returned unchanged, so a node's
    /// loop can offer every input to the assembler and handle what it does not
    /// take.
    ///
    /// # Errors
    ///
    /// As [`StreamAssembler::accept`].
    pub fn accept_event(&mut self, event: Event) -> Result<AcceptedChunk> {
        let Event::Input { id, meta, data } = event else {
            return Ok(Err(Box::new(event)));
        };
        let Some(chunk) = ChunkRef::from_metadata(&meta) else {
            return Ok(Err(Box::new(Event::Input { id, meta, data })));
        };
        self.accept(&chunk, data.bytes()).map(Ok)
    }

    /// Forgets an abandoned segment.
    ///
    /// Returns the bytes that were buffered for it.
    pub fn abandon(&mut self, session: &str, segment: i64) -> Option<Vec<u8>> {
        self.open
            .remove(&(session.to_owned(), segment))
            .map(|partial| partial.bytes)
    }
}

impl Node {
    /// Publishes one stream chunk (§9.4).
    ///
    /// The writer advances its own `(segment, seq)` bookkeeping, so a node
    /// never numbers a chunk by hand.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn stream_chunk(
        &self,
        output: &mut RawOutput,
        writer: &mut StreamWriter,
        payload: impl AsRef<[u8]>,
        fin: bool,
    ) -> Result<ChunkRef> {
        self.stream_chunk_with(output, writer, payload, fin, false)
    }

    /// Publishes one stream chunk, optionally asking for an immediate
    /// delivery.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn stream_chunk_with(
        &self,
        output: &mut RawOutput,
        writer: &mut StreamWriter,
        payload: impl AsRef<[u8]>,
        fin: bool,
        flush: bool,
    ) -> Result<ChunkRef> {
        let metadata = writer.next_metadata(self.metadata(), fin, flush);
        let chunk = ChunkRef::from_metadata(&metadata)
            .ok_or_else(|| NodeError::Pattern("a stream chunk needs a session id".to_owned()))?;
        output.send_bytes(payload, metadata)?;
        Ok(chunk)
    }

    /// Publishes a whole payload as a run of chunks of at most `chunk_size`
    /// bytes, ending the segment.
    ///
    /// # Errors
    ///
    /// [`NodeError::Pattern`] for a zero chunk size, plus whatever
    /// [`Node::stream_chunk`] reports.
    pub fn stream_segment(
        &self,
        output: &mut RawOutput,
        writer: &mut StreamWriter,
        payload: &[u8],
        chunk_size: usize,
    ) -> Result<usize> {
        if chunk_size == 0 {
            return Err(NodeError::Pattern(
                "a stream chunk size of zero would never finish".to_owned(),
            ));
        }
        if payload.is_empty() {
            let _chunk = self.stream_chunk(output, writer, [], true)?;
            return Ok(1);
        }
        let mut sent = 0;
        let mut chunks = payload.chunks(chunk_size).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let _reference = self.stream_chunk(output, writer, chunk, last)?;
            sent += 1;
        }
        Ok(sent)
    }

    /// The input a stream chunk arrived on, when an event is one.
    #[must_use]
    pub fn stream_chunk_of(event: &Event) -> Option<(DataId, ChunkRef)> {
        let Event::Input { id, meta, .. } = event else {
            return None;
        };
        ChunkRef::from_metadata(meta).map(|chunk| (id.clone(), chunk))
    }
}

/// The payload bytes of an event, for a caller assembling by hand.
#[must_use]
pub fn chunk_bytes(event: &Event) -> Option<&[u8]> {
    event.payload().map(Payload::bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::testing::TestHarness;
    use astrs_time::HlcTimestamp;
    use std::time::Duration;

    fn metadata() -> Metadata {
        Metadata::new(HlcTimestamp::new(1, 0))
    }

    #[test]
    fn a_writer_numbers_its_chunks_and_segments() {
        let mut writer = StreamWriter::with_session("s1");
        assert_eq!(writer.session(), "s1");
        assert_eq!(writer.segment(), 0);
        assert_eq!(writer.seq(), 0);

        let first = writer.next_metadata(metadata(), false, false);
        assert_eq!(first.seq(), Some(0));
        assert_eq!(first.segment_id(), Some(0));
        assert_eq!(writer.seq(), 1);

        let second = writer.next_metadata(metadata(), true, false);
        assert_eq!(second.seq(), Some(1));
        assert_eq!(second.fin(), Some(true));
        assert_eq!(writer.segment(), 1, "fin starts the next segment");
        assert_eq!(writer.seq(), 0);
        assert_eq!(writer.finished_segments(), 1);

        writer.abandon_segment();
        assert_eq!(writer.segment(), 2);
        assert_eq!(writer.finished_segments(), 1, "abandoning is not finishing");
    }

    #[test]
    fn a_flush_is_carried_without_ending_the_segment() {
        let mut writer = StreamWriter::new();
        let metadata = writer.next_metadata(metadata(), false, true);
        assert_eq!(metadata.flush(), Some(true));
        assert_eq!(metadata.fin(), None);
        assert_eq!(writer.segment(), 0);
    }

    #[test]
    fn an_assembler_reassembles_a_segment() {
        let mut writer = StreamWriter::with_session("s1");
        let mut assembler = StreamAssembler::new();

        let first =
            ChunkRef::from_metadata(&writer.next_metadata(metadata(), false, false)).unwrap();
        assert!(assembler.accept(&first, b"hello ").unwrap().is_none());
        assert_eq!(assembler.open_segments(), vec![("s1".to_owned(), 0)]);
        assert_eq!(assembler.buffered_bytes(), 6);

        let last = ChunkRef::from_metadata(&writer.next_metadata(metadata(), true, false)).unwrap();
        let segment = assembler.accept(&last, b"world").unwrap().unwrap();
        assert_eq!(segment.bytes, b"hello world");
        assert_eq!(segment.chunks, 2);
        assert_eq!(segment.segment, 0);
        assert_eq!(segment.session, "s1");
        assert_eq!(assembler.completed(), 1);
        assert!(assembler.open_segments().is_empty());
        assert_eq!(assembler.buffered_bytes(), 0);
    }

    #[test]
    fn a_gap_is_reported_rather_than_reassembled_wrong() {
        let mut assembler = StreamAssembler::new();
        let first = ChunkRef {
            session: "s".to_owned(),
            segment: 0,
            seq: 0,
            fin: false,
            flush: false,
        };
        assert!(assembler.accept(&first, b"a").unwrap().is_none());

        let skipped = ChunkRef { seq: 2, ..first };
        let error = assembler.accept(&skipped, b"c").unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
        assert!(error.to_string().contains("expected chunk 1"), "{error}");
        assert!(
            assembler.open_segments().is_empty(),
            "a broken segment is abandoned, not left half-built"
        );
    }

    #[test]
    fn interleaved_segments_are_kept_apart() {
        let mut assembler = StreamAssembler::new();
        let a0 = ChunkRef {
            session: "s".to_owned(),
            segment: 0,
            seq: 0,
            fin: false,
            flush: false,
        };
        let b0 = ChunkRef {
            segment: 1,
            ..a0.clone()
        };
        assert!(assembler.accept(&a0, b"A").unwrap().is_none());
        assert!(assembler.accept(&b0, b"B").unwrap().is_none());
        assert_eq!(assembler.open_segments().len(), 2);

        let a1 = ChunkRef {
            seq: 1,
            fin: true,
            ..a0
        };
        let segment = assembler.accept(&a1, b"a").unwrap().unwrap();
        assert_eq!(segment.bytes, b"Aa");
        assert_eq!(assembler.open_segments(), vec![("s".to_owned(), 1)]);
        assert_eq!(assembler.abandon("s", 1).unwrap(), b"B");
        assert!(assembler.abandon("s", 9).is_none());
    }

    #[test]
    fn a_non_stream_event_is_handed_back() {
        let mut assembler = StreamAssembler::new();
        let event = Event::Input {
            id: DataId::new("in").unwrap(),
            meta: metadata(),
            data: Payload::inline(vec![1]),
        };
        let handed_back = assembler.accept_event(event).unwrap().unwrap_err();
        assert!(handed_back.is_input());
        assert!(chunk_bytes(&handed_back).is_some());

        let control = assembler
            .accept_event(Event::AllInputsClosed)
            .unwrap()
            .unwrap_err();
        assert!(!control.is_input());
        assert!(chunk_bytes(&control).is_none());
    }

    #[test]
    fn a_whole_payload_is_chunked_and_reassembled_over_the_wire() {
        let (mut producer, mut consumer) =
            TestHarness::pair("sender", "chunks", "receiver", "chunks").unwrap();
        let mut output = producer.node.raw_output("chunks").unwrap();
        let mut writer = StreamWriter::with_session("transfer-1");

        let payload: Vec<u8> = (0..250u8).collect();
        let sent = producer
            .node
            .stream_segment(&mut output, &mut writer, &payload, 64)
            .unwrap();
        assert_eq!(sent, 4, "250 bytes in 64-byte chunks");

        let mut assembler = StreamAssembler::new();
        let mut segment = None;
        for _ in 0..sent {
            let event = consumer
                .events
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .expect("a chunk");
            assert!(Node::stream_chunk_of(&event).is_some());
            if let Ok(Some(done)) = assembler.accept_event(event).unwrap() {
                segment = Some(done);
            }
        }
        let segment = segment.expect("the reassembled segment");
        assert_eq!(segment.bytes, payload);
        assert_eq!(segment.chunks, 4);
        assert_eq!(segment.session, "transfer-1");
    }

    #[test]
    fn an_empty_payload_still_sends_one_terminating_chunk() {
        let mut harness = TestHarness::start().unwrap();
        let mut output = harness
            .node
            .raw_output(TestHarness::DEFAULT_OUTPUT)
            .unwrap();
        let mut writer = StreamWriter::new();
        let sent = harness
            .node
            .stream_segment(&mut output, &mut writer, &[], 32)
            .unwrap();
        assert_eq!(sent, 1);

        let sends = harness
            .daemon
            .wait_for_sends(
                harness.node.id(),
                &DataId::new(TestHarness::DEFAULT_OUTPUT).unwrap(),
                1,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(sends[0].metadata.fin(), Some(true));
        assert_eq!(sends[0].payload.len(), 0);
    }

    #[test]
    fn a_zero_chunk_size_is_refused() {
        let mut harness = TestHarness::start().unwrap();
        let mut output = harness
            .node
            .raw_output(TestHarness::DEFAULT_OUTPUT)
            .unwrap();
        let mut writer = StreamWriter::new();
        let error = harness
            .node
            .stream_segment(&mut output, &mut writer, b"data", 0)
            .unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
    }

    #[test]
    fn chunk_references_render_their_place() {
        let chunk = ChunkRef {
            session: "s".to_owned(),
            segment: 2,
            seq: 3,
            fin: true,
            flush: true,
        };
        assert_eq!(chunk.to_string(), "s/2#3 fin flush");
        assert_eq!(chunk.segment_key(), ("s".to_owned(), 2));
        assert_eq!(ChunkRef::from_metadata(&metadata()), None);
    }
}
