//! `astrs-sim-logger` — a sink that tallies `astrs-sim-node`'s three
//! outputs and writes a JSON report, proving the whole
//! `cmd_vel`-in/`scan`+`odom`+`tf`-out wiring actually ran rather than
//! only compiled (the `tf-broadcast` example's `tf-consumer` is the same
//! shape, for the identical reason).
//!
//! ```text
//!   [astrs-sim-node] ──odom──► (odom) ┐
//!                    ├─scan──► (scan) ├──► [astrs-sim-logger] ──► LoggerReport (JSON)
//!                    └─tf────► (tf)   ┘
//! ```
//!
//! Finishes when every input has closed (`astrs-sim-node` closing its
//! outputs after its own tick budget runs out), not on a tick budget of
//! its own — this node has no timer input at all, purely reactive to
//! whatever arrives.

use std::path::PathBuf;

use astrs_node_api::message::{LaserScan, Odometry, Transform};
use astrs_node_api::{Event, Node};
use serde::{Deserialize, Serialize};

/// The odometry-estimate input.
const ODOM_PORT: &str = "odom";
/// The lidar-sweep input.
const SCAN_PORT: &str = "scan";
/// The dynamic-transform input.
const TF_PORT: &str = "tf";

/// Names the JSON file this node writes its [`LoggerReport`] to.
const ENV_REPORT_PATH: &str = "ASTRS_SIM_LOGGER_REPORT";

/// Where this node writes its report when [`ENV_REPORT_PATH`] names none
/// of its own — under [`std::env::temp_dir`], matching this workspace's
/// own house rule for test-owned temporary files.
fn default_report_path() -> PathBuf {
    std::env::temp_dir().join("astrs-sim-logger-report.json")
}

/// The file this run writes its report to.
fn report_path() -> PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, PathBuf::from)
}

/// What `astrs-sim-logger` observed, written as JSON when its inputs close.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoggerReport {
    /// How many `odom` messages were received.
    pub odom_count: u64,
    /// How many `scan` messages were received.
    pub scan_count: u64,
    /// How many `tf` messages were received.
    pub tf_count: u64,
    /// The last received odometry position, `[x, y, z]`.
    pub last_odom_position: Option<[f64; 3]>,
    /// The last received scan's beam count.
    pub last_scan_beam_count: Option<usize>,
    /// The shortest *finite* range in the last received scan — [`None`]
    /// both when no scan was ever received and when the last scan's every
    /// beam reported "no return" ([`f32::INFINITY`], see
    /// `astrs_sim::lidar::LidarScan`'s own docs); [`min_finite_range`]
    /// makes that distinction precise.
    pub last_scan_min_finite_range: Option<f32>,
    /// The last received `tf` translation, `[x, y, z]`.
    pub last_tf_translation: Option<[f64; 3]>,
}

impl LoggerReport {
    /// Renders this report as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialized, which this
    /// struct's field types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// The smallest range in `ranges` that is not [`f32::INFINITY`] (a "no
/// return" reading, see [`astrs_node_api::message::LaserScan`]'s own
/// `ranges` docs) — [`None`] if `ranges` is empty or every beam reported
/// no return.
#[must_use]
fn min_finite_range(ranges: &[f32]) -> Option<f32> {
    ranges
        .iter()
        .copied()
        .filter(|range| range.is_finite())
        .fold(None, |min, range| match min {
            None => Some(range),
            Some(current) => Some(current.min(range)),
        })
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(report) => {
            println!(
                "astrs-sim-logger: odom={} scan={} tf={}",
                report.odom_count, report.scan_count, report.tf_count
            );
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("astrs-sim-logger: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Reads every input to completion, returning the accumulated report
/// (also written to [`report_path`] as JSON).
fn run() -> Result<LoggerReport, Box<dyn std::error::Error>> {
    let (node, mut events) = Node::init_from_env()?;
    node.log_info("astrs-sim-logger up: watching odom/scan/tf");

    let mut report = LoggerReport::default();
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == ODOM_PORT => {
                let odom: Odometry = data.view()?;
                report.odom_count += 1;
                report.last_odom_position = Some([
                    odom.pose.position.x,
                    odom.pose.position.y,
                    odom.pose.position.z,
                ]);
            }
            Event::Input { id, data, .. } if id.as_str() == SCAN_PORT => {
                let scan: LaserScan = data.view()?;
                report.scan_count += 1;
                report.last_scan_beam_count = Some(scan.beam_count());
                report.last_scan_min_finite_range = min_finite_range(&scan.ranges);
            }
            Event::Input { id, data, .. } if id.as_str() == TF_PORT => {
                let transform: Transform = data.view()?;
                report.tf_count += 1;
                report.last_tf_translation = Some([
                    transform.translation.x,
                    transform.translation.y,
                    transform.translation.z,
                ]);
            }
            Event::AllInputsClosed => break,
            Event::Stop(cause) => {
                node.log_info(format!("stopping: {cause}"));
                break;
            }
            _ => {}
        }
    }

    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "wrote the report to {} (odom={}, scan={}, tf={})",
        path.display(),
        report.odom_count,
        report.scan_count,
        report.tf_count
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_report_round_trips_as_json() {
        let report = LoggerReport {
            odom_count: 3,
            scan_count: 3,
            tf_count: 3,
            last_odom_position: Some([1.0, 2.0, 0.0]),
            last_scan_beam_count: Some(36),
            last_scan_min_finite_range: Some(2.5),
            last_tf_translation: Some([1.0, 2.0, 0.0]),
        };
        let json = report.to_json().unwrap();
        let parsed: LoggerReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    #[test]
    fn a_default_report_has_no_observations() {
        let report = LoggerReport::default();
        assert_eq!(report.odom_count, 0);
        assert_eq!(report.last_odom_position, None);
        assert_eq!(report.last_scan_min_finite_range, None);
    }

    #[test]
    fn min_finite_range_ignores_no_return_beams() {
        assert_eq!(
            min_finite_range(&[f32::INFINITY, 3.0, 1.5, f32::INFINITY, 4.0]),
            Some(1.5)
        );
    }

    #[test]
    fn min_finite_range_is_none_for_an_empty_or_all_infinite_scan() {
        assert_eq!(min_finite_range(&[]), None);
        assert_eq!(min_finite_range(&[f32::INFINITY, f32::INFINITY]), None);
    }

    #[test]
    fn the_report_path_defaults_under_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    #[test]
    fn the_port_names_match_the_manifest() {
        assert_eq!(ODOM_PORT, "odom");
        assert_eq!(SCAN_PORT, "scan");
        assert_eq!(TF_PORT, "tf");
    }
}
