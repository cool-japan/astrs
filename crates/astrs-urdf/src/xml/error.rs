//! [`XmlError`] and [`XmlErrorKind`] — everything [`super::Reader`] can
//! reject a document for, each carrying the exact [`Span`] the problem sits
//! at.
//!
//! Unlike some error taxonomies in this workspace (see e.g.
//! `astrs-yaml::Error`, whose span is an `Option` because a `serde` error
//! can be raised with no source location in hand), every [`XmlError`] this
//! module produces has a real span: the reader always knows exactly which
//! character it was looking at when a document stopped being well-formed.

use super::position::Span;

/// An XML syntax error, located at a [`Span`] in the source document.
///
/// # Examples
///
/// ```
/// use astrs_urdf::xml::{Event, Reader, XmlErrorKind};
///
/// let mut reader = Reader::new("<a><b></a>");
/// assert!(matches!(reader.next_event(), Ok(Event::StartElement { name: "a", .. })));
/// assert!(matches!(reader.next_event(), Ok(Event::StartElement { name: "b", .. })));
/// let error = reader.next_event().unwrap_err();
/// assert!(matches!(
///     error.kind,
///     XmlErrorKind::MismatchedClosingTag { .. }
/// ));
/// assert_eq!(error.span.start.line, 1);
/// ```
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{kind} at {span}")]
pub struct XmlError {
    /// What went wrong.
    pub kind: XmlErrorKind,
    /// Where it went wrong.
    pub span: Span,
}

impl XmlError {
    /// Builds an error of `kind`, located at `span`.
    #[must_use]
    pub(crate) fn new(kind: XmlErrorKind, span: impl Into<Span>) -> Self {
        Self {
            kind,
            span: span.into(),
        }
    }

    /// Renders this error together with the offending source line — see
    /// [`Span::render`].
    #[must_use]
    pub fn render(&self, source: &str) -> String {
        match self.span.render(source) {
            Some(caret) => format!("{self}\n{caret}"),
            None => self.to_string(),
        }
    }
}

/// What, specifically, went wrong while scanning a document.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum XmlErrorKind {
    /// The input is larger than [`super::Limits::max_input_bytes`].
    #[error("input is {actual} bytes, over the {limit}-byte limit")]
    InputTooLarge {
        /// The configured ceiling.
        limit: usize,
        /// The input's actual size.
        actual: usize,
    },

    /// Element nesting exceeded [`super::Limits::max_depth`].
    #[error("element nesting exceeds the depth limit of {limit}")]
    DepthLimitExceeded {
        /// The configured ceiling.
        limit: usize,
    },

    /// The input ended while a well-formed document still expected more —
    /// `expected` names what (e.g. `"the end of a comment"`).
    #[error("unexpected end of input while looking for {expected}")]
    UnexpectedEof {
        /// A short, human-readable description of what was expected.
        expected: &'static str,
    },

    /// A character appeared somewhere the grammar does not allow it.
    #[error("unexpected character {found:?}")]
    UnexpectedCharacter {
        /// The offending character.
        found: char,
    },

    /// An element or attribute name was empty, or started with a character
    /// that is not a legal XML `NameStartChar`.
    #[error("expected an element or attribute name")]
    InvalidName,

    /// A `<!-- ... -->` comment never saw its closing `-->`.
    #[error("unterminated comment (missing `-->`)")]
    UnterminatedComment,

    /// A `<![CDATA[ ... ]]>` section never saw its closing `]]>`.
    #[error("unterminated CDATA section (missing `]]>`)")]
    UnterminatedCData,

    /// A `<? ... ?>` processing instruction never saw its closing `?>`.
    #[error("unterminated processing instruction (missing `?>`)")]
    UnterminatedProcessingInstruction,

    /// A `<!DOCTYPE ...>` declaration never saw a balanced closing `>`.
    #[error("unterminated DOCTYPE declaration")]
    UnterminatedDoctype,

    /// A quoted attribute value never saw its closing quote.
    #[error("unterminated attribute value (missing closing quote)")]
    UnterminatedAttributeValue,

    /// A `<tag ...` start or end tag never saw its closing `>`.
    #[error("unterminated tag (missing `>`)")]
    UnterminatedTag,

    /// A `</found>` end tag was seen while `<expected>` was the innermost
    /// open element.
    #[error("closing tag `</{found}>` does not match open tag `<{expected}>`")]
    MismatchedClosingTag {
        /// The name of the innermost still-open element.
        expected: String,
        /// The name the closing tag actually named.
        found: String,
    },

    /// A `</found>` end tag was seen with no open element at all.
    #[error("closing tag `</{found}>` has no matching open tag")]
    UnexpectedClosingTag {
        /// The name the closing tag named.
        found: String,
    },

    /// The same attribute name appeared twice on one element.
    #[error("duplicate attribute `{name}`")]
    DuplicateAttribute {
        /// The repeated attribute name.
        name: String,
    },

    /// An attribute name was not followed by `=`.
    #[error("expected `=` after attribute name `{name}`")]
    MissingAttributeEquals {
        /// The attribute name that was missing its `=`.
        name: String,
    },

    /// An attribute's value did not start with `'` or `"`.
    #[error("attribute value must start with a single or double quote")]
    MissingAttributeQuote,

    /// `&name;` did not name one of the five predefined XML entities
    /// (`amp`, `lt`, `gt`, `apos`, `quot`).
    #[error("unknown entity reference `&{name};`")]
    UnknownEntity {
        /// The unrecognized entity name.
        name: String,
    },

    /// A `&` was not followed by a well-formed entity or character
    /// reference (a name or `#`/`#x` digits, terminated by `;`).
    #[error("malformed character/entity reference `&{text}`")]
    MalformedReference {
        /// As much of the malformed reference as was recovered, for the
        /// message.
        text: String,
    },

    /// A numeric character reference (`&#N;` or `&#xN;`) did not name a
    /// legal XML character (outside the `Char` production, e.g. a raw
    /// control code, an unpaired surrogate, or larger than `U+10FFFF`).
    #[error("character reference `&#{text};` is not a valid XML character")]
    InvalidCharacterReference {
        /// The digits between `#`/`#x` and `;`.
        text: String,
    },

    /// The document had no top-level element at all (empty, or only
    /// whitespace/comments/processing instructions).
    #[error("document has no root element")]
    MissingRootElement,

    /// Non-whitespace text appeared before the document's root element.
    #[error("unexpected content before the root element")]
    ContentBeforeRoot,

    /// A second top-level element (or stray non-whitespace text) appeared
    /// after the root element had already closed.
    #[error("unexpected content after the root element closed")]
    ContentAfterRoot,

    /// The input ended with one or more elements still open.
    #[error("unclosed element `<{name}>` (missing `</{name}>`)")]
    UnclosedElement {
        /// The innermost still-open element's name.
        name: String,
    },

    /// [`super::Reader::from_utf8_bytes`] was given bytes that are not
    /// well-formed UTF-8.
    #[error("input is not valid UTF-8")]
    InvalidUtf8,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::xml::position::Position;

    #[test]
    fn display_names_the_kind_and_the_span() {
        let error = XmlError::new(
            XmlErrorKind::UnexpectedClosingTag {
                found: "b".to_owned(),
            },
            Position::new(3, 4, 20),
        );
        assert_eq!(
            error.to_string(),
            "closing tag `</b>` has no matching open tag at 3:4"
        );
    }

    #[test]
    fn render_appends_the_caret_line() {
        let error = XmlError::new(XmlErrorKind::InvalidName, Position::new(1, 2, 1));
        let rendered = error.render("<1a/>");
        assert!(rendered.contains("<1a/>"));
        assert!(rendered.ends_with('^'));
    }

    #[test]
    fn is_a_std_error() {
        fn assert_std_error<E: std::error::Error>() {}
        assert_std_error::<XmlError>();
    }
}
