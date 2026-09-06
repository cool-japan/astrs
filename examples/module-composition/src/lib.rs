//! Shared types for `module-composition` — two operator modules composed
//! into one runtime process (blueprint §9.3).
//!
//! ```text
//!   astrs/timer/millis/20 ──► tick ──► [compose] ──result──► [collector]
//!                                        │
//!                            ScaleOperator ──out──► OffsetOperator
//!                            (one thread)            (one thread)
//! ```
//!
//! [`ScaleOperator`] and [`OffsetOperator`] are ordinary
//! [`astrs_operator_api::Operator`] implementations, hosted together by one
//! [`astrs_runtime::RuntimeHost`] inside the `compose-host` binary.
//! `ScaleOperator` reads the node's own `tick` input and multiplies a
//! running counter by [`SCALE_FACTOR`]; `OffsetOperator` reads
//! `ScaleOperator`'s output directly — a function call inside one process,
//! never an IPC hop (blueprint §4.2) — and adds [`OFFSET_CONSTANT`],
//! publishing the sum on the node's own `result` output. [`composed_value`]
//! is the pure function both operators together compute, and what this
//! crate's tests check the real, running composition against.
//!
//! # Why `compose` is a `path:` node, not a manifest `operators:` node
//!
//! Blueprint §8.3 lists `operators:` as a node source in its own right, and
//! `astrs-manifest` parses and validates it (`astrs_manifest::OperatorConfig`).
//! But the daemon spawns a **fixed binary literally named `astrs-runtime`**
//! for that source kind (`astrs-daemon`'s `spawn::process` module), and that
//! generic binary's own docs record a known gap: the wire handshake between
//! daemon and runtime host today carries each operator's id and registry
//! name, but not the per-operator `inputs`/`outputs` wiring
//! `astrs_manifest::OperatorConfig` carries — so a *manifest-declared*
//! `operators:` node cannot yet compose two operators together for real.
//! Naming a binary of this crate's own `astrs-runtime` would additionally
//! collide with `crates/astrs-runtime`'s own `[[bin]]` of that exact name in
//! `target/<profile>/` (`tests/conformance`'s estate guard exists precisely
//! to catch that).
//!
//! `compose-host` sidesteps both problems the way `astrs-runtime`'s own
//! module docs describe as "a real deployment": a *different* binary that
//! `register_operator!`s its own types and builds its own
//! [`astrs_runtime::RuntimeConfig`] directly, in Rust — [`runtime_config`]
//! is exactly that config, shared so `compose-host`'s `main` and this
//! crate's own tests drive the identical wiring. The daemon still only ever
//! sees an ordinary node with one input and one output; what happens
//! *inside* it — two operators, two threads, no IPC between them — is this
//! example's claim, and it is real regardless of the manifest-level gap.
//!
//! # Why the composition test drives a real `RuntimeHost`
//!
//! Every other example in this estate proves its central claim with
//! pure-function unit tests. This one is different on purpose: the claim
//! *is* that two operators compose correctly inside `astrs-runtime`'s own
//! routing and threading, and a test that only checked
//! [`composed_value`]'s arithmetic would prove nothing about composition at
//! all. [`astrs_node_api::Node::init_testing`] exists precisely so a test
//! can run the real thing — a real [`astrs_runtime::RuntimeHost`], the real
//! `ScaleOperator`/`OffsetOperator`, a real daemon-shaped session — with
//! nothing but an in-process mock standing in for the socket. See this
//! crate's own
//! `tests::the_composed_pipeline_runs_two_operators_in_one_process`.

use std::collections::BTreeMap;

use astrs_manifest::{Input, OperatorConfig};
use astrs_operator_api::{
    OpEvent, OpOutput, OpResult, Operator, OperatorRegistry, Status, register_operator,
};
use astrs_runtime::RuntimeConfig;
use serde::{Deserialize, Serialize};

/// The `compose` node's own external input, and `ScaleOperator`'s own input
/// name — the two must match: a virtual source like `astrs/timer/millis/N`
/// cannot round-trip through a two-part `PortRef`, so `astrs-runtime`'s
/// routing falls back to matching an operator input's own name directly
/// against the *node's* declared input ids (see `astrs_runtime::routing`'s
/// module docs, "External, by name").
pub const TICK_PORT: &str = "tick";
/// The `compose` node's own external output, and `OffsetOperator`'s own
/// output name — matching is what makes `astrs-runtime` also publish this
/// operator's output on the node's own port of the same name (see
/// `astrs_runtime::routing`'s module docs, "Resolving one operator output").
pub const RESULT_PORT: &str = "result";

/// `ScaleOperator`'s registration id within `compose`'s `operators:` list.
pub const SCALE_ID: &str = "scale";
/// `OffsetOperator`'s registration id within `compose`'s `operators:` list.
pub const OFFSET_ID: &str = "offset";
/// `OffsetOperator`'s input name — fed from `scale/out`, a sibling edge
/// resolved entirely inside `astrs-runtime`, never touching the node's own
/// session (blueprint §4.2).
pub const OFFSET_INPUT: &str = "value";
/// `ScaleOperator`'s own output name (internal only — no node-level output
/// is named `out`).
pub const SCALE_OUTPUT: &str = "out";

/// The cadence `compose`'s `tick` input runs at, spelled out here so
/// [`operator_configs`] can quote the identical virtual-source string the
/// manifest declares (see [`TICK_PORT`]'s docs for why the string's content
/// does not actually change resolution, but a reader should not have to
/// wonder whether it might).
pub const TICK_INTERVAL_MS: u64 = 20;

/// What `ScaleOperator` multiplies its running tick counter by.
pub const SCALE_FACTOR: i64 = 3;
/// What `OffsetOperator` adds to what `ScaleOperator` sends it.
pub const OFFSET_CONSTANT: i64 = 100;

/// Environment variable overriding how many ticks `compose-host` runs for.
pub const ENV_TICKS: &str = "MODULE_COMPOSITION_TICKS";
/// Environment variable naming the JSON file `compose-collector` writes its
/// [`CollectorReport`] to.
pub const ENV_REPORT_PATH: &str = "MODULE_COMPOSITION_REPORT";

/// How many ticks `compose-host` processes by default.
pub const DEFAULT_TICKS: u64 = 40;
/// How many bytes one composed value's wire payload carries: one
/// little-endian `i64`.
pub const VALUE_BYTES: usize = 8;

/// How many ticks this run should process.
#[must_use]
pub fn tick_budget() -> u64 {
    std::env::var(ENV_TICKS)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|budget| *budget > 0)
        .unwrap_or(DEFAULT_TICKS)
}

/// Where the collector writes its report when the manifest names no path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-module-composition-report.json")
}

/// The JSON file this run's collector writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// The value the two-operator pipeline should produce for the `index`-th
/// tick it processes (0-based): `ScaleOperator`'s own arithmetic, then
/// `OffsetOperator`'s, composed — a pure function so a test (or a reader)
/// can predict the whole run without executing anything.
#[must_use]
pub const fn composed_value(index: i64) -> i64 {
    index * SCALE_FACTOR + OFFSET_CONSTANT
}

/// Encodes a composed value as its wire payload: eight little-endian bytes.
#[must_use]
pub fn value_payload(value: i64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

/// Decodes a composed value out of a payload.
///
/// [`None`] if `payload` is not exactly [`VALUE_BYTES`] long.
#[must_use]
pub fn value_of(payload: &[u8]) -> Option<i64> {
    let bytes: [u8; VALUE_BYTES] = payload.try_into().ok()?;
    Some(i64::from_le_bytes(bytes))
}

/// Multiplies a running tick counter by [`SCALE_FACTOR`] and publishes it
/// on [`SCALE_OUTPUT`] — the first stage of the composed pipeline.
///
/// Fed from the node's own `tick` input (an `astrs/timer/*` virtual
/// source, which carries no payload at all — this operator's own counter,
/// not the event, is what it multiplies).
#[derive(Debug, Default)]
pub struct ScaleOperator {
    /// The next tick index to multiply.
    next_index: i64,
}

impl Operator for ScaleOperator {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input { id, metadata, .. } if id.as_str() == TICK_PORT => {
                let value = self.next_index * SCALE_FACTOR;
                self.next_index += 1;
                out.send_bytes(SCALE_OUTPUT, metadata.clone(), value_payload(value))?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

/// Adds [`OFFSET_CONSTANT`] to whatever [`ScaleOperator`] sends it, and
/// publishes the sum on the node's own `result` output — the second and
/// last stage of the composed pipeline.
#[derive(Debug, Default)]
pub struct OffsetOperator;

impl Operator for OffsetOperator {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                id,
                metadata,
                payload,
                ..
            } if id.as_str() == OFFSET_INPUT => {
                let scaled = value_of(payload).ok_or_else(|| {
                    astrs_operator_api::OpError::failed(format!(
                        "undecodable scale value ({} bytes)",
                        payload.len()
                    ))
                })?;
                let result = scaled + OFFSET_CONSTANT;
                out.send_bytes(RESULT_PORT, metadata.clone(), value_payload(result))?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

/// Registers [`ScaleOperator`] and [`OffsetOperator`] under the names
/// [`operator_configs`] refers to them by.
///
/// # Errors
///
/// [`astrs_operator_api::OpError::DuplicateOperator`] — unreachable in
/// practice, since the two names below are two different literal strings,
/// but propagated rather than unwrapped so a future third operator with a
/// copy-pasted name fails loudly instead of silently shadowing one of
/// these two.
pub fn build_registry() -> OpResult<OperatorRegistry> {
    OperatorRegistry::from_entries([
        register_operator!(ScaleOperator),
        register_operator!(OffsetOperator),
    ])
}

/// The `operators:` list `compose-host` hosts: `scale` feeding `offset`
/// entirely inside one process.
///
/// This is exactly the shape a manifest's own `operators:` node would
/// declare (§8.3, `astrs_manifest::OperatorConfig`'s own doctest uses the
/// identical `crop`/`nms`, sibling-wired pattern) — `compose-host` builds it
/// in Rust instead of reading it from `dataflow.yml` for the reason this
/// crate's own docs give.
///
/// Both inputs ask for [`astrs_manifest::QueuePolicy::Backpressure`]
/// explicitly rather than accepting `Input::from_source`'s own default
/// (`DropOldest` at `queue_size` 10, §11.2) — the same "every edge is
/// lossless" reasoning `examples/record-replay` documents for its own
/// byte-for-byte claim applies here: [`composed_value`] is a claim about
/// *every* tick this run processes, and `DropOldest` would make that claim
/// untestable by construction rather than false. It costs nothing in normal
/// operation (`compose`'s own `tick` is a 20 ms timer, never a burst) and it
/// is what lets `tests::the_composed_pipeline_runs_two_operators_in_one_process`
/// push a dozen ticks through `MockDaemon::send_input` back to back, ahead of
/// the operator threads starting to drain, without any of them being
/// silently evicted before the host even started running (`DropOldest`'s
/// eviction is deliberately signal-free — see `astrs_scheduler`'s queue
/// module docs — so a lost tick there would not even show up as a log line).
#[must_use]
pub fn operator_configs() -> Vec<OperatorConfig> {
    vec![
        OperatorConfig {
            id: SCALE_ID.to_owned(),
            operator: "ScaleOperator".to_owned(),
            dylib: None,
            wasm: None,
            hub: None,
            inputs: BTreeMap::from([(
                TICK_PORT.to_owned(),
                lossless_input(format!("astrs/timer/millis/{TICK_INTERVAL_MS}")),
            )]),
            outputs: vec![SCALE_OUTPUT.to_owned()],
            config: BTreeMap::new(),
        },
        OperatorConfig {
            id: OFFSET_ID.to_owned(),
            operator: "OffsetOperator".to_owned(),
            dylib: None,
            wasm: None,
            hub: None,
            inputs: BTreeMap::from([(
                OFFSET_INPUT.to_owned(),
                lossless_input(format!("{SCALE_ID}/{SCALE_OUTPUT}")),
            )]),
            outputs: vec![RESULT_PORT.to_owned()],
            config: BTreeMap::new(),
        },
    ]
}

/// An [`Input`] from `source`, with its queue policy overridden to
/// [`astrs_manifest::QueuePolicy::Backpressure`] — see [`operator_configs`]'s
/// own docs for why this composed pipeline asks for that on every edge.
fn lossless_input(source: impl Into<String>) -> Input {
    let mut input = Input::from_source(source);
    input.queue_policy = astrs_manifest::QueuePolicy::Backpressure;
    input
}

/// Builds the whole [`RuntimeConfig`] `compose-host` hosts: the operator
/// list plus the registry that knows how to build each one.
///
/// # Errors
///
/// As [`build_registry`].
pub fn runtime_config() -> OpResult<RuntimeConfig> {
    Ok(RuntimeConfig::new(operator_configs(), build_registry()?))
}

/// The collector's verdict on one run: every result it received, and
/// whether each matched [`composed_value`] at its own arrival order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectorReport {
    /// Every result received, in arrival order.
    pub values: Vec<i64>,
    /// The 0-based index of every arrival whose value did not match
    /// [`composed_value`] at that position.
    pub mismatches: Vec<u64>,
}

impl CollectorReport {
    /// Records one arrival, checking it against [`composed_value`] at its
    /// own position in the stream.
    pub fn push(&mut self, value: i64) {
        let index = self.values.len() as i64;
        if value != composed_value(index) {
            self.mismatches.push(index as u64);
        }
        self.values.push(value);
    }

    /// Whether every expected value arrived, in order, with no mismatches.
    #[must_use]
    pub fn is_clean(&self, expected: u64) -> bool {
        self.mismatches.is_empty() && self.values.len() as u64 == expected
    }

    /// Renders the report as pretty JSON.
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

    /// The composed value is a pure, deterministic function of the tick
    /// index.
    #[test]
    fn composed_value_is_deterministic() {
        assert_eq!(composed_value(0), OFFSET_CONSTANT);
        assert_eq!(composed_value(1), SCALE_FACTOR + OFFSET_CONSTANT);
        assert_eq!(composed_value(10), 10 * SCALE_FACTOR + OFFSET_CONSTANT);
        assert_eq!(composed_value(0), composed_value(0));
    }

    /// A composed value round-trips through encode/decode, and a payload of
    /// the wrong length is reported as absent rather than guessed at.
    #[test]
    fn value_payloads_round_trip_and_reject_the_wrong_length() {
        for index in 0..16i64 {
            let value = composed_value(index);
            let payload = value_payload(value);
            assert_eq!(payload.len(), VALUE_BYTES);
            assert_eq!(value_of(&payload), Some(value));
        }
        assert_eq!(value_of(&[1, 2, 3]), None);
    }

    /// `operator_configs` wires `offset` to `scale`'s own output by
    /// sibling reference, and both operators' externally-visible ports
    /// (`scale`'s `tick` input, `offset`'s `result` output) match the node
    /// level port names they must match to resolve at all (see this
    /// crate's module docs).
    #[test]
    fn operator_configs_wire_scale_into_offset() {
        let configs = operator_configs();
        assert_eq!(configs.len(), 2);
        let scale = configs.iter().find(|op| op.id == SCALE_ID).unwrap();
        let offset = configs.iter().find(|op| op.id == OFFSET_ID).unwrap();

        assert!(scale.inputs.contains_key(TICK_PORT), "{scale:?}");
        assert_eq!(scale.outputs, vec![SCALE_OUTPUT.to_owned()]);

        let offset_source = &offset
            .inputs
            .get(OFFSET_INPUT)
            .expect("an offset input")
            .source;
        assert_eq!(offset_source, &format!("{SCALE_ID}/{SCALE_OUTPUT}"));
        assert_eq!(offset.outputs, vec![RESULT_PORT.to_owned()]);
    }

    /// The registry knows both operators by the exact names
    /// `operator_configs` refers to them under, and nothing else.
    #[test]
    fn the_registry_knows_both_operators_by_name() {
        let registry = build_registry().unwrap();
        assert!(registry.contains("ScaleOperator"));
        assert!(registry.contains("OffsetOperator"));
        assert_eq!(registry.len(), 2);
        for config in operator_configs() {
            assert!(
                registry.contains(&config.operator),
                "no registry entry for {:?}",
                config.operator
            );
        }
    }

    /// A collector report of every expected value, in order, is clean.
    #[test]
    fn a_report_of_every_expected_value_is_clean() {
        let mut report = CollectorReport::default();
        for index in 0..10i64 {
            report.push(composed_value(index));
        }
        assert!(report.is_clean(10), "{report:?}");
    }

    /// A single wrong value at one position is caught, named by its
    /// position, and the report is not clean.
    #[test]
    fn a_wrong_value_is_caught_and_named() {
        let mut report = CollectorReport::default();
        report.push(composed_value(0));
        report.push(9999); // should have been composed_value(1)
        report.push(composed_value(2));
        assert_eq!(report.mismatches, vec![1]);
        assert!(!report.is_clean(3));
    }

    /// The report round-trips as JSON.
    #[test]
    fn a_report_round_trips_as_json() {
        let mut report = CollectorReport::default();
        report.push(composed_value(0));
        let json = report.to_json().unwrap();
        let parsed: CollectorReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    /// The report path defaults under the temporary directory.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, names this dataflow, and
    /// declares `compose` as an ordinary `path:` node (not `operators:`) —
    /// see this crate's module docs for why.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("module-composition"));
        let compose = manifest
            .nodes
            .iter()
            .find(|node| node.id == "compose")
            .expect("a compose node");
        assert!(compose.path.is_some(), "compose must be a path: node");
        assert!(
            compose.operators.is_none(),
            "not a manifest operators: node"
        );
    }

    /// The claim this whole example exists to make: two operators, hosted
    /// by one real `RuntimeHost`, in one process, on two threads, produce
    /// exactly [`composed_value`] for every tick — driven through
    /// `Node::init_testing`'s real session machinery, not a stub.
    #[test]
    fn the_composed_pipeline_runs_two_operators_in_one_process() {
        use astrs_node_api::testing::MockDaemon;
        use astrs_runtime::RuntimeHost;
        use astrs_time::HlcTimestamp;
        use astrs_wire::{
            DataId, InputSpec, Metadata, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, PortRef,
            QueuePolicy, StopCause,
        };

        let ticks: u64 = 12;

        // `MockDaemon::connect_node` directly, rather than the
        // `TestHarness` convenience wrapper: `TestHarness` implements
        // `Drop` (it shuts its own daemon down), which forbids moving
        // `node`/`events` out of it individually — and `RuntimeHost::new`
        // needs to own both.
        let daemon = MockDaemon::start().unwrap();
        let node_id = NodeId::new("compose").unwrap();
        let tick_id = DataId::new(TICK_PORT).unwrap();
        let result_id = DataId::new(RESULT_PORT).unwrap();
        // `Backpressure`, not the default `DropOldest`: every tick below is
        // sent through `MockDaemon::send_input` back to back, immediately
        // after spawning `host.run()` on its own thread, with nothing
        // pacing the two against each other. `DropOldest`'s eviction is
        // silent by design (no `QueueSignal`, see `astrs_scheduler::queue`'s
        // module docs) and would let a burst that outruns the operator
        // thread's start-up lose ticks before this test could ever observe
        // it — see `operator_configs`'s own docs for the identical
        // reasoning applied to the two operators' own inputs.
        let spec = NodeSpawnSpec::new(daemon.dataflow(), node_id.clone(), 0, NodeSource::Dynamic)
            .with_input(
                InputSpec::new(
                    tick_id.clone(),
                    PortRef::from_parts("timer", "tick").unwrap(),
                )
                .with_queue(astrs_wire::DEFAULT_QUEUE_SIZE, QueuePolicy::Backpressure),
            )
            .with_output(OutputSpec::new(result_id.clone()));
        let (node, events) = daemon.connect_node(spec).unwrap();

        let config = runtime_config().unwrap();
        let host = RuntimeHost::new(node, events, config).unwrap();
        let run = std::thread::spawn(move || host.run());

        for _ in 0..ticks {
            daemon
                .send_input(
                    &node_id,
                    &tick_id,
                    Metadata::new(HlcTimestamp::EPOCH),
                    Vec::new(),
                )
                .unwrap();
        }

        let sends = daemon
            .wait_for_sends(
                &node_id,
                &result_id,
                ticks as usize,
                std::time::Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(sends.len() as u64, ticks);
        for (index, send) in sends.iter().enumerate() {
            let bytes = send.bytes().expect("a result payload");
            let value = value_of(bytes).expect("a decodable composed value");
            assert_eq!(value, composed_value(index as i64), "result #{index}");
        }

        daemon.stop(&node_id, StopCause::Requested).unwrap();
        let report = run.join().unwrap().unwrap();
        assert!(report.all_healthy(), "{report:?}");
    }
}
