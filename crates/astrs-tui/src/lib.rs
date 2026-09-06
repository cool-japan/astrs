//! The AstRS terminal monitor.
//!
//! What `astrs top` opens: a live ratatui view of a running cluster
//! (blueprint §5.2, §13), or of an `.arec` recording played back with no
//! cluster at all (`astrs top --replay`, §14):
//!
//! - **Dataflows** — the dataflow list, and the selected dataflow's
//!   per-node status/restarts/CPU/RSS/queue-depth table
//!   ([`tabs::dataflows`]).
//! - **Graph** — a compact layered ASCII rendering of the running topology
//!   with plane badges ([`tabs::graph`], [`layout`]).
//! - **Logs** — a level/node-filterable live tail, formatted identically
//!   to `astrs logs` ([`tabs::logs`]).
//! - **Timeline** — recent lifecycle events, HLC-ordered
//!   ([`tabs::timeline`]).
//!
//! # Architecture
//!
//! Every tab renders from a `&`[`view::ClusterSnapshot`], never from a
//! live connection directly — [`view::ClusterView`] is the trait that
//! produces one, with two implementations:
//! [`view::coordinator::CoordinatorSource`] (a live coordinator, polled on
//! every redraw tick) and [`view::replay::ReplaySource`] (an `.arec`
//! recording). This is what makes the whole crate testable with
//! [`ratatui::backend::TestBackend`] and no terminal, no coordinator, and
//! no file: every renderer, and [`app::handle_key`]'s input state machine,
//! is a pure function of a snapshot and a [`app::UiState`].
//!
//! [`terminal::run`] is the one place that touches a real terminal —
//! entering the alternate screen, running the draw/input loop
//! ([`event::EventLoop`]), and restoring the terminal on every exit path,
//! including a panic (see that module's docs).

pub mod app;
pub mod error;
pub mod layout;
pub mod tabs;
pub mod theme;
pub mod view;

mod event;
mod terminal;

pub use app::{App, Tab, UiState, handle_key};
pub use error::TuiError;
pub use event::{EventLoop, TuiEvent};
pub use layout::GraphLayout;
pub use terminal::run;
pub use view::coordinator::{CoordinatorSource, CoordinatorSourceError};
pub use view::replay::{ReplaySource, ReplaySourceError};
pub use view::{ClusterSnapshot, ClusterView, ConnectionStatus, ViewError};
