//! Variable identity, sorts, and the ordered registry behind them.
//!
//! # Why the registry is a `Vec`, not a map
//!
//! Every variable in a constraint system is allocated once, in the order
//! the encoding creates it, and is thereafter referred to by a dense
//! [`VarId`]. Two properties follow, and both are load-bearing:
//!
//! - **Readback is ordered.** Reading a solver model means walking
//!   `0..len` and asking for each variable's value — never iterating the
//!   solver's own model map, whose iteration order is a hash order and
//!   would make counterexample text differ between runs on the same
//!   input. See [`crate::smt`], which is written against this contract.
//! - **Rendering is stable.** The textual dump of a system is a function
//!   of the encoding's own allocation order, so the same manifest always
//!   produces the same obligation text.
//!
//! A `name → VarId` side index exists for the encoders' convenience (they
//! look variables up by structured name), but it never drives iteration.

use std::collections::BTreeMap;
use std::fmt;

/// The sort of a constraint-system variable.
///
/// Only two, and deliberately: [`Sort::Int`] carries every count, duration
/// and rate on the crate's integer scales ([`crate::scale`]), and
/// [`Sort::Bool`] carries the structural choices (is this channel in the
/// siphon, is this node blocked). Reals are absent on purpose — see
/// [`crate::scale`]'s header.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    /// An unbounded mathematical integer.
    Int,
    /// A boolean.
    Bool,
}

impl Sort {
    /// This sort's SMT-LIB2 name.
    #[must_use]
    pub const fn smtlib_name(self) -> &'static str {
        match self {
            Self::Int => "Int",
            Self::Bool => "Bool",
        }
    }
}

impl fmt::Display for Sort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.smtlib_name())
    }
}

/// A dense index into a [`VarRegistry`].
///
/// Comparable and ordered so [`crate::constraint::Term`] can keep its
/// linear part sorted; the order is allocation order, which is the
/// encoding's own deterministic order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct VarId(usize);

impl VarId {
    /// Build an id from a raw index.
    ///
    /// Only meaningful against the registry that allocated it; exposed for
    /// tests and for backends that keep a parallel `Vec` of solver terms.
    #[must_use]
    pub const fn from_index(index: usize) -> Self {
        Self(index)
    }

    /// This id's raw index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

impl fmt::Display for VarId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

/// One declared variable: its solver-facing name, its sort, and the human
/// label a counterexample shows for it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VarDecl {
    /// The variable's unique name inside its system, e.g.
    /// `fire!detector` or `siphon!detector.frames`.
    ///
    /// Structured rather than opaque so a raw SMT-LIB2 dump of the system
    /// stays readable when someone is debugging an encoding.
    pub name: String,
    /// The variable's sort.
    pub sort: Sort,
    /// What this variable means, phrased for a report reader — e.g.
    /// "times `detector` fires per 1s window".
    pub label: String,
}

/// The ordered set of variables in one constraint system.
///
/// See this module's header for why the ordering matters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VarRegistry {
    declarations: Vec<VarDecl>,
    by_name: BTreeMap<String, VarId>,
}

impl VarRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a variable, or return the existing id if `name` was already
    /// declared.
    ///
    /// Idempotent on the name so encoders can reference a variable from
    /// several constraints without threading ids around. A repeat
    /// declaration keeps the *first* sort and label: the encodings never
    /// redeclare a name with a different sort, and silently rewriting one
    /// would turn an encoding bug into a wrong answer instead of a
    /// visible inconsistency.
    pub fn declare(
        &mut self,
        name: impl Into<String>,
        sort: Sort,
        label: impl Into<String>,
    ) -> VarId {
        let name = name.into();
        if let Some(&existing) = self.by_name.get(&name) {
            return existing;
        }
        let id = VarId(self.declarations.len());
        self.declarations.push(VarDecl {
            name: name.clone(),
            sort,
            label: label.into(),
        });
        self.by_name.insert(name, id);
        id
    }

    /// Look up a previously declared variable by name.
    #[must_use]
    pub fn id_of(&self, name: &str) -> Option<VarId> {
        self.by_name.get(name).copied()
    }

    /// One variable's declaration.
    #[must_use]
    pub fn get(&self, id: VarId) -> Option<&VarDecl> {
        self.declarations.get(id.0)
    }

    /// One variable's solver-facing name, or a placeholder if `id` did not
    /// come from this registry.
    ///
    /// Total rather than fallible because it is called from rendering
    /// paths where an unknown id is a bug worth *seeing* in the output
    /// rather than a reason to stop rendering.
    #[must_use]
    pub fn name_of(&self, id: VarId) -> String {
        self.declarations
            .get(id.0)
            .map_or_else(|| format!("<unknown{}>", id.0), |decl| decl.name.clone())
    }

    /// How many variables are declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.declarations.len()
    }

    /// Whether no variables are declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }

    /// Every declaration, in allocation order, paired with its id.
    pub fn iter(&self) -> impl Iterator<Item = (VarId, &VarDecl)> {
        self.declarations
            .iter()
            .enumerate()
            .map(|(index, decl)| (VarId(index), decl))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn declaration_allocates_dense_ids() {
        let mut registry = VarRegistry::new();
        let a = registry.declare("a", Sort::Int, "a");
        let b = registry.declare("b", Sort::Bool, "b");
        assert_eq!(a.index(), 0);
        assert_eq!(b.index(), 1);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn redeclaring_a_name_returns_the_same_id() {
        let mut registry = VarRegistry::new();
        let first = registry.declare("fire!camera", Sort::Int, "camera firings");
        let again = registry.declare("fire!camera", Sort::Int, "something else");
        assert_eq!(first, again);
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.get(first).expect("declared").label,
            "camera firings",
            "the first label wins"
        );
    }

    #[test]
    fn lookup_by_name_round_trips() {
        let mut registry = VarRegistry::new();
        let id = registry.declare("siphon!a.b", Sort::Bool, "channel a.b is starved");
        assert_eq!(registry.id_of("siphon!a.b"), Some(id));
        assert_eq!(registry.id_of("missing"), None);
    }

    #[test]
    fn iteration_is_allocation_order() {
        let mut registry = VarRegistry::new();
        for name in ["z", "m", "a"] {
            registry.declare(name, Sort::Int, name);
        }
        let order: Vec<&str> = registry.iter().map(|(_, d)| d.name.as_str()).collect();
        assert_eq!(order, vec!["z", "m", "a"]);
    }

    #[test]
    fn name_of_is_total() {
        let registry = VarRegistry::new();
        assert_eq!(registry.name_of(VarId::from_index(9)), "<unknown9>");
    }

    #[test]
    fn empty_registry_reports_empty() {
        let mut registry = VarRegistry::new();
        assert!(registry.is_empty());
        registry.declare("x", Sort::Int, "x");
        assert!(!registry.is_empty());
    }

    #[test]
    fn sorts_render_as_smtlib_names() {
        assert_eq!(Sort::Int.to_string(), "Int");
        assert_eq!(Sort::Bool.to_string(), "Bool");
    }

    #[test]
    fn var_id_displays_with_a_marker() {
        assert_eq!(VarId::from_index(12).to_string(), "@12");
    }
}
