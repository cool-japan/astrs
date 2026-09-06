//! Where the generated code's `astrs-data` / `astrs-operator-api` paths
//! come from.
//!
//! # The problem a hardcoded path cannot solve
//!
//! A `#[proc_macro_derive]` emits *text*, and that text has to name the
//! crate whose trait it implements. Emitting `::astrs_data::AstrsMessage`
//! works only if `astrs_data` is in the **extern prelude** of the crate
//! being compiled — and Rust puts a crate there only when it is a *direct*
//! dependency. Transitive dependencies are deliberately not nameable.
//!
//! So for the flagship §9.1 application — one whose entire `[dependencies]`
//! section is `astrs = "0.1"` — every `::astrs_data::…` path the derive
//! emits fails with `error[E0433]: cannot find astrs_data in the crate
//! root`, even though the facade re-exports the very same crate. That is not
//! a documentation gap; it is the one dependency an application adds
//! (blueprint §5.2) failing to be enough.
//!
//! # The resolution, and why it is not a single fixed path
//!
//! There is no one path that serves both callers. A crate that depends on
//! `astrs-data` directly must be handed `::astrs_data`; a crate that depends
//! only on `astrs` must be handed a path *through* the facade. The macro
//! therefore resolves the path at expansion time from the invoking crate's
//! own manifest, via [`proc_macro_crate`], in this order:
//!
//! 1. an explicit `#[astrs(crate = "…")]` on the item — the escape hatch for
//!    a build [`proc_macro_crate`] cannot see through (a vendored re-export,
//!    a non-cargo build system, a crate re-exporting the derive under its own
//!    name);
//! 2. the target crate itself as a direct dependency, under whatever name the
//!    manifest gives it — `astrs-data = { package = "astrs-data", … }` renamed
//!    to `columns` yields `::columns`, which is exactly why this step cannot
//!    be replaced by a constant;
//! 3. the `astrs` facade as a direct dependency, again under its own name,
//!    reaching the target through the facade's `#[doc(hidden)] pub mod
//!    __private` re-export chain;
//! 4. `::astrs_data` / `::astrs_operator_api` as the in-workspace fallback,
//!    for the case where no manifest can be read at all.
//!
//! Steps 2 and 3 also answer `FoundCrate::Itself`, which is what
//! [`proc_macro_crate`] reports when the crate being compiled *is* the crate
//! being looked for — `crate`, then, rather than an absolute path.
//!
//! # Why the emitted path is aliased rather than substituted
//!
//! [`crate::codegen`] builds roughly forty distinct token fragments that name
//! `astrs-data`, and threading a resolved path parameter through every one of
//! them would bury the generator's actual subject under plumbing. Instead
//! [`crate::message_derive`] wraps its whole output in an anonymous
//! `const _: () = { … };` block opening with `use <resolved> as
//! __astrs_data;`, and every fragment names `__astrs_data::…` — one
//! substitution point, and the same trick `serde_derive` uses.
//!
//! That works for the derive because its entire output is one
//! `impl … for Struct` block, which a const block can host without hiding
//! anything from the outside world. It does **not** work for
//! [`crate::operator_attr`], whose output includes a *public inherent method*
//! that callers must be able to see and rustdoc must be able to document; so
//! that macro interpolates its resolved path directly, at its three use
//! sites.

use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Span, TokenStream};
use quote::quote;

/// The crate the `#[derive(AstrsMessage)]` output names: `astrs-data`.
const DATA_CRATE: &str = "astrs-data";

/// The crate the `#[astrs::operator]` output names: `astrs-operator-api`.
const OPERATOR_API_CRATE: &str = "astrs-operator-api";

/// The facade both of the above are reachable through.
const FACADE_CRATE: &str = "astrs";

/// The facade module holding the re-export chain (`astrs::__private::data`,
/// `astrs::__private::operator_api`).
const FACADE_PRIVATE_MODULE: &str = "__private";

/// Turns one [`proc_macro_crate`] answer into the path prefix that names it
/// from inside the crate being compiled.
///
/// `Itself` becomes `crate` — the crate being compiled *is* the one wanted,
/// so no extern-prelude lookup is involved (and `::astrs_data` would be
/// wrong: a crate cannot name itself by its own package name).
fn found_crate_path(found: FoundCrate) -> TokenStream {
    match found {
        FoundCrate::Itself => quote!(crate),
        FoundCrate::Name(name) => {
            let ident = syn::Ident::new(&name, Span::call_site());
            quote!(::#ident)
        }
    }
}

/// Resolves `target` (a crate name as it appears in `[workspace.dependencies]`)
/// to the path the generated code should name it by.
///
/// `facade_item` is the identifier `target` is re-exported as inside the
/// facade's `__private` module, used only when the invoking crate depends on
/// the facade rather than on `target` itself.
///
/// `fallback` is the absolute path used when no manifest can be consulted —
/// the in-workspace answer, correct for every crate in this repository and
/// for any downstream build that keeps cargo's own naming.
fn resolve(target: &str, facade_item: &str, fallback: TokenStream) -> TokenStream {
    if let Ok(found) = crate_name(target) {
        return found_crate_path(found);
    }
    if let Ok(found) = crate_name(FACADE_CRATE) {
        let facade = found_crate_path(found);
        let private = syn::Ident::new(FACADE_PRIVATE_MODULE, Span::call_site());
        let item = syn::Ident::new(facade_item, Span::call_site());
        return quote!(#facade::#private::#item);
    }
    fallback
}

/// The path `#[derive(AstrsMessage)]`'s generated code reaches `astrs-data`
/// by, honouring an explicit `#[astrs(crate = "…")]` when one is present.
pub(crate) fn data_crate(explicit: Option<&syn::Path>) -> TokenStream {
    match explicit {
        Some(path) => quote!(#path),
        None => resolve(DATA_CRATE, "data", quote!(::astrs_data)),
    }
}

/// The path `#[astrs::operator]`'s generated code reaches
/// `astrs-operator-api` by.
///
/// No escape hatch of its own: the attribute's argument grammar is a bare
/// optional string literal (the registered operator name), and overloading it
/// with a key/value form would change how every existing
/// `#[astrs::operator("name")]` parses. A crate that needs to override this
/// path can write the two-line `operator_entry()` by hand — it is, by
/// construction, exactly what `register_operator!` produces.
pub(crate) fn operator_api_crate() -> TokenStream {
    resolve(
        OPERATOR_API_CRATE,
        "operator_api",
        quote!(::astrs_operator_api),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Normalizes a token stream for comparison: `proc_macro2`'s `Display`
    /// spaces every token, so the expected text is written the same way.
    fn text(tokens: &TokenStream) -> String {
        tokens.to_string()
    }

    #[test]
    fn an_explicit_crate_attribute_wins_over_any_manifest() {
        let path: syn::Path = syn::parse_str("::vendored::columns").unwrap();
        assert_eq!(text(&data_crate(Some(&path))), ":: vendored :: columns");
    }

    #[test]
    fn a_named_crate_becomes_an_absolute_path() {
        let path = found_crate_path(FoundCrate::Name("astrs_data".to_owned()));
        assert_eq!(text(&path), ":: astrs_data");
    }

    #[test]
    fn a_renamed_dependency_keeps_the_name_the_manifest_gave_it() {
        // The case a constant path cannot express: `columns = { package =
        // "astrs-data" }` is nameable only as `::columns`.
        let path = found_crate_path(FoundCrate::Name("columns".to_owned()));
        assert_eq!(text(&path), ":: columns");
    }

    #[test]
    fn the_crate_itself_is_named_by_the_crate_keyword() {
        assert_eq!(text(&found_crate_path(FoundCrate::Itself)), "crate");
    }

    #[test]
    fn an_unnameable_target_falls_through_to_the_fallback() {
        // Neither the named target nor the facade appears in *this* crate's
        // manifest, so both lookups miss and the fallback is what comes back
        // — the branch that keeps the derive working in a build where no
        // manifest can be consulted at all.
        let path = resolve(
            "a-crate-no-manifest-will-ever-name",
            "data",
            quote!(::astrs_data),
        );
        assert_eq!(text(&path), ":: astrs_data");
    }

    #[test]
    fn the_operator_api_path_resolves_to_something_absolute_or_relative() {
        // Whatever branch the ambient manifest takes, the result must be a
        // usable path — never empty.
        assert!(!text(&operator_api_crate()).is_empty());
    }

    #[test]
    fn this_crates_own_manifest_resolves_astrs_data_through_its_dev_dependency() {
        // `proc-macro-crate` reads `[dependencies]` *and*
        // `[dev-dependencies]`, which is what keeps this crate's own
        // `tests/derive_expansion.rs` compiling: `astrs-data` is a
        // dev-dependency here, and the derive is invoked from that test.
        assert_eq!(text(&data_crate(None)), ":: astrs_data");
    }
}
