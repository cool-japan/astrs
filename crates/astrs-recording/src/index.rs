//! The seekable index footer (blueprint §14): entry offsets plus a time
//! index, written once as the second-to-last frame in the file.

use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, NodeId, WireDecode, WireEncode};
use oxicode::{Decode, Encode};

use crate::error::RecordingError;
use crate::format::{self, FOOTER_FRAME_TAG};

/// One entry's position and identity, as recorded in the footer.
///
/// Carrying `node`/`output`/`hlc` here (duplicating what is also inside
/// the entry frame at `offset`) is what makes [`crate::Reader`]'s
/// time-range and by-port queries seekable: a reader answers "which
/// entries are in this range/on this port" from the footer alone, and
/// only then seeks to and decompresses the handful of entry frames that
/// actually matched — never the whole file.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct IndexEntry {
    /// The entry frame's absolute byte offset in the file.
    pub offset: u64,
    /// The entry's HLC timestamp.
    pub hlc: HlcTimestamp,
    /// The producer node id.
    pub node: NodeId,
    /// The producer output id.
    pub output: DataId,
    /// The entry's payload length in bytes, uncompressed — enough for
    /// `astrs bag info` to report byte totals without decompressing
    /// anything.
    pub payload_len: u64,
}

/// The footer frame's decoded body: one [`IndexEntry`] per entry that was
/// in the file when [`crate::Writer::finish`] ran, in the order they were
/// written.
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
pub struct Footer {
    /// The recording's index, in on-disk (append) order.
    pub entries: Vec<IndexEntry>,
}

impl Footer {
    /// An empty footer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Appends this footer as one frame to `out`, returning the absolute
    /// offset the frame started at and its total length on disk — exactly
    /// what the trailer needs to point back at it.
    ///
    /// # Errors
    ///
    /// [`RecordingError::Wire`] if the footer cannot be encoded.
    pub(crate) fn write(&self, out: &mut Vec<u8>) -> Result<(u64, u32), RecordingError> {
        let start = out.len() as u64;
        let plain = self.encode_to_vec()?;
        format::write_frame(out, FOOTER_FRAME_TAG, &plain)?;
        let written_len = out.len() as u64 - start;
        Ok((start, u32::try_from(written_len).unwrap_or(u32::MAX)))
    }

    /// Reads a footer frame from `bytes`, which must contain exactly one
    /// frame (the caller already knows its length from the trailer).
    ///
    /// # Errors
    ///
    /// As [`format::read_frame`], plus [`RecordingError::UnknownFrameTag`]
    /// and [`RecordingError::Malformed`] as [`crate::entry::Entry::read`]
    /// documents for the analogous case.
    pub(crate) fn read(bytes: &[u8], offset: u64) -> Result<Self, RecordingError> {
        let frame = format::read_frame(bytes, offset)?;
        if frame.tag != FOOTER_FRAME_TAG {
            return Err(RecordingError::UnknownFrameTag {
                offset,
                found: frame.tag,
            });
        }
        Self::decode_exact(&frame.body).map_err(|error| RecordingError::Malformed {
            offset,
            reason: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sample() -> Footer {
        Footer {
            entries: vec![
                IndexEntry {
                    offset: 42,
                    hlc: HlcTimestamp::new(1, 0),
                    node: NodeId::new("camera").unwrap(),
                    output: DataId::new("frames").unwrap(),
                    payload_len: 128,
                },
                IndexEntry {
                    offset: 200,
                    hlc: HlcTimestamp::new(2, 0),
                    node: NodeId::new("detector").unwrap(),
                    output: DataId::new("boxes").unwrap(),
                    payload_len: 16,
                },
            ],
        }
    }

    #[test]
    fn a_footer_round_trips() {
        let footer = sample();
        let mut bytes = Vec::new();
        let (start, len) = footer.write(&mut bytes).unwrap();
        assert_eq!(start, 0);
        assert_eq!(len as usize, bytes.len());
        let read_back = Footer::read(&bytes, 0).unwrap();
        assert_eq!(read_back, footer);
    }

    #[test]
    fn an_empty_footer_round_trips() {
        let footer = Footer::new();
        let mut bytes = Vec::new();
        footer.write(&mut bytes).unwrap();
        assert_eq!(Footer::read(&bytes, 0).unwrap(), footer);
    }

    #[test]
    fn a_large_index_still_round_trips() {
        let entries = (0..10_000)
            .map(|i| IndexEntry {
                offset: i * 100,
                hlc: HlcTimestamp::new(i, 0),
                node: NodeId::new("n").unwrap(),
                output: DataId::new("o").unwrap(),
                payload_len: 10,
            })
            .collect();
        let footer = Footer { entries };
        let mut bytes = Vec::new();
        footer.write(&mut bytes).unwrap();
        assert_eq!(Footer::read(&bytes, 0).unwrap(), footer);
    }

    #[test]
    fn writing_at_a_nonzero_offset_reports_it() {
        let mut bytes = vec![0xAA; 17];
        let footer = sample();
        let (start, len) = footer.write(&mut bytes).unwrap();
        assert_eq!(start, 17);
        let frame_bytes = &bytes[17..17 + len as usize];
        assert_eq!(Footer::read(frame_bytes, 17).unwrap(), footer);
    }
}
