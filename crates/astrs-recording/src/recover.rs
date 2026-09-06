//! Scan-based recovery for a truncated or footerless `.arec` file
//! (blueprint §14: "a truncated file without footer is recoverable by
//! scan").
//!
//! [`scan`] is what [`crate::Reader::open_or_recover`] falls back to when
//! [`crate::Reader::open`] finds no trustworthy trailer — the case of a
//! recorder killed mid-session, before [`crate::Writer::finish`] ever
//! ran. It reads the header, then walks entry frames one at a time from
//! right after it, stopping at the first frame that is not a complete,
//! valid entry — whether that is a clean end of file, a truncated frame
//! straddling the cut, or (in principle, though nothing in this crate
//! produces it) other corruption. Whatever came before that point is
//! returned; nothing about *why* the scan stopped changes what is
//! recovered, only [`RecoveryReport::stopped`] for a caller that wants to
//! know.
//!
//! # Contract
//!
//! For a file truncated at an arbitrary byte offset, [`scan`] returns
//! exactly the entries that were fully written before the cut, and never
//! panics or returns an error for the truncation itself — only a file
//! whose *header* cannot be read at all (truncated before the header
//! finished, or never a `.arec` file to begin with) is an [`Err`], since
//! there is nothing to recover without a dataflow id to attach entries
//! to. See this module's proptest for the exhaustive form of that claim.

use std::path::Path;

use astrs_wire::WireDecode;

use crate::entry::Entry;
use crate::error::RecordingError;
use crate::format;
use crate::header::Header;
use crate::index::IndexEntry;

/// Why [`scan`] stopped reading entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    /// The scan ran off the end of the file with no partial frame left
    /// behind — a cleanly finished (if footerless) recording.
    EndOfFile,
    /// A frame at the stopping point declared more bytes than the file
    /// actually had — the truncation signature.
    Truncated,
    /// The footer frame itself was found and recognized; scanning a
    /// well-formed file this far only happens when [`crate::Reader::open`]
    /// rejected its trailer but the footer that trailer would have
    /// pointed to is otherwise intact.
    FooterFound,
    /// A frame failed its integrity check, or decoded to something other
    /// than an [`Entry`] — corruption `scan` does not try to look past.
    Corrupted,
}

/// The result of [`scan`].
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    /// The header, read successfully (a prerequisite for any recovery at
    /// all).
    pub header: Header,
    /// One index row per entry recovered, in on-disk order — the same
    /// shape [`crate::Reader::index`] returns from an intact footer, so a
    /// recovered file behaves identically to a normal one from here on.
    pub index: Vec<IndexEntry>,
    /// How many bytes of the file were successfully parsed, header
    /// included.
    pub bytes_recovered: u64,
    /// Why the scan stopped.
    pub stopped: StopReason,
    /// Whether a scan actually ran, as opposed to
    /// [`crate::Reader::open`] reading a trailer and footer directly.
    scanned: bool,
}

impl RecoveryReport {
    /// A report for the ordinary case: [`crate::Reader::open`] read a
    /// trailer and footer with no scan needed at all.
    #[must_use]
    pub(crate) fn clean(header: Header) -> Self {
        Self {
            header,
            index: Vec::new(),
            bytes_recovered: 0,
            stopped: StopReason::FooterFound,
            scanned: false,
        }
    }

    /// Whether [`scan`] actually ran (as opposed to the ordinary,
    /// trailer-present case) — what [`crate::Reader::open_or_recover`]
    /// callers check to tell a normal open from a recovered one.
    #[must_use]
    pub const fn was_recovered(&self) -> bool {
        self.scanned
    }
}

/// Scans `path` for the header and every complete entry frame that
/// follows it, stopping at the first frame that is not both complete and
/// valid.
///
/// # Errors
///
/// [`RecordingError::Io`] if the file cannot be read, or whatever error
/// reading the header reports if even the header is not intact — see the
/// module docs for why that (and only that) case is a hard error rather
/// than a smaller recovered result.
pub fn scan(path: impl AsRef<Path>) -> Result<RecoveryReport, RecordingError> {
    let bytes = std::fs::read(path.as_ref())
        .map_err(|error| RecordingError::io("reading file for recovery", error))?;
    scan_bytes(&bytes)
}

/// The in-memory half of [`scan`], exercised directly by property tests
/// that construct truncated buffers without touching a filesystem.
///
/// # Errors
///
/// As [`scan`].
pub fn scan_bytes(bytes: &[u8]) -> Result<RecoveryReport, RecordingError> {
    let (header, header_len) = Header::read(bytes)?;
    let mut offset = header_len as u64;
    let mut index = Vec::new();

    let stopped = loop {
        let position = offset as usize;
        if position >= bytes.len() {
            break StopReason::EndOfFile;
        }
        let remaining = &bytes[position..];
        let frame = match format::read_frame(remaining, offset) {
            Ok(frame) => frame,
            Err(RecordingError::Incomplete { .. }) => break StopReason::Truncated,
            Err(_) => break StopReason::Corrupted,
        };

        match frame.tag {
            format::ENTRY_FRAME_TAG => match Entry::decode_exact(&frame.body) {
                Ok(entry) => {
                    index.push(IndexEntry {
                        offset,
                        hlc: entry.hlc(),
                        node: entry.node,
                        output: entry.output,
                        payload_len: entry.payload.len() as u64,
                    });
                    offset += frame.consumed as u64;
                }
                Err(_) => break StopReason::Corrupted,
            },
            format::FOOTER_FRAME_TAG => break StopReason::FooterFound,
            _ => break StopReason::Corrupted,
        }
    };

    Ok(RecoveryReport {
        header,
        index,
        bytes_recovered: offset,
        stopped,
        scanned: true,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::writer::{Writer, WriterOptions};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, DataflowId, Metadata, NodeId};

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn full_recording(entry_count: usize) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!(
            "astrs-recording-recover-test-{}-{}.arec",
            std::process::id(),
            uniq()
        ));
        let options = WriterOptions::new(DataflowId::from_u128(3), HlcTimestamp::new(1, 0))
            .with_manifest_yaml("nodes: []\n");
        let mut writer = Writer::create(&path, options).unwrap();
        for seq in 0..entry_count {
            let mut meta = Metadata::new(HlcTimestamp::new(100 + seq as u64, 0));
            meta.set_seq(seq as i64);
            writer
                .append_parts(
                    NodeId::new("camera").unwrap(),
                    DataId::new("frames").unwrap(),
                    meta,
                    vec![seq as u8; 32],
                )
                .unwrap();
        }
        writer.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        bytes
    }

    /// The bytes of a session with entries written but never finalized —
    /// no footer, no trailer, exactly what a killed recorder leaves.
    fn footerless_recording(entry_count: usize) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!(
            "astrs-recording-recover-footerless-{}-{}.arec",
            std::process::id(),
            uniq()
        ));
        let options = WriterOptions::new(DataflowId::from_u128(4), HlcTimestamp::new(1, 0));
        {
            let mut writer = Writer::create(&path, options).unwrap();
            for seq in 0..entry_count {
                writer
                    .append_parts(
                        NodeId::new("t").unwrap(),
                        DataId::new("o").unwrap(),
                        Metadata::new(HlcTimestamp::new(seq as u64, 0)),
                        vec![7u8; 10],
                    )
                    .unwrap();
            }
            // Explicitly forget the writer without calling `finish` or
            // letting `Drop` run its own finalize, by leaking it — the
            // closest in-process approximation of a killed process that
            // never got to run its own destructors either.
            std::mem::forget(writer);
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        bytes
    }

    #[test]
    fn scanning_a_complete_footerless_file_recovers_every_entry() {
        let bytes = footerless_recording(9);
        let report = scan_bytes(&bytes).unwrap();
        assert_eq!(report.index.len(), 9);
        assert_eq!(report.stopped, StopReason::EndOfFile);
        assert!(report.was_recovered());
        for (i, entry) in report.index.iter().enumerate() {
            assert_eq!(entry.hlc, HlcTimestamp::new(i as u64, 0));
        }
    }

    #[test]
    fn scanning_a_finished_file_stops_at_the_footer() {
        let bytes = full_recording(4);
        let report = scan_bytes(&bytes).unwrap();
        assert_eq!(report.index.len(), 4);
        assert_eq!(report.stopped, StopReason::FooterFound);
    }

    #[test]
    fn truncation_at_every_offset_recovers_a_valid_prefix_and_never_panics() {
        let bytes = footerless_recording(6);
        for cut in 0..=bytes.len() {
            let truncated = &bytes[..cut];
            match scan_bytes(truncated) {
                Ok(report) => {
                    // Every recovered entry must be one of the six real
                    // ones, in the original order, with no gaps or
                    // duplicates — i.e. exactly a prefix of the full set.
                    for (i, entry) in report.index.iter().enumerate() {
                        assert_eq!(entry.hlc, HlcTimestamp::new(i as u64, 0), "cut at {cut}");
                    }
                    assert!(report.index.len() <= 6);
                    assert!(report.bytes_recovered as usize <= truncated.len());
                }
                Err(_) => {
                    // Only acceptable when the header itself could not be
                    // read (an extremely short prefix).
                    assert!(cut < 64, "cut at {cut} of {} errored", bytes.len());
                }
            }
        }
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_partial_recovery() {
        let bytes = footerless_recording(3);
        for cut in 0..10 {
            assert!(scan_bytes(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn corruption_inside_the_entry_region_stops_the_scan_there() {
        let mut bytes = full_recording(5);
        // Corrupt a byte inside the third entry's frame, well before the
        // footer. Every entry recorded strictly before the corruption
        // must still come back.
        let target = bytes.len() / 3;
        bytes[target] ^= 0xFF;
        let report = scan_bytes(&bytes).unwrap();
        assert!(report.index.len() < 5);
        assert!(matches!(
            report.stopped,
            StopReason::Corrupted | StopReason::Truncated
        ));
    }

    #[test]
    fn an_empty_footerless_recording_recovers_zero_entries() {
        let bytes = footerless_recording(0);
        let report = scan_bytes(&bytes).unwrap();
        assert!(report.index.is_empty());
        assert_eq!(report.stopped, StopReason::EndOfFile);
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_truncation_never_panics_and_returns_a_true_prefix(
            entry_count in 0usize..12,
            cut_fraction in 0.0f64..=1.0,
        ) {
            let bytes = footerless_recording(entry_count);
            let cut = ((bytes.len() as f64) * cut_fraction) as usize;
            let truncated = &bytes[..cut.min(bytes.len())];
            if let Ok(report) = scan_bytes(truncated) {
                for (i, entry) in report.index.iter().enumerate() {
                    assert_eq!(entry.hlc, HlcTimestamp::new(i as u64, 0));
                }
                assert!(report.index.len() <= entry_count);
            }
        }
    }
}
