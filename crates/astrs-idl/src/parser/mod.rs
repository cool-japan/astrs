//! The recursive-descent parser: `.msg`/`.srv`/`.action` source text to an
//! [`ast`](crate::ast) tree.
//!
//! [`parse_message`], [`parse_service`] and [`parse_action`] are the three
//! entry points (blueprint §10.3's three file kinds). All three share one
//! per-line driver: [`crate::lexer::classify_line`] turns each line into a
//! [`crate::lexer::Line`], a private `split_sections` groups the lines by
//! `---` boundary and checks the count matches the file kind, and a private
//! `build_section` turns one group into a [`MessageSection`] — accumulating
//! comment blocks, dispatching each declaration line to a private
//! `parse_item`, and rejecting a repeated field/constant name within that
//! section. `parse_item` itself hands off to the private `type_spec` and
//! `literal` submodules for the type and an optional default or a
//! constant's mandatory value.
//!
//! Dispatch between a field and a constant declaration is the identifier's
//! own case — ROS 2 field names are `lower_snake_case` and constant names
//! are `UPPER_SNAKE_CASE`, so the two grammars never overlap and no `=`
//! lookahead is needed to tell them apart.

mod literal;
mod type_spec;

use std::collections::HashMap;

use crate::ast::{
    ActionFile, ConstantDecl, FieldDecl, Item, MessageFile, MessageSection, ScalarType, ServiceFile,
};
use crate::error::IdlError;
use crate::lexer::{Line, Token, TokenKind, classify_line};
use crate::naming;
use crate::span::{Position, Span};

/// Parses a `.msg` file's contents.
///
/// # Errors
///
/// Any [`IdlError`], plus [`IdlError::WrongSectionCount`] if the text
/// contains a `---` (a `.msg` file has none).
pub fn parse_message(source: &str) -> Result<MessageFile, IdlError> {
    let (mut sections, separators) = split_sections(source)?;
    if !separators.is_empty() {
        return Err(IdlError::WrongSectionCount {
            file_kind: "a .msg file",
            expected: 0,
            found: separators.len(),
            span: Span::empty(separators[0]),
        });
    }
    Ok(MessageFile {
        section: sections.remove(0),
    })
}

/// Parses a `.srv` file's contents: a request section, one `---`, a
/// response section.
///
/// # Errors
///
/// Any [`IdlError`], plus [`IdlError::WrongSectionCount`] if the text does
/// not contain exactly one `---`.
pub fn parse_service(source: &str) -> Result<ServiceFile, IdlError> {
    let (mut sections, separators) = split_sections(source)?;
    if separators.len() != 1 {
        return Err(wrong_section_count("a .srv file", 1, &separators, source));
    }
    let response = sections.remove(1);
    let request = sections.remove(0);
    Ok(ServiceFile { request, response })
}

/// Parses an `.action` file's contents: goal, `---`, result, `---`,
/// feedback.
///
/// # Errors
///
/// Any [`IdlError`], plus [`IdlError::WrongSectionCount`] if the text does
/// not contain exactly two `---`.
pub fn parse_action(source: &str) -> Result<ActionFile, IdlError> {
    let (mut sections, separators) = split_sections(source)?;
    if separators.len() != 2 {
        return Err(wrong_section_count(
            "an .action file",
            2,
            &separators,
            source,
        ));
    }
    let feedback = sections.remove(2);
    let result = sections.remove(1);
    let goal = sections.remove(0);
    Ok(ActionFile {
        goal,
        result,
        feedback,
    })
}

fn wrong_section_count(
    file_kind: &'static str,
    expected: usize,
    separators: &[Position],
    source: &str,
) -> IdlError {
    let span = if separators.len() > expected {
        Span::empty(separators[expected])
    } else {
        let last_line = u32::try_from(source.lines().count().max(1)).unwrap_or(u32::MAX);
        Span::empty(Position::new(last_line, 1))
    };
    IdlError::WrongSectionCount {
        file_kind,
        expected,
        found: separators.len(),
        span,
    }
}

/// Classifies every line of `source` and groups it into sections split on
/// `---`, returning the sections and the position of every separator found
/// (for a caller that rejects the wrong count to point at).
fn split_sections(source: &str) -> Result<(Vec<MessageSection>, Vec<Position>), IdlError> {
    let mut chunks: Vec<Vec<(u32, Line)>> = vec![Vec::new()];
    let mut separators = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let line = classify_line(line_no, raw)?;
        if matches!(line, Line::Separator) {
            separators.push(Position::new(line_no, 1));
            chunks.push(Vec::new());
        } else if let Some(current) = chunks.last_mut() {
            current.push((line_no, line));
        }
    }
    let sections = chunks
        .into_iter()
        .map(|chunk| build_section(&chunk))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((sections, separators))
}

/// Builds one [`MessageSection`] from the lines between two `---`s (or the
/// whole file, for a `.msg`).
fn build_section(chunk: &[(u32, Line)]) -> Result<MessageSection, IdlError> {
    let mut section = MessageSection::default();
    let mut pending_comment: Vec<String> = Vec::new();
    let mut seen: HashMap<String, Span> = HashMap::new();
    let mut leading_captured = false;

    for (line_no, line) in chunk {
        match line {
            Line::Blank => {
                if leading_captured || pending_comment.is_empty() {
                    // Either the section-level (message) comment has
                    // already been captured — this blank is an ordinary
                    // separator between fields, so it resets whatever
                    // comment was accumulating for the *next* item — or
                    // there is nothing accumulated to lose either way.
                    pending_comment.clear();
                } else {
                    // No field/constant has started yet, so this is still
                    // the leading comment block: the message-level
                    // overview. A blank line closes that block rather than
                    // discarding it — ROS 2 `.msg` files conventionally
                    // separate the overview from the first field's own
                    // comment with exactly this blank line (see
                    // `builtin_interfaces/msg/Time.msg`'s and `Duration
                    // .msg`'s own header). Discarding it here was the bug:
                    // the first field's comment block (captured below, at
                    // the first `Line::Tokens`) would silently take its
                    // place as the section's doc.
                    section.leading_comment = std::mem::take(&mut pending_comment);
                    leading_captured = true;
                }
            }
            Line::CommentOnly(text) => pending_comment.push(text.clone()),
            Line::Separator => {} // stripped by `split_sections`; never occurs here
            Line::Tokens(tokens, trailing_comment) => {
                if !leading_captured {
                    section.leading_comment.clone_from(&pending_comment);
                    leading_captured = true;
                }
                let item = parse_item(
                    *line_no,
                    tokens,
                    trailing_comment.as_deref(),
                    &pending_comment,
                )?;
                pending_comment.clear();
                let name = item.name().to_owned();
                let span = item.span();
                if let Some(&first) = seen.get(&name) {
                    return Err(IdlError::DuplicateMember {
                        name,
                        first,
                        second: span,
                    });
                }
                seen.insert(name, span);
                section.items.push(item);
            }
        }
    }
    if !leading_captured {
        section.leading_comment = pending_comment;
    }
    Ok(section)
}

/// Parses one declaration line into a [`Item`], dispatching on the
/// identifier's case: `UPPER_SNAKE_CASE` is a constant, `lower_snake_case`
/// (or anything else, so the specific naming error surfaces) is a field.
fn parse_item(
    line_no: u32,
    tokens: &[Token],
    trailing_comment: Option<&str>,
    pending_comment: &[String],
) -> Result<Item, IdlError> {
    let line_end = tokens
        .last()
        .map_or(Position::new(line_no, 1), |t| t.span.end);
    let mut cursor = Cursor::new(tokens, line_end);
    let start = cursor.peek().map_or(line_end, |t| t.span.start);

    let field_type = type_spec::parse_field_type(&mut cursor)?;

    let Some(name_tok) = cursor.bump() else {
        return Err(IdlError::UnexpectedEndOfLine {
            expected: "a field or constant name",
            span: cursor.eol_span(),
        });
    };
    let TokenKind::Ident(name) = name_tok.kind else {
        return Err(IdlError::UnexpectedToken {
            found: name_tok.kind.describe(),
            expected: "a field or constant name",
            span: name_tok.span,
        });
    };
    let name_span = name_tok.span;
    let comment = join_comment(pending_comment, trailing_comment);
    let is_constant_shaped = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());

    if is_constant_shaped {
        parse_constant_tail(&mut cursor, field_type, name, name_span, comment, start)
    } else {
        parse_field_tail(&mut cursor, field_type, name, name_span, comment, start)
    }
}

fn parse_constant_tail(
    cursor: &mut Cursor<'_>,
    type_: crate::ast::FieldType,
    name: String,
    name_span: Span,
    comment: Option<String>,
    start: Position,
) -> Result<Item, IdlError> {
    naming::validate_constant_name(&name, name_span)?;
    if !type_.array.is_scalar() {
        return Err(IdlError::ConstantMustBeScalar { span: type_.span });
    }
    if matches!(type_.scalar, ScalarType::Named(_)) {
        return Err(IdlError::ConstantMustBePrimitive { span: type_.span });
    }
    match cursor.bump() {
        Some(tok) if tok.kind == TokenKind::Eq => {}
        Some(tok) => {
            return Err(IdlError::UnexpectedToken {
                found: tok.kind.describe(),
                expected: "'='",
                span: tok.span,
            });
        }
        None => return Err(IdlError::MissingEquals { span: name_span }),
    }
    let value_start = cursor
        .peek()
        .map_or_else(|| cursor.eol_span().start, |t| t.span.start);
    let value = literal::parse_literal(cursor)?;
    let value_end = cursor.last_consumed_end().unwrap_or(value_start);
    let value_span = Span::new(value_start, value_end);
    literal::validate_value(&type_, &value, value_span)?;
    require_end_of_line(cursor)?;
    Ok(Item::Constant(ConstantDecl {
        type_,
        name,
        name_span,
        value,
        value_span,
        comment,
        span: Span::new(start, value_end),
    }))
}

fn parse_field_tail(
    cursor: &mut Cursor<'_>,
    type_: crate::ast::FieldType,
    name: String,
    name_span: Span,
    comment: Option<String>,
    start: Position,
) -> Result<Item, IdlError> {
    naming::validate_field_name(&name, name_span)?;
    let (default, default_span) = if cursor.is_at_end() {
        (None, None)
    } else if matches!(type_.scalar, ScalarType::Named(_)) {
        let span = cursor.remaining_span().unwrap_or_else(|| cursor.eol_span());
        return Err(IdlError::InvalidLiteral {
            expected: "no default (message-typed fields may not declare one)",
            span,
        });
    } else {
        let value_start = cursor
            .peek()
            .map_or_else(|| cursor.eol_span().start, |t| t.span.start);
        let value = literal::parse_literal(cursor)?;
        let value_end = cursor.last_consumed_end().unwrap_or(value_start);
        let value_span = Span::new(value_start, value_end);
        literal::validate_value(&type_, &value, value_span)?;
        (Some(value), Some(value_span))
    };
    require_end_of_line(cursor)?;
    let end = default_span.map_or(name_span.end, |s| s.end);
    Ok(Item::Field(FieldDecl {
        type_,
        name,
        name_span,
        default,
        default_span,
        comment,
        span: Span::new(start, end),
    }))
}

fn require_end_of_line(cursor: &Cursor<'_>) -> Result<(), IdlError> {
    match cursor.remaining_span() {
        Some(span) => Err(IdlError::TrailingTokens { span }),
        None => Ok(()),
    }
}

fn join_comment(pending: &[String], trailing: Option<&str>) -> Option<String> {
    let mut parts: Vec<&str> = pending.iter().map(String::as_str).collect();
    if let Some(text) = trailing {
        parts.push(text);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// A cursor over one line's tokens, shared by [`type_spec`] and [`literal`].
///
/// Cloning a [`Token`] on [`Cursor::bump`] (rather than returning a
/// reference) trades a small, one-off allocation-bearing clone for freedom
/// from borrow-checker fights across the `match`-heavy grammar functions —
/// lines are short, this runs at parse time only, and it is not a path any
/// budget in blueprint §20.4 tracks.
pub(crate) struct Cursor<'t> {
    tokens: &'t [Token],
    pos: usize,
    /// End of the line, for an end-of-line error when there are no tokens
    /// at all to anchor a span to.
    line_end: Position,
    /// End position of the last token [`Cursor::bump`] returned, if any.
    last_end: Option<Position>,
}

impl<'t> Cursor<'t> {
    pub(crate) fn new(tokens: &'t [Token], line_end: Position) -> Self {
        Self {
            tokens,
            pos: 0,
            line_end,
            last_end: None,
        }
    }

    pub(crate) fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    pub(crate) fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if let Some(token) = &token {
            self.pos += 1;
            self.last_end = Some(token.span.end);
        }
        token
    }

    pub(crate) fn is_at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// The span of the next unconsumed token, or `None` at end of line.
    pub(crate) fn remaining_span(&self) -> Option<Span> {
        self.peek().map(|t| t.span)
    }

    /// A zero-width span just past the last consumed token, or the line's
    /// own end if nothing has been consumed yet — for "the line ended when
    /// the grammar wanted more" diagnostics.
    pub(crate) fn eol_span(&self) -> Span {
        Span::empty(self.last_end.unwrap_or(self.line_end))
    }

    /// End position of the last token [`Cursor::bump`] returned.
    pub(crate) fn last_consumed_end(&self) -> Option<Position> {
        self.last_end
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ast::{ArraySuffix, Literal, StringKind};

    #[test]
    fn a_minimal_message_parses() {
        let file = parse_message("int32 x\nint32 y\n").unwrap();
        assert_eq!(file.section.items.len(), 2);
        assert_eq!(file.section.fields().count(), 2);
    }

    #[test]
    fn constants_and_fields_coexist_and_defaults_parse() {
        let source =
            "int32 STATUS_OK=0\nint32 STATUS_ERROR=1\nint32 status 0\nstring name \"unnamed\"\n";
        let file = parse_message(source).unwrap();
        assert_eq!(file.section.constants().count(), 2);
        assert_eq!(file.section.fields().count(), 2);
        let status = file.section.fields().find(|f| f.name == "status").unwrap();
        assert_eq!(status.default, Some(Literal::Int(0)));
    }

    #[test]
    fn comments_attach_to_the_field_and_the_section() {
        let source = "# A point in space.\nfloat64 x  # the x coordinate\nfloat64 y\n";
        let file = parse_message(source).unwrap();
        assert_eq!(
            file.section.leading_comment,
            vec!["A point in space.".to_owned()]
        );
        let x = file.section.fields().next().unwrap();
        assert_eq!(
            x.comment.as_deref(),
            Some("A point in space.\nthe x coordinate")
        );
        let y = file.section.fields().nth(1).unwrap();
        assert_eq!(y.comment, None);
    }

    #[test]
    fn a_blank_line_resets_the_pending_comment_block() {
        let source = "# stale, not attached to y\n\nfloat64 y\n";
        let file = parse_message(source).unwrap();
        let y = file.section.fields().next().unwrap();
        assert_eq!(y.comment, None);
        // The blank line closes the leading comment block instead of
        // discarding it (no field/constant had started yet) — it becomes
        // the section's own doc rather than y's.
        assert_eq!(
            file.section.leading_comment,
            vec!["stale, not attached to y".to_owned()]
        );
    }

    #[test]
    fn a_blank_line_separated_overview_becomes_the_section_doc_not_the_first_field_comment() {
        // Regression for the association bug reproduced on
        // `builtin_interfaces/msg/Time.msg`: a blank line between the
        // message-level overview and the first field's *own* comment block
        // used to discard the overview, so the first field's comment
        // silently took its place as the generated type's rustdoc.
        let source = "\
# This represents an instant in time.
#
# This time is expressed at two levels of granularity.

# The seconds component, valid over all int32 values.
int32 sec

# The nanoseconds component, valid in the range [0, 1e9).
uint32 nanosec
";
        let file = parse_message(source).unwrap();
        assert_eq!(
            file.section.leading_comment,
            vec![
                "This represents an instant in time.".to_owned(),
                String::new(),
                "This time is expressed at two levels of granularity.".to_owned(),
            ]
        );
        let mut fields = file.section.fields();
        let sec = fields.next().unwrap();
        assert_eq!(
            sec.comment.as_deref(),
            Some("The seconds component, valid over all int32 values.")
        );
        let nanosec = fields.next().unwrap();
        assert_eq!(
            nanosec.comment.as_deref(),
            Some("The nanoseconds component, valid in the range [0, 1e9).")
        );
    }

    #[test]
    fn duration_msg_shape_keeps_its_multiline_overview_separate_from_sec_comment() {
        // Same bug, `builtin_interfaces/msg/Duration.msg`'s shape: a
        // two-line overview (itself spanning a URL) followed by a blank,
        // then the first field's own single-line comment.
        let source = "\
# Duration defines a period between two time points.
# Messages of this datatype are of ROS Time following this design:
# https://design.ros2.org/articles/clock_and_time.html

# Seconds component, range is valid over any possible int32 value.
int32 sec

# Nanoseconds component in the range of [0, 1e9).
uint32 nanosec
";
        let file = parse_message(source).unwrap();
        assert_eq!(
            file.section.leading_comment,
            vec![
                "Duration defines a period between two time points.".to_owned(),
                "Messages of this datatype are of ROS Time following this design:".to_owned(),
                "https://design.ros2.org/articles/clock_and_time.html".to_owned(),
            ]
        );
        let sec = file.section.fields().next().unwrap();
        assert_eq!(
            sec.comment.as_deref(),
            Some("Seconds component, range is valid over any possible int32 value.")
        );
    }

    #[test]
    fn a_leading_overview_with_no_field_comment_at_all_still_becomes_the_section_doc() {
        // The overview-closing blank line fires even when nothing follows
        // it but the field itself (no second comment block) — the section
        // doc must not silently end up empty in that shape either.
        let source = "# An overview with nothing else.\n\nint32 x\n";
        let file = parse_message(source).unwrap();
        assert_eq!(
            file.section.leading_comment,
            vec!["An overview with nothing else.".to_owned()]
        );
        let x = file.section.fields().next().unwrap();
        assert_eq!(x.comment, None);
    }

    #[test]
    fn a_blank_line_between_two_interior_fields_still_only_resets_pending_comment() {
        // Once the section doc has been captured (here: at the very first
        // field, which has no comment of its own), later blank lines revert
        // to the plain "reset pending per-item comment" rule — a stray
        // comment block separated from the next field by a blank must not
        // leak into that field's comment, and must not overwrite the
        // already-captured section doc either.
        let source = "int32 x\n\n# stray, not attached to y\n\nint32 y\n";
        let file = parse_message(source).unwrap();
        assert!(file.section.leading_comment.is_empty());
        let y = file.section.fields().nth(1).unwrap();
        assert_eq!(y.comment, None);
    }

    #[test]
    fn duplicate_field_names_are_rejected_with_both_positions() {
        let err = parse_message("int32 x\nint32 x\n").unwrap_err();
        match err {
            IdlError::DuplicateMember {
                name,
                first,
                second,
            } => {
                assert_eq!(name, "x");
                assert_eq!(first.start.line, 1);
                assert_eq!(second.start.line, 2);
            }
            other => panic!("expected DuplicateMember, got {other:?}"),
        }
    }

    #[test]
    fn a_constant_and_a_field_whose_names_differ_only_in_case_both_parse() {
        // "X" (constant) and "x" (field) are different identifiers — ROS 2's
        // disjoint casing rules mean a constant and a field can never
        // collide under `DuplicateMember` in the first place.
        let file = parse_message("int32 X=1\nint32 x\n").unwrap();
        assert_eq!(file.section.constants().count(), 1);
        assert_eq!(file.section.fields().count(), 1);
    }

    #[test]
    fn service_file_splits_request_and_response() {
        let file = parse_service("int64 a\nint64 b\n---\nint64 sum\n").unwrap();
        assert_eq!(file.request.fields().count(), 2);
        assert_eq!(file.response.fields().count(), 1);
    }

    #[test]
    fn a_service_file_without_a_separator_is_rejected() {
        let err = parse_service("int64 a\n").unwrap_err();
        assert_eq!(
            err,
            IdlError::WrongSectionCount {
                file_kind: "a .srv file",
                expected: 1,
                found: 0,
                span: Span::empty(Position::new(1, 1)),
            }
        );
    }

    #[test]
    fn a_service_file_with_two_separators_is_rejected_at_the_second() {
        let err = parse_service("int64 a\n---\nint64 b\n---\nint64 c\n").unwrap_err();
        assert_eq!(
            err,
            IdlError::WrongSectionCount {
                file_kind: "a .srv file",
                expected: 1,
                found: 2,
                span: Span::empty(Position::new(4, 1)),
            }
        );
    }

    #[test]
    fn action_file_splits_goal_result_and_feedback() {
        let file =
            parse_action("int32 order\n---\nint32[] sequence\n---\nint32[] partial_sequence\n")
                .unwrap();
        assert_eq!(file.goal.fields().count(), 1);
        assert_eq!(file.result.fields().count(), 1);
        assert_eq!(file.feedback.fields().count(), 1);
    }

    #[test]
    fn an_action_file_with_the_wrong_separator_count_is_rejected() {
        let err = parse_action("int32 order\n---\nint32[] sequence\n").unwrap_err();
        assert_eq!(
            err,
            IdlError::WrongSectionCount {
                file_kind: "an .action file",
                expected: 2,
                found: 1,
                span: Span::empty(Position::new(3, 1)),
            }
        );
    }

    #[test]
    fn a_msg_file_containing_a_separator_is_rejected() {
        let err = parse_message("int32 x\n---\nint32 y\n").unwrap_err();
        assert_eq!(
            err,
            IdlError::WrongSectionCount {
                file_kind: "a .msg file",
                expected: 0,
                found: 1,
                span: Span::empty(Position::new(2, 1)),
            }
        );
    }

    #[test]
    fn an_empty_message_parses_with_no_items() {
        let file = parse_message("").unwrap();
        assert!(file.section.items.is_empty());
    }

    #[test]
    fn nested_and_array_typed_fields_parse() {
        let source = "geometry_msgs/Point origin\ngeometry_msgs/Point[] points\nfloat64[3] xyz\nfloat64[<=8] samples\n";
        let file = parse_message(source).unwrap();
        let fields: Vec<_> = file.section.fields().collect();
        assert_eq!(fields.len(), 4);
        assert!(matches!(fields[0].type_.scalar, ScalarType::Named(_)));
        assert_eq!(fields[1].type_.array, ArraySuffix::Unbounded);
        assert_eq!(fields[2].type_.array, ArraySuffix::Fixed(3));
        assert_eq!(fields[3].type_.array, ArraySuffix::Bounded(8));
    }

    #[test]
    fn a_bare_type_reference_means_the_same_package() {
        let file = parse_message("Header header\n").unwrap();
        let header = file.section.fields().next().unwrap();
        match &header.type_.scalar {
            ScalarType::Named(named) => assert_eq!(named.package, None),
            other => panic!("expected a named type, got {other:?}"),
        }
    }

    #[test]
    fn wstring_fields_parse_with_and_without_a_bound() {
        let file = parse_message("wstring name\nwstring<=8 short_name\n").unwrap();
        let fields: Vec<_> = file.section.fields().collect();
        assert_eq!(
            fields[0].type_.scalar,
            ScalarType::Str {
                kind: StringKind::WString,
                bound: None
            }
        );
        assert_eq!(
            fields[1].type_.scalar,
            ScalarType::Str {
                kind: StringKind::WString,
                bound: Some(8)
            }
        );
    }

    #[test]
    fn using_a_type_keyword_as_a_field_name_is_rejected() {
        let err = parse_message("int32 int32\n").unwrap_err();
        assert!(matches!(err, IdlError::ReservedIdentifier { .. }));
    }

    #[test]
    fn a_message_typed_field_may_not_declare_a_default() {
        let err = parse_message("geometry_msgs/Point origin 0\n").unwrap_err();
        assert!(matches!(err, IdlError::InvalidLiteral { .. }));
    }

    #[test]
    fn a_constant_missing_its_equals_sign_is_rejected() {
        let err = parse_message("int32 FOO\n").unwrap_err();
        assert_eq!(
            err,
            IdlError::MissingEquals {
                span: Span::new(Position::new(1, 7), Position::new(1, 10)),
            }
        );
    }

    #[test]
    fn an_array_typed_constant_is_rejected() {
        let err = parse_message("int32[] FOO=1\n").unwrap_err();
        assert!(matches!(err, IdlError::ConstantMustBeScalar { .. }));
    }

    #[test]
    fn a_message_typed_constant_is_rejected() {
        let err = parse_message("Point FOO=1\n").unwrap_err();
        assert!(matches!(err, IdlError::ConstantMustBePrimitive { .. }));
    }

    #[test]
    fn trailing_tokens_after_a_complete_field_are_rejected() {
        let err = parse_message("int32 x 5 6\n").unwrap_err();
        assert!(matches!(err, IdlError::TrailingTokens { .. }));
    }

    #[test]
    fn constants_referencing_the_primitive_range_are_checked() {
        let err = parse_message("uint8 X=256\n").unwrap_err();
        assert_eq!(
            err,
            IdlError::NumberOutOfRange {
                text: "256".to_owned(),
                type_name: "uint8",
                span: Span::new(Position::new(1, 9), Position::new(1, 12)),
            }
        );
    }

    #[test]
    fn a_fixed_size_array_default_of_the_wrong_length_is_rejected() {
        let err = parse_message("float64[3] xyz [1.0, 2.0]\n").unwrap_err();
        assert!(matches!(err, IdlError::ArrayLiteralLengthMismatch { .. }));
    }

    #[test]
    fn service_and_action_sections_track_duplicate_names_independently() {
        // The same field name in the request and the response is fine —
        // they are independent namespaces.
        let file = parse_service("int64 value\n---\nint64 value\n").unwrap();
        assert_eq!(file.request.fields().count(), 1);
        assert_eq!(file.response.fields().count(), 1);
    }
}
