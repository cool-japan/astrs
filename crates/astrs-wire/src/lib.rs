//! AstRS wire protocol: one frame format for every leg of the control plane.
//!
//! This crate owns the single normative serialization surface of AstRS
//! (blueprint §7). It provides:
//!
//! - The framed codec — `magic "AS" | ver | flags | kind | len:u32 | payload |
//!   crc32c` — with oxicode payload encoding, optional lz4/zstd route
//!   compression flags, and decoding that rejects trailing bytes.
//! - The message families, one module per direction: `ControlRequest` /
//!   `ControlReply` (CLI ↔ coordinator), `CoordinatorEvent` / `DaemonEvent`
//!   (coordinator ↔ daemon), `NodeRequest` / `NodeEvent` (daemon ↔ node) and
//!   `PeerEvent` (daemon ↔ daemon).
//! - Handshake and version negotiation: `Hello` / `Welcome` / `Refused`,
//!   a single `PROTOCOL_VERSION`, negotiated limits, and role/feature flags.
//! - The `ASTRS_NODE_CONFIG` handshake blob ([`NodeConfig`]) a daemon hands a
//!   node it spawns, and the [`base64`] codec that lets it travel in an
//!   environment variable.
//! - The append-only compatibility contract: every wire enum is
//!   `#[non_exhaustive]`, encoded by stable variant index, extended only at
//!   the tail, and frozen by the protocol snapshot tests.

// Never called. The dependency exists solely so Cargo's feature unification
// forces `blake3/pure` (Rust-only) onto the copy oxicrypto pulls in — without
// it, blake3's build script compiles C SIMD kernels via `cc`, and the tree
// must stay C/C++-free in every feature combination (blueprint §18.1).
use blake3 as _;

pub mod auth;
pub mod base64;
pub mod codec;
pub mod common;
pub mod crc32c;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod ids;
pub mod io;
pub mod messages;
pub mod metadata;
pub mod version;

pub use auth::{AUTH_TOKEN_HEX_LEN, AUTH_TOKEN_LEN, AuthToken};
pub use base64::Base64Error;
pub use codec::{WIRE_CONFIG, WireConfig, WireDecode, WireEncode};
pub use common::{
    COMPRESSION_THRESHOLD_BYTES, DEFAULT_QUEUE_SIZE, DEFAULT_SHM_POOL_SIZE, DaemonInfo,
    DaemonStats, DataFrame, DataflowResult, DataflowStatus, DataflowSummary, DeploySpec,
    DurationMs, HistogramBucket, InputSpec, IoDelta, LogConfig, LogFrame, LogLevel, LogRecord,
    MetricBatch, MetricPoint, MetricValue, NodeExitCause, NodeInfo, NodeIoSample,
    NodeMetricsSample, NodePattern, NodeRunState, NodeSource, NodeSpawnSpec, OperatorSpec,
    OutputSpec, Plane, PriorityLane, QueuePolicy, RestartConfig, RestartPolicy, RouteAcceptance,
    RouteCloseReason, RouteDowngradeReason, RouteKey, RouteRejection, RouteSpec, ShmSegmentSpec,
    SpanStatus, StopCause, TelemetryFrame, TraceData, TraceSpan, VIRTUAL_NODE, is_virtual_port,
    virtual_port_ref, virtual_source_text,
};
pub use error::{AuthTokenError, IdError, IdKind, WireError, WireResult};
pub use frame::{
    Compression, DEFAULT_MAX_PAYLOAD_BYTES, FRAME_VERSION, Frame, FrameFlags, FrameHeader,
    FrameKind, FrameLimits, FrameView, HEADER_LEN, MAGIC, MAX_SUPPORTED_PAYLOAD_BYTES,
    decode_frame, decode_frame_prefix, encode_frame, encode_message, write_frame, write_message,
};
pub use handshake::{
    Acceptor, DEFAULT_MAX_INFLIGHT_FRAMES, DEFAULT_MAX_ROUTES, DEFAULT_MAX_SUBSCRIPTIONS,
    FeatureFlags, HandshakeError, HandshakeOutcome, Hello, MIN_USABLE_PAYLOAD_BYTES,
    NegotiatedLimits, NegotiatedSession, RefusalReason, Refused, Role, RoleSet, SessionAssignment,
    Welcome, accept_welcome, negotiate,
};
pub use ids::{
    ANY_TYPE_URN, BuildId, DAEMON_ID_SEPARATOR, DaemonId, DataId, DataflowId, MAX_NAME_LEN,
    MAX_TYPE_URN_LEN, MachineName, NodeId, OperatorId, PORT_REF_SEPARATOR, ParamKey, PortRef,
    RouteId, SessionId, SubscriptionId, TypeUrn,
};
pub use io::{
    AsyncFrameReader, AsyncFrameWriter, DEFAULT_BUFFER_CAPACITY, FrameBuffer, FrameReader,
    FrameWriter,
};
pub use messages::{
    AnyMessage, BuildOutcome, BuildStep, ControlReply, ControlRequest, CoordinatorEvent,
    DEFAULT_EVENT_BATCH, DEFAULT_LOG_LIMIT, DEFAULT_ZERO_COPY_THRESHOLD, DaemonEvent,
    DaemonRegistration, DataflowSource, ENV_NODE_CONFIG, ENV_RUN_PARENT_PID, ErrorCode,
    ExtensionKey, ExtensionNamespace, LogQuery, MAX_EXTENSION_NAME_LEN, NodeConfig,
    NodeConfigError, NodeEvent, NodeHandshake, NodeRequest, OutputPayload, ParamScope, PeerEvent,
    PeerRouteDirective, RequestScope, SpawnOutcome, StateEntry, StateEntryKind, TopicQuery,
    WireMessage, samples,
};
pub use metadata::{GoalStatus, MAX_PARAMETERS, METADATA_VERSION, Metadata, Parameter, keys};
pub use version::{
    ASTRS_VERSION_STR, AstrsVersion, MIN_SUPPORTED_PROTOCOL, PROTOCOL_VERSION, ProtocolMismatch,
    negotiate_protocol, supports_protocol,
};
