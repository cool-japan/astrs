//! `astrs list`, `astrs logs [-f]` and the dataflow half of `astrs status`
//! (blueprint §17's Monitoring row).
//!
//! ```text
//!   astrs list          ─► List{all}            ─► DataflowList  ─► table
//!   astrs status <ref>  ─► Info{include_nodes}  ─► DataflowList  ─► table
//!                          Check{dataflow}      ─► DataflowResult (terminal)
//!   astrs logs <ref>    ─► Logs{query}          ─► Logs{records}  ─► lines
//!   astrs logs -f       ─► LogSubscribe         ─► Log frames ────► lines
//!                          TopicUnsubscribe     ◄── Ctrl-C
//! ```
//!
//! # Read verbs, deliberately
//!
//! Blueprint §16 splits the coordinator API into read verbs (list, logs,
//! topic) and mutating ones (start, stop, param set). Everything in this
//! module is on the read side, so none of it *requires* a token: a
//! development coordinator started without one is still inspectable, which
//! is exactly the setup a first-time user has. A coordinator that does
//! demand a token refuses the greeting, and that refusal is reported with
//! the address that was dialled.
//!
//! # `logs` and `logs -f` are two different requests
//!
//! [`astrs_wire::ControlRequest::Logs`] takes a **required**
//! [`astrs_wire::DataflowId`] and answers with a bounded batch;
//! [`astrs_wire::ControlRequest::LogSubscribe`] takes an *optional* one and
//! pushes frames until the subscription ends. So `astrs logs -f` with no
//! dataflow named follows the whole cluster, while `astrs logs` with none
//! resolves the single dataflow that exists and refuses to guess between
//! two — see [`crate::command::client::Client::sole_dataflow`].

use std::io::Write;
use std::time::Instant;

use astrs_wire::{
    ControlReply, ControlRequest, DataflowId, DataflowResult, DataflowSummary, LogLevel, LogQuery,
    LogRecord, NodeId, NodeInfo,
};

use crate::command::client::{
    Client, DataflowRef, Endpoint, new_subscription_id, reply_name, runtime,
};
use crate::command::log_stream::{LogFilter, LogStyle, StreamItem, prefix_width, render};
use crate::command::signals::Signals;
use crate::error::CliError;

/// `astrs list`'s arguments.
#[derive(Debug, Clone, Default)]
pub struct ListArgs {
    /// Include dataflows that have already finished.
    pub all: bool,
    /// Emit JSON rather than a table.
    pub json: bool,
}

/// What `astrs list` found.
#[derive(Debug, Clone)]
pub struct ListReport {
    /// One row per dataflow, in the order the coordinator returned them.
    pub dataflows: Vec<DataflowSummary>,
}

impl ListReport {
    /// The human table.
    ///
    /// Fixed-width columns rather than a box-drawing table: `astrs list |
    /// grep` and `astrs list | awk '{print $1}'` are what operators
    /// actually do with this, and both break the moment a border character
    /// appears.
    #[must_use]
    pub fn table(&self) -> String {
        if self.dataflows.is_empty() {
            return "no dataflows (start one with `astrs start <manifest>`)".to_owned();
        }
        let mut lines = vec![format!(
            "{:<36} {:<20} {:<10} {:>5} {:>7}  {}",
            "ID", "NAME", "STATUS", "NODES", "RUNNING", "DAEMONS"
        )];
        for summary in &self.dataflows {
            lines.push(format!(
                "{:<36} {:<20} {:<10} {:>5} {:>7}  {}",
                summary.id.to_string(),
                summary.name.clone().unwrap_or_else(|| "-".to_owned()),
                summary.status.as_str(),
                summary.node_count,
                summary.running_nodes,
                if summary.daemons.is_empty() {
                    "-".to_owned()
                } else {
                    summary
                        .daemons
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                }
            ));
        }
        lines.join("\n")
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "count": self.dataflows.len(),
            "dataflows": self
                .dataflows
                .iter()
                .map(summary_json)
                .collect::<Vec<_>>(),
        })
    }
}

/// One dataflow summary as JSON.
fn summary_json(summary: &DataflowSummary) -> serde_json::Value {
    serde_json::json!({
        "id": summary.id.to_string(),
        "name": summary.name,
        "status": summary.status.as_str(),
        "nodes": summary.node_count,
        "running": summary.running_nodes,
        "daemons": summary
            .daemons
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "started_at": summary.started_at.map(|stamp| stamp.to_string()),
    })
}

/// Lists the cluster's dataflows.
///
/// # Errors
///
/// - [`CliError::NoCluster`] if nothing is listening.
/// - As [`Client::request`] for a refusal or a protocol failure.
pub fn list(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ListArgs,
) -> Result<ListReport, CliError> {
    let runtime = runtime()?;
    let dataflows = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        client.list_dataflows(args.all).await
    })?;
    let report = ListReport { dataflows };
    emit(out, args.json, &report.table(), || report.to_json());
    Ok(report)
}

/// `astrs logs`'s arguments.
#[derive(Debug, Clone, Default)]
pub struct LogsArgs {
    /// The dataflow to read; resolved or followed cluster-wide when absent.
    pub dataflow: Option<DataflowRef>,
    /// Only this node's records.
    pub node: Option<String>,
    /// Hide records below this level.
    pub level: Option<LogLevel>,
    /// At most this many records in a dump.
    pub limit: Option<u32>,
    /// Keep streaming instead of dumping.
    pub follow: bool,
    /// Emit JSON rather than rendered lines.
    pub json: bool,
    /// Colorize rendered lines.
    pub color: bool,
}

/// What one `astrs logs` did.
#[derive(Debug, Clone)]
pub struct LogsReport {
    /// How many records were printed.
    pub printed: usize,
    /// Whether a dump was cut short by the query's limit.
    pub truncated: bool,
    /// Whether this was a `-f` stream rather than a dump.
    pub followed: bool,
}

impl LogsReport {
    /// The `--json` form (a `-f` stream prints records as it goes, so this
    /// is the trailer rather than the payload).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "printed": self.printed,
            "truncated": self.truncated,
            "followed": self.followed,
        })
    }
}

/// Dumps, or follows, a dataflow's logs.
///
/// # Errors
///
/// - [`CliError::BadArgument`] if `--node` is not a usable node id.
/// - [`CliError::UnknownDataflow`] if no dataflow was named and the cluster
///   does not have exactly one.
/// - [`CliError::NoCluster`] if nothing is listening.
/// - As [`Client::request`] otherwise.
pub fn logs(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &LogsArgs,
) -> Result<LogsReport, CliError> {
    let node = parse_node(args.node.as_deref())?;
    let mut query = LogQuery::new();
    if let Some(level) = args.level {
        query = query.with_min_level(level);
    }
    query.limit = args.limit.or(query.limit);

    let filter = LogFilter::new()
        .with_min_level(args.level.unwrap_or(LogLevel::Trace))
        .with_node(node.clone());
    let runtime = runtime()?;

    if args.follow {
        return runtime.block_on(follow(out, endpoint, args, node, query, &filter));
    }

    let (records, truncated) = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = match &args.dataflow {
            Some(reference) => client.resolve(reference).await?,
            None => client.sole_dataflow(true).await?,
        };
        match client
            .request(
                "logs",
                &ControlRequest::Logs {
                    dataflow,
                    node: node.clone(),
                    query,
                },
            )
            .await?
        {
            ControlReply::Logs { records, truncated } => Ok((records, truncated)),
            other => Err(CliError::UnexpectedReply {
                request: "logs",
                reply: reply_name(&other),
            }),
        }
    })?;

    if args.json {
        let rendered: Vec<serde_json::Value> = records.iter().map(record_json).collect();
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "truncated": truncated,
                "records": rendered,
            }))
            .unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let style = LogStyle {
            color: args.color,
            prefix_width: prefix_width(
                records
                    .iter()
                    .filter_map(|record| record.node.as_ref().map(NodeId::as_str)),
            ),
            elapsed: false,
        };
        for record in &records {
            let item = StreamItem::Record(Box::new(record.clone()));
            if filter.accepts(&item) {
                let _ = writeln!(out, "{}", render(&item, style, std::time::Duration::ZERO));
            }
        }
        if truncated {
            let _ = writeln!(
                out,
                "(truncated: the coordinator's page limit was reached — narrow the query or pass --limit)"
            );
        }
        let _ = out.flush();
    }
    Ok(LogsReport {
        printed: records.len(),
        truncated,
        followed: false,
    })
}

/// The `-f` path: subscribe, print pushed frames, unsubscribe on Ctrl-C.
///
/// The subscription is closed explicitly rather than left to the socket
/// closing, so a coordinator that outlives this CLI does not keep fanning
/// records at a session that stopped reading them.
async fn follow(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &LogsArgs,
    node: Option<NodeId>,
    query: LogQuery,
    filter: &LogFilter,
) -> Result<LogsReport, CliError> {
    let mut client = Client::connect(endpoint).await?;
    let dataflow: Option<DataflowId> = match &args.dataflow {
        Some(reference) => Some(client.resolve(reference).await?),
        None => None,
    };
    let subscription = new_subscription_id();
    client
        .request_ok(
            "logs -f",
            &ControlRequest::LogSubscribe {
                dataflow,
                node,
                query,
                subscription,
            },
        )
        .await?;

    let style = LogStyle {
        color: args.color,
        prefix_width: prefix_width(std::iter::empty()),
        elapsed: false,
    };
    let started = Instant::now();
    let mut printed = 0usize;
    let mut signals = Signals::install();
    let mut interrupted = false;

    loop {
        // `client` is borrowed by the frame arm for the whole `select!`, so
        // the interrupt arm records the fact and the unsubscribe happens
        // after the loop rather than inside it.
        tokio::select! {
            frame = client.next_frame() => {
                match frame? {
                    Some(frame) => {
                        printed += usize::from(print_frame(out, &frame, filter, style, started, args.json));
                    }
                    // The coordinator closed the connection: the cluster
                    // went down, which ends a tail rather than failing it.
                    None => break,
                }
            }
            () = signals.next() => {
                interrupted = true;
                break;
            }
        }
    }

    if interrupted {
        let _ = client
            .request_ok(
                "logs -f",
                &ControlRequest::TopicUnsubscribe { subscription },
            )
            .await;
    }
    let report = LogsReport {
        printed,
        truncated: false,
        followed: true,
    };
    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&report.to_json()).unwrap_or_else(|_| "{}".to_owned())
        );
        let _ = out.flush();
    }
    Ok(report)
}

/// Prints one pushed frame, returning whether anything was written.
fn print_frame(
    out: &mut dyn Write,
    frame: &astrs_wire::Frame,
    filter: &LogFilter,
    style: LogStyle,
    started: Instant,
    json: bool,
) -> bool {
    use astrs_wire::{FrameKind, LogFrame, WireDecode};

    if frame.kind() != FrameKind::Log {
        return false;
    }
    let Ok(log) = LogFrame::decode_exact(frame.payload()) else {
        // One malformed record must not end a tail that is otherwise fine.
        return false;
    };
    let item = StreamItem::Record(Box::new(log.record));
    if !filter.accepts(&item) {
        return false;
    }
    let line = if json {
        match &item {
            StreamItem::Record(record) => {
                serde_json::to_string(&record_json(record)).unwrap_or_default()
            }
            StreamItem::Notice { message, .. } => message.clone(),
        }
    } else {
        render(&item, style, started.elapsed())
    };
    if writeln!(out, "{line}").is_err() {
        return false;
    }
    let _ = out.flush();
    true
}

/// One log record as JSON.
fn record_json(record: &LogRecord) -> serde_json::Value {
    serde_json::json!({
        "timestamp": record.timestamp.to_string(),
        "level": level_word(record.level),
        "dataflow": record.dataflow.map(|id| id.to_string()),
        "node": record.node.as_ref().map(NodeId::as_str),
        "target": record.target,
        "message": record.message,
        "fields": record.fields,
    })
}

/// A stable lowercase word per level, for JSON output.
const fn level_word(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
        // `LogLevel` is `#[non_exhaustive]`; an unknown severity keeps its
        // own word rather than being reported as one it is not.
        _ => "unknown",
    }
}

/// `astrs status <dataflow>`'s arguments.
#[derive(Debug, Clone, Default)]
pub struct DataflowStatusArgs {
    /// The dataflow to describe; the cluster's only one when absent.
    pub dataflow: Option<DataflowRef>,
    /// Emit JSON rather than a table.
    pub json: bool,
}

/// What `astrs status <dataflow>` found.
#[derive(Debug, Clone)]
pub struct DataflowStatusReport {
    /// The dataflow's row.
    pub summary: Option<DataflowSummary>,
    /// One row per node.
    pub nodes: Vec<NodeInfo>,
    /// The final verdict, when the dataflow has already reached one.
    pub result: Option<DataflowResult>,
}

impl DataflowStatusReport {
    /// `0` while the dataflow is healthy or still running, `1` once it has
    /// finished with a failure — so a script can poll it.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(
            self.result
                .as_ref()
                .is_some_and(DataflowResult::has_failures),
        )
    }

    /// The human table.
    #[must_use]
    pub fn table(&self) -> String {
        let mut lines = Vec::new();
        if let Some(summary) = &self.summary {
            lines.push(format!(
                "{} ({}) {} — {}/{} node(s) running",
                summary.id,
                summary.name.clone().unwrap_or_else(|| "unnamed".to_owned()),
                summary.status.as_str(),
                summary.running_nodes,
                summary.node_count
            ));
        }
        if !self.nodes.is_empty() {
            lines.push(format!(
                "{:<20} {:<10} {:>8} {:>9}  {}",
                "NODE", "STATE", "PID", "RESTARTS", "EXIT"
            ));
            for node in &self.nodes {
                lines.push(format!(
                    "{:<20} {:<10} {:>8} {:>9}  {}",
                    node.node.as_str(),
                    node.state.as_str(),
                    node.pid
                        .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
                    node.restart_count,
                    node.exit_cause
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), ToString::to_string)
                ));
            }
        }
        if let Some(result) = &self.result {
            lines.push(format!(
                "final: {} ({} node result(s), {} failed)",
                result.status.as_str(),
                result.node_results.len(),
                result
                    .node_results
                    .values()
                    .filter(|cause| cause.is_failure())
                    .count()
            ));
        }
        if lines.is_empty() {
            lines.push("the coordinator knows nothing about that dataflow".to_owned());
        }
        lines.join("\n")
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "dataflow": self.summary.as_ref().map(summary_json),
            "nodes": self
                .nodes
                .iter()
                .map(|node| serde_json::json!({
                    "node": node.node.as_str(),
                    "state": node.state.as_str(),
                    "pid": node.pid,
                    "generation": node.generation,
                    "restarts": node.restart_count,
                    "daemon": node.daemon.to_string(),
                    "exit_cause": node.exit_cause.as_ref().map(ToString::to_string),
                }))
                .collect::<Vec<_>>(),
            "final": self.result.as_ref().map(|result| serde_json::json!({
                "status": result.status.as_str(),
                "failed": result.has_failures(),
                "message": result.message,
            })),
            "exit_code": self.exit_code(),
        })
    }
}

/// Describes one dataflow: its row, its nodes, and its verdict if it has
/// one.
///
/// # Errors
///
/// - [`CliError::UnknownDataflow`] if the reference resolves to nothing.
/// - [`CliError::NoCluster`] if nothing is listening.
/// - As [`Client::request`] otherwise.
pub fn dataflow_status(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &DataflowStatusArgs,
) -> Result<DataflowStatusReport, CliError> {
    let runtime = runtime()?;
    let report = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = match &args.dataflow {
            Some(reference) => client.resolve(reference).await?,
            None => client.sole_dataflow(true).await?,
        };
        let (summary, nodes) = match client
            .request(
                "status",
                &ControlRequest::Info {
                    dataflow,
                    include_nodes: true,
                },
            )
            .await?
        {
            ControlReply::DataflowList { dataflows, nodes } => {
                (dataflows.into_iter().next(), nodes)
            }
            other => {
                return Err(CliError::UnexpectedReply {
                    request: "status",
                    reply: reply_name(&other),
                });
            }
        };
        let result = match client
            .request(
                "status",
                &ControlRequest::Check {
                    dataflow: Some(dataflow),
                },
            )
            .await?
        {
            ControlReply::DataflowResult { result } => Some(*result),
            // `Ok` is the coordinator's answer for "known, not finished".
            _ => None,
        };
        Ok::<DataflowStatusReport, CliError>(DataflowStatusReport {
            summary,
            nodes,
            result,
        })
    })?;
    emit(out, args.json, &report.table(), || report.to_json());
    Ok(report)
}

/// Validates a `--node` argument.
///
/// # Errors
///
/// [`CliError::BadArgument`] naming what a node id may contain.
fn parse_node(node: Option<&str>) -> Result<Option<NodeId>, CliError> {
    let Some(text) = node else {
        return Ok(None);
    };
    NodeId::new(text)
        .map(Some)
        .map_err(|error| CliError::BadArgument {
            flag: "node",
            value: text.to_owned(),
            reason: error.to_string(),
        })
}

/// Writes either the human text or the JSON object.
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
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{AuthToken, DataflowStatus, NodeExitCause, NodeRunState};

    fn endpoint() -> Endpoint {
        // Port 1 on loopback: never bound by this suite, refused instantly.
        Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            AuthToken::ZERO,
        )
    }

    fn summary(name: Option<&str>, status: DataflowStatus) -> DataflowSummary {
        DataflowSummary {
            id: DataflowId::from_u128(7),
            name: name.map(ToOwned::to_owned),
            status,
            daemons: Vec::new(),
            node_count: 3,
            running_nodes: 2,
            started_at: Some(HlcTimestamp::new(1, 0)),
        }
    }

    #[test]
    fn an_empty_list_says_how_to_start_something() {
        let report = ListReport {
            dataflows: Vec::new(),
        };
        assert!(report.table().contains("astrs start"), "{}", report.table());
        assert_eq!(report.to_json()["count"], 0);
    }

    #[test]
    fn a_list_table_has_one_header_and_one_row_per_dataflow() {
        let report = ListReport {
            dataflows: vec![
                summary(Some("perception"), DataflowStatus::Running),
                summary(None, DataflowStatus::Finished),
            ],
        };
        let table = report.table();
        assert_eq!(table.lines().count(), 3, "{table}");
        assert!(table.contains("perception"), "{table}");
        assert!(table.contains("running"), "{table}");
        assert!(table.contains("finished"), "{table}");
    }

    #[test]
    fn list_json_carries_every_row() {
        let report = ListReport {
            dataflows: vec![summary(Some("perception"), DataflowStatus::Running)],
        };
        let json = report.to_json();
        assert_eq!(json["count"], 1);
        assert_eq!(json["dataflows"][0]["name"], "perception");
        assert_eq!(json["dataflows"][0]["status"], "running");
        assert_eq!(json["dataflows"][0]["nodes"], 3);
    }

    #[test]
    fn a_node_filter_must_be_a_usable_node_id() {
        assert!(parse_node(None).unwrap().is_none());
        assert_eq!(
            parse_node(Some("cam"))
                .unwrap()
                .map(|id| id.as_str().to_owned()),
            Some("cam".to_owned())
        );
        let error = parse_node(Some("not a node")).unwrap_err();
        match error {
            CliError::BadArgument { flag, .. } => assert_eq!(flag, "node"),
            other => panic!("expected BadArgument, got {other}"),
        }
    }

    #[test]
    fn every_level_has_a_stable_json_word() {
        assert_eq!(level_word(LogLevel::Trace), "trace");
        assert_eq!(level_word(LogLevel::Warn), "warn");
        assert_eq!(level_word(LogLevel::Error), "error");
    }

    #[test]
    fn a_record_renders_every_field_a_script_needs() {
        let record = LogRecord::new(HlcTimestamp::new(3, 1), LogLevel::Warn, "slow frame")
            .with_node(NodeId::new("cam").unwrap());
        let json = record_json(&record);
        assert_eq!(json["level"], "warn");
        assert_eq!(json["node"], "cam");
        assert_eq!(json["message"], "slow frame");
        assert!(json["timestamp"].is_string());
    }

    #[test]
    fn a_dataflow_status_table_lists_nodes_and_the_final_verdict() {
        let mut result = DataflowResult::new(DataflowId::from_u128(7), HlcTimestamp::new(4, 0));
        result.status = DataflowStatus::Failed;
        result.record(
            NodeId::new("cam").unwrap(),
            NodeExitCause::ExitCode { code: 3 },
        );
        let report = DataflowStatusReport {
            summary: Some(summary(Some("perception"), DataflowStatus::Failed)),
            nodes: vec![NodeInfo {
                dataflow: DataflowId::from_u128(7),
                node: NodeId::new("cam").unwrap(),
                daemon: astrs_wire::DaemonId::generate(None),
                state: NodeRunState::Failed,
                pid: Some(4242),
                generation: 1,
                restart_count: 2,
                inputs: std::collections::BTreeMap::new(),
                outputs: std::collections::BTreeMap::new(),
                started_at: None,
                exit_cause: Some(NodeExitCause::ExitCode { code: 3 }),
            }],
            result: Some(result),
        };

        let table = report.table();
        assert!(table.contains("perception"), "{table}");
        assert!(table.contains("cam"), "{table}");
        assert!(table.contains("4242"), "{table}");
        assert!(table.contains("final: failed"), "{table}");
        assert_eq!(report.exit_code(), 1);

        let json = report.to_json();
        assert_eq!(json["nodes"][0]["restarts"], 2);
        assert_eq!(json["final"]["failed"], true);
        assert_eq!(json["exit_code"], 1);
    }

    #[test]
    fn a_dataflow_with_no_verdict_yet_exits_zero() {
        let report = DataflowStatusReport {
            summary: Some(summary(Some("perception"), DataflowStatus::Running)),
            nodes: Vec::new(),
            result: None,
        };
        assert_eq!(report.exit_code(), 0);
        assert!(report.table().contains("running"));
    }

    #[test]
    fn an_empty_status_report_says_the_coordinator_knows_nothing() {
        let report = DataflowStatusReport {
            summary: None,
            nodes: Vec::new(),
            result: None,
        };
        assert!(
            report.table().contains("knows nothing"),
            "{}",
            report.table()
        );
    }

    #[test]
    fn a_pushed_log_frame_is_rendered_and_a_filtered_one_is_not() {
        use astrs_wire::{Frame, FrameFlags, FrameKind, LogFrame, SubscriptionId, WireEncode};

        let record = LogRecord::new(HlcTimestamp::new(2, 0), LogLevel::Info, "frame 1")
            .with_node(NodeId::new("cam").unwrap());
        let frame = Frame::new(
            FrameKind::Log,
            FrameFlags::EMPTY,
            LogFrame::new(SubscriptionId::new(3), record)
                .encode_to_vec()
                .unwrap(),
        )
        .unwrap();
        let style = LogStyle {
            color: false,
            prefix_width: 0,
            elapsed: false,
        };

        let mut out = Vec::new();
        assert!(print_frame(
            &mut out,
            &frame,
            &LogFilter::new(),
            style,
            Instant::now(),
            false
        ));
        assert_eq!(String::from_utf8(out).unwrap().trim_end(), "[cam] frame 1");

        // `--json` prints the same record as one object per line.
        let mut out = Vec::new();
        assert!(print_frame(
            &mut out,
            &frame,
            &LogFilter::new(),
            style,
            Instant::now(),
            true
        ));
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).unwrap().trim_end()).unwrap();
        assert_eq!(value["node"], "cam");
        assert_eq!(value["message"], "frame 1");

        // A filter that excludes it writes nothing…
        let mut out = Vec::new();
        assert!(!print_frame(
            &mut out,
            &frame,
            &LogFilter::new().with_min_level(LogLevel::Error),
            style,
            Instant::now(),
            false
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn a_frame_that_is_not_a_log_or_does_not_decode_is_skipped_not_fatal() {
        use astrs_wire::{Frame, FrameFlags, FrameKind};

        let style = LogStyle::default();
        let other = Frame::new(
            FrameKind::ControlReply,
            FrameFlags::EMPTY,
            b"not a log".to_vec(),
        )
        .unwrap();
        let mut out = Vec::new();
        assert!(!print_frame(
            &mut out,
            &other,
            &LogFilter::new(),
            style,
            Instant::now(),
            false
        ));

        let malformed = Frame::new(FrameKind::Log, FrameFlags::EMPTY, vec![0xFF; 4]).unwrap();
        assert!(!print_frame(
            &mut out,
            &malformed,
            &LogFilter::new(),
            style,
            Instant::now(),
            false
        ));
        assert!(out.is_empty(), "a tail survives one bad record");
    }

    #[test]
    fn every_read_verb_reports_a_dead_cluster_rather_than_hanging() {
        let endpoint = endpoint();
        let list_error = list(&mut Vec::new(), &endpoint, &ListArgs::default()).unwrap_err();
        assert!(
            matches!(list_error, CliError::NoCluster { .. }),
            "{list_error}"
        );

        let logs_error = logs(&mut Vec::new(), &endpoint, &LogsArgs::default()).unwrap_err();
        assert!(
            matches!(logs_error, CliError::NoCluster { .. }),
            "{logs_error}"
        );

        let status_error =
            dataflow_status(&mut Vec::new(), &endpoint, &DataflowStatusArgs::default())
                .unwrap_err();
        assert!(
            matches!(status_error, CliError::NoCluster { .. }),
            "{status_error}"
        );
    }

    #[test]
    fn a_bad_node_filter_is_refused_before_anything_is_dialled() {
        let args = LogsArgs {
            node: Some("not a node".to_owned()),
            ..LogsArgs::default()
        };
        let error = logs(&mut Vec::new(), &endpoint(), &args).unwrap_err();
        assert!(matches!(error, CliError::BadArgument { .. }), "{error}");
    }
}
