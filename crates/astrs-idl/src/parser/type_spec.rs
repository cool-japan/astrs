//! Recursive-descent grammar for a field type: a scalar (IDL primitive,
//! `string`/`wstring` with an optional `<=N` bound, or a `pkg/Type`/`Type`
//! message reference) followed by at most one array/sequence suffix.
//!
//! ```text
//!   field_type   := scalar array_suffix?
//!   scalar       := primitive_kw | string_kw bound? | ident ('/' ident)?
//!   bound        := '<=' number
//!   array_suffix := '[' ']'                 -- unbounded
//!                  | '[' number ']'          -- fixed
//!                  | '[' '<=' number ']'      -- bounded
//! ```

use crate::ast::{ArraySuffix, FieldType, NamedTypeRef, PrimitiveKind, ScalarType, StringKind};
use crate::error::IdlError;
use crate::lexer::TokenKind;
use crate::parser::Cursor;
use crate::span::Span;

/// Parses a complete field type: a scalar type plus at most one array
/// suffix.
///
/// # Errors
///
/// Any grammar-group [`IdlError`] the scalar or array-suffix parse raises,
/// plus [`IdlError::NestedArrayNotAllowed`] if a second `[` follows the
/// first array suffix.
pub(crate) fn parse_field_type(cursor: &mut Cursor<'_>) -> Result<FieldType, IdlError> {
    let start = cursor
        .peek()
        .map_or_else(|| cursor.eol_span().start, |t| t.span.start);
    let scalar = parse_scalar_type(cursor)?;
    let array = parse_array_suffix(cursor)?;
    if let Some(second) = cursor.peek()
        && second.kind == TokenKind::LBracket
    {
        return Err(IdlError::NestedArrayNotAllowed { span: second.span });
    }
    let end = cursor.last_consumed_end().unwrap_or(start);
    Ok(FieldType {
        scalar,
        array,
        span: Span::new(start, end),
    })
}

fn parse_scalar_type(cursor: &mut Cursor<'_>) -> Result<ScalarType, IdlError> {
    let Some(tok) = cursor.bump() else {
        return Err(IdlError::UnexpectedEndOfLine {
            expected: "a type",
            span: cursor.eol_span(),
        });
    };
    let TokenKind::Ident(word) = tok.kind else {
        return Err(IdlError::UnexpectedToken {
            found: tok.kind.describe(),
            expected: "a type",
            span: tok.span,
        });
    };

    if let Some(primitive) = PrimitiveKind::from_keyword(&word) {
        reject_stray_bound(cursor)?;
        return Ok(ScalarType::Primitive(primitive));
    }
    if word == "string" || word == "wstring" {
        let kind = if word == "string" {
            StringKind::String
        } else {
            StringKind::WString
        };
        let bound = parse_optional_bound(cursor)?;
        return Ok(ScalarType::Str { kind, bound });
    }

    // Not a keyword: a namespaced (`pkg/Type`) or bare (`Type`) message
    // reference.
    reject_stray_bound(cursor)?;
    if matches!(cursor.peek().map(|t| &t.kind), Some(TokenKind::Slash)) {
        cursor.bump(); // '/'
        let Some(name_tok) = cursor.bump() else {
            return Err(IdlError::UnexpectedEndOfLine {
                expected: "a type name after '/'",
                span: cursor.eol_span(),
            });
        };
        let TokenKind::Ident(name) = name_tok.kind else {
            return Err(IdlError::UnexpectedToken {
                found: name_tok.kind.describe(),
                expected: "a type name",
                span: name_tok.span,
            });
        };
        return Ok(ScalarType::Named(NamedTypeRef {
            package: Some(word),
            name,
            span: Span::new(tok.span.start, name_tok.span.end),
        }));
    }
    Ok(ScalarType::Named(NamedTypeRef {
        package: None,
        name: word,
        span: tok.span,
    }))
}

/// After a primitive or named type, a `<=` is always a mistake — bounds only
/// apply to `string`/`wstring`. Producing the specific diagnostic here (
/// rather than letting it surface a few tokens later as "expected a name")
/// is the entire reason this check exists as its own function.
fn reject_stray_bound(cursor: &mut Cursor<'_>) -> Result<(), IdlError> {
    if let Some(tok) = cursor.peek()
        && tok.kind == TokenKind::LessEq
    {
        return Err(IdlError::BoundNotAllowedHere { span: tok.span });
    }
    Ok(())
}

fn parse_optional_bound(cursor: &mut Cursor<'_>) -> Result<Option<u32>, IdlError> {
    if !matches!(cursor.peek().map(|t| &t.kind), Some(TokenKind::LessEq)) {
        return Ok(None);
    }
    cursor.bump(); // '<='
    parse_bound_number(cursor).map(Some)
}

fn parse_bound_number(cursor: &mut Cursor<'_>) -> Result<u32, IdlError> {
    let Some(tok) = cursor.bump() else {
        return Err(IdlError::UnexpectedEndOfLine {
            expected: "a bound",
            span: cursor.eol_span(),
        });
    };
    let TokenKind::Number(text) = &tok.kind else {
        return Err(IdlError::UnexpectedToken {
            found: tok.kind.describe(),
            expected: "a bound",
            span: tok.span,
        });
    };
    text.parse::<u32>()
        .map_err(|_source| IdlError::InvalidNumber {
            text: text.clone(),
            span: tok.span,
        })
}

fn parse_array_suffix(cursor: &mut Cursor<'_>) -> Result<ArraySuffix, IdlError> {
    if !matches!(cursor.peek().map(|t| &t.kind), Some(TokenKind::LBracket)) {
        return Ok(ArraySuffix::None);
    }
    let open_span = cursor.peek().map_or_else(|| cursor.eol_span(), |t| t.span);
    cursor.bump(); // '['

    let suffix = match cursor.peek().map(|t| &t.kind) {
        Some(TokenKind::RBracket) => ArraySuffix::Unbounded,
        Some(TokenKind::LessEq) => {
            cursor.bump();
            ArraySuffix::Bounded(parse_bound_number(cursor)?)
        }
        Some(TokenKind::Number(_)) => {
            let size = parse_bound_number(cursor)?;
            if size == 0 {
                return Err(IdlError::ZeroSizedFixedArray { span: open_span });
            }
            ArraySuffix::Fixed(size)
        }
        _ => return Err(IdlError::InvalidArraySuffix { span: open_span }),
    };

    match cursor.bump() {
        Some(tok) if tok.kind == TokenKind::RBracket => Ok(suffix),
        Some(tok) => Err(IdlError::UnexpectedToken {
            found: tok.kind.describe(),
            expected: "']'",
            span: tok.span,
        }),
        None => Err(IdlError::UnexpectedEndOfLine {
            expected: "']'",
            span: cursor.eol_span(),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::lexer::classify_line;
    use crate::span::Position;

    fn parse(text: &str) -> Result<FieldType, IdlError> {
        let crate::lexer::Line::Tokens(tokens, _) = classify_line(1, text).expect("lexes") else {
            panic!("expected Tokens");
        };
        let line_end = tokens.last().map_or(Position::new(1, 1), |t| t.span.end);
        let mut cursor = Cursor::new(&tokens, line_end);
        parse_field_type(&mut cursor)
    }

    #[test]
    fn every_primitive_keyword_parses() {
        for kind in PrimitiveKind::ALL {
            let field_type = parse(kind.keyword()).unwrap();
            assert_eq!(field_type.scalar, ScalarType::Primitive(kind));
            assert_eq!(field_type.array, ArraySuffix::None);
        }
    }

    #[test]
    fn unbounded_string_and_wstring() {
        assert_eq!(
            parse("string").unwrap().scalar,
            ScalarType::Str {
                kind: StringKind::String,
                bound: None
            }
        );
        assert_eq!(
            parse("wstring").unwrap().scalar,
            ScalarType::Str {
                kind: StringKind::WString,
                bound: None
            }
        );
    }

    #[test]
    fn bounded_string_and_wstring() {
        assert_eq!(
            parse("string<=10").unwrap().scalar,
            ScalarType::Str {
                kind: StringKind::String,
                bound: Some(10)
            }
        );
        assert_eq!(
            parse("wstring<=4").unwrap().scalar,
            ScalarType::Str {
                kind: StringKind::WString,
                bound: Some(4)
            }
        );
    }

    #[test]
    fn namespaced_and_bare_message_references() {
        let field_type = parse("geometry_msgs/Point").unwrap();
        assert_eq!(
            field_type.scalar,
            ScalarType::Named(NamedTypeRef {
                package: Some("geometry_msgs".to_owned()),
                name: "Point".to_owned(),
                // "geometry_msgs" is 13 characters (columns 1-13), then '/'
                // at 14, then "Point" at columns 15-19.
                span: Span::new(Position::new(1, 1), Position::new(1, 20)),
            })
        );

        let bare = parse("Point").unwrap();
        assert_eq!(
            bare.scalar,
            ScalarType::Named(NamedTypeRef {
                package: None,
                name: "Point".to_owned(),
                span: Span::new(Position::new(1, 1), Position::new(1, 6)),
            })
        );
    }

    #[test]
    fn unbounded_fixed_and_bounded_arrays() {
        assert_eq!(parse("int32[]").unwrap().array, ArraySuffix::Unbounded);
        assert_eq!(parse("int32[3]").unwrap().array, ArraySuffix::Fixed(3));
        assert_eq!(parse("int32[<=8]").unwrap().array, ArraySuffix::Bounded(8));
    }

    #[test]
    fn array_of_bounded_strings_combines_both_bounds() {
        let field_type = parse("string<=10[<=5]").unwrap();
        assert_eq!(
            field_type.scalar,
            ScalarType::Str {
                kind: StringKind::String,
                bound: Some(10)
            }
        );
        assert_eq!(field_type.array, ArraySuffix::Bounded(5));
    }

    #[test]
    fn nested_array_suffix_is_rejected() {
        let err = parse("int32[3][4]").unwrap_err();
        assert!(matches!(err, IdlError::NestedArrayNotAllowed { .. }));
    }

    #[test]
    fn a_bound_after_a_primitive_is_rejected_specifically() {
        let err = parse("int32<=5").unwrap_err();
        assert!(matches!(err, IdlError::BoundNotAllowedHere { .. }));
    }

    #[test]
    fn a_bound_after_a_named_type_is_rejected_specifically() {
        let err = parse("Point<=5").unwrap_err();
        assert!(matches!(err, IdlError::BoundNotAllowedHere { .. }));
    }

    #[test]
    fn a_malformed_array_suffix_is_rejected() {
        let err = parse("int32[abc]").unwrap_err();
        assert!(matches!(err, IdlError::InvalidArraySuffix { .. }));
    }

    #[test]
    fn an_unterminated_array_suffix_is_rejected() {
        let err = parse("int32[3").unwrap_err();
        assert!(matches!(err, IdlError::UnexpectedEndOfLine { .. }));
    }

    #[test]
    fn a_zero_sized_fixed_array_is_rejected() {
        let err = parse("int32[0]").unwrap_err();
        assert!(matches!(err, IdlError::ZeroSizedFixedArray { .. }));
    }

    #[test]
    fn a_zero_bound_on_a_bounded_array_or_bounded_string_is_left_alone() {
        // Always-empty, but representable — `BoundedSequence<T, 0>` and
        // `BoundedString<0>` are legal types, unlike `FixedSizeList(_, 0)`.
        assert_eq!(parse("int32[<=0]").unwrap().array, ArraySuffix::Bounded(0));
        assert_eq!(
            parse("string<=0").unwrap().scalar,
            ScalarType::Str {
                kind: StringKind::String,
                bound: Some(0)
            }
        );
    }

    #[test]
    fn an_out_of_range_bound_is_rejected() {
        // u32::MAX + 1, does not fit u32.
        let err = parse("int32[4294967296]").unwrap_err();
        assert!(matches!(err, IdlError::InvalidNumber { .. }));
    }
}
