//! Bounds-checked FlatBuffers *reader* primitives.
//!
//! The reader never allocates, never panics and never trusts the input: every
//! offset is range-checked against the metadata block before it is followed,
//! and every follow moves strictly forward, so a malicious buffer cannot make
//! the decoder loop. Callers still cap nesting depth themselves (see
//! [`crate::ipc::schema`]) because "strictly forward" only bounds recursion by
//! the buffer length.
//!
//! # Layout recap
//!
//! ```text
//! table    [soffset:i32][inline fields...]     vtable = table_pos - soffset
//! vtable   [vt_len:u16][table_len:u16][slot0:u16][slot1:u16]...
//! vector   [len:u32][element 0][element 1]...
//! string   [len:u32][utf-8 bytes][NUL]
//! uoffset  u32, forward-relative to its own position
//! ```
//!
//! A vtable slot value of `0` means "field absent, use the default"; that is
//! why every getter here takes a default.
//!
//! ```
//! use astrs_data::ipc::fb::{FbBuilder, root_table};
//!
//! let mut builder = FbBuilder::new();
//! let name = builder.create_string("velocity");
//! let table = builder.start_table();
//! builder.push_slot_offset(0, name);
//! builder.push_slot_i32(1, 42, 0);
//! let table = builder.end_table(table);
//! builder.finish(table);
//!
//! let bytes = builder.finished_bytes();
//! let root = root_table(bytes)?;
//! assert_eq!(root.string(0)?, Some("velocity"));
//! assert_eq!(root.i32(1, 0)?, 42);
//! assert_eq!(root.i32(2, -1)?, -1, "absent slots fall back to the default");
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

use crate::ipc::error::{IpcError, Result};

/// Size of a `uoffset_t`/`soffset_t` in bytes.
pub const SIZE_UOFFSET: usize = 4;
/// Size of a `voffset_t` in bytes.
pub const SIZE_VOFFSET: usize = 2;

/// Reads the root table of a finished flatbuffer.
///
/// # Errors
///
/// [`IpcError::MalformedFlatbuffer`] when the buffer is too short or the root
/// offset, vtable offset or vtable length leave the buffer.
pub fn root_table(buf: &[u8]) -> Result<Table<'_>> {
    let root = deref(buf, 0, "root offset")?;
    Table::at(buf, root)
}

/// Reads a `u8` at `pos`.
///
/// # Errors
///
/// [`IpcError::MalformedFlatbuffer`] when `pos` is out of range.
#[inline]
pub fn read_u8(buf: &[u8], pos: usize, context: &'static str) -> Result<u8> {
    buf.get(pos)
        .copied()
        .ok_or(IpcError::malformed(context, pos))
}

macro_rules! read_scalar {
    ($name:ident, $ty:ty, $doc:literal) => {
        #[doc = $doc]
        ///
        /// # Errors
        ///
        /// [`IpcError::MalformedFlatbuffer`] when the value would leave the
        /// buffer.
        #[inline]
        pub fn $name(buf: &[u8], pos: usize, context: &'static str) -> Result<$ty> {
            const WIDTH: usize = std::mem::size_of::<$ty>();
            let end = pos
                .checked_add(WIDTH)
                .ok_or(IpcError::malformed(context, pos))?;
            let slice = buf.get(pos..end).ok_or(IpcError::malformed(context, pos))?;
            let mut bytes = [0u8; WIDTH];
            bytes.copy_from_slice(slice);
            Ok(<$ty>::from_le_bytes(bytes))
        }
    };
}

read_scalar!(read_i8, i8, "Reads an `i8` at `pos`.");
read_scalar!(read_u16, u16, "Reads a little-endian `u16` at `pos`.");
read_scalar!(read_i16, i16, "Reads a little-endian `i16` at `pos`.");
read_scalar!(read_u32, u32, "Reads a little-endian `u32` at `pos`.");
read_scalar!(read_i32, i32, "Reads a little-endian `i32` at `pos`.");
read_scalar!(read_i64, i64, "Reads a little-endian `i64` at `pos`.");

/// Follows a `uoffset_t` stored at `pos`, returning the absolute position it
/// points to.
///
/// A zero offset is rejected: it would point at itself and let a hostile
/// buffer loop the decoder forever.
///
/// # Errors
///
/// [`IpcError::MalformedFlatbuffer`] when the offset is zero, overflows, or
/// lands outside the buffer.
#[inline]
pub fn deref(buf: &[u8], pos: usize, context: &'static str) -> Result<usize> {
    let offset = read_u32(buf, pos, context)? as usize;
    if offset == 0 {
        return Err(IpcError::malformed(context, pos));
    }
    let target = pos
        .checked_add(offset)
        .ok_or(IpcError::malformed(context, pos))?;
    if target >= buf.len() {
        return Err(IpcError::malformed(context, pos));
    }
    Ok(target)
}

/// A flatbuffer table: a vtable plus an inline field block.
///
/// Cheap to copy; holds only the backing slice and two positions.
#[derive(Debug, Clone, Copy)]
pub struct Table<'a> {
    buf: &'a [u8],
    pos: usize,
    vtable: usize,
    vt_len: usize,
}

impl<'a> Table<'a> {
    /// Reads the table whose `soffset_t` starts at `pos`.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the vtable pointer or the vtable
    /// header leaves the buffer.
    pub fn at(buf: &'a [u8], pos: usize) -> Result<Self> {
        let soffset = read_i32(buf, pos, "table vtable pointer")?;
        // vtable = pos - soffset, in i64 so a negative soffset (a vtable that
        // the writer placed *after* the table, which vtable de-duplication
        // produces) cannot wrap.
        let vtable = i64::try_from(pos)
            .map_err(|_| IpcError::malformed("table position", pos))?
            .checked_sub(i64::from(soffset))
            .ok_or(IpcError::malformed("table vtable pointer", pos))?;
        let vtable = usize::try_from(vtable)
            .map_err(|_| IpcError::malformed("table vtable pointer", pos))?;
        let vt_len = read_u16(buf, vtable, "vtable length")? as usize;
        if vt_len < 2 * SIZE_VOFFSET {
            return Err(IpcError::malformed("vtable length", vtable));
        }
        let vt_end = vtable
            .checked_add(vt_len)
            .ok_or(IpcError::malformed("vtable length", vtable))?;
        if vt_end > buf.len() {
            return Err(IpcError::malformed("vtable length", vtable));
        }
        Ok(Self {
            buf,
            pos,
            vtable,
            vt_len,
        })
    }

    /// The table's own position within the buffer.
    #[inline]
    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// The number of slots the vtable describes.
    #[inline]
    #[must_use]
    pub const fn slot_count(&self) -> usize {
        (self.vt_len - 2 * SIZE_VOFFSET) / SIZE_VOFFSET
    }

    /// The absolute position of field `slot`, or `None` when the field is
    /// absent (past the end of the vtable, or a zero entry).
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the entry points outside the
    /// buffer.
    pub fn field(&self, slot: usize) -> Result<Option<usize>> {
        let vt_offset = 2 * SIZE_VOFFSET + slot * SIZE_VOFFSET;
        if vt_offset + SIZE_VOFFSET > self.vt_len {
            return Ok(None);
        }
        let entry = read_u16(self.buf, self.vtable + vt_offset, "vtable slot")? as usize;
        if entry == 0 {
            return Ok(None);
        }
        let at = self
            .pos
            .checked_add(entry)
            .ok_or(IpcError::malformed("vtable slot", self.vtable + vt_offset))?;
        if at >= self.buf.len() {
            return Err(IpcError::malformed("vtable slot", self.vtable + vt_offset));
        }
        Ok(Some(at))
    }

    /// Reads a `bool` field, or `default` when the slot is absent.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] on an out-of-range slot.
    pub fn bool(&self, slot: usize, default: bool) -> Result<bool> {
        match self.field(slot)? {
            Some(at) => Ok(read_u8(self.buf, at, "bool field")? != 0),
            None => Ok(default),
        }
    }

    /// Reads a `u8` field, or `default` when the slot is absent.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] on an out-of-range slot.
    pub fn u8(&self, slot: usize, default: u8) -> Result<u8> {
        match self.field(slot)? {
            Some(at) => read_u8(self.buf, at, "u8 field"),
            None => Ok(default),
        }
    }

    /// Reads an `i8` field, or `default` when the slot is absent.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] on an out-of-range slot.
    pub fn i8(&self, slot: usize, default: i8) -> Result<i8> {
        match self.field(slot)? {
            Some(at) => read_i8(self.buf, at, "i8 field"),
            None => Ok(default),
        }
    }

    /// Reads an `i16` field, or `default` when the slot is absent.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] on an out-of-range slot.
    pub fn i16(&self, slot: usize, default: i16) -> Result<i16> {
        match self.field(slot)? {
            Some(at) => read_i16(self.buf, at, "i16 field"),
            None => Ok(default),
        }
    }

    /// Reads an `i32` field, or `default` when the slot is absent.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] on an out-of-range slot.
    pub fn i32(&self, slot: usize, default: i32) -> Result<i32> {
        match self.field(slot)? {
            Some(at) => read_i32(self.buf, at, "i32 field"),
            None => Ok(default),
        }
    }

    /// Reads an `i64` field, or `default` when the slot is absent.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] on an out-of-range slot.
    pub fn i64(&self, slot: usize, default: i64) -> Result<i64> {
        match self.field(slot)? {
            Some(at) => read_i64(self.buf, at, "i64 field"),
            None => Ok(default),
        }
    }

    /// Reads a nested table field.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the offset or the nested vtable
    /// is out of range.
    pub fn table(&self, slot: usize) -> Result<Option<Table<'a>>> {
        match self.field(slot)? {
            Some(at) => {
                let target = deref(self.buf, at, "nested table offset")?;
                Ok(Some(Table::at(self.buf, target)?))
            }
            None => Ok(None),
        }
    }

    /// Reads a string field.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the offset, the length or the
    /// UTF-8 encoding is invalid.
    pub fn string(&self, slot: usize) -> Result<Option<&'a str>> {
        match self.field(slot)? {
            Some(at) => {
                let target = deref(self.buf, at, "string offset")?;
                Ok(Some(read_string(self.buf, target)?))
            }
            None => Ok(None),
        }
    }

    /// Reads a vector field, yielding its element count and the position of
    /// its first element.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the offset or the length is out
    /// of range.
    pub fn vector(&self, slot: usize, stride: usize) -> Result<Option<VectorSpan<'a>>> {
        match self.field(slot)? {
            Some(at) => {
                let target = deref(self.buf, at, "vector offset")?;
                Ok(Some(VectorSpan::at(self.buf, target, stride)?))
            }
            None => Ok(None),
        }
    }

    /// The backing bytes, for callers that need to follow raw positions.
    #[inline]
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.buf
    }
}

/// Reads the UTF-8 string whose length prefix starts at `pos`.
///
/// # Errors
///
/// [`IpcError::MalformedFlatbuffer`] when the length leaves the buffer or the
/// bytes are not UTF-8.
pub fn read_string(buf: &[u8], pos: usize) -> Result<&str> {
    let len = read_u32(buf, pos, "string length")? as usize;
    let start = pos + SIZE_UOFFSET;
    let end = start
        .checked_add(len)
        .ok_or(IpcError::malformed("string length", pos))?;
    let bytes = buf
        .get(start..end)
        .ok_or(IpcError::malformed("string body", pos))?;
    std::str::from_utf8(bytes).map_err(|_| IpcError::malformed("string encoding", pos))
}

/// A flatbuffer vector: an element count plus the position of element zero.
///
/// The `stride` is the byte distance between elements — 4 for vectors of
/// offsets (tables, strings), 16 for the `FieldNode`/`Buffer` struct vectors.
#[derive(Debug, Clone, Copy)]
pub struct VectorSpan<'a> {
    buf: &'a [u8],
    start: usize,
    len: usize,
    stride: usize,
}

impl<'a> VectorSpan<'a> {
    /// Reads the vector whose length prefix starts at `pos`.
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the element block leaves the
    /// buffer.
    pub fn at(buf: &'a [u8], pos: usize, stride: usize) -> Result<Self> {
        let len = read_u32(buf, pos, "vector length")? as usize;
        let start = pos + SIZE_UOFFSET;
        let span = len
            .checked_mul(stride)
            .ok_or(IpcError::malformed("vector length", pos))?;
        let end = start
            .checked_add(span)
            .ok_or(IpcError::malformed("vector length", pos))?;
        if end > buf.len() {
            return Err(IpcError::malformed("vector body", pos));
        }
        Ok(Self {
            buf,
            start,
            len,
            stride,
        })
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the vector is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The absolute position of element `index`.
    #[inline]
    #[must_use]
    pub const fn element(&self, index: usize) -> Option<usize> {
        if index >= self.len {
            None
        } else {
            Some(self.start + index * self.stride)
        }
    }

    /// Reads element `index` as a table (vectors of offsets only).
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the index is out of range or the
    /// element offset is invalid.
    pub fn table(&self, index: usize) -> Result<Table<'a>> {
        let at = self
            .element(index)
            .ok_or(IpcError::malformed("vector index", self.start))?;
        let target = deref(self.buf, at, "vector element offset")?;
        Table::at(self.buf, target)
    }

    /// Reads element `index` as a string (vectors of offsets only).
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the index is out of range or the
    /// element is not a valid string.
    pub fn string(&self, index: usize) -> Result<&'a str> {
        let at = self
            .element(index)
            .ok_or(IpcError::malformed("vector index", self.start))?;
        let target = deref(self.buf, at, "vector element offset")?;
        read_string(self.buf, target)
    }

    /// Reads the two `i64`s of a 16-byte struct element (`FieldNode`,
    /// `Buffer`).
    ///
    /// # Errors
    ///
    /// [`IpcError::MalformedFlatbuffer`] when the index is out of range.
    pub fn struct_pair(&self, index: usize) -> Result<(i64, i64)> {
        let at = self
            .element(index)
            .ok_or(IpcError::malformed("vector index", self.start))?;
        let first = read_i64(self.buf, at, "struct field")?;
        let second = read_i64(self.buf, at + 8, "struct field")?;
        Ok((first, second))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ipc::fb::FbBuilder;

    fn simple_table() -> Vec<u8> {
        let mut b = FbBuilder::new();
        let name = b.create_string("lidar");
        let table = b.start_table();
        b.push_slot_offset(0, name);
        b.push_slot_i32(1, -7, 0);
        b.push_slot_bool(2, true, false);
        b.push_slot_i64(3, i64::MIN, 0);
        let root = b.end_table(table);
        b.finish(root);
        b.finished_bytes().to_vec()
    }

    #[test]
    fn round_trips_scalars_and_strings() {
        let bytes = simple_table();
        let root = root_table(&bytes).unwrap();
        assert_eq!(root.string(0).unwrap(), Some("lidar"));
        assert_eq!(root.i32(1, 0).unwrap(), -7);
        assert!(root.bool(2, false).unwrap());
        assert_eq!(root.i64(3, 0).unwrap(), i64::MIN);
        assert_eq!(root.i32(9, 123).unwrap(), 123);
        assert_eq!(root.string(9).unwrap(), None);
        assert!(root.slot_count() >= 4);
    }

    #[test]
    fn defaults_are_not_encoded() {
        let mut b = FbBuilder::new();
        let table = b.start_table();
        b.push_slot_i32(0, 5, 5);
        b.push_slot_i32(1, 6, 5);
        let root = b.end_table(table);
        b.finish(root);
        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        assert!(root.field(0).unwrap().is_none(), "default was elided");
        assert_eq!(root.i32(0, 5).unwrap(), 5);
        assert_eq!(root.i32(1, 5).unwrap(), 6);
    }

    #[test]
    fn truncated_buffers_are_rejected_not_panicked() {
        let bytes = simple_table();
        for cut in 0..bytes.len() {
            let head = &bytes[..cut];
            match root_table(head) {
                Ok(table) => {
                    // Every getter must still refuse rather than panic.
                    let _ = table.string(0);
                    let _ = table.i32(1, 0);
                    let _ = table.i64(3, 0);
                    let _ = table.vector(0, 4);
                }
                Err(err) => assert!(err.is_malformed(), "{err}"),
            }
        }
    }

    #[test]
    fn zero_uoffset_is_rejected() {
        let buf = [0u8; 8];
        let err = root_table(&buf).unwrap_err();
        assert!(matches!(err, IpcError::MalformedFlatbuffer { .. }));
    }

    #[test]
    fn absurd_root_offset_is_rejected() {
        let mut buf = vec![0u8; 16];
        buf[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(root_table(&buf).is_err());
    }

    #[test]
    fn vectors_of_tables_and_strings() {
        let mut b = FbBuilder::new();
        let strings: Vec<_> = ["a", "bb", "ccc"]
            .into_iter()
            .map(|s| b.create_string(s))
            .collect();
        let vector = b.create_offset_vector(&strings);
        let table = b.start_table();
        b.push_slot_offset(0, vector);
        let root = b.end_table(table);
        b.finish(root);
        let bytes = b.finished_bytes().to_vec();

        let root = root_table(&bytes).unwrap();
        let span = root.vector(0, 4).unwrap().expect("vector present");
        assert_eq!(span.len(), 3);
        assert!(!span.is_empty());
        assert_eq!(span.string(0).unwrap(), "a");
        assert_eq!(span.string(2).unwrap(), "ccc");
        assert!(span.string(3).is_err());
        assert!(span.element(3).is_none());
    }

    #[test]
    fn struct_vectors_read_pairs() {
        let mut b = FbBuilder::new();
        let vector = b.create_struct_pair_vector(&[(1, 2), (3, 4), (i64::MAX, i64::MIN)]);
        let table = b.start_table();
        b.push_slot_offset(0, vector);
        let root = b.end_table(table);
        b.finish(root);
        let bytes = b.finished_bytes().to_vec();

        let root = root_table(&bytes).unwrap();
        let span = root.vector(0, 16).unwrap().expect("vector present");
        assert_eq!(span.len(), 3);
        assert_eq!(span.struct_pair(0).unwrap(), (1, 2));
        assert_eq!(span.struct_pair(2).unwrap(), (i64::MAX, i64::MIN));
        assert!(span.struct_pair(3).is_err());
    }

    #[test]
    fn negative_soffsets_are_followed() {
        // Hand-built: a table whose vtable sits *after* it, which is what
        // vtable de-duplication produces in the second and later tables.
        let mut buf = Vec::new();
        buf.extend_from_slice(&8u32.to_le_bytes()); // root offset -> 8
        buf.extend_from_slice(&[0, 0, 0, 0]); // padding
        buf.extend_from_slice(&(-8i32).to_le_bytes()); // table @8, vtable @16
        buf.extend_from_slice(&99u32.to_le_bytes()); // inline field @12
        buf.extend_from_slice(&6u16.to_le_bytes()); // vtable @16: len 6
        buf.extend_from_slice(&8u16.to_le_bytes()); // table size 8
        buf.extend_from_slice(&4u16.to_le_bytes()); // slot 0 -> table+4
        buf.extend_from_slice(&[0, 0]); // padding

        let root = root_table(&buf).unwrap();
        assert_eq!(root.i32(0, 0).unwrap(), 99);
    }

    #[test]
    fn vtable_shorter_than_header_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&8u32.to_le_bytes());
        buf.extend_from_slice(&[0, 0, 0, 0]);
        buf.extend_from_slice(&4i32.to_le_bytes()); // table @8 -> vtable @4
        buf.extend_from_slice(&[0, 0, 0, 0]);
        // vtable at 4 reads len = 0 -> rejected
        assert!(root_table(&buf).is_err());
    }

    #[test]
    fn read_helpers_report_positions() {
        let buf = [1u8, 2, 3];
        let err = read_i64(&buf, 0, "test").unwrap_err();
        match err {
            IpcError::MalformedFlatbuffer { context, position } => {
                assert_eq!(context, "test");
                assert_eq!(position, 0);
            }
            other => panic!("unexpected {other}"),
        }
        assert_eq!(read_u8(&buf, 2, "test").unwrap(), 3);
        assert!(read_u8(&buf, 3, "test").is_err());
        assert_eq!(read_u16(&buf, 0, "test").unwrap(), 0x0201);
        assert_eq!(read_i16(&buf, 0, "test").unwrap(), 0x0201);
        assert_eq!(read_i8(&buf, 0, "test").unwrap(), 1);
    }
}
