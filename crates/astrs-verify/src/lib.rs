//! Proving an AstRS dataflow before it boots.
//!
//! `astrs validate --prove` discharges the graph obligations of blueprint
//! §15 through the [OxiZ] SMT solver: deadlock freedom, queue boundedness,
//! rate consistency, latency budgets and type-rule consistency. No
//! incumbent robotics middleware ships this.
//!
//! ```
//! use astrs_graph::DataflowGraph;
//! use astrs_manifest::Manifest;
//! use astrs_verify::{ProveOptions, prove};
//!
//! // Two nodes waiting on each other, with nothing to start them off.
//! let manifest = Manifest::from_yaml_str(
//!     "
//! nodes:
//!   - id: planner
//!     path: ./planner
//!     inputs: { pose: localizer/pose }
//!     outputs: [plan]
//!   - id: localizer
//!     path: ./localizer
//!     inputs: { plan: planner/plan }
//!     outputs: [pose]
//! ",
//! )?;
//! manifest.validate()?;
//! let (graph, _) = DataflowGraph::from_manifest(&manifest)?;
//!
//! let report = prove(&graph, &ProveOptions::default())?;
//! # #[cfg(feature = "verify")]
//! # {
//! assert!(report.has_violations());
//! let violation = report.violations().next().expect("a deadlock");
//! let counterexample = violation
//!     .discharge
//!     .counterexample()
//!     .expect("a violated obligation carries one");
//! assert!(counterexample.headline.contains("can never fire"));
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What the obligations are, in one paragraph each
//!
//! - **Deadlock freedom** ([`obligations::deadlock`]) — the graph becomes a
//!   Petri net with one transition per `(node, input)` pair, matching the
//!   merged event loop, and the encoding searches for an *initially-empty
//!   siphon*: a set of channels no transition can ever put a message into.
//!   A node whose inputs all lie in one can never fire, in any execution.
//!   The same system covers service and action clients, which block on a
//!   correlated reply and so are wedged by a starved or unwired
//!   correlation even when their other inputs are live.
//! - **Queue boundedness** ([`obligations::boundedness`]) — per node, the
//!   work its arrivals demand over one analysis window against the window
//!   itself. This is the one obligation that needs a declared service
//!   time; without one it reports so rather than guessing.
//! - **Rate consistency** ([`obligations::rate`]) — every declared rate
//!   expectation against the rate the graph delivers: a per-input
//!   `timeout` the producer's own period cannot meet, or a profile-declared
//!   node rate the wiring contradicts.
//! - **Latency budgets** ([`obligations::latency`]) — worst-case delivery
//!   age, `capacity × period` summed along the worst route, against each
//!   declared budget. Needs no service times: the bound comes from the
//!   declared queue depths and rates.
//! - **Type-rule consistency** ([`obligations::typing`]) — whether an
//!   edge's consumed type is reachable from its produced type under the
//!   manifest's `type_rules`, encoded as a search for a separating set.
//!
//! # Three things this crate refuses to do
//!
//! **Guess a number.** An obligation that needs a service time and has
//! none reports [`NotAttempted`] naming the node and the profile key that
//! would settle it. A default would turn every report into a claim about a
//! figure nobody chose.
//!
//! **Round a rate.** Every quantity rides an exact integer scale
//! ([`scale`]): rates are rationals, times are nanoseconds, and comparisons
//! multiply rather than divide. `astrs/timer/millis/3` is exactly 1000/3 Hz
//! from parse to proof.
//!
//! **Call an undecided obligation sound.** `unknown`, an exhausted budget,
//! a missing solver and a missing declaration are four distinct outcomes
//! ([`Discharge`]), none of which is "holds", and
//! [`VerificationReport::everything_discharged`] is how a caller tells
//! "nothing failed" from "everything was proved".
//!
//! # Determinism
//!
//! Two runs on the same input produce byte-identical reports. Not by
//! convention — structurally: variables are read back through this crate's
//! own ordered registry rather than the solver's hash map, every
//! collection on the path is a `BTreeMap`, the solver budget limits search
//! work rather than wall-clock time, and the arithmetic is integer, so
//! there is nothing left to vary.
//!
//! # Feature `verify`
//!
//! The solver lives behind the `verify` feature (blueprint §5.2). Without
//! it this crate still builds the model, the obligations and the report —
//! everything except the verdicts, which come back as
//! [`NotAttempted::SolverUnavailable`]. That is what lets `astrs-cli`
//! depend on it unconditionally and gate only the proving.
//!
//! [OxiZ]: https://crates.io/crates/oxiz

pub mod constraint;
pub mod model;
pub mod obligations;
pub mod scale;
pub mod smt;

mod counterexample;
mod error;
mod obligation;
mod profile;
mod render;
mod report;
#[cfg(test)]
mod test_support;

pub use constraint::{
    Assertion, Assignment, Compare, ConstraintSystem, EvaluationOutcome, Formula, Sort, Term,
    VarDecl, VarId, VarRegistry,
};
pub use counterexample::{Counterexample, Fact, Strength, TraceStep};
pub use error::{DurationRejection, ProfileError, Result, VerifyError};
pub use model::{
    Activation, Channel, EdgeTypes, Indeterminate, LatencyPath, Model, PathOrigin, Producer,
    ServiceTime,
};
pub use obligation::{Obligation, ObligationKind, Polarity, Subject};
pub use profile::{NodeProfile, PathProfile, Profile, ProfileDocument, ProfilePath, Wcet};
pub use render::{RenderOptions, render_human};
pub use report::{
    Caveat, Discharge, Inconclusive, NotAttempted, ObligationOutcome, Summary, VerificationReport,
};
pub use scale::{Nanos, Rate, Window};
pub use smt::{SolveOutcome, SolverBudget, solver_available};

use astrs_graph::DataflowGraph;
use astrs_manifest::Manifest;

/// How to run a proof.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProveOptions {
    /// The verification profile, if one was supplied.
    pub profile: Profile,
    /// How much search each obligation may consume.
    pub budget: SolverBudget,
}

impl ProveOptions {
    /// Options with a profile and the default budget.
    #[must_use]
    pub fn with_profile(profile: Profile) -> Self {
        Self {
            profile,
            budget: SolverBudget::default(),
        }
    }
}

/// Prove a dataflow graph.
///
/// Every obligation is discharged independently, so one that cannot be
/// settled never suppresses another. The returned report is the answer —
/// including when the answer is "this graph deadlocks": a refuted
/// obligation is the result, not a failure to produce one.
///
/// # Errors
///
/// Returns [`VerifyError`] when the *model* cannot be built: a declared
/// duration outside the representable range, declared rates with no common
/// integer analysis window, or a profile that does not describe this graph.
pub fn prove(graph: &DataflowGraph, options: &ProveOptions) -> Result<VerificationReport> {
    if let Some(error) = options.profile.validate_against(graph).into_iter().next() {
        return Err(error.into());
    }
    let model = Model::from_graph_with_profile(graph, &options.profile)?;
    Ok(report_for(&model, &options.budget))
}

/// Prove a manifest, running the graph construction step for the caller.
///
/// # Errors
///
/// As [`prove`], plus [`astrs_graph::GraphBuildError`] if the manifest does
/// not become a graph — which only happens for a manifest that skipped
/// [`Manifest::validate`](astrs_manifest::Manifest::validate).
pub fn prove_manifest(
    manifest: &Manifest,
    options: &ProveOptions,
) -> std::result::Result<VerificationReport, ProveManifestError> {
    let (graph, _) = DataflowGraph::from_manifest(manifest)?;
    Ok(prove(&graph, options)?)
}

/// What [`prove_manifest`] can fail with.
///
/// `PartialEq` but not `Eq`, following [`VerifyError`] — see its docs.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ProveManifestError {
    /// The manifest did not become a graph.
    #[error(transparent)]
    Graph(#[from] astrs_graph::GraphBuildError),
    /// The proof run could not be set up.
    #[error(transparent)]
    Verify(#[from] VerifyError),
}

/// Discharge every obligation against an already-built model.
///
/// Exposed for callers that build the model themselves (to inspect derived
/// rates first, say); [`prove`] is the ordinary entry point.
#[must_use]
pub fn report_for(model: &Model, budget: &SolverBudget) -> VerificationReport {
    let obligations = obligations::discharge_all(model, budget);
    let caveats = caveats_for(model, &obligations);
    let (node_count, channel_count) = model.size();
    VerificationReport {
        node_count,
        channel_count,
        window_seconds: model.window().seconds(),
        profile_applied: model.profile_applied(),
        obligations,
        caveats,
    }
}

/// Collect what the report is conditional on.
fn caveats_for(model: &Model, obligations: &[ObligationOutcome]) -> Vec<Caveat> {
    let mut caveats = Vec::new();
    if !model.profile_applied() {
        caveats.push(Caveat::NoProfile);
    }
    for (id, why) in &model.rates().indeterminate {
        caveats.push(match why {
            Indeterminate::UndeclaredSource => Caveat::UndeclaredSourceRate {
                node: id.to_string(),
            },
            other => Caveat::IndeterminateRate {
                node: id.to_string(),
                why: *other,
            },
        });
    }
    let latency_decided = obligations.iter().any(|outcome| {
        outcome.obligation.kind == ObligationKind::LatencyBudget && outcome.discharge.is_decided()
    });
    if latency_decided {
        caveats.push(Caveat::LatencyAssumesBoundedness);
    }
    caveats
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::test_support::{model_of, model_with_profile};

    fn graph_of(yaml: &str) -> DataflowGraph {
        let manifest = Manifest::from_yaml_str(yaml).expect("valid yaml");
        manifest.validate().expect("valid manifest");
        let (graph, _) = DataflowGraph::from_manifest(&manifest).expect("graph");
        graph
    }

    const SOUND: &str = "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/10 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 1
        timeout: 1.0
";

    #[test]
    fn a_report_covers_every_obligation_kind() {
        let report = report_for(&model_of(SOUND), &SolverBudget::default());
        for kind in ObligationKind::ALL {
            assert!(
                report.of_kind(kind).next().is_some(),
                "{kind} missing from the report"
            );
        }
        assert_eq!(report.node_count, 2);
        assert_eq!(report.channel_count, 2);
        assert_eq!(report.window_seconds, 1);
    }

    #[test]
    fn a_profile_free_report_records_the_missing_profile() {
        let report = report_for(&model_of(SOUND), &SolverBudget::default());
        assert!(report.caveats.contains(&Caveat::NoProfile));
        assert!(!report.profile_applied);
    }

    #[test]
    fn an_undeclared_source_becomes_a_caveat() {
        let report = report_for(
            &model_of(
                "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
  - id: sink
    path: ./sink
    inputs: { raw: sensor/raw }
",
            ),
            &SolverBudget::default(),
        );
        assert!(report.caveats.iter().any(|caveat| matches!(
            caveat,
            Caveat::UndeclaredSourceRate { node } if node == "sensor"
        )));
    }

    #[test]
    fn latency_results_carry_their_assumption() {
        let report = report_for(&model_of(SOUND), &SolverBudget::default());
        let decided = report
            .of_kind(ObligationKind::LatencyBudget)
            .any(|outcome| outcome.discharge.is_decided());
        assert_eq!(
            decided,
            report.caveats.contains(&Caveat::LatencyAssumesBoundedness),
            "the boundedness caveat rides exactly on a decided latency result"
        );
    }

    #[test]
    fn prove_rejects_a_profile_that_does_not_describe_the_graph() {
        let graph = graph_of(SOUND);
        let profile =
            Profile::from_yaml_str("nodes:\n  ghost:\n    wcet: 0.001\n").expect("valid profile");
        let err = prove(&graph, &ProveOptions::with_profile(profile))
            .expect_err("a profile naming an absent node must not be ignored");
        assert!(matches!(
            err,
            VerifyError::ProfileInvalid(ProfileError::UnknownNode { .. })
        ));
    }

    #[test]
    fn prove_manifest_runs_the_whole_pipeline() {
        let manifest = Manifest::from_yaml_str(SOUND).expect("valid yaml");
        manifest.validate().expect("valid manifest");
        let report = prove_manifest(&manifest, &ProveOptions::default()).expect("model builds");
        assert_eq!(report.node_count, 2);
    }

    #[test]
    fn options_carry_a_deterministic_budget_by_default() {
        assert!(ProveOptions::default().budget.is_deterministic());
        let with_profile = ProveOptions::with_profile(Profile::empty());
        assert!(with_profile.budget.is_deterministic());
    }

    #[test]
    fn a_profile_changes_what_the_report_can_say() {
        let without = report_for(&model_of(SOUND), &SolverBudget::default());
        let with = report_for(
            &model_with_profile(SOUND, "nodes:\n  detector:\n    wcet: 0.001\n"),
            &SolverBudget::default(),
        );
        assert!(!without.profile_applied);
        assert!(with.profile_applied);
        assert!(!with.caveats.contains(&Caveat::NoProfile));
    }

    #[test]
    fn reports_are_reproducible() {
        let first = report_for(&model_of(SOUND), &SolverBudget::default());
        let second = report_for(&model_of(SOUND), &SolverBudget::default());
        assert_eq!(first, second);
        assert_eq!(
            render_human(&first, &RenderOptions::default()),
            render_human(&second, &RenderOptions::default())
        );
    }

    #[test]
    fn prove_manifest_error_wraps_both_causes() {
        let err = ProveManifestError::from(VerifyError::ScaleOverflow {
            needed: 2,
            limit: 1,
        });
        assert!(err.to_string().contains("analysis window"));
    }
}
