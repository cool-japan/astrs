//! Sample delivery: the queue between a reader and the code awaiting it.
//!
//! [`RtpsReader`](crate::behavior::reader::RtpsReader) is synchronous by
//! design — it takes a submessage and returns a sample, with no runtime
//! involved. Something has to bridge that to an application that wants to
//! `await` the next sample, and this is it: a small queue with a
//! [`Notify`] beside it, filled by the participant's
//! receive loop and drained by whoever holds the [`ReaderHandle`].
//!
//! # Why not a channel
//!
//! An `mpsc::Receiver` would need `&mut self` to receive, which makes a
//! reader handle un-cloneable and un-shareable — and two tasks reading one
//! subscription is an ordinary thing to want. A `Mutex<VecDeque>` plus a
//! `Notify` gives `&self` receive, cloneable handles, and a
//! [`try_take`](ReaderHandle::try_take) that does not need a runtime at all.
//!
//! # Bounded, and honest about it
//!
//! The sink drops the *oldest* sample when it is full and counts what it
//! dropped, exactly as `KEEP_LAST` does. Blocking the receive loop instead
//! would let one slow subscriber stall discovery for the whole participant,
//! which is the failure mode this shape exists to avoid.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration as StdDuration;

use tokio::sync::{Mutex, Notify};

use crate::behavior::endpoint::{Sample, TopicKey};
use crate::structure::Guid;

/// Samples one sink will hold before it starts dropping the oldest.
pub const DEFAULT_SINK_CAPACITY: usize = 1_024;

/// The queue a reader's samples land in.
///
/// Shared between the participant's receive loop and every
/// [`ReaderHandle`] on the subscription.
#[derive(Debug)]
pub struct SampleSink {
    queue: Mutex<VecDeque<Sample>>,
    notify: Notify,
    capacity: usize,
    delivered: AtomicU64,
    dropped: AtomicU64,
}

impl SampleSink {
    /// A sink holding at most `capacity` samples.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            capacity: capacity.max(1),
            delivered: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// How many samples the sink will hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many samples have been pushed in, dropped ones included.
    #[must_use]
    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    /// How many samples were dropped because the sink was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Push a sample, evicting the oldest when full, and wake one waiter.
    pub async fn push(&self, sample: Sample) {
        {
            let mut queue = self.queue.lock().await;
            if queue.len() >= self.capacity {
                queue.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            queue.push_back(sample);
        }
        self.delivered.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_one();
    }

    /// Push several samples and wake once.
    pub async fn extend(&self, samples: impl IntoIterator<Item = Sample>) {
        let mut pushed = 0_u64;
        {
            let mut queue = self.queue.lock().await;
            for sample in samples {
                if queue.len() >= self.capacity {
                    queue.pop_front();
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                queue.push_back(sample);
                pushed = pushed.saturating_add(1);
            }
        }
        if pushed > 0 {
            self.delivered.fetch_add(pushed, Ordering::Relaxed);
            self.notify.notify_waiters();
        }
    }

    /// Take the oldest sample if there is one.
    pub async fn try_take(&self) -> Option<Sample> {
        self.queue.lock().await.pop_front()
    }

    /// Take every sample waiting.
    pub async fn take_all(&self) -> Vec<Sample> {
        self.queue.lock().await.drain(..).collect()
    }

    /// How many samples are waiting.
    pub async fn len(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// True when nothing is waiting.
    pub async fn is_empty(&self) -> bool {
        self.queue.lock().await.is_empty()
    }

    /// Wait until a sample is available, then take it.
    ///
    /// The wait is on a [`Notify`], never on a duration, so a caller wraps
    /// this in [`tokio::time::timeout`] rather than polling.
    pub async fn take(&self) -> Sample {
        loop {
            if let Some(sample) = self.try_take().await {
                return sample;
            }
            self.notify.notified().await;
        }
    }
}

impl Default for SampleSink {
    fn default() -> Self {
        Self::new(DEFAULT_SINK_CAPACITY)
    }
}

/// A subscription: the samples one reader is delivering.
///
/// Cloneable and shareable. Two handles on one subscription split the
/// samples between them rather than each getting a copy, which is what a
/// worker pool wants; an application that needs fan-out creates two readers.
#[derive(Debug, Clone)]
pub struct ReaderHandle {
    guid: Guid,
    topic: TopicKey,
    sink: Arc<SampleSink>,
}

impl ReaderHandle {
    /// Wrap a sink as a subscription handle.
    #[must_use]
    pub const fn new(guid: Guid, topic: TopicKey, sink: Arc<SampleSink>) -> Self {
        Self { guid, topic, sink }
    }

    /// The reader's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.guid
    }

    /// The topic and type it subscribes to.
    #[must_use]
    pub const fn topic(&self) -> &TopicKey {
        &self.topic
    }

    /// The sink behind it.
    #[must_use]
    pub const fn sink(&self) -> &Arc<SampleSink> {
        &self.sink
    }

    /// Take the next sample, waiting for one if necessary.
    pub async fn take(&self) -> Sample {
        self.sink.take().await
    }

    /// Take the next sample, or give up after `timeout`.
    pub async fn take_within(&self, timeout: StdDuration) -> Option<Sample> {
        tokio::time::timeout(timeout, self.sink.take()).await.ok()
    }

    /// Take the next sample if one is already waiting.
    pub async fn try_take(&self) -> Option<Sample> {
        self.sink.try_take().await
    }

    /// Take every sample waiting.
    pub async fn take_all(&self) -> Vec<Sample> {
        self.sink.take_all().await
    }

    /// How many samples are waiting.
    pub async fn len(&self) -> usize {
        self.sink.len().await
    }

    /// True when nothing is waiting.
    pub async fn is_empty(&self) -> bool {
        self.sink.is_empty().await
    }

    /// Wait until at least `count` samples have arrived in total, then take
    /// everything waiting.
    ///
    /// The shape a test wants: "wait for the five samples the writer sent",
    /// without a sleep and without a poll loop. Returns what was taken, which
    /// may be fewer than `count` if the sink evicted.
    pub async fn take_at_least(&self, count: usize) -> Vec<Sample> {
        let mut taken = Vec::with_capacity(count);
        while taken.len() < count {
            taken.push(self.take().await);
        }
        taken.extend(self.take_all().await);
        taken
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::behavior::cache::{ChangeKind, InstanceHandle};
    use crate::structure::{EntityId, EntityKind, GuidPrefix, SequenceNumber, VendorId};
    use std::time::Instant;

    fn guid() -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [1; 10]),
            EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
        )
    }

    fn topic() -> TopicKey {
        TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_").unwrap()
    }

    fn sample(number: i64) -> Sample {
        Sample {
            writer: guid(),
            sequence_number: SequenceNumber::new(number),
            payload: vec![number as u8; 4],
            source_timestamp: None,
            received_at: Instant::now(),
            kind: ChangeKind::Alive,
            instance: InstanceHandle::NIL,
        }
    }

    fn handle(capacity: usize) -> ReaderHandle {
        ReaderHandle::new(guid(), topic(), Arc::new(SampleSink::new(capacity)))
    }

    #[tokio::test]
    async fn a_pushed_sample_can_be_taken() {
        let handle = handle(8);
        assert!(handle.is_empty().await);
        handle.sink().push(sample(1)).await;
        assert_eq!(handle.len().await, 1);
        let taken = handle.take().await;
        assert_eq!(taken.sequence_number, SequenceNumber::FIRST);
        assert!(handle.is_empty().await);
    }

    #[tokio::test]
    async fn taking_waits_for_a_sample_that_has_not_arrived_yet() {
        let handle = handle(8);
        let waiting = handle.clone();
        let task = tokio::spawn(async move { waiting.take().await });
        // The push happens after the waiter is parked; `Notify` wakes it.
        handle.sink().push(sample(7)).await;
        let taken = task.await.expect("the task must not panic");
        assert_eq!(taken.sequence_number, SequenceNumber::new(7));
    }

    #[tokio::test]
    async fn taking_within_a_timeout_gives_up() {
        let handle = handle(8);
        assert!(
            handle
                .take_within(StdDuration::from_millis(10))
                .await
                .is_none()
        );
        handle.sink().push(sample(1)).await;
        assert!(
            handle
                .take_within(StdDuration::from_millis(10))
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_full_sink_drops_the_oldest_and_counts_it() {
        let handle = handle(2);
        for number in 1..=5 {
            handle.sink().push(sample(number)).await;
        }
        assert_eq!(handle.len().await, 2);
        assert_eq!(handle.sink().dropped(), 3);
        assert_eq!(handle.sink().delivered(), 5);
        let remaining = handle.take_all().await;
        assert_eq!(
            remaining
                .iter()
                .map(|sample| sample.sequence_number.value())
                .collect::<Vec<_>>(),
            vec![4, 5],
            "the newest survive"
        );
    }

    #[tokio::test]
    async fn extending_pushes_several_and_wakes_the_waiters() {
        let handle = handle(8);
        let waiting = handle.clone();
        let task = tokio::spawn(async move { waiting.take().await });
        handle
            .sink()
            .extend(vec![sample(1), sample(2), sample(3)])
            .await;
        task.await.expect("the waiter woke");
        assert!(handle.len().await >= 2);
        assert_eq!(handle.sink().delivered(), 3);
    }

    #[tokio::test]
    async fn take_at_least_waits_for_the_count() {
        let handle = handle(16);
        let waiting = handle.clone();
        let task = tokio::spawn(async move { waiting.take_at_least(3).await });
        for number in 1..=3 {
            handle.sink().push(sample(number)).await;
        }
        let taken = task.await.expect("the task must not panic");
        assert!(taken.len() >= 3);
    }

    #[tokio::test]
    async fn two_handles_split_the_samples() {
        let left = handle(8);
        let right = ReaderHandle::new(left.guid(), left.topic().clone(), left.sink().clone());
        left.sink().push(sample(1)).await;
        left.sink().push(sample(2)).await;

        assert!(left.try_take().await.is_some());
        assert!(right.try_take().await.is_some());
        assert!(left.try_take().await.is_none(), "each sample goes once");
    }

    #[tokio::test]
    async fn a_capacity_of_zero_is_raised_to_one() {
        let sink = SampleSink::new(0);
        assert_eq!(sink.capacity(), 1);
        sink.push(sample(1)).await;
        assert_eq!(sink.len().await, 1);
    }

    #[tokio::test]
    async fn the_handle_reports_its_identity() {
        let handle = handle(4);
        assert_eq!(handle.guid(), guid());
        assert_eq!(handle.topic().topic_name, "rt/chatter");
        assert_eq!(SampleSink::default().capacity(), DEFAULT_SINK_CAPACITY);
    }
}
