//! Shared vocabulary for the `shm-zero-copy-probe` example (blueprint §6.2,
//! §6.3, §21 M1).
//!
//! The probe is two node binaries that agree on three things, and this crate
//! is where those three things live so neither side can drift from the other:
//!
//! | Item | Why it is shared |
//! |---|---|
//! | [`ProbeFrame`] | The producer stamps a self-describing header into the ring slot; the consumer verifies it *in place* |
//! | [`ProbeSettings`] | One environment block, read identically by both bins, so a manifest tunes the probe rather than a recompile |
//! | [`ProbeReport`] | The verdict the consumer writes as JSON, which is what the M1 conformance test reads |
//!
//! # What the probe is for
//!
//! Blueprint §21's M1 row asks for zero copy to be *verified by a probe
//! example*, not asserted in a doc comment. "Verified" here means the
//! consumer proves, from facts it can observe itself, that the bytes it read
//! are the very bytes the producer wrote — never a copy of them:
//!
//! 1. **The address is inside the mapping.** [`ProbeFrame::body`] is read
//!    through [`astrs_shm::Sample::payload`], whose address must land at
//!    `mapping_base + layout.payload_offset(slot)`. The mapping base is
//!    computed from an *independent* anchor — the address of the segment's
//!    slot table, minus its layout offset — so the check is not a tautology.
//! 2. **The addresses cycle.** Across `frames` messages the consumer must see
//!    at most `slot_count` distinct payload addresses. A copying path would
//!    hand out a fresh buffer every time.
//! 3. **The sequence stamped in the bytes matches the ring's own sequence.**
//!    The producer writes [`astrs_shm::SampleMut::seq`] *into* the payload
//!    before committing, so a consumer that reads sequence *N* out of the
//!    bytes and is told *N* by the slot header is holding that exact write.
//! 4. **The fingerprint matches.** Recomputed over the body while it is still
//!    mapped, so a torn or re-used slot cannot pass.
//!
//! Virtual addresses themselves are deliberately *not* compared across the
//! two processes: the same physical page is mapped at whatever address each
//! `mmap` chose, so equality would be meaningless and inequality would prove
//! nothing. Offset-within-mapping is the portable form of the same question.

#![allow(clippy::multiple_crate_versions)]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The frame body size the probe defaults to: 4 MiB, the §21 M1 figure.
pub const DEFAULT_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// How many zero-copy frames the probe moves by default.
pub const DEFAULT_FRAMES: u64 = 24;

/// The magic word every probe frame starts with.
pub const FRAME_MAGIC: [u8; 8] = *b"ASTRSZCP";

/// The fixed header length in front of every frame body.
///
/// 128 bytes rather than the 48 the fields need, so the body itself starts
/// 128-byte aligned when the slot base is (§6.1) — the alignment contract a
/// SIMD consumer relies on survives the header.
pub const FRAME_HEADER_LEN: usize = 128;

/// Environment variable naming the file the consumer writes its verdict to.
pub const ENV_REPORT_PATH: &str = "PROBE_REPORT";

/// Environment variable overriding the frame body size in bytes.
pub const ENV_FRAME_BYTES: &str = "PROBE_FRAME_BYTES";

/// Environment variable overriding how many frames the probe moves.
pub const ENV_FRAMES: &str = "PROBE_FRAMES";

/// Environment variable overriding the upgrade wait, in milliseconds.
pub const ENV_UPGRADE_TIMEOUT_MS: &str = "PROBE_UPGRADE_TIMEOUT_MS";

/// The default wait for the slow-start upgrade to arrive (§6.3).
pub const DEFAULT_UPGRADE_TIMEOUT_MS: u64 = 20_000;

/// The output the producer publishes frames on.
pub const FRAMES_PORT: &str = "frames";

/// The output the producer announces its ring incarnation on.
pub const READY_PORT: &str = "ready";

/// The word appended to the announcement published *after* the upgrade lands.
///
/// The announcement has two jobs in one port, and the consumer has to tell
/// them apart by content rather than by arrival order: the ones published
/// *before* the upgrade are the rendezvous that lets the consumer register,
/// and the one published *after* it is the §6.2 threshold check — a
/// sub-threshold message on an output that is on the shared-memory plane.
/// Cross-input arrival order is the event mux's business (§11.2), so it cannot
/// be the discriminator.
pub const ANNOUNCE_UPGRADED: u64 = 0x5A43_5F55_5047_524Du64;

/// How many bytes a post-upgrade announcement carries.
pub const ANNOUNCE_UPGRADED_LEN: usize = 16;

/// Wall-clock nanoseconds since the Unix epoch, saturating at zero.
///
/// The same clock [`astrs_shm::Sample::commit_ns`] is stamped from, which is
/// what makes a producer-to-consumer delivery time computable across two
/// processes at all.
#[must_use]
pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// How the probe was tuned for this run.
///
/// Read from the environment so the committed manifest is the only thing that
/// changes between a smoke run and a soak run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSettings {
    /// Body bytes per frame, excluding [`FRAME_HEADER_LEN`].
    pub frame_bytes: usize,
    /// How many frames to move on the zero-copy plane.
    pub frames: u64,
    /// How long to wait for the §6.3 upgrade before giving up.
    pub upgrade_timeout_ms: u64,
    /// Where the consumer writes [`ProbeReport`].
    pub report_path: PathBuf,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            frame_bytes: DEFAULT_FRAME_BYTES,
            frames: DEFAULT_FRAMES,
            upgrade_timeout_ms: DEFAULT_UPGRADE_TIMEOUT_MS,
            report_path: std::env::temp_dir().join("astrs-shm-zero-copy-probe.json"),
        }
    }
}

impl ProbeSettings {
    /// Reads the settings from the environment, falling back to the defaults.
    ///
    /// An unparseable value is *ignored* rather than fatal: the probe's job is
    /// to report on the data plane, and refusing to start because someone
    /// typed `PROBE_FRAMES=lots` would be a worse failure than running the
    /// default sweep and saying so in the report.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            frame_bytes: env_usize(ENV_FRAME_BYTES).unwrap_or(defaults.frame_bytes),
            frames: env_u64(ENV_FRAMES).unwrap_or(defaults.frames),
            upgrade_timeout_ms: env_u64(ENV_UPGRADE_TIMEOUT_MS)
                .unwrap_or(defaults.upgrade_timeout_ms),
            report_path: std::env::var(ENV_REPORT_PATH)
                .ok()
                .filter(|value| !value.is_empty())
                .map_or(defaults.report_path, PathBuf::from),
        }
    }

    /// The total bytes one frame occupies in a slot.
    #[must_use]
    pub const fn slot_bytes(&self) -> usize {
        self.frame_bytes + FRAME_HEADER_LEN
    }
}

/// Parses an environment variable as a `u64`, or [`None`].
fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Parses an environment variable as a `usize`, or [`None`].
fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// One probe frame's header, as written into the ring slot.
///
/// Encoded little-endian by hand rather than through the columnar encoder:
/// the point of the probe is the *memory path*, and a hand-rolled 128-byte
/// header keeps the bytes the consumer verifies identical to the bytes the
/// producer wrote, with no encoder in between to explain away a mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeFrame {
    /// Which frame this is, counted from zero.
    pub index: u64,
    /// The ring sequence number the producer committed this frame under.
    pub seq: u64,
    /// The body length in bytes, excluding the header.
    pub body_len: u64,
    /// A fold of the body, recomputed by the consumer.
    pub fingerprint: u64,
    /// [`now_ns`] at the moment the producer finished writing the body.
    pub written_ns: u64,
}

/// Why a frame could not be read as a [`ProbeFrame`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// The buffer is shorter than [`FRAME_HEADER_LEN`].
    TooShort {
        /// How many bytes were there.
        len: usize,
    },
    /// The buffer does not start with [`FRAME_MAGIC`].
    BadMagic,
    /// The header's body length does not match the bytes present.
    LengthMismatch {
        /// What the header claimed.
        declared: u64,
        /// What the buffer actually holds after the header.
        present: usize,
    },
    /// The body does not fold to the fingerprint in the header.
    FingerprintMismatch {
        /// What the header claimed.
        declared: u64,
        /// What the body folds to.
        computed: u64,
    },
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort { len } => {
                write!(
                    f,
                    "a probe frame needs {FRAME_HEADER_LEN} header bytes, got {len}"
                )
            }
            Self::BadMagic => f.write_str("the buffer does not start with the probe magic word"),
            Self::LengthMismatch { declared, present } => write!(
                f,
                "the header declares a {declared}-byte body but {present} bytes are present"
            ),
            Self::FingerprintMismatch { declared, computed } => write!(
                f,
                "the body folds to {computed:#018x}, the header declares {declared:#018x}"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

impl ProbeFrame {
    /// Writes `self` and a deterministic body into `slot`.
    ///
    /// The body is generated straight into the destination — the whole point
    /// of [`astrs_node_api::RawOutput::allocate`] is that there is no staging
    /// buffer to copy from — and the fingerprint is folded as it is written,
    /// so the frame costs one pass over the bytes rather than two.
    ///
    /// Returns the completed header (with `fingerprint` filled in).
    ///
    /// # Errors
    ///
    /// [`FrameError::TooShort`] when `slot` cannot hold the header, and
    /// [`FrameError::LengthMismatch`] when it cannot hold the declared body.
    pub fn write_into(mut self, slot: &mut [u8]) -> Result<Self, FrameError> {
        if slot.len() < FRAME_HEADER_LEN {
            return Err(FrameError::TooShort { len: slot.len() });
        }
        let (header, body) = slot.split_at_mut(FRAME_HEADER_LEN);
        let declared = usize::try_from(self.body_len).unwrap_or(usize::MAX);
        if body.len() != declared {
            return Err(FrameError::LengthMismatch {
                declared: self.body_len,
                present: body.len(),
            });
        }

        // A body every consumer can regenerate from the frame index alone:
        // an 8-byte counter stream seeded by the index. Cheap to write, and
        // a single flipped bit changes the fold.
        let mut fold = FOLD_SEED;
        for (chunk_index, chunk) in body.chunks_mut(8).enumerate() {
            let word = word_for(self.index, chunk_index as u64);
            let bytes = word.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
            fold = fold_bytes(fold, &bytes[..chunk.len()]);
        }
        self.fingerprint = fold;
        self.written_ns = now_ns();

        header.fill(0);
        header[..8].copy_from_slice(&FRAME_MAGIC);
        header[8..16].copy_from_slice(&self.index.to_le_bytes());
        header[16..24].copy_from_slice(&self.seq.to_le_bytes());
        header[24..32].copy_from_slice(&self.body_len.to_le_bytes());
        header[32..40].copy_from_slice(&self.fingerprint.to_le_bytes());
        header[40..48].copy_from_slice(&self.written_ns.to_le_bytes());
        Ok(self)
    }

    /// Reads a frame header out of `bytes` without copying the body.
    ///
    /// # Errors
    ///
    /// [`FrameError::TooShort`] or [`FrameError::BadMagic`] when the buffer is
    /// not a probe frame, and [`FrameError::LengthMismatch`] when the header
    /// disagrees with the buffer it was read from.
    pub fn read_from(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() < FRAME_HEADER_LEN {
            return Err(FrameError::TooShort { len: bytes.len() });
        }
        if bytes[..8] != FRAME_MAGIC {
            return Err(FrameError::BadMagic);
        }
        let frame = Self {
            index: read_u64(bytes, 8),
            seq: read_u64(bytes, 16),
            body_len: read_u64(bytes, 24),
            fingerprint: read_u64(bytes, 32),
            written_ns: read_u64(bytes, 40),
        };
        let present = bytes.len() - FRAME_HEADER_LEN;
        if usize::try_from(frame.body_len).unwrap_or(usize::MAX) != present {
            return Err(FrameError::LengthMismatch {
                declared: frame.body_len,
                present,
            });
        }
        Ok(frame)
    }

    /// The body of a frame that starts at `bytes[0]`.
    ///
    /// A borrow, never a copy: the caller is expected to be looking straight
    /// at a mapped ring slot.
    #[must_use]
    pub fn body(bytes: &[u8]) -> &[u8] {
        bytes.get(FRAME_HEADER_LEN..).unwrap_or_default()
    }

    /// Verifies that `body` folds to this frame's fingerprint.
    ///
    /// Reads `body` where it lies; nothing is copied out of the mapping.
    ///
    /// # Errors
    ///
    /// [`FrameError::FingerprintMismatch`] when the bytes are not the bytes
    /// the producer folded.
    pub fn verify_body(&self, body: &[u8]) -> Result<(), FrameError> {
        let mut fold = FOLD_SEED;
        for chunk in body.chunks(8) {
            fold = fold_bytes(fold, chunk);
        }
        if fold == self.fingerprint {
            Ok(())
        } else {
            Err(FrameError::FingerprintMismatch {
                declared: self.fingerprint,
                computed: fold,
            })
        }
    }
}

/// The seed of the body fold — FNV-1a's 64-bit offset basis.
const FOLD_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a's 64-bit prime.
const FOLD_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Folds `bytes` into `fold` (FNV-1a).
fn fold_bytes(mut fold: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        fold ^= u64::from(*byte);
        fold = fold.wrapping_mul(FOLD_PRIME);
    }
    fold
}

/// The body word at `position` of frame `index`.
const fn word_for(index: u64, position: u64) -> u64 {
    index
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(position.wrapping_mul(0xff51_afd7_ed55_8ccd))
}

/// Reads a little-endian `u64` at `offset`, or zero past the end.
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    bytes
        .get(offset..offset + 8)
        .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
        .map_or(0, u64::from_le_bytes)
}

/// The consumer's verdict, written as JSON for the M1 conformance test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    /// Whether every zero-copy invariant held for every frame.
    pub zero_copy_engaged: bool,
    /// How many frames were read straight out of the ring.
    pub frames_verified: u64,
    /// Body bytes per frame.
    pub frame_bytes: u64,
    /// Total payload bytes moved without a copy.
    pub bytes_moved: u64,
    /// How many distinct payload addresses were observed.
    pub distinct_addresses: u64,
    /// How many slots the ring holds.
    pub slot_count: u32,
    /// The segment the frames arrived in.
    pub segment: String,
    /// The producer incarnation that owned the segment.
    pub generation: u64,
    /// Every payload address landed at its slot's layout offset.
    pub addresses_match_layout: bool,
    /// Every payload address was inside the mapping.
    pub addresses_inside_mapping: bool,
    /// Every in-band sequence matched the ring's own sequence.
    pub sequences_match: bool,
    /// Every body folded to the fingerprint the producer wrote.
    pub fingerprints_match: bool,
    /// The session attached to the ring by itself, with no segment plumbing
    /// in the node (§6.3's `InputRouteUpgrade`).
    ///
    /// Always `true` for a run that got as far as verifying a frame: the
    /// consumer has no other way to obtain a mapped payload any more. It is
    /// recorded rather than assumed because the field is what a reader of the
    /// JSON asks first — *did the middleware do this, or did the example?*
    #[serde(default)]
    pub attached_automatically: bool,
    /// A sub-threshold message published *after* the output moved to the ring
    /// still arrived (§6.2's below-threshold rule).
    ///
    /// The one case where an upgraded route has two carriers, and the one that
    /// silently lost messages before the consumer side was wired.
    #[serde(default)]
    pub announcement_after_upgrade: bool,
    /// Mean producer-commit-to-consumer-read time, in microseconds.
    pub mean_delivery_us: f64,
    /// The slowest producer-commit-to-consumer-read time, in microseconds.
    pub max_delivery_us: f64,
    /// Mean in-place verification time per frame, in microseconds.
    pub mean_verify_us: f64,
    /// Effective throughput over the zero-copy phase, in mebibytes/second.
    pub throughput_mib_s: f64,
    /// Anything that went wrong, in the order it was noticed.
    pub problems: Vec<String>,
}

impl ProbeReport {
    /// A report that explains why the probe could not run at all.
    #[must_use]
    pub fn failed(problem: impl Into<String>) -> Self {
        Self {
            zero_copy_engaged: false,
            frames_verified: 0,
            frame_bytes: 0,
            bytes_moved: 0,
            distinct_addresses: 0,
            slot_count: 0,
            segment: String::new(),
            generation: 0,
            addresses_match_layout: false,
            addresses_inside_mapping: false,
            sequences_match: false,
            fingerprints_match: false,
            attached_automatically: false,
            announcement_after_upgrade: false,
            mean_delivery_us: 0.0,
            max_delivery_us: 0.0,
            mean_verify_us: 0.0,
            throughput_mib_s: 0.0,
            problems: vec![problem.into()],
        }
    }

    /// Renders the report as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] only if the report itself cannot be serialised,
    /// which its field types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// A one-screen human summary, the shape the example prints to stdout.
    #[must_use]
    pub fn summary(&self) -> String {
        let verdict = if self.zero_copy_engaged {
            "ENGAGED"
        } else {
            "NOT ENGAGED"
        };
        let mut text = format!(
            "zero-copy: {verdict}\n\
             frames:    {frames} x {bytes} B = {moved:.1} MiB\n\
             segment:   {segment} (generation {generation}, {slots} slots)\n\
             addresses: {distinct} distinct / {slots} slots, layout {layout}, in-mapping {inside}\n\
             identity:  sequences {sequences}, fingerprints {fingerprints}\n\
             delivery:  mean {mean:.1} us, max {max:.1} us\n\
             verify:    mean {verify:.1} us in place\n\
             through:   {throughput:.0} MiB/s\n",
            frames = self.frames_verified,
            bytes = self.frame_bytes,
            moved = self.bytes_moved as f64 / (1024.0 * 1024.0),
            segment = if self.segment.is_empty() {
                "-"
            } else {
                &self.segment
            },
            generation = self.generation,
            slots = self.slot_count,
            distinct = self.distinct_addresses,
            layout = yes_no(self.addresses_match_layout),
            inside = yes_no(self.addresses_inside_mapping),
            sequences = yes_no(self.sequences_match),
            fingerprints = yes_no(self.fingerprints_match),
            mean = self.mean_delivery_us,
            max = self.max_delivery_us,
            verify = self.mean_verify_us,
            throughput = self.throughput_mib_s,
        );
        for problem in &self.problems {
            text.push_str("problem:   ");
            text.push_str(problem);
            text.push('\n');
        }
        text
    }
}

/// `yes`/`no`, for the summary table.
const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A frame written into a buffer reads back with every field intact.
    #[test]
    fn a_frame_round_trips_through_a_buffer() {
        let body_len = 4096;
        let mut slot = vec![0_u8; FRAME_HEADER_LEN + body_len];
        let written = ProbeFrame {
            index: 7,
            seq: 42,
            body_len: body_len as u64,
            fingerprint: 0,
            written_ns: 0,
        }
        .write_into(&mut slot)
        .unwrap();

        let read = ProbeFrame::read_from(&slot).unwrap();
        assert_eq!(read.index, 7);
        assert_eq!(read.seq, 42);
        assert_eq!(read.body_len, body_len as u64);
        assert_eq!(read.fingerprint, written.fingerprint);
        assert!(read.written_ns > 0);
        read.verify_body(ProbeFrame::body(&slot)).unwrap();
    }

    /// The body starts at a 128-byte offset, so a 128-byte-aligned slot base
    /// keeps the body aligned too (§6.1).
    #[test]
    fn the_header_preserves_body_alignment() {
        assert_eq!(FRAME_HEADER_LEN % 128, 0);
    }

    /// A single flipped bit in the body fails verification.
    #[test]
    fn a_corrupted_body_fails_verification() {
        let mut slot = vec![0_u8; FRAME_HEADER_LEN + 512];
        let frame = ProbeFrame {
            index: 1,
            seq: 1,
            body_len: 512,
            fingerprint: 0,
            written_ns: 0,
        }
        .write_into(&mut slot)
        .unwrap();
        slot[FRAME_HEADER_LEN + 100] ^= 0x01;
        let error = frame.verify_body(ProbeFrame::body(&slot)).unwrap_err();
        assert!(matches!(error, FrameError::FingerprintMismatch { .. }));
    }

    /// Two frames with different indices have different bodies.
    #[test]
    fn frames_differ_by_index() {
        let mut first = vec![0_u8; FRAME_HEADER_LEN + 256];
        let mut second = vec![0_u8; FRAME_HEADER_LEN + 256];
        let a = ProbeFrame {
            index: 0,
            seq: 0,
            body_len: 256,
            fingerprint: 0,
            written_ns: 0,
        }
        .write_into(&mut first)
        .unwrap();
        let b = ProbeFrame {
            index: 1,
            seq: 1,
            body_len: 256,
            fingerprint: 0,
            written_ns: 0,
        }
        .write_into(&mut second)
        .unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert!(a.verify_body(ProbeFrame::body(&second)).is_err());
    }

    /// A buffer that is not a probe frame is rejected, not misread.
    #[test]
    fn a_foreign_buffer_is_rejected() {
        assert!(matches!(
            ProbeFrame::read_from(&[0_u8; 16]),
            Err(FrameError::TooShort { len: 16 })
        ));
        assert!(matches!(
            ProbeFrame::read_from(&[0_u8; FRAME_HEADER_LEN + 8]),
            Err(FrameError::BadMagic)
        ));
    }

    /// A short slot is refused rather than silently truncating the frame.
    #[test]
    fn a_short_slot_is_refused() {
        let mut slot = vec![0_u8; 64];
        let error = ProbeFrame {
            index: 0,
            seq: 0,
            body_len: 0,
            fingerprint: 0,
            written_ns: 0,
        }
        .write_into(&mut slot)
        .unwrap_err();
        assert!(matches!(error, FrameError::TooShort { len: 64 }));
    }

    /// The settings come from the environment, and the defaults are the §21
    /// M1 figures.
    #[test]
    fn the_defaults_are_the_milestone_figures() {
        let settings = ProbeSettings::default();
        assert_eq!(settings.frame_bytes, 4 * 1024 * 1024);
        assert_eq!(settings.frames, 24);
        assert_eq!(settings.slot_bytes(), 4 * 1024 * 1024 + FRAME_HEADER_LEN);
    }

    /// A failed report serialises, and says so.
    #[test]
    fn a_failed_report_serialises() {
        let report = ProbeReport::failed("no upgrade arrived");
        assert!(!report.zero_copy_engaged);
        let json = report.to_json().unwrap();
        let parsed: ProbeReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
        assert!(report.summary().contains("NOT ENGAGED"));
    }
}
