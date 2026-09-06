//! AST field/constant declarations resolved into the Rust types and
//! expressions [`crate::codegen::emit`] assembles into `impl` blocks.
//!
//! # Why constants split into `const` items and functions
//!
//! A ROS 2 constant's *field* type (`String`, `astrs_cdr::BoundedString<N>`,
//! `astrs_cdr::WString`, …) is what a struct *field* of the same IDL type
//! uses — but `pub const NAME: String = …;` does not compile: none of
//! `String::from`, `BoundedString::new` or `WString::from` are `const fn`,
//! so a heap-allocating value cannot be a `const` item at all. An unbounded
//! `string` constant is therefore typed `&'static str` (a string literal
//! *is* const-evaluable) rather than `String`; a bounded string, `wstring`
//! or bounded `wstring` constant — needing a runtime constructor either way
//! — becomes a zero-argument associated **function** instead of a `const`
//! item. [`ConstantShape`] carries which one a given constant needs.

use proc_macro2::{Ident, TokenStream};
use quote::quote;

use crate::ast::{
    ArraySuffix, ConstantDecl, FieldDecl, Literal, PrimitiveKind, ScalarType, StringKind,
};
use crate::error::IdlError;
use crate::naming::{PackageName, TypeName, to_rust_ident};
use crate::resolve::TypeUniverse;

/// How a field's array suffix maps onto both its Rust type and which
/// `astrs_idl::runtime::ColumnValue` composition path
/// [`crate::codegen::emit`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldShape {
    /// No array suffix: the element type is the field's own type.
    Scalar,
    /// `T[]`: `Vec<Element>`.
    Unbounded,
    /// `T[N]`: `[Element; N]`.
    Fixed(u32),
    /// `T[<=N]`: `astrs_cdr::BoundedSequence<Element, N>`.
    Bounded(u32),
}

impl From<ArraySuffix> for FieldShape {
    fn from(suffix: ArraySuffix) -> Self {
        match suffix {
            ArraySuffix::None => Self::Scalar,
            ArraySuffix::Unbounded => Self::Unbounded,
            ArraySuffix::Fixed(n) => Self::Fixed(n),
            ArraySuffix::Bounded(n) => Self::Bounded(n),
        }
    }
}

/// A field, resolved to what [`crate::codegen::emit`] needs: a Rust
/// identifier and type, and a total (never-panicking) `astrs_cdr::CdrDefault`
/// expression.
#[derive(Debug)]
pub(crate) struct ResolvedField {
    pub(crate) ident: Ident,
    pub(crate) ros_name: String,
    pub(crate) doc: String,
    /// The field's full Rust type (`f64`, `Vec<f64>`, `[f64; 3]`,
    /// `astrs_cdr::BoundedSequence<f64, 3>`, a nested message type, …).
    pub(crate) rust_type: TokenStream,
    /// The element/leaf type an array/sequence shape is built from; equal
    /// to `rust_type` itself when `shape` is [`FieldShape::Scalar`].
    pub(crate) element_type: TokenStream,
    pub(crate) shape: FieldShape,
    /// True when the element is `byte`/`char`/`uint8` — decides whether
    /// [`crate::codegen::emit`] treats an `Unbounded`/`Fixed` shape as the
    /// `Vec<u8>`/`[u8; N]` octet-blob leaf (`Binary`/`FixedSizeBinary`) or
    /// wraps a non-octet element through the generic list helpers.
    pub(crate) is_octet_element: bool,
    /// True when `element_type` is one of the eleven IDL primitives — an
    /// `f64`/`i32`/`bool`/… field can be read out of `&Self` by value
    /// (`v.field`) rather than [`Clone::clone`]d, which matters because
    /// `clippy::clone_on_copy` (part of this workspace's `-D warnings` gate)
    /// rejects the latter for a `Copy` type. Only meaningful for
    /// [`FieldShape::Scalar`] — [`crate::codegen::emit`] already borrows
    /// (`&v.field`) rather than collects-and-clones for every other shape,
    /// and a `Fixed` octet leaf (`[u8; N]`) is unconditionally `Copy`
    /// regardless of this flag, since an array of `Copy` is always `Copy`.
    pub(crate) is_copy: bool,
    /// A total `astrs_cdr::CdrDefault::cdr_default()`-shaped expression.
    pub(crate) default_expr: TokenStream,
}

/// Resolves one field declaration.
///
/// # Errors
///
/// [`IdlError::UnknownType`] when the field's type is a namespaced/bare
/// reference `universe` cannot resolve.
pub(crate) fn build_field(
    field: &FieldDecl,
    home_package: &PackageName,
    universe: &TypeUniverse,
) -> Result<ResolvedField, IdlError> {
    let (element_type, is_octet_element) =
        element_type(&field.type_.scalar, home_package, universe)?;
    let shape = FieldShape::from(field.type_.array);
    let rust_type = field_rust_type(shape, &element_type);
    let default_expr = build_default_expr(shape, &field.type_.scalar, field.default.as_ref());
    let is_copy = matches!(field.type_.scalar, ScalarType::Primitive(_));
    Ok(ResolvedField {
        ident: to_rust_ident(&field.name),
        ros_name: field.name.clone(),
        doc: field
            .comment
            .clone()
            .unwrap_or_else(|| format!("The `{}` field.", field.name)),
        rust_type,
        element_type,
        shape,
        is_octet_element,
        is_copy,
        default_expr,
    })
}

/// The `structure_needs_at_least_one_member` placeholder `rosidl` itself
/// synthesizes for a message with zero real fields — OMG IDL structs must
/// have at least one member. [`crate::codegen`] calls this whenever a
/// section's field list is empty, so every downstream step (the struct
/// itself, `CdrSerde`, `ColumnValue`, `Default`) sees a uniform non-empty
/// field list and needs no empty-struct special case of its own.
pub(crate) fn placeholder_field() -> ResolvedField {
    ResolvedField {
        ident: Ident::new(
            "structure_needs_at_least_one_member",
            proc_macro2::Span::call_site(),
        ),
        ros_name: "structure_needs_at_least_one_member".to_owned(),
        doc: "Placeholder: OMG IDL structs require at least one member; this message declares \
              none (`rosidl`'s own convention for an empty `.msg`/request/response/goal/result/\
              feedback)."
            .to_owned(),
        rust_type: quote!(u8),
        element_type: quote!(u8),
        shape: FieldShape::Scalar,
        is_octet_element: true,
        is_copy: true,
        default_expr: quote!(0u8),
    }
}

/// A constant, resolved to what [`crate::codegen::emit`] needs.
#[derive(Debug)]
pub(crate) struct ResolvedConstant {
    pub(crate) ident: Ident,
    pub(crate) doc: String,
    pub(crate) rust_type: TokenStream,
    pub(crate) value_expr: TokenStream,
    /// [`ConstantShape::Const`] emits `pub const NAME: T = value;`;
    /// [`ConstantShape::Function`] emits `pub fn name() -> T { value }` —
    /// see the [module documentation](self).
    pub(crate) shape: ConstantShape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstantShape {
    Const,
    Function,
}

/// Resolves one constant declaration. Infallible: constants are always
/// scalar and never a message-type reference (enforced by
/// [`IdlError::ConstantMustBeScalar`]/[`IdlError::ConstantMustBePrimitive`]
/// at parse time), so no [`TypeUniverse`] lookup is ever needed.
pub(crate) fn build_constant(constant: &ConstantDecl) -> ResolvedConstant {
    let (rust_type, shape) = constant_rust_type(&constant.type_.scalar);
    let value_expr = literal_expr(
        &constant.type_.scalar,
        &constant.value,
        shape == ConstantShape::Function,
    );
    ResolvedConstant {
        ident: to_rust_ident(&constant.name),
        doc: constant
            .comment
            .clone()
            .unwrap_or_else(|| format!("`{}` constant.", constant.name)),
        rust_type,
        value_expr,
        shape,
    }
}

/// A const-generic-position integer literal without a `usize` suffix
/// (`3`, not `3usize`) — purely cosmetic, for generated code that reads
/// like hand-written Rust (`[f64; 3]`, `BoundedString<8>`) rather than
/// exposing that it came from a macro.
pub(crate) fn unsuffixed(n: u32) -> proc_macro2::Literal {
    proc_macro2::Literal::usize_unsuffixed(n as usize)
}

fn primitive_rust_type(kind: PrimitiveKind) -> TokenStream {
    match kind {
        PrimitiveKind::Bool => quote!(bool),
        PrimitiveKind::Byte | PrimitiveKind::Char | PrimitiveKind::Uint8 => quote!(u8),
        PrimitiveKind::Int8 => quote!(i8),
        PrimitiveKind::Int16 => quote!(i16),
        PrimitiveKind::Uint16 => quote!(u16),
        PrimitiveKind::Int32 => quote!(i32),
        PrimitiveKind::Uint32 => quote!(u32),
        PrimitiveKind::Int64 => quote!(i64),
        PrimitiveKind::Uint64 => quote!(u64),
        PrimitiveKind::Float32 => quote!(f32),
        PrimitiveKind::Float64 => quote!(f64),
    }
}

/// The field-shaped Rust type for a scalar/string/wstring type: owned
/// `String`/`WString` for the unbounded cases, since a *field* (unlike a
/// [`ConstantShape`]) always owns its data.
fn primitive_or_string_rust_type(scalar: &ScalarType) -> TokenStream {
    match scalar {
        ScalarType::Primitive(kind) => primitive_rust_type(*kind),
        ScalarType::Str {
            kind: StringKind::String,
            bound: None,
        } => quote!(::std::string::String),
        ScalarType::Str {
            kind: StringKind::String,
            bound: Some(n),
        } => {
            let n = unsuffixed(*n);
            quote!(::astrs_cdr::BoundedString<#n>)
        }
        ScalarType::Str {
            kind: StringKind::WString,
            bound: None,
        } => quote!(::astrs_cdr::WString),
        ScalarType::Str {
            kind: StringKind::WString,
            bound: Some(n),
        } => {
            let n = unsuffixed(*n);
            quote!(::astrs_cdr::BoundedWString<#n>)
        }
        // Unreachable: a `Named` scalar is resolved through `element_type`,
        // never through this string/primitive-only helper.
        ScalarType::Named(_) => quote!(()),
    }
}

fn element_type(
    scalar: &ScalarType,
    home_package: &PackageName,
    universe: &TypeUniverse,
) -> Result<(TokenStream, bool), IdlError> {
    if let ScalarType::Named(named) = scalar {
        let target = universe.resolve(named, home_package)?;
        return Ok((rust_path_for(target, home_package), false));
    }
    let is_octet = matches!(
        scalar,
        ScalarType::Primitive(PrimitiveKind::Byte | PrimitiveKind::Char | PrimitiveKind::Uint8)
    );
    Ok((primitive_or_string_rust_type(scalar), is_octet))
}

/// The Rust path for a nested message type: a bare `super::Type` when it
/// lives in the same generated package module, an absolute
/// `astrs_idl::generated::pkg::Type` otherwise (blueprint §10.3 — every
/// generated package is one `astrs_idl::generated::<pkg>` module of
/// sibling type modules re-exported at the package level, so `super::` from
/// inside one type's own file reaches every sibling).
pub(crate) fn rust_path_for(target: &TypeName, home_package: &PackageName) -> TokenStream {
    let type_ident = Ident::new(&target.name, proc_macro2::Span::call_site());
    if target.package.as_str() == home_package.as_str() {
        quote!(super::#type_ident)
    } else {
        let pkg_ident = Ident::new(target.package.as_str(), proc_macro2::Span::call_site());
        quote!(astrs_idl::generated::#pkg_ident::#type_ident)
    }
}

fn field_rust_type(shape: FieldShape, element_type: &TokenStream) -> TokenStream {
    match shape {
        FieldShape::Scalar => element_type.clone(),
        FieldShape::Unbounded => quote!(::std::vec::Vec<#element_type>),
        FieldShape::Fixed(n) => {
            let n = unsuffixed(n);
            quote!([#element_type; #n])
        }
        FieldShape::Bounded(n) => {
            let n = unsuffixed(n);
            quote!(::astrs_cdr::BoundedSequence<#element_type, #n>)
        }
    }
}

fn int_literal_tokens(kind: PrimitiveKind, value: i128) -> TokenStream {
    match kind {
        PrimitiveKind::Byte | PrimitiveKind::Char | PrimitiveKind::Uint8 => {
            let v = value as u8;
            quote!(#v)
        }
        PrimitiveKind::Int8 => {
            let v = value as i8;
            quote!(#v)
        }
        PrimitiveKind::Int16 => {
            let v = value as i16;
            quote!(#v)
        }
        PrimitiveKind::Uint16 => {
            let v = value as u16;
            quote!(#v)
        }
        PrimitiveKind::Int32 => {
            let v = value as i32;
            quote!(#v)
        }
        PrimitiveKind::Uint32 => {
            let v = value as u32;
            quote!(#v)
        }
        PrimitiveKind::Int64 => {
            let v = value as i64;
            quote!(#v)
        }
        PrimitiveKind::Uint64 => {
            let v = value as u64;
            quote!(#v)
        }
        // Unreachable: `Bool`/`Float32`/`Float64` are handled by their own
        // branches in `literal_expr` before this is ever called.
        PrimitiveKind::Bool | PrimitiveKind::Float32 | PrimitiveKind::Float64 => quote!(0),
    }
}

/// The value expression for one scalar literal against its declared type.
///
/// `owned_string` is `false` for a [`ConstantShape::Const`] unbounded
/// `string` (`&'static str` — the literal alone), `true` for a field or a
/// bounded/`wstring` constant (owned, allocated types). Total: every
/// `(scalar, literal)` pairing produces *some* expression, falling back to
/// `astrs_cdr::CdrDefault::cdr_default()` for a combination parse-time
/// validation ([`crate::parser::literal::validate_value`]) already
/// guarantees cannot occur.
fn literal_expr(scalar: &ScalarType, literal: &Literal, owned_string: bool) -> TokenStream {
    match scalar {
        ScalarType::Primitive(PrimitiveKind::Bool) => match literal {
            Literal::Bool(b) => quote!(#b),
            _ => quote!(::astrs_cdr::CdrDefault::cdr_default()),
        },
        ScalarType::Primitive(kind @ (PrimitiveKind::Float32 | PrimitiveKind::Float64)) => {
            let value = match literal {
                Literal::Float(f) => *f,
                Literal::Int(i) => *i as f64,
                _ => return quote!(::astrs_cdr::CdrDefault::cdr_default()),
            };
            if matches!(kind, PrimitiveKind::Float32) {
                let v = value as f32;
                quote!(#v)
            } else {
                quote!(#value)
            }
        }
        ScalarType::Primitive(kind) => match literal {
            Literal::Int(i) => int_literal_tokens(*kind, *i),
            _ => quote!(::astrs_cdr::CdrDefault::cdr_default()),
        },
        ScalarType::Str {
            kind: StringKind::String,
            bound: None,
        } => match literal {
            Literal::Str(s) if owned_string => quote!(::std::string::String::from(#s)),
            Literal::Str(s) => quote!(#s),
            _ => quote!(::astrs_cdr::CdrDefault::cdr_default()),
        },
        ScalarType::Str {
            kind: StringKind::String,
            bound: Some(n),
        } => match literal {
            Literal::Str(s) => {
                let n = unsuffixed(*n);
                // Already length-checked in bytes at parse time
                // (`DefaultExceedsBound`); `unwrap_or_default` turns the
                // still-fallible constructor into a total expression
                // without a second bound check at codegen time.
                quote!(::astrs_cdr::BoundedString::<#n>::new(#s).unwrap_or_default())
            }
            _ => quote!(::astrs_cdr::CdrDefault::cdr_default()),
        },
        ScalarType::Str {
            kind: StringKind::WString,
            bound: None,
        } => match literal {
            Literal::Str(s) => quote!(::astrs_cdr::WString::from(#s)),
            _ => quote!(::astrs_cdr::CdrDefault::cdr_default()),
        },
        ScalarType::Str {
            kind: StringKind::WString,
            bound: Some(n),
        } => match literal {
            Literal::Str(s) => {
                let n = unsuffixed(*n);
                // Already length-checked in UTF-16 units at parse time.
                quote!(::astrs_cdr::BoundedWString::<#n>::new(::astrs_cdr::WString::from(#s)).unwrap_or_default())
            }
            _ => quote!(::astrs_cdr::CdrDefault::cdr_default()),
        },
        ScalarType::Named(_) => quote!(::astrs_cdr::CdrDefault::cdr_default()),
    }
}

fn build_default_expr(
    shape: FieldShape,
    scalar: &ScalarType,
    default: Option<&Literal>,
) -> TokenStream {
    let elements = match default {
        Some(Literal::Array(elements)) => Some(elements),
        _ => None,
    };
    match (shape, default, elements) {
        (FieldShape::Scalar, Some(literal), None) => literal_expr(scalar, literal, true),
        (FieldShape::Unbounded, _, Some(elements)) => {
            let exprs: Vec<TokenStream> = elements
                .iter()
                .map(|e| literal_expr(scalar, e, true))
                .collect();
            quote!(::std::vec![#(#exprs),*])
        }
        (FieldShape::Fixed(_), _, Some(elements)) => {
            let exprs: Vec<TokenStream> = elements
                .iter()
                .map(|e| literal_expr(scalar, e, true))
                .collect();
            quote!([#(#exprs),*])
        }
        (FieldShape::Bounded(_), _, Some(elements)) => {
            let exprs: Vec<TokenStream> = elements
                .iter()
                .map(|e| literal_expr(scalar, e, true))
                .collect();
            // Already bound-checked (element count) at parse time
            // (`DefaultExceedsBound`); see `literal_expr`'s bounded-string
            // branch for why `unwrap_or_default` rather than a panic path.
            quote!(
                ::astrs_cdr::BoundedSequence::from_vec(::std::vec![#(#exprs),*])
                    .unwrap_or_default()
            )
        }
        // No explicit default (or, defensively, a shape/literal mismatch
        // parse-time validation already rules out): the IDL zero value.
        _ => quote!(::astrs_cdr::CdrDefault::cdr_default()),
    }
}

fn constant_rust_type(scalar: &ScalarType) -> (TokenStream, ConstantShape) {
    match scalar {
        ScalarType::Primitive(kind) => (primitive_rust_type(*kind), ConstantShape::Const),
        ScalarType::Str {
            kind: StringKind::String,
            bound: None,
        } => (quote!(&'static str), ConstantShape::Const),
        ScalarType::Str {
            kind: StringKind::String,
            bound: Some(n),
        } => {
            let n = unsuffixed(*n);
            (
                quote!(::astrs_cdr::BoundedString<#n>),
                ConstantShape::Function,
            )
        }
        ScalarType::Str {
            kind: StringKind::WString,
            bound: None,
        } => (quote!(::astrs_cdr::WString), ConstantShape::Function),
        ScalarType::Str {
            kind: StringKind::WString,
            bound: Some(n),
        } => {
            let n = unsuffixed(*n);
            (
                quote!(::astrs_cdr::BoundedWString<#n>),
                ConstantShape::Function,
            )
        }
        // Unreachable: `IdlError::ConstantMustBePrimitive` rejects a
        // message-typed constant at parse time.
        ScalarType::Named(_) => (quote!(()), ConstantShape::Const),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::naming::InterfaceKind;
    use crate::parser::parse_message;
    use crate::span::{Position, Span};

    fn pkg(name: &str) -> PackageName {
        PackageName::new(name, Span::empty(Position::new(1, 1))).unwrap()
    }

    fn one_field(source: &str) -> FieldDecl {
        let file = parse_message(source).unwrap();
        file.section.fields().next().unwrap().clone()
    }

    fn one_constant(source: &str) -> ConstantDecl {
        let file = parse_message(source).unwrap();
        file.section.constants().next().unwrap().clone()
    }

    #[test]
    fn a_scalar_primitive_field_resolves_to_its_rust_type_and_default() {
        let universe = TypeUniverse::new();
        let field = one_field("float64 x 2.5\n");
        let resolved = build_field(&field, &pkg("geometry_msgs"), &universe).unwrap();
        assert_eq!(resolved.rust_type.to_string(), "f64");
        assert_eq!(resolved.shape, FieldShape::Scalar);
        assert_eq!(resolved.default_expr.to_string(), "2.5f64");
    }

    #[test]
    fn an_unbounded_octet_array_is_a_leaf_vec_u8() {
        let universe = TypeUniverse::new();
        let field = one_field("uint8[] data\n");
        let resolved = build_field(&field, &pkg("sensor_msgs"), &universe).unwrap();
        assert_eq!(
            resolved.rust_type.to_string(),
            ":: std :: vec :: Vec < u8 >"
        );
        assert_eq!(resolved.shape, FieldShape::Unbounded);
        assert!(resolved.is_octet_element);
    }

    #[test]
    fn a_fixed_non_octet_array_is_a_rust_array_type() {
        let universe = TypeUniverse::new();
        let field = one_field("float64[3] xyz\n");
        let resolved = build_field(&field, &pkg("geometry_msgs"), &universe).unwrap();
        assert_eq!(resolved.rust_type.to_string(), "[f64 ; 3]");
        assert_eq!(resolved.shape, FieldShape::Fixed(3));
        assert!(!resolved.is_octet_element);
    }

    #[test]
    fn a_bounded_sequence_uses_the_astrs_cdr_wrapper_type() {
        let universe = TypeUniverse::new();
        let field = one_field("int32[<=4] samples\n");
        let resolved = build_field(&field, &pkg("pkg"), &universe).unwrap();
        assert_eq!(
            resolved.rust_type.to_string(),
            ":: astrs_cdr :: BoundedSequence < i32 , 4 >"
        );
    }

    #[test]
    fn a_same_package_named_field_resolves_to_a_super_path() {
        let mut universe = TypeUniverse::new();
        universe.register(
            TypeName::new(
                pkg("geometry_msgs"),
                InterfaceKind::Msg,
                "Point",
                Span::empty(Position::new(1, 1)),
            )
            .unwrap(),
        );
        let field = one_field("Point origin\n");
        let resolved = build_field(&field, &pkg("geometry_msgs"), &universe).unwrap();
        assert_eq!(resolved.rust_type.to_string(), "super :: Point");
    }

    #[test]
    fn a_cross_package_named_field_resolves_to_an_absolute_path() {
        let mut universe = TypeUniverse::new();
        universe.register(
            TypeName::new(
                pkg("geometry_msgs"),
                InterfaceKind::Msg,
                "Point",
                Span::empty(Position::new(1, 1)),
            )
            .unwrap(),
        );
        let field = one_field("geometry_msgs/Point position\n");
        let resolved = build_field(&field, &pkg("sensor_msgs"), &universe).unwrap();
        assert_eq!(
            resolved.rust_type.to_string(),
            "astrs_idl :: generated :: geometry_msgs :: Point"
        );
    }

    #[test]
    fn an_unresolvable_named_field_is_reported() {
        let universe = TypeUniverse::new();
        let field = one_field("geometry_msgs/Point position\n");
        let err = build_field(&field, &pkg("sensor_msgs"), &universe).unwrap_err();
        assert!(matches!(err, IdlError::UnknownType { .. }));
    }

    #[test]
    fn a_field_with_no_default_delegates_to_cdr_default() {
        let universe = TypeUniverse::new();
        let field = one_field("int32 x\n");
        let resolved = build_field(&field, &pkg("pkg"), &universe).unwrap();
        assert_eq!(
            resolved.default_expr.to_string(),
            ":: astrs_cdr :: CdrDefault :: cdr_default ()"
        );
    }

    #[test]
    fn an_array_default_builds_a_vec_macro_call() {
        let universe = TypeUniverse::new();
        let field = one_field("int32[] samples [1, -2, 3]\n");
        let resolved = build_field(&field, &pkg("pkg"), &universe).unwrap();
        assert_eq!(
            resolved.default_expr.to_string(),
            ":: std :: vec ! [1i32 , - 2i32 , 3i32]"
        );
    }

    #[test]
    fn the_placeholder_field_is_a_zero_u8() {
        let placeholder = placeholder_field();
        assert_eq!(placeholder.ros_name, "structure_needs_at_least_one_member");
        assert_eq!(placeholder.rust_type.to_string(), "u8");
        assert_eq!(placeholder.default_expr.to_string(), "0u8");
    }

    #[test]
    fn bool_and_integer_constants_are_const_items() {
        let constant = build_constant(&one_constant("int32 X=42\n"));
        assert_eq!(constant.shape, ConstantShape::Const);
        assert_eq!(constant.rust_type.to_string(), "i32");
        assert_eq!(constant.value_expr.to_string(), "42i32");
    }

    #[test]
    fn an_unbounded_string_constant_is_a_borrowed_const() {
        let constant = build_constant(&one_constant("string FOO=\"bar\"\n"));
        assert_eq!(constant.shape, ConstantShape::Const);
        assert_eq!(constant.rust_type.to_string(), "& 'static str");
        assert_eq!(constant.value_expr.to_string(), "\"bar\"");
    }

    #[test]
    fn a_bounded_string_constant_becomes_a_function() {
        let constant = build_constant(&one_constant("string<=8 FOO=\"bar\"\n"));
        assert_eq!(constant.shape, ConstantShape::Function);
        assert_eq!(
            constant.rust_type.to_string(),
            ":: astrs_cdr :: BoundedString < 8 >"
        );
    }

    #[test]
    fn field_and_constant_docs_fall_back_to_a_deterministic_default() {
        let field = one_field("int32 x\n");
        let resolved = build_field(&field, &pkg("pkg"), &TypeUniverse::new()).unwrap();
        assert_eq!(resolved.doc, "The `x` field.");

        let constant = build_constant(&one_constant("int32 X=1\n"));
        assert_eq!(constant.doc, "`X` constant.");
    }
}
