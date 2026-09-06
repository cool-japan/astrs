//! The `.arec` binary layout (blueprint §14, format version 1).
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │ FILE_MAGIC "ASTRSREC" (8) │ FORMAT_VERSION u16 │ HEADER frame        │
//! ├──────────────────────────────────────────────────────────────────────┤
//! │ ENTRY frame │ ENTRY frame │ ... │ ENTRY frame  (any order on disk)   │
//! ├──────────────────────────────────────────────────────────────────────┤
//! │ FOOTER frame (the seekable index)                                    │
//! ├──────────────────────────────────────────────────────────────────────┤
//! │ TRAILER (24 bytes, fixed size, always the last bytes of the file)   │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # The generic frame
//!
//! The header, every entry, and the footer are all instances of one
//! self-delimiting, self-checking shape — [`write_frame`] and
//! [`read_frame`] are the only place that shape is encoded or decoded:
//!
//! ```text
//! ┌───────────┬───────┬────────────┬───────────────────┬─────────────┐
//! │ tag: u32  │ flags │ body_len:u32│ body (raw or zstd)│ crc32c: u32 │
//! │ LE        │ u8    │ LE          │                    │ LE          │
//! └───────────┴───────┴────────────┴───────────────────┴─────────────┘
//! ```
//!
//! `flags` bit 0 means the body is zstd-compressed; every other bit is
//! reserved and must be zero on write (a reader ignores unknown bits
//! rather than rejecting them, per the append-only evolution rule other
//! AstRS wire formats follow). The CRC covers the tag, flags and
//! `body_len` fields as well as the body itself, so a corrupted length
//! prefix is caught exactly like a corrupted payload — never mistaken for
//! "not enough bytes yet".
//!
//! Each field is written little-endian; every multi-byte integer in this
//! module is `u32`/`u64` LE, matching the rest of the AstRS wire (§7.1).
//!
//! # Why entries are compressed whole, not payload-only
//!
//! The blueprint (§14) describes zstd-**framed entry blocks** of `{node,
//! output, hlc, meta, payload}` — the whole tuple, not the payload alone.
//! Compressing the oxicode encoding of the entire [`crate::Entry`]
//! (rather than just its payload) lets a run of same-shaped metadata
//! across consecutive entries from one high-rate topic compress too, and
//! keeps exactly one codec decision (worth it or not, §6.4's own lesson)
//! per frame instead of two.
//!
//! # Recovery contract
//!
//! [`read_frame`] never panics and never over-allocates on a forged or
//! truncated length: [`RecordingError::Incomplete`] is returned as soon
//! as the declared body length would run past the bytes actually on hand,
//! *before* any body-sized buffer is touched. This is what makes
//! [`crate::recover::scan`] safe to point at an arbitrarily-truncated file
//! (see that module for the full recovery algorithm).

use astrs_wire::crc32c::Crc32c;

use crate::error::RecordingError;

/// The literal byte string every `.arec` file opens with.
pub const FILE_MAGIC: &[u8; 8] = b"ASTRSREC";

/// The container format version this build writes, and the highest one it
/// reads.
pub const FORMAT_VERSION: u16 = 1;

/// The frame tag of the one header frame, immediately after
/// [`FILE_MAGIC`] and the format version.
pub const HEADER_FRAME_TAG: u32 = u32::from_le_bytes(*b"AHd1");

/// The frame tag of a data entry.
pub const ENTRY_FRAME_TAG: u32 = u32::from_le_bytes(*b"AEn1");

/// The frame tag of the trailing index footer.
pub const FOOTER_FRAME_TAG: u32 = u32::from_le_bytes(*b"AFt1");

/// The flag bit marking a frame body as zstd-compressed.
pub const FLAG_ZSTD: u8 = 0b0000_0001;

/// The zstd compression level entry and footer bodies are written at.
///
/// Level 0 is not usable: `oxiarc_zstd::compress` at level 0 emits raw/RLE
/// blocks only and reliably *grows* its input by the frame's own overhead
/// (proven by `astrs-transport`'s own regression test for exactly this
/// trap). Level 3 is zstd's own "fast, still real" default.
pub const ZSTD_LEVEL: i32 = 3;

/// The fixed-size trailer written as the very last bytes of the file.
///
/// ```text
/// ┌────────────────────┬──────────────────┬──────────────────┬─────────────┐
/// │ magic "ASTRSEND"(8)│ footer_offset:u64│ footer_len: u32  │ crc32c: u32 │
/// └────────────────────┴──────────────────┴──────────────────┴─────────────┘
/// ```
pub const TRAILER_LEN: u64 = 24;

/// The literal byte string the trailer opens with.
pub const TRAILER_MAGIC: &[u8; 8] = b"ASTRSEND";

/// The fixed overhead of a generic frame: tag(4) + flags(1) + body_len(4) +
/// crc(4).
pub const FRAME_OVERHEAD: usize = 13;

/// The length of a frame's fixed mini-header: tag(4) + flags(1) +
/// body_len(4) — everything a caller reading by seeking (rather than
/// from an already fully-buffered slice) needs before it knows how many
/// more bytes (body + crc) to fetch.
pub const MINI_HEADER_LEN: usize = 9;

/// The length of the fixed prologue every `.arec` file opens with:
/// [`FILE_MAGIC`] plus a two-byte format version, before the header's own
/// frame.
pub const PROLOGUE_LEN: u64 = FILE_MAGIC.len() as u64 + 2;

/// Validates a prologue's magic and format version.
///
/// `prologue` must be exactly [`PROLOGUE_LEN`] bytes — callers reading
/// from a file or from an in-memory buffer both check that length
/// themselves first, since the two report a mismatch differently
/// ([`RecordingError::TooShort`] vs. a plain short read).
///
/// # Errors
///
/// [`RecordingError::BadMagic`] if the magic does not match, or
/// [`RecordingError::UnsupportedVersion`] if the declared format version
/// is newer than [`FORMAT_VERSION`].
pub fn check_prologue(prologue: &[u8]) -> Result<(), RecordingError> {
    debug_assert_eq!(prologue.len(), PROLOGUE_LEN as usize);
    if &prologue[..FILE_MAGIC.len()] != FILE_MAGIC {
        return Err(RecordingError::BadMagic {
            reason: "missing the ASTRSREC magic",
        });
    }
    let version = u16::from_le_bytes([prologue[FILE_MAGIC.len()], prologue[FILE_MAGIC.len() + 1]]);
    if version > FORMAT_VERSION {
        return Err(RecordingError::UnsupportedVersion {
            found: version,
            understood: FORMAT_VERSION,
        });
    }
    Ok(())
}

/// Computes the CRC-32C of one frame's tag, flags, declared body length and
/// body, in the same order [`write_frame`] lays them on disk.
#[must_use]
fn frame_crc(tag: u32, flags: u8, body: &[u8]) -> u32 {
    let mut hasher = Crc32c::new();
    hasher.update(&tag.to_le_bytes());
    hasher.update(&[flags]);
    hasher.update(&(body.len() as u32).to_le_bytes());
    hasher.update(body);
    hasher.finalize()
}

/// Appends the fixed trailer pointing at the footer frame that starts at
/// `footer_offset` and spans `footer_len` bytes.
pub fn write_trailer(out: &mut Vec<u8>, footer_offset: u64, footer_len: u32) {
    let mut hasher = Crc32c::new();
    hasher.update(&footer_offset.to_le_bytes());
    hasher.update(&footer_len.to_le_bytes());

    out.extend_from_slice(TRAILER_MAGIC);
    out.extend_from_slice(&footer_offset.to_le_bytes());
    out.extend_from_slice(&footer_len.to_le_bytes());
    out.extend_from_slice(&hasher.finalize().to_le_bytes());
}

/// The footer's location, as recorded in a trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrailerInfo {
    /// The footer frame's absolute byte offset.
    pub footer_offset: u64,
    /// The footer frame's total length on disk.
    pub footer_len: u32,
}

/// Reads a trailer from the exact last [`TRAILER_LEN`] bytes of a file.
///
/// `tail` must be exactly [`TRAILER_LEN`] bytes — [`crate::reader::Reader`]
/// is the only caller, and it always reads exactly that many bytes from
/// the file's end first.
///
/// # Errors
///
/// [`RecordingError::NoTrailer`] if the magic or checksum does not match —
/// the normal, expected outcome for a file that was never finalized
/// (crashed mid-write) or is shorter than a trailer, and the signal
/// [`crate::Reader::open_or_recover`] uses to fall back to a full scan.
pub fn read_trailer(tail: &[u8]) -> Result<TrailerInfo, RecordingError> {
    if tail.len() != TRAILER_LEN as usize || &tail[..8] != TRAILER_MAGIC {
        return Err(RecordingError::NoTrailer);
    }
    let footer_offset = u64::from_le_bytes(tail[8..16].try_into().unwrap_or([0; 8]));
    let footer_len = u32::from_le_bytes(tail[16..20].try_into().unwrap_or([0; 4]));
    let expected_crc = u32::from_le_bytes(tail[20..24].try_into().unwrap_or([0; 4]));

    let mut hasher = Crc32c::new();
    hasher.update(&footer_offset.to_le_bytes());
    hasher.update(&footer_len.to_le_bytes());
    if hasher.finalize() != expected_crc {
        return Err(RecordingError::NoTrailer);
    }
    Ok(TrailerInfo {
        footer_offset,
        footer_len,
    })
}

/// Compresses `plain` at [`ZSTD_LEVEL`], falling back to storing it raw if
/// compression failed to produce anything smaller (a tiny or already-dense
/// body, mirroring `astrs-transport`'s "never pay for a loss" rule).
///
/// Returns the bytes to place in the frame body and whether [`FLAG_ZSTD`]
/// should be set.
fn compress_for_frame(plain: &[u8]) -> (Vec<u8>, u8) {
    match oxiarc_zstd::compress_with_level(plain, ZSTD_LEVEL) {
        Ok(compressed) if compressed.len() < plain.len() => (compressed, FLAG_ZSTD),
        _ => (plain.to_vec(), 0),
    }
}

/// Appends one generic frame carrying `plain` (compressed, when that helps)
/// under `tag` to `out`.
///
/// # Errors
///
/// Never fails today — compression failure degrades to storing the body
/// raw (see `compress_for_frame`) — but returns a [`Result`] so a future
/// codec that can genuinely fail does not need a signature change.
pub fn write_frame(out: &mut Vec<u8>, tag: u32, plain: &[u8]) -> Result<(), RecordingError> {
    let (body, flags) = compress_for_frame(plain);
    let body_len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&tag.to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&frame_crc(tag, flags, &body).to_le_bytes());
    Ok(())
}

/// One successfully parsed frame.
#[derive(Debug, Clone)]
pub struct ParsedFrame {
    /// The frame's tag.
    pub tag: u32,
    /// The decompressed body.
    pub body: Vec<u8>,
    /// How many bytes of the input this frame consumed.
    pub consumed: usize,
}

/// Reads one frame from the start of `bytes`.
///
/// `offset` is only used to annotate errors with the frame's absolute
/// position in the file — the parse itself is a pure function of `bytes`.
///
/// # Errors
///
/// - [`RecordingError::Incomplete`] if fewer bytes are available than the
///   frame's own header declares, checked *before* any allocation sized
///   from the declared length — the load-bearing property for
///   [`crate::recover::scan`].
/// - [`RecordingError::CrcMismatch`] if the frame's integrity check fails.
/// - [`RecordingError::Codec`] if the body is flagged zstd-compressed and
///   the codec rejects it.
pub fn read_frame(bytes: &[u8], offset: u64) -> Result<ParsedFrame, RecordingError> {
    if bytes.len() < FRAME_OVERHEAD {
        return Err(RecordingError::Incomplete {
            offset,
            needed: FRAME_OVERHEAD,
            available: bytes.len(),
        });
    }
    // `FRAME_OVERHEAD` bytes were just confirmed present, so every fixed
    // slice below is in range.
    let tag = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let flags = bytes[4];
    let body_len = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;

    let total_len = FRAME_OVERHEAD
        .checked_add(body_len)
        .ok_or(RecordingError::Incomplete {
            offset,
            needed: usize::MAX,
            available: bytes.len(),
        })?;
    if bytes.len() < total_len {
        return Err(RecordingError::Incomplete {
            offset,
            needed: total_len,
            available: bytes.len(),
        });
    }

    let body_raw = &bytes[9..9 + body_len];
    let crc_bytes = &bytes[9 + body_len..total_len];
    let expected_crc = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    if frame_crc(tag, flags, body_raw) != expected_crc {
        return Err(RecordingError::CrcMismatch { offset });
    }

    let body = if flags & FLAG_ZSTD != 0 {
        oxiarc_zstd::decompress(body_raw).map_err(|err| RecordingError::Codec(err.to_string()))?
    } else {
        body_raw.to_vec()
    };

    Ok(ParsedFrame {
        tag,
        body,
        consumed: total_len,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::error::RecordingError;

    #[test]
    fn a_frame_round_trips_small_and_large_bodies() {
        for len in [0usize, 1, 8, 4_096, 100_000] {
            let plain: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut out = Vec::new();
            write_frame(&mut out, ENTRY_FRAME_TAG, &plain).unwrap();
            let parsed = read_frame(&out, 0).unwrap();
            assert_eq!(parsed.tag, ENTRY_FRAME_TAG);
            assert_eq!(parsed.body, plain, "len {len}");
            assert_eq!(parsed.consumed, out.len());
        }
    }

    #[test]
    fn a_tiny_body_is_not_compressed() {
        let mut out = Vec::new();
        write_frame(&mut out, ENTRY_FRAME_TAG, b"hi").unwrap();
        // tag(4) + flags(1) + len(4) + body(2) + crc(4) = 15, and flags must
        // be zero: zstd would only add overhead here.
        assert_eq!(out.len(), FRAME_OVERHEAD + 2);
        assert_eq!(out[4], 0, "flags byte must show no compression");
    }

    #[test]
    fn a_compressible_body_is_compressed() {
        let plain = vec![7u8; 64 * 1024];
        let mut out = Vec::new();
        write_frame(&mut out, ENTRY_FRAME_TAG, &plain).unwrap();
        assert!(out.len() < plain.len() / 4, "expected real compression");
        let parsed = read_frame(&out, 0).unwrap();
        assert_eq!(parsed.body, plain);
    }

    #[test]
    fn truncation_at_every_prefix_length_is_incomplete_not_a_panic() {
        let plain = vec![9u8; 5_000];
        let mut out = Vec::new();
        write_frame(&mut out, ENTRY_FRAME_TAG, &plain).unwrap();
        for cut in 0..out.len() {
            match read_frame(&out[..cut], 0) {
                Err(RecordingError::Incomplete { .. }) => {}
                other => panic!("cut at {cut} of {} gave {other:?}", out.len()),
            }
        }
        // The full frame, of course, parses.
        assert!(read_frame(&out, 0).is_ok());
    }

    #[test]
    fn a_corrupted_byte_in_the_body_is_a_crc_mismatch() {
        // Incompressible (a cheap xorshift PRNG, not real randomness —
        // just enough to defeat zstd) so the body is stored raw: a byte
        // flip inside it is then guaranteed to land inside the frame's
        // own bytes, rather than possibly past the end of a much shorter
        // compressed frame.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let plain: Vec<u8> = (0..100)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xff) as u8
            })
            .collect();
        let mut out = Vec::new();
        write_frame(&mut out, ENTRY_FRAME_TAG, &plain).unwrap();
        let mid = FRAME_OVERHEAD + plain.len() / 2;
        assert!(mid < out.len(), "body must not have compressed away");
        out[mid] ^= 0xFF;
        assert!(matches!(
            read_frame(&out, 0),
            Err(RecordingError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn a_corrupted_length_field_is_caught_by_the_crc() {
        let plain = vec![3u8; 100];
        let mut out = Vec::new();
        write_frame(&mut out, ENTRY_FRAME_TAG, &plain).unwrap();
        // Flip a bit in the body_len field. If it happens to still describe
        // enough bytes to be structurally "complete", the CRC must catch it
        // instead; if it now claims more bytes than exist, Incomplete is
        // equally acceptable — either way, never a panic and never a
        // silently-accepted forged frame.
        out[5] ^= 0x01;
        match read_frame(&out, 0) {
            Err(RecordingError::CrcMismatch { .. } | RecordingError::Incomplete { .. }) => {}
            other => panic!("expected a typed rejection, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_declared_length_does_not_allocate_before_checking() {
        // A forged header claiming close to u32::MAX bytes must be refused
        // by the length check alone, using only the handful of bytes
        // actually supplied — never by trying to slice or allocate that
        // many bytes first.
        let mut forged = Vec::new();
        forged.extend_from_slice(&ENTRY_FRAME_TAG.to_le_bytes());
        forged.push(0);
        forged.extend_from_slice(&(u32::MAX - 1).to_le_bytes());
        assert!(matches!(
            read_frame(&forged, 0),
            Err(RecordingError::Incomplete { .. })
        ));
    }

    #[test]
    fn frame_tags_are_distinct() {
        let tags = [HEADER_FRAME_TAG, ENTRY_FRAME_TAG, FOOTER_FRAME_TAG];
        for (i, a) in tags.iter().enumerate() {
            for (j, b) in tags.iter().enumerate() {
                assert_eq!(i == j, a == b);
            }
        }
    }

    #[test]
    fn a_trailer_round_trips() {
        let mut out = Vec::new();
        write_trailer(&mut out, 12_345, 678);
        assert_eq!(out.len(), TRAILER_LEN as usize);
        let info = read_trailer(&out).unwrap();
        assert_eq!(info.footer_offset, 12_345);
        assert_eq!(info.footer_len, 678);
    }

    #[test]
    fn a_wrong_length_tail_is_no_trailer() {
        assert!(matches!(
            read_trailer(&vec![0u8; TRAILER_LEN as usize - 1]),
            Err(RecordingError::NoTrailer)
        ));
        assert!(matches!(
            read_trailer(&vec![0u8; TRAILER_LEN as usize + 1]),
            Err(RecordingError::NoTrailer)
        ));
    }

    #[test]
    fn a_corrupted_trailer_is_no_trailer_not_a_panic() {
        let mut out = Vec::new();
        write_trailer(&mut out, 1, 2);
        for i in 0..out.len() {
            let mut corrupted = out.clone();
            corrupted[i] ^= 0xFF;
            assert!(
                read_trailer(&corrupted).is_err(),
                "byte {i} flip must be caught"
            );
        }
    }

    #[test]
    fn trailer_len_matches_its_documented_layout() {
        // magic(8) + footer_offset(8) + footer_len(4) + crc(4).
        assert_eq!(TRAILER_LEN, 24);
    }
}
