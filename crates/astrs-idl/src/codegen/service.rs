//! `.srv` codegen: a request struct and a response struct, named
//! `<Srv>Request`/`<Srv>Response` in Rust and `<Srv>_Request`/`<Srv>_Response`
//! (`rosidl`'s own convention) in `ROS_TYPE_NAME`/`DDS_TYPE_NAME`.
//!
//! Blueprint §10.3 does not literally require a service *marker* type (the
//! request and response are complete, independent `AstrsMessage`s on their
//! own), so none is emitted — a caller pairs `FooRequest`/`FooResponse`
//! itself, the same way `astrs_cdr`'s own worked example pairs a service's
//! two message halves.

use proc_macro2::TokenStream;
use quote::quote;

use crate::ast::ServiceFile;
use crate::error::IdlError;
use crate::naming::TypeName;
use crate::resolve::TypeUniverse;

/// Generates a `.srv` file's two types.
///
/// # Errors
///
/// The same as [`super::generate_message`].
pub fn generate_service(
    type_name: &TypeName,
    file: &ServiceFile,
    universe: &TypeUniverse,
) -> Result<TokenStream, IdlError> {
    let request = super::build_struct_spec(
        &type_name.package,
        &format!("{}Request", type_name.name),
        format!("{}_Request", type_name.full()),
        format!("{}_Request_", super::dds_prefix(type_name)),
        &file.request,
        universe,
    )?;
    let response = super::build_struct_spec(
        &type_name.package,
        &format!("{}Response", type_name.name),
        format!("{}_Response", type_name.full()),
        format!("{}_Response_", super::dds_prefix(type_name)),
        &file.response,
        universe,
    )?;
    let request_tokens = super::emit::emit_struct(&request);
    let response_tokens = super::emit::emit_struct(&response);
    Ok(quote! {
        #request_tokens
        #response_tokens
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codegen::format_module;
    use crate::naming::{InterfaceKind, PackageName};
    use crate::parser::parse_service;
    use crate::span::{Position, Span};

    #[test]
    fn a_service_generates_request_and_response_types() {
        let package =
            PackageName::new("example_interfaces", Span::empty(Position::new(1, 1))).unwrap();
        let type_name = TypeName::new(
            package,
            InterfaceKind::Srv,
            "AddTwoInts",
            Span::empty(Position::new(1, 1)),
        )
        .unwrap();
        let file = parse_service("int64 a\nint64 b\n---\nint64 sum\n").unwrap();
        let universe = TypeUniverse::new();
        let tokens = generate_service(&type_name, &file, &universe).unwrap();
        let text = format_module("// banner", tokens).unwrap();
        assert!(text.contains("pub struct AddTwoIntsRequest {"), "{text}");
        assert!(text.contains("pub struct AddTwoIntsResponse {"), "{text}");
        assert!(text.contains("pub a: i64,"), "{text}");
        assert!(text.contains("pub sum: i64,"), "{text}");
        assert!(
            text.contains(
                r#"ROS_TYPE_NAME: &'static str = "example_interfaces/srv/AddTwoInts_Request""#
            ),
            "{text}"
        );
        assert!(
            text.contains(
                r#"DDS_TYPE_NAME: &'static str = "example_interfaces::srv::dds_::AddTwoInts_Request_""#
            ),
            "{text}"
        );
    }

    #[test]
    fn an_empty_request_gets_the_placeholder_field() {
        let package = PackageName::new("std_srvs", Span::empty(Position::new(1, 1))).unwrap();
        let type_name = TypeName::new(
            package,
            InterfaceKind::Srv,
            "Empty",
            Span::empty(Position::new(1, 1)),
        )
        .unwrap();
        let file = parse_service("---\n").unwrap();
        let universe = TypeUniverse::new();
        let tokens = generate_service(&type_name, &file, &universe).unwrap();
        let text = format_module("// banner", tokens).unwrap();
        assert!(
            text.matches("pub structure_needs_at_least_one_member: u8,")
                .count()
                == 2,
            "both Request and Response should get the placeholder: {text}"
        );
    }
}
