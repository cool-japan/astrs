//! [`Role`]: who is opening a connection.
//!
//! Blueprint §7.2 puts a role in every `Hello` because the handshake is
//! *leg-independent*: the same three messages open a CLI→coordinator link, a
//! daemon→coordinator link, a node→daemon link and a daemon→daemon link. The
//! role is what tells the acceptor which message family will follow, and
//! therefore which of its own subsystems should own the connection.

use core::fmt;
use core::str::FromStr;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::error::{IdError, IdKind};
use crate::frame::FrameKind;

/// The kind of endpoint that opened a connection (§7.2).
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameKind, Role};
///
/// assert_eq!(Role::Node.as_str(), "node");
/// assert_eq!(Role::Node.request_kind(), FrameKind::NodeRequest);
/// assert_eq!(Role::Node.event_kind(), FrameKind::NodeEvent);
/// assert_eq!("cli".parse::<Role>()?, Role::Cli);
/// # Ok::<(), astrs_wire::IdError>(())
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
pub enum Role {
    /// The `astrs` command-line tool talking to a coordinator.
    #[default]
    #[oxicode(variant = 0)]
    Cli,
    /// A daemon registering with the coordinator.
    #[oxicode(variant = 1)]
    Daemon,
    /// A node attaching to its local daemon.
    #[oxicode(variant = 2)]
    Node,
    /// A daemon opening a link to another daemon.
    #[oxicode(variant = 3)]
    Peer,
}

impl Role {
    /// Every role this build knows, in wire-index order.
    pub const ALL: &'static [Self] = &[Self::Cli, Self::Daemon, Self::Node, Self::Peer];

    /// A stable, lower-case name for logs, metrics labels and the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Daemon => "daemon",
            Self::Node => "node",
            Self::Peer => "peer",
        }
    }

    /// The frame family this role *sends* after the handshake.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameKind, Role};
    ///
    /// assert_eq!(Role::Cli.request_kind(), FrameKind::Control);
    /// assert_eq!(Role::Daemon.request_kind(), FrameKind::DaemonEvent);
    /// assert_eq!(Role::Peer.request_kind(), FrameKind::PeerEvent);
    /// ```
    #[must_use]
    pub const fn request_kind(self) -> FrameKind {
        match self {
            Self::Cli => FrameKind::Control,
            Self::Daemon => FrameKind::DaemonEvent,
            Self::Node => FrameKind::NodeRequest,
            Self::Peer => FrameKind::PeerEvent,
        }
    }

    /// The frame family this role *receives* after the handshake.
    ///
    /// The daemon↔daemon leg is symmetric: a peer both sends and receives
    /// [`FrameKind::PeerEvent`].
    #[must_use]
    pub const fn event_kind(self) -> FrameKind {
        match self {
            Self::Cli => FrameKind::ControlReply,
            Self::Daemon => FrameKind::CoordinatorEvent,
            Self::Node => FrameKind::NodeEvent,
            Self::Peer => FrameKind::PeerEvent,
        }
    }

    /// Whether this role addresses the coordinator rather than a daemon.
    #[must_use]
    pub const fn targets_coordinator(self) -> bool {
        matches!(self, Self::Cli | Self::Daemon)
    }

    /// Whether a connection from this role carries bulk payload.
    ///
    /// Node and peer links move payloads; a CLI link moves commands (and
    /// subscription fan-out, which is bulk but rate-limited).
    #[must_use]
    pub const fn carries_payload(self) -> bool {
        matches!(self, Self::Node | Self::Peer)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "cli" => Ok(Self::Cli),
            "daemon" => Ok(Self::Daemon),
            "node" => Ok(Self::Node),
            "peer" => Ok(Self::Peer),
            other => Err(IdError::Malformed {
                kind: IdKind::Name,
                value: crate::error::preview(other),
                reason: "expected one of cli, daemon, node, peer",
            }),
        }
    }
}

/// A set of [`Role`]s, as a bitmask.
///
/// An acceptor states which roles it serves — a daemon's node socket accepts
/// [`Role::Node`] and nothing else, a coordinator accepts [`Role::Cli`] and
/// [`Role::Daemon`] — and negotiation refuses anything outside the set with
/// [`crate::RefusalReason::RoleNotPermitted`]. A bitmask keeps the acceptor
/// configuration `Copy` and the membership test a single instruction.
///
/// # Examples
///
/// ```
/// use astrs_wire::{Role, RoleSet};
///
/// let coordinator = RoleSet::COORDINATOR;
/// assert!(coordinator.contains(Role::Cli));
/// assert!(coordinator.contains(Role::Daemon));
/// assert!(!coordinator.contains(Role::Node));
/// assert_eq!(coordinator.len(), 2);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleSet(u8);

impl RoleSet {
    /// The empty set: no role is accepted.
    pub const EMPTY: Self = Self(0);

    /// Every role this build knows.
    pub const ALL: Self = Self(0b1111);

    /// What a coordinator serves: CLIs and daemons.
    pub const COORDINATOR: Self = Self::EMPTY.with(Role::Cli).with(Role::Daemon);

    /// What a daemon serves on its node socket.
    pub const NODES: Self = Self::EMPTY.with(Role::Node);

    /// What a daemon serves on its peer socket.
    pub const PEERS: Self = Self::EMPTY.with(Role::Peer);

    /// The set containing exactly one role.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Role, RoleSet};
    ///
    /// assert_eq!(RoleSet::only(Role::Peer), RoleSet::PEERS);
    /// ```
    #[must_use]
    pub const fn only(role: Role) -> Self {
        Self::EMPTY.with(role)
    }

    /// This set with `role` added.
    #[must_use]
    pub const fn with(self, role: Role) -> Self {
        Self(self.0 | (1 << role as u8))
    }

    /// This set with `role` removed.
    #[must_use]
    pub const fn without(self, role: Role) -> Self {
        Self(self.0 & !(1 << role as u8))
    }

    /// Whether `role` is a member.
    #[must_use]
    pub const fn contains(self, role: Role) -> bool {
        self.0 & (1 << role as u8) != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many roles are in the set.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// The members, in wire-index order.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Role, RoleSet};
    ///
    /// assert_eq!(RoleSet::COORDINATOR.roles(), vec![Role::Cli, Role::Daemon]);
    /// ```
    #[must_use]
    pub fn roles(self) -> Vec<Role> {
        Role::ALL
            .iter()
            .copied()
            .filter(|role| self.contains(*role))
            .collect()
    }
}

impl fmt::Display for RoleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let mut first = true;
        for role in self.roles() {
            if !first {
                f.write_str("|")?;
            }
            f.write_str(role.as_str())?;
            first = false;
        }
        Ok(())
    }
}

impl FromIterator<Role> for RoleSet {
    fn from_iter<I: IntoIterator<Item = Role>>(roles: I) -> Self {
        roles.into_iter().fold(Self::EMPTY, Self::with)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn indices_are_frozen() {
        for (index, role) in Role::ALL.iter().enumerate() {
            let bytes = role.encode_to_vec().unwrap();
            assert_eq!(usize::from(bytes[0]), index, "{role} moved");
            assert_eq!(Role::decode_exact(&bytes).unwrap(), *role);
        }
        assert_eq!(Role::ALL.len(), 4);
        assert_eq!(Role::default(), Role::Cli);
    }

    #[test]
    fn names_round_trip_through_text() {
        for &role in Role::ALL {
            assert_eq!(role.to_string().parse::<Role>().unwrap(), role);
        }
        assert!("coordinator".parse::<Role>().is_err());
    }

    #[test]
    fn each_role_names_a_distinct_request_family() {
        let mut kinds: Vec<FrameKind> = Role::ALL.iter().map(|role| role.request_kind()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), Role::ALL.len());
    }

    #[test]
    fn request_and_event_kinds_pair_up_per_leg() {
        assert_eq!(Role::Cli.event_kind(), FrameKind::ControlReply);
        assert_eq!(Role::Daemon.event_kind(), FrameKind::CoordinatorEvent);
        assert_eq!(Role::Node.event_kind(), FrameKind::NodeEvent);
        // The peer leg is the symmetric one.
        assert_eq!(Role::Peer.event_kind(), Role::Peer.request_kind());
    }

    #[test]
    fn role_predicates_partition_the_set_as_documented() {
        assert!(Role::Cli.targets_coordinator());
        assert!(Role::Daemon.targets_coordinator());
        assert!(!Role::Node.targets_coordinator());
        assert!(!Role::Peer.targets_coordinator());

        assert!(Role::Node.carries_payload());
        assert!(Role::Peer.carries_payload());
        assert!(!Role::Cli.carries_payload());
    }

    #[test]
    fn serde_uses_snake_case_names() {
        for &role in Role::ALL {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            assert_eq!(serde_json::from_str::<Role>(&json).unwrap(), role);
        }
    }

    #[test]
    fn role_sets_hold_exactly_what_was_put_in_them() {
        assert!(RoleSet::EMPTY.is_empty());
        assert_eq!(RoleSet::ALL.len(), u32::try_from(Role::ALL.len()).unwrap());
        for &role in Role::ALL {
            assert!(RoleSet::ALL.contains(role));
            assert!(RoleSet::only(role).contains(role));
            assert_eq!(RoleSet::only(role).len(), 1);
            assert!(!RoleSet::EMPTY.contains(role));
            assert!(!RoleSet::ALL.without(role).contains(role));
        }
    }

    #[test]
    fn the_named_sets_match_their_documentation() {
        assert_eq!(RoleSet::COORDINATOR.roles(), vec![Role::Cli, Role::Daemon]);
        assert_eq!(RoleSet::NODES.roles(), vec![Role::Node]);
        assert_eq!(RoleSet::PEERS.roles(), vec![Role::Peer]);
        assert_eq!(RoleSet::ALL.roles(), Role::ALL.to_vec());
    }

    #[test]
    fn role_sets_collect_and_display() {
        let set: RoleSet = [Role::Peer, Role::Cli].into_iter().collect();
        assert_eq!(set.to_string(), "cli|peer");
        assert_eq!(RoleSet::EMPTY.to_string(), "none");
        assert_eq!(set.roles(), vec![Role::Cli, Role::Peer]);
    }
}
