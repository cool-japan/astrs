//! The solver seam.
//!
//! Everything above this module speaks [`ConstraintSystem`]; everything
//! below speaks OxiZ. Keeping the seam this narrow is what lets the crate
//! compile — and be tested — without the `verify` feature: with the
//! feature off, [`solve`] returns [`SolveOutcome::Unavailable`] and every
//! obligation reports
//! [`NotAttempted::SolverUnavailable`](crate::NotAttempted::SolverUnavailable)
//! rather than a verdict it did not earn.
//!
//! # Determinism
//!
//! Two rules, both enforced here rather than left to convention:
//!
//! - **Budgets are deterministic.** [`SolverBudget`]'s default limits
//!   *search work* (conflicts and decisions), never wall-clock time. A
//!   wall-clock timeout would make the verdict depend on how busy the
//!   machine was, so it is opt-in and documented as breaking
//!   reproducibility.
//! - **Readback is ordered.** The model is read by walking the constraint
//!   system's own variable registry in allocation order and asking for
//!   each variable's value — never by iterating the solver's model map,
//!   which is a hash map and would order counterexample text differently
//!   between runs.

#[cfg(feature = "verify")]
mod backend;

use std::time::Duration;

use crate::constraint::{Assignment, ConstraintSystem, Formula};
use crate::report::Inconclusive;

/// How much search a single obligation may consume.
///
/// The defaults are generous for the graph sizes a manifest describes
/// (tens of nodes) and small enough that a pathological encoding fails
/// loudly instead of hanging a CI job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolverBudget {
    /// Maximum SAT conflicts before giving up.
    pub max_conflicts: Option<u64>,
    /// Maximum SAT decisions before giving up.
    pub max_decisions: Option<u64>,
    /// An optional wall-clock timeout.
    ///
    /// `None` by default, and deliberately: a time-based budget makes the
    /// verdict depend on machine load, so two runs of the same proof
    /// could disagree. Set it only where a bounded run matters more than
    /// a reproducible one.
    pub wall_clock: Option<Duration>,
}

impl Default for SolverBudget {
    fn default() -> Self {
        Self {
            max_conflicts: Some(2_000_000),
            max_decisions: Some(20_000_000),
            wall_clock: None,
        }
    }
}

impl SolverBudget {
    /// A budget with no limits at all.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_conflicts: None,
            max_decisions: None,
            wall_clock: None,
        }
    }

    /// Whether this budget is reproducible — that is, free of wall-clock
    /// limits.
    #[must_use]
    pub fn is_deterministic(&self) -> bool {
        self.wall_clock.is_none()
    }
}

/// What a solver run concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveOutcome {
    /// Satisfiable, with an assignment.
    Sat(Box<Assignment>),
    /// Unsatisfiable.
    Unsat,
    /// Attempted, no verdict.
    Inconclusive(Inconclusive),
    /// No solver in this build.
    Unavailable,
}

impl SolveOutcome {
    /// Whether the run produced a satisfying assignment.
    #[must_use]
    pub fn is_sat(&self) -> bool {
        matches!(self, Self::Sat(_))
    }

    /// Whether the run proved unsatisfiability.
    #[must_use]
    pub fn is_unsat(&self) -> bool {
        matches!(self, Self::Unsat)
    }

    /// The satisfying assignment, if there is one.
    #[must_use]
    pub fn assignment(&self) -> Option<&Assignment> {
        match self {
            Self::Sat(assignment) => Some(assignment),
            _ => None,
        }
    }
}

/// Discharge one constraint system.
///
/// A system with a trivially false assertion is answered directly: it is
/// unsatisfiable by inspection, and routing it through a solver would only
/// add a way to get a different answer.
#[must_use]
pub fn solve(system: &ConstraintSystem, budget: &SolverBudget) -> SolveOutcome {
    solve_with_extra(system, &[], budget)
}

/// Discharge one constraint system with extra assertions layered on top.
///
/// The extras are used for refinement loops — most importantly the
/// minimal-siphon search in [`crate::obligations::deadlock`], which
/// repeatedly re-solves the same system with one more channel excluded.
/// Passing them here rather than mutating the system keeps the reported
/// [`ObligationOutcome::encoded_system`](crate::ObligationOutcome::encoded_system)
/// equal to the obligation as stated.
#[must_use]
pub fn solve_with_extra(
    system: &ConstraintSystem,
    extra: &[Formula],
    budget: &SolverBudget,
) -> SolveOutcome {
    if system.is_trivially_unsat() || extra.contains(&Formula::False) {
        return SolveOutcome::Unsat;
    }
    #[cfg(feature = "verify")]
    {
        backend::solve(system, extra, budget)
    }
    #[cfg(not(feature = "verify"))]
    {
        let _ = (system, extra, budget);
        SolveOutcome::Unavailable
    }
}

/// Whether this build can prove anything at all.
#[must_use]
pub const fn solver_available() -> bool {
    cfg!(feature = "verify")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::constraint::Term;

    #[test]
    fn default_budget_is_deterministic() {
        let budget = SolverBudget::default();
        assert!(budget.is_deterministic());
        assert!(budget.max_conflicts.is_some());
        assert!(budget.max_decisions.is_some());
    }

    #[test]
    fn a_wall_clock_budget_is_not_deterministic() {
        let budget = SolverBudget {
            wall_clock: Some(Duration::from_secs(1)),
            ..SolverBudget::default()
        };
        assert!(!budget.is_deterministic());
    }

    #[test]
    fn unlimited_budget_has_no_limits() {
        let budget = SolverBudget::unlimited();
        assert_eq!(budget.max_conflicts, None);
        assert_eq!(budget.max_decisions, None);
        assert!(budget.is_deterministic());
    }

    #[test]
    fn a_trivially_false_system_is_unsat_without_a_solver() {
        let mut system = ConstraintSystem::new();
        system.assert("g", "impossible", Formula::False);
        assert_eq!(
            solve(&system, &SolverBudget::default()),
            SolveOutcome::Unsat
        );
    }

    #[test]
    fn a_trivially_false_extra_is_unsat_without_a_solver() {
        let system = ConstraintSystem::new();
        assert_eq!(
            solve_with_extra(&system, &[Formula::False], &SolverBudget::default()),
            SolveOutcome::Unsat
        );
    }

    #[test]
    fn outcome_predicates_agree() {
        assert!(SolveOutcome::Unsat.is_unsat());
        assert!(!SolveOutcome::Unsat.is_sat());
        assert!(SolveOutcome::Unsat.assignment().is_none());
        let sat = SolveOutcome::Sat(Box::new(Assignment::new()));
        assert!(sat.is_sat());
        assert!(sat.assignment().is_some());
    }

    #[test]
    #[cfg(not(feature = "verify"))]
    fn without_the_feature_nothing_is_solved() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert("g", "x is 1", Formula::eq(Term::var(x), 1i128));
        assert_eq!(
            solve(&system, &SolverBudget::default()),
            SolveOutcome::Unavailable
        );
        assert!(!solver_available());
    }

    #[test]
    #[cfg(feature = "verify")]
    fn with_the_feature_a_simple_system_is_solved() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert("g", "x is 7", Formula::eq(Term::var(x), 7i128));
        let outcome = solve(&system, &SolverBudget::default());
        let assignment = outcome.assignment().expect("sat");
        assert_eq!(assignment.get(x), Some(7));
        assert!(solver_available());
    }
}
