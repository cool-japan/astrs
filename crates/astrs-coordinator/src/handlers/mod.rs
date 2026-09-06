//! Dispatching one [`ControlRequest`] to its handler (blueprint §7.3, §16,
//! §22).
//!
//! [`dispatch`] is the coordinator's entire public control-plane surface:
//! every verb a CLI session can send ends up here, and every branch below
//! answers with exactly one [`ControlReply`] — never a panic, never a
//! silent drop. A verb this coordinator does not yet implement
//! server-side still answers, with a typed
//! [`astrs_wire::ErrorCode::Unsupported`] (see [`topology`] and
//! [`logs::topic_publish`]), so a caller can always tell "not yet" from
//! "hung". It is also where a token's scope is enforced (blueprint §16:
//! "the coordinator API distinguishes read verbs... from mutating verbs"):
//! a mutating [`ControlRequest`] on a read-scope connection answers
//! [`astrs_wire::ErrorCode::PermissionDenied`] without ever reaching the
//! handler that would have performed it — see [`dispatch`]'s own docs.

pub mod info;
pub mod lifecycle;
pub mod logs;
pub mod misc;
pub mod params;
pub mod topology;

use astrs_wire::{
    ControlReply, ControlRequest, CoordinatorEvent, ErrorCode, ParamKey, ParamScope, Parameter,
    RequestScope, SpanStatus, WireMessage,
};
use tokio::sync::mpsc;
use tracing::Instrument as _;

use crate::coordinator::Coordinator;
use crate::session::CliOutbound;
use crate::trace::control_span;

/// Dispatches one [`ControlRequest`] against `coordinator`, on behalf of
/// the CLI session that owns `outbound` (its subscription push channel —
/// only read by `LogSubscribe`/`TopicSubscribe`), which is granted `scope`
/// (blueprint §16, §22; see [`crate::auth`] for how a presented token was
/// classified into one).
///
/// [`ControlRequest::Hello`] never reaches this function: the handshake is
/// consumed before a session starts reading ordinary requests (blueprint
/// §7.2). Handing one here would be a caller bug, so it answers a
/// diagnostic [`astrs_wire::ErrorCode::InvalidArgument`] rather than
/// panicking.
///
/// # Token scopes (§16, §22)
///
/// [`ControlRequest::is_mutating`] on a [`RequestScope::Read`] connection
/// answers [`astrs_wire::ErrorCode::PermissionDenied`] immediately —
/// `dispatch_inner`, and therefore every handler function below it, never
/// sees the request at all. [`RequestScope::Mutate`] changes nothing here:
/// it is what every token minted before 0.2 classifies as (blueprint §16
/// back-compat), so this is the same behaviour dispatch has always had.
/// Exercised end to end by this module's own
/// `tests::a_read_scope_session_is_denied_a_mutating_verb_before_it_has_any_effect`
/// and `tests::every_verb_is_denied_on_read_scope_iff_control_request_says_it_mutates`
/// — `dispatch` is not part of the crate's public surface (this module is
/// private), so it cannot carry a runnable doctest of its own.
///
/// Every dispatched request becomes one [`astrs_wire::TraceSpan`] in
/// [`Coordinator::traces`] before this returns, unconditionally — see
/// [`crate::trace`] for what that buys `GetTraces` and what it
/// deliberately does not claim. `dispatch_inner` also runs inside a real
/// `tracing::Span` covering exactly the same request; with no `tracing`
/// subscriber installed (every test in this crate that opens a bare
/// [`Coordinator`] directly) that span is a zero-cost no-op and changes
/// nothing observable here. In a real process that *did* install one —
/// `astrs-telemetry`'s, with its span layer's sink wired to this same
/// `Coordinator`'s buffer via [`Coordinator::trace_sink`] (see
/// `bins/astrs-cli::command::serve::coordinator`) — the request reaches
/// `GetTraces` twice: once as this function's own hand-built entry
/// (`control_span`, with an exact per-request `dataflow` tag `TraceBuffer`
/// can filter on) and once more as astrs-telemetry's own span (untagged —
/// `astrs-telemetry`'s `AstrsSpanLayer` stamps one
/// fixed `dataflow` for its whole process, correctly `None` for a
/// coordinator that is not itself scoped to one dataflow). That overlap is
/// the honest cost of composing a proven, always-on mechanism with a newly
/// wired, genuinely astrs-telemetry-sourced one rather than choosing
/// between them; a real, dedicated tracing UI (out of scope here — see
/// [`crate::trace`]) is where deduplicating the two would belong.
pub async fn dispatch(
    coordinator: &Coordinator,
    outbound: &mpsc::Sender<CliOutbound>,
    request: ControlRequest,
    scope: RequestScope,
) -> ControlReply {
    let span_name = request.variant_name();
    let span_dataflow = request.dataflow();
    let tracing_span = tracing::info_span!(
        "control_request",
        verb = span_name,
        dataflow = ?span_dataflow,
        error = tracing::field::Empty,
    );
    let span_start = coordinator.clock.now();
    // Blueprint §16, §22: a read-scope token may observe but not change
    // cluster state. `ControlRequest::scope` is the exhaustive classification
    // (its own doc comment explains why a verb added later cannot compile
    // until it too is classified); denying here, before `dispatch_inner` is
    // ever reached, means a read-scope session cannot even *attempt* a side
    // effect — the mutating handler body never runs — rather than relying on
    // every handler to check for itself. The denial still becomes a
    // `PermissionDenied` `TraceSpan` entry below like any other reply, so
    // `astrs trace`/`GetTraces` shows a denied attempt as loudly as a
    // successful one.
    let reply = if request.is_mutating() && !scope.is_mutating() {
        tracing::warn!(
            verb = span_name,
            ?scope,
            "mutating verb denied: this connection holds a read-scope token (§16)"
        );
        ControlReply::error(
            ErrorCode::PermissionDenied,
            format!(
                "'{span_name}' requires a mutate-scope token; this connection holds a \
                 read-scope one (§16)"
            ),
        )
    } else {
        dispatch_replicated(coordinator, outbound, request)
            .instrument(tracing_span.clone())
            .await
    };
    let span_status = if reply.error_code().is_none() {
        SpanStatus::Ok
    } else {
        SpanStatus::Error
    };
    tracing_span.record("error", span_status == SpanStatus::Error);
    coordinator.traces().push(control_span(
        span_name,
        span_dataflow,
        coordinator.next_request_id(),
        span_start,
        coordinator.clock.now(),
        span_status,
    ));
    reply
}

/// The high-availability gate (blueprint §22), between the token-scope check
/// and the per-verb routing.
///
/// Without the `ha` feature this is [`dispatch_inner`] and nothing else; the
/// build that ships by default has no consensus code in it at all.
#[cfg(not(feature = "ha"))]
async fn dispatch_replicated(
    coordinator: &Coordinator,
    outbound: &mpsc::Sender<CliOutbound>,
    request: ControlRequest,
) -> ControlReply {
    dispatch_inner(coordinator, outbound, request).await
}

/// The high-availability gate (blueprint §22).
///
/// Three outcomes, in order:
///
/// 1. **Not a replicated coordinator, or a read verb** — straight through.
///    Reads are served from the local store under the leader lease; see
///    [`crate::ha`] for why a lease rather than `ReadIndex`.
/// 2. **A mutating verb on a coordinator that is not ready to lead** — the
///    existing structured error, carrying a `leader: <address>` hint. Note
///    the gate is [`crate::ha::HaHandle::is_ready`], not "am I the leader": a
///    freshly elected leader has not applied its own no-op yet, and a
///    mutation computed from what it can currently see would be computed from
///    pre-failover state.
/// 3. **A mutating verb on the ready leader** — replicated, by one of the two
///    paths [`crate::ha`] documents.
#[cfg(feature = "ha")]
async fn dispatch_replicated(
    coordinator: &Coordinator,
    outbound: &mpsc::Sender<CliOutbound>,
    request: ControlRequest,
) -> ControlReply {
    let Some(ha) = coordinator.ha() else {
        return dispatch_inner(coordinator, outbound, request).await;
    };
    if !request.is_mutating() {
        return dispatch_inner(coordinator, outbound, request).await;
    }
    if !ha.is_ready() {
        return ha.not_leader_reply(request.variant_name());
    }

    // Parameters: propose first, apply only what committed.
    if let Some(reply) = crate::ha::params::route(coordinator, &ha, &request).await {
        return reply;
    }

    // Everything else: run on the leader, then replicate exactly the registry
    // writes it produced. The lock spans execute-capture-propose so two
    // concurrent requests can never be attributed each other's writes.
    let _guard = ha.mutation_lock().await;
    let before = match coordinator.store.last_seq().await {
        Ok(seq) => seq,
        Err(err) => return crate::error::CoordinatorError::from(err).into_reply(),
    };
    let reply = dispatch_inner(coordinator, outbound, request).await;
    if let Err(err) = ha.replicate_since(&coordinator.store, before).await {
        tracing::error!(%err, "a registry mutation could not be replicated");
        return err.into_reply();
    }
    reply
}

/// The actual per-verb routing [`dispatch`] wraps with span recording.
async fn dispatch_inner(
    coordinator: &Coordinator,
    outbound: &mpsc::Sender<CliOutbound>,
    request: ControlRequest,
) -> ControlReply {
    match request {
        ControlRequest::Hello(_) => crate::error::CoordinatorError::invalid(
            "Hello may only be the first frame on a connection",
        )
        .into_reply(),
        ControlRequest::Build {
            manifest,
            working_dir,
            name,
            force,
        } => lifecycle::build(coordinator, manifest, working_dir, name, force).await,
        ControlRequest::WaitForBuild { build, timeout } => {
            lifecycle::wait_for_build(coordinator, build, timeout).await
        }
        ControlRequest::Start {
            source,
            name,
            detach,
        } => lifecycle::start(coordinator, source, name, detach).await,
        ControlRequest::WaitForSpawn { dataflow, timeout } => {
            lifecycle::wait_for_spawn(coordinator, dataflow, timeout).await
        }
        ControlRequest::Check { dataflow } => info::check(coordinator, dataflow).await,
        ControlRequest::Stop { dataflow, grace } => {
            lifecycle::stop(coordinator, dataflow, grace).await
        }
        ControlRequest::StopByName { name, grace } => {
            lifecycle::stop_by_name(coordinator, name, grace).await
        }
        ControlRequest::Restart { dataflow, rebuild } => {
            lifecycle::restart(coordinator, dataflow, rebuild).await
        }
        ControlRequest::RestartByName { name, rebuild } => {
            lifecycle::restart_by_name(coordinator, name, rebuild).await
        }
        ControlRequest::Logs {
            dataflow,
            node,
            query,
        } => logs::logs(coordinator, dataflow, node, query).await,
        ControlRequest::LogSubscribe {
            dataflow,
            node,
            query,
            subscription,
        } => logs::log_subscribe(coordinator, outbound, dataflow, node, query, subscription).await,
        ControlRequest::List { all } => info::list(coordinator, all).await,
        ControlRequest::Info {
            dataflow,
            include_nodes,
        } => info::info(coordinator, dataflow, include_nodes).await,
        ControlRequest::Destroy { force } => lifecycle::destroy(coordinator, force).await,
        ControlRequest::Clean {
            dataflow,
            artifacts,
            logs,
        } => lifecycle::clean(coordinator, dataflow, artifacts, logs).await,
        ControlRequest::ConnectedDaemons {
            include_unreachable,
        } => info::connected_daemons(coordinator, include_unreachable).await,
        ControlRequest::GetNodeInfo { dataflow, node } => {
            info::get_node_info(coordinator, dataflow, node).await
        }
        ControlRequest::TopicSubscribe {
            dataflow,
            port,
            query,
            subscription,
        } => {
            logs::topic_subscribe(coordinator, outbound, dataflow, port, query, subscription).await
        }
        ControlRequest::TopicUnsubscribe { subscription } => {
            logs::topic_unsubscribe(coordinator, subscription).await
        }
        ControlRequest::TopicPublish {
            dataflow,
            port,
            metadata,
            payload,
        } => logs::topic_publish(coordinator, dataflow, port, metadata, payload).await,
        ControlRequest::GetParams {
            scope,
            prefix,
            inherited,
        } => params::get_params(coordinator, scope, prefix, inherited).await,
        ControlRequest::GetParam {
            scope,
            key,
            inherited,
        } => params::get_param(coordinator, scope, key, inherited).await,
        ControlRequest::SetParam {
            scope,
            key,
            value,
            create_only,
        } => params::set_param(coordinator, scope, key, value, create_only).await,
        ControlRequest::DeleteParam { scope, key } => {
            params::delete_param(coordinator, scope, key).await
        }
        ControlRequest::RestartNode { dataflow, node } => {
            topology::restart_node(coordinator, dataflow, node).await
        }
        ControlRequest::StopNode {
            dataflow,
            node,
            grace,
        } => topology::stop_node(coordinator, dataflow, node, grace).await,
        ControlRequest::AddNode {
            dataflow,
            node,
            start,
        } => topology::add_node(coordinator, dataflow, node, start).await,
        ControlRequest::RemoveNode {
            dataflow,
            node,
            grace,
        } => topology::remove_node(coordinator, dataflow, node, grace).await,
        ControlRequest::ReplaceNode {
            dataflow,
            node,
            drain,
        } => topology::replace_node(coordinator, dataflow, node, drain).await,
        ControlRequest::AddEdge {
            dataflow,
            consumer,
            input,
        } => topology::add_edge(coordinator, dataflow, consumer, input).await,
        ControlRequest::RemoveEdge {
            dataflow,
            consumer,
            input,
        } => topology::remove_edge(coordinator, dataflow, consumer, input).await,
        ControlRequest::RecordStart {
            dataflow,
            path,
            ports,
            overwrite,
        } => misc::record_start(coordinator, dataflow, path, ports, overwrite).await,
        ControlRequest::RecordStop { dataflow } => misc::record_stop(coordinator, dataflow).await,
        ControlRequest::GetTraces {
            dataflow,
            node,
            since,
            limit,
        } => misc::get_traces(coordinator, dataflow, node, since, limit).await,
        ControlRequest::GetNodeMetrics { dataflow, node } => {
            info::get_node_metrics(coordinator, dataflow, node).await
        }
        ControlRequest::GetNodeIoMetrics { dataflow, node } => {
            info::get_node_io_metrics(coordinator, dataflow, node).await
        }
        ControlRequest::GetManifest { dataflow } => info::get_manifest(coordinator, dataflow).await,
        // `ControlRequest` is `#[non_exhaustive]` (blueprint principle 4:
        // append-only wire enums); a verb added in a later protocol
        // revision has no handler yet, so it answers `Unsupported` rather
        // than failing to compile against every coordinator already
        // built against the frozen §24.1 set.
        _ => crate::error::CoordinatorError::NotYetSupported(
            "unknown verb",
            "this coordinator build does not recognise this request",
        )
        .into_reply(),
    }
}

/// Propagates a parameter write/delete to whichever daemons need to hear
/// about it, so the nodes that read it get a fresh
/// `NodeEvent::ParamUpdate`/`ParamDeleted` (forwarding from
/// `CoordinatorEvent` down to the actual node process is `astrs-daemon`'s
/// job — this coordinator's responsibility ends at getting the change to
/// the right daemon(s)):
///
/// - [`ParamScope::Global`] reaches every connected daemon — a
///   cluster-wide default is relevant to all of them.
/// - [`ParamScope::Dataflow`] reaches every daemon currently hosting one of
///   that dataflow's nodes.
/// - [`ParamScope::Node`] reaches only the one daemon hosting that node.
pub(crate) async fn dispatch_param_update(
    coordinator: &Coordinator,
    scope: &ParamScope,
    key: &ParamKey,
    value: Option<Parameter>,
) {
    let targets: Vec<astrs_wire::DaemonId> = match scope {
        ParamScope::Global => coordinator.daemons().ids().cloned().collect(),
        ParamScope::Dataflow { dataflow } => coordinator
            .dataflows()
            .get(*dataflow)
            .map(|live| live.hosting_daemons().into_iter().collect())
            .unwrap_or_default(),
        ParamScope::Node { dataflow, node } => coordinator
            .dataflows()
            .get(*dataflow)
            .and_then(|live| live.daemon_for_node(node).cloned())
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    if targets.is_empty() {
        return;
    }
    let event = match value {
        Some(value) => CoordinatorEvent::SetParam {
            scope: scope.clone(),
            key: key.clone(),
            value,
        },
        None => CoordinatorEvent::DeleteParam {
            scope: scope.clone(),
            key: key.clone(),
        },
    };
    let daemons = coordinator.daemons();
    for id in &targets {
        if let Some(handle) = daemons.get(id) {
            let _ = handle.send(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::registry::DaemonHandle;
    use astrs_wire::{AuthToken, DataflowId, SessionId};

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([10; 32])).with_port(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn hello_reaching_dispatch_is_a_clear_caller_error() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(1);
        let hello = ControlRequest::Hello(astrs_wire::Hello::new(
            astrs_wire::Role::Cli,
            AuthToken::ZERO,
        ));
        let reply = dispatch(&coordinator, &tx, hello, RequestScope::Mutate).await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::InvalidArgument)
        );
    }

    #[tokio::test]
    async fn dispatch_routes_a_representative_verb_from_each_family() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(1);
        let reply = dispatch(
            &coordinator,
            &tx,
            ControlRequest::List { all: true },
            RequestScope::Mutate,
        )
        .await;
        assert!(matches!(reply, ControlReply::DataflowList { .. }));

        let reply = dispatch(
            &coordinator,
            &tx,
            ControlRequest::GetParam {
                scope: ParamScope::Global,
                key: ParamKey::new("x").unwrap(),
                inherited: false,
            },
            RequestScope::Mutate,
        )
        .await;
        assert!(matches!(
            reply,
            ControlReply::ParamValue { value: None, .. }
        ));
    }

    #[tokio::test]
    async fn a_read_scope_session_completes_a_read_verb() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(1);
        let reply = dispatch(
            &coordinator,
            &tx,
            ControlRequest::List { all: true },
            RequestScope::Read,
        )
        .await;
        assert!(matches!(reply, ControlReply::DataflowList { .. }));
    }

    #[tokio::test]
    async fn a_read_scope_session_is_denied_a_mutating_verb_before_it_has_any_effect() {
        // Mirrors `set_param_propagates_to_every_daemon_at_global_scope`
        // below: the denial must happen *before* `dispatch_inner` ever runs,
        // so the daemon that would otherwise hear the propagated `SetParam`
        // never does — proof the read-scope session could not so much as
        // attempt the mutation, not merely that its final reply looks like a
        // failure.
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(1);
        let daemon_id = astrs_wire::DaemonId::generate(None);
        let (daemon_tx, mut daemon_rx) = mpsc::channel(4);
        coordinator.daemons().insert(DaemonHandle::new(
            daemon_id,
            None,
            SessionId::generate(),
            daemon_tx,
            astrs_time::HlcTimestamp::EPOCH,
        ));

        let reply = dispatch(
            &coordinator,
            &tx,
            ControlRequest::SetParam {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Integer(1),
                create_only: false,
            },
            RequestScope::Read,
        )
        .await;

        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::PermissionDenied)
        );
        assert!(
            daemon_rx.try_recv().is_err(),
            "a denied SetParam must never reach dispatch_inner, so no daemon hears about it"
        );
    }

    /// The exhaustive-match guard for token-scope enforcement: for every
    /// verb this build's `ControlRequest` knows about — the same sample
    /// table `astrs-wire` checks its own `scope()` classification against —
    /// a read-scope session must be denied if and only if
    /// [`ControlRequest::is_mutating`] says so. A verb `dispatch` forgot to
    /// account for would show up here as a mismatch against
    /// `ControlRequest::scope`'s own (compile-time-exhaustive, per its doc
    /// comment) answer, never as a silent gap that only a manual audit would
    /// catch.
    #[tokio::test]
    async fn every_verb_is_denied_on_read_scope_iff_control_request_says_it_mutates() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(4);
        let samples = astrs_wire::messages::samples::control_requests().expect("the sample table");
        // Pinned to the exact count, not a loose lower bound: a verb added to
        // `ControlRequest` without a matching sample-table entry must fail
        // *this* assertion, not silently skip the scope check below for it.
        assert_eq!(
            samples.len(),
            ControlRequest::VARIANT_NAMES.len(),
            "the sample table must cover every verb, including any added since this test was \
             last touched"
        );

        for sample in samples {
            if sample.is_handshake() {
                continue; // `Hello` never reaches `dispatch` (see its own doc comment).
            }
            let expect_denied = sample.is_mutating();
            let reply = dispatch(&coordinator, &tx, sample.clone(), RequestScope::Read).await;
            let was_denied = reply.error_code() == Some(astrs_wire::ErrorCode::PermissionDenied);
            assert_eq!(
                was_denied,
                expect_denied,
                "{} is classified {:?} but was {}",
                sample.variant_name(),
                sample.scope(),
                if was_denied { "denied" } else { "not denied" },
            );
        }
    }

    /// `every_verb_is_denied_on_read_scope_iff_control_request_says_it_mutates`
    /// above proves every read verb's reply is not `PermissionDenied` — as of
    /// this writing that is the same thing as "reached `dispatch_inner`",
    /// because this function's own scope gate is the only place in this
    /// crate that produces that code today (the `ha` gate's own refusal,
    /// [`crate::ha::HaHandle::not_leader_reply`], answers
    /// [`ErrorCode::Unavailable`] instead, so it does not muddy this). That
    /// is an invariant this test relies on, not one the compiler enforces,
    /// so it is worth restating rather than assuming. What the sweep does
    /// not show on its own is that `Logs`/`Info` — the two verbs §16 names
    /// alongside `List` — come back with their genuine successful payload
    /// rather than an unrelated domain error a bare coordinator happens to
    /// produce for them (`NotFound`, in particular, is also never
    /// `PermissionDenied`, so it would pass that sweep too). This test seeds
    /// real state so both are demonstrated positively instead.
    #[tokio::test]
    async fn a_read_scope_session_gets_the_real_reply_for_logs_and_info_not_merely_a_non_denial() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(1);
        let dataflow = DataflowId::generate();
        let manifest =
            astrs_manifest::Manifest::from_yaml_str("nodes:\n  - id: a\n    path: ./a\n").unwrap();

        // `Info` reads the durable store's snapshot, not the in-memory
        // registry `SetParam`'s tests populate.
        let manifest_json = serde_json::to_string(&manifest).unwrap();
        coordinator
            .store
            .upsert_dataflow(dataflow, Some("demo".to_owned()), manifest_json, 1)
            .await
            .unwrap();

        let reply = dispatch(
            &coordinator,
            &tx,
            ControlRequest::Info {
                dataflow,
                include_nodes: false,
            },
            RequestScope::Read,
        )
        .await;
        assert!(
            matches!(&reply, ControlReply::DataflowList { dataflows, .. } if dataflows.len() == 1),
            "a read-scope Info must return the real snapshot, not just avoid denial: {reply:?}"
        );

        // `Logs` reads the in-memory registry; with no hosting daemon it
        // resolves immediately to a real (if empty) successful batch.
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        let live = crate::registry::LiveDataflow::new(dataflow, None, manifest, None, graph);
        coordinator.dataflows().insert(live);

        let reply = dispatch(
            &coordinator,
            &tx,
            ControlRequest::Logs {
                dataflow,
                node: None,
                query: astrs_wire::LogQuery::new(),
            },
            RequestScope::Read,
        )
        .await;
        assert!(
            matches!(&reply, ControlReply::Logs { records, truncated } if records.is_empty() && !truncated),
            "a read-scope Logs must return the real (empty) batch, not just avoid denial: {reply:?}"
        );
    }

    #[tokio::test]
    async fn set_param_propagates_to_every_daemon_at_global_scope() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(1);
        let daemon_id = astrs_wire::DaemonId::generate(None);
        let (daemon_tx, mut daemon_rx) = mpsc::channel(4);
        coordinator.daemons().insert(DaemonHandle::new(
            daemon_id,
            None,
            SessionId::generate(),
            daemon_tx,
            astrs_time::HlcTimestamp::EPOCH,
        ));

        dispatch(
            &coordinator,
            &tx,
            ControlRequest::SetParam {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Integer(1),
                create_only: false,
            },
            RequestScope::Mutate,
        )
        .await;
        assert!(matches!(
            daemon_rx.try_recv().unwrap(),
            CoordinatorEvent::SetParam { .. }
        ));
    }

    #[tokio::test]
    async fn set_param_at_dataflow_scope_only_reaches_hosting_daemons() {
        let coordinator = coordinator();
        let (tx, _rx) = mpsc::channel(1);
        let hosting = astrs_wire::DaemonId::generate(None);
        let other = astrs_wire::DaemonId::generate(None);
        let (hosting_tx, mut hosting_rx) = mpsc::channel(4);
        let (other_tx, mut other_rx) = mpsc::channel(4);
        coordinator.daemons().insert(DaemonHandle::new(
            hosting.clone(),
            None,
            SessionId::generate(),
            hosting_tx,
            astrs_time::HlcTimestamp::EPOCH,
        ));
        coordinator.daemons().insert(DaemonHandle::new(
            other,
            None,
            SessionId::generate(),
            other_tx,
            astrs_time::HlcTimestamp::EPOCH,
        ));

        let dataflow = DataflowId::generate();
        let manifest =
            astrs_manifest::Manifest::from_yaml_str("nodes:\n  - id: a\n    path: ./a\n").unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        let mut live = crate::registry::LiveDataflow::new(dataflow, None, manifest, None, graph);
        live.placement
            .node_daemon
            .insert(astrs_wire::NodeId::new("a").unwrap(), hosting);
        coordinator.dataflows().insert(live);

        dispatch(
            &coordinator,
            &tx,
            ControlRequest::SetParam {
                scope: ParamScope::dataflow_scope(dataflow),
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Integer(1),
                create_only: false,
            },
            RequestScope::Mutate,
        )
        .await;
        assert!(hosting_rx.try_recv().is_ok());
        assert!(other_rx.try_recv().is_err());
    }

    /// `dispatch`'s own doc comment describes two independent producers of
    /// one `TraceBuffer` entry once a real subscriber is installed: this
    /// crate's always-on `control_span` bookkeeping, and a genuine
    /// `tracing` span reaching `astrs-telemetry`'s `AstrsSpanLayer`. This
    /// is the one test in this crate that installs that real subscriber
    /// (see `astrs_telemetry::subscriber::init_telemetry_with_spans`'s own
    /// doctest for the general shape) — safe here specifically because no
    /// other test in this binary ever does, so there is nothing else in
    /// the process racing to claim the one global slot `tracing` allows,
    /// under `cargo nextest`'s per-test processes or plain `cargo test`'s
    /// shared one alike.
    #[tokio::test]
    async fn a_real_tracing_subscriber_installed_on_this_coordinators_sink_also_reaches_traces() {
        let coordinator = coordinator();
        let config = astrs_telemetry::subscriber::TelemetryConfig::default();
        astrs_telemetry::subscriber::init_telemetry_with_spans(config, coordinator.trace_sink())
            .expect("the one subscriber this whole test binary installs");

        let (tx, _rx) = mpsc::channel(1);
        let reply = dispatch(
            &coordinator,
            &tx,
            ControlRequest::List { all: true },
            RequestScope::Mutate,
        )
        .await;
        assert!(matches!(reply, ControlReply::DataflowList { .. }));

        let spans = coordinator.traces().query(None, None, None, None).spans;
        assert!(
            spans.iter().any(|span| span.name == "List"),
            "control_span's own unconditional bookkeeping must still be there: {spans:?}"
        );
        assert!(
            spans.iter().any(|span| span.name == "control_request"),
            "a real tracing span, entered around the same dispatch and captured by \
             astrs-telemetry's span layer, must also have reached this coordinator's own \
             buffer through `Coordinator::trace_sink`: {spans:?}"
        );
    }
}
