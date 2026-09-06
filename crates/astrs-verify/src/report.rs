//! The proof report: every obligation, how it was settled, and what the
//! result is conditional on.
//!
//! # Three outcomes, not two
//!
//! [`Discharge`] has four arms, and the two beyond "holds"/"violated" are
//! the important ones. A solver can answer `unknown`, hit a resource
//! budget, or not be compiled into the build at all; an obligation can
//! need a declaration the manifest never made. Folding any of those into
//! "holds" would let a verifier report a graph sound because it gave up —
//! the single worst failure mode a tool like this can have. So they are
//! their own outcomes, they are counted separately in
//! [`VerificationReport::summary`], and
//! [`VerificationReport::everything_discharged`] is false whenever one is
//! present.

use std::fmt;

use crate::counterexample::Counterexample;
use crate::model::Indeterminate;
use crate::obligation::{Obligation, ObligationKind};

/// Why an obligation was not attempted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
#[non_exhaustive]
pub enum NotAttempted {
    /// The crate was built without the `verify` feature, so no solver is
    /// available.
    SolverUnavailable,
    /// The obligation needs a service time for a node and none was
    /// declared. Supply one in a verification profile
    /// ([`crate::Profile`]).
    NoServiceTime {
        /// The node whose service time is missing.
        node: String,
    },
    /// The obligation needs a firing rate the model could not derive.
    NoDerivedRate {
        /// The node whose rate is unknown.
        node: String,
        /// Why it is unknown.
        why: Indeterminate,
    },
    /// The graph contains nothing this obligation applies to — no declared
    /// budget, no typed edge, no correlation.
    ///
    /// Not a gap in the proof: an obligation with no instances is
    /// vacuously satisfied, so it does not stop
    /// [`VerificationReport::everything_discharged`] from being true. See
    /// [`NotAttempted::is_gap`].
    NothingToProve,
    /// The obligation's arithmetic does not fit the crate's exact integer
    /// scales for this graph.
    ScaleOverflow {
        /// What overflowed.
        detail: String,
    },
}

impl NotAttempted {
    /// Whether this reason leaves a genuine hole in the proof.
    ///
    /// Every reason does except [`NotAttempted::NothingToProve`]: a
    /// missing service time, an underived rate, an overflowing scale and a
    /// missing solver each mean something the graph asserts was *not*
    /// checked. "The graph declares no latency budget" means there was
    /// nothing to check, which is a different statement and must not
    /// downgrade an otherwise complete run.
    #[must_use]
    pub const fn is_gap(&self) -> bool {
        !matches!(self, Self::NothingToProve)
    }
}

impl fmt::Display for NotAttempted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SolverUnavailable => f.write_str(
                "built without the `verify` feature, so no solver is available (rebuild with `--features verify`)",
            ),
            Self::NoServiceTime { node } => write!(
                f,
                "node `{node}` has no declared service time; declare `nodes.{node}.wcet` in a verification profile"
            ),
            Self::NoDerivedRate { node, why } => {
                write!(f, "node `{node}` has no derived firing rate: {}", why.explanation())
            }
            Self::NothingToProve => f.write_str("the graph declares nothing this obligation applies to"),
            Self::ScaleOverflow { detail } => write!(f, "the obligation's arithmetic does not fit an exact integer scale: {detail}"),
        }
    }
}

/// Why an attempted obligation reached no verdict.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
#[non_exhaustive]
pub enum Inconclusive {
    /// The solver answered `unknown`.
    SolverUnknown,
    /// The solver hit a resource budget before deciding.
    BudgetExhausted {
        /// Which budget, as the solver named it.
        limit: String,
    },
    /// The solver reported a satisfying assignment that this crate's own
    /// evaluator then rejected.
    ///
    /// Reported rather than trusted: the two disagreeing means the
    /// encoding was translated wrongly somewhere, and neither answer is
    /// worth acting on.
    ModelRejected {
        /// What the replay found.
        detail: String,
    },
}

impl fmt::Display for Inconclusive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SolverUnknown => f.write_str("the solver could not decide the obligation"),
            Self::BudgetExhausted { limit } => {
                write!(f, "the solver hit its {limit} budget before deciding")
            }
            Self::ModelRejected { detail } => write!(
                f,
                "the solver reported a model this crate's own replay rejected ({detail}); neither answer is trustworthy"
            ),
        }
    }
}

/// How one obligation was settled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Discharge {
    /// The obligation holds.
    Holds {
        /// What exactly was ruled out, phrased for a reader.
        claim: String,
    },
    /// The obligation is violated, with a counterexample.
    Violated {
        /// The counterexample.
        counterexample: Box<Counterexample>,
    },
    /// The obligation was not attempted.
    NotAttempted {
        /// Why.
        reason: NotAttempted,
    },
    /// The obligation was attempted but reached no verdict.
    Inconclusive {
        /// Why.
        reason: Inconclusive,
    },
}

impl Discharge {
    /// Whether the obligation was proved to hold.
    #[must_use]
    pub fn holds(&self) -> bool {
        matches!(self, Self::Holds { .. })
    }

    /// Whether the obligation was refuted.
    #[must_use]
    pub fn is_violated(&self) -> bool {
        matches!(self, Self::Violated { .. })
    }

    /// Whether the obligation reached *any* verdict.
    #[must_use]
    pub fn is_decided(&self) -> bool {
        self.holds() || self.is_violated()
    }

    /// The counterexample, when there is one.
    #[must_use]
    pub fn counterexample(&self) -> Option<&Counterexample> {
        match self {
            Self::Violated { counterexample } => Some(counterexample),
            _ => None,
        }
    }

    /// Whether this outcome leaves a hole in the proof.
    ///
    /// Distinct from `!is_decided()`: an obligation with nothing to prove
    /// is undecided *and* complete — see [`NotAttempted::is_gap`].
    #[must_use]
    pub const fn leaves_a_gap(&self) -> bool {
        match self {
            Self::Holds { .. } | Self::Violated { .. } => false,
            Self::NotAttempted { reason } => reason.is_gap(),
            Self::Inconclusive { .. } => true,
        }
    }

    /// A one-word status for a compact report line.
    #[must_use]
    pub const fn status(&self) -> &'static str {
        match self {
            Self::Holds { .. } => "holds",
            Self::Violated { .. } => "VIOLATED",
            Self::NotAttempted { .. } => "not attempted",
            Self::Inconclusive { .. } => "inconclusive",
        }
    }
}

/// One obligation and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObligationOutcome {
    /// Which obligation.
    pub obligation: Obligation,
    /// How it was settled.
    pub discharge: Discharge,
    /// The SMT-LIB2 system that was solved, when one was built.
    ///
    /// Kept so a reader can see exactly what was claimed rather than
    /// taking the verdict on trust; it is also the artefact the crate's
    /// determinism test compares, since it is a pure function of the
    /// model and needs no solver to produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_system: Option<String>,
}

impl ObligationOutcome {
    /// Build an outcome with no encoded system (nothing was attempted).
    #[must_use]
    pub fn unattempted(obligation: Obligation, reason: NotAttempted) -> Self {
        Self {
            obligation,
            discharge: Discharge::NotAttempted { reason },
            encoded_system: None,
        }
    }

    /// Build an outcome that carries its encoded system.
    #[must_use]
    pub fn decided(obligation: Obligation, discharge: Discharge, system: String) -> Self {
        Self {
            obligation,
            discharge,
            encoded_system: Some(system),
        }
    }

    /// Build an outcome for an obligation that *was* encoded but could not
    /// be solved.
    ///
    /// The distinction from [`ObligationOutcome::unattempted`] matters: the
    /// encoding is the substantive half of a proof, and a build without the
    /// `verify` feature still produces it. Keeping it means the claim can
    /// be read, diffed and reviewed even where no solver ran — and it is
    /// what lets the encodings be tested in every build.
    #[must_use]
    pub fn encoded_but_unsolved(
        obligation: Obligation,
        reason: NotAttempted,
        system: String,
    ) -> Self {
        Self {
            obligation,
            discharge: Discharge::NotAttempted { reason },
            encoded_system: Some(system),
        }
    }
}

/// Something the whole report is conditional on.
///
/// Caveats are not failures — they are the honest small print. A latency
/// bound derived on the assumption that queues do not overflow is only as
/// good as the boundedness obligation; a rate claim about a graph with an
/// undeclared source only covers the part of the graph the declared rates
/// reach. Saying so is the difference between a proof and a claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "caveat")]
#[non_exhaustive]
pub enum Caveat {
    /// A source node's rate is undeclared, so nothing downstream of it has
    /// a derived rate.
    UndeclaredSourceRate {
        /// The source node.
        node: String,
    },
    /// A node's rate is indeterminate for a structural reason.
    IndeterminateRate {
        /// The node.
        node: String,
        /// Why.
        why: Indeterminate,
    },
    /// Latency results assume queue boundedness: the worst-case queueing
    /// delay bound `capacity × period` is only valid while the consumer
    /// keeps up, which is what
    /// [`ObligationKind::QueueBoundedness`] establishes.
    LatencyAssumesBoundedness,
    /// No verification profile was supplied, so no service time is known
    /// for any node.
    NoProfile,
}

impl fmt::Display for Caveat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UndeclaredSourceRate { node } => write!(
                f,
                "node `{node}` emits at an undeclared rate; no rate, boundedness or latency claim covers anything downstream of it"
            ),
            Self::IndeterminateRate { node, why } => {
                write!(f, "node `{node}` has no derived rate: {}", why.explanation())
            }
            Self::LatencyAssumesBoundedness => f.write_str(
                "latency bounds assume queues do not overflow; they hold only where queue boundedness does",
            ),
            Self::NoProfile => f.write_str(
                "no verification profile was supplied, so no node has a declared service time",
            ),
        }
    }
}

/// How many obligations landed in each outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    /// Obligations proved to hold.
    pub holds: usize,
    /// Obligations refuted.
    pub violated: usize,
    /// Obligations not attempted.
    pub not_attempted: usize,
    /// Obligations attempted without a verdict.
    pub inconclusive: usize,
}

impl Summary {
    /// The total number of obligations.
    #[must_use]
    pub fn total(&self) -> usize {
        self.holds + self.violated + self.not_attempted + self.inconclusive
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} obligation(s): {} hold, {} violated, {} not attempted, {} inconclusive",
            self.total(),
            self.holds,
            self.violated,
            self.not_attempted,
            self.inconclusive
        )
    }
}

/// The complete result of proving one dataflow.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerificationReport {
    /// How many nodes the model carried.
    pub node_count: usize,
    /// How many channels the model carried.
    pub channel_count: usize,
    /// The analysis window, in seconds, every flow count was expressed
    /// over — see [`crate::scale`].
    pub window_seconds: u128,
    /// Whether a verification profile contributed.
    pub profile_applied: bool,
    /// Every obligation, in [`ObligationKind::ALL`] order and then by
    /// subject.
    pub obligations: Vec<ObligationOutcome>,
    /// What the results are conditional on.
    pub caveats: Vec<Caveat>,
}

impl VerificationReport {
    /// How many obligations landed in each outcome.
    #[must_use]
    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for outcome in &self.obligations {
            match outcome.discharge {
                Discharge::Holds { .. } => summary.holds += 1,
                Discharge::Violated { .. } => summary.violated += 1,
                Discharge::NotAttempted { .. } => summary.not_attempted += 1,
                Discharge::Inconclusive { .. } => summary.inconclusive += 1,
            }
        }
        summary
    }

    /// Whether any obligation was refuted.
    #[must_use]
    pub fn has_violations(&self) -> bool {
        self.obligations
            .iter()
            .any(|outcome| outcome.discharge.is_violated())
    }

    /// Whether the run left no hole in the proof.
    ///
    /// False when an obligation was skipped for want of a declaration, or
    /// left undecided — a report can be free of violations and still not
    /// amount to a proof, and this is how a caller tells the two apart.
    /// An obligation with *nothing to prove* is not a hole; see
    /// [`NotAttempted::is_gap`].
    #[must_use]
    pub fn everything_discharged(&self) -> bool {
        self.gap_count() == 0
    }

    /// How many obligations left a hole in the proof.
    #[must_use]
    pub fn gap_count(&self) -> usize {
        self.obligations
            .iter()
            .filter(|outcome| outcome.discharge.leaves_a_gap())
            .count()
    }

    /// Every violated obligation.
    pub fn violations(&self) -> impl Iterator<Item = &ObligationOutcome> {
        self.obligations
            .iter()
            .filter(|outcome| outcome.discharge.is_violated())
    }

    /// Every outcome for one obligation kind.
    pub fn of_kind(&self, kind: ObligationKind) -> impl Iterator<Item = &ObligationOutcome> {
        self.obligations
            .iter()
            .filter(move |outcome| outcome.obligation.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::counterexample::Strength;
    use crate::obligation::Subject;

    fn counterexample() -> Counterexample {
        Counterexample {
            headline: "boom".to_string(),
            strength: Strength::Proof,
            facts: Vec::new(),
            trace: Vec::new(),
            remedy: None,
            assignment: Vec::new(),
            cross_checked: true,
        }
    }

    fn report(discharges: Vec<Discharge>) -> VerificationReport {
        VerificationReport {
            node_count: 2,
            channel_count: 2,
            window_seconds: 1,
            profile_applied: false,
            obligations: discharges
                .into_iter()
                .enumerate()
                .map(|(index, discharge)| ObligationOutcome {
                    obligation: Obligation::about(
                        ObligationKind::DeadlockFreedom,
                        Subject::Node(format!("n{index}")),
                    ),
                    discharge,
                    encoded_system: None,
                })
                .collect(),
            caveats: Vec::new(),
        }
    }

    #[test]
    fn summary_counts_every_arm() {
        let report = report(vec![
            Discharge::Holds {
                claim: "fine".to_string(),
            },
            Discharge::Violated {
                counterexample: Box::new(counterexample()),
            },
            Discharge::NotAttempted {
                reason: NotAttempted::SolverUnavailable,
            },
            Discharge::Inconclusive {
                reason: Inconclusive::SolverUnknown,
            },
        ]);
        let summary = report.summary();
        assert_eq!(summary.holds, 1);
        assert_eq!(summary.violated, 1);
        assert_eq!(summary.not_attempted, 1);
        assert_eq!(summary.inconclusive, 1);
        assert_eq!(summary.total(), 4);
        assert!(summary.to_string().contains("4 obligation(s)"));
    }

    #[test]
    fn a_skipped_obligation_is_not_a_proof() {
        let report = report(vec![
            Discharge::Holds {
                claim: "fine".to_string(),
            },
            Discharge::NotAttempted {
                reason: NotAttempted::NoServiceTime {
                    node: "detector".to_string(),
                },
            },
        ]);
        assert!(!report.has_violations());
        assert!(
            !report.everything_discharged(),
            "no violations is not the same as everything proved"
        );
        assert_eq!(report.gap_count(), 1);
    }

    #[test]
    fn an_obligation_with_nothing_to_prove_is_not_a_hole() {
        let report = report(vec![
            Discharge::Holds {
                claim: "fine".to_string(),
            },
            Discharge::NotAttempted {
                reason: NotAttempted::NothingToProve,
            },
        ]);
        assert!(
            report.everything_discharged(),
            "a vacuous obligation must not downgrade a complete run"
        );
        assert_eq!(report.gap_count(), 0);
    }

    #[test]
    fn every_other_skip_reason_is_a_gap() {
        for reason in [
            NotAttempted::SolverUnavailable,
            NotAttempted::NoServiceTime {
                node: "a".to_string(),
            },
            NotAttempted::NoDerivedRate {
                node: "a".to_string(),
                why: Indeterminate::FeedbackCycle,
            },
            NotAttempted::ScaleOverflow {
                detail: "x".to_string(),
            },
        ] {
            assert!(reason.is_gap(), "{reason:?}");
        }
        assert!(!NotAttempted::NothingToProve.is_gap());
    }

    #[test]
    fn an_inconclusive_obligation_is_not_a_proof() {
        let report = report(vec![Discharge::Inconclusive {
            reason: Inconclusive::BudgetExhausted {
                limit: "max conflicts".to_string(),
            },
        }]);
        assert!(!report.everything_discharged());
        assert!(!report.has_violations());
    }

    #[test]
    fn a_fully_held_report_is_a_proof() {
        let report = report(vec![
            Discharge::Holds {
                claim: "a".to_string(),
            },
            Discharge::Holds {
                claim: "b".to_string(),
            },
        ]);
        assert!(report.everything_discharged());
        assert!(!report.has_violations());
    }

    #[test]
    fn violations_are_enumerable() {
        let report = report(vec![
            Discharge::Holds {
                claim: "a".to_string(),
            },
            Discharge::Violated {
                counterexample: Box::new(counterexample()),
            },
        ]);
        assert_eq!(report.violations().count(), 1);
        assert_eq!(report.of_kind(ObligationKind::DeadlockFreedom).count(), 2);
        assert_eq!(report.of_kind(ObligationKind::LatencyBudget).count(), 0);
    }

    #[test]
    fn discharge_predicates_agree() {
        let holds = Discharge::Holds {
            claim: "x".to_string(),
        };
        assert!(holds.holds() && holds.is_decided() && !holds.is_violated());
        assert_eq!(holds.status(), "holds");
        assert!(holds.counterexample().is_none());

        let violated = Discharge::Violated {
            counterexample: Box::new(counterexample()),
        };
        assert!(violated.is_violated() && violated.is_decided() && !violated.holds());
        assert_eq!(violated.status(), "VIOLATED");
        assert!(violated.counterexample().is_some());

        let skipped = Discharge::NotAttempted {
            reason: NotAttempted::SolverUnavailable,
        };
        assert!(!skipped.is_decided());
        assert_eq!(skipped.status(), "not attempted");
    }

    #[test]
    fn not_attempted_reasons_explain_the_fix() {
        let reason = NotAttempted::NoServiceTime {
            node: "detector".to_string(),
        };
        let text = reason.to_string();
        assert!(text.contains("nodes.detector.wcet"), "{text}");
        assert!(
            NotAttempted::SolverUnavailable
                .to_string()
                .contains("--features verify")
        );
        assert!(
            NotAttempted::NoDerivedRate {
                node: "a".to_string(),
                why: Indeterminate::FeedbackCycle,
            }
            .to_string()
            .contains("feedback cycle")
        );
        assert!(
            !NotAttempted::ScaleOverflow {
                detail: "x".to_string()
            }
            .to_string()
            .is_empty()
        );
        assert!(!NotAttempted::NothingToProve.to_string().is_empty());
    }

    #[test]
    fn inconclusive_reasons_render() {
        assert!(
            Inconclusive::SolverUnknown
                .to_string()
                .contains("could not decide")
        );
        assert!(
            Inconclusive::BudgetExhausted {
                limit: "max conflicts".to_string()
            }
            .to_string()
            .contains("max conflicts")
        );
        assert!(
            Inconclusive::ModelRejected {
                detail: "assertion #1 is false".to_string()
            }
            .to_string()
            .contains("neither answer is trustworthy")
        );
    }

    #[test]
    fn caveats_render_with_their_subject() {
        assert!(
            Caveat::UndeclaredSourceRate {
                node: "sensor".to_string()
            }
            .to_string()
            .contains("sensor")
        );
        assert!(
            Caveat::IndeterminateRate {
                node: "a".to_string(),
                why: Indeterminate::AperiodicSource,
            }
            .to_string()
            .contains("aperiodic")
        );
        assert!(
            Caveat::LatencyAssumesBoundedness
                .to_string()
                .contains("overflow")
        );
        assert!(Caveat::NoProfile.to_string().contains("service time"));
    }

    #[test]
    fn outcome_constructors_set_the_system_field() {
        let obligation = Obligation::whole_graph(ObligationKind::TypeConsistency);
        let skipped =
            ObligationOutcome::unattempted(obligation.clone(), NotAttempted::NothingToProve);
        assert!(skipped.encoded_system.is_none());
        let decided = ObligationOutcome::decided(
            obligation.clone(),
            Discharge::Holds {
                claim: "ok".to_string(),
            },
            "(check-sat)".to_string(),
        );
        assert_eq!(decided.encoded_system.as_deref(), Some("(check-sat)"));
        let unsolved = ObligationOutcome::encoded_but_unsolved(
            obligation,
            NotAttempted::SolverUnavailable,
            "(check-sat)".to_string(),
        );
        assert_eq!(unsolved.encoded_system.as_deref(), Some("(check-sat)"));
        assert!(!unsolved.discharge.is_decided());
    }

    #[test]
    fn reports_serialize_and_round_trip() {
        let report = report(vec![Discharge::Violated {
            counterexample: Box::new(counterexample()),
        }]);
        let json = serde_json::to_string(&report).expect("serializable");
        let back: VerificationReport = serde_json::from_str(&json).expect("round trips");
        assert_eq!(back, report);
    }
}
