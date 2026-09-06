//! [`HaConfig`]: which coordinator this is, who its peers are, and where its
//! Raft log lives.
//!
//! # Peer specifications
//!
//! One flag per peer, in the same `id=address` shape the rest of the CLI uses
//! for id-keyed maps:
//!
//! ```text
//! astrs coordinator --ha-node-id 1 \
//!     --ha-peer 1=10.0.0.1:7601 \
//!     --ha-peer 2=10.0.0.2:7601 \
//!     --ha-peer 3=10.0.0.3:7601
//! ```
//!
//! `@` is accepted in place of `=` because `id@host:port` reads naturally and
//! costs nothing to allow. The **first** separator wins, which is the only
//! split that keeps an IPv6 address (full of colons, but never of `=` or `@`)
//! intact on the right-hand side.
//!
//! Every coordinator in a cluster is given the *same* peer list, including its
//! own entry; `--ha-node-id` picks which one this process is. That symmetry is
//! deliberate: one file, one flag set, deployed identically everywhere, with
//! exactly one value differing per machine.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use astrs_raft::{FsyncPolicy, PeerId, RaftConfig};

use crate::error::{CoordinatorError, Result};

/// The separators accepted between a peer id and its address.
pub const PEER_SEPARATORS: [char; 2] = ['=', '@'];

/// The file name the Raft write-ahead log gets inside the store directory.
pub const RAFT_LOG_FILE: &str = "raft.wal";

/// The default Raft tick, in wall-clock terms.
pub const DEFAULT_HA_TICK: Duration = Duration::from_millis(50);

/// The default election timeout floor, in ticks (500 ms at the default tick).
pub const DEFAULT_HA_ELECTION_TICKS: u64 = 10;

/// How a coordinator participates in a replicated set.
#[derive(Debug, Clone)]
pub struct HaConfig {
    /// Which peer this coordinator is.
    pub node_id: PeerId,
    /// Every coordinator in the set, this one included.
    pub peers: BTreeMap<PeerId, SocketAddr>,
    /// Where the Raft write-ahead log lives. `None` keeps the log in memory,
    /// which is correct only for tests and for a single-process cluster.
    pub log_path: Option<PathBuf>,
    /// How long one Raft tick is.
    pub tick_interval: Duration,
    /// The election timeout floor, in ticks.
    pub election_timeout_ticks: u64,
    /// How aggressively the Raft log is flushed.
    pub fsync: FsyncPolicy,
}

impl HaConfig {
    /// A configuration for `node_id` over `peers`.
    #[must_use]
    pub fn new(node_id: PeerId, peers: BTreeMap<PeerId, SocketAddr>) -> Self {
        Self {
            node_id,
            peers,
            log_path: None,
            tick_interval: DEFAULT_HA_TICK,
            election_timeout_ticks: DEFAULT_HA_ELECTION_TICKS,
            fsync: FsyncPolicy::Always,
        }
    }

    /// Parses one `--ha-node-id` and a list of `--ha-peer` specifications.
    ///
    /// # Errors
    ///
    /// [`CoordinatorError::InvalidArgument`] if a specification has no
    /// separator, an unparsable id, an unparsable address, a duplicate id, or
    /// if `node_id` does not appear in the list.
    pub fn parse(node_id: u64, peers: &[String]) -> Result<Self> {
        let mut parsed = BTreeMap::new();
        for spec in peers {
            let (id, address) = parse_peer(spec)?;
            if parsed.insert(id, address).is_some() {
                return Err(CoordinatorError::invalid(format!(
                    "peer id {} is listed twice in --ha-peer",
                    id.get()
                )));
            }
        }
        let node_id = PeerId::new(node_id);
        if !parsed.contains_key(&node_id) {
            return Err(CoordinatorError::invalid(format!(
                "--ha-node-id {} does not appear in the --ha-peer list ({} entries)",
                node_id.get(),
                parsed.len()
            )));
        }
        Ok(Self::new(node_id, parsed))
    }

    /// Puts the Raft log in `directory`, under [`RAFT_LOG_FILE`].
    #[must_use]
    pub fn with_log_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.log_path = Some(directory.into().join(RAFT_LOG_FILE));
        self
    }

    /// Uses an explicit log file path.
    #[must_use]
    pub fn with_log_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.log_path = Some(path.into());
        self
    }

    /// Sets the Raft tick and election-timeout floor together, since only
    /// their product is meaningful.
    #[must_use]
    pub const fn with_timing(mut self, tick: Duration, election_ticks: u64) -> Self {
        self.tick_interval = tick;
        self.election_timeout_ticks = election_ticks;
        self
    }

    /// Sets the write-ahead log's durability policy.
    #[must_use]
    pub const fn with_fsync(mut self, fsync: FsyncPolicy) -> Self {
        self.fsync = fsync;
        self
    }

    /// The address this coordinator's own Raft listener binds.
    ///
    /// # Errors
    ///
    /// [`CoordinatorError::InvalidArgument`] if this node's id is somehow not
    /// in its own peer map, which [`HaConfig::parse`] already refuses.
    pub fn listen_addr(&self) -> Result<SocketAddr> {
        self.peers.get(&self.node_id).copied().ok_or_else(|| {
            CoordinatorError::invalid(format!(
                "this coordinator's own id {} has no address",
                self.node_id.get()
            ))
        })
    }

    /// Where `peer` can be reached, for a leader hint.
    #[must_use]
    pub fn address_of(&self, peer: PeerId) -> Option<SocketAddr> {
        self.peers.get(&peer).copied()
    }

    /// Every peer except this one, which is who the transport dials.
    #[must_use]
    pub fn other_peers(&self) -> Vec<(PeerId, SocketAddr)> {
        self.peers
            .iter()
            .filter(|(id, _)| **id != self.node_id)
            .map(|(id, address)| (*id, *address))
            .collect()
    }

    /// The Raft configuration this implies.
    #[must_use]
    pub fn to_raft_config(&self) -> RaftConfig {
        RaftConfig::new(self.node_id)
            .with_peers(self.peers.iter().map(|(id, address)| (*id, *address)))
            .with_tick_interval(self.tick_interval)
            // A jitter equal to the floor: the spread has to be wide enough
            // that three coordinators starting together do not campaign in
            // lockstep and split the vote indefinitely.
            .with_election_timeout(self.election_timeout_ticks, self.election_timeout_ticks)
            .with_heartbeat_ticks((self.election_timeout_ticks / 5).max(1))
    }
}

/// Parses one `id=address` (or `id@address`) peer specification.
fn parse_peer(spec: &str) -> Result<(PeerId, SocketAddr)> {
    // The first separator wins: an IPv6 address is full of colons but never
    // of `=` or `@`, so splitting once on the left keeps it intact.
    let Some(position) = spec.find(PEER_SEPARATORS) else {
        return Err(CoordinatorError::invalid(format!(
            "--ha-peer '{spec}' must be written 'id=host:port'"
        )));
    };
    let (id_text, rest) = spec.split_at(position);
    let address_text = rest.get(1..).unwrap_or_default();
    let id: u64 = id_text.trim().parse().map_err(|_| {
        CoordinatorError::invalid(format!("--ha-peer '{spec}' has a non-numeric peer id"))
    })?;
    if id == 0 {
        return Err(CoordinatorError::invalid(
            "--ha-peer ids start at 1; 0 is reserved".to_owned(),
        ));
    }
    let address: SocketAddr = address_text.trim().parse().map_err(|_| {
        CoordinatorError::invalid(format!(
            "--ha-peer '{spec}' has an address that is not host:port"
        ))
    })?;
    Ok((PeerId::new(id), address))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn specs() -> Vec<String> {
        vec![
            "1=127.0.0.1:7601".to_owned(),
            "2=127.0.0.1:7602".to_owned(),
            "3=127.0.0.1:7603".to_owned(),
        ]
    }

    #[test]
    fn a_three_peer_list_parses_and_names_this_node() {
        let config = HaConfig::parse(2, &specs()).unwrap();
        assert_eq!(config.node_id, PeerId::new(2));
        assert_eq!(config.peers.len(), 3);
        assert_eq!(config.listen_addr().unwrap().port(), 7602);
        assert_eq!(config.other_peers().len(), 2);
        assert_eq!(
            config.address_of(PeerId::new(3)).map(|a| a.port()),
            Some(7603)
        );
    }

    #[test]
    fn the_at_separator_is_accepted_too() {
        let config = HaConfig::parse(1, &["1@127.0.0.1:7601".to_owned()]).unwrap();
        assert_eq!(config.listen_addr().unwrap().port(), 7601);
    }

    #[test]
    fn an_ipv6_address_survives_the_split() {
        // The reason the split is on the *first* separator: an IPv6 address
        // is full of colons.
        let config = HaConfig::parse(4, &["4=[::1]:7604".to_owned()]).unwrap();
        assert_eq!(
            config.listen_addr().unwrap(),
            "[::1]:7604".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn a_specification_without_a_separator_is_refused() {
        let error = HaConfig::parse(1, &["127.0.0.1:7601".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("id=host:port"), "{error}");
    }

    #[test]
    fn a_non_numeric_id_is_refused() {
        let error = HaConfig::parse(1, &["one=127.0.0.1:7601".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("non-numeric"), "{error}");
    }

    #[test]
    fn a_malformed_address_is_refused() {
        let error = HaConfig::parse(1, &["1=not-an-address".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("host:port"), "{error}");
    }

    #[test]
    fn a_duplicate_peer_id_is_refused() {
        let error = HaConfig::parse(
            1,
            &["1=127.0.0.1:7601".to_owned(), "1=127.0.0.1:7699".to_owned()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn a_node_id_outside_the_peer_list_is_refused() {
        // Otherwise this coordinator would start, never be voted for, and
        // never explain why.
        let error = HaConfig::parse(9, &specs()).unwrap_err();
        assert!(error.to_string().contains("does not appear"), "{error}");
    }

    #[test]
    fn peer_id_zero_is_reserved() {
        let error = HaConfig::parse(0, &["0=127.0.0.1:7601".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("start at 1"), "{error}");
    }

    #[test]
    fn the_raft_configuration_is_valid_and_keeps_heartbeats_inside_the_timeout() {
        let config = HaConfig::parse(1, &specs()).unwrap();
        let raft = config.to_raft_config();
        raft.validate().expect("a usable Raft configuration");
        assert!(raft.heartbeat_ticks < raft.election_timeout_ticks);
        assert_eq!(raft.id, PeerId::new(1));
        assert_eq!(raft.peers.len(), 3);
    }

    #[test]
    fn a_very_short_election_timeout_still_leaves_room_for_a_heartbeat() {
        let config = HaConfig::parse(1, &specs())
            .unwrap()
            .with_timing(Duration::from_millis(2), 2);
        let raft = config.to_raft_config();
        assert_eq!(raft.heartbeat_ticks, 1);
        raft.validate().expect("a usable Raft configuration");
    }

    #[test]
    fn the_log_path_can_be_set_by_directory_or_by_file() {
        let config = HaConfig::parse(1, &specs())
            .unwrap()
            .with_log_dir(std::env::temp_dir());
        assert!(
            config
                .log_path
                .as_ref()
                .is_some_and(|path| path.ends_with(RAFT_LOG_FILE))
        );

        let explicit = HaConfig::parse(1, &specs())
            .unwrap()
            .with_log_path(std::env::temp_dir().join("custom.wal"))
            .with_fsync(FsyncPolicy::Never);
        assert!(
            explicit
                .log_path
                .as_ref()
                .is_some_and(|path| path.ends_with("custom.wal"))
        );
        assert_eq!(explicit.fsync, FsyncPolicy::Never);
    }

    #[test]
    fn an_in_memory_log_is_the_default() {
        assert!(HaConfig::parse(1, &specs()).unwrap().log_path.is_none());
    }
}
