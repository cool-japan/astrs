//! The verification profile: the timing facts a manifest cannot state.
//!
//! # Why a sidecar rather than manifest fields
//!
//! A dataflow manifest (blueprint §8) describes *what runs and how it is
//! wired*. It deliberately says nothing about how long a node's handler
//! takes or what end-to-end deadline an application has in mind, and it
//! should not: those are properties of the code and of the product
//! requirement, not of the topology, and they change without the graph
//! changing.
//!
//! So the profile is a separate, entirely optional document. `astrs
//! validate --prove` works without one — deadlock freedom, rate
//! consistency and every latency budget the manifest itself declares (via
//! a per-input `timeout`, blueprint §8.3) are discharged from the manifest
//! alone. A profile *adds* the two things the manifest cannot express:
//!
//! - **Service times** (`wcet`), which is what turns
//!   [queue boundedness](crate::obligations::boundedness) from "not
//!   attempted, no service-time information" into a decided obligation.
//! - **Source rates and named end-to-end budgets**, for the parts of a
//!   graph a timer does not drive.
//!
//! # Format
//!
//! ```yaml
//! # verify.yaml
//! nodes:
//!   camera:
//!     rate: 30            # Hz — only meaningful for a node with no wired inputs
//!   detector:
//!     wcet: 0.012         # seconds; or an interval, below
//!   planner:
//!     wcet: { min: 0.001, max: 0.004 }
//!
//! paths:
//!   - name: shutter-to-plan
//!     target: planner/detections   # `node/input`
//!     budget: 0.100                # seconds
//! ```
//!
//! Unknown keys are rejected: a profile whose `wceat:` typo silently
//! disabled an obligation would be worse than no profile.

use std::collections::BTreeMap;
use std::path::Path;

use astrs_graph::{DataflowGraph, EdgeKey, NodeId, PortName};
use serde::{Deserialize, Serialize};

use crate::error::{ProfileError, VerifyError};
use crate::model::ServiceTime;
use crate::scale::{Nanos, Rate};

/// One node's declared timing facts, as written in the profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProfile {
    /// The node's per-firing service time, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wcet: Option<Wcet>,
    /// The node's emission rate in hertz.
    ///
    /// Only consulted for a node with no wired inputs: an event-driven
    /// node's rate is *derived* from its inputs (see
    /// [`crate::model::DerivedRates`]), and letting a profile contradict
    /// that derivation would make the model disagree with the graph it is
    /// supposed to describe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
}

/// A declared service time: one figure, or an interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Wcet {
    /// An exact per-firing service time, in seconds.
    Exact(f64),
    /// A service-time interval, in seconds.
    Interval {
        /// The lower bound — the half that can *prove overload*.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        /// The upper bound — the half that can *prove a deadline*.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
}

/// One declared end-to-end latency budget, as written in the profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathProfile {
    /// A name for this budget, used in reports.
    pub name: String,
    /// The `node/input` whose delivery must meet the budget.
    pub target: String,
    /// The budget, in seconds.
    pub budget: f64,
}

/// A verification profile as written on disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDocument {
    /// Per-node timing facts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nodes: BTreeMap<String, NodeProfile>,
    /// Named end-to-end latency budgets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathProfile>,
}

/// One profile-declared latency budget, converted onto the integer scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePath {
    /// The budget's name.
    pub name: String,
    /// The channel whose delivery must meet it.
    pub target: EdgeKey,
    /// The budget.
    pub budget: Nanos,
}

/// A parsed, scale-converted verification profile.
///
/// Parsing succeeds on any syntactically valid document; checking that it
/// *describes the manifest at hand* is a separate step
/// ([`Profile::validate_against`]) so a caller can report every mismatch
/// at once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
    service_times: BTreeMap<NodeId, ServiceTime>,
    rates: BTreeMap<NodeId, Rate>,
    paths: Vec<ProfilePath>,
    raw: ProfileDocument,
}

impl Profile {
    /// The empty profile: nothing declared, every obligation that needs a
    /// declaration reports it.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether this profile declares anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.service_times.is_empty() && self.rates.is_empty() && self.paths.is_empty()
    }

    /// Parse a profile from YAML.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::ProfileParse`] if the document is not valid
    /// YAML or has an unknown key, and [`VerifyError::ProfileInvalid`] if
    /// a declared value is not usable (an inverted interval, a rate with
    /// no exact rational form, a malformed `node/input` target).
    pub fn from_yaml_str(yaml: &str) -> crate::Result<Self> {
        let document: ProfileDocument =
            astrs_yaml::from_str(yaml).map_err(|err| VerifyError::ProfileParse {
                message: err.to_string(),
            })?;
        Self::from_document(document)
    }

    /// Read and parse a profile from a file.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::ProfileIo`] if the file cannot be read, plus
    /// anything [`Profile::from_yaml_str`] returns.
    pub fn from_file(path: &Path) -> crate::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|err| VerifyError::ProfileIo {
            path: path.display().to_string(),
            message: err.to_string(),
        })?;
        Self::from_yaml_str(&text)
    }

    /// Convert a parsed document onto the crate's integer scales.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::ProfileInvalid`] for a declaration that
    /// cannot be represented — see [`Profile::from_yaml_str`].
    pub fn from_document(document: ProfileDocument) -> crate::Result<Self> {
        let mut service_times = BTreeMap::new();
        let mut rates = BTreeMap::new();
        for (name, node) in &document.nodes {
            let id = NodeId::new(name.clone());
            if let Some(wcet) = node.wcet {
                let service = convert_wcet(name, wcet)?;
                if service.is_inverted() {
                    let (min, max) = wcet_bounds(wcet);
                    return Err(ProfileError::InvertedWcet {
                        node: name.clone(),
                        min: min.unwrap_or_default(),
                        max: max.unwrap_or_default(),
                    }
                    .into());
                }
                service_times.insert(id.clone(), service);
            }
            if let Some(rate) = node.rate {
                if rate <= 0.0 || !rate.is_finite() {
                    return Err(ProfileError::NonPositiveRate {
                        node: name.clone(),
                        rate,
                    }
                    .into());
                }
                let exact = Rate::from_f64(rate).ok_or(ProfileError::IrrationalRate {
                    node: name.clone(),
                    rate,
                })?;
                rates.insert(id, exact);
            }
        }

        let mut paths = Vec::new();
        let mut seen = BTreeMap::new();
        for path in &document.paths {
            if seen.insert(path.name.clone(), ()).is_some() {
                return Err(ProfileError::DuplicatePath {
                    name: path.name.clone(),
                }
                .into());
            }
            if path.budget <= 0.0 || !path.budget.is_finite() {
                return Err(ProfileError::NonPositiveBudget {
                    name: path.name.clone(),
                    budget: path.budget,
                }
                .into());
            }
            let budget =
                Nanos::from_secs_f64(path.budget).map_err(|reason| ProfileError::BadDuration {
                    what: format!("paths.{}.budget", path.name),
                    reason,
                })?;
            let target = parse_target(&path.target).ok_or(ProfileError::UnknownPort {
                section: "paths",
                node: path.target.clone(),
                port: String::new(),
            })?;
            paths.push(ProfilePath {
                name: path.name.clone(),
                target,
                budget,
            });
        }

        Ok(Self {
            service_times,
            rates,
            paths,
            raw: document,
        })
    }

    /// Check that everything this profile names exists in `graph`.
    ///
    /// Returns every mismatch rather than the first, so a caller can print
    /// a complete report — the same convention
    /// [`astrs_manifest::ValidationErrors`] follows.
    #[must_use]
    pub fn validate_against(&self, graph: &DataflowGraph) -> Vec<ProfileError> {
        let mut errors = Vec::new();
        for id in self.service_times.keys().chain(self.rates.keys()) {
            if !graph.nodes.contains_key(id) {
                errors.push(ProfileError::UnknownNode {
                    section: "nodes",
                    node: id.to_string(),
                });
            }
        }
        for path in &self.paths {
            if !graph.edges.contains_key(&path.target) {
                errors.push(ProfileError::DisconnectedPath {
                    name: path.name.clone(),
                    from: "<graph>".to_string(),
                    to: path.target.to_string(),
                });
            }
        }
        errors
    }

    /// One node's declared service time, or [`ServiceTime::UNKNOWN`].
    #[must_use]
    pub fn service_time_of(&self, id: &NodeId) -> ServiceTime {
        self.service_times
            .get(id)
            .copied()
            .unwrap_or(ServiceTime::UNKNOWN)
    }

    /// Every declared source rate.
    #[must_use]
    pub fn declared_rates(&self) -> BTreeMap<NodeId, Rate> {
        self.rates.clone()
    }

    /// Every declared latency budget.
    #[must_use]
    pub fn paths(&self) -> &[ProfilePath] {
        &self.paths
    }

    /// The document this profile was parsed from, for round-tripping.
    #[must_use]
    pub fn document(&self) -> &ProfileDocument {
        &self.raw
    }
}

/// Split the declared bounds out of a [`Wcet`].
fn wcet_bounds(wcet: Wcet) -> (Option<f64>, Option<f64>) {
    match wcet {
        Wcet::Exact(value) => (Some(value), Some(value)),
        Wcet::Interval { min, max } => (min, max),
    }
}

/// Convert a declared service time onto the integer nanosecond scale.
fn convert_wcet(node: &str, wcet: Wcet) -> crate::Result<ServiceTime> {
    let (min, max) = wcet_bounds(wcet);
    let convert = |value: Option<f64>, which: &str| -> crate::Result<Option<Nanos>> {
        value
            .map(|secs| {
                Nanos::from_secs_f64(secs).map_err(|reason| {
                    VerifyError::from(ProfileError::BadDuration {
                        what: format!("nodes.{node}.wcet.{which}"),
                        reason,
                    })
                })
            })
            .transpose()
    };
    Ok(ServiceTime {
        min: convert(min, "min")?,
        max: convert(max, "max")?,
    })
}

/// Parse a `node/input` target into an [`EdgeKey`].
fn parse_target(text: &str) -> Option<EdgeKey> {
    let (node, port) = text.split_once('/')?;
    if node.is_empty() || port.is_empty() || port.contains('/') {
        return None;
    }
    Some(EdgeKey::new(NodeId::new(node), PortName::new(port)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_manifest::Manifest;

    const SAMPLE: &str = "
nodes:
  camera:
    rate: 30
  detector:
    wcet: 0.012
  planner:
    wcet: { min: 0.001, max: 0.004 }
paths:
  - name: shutter-to-plan
    target: planner/dets
    budget: 0.1
";

    #[test]
    fn empty_profile_declares_nothing() {
        let profile = Profile::empty();
        assert!(profile.is_empty());
        assert_eq!(
            profile.service_time_of(&NodeId::new("anything")),
            ServiceTime::UNKNOWN
        );
        assert!(profile.declared_rates().is_empty());
        assert!(profile.paths().is_empty());
    }

    #[test]
    fn sample_profile_parses_every_section() {
        let profile = Profile::from_yaml_str(SAMPLE).expect("valid profile");
        assert!(!profile.is_empty());
        assert_eq!(
            profile.declared_rates().get(&NodeId::new("camera")),
            Rate::new(30, 1).as_ref()
        );
        assert_eq!(
            profile.service_time_of(&NodeId::new("detector")),
            ServiceTime::exact(Nanos::new(12_000_000))
        );
        assert_eq!(
            profile.service_time_of(&NodeId::new("planner")),
            ServiceTime {
                min: Some(Nanos::new(1_000_000)),
                max: Some(Nanos::new(4_000_000)),
            }
        );
        assert_eq!(profile.paths().len(), 1);
        assert_eq!(profile.paths()[0].budget, Nanos::new(100_000_000));
        assert_eq!(
            profile.paths()[0].target,
            EdgeKey::new(NodeId::new("planner"), PortName::new("dets"))
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Profile::from_yaml_str("nodes:\n  a:\n    wceat: 0.1\n")
            .expect_err("typo must not be ignored");
        assert!(matches!(err, VerifyError::ProfileParse { .. }), "{err:?}");
    }

    #[test]
    fn inverted_intervals_are_rejected() {
        let err = Profile::from_yaml_str("nodes:\n  a:\n    wcet: { min: 0.5, max: 0.1 }\n")
            .expect_err("inverted");
        assert!(
            matches!(
                err,
                VerifyError::ProfileInvalid(ProfileError::InvertedWcet { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn non_positive_rates_are_rejected() {
        let err = Profile::from_yaml_str("nodes:\n  a:\n    rate: 0\n").expect_err("zero rate");
        assert!(
            matches!(
                err,
                VerifyError::ProfileInvalid(ProfileError::NonPositiveRate { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn irrational_rates_are_rejected() {
        let err = Profile::from_yaml_str("nodes:\n  a:\n    rate: 0.333333333333\n")
            .expect_err("no exact rational form");
        assert!(
            matches!(
                err,
                VerifyError::ProfileInvalid(ProfileError::IrrationalRate { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn duplicate_path_names_are_rejected() {
        let yaml = "
paths:
  - { name: p, target: a/b, budget: 0.1 }
  - { name: p, target: a/c, budget: 0.2 }
";
        let err = Profile::from_yaml_str(yaml).expect_err("duplicate");
        assert!(
            matches!(
                err,
                VerifyError::ProfileInvalid(ProfileError::DuplicatePath { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn non_positive_budgets_are_rejected() {
        let yaml = "paths:\n  - { name: p, target: a/b, budget: 0 }\n";
        let err = Profile::from_yaml_str(yaml).expect_err("zero budget");
        assert!(
            matches!(
                err,
                VerifyError::ProfileInvalid(ProfileError::NonPositiveBudget { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn malformed_targets_are_rejected() {
        for target in ["noslash", "/leading", "trailing/", "a/b/c"] {
            let yaml = format!("paths:\n  - {{ name: p, target: {target}, budget: 0.1 }}\n");
            assert!(
                Profile::from_yaml_str(&yaml).is_err(),
                "`{target}` must be rejected"
            );
        }
    }

    #[test]
    fn validation_finds_names_the_graph_lacks() {
        let manifest = Manifest::from_yaml_str(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/10 }
    outputs: [frames]
",
        )
        .expect("valid yaml");
        manifest.validate().expect("valid manifest");
        let (graph, _) = DataflowGraph::from_manifest(&manifest).expect("graph");
        let profile = Profile::from_yaml_str(SAMPLE).expect("valid profile");
        let errors = profile.validate_against(&graph);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ProfileError::UnknownNode { node, .. } if node == "detector"))
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ProfileError::DisconnectedPath { .. }))
        );
    }

    #[test]
    fn a_matching_profile_validates_clean() {
        let manifest = Manifest::from_yaml_str(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/10 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
",
        )
        .expect("valid yaml");
        manifest.validate().expect("valid manifest");
        let (graph, _) = DataflowGraph::from_manifest(&manifest).expect("graph");
        let profile =
            Profile::from_yaml_str("nodes:\n  detector:\n    wcet: 0.005\n").expect("valid");
        assert!(profile.validate_against(&graph).is_empty());
    }

    #[test]
    fn documents_round_trip_through_yaml() {
        let profile = Profile::from_yaml_str(SAMPLE).expect("valid profile");
        let text = astrs_yaml::to_string(profile.document()).expect("serializable");
        let again = Profile::from_yaml_str(&text).expect("re-parses");
        assert_eq!(profile, again);
    }

    #[test]
    fn missing_file_is_reported_as_io() {
        let path = std::env::temp_dir().join("astrs-verify-no-such-profile.yaml");
        let err = Profile::from_file(&path).expect_err("missing file");
        assert!(matches!(err, VerifyError::ProfileIo { .. }), "{err:?}");
    }

    #[test]
    fn a_profile_reads_back_from_a_temporary_file() {
        let path = std::env::temp_dir().join(format!(
            "astrs-verify-profile-{}-{}.yaml",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, SAMPLE).expect("writable temp dir");
        let profile = Profile::from_file(&path).expect("valid profile");
        std::fs::remove_file(&path).expect("cleanup");
        assert_eq!(profile.paths().len(), 1);
    }
}
