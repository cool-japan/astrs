//! K-way HLC-ordered merge of [`LogRecord`] streams.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use astrs_time::HlcTimestamp;

use crate::record::LogRecord;

/// One buffered element of a [`LogMerger`]'s internal heap: the next
/// not-yet-emitted record from one source, plus that source's remaining
/// iterator and its index (for deterministic tie-breaking).
struct HeapEntry<I> {
    head: LogRecord,
    source: usize,
    rest: I,
}

impl<I> HeapEntry<I> {
    /// The full ordering key: the record's own `(hlc, node, seq)` plus
    /// the source index as a final, always-deterministic tie-break.
    fn key(&self) -> (HlcTimestamp, Option<&str>, u64, usize) {
        let (hlc, node, seq) = self.head.merge_key();
        (hlc, node, seq, self.source)
    }
}

impl<I> PartialEq for HeapEntry<I> {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl<I> Eq for HeapEntry<I> {}

impl<I> PartialOrd for HeapEntry<I> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<I> Ord for HeapEntry<I> {
    /// Reversed against the natural key order: [`BinaryHeap`] is a
    /// max-heap, but a k-way merge needs to pop the *smallest* key first,
    /// so the entry with the smallest natural key must compare as the
    /// *greatest* `HeapEntry`.
    fn cmp(&self, other: &Self) -> Ordering {
        other.key().cmp(&self.key())
    }
}

/// Merges multiple already-sorted [`LogRecord`] iterators into one
/// globally HLC-ordered iterator via an O(log k)-per-element k-way
/// binary-heap merge (blueprint §13: this is what backs `astrs logs -f`
/// merging every daemon's log stream in a cluster into one timeline).
///
/// # Precondition
///
/// Every input iterator **must already** yield records in non-decreasing
/// [`LogRecord::merge_key`] order (`(hlc, node, seq)` ascending). This is
/// a logic precondition the merger does not — cannot, without buffering
/// every source in full — check. Violating it does not panic or corrupt
/// other sources; it just means the corresponding stretch of the merged
/// output is not actually sorted. A [`crate::reader::LogFileReader`]
/// reading a file written by [`crate::rotate::RotatingWriter`] satisfies
/// it by construction, since records are appended in the order they were
/// produced and `seq` only increases per source.
///
/// # Combining with [`crate::LogFileReader`]
///
/// `LogFileReader` yields `Result<LogRecord>` (a malformed line is a
/// per-line error, not a whole-file abort), while `LogMerger` needs
/// `Iterator<Item = LogRecord>`. Bridge the two explicitly at the call
/// site — eagerly, `reader.collect::<Result<Vec<_>>>()?` before merging
/// (strict: one bad line fails the whole source); or leniently,
/// `reader.filter_map(Result::ok)` (best-effort: a bad line is silently
/// skipped rather than reported). This crate does not pick one for you,
/// since the two failure modes suit different callers (a one-shot `astrs
/// bag convert` wants strict; a live `astrs logs -f` tail probably wants
/// lenient).
///
/// # Ordering and stability
///
/// Output order is `(hlc, node, seq)` ascending. Two records from
/// *different* sources producing an identical `(hlc, node, seq)` key is
/// possible only for adversarial/malformed input (a single well-behaved
/// producer's `seq` is unique per node) — when it happens, the source
/// passed earlier to [`LogMerger::new`]/[`merge_logs`] wins,
/// deterministically, rather than leaving the tie to `BinaryHeap`'s
/// unspecified internal order.
///
/// # Examples
///
/// ```
/// use astrs_log::{merge_logs, HlcTimestamp, LogLevel, LogRecord};
///
/// let mk = |n: u64, seq: u64| {
///     LogRecord::new(HlcTimestamp::new(n, 0), LogLevel::Info, "t", "m").with_seq(seq)
/// };
/// let a = vec![mk(0, 0), mk(2, 1)];
/// let b = vec![mk(1, 0), mk(3, 1)];
///
/// let merged: Vec<_> = merge_logs(vec![a.into_iter(), b.into_iter()])
///     .map(|r| r.hlc.physical_ns())
///     .collect();
/// assert_eq!(merged, vec![0, 1, 2, 3]);
/// ```
pub struct LogMerger<I> {
    heap: BinaryHeap<HeapEntry<I>>,
}

impl<I: Iterator<Item = LogRecord>> LogMerger<I> {
    /// Builds a merger over `sources`, pulling one record from each to
    /// seed the heap. Sources are consumed lazily beyond that — a
    /// `LogMerger` never buffers more than one pending record per source.
    #[must_use]
    pub fn new(sources: Vec<I>) -> Self {
        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (source, mut it) in sources.into_iter().enumerate() {
            if let Some(head) = it.next() {
                heap.push(HeapEntry {
                    head,
                    source,
                    rest: it,
                });
            }
        }
        Self { heap }
    }
}

impl<I: Iterator<Item = LogRecord>> Iterator for LogMerger<I> {
    type Item = LogRecord;

    fn next(&mut self) -> Option<LogRecord> {
        let HeapEntry {
            head,
            source,
            mut rest,
        } = self.heap.pop()?;
        if let Some(next_head) = rest.next() {
            self.heap.push(HeapEntry {
                head: next_head,
                source,
                rest,
            });
        }
        Some(head)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let mut lower = 0usize;
        let mut upper = Some(0usize);
        for entry in &self.heap {
            lower = lower.saturating_add(1);
            let rest_upper = entry.rest.size_hint().1;
            upper = match (upper, rest_upper) {
                (Some(u), Some(ru)) => Some(u + 1 + ru),
                _ => None,
            };
        }
        (lower, upper)
    }
}

/// Free-function form of [`LogMerger::new`], for call sites that prefer
/// `merge_logs(sources).for_each(...)` over naming the type.
#[must_use]
pub fn merge_logs<I>(sources: Vec<I>) -> LogMerger<I>
where
    I: Iterator<Item = LogRecord>,
{
    LogMerger::new(sources)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::level::LogLevel;
    use proptest::prelude::*;

    fn mk(node: &str, physical: u64, seq: u64) -> LogRecord {
        LogRecord::new(HlcTimestamp::new(physical, 0), LogLevel::Info, "t", "m")
            .with_node(node)
            .with_seq(seq)
    }

    #[test]
    fn merges_two_interleaved_sorted_sources() {
        let a = vec![mk("a", 0, 0), mk("a", 2, 1), mk("a", 4, 2)];
        let b = vec![mk("b", 1, 0), mk("b", 3, 1)];
        let merged: Vec<u64> = merge_logs(vec![a.into_iter(), b.into_iter()])
            .map(|r| r.hlc.physical_ns())
            .collect();
        assert_eq!(merged, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn empty_sources_are_skipped_cleanly() {
        let a: Vec<LogRecord> = vec![];
        let b = vec![mk("b", 0, 0)];
        let c: Vec<LogRecord> = vec![];
        let merged: Vec<_> =
            merge_logs(vec![a.into_iter(), b.into_iter(), c.into_iter()]).collect();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn no_sources_yields_empty_iterator() {
        let merged: LogMerger<std::vec::IntoIter<LogRecord>> = merge_logs(Vec::new());
        assert_eq!(merged.count(), 0);
    }

    #[test]
    fn ties_on_hlc_break_by_node_then_seq() {
        // Same physical instant, different nodes: "a" < "b" lexically.
        let a = vec![mk("a", 5, 0)];
        let b = vec![mk("b", 5, 0)];
        let merged: Vec<_> = merge_logs(vec![b.into_iter(), a.into_iter()]).collect();
        assert_eq!(merged[0].node.as_deref(), Some("a"));
        assert_eq!(merged[1].node.as_deref(), Some("b"));

        // Same node, same physical instant: seq breaks the tie.
        let s1 = vec![mk("n", 5, 1)];
        let s0 = vec![mk("n", 5, 0)];
        let merged: Vec<_> = merge_logs(vec![s1.into_iter(), s0.into_iter()]).collect();
        assert_eq!(merged[0].seq, 0);
        assert_eq!(merged[1].seq, 1);
    }

    #[test]
    fn duplicate_keys_across_sources_do_not_panic_and_are_deterministic() {
        // Pathological input: two different sources mint the exact same
        // (hlc, node, seq). The merger must not panic, and repeated runs
        // over the same input must agree with each other.
        let a = vec![mk("n", 1, 0).with_field("who", "a")];
        let b = vec![mk("n", 1, 0).with_field("who", "b")];

        let run = || -> Vec<String> {
            merge_logs(vec![a.clone().into_iter(), b.clone().into_iter()])
                .map(|r| r.fields["who"].as_str().expect("string field").to_owned())
                .collect()
        };
        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "merge of duplicate keys must be deterministic across runs"
        );
        assert_eq!(
            first,
            vec!["a".to_owned(), "b".to_owned()],
            "earlier source wins the tie"
        );
    }

    #[test]
    fn size_hint_lower_bound_matches_eventual_count() {
        let a = vec![mk("a", 0, 0), mk("a", 1, 1)];
        let b = vec![mk("b", 2, 0)];
        let merger = merge_logs(vec![a.into_iter(), b.into_iter()]);
        let (lower, upper) = merger.size_hint();
        assert_eq!(lower, 2, "one head already buffered per non-empty source");
        assert_eq!(upper, Some(3));
        assert_eq!(merger.count(), 3);
    }

    /// Builds a sorted "master" list spread across a handful of node
    /// names, with strictly increasing `seq` per node -- which makes
    /// every `(hlc, node, seq)` key in the master list unique regardless
    /// of how many physical-time collisions the random `hlc` values
    /// produce -- then shards it while preserving each shard's relative
    /// order, and asserts the merge reconstructs the master exactly.
    fn arb_master_and_shards() -> impl Strategy<Value = (Vec<LogRecord>, usize)> {
        let node_names = ["node-a", "node-b", "node-c", "node-d"];
        let per_node_counts = proptest::collection::vec(0usize..12, node_names.len());
        (
            per_node_counts,
            1usize..5,
            proptest::collection::vec(any::<u64>(), 0..48),
        )
            .prop_map(move |(counts, shard_count, physical_pool)| {
                let mut records = Vec::new();
                let mut pool_idx = 0usize;
                for (node, &count) in node_names.iter().zip(counts.iter()) {
                    for seq in 0..count as u64 {
                        let physical = physical_pool
                            .get(pool_idx % physical_pool.len().max(1))
                            .copied()
                            .unwrap_or(0)
                            % 1000; // small range so ties across nodes are common
                        pool_idx += 1;
                        records.push(mk(node, physical, seq));
                    }
                }
                records.sort_by(|a, b| a.merge_key().cmp(&b.merge_key()));
                (records, shard_count)
            })
    }

    proptest! {
        #[test]
        fn random_shards_of_a_sorted_master_remerge_identically(
            (master, shard_count) in arb_master_and_shards(),
            shard_picks in proptest::collection::vec(0usize..5, 0..48),
        ) {
            let mut shards: Vec<Vec<LogRecord>> = vec![Vec::new(); shard_count];
            for (i, record) in master.iter().enumerate() {
                let pick = shard_picks.get(i).copied().unwrap_or(0) % shard_count;
                shards[pick].push(record.clone());
            }
            let merged: Vec<LogRecord> =
                merge_logs(shards.into_iter().map(Vec::into_iter).collect()).collect();
            prop_assert_eq!(merged, master);
        }
    }
}
