//! Per-connection session handling: the CLI and daemon actors that run once
//! a [`astrs_transport`] handshake has completed and settled which
//! [`astrs_wire::Role`] the connection speaks (blueprint §7.2, §7.3).

pub mod cli;
pub mod daemon;
pub mod outbound;

pub use outbound::CliOutbound;
