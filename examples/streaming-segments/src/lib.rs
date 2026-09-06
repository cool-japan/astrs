//! Shared types for `streaming-segments` — chunked streaming of large
//! payloads over the `session_id`/`segment_id`/`seq`/`fin` pattern
//! (blueprint §9.4, `astrs_node_api::patterns::stream`).
//!
//! ```text
//!   [streamer] ──chunks──► [collector]
//! ```
//!
//! `streamer` publishes a run of large, deterministic "segments" (a stand-in
//! for a point cloud or a keyframe — anything bigger than one message
//! comfortably carries), each split into fixed-size chunks by
//! [`astrs_node_api::Node::stream_segment`]. `collector` reassembles every
//! segment with an [`astrs_node_api::patterns::stream::StreamAssembler`] and
//! verifies the reassembled bytes against [`segment_payload`] — the same
//! pure function `streamer` used to generate them, so a byte the wire
//! dropped, duplicated or reordered is caught, not merely a chunk *count*.
//!
//! # Why chunks are not eviction-immune
//!
//! A stream chunk carries none of §11.2's correlation keys (`request_id`,
//! `goal_id`/`goal_status`) — `session_id`/`segment_id`/`seq`/`fin` are a
//! different metadata family, deliberately not on that immunity list. A
//! stream that outruns its consumer is meant to drop and have the assembler
//! say so at the gap, rather than silently reassembling the wrong bytes.
//! This example's manifest chooses not to exercise that path: both queues
//! are deep and `backpressure`, so a slow `collector` stalls `streamer`
//! instead of losing a chunk — the same choice `record-replay` makes for
//! its own byte-for-byte claim, and for the same reason.

use serde::{Deserialize, Serialize};

/// The streamer's output port, and the collector's input.
pub const CHUNKS_PORT: &str = "chunks";
/// The streamer's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable overriding how many segments the streamer
/// publishes.
pub const ENV_SEGMENT_COUNT: &str = "STREAMING_SEGMENTS_COUNT";
/// Environment variable naming the JSON file the collector writes its
/// [`StreamTally`] to.
pub const ENV_REPORT_PATH: &str = "STREAMING_SEGMENTS_REPORT";

/// How many segments the streamer publishes by default.
pub const DEFAULT_SEGMENT_COUNT: u64 = 6;
/// How many bytes one segment carries: large enough that no single message
/// in this estate's other examples comes close, small enough that six of
/// them build and verify in well under a second.
pub const SEGMENT_BYTES: usize = 256 * 1024;
/// How many bytes one chunk carries — comfortably above `astrs-shm`'s
/// default zero-copy threshold (4 KiB, §6.2), so a real cluster's chunks are
/// candidates for the shared-memory plane once the route upgrades (§6.3);
/// this example does not assert that engagement (`shm-zero-copy-probe`
/// already owns that claim) but the sizing is chosen so it is possible.
pub const CHUNK_BYTES: usize = 16 * 1024;

/// How many segments this run should publish/expect.
#[must_use]
pub fn segment_count() -> u64 {
    std::env::var(ENV_SEGMENT_COUNT)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_SEGMENT_COUNT)
}

/// Where the collector writes its [`StreamTally`] when the manifest names no
/// path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-streaming-segments-report.json")
}

/// The JSON file this run's collector writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// The payload the streamer publishes for segment `index`, `size` bytes
/// long.
///
/// A pure function of `(index, size)`, so the expected bytes of any segment
/// are available to a test — or to `collector` — without ever running the
/// streamer. The first eight bytes are the index itself, big-endian, making
/// every segment self-identifying: a segment delivered out of order or
/// mixed up with another names itself in the mismatch (the same convention
/// `record-replay::frame_payload` uses, generalised to an arbitrary size).
#[must_use]
pub fn segment_payload(index: u64, size: usize) -> Vec<u8> {
    let header = index.to_be_bytes();
    // Truncates the header itself for a `size` smaller than eight bytes,
    // rather than growing past what the caller asked for — a segment that
    // small carries no recoverable index either way (see
    // `segment_index_of`), but it must still be exactly `size` bytes long.
    let header_len = header.len().min(size);
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&header[..header_len]);
    for offset in bytes.len()..size {
        // Deterministic and different in every byte position of every
        // segment: a reassembly that truncated, padded or shifted a chunk
        // fails the comparison rather than surviving it.
        let mixed = index
            .wrapping_mul(31)
            .wrapping_add(offset as u64)
            .wrapping_mul(131);
        bytes.push((mixed >> 3) as u8);
    }
    bytes
}

/// The segment index encoded in the first eight bytes of `payload`.
///
/// [`None`] if `payload` is too short to carry one.
#[must_use]
pub fn segment_index_of(payload: &[u8]) -> Option<u64> {
    let head: [u8; 8] = payload.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(head))
}

/// The collector's running verdict, and the report it writes once the
/// streamer's budget is spent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamTally {
    /// How many segments were reassembled and verified byte-for-byte.
    pub completed: u64,
    /// Total payload bytes across every verified segment.
    pub total_bytes: u64,
    /// Total chunks it took to deliver every verified segment.
    pub total_chunks: u64,
    /// Everything that went wrong: a reassembly gap the `StreamAssembler`
    /// reported, or a completed segment whose bytes did not match
    /// [`segment_payload`].
    pub problems: Vec<String>,
}

impl StreamTally {
    /// An empty tally.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a completed, reassembled segment: verifies its bytes against
    /// [`segment_payload`] at the writer's own segment index, and tallies it
    /// either as a success or as a problem.
    pub fn accept(&mut self, segment_index: i64, chunks: usize, bytes: &[u8]) {
        let Ok(index) = u64::try_from(segment_index) else {
            self.problem(format!("segment index {segment_index} is negative"));
            return;
        };
        let expected = segment_payload(index, bytes.len());
        if bytes != expected.as_slice() {
            self.problem(format!(
                "segment {index}: reassembled {} bytes do not match the generator",
                bytes.len()
            ));
            return;
        }
        self.completed += 1;
        self.total_bytes += bytes.len() as u64;
        self.total_chunks += chunks as u64;
    }

    /// Records a problem, at most a handful of times — a report with a
    /// thousand identical gap messages is not more informative than one
    /// with eight.
    pub fn problem(&mut self, problem: impl Into<String>) {
        if self.problems.len() < 8 {
            self.problems.push(problem.into());
        }
    }

    /// Whether every expected segment arrived, was reassembled, and
    /// verified — with no problems recorded at all.
    #[must_use]
    pub fn is_clean(&self, expected_segments: u64) -> bool {
        self.problems.is_empty() && self.completed == expected_segments
    }

    /// Renders the tally as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field
    /// types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A segment payload is a pure function of its index and size, and
    /// carries that index in its own first eight bytes.
    #[test]
    fn segment_payloads_are_deterministic_and_self_identifying() {
        for index in 0..8u64 {
            let payload = segment_payload(index, 4096);
            assert_eq!(payload.len(), 4096);
            assert_eq!(segment_index_of(&payload), Some(index));
            assert_eq!(payload, segment_payload(index, 4096));
        }
        assert_ne!(segment_payload(1, 256), segment_payload(2, 256));
    }

    /// The generator honours whatever size it is asked for, including one
    /// too small to hold the eight-byte index header in full.
    #[test]
    fn the_generator_honours_the_requested_size() {
        assert_eq!(segment_payload(9, 0).len(), 0);
        assert_eq!(segment_payload(9, 3).len(), 3);
        assert_eq!(segment_payload(9, SEGMENT_BYTES).len(), SEGMENT_BYTES);
    }

    /// A payload too short to carry an index reports `None` rather than a
    /// guess.
    #[test]
    fn a_short_payload_has_no_index() {
        assert_eq!(segment_index_of(&[1, 2, 3]), None);
    }

    /// A tally that accepts every expected segment, in order, with matching
    /// bytes, is clean.
    #[test]
    fn a_tally_of_every_matching_segment_is_clean() {
        let mut tally = StreamTally::new();
        for index in 0..5i64 {
            let bytes = segment_payload(index as u64, 1024);
            tally.accept(index, 8, &bytes);
        }
        assert!(tally.is_clean(5), "{tally:?}");
        assert_eq!(tally.completed, 5);
        assert_eq!(tally.total_bytes, 5 * 1024);
        assert_eq!(tally.total_chunks, 40);
    }

    /// A segment whose reassembled bytes do not match the generator is
    /// recorded as a problem, not silently counted as completed.
    #[test]
    fn a_corrupted_segment_is_recorded_as_a_problem() {
        let mut tally = StreamTally::new();
        let mut bytes = segment_payload(0, 256);
        bytes[100] ^= 0xff; // flip one bit deep inside the payload
        tally.accept(0, 4, &bytes);
        assert_eq!(tally.completed, 0);
        assert_eq!(tally.problems.len(), 1);
        assert!(!tally.is_clean(1));
    }

    /// An explicit gap report (what `StreamAssembler::accept` returns for a
    /// missing chunk) is recorded the same way a corrupted segment is.
    #[test]
    fn a_reported_gap_is_recorded_as_a_problem() {
        let mut tally = StreamTally::new();
        tally.problem("stream s/0: expected chunk 1, got 2");
        assert!(!tally.is_clean(0));
        assert_eq!(tally.problems.len(), 1);
    }

    /// The problem list caps at eight entries so one badly-behaved run does
    /// not produce an unbounded report.
    #[test]
    fn the_problem_list_is_capped() {
        let mut tally = StreamTally::new();
        for index in 0..20 {
            tally.problem(format!("problem {index}"));
        }
        assert_eq!(tally.problems.len(), 8);
    }

    /// Fewer completed segments than expected is not clean, even with zero
    /// problems recorded — a truncated run is not a proof of anything.
    #[test]
    fn fewer_segments_than_expected_is_not_clean() {
        let mut tally = StreamTally::new();
        let bytes = segment_payload(0, 64);
        tally.accept(0, 1, &bytes);
        assert!(!tally.is_clean(2));
    }

    /// The tally round-trips as JSON.
    #[test]
    fn a_tally_round_trips_as_json() {
        let mut tally = StreamTally::new();
        let bytes = segment_payload(0, 64);
        tally.accept(0, 1, &bytes);
        let json = tally.to_json().unwrap();
        let parsed: StreamTally = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tally);
    }

    /// Both artefact paths default under the temporary directory.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, and names this dataflow.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("streaming-segments"));
        assert_eq!(manifest.nodes.len(), 2);
    }
}
