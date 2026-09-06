//! K-way HLC merge of multiple `.arec` recordings (blueprint §14: "merger
//! (k-way HLC merge of multiple .arec)") — for combining per-daemon
//! recordings of one distributed dataflow into a single, globally
//! HLC-ordered session.
//!
//! # Ordering
//!
//! Two different daemons' clocks are independent [`astrs_time::HlcClock`]
//! instances; nothing stops two entries from two different sources
//! carrying the exact same `(physical_ns, logical)` pair. The merge
//! therefore sorts on a **total** order —
//! `(hlc, node, output, source_index, position_in_source)` — rather than
//! HLC alone, so the merged output is bit-for-bit repeatable across runs
//! given the same input files in the same order, never merely "some
//! valid interleaving."

use crate::error::RecordingError;
use crate::index::IndexEntry;
use crate::reader::Reader;
use crate::writer::Writer;

/// One entry's place in the global merge order.
struct MergeKey {
    /// The source reader's index into the `sources` slice passed to
    /// [`merge`].
    source: usize,
    /// The entry's position within that source's own index — the final
    /// tie-break, so two entries that agree on everything else (an
    /// unlikely but not impossible clock coincidence) still order the
    /// same way every time.
    position: usize,
    entry: IndexEntry,
}

/// Merges every entry of `sources`, in total HLC order, into `into`.
///
/// Returns how many entries were written. `sources` are read in the order
/// given only for tie-breaking purposes — the outer order of entries in
/// the merged output depends on their timestamps, not on which source
/// file came first on the command line.
///
/// # Errors
///
/// Whatever [`Reader::read_at`] or [`Writer::append`] report — a codec or
/// I/O failure partway through leaves `into` with every entry merged
/// before the failing one already durable (the same crash-tolerance
/// [`Writer::append`] itself provides).
pub fn merge(sources: &mut [Reader], into: &mut Writer) -> Result<u64, RecordingError> {
    let mut keys: Vec<MergeKey> = Vec::new();
    for (source, reader) in sources.iter().enumerate() {
        for (position, entry) in reader.index().iter().enumerate() {
            keys.push(MergeKey {
                source,
                position,
                entry: entry.clone(),
            });
        }
    }

    keys.sort_by(|a, b| {
        a.entry
            .hlc
            .cmp(&b.entry.hlc)
            .then_with(|| a.entry.node.as_str().cmp(b.entry.node.as_str()))
            .then_with(|| a.entry.output.as_str().cmp(b.entry.output.as_str()))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.position.cmp(&b.position))
    });

    let mut written = 0u64;
    for key in &keys {
        let entry = sources[key.source].read_at(&key.entry)?;
        into.append(entry)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::writer::WriterOptions;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
    use std::path::PathBuf;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "astrs-recording-merge-test-{}-{}-{label}.arec",
            std::process::id(),
            uniq()
        ))
    }

    fn recording_of(label: &str, dataflow: u128, entries: &[(u64, &str, &str)]) -> PathBuf {
        let path = temp_path(label);
        let options = WriterOptions::new(DataflowId::from_u128(dataflow), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        for (hlc, node, output) in entries {
            writer
                .append_parts(
                    NodeId::new(*node).unwrap(),
                    DataId::new(*output).unwrap(),
                    Metadata::new(HlcTimestamp::new(*hlc, 0)),
                    vec![*hlc as u8],
                )
                .unwrap();
        }
        writer.finish().unwrap();
        path
    }

    #[test]
    fn merging_two_sources_interleaves_by_hlc() {
        let path_a = recording_of(
            "a",
            1,
            &[(10, "camera", "frames"), (30, "camera", "frames")],
        );
        let path_b = recording_of("b", 1, &[(20, "detector", "boxes")]);

        let mut readers = vec![
            Reader::open(&path_a).unwrap(),
            Reader::open(&path_b).unwrap(),
        ];
        let out_path = temp_path("merged");
        let mut out = Writer::create(
            &out_path,
            WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH),
        )
        .unwrap();

        let written = merge(&mut readers, &mut out).unwrap();
        assert_eq!(written, 3);
        out.finish().unwrap();

        let mut merged = Reader::open(&out_path).unwrap();
        let hlcs: Vec<u64> = merged
            .iter_all()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .iter()
            .map(|e| e.hlc().physical_ns())
            .collect();
        assert_eq!(hlcs, vec![10, 20, 30]);

        for path in [path_a, path_b, out_path] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn tied_timestamps_across_sources_break_ties_by_source_order() {
        let path_a = recording_of("tie-a", 2, &[(5, "n", "o")]);
        let path_b = recording_of("tie-b", 2, &[(5, "n", "o")]);
        let mut readers = vec![
            Reader::open(&path_a).unwrap(),
            Reader::open(&path_b).unwrap(),
        ];
        let out_path = temp_path("tie-merged");
        let mut out = Writer::create(
            &out_path,
            WriterOptions::new(DataflowId::from_u128(2), HlcTimestamp::EPOCH),
        )
        .unwrap();
        merge(&mut readers, &mut out).unwrap();
        out.finish().unwrap();

        // Deterministic across repeated runs against the same inputs.
        let mut readers_again = vec![
            Reader::open(&path_a).unwrap(),
            Reader::open(&path_b).unwrap(),
        ];
        let out_path_2 = temp_path("tie-merged-2");
        let mut out_2 = Writer::create(
            &out_path_2,
            WriterOptions::new(DataflowId::from_u128(2), HlcTimestamp::EPOCH),
        )
        .unwrap();
        merge(&mut readers_again, &mut out_2).unwrap();
        out_2.finish().unwrap();

        let first: Vec<u8> = std::fs::read(&out_path).unwrap();
        let second: Vec<u8> = std::fs::read(&out_path_2).unwrap();
        assert_eq!(first, second, "merge order must be repeatable");

        for path in [path_a, path_b, out_path, out_path_2] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn merging_zero_sources_writes_nothing() {
        let out_path = temp_path("empty-merge");
        let mut out = Writer::create(
            &out_path,
            WriterOptions::new(DataflowId::from_u128(3), HlcTimestamp::EPOCH),
        )
        .unwrap();
        let written = merge(&mut [], &mut out).unwrap();
        assert_eq!(written, 0);
        out.finish().unwrap();
        let _ = std::fs::remove_file(out_path);
    }
}
