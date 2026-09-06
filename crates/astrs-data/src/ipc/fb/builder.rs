//! A minimal FlatBuffers *writer*.
//!
//! This is the canonical back-to-front builder every FlatBuffers
//! implementation uses: objects are appended towards lower addresses, so a
//! parent written after its children can store plain forward `uoffset_t`s to
//! them. The consequences the caller must respect:
//!
//! 1. **Children first.** Strings, vectors and nested tables must be created
//!    *before* the table that refers to them.
//! 2. **One table at a time.** Between [`FbBuilder::start_table`] and
//!    [`FbBuilder::end_table`] no other object may be created.
//! 3. **Defaults are elided.** `push_slot_*` skips a field whose value equals
//!    the schema default, exactly like the reference implementation — which is
//!    why every reader getter takes a default.
//!
//! Vtables are de-duplicated: a table whose vtable is byte-identical to one
//! already written reuses it, which makes the `soffset_t` negative. That is
//! legal and is what the reference implementation emits too, so
//! [`Table::at`](super::Table::at) follows negative offsets.
//!
//! # Alignment model
//!
//! Positions are tracked as *distance from the end* of the finished buffer.
//! An object needs `address % A == 0`; with `address = end - distance` that is
//! `distance % A == 0` provided the finished buffer's end is itself
//! `A`-aligned. [`FbBuilder::finish`] pads the front so the total length is a
//! multiple of the largest alignment used, so placing the finished bytes at an
//! address aligned to that same value satisfies every interior object. Arrow
//! puts the metadata block at an 8-byte boundary, which is exactly the
//! guarantee these tables need.
//!
//! ```
//! use astrs_data::ipc::fb::{FbBuilder, root_table};
//!
//! let mut builder = FbBuilder::new();
//! let unit = builder.create_string("m/s");
//! let table = builder.start_table();
//! builder.push_slot_offset(0, unit);
//! builder.push_slot_i64(1, 9_000_000_000, 0);
//! let root = builder.end_table(table);
//! builder.finish(root);
//!
//! assert_eq!(builder.finished_bytes().len() % 8, 0);
//! let root = root_table(builder.finished_bytes())?;
//! assert_eq!(root.string(0)?, Some("m/s"));
//! assert_eq!(root.i64(1, 0)?, 9_000_000_000);
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

use crate::ipc::fb::reader::SIZE_UOFFSET;

/// The initial working capacity, enough for a small schema without a regrow.
const INITIAL_CAPACITY: usize = 1024;

/// A reference to a finished object, expressed as its distance from the end of
/// the buffer.
///
/// Opaque on purpose: the numeric value only means something to the builder
/// that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WipOffset(usize);

impl WipOffset {
    /// The raw distance-from-end, for tests and diagnostics.
    #[inline]
    #[must_use]
    pub const fn value(self) -> usize {
        self.0
    }
}

/// The marker returned by [`FbBuilder::start_table`] and consumed by
/// [`FbBuilder::end_table`].
#[derive(Debug, Clone, Copy)]
pub struct TableStart(usize);

/// Where one table field ended up, so the vtable can be built at `end_table`.
#[derive(Debug, Clone, Copy)]
struct FieldLoc {
    slot: usize,
    distance: usize,
}

/// A back-to-front FlatBuffers builder.
///
/// See the [module documentation](self) for the three usage rules.
#[derive(Debug)]
pub struct FbBuilder {
    /// Working storage; the live bytes are `buf[head..]`.
    buf: Vec<u8>,
    /// Index of the first live byte.
    head: usize,
    /// Largest alignment any object required.
    min_align: usize,
    /// Fields of the table currently being built.
    field_locs: Vec<FieldLoc>,
    /// Whether a table is open.
    nested: bool,
    /// Distances of the vtables written so far, for de-duplication.
    vtables: Vec<usize>,
}

impl Default for FbBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FbBuilder {
    /// Creates a builder with a default working capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(INITIAL_CAPACITY)
    }

    /// Creates a builder with room for `capacity` bytes before the first
    /// regrow.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(8);
        Self {
            buf: vec![0u8; capacity],
            head: capacity,
            min_align: 1,
            field_locs: Vec::new(),
            nested: false,
            vtables: Vec::new(),
        }
    }

    /// Drops every object, keeping the allocation for the next message.
    ///
    /// The stream writer reuses one builder for every message, so the
    /// per-message allocation cost is zero after the first batch.
    pub fn reset(&mut self) {
        self.head = self.buf.len();
        self.min_align = 1;
        self.field_locs.clear();
        self.nested = false;
        self.vtables.clear();
    }

    /// Bytes written so far, measured from the end of the finished buffer.
    #[inline]
    #[must_use]
    pub const fn used(&self) -> usize {
        self.buf.len() - self.head
    }

    /// The finished bytes. Only meaningful after [`FbBuilder::finish`].
    #[inline]
    #[must_use]
    pub fn finished_bytes(&self) -> &[u8] {
        &self.buf[self.head..]
    }

    /// The largest alignment any interior object required.
    ///
    /// The finished bytes must be placed at an address that is a multiple of
    /// this value; [`FbBuilder::finish`] also pads the total length to it.
    #[inline]
    #[must_use]
    pub const fn min_alignment(&self) -> usize {
        self.min_align
    }

    // ---------------------------------------------------------------- raw

    /// Makes room for `n` more bytes at the front, growing if needed.
    fn make_space(&mut self, n: usize) {
        if self.head >= n {
            return;
        }
        let used = self.used();
        let needed = used + n;
        let new_len = needed.next_power_of_two().max(INITIAL_CAPACITY);
        let mut next = vec![0u8; new_len];
        let start = new_len - used;
        next[start..].copy_from_slice(&self.buf[self.head..]);
        self.buf = next;
        self.head = start;
    }

    /// Appends bytes verbatim, keeping their order, with no alignment.
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.make_space(bytes.len());
        self.head -= bytes.len();
        self.buf[self.head..self.head + bytes.len()].copy_from_slice(bytes);
    }

    /// Appends `count` zero bytes.
    fn push_zeros(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.make_space(count);
        self.head -= count;
        self.buf[self.head..self.head + count].fill(0);
    }

    /// Pads so that an object of `additional` bytes written next starts at a
    /// distance that is a multiple of `alignment`.
    fn pre_align(&mut self, additional: usize, alignment: usize) {
        debug_assert!(alignment.is_power_of_two());
        if alignment > self.min_align {
            self.min_align = alignment;
        }
        let misalignment = (self.used() + additional) % alignment;
        if misalignment != 0 {
            self.push_zeros(alignment - misalignment);
        }
    }

    /// Overwrites 4 bytes at `distance` (a previously recorded position).
    fn patch_i32(&mut self, distance: usize, value: i32) {
        let index = self.buf.len() - distance;
        self.buf[index..index + 4].copy_from_slice(&value.to_le_bytes());
    }

    // ------------------------------------------------------------- scalars

    /// Appends a `u8` (alignment 1).
    pub fn push_u8(&mut self, value: u8) {
        self.push_bytes(&[value]);
    }

    /// Appends an `i8` (alignment 1).
    pub fn push_i8(&mut self, value: i8) {
        self.push_bytes(&value.to_le_bytes());
    }

    /// Appends a little-endian `u16`, aligned to 2.
    pub fn push_u16(&mut self, value: u16) {
        self.pre_align(2, 2);
        self.push_bytes(&value.to_le_bytes());
    }

    /// Appends a little-endian `i16`, aligned to 2.
    pub fn push_i16(&mut self, value: i16) {
        self.pre_align(2, 2);
        self.push_bytes(&value.to_le_bytes());
    }

    /// Appends a little-endian `u32`, aligned to 4.
    pub fn push_u32(&mut self, value: u32) {
        self.pre_align(4, 4);
        self.push_bytes(&value.to_le_bytes());
    }

    /// Appends a little-endian `i32`, aligned to 4.
    pub fn push_i32(&mut self, value: i32) {
        self.pre_align(4, 4);
        self.push_bytes(&value.to_le_bytes());
    }

    /// Appends a little-endian `i64`, aligned to 8.
    pub fn push_i64(&mut self, value: i64) {
        self.pre_align(8, 8);
        self.push_bytes(&value.to_le_bytes());
    }

    /// Appends a forward reference to an already-created object.
    pub fn push_offset(&mut self, target: WipOffset) {
        self.pre_align(SIZE_UOFFSET, SIZE_UOFFSET);
        let value = (self.used() + SIZE_UOFFSET) - target.0;
        // `value` cannot exceed u32::MAX for any message this crate builds:
        // the payload cap is 256 MiB and metadata is a fraction of that.
        self.push_bytes(&(value as u32).to_le_bytes());
    }

    // ------------------------------------------------------------- objects

    /// Creates a length-prefixed, NUL-terminated UTF-8 string.
    ///
    /// Must not be called while a table is open.
    pub fn create_string(&mut self, value: &str) -> WipOffset {
        debug_assert!(!self.nested, "create_string inside an open table");
        let bytes = value.as_bytes();
        self.pre_align(bytes.len() + 1, SIZE_UOFFSET);
        self.push_u8(0);
        self.push_bytes(bytes);
        self.push_u32(bytes.len() as u32);
        WipOffset(self.used())
    }

    /// Creates a vector of forward references (tables or strings).
    ///
    /// Must not be called while a table is open.
    pub fn create_offset_vector(&mut self, items: &[WipOffset]) -> WipOffset {
        debug_assert!(!self.nested, "create_offset_vector inside an open table");
        self.pre_align(items.len() * SIZE_UOFFSET, SIZE_UOFFSET);
        for item in items.iter().rev() {
            self.push_offset(*item);
        }
        self.push_u32(items.len() as u32);
        WipOffset(self.used())
    }

    /// Creates a vector of 16-byte, two-`i64` structs — the `FieldNode` and
    /// `Buffer` vectors of a `RecordBatch`.
    ///
    /// The pair is written in declaration order: `.0` first, `.1` second.
    ///
    /// Must not be called while a table is open.
    pub fn create_struct_pair_vector(&mut self, items: &[(i64, i64)]) -> WipOffset {
        debug_assert!(
            !self.nested,
            "create_struct_pair_vector inside an open table"
        );
        self.pre_align(items.len() * 16, 8);
        for (first, second) in items.iter().rev() {
            self.push_bytes(&second.to_le_bytes());
            self.push_bytes(&first.to_le_bytes());
        }
        self.push_u32(items.len() as u32);
        WipOffset(self.used())
    }

    // -------------------------------------------------------------- tables

    /// Opens a table. Every `push_slot_*` until [`FbBuilder::end_table`]
    /// belongs to it.
    pub fn start_table(&mut self) -> TableStart {
        debug_assert!(!self.nested, "nested start_table");
        self.nested = true;
        self.field_locs.clear();
        TableStart(self.used())
    }

    fn track_field(&mut self, slot: usize) {
        let distance = self.used();
        self.field_locs.push(FieldLoc { slot, distance });
    }

    /// Writes a `bool` slot unless it equals the schema default.
    pub fn push_slot_bool(&mut self, slot: usize, value: bool, default: bool) {
        if value == default {
            return;
        }
        self.push_u8(u8::from(value));
        self.track_field(slot);
    }

    /// Writes a `u8` slot unless it equals the schema default.
    pub fn push_slot_u8(&mut self, slot: usize, value: u8, default: u8) {
        if value == default {
            return;
        }
        self.push_u8(value);
        self.track_field(slot);
    }

    /// Writes an `i8` slot unless it equals the schema default.
    pub fn push_slot_i8(&mut self, slot: usize, value: i8, default: i8) {
        if value == default {
            return;
        }
        self.push_i8(value);
        self.track_field(slot);
    }

    /// Writes an `i16` slot unless it equals the schema default.
    pub fn push_slot_i16(&mut self, slot: usize, value: i16, default: i16) {
        if value == default {
            return;
        }
        self.push_i16(value);
        self.track_field(slot);
    }

    /// Writes an `i32` slot unless it equals the schema default.
    pub fn push_slot_i32(&mut self, slot: usize, value: i32, default: i32) {
        if value == default {
            return;
        }
        self.push_i32(value);
        self.track_field(slot);
    }

    /// Writes an `i64` slot unless it equals the schema default.
    pub fn push_slot_i64(&mut self, slot: usize, value: i64, default: i64) {
        if value == default {
            return;
        }
        self.push_i64(value);
        self.track_field(slot);
    }

    /// Writes a reference slot. Reference fields have no default, so this
    /// always emits.
    pub fn push_slot_offset(&mut self, slot: usize, target: WipOffset) {
        self.push_offset(target);
        self.track_field(slot);
    }

    /// Closes the table, emitting (or reusing) its vtable.
    pub fn end_table(&mut self, start: TableStart) -> WipOffset {
        debug_assert!(self.nested, "end_table without start_table");
        self.pre_align(SIZE_UOFFSET, SIZE_UOFFSET);
        self.push_zeros(SIZE_UOFFSET); // placeholder for the vtable soffset
        let table_distance = self.used();
        let table_size = table_distance - start.0;

        let slot_count = self
            .field_locs
            .iter()
            .map(|f| f.slot + 1)
            .max()
            .unwrap_or(0);
        let vtable_len = 2 * 2 + 2 * slot_count;
        let mut vtable = vec![0u8; vtable_len];
        vtable[0..2].copy_from_slice(&(vtable_len as u16).to_le_bytes());
        vtable[2..4].copy_from_slice(&(table_size as u16).to_le_bytes());
        for field in &self.field_locs {
            let entry = (table_distance - field.distance) as u16;
            let at = 4 + 2 * field.slot;
            vtable[at..at + 2].copy_from_slice(&entry.to_le_bytes());
        }

        let vtable_distance = match self.find_vtable(&vtable) {
            Some(existing) => existing,
            None => {
                self.pre_align(vtable_len, 2);
                self.push_bytes(&vtable);
                let distance = self.used();
                self.vtables.push(distance);
                distance
            }
        };

        // Positive when the vtable was written after the table (the common
        // case), negative when a previously written vtable was reused.
        let soffset = vtable_distance as i64 - table_distance as i64;
        self.patch_i32(table_distance, soffset as i32);
        self.nested = false;
        self.field_locs.clear();
        WipOffset(table_distance)
    }

    /// Finds a byte-identical vtable already in the buffer.
    fn find_vtable(&self, candidate: &[u8]) -> Option<usize> {
        let total = self.buf.len();
        self.vtables.iter().copied().find(|&distance| {
            let start = total - distance;
            self.buf
                .get(start..start + candidate.len())
                .is_some_and(|existing| existing == candidate)
        })
    }

    /// Writes the root offset and pads the total length to the buffer's
    /// alignment requirement.
    pub fn finish(&mut self, root: WipOffset) {
        debug_assert!(!self.nested, "finish with an open table");
        let alignment = self.min_align.max(SIZE_UOFFSET);
        self.pre_align(SIZE_UOFFSET, alignment);
        self.push_offset(root);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ipc::fb::reader::{Table, root_table};

    #[test]
    fn finished_length_matches_alignment() {
        let mut b = FbBuilder::new();
        let table = b.start_table();
        b.push_slot_i64(0, 1, 0);
        let root = b.end_table(table);
        b.finish(root);
        assert_eq!(b.min_alignment(), 8);
        assert_eq!(b.finished_bytes().len() % 8, 0);
    }

    #[test]
    fn interior_objects_land_on_their_alignment() {
        let mut b = FbBuilder::new();
        let s = b.create_string("abcdefghij");
        let table = b.start_table();
        b.push_slot_offset(0, s);
        b.push_slot_i64(1, -1, 0);
        b.push_slot_i16(2, 7, 0);
        let root = b.end_table(table);
        b.finish(root);

        let bytes = b.finished_bytes();
        let root = root_table(bytes).unwrap();
        // The i64 field must sit on an 8-byte boundary inside the buffer.
        let at = root.field(1).unwrap().expect("field present");
        assert_eq!(at % 8, 0, "i64 field at {at}");
        assert_eq!(root.i64(1, 0).unwrap(), -1);
        assert_eq!(root.i16(2, 0).unwrap(), 7);
        assert_eq!(root.string(0).unwrap(), Some("abcdefghij"));
    }

    #[test]
    fn vtables_are_deduplicated() {
        let mut b = FbBuilder::new();
        let mut tables = Vec::new();
        for value in 0..6i32 {
            let t = b.start_table();
            b.push_slot_i32(0, value + 1, 0);
            b.push_slot_bool(1, true, false);
            tables.push(b.end_table(t));
        }
        assert_eq!(b.vtables.len(), 1, "identical layouts share one vtable");

        let vector = b.create_offset_vector(&tables);
        let root_start = b.start_table();
        b.push_slot_offset(0, vector);
        let root = b.end_table(root_start);
        b.finish(root);

        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        let span = root.vector(0, 4).unwrap().expect("vector");
        assert_eq!(span.len(), 6);
        for value in 0..6usize {
            let t = span.table(value).unwrap();
            assert_eq!(t.i32(0, 0).unwrap(), value as i32 + 1);
            assert!(t.bool(1, false).unwrap());
        }
    }

    #[test]
    fn empty_table_and_empty_vector() {
        let mut b = FbBuilder::new();
        let empty_vec = b.create_offset_vector(&[]);
        let inner = b.start_table();
        let inner = b.end_table(inner);
        let outer = b.start_table();
        b.push_slot_offset(0, empty_vec);
        b.push_slot_offset(1, inner);
        let root = b.end_table(outer);
        b.finish(root);

        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        assert_eq!(root.vector(0, 4).unwrap().expect("vector").len(), 0);
        let nested = root.table(1).unwrap().expect("table");
        assert_eq!(nested.slot_count(), 0);
        assert_eq!(nested.i32(0, 11).unwrap(), 11);
    }

    #[test]
    fn struct_vectors_preserve_field_order() {
        let mut b = FbBuilder::new();
        let v = b.create_struct_pair_vector(&[(10, 20), (30, 40)]);
        let t = b.start_table();
        b.push_slot_offset(0, v);
        let root = b.end_table(t);
        b.finish(root);

        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        let span = root.vector(0, 16).unwrap().expect("vector");
        assert_eq!(span.struct_pair(0).unwrap(), (10, 20));
        assert_eq!(span.struct_pair(1).unwrap(), (30, 40));
        // Struct elements must be 8-aligned.
        assert_eq!(span.element(0).expect("element") % 8, 0);
    }

    #[test]
    fn regrow_preserves_content() {
        let mut b = FbBuilder::with_capacity(8);
        let strings: Vec<_> = (0..64)
            .map(|i| b.create_string(&format!("field-name-number-{i:03}")))
            .collect();
        let vector = b.create_offset_vector(&strings);
        let t = b.start_table();
        b.push_slot_offset(0, vector);
        let root = b.end_table(t);
        b.finish(root);

        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        let span = root.vector(0, 4).unwrap().expect("vector");
        assert_eq!(span.len(), 64);
        for i in 0..64usize {
            assert_eq!(span.string(i).unwrap(), format!("field-name-number-{i:03}"));
        }
    }

    #[test]
    fn reset_reuses_the_allocation() {
        let mut b = FbBuilder::new();
        let s = b.create_string("first");
        let t = b.start_table();
        b.push_slot_offset(0, s);
        let root = b.end_table(t);
        b.finish(root);
        let first_len = b.finished_bytes().len();

        b.reset();
        assert_eq!(b.used(), 0);
        let s = b.create_string("second");
        let t = b.start_table();
        b.push_slot_offset(0, s);
        let root = b.end_table(t);
        b.finish(root);
        assert!(b.finished_bytes().len() >= first_len);

        // A reset builder must produce exactly what a fresh one would.
        let mut fresh = FbBuilder::new();
        let s = fresh.create_string("second");
        let t = fresh.start_table();
        fresh.push_slot_offset(0, s);
        let root = fresh.end_table(t);
        fresh.finish(root);
        assert_eq!(b.finished_bytes(), fresh.finished_bytes());

        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        assert_eq!(root.string(0).unwrap(), Some("second"));
    }

    #[test]
    fn deeply_nested_tables_round_trip() {
        // Build a right-leaning chain of 32 tables, each pointing at the one
        // created before it.
        let mut b = FbBuilder::new();
        let leaf = b.start_table();
        b.push_slot_i32(0, 0, -1);
        let mut current = b.end_table(leaf);
        for depth in 1..32i32 {
            let t = b.start_table();
            b.push_slot_i32(0, depth, -1);
            b.push_slot_offset(1, current);
            current = b.end_table(t);
        }
        b.finish(current);

        let bytes = b.finished_bytes().to_vec();
        let mut table: Table<'_> = root_table(&bytes).unwrap();
        for depth in (0..32i32).rev() {
            assert_eq!(table.i32(0, -1).unwrap(), depth);
            match table.table(1).unwrap() {
                Some(child) => table = child,
                None => assert_eq!(depth, 0),
            }
        }
    }

    #[test]
    fn strings_with_multibyte_and_empty_content() {
        let mut b = FbBuilder::new();
        let empty = b.create_string("");
        let uni = b.create_string("速度 🦀");
        let t = b.start_table();
        b.push_slot_offset(0, empty);
        b.push_slot_offset(1, uni);
        let root = b.end_table(t);
        b.finish(root);

        let bytes = b.finished_bytes().to_vec();
        let root = root_table(&bytes).unwrap();
        assert_eq!(root.string(0).unwrap(), Some(""));
        assert_eq!(root.string(1).unwrap(), Some("速度 🦀"));
    }

    #[test]
    fn wip_offsets_expose_their_distance() {
        let mut b = FbBuilder::new();
        let s = b.create_string("x");
        assert!(s.value() > 0);
        assert_eq!(b.used(), s.value());
    }

    #[test]
    fn default_builder_matches_new() {
        let a = FbBuilder::default();
        let b = FbBuilder::new();
        assert_eq!(a.used(), b.used());
        assert_eq!(a.min_alignment(), b.min_alignment());
    }
}
