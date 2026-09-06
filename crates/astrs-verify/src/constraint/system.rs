//! A complete constraint system: declarations plus assertions, with a
//! canonical textual form and a solver-free evaluator.

use std::collections::BTreeMap;
use std::fmt;

use super::formula::{Formula, evaluate_term};
use super::term::Term;
use super::var::{Sort, VarDecl, VarId, VarRegistry};

/// One named group of assertions.
///
/// Encodings assert in labelled groups (`flow`, `capacity`, `violation`,
/// …) so a rendered system reads like the argument it encodes, and so a
/// reader of a counterexample can see *which* part of the model the
/// solver satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    /// The group this assertion belongs to.
    pub group: &'static str,
    /// A one-line explanation of what it says, in the model's own terms.
    pub explanation: String,
    /// The assertion itself.
    pub formula: Formula,
}

/// A quantifier-free linear-integer constraint system.
///
/// Built by the obligation encoders in [`crate::obligations`] and consumed
/// either by [`crate::smt`] (which discharges it) or by
/// [`ConstraintSystem::evaluate`] (which re-checks a candidate model
/// without a solver — the independent cross-check every counterexample
/// this crate reports has passed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConstraintSystem {
    registry: VarRegistry,
    assertions: Vec<Assertion>,
}

impl ConstraintSystem {
    /// An empty system.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare (or look up) an integer variable.
    pub fn int_var(&mut self, name: impl Into<String>, label: impl Into<String>) -> VarId {
        self.registry.declare(name, Sort::Int, label)
    }

    /// Declare (or look up) a boolean variable.
    pub fn bool_var(&mut self, name: impl Into<String>, label: impl Into<String>) -> VarId {
        self.registry.declare(name, Sort::Bool, label)
    }

    /// Add one assertion to a named group.
    ///
    /// Trivially-true assertions are dropped rather than recorded: they
    /// carry no information, and keeping them would make the rendered
    /// system's size depend on incidental structure (how many optional
    /// fields a manifest happened to set) rather than on the argument.
    pub fn assert(
        &mut self,
        group: &'static str,
        explanation: impl Into<String>,
        formula: Formula,
    ) {
        if formula == Formula::True {
            return;
        }
        self.assertions.push(Assertion {
            group,
            explanation: explanation.into(),
            formula,
        });
    }

    /// This system's variable registry.
    #[must_use]
    pub fn registry(&self) -> &VarRegistry {
        &self.registry
    }

    /// This system's assertions, in the order they were added.
    #[must_use]
    pub fn assertions(&self) -> &[Assertion] {
        &self.assertions
    }

    /// How many variables are declared.
    #[must_use]
    pub fn var_count(&self) -> usize {
        self.registry.len()
    }

    /// How many assertions were kept.
    #[must_use]
    pub fn assertion_count(&self) -> usize {
        self.assertions.len()
    }

    /// Whether the system says nothing at all.
    #[must_use]
    pub fn is_trivial(&self) -> bool {
        self.assertions.is_empty()
    }

    /// Whether any assertion is the constant `false`, which makes the
    /// system unsatisfiable without consulting a solver.
    ///
    /// Encoders can produce this legitimately — asserting a violation
    /// that the model's own constants already rule out — and reporting it
    /// directly is both faster and more honest than routing a
    /// trivially-contradictory system through a solver.
    #[must_use]
    pub fn is_trivially_unsat(&self) -> bool {
        self.assertions
            .iter()
            .any(|assertion| assertion.formula == Formula::False)
    }

    /// Evaluate every assertion under `assignment`, returning the first
    /// assertion that is false or not fully assigned.
    ///
    /// This is the independent cross-check described on
    /// [`crate::Counterexample`]: a model the solver called satisfying is
    /// replayed here, against the same constraint objects, before the
    /// crate is willing to print it as a proof. A disagreement means the
    /// encoding and the solver read the system differently, which is a bug
    /// worth surfacing rather than a counterexample worth trusting.
    #[must_use]
    pub fn evaluate(&self, assignment: &Assignment) -> EvaluationOutcome {
        let value_of = |id: VarId| assignment.get(id);
        for (index, assertion) in self.assertions.iter().enumerate() {
            match assertion.formula.evaluate(&value_of) {
                Some(true) => {}
                Some(false) => {
                    return EvaluationOutcome::Falsified {
                        index,
                        group: assertion.group,
                        explanation: assertion.explanation.clone(),
                    };
                }
                None => {
                    return EvaluationOutcome::Incomplete {
                        index,
                        group: assertion.group,
                    };
                }
            }
        }
        EvaluationOutcome::Satisfied
    }

    /// Evaluate a single term under `assignment`.
    #[must_use]
    pub fn evaluate_term(&self, term: &Term, assignment: &Assignment) -> Option<i128> {
        evaluate_term(term, &|id| assignment.get(id))
    }

    /// This system rendered as an SMT-LIB2 script.
    ///
    /// A pure function of the system's structure, so identical encodings
    /// render to identical text — which is what
    /// [`crate::VerificationReport`]'s determinism guarantee is checked
    /// against, with and without a solver in the build.
    #[must_use]
    pub fn to_smtlib(&self) -> String {
        let mut out = String::new();
        out.push_str("(set-logic QF_LIA)\n");
        for (_, decl) in self.registry.iter() {
            out.push_str("(declare-const ");
            out.push_str(&decl.name);
            out.push(' ');
            out.push_str(decl.sort.smtlib_name());
            out.push_str(")\n");
        }
        let mut current_group = "";
        for assertion in &self.assertions {
            if assertion.group != current_group {
                current_group = assertion.group;
                out.push_str("; --- ");
                out.push_str(current_group);
                out.push_str(" ---\n");
            }
            out.push_str("; ");
            out.push_str(&assertion.explanation);
            out.push('\n');
            out.push_str("(assert ");
            assertion.formula.render(&self.registry, &mut out);
            out.push_str(")\n");
        }
        out.push_str("(check-sat)\n");
        out
    }
}

impl fmt::Display for ConstraintSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_smtlib())
    }
}

/// What [`ConstraintSystem::evaluate`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvaluationOutcome {
    /// Every assertion evaluated to `true`.
    Satisfied,
    /// One assertion evaluated to `false` under the assignment.
    Falsified {
        /// Its index in [`ConstraintSystem::assertions`].
        index: usize,
        /// Its group.
        group: &'static str,
        /// Its explanation.
        explanation: String,
    },
    /// One assertion mentioned a variable the assignment did not cover.
    Incomplete {
        /// Its index in [`ConstraintSystem::assertions`].
        index: usize,
        /// Its group.
        group: &'static str,
    },
}

impl EvaluationOutcome {
    /// Whether the assignment satisfied the whole system.
    #[must_use]
    pub fn is_satisfied(&self) -> bool {
        matches!(self, Self::Satisfied)
    }
}

impl fmt::Display for EvaluationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Satisfied => f.write_str("all assertions hold"),
            Self::Falsified {
                index,
                group,
                explanation,
            } => write!(f, "assertion #{index} ({group}) is false: {explanation}"),
            Self::Incomplete { index, group } => {
                write!(
                    f,
                    "assertion #{index} ({group}) mentions an unassigned variable"
                )
            }
        }
    }
}

/// A (possibly partial) assignment of integers to variables.
///
/// Booleans are carried as `0`/`1`. Ordered by [`VarId`] so iteration —
/// and therefore every counterexample rendered from one — is stable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assignment {
    values: BTreeMap<VarId, i128>,
}

impl Assignment {
    /// An empty assignment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a variable's value.
    pub fn set(&mut self, id: VarId, value: i128) {
        self.values.insert(id, value);
    }

    /// Record a boolean variable's value.
    pub fn set_bool(&mut self, id: VarId, value: bool) {
        self.values.insert(id, i128::from(value));
    }

    /// One variable's value, if assigned.
    #[must_use]
    pub fn get(&self, id: VarId) -> Option<i128> {
        self.values.get(&id).copied()
    }

    /// One variable's value read as a boolean, if assigned.
    #[must_use]
    pub fn get_bool(&self, id: VarId) -> Option<bool> {
        self.get(id).map(|value| value != 0)
    }

    /// How many variables are assigned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether nothing is assigned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every assignment, in ascending [`VarId`] order.
    pub fn iter(&self) -> impl Iterator<Item = (VarId, i128)> + '_ {
        self.values.iter().map(|(id, value)| (*id, *value))
    }

    /// Fill every unassigned variable of `registry` with `default`.
    ///
    /// Solvers are free to leave a variable out of a model when its value
    /// is irrelevant to satisfaction. Completing the assignment before
    /// replay keeps [`ConstraintSystem::evaluate`] from reporting
    /// [`EvaluationOutcome::Incomplete`] for what is really a
    /// don't-care — and reports the completion in the counterexample so a
    /// reader can tell an inferred value from a solved one.
    pub fn complete_with(&mut self, registry: &VarRegistry, default: i128) -> Vec<VarId> {
        use std::collections::btree_map::Entry;
        let mut filled = Vec::new();
        for (id, _) in registry.iter() {
            if let Entry::Vacant(slot) = self.values.entry(id) {
                slot.insert(default);
                filled.push(id);
            }
        }
        filled
    }

    /// This assignment rendered against a registry, one `name = value`
    /// pair per entry, in [`VarId`] order.
    #[must_use]
    pub fn render(&self, registry: &VarRegistry) -> Vec<String> {
        self.values
            .iter()
            .map(|(id, value)| {
                let decl: Option<&VarDecl> = registry.get(*id);
                let name = decl.map_or_else(|| id.to_string(), |d| d.name.clone());
                match decl.map(|d| d.sort) {
                    Some(Sort::Bool) => format!("{name} = {}", *value != 0),
                    _ => format!("{name} = {value}"),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sample() -> (ConstraintSystem, VarId, VarId) {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "the x count");
        let p = system.bool_var("p", "the p flag");
        system.assert("flow", "x is at least 3", Formula::ge(Term::var(x), 3i128));
        system.assert("flow", "p holds", Formula::bool_var(p));
        (system, x, p)
    }

    #[test]
    fn trivially_true_assertions_are_dropped() {
        let mut system = ConstraintSystem::new();
        system.assert("g", "nothing", Formula::True);
        assert!(system.is_trivial());
        assert_eq!(system.assertion_count(), 0);
    }

    #[test]
    fn trivially_false_assertions_are_detected() {
        let mut system = ConstraintSystem::new();
        system.assert("g", "impossible", Formula::False);
        assert!(system.is_trivially_unsat());
    }

    #[test]
    fn declarations_are_idempotent() {
        let mut system = ConstraintSystem::new();
        let a = system.int_var("x", "x");
        let b = system.int_var("x", "x");
        assert_eq!(a, b);
        assert_eq!(system.var_count(), 1);
    }

    #[test]
    fn smtlib_output_is_stable_and_complete() {
        let (system, _, _) = sample();
        let text = system.to_smtlib();
        assert_eq!(text, system.to_smtlib());
        assert!(text.starts_with("(set-logic QF_LIA)\n"), "{text}");
        assert!(text.contains("(declare-const x Int)"), "{text}");
        assert!(text.contains("(declare-const p Bool)"), "{text}");
        assert!(text.contains("(assert (>= x 3))"), "{text}");
        assert!(text.contains("; --- flow ---"), "{text}");
        assert!(text.ends_with("(check-sat)\n"), "{text}");
    }

    #[test]
    fn evaluation_accepts_a_satisfying_assignment() {
        let (system, x, p) = sample();
        let mut assignment = Assignment::new();
        assignment.set(x, 4);
        assignment.set_bool(p, true);
        assert_eq!(system.evaluate(&assignment), EvaluationOutcome::Satisfied);
        assert!(system.evaluate(&assignment).is_satisfied());
    }

    #[test]
    fn evaluation_names_the_falsified_assertion() {
        let (system, x, p) = sample();
        let mut assignment = Assignment::new();
        assignment.set(x, 1);
        assignment.set_bool(p, true);
        match system.evaluate(&assignment) {
            EvaluationOutcome::Falsified { index, group, .. } => {
                assert_eq!(index, 0);
                assert_eq!(group, "flow");
            }
            other => panic!("expected falsification, got {other:?}"),
        }
    }

    #[test]
    fn evaluation_reports_incompleteness() {
        let (system, x, _) = sample();
        let mut assignment = Assignment::new();
        assignment.set(x, 5);
        assert!(matches!(
            system.evaluate(&assignment),
            EvaluationOutcome::Incomplete { index: 1, .. }
        ));
    }

    #[test]
    fn completion_fills_only_the_gaps() {
        let (system, x, p) = sample();
        let mut assignment = Assignment::new();
        assignment.set(x, 5);
        let filled = assignment.complete_with(system.registry(), 0);
        assert_eq!(filled, vec![p]);
        assert_eq!(assignment.get(x), Some(5));
        assert_eq!(assignment.get_bool(p), Some(false));
    }

    #[test]
    fn assignment_renders_booleans_as_booleans() {
        let (system, x, p) = sample();
        let mut assignment = Assignment::new();
        assignment.set(x, 2);
        assignment.set_bool(p, true);
        let rendered = assignment.render(system.registry());
        assert_eq!(rendered, vec!["x = 2".to_string(), "p = true".to_string()]);
    }

    #[test]
    fn assignment_iteration_is_var_ordered() {
        let mut assignment = Assignment::new();
        assignment.set(VarId::from_index(5), 1);
        assignment.set(VarId::from_index(0), 2);
        let order: Vec<usize> = assignment.iter().map(|(id, _)| id.index()).collect();
        assert_eq!(order, vec![0, 5]);
        assert_eq!(assignment.len(), 2);
        assert!(!assignment.is_empty());
    }

    #[test]
    fn term_evaluation_goes_through_the_system() {
        let (system, x, _) = sample();
        let mut assignment = Assignment::new();
        assignment.set(x, 6);
        let term = Term::var(x).scaled(2).plus(Term::constant(1));
        assert_eq!(system.evaluate_term(&term, &assignment), Some(13));
    }

    #[test]
    fn evaluation_outcomes_render() {
        assert_eq!(
            EvaluationOutcome::Satisfied.to_string(),
            "all assertions hold"
        );
        let falsified = EvaluationOutcome::Falsified {
            index: 2,
            group: "capacity",
            explanation: "node keeps up".to_string(),
        };
        assert!(falsified.to_string().contains("capacity"));
        let incomplete = EvaluationOutcome::Incomplete {
            index: 1,
            group: "flow",
        };
        assert!(incomplete.to_string().contains("unassigned"));
    }

    #[test]
    fn display_matches_smtlib() {
        let (system, _, _) = sample();
        assert_eq!(system.to_string(), system.to_smtlib());
    }
}
