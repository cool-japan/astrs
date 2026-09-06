//! Recursive-descent grammar for a default or constant value, and its
//! validation against a declared [`FieldType`].
//!
//! ```text
//!   literal        := scalar_literal | array_literal
//!   array_literal  := '[' ']' | '[' scalar_literal (',' scalar_literal)* ']'
//!   scalar_literal := 'true' | 'false' | string | number
//! ```
//!
//! Array elements are always [`parse_scalar_literal`], never
//! [`parse_literal`] itself — that is what makes `[[1, 2]]` a syntax error
//! (an unexpected `[` where a value was wanted) rather than something a
//! later semantic pass has to notice, since ROS 2 IDL has no array-of-array
//! type for such a default to describe in the first place.
//!
//! This is deliberately **not** rosidl's own default-value grammar, which
//! parses the text after the field name as YAML. Implementing a YAML flow-
//! scalar grammar by hand for this one call site was judged not worth the
//! weight; this module's literal grammar is a documented, tested subset
//! (booleans, decimal/hex integers, floats with an exponent, single- or
//! double-quoted strings, flat arrays of any of those) that covers every
//! `.msg`/`.srv`/`.action` default this crate's own `common_interfaces`
//! generation set uses. See the crate-level report for the deviation.

use crate::ast::{ArraySuffix, FieldType, Literal, PrimitiveKind, ScalarType, StringKind};
use crate::error::IdlError;
use crate::lexer::{Token, TokenKind};
use crate::parser::Cursor;
use crate::span::Span;

/// Parses one value: a scalar, or a `[...]` array of scalars.
///
/// # Errors
///
/// The literal group of [`IdlError`].
pub(crate) fn parse_literal(cursor: &mut Cursor<'_>) -> Result<Literal, IdlError> {
    if matches!(cursor.peek().map(|t| &t.kind), Some(TokenKind::LBracket)) {
        parse_array_literal(cursor)
    } else {
        parse_scalar_literal(cursor)
    }
}

fn parse_scalar_literal(cursor: &mut Cursor<'_>) -> Result<Literal, IdlError> {
    let Some(tok) = cursor.bump() else {
        return Err(IdlError::UnexpectedEndOfLine {
            expected: "a value",
            span: cursor.eol_span(),
        });
    };
    match tok.kind {
        TokenKind::Str(text) => Ok(Literal::Str(text)),
        TokenKind::Ident(word) if word == "true" => Ok(Literal::Bool(true)),
        TokenKind::Ident(word) if word == "false" => Ok(Literal::Bool(false)),
        TokenKind::Number(text) => parse_number_literal(&text, tok.span),
        other => Err(IdlError::UnexpectedToken {
            found: other.describe(),
            expected: "a value",
            span: tok.span,
        }),
    }
}

fn parse_array_literal(cursor: &mut Cursor<'_>) -> Result<Literal, IdlError> {
    cursor.bump(); // '['
    if matches!(cursor.peek().map(|t| &t.kind), Some(TokenKind::RBracket)) {
        cursor.bump();
        return Ok(Literal::Array(Vec::new()));
    }
    let mut elements = Vec::new();
    loop {
        elements.push(parse_scalar_literal(cursor)?);
        match cursor.bump() {
            Some(Token {
                kind: TokenKind::Comma,
                ..
            }) => {}
            Some(Token {
                kind: TokenKind::RBracket,
                ..
            }) => break,
            Some(other) => {
                return Err(IdlError::UnexpectedToken {
                    found: other.kind.describe(),
                    expected: "',' or ']'",
                    span: other.span,
                });
            }
            None => {
                return Err(IdlError::UnexpectedEndOfLine {
                    expected: "',' or ']'",
                    span: cursor.eol_span(),
                });
            }
        }
    }
    Ok(Literal::Array(elements))
}

/// Strict number parsing: hex (`0x...`), float (contains `.`, `e` or `E`),
/// or plain decimal — in that priority order. `_` digit separators are
/// stripped first, matching Rust's own integer-literal convention.
fn parse_number_literal(text: &str, span: Span) -> Result<Literal, IdlError> {
    let cleaned: String = text.chars().filter(|&c| c != '_').collect();
    let invalid = || IdlError::InvalidNumber {
        text: text.to_owned(),
        span,
    };

    if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        return i128::from_str_radix(hex, 16)
            .map(Literal::Int)
            .map_err(|_source| invalid());
    }
    if cleaned.contains(['.', 'e', 'E']) {
        return cleaned
            .parse::<f64>()
            .map(Literal::Float)
            .map_err(|_source| invalid());
    }
    cleaned
        .parse::<i128>()
        .map(Literal::Int)
        .map_err(|_source| invalid())
}

/// Checks a parsed value against the field type it was declared for:
/// literal shape (scalar vs. array), element count against a fixed/bounded
/// array, integer range against the primitive's width, and string/wstring
/// length against a `<=N` bound (in UTF-16 code units for `wstring`,
/// matching `astrs_cdr::BoundedWString`'s own bound accounting).
///
/// # Errors
///
/// The literal group of [`IdlError`].
pub(crate) fn validate_value(
    field_type: &FieldType,
    literal: &Literal,
    span: Span,
) -> Result<(), IdlError> {
    match field_type.array {
        ArraySuffix::None => validate_scalar(&field_type.scalar, literal, span),
        ArraySuffix::Unbounded | ArraySuffix::Fixed(_) | ArraySuffix::Bounded(_) => {
            let Literal::Array(elements) = literal else {
                return Err(IdlError::InvalidLiteral {
                    expected: "an array",
                    span,
                });
            };
            if let ArraySuffix::Fixed(expected) = field_type.array
                && elements.len() != expected as usize
            {
                return Err(IdlError::ArrayLiteralLengthMismatch {
                    expected,
                    actual: elements.len(),
                    span,
                });
            }
            if let ArraySuffix::Bounded(bound) = field_type.array
                && elements.len() > bound as usize
            {
                return Err(IdlError::DefaultExceedsBound {
                    bound,
                    actual: elements.len(),
                    span,
                });
            }
            for element in elements {
                validate_scalar(&field_type.scalar, element, span)?;
            }
            Ok(())
        }
    }
}

fn validate_scalar(scalar: &ScalarType, literal: &Literal, span: Span) -> Result<(), IdlError> {
    match scalar {
        ScalarType::Named(_) => Err(IdlError::InvalidLiteral {
            expected: "no default (message-typed fields may not declare one)",
            span,
        }),
        ScalarType::Primitive(PrimitiveKind::Bool) => {
            if matches!(literal, Literal::Bool(_)) {
                Ok(())
            } else {
                Err(IdlError::InvalidLiteral {
                    expected: "bool",
                    span,
                })
            }
        }
        ScalarType::Primitive(kind @ (PrimitiveKind::Float32 | PrimitiveKind::Float64)) => {
            let value = match literal {
                Literal::Float(f) => *f,
                Literal::Int(i) => *i as f64,
                _ => {
                    return Err(IdlError::InvalidLiteral {
                        expected: kind.keyword(),
                        span,
                    });
                }
            };
            if *kind == PrimitiveKind::Float32
                && value.is_finite()
                && value.abs() > f64::from(f32::MAX)
            {
                return Err(IdlError::NumberOutOfRange {
                    text: value.to_string(),
                    type_name: "float32",
                    span,
                });
            }
            Ok(())
        }
        ScalarType::Primitive(kind) => {
            let Literal::Int(value) = literal else {
                return Err(IdlError::InvalidLiteral {
                    expected: kind.keyword(),
                    span,
                });
            };
            // `int_range()` is `None` only for Bool/Float32/Float64, all
            // handled by the two arms above.
            let Some((min, max)) = kind.int_range() else {
                return Err(IdlError::InvalidLiteral {
                    expected: kind.keyword(),
                    span,
                });
            };
            if *value < min || *value > max {
                return Err(IdlError::NumberOutOfRange {
                    text: value.to_string(),
                    type_name: kind.keyword(),
                    span,
                });
            }
            Ok(())
        }
        ScalarType::Str {
            kind: StringKind::String,
            bound,
        } => {
            let Literal::Str(text) = literal else {
                return Err(IdlError::InvalidLiteral {
                    expected: "string",
                    span,
                });
            };
            match bound {
                Some(bound) if text.len() > *bound as usize => Err(IdlError::DefaultExceedsBound {
                    bound: *bound,
                    actual: text.len(),
                    span,
                }),
                _ => Ok(()),
            }
        }
        ScalarType::Str {
            kind: StringKind::WString,
            bound,
        } => {
            let Literal::Str(text) = literal else {
                return Err(IdlError::InvalidLiteral {
                    expected: "wstring",
                    span,
                });
            };
            let Some(bound) = bound else { return Ok(()) };
            let units = text.encode_utf16().count();
            if units > *bound as usize {
                Err(IdlError::DefaultExceedsBound {
                    bound: *bound,
                    actual: units,
                    span,
                })
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ast::NamedTypeRef;
    use crate::lexer::classify_line;
    use crate::span::Position;

    fn parse(text: &str) -> Result<Literal, IdlError> {
        let crate::lexer::Line::Tokens(tokens, _) = classify_line(1, text).expect("lexes") else {
            panic!("expected Tokens");
        };
        let line_end = tokens.last().map_or(Position::new(1, 1), |t| t.span.end);
        let mut cursor = Cursor::new(&tokens, line_end);
        parse_literal(&mut cursor)
    }

    fn at() -> Span {
        Span::empty(Position::new(1, 1))
    }

    fn scalar(scalar: ScalarType) -> FieldType {
        FieldType {
            scalar,
            array: ArraySuffix::None,
            span: at(),
        }
    }

    fn array_of(scalar: ScalarType, array: ArraySuffix) -> FieldType {
        FieldType {
            scalar,
            array,
            span: at(),
        }
    }

    #[test]
    fn booleans_parse() {
        assert_eq!(parse("true").unwrap(), Literal::Bool(true));
        assert_eq!(parse("false").unwrap(), Literal::Bool(false));
    }

    #[test]
    fn decimal_hex_and_underscored_integers_parse() {
        assert_eq!(parse("123").unwrap(), Literal::Int(123));
        assert_eq!(parse("-123").unwrap(), Literal::Int(-123));
        assert_eq!(parse("0xFF").unwrap(), Literal::Int(255));
        assert_eq!(parse("1_000_000").unwrap(), Literal::Int(1_000_000));
    }

    #[test]
    fn floats_and_exponents_parse() {
        assert_eq!(parse("1.5").unwrap(), Literal::Float(1.5));
        assert_eq!(parse("-2.5e3").unwrap(), Literal::Float(-2500.0));
    }

    #[test]
    fn strings_parse() {
        assert_eq!(parse("\"hi\"").unwrap(), Literal::Str("hi".to_owned()));
        assert_eq!(parse("'hi'").unwrap(), Literal::Str("hi".to_owned()));
    }

    #[test]
    fn arrays_parse_including_empty() {
        assert_eq!(
            parse("[1, 2, 3]").unwrap(),
            Literal::Array(vec![Literal::Int(1), Literal::Int(2), Literal::Int(3)])
        );
        assert_eq!(parse("[]").unwrap(), Literal::Array(Vec::new()));
    }

    #[test]
    fn nested_arrays_are_a_syntax_error_not_a_semantic_one() {
        let err = parse("[[1, 2]]").unwrap_err();
        assert!(matches!(err, IdlError::UnexpectedToken { .. }));
    }

    #[test]
    fn a_missing_comma_between_elements_is_rejected() {
        let err = parse("[1 2]").unwrap_err();
        assert!(matches!(err, IdlError::UnexpectedToken { .. }));
    }

    #[test]
    fn validate_scalar_accepts_a_value_within_its_primitives_range() {
        let field_type = scalar(ScalarType::Primitive(PrimitiveKind::Int8));
        assert_eq!(
            validate_value(&field_type, &Literal::Int(127), at()),
            Ok(())
        );
        assert_eq!(
            validate_value(&field_type, &Literal::Int(-128), at()),
            Ok(())
        );
    }

    #[test]
    fn validate_scalar_rejects_a_value_outside_its_primitives_range() {
        let field_type = scalar(ScalarType::Primitive(PrimitiveKind::Int8));
        assert_eq!(
            validate_value(&field_type, &Literal::Int(128), at()),
            Err(IdlError::NumberOutOfRange {
                text: "128".to_owned(),
                type_name: "int8",
                span: at(),
            })
        );
    }

    #[test]
    fn validate_scalar_accepts_an_integer_literal_for_a_float_field() {
        let field_type = scalar(ScalarType::Primitive(PrimitiveKind::Float64));
        assert_eq!(validate_value(&field_type, &Literal::Int(5), at()), Ok(()));
    }

    #[test]
    fn validate_scalar_rejects_a_float_literal_for_an_integer_field() {
        let field_type = scalar(ScalarType::Primitive(PrimitiveKind::Int32));
        assert!(validate_value(&field_type, &Literal::Float(5.0), at()).is_err());
    }

    #[test]
    fn validate_scalar_enforces_a_string_bound_in_bytes() {
        let field_type = scalar(ScalarType::Str {
            kind: StringKind::String,
            bound: Some(3),
        });
        assert_eq!(
            validate_value(&field_type, &Literal::Str("abcd".to_owned()), at()),
            Err(IdlError::DefaultExceedsBound {
                bound: 3,
                actual: 4,
                span: at()
            })
        );
    }

    #[test]
    fn validate_scalar_enforces_a_wstring_bound_in_utf16_units_not_bytes() {
        let field_type = scalar(ScalarType::Str {
            kind: StringKind::WString,
            bound: Some(1),
        });
        // "é" is 2 UTF-8 bytes but 1 UTF-16 code unit — must be accepted.
        assert_eq!(
            validate_value(&field_type, &Literal::Str("é".to_owned()), at()),
            Ok(())
        );
        // An astral-plane character costs 2 UTF-16 units even though it is
        // one `char` — must be rejected against a bound of 1.
        assert_eq!(
            validate_value(&field_type, &Literal::Str("\u{1f680}".to_owned()), at()),
            Err(IdlError::DefaultExceedsBound {
                bound: 1,
                actual: 2,
                span: at()
            })
        );
    }

    #[test]
    fn validate_array_enforces_fixed_length_exactly() {
        let field_type = array_of(
            ScalarType::Primitive(PrimitiveKind::Int32),
            ArraySuffix::Fixed(3),
        );
        let value = Literal::Array(vec![Literal::Int(1), Literal::Int(2)]);
        assert_eq!(
            validate_value(&field_type, &value, at()),
            Err(IdlError::ArrayLiteralLengthMismatch {
                expected: 3,
                actual: 2,
                span: at()
            })
        );
    }

    #[test]
    fn validate_array_enforces_the_bound_and_each_elements_own_type() {
        let field_type = array_of(
            ScalarType::Primitive(PrimitiveKind::Uint8),
            ArraySuffix::Bounded(2),
        );
        let too_many = Literal::Array(vec![Literal::Int(1), Literal::Int(2), Literal::Int(3)]);
        assert_eq!(
            validate_value(&field_type, &too_many, at()),
            Err(IdlError::DefaultExceedsBound {
                bound: 2,
                actual: 3,
                span: at()
            })
        );

        let bad_element = Literal::Array(vec![Literal::Int(1), Literal::Int(-1)]);
        assert_eq!(
            validate_value(&field_type, &bad_element, at()),
            Err(IdlError::NumberOutOfRange {
                text: "-1".to_owned(),
                type_name: "uint8",
                span: at(),
            })
        );
    }

    #[test]
    fn validate_rejects_any_default_on_a_named_type() {
        let field_type = scalar(ScalarType::Named(NamedTypeRef {
            package: None,
            name: "Point".to_owned(),
            span: at(),
        }));
        assert!(validate_value(&field_type, &Literal::Int(0), at()).is_err());
    }

    #[test]
    fn validate_rejects_a_non_array_literal_for_an_array_field() {
        let field_type = array_of(
            ScalarType::Primitive(PrimitiveKind::Int32),
            ArraySuffix::Unbounded,
        );
        assert!(validate_value(&field_type, &Literal::Int(1), at()).is_err());
    }

    // ---- Property tests -----------------------------------------------
    //
    // The unit tests above pin a handful of hand-picked boundaries (127 vs.
    // 128, "é" vs. an astral-plane character, …). These generalize the same
    // four validation rules — integer range, string/wstring bound, fixed
    // array length, bounded array length — across the *entire* input space
    // each rule is defined over, so a boundary this crate's own test author
    // did not think to hand-pick is checked exactly as strictly as one that
    // was.

    use proptest::prelude::*;

    proptest! {
        /// Every integer-kind `PrimitiveKind`'s `int_range()` is exactly
        /// where `validate_value` starts returning `NumberOutOfRange` — for
        /// all ten integer kinds, not just the ones the unit tests above
        /// happened to name, and across `i128`'s full range rather than
        /// values adjacent to one hand-picked boundary.
        #[test]
        fn integer_validation_matches_the_declared_range_exactly(
            kind in prop::sample::select(
                PrimitiveKind::ALL
                    .into_iter()
                    .filter(|kind| kind.int_range().is_some())
                    .collect::<Vec<_>>(),
            ),
            value in any::<i128>(),
        ) {
            let (min, max) = kind.int_range().expect("filtered to integer kinds");
            let field_type = scalar(ScalarType::Primitive(kind));
            let result = validate_value(&field_type, &Literal::Int(value), at());
            if (min..=max).contains(&value) {
                prop_assert_eq!(result, Ok(()));
            } else {
                prop_assert_eq!(
                    result,
                    Err(IdlError::NumberOutOfRange {
                        text: value.to_string(),
                        type_name: kind.keyword(),
                        span: at(),
                    })
                );
            }
        }

        /// A `string<=N>` bound is enforced in bytes, exactly at `<=`, for
        /// any UTF-8 text (including multi-byte characters, where the byte
        /// count and the character count diverge).
        #[test]
        fn string_bound_validation_matches_byte_length_exactly(
            text in ".{0,48}",
            bound in 0u32..64,
        ) {
            let field_type = scalar(ScalarType::Str { kind: StringKind::String, bound: Some(bound) });
            let result = validate_value(&field_type, &Literal::Str(text.clone()), at());
            if text.len() <= bound as usize {
                prop_assert_eq!(result, Ok(()));
            } else {
                prop_assert_eq!(
                    result,
                    Err(IdlError::DefaultExceedsBound { bound, actual: text.len(), span: at() })
                );
            }
        }

        /// A `wstring<=N>` bound is enforced in UTF-16 code units, not
        /// bytes and not `char`s — the property behind the two hand-picked
        /// "é" / astral-plane unit tests above, generalized to any text.
        #[test]
        fn wstring_bound_validation_matches_utf16_unit_count_exactly(
            text in ".{0,48}",
            bound in 0u32..64,
        ) {
            let field_type = scalar(ScalarType::Str { kind: StringKind::WString, bound: Some(bound) });
            let units = text.encode_utf16().count();
            let result = validate_value(&field_type, &Literal::Str(text.clone()), at());
            if units <= bound as usize {
                prop_assert_eq!(result, Ok(()));
            } else {
                prop_assert_eq!(
                    result,
                    Err(IdlError::DefaultExceedsBound { bound, actual: units, span: at() })
                );
            }
        }

        /// A fixed-size array default (`T[N]`) is accepted at exactly `N`
        /// elements and rejected at every other count.
        #[test]
        fn fixed_array_length_validation_is_exact(
            expected in 1u32..12,
            actual_len in 0usize..16,
        ) {
            let field_type = array_of(
                ScalarType::Primitive(PrimitiveKind::Int32),
                ArraySuffix::Fixed(expected),
            );
            let elements: Vec<Literal> = (0..actual_len).map(|i| Literal::Int(i as i128)).collect();
            let result = validate_value(&field_type, &Literal::Array(elements), at());
            if actual_len as u32 == expected {
                prop_assert_eq!(result, Ok(()));
            } else {
                prop_assert_eq!(
                    result,
                    Err(IdlError::ArrayLiteralLengthMismatch { expected, actual: actual_len, span: at() })
                );
            }
        }

        /// A bounded array default (`T[<=N]`) is accepted at `<=N` elements
        /// and rejected above it.
        #[test]
        fn bounded_array_length_validation_is_exact(
            bound in 0u32..12,
            actual_len in 0usize..16,
        ) {
            let field_type = array_of(
                ScalarType::Primitive(PrimitiveKind::Uint8),
                ArraySuffix::Bounded(bound),
            );
            let elements: Vec<Literal> = (0..actual_len).map(|_| Literal::Int(0)).collect();
            let result = validate_value(&field_type, &Literal::Array(elements), at());
            if actual_len as u32 <= bound {
                prop_assert_eq!(result, Ok(()));
            } else {
                prop_assert_eq!(
                    result,
                    Err(IdlError::DefaultExceedsBound { bound, actual: actual_len, span: at() })
                );
            }
        }
    }
}
