//! Shared types for `dynamic-add-remove` — `astrs node add`/`node remove`
//! against a running dataflow (blueprint §8.3, §17).
//!
//! ```text
//!   [anchor] ──beats──►  (nothing, until a reader runs `astrs node add`)
//! ```
//!
//! `anchor` is the only node the committed `dataflow.yml` declares, and it
//! runs for a while on purpose: this example's whole point needs a cluster
//! that is genuinely still up when a reader (or `astrs up`'s own
//! documentation) runs `astrs node add examples/dynamic-add-remove/
//! fragments/worker.yml`. That command spawns `dynamic-worker` — declared in
//! `fragments/worker.yml`, a single-node manifest fragment, **not** a member
//! of `dataflow.yml`'s own `nodes:` list — wired to read `anchor/beats`.
//! `astrs node remove <dataflow> worker` then removes it again while
//! `anchor` keeps running underneath, unaffected.
//!
//! # Why this crate does not drive a real coordinator
//!
//! Every other example in this estate proves its central claim with a
//! `-p`-scoped unit test that needs nothing but this crate's own code.
//! `astrs node add`/`node remove` are coordinator verbs — proving one
//! actually adds a live process to a live cluster needs a real coordinator
//! and daemon, which is squarely `tests/conformance`'s job (`m2_multi_daemon
//! .rs`'s dynamic-topology coverage), not this package's. What *is*
//! checkable here, cheaply and for real, is the one step that is entirely
//! this example's own responsibility: that `fragments/worker.yml` is a
//! well-formed node fragment that expands the way `astrs node add` actually
//! expands one — see this crate's own
//! `tests::the_fragment_expands_the_way_expand_node_fragment_would`.

use serde::{Deserialize, Serialize};

/// The anchor's output port, and the dynamically-added worker's input.
pub const BEATS_PORT: &str = "beats";
/// The anchor's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable overriding how many beats the anchor publishes.
pub const ENV_ANCHOR_BEATS: &str = "DYNAMIC_ANCHOR_BEATS";
/// Environment variable overriding how many beats the dynamically-added
/// worker waits for before it finishes on its own.
pub const ENV_WORKER_BUDGET: &str = "DYNAMIC_WORKER_BUDGET";
/// Environment variable naming the JSON file the worker writes its
/// [`WorkerProof`] to.
pub const ENV_PROOF_PATH: &str = "DYNAMIC_WORKER_PROOF";

/// How many beats the anchor publishes by default.
pub const DEFAULT_ANCHOR_BEATS: u64 = 2000;
/// How many beats the dynamically-added worker waits for by default.
pub const DEFAULT_WORKER_BUDGET: u64 = 20;
/// How many bytes a beat payload carries: one big-endian counter, nothing
/// else.
pub const BEAT_PAYLOAD_BYTES: usize = 8;

/// How many beats this run's anchor should publish.
#[must_use]
pub fn anchor_beat_budget() -> u64 {
    env_u64(ENV_ANCHOR_BEATS, DEFAULT_ANCHOR_BEATS)
}

/// How many beats this run's worker should wait for.
#[must_use]
pub fn worker_beat_budget() -> u64 {
    env_u64(ENV_WORKER_BUDGET, DEFAULT_WORKER_BUDGET)
}

/// Reads a positive `u64` from environment variable `name`, falling back to
/// `default` when it is unset, empty, zero, or not a number.
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Where the worker writes its [`WorkerProof`] when the manifest names no
/// path.
#[must_use]
pub fn default_proof_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-dynamic-add-remove-proof.json")
}

/// The JSON file this run's worker writes its proof to.
#[must_use]
pub fn proof_path() -> std::path::PathBuf {
    std::env::var(ENV_PROOF_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_proof_path, std::path::PathBuf::from)
}

/// Encodes a beat counter as its wire payload: eight big-endian bytes.
#[must_use]
pub fn beat_payload(count: u64) -> [u8; BEAT_PAYLOAD_BYTES] {
    count.to_be_bytes()
}

/// Decodes a beat counter out of a payload.
///
/// [`None`] if `payload` is not exactly [`BEAT_PAYLOAD_BYTES`] long.
#[must_use]
pub fn beat_value_of(payload: &[u8]) -> Option<u64> {
    let bytes: [u8; BEAT_PAYLOAD_BYTES] = payload.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// What the dynamically-added worker proves about its own run: that it
/// really was spawned, really did attach to `anchor/beats`, and really did
/// observe live counter values — not a stub that exited immediately.
///
/// Written as JSON to [`proof_path`] once the worker's budget is met or it
/// is asked to stop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerProof {
    /// Whether this incarnation is a restart of a previous one (§12) —
    /// always `false` for a node's first, ordinary spawn via `node add`.
    pub is_restart: bool,
    /// This process's own id, for a reader correlating the proof against
    /// `ps`/`astrs top` output.
    pub pid: u32,
    /// How many `beats` events were observed.
    pub beats_seen: u64,
    /// The first beat value observed, if any arrived.
    pub first_beat: Option<u64>,
    /// The last beat value observed, if any arrived.
    pub last_beat: Option<u64>,
}

impl WorkerProof {
    /// Whether the observed beats are internally consistent: at least one
    /// arrived, and the last is not smaller than the first — `anchor`'s
    /// counter only ever increases, so a worker that attached partway
    /// through should still see a non-decreasing run.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        match (self.first_beat, self.last_beat) {
            (Some(first), Some(last)) => self.beats_seen > 0 && first <= last,
            _ => false,
        }
    }

    /// Renders the proof as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field
    /// types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A beat counter round-trips through encode/decode, and a payload of
    /// the wrong length is reported as absent rather than guessed at.
    #[test]
    fn beat_payloads_round_trip_and_reject_the_wrong_length() {
        for count in [0_u64, 1, 42, u64::MAX] {
            let payload = beat_payload(count);
            assert_eq!(payload.len(), BEAT_PAYLOAD_BYTES);
            assert_eq!(beat_value_of(&payload), Some(count));
        }
        assert_eq!(beat_value_of(&[1, 2, 3]), None);
    }

    /// A proof built from a real, monotonic run of beats is consistent.
    #[test]
    fn a_monotonic_run_is_consistent() {
        let proof = WorkerProof {
            is_restart: false,
            pid: 1234,
            beats_seen: 20,
            first_beat: Some(5),
            last_beat: Some(24),
        };
        assert!(proof.is_consistent(), "{proof:?}");
    }

    /// A worker that never saw a single beat is not consistent — attaching
    /// to the graph is not the same as observing anything through it.
    #[test]
    fn a_worker_that_saw_nothing_is_not_consistent() {
        let proof = WorkerProof {
            is_restart: false,
            pid: 1234,
            beats_seen: 0,
            first_beat: None,
            last_beat: None,
        };
        assert!(!proof.is_consistent());
    }

    /// A last beat smaller than the first is not consistent — `anchor`'s
    /// counter never goes backwards, so this can only mean the values were
    /// misattributed.
    #[test]
    fn a_decreasing_run_is_not_consistent() {
        let proof = WorkerProof {
            is_restart: false,
            pid: 1234,
            beats_seen: 3,
            first_beat: Some(10),
            last_beat: Some(2),
        };
        assert!(!proof.is_consistent());
    }

    /// The proof round-trips as JSON.
    #[test]
    fn a_proof_round_trips_as_json() {
        let proof = WorkerProof {
            is_restart: false,
            pid: 4242,
            beats_seen: 5,
            first_beat: Some(0),
            last_beat: Some(4),
        };
        let json = proof.to_json().unwrap();
        let parsed: WorkerProof = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, proof);
    }

    /// The environment helpers fall back for anything unusable.
    #[test]
    fn the_env_helper_falls_back_for_anything_unusable() {
        assert_eq!(env_u64("ASTRS_DOES_NOT_EXIST_XYZ", 7), 7);
    }

    /// Both artefact paths default under the temporary directory.
    #[test]
    fn artefact_paths_default_to_the_temp_dir() {
        assert!(default_proof_path().starts_with(std::env::temp_dir()));
    }

    /// The committed base manifest parses, validates, names this dataflow,
    /// and declares exactly the one always-on node.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("dynamic-add-remove"));
        assert_eq!(manifest.nodes.len(), 1);
        assert_eq!(manifest.nodes[0].id, "anchor");
    }

    /// The committed fragment expands the way `astrs node add` actually
    /// expands one — `astrs_coordinator::expand_node_fragment`'s own logic,
    /// reproduced here (see this crate's docs for why this package does not
    /// simply depend on `astrs-coordinator` to call that function
    /// directly): parse the bare `Node` YAML (no `nodes:` wrapper), wrap it
    /// in a single-node `Manifest`, and build a graph from it.
    ///
    /// A single-node fragment's own inputs may legitimately reference a
    /// sibling (`anchor/beats`) that this synthetic one-node manifest does
    /// not itself declare — that is exactly what `node add` is *for* — so
    /// this test checks graph construction, not `Manifest::validate`, which
    /// would (correctly, for an ordinary manifest) reject that as an
    /// unresolved source.
    #[test]
    fn the_fragment_expands_the_way_expand_node_fragment_would() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fragments")
            .join("worker.yml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let node: astrs_manifest::Node = astrs_yaml::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(node.id, "worker");
        assert_eq!(node.path.as_deref(), Some("./target/debug/dynamic-worker"));
        assert_eq!(
            node.build.as_deref(),
            Some("cargo build -p dynamic-add-remove")
        );
        assert!(!std::path::Path::new(node.path.as_deref().unwrap_or_default()).is_absolute());
        let source = node.inputs.get(BEATS_PORT).expect("a beats input");
        assert_eq!(source.source, "anchor/beats");

        let manifest = astrs_manifest::Manifest {
            nodes: vec![node],
            ..astrs_manifest::Manifest::default()
        };
        let (graph, _diagnostics) = astrs_graph::DataflowGraph::from_manifest(&manifest)
            .unwrap_or_else(|error| panic!("the fragment did not expand into a graph: {error}"));
        assert!(graph.node(&astrs_graph::NodeId::new("worker")).is_some());
    }
}
