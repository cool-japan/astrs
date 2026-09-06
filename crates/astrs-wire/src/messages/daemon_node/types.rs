//! The vocabulary of the daemon ↔ node leg: registrations, output payload
//! placement and extension-table keys.

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::error::{IdError, IdKind};
use crate::ids::{DataId, DataflowId, NodeId, validate_name};
use crate::version::AstrsVersion;

/// What a node tells its daemon when it attaches.
///
/// A node spawned by the daemon already has its configuration — the daemon put
/// it in `ASTRS_NODE_CONFIG` (§24.2) — and registers only to prove it is alive
/// and to claim its generation. A `path: dynamic` node (§8.3) registers to
/// *ask* for one, declaring the ports it intends to use.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataflowId, NodeHandshake, NodeId};
///
/// let handshake = NodeHandshake::new(DataflowId::from_u128(1), NodeId::new("camera")?, 3)
///     .with_pid(4_242);
/// assert_eq!(handshake.generation, 3);
/// assert!(!handshake.dynamic);
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeHandshake {
    /// The dataflow the node belongs to.
    pub dataflow: DataflowId,
    /// The node's id within that dataflow.
    pub node: NodeId,
    /// The incarnation the node believes it is. A daemon that disagrees
    /// refuses the registration rather than letting a stale process write into
    /// a live segment (§6.2).
    pub generation: u64,
    /// The node's operating-system process id, when it knows it.
    pub pid: Option<u32>,
    /// The node's AstRS release, so a version skew is visible in `astrs list`
    /// rather than as mysterious decode errors.
    pub version: AstrsVersion,
    /// Whether this node attached itself rather than being spawned (§8.3).
    pub dynamic: bool,
    /// The inputs the node intends to read. Ignored for a spawned node, whose
    /// wiring the daemon already knows; required from a dynamic one.
    pub inputs: Vec<DataId>,
    /// The outputs the node intends to write, under the same rule.
    pub outputs: Vec<DataId>,
}

impl NodeHandshake {
    /// A registration for a spawned node.
    #[must_use]
    pub fn new(dataflow: DataflowId, node: NodeId, generation: u64) -> Self {
        Self {
            dataflow,
            node,
            generation,
            pid: None,
            version: AstrsVersion::current(),
            dynamic: false,
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    /// A registration for a node that attached itself.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{DataId, DataflowId, NodeHandshake, NodeId};
    ///
    /// let handshake = NodeHandshake::dynamic(DataflowId::from_u128(1), NodeId::new("probe")?)
    ///     .with_outputs([DataId::new("samples")?]);
    /// assert!(handshake.dynamic);
    /// assert_eq!(handshake.generation, 0, "the daemon assigns the generation");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn dynamic(dataflow: DataflowId, node: NodeId) -> Self {
        Self {
            dynamic: true,
            ..Self::new(dataflow, node, 0)
        }
    }

    /// Records the node's process id.
    #[must_use]
    pub const fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    /// Declares the inputs the node will read.
    #[must_use]
    pub fn with_inputs<I: IntoIterator<Item = DataId>>(mut self, inputs: I) -> Self {
        self.inputs = inputs.into_iter().collect();
        self
    }

    /// Declares the outputs the node will write.
    #[must_use]
    pub fn with_outputs<I: IntoIterator<Item = DataId>>(mut self, outputs: I) -> Self {
        self.outputs = outputs.into_iter().collect();
        self
    }

    /// Whether the node declared any ports.
    #[must_use]
    pub fn declares_ports(&self) -> bool {
        !self.inputs.is_empty() || !self.outputs.is_empty()
    }
}

impl fmt::Display for NodeHandshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} generation {}{}",
            self.dataflow,
            self.node,
            self.generation,
            if self.dynamic { " (dynamic)" } else { "" }
        )
    }
}

/// Where the bytes of an outgoing message live.
///
/// Blueprint §6.2 splits sends by size: a payload below the zero-copy
/// threshold (4 KiB by default) *rides the control channel*, and one above it
/// is written straight into a shared-memory slot the node allocated, so the
/// daemon only has to be told which slot. Modelling that as one enum keeps the
/// two paths on one code path in every consumer of this crate.
///
/// # Examples
///
/// ```
/// use astrs_wire::OutputPayload;
///
/// let small = OutputPayload::inline(vec![1, 2, 3]);
/// assert_eq!(small.len(), 3);
/// assert!(!small.is_zero_copy());
///
/// let large = OutputPayload::Shm {
///     segment: "astrs/df/camera/3/image".to_owned(),
///     slot: 7,
///     len: 4 << 20,
///     generation: 3,
/// };
/// assert!(large.is_zero_copy());
/// assert_eq!(large.len(), 4 << 20);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputPayload {
    /// The bytes themselves, carried on the control channel.
    #[oxicode(variant = 0)]
    Inline {
        /// The payload — an Arrow IPC stream (§6.1), opaque here.
        bytes: Vec<u8>,
    },
    /// A reference to a slot the node already wrote into.
    #[oxicode(variant = 1)]
    Shm {
        /// The segment name, which embeds
        /// `{dataflow_id}/{node_id}/{generation}` (§6.2).
        segment: String,
        /// The slot index within the segment's ring.
        slot: u32,
        /// How many payload bytes the slot holds.
        len: u64,
        /// The producer incarnation that owns the segment, so a daemon can
        /// reject a reference minted before a restart.
        generation: u64,
    },
}

impl OutputPayload {
    /// An inline payload.
    #[must_use]
    pub const fn inline(bytes: Vec<u8>) -> Self {
        Self::Inline { bytes }
    }

    /// An empty inline payload.
    #[must_use]
    pub const fn empty() -> Self {
        Self::Inline { bytes: Vec::new() }
    }

    /// Whether the payload is a shared-memory reference rather than bytes.
    #[must_use]
    pub const fn is_zero_copy(&self) -> bool {
        matches!(self, Self::Shm { .. })
    }

    /// The payload length in bytes, however it is carried.
    #[must_use]
    pub fn len(&self) -> u64 {
        match self {
            Self::Inline { bytes } => bytes.len() as u64,
            Self::Shm { len, .. } => *len,
        }
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The inline bytes, if the payload is carried inline.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Inline { bytes } => Some(bytes),
            Self::Shm { .. } => None,
        }
    }

    /// The segment name, if the payload is a shared-memory reference.
    #[must_use]
    pub fn segment(&self) -> Option<&str> {
        match self {
            Self::Inline { .. } => None,
            Self::Shm { segment, .. } => Some(segment),
        }
    }
}

impl fmt::Display for OutputPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline { bytes } => write!(f, "{} inline byte(s)", bytes.len()),
            Self::Shm {
                segment, slot, len, ..
            } => write!(f, "{len} byte(s) in {segment} slot {slot}"),
        }
    }
}

/// The namespace half of an [`ExtensionKey`].
///
/// The extension table is a small key/value store the daemon keeps on a node's
/// behalf, and it is shared by unrelated users: application data, pinned host
/// buffers, GPU IPC handles. Namespacing keys keeps a node's own key from
/// colliding with a runtime-managed one, and lets the daemon apply a different
/// reclamation policy to each class (§7.3 "extension & pinned-memory").
///
/// # Examples
///
/// ```
/// use astrs_wire::ExtensionNamespace;
///
/// assert!(ExtensionNamespace::PinnedMemory.is_reserved());
/// assert!(!ExtensionNamespace::User.is_reserved());
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExtensionNamespace {
    /// Application data, owned by the node.
    #[default]
    #[oxicode(variant = 0)]
    User,
    /// Page-locked host buffers the daemon allocated for the node.
    #[oxicode(variant = 1)]
    PinnedMemory,
    /// Accelerator IPC handles (CUDA, Level Zero) shared between nodes.
    #[oxicode(variant = 2)]
    GpuHandle,
    /// Runtime bookkeeping, not writable by nodes.
    #[oxicode(variant = 3)]
    Internal,
}

impl ExtensionNamespace {
    /// Every namespace, in wire-index order.
    pub const ALL: &'static [Self] = &[
        Self::User,
        Self::PinnedMemory,
        Self::GpuHandle,
        Self::Internal,
    ];

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::PinnedMemory => "pinned_memory",
            Self::GpuHandle => "gpu_handle",
            Self::Internal => "internal",
        }
    }

    /// Whether the daemon, rather than the node, owns entries here.
    ///
    /// A node may read a reserved entry it was given, but a daemon refuses an
    /// `ExtStore` into one.
    #[must_use]
    pub const fn is_reserved(self) -> bool {
        !matches!(self, Self::User)
    }
}

impl fmt::Display for ExtensionNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The maximum length of an extension key's name, in bytes.
pub const MAX_EXTENSION_NAME_LEN: usize = 255;

/// A key in the daemon's extension table.
///
/// # Examples
///
/// ```
/// use astrs_wire::{ExtensionKey, ExtensionNamespace};
///
/// let key = ExtensionKey::user("calibration")?;
/// assert_eq!(key.namespace, ExtensionNamespace::User);
/// assert_eq!(key.to_string(), "user/calibration");
/// assert!(ExtensionKey::user("").is_err());
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode)]
pub struct ExtensionKey {
    /// Which class of entry this is.
    pub namespace: ExtensionNamespace,
    /// The name within the namespace.
    pub name: String,
}

impl ExtensionKey {
    /// A validated key in the given namespace.
    ///
    /// # Errors
    ///
    /// [`IdError`] if the name is empty, longer than
    /// [`MAX_EXTENSION_NAME_LEN`] bytes, or contains a character outside the
    /// identifier grammar.
    pub fn new(namespace: ExtensionNamespace, name: impl Into<String>) -> Result<Self, IdError> {
        let name = name.into();
        validate_name(EXTENSION_NAME_KIND, &name, MAX_EXTENSION_NAME_LEN)?;
        Ok(Self { namespace, name })
    }

    /// A validated key in the [`ExtensionNamespace::User`] namespace.
    ///
    /// # Errors
    ///
    /// As [`ExtensionKey::new`].
    pub fn user(name: impl Into<String>) -> Result<Self, IdError> {
        Self::new(ExtensionNamespace::User, name)
    }

    /// Whether a node is allowed to write this key.
    #[must_use]
    pub const fn is_writable_by_node(&self) -> bool {
        !self.namespace.is_reserved()
    }
}

impl fmt::Display for ExtensionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

impl oxicode::de::Decode for ExtensionKey {
    /// Validates the name during decoding, so a peer cannot inject an
    /// unbounded or malformed key by encoding it directly into a frame.
    ///
    /// Not generic over `oxicode`'s decode context, for the same reason
    /// [`crate::Metadata`] is not: the derive emits context-free
    /// implementations for the fields this reads.
    fn decode<D: oxicode::de::Decoder<Context = ()>>(
        decoder: &mut D,
    ) -> Result<Self, oxicode::error::Error> {
        let namespace = ExtensionNamespace::decode(decoder)?;
        let name = String::decode(decoder)?;
        validate_name(EXTENSION_NAME_KIND, &name, MAX_EXTENSION_NAME_LEN)
            .map_err(|err| crate::error::codec_invalid(err.to_string()))?;
        Ok(Self { namespace, name })
    }
}

/// The identifier kind extension names are validated as.
pub(crate) const EXTENSION_NAME_KIND: IdKind = IdKind::Name;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode, round_trip};

    #[test]
    fn a_spawned_handshake_differs_from_a_dynamic_one() {
        let dataflow = DataflowId::from_u128(1);
        let spawned = NodeHandshake::new(dataflow, NodeId::new("camera").unwrap(), 3).with_pid(42);
        assert!(!spawned.dynamic);
        assert_eq!(spawned.pid, Some(42));
        assert!(!spawned.declares_ports());
        assert_eq!(round_trip(&spawned).unwrap(), spawned);

        let attached = NodeHandshake::dynamic(dataflow, NodeId::new("probe").unwrap())
            .with_inputs([DataId::new("frames").unwrap()])
            .with_outputs([DataId::new("samples").unwrap()]);
        assert!(attached.dynamic);
        assert_eq!(attached.generation, 0);
        assert!(attached.declares_ports());
        assert!(attached.to_string().contains("dynamic"));
    }

    #[test]
    fn output_payloads_report_their_size_on_both_paths() {
        let inline = OutputPayload::inline(vec![7; 100]);
        assert_eq!(inline.len(), 100);
        assert!(!inline.is_zero_copy());
        assert_eq!(inline.bytes().map(<[u8]>::len), Some(100));
        assert_eq!(inline.segment(), None);
        assert!(!inline.is_empty());
        assert!(OutputPayload::empty().is_empty());

        let shm = OutputPayload::Shm {
            segment: "astrs/df/camera/3/image".to_owned(),
            slot: 7,
            len: 4 << 20,
            generation: 3,
        };
        assert!(shm.is_zero_copy());
        assert_eq!(shm.len(), 4 << 20);
        assert_eq!(shm.bytes(), None);
        assert_eq!(shm.segment(), Some("astrs/df/camera/3/image"));
    }

    #[test]
    fn output_payload_indices_are_frozen() {
        assert_eq!(OutputPayload::empty().encode_to_vec().unwrap()[0], 0);
        let shm = OutputPayload::Shm {
            segment: "s".to_owned(),
            slot: 0,
            len: 0,
            generation: 0,
        };
        assert_eq!(shm.encode_to_vec().unwrap()[0], 1);
        assert_eq!(round_trip(&shm).unwrap(), shm);
    }

    #[test]
    fn extension_namespaces_are_frozen_and_classified() {
        for (index, namespace) in ExtensionNamespace::ALL.iter().enumerate() {
            assert_eq!(usize::from(namespace.encode_to_vec().unwrap()[0]), index);
            assert!(!namespace.as_str().is_empty());
        }
        assert!(!ExtensionNamespace::User.is_reserved());
        assert!(ExtensionNamespace::PinnedMemory.is_reserved());
        assert!(ExtensionNamespace::GpuHandle.is_reserved());
        assert!(ExtensionNamespace::Internal.is_reserved());
        assert_eq!(ExtensionNamespace::default(), ExtensionNamespace::User);
    }

    #[test]
    fn extension_keys_validate_their_names() {
        let key = ExtensionKey::user("calibration").unwrap();
        assert_eq!(key.to_string(), "user/calibration");
        assert!(key.is_writable_by_node());
        assert_eq!(round_trip(&key).unwrap(), key);

        assert!(ExtensionKey::user("").is_err());
        assert!(ExtensionKey::user("has space").is_err());
        assert!(ExtensionKey::user("x".repeat(MAX_EXTENSION_NAME_LEN + 1)).is_err());
        assert!(ExtensionKey::user("x".repeat(MAX_EXTENSION_NAME_LEN)).is_ok());

        let pinned = ExtensionKey::new(ExtensionNamespace::PinnedMemory, "frame-pool").unwrap();
        assert!(!pinned.is_writable_by_node());
    }

    #[test]
    fn decoding_rejects_a_forged_extension_key() {
        // Encode a valid key, then splice in an illegal name of the same
        // length: the decoder must refuse it rather than trusting the wire.
        let key = ExtensionKey::user("aaaaa").unwrap();
        let mut bytes = key.encode_to_vec().unwrap();
        let position = bytes.len() - 1;
        bytes[position] = b' ';
        assert!(ExtensionKey::decode_exact(&bytes).is_err());

        // And an over-long one.
        let long = ExtensionKey {
            namespace: ExtensionNamespace::User,
            name: "x".repeat(MAX_EXTENSION_NAME_LEN + 1),
        };
        let bytes = long.encode_to_vec().unwrap();
        assert!(ExtensionKey::decode_exact(&bytes).is_err());
    }

    #[test]
    fn the_extension_name_kind_is_the_identifier_grammar() {
        assert_eq!(EXTENSION_NAME_KIND, IdKind::Name);
    }
}
