//! ROS 2 interface definitions: parse and generate.
//!
//! A hand-rolled recursive-descent front end — no parser-combinator
//! dependency — and a code generator that puts every ROS type into columnar
//! space mechanically (blueprint §10.3):
//!
//! - `.msg`, `.srv` and `.action` parsing: primitives, arrays, bounded
//!   types, `wstring`, constants and default values ([`lexer`], [`ast`],
//!   [`parser`]).
//! - Cross-file resolution of type references and the
//!   `structure_needs_at_least_one_member` synthesis rule ([`resolve`]);
//!   member ordering falls out of the parse tree itself, since
//!   [`ast::MessageSection`] already preserves declaration order.
//! - `package.xml` and ament-tree discovery for locating interface packages
//!   ([`discovery`]).
//! - Rust codegen (proc-macro2/quote/prettyplease) emitting types that
//!   implement both `astrs_cdr::CdrSerde` and `astrs_data::AstrsMessage`
//!   ([`codegen`], [`runtime`]).
//! - The `common_interfaces` set (std_msgs, geometry_msgs, sensor_msgs,
//!   nav_msgs, …) ships pre-generated in-tree ([`generated`]), so bridging
//!   standard topics needs no ROS files on disk at all.
//!
//! # Layer map
//!
//! ```text
//!   generated   pre-generated common_interfaces          committed output
//!   codegen     quote! emitters, field/constant resolution AST → Rust source
//!   resolve     type-reference resolution, cycle check     parse tree → universe
//!   discovery   package.xml, ament index, source trees     packages on disk
//!   parser      recursive-descent grammar                 tokens → parse tree
//!   naming      identifiers, `pkg/kind/Type`, URNs          names ⇄ names
//!   ast         the parse tree types                        —
//!   lexer       line classification and tokenizing         text → tokens
//!   runtime     `ColumnValue` — what generated code calls   columnar leaves
//!   span        `Position` / `Span`                          —
//!   error       `IdlError`                                   —
//! ```

pub mod ast;
pub mod codegen;
pub mod discovery;
pub mod error;
pub mod generated;
pub mod lexer;
pub mod naming;
pub mod parser;
pub mod resolve;
pub mod runtime;
pub mod span;

// Generated code — whether it lives in this crate's own `generated/` module
// or, unpacked by a future `astrs idl generate`, in a downstream crate — is
// emitted with `astrs_idl::runtime::…` absolute paths unconditionally
// (`codegen`'s module docs explain why: one emitted `String` has to be
// correct in both places, and a parameterised path prefix would mean the
// drift guard compares in-tree text to in-tree text and never exercises the
// external form). This makes `astrs_idl::` resolve inside this crate too.
extern crate self as astrs_idl;
