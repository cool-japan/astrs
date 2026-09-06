//! Fragmentation and reassembly: samples too big for one datagram.
//!
//! A `serializedPayload` larger than a datagram is cut into equal fragments
//! and sent as a series of `DATA_FRAG` submessages (§8.3.7.3). The receiver
//! reassembles, and asks for what did not arrive with `NACK_FRAG`. Blueprint
//! §10.2 fixes the threshold at 64 KiB.
//!
//! # The planning half
//!
//! [`FragmentPlan`] is the arithmetic, isolated from the network so it can be
//! property-tested: how many fragments a sample of `n` octets becomes at
//! `fragment_size`, which octets fragment `k` covers, and how many fragments
//! fit in one datagram alongside the submessage header. Every off-by-one that
//! fragmentation is famous for lives in these three functions, and each one
//! has a test that names the boundary it guards.
//!
//! Two conventions the specification fixes and this module obeys without
//! exception:
//!
//! 1. **Fragment numbers are one-based.** `fragmentStartingNum` of the first
//!    fragment is 1, not 0. [`FragmentPlan::window`] subtracts before it
//!    multiplies, so a caller cannot get this wrong by passing the wrong
//!    base.
//! 2. **Only the last fragment is short.** Every other fragment is exactly
//!    `fragmentSize` octets. The total is therefore
//!    `ceil(sampleSize / fragmentSize)`, and the last one holds
//!    `sampleSize - (total - 1) * fragmentSize`.
//!
//! # The reassembly half
//!
//! [`Reassembler`] holds one [`Assembly`] per in-flight sample per writer. It
//! is written to be hostile-input-safe: the sample size and fragment size are
//! pinned by the *first* fragment and any later disagreement is a typed
//! [`ReassemblyDefect`], the buffer is allocated once from that pinned size
//! and never grown, a fragment that re-arrives with different octets is
//! rejected rather than allowed to overwrite, and both the sample size and
//! the fragment count are bounded before anything is allocated.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::behavior::fragment::FragmentPlan;
//!
//! // A 1 MiB sample at 1344 octets per fragment.
//! let plan = FragmentPlan::new(1_048_576, 1_344)?;
//! assert_eq!(plan.total_fragments(), 781);
//!
//! // The first fragment covers octets 0..1344, the last one the remainder.
//! assert_eq!(plan.window(1)?, 0..1_344);
//! assert_eq!(plan.window(781)?, 1_048_320..1_048_576);
//! assert_eq!(plan.fragment_len(781)?, 256);
//! # Ok::<(), astrs_rtps::behavior::BehaviorError>(())
//! ```

use core::ops::Range;
use std::collections::BTreeMap;

use crate::behavior::error::{BehaviorError, BehaviorResult, ReassemblyDefect};
use crate::messages::SUBMESSAGE_HEADER_LEN;
use crate::messages::{DATA_FRAG_PRELUDE_LEN, DataFrag, FragmentGeometry, SerializedPayload};
use crate::structure::{
    EntityId, FragmentNumber, FragmentNumberSet, Guid, MAX_SET_BITS, SequenceNumber,
};

/// Samples larger than this are fragmented (blueprint §10.2).
pub const FRAGMENTATION_THRESHOLD: usize = 64 * 1024;

/// Octets per fragment when nothing else is configured.
///
/// 1344 keeps a `DATA_FRAG` carrying one fragment inside a 1500-octet
/// Ethernet MTU with room for the IP header, the UDP header, the RTPS
/// header and the submessage prelude.
pub const DEFAULT_FRAGMENT_SIZE: u16 = 1_344;

/// Largest sample this crate will reassemble.
///
/// A peer may declare any `sampleSize` up to `u32::MAX`; believing it would
/// let one datagram ask for a four-gigabyte allocation.
pub const MAX_REASSEMBLY_SAMPLE: u32 = 64 * 1024 * 1024;

/// Most fragments one sample may be cut into.
pub const MAX_FRAGMENTS: u32 = 1 << 20;

/// Most samples one writer may have in flight, unreassembled, at once.
pub const MAX_ASSEMBLIES_PER_WRITER: usize = 32;

/// How a sample is cut into fragments.
///
/// Pure arithmetic: no buffers, no network, no state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FragmentPlan {
    sample_size: u32,
    fragment_size: u16,
    total_fragments: u32,
}

impl FragmentPlan {
    /// Plan the fragmentation of a `sample_size`-octet sample.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::SampleTooLarge`] when the sample is above
    /// [`MAX_REASSEMBLY_SAMPLE`], and
    /// [`BehaviorError::Reassembly`]-shaped
    /// [`ReassemblyDefect::TooManyFragments`] when the fragment size is so
    /// small that the count exceeds [`MAX_FRAGMENTS`]. A `fragment_size` of
    /// zero is refused as a sample-size error, because zero-octet fragments
    /// describe no sample at all.
    pub fn new(sample_size: usize, fragment_size: u16) -> BehaviorResult<Self> {
        let sample_size =
            u32::try_from(sample_size).map_err(|_| BehaviorError::SampleTooLarge {
                len: sample_size,
                limit: MAX_REASSEMBLY_SAMPLE as usize,
            })?;
        Self::from_declared(
            sample_size,
            fragment_size,
            Guid::UNKNOWN,
            SequenceNumber::FIRST,
        )
    }

    /// Plan from the two fields a `DATA_FRAG` declares, naming the writer and
    /// sample so a rejection is diagnosable.
    ///
    /// # Errors
    ///
    /// As [`FragmentPlan::new`].
    pub fn from_declared(
        sample_size: u32,
        fragment_size: u16,
        writer: Guid,
        sequence_number: SequenceNumber,
    ) -> BehaviorResult<Self> {
        if sample_size > MAX_REASSEMBLY_SAMPLE {
            return Err(BehaviorError::SampleTooLarge {
                len: sample_size as usize,
                limit: MAX_REASSEMBLY_SAMPLE as usize,
            });
        }
        if fragment_size == 0 {
            return Err(BehaviorError::SampleTooLarge {
                len: sample_size as usize,
                limit: 0,
            });
        }
        let total_fragments = sample_size.div_ceil(u32::from(fragment_size));
        if total_fragments > MAX_FRAGMENTS {
            return Err(BehaviorError::Reassembly {
                writer,
                sequence_number,
                defect: ReassemblyDefect::TooManyFragments {
                    needed: u64::from(total_fragments),
                    limit: u64::from(MAX_FRAGMENTS),
                },
            });
        }
        Ok(Self {
            sample_size,
            fragment_size,
            total_fragments,
        })
    }

    /// Octets the whole sample occupies.
    #[must_use]
    pub const fn sample_size(self) -> u32 {
        self.sample_size
    }

    /// Octets per fragment, the last one excepted.
    #[must_use]
    pub const fn fragment_size(self) -> u16 {
        self.fragment_size
    }

    /// How many fragments the sample becomes.
    ///
    /// Zero for a zero-octet sample, which is not something a writer should
    /// be fragmenting.
    #[must_use]
    pub const fn total_fragments(self) -> u32 {
        self.total_fragments
    }

    /// The octets fragment `number` covers, one-based.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Reassembly`] with
    /// [`ReassemblyDefect::WindowPastSample`] when `number` is zero or past
    /// the end.
    pub fn window(self, number: u32) -> BehaviorResult<Range<usize>> {
        self.window_of(number, 1)
    }

    /// The octets `count` consecutive fragments cover, starting at `number`.
    ///
    /// # Errors
    ///
    /// As [`window`](Self::window).
    pub fn window_of(self, number: u32, count: u16) -> BehaviorResult<Range<usize>> {
        if number == 0 || count == 0 || number > self.total_fragments {
            return Err(self.window_error(number, count));
        }
        let start = u64::from(number - 1) * u64::from(self.fragment_size);
        let end = start
            .saturating_add(u64::from(count) * u64::from(self.fragment_size))
            .min(u64::from(self.sample_size));
        if start >= u64::from(self.sample_size) && self.sample_size != 0 {
            return Err(self.window_error(number, count));
        }
        let start = usize::try_from(start).unwrap_or(usize::MAX);
        let end = usize::try_from(end).unwrap_or(usize::MAX);
        Ok(start..end)
    }

    /// Octets in fragment `number`.
    ///
    /// Every fragment but the last is [`fragment_size`](Self::fragment_size);
    /// the last is whatever remains.
    ///
    /// # Errors
    ///
    /// As [`window`](Self::window).
    pub fn fragment_len(self, number: u32) -> BehaviorResult<usize> {
        Ok(self.window(number)?.len())
    }

    /// How many consecutive fragments fit in one datagram of `budget` octets.
    ///
    /// Accounts for the RTPS message header the caller will prepend, the
    /// four-octet submessage header and the twenty-eight-octet `DATA_FRAG`
    /// prelude. At least one, always — a budget too small for a single
    /// fragment still has to send that fragment, and letting the kernel
    /// fragment the IP packet is better than sending nothing.
    #[must_use]
    pub fn fragments_per_datagram(self, budget: usize) -> u16 {
        let overhead = crate::messages::HEADER_LEN + SUBMESSAGE_HEADER_LEN + DATA_FRAG_PRELUDE_LEN;
        let room = budget.saturating_sub(overhead);
        let per = room / usize::from(self.fragment_size.max(1));
        u16::try_from(per.clamp(1, usize::from(u16::MAX))).unwrap_or(1)
    }

    /// The geometry field group a `DATA_FRAG` carrying `count` fragments from
    /// `number` announces.
    #[must_use]
    pub const fn geometry(self, number: FragmentNumber, count: u16) -> FragmentGeometry {
        FragmentGeometry::new(number, count, self.fragment_size, self.sample_size)
    }

    /// True when a sample of this size needs fragmenting at all.
    #[must_use]
    pub const fn is_needed(self) -> bool {
        self.total_fragments > 1
    }

    /// The error a bad window produces.
    fn window_error(self, number: u32, count: u16) -> BehaviorError {
        let end = u64::from(number.saturating_sub(1))
            .saturating_add(u64::from(count))
            .saturating_mul(u64::from(self.fragment_size));
        BehaviorError::Reassembly {
            writer: Guid::UNKNOWN,
            sequence_number: SequenceNumber::FIRST,
            defect: ReassemblyDefect::WindowPastSample {
                end,
                sample_size: self.sample_size,
            },
        }
    }
}

/// Cut `payload` into the `DATA_FRAG` submessages that carry it.
///
/// One submessage per datagram-worth of fragments, so a caller can put each
/// straight into its own [`Message`](crate::messages::Message). The payload
/// is borrowed, not copied.
///
/// # Errors
///
/// Whatever [`FragmentPlan::new`] reports.
pub fn fragment_sample<'a>(
    reader_id: EntityId,
    writer_id: EntityId,
    sequence_number: SequenceNumber,
    payload: &'a [u8],
    fragment_size: u16,
    datagram_budget: usize,
) -> BehaviorResult<Vec<DataFrag<'a>>> {
    let plan = FragmentPlan::new(payload.len(), fragment_size)?;
    let per_datagram = plan.fragments_per_datagram(datagram_budget);
    let mut submessages = Vec::new();
    let mut next = 1_u32;
    while next <= plan.total_fragments() {
        let remaining = plan.total_fragments() - next + 1;
        let count = u16::try_from(remaining.min(u32::from(per_datagram))).unwrap_or(1);
        let window = plan.window_of(next, count)?;
        let slice = payload.get(window.clone()).unwrap_or(&[]);
        submessages.push(DataFrag::new(
            reader_id,
            writer_id,
            sequence_number,
            plan.geometry(FragmentNumber::new(next), count),
            SerializedPayload::new(slice),
        ));
        next = next.saturating_add(u32::from(count));
    }
    Ok(submessages)
}

/// One sample being reassembled.
#[derive(Debug, Clone)]
pub struct Assembly {
    plan: FragmentPlan,
    buffer: Vec<u8>,
    received: Vec<u64>,
    received_count: u32,
}

impl Assembly {
    /// Start reassembling a sample of `plan`'s shape.
    #[must_use]
    pub fn new(plan: FragmentPlan) -> Self {
        let words = (plan.total_fragments() as usize).div_ceil(64).max(1);
        Self {
            buffer: vec![0_u8; plan.sample_size() as usize],
            received: vec![0_u64; words],
            received_count: 0,
            plan,
        }
    }

    /// The shape this sample was declared with.
    #[must_use]
    pub const fn plan(&self) -> FragmentPlan {
        self.plan
    }

    /// How many distinct fragments have arrived.
    #[must_use]
    pub const fn received_count(&self) -> u32 {
        self.received_count
    }

    /// True when every fragment has arrived.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.received_count >= self.plan.total_fragments()
    }

    /// True when fragment `number` has already arrived.
    #[must_use]
    pub fn has(&self, number: u32) -> bool {
        if number == 0 || number > self.plan.total_fragments() {
            return false;
        }
        let index = (number - 1) as usize;
        let Some(word) = self.received.get(index / 64) else {
            return false;
        };
        word & (1_u64 << (index % 64)) != 0
    }

    /// Mark fragment `number` as arrived.
    fn mark(&mut self, number: u32) {
        if number == 0 || number > self.plan.total_fragments() {
            return;
        }
        let index = (number - 1) as usize;
        if let Some(word) = self.received.get_mut(index / 64) {
            let bit = 1_u64 << (index % 64);
            if *word & bit == 0 {
                *word |= bit;
                self.received_count = self.received_count.saturating_add(1);
            }
        }
    }

    /// The reassembled sample, once complete.
    #[must_use]
    pub fn finish(self) -> Option<Vec<u8>> {
        if self.is_complete() {
            Some(self.buffer)
        } else {
            None
        }
    }

    /// The first missing fragments, as a `NACK_FRAG` can express them.
    ///
    /// A `FragmentNumberSet` covers at most
    /// [`MAX_SET_BITS`] numbers from its
    /// base, so this reports the window that starts at the lowest missing
    /// fragment. `None` means nothing is missing.
    #[must_use]
    pub fn missing(&self) -> Option<FragmentNumberSet> {
        let first = (1..=self.plan.total_fragments()).find(|number| !self.has(*number))?;
        let mut set = FragmentNumberSet::new(FragmentNumber::new(first));
        let end = first
            .saturating_add(MAX_SET_BITS)
            .min(self.plan.total_fragments().saturating_add(1));
        for number in first..end {
            if !self.has(number) {
                // Bounded by construction: `number - first < MAX_SET_BITS`.
                let _ = set.insert(FragmentNumber::new(number));
            }
        }
        Some(set)
    }
}

/// Reassembles the `DATA_FRAG` series arriving from every matched writer.
///
/// One instance per reader. Keyed by `(writer, sequence number)`, because two
/// writers may legitimately be sending sample 1 at the same moment.
#[derive(Debug, Clone, Default)]
pub struct Reassembler {
    assemblies: BTreeMap<(Guid, SequenceNumber), Assembly>,
    per_writer_limit: usize,
}

impl Reassembler {
    /// A reassembler holding at most [`MAX_ASSEMBLIES_PER_WRITER`] in-flight
    /// samples per writer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            assemblies: BTreeMap::new(),
            per_writer_limit: MAX_ASSEMBLIES_PER_WRITER,
        }
    }

    /// Change the per-writer in-flight ceiling.
    #[must_use]
    pub const fn with_per_writer_limit(mut self, limit: usize) -> Self {
        self.per_writer_limit = limit;
        self
    }

    /// How many samples are part-assembled.
    #[must_use]
    pub fn len(&self) -> usize {
        self.assemblies.len()
    }

    /// True when nothing is part-assembled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assemblies.is_empty()
    }

    /// Take one `DATA_FRAG` in, and return the sample when it completes it.
    ///
    /// The first fragment of a sample pins its size and fragment size; a
    /// later fragment that disagrees is rejected without disturbing what has
    /// already been stored.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Reassembly`] naming the [`ReassemblyDefect`], or
    /// [`BehaviorError::SampleTooLarge`] when the declared sample exceeds
    /// [`MAX_REASSEMBLY_SAMPLE`].
    pub fn accept(
        &mut self,
        writer: Guid,
        fragment: &DataFrag<'_>,
    ) -> BehaviorResult<Option<Vec<u8>>> {
        let sequence_number = fragment.writer_sn;
        let key = (writer, sequence_number);
        let plan = FragmentPlan::from_declared(
            fragment.sample_size,
            fragment.fragment_size,
            writer,
            sequence_number,
        )?;

        if let Some(existing) = self.assemblies.get(&key) {
            let held = existing.plan();
            if held.sample_size() != plan.sample_size() {
                return Err(BehaviorError::Reassembly {
                    writer,
                    sequence_number,
                    defect: ReassemblyDefect::SampleSizeChanged {
                        first: held.sample_size(),
                        then: plan.sample_size(),
                    },
                });
            }
            if held.fragment_size() != plan.fragment_size() {
                return Err(BehaviorError::Reassembly {
                    writer,
                    sequence_number,
                    defect: ReassemblyDefect::FragmentSizeChanged {
                        first: held.fragment_size(),
                        then: plan.fragment_size(),
                    },
                });
            }
        } else {
            self.evict_if_needed(writer);
            self.assemblies.insert(key, Assembly::new(plan));
        }

        let start = fragment.fragment_starting_num.value();
        let count = fragment.fragments_in_submessage;
        let window = plan
            .window_of(start, count)
            .map_err(|_| BehaviorError::Reassembly {
                writer,
                sequence_number,
                defect: ReassemblyDefect::WindowPastSample {
                    end: u64::from(fragment.fragment_end().saturating_sub(1))
                        * u64::from(fragment.fragment_size),
                    sample_size: fragment.sample_size,
                },
            })?;

        let octets = fragment.payload.as_slice();
        let expected = window.len();
        if octets.len() < expected {
            return Err(BehaviorError::Wire(crate::error::RtpsError::truncated(
                "DATA_FRAG serializedPayload",
                expected,
                octets.len(),
            )));
        }
        let source = octets.get(..expected).unwrap_or(&[]);

        let Some(assembly) = self.assemblies.get_mut(&key) else {
            return Ok(None);
        };
        // A fragment that re-arrives must carry the same octets. Silently
        // accepting different ones would let a spoofed datagram rewrite a
        // sample that is already half-assembled.
        let already_have_all = (0..u32::from(count))
            .map(|offset| start.saturating_add(offset))
            .all(|number| assembly.has(number));
        if let Some(slot) = assembly.buffer.get_mut(window.clone()) {
            if already_have_all && slot != source {
                return Err(BehaviorError::Reassembly {
                    writer,
                    sequence_number,
                    defect: ReassemblyDefect::Contradiction { fragment: start },
                });
            }
            slot.copy_from_slice(source);
        }
        for offset in 0..u32::from(count) {
            assembly.mark(start.saturating_add(offset));
        }

        if assembly.is_complete() {
            let finished = self.assemblies.remove(&key).and_then(Assembly::finish);
            return Ok(finished);
        }
        Ok(None)
    }

    /// The fragments still missing from one sample, as a `NACK_FRAG` set.
    #[must_use]
    pub fn missing(
        &self,
        writer: Guid,
        sequence_number: SequenceNumber,
    ) -> Option<FragmentNumberSet> {
        self.assemblies
            .get(&(writer, sequence_number))
            .and_then(Assembly::missing)
    }

    /// The part-assembled sample, if there is one.
    #[must_use]
    pub fn assembly(&self, writer: Guid, sequence_number: SequenceNumber) -> Option<&Assembly> {
        self.assemblies.get(&(writer, sequence_number))
    }

    /// Abandon one part-assembled sample — the writer GAPped it.
    pub fn discard(&mut self, writer: Guid, sequence_number: SequenceNumber) -> bool {
        self.assemblies.remove(&(writer, sequence_number)).is_some()
    }

    /// Abandon everything from one writer — it went away.
    pub fn discard_writer(&mut self, writer: Guid) -> usize {
        let doomed: Vec<(Guid, SequenceNumber)> = self
            .assemblies
            .keys()
            .filter(|(held, _)| *held == writer)
            .copied()
            .collect();
        for key in &doomed {
            self.assemblies.remove(key);
        }
        doomed.len()
    }

    /// Every sequence number this writer has part-assembled, ascending.
    pub fn in_flight(&self, writer: Guid) -> impl Iterator<Item = SequenceNumber> + '_ {
        self.assemblies
            .keys()
            .filter(move |(held, _)| *held == writer)
            .map(|(_, number)| *number)
    }

    /// Drop the oldest in-flight sample of `writer` when the ceiling is
    /// reached.
    ///
    /// A peer that starts a thousand samples and finishes none must not be
    /// able to grow this map without bound.
    fn evict_if_needed(&mut self, writer: Guid) {
        let held: Vec<SequenceNumber> = self.in_flight(writer).collect();
        if held.len() < self.per_writer_limit {
            return;
        }
        if let Some(oldest) = held.first() {
            self.assemblies.remove(&(writer, *oldest));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{ENTITYID_PARTICIPANT, EntityKind, GuidPrefix, VendorId};

    fn writer_guid() -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [1; 10]),
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    fn other_writer() -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [2; 10]),
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    #[test]
    fn an_exact_multiple_produces_no_short_fragment() {
        let plan = FragmentPlan::new(4_000, 1_000).unwrap();
        assert_eq!(plan.total_fragments(), 4);
        for number in 1..=4 {
            assert_eq!(plan.fragment_len(number).unwrap(), 1_000);
        }
    }

    #[test]
    fn a_remainder_lands_entirely_in_the_last_fragment() {
        let plan = FragmentPlan::new(4_001, 1_000).unwrap();
        assert_eq!(plan.total_fragments(), 5);
        assert_eq!(plan.fragment_len(4).unwrap(), 1_000);
        assert_eq!(plan.fragment_len(5).unwrap(), 1);
        assert_eq!(plan.window(5).unwrap(), 4_000..4_001);
    }

    #[test]
    fn fragment_numbers_are_one_based() {
        let plan = FragmentPlan::new(100, 10).unwrap();
        assert_eq!(plan.window(1).unwrap(), 0..10);
        assert_eq!(plan.window(10).unwrap(), 90..100);
        assert!(plan.window(0).is_err(), "fragment 0 does not exist");
        assert!(plan.window(11).is_err(), "there is no eleventh fragment");
    }

    #[test]
    fn a_sample_smaller_than_one_fragment_is_one_fragment() {
        let plan = FragmentPlan::new(5, 1_000).unwrap();
        assert_eq!(plan.total_fragments(), 1);
        assert_eq!(plan.window(1).unwrap(), 0..5);
        assert!(!plan.is_needed());
    }

    #[test]
    fn a_declared_sample_above_the_ceiling_is_refused() {
        let error = FragmentPlan::from_declared(
            MAX_REASSEMBLY_SAMPLE + 1,
            1_000,
            writer_guid(),
            SequenceNumber::FIRST,
        )
        .expect_err("must refuse");
        assert!(matches!(error, BehaviorError::SampleTooLarge { .. }));
    }

    #[test]
    fn a_fragment_size_of_one_on_a_large_sample_is_refused() {
        let error = FragmentPlan::from_declared(
            MAX_FRAGMENTS + 1,
            1,
            writer_guid(),
            SequenceNumber::new(4),
        )
        .expect_err("must refuse");
        assert!(matches!(
            error,
            BehaviorError::Reassembly {
                defect: ReassemblyDefect::TooManyFragments { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_zero_fragment_size_is_refused() {
        assert!(FragmentPlan::new(100, 0).is_err());
    }

    #[test]
    fn a_datagram_holds_as_many_fragments_as_it_has_room_for() {
        let plan = FragmentPlan::new(1_048_576, 1_000).unwrap();
        // 1400 - 20 header - 4 submessage header - 32 prelude = 1344 → 1.
        assert_eq!(plan.fragments_per_datagram(1_400), 1);
        // 65507 leaves room for 65 of them.
        assert!(plan.fragments_per_datagram(65_507) >= 60);
        // Even an absurdly small budget sends one.
        assert_eq!(plan.fragments_per_datagram(8), 1);
    }

    #[test]
    fn a_megabyte_round_trips_through_fragmentation_and_reassembly() {
        let payload = sample(1_048_576);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::FIRST,
            &payload,
            DEFAULT_FRAGMENT_SIZE,
            1_400,
        )
        .unwrap();
        assert_eq!(submessages.len(), 781, "1 MiB at 1344 octets per fragment");

        let mut reassembler = Reassembler::new();
        let mut finished = None;
        for fragment in &submessages {
            fragment.validate().expect("each fragment must be valid");
            if let Some(sample) = reassembler.accept(writer_guid(), fragment).unwrap() {
                finished = Some(sample);
            }
        }
        assert_eq!(finished.as_deref(), Some(payload.as_slice()));
        assert!(
            reassembler.is_empty(),
            "the assembly is released on completion"
        );
    }

    #[test]
    fn fragments_may_arrive_in_any_order() {
        let payload = sample(10_000);
        let mut submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::new(3),
            &payload,
            1_000,
            1_400,
        )
        .unwrap();
        submessages.reverse();

        let mut reassembler = Reassembler::new();
        let mut finished = None;
        for fragment in &submessages {
            if let Some(sample) = reassembler.accept(writer_guid(), fragment).unwrap() {
                finished = Some(sample);
            }
        }
        assert_eq!(finished.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn a_missing_fragment_leaves_the_sample_incomplete_and_nameable() {
        let payload = sample(10_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::new(2),
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut reassembler = Reassembler::new();
        for (index, fragment) in submessages.iter().enumerate() {
            if index == 3 {
                continue;
            }
            assert!(
                reassembler
                    .accept(writer_guid(), fragment)
                    .unwrap()
                    .is_none()
            );
        }

        let missing = reassembler
            .missing(writer_guid(), SequenceNumber::new(2))
            .expect("one fragment is missing");
        assert_eq!(missing.base(), FragmentNumber::new(4));
        assert_eq!(missing.len(), 1);
        assert!(missing.contains(FragmentNumber::new(4)));

        // …and delivering it completes the sample.
        let finished = reassembler
            .accept(writer_guid(), &submessages[3])
            .unwrap()
            .expect("now complete");
        assert_eq!(finished, payload);
    }

    #[test]
    fn a_changed_sample_size_mid_series_is_rejected() {
        let payload = sample(3_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut reassembler = Reassembler::new();
        reassembler.accept(writer_guid(), &submessages[0]).unwrap();

        let mut liar = submessages[1].clone();
        liar.sample_size = 9_000;
        let error = reassembler
            .accept(writer_guid(), &liar)
            .expect_err("must reject");
        assert!(matches!(
            error,
            BehaviorError::Reassembly {
                defect: ReassemblyDefect::SampleSizeChanged {
                    first: 3_000,
                    then: 9_000
                },
                ..
            }
        ));
    }

    #[test]
    fn a_changed_fragment_size_mid_series_is_rejected() {
        let payload = sample(3_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut reassembler = Reassembler::new();
        reassembler.accept(writer_guid(), &submessages[0]).unwrap();

        let mut liar = submessages[1].clone();
        liar.fragment_size = 500;
        let error = reassembler
            .accept(writer_guid(), &liar)
            .expect_err("must reject");
        assert!(matches!(
            error,
            BehaviorError::Reassembly {
                defect: ReassemblyDefect::FragmentSizeChanged { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_resent_fragment_with_different_octets_is_rejected() {
        let payload = sample(3_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut reassembler = Reassembler::new();
        reassembler.accept(writer_guid(), &submessages[0]).unwrap();

        let mut forged = submessages[0].clone();
        forged.payload = SerializedPayload::new(vec![0xff_u8; 1_000]);
        let error = reassembler
            .accept(writer_guid(), &forged)
            .expect_err("must reject");
        assert!(matches!(
            error,
            BehaviorError::Reassembly {
                defect: ReassemblyDefect::Contradiction { fragment: 1 },
                ..
            }
        ));
    }

    #[test]
    fn a_resent_fragment_with_the_same_octets_is_idempotent() {
        let payload = sample(3_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut reassembler = Reassembler::new();
        reassembler.accept(writer_guid(), &submessages[0]).unwrap();
        reassembler.accept(writer_guid(), &submessages[0]).unwrap();
        let assembly = reassembler
            .assembly(writer_guid(), SequenceNumber::FIRST)
            .expect("still in flight");
        assert_eq!(assembly.received_count(), 1);
    }

    #[test]
    fn a_truncated_payload_is_a_wire_error() {
        let payload = sample(3_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut short = submessages[0].clone();
        short.payload = SerializedPayload::new(vec![0_u8; 10]);
        let mut reassembler = Reassembler::new();
        let error = reassembler
            .accept(writer_guid(), &short)
            .expect_err("must reject");
        assert!(matches!(error, BehaviorError::Wire(_)));
    }

    #[test]
    fn two_writers_may_send_sample_one_at_the_same_time() {
        let payload = sample(3_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::FIRST,
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut reassembler = Reassembler::new();
        reassembler.accept(writer_guid(), &submessages[0]).unwrap();
        reassembler.accept(other_writer(), &submessages[0]).unwrap();
        assert_eq!(reassembler.len(), 2, "the two must not share an assembly");

        assert_eq!(reassembler.discard_writer(writer_guid()), 1);
        assert_eq!(reassembler.len(), 1);
    }

    #[test]
    fn a_gapped_sample_can_be_discarded() {
        let payload = sample(3_000);
        let submessages = fragment_sample(
            EntityId::UNKNOWN,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            SequenceNumber::new(11),
            &payload,
            1_000,
            1_400,
        )
        .unwrap();

        let mut reassembler = Reassembler::new();
        reassembler.accept(writer_guid(), &submessages[0]).unwrap();
        assert_eq!(
            reassembler.in_flight(writer_guid()).collect::<Vec<_>>(),
            vec![SequenceNumber::new(11)]
        );
        assert!(reassembler.discard(writer_guid(), SequenceNumber::new(11)));
        assert!(!reassembler.discard(writer_guid(), SequenceNumber::new(11)));
    }

    #[test]
    fn a_writer_cannot_grow_the_reassembler_without_bound() {
        let payload = sample(3_000);
        let mut reassembler = Reassembler::new().with_per_writer_limit(4);
        for number in 1..=20_i64 {
            let submessages = fragment_sample(
                EntityId::UNKNOWN,
                EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
                SequenceNumber::new(number),
                &payload,
                1_000,
                1_400,
            )
            .unwrap();
            reassembler.accept(writer_guid(), &submessages[0]).unwrap();
        }
        assert!(
            reassembler.len() <= 4,
            "held {} assemblies",
            reassembler.len()
        );
    }

    #[test]
    fn the_missing_set_never_exceeds_the_bitmap_width() {
        let plan = FragmentPlan::new(1_000_000, 100).unwrap();
        assert_eq!(plan.total_fragments(), 10_000);
        let assembly = Assembly::new(plan);
        let missing = assembly.missing().expect("everything is missing");
        assert_eq!(missing.base(), FragmentNumber::FIRST);
        assert_eq!(missing.num_bits(), MAX_SET_BITS);
        assert_eq!(missing.len(), MAX_SET_BITS as usize);
    }

    #[test]
    fn a_complete_assembly_reports_nothing_missing() {
        let plan = FragmentPlan::new(300, 100).unwrap();
        let mut assembly = Assembly::new(plan);
        for number in 1..=3 {
            assembly.mark(number);
        }
        assert!(assembly.is_complete());
        assert_eq!(assembly.missing(), None);
        assert_eq!(assembly.finish().map(|sample| sample.len()), Some(300));
    }

    #[test]
    fn marking_out_of_range_is_ignored() {
        let plan = FragmentPlan::new(300, 100).unwrap();
        let mut assembly = Assembly::new(plan);
        assembly.mark(0);
        assembly.mark(99);
        assert_eq!(assembly.received_count(), 0);
        assert!(!assembly.has(0));
        assert!(!assembly.has(99));
    }

    #[test]
    fn an_unknown_guid_participant_entity_is_still_a_valid_key() {
        // Guard against a future refactor keying assemblies on the prefix
        // alone: two endpoints of one participant must not collide.
        let left = Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [5; 10]),
            ENTITYID_PARTICIPANT,
        );
        let right = Guid::new(
            left.prefix,
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        );
        assert_ne!(left, right);
    }
}
