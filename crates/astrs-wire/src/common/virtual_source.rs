//! The wire form of a virtual input's producer (blueprint §8.4).
//!
//! A virtual source — `astrs/timer/hz/50`, `astrs/logs/warn/camera`,
//! `astrs/status` — is written in a manifest as a *path*, but on the wire it
//! has to be an ordinary [`PortRef`], because that is what every
//! [`crate::InputSpec`] carries and what every route table is keyed by. This
//! module is the one place that conversion is defined.
//!
//! ```text
//!   manifest      astrs/timer/millis/20
//!                        │  encode: reserved node id + '/'→'.'
//!                        ▼
//!   wire          PortRef { node: "astrs", port: "timer.millis.20" }
//!                        │  decode: '.'→'/'
//!                        ▼
//!   daemon        astrs/timer/millis/20 ──► the timer wheel (§11.1)
//! ```
//!
//! # Why it lives here and not in a producer or a consumer
//!
//! Two processes have to agree on it. The **coordinator** encodes it when it
//! expands a manifest into the [`crate::NodeSpawnSpec`]s it dispatches; the
//! **daemon** decodes it to decide which of its local sources — timer wheel,
//! log fan-out, supervisor — should serve that input. A copy of the rule on
//! each side is a protocol split-brain waiting to happen, and it already
//! happened once: the coordinator omitted virtual inputs from the spec
//! entirely, so the daemon's decoder could never fire and an
//! `astrs/timer/*` input silently never ticked on a cluster while working
//! perfectly under `astrs run`.
//!
//! The `.` separator, rather than keeping `/`: a [`crate::DataId`]'s charset
//! excludes `/` (it is the `node/port` separator itself), so the path
//! segments have to be joined with something else, and `.` is already the
//! separator the reserved `astrs.status` / `astrs.logs` ports use.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{is_virtual_port, virtual_port_ref, virtual_source_text, VIRTUAL_NODE};
//!
//! let port = virtual_port_ref("astrs/timer/millis/20")?;
//! assert_eq!(port.node().as_str(), VIRTUAL_NODE);
//! assert_eq!(port.port().as_str(), "timer.millis.20");
//! assert!(is_virtual_port(&port));
//! assert_eq!(virtual_source_text(&port), "astrs/timer/millis/20");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use crate::error::IdError;
use crate::ids::{DataId, NodeId, PortRef};

/// The reserved producer node id every virtual source lives under (§8.4).
///
/// No manifest node may take this id — `astrs-manifest`'s validator refuses
/// it — which is what makes [`is_virtual_port`] a decision rather than a
/// guess.
pub const VIRTUAL_NODE: &str = "astrs";

/// The `PortRef` a virtual-source path is carried as on the wire.
///
/// Accepts the path with or without its `astrs/` prefix, so a caller that
/// already stripped it (or that stores the tail alone) does not have to
/// re-add one.
///
/// # Errors
///
/// [`IdError`] if the remaining path is not a legal [`DataId`] once its
/// separators are folded — an empty source, or one carrying a character the
/// port charset excludes.
///
/// # Examples
///
/// ```
/// use astrs_wire::virtual_port_ref;
///
/// assert_eq!(virtual_port_ref("astrs/status")?.port().as_str(), "status");
/// assert_eq!(virtual_port_ref("logs/warn")?.port().as_str(), "logs.warn");
/// assert!(virtual_port_ref("astrs/").is_err());
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
pub fn virtual_port_ref(source: &str) -> Result<PortRef, IdError> {
    let tail = source
        .strip_prefix(VIRTUAL_NODE)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(source);
    Ok(PortRef::new(
        NodeId::new(VIRTUAL_NODE)?,
        DataId::new(tail.replace('/', "."))?,
    ))
}

/// Whether a port reference names a virtual source rather than a real
/// producer's output.
///
/// # Examples
///
/// ```
/// use astrs_wire::{is_virtual_port, virtual_port_ref, PortRef};
///
/// assert!(is_virtual_port(&virtual_port_ref("astrs/timer/hz/50")?));
/// assert!(!is_virtual_port(&PortRef::from_parts("camera", "frames")?));
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[must_use]
pub fn is_virtual_port(port: &PortRef) -> bool {
    port.node().as_str() == VIRTUAL_NODE
}

/// The virtual-source path a [`PortRef`] came from.
///
/// The exact inverse of [`virtual_port_ref`] for every source that round
/// trips, which is every one this protocol defines: no virtual-source path
/// segment contains a `.`.
///
/// # Examples
///
/// ```
/// use astrs_wire::{virtual_port_ref, virtual_source_text};
///
/// for source in ["astrs/timer/hz/50", "astrs/logs/warn/camera", "astrs/status"] {
///     assert_eq!(virtual_source_text(&virtual_port_ref(source)?), source);
/// }
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[must_use]
pub fn virtual_source_text(port: &PortRef) -> String {
    format!("{VIRTUAL_NODE}/{}", port.port().as_str().replace('.', "/"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Every virtual source §8.4 defines survives the round trip, byte for
    /// byte. This is the compatibility contract between the coordinator's
    /// encoder and the daemon's decoder: if this list ever stopped round
    /// tripping, a cluster would silently lose that source.
    #[test]
    fn every_virtual_source_round_trips() {
        for source in [
            "astrs/timer/millis/20",
            "astrs/timer/secs/2",
            "astrs/timer/hz/50",
            "astrs/logs",
            "astrs/logs/warn",
            "astrs/logs/warn/camera",
            "astrs/status",
        ] {
            let port = virtual_port_ref(source).unwrap_or_else(|error| panic!("{source}: {error}"));
            assert_eq!(port.node().as_str(), VIRTUAL_NODE, "{source}");
            assert!(is_virtual_port(&port), "{source}");
            assert_eq!(virtual_source_text(&port), source);
        }
    }

    /// The exact wire form, spelled out rather than derived — a change here
    /// is a protocol change and must be visible in a diff.
    #[test]
    fn the_wire_form_is_the_reserved_node_and_a_dotted_tail() {
        let port = virtual_port_ref("astrs/timer/millis/20").unwrap();
        assert_eq!(port.node().as_str(), "astrs");
        assert_eq!(port.port().as_str(), "timer.millis.20");
        assert_eq!(port.to_string(), "astrs/timer.millis.20");
    }

    /// A caller that already stripped the prefix gets the same answer.
    #[test]
    fn the_prefix_is_optional() {
        assert_eq!(
            virtual_port_ref("timer/hz/50").unwrap(),
            virtual_port_ref("astrs/timer/hz/50").unwrap()
        );
    }

    /// An ordinary producer port is not mistaken for a virtual one.
    #[test]
    fn a_real_producer_is_not_virtual() {
        let port = PortRef::from_parts("camera", "frames").unwrap();
        assert!(!is_virtual_port(&port));
    }

    /// A source with nothing after the prefix is rejected rather than
    /// producing a port with an empty name.
    #[test]
    fn an_empty_tail_is_refused() {
        assert!(virtual_port_ref("astrs/").is_err());
        assert!(virtual_port_ref("").is_err());
    }
}
