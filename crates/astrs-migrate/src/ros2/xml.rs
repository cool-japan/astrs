//! A minimal, generic XML element tree -- the "model" layer for
//! `from-ros2`'s XML launch-file path, exactly as [`crate::dora::model`] is
//! the model layer for dora's YAML shape.
//!
//! Unlike dora's descriptor, a ROS 2 launch file has no natural
//! `#[derive(Deserialize)]` shape: its semantics (namespace scoping through
//! nested `<group>`s, `<include>` recursion) are a tree walk, not a flat
//! struct. This module owns only the syntax-level concern -- turning XML
//! text into a small in-memory tree of elements, attributes and children --
//! and knows nothing about what a `<node>` or `<include>` tag *means*; see
//! [`super::walk`] for that.
//!
//! Built directly on the workspace's `quick-xml` shim
//! (`oxixml-quickxml-compat`, blueprint §18 -- never a second XML
//! dependency): a real launch file is small (a handful to a few dozen
//! kilobytes), so buffering the whole document into one tree up front is
//! the pragmatic choice, the same call [`crate::dora::convert`]'s own docs
//! make for its (also small, one-shot) strongly-connected-component pass.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// One XML element: its (namespace-prefix-stripped) tag name, its
/// attributes in document order, and its child elements in document order.
///
/// Text content, comments, processing instructions and the XML declaration
/// are all discarded during parsing -- a ROS 2 launch file carries no
/// semantic information in element text, only in tags and attributes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct XmlElement {
    /// The element's local tag name (e.g. `"node"`), with any XML
    /// namespace prefix stripped. Launch files do not use XML namespaces
    /// in practice; this crate does not distinguish `<foo:node>` from
    /// `<node>` rather than silently misparsing one as the other.
    pub tag: String,
    /// This element's attributes, in document order, with entity
    /// references already unescaped (`&amp;` -> `&`, and so on).
    pub attrs: Vec<(String, String)>,
    /// This element's direct child elements, in document order.
    pub children: Vec<XmlElement>,
}

impl XmlElement {
    /// The value of the first attribute named `key`, if present.
    pub(crate) fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether this element carries an `if=`/`unless=` condition attribute
    /// -- one of the "unmappable constructs" blueprint §8.6 names by
    /// example (`LaunchConfiguration` substitutions almost always drive
    /// these). Returns the attribute name and its raw value for a caller
    /// building a note, in document order if (implausibly) both are
    /// present.
    pub(crate) fn conditions(&self) -> Vec<(&str, &str)> {
        self.attrs
            .iter()
            .filter(|(k, _)| k == "if" || k == "unless")
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

/// Parse `input` as XML into a generic [`XmlElement`] tree rooted at the
/// document's single top-level element.
///
/// # Errors
///
/// Returns a human-readable message (not a typed error -- this module is
/// crate-private, and [`super::mod@super`]'s callers wrap this in
/// [`crate::error::Ros2MigrateError::Xml`] with the path that gives it
/// context) if `input` is not well-formed XML, has no top-level element, or
/// has unbalanced tags.
pub(crate) fn parse(input: &str) -> Result<XmlElement, String> {
    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<XmlElement> = Vec::new();
    let mut root: Option<XmlElement> = None;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("XML syntax error at byte {}: {e}", reader.error_position()))?;
        match event {
            Event::Eof => break,
            Event::Start(start) => {
                stack.push(build_element(&start)?);
            }
            Event::Empty(start) => {
                let element = build_element(&start)?;
                attach(&mut stack, &mut root, element);
            }
            Event::End(end) => {
                let Some(element) = stack.pop() else {
                    return Err(format!(
                        "unexpected closing tag `</{}>` with no matching open tag",
                        String::from_utf8_lossy(end.name().as_ref())
                    ));
                };
                attach(&mut stack, &mut root, element);
            }
            // Text, comments, CDATA, the `<?xml ... ?>` declaration and
            // processing instructions carry no semantic content a launch
            // file's tags/attributes don't already carry.
            _ => {}
        }
        buf.clear();
    }

    if let Some(unclosed) = stack.into_iter().next() {
        return Err(format!(
            "XML input ended with unclosed element `<{}>`",
            unclosed.tag
        ));
    }
    root.ok_or_else(|| "XML input had no top-level element".to_string())
}

/// Attach a completed element to its parent (the top of `stack`), or set it
/// as `root` if `stack` is empty -- i.e. this is the document's top-level
/// element. A second top-level element after the first is unreachable for
/// well-formed XML (the grammar allows exactly one), so this does not
/// specially guard against it beyond letting the second silently replace
/// the first; `parse`'s caller only ever sees genuinely malformed input
/// rejected earlier, by `reader.read_event_into` itself.
fn attach(stack: &mut [XmlElement], root: &mut Option<XmlElement>, element: XmlElement) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(element);
    } else {
        *root = Some(element);
    }
}

fn build_element(start: &quick_xml::events::BytesStart<'_>) -> Result<XmlElement, String> {
    let tag = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
    let mut attrs = Vec::with_capacity(start.attributes().count());
    for attr in start.attributes() {
        let attr = attr.map_err(|e| format!("malformed attribute on <{tag}>: {e}"))?;
        let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| format!("malformed attribute value on <{tag}>: {e}"))?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(XmlElement {
        tag,
        attrs,
        children: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_a_minimal_self_closing_root() {
        let root = parse(r#"<launch><node pkg="p" exec="e"/></launch>"#).unwrap();
        assert_eq!(root.tag, "launch");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].tag, "node");
        assert_eq!(root.children[0].attr("pkg"), Some("p"));
        assert_eq!(root.children[0].attr("exec"), Some("e"));
    }

    #[test]
    fn parses_nested_open_close_elements() {
        let root =
            parse("<launch>\n  <group>\n    <node pkg=\"p\" exec=\"e\"/>\n  </group>\n</launch>\n")
                .unwrap();
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].tag, "group");
        assert_eq!(root.children[0].children.len(), 1);
        assert_eq!(root.children[0].children[0].tag, "node");
    }

    #[test]
    fn unescapes_entities_in_attribute_values() {
        let root = parse(r#"<launch><node name="a &amp; b"/></launch>"#).unwrap();
        assert_eq!(root.children[0].attr("name"), Some("a & b"));
    }

    #[test]
    fn ignores_declaration_and_comments() {
        let root =
            parse("<?xml version=\"1.0\"?>\n<!-- a comment -->\n<launch><!-- inner --></launch>")
                .unwrap();
        assert_eq!(root.tag, "launch");
        assert!(root.children.is_empty());
    }

    #[test]
    fn rejects_unclosed_elements() {
        let err = parse("<launch><node>").unwrap_err();
        assert!(err.contains("unclosed"), "{err}");
    }

    #[test]
    fn rejects_a_stray_closing_tag() {
        let err = parse("<launch></node></launch>").unwrap_err();
        assert!(
            err.contains("no matching open tag") || err.contains("syntax error"),
            "{err}"
        );
    }

    #[test]
    fn rejects_input_with_no_root_element() {
        let err = parse("   \n  ").unwrap_err();
        assert!(err.contains("no top-level element"), "{err}");
    }

    #[test]
    fn conditions_finds_if_and_unless_attributes() {
        let root = parse(r#"<node if="$(var use_a)" unless="$(var use_b)"/>"#).unwrap();
        let conditions = root.conditions();
        assert_eq!(conditions.len(), 2);
        assert!(conditions.contains(&("if", "$(var use_a)")));
        assert!(conditions.contains(&("unless", "$(var use_b)")));
    }

    #[test]
    fn conditions_is_empty_when_absent() {
        let root = parse(r#"<node pkg="p"/>"#).unwrap();
        assert!(root.conditions().is_empty());
    }

    #[test]
    fn xml_namespace_prefixes_are_stripped_from_tag_names() {
        // Launch files do not use XML namespaces in practice, but the
        // parser should not choke on one if present -- `local_name()`
        // strips the prefix rather than rejecting the document.
        let root = parse(r#"<launch xmlns:x="urn:example"><x:node pkg="p"/></launch>"#).unwrap();
        assert_eq!(root.children[0].tag, "node");
    }
}
