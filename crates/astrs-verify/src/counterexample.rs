//! Counterexamples: what a violated obligation hands back.
//!
//! A solver's answer to "is this system satisfiable" is one bit plus a
//! variable assignment, and neither is something to show a robotics
//! engineer. A [`Counterexample`] is the translation: the headline claim,
//! the concrete facts that make it true, and a narrative trace naming the
//! nodes and channels involved, in that order.
//!
//! Every counterexample this crate emits has been **replayed** against the
//! constraint system that produced it ([`ConstraintSystem::evaluate`]) —
//! see [`Counterexample::cross_checked`]. A verifier that prints whatever
//! its solver says has no way to notice a translation bug in its own
//! encoding, and a wrong counterexample is worse than none.

use std::fmt;

use crate::constraint::{Assignment, ConstraintSystem};

/// One concrete fact underpinning a counterexample.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fact {
    /// What the value measures, e.g. "worst-case delivery latency".
    pub label: String,
    /// The value, already rendered with its unit.
    pub value: String,
}

impl Fact {
    /// Build a fact.
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

impl fmt::Display for Fact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.label, self.value)
    }
}

/// One step of a counterexample's narrative.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraceStep {
    /// The node or channel this step is about.
    pub actor: String,
    /// What happens to it.
    pub detail: String,
}

impl TraceStep {
    /// Build a trace step.
    #[must_use]
    pub fn new(actor: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            actor: actor.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for TraceStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} — {}", self.actor, self.detail)
    }
}

/// How firmly a counterexample refutes its obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    /// The counterexample is a **proof**: the encoding is exact for this
    /// obligation, so the failure it describes really does occur.
    ///
    /// Used where the model does not over-approximate — a starving siphon
    /// (an initially-empty siphon stays empty in *every* reachable
    /// marking, a standard Petri-net invariant), a wait-for cycle, an
    /// arithmetic bound over exactly derived rates.
    Proof,
    /// The counterexample is a **witness under the model's assumptions**:
    /// the failure follows from what was declared, and the declarations
    /// may be conservative.
    ///
    /// Used where a bound came from a declared interval rather than from
    /// the graph itself, so tightening the declaration could remove the
    /// failure.
    UnderAssumptions,
}

impl Strength {
    /// How this strength reads in a report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Proof => "proved",
            Self::UnderAssumptions => "under the declared assumptions",
        }
    }
}

/// A violated obligation, translated for a reader.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Counterexample {
    /// The one-line claim, e.g. "`detector` can never fire".
    pub headline: String,
    /// How firmly this refutes the obligation.
    pub strength: Strength,
    /// The concrete numbers behind the claim, in a fixed order.
    pub facts: Vec<Fact>,
    /// The narrative, in order.
    pub trace: Vec<TraceStep>,
    /// What to do about it, when there is a concrete answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    /// The raw satisfying assignment, one `name = value` line per
    /// variable, in variable order.
    ///
    /// Kept because a counterexample a reader cannot check is a
    /// counterexample they have to take on faith; this is the bridge back
    /// to the SMT-LIB2 system in
    /// [`crate::ObligationOutcome::encoded_system`].
    pub assignment: Vec<String>,
    /// Whether the assignment was replayed against the constraint system
    /// and satisfied every assertion.
    ///
    /// `false` means the solver and this crate's own evaluator disagreed,
    /// which is reported rather than hidden: the counterexample is still
    /// shown, flagged, because a disagreement is a bug worth seeing.
    pub cross_checked: bool,
}

impl Counterexample {
    /// Build a counterexample and cross-check its assignment against the
    /// system it came from.
    ///
    /// The replay is the whole point of this constructor: there is no way
    /// to build a [`Counterexample`] from a solver answer without going
    /// through it.
    #[must_use]
    pub fn from_assignment(
        headline: impl Into<String>,
        strength: Strength,
        system: &ConstraintSystem,
        assignment: &Assignment,
    ) -> Self {
        let mut completed = assignment.clone();
        completed.complete_with(system.registry(), 0);
        let cross_checked = system.evaluate(&completed).is_satisfied();
        Self {
            headline: headline.into(),
            strength,
            facts: Vec::new(),
            trace: Vec::new(),
            remedy: None,
            assignment: completed.render(system.registry()),
            cross_checked,
        }
    }

    /// Add a fact.
    #[must_use]
    pub fn with_fact(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push(Fact::new(label, value));
        self
    }

    /// Add a trace step.
    #[must_use]
    pub fn with_step(mut self, actor: impl Into<String>, detail: impl Into<String>) -> Self {
        self.trace.push(TraceStep::new(actor, detail));
        self
    }

    /// Add several trace steps.
    #[must_use]
    pub fn with_steps(mut self, steps: impl IntoIterator<Item = TraceStep>) -> Self {
        self.trace.extend(steps);
        self
    }

    /// Attach a suggested remedy.
    #[must_use]
    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::constraint::{Formula, Term};

    fn system_and_model() -> (ConstraintSystem, Assignment) {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "the x count");
        let p = system.bool_var("p", "the p flag");
        system.assert("g", "x exceeds 2", Formula::gt(Term::var(x), 2i128));
        system.assert("g", "p holds", Formula::bool_var(p));
        let mut assignment = Assignment::new();
        assignment.set(x, 5);
        assignment.set_bool(p, true);
        (system, assignment)
    }

    #[test]
    fn a_satisfying_assignment_cross_checks() {
        let (system, assignment) = system_and_model();
        let counterexample =
            Counterexample::from_assignment("boom", Strength::Proof, &system, &assignment);
        assert!(counterexample.cross_checked);
        assert_eq!(counterexample.headline, "boom");
        assert_eq!(
            counterexample.assignment,
            vec!["x = 5".to_string(), "p = true".to_string()]
        );
    }

    #[test]
    fn a_wrong_assignment_is_flagged_not_hidden() {
        let (system, _) = system_and_model();
        let mut wrong = Assignment::new();
        wrong.set(crate::constraint::VarId::from_index(0), 1);
        let counterexample =
            Counterexample::from_assignment("boom", Strength::Proof, &system, &wrong);
        assert!(
            !counterexample.cross_checked,
            "a disagreement must be reported, not swallowed"
        );
        assert_eq!(counterexample.headline, "boom");
    }

    #[test]
    fn partial_assignments_are_completed_before_replay() {
        let (system, _) = system_and_model();
        let mut partial = Assignment::new();
        partial.set(crate::constraint::VarId::from_index(0), 7);
        let counterexample =
            Counterexample::from_assignment("boom", Strength::Proof, &system, &partial);
        // `p` was filled with false, so the second assertion fails; the
        // point is that completion happened and the check ran.
        assert_eq!(counterexample.assignment.len(), 2);
        assert!(!counterexample.cross_checked);
    }

    #[test]
    fn builders_accumulate_in_order() {
        let (system, assignment) = system_and_model();
        let counterexample =
            Counterexample::from_assignment("boom", Strength::Proof, &system, &assignment)
                .with_fact("arrival rate", "50 Hz")
                .with_fact("service rate", "30 Hz")
                .with_step("camera", "emits a frame every 20ms")
                .with_steps([TraceStep::new("detector", "needs 33ms per frame")])
                .with_remedy("raise `queue_size` or lower the trigger rate");
        assert_eq!(counterexample.facts.len(), 2);
        assert_eq!(counterexample.facts[0].label, "arrival rate");
        assert_eq!(counterexample.trace.len(), 2);
        assert_eq!(counterexample.trace[1].actor, "detector");
        assert!(counterexample.remedy.is_some());
    }

    #[test]
    fn facts_and_steps_render() {
        assert_eq!(Fact::new("a", "b").to_string(), "a: b");
        assert_eq!(TraceStep::new("n", "waits").to_string(), "n — waits");
    }

    #[test]
    fn strengths_are_labelled_distinctly() {
        assert_ne!(Strength::Proof.label(), Strength::UnderAssumptions.label());
    }

    #[test]
    fn counterexamples_serialize_and_round_trip() {
        let (system, assignment) = system_and_model();
        let counterexample =
            Counterexample::from_assignment("boom", Strength::Proof, &system, &assignment)
                .with_fact("x", "5");
        let json = serde_json::to_string(&counterexample).expect("serializable");
        let back: Counterexample = serde_json::from_str(&json).expect("round trips");
        assert_eq!(back, counterexample);
    }

    #[test]
    fn rendering_is_deterministic() {
        let (system, assignment) = system_and_model();
        let build =
            || Counterexample::from_assignment("boom", Strength::Proof, &system, &assignment);
        assert_eq!(build(), build());
    }
}
