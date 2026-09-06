//! A minimal, position-tracked XML 1.0 pull parser — this crate's own, with
//! no dependency beyond `std` (blueprint §5.3 asks for exactly that: URDF is
//! the only XML this workspace parses, and the general-purpose XML crates
//! elsewhere in this workspace, e.g. `astrs-migrate`'s use of the
//! `oxixml-quickxml-compat` shim, are a poor fit here — that path builds a
//! whole document tree in memory and reports errors by byte offset, not the
//! carat-precise `line:column` spans a URDF diagnostic needs).
//!
//! # What this parser is
//!
//! A pull ("streaming") reader: [`Reader::next_event`] returns one
//! [`Event`] at a time — [`Event::StartElement`], [`Event::EndElement`],
//! [`Event::Text`], [`Event::CData`], [`Event::Comment`],
//! [`Event::ProcessingInstruction`] or [`Event::Eof`] — rather than building
//! a tree. [`crate::parse`] is the tree-shaped (well, URDF-shaped) consumer
//! built on top; nothing in this module knows what a `<link>` or `<joint>`
//! means.
//!
//! Supported: elements and attributes (single- or double-quoted, with XML's
//! own attribute-value whitespace normalization), text content, `<!--
//! comments -->`, `<![CDATA[ sections ]]>`, the `<?xml ... ?>` declaration
//! and other processing instructions, a best-effort `<!DOCTYPE ...>` skip,
//! the five predefined entities (`&amp; &lt; &gt; &apos; &quot;`), and
//! decimal/hexadecimal numeric character references (`&#60;`, `&#x3C;`) —
//! one step past the five predefined entities item 1 of this crate's own
//! design brief names explicitly, included because a numeric reference is
//! no more work to decode correctly once entity decoding exists at all, and
//! excluding it would make this parser reject well-formed documents no
//! other XML tool would. Every event and every [`XmlError`] carries a
//! [`Span`] built from one-based, `char`-counted [`Position`]s (see that
//! module for why `char` rather than byte columns), and the whole input is
//! ceilinged by [`Limits`] (input size, element nesting depth).
//!
//! # What it deliberately is not
//!
//! - **Not a validating parser.** No DTD/schema validation, no external
//!   entity resolution, no namespace processing (a `<foo:bar>` tag's name is
//!   `"foo:bar"` verbatim — URDF does not use XML namespaces).
//! - **Not fully XML-1.0-conformant** at the edges: this crate's private
//!   `chars` module approximates the real `Name`/`NameStart` productions
//!   via [`char::is_alphabetic`] rather than enumerating every Unicode
//!   range the spec lists (see that module's own docs), comments are not
//!   checked for an illegal embedded `--`, and a literal (unescaped) `>`
//!   in text content is tolerated rather than rejected — the same
//!   relaxation every mainstream XML parser (`expat`, `libxml2`,
//!   `quick-xml`) makes in practice, since `>` is unambiguous outside
//!   markup. What *is* enforced strictly: `<` must be escaped, exactly one
//!   root element, well-formed nesting, and the `Char` production for
//!   numeric references.
//! - **Comment, CDATA and processing-instruction content is returned
//!   verbatim** from the source, byte for byte — including a literal `\r`.
//!   Unlike [`Event::Text`] and attribute values, that content is never
//!   semantically consumed by [`crate::parse`] (URDF carries no data in
//!   element text; every value lives in an attribute), so this module does
//!   not pay to end-of-line-normalize content nobody reads. [`Event::Text`]
//!   and attribute values, which *are* consumed, are fully normalized: XML
//!   1.0 §2.11's `"\r\n"`/`"\r"` → `"\n"` end-of-line rule, then (for
//!   attribute values only) §3.3.3's literal-whitespace-to-space folding.
//!
//! # Zero-copy where nothing needs decoding
//!
//! [`Event::StartElement::name`]/[`Event::EndElement::name`]/
//! [`Attribute::name`] and comment/CDATA/PI content always borrow directly
//! from the input `&str` — XML names and raw content never need rewriting.
//! [`Event::Text::content`] and [`Attribute::value`] are `Cow<str>`:
//! borrowed for the (overwhelmingly common) run with no entity reference
//! and no literal `\r`, owned only from the first character that actually
//! needs rewriting onward.
//!
//! # Example
//!
//! ```
//! use astrs_urdf::xml::{Event, Reader};
//!
//! let xml = r#"<?xml version="1.0"?>
//! <robot name="demo">
//!   <!-- a comment -->
//!   <link name="base_link"/>
//!   <text><![CDATA[raw <stuff> & things]]></text>
//! </robot>
//! "#;
//! let mut reader = Reader::new(xml);
//! let mut link_names = Vec::new();
//! loop {
//!     match reader.next_event().unwrap() {
//!         Event::StartElement { name: "link", attributes, .. } => {
//!             let name = attributes.iter().find(|a| a.name == "name").unwrap();
//!             link_names.push(name.value.to_string());
//!         }
//!         Event::Eof { .. } => break,
//!         _ => {}
//!     }
//! }
//! assert_eq!(link_names, ["base_link"]);
//! ```

mod chars;
mod error;
mod limits;
mod position;

#[cfg(test)]
mod tests;

use std::borrow::Cow;

pub use error::{XmlError, XmlErrorKind};
pub use limits::Limits;
pub use position::{Position, Span};

/// One attribute on a [`Event::StartElement`]: `name="value"` (or
/// `name='value'`), in document order.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute<'a> {
    /// The attribute's name, exactly as written (never entity-decoded — XML
    /// names cannot contain entity references).
    pub name: &'a str,
    /// The attribute's value, entity-decoded and whitespace-normalized (XML
    /// 1.0 §3.3.3): borrowed when the raw text needed no rewriting, owned
    /// otherwise.
    pub value: Cow<'a, str>,
    /// The span of [`Attribute::name`] alone.
    pub name_span: Span,
    /// The span of the whole quoted value, delimiting quotes included.
    pub value_span: Span,
}

/// One token [`Reader::next_event`] can produce.
///
/// Every variant carries a [`Span`]: for [`Event::StartElement`],
/// [`Event::EndElement`], [`Event::Comment`], [`Event::CData`] and
/// [`Event::ProcessingInstruction`] the span covers the whole construct,
/// delimiters included (`<tag ...>` through its own `>`, `<!--` through
/// `-->`, and so on); for [`Event::Text`] it covers the run of content
/// between two markup constructs.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<'a> {
    /// `<name attr="value" ...>` or the self-closing `<name .../>`.
    ///
    /// A self-closing start tag is never followed by [`Event::EndElement`]
    /// on its own account — [`Reader`] synthesizes one immediately, so
    /// every [`Event::StartElement`] a caller sees has a matching
    /// [`Event::EndElement`] later in the stream regardless of which XML
    /// spelling produced it. `self_closing` is carried only as information
    /// for a caller that cares which spelling was used (none of this
    /// crate's own [`crate::parse`] does).
    StartElement {
        /// The element's tag name.
        name: &'a str,
        /// This element's attributes, in document order.
        attributes: Vec<Attribute<'a>>,
        /// `true` if the source spelled this `<name .../>` rather than
        /// `<name ...>`.
        self_closing: bool,
        /// The span of the whole opening tag.
        span: Span,
    },
    /// `</name>` — real or, for a self-closing element, synthesized by
    /// [`Reader`] (see [`Event::StartElement`]'s docs). A synthesized one
    /// carries the *opening* tag's own span, since there is no closing `</`
    /// in the source to point at.
    EndElement {
        /// The element's tag name (matches the corresponding
        /// [`Event::StartElement::name`]).
        name: &'a str,
        /// The span of the closing tag, or of the opening tag if this was
        /// synthesized for a self-closing element.
        span: Span,
    },
    /// A run of character content between two markup constructs, entity-
    /// decoded and end-of-line-normalized.
    Text {
        /// The decoded text.
        content: Cow<'a, str>,
        /// The span this content occupied in the source.
        span: Span,
    },
    /// `<![CDATA[ ... ]]>` — returned verbatim (see this module's docs on
    /// why CDATA/comment/PI content is never rewritten).
    CData {
        /// The raw content between `<![CDATA[` and `]]>`.
        content: &'a str,
        /// The span of the whole section, delimiters included.
        span: Span,
    },
    /// `<!-- ... -->` — returned verbatim.
    Comment {
        /// The raw content between `<!--` and `-->`.
        content: &'a str,
        /// The span of the whole comment, delimiters included.
        span: Span,
    },
    /// `<?target content?>` — e.g. the `<?xml version="1.0"?>` prolog.
    /// Returned verbatim; [`crate::parse`] ignores every one of these
    /// (URDF assigns no meaning to any processing instruction).
    ProcessingInstruction {
        /// The PI's target name (`"xml"` for the XML declaration).
        target: &'a str,
        /// The raw content between the target and `?>`.
        content: &'a str,
        /// The span of the whole instruction, delimiters included.
        span: Span,
    },
    /// The end of the input, once the single root element has closed.
    Eof {
        /// A point span at the end of the input.
        span: Span,
    },
}

/// A streaming XML reader over a borrowed `&str`.
///
/// See the [module documentation](self) for the supported grammar and this
/// parser's deliberate simplifications.
#[derive(Debug)]
pub struct Reader<'a> {
    input: &'a str,
    pos: usize,
    position: Position,
    limits: Limits,
    /// Currently-open (non-self-closing) elements: name plus the span of
    /// their own opening tag, used to build [`XmlErrorKind::MismatchedClosingTag`]
    /// and [`XmlErrorKind::UnclosedElement`] messages. `stack.len()` doubles
    /// as the current nesting depth.
    stack: Vec<(&'a str, Span)>,
    /// The synthesized [`Event::EndElement`] queued right after a
    /// self-closing [`Event::StartElement`] — see that variant's docs.
    pending: Option<Event<'a>>,
    /// Set once the outermost element's matching close (real or
    /// synthesized) has been seen. Together with `stack.is_empty()`, this
    /// is the whole "have we seen the root yet / has it already closed"
    /// state machine [`Reader::check_root_position`] and
    /// [`Reader::scan_start_tag`] consult.
    root_closed: bool,
    size_checked: bool,
}

impl<'a> Reader<'a> {
    /// A new reader over `input`, with [`Limits::default`].
    #[must_use]
    pub fn new(input: &'a str) -> Self {
        Self::with_limits(input, Limits::default())
    }

    /// A new reader over `input`, under explicit [`Limits`].
    #[must_use]
    pub fn with_limits(input: &'a str, limits: Limits) -> Self {
        Self {
            input,
            pos: 0,
            position: Position::start(),
            limits,
            stack: Vec::new(),
            pending: None,
            root_closed: false,
            size_checked: false,
        }
    }

    /// A new reader over `bytes`, first validating them as UTF-8.
    ///
    /// # Errors
    ///
    /// [`XmlErrorKind::InvalidUtf8`] if `bytes` is not well-formed UTF-8.
    pub fn from_utf8_bytes(bytes: &'a [u8]) -> Result<Self, XmlError> {
        std::str::from_utf8(bytes)
            .map(Self::new)
            .map_err(|_| XmlError::new(XmlErrorKind::InvalidUtf8, Position::start()))
    }

    /// This reader's current position — where the *next* [`Event`] will
    /// start.
    #[must_use]
    pub const fn position(&self) -> Position {
        self.position
    }

    /// This reader's current element nesting depth (0 at the very start,
    /// and again once the root element has closed).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Reads and returns the next [`Event`].
    ///
    /// Returns [`Event::Eof`] exactly once, when the root element has
    /// closed and only trailing whitespace/comments/processing instructions
    /// remain; every call after that also returns [`Event::Eof`] again
    /// (the reader does not advance further once it has nothing left to
    /// say) rather than erroring, so a caller's `loop { ... }` need not
    /// special-case "already at EOF."
    ///
    /// # Errors
    ///
    /// An [`XmlError`] the moment the input stops being well-formed XML —
    /// see [`XmlErrorKind`] for the full taxonomy. The reader's position
    /// after an error is unspecified; discard it rather than calling
    /// [`Reader::next_event`] again.
    pub fn next_event(&mut self) -> Result<Event<'a>, XmlError> {
        if !self.size_checked {
            self.size_checked = true;
            if self.input.len() > self.limits.max_input_bytes {
                return Err(XmlError::new(
                    XmlErrorKind::InputTooLarge {
                        limit: self.limits.max_input_bytes,
                        actual: self.input.len(),
                    },
                    Position::start(),
                ));
            }
        }
        if let Some(event) = self.pending.take() {
            return Ok(event);
        }
        loop {
            if let Some(event) = self.scan_one()? {
                return Ok(event);
            }
        }
    }

    // -----------------------------------------------------------------
    // Cursor primitives
    // -----------------------------------------------------------------

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn starts_with(&self, needle: &str) -> bool {
        self.rest().starts_with(needle)
    }

    /// Consumes and returns exactly one *logical* (end-of-line-normalized)
    /// character: a source `"\r\n"` pair or a lone `"\r"` both consume their
    /// full raw extent but report as a single `'\n'`, so every other part of
    /// this reader can treat `'\n'` as the only line-break character that
    /// exists. Returns `None` (consuming nothing) at the end of input.
    fn bump(&mut self) -> Option<char> {
        let first = self.rest().chars().next()?;
        if first == '\r' {
            let cr_len = first.len_utf8();
            let after = &self.input[self.pos + cr_len..];
            let total = if after.starts_with('\n') {
                cr_len + '\n'.len_utf8()
            } else {
                cr_len
            };
            self.pos += total;
            self.position.advance('\n', total);
            return Some('\n');
        }
        let len = first.len_utf8();
        self.pos += len;
        self.position.advance(first, len);
        Some(first)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if chars::is_xml_whitespace(c) {
                self.bump();
            } else {
                break;
            }
        }
    }

    // -----------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------

    /// Scans exactly one thing from the input, returning `Ok(None)` only
    /// for a construct that never surfaces as an [`Event`] on its own (a
    /// `<!DOCTYPE ...>` — see this module's docs) — [`Reader::next_event`]
    /// loops on that until it gets a real event, so this never recurses.
    fn scan_one(&mut self) -> Result<Option<Event<'a>>, XmlError> {
        let Some(c) = self.peek() else {
            return self.finish_at_eof().map(Some);
        };
        if c != '<' {
            return self.scan_text().map(Some);
        }
        if self.starts_with("<!--") {
            return self.scan_comment().map(Some);
        }
        if self.starts_with("<![CDATA[") {
            return self.scan_cdata().map(Some);
        }
        if self.starts_with("<!DOCTYPE") {
            self.skip_doctype()?;
            return Ok(None);
        }
        if self.starts_with("<?") {
            return self.scan_processing_instruction().map(Some);
        }
        if self.starts_with("</") {
            return self.scan_end_tag().map(Some);
        }
        self.scan_start_tag().map(Some)
    }

    fn finish_at_eof(&self) -> Result<Event<'a>, XmlError> {
        if let Some((name, _)) = self.stack.last() {
            return Err(XmlError::new(
                XmlErrorKind::UnclosedElement {
                    name: (*name).to_owned(),
                },
                self.position,
            ));
        }
        if !self.root_closed {
            return Err(XmlError::new(
                XmlErrorKind::MissingRootElement,
                self.position,
            ));
        }
        Ok(Event::Eof {
            span: Span::point(self.position),
        })
    }

    /// The shared "is this content allowed here" check for [`Event::Text`]
    /// and [`Event::CData`]: whitespace is always fine, and non-whitespace
    /// is fine exactly when some element is currently open (`!stack.is_empty()`)
    /// — otherwise it is either before the root ([`XmlErrorKind::ContentBeforeRoot`])
    /// or after it already closed ([`XmlErrorKind::ContentAfterRoot`]).
    fn check_root_position(&self, text: &str, span: Span) -> Result<(), XmlError> {
        if text.chars().all(chars::is_xml_whitespace) || !self.stack.is_empty() {
            return Ok(());
        }
        let kind = if self.root_closed {
            XmlErrorKind::ContentAfterRoot
        } else {
            XmlErrorKind::ContentBeforeRoot
        };
        Err(XmlError::new(kind, span))
    }

    // -----------------------------------------------------------------
    // Names, references
    // -----------------------------------------------------------------

    fn scan_name(&mut self) -> Result<(&'a str, Span), XmlError> {
        let start_pos = self.position;
        let start_off = self.pos;
        match self.peek() {
            Some(c) if chars::is_name_start_char(c) => {}
            _ => return Err(XmlError::new(XmlErrorKind::InvalidName, start_pos)),
        }
        while let Some(c) = self.peek() {
            if chars::is_name_char(c) {
                self.bump();
            } else {
                break;
            }
        }
        let end_off = self.pos;
        Ok((
            &self.input[start_off..end_off],
            Span::new(start_pos, self.position),
        ))
    }

    /// Decodes one `&...;` reference — the cursor must sit exactly at the
    /// `&`, not yet consumed. Handles the five predefined entities and
    /// decimal/hexadecimal numeric character references.
    fn scan_reference(&mut self) -> Result<char, XmlError> {
        const MAX_ENTITY_NAME_LEN: usize = 16;
        let start_pos = self.position;
        self.bump(); // '&'

        if self.peek() == Some('#') {
            self.bump();
            let hex = self.peek() == Some('x');
            if hex {
                self.bump();
            }
            let digits_start = self.pos;
            loop {
                let is_digit = match self.peek() {
                    Some(c) if hex => c.is_ascii_hexdigit(),
                    Some(c) => c.is_ascii_digit(),
                    None => false,
                };
                if !is_digit {
                    break;
                }
                self.bump();
            }
            let digits = &self.input[digits_start..self.pos];
            let malformed_text = format!("#{}{}", if hex { "x" } else { "" }, digits);
            if digits.is_empty() || self.peek() != Some(';') {
                return Err(XmlError::new(
                    XmlErrorKind::MalformedReference {
                        text: malformed_text,
                    },
                    Span::new(start_pos, self.position),
                ));
            }
            self.bump(); // ';'
            let radix = if hex { 16 } else { 10 };
            let invalid = |reader: &Self| {
                XmlError::new(
                    XmlErrorKind::InvalidCharacterReference {
                        text: digits.to_owned(),
                    },
                    Span::new(start_pos, reader.position),
                )
            };
            let value = u32::from_str_radix(digits, radix).map_err(|_| invalid(self))?;
            return char::from_u32(value)
                .filter(|&c| chars::is_legal_xml_char(c))
                .ok_or_else(|| invalid(self));
        }

        let name_start = self.pos;
        loop {
            match self.peek() {
                Some(';') => break,
                Some(c)
                    if c.is_ascii_alphanumeric() && self.pos - name_start < MAX_ENTITY_NAME_LEN =>
                {
                    self.bump();
                }
                _ => {
                    let text = self.input[name_start..self.pos].to_owned();
                    return Err(XmlError::new(
                        XmlErrorKind::MalformedReference { text },
                        Span::new(start_pos, self.position),
                    ));
                }
            }
        }
        let name = &self.input[name_start..self.pos];
        self.bump(); // ';'
        chars::resolve_named_entity(name).ok_or_else(|| {
            XmlError::new(
                XmlErrorKind::UnknownEntity {
                    name: name.to_owned(),
                },
                Span::new(start_pos, self.position),
            )
        })
    }

    // -----------------------------------------------------------------
    // Text, CDATA, comments, processing instructions, DOCTYPE
    // -----------------------------------------------------------------

    fn scan_text(&mut self) -> Result<Event<'a>, XmlError> {
        let start_pos = self.position;
        let start_off = self.pos;
        let mut decoded: Option<String> = None;
        loop {
            match self.peek() {
                None | Some('<') => break,
                Some('&') => {
                    let buf =
                        decoded.get_or_insert_with(|| self.input[start_off..self.pos].to_owned());
                    let ch = self.scan_reference()?;
                    buf.push(ch);
                }
                Some('\r') => {
                    let buf =
                        decoded.get_or_insert_with(|| self.input[start_off..self.pos].to_owned());
                    self.bump();
                    buf.push('\n');
                }
                Some(_) => match decoded.as_mut() {
                    Some(buf) => {
                        if let Some(c) = self.bump() {
                            buf.push(c);
                        }
                    }
                    None => {
                        self.bump();
                    }
                },
            }
        }
        let end_off = self.pos;
        let span = Span::new(start_pos, self.position);
        let content: Cow<'a, str> = match decoded {
            Some(s) => Cow::Owned(s),
            None => Cow::Borrowed(&self.input[start_off..end_off]),
        };
        self.check_root_position(&content, span)?;
        Ok(Event::Text { content, span })
    }

    fn scan_comment(&mut self) -> Result<Event<'a>, XmlError> {
        let start_pos = self.position;
        for _ in 0..4 {
            self.bump(); // "<!--"
        }
        let content_start = self.pos;
        while !self.starts_with("-->") {
            if self.bump().is_none() {
                return Err(XmlError::new(XmlErrorKind::UnterminatedComment, start_pos));
            }
        }
        let content = &self.input[content_start..self.pos];
        for _ in 0..3 {
            self.bump(); // "-->"
        }
        Ok(Event::Comment {
            content,
            span: Span::new(start_pos, self.position),
        })
    }

    fn scan_cdata(&mut self) -> Result<Event<'a>, XmlError> {
        let start_pos = self.position;
        for _ in 0..9 {
            self.bump(); // "<![CDATA["
        }
        let content_start = self.pos;
        while !self.starts_with("]]>") {
            if self.bump().is_none() {
                return Err(XmlError::new(XmlErrorKind::UnterminatedCData, start_pos));
            }
        }
        let content = &self.input[content_start..self.pos];
        for _ in 0..3 {
            self.bump(); // "]]>"
        }
        let span = Span::new(start_pos, self.position);
        self.check_root_position(content, span)?;
        Ok(Event::CData { content, span })
    }

    fn scan_processing_instruction(&mut self) -> Result<Event<'a>, XmlError> {
        let start_pos = self.position;
        self.bump();
        self.bump(); // "<?"
        let (target, _) = self.scan_name()?;
        self.skip_whitespace();
        let content_start = self.pos;
        while !self.starts_with("?>") {
            if self.bump().is_none() {
                return Err(XmlError::new(
                    XmlErrorKind::UnterminatedProcessingInstruction,
                    start_pos,
                ));
            }
        }
        let content = &self.input[content_start..self.pos];
        self.bump();
        self.bump(); // "?>"
        Ok(Event::ProcessingInstruction {
            target,
            content,
            span: Span::new(start_pos, self.position),
        })
    }

    /// Best-effort skip of a `<!DOCTYPE ...>` declaration, tolerating a
    /// bracketed internal subset (`[ ... ]`) that may itself contain `>`
    /// characters (e.g. inside an `<!ENTITY ...>` declaration) — see this
    /// module's docs on why full DTD parsing is out of scope.
    fn skip_doctype(&mut self) -> Result<(), XmlError> {
        let start_pos = self.position;
        for _ in 0..9 {
            self.bump(); // "<!DOCTYPE"
        }
        let mut bracket_depth: u32 = 0;
        loop {
            match self.peek() {
                None => return Err(XmlError::new(XmlErrorKind::UnterminatedDoctype, start_pos)),
                Some('[') => {
                    bracket_depth += 1;
                    self.bump();
                }
                Some(']') => {
                    bracket_depth = bracket_depth.saturating_sub(1);
                    self.bump();
                }
                Some('>') if bracket_depth == 0 => {
                    self.bump();
                    return Ok(());
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Elements
    // -----------------------------------------------------------------

    fn scan_end_tag(&mut self) -> Result<Event<'a>, XmlError> {
        let start_pos = self.position;
        self.bump();
        self.bump(); // "</"
        let (name, _) = self.scan_name()?;
        self.skip_whitespace();
        match self.peek() {
            Some('>') => {
                self.bump();
            }
            Some(found) => {
                return Err(XmlError::new(
                    XmlErrorKind::UnexpectedCharacter { found },
                    self.position,
                ));
            }
            None => return Err(XmlError::new(XmlErrorKind::UnterminatedTag, start_pos)),
        }
        let span = Span::new(start_pos, self.position);
        match self.stack.pop() {
            None => Err(XmlError::new(
                XmlErrorKind::UnexpectedClosingTag {
                    found: name.to_owned(),
                },
                span,
            )),
            Some((expected, _)) if expected == name => {
                if self.stack.is_empty() {
                    self.root_closed = true;
                }
                Ok(Event::EndElement { name, span })
            }
            Some((expected, _)) => Err(XmlError::new(
                XmlErrorKind::MismatchedClosingTag {
                    expected: expected.to_owned(),
                    found: name.to_owned(),
                },
                span,
            )),
        }
    }

    fn scan_start_tag(&mut self) -> Result<Event<'a>, XmlError> {
        let tag_start_pos = self.position;
        let was_root_position = self.stack.is_empty();
        if was_root_position && self.root_closed {
            return Err(XmlError::new(XmlErrorKind::ContentAfterRoot, tag_start_pos));
        }

        self.bump(); // '<'
        let (name, _) = self.scan_name()?;
        let mut attributes: Vec<Attribute<'a>> = Vec::new();
        let self_closing;
        loop {
            self.skip_whitespace();
            match self.peek() {
                None => return Err(XmlError::new(XmlErrorKind::UnterminatedTag, tag_start_pos)),
                Some('/') => {
                    self.bump();
                    match self.peek() {
                        Some('>') => {
                            self.bump();
                            self_closing = true;
                            break;
                        }
                        Some(found) => {
                            return Err(XmlError::new(
                                XmlErrorKind::UnexpectedCharacter { found },
                                self.position,
                            ));
                        }
                        None => {
                            return Err(XmlError::new(
                                XmlErrorKind::UnterminatedTag,
                                tag_start_pos,
                            ));
                        }
                    }
                }
                Some('>') => {
                    self.bump();
                    self_closing = false;
                    break;
                }
                Some(c) if chars::is_name_start_char(c) => {
                    let attribute = self.scan_attribute()?;
                    if attributes.iter().any(|a| a.name == attribute.name) {
                        return Err(XmlError::new(
                            XmlErrorKind::DuplicateAttribute {
                                name: attribute.name.to_owned(),
                            },
                            attribute.name_span,
                        ));
                    }
                    attributes.push(attribute);
                }
                Some(found) => {
                    return Err(XmlError::new(
                        XmlErrorKind::UnexpectedCharacter { found },
                        self.position,
                    ));
                }
            }
        }

        let span = Span::new(tag_start_pos, self.position);
        if self_closing {
            if was_root_position {
                self.root_closed = true;
            }
            self.pending = Some(Event::EndElement { name, span });
        } else {
            if self.stack.len() >= self.limits.max_depth {
                return Err(XmlError::new(
                    XmlErrorKind::DepthLimitExceeded {
                        limit: self.limits.max_depth,
                    },
                    span,
                ));
            }
            self.stack.push((name, span));
        }
        Ok(Event::StartElement {
            name,
            attributes,
            self_closing,
            span,
        })
    }

    fn scan_attribute(&mut self) -> Result<Attribute<'a>, XmlError> {
        let (name, name_span) = self.scan_name()?;
        self.skip_whitespace();
        match self.peek() {
            Some('=') => {
                self.bump();
            }
            _ => {
                return Err(XmlError::new(
                    XmlErrorKind::MissingAttributeEquals {
                        name: name.to_owned(),
                    },
                    name_span,
                ));
            }
        }
        self.skip_whitespace();
        let quote_pos = self.position;
        let quote = match self.peek() {
            Some(q @ ('"' | '\'')) => q,
            _ => {
                return Err(XmlError::new(
                    XmlErrorKind::MissingAttributeQuote,
                    quote_pos,
                ));
            }
        };
        self.bump(); // opening quote
        let (value, value_span) = self.scan_attribute_value(quote, quote_pos)?;
        Ok(Attribute {
            name,
            value,
            name_span,
            value_span,
        })
    }

    /// Scans an attribute value's content, with the cursor just past the
    /// opening `quote` — consumes through (and including) the matching
    /// closing quote.
    fn scan_attribute_value(
        &mut self,
        quote: char,
        start_pos: Position,
    ) -> Result<(Cow<'a, str>, Span), XmlError> {
        let content_start = self.pos;
        let mut decoded: Option<String> = None;
        loop {
            match self.peek() {
                None => {
                    return Err(XmlError::new(
                        XmlErrorKind::UnterminatedAttributeValue,
                        start_pos,
                    ));
                }
                Some(c) if c == quote => break,
                Some('<') => {
                    return Err(XmlError::new(
                        XmlErrorKind::UnexpectedCharacter { found: '<' },
                        self.position,
                    ));
                }
                Some('&') => {
                    let buf = decoded
                        .get_or_insert_with(|| self.input[content_start..self.pos].to_owned());
                    let ch = self.scan_reference()?;
                    buf.push(ch);
                }
                Some('\t' | '\r' | '\n') => {
                    let buf = decoded
                        .get_or_insert_with(|| self.input[content_start..self.pos].to_owned());
                    self.bump();
                    buf.push(' ');
                }
                Some(_) => match decoded.as_mut() {
                    Some(buf) => {
                        if let Some(c) = self.bump() {
                            buf.push(c);
                        }
                    }
                    None => {
                        self.bump();
                    }
                },
            }
        }
        let content_end = self.pos;
        self.bump(); // closing quote
        let value = match decoded {
            Some(s) => Cow::Owned(s),
            None => Cow::Borrowed(&self.input[content_start..content_end]),
        };
        Ok((value, Span::new(start_pos, self.position)))
    }
}
