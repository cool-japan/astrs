//! The manifest root (blueprint §8.2) and its parse/serialize API.

use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::expand::{ExpandError, ExpandOptions, ModuleLoader};
use crate::{Deploy, EnvValue, ManifestError, ModuleHeader, Node, TypeRule, ValidationErrors};

/// The manifest format major version this crate reads and writes by
/// default (blueprint §8.1: `astrs: "1"  # manifest format major`).
pub const DEFAULT_MANIFEST_FORMAT: &str = "1";

fn default_manifest_format() -> String {
    DEFAULT_MANIFEST_FORMAT.to_string()
}

fn is_default_manifest_format(v: &String) -> bool {
    v == DEFAULT_MANIFEST_FORMAT
}

/// The default `health_check_interval`, in seconds (blueprint §8.2, §24.2).
#[must_use]
pub fn default_health_check_interval() -> f64 {
    5.0
}

fn is_default_health_check_interval(v: &f64) -> bool {
    *v == default_health_check_interval()
}

/// `skip_serializing_if` helper for the `bool` fields that default to
/// `false` (`exit_when_nodes_finish`, `strict_types`).
fn is_false(v: &bool) -> bool {
    !*v
}

/// A parsed, not-yet-validated AstRS dataflow manifest (blueprint §8).
///
/// Construction and structural (single-document) parsing are separate from
/// cross-referential validation:
///
/// ```no_run
/// use astrs_manifest::Manifest;
///
/// let yaml = std::fs::read_to_string("graph.yml")?;
/// let manifest = Manifest::from_yaml_str(&yaml)?;
/// manifest.validate()?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// [`Manifest::from_yaml_str`] / [`Manifest::from_yaml_file`] only reject a
/// document that fails to *parse* (wrong YAML syntax, wrong field types,
/// unknown fields — deny_unknown_fields is enforced here). Structural
/// problems that require looking at the whole graph (duplicate ids,
/// dangling input references, ...) are reported — **all of them, not just
/// the first** — by [`Manifest::validate`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The manifest format major version. Defaults to `"1"`
    /// ([`DEFAULT_MANIFEST_FORMAT`]) when omitted.
    #[serde(
        default = "default_manifest_format",
        skip_serializing_if = "is_default_manifest_format"
    )]
    pub astrs: String,
    /// An optional display name for this dataflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The dataflow's nodes. Required — a manifest with no nodes does
    /// nothing, and [`Manifest::validate`] does not currently flag an
    /// empty list as an error (an empty graph is unusual but not
    /// inherently invalid, e.g. as a scaffold mid-edit).
    pub nodes: Vec<Node>,
    /// How often (seconds) the daemon health-checks each node. Defaults to
    /// [`default_health_check_interval`] (5.0).
    #[serde(
        default = "default_health_check_interval",
        skip_serializing_if = "is_default_health_check_interval"
    )]
    pub health_check_interval: f64,
    /// Whether the dataflow should exit once every node with a finite
    /// input set has finished. Defaults to `false` (long-running by
    /// default).
    #[serde(default, skip_serializing_if = "is_false")]
    pub exit_when_nodes_finish: bool,
    /// Whether edge type mismatches (absent an applicable `type_rules`
    /// entry) are hard errors rather than warnings. Defaults to `false`,
    /// matching §9.2's default `ASTRS_TYPE_CHECK=warn` runtime behavior.
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict_types: bool,
    /// Implicit type-coercion rules for edge type-checking (`astrs-graph`;
    /// this crate only syntax-checks the URNs involved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_rules: Vec<TypeRule>,
    /// Whether this dataflow may be tapped by `astrs topic echo/hz/info`
    /// (blueprint §13: "daemon-side taps (explicitly enabled per dataflow
    /// with `debug: true`)"). Defaults to `false`: a tap forces every
    /// tapped output to stay on the daemon-mediated path rather than
    /// upgrading to zero-copy shared memory (§6.2), a cost `astrs-daemon`
    /// only pays for a dataflow that opted in.
    #[serde(default, skip_serializing_if = "is_false")]
    pub debug: bool,
    /// Graph-wide environment variables, overridden per node by
    /// [`Node::env`] on key conflicts — see [`Node::effective_env`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    /// The graph-wide default placement, overridden per node by
    /// [`Node::deploy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<Deploy>,
    /// This manifest's `module:` header (blueprint §8.5), present iff this
    /// manifest is itself a reusable module — includable by another
    /// manifest's [`Node::module`]-sourced node — rather than (or in
    /// addition to; nothing stops running a module standalone) a
    /// directly-runnable dataflow. See [`crate::expand`] for the
    /// flattening algorithm that consumes it, and
    /// [`crate::MODULE_BOUNDARY_NODE_ID`] for the `_mod` sentinel internal
    /// nodes wire to it with.
    ///
    /// [`Manifest::expand`] always clears this field on its output: a
    /// flattened manifest is never itself still a module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<ModuleHeader>,
}

impl Manifest {
    /// Parse a manifest from a YAML string.
    ///
    /// This only performs structural (single-document) parsing — call
    /// [`Manifest::validate`] afterward to check cross-referential
    /// constraints (duplicate ids, dangling inputs, and so on).
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] for YAML syntax errors, type mismatches,
    /// unknown fields (every struct in this crate denies them), or a
    /// missing required field (for example, a node with no `id`).
    pub fn from_yaml_str(input: &str) -> Result<Self, ManifestError> {
        astrs_yaml::from_str(input).map_err(ManifestError::from_yaml)
    }

    /// Read and parse a manifest from a YAML file.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Io`] if `path` cannot be read, or the same
    /// parse errors as [`Manifest::from_yaml_str`].
    pub fn from_yaml_file(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path_ref = path.as_ref();
        let content = std::fs::read_to_string(path_ref).map_err(|source| ManifestError::Io {
            path: path_ref.display().to_string(),
            source,
        })?;
        Self::from_yaml_str(&content)
    }

    /// Serialize this manifest back to a YAML string.
    ///
    /// Fields at their default value are omitted (e.g. `astrs: "1"` and
    /// `health_check_interval: 5.0` do not round-trip literally unless the
    /// source document set them to a *non-default* value), so `to_yaml`
    /// output can differ textually from the original input while remaining
    /// semantically equivalent — see the crate's round-trip tests, which
    /// compare parsed structures rather than raw text. Duration fields
    /// (`restart_delay` and friends) always re-emit as a plain number of
    /// seconds regardless of whether the input used a humantime-style
    /// string; see [`crate::DurationSecs`].
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Serialize`] if serialization fails (see
    /// that variant's docs for why this is expected to be unreachable in
    /// practice).
    pub fn to_yaml(&self) -> Result<String, ManifestError> {
        astrs_yaml::to_string(self).map_err(|source| ManifestError::Serialize { source })
    }

    /// Run the structural validation pass (blueprint §8: id charset and
    /// uniqueness, source exclusivity, input/record reference resolution,
    /// queue sizes, restart-field consistency, URN syntax, `cpu_affinity`
    /// non-emptiness, and `input_types`/`output_types` keys naming a
    /// declared port).
    ///
    /// Collects **every** violation rather than stopping at the first —
    /// see [`ValidationErrorKind`](crate::ValidationErrorKind) for the full
    /// list of checks and [`ValidationErrors`] for how to inspect them.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationErrors`] (non-empty) if any check fails.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        crate::validate::validate(self)
    }

    /// Flatten every `module:`-sourced node in this manifest, recursively
    /// (blueprint §8.5), using [`ExpandOptions::default`].
    ///
    /// `base_dir` is the directory this manifest's own
    /// [`Node::module`] paths are resolved relative to — never the
    /// process's current working directory. See [`crate::expand`] for
    /// the full algorithm, and [`Manifest::expand_with_options`] to
    /// override the recursion depth limit.
    ///
    /// # Errors
    ///
    /// See [`crate::expand::ExpandError`].
    pub fn expand(
        &self,
        base_dir: &Path,
        loader: &dyn ModuleLoader,
    ) -> Result<Manifest, ExpandError> {
        crate::expand::expand(self, base_dir, loader)
    }

    /// [`Manifest::expand`], with an explicit [`ExpandOptions`] rather
    /// than the default.
    ///
    /// # Errors
    ///
    /// See [`crate::expand::ExpandError`].
    pub fn expand_with_options(
        &self,
        base_dir: &Path,
        loader: &dyn ModuleLoader,
        options: ExpandOptions,
    ) -> Result<Manifest, ExpandError> {
        crate::expand::expand_with_options(self, base_dir, loader, options)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn minimal_yaml() -> &'static str {
        "nodes:\n  - id: only\n    path: ./only\n"
    }

    #[test]
    fn parses_minimal_manifest_with_defaults() {
        let m = Manifest::from_yaml_str(minimal_yaml()).unwrap();
        assert_eq!(m.astrs, "1");
        assert_eq!(m.health_check_interval, 5.0);
        assert!(!m.exit_when_nodes_finish);
        assert!(!m.strict_types);
        assert_eq!(m.nodes.len(), 1);
    }

    #[test]
    fn requires_nodes_field() {
        assert!(Manifest::from_yaml_str("name: x\n").is_err());
    }

    #[test]
    fn rejects_unknown_root_fields() {
        let yaml = format!("{}\nbogus_root_field: 1\n", minimal_yaml());
        assert!(Manifest::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn to_yaml_omits_defaults() {
        let m = Manifest::from_yaml_str(minimal_yaml()).unwrap();
        let yaml = m.to_yaml().unwrap();
        assert!(!yaml.contains("astrs:"), "yaml was: {yaml}");
        assert!(!yaml.contains("health_check_interval"), "yaml was: {yaml}");
        assert!(!yaml.contains("exit_when_nodes_finish"), "yaml was: {yaml}");
    }

    #[test]
    fn from_yaml_file_reads_and_parses() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-manifest-test-{}-{}",
            std::process::id(),
            "from_yaml_file_reads_and_parses"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("graph.yml");
        std::fs::write(&path, minimal_yaml()).unwrap();

        let m = Manifest::from_yaml_file(&path).unwrap();
        assert_eq!(m.nodes.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_yaml_file_reports_io_error_for_missing_file() {
        let path = std::env::temp_dir().join("astrs-manifest-does-not-exist-hopefully.yml");
        let err = Manifest::from_yaml_file(&path).unwrap_err();
        assert!(matches!(err, ManifestError::Io { .. }));
    }

    #[test]
    fn parse_error_carries_location_when_available() {
        let err = Manifest::from_yaml_str("nodes: [").unwrap_err();
        // astrs_yaml attaches a location to syntax errors reliably; a
        // structural/type error might not, so this only asserts the
        // guaranteed-location case.
        assert!(err.line().is_some(), "error was: {err:?}");
    }

    #[test]
    fn round_trip_preserves_parsed_structure() {
        let original = Manifest::from_yaml_str(minimal_yaml()).unwrap();
        let yaml = original.to_yaml().unwrap();
        let reparsed = Manifest::from_yaml_str(&yaml).unwrap();
        assert_eq!(original, reparsed);
    }
}
