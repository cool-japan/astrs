//! Rust codegen: `.msg`/`.srv`/`.action` semantics to `TokenStream`s, and
//! their assembly into formatted, compilable source files (blueprint
//! §10.3).
//!
//! # The absolute-path rule
//!
//! Every path this module emits into generated code — `astrs_idl::…`,
//! `::astrs_data::…`, `::astrs_cdr::…` — is written unconditionally, never
//! relative to *where* the code is generated. This is deliberate and load-
//! bearing for the drift guard: the pre-generated `common_interfaces` set
//! under [`crate::generated`] lives *inside* this crate (so its own emitted
//! references have to resolve as `astrs_idl::…`, which only works because
//! `lib.rs` declares `extern crate self as astrs_idl;`), while code
//! generated for a downstream package's own `.msg` files would live in a
//! *different* crate (where the same text has to resolve as a dependency
//! path). If the path prefix were parameterised, the always-on regeneration
//! test in `tests/` would compare in-tree text against in-tree text and
//! never exercise the form external code actually needs — so instead there
//! is exactly one text, and it is correct in both places by construction.

mod action;
mod emit;
mod fields;
mod service;

pub use action::generate_action;
pub use service::generate_service;

use proc_macro2::{Ident, Span as MacroSpan, TokenStream};
use quote::quote;

use crate::ast::MessageSection;
use crate::error::IdlError;
use crate::naming::{TypeName, dds_type_name, mint_urn};
use crate::resolve::TypeUniverse;
use crate::span::{Position, Span};

/// Generates one `.msg` file's type: a struct implementing
/// `astrs_idl::runtime::ColumnValue`, `astrs_data::AstrsMessage` and
/// `astrs_cdr::CdrSerde`, plus its constants.
///
/// # Errors
///
/// [`IdlError::UnknownType`] for a field reference `universe` cannot
/// resolve, or [`IdlError::UrnNameTooLong`] for a package/type name
/// combination whose minted URN would exceed the URN segment limit.
pub fn generate_message(
    type_name: &TypeName,
    section: &MessageSection,
    universe: &TypeUniverse,
) -> Result<TokenStream, IdlError> {
    let spec = build_struct_spec(
        &type_name.package,
        &type_name.name,
        type_name.full(),
        dds_type_name(type_name),
        section,
        universe,
    )?;
    Ok(emit::emit_struct(&spec))
}

/// Builds the [`emit::StructSpec`] for one message-shaped section — shared
/// by [`generate_message`] and, for their own request/response and
/// goal/result/feedback sections plus the five synthesized wire types,
/// [`service`]/[`action`].
///
/// `rust_name` is both the emitted struct's identifier and (via
/// [`mint_urn`]) the tail of its minted URN — always a plain `[A-Z][A-Za-z0-9]*`
/// identifier (`TriggerRequest`). `ros_type_name`/`dds_type_name` are passed
/// fully formatted rather than derived from a [`TypeName`], because a
/// synthesized service/action wire type's ROS/DDS identity
/// (`Trigger_Request`, underscore included — `rosidl`'s own convention)
/// is not itself a legal [`TypeName`] segment (`TypeName::new` enforces no
/// underscore, matching every *hand-written* `.msg`/`.srv`/`.action` type
/// name) even though its Rust spelling is.
fn build_struct_spec(
    package: &crate::naming::PackageName,
    rust_name: &str,
    ros_type_name: String,
    dds_type_name: String,
    section: &MessageSection,
    universe: &TypeUniverse,
) -> Result<emit::StructSpec, IdlError> {
    let has_fields = section.fields().next().is_some();
    let fields = if has_fields {
        section
            .fields()
            .map(|field| fields::build_field(field, package, universe))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![fields::placeholder_field()]
    };
    let constants = section.constants().map(fields::build_constant).collect();
    let urn_span = Span::empty(Position::START);
    let urn = mint_urn(package, rust_name, urn_span)?.as_str().to_owned();
    let struct_doc = if section.leading_comment.is_empty() {
        format!("`{ros_type_name}`.")
    } else {
        section.leading_comment.join("\n")
    };
    Ok(emit::StructSpec {
        type_ident: Ident::new(rust_name, MacroSpan::call_site()),
        struct_doc,
        urn,
        ros_type_name,
        dds_type_name,
        fields,
        constants,
    })
}

/// The `"pkg::kind::dds_::Name"` DDS-mangled prefix, **without**
/// [`dds_type_name`]'s own trailing `_` — [`service`]/[`action`] compose a
/// synthesized sub-type's full mangled name on top of this
/// (`{prefix}_Request_`, `{prefix}_Goal_`, …); using [`dds_type_name`]
/// itself as that base would double the underscore (`AddTwoInts__Request_`)
/// since it already ends in one.
fn dds_prefix(type_name: &TypeName) -> String {
    format!(
        "{}::{}::dds_::{}",
        type_name.package,
        type_name.kind.as_str(),
        type_name.name
    )
}

/// Builds an [`emit::StructSpec`] directly from an already-resolved field
/// list, for [`action`]'s five synthesized wire types
/// (`_SendGoal_Request`/`_Response`, `_GetResult_Request`/`_Response`,
/// `_FeedbackMessage`) — these are wiring `astrs-idl` itself constructs
/// (`goal_id: unique_identifier_msgs/UUID`, `goal: <Action>_Goal`, …), never
/// parsed from an `.msg`-shaped section, so there is no
/// [`crate::ast::MessageSection`] for [`build_struct_spec`] to resolve
/// fields out of.
fn synthetic_struct_spec(
    package: &crate::naming::PackageName,
    rust_name: &str,
    ros_type_name: String,
    dds_type_name: String,
    struct_doc: String,
    fields: Vec<fields::ResolvedField>,
) -> Result<emit::StructSpec, IdlError> {
    let urn = mint_urn(package, rust_name, Span::empty(Position::START))?
        .as_str()
        .to_owned();
    Ok(emit::StructSpec {
        type_ident: Ident::new(rust_name, MacroSpan::call_site()),
        struct_doc,
        urn,
        ros_type_name,
        dds_type_name,
        fields,
        constants: Vec::new(),
    })
}

/// Formats `items` (already-assembled top-level `TokenStream` content) into
/// a complete source file: `banner` as a leading line comment, then the
/// tokens run through `syn::parse2::<syn::File>` and `prettyplease::unparse`.
///
/// This is the **one** function that turns tokens into the final `String` —
/// both the `#[ignore]`d regeneration writer and the always-on drift-guard
/// comparison in `tests/` call it, so there is no second place a banner or
/// trailing-newline difference could sneak in between what gets written and
/// what gets checked.
///
/// # `prettyplease` output and the `cargo fmt` gate
///
/// `prettyplease` is deliberately not a `rustfmt` reimplementation — it is
/// retained (blueprint §18.1: "Codegen (idl, derives)") precisely because it
/// formats a `TokenStream` deterministically **in-process**, with no `PATH`
/// dependency on the `rustfmt` binary. That is the right trade-off for a
/// library whose codegen path a downstream crate's own `build.rs` may call,
/// but it means `prettyplease`'s line-wrapping does not always match what
/// `rustfmt` itself would choose (chained method calls are the common case).
/// `#![rustfmt::skip]` cannot paper over that gap here: a whole-module inner
/// attribute is only accepted by `rustc` at the crate root (`E0658`,
/// rust-lang/rust#54726 is still open even at this crate's pinned toolchain)
/// and every one of these files is loaded via `mod`, never the root. Instead
/// each item this module's private `emit::emit_struct` produces carries its
/// own outer `#[rustfmt::skip]` — stable at any nesting depth — so this
/// function itself stays a plain formatter with no attribute-injection of
/// its own; see that submodule's own docs for where the attribute is
/// actually added.
///
/// # Errors
///
/// [`IdlError::GeneratedTokensDidNotParse`] if `items` is not syntactically
/// valid Rust — a bug in this module's emission, never in the source
/// `.msg`/`.srv`/`.action` file, since by the time anything calls this the
/// input has already parsed and resolved successfully.
pub fn format_module(banner: &str, items: TokenStream) -> Result<String, IdlError> {
    let file: syn::File =
        syn::parse2(items).map_err(|source| IdlError::GeneratedTokensDidNotParse {
            file: banner.to_owned(),
            message: source.to_string(),
        })?;
    let body = prettyplease::unparse(&file);
    Ok(format!("{banner}\n{body}"))
}

/// The banner every generated file opens with, naming the tool and the
/// source file it was mechanically produced from (never hand-edited).
#[must_use]
pub fn banner_for(source_relative_path: &str) -> String {
    format!(
        "// @generated by astrs-idl from `{source_relative_path}`. Do not edit by hand — \
         see `tests/` for the regeneration entry point."
    )
}

/// A single-parameter `#[doc = "..."]` attribute `TokenStream`, used instead
/// of `quote!`'s `///` doc-comment sugar so a doc string built at codegen
/// time (never a literal in the macro invocation) still renders as a normal
/// doc comment once `prettyplease` formats it, and so that
/// `missing_docs = "warn"` (blueprint §20.1) is satisfied for every `pub`
/// item this module ever emits, whether or not the source `.msg` carried a
/// comment — see [`fields::build_field`]/[`fields::build_constant`]'s
/// deterministic fallback text.
pub(crate) fn doc_attr(text: &str) -> TokenStream {
    // A leading space matches the conventional `/// text` (not `///text`)
    // rendering once `prettyplease` turns the attribute back into `///`
    // sugar — the attribute form itself is whitespace-literal.
    let text = format!(" {}", escape_doc_text(text));
    quote!(#[doc = #text])
}

/// Escapes `[`/`]` (and any literal `\` already present, so the new
/// escapes it introduces are the only backslashes left unescaped) so
/// arbitrary source text — most importantly a `.msg`/`.srv`/`.action`
/// comment, which routinely reads `Mass [kg]` or `Map width [cells]` — can
/// never be misparsed as (broken) Markdown link syntax once it becomes a
/// `#[doc = "..."]` attribute. `rustdoc::broken_intra_doc_links` denies
/// exactly this under `-D warnings`, and unlike every other diagnostic that
/// lint catches, this one is not a bug in the emitted code at all — it is
/// ROS 2's own comment prose, unescaped, which this function is the one
/// place responsible for fixing before it reaches rustdoc.
///
/// [`linkify_bare_urls`] runs second, for the same reason: a handful of
/// `.msg` source comments across the corpus cite a design doc or RFC by
/// bare `http://`/`https://` URL (`unique_identifier_msgs/msg/UUID.msg`'s
/// RFC 4122 reference, `rosgraph_msgs/msg/Clock.msg`'s design-doc link, …),
/// and `rustdoc::bare_urls` denies those too.
fn escape_doc_text(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]");
    linkify_bare_urls(&escaped)
}

/// Wraps every bare `http://`/`https://` URL in `<...>` so rustdoc renders
/// it as a link instead of denying it as a bare URL. Trailing sentence
/// punctuation (`. , ; : ! ?`) is left outside the link, matching
/// `rosgraph_msgs/msg/Clock.msg`'s "`…clock_and_time.html.`" — the `.`
/// ends the sentence, not the URL, and belongs outside `<...>` either way.
///
/// Deliberately not a general-purpose URL scanner: this exists to carry a
/// small, known, already-committed corpus of ROS 2 `.msg`/`.srv` doc
/// comments through `-D warnings`, the same scope `escape_doc_text` above
/// documents for its own bracket escaping.
fn linkify_bare_urls(text: &str) -> String {
    const SCHEMES: [&str; 2] = ["http://", "https://"];
    const TRAILING_PUNCTUATION: [char; 6] = ['.', ',', ';', ':', '!', '?'];

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = SCHEMES.iter().filter_map(|scheme| rest.find(scheme)).min() else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);

        let candidate = &rest[start..];
        let url_len = candidate
            .find(char::is_whitespace)
            .unwrap_or(candidate.len());
        let url = &candidate[..url_len];
        let core = url.trim_end_matches(TRAILING_PUNCTUATION);
        let trailing = &url[core.len()..];

        out.push('<');
        out.push_str(core);
        out.push('>');
        out.push_str(trailing);

        rest = &candidate[url_len..];
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn doc_attr_escapes_brackets_so_ros_comment_units_cannot_break_a_doc_link() {
        // Real `.msg` comments routinely read this way (`geometry_msgs/msg/
        // Inertia.msg`'s "Mass [kg]", `nav_msgs/msg/MapMetaData.msg`'s "Map
        // width [cells]") — unescaped, `[kg]` parses as a broken Markdown
        // link and `-D rustdoc::broken_intra_doc_links` fails the doc gate.
        let attr = doc_attr("Mass [kg]");
        let rendered = format_module("// b", quote!(#attr pub struct S;)).unwrap();
        assert!(rendered.contains(r"Mass \[kg\]"), "{rendered}");
    }

    #[test]
    fn escape_doc_text_escapes_pre_existing_backslashes_first() {
        // If a literal `\` were left alone, escaping `]` next to it could
        // read as `\]` (an escaped bracket) instead of "a backslash, then a
        // literal bracket" — escaping `\` to `\\` first keeps the two cases
        // distinguishable.
        assert_eq!(escape_doc_text(r"a\b"), r"a\\b");
        assert_eq!(escape_doc_text("[x]"), r"\[x\]");
        assert_eq!(escape_doc_text(r"\[x]"), r"\\\[x\]");
    }

    #[test]
    fn escape_doc_text_wraps_bare_urls_for_both_schemes() {
        // `unique_identifier_msgs/msg/UUID.msg`'s own RFC 4122 references,
        // verbatim: two bare URLs, one per scheme, each alone on its line.
        let text = "A universally unique identifier (UUID).\n\n \
                     http://en.wikipedia.org/wiki/Universally_unique_identifier\n \
                     https://tools.ietf.org/html/rfc4122.html";
        let escaped = escape_doc_text(text);
        assert!(
            escaped.contains("<http://en.wikipedia.org/wiki/Universally_unique_identifier>"),
            "{escaped}"
        );
        assert!(
            escaped.contains("<https://tools.ietf.org/html/rfc4122.html>"),
            "{escaped}"
        );
    }

    #[test]
    fn escape_doc_text_keeps_trailing_sentence_punctuation_outside_the_link() {
        // `rosgraph_msgs/msg/Clock.msg`'s own comment, verbatim: the URL is
        // immediately followed by the sentence's closing period, which is
        // not part of the URL and must not end up inside `<...>` (it would
        // otherwise send a reader to a link one character too long).
        let escaped = escape_doc_text(
            "For more information, see https://design.ros2.org/articles/clock_and_time.html.",
        );
        assert!(
            escaped.contains("<https://design.ros2.org/articles/clock_and_time.html>."),
            "{escaped}"
        );
    }

    #[test]
    fn escape_doc_text_wraps_every_url_in_text_with_more_than_one() {
        // `rcl_interfaces/msg/Log.msg` cites two URLs in two separate
        // comment lines; both must survive, in order, not just the first.
        let escaped = escape_doc_text(
            "See https://docs.python.org/3/library/logging.html#logging-levels and also \
             https://github.com/ros2/rcutils/blob/HEAD/include/rcutils/logging.h#L164-L172",
        );
        assert!(
            escaped.contains("<https://docs.python.org/3/library/logging.html#logging-levels>"),
            "{escaped}"
        );
        assert!(
            escaped.contains(
                "<https://github.com/ros2/rcutils/blob/HEAD/include/rcutils/logging.h#L164-L172>"
            ),
            "{escaped}"
        );
    }

    #[test]
    fn escape_doc_text_leaves_url_free_text_untouched() {
        assert_eq!(escape_doc_text("no links here"), "no links here");
        assert_eq!(escape_doc_text(""), "");
    }

    #[test]
    fn format_module_produces_prettyplease_formatted_output() {
        let tokens = quote! {
            #[doc = "A point."]
            pub struct Point { pub x: f64, pub y: f64 }
        };
        let text = format_module("// banner", tokens).unwrap();
        assert!(text.starts_with("// banner\n"));
        assert!(text.contains("pub struct Point {"));
        assert!(text.contains("pub x: f64,"));
    }

    #[test]
    fn format_module_rejects_malformed_tokens() {
        let tokens = quote! { pub struct };
        let err = format_module("// banner", tokens).unwrap_err();
        assert!(matches!(err, IdlError::GeneratedTokensDidNotParse { .. }));
    }

    #[test]
    fn banner_names_the_source_file() {
        let banner = banner_for("geometry_msgs/msg/Point.msg");
        assert!(banner.contains("geometry_msgs/msg/Point.msg"));
        assert!(banner.starts_with("// @generated"));
    }

    #[test]
    fn doc_attr_renders_as_a_doc_comment() {
        let attr = doc_attr("hello world");
        let rendered = format_module("// b", quote!(#attr pub struct S;)).unwrap();
        assert!(rendered.contains("/// hello world"), "{rendered}");
    }
}
