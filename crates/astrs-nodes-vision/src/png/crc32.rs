//! CRC-32 (IEEE 802.3 / ISO-HDLC / `zlib`'s "CRC-32"), the checksum every
//! PNG chunk trails.
//!
//! Not the same polynomial as `astrs_wire::crc32c` (Castagnoli, for the
//! AstRS wire frame) — PNG's chunk CRC is the *other*, older CRC-32
//! (reflected polynomial `0xEDB8_8320`), the one `zlib`, `gzip` and `zip`
//! all also use. Neither `astrs-wire`'s implementation nor a workspace
//! dependency covers it (`oxiarc-core`'s `Crc32` is `oxiarc-deflate`'s own
//! transitive dependency, not one of this crate's — see the module's own
//! `Cargo.toml`), so PNG's chunk framing owns this one small, entirely
//! standard table-based implementation.

/// The reflected CRC-32/ISO-HDLC generator polynomial.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// The 256-entry lookup table, one entry per possible byte value, built once
/// at compile time.
const TABLE: [u32; 256] = build_table();

/// Builds [`TABLE`]: for each possible byte, the eight-bit-at-a-time CRC
/// update the loop in [`update`] would otherwise do one bit at a time.
const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut byte = 0usize;
    while byte < 256 {
        let mut value = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                POLYNOMIAL ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[byte] = value;
        byte += 1;
    }
    table
}

/// Computes the CRC-32 of `data`. See this module's tests for the standard
/// catalogue check value this matches.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    crc32_append(0, data)
}

/// Continues a CRC-32 over `data`, starting from the finalised checksum
/// `previous` — the primitive [`ChunkCrc`] streams through so a chunk's
/// type and data never need to be concatenated into one buffer just to be
/// checksummed.
pub(crate) fn crc32_append(previous: u32, data: &[u8]) -> u32 {
    !update(!previous, data)
}

/// The running, non-finalised CRC register update: XOR-index-shift once per
/// byte, using [`TABLE`] to fold eight bits at a time instead of one.
fn update(state: u32, data: &[u8]) -> u32 {
    let mut state = state;
    for &byte in data {
        let index = ((state ^ u32::from(byte)) & 0xFF) as usize;
        state = TABLE[index] ^ (state >> 8);
    }
    state
}

/// A streaming CRC-32 hasher, for checksumming a chunk's type and data
/// without concatenating them first.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkCrc {
    state: u32,
}

impl ChunkCrc {
    /// A hasher in its initial state.
    pub(crate) const fn new() -> Self {
        Self { state: u32::MAX }
    }

    /// Folds `data` into the running checksum.
    pub(crate) fn update(&mut self, data: &[u8]) {
        self.state = update(self.state, data);
    }

    /// The finalised CRC-32 of everything folded in so far.
    pub(crate) const fn finalize(self) -> u32 {
        !self.state
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn matches_the_standard_catalogue_check_value() {
        // The canonical CRC-32/ISO-HDLC "check" value every catalogue
        // quotes for the ASCII string "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn append_over_two_pieces_matches_one_call_over_the_concatenation() {
        let whole = crc32(b"123456789");
        let split = crc32_append(crc32(b"1234"), b"56789");
        assert_eq!(whole, split);
    }

    #[test]
    fn the_streaming_hasher_matches_the_one_shot_function() {
        let mut hasher = ChunkCrc::new();
        for chunk in [b"IDAT".as_slice(), b"some pixel bytes".as_slice()] {
            hasher.update(chunk);
        }
        let mut concatenated = Vec::new();
        concatenated.extend_from_slice(b"IDAT");
        concatenated.extend_from_slice(b"some pixel bytes");
        assert_eq!(hasher.finalize(), crc32(&concatenated));
    }

    #[test]
    fn a_single_bit_flip_changes_the_checksum() {
        let original = crc32(b"PNG chunk data");
        let flipped = crc32(b"PNG chunk datA");
        assert_ne!(original, flipped);
    }
}
