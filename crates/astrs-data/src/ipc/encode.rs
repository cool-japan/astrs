//! Flattening a [`RecordBatch`] into the `FieldNode`/`Buffer` vectors and the
//! body bytes an Arrow `RecordBatch` message carries.
//!
//! # What the planner does
//!
//! [`BatchLayout::plan`] walks every column depth-first and records, for each
//! array it meets, one `FieldNode` and the buffers listed in
//! [`crate::ipc::layout`]. It never copies value bytes: a [`Buffer`] clone is
//! one atomic increment, and the plan holds clones, so a 200 MB tensor column
//! is planned in microseconds and streamed straight out of the array's own
//! allocation.
//!
//! Exactly three situations do allocate, all of them `O(rows)` rather than
//! `O(bytes)`:
//!
//! 1. **A sliced validity bitmap.** The wire format has no bit-offset field,
//!    so a bitmap that starts mid-byte is repacked ([`Bitmap::to_canonical`]).
//! 2. **A sliced offset buffer.** Arrow requires the first offset of a
//!    variable-length array to be `0`; a slice of an array starts wherever its
//!    parent left it, so those offsets are re-based.
//! 3. **The `Vec`s of the plan itself** — two `(i64, i64)` pairs per buffer.
//!
//! # Nulls
//!
//! When a column has no nulls, the validity buffer is written with **length
//! zero** rather than as an all-ones bitmap. Both are legal: the Arrow
//! columnar specification lets a producer omit the bitmap when `null_count`
//! is `0`, arrow-cpp and pyarrow do exactly this, and arrow-rs's reader keys
//! off the field node's null count. arrow-rs's *writer* materialises the
//! all-ones bitmap instead, which is why the golden vectors under
//! `tests/golden/arrow/` show a one-byte validity buffer where AstRS writes
//! none. The saving is `ceil(rows / 8)` bytes per column per message, on
//! exactly the payloads AstRS cares most about.
//!
//! `Null` columns are the one exception to "the field node's null count is
//! [`Array::null_count`]": a `Null` array carries no validity bitmap at all,
//! so `Array::null_count` reports `0` while the wire format wants `length`
//! (every slot of a `Null` column is null by construction). arrow-rs writes
//! `length` there — see the golden `null_type.arrows`, whose field node is
//! `(4, 4)` — and so does this encoder.

use std::io::Write;

use crate::array::{
    Array, ArrayExt, ArrayRef, BooleanArray, DurationArray, FixedSizeBinaryArray,
    FixedSizeListArray, GenericBinaryArray, GenericStringArray, ListArray, OffsetSizeTrait,
    PrimitiveArray, StructArray, TimestampArray, slice_array,
};
use crate::buffer::{Bitmap, Buffer, ScalarBuffer};
use crate::datatype::{DataType, F16};
use crate::ipc::error::{IpcError, Result};
use crate::ipc::layout::MAX_NESTING_DEPTH;
use crate::ipc::message::{write_all, write_padding};
use crate::record_batch::RecordBatch;

/// The `FieldNode`s, `Buffer`s and body bytes of one record batch message.
///
/// ```
/// use astrs_data::array::{Int32Array, IntoArrayRef};
/// use astrs_data::ipc::encode::BatchLayout;
/// use astrs_data::RecordBatch;
///
/// let batch = RecordBatch::from_payload(
///     Int32Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref(),
/// );
/// let layout = BatchLayout::plan(&batch, 64)?;
/// assert_eq!(layout.nodes(), &[(3, 1)]);
/// // validity (1 byte, padded to 64) then values (12 bytes).
/// assert_eq!(layout.buffers(), &[(0, 1), (64, 12)]);
/// assert_eq!(layout.body_length(), 128);
/// # Ok::<(), astrs_data::ipc::IpcError>(())
/// ```
#[derive(Debug, Clone)]
pub struct BatchLayout {
    /// `(length, null_count)` per array, depth-first.
    nodes: Vec<(i64, i64)>,
    /// `(offset, length)` per buffer, depth-first.
    buffers: Vec<(i64, i64)>,
    /// The bytes behind each entry of `buffers`, in the same order.
    body: Vec<Buffer>,
    /// Total body length, trailing padding included.
    body_length: usize,
    /// The boundary every buffer starts on.
    alignment: usize,
}

impl BatchLayout {
    /// Plans the body of `batch`, placing every buffer on an `alignment`
    /// boundary.
    ///
    /// # Errors
    ///
    /// * [`IpcError::NestingTooDeep`] for a column nested past
    ///   [`MAX_NESTING_DEPTH`].
    /// * [`IpcError::Data`] when a column's concrete type does not match its
    ///   [`DataType`] (a corrupt array, not a corrupt stream).
    /// * [`IpcError::TooLarge`] when a length does not fit in an `i64`.
    pub fn plan(batch: &RecordBatch, alignment: usize) -> Result<Self> {
        let mut layout = Self {
            nodes: Vec::new(),
            buffers: Vec::new(),
            body: Vec::new(),
            body_length: 0,
            alignment: alignment.max(8),
        };
        for column in batch.columns() {
            layout.push_array(column.as_ref(), 1)?;
        }
        Ok(layout)
    }

    /// The `FieldNode` vector: `(length, null_count)` per array, depth-first.
    #[inline]
    #[must_use]
    pub fn nodes(&self) -> &[(i64, i64)] {
        &self.nodes
    }

    /// The `Buffer` vector: `(offset, length)` per buffer, depth-first.
    #[inline]
    #[must_use]
    pub fn buffers(&self) -> &[(i64, i64)] {
        &self.buffers
    }

    /// The bytes behind each `Buffer` entry, in the same order.
    #[inline]
    #[must_use]
    pub fn body_buffers(&self) -> &[Buffer] {
        &self.body
    }

    /// `Message.bodyLength` — every buffer plus the padding between and after
    /// them.
    #[inline]
    #[must_use]
    pub const fn body_length(&self) -> i64 {
        self.body_length as i64
    }

    /// Total bytes of actual data, padding excluded. Useful for accounting.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.body.iter().map(Buffer::len).sum()
    }

    /// Streams the body to `sink`, inserting the padding the plan reserved.
    ///
    /// Returns the number of bytes written, which always equals
    /// [`BatchLayout::body_length`].
    ///
    /// # Errors
    ///
    /// [`IpcError::Io`] when the sink fails.
    pub fn write_body<W: Write>(&self, sink: &mut W) -> Result<usize> {
        let mut cursor = 0usize;
        for (entry, buffer) in self.buffers.iter().zip(self.body.iter()) {
            let offset = usize::try_from(entry.0).unwrap_or(usize::MAX);
            let gap = offset.saturating_sub(cursor);
            write_padding(sink, gap, "body buffer alignment")?;
            write_all(sink, buffer.as_slice(), "record batch body")?;
            cursor = offset.saturating_add(buffer.len());
        }
        let tail = self.body_length.saturating_sub(cursor);
        write_padding(sink, tail, "body tail alignment")?;
        Ok(self.body_length)
    }

    /// Appends one buffer entry and its bytes, keeping the cursor aligned.
    fn push_buffer(&mut self, bytes: Buffer) -> Result<()> {
        let offset = self.body_length;
        let len = bytes.len();
        self.buffers.push((to_i64(offset)?, to_i64(len)?));
        self.body.push(bytes);
        self.body_length = offset
            .checked_add(len)
            .ok_or(IpcError::TooLarge {
                what: "body",
                length: u64::MAX,
                cap: crate::MAX_PAYLOAD_BYTES as u64,
            })?
            .next_multiple_of(self.alignment);
        Ok(())
    }

    /// Appends the validity buffer of `array` — empty when it has no nulls.
    fn push_validity(&mut self, array: &dyn Array) -> Result<()> {
        match array.validity() {
            Some(bits) if bits.count_unset() > 0 => {
                self.push_buffer(canonical_bits(bits))?;
            }
            _ => self.push_buffer(Buffer::new())?,
        }
        Ok(())
    }

    /// Appends one array's node, buffers and children.
    fn push_array(&mut self, array: &dyn Array, depth: usize) -> Result<()> {
        if depth > MAX_NESTING_DEPTH {
            return Err(IpcError::NestingTooDeep {
                depth,
                limit: MAX_NESTING_DEPTH,
            });
        }
        let len = array.len();
        let data_type = array.data_type().clone();
        // A `Null` column has no validity bitmap, so `Array::null_count`
        // reports 0 — but every one of its slots *is* null, and that is what
        // the field node must say (golden `null_type.arrows`: node `(4, 4)`).
        let null_count = if matches!(data_type, DataType::Null) {
            len
        } else {
            array.null_count()
        };
        self.nodes.push((to_i64(len)?, to_i64(null_count)?));

        match &data_type {
            DataType::Null => {}
            DataType::Bool => {
                let typed = array.try_downcast::<BooleanArray>()?;
                self.push_validity(array)?;
                self.push_buffer(canonical_bits(typed.values()))?;
            }
            DataType::Int8 => self.push_primitive::<i8>(array)?,
            DataType::Int16 => self.push_primitive::<i16>(array)?,
            DataType::Int32 => self.push_primitive::<i32>(array)?,
            DataType::Int64 => self.push_primitive::<i64>(array)?,
            DataType::UInt8 => self.push_primitive::<u8>(array)?,
            DataType::UInt16 => self.push_primitive::<u16>(array)?,
            DataType::UInt32 => self.push_primitive::<u32>(array)?,
            DataType::UInt64 => self.push_primitive::<u64>(array)?,
            DataType::Float16 => self.push_primitive::<F16>(array)?,
            DataType::Float32 => self.push_primitive::<f32>(array)?,
            DataType::Float64 => self.push_primitive::<f64>(array)?,
            DataType::Timestamp => {
                let typed = array.try_downcast::<TimestampArray>()?;
                self.push_validity(array)?;
                self.push_buffer(typed.values_buffer().inner().clone())?;
            }
            DataType::Duration => {
                let typed = array.try_downcast::<DurationArray>()?;
                self.push_validity(array)?;
                self.push_buffer(typed.values_buffer().inner().clone())?;
            }
            DataType::FixedSizeBinary(size) => {
                let typed = array.try_downcast::<FixedSizeBinaryArray>()?;
                self.push_validity(array)?;
                let width = usize::try_from(*size).unwrap_or(0);
                let exact = len.saturating_mul(width);
                self.push_buffer(typed.value_data().slice(0, exact))?;
            }
            DataType::Binary => self.push_binary::<i32>(array)?,
            DataType::LargeBinary => self.push_binary::<i64>(array)?,
            DataType::Utf8 => self.push_string::<i32>(array)?,
            DataType::LargeUtf8 => self.push_string::<i64>(array)?,
            DataType::List(_) => {
                let typed = array.try_downcast::<ListArray>()?;
                self.push_validity(array)?;
                let window = offsets_window(typed.offsets_buffer(), len)?;
                self.push_buffer(window.buffer)?;
                let child = slice_array(typed.values(), window.start, window.len);
                self.push_array(child.as_ref(), depth + 1)?;
            }
            DataType::FixedSizeList(_, _) => {
                let typed = array.try_downcast::<FixedSizeListArray>()?;
                self.push_validity(array)?;
                self.push_array(typed.values().as_ref(), depth + 1)?;
            }
            DataType::Struct(_) => {
                let typed = array.try_downcast::<StructArray>()?;
                self.push_validity(array)?;
                for column in typed.columns() {
                    self.push_child_column(column, len, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    /// Pushes a struct child, narrowing it to the parent's length first.
    ///
    /// [`StructArray`] already keeps every column exactly as long as the
    /// struct, so the slice is a no-op in practice; it is here so a column
    /// that somehow disagrees produces a well-formed message rather than a
    /// field node the reader cannot reconcile.
    fn push_child_column(&mut self, column: &ArrayRef, len: usize, depth: usize) -> Result<()> {
        if column.len() == len {
            self.push_array(column.as_ref(), depth)
        } else {
            let narrowed = slice_array(column, 0, len);
            self.push_array(narrowed.as_ref(), depth)
        }
    }

    /// Validity plus a fixed-width values buffer.
    fn push_primitive<T: crate::datatype::ArrowNativeType>(
        &mut self,
        array: &dyn Array,
    ) -> Result<()> {
        let typed = array.try_downcast::<PrimitiveArray<T>>()?;
        self.push_validity(array)?;
        self.push_buffer(typed.values_buffer().inner().clone())
    }

    /// Validity, offsets and the value region of a binary array.
    fn push_binary<O: OffsetSizeTrait>(&mut self, array: &dyn Array) -> Result<()> {
        let typed = array.try_downcast::<GenericBinaryArray<O>>()?;
        self.push_validity(array)?;
        let window = offsets_window(typed.offsets_buffer(), typed.len())?;
        self.push_buffer(window.buffer)?;
        self.push_buffer(typed.value_data().slice(window.start, window.len))
    }

    /// Validity, offsets and the value region of a string array.
    fn push_string<O: OffsetSizeTrait>(&mut self, array: &dyn Array) -> Result<()> {
        let typed = array.try_downcast::<GenericStringArray<O>>()?;
        self.push_validity(array)?;
        let window = offsets_window(typed.offsets_buffer(), typed.len())?;
        self.push_buffer(window.buffer)?;
        self.push_buffer(typed.value_data().slice(window.start, window.len))
    }
}

/// The wire form of an offset buffer plus the child window it selects.
struct OffsetWindow {
    /// Offsets re-based to start at zero.
    buffer: Buffer,
    /// First child element the array covers.
    start: usize,
    /// Number of child elements the array covers.
    len: usize,
}

/// Re-bases `offsets` to start at zero and reports the child window.
///
/// The common case — an array that was never sliced — takes the fast path:
/// the existing buffer is shared, not copied.
fn offsets_window<O: OffsetSizeTrait>(
    offsets: &ScalarBuffer<O>,
    len: usize,
) -> Result<OffsetWindow> {
    let entries = offsets.as_slice();
    let Some(&first) = entries.first() else {
        // An array with no offsets at all can only be empty; Arrow spells that
        // as a zero-length offset buffer.
        return Ok(OffsetWindow {
            buffer: Buffer::new(),
            start: 0,
            len: 0,
        });
    };
    let last = entries.get(len).copied().unwrap_or(first);
    let start = first
        .to_usize()
        .ok_or(IpcError::malformed("offset base", 0))?;
    let end = last
        .to_usize()
        .ok_or(IpcError::malformed("offset end", 0))?;
    let span = end.saturating_sub(start);

    if start == 0 {
        return Ok(OffsetWindow {
            buffer: offsets.inner().clone(),
            start,
            len: span,
        });
    }
    let mut rebased = Vec::with_capacity(entries.len());
    for &offset in entries {
        let value = offset
            .to_usize()
            .ok_or(IpcError::malformed("offset entry", 0))?
            .saturating_sub(start);
        rebased.push(O::from_usize(value).ok_or(IpcError::TooLarge {
            what: "offset",
            length: value as u64,
            cap: i64::MAX as u64,
        })?);
    }
    Ok(OffsetWindow {
        buffer: Buffer::from_scalars(&rebased),
        start,
        len: span,
    })
}

/// The tightly packed, zero-offset bytes of a bitmap.
fn canonical_bits(bits: &Bitmap) -> Buffer {
    bits.to_canonical().buffer().clone()
}

/// Widens a length for the wire format.
fn to_i64(value: usize) -> Result<i64> {
    i64::try_from(value).map_err(|_| IpcError::TooLarge {
        what: "length",
        length: u64::MAX,
        cap: i64::MAX as u64,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int32Array, IntoArrayRef, NullArray, StringArray, UInt8Array};
    use crate::datatype::{Field, Schema};
    use std::sync::Arc;

    fn plan(batch: &RecordBatch, alignment: usize) -> BatchLayout {
        BatchLayout::plan(batch, alignment).expect("plan")
    }

    fn body_of(layout: &BatchLayout) -> Vec<u8> {
        let mut sink = Vec::new();
        let written = layout.write_body(&mut sink).expect("body");
        assert_eq!(written, sink.len());
        sink
    }

    #[test]
    fn a_null_column_contributes_a_node_and_no_buffers() {
        let batch = RecordBatch::from_payload(NullArray::new(4).into_array_ref());
        let layout = plan(&batch, 8);
        assert_eq!(layout.nodes(), &[(4, 4)], "every slot of a Null is null");
        assert!(layout.buffers().is_empty());
        assert_eq!(layout.body_length(), 0);
        assert!(body_of(&layout).is_empty());
    }

    #[test]
    fn a_column_without_nulls_writes_an_empty_validity_buffer() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2, 3]).into_array_ref());
        let layout = plan(&batch, 8);
        assert_eq!(layout.nodes(), &[(3, 0)]);
        assert_eq!(layout.buffers(), &[(0, 0), (0, 12)]);
        assert_eq!(layout.body_length(), 16);
        assert_eq!(body_of(&layout).len(), 16);
    }

    #[test]
    fn buffers_land_on_the_requested_alignment() {
        let batch = RecordBatch::from_payload(
            StringArray::from_opt_iter([Some("aa"), None, Some("bbb")]).into_array_ref(),
        );
        for alignment in [8usize, 16, 64, 128] {
            let layout = plan(&batch, alignment);
            for (offset, _) in layout.buffers() {
                assert_eq!(
                    *offset as usize % alignment,
                    0,
                    "alignment {alignment}, offset {offset}"
                );
            }
            assert_eq!(layout.body_length() as usize % alignment, 0);
            assert_eq!(body_of(&layout).len(), layout.body_length() as usize);
        }
    }

    #[test]
    fn slicing_rebases_offsets_and_narrows_the_child() {
        let column = StringArray::from_values(["zero", "one", "two", "three"]).into_array_ref();
        let sliced = crate::array::slice_array(&column, 2, 2);
        let batch = RecordBatch::from_payload(sliced);
        let layout = plan(&batch, 8);

        assert_eq!(layout.nodes(), &[(2, 0)]);
        let values = layout.body_buffers().last().expect("values buffer");
        assert_eq!(values.as_slice(), b"twothree");
        let offsets = &layout.body_buffers()[1];
        let typed = offsets.typed::<i32>().expect("aligned");
        assert_eq!(typed.as_slice(), &[0, 3, 8], "offsets are re-based to zero");
    }

    #[test]
    fn a_sliced_bitmap_is_repacked() {
        let column =
            Int32Array::from_opt_iter([Some(1), None, Some(3), None, Some(5), Some(6), None, None])
                .into_array_ref();
        let sliced = crate::array::slice_array(&column, 3, 4);
        let batch = RecordBatch::from_payload(sliced);
        let layout = plan(&batch, 8);
        // Rows 3..7 of the source are null, 5, 6, null.
        assert_eq!(layout.nodes(), &[(4, 2)]);
        let validity = &layout.body_buffers()[0];
        assert_eq!(validity.len(), 1, "one byte covers four slots");
        assert_eq!(
            validity.as_slice()[0] & 0x0f,
            0b0110,
            "the bitmap is repacked to bit offset zero"
        );
    }

    #[test]
    fn nested_types_emit_depth_first_nodes() {
        let child = Int32Array::from_values([1, 2, 3, 4]).into_array_ref();
        let lists = ListArray::try_from_lengths(
            Field::new("item", DataType::Int32, false),
            [2usize, 1, 1],
            child,
        )
        .expect("lists");
        let batch = RecordBatch::from_payload(lists.into_array_ref());
        let layout = plan(&batch, 8);
        assert_eq!(layout.nodes(), &[(3, 0), (4, 0)]);
        // list: validity, offsets; child: validity, values.
        assert_eq!(layout.buffers().len(), 4);
    }

    #[test]
    fn struct_children_follow_the_parent() {
        let fields = vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Utf8, true),
        ];
        let columns = vec![
            Int32Array::from_opt_iter([Some(1), None]).into_array_ref(),
            StringArray::from_values(["x", "y"]).into_array_ref(),
        ];
        let strukt = StructArray::try_new(fields, columns, None).expect("struct");
        let batch = RecordBatch::from_payload(strukt.into_array_ref());
        let layout = plan(&batch, 8);
        assert_eq!(layout.nodes(), &[(2, 0), (2, 1), (2, 0)]);
        assert_eq!(layout.buffers().len(), 1 + 2 + 3);
    }

    #[test]
    fn payload_bytes_excludes_padding() {
        let batch = RecordBatch::from_payload(
            UInt8Array::from_values((0..100u8).collect::<Vec<_>>()).into_array_ref(),
        );
        let layout = plan(&batch, 64);
        assert_eq!(layout.payload_bytes(), 100);
        assert_eq!(layout.body_length(), 128);
    }

    #[test]
    fn empty_batches_have_empty_bodies() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Utf8, true),
            Field::new("b", DataType::Bool, true),
        ]));
        let batch = RecordBatch::try_new_empty(schema).expect("empty");
        let layout = plan(&batch, 64);
        assert_eq!(layout.nodes(), &[(0, 0), (0, 0)]);
        // utf8: validity, offsets, values — then bool: validity, values. Only
        // the offset buffer carries anything, and only its single leading `0`.
        assert_eq!(
            layout.buffers(),
            &[(0, 0), (0, 4), (64, 0), (64, 0), (64, 0)]
        );
        assert_eq!(layout.payload_bytes(), 4);
        assert_eq!(body_of(&layout).len(), layout.body_length() as usize);
    }

    #[test]
    fn zero_column_batches_carry_only_a_row_count() {
        let schema = Arc::new(Schema::new(Vec::new()));
        let batch =
            RecordBatch::try_new_with_row_count(schema, Vec::new(), 5).expect("zero columns");
        let layout = plan(&batch, 64);
        assert!(layout.nodes().is_empty());
        assert!(layout.buffers().is_empty());
        assert_eq!(layout.body_length(), 0);
    }
}
