//! [`PeerBook`] — the static half of AstRS discovery (blueprint §6.4:
//! "Static peer lists + UDP multicast beacon; daemon/coordinator
//! rendezvous").
//!
//! A `PeerBook` names the cluster's coordinator address(es) and, per
//! machine label, the daemon address(es) reachable there — the same shape
//! a dataflow manifest's `deploy:` section conceptually describes, without
//! this crate depending on `astrs-manifest` for it (blueprint's layer rule,
//! §4.1: `astrs-discovery` is Layer 1, `astrs-manifest` is Layer 2, and a
//! Layer 1 crate never depends upward). Callers that already have parsed
//! manifest data pass plain [`std::net::SocketAddr`] / [`MachineName`]
//! values directly through [`PeerBook::with_coordinator`] /
//! [`PeerBook::with_daemon`]; [`PeerBook::from_file`] and
//! [`PeerBook::from_env`] exist for the common case of a standalone
//! discovery config not otherwise wired into a manifest at all (a bare
//! `astrs-daemon` deployment, or a test harness).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use astrs_wire::MachineName;
use serde::{Deserialize, Serialize};

use crate::defaults::{ENV_COORDINATOR_ADDR, ENV_DAEMON_ADDRS};
use crate::error::{DiscoveryError, DiscoveryResult};

/// The static, explicitly-configured peer address book (blueprint §6.4).
///
/// Every list of candidates is kept in caller-supplied order — first
/// entry highest preference — since that order is exactly what
/// [`crate::rendezvous::discover_coordinator`] returns unchanged for the
/// static contribution to its result (blueprint precedence rule: "static
/// config wins over multicast").
///
/// # Examples
///
/// ```
/// use astrs_discovery::PeerBook;
/// use astrs_wire::MachineName;
///
/// let book = PeerBook::new()
///     .with_coordinator("10.0.0.1:7407".parse()?)
///     .with_daemon(MachineName::new("arm-01")?, "10.0.0.5:7408".parse()?);
///
/// assert_eq!(book.coordinator_candidates().len(), 1);
/// assert_eq!(book.daemon_candidates(&MachineName::new("arm-01")?).len(), 1);
/// assert!(book.daemon_candidates(&MachineName::new("unknown")?).is_empty());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerBook {
    coordinator: Vec<SocketAddr>,
    daemons: BTreeMap<MachineName, Vec<SocketAddr>>,
}

impl PeerBook {
    /// An empty peer book: no coordinator, no daemons.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a coordinator candidate address.
    ///
    /// Order is preserved and meaningful: the first address appended is the
    /// first one [`crate::rendezvous::discover_coordinator`] returns.
    #[must_use]
    pub fn with_coordinator(mut self, addr: SocketAddr) -> Self {
        self.coordinator.push(addr);
        self
    }

    /// Appends a daemon candidate address for `machine`.
    #[must_use]
    pub fn with_daemon(mut self, machine: MachineName, addr: SocketAddr) -> Self {
        self.daemons.entry(machine).or_default().push(addr);
        self
    }

    /// Coordinator candidates, highest-preference first.
    #[must_use]
    pub fn coordinator_candidates(&self) -> &[SocketAddr] {
        &self.coordinator
    }

    /// Daemon candidates for `machine`, highest-preference first. Empty if
    /// `machine` is not in this book.
    #[must_use]
    pub fn daemon_candidates(&self, machine: &MachineName) -> &[SocketAddr] {
        self.daemons
            .get(machine)
            .map_or(&[], |addrs| addrs.as_slice())
    }

    /// Every machine label this book has daemon addresses for, in label
    /// order.
    pub fn machines(&self) -> impl Iterator<Item = &MachineName> {
        self.daemons.keys()
    }

    /// Whether this book names neither a coordinator nor any daemon.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.coordinator.is_empty() && self.daemons.is_empty()
    }

    /// Folds `other`'s entries into `self`, appending after this book's own
    /// (so `self`'s candidates keep their higher preference).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_discovery::PeerBook;
    ///
    /// let a = PeerBook::new().with_coordinator("10.0.0.1:7407".parse()?);
    /// let b = PeerBook::new().with_coordinator("10.0.0.2:7407".parse()?);
    /// let merged = a.merged(b);
    /// assert_eq!(
    ///     merged.coordinator_candidates(),
    ///     &["10.0.0.1:7407".parse()?, "10.0.0.2:7407".parse()?]
    /// );
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn merged(mut self, other: Self) -> Self {
        self.coordinator.extend(other.coordinator);
        for (machine, addrs) in other.daemons {
            self.daemons.entry(machine).or_default().extend(addrs);
        }
        self
    }

    /// Loads a peer book from a JSON file shaped like
    /// [`PeerBookConfig`]'s documentation.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::ConfigRead`] if the file cannot be read;
    /// [`DiscoveryError::ConfigParse`] if it is not valid JSON or does not
    /// match the expected shape; [`DiscoveryError::InvalidMachineName`] if a
    /// machine label fails [`MachineName`]'s grammar.
    pub fn from_file(path: &Path) -> DiscoveryResult<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| DiscoveryError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let config: PeerBookConfig =
            serde_json::from_str(&text).map_err(|source| DiscoveryError::ConfigParse {
                path: path.to_path_buf(),
                source,
            })?;
        config.into_peer_book()
    }

    /// Loads a peer book from the environment.
    ///
    /// - [`crate::defaults::ENV_COORDINATOR_ADDR`]: a comma-separated list
    ///   of `host:port` coordinator candidates, e.g.
    ///   `"10.0.0.1:7407,10.0.0.2:7407"`.
    /// - [`crate::defaults::ENV_DAEMON_ADDRS`]: `;`-separated groups of
    ///   `label=host:port,host:port`, e.g.
    ///   `"arm-01=10.0.0.5:7408;arm-02=10.0.0.6:7408,10.0.0.7:7408"`.
    ///
    /// Either or both may be unset, producing an empty book (or one with
    /// only the variable that was present) — this is not an error, since a
    /// deployment that relies purely on multicast discovery legitimately
    /// sets neither.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::EnvVar`] if a variable is set but does not match
    /// its documented grammar.
    pub fn from_env() -> DiscoveryResult<Self> {
        let mut book = Self::new();

        if let Ok(value) = std::env::var(ENV_COORDINATOR_ADDR) {
            for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let addr = part
                    .parse::<SocketAddr>()
                    .map_err(|e| DiscoveryError::EnvVar {
                        var: ENV_COORDINATOR_ADDR,
                        value: value.clone(),
                        reason: format!("{part:?} is not a valid host:port address: {e}"),
                    })?;
                book = book.with_coordinator(addr);
            }
        }

        if let Ok(value) = std::env::var(ENV_DAEMON_ADDRS) {
            for group in value.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let (label, addrs) =
                    group
                        .split_once('=')
                        .ok_or_else(|| DiscoveryError::EnvVar {
                            var: ENV_DAEMON_ADDRS,
                            value: value.clone(),
                            reason: format!("group {group:?} is missing its '=label=' separator"),
                        })?;
                let machine = MachineName::new(label).map_err(|_| DiscoveryError::EnvVar {
                    var: ENV_DAEMON_ADDRS,
                    value: value.clone(),
                    reason: format!("{label:?} is not a valid machine label"),
                })?;
                for part in addrs.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let addr = part
                        .parse::<SocketAddr>()
                        .map_err(|e| DiscoveryError::EnvVar {
                            var: ENV_DAEMON_ADDRS,
                            value: value.clone(),
                            reason: format!("{part:?} is not a valid host:port address: {e}"),
                        })?;
                    book = book.with_daemon(machine.clone(), addr);
                }
            }
        }

        Ok(book)
    }
}

/// The plain-struct, `serde`-visible shape [`PeerBook::from_file`] reads.
///
/// Deliberately structurally similar to (but independent of) a dataflow
/// manifest's `deploy:` section, so a future `astrs-manifest` importer can
/// map one onto the other mechanically without this crate ever depending on
/// `astrs-manifest`'s types.
///
/// # Examples
///
/// ```json
/// {
///   "coordinator": ["10.0.0.1:7407"],
///   "daemons": {
///     "arm-01": ["10.0.0.5:7408"],
///     "arm-02": ["10.0.0.6:7408", "10.0.0.7:7408"]
///   }
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PeerBookConfig {
    /// Coordinator candidate addresses, highest-preference first.
    pub coordinator: Vec<SocketAddr>,
    /// Per-machine daemon candidate addresses, highest-preference first.
    pub daemons: BTreeMap<String, Vec<SocketAddr>>,
}

impl PeerBookConfig {
    /// Validates every machine label and builds a [`PeerBook`].
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::InvalidMachineName`] if a key of `daemons` is not a
    /// legal [`MachineName`].
    pub fn into_peer_book(self) -> DiscoveryResult<PeerBook> {
        let mut book = PeerBook::new();
        for addr in self.coordinator {
            book = book.with_coordinator(addr);
        }
        for (label, addrs) in self.daemons {
            let machine = MachineName::new(label)?;
            for addr in addrs {
                book = book.with_daemon(machine.clone(), addr);
            }
        }
        Ok(book)
    }
}

impl TryFrom<PeerBookConfig> for PeerBook {
    type Error = DiscoveryError;

    fn try_from(config: PeerBookConfig) -> DiscoveryResult<Self> {
        config.into_peer_book()
    }
}

/// Where a discovered file lives — re-exported mainly so
/// [`DiscoveryError::ConfigRead`]/[`DiscoveryError::ConfigParse`] can name
/// it without every caller importing [`std::path::PathBuf`] themselves.
pub type ConfigPath = PathBuf;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn machine(s: &str) -> MachineName {
        MachineName::new(s).unwrap()
    }

    #[test]
    fn new_book_is_empty() {
        let book = PeerBook::new();
        assert!(book.is_empty());
        assert!(book.coordinator_candidates().is_empty());
        assert_eq!(book.machines().count(), 0);
    }

    #[test]
    fn builder_preserves_insertion_order() {
        let book = PeerBook::new()
            .with_coordinator(addr("10.0.0.1:7407"))
            .with_coordinator(addr("10.0.0.2:7407"))
            .with_daemon(machine("arm"), addr("10.0.0.5:7408"))
            .with_daemon(machine("arm"), addr("10.0.0.6:7408"));

        assert_eq!(
            book.coordinator_candidates(),
            &[addr("10.0.0.1:7407"), addr("10.0.0.2:7407")]
        );
        assert_eq!(
            book.daemon_candidates(&machine("arm")),
            &[addr("10.0.0.5:7408"), addr("10.0.0.6:7408")]
        );
        assert!(!book.is_empty());
    }

    #[test]
    fn unknown_machine_yields_no_candidates() {
        let book = PeerBook::new().with_daemon(machine("arm"), addr("10.0.0.5:7408"));
        assert!(book.daemon_candidates(&machine("other")).is_empty());
    }

    #[test]
    fn merged_appends_after_self() {
        let a = PeerBook::new()
            .with_coordinator(addr("10.0.0.1:7407"))
            .with_daemon(machine("arm"), addr("10.0.0.5:7408"));
        let b = PeerBook::new()
            .with_coordinator(addr("10.0.0.2:7407"))
            .with_daemon(machine("arm"), addr("10.0.0.6:7408"))
            .with_daemon(machine("leg"), addr("10.0.0.9:7408"));

        let merged = a.merged(b);
        assert_eq!(
            merged.coordinator_candidates(),
            &[addr("10.0.0.1:7407"), addr("10.0.0.2:7407")]
        );
        assert_eq!(
            merged.daemon_candidates(&machine("arm")),
            &[addr("10.0.0.5:7408"), addr("10.0.0.6:7408")]
        );
        assert_eq!(
            merged.daemon_candidates(&machine("leg")),
            &[addr("10.0.0.9:7408")]
        );
    }

    #[test]
    fn config_json_round_trips_into_a_peer_book() {
        let json = r#"{
            "coordinator": ["10.0.0.1:7407"],
            "daemons": {
                "arm-01": ["10.0.0.5:7408"],
                "arm-02": ["10.0.0.6:7408", "10.0.0.7:7408"]
            }
        }"#;
        let config: PeerBookConfig = serde_json::from_str(json).unwrap();
        let book: PeerBook = config.try_into().unwrap();

        assert_eq!(book.coordinator_candidates(), &[addr("10.0.0.1:7407")]);
        assert_eq!(
            book.daemon_candidates(&machine("arm-01")),
            &[addr("10.0.0.5:7408")]
        );
        assert_eq!(
            book.daemon_candidates(&machine("arm-02")),
            &[addr("10.0.0.6:7408"), addr("10.0.0.7:7408")]
        );
    }

    #[test]
    fn config_rejects_unknown_fields() {
        let json = r#"{"coordinator": [], "daemons": {}, "typo": true}"#;
        assert!(serde_json::from_str::<PeerBookConfig>(json).is_err());
    }

    #[test]
    fn config_rejects_invalid_machine_labels() {
        let json = r#"{"coordinator": [], "daemons": {"bad label": []}}"#;
        let config: PeerBookConfig = serde_json::from_str(json).unwrap();
        let err = PeerBook::try_from(config).unwrap_err();
        assert!(matches!(err, DiscoveryError::InvalidMachineName(_)));
    }

    #[test]
    fn from_file_reads_a_real_file() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-discovery-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers.json");
        std::fs::write(
            &path,
            r#"{"coordinator": ["10.0.0.1:7407"], "daemons": {}}"#,
        )
        .unwrap();

        let book = PeerBook::from_file(&path).unwrap();
        assert_eq!(book.coordinator_candidates(), &[addr("10.0.0.1:7407")]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn from_file_reports_missing_files() {
        let path = std::env::temp_dir().join("astrs-discovery-does-not-exist.json");
        let err = PeerBook::from_file(&path).unwrap_err();
        assert!(matches!(err, DiscoveryError::ConfigRead { .. }));
    }

    #[test]
    fn from_file_reports_malformed_json() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-discovery-badjson-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers.json");
        std::fs::write(&path, "not json").unwrap();

        let err = PeerBook::from_file(&path).unwrap_err();
        assert!(matches!(err, DiscoveryError::ConfigParse { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // `from_env` mutates process-global environment state, so every test
    // below runs single-threaded relative to each other (and relative to
    // every other env-mutating test in this crate) via a shared lock — see
    // `crate::test_support::with_env_vars`.
    use crate::test_support::with_env_vars;

    #[test]
    fn from_env_is_empty_when_unset() {
        with_env_vars(
            &[(ENV_COORDINATOR_ADDR, None), (ENV_DAEMON_ADDRS, None)],
            || {
                let book = PeerBook::from_env().unwrap();
                assert!(book.is_empty());
            },
        );
    }

    #[test]
    fn from_env_parses_coordinator_and_daemon_addrs() {
        with_env_vars(
            &[
                (ENV_COORDINATOR_ADDR, Some("10.0.0.1:7407,10.0.0.2:7407")),
                (
                    ENV_DAEMON_ADDRS,
                    Some("arm-01=10.0.0.5:7408;arm-02=10.0.0.6:7408,10.0.0.7:7408"),
                ),
            ],
            || {
                let book = PeerBook::from_env().unwrap();
                assert_eq!(
                    book.coordinator_candidates(),
                    &[addr("10.0.0.1:7407"), addr("10.0.0.2:7407")]
                );
                assert_eq!(
                    book.daemon_candidates(&machine("arm-01")),
                    &[addr("10.0.0.5:7408")]
                );
                assert_eq!(
                    book.daemon_candidates(&machine("arm-02")),
                    &[addr("10.0.0.6:7408"), addr("10.0.0.7:7408")]
                );
            },
        );
    }

    #[test]
    fn from_env_rejects_malformed_coordinator_addr() {
        with_env_vars(
            &[
                (ENV_COORDINATOR_ADDR, Some("not-an-addr")),
                (ENV_DAEMON_ADDRS, None),
            ],
            || {
                let err = PeerBook::from_env().unwrap_err();
                assert!(
                    matches!(err, DiscoveryError::EnvVar { var, .. } if var == ENV_COORDINATOR_ADDR)
                );
            },
        );
    }

    #[test]
    fn from_env_rejects_a_daemon_group_missing_its_label() {
        with_env_vars(
            &[
                (ENV_COORDINATOR_ADDR, None),
                (ENV_DAEMON_ADDRS, Some("10.0.0.5:7408")),
            ],
            || {
                let err = PeerBook::from_env().unwrap_err();
                assert!(
                    matches!(err, DiscoveryError::EnvVar { var, .. } if var == ENV_DAEMON_ADDRS)
                );
            },
        );
    }
}
