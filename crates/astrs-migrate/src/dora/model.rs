//! The dora dataflow descriptor shape, as much of it as `from-dora` reads.
//!
//! Modeled directly against a real dora `dora-schema.json` (the "AI-Dora
//! lineage" checkout blueprint §2 audited), not reconstructed from memory:
//! field names, optionality and the couple of value-format quirks below
//! are all ground truth, not guesses.
//!
//! Every struct here is deliberately permissive — `Option<T>` on nearly
//! everything, no `deny_unknown_fields`, and a `#[serde(flatten)] extra`
//! catch-all — because this module's only job is to *read* whatever a real
//! dora file contains, including fields this importer does not understand
//! yet. [`super::convert`] is what turns "I didn't recognize this" into a
//! labeled note rather than a parse failure; a struct that rejected unknown
//! fields would turn every future dora field into a hard migration failure
//! instead of a `MigrationNote` (blueprint §8.6: "never silently drop").
//!
//! Two value-format quirks worth flagging up front, both confirmed against
//! the reference schema rather than assumed:
//!
//! - [`DoraNode::args`] is a single shell-style **string** in dora
//!   (`args: "--flag value"`), not a list — unlike
//!   [`astrs_manifest::Node::args`], which is already a `Vec<String>`.
//!   [`super::convert`] tokenizes it with `shlex`.
//! - [`DoraNode::restart_policy`]'s `on-failure` value spells the word
//!   with a **hyphen**; AstRS's [`astrs_manifest::RestartPolicy`] spells it
//!   `on_failure` (underscore, blueprint §8.1's own literal example). Every
//!   other value/field in this module that overlaps AstRS's own manifest
//!   spelling (`pattern`, `queue_policy`, `min_log_level`, restart timing
//!   field *names*) matches exactly — AstRS deliberately kept dora's
//!   spelling for those (blueprint §2.1) — so `restart_policy`'s hyphen is
//!   the one deliberate exception, not a pattern.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The root of a dora dataflow descriptor (`dataflow.yml`).
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DoraManifest {
    /// The dataflow's nodes.
    #[serde(default)]
    pub nodes: Vec<DoraNode>,
    /// The graph-wide default placement, dora's `deploy:` block (old
    /// spelling: `_unstable_deploy:`, dropped in dora 1.0 —
    /// dora-rs/dora#3220, both accepted here via `serde(alias)`). Maps to
    /// [`astrs_manifest::Manifest::deploy`], overridden per node by
    /// [`DoraNode::deploy`] exactly as AstRS's own root/node `deploy:`
    /// relate.
    #[serde(alias = "_unstable_deploy")]
    pub deploy: Option<DoraDeploy>,
    /// Debug/inspection options, dora's `debug:` block (old spelling:
    /// `_unstable_debug:`, dropped in dora 1.0 — dora-rs/dora#3220, both
    /// accepted here via `serde(alias)`). Root-only — dora has no per-node
    /// `debug:`. Maps to [`astrs_manifest::Manifest::debug`]: dora's
    /// `enable_debug_inspection` flag is the entire AstRS `debug` bool,
    /// since AstRS has no finer-grained inspection toggle to map dora's
    /// other `Debug` fields onto.
    #[serde(alias = "_unstable_debug")]
    pub debug: Option<DoraDebug>,
    /// Graph-wide environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, DoraEnvValue>,
    /// Whether the dataflow exits once every node with real work has
    /// finished (dora-rs/dora#2920; matches
    /// [`astrs_manifest::Manifest::exit_when_nodes_finish`] exactly).
    pub exit_when_nodes_finish: Option<bool>,
    /// How often (seconds) the daemon health-checks each node.
    pub health_check_interval: Option<f64>,
    /// Whether a type mismatch with no applicable rule is a hard error.
    pub strict_types: Option<bool>,
    /// Implicit type-coercion rules.
    #[serde(default)]
    pub type_rules: Vec<DoraTypeRule>,
    /// Anything else at the root this importer does not model explicitly
    /// (surfaced as root-level [`super::convert::MigrationNote`]s).
    #[serde(flatten)]
    pub extra: BTreeMap<String, astrs_yaml::Value>,
}

/// A `{from, to}` type-coercion rule — byte-identical shape to
/// [`astrs_manifest::TypeRule`].
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DoraTypeRule {
    /// The source type URN.
    pub from: String,
    /// The target type URN.
    pub to: String,
}

/// A dora `env:` scalar value: string, bool, int or float, exactly like
/// [`astrs_manifest::EnvValue`].
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum DoraEnvValue {
    /// A string value.
    String(String),
    /// A boolean value.
    Bool(bool),
    /// A signed integer value.
    Int(i64),
    /// A floating point value.
    Float(f64),
}

impl From<DoraEnvValue> for astrs_manifest::EnvValue {
    fn from(value: DoraEnvValue) -> Self {
        match value {
            DoraEnvValue::String(s) => Self::String(s),
            DoraEnvValue::Bool(b) => Self::Bool(b),
            DoraEnvValue::Int(i) => Self::Int(i),
            DoraEnvValue::Float(f) => Self::Float(f),
        }
    }
}

/// A byte size as dora's `ByteSize` accepts it: a bare integer (raw bytes)
/// or a string with a `B`/`KB`/`MB`/`GB` suffix (binary multipliers —
/// `KB` is 1024, not 1000; see [`super::byte_size::parse_byte_size`], which
/// implements the exact grammar dora's own `ByteSize::from_str` does).
/// Used for [`DoraNode::max_log_size`] and
/// [`DoraNode::shared_memory_pool_size`].
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum DoraByteSize {
    /// A bare integer, already in bytes.
    Int(u64),
    /// A string with an optional unit suffix, e.g. `"128MB"`.
    Text(String),
}

/// One dora node.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DoraNode {
    /// The node's id.
    pub id: String,
    /// An optional display name.
    pub name: Option<String>,
    /// An optional free-form description.
    pub description: Option<String>,
    /// A prebuilt executable path.
    pub path: Option<String>,
    /// A git repository URL.
    pub git: Option<String>,
    /// Check out this branch.
    pub branch: Option<String>,
    /// Check out this tag.
    pub tag: Option<String>,
    /// Check out this commit-ish.
    pub rev: Option<String>,
    /// The build command line.
    pub build: Option<String>,
    /// Extra argv, as a single shell-style string (dora quirk — see this
    /// module's top-level docs).
    pub args: Option<String>,
    /// Node-scoped environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, DoraEnvValue>,
    /// This node's inputs.
    #[serde(default)]
    pub inputs: BTreeMap<String, DoraInput>,
    /// This node's declared output names.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Type URNs for inputs.
    #[serde(default)]
    pub input_types: BTreeMap<String, String>,
    /// Type URNs for outputs.
    #[serde(default)]
    pub output_types: BTreeMap<String, String>,
    /// The service/action pattern shorthand, e.g. `"service-server"` —
    /// spelled identically to [`astrs_manifest::Pattern`]'s own kebab-case
    /// wire form.
    pub pattern: Option<String>,
    /// Publish captured stdout as an output with this name.
    pub send_stdout_as: Option<String>,
    /// Publish structured log entries as an output with this name — no
    /// AstRS equivalent (flagged as a [`super::convert::NoteSeverity::NeedsAttention`] note).
    pub send_logs_as: Option<String>,
    /// The minimum retained log level, spelled identically to
    /// [`astrs_manifest::LogLevel`]'s wire form.
    pub min_log_level: Option<String>,
    /// The maximum size of one log file before rotation.
    pub max_log_size: Option<DoraByteSize>,
    /// The maximum number of rotated log files retained.
    pub max_rotated_files: Option<u32>,
    /// The restart policy: `"never"`, `"on-failure"` (hyphen — see this
    /// module's top-level docs) or `"always"`.
    pub restart_policy: Option<String>,
    /// The restart budget within `restart_window`.
    pub max_restarts: Option<u32>,
    /// The base restart backoff delay, in seconds.
    pub restart_delay: Option<f64>,
    /// The restart backoff cap, in seconds.
    pub max_restart_delay: Option<f64>,
    /// The sliding window `max_restarts` is counted over, in seconds.
    pub restart_window: Option<f64>,
    /// The post-registration liveness timeout, in seconds.
    pub health_check_timeout: Option<f64>,
    /// The `SIGTERM`-to-`SIGKILL` grace period, in seconds.
    pub finish_grace_secs: Option<f64>,
    /// Pin the spawned process to these CPU core indices.
    pub cpu_affinity: Option<Vec<usize>>,
    /// The shared-memory pool size for this node's outputs.
    pub shared_memory_pool_size: Option<DoraByteSize>,
    /// A path to a reusable sub-graph module descriptor.
    pub module: Option<String>,
    /// A Dora Hub package reference (`hub: dora-yolo@^0.5`) — no AstRS
    /// equivalent (no package registry in 0.1).
    pub hub: Option<String>,
    /// Compile-time parameters passed to a `module:` (`${_param.name}`
    /// substitution) — no AstRS equivalent.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// A SHA-256 checksum the `path` download must match — no AstRS
    /// equivalent (build artifact integrity is out of scope for 0.1).
    pub path_sha256: Option<String>,
    /// Per-output wire framing overrides — dead configuration in dora too
    /// (every path already stamps one wire format); dropped outright.
    #[serde(default)]
    pub output_framing: BTreeMap<String, astrs_yaml::Value>,
    /// Required metadata keys per output — no manifest-level AstRS
    /// equivalent (service/action correlation is via `pattern:` instead).
    #[serde(default)]
    pub output_metadata: BTreeMap<String, Vec<String>>,
    /// A single runtime-hosted operator (the `operator:` convenience form).
    pub operator: Option<DoraOperator>,
    /// Multiple runtime-hosted operators sharing one process.
    #[serde(default)]
    pub operators: Vec<DoraOperator>,
    /// A declarative ROS 2 bridge block — carried through as a comment
    /// only (§8.6's mapping scope does not include `ros2:`; AstRS's own
    /// `ros2:` shape, §10.5, is not field-compatible with dora's).
    pub ros2: Option<astrs_yaml::Value>,
    /// Placement, dora's `deploy:` block (old spelling: `_unstable_deploy:`,
    /// dropped in dora 1.0 — dora-rs/dora#3220, both accepted here via
    /// `serde(alias)`). Maps to [`astrs_manifest::Deploy`].
    #[serde(alias = "_unstable_deploy")]
    pub deploy: Option<DoraDeploy>,
    /// Anything else this importer does not model explicitly.
    #[serde(flatten)]
    pub extra: BTreeMap<String, astrs_yaml::Value>,
}

/// One node input: dora's short (`"node/output"`) or long
/// (`{source, queue_size, queue_policy, input_timeout}`) form.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum DoraInput {
    /// The bare `"node/output"` (or `dora/...` virtual source) form.
    Short(String),
    /// The long form, with per-input overrides.
    Long {
        /// The source string.
        source: String,
        /// The queue depth override.
        #[serde(default)]
        queue_size: Option<u32>,
        /// The overflow policy, spelled identically to
        /// [`astrs_manifest::QueuePolicy`]'s wire form.
        #[serde(default)]
        queue_policy: Option<String>,
        /// The per-input delivery timeout, in seconds — named
        /// `input_timeout` in dora, `timeout` in
        /// [`astrs_manifest::Input`].
        #[serde(default)]
        input_timeout: Option<f64>,
    },
}

/// A runtime-hosted operator entry (either the singular `operator:` or one
/// entry of `operators:`).
///
/// Only wiring (`id`/`inputs`/`outputs`) is modeled explicitly: dora
/// operators are loaded from a shared library or a Python script
/// (`shared-library:`/`python:`, depending on the dora version), while
/// AstRS operators are Rust trait objects registered at compile time
/// (blueprint §9.3) — there is no automatic mapping from "a path to a
/// dynamically loaded implementation" to "a name registered in this
/// runtime binary's source code". [`super::convert`] therefore maps the
/// wiring mechanically and turns every other key this operator carries
/// (`extra`) into a note naming exactly what needs re-implementing by
/// hand.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DoraOperator {
    /// The operator's id (optional for the singular `operator:` form).
    pub id: Option<String>,
    /// This operator's inputs.
    #[serde(default)]
    pub inputs: BTreeMap<String, DoraInput>,
    /// This operator's declared output names.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Everything else (`build`, `python`, `shared-library`, ...) — never
    /// interpreted, only quoted back in a migration note.
    #[serde(flatten)]
    pub extra: BTreeMap<String, astrs_yaml::Value>,
}

/// Dora's `deploy:` placement block (old spelling: `_unstable_deploy:`),
/// carried at both [`DoraManifest::deploy`] (root, graph-wide default) and
/// [`DoraNode::deploy`] (per-node override) — dora uses the identical shape
/// at both levels, exactly like [`astrs_manifest::Deploy`] does.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DoraDeploy {
    /// The target machine id.
    pub machine: Option<String>,
    /// A working directory override.
    pub working_dir: Option<String>,
    /// Label-selector placement metadata — identical shape to
    /// [`astrs_manifest::Deploy::labels`].
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// How built binaries reach remote daemons (`local`/`scp`/`http`) — no
    /// AstRS equivalent (every daemon builds from source in 0.1).
    pub distribute: Option<String>,
    /// Anything else this importer does not model explicitly. Never
    /// silently dropped: [`super::convert`] turns each entry into a
    /// [`super::convert::MigrationNote`] scoped like the rest of this
    /// block (root or node), matching every other `extra` catch-all in
    /// this module — `DoraDeploy` is not exempt just because most of its
    /// fields already map cleanly.
    #[serde(flatten)]
    pub extra: BTreeMap<String, astrs_yaml::Value>,
}

/// Dora's `debug:` block (old spelling: `_unstable_debug:`) — root-only,
/// unlike [`DoraDeploy`] (dora has no per-node debug options).
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DoraDebug {
    /// When `true`, daemons mirror every node output to the coordinator
    /// WebSocket so `dora topic echo`/`hz`/`info` can inspect runtime
    /// messages. Maps to [`astrs_manifest::Manifest::debug`].
    #[serde(default)]
    pub enable_debug_inspection: bool,
    /// Anything else this importer does not model explicitly — e.g. the
    /// `publish_all_messages_to_zenoh` alias dora itself removed for 1.0.
    /// Never silently dropped: [`super::convert`] turns each entry into a
    /// root-scoped [`super::convert::MigrationNote`], matching every other
    /// `extra` catch-all in this module.
    #[serde(flatten)]
    pub extra: BTreeMap<String, astrs_yaml::Value>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_a_minimal_node() {
        let node: DoraNode = astrs_yaml::from_str("id: camera\npath: ./camera\n").unwrap();
        assert_eq!(node.id, "camera");
        assert_eq!(node.path.as_deref(), Some("./camera"));
        assert!(node.extra.is_empty());
    }

    #[test]
    fn captures_unknown_fields_in_extra() {
        let node: DoraNode =
            astrs_yaml::from_str("id: x\npath: ./x\nsome_future_field: 42\n").unwrap();
        assert_eq!(
            node.extra.get("some_future_field"),
            Some(&astrs_yaml::Value::Number(42.into()))
        );
    }

    #[test]
    fn short_and_long_input_forms_both_parse() {
        let short: DoraInput = astrs_yaml::from_str("camera/frames").unwrap();
        assert!(matches!(short, DoraInput::Short(s) if s == "camera/frames"));

        let long: DoraInput =
            astrs_yaml::from_str("source: camera/frames\nqueue_size: 2\ninput_timeout: 1.5\n")
                .unwrap();
        match long {
            DoraInput::Long {
                source,
                queue_size,
                input_timeout,
                ..
            } => {
                assert_eq!(source, "camera/frames");
                assert_eq!(queue_size, Some(2));
                assert_eq!(input_timeout, Some(1.5));
            }
            DoraInput::Short(_) => panic!("expected long form"),
        }
    }

    #[test]
    fn byte_size_accepts_both_int_and_string() {
        let int_form: DoraByteSize = astrs_yaml::from_str("1024").unwrap();
        assert!(matches!(int_form, DoraByteSize::Int(1024)));
        let str_form: DoraByteSize = astrs_yaml::from_str("\"128MB\"").unwrap();
        assert!(matches!(str_form, DoraByteSize::Text(s) if s == "128MB"));
    }

    #[test]
    fn node_deploy_is_recognized_under_the_modern_spelling() {
        let node: DoraNode =
            astrs_yaml::from_str("id: x\npath: ./x\ndeploy:\n  machine: robot-1\n").unwrap();
        assert_eq!(
            node.deploy.as_ref().and_then(|d| d.machine.clone()),
            Some("robot-1".to_string())
        );
        // The critical check: `deploy` must be routed to the named field,
        // not *also* captured by the `#[serde(flatten)] extra` catch-all —
        // double-routing would make `convert` emit an "unrecognized field
        // `deploy`" TODO comment right next to the very field it just
        // mapped cleanly, which is exactly what this whole change exists
        // to stop happening.
        assert!(node.extra.is_empty(), "extra: {:?}", node.extra);
    }

    #[test]
    fn node_deploy_is_still_recognized_under_the_legacy_underscore_spelling() {
        let node: DoraNode =
            astrs_yaml::from_str("id: x\npath: ./x\n_unstable_deploy:\n  machine: robot-1\n")
                .unwrap();
        assert_eq!(
            node.deploy.as_ref().and_then(|d| d.machine.clone()),
            Some("robot-1".to_string())
        );
        assert!(node.extra.is_empty(), "extra: {:?}", node.extra);
    }

    #[test]
    fn both_spellings_present_at_once_is_a_parse_error_not_a_silent_pick() {
        // Not a shape any real dora file would ever produce (a file is
        // written against one dora era or the other, never both at once
        // for the same block) -- pinned anyway so a future serde upgrade
        // changing `alias` + duplicate-key handling would be caught here
        // rather than surfacing as a silent "last one wins" surprise.
        let result: Result<DoraNode, _> = astrs_yaml::from_str(
            "id: x\npath: ./x\ndeploy:\n  machine: modern\n_unstable_deploy:\n  machine: legacy\n",
        );
        assert!(
            result.is_err(),
            "expected a duplicate-field error, got: {result:?}"
        );
    }

    #[test]
    fn root_deploy_and_debug_are_recognized_under_the_modern_spelling() {
        let manifest: DoraManifest = astrs_yaml::from_str(
            "nodes: []\ndeploy:\n  machine: robot-1\ndebug:\n  enable_debug_inspection: true\n",
        )
        .unwrap();
        assert_eq!(
            manifest.deploy.as_ref().and_then(|d| d.machine.clone()),
            Some("robot-1".to_string())
        );
        assert!(
            manifest
                .debug
                .as_ref()
                .is_some_and(|d| d.enable_debug_inspection)
        );
        assert!(manifest.extra.is_empty(), "extra: {:?}", manifest.extra);
    }

    #[test]
    fn root_deploy_and_debug_are_recognized_under_the_legacy_underscore_spelling() {
        let manifest: DoraManifest = astrs_yaml::from_str(
            "nodes: []\n_unstable_deploy:\n  machine: robot-1\n_unstable_debug:\n  \
             enable_debug_inspection: true\n",
        )
        .unwrap();
        assert_eq!(
            manifest.deploy.as_ref().and_then(|d| d.machine.clone()),
            Some("robot-1".to_string())
        );
        assert!(
            manifest
                .debug
                .as_ref()
                .is_some_and(|d| d.enable_debug_inspection)
        );
        assert!(manifest.extra.is_empty(), "extra: {:?}", manifest.extra);
    }

    #[test]
    fn debug_block_captures_unknown_keys_in_extra_rather_than_dropping_them() {
        // `publish_all_messages_to_zenoh` is the alias dora itself removed
        // for 1.0 (see this module's `DoraDebug` docs) — a stand-in here
        // for "any key this importer does not yet know about", proving the
        // same "never silently drop" invariant every other `extra`
        // catch-all in this module already upholds.
        let debug: DoraDebug = astrs_yaml::from_str(
            "enable_debug_inspection: true\npublish_all_messages_to_zenoh: true\n",
        )
        .unwrap();
        assert!(debug.enable_debug_inspection);
        assert!(debug.extra.contains_key("publish_all_messages_to_zenoh"));
    }

    #[test]
    fn root_parses_nodes_and_captures_extras() {
        let manifest: DoraManifest = astrs_yaml::from_str(
            "nodes:\n  - id: a\n    path: ./a\nexit_when_nodes_finish: true\nsome_root_field: 1\n",
        )
        .unwrap();
        assert_eq!(manifest.nodes.len(), 1);
        assert_eq!(manifest.exit_when_nodes_finish, Some(true));
        assert!(manifest.extra.contains_key("some_root_field"));
    }

    #[test]
    fn env_value_converts_to_astrs_env_value() {
        let dora = DoraEnvValue::Int(42);
        let astrs: astrs_manifest::EnvValue = dora.into();
        assert_eq!(astrs, astrs_manifest::EnvValue::Int(42));
    }

    #[test]
    fn operator_captures_extra_source_fields() {
        let op: DoraOperator = astrs_yaml::from_str(
            "id: op\nshared-library: ../../target/debug/foo\ninputs:\n  tick: dora/timer/millis/5\noutputs: [data]\n",
        )
        .unwrap();
        assert_eq!(op.id.as_deref(), Some("op"));
        assert_eq!(op.outputs, vec!["data".to_string()]);
        assert!(op.extra.contains_key("shared-library"));
    }

    #[test]
    fn deploy_block_captures_unknown_keys_in_extra_rather_than_dropping_them() {
        // A stand-in for "any deploy: sub-key this importer does not yet
        // know about" -- `DoraDeploy` models `machine`/`working_dir`/
        // `labels`/`distribute` explicitly and nothing else, so unlike
        // `DoraDebug` there is no real dora field to reach for here.
        let deploy: DoraDeploy =
            astrs_yaml::from_str("machine: robot-1\nsome_future_deploy_field: 42\n").unwrap();
        assert_eq!(deploy.machine.as_deref(), Some("robot-1"));
        assert_eq!(
            deploy.extra.get("some_future_deploy_field"),
            Some(&astrs_yaml::Value::Number(42.into()))
        );
    }
}
