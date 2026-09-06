//! [`OperatorRegistry`] and [`register_operator!`](crate::register_operator) — the static
//! registration table `astrs-runtime` iterates to build the operators a
//! manifest's `operators:` node names (blueprint §9.3).
//!
//! Operators compile into the runtime binary; this table is not populated
//! by `dlopen` (the `dylib` feature's [`crate::export_dylib_operator`] is a
//! separate, parallel path — see that macro's docs — with its own loader
//! in `astrs-runtime`'s `dylib-operators` feature, never funneled through
//! this registry), so "static" here means "fixed once the table is built",
//! not "collected by the linker" — this crate adds no dependency capable of
//! that (`linkme`/`ctor` are not on the retained list, blueprint §18.1),
//! and a portable, Pure Rust table the caller assembles explicitly is both
//! simpler and dependency-free. `register_operator!` is the ergonomic
//! front-end for one entry; `OperatorRegistry::from_entries` builds the
//! whole table from a list of them.

use std::collections::BTreeMap;

use crate::error::{OpError, OpResult};
use crate::operator::Operator;

/// A registered operator's constructor.
///
/// `Send + Sync` so one [`OperatorRegistry`] can be shared across the
/// per-operator threads blueprint §9.3 describes.
pub type OperatorConstructor = Box<dyn Fn() -> Box<dyn Operator> + Send + Sync + 'static>;

/// The static registration table `astrs-runtime` iterates by name.
///
/// ```
/// use astrs_operator_api::{register_operator, OperatorRegistry, OpEvent, OpOutput, OpResult, Operator, Status};
///
/// #[derive(Default)]
/// struct Noop;
///
/// impl Operator for Noop {
///     fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
///         Ok(Status::Continue)
///     }
/// }
///
/// let registry = OperatorRegistry::from_entries([register_operator!(Noop)])?;
/// assert!(registry.contains("Noop"));
/// let operator = registry.build("Noop")?;
/// drop(operator);
/// # Ok::<(), astrs_operator_api::OpError>(())
/// ```
#[derive(Default)]
pub struct OperatorRegistry {
    entries: BTreeMap<&'static str, OperatorConstructor>,
}

impl OperatorRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Builds a registry from a list of `(name, constructor)` entries —
    /// exactly the shape [`register_operator!`](crate::register_operator) produces.
    ///
    /// # Errors
    ///
    /// [`OpError::DuplicateOperator`] if two entries share a name.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (&'static str, OperatorConstructor)>,
    ) -> OpResult<Self> {
        let mut registry = Self::new();
        for (name, constructor) in entries {
            registry.register(name, constructor)?;
        }
        Ok(registry)
    }

    /// Adds one entry.
    ///
    /// # Errors
    ///
    /// [`OpError::DuplicateOperator`] if `name` is already registered.
    pub fn register(
        &mut self,
        name: &'static str,
        constructor: OperatorConstructor,
    ) -> OpResult<()> {
        if self.entries.contains_key(name) {
            return Err(OpError::DuplicateOperator {
                name: name.to_owned(),
            });
        }
        self.entries.insert(name, constructor);
        Ok(())
    }

    /// Whether `name` has a registered constructor.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Every registered name, in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.keys().copied()
    }

    /// Number of registered entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Constructs the operator registered as `name`.
    ///
    /// # Errors
    ///
    /// [`OpError::UnknownOperator`] when nothing is registered under that
    /// name.
    pub fn build(&self, name: &str) -> OpResult<Box<dyn Operator>> {
        let constructor = self
            .entries
            .get(name)
            .ok_or_else(|| OpError::UnknownOperator {
                name: name.to_owned(),
            })?;
        Ok(constructor())
    }
}

/// Builds one `(name, constructor)` registry entry for `$ty`.
///
/// Two forms:
///
/// - `register_operator!(MyOp)` — the registered name is `stringify!(MyOp)`
///   (the type's own written name; for a qualified path this includes the
///   path, spaces and all — use the explicit-name form for those).
/// - `register_operator!("my-op" => MyOp)` — an explicit name, for a
///   qualified path or a name the manifest's `operators:` block expects
///   that differs from the Rust type name.
///
/// `$ty` must implement both [`Operator`](crate::Operator) and
/// [`Default`] — the compile error if it does not is the enforcement point
/// this crate uses in place of the blueprint's non-object-safe `Operator:
/// Default + Send` (see [`Operator`](crate::Operator)'s own docs).
///
/// ```
/// use astrs_operator_api::{register_operator, Operator, OpEvent, OpOutput, OpResult, Status};
///
/// #[derive(Default)]
/// struct Crop;
///
/// impl Operator for Crop {
///     fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
///         Ok(Status::Continue)
///     }
/// }
///
/// let (name, ctor) = register_operator!(Crop);
/// assert_eq!(name, "Crop");
/// let operator = ctor();
/// drop(operator);
///
/// let (name, _ctor) = register_operator!("crop-v2" => Crop);
/// assert_eq!(name, "crop-v2");
/// ```
#[macro_export]
macro_rules! register_operator {
    ($ty:ty) => {
        (
            ::core::stringify!($ty),
            (Box::new(|| -> ::std::boxed::Box<dyn $crate::Operator> {
                ::std::boxed::Box::new(<$ty as ::core::default::Default>::default())
            }) as $crate::OperatorConstructor),
        )
    };
    ($name:expr => $ty:ty) => {
        (
            $name,
            (Box::new(|| -> ::std::boxed::Box<dyn $crate::Operator> {
                ::std::boxed::Box::new(<$ty as ::core::default::Default>::default())
            }) as $crate::OperatorConstructor),
        )
    };
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::event::OpEvent;
    use crate::operator::Status;
    use crate::output::OpOutput;

    #[derive(Default)]
    struct Alpha;
    impl Operator for Alpha {
        fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            Ok(Status::Continue)
        }
    }

    #[derive(Default)]
    struct Beta;
    impl Operator for Beta {
        fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            Ok(Status::Finished)
        }
    }

    #[test]
    fn register_operator_produces_the_stringified_name() {
        let (name, ctor) = register_operator!(Alpha);
        assert_eq!(name, "Alpha");
        let mut op = ctor();
        let mut out = OpOutput::new();
        let status = op.on_event(&OpEvent::Reload, &mut out).unwrap();
        assert_eq!(status, Status::Continue);
    }

    #[test]
    fn register_operator_accepts_an_explicit_name() {
        let (name, _ctor) = register_operator!("beta-v1" => Beta);
        assert_eq!(name, "beta-v1");
    }

    #[test]
    fn registry_lookup_and_registration() {
        let mut registry = OperatorRegistry::new();
        assert!(registry.is_empty());

        let (name, ctor) = register_operator!(Alpha);
        registry.register(name, ctor).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.contains("Alpha"));
        assert!(!registry.contains("Beta"));

        let mut op = registry.build("Alpha").unwrap();
        let mut out = OpOutput::new();
        assert_eq!(
            op.on_event(&OpEvent::Reload, &mut out).unwrap(),
            Status::Continue
        );
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut registry = OperatorRegistry::new();
        let (name, ctor) = register_operator!(Alpha);
        registry.register(name, ctor).unwrap();
        let (name2, ctor2) = register_operator!(Alpha);
        let err = registry.register(name2, ctor2).unwrap_err();
        match err {
            OpError::DuplicateOperator { name } => assert_eq!(name, "Alpha"),
            other => panic!("expected DuplicateOperator, got {other:?}"),
        }
    }

    #[test]
    fn unknown_operator_is_reported() {
        let registry = OperatorRegistry::new();
        // `Box<dyn Operator>` is not `Debug` (it is a trait-object closure
        // product), so this checks the error branch by hand rather than
        // through `unwrap_err`, which needs the `Ok` side to be `Debug`.
        match registry.build("nope") {
            Err(OpError::UnknownOperator { name }) => assert_eq!(name, "nope"),
            other => panic!("expected Err(UnknownOperator), got {}", other.is_ok()),
        }
    }

    #[test]
    fn from_entries_builds_a_multi_operator_table() {
        let registry =
            OperatorRegistry::from_entries([register_operator!(Alpha), register_operator!(Beta)])
                .unwrap();
        assert_eq!(registry.len(), 2);
        let names: Vec<&str> = registry.names().collect();
        assert_eq!(names, ["Alpha", "Beta"]);

        let mut out = OpOutput::new();
        let mut beta = registry.build("Beta").unwrap();
        assert_eq!(
            beta.on_event(&OpEvent::Reload, &mut out).unwrap(),
            Status::Finished
        );
    }

    #[test]
    fn from_entries_rejects_duplicate_names_from_the_same_type() {
        // `OperatorRegistry` holds `Box<dyn Fn(..) -> ..>` constructors,
        // which are not `Debug`, so this checks the error branch by hand
        // rather than through `unwrap_err`.
        match OperatorRegistry::from_entries([register_operator!(Alpha), register_operator!(Alpha)])
        {
            Err(OpError::DuplicateOperator { name }) => assert_eq!(name, "Alpha"),
            Ok(_) => panic!("expected a duplicate-name error"),
            Err(other) => panic!(
                "expected DuplicateOperator, got a different OpError variant instead: {other}"
            ),
        }
    }
}
