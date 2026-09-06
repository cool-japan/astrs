//! [`AnyMessage`]: decode a frame of any family without knowing which.
//!
//! A router usually does **not** want this — the whole point of the `kind`
//! header field is that a frame can be forwarded on two bytes, without decoding
//! (§7.1). But three callers legitimately need to look inside an arbitrary
//! frame: a diagnostic dump (`astrs doctor`, a captured trace), a replay of a
//! recorded control stream (§14), and the property tests that assert no frame
//! ever panics a decoder.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{AnyMessage, ControlRequest, FrameFlags, FrameKind, FrameLimits, WireMessage};
//!
//! let limits = FrameLimits::uds();
//! let bytes = ControlRequest::List { all: true }.to_frame(FrameFlags::EMPTY, &limits)?;
//!
//! let message = AnyMessage::from_bytes(&bytes, &limits)?;
//! assert_eq!(message.kind(), FrameKind::Control);
//! assert_eq!(message.variant_name(), "List");
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

use core::fmt;

use crate::common::stream::{DataFrame, LogFrame, TelemetryFrame};
use crate::error::WireResult;
use crate::frame::{FrameFlags, FrameKind, FrameLimits, FrameView, decode_frame};
use crate::messages::WireMessage;
use crate::messages::control::{ControlReply, ControlRequest};
use crate::messages::coordinator_daemon::{CoordinatorEvent, DaemonEvent};
use crate::messages::daemon_node::{NodeEvent, NodeRequest};
use crate::messages::peer::PeerEvent;

/// One decoded message of any family.
///
/// The variant set mirrors [`FrameKind`] exactly — one arm per family — so a
/// `match` here is total over the protocol.
///
/// # Examples
///
/// ```
/// use astrs_wire::{AnyMessage, FrameKind, PeerEvent};
///
/// let message = AnyMessage::PeerEvent(PeerEvent::ping(1, Default::default()));
/// assert_eq!(message.kind(), FrameKind::PeerEvent);
/// assert!(message.is_control_plane());
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AnyMessage {
    /// A CLI → coordinator request.
    Control(ControlRequest),
    /// A coordinator → CLI reply.
    ControlReply(ControlReply),
    /// A coordinator → daemon event.
    CoordinatorEvent(CoordinatorEvent),
    /// A daemon → coordinator event.
    DaemonEvent(DaemonEvent),
    /// A node → daemon request.
    NodeRequest(NodeRequest),
    /// A daemon → node event.
    NodeEvent(NodeEvent),
    /// A daemon ↔ daemon event.
    PeerEvent(PeerEvent),
    /// A subscribed data payload.
    Data(DataFrame),
    /// A subscribed log record.
    Log(LogFrame),
    /// A telemetry sample batch.
    Telemetry(TelemetryFrame),
}

impl AnyMessage {
    /// Decodes whichever family the frame's header names.
    ///
    /// # Errors
    ///
    /// - [`crate::WireError::CompressedPayload`] if the payload is still
    ///   compressed.
    /// - [`crate::WireError::Codec`] or [`crate::WireError::TrailingBytes`]
    ///   from the codec.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{
    ///     AnyMessage, FrameFlags, FrameLimits, PeerEvent, WireMessage, decode_frame,
    /// };
    ///
    /// let limits = FrameLimits::uds();
    /// let bytes = PeerEvent::ping(1, Default::default()).to_frame(FrameFlags::EMPTY, &limits)?;
    /// let view = decode_frame(&bytes, &limits)?;
    /// assert!(matches!(AnyMessage::from_frame(&view)?, AnyMessage::PeerEvent(_)));
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn from_frame(view: &FrameView<'_>) -> WireResult<Self> {
        Ok(match view.kind() {
            FrameKind::Control => Self::Control(view.decode()?),
            FrameKind::ControlReply => Self::ControlReply(view.decode()?),
            FrameKind::CoordinatorEvent => Self::CoordinatorEvent(view.decode()?),
            FrameKind::DaemonEvent => Self::DaemonEvent(view.decode()?),
            FrameKind::NodeRequest => Self::NodeRequest(view.decode()?),
            FrameKind::NodeEvent => Self::NodeEvent(view.decode()?),
            FrameKind::PeerEvent => Self::PeerEvent(view.decode()?),
            FrameKind::Data => Self::Data(view.decode()?),
            FrameKind::Log => Self::Log(view.decode()?),
            FrameKind::Telemetry => Self::Telemetry(view.decode()?),
        })
    }

    /// Decodes a buffer holding exactly one complete frame.
    ///
    /// # Errors
    ///
    /// As [`decode_frame`] and [`AnyMessage::from_frame`].
    pub fn from_bytes(bytes: &[u8], limits: &FrameLimits) -> WireResult<Self> {
        let view = decode_frame(bytes, limits)?;
        Self::from_frame(&view)
    }

    /// The family this message belongs to.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        match self {
            Self::Control(_) => FrameKind::Control,
            Self::ControlReply(_) => FrameKind::ControlReply,
            Self::CoordinatorEvent(_) => FrameKind::CoordinatorEvent,
            Self::DaemonEvent(_) => FrameKind::DaemonEvent,
            Self::NodeRequest(_) => FrameKind::NodeRequest,
            Self::NodeEvent(_) => FrameKind::NodeEvent,
            Self::PeerEvent(_) => FrameKind::PeerEvent,
            Self::Data(_) => FrameKind::Data,
            Self::Log(_) => FrameKind::Log,
            Self::Telemetry(_) => FrameKind::Telemetry,
        }
    }

    /// The wire index of the variant inside its family.
    #[must_use]
    pub fn variant_index(&self) -> u16 {
        match self {
            Self::Control(message) => message.variant_index(),
            Self::ControlReply(message) => message.variant_index(),
            Self::CoordinatorEvent(message) => message.variant_index(),
            Self::DaemonEvent(message) => message.variant_index(),
            Self::NodeRequest(message) => message.variant_index(),
            Self::NodeEvent(message) => message.variant_index(),
            Self::PeerEvent(message) => message.variant_index(),
            Self::Data(message) => message.variant_index(),
            Self::Log(message) => message.variant_index(),
            Self::Telemetry(message) => message.variant_index(),
        }
    }

    /// The variant name inside its family, as the protocol snapshot spells it.
    #[must_use]
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Control(message) => message.variant_name(),
            Self::ControlReply(message) => message.variant_name(),
            Self::CoordinatorEvent(message) => message.variant_name(),
            Self::DaemonEvent(message) => message.variant_name(),
            Self::NodeRequest(message) => message.variant_name(),
            Self::NodeEvent(message) => message.variant_name(),
            Self::PeerEvent(message) => message.variant_name(),
            Self::Data(message) => message.variant_name(),
            Self::Log(message) => message.variant_name(),
            Self::Telemetry(message) => message.variant_name(),
        }
    }

    /// Whether this message belongs to the control plane proper rather than a
    /// fan-out stream.
    #[must_use]
    pub const fn is_control_plane(&self) -> bool {
        self.kind().is_control_plane()
    }

    /// Re-encodes this message as a complete frame.
    ///
    /// # Errors
    ///
    /// As [`crate::write_message`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{AnyMessage, FrameFlags, FrameLimits, PeerEvent, WireMessage};
    ///
    /// let limits = FrameLimits::uds();
    /// let event = PeerEvent::ping(3, Default::default());
    /// let original = event.to_frame(FrameFlags::EMPTY, &limits)?;
    /// let decoded = AnyMessage::from_bytes(&original, &limits)?;
    /// assert_eq!(decoded.to_frame(FrameFlags::EMPTY, &limits)?, original);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn to_frame(&self, flags: FrameFlags, limits: &FrameLimits) -> WireResult<Vec<u8>> {
        match self {
            Self::Control(message) => message.to_frame(flags, limits),
            Self::ControlReply(message) => message.to_frame(flags, limits),
            Self::CoordinatorEvent(message) => message.to_frame(flags, limits),
            Self::DaemonEvent(message) => message.to_frame(flags, limits),
            Self::NodeRequest(message) => message.to_frame(flags, limits),
            Self::NodeEvent(message) => message.to_frame(flags, limits),
            Self::PeerEvent(message) => message.to_frame(flags, limits),
            Self::Data(message) => message.to_frame(flags, limits),
            Self::Log(message) => message.to_frame(flags, limits),
            Self::Telemetry(message) => message.to_frame(flags, limits),
        }
    }

    /// The payload bytes this message carries, for bandwidth accounting.
    #[must_use]
    pub fn payload_len(&self) -> u64 {
        match self {
            Self::Control(message) => message.payload_len() as u64,
            Self::DaemonEvent(message) => message.payload_len() as u64,
            Self::NodeRequest(message) => message.payload_len(),
            Self::NodeEvent(message) => message.payload_len() as u64,
            Self::PeerEvent(message) => message.payload_len() as u64,
            Self::Data(message) => message.payload_len() as u64,
            _ => 0,
        }
    }

    /// Compares two messages with `f64` bit patterns rather than IEEE
    /// equality — see [`crate::PeerEvent::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Control(left), Self::Control(right)) => left.bitwise_eq(right),
            (Self::ControlReply(left), Self::ControlReply(right)) => left.bitwise_eq(right),
            (Self::CoordinatorEvent(left), Self::CoordinatorEvent(right)) => left.bitwise_eq(right),
            (Self::DaemonEvent(left), Self::DaemonEvent(right)) => left.bitwise_eq(right),
            (Self::NodeRequest(left), Self::NodeRequest(right)) => left.bitwise_eq(right),
            (Self::NodeEvent(left), Self::NodeEvent(right)) => left.bitwise_eq(right),
            (Self::PeerEvent(left), Self::PeerEvent(right)) => left.bitwise_eq(right),
            (Self::Data(left), Self::Data(right)) => left.bitwise_eq(right),
            _ => self == other,
        }
    }
}

impl fmt::Display for AnyMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::", self.kind())?;
        match self {
            Self::Control(message) => write!(f, "{message}"),
            Self::ControlReply(message) => write!(f, "{message}"),
            Self::CoordinatorEvent(message) => write!(f, "{message}"),
            Self::DaemonEvent(message) => write!(f, "{message}"),
            Self::NodeRequest(message) => write!(f, "{message}"),
            Self::NodeEvent(message) => write!(f, "{message}"),
            Self::PeerEvent(message) => write!(f, "{message}"),
            Self::Data(message) => write!(
                f,
                "subscription {} ({} byte(s))",
                message.subscription,
                message.payload_len()
            ),
            Self::Log(message) => write!(f, "subscription {}", message.subscription),
            Self::Telemetry(message) => write!(
                f,
                "subscription {} ({} point(s))",
                message.subscription,
                message.batch.points.len()
            ),
        }
    }
}

macro_rules! impl_from_family {
    ($ty:ty, $variant:ident) => {
        impl From<$ty> for AnyMessage {
            fn from(message: $ty) -> Self {
                Self::$variant(message)
            }
        }
    };
}

impl_from_family!(ControlRequest, Control);
impl_from_family!(ControlReply, ControlReply);
impl_from_family!(CoordinatorEvent, CoordinatorEvent);
impl_from_family!(DaemonEvent, DaemonEvent);
impl_from_family!(NodeRequest, NodeRequest);
impl_from_family!(NodeEvent, NodeEvent);
impl_from_family!(PeerEvent, PeerEvent);
impl_from_family!(DataFrame, Data);
impl_from_family!(LogFrame, Log);
impl_from_family!(TelemetryFrame, Telemetry);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::messages::samples;

    /// One message of every family, in [`FrameKind`] order.
    fn one_per_family() -> Vec<AnyMessage> {
        vec![
            samples::control_requests().unwrap()[0].clone().into(),
            samples::control_replies().unwrap()[0].clone().into(),
            samples::coordinator_events().unwrap()[0].clone().into(),
            samples::daemon_events().unwrap()[0].clone().into(),
            samples::node_requests().unwrap()[0].clone().into(),
            samples::node_events().unwrap()[0].clone().into(),
            samples::peer_events().unwrap()[0].clone().into(),
            samples::sample_data_frame().unwrap().into(),
            samples::sample_log_frame().unwrap().into(),
            samples::sample_telemetry_frame().unwrap().into(),
        ]
    }

    #[test]
    fn the_variant_set_mirrors_the_frame_kinds() {
        let messages = one_per_family();
        assert_eq!(messages.len(), FrameKind::ALL.len());
        for (message, &kind) in messages.iter().zip(FrameKind::ALL) {
            assert_eq!(message.kind(), kind);
        }
    }

    #[test]
    fn every_family_round_trips_through_the_dispatcher() {
        let limits = FrameLimits::network();
        for message in one_per_family() {
            let bytes = message.to_frame(FrameFlags::CRC, &limits).unwrap();
            let decoded = AnyMessage::from_bytes(&bytes, &limits).unwrap();
            assert!(decoded.bitwise_eq(&message), "{message}");
            assert_eq!(decoded.kind(), message.kind());
            assert_eq!(decoded.variant_name(), message.variant_name());
            assert_eq!(decoded.variant_index(), message.variant_index());
        }
    }

    #[test]
    fn re_encoding_is_byte_identical() {
        let limits = FrameLimits::uds();
        for message in one_per_family() {
            let original = message.to_frame(FrameFlags::EMPTY, &limits).unwrap();
            let decoded = AnyMessage::from_bytes(&original, &limits).unwrap();
            assert_eq!(
                decoded.to_frame(FrameFlags::EMPTY, &limits).unwrap(),
                original
            );
        }
    }

    #[test]
    fn the_control_plane_split_matches_the_frame_kind() {
        for message in one_per_family() {
            assert_eq!(
                message.is_control_plane(),
                message.kind().is_control_plane()
            );
        }
    }

    #[test]
    fn payload_accounting_sees_the_bulk_families() {
        let data: AnyMessage = samples::sample_data_frame().unwrap().into();
        assert_eq!(data.payload_len(), 4);

        let peer: AnyMessage = samples::peer_events().unwrap()[3].clone().into();
        assert_eq!(peer.payload_len(), 4);

        let log: AnyMessage = samples::sample_log_frame().unwrap().into();
        assert_eq!(log.payload_len(), 0);
    }

    #[test]
    fn display_names_the_family_and_the_message() {
        for message in one_per_family() {
            let text = message.to_string();
            assert!(text.starts_with(message.kind().as_str()), "{text}");
            assert!(text.len() > message.kind().as_str().len() + 2, "{text}");
        }
    }

    #[test]
    fn a_frame_of_the_wrong_shape_is_an_error_not_a_panic() {
        // A Control frame whose payload is a PeerEvent decodes as garbage or
        // fails; either way it must not panic.
        let limits = FrameLimits::uds();
        let payload =
            crate::codec::WireEncode::encode_to_vec(&samples::peer_events().unwrap()[0]).unwrap();
        let bytes =
            crate::frame::encode_frame(FrameKind::Control, FrameFlags::EMPTY, &payload, &limits)
                .unwrap();
        let _ = AnyMessage::from_bytes(&bytes, &limits);
    }
}
