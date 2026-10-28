//! The KML document model: a whole document raised into KML 2.3's terms, so
//! nothing in it is lost and every KML construct in it is known by what it is
//! (`plans/kml-standard.org` D-K4, K3).
//!
//! *An identified node tree, not a struct per type* (D-K4, decided
//! 2026-10-23). Every element in a KML namespace is resolved to a
//! [`KmlElement`], the closed 2.3 vocabulary, with the source's spelling
//! recorded beside it ([`Spelling`]); its attributes, children, text and
//! comments stay on its [`Node`] as they were. An element in no KML namespace
//! is carried as the source's own bytes ([`Kind::Foreign`]). So raising a
//! document and writing it back loses nothing by construction, however far the
//! document strays from the schema — which real files do (children out of 2.2's
//! order, interleaved tracks, a 2.3-only element under no stamp). What 2.2 and
//! 2.3 spell differently, and the order 2.2 requires, are tables transcribed
//! from the schemas, not knowledge spread through per-type code; typed reading
//! is done by the projection into the IR, which needs a dozen element kinds,
//! not 168.
//!
//! The vocabulary is generated from `ogckml23.xsd` (SHA-256
//! `5222be3703c634add22c0bfa0f90df8935cb01c4a1c60b287d33dc88fc695bcd`) by
//! `tests/fixtures/gen_kml_vocab.py`; `the_vocabulary_is_the_schemas` checks it
//! against the schema when one is supplied.

use std::borrow::Cow;

use std::rc::Rc;

use crate::error::Result;
use crate::xml::{self, Builder, Content, Parsed, Tree, XmlAttr, XmlElement};

/// OGC KML 2.2's namespace, which 2.3 keeps.
pub(crate) const KML_NS: &str = "http://www.opengis.net/kml/2.2";
/// Google's pre-OGC namespace for 2.2, read as OGC 2.2 with the namespace
/// recorded (`plans/kml-standard.org` open question 8).
pub(crate) const GOOGLE_KML_2_2_NS: &str = "http://earth.google.com/kml/2.2";
/// Google's extension namespace.
pub(crate) const GX_NS: &str = "http://www.google.com/kml/ext/2.2";

/// The prefix this crate writes a `gx:` element in, declaring it on the
/// element itself so the element means the same wherever it lands.
pub(crate) const GX_PREFIX: &str = "gx";

/// Whether `el` is a KML element: in a KML namespace, or in no namespace and
/// unprefixed (a document that declares none, D-K13).
pub(crate) fn is_kml(ns: Option<&str>, prefix: Option<&str>) -> bool {
    match ns {
        Some(ns) => ns == KML_NS || ns == GOOGLE_KML_2_2_NS,
        None => prefix.is_none(),
    }
}

/// Whether `el` is a `gx:` extension element. A `gx` prefix that was never
/// declared is taken at its conventional meaning: the alternative is to drop
/// a `<gx:Track>` as an unknown element, and no other namespace uses it.
pub(crate) fn is_gx(ns: Option<&str>, prefix: Option<&str>) -> bool {
    ns == Some(GX_NS) || (ns.is_none() && prefix == Some("gx"))
}

/// The `gx:` elements KML 2.3 adopted, as `(gx local name, 2.3 element)` —
/// `plans/kml-standard.org` § WHAT THE SCHEMAS SAY, checked against
/// `kml22gx.xsd` and `ogckml23.xsd`. Twenty-five keep their name; one is
/// renamed (`gx:altitudeMode`, whose only values are the seafloor modes).
///
/// *Not here, deliberately*: `gx:drawOrder`, `gx:x` and `gx:y` share a local
/// name with a 2.3 element that means something else, and the ten `gx:`
/// elements with no 2.3 counterpart (`ViewerOptions`, `outerColor`, ...). All
/// of them are foreign content to 2.3 and are carried verbatim (D-K10).
const GX_ADOPTED: [(&str, KmlElement); 26] = [
    ("AnimatedUpdate", KmlElement::AnimatedUpdate),
    ("FlyTo", KmlElement::FlyTo),
    ("LatLonQuad", KmlElement::LatLonQuad),
    ("MultiTrack", KmlElement::MultiTrack),
    ("Playlist", KmlElement::Playlist),
    ("SimpleArrayData", KmlElement::SimpleArrayData),
    ("SimpleArrayField", KmlElement::SimpleArrayField),
    ("SoundCue", KmlElement::SoundCue),
    ("TimeSpan", KmlElement::TimeSpan),
    ("TimeStamp", KmlElement::TimeStamp),
    ("Tour", KmlElement::Tour),
    ("TourControl", KmlElement::TourControl),
    ("Track", KmlElement::Track),
    ("Wait", KmlElement::Wait),
    ("altitudeMode", KmlElement::SeaFloorAltitudeMode),
    ("altitudeOffset", KmlElement::AltitudeOffset),
    ("angles", KmlElement::Angles),
    ("balloonVisibility", KmlElement::BalloonVisibility),
    ("coord", KmlElement::Coord),
    ("delayedStart", KmlElement::DelayedStart),
    ("duration", KmlElement::Duration),
    ("flyToMode", KmlElement::FlyToMode),
    ("horizFov", KmlElement::HorizFov),
    ("interpolate", KmlElement::Interpolate),
    ("playMode", KmlElement::PlayMode),
    ("value", KmlElement::Value),
];

/// Every element of the 2.3 vocabulary.
#[cfg(test)]
pub(crate) fn vocabulary() -> impl Iterator<Item = KmlElement> {
    NAMES.iter().map(|(_, element)| *element)
}

impl KmlElement {
    /// The 2.3 element a `gx:` local name was adopted as, if it was.
    pub fn from_gx_name(name: &str) -> Option<KmlElement> {
        GX_ADOPTED.iter().find(|(n, _)| *n == name).map(|(_, el)| *el)
    }

    /// This element's local name in the `gx:` namespace, if 2.3 adopted it
    /// from there.
    pub fn gx_name(self) -> Option<&'static str> {
        GX_ADOPTED.iter().find(|(_, el)| *el == self).map(|(n, _)| *n)
    }
}

/// Which namespace a KML element was spelled in. The element is the same
/// either way; this is provenance (D-K5), so a same-version write gives back
/// the spelling it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spelling {
    /// The document's KML namespace: OGC's, Google's pre-OGC 2.2, or none.
    Kml,
    /// The `gx:` namespace, for an element 2.3 adopted from it.
    Gx,
}

/// What a [`Node`] is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    /// An element of the 2.3 vocabulary, and the namespace it was spelled in.
    Kml { element: KmlElement, spelling: Spelling },
    /// Anything else, as the source's own bytes: an extension element, a
    /// `gx:` element 2.3 did not adopt, or a name in a KML namespace that 2.3
    /// does not define (`in_kml_namespace`) — a typo, or a later version's.
    /// Its prefixes resolve through the declarations of the elements around
    /// it, which are kept, so the bytes mean the same thing where they are
    /// written back.
    ///
    /// Boxed: almost no node is foreign, and inline its two strings would
    /// make every node of a large document larger.
    Foreign(Box<Foreign>),
}

/// The content of a [`Kind::Foreign`] node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Foreign {
    pub qualified_name: String,
    pub verbatim: String,
    pub in_kml_namespace: bool,
}

/// One element of a raised document. For a [`Kind::Foreign`] node everything
/// is in its verbatim bytes, and the other members are empty.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Node {
    pub kind: Kind,
    /// The prefix as written (`gx` in `<gx:Track>`), or `None`.
    pub prefix: Option<String>,
    /// In document order: namespace declarations, KML attributes and foreign
    /// attributes alike, each with its own namespace.
    pub attrs: Vec<XmlAttr>,
    pub children: Vec<Node>,
    /// Text runs, comments and processing instructions, each tagged with the
    /// index of the child it precedes, as [`XmlElement::content`] is. The
    /// whitespace runs are the document's layout (`plans/kml-layout.org`).
    pub content: Vec<(usize, Content)>,
}

/// A raised document: the root `<kml>` and what surrounds it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KmlDocument {
    pub before: Vec<Content>,
    pub root: Node,
    pub after: Vec<Content>,
    /// The root element's namespace URI, which version detection reads. Not
    /// written: the root's declarations are among its attributes.
    pub root_ns: Option<Rc<str>>,
}

impl Node {
    /// The 2.3 element this node is, or `None` for foreign content.
    pub fn element(&self) -> Option<KmlElement> {
        match self.kind {
            Kind::Kml { element, .. } => Some(element),
            Kind::Foreign(_) => None,
        }
    }

    /// Whether this node is `element`, in whichever spelling.
    pub fn is(&self, element: KmlElement) -> bool {
        self.element() == Some(element)
    }

    /// The first child that is `element`.
    pub fn child(&self, element: KmlElement) -> Option<&Node> {
        self.children.iter().find(|c| c.is(element))
    }

    /// Every child that is `element`, in document order.
    pub fn children_of(&self, element: KmlElement) -> impl Iterator<Item = &Node> {
        self.children.iter().filter(move |c| c.is(element))
    }

    /// An unprefixed attribute's value — the form every KML attribute takes.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.ns.is_none() && a.name == name).map(|a| a.value.as_str())
    }

    /// A leaf's text: its one text run ([`xml::merge_leaf_text`]).
    pub fn text(&self) -> &str {
        xml::first_text(&self.content)
    }

    /// Direct text, trimmed — the form KML's leaf elements want.
    pub fn text_trimmed(&self) -> &str {
        self.text().trim()
    }

    /// Whether this node has child elements and no whitespace between them:
    /// its inside on one line, a habit an element inserted beside it copies.
    pub fn keeps_inside_on_one_line(&self) -> bool {
        !self.children.is_empty() && !self.content.iter().any(|(_, c)| c.is_space())
    }

    /// Whether any text run holds more than whitespace.
    pub fn has_text(&self) -> bool {
        self.content.iter().any(|(_, c)| matches!(c, Content::Text(_)) && !c.is_space())
    }

    /// Make a leaf's text `value`, keeping the whitespace around the old
    /// value (`plans/kml-layout.org` D-L5): `<name>\n  Zürich\n</name>` set to
    /// `Bern` is `<name>\n  Bern\n</name>`.
    pub fn set_text(&mut self, value: &str) {
        let old = self.text();
        let trimmed = old.trim();
        if trimmed.is_empty() {
            return self.replace_text(value.to_string());
        }
        let lead = &old[..old.len() - old.trim_start().len()];
        let trail = &old[old.trim_end().len()..];
        self.replace_text(format!("{lead}{value}{trail}"));
    }

    /// Make a leaf's text exactly `text`, in the place of its old text.
    pub fn replace_text(&mut self, text: String) {
        let at = self.content.iter().position(|(_, c)| matches!(c, Content::Text(_)));
        self.content.retain(|(_, c)| !matches!(c, Content::Text(_)));
        if !text.is_empty() {
            self.content.insert(at.unwrap_or(0), (0, Content::Text(Rc::from(text))));
        }
    }

    /// Insert `child` at `at`, keeping each comment before the child it
    /// preceded.
    pub fn insert_child(&mut self, at: usize, child: Node) {
        self.children.insert(at, child);
        for (i, _) in self.content.iter_mut() {
            if *i >= at {
                *i += 1;
            }
        }
    }

    /// Remove the child at `at`, with the whitespace that indented it
    /// (`plans/kml-layout.org` D-L4). A comment before it now precedes
    /// whatever follows.
    pub fn remove_child(&mut self, at: usize) {
        self.children.remove(at);
        if let Some(lead) = self.lead(at) {
            self.content.remove(lead);
        }
        for (i, _) in self.content.iter_mut() {
            if *i > at {
                *i -= 1;
            }
        }
    }

    /// Where in [`Node::content`] the whitespace run straight before child
    /// `at` is — the one that indents it — if it has one.
    fn lead(&self, at: usize) -> Option<usize> {
        let end = self.content.partition_point(|(i, _)| *i <= at);
        (end > 0 && self.content[end - 1].0 == at && self.content[end - 1].1.is_space()).then(|| end - 1)
    }

    /// The text run at `index` in [`Node::content`], shared.
    fn run(&self, index: usize) -> &Rc<str> {
        match &self.content[index].1 {
            Content::Text(t) => t,
            _ => unreachable!("a lead is a text run"),
        }
    }

    /// Insert `child` at `at`, laid out as this node lays out its children
    /// (`plans/kml-layout.org` D-L3): indented as a sibling is, or one unit
    /// past this node's end tag when it has none, and its inside laid out
    /// the same way — unless the sibling it copies keeps its own inside on
    /// one line. A node with no whitespace runs, or a document with no
    /// layout, takes it as it is.
    pub fn insert_laid_out(&mut self, at: usize, mut child: Node, layout: Option<&Layout>) {
        let lead = layout.and_then(|l| {
            let (lead, inside) = self.lead_at(at, l)?;
            if inside {
                l.lay_out_under(&mut child, &lead);
            }
            Some(lead)
        });
        self.insert_child(at, child);
        if let Some(lead) = lead {
            let i = self.content.partition_point(|(i, _)| *i <= at);
            self.content.insert(i, (at, Content::Text(lead)));
        }
    }

    /// Replace child `at` with `child`, which keeps the place and the
    /// indentation of the one it replaces and has its inside laid out to
    /// match.
    pub fn replace_child(&mut self, at: usize, mut child: Node, layout: Option<&Layout>) {
        if let (Some(l), Some(lead)) = (layout, self.lead(at)) {
            l.lay_out_under(&mut child, self.run(lead));
        }
        self.children[at] = child;
    }

    /// Lay out the inside of child `at`, which was inserted before it had
    /// children to lay out.
    pub fn lay_out_child(&mut self, at: usize, layout: Option<&Layout>) {
        if let (Some(l), Some(lead)) = (layout, self.lead(at)) {
            let lead = self.run(lead).clone();
            l.lay_out_under(&mut self.children[at], &lead);
        }
    }

    /// The run to indent a child inserted at `at` with, and whether its
    /// inside is laid out too: a sibling's indentation, and its habit — a
    /// sibling that keeps its inside on one line is copied in that — or,
    /// with no sibling indented, one unit past this node's end tag.
    pub fn lead_at(&self, at: usize, layout: &Layout) -> Option<(Rc<str>, bool)> {
        let len = self.children.len();
        let sibling = [at, at.wrapping_sub(1)]
            .into_iter()
            .filter(|&i| i < len)
            .chain(0..len)
            .find_map(|i| self.lead(i).map(|run| (i, run)));
        if let Some((i, run)) = sibling {
            return Some((self.run(run).clone(), !self.children[i].keeps_inside_on_one_line()));
        }
        let closing = self.content.last().filter(|(i, c)| *i == len && c.is_space())?.1.text()?;
        closing.contains('\n').then(|| (Rc::from(format!("{closing}{}", layout.unit)), true))
    }

    /// Put this node and every KML node under it in `prefix` — for a node
    /// built to join a document whose KML namespace is prefixed. Foreign
    /// content and `gx:` elements are left as they are.
    pub fn set_prefix(&mut self, prefix: Option<&str>) {
        if let Kind::Kml { spelling, .. } = self.kind {
            // A `gx:` element keeps the prefix its own declaration binds.
            if spelling == Spelling::Kml {
                self.prefix = prefix.map(str::to_string);
            }
            for child in &mut self.children {
                child.set_prefix(prefix);
            }
        }
    }

    /// The local name this node is written with: the element's name in its
    /// spelling's namespace.
    fn local_name(&self) -> Cow<'static, str> {
        match self.kind {
            Kind::Kml { element, spelling: Spelling::Kml } => Cow::Borrowed(element.name()),
            Kind::Kml { element, spelling: Spelling::Gx } => {
                Cow::Borrowed(element.gx_name().expect("a gx spelling is only recorded for an adopted element"))
            }
            Kind::Foreign(_) => unreachable!("foreign content is written verbatim"),
        }
    }
}

impl Tree for Node {
    fn parts(&mut self) -> (&mut [Self], &mut Vec<(usize, Content)>) {
        (&mut self.children, &mut self.content)
    }

    fn opaque(&self) -> bool {
        matches!(self.kind, Kind::Foreign(_))
    }
}

/// The house indent, for a document built rather than read
/// (`plans/kml-layout.org` D-L2).
pub(crate) const HOUSE_INDENT: &str = "  ";

/// How a document lays its elements out: the indent one level adds
/// (`plans/kml-layout.org` D-L3). A document with none — one line, or
/// whitespace that never nests — has no layout, and what an edit adds to it
/// stays compact.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Layout {
    unit: String,
}

impl Layout {
    /// The layout `root`'s document is in: the first element found, depth
    /// first, whose children and end tag each start a line, with the
    /// children indented past the end tag, gives the unit.
    pub fn of(root: &Node) -> Option<Layout> {
        let closing = match root.content.last() {
            Some((i, Content::Text(t))) if *i == root.children.len() && t.contains('\n') => Some(indentation(t)),
            _ => None,
        };
        if let Some(closing) = closing {
            for at in 0..root.children.len() {
                if let Some(run) = root.lead(at)
                    && let Some(unit) = indentation(root.run(run)).strip_prefix(closing)
                    && !unit.is_empty()
                    && root.run(run).contains('\n')
                {
                    return Some(Layout { unit: unit.to_string() });
                }
            }
        }
        root.children.iter().filter(|c| !c.opaque()).find_map(Layout::of)
    }

    /// Lay out `node`'s inside for a node indented by `lead`, a run from the
    /// document: its newline and indentation are the document's own.
    pub fn lay_out_under(&self, node: &mut Node, lead: &str) {
        let Some(nl) = lead.rfind('\n') else { return };
        let (newline, indent) = lead.split_at(nl + 1);
        let newline = if newline.ends_with("\r\n") { "\r\n" } else { "\n" };
        xml::lay_out(node, newline, indent, &self.unit);
    }
}

/// The indentation a whitespace run ends with: what follows its last line
/// break.
fn indentation(run: &str) -> &str {
    run.rfind('\n').map_or(run, |at| &run[at + 1..])
}

/// Parse `source` straight into the model. Each element is raised as the
/// parser finishes it, so the document never exists as an [`XmlElement`] tree
/// beside its model — which held a large file twice, and left the freed tree's
/// pages scattered through the model's.
pub(crate) fn parse(source: &str) -> Result<KmlDocument> {
    let mut raiser = Raiser { source, last_ns: None };
    let (before, root, after) = xml::parse_document_with(source, &mut raiser)?;
    // Built bottom-up, so the last element finished is the root.
    Ok(KmlDocument { before, root, after, root_ns: raiser.last_ns })
}

/// The [`Builder`] behind [`parse`].
struct Raiser<'s> {
    source: &'s str,
    last_ns: Option<Rc<str>>,
}

impl Builder for Raiser<'_> {
    type Node = Node;

    fn build(&mut self, el: Parsed<'_, Node>) -> Node {
        let ns = el.ns.as_deref();
        let identified = if is_kml(ns, el.prefix) {
            KmlElement::from_name(el.name).map(|element| (element, Spelling::Kml))
        } else if is_gx(ns, el.prefix) {
            KmlElement::from_gx_name(el.name).map(|element| (element, Spelling::Gx))
        } else {
            None
        };
        let in_kml_namespace = is_kml(ns, el.prefix);
        self.last_ns = el.ns.clone();
        let Some((element, spelling)) = identified else {
            return Node {
                kind: Kind::Foreign(Box::new(Foreign {
                    qualified_name: xml::qualify(el.prefix, el.name).into_owned(),
                    verbatim: self.source[el.span].to_string(),
                    in_kml_namespace,
                })),
                prefix: None,
                attrs: Vec::new(),
                children: Vec::new(),
                content: Vec::new(),
            };
        };
        // A model outlives its parse — a feature's element until the write —
        // so it keeps no growth slack.
        let mut children = el.children;
        children.shrink_to_fit();
        Node {
            kind: Kind::Kml { element, spelling },
            prefix: el.prefix.map(str::to_string),
            attrs: el.attrs,
            children,
            content: el.content,
        }
    }
}

/// A node for an element this crate built rather than read: every element in
/// it is a KML one, in the KML spelling, or in the `gx:` one when the writer
/// built it with that prefix. Panics on a name 2.3 does not define, which
/// would be a writer bug.
pub(crate) fn from_built(el: &XmlElement) -> Node {
    let (element, spelling) = match el.prefix.as_deref() {
        Some(GX_PREFIX) => (KmlElement::from_gx_name(&el.name), Spelling::Gx),
        _ => (KmlElement::from_name(&el.name), Spelling::Kml),
    };
    let element =
        element.unwrap_or_else(|| panic!("the KML writer built <{}>, which KML 2.3 does not define", el.qualified_name()));
    Node {
        kind: Kind::Kml { element, spelling },
        prefix: el.prefix.clone(),
        attrs: el.attrs.clone(),
        children: el.children.iter().map(from_built).collect(),
        content: el.content.clone(),
    }
}

/// Take every whitespace run between elements out of `doc` — before and
/// after the root, and inside every element with child elements — leaving
/// a leaf's text and foreign content's bytes as they are: `--compact`
/// (`plans/kml-layout.org` D-L8).
pub(crate) fn flatten(doc: &mut KmlDocument) {
    doc.before.retain(|c| !c.is_space());
    doc.after.retain(|c| !c.is_space());
    flatten_node(&mut doc.root);
}

fn flatten_node(node: &mut Node) {
    if node.children.is_empty() {
        return;
    }
    node.content.retain(|(_, c)| !c.is_space());
    for child in node.children.iter_mut().filter(|c| !c.opaque()) {
        flatten_node(child);
    }
}

/// Write a raised document back out as it was raised — the same-version write,
/// with no version's transforms applied. Parsing the result gives back the
/// [`XmlDocument`] the model was raised from (D-K5's invariant, at the XML
/// level; `plans/kml-standard.org` K3).
pub(crate) fn write(doc: &KmlDocument) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    for c in &doc.before {
        xml::write_content(c, &mut out);
    }
    write_node(&doc.root, &mut out);
    for c in &doc.after {
        xml::write_content(c, &mut out);
    }
    out
}

fn write_node(node: &Node, out: &mut String) {
    if let Kind::Foreign(foreign) = &node.kind {
        out.push_str(&foreign.verbatim);
        return;
    }
    let local = node.local_name();
    let name = xml::qualify(node.prefix.as_deref(), &local);
    out.push('<');
    out.push_str(&name);
    for a in &node.attrs {
        out.push(' ');
        out.push_str(&xml::qualify(a.prefix.as_deref(), &a.name));
        out.push_str("=\"");
        xml::escape_into(&a.value, out, true);
        out.push('"');
    }
    if node.children.is_empty() && node.content.is_empty() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    let mut content = node.content.iter().peekable();
    for (i, child) in node.children.iter().enumerate() {
        while let Some((_, c)) = content.next_if(|(at, _)| *at <= i) {
            xml::write_content(c, out);
        }
        write_node(child, out);
    }
    for (_, c) in content {
        xml::write_content(c, out);
    }
    out.push_str("</");
    out.push_str(&name);
    out.push('>');
}

// --- Generated from ogckml23.xsd by tests/fixtures/gen_kml_vocab.py; do not edit by hand. ---

/// Every concrete element KML 2.3 defines: the 168 global, non-abstract
/// elements of `ogckml23.xsd`. KML 2.2's 145 are a subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum KmlElement {
    Address,
    Alias,
    Altitude,
    AltitudeMode,
    AltitudeOffset,
    Angles,
    AnimatedUpdate,
    BalloonStyle,
    BalloonVisibility,
    Begin,
    BgColor,
    BottomFov,
    Camera,
    Change,
    Color,
    ColorMode,
    Cookie,
    Coord,
    Coordinates,
    Create,
    Data,
    DelayedStart,
    Delete,
    Description,
    DisplayMode,
    DisplayName,
    Document,
    DrawOrder,
    Duration,
    East,
    End,
    Expires,
    ExtendedData,
    Extrude,
    Fill,
    FlyTo,
    FlyToMode,
    FlyToView,
    Folder,
    GridOrigin,
    GroundOverlay,
    Heading,
    HorizFov,
    HotSpot,
    Href,
    HttpQuery,
    Icon,
    IconStyle,
    ImagePyramid,
    InnerBoundaryIs,
    Interpolate,
    ItemIcon,
    Key,
    Kml,
    LabelStyle,
    Latitude,
    LatLonAltBox,
    LatLonBox,
    LatLonQuad,
    LeftFov,
    LinearRing,
    LineString,
    LineStyle,
    Link,
    LinkDescription,
    LinkName,
    LinkSnippet,
    ListItemType,
    ListStyle,
    Location,
    Lod,
    Longitude,
    LookAt,
    MaxAltitude,
    MaxFadeExtent,
    MaxHeight,
    MaxLodPixels,
    MaxSessionLength,
    MaxSnippetLines,
    MaxWidth,
    Message,
    Metadata,
    MinAltitude,
    MinFadeExtent,
    MinLodPixels,
    MinRefreshPeriod,
    Model,
    MultiGeometry,
    MultiTrack,
    Name,
    Near,
    NetworkLink,
    NetworkLinkControl,
    North,
    Open,
    Orientation,
    OuterBoundaryIs,
    Outline,
    OverlayXY,
    Pair,
    PhoneNumber,
    PhotoOverlay,
    Placemark,
    Playlist,
    PlayMode,
    Point,
    Polygon,
    PolyStyle,
    Range,
    RefreshInterval,
    RefreshMode,
    RefreshVisibility,
    Region,
    ResourceMap,
    RightFov,
    Roll,
    Rotation,
    RotationXY,
    /// `<Scale>`: `<Model>`'s three-axis scale, beside the sub-styles' `<scale>`.
    ScaleUpper,
    Scale,
    Schema,
    SchemaData,
    ScreenOverlay,
    ScreenXY,
    SeaFloorAltitudeMode,
    Shape,
    SimpleArrayData,
    SimpleArrayField,
    SimpleData,
    SimpleField,
    Size,
    /// `<Snippet>`: the feature summary's capitalised spelling, beside `<snippet>`.
    SnippetUpper,
    Snippet,
    SoundCue,
    SourceHref,
    South,
    State,
    Style,
    StyleMap,
    StyleUrl,
    TargetHref,
    Tessellate,
    Text,
    TextColor,
    TileSize,
    Tilt,
    TimeSpan,
    TimeStamp,
    TopFov,
    Tour,
    TourControl,
    Track,
    Update,
    Url,
    Value,
    ViewBoundScale,
    ViewFormat,
    ViewRefreshMode,
    ViewRefreshTime,
    ViewVolume,
    Visibility,
    Wait,
    West,
    When,
    Width,
    X,
    Y,
    Z,
}

/// `(local name, element)`, sorted bytewise for [`KmlElement::from_name`].
const NAMES: [(&str, KmlElement); 168] = [
    ("Alias", KmlElement::Alias),
    ("AnimatedUpdate", KmlElement::AnimatedUpdate),
    ("BalloonStyle", KmlElement::BalloonStyle),
    ("Camera", KmlElement::Camera),
    ("Change", KmlElement::Change),
    ("Create", KmlElement::Create),
    ("Data", KmlElement::Data),
    ("Delete", KmlElement::Delete),
    ("Document", KmlElement::Document),
    ("ExtendedData", KmlElement::ExtendedData),
    ("FlyTo", KmlElement::FlyTo),
    ("Folder", KmlElement::Folder),
    ("GroundOverlay", KmlElement::GroundOverlay),
    ("Icon", KmlElement::Icon),
    ("IconStyle", KmlElement::IconStyle),
    ("ImagePyramid", KmlElement::ImagePyramid),
    ("ItemIcon", KmlElement::ItemIcon),
    ("LabelStyle", KmlElement::LabelStyle),
    ("LatLonAltBox", KmlElement::LatLonAltBox),
    ("LatLonBox", KmlElement::LatLonBox),
    ("LatLonQuad", KmlElement::LatLonQuad),
    ("LineString", KmlElement::LineString),
    ("LineStyle", KmlElement::LineStyle),
    ("LinearRing", KmlElement::LinearRing),
    ("Link", KmlElement::Link),
    ("ListStyle", KmlElement::ListStyle),
    ("Location", KmlElement::Location),
    ("Lod", KmlElement::Lod),
    ("LookAt", KmlElement::LookAt),
    ("Metadata", KmlElement::Metadata),
    ("Model", KmlElement::Model),
    ("MultiGeometry", KmlElement::MultiGeometry),
    ("MultiTrack", KmlElement::MultiTrack),
    ("NetworkLink", KmlElement::NetworkLink),
    ("NetworkLinkControl", KmlElement::NetworkLinkControl),
    ("Orientation", KmlElement::Orientation),
    ("Pair", KmlElement::Pair),
    ("PhotoOverlay", KmlElement::PhotoOverlay),
    ("Placemark", KmlElement::Placemark),
    ("Playlist", KmlElement::Playlist),
    ("Point", KmlElement::Point),
    ("PolyStyle", KmlElement::PolyStyle),
    ("Polygon", KmlElement::Polygon),
    ("Region", KmlElement::Region),
    ("ResourceMap", KmlElement::ResourceMap),
    ("Scale", KmlElement::ScaleUpper),
    ("Schema", KmlElement::Schema),
    ("SchemaData", KmlElement::SchemaData),
    ("ScreenOverlay", KmlElement::ScreenOverlay),
    ("SimpleArrayData", KmlElement::SimpleArrayData),
    ("SimpleArrayField", KmlElement::SimpleArrayField),
    ("SimpleData", KmlElement::SimpleData),
    ("SimpleField", KmlElement::SimpleField),
    ("Snippet", KmlElement::SnippetUpper),
    ("SoundCue", KmlElement::SoundCue),
    ("Style", KmlElement::Style),
    ("StyleMap", KmlElement::StyleMap),
    ("TimeSpan", KmlElement::TimeSpan),
    ("TimeStamp", KmlElement::TimeStamp),
    ("Tour", KmlElement::Tour),
    ("TourControl", KmlElement::TourControl),
    ("Track", KmlElement::Track),
    ("Update", KmlElement::Update),
    ("Url", KmlElement::Url),
    ("ViewVolume", KmlElement::ViewVolume),
    ("Wait", KmlElement::Wait),
    ("address", KmlElement::Address),
    ("altitude", KmlElement::Altitude),
    ("altitudeMode", KmlElement::AltitudeMode),
    ("altitudeOffset", KmlElement::AltitudeOffset),
    ("angles", KmlElement::Angles),
    ("balloonVisibility", KmlElement::BalloonVisibility),
    ("begin", KmlElement::Begin),
    ("bgColor", KmlElement::BgColor),
    ("bottomFov", KmlElement::BottomFov),
    ("color", KmlElement::Color),
    ("colorMode", KmlElement::ColorMode),
    ("cookie", KmlElement::Cookie),
    ("coord", KmlElement::Coord),
    ("coordinates", KmlElement::Coordinates),
    ("delayedStart", KmlElement::DelayedStart),
    ("description", KmlElement::Description),
    ("displayMode", KmlElement::DisplayMode),
    ("displayName", KmlElement::DisplayName),
    ("drawOrder", KmlElement::DrawOrder),
    ("duration", KmlElement::Duration),
    ("east", KmlElement::East),
    ("end", KmlElement::End),
    ("expires", KmlElement::Expires),
    ("extrude", KmlElement::Extrude),
    ("fill", KmlElement::Fill),
    ("flyToMode", KmlElement::FlyToMode),
    ("flyToView", KmlElement::FlyToView),
    ("gridOrigin", KmlElement::GridOrigin),
    ("heading", KmlElement::Heading),
    ("horizFov", KmlElement::HorizFov),
    ("hotSpot", KmlElement::HotSpot),
    ("href", KmlElement::Href),
    ("httpQuery", KmlElement::HttpQuery),
    ("innerBoundaryIs", KmlElement::InnerBoundaryIs),
    ("interpolate", KmlElement::Interpolate),
    ("key", KmlElement::Key),
    ("kml", KmlElement::Kml),
    ("latitude", KmlElement::Latitude),
    ("leftFov", KmlElement::LeftFov),
    ("linkDescription", KmlElement::LinkDescription),
    ("linkName", KmlElement::LinkName),
    ("linkSnippet", KmlElement::LinkSnippet),
    ("listItemType", KmlElement::ListItemType),
    ("longitude", KmlElement::Longitude),
    ("maxAltitude", KmlElement::MaxAltitude),
    ("maxFadeExtent", KmlElement::MaxFadeExtent),
    ("maxHeight", KmlElement::MaxHeight),
    ("maxLodPixels", KmlElement::MaxLodPixels),
    ("maxSessionLength", KmlElement::MaxSessionLength),
    ("maxSnippetLines", KmlElement::MaxSnippetLines),
    ("maxWidth", KmlElement::MaxWidth),
    ("message", KmlElement::Message),
    ("minAltitude", KmlElement::MinAltitude),
    ("minFadeExtent", KmlElement::MinFadeExtent),
    ("minLodPixels", KmlElement::MinLodPixels),
    ("minRefreshPeriod", KmlElement::MinRefreshPeriod),
    ("name", KmlElement::Name),
    ("near", KmlElement::Near),
    ("north", KmlElement::North),
    ("open", KmlElement::Open),
    ("outerBoundaryIs", KmlElement::OuterBoundaryIs),
    ("outline", KmlElement::Outline),
    ("overlayXY", KmlElement::OverlayXY),
    ("phoneNumber", KmlElement::PhoneNumber),
    ("playMode", KmlElement::PlayMode),
    ("range", KmlElement::Range),
    ("refreshInterval", KmlElement::RefreshInterval),
    ("refreshMode", KmlElement::RefreshMode),
    ("refreshVisibility", KmlElement::RefreshVisibility),
    ("rightFov", KmlElement::RightFov),
    ("roll", KmlElement::Roll),
    ("rotation", KmlElement::Rotation),
    ("rotationXY", KmlElement::RotationXY),
    ("scale", KmlElement::Scale),
    ("screenXY", KmlElement::ScreenXY),
    ("seaFloorAltitudeMode", KmlElement::SeaFloorAltitudeMode),
    ("shape", KmlElement::Shape),
    ("size", KmlElement::Size),
    ("snippet", KmlElement::Snippet),
    ("sourceHref", KmlElement::SourceHref),
    ("south", KmlElement::South),
    ("state", KmlElement::State),
    ("styleUrl", KmlElement::StyleUrl),
    ("targetHref", KmlElement::TargetHref),
    ("tessellate", KmlElement::Tessellate),
    ("text", KmlElement::Text),
    ("textColor", KmlElement::TextColor),
    ("tileSize", KmlElement::TileSize),
    ("tilt", KmlElement::Tilt),
    ("topFov", KmlElement::TopFov),
    ("value", KmlElement::Value),
    ("viewBoundScale", KmlElement::ViewBoundScale),
    ("viewFormat", KmlElement::ViewFormat),
    ("viewRefreshMode", KmlElement::ViewRefreshMode),
    ("viewRefreshTime", KmlElement::ViewRefreshTime),
    ("visibility", KmlElement::Visibility),
    ("west", KmlElement::West),
    ("when", KmlElement::When),
    ("width", KmlElement::Width),
    ("x", KmlElement::X),
    ("y", KmlElement::Y),
    ("z", KmlElement::Z),
];

// Every group predicate is generated, whether or not the model reads it yet.
#[allow(dead_code)]
impl KmlElement {
    /// The element a KML-namespace local name denotes, if 2.3 defines one.
    pub fn from_name(name: &str) -> Option<KmlElement> {
        NAMES.binary_search_by(|(n, _)| (*n).cmp(name)).ok().map(|i| NAMES[i].1)
    }

    /// The local name, as the KML namespace spells it.
    pub fn name(self) -> &'static str {
        match self {
            KmlElement::Address => "address",
            KmlElement::Alias => "Alias",
            KmlElement::Altitude => "altitude",
            KmlElement::AltitudeMode => "altitudeMode",
            KmlElement::AltitudeOffset => "altitudeOffset",
            KmlElement::Angles => "angles",
            KmlElement::AnimatedUpdate => "AnimatedUpdate",
            KmlElement::BalloonStyle => "BalloonStyle",
            KmlElement::BalloonVisibility => "balloonVisibility",
            KmlElement::Begin => "begin",
            KmlElement::BgColor => "bgColor",
            KmlElement::BottomFov => "bottomFov",
            KmlElement::Camera => "Camera",
            KmlElement::Change => "Change",
            KmlElement::Color => "color",
            KmlElement::ColorMode => "colorMode",
            KmlElement::Cookie => "cookie",
            KmlElement::Coord => "coord",
            KmlElement::Coordinates => "coordinates",
            KmlElement::Create => "Create",
            KmlElement::Data => "Data",
            KmlElement::DelayedStart => "delayedStart",
            KmlElement::Delete => "Delete",
            KmlElement::Description => "description",
            KmlElement::DisplayMode => "displayMode",
            KmlElement::DisplayName => "displayName",
            KmlElement::Document => "Document",
            KmlElement::DrawOrder => "drawOrder",
            KmlElement::Duration => "duration",
            KmlElement::East => "east",
            KmlElement::End => "end",
            KmlElement::Expires => "expires",
            KmlElement::ExtendedData => "ExtendedData",
            KmlElement::Extrude => "extrude",
            KmlElement::Fill => "fill",
            KmlElement::FlyTo => "FlyTo",
            KmlElement::FlyToMode => "flyToMode",
            KmlElement::FlyToView => "flyToView",
            KmlElement::Folder => "Folder",
            KmlElement::GridOrigin => "gridOrigin",
            KmlElement::GroundOverlay => "GroundOverlay",
            KmlElement::Heading => "heading",
            KmlElement::HorizFov => "horizFov",
            KmlElement::HotSpot => "hotSpot",
            KmlElement::Href => "href",
            KmlElement::HttpQuery => "httpQuery",
            KmlElement::Icon => "Icon",
            KmlElement::IconStyle => "IconStyle",
            KmlElement::ImagePyramid => "ImagePyramid",
            KmlElement::InnerBoundaryIs => "innerBoundaryIs",
            KmlElement::Interpolate => "interpolate",
            KmlElement::ItemIcon => "ItemIcon",
            KmlElement::Key => "key",
            KmlElement::Kml => "kml",
            KmlElement::LabelStyle => "LabelStyle",
            KmlElement::Latitude => "latitude",
            KmlElement::LatLonAltBox => "LatLonAltBox",
            KmlElement::LatLonBox => "LatLonBox",
            KmlElement::LatLonQuad => "LatLonQuad",
            KmlElement::LeftFov => "leftFov",
            KmlElement::LinearRing => "LinearRing",
            KmlElement::LineString => "LineString",
            KmlElement::LineStyle => "LineStyle",
            KmlElement::Link => "Link",
            KmlElement::LinkDescription => "linkDescription",
            KmlElement::LinkName => "linkName",
            KmlElement::LinkSnippet => "linkSnippet",
            KmlElement::ListItemType => "listItemType",
            KmlElement::ListStyle => "ListStyle",
            KmlElement::Location => "Location",
            KmlElement::Lod => "Lod",
            KmlElement::Longitude => "longitude",
            KmlElement::LookAt => "LookAt",
            KmlElement::MaxAltitude => "maxAltitude",
            KmlElement::MaxFadeExtent => "maxFadeExtent",
            KmlElement::MaxHeight => "maxHeight",
            KmlElement::MaxLodPixels => "maxLodPixels",
            KmlElement::MaxSessionLength => "maxSessionLength",
            KmlElement::MaxSnippetLines => "maxSnippetLines",
            KmlElement::MaxWidth => "maxWidth",
            KmlElement::Message => "message",
            KmlElement::Metadata => "Metadata",
            KmlElement::MinAltitude => "minAltitude",
            KmlElement::MinFadeExtent => "minFadeExtent",
            KmlElement::MinLodPixels => "minLodPixels",
            KmlElement::MinRefreshPeriod => "minRefreshPeriod",
            KmlElement::Model => "Model",
            KmlElement::MultiGeometry => "MultiGeometry",
            KmlElement::MultiTrack => "MultiTrack",
            KmlElement::Name => "name",
            KmlElement::Near => "near",
            KmlElement::NetworkLink => "NetworkLink",
            KmlElement::NetworkLinkControl => "NetworkLinkControl",
            KmlElement::North => "north",
            KmlElement::Open => "open",
            KmlElement::Orientation => "Orientation",
            KmlElement::OuterBoundaryIs => "outerBoundaryIs",
            KmlElement::Outline => "outline",
            KmlElement::OverlayXY => "overlayXY",
            KmlElement::Pair => "Pair",
            KmlElement::PhoneNumber => "phoneNumber",
            KmlElement::PhotoOverlay => "PhotoOverlay",
            KmlElement::Placemark => "Placemark",
            KmlElement::Playlist => "Playlist",
            KmlElement::PlayMode => "playMode",
            KmlElement::Point => "Point",
            KmlElement::Polygon => "Polygon",
            KmlElement::PolyStyle => "PolyStyle",
            KmlElement::Range => "range",
            KmlElement::RefreshInterval => "refreshInterval",
            KmlElement::RefreshMode => "refreshMode",
            KmlElement::RefreshVisibility => "refreshVisibility",
            KmlElement::Region => "Region",
            KmlElement::ResourceMap => "ResourceMap",
            KmlElement::RightFov => "rightFov",
            KmlElement::Roll => "roll",
            KmlElement::Rotation => "rotation",
            KmlElement::RotationXY => "rotationXY",
            KmlElement::ScaleUpper => "Scale",
            KmlElement::Scale => "scale",
            KmlElement::Schema => "Schema",
            KmlElement::SchemaData => "SchemaData",
            KmlElement::ScreenOverlay => "ScreenOverlay",
            KmlElement::ScreenXY => "screenXY",
            KmlElement::SeaFloorAltitudeMode => "seaFloorAltitudeMode",
            KmlElement::Shape => "shape",
            KmlElement::SimpleArrayData => "SimpleArrayData",
            KmlElement::SimpleArrayField => "SimpleArrayField",
            KmlElement::SimpleData => "SimpleData",
            KmlElement::SimpleField => "SimpleField",
            KmlElement::Size => "size",
            KmlElement::SnippetUpper => "Snippet",
            KmlElement::Snippet => "snippet",
            KmlElement::SoundCue => "SoundCue",
            KmlElement::SourceHref => "sourceHref",
            KmlElement::South => "south",
            KmlElement::State => "state",
            KmlElement::Style => "Style",
            KmlElement::StyleMap => "StyleMap",
            KmlElement::StyleUrl => "styleUrl",
            KmlElement::TargetHref => "targetHref",
            KmlElement::Tessellate => "tessellate",
            KmlElement::Text => "text",
            KmlElement::TextColor => "textColor",
            KmlElement::TileSize => "tileSize",
            KmlElement::Tilt => "tilt",
            KmlElement::TimeSpan => "TimeSpan",
            KmlElement::TimeStamp => "TimeStamp",
            KmlElement::TopFov => "topFov",
            KmlElement::Tour => "Tour",
            KmlElement::TourControl => "TourControl",
            KmlElement::Track => "Track",
            KmlElement::Update => "Update",
            KmlElement::Url => "Url",
            KmlElement::Value => "value",
            KmlElement::ViewBoundScale => "viewBoundScale",
            KmlElement::ViewFormat => "viewFormat",
            KmlElement::ViewRefreshMode => "viewRefreshMode",
            KmlElement::ViewRefreshTime => "viewRefreshTime",
            KmlElement::ViewVolume => "ViewVolume",
            KmlElement::Visibility => "visibility",
            KmlElement::Wait => "Wait",
            KmlElement::West => "west",
            KmlElement::When => "when",
            KmlElement::Width => "width",
            KmlElement::X => "x",
            KmlElement::Y => "y",
            KmlElement::Z => "z",
        }
    }

    /// Whether this substitutes for `kml:AbstractGeometryGroup`.
    pub fn is_geometry(self) -> bool {
        matches!(
            self,
            KmlElement::LinearRing
                | KmlElement::LineString
                | KmlElement::Model
                | KmlElement::MultiGeometry
                | KmlElement::MultiTrack
                | KmlElement::Point
                | KmlElement::Polygon
                | KmlElement::Track
        )
    }

    /// Whether this substitutes for `kml:AbstractFeatureGroup`.
    pub fn is_feature(self) -> bool {
        matches!(
            self,
            KmlElement::Document
                | KmlElement::Folder
                | KmlElement::GroundOverlay
                | KmlElement::NetworkLink
                | KmlElement::PhotoOverlay
                | KmlElement::Placemark
                | KmlElement::ScreenOverlay
                | KmlElement::Tour
        )
    }

    /// Whether this substitutes for `kml:AbstractContainerGroup`.
    pub fn is_container(self) -> bool {
        matches!(
            self,
            KmlElement::Document
                | KmlElement::Folder
        )
    }

    /// Whether this substitutes for `kml:AbstractOverlayGroup`.
    pub fn is_overlay(self) -> bool {
        matches!(
            self,
            KmlElement::GroundOverlay
                | KmlElement::PhotoOverlay
                | KmlElement::ScreenOverlay
        )
    }

    /// Whether this substitutes for `kml:AbstractStyleSelectorGroup`.
    pub fn is_style_selector(self) -> bool {
        matches!(
            self,
            KmlElement::Style
                | KmlElement::StyleMap
        )
    }

    /// Whether this substitutes for `kml:AbstractSubStyleGroup`.
    pub fn is_sub_style(self) -> bool {
        matches!(
            self,
            KmlElement::BalloonStyle
                | KmlElement::IconStyle
                | KmlElement::LabelStyle
                | KmlElement::LineStyle
                | KmlElement::ListStyle
                | KmlElement::PolyStyle
        )
    }

    /// Whether this substitutes for `kml:AbstractTimePrimitiveGroup`.
    pub fn is_time_primitive(self) -> bool {
        matches!(
            self,
            KmlElement::TimeSpan
                | KmlElement::TimeStamp
        )
    }

    /// Whether this substitutes for `kml:AbstractViewGroup`.
    pub fn is_view(self) -> bool {
        matches!(
            self,
            KmlElement::Camera
                | KmlElement::LookAt
        )
    }

    /// Whether this substitutes for `kml:AbstractTourPrimitiveGroup`.
    pub fn is_tour_primitive(self) -> bool {
        matches!(
            self,
            KmlElement::AnimatedUpdate
                | KmlElement::FlyTo
                | KmlElement::SoundCue
                | KmlElement::TourControl
                | KmlElement::Wait
        )
    }
}
// --- End of generated code. ---

#[cfg(test)]
mod tests {
    use super::*;

    fn raised(source: &str) -> KmlDocument {
        parse(source).unwrap()
    }

    /// D-K5's invariant at the XML level: what the model writes parses to the
    /// document it was raised from, and raises to the same model.
    fn assert_round_trips(source: &str) {
        let model = parse(source).unwrap();
        let written = write(&model);
        assert_eq!(xml::parse_document(&written).unwrap(), xml::parse_document(source).unwrap(), "written:\n{written}");
        assert_eq!(parse(&written).unwrap(), model);
    }

    #[test]
    fn the_vocabulary_round_trips_its_own_names() {
        for (name, element) in NAMES {
            assert_eq!(KmlElement::from_name(name), Some(element));
            assert_eq!(element.name(), name);
        }
        assert_eq!(KmlElement::from_name("Bogus"), None);
        assert_eq!(KmlElement::from_name("snippet"), Some(KmlElement::Snippet));
        assert_eq!(KmlElement::from_name("Snippet"), Some(KmlElement::SnippetUpper));
    }

    #[test]
    fn the_groups_are_the_schemas() {
        let geometry: Vec<_> = NAMES.iter().filter(|(_, e)| e.is_geometry()).map(|(n, _)| *n).collect();
        assert_eq!(geometry, ["LineString", "LinearRing", "Model", "MultiGeometry", "MultiTrack", "Point", "Polygon", "Track"]);
        let features: Vec<_> = NAMES.iter().filter(|(_, e)| e.is_feature()).map(|(n, _)| *n).collect();
        assert_eq!(
            features,
            ["Document", "Folder", "GroundOverlay", "NetworkLink", "PhotoOverlay", "Placemark", "ScreenOverlay", "Tour"]
        );
        assert!(KmlElement::Document.is_container() && !KmlElement::Placemark.is_container());
    }

    #[test]
    fn every_adopted_gx_name_maps_both_ways() {
        for (name, element) in GX_ADOPTED {
            assert_eq!(KmlElement::from_gx_name(name), Some(element));
            assert_eq!(element.gx_name(), Some(name));
        }
        assert_eq!(KmlElement::from_gx_name("altitudeMode"), Some(KmlElement::SeaFloorAltitudeMode));
    }

    #[test]
    fn shared_local_names_that_mean_different_things_are_not_adopted() {
        // The trap § WHAT THE SCHEMAS SAY names: keyed on local name alone,
        // gx:drawOrder on a line string would become a kml:drawOrder no line
        // string may hold.
        for name in ["drawOrder", "x", "y", "ViewerOptions", "outerColor"] {
            assert_eq!(KmlElement::from_gx_name(name), None, "{name}");
        }
    }

    const NS: &str = r#"xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2" xmlns:x="urn:x""#;

    #[test]
    fn elements_are_identified_with_their_spelling() {
        let doc = raised(&format!(
            "<kml {NS}><Placemark><gx:Track/><Track/><gx:altitudeMode>clampToSeaFloor</gx:altitudeMode></Placemark></kml>"
        ));
        assert!(doc.root.is(KmlElement::Kml));
        let placemark = doc.root.child(KmlElement::Placemark).unwrap();
        let spellings: Vec<_> = placemark
            .children
            .iter()
            .map(|c| match c.kind {
                Kind::Kml { element, spelling } => (element, spelling),
                ref other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            spellings,
            [
                (KmlElement::Track, Spelling::Gx),
                (KmlElement::Track, Spelling::Kml),
                (KmlElement::SeaFloorAltitudeMode, Spelling::Gx),
            ]
        );
    }

    #[test]
    fn what_is_not_kml_is_carried_as_its_own_bytes() {
        let doc = raised(&format!(
            "<kml {NS}><Placemark><x:thing  a='1'><x:inner/></x:thing><gx:drawOrder>2</gx:drawOrder><Bogus/></Placemark></kml>"
        ));
        let placemark = doc.root.child(KmlElement::Placemark).unwrap();
        let foreign: Vec<_> = placemark
            .children
            .iter()
            .map(|c| match &c.kind {
                Kind::Foreign(f) => (f.qualified_name.as_str(), f.verbatim.as_str(), f.in_kml_namespace),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            foreign,
            [
                ("x:thing", "<x:thing  a='1'><x:inner/></x:thing>", false),
                ("gx:drawOrder", "<gx:drawOrder>2</gx:drawOrder>", false),
                ("Bogus", "<Bogus/>", true),
            ]
        );
    }

    #[test]
    fn a_hand_built_document_round_trips() {
        assert_round_trips(&format!(
            "<!--head--><kml {NS} version=\"2.3\"><!--a--><Document id=\"d\"><name>A &amp; B</name>\
             <Placemark x:attr=\"1\"><gx:Track><when>2026-01-01T00:00:00Z</when><gx:coord>1 2 3</gx:coord></gx:Track>\
             <x:ext><x:deep k=\"v\">text</x:deep></x:ext><?app keep?></Placemark></Document></kml><!--tail-->"
        ));
    }

    #[test]
    fn every_kml_fixture_round_trips() {
        for source in [
            include_str!("../../tests/fixtures/sample.kml"),
            include_str!("../../tests/fixtures/duckdb_geoms.kml"),
            include_str!("../../tests/fixtures/hand_hostile.kml"),
            include_str!("../../tests/fixtures/hand_hostile_2_3.kml"),
        ] {
            assert_round_trips(source);
        }
    }

    #[test]
    fn every_kml_entry_of_the_gdal_kmz_round_trips() {
        let entries = crate::zip::read(include_bytes!("../../tests/fixtures/gdal_networklink.kmz")).unwrap();
        let mut n = 0;
        for entry in entries.iter().filter(|e| e.name.ends_with(".kml")) {
            assert_round_trips(&xml::decode(&entry.data).unwrap());
            n += 1;
        }
        assert!(n > 1, "the fixture is a layered archive");
    }

    /// The vocabulary and the adoption table against the schemas themselves.
    /// `#[ignore]`d: the schemas are not in the tree (`plans/kml-standard.org`
    /// § ORACLES). Run with `GEOSETTA_KML_SCHEMAS=<dir>`, the directory holding
    /// `2.3/ogckml23.xsd` and `kml22gx.xsd`.
    #[test]
    #[ignore]
    fn the_vocabulary_is_the_schemas() {
        let dir = std::env::var("GEOSETTA_KML_SCHEMAS").expect(
            "set GEOSETTA_KML_SCHEMAS to a directory holding 2.3/ogckml23.xsd \
             (https://schemas.opengis.net/kml/2.3/ogckml23.xsd) and kml22gx.xsd \
             (https://developers.google.com/kml/schema/kml22gx.xsd)",
        );
        let globals = |path: &str| -> Vec<(String, bool)> {
            let bytes = std::fs::read(format!("{dir}/{path}")).unwrap_or_else(|e| panic!("{dir}/{path}: {e}"));
            let schema = xml::parse(&xml::decode(&bytes).unwrap()).unwrap();
            schema
                .children
                .iter()
                .filter(|c| c.name == "element" && c.in_ns("http://www.w3.org/2001/XMLSchema"))
                .map(|c| (c.attr("name").unwrap().to_string(), c.attr("abstract") == Some("true")))
                .collect()
        };
        let mut concrete: Vec<String> =
            globals("2.3/ogckml23.xsd").into_iter().filter(|(_, a)| !a).map(|(n, _)| n).collect();
        concrete.sort();
        let ours: Vec<String> = NAMES.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(ours, concrete, "regenerate with tests/fixtures/gen_kml_vocab.py");
        let gx: Vec<String> = globals("kml22gx.xsd").into_iter().filter(|(_, a)| !a).map(|(n, _)| n).collect();
        for (name, _) in GX_ADOPTED {
            assert!(gx.iter().any(|g| g == name), "gx:{name} is not in kml22gx.xsd");
        }
    }
}
