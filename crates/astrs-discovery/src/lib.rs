//! Peer rendezvous for AstRS clusters.
//!
//! How a daemon finds its coordinator and its sibling daemons without a
//! routing middleware in the middle (blueprint §5.2):
//!
//! - Static peer lists from configuration and environment
//!   ([`PeerBook`]).
//! - A UDP multicast beacon ([`Beacon`]) with periodic announcements
//!   ([`sender::BeaconSender`]), liveness expiry and deduplication by
//!   daemon identity ([`watcher::BeaconWatcher`]).
//! - The rendezvous function ([`rendezvous::discover_coordinator`]) that
//!   feeds confirmed peers into `astrs-transport` for connection
//!   establishment.
//!
//! # Layout
//!
//! | Module | What it owns |
//! |---|---|
//! | [`beacon`] | The [`Beacon`] wire type: construction, HMAC `auth_tag`, codec |
//! | [`peer_book`] | [`PeerBook`]: static config, file/env loading |
//! | [`socket`] | [`socket::DiscoverySocket`]: the async transport seam + real `tokio` impl |
//! | [`sender`] | [`sender::BeaconSender`]: periodic jittered announce |
//! | [`watcher`] | [`watcher::BeaconWatcher`] / [`watcher::PeerTable`]: inbound beacons -> [`watcher::BeaconEvent`]s |
//! | [`rendezvous`] | [`rendezvous::discover_coordinator`]: static-then-multicast lookup |
//! | [`jitter`] | The jittered-interval helper `sender` uses |
//! | [`defaults`] | Well-known addresses, timings, environment variable names |
//! | [`error`] | [`DiscoveryError`], the one error type this crate returns |
//!
//! # Naming: `BeaconRole` and `BeaconEvent`, not `Role`/`PeerEvent`
//!
//! The two names above are deliberately not the shorter `Role` and
//! `PeerEvent` a first draft (and the plain-English task description this
//! crate was built against) would suggest. Both collide with unrelated,
//! already-`#[non_exhaustive]`-frozen top-level exports from
//! [`astrs_wire`]: a handshake [`astrs_wire::Role`] (`Cli|Daemon|Node|Peer`,
//! blueprint §7.2) and a daemon-to-daemon [`astrs_wire::PeerEvent`]
//! (`RouteSetup|RouteAccept|RouteTeardown|Output|OutputClosed|Ping`,
//! blueprint §24.1). `astrs-daemon` (W3) is expected to depend on both this
//! crate and `astrs-wire` at once, so an identical top-level name would
//! force `as`-renaming imports at every call site that needs both — cheaper
//! to settle now, while nothing downstream depends on this crate yet, than
//! after W3 exists. The `Beacon`-prefixed spelling also reads as a family
//! with [`Beacon`], [`BeaconSender`] and [`BeaconWatcher`].
//!
//! # Multicast availability
//!
//! Multicast is routinely unavailable in sandboxes, containers and CI
//! runners. Every layer here is built to degrade rather than fail when it
//! is missing: [`socket::bind`] reports [`socket::MulticastStatus::Degraded`]
//! instead of erroring when a group join fails, [`sender::BeaconSender`]
//! keeps sending to its unicast fallback list when a multicast send fails,
//! and [`rendezvous::discover_coordinator`] simply returns an empty list
//! (not an error) if a collection window sees no beacons at all. A
//! deployment that cannot use multicast at all still works end to end via
//! [`PeerBook`] and unicast fallback addresses alone.

pub mod beacon;
pub mod defaults;
pub mod error;
pub mod jitter;
pub mod peer_book;
pub mod rendezvous;
pub mod sender;
pub mod socket;
pub mod watcher;

#[cfg(test)]
mod test_support;

pub use beacon::{Beacon, BeaconRole};
pub use error::{DiscoveryError, DiscoveryResult};
pub use peer_book::{PeerBook, PeerBookConfig};
pub use rendezvous::{CandidateSource, CoordinatorCandidate, discover_coordinator};
pub use sender::{BeaconSender, SenderConfig};
pub use socket::{DegradedReason, DiscoverySocket, MulticastStatus, TokioSocket};
pub use watcher::{BeaconEvent, BeaconWatcher, PeerInfo, PeerTable, WatcherConfig};
