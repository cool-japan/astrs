//! The `astrs-idl` error taxonomy.
//!
//! Every fallible path in this crate returns [`IdlError`]. Mirroring
//! `astrs-cdr`'s [`CdrError`](astrs_cdr::CdrError), the variants are
//! deliberately fine-grained and the type is `Clone + PartialEq + Eq` so
//! tests assert an exact expected error rather than a `matches!` shape — the
//! blueprint's rejection-test requirement (§10.3) needs both the *kind* of
//! mistake and the *position* it was found at to be checkable in one
//! `assert_eq!`. The taxonomy splits into seven groups:
//!
//! 1. **Lexical** — [`IdlError::UnterminatedString`],
//!    [`IdlError::InvalidEscape`], [`IdlError::UnexpectedCharacter`],
//!    [`IdlError::InvalidNumber`]: the character stream does not spell a
//!    legal token.
//! 2. **Grammar** — [`IdlError::UnexpectedToken`],
//!    [`IdlError::UnexpectedEndOfLine`], [`IdlError::InvalidArraySuffix`],
//!    [`IdlError::NestedArrayNotAllowed`], [`IdlError::BoundNotAllowedHere`],
//!    [`IdlError::TrailingTokens`], [`IdlError::MissingEquals`],
//!    [`IdlError::WrongSectionCount`]: the tokens do not spell a legal
//!    declaration or file.
//! 3. **Identifiers** — [`IdlError::InvalidFieldName`],
//!    [`IdlError::InvalidConstantName`], [`IdlError::InvalidTypeName`],
//!    [`IdlError::InvalidPackageName`], [`IdlError::ReservedIdentifier`],
//!    [`IdlError::DuplicateMember`]: a name violates the ROS 2 identifier
//!    rules.
//! 4. **Literals** — [`IdlError::InvalidLiteral`],
//!    [`IdlError::NumberOutOfRange`],
//!    [`IdlError::ArrayLiteralLengthMismatch`],
//!    [`IdlError::DefaultExceedsBound`], [`IdlError::ConstantMustBeScalar`],
//!    [`IdlError::ConstantMustBePrimitive`]: a default or constant value does
//!    not fit its declared type.
//! 5. **Resolution** — [`IdlError::UnknownType`],
//!    [`IdlError::CircularTypeReference`], [`IdlError::UrnNameTooLong`]: a
//!    parsed file cannot be turned into a semantic model.
//! 6. **Discovery** — [`IdlError::PackageXmlNotFound`],
//!    [`IdlError::PackageXmlMissingName`],
//!    [`IdlError::PackageXmlInvalidName`], [`IdlError::PackageXmlParse`],
//!    [`IdlError::Io`], [`IdlError::InterfaceFileNotFound`]: locating
//!    packages and interface files on disk failed.
//! 7. **Codegen** — [`IdlError::GeneratedTokensDidNotParse`]: the emitted
//!    `TokenStream` is not syntactically valid Rust (a bug in this crate, not
//!    in the input `.msg`/`.srv`/`.action` file).
//!
//! [`quick_xml::Error`], [`syn::Error`] and [`std::io::Error`] are none of
//! them `Clone`/`Eq`, so this crate never stores one directly: every
//! discovery or codegen variant that wraps one keeps its rendered
//! [`ToString`] output instead, which is what preserves `IdlError`'s own
//! `Clone + PartialEq + Eq`.

use std::path::PathBuf;

use thiserror::Error;

use crate::span::Span;

/// The result type every fallible `astrs-idl` operation returns.
pub type IdlResult<T> = Result<T, IdlError>;

/// Everything that can go wrong parsing, resolving, discovering or
/// generating ROS 2 interface files.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum IdlError {
    // ---- 1. Lexical -----------------------------------------------------
    /// A `"`/`'`-quoted string literal was not closed before the end of its
    /// line.
    ///
    /// `.msg` grammar has no line continuation, so an unterminated string is
    /// always an end-of-line condition, never an end-of-file one.
    #[error("unterminated string literal at {span}")]
    UnterminatedString {
        /// Where the opening quote was found.
        span: Span,
    },

    /// A `\`-escape inside a string literal names a character this crate
    /// does not recognise.
    #[error("unknown escape '\\{escape}' in string literal at {span}")]
    InvalidEscape {
        /// The character following the backslash.
        escape: char,
        /// Location of the escape sequence.
        span: Span,
    },

    /// A character cannot start any token of this grammar.
    #[error("unexpected character {found:?} at {span}")]
    UnexpectedCharacter {
        /// The offending character.
        found: char,
        /// Its location.
        span: Span,
    },

    /// A numeric lexeme (`-`, digits, at most one `.`, an optional exponent)
    /// does not form a legal integer or floating-point literal.
    #[error("invalid numeric literal {text:?} at {span}")]
    InvalidNumber {
        /// The rejected lexeme.
        text: String,
        /// Its location.
        span: Span,
    },

    // ---- 2. Grammar -------------------------------------------------------
    /// A token appeared where the grammar required something else.
    #[error("unexpected token {found:?} at {span}, expected {expected}")]
    UnexpectedToken {
        /// Text of the token actually found.
        found: String,
        /// What the grammar rule expected, e.g. `"a type name"`.
        expected: &'static str,
        /// Location of the offending token.
        span: Span,
    },

    /// The line ended where the grammar required another token.
    #[error("unexpected end of line at {span}, expected {expected}")]
    UnexpectedEndOfLine {
        /// What the grammar rule expected.
        expected: &'static str,
        /// Location just past the last token on the line.
        span: Span,
    },

    /// An array suffix (`[`...`]`) is not one of `[]`, `[N]` or `[<=N]`.
    #[error("invalid array suffix at {span}, expected '[]', '[N]' or '[<=N]'")]
    InvalidArraySuffix {
        /// Location of the malformed suffix.
        span: Span,
    },

    /// A fixed-size array suffix (`T[N]`) declared `N` as `0`.
    ///
    /// `astrs_data::DataType::FixedSizeBinary`/`FixedSizeList` and
    /// `astrs_data::array::FixedSizeListArray::try_new`'s own
    /// `check_fixed_size` all require a positive size, so `int32[0]` cannot
    /// be encoded — rejected here rather than surfacing as a columnar
    /// construction failure at codegen time. `string<=0` and `T[<=0]` are
    /// left alone: an always-empty bounded string/sequence is unusual but
    /// representable.
    #[error("fixed-size array suffix at {span} declares a size of 0, which cannot be encoded")]
    ZeroSizedFixedArray {
        /// Location of the `[0]` suffix.
        span: Span,
    },

    /// A type carries two array suffixes (`T[][]`, `T[2][3]`, …).
    ///
    /// ROS 2 IDL has no multi-dimensional array type; a field is a scalar,
    /// or a scalar with exactly one array/sequence wrapper.
    #[error("a field type may carry at most one array suffix, found a second at {span}")]
    NestedArrayNotAllowed {
        /// Location of the second suffix.
        span: Span,
    },

    /// A `<=N` bound was written on a type other than `string` or `wstring`.
    #[error("'<=' bounds only apply to string/wstring, found one at {span}")]
    BoundNotAllowedHere {
        /// Location of the bound.
        span: Span,
    },

    /// A field or constant declaration has tokens left over after a
    /// complete value was parsed, before the end of the line or a comment.
    #[error("unexpected trailing content at {span}")]
    TrailingTokens {
        /// Location of the first unconsumed token.
        span: Span,
    },

    /// An identifier that looks like a constant name (`UPPER_CASE`) was not
    /// followed by `=`.
    #[error("constant {span} must be followed by '=' and a value")]
    MissingEquals {
        /// Location of the constant name.
        span: Span,
    },

    /// A `.srv` or `.action` file does not have the section count its kind
    /// requires: exactly one `---` for `.srv`, exactly two for `.action`.
    #[error("{file_kind} needs {expected} '---' separator(s) but found {found}, checked at {span}")]
    WrongSectionCount {
        /// `"a .srv file"` / `"an .action file"`.
        file_kind: &'static str,
        /// Separators the kind requires.
        expected: usize,
        /// Separators actually found.
        found: usize,
        /// Location of the offending separator, or end of file.
        span: Span,
    },

    // ---- 3. Identifiers -----------------------------------------------
    /// A field name does not match `[a-z][a-z0-9_]*`, or has a leading,
    /// trailing or doubled underscore.
    #[error("{name:?} at {span} is not a valid ROS 2 field name")]
    InvalidFieldName {
        /// The rejected name.
        name: String,
        /// Its location.
        span: Span,
    },

    /// A constant name does not match `[A-Z][A-Z0-9_]*`, or has a leading,
    /// trailing or doubled underscore.
    #[error("{name:?} at {span} is not a valid ROS 2 constant name")]
    InvalidConstantName {
        /// The rejected name.
        name: String,
        /// Its location.
        span: Span,
    },

    /// A message/service/action type name does not match
    /// `[A-Z][A-Za-z0-9]*`.
    #[error("{name:?} at {span} is not a valid ROS 2 type name")]
    InvalidTypeName {
        /// The rejected name.
        name: String,
        /// Its location.
        span: Span,
    },

    /// A package name (in a namespaced type reference) does not match
    /// `[a-z][a-z0-9_]*`, or has a leading, trailing or doubled underscore.
    #[error("{name:?} at {span} is not a valid ROS 2 package name")]
    InvalidPackageName {
        /// The rejected name.
        name: String,
        /// Its location.
        span: Span,
    },

    /// A field or constant name collides with an IDL primitive type
    /// keyword.
    #[error("{name:?} at {span} is reserved (it names a {kind})")]
    ReservedIdentifier {
        /// The rejected name.
        name: String,
        /// What it collides with, e.g. `"primitive type"`.
        kind: &'static str,
        /// Its location.
        span: Span,
    },

    /// The same field or constant name appears twice in one message
    /// section.
    #[error("{name:?} declared again at {second}, first declared at {first}")]
    DuplicateMember {
        /// The repeated name.
        name: String,
        /// The first declaration's location.
        first: Span,
        /// The repeated declaration's location.
        second: Span,
    },

    // ---- 4. Literals --------------------------------------------------
    /// A default or constant value's shape does not match what the field's
    /// type requires (an array literal for a scalar field, a scalar for an
    /// array field, a string where a number was required, …).
    #[error("value at {span} is not a valid {expected}")]
    InvalidLiteral {
        /// What was required, e.g. `"int32"` or `"an array of float64"`.
        expected: &'static str,
        /// Location of the value.
        span: Span,
    },

    /// A numeric literal is syntactically valid but does not fit the
    /// declared IDL type's range.
    #[error("{text} at {span} does not fit {type_name}")]
    NumberOutOfRange {
        /// The literal as written.
        text: String,
        /// The IDL type it was checked against, e.g. `"int8"`.
        type_name: &'static str,
        /// Location of the literal.
        span: Span,
    },

    /// A fixed-size array field (`T[N]`) was given a default whose element
    /// count is not exactly `N`.
    #[error("array default at {span} has {actual} element(s), field requires exactly {expected}")]
    ArrayLiteralLengthMismatch {
        /// The declared fixed size.
        expected: u32,
        /// The default literal's element count.
        actual: usize,
        /// Location of the default.
        span: Span,
    },

    /// A bounded array or string/wstring field (`T[<=N]`, `string<=N`) was
    /// given a default longer than `N`.
    #[error("default at {span} has length {actual}, exceeding the bound of {bound}")]
    DefaultExceedsBound {
        /// The declared bound.
        bound: u32,
        /// The default's actual length.
        actual: usize,
        /// Location of the default.
        span: Span,
    },

    /// A constant declaration used an array type. ROS 2 constants are
    /// always scalar.
    #[error("constant at {span} may not have an array type")]
    ConstantMustBeScalar {
        /// Location of the array suffix.
        span: Span,
    },

    /// A constant declaration used a namespaced message type. ROS 2
    /// constants are always an IDL primitive, `string` or `wstring`.
    #[error("constant at {span} may not have a message type")]
    ConstantMustBePrimitive {
        /// Location of the named type.
        span: Span,
    },

    // ---- 5. Resolution --------------------------------------------------
    /// A field's namespaced or bare type reference does not name a type in
    /// the resolver's known universe (blueprint §10.3's discovery set).
    #[error("unknown type {reference:?} referenced at {span}")]
    UnknownType {
        /// The reference exactly as written (`"pkg/Type"` or `"Type"`).
        reference: String,
        /// Location of the reference.
        span: Span,
    },

    /// Two or more message types reference each other, directly or
    /// transitively, as field types — which cannot have a finite size.
    #[error("circular type reference {cycle} (via the field at {span})")]
    CircularTypeReference {
        /// The cycle, rendered `"A -> B -> A"`.
        cycle: String,
        /// Location of the field that closes the cycle.
        span: Span,
    },

    /// A minted `std/ros2/v1/<Name>` URN (see [`crate::naming::mint_urn`])
    /// would exceed `astrs_data::TypeUrn::MAX_SEGMENT_LEN`.
    #[error("generated URN name {urn:?} ({len} bytes) exceeds the {max}-byte URN segment limit")]
    UrnNameTooLong {
        /// The name that would have been minted.
        urn: String,
        /// Its length in bytes.
        len: usize,
        /// The limit.
        max: usize,
        /// Location of the type's declaration.
        span: Span,
    },

    // ---- 6. Discovery -----------------------------------------------------
    /// A directory that should contain a ROS 2 package has no
    /// `package.xml`.
    #[error("no package.xml found at {path}", path = .path.display())]
    PackageXmlNotFound {
        /// The directory that was checked.
        path: PathBuf,
    },

    /// A `package.xml` has no `<name>` element.
    #[error("{path} has no <name> element", path = .path.display())]
    PackageXmlMissingName {
        /// The file that was checked.
        path: PathBuf,
    },

    /// A `package.xml`'s `<name>` element is not a valid ROS 2 package
    /// name.
    #[error("{path} declares an invalid package name {name:?}", path = .path.display())]
    PackageXmlInvalidName {
        /// The file that was checked.
        path: PathBuf,
        /// The rejected name.
        name: String,
    },

    /// A `package.xml` could not be parsed as XML.
    #[error("{path} is not well-formed XML: {message}", path = .path.display())]
    PackageXmlParse {
        /// The file that was checked.
        path: PathBuf,
        /// [`quick_xml::Error`]'s rendered message.
        message: String,
    },

    /// A filesystem operation failed during discovery.
    #[error("I/O error at {path}: {message}", path = .path.display())]
    Io {
        /// The path being read or walked.
        path: PathBuf,
        /// [`std::io::Error`]'s rendered message.
        message: String,
    },

    /// A type reference resolved to a known package but no matching
    /// `.msg`/`.srv`/`.action` file exists under it.
    #[error("no interface file for {reference:?} under {path}", path = .path.display())]
    InterfaceFileNotFound {
        /// The reference that was being resolved.
        reference: String,
        /// The directory that was searched.
        path: PathBuf,
    },

    // ---- 7. Codegen -------------------------------------------------------
    /// The `TokenStream` this crate emitted for `file` did not parse as a
    /// [`syn::File`] — a bug in this crate's codegen, never in the input
    /// `.msg`/`.srv`/`.action`.
    #[error("generated code for {file} did not parse as Rust: {message}")]
    GeneratedTokensDidNotParse {
        /// Name of the file being generated (for diagnostics only).
        file: String,
        /// [`syn::Error`]'s rendered message.
        message: String,
    },
}

impl IdlError {
    /// The source span this error points at, when it points at one.
    ///
    /// `None` only for the [discovery](#discovery) group, whose errors are
    /// about file/directory identity rather than a position within a parsed
    /// file.
    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        match self {
            Self::UnterminatedString { span }
            | Self::InvalidEscape { span, .. }
            | Self::UnexpectedCharacter { span, .. }
            | Self::InvalidNumber { span, .. }
            | Self::UnexpectedToken { span, .. }
            | Self::UnexpectedEndOfLine { span, .. }
            | Self::InvalidArraySuffix { span }
            | Self::ZeroSizedFixedArray { span }
            | Self::NestedArrayNotAllowed { span }
            | Self::BoundNotAllowedHere { span }
            | Self::TrailingTokens { span }
            | Self::MissingEquals { span }
            | Self::WrongSectionCount { span, .. }
            | Self::InvalidFieldName { span, .. }
            | Self::InvalidConstantName { span, .. }
            | Self::InvalidTypeName { span, .. }
            | Self::InvalidPackageName { span, .. }
            | Self::ReservedIdentifier { span, .. }
            | Self::DuplicateMember { second: span, .. }
            | Self::InvalidLiteral { span, .. }
            | Self::NumberOutOfRange { span, .. }
            | Self::ArrayLiteralLengthMismatch { span, .. }
            | Self::DefaultExceedsBound { span, .. }
            | Self::ConstantMustBeScalar { span }
            | Self::ConstantMustBePrimitive { span }
            | Self::UnknownType { span, .. }
            | Self::CircularTypeReference { span, .. }
            | Self::UrnNameTooLong { span, .. } => Some(*span),
            Self::PackageXmlNotFound { .. }
            | Self::PackageXmlMissingName { .. }
            | Self::PackageXmlInvalidName { .. }
            | Self::PackageXmlParse { .. }
            | Self::Io { .. }
            | Self::InterfaceFileNotFound { .. }
            | Self::GeneratedTokensDidNotParse { .. } => None,
        }
    }

    /// True for the lexical and grammar groups: the input could not even be
    /// parsed into an AST.
    #[must_use]
    pub const fn is_syntax_error(&self) -> bool {
        matches!(
            self,
            Self::UnterminatedString { .. }
                | Self::InvalidEscape { .. }
                | Self::UnexpectedCharacter { .. }
                | Self::InvalidNumber { .. }
                | Self::UnexpectedToken { .. }
                | Self::UnexpectedEndOfLine { .. }
                | Self::InvalidArraySuffix { .. }
                | Self::ZeroSizedFixedArray { .. }
                | Self::NestedArrayNotAllowed { .. }
                | Self::BoundNotAllowedHere { .. }
                | Self::TrailingTokens { .. }
                | Self::MissingEquals { .. }
                | Self::WrongSectionCount { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::span::Position;

    fn at(line: u32, column: u32) -> Span {
        Span::empty(Position::new(line, column))
    }

    #[test]
    fn errors_compare_by_value() {
        let a = IdlError::InvalidFieldName {
            name: "Bad".to_owned(),
            span: at(3, 5),
        };
        let b = IdlError::InvalidFieldName {
            name: "Bad".to_owned(),
            span: at(3, 5),
        };
        let c = IdlError::InvalidFieldName {
            name: "Bad".to_owned(),
            span: at(3, 6),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn span_extracts_the_position_for_syntax_and_semantic_errors() {
        let error = IdlError::UnknownType {
            reference: "pkg/Missing".to_owned(),
            span: at(9, 2),
        };
        assert_eq!(error.span(), Some(at(9, 2)));
    }

    #[test]
    fn span_is_none_for_discovery_errors() {
        let error = IdlError::PackageXmlNotFound {
            path: PathBuf::from("/tmp/pkg"),
        };
        assert_eq!(error.span(), None);
    }

    #[test]
    fn duplicate_member_reports_the_second_occurrence_as_its_span() {
        let error = IdlError::DuplicateMember {
            name: "x".to_owned(),
            first: at(1, 1),
            second: at(5, 1),
        };
        assert_eq!(error.span(), Some(at(5, 1)));
    }

    #[test]
    fn is_syntax_error_partitions_lexical_and_grammar_from_the_rest() {
        assert!(
            IdlError::UnterminatedString { span: at(1, 1) }.is_syntax_error(),
            "lexical"
        );
        assert!(
            IdlError::TrailingTokens { span: at(1, 1) }.is_syntax_error(),
            "grammar"
        );
        assert!(
            !IdlError::UnknownType {
                reference: "X".to_owned(),
                span: at(1, 1),
            }
            .is_syntax_error(),
            "resolution is not a syntax error"
        );
        assert!(
            !IdlError::PackageXmlNotFound {
                path: PathBuf::from("/tmp"),
            }
            .is_syntax_error(),
            "discovery is not a syntax error"
        );
    }

    #[test]
    fn display_mentions_the_offending_position() {
        let text = IdlError::UnexpectedToken {
            found: "]".to_owned(),
            expected: "a field name",
            span: at(4, 12),
        }
        .to_string();
        assert!(text.contains("4:12"), "{text}");
        assert!(text.contains("a field name"), "{text}");
    }

    #[test]
    fn display_formats_paths_with_display_not_debug() {
        let text = IdlError::PackageXmlNotFound {
            path: PathBuf::from("/tmp/my_pkg"),
        }
        .to_string();
        assert!(text.contains("/tmp/my_pkg"), "{text}");
        assert!(!text.contains('"'), "path should not be quoted: {text}");
    }
}
