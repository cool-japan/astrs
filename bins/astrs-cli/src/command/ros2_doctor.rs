//! `astrs ros2 doctor` (blueprint §17): the discovery probe.
//!
//! `astrs doctor` answers "can this machine run AstRS?"; this answers the
//! neighbouring question, "can this machine *see* a ROS 2 domain, and what
//! is on it?" — the one a user asks when a `ros2:` bridge (§10.5) is not
//! receiving anything and they need to know whether the fault is discovery,
//! QoS or the graph.
//!
//! Five checks, in a fixed order, each of which reports rather than aborts —
//! the same convention `astrs doctor` and `astrs validate` follow:
//!
//! | Check | Says |
//! |---|---|
//! | `ros2:domain` | which domain was probed, and where it came from |
//! | `ros2:multicast` | whether the SPDP group could be joined |
//! | `ros2:participants` | how many participants answered, and how many are AstRS |
//! | `ros2:graph` | how many ROS nodes announced `ros_discovery_info` |
//! | `ros2:endpoints` | how many topics, services and actions were seen |
//!
//! # Why "no participants" is a warning and not an error
//!
//! An empty domain is a perfectly legal state — nothing is running yet — and
//! a diagnostic that exits non-zero for it would be unusable in a script
//! that starts the stack and then checks. A *refused* multicast join with no
//! participants at all is the shape that usually means "this will not work",
//! and that pair is what the warning text names.

use std::io::Write;

use serde::Serialize;

use crate::command::doctor::{CheckStatus, DoctorCheck};
use crate::command::ros2::{ProbeArgs, ProbeReport, probe};
use crate::error::CliError;

/// The complete `astrs ros2 doctor` report.
#[derive(Debug, Clone, Serialize)]
pub struct Ros2DoctorReport {
    /// Every check, in a fixed, deterministic order.
    pub checks: Vec<DoctorCheck>,
    /// What the probe actually heard, so `--json` consumers do not have to
    /// re-parse the check text.
    pub probe: ProbeReport,
}

impl Ros2DoctorReport {
    /// `1` if any check is [`CheckStatus::Error`], else `0`.
    ///
    /// A warning does not fail the verb: see the module docs on why an empty
    /// domain is a legal state.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(
            self.checks
                .iter()
                .any(|check| check.status == CheckStatus::Error),
        )
    }
}

/// Probe the domain and print the report.
///
/// # Errors
///
/// [`CliError::Ros2`] when the probe participant cannot be created at all —
/// the one failure that is not a *finding*, because there is nothing to
/// report on. [`CliError::Io`] when writing to `out` fails.
pub fn run(out: &mut dyn Write, args: &ProbeArgs) -> Result<Ros2DoctorReport, CliError> {
    let probe = probe(args)?;
    let checks = checks_for(&probe);
    let report = Ros2DoctorReport { checks, probe };

    if args.json {
        let rendered = serde_json::to_string_pretty(&report)
            .map_err(|error| CliError::Ros2(format!("the report will not serialize: {error}")))?;
        writeln!(out, "{rendered}").map_err(io_error)?;
    } else {
        render(out, &report)?;
    }
    Ok(report)
}

/// The five checks a probe report produces.
#[must_use]
pub fn checks_for(probe: &ProbeReport) -> Vec<DoctorCheck> {
    vec![
        check_domain(probe),
        check_multicast(probe),
        check_participants(probe),
        check_graph(probe),
        check_endpoints(probe),
    ]
}

/// Which domain was probed.
fn check_domain(probe: &ProbeReport) -> DoctorCheck {
    DoctorCheck {
        name: "ros2:domain".to_owned(),
        status: CheckStatus::Info,
        detail: format!(
            "probed DDS domain {} for {} ms from {}",
            probe.domain_id, probe.listened_ms, probe.locator
        ),
    }
}

/// Whether the SPDP multicast group could be joined.
///
/// A refusal is a *warning*, never an error: §10.2's addendum makes unicast
/// initial peers a first-class discovery path, so a bridge with
/// `ASTRS_ROS2_INITIAL_PEERS` set works perfectly with multicast refused.
fn check_multicast(probe: &ProbeReport) -> DoctorCheck {
    let (status, detail) = if probe.multicast_joined {
        (
            CheckStatus::Ok,
            format!("SPDP multicast {}", probe.multicast),
        )
    } else {
        (
            CheckStatus::Warning,
            format!(
                "SPDP multicast {} — discovery needs unicast initial peers \
                 (`ASTRS_ROS2_INITIAL_PEERS` on a bridge node)",
                probe.multicast
            ),
        )
    };
    DoctorCheck {
        name: "ros2:multicast".to_owned(),
        status,
        detail,
    }
}

/// How many participants answered.
fn check_participants(probe: &ProbeReport) -> DoctorCheck {
    let total = probe.participants.len();
    let astrs = probe
        .participants
        .iter()
        .filter(|participant| participant.is_astrs)
        .count();
    let (status, detail) = if total == 0 {
        (
            CheckStatus::Warning,
            if probe.multicast_joined {
                "no participants answered — nothing is running on this domain".to_owned()
            } else {
                "no participants answered, and the multicast join was refused — \
                 this domain is very likely unreachable from here"
                    .to_owned()
            },
        )
    } else {
        (
            CheckStatus::Ok,
            format!("{total} participant(s) answered, {astrs} of them AstRS (vendor id 41 53)"),
        )
    };
    DoctorCheck {
        name: "ros2:participants".to_owned(),
        status,
        detail,
    }
}

/// How many ROS nodes announced themselves.
fn check_graph(probe: &ProbeReport) -> DoctorCheck {
    let (status, detail) = if probe.nodes.is_empty() && !probe.participants.is_empty() {
        (
            CheckStatus::Warning,
            "participants answered but none announced `ros_discovery_info` — \
             they are DDS peers rather than ROS 2 nodes, or their graph \
             sample has not arrived yet"
                .to_owned(),
        )
    } else if probe.nodes.is_empty() {
        (CheckStatus::Info, "no ROS 2 nodes on the graph".to_owned())
    } else {
        (
            CheckStatus::Ok,
            format!(
                "{} ROS 2 node(s): {}",
                probe.nodes.len(),
                probe.nodes.join(", ")
            ),
        )
    };
    DoctorCheck {
        name: "ros2:graph".to_owned(),
        status,
        detail,
    }
}

/// How many endpoints were seen.
fn check_endpoints(probe: &ProbeReport) -> DoctorCheck {
    let mut detail = format!(
        "{} topic(s), {} service(s), {} action(s), {} endpoint(s) total",
        probe.topics.len(),
        probe.services.len(),
        probe.actions.len(),
        probe.endpoint_count()
    );
    if probe.unclaimed_endpoints > 0 {
        detail.push_str(&format!(
            "; {} endpoint(s) claimed by no node",
            probe.unclaimed_endpoints
        ));
    }
    DoctorCheck {
        name: "ros2:endpoints".to_owned(),
        status: if probe.endpoint_count() == 0 {
            CheckStatus::Info
        } else {
            CheckStatus::Ok
        },
        detail,
    }
}

/// Print the report as a table, matching `astrs doctor`'s layout.
fn render(out: &mut dyn Write, report: &Ros2DoctorReport) -> Result<(), CliError> {
    let width = report
        .checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or(0);
    for check in &report.checks {
        writeln!(
            out,
            "{:<width$}  {:<7}  {}",
            check.name,
            check.status.to_string(),
            check.detail,
            width = width
        )
        .map_err(io_error)?;
    }
    Ok(())
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

    use crate::command::ros2::{ParticipantSummary, TopicSummary};

    use super::*;

    fn report(
        multicast_joined: bool,
        participants: Vec<ParticipantSummary>,
        nodes: Vec<String>,
        topics: Vec<TopicSummary>,
    ) -> ProbeReport {
        ProbeReport {
            domain_id: 0,
            listened_ms: 12,
            multicast: if multicast_joined {
                "joined 239.255.0.1".to_owned()
            } else {
                "refused 239.255.0.1 (operation not permitted)".to_owned()
            },
            multicast_joined,
            locator: "127.0.0.1:54321".to_owned(),
            participants,
            nodes,
            topics,
            services: Vec::new(),
            actions: Vec::new(),
            unclaimed_endpoints: 0,
        }
    }

    fn participant(is_astrs: bool, nodes: &[&str]) -> ParticipantSummary {
        ParticipantSummary {
            guid: "4153000000000001".to_owned(),
            is_astrs,
            nodes: nodes.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn topic(name: &str, publishers: usize, subscribers: usize) -> TopicSummary {
        TopicSummary {
            name: name.to_owned(),
            types: vec!["std_msgs/msg/String".to_owned()],
            publishers,
            subscribers,
            publisher_nodes: Vec::new(),
            subscriber_nodes: Vec::new(),
        }
    }

    #[test]
    fn a_healthy_domain_passes_every_check() {
        let probe = report(
            true,
            vec![participant(true, &["/talker"])],
            vec!["/talker".to_owned()],
            vec![topic("/chatter", 1, 1)],
        );
        let checks = checks_for(&probe);
        assert_eq!(checks.len(), 5);
        assert_eq!(checks[1].status, CheckStatus::Ok, "multicast");
        assert_eq!(checks[2].status, CheckStatus::Ok, "participants");
        assert_eq!(checks[3].status, CheckStatus::Ok, "graph");
        assert_eq!(checks[4].status, CheckStatus::Ok, "endpoints");

        let doctor = Ros2DoctorReport { checks, probe };
        assert_eq!(doctor.exit_code(), 0);
    }

    #[test]
    fn a_refused_multicast_join_is_a_warning_not_an_error() {
        let probe = report(
            false,
            vec![participant(false, &["/talker"])],
            vec!["/talker".to_owned()],
            Vec::new(),
        );
        let checks = checks_for(&probe);
        assert_eq!(checks[1].status, CheckStatus::Warning);
        assert!(
            checks[1].detail.contains("ASTRS_ROS2_INITIAL_PEERS"),
            "{}",
            checks[1].detail
        );
        let doctor = Ros2DoctorReport { checks, probe };
        assert_eq!(doctor.exit_code(), 0, "a warning does not fail the verb");
    }

    #[test]
    fn an_empty_domain_with_multicast_refused_says_so_plainly() {
        let probe = report(false, Vec::new(), Vec::new(), Vec::new());
        let checks = checks_for(&probe);
        assert_eq!(checks[2].status, CheckStatus::Warning);
        assert!(
            checks[2].detail.contains("unreachable"),
            "{}",
            checks[2].detail
        );
    }

    #[test]
    fn an_empty_domain_with_multicast_working_is_merely_empty() {
        let probe = report(true, Vec::new(), Vec::new(), Vec::new());
        let checks = checks_for(&probe);
        assert!(
            checks[2].detail.contains("nothing is running"),
            "{}",
            checks[2].detail
        );
        assert_eq!(checks[3].status, CheckStatus::Info);
        assert_eq!(checks[4].status, CheckStatus::Info);
    }

    #[test]
    fn a_dds_peer_that_is_not_a_ros_node_is_flagged() {
        let probe = report(true, vec![participant(false, &[])], Vec::new(), Vec::new());
        let checks = checks_for(&probe);
        assert_eq!(checks[3].status, CheckStatus::Warning);
        assert!(
            checks[3].detail.contains("ros_discovery_info"),
            "{}",
            checks[3].detail
        );
    }

    #[test]
    fn unclaimed_endpoints_are_counted_in_the_detail() {
        let mut probe = report(
            true,
            vec![participant(true, &["/n"])],
            vec!["/n".to_owned()],
            vec![topic("/t", 1, 0)],
        );
        probe.unclaimed_endpoints = 2;
        let checks = checks_for(&probe);
        assert!(
            checks[4].detail.contains("claimed by no node"),
            "{}",
            checks[4].detail
        );
    }

    #[test]
    fn the_table_prints_one_line_per_check() {
        let probe = report(
            true,
            vec![participant(true, &["/n"])],
            vec!["/n".to_owned()],
            Vec::new(),
        );
        let checks = checks_for(&probe);
        let doctor = Ros2DoctorReport { checks, probe };
        let mut out = Vec::new();
        render(&mut out, &doctor).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 5);
        assert!(text.contains("ros2:domain"), "{text}");
        assert!(text.contains("ros2:multicast"), "{text}");
    }

    #[test]
    fn the_json_form_carries_the_probe_beside_the_checks() {
        let probe = report(
            true,
            vec![participant(true, &["/n"])],
            vec!["/n".to_owned()],
            Vec::new(),
        );
        let checks = checks_for(&probe);
        let doctor = Ros2DoctorReport { checks, probe };
        let rendered = serde_json::to_string(&doctor).unwrap();
        assert!(rendered.contains("\"checks\""), "{rendered}");
        assert!(rendered.contains("\"probe\""), "{rendered}");
        assert!(rendered.contains("\"domain_id\":0"), "{rendered}");
    }
}
