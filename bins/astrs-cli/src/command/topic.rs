//! `astrs topic echo/hz/info/pub` (blueprint §13, §17).
//!
//! ```text
//!   astrs topic echo cam/frames   ─► TopicSubscribe ─► pushed `Data` frames ─► decode+print
//!   astrs topic hz   cam/frames   ─► TopicSubscribe ─► pushed `Data` frames ─► windowed rate
//!   astrs topic info cam/frames   ─► GetNodeInfo (+ optional --manifest)   ─► type/subscribers
//!   astrs topic pub  src/out '1'  ─► a dynamic-node attach, one `send_batch`
//! ```
//!
//! # Why `echo`/`hz` need `debug: true`
//!
//! A tap is a *copy* of a local output the daemon must keep seeing bytes
//! for, which is only cheap because it is opt-in
//! ([`crate::command::param`]'s sibling module doc has nothing to say about
//! this — see `astrs-daemon`'s `tap` module and blueprint §13 instead). The
//! coordinator refuses `TopicSubscribe` for a dataflow whose manifest never
//! set `debug: true`; this module surfaces that refusal exactly as the
//! coordinator worded it ([`crate::error::CliError::Refused`]) rather than
//! inventing a second message.
//!
//! # `pub` attaches, it does not inject
//!
//! There is no wire verb that hands the coordinator a byte string to splice
//! into a running edge — see `astrs-coordinator`'s `handlers::logs::topic_publish`
//! for why that gap is deliberate. `astrs topic pub` instead does what
//! blueprint §8.3's `path: dynamic` exists for: it connects to the local
//! daemon *as* the named node (which the manifest must have declared with
//! `path: dynamic`) and publishes through the ordinary node API. The
//! "topic" a user names is therefore the *producer* port
//! (`dynamic_node/output`), not a consumer's input — exactly the shape
//! `astrs topic echo` already expects a port reference in.

use std::collections::VecDeque;
use std::io::Write;
use std::time::{Duration, Instant};

use astrs_data::builder::{BinaryBuilder, BooleanBuilder, Float64Builder};
use astrs_data::ipc::decode_payload;
use astrs_data::record_batch::RecordBatch;
use astrs_data::urn::layout_of;
use astrs_data::{DataType, IntoArrayRef};
use astrs_node_api::NodeBuilder;
use astrs_wire::{
    ControlReply, ControlRequest, DataFrame, DataflowId, FrameKind, Metadata, NodeInfo, PortRef,
    TopicQuery, TypeUrn, WireDecode,
};

use crate::command::client::{
    Client, DataflowRef, Endpoint, new_subscription_id, reply_name, runtime,
};
use crate::command::param::parse_value as parse_parameter_value;
use crate::command::signals::Signals;
use crate::error::CliError;

/// Arguments shared by `echo` and `hz`: a port reference, which dataflow it
/// belongs to (defaulting to the cluster's sole one), and how to reach the
/// coordinator.
#[derive(Debug, Clone)]
pub struct TopicStreamArgs {
    /// The port to tap, as `node/output`.
    pub topic: String,
    /// The dataflow it belongs to; `None` means "the only one running".
    pub dataflow: Option<String>,
    /// Emit JSON rather than human-readable lines.
    pub json: bool,
    /// Stop after this many data frames instead of streaming until
    /// interrupted (`ros2 topic echo --once` for a count of one).
    ///
    /// Mainly for scripts and tests: a bounded `echo`/`hz` is a function
    /// that returns rather than a process a caller must signal, which is
    /// what makes either one assertable without touching `SIGTERM` from a
    /// test.
    pub count: Option<u32>,
}

/// Parses the `node/output` shorthand every topic verb takes.
///
/// # Errors
///
/// [`CliError::BadArgument`] when the text has no separator, more than one,
/// or either half fails the identifier grammar.
pub fn parse_port(text: &str) -> Result<PortRef, CliError> {
    text.parse()
        .map_err(|error: astrs_wire::IdError| CliError::BadArgument {
            flag: "topic",
            value: text.to_owned(),
            reason: error.to_string(),
        })
}

/// Resolves which dataflow a topic verb targets: the one named, or the
/// cluster's sole running one when none was.
///
/// # Errors
///
/// As [`Client::resolve`]/[`Client::sole_dataflow`].
async fn resolve_dataflow(
    client: &mut Client,
    dataflow: Option<&str>,
) -> Result<DataflowId, CliError> {
    match dataflow {
        Some(text) => client.resolve(&DataflowRef::parse(text)).await,
        None => client.sole_dataflow(true).await,
    }
}

/// What `echo`/`hz` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamReport {
    /// How many data frames were printed (or folded into a rate).
    pub received: usize,
}

/// `astrs topic echo`: prints every message on a port as it arrives.
///
/// Runs until the coordinator closes the connection (the cluster went
/// down), [`TopicStreamArgs::count`] frames have been printed, or the
/// process is interrupted — at which point the subscription is closed
/// explicitly (in the latter two cases) — the same shape as `astrs logs
/// -f` — before this returns.
///
/// # Errors
///
/// - [`CliError::BadArgument`] for an unparsable `node/output`.
/// - [`CliError::Refused`] when the dataflow's manifest never set
///   `debug: true`.
/// - As [`Client::connect`]/[`Client::request`] otherwise.
pub fn echo(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &TopicStreamArgs,
) -> Result<StreamReport, CliError> {
    let port = parse_port(&args.topic)?;
    let runtime = runtime()?;
    runtime.block_on(async move {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = resolve_dataflow(&mut client, args.dataflow.as_deref()).await?;
        let subscription = new_subscription_id();
        client
            .request_ok(
                "topic echo",
                &ControlRequest::TopicSubscribe {
                    dataflow,
                    port: port.clone(),
                    query: TopicQuery::new(),
                    subscription,
                },
            )
            .await?;

        let mut signals = Signals::install();
        let mut received = 0usize;
        let mut stopped_early = false;
        loop {
            tokio::select! {
                frame = client.next_frame() => match frame? {
                    Some(frame) => {
                        if let Some(data) = decode_data_frame(&frame) {
                            print_echo(out, &data, args.json);
                            received += 1;
                            if args.count.is_some_and(|limit| received >= limit as usize) {
                                stopped_early = true;
                                break;
                            }
                        }
                    }
                    None => break,
                },
                () = signals.next() => {
                    stopped_early = true;
                    break;
                }
            }
        }
        // A bound reached is the same kind of deliberate stop as an
        // interrupt (as opposed to the coordinator closing the
        // connection): the subscription is still live and worth ending
        // explicitly rather than leaving it to the daemon's tap to notice
        // this session went away.
        if stopped_early {
            let _ = client
                .request_ok(
                    "topic echo",
                    &ControlRequest::TopicUnsubscribe { subscription },
                )
                .await;
        }
        Ok(StreamReport { received })
    })
}

/// `astrs topic hz`: reports a port's observed message rate, recomputed
/// every time a frame arrives from a trailing window (blueprint §17).
///
/// # Errors
///
/// As [`echo`].
pub fn hz(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &TopicStreamArgs,
    window: Duration,
) -> Result<StreamReport, CliError> {
    let port = parse_port(&args.topic)?;
    let runtime = runtime()?;
    runtime.block_on(async move {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = resolve_dataflow(&mut client, args.dataflow.as_deref()).await?;
        let subscription = new_subscription_id();
        client
            .request_ok(
                "topic hz",
                &ControlRequest::TopicSubscribe {
                    dataflow,
                    port: port.clone(),
                    query: TopicQuery::new(),
                    subscription,
                },
            )
            .await?;

        let mut rate = HzWindow::new(window);
        let mut signals = Signals::install();
        let mut received = 0usize;
        let mut stopped_early = false;
        loop {
            tokio::select! {
                frame = client.next_frame() => match frame? {
                    Some(frame) => {
                        if frame.kind() == FrameKind::Data {
                            let now = Instant::now();
                            rate.record(now);
                            received += 1;
                            print_hz(out, &rate, now, args.json);
                            if args.count.is_some_and(|limit| received >= limit as usize) {
                                stopped_early = true;
                                break;
                            }
                        }
                    }
                    None => break,
                },
                () = signals.next() => {
                    stopped_early = true;
                    break;
                }
            }
        }
        if stopped_early {
            let _ = client
                .request_ok(
                    "topic hz",
                    &ControlRequest::TopicUnsubscribe { subscription },
                )
                .await;
        }
        Ok(StreamReport { received })
    })
}

/// A trailing window of arrival instants, for `astrs topic hz`'s rate math.
///
/// Pure and independent of any socket, so the rate computation is tested
/// without a live cluster.
#[derive(Debug, Clone)]
pub struct HzWindow {
    /// How far back an arrival still counts.
    window: Duration,
    /// Arrival instants, oldest first.
    arrivals: VecDeque<Instant>,
}

impl HzWindow {
    /// A window that keeps arrivals for `window`.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            arrivals: VecDeque::new(),
        }
    }

    /// Records one arrival at `now`, dropping anything that has aged out.
    pub fn record(&mut self, now: Instant) {
        self.arrivals.push_back(now);
        self.evict(now);
    }

    /// Drops arrivals older than [`Self::window`] relative to `now`.
    fn evict(&mut self, now: Instant) {
        while let Some(&oldest) = self.arrivals.front() {
            if now.saturating_duration_since(oldest) > self.window {
                self.arrivals.pop_front();
            } else {
                break;
            }
        }
    }

    /// How many arrivals are currently inside the window.
    #[must_use]
    pub fn count(&self) -> usize {
        self.arrivals.len()
    }

    /// The observed rate, in messages per second, as of `now`.
    ///
    /// `None` before any arrival, or while the observed span is too short
    /// to divide by without an inflated result — the span used is the
    /// window itself once at least two arrivals separate by less than it,
    /// and the actual oldest-to-`now` span otherwise, which is what keeps
    /// the very first arrival from reading as "infinite Hz".
    #[must_use]
    pub fn rate_per_sec(&self, now: Instant) -> Option<f64> {
        let oldest = *self.arrivals.front()?;
        let span = now
            .saturating_duration_since(oldest)
            .as_secs_f64()
            .max(f64::EPSILON);
        if self.arrivals.len() < 2 {
            return None;
        }
        Some((self.arrivals.len() - 1) as f64 / span)
    }
}

/// Prints one `astrs topic hz` update.
fn print_hz(out: &mut dyn Write, rate: &HzWindow, now: Instant, json: bool) {
    let value = rate.rate_per_sec(now);
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({ "count": rate.count(), "hz": value })
        );
    } else {
        match value {
            Some(hz) => {
                let _ = writeln!(out, "{hz:.2} Hz ({} in window)", rate.count());
            }
            None => {
                let _ = writeln!(out, "… ({} in window)", rate.count());
            }
        }
    }
    let _ = out.flush();
}

/// Decodes a pushed frame as a [`DataFrame`], returning `None` for anything
/// that is not one — a stray `Log` push on a session that also has a log
/// subscription open, or a malformed frame, neither of which should end an
/// otherwise-healthy tail.
fn decode_data_frame(frame: &astrs_wire::Frame) -> Option<DataFrame> {
    if frame.kind() != FrameKind::Data {
        return None;
    }
    DataFrame::decode_exact(frame.payload()).ok()
}

/// Builds the line (or JSON object) `echo` prints for one frame.
fn print_echo(out: &mut dyn Write, frame: &DataFrame, json: bool) {
    let summary = summarize_payload(&frame.payload);
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({
                "source": frame.source.to_string(),
                "bytes": frame.payload.len(),
                "summary": summary,
            })
        );
    } else {
        let _ = writeln!(out, "[{}] {summary}", frame.source);
    }
    let _ = out.flush();
}

/// Summarizes a payload for a human: the Arrow schema when it decodes,
/// raw hex otherwise (blueprint §17: "raw hex fallback").
///
/// Not URN-aware on its own — a bare payload carries no type marker, only
/// its Arrow schema (blueprint §6.1) — but [`info`] resolves the port's
/// *declared* URN separately and prints it alongside this summary, which is
/// the honest split: what the bytes say, and what the manifest says they
/// should be.
#[must_use]
pub fn summarize_payload(payload: &[u8]) -> String {
    match decode_payload(payload) {
        Ok(batch) => summarize_batch(&batch),
        Err(_) => format!(
            "{} bytes (not decodable as Arrow IPC): {}",
            payload.len(),
            hex(payload)
        ),
    }
}

/// A one-line summary of a decoded batch: row count and each column's name
/// and Arrow type.
fn summarize_batch(batch: &RecordBatch) -> String {
    let columns: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|field| format!("{}:{:?}", field.name(), field.data_type()))
        .collect();
    format!(
        "{} row(s), {} column(s) [{}]",
        batch.num_rows(),
        batch.num_columns(),
        columns.join(", ")
    )
}

/// Renders `bytes` as lowercase hex, truncated with a marker past
/// [`HEX_PREVIEW_LIMIT`] bytes so one giant malformed payload cannot flood
/// a terminal.
const HEX_PREVIEW_LIMIT: usize = 64;

/// See `HEX_PREVIEW_LIMIT`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(HEX_PREVIEW_LIMIT)];
    let mut text = String::with_capacity(shown.len() * 2 + 4);
    for byte in shown {
        text.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > HEX_PREVIEW_LIMIT {
        text.push_str("...");
    }
    text
}

/// `astrs topic info`: a port's declared type, its subscriber list (when a
/// manifest is given), and a best-effort observed schema hash — obtained by
/// opening a tap for `INFO_PEEK_TIMEOUT` and reporting whatever the first
/// frame (if any) says, since a bare port carries no type marker of its own
/// (blueprint §6.1).
///
/// # Errors
///
/// As [`echo`], plus manifest parse/expand/graph-build errors when
/// `manifest_path` is given.
pub fn info(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    topic: &str,
    dataflow: Option<&str>,
    manifest_path: Option<&std::path::Path>,
    json: bool,
) -> Result<InfoReport, CliError> {
    let port = parse_port(topic)?;
    let runtime = runtime()?;
    let report = runtime.block_on(async move {
        let mut client = Client::connect(endpoint).await?;
        let dataflow_id = resolve_dataflow(&mut client, dataflow).await?;
        let node_info = match client
            .request(
                "topic info",
                &ControlRequest::GetNodeInfo {
                    dataflow: dataflow_id,
                    node: port.node().clone(),
                },
            )
            .await?
        {
            ControlReply::NodeInfo { nodes } => nodes.into_iter().next(),
            other => {
                return Err(CliError::UnexpectedReply {
                    request: "topic info",
                    reply: reply_name(&other),
                });
            }
        };
        let type_urn = node_info
            .as_ref()
            .and_then(|info: &NodeInfo| info.outputs.get(port.port()))
            .and_then(|urn| urn.clone());

        let observed = peek_one_frame(&mut client, dataflow_id, port.clone()).await;
        let (observed_schema, schema_hash) = match observed {
            Some((summary, hash)) => (Some(summary), hash),
            None => (None, None),
        };
        Ok(InfoReport {
            port: port.clone(),
            type_urn,
            observed_schema,
            schema_hash,
            subscribers: manifest_path
                .map(|path| subscribers_of(path, &port))
                .transpose()?
                .unwrap_or_default(),
        })
    })?;
    print_info(out, &report, json);
    Ok(report)
}

/// How long `info` waits for one tapped frame before giving up.
const INFO_PEEK_TIMEOUT: Duration = Duration::from_millis(800);

/// As [`summarize_payload`], but also returns the [`astrs_data::SchemaHash`]
/// blueprint §6.1 defines, when the payload decodes as Arrow IPC — `info`'s
/// own concern (`echo`/`hz` never need the hash, so `summarize_payload`
/// itself, and every test already written against its `String`-only
/// signature, stays exactly as it is).
fn summarize_with_hash(payload: &[u8]) -> (String, Option<astrs_data::SchemaHash>) {
    match astrs_data::ipc::decode_payload(payload) {
        Ok(batch) => (
            summarize_batch(&batch),
            Some(astrs_data::SchemaHash::of(batch.schema())),
        ),
        // The same payload fails the same way through `summarize_payload`'s
        // own decode attempt — reusing it here keeps the hex-fallback
        // rendering defined in exactly one place.
        Err(_) => (summarize_payload(payload), None),
    }
}

/// Opens a short-lived tap, returns the first frame's payload summary and
/// schema hash (if one arrives before [`INFO_PEEK_TIMEOUT`]), and always
/// unsubscribes before returning.
async fn peek_one_frame(
    client: &mut Client,
    dataflow: DataflowId,
    port: PortRef,
) -> Option<(String, Option<astrs_data::SchemaHash>)> {
    let subscription = new_subscription_id();
    client
        .request_ok(
            "topic info",
            &ControlRequest::TopicSubscribe {
                dataflow,
                port,
                query: TopicQuery::new(),
                subscription,
            },
        )
        .await
        .ok()?;

    let observed = tokio::time::timeout(INFO_PEEK_TIMEOUT, async {
        loop {
            match client.next_frame().await.ok()? {
                Some(frame) => {
                    if let Some(data) = decode_data_frame(&frame) {
                        return Some(summarize_with_hash(&data.payload));
                    }
                }
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();

    let _ = client
        .request_ok(
            "topic info",
            &ControlRequest::TopicUnsubscribe { subscription },
        )
        .await;
    observed
}

/// Every `node/input` that reads from `port`, from a manifest on disk.
///
/// `astrs-coordinator`'s `GetManifest` (a running dataflow's *expanded*
/// manifest) has no answering reply yet — see this crate's final report —
/// so this reads the same manifest a user would pass to `astrs graph`
/// rather than the live one, exactly the pattern `command::graph::run`
/// already uses.
///
/// Matches [`astrs_manifest::node::io::Input::source`] textually against
/// `port`'s own `node/output` rendering rather than building an
/// `astrs-graph` graph — [`astrs_manifest::Node::inputs`] already *is* the
/// edge list this needs, one `BTreeMap<String, Input>` per node, and going
/// through it directly avoids a second identifier system
/// (`astrs-graph::NodeId`/`PortName` wrap the same strings independently,
/// precisely so this crate's callers cannot mix the two up by accident —
/// which also means there is no free conversion the other way).
///
/// # Errors
///
/// As `command::graph::run`'s manifest pipeline.
fn subscribers_of(path: &std::path::Path, port: &PortRef) -> Result<Vec<PortRef>, CliError> {
    let content = std::fs::read_to_string(path).map_err(|err| CliError::io(path, err))?;
    let manifest = astrs_manifest::Manifest::from_yaml_str(&content)?;
    manifest.validate()?;
    let base_dir = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let loader = astrs_manifest::expand::FsModuleLoader;
    let expanded = manifest.expand(&base_dir, &loader)?;
    expanded.validate()?;

    let wanted = port.to_string();
    let mut subscribers = Vec::new();
    for node in &expanded.nodes {
        let Ok(consumer) = astrs_wire::NodeId::new(&node.id) else {
            continue;
        };
        for (input_name, input) in &node.inputs {
            if input.source != wanted {
                continue;
            }
            if let Ok(input_id) = astrs_wire::DataId::new(input_name) {
                subscribers.push(PortRef::new(consumer.clone(), input_id));
            }
        }
    }
    Ok(subscribers)
}

/// `astrs topic info`'s report.
#[derive(Debug, Clone)]
pub struct InfoReport {
    /// The port inspected.
    pub port: PortRef,
    /// Its declared type, when the manifest annotated it.
    pub type_urn: Option<TypeUrn>,
    /// A summary of one observed frame's actual Arrow schema, when one
    /// arrived inside `INFO_PEEK_TIMEOUT`.
    pub observed_schema: Option<String>,
    /// That frame's [`astrs_data::SchemaHash`] (blueprint §6.1) — the same
    /// fingerprint a real receiver caches decoded schemas against, so this
    /// is not only a summary but the literal identity a `_schema_hash`
    /// metadata entry would carry. `None` exactly when `observed_schema`
    /// is `None`, or when a frame arrived but was not decodable as Arrow
    /// IPC (raw bytes have no schema to hash).
    pub schema_hash: Option<astrs_data::SchemaHash>,
    /// Every `node/input` wired from this port, when `--manifest` was
    /// given; empty otherwise.
    pub subscribers: Vec<PortRef>,
}

/// Prints an [`InfoReport`].
fn print_info(out: &mut dyn Write, report: &InfoReport, json: bool) {
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({
                "port": report.port.to_string(),
                "type_urn": report.type_urn.as_ref().map(ToString::to_string),
                "plane": report.observed_schema.as_ref().map(|_| "daemon-mediated (tapped)"),
                "observed_schema": report.observed_schema,
                "schema_hash": report.schema_hash.as_ref().map(ToString::to_string),
                "subscribers": report.subscribers.iter().map(ToString::to_string).collect::<Vec<_>>(),
            })
        );
    } else {
        let _ = writeln!(out, "{}", report.port);
        let _ = writeln!(
            out,
            "  type: {}",
            report
                .type_urn
                .as_ref()
                .map_or_else(|| "(untyped)".to_owned(), ToString::to_string)
        );
        match &report.observed_schema {
            Some(schema) => {
                let _ = writeln!(out, "  plane: daemon-mediated (tapped)");
                let _ = writeln!(out, "  observed: {schema}");
                match &report.schema_hash {
                    Some(hash) => {
                        let _ = writeln!(out, "  schema_hash: {hash}");
                    }
                    None => {
                        let _ = writeln!(out, "  schema_hash: (not decodable as Arrow IPC)");
                    }
                }
            }
            None => {
                let _ = writeln!(out, "  plane: unknown (no frame observed)");
            }
        }
        if report.subscribers.is_empty() {
            let _ = writeln!(out, "  subscribers: (pass --manifest to list them)");
        } else {
            let _ = writeln!(out, "  subscribers:");
            for subscriber in &report.subscribers {
                let _ = writeln!(out, "    {subscriber}");
            }
        }
    }
    let _ = out.flush();
}

/// `astrs topic pub`: publishes one message through a dynamic-node attach
/// (see this module's own docs for why `pub` cannot inject into an
/// arbitrary running edge).
///
/// # Errors
///
/// - [`CliError::BadArgument`] for an unparsable `node/output` or a
///   message JSON `--typed` cannot coerce to the port's declared type.
/// - [`CliError::Node`] if the daemon refuses the attach (the manifest
///   node named is not `path: dynamic`, or is already attached).
/// - As [`echo`] for the coordinator half (resolving the dataflow, reading
///   the declared type).
pub fn publish(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &PublishArgs,
) -> Result<PublishReport, CliError> {
    let port = parse_port(&args.topic)?;
    let runtime = runtime()?;
    let lookup_port = port.clone();
    let (dataflow, type_urn) = runtime.block_on(async move {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = resolve_dataflow(&mut client, args.dataflow.as_deref()).await?;
        let type_urn = match client
            .request(
                "topic pub",
                &ControlRequest::GetNodeInfo {
                    dataflow,
                    node: lookup_port.node().clone(),
                },
            )
            .await
        {
            Ok(ControlReply::NodeInfo { nodes }) => nodes
                .into_iter()
                .next()
                .and_then(|info| info.outputs.get(lookup_port.port()).cloned())
                .flatten(),
            // A node the coordinator has no record of yet (or any other
            // read failure) is not fatal here: the untyped fallback below
            // still lets the attach itself be the real test of whether the
            // node exists and is dynamic.
            _ => None,
        };
        Ok::<_, CliError>((dataflow, type_urn))
    })?;

    let batch = encode_message(&args.message, type_urn.as_ref())?;

    let repeat = args.rate.filter(|hz| *hz > 0.0);
    let attach_port = port.clone();
    let sent = runtime.block_on(async move {
        let builder = NodeBuilder::new()
            .dataflow(dataflow)
            .node_id(attach_port.node().as_str())?
            .dynamic(true)
            .output(attach_port.port().as_str())?
            .auth(endpoint.token.clone());
        // `connect_async`, not `connect`: this whole block already runs
        // inside `runtime`'s own `block_on` (a *current-thread* runtime —
        // see `command::client::runtime`), and `NodeBuilder::connect`'s
        // sync form blocks that runtime's one worker to drive its own
        // dial — a deadlock `astrs-node-api` refuses outright rather than
        // hang on. `spawn_blocking` does not sidestep that refusal: a
        // blocking-pool thread still reports the *enclosing* runtime's
        // flavor to `NodeRuntime::acquire`, so `connect()` still sees
        // "current-thread" and still refuses. Calling the async form
        // directly is not a workaround, it is the correct call from
        // inside an already-async context.
        let (mut node, _events) = builder.connect_async().await?;
        let mut output = node.raw_output(attach_port.port().as_str())?;
        output.send_batch(&batch, Metadata::new(node.hlc_now()))?;
        let mut sent = 1usize;

        if let Some(hz) = repeat {
            let mut signals = Signals::install();
            let interval = Duration::from_secs_f64(1.0 / hz);
            loop {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {
                        output.send_batch(&batch, Metadata::new(node.hlc_now()))?;
                        sent += 1;
                    }
                    () = signals.next() => break,
                }
            }
        }
        // `shutdown`, not the bare `close_outputs` this line used to
        // call: only `shutdown` also queues the writer's own shutdown
        // sentinel (`request_drain`), which is what lets the writer task
        // *end* — and therefore what lets `flush_outputs` below observe
        // it ending — the moment everything is flushed, rather than
        // idling until `flush_outputs`'s own timeout gives up on it.
        let _ = node.shutdown();
        // Queuing the close is not sending it: `flush_outputs` awaits the
        // writer task actually putting every queued frame (this publish,
        // and the close itself) on the wire before the node — and the
        // runtime under it — go away. Without this, a single-shot
        // `astrs topic pub` racing its own current-thread runtime's
        // teardown could return "sent" for a message the daemon never
        // saw (see `Node::flush_outputs`'s own docs for the mechanism).
        node.flush_outputs().await;
        Ok::<_, CliError>(sent)
    })?;

    let report = PublishReport { port, sent };
    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({ "port": report.port.to_string(), "sent": report.sent })
        );
    } else {
        let _ = writeln!(
            out,
            "published {} message(s) on {}",
            report.sent, report.port
        );
    }
    let _ = out.flush();
    Ok(report)
}

/// Arguments for [`publish`].
#[derive(Debug, Clone)]
pub struct PublishArgs {
    /// The port to publish onto, as `node/output`.
    pub topic: String,
    /// The message, as JSON.
    pub message: String,
    /// The dataflow it belongs to; `None` means "the only one running".
    pub dataflow: Option<String>,
    /// Repeat at this rate (Hz) instead of publishing once.
    pub rate: Option<f64>,
    /// Emit JSON rather than a human-readable line.
    pub json: bool,
}

/// What [`publish`] did.
#[derive(Debug, Clone)]
pub struct PublishReport {
    /// The port published onto.
    pub port: PortRef,
    /// How many messages were sent.
    pub sent: usize,
}

/// Encodes a JSON message as a one-row [`RecordBatch`], typed by `urn`'s
/// layout when it names a scalar this function knows how to build, and as
/// a single raw `Binary` column of the message's UTF-8 bytes otherwise
/// (blueprint §17: "typed via URN layout when annotated, bytes otherwise").
///
/// `urn` is [`astrs_wire::TypeUrn`] — the validated-string form a manifest
/// annotation carries over the wire — and is re-parsed here into
/// [`astrs_data::urn::TypeUrn`], the richer form [`layout_of`] resolves a
/// [`DataType`] from; the two are distinct types in distinct crates (the
/// wire family does not depend on `astrs-data`), and this is the one seam
/// that bridges them.
///
/// # Errors
///
/// [`CliError::BadArgument`] when `urn` names a scalar layout but
/// `message` is not JSON of a shape [`crate::command::param::parse_value`]
/// can coerce to it.
fn encode_message(message: &str, urn: Option<&TypeUrn>) -> Result<RecordBatch, CliError> {
    if let Some(urn) = urn
        && let Ok(data_urn) = astrs_data::urn::TypeUrn::parse(urn.as_str())
        && let Ok(layout) = layout_of(&data_urn)
        && let Some(batch) = typed_batch(message, &layout)?
    {
        return Ok(batch);
    }
    let mut builder = BinaryBuilder::with_capacity(1, message.len());
    builder.append_value(message.as_bytes());
    Ok(RecordBatch::from_payload(builder.finish().into_array_ref()))
}

/// Builds a one-row batch of `layout`, or `None` for a layout this
/// CLI-side encoder does not attempt (anything beyond the plain scalars a
/// parameter value already covers — struct/list/tensor layouts need the
/// node's own schema, not a guess from one JSON scalar).
fn typed_batch(message: &str, layout: &DataType) -> Result<Option<RecordBatch>, CliError> {
    use astrs_data::builder::{Int64Builder, StringBuilder};
    use astrs_wire::Parameter;

    let mismatch = || CliError::BadArgument {
        flag: "message",
        value: message.to_owned(),
        reason: format!("the port's declared type needs a {layout:?}, which that value is not"),
    };

    let batch = match layout {
        DataType::Bool => {
            let Parameter::Bool(value) = parse_parameter_value(message)? else {
                return Err(mismatch());
            };
            let mut builder = BooleanBuilder::with_capacity(1);
            builder.append_value(value);
            RecordBatch::from_payload(builder.finish().into_array_ref())
        }
        DataType::Float64 => {
            let value = match parse_parameter_value(message)? {
                Parameter::Float(value) => value,
                #[allow(clippy::cast_precision_loss)]
                Parameter::Integer(value) => value as f64,
                _ => return Err(mismatch()),
            };
            let mut builder = Float64Builder::with_capacity(1);
            builder.append_value(value);
            RecordBatch::from_payload(builder.finish().into_array_ref())
        }
        DataType::Int64 => {
            let Parameter::Integer(value) = parse_parameter_value(message)? else {
                return Err(mismatch());
            };
            let mut builder = Int64Builder::with_capacity(1);
            builder.append_value(value);
            RecordBatch::from_payload(builder.finish().into_array_ref())
        }
        DataType::Utf8 => {
            let Parameter::String(value) = parse_parameter_value(message)? else {
                return Err(mismatch());
            };
            let mut builder = StringBuilder::with_capacity(1, value.len());
            builder.append_value(&value);
            RecordBatch::from_payload(builder.finish().into_array_ref())
        }
        _ => return Ok(None),
    };
    Ok(Some(batch))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_port_reference_parses_node_and_output() {
        let port = parse_port("camera/frames").unwrap();
        assert_eq!(port.node().as_str(), "camera");
        assert_eq!(port.port().as_str(), "frames");
    }

    #[test]
    fn a_port_reference_with_no_separator_is_refused() {
        let error = parse_port("camera").unwrap_err();
        assert!(matches!(error, CliError::BadArgument { flag: "topic", .. }));
    }

    #[test]
    fn hz_window_is_empty_before_any_arrival() {
        let window = HzWindow::new(Duration::from_secs(5));
        let now = Instant::now();
        assert_eq!(window.count(), 0);
        assert_eq!(window.rate_per_sec(now), None);
    }

    #[test]
    fn hz_window_reports_none_on_a_single_arrival() {
        let mut window = HzWindow::new(Duration::from_secs(5));
        let now = Instant::now();
        window.record(now);
        assert_eq!(window.count(), 1);
        assert_eq!(window.rate_per_sec(now), None);
    }

    #[test]
    fn hz_window_computes_the_rate_across_evenly_spaced_arrivals() {
        let mut window = HzWindow::new(Duration::from_secs(10));
        let start = Instant::now();
        for step in 0..5u64 {
            window.record(start + Duration::from_millis(step * 100));
        }
        let now = start + Duration::from_millis(400);
        // 5 arrivals spanning 400ms is 4 intervals over 0.4s = 10 Hz.
        let hz = window.rate_per_sec(now).unwrap();
        assert!((hz - 10.0).abs() < 1e-6, "{hz}");
    }

    #[test]
    fn hz_window_evicts_arrivals_older_than_the_window() {
        let mut window = HzWindow::new(Duration::from_millis(500));
        let start = Instant::now();
        window.record(start);
        window.record(start + Duration::from_millis(100));
        window.record(start + Duration::from_secs(2));
        // The first two arrivals are more than 500ms behind the third.
        assert_eq!(window.count(), 1);
    }

    #[test]
    fn a_valid_arrow_payload_summarizes_with_its_schema() {
        use astrs_data::builder::Int64Builder;
        use astrs_data::ipc::encode_payload;

        let mut builder = Int64Builder::with_capacity(3);
        builder.append_value(1);
        builder.append_value(2);
        builder.append_value(3);
        let batch = RecordBatch::from_payload(builder.finish().into_array_ref());
        let encoded = encode_payload(&batch).expect("a valid batch encodes");

        let summary = summarize_payload(encoded.as_ref());
        assert!(summary.contains("3 row(s)"), "{summary}");
        assert!(summary.contains("1 column(s)"), "{summary}");
        assert!(summary.contains("Int64"), "{summary}");
        assert!(
            !summary.contains("not decodable"),
            "a real Arrow payload must not fall back to hex: {summary}"
        );
    }

    #[test]
    fn a_non_arrow_payload_summarizes_as_hex() {
        let summary = summarize_payload(b"not arrow at all");
        assert!(summary.contains("not decodable"), "{summary}");
        assert!(summary.contains("6e6f7420"), "{summary}");
    }

    #[test]
    fn a_valid_arrow_payload_also_carries_its_schema_hash() {
        use astrs_data::builder::Int64Builder;
        use astrs_data::ipc::encode_payload;

        let mut builder = Int64Builder::with_capacity(1);
        builder.append_value(1);
        let batch = RecordBatch::from_payload(builder.finish().into_array_ref());
        let encoded = encode_payload(&batch).expect("a valid batch encodes");

        let (summary, hash) = summarize_with_hash(encoded.as_ref());
        assert!(summary.contains("Int64"), "{summary}");
        let hash = hash.expect("a decodable batch has a schema to hash");
        assert_eq!(
            hash,
            astrs_data::SchemaHash::of(batch.schema()),
            "`info` must hash exactly the schema blueprint §6.1 defines, \
             not a summary of it"
        );
    }

    #[test]
    fn a_non_arrow_payload_has_no_schema_hash_but_still_summarizes() {
        let (summary, hash) = summarize_with_hash(b"not arrow at all");
        assert!(summary.contains("not decodable"), "{summary}");
        assert!(hash.is_none(), "raw bytes have no schema to hash");
    }

    #[test]
    fn hex_truncates_long_payloads_with_a_marker() {
        let bytes = vec![0xAB; HEX_PREVIEW_LIMIT + 10];
        let text = hex(&bytes);
        assert!(text.ends_with("..."));
        assert_eq!(text.len(), HEX_PREVIEW_LIMIT * 2 + 3);
    }

    #[test]
    fn an_untyped_message_encodes_as_a_binary_column() {
        let batch = encode_message("hello", None).unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.schema().fields()[0].data_type(), &DataType::Binary);
    }

    #[test]
    fn a_float_urn_encodes_a_plain_number_as_float64() {
        let urn = TypeUrn::new("std/core/v1/Float64").unwrap();
        let batch = encode_message("1.5", Some(&urn)).unwrap();
        assert_eq!(batch.schema().fields()[0].data_type(), &DataType::Float64);
    }

    #[test]
    fn a_bool_urn_rejects_a_non_boolean_value() {
        let urn = TypeUrn::new("std/core/v1/Bool").unwrap();
        let error = encode_message("\"left\"", Some(&urn)).unwrap_err();
        assert!(matches!(
            error,
            CliError::BadArgument {
                flag: "message",
                ..
            }
        ));
    }

    #[test]
    fn every_topic_verb_reports_a_dead_cluster_rather_than_hanging() {
        let endpoint = Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            astrs_wire::AuthToken::ZERO,
        );
        let stream_args = TopicStreamArgs {
            topic: "camera/frames".to_owned(),
            dataflow: None,
            json: false,
            count: None,
        };
        let echo_error = echo(&mut Vec::new(), &endpoint, &stream_args).unwrap_err();
        assert!(
            matches!(echo_error, CliError::NoCluster { .. }),
            "{echo_error}"
        );

        let hz_error = hz(
            &mut Vec::new(),
            &endpoint,
            &stream_args,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(hz_error, CliError::NoCluster { .. }), "{hz_error}");

        let info_error = info(
            &mut Vec::new(),
            &endpoint,
            "camera/frames",
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            matches!(info_error, CliError::NoCluster { .. }),
            "{info_error}"
        );

        let publish_args = PublishArgs {
            topic: "camera/frames".to_owned(),
            message: "1".to_owned(),
            dataflow: None,
            rate: None,
            json: false,
        };
        let publish_error = publish(&mut Vec::new(), &endpoint, &publish_args).unwrap_err();
        assert!(
            matches!(publish_error, CliError::NoCluster { .. }),
            "{publish_error}"
        );
    }
}
