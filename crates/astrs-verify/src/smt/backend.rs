//! The OxiZ backend: translating a [`ConstraintSystem`] into SMT terms and
//! reading a model back out.
//!
//! Only compiled with the `verify` feature. Everything here is a total
//! function of the constraint system — no configuration, no state carried
//! between calls — which is what makes two runs of the same obligation
//! produce the same answer and the same counterexample text.

use oxiz::core::ast::TermKind;
use oxiz::resource_limits::{ResourceExhausted, ResourceLimits};
use oxiz::{Solver, SolverResult, TermId, TermManager};

use super::{SolveOutcome, SolverBudget};
use crate::constraint::{
    Assignment, Compare, ConstraintSystem, Formula, Sort, Term, VarId, VarRegistry,
};
use crate::report::Inconclusive;

/// Solve one system plus extra assertions.
pub(super) fn solve(
    system: &ConstraintSystem,
    extra: &[Formula],
    budget: &SolverBudget,
) -> SolveOutcome {
    let mut manager = TermManager::new();
    let mut solver = Solver::new();
    solver.set_logic("QF_LIA");

    let terms = declare_variables(system.registry(), &mut manager, &mut solver);

    for assertion in system.assertions() {
        let term = encode_formula(&assertion.formula, &terms, &mut manager);
        solver.assert(term, &mut manager);
    }
    for formula in extra {
        let term = encode_formula(formula, &terms, &mut manager);
        solver.assert(term, &mut manager);
    }

    let limits = resource_limits(budget);
    let result = match solver.check_with_limits(&mut manager, &limits) {
        Ok(result) => result,
        Err(exhausted) => {
            return SolveOutcome::Inconclusive(Inconclusive::BudgetExhausted {
                limit: describe_exhaustion(exhausted).to_string(),
            });
        }
    };

    match result {
        SolverResult::Unsat => SolveOutcome::Unsat,
        SolverResult::Unknown => SolveOutcome::Inconclusive(Inconclusive::SolverUnknown),
        SolverResult::Sat => match read_model(&solver, &mut manager, system.registry(), &terms) {
            Some(assignment) => SolveOutcome::Sat(Box::new(assignment)),
            None => SolveOutcome::Inconclusive(Inconclusive::ModelRejected {
                detail: "the solver reported `sat` without a model".to_string(),
            }),
        },
    }
}

/// Declare every variable, in registry order, keeping a parallel term
/// table indexed by [`VarId`].
fn declare_variables(
    registry: &VarRegistry,
    manager: &mut TermManager,
    solver: &mut Solver,
) -> Vec<TermId> {
    let mut terms = Vec::with_capacity(registry.len());
    for (_, decl) in registry.iter() {
        let sort = match decl.sort {
            Sort::Int => manager.sorts.int_sort,
            Sort::Bool => manager.sorts.bool_sort,
        };
        let term = manager.mk_var(&decl.name, sort);
        // Declaring the constant with the solver keeps unconstrained
        // variables present in the model, so a counterexample shows every
        // quantity it names rather than only the ones the search happened
        // to touch.
        solver.register_declared_const(term, sort);
        terms.push(term);
    }
    terms
}

/// Translate one linear term.
fn encode_term(term: &Term, terms: &[TermId], manager: &mut TermManager) -> TermId {
    let mut parts: Vec<TermId> = Vec::with_capacity(term.linear_part().len() + 1);
    for (id, coefficient) in term.linear_part() {
        let Some(&var) = terms.get(id.index()) else {
            // A term referencing a variable outside its own system's
            // registry cannot arise from the encoders (they allocate
            // through the system they assert into). Encoding it as a
            // literal zero keeps this function total; the constraint
            // system's own replay would reject any model built from it.
            continue;
        };
        if *coefficient == 1 {
            parts.push(var);
        } else {
            let factor = mk_int(*coefficient, manager);
            parts.push(manager.mk_mul([factor, var]));
        }
    }
    if term.constant_part() != 0 || parts.is_empty() {
        parts.push(mk_int(term.constant_part(), manager));
    }
    if parts.len() == 1 {
        parts[0]
    } else {
        manager.mk_add(parts)
    }
}

/// Build an integer literal, going through `i64` where possible and a
/// decimal string otherwise so no magnitude is lost.
fn mk_int(value: i128, manager: &mut TermManager) -> TermId {
    match i64::try_from(value) {
        Ok(small) => manager.mk_int(small),
        Err(_) => {
            // Beyond `i64` the scales in this crate never go (the analysis
            // window is capped at a day of nanoseconds), but a lossy
            // truncation here would be a silently wrong proof, so build
            // the value by parts instead: value = high · 2^64 + low.
            let high = value >> 64;
            let low = (value & i128::from(u64::MAX)) as u64;
            let shift = manager.mk_int(1i64 << 62);
            let four = manager.mk_int(4i64);
            let two_to_64 = manager.mk_mul([shift, four]);
            let high_term = mk_int(high, manager);
            let scaled = manager.mk_mul([high_term, two_to_64]);
            let low_term = match i64::try_from(low) {
                Ok(small) => manager.mk_int(small),
                Err(_) => {
                    let half = manager.mk_int((low / 2) as i64);
                    let two = manager.mk_int(2i64);
                    let doubled = manager.mk_mul([half, two]);
                    let remainder = manager.mk_int((low % 2) as i64);
                    manager.mk_add([doubled, remainder])
                }
            };
            manager.mk_add([scaled, low_term])
        }
    }
}

/// Translate one formula.
fn encode_formula(formula: &Formula, terms: &[TermId], manager: &mut TermManager) -> TermId {
    match formula {
        Formula::True => manager.mk_bool(true),
        Formula::False => manager.mk_bool(false),
        Formula::Bool(id) => match terms.get(id.index()) {
            Some(&term) => term,
            None => manager.mk_bool(false),
        },
        Formula::Not(inner) => {
            let inner = encode_formula(inner, terms, manager);
            manager.mk_not(inner)
        }
        Formula::And(parts) => {
            let encoded: Vec<TermId> = parts
                .iter()
                .map(|part| encode_formula(part, terms, manager))
                .collect();
            manager.mk_and(encoded)
        }
        Formula::Or(parts) => {
            let encoded: Vec<TermId> = parts
                .iter()
                .map(|part| encode_formula(part, terms, manager))
                .collect();
            manager.mk_or(encoded)
        }
        Formula::Implies(antecedent, consequent) => {
            let antecedent = encode_formula(antecedent, terms, manager);
            let consequent = encode_formula(consequent, terms, manager);
            manager.mk_implies(antecedent, consequent)
        }
        Formula::Compare { op, left, right } => {
            let left = encode_term(left, terms, manager);
            let right = encode_term(right, terms, manager);
            match op {
                Compare::Eq => manager.mk_eq(left, right),
                Compare::Ne => {
                    let eq = manager.mk_eq(left, right);
                    manager.mk_not(eq)
                }
                Compare::Lt => manager.mk_lt(left, right),
                Compare::Le => manager.mk_le(left, right),
                Compare::Gt => manager.mk_gt(left, right),
                Compare::Ge => manager.mk_ge(left, right),
            }
        }
    }
}

/// Read the model back, walking the registry in allocation order.
///
/// A variable the solver left unconstrained is simply absent from the
/// assignment; [`crate::Counterexample::from_assignment`] completes it and
/// records that it did, so an inferred value is never mistaken for a
/// solved one.
fn read_model(
    solver: &Solver,
    manager: &mut TermManager,
    registry: &VarRegistry,
    terms: &[TermId],
) -> Option<Assignment> {
    let model = solver.model()?;
    let mut assignment = Assignment::new();
    for (id, _) in registry.iter() {
        let Some(&term) = terms.get(id.index()) else {
            continue;
        };
        let value = model
            .get(term)
            .and_then(|assigned| constant_value(assigned, manager));
        let value = match value {
            Some(value) => Some(value),
            None => constant_value(model.eval(term, manager), manager),
        };
        if let Some(value) = value {
            assignment.set(id, value);
        } else {
            let _ = VarId::from_index(id.index());
        }
    }
    Some(assignment)
}

/// Read an integer or boolean constant out of a term.
fn constant_value(term: TermId, manager: &TermManager) -> Option<i128> {
    match manager.get(term).map(|node| &node.kind) {
        Some(TermKind::True) => Some(1),
        Some(TermKind::False) => Some(0),
        // `BigInt` is not nameable from here (`num-bigint` is not on the
        // blueprint's §18.1 closed list, and OxiZ does not re-export it),
        // so the value comes back through its decimal rendering. Exact for
        // every magnitude, and it fails loudly — `None`, never a silent
        // truncation — for anything outside `i128`.
        Some(TermKind::IntConst(value)) => value.to_string().parse::<i128>().ok(),
        Some(TermKind::Neg(inner)) => constant_value(*inner, manager).and_then(i128::checked_neg),
        _ => None,
    }
}

/// Translate the crate's budget into the solver's.
fn resource_limits(budget: &SolverBudget) -> ResourceLimits {
    let mut limits = ResourceLimits::new();
    limits.max_conflicts = budget.max_conflicts;
    limits.max_decisions = budget.max_decisions;
    limits.timeout = budget.wall_clock;
    limits
}

/// Name the budget a run exhausted.
fn describe_exhaustion(reason: ResourceExhausted) -> &'static str {
    match reason {
        ResourceExhausted::Timeout => "wall-clock",
        ResourceExhausted::MaxConflicts => "max-conflicts",
        ResourceExhausted::MaxMemory => "max-memory",
        ResourceExhausted::MaxDecisions => "max-decisions",
        ResourceExhausted::MaxRestarts => "max-restarts",
        ResourceExhausted::MaxTheoryChecks => "max-theory-checks",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::smt::{solve as solve_public, solve_with_extra};

    fn budget() -> SolverBudget {
        SolverBudget::default()
    }

    #[test]
    fn linear_integer_systems_round_trip() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        let y = system.int_var("y", "y");
        system.assert(
            "g",
            "x + y = 10",
            Formula::eq(Term::var(x).plus(Term::var(y)), 10i128),
        );
        system.assert("g", "x > 5", Formula::gt(Term::var(x), 5i128));
        let outcome = solve_public(&system, &budget());
        let assignment = outcome.assignment().expect("sat");
        let vx = assignment.get(x).expect("x assigned");
        let vy = assignment.get(y).expect("y assigned");
        assert_eq!(vx + vy, 10);
        assert!(vx > 5);
        assert!(system.evaluate(assignment).is_satisfied());
    }

    #[test]
    fn contradictions_come_back_unsat() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert("g", "x > 2", Formula::gt(Term::var(x), 2i128));
        system.assert("g", "x < 1", Formula::lt(Term::var(x), 1i128));
        assert!(solve_public(&system, &budget()).is_unsat());
    }

    #[test]
    fn coefficients_are_encoded_faithfully() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert(
            "g",
            "3x + 2 = 17",
            Formula::eq(Term::var(x).scaled(3).plus(Term::constant(2)), 17i128),
        );
        let outcome = solve_public(&system, &budget());
        assert_eq!(outcome.assignment().and_then(|a| a.get(x)), Some(5));
    }

    #[test]
    fn negative_constants_survive_encoding() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert("g", "x = -7", Formula::eq(Term::var(x), -7i128));
        let outcome = solve_public(&system, &budget());
        assert_eq!(outcome.assignment().and_then(|a| a.get(x)), Some(-7));
    }

    #[test]
    fn boolean_structure_is_encoded() {
        let mut system = ConstraintSystem::new();
        let p = system.bool_var("p", "p");
        let q = system.bool_var("q", "q");
        system.assert(
            "g",
            "p implies q",
            Formula::bool_var(p).implies(Formula::bool_var(q)),
        );
        system.assert("g", "p holds", Formula::bool_var(p));
        let outcome = solve_public(&system, &budget());
        let assignment = outcome.assignment().expect("sat");
        assert_eq!(assignment.get_bool(p), Some(true));
        assert_eq!(assignment.get_bool(q), Some(true));
    }

    #[test]
    fn mutual_implication_forces_a_cycle_to_be_witnessed() {
        // The shape the deadlock encoding relies on: a set closed under
        // "is justified by another member" is satisfiable exactly when a
        // cycle exists.
        let mut system = ConstraintSystem::new();
        let a = system.bool_var("dead!a", "a is blocked");
        let b = system.bool_var("dead!b", "b is blocked");
        system.assert(
            "wait",
            "a waits on b",
            Formula::bool_var(a).implies(Formula::bool_var(b)),
        );
        system.assert(
            "wait",
            "b waits on a",
            Formula::bool_var(b).implies(Formula::bool_var(a)),
        );
        system.assert(
            "violation",
            "someone is blocked",
            Formula::any([Formula::bool_var(a), Formula::bool_var(b)]),
        );
        let outcome = solve_public(&system, &budget());
        let assignment = outcome.assignment().expect("sat");
        assert_eq!(assignment.get_bool(a), Some(true));
        assert_eq!(assignment.get_bool(b), Some(true));
    }

    #[test]
    fn an_acyclic_wait_chain_is_unsat() {
        let mut system = ConstraintSystem::new();
        let a = system.bool_var("dead!a", "a is blocked");
        let b = system.bool_var("dead!b", "b is blocked");
        system.assert(
            "wait",
            "a waits on b",
            Formula::bool_var(a).implies(Formula::bool_var(b)),
        );
        system.assert("wait", "b waits on nothing", Formula::bool_var(b).not());
        system.assert(
            "violation",
            "someone is blocked",
            Formula::any([Formula::bool_var(a), Formula::bool_var(b)]),
        );
        assert!(solve_public(&system, &budget()).is_unsat());
    }

    #[test]
    fn extra_assertions_narrow_a_solution() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert("g", "x in 1..3", Formula::ge(Term::var(x), 1i128));
        system.assert("g", "x <= 3", Formula::le(Term::var(x), 3i128));
        let narrowed = solve_with_extra(&system, &[Formula::eq(Term::var(x), 2i128)], &budget());
        assert_eq!(narrowed.assignment().and_then(|a| a.get(x)), Some(2));
        let impossible = solve_with_extra(&system, &[Formula::eq(Term::var(x), 9i128)], &budget());
        assert!(impossible.is_unsat());
    }

    #[test]
    fn distinct_is_encoded_as_a_negated_equality() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert(
            "g",
            "x is not 4",
            Formula::compare(Compare::Ne, Term::var(x), 4i128),
        );
        system.assert("g", "x in 4..4", Formula::ge(Term::var(x), 4i128));
        system.assert("g", "x <= 4", Formula::le(Term::var(x), 4i128));
        assert!(solve_public(&system, &budget()).is_unsat());
    }

    #[test]
    fn large_magnitudes_encode_exactly() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        let big = 86_400i128 * 1_000_000_000;
        system.assert(
            "g",
            "x is a day in nanoseconds",
            Formula::eq(Term::var(x), big),
        );
        let outcome = solve_public(&system, &budget());
        assert_eq!(outcome.assignment().and_then(|a| a.get(x)), Some(big));
    }

    #[test]
    fn solving_is_reproducible() {
        let build = || {
            let mut system = ConstraintSystem::new();
            let x = system.int_var("x", "x");
            let y = system.int_var("y", "y");
            system.assert(
                "g",
                "2x + 3y = 30",
                Formula::eq(Term::var(x).scaled(2).plus(Term::var(y).scaled(3)), 30i128),
            );
            system.assert("g", "x >= 0", Formula::ge(Term::var(x), 0i128));
            system.assert("g", "y >= 0", Formula::ge(Term::var(y), 0i128));
            system
        };
        let first = solve_public(&build(), &budget());
        let second = solve_public(&build(), &budget());
        assert_eq!(first, second);
    }

    #[test]
    fn exhaustion_reasons_are_all_named() {
        for reason in [
            ResourceExhausted::Timeout,
            ResourceExhausted::MaxConflicts,
            ResourceExhausted::MaxMemory,
            ResourceExhausted::MaxDecisions,
            ResourceExhausted::MaxRestarts,
            ResourceExhausted::MaxTheoryChecks,
        ] {
            assert!(!describe_exhaustion(reason).is_empty());
        }
    }

    #[test]
    fn budget_translation_carries_every_field() {
        let budget = SolverBudget {
            max_conflicts: Some(7),
            max_decisions: Some(9),
            wall_clock: Some(std::time::Duration::from_millis(5)),
        };
        let limits = resource_limits(&budget);
        assert_eq!(limits.max_conflicts, Some(7));
        assert_eq!(limits.max_decisions, Some(9));
        assert_eq!(limits.timeout, Some(std::time::Duration::from_millis(5)));
    }
}
