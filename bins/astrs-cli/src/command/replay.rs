//! `astrs replay` (blueprint §14, §17), in its two forms.
//!
//! Every node the recording covers (or every node `--replace` names
//! explicitly) is replaced **in place**: its `id` and its `outputs:`
//! are untouched, only its source becomes `path: astrs-replay-node` with
//! `args:` pointing at the recording and restricting it to that node's
//! own outputs. Because the id and outputs never change, every sibling
//! node's `inputs: { x: camera/frames }` still resolves — nothing about
//! the graph's wiring is rewritten, only what actually produces it.
//!
//! ```text
//!   - id: camera              - id: camera
//!     path: ./camera            path: astrs-replay-node
//!     outputs: [frames]  ─►     outputs: [frames]
//!                                args: [session.arec, --mode, real-time,
//!                                       --only, camera/frames]
//! ```
//!
//! # Two forms, one rewrite
//!
//! ```text
//!   astrs replay s.arec --into graph.yml   offline: print the rewritten manifest
//!   astrs replay s.arec perception         live:    cut the running graph over
//! ```
//!
//! The offline form ([`run`]) is a pure function of a file and a manifest: it
//! prints YAML and touches nothing. The live form ([`run_live`]) does the
//! *same rewrite* — literally the same `rewrite_node` on the same
//! `resolve_targets` result (both private to this module) — against the
//! expanded manifest the coordinator
//! is already holding for a running dataflow, and then hands each rewritten
//! node to §8/§17's dynamic-topology machinery as a
//! [`astrs_wire::ControlRequest::ReplaceNode`].
//!
//! Sharing the rewrite is the point, not an economy: "what a replay node
//! would have looked like offline" and "what the running graph is cut over
//! to" are the same question, and a second implementation of it would be a
//! second answer waiting to disagree.
//!
//! # Why `ReplaceNode` and not `AddNode`
//!
//! A replay is a *substitution*: last week's sensor data standing in for the
//! sensor, on the edges the sensor already had. `AddNode` would introduce a
//! second producer beside the live one — two nodes publishing the same port,
//! which is not what "test a new planner against last week's data" means.
//! `ReplaceNode` keeps the target's id and therefore its edges, which is the
//! whole reason the offline rewrite leaves `id:` and `outputs:` alone: the
//! two forms are the same edit expressed against a file and against a live
//! graph.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;

use astrs_manifest::Manifest;
use astrs_recording::Reader;
use astrs_wire::{ControlReply, ControlRequest, DataflowId};

use crate::cli::ReplayTimingMode;
use crate::command::client::{Client, DataflowRef, Endpoint, reply_name, runtime};
use crate::error::CliError;

/// `astrs replay` arguments, independent of `clap`.
#[derive(Debug, Clone)]
pub struct ReplayIntoArgs {
    /// The `.arec` file to replay.
    pub input: PathBuf,
    /// The manifest file to rewrite.
    pub into: PathBuf,
    /// The timing mode the generated replay node(s) pace entries with.
    pub mode: ReplayTimingMode,
    /// The real-time speed multiplier.
    pub speed: Option<f64>,
    /// The fixed-rate emission frequency.
    pub rate: Option<f64>,
    /// Whether the generated replay node(s) loop.
    pub r#loop: bool,
    /// Node ids to replace; every node the recording covers, if empty.
    pub replace: Vec<String>,
}

/// What [`run`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayIntoReport {
    /// The rewritten manifest.
    pub manifest: Manifest,
    /// [`Self::manifest`] rendered as YAML — exactly what was printed.
    pub yaml: String,
    /// The node ids that were actually replaced, in manifest order.
    pub replaced: Vec<String>,
}

/// Rewrites `args.into` (parsed, validated), replacing each node in
/// [`ReplayIntoArgs::replace`] — or, if that is empty, every node whose
/// id matches a producer `args.input`'s footer index recorded — with an
/// `astrs-replay-node` instance, then validates and prints the result.
///
/// # Errors
///
/// - [`CliError::Recording`] if `args.input` cannot be opened (falling
///   back to scan recovery for a footerless file, same as `bag info`).
/// - [`CliError::Io`] / [`CliError::Manifest`] / [`CliError::Validation`]
///   for the target manifest, exactly as `astrs expand`/`astrs validate`
///   report them.
/// - [`CliError::BadArgument`] if an explicit `--replace` id names no
///   node in the manifest, or if nothing ends up replaced at all (every
///   `--replace` id was bad, or auto-detection matched nothing — the
///   recording and the manifest name disjoint node ids).
pub fn run(out: &mut dyn Write, args: &ReplayIntoArgs) -> Result<ReplayIntoReport, CliError> {
    let (reader, _recovery) = Reader::open_or_recover(&args.input)?;
    let recorded_nodes: BTreeSet<String> = reader
        .index()
        .iter()
        .map(|entry| entry.node.as_str().to_owned())
        .collect();

    let content =
        std::fs::read_to_string(&args.into).map_err(|error| CliError::io(&args.into, error))?;
    let mut manifest = Manifest::from_yaml_str(&content)?;
    manifest.validate()?;

    let targets = resolve_targets(&manifest, &recorded_nodes, &args.replace)?;
    let replay_args = fixed_replay_args(args);

    let mut replaced = Vec::new();
    for node in &mut manifest.nodes {
        if !targets.contains(&node.id) {
            continue;
        }
        rewrite_node(node, &args.input, &replay_args);
        replaced.push(node.id.clone());
    }

    manifest.validate()?;
    let yaml = manifest.to_yaml().map_err(CliError::Manifest)?;
    let _ = writeln!(out, "{yaml}");
    let _ = out.flush();

    Ok(ReplayIntoReport {
        manifest,
        yaml,
        replaced,
    })
}

/// The `--mode`/`--speed`/`--rate`/`--loop` flags every generated node
/// gets, independent of which node it is.
fn fixed_replay_args(args: &ReplayIntoArgs) -> Vec<String> {
    fixed_replay_args_for(args.mode, args.speed, args.rate, args.r#loop)
}

/// The node ids to replace: `replace` verbatim if it names anything, else
/// every manifest node whose id `recorded_nodes` contains.
///
/// # Errors
///
/// [`CliError::BadArgument`] if an explicit id names no manifest node, or
/// if the result is empty either way.
fn resolve_targets(
    manifest: &Manifest,
    recorded_nodes: &BTreeSet<String>,
    replace: &[String],
) -> Result<BTreeSet<String>, CliError> {
    let declared: BTreeSet<&str> = manifest.nodes.iter().map(|node| node.id.as_str()).collect();

    let targets: BTreeSet<String> = if replace.is_empty() {
        manifest
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .filter(|id| recorded_nodes.contains(id))
            .collect()
    } else {
        for id in replace {
            if !declared.contains(id.as_str()) {
                return Err(CliError::BadArgument {
                    flag: "replace",
                    value: id.clone(),
                    reason: "does not name a node in the target manifest".to_owned(),
                });
            }
        }
        replace.iter().cloned().collect()
    };

    if targets.is_empty() {
        return Err(CliError::BadArgument {
            flag: "replace",
            value: String::new(),
            reason: "no node was replaced — the recording and the manifest name disjoint \
                     node ids; pass --replace explicitly"
                .to_owned(),
        });
    }
    Ok(targets)
}

/// Rewrites one node in place: clears every source field but `path`, and
/// builds `args:` from the fixed replay flags plus one `--only
/// <id>/<output>` per output this node declares.
fn rewrite_node(node: &mut astrs_manifest::Node, input: &std::path::Path, fixed_args: &[String]) {
    node.path = Some("astrs-replay-node".to_string());
    node.git = None;
    node.branch = None;
    node.tag = None;
    node.rev = None;
    node.module = None;
    node.operators = None;
    node.ros2 = None;
    node.record = None;
    node.build = None;

    let mut args = vec![input.display().to_string()];
    args.extend(fixed_args.iter().cloned());
    for output in &node.outputs {
        args.push("--only".to_string());
        args.push(format!("{}/{output}", node.id));
    }
    node.args = args;
}

// ---------------------------------------------------------------------
// The live form
// ---------------------------------------------------------------------

/// `astrs replay <file> <dataflow>` arguments — the live form.
#[derive(Debug, Clone)]
pub struct ReplayLiveArgs {
    /// The `.arec` file to replay.
    pub input: PathBuf,
    /// The running dataflow to cut over, by id or by name.
    pub dataflow: DataflowRef,
    /// The timing mode the replay node(s) pace entries with.
    pub mode: ReplayTimingMode,
    /// The real-time speed multiplier.
    pub speed: Option<f64>,
    /// The fixed-rate emission frequency.
    pub rate: Option<f64>,
    /// Whether the replay node(s) loop.
    pub r#loop: bool,
    /// Node ids to replace; every node the recording *and* the running graph
    /// both name, if empty.
    pub replace: Vec<String>,
    /// Let each outgoing node drain its inputs before it is stopped (§8).
    pub drain: bool,
    /// Emit JSON rather than a human summary.
    pub json: bool,
}

/// What [`run_live`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayLiveReport {
    /// The dataflow that was cut over.
    pub dataflow: astrs_wire::DataflowId,
    /// The node ids replaced, in manifest order.
    pub replaced: Vec<String>,
}

impl ReplayLiveReport {
    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "dataflow": self.dataflow.to_string(),
            "replaced": self.replaced,
        })
    }

    /// The human line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "replaced {} node(s) in {} with replay sources: {}",
            self.replaced.len(),
            self.dataflow,
            self.replaced.join(", ")
        )
    }
}

/// Cuts a *running* dataflow over to a recording: every node the recording
/// covers (or every node `--replace` names) is replaced in place by an
/// `astrs-replay-node` reading `args.input`.
///
/// The graph the rewrite is computed against is the coordinator's own
/// expanded manifest (`GetManifest`, §8.5 — every `module:` already
/// flattened), not a file on this machine: the point of the live form is that
/// the operator need not have the manifest that started the run, and a
/// *different* file would rewrite a graph the cluster is not running.
///
/// # Errors
///
/// - [`CliError::Recording`] if `args.input` cannot be opened (falling back
///   to scan recovery for a footerless file, as `bag info` does).
/// - [`CliError::NoCluster`] / [`CliError::UnknownDataflow`] if the dataflow
///   cannot be resolved.
/// - [`CliError::BadArgument`] if `--replace` names a node the running graph
///   does not have, or if nothing would be replaced at all.
/// - [`CliError::Refused`] if the coordinator rejects a replacement — the
///   first refusal stops the cutover, so a graph is never left half replayed
///   by a rejection the operator has not seen.
pub fn run_live(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ReplayLiveArgs,
) -> Result<ReplayLiveReport, CliError> {
    let (reader, _recovery) = Reader::open_or_recover(&args.input)?;
    let recorded_nodes: BTreeSet<String> = reader
        .index()
        .iter()
        .map(|entry| entry.node.as_str().to_owned())
        .collect();
    let replay_args = fixed_replay_args_for(args.mode, args.speed, args.rate, args.r#loop);

    let runtime = runtime()?;
    let report = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = client.resolve(&args.dataflow).await?;
        let manifest = live_manifest(&mut client, dataflow).await?;

        let targets = resolve_targets(&manifest, &recorded_nodes, &args.replace)?;
        // The rewrite is computed for every target *before* any of it is
        // sent, so a fragment that cannot be expanded fails the whole verb
        // rather than half of a running graph.
        let mut specs = Vec::new();
        for node in &manifest.nodes {
            if !targets.contains(&node.id) {
                continue;
            }
            let mut node = node.clone();
            rewrite_node(&mut node, &args.input, &replay_args);
            specs.push((node.id.clone(), node_spec(dataflow, &node)?));
        }

        let mut replaced = Vec::new();
        for (id, spec) in specs {
            client
                .request_ok(
                    "replay",
                    &ControlRequest::ReplaceNode {
                        dataflow,
                        node: Box::new(spec),
                        drain: args.drain,
                    },
                )
                .await?;
            replaced.push(id);
        }
        Ok::<_, CliError>(ReplayLiveReport { dataflow, replaced })
    })?;

    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&report.to_json()).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let _ = writeln!(out, "{}", report.summary());
    }
    let _ = out.flush();
    Ok(report)
}

/// The running dataflow's own expanded manifest, from the coordinator.
async fn live_manifest(client: &mut Client, dataflow: DataflowId) -> Result<Manifest, CliError> {
    match client
        .request("replay", &ControlRequest::GetManifest { dataflow })
        .await?
    {
        ControlReply::Manifest { yaml, .. } => Ok(Manifest::from_yaml_str(&yaml)?),
        other => Err(CliError::UnexpectedReply {
            request: "replay",
            reply: reply_name(&other),
        }),
    }
}

/// One rewritten manifest node, expanded into the spawn specification
/// `ReplaceNode` carries.
///
/// Delegated rather than built field by field: the expansion rules (`args:`
/// resolution, port declaration, restart defaults) live in
/// `astrs_coordinator::expand_node`, and a second hand-rolled path into
/// `NodeSpawnSpec` would be a second set of defaults to keep in step.
fn node_spec(
    dataflow: DataflowId,
    node: &astrs_manifest::Node,
) -> Result<astrs_wire::NodeSpawnSpec, CliError> {
    // Generation 0: the coordinator stamps the incarnation it actually
    // assigns when it dispatches the replacement (§12), exactly as
    // `astrs node replace` does.
    Ok(astrs_coordinator::expand_node(dataflow, 0, node)?)
}

/// [`fixed_replay_args`], as the live form's flags rather than
/// [`ReplayIntoArgs`]'s.
///
/// The two forms take the same four pacing flags and must produce the same
/// `args:`, so both spell them here.
fn fixed_replay_args_for(
    mode: ReplayTimingMode,
    speed: Option<f64>,
    rate: Option<f64>,
    looping: bool,
) -> Vec<String> {
    let mut out = vec!["--mode".to_string(), mode.as_str().to_string()];
    if let Some(speed) = speed {
        out.push("--speed".to_string());
        out.push(speed.to_string());
    }
    if let Some(rate) = rate {
        out.push("--rate".to_string());
        out.push(rate.to_string());
    }
    if looping {
        out.push("--loop".to_string());
    }
    out
}

/// Every `node/output` pair a recording's footer index covers, grouped by
/// node — exposed for callers (tests, `astrs bag info`-adjacent tooling)
/// that want the same grouping this module derives internally when it
/// resolves replacement targets.
#[must_use]
pub fn ports_by_node(reader: &Reader) -> BTreeMap<String, BTreeSet<String>> {
    let mut grouped: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in reader.index() {
        grouped
            .entry(entry.node.as_str().to_owned())
            .or_default()
            .insert(entry.output.as_str().to_owned());
    }
    grouped
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_recording::{Writer, WriterOptions};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, DataflowId, Metadata, NodeId};

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-replay-test-{}-{}-{label}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_recording(path: &PathBuf) {
        let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(path, options).unwrap();
        writer
            .append_parts(
                NodeId::new("camera").unwrap(),
                DataId::new("frames").unwrap(),
                Metadata::new(HlcTimestamp::new(1, 0)),
                vec![1],
            )
            .unwrap();
        writer.finish().unwrap();
    }

    fn base_args(input: PathBuf, into: PathBuf) -> ReplayIntoArgs {
        ReplayIntoArgs {
            input,
            into,
            mode: ReplayTimingMode::AsFastAsPossible,
            speed: None,
            rate: None,
            r#loop: false,
            replace: Vec::new(),
        }
    }

    #[test]
    fn replaces_the_recorded_node_in_place_and_preserves_wiring() {
        let dir = temp_dir("basic");
        let recording = dir.join("session.arec");
        sample_recording(&recording);
        let manifest_path = dir.join("graph.yaml");
        std::fs::write(
            &manifest_path,
            "nodes:\n\
             \x20 - id: camera\n\
             \x20   path: ./camera\n\
             \x20   outputs: [frames]\n\
             \x20 - id: detector\n\
             \x20   path: ./detector\n\
             \x20   inputs:\n\
             \x20     frames: camera/frames\n",
        )
        .unwrap();

        let mut out = Vec::new();
        let report = run(&mut out, &base_args(recording.clone(), manifest_path)).unwrap();
        assert_eq!(report.replaced, vec!["camera".to_string()]);

        let camera = report
            .manifest
            .nodes
            .iter()
            .find(|n| n.id == "camera")
            .unwrap();
        assert_eq!(camera.path.as_deref(), Some("astrs-replay-node"));
        assert_eq!(camera.outputs, vec!["frames".to_string()]);
        assert!(camera.args.contains(&recording.display().to_string()));
        assert!(camera.args.iter().any(|a| a == "camera/frames"));

        let detector = report
            .manifest
            .nodes
            .iter()
            .find(|n| n.id == "detector")
            .unwrap();
        assert_eq!(
            detector.inputs.get("frames").map(|i| i.source.as_str()),
            Some("camera/frames"),
            "sibling wiring must be untouched"
        );

        // The printed YAML round-trips to the identical manifest.
        let printed = String::from_utf8(out).unwrap();
        let reparsed = Manifest::from_yaml_str(printed.trim_end()).unwrap();
        assert_eq!(reparsed, report.manifest);
        reparsed.validate().expect("must still validate");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_replace_overrides_auto_detection() {
        let dir = temp_dir("explicit");
        let recording = dir.join("session.arec");
        sample_recording(&recording); // only "camera" is recorded
        let manifest_path = dir.join("graph.yaml");
        std::fs::write(
            &manifest_path,
            "nodes:\n\
             \x20 - id: camera\n\
             \x20   path: ./camera\n\
             \x20   outputs: [frames]\n\
             \x20 - id: other\n\
             \x20   path: ./other\n\
             \x20   outputs: [x]\n",
        )
        .unwrap();

        let mut args = base_args(recording, manifest_path);
        args.replace = vec!["other".to_string()];
        let mut out = Vec::new();
        let report = run(&mut out, &args).unwrap();
        assert_eq!(report.replaced, vec!["other".to_string()]);
        let camera = report
            .manifest
            .nodes
            .iter()
            .find(|n| n.id == "camera")
            .unwrap();
        assert_eq!(camera.path.as_deref(), Some("./camera"), "untouched");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_replace_id_is_rejected() {
        let dir = temp_dir("unknown-replace");
        let recording = dir.join("session.arec");
        sample_recording(&recording);
        let manifest_path = dir.join("graph.yaml");
        std::fs::write(
            &manifest_path,
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n",
        )
        .unwrap();

        let mut args = base_args(recording, manifest_path);
        args.replace = vec!["ghost".to_string()];
        let mut out = Vec::new();
        let error = run(&mut out, &args).unwrap_err();
        assert!(matches!(
            error,
            CliError::BadArgument {
                flag: "replace",
                ..
            }
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disjoint_node_ids_are_rejected_rather_than_a_silent_no_op() {
        let dir = temp_dir("disjoint");
        let recording = dir.join("session.arec");
        sample_recording(&recording); // records "camera"
        let manifest_path = dir.join("graph.yaml");
        std::fs::write(
            &manifest_path,
            "nodes:\n  - id: unrelated\n    path: ./unrelated\n",
        )
        .unwrap();

        let mut out = Vec::new();
        let error = run(&mut out, &base_args(recording, manifest_path)).unwrap_err();
        assert!(matches!(
            error,
            CliError::BadArgument {
                flag: "replace",
                ..
            }
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fixed_args_carry_mode_speed_rate_and_loop() {
        let args = ReplayIntoArgs {
            input: PathBuf::from("x.arec"),
            into: PathBuf::from("g.yaml"),
            mode: ReplayTimingMode::FixedRate,
            speed: Some(2.0),
            rate: Some(30.0),
            r#loop: true,
            replace: Vec::new(),
        };
        let fixed = fixed_replay_args(&args);
        assert_eq!(
            fixed,
            vec![
                "--mode".to_string(),
                "fixed-rate".to_string(),
                "--speed".to_string(),
                "2".to_string(),
                "--rate".to_string(),
                "30".to_string(),
                "--loop".to_string(),
            ]
        );
    }

    #[test]
    fn ports_by_node_groups_the_footer_index() {
        let dir = temp_dir("ports-by-node");
        let recording = dir.join("session.arec");
        let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&recording, options).unwrap();
        writer
            .append_parts(
                NodeId::new("camera").unwrap(),
                DataId::new("frames").unwrap(),
                Metadata::default(),
                vec![],
            )
            .unwrap();
        writer
            .append_parts(
                NodeId::new("camera").unwrap(),
                DataId::new("meta").unwrap(),
                Metadata::default(),
                vec![],
            )
            .unwrap();
        writer.finish().unwrap();

        let reader = Reader::open(&recording).unwrap();
        let grouped = ports_by_node(&reader);
        assert_eq!(
            grouped.get("camera").cloned(),
            Some(BTreeSet::from(["frames".to_string(), "meta".to_string()]))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // The live form
    // -----------------------------------------------------------------

    fn live_args(input: PathBuf, dataflow: &str) -> ReplayLiveArgs {
        ReplayLiveArgs {
            input,
            dataflow: DataflowRef::parse(dataflow),
            mode: ReplayTimingMode::RealTime,
            speed: None,
            rate: None,
            r#loop: false,
            replace: Vec::new(),
            drain: false,
            json: false,
        }
    }

    /// Port 1 on loopback: never bound by this suite, refused instantly.
    fn dead_endpoint() -> Endpoint {
        Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            astrs_wire::AuthToken::ZERO,
        )
    }

    #[test]
    fn the_live_form_reads_the_recording_before_it_dials_anything() {
        // A `.arec` this machine cannot open is the operator's mistake, and
        // saying so beats a connection error that blames the cluster.
        let dir = temp_dir("live-missing-recording");
        let error = run_live(
            &mut Vec::new(),
            &dead_endpoint(),
            &live_args(dir.join("absent.arec"), "perception"),
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Recording(_)), "{error}");
    }

    #[test]
    fn the_live_form_reports_a_dead_cluster_rather_than_hanging() {
        let dir = temp_dir("live-no-cluster");
        let input = dir.join("session.arec");
        sample_recording(&input);
        let error = run_live(
            &mut Vec::new(),
            &dead_endpoint(),
            &live_args(input, "perception"),
        )
        .unwrap_err();
        assert!(matches!(error, CliError::NoCluster { .. }), "{error}");
    }

    #[test]
    fn both_forms_rewrite_a_node_identically() {
        // The property the two forms are built to have: whatever `--into`
        // would have written into a manifest is exactly what the live form
        // hands to `ReplaceNode`. Asserted on the rewrite itself, which is
        // the single shared step.
        let dir = temp_dir("both-forms");
        let input = dir.join("session.arec");
        sample_recording(&input);

        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n",
        )
        .unwrap();
        let mut offline = manifest.nodes[0].clone();
        let mut live = manifest.nodes[0].clone();

        let into = base_args(input.clone(), dir.join("graph.yml"));
        rewrite_node(&mut offline, &input, &fixed_replay_args(&into));
        // Same pacing flags on both sides, which is the whole comparison:
        // given identical flags the two forms must produce identical `args:`.
        let mut args = live_args(input.clone(), "perception");
        args.mode = into.mode;
        args.speed = into.speed;
        args.rate = into.rate;
        args.r#loop = into.r#loop;
        rewrite_node(
            &mut live,
            &input,
            &fixed_replay_args_for(args.mode, args.speed, args.rate, args.r#loop),
        );

        assert_eq!(offline.path, live.path);
        assert_eq!(offline.args, live.args);
        assert_eq!(offline.outputs, live.outputs);
        assert_eq!(offline.id, live.id);
    }

    #[test]
    fn a_rewritten_node_expands_into_a_spawn_spec_naming_the_replay_binary() {
        // The step the live form adds on top of the shared rewrite: the
        // rewritten node has to survive expansion into the `NodeSpawnSpec`
        // `ReplaceNode` carries, with its outputs (and therefore its edges)
        // intact.
        let dir = temp_dir("expand");
        let input = dir.join("session.arec");
        sample_recording(&input);
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n",
        )
        .unwrap();
        let mut node = manifest.nodes[0].clone();
        let args = live_args(input.clone(), "perception");
        rewrite_node(
            &mut node,
            &input,
            &fixed_replay_args_for(args.mode, args.speed, args.rate, args.r#loop),
        );

        let spec = node_spec(DataflowId::from_u128(1), &node).unwrap();
        assert_eq!(spec.node.as_str(), "camera");
        assert_eq!(
            spec.outputs.len(),
            1,
            "the edges' anchor survives: {spec:?}"
        );
        assert!(
            format!("{:?}", spec.source).contains("astrs-replay-node"),
            "{:?}",
            spec.source
        );
        assert!(
            spec.args.iter().any(|arg| arg == "--only"),
            "the replay node is restricted to this node's own outputs: {:?}",
            spec.args
        );
    }

    #[test]
    fn the_live_report_names_what_it_replaced() {
        let report = ReplayLiveReport {
            dataflow: DataflowId::from_u128(7),
            replaced: vec!["camera".to_owned(), "lidar".to_owned()],
        };
        assert!(report.summary().contains("camera, lidar"));
        assert_eq!(report.to_json()["replaced"][1], "lidar");
    }
}
