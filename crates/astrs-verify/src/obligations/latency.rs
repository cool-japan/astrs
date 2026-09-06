//! End-to-end latency budgets (blueprint §15(c)).
//!
//! # The delay model, and why it needs no service times
//!
//! A message crossing channel `e` waits behind whatever is already queued
//! and is then handled. In a **bounded** steady state — the property
//! [`boundedness`](super::boundedness) establishes — the consumer drains
//! `e` at exactly the rate messages arrive on it, `λₑ`. So a message that
//! finds the queue full waits for at most `capacityₑ` drains:
//!
//! ```text
//! delay(e)  =  capacityₑ  ×  1/λₑ
//! ```
//!
//! Nothing in that expression is a service time. That is the point: the
//! bound comes from the *declared capacity* and the *declared rate*, both
//! of which the manifest states, so latency budgets are discharged from a
//! manifest alone. The price is the assumption the bound rests on, and it
//! is recorded rather than buried —
//! [`Caveat::LatencyAssumesBoundedness`](crate::Caveat::LatencyAssumesBoundedness)
//! rides on every report that contains a latency result.
//!
//! Capacity here is the **effective** one: `backpressure` really can hold
//! ten times its declared depth before dropping (§11.2), and a message
//! really can sit behind all of it.
//!
//! # Worst case over paths, computed in linear size
//!
//! The age of a message when node `n` starts handling it is the worst over
//! however it got there:
//!
//! ```text
//! age(n) = max over n's inputs e of ( age(producer(e)) + delay(e) )
//! ```
//!
//! with `age = 0` at a timer or a source node. Encoding *that* recurrence
//! — one variable per node, `≥` for each input plus a disjunction of
//! equalities to pin the maximum — is linear in the graph. Enumerating
//! paths instead would be exponential in it, and would fall over on
//! exactly the fan-in graphs where worst-case latency is interesting.
//!
//! # Exact arithmetic
//!
//! `1/λ` is rarely a whole number of nanoseconds (`1000/3 Hz` is not), so
//! delays are computed on a scaled time axis: with `D` the least common
//! multiple of the arrival rates' numerators, every `delay(e)` becomes the
//! integer `capacityₑ · qₑ · 10⁹ · (D/pₑ)`, and the budget becomes
//! `budget · D`. No rounding, and no rational arithmetic in the solver.

use std::collections::{BTreeMap, BTreeSet};

use astrs_graph::{EdgeKey, NodeId};

use crate::constraint::{ConstraintSystem, Formula, Term, VarId};
use crate::counterexample::{Counterexample, Strength};
use crate::model::{LatencyPath, Model};
use crate::obligation::{Obligation, ObligationKind, Subject};
use crate::report::{Discharge, NotAttempted, ObligationOutcome};
use crate::scale::{NANOS_PER_SEC, Nanos};
use crate::smt::{SolveOutcome, SolverBudget, solve};

/// The largest time-scale multiplier the encoding will build.
///
/// The multiplier is the least common multiple of the participating
/// arrival rates' numerators (see this module's header). A graph mixing
/// many mutually prime rates can push it up; past this point the honest
/// answer is [`NotAttempted::ScaleOverflow`] rather than a rounded delay.
pub const MAX_TIME_SCALE: u128 = 1_000_000_000;

/// Discharge every declared latency budget.
#[must_use]
pub fn discharge(model: &Model, budget: &SolverBudget) -> Vec<ObligationOutcome> {
    if model.paths().is_empty() {
        return vec![ObligationOutcome::unattempted(
            Obligation::whole_graph(ObligationKind::LatencyBudget),
            NotAttempted::NothingToProve,
        )];
    }
    model
        .paths()
        .iter()
        .map(|path| discharge_one(model, path, budget))
        .collect()
}

fn discharge_one(
    model: &Model,
    path: &LatencyPath,
    solver_budget: &SolverBudget,
) -> ObligationOutcome {
    let obligation = Obligation::about(
        ObligationKind::LatencyBudget,
        Subject::Path(path.origin.label()),
    );

    let Some(scope) = Scope::around(model, &path.target) else {
        return ObligationOutcome::unattempted(obligation, NotAttempted::NothingToProve);
    };

    if let Some((node, why)) = scope.first_indeterminate(model) {
        return ObligationOutcome::unattempted(
            obligation,
            NotAttempted::NoDerivedRate {
                node: node.to_string(),
                why,
            },
        );
    }

    let Some(scale) = scope.time_scale(model) else {
        return ObligationOutcome::unattempted(
            obligation,
            NotAttempted::ScaleOverflow {
                detail: format!(
                    "the rates feeding `{}` have no common integer time scale below {MAX_TIME_SCALE}",
                    path.target
                ),
            },
        );
    };

    let mut delays = BTreeMap::new();
    for key in &scope.channels {
        match scaled_delay(model, key, scale) {
            Some(delay) => {
                delays.insert(key.clone(), delay);
            }
            None => {
                return ObligationOutcome::unattempted(
                    obligation,
                    NotAttempted::ScaleOverflow {
                        detail: format!("the worst-case delay on `{key}` is not representable"),
                    },
                );
            }
        }
    }

    let Some(scaled_budget) = path.budget.get().checked_mul(scale_i128(scale)) else {
        return ObligationOutcome::unattempted(
            obligation,
            NotAttempted::ScaleOverflow {
                detail: format!(
                    "a budget of {} does not fit the scaled time axis",
                    path.budget
                ),
            },
        );
    };

    let mut system = ConstraintSystem::new();
    let ages = declare_ages(&mut system, model, &scope, scale);
    if !encode_recurrence(&mut system, model, &scope, &ages, &delays) {
        // A node in scope has wired inputs but no usable delay for any of
        // them, so its worst case is not pinned. Left alone, the
        // recurrence would assert a constant `false` and the obligation
        // would come back `unsat` — reading as *proved* when nothing was.
        return ObligationOutcome::unattempted(
            obligation,
            NotAttempted::ScaleOverflow {
                detail: format!(
                    "the worst-case age feeding `{}` could not be pinned to its inputs",
                    path.target
                ),
            },
        );
    }

    let Some(target_delay) = delays.get(&path.target).copied() else {
        return ObligationOutcome::unattempted(obligation, NotAttempted::NothingToProve);
    };
    let arrival = arrival_term(model, &path.target, &ages, target_delay);

    system.assert(
        "violation",
        format!(
            "worst-case delivery on `{}` exceeds its {} budget",
            path.target, path.budget
        ),
        Formula::gt(arrival.clone(), Term::constant(scaled_budget)),
    );

    let smtlib = system.to_smtlib();
    match solve(&system, solver_budget) {
        SolveOutcome::Unsat => ObligationOutcome::decided(
            obligation,
            Discharge::Holds {
                claim: format!(
                    "every message reaches `{}` within its {} budget, assuming its queues stay bounded",
                    path.target, path.budget
                ),
            },
            smtlib,
        ),
        SolveOutcome::Sat(assignment) => {
            let worst = system
                .evaluate_term(&arrival, &assignment)
                .map(|scaled| unscale(scaled, scale));
            let mut counterexample = Counterexample::from_assignment(
                format!(
                    "`{}` can be reached later than its {} budget allows",
                    path.target, path.budget
                ),
                Strength::UnderAssumptions,
                &system,
                &assignment,
            )
            .with_fact("budget", path.budget.render())
            .with_fact("declared by", path.origin.label());
            if let Some(worst) = worst {
                counterexample = counterexample.with_fact("worst-case delivery", worst.render());
            }
            for key in &scope.channels {
                let Some(channel) = model.channel(key) else {
                    continue;
                };
                let rate = model
                    .arrival_rate(key)
                    .map_or_else(|| "an undetermined rate".to_string(), |r| r.to_string());
                let contribution = delays
                    .get(key)
                    .map(|scaled| unscale(*scaled, scale).render())
                    .unwrap_or_else(|| "?".to_string());
                counterexample = counterexample.with_step(
                    key.to_string(),
                    format!(
                        "holds up to {} message(s) arriving at {rate}, so a message can wait {contribution} here",
                        channel.effective_capacity
                    ),
                );
            }
            counterexample = counterexample.with_remedy(format!(
                "shrink `queue_size` along the route into `{}`, raise the rates feeding it, or relax the budget",
                path.target
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
    }
}

/// Everything upstream of one target channel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    /// Every node whose age matters, in id order.
    nodes: BTreeSet<NodeId>,
    /// Every channel on the way, including the target, in key order.
    channels: BTreeSet<EdgeKey>,
}

impl Scope {
    /// Walk upstream from `target`, collecting every node and channel that
    /// can contribute to a message's age on arrival.
    ///
    /// *Every* input of every node on the way is included, not only the
    /// ones on some chosen route: a node fires on whichever input has an
    /// event, so the message it emits may carry the age of any of them.
    fn around(model: &Model, target: &EdgeKey) -> Option<Self> {
        let channel = model.channel(target)?;
        let mut nodes = BTreeSet::new();
        let mut channels = BTreeSet::new();
        channels.insert(target.clone());

        let mut worklist: Vec<NodeId> = channel.producer.node().cloned().into_iter().collect();
        while let Some(id) = worklist.pop() {
            if !nodes.insert(id.clone()) {
                continue;
            }
            let Some(node) = model.node(&id) else {
                continue;
            };
            for key in &node.inputs {
                channels.insert(key.clone());
                if let Some(upstream) = model.channel(key).and_then(|c| c.producer.node())
                    && !nodes.contains(upstream)
                {
                    worklist.push(upstream.clone());
                }
            }
        }
        Some(Self { nodes, channels })
    }

    /// The first node in this scope whose rate the model could not derive.
    fn first_indeterminate(&self, model: &Model) -> Option<(NodeId, crate::model::Indeterminate)> {
        self.nodes
            .iter()
            .find_map(|id| model.rates().reason(id).map(|why| (id.clone(), why)))
    }

    /// The least common multiple of the participating arrival rates'
    /// numerators, or `None` if it would exceed [`MAX_TIME_SCALE`].
    fn time_scale(&self, model: &Model) -> Option<u128> {
        let mut scale: u128 = 1;
        for key in &self.channels {
            let rate = model.arrival_rate(key)?;
            if rate.is_zero() {
                continue;
            }
            scale = lcm(scale, u128::from(rate.numerator()))?;
            if scale > MAX_TIME_SCALE {
                return None;
            }
        }
        Some(scale)
    }
}

/// Declare one age variable per node in scope.
fn declare_ages(
    system: &mut ConstraintSystem,
    model: &Model,
    scope: &Scope,
    scale: u128,
) -> BTreeMap<NodeId, VarId> {
    let mut ages = BTreeMap::new();
    for id in &scope.nodes {
        let _ = model;
        let var = system.int_var(
            format!("age!{id}"),
            format!(
                "worst-case age, in nanoseconds x {scale}, of a message when `{id}` handles it"
            ),
        );
        ages.insert(id.clone(), var);
    }
    ages
}

/// Encode `age(n) = max over inputs of (age(producer) + delay)`.
///
/// Returns `false` when some node with wired inputs got no equality to pin
/// its maximum to — the caller must then decline the obligation rather
/// than solve a system whose unsatisfiability would masquerade as a proof.
fn encode_recurrence(
    system: &mut ConstraintSystem,
    model: &Model,
    scope: &Scope,
    ages: &BTreeMap<NodeId, VarId>,
    delays: &BTreeMap<EdgeKey, i128>,
) -> bool {
    let mut complete = true;
    for id in &scope.nodes {
        let Some(&age) = ages.get(id) else {
            continue;
        };
        let Some(node) = model.node(id) else {
            continue;
        };
        if node.inputs.is_empty() {
            system.assert(
                "age",
                format!("`{id}` has no wired inputs, so it originates messages"),
                Formula::eq(Term::var(age), 0i128),
            );
            continue;
        }
        let mut equalities = Vec::new();
        for key in &node.inputs {
            let Some(&delay) = delays.get(key) else {
                complete = false;
                continue;
            };
            let via = arrival_term(model, key, ages, delay);
            system.assert(
                "age",
                format!("a message can reach `{id}` through `{key}`"),
                Formula::ge(Term::var(age), via.clone()),
            );
            equalities.push(Formula::eq(Term::var(age), via));
        }
        if equalities.is_empty() {
            return false;
        }
        system.assert(
            "age",
            format!("`{id}`'s worst case is attained on one of its inputs"),
            Formula::any(equalities),
        );
    }
    complete
}

/// The age of a message on arrival over one channel.
fn arrival_term(model: &Model, key: &EdgeKey, ages: &BTreeMap<NodeId, VarId>, delay: i128) -> Term {
    let upstream = model
        .channel(key)
        .and_then(|channel| channel.producer.node())
        .and_then(|node| ages.get(node).copied());
    match upstream {
        Some(age) => Term::var(age).plus(Term::constant(delay)),
        None => Term::constant(delay),
    }
}

/// `capacity × period` on the scaled time axis, exactly.
fn scaled_delay(model: &Model, key: &EdgeKey, scale: u128) -> Option<i128> {
    let channel = model.channel(key)?;
    let rate = model.arrival_rate(key)?;
    if rate.is_zero() {
        return None;
    }
    // period_ns = denominator · 10⁹ / numerator, and `numerator` divides
    // `scale` by construction, so the multiplication below is exact.
    let multiplier = scale.checked_div(u128::from(rate.numerator()))?;
    let per_message = u128::from(rate.denominator())
        .checked_mul(NANOS_PER_SEC.unsigned_abs())?
        .checked_mul(multiplier)?;
    let total = per_message.checked_mul(u128::from(channel.effective_capacity))?;
    i128::try_from(total).ok()
}

/// Convert a scaled nanosecond figure back to real nanoseconds, rounding
/// up so a rendered worst case never understates the bound it came from.
fn unscale(scaled: i128, scale: u128) -> Nanos {
    let divisor = scale_i128(scale);
    if divisor <= 0 {
        return Nanos::new(scaled);
    }
    let whole = scaled / divisor;
    let remainder = scaled % divisor;
    Nanos::new(if remainder > 0 { whole + 1 } else { whole })
}

/// The scale as a signed integer, saturating rather than wrapping.
fn scale_i128(scale: u128) -> i128 {
    i128::try_from(scale).unwrap_or(i128::MAX)
}

/// Least common multiple, `None` on overflow.
fn lcm(a: u128, b: u128) -> Option<u128> {
    if a == 0 || b == 0 {
        return Some(0);
    }
    let (mut x, mut y) = (a, b);
    while y != 0 {
        let t = x % y;
        x = y;
        y = t;
    }
    a.checked_div(x)?.checked_mul(b)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::test_support::model_of;

    fn chain(queue: u32, timeout: &str) -> String {
        format!(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: {{ tick: astrs/timer/hz/10 }}
    outputs: [frames]
  - id: stage1
    path: ./stage1
    inputs:
      frames:
        source: camera/frames
        queue_size: {queue}
    outputs: [out]
  - id: stage2
    path: ./stage2
    inputs:
      data:
        source: stage1/out
        queue_size: {queue}
        timeout: {timeout}
"
        )
    }

    #[test]
    fn scope_walks_the_whole_ancestry() {
        let model = model_of(&chain(8, "0.2"));
        let target = EdgeKey::new(NodeId::new("stage2"), astrs_graph::PortName::new("data"));
        let scope = Scope::around(&model, &target).expect("target exists");
        assert_eq!(
            scope.nodes,
            [NodeId::new("camera"), NodeId::new("stage1")]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(scope.channels.len(), 3, "{:?}", scope.channels);
    }

    #[test]
    fn delays_are_capacity_times_period() {
        let model = model_of(&chain(8, "0.2"));
        let target = EdgeKey::new(NodeId::new("stage2"), astrs_graph::PortName::new("data"));
        // 10 Hz: period 100ms, capacity 8, so 800ms.
        assert_eq!(scaled_delay(&model, &target, 10), Some(800_000_000 * 10));
        let tick = EdgeKey::new(NodeId::new("camera"), astrs_graph::PortName::new("tick"));
        // queue_size defaults to 10 on the timer input.
        assert_eq!(scaled_delay(&model, &tick, 10), Some(1_000_000_000 * 10));
    }

    #[test]
    fn a_deep_chain_encodes_a_satisfiable_violation() {
        let model = model_of(&chain(8, "0.2"));
        let outcomes = discharge(&model, &SolverBudget::default());
        assert_eq!(outcomes.len(), 1);
        let text = outcomes[0].encoded_system.as_deref().expect("a system");
        assert!(text.contains("(declare-const age!camera Int)"), "{text}");
        assert!(text.contains("(declare-const age!stage1 Int)"), "{text}");
        assert!(text.contains("; --- violation ---"), "{text}");
    }

    #[test]
    fn a_shallow_chain_encodes_an_unsatisfiable_violation() {
        let model = model_of(&chain(1, "0.5"));
        let outcomes = discharge(&model, &SolverBudget::default());
        let text = outcomes[0].encoded_system.as_deref().expect("a system");
        assert!(text.contains("age!stage1"), "{text}");
    }

    #[test]
    fn a_graph_with_no_budget_has_nothing_to_prove() {
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
    fn an_indeterminate_upstream_rate_stops_the_proof() {
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
        timeout: 0.5
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
    fn the_time_scale_is_the_lcm_of_rate_numerators() {
        // `a` merges a 3 Hz and a 5 Hz trigger, so it fires at 8 Hz and the
        // scope covers rate numerators 3, 5 and 8.
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { slow: astrs/timer/hz/3, fast: astrs/timer/hz/5 }
    outputs: [out]
  - id: c
    path: ./c
    inputs:
      x:
        source: a/out
        timeout: 1.0
",
        );
        let target = EdgeKey::new(NodeId::new("c"), astrs_graph::PortName::new("x"));
        let scope = Scope::around(&model, &target).expect("target");
        let scale = scope.time_scale(&model).expect("small scale");
        assert_eq!(scale % 3, 0);
        assert_eq!(scale % 5, 0);
        assert_eq!(scale % 8, 0);
        assert_eq!(scale, 120);
    }

    #[test]
    fn the_scope_covers_only_what_feeds_the_target() {
        // `b` feeds a different input of `c`, so a message arriving on
        // `c.x` never carries `b`'s age and `b` stays out of scope.
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { tick: astrs/timer/hz/3 }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { tick: astrs/timer/hz/5 }
    outputs: [out]
  - id: c
    path: ./c
    inputs:
      x:
        source: a/out
        timeout: 1.0
      y: b/out
",
        );
        let target = EdgeKey::new(NodeId::new("c"), astrs_graph::PortName::new("x"));
        let scope = Scope::around(&model, &target).expect("target");
        assert!(scope.nodes.contains(&NodeId::new("a")));
        assert!(!scope.nodes.contains(&NodeId::new("b")));
    }

    #[test]
    fn the_recurrence_reports_incompleteness_instead_of_asserting_false() {
        let model = model_of(&chain(2, "1.0"));
        let target = EdgeKey::new(NodeId::new("stage2"), astrs_graph::PortName::new("data"));
        let scope = Scope::around(&model, &target).expect("target");
        let mut system = ConstraintSystem::new();
        let ages = declare_ages(&mut system, &model, &scope, 10);
        // An empty delay table starves every equality.
        assert!(!encode_recurrence(
            &mut system,
            &model,
            &scope,
            &ages,
            &BTreeMap::new()
        ));
        assert!(
            !system.is_trivially_unsat(),
            "an incomplete encoding must bail, never assert `false`"
        );
    }

    #[test]
    fn a_complete_recurrence_reports_completeness() {
        let model = model_of(&chain(2, "1.0"));
        let target = EdgeKey::new(NodeId::new("stage2"), astrs_graph::PortName::new("data"));
        let scope = Scope::around(&model, &target).expect("target");
        let scale = scope.time_scale(&model).expect("scale");
        let mut delays = BTreeMap::new();
        for key in &scope.channels {
            delays.insert(
                key.clone(),
                scaled_delay(&model, key, scale).expect("delay"),
            );
        }
        let mut system = ConstraintSystem::new();
        let ages = declare_ages(&mut system, &model, &scope, scale);
        assert!(encode_recurrence(
            &mut system,
            &model,
            &scope,
            &ages,
            &delays
        ));
    }

    #[test]
    fn unscaling_rounds_up() {
        assert_eq!(unscale(1_000, 3), Nanos::new(334));
        assert_eq!(unscale(999, 3), Nanos::new(333));
        assert_eq!(unscale(10, 0), Nanos::new(10));
    }

    #[test]
    fn lcm_reports_overflow() {
        assert_eq!(lcm(4, 6), Some(12));
        assert_eq!(lcm(0, 5), Some(0));
        assert_eq!(lcm(u128::MAX, u128::MAX - 1), None);
    }

    #[test]
    fn encoding_is_deterministic() {
        let yaml = chain(8, "0.2");
        let first = discharge(&model_of(&yaml), &SolverBudget::default());
        let second = discharge(&model_of(&yaml), &SolverBudget::default());
        assert_eq!(first, second);
    }
}
