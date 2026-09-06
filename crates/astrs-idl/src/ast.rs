//! The parse tree: what [`crate::parser`] builds directly out of tokens,
//! before [`crate::resolve`] turns type references into a semantic model.
//!
//! Every node keeps a [`Span`] so a later phase (resolution, codegen) can
//! still report a precise position even though the token stream itself is
//! gone by then.

use crate::span::Span;

/// The thirteen IDL primitive types (blueprint §10.3), excluding `string`
/// and `wstring` — those carry an optional bound and get their own
/// [`ScalarType`] variants instead.
///
/// `byte` and `char` both map to the CDR octet (`u8`); see
/// `astrs_cdr::impls::primitive`'s module docs for why Rust's `char` is
/// deliberately not the target of either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveKind {
    /// `bool`
    Bool,
    /// `byte` (IDL octet)
    Byte,
    /// `char` (IDL octet)
    Char,
    /// `float32` (IDL float)
    Float32,
    /// `float64` (IDL double)
    Float64,
    /// `int8`
    Int8,
    /// `uint8` (IDL octet)
    Uint8,
    /// `int16` (IDL short)
    Int16,
    /// `uint16` (IDL unsigned short)
    Uint16,
    /// `int32` (IDL long)
    Int32,
    /// `uint32` (IDL unsigned long)
    Uint32,
    /// `int64` (IDL long long)
    Int64,
    /// `uint64` (IDL unsigned long long)
    Uint64,
}

impl PrimitiveKind {
    /// Every primitive keyword, in the order `.msg` files most commonly list
    /// them — used by tests that want to exercise "every primitive".
    pub const ALL: [Self; 13] = [
        Self::Bool,
        Self::Byte,
        Self::Char,
        Self::Float32,
        Self::Float64,
        Self::Int8,
        Self::Uint8,
        Self::Int16,
        Self::Uint16,
        Self::Int32,
        Self::Uint32,
        Self::Int64,
        Self::Uint64,
    ];

    /// The `.msg` keyword spelling.
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Byte => "byte",
            Self::Char => "char",
            Self::Float32 => "float32",
            Self::Float64 => "float64",
            Self::Int8 => "int8",
            Self::Uint8 => "uint8",
            Self::Int16 => "int16",
            Self::Uint16 => "uint16",
            Self::Int32 => "int32",
            Self::Uint32 => "uint32",
            Self::Int64 => "int64",
            Self::Uint64 => "uint64",
        }
    }

    /// Parses a `.msg` keyword, or `None` if `text` is not one of the
    /// thirteen primitive keywords (including `string`/`wstring`, which are
    /// handled separately since they are not in [`Self::ALL`]).
    #[must_use]
    pub fn from_keyword(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.keyword() == text)
    }

    /// The legal `(min, max)` range for an integer literal of this kind, or
    /// `None` for [`Self::Bool`]/[`Self::Float32`]/[`Self::Float64`], which
    /// are not integer kinds at all (checked by `crate::parser::literal`
    /// through a different branch before this would ever be consulted).
    ///
    /// `byte` and `char` share `uint8`'s range — both map to the CDR octet.
    #[must_use]
    pub const fn int_range(self) -> Option<(i128, i128)> {
        match self {
            Self::Bool | Self::Float32 | Self::Float64 => None,
            Self::Byte | Self::Char | Self::Uint8 => Some((0, u8::MAX as i128)),
            Self::Int8 => Some((i8::MIN as i128, i8::MAX as i128)),
            Self::Uint16 => Some((0, u16::MAX as i128)),
            Self::Int16 => Some((i16::MIN as i128, i16::MAX as i128)),
            Self::Uint32 => Some((0, u32::MAX as i128)),
            Self::Int32 => Some((i32::MIN as i128, i32::MAX as i128)),
            Self::Uint64 => Some((0, u64::MAX as i128)),
            Self::Int64 => Some((i64::MIN as i128, i64::MAX as i128)),
        }
    }
}

/// The two non-primitive scalar keywords, `string` and `wstring`, each
/// optionally bounded (`string<=N`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringKind {
    /// `string` (UTF-8, `astrs_cdr::CdrSerde` via `String`/`str`).
    String,
    /// `wstring` (UTF-16, `astrs_cdr::WString`).
    WString,
}

impl StringKind {
    /// The `.msg` keyword spelling.
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::WString => "wstring",
        }
    }
}

/// A field type before an array suffix: an IDL primitive, `string`/`wstring`
/// (optionally bounded), or a reference to a message type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarType {
    /// One of the thirteen IDL primitives.
    Primitive(PrimitiveKind),
    /// `string` or `string<=N`.
    Str {
        /// Which of the two keywords.
        kind: StringKind,
        /// The `<=N` bound, if any.
        bound: Option<u32>,
    },
    /// A reference to another message type, namespaced (`pkg/Type`) or bare
    /// (`Type`, meaning "the same package").
    Named(NamedTypeRef),
}

/// A `pkg/Type` or bare `Type` reference to a message type, as written in a
/// field's type position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTypeRef {
    /// `Some("geometry_msgs")` for `geometry_msgs/Point`, `None` for a bare
    /// `Point` (same package as the file being parsed).
    pub package: Option<String>,
    /// The type name (`"Point"`).
    pub name: String,
    /// Location of the whole reference.
    pub span: Span,
}

/// The array/sequence suffix on a field type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArraySuffix {
    /// No suffix: one scalar value.
    None,
    /// `T[N]`: exactly `N` values.
    Fixed(u32),
    /// `T[]`: any number of values.
    Unbounded,
    /// `T[<=N]`: at most `N` values.
    Bounded(u32),
}

impl ArraySuffix {
    /// True for [`Self::None`].
    #[must_use]
    pub const fn is_scalar(self) -> bool {
        matches!(self, Self::None)
    }
}

/// A complete field type: a scalar type plus its array suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldType {
    /// The element type.
    pub scalar: ScalarType,
    /// The array/sequence wrapper, if any.
    pub array: ArraySuffix,
    /// Location of the whole type expression.
    pub span: Span,
}

/// A default or constant value.
///
/// Not `Eq` — [`Self::Float`] holds an `f64`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// `true` / `false`.
    Bool(bool),
    /// An integer, wide enough to hold any `int8..uint64` value before it is
    /// range-checked against the field's declared type.
    Int(i128),
    /// A floating-point value.
    Float(f64),
    /// A string or wstring value.
    Str(String),
    /// An array default: `[v, v, …]`.
    Array(Vec<Literal>),
}

impl Literal {
    /// A short name for this literal's shape, for
    /// [`crate::error::IdlError::InvalidLiteral`].
    #[must_use]
    pub const fn shape(&self) -> &'static str {
        match self {
            Self::Bool(_) => "a boolean",
            Self::Int(_) => "an integer",
            Self::Float(_) => "a floating-point number",
            Self::Str(_) => "a string",
            Self::Array(_) => "an array",
        }
    }
}

/// A `TYPE NAME=value` constant declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstantDecl {
    /// The declared type (always scalar and non-array — enforced during
    /// parsing).
    pub type_: FieldType,
    /// The constant's name (`UPPER_SNAKE_CASE`).
    pub name: String,
    /// Location of the name.
    pub name_span: Span,
    /// The value after `=`.
    pub value: Literal,
    /// Location of the value.
    pub value_span: Span,
    /// The joined leading-comment-block and same-line trailing comment, if
    /// any.
    pub comment: Option<String>,
    /// Location of the whole declaration.
    pub span: Span,
}

/// A `TYPE name [default]` field declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    /// The declared type.
    pub type_: FieldType,
    /// The field's name (`lower_snake_case`).
    pub name: String,
    /// Location of the name.
    pub name_span: Span,
    /// The default value, if one was written.
    pub default: Option<Literal>,
    /// Location of the default value, if any.
    pub default_span: Option<Span>,
    /// The joined leading-comment-block and same-line trailing comment, if
    /// any.
    pub comment: Option<String>,
    /// Location of the whole declaration.
    pub span: Span,
}

/// One line of a message section: either a constant or a field.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A `TYPE NAME=value` line.
    Constant(ConstantDecl),
    /// A `TYPE name [default]` line.
    Field(FieldDecl),
}

impl Item {
    /// The declaration's span, regardless of which kind it is.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Constant(c) => c.span,
            Self::Field(f) => f.span,
        }
    }

    /// The declared name, regardless of which kind it is.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Constant(c) => &c.name,
            Self::Field(f) => &f.name,
        }
    }
}

/// One `.msg`-shaped section of items: a whole `.msg` file, or one part
/// (request/response, goal/result/feedback) of a `.srv`/`.action` file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessageSection {
    /// The section's opening comment block — the message-level overview —
    /// joined with `\n`. Becomes the generated type's own rustdoc.
    ///
    /// This is the *first* comment block in the section, closed by
    /// whichever comes first: a blank line (the common ROS 2 convention of
    /// separating the overview from the first field's own comment, e.g.
    /// `builtin_interfaces/msg/Time.msg`) or the first field/constant
    /// declaration — or, for a section with no items at all, simply
    /// whatever trailing comment lines the section ends on. Once closed, it
    /// never grows back: comment blocks that follow attach to the next item
    /// instead, per the ordinary "a blank line resets the pending per-item
    /// comment" rule.
    pub leading_comment: Vec<String>,
    /// Constants and fields, in declaration order (blueprint §10.3's member
    /// ordering rule: this is also CDR wire order).
    pub items: Vec<Item>,
}

impl MessageSection {
    /// The section's fields only, in declaration order.
    pub fn fields(&self) -> impl Iterator<Item = &FieldDecl> {
        self.items.iter().filter_map(|item| match item {
            Item::Field(f) => Some(f),
            Item::Constant(_) => None,
        })
    }

    /// The section's constants only, in declaration order.
    pub fn constants(&self) -> impl Iterator<Item = &ConstantDecl> {
        self.items.iter().filter_map(|item| match item {
            Item::Constant(c) => Some(c),
            Item::Field(_) => None,
        })
    }
}

/// A parsed `.msg` file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessageFile {
    /// The file's one section.
    pub section: MessageSection,
}

/// A parsed `.srv` file: two sections split by one `---`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServiceFile {
    /// The fields before `---`.
    pub request: MessageSection,
    /// The fields after `---`.
    pub response: MessageSection,
}

/// A parsed `.action` file: three sections split by two `---`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ActionFile {
    /// The fields before the first `---`.
    pub goal: MessageSection,
    /// The fields between the two `---`.
    pub result: MessageSection,
    /// The fields after the second `---`.
    pub feedback: MessageSection,
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
    fn primitive_keyword_round_trips() {
        for kind in PrimitiveKind::ALL {
            assert_eq!(PrimitiveKind::from_keyword(kind.keyword()), Some(kind));
        }
        assert_eq!(PrimitiveKind::from_keyword("string"), None);
        assert_eq!(PrimitiveKind::from_keyword("nonsense"), None);
    }

    #[test]
    fn all_lists_exactly_the_thirteen_primitives() {
        assert_eq!(PrimitiveKind::ALL.len(), 13);
    }

    #[test]
    fn int_range_is_none_for_the_non_integer_kinds() {
        assert_eq!(PrimitiveKind::Bool.int_range(), None);
        assert_eq!(PrimitiveKind::Float32.int_range(), None);
        assert_eq!(PrimitiveKind::Float64.int_range(), None);
    }

    #[test]
    fn int_range_matches_the_rust_scalar_widths() {
        assert_eq!(PrimitiveKind::Int8.int_range(), Some((-128, 127)));
        assert_eq!(PrimitiveKind::Uint8.int_range(), Some((0, 255)));
        assert_eq!(
            PrimitiveKind::Byte.int_range(),
            PrimitiveKind::Uint8.int_range()
        );
        assert_eq!(
            PrimitiveKind::Char.int_range(),
            PrimitiveKind::Uint8.int_range()
        );
        assert_eq!(
            PrimitiveKind::Int64.int_range(),
            Some((i64::MIN as i128, i64::MAX as i128))
        );
        assert_eq!(
            PrimitiveKind::Uint64.int_range(),
            Some((0, u64::MAX as i128))
        );
    }

    #[test]
    fn array_suffix_is_scalar_only_for_none() {
        assert!(ArraySuffix::None.is_scalar());
        assert!(!ArraySuffix::Unbounded.is_scalar());
        assert!(!ArraySuffix::Fixed(3).is_scalar());
        assert!(!ArraySuffix::Bounded(3).is_scalar());
    }

    #[test]
    fn message_section_splits_fields_and_constants_preserving_order() {
        let section = MessageSection {
            leading_comment: Vec::new(),
            items: vec![
                Item::Constant(ConstantDecl {
                    type_: FieldType {
                        scalar: ScalarType::Primitive(PrimitiveKind::Int32),
                        array: ArraySuffix::None,
                        span: at(1, 1),
                    },
                    name: "A".to_owned(),
                    name_span: at(1, 1),
                    value: Literal::Int(1),
                    value_span: at(1, 1),
                    comment: None,
                    span: at(1, 1),
                }),
                Item::Field(FieldDecl {
                    type_: FieldType {
                        scalar: ScalarType::Primitive(PrimitiveKind::Bool),
                        array: ArraySuffix::None,
                        span: at(2, 1),
                    },
                    name: "flag".to_owned(),
                    name_span: at(2, 1),
                    default: None,
                    default_span: None,
                    comment: None,
                    span: at(2, 1),
                }),
            ],
        };
        assert_eq!(section.constants().count(), 1);
        assert_eq!(section.fields().count(), 1);
        assert_eq!(section.fields().next().unwrap().name, "flag");
    }

    #[test]
    fn item_span_and_name_dispatch_by_kind() {
        let field = Item::Field(FieldDecl {
            type_: FieldType {
                scalar: ScalarType::Primitive(PrimitiveKind::Int8),
                array: ArraySuffix::None,
                span: at(3, 1),
            },
            name: "x".to_owned(),
            name_span: at(3, 3),
            default: None,
            default_span: None,
            comment: None,
            span: at(3, 1),
        });
        assert_eq!(field.name(), "x");
        assert_eq!(field.span(), at(3, 1));
    }

    #[test]
    fn literal_shape_names_every_variant() {
        assert_eq!(Literal::Bool(true).shape(), "a boolean");
        assert_eq!(Literal::Int(1).shape(), "an integer");
        assert_eq!(Literal::Float(1.0).shape(), "a floating-point number");
        assert_eq!(Literal::Str(String::new()).shape(), "a string");
        assert_eq!(Literal::Array(Vec::new()).shape(), "an array");
    }
}
