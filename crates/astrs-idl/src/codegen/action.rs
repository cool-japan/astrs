//! `.action` codegen: the three declared types (`Goal`/`Result`/`Feedback`)
//! plus the five wire types ROS 2 actions synthesize around them —
//! `_SendGoal_Request`/`_Response`, `_GetResult_Request`/`_Response`, and
//! `_FeedbackMessage` (blueprint §10.3's "member ordering rules" applied to
//! action wiring, not just declared fields).
//!
//! ```text
//!   <Act>SendGoalRequest   = { goal_id: unique_identifier_msgs/UUID, goal: <Act>Goal }
//!   <Act>SendGoalResponse  = { accepted: bool, stamp: builtin_interfaces/Time }
//!   <Act>GetResultRequest  = { goal_id: unique_identifier_msgs/UUID }
//!   <Act>GetResultResponse = { status: int8, result: <Act>Result }
//!   <Act>FeedbackMessage   = { goal_id: unique_identifier_msgs/UUID, feedback: <Act>Feedback }
//! ```
//!
//! `unique_identifier_msgs/msg/UUID` and `builtin_interfaces/msg/Time` are
//! resolved through the *same* [`TypeUniverse`] lookup a hand-written
//! `.msg` field reference would use — the caller registers both packages
//! before generating any action, exactly as it would for any other
//! cross-package dependency; this module carries no special-cased path to
//! either.
//!
//! # Provenance
//!
//! The three declared sections' *fields* come from the parsed `.action`
//! file and are exercised against real interface content
//! ([`crate::generated`]'s `common_interfaces` set). The five synthesized
//! types' field *order* above is accurate to `rosidl`'s own convention to
//! the best of this implementation's knowledge, but — unlike
//! `common_interfaces` — could not be checked against a real ROS
//! installation or the §19.2 vendor references in this sandbox; treat it as
//! reviewed-not-verified and see the crate-level report for the same
//! caveat stated plainly.

use proc_macro2::{Ident, Span as MacroSpan, TokenStream};
use quote::quote;

use crate::ast::{ActionFile, NamedTypeRef};
use crate::error::IdlError;
use crate::naming::{PackageName, TypeName};
use crate::resolve::TypeUniverse;
use crate::span::{Position, Span};

use super::fields::{FieldShape, ResolvedField, rust_path_for};

/// Generates an `.action` file's eight types: `Goal`, `Result`, `Feedback`,
/// and the five synthesized wire types.
///
/// # Errors
///
/// The same as [`super::generate_message`], plus [`IdlError::UnknownType`]
/// if `unique_identifier_msgs/UUID` or `builtin_interfaces/Time` is not
/// registered in `universe`.
pub fn generate_action(
    type_name: &TypeName,
    file: &ActionFile,
    universe: &TypeUniverse,
) -> Result<TokenStream, IdlError> {
    let base = type_name.full();
    let dds_base = super::dds_prefix(type_name);
    let package = &type_name.package;
    let act = &type_name.name;

    let goal = super::build_struct_spec(
        package,
        &format!("{act}Goal"),
        format!("{base}_Goal"),
        format!("{dds_base}_Goal_"),
        &file.goal,
        universe,
    )?;
    let result = super::build_struct_spec(
        package,
        &format!("{act}Result"),
        format!("{base}_Result"),
        format!("{dds_base}_Result_"),
        &file.result,
        universe,
    )?;
    let feedback = super::build_struct_spec(
        package,
        &format!("{act}Feedback"),
        format!("{base}_Feedback"),
        format!("{dds_base}_Feedback_"),
        &file.feedback,
        universe,
    )?;

    let uuid = resolve_dependency(universe, "unique_identifier_msgs", "UUID", package)?;
    let time = resolve_dependency(universe, "builtin_interfaces", "Time", package)?;
    let goal_type = TypeName::new(
        package.clone(),
        type_name.kind,
        format!("{act}Goal"),
        Span::empty(Position::START),
    )?;
    let result_type = TypeName::new(
        package.clone(),
        type_name.kind,
        format!("{act}Result"),
        Span::empty(Position::START),
    )?;
    let feedback_type = TypeName::new(
        package.clone(),
        type_name.kind,
        format!("{act}Feedback"),
        Span::empty(Position::START),
    )?;

    let goal_id_field = || named_field("goal_id", &uuid, package, "The goal's unique identifier.");

    let send_goal_request = super::synthetic_struct_spec(
        package,
        &format!("{act}SendGoalRequest"),
        format!("{base}_SendGoal_Request"),
        format!("{dds_base}_SendGoal_Request_"),
        format!("The `SendGoal` service request for `{base}`."),
        vec![
            goal_id_field(),
            named_field("goal", &goal_type, package, "The goal itself."),
        ],
    )?;
    let send_goal_response = super::synthetic_struct_spec(
        package,
        &format!("{act}SendGoalResponse"),
        format!("{base}_SendGoal_Response"),
        format!("{dds_base}_SendGoal_Response_"),
        format!("The `SendGoal` service response for `{base}`."),
        vec![
            primitive_field(
                "accepted",
                quote!(bool),
                quote!(false),
                "Whether the goal was accepted.",
            ),
            named_field(
                "stamp",
                &time,
                package,
                "When the goal was accepted or rejected.",
            ),
        ],
    )?;
    let get_result_request = super::synthetic_struct_spec(
        package,
        &format!("{act}GetResultRequest"),
        format!("{base}_GetResult_Request"),
        format!("{dds_base}_GetResult_Request_"),
        format!("The `GetResult` service request for `{base}`."),
        vec![goal_id_field()],
    )?;
    let get_result_response = super::synthetic_struct_spec(
        package,
        &format!("{act}GetResultResponse"),
        format!("{base}_GetResult_Response"),
        format!("{dds_base}_GetResult_Response_"),
        format!("The `GetResult` service response for `{base}`."),
        vec![
            primitive_field(
                "status",
                quote!(i8),
                quote!(0i8),
                "The goal's terminal status (`action_msgs/msg/GoalStatus`'s `STATUS_*` constants).",
            ),
            named_field("result", &result_type, package, "The goal's result."),
        ],
    )?;
    let feedback_message = super::synthetic_struct_spec(
        package,
        &format!("{act}FeedbackMessage"),
        format!("{base}_FeedbackMessage"),
        format!("{dds_base}_FeedbackMessage_"),
        format!("The `{base}` feedback topic's message."),
        vec![
            goal_id_field(),
            named_field("feedback", &feedback_type, package, "The feedback itself."),
        ],
    )?;

    let goal_tokens = super::emit::emit_struct(&goal);
    let result_tokens = super::emit::emit_struct(&result);
    let feedback_tokens = super::emit::emit_struct(&feedback);
    let send_goal_request_tokens = super::emit::emit_struct(&send_goal_request);
    let send_goal_response_tokens = super::emit::emit_struct(&send_goal_response);
    let get_result_request_tokens = super::emit::emit_struct(&get_result_request);
    let get_result_response_tokens = super::emit::emit_struct(&get_result_response);
    let feedback_message_tokens = super::emit::emit_struct(&feedback_message);

    Ok(quote! {
        #goal_tokens
        #result_tokens
        #feedback_tokens
        #send_goal_request_tokens
        #send_goal_response_tokens
        #get_result_request_tokens
        #get_result_response_tokens
        #feedback_message_tokens
    })
}

fn resolve_dependency(
    universe: &TypeUniverse,
    package: &str,
    name: &str,
    home_package: &PackageName,
) -> Result<TypeName, IdlError> {
    let reference = NamedTypeRef {
        package: Some(package.to_owned()),
        name: name.to_owned(),
        span: Span::empty(Position::START),
    };
    universe.resolve(&reference, home_package).cloned()
}

fn named_field(
    name: &'static str,
    target: &TypeName,
    home_package: &PackageName,
    doc: &str,
) -> ResolvedField {
    let rust_type = rust_path_for(target, home_package);
    ResolvedField {
        ident: Ident::new(name, MacroSpan::call_site()),
        ros_name: name.to_owned(),
        doc: doc.to_owned(),
        rust_type: rust_type.clone(),
        element_type: rust_type,
        shape: FieldShape::Scalar,
        is_octet_element: false,
        // A nested message type — never `Copy` (generated structs only
        // ever derive `Clone`).
        is_copy: false,
        default_expr: quote!(::astrs_cdr::CdrDefault::cdr_default()),
    }
}

fn primitive_field(
    name: &'static str,
    rust_type: TokenStream,
    default_expr: TokenStream,
    doc: &str,
) -> ResolvedField {
    ResolvedField {
        ident: Ident::new(name, MacroSpan::call_site()),
        ros_name: name.to_owned(),
        doc: doc.to_owned(),
        rust_type: rust_type.clone(),
        element_type: rust_type,
        shape: FieldShape::Scalar,
        is_octet_element: false,
        // Every caller passes an IDL primitive (`bool`, `i8`).
        is_copy: true,
        default_expr,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codegen::format_module;
    use crate::naming::InterfaceKind;
    use crate::parser::parse_action;
    use crate::span::Position;

    fn pkg(name: &str) -> PackageName {
        PackageName::new(name, Span::empty(Position::new(1, 1))).unwrap()
    }

    fn type_name(package: &str, kind: InterfaceKind, name: &str) -> TypeName {
        TypeName::new(pkg(package), kind, name, Span::empty(Position::new(1, 1))).unwrap()
    }

    fn dependency_universe() -> TypeUniverse {
        let mut universe = TypeUniverse::new();
        universe.register(type_name(
            "unique_identifier_msgs",
            InterfaceKind::Msg,
            "UUID",
        ));
        universe.register(type_name("builtin_interfaces", InterfaceKind::Msg, "Time"));
        universe
    }

    #[test]
    fn an_action_generates_all_eight_types_as_valid_rust() {
        let type_name = type_name("test_action_pkg", InterfaceKind::Action, "Probe");
        let file =
            parse_action("int32 order\n---\nint32[] sequence\n---\nint32[] partial_sequence\n")
                .unwrap();
        let universe = dependency_universe();
        let tokens = generate_action(&type_name, &file, &universe).unwrap();
        let text = format_module("// banner", tokens).unwrap();

        for name in [
            "ProbeGoal",
            "ProbeResult",
            "ProbeFeedback",
            "ProbeSendGoalRequest",
            "ProbeSendGoalResponse",
            "ProbeGetResultRequest",
            "ProbeGetResultResponse",
            "ProbeFeedbackMessage",
        ] {
            assert!(
                text.contains(&format!("pub struct {name} {{")),
                "missing {name} in:\n{text}"
            );
        }
    }

    #[test]
    fn send_goal_request_pairs_goal_id_and_goal_in_order() {
        let type_name = type_name("test_action_pkg", InterfaceKind::Action, "Probe");
        let file =
            parse_action("int32 order\n---\nint32[] sequence\n---\nint32[] partial_sequence\n")
                .unwrap();
        let universe = dependency_universe();
        let tokens = generate_action(&type_name, &file, &universe).unwrap();
        let text = format_module("// banner", tokens).unwrap();

        let struct_start = text
            .find("pub struct ProbeSendGoalRequest {")
            .expect("struct present");
        let body = &text[struct_start..];
        let goal_id_pos = body.find("pub goal_id:").expect("goal_id present");
        let goal_pos = body.find("pub goal:").expect("goal present");
        assert!(goal_id_pos < goal_pos, "goal_id must precede goal:\n{body}");
        assert!(
            body.contains("pub goal_id: astrs_idl::generated::unique_identifier_msgs::UUID,"),
            "{body}"
        );
        assert!(body.contains("pub goal: super::ProbeGoal,"), "{body}");
    }

    #[test]
    fn get_result_response_pairs_status_and_result_in_order() {
        let type_name = type_name("test_action_pkg", InterfaceKind::Action, "Probe");
        let file =
            parse_action("int32 order\n---\nint32[] sequence\n---\nint32[] partial_sequence\n")
                .unwrap();
        let universe = dependency_universe();
        let tokens = generate_action(&type_name, &file, &universe).unwrap();
        let text = format_module("// banner", tokens).unwrap();

        let struct_start = text
            .find("pub struct ProbeGetResultResponse {")
            .expect("struct present");
        let body = &text[struct_start..];
        let status_pos = body.find("pub status:").expect("status present");
        let result_pos = body.find("pub result:").expect("result present");
        assert!(
            status_pos < result_pos,
            "status must precede result:\n{body}"
        );
    }

    #[test]
    fn an_action_without_the_dependency_packages_registered_is_reported() {
        let type_name = type_name("test_action_pkg", InterfaceKind::Action, "Probe");
        let file =
            parse_action("int32 order\n---\nint32[] sequence\n---\nint32[] partial_sequence\n")
                .unwrap();
        let universe = TypeUniverse::new();
        let err = generate_action(&type_name, &file, &universe).unwrap_err();
        assert!(matches!(err, IdlError::UnknownType { .. }));
    }
}
