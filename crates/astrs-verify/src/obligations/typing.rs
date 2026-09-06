//! Type-rule consistency (blueprint §15(d)).
//!
//! # The question
//!
//! A manifest may declare `type_rules: [{from, to}]` — implicit coercions
//! that let a typed output feed a differently-typed input (blueprint §8.2).
//! Rules compose, so the real question about an edge is not "do the two
//! URNs match" but "is the consumer's type **reachable** from the
//! producer's under the transitive closure of the rules".
//!
//! # Encoding unreachability, not reachability
//!
//! Reachability is a least fixed point, and a least fixed point is the
//! wrong shape for a satisfiability question: the constraints `u[x] ⇒
//! u[y]` are satisfied by setting everything false, so asking a solver
//! "can you reach the target" would be answered trivially and wrongly.
//!
//! Its complement is exactly right. A type `to` is **un**reachable from
//! `from` precisely when some *separating set* `U` exists with
//!
//! - `from ∈ U`,
//! - `to ∉ U`, and
//! - `U` closed under the rules: `x ∈ U ∧ (x → y) ⇒ y ∈ U`.
//!
//! Such a `U` is an inductive invariant: every type the rules can walk to
//! from `from` stays inside it, and `to` is outside, so no chain of
//! coercions gets there. Finding one is a satisfiability question with the
//! same polarity as every other obligation in this crate — **`sat` means
//! violated** — and the set itself is the counterexample: it is the answer
//! to "why can't these connect", not merely the assertion that they can't.
//!
//! The encoding is linear in the number of rules, with one boolean per
//! distinct URN. No closure matrix, no path enumeration.
//!
//! # Which edges are checked
//!
//! Only edges with a declared type on **both** ends. An absent
//! `input_types`/`output_types` entry is an explicit opt-out — blueprint
//! §3.7's "raw/untyped ports remain available" — not a missing fact to
//! infer. Whether a mismatch is fatal is the manifest's `strict_types`
//! decision, reported alongside the counterexample; the obligation itself
//! is the same either way.

use std::collections::{BTreeMap, BTreeSet};

use astrs_manifest::Urn;

use crate::constraint::{ConstraintSystem, Formula, VarId};
use crate::counterexample::{Counterexample, Strength};
use crate::model::Model;
use crate::obligation::{Obligation, ObligationKind, Subject};
use crate::report::{Discharge, NotAttempted, ObligationOutcome};
use crate::smt::{SolveOutcome, SolverBudget, solve};

/// Discharge type-rule consistency, one obligation per fully typed edge.
#[must_use]
pub fn discharge(model: &Model, budget: &SolverBudget) -> Vec<ObligationOutcome> {
    let mut outcomes = Vec::new();
    for key in model.channels().keys() {
        let Some(types) = model.edge_types(key) else {
            continue;
        };
        let Some((produced, consumed)) = types.both() else {
            continue;
        };
        let obligation = Obligation::about(
            ObligationKind::TypeConsistency,
            Subject::Channel(key.to_string()),
        );

        let (system, universe) = encode(model, produced, consumed);
        let smtlib = system.to_smtlib();

        let outcome = match solve(&system, budget) {
            SolveOutcome::Unsat => ObligationOutcome::decided(
                obligation,
                Discharge::Holds {
                    claim: if produced == consumed {
                        format!("`{key}` carries `{}` on both ends", produced.as_str())
                    } else {
                        format!(
                            "`{key}` coerces `{}` to `{}` through the manifest's type rules",
                            produced.as_str(),
                            consumed.as_str()
                        )
                    },
                },
                smtlib,
            ),
            SolveOutcome::Sat(assignment) => {
                let separator: Vec<&str> = universe
                    .iter()
                    .filter(|(_, id)| assignment.get_bool(**id) == Some(true))
                    .map(|(urn, _)| urn.as_str())
                    .collect();
                let counterexample = Counterexample::from_assignment(
                    format!(
                        "`{key}` connects `{}` to `{}`, which no chain of type rules reaches",
                        produced.as_str(),
                        consumed.as_str()
                    ),
                    Strength::Proof,
                    &system,
                    &assignment,
                )
                .with_fact("produced type", produced.as_str().to_string())
                .with_fact("consumed type", consumed.as_str().to_string())
                .with_fact(
                    "declared type rules",
                    if model.type_rules().is_empty() {
                        "none".to_string()
                    } else {
                        model
                            .type_rules()
                            .iter()
                            .map(|rule| format!("{} -> {}", rule.from.as_str(), rule.to.as_str()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                )
                .with_fact(
                    "severity",
                    if model.strict_types() {
                        "error (`strict_types: true`)".to_string()
                    } else {
                        "warning (`strict_types` is off)".to_string()
                    },
                )
                .with_step(
                    key.to_string(),
                    format!(
                        "everything reachable from `{}` stays inside {{{}}}, and `{}` is outside it",
                        produced.as_str(),
                        separator.join(", "),
                        consumed.as_str()
                    ),
                )
                .with_remedy(format!(
                    "declare a `type_rules` entry from `{}` to `{}`, correct one of the two declarations, or drop the type from the port",
                    produced.as_str(),
                    consumed.as_str()
                ));
                ObligationOutcome::decided(
                    obligation,
                    Discharge::Violated {
                        counterexample: Box::new(counterexample),
                    },
                    smtlib,
                )
            }
            SolveOutcome::Inconclusive(reason) => {
                ObligationOutcome::decided(obligation, Discharge::Inconclusive { reason }, smtlib)
            }
            SolveOutcome::Unavailable => ObligationOutcome::encoded_but_unsolved(
                obligation,
                NotAttempted::SolverUnavailable,
                smtlib,
            ),
        };
        outcomes.push(outcome);
    }

    if outcomes.is_empty() {
        outcomes.push(ObligationOutcome::unattempted(
            Obligation::whole_graph(ObligationKind::TypeConsistency),
            NotAttempted::NothingToProve,
        ));
    }
    outcomes
}

/// Encode "no chain of type rules takes `produced` to `consumed`".
fn encode(
    model: &Model,
    produced: &Urn,
    consumed: &Urn,
) -> (ConstraintSystem, BTreeMap<Urn, VarId>) {
    let mut universe: BTreeSet<Urn> = BTreeSet::new();
    universe.insert(produced.clone());
    universe.insert(consumed.clone());
    for rule in model.type_rules() {
        universe.insert(rule.from.clone());
        universe.insert(rule.to.clone());
    }

    let mut system = ConstraintSystem::new();
    let mut vars = BTreeMap::new();
    for urn in &universe {
        let id = system.bool_var(
            format!("reach!{}", urn.as_str()),
            format!("`{}` is inside the separating set", urn.as_str()),
        );
        vars.insert(urn.clone(), id);
    }

    for rule in model.type_rules() {
        let (Some(&from), Some(&to)) = (vars.get(&rule.from), vars.get(&rule.to)) else {
            continue;
        };
        system.assert(
            "closure",
            format!(
                "the separating set is closed under `{} -> {}`",
                rule.from.as_str(),
                rule.to.as_str()
            ),
            Formula::bool_var(from).implies(Formula::bool_var(to)),
        );
    }

    if let Some(&start) = vars.get(produced) {
        system.assert(
            "violation",
            format!(
                "the produced type `{}` is inside the set",
                produced.as_str()
            ),
            Formula::bool_var(start),
        );
    }
    if let Some(&target) = vars.get(consumed) {
        system.assert(
            "violation",
            format!(
                "the consumed type `{}` is outside it, so no chain of rules reaches it",
                consumed.as_str()
            ),
            Formula::bool_var(target).not(),
        );
    }

    (system, vars)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::test_support::model_of;

    fn typed(produced: &str, consumed: &str, rules: &str) -> String {
        format!(
            "
strict_types: true
{rules}nodes:
  - id: camera
    path: ./camera
    inputs: {{ tick: astrs/timer/hz/1 }}
    outputs: [frames]
    output_types: {{ frames: \"{produced}\" }}
  - id: detector
    path: ./detector
    inputs: {{ frames: camera/frames }}
    input_types: {{ frames: \"{consumed}\" }}
"
        )
    }

    #[test]
    fn matching_types_encode_a_trivially_unsatisfiable_violation() {
        let model = model_of(&typed("std/media/v1/Image", "std/media/v1/Image", ""));
        let outcomes = discharge(&model, &SolverBudget::default());
        let edge = outcomes
            .iter()
            .find(|o| o.obligation.subject == Subject::Channel("detector.frames".to_string()))
            .expect("edge obligation");
        let text = edge.encoded_system.as_deref().expect("a system");
        assert!(text.contains("(assert reach!std/media/v1/Image)"), "{text}");
        assert!(
            text.contains("(assert (not reach!std/media/v1/Image))"),
            "{text}"
        );
    }

    #[test]
    fn a_declared_rule_closes_the_gap() {
        let rules =
            "type_rules:\n  - { from: \"std/media/v1/Image\", to: \"std/media/v2/Image\" }\n";
        let model = model_of(&typed("std/media/v1/Image", "std/media/v2/Image", rules));
        let outcomes = discharge(&model, &SolverBudget::default());
        let edge = outcomes
            .iter()
            .find(|o| o.obligation.subject == Subject::Channel("detector.frames".to_string()))
            .expect("edge obligation");
        let text = edge.encoded_system.as_deref().expect("a system");
        assert!(
            text.contains("(=> reach!std/media/v1/Image reach!std/media/v2/Image)"),
            "{text}"
        );
    }

    #[test]
    fn rules_compose_transitively() {
        let rules = "type_rules:\n  - { from: \"a/b/v1/A\", to: \"a/b/v1/B\" }\n  - { from: \"a/b/v1/B\", to: \"a/b/v1/C\" }\n";
        let model = model_of(&typed("a/b/v1/A", "a/b/v1/C", rules));
        let outcomes = discharge(&model, &SolverBudget::default());
        let edge = outcomes
            .iter()
            .find(|o| o.obligation.subject == Subject::Channel("detector.frames".to_string()))
            .expect("edge obligation");
        let text = edge.encoded_system.as_deref().expect("a system");
        assert!(
            text.contains("(=> reach!a/b/v1/A reach!a/b/v1/B)"),
            "{text}"
        );
        assert!(
            text.contains("(=> reach!a/b/v1/B reach!a/b/v1/C)"),
            "{text}"
        );
    }

    #[test]
    fn an_unbridged_mismatch_leaves_a_separating_set() {
        let model = model_of(&typed("a/b/v1/A", "a/b/v1/Z", ""));
        let (system, vars) = encode(&model, &Urn::new("a/b/v1/A"), &Urn::new("a/b/v1/Z"));
        assert_eq!(vars.len(), 2);
        // Setting only the produced type true satisfies the system: no
        // rule forces `Z` in, so `A`'s reachable set excludes it.
        let mut assignment = crate::constraint::Assignment::new();
        assignment.complete_with(system.registry(), 0);
        let from = system.registry().id_of("reach!a/b/v1/A").expect("declared");
        assignment.set_bool(from, true);
        assert!(system.evaluate(&assignment).is_satisfied());
    }

    #[test]
    fn untyped_edges_are_not_checked() {
        let model = model_of(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/1 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
",
        );
        let outcomes = discharge(&model, &SolverBudget::default());
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            outcomes[0].discharge,
            Discharge::NotAttempted {
                reason: NotAttempted::NothingToProve
            }
        ));
    }

    #[test]
    fn one_sided_types_are_not_checked() {
        let model = model_of(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/1 }
    outputs: [frames]
    output_types: { frames: \"std/media/v1/Image\" }
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
",
        );
        let outcomes = discharge(&model, &SolverBudget::default());
        assert!(matches!(
            outcomes[0].discharge,
            Discharge::NotAttempted {
                reason: NotAttempted::NothingToProve
            }
        ));
    }

    #[test]
    fn encoding_is_deterministic() {
        let yaml = typed("a/b/v1/A", "a/b/v1/Z", "");
        let first = discharge(&model_of(&yaml), &SolverBudget::default());
        let second = discharge(&model_of(&yaml), &SolverBudget::default());
        assert_eq!(first, second);
    }
}
