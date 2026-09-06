//! [`DaemonConfig`] — the identity, addresses and budgets one daemon runs on.
//!
//! Everything the local core reads from the outside world is gathered here so
//! the rest of the crate takes it as a parameter and stays testable: no module
//! below this one calls [`std::env::var`], and none of them invents a timeout.
//!
//! | Sub-module | Contents |
//! |---|---|
//! | [`paths`] | The runtime directory and the socket paths inside it (§24.2) |
//! | [`listen`] | Which local listeners to open, and the endpoints nodes dial |
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::config::{DaemonConfig, ListenConfig, RuntimePaths};
//!
//! let paths = RuntimePaths::under(std::env::temp_dir().join("astrs-doc"));
//! let config = DaemonConfig::new(paths)
//!     .with_listen(ListenConfig::loopback_tcp(0))
//!     .with_env_passthrough(["CUDA_VISIBLE_DEVICES"]);
//!
//! assert_eq!(config.heartbeat_interval().as_secs(), 5);
//! assert!(config.env_passthrough().contains(&"CUDA_VISIBLE_DEVICES".to_string()));
//! ```

pub mod listen;
pub mod paths;

use std::path::{Path, PathBuf};
use std::time::Duration;

use astrs_wire::{AuthToken, DaemonId, MachineName, NegotiatedLimits};

use crate::error::DaemonResult;
use crate::peer::PeerConfig;

pub use listen::{DEFAULT_DAEMON_PORT, ENV_DAEMON_PORT, ListenConfig, loopback, port_from_env};
pub use paths::{
    DEFAULT_SHM_SOCKET_NAME, DEFAULT_SOCKET_NAME, ENV_RUNTIME_DIR, ENV_XDG_RUNTIME_DIR,
    MAX_SOCKET_PATH_LEN, RUNTIME_DIR_NAME, RuntimePaths, check_socket_path_len,
    default_runtime_dir,
};

/// The heartbeat period (§24.2: 5 s).
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// The per-node metric sampling period (§24.2: 2 s).
pub const DEFAULT_METRICS_INTERVAL: Duration = Duration::from_secs(2);

/// The health-check period (§24.2: 5 s).
pub const DEFAULT_HEALTH_INTERVAL: Duration = Duration::from_secs(5);

/// How long a spawned node has to register before it is declared hung (§12).
///
/// This is dora's closed gap: a node that hangs *before* subscribing was
/// previously invisible to the health check, which only starts once the node
/// is registered.
pub const DEFAULT_SPAWN_DEADLINE: Duration = Duration::from_secs(30);

/// How long a stopping node has before `SIGTERM` escalates to `SIGKILL` (§12).
pub const DEFAULT_FINISH_GRACE: Duration = Duration::from_secs(15);

/// The environment variables inherited from the daemon's own environment
/// (§16): everything else is scrubbed.
pub const DEFAULT_ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "TMPDIR", "RUST_LOG"];

/// One daemon's configuration.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// This machine's name, as the manifest's `deploy.machine` refers to it.
    machine: Option<MachineName>,
    /// This daemon's identity, minted once at construction.
    id: DaemonId,
    /// Where the daemon keeps its sockets and logs.
    paths: RuntimePaths,
    /// Which listeners to open.
    listen: ListenConfig,
    /// The cluster auth token every `Hello` must present (§16).
    auth: AuthToken,
    /// The limits the daemon proposes to nodes at handshake.
    limits: NegotiatedLimits,
    /// The dataflow working directory, resolved to an absolute path.
    working_dir: PathBuf,
    /// Extra inherited variables a spawned node may see, beyond the allowlist.
    env_passthrough: Vec<String>,
    /// The heartbeat period.
    heartbeat_interval: Duration,
    /// The metric sampling period.
    metrics_interval: Duration,
    /// The health-check period.
    health_interval: Duration,
    /// How long a node has to register after being spawned.
    spawn_deadline: Duration,
    /// How long a stopping node has before the watchdog escalates.
    finish_grace: Duration,
    /// The zero-copy threshold handed to nodes (§24.2).
    zero_copy_threshold: u64,
    /// Whether the run is deterministic (§14).
    deterministic: bool,
    /// The recording a deterministic run replays as its clock source, and
    /// its pacing factor (§14). `None` here even when [`Self::deterministic`]
    /// is set means "not yet supplied" — [`crate::server::core::Daemon::new`]
    /// still honors `deterministic` for the handshake blob, but there is no
    /// clock to drive the wheel from.
    replay_recording: Option<(PathBuf, Option<f64>)>,
    /// The pid a spawned node should watch as its orphan guard (§4.2).
    run_parent_pid: Option<u32>,
    /// The liveness timeout applied to a node whose manifest names none (§12).
    ///
    /// [`None`] — the default — means a node is monitored only when its own
    /// `health_check_timeout` asks for it. See
    /// [`crate::health::HealthTable`] for why a universal deadline is the
    /// wrong default.
    default_health_timeout: Option<Duration>,
    /// Whether to broker a shared-memory plane at all (§6.2).
    shm_enabled: bool,
    /// How this daemon talks to other daemons (§6.4).
    peer: PeerConfig,
}

impl DaemonConfig {
    /// A configuration with the blueprint defaults, under `paths`.
    ///
    /// The identity is minted here, so two `DaemonConfig`s are two daemons.
    /// The auth token starts at [`AuthToken::ZERO`]; a cluster daemon replaces
    /// it with the one `astrs up` generated, while `astrs run` — a single
    /// process with a private socket — legitimately keeps it.
    #[must_use]
    pub fn new(paths: RuntimePaths) -> Self {
        let listen = ListenConfig::defaults(&paths);
        Self {
            machine: None,
            id: DaemonId::generate(None),
            paths,
            listen,
            auth: AuthToken::ZERO,
            limits: NegotiatedLimits::uds(),
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env_passthrough: Vec::new(),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            metrics_interval: DEFAULT_METRICS_INTERVAL,
            health_interval: DEFAULT_HEALTH_INTERVAL,
            spawn_deadline: DEFAULT_SPAWN_DEADLINE,
            finish_grace: DEFAULT_FINISH_GRACE,
            zero_copy_threshold: astrs_wire::DEFAULT_ZERO_COPY_THRESHOLD,
            deterministic: false,
            replay_recording: None,
            run_parent_pid: None,
            default_health_timeout: None,
            shm_enabled: true,
            peer: PeerConfig::default(),
        }
    }

    /// A configuration read from the environment (§24.2).
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(RuntimePaths::from_env())
    }

    /// A configuration for an embedded, single-process daemon: no listener at
    /// all beyond the Unix socket under `root`, and the orphan guard armed.
    ///
    /// This is what `astrs run` uses (§4.2) — see
    /// [`crate::run_dataflow_with`].
    #[must_use]
    pub fn embedded(root: impl Into<PathBuf>, socket_name: impl Into<String>) -> Self {
        let paths = RuntimePaths::under(root).with_socket_name(socket_name);
        let listen = ListenConfig::uds(paths.socket_path());
        Self {
            listen,
            run_parent_pid: Some(std::process::id()),
            ..Self::new(paths)
        }
    }

    /// Names the machine this daemon runs on.
    #[must_use]
    pub fn with_machine(mut self, machine: MachineName) -> Self {
        self.id = DaemonId::new(Some(machine.clone()), self.id.uuid());
        self.machine = Some(machine);
        self
    }

    /// Uses an explicit daemon identity, for a daemon rejoining a cluster.
    #[must_use]
    pub fn with_id(mut self, id: DaemonId) -> Self {
        self.machine = id.machine_name().cloned();
        self.id = id;
        self
    }

    /// Replaces the listener configuration.
    #[must_use]
    pub fn with_listen(mut self, listen: ListenConfig) -> Self {
        self.listen = listen;
        self
    }

    /// Sets the cluster auth token (§16).
    #[must_use]
    pub fn with_auth(mut self, auth: AuthToken) -> Self {
        self.auth = auth;
        self
    }

    /// Sets the handshake limits proposed to nodes.
    #[must_use]
    pub const fn with_limits(mut self, limits: NegotiatedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the dataflow working directory.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = dir.into();
        self
    }

    /// Adds inherited variables a spawned node may see beyond the §16
    /// allowlist.
    ///
    /// This is the *explicit passthrough* half of the env hygiene rule: an
    /// operator who needs `CUDA_VISIBLE_DEVICES` or a proxy setting in every
    /// node says so once, here, rather than the daemon leaking its whole
    /// environment.
    #[must_use]
    pub fn with_env_passthrough<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.env_passthrough
            .extend(names.into_iter().map(Into::into));
        self.env_passthrough.sort_unstable();
        self.env_passthrough.dedup();
        self
    }

    /// Sets the heartbeat period.
    #[must_use]
    pub const fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Sets the metric sampling period.
    #[must_use]
    pub const fn with_metrics_interval(mut self, interval: Duration) -> Self {
        self.metrics_interval = interval;
        self
    }

    /// Sets the health-check period.
    #[must_use]
    pub const fn with_health_interval(mut self, interval: Duration) -> Self {
        self.health_interval = interval;
        self
    }

    /// Sets the spawn deadline (§12).
    #[must_use]
    pub const fn with_spawn_deadline(mut self, deadline: Duration) -> Self {
        self.spawn_deadline = deadline;
        self
    }

    /// Sets the finish grace period (§12).
    #[must_use]
    pub const fn with_finish_grace(mut self, grace: Duration) -> Self {
        self.finish_grace = grace;
        self
    }

    /// Sets the zero-copy threshold handed to nodes.
    #[must_use]
    pub const fn with_zero_copy_threshold(mut self, bytes: u64) -> Self {
        self.zero_copy_threshold = bytes;
        self
    }

    /// Marks the run deterministic (§14).
    #[must_use]
    pub const fn with_deterministic(mut self, deterministic: bool) -> Self {
        self.deterministic = deterministic;
        self
    }

    /// Sets the recording a deterministic run replays as its clock source,
    /// and its pacing factor (§14).
    ///
    /// `recording` of [`None`] clears any previously configured recording
    /// (`speed` is then irrelevant and ignored). A `speed` of `None` steps
    /// through the recording as fast as the loop can; `Some(1.0)` reproduces
    /// the original wall-clock spacing; `Some(factor)` scales it — see
    /// [`crate::local::ReplaySource::speed`].
    #[must_use]
    pub fn with_replay_recording(mut self, recording: Option<PathBuf>, speed: Option<f64>) -> Self {
        self.replay_recording = recording.map(|path| (path, speed));
        self
    }

    /// Sets the liveness timeout for nodes whose manifest names none (§12).
    #[must_use]
    pub const fn with_default_health_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.default_health_timeout = timeout;
        self
    }

    /// Turns the shared-memory plane on or off (§6.2).
    #[must_use]
    pub const fn with_shm(mut self, enabled: bool) -> Self {
        self.shm_enabled = enabled;
        self
    }

    /// Replaces the peer configuration (§6.4).
    #[must_use]
    pub fn with_peer(mut self, peer: PeerConfig) -> Self {
        self.peer = peer;
        self
    }

    /// The liveness timeout for nodes whose manifest names none (§12).
    #[must_use]
    pub const fn default_health_timeout(&self) -> Option<Duration> {
        self.default_health_timeout
    }

    /// Whether the shared-memory plane is wanted (§6.2).
    #[must_use]
    pub const fn shm_enabled(&self) -> bool {
        self.shm_enabled
    }

    /// How this daemon talks to other daemons (§6.4).
    #[must_use]
    pub const fn peer(&self) -> &PeerConfig {
        &self.peer
    }

    /// Sets the orphan-guard pid a spawned node watches (§4.2).
    #[must_use]
    pub const fn with_run_parent_pid(mut self, pid: Option<u32>) -> Self {
        self.run_parent_pid = pid;
        self
    }

    /// This daemon's identity.
    #[must_use]
    pub const fn id(&self) -> &DaemonId {
        &self.id
    }

    /// This machine's name, if it has one.
    #[must_use]
    pub const fn machine(&self) -> Option<&MachineName> {
        self.machine.as_ref()
    }

    /// The directory layout.
    #[must_use]
    pub const fn paths(&self) -> &RuntimePaths {
        &self.paths
    }

    /// The listener configuration.
    #[must_use]
    pub const fn listen(&self) -> &ListenConfig {
        &self.listen
    }

    /// The listener configuration, mutably — for recording the port the
    /// kernel actually chose after binding `0`.
    pub const fn listen_mut(&mut self) -> &mut ListenConfig {
        &mut self.listen
    }

    /// The cluster auth token.
    #[must_use]
    pub const fn auth(&self) -> &AuthToken {
        &self.auth
    }

    /// The handshake limits proposed to nodes.
    #[must_use]
    pub const fn limits(&self) -> NegotiatedLimits {
        self.limits
    }

    /// The dataflow working directory.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// The extra inherited variables spawned nodes may see.
    #[must_use]
    pub fn env_passthrough(&self) -> &[String] {
        &self.env_passthrough
    }

    /// The heartbeat period.
    #[must_use]
    pub const fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }

    /// The metric sampling period.
    #[must_use]
    pub const fn metrics_interval(&self) -> Duration {
        self.metrics_interval
    }

    /// The health-check period.
    #[must_use]
    pub const fn health_interval(&self) -> Duration {
        self.health_interval
    }

    /// The spawn deadline.
    #[must_use]
    pub const fn spawn_deadline(&self) -> Duration {
        self.spawn_deadline
    }

    /// The finish grace period.
    #[must_use]
    pub const fn finish_grace(&self) -> Duration {
        self.finish_grace
    }

    /// The zero-copy threshold handed to nodes.
    #[must_use]
    pub const fn zero_copy_threshold(&self) -> u64 {
        self.zero_copy_threshold
    }

    /// Whether the run is deterministic.
    #[must_use]
    pub const fn deterministic(&self) -> bool {
        self.deterministic
    }

    /// The recording a deterministic run replays as its clock source, and
    /// its pacing factor, if one was configured.
    #[must_use]
    pub fn replay_recording(&self) -> Option<(&Path, Option<f64>)> {
        self.replay_recording
            .as_ref()
            .map(|(path, speed)| (path.as_path(), *speed))
    }

    /// The orphan-guard pid handed to spawned nodes, if any.
    #[must_use]
    pub const fn run_parent_pid(&self) -> Option<u32> {
        self.run_parent_pid
    }

    /// Creates the runtime directory and checks the listener configuration.
    ///
    /// # Errors
    ///
    /// Anything [`RuntimePaths::prepare`] or [`ListenConfig::validate`] can
    /// return.
    pub fn prepare(&self) -> DaemonResult<()> {
        self.paths.prepare()?;
        self.listen.validate()
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn config() -> DaemonConfig {
        DaemonConfig::new(RuntimePaths::under("/run/astrs"))
    }

    #[test]
    fn defaults_follow_the_appendix() {
        let config = config();
        assert_eq!(config.heartbeat_interval(), Duration::from_secs(5));
        assert_eq!(config.metrics_interval(), Duration::from_secs(2));
        assert_eq!(config.health_interval(), Duration::from_secs(5));
        assert_eq!(config.zero_copy_threshold(), 4_096);
        assert_eq!(config.listen().tcp_port(), Some(7408));
        assert!(!config.deterministic());
        assert!(config.run_parent_pid().is_none());
    }

    #[test]
    fn the_allowlist_is_the_documented_five() {
        assert_eq!(
            DEFAULT_ENV_ALLOWLIST,
            &["PATH", "HOME", "USER", "TMPDIR", "RUST_LOG"]
        );
    }

    #[test]
    fn two_configurations_are_two_daemons() {
        assert_ne!(config().id().uuid(), config().id().uuid());
    }

    #[test]
    fn naming_a_machine_keeps_the_uuid_and_adds_the_label() {
        let bare = config();
        let uuid = bare.id().uuid();
        let named = bare.with_machine(MachineName::new("robot-a").unwrap());
        assert_eq!(named.id().uuid(), uuid);
        assert_eq!(named.id().machine(), Some("robot-a"));
        assert_eq!(named.machine().map(MachineName::as_str), Some("robot-a"));
    }

    #[test]
    fn an_explicit_identity_carries_its_machine_name() {
        let id = DaemonId::from_parts(Some("robot-b"), astrs_wire::DaemonId::generate(None).uuid())
            .unwrap();
        let config = config().with_id(id.clone());
        assert_eq!(config.id(), &id);
        assert_eq!(config.machine().map(MachineName::as_str), Some("robot-b"));
    }

    #[test]
    fn passthrough_names_are_sorted_and_deduplicated() {
        let config = config()
            .with_env_passthrough(["B", "A"])
            .with_env_passthrough(["A", "C"]);
        assert_eq!(config.env_passthrough(), ["A", "B", "C"]);
    }

    #[test]
    fn the_embedded_configuration_arms_the_orphan_guard() {
        let config = DaemonConfig::embedded(std::env::temp_dir(), "run.sock");
        assert_eq!(config.run_parent_pid(), Some(std::process::id()));
        assert!(config.listen().uds_path().is_some());
        assert!(
            config.listen().tcp_addr().is_none(),
            "an embedded daemon has no reason to open a TCP port"
        );
    }

    #[test]
    fn budgets_are_all_overridable() {
        let config = config()
            .with_heartbeat_interval(Duration::from_millis(10))
            .with_metrics_interval(Duration::from_millis(20))
            .with_health_interval(Duration::from_millis(30))
            .with_spawn_deadline(Duration::from_millis(40))
            .with_finish_grace(Duration::from_millis(50))
            .with_zero_copy_threshold(1)
            .with_deterministic(true)
            .with_run_parent_pid(Some(7));
        assert_eq!(config.heartbeat_interval(), Duration::from_millis(10));
        assert_eq!(config.metrics_interval(), Duration::from_millis(20));
        assert_eq!(config.health_interval(), Duration::from_millis(30));
        assert_eq!(config.spawn_deadline(), Duration::from_millis(40));
        assert_eq!(config.finish_grace(), Duration::from_millis(50));
        assert_eq!(config.zero_copy_threshold(), 1);
        assert!(config.deterministic());
        assert_eq!(config.run_parent_pid(), Some(7));
    }

    #[test]
    fn the_bound_port_can_be_written_back() {
        let mut config = config().with_listen(ListenConfig::loopback_tcp(0));
        config.listen_mut().set_bound_tcp(loopback(40_000));
        assert_eq!(config.listen().tcp_port(), Some(40_000));
    }

    #[test]
    fn prepare_creates_the_runtime_directory() {
        let root = std::env::temp_dir().join(format!("astrs-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config = DaemonConfig::new(RuntimePaths::under(&root))
            .with_listen(ListenConfig::loopback_tcp(0));
        config.prepare().unwrap();
        assert!(root.is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }
}
