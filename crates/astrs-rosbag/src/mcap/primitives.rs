//! Byte-safe primitive readers for MCAP's "Serialization" rules
//! (`website/docs/spec/index.md` § Serialization, fetched from
//! `github.com/foxglove/mcap` and cross-checked against its
//! `mcap.ksy` Kaitai Struct definition — both quoted verbatim in the
//! doc comments below, never guessed):
//!
//! - Fixed-width integers (`uint16`/`uint32`/`uint64`) are little-endian.
//! - `String` is a `uint32` byte length followed by UTF-8 bytes.
//! - `Bytes` has no length of its own — each record gives its byte
//!   fields their own explicit prefix width (`uint32` for
//!   [`Schema::data`](crate::mcap::records::Schema::data); `uint64` for
//!   [`Chunk::records`](crate::mcap::records::Chunk::records) and
//!   [`Attachment::data`](crate::mcap::records::Attachment::data)).
//! - `Map<K, V>` is a `uint32` total byte length followed by
//!   back-to-back `(K, V)` pairs filling exactly that many bytes.
//!
//! [`Cursor`] wraps one already-length-bounded byte slice (a record's
//! body, or a decompressed chunk's `records` bytes) and never panics or
//! over-allocates on forged input: every `read_*` method checks the
//! requested length against what actually remains *before* touching it,
//! mirroring `astrs_recording::format::read_frame`'s discipline. The
//! slice itself is always already-validated-length by the caller (the
//! record framing read that produced it already checked its declared
//! length against the real file size or the real decompressed-chunk
//! size), so no method here needs its own separate ceiling — bounding the
//! *outer* framing is what prevents the bomb; see
//! [`crate::mcap::reader`] for where that happens.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::RosbagError;

/// A read cursor over one record's (or one decompressed chunk's) body
/// bytes.
pub struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// The body's own absolute file offset — `offset + pos` is always the
    /// exact byte position an error should report.
    base_offset: u64,
    path: &'a Path,
}

impl<'a> Cursor<'a> {
    /// Wraps `bytes`, an already-length-validated body starting at
    /// absolute file offset `base_offset`.
    #[must_use]
    pub const fn new(bytes: &'a [u8], base_offset: u64, path: &'a Path) -> Self {
        Self {
            bytes,
            pos: 0,
            base_offset,
            path,
        }
    }

    /// How many bytes remain unread.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// This cursor's current absolute file offset — `base_offset` plus
    /// how much has been consumed so far.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.base_offset + self.pos as u64
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], RosbagError> {
        if self.remaining() < n {
            return Err(RosbagError::Truncated {
                path: self.path.to_path_buf(),
                offset: self.offset(),
                needed: n,
                available: self.remaining(),
            });
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Reads one `uint8`.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if no byte remains.
    pub fn read_u8(&mut self) -> Result<u8, RosbagError> {
        Ok(self.take(1)?[0])
    }

    /// Reads one little-endian `uint16`.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if fewer than 2 bytes remain.
    pub fn read_u16(&mut self) -> Result<u16, RosbagError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Reads one little-endian `uint32`.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if fewer than 4 bytes remain.
    pub fn read_u32(&mut self) -> Result<u32, RosbagError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads one little-endian `uint64`.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if fewer than 8 bytes remain.
    pub fn read_u64(&mut self) -> Result<u64, RosbagError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Reads a `uint32`-length-prefixed UTF-8 `String`.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the declared length runs past what
    /// remains, or [`RosbagError::Malformed`] if the bytes are not valid
    /// UTF-8 (the spec requires it; a file that violates this is
    /// malformed, not merely old or unusual).
    pub fn read_string(&mut self) -> Result<String, RosbagError> {
        let len = self.read_u32()? as usize;
        let offset = self.offset();
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|source| RosbagError::Malformed {
            path: self.path.to_path_buf(),
            offset,
            reason: format!("string field is not valid UTF-8: {source}"),
        })
    }

    /// Reads a `uint32`-length-prefixed `Bytes` field (`Schema::data`'s
    /// own width).
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the declared length runs past what
    /// remains.
    pub fn read_bytes_u32(&mut self) -> Result<Vec<u8>, RosbagError> {
        let len = self.read_u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    /// Reads a `uint64`-length-prefixed `Bytes` field (`Chunk::records`'s
    /// and `Attachment::data`'s width).
    ///
    /// Never over-allocates on a forged length: `self.bytes` is already
    /// the *whole* validated body, so `take` rejects any `len` past what
    /// is actually left before this ever reaches `Vec::with_capacity`.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the declared length runs past what
    /// remains.
    pub fn read_bytes_u64(&mut self) -> Result<Vec<u8>, RosbagError> {
        let len = usize::try_from(self.read_u64()?).map_err(|_| RosbagError::Truncated {
            path: self.path.to_path_buf(),
            offset: self.offset(),
            needed: usize::MAX,
            available: self.remaining(),
        })?;
        Ok(self.take(len)?.to_vec())
    }

    /// Reads a `uint32` byte length, takes exactly that many bytes, and
    /// wraps them as their own [`Cursor`] — the shape every
    /// length-prefixed *collection* (`Map<K, V>`, `Array<T>`) shares
    /// before its own element-by-element decoding begins.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the declared length runs past what
    /// remains.
    pub fn sub_cursor(&mut self) -> Result<Cursor<'a>, RosbagError> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        let sub_offset = self.offset() - len as u64;
        Ok(Cursor::new(bytes, sub_offset, self.path))
    }

    /// Reads a `Map<string, string>`: a `uint32` total byte length
    /// followed by back-to-back `(String, String)` pairs filling exactly
    /// that many bytes.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Self::read_string`].
    pub fn read_map_str_str(&mut self) -> Result<BTreeMap<String, String>, RosbagError> {
        let mut sub = self.sub_cursor()?;
        let mut map = BTreeMap::new();
        while !sub.is_empty() {
            let key = sub.read_string()?;
            let value = sub.read_string()?;
            map.insert(key, value);
        }
        Ok(map)
    }

    /// Reads a `uint16`-keyed `Map<uint16, uint64>` (`ChunkIndex`'s
    /// `message_index_offsets`).
    ///
    /// # Errors
    ///
    /// As [`Self::read_map_str_str`].
    pub fn read_map_u16_u64(&mut self) -> Result<BTreeMap<u16, u64>, RosbagError> {
        let mut sub = self.sub_cursor()?;
        let mut map = BTreeMap::new();
        while !sub.is_empty() {
            let key = sub.read_u16()?;
            let value = sub.read_u64()?;
            map.insert(key, value);
        }
        Ok(map)
    }

    /// Consumes and returns every remaining byte — the `size-eos`
    /// primitive [`Message::data`](crate::mcap::records::Message::data)
    /// uses: whatever is left in an already-length-bounded record body,
    /// with no length prefix of its own.
    pub fn read_remaining(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.pos..];
        self.pos = self.bytes.len();
        rest
    }

    /// Whether every byte has been consumed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn cursor(bytes: &[u8]) -> Cursor<'_> {
        Cursor::new(bytes, 0, Path::new("test.mcap"))
    }

    #[test]
    fn reads_fixed_width_little_endian_integers() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let mut c = cursor(&bytes);
        assert_eq!(c.read_u8().unwrap(), 0x01);
        let mut c = cursor(&bytes);
        assert_eq!(c.read_u16().unwrap(), 0x0201);
        let mut c = cursor(&bytes);
        assert_eq!(c.read_u32().unwrap(), 0x0403_0201);
        let mut c = cursor(&bytes);
        assert_eq!(c.read_u64().unwrap(), 0x0807_0605_0403_0201);
    }

    #[test]
    fn reads_a_length_prefixed_string() {
        let mut bytes = 5u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"hello");
        let mut c = cursor(&bytes);
        assert_eq!(c.read_string().unwrap(), "hello");
        assert!(c.is_empty());
    }

    #[test]
    fn an_empty_string_is_a_bare_zero_length_prefix() {
        let bytes = 0u32.to_le_bytes();
        let mut c = cursor(&bytes);
        assert_eq!(c.read_string().unwrap(), "");
    }

    #[test]
    fn invalid_utf8_is_malformed_not_a_panic() {
        let mut bytes = 2u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        let mut c = cursor(&bytes);
        assert!(matches!(
            c.read_string(),
            Err(RosbagError::Malformed { .. })
        ));
    }

    #[test]
    fn a_truncated_length_prefix_is_truncated_not_a_panic() {
        let mut c = cursor(&[0x05, 0x00]);
        assert!(matches!(c.read_u32(), Err(RosbagError::Truncated { .. })));
    }

    #[test]
    fn a_declared_length_past_what_remains_is_truncated_not_an_allocation() {
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        let mut c = cursor(&bytes);
        assert!(matches!(
            c.read_string(),
            Err(RosbagError::Truncated { .. })
        ));
    }

    #[test]
    fn a_declared_u64_length_past_what_remains_is_truncated_not_an_allocation() {
        let mut bytes = u64::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        let mut c = cursor(&bytes);
        assert!(matches!(
            c.read_bytes_u64(),
            Err(RosbagError::Truncated { .. })
        ));
    }

    #[test]
    fn reads_bytes_u32_and_bytes_u64() {
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[1, 2, 3]);
        let mut c = cursor(&bytes);
        assert_eq!(c.read_bytes_u32().unwrap(), vec![1, 2, 3]);

        let mut bytes = 3u64.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[4, 5, 6]);
        let mut c = cursor(&bytes);
        assert_eq!(c.read_bytes_u64().unwrap(), vec![4, 5, 6]);
    }

    #[test]
    fn reads_a_map_of_strings() {
        // len(20) + ("a"->"1") + ("bb"->"22")
        let mut inner = Vec::new();
        for (k, v) in [("a", "1"), ("bb", "22")] {
            inner.extend_from_slice(&(k.len() as u32).to_le_bytes());
            inner.extend_from_slice(k.as_bytes());
            inner.extend_from_slice(&(v.len() as u32).to_le_bytes());
            inner.extend_from_slice(v.as_bytes());
        }
        let mut bytes = (inner.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&inner);
        let mut c = cursor(&bytes);
        let map = c.read_map_str_str().unwrap();
        assert_eq!(map.get("a"), Some(&"1".to_owned()));
        assert_eq!(map.get("bb"), Some(&"22".to_owned()));
        assert!(c.is_empty());
    }

    #[test]
    fn an_empty_map_round_trips() {
        let bytes = 0u32.to_le_bytes();
        let mut c = cursor(&bytes);
        assert!(c.read_map_str_str().unwrap().is_empty());
    }

    #[test]
    fn reads_a_map_of_u16_to_u64() {
        let mut inner = Vec::new();
        inner.extend_from_slice(&7u16.to_le_bytes());
        inner.extend_from_slice(&123u64.to_le_bytes());
        let mut bytes = (inner.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&inner);
        let mut c = cursor(&bytes);
        let map = c.read_map_u16_u64().unwrap();
        assert_eq!(map.get(&7), Some(&123));
    }

    #[test]
    fn read_remaining_consumes_everything_left() {
        let bytes = [1, 2, 3, 4, 5];
        let mut c = cursor(&bytes);
        let _ = c.read_u16().unwrap();
        assert_eq!(c.read_remaining(), &[3, 4, 5]);
        assert!(c.is_empty());
    }

    #[test]
    fn offset_tracks_the_base_plus_bytes_consumed() {
        let bytes = [0u8; 10];
        let mut c = Cursor::new(&bytes, 1_000, Path::new("x"));
        assert_eq!(c.offset(), 1_000);
        let _ = c.read_u32().unwrap();
        assert_eq!(c.offset(), 1_004);
    }
}
