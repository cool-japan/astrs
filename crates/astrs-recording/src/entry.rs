//! [`Entry`] — one recorded message (blueprint §14: `{node, output, hlc,
//! metadata, payload}`).

use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, Metadata, NodeId, WireDecode, WireEncode};
use oxicode::{Decode, Encode};

use crate::error::RecordingError;
use crate::format::{self, ENTRY_FRAME_TAG};

/// One recorded message.
///
/// The blueprint's `hlc` field is not a separate one here: it is exactly
/// [`Metadata::timestamp`], so an entry never carries two timestamps that
/// could disagree. [`Entry::hlc`] reads through to it for callers that
/// want the value without reaching into `meta`.
///
/// `node`/`output` name the **producer** port the message came from —
/// `Node::input_source` on the recording node, not the recorder's own
/// input name. For a message that arrived on a recorded
/// `astrs/...` virtual source (a timer tick, a log fan-out), these are
/// whatever synthetic [`astrs_wire::PortRef`] the daemon assigned that
/// source; a reader should not assume `node` always names a real
/// manifest node, only that it round-trips exactly what was recorded.
///
/// # Examples
///
/// ```
/// use astrs_recording::Entry;
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::{DataId, Metadata, NodeId};
///
/// let entry = Entry::new(
///     NodeId::new("camera")?,
///     DataId::new("frames")?,
///     Metadata::new(HlcTimestamp::new(1_000, 0)),
///     vec![1, 2, 3],
/// );
/// assert_eq!(entry.hlc(), HlcTimestamp::new(1_000, 0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct Entry {
    /// The producer node's id.
    pub node: NodeId,
    /// The producer output's id.
    pub output: DataId,
    /// The metadata that rode beside the payload, HLC timestamp included
    /// (blueprint §6.1).
    pub meta: Metadata,
    /// The raw payload bytes, exactly as published (an Arrow IPC stream
    /// for a typed or `send_bytes` output).
    pub payload: Vec<u8>,
}

impl Entry {
    /// Builds an entry from its parts.
    #[must_use]
    pub const fn new(node: NodeId, output: DataId, meta: Metadata, payload: Vec<u8>) -> Self {
        Self {
            node,
            output,
            meta,
            payload,
        }
    }

    /// This entry's HLC timestamp — [`Metadata::timestamp`].
    #[must_use]
    pub const fn hlc(&self) -> HlcTimestamp {
        self.meta.timestamp
    }

    /// Appends this entry as one frame to `out`.
    ///
    /// # Errors
    ///
    /// [`RecordingError::Wire`] if the entry cannot be encoded.
    pub(crate) fn write(&self, out: &mut Vec<u8>) -> Result<(), RecordingError> {
        let plain = self.encode_to_vec()?;
        format::write_frame(out, ENTRY_FRAME_TAG, &plain)
    }

    /// Reads one entry frame from the start of `bytes`.
    ///
    /// `offset` is the frame's absolute position in the file, used only to
    /// annotate errors. A public primitive for a caller doing its own
    /// low-level, in-memory frame walking; [`crate::Reader`] reads
    /// entries by seeking instead, so it does not call this directly.
    ///
    /// # Errors
    ///
    /// As [`format::read_frame`], plus [`RecordingError::UnknownFrameTag`]
    /// if the frame at this position is not tagged as an entry, and
    /// [`RecordingError::Malformed`] if a structurally valid frame does
    /// not decode as an [`Entry`].
    pub fn read(bytes: &[u8], offset: u64) -> Result<(Self, usize), RecordingError> {
        let frame = format::read_frame(bytes, offset)?;
        if frame.tag != ENTRY_FRAME_TAG {
            return Err(RecordingError::UnknownFrameTag {
                offset,
                found: frame.tag,
            });
        }
        let entry = Self::decode_exact(&frame.body).map_err(|error| RecordingError::Malformed {
            offset,
            reason: error.to_string(),
        })?;
        Ok((entry, frame.consumed))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sample() -> Entry {
        let mut meta = Metadata::new(HlcTimestamp::new(42, 1));
        meta.set_seq(3);
        Entry::new(
            NodeId::new("camera").unwrap(),
            DataId::new("frames").unwrap(),
            meta,
            vec![9, 8, 7, 6],
        )
    }

    #[test]
    fn an_entry_round_trips() {
        let entry = sample();
        let mut bytes = Vec::new();
        entry.write(&mut bytes).unwrap();
        let (read_back, consumed) = Entry::read(&bytes, 0).unwrap();
        assert_eq!(read_back, entry);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn hlc_reads_through_to_the_metadata_timestamp() {
        let entry = sample();
        assert_eq!(entry.hlc(), entry.meta.timestamp);
    }

    #[test]
    fn an_empty_payload_round_trips() {
        let entry = Entry::new(
            NodeId::new("t").unwrap(),
            DataId::new("hz_10").unwrap(),
            Metadata::default(),
            Vec::new(),
        );
        let mut bytes = Vec::new();
        entry.write(&mut bytes).unwrap();
        let (read_back, _) = Entry::read(&bytes, 0).unwrap();
        assert!(read_back.payload.is_empty());
    }

    #[test]
    fn a_large_compressible_payload_round_trips_byte_for_byte() {
        let payload: Vec<u8> = (0..200_000).map(|i| (i % 17) as u8).collect();
        let entry = Entry::new(
            NodeId::new("camera").unwrap(),
            DataId::new("frames").unwrap(),
            Metadata::new(HlcTimestamp::new(1, 0)),
            payload.clone(),
        );
        let mut bytes = Vec::new();
        entry.write(&mut bytes).unwrap();
        assert!(bytes.len() < payload.len(), "expected compression to help");
        let (read_back, _) = Entry::read(&bytes, 0).unwrap();
        assert_eq!(read_back.payload, payload);
    }

    #[test]
    fn reading_a_footer_frame_as_an_entry_is_rejected() {
        let mut bytes = Vec::new();
        format::write_frame(&mut bytes, format::FOOTER_FRAME_TAG, b"not an entry").unwrap();
        assert!(matches!(
            Entry::read(&bytes, 0),
            Err(RecordingError::UnknownFrameTag { .. })
        ));
    }
}
