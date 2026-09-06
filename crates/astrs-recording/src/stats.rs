//! Recording statistics (`astrs bag info`, blueprint §17): everything
//! about an `.arec` file's contents that can be answered from its
//! header and footer index alone, without decompressing a single entry.

use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, DataflowId, NodeId};

use crate::reader::Reader;

/// Per-port totals within a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PortStats {
    /// How many entries were recorded from this port.
    pub count: u64,
    /// The sum of their uncompressed payload lengths, in bytes.
    pub payload_bytes: u64,
}

/// A summary of one `.arec` recording's contents.
///
/// # Examples
///
/// ```
/// use astrs_recording::{Entry, Reader, Stats, Writer, WriterOptions};
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
///
/// let path = std::env::temp_dir().join(format!("astrs-recording-stats-doctest-{}.arec", std::process::id()));
/// let mut writer = Writer::create(&path, WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH))?;
/// writer.append(Entry::new(
///     NodeId::new("camera")?,
///     DataId::new("frames")?,
///     Metadata::new(HlcTimestamp::new(5, 0)),
///     vec![0; 100],
/// ))?;
/// writer.finish()?;
///
/// let reader = Reader::open(&path)?;
/// let stats = Stats::of(&reader, std::fs::metadata(&path)?.len());
/// assert_eq!(stats.entry_count, 1);
/// assert_eq!(stats.uncompressed_payload_bytes, 100);
/// # std::fs::remove_file(&path).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct Stats {
    /// The recorded dataflow.
    pub dataflow: DataflowId,
    /// How many entries the recording holds.
    pub entry_count: usize,
    /// The file's size on disk, in bytes (as reported by the caller —
    /// [`Stats::of`] does not itself touch the filesystem).
    pub file_size_bytes: u64,
    /// The sum of every entry's uncompressed payload length.
    pub uncompressed_payload_bytes: u64,
    /// The oldest and newest entry timestamps, or [`None`] for an empty
    /// recording.
    pub hlc_range: Option<(HlcTimestamp, HlcTimestamp)>,
    /// Per-`(node, output)` totals, in node-then-output order.
    pub ports: BTreeMap<(NodeId, DataId), PortStats>,
}

impl Stats {
    /// Derives statistics for `reader` from its footer index alone.
    ///
    /// `file_size_bytes` is threaded through rather than read here so
    /// this stays a pure function of what a [`Reader`] already knows —
    /// the caller (which opened the file) already has the size for free.
    #[must_use]
    pub fn of(reader: &Reader, file_size_bytes: u64) -> Self {
        let mut uncompressed_payload_bytes = 0u64;
        let mut hlc_range: Option<(HlcTimestamp, HlcTimestamp)> = None;
        let mut ports: BTreeMap<(NodeId, DataId), PortStats> = BTreeMap::new();

        for entry in reader.index() {
            uncompressed_payload_bytes += entry.payload_len;
            hlc_range = Some(match hlc_range {
                None => (entry.hlc, entry.hlc),
                Some((min, max)) => (min.min(entry.hlc), max.max(entry.hlc)),
            });
            let port_stats = ports
                .entry((entry.node.clone(), entry.output.clone()))
                .or_default();
            port_stats.count += 1;
            port_stats.payload_bytes += entry.payload_len;
        }

        Self {
            dataflow: reader.header().dataflow,
            entry_count: reader.len(),
            file_size_bytes,
            uncompressed_payload_bytes,
            hlc_range,
            ports,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::writer::{Writer, WriterOptions};
    use astrs_wire::Metadata;
    use std::path::PathBuf;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "astrs-recording-stats-test-{}-{}.arec",
            std::process::id(),
            uniq()
        ))
    }

    #[test]
    fn stats_summarize_counts_bytes_and_range() {
        let path = temp_path();
        let options = WriterOptions::new(DataflowId::from_u128(11), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        for (hlc, node, output, len) in [
            (10u64, "camera", "frames", 100),
            (30u64, "camera", "frames", 200),
            (20u64, "detector", "boxes", 50),
        ] {
            writer
                .append_parts(
                    NodeId::new(node).unwrap(),
                    DataId::new(output).unwrap(),
                    Metadata::new(HlcTimestamp::new(hlc, 0)),
                    vec![0u8; len],
                )
                .unwrap();
        }
        writer.finish().unwrap();

        let reader = Reader::open(&path).unwrap();
        let stats = Stats::of(&reader, 12_345);
        assert_eq!(stats.dataflow, DataflowId::from_u128(11));
        assert_eq!(stats.entry_count, 3);
        assert_eq!(stats.file_size_bytes, 12_345);
        assert_eq!(stats.uncompressed_payload_bytes, 350);
        assert_eq!(
            stats.hlc_range,
            Some((HlcTimestamp::new(10, 0), HlcTimestamp::new(30, 0)))
        );
        let camera = stats
            .ports
            .get(&(
                NodeId::new("camera").unwrap(),
                DataId::new("frames").unwrap(),
            ))
            .unwrap();
        assert_eq!(camera.count, 2);
        assert_eq!(camera.payload_bytes, 300);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_recording_has_no_hlc_range() {
        let path = temp_path();
        let options = WriterOptions::new(DataflowId::from_u128(12), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        writer.finish().unwrap();
        let reader = Reader::open(&path).unwrap();
        let stats = Stats::of(&reader, 0);
        assert_eq!(stats.entry_count, 0);
        assert!(stats.hlc_range.is_none());
        assert!(stats.ports.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
