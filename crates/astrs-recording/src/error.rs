//! [`RecordingError`] — everything that can go wrong reading or writing an
//! `.arec` container (blueprint §14).

use std::io;

use astrs_wire::WireError;

/// Errors raised by [`crate::Writer`], [`crate::Reader`],
/// [`crate::recover`] and [`crate::merge()`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecordingError {
    /// A value could not be encoded or decoded through the shared wire
    /// codec (blueprint §7.1's `oxicode`, reused here for every frame
    /// body).
    #[error("wire codec failure: {0}")]
    Wire(#[from] WireError),

    /// An underlying filesystem operation failed.
    #[error("I/O error ({context}): {source}")]
    Io {
        /// A short label for what was being attempted, for the error text.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The file does not open with the `.arec` magic (`ASTRSREC`).
    #[error("not an .arec file: {reason}")]
    BadMagic {
        /// What was wrong.
        reason: &'static str,
    },

    /// The file's format version is newer than this build understands.
    #[error("unsupported .arec format version {found} (this build understands up to {understood})")]
    UnsupportedVersion {
        /// The version the file declares.
        found: u16,
        /// The highest version this build reads.
        understood: u16,
    },

    /// A frame's declared body length runs past the bytes actually
    /// available at this point in the file — the truncation signature.
    #[error(
        "frame at byte offset {offset} is incomplete: needed {needed} more bytes, {available} were available"
    )]
    Incomplete {
        /// Where the truncated frame starts.
        offset: u64,
        /// How many bytes the frame's own header said would follow.
        needed: usize,
        /// How many bytes were actually there to read.
        available: usize,
    },

    /// A frame's CRC-32C did not match its bytes.
    #[error("frame at byte offset {offset} failed its integrity check")]
    CrcMismatch {
        /// Where the corrupted frame starts.
        offset: u64,
    },

    /// A frame's magic tag was not one this reader recognizes at this
    /// position.
    #[error("frame at byte offset {offset} has an unrecognized tag {found:#010x}")]
    UnknownFrameTag {
        /// Where the offending frame starts.
        offset: u64,
        /// The tag that was found.
        found: u32,
    },

    /// The zstd codec rejected a frame's compressed body.
    #[error("compression codec failure: {0}")]
    Codec(String),

    /// A frame's decoded body was not the type expected at this position
    /// (an oxicode structural mismatch, not a codec failure).
    #[error("frame at byte offset {offset} could not be decoded: {reason}")]
    Malformed {
        /// Where the offending frame starts.
        offset: u64,
        /// What went wrong.
        reason: String,
    },

    /// The file is shorter than a bare header, or shorter than the fixed
    /// trailer — there is structurally nothing to read or recover.
    #[error("file is too short to be an .arec container ({len} bytes)")]
    TooShort {
        /// The file's actual length.
        len: u64,
    },

    /// [`crate::Reader::open`] found no usable trailer.
    ///
    /// Not necessarily fatal: [`crate::Reader::open_or_recover`] falls back
    /// to a full scan when this is the reason `open` failed.
    #[error("no valid trailer at end of file; the file may be truncated or still being written")]
    NoTrailer,

    /// An index entry pointed at a byte offset the entry region does not
    /// contain.
    #[error("index entry at offset {offset} is out of range for a file of {len} bytes")]
    IndexOffsetOutOfRange {
        /// The out-of-range offset the index named.
        offset: u64,
        /// The file's actual length.
        len: u64,
    },
}

impl RecordingError {
    /// Wraps an [`io::Error`] with a short label for what was being done.
    #[must_use]
    pub(crate) fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }
}
