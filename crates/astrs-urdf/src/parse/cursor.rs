//! [`consume_element_body`] — the single primitive every element parser in
//! this module is built from.
//!
//! # The uniform postcondition
//!
//! Every `parse_*` function in this module shares one contract: called with
//! a `Reader` positioned *just after* an element's own [`Event::StartElement`]
//! (attributes already in hand from that event), it returns having consumed
//! *through* that element's own matching [`Event::EndElement`] — never more,
//! never less. That is exactly what [`consume_element_body`] does for a
//! "leaf" element (one this crate cares only about the attributes of, e.g.
//! `<parent link="..."/>` or `<mass value="..."/>`): parse the attributes
//! already in hand, then hand off to this function to swallow anything else
//! — real children this crate does not model, or just the synthesized close
//! of a self-closing tag — before returning.
//!
//! A "container" element (`<link>`, `<joint>`, `<geometry>`, ...) satisfies
//! the same postcondition a different way: its own dispatch loop calls a
//! recognized child's `parse_*` function (which, by the same contract,
//! leaves the reader at the next sibling) for every child it understands,
//! calls [`consume_element_body`] for every child it does not, and finally
//! breaks on its *own* [`Event::EndElement`] — so nothing downstream of
//! either kind of element parser ever needs to special-case which one it
//! was looking at.
//!
//! This uniform contract is what makes "silently skip an extension element
//! this crate does not model" (a `<gazebo>` or `<transmission>` block, both
//! common in real-world URDFs despite being outside URDF's core schema)
//! exactly as cheap and exactly as correct as parsing a recognized one:
//! both consume exactly their own subtree, no more.
//!
//! # The postcondition applies to `Ok`, deliberately not to `Err`
//!
//! An early `return Err(...)` from inside a `parse_*` function — a missing
//! required attribute, an unknown joint type — does **not** drain the rest
//! of that element's own body first, even where doing so is easy (a
//! self-closing element's single pending `EndElement`, say). This is
//! deliberate, not an oversight symmetric with the `Ok` case: the whole
//! parse is aborting either way, [`Reader::next_event`]'s own docs already
//! say its position after an error is unspecified and not meant to be read
//! again, and draining first would risk a *generic* [`crate::xml::XmlError`]
//! (from whatever malformed content follows) silently replacing the
//! specific, actionable [`crate::UrdfError`] this function was already
//! about to report. Only a **successful** early return — reaching the end
//! of a self-closing element's attributes with everything it needed already
//! in hand — has to drain first, because only that path lets the *caller's*
//! loop keep running afterward.

use crate::xml::{Event, Reader};

/// Consumes every event up to and including the [`Event::EndElement`] that
/// closes the element whose [`Event::StartElement`] the caller already
/// consumed. `self_closing` is that `StartElement`'s own flag: for a
/// self-closing element the synthesized close is the *only* remaining
/// event to consume (see [`crate::xml::Event::StartElement`]'s own docs on
/// why a caller can rely on the reader having queued exactly that one event
/// next); for anything else this walks a local depth counter that mirrors
/// [`Reader`]'s own internal element stack until it returns to zero.
///
/// # Errors
///
/// Whatever [`Reader::next_event`] returns, converted through
/// [`crate::UrdfError::Xml`].
pub(super) fn consume_element_body(
    reader: &mut Reader<'_>,
    self_closing: bool,
) -> crate::Result<()> {
    if self_closing {
        // Deterministic: a self-closing StartElement always leaves its own
        // synthesized EndElement as the reader's very next event (see
        // `Reader::next_event`'s `pending` slot) — nothing else can come
        // back here.
        reader.next_event()?;
        return Ok(());
    }
    let mut depth: u32 = 1;
    loop {
        match reader.next_event()? {
            // Every `StartElement` — self-closing or not — gets exactly
            // one matching `EndElement` later (real for the former, a
            // reader-synthesized one immediately following for the
            // latter; see `Event::StartElement`'s own docs), so `depth`
            // must count both alike: the `EndElement` arm below
            // decrements once per `EndElement` regardless of which kind
            // produced it, and only balances out if this arm increments
            // just as unconditionally.
            Event::StartElement { .. } => depth += 1,
            Event::EndElement { .. } => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            // `Reader` guarantees `Event::Eof` is only ever produced once
            // its own element stack is empty (any unclosed element instead
            // raises `XmlErrorKind::UnclosedElement` through the `?`
            // above), and this loop's `depth` stays >= 1 exactly as long
            // as that stack is non-empty (each arm above mirrors the
            // reader's own push/pop 1:1, and the caller already pushed one
            // entry by consuming the non-self-closing `StartElement` this
            // call is closing out) — so in practice this arm is never
            // taken. Kept as a reachable, typed-error arm rather than
            // `unreachable!()` so a future change to `Reader`'s own EOF
            // invariant would surface as an ordinary `UrdfError` instead of
            // a panic (`astrs-daemon::spawn::rt::arm` documents the
            // identical choice, and this workspace's policy already bans
            // `panic!`/`unwrap`/`expect` from non-test code outright).
            Event::Eof { span } => {
                return Err(crate::UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: String::new(),
                    },
                    span,
                )));
            }
            Event::Text { .. }
            | Event::CData { .. }
            | Event::Comment { .. }
            | Event::ProcessingInstruction { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// Wraps `inner_xml` in a synthetic `<root>` so a fixture with an
    /// "element under test" followed by a `<next/>` sibling is well-formed
    /// XML (a bare document can only ever have *one* top-level element —
    /// see `crate::xml`'s own `ContentAfterRoot`/`ContentBeforeRoot`).
    /// Consumes `<root>`'s own `StartElement` and the *first* child's
    /// `StartElement`, calls [`consume_element_body`] on that first child,
    /// then asserts the reader is positioned exactly at `expected_next`
    /// (or `<root>`'s own `EndElement` if `expected_next` is `None`) — the
    /// behavior every `parse_*` function in this module depends on.
    fn assert_consumes_through_own_close(inner_xml: &str, expected_next: Option<&str>) {
        let wrapped = format!("<root>{inner_xml}</root>");
        let mut reader = Reader::new(&wrapped);
        reader.next_event().expect("<root>"); // <root>'s own StartElement.
        let self_closing = match reader.next_event().expect("first inner element") {
            Event::StartElement { self_closing, .. } => self_closing,
            other => panic!("expected StartElement, got {other:?}"),
        };
        consume_element_body(&mut reader, self_closing).expect("consumes cleanly");
        match (reader.next_event().expect("next event"), expected_next) {
            (Event::EndElement { name: "root", .. }, None) => {}
            (Event::StartElement { name, .. }, Some(expected)) => assert_eq!(name, expected),
            (other, expected) => panic!("expected next={expected:?}, got {other:?}"),
        }
    }

    #[test]
    fn a_self_closing_leaf_consumes_just_its_own_synthesized_close() {
        assert_consumes_through_own_close("<mass value=\"1.0\"/><next/>", Some("next"));
    }

    #[test]
    fn an_explicitly_closed_empty_leaf_consumes_through_its_own_close() {
        assert_consumes_through_own_close("<mass value=\"1.0\"></mass><next/>", Some("next"));
    }

    #[test]
    fn an_element_with_nested_children_consumes_the_whole_subtree() {
        assert_consumes_through_own_close("<a><b><c/></b><d/></a><next/>", Some("next"));
    }

    #[test]
    fn a_single_self_closing_child_is_consumed_without_affecting_depth() {
        assert_consumes_through_own_close("<a><b/></a><next/>", Some("next"));
    }

    #[test]
    fn several_self_closing_children_in_a_row_do_not_underflow_depth() {
        // The regression case for the depth-tracking bug this module's
        // `consume_element_body` once had: every self-closing child's own
        // synthesized `EndElement` must be balanced by counting its
        // `StartElement` too, not skipped.
        assert_consumes_through_own_close("<a><b/><b/><b/></a><next/>", Some("next"));
    }

    #[test]
    fn text_comments_and_processing_instructions_inside_are_tolerated() {
        assert_consumes_through_own_close(
            "<a>some text<!-- a comment --><?pi data?></a><next/>",
            Some("next"),
        );
    }

    #[test]
    fn consuming_the_last_child_leaves_the_reader_at_the_parents_own_close() {
        assert_consumes_through_own_close("<a><b/></a>", None);
    }

    #[test]
    fn consuming_a_genuine_top_level_element_leaves_the_reader_at_eof() {
        // Unlike `assert_consumes_through_own_close`'s synthetic `<root>`
        // wrapper (needed only because a fixture with a `<next/>` sibling
        // is not otherwise well-formed XML), this exercises the real
        // top-level shape `parse::robot::parse_str` actually hits: no
        // wrapper, `<a>` genuinely is the document's one root element.
        let mut reader = Reader::new("<a><b/></a>");
        let self_closing = match reader.next_event().unwrap() {
            Event::StartElement { self_closing, .. } => self_closing,
            other => panic!("expected StartElement, got {other:?}"),
        };
        consume_element_body(&mut reader, self_closing).expect("consumes cleanly");
        assert!(matches!(reader.next_event().unwrap(), Event::Eof { .. }));
    }

    #[test]
    fn a_malformed_nested_element_still_surfaces_its_xml_error() {
        let mut reader = Reader::new("<a><b></a>");
        let self_closing = match reader.next_event().unwrap() {
            Event::StartElement { self_closing, .. } => self_closing,
            other => panic!("expected StartElement, got {other:?}"),
        };
        let error = consume_element_body(&mut reader, self_closing).unwrap_err();
        assert!(matches!(error, crate::UrdfError::Xml(_)));
    }
}
