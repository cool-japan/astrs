//! Quantifier-free formulas over [`Term`]s and boolean variables.
//!
//! The whole grammar fits on one screen, and that is the point: it is
//! exactly QF_LIA, so [`crate::smt`]'s translation to the solver is a
//! total function with no "unsupported construct" arm. Anything an
//! obligation wants to say — a siphon closure rule, a flow equation, a
//! latency bound — is expressible here, and nothing else is.

use super::term::Term;
use super::var::{VarId, VarRegistry};

/// A comparison between two linear terms.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Compare {
    /// `=`
    Eq,
    /// `distinct`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl Compare {
    /// This comparison's SMT-LIB2 operator.
    #[must_use]
    pub const fn smtlib_op(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "distinct",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    /// Whether `left <op> right` holds for two concrete integers.
    ///
    /// Used to evaluate a candidate model without a solver, which is how
    /// [`crate::constraint::ConstraintSystem::evaluate`] independently
    /// re-checks every counterexample a solver hands back.
    #[must_use]
    pub const fn holds(self, left: i128, right: i128) -> bool {
        match self {
            Self::Eq => left == right,
            Self::Ne => left != right,
            Self::Lt => left < right,
            Self::Le => left <= right,
            Self::Gt => left > right,
            Self::Ge => left >= right,
        }
    }
}

/// One quantifier-free formula.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Formula {
    /// The constant `true`.
    True,
    /// The constant `false`.
    False,
    /// A boolean variable.
    Bool(VarId),
    /// Negation.
    Not(Box<Formula>),
    /// Conjunction; an empty conjunction is `true`.
    And(Vec<Formula>),
    /// Disjunction; an empty disjunction is `false`.
    Or(Vec<Formula>),
    /// Implication.
    Implies(Box<Formula>, Box<Formula>),
    /// A comparison between two linear terms.
    Compare {
        /// The comparison operator.
        op: Compare,
        /// The left-hand term.
        left: Term,
        /// The right-hand term.
        right: Term,
    },
}

impl Formula {
    /// A boolean variable as a formula.
    #[must_use]
    pub fn bool_var(id: VarId) -> Self {
        Self::Bool(id)
    }

    /// Negate a formula, folding double negations and constants so the
    /// rendered output stays as small as the encodings' intent.
    ///
    /// Named `not` rather than implementing [`std::ops::Not`] on purpose:
    /// `!formula` would read as a value-level negation of a boolean, and
    /// this is a *syntactic* transformation that also simplifies. An
    /// encoder should see that it is building a term.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Not(inner) => *inner,
            other => Self::Not(Box::new(other)),
        }
    }

    /// Conjoin a sequence, dropping `true`s and collapsing on a `false`.
    #[must_use]
    pub fn all(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut kept = Vec::new();
        for part in parts {
            match part {
                Self::True => {}
                Self::False => return Self::False,
                other => kept.push(other),
            }
        }
        match kept.len() {
            0 => Self::True,
            1 => kept.swap_remove(0),
            _ => Self::And(kept),
        }
    }

    /// Disjoin a sequence, dropping `false`s and collapsing on a `true`.
    #[must_use]
    pub fn any(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut kept = Vec::new();
        for part in parts {
            match part {
                Self::False => {}
                Self::True => return Self::True,
                other => kept.push(other),
            }
        }
        match kept.len() {
            0 => Self::False,
            1 => kept.swap_remove(0),
            _ => Self::Or(kept),
        }
    }

    /// `antecedent ⇒ consequent`.
    #[must_use]
    pub fn implies(self, consequent: Self) -> Self {
        match (&self, &consequent) {
            (Self::False, _) | (_, Self::True) => Self::True,
            (Self::True, _) => consequent,
            _ => Self::Implies(Box::new(self), Box::new(consequent)),
        }
    }

    /// `left = right`.
    #[must_use]
    pub fn eq(left: impl Into<Term>, right: impl Into<Term>) -> Self {
        Self::compare(Compare::Eq, left, right)
    }

    /// `left ≤ right`.
    #[must_use]
    pub fn le(left: impl Into<Term>, right: impl Into<Term>) -> Self {
        Self::compare(Compare::Le, left, right)
    }

    /// `left < right`.
    #[must_use]
    pub fn lt(left: impl Into<Term>, right: impl Into<Term>) -> Self {
        Self::compare(Compare::Lt, left, right)
    }

    /// `left ≥ right`.
    #[must_use]
    pub fn ge(left: impl Into<Term>, right: impl Into<Term>) -> Self {
        Self::compare(Compare::Ge, left, right)
    }

    /// `left > right`.
    #[must_use]
    pub fn gt(left: impl Into<Term>, right: impl Into<Term>) -> Self {
        Self::compare(Compare::Gt, left, right)
    }

    /// A comparison with an explicit operator.
    #[must_use]
    pub fn compare(op: Compare, left: impl Into<Term>, right: impl Into<Term>) -> Self {
        Self::Compare {
            op,
            left: left.into(),
            right: right.into(),
        }
    }

    /// Every variable this formula mentions, in first-appearance order.
    ///
    /// Callers that need a canonical set should sort the result; the
    /// traversal order is structural (and therefore itself deterministic),
    /// which is enough for the rendering and evaluation paths that use it.
    #[must_use]
    pub fn variables(&self) -> Vec<VarId> {
        let mut out = Vec::new();
        self.collect_variables(&mut out);
        out
    }

    fn collect_variables(&self, out: &mut Vec<VarId>) {
        match self {
            Self::True | Self::False => {}
            Self::Bool(id) => out.push(*id),
            Self::Not(inner) => inner.collect_variables(out),
            Self::And(parts) | Self::Or(parts) => {
                for part in parts {
                    part.collect_variables(out);
                }
            }
            Self::Implies(a, b) => {
                a.collect_variables(out);
                b.collect_variables(out);
            }
            Self::Compare { left, right, .. } => {
                out.extend(left.variables());
                out.extend(right.variables());
            }
        }
    }

    /// Render this formula in SMT-LIB2 prefix form.
    pub fn render(&self, registry: &VarRegistry, out: &mut String) {
        let name_of = |id: VarId| registry.name_of(id);
        self.render_with(&name_of, out);
    }

    /// Render with an explicit name resolver — the seam tests use to
    /// render without building a registry.
    pub fn render_with(&self, name_of: &dyn Fn(VarId) -> String, out: &mut String) {
        match self {
            Self::True => out.push_str("true"),
            Self::False => out.push_str("false"),
            Self::Bool(id) => out.push_str(&name_of(*id)),
            Self::Not(inner) => {
                out.push_str("(not ");
                inner.render_with(name_of, out);
                out.push(')');
            }
            Self::And(parts) => render_nary("and", parts, name_of, out),
            Self::Or(parts) => render_nary("or", parts, name_of, out),
            Self::Implies(a, b) => {
                out.push_str("(=> ");
                a.render_with(name_of, out);
                out.push(' ');
                b.render_with(name_of, out);
                out.push(')');
            }
            Self::Compare { op, left, right } => {
                out.push('(');
                out.push_str(op.smtlib_op());
                out.push(' ');
                left.render(name_of, out);
                out.push(' ');
                right.render(name_of, out);
                out.push(')');
            }
        }
    }

    /// Evaluate this formula under a total assignment.
    ///
    /// `value_of` returns the integer value of an integer variable, or `0`
    /// / `1` for a boolean one. Returns `None` when the assignment does
    /// not cover a variable the formula mentions — an incomplete model is
    /// reported, never silently defaulted.
    pub fn evaluate(&self, value_of: &dyn Fn(VarId) -> Option<i128>) -> Option<bool> {
        match self {
            Self::True => Some(true),
            Self::False => Some(false),
            Self::Bool(id) => value_of(*id).map(|value| value != 0),
            Self::Not(inner) => inner.evaluate(value_of).map(|value| !value),
            Self::And(parts) => {
                let mut result = true;
                for part in parts {
                    result &= part.evaluate(value_of)?;
                }
                Some(result)
            }
            Self::Or(parts) => {
                let mut result = false;
                for part in parts {
                    result |= part.evaluate(value_of)?;
                }
                Some(result)
            }
            Self::Implies(a, b) => Some(!a.evaluate(value_of)? || b.evaluate(value_of)?),
            Self::Compare { op, left, right } => {
                let left = evaluate_term(left, value_of)?;
                let right = evaluate_term(right, value_of)?;
                Some(op.holds(left, right))
            }
        }
    }
}

/// Evaluate a linear term under an assignment.
pub(crate) fn evaluate_term(term: &Term, value_of: &dyn Fn(VarId) -> Option<i128>) -> Option<i128> {
    let mut total = term.constant_part();
    for (id, coefficient) in term.linear_part() {
        let value = value_of(*id)?;
        total = total.checked_add(value.checked_mul(*coefficient)?)?;
    }
    Some(total)
}

fn render_nary(op: &str, parts: &[Formula], name_of: &dyn Fn(VarId) -> String, out: &mut String) {
    out.push('(');
    out.push_str(op);
    for part in parts {
        out.push(' ');
        part.render_with(name_of, out);
    }
    out.push(')');
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn names(id: VarId) -> String {
        format!("v{}", id.index())
    }

    fn render(formula: &Formula) -> String {
        let mut out = String::new();
        formula.render_with(&names, &mut out);
        out
    }

    #[test]
    fn constants_render_and_evaluate() {
        assert_eq!(render(&Formula::True), "true");
        assert_eq!(render(&Formula::False), "false");
        assert_eq!(Formula::True.evaluate(&|_| None), Some(true));
        assert_eq!(Formula::False.evaluate(&|_| None), Some(false));
    }

    #[test]
    fn double_negation_folds() {
        let formula = Formula::bool_var(VarId::from_index(0)).not().not();
        assert_eq!(render(&formula), "v0");
    }

    #[test]
    fn conjunction_drops_trues() {
        let formula = Formula::all([
            Formula::True,
            Formula::bool_var(VarId::from_index(1)),
            Formula::True,
        ]);
        assert_eq!(render(&formula), "v1");
    }

    #[test]
    fn conjunction_collapses_on_false() {
        let formula = Formula::all([Formula::bool_var(VarId::from_index(1)), Formula::False]);
        assert_eq!(formula, Formula::False);
    }

    #[test]
    fn disjunction_drops_falses_and_collapses_on_true() {
        assert_eq!(
            render(&Formula::any([
                Formula::False,
                Formula::bool_var(VarId::from_index(2))
            ])),
            "v2"
        );
        assert_eq!(
            Formula::any([Formula::bool_var(VarId::from_index(2)), Formula::True]),
            Formula::True
        );
        assert_eq!(Formula::any([]), Formula::False);
        assert_eq!(Formula::all([]), Formula::True);
    }

    #[test]
    fn implication_folds_constants() {
        let p = Formula::bool_var(VarId::from_index(0));
        assert_eq!(Formula::False.implies(p.clone()), Formula::True);
        assert_eq!(p.clone().implies(Formula::True), Formula::True);
        assert_eq!(Formula::True.implies(p.clone()), p);
    }

    #[test]
    fn implication_renders_in_prefix_form() {
        let formula = Formula::bool_var(VarId::from_index(0))
            .implies(Formula::bool_var(VarId::from_index(1)));
        assert_eq!(render(&formula), "(=> v0 v1)");
    }

    #[test]
    fn comparisons_render_with_their_operators() {
        let left = Term::var(VarId::from_index(0));
        assert_eq!(render(&Formula::le(left.clone(), 5i128)), "(<= v0 5)");
        assert_eq!(render(&Formula::gt(left.clone(), 0i128)), "(> v0 0)");
        assert_eq!(render(&Formula::eq(left, 3i128)), "(= v0 3)");
    }

    #[test]
    fn nary_rendering_is_stable() {
        let formula = Formula::all([
            Formula::bool_var(VarId::from_index(0)),
            Formula::bool_var(VarId::from_index(1)),
            Formula::bool_var(VarId::from_index(2)),
        ]);
        assert_eq!(render(&formula), "(and v0 v1 v2)");
        assert_eq!(render(&formula), render(&formula.clone()));
    }

    #[test]
    fn evaluation_walks_the_whole_structure() {
        let assignment = |id: VarId| match id.index() {
            0 => Some(1),
            1 => Some(0),
            2 => Some(7),
            _ => None,
        };
        let formula = Formula::all([
            Formula::bool_var(VarId::from_index(0)),
            Formula::bool_var(VarId::from_index(1)).not(),
            Formula::gt(Term::var(VarId::from_index(2)), 5i128),
        ]);
        assert_eq!(formula.evaluate(&assignment), Some(true));
    }

    #[test]
    fn evaluation_reports_missing_assignments() {
        let formula = Formula::bool_var(VarId::from_index(9));
        assert_eq!(formula.evaluate(&|_| None), None);
    }

    #[test]
    fn evaluation_respects_linear_coefficients() {
        let term = Term::var(VarId::from_index(0))
            .scaled(3)
            .plus(Term::constant(2));
        let formula = Formula::eq(term, 11i128);
        assert_eq!(formula.evaluate(&|_| Some(3)), Some(true));
        assert_eq!(formula.evaluate(&|_| Some(4)), Some(false));
    }

    #[test]
    fn variables_are_collected_from_every_arm() {
        let formula = Formula::all([
            Formula::bool_var(VarId::from_index(0)),
            Formula::bool_var(VarId::from_index(1))
                .not()
                .implies(Formula::any([Formula::bool_var(VarId::from_index(2))])),
            Formula::le(
                Term::var(VarId::from_index(3)),
                Term::var(VarId::from_index(4)),
            ),
        ]);
        let mut seen: Vec<usize> = formula.variables().into_iter().map(VarId::index).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn compare_holds_matches_its_operator() {
        assert!(Compare::Ne.holds(1, 2));
        assert!(!Compare::Ne.holds(2, 2));
        assert!(Compare::Ge.holds(2, 2));
        assert!(!Compare::Lt.holds(2, 2));
        assert_eq!(Compare::Ne.smtlib_op(), "distinct");
    }
}
