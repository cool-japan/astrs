//! The `kind` header field: the top-level message family of a frame.
//!
//! `kind` exists so a router can dispatch a frame **without decoding its
//! payload** (blueprint §7.1). A daemon relaying a topic tap to the CLI, or a
//! coordinator fanning logs out to subscribers, only needs the two bytes at
//! offset 4 to know where a frame belongs.
//!
//! Discriminants are frozen; new families are appended at the tail.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::WireError;

/// The top-level message family carried by a frame.
///
/// Each control-plane family maps one-to-one onto a message enum in
/// [`crate::messages`]; [`FrameKind::Data`], [`FrameKind::Log`] and
/// [`FrameKind::Telemetry`] carry the fan-out payloads described in §7.3,
/// which ride the same framing with a [`crate::SubscriptionId`] in the payload
/// rather than a bespoke side channel.
///
/// # Examples
///
/// ```
/// use astrs_wire::FrameKind;
///
/// assert_eq!(FrameKind::Control.as_u16(), 0);
/// assert_eq!(FrameKind::from_u16(6)?, FrameKind::PeerEvent);
/// assert!(FrameKind::from_u16(9_999).is_err());
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u16)]
#[non_exhaustive]
pub enum FrameKind {
    /// A [`crate::ControlRequest`]: CLI → coordinator.
    Control = 0,
    /// A [`crate::ControlReply`]: coordinator → CLI.
    ControlReply = 1,
    /// A [`crate::CoordinatorEvent`]: coordinator → daemon.
    CoordinatorEvent = 2,
    /// A [`crate::DaemonEvent`]: daemon → coordinator.
    DaemonEvent = 3,
    /// A [`crate::NodeRequest`]: node → daemon.
    NodeRequest = 4,
    /// A [`crate::NodeEvent`]: daemon → node.
    NodeEvent = 5,
    /// A [`crate::PeerEvent`]: daemon → daemon.
    PeerEvent = 6,
    /// A subscribed data payload (`astrs topic echo`, replay feeds): the
    /// payload is a [`crate::DataFrame`].
    Data = 7,
    /// A subscribed log record (`astrs logs -f`): the payload is a
    /// [`crate::LogFrame`].
    Log = 8,
    /// A telemetry sample batch: the payload is a
    /// [`crate::TelemetryFrame`].
    Telemetry = 9,
}

impl FrameKind {
    /// Every family this build knows, in discriminant order.
    ///
    /// Used by the protocol snapshot and by exhaustiveness tests.
    pub const ALL: &'static [Self] = &[
        Self::Control,
        Self::ControlReply,
        Self::CoordinatorEvent,
        Self::DaemonEvent,
        Self::NodeRequest,
        Self::NodeEvent,
        Self::PeerEvent,
        Self::Data,
        Self::Log,
        Self::Telemetry,
    ];

    /// The wire discriminant.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameKind;
    ///
    /// assert_eq!(FrameKind::Telemetry.as_u16(), 9);
    /// ```
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Parses a wire discriminant.
    ///
    /// # Errors
    ///
    /// [`WireError::UnknownKind`] if the discriminant is not one this build
    /// knows. Frames of an unknown family are rejected rather than guessed at;
    /// a router that wishes to forward them opaquely can inspect the raw two
    /// bytes itself.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameKind;
    ///
    /// assert_eq!(FrameKind::from_u16(3)?, FrameKind::DaemonEvent);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub const fn from_u16(value: u16) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Control),
            1 => Ok(Self::ControlReply),
            2 => Ok(Self::CoordinatorEvent),
            3 => Ok(Self::DaemonEvent),
            4 => Ok(Self::NodeRequest),
            5 => Ok(Self::NodeEvent),
            6 => Ok(Self::PeerEvent),
            7 => Ok(Self::Data),
            8 => Ok(Self::Log),
            9 => Ok(Self::Telemetry),
            found => Err(WireError::UnknownKind { found }),
        }
    }

    /// A stable, lower-case name for logs, metrics labels and the protocol
    /// snapshot.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameKind;
    ///
    /// assert_eq!(FrameKind::NodeRequest.as_str(), "node_request");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::ControlReply => "control_reply",
            Self::CoordinatorEvent => "coordinator_event",
            Self::DaemonEvent => "daemon_event",
            Self::NodeRequest => "node_request",
            Self::NodeEvent => "node_event",
            Self::PeerEvent => "peer_event",
            Self::Data => "data",
            Self::Log => "log",
            Self::Telemetry => "telemetry",
        }
    }

    /// Whether this family carries bulk payload rather than control messages.
    ///
    /// Bulk families are the ones route compression (§6.4) and the 16 KiB
    /// compression threshold apply to.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameKind;
    ///
    /// assert!(FrameKind::Data.is_bulk());
    /// assert!(!FrameKind::Control.is_bulk());
    /// ```
    #[must_use]
    pub const fn is_bulk(self) -> bool {
        matches!(self, Self::Data | Self::Log | Self::Telemetry)
    }

    /// Whether this family is part of the control plane proper — i.e. maps to
    /// one of the seven §24.1 message enums.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameKind;
    ///
    /// assert!(FrameKind::PeerEvent.is_control_plane());
    /// assert!(!FrameKind::Log.is_control_plane());
    /// ```
    #[must_use]
    pub const fn is_control_plane(self) -> bool {
        !self.is_bulk()
    }
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<FrameKind> for u16 {
    fn from(kind: FrameKind) -> Self {
        kind.as_u16()
    }
}

impl TryFrom<u16> for FrameKind {
    type Error = WireError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::from_u16(value)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn discriminants_are_frozen() {
        // Changing any of these numbers is a wire break. The protocol
        // snapshot test freezes them too; this is the fast local signal.
        assert_eq!(FrameKind::Control.as_u16(), 0);
        assert_eq!(FrameKind::ControlReply.as_u16(), 1);
        assert_eq!(FrameKind::CoordinatorEvent.as_u16(), 2);
        assert_eq!(FrameKind::DaemonEvent.as_u16(), 3);
        assert_eq!(FrameKind::NodeRequest.as_u16(), 4);
        assert_eq!(FrameKind::NodeEvent.as_u16(), 5);
        assert_eq!(FrameKind::PeerEvent.as_u16(), 6);
        assert_eq!(FrameKind::Data.as_u16(), 7);
        assert_eq!(FrameKind::Log.as_u16(), 8);
        assert_eq!(FrameKind::Telemetry.as_u16(), 9);
    }

    #[test]
    fn all_is_dense_and_ordered() {
        for (index, kind) in FrameKind::ALL.iter().enumerate() {
            assert_eq!(usize::from(kind.as_u16()), index);
        }
        assert_eq!(FrameKind::ALL.len(), 10);
    }

    #[test]
    fn round_trips_through_u16() {
        for &kind in FrameKind::ALL {
            assert_eq!(FrameKind::from_u16(kind.as_u16()).unwrap(), kind);
            assert_eq!(FrameKind::try_from(u16::from(kind)).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_discriminants_are_rejected() {
        for value in [10u16, 11, 255, 256, u16::MAX] {
            match FrameKind::from_u16(value) {
                Err(WireError::UnknownKind { found }) => assert_eq!(found, value),
                other => panic!("expected UnknownKind for {value}, got {other:?}"),
            }
        }
    }

    #[test]
    fn names_are_unique_and_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for &kind in FrameKind::ALL {
            let name = kind.as_str();
            assert!(seen.insert(name), "duplicate name {name}");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not snake_case"
            );
            assert_eq!(kind.to_string(), name);
        }
    }

    #[test]
    fn bulk_and_control_plane_partition_the_set() {
        for &kind in FrameKind::ALL {
            assert_ne!(kind.is_bulk(), kind.is_control_plane());
        }
        assert_eq!(
            FrameKind::ALL.iter().filter(|k| k.is_bulk()).count(),
            3,
            "Data, Log and Telemetry are the bulk families"
        );
    }

    #[test]
    fn serde_uses_snake_case_names() {
        let json = serde_json::to_string(&FrameKind::CoordinatorEvent).unwrap();
        assert_eq!(json, "\"coordinator_event\"");
        let back: FrameKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, FrameKind::CoordinatorEvent);
    }
}
