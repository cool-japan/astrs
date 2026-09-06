//! The `.arec` file header (blueprint §14): everything a reader needs to
//! know before it looks at a single entry.

use astrs_time::HlcTimestamp;
use astrs_wire::{DataflowId, WireDecode, WireEncode};
use oxicode::{Decode, Encode};

use crate::error::RecordingError;
use crate::format::{self, HEADER_FRAME_TAG, PROLOGUE_LEN};

/// The recording's header: the HLC epoch recording began at, the
/// dataflow id, and the dataflow's manifest embedded verbatim.
///
/// # Examples
///
/// ```
/// use astrs_recording::Header;
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::DataflowId;
///
/// let header = Header::new(DataflowId::from_u128(1), HlcTimestamp::new(1_000, 0))
///     .with_manifest_yaml("nodes: []\n");
/// assert_eq!(header.dataflow, DataflowId::from_u128(1));
/// assert_eq!(header.manifest_yaml, "nodes: []\n");
/// ```
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct Header {
    /// The HLC reading recording began at.
    ///
    /// A reference point for human-readable summaries (`astrs bag info`)
    /// and for [`crate::WriterOptions::new`]'s callers to pick a sensible
    /// value from their own clock — it is not itself load-bearing for entry
    /// ordering, since every entry carries its own timestamp in its
    /// [`astrs_wire::Metadata`].
    pub hlc_epoch: HlcTimestamp,
    /// The dataflow this recording captured.
    pub dataflow: DataflowId,
    /// The dataflow's manifest, as YAML, exactly as `astrs-manifest` would
    /// serialize it. Empty when the writer was not given one.
    pub manifest_yaml: String,
}

impl Header {
    /// A header for `dataflow`, stamped at `hlc_epoch`, with no manifest
    /// embedded yet.
    #[must_use]
    pub fn new(dataflow: DataflowId, hlc_epoch: HlcTimestamp) -> Self {
        Self {
            hlc_epoch,
            dataflow,
            manifest_yaml: String::new(),
        }
    }

    /// Attaches the dataflow's manifest YAML.
    #[must_use]
    pub fn with_manifest_yaml(mut self, yaml: impl Into<String>) -> Self {
        self.manifest_yaml = yaml.into();
        self
    }

    /// Writes the file magic, format version and this header's frame to
    /// `out`.
    ///
    /// # Errors
    ///
    /// [`RecordingError::Wire`] if the header cannot be encoded (an
    /// allocation failure).
    pub(crate) fn write(&self, out: &mut Vec<u8>) -> Result<(), RecordingError> {
        out.extend_from_slice(format::FILE_MAGIC);
        out.extend_from_slice(&format::FORMAT_VERSION.to_le_bytes());
        let plain = self.encode_to_vec()?;
        format::write_frame(out, HEADER_FRAME_TAG, &plain)
    }

    /// Reads the file magic, format version and header frame from the
    /// start of `bytes`.
    ///
    /// Returns the header and how many bytes of `bytes` it consumed, so a
    /// caller can continue reading entries immediately after.
    ///
    /// # Errors
    ///
    /// - [`RecordingError::TooShort`] if `bytes` does not even hold the
    ///   fixed prologue.
    /// - [`RecordingError::BadMagic`] if the prologue's magic does not
    ///   match.
    /// - [`RecordingError::UnsupportedVersion`] if the format version is
    ///   newer than this build understands.
    /// - Whatever [`format::read_frame`] reports for the header frame
    ///   itself, or [`RecordingError::Malformed`] if a structurally valid
    ///   frame does not decode as a [`Header`].
    pub(crate) fn read(bytes: &[u8]) -> Result<(Self, usize), RecordingError> {
        let prologue_len = PROLOGUE_LEN as usize;
        if bytes.len() < prologue_len {
            return Err(RecordingError::TooShort {
                len: bytes.len() as u64,
            });
        }
        format::check_prologue(&bytes[..prologue_len])?;

        let frame = format::read_frame(&bytes[prologue_len..], PROLOGUE_LEN)?;
        if frame.tag != HEADER_FRAME_TAG {
            return Err(RecordingError::UnknownFrameTag {
                offset: PROLOGUE_LEN,
                found: frame.tag,
            });
        }
        let header =
            Self::decode_exact(&frame.body).map_err(|error| RecordingError::Malformed {
                offset: PROLOGUE_LEN,
                reason: error.to_string(),
            })?;
        Ok((header, prologue_len + frame.consumed))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sample() -> Header {
        Header::new(DataflowId::from_u128(7), HlcTimestamp::new(1_000, 3))
            .with_manifest_yaml("nodes:\n  - id: camera\n    path: ./camera\n")
    }

    #[test]
    fn a_header_round_trips() {
        let header = sample();
        let mut bytes = Vec::new();
        header.write(&mut bytes).unwrap();
        let (read_back, consumed) = Header::read(&bytes).unwrap();
        assert_eq!(read_back, header);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn an_empty_manifest_round_trips() {
        let header = Header::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH);
        let mut bytes = Vec::new();
        header.write(&mut bytes).unwrap();
        let (read_back, _) = Header::read(&bytes).unwrap();
        assert!(read_back.manifest_yaml.is_empty());
    }

    #[test]
    fn a_short_file_is_too_short_not_a_panic() {
        for len in 0..PROLOGUE_LEN as usize {
            assert!(matches!(
                Header::read(&vec![0u8; len]),
                Err(RecordingError::TooShort { .. })
            ));
        }
    }

    #[test]
    fn a_wrong_magic_is_rejected() {
        let mut bytes = sample().write_to_vec();
        bytes[0] = b'X';
        assert!(matches!(
            Header::read(&bytes),
            Err(RecordingError::BadMagic { .. })
        ));
    }

    #[test]
    fn a_future_version_is_rejected() {
        let mut bytes = sample().write_to_vec();
        bytes[8..10].copy_from_slice(&(format::FORMAT_VERSION + 1).to_le_bytes());
        assert!(matches!(
            Header::read(&bytes),
            Err(RecordingError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn a_corrupted_header_frame_is_rejected() {
        let mut bytes = sample().write_to_vec();
        let mid = bytes.len() - 5;
        bytes[mid] ^= 0xFF;
        assert!(matches!(
            Header::read(&bytes),
            Err(RecordingError::CrcMismatch { .. })
        ));
    }

    impl Header {
        /// Test helper: [`Header::write`] into a fresh `Vec`.
        fn write_to_vec(&self) -> Vec<u8> {
            let mut out = Vec::new();
            self.write(&mut out).unwrap();
            out
        }
    }
}
