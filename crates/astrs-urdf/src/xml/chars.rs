//! Character classification: the XML 1.0 `Char`, `NameStartChar` and
//! `NameChar` productions (simplified — see each function's docs for
//! exactly how), plus the five predefined entity names.
//!
//! Kept apart from [`super::Reader`] itself so the character-level rules
//! this module encodes can be unit-tested in isolation from the scanning
//! control flow that calls them.

/// XML's `S` (whitespace) production: `(#x20 | #x9 | #xD | #xA)+`, tested
/// per character.
#[must_use]
pub(crate) const fn is_xml_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// A simplified `NameStartChar`: ASCII letters, `_`, `:`, and anything
/// Unicode considers alphabetic.
///
/// The real XML 1.0 production enumerates specific Unicode code point
/// ranges (`NameStartChar ::= ":" | [A-Z] | "_" | [a-z] | [#xC0-#xD6] |
/// ...`); this crate accepts the same practical character set through
/// [`char::is_alphabetic`] instead of transcribing every range by hand.
/// Every ASCII identifier URDF actually uses (link and joint names are
/// conventionally `[a-zA-Z0-9_-]+`) parses identically either way, and a
/// name using a rarer Unicode letter block parses the same as the real
/// grammar would; the only theoretical gap is a handful of Unicode
/// code points the XML grammar admits as name characters that
/// [`char::is_alphabetic`] does not classify as alphabetic (or vice
/// versa) — deliberately out of scope for a *minimal* parser.
#[must_use]
pub(crate) fn is_name_start_char(c: char) -> bool {
    c == ':' || c == '_' || c.is_alphabetic()
}

/// A simplified `NameChar`: [`is_name_start_char`] plus `-`, `.` and ASCII
/// digits. See [`is_name_start_char`] for the same simplification applied
/// consistently to continuation characters.
#[must_use]
pub(crate) fn is_name_char(c: char) -> bool {
    is_name_start_char(c) || c == '-' || c == '.' || c.is_ascii_digit()
}

/// XML 1.0's `Char` production: `#x9 | #xA | #xD | [#x20-#xD7FF] |
/// [#xE000-#xFFFD] | [#x10000-#x10FFFF]` — every code point a numeric
/// character reference (`&#N;`/`&#xN;`) is allowed to name.
///
/// `char::from_u32` already rejects surrogate halves and values above
/// `U+10FFFF` (neither is a legal [`char`] at all), so this only needs to
/// additionally reject the C0 control codes below `U+20` other than tab/LF/
/// CR, plus the two non-characters `U+FFFE`/`U+FFFF`.
#[must_use]
pub(crate) const fn is_legal_xml_char(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{A}' | '\u{D}')
        || matches!(c, '\u{20}'..='\u{D7FF}')
        || matches!(c, '\u{E000}'..='\u{FFFD}')
        || matches!(c, '\u{10000}'..='\u{10FFFF}')
}

/// Resolves one of the five entity names XML 1.0 predefines
/// (`amp`/`lt`/`gt`/`apos`/`quot`) to its character, or `None` for any
/// other name.
///
/// Matching is case-sensitive, as XML itself is throughout: `&AMP;` is not
/// `&amp;`.
#[must_use]
pub(crate) const fn resolve_named_entity(name: &str) -> Option<char> {
    match name.as_bytes() {
        b"amp" => Some('&'),
        b"lt" => Some('<'),
        b"gt" => Some('>'),
        b"apos" => Some('\''),
        b"quot" => Some('"'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn whitespace_is_exactly_the_four_xml_whitespace_characters() {
        for c in [' ', '\t', '\r', '\n'] {
            assert!(is_xml_whitespace(c), "{c:?} should be whitespace");
        }
        for c in ['a', '_', '\u{b}', '\u{c}'] {
            assert!(!is_xml_whitespace(c), "{c:?} should not be whitespace");
        }
    }

    #[test]
    fn name_start_chars_include_letters_underscore_and_colon() {
        for c in ['a', 'Z', '_', ':', 'あ'] {
            assert!(is_name_start_char(c), "{c:?} should start a name");
        }
        for c in ['0', '-', '.', ' ', '<'] {
            assert!(!is_name_start_char(c), "{c:?} should not start a name");
        }
    }

    #[test]
    fn name_chars_additionally_allow_hyphen_dot_and_digits() {
        for c in ['-', '.', '5'] {
            assert!(is_name_char(c), "{c:?} should continue a name");
        }
        assert!(!is_name_char(' '));
        assert!(!is_name_char('/'));
    }

    #[test]
    fn legal_xml_chars_exclude_c0_controls_other_than_tab_lf_cr() {
        assert!(is_legal_xml_char('\t'));
        assert!(is_legal_xml_char('\n'));
        assert!(is_legal_xml_char('\r'));
        assert!(is_legal_xml_char(' '));
        assert!(is_legal_xml_char('A'));
        assert!(!is_legal_xml_char('\u{0}'));
        assert!(!is_legal_xml_char('\u{1}'));
        assert!(!is_legal_xml_char('\u{b}'));
        assert!(!is_legal_xml_char('\u{c}'));
    }

    #[test]
    fn legal_xml_chars_exclude_the_noncharacters_and_admit_astral_scalars() {
        assert!(!is_legal_xml_char('\u{fffe}'));
        assert!(!is_legal_xml_char('\u{ffff}'));
        assert!(is_legal_xml_char('\u{10000}'));
        assert!(is_legal_xml_char('\u{10ffff}'));
    }

    #[test]
    fn the_five_predefined_entities_resolve_case_sensitively() {
        assert_eq!(resolve_named_entity("amp"), Some('&'));
        assert_eq!(resolve_named_entity("lt"), Some('<'));
        assert_eq!(resolve_named_entity("gt"), Some('>'));
        assert_eq!(resolve_named_entity("apos"), Some('\''));
        assert_eq!(resolve_named_entity("quot"), Some('"'));
        assert_eq!(resolve_named_entity("AMP"), None);
        assert_eq!(resolve_named_entity("nbsp"), None);
        assert_eq!(resolve_named_entity(""), None);
    }
}
