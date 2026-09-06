//! Linear integer terms.
//!
//! Deliberately *linear*: a [`Term`] is a sum of integer multiples of
//! variables plus a constant, and nothing else. That is not a shortcut —
//! it is the guarantee that every obligation this crate builds lands in
//! QF_LIA, the fragment [`crate::smt`]'s solver decides completely. A
//! non-linear encoding would let a `Discharge::Inconclusive` verdict creep
//! into obligations that ought to be decidable, which is exactly the
//! failure mode [`crate::error`] argues against.
//!
//! Multiplication by a *variable* is therefore not representable, by
//! construction rather than by review: [`Term::scaled`] takes an `i128`
//! coefficient, and there is no `Term × Term`.

use std::fmt;

use super::var::VarId;

/// One linear integer term: `Σ coefficient·variable + constant`.
///
/// Built through the constructors below rather than by hand so the
/// normal form (sorted, coefficient-merged, zero-coefficient-free) is an
/// invariant of the type. That normal form is what makes two structurally
/// equal terms render identically, which the crate's determinism tests
/// depend on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Term {
    /// Variable terms, sorted by [`VarId`], with no zero coefficients and
    /// no repeated variables.
    linear: Vec<(VarId, i128)>,
    /// The additive constant.
    constant: i128,
}

impl Term {
    /// The constant zero.
    #[must_use]
    pub fn zero() -> Self {
        Self::default()
    }

    /// A literal integer.
    #[must_use]
    pub fn constant(value: i128) -> Self {
        Self {
            linear: Vec::new(),
            constant: value,
        }
    }

    /// A single variable, with coefficient one.
    #[must_use]
    pub fn var(id: VarId) -> Self {
        Self {
            linear: vec![(id, 1)],
            constant: 0,
        }
    }

    /// This term multiplied by an integer constant.
    ///
    /// A zero coefficient collapses the term to the constant zero, which
    /// is how [`Term`]'s no-zero-coefficient invariant survives scaling.
    #[must_use]
    pub fn scaled(mut self, coefficient: i128) -> Self {
        if coefficient == 0 {
            return Self::zero();
        }
        for entry in &mut self.linear {
            entry.1 = entry.1.saturating_mul(coefficient);
        }
        self.constant = self.constant.saturating_mul(coefficient);
        self
    }

    /// The sum of `self` and `other`, merging coefficients on shared
    /// variables and dropping any that cancel to zero.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        let mut linear = self.linear;
        for (id, coefficient) in other.linear {
            match linear.binary_search_by_key(&id, |entry| entry.0) {
                Ok(at) => {
                    linear[at].1 = linear[at].1.saturating_add(coefficient);
                    if linear[at].1 == 0 {
                        linear.remove(at);
                    }
                }
                Err(at) => linear.insert(at, (id, coefficient)),
            }
        }
        Self {
            linear,
            constant: self.constant.saturating_add(other.constant),
        }
    }

    /// The difference `self - other`.
    #[must_use]
    pub fn minus(self, other: Self) -> Self {
        self.plus(other.scaled(-1))
    }

    /// The sum of every term in `terms`.
    #[must_use]
    pub fn sum(terms: impl IntoIterator<Item = Self>) -> Self {
        terms.into_iter().fold(Self::zero(), Self::plus)
    }

    /// This term's additive constant.
    #[must_use]
    pub const fn constant_part(&self) -> i128 {
        self.constant
    }

    /// This term's variable part, sorted by [`VarId`].
    #[must_use]
    pub fn linear_part(&self) -> &[(VarId, i128)] {
        &self.linear
    }

    /// Whether this term mentions no variables.
    #[must_use]
    pub fn is_constant(&self) -> bool {
        self.linear.is_empty()
    }

    /// Every variable this term mentions, in ascending [`VarId`] order.
    pub fn variables(&self) -> impl Iterator<Item = VarId> + '_ {
        self.linear.iter().map(|entry| entry.0)
    }

    /// Render this term in SMT-LIB2 prefix form, resolving variable names
    /// through `name_of`.
    ///
    /// Used for the crate's textual proof-obligation dump (which is what
    /// the determinism test compares) and for `Debug`-grade diagnostics;
    /// the solver backend builds terms structurally rather than by
    /// re-parsing this.
    pub fn render(&self, name_of: &dyn Fn(VarId) -> String, out: &mut String) {
        let pieces = self.linear.len() + usize::from(self.constant != 0 || self.linear.is_empty());
        if pieces > 1 {
            out.push_str("(+ ");
        }
        for (index, (id, coefficient)) in self.linear.iter().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            if *coefficient == 1 {
                out.push_str(&name_of(*id));
            } else {
                out.push_str("(* ");
                render_int(*coefficient, out);
                out.push(' ');
                out.push_str(&name_of(*id));
                out.push(')');
            }
        }
        if self.constant != 0 || self.linear.is_empty() {
            if !self.linear.is_empty() {
                out.push(' ');
            }
            render_int(self.constant, out);
        }
        if pieces > 1 {
            out.push(')');
        }
    }
}

/// Render an integer in SMT-LIB2 form, where negatives are `(- n)`.
pub(crate) fn render_int(value: i128, out: &mut String) {
    use fmt::Write as _;
    if value < 0 {
        // `-i128::MIN` overflows; the encodings never build a coefficient
        // anywhere near it, but formatting must stay total regardless.
        out.push_str("(- ");
        let _ = write!(out, "{}", value.unsigned_abs());
        out.push(')');
    } else {
        let _ = write!(out, "{value}");
    }
}

impl From<i128> for Term {
    fn from(value: i128) -> Self {
        Self::constant(value)
    }
}

impl From<VarId> for Term {
    fn from(id: VarId) -> Self {
        Self::var(id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn names(id: VarId) -> String {
        format!("v{}", id.index())
    }

    fn render(term: &Term) -> String {
        let mut out = String::new();
        term.render(&names, &mut out);
        out
    }

    #[test]
    fn zero_renders_as_zero() {
        assert_eq!(render(&Term::zero()), "0");
    }

    #[test]
    fn single_variable_renders_bare() {
        assert_eq!(render(&Term::var(VarId::from_index(3))), "v3");
    }

    #[test]
    fn coefficients_render_as_products() {
        let term = Term::var(VarId::from_index(1)).scaled(7);
        assert_eq!(render(&term), "(* 7 v1)");
    }

    #[test]
    fn negative_constants_render_in_smtlib_form() {
        assert_eq!(render(&Term::constant(-5)), "(- 5)");
    }

    #[test]
    fn sums_merge_shared_variables() {
        let a = Term::var(VarId::from_index(0));
        let b = Term::var(VarId::from_index(0)).scaled(2);
        let sum = a.plus(b);
        assert_eq!(sum.linear_part(), &[(VarId::from_index(0), 3)]);
    }

    #[test]
    fn cancelling_coefficients_disappear() {
        let a = Term::var(VarId::from_index(4));
        let b = Term::var(VarId::from_index(4)).scaled(-1);
        let sum = a.plus(b);
        assert!(sum.is_constant());
        assert_eq!(render(&sum), "0");
    }

    #[test]
    fn scaling_by_zero_collapses() {
        let term = Term::var(VarId::from_index(2)).plus(Term::constant(9));
        assert_eq!(term.scaled(0), Term::zero());
    }

    #[test]
    fn variables_come_back_sorted() {
        let term = Term::sum([
            Term::var(VarId::from_index(5)),
            Term::var(VarId::from_index(1)),
            Term::var(VarId::from_index(3)),
        ]);
        let seen: Vec<usize> = term.variables().map(VarId::index).collect();
        assert_eq!(seen, vec![1, 3, 5]);
    }

    #[test]
    fn addition_is_order_independent_in_rendering() {
        let left = Term::sum([
            Term::var(VarId::from_index(2)),
            Term::var(VarId::from_index(0)),
        ]);
        let right = Term::sum([
            Term::var(VarId::from_index(0)),
            Term::var(VarId::from_index(2)),
        ]);
        assert_eq!(render(&left), render(&right));
        assert_eq!(render(&left), "(+ v0 v2)");
    }

    #[test]
    fn minus_subtracts_constants_and_variables() {
        let term = Term::var(VarId::from_index(1))
            .plus(Term::constant(10))
            .minus(Term::constant(4));
        assert_eq!(term.constant_part(), 6);
        assert_eq!(render(&term), "(+ v1 6)");
    }

    #[test]
    fn conversions_are_available() {
        assert_eq!(Term::from(7i128), Term::constant(7));
        assert_eq!(
            Term::from(VarId::from_index(2)),
            Term::var(VarId::from_index(2))
        );
    }
}
