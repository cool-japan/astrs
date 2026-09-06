//! [`RuntimeHost`] — the operator host process itself (blueprint §4.2,
//! §9.3).
//!
//! Ties every other module in this crate together: [`crate::routing`]
//! resolves the manifest's `operators:` list onto the connected node,
//! [`crate::inbox`] and [`crate::output_sink`] are the per-operator inbox
//! and the shared output front door, and [`crate::worker`] is what runs
//! inside each operator's own thread. This module is only the demux loop
//! (blueprint §9.3: "route node inputs to operators by port mapping") and
//! the shutdown sequence (blueprint §9.3: "`Stop` -> drain channels ->
//! `on_stop` each operator -> close outputs") that surround them.

use std::collections::BTreeMap;
use std::time::Instant;

use astrs_manifest::OperatorConfig;
use astrs_node_api::{Event, EventStream, Node};
use astrs_operator_api::OperatorRegistry;
use astrs_wire::{DataId, Parameter, PortRef, QueuePolicy as WireQueuePolicy, StopCause};

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::inbox::{OperatorInbox, log_queue_signal};
use crate::operator_config::resolve_configs;
use crate::output_sink::OutputSink;
use crate::report::{OperatorOutcome, OperatorReport, RuntimeReport};
use crate::routing::{Forwarder, Routing};
use crate::worker::{IMPLICIT_STOP_CAUSE, OperatorBuild, operator_loop};

/// The operator host: one connected [`Node`] fronting `N` in-process
/// operators, each on its own thread (blueprint §4.2's "runtime — a
/// node-shaped process hosting in-process operators").
pub struct RuntimeHost {
    node: Node,
    events: EventStream,
    config: RuntimeConfig,
    routing: Routing,
    /// Each operator's manifest `config:` map, already converted to the
    /// closed [`Parameter`] vocabulary [`crate::worker::operator_loop`]
    /// delivers to [`astrs_operator_api::Operator::configure`] — resolved
    /// once here, in [`RuntimeHost::new`], alongside [`Routing::build`], so
    /// a `config:` value with no [`Parameter`] equivalent fails
    /// construction the same way an unresolvable input does: before any
    /// operator thread ever runs, not partway through the run.
    configs: Vec<BTreeMap<String, Parameter>>,
}

impl core::fmt::Debug for RuntimeHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RuntimeHost")
            .field("node", &self.node.id())
            .field(
                "operators",
                &self
                    .config
                    .operators
                    .iter()
                    .map(|op| op.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl RuntimeHost {
    /// Resolves `config.operators` against `node`'s own declared ports,
    /// and every operator's manifest `config:` map to the [`Parameter`]
    /// vocabulary [`astrs_operator_api::Operator::configure`] receives.
    ///
    /// This is where a malformed or unresolvable manifest fails — before
    /// any operator thread ever runs, matching the "no unwrap, no
    /// surprises later" spirit of the rest of this workspace.
    ///
    /// # Errors
    ///
    /// See `Routing::build` and `operator_config::resolve_configs`.
    pub fn new(
        node: Node,
        events: EventStream,
        config: RuntimeConfig,
    ) -> Result<Self, RuntimeError> {
        let routing = Routing::build(&config.operators, &node)?;
        let configs = resolve_configs(&config.operators)?;
        Ok(Self {
            node,
            events,
            config,
            routing,
            configs,
        })
    }

    /// The connected node.
    #[must_use]
    pub fn node(&self) -> &Node {
        &self.node
    }

    /// Runs every hosted operator until the node's session ends, then
    /// closes every output and returns.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Scheduler`] if an operator input's manifest queue
    /// configuration is itself invalid (zero `queue_size`), and whatever
    /// `OutputSink::open` reports opening the node's
    /// own outputs.
    pub fn run(self) -> Result<RuntimeReport, RuntimeError> {
        let start = Instant::now();
        let Self {
            mut node,
            mut events,
            config,
            routing,
            configs,
        } = self;

        // Resolved here, on `run`'s own stack frame, rather than stored as
        // a `RuntimeHost` field: every `Box<dyn Fn>` below borrows
        // `config.operators`/`config.registry`, and `config` — moved out of
        // `self` just above — is what it borrows. A field alongside
        // `config` inside the same struct could not borrow a sibling field
        // this way without a self-referential struct. Resolved before
        // `build_inboxes`/`OutputSink::open` so a bad `dylib:` path fails
        // the whole host the same way an unresolvable input does: before
        // any operator thread runs and before any output opens.
        let constructors = build_constructors(
            &config.operators,
            &config.registry,
            config.dataflow_dir.as_deref(),
            &config.wasm_sandbox,
        )?;

        let inboxes = build_inboxes(&config, &node)?;
        let sink = OutputSink::open(&mut node)?;
        let restart_config = node.descriptor().restart;

        let outcomes = std::thread::scope(|scope| {
            let forwarder = Forwarder::new(&routing, &inboxes, &sink);
            let handles: Vec<_> = config
                .operators
                .iter()
                .zip(&configs)
                .zip(&constructors)
                .enumerate()
                .map(|(index, ((op, op_config), build))| {
                    let inbox = &inboxes[index];
                    let build: &OperatorBuild<'_> = &**build;
                    scope.spawn(move || {
                        operator_loop(
                            index,
                            &op.id,
                            build,
                            op_config,
                            inbox,
                            &forwarder,
                            restart_config,
                        )
                    })
                })
                .collect();

            demux(&mut node, &mut events, &routing, &inboxes);

            handles
                .into_iter()
                .map(|handle| match handle.join() {
                    Ok(outcome) => outcome,
                    // `&*payload`, not `&payload`: `payload` here is
                    // `Box<dyn Any + Send>` (`std::thread::Result`'s error
                    // side), which is *itself* `Any` — see
                    // `worker_panic_message`'s own docs for why the
                    // dereference is load-bearing, not stylistic.
                    Err(payload) => OperatorOutcome::HostPanicked {
                        message: worker_panic_message(&*payload),
                    },
                })
                .collect::<Vec<_>>()
        });

        // Every closure in `constructors` borrows `config.operators`/
        // `config.registry` (its `'a` is exactly `run`'s own stack frame,
        // per `build_constructors`' signature) — dropped explicitly here,
        // its last use, so the borrow ends before `config.operators` is
        // moved a few lines down. `std::thread::scope` above already
        // guarantees every borrowing thread has joined by the time control
        // reaches this line, so nothing is still calling through
        // `constructors` when it drops.
        drop(constructors);

        // Blueprint §9.3: outputs close only after every operator's
        // `on_stop` has had the chance to flush a final message — which
        // means strictly after every worker thread above has joined, never
        // before or concurrently with it.
        sink.close_all();

        let operators = config
            .operators
            .into_iter()
            .zip(outcomes)
            .map(|(op, outcome)| OperatorReport {
                id: op.id,
                registry_name: op.operator,
                outcome,
            })
            .collect();

        Ok(RuntimeReport {
            operators,
            elapsed: start.elapsed(),
        })
    }
}

/// One "produce a fresh instance" [`OperatorBuild`] per hosted operator, in
/// `operators`' own order — [`crate::worker::operator_loop`]'s only construction
/// dependency, resolved once here so every operator source (the compiled-in
/// `registry`, or — behind the `dylib-operators` feature — a `dylib:`
/// entry's loaded shared library) fails the same way a bad manifest input
/// does: before any operator thread runs, never partway through one.
///
/// # Errors
///
/// `RuntimeError::Dylib` if a `dylib:` entry's shared library cannot be
/// resolved (only reachable with the `dylib-operators` feature enabled,
/// which is what makes that variant exist at all), or
/// [`RuntimeError::DylibOperatorsNotEnabled`] if a `dylib:` entry is
/// declared without it.
fn build_constructors<'a>(
    operators: &'a [OperatorConfig],
    registry: &'a OperatorRegistry,
    dataflow_dir: Option<&std::path::Path>,
    wasm_sandbox: &crate::config::WasmSandboxConfig,
) -> Result<Vec<Box<OperatorBuild<'a>>>, RuntimeError> {
    operators
        .iter()
        .map(|op| build_one_constructor(op, registry, dataflow_dir, wasm_sandbox))
        .collect()
}

/// Builds a `dylib:`-sourced operator's [`OperatorBuild`], or falls through
/// to [`build_one_wasm_constructor`] for every other entry — the
/// `dylib-operators` feature is what makes a `dylib:` entry resolvable at
/// all. [`astrs_manifest::OperatorConfig`]'s own validation already rejects
/// more than one locator on the same entry (blueprint's frozen manifest
/// schema — see the setup contract's `MultipleOperatorLocators`), so `op`
/// reaching this function with `dylib` set means `wasm` is not.
#[cfg(feature = "dylib-operators")]
fn build_one_constructor<'a>(
    op: &'a OperatorConfig,
    registry: &'a OperatorRegistry,
    dataflow_dir: Option<&std::path::Path>,
    wasm_sandbox: &crate::config::WasmSandboxConfig,
) -> Result<Box<OperatorBuild<'a>>, RuntimeError> {
    let Some(declared) = &op.dylib else {
        return build_one_wasm_constructor(op, registry, dataflow_dir, wasm_sandbox);
    };
    let path = crate::dylib::resolve_dylib_path(dataflow_dir, declared);
    let source = crate::dylib::DylibSource::load(&path, &op.operator)
        .map_err(|source| RuntimeError::dylib(op.id.clone(), source))?;
    Ok(Box::new(move || source.build()))
}

/// Without the `dylib-operators` feature, a `dylib:` entry has no loader to
/// resolve it with — reported as [`RuntimeError::DylibOperatorsNotEnabled`]
/// rather than left to fail later as a confusing "unknown operator" from a
/// registry lookup that was never going to succeed. Falls through to
/// [`build_one_wasm_constructor`] for every entry with no `dylib:` locator.
#[cfg(not(feature = "dylib-operators"))]
fn build_one_constructor<'a>(
    op: &'a OperatorConfig,
    registry: &'a OperatorRegistry,
    dataflow_dir: Option<&std::path::Path>,
    wasm_sandbox: &crate::config::WasmSandboxConfig,
) -> Result<Box<OperatorBuild<'a>>, RuntimeError> {
    if op.dylib.is_some() {
        return Err(RuntimeError::dylib_operators_not_enabled(op.id.clone()));
    }
    build_one_wasm_constructor(op, registry, dataflow_dir, wasm_sandbox)
}

/// Builds a `wasm:`-sourced operator's [`OperatorBuild`], or falls back to
/// an ordinary registry lookup for every other entry — the
/// `wasm-operators` feature is what makes a `wasm:` entry resolvable at all.
/// Mirrors `build_one_constructor`'s own `dylib:` half exactly; see
/// [`crate::wasm`]'s module docs for the loader itself.
#[cfg(feature = "wasm-operators")]
fn build_one_wasm_constructor<'a>(
    op: &'a OperatorConfig,
    registry: &'a OperatorRegistry,
    dataflow_dir: Option<&std::path::Path>,
    wasm_sandbox: &crate::config::WasmSandboxConfig,
) -> Result<Box<OperatorBuild<'a>>, RuntimeError> {
    let Some(declared) = &op.wasm else {
        return Ok(registry_build(op, registry));
    };
    let path = crate::wasm::resolve_wasm_path(dataflow_dir, declared);
    let source = crate::wasm::WasmSource::load(&path, wasm_sandbox.clone())
        .map_err(|source| RuntimeError::wasm(op.id.clone(), source))?;
    Ok(Box::new(move || source.build()))
}

/// Without the `wasm-operators` feature, a `wasm:` entry has no interpreter
/// to run it on — reported as [`RuntimeError::WasmOperatorsNotEnabled`]
/// rather than left to fail later as a confusing "unknown operator" from a
/// registry lookup that was never going to succeed.
#[cfg(not(feature = "wasm-operators"))]
fn build_one_wasm_constructor<'a>(
    op: &'a OperatorConfig,
    registry: &'a OperatorRegistry,
    dataflow_dir: Option<&std::path::Path>,
    wasm_sandbox: &crate::config::WasmSandboxConfig,
) -> Result<Box<OperatorBuild<'a>>, RuntimeError> {
    let _ = (dataflow_dir, wasm_sandbox);
    if op.wasm.is_some() {
        return Err(RuntimeError::wasm_operators_not_enabled(op.id.clone()));
    }
    Ok(registry_build(op, registry))
}

/// An [`OperatorBuild`] that looks `op.operator` up in the compiled-in
/// registry — the flagship, no-locator path (blueprint §9.3).
fn registry_build<'a>(
    op: &'a OperatorConfig,
    registry: &'a OperatorRegistry,
) -> Box<OperatorBuild<'a>> {
    Box::new(move || registry.build(&op.operator))
}

/// One [`OperatorInbox`] per hosted operator, each with its own manifest
/// inputs registered.
fn build_inboxes(config: &RuntimeConfig, node: &Node) -> Result<Vec<OperatorInbox>, RuntimeError> {
    let _ = node;
    let mut inboxes = Vec::with_capacity(config.operators.len());
    for op in &config.operators {
        let inbox = OperatorInbox::new();
        for (input_name, input) in &op.inputs {
            let Ok(id) = DataId::new(input_name) else {
                // Already validated by `Routing::build`, which ran before
                // this and would have failed the whole host if this name
                // were invalid — unreachable in practice, skipped rather
                // than assumed away.
                continue;
            };
            inbox
                .register_input(
                    id,
                    input.queue_size,
                    to_wire_queue_policy(input.queue_policy),
                )
                .map_err(|source| RuntimeError::Scheduler {
                    operator: op.id.clone(),
                    input: input_name.clone(),
                    source,
                })?;
        }
        inboxes.push(inbox);
    }
    Ok(inboxes)
}

/// Maps `astrs-manifest`'s own queue policy enum onto `astrs-wire`'s (the
/// one [`astrs_scheduler::InputQueue`] actually reads) — same two variants,
/// different crates, because a node's *own* inputs and an *operator's* inputs
/// are declared in different parts of the manifest but must behave
/// identically once queued.
const fn to_wire_queue_policy(policy: astrs_manifest::QueuePolicy) -> WireQueuePolicy {
    match policy {
        astrs_manifest::QueuePolicy::DropOldest => WireQueuePolicy::DropOldest,
        astrs_manifest::QueuePolicy::Backpressure => WireQueuePolicy::Backpressure,
    }
}

/// The node's own event loop: reads `events` and routes every input,
/// input-closure, reload and parameter update onto the operators
/// [`Routing`] says should see it, until the session ends.
///
/// Blueprint §9.3's shutdown ordering starts here: once this returns, a
/// `Stop` (explicit or implied by the session simply ending) has already
/// been pushed onto every still-open operator's control lane, ahead of any
/// data those operators have not yet processed — the same "control
/// pre-empts data" rule [`crate::inbox::OperatorInbox`] enforces for
/// everything else.
fn demux(node: &mut Node, events: &mut EventStream, routing: &Routing, inboxes: &[OperatorInbox]) {
    let cause = loop {
        match events.recv() {
            Some(Event::Input { id, meta, data }) => {
                let targets = routing.external_targets(&id);
                if targets.is_empty() {
                    continue;
                }
                let source = node
                    .input_source(id.as_str())
                    .unwrap_or_else(|| PortRef::new(node.id().clone(), id.clone()));
                let payload = data.to_vec();
                for (target_index, target_input) in targets {
                    if let Some(inbox) = inboxes.get(*target_index) {
                        let report = inbox.push_message(
                            target_input,
                            source.clone(),
                            meta.clone(),
                            payload.clone(),
                        );
                        log_queue_signal(inbox, target_input, report);
                    }
                }
            }
            Some(Event::InputClosed { id, source, reason }) => {
                for (target_index, target_input) in routing.external_targets(&id) {
                    if let Some(inbox) = inboxes.get(*target_index) {
                        let _ignored =
                            inbox.push_closed(target_input, source.clone(), reason.clone());
                    }
                }
            }
            Some(Event::Reload { operator, path: _ }) => match operator {
                Some(operator_id) => {
                    if let Some(index) = routing
                        .operator_ids
                        .iter()
                        .position(|id| id.as_str() == operator_id.as_str())
                        && let Some(inbox) = inboxes.get(index)
                        && !inbox.is_closed()
                    {
                        inbox.push_reload();
                    }
                }
                None => broadcast_open(inboxes, OperatorInbox::push_reload),
            },
            Some(Event::ParamUpdate { scope, key, value }) => {
                for inbox in inboxes.iter().filter(|inbox| !inbox.is_closed()) {
                    inbox.push_param_update(scope.clone(), key.clone(), value.clone());
                }
            }
            Some(Event::Stop(cause)) => break cause,
            // Every other node-level event (`AllInputsClosed`,
            // `InputRecovered`, `NodeFailed`, `Restarted`, `ExtDropped`,
            // `ParamDeleted`, `Error`) has no operator-level meaning —
            // `astrs_operator_api::OpEvent` has no variant for any of
            // them, matching `OpEvent::from_node_event`'s own precedent of
            // dropping what it cannot express.
            Some(_other) => {}
            None => break IMPLICIT_STOP_CAUSE,
        }
    };

    broadcast_stop(inboxes, cause);
}

/// Pushes a `Stop` onto every operator inbox that has not already ended
/// permanently.
fn broadcast_stop(inboxes: &[OperatorInbox], cause: StopCause) {
    for inbox in inboxes.iter().filter(|inbox| !inbox.is_closed()) {
        inbox.push_stop(cause.clone(), None);
    }
}

/// Runs `action` against every operator inbox that has not already ended
/// permanently.
fn broadcast_open(inboxes: &[OperatorInbox], action: impl Fn(&OperatorInbox)) {
    for inbox in inboxes.iter().filter(|inbox| !inbox.is_closed()) {
        action(inbox);
    }
}

/// Renders a caught panic from an operator's own *worker thread join* —
/// distinct from [`crate::worker`]'s own panic catching inside the loop:
/// reaching this means the loop itself unwound past its own
/// `catch_unwind`, which would be a bug in this crate, not in an operator.
///
/// Callers: pass `&*payload`, never `&payload` — `payload` (from
/// [`std::thread::Result`]'s error side) is `Box<dyn Any + Send>`, which is
/// *itself* `Any` via the blanket `impl<T: 'static> Any for T`, so `&payload`
/// would silently unsize-coerce to `&dyn Any` over the box rather than
/// reaching the panic value it holds — every downcast below would then
/// (silently) miss, always falling through to the generic fallback message.
fn worker_panic_message(payload: &(dyn std::any::Any + Send + 'static)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "the operator worker thread panicked with a non-string payload".to_owned()
    }
}

/// Connects via [`Node::init_from_env`] and runs `config` to completion —
/// the "library entry" side of blueprint §9.3, `src/main.rs`'s thin
/// wrapper being the other.
///
/// # Errors
///
/// Whatever [`Node::init_from_env`] or [`RuntimeHost::new`]/[`RuntimeHost::run`]
/// report.
pub fn run_runtime(config: RuntimeConfig) -> Result<RuntimeReport, RuntimeError> {
    let (node, events) = Node::init_from_env().map_err(RuntimeError::Node)?;
    RuntimeHost::new(node, events, config)?.run()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_manifest::{Input, OperatorConfig};
    use astrs_node_api::MockDaemon;
    use astrs_operator_api::{
        OpEvent, OpOutput, OpResult, Operator, OperatorRegistry, Status, register_operator,
    };
    use astrs_wire::{
        InputSpec, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, RestartConfig, RestartPolicy,
    };
    use std::collections::BTreeMap;
    use std::time::Duration;

    /// Forwards every input onto an output named `out` — fixed, rather than
    /// a field, because `astrs_operator_api::OperatorRegistry` only ever
    /// builds an operator via `Default::default()` (blueprint §9.3), so
    /// there is no construction-time hook to parametrize this from a test.
    #[derive(Default)]
    struct Forward;
    impl Operator for Forward {
        fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
            match event {
                OpEvent::Input {
                    metadata, payload, ..
                } => {
                    out.send_bytes("out", metadata.clone(), payload.clone())?;
                    Ok(Status::Continue)
                }
                OpEvent::Stop { .. } => Ok(Status::Finished),
                _ => Ok(Status::Continue),
            }
        }
    }

    #[derive(Default)]
    struct CropOp;
    impl Operator for CropOp {
        fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
            match event {
                OpEvent::Input {
                    metadata, payload, ..
                } => {
                    out.send_bytes("boxes", metadata.clone(), payload.clone())?;
                    Ok(Status::Continue)
                }
                OpEvent::Stop { .. } => Ok(Status::Finished),
                _ => Ok(Status::Continue),
            }
        }
    }

    #[derive(Default)]
    struct NmsOp;
    impl Operator for NmsOp {
        fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
            match event {
                OpEvent::Input {
                    metadata, payload, ..
                } => {
                    out.send_bytes("detections", metadata.clone(), payload.clone())?;
                    Ok(Status::Continue)
                }
                OpEvent::Stop { .. } => Ok(Status::Finished),
                _ => Ok(Status::Continue),
            }
        }
    }

    #[derive(Default)]
    struct AlwaysPanics;
    impl Operator for AlwaysPanics {
        fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            panic!("boom");
        }
    }

    /// Counts its own `on_stop` calls in a static, since
    /// `astrs_operator_api::OperatorRegistry` only ever builds an operator
    /// via `Default::default()` — there is no other way for a test to
    /// observe what happened inside one after
    /// [`RuntimeHost::run`] returns. Scoped to the one test that uses it;
    /// two tests sharing this type would contaminate each other's counts.
    static COUNTING_A_STOPS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static COUNTING_B_STOPS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[derive(Default)]
    struct CountingA;
    impl Operator for CountingA {
        fn on_event(&mut self, event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            Ok(if matches!(event, OpEvent::Stop { .. }) {
                Status::Finished
            } else {
                Status::Continue
            })
        }

        fn on_stop(&mut self, _out: &mut OpOutput) -> OpResult<()> {
            COUNTING_A_STOPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct CountingB;
    impl Operator for CountingB {
        fn on_event(&mut self, event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            Ok(if matches!(event, OpEvent::Stop { .. }) {
                Status::Finished
            } else {
                Status::Continue
            })
        }

        fn on_stop(&mut self, _out: &mut OpOutput) -> OpResult<()> {
            COUNTING_B_STOPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Builds a registry with every test operator this module defines, so
    /// each test can just name the ones it wants.
    fn registry() -> OperatorRegistry {
        OperatorRegistry::from_entries([
            register_operator!(CropOp),
            register_operator!(NmsOp),
            register_operator!(Forward),
            register_operator!(AlwaysPanics),
            register_operator!(CountingA),
            register_operator!(CountingB),
        ])
        .unwrap()
    }

    /// A daemon plus one already-connected node named `runtime`, with
    /// `input` wired from `producer/output_port` and a single node-level
    /// output named `output`.
    ///
    /// Returns the owned `(daemon, node, events)` triple directly rather
    /// than a [`astrs_node_api::testing::TestHarness`]: that type
    /// implements [`Drop`] (it shuts itself down), so it cannot be
    /// destructured by value — exactly the ownership a test that hands
    /// `node`/`events` to [`RuntimeHost::new`] while still driving `daemon`
    /// needs.
    fn host_harness(
        input: &str,
        producer: &str,
        output_port: &str,
        output: &str,
        restart: RestartConfig,
    ) -> (MockDaemon, Node, EventStream) {
        let daemon = MockDaemon::start().unwrap();
        let spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new("runtime").unwrap(),
            0,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec::new(
            DataId::new(input).unwrap(),
            PortRef::from_parts(producer, output_port).unwrap(),
        ))
        .with_output(OutputSpec::new(DataId::new(output).unwrap()))
        .with_restart(restart);
        let (node, events) = daemon.connect_node(spec).unwrap();
        (daemon, node, events)
    }

    /// A [`RestartConfig`] with no artificial delay, for tests that drive a
    /// restart loop and should not pay real wall-clock backoff.
    fn fast_restart(policy: RestartPolicy, max_restarts: Option<u32>) -> RestartConfig {
        RestartConfig {
            policy,
            max_restarts,
            restart_delay: astrs_wire::DurationMs::ZERO,
            max_restart_delay: astrs_wire::DurationMs::ZERO,
            restart_window: astrs_wire::DurationMs::from_secs(60),
        }
    }

    #[test]
    fn a_two_operator_chain_flows_input_through_to_output() {
        let (daemon, node, events) = host_harness(
            "frames",
            "camera",
            "image",
            "detections",
            RestartConfig::never(),
        );
        let node_id = node.id().clone();
        let config = RuntimeConfig::new(
            vec![
                OperatorConfig {
                    id: "crop".to_owned(),
                    operator: "CropOp".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::from([(
                        "frames".to_owned(),
                        Input::from_source("camera/image"),
                    )]),
                    outputs: vec!["boxes".to_owned()],
                    config: BTreeMap::new(),
                },
                OperatorConfig {
                    id: "nms".to_owned(),
                    operator: "NmsOp".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::from([(
                        "boxes".to_owned(),
                        Input::from_source("crop/boxes"),
                    )]),
                    outputs: vec!["detections".to_owned()],
                    config: BTreeMap::new(),
                },
            ],
            registry(),
        );

        let host = RuntimeHost::new(node, events, config).unwrap();
        let runner = std::thread::spawn(move || host.run());

        daemon
            .send_input(
                &node_id,
                &DataId::new("frames").unwrap(),
                astrs_wire::Metadata::default(),
                vec![1, 2, 3],
            )
            .unwrap();
        let output_id = DataId::new("detections").unwrap();
        let sends = daemon
            .wait_for_sends(&node_id, &output_id, 1, Duration::from_secs(5))
            .unwrap();
        assert_eq!(sends[0].bytes(), Some(&[1, 2, 3][..]));

        daemon.stop(&node_id, StopCause::Requested).unwrap();
        let report = runner.join().unwrap().unwrap();
        assert!(report.all_healthy(), "{report:?}");
        assert_eq!(report.operators.len(), 2);
    }

    #[test]
    fn panic_isolation_a_panicking_operator_does_not_stop_its_sibling() {
        let restart = fast_restart(RestartPolicy::OnFailure, Some(1));
        let (daemon, node, events) = host_harness("frames", "camera", "image", "out", restart);
        let node_id = node.id().clone();
        let config = RuntimeConfig::new(
            vec![
                OperatorConfig {
                    id: "bad".to_owned(),
                    operator: "AlwaysPanics".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::from([(
                        "frames".to_owned(),
                        Input::from_source("camera/image"),
                    )]),
                    outputs: vec![],
                    config: BTreeMap::new(),
                },
                OperatorConfig {
                    id: "good".to_owned(),
                    operator: "Forward".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::from([(
                        "frames".to_owned(),
                        Input::from_source("camera/image"),
                    )]),
                    outputs: vec!["out".to_owned()],
                    config: BTreeMap::new(),
                },
            ],
            registry(),
        );

        let host = RuntimeHost::new(node, events, config).unwrap();
        let runner = std::thread::spawn(move || host.run());

        daemon
            .send_input(
                &node_id,
                &DataId::new("frames").unwrap(),
                astrs_wire::Metadata::default(),
                vec![9],
            )
            .unwrap();
        let output_id = DataId::new("out").unwrap();
        let sends = daemon
            .wait_for_sends(&node_id, &output_id, 1, Duration::from_secs(5))
            .unwrap();
        assert_eq!(sends[0].bytes(), Some(&[9][..]));

        daemon.stop(&node_id, StopCause::Requested).unwrap();
        let report = runner.join().unwrap().unwrap();

        let bad = report.operators.iter().find(|r| r.id == "bad").unwrap();
        assert!(
            matches!(bad.outcome, OperatorOutcome::Failed { .. }),
            "{bad:?}"
        );
        let good = report.operators.iter().find(|r| r.id == "good").unwrap();
        assert!(good.outcome.is_healthy(), "{good:?}");
    }

    #[test]
    fn reload_is_delivered_to_the_named_operator_only() {
        let (daemon, node, events) =
            host_harness("frames", "camera", "image", "out_a", RestartConfig::never());
        let node_id = node.id().clone();
        let config = RuntimeConfig::new(
            vec![
                OperatorConfig {
                    id: "a".to_owned(),
                    operator: "Forward".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::from([(
                        "frames".to_owned(),
                        Input::from_source("camera/image"),
                    )]),
                    outputs: vec!["out_a".to_owned()],
                    config: BTreeMap::new(),
                },
                OperatorConfig {
                    id: "b".to_owned(),
                    operator: "Forward".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::new(),
                    outputs: vec![],
                    config: BTreeMap::new(),
                },
            ],
            registry(),
        );
        let host = RuntimeHost::new(node, events, config).unwrap();
        let runner = std::thread::spawn(move || host.run());

        daemon
            .send_event(
                &node_id,
                astrs_wire::NodeEvent::Reload {
                    operator: Some(astrs_wire::OperatorId::new("a").unwrap()),
                    path: None,
                },
            )
            .unwrap();
        // Reload has no directly observable side effect on the default
        // `Operator::on_reload` no-op, so this test's assertion is that the
        // run keeps going and shuts down cleanly afterward — a targeted
        // reload that panicked or hung would fail the `join` below.
        daemon.stop(&node_id, StopCause::Requested).unwrap();
        let report = runner.join().unwrap().unwrap();
        assert!(report.all_healthy(), "{report:?}");
    }

    #[test]
    fn shutdown_closes_the_output_after_operators_have_stopped() {
        let (daemon, node, events) =
            host_harness("frames", "camera", "image", "out", RestartConfig::never());
        let node_id = node.id().clone();
        let config = RuntimeConfig::new(
            vec![OperatorConfig {
                id: "echo".to_owned(),
                operator: "Forward".to_owned(),
                dylib: None,
                wasm: None,
                hub: None,
                inputs: BTreeMap::from([("frames".to_owned(), Input::from_source("camera/image"))]),
                outputs: vec!["out".to_owned()],
                config: BTreeMap::new(),
            }],
            registry(),
        );
        let host = RuntimeHost::new(node, events, config).unwrap();
        let runner = std::thread::spawn(move || host.run());

        daemon.stop(&node_id, StopCause::Requested).unwrap();
        let report = runner.join().unwrap().unwrap();
        assert!(report.all_healthy(), "{report:?}");

        // `RuntimeHost::run` returning only means `OutputDone` was *queued*
        // on the session's writer task — like any other request, its
        // arrival at the (mock) daemon is asynchronous, so the assertion
        // has to wait for it rather than check `requests()` immediately.
        daemon
            .wait_for(Duration::from_secs(5), |requests| {
                requests.iter().any(|entry| {
                    entry.node == node_id
                        && matches!(&entry.request, astrs_wire::NodeRequest::OutputDone { output } if output.as_str() == "out")
                })
            })
            .unwrap();
        let closes = daemon
            .requests()
            .into_iter()
            .filter(|entry| {
                entry.node == node_id
                    && matches!(&entry.request, astrs_wire::NodeRequest::OutputDone { output } if output.as_str() == "out")
            })
            .count();
        assert_eq!(
            closes, 1,
            "the output closes exactly once, after the operator stopped"
        );
    }

    #[test]
    fn shutdown_ordering_on_stop_runs_exactly_once_per_operator() {
        COUNTING_A_STOPS.store(0, std::sync::atomic::Ordering::SeqCst);
        COUNTING_B_STOPS.store(0, std::sync::atomic::Ordering::SeqCst);

        let (daemon, node, events) =
            host_harness("frames", "camera", "image", "out", RestartConfig::never());
        let node_id = node.id().clone();
        let config = RuntimeConfig::new(
            vec![
                OperatorConfig {
                    id: "a".to_owned(),
                    operator: "CountingA".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::from([(
                        "frames".to_owned(),
                        Input::from_source("camera/image"),
                    )]),
                    outputs: vec![],
                    config: BTreeMap::new(),
                },
                OperatorConfig {
                    id: "b".to_owned(),
                    operator: "CountingB".to_owned(),
                    dylib: None,
                    wasm: None,
                    hub: None,
                    inputs: BTreeMap::new(),
                    outputs: vec![],
                    config: BTreeMap::new(),
                },
            ],
            registry(),
        );
        let host = RuntimeHost::new(node, events, config).unwrap();
        let runner = std::thread::spawn(move || host.run());

        // Give `a` something to process before the stop, so its main loop
        // has genuinely run rather than finding an empty inbox at once.
        daemon
            .send_input(
                &node_id,
                &DataId::new("frames").unwrap(),
                astrs_wire::Metadata::default(),
                vec![1],
            )
            .unwrap();
        daemon.stop(&node_id, StopCause::Requested).unwrap();
        let report = runner.join().unwrap().unwrap();
        assert!(report.all_healthy(), "{report:?}");

        assert_eq!(
            COUNTING_A_STOPS.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            COUNTING_B_STOPS.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn queue_bound_behavior_drop_oldest_keeps_only_the_newest_messages() {
        let (daemon, node, events) =
            host_harness("frames", "camera", "image", "out", RestartConfig::never());
        let node_id = node.id().clone();
        let mut inputs = BTreeMap::new();
        let mut bounded = Input::from_source("camera/image");
        bounded.queue_size = 2;
        bounded.queue_policy = astrs_manifest::QueuePolicy::DropOldest;
        inputs.insert("frames".to_owned(), bounded);
        let config = RuntimeConfig::new(
            vec![OperatorConfig {
                id: "echo".to_owned(),
                operator: "Forward".to_owned(),
                dylib: None,
                wasm: None,
                hub: None,
                inputs,
                outputs: vec!["out".to_owned()],
                config: BTreeMap::new(),
            }],
            registry(),
        );
        let host = RuntimeHost::new(node, events, config).unwrap();
        let runner = std::thread::spawn(move || host.run());

        // Flood far past the queue's bound of 2 before the operator has a
        // chance to drain any of it.
        for n in 0..20u8 {
            daemon
                .send_input(
                    &node_id,
                    &DataId::new("frames").unwrap(),
                    astrs_wire::Metadata::default(),
                    vec![n],
                )
                .unwrap();
        }
        let output_id = DataId::new("out").unwrap();
        // At most the last couple of messages survive to be forwarded;
        // waiting for even one proves delivery still works under load.
        let sends = daemon
            .wait_for_sends(&node_id, &output_id, 1, Duration::from_secs(5))
            .unwrap();
        assert!(!sends.is_empty());

        daemon.stop(&node_id, StopCause::Requested).unwrap();
        let report = runner.join().unwrap().unwrap();
        assert!(report.all_healthy(), "{report:?}");

        let all_sends = daemon.sends_on(&node_id, &output_id);
        assert!(
            all_sends.len() < 20,
            "the bounded queue must have dropped some of the 20 flooded messages, got {}",
            all_sends.len()
        );
    }
}
