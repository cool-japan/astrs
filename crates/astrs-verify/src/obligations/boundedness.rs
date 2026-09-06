//! Queue boundedness (blueprint §15(b)).
//!
//! # The statement
//!
//! An AstRS input queue is bounded by construction — `queue_size`, times
//! ten for `backpressure` (§11.2). So "boundedness" here cannot mean
//! "memory stays finite"; it already does. What it means is the property
//! the bound was there to guarantee: **no message is dropped**. A queue
//! whose consumer cannot keep up fills, and then §11.2's policy discards —
//! the oldest, or the newest — and the dataflow silently loses data.
//!
//! Per analysis window, a node fires once per delivered event (blueprint
//! §9.1's merged loop), so the work it is asked to do is
//!
//! ```text
//! Σ over its inputs (arrivals per window) × (service time per firing)
//! ```
//!
//! and the obligation is that this fits inside the window. Every quantity
//! is an exact integer on the scales of [`crate::scale`], so the
//! comparison is exact — no floating-point rate arithmetic anywhere.
//!
//! # Why this one needs a profile
//!
//! Look at which bound proves which direction:
//!
//! - An **upper** bound on service time proves the obligation *holds*:
//!   even at its slowest, the node keeps up.
//! - A **lower** bound proves it *fails*: even at its fastest, it cannot.
//!
//! A manifest declares neither. This crate will not invent one — a default
//! service time would turn every report into a statement about a number
//! nobody chose. So a node with no declared upper bound reports
//! [`NotAttempted::NoServiceTime`] naming the node and the profile key
//! that would settle it, and a
//! [`Profile`](crate::Profile) is how the number arrives. The other three
//! obligations need no profile at all.

use crate::constraint::{ConstraintSystem, Formula, Term};
use crate::counterexample::{Counterexample, Strength};
use crate::model::Model;
use crate::obligation::{Obligation, ObligationKind, Subject};
use crate::report::{Discharge, NotAttempted, ObligationOutcome};
use crate::scale::Nanos;
use crate::smt::{SolveOutcome, SolverBudget, solve};

/// Discharge queue boundedness, one obligation per node with wired inputs.
#[must_use]
pub fn discharge(model: &Model, budget: &SolverBudget) -> Vec<ObligationOutcome> {
    let mut outcomes = Vec::new();
    for (id, node) in model.nodes() {
        if node.inputs.is_empty() {
            continue;
        }
        let obligation = Obligation::about(
            ObligationKind::QueueBoundedness,
            Subject::Node(id.to_string()),
        );

        let Some(rate) = node.derived_rate else {
            let why = model
                .rates()
                .reason(id)
                .unwrap_or(crate::model::Indeterminate::UpstreamIndeterminate);
            outcomes.push(ObligationOutcome::unattempted(
                obligation,
                NotAttempted::NoDerivedRate {
                    node: id.to_string(),
                    why,
                },
            ));
            continue;
        };

        let Some(max_service) = node.service_time.max else {
            outcomes.push(ObligationOutcome::unattempted(
                obligation,
                NotAttempted::NoServiceTime {
                    node: id.to_string(),
                },
            ));
            continue;
        };

        let Some(firings) = rate.events_per_window(model.window()) else {
            outcomes.push(ObligationOutcome::unattempted(
                obligation,
                NotAttempted::ScaleOverflow {
                    detail: format!("`{id}` fires {rate} over a {} window", model.window()),
                },
            ));
            continue;
        };

        let mut system = ConstraintSystem::new();
        let fire = system.int_var(
            format!("fire!{id}"),
            format!("times `{id}` fires per {} window", model.window()),
        );
        let service = system.int_var(
            format!("service!{id}"),
            format!("nanoseconds one firing of `{id}` takes"),
        );

        system.assert(
            "flow",
            format!(
                "`{id}` fires once per delivered event: {firings} per {} window at {rate}",
                model.window()
            ),
            Formula::eq(Term::var(fire), firings),
        );
        if let Some(min) = node.service_time.min {
            system.assert(
                "service",
                format!("`{id}` takes at least {} per firing", min.render()),
                Formula::ge(Term::var(service), min.get()),
            );
        } else {
            system.assert(
                "service",
                format!("`{id}`'s service time is non-negative"),
                Formula::ge(Term::var(service), 0i128),
            );
        }
        system.assert(
            "service",
            format!("`{id}` takes at most {} per firing", max_service.render()),
            Formula::le(Term::var(service), max_service.get()),
        );

        let window_nanos = model.window().nanos();
        system.assert(
            "violation",
            format!(
                "`{id}`'s work over the window exceeds the window itself, so its queues drain slower than they fill"
            ),
            Formula::gt(
                Term::var(service).scaled(firings),
                window_nanos.get(),
            ),
        );

        let smtlib = system.to_smtlib();
        let outcome = match solve(&system, budget) {
            SolveOutcome::Unsat => ObligationOutcome::decided(
                obligation,
                Discharge::Holds {
                    claim: format!(
                        "`{id}` services {firings} event(s) per {} window within {} each, so no input queue backs up",
                        model.window(),
                        max_service.render()
                    ),
                },
                smtlib,
            ),
            SolveOutcome::Sat(assignment) => {
                let strength = match node.service_time.min {
                    Some(min) if overloads(firings, min, window_nanos) => Strength::Proof,
                    _ => Strength::UnderAssumptions,
                };
                let mut counterexample = Counterexample::from_assignment(
                    format!("`{id}` cannot keep up with its own inputs"),
                    strength,
                    &system,
                    &assignment,
                )
                .with_fact(
                    "firings per window",
                    format!("{firings} per {}", model.window()),
                )
                .with_fact("declared service time", node.service_time.render())
                .with_fact(
                    "work per window",
                    Nanos::new(max_service.get().saturating_mul(firings)).render(),
                )
                .with_fact("window", window_nanos.render())
                .with_step(
                    id.to_string(),
                    format!(
                        "is triggered at {rate} and needs up to {} per event",
                        max_service.render()
                    ),
                );
                for channel in model.inputs_of(id) {
                    let arrivals = model
                        .arrival_rate(&channel.key)
                        .map_or_else(|| "an undetermined rate".to_string(), |r| r.to_string());
                    counterexample = counterexample.with_step(
                        channel.key.to_string(),
                        format!(
                            "delivers at {arrivals} into a queue of {} ({:?}), which fills and then drops",
                            channel.effective_capacity, channel.policy
                        ),
                    );
                }
                counterexample = counterexample.with_remedy(format!(
                    "slow the triggers feeding `{id}`, split its work across nodes, or correct `nodes.{id}.wcet` if the declared service time is pessimistic"
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
    outcomes
}

/// Whether the node is overloaded even at its fastest declared speed.
fn overloads(firings: i128, service: Nanos, window: Nanos) -> bool {
    service.get().saturating_mul(firings) > window.get()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::test_support::{model_of, model_with_profile};

    const CHAIN: &str = "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/50 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
";

    #[test]
    fn without_a_profile_every_node_reports_a_missing_service_time() {
        let model = model_of(CHAIN);
        let outcomes = discharge(&model, &SolverBudget::default());
        assert_eq!(outcomes.len(), 2);
        for outcome in &outcomes {
            assert!(matches!(
                outcome.discharge,
                Discharge::NotAttempted {
                    reason: NotAttempted::NoServiceTime { .. }
                }
            ));
            assert!(outcome.encoded_system.is_none());
        }
    }

    #[test]
    fn an_indeterminate_rate_is_reported_before_the_service_time() {
        let model = model_of(
            "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
  - id: sink
    path: ./sink
    inputs: { raw: sensor/raw }
",
        );
        let outcomes = discharge(&model, &SolverBudget::default());
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            outcomes[0].discharge,
            Discharge::NotAttempted {
                reason: NotAttempted::NoDerivedRate { .. }
            }
        ));
    }

    #[test]
    fn a_fast_enough_node_encodes_an_unsatisfiable_violation() {
        // 50 firings per second at 10ms each is 500ms of work per second.
        let model = model_with_profile(CHAIN, "nodes:\n  detector:\n    wcet: 0.010\n");
        let outcomes = discharge(&model, &SolverBudget::default());
        let detector = outcomes
            .iter()
            .find(|o| o.obligation.subject == Subject::Node("detector".to_string()))
            .expect("detector obligation");
        let text = detector
            .encoded_system
            .as_deref()
            .expect("a system was built");
        assert!(text.contains("(= fire!detector 50)"), "{text}");
        assert!(text.contains("(<= service!detector 10000000)"), "{text}");
        assert!(
            text.contains("(> (* 50 service!detector) 1000000000)"),
            "{text}"
        );
    }

    #[test]
    fn a_slow_node_encodes_a_satisfiable_violation() {
        // 50 firings per second at 30ms each is 1.5s of work per second.
        let model = model_with_profile(CHAIN, "nodes:\n  detector:\n    wcet: 0.030\n");
        let outcomes = discharge(&model, &SolverBudget::default());
        let detector = outcomes
            .iter()
            .find(|o| o.obligation.subject == Subject::Node("detector".to_string()))
            .expect("detector obligation");
        let text = detector.encoded_system.as_deref().expect("a system");
        assert!(text.contains("(>= service!detector 30000000)"), "{text}");
    }

    #[test]
    fn overload_at_the_minimum_is_a_proof() {
        let window = Nanos::new(1_000_000_000);
        assert!(overloads(50, Nanos::new(30_000_000), window));
        assert!(!overloads(50, Nanos::new(10_000_000), window));
    }

    #[test]
    fn source_nodes_are_skipped() {
        let model = model_with_profile(
            "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
",
            "nodes:\n  sensor:\n    rate: 10\n    wcet: 0.001\n",
        );
        assert!(discharge(&model, &SolverBudget::default()).is_empty());
    }

    #[test]
    fn encoding_is_deterministic() {
        let first = discharge(
            &model_with_profile(CHAIN, "nodes:\n  detector:\n    wcet: 0.030\n"),
            &SolverBudget::default(),
        );
        let second = discharge(
            &model_with_profile(CHAIN, "nodes:\n  detector:\n    wcet: 0.030\n"),
            &SolverBudget::default(),
        );
        assert_eq!(first, second);
    }
}
