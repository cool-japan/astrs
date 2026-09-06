//! The protocol snapshot: the compatibility contract of the whole project.
//!
//! Blueprint §7.2: *"All enums `#[non_exhaustive]`, append-only; `xtask
//! snapshot-protocol` freezes variant indices into `tests/golden/protocol.snap`
//! and CI fails on any reorder."*
//!
//! This test **is** that freeze. It encodes one deterministic sample per
//! variant of every family (from `astrs_wire::samples`), renders the result as
//! a stable text file, and compares it byte for byte with the committed
//! `tests/golden/protocol.snap`. A reordered variant, a renamed one, a changed
//! field order or a changed field type all move at least one byte, and the
//! comparison fails with the exact line that moved.
//!
//! # Regenerating
//!
//! When a change to the snapshot is *intended* — a variant appended at the tail
//! — run this test, copy the file it writes into `tests/golden/protocol.snap`,
//! and review the diff line by line. A diff that shows anything other than
//! **added lines at the end of a section** is a wire break, not an update.
//!
//! # Two goldens, two questions
//!
//! Regeneration is the weak point of any snapshot test: the fix for a failure
//! is "copy the new file over the old one", and a reviewer who does that
//! without reading the diff has just approved a wire break. So there is a
//! second golden, `tests/golden/protocol.frozen.snap`, which is a verbatim copy
//! of `protocol.snap` and is **never regenerated**:
//!
//! | File | Question | Regenerated on a tail append |
//! |---|---|---|
//! | `protocol.snap` | did *anything* change? | yes |
//! | `protocol.frozen.snap` | did anything *already frozen* move? | no |
//!
//! [`the_frozen_prefix_of_every_family_is_unchanged`] asserts that every
//! section of the live snapshot **begins with** the lines the frozen copy
//! records, in order. A tail append leaves every one of those lines where it
//! was and passes; a renumber, a rename, a reordered field or a widened field
//! type moves one and fails — and no amount of regenerating `protocol.snap`
//! makes it pass again.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;

use astrs_wire::{
    Compression, ControlReply, ControlRequest, CoordinatorEvent, DaemonEvent, DataflowStatus,
    ErrorCode, ExtensionNamespace, FRAME_VERSION, FrameFlags, FrameKind, FrameLimits, GoalStatus,
    HEADER_LEN, LogLevel, MAGIC, MIN_SUPPORTED_PROTOCOL, NodeEvent, NodePattern, NodeRequest,
    NodeRunState, PROTOCOL_VERSION, PeerEvent, Plane, QueuePolicy, RequestScope, RestartPolicy,
    Role, SpanStatus, WireEncode, WireMessage, samples,
};

/// The committed snapshot.
const GOLDEN: &str = include_str!("golden/protocol.snap");

/// The file name the regenerated snapshot is written to, inside the system
/// temporary directory.
const REGENERATED_NAME: &str = "astrs-protocol.snap";

/// Renders `bytes` as lower-case hex.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Appends one `<index> <name> <hex>` line per variant of a family.
fn render_family<T: WireMessage>(out: &mut String, family: &str, samples: &[T]) {
    let _ = writeln!(out, "\n[{family}] kind={}", T::KIND.as_u16());
    assert_eq!(
        samples.len(),
        T::VARIANT_NAMES.len(),
        "{family}: the sample table must cover every variant"
    );
    for (index, sample) in samples.iter().enumerate() {
        let index = u16::try_from(index).expect("a family has fewer than 65_536 variants");
        assert_eq!(
            sample.variant_index(),
            index,
            "{family}: sample {index} is out of order"
        );
        let bytes = sample.encode_to_vec().expect("a sample always encodes");
        assert_eq!(
            u16::from(bytes[0]),
            index,
            "{family}: the encoded discriminant of {} is not {index}",
            sample.variant_name()
        );
        let _ = writeln!(out, "{index} {} {}", sample.variant_name(), hex(&bytes));
    }
}

/// Appends one line per variant of a supporting enum.
fn render_support<T: WireEncode>(out: &mut String, name: &str, variants: &[T], labels: &[&str]) {
    assert_eq!(
        variants.len(),
        labels.len(),
        "{name}: every variant needs a label"
    );
    for (index, (variant, label)) in variants.iter().zip(labels).enumerate() {
        let bytes = variant
            .encode_to_vec()
            .expect("a support value always encodes");
        assert_eq!(
            usize::from(bytes[0]),
            index,
            "{name}: {label} does not encode as {index}"
        );
        let _ = writeln!(out, "{name} {index} {label} {}", hex(&bytes));
    }
}

/// Renders the whole snapshot.
fn render_snapshot() -> String {
    let mut out = String::with_capacity(64 * 1024);

    out.push_str(
        "# AstRS protocol snapshot v1 — the compatibility contract (blueprint §7.2, §24.1).\n\
         #\n\
         # Every line below is frozen. Variant indices, field order and field types are all\n\
         # visible in the encoded bytes, so any accidental reorder fails this test.\n\
         #\n\
         # Regenerate (only for an intended, tail-only addition):\n\
         #     cargo test -p astrs-wire --test protocol_snapshot\n\
         # then copy the file the failure names over tests/golden/protocol.snap and review\n\
         # the diff: anything other than added lines at the end of a section is a wire break.\n\
         #\n\
         # Format:\n\
         #   [Family] kind=N          one section per message family\n\
         #   <index> <Variant> <hex>  the oxicode encoding of one deterministic sample\n",
    );

    let _ = writeln!(out, "\n[meta]");
    let _ = writeln!(out, "protocol_version {PROTOCOL_VERSION}");
    let _ = writeln!(out, "min_supported_protocol {MIN_SUPPORTED_PROTOCOL}");
    let _ = writeln!(out, "frame_version {FRAME_VERSION}");
    let _ = writeln!(out, "frame_magic {}", hex(&MAGIC));
    let _ = writeln!(out, "frame_header_len {HEADER_LEN}");

    let _ = writeln!(out, "\n[frame_kinds]");
    for kind in FrameKind::ALL {
        let _ = writeln!(out, "{} {}", kind.as_u16(), kind.as_str());
    }

    // A complete frame, header and checksum included, so the *framing* is
    // frozen and not merely the payload encoding.
    let _ = writeln!(out, "\n[frames]");
    let limits = FrameLimits::network();
    for (label, flags) in [("plain", FrameFlags::EMPTY), ("crc", FrameFlags::CRC)] {
        let bytes = astrs_wire::encode_frame(FrameKind::Data, flags, b"astrs", &limits)
            .expect("a small frame always encodes");
        let _ = writeln!(out, "data_{label} {}", hex(&bytes));
    }

    render_family(
        &mut out,
        "ControlRequest",
        &samples::control_requests().expect("samples"),
    );
    render_family(
        &mut out,
        "ControlReply",
        &samples::control_replies().expect("samples"),
    );
    render_family(
        &mut out,
        "CoordinatorEvent",
        &samples::coordinator_events().expect("samples"),
    );
    render_family(
        &mut out,
        "DaemonEvent",
        &samples::daemon_events().expect("samples"),
    );
    render_family(
        &mut out,
        "NodeRequest",
        &samples::node_requests().expect("samples"),
    );
    render_family(
        &mut out,
        "NodeEvent",
        &samples::node_events().expect("samples"),
    );
    render_family(
        &mut out,
        "PeerEvent",
        &samples::peer_events().expect("samples"),
    );

    let _ = writeln!(out, "\n[fan_out]");
    let data = samples::sample_data_frame().expect("samples");
    let log = samples::sample_log_frame().expect("samples");
    let telemetry = samples::sample_telemetry_frame().expect("samples");
    let _ = writeln!(
        out,
        "DataFrame {}",
        hex(&data.encode_to_vec().expect("encodes"))
    );
    let _ = writeln!(
        out,
        "LogFrame {}",
        hex(&log.encode_to_vec().expect("encodes"))
    );
    let _ = writeln!(
        out,
        "TelemetryFrame {}",
        hex(&telemetry.encode_to_vec().expect("encodes"))
    );

    let _ = writeln!(out, "\n[handshake]");
    let _ = writeln!(
        out,
        "Hello {}",
        hex(&samples::sample_hello()
            .expect("samples")
            .encode_to_vec()
            .expect("encodes"))
    );
    let _ = writeln!(
        out,
        "Welcome {}",
        hex(&samples::sample_welcome()
            .expect("samples")
            .encode_to_vec()
            .expect("encodes"))
    );
    let _ = writeln!(
        out,
        "Refused {}",
        hex(&samples::sample_refused().encode_to_vec().expect("encodes"))
    );

    let _ = writeln!(out, "\n[support]");
    render_support(
        &mut out,
        "Role",
        Role::ALL,
        &["Cli", "Daemon", "Node", "Peer"],
    );
    render_support(
        &mut out,
        "RequestScope",
        RequestScope::ALL,
        &["Read", "Mutate"],
    );
    render_support(
        &mut out,
        "ErrorCode",
        ErrorCode::ALL,
        &[
            "Internal",
            "NotFound",
            "AlreadyExists",
            "InvalidArgument",
            "FailedPrecondition",
            "PermissionDenied",
            "Unavailable",
            "Timeout",
            "ResourceExhausted",
            "Unsupported",
            "Cancelled",
            "ValidationFailed",
            "BuildFailed",
        ],
    );
    render_support(
        &mut out,
        "Plane",
        Plane::ALL,
        &["Uds", "Shm", "Tcp", "Quic"],
    );
    render_support(
        &mut out,
        "Compression",
        Compression::ALL,
        &["None", "Lz4", "Zstd"],
    );
    render_support(
        &mut out,
        "LogLevel",
        LogLevel::ALL,
        &["Error", "Warn", "Info", "Debug", "Trace"],
    );
    render_support(
        &mut out,
        "DataflowStatus",
        DataflowStatus::ALL,
        &[
            "Pending", "Building", "Ready", "Starting", "Running", "Stopping", "Finished", "Failed",
        ],
    );
    render_support(
        &mut out,
        "NodeRunState",
        NodeRunState::ALL,
        &[
            "Pending",
            "Spawning",
            "Running",
            "Restarting",
            "Stopping",
            "Finished",
            "Failed",
        ],
    );
    render_support(
        &mut out,
        "QueuePolicy",
        QueuePolicy::ALL,
        &["DropOldest", "Backpressure"],
    );
    render_support(
        &mut out,
        "RestartPolicy",
        RestartPolicy::ALL,
        &["Never", "OnFailure", "Always"],
    );
    render_support(
        &mut out,
        "NodePattern",
        NodePattern::ALL,
        &["Service", "Client", "ActionServer", "ActionClient"],
    );
    render_support(
        &mut out,
        "SpanStatus",
        SpanStatus::ALL,
        &["Unset", "Ok", "Error"],
    );
    render_support(
        &mut out,
        "ExtensionNamespace",
        ExtensionNamespace::ALL,
        &["User", "PinnedMemory", "GpuHandle", "Internal"],
    );
    render_support(
        &mut out,
        "GoalStatus",
        GoalStatus::ALL,
        &[
            "Unknown",
            "Accepted",
            "Executing",
            "Canceling",
            "Succeeded",
            "Canceled",
            "Aborted",
        ],
    );

    out
}

/// Writes the regenerated snapshot to `name` inside the system temporary
/// directory and returns its path, for the failure message to name.
///
/// The file name is a parameter so that the test which *exercises* this path
/// cannot delete the file the failure path just wrote — the two run
/// concurrently.
fn write_regenerated(rendered: &str, name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, rendered).expect("the temporary directory is writable");
    path
}

/// The first line at which two texts differ, one-based, with both sides.
fn first_difference(left: &str, right: &str) -> Option<(usize, String, String)> {
    let mut left_lines = left.lines();
    let mut right_lines = right.lines();
    let mut line = 0usize;
    loop {
        line += 1;
        match (left_lines.next(), right_lines.next()) {
            (None, None) => return None,
            (left_line, right_line) => {
                let left_line = left_line.unwrap_or("<end of file>");
                let right_line = right_line.unwrap_or("<end of file>");
                if left_line != right_line {
                    return Some((line, left_line.to_owned(), right_line.to_owned()));
                }
            }
        }
    }
}

#[test]
fn the_protocol_snapshot_is_unchanged() {
    let rendered = render_snapshot();
    if rendered == GOLDEN {
        return;
    }

    let path = write_regenerated(&rendered, REGENERATED_NAME);
    let difference = first_difference(GOLDEN, &rendered)
        .map(|(line, golden, current)| {
            format!(
                "first difference at line {line}:\n  committed: {golden}\n  current:   {current}"
            )
        })
        .unwrap_or_else(|| "the files differ only in trailing content".to_owned());

    panic!(
        "\nThe protocol snapshot changed — this is a WIRE COMPATIBILITY BREAK unless every\n\
         difference is a line *appended* at the end of a section.\n\n\
         {difference}\n\n\
         If the change is intended (a variant appended at the tail of a family), copy the\n\
         regenerated snapshot over the committed one and review the diff:\n\n    \
         cp {} crates/astrs-wire/tests/golden/protocol.snap\n\n\
         If it is not intended, restore the variant order you changed.\n",
        path.display()
    );
}

/// The frozen copy: what every section looked like when it was frozen.
///
/// Never regenerated. See this file's module documentation for why there are
/// two goldens.
const FROZEN: &str = include_str!("golden/protocol.frozen.snap");

/// Splits a rendered snapshot into `section name → its lines`, in file order.
///
/// Comment and blank lines are dropped: they carry no protocol content, and
/// keeping them would make a reworded header look like a wire break. A section
/// starts at a `[name]` line; anything before the first one (there is nothing)
/// is ignored.
fn sections(snapshot: &str) -> Vec<(String, Vec<&str>)> {
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    for line in snapshot.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // `[NodeEvent] kind=4` — the name is the bracketed part; the
            // `kind=` suffix rides along in the section's own first line so a
            // changed frame kind is caught as content, not as a missing
            // section.
            let name = line
                .split(']')
                .next()
                .unwrap_or(line)
                .trim_start_matches('[')
                .to_owned();
            sections.push((name, vec![line]));
        } else if let Some((_, lines)) = sections.last_mut() {
            lines.push(line);
        }
    }
    sections
}

/// Why a live snapshot is not an append-only successor of the frozen one.
#[derive(Debug, PartialEq, Eq)]
enum PrefixBreak {
    /// A whole family disappeared.
    SectionMissing(String),
    /// A family lost lines: a frozen variant was removed.
    SectionShrank(String),
    /// A frozen line moved: a renumber, a rename, a reordered or widened
    /// field.
    LineMoved {
        /// The family it happened in.
        section: String,
        /// Its zero-based line within that family.
        index: usize,
        /// What the frozen copy records.
        frozen: String,
        /// What the live snapshot renders.
        live: String,
    },
    /// A family the frozen copy has never seen.
    SectionNew(String),
}

/// The first way `live` fails to be an append-only successor of `frozen`.
///
/// One function, two callers: the guard asserts it answers [`None`] for the
/// real snapshot, and the negative test asserts it answers `Some` for a
/// deliberately reordered one. A guard whose failure path is never executed is
/// decoration, and this one is behind a golden file that is never regenerated
/// — so nobody would notice it rotting.
fn first_prefix_break(
    frozen: &[(String, Vec<&str>)],
    live: &[(String, Vec<&str>)],
) -> Option<PrefixBreak> {
    for (name, frozen_lines) in frozen {
        let Some((_, live_lines)) = live.iter().find(|(live_name, _)| live_name == name) else {
            return Some(PrefixBreak::SectionMissing(name.clone()));
        };
        if live_lines.len() < frozen_lines.len() {
            return Some(PrefixBreak::SectionShrank(name.clone()));
        }
        for (index, frozen_line) in frozen_lines.iter().enumerate() {
            if live_lines[index] != *frozen_line {
                return Some(PrefixBreak::LineMoved {
                    section: name.clone(),
                    index,
                    frozen: (*frozen_line).to_owned(),
                    live: live_lines[index].to_owned(),
                });
            }
        }
    }
    // A family the frozen copy has never seen could otherwise be inserted
    // ahead of a frozen one without the loop above ever looking at it.
    for (name, _) in live {
        if !frozen.iter().any(|(frozen_name, _)| frozen_name == name) {
            return Some(PrefixBreak::SectionNew(name.clone()));
        }
    }
    None
}

#[test]
fn the_frozen_prefix_of_every_family_is_unchanged() {
    // The question `protocol.snap` cannot answer, because the documented fix
    // for a `protocol.snap` failure is to overwrite it: *did anything that was
    // already frozen move?* A tail append leaves every frozen line exactly
    // where it was, so every frozen section is still a prefix of the live one.
    let rendered = render_snapshot();
    let live = sections(&rendered);
    let frozen = sections(FROZEN);
    assert!(!frozen.is_empty(), "the frozen golden is empty");

    if let Some(break_) = first_prefix_break(&frozen, &live) {
        panic!(
            "\nThe frozen protocol prefix moved — this is a WIRE COMPATIBILITY BREAK.\n\n  \
             {break_:?}\n\n\
             Frozen bytes encode the variant index, the field order and every field type, so a \
             renumber, a rename, a reordered field or a widened field all land here. Append at \
             the tail instead; do not edit \
             crates/astrs-wire/tests/golden/protocol.frozen.snap.\n\n\
             A section the frozen copy has never seen is a *new* family: fine to add, but \
             record it in protocol.frozen.snap in the same commit as protocol.snap.\n"
        );
    }
}

#[test]
fn the_frozen_prefix_guard_notices_a_renumber() {
    // The guard has to fail when it should. Swapping two variants of a frozen
    // family is exactly what a renumber looks like on the wire, and this runs
    // it through the *same* comparison the guard uses rather than restating
    // that two different lines differ.
    let rendered = render_snapshot();
    let frozen = sections(FROZEN);
    let mut live = sections(&rendered);
    assert_eq!(first_prefix_break(&frozen, &live), None, "the guard passes");

    let (_, live_lines) = live
        .iter_mut()
        .find(|(name, lines)| name == "NodeEvent" && lines.len() > 3)
        .expect("the NodeEvent family has several variants");
    // Index 0 is the `[NodeEvent] kind=4` header; 1 and 2 are `Input` and
    // `InputClosed`.
    live_lines.swap(1, 2);

    assert!(
        matches!(
            first_prefix_break(&frozen, &live),
            Some(PrefixBreak::LineMoved { ref section, index: 1, .. }) if section == "NodeEvent"
        ),
        "a reordered variant must be caught at the line it moved to"
    );
}

#[test]
fn the_frozen_prefix_guard_notices_a_removed_family() {
    // The other two shapes of break, through the same comparison.
    let rendered = render_snapshot();
    let frozen = sections(FROZEN);

    let without_family: Vec<(String, Vec<&str>)> = sections(&rendered)
        .into_iter()
        .filter(|(name, _)| name != "PeerEvent")
        .collect();
    assert_eq!(
        first_prefix_break(&frozen, &without_family),
        Some(PrefixBreak::SectionMissing("PeerEvent".to_owned()))
    );

    let mut shortened = sections(&rendered);
    if let Some((_, lines)) = shortened.iter_mut().find(|(name, _)| name == "PeerEvent") {
        lines.pop();
    }
    assert_eq!(
        first_prefix_break(&frozen, &shortened),
        Some(PrefixBreak::SectionShrank("PeerEvent".to_owned()))
    );
}

#[test]
fn the_snapshot_carries_no_build_version() {
    // Every sample pins its own `AstrsVersion` (`0.0.0`). If a sample ever
    // reaches a constructor that injects `AstrsVersion::current()` instead, the
    // crate's version string appears in the frozen bytes and the snapshot
    // breaks at the next version bump — for a reason nobody would remember.
    let rendered = render_snapshot();
    let needle = hex(astrs_wire::ASTRS_VERSION_STR.as_bytes());
    assert!(
        !rendered.contains(&needle),
        "the snapshot embeds the crate version {} ({needle}); a sample is using \
         AstrsVersion::current() instead of samples::sample_version()",
        astrs_wire::ASTRS_VERSION_STR
    );
}

#[test]
fn the_snapshot_is_deterministic() {
    // Two renderings of the same build must be identical, or the snapshot
    // would fail at random rather than when the protocol changes.
    assert_eq!(render_snapshot(), render_snapshot());
}

#[test]
fn the_families_have_the_variant_counts_the_blueprint_froze() {
    // §24.1 lists these counts; the families that exceed them do so only by
    // tail appends, which are documented on the types themselves.
    // `ControlRequest`'s own tail append is `GetNodeMetrics` (index 35,
    // `astrs top`'s poll — see that variant's doc comment).
    assert!(ControlRequest::VARIANT_NAMES.len() >= 35);
    assert!(ControlReply::VARIANT_NAMES.len() >= 11);
    // `PeerRoutes` (index 16) is a tail append: §6.4's cross-daemon route
    // directives, which §24.1's original sixteen had no carrier for.
    // `ReplaceNode`/`AddEdge`/`RemoveEdge` (indices 17-19) are further tail
    // appends: §8/§17's dynamic-topology verbs, which §24.1's frozen set
    // validates (`ControlRequest`) but never gave a daemon-facing carrier.
    assert!(CoordinatorEvent::VARIANT_NAMES.len() >= 20);
    // `NodeIoMetrics` (index 12) is a tail append: §13's bandwidth half of
    // `NodeMetrics`, which could not be added to `NodeMetrics` itself because
    // index 7 is inside the frozen prefix (see `astrs_wire::NodeIoSample`).
    assert!(DaemonEvent::VARIANT_NAMES.len() >= 12);
    // `ReportDeadlineViolation` (index 11) is a tail append: §11.3's
    // node-detected deadline violation, relayed to the daemon for its
    // `astrs/status` fan-out and metric — §24.1's frozen eleven had no
    // carrier for a node reporting its own measured condition.
    assert!(NodeRequest::VARIANT_NAMES.len() >= 11);
    assert!(NodeEvent::VARIANT_NAMES.len() >= 13);
    assert_eq!(PeerEvent::VARIANT_NAMES.len(), 6);
}

#[test]
fn the_variant_names_match_the_blueprint_word_for_word() {
    // §24.1, transcribed. A rename is as much of a break as a reorder,
    // because every other crate in the workspace matches on these names.
    assert_eq!(
        &ControlRequest::VARIANT_NAMES[..35],
        &[
            "Hello",
            "Build",
            "WaitForBuild",
            "Start",
            "WaitForSpawn",
            "Check",
            "Stop",
            "StopByName",
            "Restart",
            "RestartByName",
            "Logs",
            "LogSubscribe",
            "List",
            "Info",
            "Destroy",
            "Clean",
            "ConnectedDaemons",
            "GetNodeInfo",
            "TopicSubscribe",
            "TopicUnsubscribe",
            "TopicPublish",
            "GetParams",
            "GetParam",
            "SetParam",
            "DeleteParam",
            "RestartNode",
            "StopNode",
            "AddNode",
            "RemoveNode",
            "ReplaceNode",
            "AddEdge",
            "RemoveEdge",
            "RecordStart",
            "RecordStop",
            "GetTraces",
        ]
    );
    assert_eq!(
        &ControlReply::VARIANT_NAMES[..11],
        &[
            "Ok",
            "Error",
            "DataflowList",
            "DataflowResult",
            "NodeInfo",
            "DaemonList",
            "Logs",
            "ParamValue",
            "ParamList",
            "TraceData",
            "Refused",
        ]
    );
    assert_eq!(
        &CoordinatorEvent::VARIANT_NAMES[..16],
        &[
            "Heartbeat",
            "Build",
            "Spawn",
            "AllNodesReady",
            "StopDataflow",
            "ReloadNode",
            "Logs",
            "RestartNode",
            "StopNode",
            "SetParam",
            "DeleteParam",
            "Destroy",
            "PeerDisconnected",
            "StateCatchUp",
            "TopicTapStart",
            "TopicTapStop",
        ]
    );
    assert_eq!(CoordinatorEvent::VARIANT_NAMES[16], "PeerRoutes");
    assert_eq!(
        &DaemonEvent::VARIANT_NAMES[..12],
        &[
            "Register",
            "Heartbeat",
            "BuildResult",
            "SpawnResult",
            "AllNodesReady",
            "AllNodesFinished",
            "NodeStopped",
            "NodeMetrics",
            "Log",
            "TopicTapData",
            "StateCatchUpAck",
            "Exit",
        ]
    );
    assert_eq!(DaemonEvent::VARIANT_NAMES[12], "NodeIoMetrics");
    assert_eq!(
        &NodeRequest::VARIANT_NAMES[..11],
        &[
            "Register",
            "Subscribe",
            "SendMessage",
            "OutputDone",
            "CloseOutputs",
            "NextEvent",
            "EventStreamDropped",
            "ExtStore",
            "ExtLoad",
            "ExtDrop",
            "RouteUpgradeAck",
        ]
    );
    assert_eq!(NodeRequest::VARIANT_NAMES[11], "ReportDeadlineViolation");
    assert_eq!(
        &NodeEvent::VARIANT_NAMES[..13],
        &[
            "Input",
            "InputClosed",
            "InputRecovered",
            "Stop",
            "Reload",
            "AllInputsClosed",
            "NodeFailed",
            "Restarted",
            "ParamUpdate",
            "ParamDeleted",
            "ExtDropped",
            "RouteUpgrade",
            "RouteDowngrade",
        ]
    );
    assert_eq!(
        PeerEvent::VARIANT_NAMES,
        &[
            "RouteSetup",
            "RouteAccept",
            "RouteTeardown",
            "Output",
            "OutputClosed",
            "Ping",
        ]
    );
}

#[test]
fn every_family_maps_to_a_distinct_frame_kind() {
    let kinds = [
        ControlRequest::KIND,
        ControlReply::KIND,
        CoordinatorEvent::KIND,
        DaemonEvent::KIND,
        NodeRequest::KIND,
        NodeEvent::KIND,
        PeerEvent::KIND,
    ];
    let mut sorted = kinds.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), kinds.len());
    for kind in kinds {
        assert!(kind.is_control_plane());
    }
}

#[test]
fn the_snapshot_names_every_variant_of_every_family() {
    let rendered = render_snapshot();
    for name in ControlRequest::VARIANT_NAMES
        .iter()
        .chain(ControlReply::VARIANT_NAMES)
        .chain(CoordinatorEvent::VARIANT_NAMES)
        .chain(DaemonEvent::VARIANT_NAMES)
        .chain(NodeRequest::VARIANT_NAMES)
        .chain(NodeEvent::VARIANT_NAMES)
        .chain(PeerEvent::VARIANT_NAMES)
    {
        assert!(
            rendered.contains(&format!(" {name} ")),
            "the snapshot does not mention {name}"
        );
    }
}

#[test]
fn a_regenerated_snapshot_can_be_written_to_a_temporary_directory() {
    // The failure path must work when it is needed, not first be discovered
    // broken by whoever is already staring at a wire break.
    let path = write_regenerated(&render_snapshot(), "astrs-protocol-selftest.snap");
    let written = std::fs::read_to_string(&path).expect("the snapshot was written");
    assert_eq!(written, render_snapshot());
    let _ = std::fs::remove_file(&path);
}
