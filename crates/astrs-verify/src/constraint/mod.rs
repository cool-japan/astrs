//! The solver-independent constraint intermediate representation.
//!
//! Every obligation this crate discharges is first encoded into a
//! [`ConstraintSystem`] — declarations plus labelled assertions in
//! quantifier-free linear integer arithmetic — and only then handed to a
//! solver. The indirection buys three things that a direct-to-solver
//! encoding cannot:
//!
//! 1. **The encodings are testable without the `verify` feature.** The
//!    interesting, error-prone half of a proof is the encoding, not the
//!    solving: whether a siphon closure rule quantifies over the right
//!    transitions, whether a flow equation sums the right edges. Those
//!    tests run in every build.
//! 2. **The obligation has a canonical text.** [`ConstraintSystem::to_smtlib`]
//!    renders the exact system that will be solved, which is both the
//!    crate's determinism check and what a `--prove --explain` reader sees
//!    when they want to know what was actually claimed.
//! 3. **Counterexamples are independently re-checked.** A model the solver
//!    reports satisfying is replayed through
//!    [`ConstraintSystem::evaluate`], against the same constraint objects,
//!    before this crate will print it. A verifier that trusts its solver
//!    without a cross-check has no way to notice an encoding-translation
//!    bug.
//!
//! The grammar is deliberately minimal — see [`formula::Formula`].

mod formula;
mod system;
mod term;
mod var;

pub use formula::{Compare, Formula};
pub use system::{Assertion, Assignment, ConstraintSystem, EvaluationOutcome};
pub use term::Term;
pub use var::{Sort, VarDecl, VarId, VarRegistry};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// The end-to-end shape every obligation encoder follows: declare,
    /// assert in groups, render, and replay a candidate model.
    #[test]
    fn encode_render_and_replay() {
        let mut system = ConstraintSystem::new();
        let fire_camera = system.int_var("fire!camera", "times `camera` fires per window");
        let fire_detector = system.int_var("fire!detector", "times `detector` fires per window");

        system.assert(
            "flow",
            "`camera` fires once per 20ms tick over a 1s window",
            Formula::eq(Term::var(fire_camera), 50i128),
        );
        system.assert(
            "flow",
            "`detector` fires once per message from `camera`",
            Formula::eq(Term::var(fire_detector), Term::var(fire_camera)),
        );
        system.assert(
            "violation",
            "`detector` cannot service its arrivals within the window",
            Formula::gt(
                Term::var(fire_detector).scaled(30_000_000),
                1_000_000_000i128,
            ),
        );

        let text = system.to_smtlib();
        assert!(text.contains("(declare-const fire!camera Int)"), "{text}");
        assert!(text.contains("; --- violation ---"), "{text}");

        let mut assignment = Assignment::new();
        assignment.set(fire_camera, 50);
        assignment.set(fire_detector, 50);
        assert!(system.evaluate(&assignment).is_satisfied());

        // 50 firings × 30ms = 1.5s of work in a 1s window: the violation
        // assertion is genuinely satisfied, which is what makes this a
        // counterexample rather than a proof.
        assert_eq!(
            system.evaluate_term(&Term::var(fire_detector).scaled(30_000_000), &assignment),
            Some(1_500_000_000)
        );
    }

    #[test]
    fn a_wrong_model_is_caught_by_replay() {
        let mut system = ConstraintSystem::new();
        let x = system.int_var("x", "x");
        system.assert("g", "x is exactly 4", Formula::eq(Term::var(x), 4i128));
        let mut assignment = Assignment::new();
        assignment.set(x, 5);
        assert!(!system.evaluate(&assignment).is_satisfied());
    }

    #[test]
    fn rendering_is_a_pure_function_of_structure() {
        let build = || {
            let mut system = ConstraintSystem::new();
            let a = system.bool_var("siphon!a.in", "channel a.in never carries a message");
            let b = system.bool_var("siphon!b.in", "channel b.in never carries a message");
            system.assert(
                "siphon",
                "a.in can only starve if b.in does",
                Formula::bool_var(a).implies(Formula::bool_var(b)),
            );
            system
        };
        assert_eq!(build().to_smtlib(), build().to_smtlib());
    }
}
