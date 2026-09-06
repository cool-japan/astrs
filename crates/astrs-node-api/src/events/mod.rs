//! [`Event`] and [`EventStream`] — what a node's loop reads (blueprint §9.1).
//!
//! ```no_run
//! # use astrs_node_api::prelude::*;
//! # fn run(events: &mut EventStream) {
//! while let Some(event) = events.recv() {
//!     match event {
//!         Event::Input { id, data, meta } => { let _ = (id, data, meta); }
//!         Event::Stop(_) => break,
//!         _ => {}
//!     }
//! }
//! # }
//! ```
//!
//! # Two lanes, one stream
//!
//! Events reach the stream on one of two paths, and the difference is
//! observable (§11.3):
//!
//! | Lane | Events | Behaviour |
//! |---|---|---|
//! | **Control** | `Stop`, `Reload`, `AllInputsClosed`, `NodeFailed`, `Restarted`, `ParamUpdate`, `ParamDeleted`, `ExtDropped`, `Error`, merged external events | Unbounded FIFO, delivered **before** any data |
//! | **Data** | `Input`, `InputClosed`, `InputRecovered` | Per-input bounded queues under the manifest's `queue_size`/`queue_policy`, round-robin across inputs |
//!
//! That is why a `Stop` never waits behind a backlog of camera frames, and
//! why an `InputClosed` *does* wait behind that input's own queued frames —
//! it has to, or a node would see "this input is finished" before the frames
//! that came first.
//!
//! # Fusing
//!
//! After [`Event::Stop`] the stream fuses: it is delivered once, and every
//! later `recv` returns `None` (§9.1). A node that ignores `Stop` and keeps
//! looping therefore terminates rather than spinning.
//!
//! # Queue policy is applied *here*
//!
//! The daemon does not decide what a node drops. Each input owns an
//! [`astrs_scheduler::InputQueue`] on the node side, so `queue_size: 1` means
//! "the node always sees the newest frame" no matter how the daemon batched
//! them, and eviction immunity (§11.2) protects correlated messages from ever
//! being the one dropped.

pub mod queued;
pub mod source;
pub mod stream;

use astrs_wire::messages::control::types::ParamScope;
use astrs_wire::metadata::Parameter;
use astrs_wire::{
    DataId, ExtensionKey, Metadata, NodeExitCause, NodeId, OperatorId, ParamKey, PortRef,
    RouteCloseReason, StopCause,
};

use crate::payload::Payload;

pub use queued::QueuedEvent;
pub use source::EventSource;
pub use stream::{EventStream, Stream};

/// One thing that happened to this node.
///
/// [`Event::Input`] carries exactly the three fields blueprint §9.1
/// destructures — `{ id, data, meta }` — so the example there compiles
/// verbatim. The producer port behind an input is a property of the node's
/// wiring rather than of the message, and is available from
/// [`crate::Node::input_source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// A message arrived on one of the node's inputs.
    Input {
        /// The input it arrived on.
        id: DataId,
        /// The metadata beside it, with AstRS-internal keys already
        /// stripped (§6.1).
        meta: Metadata,
        /// The payload — decoded lazily, and zero-copy when the route was
        /// upgraded (§6.3).
        data: Payload,
    },
    /// An input will receive nothing further.
    InputClosed {
        /// The input that closed.
        id: DataId,
        /// The producer port that stopped.
        source: PortRef,
        /// Why it closed.
        reason: RouteCloseReason,
    },
    /// A closed input is live again, because its producer restarted (§12).
    InputRecovered {
        /// The input that recovered.
        id: DataId,
        /// The producer port that came back.
        source: PortRef,
        /// The producer's new incarnation.
        generation: u64,
    },
    /// Finish up and exit. The stream fuses after this.
    Stop(StopCause),
    /// Reload the node's code, or one operator inside it (§9.3).
    Reload {
        /// The operator to reload, or `None` for the whole node.
        operator: Option<OperatorId>,
        /// The artefact to load, when it moved.
        path: Option<String>,
    },
    /// Every input of this node has closed.
    AllInputsClosed,
    /// A parameter this node reads was written (§17 `param set`).
    ParamUpdate {
        /// The scope it was written in.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
        /// The new value.
        value: Parameter,
    },
    /// A parameter this node reads was deleted.
    ParamDeleted {
        /// The scope it was deleted from.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
    },
    /// A peer node failed (§12).
    NodeFailed {
        /// The node that failed.
        peer: NodeId,
        /// How it failed.
        cause: NodeExitCause,
    },
    /// A peer node was restarted under its restart policy (§12).
    Restarted {
        /// The node that came back.
        peer: NodeId,
        /// Its new incarnation.
        generation: u64,
    },
    /// An extension entry this node owned was dropped.
    ExtDropped {
        /// The key that went away.
        key: ExtensionKey,
        /// Why.
        reason: String,
    },
    /// The session hit a condition the node should know about but that does
    /// not end it — a queue overflow, an undecodable frame, a route the node
    /// could not map.
    Error(String),
}

impl Event {
    /// The input this event concerns, when it concerns one.
    #[must_use]
    pub const fn input(&self) -> Option<&DataId> {
        match self {
            Self::Input { id, .. }
            | Self::InputClosed { id, .. }
            | Self::InputRecovered { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Whether this event delivers data.
    #[must_use]
    pub const fn is_input(&self) -> bool {
        matches!(self, Self::Input { .. })
    }

    /// Whether this event fuses the stream.
    #[must_use]
    pub const fn is_stop(&self) -> bool {
        matches!(self, Self::Stop(_))
    }

    /// Whether this event travels on the control lane (§11.3).
    #[must_use]
    pub const fn is_control(&self) -> bool {
        !matches!(
            self,
            Self::Input { .. } | Self::InputClosed { .. } | Self::InputRecovered { .. }
        )
    }

    /// Whether this event reports a fault rather than normal progress.
    #[must_use]
    pub fn is_fault(&self) -> bool {
        match self {
            Self::NodeFailed { .. } | Self::Error(_) => true,
            Self::InputClosed { reason, .. } => !reason.is_expected(),
            _ => false,
        }
    }

    /// The metadata this event carries, when it carries any.
    #[must_use]
    pub const fn metadata(&self) -> Option<&Metadata> {
        match self {
            Self::Input { meta, .. } => Some(meta),
            _ => None,
        }
    }

    /// The payload this event carries, when it carries one.
    #[must_use]
    pub const fn payload(&self) -> Option<&Payload> {
        match self {
            Self::Input { data, .. } => Some(data),
            _ => None,
        }
    }

    /// Consumes the event and returns its payload and metadata, if it is an
    /// input.
    #[must_use]
    pub fn into_input(self) -> Option<(DataId, Metadata, Payload)> {
        match self {
            Self::Input { id, meta, data } => Some((id, meta, data)),
            _ => None,
        }
    }

    /// A short, stable slug for metrics labels and structured logs.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Input { .. } => "input",
            Self::InputClosed { .. } => "input_closed",
            Self::InputRecovered { .. } => "input_recovered",
            Self::Stop(_) => "stop",
            Self::Reload { .. } => "reload",
            Self::AllInputsClosed => "all_inputs_closed",
            Self::ParamUpdate { .. } => "param_update",
            Self::ParamDeleted { .. } => "param_deleted",
            Self::NodeFailed { .. } => "node_failed",
            Self::Restarted { .. } => "restarted",
            Self::ExtDropped { .. } => "ext_dropped",
            Self::Error(_) => "error",
        }
    }
}

impl core::fmt::Display for Event {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Input { id, data, .. } => write!(f, "input {id} ({data})"),
            Self::InputClosed { id, reason, .. } => write!(f, "input {id} closed: {reason}"),
            Self::InputRecovered { id, generation, .. } => {
                write!(f, "input {id} recovered at generation {generation}")
            }
            Self::Stop(cause) => write!(f, "stop: {cause}"),
            Self::Reload { operator, .. } => match operator {
                Some(operator) => write!(f, "reload operator {operator}"),
                None => f.write_str("reload node"),
            },
            Self::AllInputsClosed => f.write_str("all inputs closed"),
            Self::ParamUpdate { scope, key, value } => write!(f, "{scope}: {key} = {value}"),
            Self::ParamDeleted { scope, key } => write!(f, "{scope}: {key} deleted"),
            Self::NodeFailed { peer, cause } => write!(f, "peer {peer} failed: {cause}"),
            Self::Restarted { peer, generation } => {
                write!(f, "peer {peer} restarted as generation {generation}")
            }
            Self::ExtDropped { key, reason } => write!(f, "extension {key} dropped: {reason}"),
            Self::Error(message) => write!(f, "error: {message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn input() -> Event {
        Event::Input {
            id: DataId::new("frames").unwrap(),
            meta: Metadata::new(HlcTimestamp::new(1, 0)),
            data: Payload::inline(vec![1, 2, 3]),
        }
    }

    fn every_variant() -> Vec<Event> {
        vec![
            input(),
            Event::InputClosed {
                id: DataId::new("frames").unwrap(),
                source: PortRef::from_parts("camera", "image").unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            },
            Event::InputRecovered {
                id: DataId::new("frames").unwrap(),
                source: PortRef::from_parts("camera", "image").unwrap(),
                generation: 2,
            },
            Event::Stop(StopCause::Requested),
            Event::Reload {
                operator: None,
                path: None,
            },
            Event::AllInputsClosed,
            Event::ParamUpdate {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::from(2.0),
            },
            Event::ParamDeleted {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
            },
            Event::NodeFailed {
                peer: NodeId::new("camera").unwrap(),
                cause: NodeExitCause::Success,
            },
            Event::Restarted {
                peer: NodeId::new("camera").unwrap(),
                generation: 1,
            },
            Event::ExtDropped {
                key: ExtensionKey::user("calibration").unwrap(),
                reason: "ttl".to_owned(),
            },
            Event::Error("queue overflow".to_owned()),
        ]
    }

    #[test]
    fn every_variant_renders_and_classifies() {
        for event in every_variant() {
            assert!(!event.to_string().is_empty(), "{event:?}");
            assert!(!event.kind_name().is_empty());
        }
    }

    #[test]
    fn kind_names_are_unique() {
        let mut names: Vec<&str> = every_variant().iter().map(Event::kind_name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }

    #[test]
    fn the_control_lane_is_everything_that_is_not_an_input() {
        for event in every_variant() {
            let data_lane = matches!(
                event,
                Event::Input { .. } | Event::InputClosed { .. } | Event::InputRecovered { .. }
            );
            assert_eq!(event.is_control(), !data_lane, "{event}");
        }
    }

    #[test]
    fn inputs_expose_their_parts() {
        let event = input();
        assert!(event.is_input());
        assert!(!event.is_stop());
        assert_eq!(event.input().map(DataId::as_str), Some("frames"));
        assert!(event.metadata().is_some());
        assert_eq!(event.payload().map(Payload::len), Some(3));

        let (id, meta, data) = input().into_input().unwrap();
        assert_eq!(id.as_str(), "frames");
        assert_eq!(meta.timestamp, HlcTimestamp::new(1, 0));
        assert_eq!(data.len(), 3);
        assert!(Event::AllInputsClosed.into_input().is_none());
    }

    #[test]
    fn stop_is_the_only_fusing_event() {
        for event in every_variant() {
            assert_eq!(event.is_stop(), matches!(event, Event::Stop(_)));
        }
    }

    #[test]
    fn faults_are_reported_as_faults() {
        assert!(Event::Error(String::new()).is_fault());
        assert!(
            Event::NodeFailed {
                peer: NodeId::new("x").unwrap(),
                cause: NodeExitCause::Success
            }
            .is_fault()
        );
        assert!(!input().is_fault());
        assert!(
            !Event::InputClosed {
                id: DataId::new("a").unwrap(),
                source: PortRef::from_parts("b", "c").unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            }
            .is_fault(),
            "a producer finishing is not a fault"
        );
    }
}
