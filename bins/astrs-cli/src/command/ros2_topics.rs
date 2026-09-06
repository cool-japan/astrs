//! `astrs ros2 topics` (blueprint §17): the live DDS graph, listed.
//!
//! The same probe `ros2 doctor` runs ([`crate::command::ros2::probe`]),
//! presented as `ros2 topic list -t` does — one line per name, with the
//! type and the endpoint counts — plus the two families a plain topic
//! listing hides: services and actions, which on the wire are just more
//! topics under `rq/`, `rr/` and `<action>/_action/…`.
//!
//! ```text
//!   TOPIC                 TYPE                        PUB  SUB
//!   /chatter              std_msgs/msg/String           1    2
//!   /scan                 sensor_msgs/msg/LaserScan     1    1
//!
//!   SERVICE               TYPE                        SRV  CLI
//!   /add_two_ints         example_interfaces/srv/…      1    1
//! ```
//!
//! # Why the counts are not "publishers" for a service
//!
//! A service's reply writer is its *server* and its request writer is its
//! *client*, so labelling the columns `PUB`/`SUB` there would be technically
//! accurate and practically useless. The header changes with the section;
//! the underlying numbers are the same endpoint counts.

use std::io::Write;

use serde::Serialize;

use crate::command::ros2::{ProbeArgs, ProbeReport, TopicSummary, probe};
use crate::error::CliError;

/// The `astrs ros2 topics` report.
///
/// A thin wrapper over the probe rather than a second model: everything a
/// caller wants is already in [`ProbeReport`], and a `--json` consumer
/// should get the same shape from either verb.
#[derive(Debug, Clone, Serialize)]
pub struct Ros2TopicsReport {
    /// What the probe heard.
    pub probe: ProbeReport,
}

impl Ros2TopicsReport {
    /// How many named entities were listed, across all three families.
    #[must_use]
    pub fn len(&self) -> usize {
        self.probe.topics.len() + self.probe.services.len() + self.probe.actions.len()
    }

    /// Whether the graph was empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `0` always: an empty graph is a legal answer, not a failure.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        0
    }
}

/// Probe the domain and list what is on it.
///
/// # Errors
///
/// [`CliError::Ros2`] when the probe participant cannot be created, and
/// [`CliError::Io`] when writing to `out` fails.
pub fn run(out: &mut dyn Write, args: &ProbeArgs) -> Result<Ros2TopicsReport, CliError> {
    let report = Ros2TopicsReport {
        probe: probe(args)?,
    };

    if args.json {
        let rendered = serde_json::to_string_pretty(&report)
            .map_err(|error| CliError::Ros2(format!("the report will not serialize: {error}")))?;
        writeln!(out, "{rendered}").map_err(io_error)?;
    } else {
        render(out, &report)?;
    }
    Ok(report)
}

/// Print the three sections, omitting the ones that are empty.
fn render(out: &mut dyn Write, report: &Ros2TopicsReport) -> Result<(), CliError> {
    if report.is_empty() {
        writeln!(
            out,
            "no ROS 2 topics on domain {} (listened {} ms; multicast {})",
            report.probe.domain_id, report.probe.listened_ms, report.probe.multicast
        )
        .map_err(io_error)?;
        return Ok(());
    }

    let mut first = true;
    for (heading, left, right, entries) in [
        ("TOPIC", "PUB", "SUB", &report.probe.topics),
        ("SERVICE", "SRV", "CLI", &report.probe.services),
        ("ACTION", "SRV", "CLI", &report.probe.actions),
    ] {
        if entries.is_empty() {
            continue;
        }
        if !first {
            writeln!(out).map_err(io_error)?;
        }
        first = false;
        section(out, heading, left, right, entries)?;
    }
    Ok(())
}

/// Print one section as an aligned table.
fn section(
    out: &mut dyn Write,
    heading: &str,
    left: &str,
    right: &str,
    entries: &[TopicSummary],
) -> Result<(), CliError> {
    let name_width = entries
        .iter()
        .map(|entry| entry.name.len())
        .chain(std::iter::once(heading.len()))
        .max()
        .unwrap_or(heading.len());
    let type_width = entries
        .iter()
        .map(|entry| rendered_types(entry).len())
        .chain(std::iter::once(4))
        .max()
        .unwrap_or(4);

    writeln!(
        out,
        "{heading:<name_width$}  {:<type_width$}  {left:>4}  {right:>4}",
        "TYPE"
    )
    .map_err(io_error)?;
    for entry in entries {
        writeln!(
            out,
            "{:<name_width$}  {:<type_width$}  {:>4}  {:>4}",
            entry.name,
            rendered_types(entry),
            entry.publishers,
            entry.subscribers
        )
        .map_err(io_error)?;
    }
    Ok(())
}

/// The type column's text.
///
/// A name with two announced types shows both, separated by `|` — a peer
/// disagreement is exactly the thing a graph listing exists to surface, and
/// showing only the first would hide the bug the user is looking for.
#[must_use]
pub fn rendered_types(entry: &TopicSummary) -> String {
    if entry.types.is_empty() {
        "<unannounced>".to_owned()
    } else {
        entry.types.join(" | ")
    }
}

/// Wrap a write failure the way every other verb does.
fn io_error(source: std::io::Error) -> CliError {
    CliError::Io {
        path: "<stdout>".to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn summary(name: &str, types: &[&str], publishers: usize, subscribers: usize) -> TopicSummary {
        TopicSummary {
            name: name.to_owned(),
            types: types.iter().map(|t| (*t).to_owned()).collect(),
            publishers,
            subscribers,
            publisher_nodes: Vec::new(),
            subscriber_nodes: Vec::new(),
        }
    }

    fn report(
        topics: Vec<TopicSummary>,
        services: Vec<TopicSummary>,
        actions: Vec<TopicSummary>,
    ) -> Ros2TopicsReport {
        Ros2TopicsReport {
            probe: ProbeReport {
                domain_id: 3,
                listened_ms: 42,
                multicast: "joined 239.255.0.1".to_owned(),
                multicast_joined: true,
                locator: "127.0.0.1:1234".to_owned(),
                participants: Vec::new(),
                nodes: Vec::new(),
                topics,
                services,
                actions,
                unclaimed_endpoints: 0,
            },
        }
    }

    fn rendered(report: &Ros2TopicsReport) -> String {
        let mut out = Vec::new();
        render(&mut out, report).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn an_empty_graph_says_so_and_names_the_domain() {
        let report = report(Vec::new(), Vec::new(), Vec::new());
        assert!(report.is_empty());
        assert_eq!(report.exit_code(), 0);
        let text = rendered(&report);
        assert!(text.contains("no ROS 2 topics on domain 3"), "{text}");
        assert!(text.contains("multicast joined"), "{text}");
    }

    #[test]
    fn topics_are_listed_with_their_type_and_counts() {
        let report = report(
            vec![
                summary("/chatter", &["std_msgs/msg/String"], 1, 2),
                summary("/scan", &["sensor_msgs/msg/LaserScan"], 1, 1),
            ],
            Vec::new(),
            Vec::new(),
        );
        let text = rendered(&report);
        assert!(text.starts_with("TOPIC"), "{text}");
        assert!(text.contains("std_msgs/msg/String"), "{text}");
        assert!(text.contains("/scan"), "{text}");
        assert_eq!(text.lines().count(), 3, "a header and two rows: {text}");
        assert_eq!(report.len(), 2);
    }

    #[test]
    fn the_three_families_are_separate_sections_with_their_own_headers() {
        let report = report(
            vec![summary("/chatter", &["std_msgs/msg/String"], 1, 0)],
            vec![summary(
                "/add",
                &["example_interfaces/srv/AddTwoInts"],
                1,
                1,
            )],
            vec![summary(
                "/fib",
                &["example_interfaces/action/Fibonacci"],
                5,
                5,
            )],
        );
        let text = rendered(&report);
        assert!(text.contains("TOPIC"), "{text}");
        assert!(text.contains("SERVICE"), "{text}");
        assert!(text.contains("ACTION"), "{text}");
        assert!(
            text.contains("SRV"),
            "the service columns are relabelled: {text}"
        );
        assert_eq!(report.len(), 3);
    }

    #[test]
    fn an_empty_family_is_omitted_rather_than_shown_as_a_bare_header() {
        let report = report(
            vec![summary("/chatter", &["std_msgs/msg/String"], 1, 0)],
            Vec::new(),
            Vec::new(),
        );
        let text = rendered(&report);
        assert!(!text.contains("SERVICE"), "{text}");
        assert!(!text.contains("ACTION"), "{text}");
    }

    #[test]
    fn two_types_on_one_name_are_both_shown() {
        let entry = summary("/x", &["std_msgs/msg/String", "std_msgs/msg/Int32"], 1, 1);
        assert_eq!(
            rendered_types(&entry),
            "std_msgs/msg/String | std_msgs/msg/Int32"
        );
    }

    #[test]
    fn a_name_with_no_announced_type_is_labelled() {
        let entry = summary("/x", &[], 0, 1);
        assert_eq!(rendered_types(&entry), "<unannounced>");
    }

    #[test]
    fn the_columns_align_to_the_longest_name() {
        let report = report(
            vec![
                summary("/a", &["t"], 1, 0),
                summary("/a_very_long_topic_name", &["t"], 1, 0),
            ],
            Vec::new(),
            Vec::new(),
        );
        let text = rendered(&report);
        let widths: Vec<usize> = text
            .lines()
            .map(|line| line.find("  ").unwrap_or(0))
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] <= pair[1] + 24),
            "columns must line up: {text}"
        );
        assert!(text.contains("/a_very_long_topic_name"), "{text}");
    }

    #[test]
    fn the_json_form_is_the_probe_report() {
        let report = report(
            vec![summary("/chatter", &["std_msgs/msg/String"], 1, 0)],
            Vec::new(),
            Vec::new(),
        );
        let rendered = serde_json::to_string(&report).unwrap();
        assert!(rendered.contains("\"probe\""), "{rendered}");
        assert!(rendered.contains("\"/chatter\""), "{rendered}");
    }
}
