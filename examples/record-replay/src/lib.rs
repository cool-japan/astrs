//! Shared types for `record-replay` — blueprint §14's promise, made checkable.
//!
//! > *Replay: `astrs replay session.arec --speed 1.0 --into graph.yml`
//! > re-injects recorded outputs as sources — replacing any subset of nodes
//! > (test a new planner against last week's sensor data **byte-for-byte**).*
//!
//! That sentence is only worth anything if "byte-for-byte" is a measurement.
//! This example makes it one:
//!
//! ```text
//!   live:    astrs/timer ─► [sensor] ─frames─► [detector] ─detections─┐
//!                                  └──frames──────────────────────────┴─► [recorder] ─► session.arec
//!
//!   replay:  session.arec ─► [sensor := astrs-replay-node] ─frames─► [probe] ─► sequence.json
//! ```
//!
//! The live graph writes an `.arec` session holding every payload that
//! crossed `sensor/frames`. The replay graph is the *same* graph with two
//! changes: the sensor is served from the recording instead of from a clock,
//! and the detector is replaced by an assertion sink. The conformance suite
//! then asserts a three-way byte equality:
//!
//! | Sequence | Where it comes from |
//! |---|---|
//! | what the sensor generated | [`frame_payload`], a pure function of the frame index |
//! | what the live run carried | the `.arec` entries for `sensor/frames` |
//! | what the replay delivered | the probe's [`PayloadSequence`] |
//!
//! # Why this edge is untyped
//!
//! Every other example in this estate declares a type URN on every port,
//! and that is the right default (§3.7). Here the claim under test is about
//! *bytes*, so the payload is raw bytes and the port carries no URN: a typed
//! edge would leave room to argue that a mismatch was an encoding detail
//! rather than a lost message. The bytes are self-describing instead — the
//! first eight of every payload are the frame index — so a reordering or a
//! gap is visible in the artefact itself, not only in its length.
//!
//! # What is deliberately *not* asserted
//!
//! Timestamps. `astrs-replay-node` stamps every republished message with a
//! **fresh** HLC from its own clock (replay is a new live event, not a
//! literal replica of history), carrying the rest of the metadata through
//! unchanged. Payload bytes are reproducible; the clock is not, and pretending
//! otherwise would be a test that fails for the wrong reason.

use serde::{Deserialize, Serialize};

/// The sensor's output port, and the detector's and probe's input.
pub const FRAMES_PORT: &str = "frames";

/// The detector's output port.
pub const DETECTIONS_PORT: &str = "detections";

/// The recorder's input carrying frames.
pub const RECORDER_FRAMES_PORT: &str = "frames";

/// The recorder's input carrying detections.
pub const RECORDER_DETECTIONS_PORT: &str = "detections";

/// The sensor's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable naming the `.arec` session the recorder writes.
pub const ENV_SESSION_PATH: &str = "RECORD_REPLAY_SESSION";

/// Environment variable naming the JSON file an observer writes its
/// [`PayloadSequence`] to.
pub const ENV_SEQUENCE_PATH: &str = "RECORD_REPLAY_SEQUENCE";

/// Environment variable overriding how many frames the sensor publishes.
pub const ENV_FRAMES: &str = "RECORD_REPLAY_FRAMES";

/// How many frames the sensor publishes by default.
pub const DEFAULT_FRAMES: u64 = 12;

/// How many bytes each frame payload carries.
pub const FRAME_BYTES: usize = 64;

/// Where the recorder writes its session when the manifest names no path.
#[must_use]
pub fn default_session_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-record-replay-session.arec")
}

/// Where an observer writes its sequence when the manifest names no path.
#[must_use]
pub fn default_sequence_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-record-replay-sequence.json")
}

/// The `.arec` session this run writes.
#[must_use]
pub fn session_path() -> std::path::PathBuf {
    path_from_env(ENV_SESSION_PATH, default_session_path)
}

/// The JSON file this run's observer writes.
#[must_use]
pub fn sequence_path() -> std::path::PathBuf {
    path_from_env(ENV_SEQUENCE_PATH, default_sequence_path)
}

/// How many frames this run should publish.
#[must_use]
pub fn frame_budget() -> u64 {
    std::env::var(ENV_FRAMES)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_FRAMES)
}

/// Reads a path from `name`, falling back to `default` when it is unset or
/// empty.
fn path_from_env(name: &str, default: impl FnOnce() -> std::path::PathBuf) -> std::path::PathBuf {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default, std::path::PathBuf::from)
}

/// The payload the sensor publishes for frame `index`.
///
/// A pure function of the index, so the expected byte sequence of a whole run
/// is available to a test without running anything. The first eight bytes are
/// the index itself, big-endian, which makes every payload self-identifying:
/// a probe that received frames out of order, or missed one, reports *which*
/// rather than only *how many*.
#[must_use]
pub fn frame_payload(index: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(FRAME_BYTES);
    bytes.extend_from_slice(&index.to_be_bytes());
    for offset in bytes.len()..FRAME_BYTES {
        // Deterministic, and different in every byte position of every
        // frame: a payload that was truncated, padded or shifted by one
        // fails the comparison rather than surviving it.
        let mixed = index
            .wrapping_mul(31)
            .wrapping_add(offset as u64)
            .wrapping_mul(131);
        bytes.push((mixed >> 3) as u8);
    }
    bytes
}

/// The frame index encoded in the first eight bytes of `payload`.
///
/// [`None`] if the payload is too short to carry one, which a probe reports
/// as a problem rather than silently treating as frame zero.
#[must_use]
pub fn frame_index_of(payload: &[u8]) -> Option<u64> {
    let head: [u8; 8] = payload.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(head))
}

/// The detection payload the detector derives from one frame.
///
/// Twelve bytes: the frame index it came from, then a checksum of the frame's
/// own bytes. Recorded alongside the frames, so the `.arec` holds two ports
/// and the replay's `--only` filter has something to actually filter.
#[must_use]
pub fn detection_payload(frame: &[u8]) -> Vec<u8> {
    let index = frame_index_of(frame).unwrap_or_default();
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&index.to_be_bytes());
    bytes.extend_from_slice(&checksum(frame).to_be_bytes());
    bytes
}

/// A 32-bit FNV-1a checksum, used only to give detections a payload that
/// depends on the whole frame.
#[must_use]
pub fn checksum(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Lowercase hexadecimal, the form every payload is compared in.
///
/// Hex rather than base64 or a byte array so a failing assertion prints
/// something a human can line up column by column against the recording.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The byte sequence one observer saw on one port, in arrival order.
///
/// Written as JSON when the observer's inputs close. A file rather than a log
/// line because it is what the conformance suite compares: a test that grepped
/// the terminal would be asserting on formatting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadSequence {
    /// The node that observed this sequence.
    pub observer: String,
    /// The input port it observed.
    pub port: String,
    /// How many payloads arrived.
    pub count: u64,
    /// Every payload, hex-encoded, in arrival order.
    pub payloads: Vec<String>,
    /// How many payload bytes arrived in total.
    pub total_bytes: u64,
    /// Whether the observer finished because its inputs closed, rather than
    /// because it was stopped mid-stream.
    pub inputs_closed: bool,
    /// Every frame index that arrived out of order, or whose payload was too
    /// short to carry one.
    pub problems: Vec<String>,
}

impl PayloadSequence {
    /// An empty sequence for `observer` watching `port`.
    #[must_use]
    pub fn new(observer: impl Into<String>, port: impl Into<String>) -> Self {
        Self {
            observer: observer.into(),
            port: port.into(),
            count: 0,
            payloads: Vec::new(),
            total_bytes: 0,
            inputs_closed: false,
            problems: Vec::new(),
        }
    }

    /// Records one arrival, checking its self-declared index against its
    /// position in the stream.
    pub fn push(&mut self, payload: &[u8]) {
        match frame_index_of(payload) {
            Some(index) if index == self.count => {}
            Some(index) => self.problems.push(format!(
                "payload {} declares frame index {index}",
                self.count
            )),
            None => self.problems.push(format!(
                "payload {} is too short to carry an index",
                self.count
            )),
        }
        self.count += 1;
        self.total_bytes += payload.len() as u64;
        self.payloads.push(hex(payload));
    }

    /// A 64-bit FNV-1a digest of the whole sequence, for a one-line summary.
    ///
    /// Only ever a *summary*: every assertion in the conformance suite
    /// compares [`Self::payloads`] itself, so a digest collision could never
    /// make a failing run pass.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for payload in &self.payloads {
            for byte in payload.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        format!("{hash:016x}")
    }

    /// Whether this sequence is exactly the first `frames` payloads the
    /// sensor generates, with nothing missing, extra or reordered.
    #[must_use]
    pub fn matches_generated(&self, frames: u64) -> bool {
        self.problems.is_empty()
            && self.count == frames
            && self
                .payloads
                .iter()
                .enumerate()
                .all(|(index, payload)| *payload == hex(&frame_payload(index as u64)))
    }

    /// Renders the sequence as pretty JSON.
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

    /// A frame payload is a pure function of its index, the declared width,
    /// and carries that index in its own first eight bytes.
    #[test]
    fn frame_payloads_are_deterministic_and_self_identifying() {
        for index in 0..32u64 {
            let payload = frame_payload(index);
            assert_eq!(payload.len(), FRAME_BYTES);
            assert_eq!(frame_index_of(&payload), Some(index));
            assert_eq!(payload, frame_payload(index));
        }
        assert_ne!(frame_payload(3), frame_payload(4));
    }

    /// Two different frames never produce the same detection payload, so a
    /// recording that mixed them up would be visible.
    #[test]
    fn detections_depend_on_the_whole_frame() {
        let first = detection_payload(&frame_payload(1));
        let second = detection_payload(&frame_payload(2));
        assert_eq!(first.len(), 12);
        assert_ne!(first, second);
    }

    /// A payload with no index at all is reported rather than assumed.
    #[test]
    fn a_short_payload_has_no_index() {
        assert_eq!(frame_index_of(&[1, 2, 3]), None);
    }

    #[test]
    fn hex_encodes_every_nibble() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex(&[]), "");
    }

    /// A sequence of the generated frames, in order, matches; one with a
    /// frame missing does not.
    #[test]
    fn a_sequence_matches_only_the_generated_stream() {
        let mut sequence = PayloadSequence::new("probe", FRAMES_PORT);
        for index in 0..4u64 {
            sequence.push(&frame_payload(index));
        }
        assert!(sequence.matches_generated(4));
        assert!(!sequence.matches_generated(5));
        assert!(sequence.problems.is_empty());
        assert_eq!(sequence.total_bytes, 4 * FRAME_BYTES as u64);

        let mut gapped = PayloadSequence::new("probe", FRAMES_PORT);
        gapped.push(&frame_payload(0));
        gapped.push(&frame_payload(2));
        assert!(!gapped.matches_generated(2));
        assert_eq!(gapped.problems.len(), 1, "{gapped:?}");
    }

    /// The digest summarises the sequence and changes when it does.
    #[test]
    fn the_digest_tracks_the_sequence() {
        let mut first = PayloadSequence::new("a", FRAMES_PORT);
        first.push(&frame_payload(0));
        let mut second = PayloadSequence::new("b", FRAMES_PORT);
        second.push(&frame_payload(0));
        assert_eq!(
            first.digest(),
            second.digest(),
            "the observer is not hashed"
        );
        second.push(&frame_payload(1));
        assert_ne!(first.digest(), second.digest());
        assert_eq!(first.digest().len(), 16);
    }

    /// The sequence round-trips as JSON, which is how the suite reads it.
    #[test]
    fn a_sequence_round_trips_as_json() {
        let mut sequence = PayloadSequence::new("probe", FRAMES_PORT);
        sequence.push(&frame_payload(0));
        sequence.inputs_closed = true;
        let json = sequence.to_json().unwrap();
        let parsed: PayloadSequence = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, sequence);
    }

    /// Both artefact paths default under the temporary directory, so a run
    /// never writes into the checkout.
    #[test]
    fn the_artefact_paths_default_to_the_temp_dir() {
        assert!(default_session_path().starts_with(std::env::temp_dir()));
        assert!(default_sequence_path().starts_with(std::env::temp_dir()));
    }
}
