//! `cargo xtask layer-lint`: enforce blueprint §4.1's layer stack over the
//! real dependency graph.
//!
//! # The rule this lints enforces
//!
//! Every crate in [`PRODUCTION_LAYERS`] is assigned a [`Layer`] (`1`
//! Substrate .. `4` Interfaces & tools). For every `[dependencies]` and
//! `[build-dependencies]` edge `dependent -> dependency` where *both*
//! endpoints are in that table:
//!
//! - **legal** if `layer(dependency) <= layer(dependent)` -- downward, or
//!   sideways within the same layer (Layer 1 is full of this: `astrs-shm`,
//!   `astrs-transport` and `astrs-discovery` all depend on `astrs-wire`,
//!   which itself depends on `astrs-time`);
//! - **illegal ("upward")** if `layer(dependency) > layer(dependent)` --
//!   the crate stack's whole point (§4.1: "Dependency rule: strictly
//!   downward");
//! - **illegal ("sideways-illegal")** if a same-layer edge closes a cycle.
//!   Once the upward check holds for every edge, a cycle can only ever be
//!   *entirely* inside one layer (going around it and returning to the
//!   start requires every step to be layer-non-increasing *and* the total
//!   change to be zero, which forces every step to be exactly zero) -- so
//!   this is precisely what "sideways" can mean that "downward-or-same"
//!   does not already allow. Cargo itself refuses to build a workspace
//!   with a `[dependencies]`/`[build-dependencies]` cycle, so on a
//!   compiling workspace this arm is unreachable; it stays as an explicit,
//!   named check rather than silent, untested coverage (a future
//!   restructuring that *did* introduce one would otherwise surface as an
//!   opaque Cargo "cyclic package dependency" error instead of a clear
//!   layer-lint line naming the crates involved).
//!
//! `[dev-dependencies]` are exempt from all of the above -- crossing
//! layers, and even forming a cycle, is fine. This is not a convenience
//! reading of the task brief: the workspace already relies on it.
//! `astrs-node-api`'s own `[dev-dependencies]` carries `astrs-daemon` with
//! the comment *"Dev-only, and acyclic: `astrs-daemon` does not depend on
//! this crate"* -- explicit acknowledgement, in the crate's own manifest,
//! that a dev-only edge is a deliberately different animal from a regular
//! one. Cargo permits a dev-dependency cycle between two packages (neither
//! participates in the other's *build*, only its *test/example* graph),
//! and this workspace uses that: `astrs-daemon` dev-depends on
//! `astrs-coordinator`, `astrs-node-api` dev-depends on `astrs-daemon`,
//! `tests/conformance` dev-depends on nearly everything. None of that is a
//! layering violation.
//!
//! # Why the table matches §5.2, not §4.1's diagram
//!
//! §4.1's mermaid diagram groups `astrs-node-api`/`astrs-operator-api`
//! into an illustrative "L4 API" box, which disagrees with both §5.2 (its
//! own crate catalog places them under "Orchestration & execution (Layer
//! 3)", beside `astrs-daemon`/`astrs-coordinator`/`astrs-runtime`) and the
//! real dependency graph: `astrs-runtime` (Layer 3 in both sources)
//! regular-depends on `astrs-node-api` directly. If node-api were Layer 4,
//! that edge would be upward on a crate the workspace requires to compile
//! -- so §4.1's box placement cannot be the operative one. §5.2's per-crate
//! table is what the real crate graph was built against (every edge in
//! `the_real_workspace_has_no_layer_violations` below checks out against
//! it with zero exceptions), so it is what [`PRODUCTION_LAYERS`]
//! transcribes; the §4.1 pointer in each block below is to the layer
//! *definitions* (what Layer 1..4 mean), not to the diagram's box
//! placement.
//!
//! Workspace members outside the §5.2 catalog -- the sixteen examples
//! (§5.4) and `tests/conformance` (§20.3) -- have no assigned layer and
//! are skipped as *dependents*: they sit conceptually above the
//! architecture (consumers of it, never depended on by it), so nothing
//! they depend on can be "upward" in any meaningful sense. [`LintReport`]
//! still names them, in `skipped`, so a reader can see they were
//! considered and deliberately excluded rather than silently missed.

use std::collections::HashMap;
use std::path::Path;

use crate::error::XtaskError;
use crate::workspace::{self, Member};

/// A layer in the §4.1 stack. `1` is Substrate (most foundational); `4` is
/// Interfaces & tools (most user-facing). Higher may depend on lower or
/// equal; never the reverse.
pub type Layer = u8;

/// Every crate the blueprint's §5.2 catalog assigns a layer, transcribed
/// section by section. See the module docs for why this follows §5.2's
/// table rather than §4.1's diagram where the two disagree.
pub const PRODUCTION_LAYERS: &[(&str, Layer)] = &[
    // Layer 1 -- Substrate (§5.2 "Substrate (Layer 1) -- 59.5k")
    ("astrs-wire", 1),
    ("astrs-time", 1),
    ("astrs-data", 1),
    ("astrs-shm", 1),
    ("astrs-transport", 1),
    ("astrs-discovery", 1),
    ("astrs-store", 1),
    // The workspace's own YAML 1.2 reader/writer: `serde` and nothing else,
    // so it sits beside the other substrate crates rather than above them.
    ("astrs-yaml", 1),
    // Layer 2 -- Domain libraries (§5.2 "Domain libraries (Layer 2) -- 51.5k")
    ("astrs-manifest", 2),
    ("astrs-graph", 2),
    ("astrs-scheduler", 2),
    ("astrs-recording", 2),
    ("astrs-telemetry", 2),
    ("astrs-log", 2),
    ("astrs-verify", 2),
    ("astrs-migrate", 2),
    // Coordinator HA: Raft over `astrs-wire` (Layer 1) only -- a domain
    // library like its siblings above, not part of the orchestration layer
    // that consumes it.
    ("astrs-raft", 2),
    // Layer 2 -- ROS 2 stack, its own pillar, still Layer 2
    // (§5.2 "ROS 2 stack (Layer 2, own pillar) -- 57.5k")
    ("astrs-cdr", 2),
    ("astrs-rtps", 2),
    ("astrs-idl", 2),
    ("astrs-ros2", 2),
    ("astrs-rosbag", 2),
    ("astrs-tf", 2),
    // The URDF model `astrs-tf`'s own docs point at: it consumes
    // `TransformBuffer` (Layer 2) sideways, never the other way round.
    ("astrs-urdf", 2),
    // Layer 3 -- Orchestration & execution
    // (§5.2 "Orchestration & execution (Layer 3) -- 47.5k")
    ("astrs-daemon", 3),
    ("astrs-coordinator", 3),
    ("astrs-runtime", 3),
    ("astrs-node-api", 3),
    ("astrs-operator-api", 3),
    ("astrs-operator-macros", 3),
    // Layer 4 -- Interfaces & tools (§5.2 "Interfaces & tools (Layer 4) -- 31.5k")
    ("astrs-cli", 4),
    ("astrs-tui", 4),
    ("astrs", 4),
    ("astrs-record-node", 4),
    ("astrs-replay-node", 4),
    ("astrs-ros2-bridge-node", 4),
    ("xtask", 4),
    // Layer 4 -- the consumer-facing surfaces added by the 0.2.0/0.3.0
    // pull-forward. Each depends on `astrs-node-api` (Layer 3) or above, so
    // none of them can sit lower: `astrs-capi` exports the node API over a C
    // ABI, the two `astrs-nodes-*` crates are ready-made nodes/operators
    // built on it, and `astrs-sim` drives whole dataflows against a simulated
    // robot.
    ("astrs-capi", 4),
    ("astrs-nodes-signal", 4),
    ("astrs-nodes-vision", 4),
    ("astrs-sim", 4),
];

/// One dependency edge that breaks the layer rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `dependency`'s layer is numerically greater than `dependent`'s --
    /// the crate stack's dependency rule violated directly.
    Upward {
        /// The crate whose `[dependencies]`/`[build-dependencies]` names
        /// `dependency`.
        dependent: String,
        /// `dependent`'s layer.
        dependent_layer: Layer,
        /// The crate depended on.
        dependency: String,
        /// `dependency`'s layer -- greater than `dependent_layer`.
        dependency_layer: Layer,
    },
    /// A same-layer dependency cycle. See the module docs for why this,
    /// specifically, is what "sideways-illegal" means once the upward
    /// check already holds.
    SidewaysCycle {
        /// The crates on the cycle, in dependency order, first crate
        /// repeated at the end (`a -> b -> a`).
        cycle: Vec<String>,
    },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upward {
                dependent,
                dependent_layer,
                dependency,
                dependency_layer,
            } => write!(
                f,
                "upward: {dependent} (layer {dependent_layer}) depends on \
                 {dependency} (layer {dependency_layer}); a crate must not depend on a \
                 higher layer"
            ),
            Self::SidewaysCycle { cycle } => write!(
                f,
                "sideways-illegal: same-layer dependency cycle: {}",
                cycle.join(" -> ")
            ),
        }
    }
}

/// The result of one [`lint`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintReport {
    /// How many members had an assigned layer (and so had their
    /// `[dependencies]`/`[build-dependencies]` edges checked).
    pub checked: usize,
    /// Member names with no assigned layer -- outside the §5.2 catalog
    /// (examples, `tests/conformance`) -- sorted. Not an error; recorded
    /// so a reader can see they were considered.
    pub skipped: Vec<String>,
    /// Every edge that broke the rule.
    pub violations: Vec<Violation>,
}

impl LintReport {
    /// Whether this run found nothing to report.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Check `members` against `layers`. Pure and filesystem-free -- the
/// function [`run`] and this crate's tests both funnel through it, the
/// tests with a small synthetic table so production layering questions
/// never leak into a unit test's expected shape.
#[must_use]
pub fn lint(members: &[Member], layers: &[(&str, Layer)]) -> LintReport {
    let layer_of: HashMap<&str, Layer> = layers.iter().copied().collect();

    let mut checked = 0usize;
    let mut skipped = Vec::new();
    let mut violations = Vec::new();

    for member in members {
        let Some(&dependent_layer) = layer_of.get(member.name.as_str()) else {
            skipped.push(member.name.clone());
            continue;
        };
        checked += 1;

        let edges = member
            .dependencies
            .iter()
            .chain(member.build_dependencies.iter());
        for dependency in edges {
            if dependency == &member.name {
                continue; // a crate testing/deriving against itself, e.g. astrs-operator-macros
            }
            let Some(&dependency_layer) = layer_of.get(dependency.as_str()) else {
                continue; // not a layered crate: external, or outside the §5.2 catalog
            };
            if dependency_layer > dependent_layer {
                violations.push(Violation::Upward {
                    dependent: member.name.clone(),
                    dependent_layer,
                    dependency: dependency.clone(),
                    dependency_layer,
                });
            }
        }
    }

    if let Some(cycle) = find_same_layer_cycle(members, &layer_of) {
        violations.push(Violation::SidewaysCycle { cycle });
    }

    skipped.sort();
    LintReport {
        checked,
        skipped,
        violations,
    }
}

/// Discover `root`'s workspace members and [`lint`] them against `layers`.
///
/// # Errors
///
/// Whatever [`workspace::discover_members`] returns.
pub fn run(root: &Path, layers: &[(&str, Layer)]) -> Result<LintReport, XtaskError> {
    let members = workspace::discover_members(root)?;
    Ok(lint(&members, layers))
}

/// The first same-layer dependency cycle found among `[dependencies]` +
/// `[build-dependencies]` edges, restricted to crates that share a layer
/// (edges out of that restriction cannot be part of a cycle once the
/// upward check holds -- see the module docs).
///
/// A small recursive depth-first search: recursion depth is bounded by the
/// number of layered crates (a few dozen today), so no explicit stack is
/// needed. `done` marks a node whose whole subtree is cycle-free so a
/// later start does not re-walk it.
fn find_same_layer_cycle(
    members: &[Member],
    layer_of: &HashMap<&str, Layer>,
) -> Option<Vec<String>> {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for member in members {
        let Some(&dependent_layer) = layer_of.get(member.name.as_str()) else {
            continue;
        };
        let edges = member
            .dependencies
            .iter()
            .chain(member.build_dependencies.iter());
        for dependency in edges {
            if dependency == &member.name {
                continue;
            }
            if layer_of.get(dependency.as_str()) == Some(&dependent_layer) {
                adjacency
                    .entry(member.name.as_str())
                    .or_default()
                    .push(dependency.as_str());
            }
        }
    }

    let mut done: HashMap<&str, bool> = HashMap::new();
    let starts: Vec<&str> = adjacency.keys().copied().collect();
    for start in starts {
        if done.get(start).copied().unwrap_or(false) {
            continue;
        }
        let mut path: Vec<&str> = Vec::new();
        if let Some(cycle) = visit(&adjacency, &mut done, &mut path, start) {
            return Some(cycle.into_iter().map(str::to_owned).collect());
        }
    }
    None
}

/// The depth-first walk behind [`find_same_layer_cycle`].
fn visit<'a>(
    adjacency: &HashMap<&'a str, Vec<&'a str>>,
    done: &mut HashMap<&'a str, bool>,
    path: &mut Vec<&'a str>,
    node: &'a str,
) -> Option<Vec<&'a str>> {
    if let Some(pos) = path.iter().position(|&n| n == node) {
        let mut cycle: Vec<&str> = path[pos..].to_vec();
        cycle.push(node);
        return Some(cycle);
    }
    if done.get(node).copied().unwrap_or(false) {
        return None;
    }
    path.push(node);
    if let Some(neighbors) = adjacency.get(node) {
        for &next in neighbors {
            if let Some(cycle) = visit(adjacency, done, path, next) {
                return Some(cycle);
            }
        }
    }
    path.pop();
    done.insert(node, true);
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use super::*;

    fn member(name: &str, deps: &[&str], dev_deps: &[&str]) -> Member {
        Member {
            name: name.to_owned(),
            dir: PathBuf::from(name),
            publish: true,
            dependencies: deps.iter().map(|s| (*s).to_owned()).collect(),
            dev_dependencies: dev_deps.iter().map(|s| (*s).to_owned()).collect(),
            build_dependencies: BTreeSet::new(),
        }
    }

    /// A small four-layer table with fake names, entirely disjoint from
    /// [`PRODUCTION_LAYERS`] -- so a bug that accidentally checked
    /// production names in a unit test (or vice versa) would show up as a
    /// glaring failure rather than passing for the wrong reason.
    const FIXTURE_LAYERS: &[(&str, Layer)] = &[
        ("fx-base-a", 1),
        ("fx-base-b", 1),
        ("fx-domain", 2),
        ("fx-orchestrate", 3),
        ("fx-top", 4),
    ];

    #[test]
    fn a_legal_downward_chain_is_clean() {
        let members = vec![
            member("fx-base-a", &[], &[]),
            member("fx-base-b", &["fx-base-a"], &[]), // sideways within layer 1: fine
            member("fx-domain", &["fx-base-a", "fx-base-b"], &[]),
            member("fx-orchestrate", &["fx-domain"], &[]),
            member("fx-top", &["fx-orchestrate", "fx-base-a"], &[]),
        ];
        let report = lint(&members, FIXTURE_LAYERS);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.checked, 5);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn an_upward_edge_is_named_precisely() {
        // fx-base-a (layer 1) regular-depends on fx-top (layer 4).
        let members = vec![
            member("fx-base-a", &["fx-top"], &[]),
            member("fx-top", &[], &[]),
        ];
        let report = lint(&members, FIXTURE_LAYERS);
        assert_eq!(
            report.violations,
            vec![Violation::Upward {
                dependent: "fx-base-a".to_owned(),
                dependent_layer: 1,
                dependency: "fx-top".to_owned(),
                dependency_layer: 4,
            }]
        );
    }

    #[test]
    fn a_dev_dependency_that_would_be_upward_as_regular_is_exempt() {
        // Mirrors astrs-node-api's real `[dev-dependencies]` on
        // astrs-daemon: illegal as a regular edge, fine as dev-only.
        let members = vec![
            member("fx-base-a", &[], &["fx-top"]),
            member("fx-top", &[], &[]),
        ];
        let report = lint(&members, FIXTURE_LAYERS);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn an_unlayered_dependent_is_skipped_not_flagged() {
        // An "example"-shaped crate: not in the table, depends on
        // everything freely.
        let members = vec![
            member("example-app", &["fx-top", "fx-base-a"], &[]),
            member("fx-top", &[], &[]),
            member("fx-base-a", &[], &[]),
        ];
        let report = lint(&members, FIXTURE_LAYERS);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.checked, 2); // fx-top, fx-base-a
        assert_eq!(report.skipped, vec!["example-app".to_owned()]);
    }

    #[test]
    fn a_dependency_on_an_unlayered_crate_is_not_checked() {
        // A layered crate depending on something outside the catalog
        // (should never happen for a real astrs-* crate, but must not
        // crash or false-positive).
        let members = vec![member("fx-base-a", &["not-in-any-table"], &[])];
        let report = lint(&members, FIXTURE_LAYERS);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn self_dependencies_do_not_crash_or_flag() {
        // Mirrors astrs-operator-macros' real self dev-dependency; here as
        // a regular one too, to prove the guard is unconditional.
        let members = vec![member("fx-domain", &["fx-domain"], &["fx-domain"])];
        let report = lint(&members, FIXTURE_LAYERS);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn a_same_layer_cycle_is_sideways_illegal() {
        let members = vec![
            member("fx-base-a", &["fx-base-b"], &[]),
            member("fx-base-b", &["fx-base-a"], &[]),
        ];
        let report = lint(&members, FIXTURE_LAYERS);
        assert_eq!(report.violations.len(), 1);
        match &report.violations[0] {
            Violation::SidewaysCycle { cycle } => {
                assert!(cycle.contains(&"fx-base-a".to_owned()));
                assert!(cycle.contains(&"fx-base-b".to_owned()));
            }
            other => panic!("expected SidewaysCycle, got {other:?}"),
        }
    }

    #[test]
    fn a_dev_only_cycle_is_not_sideways_illegal() {
        // Same shape as the real astrs-daemon <-> astrs-coordinator /
        // astrs-node-api dev-dependency pattern: a mutual edge that is
        // dev-only on at least one side must not trip the cycle check.
        let members = vec![
            member("fx-base-a", &["fx-base-b"], &[]),
            member("fx-base-b", &[], &["fx-base-a"]),
        ];
        let report = lint(&members, FIXTURE_LAYERS);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn display_messages_name_the_layers_and_the_cycle() {
        let upward = Violation::Upward {
            dependent: "a".to_owned(),
            dependent_layer: 1,
            dependency: "b".to_owned(),
            dependency_layer: 2,
        };
        assert!(upward.to_string().contains("layer 1"));
        assert!(upward.to_string().contains("layer 2"));

        let cycle = Violation::SidewaysCycle {
            cycle: vec!["a".to_owned(), "b".to_owned(), "a".to_owned()],
        };
        assert_eq!(
            cycle.to_string(),
            "sideways-illegal: same-layer dependency cycle: a -> b -> a"
        );
    }

    // -- Fixture-Cargo.toml-set tests (temp_dir), exercising `run` end to
    // -- end through the real toml parser, per this task's brief.

    fn scratch_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-xtask-layer-lint-test-{}-{name}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_member(dir: &Path, name: &str, deps_toml: &str) {
        let member_dir = dir.join(name);
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(
            member_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\n\n{deps_toml}"),
        )
        .unwrap();
    }

    #[test]
    fn run_reads_a_clean_fixture_workspace_from_disk() {
        let dir = scratch_dir("clean");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"base\", \"top\"]\n",
        )
        .unwrap();
        write_member(&dir, "base", "[dependencies]\n");
        write_member(
            &dir,
            "top",
            "[dependencies]\nbase = { path = \"../base\" }\n",
        );

        let layers: &[(&str, Layer)] = &[("base", 1), ("top", 4)];
        let report = run(&dir, layers).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.checked, 2);
    }

    #[test]
    fn run_reads_an_upward_violation_from_disk() {
        let dir = scratch_dir("violating");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"base\", \"top\"]\n",
        )
        .unwrap();
        write_member(
            &dir,
            "base",
            "[dependencies]\ntop = { path = \"../top\" }\n",
        );
        write_member(&dir, "top", "[dependencies]\n");

        let layers: &[(&str, Layer)] = &[("base", 1), ("top", 4)];
        let report = run(&dir, layers).unwrap();
        assert_eq!(
            report.violations,
            vec![Violation::Upward {
                dependent: "base".to_owned(),
                dependent_layer: 1,
                dependency: "top".to_owned(),
                dependency_layer: 4,
            }]
        );
    }

    #[test]
    fn run_treats_build_dependencies_like_regular_ones() {
        let dir = scratch_dir("build-dep");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"base\", \"top\"]\n",
        )
        .unwrap();
        write_member(
            &dir,
            "base",
            "[build-dependencies]\ntop = { path = \"../top\" }\n",
        );
        write_member(&dir, "top", "[dependencies]\n");

        let layers: &[(&str, Layer)] = &[("base", 1), ("top", 4)];
        let report = run(&dir, layers).unwrap();
        assert_eq!(report.violations.len(), 1);
        assert!(matches!(report.violations[0], Violation::Upward { .. }));
    }

    // -- The real workspace: the regression guard.

    #[test]
    fn the_real_workspace_has_no_layer_violations() {
        let root = workspace::workspace_root();
        let members = workspace::discover_members(&root).unwrap();
        let report = lint(&members, PRODUCTION_LAYERS);
        assert!(
            report.is_clean(),
            "layer-lint found real violations:\n{}",
            report
                .violations
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(report.checked, PRODUCTION_LAYERS.len());

        // Every workspace member that IS skipped (no PRODUCTION_LAYERS
        // entry) must live outside `crates/` and `bins/` -- the two
        // directories §5.2's catalog actually covers. A member skipped
        // from either one means PRODUCTION_LAYERS is missing an entry for
        // a real production crate; a member skipped from `examples/`,
        // `tests/` or `benches/` (the estates §5.4/§20.3/§20.4 describe,
        // which the catalog never assigns a layer) is exactly the
        // intended, unavoidably open-ended case -- so this checks the
        // *shape* of what is skipped rather than an exhaustive, brittle
        // name list that new examples or bench crates would keep breaking.
        for name in &report.skipped {
            let member = members
                .iter()
                .find(|candidate| &candidate.name == name)
                .unwrap();
            let relative = member.dir.strip_prefix(&root).unwrap_or(&member.dir);
            let top_level = relative
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned());
            assert!(
                !matches!(top_level.as_deref(), Some("crates") | Some("bins")),
                "{name} lives under {} but has no PRODUCTION_LAYERS entry -- add one \
                 (§5.2 assigns every crates/ and bins/ member a layer)",
                relative.display()
            );
        }
    }
}
