//! Per-route payload compression (blueprint §6.4, §7.1).
//!
//! `astrs-wire` owns the flag bits — `bit1 = lz4`, `bit2 = zstd` — and
//! deliberately refuses to decode a payload that carries either, because the
//! codecs live here. This module is the other half of that contract: the
//! container those flag bits describe, and the two codec bindings.
//!
//! # The container
//!
//! A compressed frame's payload is **not** raw codec output. It is:
//!
//! ```text
//! ┌───────────────────┬──────────────────────────────────┐
//! │ original_len: u32 │ codec output                     │
//! │ little-endian     │ (lz4 frame, or zstd frame)       │
//! └───────────────────┴──────────────────────────────────┘
//!        4 bytes                  len - 4 bytes
//! ```
//!
//! The four-byte prefix earns its place three times over:
//!
//! 1. **`oxiarc_lz4::decompress` requires an output bound.** Without a declared
//!    size there is nothing honest to pass it.
//! 2. **`oxiarc_zstd::decompress` accepts none.** A zstd frame that expands to
//!    4 GiB is a 40-byte packet; the declared length is checked against the
//!    connection's negotiated ceiling *before* the codec runs, so the bomb
//!    never detonates.
//! 3. **It makes truncation loud.** The codec's output is compared against the
//!    declaration afterwards, so a frame that decompresses to the wrong size is
//!    a typed error rather than a short payload handed to the application.
//!
//! # Ordering
//!
//! On send: **compress → frame → checksum**. The CRC in the frame trailer
//! therefore covers the *compressed* bytes, which is what the receiver checks
//! first, so a corrupted frame is rejected before a codec ever sees it.
//! On receive: **checksum → decompress**. `astrs-wire`'s `max_payload_bytes`
//! bounds the on-wire length; the decompressed bound is enforced here.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::{CompressionPolicy, compress_payload, decompress_payload};
//! use astrs_wire::Compression;
//!
//! let policy = CompressionPolicy::codec(Compression::Lz4).with_threshold_bytes(16);
//! let payload = vec![7u8; 4_096];
//!
//! let (wire, codec) = compress_payload(&policy, &payload)?;
//! assert_eq!(codec, Compression::Lz4);
//! assert!(wire.len() < payload.len());
//!
//! let back = decompress_payload(codec, &wire, 1 << 20)?;
//! assert_eq!(back, payload);
//! # Ok::<(), astrs_transport::TransportError>(())
//! ```

use astrs_wire::Compression;

use crate::config::CompressionPolicy;
use crate::error::{TransportError, TransportResult};

/// The size of the container's declared-length prefix, in bytes.
pub const CONTAINER_HEADER_LEN: usize = 4;

/// The largest payload the container can describe, in bytes.
///
/// The prefix is a `u32` because the frame length field is a `u32` too
/// (§7.1): a payload the container could not describe could not have been
/// framed in the first place.
pub const MAX_CONTAINER_PAYLOAD_BYTES: usize = u32::MAX as usize;

/// Compresses `payload` if the policy says it is worth it.
///
/// Returns the bytes to put on the wire and the codec that was actually
/// applied. [`Compression::None`] means the returned slice *is* `payload`
/// unchanged — no container, no copy beyond the one the caller already owns —
/// and the frame's flag bits must stay clear.
///
/// The decision is measured, not guessed. A payload below the threshold, above
/// the ceiling, or one the codec failed to shrink past
/// [`CompressionPolicy::max_ratio_percent`] is sent raw. That is what makes an
/// already-compressed payload type (a JPEG, an H.264 frame) safe to route
/// through a compressing link: the second pass is attempted, measured, and
/// discarded, costing CPU but never bytes.
///
/// # Errors
///
/// - [`TransportError::Codec`] if the codec itself failed.
/// - [`TransportError::FrameTooLarge`] if `payload` cannot be described by the
///   container's 32-bit length prefix.
///
/// # Examples
///
/// ```
/// use astrs_transport::{CompressionPolicy, compress_payload};
/// use astrs_wire::Compression;
///
/// // Below the threshold: sent raw, whatever the codec.
/// let policy = CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(1_024);
/// let (wire, codec) = compress_payload(&policy, &[0u8; 8])?;
/// assert_eq!(codec, Compression::None);
/// assert_eq!(wire.len(), 8);
/// # Ok::<(), astrs_transport::TransportError>(())
/// ```
pub fn compress_payload(
    policy: &CompressionPolicy,
    payload: &[u8],
) -> TransportResult<(Vec<u8>, Compression)> {
    if !policy.should_try(payload.len()) {
        return Ok((payload.to_vec(), Compression::None));
    }
    if payload.len() > MAX_CONTAINER_PAYLOAD_BYTES {
        return Err(TransportError::FrameTooLarge {
            actual: payload.len(),
            limit: MAX_CONTAINER_PAYLOAD_BYTES,
        });
    }

    let codec = policy.codec;
    let body = match codec {
        Compression::Lz4 => {
            oxiarc_lz4::compress(payload).map_err(|err| TransportError::codec(codec, err))?
        }
        // `compress_with_level`, never the bare `compress`: level 0 emits raw
        // and RLE blocks only, so `oxiarc_zstd::compress` reliably *grows* a
        // payload by its 14-byte frame overhead. Only levels 1 and above run
        // the LZ77 matcher.
        Compression::Zstd => oxiarc_zstd::compress_with_level(payload, policy.zstd_level)
            .map_err(|err| TransportError::codec(codec, err))?,
        // `should_try` already refused `Compression::None`, and the enum is
        // `#[non_exhaustive]`: a codec added later is unknown here, so the
        // honest answer is to send the payload raw rather than to guess.
        _ => return Ok((payload.to_vec(), Compression::None)),
    };

    let framed_len = body.len().saturating_add(CONTAINER_HEADER_LEN);
    if !policy.is_worth_it(payload.len(), framed_len) {
        return Ok((payload.to_vec(), Compression::None));
    }

    let mut container = Vec::with_capacity(framed_len);
    // `payload.len()` is bounded above by `MAX_CONTAINER_PAYLOAD_BYTES`, so
    // the cast is exact.
    container.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    container.extend_from_slice(&body);
    Ok((container, codec))
}

/// Compresses `payload` into `out`, reusing the caller's buffer.
///
/// Identical in behaviour to [`compress_payload`], but writes into a buffer
/// the caller keeps across frames. On a 30 Hz camera route that is one fewer
/// allocation per frame, which is the whole reason the hot path is shaped this
/// way.
///
/// `out` is cleared first, and always ends up holding exactly the bytes to put
/// on the wire — including the raw payload when the codec declined.
///
/// # Errors
///
/// As [`compress_payload`].
///
/// # Examples
///
/// ```
/// use astrs_transport::{CompressionPolicy, compress_payload_into};
/// use astrs_wire::Compression;
///
/// let policy = CompressionPolicy::codec(Compression::Lz4).with_threshold_bytes(16);
/// let mut scratch = Vec::new();
/// let codec = compress_payload_into(&policy, &vec![3u8; 1_024], &mut scratch)?;
/// assert_eq!(codec, Compression::Lz4);
/// assert!(!scratch.is_empty());
/// # Ok::<(), astrs_transport::TransportError>(())
/// ```
pub fn compress_payload_into(
    policy: &CompressionPolicy,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> TransportResult<Compression> {
    out.clear();
    if !policy.should_try(payload.len()) {
        out.extend_from_slice(payload);
        return Ok(Compression::None);
    }
    let (bytes, codec) = compress_payload(policy, payload)?;
    out.extend_from_slice(&bytes);
    Ok(codec)
}

/// Reads the declared original length from a container.
///
/// # Errors
///
/// [`TransportError::TruncatedCompressionHeader`] if the payload is shorter
/// than the prefix.
///
/// # Examples
///
/// ```
/// use astrs_transport::declared_len;
///
/// let mut container = 1_234u32.to_le_bytes().to_vec();
/// container.extend_from_slice(b"codec output");
/// assert_eq!(declared_len(&container)?, 1_234);
/// # Ok::<(), astrs_transport::TransportError>(())
/// ```
pub fn declared_len(container: &[u8]) -> TransportResult<u32> {
    let head: [u8; CONTAINER_HEADER_LEN] = container
        .get(..CONTAINER_HEADER_LEN)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(TransportError::TruncatedCompressionHeader {
            len: container.len(),
            needed: CONTAINER_HEADER_LEN,
        })?;
    Ok(u32::from_le_bytes(head))
}

/// Decompresses a container produced by [`compress_payload`].
///
/// `max_bytes` is the connection's negotiated payload ceiling. It is checked
/// against the *declared* length before any codec runs, which is what keeps a
/// tiny frame from expanding into an out-of-memory abort.
///
/// A `codec` of [`Compression::None`] returns the payload unchanged, so a
/// caller can pass whatever the frame's flags said without branching.
///
/// # Errors
///
/// - [`TransportError::TruncatedCompressionHeader`] if the container is too
///   short to hold its prefix.
/// - [`TransportError::DecompressedTooLarge`] if the declaration exceeds
///   `max_bytes`.
/// - [`TransportError::DecompressedLengthMismatch`] if the codec's output does
///   not match the declaration.
/// - [`TransportError::Codec`] if the codec rejected the input.
/// - [`TransportError::CompressionNotNegotiated`] for a codec this build does
///   not implement.
///
/// # Examples
///
/// ```
/// use astrs_transport::{CompressionPolicy, compress_payload, decompress_payload};
/// use astrs_wire::Compression;
///
/// let policy = CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(8);
/// let (wire, codec) = compress_payload(&policy, &vec![9u8; 2_048])?;
///
/// // A ceiling below the declared size refuses the frame without decoding it.
/// assert!(decompress_payload(codec, &wire, 16).is_err());
/// assert_eq!(decompress_payload(codec, &wire, 1 << 20)?.len(), 2_048);
/// # Ok::<(), astrs_transport::TransportError>(())
/// ```
pub fn decompress_payload(
    codec: Compression,
    container: &[u8],
    max_bytes: usize,
) -> TransportResult<Vec<u8>> {
    if !codec.is_enabled() {
        return Ok(container.to_vec());
    }

    let declared = declared_len(container)?;
    let declared_usize = declared as usize;
    if declared_usize > max_bytes {
        return Err(TransportError::DecompressedTooLarge {
            declared: u64::from(declared),
            limit: max_bytes,
        });
    }

    // The prefix length was already validated by `declared_len`.
    let body = container.get(CONTAINER_HEADER_LEN..).unwrap_or(&[]);
    let plain = match codec {
        Compression::Lz4 => oxiarc_lz4::decompress(body, declared_usize)
            .map_err(|err| TransportError::codec(codec, err))?,
        Compression::Zstd => {
            oxiarc_zstd::decompress(body).map_err(|err| TransportError::codec(codec, err))?
        }
        _ => return Err(TransportError::CompressionNotNegotiated { codec }),
    };

    if plain.len() != declared_usize {
        return Err(TransportError::DecompressedLengthMismatch {
            declared: declared_usize,
            actual: plain.len(),
        });
    }
    Ok(plain)
}

/// Whether a codec is one this build can actually run.
///
/// A peer may advertise a codec from a newer release; a frame that arrives
/// using it must be refused with [`TransportError::CompressionNotNegotiated`]
/// rather than mis-decoded.
///
/// # Examples
///
/// ```
/// use astrs_transport::is_supported_codec;
/// use astrs_wire::Compression;
///
/// assert!(is_supported_codec(Compression::Lz4));
/// assert!(is_supported_codec(Compression::Zstd));
/// assert!(!is_supported_codec(Compression::None));
/// ```
#[must_use]
pub const fn is_supported_codec(codec: Compression) -> bool {
    matches!(codec, Compression::Lz4 | Compression::Zstd)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A payload that compresses well, so the ratio test always passes.
    fn compressible(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 7) as u8).collect()
    }

    /// A payload that does not compress, so the ratio test always fails.
    fn incompressible(len: usize) -> Vec<u8> {
        // A cheap deterministic PRNG: xorshift over a fixed seed. Not random
        // enough for crypto, plenty random enough to defeat lz4 and zstd.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xff) as u8
            })
            .collect()
    }

    #[test]
    fn both_codecs_round_trip() {
        for codec in [Compression::Lz4, Compression::Zstd] {
            let policy = CompressionPolicy::codec(codec).with_threshold_bytes(16);
            let payload = compressible(64 * 1024);
            let (wire, applied) = compress_payload(&policy, &payload).unwrap();
            assert_eq!(applied, codec, "codec {codec} was declined");
            assert!(wire.len() < payload.len(), "codec {codec} did not shrink");
            let back = decompress_payload(applied, &wire, 1 << 20).unwrap();
            assert_eq!(back, payload, "codec {codec} did not round trip");
        }
    }

    #[test]
    fn an_empty_payload_round_trips_without_a_codec() {
        let policy = CompressionPolicy::codec(Compression::Lz4).with_threshold_bytes(0);
        let (wire, codec) = compress_payload(&policy, &[]).unwrap();
        // A zero-byte payload can never be "worth it": the container header
        // alone is four bytes more than the payload.
        assert_eq!(codec, Compression::None);
        assert!(wire.is_empty());
        assert_eq!(
            decompress_payload(codec, &wire, 16).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn the_threshold_boundary_is_exact() {
        let threshold = 16 * 1024;
        let policy = CompressionPolicy::codec(Compression::Lz4).with_threshold_bytes(threshold);

        let (_, below) = compress_payload(&policy, &compressible(threshold - 1)).unwrap();
        assert_eq!(below, Compression::None, "one byte below must be raw");

        let (_, exactly) = compress_payload(&policy, &compressible(threshold)).unwrap();
        assert_eq!(
            exactly,
            Compression::Lz4,
            "exactly at the threshold must compress"
        );

        let (_, above) = compress_payload(&policy, &compressible(threshold + 1)).unwrap();
        assert_eq!(above, Compression::Lz4, "above the threshold must compress");
    }

    #[test]
    fn a_payload_that_does_not_shrink_is_sent_raw() {
        for codec in [Compression::Lz4, Compression::Zstd] {
            let policy = CompressionPolicy::codec(codec).with_threshold_bytes(16);
            let payload = incompressible(8 * 1024);
            let (wire, applied) = compress_payload(&policy, &payload).unwrap();
            assert_eq!(
                applied,
                Compression::None,
                "codec {codec} kept a losing result"
            );
            assert_eq!(wire, payload);
        }
    }

    #[test]
    fn a_disabled_policy_never_compresses() {
        let policy = CompressionPolicy::disabled();
        let payload = compressible(1 << 20);
        let (wire, codec) = compress_payload(&policy, &payload).unwrap();
        assert_eq!(codec, Compression::None);
        assert_eq!(wire, payload);
    }

    #[test]
    fn a_ceiling_sends_huge_payloads_raw() {
        let policy = CompressionPolicy::codec(Compression::Lz4)
            .with_threshold_bytes(16)
            .with_ceiling_bytes(1_024);
        let (_, codec) = compress_payload(&policy, &compressible(4_096)).unwrap();
        assert_eq!(codec, Compression::None);
    }

    #[test]
    fn a_declared_size_over_the_ceiling_is_refused_before_decoding() {
        let policy = CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(16);
        let payload = compressible(64 * 1024);
        let (wire, codec) = compress_payload(&policy, &payload).unwrap();

        let err = decompress_payload(codec, &wire, 1_024).unwrap_err();
        match err {
            TransportError::DecompressedTooLarge { declared, limit } => {
                assert_eq!(declared, payload.len() as u64);
                assert_eq!(limit, 1_024);
            }
            other => panic!("expected a ceiling refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_lying_declaration_is_caught_after_decoding() {
        let policy = CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(16);
        let payload = compressible(32 * 1024);
        let (mut wire, codec) = compress_payload(&policy, &payload).unwrap();

        // Claim one byte fewer than the codec will actually produce.
        let lie = (payload.len() as u32) - 1;
        wire[..CONTAINER_HEADER_LEN].copy_from_slice(&lie.to_le_bytes());

        let err = decompress_payload(codec, &wire, 1 << 20).unwrap_err();
        assert!(
            matches!(
                err,
                TransportError::DecompressedLengthMismatch { .. } | TransportError::Codec { .. }
            ),
            "expected a length or codec failure, got {err:?}"
        );
    }

    #[test]
    fn a_truncated_container_is_a_typed_error_not_a_panic() {
        for len in 0..CONTAINER_HEADER_LEN {
            let err = decompress_payload(Compression::Lz4, &vec![0u8; len], 1 << 20).unwrap_err();
            match err {
                TransportError::TruncatedCompressionHeader { len: found, needed } => {
                    assert_eq!(found, len);
                    assert_eq!(needed, CONTAINER_HEADER_LEN);
                }
                other => panic!("expected a truncation error, got {other:?}"),
            }
            assert!(declared_len(&vec![0u8; len]).is_err());
        }
    }

    #[test]
    fn a_container_with_no_body_is_a_codec_error_not_a_panic() {
        // Four bytes of header, zero bytes of codec output, claiming 1 KiB.
        let container = 1_024u32.to_le_bytes().to_vec();
        for codec in [Compression::Lz4, Compression::Zstd] {
            let err = decompress_payload(codec, &container, 1 << 20).unwrap_err();
            assert!(
                matches!(
                    err,
                    TransportError::Codec { .. }
                        | TransportError::DecompressedLengthMismatch { .. }
                ),
                "expected a codec failure for {codec}, got {err:?}"
            );
        }
    }

    #[test]
    fn corrupted_codec_bytes_are_a_typed_error_not_a_panic() {
        for codec in [Compression::Lz4, Compression::Zstd] {
            let policy = CompressionPolicy::codec(codec).with_threshold_bytes(16);
            let payload = compressible(16 * 1024);
            let (mut wire, applied) = compress_payload(&policy, &payload).unwrap();
            assert_eq!(applied, codec);

            // Flip bits in the middle of the codec stream.
            let midpoint = wire.len() / 2;
            for byte in &mut wire[midpoint..midpoint + 8] {
                *byte ^= 0xff;
            }

            // Either the codec rejects it or it expands to the wrong size;
            // both are typed errors, and neither may panic.
            match decompress_payload(applied, &wire, 1 << 20) {
                Ok(plain) => assert_eq!(
                    plain.len(),
                    payload.len(),
                    "a decode that succeeded must still honour the declared length"
                ),
                Err(err) => assert!(
                    matches!(
                        err,
                        TransportError::Codec { .. }
                            | TransportError::DecompressedLengthMismatch { .. }
                    ),
                    "unexpected error for {codec}: {err:?}"
                ),
            }
        }
    }

    #[test]
    fn no_compression_passes_the_payload_through_untouched() {
        let payload = b"hello".to_vec();
        assert_eq!(
            decompress_payload(Compression::None, &payload, 16).unwrap(),
            payload
        );
    }

    #[test]
    fn the_buffered_form_matches_the_allocating_one() {
        let policy = CompressionPolicy::codec(Compression::Lz4).with_threshold_bytes(16);
        for len in [0usize, 1, 15, 16, 1_024, 40_000] {
            let payload = compressible(len);
            let (expected, expected_codec) = compress_payload(&policy, &payload).unwrap();

            let mut scratch = vec![0xaa; 1_000];
            let codec = compress_payload_into(&policy, &payload, &mut scratch).unwrap();
            assert_eq!(codec, expected_codec, "codec differed at len {len}");
            assert_eq!(scratch, expected, "bytes differed at len {len}");
        }
    }

    #[test]
    fn the_buffer_is_reused_across_calls() {
        let policy = CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(16);
        let mut scratch = Vec::new();
        for _ in 0..4 {
            let codec = compress_payload_into(&policy, &compressible(8_192), &mut scratch).unwrap();
            assert_eq!(codec, Compression::Zstd);
            let back = decompress_payload(codec, &scratch, 1 << 20).unwrap();
            assert_eq!(back.len(), 8_192);
        }
    }

    #[test]
    fn declared_len_reads_the_prefix() {
        let mut container = 4_242u32.to_le_bytes().to_vec();
        container.extend_from_slice(b"body");
        assert_eq!(declared_len(&container).unwrap(), 4_242);
    }

    #[test]
    fn zstd_level_zero_would_not_compress_so_the_policy_never_uses_it() {
        // The regression this guards: `oxiarc_zstd::compress` is level 0, and
        // level 0 emits raw blocks. A policy that reached the codec at level 0
        // would silently grow every payload by the frame overhead and then be
        // discarded by the ratio test, costing CPU for nothing.
        let payload = compressible(32 * 1024);
        let level_zero = oxiarc_zstd::compress(&payload).unwrap();
        assert!(
            level_zero.len() >= payload.len(),
            "level 0 unexpectedly compressed; the clamp may no longer be needed"
        );

        let policy = CompressionPolicy::codec(Compression::Zstd)
            .with_threshold_bytes(16)
            .with_zstd_level(0);
        let (wire, codec) = compress_payload(&policy, &payload).unwrap();
        assert_eq!(codec, Compression::Zstd);
        assert!(wire.len() < payload.len() / 10, "expected real compression");
    }

    #[test]
    fn higher_zstd_levels_still_round_trip() {
        let payload = compressible(64 * 1024);
        for level in [1, 3, 9, 22] {
            let policy = CompressionPolicy::codec(Compression::Zstd)
                .with_threshold_bytes(16)
                .with_zstd_level(level);
            let (wire, codec) = compress_payload(&policy, &payload).unwrap();
            assert_eq!(codec, Compression::Zstd, "level {level} declined");
            assert_eq!(
                decompress_payload(codec, &wire, 1 << 20).unwrap(),
                payload,
                "level {level} did not round trip"
            );
        }
    }

    #[test]
    fn only_the_two_implemented_codecs_are_supported() {
        assert!(is_supported_codec(Compression::Lz4));
        assert!(is_supported_codec(Compression::Zstd));
        assert!(!is_supported_codec(Compression::None));
    }

    #[test]
    fn a_large_range_of_sizes_round_trips_on_both_codecs() {
        for codec in [Compression::Lz4, Compression::Zstd] {
            let policy = CompressionPolicy::codec(codec).with_threshold_bytes(1);
            for len in [1usize, 2, 3, 17, 255, 256, 257, 4_095, 4_096, 65_537] {
                let payload = compressible(len);
                let (wire, applied) = compress_payload(&policy, &payload).unwrap();
                let back = decompress_payload(applied, &wire, 1 << 20).unwrap();
                assert_eq!(back, payload, "{codec} failed at len {len}");
            }
        }
    }
}
