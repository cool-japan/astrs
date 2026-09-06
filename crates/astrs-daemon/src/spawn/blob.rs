//! The daemon-owned variables — the last word on a child's environment (§16).
//!
//! Two variables, applied after everything else and never negotiable:
//!
//! | Variable | Contents |
//! |---|---|
//! | `ASTRS_NODE_CONFIG` | oxicode + base64 of an [`astrs_wire::NodeConfig`] — the spec, generation, dial endpoints, auth token and limits |
//! | `ASTRS_RUN_PARENT_PID` | the orphan guard: the pid a node should exit with when `astrs run` hosts the daemon in the CLI process (§4.2) |
//!
//! Both names live in the `ASTRS_` namespace the manifest denylist refuses
//! ([`crate::spawn::deny_reason`]), so "daemon-owned wins" is enforced twice:
//! a manifest cannot even name them, and if it somehow did, these are applied
//! last.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::spawn::{DaemonOwnedVars, EnvPolicy};
//! use astrs_wire::{AuthToken, DaemonId, DataflowId, NodeConfig, NodeId, NodeSource, NodeSpawnSpec};
//! use std::collections::BTreeMap;
//!
//! let spec = NodeSpawnSpec::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     3,
//!     NodeSource::Executable { path: "./camera".into() },
//! );
//! let config = NodeConfig::new(spec, DaemonId::generate(None), AuthToken::ZERO)
//!     .with_endpoint("uds:///run/astrs/daemon.sock");
//!
//! let owned = DaemonOwnedVars::new(&config)?.with_run_parent_pid(Some(42));
//! let mut env = EnvPolicy::new().build(&BTreeMap::new(), &BTreeMap::new())?;
//! owned.apply(&mut env);
//!
//! assert!(env.contains("ASTRS_NODE_CONFIG"));
//! assert_eq!(env.get("ASTRS_RUN_PARENT_PID"), Some("42"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use astrs_wire::{ENV_NODE_CONFIG, ENV_RUN_PARENT_PID, NodeConfig, WireError};

use crate::spawn::env::BuiltEnv;

/// The variables the daemon sets on every node it spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonOwnedVars {
    /// The encoded handshake blob.
    node_config: String,
    /// The orphan-guard pid, when one is armed.
    run_parent_pid: Option<u32>,
}

impl DaemonOwnedVars {
    /// Encodes `config` into the handshake blob.
    ///
    /// # Errors
    ///
    /// [`WireError`] if the configuration cannot be encoded, which for this
    /// type means an allocation failure.
    pub fn new(config: &NodeConfig) -> Result<Self, WireError> {
        Ok(Self {
            node_config: config.to_env_value()?,
            run_parent_pid: None,
        })
    }

    /// Arms (or disarms) the orphan guard.
    #[must_use]
    pub const fn with_run_parent_pid(mut self, pid: Option<u32>) -> Self {
        self.run_parent_pid = pid;
        self
    }

    /// The encoded handshake blob.
    #[must_use]
    pub fn node_config(&self) -> &str {
        &self.node_config
    }

    /// The orphan-guard pid, if armed.
    #[must_use]
    pub const fn run_parent_pid(&self) -> Option<u32> {
        self.run_parent_pid
    }

    /// The name/value pairs, in the order they are applied.
    #[must_use]
    pub fn pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![(ENV_NODE_CONFIG.to_string(), self.node_config.clone())];
        if let Some(pid) = self.run_parent_pid {
            pairs.push((ENV_RUN_PARENT_PID.to_string(), pid.to_string()));
        }
        pairs
    }

    /// Writes these variables into `env`, last and unconditionally.
    pub fn apply(&self, env: &mut BuiltEnv) {
        env.set_daemon_owned(self.pairs());
    }

    /// Decodes the blob back, for a test or a diagnostic.
    ///
    /// # Errors
    ///
    /// [`astrs_wire::NodeConfigError`] if the blob is not a valid
    /// configuration — which, for a blob this type produced, cannot happen,
    /// and is therefore worth surfacing rather than asserting.
    pub fn decode(&self) -> Result<NodeConfig, astrs_wire::NodeConfigError> {
        NodeConfig::from_env_value(&self.node_config)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use astrs_wire::{AuthToken, DaemonId, DataflowId, NodeId, NodeSource, NodeSpawnSpec};

    use super::*;
    use crate::spawn::env::EnvPolicy;

    fn config() -> NodeConfig {
        let spec = NodeSpawnSpec::new(
            DataflowId::from_u128(9),
            NodeId::new("camera").unwrap(),
            3,
            NodeSource::Executable {
                path: "./camera".into(),
            },
        );
        NodeConfig::new(spec, DaemonId::generate(None), AuthToken::ZERO)
            .with_endpoint("uds:///run/astrs/daemon.sock")
    }

    #[test]
    fn the_blob_round_trips_through_the_variable() {
        let owned = DaemonOwnedVars::new(&config()).unwrap();
        let decoded = owned.decode().unwrap();
        assert_eq!(decoded.generation(), 3);
        assert_eq!(decoded.node().as_str(), "camera");
        assert_eq!(decoded.endpoints, ["uds:///run/astrs/daemon.sock"]);
    }

    #[test]
    fn the_orphan_guard_is_optional() {
        let bare = DaemonOwnedVars::new(&config()).unwrap();
        assert_eq!(bare.pairs().len(), 1);
        assert_eq!(bare.run_parent_pid(), None);

        let guarded = bare.with_run_parent_pid(Some(7));
        let pairs = guarded.pairs();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[1], (ENV_RUN_PARENT_PID.to_string(), "7".to_string()));
    }

    #[test]
    fn the_config_variable_comes_first() {
        let owned = DaemonOwnedVars::new(&config())
            .unwrap()
            .with_run_parent_pid(Some(1));
        assert_eq!(owned.pairs()[0].0, ENV_NODE_CONFIG);
    }

    #[test]
    fn applying_writes_both_variables() {
        let owned = DaemonOwnedVars::new(&config())
            .unwrap()
            .with_run_parent_pid(Some(42));
        let mut env = EnvPolicy::new()
            .build(&BTreeMap::new(), &BTreeMap::new())
            .unwrap();
        owned.apply(&mut env);
        assert_eq!(env.get(ENV_NODE_CONFIG), Some(owned.node_config()));
        assert_eq!(env.get(ENV_RUN_PARENT_PID), Some("42"));
    }

    #[test]
    fn the_blob_is_a_legal_environment_value() {
        let owned = DaemonOwnedVars::new(&config()).unwrap();
        let blob = owned.node_config();
        assert!(!blob.is_empty());
        assert!(!blob.contains('\0'));
        assert!(blob.is_ascii());
    }

    #[test]
    fn two_generations_produce_two_blobs() {
        let mut later = config();
        later.spec.generation = 4;
        assert_ne!(
            DaemonOwnedVars::new(&config()).unwrap().node_config(),
            DaemonOwnedVars::new(&later).unwrap().node_config(),
            "the generation is stamped into the blob"
        );
    }
}
