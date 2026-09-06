//! `astrs node add/remove/replace/connect/disconnect` (blueprint §8, §17:
//! dynamic topology) — closing the loop
//! [`crate::command::stub`] used to leave as a scheduled-later verb.
//!
//! ```text
//!   astrs node add <flow> node.yml       ─► AddNode{node, start}     ─► Ok
//!   astrs node remove <flow> <id>        ─► RemoveNode{node, grace}  ─► Ok
//!   astrs node replace <flow> <id> f.yml ─► ReplaceNode{node, drain} ─► Ok
//!   astrs node connect <flow> <id> <in> <src> ─► AddEdge{..}         ─► Ok
//!   astrs node disconnect <flow> <id> <in>    ─► RemoveEdge{..}      ─► Ok
//! ```
//!
//! `add`/`replace` read a *single-node* manifest fragment from disk (§8.3's
//! per-node schema, with no `nodes:` wrapper) and expand it client-side
//! into an [`astrs_wire::NodeSpawnSpec`] via
//! [`astrs_coordinator::expand_node_fragment`]
//! — the coordinator itself never reads manifest text for a dynamic node
//! (see `astrs-coordinator`'s `handlers::topology` module docs); this is
//! the one piece of that expansion a caller has to do instead.
//!
//! Every verb here is a thin, uniform shape: build the typed request,
//! [`Client::request_ok`] it, print a one-line confirmation (or the `--json`
//! equivalent). None of the five needs to inspect a reply beyond "did the
//! coordinator accept it" — the interesting state (did the node actually
//! come up, is the edge actually delivering) is what `astrs status`/`astrs
//! top`/`astrs topic echo` are for.

use std::io::Write;
use std::path::Path;

use astrs_wire::{ControlRequest, DataId, InputSpec, NodeId, PortRef, QueuePolicy};

use crate::cli::QueuePolicyArg;
use crate::command::client::{Client, DataflowRef, Endpoint, runtime};
use crate::error::CliError;

/// Reads a node manifest fragment and expands it into a spawn
/// specification for `dataflow`, at `generation`.
///
/// # Errors
///
/// [`CliError::Io`] if `path` cannot be read. As
/// [`astrs_coordinator::expand_node_fragment`] otherwise (via
/// [`CliError::Coordinator`]).
fn read_node_fragment(
    path: &Path,
    dataflow: astrs_wire::DataflowId,
    generation: u64,
) -> Result<astrs_wire::NodeSpawnSpec, CliError> {
    let text = std::fs::read_to_string(path).map_err(|source| CliError::io(path, source))?;
    Ok(astrs_coordinator::expand_node_fragment(
        dataflow, generation, &text,
    )?)
}

/// Parses a user-typed node id, naming the offending flag on failure.
fn parse_node_id(flag: &'static str, value: &str) -> Result<NodeId, CliError> {
    NodeId::new(value).map_err(|source| CliError::BadArgument {
        flag,
        value: value.to_owned(),
        reason: source.to_string(),
    })
}

// ---------------------------------------------------------------------
// node add
// ---------------------------------------------------------------------

/// `astrs node add` arguments.
#[derive(Debug, Clone)]
pub struct AddArgs {
    /// The dataflow to add the node to.
    pub dataflow: DataflowRef,
    /// Path to the single-node manifest fragment.
    pub node_manifest: std::path::PathBuf,
    /// Spawn it immediately rather than leaving it pending.
    pub start: bool,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Adds a node to a running dataflow.
///
/// # Errors
///
/// As reading and expanding the manifest fragment, [`Client::resolve`]
/// and [`Client::request_ok`].
pub fn add(out: &mut dyn Write, endpoint: &Endpoint, args: &AddArgs) -> Result<(), CliError> {
    let runtime = runtime()?;
    let node_id = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&args.dataflow).await?;
        let spec = read_node_fragment(&args.node_manifest, dataflow, 0)?;
        let node_id = spec.node.clone();
        client
            .request_ok(
                "node add",
                &ControlRequest::AddNode {
                    dataflow,
                    node: Box::new(spec),
                    start: args.start,
                },
            )
            .await?;
        Ok::<_, CliError>(node_id)
    })?;
    let text = format!(
        "{node_id} added to {}{}",
        args.dataflow,
        if args.start { "" } else { " (not started)" }
    );
    emit(
        out,
        args.json,
        &text,
        || serde_json::json!({ "ok": true, "node": node_id.as_str(), "started": args.start }),
    );
    Ok(())
}

// ---------------------------------------------------------------------
// node remove
// ---------------------------------------------------------------------

/// `astrs node remove` arguments.
#[derive(Debug, Clone)]
pub struct RemoveArgs {
    /// The dataflow to remove the node from.
    pub dataflow: DataflowRef,
    /// The node id to remove.
    pub node_id: String,
    /// How long the node gets to finish before it is killed.
    pub grace: Option<f64>,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Removes a node from a running dataflow.
///
/// # Errors
///
/// As [`Client::resolve`] and [`Client::request_ok`].
pub fn remove(out: &mut dyn Write, endpoint: &Endpoint, args: &RemoveArgs) -> Result<(), CliError> {
    let node = parse_node_id("node-id", &args.node_id)?;
    let grace = args.grace.map(|secs| {
        astrs_wire::DurationMs::from_duration(std::time::Duration::from_secs_f64(secs))
    });
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&args.dataflow).await?;
        client
            .request_ok(
                "node remove",
                &ControlRequest::RemoveNode {
                    dataflow,
                    node: node.clone(),
                    grace,
                },
            )
            .await
    })?;
    let text = format!("{node} removed from {}", args.dataflow);
    emit(
        out,
        args.json,
        &text,
        || serde_json::json!({ "ok": true, "node": node.as_str() }),
    );
    Ok(())
}

// ---------------------------------------------------------------------
// node replace
// ---------------------------------------------------------------------

/// `astrs node replace` arguments.
#[derive(Debug, Clone)]
pub struct ReplaceArgs {
    /// The dataflow whose node should be replaced.
    pub dataflow: DataflowRef,
    /// The node id to replace.
    pub node_id: String,
    /// Path to the replacement's single-node manifest fragment.
    pub node_manifest: std::path::PathBuf,
    /// Let the outgoing incarnation drain first.
    ///
    /// Accepted, but not yet enforced by the coordinator — see
    /// `astrs_coordinator::handlers::topology::replace_node`'s own docs.
    pub drain: bool,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Replaces a live node with a new definition, keeping its existing
/// edges (a fresh generation cuts over with a brief dual-run window; see
/// `astrs-daemon`'s `dataflow::topology` module).
///
/// # Errors
///
/// As reading and expanding the manifest fragment, [`Client::resolve`]
/// and [`Client::request_ok`].
pub fn replace(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ReplaceArgs,
) -> Result<(), CliError> {
    let target = parse_node_id("node-id", &args.node_id)?;
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&args.dataflow).await?;
        let mut spec = read_node_fragment(&args.node_manifest, dataflow, 0)?;
        spec.node = target.clone();
        client
            .request_ok(
                "node replace",
                &ControlRequest::ReplaceNode {
                    dataflow,
                    node: Box::new(spec),
                    drain: args.drain,
                },
            )
            .await
    })?;
    let text = format!("{target} in {} replaced", args.dataflow);
    emit(
        out,
        args.json,
        &text,
        || serde_json::json!({ "ok": true, "node": target.as_str() }),
    );
    Ok(())
}

// ---------------------------------------------------------------------
// node connect / disconnect
// ---------------------------------------------------------------------

/// `astrs node connect` arguments.
#[derive(Debug, Clone)]
pub struct ConnectArgs {
    /// The dataflow to edit.
    pub dataflow: DataflowRef,
    /// The node whose input is being connected (or rewired).
    pub node_id: String,
    /// The input's name.
    pub input: String,
    /// The producer port to read from, as `node/output`.
    pub source: String,
    /// How many messages to buffer, or the default (§24.2) if unset.
    pub queue_size: Option<u32>,
    /// What to do when the queue is full, or the default if unset.
    pub queue_policy: Option<QueuePolicyArg>,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Adds — or, for an already-wired input, rewires — one input edge on a
/// live node.
///
/// # Errors
///
/// [`CliError::BadArgument`] if `node_id`/`input`/`source` are not usable.
/// As [`Client::resolve`] and [`Client::request_ok`] otherwise.
pub fn connect(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ConnectArgs,
) -> Result<(), CliError> {
    let consumer = parse_node_id("node-id", &args.node_id)?;
    let input_id = DataId::new(&args.input).map_err(|source| CliError::BadArgument {
        flag: "input",
        value: args.input.clone(),
        reason: source.to_string(),
    })?;
    let source: PortRef = args.source.parse().map_err(|_| CliError::BadArgument {
        flag: "source",
        value: args.source.clone(),
        reason: "expected a `node/output` port reference".to_owned(),
    })?;
    let mut input = InputSpec::new(input_id, source);
    if let Some(size) = args.queue_size {
        input.queue_size = size;
    }
    input.queue_policy = match args.queue_policy {
        Some(QueuePolicyArg::Backpressure) => QueuePolicy::Backpressure,
        Some(QueuePolicyArg::DropOldest) | None => QueuePolicy::DropOldest,
    };

    let runtime = runtime()?;
    runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&args.dataflow).await?;
        client
            .request_ok(
                "node connect",
                &ControlRequest::AddEdge {
                    dataflow,
                    consumer: consumer.clone(),
                    input: input.clone(),
                },
            )
            .await
    })?;
    let text = format!(
        "{consumer}.{} in {} now reads from {}",
        args.input, args.dataflow, input.source
    );
    emit(out, args.json, &text, || {
        serde_json::json!({
            "ok": true,
            "node": consumer.as_str(),
            "input": args.input,
            "source": input.source.to_string(),
        })
    });
    Ok(())
}

/// `astrs node disconnect` arguments.
#[derive(Debug, Clone)]
pub struct DisconnectArgs {
    /// The dataflow to edit.
    pub dataflow: DataflowRef,
    /// The node whose input is being disconnected.
    pub node_id: String,
    /// The input's name.
    pub input: String,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Disconnects one input edge from a live node — the consumer is told
/// [`astrs_wire::NodeEvent::InputClosed`] with
/// [`astrs_wire::RouteCloseReason::Disconnected`], not treated as a
/// producer failure.
///
/// # Errors
///
/// [`CliError::BadArgument`] if `node_id`/`input` are not usable. As
/// [`Client::resolve`] and [`Client::request_ok`] otherwise.
pub fn disconnect(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &DisconnectArgs,
) -> Result<(), CliError> {
    let consumer = parse_node_id("node-id", &args.node_id)?;
    let input_id = DataId::new(&args.input).map_err(|source| CliError::BadArgument {
        flag: "input",
        value: args.input.clone(),
        reason: source.to_string(),
    })?;

    let runtime = runtime()?;
    runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&args.dataflow).await?;
        client
            .request_ok(
                "node disconnect",
                &ControlRequest::RemoveEdge {
                    dataflow,
                    consumer: consumer.clone(),
                    input: input_id.clone(),
                },
            )
            .await
    })?;
    let text = format!(
        "{consumer}.{} in {} disconnected",
        args.input, args.dataflow
    );
    emit(
        out,
        args.json,
        &text,
        || serde_json::json!({ "ok": true, "node": consumer.as_str(), "input": args.input }),
    );
    Ok(())
}

/// Writes either the human line or the JSON object — this module's own
/// copy of `command::lifecycle`'s private helper of the same shape (kept
/// local rather than made `pub(crate)` there, to leave that module's own
/// concurrent edits alone).
fn emit(out: &mut dyn Write, json: bool, text: &str, value: impl FnOnce() -> serde_json::Value) {
    if json {
        let _ = writeln!(out, "{}", value());
    } else {
        let _ = writeln!(out, "{text}");
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-node-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn write_fragment(dir: &Path, name: &str, yaml: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, yaml).unwrap();
        path
    }

    fn dead_endpoint() -> Endpoint {
        // Port 1 on loopback: privileged, never bound in this test suite,
        // refused immediately rather than timing out — every verb below
        // only needs to prove it reaches the network layer with the right
        // request shape, not that a coordinator answers.
        Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            astrs_wire::AuthToken::ZERO,
        )
    }

    #[test]
    fn read_node_fragment_expands_a_bare_node_yaml() {
        let dir = scratch("fragment");
        let path = write_fragment(
            &dir,
            "extra.yml",
            "id: extra\npath: ./extra\noutputs: [out]\n",
        );
        let spec = read_node_fragment(&path, astrs_wire::DataflowId::generate(), 0).unwrap();
        assert_eq!(spec.node.as_str(), "extra");
        assert_eq!(spec.outputs.len(), 1);
    }

    #[test]
    fn read_node_fragment_reports_a_missing_file() {
        let err = read_node_fragment(
            Path::new("/does/not/exist/node.yml"),
            astrs_wire::DataflowId::generate(),
            0,
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Io { .. }));
    }

    #[test]
    fn read_node_fragment_reports_a_malformed_fragment() {
        let dir = scratch("bad-fragment");
        let path = write_fragment(&dir, "bad.yml", "not: [valid");
        let err = read_node_fragment(&path, astrs_wire::DataflowId::generate(), 0).unwrap_err();
        assert!(matches!(err, CliError::Coordinator(_)));
    }

    #[test]
    fn parse_node_id_names_the_offending_flag() {
        let err = parse_node_id("node-id", "not a legal id!").unwrap_err();
        match err {
            CliError::BadArgument { flag, value, .. } => {
                assert_eq!(flag, "node-id");
                assert_eq!(value, "not a legal id!");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn add_against_no_cluster_reports_no_cluster_not_a_panic() {
        let dir = scratch("add-no-cluster");
        let path = write_fragment(&dir, "extra.yml", "id: extra\npath: ./extra\n");
        let mut out = Vec::new();
        let err = add(
            &mut out,
            &dead_endpoint(),
            &AddArgs {
                dataflow: DataflowRef::parse("flow"),
                node_manifest: path,
                start: true,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::NoCluster { .. } | CliError::Transport(_)
        ));
    }

    #[test]
    fn remove_rejects_a_malformed_node_id_before_dialling_anything() {
        let mut out = Vec::new();
        let err = remove(
            &mut out,
            &dead_endpoint(),
            &RemoveArgs {
                dataflow: DataflowRef::parse("flow"),
                node_id: "not a legal id!".to_owned(),
                grace: None,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::BadArgument { .. }));
        assert!(out.is_empty(), "a rejected argument must print nothing");
    }

    #[test]
    fn connect_rejects_a_malformed_source_before_dialling_anything() {
        let mut out = Vec::new();
        let err = connect(
            &mut out,
            &dead_endpoint(),
            &ConnectArgs {
                dataflow: DataflowRef::parse("flow"),
                node_id: "detector".to_owned(),
                input: "frames".to_owned(),
                source: "not-a-port-ref".to_owned(),
                queue_size: None,
                queue_policy: None,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::BadArgument { flag: "source", .. }));
    }

    #[test]
    fn disconnect_rejects_a_malformed_input_before_dialling_anything() {
        let mut out = Vec::new();
        let err = disconnect(
            &mut out,
            &dead_endpoint(),
            &DisconnectArgs {
                dataflow: DataflowRef::parse("flow"),
                node_id: "detector".to_owned(),
                input: "not a legal id!".to_owned(),
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::BadArgument { flag: "input", .. }));
    }
}
