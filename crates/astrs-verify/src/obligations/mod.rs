//! The obligation catalog: one module per claim in blueprint §15.
//!
//! Every obligation follows the same shape, and the uniformity is
//! deliberate:
//!
//! 1. **Encode the violation, not the property.** Each module builds a
//!    [`ConstraintSystem`](crate::ConstraintSystem) that is satisfiable
//!    exactly when the graph is broken. So `unsat` proves the obligation
//!    and `sat` hands back a witness — one polarity across the whole
//!    crate, recorded per obligation as
//!    [`Polarity::SatIsViolation`](crate::Polarity::SatIsViolation).
//! 2. **Report what you cannot decide.** An obligation missing a fact it
//!    needs returns [`NotAttempted`](crate::NotAttempted) naming the fact
//!    and how to supply it; an obligation the solver could not settle
//!    returns [`Inconclusive`](crate::Inconclusive). Neither is folded
//!    into "holds".
//! 3. **Translate the model.** A satisfying assignment becomes a
//!    [`Counterexample`](crate::Counterexample) that names nodes and
//!    channels, and it is replayed against the constraint system before it
//!    is believed.
//!
//! Each module's own header carries the argument for *why* its encoding is
//! the right one, which is where the real content of this crate lives.

pub mod boundedness;
pub mod deadlock;
pub mod latency;
pub mod rate;
pub mod typing;

use crate::model::Model;
use crate::report::ObligationOutcome;
use crate::smt::SolverBudget;

/// Discharge every obligation, in [`ObligationKind::ALL`] order.
///
/// [`ObligationKind::ALL`]: crate::ObligationKind::ALL
#[must_use]
pub fn discharge_all(model: &Model, budget: &SolverBudget) -> Vec<ObligationOutcome> {
    let mut outcomes = Vec::new();
    outcomes.push(deadlock::discharge(model, budget));
    outcomes.extend(boundedness::discharge(model, budget));
    outcomes.extend(rate::discharge(model, budget));
    outcomes.extend(latency::discharge(model, budget));
    outcomes.extend(typing::discharge(model, budget));
    outcomes
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::obligation::ObligationKind;
    use crate::test_support::model_of;

    #[test]
    fn every_kind_appears_in_a_full_run() {
        let model = model_of(
            "
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
        timeout: 1.0
",
        );
        let outcomes = discharge_all(&model, &SolverBudget::default());
        for kind in ObligationKind::ALL {
            assert!(
                outcomes.iter().any(|o| o.obligation.kind == kind),
                "{kind} is missing from the catalog run"
            );
        }
    }

    #[test]
    fn obligation_ids_are_unique_within_a_run() {
        let model = model_of(
            "
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
        timeout: 1.0
    outputs: [dets]
  - id: planner
    path: ./planner
    inputs:
      dets:
        source: detector/dets
        timeout: 2.0
",
        );
        let outcomes = discharge_all(&model, &SolverBudget::default());
        let mut ids: Vec<&str> = outcomes.iter().map(|o| o.obligation.id.as_str()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "obligation ids must not collide");
    }

    #[test]
    fn a_full_run_is_deterministic() {
        let yaml = "
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
        timeout: 1.0
";
        let first = discharge_all(&model_of(yaml), &SolverBudget::default());
        let second = discharge_all(&model_of(yaml), &SolverBudget::default());
        assert_eq!(first, second);
    }
}
