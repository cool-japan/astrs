//! Unit tests for [`super::Reader`] itself — element/attribute/text/CDATA/
//! comment/processing-instruction scanning, the five predefined entities
//! and numeric character references, end-of-line and attribute-value
//! normalization, resource limits, and malformed-input rejection.
//!
//! `crate::parse`'s own tests exercise the URDF-shaped consumer built on
//! top of this reader; these tests stay purely at the XML syntax layer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// Collects every event from `input` (via [`Reader::new`]), panicking on
/// the first error — a convenience for tests asserting a *successful*
/// parse's event shape.
fn events(input: &str) -> Vec<Event<'_>> {
    let mut reader = Reader::new(input);
    let mut out = Vec::new();
    loop {
        match reader.next_event().expect("well-formed input") {
            Event::Eof { .. } => break,
            event => out.push(event),
        }
    }
    out
}

/// The first error `input` produces, panicking if it parses cleanly to
/// EOF.
fn first_error(input: &str) -> XmlError {
    let mut reader = Reader::new(input);
    loop {
        match reader.next_event() {
            Ok(Event::Eof { .. }) => panic!("{input:?} parsed cleanly; expected an error"),
            Ok(_) => continue,
            Err(error) => return error,
        }
    }
}

fn attr<'a>(event: &'a Event<'_>, name: &str) -> &'a str {
    let Event::StartElement { attributes, .. } = event else {
        panic!("not a StartElement: {event:?}");
    };
    &attributes.iter().find(|a| a.name == name).unwrap().value
}

// ---------------------------------------------------------------------
// Elements and attributes
// ---------------------------------------------------------------------

#[test]
fn a_self_closing_element_synthesizes_its_own_end_element() {
    let evs = events("<robot/>");
    assert!(matches!(
        evs.as_slice(),
        [
            Event::StartElement {
                name: "robot",
                self_closing: true,
                ..
            },
            Event::EndElement { name: "robot", .. },
        ]
    ));
}

#[test]
fn an_explicitly_closed_element_is_not_self_closing() {
    let evs = events("<robot></robot>");
    assert!(matches!(
        evs.as_slice(),
        [
            Event::StartElement {
                name: "robot",
                self_closing: false,
                ..
            },
            Event::EndElement { name: "robot", .. },
        ]
    ));
}

#[test]
fn nested_elements_scan_in_document_order() {
    let evs = events("<a><b><c/></b></a>");
    let names: Vec<&str> = evs
        .iter()
        .map(|e| match e {
            Event::StartElement { name, .. } | Event::EndElement { name, .. } => *name,
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    assert_eq!(names, ["a", "b", "c", "c", "b", "a"]);
}

#[test]
fn attributes_parse_in_document_order_with_either_quote_style() {
    let evs = events(r#"<link name="base_link" mass='1.5'/>"#);
    let Event::StartElement { attributes, .. } = &evs[0] else {
        panic!("expected StartElement");
    };
    assert_eq!(attributes.len(), 2);
    assert_eq!(attributes[0].name, "name");
    assert_eq!(attributes[0].value, "base_link");
    assert_eq!(attributes[1].name, "mass");
    assert_eq!(attributes[1].value, "1.5");
}

#[test]
fn whitespace_around_attributes_and_the_equals_sign_is_insignificant() {
    let evs = events("<a   x  =   \"1\"   y=\"2\"   />");
    assert_eq!(attr(&evs[0], "x"), "1");
    assert_eq!(attr(&evs[0], "y"), "2");
}

#[test]
fn duplicate_attributes_are_rejected() {
    let error = first_error(r#"<a x="1" x="2"/>"#);
    assert!(matches!(
        error.kind,
        XmlErrorKind::DuplicateAttribute { name } if name == "x"
    ));
}

#[test]
fn depth_is_checked_only_for_elements_that_can_have_children() {
    // A run of self-closing siblings never nests, however many there are.
    let evs = events("<a><b/><b/><b/></a>");
    assert_eq!(evs.len(), 2 + 3 * 2); // <a>,</a> plus 3 x (start,end) for b
}

#[test]
fn depth_reports_the_current_nesting() {
    let mut reader = Reader::new("<a><b><c></c></b></a>");
    assert_eq!(reader.depth(), 0);
    reader.next_event().unwrap(); // <a>
    assert_eq!(reader.depth(), 1);
    reader.next_event().unwrap(); // <b>
    assert_eq!(reader.depth(), 2);
    reader.next_event().unwrap(); // <c>
    assert_eq!(reader.depth(), 3);
}

// ---------------------------------------------------------------------
// Text, CDATA, comments, processing instructions
// ---------------------------------------------------------------------

#[test]
fn text_content_is_captured_between_markup() {
    let evs = events("<a>hello world</a>");
    assert!(matches!(&evs[1], Event::Text { content, .. } if content == "hello world"));
}

#[test]
fn cdata_content_is_returned_verbatim_with_no_entity_expansion() {
    let evs = events("<a><![CDATA[<b> & </b>]]></a>");
    assert!(matches!(&evs[1], Event::CData { content, .. } if *content == "<b> & </b>"));
}

#[test]
fn comments_are_returned_verbatim_and_do_not_close_the_element_they_sit_in() {
    let evs = events("<a><!-- a <fake> tag & entity --></a>");
    assert!(matches!(
        &evs[1],
        Event::Comment { content, .. } if *content == " a <fake> tag & entity "
    ));
}

#[test]
fn the_xml_declaration_is_a_processing_instruction_and_is_ignored_structurally() {
    let evs = events(r#"<?xml version="1.0" encoding="UTF-8"?><robot/>"#);
    assert!(matches!(
        &evs[0],
        Event::ProcessingInstruction { target: "xml", .. }
    ));
    assert!(matches!(&evs[1], Event::StartElement { name: "robot", .. }));
}

#[test]
fn a_doctype_declaration_is_skipped_and_produces_no_event() {
    let evs = events(r#"<!DOCTYPE robot SYSTEM "robot.dtd"><robot/>"#);
    assert_eq!(evs.len(), 2); // just StartElement + EndElement for <robot/>
    assert!(matches!(&evs[0], Event::StartElement { name: "robot", .. }));
}

#[test]
fn a_doctype_with_a_bracketed_internal_subset_containing_a_close_angle_is_skipped() {
    let evs = events("<!DOCTYPE a [ <!ENTITY foo \"a > b\"> ]><a/>");
    assert!(matches!(&evs[0], Event::StartElement { name: "a", .. }));
}

// ---------------------------------------------------------------------
// Entities and character references
// ---------------------------------------------------------------------

#[test]
fn the_five_predefined_entities_decode_in_text() {
    let evs = events("<a>&amp; &lt; &gt; &apos; &quot;</a>");
    assert!(matches!(&evs[1], Event::Text { content, .. } if content == "& < > ' \""));
}

#[test]
fn the_five_predefined_entities_decode_in_attribute_values() {
    let evs = events(r#"<a x="&amp;&lt;&gt;&apos;&quot;"/>"#);
    assert_eq!(attr(&evs[0], "x"), "&<>'\"");
}

#[test]
fn decimal_and_hex_numeric_character_references_decode() {
    // &#65; and &#x41; are both 'A' (decimal 65 == hex 0x41); &#97; is 'a'.
    let evs = events("<a>&#65;&#x41;&#97;</a>");
    assert!(matches!(&evs[1], Event::Text { content, .. } if content == "AAa"));
}

#[test]
fn an_unknown_entity_name_is_rejected() {
    let error = first_error("<a>&nbsp;</a>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::UnknownEntity { name } if name == "nbsp"
    ));
}

#[test]
fn an_unterminated_entity_reference_is_rejected() {
    let error = first_error("<a>&amp</a>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::MalformedReference { .. }
    ));
}

#[test]
fn a_bare_ampersand_not_starting_a_reference_is_rejected() {
    let error = first_error("<a>fish & chips</a>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::MalformedReference { .. }
    ));
}

#[test]
fn a_character_reference_naming_an_illegal_control_code_is_rejected() {
    let error = first_error("<a>&#1;</a>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::InvalidCharacterReference { .. }
    ));
}

#[test]
fn a_character_reference_naming_an_unpaired_surrogate_is_rejected() {
    let error = first_error("<a>&#xD800;</a>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::InvalidCharacterReference { .. }
    ));
}

#[test]
fn a_character_reference_with_no_digits_is_malformed() {
    let error = first_error("<a>&#;</a>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::MalformedReference { .. }
    ));
}

#[test]
fn an_astral_character_reference_decodes_correctly() {
    // U+1F600 GRINNING FACE.
    let evs = events("<a>&#x1F600;</a>");
    assert!(matches!(&evs[1], Event::Text { content, .. } if content == "\u{1F600}"));
}

// ---------------------------------------------------------------------
// End-of-line and attribute-value normalization
// ---------------------------------------------------------------------

#[test]
fn crlf_and_lone_cr_normalize_to_a_single_lf_in_text() {
    let evs = events("<a>one\r\ntwo\rthree\nfour</a>");
    assert!(matches!(
        &evs[1],
        Event::Text { content, .. } if content == "one\ntwo\nthree\nfour"
    ));
}

#[test]
fn crlf_line_endings_still_advance_the_line_counter_exactly_once() {
    let mut reader = Reader::new("<a>\r\n</a>");
    reader.next_event().unwrap(); // <a>
    reader.next_event().unwrap(); // text "\n"
    let end = reader.next_event().unwrap(); // </a>
    assert!(matches!(end, Event::EndElement { span, .. } if span.start.line == 2));
}

#[test]
fn literal_tab_and_newline_fold_to_a_single_space_in_attribute_values() {
    let evs = events("<a x=\"one\ttwo\r\nthree\"/>");
    assert_eq!(attr(&evs[0], "x"), "one two three");
}

#[test]
fn a_character_reference_to_a_whitespace_codepoint_is_not_folded_in_an_attribute_value() {
    // Attribute-value normalization only folds *literal* whitespace bytes,
    // never a decoded character reference — XML 1.0 §3.3.3.
    let evs = events("<a x=\"one&#9;two\"/>");
    assert_eq!(attr(&evs[0], "x"), "one\ttwo");
}

// ---------------------------------------------------------------------
// Zero-copy borrowing
// ---------------------------------------------------------------------

#[test]
fn text_with_no_entities_or_carriage_returns_borrows_the_source() {
    let evs = events("<a>plain text</a>");
    let Event::Text { content, .. } = &evs[1] else {
        panic!("expected Text");
    };
    assert!(matches!(content, std::borrow::Cow::Borrowed(_)));
}

#[test]
fn text_containing_an_entity_is_owned() {
    let evs = events("<a>a &amp; b</a>");
    let Event::Text { content, .. } = &evs[1] else {
        panic!("expected Text");
    };
    assert!(matches!(content, std::borrow::Cow::Owned(_)));
}

// ---------------------------------------------------------------------
// Root-element structure
// ---------------------------------------------------------------------

#[test]
fn an_empty_document_has_no_root_element() {
    let error = first_error("");
    assert!(matches!(error.kind, XmlErrorKind::MissingRootElement));
}

#[test]
fn a_whitespace_only_document_has_no_root_element() {
    let error = first_error("   \n\t  ");
    assert!(matches!(error.kind, XmlErrorKind::MissingRootElement));
}

#[test]
fn leading_whitespace_comments_and_a_pi_before_the_root_are_fine() {
    let evs = events("  <?xml version=\"1.0\"?>\n<!-- hi -->\n<a/>");
    assert!(matches!(
        evs.last(),
        Some(Event::EndElement { name: "a", .. })
    ));
}

#[test]
fn non_whitespace_text_before_the_root_is_rejected() {
    let error = first_error("hello<a/>");
    assert!(matches!(error.kind, XmlErrorKind::ContentBeforeRoot));
}

#[test]
fn a_second_top_level_element_after_the_root_closed_is_rejected() {
    let error = first_error("<a/><b/>");
    assert!(matches!(error.kind, XmlErrorKind::ContentAfterRoot));
}

#[test]
fn non_whitespace_text_after_the_root_closed_is_rejected() {
    let error = first_error("<a/>stray");
    assert!(matches!(error.kind, XmlErrorKind::ContentAfterRoot));
}

#[test]
fn trailing_whitespace_and_comments_after_the_root_are_fine() {
    let mut reader = Reader::new("<a/>\n<!-- bye -->\n");
    loop {
        match reader.next_event().unwrap() {
            Event::Eof { .. } => break,
            _ => continue,
        }
    }
}

#[test]
fn eof_is_returned_again_on_a_repeated_call() {
    let mut reader = Reader::new("<a/>");
    reader.next_event().unwrap(); // <a>
    reader.next_event().unwrap(); // </a>
    assert!(matches!(reader.next_event(), Ok(Event::Eof { .. })));
    assert!(matches!(reader.next_event(), Ok(Event::Eof { .. })));
}

// ---------------------------------------------------------------------
// Malformed XML
// ---------------------------------------------------------------------

#[test]
fn an_unclosed_element_is_rejected_at_eof() {
    let error = first_error("<a><b></b>");
    assert!(matches!(error.kind, XmlErrorKind::UnclosedElement { name } if name == "a"));
}

#[test]
fn a_mismatched_closing_tag_is_rejected() {
    let error = first_error("<a><b></a></b>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::MismatchedClosingTag { expected, found }
            if expected == "b" && found == "a"
    ));
}

#[test]
fn a_closing_tag_with_no_open_element_is_rejected() {
    let error = first_error("</a>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::UnexpectedClosingTag { found } if found == "a"
    ));
}

#[test]
fn an_unterminated_comment_is_rejected() {
    let error = first_error("<a><!-- oops</a>");
    assert!(matches!(error.kind, XmlErrorKind::UnterminatedComment));
}

#[test]
fn an_unterminated_cdata_section_is_rejected() {
    let error = first_error("<a><![CDATA[ oops</a>");
    assert!(matches!(error.kind, XmlErrorKind::UnterminatedCData));
}

#[test]
fn an_unterminated_processing_instruction_is_rejected() {
    let error = first_error("<?xml version=\"1.0\"");
    assert!(matches!(
        error.kind,
        XmlErrorKind::UnterminatedProcessingInstruction
    ));
}

#[test]
fn a_truncated_start_tag_is_rejected() {
    let error = first_error("<robot");
    assert!(matches!(error.kind, XmlErrorKind::UnterminatedTag));
}

#[test]
fn an_attribute_value_missing_its_closing_quote_is_rejected() {
    let error = first_error("<a x=\"unterminated>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::UnterminatedAttributeValue
    ));
}

#[test]
fn a_literal_less_than_inside_an_attribute_value_is_rejected() {
    let error = first_error("<a x=\"a<b\"/>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::UnexpectedCharacter { found: '<' }
    ));
}

#[test]
fn an_attribute_value_not_starting_with_a_quote_is_rejected() {
    let error = first_error("<a x=1/>");
    assert!(matches!(error.kind, XmlErrorKind::MissingAttributeQuote));
}

#[test]
fn an_attribute_name_not_followed_by_equals_is_rejected() {
    let error = first_error("<a x/>");
    assert!(matches!(
        error.kind,
        XmlErrorKind::MissingAttributeEquals { name } if name == "x"
    ));
}

#[test]
fn a_name_that_cannot_start_with_a_digit_is_rejected() {
    let error = first_error("<1a/>");
    assert!(matches!(error.kind, XmlErrorKind::InvalidName));
}

#[test]
fn a_slash_not_immediately_followed_by_gt_is_rejected() {
    // XML requires `/>` with no space between `/` and `>`.
    let error = first_error("<a / >");
    assert!(matches!(
        error.kind,
        XmlErrorKind::UnexpectedCharacter { found: ' ' }
    ));
}

// ---------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------

#[test]
fn input_larger_than_the_configured_byte_limit_is_rejected() {
    let limits = Limits::default().with_max_input_bytes(4);
    let mut reader = Reader::with_limits("<abcdef/>", limits);
    let error = reader.next_event().unwrap_err();
    assert!(matches!(
        error.kind,
        XmlErrorKind::InputTooLarge {
            limit: 4,
            actual: 9
        }
    ));
}

#[test]
fn input_at_exactly_the_byte_limit_is_accepted() {
    let input = "<a/>"; // 4 bytes
    let limits = Limits::default().with_max_input_bytes(4);
    let mut reader = Reader::with_limits(input, limits);
    assert!(reader.next_event().is_ok());
}

#[test]
fn nested_elements_within_the_depth_limit_are_accepted() {
    let limits = Limits::default().with_max_depth(3);
    let mut reader = Reader::with_limits("<a><b><c></c></b></a>", limits);
    loop {
        match reader
            .next_event()
            .expect("depth 3 is within the limit of 3")
        {
            Event::Eof { .. } => break,
            _ => continue,
        }
    }
}

#[test]
fn nested_elements_exceeding_the_depth_limit_are_rejected() {
    let limits = Limits::default().with_max_depth(2);
    let mut reader = Reader::with_limits("<a><b><c></c></b></a>", limits);
    reader.next_event().unwrap(); // <a> (depth 1)
    reader.next_event().unwrap(); // <b> (depth 2)
    let error = reader.next_event().unwrap_err(); // <c> would be depth 3
    assert!(matches!(
        error.kind,
        XmlErrorKind::DepthLimitExceeded { limit: 2 }
    ));
}

#[test]
fn a_run_of_self_closing_siblings_never_trips_the_depth_limit() {
    let limits = Limits::default().with_max_depth(1);
    let mut reader = Reader::with_limits("<a><b/><b/><b/></a>", limits);
    loop {
        match reader.next_event().unwrap() {
            Event::Eof { .. } => break,
            _ => continue,
        }
    }
}

// ---------------------------------------------------------------------
// UTF-8 and Unicode names
// ---------------------------------------------------------------------

#[test]
fn from_utf8_bytes_accepts_valid_utf8() {
    let reader = Reader::from_utf8_bytes("<a/>".as_bytes());
    assert!(reader.is_ok());
}

#[test]
fn from_utf8_bytes_rejects_invalid_utf8() {
    let bytes: &[u8] = &[b'<', b'a', 0xff, 0xfe, b'/', b'>'];
    let error = Reader::from_utf8_bytes(bytes).unwrap_err();
    assert!(matches!(error.kind, XmlErrorKind::InvalidUtf8));
}

#[test]
fn a_multi_byte_unicode_name_and_content_round_trip() {
    let evs = events("<ロボット>こんにちは</ロボット>");
    assert!(matches!(
        &evs[0],
        Event::StartElement {
            name: "ロボット",
            ..
        }
    ));
    assert!(matches!(&evs[1], Event::Text { content, .. } if content == "こんにちは"));
}

#[test]
fn positions_count_columns_in_chars_not_bytes() {
    let mut reader = Reader::new("<あ/>");
    let event = reader.next_event().unwrap();
    let Event::StartElement { span, .. } = event else {
        panic!("expected StartElement");
    };
    // "<あ/>" is 6 bytes (あ is 3 bytes) but 4 chars; the closing '>' sits
    // at the 5th *character*, column 5, not byte offset 7.
    assert_eq!(span.end.column, 5);
}
