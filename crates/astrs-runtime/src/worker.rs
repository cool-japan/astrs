//! `operator_loop` — one hosted operator's worker thread body (blueprint
//! §9.3).
//!
//! Every hosted operator gets a genuine [`std::thread`] (not a task on a
//! shared pool): panic isolation via [`std::panic::catch_unwind`] stops an
//! unwind at the boundary either way, but a dedicated OS thread also means
//! a CPU-bound operator (a model doing inference) never steals a worker
//! another operator or the demux loop needs, and a thread that never comes
//! back from a hang is exactly one operator's problem, not the process's.
//!
//! # Restart runs in place, on this same thread
//!
//! Blueprint §9.3: *"panics are caught, reported as `NodeFailed`, and honor
//! the operator's restart policy without tearing down siblings."* A
//! restart that ran on some *other* thread (the demux loop, say) would have
//! to block that thread for the backoff delay, stalling every sibling's
//! delivery for no reason — the opposite of "siblings unaffected". Instead,
//! `operator_loop` itself is the restart loop: on a caught failure it
//! sleeps the backoff *here*, then rebuilds and resumes, all inside the one
//! thread this operator has always owned. The operator's
//! `OperatorInbox` outlives every incarnation, so a message
//! queued while the previous incarnation was unwinding is not lost to the
//! rebuild.
//!
//! # `on_stop` runs at most once per incarnation, and never after a panic
//!
//! [`astrs_operator_api::Operator::on_stop`]'s documented job is flushing a
//! final message — calling it on an object that just unwound out of
//! `on_start`/`on_event`/`on_reload` would run user code against
//! (potentially) half-initialized state for no benefit, so a panicked or
//! erroring incarnation skips straight to the restart decision. `on_stop`
//! only ever runs after [`astrs_operator_api::Status::Finished`] or an
//! [`astrs_operator_api::OpEvent::Stop`] was itself handled without error.
//!
//! # A `Stop` never restarts, whatever the policy says
//!
//! [`astrs_wire::RestartPolicy::Always`] restarts "after any exit, clean or
//! not" at the *node* granularity (blueprint §12), where a fresh incarnation
//! is a fresh process the daemon can feed new work. An operator incarnation
//! that just handled the *host's own* [`astrs_operator_api::OpEvent::Stop`]
//! has no such thing to look forward to: the demux loop that would feed a
//! rebuilt incarnation has itself already stopped reading. Rebuilding here
//! would produce a thread blocked forever on its own inbox, which
//! [`crate::RuntimeHost::run`] would then hang joining. So `Stop` always
//! ends an operator's run through this host, regardless of
//! [`astrs_wire::RestartPolicy`] — the one exception to "the policy
//! decides", justified by what a restart could not possibly accomplish
//! here.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use astrs_operator_api::{OpEvent, OpOutput, OpResult, Operator};
use astrs_wire::{Parameter, RestartConfig, RouteCloseReason, StopCause};

/// The [`StopCause`] used when the host's own session ends without ever
/// delivering an explicit `Stop` (blueprint §12: daemon gone is still a
/// shutdown).
pub(crate) const IMPLICIT_STOP_CAUSE: StopCause = StopCause::DaemonShutdown;

use crate::inbox::OperatorInbox;
use crate::report::{FailedHook, OperatorFailure, OperatorOutcome};
use crate::routing::Forwarder;

/// The one place `catch_unwind` is called, wrapping any of an operator's
/// four hooks (or its own construction) into a uniform outcome: the value
/// on success, or a rendered message plus whether it was a panic (as
/// opposed to an ordinary `Err`) on failure.
fn run_hook<T>(f: impl FnOnce() -> OpResult<T>) -> Result<T, (String, bool)> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(op_error)) => Err((op_error.to_string(), false)),
        // `&*payload`, not `&payload`: `payload` here is `Box<dyn Any +
        // Send>`, which is *itself* `Any` (the blanket `impl<T: 'static>
        // Any for T`) — `&payload` would silently unsize-coerce to `&dyn
        // Any` over the box, so `panic_message`'s downcasts would compare
        // against the box's own type rather than the panic payload it
        // holds, and always miss. Dereferencing first reaches the payload.
        Err(payload) => Err((panic_message(&*payload), true)),
    }
}

/// Renders a caught panic payload as a message. Every panic raised through
/// `panic!`/`assert!`/`.unwrap()`/`.expect()` carries a `&str` or `String`
/// payload; anything else gets a deliberately non-specific fallback rather
/// than a guess.
fn panic_message(payload: &(dyn std::any::Any + Send + 'static)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "operator panicked with a non-string payload".to_owned()
    }
}

/// What [`RestartState::decide`] concluded.
enum Decision {
    /// Sleep this long, then rebuild and resume.
    Restart(Duration),
    /// Do not restart; the operator's run through this host is over.
    GiveUp {
        /// Whether the restart *budget* (rather than the policy itself)
        /// was why.
        budget_exhausted: bool,
    },
}

/// This operator's own restart bookkeeping — independent of every sibling's
/// (blueprint §9.3: "siblings unaffected").
struct RestartState {
    /// How many times this operator has been rebuilt.
    incarnation: u32,
    /// The start of the current [`astrs_wire::RestartConfig::restart_window`].
    window_start: Instant,
    /// Restarts counted within the current window.
    restarts_in_window: u32,
}

impl RestartState {
    fn new() -> Self {
        Self {
            incarnation: 0,
            window_start: Instant::now(),
            restarts_in_window: 0,
        }
    }

    /// Decides what happens after this incarnation ended, `success`
    /// matching [`astrs_wire::RestartPolicy::should_restart`]'s own sense
    /// of the word (a caught failure is never a success; a clean
    /// [`astrs_operator_api::Status::Finished`] is).
    fn decide(&mut self, restart: &RestartConfig, success: bool) -> Decision {
        if !restart.policy.should_restart(success) {
            return Decision::GiveUp {
                budget_exhausted: false,
            };
        }
        let window = restart.restart_window.to_duration();
        if !window.is_zero() && self.window_start.elapsed() >= window {
            self.window_start = Instant::now();
            self.restarts_in_window = 0;
        }
        if restart.budget_exhausted(self.restarts_in_window) {
            return Decision::GiveUp {
                budget_exhausted: true,
            };
        }
        let delay = restart.backoff_for(self.incarnation).to_duration();
        self.restarts_in_window += 1;
        self.incarnation += 1;
        Decision::Restart(delay)
    }
}

/// Why one incarnation's main loop ended.
enum Ending {
    /// The inbox closed with nothing left to deliver — no `Stop` was ever
    /// seen (e.g. a caller dropped this operator's inbox directly, the
    /// path this module's own tests exercise).
    InboxClosed,
    /// [`astrs_operator_api::Status::Finished`] came back from an ordinary
    /// event.
    StatusFinished,
    /// An [`OpEvent::Stop`] was delivered and handled without error — see
    /// this module's docs on why this never restarts.
    HostStopping,
    /// `on_event` or `on_reload` itself failed.
    Failed {
        message: String,
        panicked: bool,
        hook: FailedHook,
    },
}

/// One hosted operator's "produce a fresh instance" step, resolved once by
/// [`crate::host::RuntimeHost::run`] before any operator thread spawns and
/// called again on every restarted incarnation.
///
/// The one seam between "where does this operator come from" and "how is
/// it run": a registry-sourced operator's build is
/// [`astrs_operator_api::OperatorRegistry::build`] itself (closed over its
/// own name), and — behind the `dylib-operators` feature — a
/// `dylib:`-sourced operator's build is `crate::dylib::DylibSource::build`
/// (closed over its loaded library; a plain code span rather than an
/// intra-doc link, since that module only exists when its own feature is
/// on). Both already return exactly this `OpResult<Box<dyn Operator>>`
/// shape, so [`operator_loop`] never needs to know which kind of source it
/// was handed.
///
/// Lifetime-parameterized (rather than a bare `dyn Fn(..) + Send + Sync`,
/// which — with no enclosing reference at its own definition site — would
/// default to a `'static` bound) because every real build closure borrows
/// non-`'static` data: a manifest's own `OperatorConfig`/`OperatorRegistry`,
/// borrowed for the span of one [`crate::host::RuntimeHost::run`] call.
pub(crate) type OperatorBuild<'a> = dyn Fn() -> OpResult<Box<dyn Operator>> + Send + Sync + 'a;

/// Builds one operator instance via `build`, then immediately runs its
/// [`Operator::configure`] with `config` — folding both possible failure
/// points (a panicking/erroring construction, and a panicking/erroring
/// `configure`) into one `(FailedHook, message, panicked)` triple, since
/// [`operator_loop`] treats either one exactly the same way: this
/// incarnation never reaches `on_start`, and the restart decision runs on
/// whichever tag came back.
fn build_and_configure(
    build: &OperatorBuild<'_>,
    config: &BTreeMap<String, Parameter>,
) -> Result<Box<dyn Operator>, (FailedHook, String, bool)> {
    let mut operator = run_hook(build)
        .map_err(|(message, panicked)| (FailedHook::Construct, message, panicked))?;
    run_hook(|| operator.configure(config))
        .map_err(|(message, panicked)| (FailedHook::Configure, message, panicked))?;
    Ok(operator)
}

/// Runs one hosted operator to completion: build, `configure`, `on_start`,
/// the main event loop, `on_stop`, and — on any caught failure — the
/// in-place restart loop described in this module's docs.
///
/// `config` is this operator's manifest `config:` map, already converted
/// from JSON to the closed [`Parameter`] vocabulary by
/// [`crate::operator_config::resolve_configs`] — delivered fresh to
/// [`Operator::configure`] on every incarnation, restarts included (see
/// that method's own docs on why).
///
/// Never panics itself: every failure this function can observe is folded
/// into the returned [`OperatorOutcome`], which is exactly what lets a
/// panicking operator never take its host down.
// Seven parameters, each independently meaningful to a caller and none
// naturally grouped (this crate's `routing::resolve_input` carries the
// same `#[allow]` for the same reason: a bundling struct here would exist
// solely to satisfy the lint, not to express a real relationship between
// the fields).
#[allow(clippy::too_many_arguments)]
pub(crate) fn operator_loop(
    index: usize,
    id: &str,
    build: &OperatorBuild<'_>,
    config: &BTreeMap<String, Parameter>,
    inbox: &OperatorInbox,
    forwarder: &Forwarder<'_>,
    restart: RestartConfig,
) -> OperatorOutcome {
    let mut state = RestartState::new();
    let mut failures: Vec<OperatorFailure> = Vec::new();

    loop {
        let incarnation = state.incarnation;
        let mut operator = match build_and_configure(build, config) {
            Ok(operator) => operator,
            Err((hook, message, panicked)) => {
                record_failure(&mut failures, id, hook, message, panicked, incarnation);
                match give_up_or_restart(&mut state, &restart, false, id) {
                    Decision::Restart(delay) => {
                        std::thread::sleep(delay);
                        continue;
                    }
                    Decision::GiveUp { budget_exhausted } => {
                        forwarder.close_outputs(
                            index,
                            RouteCloseReason::ProducerCrashed {
                                generation: u64::from(incarnation),
                            },
                        );
                        inbox.close();
                        return OperatorOutcome::Failed {
                            failures,
                            budget_exhausted,
                        };
                    }
                }
            }
        };

        let mut out = OpOutput::new();
        let ending = match run_hook(|| operator.on_start(&mut out)) {
            Err((message, panicked)) => Ending::Failed {
                message,
                panicked,
                hook: FailedHook::OnStart,
            },
            Ok(()) => {
                forwarder.forward(index, out.drain());
                run_main_loop(&mut *operator, inbox, forwarder, index, &mut out)
            }
        };

        match ending {
            Ending::Failed {
                message,
                panicked,
                hook,
            } => {
                record_failure(&mut failures, id, hook, message, panicked, incarnation);
                match give_up_or_restart(&mut state, &restart, false, id) {
                    Decision::Restart(delay) => {
                        std::thread::sleep(delay);
                        continue;
                    }
                    Decision::GiveUp { budget_exhausted } => {
                        forwarder.close_outputs(
                            index,
                            RouteCloseReason::ProducerCrashed {
                                generation: u64::from(incarnation),
                            },
                        );
                        inbox.close();
                        return OperatorOutcome::Failed {
                            failures,
                            budget_exhausted,
                        };
                    }
                }
            }
            Ending::HostStopping => {
                finalize_on_stop(
                    &mut *operator,
                    &mut out,
                    forwarder,
                    index,
                    id,
                    incarnation,
                    &mut failures,
                );
                forwarder.close_outputs(index, RouteCloseReason::ProducerFinished);
                inbox.close();
                return OperatorOutcome::Finished {
                    restarts: incarnation,
                };
            }
            Ending::InboxClosed | Ending::StatusFinished => {
                finalize_on_stop(
                    &mut *operator,
                    &mut out,
                    forwarder,
                    index,
                    id,
                    incarnation,
                    &mut failures,
                );
                match give_up_or_restart(&mut state, &restart, true, id) {
                    Decision::Restart(delay) => {
                        std::thread::sleep(delay);
                        continue;
                    }
                    Decision::GiveUp { .. } => {
                        forwarder.close_outputs(index, RouteCloseReason::ProducerFinished);
                        inbox.close();
                        return OperatorOutcome::Finished {
                            restarts: state.incarnation,
                        };
                    }
                }
            }
        }
    }
}

/// The main per-incarnation loop: reads events from `inbox` and drives
/// `operator` until it either ends (see [`Ending`]) or fails.
fn run_main_loop(
    operator: &mut dyn Operator,
    inbox: &OperatorInbox,
    forwarder: &Forwarder<'_>,
    index: usize,
    out: &mut OpOutput,
) -> Ending {
    loop {
        let Some(event) = inbox.recv_blocking(None) else {
            return Ending::InboxClosed;
        };

        if matches!(event, OpEvent::Reload) {
            match run_hook(|| operator.on_reload(&mut *out)) {
                Ok(()) => {
                    forwarder.forward(index, out.drain());
                    continue;
                }
                Err((message, panicked)) => {
                    return Ending::Failed {
                        message,
                        panicked,
                        hook: FailedHook::OnReload,
                    };
                }
            }
        }

        let is_stop = matches!(event, OpEvent::Stop { .. });
        match run_hook(|| operator.on_event(&event, &mut *out)) {
            Ok(status) => {
                forwarder.forward(index, out.drain());
                if is_stop {
                    return Ending::HostStopping;
                }
                if status.is_finished() {
                    return Ending::StatusFinished;
                }
            }
            Err((message, panicked)) => {
                return Ending::Failed {
                    message,
                    panicked,
                    hook: FailedHook::OnEvent,
                };
            }
        }
    }
}

/// Runs `on_stop` exactly once, forwarding whatever it flushed or recording
/// its failure — never restarted on its own, per this module's docs.
#[allow(clippy::too_many_arguments)]
fn finalize_on_stop(
    operator: &mut dyn Operator,
    out: &mut OpOutput,
    forwarder: &Forwarder<'_>,
    index: usize,
    id: &str,
    incarnation: u32,
    failures: &mut Vec<OperatorFailure>,
) {
    match run_hook(|| operator.on_stop(&mut *out)) {
        Ok(()) => forwarder.forward(index, out.drain()),
        Err((message, panicked)) => {
            tracing::warn!(operator = id, incarnation, panicked, %message, "operator on_stop failed");
            failures.push(OperatorFailure {
                hook: FailedHook::OnStop,
                message,
                panicked,
                incarnation,
            });
        }
    }
}

/// Records a failure and logs it, then asks [`RestartState::decide`] what
/// happens next.
fn record_failure(
    failures: &mut Vec<OperatorFailure>,
    id: &str,
    hook: FailedHook,
    message: String,
    panicked: bool,
    incarnation: u32,
) {
    tracing::error!(operator = id, incarnation, panicked, hook = %hook, %message, "operator hook failed");
    failures.push(OperatorFailure {
        hook,
        message,
        panicked,
        incarnation,
    });
}

/// [`RestartState::decide`], logging the outcome.
fn give_up_or_restart(
    state: &mut RestartState,
    restart: &RestartConfig,
    success: bool,
    id: &str,
) -> Decision {
    let decision = state.decide(restart, success);
    match &decision {
        Decision::Restart(delay) => {
            tracing::warn!(
                operator = id,
                incarnation = state.incarnation,
                delay_ms = delay.as_millis() as u64,
                "restarting operator"
            );
        }
        Decision::GiveUp { budget_exhausted } => {
            tracing::error!(
                operator = id,
                budget_exhausted,
                "operator will not be restarted; siblings unaffected"
            );
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::output_sink::OutputSink;
    use crate::routing::Routing;
    use astrs_manifest::OperatorConfig;
    use astrs_node_api::testing::TestHarness;
    use astrs_operator_api::{
        OpOutput as OperatorApiOutput, OpResult as OperatorApiResult, OperatorRegistry, Status,
        register_operator,
    };
    use astrs_wire::{
        DataId, DurationMs, Metadata, PortRef, QueuePolicy, RestartPolicy, StopCause,
    };
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Echo;
    impl Operator for Echo {
        fn on_event(
            &mut self,
            event: &OpEvent,
            out: &mut OperatorApiOutput,
        ) -> OperatorApiResult<Status> {
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
    struct AlwaysPanics;
    impl Operator for AlwaysPanics {
        fn on_event(
            &mut self,
            _event: &OpEvent,
            _out: &mut OperatorApiOutput,
        ) -> OperatorApiResult<Status> {
            panic!("boom");
        }
    }

    /// Echoes its `configure`d threshold back as the payload of every
    /// input it sees, and refuses any config map without one — the
    /// fixture [`operator_loop`]'s `configure`-wiring tests drive.
    #[derive(Default)]
    struct Configurable {
        threshold: i64,
    }
    impl Operator for Configurable {
        fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OperatorApiResult<()> {
            match config.get("threshold") {
                Some(Parameter::Integer(value)) => {
                    self.threshold = *value;
                    Ok(())
                }
                _ => Err(astrs_operator_api::OpError::failed("missing threshold")),
            }
        }

        fn on_event(
            &mut self,
            event: &OpEvent,
            out: &mut OperatorApiOutput,
        ) -> OperatorApiResult<Status> {
            match event {
                OpEvent::Input { metadata, .. } => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    out.send_bytes("out", metadata.clone(), vec![self.threshold as u8])?;
                    Ok(Status::Continue)
                }
                OpEvent::Stop { .. } => Ok(Status::Finished),
                _ => Ok(Status::Continue),
            }
        }
    }

    fn registry() -> OperatorRegistry {
        OperatorRegistry::from_entries([
            register_operator!(Echo),
            register_operator!(AlwaysPanics),
            register_operator!(Configurable),
        ])
        .unwrap()
    }

    /// A routing table for one operator named `id`, publishing `outputs`,
    /// resolved against a fresh [`TestHarness`]'s default node (which
    /// declares an output named `out`).
    fn single_operator_routing(id: &str, outputs: &[&str]) -> (TestHarness, Routing) {
        let harness = TestHarness::start().unwrap();
        let config = OperatorConfig {
            id: id.to_owned(),
            operator: id.to_owned(),
            dylib: None,
            wasm: None,
            hub: None,
            inputs: BTreeMap::new(),
            outputs: outputs.iter().map(|s| (*s).to_owned()).collect(),
            config: BTreeMap::new(),
        };
        let routing = Routing::build(&[config], &harness.node).unwrap();
        (harness, routing)
    }

    fn producer_port() -> PortRef {
        PortRef::from_parts("camera", "image").unwrap()
    }

    /// A `build` closure that looks `name` up in a fresh registry
    /// containing every operator this module's tests define — the shape
    /// [`operator_loop`]'s `build` parameter needs, regardless of which
    /// concrete operator source (registry or `dylib:`) produced it in
    /// production code.
    fn registry_build(name: &'static str) -> Box<OperatorBuild<'static>> {
        let reg = registry();
        Box::new(move || reg.build(name))
    }

    #[test]
    fn a_clean_operator_with_no_stop_ends_when_its_inbox_closes() {
        let (mut harness, routing) = single_operator_routing("echo", &["out"]);
        let inbox = OperatorInbox::new();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        let inboxes: [OperatorInbox; 0] = [];
        let forwarder = Forwarder::new(&routing, &inboxes, &sink);

        inbox.close();
        let outcome = operator_loop(
            0,
            "echo",
            &registry_build("Echo"),
            &BTreeMap::new(),
            &inbox,
            &forwarder,
            RestartConfig::never(),
        );
        assert_eq!(outcome, OperatorOutcome::Finished { restarts: 0 });
    }

    #[test]
    fn a_stop_event_ends_the_operator_and_never_restarts_even_under_always() {
        let (mut harness, routing) = single_operator_routing("echo", &["out"]);
        let inbox = OperatorInbox::new();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        let inboxes: [OperatorInbox; 0] = [];
        let forwarder = Forwarder::new(&routing, &inboxes, &sink);

        inbox.push_stop(StopCause::Requested, None);
        let mut restart = RestartConfig::never();
        restart.policy = RestartPolicy::Always;
        let outcome = operator_loop(
            0,
            "echo",
            &registry_build("Echo"),
            &BTreeMap::new(),
            &inbox,
            &forwarder,
            restart,
        );
        assert_eq!(outcome, OperatorOutcome::Finished { restarts: 0 });
    }

    #[test]
    fn a_finished_operator_is_rebuilt_under_always_while_the_host_keeps_running() {
        let (mut harness, routing) = single_operator_routing("echo", &["out"]);
        let inbox = OperatorInbox::new();
        let id = DataId::new("in").unwrap();
        inbox
            .register_input(id.clone(), 4, QueuePolicy::DropOldest)
            .unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        let inboxes: [OperatorInbox; 0] = [];
        let forwarder = Forwarder::new(&routing, &inboxes, &sink);

        // `Echo` only returns `Status::Finished` on an `OpEvent::Stop`, so
        // close the inbox instead: `InboxClosed` is the ordinary-exit path
        // this test wants, distinct from a host-broadcast `Stop`.
        inbox.close();
        let mut restart = RestartConfig::never();
        restart.policy = RestartPolicy::Always;
        restart.max_restarts = Some(1);
        restart.restart_delay = DurationMs::ZERO;
        restart.max_restart_delay = DurationMs::ZERO;
        let outcome = operator_loop(
            0,
            "echo",
            &registry_build("Echo"),
            &BTreeMap::new(),
            &inbox,
            &forwarder,
            restart,
        );
        // One rebuild permitted, then the (still-closed) inbox ends the
        // second incarnation too, and the budget is spent.
        assert_eq!(outcome, OperatorOutcome::Finished { restarts: 1 });
    }

    #[test]
    fn a_panicking_operator_restarts_under_on_failure_then_gives_up_at_the_budget() {
        let (mut harness, routing) = single_operator_routing("panics", &[]);
        let inbox = OperatorInbox::new();
        let id = DataId::new("in").unwrap();
        inbox
            .register_input(id.clone(), 4, QueuePolicy::DropOldest)
            .unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        let inboxes: [OperatorInbox; 0] = [];
        let forwarder = Forwarder::new(&routing, &inboxes, &sink);

        // One message queued up front: every incarnation panics on it
        // immediately, so this drives the restart loop deterministically
        // without a second thread feeding the inbox live.
        for _ in 0..5 {
            inbox
                .push_message(&id, producer_port(), Metadata::default(), vec![1])
                .unwrap();
        }
        let mut restart = RestartConfig::never();
        restart.policy = RestartPolicy::OnFailure;
        restart.max_restarts = Some(2);
        restart.restart_delay = DurationMs::ZERO;
        restart.max_restart_delay = DurationMs::ZERO;

        let outcome = operator_loop(
            0,
            "panics",
            &registry_build("AlwaysPanics"),
            &BTreeMap::new(),
            &inbox,
            &forwarder,
            restart,
        );
        match outcome {
            OperatorOutcome::Failed {
                failures,
                budget_exhausted,
            } => {
                assert!(budget_exhausted);
                // 1 initial attempt + 2 permitted restarts = 3 failures.
                assert_eq!(failures.len(), 3);
                assert!(failures.iter().all(|f| f.panicked));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn on_failure_never_restarts_a_clean_finish() {
        let (mut harness, routing) = single_operator_routing("echo", &["out"]);
        let inbox = OperatorInbox::new();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        let inboxes: [OperatorInbox; 0] = [];
        let forwarder = Forwarder::new(&routing, &inboxes, &sink);

        inbox.close();
        let mut restart = RestartConfig::never();
        restart.policy = RestartPolicy::OnFailure;
        let outcome = operator_loop(
            0,
            "echo",
            &registry_build("Echo"),
            &BTreeMap::new(),
            &inbox,
            &forwarder,
            restart,
        );
        assert_eq!(outcome, OperatorOutcome::Finished { restarts: 0 });
    }

    #[test]
    fn configure_runs_before_on_start_and_reaches_the_operator() {
        let (mut harness, routing) = single_operator_routing("configurable", &["out"]);
        let inbox = OperatorInbox::new();
        let id = DataId::new("in").unwrap();
        inbox
            .register_input(id.clone(), 4, QueuePolicy::DropOldest)
            .unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        let inboxes: [OperatorInbox; 0] = [];
        let forwarder = Forwarder::new(&routing, &inboxes, &sink);

        inbox
            .push_message(&id, producer_port(), Metadata::default(), vec![0])
            .unwrap();
        inbox.push_stop(StopCause::Requested, None);

        let mut config = BTreeMap::new();
        config.insert("threshold".to_owned(), Parameter::Integer(9));
        let outcome = operator_loop(
            0,
            "configurable",
            &registry_build("Configurable"),
            &config,
            &inbox,
            &forwarder,
            RestartConfig::never(),
        );
        assert_eq!(
            outcome,
            OperatorOutcome::Finished { restarts: 0 },
            "{outcome:?}"
        );
    }

    #[test]
    fn a_configure_failure_is_reported_as_a_configure_hook_failure_and_honors_restart_policy() {
        let (mut harness, routing) = single_operator_routing("configurable", &["out"]);
        let inbox = OperatorInbox::new();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        let inboxes: [OperatorInbox; 0] = [];
        let forwarder = Forwarder::new(&routing, &inboxes, &sink);

        // No `threshold` key: `Configurable::configure` refuses every
        // incarnation, so this drives the restart loop deterministically
        // without ever reaching `on_start`.
        let mut restart = RestartConfig::never();
        restart.policy = RestartPolicy::OnFailure;
        restart.max_restarts = Some(1);
        restart.restart_delay = DurationMs::ZERO;
        restart.max_restart_delay = DurationMs::ZERO;

        let outcome = operator_loop(
            0,
            "configurable",
            &registry_build("Configurable"),
            &BTreeMap::new(),
            &inbox,
            &forwarder,
            restart,
        );
        match outcome {
            OperatorOutcome::Failed {
                failures,
                budget_exhausted,
            } => {
                assert!(budget_exhausted);
                // 1 initial attempt + 1 permitted restart = 2 failures,
                // every one of them tagged as the `configure` hook, never
                // `on_start` or `on_event` (neither ever ran).
                assert_eq!(failures.len(), 2);
                assert!(failures.iter().all(|f| f.hook == FailedHook::Configure));
                assert!(failures.iter().all(|f| !f.panicked));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*payload), "boom");
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("also boom"));
        assert_eq!(panic_message(&*payload), "also boom");
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_i32);
        assert!(panic_message(&*payload).contains("non-string"));
    }
}
