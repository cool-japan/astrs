//! The error taxonomy every fallible entry point in this crate returns.
//!
//! [`Error`] is deliberately one type for three different jobs — scanning,
//! tree building, and `serde` (de)serialization — because a caller reading a
//! manifest does not care which of the three noticed the problem, only where
//! it is and what it was. What varies is [`ErrorKind`], which is
//! fine-grained enough that `astrs-manifest` can special-case a duplicate key
//! or a resource limit without string-matching a message.
//!
//! Two properties are load-bearing:
//!
//! - **A location whenever one exists.** [`Error::span`] is `Some` for every
//!   failure the parser raises, and `astrs-manifest` can turn it into a
//!   caret under the offending source line with [`Span::render`].
//! - **No allocation on the happy path.** [`Error`] boxes its payload, so
//!   `Result<Value, Error>` stays pointer-sized-ish and the parser's hot
//!   loop does not pay for the error case it (almost) never takes.

use std::fmt;

use crate::position::{Position, Span};

/// The result type every fallible operation in this crate returns.
pub type Result<T> = std::result::Result<T, Error>;

/// A YAML scanning, parsing, or `serde` conversion failure.
///
/// # Examples
///
/// ```
/// use astrs_yaml::{ErrorKind, Value};
///
/// let error = astrs_yaml::from_str::<Value>("a: 1\na: 2\n").unwrap_err();
/// assert!(matches!(error.kind(), ErrorKind::DuplicateKey { .. }));
/// assert_eq!(error.line(), Some(2));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    inner: Box<ErrorImpl>,
}

#[derive(Debug, Clone, PartialEq)]
struct ErrorImpl {
    kind: ErrorKind,
    span: Option<Span>,
}

impl Error {
    /// Build an error of `kind` with no known source location.
    ///
    /// Used for failures that are not tied to a byte of input — a `serde`
    /// type mismatch raised by a `Deserialize` implementation, say.
    #[must_use]
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            inner: Box::new(ErrorImpl { kind, span: None }),
        }
    }

    /// Build an error of `kind` located at `span`.
    #[must_use]
    pub fn at(kind: ErrorKind, span: impl Into<Span>) -> Self {
        Self {
            inner: Box::new(ErrorImpl {
                kind,
                span: Some(span.into()),
            }),
        }
    }

    /// Attach `span` to an error that does not already carry one.
    ///
    /// This is how a `serde` error raised deep inside a `Deserialize`
    /// implementation — which has no idea where in the document it is —
    /// picks up the location of the value it was looking at.
    #[must_use]
    pub fn or_span(mut self, span: impl Into<Span>) -> Self {
        if self.inner.span.is_none() {
            self.inner.span = Some(span.into());
        }
        self
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &ErrorKind {
        &self.inner.kind
    }

    /// Where it went wrong, when the failure is tied to source text.
    #[must_use]
    pub fn span(&self) -> Option<Span> {
        self.inner.span
    }

    /// The one-based line number of the failure, when known.
    ///
    /// Mirrors `serde_yaml::Error::location().map(|l| l.line())`, so a
    /// caller migrating off `serde_yaml` keeps the same shape.
    #[must_use]
    pub fn line(&self) -> Option<usize> {
        self.inner.span.map(|span| span.start.line)
    }

    /// The one-based column number of the failure, when known.
    #[must_use]
    pub fn column(&self) -> Option<usize> {
        self.inner.span.map(|span| span.start.column)
    }

    /// True when this failure is a resource limit rather than malformed
    /// input — a depth cap, an input-size cap, or an alias expansion budget.
    ///
    /// Compatibility tests use this: a document this crate rejects on a
    /// limit is not a disagreement with another parser about what the
    /// document *means*, it is a deliberate refusal to build the value at
    /// all (see [`crate::Limits`]).
    #[must_use]
    pub fn is_limit(&self) -> bool {
        matches!(
            self.inner.kind,
            ErrorKind::InputTooLarge { .. }
                | ErrorKind::DepthLimitExceeded { .. }
                | ErrorKind::AliasBudgetExhausted { .. }
        )
    }

    /// Render this error together with the offending source line.
    ///
    /// Returns the plain [`Display`](fmt::Display) form when the error has
    /// no span, or when `source` is not the document it came from.
    #[must_use]
    pub fn render(&self, source: &str) -> String {
        match self.inner.span.and_then(|span| span.render(source)) {
            Some(caret) => format!("{self}\n{caret}"),
            None => self.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner.span {
            Some(span) => write!(f, "{} at {}", self.inner.kind, span.start),
            None => fmt::Display::fmt(&self.inner.kind, f),
        }
    }
}

impl std::error::Error for Error {}

impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::new(ErrorKind::Message(msg.to_string()))
    }
}

impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::new(ErrorKind::Message(msg.to_string()))
    }
}

/// Everything that can go wrong reading or writing YAML.
///
/// The variants split into four groups:
///
/// 1. **Lexical** — [`ErrorKind::UnexpectedCharacter`],
///    [`ErrorKind::UnexpectedEndOfInput`], [`ErrorKind::TabInIndentation`],
///    [`ErrorKind::ControlCharacter`], [`ErrorKind::UnknownEscape`],
///    [`ErrorKind::InvalidEscapeValue`]: the bytes do not spell a YAML
///    token.
/// 2. **Structural** — [`ErrorKind::MappingValueNotAllowed`],
///    [`ErrorKind::DuplicateKey`], [`ErrorKind::ExpectedNodeContent`],
///    [`ErrorKind::UnexpectedDocumentEnd`],
///    [`ErrorKind::MultipleDocuments`], [`ErrorKind::UnclosedFlow`],
///    [`ErrorKind::InvalidIndentation`]: the tokens are fine but do not
///    compose into a document.
/// 3. **Resolution** — [`ErrorKind::UnknownAnchor`],
///    [`ErrorKind::TagMismatch`], [`ErrorKind::UnknownTagHandle`],
///    [`ErrorKind::InvalidTag`], [`ErrorKind::IntegerOutOfRange`],
///    [`ErrorKind::InvalidDirective`]: a node names something that does not
///    resolve.
/// 4. **Limits and `serde`** — [`ErrorKind::InputTooLarge`],
///    [`ErrorKind::DepthLimitExceeded`],
///    [`ErrorKind::AliasBudgetExhausted`], [`ErrorKind::Message`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A character appeared where the grammar does not allow it.
    UnexpectedCharacter {
        /// The offending character.
        found: char,
        /// What the scanner was in the middle of reading.
        context: &'static str,
    },

    /// The document ended in the middle of a construct.
    UnexpectedEndOfInput {
        /// What was still open — `"a double-quoted scalar"`, `"a flow
        /// mapping"`.
        context: &'static str,
    },

    /// A character YAML does not allow in a stream at all appeared in the
    /// input.
    ///
    /// YAML 1.2's `c-printable` production excludes the C0 controls other
    /// than tab and line feed, `DEL`, the C1 controls, and the two
    /// non-characters `U+FFFE`/`U+FFFF`. They are rejected outright rather
    /// than carried into a [`crate::Value`], because a control character in
    /// a configuration file is either a corrupted download or an injection
    /// attempt, and in both cases silently accepting it is worse than
    /// failing. Write `"\x01"` in a double-quoted scalar to put one in a
    /// *value* deliberately.
    ControlCharacter {
        /// The offending character.
        found: char,
    },

    /// A tab character was used for indentation.
    ///
    /// YAML forbids this outright (a tab's width is not defined), and
    /// silently accepting it is how a manifest renders differently in two
    /// editors. Tabs *inside* a scalar are fine and are not reported here.
    TabInIndentation,

    /// A block collection entry sat at a column its siblings do not share.
    InvalidIndentation {
        /// The indentation the enclosing collection established.
        expected: usize,
        /// The indentation this entry actually has.
        found: usize,
    },

    /// A `key: value` pair appeared where only a value is allowed — the
    /// `a: b: c` shape.
    MappingValueNotAllowed,

    /// The same key appeared twice in one mapping.
    ///
    /// Rejected rather than last-wins: a duplicate `id:` in a manifest is
    /// always a mistake, and silently dropping one of them is the worst
    /// possible outcome.
    DuplicateKey {
        /// The repeated key, rendered for a human.
        key: String,
    },

    /// A node was required but the input had none — `...` before any
    /// document content, for instance.
    ExpectedNodeContent,

    /// A `...` document-end marker appeared where no document was open.
    UnexpectedDocumentEnd,

    /// [`crate::from_str`] was handed a stream with more than one document.
    ///
    /// Use [`crate::from_str_multi`] when several documents are expected.
    MultipleDocuments {
        /// How many documents the stream actually contained.
        found: usize,
    },

    /// A flow collection was never closed.
    UnclosedFlow {
        /// The bracket that would have closed it: `']'` or `'}'`.
        expected: char,
    },

    /// A `*alias` named an anchor that has not been defined (yet).
    UnknownAnchor {
        /// The alias name as written, without the `*`.
        name: String,
    },

    /// An explicit `!!` tag demanded a type the scalar cannot spell —
    /// `!!int 1e3`, `!!bool yes`.
    TagMismatch {
        /// The tag as written.
        tag: &'static str,
        /// The scalar's text.
        literal: String,
    },

    /// A tag shorthand used a handle no `%TAG` directive defined.
    UnknownTagHandle {
        /// The handle as written, including both `!` delimiters.
        handle: String,
    },

    /// A tag was syntactically malformed.
    InvalidTag {
        /// Why it could not be read.
        reason: &'static str,
    },

    /// A `%` directive could not be understood.
    InvalidDirective {
        /// The directive name, without the `%`.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// A decimal, hex, octal or binary integer literal does not fit in
    /// either [`i64`] or [`u64`].
    ///
    /// Reported rather than silently widened to a float, which would lose
    /// digits without saying so.
    IntegerOutOfRange {
        /// The literal as written.
        literal: String,
    },

    /// An escape sequence this crate does not implement.
    UnknownEscape {
        /// The character following the backslash.
        found: char,
    },

    /// A `\x`/`\u`/`\U` escape named something that is not a Unicode scalar
    /// value, or was truncated.
    InvalidEscapeValue {
        /// The escape text as written, without the leading backslash.
        literal: String,
    },

    /// The input is larger than [`crate::Limits::max_input_bytes`].
    InputTooLarge {
        /// The configured ceiling, in bytes.
        limit: usize,
        /// The input's actual length, in bytes.
        found: usize,
    },

    /// Collections nested deeper than [`crate::Limits::max_depth`].
    DepthLimitExceeded {
        /// The configured ceiling.
        limit: usize,
    },

    /// Alias expansion would have materialized more nodes than
    /// [`crate::Limits::max_alias_nodes`] allows.
    ///
    /// This is the billion-laughs guard: a document whose anchors multiply
    /// each other is refused at the point the budget runs out rather than
    /// after it has exhausted memory.
    AliasBudgetExhausted {
        /// The configured ceiling, in materialized nodes.
        limit: usize,
    },

    /// A `serde` `Serialize`/`Deserialize` implementation reported a
    /// problem, or a conversion this crate cannot express was requested.
    Message(String),
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedCharacter { found, context } => {
                write!(f, "unexpected character {found:?} while reading {context}")
            }
            Self::UnexpectedEndOfInput { context } => {
                write!(f, "unexpected end of input while reading {context}")
            }
            Self::ControlCharacter { found } => write!(
                f,
                "the control character U+{:04X} is not allowed in a YAML stream (write it as an escape inside a double-quoted scalar)",
                *found as u32
            ),
            Self::TabInIndentation => f.write_str("a tab character cannot be used for indentation"),
            Self::InvalidIndentation { expected, found } => write!(
                f,
                "wrong indentation: this entry is indented {found} column(s) but its siblings are indented {expected}"
            ),
            Self::MappingValueNotAllowed => {
                f.write_str("a mapping value is not allowed in this context")
            }
            Self::DuplicateKey { key } => write!(f, "duplicate mapping key `{key}`"),
            Self::ExpectedNodeContent => f.write_str("expected node content"),
            Self::UnexpectedDocumentEnd => {
                f.write_str("`...` ends a document, but no document is open")
            }
            Self::MultipleDocuments { found } => write!(
                f,
                "expected a single YAML document but the stream holds {found}"
            ),
            Self::UnclosedFlow { expected } => {
                write!(f, "unclosed flow collection: expected {expected:?}")
            }
            Self::UnknownAnchor { name } => write!(f, "unknown anchor `{name}`"),
            Self::TagMismatch { tag, literal } => {
                write!(f, "scalar {literal:?} is not a valid `{tag}`")
            }
            Self::UnknownTagHandle { handle } => {
                write!(
                    f,
                    "undefined tag handle `{handle}` (no matching %TAG directive)"
                )
            }
            Self::InvalidTag { reason } => write!(f, "malformed tag: {reason}"),
            Self::InvalidDirective { name, reason } => {
                write!(f, "malformed %{name} directive: {reason}")
            }
            Self::IntegerOutOfRange { literal } => {
                write!(f, "integer `{literal}` does not fit in either i64 or u64")
            }
            Self::UnknownEscape { found } => {
                write!(f, "unknown escape sequence `\\{found}`")
            }
            Self::InvalidEscapeValue { literal } => {
                write!(f, "escape `\\{literal}` is not a Unicode scalar value")
            }
            Self::InputTooLarge { limit, found } => {
                write!(f, "input is {found} byte(s), over the {limit}-byte limit")
            }
            Self::DepthLimitExceeded { limit } => {
                write!(f, "collections nested deeper than the limit of {limit}")
            }
            Self::AliasBudgetExhausted { limit } => write!(
                f,
                "alias expansion would exceed the budget of {limit} node(s)"
            ),
            Self::Message(message) => f.write_str(message),
        }
    }
}

/// Shorthand used throughout the parser for `Err(Error::at(kind, span))`.
pub(crate) fn err_at<T>(kind: ErrorKind, position: Position) -> Result<T> {
    Err(Error::at(kind, Span::point(position)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn the_error_type_slots_into_a_thiserror_source_position() {
        // `astrs-manifest` carries the underlying YAML error as a
        // `#[source]`, which requires exactly these bounds. Asserting them
        // here means a future field cannot quietly break that call site.
        fn assert_bounds<T: std::error::Error + Send + Sync + Clone + 'static>() {}
        assert_bounds::<Error>();
    }

    #[test]
    fn an_error_without_a_span_displays_only_its_kind() {
        let error = Error::new(ErrorKind::MappingValueNotAllowed);
        assert_eq!(
            error.to_string(),
            "a mapping value is not allowed in this context"
        );
        assert_eq!(error.line(), None);
        assert_eq!(error.column(), None);
        assert_eq!(error.span(), None);
    }

    #[test]
    fn an_error_with_a_span_displays_a_location() {
        let error = Error::at(
            ErrorKind::TabInIndentation,
            Span::point(Position::new(4, 2)),
        );
        assert_eq!(
            error.to_string(),
            "a tab character cannot be used for indentation at 4:2"
        );
        assert_eq!(error.line(), Some(4));
        assert_eq!(error.column(), Some(2));
    }

    #[test]
    fn or_span_fills_in_only_a_missing_location() {
        let bare = Error::new(ErrorKind::ExpectedNodeContent).or_span(Position::new(2, 2));
        assert_eq!(bare.line(), Some(2));

        let already = Error::at(ErrorKind::ExpectedNodeContent, Position::new(1, 1))
            .or_span(Position::new(9, 9));
        assert_eq!(already.line(), Some(1));
    }

    #[test]
    fn limit_errors_are_distinguishable_from_malformed_input() {
        assert!(Error::new(ErrorKind::DepthLimitExceeded { limit: 128 }).is_limit());
        assert!(Error::new(ErrorKind::AliasBudgetExhausted { limit: 1 }).is_limit());
        assert!(Error::new(ErrorKind::InputTooLarge { limit: 1, found: 2 }).is_limit());
        assert!(!Error::new(ErrorKind::MappingValueNotAllowed).is_limit());
    }

    #[test]
    fn render_draws_a_caret_under_the_offending_column() {
        let error = Error::at(ErrorKind::TabInIndentation, Position::new(2, 1));
        let rendered = error.render("a: 1\n\tb: 2\n");
        assert!(rendered.ends_with("\tb: 2\n^"), "{rendered}");
    }

    #[test]
    fn render_degrades_to_display_without_a_usable_span() {
        let error = Error::new(ErrorKind::ExpectedNodeContent);
        assert_eq!(error.render("a: 1\n"), "expected node content");

        let off_document = Error::at(ErrorKind::ExpectedNodeContent, Position::new(99, 1));
        assert_eq!(
            off_document.render("a: 1\n"),
            "expected node content at 99:1"
        );
    }

    #[test]
    fn serde_custom_errors_carry_their_message() {
        use serde::de::Error as _;
        let error = <Error as serde::de::Error>::custom("boom");
        assert_eq!(error.to_string(), "boom");
        let ser = <Error as serde::ser::Error>::custom("bang");
        assert_eq!(ser.to_string(), "bang");
        assert!(matches!(Error::custom("x").kind(), ErrorKind::Message(_)));
    }

    #[test]
    fn every_kind_renders_a_non_empty_message() {
        let kinds = [
            ErrorKind::UnexpectedCharacter {
                found: '@',
                context: "a plain scalar",
            },
            ErrorKind::UnexpectedEndOfInput {
                context: "a flow mapping",
            },
            ErrorKind::TabInIndentation,
            ErrorKind::ControlCharacter { found: '\u{1}' },
            ErrorKind::InvalidIndentation {
                expected: 2,
                found: 3,
            },
            ErrorKind::MappingValueNotAllowed,
            ErrorKind::DuplicateKey { key: "a".into() },
            ErrorKind::ExpectedNodeContent,
            ErrorKind::UnexpectedDocumentEnd,
            ErrorKind::MultipleDocuments { found: 3 },
            ErrorKind::UnclosedFlow { expected: ']' },
            ErrorKind::UnknownAnchor { name: "x".into() },
            ErrorKind::TagMismatch {
                tag: "!!int",
                literal: "x".into(),
            },
            ErrorKind::UnknownTagHandle {
                handle: "!e!".into(),
            },
            ErrorKind::InvalidTag { reason: "empty" },
            ErrorKind::InvalidDirective {
                name: "TAG".into(),
                reason: "missing prefix",
            },
            ErrorKind::IntegerOutOfRange {
                literal: "1".into(),
            },
            ErrorKind::UnknownEscape { found: 'q' },
            ErrorKind::InvalidEscapeValue {
                literal: "uD800".into(),
            },
            ErrorKind::InputTooLarge { limit: 1, found: 2 },
            ErrorKind::DepthLimitExceeded { limit: 128 },
            ErrorKind::AliasBudgetExhausted { limit: 10 },
            ErrorKind::Message("hi".into()),
        ];
        for kind in kinds {
            assert!(!kind.to_string().is_empty(), "{kind:?}");
        }
    }
}
