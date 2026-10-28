//! A small, from-scratch XML reader and writer, scoped to what the KML spoke
//! needs — not a general-purpose XML library. See `plans/kml.org` for the
//! original design and `plans/kml-standard.org` D-K6 for what it keeps.
//!
//! Well-formed documents only: no DTD processing and no external entities (a
//! deliberate non-goal — entity expansion is a well-known XML attack surface,
//! and KML never needs either; a `<!DOCTYPE ...>`, if present, is carried as
//! its bytes rather than interpreted).
//!
//! *Namespaces are resolved*, per Namespaces in XML 1.0: every element and
//! attribute carries its namespace URI beside its local name, with the prefix
//! it was written with kept so a writer can repeat it. Matching on local name
//! alone read an extension element named `Point` as a KML Point, which is a
//! correctness bug once foreign content is carried rather than dropped. A
//! prefix with no binding in scope is *not* an error — the element keeps its
//! prefix and gets no namespace, and the caller decides what that means
//! (libxml2 warns and carries on the same way).
//!
//! Text runs, comments and processing instructions are kept with their
//! position among an element's children, so the whitespace that lays a
//! document out is written back where it was (`plans/kml-layout.org` L1);
//! a CDATA section's characters join the text around it, since CDATA and
//! escaped text are the same characters (D-L1). The DOCTYPE is kept verbatim
//! and never interpreted (D-L6). Every parsed element records the byte span of
//! its source text, so a caller can carry content it does not model as the
//! source's own bytes (`plans/kml-standard.org` D-K4).
//!
//! Input decoding is [`decode`]'s job, ahead of parsing: a byte-order mark or
//! the XML declaration's `encoding` decides it, so a parser over `&str` never
//! has to guess.

use std::borrow::Cow;
use std::ops::Range;
use std::rc::Rc;

use crate::error::{Error, Result};

/// The namespace the `xml` prefix is permanently bound to.
pub(crate) const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// The namespace of every `xmlns` / `xmlns:p` declaration attribute.
pub(crate) const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// A parsed XML element: a name, its attributes, child elements, and the
/// text runs, comments and processing instructions among them. A child
/// element's own content lives on the child, not here.
#[derive(Debug, Clone)]
pub(crate) struct XmlElement {
    /// Local name, without any prefix.
    pub name: String,
    /// The prefix as written (`gx` in `<gx:Track>`), or `None` when unprefixed.
    pub prefix: Option<String>,
    /// The namespace URI the element is in, resolved from the declarations in
    /// scope; `None` for no namespace, including an unbound prefix.
    pub ns: Option<Rc<str>>,
    /// In document order, namespace declarations included (as attributes in
    /// [`XMLNS_NS`]), so a writer reproduces them where they were.
    pub attrs: Vec<XmlAttr>,
    pub children: Vec<XmlElement>,
    /// Text runs, comments and processing instructions, in document order,
    /// each tagged with the index of the child it precedes
    /// (`children.len()` for after the last one).
    pub content: Vec<(usize, Content)>,
    /// The element's source text, `<` to the final `>`, as byte offsets into
    /// the parsed `&str`. `None` for an element built rather than parsed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub span: Option<Range<usize>>,
}

/// Content equality: everything but [`XmlElement::span`], which is where the
/// element came from rather than what it is — a parsed tree and the same tree
/// built by hand are equal.
impl PartialEq for XmlElement {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.prefix == other.prefix
            && self.ns == other.ns
            && self.attrs == other.attrs
            && self.children == other.children
            && self.content == other.content
    }
}

/// One attribute, with the same three-part name as [`XmlElement`]. An
/// unprefixed attribute is in *no* namespace, whatever the default namespace
/// is — Namespaces in XML 1.0 §6.2.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct XmlAttr {
    pub prefix: Option<String>,
    pub name: String,
    pub ns: Option<Rc<str>>,
    pub value: String,
}

/// A node that is not an element.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Content {
    /// Character data, entity references decoded. Adjacent text and CDATA are
    /// one run. Shared, because the whitespace runs that indent a document
    /// repeat by the thousand (see [`Parser::run`]).
    Text(Rc<str>),
    /// The text between `<!--` and `-->`, verbatim.
    Comment(Box<str>),
    /// `<?target data?>`; `data` has its leading whitespace removed.
    Pi { target: Box<str>, data: Box<str> },
    /// A `<!DOCTYPE ...>` declaration, `<!` to `>`, verbatim and never
    /// interpreted (`plans/kml-layout.org` D-L6). Only before the root.
    Doctype(Box<str>),
}

impl Content {
    /// A text run that is all whitespace — the layout between elements.
    pub fn is_space(&self) -> bool {
        matches!(self, Content::Text(t) if t.bytes().all(is_space))
    }

    /// A text run's text.
    pub fn text(&self) -> Option<&str> {
        match self {
            Content::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// XML's whitespace (§2.3 `S`).
pub(crate) fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// A whole document: the root element plus the whitespace, comments,
/// processing instructions and DOCTYPE on either side of it. The XML
/// declaration is not kept — the writer emits its own, and the whitespace
/// straight after it is the writer's too (`plans/kml-layout.org` tier 3).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct XmlDocument {
    pub before: Vec<Content>,
    pub root: XmlElement,
    pub after: Vec<Content>,
}

// The query half is what a parsed tree is read with — by the tests, since the
// KML reader parses into its own model; the builder half is the KML writer's.
#[cfg_attr(not(test), allow(dead_code))]
impl XmlElement {
    /// An unprefixed attribute's value by local name — the form every KML
    /// attribute (`id`, `targetId`, `name`, `version`) takes. First match wins.
    /// A prefixed attribute is in a namespace and is not found here: use
    /// [`XmlElement::attr_ns`].
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.ns.is_none() && a.name == name).map(|a| a.value.as_str())
    }

    /// An attribute's value by namespace URI and local name.
    pub fn attr_ns(&self, ns: &str, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.ns.as_deref() == Some(ns) && a.name == name)
            .map(|a| a.value.as_str())
    }

    /// Whether this element is in namespace `ns`.
    pub fn in_ns(&self, ns: &str) -> bool {
        self.ns.as_deref() == Some(ns)
    }

    /// The name as written: `prefix:local`, or `local` alone.
    pub fn qualified_name(&self) -> Cow<'_, str> {
        qualify(self.prefix.as_deref(), &self.name)
    }

    /// A leaf's text: its first text run, which for an element with no
    /// child elements is all of it ([`merge_leaf_text`]).
    pub fn text(&self) -> &str {
        first_text(&self.content)
    }

    /// This element's direct text, trimmed of leading/trailing whitespace —
    /// the form KML's leaf text elements (`<name>`, `<coordinates>`, ...) want.
    #[cfg(test)]
    pub fn text_trimmed(&self) -> &str {
        self.text().trim()
    }

    /// A leaf element with only text content, e.g. `<name>A Point</name>`.
    pub fn leaf(name: &str, text: impl Into<String>) -> XmlElement {
        let text: String = text.into();
        let content = if text.is_empty() { Vec::new() } else { vec![(0, Content::Text(Rc::from(text)))] };
        XmlElement::built(name, Vec::new(), content)
    }

    /// An element made up of child elements, with no direct text of its own.
    pub fn with_children(name: &str, children: Vec<XmlElement>) -> XmlElement {
        XmlElement::built(name, children, Vec::new())
    }

    fn built(name: &str, children: Vec<XmlElement>, content: Vec<(usize, Content)>) -> XmlElement {
        XmlElement { name: name.to_string(), prefix: None, ns: None, attrs: Vec::new(), children, content, span: None }
    }

    /// Append one unprefixed attribute (builder-style, for chaining onto
    /// [`XmlElement::leaf`] / [`XmlElement::with_children`]).
    pub fn with_attr(mut self, name: &str, value: impl Into<String>) -> XmlElement {
        self.attrs.push(XmlAttr { prefix: None, name: name.to_string(), ns: None, value: value.into() });
        self
    }

    /// Append a namespace declaration: `xmlns="uri"` for `prefix: None`,
    /// `xmlns:prefix="uri"` otherwise. It declares; it does not move this
    /// element or its children into the namespace — set [`XmlElement::ns`]
    /// for that.
    pub fn with_ns_decl(mut self, prefix: Option<&str>, uri: &str) -> XmlElement {
        let (attr_prefix, name) = match prefix {
            None => (None, "xmlns".to_string()),
            Some(p) => (Some("xmlns".to_string()), p.to_string()),
        };
        self.attrs.push(XmlAttr { prefix: attr_prefix, name, ns: Some(Rc::from(XMLNS_NS)), value: uri.to_string() });
        self
    }
}

/// The first text run in `content`, or `""`.
pub(crate) fn first_text(content: &[(usize, Content)]) -> &str {
    content
        .iter()
        .find_map(|(_, c)| match c {
            Content::Text(t) => Some(&**t),
            _ => None,
        })
        .unwrap_or("")
}

/// An element with no child elements has one text run, in the place of its
/// first: a leaf's text is a value, and a comment splitting it
/// (`<name>a<!--c-->b</name>`) does not split the value. The comment then
/// follows the text, which is where the writer put it before text had a
/// position.
pub(crate) fn merge_leaf_text(content: &mut Vec<(usize, Content)>) {
    if content.iter().filter(|(_, c)| matches!(c, Content::Text(_))).count() < 2 {
        return;
    }
    let mut text = String::new();
    let mut first = None;
    let mut i = 0;
    content.retain(|(_, c)| {
        let keep = match c {
            Content::Text(t) => {
                text.push_str(t);
                first.get_or_insert(i);
                false
            }
            _ => true,
        };
        i += usize::from(keep);
        keep
    });
    content.insert(first.unwrap_or(0), (0, Content::Text(Rc::from(text))));
}

/// A tree [`lay_out`] can indent: [`XmlElement`], and the KML model's node.
pub(crate) trait Tree: Sized {
    fn parts(&mut self) -> (&mut [Self], &mut Vec<(usize, Content)>);
    /// Whether the node is source bytes, whose inside is never re-indented.
    fn opaque(&self) -> bool {
        false
    }
}

impl Tree for XmlElement {
    fn parts(&mut self) -> (&mut [Self], &mut Vec<(usize, Content)>) {
        (&mut self.children, &mut self.content)
    }
}

/// Lay out `node`'s inside, one child element per line: each child on a new
/// line indented one `unit` past `indent`, and the end tag on a line of its
/// own at `indent` (`plans/kml-layout.org` D-L2, D-L3). `newline` is the
/// document's (`\n` or `\r\n`). An element with no child elements — a leaf —
/// keeps its text inline, and a node already holding whitespace runs is laid
/// out already and left as it is, so only what was built is touched.
pub(crate) fn lay_out<T: Tree>(node: &mut T, newline: &str, indent: &str, unit: &str) {
    if node.opaque() {
        return;
    }
    let (children, content) = node.parts();
    if children.is_empty() || content.iter().any(|(_, c)| c.is_space()) {
        return;
    }
    let inner = format!("{indent}{unit}");
    let lead: Rc<str> = Rc::from(format!("{newline}{inner}"));
    let mut laid = Vec::with_capacity(content.len() + children.len() + 1);
    let mut old = std::mem::take(content).into_iter().peekable();
    for i in 0..=children.len() {
        while let Some(c) = old.next_if(|(at, _)| *at <= i) {
            laid.push(c);
        }
        let run = if i < children.len() { lead.clone() } else { Rc::from(format!("{newline}{indent}")) };
        laid.push((i, Content::Text(run)));
    }
    *content = laid;
    for child in children.iter_mut() {
        lay_out(child, newline, &inner, unit);
    }
}

pub(crate) fn qualify<'a>(prefix: Option<&str>, name: &'a str) -> Cow<'a, str> {
    match prefix {
        Some(p) => Cow::Owned(format!("{p}:{name}")),
        None => Cow::Borrowed(name),
    }
}

/// Decode a document's bytes to text, by the rules of XML 1.0 Appendix F: a
/// byte-order mark wins; failing one, the first bytes' pattern identifies
/// UTF-16; failing that, the XML declaration's `encoding` names it, and with
/// no declaration the document is UTF-8.
///
/// Decoded exactly: UTF-8, UTF-16 (either byte order), US-ASCII and
/// ISO-8859-1. Any other declared encoding is accepted only when every byte is
/// ASCII — the bytes then mean the same thing under every encoding a
/// declaration could plausibly name — and refused otherwise, rather than
/// guessed at.
pub(crate) fn decode(bytes: &[u8]) -> Result<Cow<'_, str>> {
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => {
            return std::str::from_utf8(rest).map(Cow::Borrowed).map_err(|_| decode_err("input is not valid UTF-8"));
        }
        [0xFF, 0xFE, rest @ ..] => return utf16(rest, u16::from_le_bytes).map(Cow::Owned),
        [0xFE, 0xFF, rest @ ..] => return utf16(rest, u16::from_be_bytes).map(Cow::Owned),
        [0x3C, 0x00, 0x3F, 0x00, ..] => return utf16(bytes, u16::from_le_bytes).map(Cow::Owned),
        [0x00, 0x3C, 0x00, 0x3F, ..] => return utf16(bytes, u16::from_be_bytes).map(Cow::Owned),
        _ => {}
    }
    let declared = declared_encoding(bytes);
    let canonical = declared.map(|d| d.to_ascii_lowercase().replace(['-', '_', ' '], ""));
    match canonical.as_deref() {
        None | Some("utf8") => {
            std::str::from_utf8(bytes).map(Cow::Borrowed).map_err(|_| decode_err("input is not valid UTF-8"))
        }
        Some("utf16" | "utf16le" | "utf16be") => {
            Err(decode_err("the declaration names UTF-16, but the input is not UTF-16-encoded"))
        }
        Some("usascii" | "ascii") if !bytes.is_ascii() => {
            Err(decode_err("the declaration names US-ASCII, but the input holds non-ASCII bytes"))
        }
        Some("iso88591" | "88591" | "latin1" | "l1" | "cp819") => {
            Ok(Cow::Owned(bytes.iter().map(|&b| b as char).collect()))
        }
        _ if bytes.is_ascii() => Ok(Cow::Borrowed(std::str::from_utf8(bytes).expect("ASCII is UTF-8"))),
        _ => Err(decode_err(&format!(
            "the declaration names encoding \"{}\", which this reader cannot decode (it reads UTF-8, UTF-16 and ISO-8859-1)",
            declared.unwrap_or_default()
        ))),
    }
}

fn decode_err(message: &str) -> Error {
    Error::Convert(format!("xml: {message}"))
}

fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String> {
    if !bytes.len().is_multiple_of(2) {
        return Err(decode_err("UTF-16 input has an odd number of bytes"));
    }
    let units = bytes.chunks_exact(2).map(|c| unit([c[0], c[1]]));
    let text: String = char::decode_utf16(units)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| decode_err("UTF-16 input holds an unpaired surrogate"))?;
    Ok(text)
}

/// The `encoding` pseudo-attribute of a leading `<?xml ...?>` declaration, read
/// from an ASCII-compatible byte stream.
fn declared_encoding(bytes: &[u8]) -> Option<&str> {
    let rest = bytes.strip_prefix(b"<?xml")?;
    if !rest.first().is_some_and(u8::is_ascii_whitespace) {
        return None;
    }
    let end = rest.windows(2).position(|w| w == b"?>")?;
    let decl = std::str::from_utf8(&rest[..end]).ok()?;
    let at = decl.find("encoding")?;
    let after = decl[at + "encoding".len()..].trim_start().strip_prefix('=')?.trim_start();
    let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let value = &after[1..];
    value.find(quote).map(|close| &value[..close])
}

/// Parse a complete XML document, returning its root element. See
/// [`parse_document`] for what surrounds the root. The readers parse whole
/// documents; this is the tests' shorthand.
#[cfg(test)]
pub(crate) fn parse(input: &str) -> Result<XmlElement> {
    parse_document(input).map(|doc| doc.root)
}

/// Parse a complete XML document. A leading BOM, the XML declaration and the
/// whitespace after it are skipped; the whitespace, comments, processing
/// instructions and DOCTYPE before and after the root element are kept;
/// anything else trailing the root element is rejected. Spans index `input`
/// as given, BOM included.
///
/// The KML reader builds its own tree through [`parse_document_with`]; this is
/// the [`XmlElement`] form the tests and the schema oracle read.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_document(input: &str) -> Result<XmlDocument> {
    let (before, root, after) = parse_document_with(input, &mut Elements)?;
    Ok(XmlDocument { before, root, after })
}

/// An element the parser has finished, handed to a [`Builder`] with its
/// children already built. Names borrow the input; the namespace is resolved.
pub(crate) struct Parsed<'a, N> {
    pub name: &'a str,
    pub prefix: Option<&'a str>,
    pub ns: Option<Rc<str>>,
    pub attrs: Vec<XmlAttr>,
    pub children: Vec<N>,
    pub content: Vec<(usize, Content)>,
    pub span: Range<usize>,
}

/// What the parser builds each finished element into, bottom-up. A caller
/// with its own tree — the KML model — builds it directly, so a large document
/// never exists twice; [`parse_document`] builds [`XmlElement`]s.
pub(crate) trait Builder {
    type Node;
    fn build(&mut self, el: Parsed<'_, Self::Node>) -> Self::Node;
}

/// The [`Builder`] behind [`parse_document`].
#[cfg_attr(not(test), allow(dead_code))]
struct Elements;

impl Builder for Elements {
    type Node = XmlElement;
    fn build(&mut self, el: Parsed<'_, XmlElement>) -> XmlElement {
        XmlElement {
            name: el.name.to_string(),
            prefix: el.prefix.map(str::to_string),
            ns: el.ns,
            attrs: el.attrs,
            children: el.children,
            content: el.content,
            span: Some(el.span),
        }
    }
}

/// [`parse_document`] with the tree built by `builder`: the comments and
/// processing instructions before the root, the root, and those after it.
pub(crate) fn parse_document_with<B: Builder>(
    input: &str,
    builder: &mut B,
) -> Result<(Vec<Content>, B::Node, Vec<Content>)> {
    let mut p = Parser::new(input);
    if p.starts_with("\u{feff}") {
        p.pos += '\u{feff}'.len_utf8();
    }
    if p.starts_with("<?xml") && p.bytes.get(p.pos + 5).is_some_and(|&b| is_space(b) || b == b'?') {
        p.parse_processing_instruction()?;
        p.skip_ws();
    }
    let before = p.misc_outside_root()?;
    let root = p.parse_element(builder)?;
    let after = p.misc_outside_root()?;
    if p.pos != p.bytes.len() {
        return Err(p.err("unexpected trailing content after root element"));
    }
    Ok((before, root, after))
}

/// Serialize an element tree back to a complete XML document: an `<?xml?>`
/// declaration followed by the element, with text and attribute content
/// escaped. The inverse of [`parse`], up to lexical form
/// (`plans/kml-layout.org` tier 3).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write(root: &XmlElement) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    write_element(root, &mut out);
    out
}

/// [`write`] for a whole [`XmlDocument`], with the comments and processing
/// instructions on either side of the root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write_document(doc: &XmlDocument) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    for m in &doc.before {
        write_content(m, &mut out);
    }
    write_element(&doc.root, &mut out);
    for m in &doc.after {
        write_content(m, &mut out);
    }
    out
}

fn write_element(el: &XmlElement, out: &mut String) {
    let name = el.qualified_name();
    out.push('<');
    out.push_str(&name);
    for a in &el.attrs {
        out.push(' ');
        out.push_str(&qualify(a.prefix.as_deref(), &a.name));
        out.push_str("=\"");
        escape_into(&a.value, out, true);
        out.push('"');
    }
    if el.children.is_empty() && el.content.is_empty() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    let mut content = el.content.iter().peekable();
    for (i, child) in el.children.iter().enumerate() {
        while let Some((_, c)) = content.next_if(|(at, _)| *at <= i) {
            write_content(c, out);
        }
        write_element(child, out);
    }
    for (_, c) in content {
        write_content(c, out);
    }
    out.push_str("</");
    out.push_str(&name);
    out.push('>');
}

pub(crate) fn write_content(c: &Content, out: &mut String) {
    match c {
        Content::Text(text) => escape_into(text, out, false),
        Content::Comment(text) => {
            out.push_str("<!--");
            out.push_str(text);
            out.push_str("-->");
        }
        Content::Doctype(text) => out.push_str(text),
        Content::Pi { target, data } => {
            out.push_str("<?");
            out.push_str(target);
            if !data.is_empty() {
                out.push(' ');
                out.push_str(data);
            }
            out.push_str("?>");
        }
    }
}

/// Escape `&`/`<`/`>` (plus `"` when `in_attr`) into `out`. Bulk-copies runs
/// of ordinary bytes with one `push_str` and only stops on a byte that must
/// be escaped, mirroring `json::value::write_json_string` — every
/// escape-triggering byte here is ASCII, so run boundaries always fall on
/// char boundaries and each slice is valid UTF-8.
pub(crate) fn escape_into(s: &str, out: &mut String, in_attr: bool) {
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
    /// Namespace bindings in scope, innermost last: `(prefix, uri)`, with
    /// `prefix: None` for the default namespace and `uri: None` for
    /// `xmlns=""`, which undeclares it.
    scopes: Vec<(Option<String>, Option<Rc<str>>)>,
    xml_ns: Rc<str>,
    xmlns_ns: Rc<str>,
    /// The whitespace runs read so far, shared ([`Parser::run`]).
    spaces: Vec<Rc<str>>,
    /// A spare buffer for the text run being read.
    text: String,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser {
            bytes: input.as_bytes(),
            pos: 0,
            scopes: Vec::new(),
            xml_ns: Rc::from(XML_NS),
            xmlns_ns: Rc::from(XMLNS_NS),
            spaces: Vec::new(),
            text: String::new(),
        }
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
        while self.peek().is_some_and(is_space) {
            self.pos += 1;
        }
    }

    fn str_between(&self, start: usize, end: usize) -> Result<&'a str> {
        std::str::from_utf8(&self.bytes[start..end]).map_err(|_| self.err("invalid utf-8"))
    }

    /// Whitespace, processing instructions, comments and a `<!DOCTYPE ...>`
    /// — anything that can appear before or after the document's root
    /// element, in order. An XML declaration found here is not returned.
    fn misc_outside_root(&mut self) -> Result<Vec<Content>> {
        let mut out = Vec::new();
        loop {
            let start = self.pos;
            self.skip_ws();
            if self.pos > start {
                let run = self.str_between(start, self.pos)?;
                out.push(Content::Text(self.run(run)));
            }
            if self.starts_with("<?") {
                if let Some(pi) = self.parse_processing_instruction()? {
                    out.push(pi);
                }
            } else if self.starts_with("<!--") {
                out.push(self.parse_comment()?);
            } else if self.starts_with("<!DOCTYPE") || self.starts_with("<!doctype") {
                out.push(self.parse_doctype()?);
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// A text run as stored: whitespace shared with every equal run before
    /// it, so a document's indentation is a handful of strings however many
    /// elements it lays out; anything else its own.
    fn run(&mut self, text: &str) -> Rc<str> {
        if text.len() > 128 || !text.bytes().all(is_space) {
            return Rc::from(text);
        }
        if let Some(r) = self.spaces.iter().find(|r| ***r == *text) {
            return r.clone();
        }
        let r: Rc<str> = Rc::from(text);
        if self.spaces.len() < 64 {
            self.spaces.push(r.clone());
        }
        r
    }

    /// `<?target data?>`, or `None` for the XML declaration itself.
    fn parse_processing_instruction(&mut self) -> Result<Option<Content>> {
        self.pos += 2; // "<?"
        let end = self.find(b"?>").ok_or_else(|| self.err("unterminated processing instruction"))?;
        let body = self.str_between(self.pos, end)?;
        self.pos = end + 2;
        let (target, data) = match body.find(|c: char| c.is_ascii_whitespace()) {
            Some(at) => (&body[..at], body[at..].trim_start()),
            None => (body, ""),
        };
        if target == "xml" {
            return Ok(None);
        }
        Ok(Some(Content::Pi { target: target.into(), data: data.into() }))
    }

    fn parse_comment(&mut self) -> Result<Content> {
        self.pos += 4; // "<!--"
        let end = self.find(b"-->").ok_or_else(|| self.err("unterminated comment"))?;
        let text = self.str_between(self.pos, end)?;
        self.pos = end + 3;
        Ok(Content::Comment(text.into()))
    }

    /// A `<!DOCTYPE ...>` declaration, with an optional bracketed internal
    /// subset (`[ ... ]`), as its source text. Read for its extent only: no
    /// entity it declares is expanded and no external subset is fetched
    /// (`plans/kml-layout.org` D-L6). A quoted literal or a comment in the
    /// subset may hold `]` or `>` without ending anything.
    fn parse_doctype(&mut self) -> Result<Content> {
        let start = self.pos;
        self.pos += 2; // "<!"
        let mut in_subset = false;
        loop {
            if in_subset && self.starts_with("<!--") {
                self.parse_comment()?;
                continue;
            }
            match self.bump() {
                None => return Err(self.err("unterminated <!DOCTYPE ...>")),
                Some(quote @ (b'"' | b'\'')) => {
                    let end = self.find(&[quote]).ok_or_else(|| self.err("unterminated literal in <!DOCTYPE ...>"))?;
                    self.pos = end + 1;
                }
                Some(b'[') => in_subset = true,
                Some(b']') => in_subset = false,
                Some(b'>') if !in_subset => break,
                _ => {}
            }
        }
        Ok(Content::Doctype(self.str_between(start, self.pos)?.into()))
    }

    fn find(&self, needle: &[u8]) -> Option<usize> {
        self.bytes[self.pos..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|i| self.pos + i)
    }

    /// The namespace a prefix (or, for `None`, the default namespace) is bound
    /// to in the current scope.
    fn resolve(&self, prefix: Option<&str>) -> Option<Rc<str>> {
        if prefix == Some("xml") {
            return Some(self.xml_ns.clone());
        }
        self.scopes.iter().rev().find(|(p, _)| p.as_deref() == prefix).and_then(|(_, uri)| uri.clone())
    }

    fn parse_element<B: Builder>(&mut self, builder: &mut B) -> Result<B::Node> {
        let start = self.pos;
        self.expect(b'<')?;
        let raw_name = self.read_name()?;
        let scope_depth = self.scopes.len();
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
                    let (prefix, name) = split_qname(raw_attr);
                    let declared = match (prefix, name) {
                        (None, "xmlns") => Some(None),
                        (Some("xmlns"), p) => Some(Some(p.to_string())),
                        _ => None,
                    };
                    let ns = match declared {
                        Some(declared) => {
                            let uri = if value.is_empty() { None } else { Some(Rc::from(value.as_str())) };
                            self.scopes.push((declared, uri));
                            Some(self.xmlns_ns.clone())
                        }
                        None => None,
                    };
                    attrs.push(XmlAttr { prefix: prefix.map(str::to_string), name: name.to_string(), ns, value });
                }
            }
        }
        // A prefixed attribute's namespace is resolved once the whole start
        // tag is read: a declaration is in scope for the tag it sits in,
        // wherever in the tag that is.
        for a in attrs.iter_mut().filter(|a| a.ns.is_none() && a.prefix.is_some()) {
            a.ns = self.resolve(a.prefix.as_deref());
        }
        let (prefix, name) = split_qname(raw_name);
        let ns = self.resolve(prefix);
        let mut el = Parsed {
            name,
            prefix,
            ns,
            attrs,
            children: Vec::new(),
            content: Vec::new(),
            span: start..start,
        };

        if self.peek() == Some(b'/') {
            self.pos += 1;
            self.expect(b'>')?;
        } else {
            self.expect(b'>')?;
            self.parse_content(raw_name, &mut el, builder)?;
        }
        self.scopes.truncate(scope_depth);
        el.span = start..self.pos;
        Ok(builder.build(el))
    }

    /// Parse an element's content up to (and consuming) its matching closing
    /// tag, whose name as written must equal `open_name`'s.
    fn parse_content<B: Builder>(
        &mut self,
        open_name: &str,
        el: &mut Parsed<'a, B::Node>,
        builder: &mut B,
    ) -> Result<()> {
        // The run being read: text and CDATA up to the next node that is
        // neither. Reused across elements; an element's content is complete
        // before its first child's starts or after its last child's ends.
        let mut text = std::mem::take(&mut self.text);
        loop {
            self.read_text_into(&mut text)?;
            if !self.starts_with("<![CDATA[") && !text.is_empty() {
                let run = self.run(&text);
                el.content.push((el.children.len(), Content::Text(run)));
                text.clear();
            }
            match self.peek() {
                None => return Err(self.err("unexpected end of input inside element")),
                Some(b'<') if self.starts_with("</") => {
                    self.pos += 2;
                    let raw_name = self.read_name()?;
                    self.skip_ws();
                    self.expect(b'>')?;
                    if raw_name != open_name {
                        return Err(self.err("mismatched closing tag"));
                    }
                    if el.children.is_empty() {
                        merge_leaf_text(&mut el.content);
                    }
                    self.text = text;
                    return Ok(());
                }
                Some(b'<') if self.starts_with("<!--") => {
                    let comment = self.parse_comment()?;
                    el.content.push((el.children.len(), comment));
                }
                Some(b'<') if self.starts_with("<![CDATA[") => {
                    self.pos += 9;
                    let end = self.find(b"]]>").ok_or_else(|| self.err("unterminated CDATA section"))?;
                    let s = std::str::from_utf8(&self.bytes[self.pos..end])
                        .map_err(|_| self.err("invalid utf-8 in CDATA section"))?;
                    text.push_str(s);
                    self.pos = end + 3;
                }
                Some(b'<') if self.starts_with("<?") => {
                    if let Some(pi) = self.parse_processing_instruction()? {
                        el.content.push((el.children.len(), pi));
                    }
                }
                Some(b'<') => {
                    self.text = std::mem::take(&mut text);
                    let child = self.parse_element(builder)?;
                    text = std::mem::take(&mut self.text);
                    el.children.push(child);
                }
                Some(_) => unreachable!("read_text_into only stops at '<' or end of input"),
            }
        }
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
                out.push_str(self.str_between(start, self.pos)?);
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
                out.push_str(self.str_between(start, self.pos)?);
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

    /// An element or attribute name as written, prefix included (see
    /// [`split_qname`]), borrowed from the input. Permissive rather than
    /// spec-exact: anything up to the next whitespace, `/`, `>`, or `=`.
    fn read_name(&mut self) -> Result<&'a str> {
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
        std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| self.err("invalid utf-8 in name"))
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

/// Split a qualified name into its prefix and local part: `gx:Track` ->
/// `(Some("gx"), "Track")`, `Track` -> `(None, "Track")`.
fn split_qname(raw: &str) -> (Option<&str>, &str) {
    match raw.split_once(':') {
        Some((prefix, local)) => (Some(prefix), local),
        None => (None, raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KML: &str = "http://www.opengis.net/kml/2.2";
    const GX: &str = "http://www.google.com/kml/ext/2.2";

    fn child<'a>(el: &'a XmlElement, name: &str) -> &'a XmlElement {
        el.children.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn parses_nested_elements_with_attrs_and_text() {
        let root = parse(
            r#"<kml><Placemark id="p1"><name>A Point</name><Point><coordinates>1,2,3</coordinates></Point></Placemark></kml>"#,
        )
        .unwrap();
        assert_eq!(root.name, "kml");
        let placemark = child(&root, "Placemark");
        assert_eq!(placemark.attr("id"), Some("p1"));
        assert_eq!(child(placemark, "name").text_trimmed(), "A Point");
        let point = child(placemark, "Point");
        assert_eq!(child(point, "coordinates").text_trimmed(), "1,2,3");
    }

    #[test]
    fn handles_self_closing_and_multiple_attrs() {
        let root = parse(r#"<a x="1" y="2"/>"#).unwrap();
        assert_eq!(root.name, "a");
        assert_eq!(root.attr("x"), Some("1"));
        assert_eq!(root.attr("y"), Some("2"));
        assert!(root.children.is_empty());
        assert!(root.text().is_empty());
    }

    #[test]
    fn skips_the_declaration_and_keeps_the_doctype_comments_pis_and_whitespace() {
        let doc = parse_document(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE kml [ <!ENTITY foo "bar"> ]>
<!-- a comment --><kml><!-- inner --><a/></kml>
<!-- trailing --><?pi trailing?>"#,
        )
        .unwrap();
        assert_eq!(doc.root.name, "kml");
        assert_eq!(doc.root.children.len(), 1);
        let nl = || Content::Text(Rc::from("\n"));
        assert_eq!(
            doc.before,
            vec![Content::Doctype(r#"<!DOCTYPE kml [ <!ENTITY foo "bar"> ]>"#.into()), nl(), Content::Comment(" a comment ".into())]
        );
        assert_eq!(doc.root.content, vec![(0, Content::Comment(" inner ".into()))]);
        assert_eq!(
            doc.after,
            vec![nl(), Content::Comment(" trailing ".into()), Content::Pi { target: "pi".into(), data: "trailing".into() }]
        );
    }

    #[test]
    fn a_doctype_ends_at_its_own_close_whatever_its_subset_quotes() {
        let doctype = r#"<!DOCTYPE kml [ <!ENTITY a "]>"> <!-- ]> --> <!ENTITY b '>'> ]>"#;
        let doc = parse_document(&format!("{doctype}<kml/>")).unwrap();
        assert_eq!(doc.before, vec![Content::Doctype(doctype.into())]);
        assert_eq!(write_document(&doc), format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{doctype}<kml/>"));
    }

    #[test]
    fn keeps_whitespace_runs_where_they_were() {
        let src = "<a>\n  <b>x</b>\n  <!--c-->\n  <d/>\n</a>";
        let root = parse(src).unwrap();
        let ws = |s: &str| Content::Text(Rc::from(s));
        assert_eq!(
            root.content,
            vec![(0, ws("\n  ")), (1, ws("\n  ")), (1, Content::Comment("c".into())), (1, ws("\n  ")), (2, ws("\n"))]
        );
        assert_eq!(write(&root), format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{src}"));
    }

    #[test]
    fn equal_whitespace_runs_are_one_string() {
        let root = parse("<a>\n  <b/>\n  <c/>\n</a>").unwrap();
        let (Content::Text(first), Content::Text(second)) = (&root.content[0].1, &root.content[1].1) else {
            panic!("{:?}", root.content)
        };
        assert!(Rc::ptr_eq(first, second));
    }

    #[test]
    fn records_comment_positions_among_children() {
        let root = parse("<a><!--0--><b/><!--1--><?p x?><c/><!--2--></a>").unwrap();
        assert_eq!(
            root.content,
            vec![
                (0, Content::Comment("0".into())),
                (1, Content::Comment("1".into())),
                (1, Content::Pi { target: "p".into(), data: "x".into() }),
                (2, Content::Comment("2".into())),
            ]
        );
    }

    #[test]
    fn strips_bom() {
        let root = parse("\u{feff}<a/>").unwrap();
        assert_eq!(root.name, "a");
    }

    #[test]
    fn decodes_predefined_and_numeric_entities() {
        let root = parse("<a>&amp;&lt;&gt;&quot;&apos; &#65;&#x42;</a>").unwrap();
        assert_eq!(root.text(), "&<>\"' AB");
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
        assert_eq!(root.text(), "<b>not a tag</b> & neither is this");
    }

    #[test]
    fn cdata_joins_the_text_around_it_and_is_written_escaped() {
        let root = parse("<a>x <![CDATA[<b>]]> y</a>").unwrap();
        assert_eq!(root.content.len(), 1);
        assert_eq!(root.text(), "x <b> y");
        assert!(write(&root).ends_with("<a>x &lt;b&gt; y</a>"));
    }

    #[test]
    fn resolves_default_and_prefixed_namespaces() {
        let root = parse(&format!(
            r#"<kml xmlns="{KML}" xmlns:gx="{GX}"><Placemark><gx:Track/><Point/></Placemark></kml>"#
        ))
        .unwrap();
        assert!(root.in_ns(KML));
        let placemark = &root.children[0];
        assert!(placemark.in_ns(KML));
        let track = &placemark.children[0];
        assert_eq!((track.prefix.as_deref(), track.name.as_str()), (Some("gx"), "Track"));
        assert!(track.in_ns(GX));
        assert_eq!(track.qualified_name(), "gx:Track");
        assert!(placemark.children[1].in_ns(KML));
    }

    #[test]
    fn a_foreign_element_with_a_kml_local_name_is_not_in_the_kml_namespace() {
        // The correctness half of D-K6: by local name alone this is a <Point>.
        let root = parse(&format!(r#"<kml xmlns="{KML}" xmlns:x="urn:x"><x:Point/></kml>"#)).unwrap();
        let point = &root.children[0];
        assert_eq!(point.name, "Point");
        assert!(!point.in_ns(KML));
        assert!(point.in_ns("urn:x"));
    }

    #[test]
    fn a_prefixed_root_keeps_its_prefix() {
        let root = parse(&format!(r#"<kml:kml xmlns:kml="{KML}"><kml:Placemark/></kml:kml>"#)).unwrap();
        assert_eq!(root.prefix.as_deref(), Some("kml"));
        assert!(root.in_ns(KML));
        assert!(root.children[0].in_ns(KML));
        assert_eq!(child(&root, "Placemark").qualified_name(), "kml:Placemark");
    }

    #[test]
    fn declarations_are_scoped_to_their_element() {
        let root = parse(r#"<a xmlns="urn:outer"><b xmlns="urn:inner"><c/></b><d/><e xmlns=""/></a>"#).unwrap();
        assert!(root.in_ns("urn:outer"));
        assert!(root.children[0].in_ns("urn:inner"));
        assert!(root.children[0].children[0].in_ns("urn:inner"));
        assert!(root.children[1].in_ns("urn:outer"));
        // xmlns="" undeclares the default namespace.
        assert_eq!(root.children[2].ns, None);
    }

    #[test]
    fn a_declaration_after_the_attribute_it_binds_still_applies() {
        let root = parse(r#"<a p:x="1" xmlns:p="urn:p"/>"#).unwrap();
        assert_eq!(root.attr_ns("urn:p", "x"), Some("1"));
    }

    #[test]
    fn an_unbound_prefix_keeps_the_prefix_and_gets_no_namespace() {
        let root = parse("<kml><gx:Track/></kml>").unwrap();
        let track = &root.children[0];
        assert_eq!(track.prefix.as_deref(), Some("gx"));
        assert_eq!(track.ns, None);
    }

    #[test]
    fn the_xml_prefix_is_always_bound() {
        let root = parse(r#"<a xml:lang="en"/>"#).unwrap();
        assert_eq!(root.attr_ns(XML_NS, "lang"), Some("en"));
    }

    #[test]
    fn unprefixed_attributes_are_in_no_namespace_and_prefixed_ones_are() {
        let root = parse(&format!(r#"<a xmlns="{KML}" xmlns:gx="{GX}" id="1" gx:id="2"/>"#)).unwrap();
        assert_eq!(root.attr("id"), Some("1"));
        assert_eq!(root.attr_ns(GX, "id"), Some("2"));
        // The declarations are attributes in the xmlns namespace, not found by name.
        assert_eq!(root.attr("xmlns"), None);
        assert_eq!(root.attr_ns(XMLNS_NS, "xmlns"), Some(KML));
        assert_eq!(root.attr_ns(XMLNS_NS, "gx"), Some(GX));
    }

    #[test]
    fn records_each_elements_source_span() {
        let input = r#"<a><b x="1">t</b><c/></a>"#;
        let root = parse(input).unwrap();
        assert_eq!(&input[root.span.clone().unwrap()], input);
        assert_eq!(&input[root.children[0].span.clone().unwrap()], r#"<b x="1">t</b>"#);
        assert_eq!(&input[root.children[1].span.clone().unwrap()], "<c/>");
    }

    #[test]
    fn spans_index_the_input_past_a_bom() {
        let input = "\u{feff}<a><b/></a>";
        let root = parse(input).unwrap();
        assert_eq!(&input[root.children[0].span.clone().unwrap()], "<b/>");
    }

    #[test]
    fn keeps_mixed_text_in_place_among_child_elements() {
        let root = parse("<a>one<b/>two<c/>three</a>").unwrap();
        assert_eq!(root.children.len(), 2);
        assert_eq!(write(&root), "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a>one<b/>two<c/>three</a>");
    }

    #[test]
    fn a_leafs_text_split_by_a_comment_is_one_value() {
        let root = parse("<a>one<!--c-->two</a>").unwrap();
        assert_eq!(root.text(), "onetwo");
    }

    #[test]
    fn rejects_mismatched_closing_tag() {
        assert!(parse("<a><b></c></a>").is_err());
    }

    #[test]
    fn rejects_a_closing_tag_with_a_different_prefix() {
        // Same local name, different qualified name: not well-formed.
        assert!(parse(r#"<a xmlns:p="urn:x" xmlns:q="urn:x"><p:b></q:b></a>"#).is_err());
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
    fn writes_namespace_declarations() {
        let el = XmlElement::with_children("kml", vec![]).with_ns_decl(None, KML).with_ns_decl(Some("gx"), GX);
        assert_eq!(
            write(&el),
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"{KML}\" xmlns:gx=\"{GX}\"/>")
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

    #[test]
    fn parse_then_write_reproduces_prefixes_declarations_comments_and_pis() {
        let input = format!(
            r#"<!--head--><kml xmlns="{KML}" xmlns:gx="{GX}"><!--a--><Placemark gx:x="1"><gx:Track><gx:coord>1 2 3</gx:coord></gx:Track><?app keep?></Placemark></kml><!--tail-->"#
        );
        let doc = parse_document(&input).unwrap();
        let written = write_document(&doc);
        assert_eq!(written, format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{input}"));
        assert_eq!(parse_document(&written).unwrap(), doc);
    }

    #[test]
    fn decode_reads_utf8_with_and_without_a_bom() {
        assert_eq!(decode("<a>é</a>".as_bytes()).unwrap(), "<a>é</a>");
        assert_eq!(decode(b"\xEF\xBB\xBF<a/>").unwrap(), "<a/>");
        assert!(decode(b"<a>\xFF</a>").is_err());
    }

    fn utf16_bytes(s: &str, le: bool, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let units = bom.then_some(0xFEFF).into_iter().chain(s.encode_utf16());
        for u in units {
            out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        out
    }

    #[test]
    fn decode_reads_utf16_in_both_byte_orders_with_and_without_a_bom() {
        let doc = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><a>Zürich 𝄞</a>";
        for le in [true, false] {
            for bom in [true, false] {
                let bytes = utf16_bytes(doc, le, bom);
                let text = decode(&bytes).unwrap();
                assert_eq!(text, doc, "le={le} bom={bom}");
                assert_eq!(parse(&text).unwrap().text(), "Zürich 𝄞");
            }
        }
    }

    #[test]
    fn decode_refuses_malformed_utf16() {
        let mut odd = utf16_bytes("<a/>", true, true);
        odd.push(0);
        assert!(decode(&odd).is_err());
        // A lone high surrogate.
        assert!(decode(&[0xFF, 0xFE, b'<', 0, 0x00, 0xD8, b'>', 0]).is_err());
    }

    #[test]
    fn decode_honours_a_declared_iso_8859_1() {
        let bytes = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><a>K\xF8benhavn</a>";
        let text = decode(bytes).unwrap();
        assert_eq!(parse(&text).unwrap().text(), "København");
    }

    #[test]
    fn decode_accepts_any_declared_encoding_over_ascii_bytes_and_refuses_it_over_others() {
        let ascii = b"<?xml version='1.0' encoding='windows-1252'?><a>plain</a>";
        assert_eq!(parse(&decode(ascii).unwrap()).unwrap().text(), "plain");
        let high = b"<?xml version='1.0' encoding='windows-1252'?><a>\x93quoted\x94</a>";
        let err = decode(high).unwrap_err().to_string();
        assert!(err.contains("windows-1252"), "{err}");
    }

    #[test]
    fn decode_refuses_a_utf16_declaration_over_single_byte_input() {
        assert!(decode(b"<?xml version=\"1.0\" encoding=\"UTF-16\"?><a/>").is_err());
    }

    #[test]
    fn decode_refuses_non_ascii_under_a_us_ascii_declaration() {
        assert!(decode(b"<?xml version=\"1.0\" encoding=\"US-ASCII\"?><a>\xC3\xA9</a>").is_err());
    }
}
