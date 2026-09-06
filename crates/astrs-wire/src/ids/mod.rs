//! The identifier vocabulary of the control plane.
//!
//! Every id AstRS puts on the wire is a distinct newtype with its own
//! validator, for two reasons:
//!
//! 1. **The compiler catches confusions.** A [`SessionId`] and a
//!    [`DataflowId`] are both sixteen bytes; nothing but the type system will
//!    notice one standing in for the other.
//! 2. **The wire cannot smuggle in a malformed value.** Validation runs on
//!    `FromStr`, `TryFrom`, `serde::Deserialize` *and* `oxicode::Decode`, so a
//!    peer cannot bypass the grammar by encoding a raw string into a frame.
//!    Each family also carries a byte cap enforced during decoding, so a
//!    forged length prefix cannot allocate without bound inside an otherwise
//!    legal frame.
//!
//! | Type | Shape | Wire cost |
//! |---|---|---|
//! | [`NodeId`], [`DataId`], [`OperatorId`], [`MachineName`], [`ParamKey`] | `[A-Za-z0-9_.-]+`, ≤ 255 B | varint length + bytes |
//! | [`TypeUrn`] | printable ASCII, ≤ 512 B | varint length + bytes |
//! | [`DataflowId`], [`SessionId`], [`BuildId`] | UUID (v7 when generated) | 16 B |
//! | [`DaemonId`] | optional machine label + UUID | 17 B + label |
//! | [`SubscriptionId`], [`RouteId`] | `u64` counter | 1–9 B (varint) |
//! | [`PortRef`] | `node/port` | both halves |
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DataflowId, DataId, NodeId, PortRef};
//!
//! let dataflow = DataflowId::generate();
//! let port = PortRef::new(NodeId::new("camera")?, DataId::new("image")?);
//! assert!(!dataflow.is_nil());
//! assert_eq!(port.to_string(), "camera/image");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

pub mod daemon;
pub mod name;
pub mod port;
pub mod route;
pub mod urn;
pub mod uuid_ids;

pub use daemon::{DAEMON_ID_SEPARATOR, DaemonId, UUID_TEXT_LEN};
pub use name::{
    DataId, MAX_NAME_LEN, MachineName, NodeId, OperatorId, ParamKey, is_name_char, validate_name,
};
pub use port::{PORT_REF_SEPARATOR, PortRef};
pub use route::RouteId;
pub use urn::{ANY_TYPE_URN, MAX_TYPE_URN_LEN, TypeUrn, is_type_urn_char};
pub use uuid_ids::{BuildId, DataflowId, SessionId, SubscriptionId, UUID_WIRE_LEN};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn every_id_family_survives_a_round_trip() {
        let node = NodeId::new("camera").unwrap();
        let data = DataId::new("image").unwrap();
        let operator = OperatorId::new("crop").unwrap();
        let machine = MachineName::new("robot-01").unwrap();
        let param = ParamKey::new("request_id").unwrap();
        let urn = TypeUrn::new("std/media/v1/Image").unwrap();
        let dataflow = DataflowId::generate();
        let session = SessionId::generate();
        let build = BuildId::generate();
        let daemon = DaemonId::generate(Some(machine.clone()));
        let subscription = SubscriptionId::new(9);
        let route = RouteId::new(3);
        let port = PortRef::new(node.clone(), data.clone());

        macro_rules! round_trip {
            ($value:expr, $ty:ty) => {{
                let bytes = $value.encode_to_vec().unwrap();
                assert_eq!(<$ty>::decode_exact(&bytes).unwrap(), $value);
            }};
        }

        round_trip!(node, NodeId);
        round_trip!(data, DataId);
        round_trip!(operator, OperatorId);
        round_trip!(machine, MachineName);
        round_trip!(param, ParamKey);
        round_trip!(urn, TypeUrn);
        round_trip!(dataflow, DataflowId);
        round_trip!(session, SessionId);
        round_trip!(build, BuildId);
        round_trip!(daemon, DaemonId);
        round_trip!(subscription, SubscriptionId);
        round_trip!(route, RouteId);
        round_trip!(port, PortRef);
    }

    #[test]
    fn name_shaped_ids_share_one_grammar() {
        let good = "ok-name.1_2";
        let bad = "not ok";
        assert!(NodeId::new(good).is_ok() && NodeId::new(bad).is_err());
        assert!(DataId::new(good).is_ok() && DataId::new(bad).is_err());
        assert!(OperatorId::new(good).is_ok() && OperatorId::new(bad).is_err());
        assert!(MachineName::new(good).is_ok() && MachineName::new(bad).is_err());
        assert!(ParamKey::new(good).is_ok() && ParamKey::new(bad).is_err());
    }

    #[test]
    fn validate_name_is_the_single_source_of_truth() {
        use crate::error::IdKind;

        assert!(validate_name(IdKind::Node, "a", MAX_NAME_LEN).is_ok());
        assert!(validate_name(IdKind::Node, "", MAX_NAME_LEN).is_err());
        assert!(validate_name(IdKind::Node, "ab", 1).is_err());
        assert!(is_name_char('a') && !is_name_char('/'));
        assert!(is_type_urn_char('/') && !is_type_urn_char(' '));
    }
}
