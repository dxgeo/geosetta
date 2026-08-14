//! A small, from-scratch XML reader, scoped to what the KML spoke needs — not
//! a general-purpose XML library. See `plans/kml.org` for the fuller design
//! writeup this implements.
//!
//! Well-formed documents only: no DTD processing and no external entities (a
//! deliberate non-goal — entity expansion is a well-known XML attack surface,
//! and KML never needs either; a `<!DOCTYPE ...>`, if present, is skipped
//! rather than interpreted). Namespaces are handled by ignoring prefixes and
//! matching local element/attribute names only, since KML/`gx` producers are
//! never ambiguous about which element is meant once the prefix is stripped.
//!
use crate::error::{Error, Result};

/// A parsed XML element: a name, its attributes, child elements, and the text
/// found directly inside it. A child element's own text lives on the child,
/// not here — mixed-content ordering between text runs and child elements
/// isn't preserved (all direct text runs are concatenated in document order),
/// since nothing this module feeds needs it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct XmlElement {
    /// Local name (any namespace prefix already stripped).
    pub name: String,
    /// Local name (any namespace prefix already stripped) -> value, in
    /// document order.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<XmlElement>,
    pub text: String,
}

impl XmlElement {
    /// An attribute's value by local name. First match wins.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The first child with the given local name.
    pub fn child(&self, name: &str) -> Option<&XmlElement> {
        self.children.iter().find(|c| c.name == name)
    }

    /// All children with the given local name, in document order.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlElement> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// This element's direct text, trimmed of leading/trailing whitespace —
    /// the form KML's leaf text elements (`<name>`, `<coordinates>`, ...) want.
    pub fn text_trimmed(&self) -> &str {
        self.text.trim()
    }

    /// A leaf element with only text content, e.g. `<name>A Point</name>`.
    pub fn leaf(name: &str, text: impl Into<String>) -> XmlElement {
        XmlElement { name: name.to_string(), attrs: Vec::new(), children: Vec::new(), text: text.into() }
    }

    /// An element made up of child elements, with no direct text of its own.
    pub fn with_children(name: &str, children: Vec<XmlElement>) -> XmlElement {
        XmlElement { name: name.to_string(), attrs: Vec::new(), children, text: String::new() }
    }

    /// Append one attribute (builder-style, for chaining onto [`XmlElement::leaf`]
    /// / [`XmlElement::with_children`]).
    pub fn with_attr(mut self, name: &str, value: impl Into<String>) -> XmlElement {
        self.attrs.push((name.to_string(), value.into()));
        self
    }
}

/// Parse a complete XML document, returning its root element. A leading BOM,
/// XML declaration (`<?xml ... ?>`), comments, and a `<!DOCTYPE ...>` are
/// skipped before and after the root element; anything else trailing the root
/// element is rejected.
pub(crate) fn parse(input: &str) -> Result<XmlElement> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut p = Parser::new(input);
    p.skip_misc()?;
    let root = p.parse_element()?;
    p.skip_misc()?;
    if p.pos != p.bytes.len() {
        return Err(p.err("unexpected trailing content after root element"));
    }
    Ok(root)
}

/// Serialize an element tree back to a complete XML document: an `<?xml?>`
/// declaration followed by the element, with text and attribute content
/// escaped. The inverse of [`parse`] (modulo the information [`XmlElement`]
/// doesn't keep — comments, processing instructions, mixed-content ordering).
pub(crate) fn write(root: &XmlElement) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    write_element(root, &mut out);
    out
}

fn write_element(el: &XmlElement, out: &mut String) {
    out.push('<');
    out.push_str(&el.name);
    for (k, v) in &el.attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        escape_into(v, out, true);
        out.push('"');
    }
    if el.children.is_empty() && el.text.is_empty() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    escape_into(&el.text, out, false);
    for child in &el.children {
        write_element(child, out);
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
}

/// Escape `&`/`<`/`>` (plus `"` when `in_attr`) into `out`. Bulk-copies runs
/// of ordinary bytes with one `push_str` and only stops on a byte that must
/// be escaped, mirroring `json::value::write_json_string` — every
/// escape-triggering byte here is ASCII, so run boundaries always fall on
/// char boundaries and each slice is valid UTF-8.
fn escape_into(s: &str, out: &mut String, in_attr: bool) {
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc = match b {
            b'&' => Some("&amp;"),
            b'<' => Some("&lt;"),
            b'>' => Some("&gt;"),
            b'"' if in_attr => Some("&quot;"),
            _ => None,
        };
        if let Some(esc) = esc {
            if start < i {
                out.push_str(&s[start..i]);
            }
            out.push_str(esc);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push_str(&s[start..]);
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser { bytes: input.as_bytes(), pos: 0 }
    }

    fn err(&self, message: &str) -> Error {
        Error::Convert(format!("xml: {message} (at byte {})", self.pos))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn starts_with(&self, s: &str) -> bool {
        self.bytes[self.pos..].starts_with(s.as_bytes())
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    /// Skip whitespace, `<?...?>` processing instructions, `<!-- -->`
    /// comments, and a `<!DOCTYPE ...>` declaration — anything that can
    /// appear before or after the document's root element.
    fn skip_misc(&mut self) -> Result<()> {
        loop {
            self.skip_ws();
            if self.starts_with("<?") {
                self.skip_processing_instruction()?;
            } else if self.starts_with("<!--") {
                self.skip_comment()?;
            } else if self.starts_with("<!DOCTYPE") || self.starts_with("<!doctype") {
                self.skip_doctype()?;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn skip_processing_instruction(&mut self) -> Result<()> {
        self.pos += 2; // "<?"
        match self.find(b"?>") {
            Some(end) => self.pos = end + 2,
            None => return Err(self.err("unterminated processing instruction")),
        }
        Ok(())
    }

    fn skip_comment(&mut self) -> Result<()> {
        self.pos += 4; // "<!--"
        match self.find(b"-->") {
            Some(end) => self.pos = end + 3,
            None => return Err(self.err("unterminated comment")),
        }
        Ok(())
    }

    /// A `<!DOCTYPE ...>` declaration, with an optional bracketed internal
    /// subset (`[ ... ]`). KML never carries one; skipped rather than
    /// rejected in case some producer emits a harmless one anyway.
    fn skip_doctype(&mut self) -> Result<()> {
        self.pos += 2; // "<!"
        let mut depth = 0i32;
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated <!DOCTYPE ...>")),
                Some(b'[') => depth += 1,
                Some(b']') => depth -= 1,
                Some(b'>') if depth <= 0 => break,
                _ => {}
            }
        }
        Ok(())
    }

    fn find(&self, needle: &[u8]) -> Option<usize> {
        self.bytes[self.pos..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|i| self.pos + i)
    }

    fn parse_element(&mut self) -> Result<XmlElement> {
        self.expect(b'<')?;
        let raw_name = self.read_name()?;
        let name = local_name(&raw_name).to_string();
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'/') | Some(b'>') | None => break,
                _ => {
                    let raw_attr = self.read_name()?;
                    self.skip_ws();
                    self.expect(b'=')?;
                    self.skip_ws();
                    let value = self.parse_attr_value()?;
                    attrs.push((local_name(&raw_attr).to_string(), value));
                }
            }
        }
        if self.peek() == Some(b'/') {
            self.pos += 1;
            self.expect(b'>')?;
            return Ok(XmlElement { name, attrs, children: Vec::new(), text: String::new() });
        }
        self.expect(b'>')?;
        let (children, text) = self.parse_content(&name)?;
        Ok(XmlElement { name, attrs, children, text })
    }

    /// Parse an element's content up to (and consuming) its matching closing
    /// tag, whose local name must match `open_name`.
    fn parse_content(&mut self, open_name: &str) -> Result<(Vec<XmlElement>, String)> {
        let mut children = Vec::new();
        let mut text = String::new();
        loop {
            self.read_text_into(&mut text)?;
            match self.peek() {
                None => return Err(self.err("unexpected end of input inside element")),
                Some(b'<') if self.starts_with("</") => {
                    self.pos += 2;
                    let raw_name = self.read_name()?;
                    self.skip_ws();
                    self.expect(b'>')?;
                    if local_name(&raw_name) != open_name {
                        return Err(self.err("mismatched closing tag"));
                    }
                    break;
                }
                Some(b'<') if self.starts_with("<!--") => self.skip_comment()?,
                Some(b'<') if self.starts_with("<![CDATA[") => {
                    self.pos += 9;
                    match self.find(b"]]>") {
                        Some(end) => {
                            let s = std::str::from_utf8(&self.bytes[self.pos..end])
                                .map_err(|_| self.err("invalid utf-8 in CDATA section"))?;
                            text.push_str(s);
                            self.pos = end + 3;
                        }
                        None => return Err(self.err("unterminated CDATA section")),
                    }
                }
                Some(b'<') if self.starts_with("<?") => self.skip_processing_instruction()?,
                Some(b'<') => children.push(self.parse_element()?),
                Some(_) => unreachable!("read_text_into only stops at '<' or end of input"),
            }
        }
        Ok((children, text))
    }

    /// Bulk-scan text content up to the next `<`, decoding entity/character
    /// references along the way. Mirrors `json::Parser::parse_string`'s
    /// run-scan-then-push-str shape.
    fn read_text_into(&mut self, out: &mut String) -> Result<()> {
        loop {
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'<' || b == b'&' {
                    break;
                }
                self.pos += 1;
            }
            if self.pos > start {
                let s = std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| self.err("invalid utf-8"))?;
                out.push_str(s);
            }
            match self.peek() {
                Some(b'&') => {
                    self.pos += 1;
                    let c = self.read_entity()?;
                    out.push(c);
                }
                _ => return Ok(()),
            }
        }
    }

    fn parse_attr_value(&mut self) -> Result<String> {
        let quote = match self.bump() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.err("expected quoted attribute value")),
        };
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == quote || b == b'&' || b == b'<' {
                    break;
                }
                self.pos += 1;
            }
            if self.pos > start {
                let s = std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| self.err("invalid utf-8"))?;
                out.push_str(s);
            }
            match self.bump() {
                Some(b) if b == quote => break,
                Some(b'&') => out.push(self.read_entity()?),
                _ => return Err(self.err("unterminated or invalid attribute value")),
            }
        }
        Ok(out)
    }

    /// The character denoted by an entity/character reference, with the
    /// leading `&` already consumed: a predefined entity (`amp`/`lt`/`gt`/
    /// `quot`/`apos`) or a numeric character reference (`#NNN` / `#xHHHH`),
    /// terminated by `;`.
    fn read_entity(&mut self) -> Result<char> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b';' {
                break;
            }
            if !(b.is_ascii_alphanumeric() || b == b'#') {
                return Err(self.err("malformed entity reference"));
            }
            self.pos += 1;
        }
        let name = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid utf-8 in entity reference"))?;
        if self.bump() != Some(b';') {
            return Err(self.err("unterminated entity reference"));
        }
        let cp = match name {
            "amp" => return Ok('&'),
            "lt" => return Ok('<'),
            "gt" => return Ok('>'),
            "quot" => return Ok('"'),
            "apos" => return Ok('\''),
            _ if name.starts_with("#x") || name.starts_with("#X") => {
                u32::from_str_radix(&name[2..], 16)
                    .map_err(|_| self.err("invalid numeric character reference"))?
            }
            _ if name.starts_with('#') => name[1..]
                .parse::<u32>()
                .map_err(|_| self.err("invalid numeric character reference"))?,
            _ => return Err(self.err(&format!("unknown entity \"&{name};\""))),
        };
        char::from_u32(cp).ok_or_else(|| self.err("invalid numeric character reference"))
    }

    /// An element or attribute name (namespace prefix included; callers strip
    /// it via [`local_name`]). Permissive rather than spec-exact: anything up
    /// to the next whitespace, `/`, `>`, or `=`.
    fn read_name(&mut self) -> Result<String> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() || matches!(b, b'/' | b'>' | b'=') {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err(self.err("expected a name"));
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .map(str::to_string)
            .map_err(|_| self.err("invalid utf-8 in name"))
    }

    fn expect(&mut self, expected: u8) -> Result<()> {
        match self.peek() {
            Some(b) if b == expected => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.err(&format!("expected '{}'", expected as char))),
        }
    }
}

/// Strip a namespace prefix (`prefix:local` -> `local`); elements/attributes
/// are matched on local name only (see the module doc).
fn local_name(raw: &str) -> &str {
    raw.split_once(':').map(|(_, local)| local).unwrap_or(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_elements_with_attrs_and_text() {
        let root = parse(
            r#"<kml><Placemark id="p1"><name>A Point</name><Point><coordinates>1,2,3</coordinates></Point></Placemark></kml>"#,
        )
        .unwrap();
        assert_eq!(root.name, "kml");
        let placemark = root.child("Placemark").unwrap();
        assert_eq!(placemark.attr("id"), Some("p1"));
        assert_eq!(placemark.child("name").unwrap().text_trimmed(), "A Point");
        let point = placemark.child("Point").unwrap();
        assert_eq!(point.child("coordinates").unwrap().text_trimmed(), "1,2,3");
    }

    #[test]
    fn handles_self_closing_and_multiple_attrs() {
        let root = parse(r#"<a x="1" y="2"/>"#).unwrap();
        assert_eq!(root.name, "a");
        assert_eq!(root.attr("x"), Some("1"));
        assert_eq!(root.attr("y"), Some("2"));
        assert!(root.children.is_empty());
        assert!(root.text.is_empty());
    }

    #[test]
    fn skips_prolog_comments_and_doctype() {
        let root = parse(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE kml [ <!ENTITY foo "bar"> ]>
<!-- a comment --><kml><!-- inner --><a/></kml>
<!-- trailing --><?pi trailing?>"#,
        )
        .unwrap();
        assert_eq!(root.name, "kml");
        assert_eq!(root.children.len(), 1);
    }

    #[test]
    fn strips_bom() {
        let root = parse("\u{feff}<a/>").unwrap();
        assert_eq!(root.name, "a");
    }

    #[test]
    fn decodes_predefined_and_numeric_entities() {
        let root = parse("<a>&amp;&lt;&gt;&quot;&apos; &#65;&#x42;</a>").unwrap();
        assert_eq!(root.text, "&<>\"' AB");
    }

    #[test]
    fn rejects_unknown_entity() {
        assert!(parse("<a>&bogus;</a>").is_err());
    }

    #[test]
    fn decodes_entities_in_attribute_values() {
        let root = parse(r#"<a x="1 &amp; 2"/>"#).unwrap();
        assert_eq!(root.attr("x"), Some("1 & 2"));
    }

    #[test]
    fn reads_cdata_verbatim() {
        let root = parse("<a><![CDATA[<b>not a tag</b> & neither is this]]></a>").unwrap();
        assert_eq!(root.text, "<b>not a tag</b> & neither is this");
    }

    #[test]
    fn strips_namespace_prefixes_on_elements_and_attributes() {
        let root = parse(r#"<kml:kml xmlns:kml="http://www.opengis.net/kml/2.2"><kml:Placemark gx:id="1"/></kml:kml>"#).unwrap();
        assert_eq!(root.name, "kml");
        let placemark = root.child("Placemark").unwrap();
        assert_eq!(placemark.attr("id"), Some("1"));
    }

    #[test]
    fn concatenates_mixed_text_and_child_elements() {
        let root = parse("<a>one<b/>two<c/>three</a>").unwrap();
        assert_eq!(root.text, "onetwothree");
        assert_eq!(root.children.len(), 2);
    }

    #[test]
    fn rejects_mismatched_closing_tag() {
        assert!(parse("<a><b></c></a>").is_err());
    }

    #[test]
    fn rejects_unterminated_element() {
        assert!(parse("<a><b></b>").is_err());
        assert!(parse("<a>").is_err());
    }

    #[test]
    fn rejects_trailing_content_after_root() {
        assert!(parse("<a/><b/>").is_err());
    }

    #[test]
    fn children_named_filters_by_local_name() {
        let root = parse("<a><b/>text<b/><c/></a>").unwrap();
        assert_eq!(root.children_named("b").count(), 2);
    }

    #[test]
    fn writes_self_closing_for_empty_elements() {
        let el = XmlElement::with_children("a", vec![]).with_attr("x", "1");
        assert_eq!(write(&el), "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a x=\"1\"/>");
    }

    #[test]
    fn writes_leaf_text_and_nested_children() {
        let el = XmlElement::with_children(
            "a",
            vec![XmlElement::leaf("b", "hi"), XmlElement::with_children("c", vec![])],
        );
        assert_eq!(
            write(&el),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a><b>hi</b><c/></a>"
        );
    }

    #[test]
    fn escapes_special_characters_in_text_and_attributes() {
        let el = XmlElement::leaf("a", "1 < 2 & 3 > 0").with_attr("q", "say \"hi\" & bye");
        let out = write(&el);
        assert!(out.contains("1 &lt; 2 &amp; 3 &gt; 0"));
        assert!(out.contains("q=\"say &quot;hi&quot; &amp; bye\""));
    }

    #[test]
    fn write_then_parse_round_trips() {
        let original = XmlElement::with_children(
            "kml",
            vec![XmlElement::with_children(
                "Placemark",
                vec![
                    XmlElement::leaf("name", "Café <5> & \"friends\""),
                    XmlElement::with_children("Point", vec![XmlElement::leaf("coordinates", "1,2")]),
                ],
            )
            .with_attr("id", "p1")],
        );
        let bytes = write(&original);
        let reparsed = parse(&bytes).unwrap();
        assert_eq!(reparsed, original);
    }
}
