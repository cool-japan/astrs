//! The `module:` header (blueprint §8.5): what makes a manifest file usable
//! as a reusable sub-graph included by another manifest's `module:`-sourced
//! node, and the reserved node id internal nodes use to wire to that
//! sub-graph's boundary.
//!
//! See [`crate::expand`] for the flattening algorithm that consumes this
//! header.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The reserved node id internal nodes use to wire to their module's
/// boundary ports (blueprint §8.5).
///
/// An internal node's input `source: _mod/frames` reads the module's
/// declared boundary input `frames` — the including manifest's
/// `module:`-sourced node supplies the actual value via its own `inputs:`
/// map. Symmetrically, an internal node surfaces one of its own outputs as
/// the module's boundary output `detections` by naming that output either
/// `detections` or `_mod/detections` — see [`crate::expand`]'s module docs
/// for exactly how the two spellings are matched; `detections` (the bare
/// name, matching the module header's own `outputs` list) is the
/// recommended spelling for anything hand-written, since `_mod/detections`
/// only exists as an input-side reference target, not as a name a human
/// needs to invent for an output.
///
/// Using this string as an ordinary node id, or as a `source:` outside any
/// module context, is rejected by [`crate::Manifest::validate`] — see
/// [`crate::UnresolvedReferenceReason::UnknownNode`] and
/// [`crate::UnresolvedReferenceReason::UnknownModuleInput`].
pub const MODULE_BOUNDARY_NODE_ID: &str = "_mod";

/// The `module:` header at a manifest root (blueprint §8.5), declaring that
/// this manifest is a reusable sub-graph — includable by another manifest's
/// node via that node's own `module:` field (a path to *this* manifest's
/// file) — rather than a directly-runnable top-level dataflow.
///
/// A manifest carrying this header still parses and
/// [validates](crate::Manifest::validate) like any other: nothing prevents
/// running it standalone (its internal `_mod/<port>` references simply
/// resolve against `inputs`/`outputs` below, with no external caller
/// supplying real values — see [`crate::Manifest::validate`]'s module-aware
/// resolution). Module semantics only take effect when another manifest's
/// [`crate::Node::module`] actually references this file, and that
/// including manifest is run through [`crate::expand`].
///
/// # Examples
///
/// ```
/// use astrs_manifest::Manifest;
///
/// let yaml = "\
/// module:
///   name: perception
///   inputs: [frames]
///   outputs: [detections]
/// nodes:
///   - id: detector
///     path: ./detector
///     inputs:
///       frames: _mod/frames
///     outputs: [detections]
/// ";
/// let manifest = Manifest::from_yaml_str(yaml)?;
/// manifest.validate()?;
/// let header = manifest.module.as_ref().ok_or("expected a module header")?;
/// assert_eq!(header.name, "perception");
/// assert_eq!(header.inputs, vec!["frames".to_string()]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleHeader {
    /// This module's display name.
    ///
    /// Documentation only: [`crate::expand`] identifies a module
    /// *instantiation* by the including manifest's hosting node id, never
    /// by this field, so two modules may share a name without conflict.
    pub name: String,
    /// The boundary input port names this module declares. An internal
    /// node's `source: _mod/<name>` is only valid for a `<name>` listed
    /// here — see [`crate::Manifest::validate`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// The boundary output port names this module declares. See
    /// [`crate::expand`] for how an internal node's own output surfaces as
    /// one of these to whatever manifest includes this module.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_full_header() {
        let yaml = "name: perception\ninputs: [frames]\noutputs: [detections]\n";
        let header: ModuleHeader = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(header.name, "perception");
        assert_eq!(header.inputs, vec!["frames".to_string()]);
        assert_eq!(header.outputs, vec!["detections".to_string()]);
    }

    #[test]
    fn inputs_and_outputs_default_to_empty() {
        let header: ModuleHeader = astrs_yaml::from_str("name: sink-only\n").unwrap();
        assert!(header.inputs.is_empty());
        assert!(header.outputs.is_empty());
    }

    #[test]
    fn requires_name() {
        assert!(astrs_yaml::from_str::<ModuleHeader>("inputs: [a]\n").is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(astrs_yaml::from_str::<ModuleHeader>("name: x\nbogus: 1\n").is_err());
    }

    #[test]
    fn round_trips_and_omits_empty_lists() {
        let header = ModuleHeader {
            name: "leaf".to_string(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        };
        let yaml = astrs_yaml::to_string(&header).unwrap();
        assert!(!yaml.contains("inputs"), "yaml was: {yaml}");
        assert!(!yaml.contains("outputs"), "yaml was: {yaml}");
        let back: ModuleHeader = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(header, back);
    }

    #[test]
    fn boundary_node_id_is_the_documented_sentinel() {
        assert_eq!(MODULE_BOUNDARY_NODE_ID, "_mod");
    }
}
