//! Local delivery — the daemon-mediated reliable path (§4.3, §6.3, §8.4).
//!
//! | Module | Concern |
//! |---|---|
//! | [`mailbox`] | One node's inbound queue set, over `astrs-scheduler`'s mux |
//! | [`router`] | Producer port → every local consumer, honouring queue configuration |
//! | [`virtual_src`] | `astrs/timer/*` off one shared wheel, `astrs/logs/*` as parsed filters |
//! | [`replay`] | The recorded clock a deterministic run drives the wheel from (§14) |
//!
//! # The two ports the daemon reserves
//!
//! Lifecycle events that are not about any particular input — a `Stop`, a
//! peer's `NodeFailed`, a parameter update — are delivered on the synthetic
//! input [`status_port`] (`astrs.status`, §8.4). One queue, one lane, one
//! ordering: a node that sees `InputClosed` and then `NodeFailed` sees them in
//! that order because they went through the same queue, not because of luck.
//!
//! [`logs_port`] (`astrs.logs`) is the corresponding default for a node that
//! subscribed to `astrs/logs` without naming its own input.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::local::{logs_port, status_port};
//!
//! assert_eq!(status_port().as_str(), "astrs.status");
//! assert_eq!(logs_port().as_str(), "astrs.logs");
//! ```

pub mod mailbox;
pub mod replay;
pub mod router;
pub mod virtual_src;

pub use mailbox::{DeliveryReport, NodeMailbox};
pub use replay::ReplaySource;
pub use router::{FanOutOutcome, LocalRouter, PayloadOrigin};
pub use virtual_src::{LogSubscriptions, TimerRegistry, VirtualSubscriber, timer_interval};

use astrs_wire::DataId;

/// The synthetic input lifecycle events arrive on (`astrs.status`, §8.4).
///
/// Built through [`DataId::sanitized`], which is total: this literal already
/// satisfies the grammar (the test below proves it), so the repair path is
/// never taken and the function needs no error case at its call sites.
#[must_use]
pub fn status_port() -> DataId {
    DataId::sanitized("astrs.status")
}

/// The synthetic input log records arrive on (`astrs.logs`, §8.4).
#[must_use]
pub fn logs_port() -> DataId {
    DataId::sanitized("astrs.logs")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_reserved_ports_are_legal_identifiers() {
        assert_eq!(status_port().as_str(), "astrs.status");
        assert_eq!(logs_port().as_str(), "astrs.logs");
        assert!(DataId::new("astrs.status").is_ok());
        assert!(DataId::new("astrs.logs").is_ok());
    }

    #[test]
    fn the_reserved_ports_are_distinct() {
        assert_ne!(status_port(), logs_port());
    }
}
