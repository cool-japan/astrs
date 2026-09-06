//! Turns a [`FieldPlan`] list into the three pieces of an `AstrsMessage`
//! impl: the `DataType` expression, the encode expression, and the decode
//! expression for one field.
//!
//! Every function here is total over its `ContainerLeaf`/`LeafShape` input
//! — [`ContainerLeaf`] has no `Nested` variant (see `shape.rs`'s module
//! docs), so the functions that build or read a container's child array
//! never need a defensive "this should not happen" branch for a nested
//! message type; the parser already made that state unrepresentable.
//!
//! # A note on trait scope
//!
//! `Array::is_null`/`Array::len` and `ArrayExt::try_downcast` are trait
//! methods, and this module's output is spliced into an arbitrary user
//! module that has no reason to have `use`d either trait. Converting every
//! call site to fully-qualified syntax would work but reads badly in an
//! expanded-macro dump; instead, [`derive_body`] opens each generated
//! function body with a scoped `use ... as _;` for both traits (a name-less
//! import — see that function's own comment), so every helper below can use
//! ordinary `.method()` syntax.

use proc_macro2::TokenStream;
use quote::quote;

use crate::shape::{ContainerLeaf, FieldPlan, FieldShape, LeafShape, ListElement, ScalarKind};

/// The `astrs_data::DataType` variant a scalar kind maps onto.
fn scalar_data_type(kind: ScalarKind) -> TokenStream {
    match kind {
        ScalarKind::Bool => quote!(__astrs_data::DataType::Bool),
        ScalarKind::I8 => quote!(__astrs_data::DataType::Int8),
        ScalarKind::I16 => quote!(__astrs_data::DataType::Int16),
        ScalarKind::I32 => quote!(__astrs_data::DataType::Int32),
        ScalarKind::I64 => quote!(__astrs_data::DataType::Int64),
        ScalarKind::U8 => quote!(__astrs_data::DataType::UInt8),
        ScalarKind::U16 => quote!(__astrs_data::DataType::UInt16),
        ScalarKind::U32 => quote!(__astrs_data::DataType::UInt32),
        ScalarKind::U64 => quote!(__astrs_data::DataType::UInt64),
        ScalarKind::F32 => quote!(__astrs_data::DataType::Float32),
        ScalarKind::F64 => quote!(__astrs_data::DataType::Float64),
    }
}

/// The concrete array type a scalar kind encodes/decodes through — the
/// same named aliases `astrs-data`'s own examples use (`Float32Array`, not
/// `PrimitiveArray<f32>`), so generated code reads like hand-written code.
fn scalar_array_type(kind: ScalarKind) -> TokenStream {
    match kind {
        ScalarKind::Bool => quote!(__astrs_data::array::BooleanArray),
        ScalarKind::I8 => quote!(__astrs_data::array::Int8Array),
        ScalarKind::I16 => quote!(__astrs_data::array::Int16Array),
        ScalarKind::I32 => quote!(__astrs_data::array::Int32Array),
        ScalarKind::I64 => quote!(__astrs_data::array::Int64Array),
        ScalarKind::U8 => quote!(__astrs_data::array::UInt8Array),
        ScalarKind::U16 => quote!(__astrs_data::array::UInt16Array),
        ScalarKind::U32 => quote!(__astrs_data::array::UInt32Array),
        ScalarKind::U64 => quote!(__astrs_data::array::UInt64Array),
        ScalarKind::F32 => quote!(__astrs_data::array::Float32Array),
        ScalarKind::F64 => quote!(__astrs_data::array::Float64Array),
    }
}

/// The scalar's own Rust type token (`f32`, `bool`, ...).
fn scalar_rust_type(kind: ScalarKind) -> TokenStream {
    match kind {
        ScalarKind::Bool => quote!(bool),
        ScalarKind::I8 => quote!(i8),
        ScalarKind::I16 => quote!(i16),
        ScalarKind::I32 => quote!(i32),
        ScalarKind::I64 => quote!(i64),
        ScalarKind::U8 => quote!(u8),
        ScalarKind::U16 => quote!(u16),
        ScalarKind::U32 => quote!(u32),
        ScalarKind::U64 => quote!(u64),
        ScalarKind::F32 => quote!(f32),
        ScalarKind::F64 => quote!(f64),
    }
}

/// The `DataType` a [`ContainerLeaf`] maps onto.
fn container_leaf_data_type(leaf: &ContainerLeaf) -> TokenStream {
    match leaf {
        ContainerLeaf::Scalar(kind) => scalar_data_type(*kind),
        ContainerLeaf::String => quote!(__astrs_data::DataType::Utf8),
    }
}

/// The array type a [`ContainerLeaf`] encodes/decodes through.
fn container_leaf_array_type(leaf: &ContainerLeaf) -> TokenStream {
    match leaf {
        ContainerLeaf::Scalar(kind) => scalar_array_type(*kind),
        ContainerLeaf::String => quote!(__astrs_data::array::StringArray),
    }
}

/// The leaf's owned Rust type token (`f32`, `::std::string::String`, ...).
fn container_leaf_rust_type(leaf: &ContainerLeaf) -> TokenStream {
    match leaf {
        ContainerLeaf::Scalar(kind) => scalar_rust_type(*kind),
        ContainerLeaf::String => quote!(::std::string::String),
    }
}

/// The `DataType` a [`LeafShape`] maps onto.
fn leaf_data_type(leaf: &LeafShape) -> TokenStream {
    match leaf {
        LeafShape::Container(container) => container_leaf_data_type(container),
        LeafShape::Nested(path) => quote!(<#path as __astrs_data::AstrsMessage>::data_type()),
    }
}

/// The `DataType::FixedSizeList` expression for a `[T; N]` shape.
fn fixed_array_data_type(leaf: &ContainerLeaf, len: usize) -> TokenStream {
    let item = container_leaf_data_type(leaf);
    let len_lit = proc_macro2::Literal::usize_unsuffixed(len);
    quote! { __astrs_data::DataType::fixed_size_list(__astrs_data::Field::required("item", #item), #len_lit as i32) }
}

/// The `DataType` a [`ListElement`] maps onto.
fn list_element_data_type(element: &ListElement) -> TokenStream {
    match element {
        ListElement::Leaf(leaf) => container_leaf_data_type(leaf),
        ListElement::FixedArray(leaf, len) => fixed_array_data_type(leaf, *len),
    }
}

/// The `DataType` a whole [`FieldShape`] maps onto (unwrapped — nullability
/// is a [`astrs_data::Field`] property, applied by the caller).
pub(crate) fn data_type_expr(shape: &FieldShape) -> TokenStream {
    match shape {
        FieldShape::Leaf(leaf) => leaf_data_type(leaf),
        FieldShape::List(element) => {
            let item = list_element_data_type(element);
            quote! { __astrs_data::DataType::list(__astrs_data::Field::required("item", #item)) }
        }
        FieldShape::FixedArray(leaf, len) => fixed_array_data_type(leaf, *len),
    }
}

/// Builds a single-row array for a [`ContainerLeaf`] from one owned Rust
/// value expression (a plain scalar for [`ContainerLeaf::Scalar`], a
/// `&str`-compatible expression for [`ContainerLeaf::String`]).
fn container_leaf_single_array(leaf: &ContainerLeaf, value: &TokenStream) -> TokenStream {
    let array_ty = container_leaf_array_type(leaf);
    quote! { __astrs_data::array::IntoArrayRef::into_array_ref(<#array_ty>::from_values([#value])) }
}

/// Builds a container's child array (the flattened values behind a
/// `List`/`FixedSizeList` row) from an iterator expression yielding the
/// leaf's owned Rust values.
fn container_leaf_child_array(leaf: &ContainerLeaf, iter: &TokenStream) -> TokenStream {
    let array_ty = container_leaf_array_type(leaf);
    quote! { __astrs_data::array::IntoArrayRef::into_array_ref(<#array_ty>::from_values(#iter)) }
}

/// Appends the final `.copied()` (for a `Copy` scalar) or
/// `.map(String::as_str)` conversion turning an iterator of *references* to
/// a leaf's stored type into an iterator of the type
/// [`container_leaf_child_array`] wants.
fn finish_leaf_ref_iter(leaf: &ContainerLeaf, ref_iter: TokenStream) -> TokenStream {
    match leaf {
        ContainerLeaf::Scalar(_) => quote! { #ref_iter.copied() },
        ContainerLeaf::String => quote! { #ref_iter.map(::std::string::String::as_str) },
    }
}

/// An iterator expression over `base: &Vec<Leaf>`'s owned leaf values —
/// `Vec<f32>`'s `.iter().copied()`, `Vec<String>`'s
/// `.iter().map(String::as_str)`.
fn container_leaf_iter(leaf: &ContainerLeaf, base: &TokenStream) -> TokenStream {
    finish_leaf_ref_iter(leaf, quote! { #base.iter() })
}

/// An iterator expression flattening `base: &Vec<[Leaf; N]>` into its
/// leaf-typed elements — the child array behind a `Vec<[T; N]>` field
/// (blueprint §9.1's `boxes: Vec<[f32; 4]>`).
fn container_leaf_flatten_iter(leaf: &ContainerLeaf, base: &TokenStream) -> TokenStream {
    finish_leaf_ref_iter(leaf, quote! { #base.iter().flatten() })
}

/// Decodes an already-downcast, already-bound container array (`array`) at
/// `index` into the leaf's owned Rust value, erroring with
/// [`astrs_data::DataError::RequiredFieldIsNull`] if that slot turns out to
/// be null.
fn container_leaf_get(
    leaf: &ContainerLeaf,
    array: &syn::Ident,
    index: &TokenStream,
    field_name: &str,
) -> TokenStream {
    let convert = match leaf {
        ContainerLeaf::Scalar(_) => quote! {},
        ContainerLeaf::String => quote! { .map(::std::borrow::ToOwned::to_owned) },
    };
    quote! {
        #array.get(#index) #convert .ok_or_else(|| __astrs_data::DataError::RequiredFieldIsNull {
            field: #field_name.to_owned(),
            row: #index,
        })?
    }
}

/// Decodes a whole container array (already downcast and bound as
/// `array`) into a `Vec` of the leaf's owned Rust values, checking every
/// element for an unexpected null.
fn decode_container_values(
    leaf: &ContainerLeaf,
    array: &syn::Ident,
    field_name: &str,
) -> TokenStream {
    let one = container_leaf_get(leaf, array, &quote! { __i }, field_name);
    quote! {
        {
            let mut __out = ::std::vec::Vec::with_capacity(#array.len());
            for __i in 0..#array.len() {
                __out.push(#one);
            }
            __out
        }
    }
}

/// Builds the encode expression for one field: a block, of type
/// `astrs_data::ArrayRef`, that may use `?` internally.
pub(crate) fn encode_field_expr(plan: &FieldPlan) -> TokenStream {
    let ident = &plan.ident;
    let self_field = quote! { self.#ident };

    if plan.nullable {
        // Matching `&self.field: &Option<Leaf>` binds `__value: &Leaf` by
        // match ergonomics, regardless of the leaf's own shape. That is
        // exactly what `encode_required` wants for `String`/`Nested`
        // (whose encode paths call a `&self` method, so a reference is
        // either already the right type or coerces to it) and for
        // `List`/`FixedArray` (whose `.len()`/`.iter()` calls resolve
        // identically through a reference or an owned place) — but a
        // `Copy` scalar's encode path builds `[value]` directly as the
        // one-element iterator `PrimitiveArray::from_values` wants, which
        // needs the *owned* value, not `&Leaf`; only that case needs an
        // explicit deref here.
        let value_expr = if matches!(
            &plan.shape,
            FieldShape::Leaf(LeafShape::Container(ContainerLeaf::Scalar(_)))
        ) {
            quote! { *__value }
        } else {
            quote! { __value }
        };
        let inner = encode_required(&plan.shape, &value_expr);
        let data_type = data_type_expr(&plan.shape);
        quote! {
            match &#self_field {
                ::core::option::Option::Some(__value) => #inner,
                ::core::option::Option::None => __astrs_data::array::new_null_array(&(#data_type), 1)?,
            }
        }
    } else {
        encode_required(&plan.shape, &self_field)
    }
}

/// Builds the encode expression for a non-nullable shape, given an
/// expression `value` bound to that shape's owned Rust value (either
/// `self.field` directly, or `__value` inside the `Option::Some` arm
/// [`encode_field_expr`] builds).
fn encode_required(shape: &FieldShape, value: &TokenStream) -> TokenStream {
    match shape {
        FieldShape::Leaf(LeafShape::Container(leaf @ ContainerLeaf::Scalar(_))) => {
            container_leaf_single_array(leaf, value)
        }
        FieldShape::Leaf(LeafShape::Container(ContainerLeaf::String)) => {
            let string_value = quote! { #value.as_str() };
            container_leaf_single_array(&ContainerLeaf::String, &string_value)
        }
        FieldShape::Leaf(LeafShape::Nested(path)) => quote! {
            {
                let __nested_batch = <#path as __astrs_data::AstrsMessage>::to_record_batch(&#value)?;
                match __nested_batch.payload_column() {
                    ::core::option::Option::Some(__col) => ::std::clone::Clone::clone(__col),
                    ::core::option::Option::None => {
                        return ::core::result::Result::Err(__astrs_data::DataError::ColumnCountMismatch {
                            fields: 1,
                            columns: 0,
                        });
                    }
                }
            }
        },
        FieldShape::List(element) => encode_list(element, value),
        FieldShape::FixedArray(leaf, len) => encode_fixed_array(leaf, *len, value),
    }
}

/// Builds the encode expression for a `Vec<_>` field's value.
fn encode_list(element: &ListElement, value: &TokenStream) -> TokenStream {
    let len_expr = quote! { #value.len() };
    match element {
        ListElement::Leaf(leaf) => {
            let iter = container_leaf_iter(leaf, value);
            let child = container_leaf_child_array(leaf, &iter);
            let item = container_leaf_data_type(leaf);
            quote! {
                __astrs_data::array::IntoArrayRef::into_array_ref(__astrs_data::array::ListArray::try_from_lengths(
                    __astrs_data::Field::required("item", #item),
                    [#len_expr],
                    #child,
                )?)
            }
        }
        ListElement::FixedArray(leaf, len) => {
            let iter = container_leaf_flatten_iter(leaf, value);
            let child = container_leaf_child_array(leaf, &iter);
            let leaf_item = container_leaf_data_type(leaf);
            let len_lit = proc_macro2::Literal::usize_unsuffixed(*len);
            let row_field = fixed_array_data_type(leaf, *len);
            quote! {
                __astrs_data::array::IntoArrayRef::into_array_ref(__astrs_data::array::ListArray::try_from_lengths(
                    __astrs_data::Field::required("item", #row_field),
                    [#len_expr],
                    __astrs_data::array::IntoArrayRef::into_array_ref(__astrs_data::array::FixedSizeListArray::try_new(
                        __astrs_data::Field::required("item", #leaf_item),
                        #len_lit as i32,
                        #child,
                        ::core::option::Option::None,
                    )?),
                )?)
            }
        }
    }
}

/// Builds the encode expression for a `[T; N]` field's value.
fn encode_fixed_array(leaf: &ContainerLeaf, len: usize, value: &TokenStream) -> TokenStream {
    let iter = container_leaf_iter(leaf, value);
    let child = container_leaf_child_array(leaf, &iter);
    let item_field = container_leaf_data_type(leaf);
    let len_lit = proc_macro2::Literal::usize_unsuffixed(len);
    quote! {
        __astrs_data::array::IntoArrayRef::into_array_ref(__astrs_data::array::FixedSizeListArray::try_new(
            __astrs_data::Field::required("item", #item_field),
            #len_lit as i32,
            #child,
            ::core::option::Option::None,
        )?)
    }
}

/// Builds the decode expression for one field: an expression, of the
/// field's own Rust type (`Option<_>` when nullable), that may use `?`
/// internally.
///
/// `column` is a plain identifier already bound to `&astrs_data::ArrayRef`
/// (see [`crate::derive_message`]) — never a compound expression, so every
/// generated `#column.method()` call needs no extra parenthesization.
pub(crate) fn decode_field_expr(plan: &FieldPlan, column: &syn::Ident) -> TokenStream {
    let name = plan.ident.to_string();
    if plan.nullable {
        decode_nullable(&plan.shape, column, &name)
    } else {
        decode_required(&plan.shape, column, &name)
    }
}

fn decode_required(shape: &FieldShape, column: &syn::Ident, name: &str) -> TokenStream {
    match shape {
        FieldShape::Leaf(LeafShape::Container(leaf)) => {
            let array_ty = container_leaf_array_type(leaf);
            let row = syn::Ident::new("__row", proc_macro2::Span::call_site());
            let one = container_leaf_get(leaf, &row, &quote! { 0usize }, name);
            quote! {
                {
                    let __row = #column.try_downcast::<#array_ty>()?;
                    #one
                }
            }
        }
        FieldShape::Leaf(LeafShape::Nested(path)) => quote! {
            {
                if #column.is_null(0) {
                    return ::core::result::Result::Err(__astrs_data::DataError::RequiredFieldIsNull {
                        field: #name.to_owned(),
                        row: 0,
                    });
                }
                let __nested_batch = __astrs_data::RecordBatch::from_payload(::std::clone::Clone::clone(#column));
                <#path as __astrs_data::AstrsMessage>::from_record_batch(&__nested_batch)?
            }
        },
        FieldShape::List(element) => decode_list_required(element, column, name),
        FieldShape::FixedArray(leaf, len) => decode_fixed_array_required(leaf, *len, column, name),
    }
}

fn decode_nullable(shape: &FieldShape, column: &syn::Ident, name: &str) -> TokenStream {
    match shape {
        FieldShape::Leaf(LeafShape::Container(leaf)) => {
            let array_ty = container_leaf_array_type(leaf);
            let convert = match leaf {
                ContainerLeaf::Scalar(_) => quote! {},
                ContainerLeaf::String => quote! { .map(::std::borrow::ToOwned::to_owned) },
            };
            quote! { #column.try_downcast::<#array_ty>()?.get(0) #convert }
        }
        FieldShape::Leaf(LeafShape::Nested(path)) => quote! {
            if #column.is_null(0) {
                ::core::option::Option::None
            } else {
                let __nested_batch = __astrs_data::RecordBatch::from_payload(::std::clone::Clone::clone(#column));
                ::core::option::Option::Some(<#path as __astrs_data::AstrsMessage>::from_record_batch(&__nested_batch)?)
            }
        },
        FieldShape::List(element) => decode_list_nullable(element, column, name),
        FieldShape::FixedArray(leaf, len) => decode_fixed_array_nullable(leaf, *len, column, name),
    }
}

fn decode_list_required(element: &ListElement, column: &syn::Ident, name: &str) -> TokenStream {
    let row_decode = list_row_decode(element, name);
    quote! {
        {
            let __list = #column.try_downcast::<__astrs_data::array::ListArray>()?;
            let __row = __list.get(0).ok_or_else(|| __astrs_data::DataError::RequiredFieldIsNull {
                field: #name.to_owned(),
                row: 0,
            })?;
            #row_decode
        }
    }
}

fn decode_list_nullable(element: &ListElement, column: &syn::Ident, name: &str) -> TokenStream {
    let row_decode = list_row_decode(element, name);
    quote! {
        {
            let __list = #column.try_downcast::<__astrs_data::array::ListArray>()?;
            match __list.get(0) {
                ::core::option::Option::Some(__row) => ::core::option::Option::Some({ #row_decode }),
                ::core::option::Option::None => ::core::option::Option::None,
            }
        }
    }
}

/// Decodes `__row` (an `ArrayRef` already bound by the caller — either the
/// list's row for [`ListElement::Leaf`], or the same for
/// [`ListElement::FixedArray`], one level up from the fixed-size rows
/// themselves) into the list element's owned Rust value: a
/// `Vec<Scalar>`/`Vec<String>` for [`ListElement::Leaf`], or a `Vec<[T;
/// N]>` for [`ListElement::FixedArray`].
fn list_row_decode(element: &ListElement, name: &str) -> TokenStream {
    match element {
        ListElement::Leaf(leaf) => {
            let array_ty = container_leaf_array_type(leaf);
            let values_ident = syn::Ident::new("__values", proc_macro2::Span::call_site());
            let values = decode_container_values(leaf, &values_ident, name);
            quote! {
                {
                    let __values = __row.try_downcast::<#array_ty>()?;
                    #values
                }
            }
        }
        ListElement::FixedArray(leaf, len) => {
            let array_ty = container_leaf_array_type(leaf);
            let rust_ty = container_leaf_rust_type(leaf);
            let len_lit = proc_macro2::Literal::usize_unsuffixed(*len);
            let values_ident = syn::Ident::new("__values", proc_macro2::Span::call_site());
            let values_expr = decode_container_values(leaf, &values_ident, name);
            quote! {
                {
                    let __fsl = __row.try_downcast::<__astrs_data::array::FixedSizeListArray>()?;
                    let mut __rows = ::std::vec::Vec::with_capacity(__fsl.len());
                    for __row_index in 0..__fsl.len() {
                        let __elem = __fsl.get(__row_index).ok_or_else(|| __astrs_data::DataError::RequiredFieldIsNull {
                            field: #name.to_owned(),
                            row: __row_index,
                        })?;
                        let __values = __elem.try_downcast::<#array_ty>()?;
                        let __buf: ::std::vec::Vec<#rust_ty> = #values_expr;
                        let __array: [#rust_ty; #len_lit] = ::core::convert::TryFrom::try_from(__buf.as_slice())
                            .map_err(|_| __astrs_data::DataError::MessageFixedArrayLength {
                                field: #name.to_owned(),
                                expected: #len_lit,
                                actual: __buf.len(),
                            })?;
                        __rows.push(__array);
                    }
                    __rows
                }
            }
        }
    }
}

fn decode_fixed_array_required(
    leaf: &ContainerLeaf,
    len: usize,
    column: &syn::Ident,
    name: &str,
) -> TokenStream {
    let array_ty = container_leaf_array_type(leaf);
    let rust_ty = container_leaf_rust_type(leaf);
    let len_lit = proc_macro2::Literal::usize_unsuffixed(len);
    let values_ident = syn::Ident::new("__values", proc_macro2::Span::call_site());
    let values = decode_container_values(leaf, &values_ident, name);
    quote! {
        {
            let __fsl = #column.try_downcast::<__astrs_data::array::FixedSizeListArray>()?;
            let __row = __fsl.get(0).ok_or_else(|| __astrs_data::DataError::RequiredFieldIsNull {
                field: #name.to_owned(),
                row: 0,
            })?;
            let __values = __row.try_downcast::<#array_ty>()?;
            let __buf: ::std::vec::Vec<#rust_ty> = #values;
            let __array: [#rust_ty; #len_lit] = ::core::convert::TryFrom::try_from(__buf.as_slice())
                .map_err(|_| __astrs_data::DataError::MessageFixedArrayLength {
                    field: #name.to_owned(),
                    expected: #len_lit,
                    actual: __buf.len(),
                })?;
            __array
        }
    }
}

fn decode_fixed_array_nullable(
    leaf: &ContainerLeaf,
    len: usize,
    column: &syn::Ident,
    name: &str,
) -> TokenStream {
    let array_ty = container_leaf_array_type(leaf);
    let rust_ty = container_leaf_rust_type(leaf);
    let len_lit = proc_macro2::Literal::usize_unsuffixed(len);
    let values_ident = syn::Ident::new("__values", proc_macro2::Span::call_site());
    let values = decode_container_values(leaf, &values_ident, name);
    quote! {
        {
            let __fsl = #column.try_downcast::<__astrs_data::array::FixedSizeListArray>()?;
            match __fsl.get(0) {
                ::core::option::Option::Some(__row) => {
                    let __values = __row.try_downcast::<#array_ty>()?;
                    let __buf: ::std::vec::Vec<#rust_ty> = #values;
                    let __array: [#rust_ty; #len_lit] = ::core::convert::TryFrom::try_from(__buf.as_slice())
                        .map_err(|_| __astrs_data::DataError::MessageFixedArrayLength {
                            field: #name.to_owned(),
                            expected: #len_lit,
                            actual: __buf.len(),
                        })?;
                    ::core::option::Option::Some(__array)
                }
                ::core::option::Option::None => ::core::option::Option::None,
            }
        }
    }
}
