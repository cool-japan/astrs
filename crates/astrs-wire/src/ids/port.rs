//! [`PortRef`] — one endpoint of a graph edge.
//!
//! A manifest input is written `node/output` in its short form (blueprint
//! §8.3), and that is exactly what a `PortRef` denotes: the node that owns the
//! port and the port's own id. Because neither [`NodeId`] nor [`DataId`]
//! admits `/`, the text form parses unambiguously by splitting on the single
//! separator.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::PortRef;
//!
//! let port: PortRef = "camera/image".parse()?;
//! assert_eq!(port.node().as_str(), "camera");
//! assert_eq!(port.port().as_str(), "image");
//! assert_eq!(port.to_string(), "camera/image");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;
use core::str::FromStr;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{IdError, IdKind, preview};
use crate::ids::name::{DataId, NodeId};

/// The separator between the node id and the port id in the text form.
pub const PORT_REF_SEPARATOR: char = '/';

/// A reference to one input or output port of one node.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataId, NodeId, PortRef};
///
/// let port = PortRef::new(NodeId::new("detector")?, DataId::new("boxes")?);
/// assert_eq!(port.to_string(), "detector/boxes");
/// assert_eq!("detector/boxes".parse::<PortRef>()?, port);
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Encode, Decode)]
pub struct PortRef {
    /// The node that owns the port.
    node: NodeId,
    /// The port's own id.
    port: DataId,
}

impl PortRef {
    /// Builds a port reference from its two halves.
    #[must_use]
    pub const fn new(node: NodeId, port: DataId) -> Self {
        Self { node, port }
    }

    /// Builds a port reference from unvalidated strings.
    ///
    /// # Errors
    ///
    /// [`IdError`] if either half fails its grammar.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::PortRef;
    ///
    /// assert!(PortRef::from_parts("camera", "image").is_ok());
    /// assert!(PortRef::from_parts("camera", "im age").is_err());
    /// ```
    pub fn from_parts(node: &str, port: &str) -> Result<Self, IdError> {
        Ok(Self {
            node: NodeId::new(node)?,
            port: DataId::new(port)?,
        })
    }

    /// The node that owns the port.
    #[must_use]
    pub const fn node(&self) -> &NodeId {
        &self.node
    }

    /// The port's own id.
    #[must_use]
    pub const fn port(&self) -> &DataId {
        &self.port
    }

    /// Consumes the reference and returns its two halves.
    #[must_use]
    pub fn into_parts(self) -> (NodeId, DataId) {
        (self.node, self.port)
    }
}

impl fmt::Display for PortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{PORT_REF_SEPARATOR}{}", self.node, self.port)
    }
}

impl fmt::Debug for PortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PortRef({self})")
    }
}

impl FromStr for PortRef {
    type Err = IdError;

    /// Parses the `node/port` short form.
    ///
    /// # Errors
    ///
    /// [`IdError::Malformed`] when there is no separator or more than one;
    /// [`IdError`] from the halves' own validation otherwise.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let malformed = |reason: &'static str| IdError::Malformed {
            kind: IdKind::Name,
            value: preview(text),
            reason,
        };
        let (node, port) = text
            .split_once(PORT_REF_SEPARATOR)
            .ok_or_else(|| malformed("expected the form `node/port`"))?;
        if port.contains(PORT_REF_SEPARATOR) {
            return Err(malformed("more than one '/' in a port reference"));
        }
        Self::from_parts(node, port)
    }
}

impl TryFrom<&str> for PortRef {
    type Error = IdError;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl<'de> Deserialize<'de> for PortRef {
    /// Accepts both the short string form (`"camera/image"`) and the explicit
    /// map form (`{"node": "camera", "port": "image"}`).
    ///
    /// Manifests use the short form; machine-generated JSON tends to use the
    /// map. Accepting both keeps `--json` output round-trippable without
    /// forcing either style on a human editor.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Short(String),
            Long {
                /// The node that owns the port.
                node: NodeId,
                /// The port's own id.
                port: DataId,
            },
        }

        match Repr::deserialize(deserializer)? {
            Repr::Short(text) => text.parse().map_err(serde::de::Error::custom),
            Repr::Long { node, port } => Ok(Self { node, port }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn text_form_round_trips() {
        for text in [
            "camera/image",
            "perception.detector/boxes",
            "a/b",
            "node-1/out_0",
        ] {
            let port: PortRef = text.parse().unwrap();
            assert_eq!(port.to_string(), text);
        }
    }

    #[test]
    fn accessors_expose_both_halves() {
        let port = PortRef::from_parts("camera", "image").unwrap();
        assert_eq!(port.node().as_str(), "camera");
        assert_eq!(port.port().as_str(), "image");
        let (node, data) = port.clone().into_parts();
        assert_eq!(PortRef::new(node, data), port);
        assert_eq!(format!("{port:?}"), "PortRef(camera/image)");
    }

    #[test]
    fn missing_or_extra_separators_are_rejected() {
        for bad in ["camera", "camera/image/extra", "/image", "camera/"] {
            assert!(bad.parse::<PortRef>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn invalid_halves_are_rejected() {
        assert!("cam era/image".parse::<PortRef>().is_err());
        assert!("camera/im age".parse::<PortRef>().is_err());
    }

    #[test]
    fn codec_round_trips() {
        let port = PortRef::from_parts("planner", "path").unwrap();
        let bytes = port.encode_to_vec().unwrap();
        assert_eq!(PortRef::decode_exact(&bytes).unwrap(), port);
    }

    #[test]
    fn decoding_validates_both_halves() {
        let forged = ("bad node".to_owned(), "port".to_owned())
            .encode_to_vec()
            .unwrap();
        assert!(PortRef::decode_exact(&forged).is_err());
    }

    #[test]
    fn serde_writes_the_map_form_and_reads_both() {
        let port = PortRef::from_parts("camera", "image").unwrap();
        let json = serde_json::to_string(&port).unwrap();
        assert_eq!(json, r#"{"node":"camera","port":"image"}"#);
        assert_eq!(serde_json::from_str::<PortRef>(&json).unwrap(), port);
        assert_eq!(
            serde_json::from_str::<PortRef>("\"camera/image\"").unwrap(),
            port
        );
        assert!(serde_json::from_str::<PortRef>("\"camera\"").is_err());
    }

    #[test]
    fn try_from_matches_parse() {
        assert_eq!(
            PortRef::try_from("a/b").unwrap(),
            "a/b".parse::<PortRef>().unwrap()
        );
    }

    #[test]
    fn ordering_is_by_node_then_port() {
        let mut ports = [
            PortRef::from_parts("b", "a").unwrap(),
            PortRef::from_parts("a", "z").unwrap(),
            PortRef::from_parts("a", "a").unwrap(),
        ];
        ports.sort();
        assert_eq!(ports[0].to_string(), "a/a");
        assert_eq!(ports[1].to_string(), "a/z");
        assert_eq!(ports[2].to_string(), "b/a");
    }
}
