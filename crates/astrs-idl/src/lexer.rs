//! The tokenizer for one line of `.msg`/`.srv`/`.action` source.
//!
//! The grammar (blueprint §10.3) is line-oriented — a field or constant
//! declaration never spans more than one line, and the `---` section
//! separator is recognised as a whole line before any tokenizing happens.
//! [`classify_line`] is therefore the whole lexical layer: given one line's
//! text it returns a [`Line`] telling [`crate::parser`] whether the line was
//! blank, a comment, a separator, or a run of [`Token`]s (with an optional
//! trailing `# comment`) ready for the recursive-descent grammar in
//! `crate::parser`'s private `type_spec` and `literal` submodules.
//!
//! # Token classes
//!
//! Alphabetic identifiers are lexed uniformly as [`TokenKind::Ident`],
//! whether they spell a primitive type keyword (`int32`), a boolean literal
//! (`true`), a package or type name, or a field/constant name. Keeping type
//! keywords out of the token kind is deliberate: it lets the parser accept
//! any identifier in name position and reject `int32 int32` with the
//! specific [`crate::error::IdlError::ReservedIdentifier`] instead of a
//! generic "expected identifier" syntax error.
//!
//! Numeric lexemes are collected by a permissive maximal munch — a leading
//! sign, digits, letters (for `0x` hex and exponents), `.` and internal
//! signs — and handed to `crate::parser::literal` as raw text for strict
//! parsing. The split keeps this module a fast, unopinionated classifier and
//! keeps "is `1.2.3` a valid number" a parser-layer question with one answer,
//! not two.

use std::iter::Peekable;
use std::str::CharIndices;

use crate::error::IdlError;
use crate::span::{Position, Span};

/// One lexical token and the span of source text it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Where it came from.
    pub span: Span,
}

/// The kinds of token this grammar's lines are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// `[A-Za-z_][A-Za-z0-9_]*` — a type keyword, `true`/`false`, or a
    /// field/constant/type/package name, disambiguated by the parser.
    Ident(String),
    /// A maximal run of characters that could start a number: digits, a
    /// leading sign, `.`, letters (for `0x`/exponents) and internal signs.
    /// Validated strictly in `crate::parser::literal`.
    Number(String),
    /// The unescaped content of a `"`- or `'`-quoted string literal.
    Str(String),
    /// `/` — the package/type separator.
    Slash,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `<=`
    LessEq,
    /// `=`
    Eq,
    /// `,`
    Comma,
}

impl TokenKind {
    /// A short human-readable name, for [`IdlError::UnexpectedToken`]'s
    /// `found` field.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Ident(text) => text.clone(),
            Self::Number(text) => text.clone(),
            Self::Str(text) => format!("{text:?}"),
            Self::Slash => "/".to_owned(),
            Self::LBracket => "[".to_owned(),
            Self::RBracket => "]".to_owned(),
            Self::LessEq => "<=".to_owned(),
            Self::Eq => "=".to_owned(),
            Self::Comma => ",".to_owned(),
        }
    }
}

/// The classification of one source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Empty, or whitespace only.
    Blank,
    /// A `#`-comment with nothing else on the line. The text has the
    /// leading `#` stripped and is trimmed of one following space, if any.
    CommentOnly(String),
    /// The literal `---` section separator, alone on the line.
    Separator,
    /// A field or constant declaration: its tokens, and an optional
    /// trailing `# comment` (stripped and trimmed the same way as
    /// [`Line::CommentOnly`]).
    Tokens(Vec<Token>, Option<String>),
}

/// Tokenizes one line (1-based `line_no`) of `.msg`/`.srv`/`.action` source.
///
/// # Errors
///
/// The lexical group of [`IdlError`]: [`IdlError::UnterminatedString`],
/// [`IdlError::InvalidEscape`], [`IdlError::UnexpectedCharacter`].
pub fn classify_line(line_no: u32, raw: &str) -> Result<Line, IdlError> {
    let trimmed = raw.trim_end_matches(['\r']);
    if trimmed.trim().is_empty() {
        return Ok(Line::Blank);
    }
    if trimmed.trim() == "---" {
        return Ok(Line::Separator);
    }

    let mut scanner = Scanner::new(line_no, trimmed);
    scanner.skip_whitespace();
    if scanner.at_comment_start() {
        let text = scanner.take_comment_text();
        return Ok(Line::CommentOnly(text));
    }

    let mut tokens = Vec::new();
    loop {
        scanner.skip_whitespace();
        if scanner.at_end() {
            return Ok(Line::Tokens(tokens, None));
        }
        if scanner.at_comment_start() {
            let text = scanner.take_comment_text();
            return Ok(Line::Tokens(tokens, Some(text)));
        }
        tokens.push(scanner.next_token()?);
    }
}

/// A one-line character scanner tracking a 1-based column.
struct Scanner<'a> {
    line_no: u32,
    text: &'a str,
    iter: Peekable<CharIndices<'a>>,
    col: u32,
}

impl<'a> Scanner<'a> {
    fn new(line_no: u32, text: &'a str) -> Self {
        Self {
            line_no,
            text,
            iter: text.char_indices().peekable(),
            col: 1,
        }
    }

    fn pos(&self) -> Position {
        Position::new(self.line_no, self.col)
    }

    fn at_end(&mut self) -> bool {
        self.iter.peek().is_none()
    }

    fn peek_char(&mut self) -> Option<char> {
        self.iter.peek().map(|(_, c)| *c)
    }

    fn peek_byte(&mut self) -> Option<usize> {
        self.iter.peek().map(|(byte, _)| *byte)
    }

    /// Consumes and returns the next character, advancing the column.
    fn bump(&mut self) -> Option<char> {
        let (_, c) = self.iter.next()?;
        self.col += 1;
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek_char(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    fn at_comment_start(&mut self) -> bool {
        self.peek_char() == Some('#')
    }

    /// Consumes `#` and the rest of the line, stripped of the `#` and one
    /// leading space.
    fn take_comment_text(&mut self) -> String {
        self.bump(); // '#'
        if self.peek_char() == Some(' ') {
            self.bump();
        }
        let start = self.peek_byte().unwrap_or(self.text.len());
        self.text[start..].to_owned()
    }

    fn next_token(&mut self) -> Result<Token, IdlError> {
        let start_pos = self.pos();
        let start_byte = self.peek_byte().unwrap_or(self.text.len());
        let Some(c) = self.peek_char() else {
            return Err(IdlError::UnexpectedEndOfLine {
                expected: "a token",
                span: Span::empty(start_pos),
            });
        };

        let kind = match c {
            '/' => {
                self.bump();
                TokenKind::Slash
            }
            '[' => {
                self.bump();
                TokenKind::LBracket
            }
            ']' => {
                self.bump();
                TokenKind::RBracket
            }
            ',' => {
                self.bump();
                TokenKind::Comma
            }
            '=' => {
                self.bump();
                TokenKind::Eq
            }
            '<' => {
                self.bump();
                if self.peek_char() == Some('=') {
                    self.bump();
                    TokenKind::LessEq
                } else {
                    return Err(IdlError::UnexpectedCharacter {
                        found: '<',
                        span: Span::from_len(start_pos, 1),
                    });
                }
            }
            '"' | '\'' => return self.scan_string(c),
            c if c.is_ascii_digit() => self.scan_number(start_byte),
            '+' | '-' if self.starts_signed_number() => self.scan_number(start_byte),
            c if is_ident_start(c) => self.scan_ident(start_byte),
            other => {
                self.bump();
                return Err(IdlError::UnexpectedCharacter {
                    found: other,
                    span: Span::from_len(start_pos, 1),
                });
            }
        };

        let end_pos = self.pos();
        Ok(Token {
            kind,
            span: Span::new(start_pos, end_pos),
        })
    }

    /// True when the character under the cursor is `+`/`-` immediately
    /// followed by a digit — the only context a sign starts a number rather
    /// than being some other (currently unsupported, hence rejected)
    /// punctuation.
    fn starts_signed_number(&mut self) -> bool {
        let mut probe = self.iter.clone();
        probe.next(); // the sign itself
        matches!(probe.peek(), Some((_, c)) if c.is_ascii_digit())
    }

    fn scan_number(&mut self, start_byte: usize) -> TokenKind {
        self.bump(); // the leading digit or sign
        while matches!(
            self.peek_char(),
            Some(c) if c.is_ascii_alphanumeric() || c == '.' || c == '+' || c == '-' || c == '_'
        ) {
            self.bump();
        }
        let end_byte = self.peek_byte().unwrap_or(self.text.len());
        TokenKind::Number(self.text[start_byte..end_byte].to_owned())
    }

    fn scan_ident(&mut self, start_byte: usize) -> TokenKind {
        self.bump();
        while matches!(self.peek_char(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            self.bump();
        }
        let end_byte = self.peek_byte().unwrap_or(self.text.len());
        TokenKind::Ident(self.text[start_byte..end_byte].to_owned())
    }

    fn scan_string(&mut self, quote: char) -> Result<Token, IdlError> {
        let start_pos = self.pos();
        self.bump(); // opening quote
        let mut value = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(IdlError::UnterminatedString {
                        span: Span::new(start_pos, self.pos()),
                    });
                }
                Some(c) if c == quote => break,
                Some('\\') => {
                    let escape_pos = self.pos();
                    match self.bump() {
                        Some('\\') => value.push('\\'),
                        Some('"') => value.push('"'),
                        Some('\'') => value.push('\''),
                        Some('n') => value.push('\n'),
                        Some('t') => value.push('\t'),
                        Some('r') => value.push('\r'),
                        Some('0') => value.push('\0'),
                        Some(other) => {
                            return Err(IdlError::InvalidEscape {
                                escape: other,
                                span: Span::from_len(escape_pos, 1),
                            });
                        }
                        None => {
                            return Err(IdlError::UnterminatedString {
                                span: Span::new(start_pos, self.pos()),
                            });
                        }
                    }
                }
                Some(c) => value.push(c),
            }
        }
        let end_pos = self.pos();
        Ok(Token {
            kind: TokenKind::Str(value),
            span: Span::new(start_pos, end_pos),
        })
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn tokens_of(line: &str) -> Vec<TokenKind> {
        match classify_line(1, line).expect("lexes") {
            Line::Tokens(tokens, _) => tokens.into_iter().map(|t| t.kind).collect(),
            other => panic!("expected Tokens, got {other:?}"),
        }
    }

    #[test]
    fn blank_lines_are_recognised() {
        assert_eq!(classify_line(1, "").unwrap(), Line::Blank);
        assert_eq!(classify_line(1, "   \t  ").unwrap(), Line::Blank);
        assert_eq!(classify_line(1, "\r").unwrap(), Line::Blank);
    }

    #[test]
    fn separators_are_recognised_with_surrounding_whitespace() {
        assert_eq!(classify_line(1, "---").unwrap(), Line::Separator);
        assert_eq!(classify_line(1, "  ---  ").unwrap(), Line::Separator);
        assert_eq!(classify_line(1, "---\r").unwrap(), Line::Separator);
    }

    #[test]
    fn a_lone_dash_run_that_is_not_exactly_three_is_not_a_separator() {
        // `----` is not `---`, so it must not be misclassified as
        // `Separator` — whether it goes on to lex cleanly or not (it does
        // not: `-` alone starts no legal token) is a separate question.
        assert_ne!(classify_line(1, "----"), Ok(Line::Separator));
    }

    #[test]
    fn comment_only_lines_strip_the_hash_and_one_space() {
        assert_eq!(
            classify_line(1, "# hello world").unwrap(),
            Line::CommentOnly("hello world".to_owned())
        );
        assert_eq!(
            classify_line(1, "#no leading space").unwrap(),
            Line::CommentOnly("no leading space".to_owned())
        );
        assert_eq!(
            classify_line(1, "  # indented").unwrap(),
            Line::CommentOnly("indented".to_owned())
        );
        assert_eq!(
            classify_line(1, "#").unwrap(),
            Line::CommentOnly(String::new())
        );
    }

    #[test]
    fn identifiers_and_primitive_keywords_lex_uniformly() {
        assert_eq!(
            tokens_of("int32 x"),
            vec![
                TokenKind::Ident("int32".to_owned()),
                TokenKind::Ident("x".to_owned()),
            ]
        );
    }

    #[test]
    fn namespaced_type_reference_lexes_with_a_slash() {
        assert_eq!(
            tokens_of("geometry_msgs/Point origin"),
            vec![
                TokenKind::Ident("geometry_msgs".to_owned()),
                TokenKind::Slash,
                TokenKind::Ident("Point".to_owned()),
                TokenKind::Ident("origin".to_owned()),
            ]
        );
    }

    #[test]
    fn array_suffixes_lex_their_punctuation() {
        assert_eq!(
            tokens_of("int32[<=5] xs"),
            vec![
                TokenKind::Ident("int32".to_owned()),
                TokenKind::LBracket,
                TokenKind::LessEq,
                TokenKind::Number("5".to_owned()),
                TokenKind::RBracket,
                TokenKind::Ident("xs".to_owned()),
            ]
        );
    }

    #[test]
    fn constant_assignment_lexes_the_equals_sign() {
        assert_eq!(
            tokens_of("int32 X=123"),
            vec![
                TokenKind::Ident("int32".to_owned()),
                TokenKind::Ident("X".to_owned()),
                TokenKind::Eq,
                TokenKind::Number("123".to_owned()),
            ]
        );
    }

    #[test]
    fn negative_and_positive_numbers_lex_with_their_sign() {
        assert_eq!(
            tokens_of("int32 X=-5"),
            vec![
                TokenKind::Ident("int32".to_owned()),
                TokenKind::Ident("X".to_owned()),
                TokenKind::Eq,
                TokenKind::Number("-5".to_owned()),
            ]
        );
        assert_eq!(tokens_of("+5"), vec![TokenKind::Number("+5".to_owned())]);
    }

    #[test]
    fn floats_and_hex_and_exponents_lex_as_one_number_token() {
        assert_eq!(
            tokens_of("-3.5"),
            vec![TokenKind::Number("-3.5".to_owned())]
        );
        assert_eq!(
            tokens_of("0xFF"),
            vec![TokenKind::Number("0xFF".to_owned())]
        );
        assert_eq!(
            tokens_of("1.5e-10"),
            vec![TokenKind::Number("1.5e-10".to_owned())]
        );
    }

    #[test]
    fn array_literal_lexes_brackets_and_commas() {
        assert_eq!(
            tokens_of("[1, 2, 3]"),
            vec![
                TokenKind::LBracket,
                TokenKind::Number("1".to_owned()),
                TokenKind::Comma,
                TokenKind::Number("2".to_owned()),
                TokenKind::Comma,
                TokenKind::Number("3".to_owned()),
                TokenKind::RBracket,
            ]
        );
    }

    #[test]
    fn double_quoted_strings_unescape() {
        assert_eq!(
            tokens_of(r#""hello \"world\"\n""#),
            vec![TokenKind::Str("hello \"world\"\n".to_owned())]
        );
    }

    #[test]
    fn single_quoted_strings_are_accepted() {
        assert_eq!(
            tokens_of("'hello'"),
            vec![TokenKind::Str("hello".to_owned())]
        );
    }

    #[test]
    fn a_hash_inside_a_string_does_not_start_a_comment() {
        match classify_line(1, r#"string s "a#b""#).unwrap() {
            Line::Tokens(tokens, comment) => {
                assert_eq!(comment, None);
                assert_eq!(
                    tokens.last().map(|t| &t.kind),
                    Some(&TokenKind::Str("a#b".to_owned()))
                );
            }
            other => panic!("expected Tokens, got {other:?}"),
        }
    }

    #[test]
    fn trailing_comment_after_tokens_is_split_out() {
        match classify_line(1, "int32 x 5  # a default").unwrap() {
            Line::Tokens(tokens, comment) => {
                assert_eq!(tokens.len(), 3);
                assert_eq!(comment.as_deref(), Some("a default"));
            }
            other => panic!("expected Tokens, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_string_reports_its_start() {
        let err = classify_line(1, r#"string s "unterminated"#).unwrap_err();
        assert_eq!(
            err,
            IdlError::UnterminatedString {
                span: Span::new(Position::new(1, 10), Position::new(1, 23)),
            }
        );
    }

    #[test]
    fn invalid_escape_reports_the_offending_character_and_position() {
        let err = classify_line(1, r#""bad \q escape""#).unwrap_err();
        assert_eq!(
            err,
            IdlError::InvalidEscape {
                escape: 'q',
                span: Span::from_len(Position::new(1, 7), 1),
            }
        );
    }

    #[test]
    fn unexpected_character_reports_its_column() {
        let err = classify_line(1, "int32 x @ 5").unwrap_err();
        assert_eq!(
            err,
            IdlError::UnexpectedCharacter {
                found: '@',
                span: Span::from_len(Position::new(1, 9), 1),
            }
        );
    }

    #[test]
    fn a_lone_less_than_without_equals_is_rejected() {
        let err = classify_line(1, "int32[<5] xs").unwrap_err();
        assert_eq!(
            err,
            IdlError::UnexpectedCharacter {
                found: '<',
                span: Span::from_len(Position::new(1, 7), 1),
            }
        );
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        // `"é"` is a 4-byte string (two quotes plus a 2-byte UTF-8 char) but
        // three characters; a byte-counting scanner would place the `@'`
        // that follows at column 5, not column 4.
        let err = classify_line(1, "\"é\"@").unwrap_err();
        assert_eq!(
            err,
            IdlError::UnexpectedCharacter {
                found: '@',
                span: Span::from_len(Position::new(1, 4), 1),
            }
        );
    }
}
