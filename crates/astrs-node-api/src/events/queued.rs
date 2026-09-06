//! [`QueuedEvent`] — what sits in a per-input queue before delivery.
//!
//! The data lane's three events (`Input`, `InputClosed`, `InputRecovered`)
//! share one queue per input, because their **order relative to each other
//! matters**: a node must see the last frame before it sees "this input is
//! finished". Putting the two closing events on the control lane would let
//! them overtake the frames that preceded them, which is exactly the bug
//! blueprint §11.2's queue discipline exists to avoid.
//!
//! [`QueuedEvent`] implements [`MetadataView`] so
//! [`astrs_scheduler::InputQueue`] can apply the §11.2 rules to it:
//!
//! | Variant | Evictable? |
//! |---|---|
//! | `Input` with a `request_id`/`goal_id`/`goal_status` | No — dropping it wedges a client forever |
//! | `Input` otherwise | Yes, under the input's `queue_policy` |
//! | `Closed`, `Recovered` | No — they are wind-down signals, not data |

use astrs_scheduler::MetadataView;
use astrs_wire::{Metadata, PortRef, RouteCloseReason};

use crate::events::Event;
use crate::payload::Payload;
use astrs_wire::DataId;

/// One queued data-lane event for a single input.
#[derive(Debug)]
pub enum QueuedEvent {
    /// A message.
    Input {
        /// The producer port it came from.
        source: PortRef,
        /// The metadata beside it.
        metadata: Metadata,
        /// The payload.
        payload: Payload,
    },
    /// The input closed.
    Closed {
        /// The producer port that stopped.
        source: PortRef,
        /// Why it closed.
        reason: RouteCloseReason,
    },
    /// The input recovered after its producer restarted.
    Recovered {
        /// The producer port that came back.
        source: PortRef,
        /// The producer's new incarnation.
        generation: u64,
    },
}

impl QueuedEvent {
    /// Turns the queued event into the user-facing [`Event`] for `id`.
    ///
    /// The metadata is stripped of AstRS-internal keys here rather than at
    /// the wire, so the session can read `_schema_hash` while the node never
    /// sees it (§6.1).
    #[must_use]
    pub fn into_event(self, id: DataId) -> Event {
        match self {
            Self::Input {
                mut metadata,
                payload,
                ..
            } => {
                let _stripped = metadata.strip_internal();
                Event::Input {
                    id,
                    meta: metadata,
                    data: payload,
                }
            }
            Self::Closed { source, reason } => Event::InputClosed { id, source, reason },
            Self::Recovered { source, generation } => Event::InputRecovered {
                id,
                source,
                generation,
            },
        }
    }

    /// The producer port this event came from.
    #[must_use]
    pub const fn source(&self) -> &PortRef {
        match self {
            Self::Input { source, .. }
            | Self::Closed { source, .. }
            | Self::Recovered { source, .. } => source,
        }
    }

    /// Whether this event carries data.
    #[must_use]
    pub const fn is_message(&self) -> bool {
        matches!(self, Self::Input { .. })
    }

    /// The payload byte count, for bandwidth accounting.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Input { payload, .. } => payload.len(),
            _ => 0,
        }
    }
}

impl MetadataView for QueuedEvent {
    fn metadata(&self) -> Option<&Metadata> {
        match self {
            Self::Input { metadata, .. } => Some(metadata),
            _ => None,
        }
    }

    /// `Closed` and `Recovered` are wind-down signals, and are immune for the
    /// same reason a `Stop` is: a node that never learns its input ended waits
    /// for a message that will never come.
    fn is_stop_signal(&self) -> bool {
        matches!(self, Self::Closed { .. } | Self::Recovered { .. })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn port() -> PortRef {
        PortRef::from_parts("camera", "image").unwrap()
    }

    fn message(metadata: Metadata) -> QueuedEvent {
        QueuedEvent::Input {
            source: port(),
            metadata,
            payload: Payload::inline(vec![1, 2, 3]),
        }
    }

    #[test]
    fn an_ordinary_message_is_evictable() {
        let event = message(Metadata::new(HlcTimestamp::new(1, 0)));
        assert!(!event.is_evict_immune());
        assert!(event.is_message());
        assert_eq!(event.payload_len(), 3);
        assert_eq!(event.source(), &port());
    }

    #[test]
    fn a_correlated_message_is_immune() {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.set_request_id("req-1");
        assert!(message(metadata).is_evict_immune());

        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.set_goal_id("goal-1");
        assert!(message(metadata).is_evict_immune());
    }

    #[test]
    fn wind_down_signals_are_immune() {
        let closed = QueuedEvent::Closed {
            source: port(),
            reason: RouteCloseReason::ProducerFinished,
        };
        assert!(closed.is_evict_immune());
        assert!(!closed.is_message());
        assert_eq!(closed.payload_len(), 0);

        let recovered = QueuedEvent::Recovered {
            source: port(),
            generation: 3,
        };
        assert!(recovered.is_evict_immune());
        assert!(recovered.metadata().is_none());
    }

    #[test]
    fn conversion_strips_internal_metadata_keys() {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.insert_schema_hash("abc123");
        metadata.set_request_id("req-1");
        let id = DataId::new("frames").unwrap();

        let Event::Input { meta, data, id: at } = message(metadata).into_event(id.clone()) else {
            panic!("expected an input");
        };
        assert_eq!(at, id);
        assert_eq!(data.len(), 3);
        assert_eq!(meta.schema_hash(), None, "internal keys are stripped");
        assert_eq!(
            meta.request_id(),
            Some("req-1"),
            "well-known keys survive: patterns depend on them"
        );
    }

    #[test]
    fn closing_events_convert_with_their_reason() {
        let id = DataId::new("frames").unwrap();
        let event = QueuedEvent::Closed {
            source: port(),
            reason: RouteCloseReason::ProducerCrashed { generation: 2 },
        }
        .into_event(id.clone());
        assert!(matches!(event, Event::InputClosed { .. }));
        assert!(event.is_fault());

        let event = QueuedEvent::Recovered {
            source: port(),
            generation: 5,
        }
        .into_event(id);
        let Event::InputRecovered { generation, .. } = event else {
            panic!("expected a recovery");
        };
        assert_eq!(generation, 5);
    }
}
