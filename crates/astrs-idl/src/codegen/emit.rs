//! Assembles one message-shaped type's `TokenStream`: the struct
//! definition, its `ROS_TYPE_NAME`/`DDS_TYPE_NAME`/user constants, and the
//! five trait impls every generated type carries —
//! `astrs_idl::runtime::ColumnValue`, `astrs_data::AstrsMessage`,
//! `astrs_cdr::{CdrType,CdrSerialize,CdrDeserialize}` (one block),
//! `astrs_cdr::CdrDefault` + `::core::default::Default`.
//!
//! # `#[rustfmt::skip]` on every item
//!
//! `super::format_module`'s own docs explain why a single whole-module
//! `#![rustfmt::skip]` cannot cover a generated file (`E0658`: only a crate
//! root accepts a custom inner attribute, and every file under
//! [`crate::generated`] is loaded via `mod`). The fix lives here instead:
//! every top-level item [`emit_struct`] hands to `quote!` — the struct
//! definition and each `impl` block, nine per generated type — carries its
//! own outer `#[rustfmt::skip]`, which `rustc` accepts at any nesting depth.
//! `prettyplease`'s own formatting of the item is unaffected (`rustfmt` is
//! never invoked in this crate's own pipeline at all, see
//! `super::format_module`); the attribute only tells a *subsequent* human
//! run of `cargo fmt` to leave the `@generated` item exactly as
//! `prettyplease` rendered it, matching the banner's "do not edit by hand"
//! promise instead of silently reformatting it out of sync with the drift
//! guard.
//!
//! # One field list, three consumers
//!
//! [`ColumnValue::value_data_type`]'s `Vec<Field>` is built exactly once
//! ([`build_fields_expr`]) and referenced — never re-derived — by
//! `encode_column` (`Self::value_data_type()`, destructured) and by
//! `decode_row` (positional [`astrs_idl::runtime::struct_column`] access, so
//! it does not even need the field list). A second, independently-written
//! copy of "what are this struct's columns" is exactly the bug class the
//! `unreachable!()`-avoiding total-match pattern below exists to make
//! impossible to introduce by accident: if `value_data_type()` and the
//! columns `encode_column` actually builds ever disagreed, this shape makes
//! that a contradiction within one function rather than a runtime mismatch
//! between two.
//!
//! # Why `MessageFixedArrayLength` for a bound violation
//!
//! `astrs_data::DataError` has no "bound exceeded" variant — bounds are an
//! `astrs_cdr` concept (`CdrError::BoundExceeded`), and `astrs_data` sits
//! below `astrs_cdr` in the layer stack (blueprint §4.1), so it cannot know
//! about it. `MessageFixedArrayLength` ("decoded N element(s), expected the
//! layout's M") is the closest existing fit for "this field's data doesn't
//! match its static size contract" and is reused here for the columnar
//! decode path's defensive bound re-check (a foreign `RecordBatch` is not
//! obliged to respect a bound `astrs_cdr::Bounded*`'s constructor already
//! enforces for any value this crate produced itself) — see
//! `astrs_idl::runtime`'s own module docs for the encode-side mirror of this
//! decision.

use proc_macro2::{Ident, TokenStream};
use quote::quote;

use super::doc_attr;
use super::fields::{ConstantShape, FieldShape, ResolvedConstant, ResolvedField, unsuffixed};

/// Everything needed to emit one message-shaped Rust type.
pub(crate) struct StructSpec {
    pub(crate) type_ident: Ident,
    pub(crate) struct_doc: String,
    /// The minted `std/ros2/v1/...` URN (`crate::naming::mint_urn`).
    pub(crate) urn: String,
    /// `"pkg/kind/Type"`.
    pub(crate) ros_type_name: String,
    /// `"pkg::kind::dds_::Type_"`.
    pub(crate) dds_type_name: String,
    /// Never empty — a zero-field section is represented by
    /// [`super::fields::placeholder_field`] before this is called.
    pub(crate) fields: Vec<ResolvedField>,
    pub(crate) constants: Vec<ResolvedConstant>,
}

/// Emits the struct definition, its constants, and all five trait impls.
pub(crate) fn emit_struct(spec: &StructSpec) -> TokenStream {
    let struct_def = emit_struct_def(&spec.type_ident, &spec.struct_doc, &spec.fields);
    let naming_and_constants = emit_naming_and_constants(
        &spec.type_ident,
        &spec.ros_type_name,
        &spec.dds_type_name,
        &spec.constants,
    );
    let column_value = emit_column_value_impl(&spec.type_ident, &spec.fields);
    let astrs_message = emit_astrs_message_impl(&spec.type_ident, &spec.urn, &spec.ros_type_name);
    let cdr = emit_cdr_impl(&spec.type_ident, &spec.fields);
    let defaults = emit_default_impl(&spec.type_ident, &spec.fields);
    quote! {
        #struct_def
        #naming_and_constants
        #column_value
        #astrs_message
        #cdr
        #defaults
    }
}

fn emit_struct_def(type_ident: &Ident, doc: &str, fields: &[ResolvedField]) -> TokenStream {
    let struct_doc = doc_attr(doc);
    let field_decls = fields.iter().map(|field| {
        let field_doc = doc_attr(&field.doc);
        let ident = &field.ident;
        let rust_type = &field.rust_type;
        quote! {
            #field_doc
            pub #ident: #rust_type,
        }
    });
    quote! {
        #[rustfmt::skip]
        #[derive(Debug, Clone, PartialEq)]
        #struct_doc
        pub struct #type_ident {
            #(#field_decls)*
        }
    }
}

fn emit_naming_and_constants(
    type_ident: &Ident,
    ros_type_name: &str,
    dds_type_name: &str,
    constants: &[ResolvedConstant],
) -> TokenStream {
    let ros_doc = doc_attr(&format!("ROS 2 type name: `{ros_type_name}`."));
    let dds_doc = doc_attr(
        "The DDS-mangled type name (`rosidl`'s own convention) SEDP discovery announcements \
         carry for this type.",
    );
    let const_items = constants.iter().map(|constant| {
        let item_doc = doc_attr(&constant.doc);
        let ident = &constant.ident;
        let rust_type = &constant.rust_type;
        let value_expr = &constant.value_expr;
        match constant.shape {
            ConstantShape::Const => quote! {
                #item_doc
                pub const #ident: #rust_type = #value_expr;
            },
            ConstantShape::Function => quote! {
                #item_doc
                #[must_use]
                pub fn #ident() -> #rust_type {
                    #value_expr
                }
            },
        }
    });
    quote! {
        #[rustfmt::skip]
        impl #type_ident {
            #ros_doc
            pub const ROS_TYPE_NAME: &'static str = #ros_type_name;
            #dds_doc
            pub const DDS_TYPE_NAME: &'static str = #dds_type_name;
            #(#const_items)*
        }
    }
}

/// The `astrs_data::DataType` expression for one field's own column,
/// dispatching on its array shape and (for `Unbounded`/`Fixed` only —
/// `Bounded` is deliberately uniform, see `astrs_idl::runtime`'s module
/// docs) whether its element is an octet.
fn field_data_type_expr(field: &ResolvedField) -> TokenStream {
    let element = &field.element_type;
    match (field.shape, field.is_octet_element) {
        (FieldShape::Scalar, _) => quote! {
            <#element as astrs_idl::runtime::ColumnValue>::value_data_type()
        },
        (FieldShape::Unbounded, true) => quote! {
            <::std::vec::Vec<u8> as astrs_idl::runtime::ColumnValue>::value_data_type()
        },
        (FieldShape::Unbounded, false) => quote! {
            astrs_data::DataType::list(astrs_data::Field::required(
                astrs_idl::runtime::ITEM_FIELD,
                <#element as astrs_idl::runtime::ColumnValue>::value_data_type(),
            ))
        },
        (FieldShape::Fixed(n), true) => {
            let n = unsuffixed(n);
            quote! { <[u8; #n] as astrs_idl::runtime::ColumnValue>::value_data_type() }
        }
        (FieldShape::Fixed(n), false) => {
            let size = i32::try_from(n).unwrap_or(i32::MAX);
            quote! {
                astrs_data::DataType::fixed_size_list(
                    astrs_data::Field::required(
                        astrs_idl::runtime::ITEM_FIELD,
                        <#element as astrs_idl::runtime::ColumnValue>::value_data_type(),
                    ),
                    #size,
                )
            }
        }
        (FieldShape::Bounded(_), _) => quote! {
            astrs_data::DataType::list(astrs_data::Field::required(
                astrs_idl::runtime::ITEM_FIELD,
                <#element as astrs_idl::runtime::ColumnValue>::value_data_type(),
            ))
        },
    }
}

fn build_fields_expr(fields: &[ResolvedField]) -> TokenStream {
    let entries = fields.iter().map(|field| {
        let name = &field.ros_name;
        let data_type_expr = field_data_type_expr(field);
        quote! { astrs_data::Field::required(#name, #data_type_expr) }
    });
    quote! { ::std::vec![#(#entries),*] }
}

/// The `encode_column` expression for one field, evaluating to
/// `astrs_data::array::ArrayRef` (the trailing `?` already applied) given
/// `values: &[Self]` in scope.
fn field_encode_expr(field: &ResolvedField) -> TokenStream {
    let ident = &field.ident;
    let element = &field.element_type;
    // `clippy::clone_on_copy` (this workspace's `-D warnings` gate) rejects
    // `.clone()` on a `Copy` value, so a collected-by-value field reads
    // `v.field` (a `Copy` out of `&Self`) when it can and `v.field.clone()`
    // only when it must. `[u8; N]` is unconditionally `Copy` (an array of
    // `Copy` is `Copy` for any `N`), independent of `field.is_copy` (which
    // tracks the *element* type — irrelevant here since the collected item
    // *is* the array); `Vec<u8>` is never `Copy` regardless.
    let collected_by_value = |field_access: TokenStream| {
        if field.is_copy {
            field_access
        } else {
            quote! { #field_access.clone() }
        }
    };
    match (field.shape, field.is_octet_element) {
        (FieldShape::Scalar, _) => {
            let item = collected_by_value(quote! { v.#ident });
            quote! {
                <#element as astrs_idl::runtime::ColumnValue>::encode_column(
                    &values.iter().map(|v| #item).collect::<::std::vec::Vec<_>>(),
                )?
            }
        }
        (FieldShape::Unbounded, true) => quote! {
            <::std::vec::Vec<u8> as astrs_idl::runtime::ColumnValue>::encode_column(
                &values.iter().map(|v| v.#ident.clone()).collect::<::std::vec::Vec<_>>(),
            )?
        },
        (FieldShape::Unbounded, false) => quote! {
            astrs_idl::runtime::encode_list_rows(values.iter().map(|v| v.#ident.as_slice()))?
        },
        (FieldShape::Fixed(n), true) => {
            let n = unsuffixed(n);
            quote! {
                <[u8; #n] as astrs_idl::runtime::ColumnValue>::encode_column(
                    &values.iter().map(|v| v.#ident).collect::<::std::vec::Vec<_>>(),
                )?
            }
        }
        (FieldShape::Fixed(_), false) => quote! {
            astrs_idl::runtime::encode_fixed_list_rows(values.iter().map(|v| &v.#ident))?
        },
        (FieldShape::Bounded(_), _) => quote! {
            astrs_idl::runtime::encode_bounded_list_rows(values.iter().map(|v| &v.#ident))?
        },
    }
}

/// The `decode_row` expression for one field, evaluating to the field's own
/// Rust type (the trailing `?` already applied) given `column: &ArrayRef`
/// (already positioned at this field) and `row: usize` in scope.
fn field_decode_expr(field: &ResolvedField) -> TokenStream {
    let element = &field.element_type;
    let ros_name = &field.ros_name;
    match (field.shape, field.is_octet_element) {
        (FieldShape::Scalar, _) => quote! {
            <#element as astrs_idl::runtime::ColumnValue>::decode_row(column, row, #ros_name)?
        },
        (FieldShape::Unbounded, true) => quote! {
            <::std::vec::Vec<u8> as astrs_idl::runtime::ColumnValue>::decode_row(column, row, #ros_name)?
        },
        (FieldShape::Unbounded, false) => quote! {
            astrs_idl::runtime::decode_list_column(column, row, #ros_name)?
        },
        (FieldShape::Fixed(n), true) => {
            let n = unsuffixed(n);
            quote! { <[u8; #n] as astrs_idl::runtime::ColumnValue>::decode_row(column, row, #ros_name)? }
        }
        (FieldShape::Fixed(_), false) => quote! {
            astrs_idl::runtime::decode_fixed_list_column(column, row, #ros_name)?
        },
        (FieldShape::Bounded(_), _) => quote! {
            astrs_idl::runtime::decode_bounded_list_column(column, row, #ros_name)?
        },
    }
}

fn emit_column_value_impl(type_ident: &Ident, fields: &[ResolvedField]) -> TokenStream {
    let fields_expr = build_fields_expr(fields);
    let field_count = fields.len();
    let encode_entries = fields.iter().map(field_encode_expr);

    let decode_lets = fields.iter().enumerate().map(|(index, field)| {
        let ident = &field.ident;
        let rust_type = &field.rust_type;
        let decode_expr = field_decode_expr(field);
        quote! {
            let #ident: #rust_type = {
                let column = astrs_idl::runtime::struct_column(array, #index, #field_count)?;
                #decode_expr
            };
        }
    });
    let field_idents = fields.iter().map(|field| &field.ident);

    quote! {
        #[rustfmt::skip]
        impl astrs_idl::runtime::ColumnValue for #type_ident {
            fn value_data_type() -> astrs_data::DataType {
                astrs_data::DataType::strukt(#fields_expr)
            }

            fn encode_column(values: &[Self]) -> astrs_data::Result<astrs_data::array::ArrayRef> {
                let columns: ::std::vec::Vec<astrs_data::array::ArrayRef> =
                    ::std::vec![#(#encode_entries),*];
                let astrs_data::DataType::Struct(fields) = <Self as astrs_idl::runtime::ColumnValue>::value_data_type() else {
                    return ::std::result::Result::Err(astrs_data::DataError::type_mismatch(
                        <Self as astrs_idl::runtime::ColumnValue>::value_data_type(),
                        astrs_data::DataType::Null,
                    ));
                };
                let strukt = astrs_data::array::StructArray::try_new_with_len(
                    fields,
                    columns,
                    values.len(),
                    ::std::option::Option::None,
                )?;
                ::std::result::Result::Ok(<astrs_data::array::StructArray as astrs_data::array::IntoArrayRef>::into_array_ref(strukt))
            }

            fn decode_row(
                array: &astrs_data::array::ArrayRef,
                row: usize,
                field_name: &'static str,
            ) -> astrs_data::Result<Self> {
                let _ = field_name;
                #(#decode_lets)*
                ::std::result::Result::Ok(Self { #(#field_idents),* })
            }
        }
    }
}

fn emit_astrs_message_impl(type_ident: &Ident, urn: &str, ros_type_name: &str) -> TokenStream {
    quote! {
        #[rustfmt::skip]
        impl astrs_data::AstrsMessage for #type_ident {
            const URN: &'static str = #urn;

            fn data_type() -> astrs_data::DataType {
                <Self as astrs_idl::runtime::ColumnValue>::value_data_type()
            }

            fn to_record_batch(&self) -> astrs_data::Result<astrs_data::RecordBatch> {
                let array = <Self as astrs_idl::runtime::ColumnValue>::encode_column(
                    ::std::slice::from_ref(self),
                )?;
                ::std::result::Result::Ok(astrs_data::RecordBatch::from_payload(array))
            }

            fn from_record_batch(batch: &astrs_data::RecordBatch) -> astrs_data::Result<Self> {
                if batch.num_rows() != 1 {
                    return ::std::result::Result::Err(astrs_data::DataError::MessageRowCount {
                        actual: batch.num_rows(),
                    });
                }
                let column = batch
                    .payload_column()
                    .ok_or(astrs_data::DataError::ColumnCountMismatch { fields: 1, columns: 0 })?;
                <Self as astrs_idl::runtime::ColumnValue>::decode_row(column, 0, #ros_type_name)
            }
        }
    }
}

fn emit_cdr_impl(type_ident: &Ident, fields: &[ResolvedField]) -> TokenStream {
    let size_types = fields.iter().map(|field| &field.rust_type);
    let serialize_idents = fields.iter().map(|field| &field.ident);
    let deserialize_idents = fields.iter().map(|field| &field.ident);
    let deserialize_types = fields.iter().map(|field| &field.rust_type);

    quote! {
        #[rustfmt::skip]
        impl astrs_cdr::CdrType for #type_ident {
            const MIN_SERIALIZED_SIZE: usize = 0usize
                #(.saturating_add(<#size_types as astrs_cdr::CdrType>::MIN_SERIALIZED_SIZE))*;
        }

        #[rustfmt::skip]
        impl astrs_cdr::CdrSerialize for #type_ident {
            fn serialize(&self, writer: &mut astrs_cdr::CdrWriter) -> astrs_cdr::CdrResult<()> {
                writer.write_struct::<Self, ()>(|writer| {
                    #(astrs_cdr::CdrSerialize::serialize(&self.#serialize_idents, writer)?;)*
                    ::std::result::Result::Ok(())
                })
            }
        }

        #[rustfmt::skip]
        impl<'de> astrs_cdr::CdrDeserialize<'de> for #type_ident {
            fn deserialize(reader: &mut astrs_cdr::CdrReader<'de>) -> astrs_cdr::CdrResult<Self> {
                reader.read_struct::<Self, Self>(|reader| {
                    ::std::result::Result::Ok(Self {
                        #(#deserialize_idents: <#deserialize_types as astrs_cdr::CdrDeserialize<'de>>::deserialize(reader)?,)*
                    })
                })
            }
        }
    }
}

fn emit_default_impl(type_ident: &Ident, fields: &[ResolvedField]) -> TokenStream {
    let idents = fields.iter().map(|field| &field.ident);
    let default_exprs = fields.iter().map(|field| &field.default_expr);
    quote! {
        #[rustfmt::skip]
        impl astrs_cdr::CdrDefault for #type_ident {
            fn cdr_default() -> Self {
                Self {
                    #(#idents: #default_exprs,)*
                }
            }
        }

        #[rustfmt::skip]
        impl ::core::default::Default for #type_ident {
            fn default() -> Self {
                <Self as astrs_cdr::CdrDefault>::cdr_default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codegen::fields::placeholder_field;
    use crate::codegen::format_module;
    use quote::quote as q;

    fn ident(name: &str) -> Ident {
        Ident::new(name, proc_macro2::Span::call_site())
    }

    fn scalar_field(
        name: &str,
        rust_type: TokenStream,
        default_expr: TokenStream,
    ) -> ResolvedField {
        ResolvedField {
            ident: ident(name),
            ros_name: name.to_owned(),
            doc: format!("The `{name}` field."),
            rust_type: rust_type.clone(),
            element_type: rust_type,
            shape: FieldShape::Scalar,
            is_octet_element: false,
            is_copy: true,
            default_expr,
        }
    }

    #[test]
    fn a_simple_struct_formats_as_valid_rust() {
        let spec = StructSpec {
            type_ident: ident("Point"),
            struct_doc: "`geometry_msgs/msg/Point`.".to_owned(),
            urn: "std/ros2/v1/GeometryMsgsPoint".to_owned(),
            ros_type_name: "geometry_msgs/msg/Point".to_owned(),
            dds_type_name: "geometry_msgs::msg::dds_::Point_".to_owned(),
            fields: vec![
                scalar_field("x", q!(f64), q!(0f64)),
                scalar_field("y", q!(f64), q!(0f64)),
            ],
            constants: Vec::new(),
        };
        let tokens = emit_struct(&spec);
        let text = format_module("// banner", tokens).unwrap();
        assert!(text.contains("pub struct Point {"), "{text}");
        assert!(text.contains("pub x: f64,"), "{text}");
        assert!(
            text.contains("impl astrs_idl::runtime::ColumnValue for Point"),
            "{text}"
        );
        assert!(
            text.contains("impl astrs_data::AstrsMessage for Point"),
            "{text}"
        );
        assert!(text.contains("impl astrs_cdr::CdrType for Point"), "{text}");
        assert!(
            text.contains("impl astrs_cdr::CdrSerialize for Point"),
            "{text}"
        );
        assert!(
            text.contains("impl<'de> astrs_cdr::CdrDeserialize<'de> for Point"),
            "{text}"
        );
        assert!(
            text.contains("impl astrs_cdr::CdrDefault for Point"),
            "{text}"
        );
        assert!(
            text.contains("impl ::core::default::Default for Point"),
            "{text}"
        );
        assert!(
            text.contains(r#"const URN: &'static str = "std/ros2/v1/GeometryMsgsPoint""#),
            "{text}"
        );
        assert!(
            text.contains(r#"const ROS_TYPE_NAME: &'static str = "geometry_msgs/msg/Point""#),
            "{text}"
        );
        // Nine items (the struct definition, the naming/constants impl, and
        // the seven trait impls below `astrs_data`/`astrs_cdr`) each carry
        // their own `#[rustfmt::skip]` — see this module's own docs for why
        // a single whole-file `#![rustfmt::skip]` cannot substitute.
        assert_eq!(
            text.matches("#[rustfmt::skip]").count(),
            9,
            "every emitted item must opt out of cargo fmt individually: {text}"
        );
    }

    #[test]
    fn a_struct_with_the_placeholder_field_formats_as_valid_rust() {
        let spec = StructSpec {
            type_ident: ident("Empty"),
            struct_doc: "`std_srvs/srv/Empty`.".to_owned(),
            urn: "std/ros2/v1/StdSrvsEmptyRequest".to_owned(),
            ros_type_name: "std_srvs/srv/EmptyRequest".to_owned(),
            dds_type_name: "std_srvs::srv::dds_::Empty_Request_".to_owned(),
            fields: vec![placeholder_field()],
            constants: Vec::new(),
        };
        let tokens = emit_struct(&spec);
        let text = format_module("// banner", tokens).unwrap();
        assert!(
            text.contains("pub structure_needs_at_least_one_member: u8,"),
            "{text}"
        );
    }

    #[test]
    fn a_const_and_a_function_shaped_constant_both_format() {
        let spec = StructSpec {
            type_ident: ident("NavSatStatus"),
            struct_doc: "doc".to_owned(),
            urn: "std/ros2/v1/SensorMsgsNavSatStatus".to_owned(),
            ros_type_name: "sensor_msgs/msg/NavSatStatus".to_owned(),
            dds_type_name: "sensor_msgs::msg::dds_::NavSatStatus_".to_owned(),
            fields: vec![scalar_field("status", q!(i8), q!(0i8))],
            constants: vec![
                ResolvedConstant {
                    ident: ident("STATUS_NO_FIX"),
                    doc: "`STATUS_NO_FIX` constant.".to_owned(),
                    rust_type: q!(i8),
                    value_expr: q!(-1i8),
                    shape: ConstantShape::Const,
                },
                ResolvedConstant {
                    ident: ident("label"),
                    doc: "`LABEL` constant.".to_owned(),
                    rust_type: q!(::astrs_cdr::BoundedString<8>),
                    value_expr: q!(
                        ::astrs_cdr::BoundedString::<8usize>::new("fix").unwrap_or_default()
                    ),
                    shape: ConstantShape::Function,
                },
            ],
        };
        let tokens = emit_struct(&spec);
        let text = format_module("// banner", tokens).unwrap();
        assert!(
            text.contains("pub const STATUS_NO_FIX: i8 = -1i8;"),
            "{text}"
        );
        assert!(
            text.contains("pub fn label() -> ::astrs_cdr::BoundedString<8> {"),
            "{text}"
        );
    }

    #[test]
    fn every_array_shape_and_octet_dispatch_formats_as_valid_rust() {
        let mut fields = vec![scalar_field("id", q!(i32), q!(0i32))];
        fields.push(ResolvedField {
            ident: ident("data"),
            ros_name: "data".to_owned(),
            doc: "octet blob".to_owned(),
            rust_type: q!(::std::vec::Vec<u8>),
            element_type: q!(u8),
            shape: FieldShape::Unbounded,
            is_octet_element: true,
            is_copy: true,
            default_expr: q!(::astrs_cdr::CdrDefault::cdr_default()),
        });
        fields.push(ResolvedField {
            ident: ident("samples"),
            ros_name: "samples".to_owned(),
            doc: "unbounded non-octet".to_owned(),
            rust_type: q!(::std::vec::Vec<f64>),
            element_type: q!(f64),
            shape: FieldShape::Unbounded,
            is_octet_element: false,
            is_copy: true,
            default_expr: q!(::astrs_cdr::CdrDefault::cdr_default()),
        });
        fields.push(ResolvedField {
            ident: ident("uuid"),
            ros_name: "uuid".to_owned(),
            doc: "fixed octet".to_owned(),
            rust_type: q!([u8; 16usize]),
            element_type: q!(u8),
            shape: FieldShape::Fixed(16),
            is_octet_element: true,
            is_copy: true,
            default_expr: q!(::astrs_cdr::CdrDefault::cdr_default()),
        });
        fields.push(ResolvedField {
            ident: ident("xyz"),
            ros_name: "xyz".to_owned(),
            doc: "fixed non-octet".to_owned(),
            rust_type: q!([f64; 3usize]),
            element_type: q!(f64),
            shape: FieldShape::Fixed(3),
            is_octet_element: false,
            is_copy: true,
            default_expr: q!(::astrs_cdr::CdrDefault::cdr_default()),
        });
        fields.push(ResolvedField {
            ident: ident("bounded"),
            ros_name: "bounded".to_owned(),
            doc: "bounded".to_owned(),
            rust_type: q!(::astrs_cdr::BoundedSequence<i32, 4usize>),
            element_type: q!(i32),
            shape: FieldShape::Bounded(4),
            is_octet_element: false,
            is_copy: true,
            default_expr: q!(::astrs_cdr::CdrDefault::cdr_default()),
        });

        let spec = StructSpec {
            type_ident: ident("Kitchen"),
            struct_doc: "doc".to_owned(),
            urn: "std/ros2/v1/PkgKitchen".to_owned(),
            ros_type_name: "pkg/msg/Kitchen".to_owned(),
            dds_type_name: "pkg::msg::dds_::Kitchen_".to_owned(),
            fields,
            constants: Vec::new(),
        };
        let tokens = emit_struct(&spec);
        // The real assertion is that this does not return
        // `GeneratedTokensDidNotParse` — every shape/octet combination must
        // produce syntactically valid Rust.
        let text = format_module("// banner", tokens).unwrap();
        assert!(text.contains("pub data: ::std::vec::Vec<u8>,"), "{text}");
        assert!(text.contains("pub uuid: [u8; 16usize],"), "{text}");
        assert!(
            text.contains("pub bounded: ::astrs_cdr::BoundedSequence<i32, 4usize>,"),
            "{text}"
        );
    }
}
