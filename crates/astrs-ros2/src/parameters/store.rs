//! [`ParameterStore`]: declare, get, set, list, describe — with no I/O.
//!
//! Every rule ROS 2 applies to a parameter lives here, as a synchronous
//! function of (state, request): the type check, the range check, the
//! read-only rule, the atomic-set semantics, the `list` recursion. Nothing
//! in this module touches a socket, so all of it is testable without a
//! runtime — and [`crate::parameters::service`], which does touch sockets,
//! is a thin translation of the six `rcl_interfaces` services onto these
//! calls.
//!
//! # The separator is `.`
//!
//! ROS 2 parameters nest with dots, not slashes:
//! `qos_overrides./scan.publisher.depth` is one parameter whose name has
//! three separators and a slash inside a token.
//! [`ParameterStore::list`] implements `rcl_list_parameters`' recursion over
//! that separator.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{NameKind, Ros2Error, Ros2Result};
use crate::names::validate::validate_parameter_name;
use crate::parameters::descriptor::ParameterDescriptor;
use crate::parameters::value::ParameterValue;

/// The separator ROS 2 parameter names nest with.
pub const SEPARATOR: char = '.';

/// The `depth` value that means "recurse without limit".
pub const DEPTH_RECURSIVE: u64 = 0;

/// What happened to one parameter.
///
/// The three sets `rcl_interfaces/msg/ParameterEvent` carries, as one enum
/// so a caller can react to a batch in order rather than in three passes.
#[derive(Debug, Clone, PartialEq)]
pub enum ParameterChange {
    /// The parameter was declared for the first time.
    Declared {
        /// Its name.
        name: String,
        /// Its initial value.
        value: ParameterValue,
    },
    /// An already-declared parameter's value changed.
    Changed {
        /// Its name.
        name: String,
        /// What it was.
        previous: ParameterValue,
        /// What it is now.
        value: ParameterValue,
    },
    /// The parameter was undeclared.
    Deleted {
        /// Its name.
        name: String,
        /// What it held when it went.
        value: ParameterValue,
    },
}

impl ParameterChange {
    /// The parameter's name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Declared { name, .. }
            | Self::Changed { name, .. }
            | Self::Deleted { name, .. } => name,
        }
    }

    /// The value after the change.
    #[must_use]
    pub const fn value(&self) -> &ParameterValue {
        match self {
            Self::Declared { value, .. }
            | Self::Changed { value, .. }
            | Self::Deleted { value, .. } => value,
        }
    }
}

/// One declared parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    /// The parameter's name.
    pub name: String,
    /// Its current value.
    pub value: ParameterValue,
    /// Everything else declared about it.
    pub descriptor: ParameterDescriptor,
}

/// What [`ParameterStore::list`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListResult {
    /// The parameter names, sorted.
    pub names: Vec<String>,
    /// The prefixes discovered beneath the requested ones, sorted.
    pub prefixes: Vec<String>,
}

/// A node's parameters.
///
/// Synchronous and lock-free: the node wraps it in a `Mutex` and the
/// services translate onto it. Nothing here awaits.
#[derive(Debug, Clone, Default)]
pub struct ParameterStore {
    entries: BTreeMap<String, Parameter>,
    allow_undeclared: bool,
}

impl ParameterStore {
    /// An empty store that refuses undeclared parameters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store whose `set` declares on first use.
    ///
    /// `rclcpp`'s `automatically_declare_parameters_from_overrides`, and what
    /// a tool that pushes parameters into a node it did not write needs.
    #[must_use]
    pub const fn permissive() -> Self {
        Self {
            entries: BTreeMap::new(),
            allow_undeclared: true,
        }
    }

    /// Whether setting an undeclared parameter declares it.
    #[must_use]
    pub const fn allows_undeclared(&self) -> bool {
        self.allow_undeclared
    }

    /// Turn the permissive rule on or off.
    pub const fn set_allow_undeclared(&mut self, allowed: bool) {
        self.allow_undeclared = allowed;
    }

    /// How many parameters are declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when `name` is declared.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Every declared parameter, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = &Parameter> {
        self.entries.values()
    }

    /// Every declared name, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Declare a parameter with a value and a default descriptor.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::ParameterAlreadyDeclared`],
    /// [`Ros2Error::InvalidServiceName`] for a malformed name, or whatever
    /// [`ParameterDescriptor::check`] reports.
    pub fn declare(
        &mut self,
        name: impl Into<String>,
        value: impl Into<ParameterValue>,
    ) -> Ros2Result<ParameterChange> {
        let name = name.into();
        let value = value.into();
        let descriptor = ParameterDescriptor::for_value(name.clone(), &value);
        self.declare_with(name, value, descriptor)
    }

    /// Declare a parameter with an explicit descriptor.
    ///
    /// The descriptor's `name` is overwritten with `name`: two names for one
    /// parameter is a bug waiting to happen, and the argument is the one the
    /// caller meant.
    ///
    /// # Errors
    ///
    /// As [`declare`](Self::declare).
    pub fn declare_with(
        &mut self,
        name: impl Into<String>,
        value: impl Into<ParameterValue>,
        mut descriptor: ParameterDescriptor,
    ) -> Ros2Result<ParameterChange> {
        let name = name.into();
        let value = value.into();
        check_name(&name)?;
        if self.entries.contains_key(&name) {
            return Err(Ros2Error::ParameterAlreadyDeclared { name });
        }
        descriptor.name.clone_from(&name);
        descriptor.check(&value)?;
        self.entries.insert(
            name.clone(),
            Parameter {
                name: name.clone(),
                value: value.clone(),
                descriptor,
            },
        );
        Ok(ParameterChange::Declared { name, value })
    }

    /// Undeclare a parameter.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownParameter`] when it was never declared, and
    /// [`Ros2Error::ParameterReadOnly`] when it is read-only — a read-only
    /// parameter that could be undeclared and re-declared would not be
    /// read-only at all.
    pub fn undeclare(&mut self, name: &str) -> Ros2Result<ParameterChange> {
        let Some(entry) = self.entries.get(name) else {
            return Err(Ros2Error::UnknownParameter {
                name: name.to_owned(),
            });
        };
        if entry.descriptor.read_only {
            return Err(Ros2Error::ParameterReadOnly {
                name: name.to_owned(),
            });
        }
        let removed = self.entries.remove(name).map(|entry| entry.value);
        Ok(ParameterChange::Deleted {
            name: name.to_owned(),
            value: removed.unwrap_or(ParameterValue::NotSet),
        })
    }

    /// Read a parameter.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownParameter`].
    pub fn get(&self, name: &str) -> Ros2Result<&ParameterValue> {
        self.entries
            .get(name)
            .map(|entry| &entry.value)
            .ok_or_else(|| Ros2Error::UnknownParameter {
                name: name.to_owned(),
            })
    }

    /// Read a parameter, or [`ParameterValue::NotSet`] if it is undeclared.
    ///
    /// What `rcl_interfaces/srv/GetParameters` returns for a name it does
    /// not know: the service answers positionally, so it cannot omit one.
    #[must_use]
    pub fn get_or_unset(&self, name: &str) -> ParameterValue {
        self.entries
            .get(name)
            .map_or(ParameterValue::NotSet, |entry| entry.value.clone())
    }

    /// Read a parameter's whole record.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownParameter`].
    pub fn parameter(&self, name: &str) -> Ros2Result<&Parameter> {
        self.entries
            .get(name)
            .ok_or_else(|| Ros2Error::UnknownParameter {
                name: name.to_owned(),
            })
    }

    /// Read a parameter's descriptor, or an empty one if undeclared.
    #[must_use]
    pub fn describe(&self, name: &str) -> ParameterDescriptor {
        self.entries.get(name).map_or_else(
            || ParameterDescriptor::new(name, crate::parameters::value::TYPE_NOT_SET),
            |entry| entry.descriptor.clone(),
        )
    }

    /// Read a parameter's type code, or `NOT_SET` if undeclared.
    #[must_use]
    pub fn type_of(&self, name: &str) -> u8 {
        self.entries
            .get(name)
            .map_or(crate::parameters::value::TYPE_NOT_SET, |entry| {
                entry.value.type_code()
            })
    }

    /// Set one parameter.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownParameter`] (unless the store is permissive),
    /// [`Ros2Error::ParameterReadOnly`], and whatever the descriptor's check
    /// reports.
    pub fn set(
        &mut self,
        name: &str,
        value: impl Into<ParameterValue>,
    ) -> Ros2Result<ParameterChange> {
        let value = value.into();
        let Some(entry) = self.entries.get_mut(name) else {
            if !self.allow_undeclared {
                return Err(Ros2Error::UnknownParameter {
                    name: name.to_owned(),
                });
            }
            return self.declare(name.to_owned(), value);
        };
        if entry.descriptor.read_only {
            return Err(Ros2Error::ParameterReadOnly {
                name: name.to_owned(),
            });
        }
        entry.descriptor.check(&value)?;
        let previous = core::mem::replace(&mut entry.value, value.clone());
        Ok(ParameterChange::Changed {
            name: name.to_owned(),
            previous,
            value,
        })
    }

    /// Set several parameters, each independently.
    ///
    /// Mirrors `rcl_interfaces/srv/SetParameters`: one outcome per
    /// parameter, in order, and a failure does not stop the rest.
    pub fn set_each<I, N, V>(&mut self, updates: I) -> Vec<Ros2Result<ParameterChange>>
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: Into<ParameterValue>,
    {
        updates
            .into_iter()
            .map(|(name, value)| self.set(name.as_ref(), value))
            .collect()
    }

    /// Set several parameters as one transaction.
    ///
    /// Mirrors `rcl_interfaces/srv/SetParametersAtomically`: if any one
    /// fails, none is applied. Implemented by validating against a clone and
    /// swapping it in, which is the only way to be atomic in the presence of
    /// a rule that depends on the *order* updates are applied in (setting a
    /// parameter can declare it, which changes what a later update in the
    /// same batch is checked against).
    ///
    /// # Errors
    ///
    /// The first error any update produces.
    pub fn set_atomically<I, N, V>(&mut self, updates: I) -> Ros2Result<Vec<ParameterChange>>
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: Into<ParameterValue>,
    {
        let mut candidate = self.clone();
        let mut changes = Vec::new();
        for (name, value) in updates {
            changes.push(candidate.set(name.as_ref(), value)?);
        }
        *self = candidate;
        Ok(changes)
    }

    /// List parameter names and the prefixes beneath them.
    ///
    /// `rcl_list_parameters`' recursion:
    ///
    /// - An empty `prefixes` lists from the root.
    /// - `depth` of [`DEPTH_RECURSIVE`] (zero) means no limit; otherwise a
    ///   name is listed when the part of it below the prefix has fewer than
    ///   `depth` separators.
    /// - Every intermediate prefix a listed name passes through is reported
    ///   in [`ListResult::prefixes`], which is what makes
    ///   `ros2 param list` able to print a tree.
    #[must_use]
    pub fn list(&self, prefixes: &[String], depth: u64) -> ListResult {
        let mut names = Vec::new();
        let mut found_prefixes = BTreeSet::new();

        for name in self.entries.keys() {
            let Some(relative) = relative_to(name, prefixes) else {
                continue;
            };
            let separators = relative.matches(SEPARATOR).count() as u64;
            if depth != DEPTH_RECURSIVE && separators >= depth {
                continue;
            }
            names.push(name.clone());
            collect_prefixes(name, &mut found_prefixes);
        }

        ListResult {
            names,
            prefixes: found_prefixes.into_iter().collect(),
        }
    }

    /// Replace the whole store's contents, reporting every change.
    ///
    /// What loading a parameter file does: the caller has a batch, and the
    /// difference from what was there is what `/parameter_events` must
    /// carry.
    ///
    /// # Errors
    ///
    /// As [`set_atomically`](Self::set_atomically).
    pub fn load<I, N, V>(&mut self, parameters: I) -> Ros2Result<Vec<ParameterChange>>
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: Into<ParameterValue>,
    {
        let mut candidate = self.clone();
        candidate.allow_undeclared = true;
        let mut changes = Vec::new();
        for (name, value) in parameters {
            changes.push(candidate.set(name.as_ref(), value)?);
        }
        candidate.allow_undeclared = self.allow_undeclared;
        *self = candidate;
        Ok(changes)
    }
}

/// The part of `name` below whichever of `prefixes` it lives under, or
/// `None` when it lives under none of them.
///
/// An empty `prefixes` list means the root, under which everything lives.
fn relative_to<'a>(name: &'a str, prefixes: &[String]) -> Option<&'a str> {
    if prefixes.is_empty() {
        return Some(name);
    }
    for prefix in prefixes {
        if name == prefix {
            return Some("");
        }
        let with_separator = format!("{prefix}{SEPARATOR}");
        if let Some(rest) = name.strip_prefix(&with_separator) {
            return Some(rest);
        }
    }
    None
}

/// Add every proper prefix of `name` to `into`.
fn collect_prefixes(name: &str, into: &mut BTreeSet<String>) {
    let mut end = 0_usize;
    for (index, character) in name.char_indices() {
        if character == SEPARATOR {
            end = index;
            into.insert(name.get(..end).unwrap_or_default().to_owned());
        }
    }
    let _ = end;
}

/// Reject a malformed parameter name.
fn check_name(name: &str) -> Ros2Result<()> {
    validate_parameter_name(name).map_err(|fault| Ros2Error::InvalidServiceName {
        kind: NameKind::ParameterName,
        name: name.to_owned(),
        fault,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::parameters::descriptor::IntegerRange;
    use crate::parameters::value::{TYPE_INTEGER, TYPE_STRING};

    fn store_with_tree() -> ParameterStore {
        let mut store = ParameterStore::new();
        store.declare("depth", 10_i64).expect("declare");
        store.declare("camera.width", 640_i64).expect("declare");
        store.declare("camera.height", 480_i64).expect("declare");
        store
            .declare("camera.lens.focal_length", 3.5_f64)
            .expect("declare");
        store
    }

    #[test]
    fn a_fresh_store_is_empty_and_strict() {
        let store = ParameterStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.allows_undeclared());
        assert!(!store.has("anything"));
    }

    #[test]
    fn declaring_gives_a_descriptor_matching_the_value() {
        let mut store = ParameterStore::new();
        let change = store.declare("gain", 1.5_f64).expect("declare");
        assert_eq!(change.name(), "gain");
        assert_eq!(change.value(), &ParameterValue::Double(1.5));
        assert_eq!(store.describe("gain").type_name(), "double");
        assert_eq!(store.type_of("gain"), crate::parameters::value::TYPE_DOUBLE);
        assert!(store.has("gain"));
    }

    #[test]
    fn declaring_twice_is_refused() {
        let mut store = ParameterStore::new();
        store.declare("gain", 1.0_f64).expect("declare");
        let error = store.declare("gain", 2.0_f64).expect_err("twice");
        assert!(matches!(error, Ros2Error::ParameterAlreadyDeclared { .. }));
        assert_eq!(
            store.get("gain").expect("still there"),
            &ParameterValue::Double(1.0)
        );
    }

    #[test]
    fn a_malformed_name_is_refused_at_declaration() {
        let mut store = ParameterStore::new();
        let error = store.declare("has space", 1_i64).expect_err("malformed");
        assert!(matches!(
            error,
            Ros2Error::InvalidServiceName {
                kind: NameKind::ParameterName,
                ..
            }
        ));
    }

    #[test]
    fn reading_an_undeclared_parameter_is_an_error_or_an_unset_value() {
        let store = ParameterStore::new();
        assert!(matches!(
            store.get("missing").expect_err("undeclared"),
            Ros2Error::UnknownParameter { .. }
        ));
        assert_eq!(store.get_or_unset("missing"), ParameterValue::NotSet);
        assert_eq!(store.type_of("missing"), 0);
        assert_eq!(store.describe("missing").name, "missing");
    }

    #[test]
    fn setting_an_undeclared_parameter_depends_on_the_policy() {
        let mut strict = ParameterStore::new();
        assert!(matches!(
            strict.set("gain", 1.0_f64).expect_err("undeclared"),
            Ros2Error::UnknownParameter { .. }
        ));

        let mut permissive = ParameterStore::permissive();
        assert!(permissive.allows_undeclared());
        let change = permissive.set("gain", 1.0_f64).expect("declared on demand");
        assert!(matches!(change, ParameterChange::Declared { .. }));
        assert_eq!(permissive.len(), 1);
    }

    #[test]
    fn setting_reports_the_previous_value() {
        let mut store = ParameterStore::new();
        store.declare("gain", 1.0_f64).expect("declare");
        let change = store.set("gain", 2.0_f64).expect("set");
        assert_eq!(
            change,
            ParameterChange::Changed {
                name: "gain".to_owned(),
                previous: ParameterValue::Double(1.0),
                value: ParameterValue::Double(2.0),
            }
        );
    }

    #[test]
    fn a_read_only_parameter_refuses_both_set_and_undeclare() {
        let mut store = ParameterStore::new();
        store
            .declare_with(
                "serial",
                "ABC123",
                ParameterDescriptor::new("serial", TYPE_STRING).read_only(),
            )
            .expect("declare");

        assert!(matches!(
            store.set("serial", "XYZ").expect_err("read-only"),
            Ros2Error::ParameterReadOnly { .. }
        ));
        assert!(matches!(
            store.undeclare("serial").expect_err("read-only"),
            Ros2Error::ParameterReadOnly { .. }
        ));
        assert_eq!(
            store.get("serial").expect("unchanged"),
            &ParameterValue::String("ABC123".to_owned())
        );
    }

    #[test]
    fn setting_the_wrong_type_is_refused() {
        let mut store = ParameterStore::new();
        store.declare("count", 1_i64).expect("declare");
        assert!(matches!(
            store.set("count", "not a number").expect_err("type"),
            Ros2Error::ParameterTypeMismatch { .. }
        ));
    }

    #[test]
    fn a_range_is_enforced_on_every_set() {
        let mut store = ParameterStore::new();
        store
            .declare_with(
                "count",
                5_i64,
                ParameterDescriptor::new("count", TYPE_INTEGER)
                    .with_integer_range(IntegerRange::new(0, 10)),
            )
            .expect("declare");
        assert!(store.set("count", 10_i64).is_ok());
        assert!(matches!(
            store.set("count", 11_i64).expect_err("out of range"),
            Ros2Error::ParameterOutOfRange { .. }
        ));
        assert_eq!(
            store.get("count").expect("unchanged"),
            &ParameterValue::Integer(10)
        );
    }

    #[test]
    fn a_declaration_outside_its_own_range_is_refused() {
        let mut store = ParameterStore::new();
        let error = store
            .declare_with(
                "count",
                99_i64,
                ParameterDescriptor::new("count", TYPE_INTEGER)
                    .with_integer_range(IntegerRange::new(0, 10)),
            )
            .expect_err("out of range at declaration");
        assert!(matches!(error, Ros2Error::ParameterOutOfRange { .. }));
        assert!(store.is_empty(), "nothing was declared");
    }

    #[test]
    fn undeclaring_removes_and_reports_the_last_value() {
        let mut store = ParameterStore::new();
        store.declare("gain", 1.0_f64).expect("declare");
        let change = store.undeclare("gain").expect("undeclare");
        assert_eq!(
            change,
            ParameterChange::Deleted {
                name: "gain".to_owned(),
                value: ParameterValue::Double(1.0),
            }
        );
        assert!(store.is_empty());
        assert!(matches!(
            store.undeclare("gain").expect_err("gone"),
            Ros2Error::UnknownParameter { .. }
        ));
    }

    #[test]
    fn set_each_reports_one_outcome_per_update_and_does_not_stop() {
        let mut store = ParameterStore::new();
        store.declare("a", 1_i64).expect("declare");
        store.declare("b", 2_i64).expect("declare");
        let outcomes = store.set_each([("a", 10_i64), ("missing", 0_i64), ("b", 20_i64)]);
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes[0].is_ok());
        assert!(outcomes[1].is_err());
        assert!(outcomes[2].is_ok(), "a failure does not stop the rest");
        assert_eq!(store.get("b").expect("set"), &ParameterValue::Integer(20));
    }

    #[test]
    fn an_atomic_set_applies_all_or_nothing() {
        let mut store = ParameterStore::new();
        store.declare("a", 1_i64).expect("declare");
        store.declare("b", 2_i64).expect("declare");

        assert!(
            store
                .set_atomically([("a", 10_i64), ("missing", 0_i64)])
                .is_err()
        );
        assert_eq!(
            store.get("a").expect("unchanged"),
            &ParameterValue::Integer(1),
            "the first update was rolled back"
        );

        let changes = store
            .set_atomically([("a", 10_i64), ("b", 20_i64)])
            .expect("both succeed");
        assert_eq!(changes.len(), 2);
        assert_eq!(store.get("a").expect("set"), &ParameterValue::Integer(10));
        assert_eq!(store.get("b").expect("set"), &ParameterValue::Integer(20));
    }

    #[test]
    fn loading_declares_what_is_missing_without_relaxing_the_store() {
        let mut store = ParameterStore::new();
        store.declare("a", 1_i64).expect("declare");
        let changes = store.load([("a", 10_i64), ("b", 2_i64)]).expect("load");
        assert_eq!(changes.len(), 2);
        assert!(matches!(changes[0], ParameterChange::Changed { .. }));
        assert!(matches!(changes[1], ParameterChange::Declared { .. }));
        assert!(
            !store.allows_undeclared(),
            "loading does not leave the store permissive"
        );
        assert!(store.set("c", 1_i64).is_err());
    }

    #[test]
    fn listing_from_the_root_recursively_finds_everything() {
        let store = store_with_tree();
        let result = store.list(&[], DEPTH_RECURSIVE);
        assert_eq!(
            result.names,
            vec![
                "camera.height".to_owned(),
                "camera.lens.focal_length".to_owned(),
                "camera.width".to_owned(),
                "depth".to_owned(),
            ]
        );
        assert_eq!(
            result.prefixes,
            vec!["camera".to_owned(), "camera.lens".to_owned()]
        );
    }

    #[test]
    fn a_depth_of_one_lists_only_the_top_level() {
        let store = store_with_tree();
        let result = store.list(&[], 1);
        assert_eq!(result.names, vec!["depth".to_owned()]);
        assert!(result.prefixes.is_empty());
    }

    #[test]
    fn a_depth_of_two_reaches_one_level_down() {
        let store = store_with_tree();
        let result = store.list(&[], 2);
        assert_eq!(
            result.names,
            vec![
                "camera.height".to_owned(),
                "camera.width".to_owned(),
                "depth".to_owned(),
            ]
        );
        assert_eq!(result.prefixes, vec!["camera".to_owned()]);
    }

    #[test]
    fn a_prefix_restricts_the_listing_to_its_subtree() {
        let store = store_with_tree();
        let result = store.list(&["camera".to_owned()], DEPTH_RECURSIVE);
        assert_eq!(
            result.names,
            vec![
                "camera.height".to_owned(),
                "camera.lens.focal_length".to_owned(),
                "camera.width".to_owned(),
            ]
        );
        assert!(!result.names.contains(&"depth".to_owned()));
    }

    #[test]
    fn a_prefix_and_a_depth_compose() {
        let store = store_with_tree();
        let result = store.list(&["camera".to_owned()], 1);
        assert_eq!(
            result.names,
            vec!["camera.height".to_owned(), "camera.width".to_owned()],
            "the depth is measured below the prefix, not from the root"
        );
    }

    #[test]
    fn a_prefix_that_matches_nothing_lists_nothing() {
        let store = store_with_tree();
        assert_eq!(
            store.list(&["lidar".to_owned()], DEPTH_RECURSIVE),
            ListResult::default()
        );
    }

    #[test]
    fn a_prefix_that_is_itself_a_parameter_lists_it() {
        let mut store = ParameterStore::new();
        store.declare("camera", 1_i64).expect("declare");
        store.declare("camera.width", 2_i64).expect("declare");
        let result = store.list(&["camera".to_owned()], DEPTH_RECURSIVE);
        assert_eq!(
            result.names,
            vec!["camera".to_owned(), "camera.width".to_owned()]
        );
    }

    #[test]
    fn a_qos_override_style_name_survives_every_rule() {
        let mut store = ParameterStore::new();
        let name = "qos_overrides./scan.publisher.depth";
        store
            .declare(name, 10_i64)
            .expect("a slash inside a token is legal");
        assert!(store.has(name));
        let result = store.list(&["qos_overrides".to_owned()], DEPTH_RECURSIVE);
        assert_eq!(result.names, vec![name.to_owned()]);
    }

    #[test]
    fn iteration_is_sorted_by_name() {
        let store = store_with_tree();
        let names: Vec<&str> = store.iter().map(|entry| entry.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(store.names().len(), 4);
    }

    #[test]
    fn the_parameter_record_carries_value_and_descriptor_together() {
        let mut store = ParameterStore::new();
        store
            .declare_with(
                "gain",
                1.0_f64,
                ParameterDescriptor::new("wrong name", crate::parameters::value::TYPE_DOUBLE)
                    .with_description("loop gain"),
            )
            .expect("declare");
        let parameter = store.parameter("gain").expect("declared");
        assert_eq!(parameter.name, "gain");
        assert_eq!(
            parameter.descriptor.name, "gain",
            "the descriptor's name is overwritten with the one the caller meant"
        );
        assert_eq!(parameter.descriptor.description, "loop gain");
    }
}
