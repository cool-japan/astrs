//! [`NodeBuilder`] — the constructor for everything `init_from_env` does not
//! cover (blueprint §9.1).
//!
//! Three shapes of node reach a daemon, and the builder is the one path all
//! three take:
//!
//! | Shape | How it is built |
//! |---|---|
//! | Spawned | [`NodeBuilder::from_env`] reads `ASTRS_NODE_CONFIG` (§24.2) |
//! | Dynamic | [`NodeBuilder::node_id`] + `dynamic(true)`, endpoints from the environment (§8.3) |
//! | Overridden | Either of the above, plus explicit `daemon`/`auth`/`dataflow` |
//!
//! # Defaults
//!
//! Everything the builder does not set comes from §24.2: the Unix socket under
//! `$XDG_RUNTIME_DIR/astrs` first, then TCP loopback on 7408; the 4 KiB
//! zero-copy threshold; `ASTRS_TYPE_CHECK=warn`; and the orphan guard only
//! when `ASTRS_RUN_PARENT_PID` names a supervisor.

use std::time::Duration;

use astrs_wire::{
    AuthToken, DataId, DataflowId, NodeConfig, NodeHandshake, NodeId, NodeSource, NodeSpawnSpec,
};

use crate::env::{self, TypeCheckMode};
use crate::error::{NodeError, Result};
use crate::events::EventStream;
use crate::node::{Node, init};
use crate::orphan::{DEFAULT_POLL_INTERVAL, OrphanGuard};
use crate::runtime::NodeRuntime;
use crate::session::connect;

/// The environment variable carrying the cluster token in hex (§16).
///
/// Not in the §24.2 table, because a *spawned* node gets its token inside the
/// `ASTRS_NODE_CONFIG` blob. A dynamic node has no blob, so it needs some way
/// to present one; this is that way, and it is read only when the builder was
/// given no token of its own.
pub const ENV_AUTH_TOKEN: &str = "ASTRS_AUTH_TOKEN";

/// How long the dial and greeting are allowed to take.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds a [`Node`].
#[derive(Debug, Clone, Default)]
pub struct NodeBuilder {
    /// The blob a spawned node was handed.
    config: Option<Box<NodeConfig>>,
    /// An explicit node id.
    node_id: Option<NodeId>,
    /// An explicit dataflow id.
    dataflow: Option<DataflowId>,
    /// Endpoints to dial, overriding the configuration's.
    endpoints: Vec<String>,
    /// The cluster token.
    auth: Option<AuthToken>,
    /// Whether this node attaches itself (§8.3).
    dynamic: bool,
    /// The inputs a dynamic node declares.
    inputs: Vec<DataId>,
    /// The outputs a dynamic node declares.
    outputs: Vec<DataId>,
    /// The inputs to subscribe to; empty means all.
    subscribe: Vec<DataId>,
    /// An explicit type-check mode, overriding the environment.
    type_check: Option<TypeCheckMode>,
    /// An explicit zero-copy threshold, overriding the configuration.
    zero_copy_threshold: Option<u64>,
    /// The supervisor to watch, overriding `ASTRS_RUN_PARENT_PID`.
    parent_pid: Option<u32>,
    /// Whether to start the orphan guard at all.
    orphan_guard: bool,
    /// How long the dial and greeting may take.
    connect_timeout: Duration,
    /// A label for the greeting, shown in `astrs list`.
    label: Option<String>,
}

impl NodeBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            orphan_guard: true,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            ..Self::default()
        }
    }

    /// A builder seeded from `ASTRS_NODE_CONFIG` (§24.2).
    ///
    /// # Errors
    ///
    /// [`NodeError::MissingEnv`] when the variable is unset, and
    /// [`NodeError::Config`] when it does not decode.
    pub fn from_env() -> Result<Self> {
        let value =
            std::env::var(astrs_wire::ENV_NODE_CONFIG).map_err(|_| NodeError::MissingEnv {
                name: astrs_wire::ENV_NODE_CONFIG,
            })?;
        Self::from_env_value(&value)
    }

    /// A builder seeded from an explicit blob.
    ///
    /// # Errors
    ///
    /// [`NodeError::Config`] when the blob does not decode.
    pub fn from_env_value(value: &str) -> Result<Self> {
        let config = NodeConfig::from_env_value(value)?;
        Ok(Self::new().with_config(config))
    }

    /// Seeds the builder from an already-decoded configuration.
    #[must_use]
    pub fn with_config(mut self, config: NodeConfig) -> Self {
        self.config = Some(Box::new(config));
        self
    }

    /// Sets the node's id.
    ///
    /// # Errors
    ///
    /// [`NodeError::Id`] when the name fails the identifier grammar.
    pub fn node_id(mut self, node_id: impl AsRef<str>) -> Result<Self> {
        self.node_id = Some(NodeId::new(node_id.as_ref())?);
        Ok(self)
    }

    /// Sets the dataflow to join.
    #[must_use]
    pub const fn dataflow(mut self, dataflow: DataflowId) -> Self {
        self.dataflow = Some(dataflow);
        self
    }

    /// Adds an endpoint to dial, overriding the configuration's list.
    #[must_use]
    pub fn daemon(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoints.push(endpoint.into());
        self
    }

    /// Sets the cluster token (§16).
    #[must_use]
    pub fn auth(mut self, auth: AuthToken) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Marks the node as attaching itself (§8.3).
    #[must_use]
    pub const fn dynamic(mut self, dynamic: bool) -> Self {
        self.dynamic = dynamic;
        self
    }

    /// Declares an input a dynamic node intends to read.
    ///
    /// # Errors
    ///
    /// [`NodeError::Id`] when the name fails the identifier grammar.
    pub fn input(mut self, input: impl AsRef<str>) -> Result<Self> {
        self.inputs.push(DataId::new(input.as_ref())?);
        Ok(self)
    }

    /// Declares an output a dynamic node intends to write.
    ///
    /// # Errors
    ///
    /// [`NodeError::Id`] when the name fails the identifier grammar.
    pub fn output(mut self, output: impl AsRef<str>) -> Result<Self> {
        self.outputs.push(DataId::new(output.as_ref())?);
        Ok(self)
    }

    /// Subscribes to a subset of the node's inputs; the default is all of
    /// them.
    ///
    /// # Errors
    ///
    /// [`NodeError::Id`] when the name fails the identifier grammar.
    pub fn subscribe(mut self, input: impl AsRef<str>) -> Result<Self> {
        self.subscribe.push(DataId::new(input.as_ref())?);
        Ok(self)
    }

    /// Overrides the §9.2 type-check mode.
    #[must_use]
    pub const fn type_check(mut self, mode: TypeCheckMode) -> Self {
        self.type_check = Some(mode);
        self
    }

    /// Overrides the §6.2 zero-copy threshold.
    #[must_use]
    pub const fn zero_copy_threshold(mut self, bytes: u64) -> Self {
        self.zero_copy_threshold = Some(bytes);
        self
    }

    /// Watches `pid` as the supervising process (§4.2).
    #[must_use]
    pub const fn parent_pid(mut self, pid: u32) -> Self {
        self.parent_pid = Some(pid);
        self
    }

    /// Turns the orphan guard off, for a node that outlives its launcher on
    /// purpose.
    #[must_use]
    pub const fn orphan_guard(mut self, enabled: bool) -> Self {
        self.orphan_guard = enabled;
        self
    }

    /// Bounds the dial and greeting.
    #[must_use]
    pub const fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets the label the greeting carries, shown in `astrs list`.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The endpoints this builder will dial, in order.
    ///
    /// Explicit endpoints win; then the configuration blob's; then the §24.2
    /// defaults (Unix socket, then TCP loopback).
    ///
    /// # Errors
    ///
    /// [`NodeError::BadEnv`] when `ASTRS_DAEMON_PORT` is not a port.
    pub fn resolved_endpoints(&self) -> Result<Vec<String>> {
        if !self.endpoints.is_empty() {
            return Ok(self.endpoints.clone());
        }
        if let Some(config) = &self.config
            && !config.endpoints.is_empty()
        {
            return Ok(config.endpoints.clone());
        }
        Ok(vec![
            format!("uds://{}", env::daemon_socket_path().display()),
            format!("tcp://127.0.0.1:{}", env::daemon_port()?),
        ])
    }

    /// The token this builder will present.
    ///
    /// # Errors
    ///
    /// [`NodeError::BadEnv`] when `ASTRS_AUTH_TOKEN` is set but not valid hex.
    pub fn resolved_auth(&self) -> Result<AuthToken> {
        if let Some(auth) = &self.auth {
            return Ok(auth.clone());
        }
        if let Some(config) = &self.config {
            return Ok(config.auth.clone());
        }
        match std::env::var(ENV_AUTH_TOKEN) {
            Ok(hex) if !hex.trim().is_empty() => {
                AuthToken::parse_hex(hex.trim()).map_err(|error| NodeError::BadEnv {
                    name: ENV_AUTH_TOKEN,
                    value: "<redacted>".to_owned(),
                    reason: error.to_string(),
                })
            }
            _ => Ok(AuthToken::ZERO),
        }
    }

    /// The registration this builder will send.
    ///
    /// # Errors
    ///
    /// [`NodeError::Pattern`] when neither a configuration blob nor a node id
    /// was supplied, since there is then nothing to register *as*.
    pub fn handshake(&self) -> Result<NodeHandshake> {
        let (dataflow, node, generation) = self.identity()?;
        let handshake = if self.dynamic {
            NodeHandshake::dynamic(dataflow, node)
                .with_inputs(self.inputs.clone())
                .with_outputs(self.outputs.clone())
        } else {
            NodeHandshake::new(dataflow, node, generation)
        };
        Ok(handshake.with_pid(std::process::id()))
    }

    /// The specification a dynamic node registers with, when it has no blob.
    ///
    /// # Errors
    ///
    /// As [`NodeBuilder::handshake`].
    pub fn provisional_spec(&self) -> Result<NodeSpawnSpec> {
        if let Some(config) = &self.config {
            return Ok(config.spec.clone());
        }
        let (dataflow, node, generation) = self.identity()?;
        Ok(NodeSpawnSpec::new(
            dataflow,
            node,
            generation,
            NodeSource::Dynamic,
        ))
    }

    /// Connects, registers and starts the session.
    ///
    /// # Errors
    ///
    /// [`NodeError::Connect`] when no endpoint answered, [`NodeError::Handshake`]
    /// when the greeting was refused, and [`NodeError::Registration`] when the
    /// daemon refused the registration.
    pub fn connect(self) -> Result<(Node, EventStream)> {
        let runtime = NodeRuntime::acquire()?;
        let handle = runtime.clone();
        handle.block_on("Node::init", "NodeBuilder::connect_async", async move {
            self.connect_with(runtime).await
        })?
    }

    /// The async form of [`NodeBuilder::connect`].
    ///
    /// # Errors
    ///
    /// As [`NodeBuilder::connect`].
    pub async fn connect_async(self) -> Result<(Node, EventStream)> {
        let runtime = NodeRuntime::acquire()?;
        self.connect_with(runtime).await
    }

    /// Connects on a runtime the caller chose.
    ///
    /// # Errors
    ///
    /// As [`NodeBuilder::connect`].
    pub async fn connect_with(self, runtime: NodeRuntime) -> Result<(Node, EventStream)> {
        let endpoints = self.resolved_endpoints()?;
        let auth = self.resolved_auth()?;
        let handshake = self.handshake()?;
        let link =
            connect::dial(&endpoints, &auth, self.label.clone(), self.connect_timeout).await?;
        self.finish(link, handshake, runtime).await
    }

    /// Registers over an already-established link.
    ///
    /// The seam the testing harness uses: it supplies a link over a
    /// `tokio::io::duplex` pair rather than a socket.
    ///
    /// # Errors
    ///
    /// As [`NodeBuilder::connect`].
    pub async fn finish(
        self,
        link: connect::NodeLink,
        handshake: NodeHandshake,
        runtime: NodeRuntime,
    ) -> Result<(Node, EventStream)> {
        let type_check = match self.type_check {
            Some(mode) => mode,
            None => TypeCheckMode::from_env()?,
        };
        let threshold = match self.zero_copy_threshold {
            Some(threshold) => threshold,
            None => match &self.config {
                Some(config) => config.zero_copy_threshold,
                None => env::zero_copy_threshold()?,
            },
        };
        // The daemon's own answer to "where do I attach segments?", straight
        // from the handshake blob: a node must never recompute it from the
        // environment, because `astrs run --runtime-dir` moves it (§6.2).
        let broker = self
            .config
            .as_ref()
            .and_then(|config| config.shm_broker.clone())
            .map(std::path::PathBuf::from);
        let registration = init::register_with(
            link,
            handshake,
            runtime.clone(),
            threshold,
            self.subscribe.clone(),
            broker,
        )
        .await?;

        let parent = match self.parent_pid {
            Some(pid) => Some(pid),
            None => env::parent_pid()?,
        };
        let orphan = if self.orphan_guard {
            OrphanGuard::maybe_watch(
                parent,
                &runtime,
                std::sync::Arc::clone(&registration.shared.source),
                DEFAULT_POLL_INTERVAL,
            )
        } else {
            None
        };

        Ok(Node::from_registration(registration, type_check, orphan))
    }

    /// The `(dataflow, node, generation)` triple this builder registers as.
    fn identity(&self) -> Result<(DataflowId, NodeId, u64)> {
        if let Some(config) = &self.config {
            let node = self
                .node_id
                .clone()
                .unwrap_or_else(|| config.node().clone());
            let dataflow = self.dataflow.unwrap_or_else(|| config.dataflow());
            return Ok((dataflow, node, config.generation()));
        }
        let Some(node) = self.node_id.clone() else {
            return Err(NodeError::Pattern(
                "a node needs either ASTRS_NODE_CONFIG or an explicit node id".to_owned(),
            ));
        };
        Ok((
            self.dataflow.unwrap_or_else(|| DataflowId::from_u128(0)),
            node,
            0,
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::DaemonId;

    fn config() -> NodeConfig {
        let spec = NodeSpawnSpec::new(
            DataflowId::from_u128(9),
            NodeId::new("camera").unwrap(),
            3,
            NodeSource::Executable {
                path: "./camera".to_owned(),
            },
        );
        NodeConfig::new(
            spec,
            DaemonId::generate(None),
            AuthToken::from_bytes([4; 32]),
        )
        .with_endpoint("uds:///tmp/astrs/daemon.sock")
        .with_zero_copy_threshold(8192)
    }

    #[test]
    fn a_configured_builder_takes_its_identity_from_the_blob() {
        let builder = NodeBuilder::new().with_config(config());
        let handshake = builder.handshake().unwrap();
        assert_eq!(handshake.node.as_str(), "camera");
        assert_eq!(handshake.generation, 3);
        assert!(!handshake.dynamic);
        assert_eq!(handshake.pid, Some(std::process::id()));
        assert_eq!(
            builder.resolved_endpoints().unwrap(),
            vec!["uds:///tmp/astrs/daemon.sock"]
        );
        assert_eq!(
            builder.resolved_auth().unwrap(),
            AuthToken::from_bytes([4; 32])
        );
        assert_eq!(builder.provisional_spec().unwrap().generation, 3);
    }

    #[test]
    fn a_dynamic_builder_declares_its_ports() {
        let builder = NodeBuilder::new()
            .node_id("probe")
            .unwrap()
            .dynamic(true)
            .input("frames")
            .unwrap()
            .output("samples")
            .unwrap();
        let handshake = builder.handshake().unwrap();
        assert!(handshake.dynamic);
        assert_eq!(handshake.generation, 0);
        assert!(handshake.declares_ports());
        assert_eq!(handshake.inputs.len(), 1);
        assert_eq!(handshake.outputs.len(), 1);
        assert!(matches!(
            builder.provisional_spec().unwrap().source,
            NodeSource::Dynamic
        ));
    }

    #[test]
    fn a_builder_with_neither_blob_nor_id_is_refused() {
        let error = NodeBuilder::new().handshake().unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
    }

    #[test]
    fn explicit_endpoints_win_over_the_blob() {
        let builder = NodeBuilder::new()
            .with_config(config())
            .daemon("tcp://10.0.0.2:7408");
        assert_eq!(
            builder.resolved_endpoints().unwrap(),
            vec!["tcp://10.0.0.2:7408"]
        );
    }

    #[test]
    fn a_bare_builder_falls_back_to_the_appendix_defaults() {
        let builder = NodeBuilder::new().node_id("probe").unwrap();
        let endpoints = builder.resolved_endpoints().unwrap();
        assert_eq!(endpoints.len(), 2, "uds first, tcp second");
        assert!(endpoints[0].starts_with("uds://"), "{endpoints:?}");
        assert!(
            endpoints[1].starts_with("tcp://127.0.0.1:"),
            "{endpoints:?}"
        );
    }

    #[test]
    fn an_explicit_token_wins_over_everything() {
        let token = AuthToken::from_bytes([7; 32]);
        let builder = NodeBuilder::new().with_config(config()).auth(token.clone());
        assert_eq!(builder.resolved_auth().unwrap(), token);
    }

    #[test]
    fn a_missing_environment_blob_is_named() {
        // `from_env_value` is exercised directly; the process environment is
        // shared with parallel tests.
        let error = NodeBuilder::from_env_value("!!!!").unwrap_err();
        assert!(matches!(error, NodeError::Config(_)), "{error}");

        let blob = config().to_env_value().unwrap();
        let builder = NodeBuilder::from_env_value(&blob).unwrap();
        assert_eq!(builder.handshake().unwrap().node.as_str(), "camera");
    }

    #[test]
    fn builder_settings_are_recorded() {
        let builder = NodeBuilder::new()
            .node_id("probe")
            .unwrap()
            .dataflow(DataflowId::from_u128(2))
            .type_check(TypeCheckMode::Error)
            .zero_copy_threshold(1)
            .parent_pid(42)
            .orphan_guard(false)
            .connect_timeout(Duration::from_millis(5))
            .label("probe-1")
            .subscribe("frames")
            .unwrap();
        assert_eq!(builder.type_check, Some(TypeCheckMode::Error));
        assert_eq!(builder.zero_copy_threshold, Some(1));
        assert_eq!(builder.parent_pid, Some(42));
        assert!(!builder.orphan_guard);
        assert_eq!(builder.connect_timeout, Duration::from_millis(5));
        assert_eq!(builder.label.as_deref(), Some("probe-1"));
        assert_eq!(builder.subscribe.len(), 1);
        let handshake = builder.handshake().unwrap();
        assert_eq!(handshake.dataflow, DataflowId::from_u128(2));
    }

    #[test]
    fn connecting_to_nothing_reports_every_attempt() {
        let error = NodeBuilder::new()
            .node_id("probe")
            .unwrap()
            .daemon(format!(
                "uds://{}",
                std::env::temp_dir().join("astrs-absent.sock").display()
            ))
            .connect_timeout(Duration::from_millis(100))
            .connect()
            .unwrap_err();
        assert!(matches!(error, NodeError::Connect { .. }), "{error}");
    }

    #[test]
    fn the_documented_constants_are_stable() {
        assert_eq!(ENV_AUTH_TOKEN, "ASTRS_AUTH_TOKEN");
        assert_eq!(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs(10));
    }
}
