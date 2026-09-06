//! [`MetadataView`] — the seam between a caller's message envelope and the
//! scheduler's eviction-immunity rule (blueprint §11.2).
//!
//! This crate's queues are generic over the message payload `T` precisely so
//! that `astrs-daemon` and `astrs-node-api` can each route their own event
//! envelope type through the same [`crate::InputQueue`] and
//! [`crate::EventMux`] machinery without either crate depending on the
//! other's type, or this crate depending on either. The one thing every
//! caller's envelope must be able to answer is: *does dropping this message
//! wedge a client forever?* [`MetadataView`] is that question, expressed as
//! a trait.

use astrs_wire::{Metadata, NodeEvent};

/// What the scheduler needs to see on a queued message to enforce
/// eviction immunity (blueprint §11.2): messages carrying `request_id`,
/// `goal_id`, or `goal_status` in their [`Metadata`], and Stop-class control
/// events, are never evicted to make room for a later arrival.
///
/// Implement this on whatever envelope type a caller's queue actually
/// stores — typically a small `enum` distinguishing "an ordinary data
/// message with metadata" from "a control signal with none" (a `Stop` has
/// no [`Metadata`] of its own; it is immune by virtue of
/// [`MetadataView::is_stop_signal`], not correlation). [`Envelope`] is a
/// ready-made implementation for the common case of "a payload plus
/// optional metadata" when a caller does not need a richer envelope.
///
/// # Immunity is not priority
///
/// Eviction immunity only protects a message from being *dropped* while it
/// waits. It says nothing about how soon it is *delivered* — that is
/// [`astrs_wire::PriorityLane`]'s job (blueprint §11.3), enforced by
/// [`crate::EventMux`], not by the queue. A `Stop` registered on
/// [`astrs_wire::PriorityLane::Data`] is never dropped, but it still queues
/// behind that lane's ordinary data backlog; a caller that needs a
/// control-class message *seen* promptly, not merely *kept*, must register
/// its input on [`astrs_wire::PriorityLane::Control`].
///
/// # Examples
///
/// ```
/// use astrs_scheduler::MetadataView;
/// use astrs_wire::Metadata;
/// use astrs_time::HlcTimestamp;
///
/// enum MyEvent {
///     Data { metadata: Metadata },
///     Stop,
/// }
///
/// impl MetadataView for MyEvent {
///     fn metadata(&self) -> Option<&Metadata> {
///         match self {
///             MyEvent::Data { metadata } => Some(metadata),
///             MyEvent::Stop => None,
///         }
///     }
///
///     fn is_stop_signal(&self) -> bool {
///         matches!(self, MyEvent::Stop)
///     }
/// }
///
/// assert!(MyEvent::Stop.is_evict_immune());
///
/// let plain = MyEvent::Data { metadata: Metadata::new(HlcTimestamp::EPOCH) };
/// assert!(!plain.is_evict_immune());
///
/// let mut correlated_meta = Metadata::new(HlcTimestamp::EPOCH);
/// correlated_meta.set_request_id("req-1");
/// let correlated = MyEvent::Data { metadata: correlated_meta };
/// assert!(correlated.is_evict_immune());
/// ```
pub trait MetadataView {
    /// The metadata carried alongside this message, if any.
    ///
    /// Pure control signals (a virtual `Stop`, a heartbeat) typically carry
    /// none; [`MetadataView::is_stop_signal`] is how those still get
    /// immunity.
    fn metadata(&self) -> Option<&Metadata>;

    /// Whether this message is a Stop-class control event.
    ///
    /// Stop-class events are immune regardless of metadata (blueprint
    /// §11.2) — a dataflow shutdown signal must never be sacrificed to make
    /// room for a camera frame. The default implementation returns `false`;
    /// override it on whichever envelope variant represents a stop signal
    /// (e.g. the `NodeEvent::Stop` case of a caller's own event enum).
    fn is_stop_signal(&self) -> bool {
        false
    }

    /// Whether this message must never be evicted from a bounded queue.
    ///
    /// True for a Stop-class event, or for a message whose metadata is
    /// [correlated](Metadata::is_correlated) (`request_id`, `goal_id`, or
    /// `goal_status` present). This is the exact predicate
    /// [`crate::InputQueue::push`] consults.
    fn is_evict_immune(&self) -> bool {
        self.is_stop_signal() || self.metadata().is_some_and(Metadata::is_correlated)
    }
}

/// A ready-made [`MetadataView`] for the common case: a payload paired with
/// optional metadata, and an explicit stop flag.
///
/// Callers with a richer event enum of their own (the usual case for
/// `astrs-daemon` and `astrs-node-api`, whose wire-level `NodeEvent` already
/// distinguishes `Input`/`Stop`/etc.) should implement [`MetadataView`]
/// directly on that type instead — `Envelope` exists for tests, examples,
/// and any caller that does not already have such a type.
///
/// # Examples
///
/// ```
/// use astrs_scheduler::{Envelope, MetadataView};
///
/// let stop = Envelope::<()>::stop_signal();
/// assert!(stop.is_evict_immune());
///
/// let data = Envelope::new(42);
/// assert!(!data.is_evict_immune());
/// assert_eq!(data.payload, 42);
/// ```
// No `Eq`: `Metadata` carries `Parameter` values that may hold an `f64`
// (see `Parameter`'s own docs on why it stops at `PartialEq`), so `Envelope`
// follows suit.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope<T> {
    /// The wrapped message payload.
    pub payload: T,
    /// Metadata accompanying the payload, if any.
    pub metadata: Option<Metadata>,
    /// Whether this envelope represents a Stop-class control event rather
    /// than an ordinary payload delivery.
    pub stop: bool,
}

impl<T> Envelope<T> {
    /// Wraps `payload` with no metadata and no stop flag.
    #[must_use]
    pub fn new(payload: T) -> Self {
        Self {
            payload,
            metadata: None,
            stop: false,
        }
    }

    /// Wraps `payload` together with `metadata`.
    #[must_use]
    pub fn with_metadata(payload: T, metadata: Metadata) -> Self {
        Self {
            payload,
            metadata: Some(metadata),
            stop: false,
        }
    }

    /// Unwraps the payload, discarding metadata and the stop flag.
    #[must_use]
    pub fn into_payload(self) -> T {
        self.payload
    }
}

impl Envelope<()> {
    /// A Stop-class control envelope carrying no payload.
    ///
    /// The `()` payload type is a convenience for the common case of a pure
    /// signal; use [`Envelope { payload, stop: true, .. }`](Envelope) directly
    /// to attach a real payload (e.g. a `StopCause`) to a stop signal.
    #[must_use]
    pub fn stop_signal() -> Self {
        Self {
            payload: (),
            metadata: None,
            stop: true,
        }
    }
}

impl<T> MetadataView for Envelope<T> {
    fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    fn is_stop_signal(&self) -> bool {
        self.stop
    }
}

/// [`MetadataView`] for `astrs-wire`'s own daemon→node event enum — the
/// real event type `astrs-node-api` (this crate's intended consumer, per
/// the crate docs) delivers to a node's merged event loop, not a stand-in.
///
/// Only [`NodeEvent::Input`] carries [`Metadata`] at all, so it is the only
/// variant [`MetadataView::metadata`] returns `Some` for; every other
/// variant's immunity (if any) therefore has to come from
/// [`MetadataView::is_stop_signal`] instead, which this impl maps onto
/// [`NodeEvent::is_terminal`] — `Stop` and `AllInputsClosed`. Blueprint
/// §11.2 names `Stop` explicitly; `AllInputsClosed` is included alongside
/// it because it is the *other* signal that tells a node its working life
/// is over (blueprint §12's wind-down path runs through both), and losing
/// either to eviction produces the identical failure mode the blueprint is
/// guarding against: a node that keeps running past the point it should
/// have wound down. Every other control-ish variant (`InputClosed`,
/// `NodeFailed`, `ParamUpdate`, …) is important but not itself a
/// wind-down-or-correlated signal, so it is immune only when — like any
/// other message — it happens to be correlated, which for these variants
/// it structurally cannot be (none of them carry [`Metadata`]).
///
/// # Examples
///
/// ```
/// use astrs_scheduler::MetadataView;
/// use astrs_wire::{Metadata, NodeEvent, StopCause};
/// use astrs_time::HlcTimestamp;
///
/// let stop = NodeEvent::Stop { cause: StopCause::Requested, grace: None };
/// assert!(stop.is_evict_immune(), "a Stop-class event is always immune");
///
/// let mut correlated = Metadata::new(HlcTimestamp::EPOCH);
/// correlated.set_request_id("req-1");
/// let response = NodeEvent::Input {
///     id: "replies".parse()?,
///     source: "server/replies".parse()?,
///     metadata: correlated,
///     payload: Vec::new(),
/// };
/// assert!(response.is_evict_immune(), "a correlated Input is immune");
///
/// let frame = NodeEvent::Input {
///     id: "frames".parse()?,
///     source: "camera/image".parse()?,
///     metadata: Metadata::default(),
///     payload: vec![1, 2, 3],
/// };
/// assert!(!frame.is_evict_immune(), "an ordinary, uncorrelated frame is not");
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
impl MetadataView for NodeEvent {
    fn metadata(&self) -> Option<&Metadata> {
        match self {
            Self::Input { metadata, .. } => Some(metadata),
            _ => None,
        }
    }

    fn is_stop_signal(&self) -> bool {
        self.is_terminal()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    #[test]
    fn plain_payload_is_not_immune() {
        let env = Envelope::new("frame");
        assert!(!env.is_evict_immune());
        assert!(!env.is_stop_signal());
        assert!(env.metadata().is_none());
    }

    #[test]
    fn stop_signal_is_always_immune() {
        let stop = Envelope::<()>::stop_signal();
        assert!(stop.is_stop_signal());
        assert!(stop.is_evict_immune());
        assert!(
            stop.metadata().is_none(),
            "immune via the stop flag, not metadata"
        );
    }

    #[test]
    fn correlated_metadata_makes_a_non_stop_message_immune() {
        let mut meta = Metadata::new(HlcTimestamp::EPOCH);
        meta.set_goal_id("goal-1");
        let env = Envelope::with_metadata(7u32, meta);
        assert!(!env.is_stop_signal());
        assert!(env.is_evict_immune());
    }

    #[test]
    fn uncorrelated_metadata_does_not_grant_immunity() {
        let meta = Metadata::new(HlcTimestamp::EPOCH);
        let env = Envelope::with_metadata(7u32, meta);
        assert!(!env.is_evict_immune());
    }

    #[test]
    fn every_correlation_key_grants_immunity_through_the_default_impl() {
        for key in astrs_wire::keys::CORRELATION {
            let mut meta = Metadata::new(HlcTimestamp::EPOCH);
            meta.insert(key, astrs_wire::Parameter::String("x".into()))
                .unwrap();
            let env = Envelope::with_metadata((), meta);
            assert!(env.is_evict_immune(), "{key} must grant immunity");
        }
    }

    #[test]
    fn stream_keys_alone_do_not_grant_immunity() {
        for key in astrs_wire::keys::STREAM {
            let mut meta = Metadata::new(HlcTimestamp::EPOCH);
            meta.insert(key, astrs_wire::Parameter::Integer(1)).unwrap();
            let env = Envelope::with_metadata((), meta);
            assert!(
                !env.is_evict_immune(),
                "{key} alone must not grant immunity"
            );
        }
    }

    #[test]
    fn into_payload_discards_metadata_and_stop_flag() {
        let env = Envelope::with_metadata(99, Metadata::new(HlcTimestamp::EPOCH));
        assert_eq!(env.into_payload(), 99);
    }

    // -- `MetadataView for astrs_wire::NodeEvent`: the real consumer type,
    // not the crate's own `Envelope` stand-in. --

    fn node_event_input(metadata: Metadata) -> astrs_wire::NodeEvent {
        astrs_wire::NodeEvent::Input {
            id: astrs_wire::DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata,
            payload: vec![1, 2, 3],
        }
    }

    #[test]
    fn node_event_input_exposes_its_metadata() {
        let mut meta = Metadata::new(HlcTimestamp::EPOCH);
        meta.set_seq(1);
        let event = node_event_input(meta.clone());
        assert_eq!(event.metadata(), Some(&meta));
        assert!(!event.is_stop_signal());
    }

    #[test]
    fn node_event_input_with_correlated_metadata_is_immune() {
        let mut meta = Metadata::new(HlcTimestamp::EPOCH);
        meta.set_goal_id("goal-1");
        assert!(node_event_input(meta).is_evict_immune());
    }

    #[test]
    fn node_event_input_with_plain_metadata_is_not_immune() {
        assert!(!node_event_input(Metadata::default()).is_evict_immune());
    }

    #[test]
    fn node_event_stop_is_always_immune_and_carries_no_metadata() {
        let stop = astrs_wire::NodeEvent::Stop {
            cause: astrs_wire::StopCause::Requested,
            grace: None,
        };
        assert!(stop.is_stop_signal());
        assert!(stop.is_evict_immune());
        assert!(stop.metadata().is_none());
    }

    #[test]
    fn node_event_all_inputs_closed_is_immune_alongside_stop() {
        assert!(astrs_wire::NodeEvent::AllInputsClosed.is_stop_signal());
        assert!(astrs_wire::NodeEvent::AllInputsClosed.is_evict_immune());
    }

    #[test]
    fn node_event_variants_without_metadata_or_termination_are_not_immune() {
        let reload = astrs_wire::NodeEvent::Reload {
            operator: None,
            path: None,
        };
        assert!(!reload.is_stop_signal());
        assert!(reload.metadata().is_none());
        assert!(!reload.is_evict_immune());
    }

    #[test]
    fn is_stop_signal_matches_node_event_is_terminal_exactly() {
        // `MetadataView::is_stop_signal` for `NodeEvent` is defined *as*
        // `is_terminal()`; assert that mapping directly so a future change
        // to either side shows up here rather than only in the two
        // variant-specific tests above.
        use astrs_wire::messages::samples::node_events;
        for sample in node_events().unwrap() {
            assert_eq!(sample.is_stop_signal(), sample.is_terminal(), "{sample:?}");
        }
    }
}
