//! [`OpOutput`] — the buffered send side of an [`crate::Operator`].
//!
//! An operator does not hold a live connection to the daemon; it hands
//! finished messages to `out: &mut OpOutput` inside
//! [`crate::Operator::on_event`] (and the lifecycle hooks), and the runtime
//! host drains the buffer after the call returns (blueprint §9.3's "each
//! operator runs on its own thread over a bounded channel" — the buffer is
//! what crosses that channel). Every buffered send already carries Arrow
//! IPC-encoded bytes, matching the wire's own `NodeEvent::Input { payload:
//! Vec<u8>, .. }` convention (blueprint §6.1), so the host never has to
//! re-encode what an operator already finished.

use astrs_data::ipc::encode_payload;
use astrs_data::{AstrsMessage, RecordBatch};
use astrs_wire::{DataId, Metadata};

use crate::error::{OpError, OpResult};

/// One buffered send, ready for the runtime host to deliver.
#[derive(Debug, Clone, PartialEq)]
pub struct OpSend {
    id: DataId,
    metadata: Metadata,
    payload: Vec<u8>,
}

impl OpSend {
    /// The output this send targets.
    #[must_use]
    pub const fn id(&self) -> &DataId {
        &self.id
    }

    /// The metadata riding beside the payload.
    #[must_use]
    pub const fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// The encoded payload bytes (an Arrow IPC stream).
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the send, returning its three parts.
    #[must_use]
    pub fn into_parts(self) -> (DataId, Metadata, Vec<u8>) {
        (self.id, self.metadata, self.payload)
    }
}

/// The buffered output side an [`crate::Operator`] writes through.
///
/// Sends accumulate in the order they were made and are read out with
/// [`OpOutput::drain`]; nothing is delivered until the host does that, so an
/// operator can call `send`/`send_batch`/`send_bytes` any number of times
/// within one [`crate::Operator::on_event`] call without touching a
/// connection.
///
/// ```
/// use astrs_data::prelude::*;
/// use astrs_operator_api::OpOutput;
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::Metadata;
///
/// #[derive(Debug, PartialEq)]
/// struct Ping;
///
/// impl astrs_data::AstrsMessage for Ping {
///     const URN: &'static str = "std/core/v1/PingMsg";
///     fn data_type() -> DataType {
///         DataType::strukt([Field::required("tick", DataType::UInt8)])
///     }
///     fn to_record_batch(&self) -> astrs_data::Result<RecordBatch> {
///         let DataType::Struct(fields) = Self::data_type() else { unreachable!() };
///         let column = UInt8Array::from_values([1u8]).into_array_ref();
///         let strukt = StructArray::try_new(fields, vec![column], None)?;
///         Ok(RecordBatch::from_payload(strukt.into_array_ref()))
///     }
///     fn from_record_batch(_: &RecordBatch) -> astrs_data::Result<Self> {
///         Ok(Self)
///     }
/// }
///
/// let mut out = OpOutput::new();
/// assert!(out.is_empty());
/// out.send("tick", Metadata::new(HlcTimestamp::EPOCH), &Ping)?;
/// assert_eq!(out.len(), 1);
///
/// let sends = out.drain();
/// assert_eq!(sends[0].id().as_str(), "tick");
/// assert!(out.is_empty(), "drain empties the buffer");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Default)]
pub struct OpOutput {
    pending: Vec<OpSend>,
}

impl OpOutput {
    /// An empty output buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Buffers a send of already-encoded bytes.
    ///
    /// The escape hatch for a payload an operator built itself (or is
    /// forwarding unchanged from an input) — `send`/`send_batch` are the
    /// typed front doors that end up calling this.
    ///
    /// # Errors
    ///
    /// [`OpError::InvalidOutputId`] when `id` is not a valid
    /// [`astrs_wire::DataId`].
    pub fn send_bytes(&mut self, id: &str, metadata: Metadata, payload: Vec<u8>) -> OpResult<()> {
        let id = DataId::new(id).map_err(|source| OpError::invalid_output_id(id, source))?;
        self.pending.push(OpSend {
            id,
            metadata,
            payload,
        });
        Ok(())
    }

    /// Buffers a send of an already-built [`RecordBatch`].
    ///
    /// # Errors
    ///
    /// [`OpError::InvalidOutputId`] as [`OpOutput::send_bytes`], or
    /// [`OpError::Ipc`] when Arrow IPC encoding fails (blueprint §6.1's
    /// 256 MB payload cap, for example).
    pub fn send_batch(
        &mut self,
        id: &str,
        metadata: Metadata,
        batch: &RecordBatch,
    ) -> OpResult<()> {
        let bytes = encode_payload(batch)?.to_vec();
        self.send_bytes(id, metadata, bytes)
    }

    /// Buffers a send of a typed [`AstrsMessage`] value.
    ///
    /// Encodes `message` to its one-row [`RecordBatch`] and then to Arrow
    /// IPC bytes — the same two steps a hand-written `send_batch` call
    /// would make, saved for the common case of sending a
    /// `#[derive(AstrsMessage)]` type directly.
    ///
    /// # Errors
    ///
    /// [`OpError::InvalidOutputId`] as [`OpOutput::send_bytes`],
    /// [`OpError::Encode`] when [`AstrsMessage::to_record_batch`] fails, or
    /// [`OpError::Ipc`] as [`OpOutput::send_batch`].
    pub fn send<M: AstrsMessage>(
        &mut self,
        id: &str,
        metadata: Metadata,
        message: &M,
    ) -> OpResult<()> {
        let batch = message.to_record_batch()?;
        self.send_batch(id, metadata, &batch)
    }

    /// Removes and returns every buffered send, oldest first.
    pub fn drain(&mut self) -> Vec<OpSend> {
        std::mem::take(&mut self.pending)
    }

    /// Number of buffered sends.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing has been buffered (or everything was drained).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_data::prelude::*;
    use astrs_time::HlcTimestamp;

    #[derive(Debug, PartialEq)]
    struct Ping {
        tick: u8,
    }

    impl AstrsMessage for Ping {
        const URN: &'static str = "std/core/v1/PingMsg";

        fn data_type() -> DataType {
            DataType::strukt([Field::required("tick", DataType::UInt8)])
        }

        fn to_record_batch(&self) -> astrs_data::Result<RecordBatch> {
            let DataType::Struct(fields) = Self::data_type() else {
                unreachable!()
            };
            let column = UInt8Array::from_values([self.tick]).into_array_ref();
            let strukt = StructArray::try_new(fields, vec![column], None)?;
            Ok(RecordBatch::from_payload(strukt.into_array_ref()))
        }

        fn from_record_batch(batch: &RecordBatch) -> astrs_data::Result<Self> {
            let column =
                batch
                    .payload_column()
                    .ok_or(astrs_data::DataError::ColumnCountMismatch {
                        fields: 1,
                        columns: 0,
                    })?;
            let strukt = column.try_downcast::<StructArray>()?;
            let tick = strukt.columns()[0]
                .try_downcast::<UInt8Array>()?
                .get(0)
                .ok_or(astrs_data::DataError::RequiredFieldIsNull {
                    field: "tick".to_owned(),
                    row: 0,
                })?;
            Ok(Self { tick })
        }
    }

    fn meta() -> Metadata {
        Metadata::new(HlcTimestamp::EPOCH)
    }

    #[test]
    fn new_output_is_empty() {
        let out = OpOutput::new();
        assert!(out.is_empty());
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn send_bytes_buffers_and_drain_empties() {
        let mut out = OpOutput::new();
        out.send_bytes("frames", meta(), vec![1, 2, 3]).unwrap();
        assert_eq!(out.len(), 1);
        assert!(!out.is_empty());

        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id().as_str(), "frames");
        assert_eq!(sends[0].payload(), &[1, 2, 3]);
        assert!(out.is_empty(), "drain empties the buffer");
    }

    #[test]
    fn send_bytes_rejects_an_invalid_id() {
        let mut out = OpOutput::new();
        let err = out.send_bytes("bad id", meta(), vec![]).unwrap_err();
        assert!(matches!(err, OpError::InvalidOutputId { .. }));
        assert!(out.is_empty());
    }

    #[test]
    fn send_batch_encodes_a_record_batch() {
        let mut out = OpOutput::new();
        let batch = Ping { tick: 7 }.to_record_batch().unwrap();
        out.send_batch("tick", meta(), &batch).unwrap();
        let sends = out.drain();
        assert_eq!(sends.len(), 1);

        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        assert_eq!(Ping::from_record_batch(&decoded).unwrap(), Ping { tick: 7 });
    }

    #[test]
    fn send_encodes_a_typed_message_round_trippably() {
        let mut out = OpOutput::new();
        out.send("tick", meta(), &Ping { tick: 42 }).unwrap();
        let sends = out.drain();

        let decoded = astrs_data::ipc::decode_payload(sends[0].payload()).unwrap();
        assert_eq!(
            Ping::from_record_batch(&decoded).unwrap(),
            Ping { tick: 42 }
        );
        assert_eq!(sends[0].metadata(), &meta());
    }

    #[test]
    fn multiple_sends_accumulate_in_order() {
        let mut out = OpOutput::new();
        out.send_bytes("a", meta(), vec![1]).unwrap();
        out.send_bytes("b", meta(), vec![2]).unwrap();
        out.send_bytes("c", meta(), vec![3]).unwrap();
        let sends = out.drain();
        let ids: Vec<&str> = sends.iter().map(|s| s.id().as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn into_parts_returns_the_three_fields() {
        let mut out = OpOutput::new();
        out.send_bytes("x", meta(), vec![7]).unwrap();
        let (id, metadata, payload) = out.drain().remove(0).into_parts();
        assert_eq!(id.as_str(), "x");
        assert_eq!(metadata, meta());
        assert_eq!(payload, vec![7]);
    }
}
