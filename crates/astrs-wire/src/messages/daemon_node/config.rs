//! [`NodeConfig`] — the `ASTRS_NODE_CONFIG` handshake blob (§4.2, §24.2).
//!
//! > *Node — one OS process per manifest node, spawned by the daemon with a
//! > scrubbed environment and an `ASTRS_NODE_CONFIG` handshake blob (oxicode,
//! > base64) — never YAML-in-env.*
//!
//! This is that blob's type. It lives in `astrs-wire` rather than in
//! `astrs-daemon` because it is a contract with two ends: the daemon writes it
//! (§16 — daemon-owned variables are applied last, so a manifest can never
//! forge one) and the node API reads it back in `Node::init`. One type, one
//! encoding, one place to change it.
//!
//! It is *not* a frame family: it never travels in a frame, it travels in an
//! environment variable, so it has no [`crate::FrameKind`] and no entry in the
//! §24.1 snapshot. It is still `#[non_exhaustive]`-friendly in the way that
//! matters — every field is appended, never renumbered, and `oxicode` decodes
//! a struct positionally, so a node built against an older `astrs-wire` sees a
//! trailing-bytes error rather than a silently misread configuration. That is
//! why [`NodeConfig::protocol`] is checked before anything else is trusted.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{
//!     AuthToken, DaemonId, DataflowId, NodeConfig, NodeId, NodeSource, NodeSpawnSpec,
//! };
//!
//! let spec = NodeSpawnSpec::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     0,
//!     NodeSource::Executable { path: "./camera".into() },
//! );
//! let config = NodeConfig::new(spec, DaemonId::generate(None), AuthToken::ZERO)
//!     .with_endpoint("uds:///run/astrs/daemon.sock");
//!
//! let blob = config.to_env_value()?;
//! assert!(!blob.contains('\0'), "an env value is a C string");
//! assert_eq!(NodeConfig::from_env_value(&blob)?, config);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::auth::AuthToken;
use crate::base64::{self, Base64Error};
use crate::codec::{WireDecode, WireEncode};
use crate::common::node::NodeSpawnSpec;
use crate::error::WireError;
use crate::handshake::limits::NegotiatedLimits;
use crate::ids::{DaemonId, DataflowId, NodeId};
use crate::version::PROTOCOL_VERSION;

/// The environment variable the blob travels in (§24.2).
pub const ENV_NODE_CONFIG: &str = "ASTRS_NODE_CONFIG";

/// The environment variable carrying the orphan-guard parent pid (§4.2).
///
/// `astrs run` embeds the daemon in the CLI process; a node that outlives its
/// parent is an orphan, and this is how it notices.
pub const ENV_RUN_PARENT_PID: &str = "ASTRS_RUN_PARENT_PID";

/// The default zero-copy threshold in bytes (§24.2).
pub const DEFAULT_ZERO_COPY_THRESHOLD: u64 = 4_096;

/// Everything a spawned node needs before it has spoken to anybody.
///
/// Compare [`crate::NodeHandshake`], which is what the node then *sends*: this
/// is the daemon's half of the introduction, delivered out of band because
/// there is no connection yet over which to deliver it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeConfig {
    /// The protocol version the spawning daemon speaks.
    ///
    /// Checked first by [`NodeConfig::from_env_value`]'s callers: a node from a
    /// different release should refuse loudly, not misinterpret the rest.
    pub protocol: u16,
    /// The node's effective specification, generation included.
    pub spec: NodeSpawnSpec,
    /// The daemon that spawned it.
    pub daemon: DaemonId,
    /// Where to dial, in preference order (`uds:///…`, `tcp://127.0.0.1:7408`).
    ///
    /// Strings rather than a typed address because `astrs-wire` sits below
    /// `astrs-transport` in the layer stack (§4.1) and must not depend upward;
    /// `astrs_transport::TransportAddr` parses exactly this form.
    pub endpoints: Vec<String>,
    /// The cluster auth token to present in `Hello` (§16).
    pub auth: AuthToken,
    /// The limits the daemon will propose at handshake, so a node can size its
    /// buffers before connecting.
    pub limits: NegotiatedLimits,
    /// Payloads at or above this many bytes should go through shared memory
    /// (§24.2 — `ASTRS_ZERO_COPY_THRESHOLD`).
    pub zero_copy_threshold: u64,
    /// The SHM broker socket, when the daemon brokers a same-host plane.
    ///
    /// `None` in a daemon build without the shared-memory plane wired up, and
    /// on a platform where it is unsupported; a node then stays on the
    /// daemon-mediated reliable path.
    pub shm_broker: Option<String>,
    /// The dataflow's working directory, already resolved to an absolute path.
    pub working_dir: Option<String>,
    /// Whether the daemon is running the dataflow in deterministic mode (§14).
    pub deterministic: bool,
}

impl NodeConfig {
    /// A configuration for `spec`, spawned by `daemon`, authenticating with
    /// `auth`.
    ///
    /// Everything else takes its §24.2 default; the `with_*` builders adjust.
    #[must_use]
    pub fn new(spec: NodeSpawnSpec, daemon: DaemonId, auth: AuthToken) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            spec,
            daemon,
            endpoints: Vec::new(),
            auth,
            limits: NegotiatedLimits::uds(),
            zero_copy_threshold: DEFAULT_ZERO_COPY_THRESHOLD,
            shm_broker: None,
            working_dir: None,
            deterministic: false,
        }
    }

    /// Appends one dial address.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoints.push(endpoint.into());
        self
    }

    /// Replaces the dial addresses.
    #[must_use]
    pub fn with_endpoints<I, S>(mut self, endpoints: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.endpoints = endpoints.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the handshake limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: NegotiatedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the zero-copy threshold.
    #[must_use]
    pub const fn with_zero_copy_threshold(mut self, bytes: u64) -> Self {
        self.zero_copy_threshold = bytes;
        self
    }

    /// Sets the SHM broker socket path.
    #[must_use]
    pub fn with_shm_broker(mut self, path: impl Into<String>) -> Self {
        self.shm_broker = Some(path.into());
        self
    }

    /// Sets the resolved working directory.
    #[must_use]
    pub fn with_working_dir(mut self, path: impl Into<String>) -> Self {
        self.working_dir = Some(path.into());
        self
    }

    /// Marks the run deterministic (§14).
    #[must_use]
    pub const fn with_deterministic(mut self, deterministic: bool) -> Self {
        self.deterministic = deterministic;
        self
    }

    /// The dataflow this node belongs to.
    #[must_use]
    pub const fn dataflow(&self) -> DataflowId {
        self.spec.dataflow
    }

    /// The node's identifier.
    #[must_use]
    pub const fn node(&self) -> &NodeId {
        &self.spec.node
    }

    /// The node's incarnation counter (§12).
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.spec.generation
    }

    /// Whether the protocol version in the blob is one this build speaks.
    #[must_use]
    pub const fn protocol_is_supported(&self) -> bool {
        crate::version::supports_protocol(self.protocol)
    }

    /// Encodes the blob as an environment-variable value.
    ///
    /// oxicode, then base64 — an environment value is a NUL-terminated C
    /// string, so the binary encoding cannot travel raw.
    ///
    /// # Errors
    ///
    /// [`WireError::Codec`] if encoding fails, which for this type means an
    /// allocation failure.
    pub fn to_env_value(&self) -> Result<String, WireError> {
        Ok(base64::encode(&self.encode_to_vec()?))
    }

    /// Decodes a blob produced by [`NodeConfig::to_env_value`].
    ///
    /// # Errors
    ///
    /// [`NodeConfigError::Base64`] for a corrupt text form, or
    /// [`NodeConfigError::Decode`] when the bytes are not a `NodeConfig` this
    /// build understands.
    pub fn from_env_value(value: &str) -> Result<Self, NodeConfigError> {
        let bytes = base64::decode(value).map_err(NodeConfigError::Base64)?;
        Self::decode_exact(&bytes).map_err(NodeConfigError::Decode)
    }
}

impl fmt::Display for NodeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} generation {} via {}",
            self.spec.dataflow,
            self.spec.node,
            self.spec.generation,
            if self.endpoints.is_empty() {
                "<no endpoint>"
            } else {
                self.endpoints
                    .first()
                    .map_or("<no endpoint>", String::as_str)
            }
        )
    }
}

/// Why an `ASTRS_NODE_CONFIG` value could not be read back.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NodeConfigError {
    /// The value is not well-formed base64.
    #[error("ASTRS_NODE_CONFIG is not valid base64: {0}")]
    Base64(#[source] Base64Error),
    /// The decoded bytes are not a `NodeConfig`.
    #[error("ASTRS_NODE_CONFIG is not a valid node configuration: {0}")]
    Decode(#[source] WireError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::common::node::{InputSpec, NodeSource, OutputSpec};
    use crate::ids::{DataId, PortRef};

    fn spec() -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(42),
            NodeId::new("detector").unwrap(),
            7,
            NodeSource::Executable {
                path: "./target/release/detector".into(),
            },
        )
        .with_input(InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::new(
                NodeId::new("camera").unwrap(),
                DataId::new("image").unwrap(),
            ),
        ))
        .with_output(OutputSpec::new(DataId::new("detections").unwrap()))
    }

    fn config() -> NodeConfig {
        NodeConfig::new(spec(), DaemonId::generate(None), AuthToken::ZERO)
            .with_endpoints(["uds:///tmp/astrs/daemon.sock", "tcp://127.0.0.1:7408"])
            .with_zero_copy_threshold(8_192)
            .with_shm_broker("/tmp/astrs/shm.sock")
            .with_working_dir("/workspace")
            .with_deterministic(true)
    }

    #[test]
    fn a_config_round_trips_through_an_env_value() {
        let original = config();
        let value = original.to_env_value().unwrap();
        assert_eq!(NodeConfig::from_env_value(&value).unwrap(), original);
    }

    #[test]
    fn an_env_value_is_a_valid_c_string() {
        let value = config().to_env_value().unwrap();
        assert!(!value.contains('\0'));
        assert!(value.is_ascii());
        assert!(!value.contains('\n'));
    }

    #[test]
    fn the_current_protocol_is_stamped_and_recognized() {
        let config = config();
        assert_eq!(config.protocol, PROTOCOL_VERSION);
        assert!(config.protocol_is_supported());

        let mut future = config;
        future.protocol = u16::MAX;
        assert!(!future.protocol_is_supported());
    }

    #[test]
    fn accessors_read_through_to_the_spec() {
        let config = config();
        assert_eq!(config.dataflow(), DataflowId::from_u128(42));
        assert_eq!(config.node().as_str(), "detector");
        assert_eq!(config.generation(), 7);
    }

    #[test]
    fn a_corrupt_blob_is_reported_not_guessed() {
        assert!(matches!(
            NodeConfig::from_env_value("not base64!"),
            Err(NodeConfigError::Base64(_))
        ));
        assert!(matches!(
            NodeConfig::from_env_value(&base64::encode(b"garbage")),
            Err(NodeConfigError::Decode(_))
        ));
    }

    #[test]
    fn a_truncated_blob_is_refused_rather_than_half_read() {
        let value = config().to_env_value().unwrap();
        let bytes = base64::decode(&value).unwrap();
        let truncated = base64::encode(&bytes[..bytes.len() / 2]);
        assert!(NodeConfig::from_env_value(&truncated).is_err());
    }

    #[test]
    fn defaults_follow_the_appendix() {
        let config = NodeConfig::new(spec(), DaemonId::generate(None), AuthToken::ZERO);
        assert_eq!(config.zero_copy_threshold, DEFAULT_ZERO_COPY_THRESHOLD);
        assert_eq!(config.zero_copy_threshold, 4_096);
        assert!(config.endpoints.is_empty());
        assert!(config.shm_broker.is_none());
        assert!(!config.deterministic);
        assert!(!config.limits.require_crc, "UDS legs may skip the checksum");
    }

    #[test]
    fn the_env_variable_names_are_the_documented_ones() {
        assert_eq!(ENV_NODE_CONFIG, "ASTRS_NODE_CONFIG");
        assert_eq!(ENV_RUN_PARENT_PID, "ASTRS_RUN_PARENT_PID");
    }

    #[test]
    fn display_names_the_node_and_its_endpoint() {
        let rendered = config().to_string();
        assert!(rendered.contains("detector"), "{rendered}");
        assert!(rendered.contains("generation 7"), "{rendered}");
        assert!(rendered.contains("uds://"), "{rendered}");

        let bare = NodeConfig::new(spec(), DaemonId::generate(None), AuthToken::ZERO).to_string();
        assert!(bare.contains("<no endpoint>"), "{bare}");
    }

    #[test]
    fn the_auth_token_survives_the_round_trip_exactly() {
        let token = AuthToken::parse_hex(&"ab".repeat(32)).unwrap();
        let config = NodeConfig::new(spec(), DaemonId::generate(None), token.clone());
        let decoded = NodeConfig::from_env_value(&config.to_env_value().unwrap()).unwrap();
        assert_eq!(decoded.auth.reveal_bytes(), token.reveal_bytes());
    }
}
