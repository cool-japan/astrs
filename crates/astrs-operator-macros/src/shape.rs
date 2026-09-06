//! Field-type grammar for `#[derive(AstrsMessage)]`, factored into plain
//! functions over `syn` types.
//!
//! Kept separate from the actual `#[proc_macro_derive]` entry point so it is
//! testable without macro expansion (trybuild-style negative tests are
//! unavailable without adding a dev-dependency this workspace does not
//! retain — see this crate's `lib.rs` docs) — every function here takes and
//! returns ordinary `syn`/`proc_macro2` values and is exercised directly by
//! this module's `#[cfg(test)]` block, including every rejection path
//! (`Vec<X>` where `X` is a nested message type, a unit struct, a tuple
//! struct, `Option<Option<T>>`, an unsupported field type, a non-literal
//! array length).
//!
//! # Grammar
//!
//! ```text
//! FieldType    ::= Option<Unwrapped> | Unwrapped
//! Unwrapped    ::= "Vec" "<" ListElement ">" | "[" ContainerLeaf ";" INT "]" | Leaf
//! ListElement  ::= "[" ContainerLeaf ";" INT "]" | ContainerLeaf
//! Leaf         ::= ContainerLeaf | Path                          (Path = nested AstrsMessage)
//! ContainerLeaf::= ScalarIdent | "String"
//! ScalarIdent  ::= "bool" | "i8".."i64" | "u8".."u64" | "f32" | "f64"
//! ```
//!
//! `Vec<[T; N]>` (blueprint §9.1's own `boxes: Vec<[f32; 4]>`) is the one
//! two-level composition this grammar admits — a `List` of `FixedSizeList`
//! rows — because it is the shape every parallel-array detector/keypoint
//! message in the closed `std` registry actually uses
//! (`astrs_data::urn::layouts::vision`). Nothing else composes: no
//! `Vec<Vec<_>>`, no `[[T; N]; M]`.
//!
//! [`ContainerLeaf`] — not [`LeafShape`] — is the element type of both
//! container positions (`Vec<_>` and `[T; N]`), and it has no `Nested`
//! variant at all: a nested `AstrsMessage` type is only a
//! [`FieldShape::Leaf`], never inside a container. This is a type-level
//! choice, not a checked-at-codegen-time rule — [`crate::codegen`]'s
//! container-encoding functions take a `&ContainerLeaf` and so cannot be
//! handed a nested type to begin with, which is what lets them build their
//! array unconditionally instead of holding a defensive error branch for a
//! case the parser already made unreachable. (The runtime reason it is
//! excluded: a container field is always read back with a per-element
//! `.get(i)` call that can only report "unexpectedly null", never itself
//! recurse into another message's `to_record_batch`/`from_record_batch` —
//! that recursion is exactly what [`LeafShape::Nested`] gets when it is the
//! *whole* field, and only there.)
//!
//! `Option` may wrap only a field's outermost type; an inner `Option`
//! anywhere else (`Option<Option<T>>`, or one found while parsing a `Vec`'s
//! or array's element) is rejected.

use syn::spanned::Spanned;

/// A field's type after unwrapping any outer `Option`.
///
/// Not `Debug` (transitively, through [`LeafShape::Nested`]'s `syn::Path`,
/// which is only `Debug` behind `syn`'s `extra-traits` feature — not part
/// of this crate's workspace-pinned feature set); tests that need to
/// inspect a rejection match on the `Err` variant directly rather than
/// through `unwrap_err`.
#[derive(Clone)]
pub(crate) enum FieldShape {
    /// A bare scalar, `String`, or nested `AstrsMessage` type.
    Leaf(LeafShape),
    /// `Vec<T>` or `Vec<[T; N]>` — one [`astrs_data::DataType::List`] per
    /// message.
    List(ListElement),
    /// `[T; N]` — one [`astrs_data::DataType::FixedSizeList`] of size `N`.
    FixedArray(ContainerLeaf, usize),
}

/// The element type of a [`FieldShape::List`] — either a bare leaf
/// (`Vec<f32>`) or a fixed-size row (`Vec<[f32; 4]>`).
#[derive(Debug, Clone)]
pub(crate) enum ListElement {
    /// `Vec<T>`.
    Leaf(ContainerLeaf),
    /// `Vec<[T; N]>`.
    FixedArray(ContainerLeaf, usize),
}

/// The element type of a [`FieldShape::Leaf`]: a scalar, `String`, or a
/// path naming another `AstrsMessage` type.
///
/// Not `Debug` — see [`FieldShape`]'s docs.
#[derive(Clone)]
pub(crate) enum LeafShape {
    /// A scalar or `String` — anything [`ContainerLeaf`] also allows.
    Container(ContainerLeaf),
    /// A path naming another type that itself derives `AstrsMessage`.
    Nested(syn::Path),
}

/// The element type allowed inside a `Vec<_>` or `[T; N]` — deliberately
/// **without** a nested-`AstrsMessage` variant (see the [module
/// docs](self)).
#[derive(Debug, Clone)]
pub(crate) enum ContainerLeaf {
    /// A numeric or boolean scalar.
    Scalar(ScalarKind),
    /// `String`.
    String,
}

/// The eleven scalar Rust types `#[derive(AstrsMessage)]` recognizes —
/// `bool` plus the ten [`astrs_data::ArrowNativeType`] integer/float widths
/// (`f16`/[`astrs_data::F16`] has no native Rust literal type, so it is not
/// in this list; a field can still reach `Float16` by nesting a hand-written
/// [`astrs_data::AstrsMessage`] type that uses it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarKind {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

impl ScalarKind {
    /// Recognizes one of the eleven scalar type idents.
    pub(crate) fn from_ident(ident: &syn::Ident) -> Option<Self> {
        Some(match ident.to_string().as_str() {
            "bool" => Self::Bool,
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "i64" => Self::I64,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "f32" => Self::F32,
            "f64" => Self::F64,
            _ => return None,
        })
    }
}

/// One parsed struct field: its name, whether it was `Option`-wrapped, and
/// its unwrapped shape.
///
/// Not `Debug` — see [`FieldShape`]'s docs.
pub(crate) struct FieldPlan {
    pub(crate) ident: syn::Ident,
    pub(crate) nullable: bool,
    pub(crate) shape: FieldShape,
}

/// Parses every field of a `#[derive(AstrsMessage)]` struct.
///
/// # Errors
///
/// A [`syn::Error`] naming the first field (or the struct itself) that does
/// not fit the grammar: a non-struct item, a unit or tuple struct, a struct
/// with no fields, or any field whose type [`parse_field_shape`] rejects.
pub(crate) fn parse_message_fields(
    data: &syn::Data,
    span: proc_macro2::Span,
) -> syn::Result<Vec<FieldPlan>> {
    let syn::Data::Struct(data_struct) = data else {
        return Err(syn::Error::new(
            span,
            "AstrsMessage can only be derived for a struct",
        ));
    };
    let named = match &data_struct.fields {
        syn::Fields::Named(named) => &named.named,
        syn::Fields::Unit => {
            return Err(syn::Error::new(
                span,
                "AstrsMessage requires at least one named field; a unit struct has no columnar layout to derive",
            ));
        }
        syn::Fields::Unnamed(unnamed) => {
            return Err(syn::Error::new_spanned(
                unnamed,
                "AstrsMessage requires named struct fields; tuple structs are not supported",
            ));
        }
    };
    if named.is_empty() {
        return Err(syn::Error::new(
            span,
            "AstrsMessage requires at least one field; a struct with no fields has no columnar layout to derive",
        ));
    }
    named.iter().map(parse_field_plan).collect()
}

/// Parses one struct field into a [`FieldPlan`].
///
/// # Errors
///
/// A [`syn::Error`] when the field has no name (tuple-struct position) or
/// its type is rejected by [`parse_field_shape`].
pub(crate) fn parse_field_plan(field: &syn::Field) -> syn::Result<FieldPlan> {
    let ident = field.ident.clone().ok_or_else(|| {
        syn::Error::new_spanned(
            field,
            "AstrsMessage requires named struct fields; tuple structs are not supported",
        )
    })?;
    let (shape, nullable) = parse_field_shape(&field.ty)?;
    Ok(FieldPlan {
        ident,
        nullable,
        shape,
    })
}

/// Parses one field's type into a `(shape, nullable)` pair, per the
/// grammar in the [module docs](self).
///
/// # Errors
///
/// A [`syn::Error`] when the type does not fit the grammar — see the module
/// docs for the specific rejections.
pub(crate) fn parse_field_shape(ty: &syn::Type) -> syn::Result<(FieldShape, bool)> {
    if let Some(inner) = option_inner(ty)? {
        let shape = parse_unwrapped_shape(inner)?;
        Ok((shape, true))
    } else {
        let shape = parse_unwrapped_shape(ty)?;
        Ok((shape, false))
    }
}

/// Returns the inner type when `ty` is exactly `Option<Inner>`.
fn option_inner(ty: &syn::Type) -> syn::Result<Option<&syn::Type>> {
    let syn::Type::Path(type_path) = ty else {
        return Ok(None);
    };
    if type_path.qself.is_some() {
        return Ok(None);
    }
    let Some(segment) = type_path.path.segments.last() else {
        return Ok(None);
    };
    if segment.ident != "Option" {
        return Ok(None);
    }
    match single_type_arg(segment)? {
        Some(inner) => Ok(Some(inner)),
        None => Err(syn::Error::new_spanned(
            segment,
            "Option must have exactly one type parameter",
        )),
    }
}

/// Extracts the sole type argument of a generic path segment
/// (`Vec<T>`/`Option<T>`), or `None` when the segment has no angle-bracketed
/// arguments at all (a bare ident).
fn single_type_arg(segment: &syn::PathSegment) -> syn::Result<Option<&syn::Type>> {
    match &segment.arguments {
        syn::PathArguments::None => Ok(None),
        syn::PathArguments::AngleBracketed(generic) => {
            let types: Vec<&syn::Type> = generic
                .args
                .iter()
                .filter_map(|arg| match arg {
                    syn::GenericArgument::Type(ty) => Some(ty),
                    _ => None,
                })
                .collect();
            match types.len() {
                1 => Ok(Some(types[0])),
                _ => Err(syn::Error::new_spanned(
                    generic,
                    format!("{} must have exactly one type parameter", segment.ident),
                )),
            }
        }
        syn::PathArguments::Parenthesized(paren) => Err(syn::Error::new_spanned(
            paren,
            "AstrsMessage does not support function-pointer-style path arguments",
        )),
    }
}

/// Parses a field's type after any outer `Option` has already been removed.
fn parse_unwrapped_shape(ty: &syn::Type) -> syn::Result<FieldShape> {
    match ty {
        syn::Type::Array(array) => {
            let leaf = parse_container_leaf(&array.elem)?;
            let len = eval_array_len(&array.len)?;
            Ok(FieldShape::FixedArray(leaf, len))
        }
        syn::Type::Path(type_path) => {
            if type_path.qself.is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "AstrsMessage does not support qualified-self types",
                ));
            }
            let Some(segment) = type_path.path.segments.last() else {
                return Err(syn::Error::new_spanned(
                    ty,
                    "AstrsMessage requires a named field type",
                ));
            };
            if segment.ident == "Option" {
                return Err(syn::Error::new_spanned(
                    ty,
                    "AstrsMessage does not support a nested Option (Option<Option<_>>); \
                     Option may only wrap a field's outermost type",
                ));
            }
            if segment.ident == "Vec" {
                let inner = single_type_arg(segment)?.ok_or_else(|| {
                    syn::Error::new_spanned(segment, "Vec must have exactly one type parameter")
                })?;
                let element = parse_list_element(inner)?;
                return Ok(FieldShape::List(element));
            }
            let leaf = parse_leaf(ty)?;
            Ok(FieldShape::Leaf(leaf))
        }
        other => Err(syn::Error::new_spanned(
            other,
            "unsupported field type for AstrsMessage: expected a numeric scalar, bool, String, \
             Vec<_>, a fixed-size array, Option<_>, or a nested AstrsMessage type",
        )),
    }
}

/// Parses a `Vec<_>`'s element: either a bare leaf (`Vec<f32>`) or a
/// fixed-size row (`Vec<[f32; 4]>` — see the [module docs](self)).
fn parse_list_element(ty: &syn::Type) -> syn::Result<ListElement> {
    if let syn::Type::Array(array) = ty {
        let leaf = parse_container_leaf(&array.elem)?;
        let len = eval_array_len(&array.len)?;
        return Ok(ListElement::FixedArray(leaf, len));
    }
    let leaf = parse_container_leaf(ty)?;
    Ok(ListElement::Leaf(leaf))
}

/// Parses a bare leaf type: a scalar, `String`, or a path assumed to name
/// another `AstrsMessage` type.
fn parse_leaf(ty: &syn::Type) -> syn::Result<LeafShape> {
    if let Ok(container) = parse_container_leaf(ty) {
        return Ok(LeafShape::Container(container));
    }
    let syn::Type::Path(type_path) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "unsupported field type for AstrsMessage: expected a scalar, String, or a named \
             nested AstrsMessage type",
        ));
    };
    if type_path.qself.is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "AstrsMessage does not support qualified-self types",
        ));
    }
    let path = &type_path.path;
    let Some(segment) = path.segments.last() else {
        return Err(syn::Error::new_spanned(
            ty,
            "AstrsMessage requires a named field type",
        ));
    };
    if !matches!(segment.arguments, syn::PathArguments::None) {
        if segment.ident == "Vec" || segment.ident == "Option" {
            return Err(syn::Error::new_spanned(
                ty,
                format!(
                    "AstrsMessage does not support nesting {} here; only a scalar, String, or a \
                     nested AstrsMessage type may sit in a bare field position",
                    segment.ident
                ),
            ));
        }
        return Err(syn::Error::new_spanned(
            ty,
            "AstrsMessage does not support generic field types here",
        ));
    }
    // A bare, non-generic, named type that is not a recognized scalar/String
    // is assumed to be another type deriving `AstrsMessage` — the compile
    // error if it is not surfaces later, from the generated `impl` body
    // referencing `<Path as astrs_data::AstrsMessage>`, which is exactly
    // where an unsatisfied-trait-bound error belongs.
    Ok(LeafShape::Nested(path.clone()))
}

/// Parses the element type of a `Vec<_>` or `[T; N]`: a scalar or `String`
/// only (see the [module docs](self) for why a nested `AstrsMessage` type
/// is not accepted here).
///
/// # Errors
///
/// A [`syn::Error`] when `ty` is anything else, including a path this crate
/// would otherwise treat as a nested message type in a bare field position.
fn parse_container_leaf(ty: &syn::Type) -> syn::Result<ContainerLeaf> {
    let syn::Type::Path(type_path) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "unsupported element type inside Vec<_>/[T; N] for AstrsMessage: expected a numeric \
             scalar, bool, or String",
        ));
    };
    if type_path.qself.is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "AstrsMessage does not support qualified-self types",
        ));
    }
    let path = &type_path.path;
    let Some(segment) = path.segments.last() else {
        return Err(syn::Error::new_spanned(
            ty,
            "AstrsMessage requires a named element type",
        ));
    };
    if !matches!(segment.arguments, syn::PathArguments::None) {
        return Err(syn::Error::new_spanned(
            ty,
            format!(
                "AstrsMessage does not support nesting {} inside Vec<_>/[T; N]; only a numeric \
                 scalar, bool, or String is allowed there",
                segment.ident
            ),
        ));
    }
    if segment.ident == "String" {
        return Ok(ContainerLeaf::String);
    }
    if let Some(kind) = ScalarKind::from_ident(&segment.ident) {
        return Ok(ContainerLeaf::Scalar(kind));
    }
    Err(syn::Error::new_spanned(
        ty,
        "AstrsMessage does not support a nested message type inside Vec<_> or a fixed-size \
         array in this release; only a numeric scalar, bool, or String is allowed there",
    ))
}

/// Evaluates a fixed-size array's length, requiring a literal integer.
///
/// # Errors
///
/// A [`syn::Error`] when the length is a const expression rather than a
/// literal, or does not fit `usize`.
fn eval_array_len(expr: &syn::Expr) -> syn::Result<usize> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(lit_int),
            ..
        }) => lit_int.base10_parse::<usize>(),
        other => Err(syn::Error::new(
            other.span(),
            "AstrsMessage requires a literal integer array length (e.g. `[f32; 4]`), not a \
             const expression",
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Unwraps a `syn::Result`'s `Err` side without requiring the `Ok` side
    /// to be `Debug` — [`FieldShape`]/[`FieldPlan`] deliberately are not
    /// (see their docs).
    fn expect_err<T>(result: syn::Result<T>) -> syn::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }

    fn parse_type(text: &str) -> syn::Type {
        syn::parse_str(text).unwrap()
    }

    fn parse_fields_of(item: &str) -> syn::Result<Vec<FieldPlan>> {
        let input: syn::DeriveInput = syn::parse_str(item).unwrap();
        parse_message_fields(&input.data, proc_macro2::Span::call_site())
    }

    #[test]
    fn scalar_kinds_round_trip_every_supported_ident() {
        for (text, expected) in [
            ("bool", ScalarKind::Bool),
            ("i8", ScalarKind::I8),
            ("i16", ScalarKind::I16),
            ("i32", ScalarKind::I32),
            ("i64", ScalarKind::I64),
            ("u8", ScalarKind::U8),
            ("u16", ScalarKind::U16),
            ("u32", ScalarKind::U32),
            ("u64", ScalarKind::U64),
            ("f32", ScalarKind::F32),
            ("f64", ScalarKind::F64),
        ] {
            let ident = syn::Ident::new(text, proc_macro2::Span::call_site());
            assert_eq!(ScalarKind::from_ident(&ident), Some(expected), "{text}");
        }
        let not_scalar = syn::Ident::new("String", proc_macro2::Span::call_site());
        assert_eq!(ScalarKind::from_ident(&not_scalar), None);
    }

    #[test]
    fn bare_scalar_and_string_and_nested_leaves() {
        let (shape, nullable) = parse_field_shape(&parse_type("f32")).unwrap();
        assert!(!nullable);
        assert!(matches!(
            shape,
            FieldShape::Leaf(LeafShape::Container(ContainerLeaf::Scalar(ScalarKind::F32)))
        ));

        let (shape, nullable) = parse_field_shape(&parse_type("String")).unwrap();
        assert!(!nullable);
        assert!(matches!(
            shape,
            FieldShape::Leaf(LeafShape::Container(ContainerLeaf::String))
        ));

        let (shape, nullable) = parse_field_shape(&parse_type("Quaternion")).unwrap();
        assert!(!nullable);
        match shape {
            FieldShape::Leaf(LeafShape::Nested(path)) => {
                assert_eq!(quote::quote!(#path).to_string(), "Quaternion");
            }
            _ => panic!("expected a nested leaf"),
        }
    }

    #[test]
    fn vec_of_scalar_and_string_are_lists() {
        let (shape, nullable) = parse_field_shape(&parse_type("Vec<f32>")).unwrap();
        assert!(!nullable);
        assert!(matches!(
            shape,
            FieldShape::List(ListElement::Leaf(ContainerLeaf::Scalar(ScalarKind::F32)))
        ));

        let (shape, _) = parse_field_shape(&parse_type("Vec<String>")).unwrap();
        assert!(matches!(
            shape,
            FieldShape::List(ListElement::Leaf(ContainerLeaf::String))
        ));
    }

    #[test]
    fn vec_of_fixed_arrays_is_a_list_of_fixed_size_rows() {
        // blueprint §9.1: `boxes: Vec<[f32; 4]>`.
        let (shape, nullable) = parse_field_shape(&parse_type("Vec<[f32; 4]>")).unwrap();
        assert!(!nullable);
        match shape {
            FieldShape::List(ListElement::FixedArray(
                ContainerLeaf::Scalar(ScalarKind::F32),
                len,
            )) => {
                assert_eq!(len, 4);
            }
            _ => panic!("expected a list of 4-wide f32 rows"),
        }
    }

    #[test]
    fn fixed_arrays_capture_their_literal_length() {
        let (shape, _) = parse_field_shape(&parse_type("[f32; 4]")).unwrap();
        match shape {
            FieldShape::FixedArray(ContainerLeaf::Scalar(ScalarKind::F32), len) => {
                assert_eq!(len, 4);
            }
            _ => panic!("expected a fixed array of f32"),
        }

        let (shape, _) = parse_field_shape(&parse_type("[f64; 9]")).unwrap();
        match shape {
            FieldShape::FixedArray(ContainerLeaf::Scalar(ScalarKind::F64), len) => {
                assert_eq!(len, 9);
            }
            _ => panic!("expected a fixed array of f64"),
        }
    }

    #[test]
    fn option_wraps_any_shape_at_the_top_level_only() {
        let (shape, nullable) = parse_field_shape(&parse_type("Option<f64>")).unwrap();
        assert!(nullable);
        assert!(matches!(
            shape,
            FieldShape::Leaf(LeafShape::Container(ContainerLeaf::Scalar(ScalarKind::F64)))
        ));

        let (shape, nullable) = parse_field_shape(&parse_type("Option<Vec<f32>>")).unwrap();
        assert!(nullable);
        assert!(matches!(
            shape,
            FieldShape::List(ListElement::Leaf(ContainerLeaf::Scalar(ScalarKind::F32)))
        ));

        let (shape, nullable) = parse_field_shape(&parse_type("Option<[f32; 4]>")).unwrap();
        assert!(nullable);
        assert!(matches!(shape, FieldShape::FixedArray(_, 4)));
    }

    #[test]
    fn double_option_is_rejected() {
        assert!(parse_field_shape(&parse_type("Option<Option<f32>>")).is_err());
    }

    #[test]
    fn nested_type_inside_vec_or_fixed_array_is_rejected() {
        let err = expect_err(parse_field_shape(&parse_type("Vec<Quaternion>")));
        assert!(
            err.to_string()
                .contains("does not support a nested message type")
        );

        let err = expect_err(parse_field_shape(&parse_type("[Quaternion; 3]")));
        assert!(
            err.to_string()
                .contains("does not support a nested message type")
        );

        let err = expect_err(parse_field_shape(&parse_type("Vec<[Quaternion; 3]>")));
        assert!(
            err.to_string()
                .contains("does not support a nested message type")
        );
    }

    #[test]
    fn option_inside_a_container_is_rejected() {
        assert!(parse_field_shape(&parse_type("Vec<Option<f32>>")).is_err());
        assert!(parse_field_shape(&parse_type("[Option<f32>; 3]")).is_err());
    }

    #[test]
    fn unsupported_field_types_are_rejected() {
        for text in ["&str", "(f32, f32)", "*const f32", "[f32]"] {
            assert!(
                parse_field_shape(&parse_type(text)).is_err(),
                "{text} should be rejected"
            );
        }
    }

    #[test]
    fn a_const_expression_array_length_is_rejected() {
        let err = expect_err(parse_field_shape(&parse_type("[f32; N]")));
        assert!(err.to_string().contains("literal integer array length"));

        let err = expect_err(parse_field_shape(&parse_type("Vec<[f32; N]>")));
        assert!(err.to_string().contains("literal integer array length"));
    }

    #[test]
    fn hash_map_like_multi_generic_types_are_rejected() {
        assert!(parse_field_shape(&parse_type("HashMap<String, String>")).is_err());
    }

    #[test]
    fn unit_struct_has_no_fields_to_derive() {
        let err = expect_err(parse_fields_of("struct Unit;"));
        assert!(err.to_string().contains("no columnar layout to derive"));
    }

    #[test]
    fn empty_named_struct_has_no_fields_to_derive() {
        let err = expect_err(parse_fields_of("struct Empty {}"));
        assert!(err.to_string().contains("no columnar layout to derive"));
    }

    #[test]
    fn tuple_struct_is_rejected() {
        let err = expect_err(parse_fields_of("struct Tuple(f32, f32);"));
        assert!(err.to_string().contains("tuple structs are not supported"));
    }

    #[test]
    fn enum_is_rejected() {
        let err = expect_err(parse_fields_of("enum NotAStruct { A, B }"));
        assert!(err.to_string().contains("can only be derived for a struct"));
    }

    #[test]
    fn a_well_formed_struct_parses_every_field_in_order() {
        let plans = parse_fields_of(
            "struct Detections { boxes: Vec<[f32; 4]>, scores: Vec<f32>, labels: Vec<u32> }",
        )
        .unwrap();
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].ident, "boxes");
        assert!(matches!(
            plans[0].shape,
            FieldShape::List(ListElement::FixedArray(ContainerLeaf::Scalar(_), 4))
        ));
        assert_eq!(plans[1].ident, "scores");
        assert!(matches!(
            plans[1].shape,
            FieldShape::List(ListElement::Leaf(ContainerLeaf::Scalar(_)))
        ));
        assert_eq!(plans[2].ident, "labels");
        assert!(!plans[0].nullable && !plans[1].nullable && !plans[2].nullable);
    }
}
