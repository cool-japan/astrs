//! Parses the `#[astrs(urn = "...", crate = "...")]` container attribute.
//!
//! `urn` is required; `crate` is the optional escape hatch documented on
//! [`crate::derive_astrs_message`] — the path the generated code should
//! reach `astrs-data` by, for a build [`proc_macro_crate`](proc_macro_crate)
//! cannot resolve on its own (see [`crate::crate_path`]).

use crate::urn_check::check_urn_syntax;

/// The parsed `#[astrs(...)]` attribute on a `#[derive(AstrsMessage)]`
/// struct.
///
/// Not `Debug`: `syn::LitStr` is only `Debug` behind `syn`'s `extra-traits`
/// feature, which is not part of this crate's workspace-pinned feature set.
pub(crate) struct MessageAttr {
    pub(crate) urn: syn::LitStr,
    /// An explicit `crate = "..."` override, already parsed as a path.
    pub(crate) crate_path: Option<syn::Path>,
}

/// Finds and parses `#[astrs(urn = "...")]` among a struct's attributes.
///
/// # Errors
///
/// A [`syn::Error`] when no `#[astrs(...)]` attribute is present, when it
/// has no `urn` key, when `urn`'s value is not a string literal, when
/// `crate`'s value is not a string literal holding a syntactically valid
/// path, when an unrecognized key appears, or when the URN string fails
/// [`check_urn_syntax`].
pub(crate) fn parse_message_attr(
    attrs: &[syn::Attribute],
    fallback_span: proc_macro2::Span,
) -> syn::Result<MessageAttr> {
    let mut urn: Option<syn::LitStr> = None;
    let mut crate_path: Option<syn::Path> = None;
    let mut found_astrs_attr = false;

    for attr in attrs {
        if !attr.path().is_ident("astrs") {
            continue;
        }
        found_astrs_attr = true;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("urn") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                urn = Some(lit);
                Ok(())
            } else if meta.path.is_ident("crate") {
                // `crate` is a keyword, but `parse_nested_meta` parses the
                // key with `Ident::parse_any`, so it arrives as an ordinary
                // path segment — the same spelling `serde(crate = "...")`
                // uses, which is the point of matching the convention.
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                crate_path = Some(lit.parse()?);
                Ok(())
            } else {
                Err(meta.error("unsupported astrs(..) attribute key; expected `urn` or `crate`"))
            }
        })?;
    }

    let urn = match urn {
        Some(urn) => urn,
        None if found_astrs_attr => {
            return Err(syn::Error::new(
                fallback_span,
                "#[astrs(...)] is missing its required `urn = \"...\"` key",
            ));
        }
        None => {
            return Err(syn::Error::new(
                fallback_span,
                "AstrsMessage requires #[astrs(urn = \"...\")] on the struct",
            ));
        }
    };
    check_urn_syntax(&urn)?;
    Ok(MessageAttr { urn, crate_path })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Unwraps a `syn::Result`'s `Err` side without requiring the `Ok` side
    /// to be `Debug` — [`MessageAttr`] deliberately is not (see its docs).
    fn expect_err<T>(result: syn::Result<T>) -> syn::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }

    fn attrs_of(item: &str) -> Vec<syn::Attribute> {
        let input: syn::DeriveInput = syn::parse_str(item).unwrap();
        input.attrs
    }

    #[test]
    fn parses_a_well_formed_attribute() {
        let attrs =
            attrs_of(r#"#[astrs(urn = "std/vision/v1/Detections")] struct Detections { x: f32 }"#);
        let parsed = parse_message_attr(&attrs, proc_macro2::Span::call_site()).unwrap();
        assert_eq!(parsed.urn.value(), "std/vision/v1/Detections");
    }

    #[test]
    fn missing_attribute_is_an_error() {
        let attrs = attrs_of("struct Plain { x: f32 }");
        let err = expect_err(parse_message_attr(&attrs, proc_macro2::Span::call_site()));
        assert!(err.to_string().contains("requires #[astrs"));
    }

    #[test]
    fn missing_urn_key_is_an_error() {
        let attrs = attrs_of(r#"#[astrs(nope = "x")] struct Plain { x: f32 }"#);
        let err = expect_err(parse_message_attr(&attrs, proc_macro2::Span::call_site()));
        assert!(
            err.to_string()
                .contains("unsupported astrs(..) attribute key")
        );
        assert!(err.to_string().contains("`urn` or `crate`"));
    }

    #[test]
    fn no_crate_key_leaves_the_path_to_be_resolved_from_the_manifest() {
        let attrs = attrs_of(r#"#[astrs(urn = "std/core/v1/Foo")] struct Foo { x: f32 }"#);
        let parsed = parse_message_attr(&attrs, proc_macro2::Span::call_site()).unwrap();
        assert!(parsed.crate_path.is_none());
    }

    #[test]
    fn the_crate_key_is_parsed_as_a_path_despite_being_a_keyword() {
        let attrs = attrs_of(
            r#"#[astrs(urn = "std/core/v1/Foo", crate = "::vendored::columns")] struct Foo { x: f32 }"#,
        );
        let parsed = parse_message_attr(&attrs, proc_macro2::Span::call_site()).unwrap();
        let path = parsed.crate_path.expect("the override is recorded");
        let rendered = quote::ToTokens::to_token_stream(&path).to_string();
        assert_eq!(rendered, ":: vendored :: columns");
    }

    #[test]
    fn the_crate_key_may_be_spelled_in_its_own_attribute() {
        // Two `#[astrs(...)]` attributes on one struct are merged, so the
        // override does not have to share a bracket with the URN.
        let attrs = attrs_of(
            r#"
            #[astrs(urn = "std/core/v1/Foo")]
            #[astrs(crate = "astrs::__private::data")]
            struct Foo { x: f32 }
            "#,
        );
        let parsed = parse_message_attr(&attrs, proc_macro2::Span::call_site()).unwrap();
        assert!(parsed.crate_path.is_some());
        assert_eq!(parsed.urn.value(), "std/core/v1/Foo");
    }

    #[test]
    fn a_crate_value_that_is_not_a_path_is_rejected() {
        let attrs =
            attrs_of(r#"#[astrs(urn = "std/core/v1/Foo", crate = "1 + 1")] struct Foo { x: f32 }"#);
        assert!(parse_message_attr(&attrs, proc_macro2::Span::call_site()).is_err());
    }

    #[test]
    fn urn_without_a_value_is_an_error() {
        let attrs = attrs_of(r#"#[astrs(urn)] struct Plain { x: f32 }"#);
        assert!(parse_message_attr(&attrs, proc_macro2::Span::call_site()).is_err());
    }

    #[test]
    fn a_syntactically_invalid_urn_is_rejected() {
        let attrs = attrs_of(r#"#[astrs(urn = "not a urn")] struct Plain { x: f32 }"#);
        let err = expect_err(parse_message_attr(&attrs, proc_macro2::Span::call_site()));
        assert!(err.to_string().contains("std/"));
    }
}
