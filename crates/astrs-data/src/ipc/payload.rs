//! The AstRS payload convenience layer: one batch in, one aligned buffer out.
//!
//! Blueprint §6.1 fixes what travels on an AstRS leg: **one record batch per
//! message**, carrying a single top-level column named
//! [`DATA_COLUMN`](crate::DATA_COLUMN) (the dora convention, kept so dora
//! tooling keeps working), capped at [`crate::MAX_PAYLOAD_BYTES`]. This module
//! is that shape as two functions.
//!
//! # Alignment contract
//!
//! [`encode_payload`] returns an [`AlignedBuf`], whose start address is
//! [`crate::ALIGNMENT`]-aligned (64 bytes), and pads the encoded length up to
//! a multiple of [`PAYLOAD_ALIGNMENT`] (128 bytes). Together with the writer's
//! 64-byte message alignment that gives the property the shared-memory plane
//! needs:
//!
//! * place the payload at a 128-byte-aligned slot address — which is what
//!   `astrs-shm` slots guarantee — and **every body buffer inside it is
//!   64-byte aligned in memory**, so a mapped tensor column is directly usable
//!   as a SIMD source;
//! * the padded length keeps the *next* payload in a packed slot on the same
//!   128-byte boundary.
//!
//! The padding sits after the end-of-stream marker, so the buffer stays a
//! valid Arrow stream for arrow-rs, pyarrow and arrow-cpp, all of which stop
//! at the marker.
//!
//! ```
//! use astrs_data::array::{Float32Array, IntoArrayRef};
//! use astrs_data::ipc::{decode_payload, encode_payload, PAYLOAD_ALIGNMENT};
//! use astrs_data::{RecordBatch, ALIGNMENT};
//!
//! let batch = RecordBatch::from_payload(Float32Array::from_values([1.0, 2.0, 3.0]).into_array_ref());
//! let payload = encode_payload(&batch)?;
//!
//! assert_eq!(payload.len() % PAYLOAD_ALIGNMENT, 0, "length is slot-aligned");
//! assert_eq!(payload.as_ptr() as usize % ALIGNMENT, 0, "base is buffer-aligned");
//! assert_eq!(decode_payload(payload.as_slice())?, batch);
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

use crate::buffer::{AlignedBuf, Buffer};
use crate::ipc::error::{IpcError, Result};
use crate::ipc::message::MessageLimits;
use crate::ipc::reader::{IpcStreamReader, ReadOptions};
use crate::ipc::writer::{WriteOptions, to_ipc_bytes_with};
use crate::record_batch::RecordBatch;

/// The boundary a whole payload is padded to, so a shared-memory slot can hold
/// payloads back to back and keep every one of them aligned (blueprint §6.1).
pub const PAYLOAD_ALIGNMENT: usize = 128;

/// Encodes one record batch as a complete, self-describing Arrow IPC stream.
///
/// The result is schema message, one record batch message, end-of-stream
/// marker, then zero padding up to a multiple of [`PAYLOAD_ALIGNMENT`].
///
/// The batch is written exactly as given: this function does not impose the
/// single-`data`-column convention, it *carries* it — build the batch with
/// [`RecordBatch::from_payload`] and it holds.
///
/// # Errors
///
/// * [`IpcError::TooLarge`] when the encoded payload would exceed
///   [`crate::MAX_PAYLOAD_BYTES`].
/// * [`IpcError::Data`] when a column's concrete array type disagrees with its
///   declared type.
pub fn encode_payload(batch: &RecordBatch) -> Result<AlignedBuf> {
    encode_payload_with(batch, WriteOptions::new())
}

/// [`encode_payload`] with explicit framing options.
///
/// # Errors
///
/// As [`encode_payload`].
pub fn encode_payload_with(batch: &RecordBatch, options: WriteOptions) -> Result<AlignedBuf> {
    let estimate = batch.buffer_memory_size();
    if estimate > crate::MAX_PAYLOAD_BYTES {
        return Err(IpcError::TooLarge {
            what: "payload",
            length: estimate as u64,
            cap: crate::MAX_PAYLOAD_BYTES as u64,
        });
    }
    let mut bytes = to_ipc_bytes_with(batch.schema_ref(), std::slice::from_ref(batch), options)?;
    if bytes.len() > crate::MAX_PAYLOAD_BYTES {
        return Err(IpcError::TooLarge {
            what: "payload",
            length: bytes.len() as u64,
            cap: crate::MAX_PAYLOAD_BYTES as u64,
        });
    }
    let padded = bytes.len().next_multiple_of(PAYLOAD_ALIGNMENT);
    bytes.resize(padded, 0);
    Ok(bytes)
}

/// Decodes a payload written by [`encode_payload`] — or by any Arrow producer
/// that sends exactly one record batch.
///
/// The bytes are copied once into an aligned buffer; use
/// [`decode_payload_buffer`] to decode a mapped payload without copying it.
///
/// # Errors
///
/// * [`IpcError::TooLarge`] when the input exceeds [`crate::MAX_PAYLOAD_BYTES`].
/// * [`IpcError::NotASinglePayload`] when the stream holds no batch or more
///   than one.
/// * Any decode error from the stream itself.
pub fn decode_payload(bytes: &[u8]) -> Result<RecordBatch> {
    check_payload_size(bytes.len())?;
    decode_payload_buffer(Buffer::from(AlignedBuf::from_slice(bytes)))
}

/// [`decode_payload`] over an owned buffer, decoding in place.
///
/// The returned batch's columns are windows into `buffer`; nothing is copied
/// unless a typed buffer turns out to be misaligned (see
/// [`crate::ipc::reader`]).
///
/// # Errors
///
/// As [`decode_payload`].
pub fn decode_payload_buffer(buffer: Buffer) -> Result<RecordBatch> {
    check_payload_size(buffer.len())?;
    let options = ReadOptions::new()
        .with_limits(MessageLimits::default().with_max_body_bytes(crate::MAX_PAYLOAD_BYTES))
        .with_max_stream_bytes(crate::MAX_PAYLOAD_BYTES);
    let mut reader = IpcStreamReader::with_options(buffer, options)?;
    let Some(batch) = reader.next_batch()? else {
        return Err(IpcError::NotASinglePayload { count: 0 });
    };
    // Exactly one batch: a second one means the producer is not speaking the
    // payload convention, and silently dropping it would lose data.
    let mut count = 1usize;
    while reader.next_batch()?.is_some() {
        count += 1;
    }
    if count != 1 {
        return Err(IpcError::NotASinglePayload { count });
    }
    Ok(batch)
}

/// Rejects a payload past the blueprint's hard cap.
fn check_payload_size(len: usize) -> Result<()> {
    if len > crate::MAX_PAYLOAD_BYTES {
        return Err(IpcError::TooLarge {
            what: "payload",
            length: len as u64,
            cap: crate::MAX_PAYLOAD_BYTES as u64,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{
        Float32Array, Int64Array, IntoArrayRef, StringArray, StructArray, UInt8Array,
    };
    use crate::datatype::{DataType, Field, Schema};
    use crate::ipc::message::{MessageLimits, scan_message};
    use std::sync::Arc;

    #[test]
    fn payloads_round_trip() {
        let batch = RecordBatch::from_payload(
            Int64Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref(),
        );
        let payload = encode_payload(&batch).expect("encode");
        assert_eq!(decode_payload(payload.as_slice()).expect("decode"), batch);
    }

    #[test]
    fn payload_lengths_and_bases_are_aligned() {
        for rows in [0usize, 1, 7, 64, 1000] {
            let column = UInt8Array::from_values((0..rows).map(|i| i as u8).collect::<Vec<_>>())
                .into_array_ref();
            let payload = encode_payload(&RecordBatch::from_payload(column)).expect("encode");
            assert_eq!(payload.len() % PAYLOAD_ALIGNMENT, 0, "rows {rows}");
            assert_eq!(payload.as_ptr() as usize % crate::ALIGNMENT, 0);
        }
    }

    #[test]
    fn every_message_and_body_buffer_is_64_byte_aligned() {
        let batch = RecordBatch::from_payload(
            Float32Array::from_opt_iter([Some(1.0), None, Some(3.0)]).into_array_ref(),
        );
        let payload = encode_payload(&batch).expect("encode");
        let limits = MessageLimits::default();
        let mut pos = 0usize;
        while let Some(message) = scan_message(payload.as_slice(), pos, &limits).expect("scan") {
            assert_eq!(pos % 64, 0, "message start");
            assert_eq!(message.body.start % 64, 0, "body start");
            pos = message.next;
        }
    }

    #[test]
    fn decoding_a_buffer_is_zero_copy() {
        let batch = RecordBatch::from_payload(
            UInt8Array::from_values((0..200u8).collect::<Vec<_>>()).into_array_ref(),
        );
        let payload = encode_payload(&batch).expect("encode");
        let buffer = Buffer::from(payload);
        let before = buffer.share_count();
        let decoded = decode_payload_buffer(buffer.clone()).expect("decode");
        assert_eq!(decoded, batch);
        assert!(
            buffer.share_count() > before,
            "the decoded column must share the payload allocation"
        );
    }

    #[test]
    fn a_stream_with_two_batches_is_not_a_payload() {
        let batch = RecordBatch::from_payload(Int64Array::from_values([1]).into_array_ref());
        let bytes = crate::ipc::writer::to_ipc_bytes(&[batch.clone(), batch]).expect("write");
        let err = decode_payload(bytes.as_slice()).unwrap_err();
        assert!(
            matches!(err, IpcError::NotASinglePayload { count: 2 }),
            "{err}"
        );
    }

    #[test]
    fn a_schema_only_stream_is_not_a_payload() {
        let schema = Arc::new(Schema::payload(DataType::Int64, true));
        let bytes =
            crate::ipc::writer::to_ipc_bytes_with(schema, &[], WriteOptions::new()).expect("write");
        let err = decode_payload(bytes.as_slice()).unwrap_err();
        assert!(
            matches!(err, IpcError::NotASinglePayload { count: 0 }),
            "{err}"
        );
    }

    #[test]
    fn oversized_inputs_are_refused() {
        let err = check_payload_size(crate::MAX_PAYLOAD_BYTES + 1).unwrap_err();
        assert!(
            matches!(
                err,
                IpcError::TooLarge {
                    what: "payload",
                    ..
                }
            ),
            "{err}"
        );
        assert!(check_payload_size(crate::MAX_PAYLOAD_BYTES).is_ok());
    }

    #[test]
    fn the_data_column_convention_survives_the_round_trip() {
        let batch =
            RecordBatch::from_payload(StringArray::from_values(["a", "bb"]).into_array_ref());
        let payload = encode_payload(&batch).expect("encode");
        let decoded = decode_payload(payload.as_slice()).expect("decode");
        assert_eq!(
            decoded.schema().field(0).map(Field::name),
            Some(crate::DATA_COLUMN)
        );
        assert!(decoded.payload_column().is_some());
    }

    #[test]
    fn nested_payloads_round_trip() {
        let fields = vec![
            Field::new("x", DataType::Float32, false),
            Field::new("y", DataType::Float32, false),
        ];
        let columns = vec![
            Float32Array::from_values([1.0, 2.0]).into_array_ref(),
            Float32Array::from_values([3.0, 4.0]).into_array_ref(),
        ];
        let strukt = StructArray::try_new(fields, columns, None).expect("struct");
        let batch = RecordBatch::from_payload(strukt.into_array_ref());
        let payload = encode_payload(&batch).expect("encode");
        let decoded = decode_payload(payload.as_slice()).expect("decode");
        assert_eq!(decoded, batch);
        assert_eq!(decoded.column(0).map(|c| c.len()), Some(2));
    }

    #[test]
    fn trailing_padding_does_not_disturb_the_reader() {
        let batch = RecordBatch::from_payload(Int64Array::from_values([9]).into_array_ref());
        let payload = encode_payload(&batch).expect("encode");
        let unpadded =
            crate::ipc::writer::to_ipc_bytes(std::slice::from_ref(&batch)).expect("write");
        assert!(payload.len() >= unpadded.len());
        assert!(
            payload.as_slice()[unpadded.len()..].iter().all(|b| *b == 0),
            "padding is zero"
        );
        assert_eq!(decode_payload(payload.as_slice()).expect("decode"), batch);
    }
}
