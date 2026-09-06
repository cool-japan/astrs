//! A single dataflow node (blueprint §8.3, complete).

mod enums;
mod hub;
mod io;
mod operator;
mod restart;
mod ros2;
mod rt;

pub use enums::{LogLevel, Pattern};
pub use hub::{HUB_REV_SEPARATOR, HubSource};
pub use io::{Input, PriorityLane, QueuePolicy, default_queue_size};
pub use operator::OperatorConfig;
pub use restart::RestartPolicy;
pub use ros2::{BridgeDirection, Durability, Qos, Ros2Config, Ros2Role, Ros2Topic, RosCompat};
pub use rt::{RT_PRIORITY_MAX, RT_PRIORITY_MIN, RtConfig, RtPolicy};

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Deploy, EnvValue, Urn};

/// The literal `path:` value that marks a node as a **dynamic** node — one
/// with no process for the daemon to spawn, attached externally at runtime
/// (blueprint §8.3: "`path: dynamic` (external attach)").
pub const DYNAMIC_PATH_SENTINEL: &str = "dynamic";

fn default_max_rotated_files() -> u32 {
    5
}

fn is_default_max_rotated_files(v: &u32) -> bool {
    *v == default_max_rotated_files()
}

/// One node in a [`crate::Manifest`]'s `nodes:` list (blueprint §8.3).
///
/// # Source (exactly one)
///
/// A node's source is not a single tagged field but a set of plain
/// optional fields, validated for mutual exclusivity by
/// [`crate::Manifest::validate`] rather than by the `Deserialize` shape — a tagged
/// `enum` would force every source variant into one YAML key (`type:
/// git`), which is not how §8.1's examples are written, and combining
/// `#[serde(flatten)]` with `#[serde(deny_unknown_fields)]` on the same
/// struct is rejected by serde outright. The recognized source *kinds* are:
///
/// - **`path`** alone — a prebuilt executable path, or the literal
///   [`DYNAMIC_PATH_SENTINEL`] (`"dynamic"`) for an externally-attached
///   node.
/// - **`git`** (+ optional `path` as the build artifact's location *within*
///   the checked-out repo, + at most one of `branch`/`tag`/`rev`) — §8.1's
///   `detector` node uses exactly this shape.
/// - **`hub`** — a package fetched from the AstRS index by name, in either
///   of [`HubSource`]'s two spellings (`name`/`name@rev`, or a
///   `{ name, rev? }` map). What `git:` is to a repository, this is to the
///   package index.
/// - **`module`** — a path to a reusable sub-graph manifest (§8.5),
///   flattened by [`crate::expand`].
/// - **`operators`** — one or more runtime-hosted operators (§9.3).
/// - **`ros2`** — a declarative ROS 2 bridge (§10.5).
/// - **`record`** — the recorder sugar (§14): a list of `node/output`
///   strings to capture, expanding to an ordinary `astrs-record-node`.
///
/// See [`crate::Manifest::validate`] for the exact exclusivity rules enforced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Node {
    // ---- identity ---------------------------------------------------
    /// This node's id: unique within the manifest, charset
    /// `[a-zA-Z0-9_.-]+` (checked by [`crate::Manifest::validate`]). Referenced by
    /// other nodes' inputs as `<id>/<output>`.
    pub id: String,
    /// An optional human-readable display name, distinct from `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// An optional free-form description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    // ---- source (exactly one kind; see struct docs) ------------------
    /// A prebuilt executable path, the in-repo build artifact path when
    /// `git` is also set, or the literal `"dynamic"` sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A git repository URL to clone/build from (shells out to the system
    /// `git` binary — no `git2`/`libgit2`, per §2.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// Check out this branch (at most one of `branch`/`tag`/`rev`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Check out this tag (at most one of `branch`/`tag`/`rev`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Check out this commit-ish (at most one of `branch`/`tag`/`rev`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// An AstRS package-index source — see [`HubSource`] for the two
    /// spellings. Its own optional revision lives inside the value
    /// (`hub: pkg@v1`, or `hub: { name: pkg, rev: v1 }`), never in this
    /// node's `rev:` field, which belongs to `git:` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<HubSource>,
    /// A path to a reusable sub-graph manifest (§8.5), resolved relative
    /// to the *including* manifest's own directory by [`crate::expand`] —
    /// never the process's current working directory. The referenced
    /// manifest must carry a [`crate::ModuleHeader`]; this node's own
    /// `inputs`/`outputs`/`input_types`/`output_types` wire up (a subset
    /// of) that header's declared boundary ports, exactly like any other
    /// node's I/O.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// One or more runtime-hosted operators (§9.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operators: Option<Vec<OperatorConfig>>,
    /// A declarative ROS 2 bridge configuration (§10.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ros2: Option<Ros2Config>,
    /// The `record:` sugar: a list of `node/output` strings to capture
    /// into a `.arec` recording (§14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Vec<String>>,

    // ---- build / run --------------------------------------------------
    /// The build command line, executed via the system shell with a
    /// scrubbed environment (§16). Not tokenized by this crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Extra arguments appended to the spawned process's argv.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Node-scoped environment variables, merged over the manifest-wide
    /// `env:` map with this node's values taking precedence on key
    /// conflicts (see [`Node::effective_env`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    /// The working directory the spawned process starts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,

    // ---- I/O ------------------------------------------------------------
    /// This node's inputs, keyed by input name. Each value is either a
    /// bare `node/output` string or a long form with queueing overrides —
    /// see [`Input`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, Input>,
    /// This node's declared output names, referenced by other nodes as
    /// `<this id>/<name>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    /// Type URNs for inputs, keyed by input name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_types: BTreeMap<String, Urn>,
    /// Type URNs for outputs, keyed by output name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub output_types: BTreeMap<String, Urn>,
    /// The service/action wiring pattern this node participates in, if
    /// any (§9.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<Pattern>,

    // ---- logging --------------------------------------------------------
    /// Publish this node's captured stdout as an output with this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_stdout_as: Option<String>,
    /// The minimum log level retained for this node's log records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_log_level: Option<LogLevel>,
    /// The maximum size in bytes of one log file before rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_log_size: Option<u64>,
    /// The maximum number of rotated log files retained (default 5).
    #[serde(
        default = "default_max_rotated_files",
        skip_serializing_if = "is_default_max_rotated_files"
    )]
    pub max_rotated_files: u32,

    // ---- fault tolerance (§12) -------------------------------------------
    /// The supervised-restart policy. Absent means [`RestartPolicy::Never`]
    /// — see [`Node::effective_restart_policy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,
    /// The restart budget within `restart_window`. Requires
    /// `restart_policy` to be set to something other than `never`
    /// (checked by [`crate::Manifest::validate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_restarts: Option<u32>,
    /// The base delay before the first restart attempt; doubles per
    /// attempt up to `max_restart_delay` (§12's `restart_delay`×2^n).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_delay: Option<crate::DurationSecs>,
    /// The cap on the exponential-backoff restart delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_restart_delay: Option<crate::DurationSecs>,
    /// The sliding window `max_restarts` is counted over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_window: Option<crate::DurationSecs>,
    /// How long to wait for a post-registration liveness ping before
    /// treating the node as hung (§12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_timeout: Option<crate::DurationSecs>,
    /// How long to wait after `SIGTERM` before escalating to `SIGKILL`
    /// during graceful shutdown (§12's finish-straggler watchdog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_grace_secs: Option<crate::DurationSecs>,

    // ---- placement / performance -----------------------------------------
    /// Where and how this node is deployed, overriding the manifest-wide
    /// default placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<Deploy>,
    /// Pin the spawned process to these CPU core indices (§11.3): applied
    /// race-free via `rustix::thread::sched_setaffinity` before the child's
    /// own program runs, on Linux — see `astrs_daemon::spawn::affinity` for
    /// the mechanism. macOS (and every other non-Linux platform) has no
    /// per-process affinity API at all, so a request there is left unpinned
    /// on purpose, logged once and counted, rather than approximated with a
    /// QoS-class hint that would not actually pin anything. Must be
    /// non-empty when present (checked by [`crate::Manifest::validate`]) — an empty
    /// list would mean "pin to nothing," which is not a meaningful pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_affinity: Option<Vec<usize>>,
    /// The OS scheduling class this node's process is spawned under, and its
    /// priority within that class (§11.3) — see [`RtConfig`]. Absent means
    /// the platform default (`SCHED_OTHER`), which is also what
    /// `rt: { policy: normal }` spells explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rt: Option<RtConfig>,
    /// Override the default SHM pool size (bytes) for this node's outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shm_pool_size: Option<u64>,
}

impl Node {
    /// Build a minimal node with only an id and a `path` source — the
    /// common case in tests and examples.
    #[must_use]
    pub fn with_path(id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            description: None,
            path: Some(path.into()),
            git: None,
            branch: None,
            tag: None,
            rev: None,
            hub: None,
            module: None,
            operators: None,
            ros2: None,
            record: None,
            build: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            working_dir: None,
            inputs: BTreeMap::new(),
            outputs: Vec::new(),
            input_types: BTreeMap::new(),
            output_types: BTreeMap::new(),
            pattern: None,
            send_stdout_as: None,
            min_log_level: None,
            max_log_size: None,
            max_rotated_files: default_max_rotated_files(),
            restart_policy: None,
            max_restarts: None,
            restart_delay: None,
            max_restart_delay: None,
            restart_window: None,
            health_check_timeout: None,
            finish_grace_secs: None,
            deploy: None,
            cpu_affinity: None,
            rt: None,
            shm_pool_size: None,
        }
    }

    /// The restart policy in effect for this node.
    ///
    /// A node with no explicit `restart_policy` is never restarted — see
    /// [`RestartPolicy::default`].
    #[must_use]
    pub fn effective_restart_policy(&self) -> RestartPolicy {
        self.restart_policy.unwrap_or_default()
    }

    /// Whether `path` is set to the dynamic-attach sentinel
    /// (`"dynamic"`), *without* regard to whether that combination is
    /// otherwise valid (see [`crate::Manifest::validate`] for the actual exclusivity
    /// check — combining `path: dynamic` with `git` is rejected there).
    #[must_use]
    pub fn is_dynamic_path(&self) -> bool {
        self.path.as_deref() == Some(DYNAMIC_PATH_SENTINEL)
    }

    /// This node's environment, merged with a manifest-wide `env:` map.
    ///
    /// Keys in `graph_env` are applied first; keys in `self.env` override
    /// them (§8.3: node-scoped `env:` takes precedence over the graph-wide
    /// default). Neither map is expanded here — see [`crate::env::expand_map`]
    /// for `$VAR` substitution, which the caller applies afterward with
    /// whatever lookup source (daemon-scrubbed environment, not the raw
    /// process environment) is appropriate for the call site.
    #[must_use]
    pub fn effective_env(
        &self,
        graph_env: &BTreeMap<String, EnvValue>,
    ) -> BTreeMap<String, EnvValue> {
        let mut merged = graph_env.clone();
        merged.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        merged
    }

    /// This node's inputs whose `source` is an `astrs/...` virtual source
    /// (§8.4), parsed into a structured [`crate::VirtualSource`] rather
    /// than left as a string — the shape downstream crates
    /// (`astrs-scheduler` registering timer ticks, `astrs-log` filtering
    /// `astrs/logs/*` fan-out) actually consume.
    ///
    /// Ordinary `node/output` inputs are silently excluded, not just
    /// unparsed — this is a filter, not a partition; pair it with
    /// [`Node::inputs`] directly when the ordinary-reference inputs are
    /// also needed. A source that starts with `astrs/` but does not match
    /// any recognized virtual-source grammar still yields an entry, with
    /// `Err(_)` as its value — the same [`crate::VirtualSourceError`]
    /// [`crate::Manifest::validate`] would report, for callers that want
    /// it without re-deriving it from a `ValidationErrors` list.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::{Manifest, VirtualSource};
    ///
    /// let yaml = "\
    /// nodes:
    ///   - id: planner
    ///     path: ./planner
    ///     inputs:
    ///       detections: detector/detections
    ///       tick: astrs/timer/hz/50
    /// ";
    /// let manifest = Manifest::from_yaml_str(yaml)?;
    /// let planner = &manifest.nodes[0];
    ///
    /// let ticks: Vec<_> = planner.virtual_inputs().collect();
    /// assert_eq!(ticks.len(), 1); // `detections` is an ordinary reference, excluded
    /// let (name, source) = &ticks[0];
    /// assert_eq!(*name, "tick");
    /// assert_eq!(source, &Ok(VirtualSource::TimerHz(50)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn virtual_inputs(
        &self,
    ) -> impl Iterator<
        Item = (
            &str,
            Result<crate::VirtualSource, crate::VirtualSourceError>,
        ),
    > {
        self.inputs.iter().filter_map(|(name, input)| {
            crate::virtual_source::recognize(&input.source).map(|result| (name.as_str(), result))
        })
    }

    /// This node's `record:` sugar (§14), lowered to the ordinary
    /// [`Input`]s it is shorthand for.
    ///
    /// Entry `i` of `record:` becomes an input named `_record_{i}`, its
    /// `source` set to that entry's string verbatim (a `node/output`
    /// reference or an `astrs/...` virtual source — both already resolve
    /// via [`crate::Manifest::validate`]'s own `record:` check, so this
    /// performs no re-validation of its own). This is a read-only view:
    /// it does not write the synthesized inputs back into
    /// [`Node::inputs`], and a node with no `record:` sugar (or an empty
    /// list) yields nothing.
    ///
    /// The daemon's dataflow planner (`astrs-daemon`) is the intended
    /// caller: a `record:`-sourced node has no hand-written `inputs:` of
    /// its own, so without this, the daemon would spawn its recorder
    /// process with zero wired inputs. Building the equivalent wire
    /// `InputSpec`s from this method's output, rather than from
    /// [`Node::inputs`] alone, is what actually feeds it the recorded
    /// ports.
    ///
    /// # Why `_record_<i>`, not `_record/<i>`
    ///
    /// `astrs-graph`'s own independent graph-model synthesis of this same
    /// sugar spells its (unrelated, `Manifest`-external) port names
    /// `_record/<i>` — but a manifest input *name* is a wire `DataId`,
    /// whose grammar (`[A-Za-z0-9_.-]+`) forbids `/`. This method's names
    /// satisfy that grammar by construction, at the cost of the two
    /// crates spelling the same reserved-prefix idea with a different
    /// separator. Both agree on enumerating `record:` in
    /// list order and on the `_record` prefix; that is the only agreement
    /// they need, since neither ever reads the other's synthesized names.
    ///
    /// # Collisions
    ///
    /// A node whose `inputs:` already declares `_record_0` by hand (legal,
    /// if unusual) keeps that explicit declaration: this method skips any
    /// index whose synthesized name is already a key of [`Node::inputs`],
    /// so a caller that always applies both [`Node::inputs`] and this
    /// method's output never sees the same input name twice.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::{Input, Node};
    ///
    /// let mut node = Node::with_path("recorder", "dummy");
    /// node.path = None;
    /// node.record = Some(vec![
    ///     "camera/frames".to_string(),
    ///     "detector/detections".to_string(),
    /// ]);
    ///
    /// let synthesized = node.record_sugar_inputs();
    /// assert_eq!(synthesized.len(), 2);
    /// assert_eq!(synthesized[0], ("_record_0".to_string(), Input::from_source("camera/frames")));
    /// assert_eq!(synthesized[1].0, "_record_1");
    /// ```
    #[must_use]
    pub fn record_sugar_inputs(&self) -> Vec<(String, Input)> {
        self.record
            .iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, source)| {
                let name = format!("_record_{index}");
                if self.inputs.contains_key(&name) {
                    None
                } else {
                    Some((name, Input::from_source(source.clone())))
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn record_sugar_inputs_is_empty_for_a_node_without_record() {
        let node = Node::with_path("x", "./x");
        assert_eq!(node.record_sugar_inputs(), Vec::new());
    }

    #[test]
    fn record_sugar_inputs_is_empty_for_an_empty_record_list() {
        let mut node = Node::with_path("recorder", "dummy");
        node.path = None;
        node.record = Some(Vec::new());
        assert_eq!(node.record_sugar_inputs(), Vec::new());
    }

    #[test]
    fn record_sugar_inputs_numbers_entries_in_list_order() {
        let mut node = Node::with_path("recorder", "dummy");
        node.path = None;
        node.record = Some(vec![
            "camera/frames".to_string(),
            "detector/detections".to_string(),
            "astrs/timer/hz/50".to_string(),
        ]);

        let synthesized = node.record_sugar_inputs();
        assert_eq!(
            synthesized,
            vec![
                ("_record_0".to_string(), Input::from_source("camera/frames")),
                (
                    "_record_1".to_string(),
                    Input::from_source("detector/detections")
                ),
                (
                    "_record_2".to_string(),
                    Input::from_source("astrs/timer/hz/50")
                ),
            ]
        );
    }

    #[test]
    fn record_sugar_inputs_skips_an_index_with_an_explicit_collision() {
        let mut node = Node::with_path("recorder", "dummy");
        node.path = None;
        node.record = Some(vec![
            "camera/frames".to_string(),
            "detector/detections".to_string(),
        ]);
        // A hand-written input already claims `_record_0`; the explicit
        // declaration must win, exactly as `astrs-graph`'s own
        // `_record/<i>` synthesis treats the analogous collision.
        node.inputs
            .insert("_record_0".to_string(), Input::from_source("other/thing"));

        let synthesized = node.record_sugar_inputs();
        assert_eq!(synthesized.len(), 1);
        assert_eq!(synthesized[0].0, "_record_1");
    }

    #[test]
    fn record_sugar_input_names_are_legal_data_ids() {
        // The whole point of `_record_<i>` over `astrs-graph`'s
        // `_record/<i>`: every synthesized name must satisfy the
        // `[A-Za-z0-9_.-]+` grammar a manifest input name (and the wire
        // `DataId` it becomes) is held to.
        let mut node = Node::with_path("recorder", "dummy");
        node.path = None;
        node.record = Some(vec!["a/b".to_string(); 3]);
        for (name, _) in node.record_sugar_inputs() {
            assert!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
                "{name} is not a legal data id"
            );
        }
    }

    #[test]
    fn with_path_round_trips() {
        let node = Node::with_path("camera", "./target/release/camera-node");
        let yaml = astrs_yaml::to_string(&node).unwrap();
        let back: Node = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(node, back);
    }

    #[test]
    fn virtual_inputs_excludes_ordinary_references() {
        let mut node = Node::with_path("planner", "./planner");
        node.inputs.insert(
            "detections".to_string(),
            Input::from_source("detector/detections"),
        );
        node.inputs
            .insert("tick".to_string(), Input::from_source("astrs/timer/hz/50"));

        let virtual_inputs: BTreeMap<_, _> = node.virtual_inputs().collect();
        assert_eq!(virtual_inputs.len(), 1);
        assert_eq!(
            virtual_inputs.get("tick"),
            Some(&Ok(crate::VirtualSource::TimerHz(50)))
        );
        assert!(!virtual_inputs.contains_key("detections"));
    }

    #[test]
    fn virtual_inputs_surfaces_malformed_virtual_sources_as_errors() {
        let mut node = Node::with_path("consumer", "./consumer");
        node.inputs
            .insert("bad".to_string(), Input::from_source("astrs/timer/hz/0"));

        let virtual_inputs: Vec<_> = node.virtual_inputs().collect();
        assert_eq!(virtual_inputs.len(), 1);
        assert!(virtual_inputs[0].1.is_err());
    }

    #[test]
    fn virtual_inputs_empty_when_no_inputs_are_virtual() {
        let mut node = Node::with_path("consumer", "./consumer");
        node.inputs
            .insert("data".to_string(), Input::from_source("producer/out"));
        assert_eq!(node.virtual_inputs().count(), 0);
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = "id: x\npath: ./x\nbogus_field: 1\n";
        assert!(astrs_yaml::from_str::<Node>(yaml).is_err());
    }

    #[test]
    fn effective_restart_policy_defaults_to_never() {
        let node = Node::with_path("x", "./x");
        assert_eq!(node.effective_restart_policy(), RestartPolicy::Never);
    }

    #[test]
    fn is_dynamic_path_detects_sentinel() {
        let mut node = Node::with_path("x", DYNAMIC_PATH_SENTINEL);
        assert!(node.is_dynamic_path());
        node.path = Some("./real/path".to_string());
        assert!(!node.is_dynamic_path());
    }

    #[test]
    fn effective_env_merges_with_node_precedence() {
        let mut node = Node::with_path("x", "./x");
        node.env
            .insert("A".to_string(), EnvValue::String("node".to_string()));
        node.env.insert("B".to_string(), EnvValue::Int(2));

        let mut graph_env = BTreeMap::new();
        graph_env.insert("A".to_string(), EnvValue::String("graph".to_string()));
        graph_env.insert("C".to_string(), EnvValue::Bool(true));

        let merged = node.effective_env(&graph_env);
        assert_eq!(merged.get("A"), Some(&EnvValue::String("node".to_string())));
        assert_eq!(merged.get("B"), Some(&EnvValue::Int(2)));
        assert_eq!(merged.get("C"), Some(&EnvValue::Bool(true)));
    }

    #[test]
    fn parses_git_plus_path_source() {
        let yaml = "\
id: detector
git: https://github.com/cool-japan/astrs-yolo
tag: v0.3.1
build: cargo build --release
path: target/release/yolo-node
outputs: [detections]
";
        let node: Node = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(
            node.git.as_deref(),
            Some("https://github.com/cool-japan/astrs-yolo")
        );
        assert_eq!(node.tag.as_deref(), Some("v0.3.1"));
        assert_eq!(node.path.as_deref(), Some("target/release/yolo-node"));
    }

    #[test]
    fn parses_record_sugar() {
        let yaml = "id: recorder\nrecord: [camera/frames, detector/detections]\n";
        let node: Node = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(
            node.record,
            Some(vec![
                "camera/frames".to_string(),
                "detector/detections".to_string()
            ])
        );
    }

    #[test]
    fn max_rotated_files_defaults_to_five_and_is_omitted_when_default() {
        let node = Node::with_path("x", "./x");
        assert_eq!(node.max_rotated_files, 5);
        let yaml = astrs_yaml::to_string(&node).unwrap();
        assert!(!yaml.contains("max_rotated_files"), "yaml was: {yaml}");
    }

    #[test]
    fn cpu_affinity_round_trips() {
        let mut node = Node::with_path("x", "./x");
        node.cpu_affinity = Some(vec![0, 1, 2]);
        let yaml = astrs_yaml::to_string(&node).unwrap();
        let back: Node = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.cpu_affinity, Some(vec![0, 1, 2]));
    }

    #[test]
    fn empty_cpu_affinity_is_distinguishable_from_absent() {
        let absent = Node::with_path("x", "./x");
        assert_eq!(absent.cpu_affinity, None);
        let mut present_empty = Node::with_path("x", "./x");
        present_empty.cpu_affinity = Some(Vec::new());
        assert_eq!(present_empty.cpu_affinity, Some(Vec::new()));
        assert_ne!(absent.cpu_affinity, present_empty.cpu_affinity);
    }

    #[test]
    fn no_output_framing_field_exists() {
        // Blueprint §2.2 / §8.3: `output_framing` must never exist as a
        // manifest field. A manifest that sets it must be rejected by
        // `deny_unknown_fields`, not silently accepted.
        let yaml = "id: x\npath: ./x\noutput_framing: arrow-ipc\n";
        assert!(astrs_yaml::from_str::<Node>(yaml).is_err());
    }
}
