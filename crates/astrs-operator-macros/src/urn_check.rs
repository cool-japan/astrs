//! A permissive, macro-time syntax check on `#[astrs(urn = "...")]`.
//!
//! This is the genuinely **compile-time** half of this crate's layout
//! checking (the other half — cross-checking the derived layout against
//! `astrs-data`'s registered layout for the URN — cannot run until the
//! registry, a runtime data structure, exists; see this crate's `tests/`
//! for that half, run as ordinary `#[test]`s rather than macro-generated
//! ones, per this crate's own docs on why). A malformed URN string fails
//! the derive itself, with a real `compile_error!` at the attribute's own
//! span — no waiting for `cargo test`.
//!
//! Deliberately **not** a full implementation of the grammar
//! `astrs_data::urn::parse` owns (`std/<category>/v<u16>/<Type>[k=v,…]`,
//! blueprint §24.3): this crate has no dependency on `astrs-data` outside
//! its dev-dependencies (a proc-macro crate emits *references* to paths in
//! the invoking crate's own dependency graph; it does not need the crate
//! whose types it names), so re-implementing the parser here would be a
//! second, drifting copy of the same grammar. This check only rejects what
//! is unambiguously wrong — a missing "std/" namespace, the wrong number of
//! segments, a version segment that is not "v" followed by digits, or a
//! type name that does not start with an uppercase letter — and defers
//! everything else (is this *specific* type actually registered? does it
//! accept these parameters?) to the registry itself.

/// Checks `urn`'s literal text against the shape every `std/v1` URN has,
/// without attempting to resolve it against the registry.
///
/// # Errors
///
/// A [`syn::Error`] spanning the literal, naming the specific rule it
/// breaks.
pub(crate) fn check_urn_syntax(urn: &syn::LitStr) -> syn::Result<()> {
    let text = urn.value();
    if text.trim().is_empty() {
        return Err(syn::Error::new_spanned(
            urn,
            "AstrsMessage urn must not be empty",
        ));
    }
    if !text.starts_with("std/") {
        return Err(syn::Error::new_spanned(
            urn,
            format!(
                "AstrsMessage urn {text:?} must start with \"std/\" — 0.1.0 accepts only the \
                 std namespace (blueprint §24.3)"
            ),
        ));
    }
    let segments: Vec<&str> = text.split('/').collect();
    let [_namespace, category, version, type_and_params] = segments.as_slice() else {
        return Err(syn::Error::new_spanned(
            urn,
            format!(
                "AstrsMessage urn {text:?} must have the form std/<category>/v<N>/<Type>[params], \
                 found {} segment(s)",
                segments.len()
            ),
        ));
    };
    if category.is_empty() {
        return Err(syn::Error::new_spanned(
            urn,
            format!("AstrsMessage urn {text:?} is missing a category between the two slashes"),
        ));
    }
    let is_version = version.len() > 1
        && version.starts_with('v')
        && version[1..].bytes().all(|b| b.is_ascii_digit());
    if !is_version {
        return Err(syn::Error::new_spanned(
            urn,
            format!(
                "AstrsMessage urn {text:?} must have a version segment shaped like \"v1\", found {version:?}"
            ),
        ));
    }
    let type_name = type_and_params.split('[').next().unwrap_or(type_and_params);
    let starts_uppercase = type_name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase());
    if type_name.is_empty() || !starts_uppercase {
        return Err(syn::Error::new_spanned(
            urn,
            format!(
                "AstrsMessage urn {text:?} must name a type starting with an uppercase letter, \
                 found {type_name:?}"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn lit(text: &str) -> syn::LitStr {
        syn::LitStr::new(text, proc_macro2::Span::call_site())
    }

    #[test]
    fn accepts_every_real_std_urn_shape() {
        for text in [
            "std/core/v1/Float64",
            "std/vision/v1/Detections",
            "std/geometry/v1/Pose",
            "std/sensor/v1/Imu",
            "std/media/v1/CompressedImage[format=jpeg]",
            "std/sensor/v1/PointCloud[fields=x:y:z]",
        ] {
            assert!(check_urn_syntax(&lit(text)).is_ok(), "{text}");
        }
    }

    #[test]
    fn accepts_a_plausible_urn_not_yet_in_the_registry() {
        // The syntax check does not know the registry's contents — it only
        // rejects unambiguous shape violations. `layout_of` is where an
        // unregistered type is caught (see this crate's `tests/`).
        assert!(check_urn_syntax(&lit("std/core/v1/TagSet")).is_ok());
    }

    #[test]
    fn rejects_an_empty_string() {
        assert!(check_urn_syntax(&lit("")).is_err());
        assert!(check_urn_syntax(&lit("   ")).is_err());
    }

    #[test]
    fn rejects_a_non_std_namespace() {
        let err = check_urn_syntax(&lit("custom/core/v1/Foo")).unwrap_err();
        assert!(err.to_string().contains("must start with \"std/\""));
    }

    #[test]
    fn rejects_the_wrong_segment_count() {
        for text in ["std/core/v1", "std/core/v1/Foo/extra", "std//v1/Foo"] {
            let err = check_urn_syntax(&lit(text)).unwrap_err();
            assert!(
                err.to_string().contains("segment(s)") || err.to_string().contains("category"),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn rejects_a_malformed_version_segment() {
        for text in [
            "std/core/V1/Foo",
            "std/core/1/Foo",
            "std/core/version1/Foo",
            "std/core/v/Foo",
        ] {
            let err = check_urn_syntax(&lit(text)).unwrap_err();
            assert!(err.to_string().contains("version segment"), "{text}: {err}");
        }
    }

    #[test]
    fn rejects_a_lowercase_type_name() {
        let err = check_urn_syntax(&lit("std/core/v1/foo")).unwrap_err();
        assert!(err.to_string().contains("uppercase letter"));
    }

    #[test]
    fn rejects_a_missing_type_name_before_parameters() {
        let err = check_urn_syntax(&lit("std/core/v1/[fields=x]")).unwrap_err();
        assert!(err.to_string().contains("uppercase letter"));
    }

    #[test]
    fn accepts_bracketed_parameters_after_the_type_name() {
        assert!(check_urn_syntax(&lit("std/media/v1/Image[pixel=rgb8,width=640]")).is_ok());
    }
}
