//! The cluster-side lifecycle verbs: `start`, `stop`, `restart`,
//! `destroy`, `clean` (blueprint §17).
//!
//! Each is one [`ControlRequest`] over the shared
//! [`crate::command::client::Client`], plus the presentation a terminal and
//! a script each need. Nothing here decides *policy* — placement, grace
//! ladders, artefact reclamation all belong to the coordinator; this module
//! decides which request a user's words mean, and how to say what came back.
//!
//! ```text
//!   astrs start d.yml --attach ─► Start{Manifest}   ─► Started{dataflow}
//!                                 LogSubscribe      ─► Log frames ──► terminal
//!                                 Check (polled)    ─► DataflowResult ─► exit code
//!
//!   astrs stop <ref>           ─► Stop / StopByName ─► Ok
//!   astrs restart <ref>        ─► Restart / RestartByName
//!   astrs destroy [<ref>]      ─► Destroy{force}
//!   astrs clean [<ref>]        ─► Clean{artifacts, logs}
//! ```
//!
//! # An id or a name, never both spellings at one call site
//!
//! Every one of these verbs takes "the dataflow" as one positional word.
//! The protocol has *two* requests for most of them — `Stop{dataflow}` and
//! `StopByName{name}` — so [`crate::command::client::DataflowRef`] does the
//! classification once and each verb picks the matching request. A user
//! never types `--by-name`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astrs_wire::{
    ControlReply, ControlRequest, DataflowId, DataflowResult, DataflowSource, DataflowStatus,
    DurationMs, FrameKind, LogFrame, LogLevel, LogQuery, LogRecord, WireDecode,
};

use crate::command::client::{
    Client, DataflowRef, Endpoint, new_subscription_id, reply_name, runtime,
};
use crate::command::log_stream::{LogFilter, LogStyle, StreamItem, prefix_width, render};
use crate::error::CliError;

/// How often an attached `start` asks whether the dataflow has finished.
///
/// The coordinator has no "tell me when it ends" push for a CLI (§7.3's
/// control family answers requests; only subscriptions push), so an attached
/// run polls. Fast enough that a short graph's exit is not perceptibly
/// delayed, slow enough that a long one costs nothing.
pub const ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// `astrs start`'s arguments.
#[derive(Debug, Clone)]
pub struct StartArgs {
    /// The manifest to start.
    pub manifest_path: PathBuf,
    /// A name for the run.
    pub name: Option<String>,
    /// `--machine` placement overrides, as `MACHINE` or `NODE=MACHINE`
    /// (§8.3 `deploy.machine`).
    pub machines: Vec<String>,
    /// Stream logs until it finishes, then exit with its severity.
    pub attach: bool,
    /// Hide attached output below this level.
    pub level: Option<LogLevel>,
    /// Emit JSON rather than a human summary.
    pub json: bool,
    /// Colorize attached output.
    pub color: bool,
}

impl StartArgs {
    /// The arguments for starting `manifest_path` with every default.
    #[must_use]
    pub fn new(manifest_path: impl Into<PathBuf>) -> Self {
        Self {
            manifest_path: manifest_path.into(),
            name: None,
            machines: Vec::new(),
            attach: false,
            level: None,
            json: false,
            color: false,
        }
    }
}

/// What one `astrs start` did.
#[derive(Debug, Clone)]
pub struct StartReport {
    /// The id the coordinator assigned.
    pub dataflow: DataflowId,
    /// The name it was started under.
    pub name: Option<String>,
    /// The final verdict, when `--attach` waited for one.
    pub result: Option<DataflowResult>,
}

impl StartReport {
    /// The process exit code: the dataflow's severity when attached, `0`
    /// when the start merely succeeded.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match &self.result {
            None => 0,
            Some(result) => match result.status {
                DataflowStatus::Finished if !result.has_failures() => 0,
                DataflowStatus::Finished | DataflowStatus::Failed => 1,
                _ => 2,
            },
        }
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "dataflow": self.dataflow.to_string(),
            "name": self.name,
            "attached": self.result.is_some(),
            "status": self.result.as_ref().map(|result| result.status.as_str()),
            "failed": self.result.as_ref().is_some_and(DataflowResult::has_failures),
            "exit_code": self.exit_code(),
        })
    }
}

/// Starts a dataflow on a running cluster.
///
/// # Errors
///
/// - [`CliError::Manifest`] / [`CliError::Validation`] if the manifest is
///   not usable — checked *locally* before it is sent, so a typo costs a
///   round trip's worth of nothing rather than a coordinator error about a
///   plan.
/// - [`CliError::NoCluster`] if nothing is listening.
/// - [`CliError::Refused`] if the coordinator refused the start.
pub fn start(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &StartArgs,
) -> Result<StartReport, CliError> {
    let mut manifest = astrs_manifest::Manifest::from_yaml_file(&args.manifest_path)?;
    manifest.validate()?;
    // The file's own bytes are sent unchanged when nothing overrides them,
    // so a manifest with comments, anchors or key ordering the coordinator
    // never sees round-trips exactly as written. Re-serializing is the
    // *override* path's cost, not everybody's.
    let yaml = if args.machines.is_empty() {
        std::fs::read_to_string(&args.manifest_path)
            .map_err(|source| CliError::io(&args.manifest_path, source))?
    } else {
        apply_machine_overrides(&mut manifest, &args.machines)?;
        manifest.to_yaml()?
    };
    let working_dir = manifest_dir(&args.manifest_path);

    let style = LogStyle {
        color: args.color,
        prefix_width: prefix_width(manifest.nodes.iter().map(|node| node.id.as_str())),
        elapsed: true,
    };
    let filter = LogFilter::new().with_min_level(args.level.unwrap_or(LogLevel::Trace));

    let runtime = runtime()?;
    let report = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let request = ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml,
                working_dir: Some(working_dir.display().to_string()),
            },
            name: args.name.clone(),
            detach: !args.attach,
        };
        let (dataflow, name) = match client.request("start", &request).await? {
            ControlReply::Started { dataflow, name } => (dataflow, name),
            other => {
                return Err(CliError::UnexpectedReply {
                    request: "start",
                    reply: reply_name(&other),
                });
            }
        };
        if !args.attach {
            return Ok(StartReport {
                dataflow,
                name,
                result: None,
            });
        }
        let result = attach(out, endpoint, &mut client, dataflow, &filter, style).await?;
        Ok(StartReport {
            dataflow,
            name,
            result: Some(result),
        })
    })?;

    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&report.to_json()).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let _ = writeln!(out, "{}", start_summary(&report));
    }
    Ok(report)
}

/// The human line a finished `start` prints.
fn start_summary(report: &StartReport) -> String {
    let named = report
        .name
        .as_ref()
        .map_or_else(String::new, |name| format!(" ({name})"));
    match &report.result {
        None => format!("started {}{named}", report.dataflow),
        Some(result) => format!(
            "{}{named} {}: {}",
            report.dataflow,
            result.status.as_str(),
            if result.message.is_empty() {
                "no message".to_owned()
            } else {
                result.message.clone()
            }
        ),
    }
}

/// Streams a started dataflow's logs on a second connection until it reaches
/// a terminal status, polling for that status on `control`, then backfills
/// whatever the live stream was too late to see.
///
/// Two connections rather than one: pushed `Log` frames and answered
/// `ControlReply` frames share a socket, so a single connection would have
/// to demultiplex them by kind while a request is in flight — and a poll
/// that consumed a pushed log frame while waiting for its own reply would
/// lose it. One socket subscribes and only reads; the other only asks.
///
/// # Why a subscription alone is not enough
///
/// `LogSubscribe` is the *push* path (§13): the coordinator fans out records
/// as daemons produce them and delivers nothing retroactively. A short
/// dataflow — the whole `examples/` estate, and every conformance graph —
/// can start, run and finish inside the round trip that opens the
/// subscription, so an attached start that only subscribed printed nothing
/// at all and then reported the verdict for a run whose output the user
/// never saw. The *pull* path (`ControlRequest::Logs`, answered from each
/// daemon's `LogHistory` ring) is what remembers, so the two are used
/// together: subscribe first so a long run streams as it happens, pull once
/// at the end so a short one is not lost. [`LogTail`] de-duplicates the
/// overlap the two paths necessarily share.
async fn attach(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    control: &mut Client,
    dataflow: DataflowId,
    filter: &LogFilter,
    style: LogStyle,
) -> Result<DataflowResult, CliError> {
    let mut logs = Client::connect(endpoint).await?;
    let subscription = new_subscription_id();
    logs.request_ok(
        "logs -f",
        &ControlRequest::LogSubscribe {
            dataflow: Some(dataflow),
            node: None,
            query: LogQuery::default(),
            subscription,
        },
    )
    .await?;

    let mut tail = LogTail::new(filter.clone(), style);
    let mut poll = tokio::time::interval(ATTACH_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            frame = logs.next_frame() => {
                match frame? {
                    Some(frame) => tail.push_frame(out, &frame),
                    // The coordinator closed the log connection; the poll
                    // below still decides the outcome.
                    None => tokio::time::sleep(ATTACH_POLL_INTERVAL).await,
                }
            }
            _ = poll.tick() => {
                if let Some(result) = check_terminal(control, dataflow).await? {
                    break result;
                }
            }
        }
    };
    backfill(out, control, dataflow, &mut tail).await;
    Ok(result)
}

/// Prints every record the live subscription missed, on the connection that
/// was already asking questions.
///
/// Deliberately infallible: the dataflow's verdict is already decided and is
/// what the caller's exit code is made of, so a coordinator that cannot
/// answer a *history* query — the dataflow was cleaned up between the verdict
/// and this call, a daemon dropped out — must not turn a successful run into
/// a failed command. The records are a courtesy; the verdict is the contract.
async fn backfill(
    out: &mut dyn Write,
    control: &mut Client,
    dataflow: DataflowId,
    tail: &mut LogTail,
) {
    let reply = control
        .request(
            "logs",
            &ControlRequest::Logs {
                dataflow,
                node: None,
                query: LogQuery::default(),
            },
        )
        .await;
    if let Ok(ControlReply::Logs { records, .. }) = reply {
        tail.backfill(out, records);
    }
}

/// One attached run's log output: renders records, and remembers which ones
/// it has already rendered.
///
/// The push and pull paths overlap by construction — every record the
/// subscription delivered is also in the daemon's history ring — so the
/// closing pull would otherwise reprint the whole run. The key is
/// `(timestamp, node, message)` rather than the record itself because
/// [`astrs_wire::LogRecord`] is not ordered or hashed, and because those
/// three fields are what a reader would call "the same line": an HLC stamp
/// is unique per producer per event (§14), so two records sharing all three
/// are the same record having travelled two paths.
struct LogTail {
    /// Which records to render at all.
    filter: LogFilter,
    /// How to render them.
    style: LogStyle,
    /// When the attach began, for the elapsed-time prefix.
    started: Instant,
    /// The keys of every record already rendered.
    seen: std::collections::BTreeSet<(astrs_time::HlcTimestamp, String, String)>,
}

impl LogTail {
    /// An empty tail, starting its elapsed clock now.
    fn new(filter: LogFilter, style: LogStyle) -> Self {
        Self {
            filter,
            style,
            started: Instant::now(),
            seen: std::collections::BTreeSet::new(),
        }
    }

    /// The de-duplication key for one record.
    fn key(record: &LogRecord) -> (astrs_time::HlcTimestamp, String, String) {
        (
            record.timestamp,
            record
                .node
                .as_ref()
                .map(|node| node.as_str().to_owned())
                .unwrap_or_default(),
            record.message.clone(),
        )
    }

    /// Renders one pushed frame, ignoring kinds this verb did not subscribe
    /// to.
    ///
    /// A frame that fails to decode is dropped rather than ending the stream:
    /// a log tail must survive one malformed record from a node whose output
    /// was not valid UTF-8, not exit because of it.
    fn push_frame(&mut self, out: &mut dyn Write, frame: &astrs_wire::Frame) {
        if frame.kind() != FrameKind::Log {
            return;
        }
        let Ok(log) = LogFrame::decode_exact(frame.payload()) else {
            return;
        };
        self.render(out, log.record);
    }

    /// Renders every record of a closing history pull that the live stream
    /// did not already show, oldest first.
    fn backfill(&mut self, out: &mut dyn Write, records: Vec<LogRecord>) {
        let mut records = records;
        records.sort_by_key(|record| record.timestamp);
        for record in records {
            self.render(out, record);
        }
    }

    /// Renders one record, once.
    fn render(&mut self, out: &mut dyn Write, record: LogRecord) {
        if !self.seen.insert(Self::key(&record)) {
            return;
        }
        let item = StreamItem::Record(Box::new(record));
        if !self.filter.accepts(&item) {
            return;
        }
        let _ = writeln!(out, "{}", render(&item, self.style, self.started.elapsed()));
        let _ = out.flush();
    }
}

/// One `Check`, answering `Some` only for a terminal verdict.
async fn check_terminal(
    client: &mut Client,
    dataflow: DataflowId,
) -> Result<Option<DataflowResult>, CliError> {
    match client
        .request(
            "status",
            &ControlRequest::Check {
                dataflow: Some(dataflow),
            },
        )
        .await?
    {
        ControlReply::DataflowResult { result } => Ok(Some(*result)),
        // `Ok` means "known, still going" — the coordinator's own answer for
        // a dataflow that has not reached a verdict yet.
        ControlReply::Ok => Ok(None),
        other => Err(CliError::UnexpectedReply {
            request: "status",
            reply: reply_name(&other),
        }),
    }
}

/// `astrs stop`'s arguments.
#[derive(Debug, Clone)]
pub struct StopArgs {
    /// The dataflow to stop.
    pub dataflow: DataflowRef,
    /// How long nodes get to finish.
    pub grace: Option<Duration>,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Stops a dataflow.
///
/// # Errors
///
/// As [`Client::request`], plus [`CliError::UnexpectedReply`].
pub fn stop(out: &mut dyn Write, endpoint: &Endpoint, args: &StopArgs) -> Result<(), CliError> {
    let grace = args.grace.map(DurationMs::from_duration);
    let request = match &args.dataflow {
        DataflowRef::Id(dataflow) => ControlRequest::Stop {
            dataflow: *dataflow,
            grace,
        },
        DataflowRef::Name(name) => ControlRequest::StopByName {
            name: name.clone(),
            grace,
        },
    };
    one_shot(out, endpoint, "stop", &request, args.json, |()| {
        format!("stopped {}", args.dataflow)
    })
}

/// `astrs restart`'s arguments.
#[derive(Debug, Clone)]
pub struct RestartArgs {
    /// The dataflow to restart.
    pub dataflow: DataflowRef,
    /// Re-run the `build:` lines first.
    pub rebuild: bool,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Restarts a dataflow.
///
/// # Errors
///
/// As [`Client::request`], plus [`CliError::UnexpectedReply`].
pub fn restart(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &RestartArgs,
) -> Result<(), CliError> {
    let request = match &args.dataflow {
        DataflowRef::Id(dataflow) => ControlRequest::Restart {
            dataflow: *dataflow,
            rebuild: args.rebuild,
        },
        DataflowRef::Name(name) => ControlRequest::RestartByName {
            name: name.clone(),
            rebuild: args.rebuild,
        },
    };
    one_shot(out, endpoint, "restart", &request, args.json, |()| {
        format!("restarted {}", args.dataflow)
    })
}

/// `astrs destroy`'s arguments.
#[derive(Debug, Clone)]
pub struct DestroyArgs {
    /// The dataflow to destroy; every one when absent.
    pub dataflow: Option<DataflowRef>,
    /// Destroy even while dataflows are running.
    pub force: bool,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Destroys a dataflow, or the whole cluster's dataflows.
///
/// A named dataflow is stopped first and then cleaned: the protocol's
/// [`ControlRequest::Destroy`] is cluster-wide by design (§24.1), so
/// "destroy this one" is expressed as the two requests that mean it rather
/// than by inventing a variant the wire does not have.
///
/// # Errors
///
/// As [`Client::request`], plus [`CliError::UnexpectedReply`].
pub fn destroy(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &DestroyArgs,
) -> Result<(), CliError> {
    let Some(reference) = args.dataflow.clone() else {
        return one_shot(
            out,
            endpoint,
            "destroy",
            &ControlRequest::Destroy { force: args.force },
            args.json,
            |()| "destroyed every dataflow".to_owned(),
        );
    };

    let runtime = runtime()?;
    let dataflow = reference.clone();
    runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let stop_request = match &dataflow {
            DataflowRef::Id(id) => ControlRequest::Stop {
                dataflow: *id,
                grace: None,
            },
            DataflowRef::Name(name) => ControlRequest::StopByName {
                name: name.clone(),
                grace: None,
            },
        };
        client.request_ok("destroy", &stop_request).await?;
        if let DataflowRef::Id(id) = &dataflow {
            client
                .request_ok(
                    "destroy",
                    &ControlRequest::Clean {
                        dataflow: Some(*id),
                        artifacts: true,
                        logs: false,
                    },
                )
                .await?;
        }
        Ok::<(), CliError>(())
    })?;

    emit(
        out,
        args.json,
        &format!("destroyed {reference}"),
        || serde_json::json!({ "destroyed": reference.to_string() }),
    );
    Ok(())
}

/// `astrs clean`'s arguments.
#[derive(Debug, Clone)]
pub struct CleanArgs {
    /// The finished dataflow to clean; every finished one when absent.
    pub dataflow: Option<DataflowRef>,
    /// Also delete build artefacts.
    pub artifacts: bool,
    /// Also delete captured logs.
    pub logs: bool,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Reclaims finished dataflows' resources.
///
/// A named (rather than id'd) dataflow costs one `List` first: the wire's
/// [`ControlRequest::Clean`] takes an id or nothing at all, so the name a
/// user types has to become an id somewhere, and doing it here keeps
/// `astrs clean my-flow` working exactly like `astrs stop my-flow`.
///
/// # Errors
///
/// As [`Client::request`], plus [`CliError::UnknownDataflow`] when a name
/// resolves to nothing.
pub fn clean(out: &mut dyn Write, endpoint: &Endpoint, args: &CleanArgs) -> Result<(), CliError> {
    let runtime = runtime()?;
    let reference = args.dataflow.clone();
    let cleaned = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = match &reference {
            Some(reference) => Some(client.resolve(reference).await?),
            None => None,
        };
        client
            .request_ok(
                "clean",
                &ControlRequest::Clean {
                    dataflow,
                    artifacts: args.artifacts,
                    logs: args.logs,
                },
            )
            .await?;
        Ok::<Option<DataflowId>, CliError>(dataflow)
    })?;

    let text = cleaned.map_or_else(
        || "cleaned every finished dataflow".to_owned(),
        |id| format!("cleaned {id}"),
    );
    emit(
        out,
        args.json,
        &text,
        || serde_json::json!({ "ok": true, "message": text }),
    );
    Ok(())
}

/// Connects, sends one request that must answer `Ok`, and reports it.
fn one_shot(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    label: &'static str,
    request: &ControlRequest,
    json: bool,
    summary: impl FnOnce(()) -> String,
) -> Result<(), CliError> {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        client.request_ok(label, request).await
    })?;
    let text = summary(());
    emit(
        out,
        json,
        &text,
        || serde_json::json!({ "ok": true, "message": text }),
    );
    Ok(())
}

/// Writes either the human line or the JSON object.
fn emit(out: &mut dyn Write, json: bool, text: &str, value: impl FnOnce() -> serde_json::Value) {
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&value()).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let _ = writeln!(out, "{text}");
    }
}

/// Applies `astrs start --machine` placement overrides to `manifest`
/// (blueprint §8.3 `deploy.machine`, §17).
///
/// Two forms, and the difference between them is deliberate:
///
/// | Form | Meaning |
/// |---|---|
/// | `MACHINE` | the default for every node the manifest left unplaced |
/// | `NODE=MACHINE` | pins one node, *overriding* whatever the manifest said |
///
/// A bare machine is a **default**, not an override, because a manifest that
/// pins one node to the machine with the camera on it meant that, and a
/// deployment-time flag that silently moved it would break the graph. A
/// `NODE=MACHINE` is an override, because it names the node it is moving and
/// therefore cannot be an accident.
///
/// # Errors
///
/// [`CliError::BadArgument`] for an empty machine name, a `NODE=MACHINE`
/// naming a node the manifest does not declare, or more than one bare
/// machine (which of the two would be the default is not a question this
/// answers by picking).
pub fn apply_machine_overrides(
    manifest: &mut astrs_manifest::Manifest,
    specs: &[String],
) -> Result<(), CliError> {
    let bad = |value: &str, reason: &str| CliError::BadArgument {
        flag: "machine",
        value: value.to_owned(),
        reason: reason.to_owned(),
    };

    let mut default_machine: Option<&str> = None;
    let mut pinned: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for spec in specs {
        match spec.split_once('=') {
            Some((node, machine)) => {
                if node.is_empty() || machine.is_empty() {
                    return Err(bad(spec, "expected NODE=MACHINE, with both halves present"));
                }
                pinned.insert(node, machine);
            }
            None => {
                if spec.is_empty() {
                    return Err(bad(spec, "a machine name may not be empty"));
                }
                if default_machine.is_some_and(|existing| existing != spec.as_str()) {
                    return Err(bad(
                        spec,
                        "only one bare --machine may be given; use NODE=MACHINE to place                          individual nodes",
                    ));
                }
                default_machine = Some(spec);
            }
        }
    }

    let declared: std::collections::BTreeSet<&str> =
        manifest.nodes.iter().map(|node| node.id.as_str()).collect();
    for node in pinned.keys() {
        if !declared.contains(node) {
            return Err(bad(node, "the manifest declares no node with that id"));
        }
    }

    for node in &mut manifest.nodes {
        let wanted = pinned.get(node.id.as_str()).copied().or_else(|| {
            let already_placed = node
                .deploy
                .as_ref()
                .is_some_and(|deploy| deploy.machine.is_some());
            if already_placed {
                None
            } else {
                default_machine
            }
        });
        let Some(machine) = wanted else {
            continue;
        };
        node.deploy
            .get_or_insert_with(astrs_manifest::Deploy::default)
            .machine = Some(machine.to_owned());
    }
    Ok(())
}

/// The directory a manifest's relative paths resolve against.
fn manifest_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{AuthToken, NodeExitCause, NodeId, SubscriptionId, WireEncode};

    fn endpoint() -> Endpoint {
        Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            AuthToken::ZERO,
        )
    }

    /// A two-node manifest with one node already placed, so every
    /// override rule below has something to be right or wrong about.
    fn placed_manifest() -> astrs_manifest::Manifest {
        astrs_manifest::Manifest::from_yaml_str(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: planner
    path: ./planner
    deploy:
      machine: robot-9
    inputs:
      frames: camera/frames
",
        )
        .unwrap()
    }

    fn machine_of(manifest: &astrs_manifest::Manifest, node: &str) -> Option<String> {
        manifest
            .nodes
            .iter()
            .find(|candidate| candidate.id == node)
            .and_then(|candidate| candidate.deploy.as_ref())
            .and_then(|deploy| deploy.machine.clone())
    }

    #[test]
    fn no_machine_override_changes_nothing() {
        let mut manifest = placed_manifest();
        apply_machine_overrides(&mut manifest, &[]).unwrap();
        assert_eq!(machine_of(&manifest, "camera"), None);
        assert_eq!(machine_of(&manifest, "planner").as_deref(), Some("robot-9"));
    }

    #[test]
    fn a_bare_machine_places_only_the_unplaced_nodes() {
        let mut manifest = placed_manifest();
        apply_machine_overrides(&mut manifest, &["robot-1".to_owned()]).unwrap();
        assert_eq!(machine_of(&manifest, "camera").as_deref(), Some("robot-1"));
        assert_eq!(
            machine_of(&manifest, "planner").as_deref(),
            Some("robot-9"),
            "a bare --machine is a default, never an override"
        );
    }

    #[test]
    fn a_node_scoped_machine_overrides_the_manifest() {
        let mut manifest = placed_manifest();
        apply_machine_overrides(&mut manifest, &["planner=robot-2".to_owned()]).unwrap();
        assert_eq!(machine_of(&manifest, "planner").as_deref(), Some("robot-2"));
        assert_eq!(machine_of(&manifest, "camera"), None);
    }

    #[test]
    fn the_two_forms_combine() {
        let mut manifest = placed_manifest();
        apply_machine_overrides(
            &mut manifest,
            &["robot-1".to_owned(), "planner=robot-2".to_owned()],
        )
        .unwrap();
        assert_eq!(machine_of(&manifest, "camera").as_deref(), Some("robot-1"));
        assert_eq!(machine_of(&manifest, "planner").as_deref(), Some("robot-2"));
    }

    #[test]
    fn an_overridden_manifest_still_serializes_back_to_yaml() {
        let mut manifest = placed_manifest();
        apply_machine_overrides(&mut manifest, &["robot-1".to_owned()]).unwrap();
        let yaml = manifest.to_yaml().unwrap();
        let round_tripped = astrs_manifest::Manifest::from_yaml_str(&yaml).unwrap();
        assert_eq!(
            machine_of(&round_tripped, "camera").as_deref(),
            Some("robot-1"),
            "the coordinator is sent yaml, so the override has to survive it"
        );
    }

    #[test]
    fn an_unknown_node_is_a_typed_argument_error() {
        let mut manifest = placed_manifest();
        let err = apply_machine_overrides(&mut manifest, &["ghost=robot-1".to_owned()])
            .expect_err("no such node");
        assert!(matches!(
            err,
            CliError::BadArgument {
                flag: "machine",
                ..
            }
        ));
    }

    #[test]
    fn two_different_bare_machines_are_refused_rather_than_guessed_at() {
        let mut manifest = placed_manifest();
        let err =
            apply_machine_overrides(&mut manifest, &["robot-1".to_owned(), "robot-2".to_owned()])
                .expect_err("ambiguous");
        assert!(matches!(
            err,
            CliError::BadArgument {
                flag: "machine",
                ..
            }
        ));
        // The same one twice is not ambiguous at all.
        apply_machine_overrides(&mut manifest, &["robot-1".to_owned(), "robot-1".to_owned()])
            .unwrap();
    }

    #[test]
    fn a_malformed_pair_is_refused() {
        let mut manifest = placed_manifest();
        for spec in ["=robot-1", "planner=", ""] {
            assert!(
                apply_machine_overrides(&mut manifest, &[spec.to_owned()]).is_err(),
                "{spec:?} must be refused"
            );
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-lifecycle-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn result(status: DataflowStatus, failed: bool) -> DataflowResult {
        let mut result = DataflowResult::new(DataflowId::from_u128(1), HlcTimestamp::new(1, 0));
        result.status = status;
        if failed {
            result.record(
                NodeId::new("a").unwrap(),
                NodeExitCause::ExitCode { code: 1 },
            );
        }
        result
    }

    #[test]
    fn a_detached_start_always_exits_zero() {
        let report = StartReport {
            dataflow: DataflowId::from_u128(1),
            name: Some("flow".to_owned()),
            result: None,
        };
        assert_eq!(report.exit_code(), 0);
        assert!(report.to_json()["attached"] == false);
        assert!(start_summary(&report).contains("started"));
    }

    #[test]
    fn an_attached_start_reports_the_dataflow_s_own_severity() {
        for (status, failed, code) in [
            (DataflowStatus::Finished, false, 0),
            (DataflowStatus::Finished, true, 1),
            (DataflowStatus::Failed, true, 1),
            (DataflowStatus::Running, false, 2),
        ] {
            let report = StartReport {
                dataflow: DataflowId::from_u128(1),
                name: None,
                result: Some(result(status, failed)),
            };
            assert_eq!(report.exit_code(), code, "{status:?} failed={failed}");
        }
    }

    #[test]
    fn the_start_json_carries_the_exit_code_and_the_name() {
        let report = StartReport {
            dataflow: DataflowId::from_u128(7),
            name: Some("perception".to_owned()),
            result: Some(result(DataflowStatus::Failed, true)),
        };
        let json = report.to_json();
        assert_eq!(json["name"], "perception");
        assert_eq!(json["exit_code"], 1);
        assert_eq!(json["failed"], true);
    }

    #[test]
    fn start_validates_the_manifest_before_dialling_anything() {
        let dir = scratch("bad-manifest");
        let path = dir.join("d.yml");
        std::fs::write(
            &path,
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    inputs:\n      in: ghost/out\n",
        )
        .unwrap();
        // Port 1 would refuse instantly, so a `NoCluster` here would prove
        // the dial happened first; `Validation` proves it did not.
        let error = start(&mut Vec::new(), &endpoint(), &StartArgs::new(path)).unwrap_err();
        assert!(matches!(error, CliError::Validation(_)), "{error}");
    }

    #[test]
    fn start_reports_a_missing_manifest_as_a_manifest_error() {
        let dir = scratch("absent");
        let error = start(
            &mut Vec::new(),
            &endpoint(),
            &StartArgs::new(dir.join("nope.yml")),
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Manifest(_)), "{error}");
    }

    #[test]
    fn stop_by_id_and_by_name_are_different_requests() {
        // The classification the whole module hangs off, asserted directly:
        // an id becomes `Stop`, anything else becomes `StopByName`.
        let id = DataflowId::generate();
        assert!(matches!(
            DataflowRef::parse(&id.to_string()),
            DataflowRef::Id(_)
        ));
        assert!(matches!(
            DataflowRef::parse("perception"),
            DataflowRef::Name(_)
        ));
    }

    #[test]
    fn every_lifecycle_verb_reports_a_dead_cluster_rather_than_hanging() {
        let endpoint = endpoint();
        let checks: Vec<Box<dyn Fn() -> Result<(), CliError>>> = vec![
            Box::new(|| {
                stop(
                    &mut Vec::new(),
                    &endpoint,
                    &StopArgs {
                        dataflow: DataflowRef::Name("f".to_owned()),
                        grace: None,
                        json: false,
                    },
                )
            }),
            Box::new(|| {
                restart(
                    &mut Vec::new(),
                    &endpoint,
                    &RestartArgs {
                        dataflow: DataflowRef::Name("f".to_owned()),
                        rebuild: false,
                        json: false,
                    },
                )
            }),
            Box::new(|| {
                destroy(
                    &mut Vec::new(),
                    &endpoint,
                    &DestroyArgs {
                        dataflow: None,
                        force: true,
                        json: false,
                    },
                )
            }),
            Box::new(|| {
                clean(
                    &mut Vec::new(),
                    &endpoint,
                    &CleanArgs {
                        dataflow: None,
                        artifacts: false,
                        logs: false,
                        json: false,
                    },
                )
            }),
        ];
        for check in checks {
            let error = check().unwrap_err();
            assert!(matches!(error, CliError::NoCluster { .. }), "{error}");
        }
    }

    /// The plain rendering style every log test below asserts against.
    fn plain() -> LogStyle {
        LogStyle {
            color: false,
            prefix_width: 0,
            elapsed: false,
        }
    }

    /// One node record, at `stamp`, saying `message`.
    fn record(stamp: u64, message: &str) -> LogRecord {
        LogRecord::new(HlcTimestamp::new(stamp, 0), LogLevel::Info, message)
            .with_node(NodeId::new("cam").unwrap())
    }

    /// `record` wrapped in the pushed frame a subscription delivers.
    fn log_frame(record: LogRecord) -> astrs_wire::Frame {
        let log = LogFrame {
            subscription: SubscriptionId::new(7),
            record,
        };
        astrs_wire::Frame::new(
            FrameKind::Log,
            astrs_wire::FrameFlags::EMPTY,
            log.encode_to_vec().unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn a_non_log_frame_is_ignored_by_the_log_tail() {
        let frame = astrs_wire::Frame::new(
            FrameKind::ControlReply,
            astrs_wire::FrameFlags::EMPTY,
            b"not a log".to_vec(),
        )
        .unwrap();
        let mut out = Vec::new();
        LogTail::new(LogFilter::new(), LogStyle::default()).push_frame(&mut out, &frame);
        assert!(out.is_empty());
    }

    #[test]
    fn a_log_frame_is_rendered_through_the_shared_filter() {
        let frame = log_frame(record(1, "hello"));

        let mut out = Vec::new();
        LogTail::new(LogFilter::new(), plain()).push_frame(&mut out, &frame);
        assert_eq!(String::from_utf8(out).unwrap().trim_end(), "[cam] hello");

        // …and a filter that excludes it prints nothing.
        let mut out = Vec::new();
        LogTail::new(
            LogFilter::new().with_min_level(LogLevel::Error),
            LogStyle::default(),
        )
        .push_frame(&mut out, &frame);
        assert!(out.is_empty());
    }

    #[test]
    fn a_malformed_log_frame_is_dropped_rather_than_ending_the_stream() {
        let frame =
            astrs_wire::Frame::new(FrameKind::Log, astrs_wire::FrameFlags::EMPTY, vec![0xFF; 4])
                .unwrap();
        let mut out = Vec::new();
        LogTail::new(LogFilter::new(), LogStyle::default()).push_frame(&mut out, &frame);
        assert!(out.is_empty());
    }

    #[test]
    fn the_closing_pull_prints_only_what_the_live_stream_missed() {
        // The push path delivered the second line; the history ring holds
        // both. An attached start must end up having printed each exactly
        // once, in timestamp order for the part it backfilled.
        let mut tail = LogTail::new(LogFilter::new(), plain());
        let mut out = Vec::new();
        tail.push_frame(&mut out, &log_frame(record(2, "second")));
        tail.backfill(
            &mut out,
            vec![record(2, "second"), record(1, "first"), record(3, "third")],
        );

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, ["[cam] second", "[cam] first", "[cam] third"]);
    }

    #[test]
    fn a_backfill_with_nothing_new_prints_nothing() {
        let mut tail = LogTail::new(LogFilter::new(), plain());
        let mut out = Vec::new();
        tail.push_frame(&mut out, &log_frame(record(1, "only")));
        out.clear();
        tail.backfill(&mut out, vec![record(1, "only")]);
        assert!(
            out.is_empty(),
            "the whole point of the seen-set: a short run is backfilled, a long one is not reprinted"
        );
    }

    #[test]
    fn two_nodes_logging_the_same_words_at_the_same_stamp_are_two_lines() {
        // The de-duplication key includes the node, so a fan-out of identical
        // messages is not collapsed into one.
        let mut tail = LogTail::new(LogFilter::new(), plain());
        let mut out = Vec::new();
        let stamp = HlcTimestamp::new(4, 0);
        tail.backfill(
            &mut out,
            vec![
                LogRecord::new(stamp, LogLevel::Info, "ready").with_node(NodeId::new("a").unwrap()),
                LogRecord::new(stamp, LogLevel::Info, "ready").with_node(NodeId::new("b").unwrap()),
            ],
        );
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 2);
    }
}
