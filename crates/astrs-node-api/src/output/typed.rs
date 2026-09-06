//! [`Output<T>`] — the typed publishing handle of blueprint §9.1.
//!
//! ```no_run
//! # use astrs_node_api::prelude::*;
//! # use astrs_node_api::message::Vector3;
//! # fn run(node: &mut Node) -> Result<(), NodeError> {
//! let mut velocity = node.output::<Vector3>("velocity")?;
//! velocity.send(Vector3::new(1.0, 0.0, 0.0), velocity.metadata())?;
//! # Ok(())
//! # }
//! ```
//!
//! `Output<T>` is a thin typed skin over [`RawOutput`]: it encodes through
//! `T`'s [`AstrsMessage`] implementation and hands the bytes to exactly the
//! same send path, so a typed publish is neither slower nor differently
//! routed than an untyped one.
//!
//! # `impl Into<T>`
//!
//! [`Output::send`] takes `impl Into<T>` rather than `T`, which is what lets
//! a scalar channel read as `speed.send(2.5, meta)` even though the message
//! type is [`Scalar<f64>`](crate::message::Scalar) — see that module for why
//! the wrapper exists at all.
//!
//! # Type checking
//!
//! Opening a typed handle checks `T::URN` against the port's declared type
//! under `ASTRS_TYPE_CHECK` (§9.2): `off` skips it, `warn` logs a mismatch and
//! continues, `error` refuses the handle. The default is `warn` in 0.1.0 — a
//! type system nobody can switch on is useless, and one that breaks every
//! existing graph on upgrade is worse.

use core::marker::PhantomData;

use astrs_data::{ArrayRef, AstrsMessage, RecordBatch};
use astrs_wire::{DataId, Metadata, PortRef};

use crate::error::Result;
use crate::output::{OutputSample, RawOutput};
use crate::session::routes::RoutePlane;

/// A typed publishing handle for one of the node's outputs.
#[derive(Debug)]
pub struct Output<T> {
    /// The untyped handle everything funnels through.
    raw: RawOutput,
    /// The message type this handle publishes.
    marker: PhantomData<fn(T)>,
}

impl<T> Output<T> {
    /// Wraps an untyped handle.
    #[must_use]
    pub const fn new(raw: RawOutput) -> Self {
        Self {
            raw,
            marker: PhantomData,
        }
    }

    /// The output's id.
    #[must_use]
    pub const fn id(&self) -> &DataId {
        self.raw.id()
    }

    /// The plane this output publishes on right now (§6.3).
    #[must_use]
    pub fn plane(&self) -> RoutePlane {
        self.raw.plane()
    }

    /// The consumers the daemon reported at upgrade time.
    #[must_use]
    pub fn consumers(&self) -> &[PortRef] {
        self.raw.consumers()
    }

    /// A fresh metadata block stamped with this node's clock (§4.3).
    #[must_use]
    pub fn metadata(&self) -> Metadata {
        self.raw.metadata()
    }

    /// The untyped handle underneath, for a send this type cannot express.
    #[must_use]
    pub const fn raw(&self) -> &RawOutput {
        &self.raw
    }

    /// The untyped handle underneath, mutably.
    pub const fn raw_mut(&mut self) -> &mut RawOutput {
        &mut self.raw
    }

    /// Unwraps the typed skin.
    #[must_use]
    pub fn into_raw(self) -> RawOutput {
        self.raw
    }

    /// Publishes raw payload bytes, bypassing the type.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn send_bytes(&mut self, bytes: impl AsRef<[u8]>, metadata: Metadata) -> Result<()> {
        self.raw.send_bytes(bytes, metadata)
    }

    /// Publishes a record batch, bypassing the type.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_batch`].
    pub fn send_batch(&mut self, batch: &RecordBatch, metadata: Metadata) -> Result<()> {
        self.raw.send_batch(batch, metadata)
    }

    /// Publishes a single column as a whole payload, bypassing the type.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_array`].
    pub fn send_array(&mut self, array: ArrayRef, metadata: Metadata) -> Result<()> {
        self.raw.send_array(array, metadata)
    }

    /// Reserves `len` bytes to write into (§9.1).
    ///
    /// # Errors
    ///
    /// As [`RawOutput::allocate`].
    pub fn allocate(&mut self, len: usize) -> Result<OutputSample<'_>> {
        self.raw.allocate(len)
    }

    /// Tells the daemon this output will produce nothing further.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::close`].
    pub fn close(&mut self) -> Result<()> {
        self.raw.close()
    }
}

impl<T: AstrsMessage> Output<T> {
    /// The type URN this handle publishes.
    #[must_use]
    pub const fn type_urn(&self) -> &'static str {
        T::URN
    }

    /// Publishes one value.
    ///
    /// # Errors
    ///
    /// [`crate::NodeError::Data`] when the value cannot be encoded, plus
    /// whatever [`RawOutput::send_batch`] reports.
    pub fn send(&mut self, value: impl Into<T>, metadata: Metadata) -> Result<()> {
        let batch = value.into().to_record_batch()?;
        self.raw.send_batch(&batch, metadata)
    }

    /// Publishes one value with a freshly stamped metadata block.
    ///
    /// The shortest form, for a node with nothing to correlate.
    ///
    /// # Errors
    ///
    /// As [`Output::send`].
    pub fn publish(&mut self, value: impl Into<T>) -> Result<()> {
        let metadata = self.metadata();
        self.send(value, metadata)
    }

    /// Publishes one value, following `cause`'s correlation and timing
    /// metadata (§9.4).
    ///
    /// This is the blueprint §9.1 shape: `detections.send(run_model(&img)?,
    /// meta.follow())?` — the response of a service, the next chunk of a
    /// stream, and the derived output of a pipeline stage all keep the keys
    /// that make them traceable.
    ///
    /// # Errors
    ///
    /// As [`Output::send`].
    pub fn send_following(&mut self, value: impl Into<T>, cause: &Metadata) -> Result<()> {
        self.send(value, cause.follow())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::events::EventSource;
    use crate::message::{Scalar, Vector3};
    use crate::runtime::NodeRuntime;
    use crate::session::{OUTGOING_CAPACITY, Outgoing, SessionShared};
    use astrs_wire::{
        DataflowId, FrameLimits, NodeId, NodeRequest, NodeSource, NodeSpawnSpec, SessionId,
    };
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn handle<T>() -> (Output<T>, Arc<SessionShared>, mpsc::Receiver<Outgoing>) {
        let (sender, receiver) = mpsc::channel(OUTGOING_CAPACITY);
        let spec = Arc::new(NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("planner").unwrap(),
            0,
            NodeSource::Dynamic,
        ));
        let shared = Arc::new(SessionShared::new(
            spec,
            SessionId::from_u128(1),
            sender,
            Arc::new(EventSource::new()),
            NodeRuntime::acquire().unwrap(),
            4096,
            FrameLimits::uds(),
        ));
        let raw = RawOutput::new(Arc::clone(&shared), DataId::new("velocity").unwrap());
        (Output::new(raw), shared, receiver)
    }

    fn take_payload(receiver: &mut mpsc::Receiver<Outgoing>) -> (Metadata, Vec<u8>) {
        loop {
            let Ok(Outgoing::Request(request)) = receiver.try_recv() else {
                panic!("expected a request");
            };
            if let NodeRequest::SendMessage {
                metadata, payload, ..
            } = *request
            {
                return (metadata, payload.bytes().unwrap_or_default().to_vec());
            }
        }
    }

    #[test]
    fn a_typed_send_round_trips_through_the_wire_payload() {
        let (mut output, _shared, mut receiver) = handle::<Vector3>();
        let value = Vector3::new(1.0, 2.0, 3.0);
        output.send(value, output.metadata()).unwrap();

        let (_, bytes) = take_payload(&mut receiver);
        let batch = astrs_data::ipc::decode_payload(&bytes).unwrap();
        assert_eq!(Vector3::from_record_batch(&batch).unwrap(), value);
        assert_eq!(output.type_urn(), "std/geometry/v1/Vector3");
        assert_eq!(output.id().as_str(), "velocity");
    }

    #[test]
    fn into_makes_scalar_channels_read_naturally() {
        let (mut output, _shared, mut receiver) = handle::<Scalar<f64>>();
        output.publish(2.5_f64).unwrap();
        let (_, bytes) = take_payload(&mut receiver);
        let batch = astrs_data::ipc::decode_payload(&bytes).unwrap();
        assert_eq!(
            Scalar::<f64>::from_record_batch(&batch)
                .unwrap()
                .into_inner(),
            2.5
        );
    }

    #[test]
    fn following_keeps_the_correlation_keys() {
        let (mut output, _shared, mut receiver) = handle::<Vector3>();
        let mut cause = output.metadata();
        cause.set_request_id("req-7");
        output.send_following(Vector3::default(), &cause).unwrap();
        let (metadata, _) = take_payload(&mut receiver);
        assert_eq!(metadata.request_id(), Some("req-7"));
    }

    #[test]
    fn the_untyped_faces_are_reachable() {
        let (mut output, _shared, mut receiver) = handle::<Vector3>();
        output.send_bytes(vec![1, 2, 3], output.metadata()).unwrap();
        let (_, bytes) = take_payload(&mut receiver);
        assert_eq!(bytes, vec![1, 2, 3]);

        assert_eq!(output.plane(), RoutePlane::Daemon);
        assert!(output.consumers().is_empty());
        assert_eq!(output.raw().id().as_str(), "velocity");
        assert_eq!(output.raw_mut().id().as_str(), "velocity");

        let mut sample = output.allocate(2).unwrap();
        sample.as_mut_slice().copy_from_slice(&[8, 9]);
        sample.send(Metadata::default()).unwrap();
        let (_, bytes) = take_payload(&mut receiver);
        assert_eq!(bytes, vec![8, 9]);

        output.close().unwrap();
        assert!(output.into_raw().is_closed());
    }
}
