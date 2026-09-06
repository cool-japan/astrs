//! Rate consistency (blueprint §15).
//!
//! # Declared versus delivered
//!
//! Two kinds of rate declaration exist, and this obligation checks both
//! against the rate the graph actually delivers.
//!
//! ## The manifest's own: a per-input `timeout`
//!
//! An input's long form (blueprint §8.3) carries a `timeout`, the delivery
//! deadline the daemon holds that input to. Declaring one is a statement
//! about **how often** the input expects traffic: if consecutive messages
//! are further apart than the timeout, the input times out every single
//! period — not occasionally under load, but by construction, from the
//! declared rates alone. A 0.5 Hz producer feeding an input that declares
//! `timeout: 0.5` is a graph that cannot work, and it is visible without
//! running anything.
//!
//! The check is exact and division-free. With `W` the analysis window and
//! `n` the exact number of arrivals in it, the inter-arrival time is `W/n`,
//! and the violation `W/n > timeout` is asserted as
//! `W > timeout · n` — integers throughout, so a rate like 1000/3 Hz is
//! compared without a rounding step anywhere.
//!
//! ## The profile's: a declared node rate
//!
//! A [`Profile`](crate::Profile) may declare a node's rate. For a source
//! node that *is* the rate — there is nothing to check. For an
//! event-driven node the graph already determines the rate, so a
//! declaration is a claim about the wiring, and a claim that disagrees
//! with the derivation is exactly the inconsistency this obligation is for:
//! it means the manifest and the author's mental model have diverged.

use crate::constraint::{ConstraintSystem, Formula, Term};
use crate::counterexample::{Counterexample, Strength};
use crate::model::Model;
use crate::obligation::{Obligation, ObligationKind, Subject};
use crate::report::{Discharge, NotAttempted, ObligationOutcome};
use crate::smt::{SolveOutcome, SolverBudget, solve};

/// Discharge rate consistency: one obligation per declared expectation.
#[must_use]
pub fn discharge(model: &Model, budget: &SolverBudget) -> Vec<ObligationOutcome> {
    let mut outcomes = Vec::new();
    outcomes.extend(check_input_timeouts(model, budget));
    outcomes.extend(check_declared_rates(model, budget));
    if outcomes.is_empty() {
        outcomes.push(ObligationOutcome::unattempted(
            Obligation::whole_graph(ObligationKind::RateConsistency),
            NotAttempted::NothingToProve,
        ));
    }
    outcomes
}

/// Every input that declares a `timeout` must be fed at least that often.
fn check_input_timeouts(model: &Model, budget: &SolverBudget) -> Vec<ObligationOutcome> {
    let mut outcomes = Vec::new();
    for (key, channel) in model.channels() {
        let Some(timeout) = channel.timeout else {
            continue;
        };
        let obligation = Obligation::about(
            ObligationKind::RateConsistency,
            Subject::Channel(key.to_string()),
        );

        let Some(rate) = model.arrival_rate(key) else {
            let node = channel
                .producer
                .node()
                .map_or_else(|| channel.consumer().to_string(), ToString::to_string);
            let why = model
                .rates()
                .reason(&astrs_graph::NodeId::new(node.clone()))
                .unwrap_or(crate::model::Indeterminate::AperiodicSource);
            outcomes.push(ObligationOutcome::unattempted(
                obligation,
                NotAttempted::NoDerivedRate { node, why },
            ));
            continue;
        };

        let Some(arrivals) = rate.events_per_window(model.window()) else {
            outcomes.push(ObligationOutcome::unattempted(
                obligation,
                NotAttempted::ScaleOverflow {
                    detail: format!("`{key}` arrives at {rate} over a {} window", model.window()),
                },
            ));
            continue;
        };

        let window_nanos = model.window().nanos();
        let mut system = ConstraintSystem::new();
        let arrivals_var = system.int_var(
            format!("arrivals!{key}"),
            format!(
                "messages delivered to `{key}` per {} window",
                model.window()
            ),
        );
        system.assert(
            "flow",
            format!(
                "`{key}` is fed by {} at {rate}: {arrivals} message(s) per {} window",
                channel.producer.render(),
                model.window()
            ),
            Formula::eq(Term::var(arrivals_var), arrivals),
        );
        system.assert(
            "violation",
            format!(
                "the gap between messages on `{key}` exceeds its declared timeout of {} (window > timeout x arrivals)",
                timeout.render()
            ),
            Formula::gt(
                Term::constant(window_nanos.get()),
                Term::var(arrivals_var).scaled(timeout.get()),
            ),
        );

        let smtlib = system.to_smtlib();
        let outcome = match solve(&system, budget) {
            SolveOutcome::Unsat => ObligationOutcome::decided(
                obligation,
                Discharge::Holds {
                    claim: format!(
                        "`{key}` is fed at {rate}, inside its declared {} timeout",
                        timeout.render()
                    ),
                },
                smtlib,
            ),
            SolveOutcome::Sat(assignment) => {
                let period = rate
                    .exact_period_nanos()
                    .map_or_else(|| format!("1/({rate})"), |p| p.render());
                let counterexample = Counterexample::from_assignment(
                    format!("`{key}` times out on every message it receives"),
                    Strength::Proof,
                    &system,
                    &assignment,
                )
                .with_fact("declared timeout", timeout.render())
                .with_fact("arrival rate", rate.to_string())
                .with_fact("gap between messages", period)
                .with_step(
                    channel.producer.render(),
                    format!("produces at {rate}"),
                )
                .with_step(
                    key.to_string(),
                    format!(
                        "declares `timeout: {}`, which the producer's own rate cannot meet",
                        seconds(timeout.get())
                    ),
                )
                .with_remedy(format!(
                    "raise `timeout` on `{key}` above the producer's period, or speed the producer up"
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

/// Every profile-declared rate on an event-driven node must match what the
/// graph derives.
fn check_declared_rates(model: &Model, budget: &SolverBudget) -> Vec<ObligationOutcome> {
    let mut outcomes = Vec::new();
    for (id, declared) in model.declared_rates() {
        let Some(node) = model.node(id) else {
            continue;
        };
        if node.is_source() {
            // The declaration *is* the rate; there is nothing to compare
            // it against.
            continue;
        }
        let obligation = Obligation::about(
            ObligationKind::RateConsistency,
            Subject::Node(id.to_string()),
        );
        let Some(derived) = node.derived_rate else {
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

        let window = model.window();
        let (Some(declared_count), Some(derived_count)) = (
            declared.events_per_window(window),
            derived.events_per_window(window),
        ) else {
            outcomes.push(ObligationOutcome::unattempted(
                obligation,
                NotAttempted::ScaleOverflow {
                    detail: format!("`{id}` declares {declared} against a derived {derived}"),
                },
            ));
            continue;
        };

        let mut system = ConstraintSystem::new();
        let derived_var = system.int_var(
            format!("derived!{id}"),
            format!("times `{id}` fires per {window} window, derived from the graph"),
        );
        let declared_var = system.int_var(
            format!("declared!{id}"),
            format!("times `{id}` fires per {window} window, as the profile declares"),
        );
        system.assert(
            "flow",
            format!("the graph drives `{id}` at {derived}"),
            Formula::eq(Term::var(derived_var), derived_count),
        );
        system.assert(
            "declaration",
            format!("the profile declares `{id}` at {declared}"),
            Formula::eq(Term::var(declared_var), declared_count),
        );
        system.assert(
            "violation",
            format!("the declared and derived rates of `{id}` disagree"),
            Formula::compare(
                crate::constraint::Compare::Ne,
                Term::var(declared_var),
                Term::var(derived_var),
            ),
        );

        let smtlib = system.to_smtlib();
        let outcome = match solve(&system, budget) {
            SolveOutcome::Unsat => ObligationOutcome::decided(
                obligation,
                Discharge::Holds {
                    claim: format!("`{id}` runs at the declared {declared}"),
                },
                smtlib,
            ),
            SolveOutcome::Sat(assignment) => {
                let counterexample = Counterexample::from_assignment(
                    format!("`{id}` does not run at its declared rate"),
                    Strength::Proof,
                    &system,
                    &assignment,
                )
                .with_fact("declared rate", declared.to_string())
                .with_fact("rate the graph delivers", derived.to_string())
                .with_step(
                    id.to_string(),
                    format!(
                        "fires once per delivered event, and its inputs together deliver {derived}"
                    ),
                )
                .with_remedy(format!(
                    "correct `nodes.{id}.rate` in the profile, or rewire `{id}`'s inputs to match the declaration"
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

/// Render a nanosecond count the way a manifest would write it.
fn seconds(nanos: i128) -> String {
    let whole = nanos / 1_000_000_000;
    let fraction = (nanos % 1_000_000_000).unsigned_abs();
    if fraction == 0 {
        format!("{whole}")
    } else {
        format!("{whole}.{}", format!("{fraction:09}").trim_end_matches('0'))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::test_support::{model_of, model_with_profile};

    fn slow_producer(timeout: &str) -> String {
        format!(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: {{ tick: astrs/timer/secs/2 }}
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        timeout: {timeout}
"
        )
    }

    #[test]
    fn a_timeout_below_the_producer_period_encodes_a_satisfiable_violation() {
        let model = model_of(&slow_producer("0.5"));
        let outcomes = discharge(&model, &SolverBudget::default());
        let outcome = &outcomes[0];
        let text = outcome.encoded_system.as_deref().expect("a system");
        // 0.5 Hz over a 2s window is one arrival; 2e9 > 5e8 * 1.
        assert!(text.contains("(= arrivals!detector.frames 1)"), "{text}");
        assert!(
            text.contains("(> 2000000000 (* 500000000 arrivals!detector.frames))"),
            "{text}"
        );
    }

    #[test]
    fn a_generous_timeout_encodes_an_unsatisfiable_violation() {
        let model = model_of(&slow_producer("5.0"));
        let outcomes = discharge(&model, &SolverBudget::default());
        let text = outcomes[0].encoded_system.as_deref().expect("a system");
        assert!(
            text.contains("(> 2000000000 (* 5000000000 arrivals!detector.frames))"),
            "{text}"
        );
    }

    #[test]
    fn a_graph_with_no_declarations_has_nothing_to_prove() {
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { tick: astrs/timer/hz/1 }
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
    fn an_indeterminate_producer_rate_is_reported() {
        let model = model_of(
            "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
  - id: sink
    path: ./sink
    inputs:
      raw:
        source: sensor/raw
        timeout: 0.1
",
        );
        let outcomes = discharge(&model, &SolverBudget::default());
        assert!(matches!(
            outcomes[0].discharge,
            Discharge::NotAttempted {
                reason: NotAttempted::NoDerivedRate { .. }
            }
        ));
    }

    #[test]
    fn a_profile_rate_on_a_source_node_is_not_second_guessed() {
        let model = model_with_profile(
            "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
  - id: sink
    path: ./sink
    inputs: { raw: sensor/raw }
",
            "nodes:\n  sensor:\n    rate: 10\n",
        );
        let outcomes = discharge(&model, &SolverBudget::default());
        assert!(
            outcomes
                .iter()
                .all(|o| o.obligation.subject != Subject::Node("sensor".to_string())),
            "a source node's declared rate is the rate, not a claim to check"
        );
    }

    #[test]
    fn a_profile_rate_on_an_event_driven_node_is_compared() {
        let model = model_with_profile(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/50 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
",
            "nodes:\n  detector:\n    rate: 30\n",
        );
        let outcomes = discharge(&model, &SolverBudget::default());
        let detector = outcomes
            .iter()
            .find(|o| o.obligation.subject == Subject::Node("detector".to_string()))
            .expect("detector obligation");
        let text = detector.encoded_system.as_deref().expect("a system");
        assert!(text.contains("(= derived!detector 50)"), "{text}");
        assert!(text.contains("(= declared!detector 30)"), "{text}");
        assert!(
            text.contains("(distinct declared!detector derived!detector)"),
            "{text}"
        );
    }

    #[test]
    fn seconds_renders_like_a_manifest_would() {
        assert_eq!(seconds(500_000_000), "0.5");
        assert_eq!(seconds(2_000_000_000), "2");
        assert_eq!(seconds(1_250_000_000), "1.25");
    }

    #[test]
    fn encoding_is_deterministic() {
        let yaml = slow_producer("0.5");
        let first = discharge(&model_of(&yaml), &SolverBudget::default());
        let second = discharge(&model_of(&yaml), &SolverBudget::default());
        assert_eq!(first, second);
    }
}
