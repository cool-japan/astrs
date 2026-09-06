//! PNG chunk framing (spec §5): the 8-byte signature, and the
//! length/type/data/CRC-32 shape every chunk after it shares.
//!
//! This is the layer that recognises `IHDR`/`IDAT`/`PLTE`/`IEND` (and every
//! other four-letter chunk type) as chunks at all; [`super::decode`] is
//! where their *meaning* — IHDR's fields, IDAT's concatenation, PLTE being
//! ignored because this crate never decodes an indexed-colour image — is
//! decided.

use super::PngError;
use super::crc32::{ChunkCrc, crc32, crc32_append};

/// The eight magic bytes every PNG stream starts with (spec §5.2).
pub(crate) const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// A chunk's largest permitted length (spec §5.3: "shall not exceed
/// 2^31 - 1 bytes"), rejected earlier and more legibly than letting a
/// bogus length run the reader off the end of the buffer.
const MAX_CHUNK_LENGTH: u32 = 0x7FFF_FFFF;

/// One parsed chunk: a four-byte type and its data, with the length and
/// CRC-32 already consumed and verified.
pub(crate) struct Chunk<'a> {
    pub(crate) kind: [u8; 4],
    pub(crate) data: &'a [u8],
}

impl Chunk<'_> {
    /// This chunk's type, as the lossy-UTF-8 text a PNG type is defined to
    /// be — for error messages only, never for comparison (use `kind ==
    /// *b"IDAT"` for that).
    pub(crate) fn kind_str(&self) -> String {
        String::from_utf8_lossy(&self.kind).into_owned()
    }
}

/// Reads the four big-endian bytes at the start of `bytes` as a `u32`, or
/// [`None`] when `bytes` has fewer than four.
fn read_u32_be(bytes: &[u8]) -> Option<u32> {
    let [a, b, c, d] = *bytes.first_chunk::<4>()?;
    Some(u32::from_be_bytes([a, b, c, d]))
}

/// An iterator over the chunks of a PNG stream, positioned just after the
/// signature by [`ChunkReader::new`].
///
/// Every chunk's CRC-32 is verified before it is yielded; a mismatch (or a
/// stream that ends mid-chunk) ends iteration with an `Err`, and the reader
/// stops on its own right after yielding `IEND` — a well-formed stream never
/// needs [`ChunkReader::new`]'s caller to check for a "chunk after IEND"
/// case that the iterator already never produces.
pub(crate) struct ChunkReader<'a> {
    remaining: &'a [u8],
    done: bool,
}

impl<'a> ChunkReader<'a> {
    /// Verifies the 8-byte PNG signature and returns a reader positioned
    /// right after it.
    ///
    /// # Errors
    ///
    /// [`PngError::BadSignature`] when `data` is shorter than the signature
    /// or does not start with it.
    pub(crate) fn new(data: &'a [u8]) -> Result<Self, PngError> {
        if data.len() < SIGNATURE.len() || data[..SIGNATURE.len()] != SIGNATURE {
            return Err(PngError::BadSignature);
        }
        Ok(Self {
            remaining: &data[SIGNATURE.len()..],
            done: false,
        })
    }
}

impl<'a> Iterator for ChunkReader<'a> {
    type Item = Result<Chunk<'a>, PngError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let Some(length) = read_u32_be(self.remaining) else {
            self.done = true;
            return Some(Err(PngError::TruncatedChunk));
        };
        if length > MAX_CHUNK_LENGTH {
            self.done = true;
            return Some(Err(PngError::ChunkTooLarge { length }));
        }
        let length = length as usize;
        // 4 (length, already consumed above) + 4 (type) + length (data) + 4 (crc).
        let Some(total) = 8usize.checked_add(length).and_then(|n| n.checked_add(4)) else {
            self.done = true;
            return Some(Err(PngError::ChunkTooLarge {
                length: MAX_CHUNK_LENGTH,
            }));
        };
        let Some(frame) = self.remaining.get(..total) else {
            self.done = true;
            return Some(Err(PngError::TruncatedChunk));
        };
        let kind = [frame[4], frame[5], frame[6], frame[7]];
        let data = &frame[8..8 + length];
        let Some(expected_crc) = read_u32_be(&frame[8 + length..]) else {
            self.done = true;
            return Some(Err(PngError::TruncatedChunk));
        };

        let mut hasher = ChunkCrc::new();
        hasher.update(&kind);
        hasher.update(data);
        let actual_crc = hasher.finalize();
        if actual_crc != expected_crc {
            self.done = true;
            return Some(Err(PngError::CrcMismatch {
                kind: String::from_utf8_lossy(&kind).into_owned(),
                expected: expected_crc,
                actual: actual_crc,
            }));
        }

        self.remaining = &self.remaining[total..];
        if kind == *b"IEND" {
            self.done = true;
        }
        Some(Ok(Chunk { kind, data }))
    }
}

/// Appends one length-prefixed, CRC-terminated chunk to `out`.
pub(crate) fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    // Every chunk this crate ever writes (IHDR: 13 bytes, IDAT: one
    // zlib-compressed frame well under 2^31 bytes for any image this
    // process could hold in memory in the first place, IEND: 0 bytes) fits
    // comfortably in a `u32`; `unwrap_or(u32::MAX)` is unreachable in
    // practice and, if it were ever reached, produces an over-length chunk
    // a reader rejects cleanly rather than a corrupt one.
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32_append(crc32(kind), data).to_be_bytes());
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn stream_with_chunks(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        for (kind, data) in chunks {
            write_chunk(&mut out, kind, data);
        }
        out
    }

    #[test]
    fn new_rejects_a_missing_or_wrong_signature() {
        assert!(matches!(ChunkReader::new(b""), Err(PngError::BadSignature)));
        assert!(matches!(
            ChunkReader::new(b"not a png file at all!!"),
            Err(PngError::BadSignature)
        ));
    }

    #[test]
    fn write_then_read_round_trips_a_chunk() {
        // A well-formed (if minimal) stream needs an IEND too — the reader
        // has no way to tell "the stream just ends here" apart from
        // "the stream was cut off", and correctly reports the latter (see
        // `a_truncated_stream_is_reported_not_panicked_on` below) for a
        // chunk sequence with no closing IEND.
        let stream = stream_with_chunks(&[(b"IDAT", b"hello"), (b"IEND", b"")]);
        let mut reader = ChunkReader::new(&stream).unwrap();
        let chunk = reader.next().unwrap().unwrap();
        assert_eq!(&chunk.kind, b"IDAT");
        assert_eq!(chunk.data, b"hello");
        let iend = reader.next().unwrap().unwrap();
        assert_eq!(&iend.kind, b"IEND");
        assert!(reader.next().is_none());
    }

    #[test]
    fn the_reader_stops_right_after_iend() {
        let stream = stream_with_chunks(&[(b"IHDR", b"1"), (b"IEND", b""), (b"IDAT", b"trailing")]);
        let mut reader = ChunkReader::new(&stream).unwrap();
        let kinds: Vec<[u8; 4]> = std::iter::from_fn(|| reader.next())
            .map(|item| item.unwrap().kind)
            .collect();
        assert_eq!(
            kinds,
            [*b"IHDR", *b"IEND"],
            "the trailing IDAT is never reached"
        );
    }

    #[test]
    fn a_flipped_data_byte_fails_the_crc_check() {
        let mut stream = stream_with_chunks(&[(b"tEXt", b"comment")]);
        // Flip one bit inside the chunk's data, well after the signature
        // and length/type fields.
        let data_offset = SIGNATURE.len() + 8;
        stream[data_offset] ^= 0x01;
        let mut reader = ChunkReader::new(&stream).unwrap();
        assert!(matches!(
            reader.next(),
            Some(Err(PngError::CrcMismatch { .. }))
        ));
    }

    #[test]
    fn a_truncated_stream_is_reported_not_panicked_on() {
        let mut stream = stream_with_chunks(&[(b"IDAT", b"0123456789")]);
        stream.truncate(stream.len() - 3); // cut off inside the CRC
        let mut reader = ChunkReader::new(&stream).unwrap();
        assert!(matches!(reader.next(), Some(Err(PngError::TruncatedChunk))));
    }

    #[test]
    fn a_length_field_that_would_overrun_the_buffer_is_truncated_not_a_panic() {
        let mut stream = SIGNATURE.to_vec();
        stream.extend_from_slice(&0x00FF_FFFF_u32.to_be_bytes()); // huge, bogus length
        stream.extend_from_slice(b"IDAT");
        let mut reader = ChunkReader::new(&stream).unwrap();
        assert!(matches!(reader.next(), Some(Err(PngError::TruncatedChunk))));
    }

    #[test]
    fn kind_str_is_the_ascii_chunk_name() {
        let chunk = Chunk {
            kind: *b"IDAT",
            data: &[],
        };
        assert_eq!(chunk.kind_str(), "IDAT");
    }
}
