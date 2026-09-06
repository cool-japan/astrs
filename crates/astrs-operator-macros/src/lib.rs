//! Procedural macros for the AstRS operator API.
//!
//! Companion crate to `astrs-operator-api` (blueprint §9.3). Hosts the
//! derive and attribute macros that generate operator registration glue,
//! typed input/output accessors and the schema wiring an operator would
//! otherwise write by hand:
//!
//! - `#[derive(AstrsMessage)]` (blueprint §9.1, §9.2): maps a struct's
//!   fields onto the closed columnar type set and implements
//!   `astrs_data::AstrsMessage` — the trait itself lives in `astrs-data`,
//!   not here (see that crate's `message` module docs for why: it is a
//!   Layer 1 crate every future implementor, including `astrs-node-api` and
//!   `astrs-idl`'s ROS 2 codegen, already depends on; this derive is one
//!   consumer among several, not the trait's owner).
//! - `#[astrs::operator]`: optional sugar adding a ready-made registry
//!   entry to an `astrs_operator_api::Operator` impl.
//!
//! # Why this crate has no dependency on `astrs-data`
//!
//! A proc-macro crate emits *references* to paths (`::astrs_data::...`)
//! that resolve in the *invoking* crate's own dependency graph — it does
//! not need to depend on the crate whose types it names, any more than
//! `serde_derive` depends on `serde`'s data types. This crate's `[dev-
//! dependencies]` do include `astrs-data` (and, for the expansion tests
//! below, this crate itself), because its own tests exercise the derive by
//! actually invoking it.
//!
//! # Testing strategy
//!
//! Trybuild-style negative compile tests are unavailable here without
//! adding a dependency this workspace's retained list (blueprint §18.1)
//! does not include. Instead:
//!
//! - `shape`, `urn_check` and `attr` (private modules) factor every
//!   parsing/validation rule into plain functions over `syn`/`proc_macro2`
//!   values, unit-tested directly in each module — including every
//!   rejection path a real negative compile test would otherwise cover (a
//!   unit struct, a tuple struct, `Vec<NestedType>`, `Option<Option<_>>`, a
//!   malformed URN, and so on).
//! - `tests/derive_expansion.rs` is the positive path: real
//!   `#[derive(AstrsMessage)]` uses on six representative structs (scalars
//!   plus a nullable field, a nested message, `Vec`-of-scalar and
//!   `Vec`-of-fixed-array per blueprint §9.1's own `Detections` example,
//!   bare fixed arrays, and a `String`/`Vec<String>`/`bool` struct), each
//!   round-tripped through `to_record_batch`/`from_record_batch` and, where
//!   the target URN is one of `astrs-data`'s registered `std` types,
//!   cross-checked against `astrs_data::urn::layout_of_str` — the
//!   "compile-time layout assertion" the blueprint asks for realized as a
//!   generated-code-exercising test, since the registry it must agree with
//!   is a runtime data structure and cannot be consulted from inside a
//!   `#[proc_macro_derive]`. The genuinely compile-time half — the one a
//!   `#[proc_macro_derive]` really can enforce — is `urn_check`:
//!   `#[astrs(urn = "...")]`'s literal text is checked against the URN
//!   grammar's shape at macro-expansion time, so a malformed URN fails the
//!   derive itself with a real `compile_error!`, no `cargo test` required.

mod attr;
mod codegen;
mod crate_path;
mod message_derive;
mod operator_attr;
mod shape;
mod urn_check;

use proc_macro::TokenStream;

/// Derives `astrs_data::AstrsMessage` for a struct (blueprint §9.1, §9.2).
///
/// ```
/// # #[allow(dead_code)]
/// use astrs_operator_macros::AstrsMessage;
///
/// #[derive(AstrsMessage)]
/// #[astrs(urn = "std/vision/v1/Detections")]
/// struct Detections {
///     boxes: Vec<[f32; 4]>,
///     scores: Vec<f32>,
///     labels: Vec<u32>,
/// }
/// ```
///
/// See the [crate docs](self) for the supported field grammar and this
/// crate's testing strategy.
///
/// # Which crate the generated code names
///
/// The expansion implements a trait that lives in `astrs-data`, so it has to
/// name that crate — and the name it must use depends on the *invoking*
/// crate's own dependency list, because Rust only puts **direct**
/// dependencies in the extern prelude. A crate whose entire `[dependencies]`
/// section is `astrs = "0.1"` cannot name `astrs_data` at all, however
/// faithfully the facade re-exports it.
///
/// The macro therefore resolves the path at expansion time from the invoking
/// crate's `Cargo.toml`: a direct `astrs-data` dependency (under whatever
/// name that manifest gives it) wins; otherwise a direct `astrs` dependency
/// is reached through the facade's re-export chain; otherwise `::astrs_data`
/// is assumed. All three are correct for the crate they describe, and no
/// single hardcoded path is correct for more than one of them.
///
/// # `#[astrs(crate = "...")]`
///
/// The escape hatch, for a build the manifest lookup cannot see through — a
/// vendored tree, a non-cargo build system, or a crate that re-exports this
/// derive under its own name. Its value is the path at which **`astrs-data`
/// itself** is reachable, exactly as `serde`'s `#[serde(crate = "...")]`
/// names `serde`:
///
/// ```
/// # #[allow(dead_code)]
/// use astrs_operator_macros::AstrsMessage;
///
/// #[derive(AstrsMessage)]
/// #[astrs(urn = "std/test/v1/Reading", crate = "::astrs_data")]
/// struct Reading {
///     values: Vec<f64>,
/// }
/// ```
///
/// Through the facade, that path is `::astrs::__private::data` — which is
/// what the automatic resolution already emits, so the override is only ever
/// needed when the automatic answer is unavailable.
#[proc_macro_derive(AstrsMessage, attributes(astrs))]
pub fn derive_astrs_message(input: TokenStream) -> TokenStream {
    let input = match syn::parse::<syn::DeriveInput>(input) {
        Ok(input) => input,
        Err(err) => return err.to_compile_error().into(),
    };
    match message_derive::expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Optional sugar adding a ready-made `astrs_operator_api::OperatorRegistry`
/// entry to an `astrs_operator_api::Operator` impl (blueprint §9.3).
///
/// ```ignore
/// // `ignore`d: exercising this for real needs `astrs-operator-api` as a
/// // dependency, and this crate cannot add one — `astrs-operator-api`
/// // already depends on `astrs-operator-macros` for the derive above, so
/// // the reverse dependency would be circular. The real, compiled,
/// // end-to-end exercise of this macro lives in `astrs-operator-api`'s own
/// // test suite instead, where both crates are legitimately available;
/// // this crate's own tests check the expansion at the token level (see
/// // `operator_attr`'s `#[cfg(test)]` module).
/// use astrs_operator_macros::operator;
///
/// #[operator]
/// #[derive(Default)]
/// struct Crop;
/// ```
///
/// gives `Crop` an inherent `operator_entry()` returning `(&'static str,
/// astrs_operator_api::OperatorConstructor)` — the same shape
/// `astrs_operator_api::register_operator!(Crop)` produces, so either can
/// feed `OperatorRegistry::from_entries`. See the `operator_attr` module
/// docs for the default name it picks and how to override it.
#[proc_macro_attribute]
pub fn operator(attr: TokenStream, item: TokenStream) -> TokenStream {
    match operator_attr::expand(attr.into(), item.into()) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}
