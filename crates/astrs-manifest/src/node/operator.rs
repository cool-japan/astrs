//! Runtime-hosted operator entries under a node's `operators:` source (§9.3).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{HubSource, Input};

/// A runtime-hosted operator entry under a node's `operators:` source
/// (§9.3).
///
/// # `operator` names the type; the locators say where to find it
///
/// [`OperatorConfig::operator`] is **always** required: it is the operator
/// *type's* name, and every backend needs one — the compiled-in registry
/// looks it up directly, a shared library and a WebAssembly module each
/// export several operators and need to be told which, and a hub package is
/// no different. What the three optional locator fields add is *where* that
/// name is resolved:
///
/// - **none of the three** — the hosting runtime binary's own
///   `astrs::register_operator!` registry. The flagship path of §9.3, the
///   default, and the only one with no loading step at all.
/// - **`dylib`** — a path to a platform shared library (`.so`/`.dylib`/
///   `.dll`) the runtime opens with `dlopen`/`LoadLibrary`
///   (`astrs-runtime`'s `dylib-operators` feature).
/// - **`wasm`** — a path to a WebAssembly module the runtime runs on its
///   pure-Rust `wasmi` interpreter (`astrs-runtime`'s `wasm-operators`
///   feature).
/// - **`hub`** — a package fetched from the AstRS index by name, in either of
///   [`HubSource`]'s two spellings.
///
/// At most one locator may be present; [`crate::Manifest::validate`] rejects
/// a combination, exactly as it does for [`crate::Node`]'s own source kinds.
/// Keeping `operator` required is what makes every pre-existing manifest, and
/// every existing consumer of this type, keep working unchanged: the new
/// kinds are additive.
///
/// `config` stays opaque JSON in every case: per-operator schemas are out of
/// this crate's scope.
///
/// ```yaml
/// operators:
///   - id: crop
///     operator: CropOperator          # compiled-in registry
///   - id: detect
///     operator: YoloDetector
///     dylib: ./libyolo.so             # ...from a shared library
///   - id: filter
///     operator: BandPass
///     wasm: ./filter.wasm             # ...from a WebAssembly module
///   - id: track
///     operator: ByteTracker
///     hub: byte-tracker@v0.2.0        # ...from the package index
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfig {
    /// This operator instance's id, unique within the hosting node.
    pub id: String,
    /// The operator type's name — the name it was registered under via
    /// `astrs::register_operator!`, or the name the shared library, wasm
    /// module or hub package exports it as. Always required; see the struct
    /// docs for why the new locator fields did not turn this optional.
    pub operator: String,

    // ---- locator (at most one; see struct docs) ----------------------
    /// A path to a platform shared library exporting this operator, loaded
    /// by `astrs-runtime`'s `dylib-operators` feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dylib: Option<String>,
    /// A path to a WebAssembly module implementing this operator, run by
    /// `astrs-runtime`'s `wasm-operators` feature on its pure-Rust `wasmi`
    /// interpreter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm: Option<String>,
    /// An AstRS package-index source for this operator — see [`HubSource`]
    /// for the two spellings (`name`/`name@rev`, or a `{ name, rev? }` map).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<HubSource>,

    // ---- I/O and configuration ---------------------------------------
    /// This operator's inputs, keyed by input name (same short/long form
    /// as [`crate::Node::inputs`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, Input>,
    /// This operator's declared output names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    /// Free-form operator configuration, opaque to the manifest crate.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl OperatorConfig {
    /// A minimal operator entry resolved against the compiled-in registry —
    /// the common case in tests, examples, and every manifest written before
    /// the `dylib`/`wasm`/`hub` locators existed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::OperatorConfig;
    ///
    /// let crop = OperatorConfig::with_operator("crop", "CropOperator");
    /// assert_eq!(crop.operator, "CropOperator");
    /// assert!(crop.locator_kinds().is_empty()); // the compiled-in registry
    /// ```
    #[must_use]
    pub fn with_operator(id: impl Into<String>, operator: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            operator: operator.into(),
            dylib: None,
            wasm: None,
            hub: None,
            inputs: BTreeMap::new(),
            outputs: Vec::new(),
            config: BTreeMap::new(),
        }
    }

    /// Every locator this entry declares, in the field order the struct
    /// documents (`dylib`, `wasm`, `hub`).
    ///
    /// Empty means the compiled-in registry — the default and by far the
    /// common case. More than one is a conflict
    /// [`crate::Manifest::validate`] rejects, and this is the list it
    /// reports.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::OperatorConfig;
    ///
    /// let registry = OperatorConfig::with_operator("crop", "CropOperator");
    /// assert!(registry.locator_kinds().is_empty());
    ///
    /// let mut confused = OperatorConfig::with_operator("x", "X");
    /// confused.dylib = Some("./x.so".to_string());
    /// confused.wasm = Some("./x.wasm".to_string());
    /// assert_eq!(confused.locator_kinds(), vec!["dylib", "wasm"]);
    /// ```
    #[must_use]
    pub fn locator_kinds(&self) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        if self.dylib.is_some() {
            kinds.push("dylib");
        }
        if self.wasm.is_some() {
            kinds.push("wasm");
        }
        if self.hub.is_some() {
            kinds.push("hub");
        }
        kinds
    }

    /// Whether this operator is resolved against the hosting binary's own
    /// compiled-in `register_operator!` registry — i.e. no locator is set.
    #[must_use]
    pub fn is_registry_sourced(&self) -> bool {
        self.locator_kinds().is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_minimal_operator() {
        let yaml = "id: crop\noperator: CropOperator\n";
        let op: OperatorConfig = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(op.id, "crop");
        assert_eq!(op.operator, "CropOperator");
        assert!(op.dylib.is_none());
        assert!(op.wasm.is_none());
        assert!(op.hub.is_none());
        assert!(op.is_registry_sourced());
        assert!(op.inputs.is_empty());
        assert!(op.outputs.is_empty());
        assert!(op.config.is_empty());
    }

    #[test]
    fn parses_operator_with_config() {
        let yaml = "\
id: nms
operator: NmsOperator
inputs:
  boxes: crop/boxes
outputs: [detections]
config:
  threshold: 0.5
  labels: [person, car]
";
        let op: OperatorConfig = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(op.inputs.len(), 1);
        assert_eq!(op.outputs, vec!["detections".to_string()]);
        assert_eq!(
            op.config.get("threshold").and_then(|v| v.as_f64()),
            Some(0.5)
        );
    }

    #[test]
    fn parses_a_dylib_locator() {
        let yaml = "id: yolo\noperator: YoloDetector\ndylib: ./libyolo.so\n";
        let op: OperatorConfig = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(op.operator, "YoloDetector");
        assert_eq!(op.dylib.as_deref(), Some("./libyolo.so"));
        assert_eq!(op.locator_kinds(), vec!["dylib"]);
        assert!(!op.is_registry_sourced());
    }

    #[test]
    fn parses_a_wasm_locator() {
        let yaml = "id: f\noperator: BandPass\nwasm: ./filter.wasm\n";
        let op: OperatorConfig = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(op.wasm.as_deref(), Some("./filter.wasm"));
        assert_eq!(op.locator_kinds(), vec!["wasm"]);
    }

    #[test]
    fn parses_a_hub_locator_in_both_spellings() {
        let short: OperatorConfig =
            astrs_yaml::from_str("id: t\noperator: ByteTracker\nhub: byte-tracker@v0.2.0\n")
                .unwrap();
        assert_eq!(short.hub, Some(HubSource::pinned("byte-tracker", "v0.2.0")));
        assert_eq!(short.locator_kinds(), vec!["hub"]);

        let long: OperatorConfig = astrs_yaml::from_str(
            "id: t\noperator: ByteTracker\nhub:\n  name: byte-tracker\n  rev: v0.2.0\n",
        )
        .unwrap();
        assert_eq!(long.hub, short.hub);
    }

    #[test]
    fn locator_kinds_reports_every_declared_locator_in_field_order() {
        let mut op = OperatorConfig::with_operator("x", "X");
        assert!(op.locator_kinds().is_empty());
        op.hub = Some(HubSource::latest("pkg"));
        op.dylib = Some("./x.so".to_string());
        op.wasm = Some("./x.wasm".to_string());
        assert_eq!(op.locator_kinds(), vec!["dylib", "wasm", "hub"]);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(
            astrs_yaml::from_str::<OperatorConfig>("id: x\noperator: Y\nbogus: true\n").is_err()
        );
    }

    #[test]
    fn requires_id_and_operator() {
        assert!(astrs_yaml::from_str::<OperatorConfig>("operator: Y\n").is_err());
        assert!(astrs_yaml::from_str::<OperatorConfig>("id: x\n").is_err());
        // Even with a locator: the type's name is still required, since a
        // shared library or wasm module exports more than one operator.
        assert!(astrs_yaml::from_str::<OperatorConfig>("id: x\nwasm: ./x.wasm\n").is_err());
    }

    #[test]
    fn round_trips_every_locator_kind() {
        let entries = vec![
            OperatorConfig::with_operator("crop", "CropOperator"),
            OperatorConfig {
                dylib: Some("./libyolo.so".to_string()),
                ..OperatorConfig::with_operator("yolo", "YoloDetector")
            },
            OperatorConfig {
                wasm: Some("./filter.wasm".to_string()),
                ..OperatorConfig::with_operator("filter", "BandPass")
            },
            OperatorConfig {
                hub: Some(HubSource::pinned("byte-tracker", "v0.2.0")),
                ..OperatorConfig::with_operator("track", "ByteTracker")
            },
        ];

        for entry in entries {
            let yaml = astrs_yaml::to_string(&entry).unwrap();
            let back: OperatorConfig = astrs_yaml::from_str(&yaml).unwrap();
            assert_eq!(back, entry, "yaml was: {yaml}");
            assert!(back.locator_kinds().len() <= 1, "yaml was: {yaml}");
        }
    }

    #[test]
    fn an_absent_source_field_is_omitted_from_serialized_output() {
        let yaml = astrs_yaml::to_string(&OperatorConfig::with_operator("crop", "Crop")).unwrap();
        assert!(!yaml.contains("dylib"), "yaml was: {yaml}");
        assert!(!yaml.contains("wasm"), "yaml was: {yaml}");
        assert!(!yaml.contains("hub"), "yaml was: {yaml}");
    }
}
