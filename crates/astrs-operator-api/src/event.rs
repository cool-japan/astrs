//! [`OpEvent`] — the event stream an [`crate::Operator`] sees.
//!
//! A lean mirror of [`astrs_wire::NodeEvent`] (blueprint §7.3, §9.3):
//! operators run inside a node-shaped host (`astrs-runtime`) that already
//! resolved routing, extension-table plumbing and peer bookkeeping at the
//! node level, so the operator only ever needs the five variants below.
//! Every field reuses the exact `astrs-wire` type the node-level event
//! carries — no parallel id/metadata vocabulary, no wire dependency beyond
//! those types themselves.

use astrs_wire::{
    DataId, DurationMs, Metadata, NodeEvent, ParamKey, ParamScope, Parameter, PortRef,
    RouteCloseReason, StopCause,
};

/// One event delivered to an [`crate::Operator`] (blueprint §9.3).
///
/// `#[non_exhaustive]`: new variants may join at the tail as the runtime's
/// operator-hosting story grows (blueprint §3.4's append-only rule).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum OpEvent {
    /// One message for one of the operator's inputs.
    ///
    /// The payload is raw bytes — an Arrow IPC stream (blueprint §6.1) —
    /// exactly as it rides `astrs_wire::NodeEvent::Input`; decoding into a
    /// typed [`astrs_data::AstrsMessage`] is the operator's own business.
    Input {
        /// The input this message arrived on.
        id: DataId,
        /// The producer port it came from.
        source: PortRef,
        /// The metadata riding beside the payload.
        metadata: Metadata,
        /// The payload bytes.
        payload: Vec<u8>,
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
    /// Finish up: the host is winding the operator down.
    Stop {
        /// Why the operator is being stopped.
        cause: StopCause,
        /// How long it has before it is dropped without a clean exit.
        grace: Option<DurationMs>,
    },
    /// Reset the operator's own state (blueprint §9.3).
    ///
    /// Unlike `astrs_wire::NodeEvent::Reload`, this carries no `operator`
    /// selector or `path`: by the time the host delivers this event on a
    /// *specific* operator's channel, the routing question is already
    /// answered, and dylib hot-reload (the case `path` exists for) is
    /// deferred to 0.2 (blueprint §9.3, §22) — there is no artefact to name
    /// yet.
    Reload,
    /// A parameter this operator reads was written (blueprint §17
    /// `param set`).
    ParamUpdate {
        /// The scope it was written in.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
        /// The new value.
        value: Parameter,
    },
}

impl OpEvent {
    /// The input this event concerns, when it concerns one.
    #[must_use]
    pub const fn input(&self) -> Option<&DataId> {
        match self {
            Self::Input { id, .. } | Self::InputClosed { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Whether this event delivers data.
    #[must_use]
    pub const fn is_input(&self) -> bool {
        matches!(self, Self::Input { .. })
    }

    /// Whether this event means the operator should wind down.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Stop { .. })
    }

    /// The payload bytes this event carries, for bandwidth accounting.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Input { payload, .. } => payload.len(),
            _ => 0,
        }
    }

    /// Narrows a node-level [`NodeEvent`] to the operator-level event it
    /// corresponds to, or `None` when the node event has no operator-level
    /// meaning (route-plane changes, peer lifecycle, extension-table
    /// traffic and registration — all handled by the node host itself,
    /// never surfaced to an operator).
    ///
    /// This is the bridge `astrs-runtime` uses to fan a node's event stream
    /// out to each hosted operator's own channel (blueprint §9.3: "each
    /// operator runs on its own thread over a bounded channel").
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_operator_api::OpEvent;
    /// use astrs_wire::{DataId, Metadata, NodeEvent};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let node_event = NodeEvent::Input {
    ///     id: DataId::new("frames").unwrap(),
    ///     source: "camera/image".parse().unwrap(),
    ///     metadata: Metadata::new(HlcTimestamp::EPOCH),
    ///     payload: vec![1, 2, 3],
    /// };
    /// assert!(OpEvent::from_node_event(&node_event).is_some());
    /// assert!(OpEvent::from_node_event(&NodeEvent::AllInputsClosed).is_none());
    /// ```
    #[must_use]
    pub fn from_node_event(event: &NodeEvent) -> Option<Self> {
        match event {
            NodeEvent::Input {
                id,
                source,
                metadata,
                payload,
            } => Some(Self::Input {
                id: id.clone(),
                source: source.clone(),
                metadata: metadata.clone(),
                payload: payload.clone(),
            }),
            NodeEvent::InputClosed { id, source, reason } => Some(Self::InputClosed {
                id: id.clone(),
                source: source.clone(),
                reason: reason.clone(),
            }),
            NodeEvent::Stop { cause, grace } => Some(Self::Stop {
                cause: cause.clone(),
                grace: *grace,
            }),
            NodeEvent::Reload { .. } => Some(Self::Reload),
            NodeEvent::ParamUpdate { scope, key, value } => Some(Self::ParamUpdate {
                scope: scope.clone(),
                key: key.clone(),
                value: value.clone(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn sample_input() -> OpEvent {
        OpEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn input_accessors() {
        let event = sample_input();
        assert!(event.is_input());
        assert_eq!(event.input().map(DataId::as_str), Some("frames"));
        assert_eq!(event.payload_len(), 4);
        assert!(!event.is_terminal());
    }

    #[test]
    fn stop_is_terminal_and_carries_no_input() {
        let event = OpEvent::Stop {
            cause: StopCause::Requested,
            grace: None,
        };
        assert!(event.is_terminal());
        assert!(event.input().is_none());
        assert!(!event.is_input());
        assert_eq!(event.payload_len(), 0);
    }

    #[test]
    fn input_closed_names_its_input_but_is_not_data() {
        let event = OpEvent::InputClosed {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            reason: RouteCloseReason::ProducerFinished,
        };
        assert_eq!(event.input().map(DataId::as_str), Some("frames"));
        assert!(!event.is_input());
    }

    #[test]
    fn from_node_event_narrows_the_five_operator_relevant_variants() {
        let input = NodeEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload: vec![9],
        };
        assert_eq!(
            OpEvent::from_node_event(&input),
            Some(OpEvent::Input {
                id: DataId::new("frames").unwrap(),
                source: "camera/image".parse().unwrap(),
                metadata: Metadata::new(HlcTimestamp::EPOCH),
                payload: vec![9],
            })
        );

        let closed = NodeEvent::InputClosed {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            reason: RouteCloseReason::ProducerFinished,
        };
        assert!(matches!(
            OpEvent::from_node_event(&closed),
            Some(OpEvent::InputClosed { .. })
        ));

        let stop = NodeEvent::Stop {
            cause: StopCause::Requested,
            grace: None,
        };
        assert_eq!(
            OpEvent::from_node_event(&stop),
            Some(OpEvent::Stop {
                cause: StopCause::Requested,
                grace: None
            })
        );

        let reload = NodeEvent::Reload {
            operator: None,
            path: None,
        };
        assert_eq!(OpEvent::from_node_event(&reload), Some(OpEvent::Reload));

        let param = NodeEvent::ParamUpdate {
            scope: ParamScope::Global,
            key: ParamKey::new("gain").unwrap(),
            value: Parameter::Float(1.5),
        };
        assert_eq!(
            OpEvent::from_node_event(&param),
            Some(OpEvent::ParamUpdate {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Float(1.5),
            })
        );
    }

    #[test]
    fn from_node_event_drops_events_with_no_operator_level_meaning() {
        for event in [
            NodeEvent::AllInputsClosed,
            NodeEvent::InputRecovered {
                id: DataId::new("frames").unwrap(),
                source: "camera/image".parse().unwrap(),
                generation: 2,
            },
            NodeEvent::NodeFailed {
                peer: astrs_wire::NodeId::new("camera").unwrap(),
                cause: astrs_wire::NodeExitCause::Success,
            },
            NodeEvent::Restarted {
                peer: astrs_wire::NodeId::new("camera").unwrap(),
                generation: 2,
            },
            NodeEvent::ParamDeleted {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
            },
        ] {
            assert!(
                OpEvent::from_node_event(&event).is_none(),
                "{event:?} should have no operator-level meaning"
            );
        }
    }
}
