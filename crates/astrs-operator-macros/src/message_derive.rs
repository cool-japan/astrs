//! Assembles the `impl astrs_data::AstrsMessage for Struct` block from a
//! parsed [`syn::DeriveInput`] — the one place [`crate::shape`]'s parsing
//! and [`crate::codegen`]'s per-field token generation meet.
//!
//! # The `const _` wrapper
//!
//! The impl block does not land at the invocation site directly; it lands
//! inside an anonymous `const _: () = { … };` that opens with `use
//! <resolved crate> as __astrs_data;`. That single alias is what lets every
//! token fragment in [`crate::codegen`] name `__astrs_data::…` while the
//! path it actually resolves to is decided per invoking crate — see
//! [`crate::crate_path`]'s module docs for why a fixed `::astrs_data` is
//! wrong for a downstream crate whose only dependency is the `astrs` facade,
//! and why the alias must be a `use` rather than a leading-`::` path (a
//! leading `::` resolves through the crate root and the extern prelude, both
//! of which are blind to a block-local item).
//!
//! Wrapping is sound here precisely because the whole output is one
//! `impl … for #ident` block: trait impls are registered globally regardless
//! of the scope they are written in, so nothing the caller can observe moves
//! inside the block. The struct's own name still resolves, because a block
//! inherits its enclosing scope for name resolution — the same shape
//! `serde_derive` emits.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::attr::parse_message_attr;
use crate::codegen::{data_type_expr, decode_field_expr, encode_field_expr};
use crate::crate_path::data_crate;
use crate::shape::parse_message_fields;

/// Expands `#[derive(AstrsMessage)]` for one struct.
///
/// # Errors
///
/// A [`syn::Error`] from attribute parsing ([`parse_message_attr`]), field
/// parsing ([`parse_message_fields`]), or this function's own check that
/// the struct is not generic (generic message types are out of scope: every
/// field type this derive maps is concrete, and threading a type parameter
/// through a nested nested-message nested trait bound is not something any
/// of this crate's supported shapes need).
pub(crate) fn expand(input: &syn::DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "AstrsMessage does not support generic structs",
        ));
    }

    let attr = parse_message_attr(&input.attrs, input.ident.span())?;
    let fields = parse_message_fields(&input.data, input.ident.span())?;

    let struct_ident = &input.ident;
    let urn_lit = &attr.urn;
    let field_count = fields.len();
    let field_count_lit = proc_macro2::Literal::usize_unsuffixed(field_count);

    let mut field_defs = Vec::with_capacity(field_count);
    let mut encode_lets = Vec::with_capacity(field_count);
    let mut encode_idents = Vec::with_capacity(field_count);
    let mut decode_lets = Vec::with_capacity(field_count);
    let mut field_idents = Vec::with_capacity(field_count);

    for (index, plan) in fields.iter().enumerate() {
        let field_ident = plan.ident.clone();
        let name_str = field_ident.to_string();
        let data_type = data_type_expr(&plan.shape);
        let field_def = if plan.nullable {
            quote! { __astrs_data::Field::new(#name_str, #data_type, true) }
        } else {
            quote! { __astrs_data::Field::required(#name_str, #data_type) }
        };
        field_defs.push(field_def);

        let encode_ident = format_ident!("__astrs_col_{index}");
        let encode_block = encode_field_expr(plan);
        encode_lets.push(quote! {
            let #encode_ident: __astrs_data::ArrayRef = #encode_block;
        });
        encode_idents.push(encode_ident);

        let column_ident = format_ident!("__astrs_row_{index}");
        decode_lets.push(quote! {
            let #column_ident = &__astrs_cols[#index];
        });
        let decode_block = decode_field_expr(plan, &column_ident);
        decode_lets.push(quote! {
            let #field_ident = #decode_block;
        });
        field_idents.push(field_ident);
    }

    let data_crate = data_crate(attr.crate_path.as_ref());

    let message_impl = quote! {
        #[automatically_derived]
        impl __astrs_data::AstrsMessage for #struct_ident {
            const URN: &'static str = #urn_lit;

            fn data_type() -> __astrs_data::DataType {
                __astrs_data::DataType::strukt([ #(#field_defs),* ])
            }

            fn to_record_batch(&self) -> __astrs_data::Result<__astrs_data::RecordBatch> {
                #( #encode_lets )*
                let __astrs_fields: ::std::vec::Vec<__astrs_data::Field> = ::std::vec![ #(#field_defs),* ];
                let __astrs_columns: ::std::vec::Vec<__astrs_data::ArrayRef> = ::std::vec![ #(#encode_idents),* ];
                let __astrs_struct = __astrs_data::array::StructArray::try_new(
                    __astrs_fields,
                    __astrs_columns,
                    ::core::option::Option::None,
                )?;
                ::core::result::Result::Ok(__astrs_data::RecordBatch::from_payload(
                    __astrs_data::array::IntoArrayRef::into_array_ref(__astrs_struct),
                ))
            }

            fn from_record_batch(batch: &__astrs_data::RecordBatch) -> __astrs_data::Result<Self> {
                // A name-less import: brings `Array`'s and `ArrayExt`'s
                // methods (`.is_null()`, `.len()`, `.try_downcast()`) into
                // scope for the rest of this function body without binding
                // either trait's name — safe regardless of what the
                // invoking module itself imports (see `codegen`'s module
                // docs).
                #[allow(unused_imports)]
                use __astrs_data::array::{Array as _, ArrayExt as _};

                if batch.num_rows() != 1 {
                    return ::core::result::Result::Err(__astrs_data::DataError::MessageRowCount {
                        actual: batch.num_rows(),
                    });
                }
                let __astrs_payload = batch.payload_column().ok_or(__astrs_data::DataError::ColumnCountMismatch {
                    fields: 1,
                    columns: 0,
                })?;
                let __astrs_struct = __astrs_payload.try_downcast::<__astrs_data::array::StructArray>()?;
                let __astrs_cols = __astrs_struct.columns();
                if __astrs_cols.len() != #field_count_lit {
                    return ::core::result::Result::Err(__astrs_data::DataError::ColumnCountMismatch {
                        fields: #field_count_lit,
                        columns: __astrs_cols.len(),
                    });
                }
                #( #decode_lets )*
                ::core::result::Result::Ok(Self { #(#field_idents),* })
            }
        }
    };

    Ok(quote! {
        const _: () = {
            // The one place the generated code names a crate path (see the
            // module docs). `#[allow(unused_imports)]` because a struct with
            // no fields at all is rejected earlier, but a future shape that
            // never touches the alias should not warn in a user's build.
            #[allow(unused_imports)]
            use #data_crate as __astrs_data;

            #message_impl
        };
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn expand_str(item: &str) -> syn::Result<String> {
        let input: syn::DeriveInput = syn::parse_str(item).unwrap();
        expand(&input).map(|tokens| tokens.to_string())
    }

    #[test]
    fn expands_a_well_formed_struct_without_error() {
        let expanded = expand_str(
            r#"
            #[astrs(urn = "std/vision/v1/Detections")]
            struct Detections { boxes: Vec<[f32; 4]>, scores: Vec<f32>, labels: Vec<u32> }
            "#,
        )
        .unwrap();
        assert!(expanded.contains("AstrsMessage"));
        assert!(expanded.contains("to_record_batch"));
        assert!(expanded.contains("from_record_batch"));
        assert!(expanded.contains("std/vision/v1/Detections"));
    }

    #[test]
    fn rejects_a_generic_struct() {
        let err =
            expand_str(r#"#[astrs(urn = "std/core/v1/Foo")] struct Foo<T> { x: T }"#).unwrap_err();
        assert!(err.to_string().contains("generic structs"));
    }

    #[test]
    fn rejects_a_struct_missing_the_attribute() {
        assert!(expand_str("struct Foo { x: f32 }").is_err());
    }

    #[test]
    fn rejects_a_unit_struct_even_with_a_valid_attribute() {
        let err = expand_str(r#"#[astrs(urn = "std/core/v1/Foo")] struct Foo;"#).unwrap_err();
        assert!(err.to_string().contains("no columnar layout to derive"));
    }

    #[test]
    fn every_field_becomes_a_local_binding_named_after_it() {
        let expanded = expand_str(
            r#"#[astrs(urn = "std/geometry/v1/Vector3")] struct V { x: f64, y: f64, z: f64 }"#,
        )
        .unwrap();
        // Field-init shorthand `Self { x , y , z }` — proc-macro2's
        // `Display` for `TokenStream` always spaces tokens, so match on the
        // spaced form rather than the exact written syntax.
        assert!(expanded.contains("Self { x , y , z }") || expanded.contains("Self { x, y, z }"));
    }
}
