//! A thin, generic `astrs-runtime` binary.
//!
//! Blueprint §9.3: operators compile **into** the runtime binary via
//! [`astrs_operator_api::register_operator!`] — static dispatch, no
//! `dlopen`. That means *this* generic binary, which links no operator
//! crate of its own, can register none: it exists only to prove the
//! plumbing from [`astrs_node_api::Node::init_from_env`] through
//! [`astrs_runtime::RuntimeHost`] compiles and runs end to end, not to host
//! anyone's real operators.
//!
//! A real deployment is a *different* binary — one that
//! `register_operator!`s its own types and builds its own
//! [`astrs_runtime::RuntimeConfig`] — whose `main` is exactly this file's
//! body with a non-empty registry. That binary is what blueprint §5.1
//! means by "consumed by the CLI later": `astrs-cli` (or any operator
//! binary) calls [`astrs_runtime::run_runtime`] as a library function; it
//! does not spawn *this* executable.
//!
//! # Where the operator list comes from, today
//!
//! The daemon negotiates a `operators:` node's ports exactly like any
//! other node's (`NodeSpawnSpec::inputs`/`.outputs`), and separately tells
//! it *which* operators to build via `NodeSource::Runtime { operators:
//! Vec<OperatorSpec> }` — but `OperatorSpec` (blueprint's wire-level
//! shape) carries only an id, a registry name and static config, never the
//! per-operator input/output wiring `astrs_manifest::OperatorConfig`
//! carries (see `astrs-manifest`'s own `validate` module docs, and
//! `crate::routing`'s). Threading the fuller manifest shape onto the spawn
//! handshake is `astrs-daemon`'s side of this wave, not done yet, so this
//! binary can only build [`astrs_manifest::OperatorConfig`] entries with
//! empty `inputs`/`outputs` from what the wire already carries — enough to
//! prove every operator name the daemon expects is at least *known*, not
//! enough to wire a real chain. Reported as a deviation in this crate's
//! implementation report.

use astrs_manifest::OperatorConfig;
use astrs_node_api::Node;
use astrs_operator_api::OperatorRegistry;
use astrs_runtime::{RuntimeConfig, RuntimeHost};
use astrs_wire::NodeSource;

fn main() {
    std::process::exit(run());
}

/// Connects, builds the best-effort config described in this module's
/// docs, runs it, and returns the process exit code.
fn run() -> i32 {
    let (node, events) = match Node::init_from_env() {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("astrs-runtime: could not connect: {error}");
            return error.exit_code();
        }
    };

    let operators = match &node.descriptor().source {
        NodeSource::Runtime { operators } => operators
            .iter()
            .map(|spec| OperatorConfig {
                id: spec.id.as_str().to_owned(),
                operator: spec.registry_name.clone(),
                dylib: None,
                wasm: None,
                hub: None,
                inputs: Default::default(),
                outputs: Vec::new(),
                config: Default::default(),
            })
            .collect(),
        _ => Vec::new(),
    };

    let config = RuntimeConfig::new(operators, OperatorRegistry::new());
    let host = match RuntimeHost::new(node, events, config) {
        Ok(host) => host,
        Err(error) => {
            eprintln!("astrs-runtime: could not build the operator host: {error}");
            return 1;
        }
    };

    match host.run() {
        Ok(report) => {
            for operator in &report.operators {
                println!("{}: {:?}", operator.id, operator.outcome);
            }
            if report.all_healthy() { 0 } else { 1 }
        }
        Err(error) => {
            eprintln!("astrs-runtime: run failed: {error}");
            1
        }
    }
}
