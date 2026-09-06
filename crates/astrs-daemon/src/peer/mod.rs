//! Cross-host routes: the daemon↔daemon leg (§6.4, §7.3, §12).
//!
//! > *`daemon↔daemon`: `PeerEvent` (route setup/teardown, output,
//! > output-closed, pool ops).*
//!
//! `astrs-transport` owns the connection: the framing, the handshake, the
//! per-route logical streams, the compression container, the reconnect
//! supervisor. This module owns what a *daemon* does with one — which peers it
//! keeps, which graph edges become routes, what a payload from a peer means,
//! and what happens to a consumer when the peer carrying its input goes away.
//!
//! | Module | Concern |
//! |---|---|
//! | [`config`] | [`PeerConfig`]: the peer port, the cluster token, the codec offered at setup |
//! | [`routes`] | [`PeerRouteTable`]: every route, in both directions, as pure state |
//! | [`link`] | [`PeerLink`]: one established connection and the task that reads it |
//! | [`manager`] | [`PeerManager`]: the peer table, the dial and accept paths, the frame conversation |
//!
//! # The conversation
//!
//! ```text
//!   producer's daemon                         consumer's daemon
//!   ─────────────────                         ─────────────────
//!   setup_route()  ─── RouteSetup ─────────►  PeerReaction::SetupRequested
//!                                             answer_setup()
//!   PeerReaction::RouteEstablished  ◄─ RouteAccept ─┘
//!
//!   forward()      ─── Output(seq) ────────►  PeerReaction::Deliver
//!   close_output() ─── OutputClosed ───────►  PeerReaction::InputClosed
//!   teardown_route() ─ RouteTeardown ──────►  PeerReaction::InputClosed
//!                  ◄── Ping / Ping{reply} ─►  (answered inside the manager)
//! ```
//!
//! # Where the boundary is
//!
//! The manager never consults the graph. It cannot answer "should I accept
//! this route?" — that needs the dataflow, the consumer's registration state
//! and the port's type — so it hands the question back as
//! [`PeerReaction::SetupRequested`] and the event loop answers. Everything the
//! manager *can* answer alone (which handle, which sequence number, which
//! stream, which peer) it answers alone.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::peer::{PeerConfig, PeerManager, PeerReaction};
//! use astrs_wire::{AuthToken, DaemonId, DataflowId, PeerEvent, RouteId, RouteKey, RouteSpec};
//!
//! let mut manager = PeerManager::new(
//!     DaemonId::generate(None),
//!     PeerConfig::new(AuthToken::from_bytes([1; 32])),
//! );
//!
//! let peer = DaemonId::generate(None);
//! let spec = RouteSpec::new(RouteKey::new(
//!     DataflowId::from_u128(1),
//!     "camera/image".parse()?,
//!     "detect/frames".parse()?,
//! ));
//!
//! let reactions = manager.handle_frame(
//!     &peer,
//!     PeerEvent::RouteSetup {
//!         route_id: RouteId::FIRST,
//!         route: spec,
//!         generation: 1,
//!         type_urn: None,
//!         max_payload_bytes: 1 << 20,
//!         pool_hint_bytes: None,
//!     },
//! );
//! assert_eq!(reactions[0].kind_name(), "setup_requested");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod config;
pub mod link;
pub mod manager;
pub mod routes;

pub use config::{
    DEFAULT_DIAL_TIMEOUT, DEFAULT_PEER_PORT, DEFAULT_PEER_TIMEOUT, DEFAULT_PING_INTERVAL,
    ENV_PEER_PORT, PeerConfig,
};
pub use link::PeerLink;
pub use manager::{PeerManager, PeerReaction};
pub use routes::{PeerRouteTable, RemoteRoute, RemoteRouteState};
