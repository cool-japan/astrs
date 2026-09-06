//! `astrs bag info` and `astrs bag convert` (blueprint §10.6, §14, §17).
//!
//! `info` reads whichever of the three formats this build understands
//! (`.arec` via `astrs-recording`'s own footer index, `.db3`/`.mcap` via
//! `astrs-rosbag`) the input's extension names —
//! [`astrs_rosbag::convert::detect_format`] is the single place that
//! sniffing happens, so this module and `astrs_rosbag::convert::convert_bag`
//! never disagree about what a given path *is*. `convert` bridges between
//! them via [`astrs_rosbag::convert::convert_bag`] directly.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use astrs_recording::{Reader, RecoveryReport, Stats};
use astrs_rosbag::convert::{BagFormat, ConversionReport, convert_bag, detect_format};
use astrs_rosbag::db3::Reader as Db3Reader;
use astrs_rosbag::mcap::Reader as McapReader;

use crate::error::CliError;

/// `astrs bag info` arguments.
#[derive(Debug, Clone)]
pub struct InfoArgs {
    /// The bag file to inspect — `.arec`, `.db3`, or `.mcap`.
    pub input: PathBuf,
    /// Emit JSON rather than a human-readable summary.
    pub json: bool,
}

/// One topic/channel's summary within a `.db3`/`.mcap` [`BagSummary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicInfo {
    /// The topic name (`.db3`) or channel topic (`.mcap`).
    pub name: String,
    /// The message type, or empty when unknown (see
    /// [`astrs_rosbag::TopicRecord::type`]'s own docs on when that
    /// happens).
    pub r#type: String,
    /// `.db3`'s `serialization_format` column / `.mcap`'s
    /// `message_encoding` field.
    pub serialization_format: String,
    /// How many messages this topic carries.
    pub message_count: u64,
}

/// A `.db3`/`.mcap` file's summary: every topic/channel plus the total
/// message count — what [`crate::cli::BagInfoArgs`]'s own doc comment
/// promises ("topics, types, message counts").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BagSummary {
    /// Which of the two formats `input` was.
    pub format: BagFormat,
    /// Every topic/channel, in the source format's own order.
    pub topics: Vec<TopicInfo>,
    /// The total message count across every topic.
    pub message_count: u64,
    /// Non-fatal notes the underlying reader accumulated (an unusable
    /// `metadata.yaml` and its in-database fallback both failing, a
    /// summary section that could not be used, and similar).
    pub warnings: Vec<String>,
}

/// What [`info`] found, one variant per format family — `.arec`'s own
/// footer-index [`Stats`] are a different shape than a `.db3`/`.mcap`
/// topic table, so this is not forced into one struct.
#[derive(Debug, Clone)]
pub enum InfoReport {
    /// `.arec` statistics, from [`astrs_recording::Stats::of`].
    Recording {
        /// The computed statistics.
        stats: Stats,
        /// Whether [`Reader::open`]'s trailer was missing or unusable and
        /// a full scan (`astrs-recording::recover`) was needed instead.
        recovered: bool,
    },
    /// A `.db3`/`.mcap` topic/message summary.
    Bag(BagSummary),
}

/// Prints a bag's summary: for `.arec`, dataflow id, entry count, byte
/// totals, the recorded time range, and a per-port breakdown (falling
/// back to scan recovery rather than refusing a file still being
/// written, exactly as before); for `.db3`/`.mcap`, every topic's type,
/// serialization format and message count.
///
/// # Errors
///
/// [`CliError::Io`] if the file cannot be opened at all,
/// [`CliError::Recording`] if an `.arec` cannot be made sense of even by
/// scanning, or [`CliError::Rosbag`] if a `.db3`/`.mcap` cannot be opened
/// or read, or if `input`'s extension names no format this build
/// understands.
pub fn info(out: &mut dyn Write, args: &InfoArgs) -> Result<InfoReport, CliError> {
    let format = detect_format(&args.input)?;
    match format {
        BagFormat::Arec => info_recording(out, args),
        BagFormat::Db3 => info_bag(out, args, BagFormat::Db3, info_db3(&args.input)?),
        BagFormat::Mcap => info_bag(out, args, BagFormat::Mcap, info_mcap(&args.input)?),
        // `BagFormat` is `#[non_exhaustive]`: a future astrs-rosbag release
        // adding a fourth format compiles here (no silent match-arm gap)
        // but needs an explicit `bag info` handler above before this CLI
        // build can read one — an honest "not yet", not a panic.
        _ => Err(CliError::BadArgument {
            flag: "input",
            value: args.input.display().to_string(),
            reason: format!(
                "astrs-rosbag recognizes `.{}` but this build's `bag info` has no handler for \
                 it yet",
                format.extension()
            ),
        }),
    }
}

fn info_recording(out: &mut dyn Write, args: &InfoArgs) -> Result<InfoReport, CliError> {
    let file_size = std::fs::metadata(&args.input)
        .map_err(|error| CliError::io(&args.input, error))?
        .len();
    let (reader, report) = Reader::open_or_recover(&args.input)?;
    let stats = Stats::of(&reader, file_size);
    let recovered = report.was_recovered();

    if args.json {
        emit_recording_json(out, &args.input, &stats, &report);
    } else {
        emit_recording_human(out, &args.input, &stats, recovered);
    }

    Ok(InfoReport::Recording { stats, recovered })
}

fn info_bag(
    out: &mut dyn Write,
    args: &InfoArgs,
    format: BagFormat,
    (topics, message_count, warnings): (Vec<TopicInfo>, u64, Vec<String>),
) -> Result<InfoReport, CliError> {
    let summary = BagSummary {
        format,
        topics,
        message_count,
        warnings,
    };
    if args.json {
        emit_bag_json(out, &args.input, &summary);
    } else {
        emit_bag_human(out, &args.input, &summary);
    }
    Ok(InfoReport::Bag(summary))
}

/// Opens a `.db3` and returns its topic summary — a single streaming pass
/// over `messages` tallies each topic's count (bounded memory, mirroring
/// [`Db3Reader::iter_messages`]'s own batching), since the schema carries
/// no per-topic count directly.
///
/// # Errors
///
/// [`CliError::Rosbag`] if the file cannot be opened or read.
fn info_db3(input: &Path) -> Result<(Vec<TopicInfo>, u64, Vec<String>), CliError> {
    let reader = Db3Reader::open(input)?;
    let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
    let mut total = 0u64;
    for message in reader.iter_messages() {
        let message = message?;
        *counts.entry(message.topic_id).or_insert(0) += 1;
        total += 1;
    }
    let topics = reader
        .topics()
        .iter()
        .map(|(id, record)| TopicInfo {
            name: record.topic.clone(),
            r#type: record.r#type.clone(),
            serialization_format: record.serialization_format.clone(),
            message_count: counts.get(id).copied().unwrap_or(0),
        })
        .collect();
    Ok((topics, total, reader.warnings().to_vec()))
}

/// Opens an `.mcap` and returns its channel summary. Per-channel counts
/// come from the summary section's `Statistics.channel_message_counts`
/// when the file is indexed and populated it (no message walk needed);
/// otherwise falls back to draining [`McapReader::iter_messages`] once to
/// tally them, the same two-pass shape
/// [`astrs_rosbag::convert::mcap_to_arec::mcap_to_arec`] uses and for the
/// same reason (an unindexed file's channel table itself is only
/// complete after a full walk).
///
/// # Errors
///
/// [`CliError::Rosbag`] if the file cannot be opened or read.
fn info_mcap(input: &Path) -> Result<(Vec<TopicInfo>, u64, Vec<String>), CliError> {
    let mut reader = McapReader::open(input)?;
    let counts: BTreeMap<u16, u64> = match reader
        .statistics()
        .filter(|stats| !stats.channel_message_counts.is_empty())
    {
        Some(stats) => stats.channel_message_counts.clone(),
        None => {
            let mut tally = BTreeMap::new();
            for message in reader.iter_messages() {
                let message = message?;
                *tally.entry(message.channel.id).or_insert(0) += 1;
            }
            tally
        }
    };
    let total = counts.values().sum();
    let topics = reader
        .channels()
        .iter()
        .map(|(id, channel)| {
            let r#type = if channel.schema_id == 0 {
                String::new()
            } else {
                reader
                    .schemas()
                    .get(&channel.schema_id)
                    .map(|schema| schema.name.clone())
                    .unwrap_or_default()
            };
            TopicInfo {
                name: channel.topic.clone(),
                r#type,
                serialization_format: channel.message_encoding.clone(),
                message_count: counts.get(id).copied().unwrap_or(0),
            }
        })
        .collect();
    Ok((topics, total, reader.warnings().to_vec()))
}

fn emit_recording_human(out: &mut dyn Write, path: &Path, stats: &Stats, recovered: bool) {
    let _ = writeln!(out, "{}", path.display());
    let _ = writeln!(out, "  dataflow:   {}", stats.dataflow);
    let _ = writeln!(out, "  entries:    {}", stats.entry_count);
    let _ = writeln!(out, "  file size:  {} bytes", stats.file_size_bytes);
    let _ = writeln!(
        out,
        "  payload:    {} bytes (uncompressed)",
        stats.uncompressed_payload_bytes
    );
    match stats.hlc_range {
        Some((min, max)) => {
            let _ = writeln!(out, "  hlc range:  {min} .. {max}");
        }
        None => {
            let _ = writeln!(out, "  hlc range:  (empty)");
        }
    }
    if recovered {
        let _ = writeln!(out, "  note:       recovered by scan (no trailer found)");
    }
    let _ = writeln!(out, "  ports:");
    for ((node, output), port_stats) in &stats.ports {
        let _ = writeln!(
            out,
            "    {node}/{output}: {} entries, {} bytes",
            port_stats.count, port_stats.payload_bytes
        );
    }
    let _ = out.flush();
}

fn emit_recording_json(out: &mut dyn Write, path: &Path, stats: &Stats, report: &RecoveryReport) {
    let ports: serde_json::Map<String, serde_json::Value> = stats
        .ports
        .iter()
        .map(|((node, output), port_stats)| {
            (
                format!("{node}/{output}"),
                serde_json::json!({
                    "count": port_stats.count,
                    "bytes": port_stats.payload_bytes,
                }),
            )
        })
        .collect();
    let value = serde_json::json!({
        "kind": "arec",
        "path": path.display().to_string(),
        "dataflow": stats.dataflow.to_string(),
        "entries": stats.entry_count,
        "file_size_bytes": stats.file_size_bytes,
        "uncompressed_payload_bytes": stats.uncompressed_payload_bytes,
        "hlc_range": stats.hlc_range.map(|(min, max)| serde_json::json!({
            "min": min.physical_ns(),
            "max": max.physical_ns(),
        })),
        "recovered": report.was_recovered(),
        "ports": ports,
    });
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
    );
    let _ = out.flush();
}

fn emit_bag_human(out: &mut dyn Write, path: &Path, summary: &BagSummary) {
    let _ = writeln!(out, "{}", path.display());
    let _ = writeln!(out, "  format:     .{}", summary.format.extension());
    let _ = writeln!(out, "  messages:   {}", summary.message_count);
    let _ = writeln!(out, "  topics:");
    for topic in &summary.topics {
        let type_label = if topic.r#type.is_empty() {
            "(unknown type)"
        } else {
            topic.r#type.as_str()
        };
        let _ = writeln!(
            out,
            "    {} [{}] {type_label}: {} messages",
            topic.name, topic.serialization_format, topic.message_count
        );
    }
    for warning in &summary.warnings {
        let _ = writeln!(out, "  warning:    {warning}");
    }
    let _ = out.flush();
}

fn emit_bag_json(out: &mut dyn Write, path: &Path, summary: &BagSummary) {
    let topics: Vec<serde_json::Value> = summary
        .topics
        .iter()
        .map(|topic| {
            serde_json::json!({
                "name": topic.name,
                "type": topic.r#type,
                "serialization_format": topic.serialization_format,
                "message_count": topic.message_count,
            })
        })
        .collect();
    let value = serde_json::json!({
        "kind": summary.format.extension(),
        "path": path.display().to_string(),
        "messages": summary.message_count,
        "topics": topics,
        "warnings": summary.warnings,
    });
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
    );
    let _ = out.flush();
}

/// `astrs bag convert` arguments.
#[derive(Debug, Clone)]
pub struct ConvertArgs {
    /// The input file (`.arec`, `.db3`, or `.mcap` — inferred from the
    /// extension).
    pub input: PathBuf,
    /// The output file (extension selects the target format).
    pub output: PathBuf,
    /// Emit JSON rather than a human-readable summary.
    pub json: bool,
}

/// Converts `args.input` to `args.output` via
/// [`astrs_rosbag::convert::convert_bag`], printing a summary of what was
/// written.
///
/// # Errors
///
/// [`CliError::Rosbag`] for anything [`convert_bag`] itself reports:
/// an unrecognized extension on either side, `→ .mcap` (no `.mcap`
/// writer), `.mcap → .db3` or a same-format pair (neither bridges without
/// going through `.arec` — see [`astrs_rosbag::RosbagError::UnsupportedConversion`]),
/// or a read/write failure on either file.
pub fn convert(out: &mut dyn Write, args: &ConvertArgs) -> Result<ConversionReport, CliError> {
    let report = convert_bag(&args.input, &args.output)?;

    if args.json {
        let value = serde_json::json!({
            "input": report.input.display().to_string(),
            "output": report.output.display().to_string(),
            "direction": report.direction,
            "topics": report.topics,
            "messages": report.messages,
            "warnings": report.warnings,
        });
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let _ = writeln!(
            out,
            "{} -> {} ({})",
            report.input.display(),
            report.output.display(),
            report.direction
        );
        let _ = writeln!(out, "  topics:    {}", report.topics);
        let _ = writeln!(out, "  messages:  {}", report.messages);
        for warning in &report.warnings {
            let _ = writeln!(out, "  warning:   {warning}");
        }
    }
    let _ = out.flush();
    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_recording::{Writer, WriterOptions};
    use astrs_rosbag::TopicRecord;
    use astrs_rosbag::db3::Writer as Db3Writer;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, DataflowId, Metadata, NodeId};

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-bag-test-{}-{}-{label}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_recording(path: &PathBuf, finish: bool) {
        let options = WriterOptions::new(DataflowId::from_u128(42), HlcTimestamp::new(1, 0));
        let mut writer = Writer::create(path, options).unwrap();
        writer
            .append_parts(
                NodeId::new("camera").unwrap(),
                DataId::new("frames").unwrap(),
                Metadata::new(HlcTimestamp::new(10, 0)),
                vec![0; 64],
            )
            .unwrap();
        if finish {
            writer.finish().unwrap();
        } else {
            std::mem::forget(writer);
        }
    }

    #[test]
    fn info_reports_stats_for_a_finished_recording() {
        let dir = temp_dir("finished");
        let path = dir.join("session.arec");
        sample_recording(&path, true);
        let mut out = Vec::new();
        let report = info(
            &mut out,
            &InfoArgs {
                input: path.clone(),
                json: false,
            },
        )
        .unwrap();
        let InfoReport::Recording { stats, recovered } = report else {
            panic!("expected a Recording report");
        };
        assert!(!recovered);
        assert_eq!(stats.entry_count, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("camera/frames"));
    }

    #[test]
    fn info_falls_back_to_recovery_for_a_footerless_file() {
        let dir = temp_dir("footerless");
        let path = dir.join("session.arec");
        sample_recording(&path, false);
        let mut out = Vec::new();
        let report = info(
            &mut out,
            &InfoArgs {
                input: path.clone(),
                json: false,
            },
        )
        .unwrap();
        let InfoReport::Recording { recovered, .. } = report else {
            panic!("expected a Recording report");
        };
        assert!(recovered);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("recovered by scan"));
    }

    #[test]
    fn info_emits_valid_json_for_a_recording() {
        let dir = temp_dir("json");
        let path = dir.join("session.arec");
        sample_recording(&path, true);
        let mut out = Vec::new();
        info(
            &mut out,
            &InfoArgs {
                input: path.clone(),
                json: true,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["entries"], 1);
        assert_eq!(value["recovered"], false);
        assert_eq!(value["kind"], "arec");
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let path = std::env::temp_dir().join("astrs-cli-bag-does-not-exist.arec");
        let mut out = Vec::new();
        let error = info(
            &mut out,
            &InfoArgs {
                input: path,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Io { .. }));
    }

    #[test]
    fn an_unrecognized_extension_is_a_rosbag_error() {
        let dir = temp_dir("unknown-ext");
        let path = dir.join("session.bag");
        std::fs::write(&path, b"anything").unwrap();
        let mut out = Vec::new();
        let error = info(
            &mut out,
            &InfoArgs {
                input: path,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Rosbag(_)));
    }

    fn sample_db3(path: &PathBuf) {
        let mut writer = Db3Writer::create(path, "jazzy").unwrap();
        let topic_id = writer
            .create_topic(&TopicRecord::new(
                "/scan",
                "sensor_msgs/msg/LaserScan",
                "cdr",
            ))
            .unwrap();
        writer
            .write_messages(
                vec![
                    Ok((topic_id, 1_000, vec![1, 2, 3])),
                    Ok((topic_id, 2_000, vec![4, 5, 6])),
                ]
                .into_iter(),
            )
            .unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn info_reads_a_db3_bag_with_per_topic_message_counts() {
        let dir = temp_dir("db3");
        let path = dir.join("session.db3");
        sample_db3(&path);
        let mut out = Vec::new();
        let report = info(
            &mut out,
            &InfoArgs {
                input: path.clone(),
                json: false,
            },
        )
        .unwrap();
        let InfoReport::Bag(summary) = report else {
            panic!("expected a Bag report");
        };
        assert_eq!(summary.format, BagFormat::Db3);
        assert_eq!(summary.message_count, 2);
        assert_eq!(summary.topics.len(), 1);
        assert_eq!(summary.topics[0].name, "/scan");
        assert_eq!(summary.topics[0].message_count, 2);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("/scan"));
        assert!(text.contains(".db3"));
    }

    #[test]
    fn info_emits_valid_json_for_a_db3_bag() {
        let dir = temp_dir("db3-json");
        let path = dir.join("session.db3");
        sample_db3(&path);
        let mut out = Vec::new();
        info(
            &mut out,
            &InfoArgs {
                input: path,
                json: true,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["kind"], "db3");
        assert_eq!(value["messages"], 2);
        assert_eq!(value["topics"][0]["name"], "/scan");
        assert_eq!(value["topics"][0]["message_count"], 2);
    }

    #[test]
    fn convert_bridges_arec_to_db3_and_prints_a_summary() {
        let dir = temp_dir("convert-arec-db3");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        sample_recording(&input, true);

        let mut out = Vec::new();
        let report = convert(
            &mut out,
            &ConvertArgs {
                input: input.clone(),
                output: output.clone(),
                json: false,
            },
        )
        .unwrap();
        assert_eq!(report.direction, "arec -> db3");
        assert_eq!(report.messages, 1);
        assert!(output.exists());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("arec -> db3"));
    }

    #[test]
    fn convert_emits_valid_json() {
        let dir = temp_dir("convert-json");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        sample_recording(&input, true);

        let mut out = Vec::new();
        convert(
            &mut out,
            &ConvertArgs {
                input,
                output,
                json: true,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["direction"], "arec -> db3");
        assert_eq!(value["messages"], 1);
    }

    #[test]
    fn convert_propagates_a_typed_error_for_an_unsupported_pair() {
        let dir = temp_dir("convert-unsupported");
        let input = dir.join("session.db3");
        let output = dir.join("session.mcap");
        sample_db3(&input);

        let mut out = Vec::new();
        let error = convert(
            &mut out,
            &ConvertArgs {
                input,
                output,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Rosbag(_)));
    }
}
