//! [`Payload`] — the bytes of one input, decoded only if the node looks.
//!
//! A payload arrives one of two ways (blueprint §6.2, §6.3):
//!
//! * **Inline**, as bytes the daemon put in an `astrs_wire::NodeEvent::Input`
//!   frame — the
//!   reliable path every route starts on, and the one small messages stay on
//!   forever because a 200-byte pose does not repay a shared-memory slot.
//! * **Zero-copy**, as an [`astrs_shm::Sample`] pinned in a ring slot. Nothing
//!   was copied to get here and nothing is copied to read it: [`Payload::bytes`]
//!   hands back the mapped region itself, 128-byte aligned per §6.1 so it is
//!   directly usable as a SIMD source.
//!
//! Both are the same thing to a node: an Arrow IPC stream (§6.1). The
//! difference is *where the bytes live*, which is why [`Payload::is_zero_copy`]
//! exists but almost nothing else needs to care.
//!
//! # Where a zero-copy payload comes from
//!
//! From the session, without the node asking. When the daemon says an input
//! now reads from a ring ([`astrs_wire::NodeEvent::InputRouteUpgrade`]),
//! [`crate::session::inputs`] attaches to the segment and every message that
//! ring carries arrives here as a [`Payload::zero_copy`]. Nothing about the
//! node's loop changes: the same `Event::Input`, the same [`Payload::batch`],
//! and [`Payload::is_zero_copy`] turning true.
//!
//! Not every message on an upgraded route is zero copy. §6.2 keeps the
//! threshold rule — below it a payload rides the daemon's control channel —
//! and a ring that is momentarily full falls back the same way (§6.2: *never
//! sleep-retry*). Both arrive as [`PayloadKind::Inline`] on the same input,
//! which is exactly why a node reads [`Payload::bytes`] and not the plane.
//!
//! [`Payload::zero_copy`] stays public for a caller that brokers a segment
//! itself — a recorder replaying into a ring, an embedder outside the daemon's
//! graph. A normal pipeline does not need it.
//!
//! # Decoding is lazy, and happens once
//!
//! A node that forwards a payload, records it, or drops it on a queue-policy
//! decision must not pay for a decode it never uses. [`Payload::batch`] decodes
//! on first call and caches the result in a [`OnceLock`], so:
//!
//! ```text
//!   payload.bytes()      →  no decode, ever
//!   payload.batch()      →  decodes once; later calls are a pointer read
//!   payload.view::<T>()  →  batch(), then one typed conversion
//! ```
//!
//! # Holding a zero-copy payload holds a slot
//!
//! A zero-copy payload pins the ring slot it points at for
//! as long as it lives — that is what makes reading it safe, and it is also
//! why a node that stashes payloads in a `Vec` will eventually stall its
//! producer. [`Payload::detach`] copies out and releases the slot, which is
//! the right move for anything that outlives the current event.
//!
//! # Examples
//!
//! ```
//! use astrs_data::prelude::*;
//! use astrs_node_api::Payload;
//!
//! let batch = RecordBatch::from_payload(Float64Array::from_values([1.5, 2.5]).into_array_ref());
//! let bytes = encode_payload(&batch)?.to_vec();
//!
//! let payload = Payload::inline(bytes);
//! assert!(!payload.is_zero_copy());
//! assert_eq!(payload.batch()?.num_rows(), 2);
//! assert_eq!(
//!     payload.view::<astrs_node_api::message::ScalarRun<f64>>()?.as_slice(),
//!     &[1.5, 2.5]
//! );
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::sync::OnceLock;

use astrs_data::ipc::decode_payload;
use astrs_data::{ArrayRef, RecordBatch, SchemaHash};
use astrs_shm::Sample;

use crate::error::{NodeError, Result};
use crate::message::FromPayload;

/// Where a zero-copy payload sits in its ring (§6.2).
///
/// Everything needed to *prove* the bytes were never copied, which is what
/// blueprint §21's M1 row asks a probe to demonstrate rather than assert:
///
/// * [`SlotLocation::is_inside_mapping`] — the payload address lies within the
///   mapped segment, so it is not a heap buffer.
/// * [`SlotLocation::is_at_layout_offset`] — and it lies exactly where the
///   segment's layout puts that slot, computed from the mapping base rather
///   than from the pointer being checked.
/// * [`SlotLocation::slot`] and [`SlotLocation::sequence`] — which slot of the
///   ring, and which message. Over a long run the slots must *cycle*: a
///   copying path would hand out a fresh address every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SlotLocation {
    /// The ring slot the message occupies.
    pub slot: u32,
    /// The message's sequence number within the ring.
    pub sequence: u64,
    /// The address the payload bytes actually start at.
    pub address: usize,
    /// The address the segment's layout puts this slot's payload at.
    pub layout_address: usize,
    /// The first byte of the mapping.
    pub mapping_base: usize,
    /// How many bytes are mapped.
    pub mapping_len: usize,
}

impl SlotLocation {
    /// Whether the payload starts where the layout says this slot's payload
    /// starts.
    #[must_use]
    pub const fn is_at_layout_offset(&self) -> bool {
        self.address == self.layout_address
    }

    /// Whether the payload address is inside the mapping.
    #[must_use]
    pub const fn is_inside_mapping(&self) -> bool {
        self.address >= self.mapping_base
            && self.address < self.mapping_base.saturating_add(self.mapping_len)
    }

    /// The payload's offset from the start of the mapping.
    ///
    /// The portable form of "the same place": two processes map the same ring
    /// at whatever addresses their `mmap` chose, so the *addresses* differ
    /// between them and the offsets do not.
    #[must_use]
    pub const fn offset_in_mapping(&self) -> usize {
        self.address.saturating_sub(self.mapping_base)
    }
}

/// Where a payload's bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PayloadKind {
    /// Bytes owned by this process, carried on the control channel.
    Inline,
    /// A pinned slot in a shared-memory ring (§6.2).
    ZeroCopy,
}

impl PayloadKind {
    /// A stable name for metrics labels and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::ZeroCopy => "zero_copy",
        }
    }

    /// Whether reading this payload copies nothing.
    #[must_use]
    pub const fn is_zero_copy(self) -> bool {
        matches!(self, Self::ZeroCopy)
    }
}

impl core::fmt::Display for PayloadKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The bytes themselves, in whichever form they arrived.
#[derive(Debug)]
enum PayloadSource {
    /// Owned bytes.
    Inline(Vec<u8>),
    /// A pinned shared-memory slot. Dropping this releases the pin.
    Shm(Box<Sample>),
}

/// One message's payload: raw Arrow IPC bytes plus a decode cached on demand.
#[derive(Debug)]
pub struct Payload {
    /// The bytes.
    source: PayloadSource,
    /// The decoded batch, filled by the first [`Payload::batch`] call.
    decoded: OnceLock<RecordBatch>,
}

impl Payload {
    /// A payload over bytes this process owns.
    #[must_use]
    pub fn inline(bytes: Vec<u8>) -> Self {
        Self {
            source: PayloadSource::Inline(bytes),
            decoded: OnceLock::new(),
        }
    }

    /// An empty payload.
    #[must_use]
    pub fn empty() -> Self {
        Self::inline(Vec::new())
    }

    /// A payload over a pinned shared-memory slot (§6.2).
    ///
    /// The slot stays pinned — and therefore unavailable to the producer —
    /// until this payload is dropped or [`Payload::detach`]ed.
    #[must_use]
    pub fn zero_copy(sample: Sample) -> Self {
        Self {
            source: PayloadSource::Shm(Box::new(sample)),
            decoded: OnceLock::new(),
        }
    }

    /// A payload holding an already-decoded batch, for a node that produced
    /// one locally (the testing harness, the replay node).
    ///
    /// # Errors
    ///
    /// [`NodeError::Data`] when the batch cannot be encoded, which is what
    /// keeps [`Payload::bytes`] total for every payload however it was built.
    pub fn from_batch(batch: RecordBatch) -> Result<Self> {
        let bytes = astrs_data::ipc::encode_payload(&batch)?.to_vec();
        let payload = Self::inline(bytes);
        // The encode above proves the batch is well formed, so seeding the
        // cache with it saves the round trip a later `batch()` would pay.
        let _ = payload.decoded.set(batch);
        Ok(payload)
    }

    /// Which plane this payload came in on.
    #[must_use]
    pub const fn kind(&self) -> PayloadKind {
        match self.source {
            PayloadSource::Inline(_) => PayloadKind::Inline,
            PayloadSource::Shm(_) => PayloadKind::ZeroCopy,
        }
    }

    /// Whether reading this payload copies nothing.
    #[must_use]
    pub const fn is_zero_copy(&self) -> bool {
        self.kind().is_zero_copy()
    }

    /// The raw Arrow IPC bytes, without decoding anything.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match &self.source {
            PayloadSource::Inline(bytes) => bytes,
            PayloadSource::Shm(sample) => sample.payload(),
        }
    }

    /// How many bytes the payload holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }

    /// The payload's base address, for a caller that wants to assert the
    /// §6.1 128-byte alignment contract itself.
    #[must_use]
    pub fn address(&self) -> usize {
        self.bytes().as_ptr() as usize
    }

    /// Whether the payload's base address satisfies `alignment`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_node_api::Payload;
    ///
    /// // An inline payload makes no alignment promise; a zero-copy one does.
    /// let payload = Payload::empty();
    /// let _ = payload.is_aligned_to(128);
    /// ```
    #[must_use]
    pub fn is_aligned_to(&self, alignment: usize) -> bool {
        alignment != 0 && alignment.is_power_of_two() && self.address().is_multiple_of(alignment)
    }

    /// Copies the bytes out.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes().to_vec()
    }

    /// Releases any shared-memory slot, copying the bytes into this process.
    ///
    /// A no-op for an inline payload. Call this before stashing a payload
    /// beyond the event that delivered it: a pinned slot the producer cannot
    /// reclaim is the one way a well-behaved consumer can stall a ring.
    ///
    /// The decoded batch, if one was cached, survives — it never pointed into
    /// the mapping in the first place, since decoding copies buffers into
    /// aligned owned memory.
    #[must_use]
    pub fn detach(self) -> Self {
        match self.source {
            PayloadSource::Inline(_) => self,
            PayloadSource::Shm(ref sample) => {
                let bytes = sample.payload().to_vec();
                let decoded = self.decoded;
                Self {
                    source: PayloadSource::Inline(bytes),
                    decoded,
                }
            }
        }
    }

    /// The decoded record batch, decoding on the first call only.
    ///
    /// # Errors
    ///
    /// [`NodeError::Data`] when the bytes are not a valid one-batch Arrow IPC
    /// stream (§6.1).
    pub fn batch(&self) -> Result<&RecordBatch> {
        if let Some(batch) = self.decoded.get() {
            return Ok(batch);
        }
        let batch = decode_payload(self.bytes())?;
        // A concurrent caller may have won the race; either value is the same
        // decode of the same bytes, so whichever landed first is kept.
        let _ = self.decoded.set(batch);
        self.decoded
            .get()
            .ok_or(NodeError::Data(astrs_data::DataError::AllocationFailed {
                bytes: 0,
            }))
    }

    /// The decoded batch, consuming the payload and releasing any slot.
    ///
    /// # Errors
    ///
    /// As [`Payload::batch`].
    pub fn into_batch(self) -> Result<RecordBatch> {
        let Self { source, decoded } = self;
        if let Some(batch) = decoded.into_inner() {
            return Ok(batch);
        }
        let bytes = match &source {
            PayloadSource::Inline(bytes) => bytes.as_slice(),
            PayloadSource::Shm(sample) => sample.payload(),
        };
        Ok(decode_payload(bytes)?)
    }

    /// The single top-level `"data"` column of the payload (§6.1).
    ///
    /// # Errors
    ///
    /// [`NodeError::Data`] when the payload does not decode, or carries no
    /// column at all.
    pub fn column(&self) -> Result<&ArrayRef> {
        let batch = self.batch()?;
        batch
            .payload_column()
            .or_else(|| batch.column(0))
            .ok_or(NodeError::Data(
                astrs_data::DataError::ColumnCountMismatch {
                    fields: 1,
                    columns: 0,
                },
            ))
    }

    /// How many rows the payload's batch holds.
    ///
    /// # Errors
    ///
    /// As [`Payload::batch`].
    pub fn num_rows(&self) -> Result<usize> {
        Ok(self.batch()?.num_rows())
    }

    /// The payload's schema fingerprint (§6.1), for cheap type-drift
    /// detection.
    ///
    /// # Errors
    ///
    /// As [`Payload::batch`].
    pub fn schema_hash(&self) -> Result<SchemaHash> {
        Ok(SchemaHash::of(self.batch()?.schema()))
    }

    /// Reads the payload as a typed value (§9.1 `data.view()?`).
    ///
    /// # Errors
    ///
    /// [`NodeError::Data`] when the payload does not decode, or when its
    /// columnar layout is not the one `T` expects.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_node_api::message::{AstrsMessage, Scalar};
    /// use astrs_node_api::Payload;
    ///
    /// let payload = Payload::from_batch(Scalar::from(42_i64).to_record_batch()?)?;
    /// assert_eq!(payload.view::<Scalar<i64>>()?.into_inner(), 42);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn view<T: FromPayload>(&self) -> Result<T> {
        Ok(T::from_batch(self.batch()?)?)
    }

    /// The sequence number of the ring slot this payload occupies, when it
    /// came in on the zero-copy plane.
    #[must_use]
    pub fn sequence(&self) -> Option<u64> {
        match &self.source {
            PayloadSource::Inline(_) => None,
            PayloadSource::Shm(sample) => Some(sample.seq()),
        }
    }

    /// Where this payload sits in its ring, when it came in on the zero-copy
    /// plane (§6.2).
    ///
    /// `None` for an inline payload, which sits in this process's heap and has
    /// no slot at all.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_node_api::Payload;
    ///
    /// // An inline payload occupies no ring slot.
    /// assert!(Payload::empty().slot().is_none());
    /// ```
    #[must_use]
    pub fn slot(&self) -> Option<SlotLocation> {
        match &self.source {
            PayloadSource::Inline(_) => None,
            PayloadSource::Shm(sample) => Some(SlotLocation {
                slot: sample.slot_index(),
                sequence: sample.seq(),
                address: sample.payload_address(),
                layout_address: sample.layout_payload_address(),
                mapping_base: sample.mapping_base(),
                mapping_len: sample.mapping_len(),
            }),
        }
    }

    /// The metadata bytes that rode beside a zero-copy payload in its slot.
    ///
    /// `None` for an inline payload, whose metadata travelled in the frame as
    /// a decoded [`astrs_wire::Metadata`] instead.
    #[must_use]
    pub fn slot_metadata(&self) -> Option<&[u8]> {
        match &self.source {
            PayloadSource::Inline(_) => None,
            PayloadSource::Shm(sample) => Some(sample.metadata()),
        }
    }
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Self::inline(bytes)
    }
}

impl From<Sample> for Payload {
    fn from(sample: Sample) -> Self {
        Self::zero_copy(sample)
    }
}

impl core::fmt::Display for Payload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} byte(s) ({})", self.len(), self.kind())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_data::array::{Float64Array, IntoArrayRef};
    use astrs_data::ipc::encode_payload;

    fn sample_batch() -> RecordBatch {
        RecordBatch::from_payload(Float64Array::from_values([1.5, 2.5, 3.5]).into_array_ref())
    }

    fn sample_bytes() -> Vec<u8> {
        encode_payload(&sample_batch()).unwrap().to_vec()
    }

    #[test]
    fn an_inline_payload_reports_its_bytes_without_decoding() {
        let bytes = sample_bytes();
        let payload = Payload::inline(bytes.clone());
        assert_eq!(payload.kind(), PayloadKind::Inline);
        assert!(!payload.is_zero_copy());
        assert_eq!(payload.bytes(), bytes.as_slice());
        assert_eq!(payload.len(), bytes.len());
        assert!(!payload.is_empty());
        assert_eq!(payload.to_vec(), bytes);
        assert_eq!(payload.sequence(), None);
        assert_eq!(payload.slot_metadata(), None);
        assert!(payload.to_string().contains("inline"));
    }

    #[test]
    fn decoding_happens_once_and_is_cached() {
        let payload = Payload::inline(sample_bytes());
        let first = payload.batch().unwrap();
        let first_address = std::ptr::from_ref(first) as usize;
        let second = payload.batch().unwrap();
        assert_eq!(std::ptr::from_ref(second) as usize, first_address);
        assert_eq!(second.num_rows(), 3);
        assert_eq!(payload.num_rows().unwrap(), 3);
    }

    #[test]
    fn a_payload_built_from_a_batch_skips_the_first_decode() {
        let payload = Payload::from_batch(sample_batch()).unwrap();
        assert_eq!(payload.batch().unwrap(), &sample_batch());
        assert_eq!(payload.bytes(), sample_bytes().as_slice());
        assert_eq!(payload.into_batch().unwrap(), sample_batch());
    }

    #[test]
    fn an_empty_payload_is_empty_and_does_not_decode() {
        let payload = Payload::empty();
        assert!(payload.is_empty());
        assert_eq!(payload.len(), 0);
        assert!(payload.batch().is_err(), "no bytes is not a valid stream");
    }

    #[test]
    fn corrupt_bytes_are_reported_not_guessed() {
        let mut bytes = sample_bytes();
        bytes[4] ^= 0xFF;
        let payload = Payload::inline(bytes);
        let error = payload.batch().unwrap_err();
        assert_eq!(error.kind_name(), "ipc");
    }

    #[test]
    fn the_payload_column_is_the_data_column() {
        let payload = Payload::inline(sample_bytes());
        let column = payload.column().unwrap();
        assert_eq!(column.len(), 3);
        assert_eq!(column.data_type(), &astrs_data::DataType::Float64);
    }

    #[test]
    fn detaching_an_inline_payload_is_a_no_op() {
        let payload = Payload::inline(sample_bytes());
        let bytes = payload.to_vec();
        let detached = payload.detach();
        assert_eq!(detached.to_vec(), bytes);
        assert_eq!(detached.kind(), PayloadKind::Inline);
    }

    #[test]
    fn schema_hashes_agree_for_identical_layouts() {
        let left = Payload::inline(sample_bytes());
        let right = Payload::from_batch(sample_batch()).unwrap();
        assert_eq!(left.schema_hash().unwrap(), right.schema_hash().unwrap());
    }

    #[test]
    fn conversions_and_as_ref_agree_with_bytes() {
        let bytes = sample_bytes();
        let payload: Payload = bytes.clone().into();
        assert_eq!(payload.as_ref(), bytes.as_slice());
    }

    #[test]
    fn alignment_is_reported_honestly() {
        let payload = Payload::inline(sample_bytes());
        assert!(payload.is_aligned_to(1));
        assert!(!payload.is_aligned_to(0), "zero is not an alignment");
        assert!(!payload.is_aligned_to(3), "only powers of two are");
    }

    #[test]
    fn payload_kinds_describe_themselves() {
        assert_eq!(PayloadKind::Inline.as_str(), "inline");
        assert_eq!(PayloadKind::ZeroCopy.as_str(), "zero_copy");
        assert!(PayloadKind::ZeroCopy.is_zero_copy());
        assert!(!PayloadKind::Inline.is_zero_copy());
        assert_eq!(PayloadKind::Inline.to_string(), "inline");
    }

    #[test]
    fn payloads_cross_thread_boundaries() {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Payload>();
    }
}
