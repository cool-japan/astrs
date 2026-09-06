//! [`EventStream`] — the node's inbox, in four flavours.
//!
//! Blueprint §9.1: *"`EventStream` implements `Iterator` + `Stream` and can
//! merge external streams. After `Stop` the stream fuses."* All four ways of
//! reading it — [`recv`](EventStream::recv), [`recv_async`](EventStream::recv_async),
//! [`Iterator`] and [`Stream`] — are the same queue seen through different
//! doors, so a node can switch styles without changing its wiring.
//!
//! # Which door to use
//!
//! | Call | Blocks | Use when |
//! |---|---|---|
//! | [`recv`](EventStream::recv) | Yes | An ordinary synchronous node loop |
//! | [`recv_timeout`](EventStream::recv_timeout) | Yes, bounded | A loop that must do periodic work |
//! | [`try_recv`](EventStream::try_recv) | No | A node polling its own event loop |
//! | [`recv_async`](EventStream::recv_async) | No (awaits) | Inside a tokio task |
//!
//! The blocking doors return [`NodeError::BlockingInAsync`] rather than
//! deadlocking when called from a current-thread runtime — see
//! [`crate::runtime`].
//!
//! # A local `Stream`
//!
//! This crate defines its own [`Stream`] trait rather than depending on
//! `futures-core`, which is not on blueprint §18.1's retained list. It is
//! *signature-identical* to `futures_core::Stream`, so a caller who does have
//! that crate can write a two-line forwarding adapter — and this crate stays
//! dependency-clean.
//!
//! # Fusing
//!
//! [`Event::Stop`] is delivered once and sets the fuse; every later read
//! returns `None`. [`EventStream::is_fused`] reports it, and
//! [`EventStream::stop_cause`] remembers why.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;
use std::time::Duration;

use astrs_wire::StopCause;
use tokio::sync::mpsc::Receiver;

use crate::error::{NodeError, Result};
use crate::events::Event;
use crate::events::source::{EventSource, StreamStats};
use crate::runtime::NodeRuntime;
use crate::signal::WaitOutcome;

/// An asynchronous sequence of values.
///
/// Signature-identical to `futures_core::Stream`; see the module docs for why
/// this crate declares its own.
pub trait Stream {
    /// The values the stream yields.
    type Item;

    /// Attempts to pull out the next value, registering the current task for
    /// wakeup if none is ready.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>;

    /// A size hint, in the [`Iterator::size_hint`] shape.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

/// The node's event inbox (blueprint §9.1).
#[derive(Debug)]
pub struct EventStream {
    /// The shared inbox the session's reader task pushes into.
    source: Arc<EventSource>,
    /// The runtime background work (merged external streams) runs on.
    runtime: NodeRuntime,
    /// Set once [`Event::Stop`] has been delivered.
    fused: bool,
    /// Why the stream fused.
    stop_cause: Option<StopCause>,
    /// Set once a blocking read was refused because this is a current-thread
    /// runtime. Latched: reporting it once is diagnosis, reporting it every
    /// call would turn `while let Some(event) = events.recv()` into an
    /// allocating infinite loop.
    blocking_refused: bool,
}

impl EventStream {
    /// A stream over `source`.
    #[must_use]
    pub fn new(source: Arc<EventSource>, runtime: NodeRuntime) -> Self {
        Self {
            source,
            runtime,
            fused: false,
            stop_cause: None,
            blocking_refused: false,
        }
    }

    /// The shared inbox, for a caller that wants to feed it directly (the
    /// testing harness does).
    #[must_use]
    pub fn source(&self) -> &Arc<EventSource> {
        &self.source
    }

    /// Whether the stream has fused.
    #[must_use]
    pub const fn is_fused(&self) -> bool {
        self.fused
    }

    /// Why the stream fused, once it has.
    #[must_use]
    pub fn stop_cause(&self) -> Option<StopCause> {
        self.stop_cause.clone()
    }

    /// A snapshot of the stream's counters.
    #[must_use]
    pub fn stats(&self) -> StreamStats {
        self.source.stats()
    }

    /// The next event, or `None` if none is ready right now.
    ///
    /// Never blocks, and never fuses on emptiness — only on [`Event::Stop`].
    pub fn try_recv(&mut self) -> Option<Event> {
        if self.fused {
            return None;
        }
        let event = self.source.try_next()?;
        Some(self.observe(event))
    }

    /// Blocks until the next event arrives, the session ends, or the stream
    /// fuses.
    ///
    /// # Panics
    ///
    /// Never. A blocking call from a current-thread runtime reports one
    /// [`Event::Error`] and then ends the stream, rather than deadlocking or
    /// spinning; use [`EventStream::recv_checked`] to see the typed error
    /// instead, or [`EventStream::recv_async`] to do the right thing.
    #[must_use]
    pub fn recv(&mut self) -> Option<Event> {
        if self.blocking_refused {
            // Latched: a loop of `recv()` on a current-thread runtime must
            // terminate, not report the same misuse forever.
            return None;
        }
        match self.recv_checked() {
            Ok(event) => event,
            Err(error) => {
                self.blocking_refused = true;
                Some(Event::Error(error.to_string()))
            }
        }
    }

    /// Whether a blocking read was refused because this is a current-thread
    /// runtime (see [`EventStream::recv`]).
    #[must_use]
    pub const fn blocking_refused(&self) -> bool {
        self.blocking_refused
    }

    /// [`EventStream::recv`], reporting a misuse instead of swallowing it.
    ///
    /// # Errors
    ///
    /// [`NodeError::BlockingInAsync`] when called from a current-thread
    /// runtime's only worker.
    pub fn recv_checked(&mut self) -> Result<Option<Event>> {
        self.recv_blocking(None)
    }

    /// Blocks for at most `timeout`.
    ///
    /// Returns `Ok(None)` both when the timeout expires and when the stream
    /// has ended; [`EventStream::is_fused`] and
    /// [`EventStream::session_ended`] tell the two apart.
    ///
    /// # Errors
    ///
    /// As [`EventStream::recv_checked`].
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Event>> {
        self.recv_blocking(Some(timeout))
    }

    /// Awaits the next event.
    ///
    /// The async twin of [`EventStream::recv`], and the one to use inside a
    /// tokio task.
    pub async fn recv_async(&mut self) -> Option<Event> {
        loop {
            if self.fused {
                return None;
            }
            // Create the wakeup future *before* checking the queue, so an
            // event pushed in between cannot be missed.
            let notified = self.source.signal().notified();
            let ready = self.source.try_next();
            if let Some(event) = ready {
                drop(notified);
                return Some(self.observe(event));
            }
            if self.source.is_closed() {
                return None;
            }
            notified.await;
        }
    }

    /// Awaits the next event, giving up after `timeout`.
    pub async fn recv_async_timeout(&mut self, timeout: Duration) -> Option<Event> {
        tokio::time::timeout(timeout, self.recv_async())
            .await
            .unwrap_or(None)
    }

    /// Whether the underlying session has ended.
    #[must_use]
    pub fn session_ended(&self) -> bool {
        self.source.is_closed()
    }

    /// Merges an external event source into this stream.
    ///
    /// Everything the receiver yields is delivered on the **control lane**,
    /// so a user's own timer or sensor thread is never queued behind a
    /// backlog of frames. The forwarder stops when the sender is dropped or
    /// the session ends.
    ///
    /// # Examples
    ///
    /// ```
    /// # use astrs_node_api::prelude::*;
    /// # use astrs_node_api::events::EventSource;
    /// # use astrs_node_api::runtime::NodeRuntime;
    /// # use std::sync::Arc;
    /// # fn main() -> Result<(), NodeError> {
    /// let runtime = NodeRuntime::owned()?;
    /// let mut events = EventStream::new(Arc::new(EventSource::new()), runtime);
    /// let sender = events.merge_external_channel(8);
    /// sender.blocking_send(Event::Error("from my own thread".to_owned())).ok();
    ///
    /// let event = events.recv_timeout(std::time::Duration::from_secs(5))?;
    /// assert!(matches!(event, Some(Event::Error(_))));
    /// # Ok(())
    /// # }
    /// ```
    pub fn merge_external(&mut self, mut receiver: Receiver<Event>) {
        let source = Arc::clone(&self.source);
        let _task = self.runtime.spawn(async move {
            while let Some(event) = receiver.recv().await {
                if source.is_closed() {
                    break;
                }
                source.push_control(event);
            }
        });
    }

    /// Merges a freshly created channel and hands back its sender.
    ///
    /// The convenience form of [`EventStream::merge_external`] for the common
    /// case where the caller has no channel yet.
    #[must_use]
    pub fn merge_external_channel(&mut self, capacity: usize) -> tokio::sync::mpsc::Sender<Event> {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity.max(1));
        self.merge_external(receiver);
        sender
    }

    /// The blocking read, with an optional deadline.
    fn recv_blocking(&mut self, timeout: Option<Duration>) -> Result<Option<Event>> {
        let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
        loop {
            if self.fused {
                return Ok(None);
            }
            // Take the ticket *before* looking, so a push that lands between
            // the look and the wait is not missed.
            let ticket = self.source.signal().ticket();
            if let Some(event) = self.source.try_next() {
                return Ok(Some(self.observe(event)));
            }
            if self.source.is_closed() {
                return Ok(None);
            }
            if !self.runtime.can_block() {
                return Err(NodeError::BlockingInAsync {
                    method: "EventStream::recv",
                    alternative: "EventStream::recv_async",
                });
            }
            let remaining = match deadline {
                None => None,
                Some(deadline) => {
                    match deadline.checked_duration_since(std::time::Instant::now()) {
                        Some(remaining) => Some(remaining),
                        None => return Ok(None),
                    }
                }
            };
            match self.source.signal().wait_blocking(ticket, remaining) {
                WaitOutcome::Signalled => {}
                WaitOutcome::TimedOut => return Ok(None),
                WaitOutcome::Closed => {
                    // Drain whatever landed before the close.
                    return Ok(self.source.try_next().map(|event| self.observe(event)));
                }
            }
        }
    }

    /// Records the fuse when a [`Event::Stop`] passes through.
    fn observe(&mut self, event: Event) -> Event {
        if let Event::Stop(cause) = &event {
            self.fused = true;
            self.stop_cause = Some(cause.clone());
        }
        event
    }
}

impl Drop for EventStream {
    /// Tells the session the node stopped reading (§7.3
    /// `EventStreamDropped`), so the daemon can stop queueing for it.
    fn drop(&mut self) {
        self.source.abandon();
    }
}

impl Iterator for EventStream {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv()
    }
}

impl core::iter::FusedIterator for EventStream {}

impl Stream for EventStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        if this.fused {
            return Poll::Ready(None);
        }
        loop {
            // The documented `Notify` pattern: build the wakeup future, then
            // check, then park on it.
            let notified = this.source.signal().notified();
            let ready = this.source.try_next();
            if let Some(event) = ready {
                drop(notified);
                return Poll::Ready(Some(this.observe(event)));
            }
            if this.source.is_closed() {
                return Poll::Ready(None);
            }
            let mut notified = core::pin::pin!(notified);
            match Future::poll(notified.as_mut(), cx) {
                Poll::Pending => return Poll::Pending,
                // Woken already: loop round and look again.
                Poll::Ready(()) => {}
            }
        }
    }
}

use core::future::Future;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::events::QueuedEvent;
    use crate::payload::Payload;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, Metadata, PortRef, PriorityLane, QueuePolicy};

    fn stream() -> (Arc<EventSource>, EventStream) {
        let source = Arc::new(EventSource::new());
        source
            .register_raw(
                DataId::new("frames").unwrap(),
                8,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();
        let runtime = NodeRuntime::owned().expect("runtime");
        let stream = EventStream::new(Arc::clone(&source), runtime);
        (source, stream)
    }

    fn message() -> QueuedEvent {
        QueuedEvent::Input {
            source: PortRef::from_parts("camera", "image").unwrap(),
            metadata: Metadata::new(HlcTimestamp::new(1, 0)),
            payload: Payload::inline(vec![7]),
        }
    }

    #[test]
    fn try_recv_never_blocks() {
        let (source, mut events) = stream();
        assert!(events.try_recv().is_none());
        source.push_input(&DataId::new("frames").unwrap(), message());
        assert!(events.try_recv().unwrap().is_input());
        assert!(events.try_recv().is_none());
    }

    #[test]
    fn recv_blocks_until_an_event_arrives() {
        let (source, mut events) = stream();
        let producer = Arc::clone(&source);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            producer.push_input(&DataId::new("frames").unwrap(), message());
        });
        let event = events.recv().unwrap();
        assert!(event.is_input());
        handle.join().unwrap();
    }

    #[test]
    fn recv_timeout_returns_none_without_ending_the_stream() {
        let (_source, mut events) = stream();
        let event = events.recv_timeout(Duration::from_millis(10)).unwrap();
        assert!(event.is_none());
        assert!(!events.is_fused());
        assert!(!events.session_ended());
    }

    #[test]
    fn the_stream_fuses_after_stop() {
        let (source, mut events) = stream();
        source.push_input(&DataId::new("frames").unwrap(), message());
        source.push_control(Event::Stop(StopCause::Requested));
        source.push_input(&DataId::new("frames").unwrap(), message());

        // Control first, so Stop arrives before the queued frames.
        let first = events.recv().unwrap();
        assert!(first.is_stop());
        assert!(events.is_fused());
        assert_eq!(events.stop_cause(), Some(StopCause::Requested));
        assert!(events.recv().is_none(), "fused");
        assert!(events.try_recv().is_none());
        assert!(events.next().is_none());
    }

    #[test]
    fn closing_the_session_ends_the_stream_after_draining() {
        let (source, mut events) = stream();
        source.push_input(&DataId::new("frames").unwrap(), message());
        source.close();
        assert!(events.recv().unwrap().is_input(), "the tail is drained");
        assert!(events.recv().is_none());
        assert!(events.session_ended());
    }

    #[test]
    fn the_iterator_face_is_the_same_queue() {
        let (source, events) = stream();
        for _ in 0..3 {
            source.push_input(&DataId::new("frames").unwrap(), message());
        }
        source.push_control(Event::Stop(StopCause::Requested));
        let kinds: Vec<&str> = events.map(|event| event.kind_name()).collect();
        assert_eq!(kinds, vec!["stop"], "the fuse stops the iteration");
    }

    #[test]
    fn dropping_the_stream_marks_the_source_abandoned() {
        let (source, events) = stream();
        assert!(!source.is_abandoned());
        drop(events);
        assert!(source.is_abandoned());
    }

    #[test]
    fn stats_track_delivery() {
        let (source, mut events) = stream();
        source.push_input(&DataId::new("frames").unwrap(), message());
        assert_eq!(events.stats().delivered, 0);
        let _ = events.try_recv();
        assert_eq!(events.stats().delivered, 1);
        assert!(std::ptr::eq(
            Arc::as_ptr(events.source()),
            Arc::as_ptr(&source)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_blocking_recv_in_a_current_thread_runtime_is_refused() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let mut events = EventStream::new(source, runtime);
        let error = events.recv_checked().unwrap_err();
        assert!(matches!(error, NodeError::BlockingInAsync { .. }));

        // The lenient face reports it as an event instead of hanging …
        let event = events.recv().unwrap();
        assert!(matches!(event, Event::Error(_)));
        assert!(events.blocking_refused());

        // … exactly once: a `while let Some(event) = events.recv()` loop must
        // terminate rather than spin on the same misuse.
        assert!(events.recv().is_none());
        let mut iterations = 0;
        while events.next().is_some() {
            iterations += 1;
            assert!(iterations < 4, "the loop never terminated");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recv_async_awaits_an_event() {
        let source = Arc::new(EventSource::new());
        source
            .register_raw(
                DataId::new("frames").unwrap(),
                4,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();
        let runtime = NodeRuntime::acquire().unwrap();
        let mut events = EventStream::new(Arc::clone(&source), runtime);

        let producer = Arc::clone(&source);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            producer.push_input(&DataId::new("frames").unwrap(), message());
        });
        let event = events.recv_async().await.unwrap();
        assert!(event.is_input());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recv_async_timeout_gives_up() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let mut events = EventStream::new(source, runtime);
        assert!(
            events
                .recv_async_timeout(Duration::from_millis(10))
                .await
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stream_face_polls_the_same_queue() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let mut events = EventStream::new(Arc::clone(&source), runtime);
        source.push_control(Event::AllInputsClosed);

        let polled = core::future::poll_fn(|cx| {
            let pinned = core::pin::Pin::new(&mut events);
            Stream::poll_next(pinned, cx)
        })
        .await;
        assert!(matches!(polled, Some(Event::AllInputsClosed)));
        assert_eq!(Stream::size_hint(&events), (0, None));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_streams_merge_onto_the_control_lane() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let mut events = EventStream::new(source, runtime);
        let sender = events.merge_external_channel(4);
        sender
            .send(Event::Error("external".to_owned()))
            .await
            .unwrap();
        let event = events.recv_async().await.unwrap();
        let Event::Error(message) = event else {
            panic!("expected the merged event");
        };
        assert_eq!(message, "external");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_merged_receiver_can_be_supplied_directly() {
        let source = Arc::new(EventSource::new());
        let runtime = NodeRuntime::acquire().unwrap();
        let mut events = EventStream::new(source, runtime);
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        events.merge_external(receiver);
        sender.send(Event::AllInputsClosed).await.unwrap();
        assert!(matches!(
            events.recv_async().await,
            Some(Event::AllInputsClosed)
        ));
        drop(sender);
    }
}
