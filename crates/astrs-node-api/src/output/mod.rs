//! Publishing: [`RawOutput`], [`Output<T>`](Output) and [`OutputSample`]
//! (blueprint §9.1, §6.2, §6.3).
//!
//! ```text
//!   output.send(value, meta)      typed   ─┐
//!   output.send_batch(batch, …)   columnar ├─► encode ─► send_slice
//!   output.send_bytes(bytes, …)   raw     ─┘                 │
//!                                                            ▼
//!                              len ≥ threshold and route upgraded?
//!                                    yes │            │ no
//!                                        ▼            ▼
//!                              SHM slot (no copy)   inline frame
//!   output.allocate(len)  ────────────────┘   (writes straight into the slot)
//! ```
//!
//! # The threshold
//!
//! Blueprint §6.2: *"A heap payload ≥ threshold (default 4 KiB,
//! `ASTRS_ZERO_COPY_THRESHOLD`) is copied once into a slot; below threshold it
//! rides the UDS control channel."* A 200-byte pose does not repay a ring
//! slot, and a 4-megapixel frame does not survive a control channel.
//!
//! # Never sleep-retry
//!
//! When the ring has no reclaimable slot, the send does **not** wait. It falls
//! back to the daemon path and increments the `shm_fallback_total` counter
//! ([`crate::session::SessionStats::shm_fallbacks`]) — the exact lesson §6.2
//! records from dora's PR-2366.
//!
//! # Why the handles take `&mut self`
//!
//! An output owns its shared-memory [`astrs_shm::Producer`], because
//! `astrs-shm` allows exactly one writer per ring and its `allocate` needs
//! `&mut`. That ownership is what makes `allocate(len) -> SampleMut` — the
//! genuinely zero-copy path §9.1 promises — expressible in safe Rust at all.

/// Publishing arrow-rs values through `astrs-data`'s `arrow-interop` bridge
/// (blueprint §9.1's `send_arrow(ArrayRef) [arrow-interop]`).
#[cfg(feature = "arrow-interop")]
#[cfg_attr(docsrs, doc(cfg(feature = "arrow-interop")))]
pub mod arrow;
pub mod sample;
pub mod typed;

use std::sync::Arc;

use astrs_data::ipc::encode_payload;
use astrs_data::{ArrayRef, RecordBatch};
use astrs_shm::Producer;
use astrs_wire::{DataId, Metadata, NodeRequest, OutputPayload, PortRef, ShmSegmentSpec};

use crate::error::{NodeError, Result};
use crate::session::SessionShared;
use crate::session::routes::{RoutePlane, RouteSlot, RouteUpdate};

#[cfg(feature = "arrow-interop")]
#[cfg_attr(docsrs, doc(cfg(feature = "arrow-interop")))]
pub use arrow::{ArrowSendError, ArrowSendResult};
pub use sample::{OutputSample, SampleBuffer};
pub use typed::Output;

/// An untyped publishing handle for one of the node's outputs.
pub struct RawOutput {
    /// The session this publishes through.
    shared: Arc<SessionShared>,
    /// The output's id.
    id: DataId,
    /// The route hand-off point (§6.3).
    slot: Arc<RouteSlot>,
    /// The shared-memory producer, once the route was upgraded.
    producer: Option<Producer>,
    /// The segment that producer writes into.
    segment: Option<ShmSegmentSpec>,
    /// The consumers the daemon reported at upgrade time.
    consumers: Vec<PortRef>,
    /// Set by [`RawOutput::close`].
    closed: bool,
}

impl core::fmt::Debug for RawOutput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawOutput")
            .field("id", &self.id)
            .field("plane", &self.plane())
            .field("consumers", &self.consumers.len())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl RawOutput {
    /// A handle for `id` on `shared`.
    #[must_use]
    pub fn new(shared: Arc<SessionShared>, id: DataId) -> Self {
        let slot = shared.routes.slot(&id);
        Self {
            shared,
            id,
            slot,
            producer: None,
            segment: None,
            consumers: Vec::new(),
            closed: false,
        }
    }

    /// The output's id.
    #[must_use]
    pub const fn id(&self) -> &DataId {
        &self.id
    }

    /// The plane this output is publishing on right now (§6.3).
    #[must_use]
    pub fn plane(&self) -> RoutePlane {
        if self.producer.is_some() {
            RoutePlane::Shm
        } else {
            RoutePlane::Daemon
        }
    }

    /// The segment this output publishes into, once upgraded.
    #[must_use]
    pub const fn segment(&self) -> Option<&ShmSegmentSpec> {
        self.segment.as_ref()
    }

    /// The consumers the daemon reported at upgrade time.
    #[must_use]
    pub fn consumers(&self) -> &[PortRef] {
        &self.consumers
    }

    /// The byte count at or above which a payload takes the zero-copy plane.
    #[must_use]
    pub fn zero_copy_threshold(&self) -> u64 {
        self.shared.zero_copy_threshold
    }

    /// Applies any pending route change (§6.3).
    ///
    /// Called before every publish; also public so a node can observe a
    /// transition at a moment of its choosing.
    pub fn refresh(&mut self) {
        match self.slot.take() {
            RouteUpdate::Idle => {}
            RouteUpdate::Upgrade {
                producer,
                segment,
                consumers,
            } => {
                self.producer = Some(*producer);
                self.segment = Some(segment);
                self.consumers = consumers;
                self.slot.set_plane(RoutePlane::Shm);
            }
            RouteUpdate::Downgrade { .. } => {
                // Dropping the producer closes this end of the ring; the
                // daemon has already told the consumers.
                self.producer = None;
                self.segment = None;
                self.consumers.clear();
                self.slot.set_plane(RoutePlane::Daemon);
            }
        }
    }

    /// Publishes raw payload bytes, which must already be an Arrow IPC stream
    /// (§6.1).
    ///
    /// # Errors
    ///
    /// [`NodeError::PayloadTooLarge`] over the negotiated frame budget,
    /// [`NodeError::DaemonGone`] once the session has ended, and
    /// [`NodeError::UnknownOutput`] after [`RawOutput::close`].
    pub fn send_bytes(&mut self, bytes: impl AsRef<[u8]>, metadata: Metadata) -> Result<()> {
        self.send_slice(bytes.as_ref(), metadata)
    }

    /// Encodes and publishes a record batch (§6.1).
    ///
    /// # Errors
    ///
    /// [`NodeError::Ipc`] when the batch cannot be encoded, plus whatever
    /// [`RawOutput::send_bytes`] reports.
    pub fn send_batch(&mut self, batch: &RecordBatch, metadata: Metadata) -> Result<()> {
        let encoded = encode_payload(batch)?;
        self.send_slice(encoded.as_slice(), metadata)
    }

    /// Publishes one column as a whole payload (§6.1's single-payload-column
    /// convention).
    ///
    /// The shorthand behind blueprint §9.1's `send_arrow(ArrayRef)` for a
    /// caller that already holds a single [`ArrayRef`] — including one an
    /// arrow-rs array became through
    /// `astrs_data::interop::from_arrow_array`, which is how an
    /// `arrow_array::ArrayRef` reaches this output without this crate
    /// depending on arrow-rs (see [`arrow`]'s module docs). Unfeatured,
    /// because nothing about it needs the bridge.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_batch`].
    pub fn send_array(&mut self, array: ArrayRef, metadata: Metadata) -> Result<()> {
        self.send_batch(&RecordBatch::from_payload(array), metadata)
    }

    /// Publishes an already-encoded payload.
    ///
    /// The one place both planes meet: everything above funnels here.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn send_slice(&mut self, bytes: &[u8], metadata: Metadata) -> Result<()> {
        self.check_open()?;
        self.check_size(bytes.len())?;
        self.refresh();

        if self.should_use_shm(bytes.len())
            && let Some(payload) = self.write_into_slot(bytes, &metadata)?
        {
            self.shared.count_zero_copy_send();
            return self.publish(payload, metadata);
        }
        self.shared.count_inline_send();
        self.publish(OutputPayload::inline(bytes.to_vec()), metadata)
    }

    /// Reserves `len` bytes to write into, without copying (§9.1).
    ///
    /// On the zero-copy plane the returned buffer *is* the ring slot; on the
    /// daemon path it is an owned heap buffer that the same
    /// [`OutputSample::send`] publishes inline. A node therefore writes one
    /// code path and gets the best plane available at that instant.
    ///
    /// # Errors
    ///
    /// [`NodeError::PayloadTooLarge`] over the frame budget, and
    /// [`NodeError::UnknownOutput`] after [`RawOutput::close`].
    pub fn allocate(&mut self, len: usize) -> Result<OutputSample<'_>> {
        self.check_open()?;
        self.check_size(len)?;
        self.refresh();

        let shared = Arc::clone(&self.shared);
        let id = self.id.clone();
        let segment = self
            .segment
            .as_ref()
            .map(|segment| (segment.name.clone(), segment.generation));

        if self.should_use_shm(len)
            && let Some(producer) = self.producer.as_mut()
        {
            match producer.try_allocate(len) {
                Ok(window) => {
                    return Ok(OutputSample::shm(window, shared, id, segment));
                }
                Err(_) => {
                    // §6.2: never sleep-retry. Fall back to the heap and let
                    // the daemon path carry it.
                    shared.count_shm_fallback();
                }
            }
        }
        Ok(OutputSample::heap(vec![0; len], shared, id))
    }

    /// Tells the daemon this output will produce nothing further, as a
    /// deliberate call from a node that goes on running (§7.3
    /// [`NodeRequest::OutputDone`]).
    ///
    /// Idempotent.
    ///
    /// # What the daemon does with it, and why the spelling matters
    ///
    /// `OutputDone` is what blueprint §12 calls *an explicit output-close on a
    /// live process*: the daemon tells this port's consumers
    /// `ProducerFinished` at once, because nothing about the call predicts the
    /// node's exit and making a live graph wait for one would stall it.
    ///
    /// [`RawOutput::drop`] deliberately does **not** use this spelling — see
    /// its own documentation.
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`] once the session has ended.
    pub fn close(&mut self) -> Result<()> {
        let request = NodeRequest::OutputDone {
            output: self.id.clone(),
        };
        self.close_with(request)
    }

    /// The closure a *disappearing handle* sends: §7.3's
    /// [`NodeRequest::CloseOutputs`], the request the protocol defines as
    /// "what a node does as it exits".
    ///
    /// Idempotent, and the same local teardown as [`RawOutput::close`] — only
    /// the frame differs, and that difference is the whole of §12's truthful
    /// producer-failure rule: the daemon holds a teardown closure until the
    /// reap says whether the process finished or crashed.
    fn close_on_drop(&mut self) -> Result<()> {
        let request = NodeRequest::CloseOutputs {
            outputs: vec![self.id.clone()],
        };
        self.close_with(request)
    }

    /// Retires the local half of the output and sends `request`, once.
    fn close_with(&mut self, request: NodeRequest) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if let Some(producer) = self.producer.as_mut() {
            producer.close();
        }
        self.producer = None;
        self.slot.set_plane(RoutePlane::Daemon);
        self.shared.send_request(request)
    }

    /// Whether [`RawOutput::close`] has been called.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// A fresh metadata block stamped with this node's clock (§4.3).
    #[must_use]
    pub fn metadata(&self) -> Metadata {
        Metadata::new(self.shared.hlc_now())
    }

    /// The session this output publishes through.
    #[must_use]
    pub fn session(&self) -> &Arc<SessionShared> {
        &self.shared
    }

    /// Whether a payload of `len` bytes should take the zero-copy plane.
    fn should_use_shm(&self, len: usize) -> bool {
        self.producer.is_some() && len as u64 >= self.shared.zero_copy_threshold
    }

    /// Writes into a ring slot, returning the reference to publish.
    ///
    /// `Ok(None)` means the ring could not take it and the caller should use
    /// the daemon path — never an error, because a full ring is a capacity
    /// condition rather than a fault (§6.2).
    fn write_into_slot(
        &mut self,
        bytes: &[u8],
        metadata: &Metadata,
    ) -> Result<Option<OutputPayload>> {
        let Some((name, generation)) = self
            .segment
            .as_ref()
            .map(|segment| (segment.name.clone(), segment.generation))
        else {
            return Ok(None);
        };
        let meta_bytes = encode_metadata(metadata)?;
        let Some(producer) = self.producer.as_mut() else {
            return Ok(None);
        };
        let mut window = match producer.try_allocate(bytes.len()) {
            Ok(window) => window,
            Err(_) => {
                self.shared.count_shm_fallback();
                return Ok(None);
            }
        };
        let slot = window.slot_index();
        window.as_mut_slice().copy_from_slice(bytes);
        match window.commit(&meta_bytes) {
            Ok(_seq) => Ok(Some(OutputPayload::Shm {
                segment: name,
                slot,
                len: bytes.len() as u64,
                generation,
            })),
            Err(_) => {
                self.shared.count_shm_fallback();
                Ok(None)
            }
        }
    }

    /// Hands the publish to the session.
    fn publish(&self, payload: OutputPayload, metadata: Metadata) -> Result<()> {
        self.shared.send_request(NodeRequest::SendMessage {
            output: self.id.clone(),
            metadata,
            payload,
        })
    }

    /// Refuses a publish on a closed output.
    fn check_open(&self) -> Result<()> {
        if self.closed {
            return Err(NodeError::UnknownOutput {
                output: self.id.clone(),
            });
        }
        if self.shared.is_closed() {
            return Err(NodeError::DaemonGone);
        }
        Ok(())
    }

    /// Refuses a payload the wire cannot carry.
    fn check_size(&self, len: usize) -> Result<()> {
        let max = self
            .shared
            .limits
            .max_payload_bytes()
            .min(astrs_data::MAX_PAYLOAD_BYTES);
        if len > max {
            return Err(NodeError::PayloadTooLarge { len, max });
        }
        Ok(())
    }
}

impl Drop for RawOutput {
    /// Closing an output on drop is what makes a node that simply returns
    /// from `main` produce an `InputClosed` downstream rather than a
    /// mysterious silence.
    ///
    /// It sends [`NodeRequest::CloseOutputs`] rather than
    /// [`NodeRequest::OutputDone`], and the difference is not cosmetic. A
    /// handle that is going away because its owner is going away carries no
    /// evidence about *how* the process ends: a node that publishes, returns
    /// from `main` and exits non-zero drops this handle first, and calling
    /// that "the producer finished" tells every consumer the opposite of what
    /// happened. §12's rule is written on exactly this distinction — the
    /// daemon holds a teardown closure until the reap resolves the exit
    /// status, and then closes with `ProducerCrashed` or `ProducerFinished`
    /// accordingly. Sending the explicit spelling here would make
    /// `ProducerCrashed` unreachable for every node that closes on the way
    /// out, which is every node written in the §9.1 style.
    fn drop(&mut self) {
        let _ = self.close_on_drop();
    }
}

/// Encodes metadata for a shared-memory slot's metadata region.
///
/// # Errors
///
/// [`NodeError::Wire`] when the metadata cannot be encoded.
pub fn encode_metadata(metadata: &Metadata) -> Result<Vec<u8>> {
    use astrs_wire::WireEncode;
    Ok(metadata.encode_to_vec()?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::events::{Event, EventSource, QueuedEvent};
    use crate::payload::Payload;
    use crate::runtime::NodeRuntime;
    use crate::session::{OUTGOING_CAPACITY, Outgoing};
    use astrs_data::array::{Float64Array, IntoArrayRef};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{
        DataflowId, DurationMs, FrameLimits, InputSpec, NodeId, NodeSource, NodeSpawnSpec,
        OutputSpec, SessionId,
    };
    use tokio::sync::mpsc;

    fn session() -> (Arc<SessionShared>, mpsc::Receiver<Outgoing>) {
        let (sender, receiver) = mpsc::channel(OUTGOING_CAPACITY);
        let spec = Arc::new(
            NodeSpawnSpec::new(
                DataflowId::from_u128(1),
                NodeId::new("camera").unwrap(),
                0,
                NodeSource::Dynamic,
            )
            .with_output(OutputSpec::new(DataId::new("image").unwrap())),
        );
        let shared = Arc::new(SessionShared::new(
            spec,
            SessionId::from_u128(1),
            sender,
            Arc::new(EventSource::new()),
            NodeRuntime::acquire().unwrap(),
            4096,
            FrameLimits::uds(),
        ));
        (shared, receiver)
    }

    fn output(shared: &Arc<SessionShared>) -> RawOutput {
        RawOutput::new(Arc::clone(shared), DataId::new("image").unwrap())
    }

    fn take_send(receiver: &mut mpsc::Receiver<Outgoing>) -> (Metadata, OutputPayload) {
        loop {
            let Ok(Outgoing::Request(request)) = receiver.try_recv() else {
                panic!("expected a request");
            };
            if let NodeRequest::SendMessage {
                metadata, payload, ..
            } = *request
            {
                return (metadata, payload);
            }
        }
    }

    /// §11.3 end to end: publishing through the real
    /// [`RawOutput::send_bytes`] → [`SessionShared::send_request`] path
    /// closes whatever input-to-output deadline measurement is open on the
    /// session's own [`EventSource`] — the seam
    /// [`SessionShared::send_request`]'s own docs describe.
    #[test]
    fn a_publish_finishes_an_open_deadline_measurement() {
        let (shared, mut receiver) = session();

        // A real, deadline-bearing input, registered exactly as
        // `node::init::register_with` registers every `spec.inputs` entry.
        let mut spec = InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("upstream", "frames").unwrap(),
        );
        // A zero budget: any real elapsed time between the input landing
        // and the publish below is already "over", so this needs no sleep
        // to be deterministic.
        spec.deadline = Some(DurationMs::new(0));
        shared.source.register_input(&spec).unwrap();
        shared.source.push_input(
            &spec.id,
            QueuedEvent::Input {
                source: PortRef::from_parts("upstream", "frames").unwrap(),
                metadata: Metadata::new(HlcTimestamp::new(1, 0)),
                payload: Payload::inline(vec![9]),
            },
        );
        assert!(
            shared.source.try_next().unwrap().is_input(),
            "the input opened a measurement"
        );
        assert_eq!(shared.source.stats().deadline_violations, 0, "not yet");

        let mut out = output(&shared);
        out.send_bytes(vec![1, 2, 3], out.metadata()).unwrap();
        let _ = take_send(&mut receiver);

        assert_eq!(
            shared.source.stats().deadline_violations,
            1,
            "the publish above must have finished the open measurement"
        );
        let event = shared
            .source
            .try_next()
            .expect("the violation reached the control lane");
        assert!(matches!(event, Event::Error(_)), "{event:?}");
    }

    /// The companion case: a publish with nothing open behaves exactly as
    /// it always has — no violation appears out of nowhere.
    #[test]
    fn a_publish_with_no_open_deadline_is_unaffected() {
        let (shared, mut receiver) = session();
        let mut out = output(&shared);
        out.send_bytes(vec![1, 2, 3], out.metadata()).unwrap();
        let _ = take_send(&mut receiver);
        assert_eq!(shared.source.stats().deadline_violations, 0);
    }

    #[test]
    fn a_fresh_output_publishes_on_the_daemon_path() {
        let (shared, mut receiver) = session();
        let mut out = output(&shared);
        assert_eq!(out.plane(), RoutePlane::Daemon);
        assert_eq!(out.id().as_str(), "image");
        assert!(out.segment().is_none());
        assert!(out.consumers().is_empty());
        assert_eq!(out.zero_copy_threshold(), 4096);

        out.send_bytes(vec![1, 2, 3], out.metadata()).unwrap();
        let (_, payload) = take_send(&mut receiver);
        assert_eq!(payload.bytes(), Some(&[1, 2, 3][..]));
        assert_eq!(shared.stats().sends_inline, 1);
    }

    #[test]
    fn a_record_batch_is_encoded_before_it_is_sent() {
        let (shared, mut receiver) = session();
        let mut out = output(&shared);
        let batch =
            RecordBatch::from_payload(Float64Array::from_values([1.0, 2.0]).into_array_ref());
        out.send_batch(&batch, out.metadata()).unwrap();
        let (_, payload) = take_send(&mut receiver);
        let bytes = payload.bytes().unwrap();
        let decoded = astrs_data::ipc::decode_payload(bytes).unwrap();
        assert_eq!(decoded, batch);
    }

    #[test]
    fn allocate_falls_back_to_the_heap_without_a_producer() {
        let (shared, mut receiver) = session();
        let mut out = output(&shared);
        let mut sample = out.allocate(8).unwrap();
        assert_eq!(sample.len(), 8);
        assert!(!sample.is_zero_copy());
        sample.as_mut_slice().copy_from_slice(&[7; 8]);
        let metadata = Metadata::new(shared.hlc_now());
        sample.send(metadata).unwrap();

        let (_, payload) = take_send(&mut receiver);
        assert_eq!(payload.bytes(), Some(&[7; 8][..]));
    }

    #[test]
    fn an_oversized_payload_is_refused_before_it_is_copied() {
        let (shared, _receiver) = session();
        let mut out = output(&shared);
        let max = shared
            .limits
            .max_payload_bytes()
            .min(astrs_data::MAX_PAYLOAD_BYTES);
        let error = out.check_size(max + 1).unwrap_err();
        assert!(matches!(error, NodeError::PayloadTooLarge { .. }));
        assert!(out.allocate(max + 1).is_err());
    }

    #[test]
    fn closing_is_idempotent_and_refuses_later_sends() {
        let (shared, mut receiver) = session();
        let mut out = output(&shared);
        out.close().unwrap();
        out.close().unwrap();
        assert!(out.is_closed());

        let error = out.send_bytes(vec![1], Metadata::default()).unwrap_err();
        assert!(matches!(error, NodeError::UnknownOutput { .. }));

        let mut closes = 0;
        while let Ok(Outgoing::Request(request)) = receiver.try_recv() {
            if matches!(*request, NodeRequest::OutputDone { .. }) {
                closes += 1;
            }
        }
        assert_eq!(closes, 1, "one OutputDone, however many close() calls");
    }

    #[test]
    fn dropping_an_output_closes_it_as_a_teardown() {
        // Blueprint §12: a disappearing handle sends `CloseOutputs`, not
        // `OutputDone`, so the daemon can wait for the exit status before
        // deciding whether the producer finished or crashed. Sending the
        // explicit spelling here would make `ProducerCrashed` unreachable for
        // every node that closes on the way out.
        let (shared, mut receiver) = session();
        let id = output(&shared).id.clone();
        drop(output(&shared));
        let Ok(Outgoing::Request(request)) = receiver.try_recv() else {
            panic!("expected a request");
        };
        match &*request {
            NodeRequest::CloseOutputs { outputs } => assert_eq!(outputs, &[id]),
            other => panic!("expected CloseOutputs, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_close_stays_an_output_done() {
        // The other half of the same rule: a node that closes a port and goes
        // on running must not have its consumers wait for an exit.
        let (shared, mut receiver) = session();
        let mut out = output(&shared);
        out.close().unwrap();
        let Ok(Outgoing::Request(request)) = receiver.try_recv() else {
            panic!("expected a request");
        };
        assert!(matches!(*request, NodeRequest::OutputDone { .. }));
    }

    #[test]
    fn a_closed_session_refuses_publishing() {
        let (shared, _receiver) = session();
        let mut out = output(&shared);
        shared.close();
        assert!(matches!(
            out.send_bytes(vec![1], Metadata::default()),
            Err(NodeError::DaemonGone)
        ));
    }

    #[test]
    fn a_route_downgrade_is_picked_up_on_the_next_publish() {
        let (shared, _receiver) = session();
        let mut out = output(&shared);
        shared
            .routes
            .slot(&DataId::new("image").unwrap())
            .offer_downgrade(astrs_wire::RouteDowngradeReason::ConsumerDetached {
                consumer: PortRef::from_parts("detect", "frames").unwrap(),
            });
        out.refresh();
        assert_eq!(out.plane(), RoutePlane::Daemon);
        assert!(out.segment().is_none());
    }

    #[test]
    fn metadata_is_stamped_with_the_node_clock() {
        let (shared, _receiver) = session();
        let out = output(&shared);
        let first = out.metadata();
        let second = out.metadata();
        assert!(second.timestamp >= first.timestamp);
        assert!(format!("{out:?}").contains("image"));
    }

    #[test]
    fn metadata_encodes_for_a_slot() {
        let metadata = Metadata::default();
        assert!(!encode_metadata(&metadata).unwrap().is_empty());
    }
}
