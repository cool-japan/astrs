//! Rebuilding arrays from a record batch body.
//!
//! The mirror of [`crate::ipc::encode`]: it walks the schema depth-first,
//! consuming one `FieldNode` and the buffers [`crate::ipc::layout`] prescribes
//! for each array it meets, and hands the pieces to the columnar core's
//! *checked* constructors — the ones that validate offsets, UTF-8 and bitmap
//! lengths. A malformed stream therefore produces a typed error, never a
//! silently wrong array.
//!
//! # Copy versus zero-copy
//!
//! Every buffer an array receives is a [`Buffer`] **window into the message
//! body**: one atomic increment, no bytes moved. Two situations force a copy,
//! both reported by nothing louder than a slower decode:
//!
//! 1. **Misaligned typed buffers.** Reading `[i64]` out of a body whose base
//!    address is not 8-byte aligned is undefined behaviour, so the window is
//!    copied into a fresh 64-byte-aligned allocation. Bodies AstRS writes are
//!    64-byte aligned inside a 64-byte-aligned buffer, so this never fires on
//!    an AstRS stream, and it fires on a foreign stream only when the reader
//!    was handed unaligned bytes to begin with (see
//!    [`crate::ipc::IpcStreamReader::new`]).
//! 2. **UTF-8 validation.** `Utf8` and `LargeUtf8` columns are validated
//!    before construction, which reads the bytes once but does not copy them.
//!
//! # Null counts
//!
//! The validity buffer is decoded only when the field node declares at least
//! one null. That is what makes a zero-length validity buffer — the encoding
//! arrow-cpp, pyarrow and [`crate::ipc::encode`] all produce for a column
//! without nulls — decode correctly.

use std::sync::Arc;

use crate::array::{
    ArrayRef, BooleanArray, DurationArray, FixedSizeBinaryArray, FixedSizeListArray,
    GenericBinaryArray, GenericStringArray, IntoArrayRef, ListArray, NullArray, OffsetSizeTrait,
    PrimitiveArray, StructArray, TimestampArray,
};
use crate::buffer::{Bitmap, Buffer, ScalarBuffer};
use crate::datatype::{ArrowNativeType, DataType, F16, Field, Schema};
use crate::ipc::error::{IpcError, Result};
use crate::ipc::layout::{BufferRole, MAX_NESTING_DEPTH, buffer_roles, validity_byte_len};

/// Consumes the `FieldNode`/`Buffer` vectors of one record batch message in
/// the depth-first order the format prescribes.
#[derive(Debug)]
pub struct BatchDecoder<'a> {
    /// The message body, as a shareable window.
    body: &'a Buffer,
    /// `(length, null_count)` per array.
    nodes: &'a [(i64, i64)],
    /// `(offset, length)` per buffer.
    buffers: &'a [(i64, i64)],
    /// How many nodes have been consumed.
    node_index: usize,
    /// How many buffers have been consumed.
    buffer_index: usize,
}

/// One decoded `FieldNode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeInfo {
    /// Slot count.
    rows: usize,
    /// Declared nulls, or `None` for the `-1` "unknown" convention.
    null_count: Option<usize>,
}

impl<'a> BatchDecoder<'a> {
    /// Starts a decoder over one message's vectors.
    #[must_use]
    pub const fn new(body: &'a Buffer, nodes: &'a [(i64, i64)], buffers: &'a [(i64, i64)]) -> Self {
        Self {
            body,
            nodes,
            buffers,
            node_index: 0,
            buffer_index: 0,
        }
    }

    /// Nodes consumed so far.
    #[inline]
    #[must_use]
    pub const fn nodes_consumed(&self) -> usize {
        self.node_index
    }

    /// Buffers consumed so far.
    #[inline]
    #[must_use]
    pub const fn buffers_consumed(&self) -> usize {
        self.buffer_index
    }

    /// Fails unless every node and buffer was consumed.
    ///
    /// # Errors
    ///
    /// [`IpcError::LayoutCountMismatch`] when the message declared more than
    /// the schema needs.
    pub fn finish(&self) -> Result<()> {
        if self.node_index != self.nodes.len() {
            return Err(IpcError::LayoutCountMismatch {
                what: "field node(s)",
                expected: self.node_index,
                actual: self.nodes.len(),
            });
        }
        if self.buffer_index != self.buffers.len() {
            return Err(IpcError::LayoutCountMismatch {
                what: "buffer(s)",
                expected: self.buffer_index,
                actual: self.buffers.len(),
            });
        }
        Ok(())
    }

    /// Decodes the column declared by `field`.
    ///
    /// # Errors
    ///
    /// Every structural variant of [`IpcError`], plus [`IpcError::Data`] when
    /// the columnar core rejects the reconstructed buffers.
    pub fn decode_column(&mut self, field: &Field, depth: usize) -> Result<ArrayRef> {
        if depth > MAX_NESTING_DEPTH {
            return Err(IpcError::NestingTooDeep {
                depth,
                limit: MAX_NESTING_DEPTH,
            });
        }
        let node = self.next_node()?;
        let data_type = field.data_type();
        let validity = self.next_validity(data_type, node)?;
        let rows = node.rows;

        let array: ArrayRef = match data_type {
            DataType::Null => NullArray::new(rows).into_array_ref(),
            DataType::Bool => {
                let values = self.next_bits(rows)?;
                BooleanArray::new(values, validity)?.into_array_ref()
            }
            DataType::Int8 => self.primitive::<i8>(rows, validity)?,
            DataType::Int16 => self.primitive::<i16>(rows, validity)?,
            DataType::Int32 => self.primitive::<i32>(rows, validity)?,
            DataType::Int64 => self.primitive::<i64>(rows, validity)?,
            DataType::UInt8 => self.primitive::<u8>(rows, validity)?,
            DataType::UInt16 => self.primitive::<u16>(rows, validity)?,
            DataType::UInt32 => self.primitive::<u32>(rows, validity)?,
            DataType::UInt64 => self.primitive::<u64>(rows, validity)?,
            DataType::Float16 => self.primitive::<F16>(rows, validity)?,
            DataType::Float32 => self.primitive::<f32>(rows, validity)?,
            DataType::Float64 => self.primitive::<f64>(rows, validity)?,
            DataType::Timestamp => {
                let values = self.next_scalars::<i64>(rows)?;
                TimestampArray::try_new(values, validity)?.into_array_ref()
            }
            DataType::Duration => {
                let values = self.next_scalars::<i64>(rows)?;
                DurationArray::try_new(values, validity)?.into_array_ref()
            }
            DataType::FixedSizeBinary(size) => {
                let width = usize::try_from(*size).unwrap_or(0);
                let values = self.next_values(rows.saturating_mul(width), rows)?;
                FixedSizeBinaryArray::try_new(*size, values, validity)?.into_array_ref()
            }
            DataType::Binary => self.binary::<i32>(rows, validity)?,
            DataType::LargeBinary => self.binary::<i64>(rows, validity)?,
            DataType::Utf8 => self.string::<i32>(rows, validity)?,
            DataType::LargeUtf8 => self.string::<i64>(rows, validity)?,
            DataType::List(item) => {
                let offsets = self.next_offsets::<i32>(rows)?;
                let child = self.decode_column(item, depth + 1)?;
                ListArray::try_new(item.as_ref().clone(), offsets, child, validity)?
                    .into_array_ref()
            }
            DataType::FixedSizeList(item, size) => {
                let child = self.decode_column(item, depth + 1)?;
                let expected = rows.saturating_mul(usize::try_from(*size).unwrap_or(0));
                if child.len() != expected {
                    return Err(IpcError::RowCountMismatch {
                        declared: expected,
                        column: self.node_index.saturating_sub(1),
                        actual: child.len(),
                    });
                }
                FixedSizeListArray::try_new(item.as_ref().clone(), *size, child, validity)?
                    .into_array_ref()
            }
            DataType::Struct(fields) => {
                let mut columns = Vec::with_capacity(fields.len());
                for child in fields {
                    let column = self.decode_column(child, depth + 1)?;
                    if column.len() != rows {
                        return Err(IpcError::RowCountMismatch {
                            declared: rows,
                            column: columns.len(),
                            actual: column.len(),
                        });
                    }
                    columns.push(column);
                }
                StructArray::try_new_with_len(fields.clone(), columns, rows, validity)?
                    .into_array_ref()
            }
        };
        Ok(array)
    }

    /// Reads the next field node.
    fn next_node(&mut self) -> Result<NodeInfo> {
        let index = self.node_index;
        let Some(&(length, null_count)) = self.nodes.get(index) else {
            return Err(IpcError::LayoutCountMismatch {
                what: "field node(s)",
                expected: index + 1,
                actual: self.nodes.len(),
            });
        };
        self.node_index += 1;
        let invalid = || IpcError::InvalidFieldNode {
            index,
            length,
            null_count,
        };
        let rows = usize::try_from(length).map_err(|_| invalid())?;
        // `-1` is the "null count not computed" convention of the Arrow
        // specification; the bitmap then decides. Any other negative value,
        // or more nulls than slots, is a corrupt node.
        let nulls = if null_count == -1 {
            None
        } else {
            let nulls = usize::try_from(null_count).map_err(|_| invalid())?;
            if nulls > rows {
                return Err(invalid());
            }
            Some(nulls)
        };
        Ok(NodeInfo {
            rows,
            null_count: nulls,
        })
    }

    /// Reads the next buffer entry as a window into the body.
    fn next_buffer(&mut self) -> Result<Buffer> {
        let index = self.buffer_index;
        let Some(&(offset, length)) = self.buffers.get(index) else {
            return Err(IpcError::LayoutCountMismatch {
                what: "buffer(s)",
                expected: index + 1,
                actual: self.buffers.len(),
            });
        };
        self.buffer_index += 1;
        let body_len = self.body.len();
        let out_of_bounds = || IpcError::BufferOutOfBounds {
            index,
            offset,
            length,
            body_len,
        };
        if offset < 0 || length < 0 {
            return Err(out_of_bounds());
        }
        let start = usize::try_from(offset).map_err(|_| out_of_bounds())?;
        let len = usize::try_from(length).map_err(|_| out_of_bounds())?;
        let end = start.checked_add(len).ok_or_else(out_of_bounds)?;
        if end > body_len {
            return Err(out_of_bounds());
        }
        Ok(self.body.slice(start, len))
    }

    /// Reads the validity bitmap, or `None` when the node declares no nulls.
    ///
    /// `Null` columns have no validity buffer at all, so nothing is consumed
    /// for them.
    fn next_validity(&mut self, data_type: &DataType, node: NodeInfo) -> Result<Option<Bitmap>> {
        if !buffer_roles(data_type)
            .first()
            .is_some_and(|role| *role == BufferRole::Validity)
        {
            return Ok(None);
        }
        let index = self.buffer_index;
        let buffer = self.next_buffer()?;
        let wanted = match node.null_count {
            // A column without nulls may still carry an all-ones bitmap
            // (arrow-rs writes one); there is nothing to represent either way.
            Some(0) => false,
            Some(_) => true,
            // "Not computed": the bitmap decides, if there is one.
            None => !buffer.is_empty(),
        };
        if !wanted {
            return Ok(None);
        }
        let required = validity_byte_len(node.rows);
        if buffer.len() < required {
            return Err(IpcError::BufferTooShort {
                index,
                role: BufferRole::Validity.name(),
                actual: buffer.len(),
                required,
                rows: node.rows,
            });
        }
        Ok(Some(Bitmap::try_new(
            buffer.slice(0, required),
            0,
            node.rows,
        )?))
    }

    /// Reads a bit-packed values buffer of `rows` bits.
    fn next_bits(&mut self, rows: usize) -> Result<Bitmap> {
        let index = self.buffer_index;
        let buffer = self.next_buffer()?;
        let required = validity_byte_len(rows);
        if buffer.len() < required {
            return Err(IpcError::BufferTooShort {
                index,
                role: BufferRole::Values.name(),
                actual: buffer.len(),
                required,
                rows,
            });
        }
        Ok(Bitmap::try_new(buffer.slice(0, required), 0, rows)?)
    }

    /// Reads a values buffer of exactly `bytes` bytes covering `rows` slots.
    fn next_values(&mut self, bytes: usize, rows: usize) -> Result<Buffer> {
        let index = self.buffer_index;
        let buffer = self.next_buffer()?;
        if buffer.len() < bytes {
            return Err(IpcError::BufferTooShort {
                index,
                role: BufferRole::Values.name(),
                actual: buffer.len(),
                required: bytes,
                rows,
            });
        }
        Ok(buffer.slice(0, bytes))
    }

    /// Reads a values buffer holding `count` fixed-width scalars.
    fn next_scalars<T: ArrowNativeType>(&mut self, count: usize) -> Result<ScalarBuffer<T>> {
        let index = self.buffer_index;
        let width = std::mem::size_of::<T>();
        let required = count.saturating_mul(width);
        let buffer = self.next_buffer()?;
        if buffer.len() < required {
            return Err(IpcError::BufferTooShort {
                index,
                role: BufferRole::Values.name(),
                actual: buffer.len(),
                required,
                rows: count,
            });
        }
        Ok(typed_window(&buffer.slice(0, required)))
    }

    /// Reads an offset buffer of `rows + 1` entries.
    ///
    /// A zero-length buffer is accepted for an empty array — that is how
    /// arrow-rs writes an empty variable-length column — and turned into the
    /// canonical single `0` entry the columnar core expects.
    fn next_offsets<O: OffsetSizeTrait>(&mut self, rows: usize) -> Result<ScalarBuffer<O>> {
        let index = self.buffer_index;
        let width = std::mem::size_of::<O>();
        let required = rows.saturating_add(1).saturating_mul(width);
        let buffer = self.next_buffer()?;
        if buffer.is_empty() && rows == 0 {
            return Ok(ScalarBuffer::from_slice(&[O::ZERO]));
        }
        if buffer.len() < required {
            return Err(IpcError::BufferTooShort {
                index,
                role: BufferRole::Offsets.name(),
                actual: buffer.len(),
                required,
                rows,
            });
        }
        Ok(typed_window(&buffer.slice(0, required)))
    }

    /// Decodes a fixed-width primitive column.
    fn primitive<T: ArrowNativeType>(
        &mut self,
        rows: usize,
        validity: Option<Bitmap>,
    ) -> Result<ArrayRef> {
        let values = self.next_scalars::<T>(rows)?;
        Ok(PrimitiveArray::<T>::try_new(values, validity)?.into_array_ref())
    }

    /// Decodes a `Binary`/`LargeBinary` column.
    fn binary<O: OffsetSizeTrait>(
        &mut self,
        rows: usize,
        validity: Option<Bitmap>,
    ) -> Result<ArrayRef> {
        let offsets = self.next_offsets::<O>(rows)?;
        let values = self.next_value_region(&offsets, rows)?;
        Ok(GenericBinaryArray::<O>::try_new(offsets, values, validity)?.into_array_ref())
    }

    /// Decodes a `Utf8`/`LargeUtf8` column, validating the encoding.
    fn string<O: OffsetSizeTrait>(
        &mut self,
        rows: usize,
        validity: Option<Bitmap>,
    ) -> Result<ArrayRef> {
        let offsets = self.next_offsets::<O>(rows)?;
        let values = self.next_value_region(&offsets, rows)?;
        Ok(GenericStringArray::<O>::try_new(offsets, values, validity)?.into_array_ref())
    }

    /// Reads the value region a variable-length column's offsets address.
    fn next_value_region<O: OffsetSizeTrait>(
        &mut self,
        offsets: &ScalarBuffer<O>,
        rows: usize,
    ) -> Result<Buffer> {
        let index = self.buffer_index;
        let needed = offsets
            .as_slice()
            .get(rows)
            .copied()
            .and_then(OffsetSizeTrait::to_usize)
            .unwrap_or(0);
        let buffer = self.next_buffer()?;
        if buffer.len() < needed {
            return Err(IpcError::BufferTooShort {
                index,
                role: BufferRole::Values.name(),
                actual: buffer.len(),
                required: needed,
                rows,
            });
        }
        Ok(buffer)
    }
}

/// Reinterprets a byte window as `[T]`, copying only if it is misaligned.
///
/// The caller has already checked that the window is a whole number of
/// elements, which is the invariant
/// [`ScalarBuffer::from_buffer_lossy`] cannot check for itself.
fn typed_window<T: ArrowNativeType>(buffer: &Buffer) -> ScalarBuffer<T> {
    match buffer.typed::<T>() {
        Ok(typed) => typed,
        Err(_) => ScalarBuffer::from_buffer_lossy(buffer),
    }
}

/// Rebuilds every column of a batch body.
///
/// # Errors
///
/// As [`BatchDecoder::decode_column`], plus [`IpcError::RowCountMismatch`]
/// when a column disagrees with `rows`.
pub fn decode_columns(
    fields: &[Field],
    rows: usize,
    body: &Buffer,
    nodes: &[(i64, i64)],
    buffers: &[(i64, i64)],
) -> Result<Vec<ArrayRef>> {
    let expected_nodes = crate::ipc::layout::schema_node_count(fields);
    if nodes.len() != expected_nodes {
        return Err(IpcError::LayoutCountMismatch {
            what: "field node(s)",
            expected: expected_nodes,
            actual: nodes.len(),
        });
    }
    let expected_buffers = crate::ipc::layout::schema_buffer_count(fields);
    if buffers.len() != expected_buffers {
        return Err(IpcError::LayoutCountMismatch {
            what: "buffer(s)",
            expected: expected_buffers,
            actual: buffers.len(),
        });
    }

    let mut decoder = BatchDecoder::new(body, nodes, buffers);
    let mut columns = Vec::with_capacity(fields.len());
    for (index, field) in fields.iter().enumerate() {
        let column = decoder.decode_column(field, 1)?;
        if column.len() != rows {
            return Err(IpcError::RowCountMismatch {
                declared: rows,
                column: index,
                actual: column.len(),
            });
        }
        columns.push(column);
    }
    decoder.finish()?;
    Ok(columns)
}

/// Wraps decoded columns in a batch without re-validating what the decoder
/// already proved.
///
/// Column types are still checked exactly — they are built from the schema, so
/// a mismatch is a decoder bug worth surfacing — but the nullability check is
/// relaxed: a foreign producer may legitimately mark a field non-nullable and
/// still hand over a validity bitmap, and rejecting the batch for it would be
/// stricter than the format.
///
/// # Errors
///
/// [`IpcError::Data`] when a column does not match its field.
pub fn assemble_batch(
    schema: &Arc<Schema>,
    columns: Vec<ArrayRef>,
    rows: usize,
) -> Result<crate::record_batch::RecordBatch> {
    let options = crate::record_batch::RecordBatchOptions::default()
        .with_row_count(rows)
        .with_nullability_check(false);
    Ok(crate::record_batch::RecordBatch::try_new_with_options(
        Arc::clone(schema),
        columns,
        options,
    )?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ipc::encode::BatchLayout;
    use crate::record_batch::RecordBatch;

    /// Round-trips a batch through the plan/decode pair without any framing.
    fn round_trip(batch: &RecordBatch) -> RecordBatch {
        let layout = BatchLayout::plan(batch, 64).expect("plan");
        let mut body = Vec::new();
        layout.write_body(&mut body).expect("body");
        let buffer = Buffer::from(crate::buffer::AlignedBuf::from_slice(&body));
        let columns = decode_columns(
            batch.schema().fields(),
            batch.num_rows(),
            &buffer,
            layout.nodes(),
            layout.buffers(),
        )
        .expect("decode");
        assemble_batch(batch.schema(), columns, batch.num_rows()).expect("assemble")
    }

    fn body_of(batch: &RecordBatch) -> (BatchLayout, Buffer) {
        let layout = BatchLayout::plan(batch, 64).expect("plan");
        let mut body = Vec::new();
        layout.write_body(&mut body).expect("body");
        (
            layout,
            Buffer::from(crate::buffer::AlignedBuf::from_slice(&body)),
        )
    }

    #[test]
    fn primitives_round_trip_through_the_body() {
        let batch = RecordBatch::from_payload(
            crate::array::Int32Array::from_opt_iter([Some(1), None, Some(-3)]).into_array_ref(),
        );
        assert_eq!(round_trip(&batch), batch);
    }

    #[test]
    fn strings_round_trip_and_validate() {
        let batch = RecordBatch::from_payload(
            crate::array::StringArray::from_opt_iter([Some("héllo"), None, Some("")])
                .into_array_ref(),
        );
        assert_eq!(round_trip(&batch), batch);
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        // A binary column whose bytes are not UTF-8, decoded as a string
        // column: the checked constructor must refuse it.
        let bytes = crate::array::BinaryArray::from_values([&[0xff, 0xfe][..]]);
        let batch = RecordBatch::from_payload(bytes.into_array_ref());
        let (layout, buffer) = body_of(&batch);
        let fields = vec![Field::new("data", DataType::Utf8, true)];
        let err =
            decode_columns(&fields, 1, &buffer, layout.nodes(), layout.buffers()).unwrap_err();
        assert!(matches!(err, IpcError::Data(_)), "{err}");
    }

    #[test]
    fn a_short_values_buffer_is_reported() {
        let batch = RecordBatch::from_payload(
            crate::array::Int64Array::from_values([1, 2, 3]).into_array_ref(),
        );
        let (layout, buffer) = body_of(&batch);
        let mut buffers = layout.buffers().to_vec();
        buffers[1].1 = 8; // one value where three were promised
        let err = decode_columns(
            batch.schema().fields(),
            3,
            &buffer,
            layout.nodes(),
            &buffers,
        )
        .unwrap_err();
        match err {
            IpcError::BufferTooShort { role, required, .. } => {
                assert_eq!(role, "values");
                assert_eq!(required, 24);
            }
            other => panic!("unexpected {other}"),
        }
    }

    #[test]
    fn buffers_outside_the_body_are_rejected() {
        let batch = RecordBatch::from_payload(
            crate::array::Int64Array::from_values([1, 2, 3]).into_array_ref(),
        );
        let (layout, buffer) = body_of(&batch);
        for bad in [(1_000_000i64, 8i64), (-8, 8), (0, -1)] {
            let mut buffers = layout.buffers().to_vec();
            buffers[1] = bad;
            let err = decode_columns(
                batch.schema().fields(),
                3,
                &buffer,
                layout.nodes(),
                &buffers,
            )
            .unwrap_err();
            assert!(
                matches!(err, IpcError::BufferOutOfBounds { .. }),
                "{bad:?}: {err}"
            );
        }
    }

    #[test]
    fn wrong_vector_lengths_are_rejected() {
        let batch =
            RecordBatch::from_payload(crate::array::Int64Array::from_values([1]).into_array_ref());
        let (layout, buffer) = body_of(&batch);
        let err =
            decode_columns(batch.schema().fields(), 1, &buffer, &[], layout.buffers()).unwrap_err();
        assert!(matches!(err, IpcError::LayoutCountMismatch { .. }), "{err}");
        let err =
            decode_columns(batch.schema().fields(), 1, &buffer, layout.nodes(), &[]).unwrap_err();
        assert!(matches!(err, IpcError::LayoutCountMismatch { .. }), "{err}");
    }

    #[test]
    fn absurd_field_nodes_are_rejected() {
        let batch =
            RecordBatch::from_payload(crate::array::Int64Array::from_values([1]).into_array_ref());
        let (layout, buffer) = body_of(&batch);
        for node in [(-1i64, 0i64), (1, 5), (1, -7)] {
            let err = decode_columns(
                batch.schema().fields(),
                1,
                &buffer,
                &[node],
                layout.buffers(),
            )
            .unwrap_err();
            assert!(
                matches!(err, IpcError::InvalidFieldNode { .. }),
                "{node:?}: {err}"
            );
        }
    }

    #[test]
    fn nested_columns_round_trip() {
        let child = crate::array::Float64Array::from_values([1.0, 2.0, 3.0, 4.0]).into_array_ref();
        let lists = ListArray::try_from_lengths(
            Field::new("item", DataType::Float64, false),
            [2usize, 0, 2],
            child,
        )
        .expect("lists");
        let batch = RecordBatch::from_payload(lists.into_array_ref());
        assert_eq!(round_trip(&batch), batch);
    }

    #[test]
    fn a_null_column_needs_no_buffers() {
        let batch = RecordBatch::from_payload(NullArray::new(3).into_array_ref());
        let (layout, buffer) = body_of(&batch);
        assert!(layout.buffers().is_empty());
        let columns =
            decode_columns(batch.schema().fields(), 3, &buffer, &[(3, 3)], &[]).expect("decode");
        assert_eq!(columns[0].len(), 3);
        assert_eq!(columns[0].data_type(), &DataType::Null);
    }

    #[test]
    fn empty_offset_buffers_decode_as_empty_columns() {
        let fields = vec![Field::new("data", DataType::Utf8, true)];
        let buffer = Buffer::new();
        let columns = decode_columns(&fields, 0, &buffer, &[(0, 0)], &[(0, 0), (0, 0), (0, 0)])
            .expect("decode");
        assert_eq!(columns[0].len(), 0);
    }

    #[test]
    fn deep_nesting_is_refused() {
        let mut data_type = DataType::Int32;
        for _ in 0..MAX_NESTING_DEPTH {
            data_type = DataType::list(Field::new("item", data_type, true));
        }
        let fields = vec![Field::new("deep", data_type, true)];
        let buffer = Buffer::new();
        let nodes = vec![(0i64, 0i64); MAX_NESTING_DEPTH + 1];
        let buffers = vec![(0i64, 0i64); 2 * MAX_NESTING_DEPTH + 2];
        let err = decode_columns(&fields, 0, &buffer, &nodes, &buffers).unwrap_err();
        assert!(matches!(err, IpcError::NestingTooDeep { .. }), "{err}");
    }
}
