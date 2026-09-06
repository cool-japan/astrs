//! The daemon's merged event loop and its listeners (§4.3).
//!
//! | Module | Concern |
//! |---|---|
//! | [`core`] | [`Daemon`]: the `select!`, the HLC stamping, the deadline arithmetic |
//! | [`closure`] | When a closed output becomes an `InputClosed`, and what reason it carries (§12) |
//! | [`handlers`] | What the loop *does* about each event — split out for the 2000-line ceiling and because it is a real seam |
//! | [`listener`] | The UDS and loopback-TCP accept tasks, and the session-id minter they share |
//! | [`planes`] | Where the shared-memory, peer, health and tap machines meet the loop's state (§6.2–6.4, §12, §13) |
//! | [`replay`] | The six places the loop touches the recorded clock (§14) |

pub mod closure;
pub mod core;
pub mod handlers;
pub mod listener;
pub mod planes;
pub mod replay;

pub use core::{Daemon, IDLE_TICK, KILL_GRACE};
pub use listener::{
    ACCEPT_QUEUE_DEPTH, AcceptedNode, ConnectionOrigin, NodeListeners, NodeStream, SessionMinter,
    credentials_acceptable, daemon_uid,
};
