//! `astrs record start/stop` (blueprint §14, §17).
//!
//! ```text
//!   astrs record start <flow> out.arec [--only n/o]* [--overwrite]
//!       ─► RecordStart{dataflow, path, ports, overwrite} ─► Ok
//!   astrs record stop <flow>
//!       ─► RecordStop{dataflow}                          ─► Ok
//! ```
//!
//! Both verbs are thin: `astrs-coordinator::handlers::misc` already does
//! the real work (synthesizing a [`astrs_wire::NodeSource::Recorder`]
//! node wired to the requested ports and dynamically spawning it, the
//! same mechanism `astrs node add` uses). This module's job is only to
//! turn CLI flags into the wire request and the reply into a line of
//! output.

use std::io::Write;

use astrs_wire::{ControlRequest, PortRef};

use crate::command::client::{Client, DataflowRef, Endpoint, runtime};
use crate::error::CliError;

/// `astrs record start` arguments, independent of `clap`.
#[derive(Debug, Clone)]
pub struct StartArgs {
    /// The dataflow id (or name) to record.
    pub dataflow: String,
    /// The `.arec` file to write.
    pub output: String,
    /// `node/output` ports to restrict recording to; every declared
    /// output of every node when empty (mirrors
    /// [`astrs_wire::ControlRequest::RecordStart`]'s own "empty records
    /// every port" contract).
    pub only: Vec<String>,
    /// Overwrite an already-recording session at the same path.
    pub overwrite: bool,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Starts recording a dataflow.
///
/// # Errors
///
/// - [`CliError::BadArgument`] if an `--only` entry is not a well-formed
///   `node/output` string.
/// - [`CliError::UnknownDataflow`] if the dataflow reference resolves to
///   nothing.
/// - As [`Client::request_ok`] otherwise.
pub fn start(out: &mut dyn Write, endpoint: &Endpoint, args: &StartArgs) -> Result<(), CliError> {
    let ports = parse_ports(&args.only)?;
    let reference = DataflowRef::parse(&args.dataflow);
    let runtime = runtime()?;
    let dataflow = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&reference).await?;
        client
            .request_ok(
                "record start",
                &ControlRequest::RecordStart {
                    dataflow,
                    path: args.output.clone(),
                    ports: ports.clone(),
                    overwrite: args.overwrite,
                },
            )
            .await?;
        Ok::<_, CliError>(dataflow)
    })?;

    if args.json {
        emit_json(
            out,
            &serde_json::json!({
                "dataflow": dataflow.to_string(),
                "output": args.output,
                "ports": ports.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
        );
    } else {
        let _ = writeln!(out, "recording {dataflow} to {}", args.output);
        let _ = out.flush();
    }
    Ok(())
}

/// `astrs record stop` arguments, independent of `clap`.
#[derive(Debug, Clone)]
pub struct StopArgs {
    /// The dataflow id (or name) whose recording should stop.
    pub dataflow: String,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Stops a dataflow's in-progress recording.
///
/// # Errors
///
/// - [`CliError::UnknownDataflow`] if the dataflow reference resolves to
///   nothing.
/// - [`CliError::Refused`] if the dataflow was not recording (the
///   coordinator answers `RecordStop` for a non-recording dataflow with
///   an error, not a silent no-op — see `handlers::misc::record_stop`).
/// - As [`Client::request_ok`] otherwise.
pub fn stop(out: &mut dyn Write, endpoint: &Endpoint, args: &StopArgs) -> Result<(), CliError> {
    let reference = DataflowRef::parse(&args.dataflow);
    let runtime = runtime()?;
    let dataflow = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&reference).await?;
        client
            .request_ok("record stop", &ControlRequest::RecordStop { dataflow })
            .await?;
        Ok::<_, CliError>(dataflow)
    })?;

    if args.json {
        emit_json(
            out,
            &serde_json::json!({ "dataflow": dataflow.to_string() }),
        );
    } else {
        let _ = writeln!(out, "stopped recording {dataflow}");
        let _ = out.flush();
    }
    Ok(())
}

/// Parses every `--only` entry into a [`PortRef`].
///
/// # Errors
///
/// [`CliError::BadArgument`] naming the first entry that does not parse.
fn parse_ports(only: &[String]) -> Result<Vec<PortRef>, CliError> {
    only.iter()
        .map(|text| {
            text.parse::<PortRef>()
                .map_err(|error| CliError::BadArgument {
                    flag: "only",
                    value: text.clone(),
                    reason: error.to_string(),
                })
        })
        .collect()
}

/// Writes one JSON object.
///
/// Duplicated from `command::param`'s own copy rather than shared — see
/// that module's docs on this crate's small-per-module-helper convention.
fn emit_json(out: &mut dyn Write, value: &serde_json::Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
    );
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parse_ports_accepts_well_formed_entries() {
        let ports =
            parse_ports(&["camera/frames".to_string(), "detector/boxes".to_string()]).unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].to_string(), "camera/frames");
    }

    #[test]
    fn parse_ports_is_empty_for_an_empty_list() {
        assert!(parse_ports(&[]).unwrap().is_empty());
    }

    #[test]
    fn parse_ports_rejects_a_malformed_entry() {
        let error = parse_ports(&["not a port".to_string()]).unwrap_err();
        assert!(matches!(error, CliError::BadArgument { flag: "only", .. }));
    }
}
