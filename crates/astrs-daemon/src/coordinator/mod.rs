//! The coordinator leg: the uplink, and everything a daemon does because it
//! belongs to a cluster (blueprint §4.2, §7.3, §12).
//!
//! > *Coordinator — one per cluster … Daemons dial **out** to it.*
//!
//! `astrs run` needs none of this: the CLI embeds a daemon, hands it a plan,
//! and reads the result back in-process. A *cluster* daemon is the other half
//! of the same object — the same event loop, the same state, the same planes —
//! driven by a coordinator over the wire instead of by a function call. This
//! module is that driver.
//!
//! ```text
//!            ┌──────────────────────── astrs-daemon ────────────────────────┐
//!            │                                                              │
//!  coordinator ◄── DaemonEvent ── UplinkSink ◄── ReportSink ◄── event loop  │
//!     :7407  ──── CoordinatorEvent ──► uplink task ──► DaemonEvent::        │
//!            │                                        CoordinatorFrame ──►  │
//!            │                                              apply::         │
//!            │                                       handle_coordinator_    │
//!            │                                              frame           │
//!            └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! | Module | Concern |
//! |---|---|
//! | [`config`] | [`UplinkConfig`]: where the coordinator is, what this daemon claims, the reconnect ladder |
//! | [`sink`] | [`UplinkSink`]: the bounded outbox §12's degraded-autonomous mode buffers into |
//! | [`link`] | the uplink task: dial → `Hello` → `Register` → pump → reconnect |
//! | [`apply`] | one arm per §24.1 instruction, executed against the local daemon |
//! | [`routes`] | [`PeerDirectory`]: the cross-daemon edges, who dials, and when a route opens (§6.4) |
//! | [`logs`] | [`LogHistory`]: the bounded ring `astrs logs` is answered from (§17) |
//! | [`state`] | [`ClusterState`]: the one field on the event loop that holds all of it |
//!
//! # Degraded-autonomous mode (§12)
//!
//! > *daemon enters degraded-autonomous mode after 20 s silence (keeps local
//! > dataflow running, buffers events), reconnects with backoff, resyncs via
//! > sequence-numbered `StateCatchUp`.*
//!
//! Three separate mechanisms, one per clause, and none of them stops anything:
//!
//! 1. **Keeps running.** Losing the link produces exactly one internal event,
//!    [`crate::session::DaemonEvent::CoordinatorLost`], whose handler logs and
//!    returns. No node is stopped, no ring is retired, no peer route is torn
//!    down — the coordinator owns the cluster's *plan*, and the daemon owns its
//!    *execution*.
//! 2. **Buffers, bounded.** [`UplinkSink`] keeps accepting reports into a ring
//!    that sheds liveness and telemetry before it sheds a lifecycle fact, so
//!    an hour-long outage costs a fixed amount of memory and loses nothing the
//!    coordinator's dataflow FSM depends on.
//! 3. **Resyncs.** The daemon's registration carries the highest catch-up
//!    sequence it has applied, so the replay after a reconnect starts *after*
//!    what it already has (see [`link::UplinkState::observe_catch_up`]).
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::coordinator::UplinkConfig;
//! use astrs_daemon::peer::PeerConfig;
//! use astrs_daemon::{Daemon, DaemonConfig};
//! use astrs_wire::{AuthToken, MachineName};
//!
//! let token = AuthToken::from_bytes([7; 32]);
//! let config = DaemonConfig::from_env()
//!     .with_machine(MachineName::new("robot-01")?)
//!     .with_peer(PeerConfig::new(token.clone()).with_loopback(0));
//!
//! let mut daemon = Daemon::new(config)?;
//! daemon.bind().await?;
//! daemon
//!     .connect_coordinator(
//!         UplinkConfig::new("127.0.0.1:7407".parse()?, token)
//!             .with_machine(MachineName::new("robot-01")?),
//!     )
//!     .await?;
//! daemon.run().await;
//! # Ok(()) }
//! ```

pub mod apply;
pub mod config;
pub mod link;
pub mod logs;
pub mod routes;
pub mod sink;
pub mod state;

pub use config::{
    DEFAULT_COORDINATOR_PORT, ENV_COORDINATOR_ADDR, ENV_COORDINATOR_PORT, UplinkConfig,
    resolve_coordinator_addr,
};
pub use link::{CONNECT_POLL, UplinkHandle, UplinkState, spawn_uplink};
pub use logs::{DEFAULT_LOG_HISTORY, LogBatch, LogHistory};
pub use routes::{PEER_RECONCILE_INTERVAL, PeerDirectory};
pub use sink::UplinkSink;
pub use state::ClusterState;
