//! Changes a raised KML 2.3 document to KML 2.2 (`plans/kml-standard.org`
//! K5, D-K7, D-K10).
//!
//! *A respelling and a reorder.* Each element 2.3 adopted from `gx:` goes
//! back to its `gx:` spelling wherever 2.2 has it only there — and only
//! there: `<TimeStamp>` is `kml:` on a feature and `gx:` in a view, `<value>`
//! `kml:` in `<Data>` and `gx:` in `<SimpleArrayData>`, so the ruling is keyed
//! on the parent, as § WHAT THE SCHEMAS SAY requires. Then every element's
//! children are put in the sequence `ogckml22.xsd` declares, which 2.3's
//! unordered groups never required of the source: [`order_2_2`] is that
//! sequence, generated from the schemas. A track's `<when>`s, `<gx:coord>`s
//! and `<gx:angles>` come out grouped by the same rule.
//!
//! *What 2.2 has no place for is dropped, and reported* (D-K7's warning 2):
//! a geometry in `<Create>` or `<Delete>`, a turned `<LatLonBox>` or a
//! `<LatLonQuad>` in a `<Region>`, a `<LatLonAltBox>` on a ground overlay,
//! and the `uom` attribute. Nothing else in 2.3 lacks a 2.2 or `gx:` form
//! (`every_element_has_a_2_2_spelling`). An unturned `<LatLonBox>` in a
//! `<Region>` is the `<LatLonAltBox>` with its edges and no altitudes, so it
//! is written as one and reported as renamed ([`as_alt_box`]) — what 2.3's
//! own assertion on `RegionType` asks for.
//!
//! *Foreign content keeps its neighbour.* An extension element, or a name in
//! the KML namespace 2.3 does not define, constrains nothing the tables know
//! of, so it moves with the element before it — an `<atom:author>` after
//! `<name>` stays after `<name>`. Comments move with the element they
//! precede. An element whose 2.2 type lists no element content (`<Metadata>`,
//! whose content is any) keeps its children's order.
//!
//! The generated region is from `ogckml22.xsd` (SHA-256
//! `67bb8fd82e7098245360f2aeb2afb561a8eb868ec3660dc06542475f67a4b665`) and
//! `kml22gx.xsd` (`d929409bfc6846c8430317c1c31435e65027882dd87610fc99489b71a1ac013e`)
//! by `tests/fixtures/gen_kml22_order.py`.

use std::rc::Rc;

use crate::format::KmlVersion;
use crate::kml::model::{Foreign, KmlDocument, KmlElement, Kind, Node, Spelling, GX_NS, GX_PREFIX, KML_NS};
use crate::xml::{XmlAttr, Content, XMLNS_NS};

/// What a version change changed: each respelling, each element renamed for
/// its successor, and each loss, counted, in the order first met. A lowering
/// and a raising (`crate::kml::raise`) both report in it.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Changes {
    pub respelled: Vec<(KmlElement, Spelling, usize)>,
    pub renamed: Vec<(KmlElement, KmlElement, usize)>,
    pub dropped: Vec<(Dropped, usize)>,
}

/// Something 2.2 has no place for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// An element, in the element it sat in.
    Element { parent: KmlElement, child: KmlElement },
    /// An attribute, on the element that held it.
    Attr { element: KmlElement, attr: &'static str },
}

impl Changes {
    pub(crate) fn rename(&mut self, from: KmlElement, to: KmlElement) {
        match self.renamed.iter_mut().find(|(f, t, _)| *f == from && *t == to) {
            Some((_, _, n)) => *n += 1,
            None => self.renamed.push((from, to, 1)),
        }
    }

    pub(crate) fn respell(&mut self, element: KmlElement, to: Spelling) {
        match self.respelled.iter_mut().find(|(e, s, _)| *e == element && *s == to) {
            Some((_, _, n)) => *n += 1,
            None => self.respelled.push((element, to, 1)),
        }
    }

    fn drop(&mut self, what: Dropped) {
        match self.dropped.iter_mut().find(|(d, _)| *d == what) {
            Some((_, n)) => *n += 1,
            None => self.dropped.push((what, 1)),
        }
    }

    /// D-K7's warning 1, when anything was respelled or renamed: the version
    /// change, with each counted. A change of the version stamp alone is
    /// silent.
    pub fn respelled_warning(&self, from: KmlVersion, to: KmlVersion) -> Option<String> {
        if self.respelled.is_empty() && self.renamed.is_empty() {
            return None;
        }
        let respelled = self.respelled.iter().map(|&(element, to, n)| {
            let from = if to == Spelling::Gx { Spelling::Kml } else { Spelling::Gx };
            format!("{}→{}{}", spelled(element, from), spelled(element, to), count(n))
        });
        let renamed = self.renamed.iter().map(|&(f, t, n)| format!("{}→{}{}", f.name(), t.name(), count(n)));
        let list: Vec<String> = respelled.chain(renamed).collect();
        Some(format!("kml: input is KML {}; written as KML {} ({})", from.as_str(), to.as_str(), list.join(", ")))
    }

    /// D-K7's warning 2, when anything was dropped.
    pub fn dropped_warning(&self) -> Option<String> {
        if self.dropped.is_empty() {
            return None;
        }
        let list: Vec<String> = self
            .dropped
            .iter()
            .map(|&(what, n)| match what {
                Dropped::Element { parent, child } => format!("<{}> in <{}>{}", child.name(), parent.name(), count(n)),
                Dropped::Attr { element, attr } => format!("{attr}= on <{}>{}", element.name(), count(n)),
            })
            .collect();
        Some(format!("kml: KML 2.2 has no place for {}; dropped", list.join(", ")))
    }
}

fn spelled(element: KmlElement, spelling: Spelling) -> String {
    match spelling {
        Spelling::Kml => element.name().to_string(),
        Spelling::Gx => format!("gx:{}", element.gx_name().expect("a gx spelling is only given to an adopted element")),
    }
}

pub(crate) fn count(n: usize) -> String {
    if n > 1 { format!(" ×{n}") } else { String::new() }
}

/// Where 2.2 puts a child.
#[derive(Debug, PartialEq)]
enum Place {
    /// In its parent's sequence, at this slot, in this spelling.
    Slot(Spelling, u8),
    /// The parent's 2.2 type lists no element content, so it orders nothing:
    /// in this spelling, where it is.
    Anywhere(Spelling),
    /// Nowhere.
    Nowhere,
}

/// What 2.2 makes of `element`, spelled `spelling`, under `parent`. The
/// spelling it has is kept when 2.2 admits it there.
fn place(parent: (KmlElement, Spelling), element: KmlElement, spelling: Spelling) -> Place {
    let table = order_2_2(parent.0, parent.1);
    if table.is_empty() {
        let kml = in_2_2(element);
        let gx = element.gx_name().is_some();
        return match spelling {
            Spelling::Kml if kml => Place::Anywhere(Spelling::Kml),
            Spelling::Gx if gx => Place::Anywhere(Spelling::Gx),
            _ if kml => Place::Anywhere(Spelling::Kml),
            _ if gx => Place::Anywhere(Spelling::Gx),
            _ => Place::Nowhere,
        };
    }
    let mut admitted = table.iter().filter(|(e, _, _)| *e == element);
    let first = admitted.clone().next();
    match admitted.find(|(_, s, _)| *s == spelling).or(first) {
        Some(&(_, s, slot)) => Place::Slot(s, slot),
        None => Place::Nowhere,
    }
}

/// Whether `node`, among `siblings` under `parent`, is a `<Region>`'s
/// `<LatLonBox>` that 2.2 holds as the `<LatLonAltBox>` it equals: unturned,
/// and the region's only box. Its edges carry over; an altitude box with
/// none is on the ground, as a `<LatLonBox>` is.
fn as_alt_box(parent: KmlElement, node: &Node, siblings: &[Node]) -> bool {
    use KmlElement as K;
    let boxes = siblings.iter().filter(|s| matches!(s.element(), Some(K::LatLonBox | K::LatLonAltBox | K::LatLonQuad))).count();
    parent == K::Region && node.element() == Some(K::LatLonBox) && boxes == 1 && node.children.iter().all(|c| !is_turn(c))
}

/// Whether `node` is a `<rotation>` that turns its box. An empty one is
/// the schema's default, 0.
fn is_turn(node: &Node) -> bool {
    node.element() == Some(KmlElement::Rotation)
        && !node.text_trimmed().is_empty()
        && !node.text_trimmed().parse::<f64>().is_ok_and(|r| r == 0.0)
}

/// The attributes 2.3 added: `uom` on `<SimpleField>` and `<Data>`.
fn attr_2_3_only(element: KmlElement, attr: &XmlAttr) -> Option<&'static str> {
    (attr.ns.is_none() && attr.name == "uom" && matches!(element, KmlElement::SimpleField | KmlElement::Data)).then_some("uom")
}

/// OGC's published schema locations, 2.3's and 2.2's.
pub(crate) const SCHEMA_2_3: &str = "http://schemas.opengis.net/kml/2.3/ogckml23.xsd";
pub(crate) const SCHEMA_2_2: &str = "http://schemas.opengis.net/kml/2.2.0/ogckml22.xsd";
pub(crate) const XSI_NS: &str = "http://www.w3.org/2001/XMLSchema-instance";

/// Lower `doc`, a 2.3 document, to 2.2 in place.
pub(crate) fn lower(doc: &mut KmlDocument) -> Changes {
    let mut report = Changes::default();
    let root = &mut doc.root;
    // The root: OGC's namespace, declared when the source declared none; no
    // version stamp, which 2.2's schema has no attribute for; and 2.2's
    // schema where the source named 2.3's.
    root.attrs.retain(|a| !(a.ns.is_none() && a.name == "version"));
    for a in &mut root.attrs {
        if a.ns.as_deref() == Some(XSI_NS) && a.name == "schemaLocation" {
            a.value = a.value.replace(SCHEMA_2_3, SCHEMA_2_2);
        }
    }
    if doc.root_ns.is_none() {
        root.attrs.insert(0, ns_decl(None, KML_NS));
        doc.root_ns = Some(Rc::from(KML_NS));
    }
    let prefixes = Prefixes { kml: root.prefix.clone(), gx: gx_prefix(&root.attrs) };
    let declared = prefixes.gx_declared(&root.attrs);
    let mut scope = Vec::new();
    lower_children(root, (KmlElement::Kml, Spelling::Kml), &prefixes, &mut scope, &mut report);
    let gx_used = report.respelled.iter().any(|(_, s, _)| *s == Spelling::Gx);
    if gx_used && !declared {
        root.attrs.push(ns_decl(Some(&prefixes.gx), GX_NS));
    }
    report
}

/// What [`lower`] would report for `node`, read as a child of `parent`,
/// without changing it — for a warning given before the write.
pub(crate) fn survey(node: &Node, parent: (KmlElement, Spelling), report: &mut Changes) {
    survey_as(node, parent, false, report);
}

/// [`survey`], `node` being written as a `<LatLonAltBox>` when `alt_box`.
fn survey_as(node: &Node, parent: (KmlElement, Spelling), alt_box: bool, report: &mut Changes) {
    let Kind::Kml { element, spelling } = node.kind else { return };
    let (element, spelling) = if alt_box {
        report.rename(element, KmlElement::LatLonAltBox);
        (KmlElement::LatLonAltBox, Spelling::Kml)
    } else {
        (element, spelling)
    };
    let spelling = match place(parent, element, spelling) {
        Place::Nowhere => {
            report.drop(Dropped::Element { parent: parent.0, child: element });
            return;
        }
        Place::Slot(s, _) | Place::Anywhere(s) => {
            if s != spelling {
                report.respell(element, s);
            }
            s
        }
    };
    for a in &node.attrs {
        if let Some(attr) = attr_2_3_only(element, a) {
            report.drop(Dropped::Attr { element, attr });
        }
    }
    for child in &node.children {
        if alt_box && child.element() == Some(KmlElement::Rotation) {
            continue;
        }
        survey_as(child, (element, spelling), as_alt_box(element, child, &node.children), report);
    }
}

/// The prefixes a respelled element is written in.
struct Prefixes {
    kml: Option<String>,
    gx: String,
}

impl Prefixes {
    fn gx_declared(&self, attrs: &[XmlAttr]) -> bool {
        attrs.iter().any(|a| is_decl(a) && a.prefix.is_some() && a.name == self.gx && a.value == GX_NS)
    }
}

pub(crate) fn is_decl(a: &XmlAttr) -> bool {
    a.ns.as_deref() == Some(XMLNS_NS)
}

/// The prefix the root binds to `gx:`'s namespace, else `gx` — or, should
/// the root bind `gx` to something else, a prefix it does not bind.
fn gx_prefix(attrs: &[XmlAttr]) -> String {
    if let Some(a) = attrs.iter().find(|a| is_decl(a) && a.prefix.is_some() && a.value == GX_NS) {
        return a.name.clone();
    }
    let bound = |p: &str| attrs.iter().any(|a| is_decl(a) && a.prefix.is_some() && a.name == p);
    let mut prefix = GX_PREFIX.to_string();
    while bound(&prefix) {
        prefix.push('_');
    }
    prefix
}

pub(crate) fn ns_decl(prefix: Option<&str>, uri: &str) -> XmlAttr {
    let (prefix, name) = match prefix {
        None => (None, "xmlns".to_string()),
        Some(p) => (Some("xmlns".to_string()), p.to_string()),
    };
    XmlAttr { prefix, name, ns: Some(Rc::from(XMLNS_NS)), value: uri.to_string() }
}

/// Lower `node`'s children, `node` being `parent` as 2.2 spells it: respell
/// each, drop what 2.2 has no place for, and put the rest in 2.2's sequence.
/// `scope` holds the prefix bindings declared above `node`, innermost last.
fn lower_children(
    node: &mut Node,
    parent: (KmlElement, Spelling),
    prefixes: &Prefixes,
    scope: &mut Vec<(String, String)>,
    report: &mut Changes,
) {
    let depth = scope.len();
    scope.extend(node.attrs.iter().filter(|a| is_decl(a) && a.prefix.is_some()).map(|a| (a.name.clone(), a.value.clone())));
    let ordered = !order_2_2(parent.0, parent.1).is_empty();
    let children = std::mem::take(&mut node.children);
    let alt_boxes: Vec<bool> = children.iter().map(|c| as_alt_box(parent.0, c, &children)).collect();
    let mut misc = std::mem::take(&mut node.content).into_iter().peekable();
    // Each child with its slot and the comments and whitespace before it. A
    // dropped child's comments go to the child after it; the whitespace that
    // indented it goes with it (`plans/kml-layout.org` D-L4).
    let mut kept: Vec<(u8, Vec<Content>, Node)> = Vec::with_capacity(children.len());
    let mut pending = Vec::new();
    let mut slot = 0;
    for (i, mut child) in children.into_iter().enumerate() {
        while let Some((_, m)) = misc.next_if(|(at, _)| *at <= i) {
            pending.push(m);
        }
        if let Kind::Foreign(foreign) = &child.kind
            && let Some(at) = foreign_slot(parent, foreign, scope)
        {
            slot = at;
        }
        if alt_boxes[i] {
            // Its `<rotation>`, if any, says 0, which an altitude box is.
            while let Some(at) = child.children.iter().position(|c| c.element() == Some(KmlElement::Rotation)) {
                child.remove_child(at);
            }
            report.rename(KmlElement::LatLonBox, KmlElement::LatLonAltBox);
            child.kind = Kind::Kml { element: KmlElement::LatLonAltBox, spelling: Spelling::Kml };
        }
        if let Kind::Kml { element, spelling } = child.kind {
            let to = match place(parent, element, spelling) {
                Place::Nowhere => {
                    report.drop(Dropped::Element { parent: parent.0, child: element });
                    if pending.last().is_some_and(Content::is_space) {
                        pending.pop();
                    }
                    continue;
                }
                Place::Slot(s, at) => {
                    slot = at;
                    s
                }
                Place::Anywhere(s) => s,
            };
            if to != spelling {
                report.respell(element, to);
                child.kind = Kind::Kml { element, spelling: to };
                child.prefix = match to {
                    Spelling::Gx => Some(prefixes.gx.clone()),
                    Spelling::Kml => prefixes.kml.clone(),
                };
            }
            child.attrs.retain(|a| match attr_2_3_only(element, a) {
                Some(attr) => {
                    report.drop(Dropped::Attr { element, attr });
                    false
                }
                None => true,
            });
            lower_children(&mut child, (element, to), prefixes, scope, report);
        }
        kept.push((slot, std::mem::take(&mut pending), child));
    }
    if ordered {
        // Stable: children sharing a slot, and foreign content glued to the
        // child before it, keep their order.
        kept.sort_by_key(|(slot, _, _)| *slot);
    }
    node.children.reserve_exact(kept.len());
    for (_, leading, child) in kept {
        node.content.extend(leading.into_iter().map(|m| (node.children.len(), m)));
        node.children.push(child);
    }
    let end = node.children.len();
    node.content.extend(pending.into_iter().chain(misc.map(|(_, m)| m)).map(|m| (end, m)));
    scope.truncate(depth);
}

/// The slot the schemas give foreign content under `parent`, its prefix
/// resolved through `scope`, or through a declaration on its own start tag.
fn foreign_slot(parent: (KmlElement, Spelling), foreign: &Foreign, scope: &[(String, String)]) -> Option<u8> {
    let (prefix, local) = foreign.qualified_name.split_once(':')?;
    let ns = match scope.iter().rev().find(|(p, _)| p == prefix) {
        Some((_, uri)) => uri.as_str(),
        None => own_declaration(&foreign.verbatim, prefix)?,
    };
    foreign_slot_2_2(parent.0, parent.1, ns, local)
}

/// The namespace `xmlns:{prefix}` binds on `verbatim`'s start tag.
fn own_declaration<'a>(verbatim: &'a str, prefix: &str) -> Option<&'a str> {
    let tag = &verbatim[..verbatim.find('>')?];
    let at = tag.find(&format!("xmlns:{prefix}="))? + "xmlns:=".len() + prefix.len();
    let quote = tag[at..].chars().next()?;
    let value = &tag[at + 1..];
    Some(&value[..value.find(quote)?])
}

// --- Generated from ogckml22.xsd and kml22gx.xsd by tests/fixtures/gen_kml22_order.py; do not edit by hand. ---

/// Every child 2.2 admits under `parent` spelled `spelling`, as
/// `(element, spelling, slot)`: sorted by slot, children are in the
/// sequence `ogckml22.xsd` and `kml22gx.xsd` declare. Empty for an element
/// with no element content, or none in 2.2.
pub(crate) fn order_2_2(parent: KmlElement, spelling: Spelling) -> &'static [(KmlElement, Spelling, u8)] {
    use KmlElement as K;
    use Spelling::{Gx, Kml};
    match (parent, spelling) {
        (K::Alias, Kml) => &[
            (K::TargetHref, Kml, 1),
            (K::SourceHref, Kml, 2),
        ],
        (K::AnimatedUpdate, Gx) => &[
            (K::Duration, Gx, 1),
            (K::Update, Kml, 2),
            (K::DelayedStart, Gx, 3),
        ],
        (K::BalloonStyle, Kml) => &[
            (K::Color, Kml, 3),
            (K::BgColor, Kml, 3),
            (K::TextColor, Kml, 4),
            (K::Text, Kml, 5),
            (K::DisplayMode, Kml, 6),
        ],
        (K::Camera, Kml) => &[
            (K::HorizFov, Gx, 1),
            (K::TimeStamp, Gx, 2),
            (K::TimeSpan, Gx, 2),
            (K::Longitude, Kml, 3),
            (K::Latitude, Kml, 4),
            (K::Altitude, Kml, 5),
            (K::Heading, Kml, 6),
            (K::Tilt, Kml, 7),
            (K::Roll, Kml, 8),
            (K::AltitudeMode, Kml, 9),
            (K::SeaFloorAltitudeMode, Gx, 9),
        ],
        (K::Change, Kml) => &[
            (K::Document, Kml, 0),
            (K::Folder, Kml, 0),
            (K::GroundOverlay, Kml, 0),
            (K::ScreenOverlay, Kml, 0),
            (K::PhotoOverlay, Kml, 0),
            (K::Placemark, Kml, 0),
            (K::NetworkLink, Kml, 0),
            (K::Tour, Gx, 0),
            (K::LookAt, Kml, 0),
            (K::Camera, Kml, 0),
            (K::TimeStamp, Gx, 0),
            (K::TimeSpan, Gx, 0),
            (K::SchemaData, Kml, 0),
            (K::Data, Kml, 0),
            (K::MultiGeometry, Kml, 0),
            (K::Point, Kml, 0),
            (K::LineString, Kml, 0),
            (K::LinearRing, Kml, 0),
            (K::Polygon, Kml, 0),
            (K::Model, Kml, 0),
            (K::Track, Gx, 0),
            (K::MultiTrack, Gx, 0),
            (K::Style, Kml, 0),
            (K::StyleMap, Kml, 0),
            (K::TimeStamp, Kml, 0),
            (K::TimeSpan, Kml, 0),
            (K::Region, Kml, 0),
            (K::LatLonAltBox, Kml, 0),
            (K::Lod, Kml, 0),
            (K::Icon, Kml, 0),
            (K::Link, Kml, 0),
            (K::Url, Kml, 0),
            (K::Location, Kml, 0),
            (K::Orientation, Kml, 0),
            (K::ScaleUpper, Kml, 0),
            (K::ResourceMap, Kml, 0),
            (K::Alias, Kml, 0),
            (K::LatLonQuad, Gx, 0),
            (K::LatLonBox, Kml, 0),
            (K::ViewVolume, Kml, 0),
            (K::ImagePyramid, Kml, 0),
            (K::Pair, Kml, 0),
            (K::IconStyle, Kml, 0),
            (K::LabelStyle, Kml, 0),
            (K::LineStyle, Kml, 0),
            (K::PolyStyle, Kml, 0),
            (K::BalloonStyle, Kml, 0),
            (K::ListStyle, Kml, 0),
            (K::ItemIcon, Kml, 0),
            (K::AnimatedUpdate, Gx, 0),
            (K::FlyTo, Gx, 0),
            (K::SoundCue, Gx, 0),
            (K::TourControl, Gx, 0),
            (K::Wait, Gx, 0),
            (K::Playlist, Gx, 0),
        ],
        (K::Create, Kml) => &[
            (K::Document, Kml, 0),
            (K::Folder, Kml, 0),
        ],
        (K::Data, Kml) => &[
            (K::DisplayName, Kml, 1),
            (K::Value, Kml, 2),
        ],
        (K::Delete, Kml) => &[
            (K::Document, Kml, 0),
            (K::Folder, Kml, 0),
            (K::GroundOverlay, Kml, 0),
            (K::ScreenOverlay, Kml, 0),
            (K::PhotoOverlay, Kml, 0),
            (K::Placemark, Kml, 0),
            (K::NetworkLink, Kml, 0),
            (K::Tour, Gx, 0),
        ],
        (K::Document, Kml) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::Schema, Kml, 21),
            (K::Document, Kml, 22),
            (K::Folder, Kml, 22),
            (K::GroundOverlay, Kml, 22),
            (K::ScreenOverlay, Kml, 22),
            (K::PhotoOverlay, Kml, 22),
            (K::Placemark, Kml, 22),
            (K::NetworkLink, Kml, 22),
            (K::Tour, Gx, 22),
        ],
        (K::ExtendedData, Kml) => &[
            (K::Data, Kml, 0),
            (K::SchemaData, Kml, 1),
        ],
        (K::FlyTo, Gx) => &[
            (K::Duration, Gx, 1),
            (K::FlyToMode, Gx, 2),
            (K::LookAt, Kml, 3),
            (K::Camera, Kml, 3),
        ],
        (K::Folder, Kml) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::Document, Kml, 21),
            (K::Folder, Kml, 21),
            (K::GroundOverlay, Kml, 21),
            (K::ScreenOverlay, Kml, 21),
            (K::PhotoOverlay, Kml, 21),
            (K::Placemark, Kml, 21),
            (K::NetworkLink, Kml, 21),
            (K::Tour, Gx, 21),
        ],
        (K::GroundOverlay, Kml) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::Color, Kml, 19),
            (K::DrawOrder, Kml, 20),
            (K::Icon, Kml, 21),
            (K::Altitude, Kml, 24),
            (K::AltitudeMode, Kml, 25),
            (K::SeaFloorAltitudeMode, Gx, 25),
            (K::LatLonBox, Kml, 26),
            (K::LatLonQuad, Gx, 28),
        ],
        (K::Icon, Kml) => &[
            (K::Href, Kml, 1),
            (K::RefreshMode, Kml, 4),
            (K::RefreshInterval, Kml, 5),
            (K::ViewRefreshMode, Kml, 6),
            (K::ViewRefreshTime, Kml, 7),
            (K::ViewBoundScale, Kml, 8),
            (K::ViewFormat, Kml, 9),
            (K::HttpQuery, Kml, 10),
        ],
        (K::IconStyle, Kml) => &[
            (K::Color, Kml, 3),
            (K::ColorMode, Kml, 4),
            (K::Scale, Kml, 7),
            (K::Heading, Kml, 8),
            (K::Icon, Kml, 9),
            (K::HotSpot, Kml, 10),
        ],
        (K::ImagePyramid, Kml) => &[
            (K::TileSize, Kml, 1),
            (K::MaxWidth, Kml, 2),
            (K::MaxHeight, Kml, 3),
            (K::GridOrigin, Kml, 4),
        ],
        (K::InnerBoundaryIs, Kml) => &[
            (K::LinearRing, Kml, 0),
        ],
        (K::ItemIcon, Kml) => &[
            (K::State, Kml, 1),
            (K::Href, Kml, 2),
        ],
        (K::Kml, Kml) => &[
            (K::NetworkLinkControl, Kml, 0),
            (K::Document, Kml, 1),
            (K::Folder, Kml, 1),
            (K::GroundOverlay, Kml, 1),
            (K::ScreenOverlay, Kml, 1),
            (K::PhotoOverlay, Kml, 1),
            (K::Placemark, Kml, 1),
            (K::NetworkLink, Kml, 1),
            (K::Tour, Gx, 1),
        ],
        (K::LabelStyle, Kml) => &[
            (K::Color, Kml, 3),
            (K::ColorMode, Kml, 4),
            (K::Scale, Kml, 7),
        ],
        (K::LatLonAltBox, Kml) => &[
            (K::North, Kml, 1),
            (K::South, Kml, 2),
            (K::East, Kml, 3),
            (K::West, Kml, 4),
            (K::MinAltitude, Kml, 7),
            (K::MaxAltitude, Kml, 8),
            (K::AltitudeMode, Kml, 9),
            (K::SeaFloorAltitudeMode, Gx, 9),
        ],
        (K::LatLonBox, Kml) => &[
            (K::North, Kml, 1),
            (K::South, Kml, 2),
            (K::East, Kml, 3),
            (K::West, Kml, 4),
            (K::Rotation, Kml, 7),
        ],
        (K::LatLonQuad, Gx) => &[
            (K::Coordinates, Kml, 1),
        ],
        (K::LineString, Kml) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::Extrude, Kml, 3),
            (K::Tessellate, Kml, 4),
            (K::AltitudeMode, Kml, 5),
            (K::SeaFloorAltitudeMode, Gx, 5),
            (K::Coordinates, Kml, 6),
        ],
        (K::LineStyle, Kml) => &[
            (K::Color, Kml, 3),
            (K::ColorMode, Kml, 4),
            (K::Width, Kml, 7),
        ],
        (K::LinearRing, Kml) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::Extrude, Kml, 3),
            (K::Tessellate, Kml, 4),
            (K::AltitudeMode, Kml, 5),
            (K::SeaFloorAltitudeMode, Gx, 5),
            (K::Coordinates, Kml, 6),
        ],
        (K::Link, Kml) => &[
            (K::Href, Kml, 1),
            (K::RefreshMode, Kml, 4),
            (K::RefreshInterval, Kml, 5),
            (K::ViewRefreshMode, Kml, 6),
            (K::ViewRefreshTime, Kml, 7),
            (K::ViewBoundScale, Kml, 8),
            (K::ViewFormat, Kml, 9),
            (K::HttpQuery, Kml, 10),
        ],
        (K::ListStyle, Kml) => &[
            (K::ListItemType, Kml, 3),
            (K::BgColor, Kml, 4),
            (K::ItemIcon, Kml, 5),
            (K::MaxSnippetLines, Kml, 6),
        ],
        (K::Location, Kml) => &[
            (K::Longitude, Kml, 1),
            (K::Latitude, Kml, 2),
            (K::Altitude, Kml, 3),
        ],
        (K::Lod, Kml) => &[
            (K::MinLodPixels, Kml, 1),
            (K::MaxLodPixels, Kml, 2),
            (K::MinFadeExtent, Kml, 3),
            (K::MaxFadeExtent, Kml, 4),
        ],
        (K::LookAt, Kml) => &[
            (K::HorizFov, Gx, 1),
            (K::TimeStamp, Gx, 2),
            (K::TimeSpan, Gx, 2),
            (K::Longitude, Kml, 3),
            (K::Latitude, Kml, 4),
            (K::Altitude, Kml, 5),
            (K::Heading, Kml, 6),
            (K::Tilt, Kml, 7),
            (K::Range, Kml, 8),
            (K::AltitudeMode, Kml, 9),
            (K::SeaFloorAltitudeMode, Gx, 9),
        ],
        (K::Model, Kml) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::AltitudeMode, Kml, 3),
            (K::SeaFloorAltitudeMode, Gx, 3),
            (K::Location, Kml, 4),
            (K::Orientation, Kml, 5),
            (K::ScaleUpper, Kml, 6),
            (K::Link, Kml, 7),
            (K::ResourceMap, Kml, 8),
        ],
        (K::MultiGeometry, Kml) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::MultiGeometry, Kml, 3),
            (K::Point, Kml, 3),
            (K::LineString, Kml, 3),
            (K::LinearRing, Kml, 3),
            (K::Polygon, Kml, 3),
            (K::Model, Kml, 3),
            (K::Track, Gx, 3),
            (K::MultiTrack, Gx, 3),
        ],
        (K::MultiTrack, Gx) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::AltitudeMode, Kml, 3),
            (K::SeaFloorAltitudeMode, Gx, 3),
            (K::Interpolate, Gx, 4),
            (K::Track, Gx, 5),
        ],
        (K::NetworkLink, Kml) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::RefreshVisibility, Kml, 19),
            (K::FlyToView, Kml, 20),
            (K::Url, Kml, 21),
            (K::Link, Kml, 21),
        ],
        (K::NetworkLinkControl, Kml) => &[
            (K::MinRefreshPeriod, Kml, 0),
            (K::MaxSessionLength, Kml, 1),
            (K::Cookie, Kml, 2),
            (K::Message, Kml, 3),
            (K::LinkName, Kml, 4),
            (K::LinkDescription, Kml, 5),
            (K::LinkSnippet, Kml, 6),
            (K::Expires, Kml, 7),
            (K::Update, Kml, 8),
            (K::LookAt, Kml, 9),
            (K::Camera, Kml, 9),
        ],
        (K::Orientation, Kml) => &[
            (K::Heading, Kml, 1),
            (K::Tilt, Kml, 2),
            (K::Roll, Kml, 3),
        ],
        (K::OuterBoundaryIs, Kml) => &[
            (K::LinearRing, Kml, 0),
        ],
        (K::Pair, Kml) => &[
            (K::Key, Kml, 1),
            (K::StyleUrl, Kml, 2),
            (K::Style, Kml, 3),
            (K::StyleMap, Kml, 3),
        ],
        (K::PhotoOverlay, Kml) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::Color, Kml, 19),
            (K::DrawOrder, Kml, 20),
            (K::Icon, Kml, 21),
            (K::Rotation, Kml, 24),
            (K::ViewVolume, Kml, 25),
            (K::ImagePyramid, Kml, 26),
            (K::Point, Kml, 27),
            (K::Shape, Kml, 28),
        ],
        (K::Placemark, Kml) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::MultiGeometry, Kml, 19),
            (K::Point, Kml, 19),
            (K::LineString, Kml, 19),
            (K::LinearRing, Kml, 19),
            (K::Polygon, Kml, 19),
            (K::Model, Kml, 19),
            (K::Track, Gx, 19),
            (K::MultiTrack, Gx, 19),
        ],
        (K::Playlist, Gx) => &[
            (K::AnimatedUpdate, Gx, 1),
            (K::FlyTo, Gx, 1),
            (K::SoundCue, Gx, 1),
            (K::TourControl, Gx, 1),
            (K::Wait, Gx, 1),
        ],
        (K::Point, Kml) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::Extrude, Kml, 3),
            (K::AltitudeMode, Kml, 4),
            (K::SeaFloorAltitudeMode, Gx, 4),
            (K::Coordinates, Kml, 5),
        ],
        (K::PolyStyle, Kml) => &[
            (K::Color, Kml, 3),
            (K::ColorMode, Kml, 4),
            (K::Fill, Kml, 7),
            (K::Outline, Kml, 8),
        ],
        (K::Polygon, Kml) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::Extrude, Kml, 3),
            (K::Tessellate, Kml, 4),
            (K::AltitudeMode, Kml, 5),
            (K::SeaFloorAltitudeMode, Gx, 5),
            (K::OuterBoundaryIs, Kml, 6),
            (K::InnerBoundaryIs, Kml, 7),
        ],
        (K::Region, Kml) => &[
            (K::LatLonAltBox, Kml, 1),
            (K::Lod, Kml, 2),
        ],
        (K::ResourceMap, Kml) => &[
            (K::Alias, Kml, 1),
        ],
        (K::ScaleUpper, Kml) => &[
            (K::X, Kml, 1),
            (K::Y, Kml, 2),
            (K::Z, Kml, 3),
        ],
        (K::Schema, Kml) => &[
            (K::SimpleField, Kml, 0),
            (K::SimpleArrayField, Gx, 1),
        ],
        (K::SchemaData, Kml) => &[
            (K::SimpleData, Kml, 1),
            (K::SimpleArrayData, Gx, 2),
        ],
        (K::ScreenOverlay, Kml) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::Color, Kml, 19),
            (K::DrawOrder, Kml, 20),
            (K::Icon, Kml, 21),
            (K::OverlayXY, Kml, 24),
            (K::ScreenXY, Kml, 25),
            (K::RotationXY, Kml, 26),
            (K::Size, Kml, 27),
            (K::Rotation, Kml, 28),
        ],
        (K::SimpleArrayData, Gx) => &[
            (K::Value, Gx, 1),
        ],
        (K::SimpleArrayField, Gx) => &[
            (K::DisplayName, Kml, 0),
        ],
        (K::SimpleField, Kml) => &[
            (K::DisplayName, Kml, 0),
        ],
        (K::SoundCue, Gx) => &[
            (K::Href, Kml, 1),
            (K::DelayedStart, Gx, 2),
        ],
        (K::Style, Kml) => &[
            (K::IconStyle, Kml, 3),
            (K::LabelStyle, Kml, 4),
            (K::LineStyle, Kml, 5),
            (K::PolyStyle, Kml, 6),
            (K::BalloonStyle, Kml, 7),
            (K::ListStyle, Kml, 8),
        ],
        (K::StyleMap, Kml) => &[
            (K::Pair, Kml, 3),
        ],
        (K::TimeSpan, Gx) => &[
            (K::Begin, Kml, 3),
            (K::End, Kml, 4),
        ],
        (K::TimeSpan, Kml) => &[
            (K::Begin, Kml, 3),
            (K::End, Kml, 4),
        ],
        (K::TimeStamp, Gx) => &[
            (K::When, Kml, 3),
        ],
        (K::TimeStamp, Kml) => &[
            (K::When, Kml, 3),
        ],
        (K::Tour, Gx) => &[
            (K::Name, Kml, 1),
            (K::Visibility, Kml, 2),
            (K::Open, Kml, 3),
            (K::Address, Kml, 6),
            (K::PhoneNumber, Kml, 8),
            (K::SnippetUpper, Kml, 9),
            (K::Snippet, Kml, 9),
            (K::Description, Kml, 10),
            (K::LookAt, Kml, 11),
            (K::Camera, Kml, 11),
            (K::TimeStamp, Kml, 12),
            (K::TimeSpan, Kml, 12),
            (K::StyleUrl, Kml, 13),
            (K::Style, Kml, 14),
            (K::StyleMap, Kml, 14),
            (K::Region, Kml, 15),
            (K::Metadata, Kml, 16),
            (K::ExtendedData, Kml, 16),
            (K::BalloonVisibility, Gx, 17),
            (K::Playlist, Gx, 19),
        ],
        (K::TourControl, Gx) => &[
            (K::PlayMode, Gx, 1),
        ],
        (K::Track, Gx) => &[
            (K::AltitudeOffset, Gx, 1),
            (K::Extrude, Kml, 3),
            (K::Tessellate, Kml, 4),
            (K::AltitudeMode, Kml, 5),
            (K::SeaFloorAltitudeMode, Gx, 5),
            (K::When, Kml, 6),
            (K::Coord, Gx, 7),
            (K::Angles, Gx, 8),
            (K::Model, Kml, 9),
            (K::ExtendedData, Kml, 10),
        ],
        (K::Update, Kml) => &[
            (K::TargetHref, Kml, 0),
            (K::Create, Kml, 1),
            (K::Delete, Kml, 1),
            (K::Change, Kml, 1),
        ],
        (K::Url, Kml) => &[
            (K::Href, Kml, 1),
            (K::RefreshMode, Kml, 4),
            (K::RefreshInterval, Kml, 5),
            (K::ViewRefreshMode, Kml, 6),
            (K::ViewRefreshTime, Kml, 7),
            (K::ViewBoundScale, Kml, 8),
            (K::ViewFormat, Kml, 9),
            (K::HttpQuery, Kml, 10),
        ],
        (K::ViewVolume, Kml) => &[
            (K::LeftFov, Kml, 1),
            (K::RightFov, Kml, 2),
            (K::BottomFov, Kml, 3),
            (K::TopFov, Kml, 4),
            (K::Near, Kml, 5),
        ],
        (K::Wait, Gx) => &[
            (K::Duration, Gx, 1),
        ],
        _ => &[],
    }
}

/// The slot of a child the model carries as foreign content, by its
/// namespace and local name, when the schemas name it under `parent`.
pub(crate) fn foreign_slot_2_2(parent: KmlElement, spelling: Spelling, ns: &str, local: &str) -> Option<u8> {
    use KmlElement as K;
    use Spelling::{Gx, Kml};
    const ATOM: &str = "http://www.w3.org/2005/Atom";
    const XAL: &str = "urn:oasis:names:tc:ciq:xsdschema:xAL:2.0";
    const GX: &str = "http://www.google.com/kml/ext/2.2";
    const FOREIGN: &[(KmlElement, Spelling, &str, &str, u8)] = &[
        (K::Camera, Kml, GX, "ViewerOptions", 2),
        (K::Change, Kml, GX, "ViewerOptions", 0),
        (K::Document, Kml, ATOM, "author", 4),
        (K::Document, Kml, ATOM, "link", 5),
        (K::Document, Kml, XAL, "AddressDetails", 7),
        (K::Document, Kml, GX, "rank", 17),
        (K::Folder, Kml, ATOM, "author", 4),
        (K::Folder, Kml, ATOM, "link", 5),
        (K::Folder, Kml, XAL, "AddressDetails", 7),
        (K::Folder, Kml, GX, "rank", 17),
        (K::GroundOverlay, Kml, ATOM, "author", 4),
        (K::GroundOverlay, Kml, ATOM, "link", 5),
        (K::GroundOverlay, Kml, XAL, "AddressDetails", 7),
        (K::GroundOverlay, Kml, GX, "rank", 17),
        (K::Icon, Kml, GX, "x", 2),
        (K::Icon, Kml, GX, "y", 2),
        (K::Icon, Kml, GX, "w", 2),
        (K::Icon, Kml, GX, "h", 2),
        (K::IconStyle, Kml, GX, "labelVisibility", 5),
        (K::LabelStyle, Kml, GX, "labelVisibility", 5),
        (K::LineString, Kml, GX, "drawOrder", 1),
        (K::LineStyle, Kml, GX, "labelVisibility", 5),
        (K::LineStyle, Kml, GX, "outerColor", 8),
        (K::LineStyle, Kml, GX, "outerWidth", 8),
        (K::LineStyle, Kml, GX, "physicalWidth", 8),
        (K::LinearRing, Kml, GX, "drawOrder", 1),
        (K::Link, Kml, GX, "x", 2),
        (K::Link, Kml, GX, "y", 2),
        (K::Link, Kml, GX, "w", 2),
        (K::Link, Kml, GX, "h", 2),
        (K::LookAt, Kml, GX, "ViewerOptions", 2),
        (K::Model, Kml, GX, "drawOrder", 1),
        (K::MultiGeometry, Kml, GX, "drawOrder", 1),
        (K::MultiTrack, Gx, GX, "drawOrder", 1),
        (K::NetworkLink, Kml, ATOM, "author", 4),
        (K::NetworkLink, Kml, ATOM, "link", 5),
        (K::NetworkLink, Kml, XAL, "AddressDetails", 7),
        (K::NetworkLink, Kml, GX, "rank", 17),
        (K::PhotoOverlay, Kml, ATOM, "author", 4),
        (K::PhotoOverlay, Kml, ATOM, "link", 5),
        (K::PhotoOverlay, Kml, XAL, "AddressDetails", 7),
        (K::PhotoOverlay, Kml, GX, "rank", 17),
        (K::Placemark, Kml, ATOM, "author", 4),
        (K::Placemark, Kml, ATOM, "link", 5),
        (K::Placemark, Kml, XAL, "AddressDetails", 7),
        (K::Placemark, Kml, GX, "rank", 17),
        (K::Point, Kml, GX, "drawOrder", 1),
        (K::PolyStyle, Kml, GX, "labelVisibility", 5),
        (K::Polygon, Kml, GX, "drawOrder", 1),
        (K::ScreenOverlay, Kml, ATOM, "author", 4),
        (K::ScreenOverlay, Kml, ATOM, "link", 5),
        (K::ScreenOverlay, Kml, XAL, "AddressDetails", 7),
        (K::ScreenOverlay, Kml, GX, "rank", 17),
        (K::Tour, Gx, ATOM, "author", 4),
        (K::Tour, Gx, ATOM, "link", 5),
        (K::Tour, Gx, XAL, "AddressDetails", 7),
        (K::Tour, Gx, GX, "rank", 17),
        (K::Track, Gx, GX, "drawOrder", 1),
        (K::Url, Kml, GX, "x", 2),
        (K::Url, Kml, GX, "y", 2),
        (K::Url, Kml, GX, "w", 2),
        (K::Url, Kml, GX, "h", 2),
    ];
    FOREIGN.iter().find(|(p, s, n, l, _)| *p == parent && *s == spelling && *n == ns && *l == local).map(|f| f.4)
}

/// Whether `ogckml22.xsd` defines `element` in the KML namespace.
pub(crate) fn in_2_2(element: KmlElement) -> bool {
    use KmlElement as K;
    matches!(
        element,
        K::Address
            | K::Alias
            | K::Altitude
            | K::AltitudeMode
            | K::BalloonStyle
            | K::Begin
            | K::BgColor
            | K::BottomFov
            | K::Camera
            | K::Change
            | K::Color
            | K::ColorMode
            | K::Cookie
            | K::Coordinates
            | K::Create
            | K::Data
            | K::Delete
            | K::Description
            | K::DisplayMode
            | K::DisplayName
            | K::Document
            | K::DrawOrder
            | K::East
            | K::End
            | K::Expires
            | K::ExtendedData
            | K::Extrude
            | K::Fill
            | K::FlyToView
            | K::Folder
            | K::GridOrigin
            | K::GroundOverlay
            | K::Heading
            | K::HotSpot
            | K::Href
            | K::HttpQuery
            | K::Icon
            | K::IconStyle
            | K::ImagePyramid
            | K::InnerBoundaryIs
            | K::ItemIcon
            | K::Key
            | K::Kml
            | K::LabelStyle
            | K::LatLonAltBox
            | K::LatLonBox
            | K::Latitude
            | K::LeftFov
            | K::LineString
            | K::LineStyle
            | K::LinearRing
            | K::Link
            | K::LinkDescription
            | K::LinkName
            | K::LinkSnippet
            | K::ListItemType
            | K::ListStyle
            | K::Location
            | K::Lod
            | K::Longitude
            | K::LookAt
            | K::MaxAltitude
            | K::MaxFadeExtent
            | K::MaxHeight
            | K::MaxLodPixels
            | K::MaxSessionLength
            | K::MaxSnippetLines
            | K::MaxWidth
            | K::Message
            | K::Metadata
            | K::MinAltitude
            | K::MinFadeExtent
            | K::MinLodPixels
            | K::MinRefreshPeriod
            | K::Model
            | K::MultiGeometry
            | K::Name
            | K::Near
            | K::NetworkLink
            | K::NetworkLinkControl
            | K::North
            | K::Open
            | K::Orientation
            | K::OuterBoundaryIs
            | K::Outline
            | K::OverlayXY
            | K::Pair
            | K::PhoneNumber
            | K::PhotoOverlay
            | K::Placemark
            | K::Point
            | K::PolyStyle
            | K::Polygon
            | K::Range
            | K::RefreshInterval
            | K::RefreshMode
            | K::RefreshVisibility
            | K::Region
            | K::ResourceMap
            | K::RightFov
            | K::Roll
            | K::Rotation
            | K::RotationXY
            | K::Scale
            | K::ScaleUpper
            | K::Schema
            | K::SchemaData
            | K::ScreenOverlay
            | K::ScreenXY
            | K::Shape
            | K::SimpleData
            | K::SimpleField
            | K::Size
            | K::Snippet
            | K::SnippetUpper
            | K::SourceHref
            | K::South
            | K::State
            | K::Style
            | K::StyleMap
            | K::StyleUrl
            | K::TargetHref
            | K::Tessellate
            | K::Text
            | K::TextColor
            | K::TileSize
            | K::Tilt
            | K::TimeSpan
            | K::TimeStamp
            | K::TopFov
            | K::Update
            | K::Url
            | K::Value
            | K::ViewBoundScale
            | K::ViewFormat
            | K::ViewRefreshMode
            | K::ViewRefreshTime
            | K::ViewVolume
            | K::Visibility
            | K::West
            | K::When
            | K::Width
            | K::X
            | K::Y
            | K::Z
    )
}
// --- End of generated code. ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kml::model;
    use crate::xml;

    const NS: &str = r#"xmlns="http://www.opengis.net/kml/2.2""#;

    fn lowered(source: &str) -> (String, Changes) {
        let mut doc = model::parse(source).unwrap();
        let report = lower(&mut doc);
        (model::write(&doc), report)
    }

    fn body(written: &str) -> &str {
        &written[written.find("?>\n").unwrap() + 3..]
    }

    #[test]
    fn adopted_elements_go_back_to_gx_and_the_root_declares_it() {
        let (out, report) = lowered(&format!(
            "<kml {NS} version=\"2.3\"><Placemark><Track><when>a</when><coord>1 2 3</coord></Track></Placemark></kml>"
        ));
        assert_eq!(
            body(&out),
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2"><Placemark><gx:Track><when>a</when><gx:coord>1 2 3</gx:coord></gx:Track></Placemark></kml>"#
        );
        assert_eq!(
            report.respelled_warning(KmlVersion::V2_3, KmlVersion::V2_2).unwrap(),
            "kml: input is KML 2.3; written as KML 2.2 (Track→gx:Track, coord→gx:coord)"
        );
    }

    #[test]
    fn a_shared_name_is_lowered_by_its_parent() {
        let (out, _) = lowered(&format!(
            "<kml {NS} version=\"2.3\"><Placemark><TimeStamp><when>t</when></TimeStamp>\
             <LookAt><TimeStamp><when>v</when></TimeStamp><longitude>1</longitude></LookAt>\
             <ExtendedData><Data name=\"d\"><value>1</value></Data></ExtendedData></Placemark></kml>"
        ));
        // The feature's time stays kml:; the view's is gx:, and 2.2 puts the
        // view before the feature's time.
        assert!(
            out.contains("<Placemark><LookAt><gx:TimeStamp><when>v</when></gx:TimeStamp><longitude>1</longitude></LookAt><TimeStamp><when>t</when></TimeStamp>"),
            "{out}"
        );
        assert!(out.contains("<Data name=\"d\"><value>1</value></Data>"), "{out}");
    }

    #[test]
    fn an_interleaved_track_is_grouped() {
        let (out, _) = lowered(&format!(
            "<kml {NS} version=\"2.3\"><Placemark><Track><when>1</when><coord>a</coord><when>2</when><coord>b</coord>\
             <altitudeMode>absolute</altitudeMode></Track></Placemark></kml>"
        ));
        assert!(
            out.contains("<gx:Track><altitudeMode>absolute</altitudeMode><when>1</when><when>2</when><gx:coord>a</gx:coord><gx:coord>b</gx:coord></gx:Track>"),
            "{out}"
        );
    }

    #[test]
    fn foreign_content_and_comments_keep_their_neighbours() {
        let (out, _) = lowered(&format!(
            "<kml {NS} xmlns:atom=\"http://www.w3.org/2005/Atom\" version=\"2.3\"><Placemark><description>d</description>\
             <name>n</name><atom:author><atom:name>x</atom:name></atom:author><!--before point--><Point><coordinates>1,2</coordinates></Point>\
             <!--end--></Placemark></kml>"
        ));
        assert!(
            out.contains("<Placemark><name>n</name><atom:author><atom:name>x</atom:name></atom:author><description>d</description><!--before point--><Point><coordinates>1,2</coordinates></Point><!--end--></Placemark>"),
            "{out}"
        );
    }

    #[test]
    fn foreign_content_the_schemas_name_takes_its_slot() {
        let (out, _) = lowered(&format!(
            "<kml {NS} xmlns:atom=\"http://www.w3.org/2005/Atom\" version=\"2.3\"><Placemark><name>n</name>\
             <atom:author><atom:name>x</atom:name></atom:author><visibility>1</visibility>\
             <LineString><coordinates>1,2 3,4</coordinates><g:drawOrder xmlns:g=\"http://www.google.com/kml/ext/2.2\">1</g:drawOrder>\
             </LineString></Placemark></kml>"
        ));
        // atom:author follows <visibility>, as AbstractFeatureType puts it;
        // gx:drawOrder, declared on itself, goes before the coordinates.
        assert!(out.contains("<name>n</name><visibility>1</visibility><atom:author>"), "{out}");
        assert!(out.contains("<LineString><g:drawOrder xmlns:g=\"http://www.google.com/kml/ext/2.2\">1</g:drawOrder><coordinates>"), "{out}");
    }

    #[test]
    fn what_2_2_has_no_place_for_is_dropped_and_reported() {
        let (out, report) = lowered(&format!(
            "<kml {NS} version=\"2.3\"><Document><Schema id=\"s\"><SimpleField name=\"h\" type=\"double\" uom=\"m\"/></Schema>\
             <NetworkLinkControl/><Placemark><Region><LatLonBox><north>1</north><rotation>45</rotation></LatLonBox></Region></Placemark></Document></kml>"
        ));
        assert!(out.contains("<SimpleField name=\"h\" type=\"double\"/>"), "{out}");
        assert!(out.contains("<Region/>"), "{out}");
        assert_eq!(
            report.dropped_warning().unwrap(),
            "kml: KML 2.2 has no place for uom= on <SimpleField>, <NetworkLinkControl> in <Document>, <LatLonBox> in <Region>; dropped"
        );
    }

    #[test]
    fn an_unturned_region_box_is_written_as_the_altitude_box_it_equals() {
        let (out, report) = lowered(&format!(
            "<kml {NS} version=\"2.3\"><Placemark><Region><Lod><minLodPixels>128</minLodPixels></Lod>\
             <LatLonBox id=\"b\"><rotation>0</rotation><west>1</west><north>4</north><south>3</south><east>2</east></LatLonBox>\
             </Region></Placemark></kml>"
        ));
        // The edges in 2.2's order, the zero rotation gone, the box before <Lod>.
        assert!(
            out.contains("<Region><LatLonAltBox id=\"b\"><north>4</north><south>3</south><east>2</east><west>1</west></LatLonAltBox><Lod>"),
            "{out}"
        );
        assert_eq!(report.dropped_warning(), None);
        assert_eq!(
            report.respelled_warning(KmlVersion::V2_3, KmlVersion::V2_2).unwrap(),
            "kml: input is KML 2.3; written as KML 2.2 (LatLonBox→LatLonAltBox)"
        );
    }

    #[test]
    fn an_empty_rotation_is_the_default_and_unturned() {
        let (out, _) = lowered(&format!(
            "<kml {NS} version=\"2.3\"><Placemark><Region><LatLonBox><north>1</north><rotation/></LatLonBox></Region></Placemark></kml>"
        ));
        assert!(out.contains("<Region><LatLonAltBox><north>1</north></LatLonAltBox></Region>"), "{out}");
    }

    #[test]
    fn a_turned_box_a_second_box_and_a_box_outside_a_region_stay_as_they_were() {
        // A ground overlay's box keeps its rotation; a region with two boxes,
        // which 2.3 does not admit, would have two altitude boxes, which 2.2
        // does not either.
        let (out, report) = lowered(&format!(
            "<kml {NS} version=\"2.3\"><GroundOverlay><LatLonBox><north>1</north><rotation>10</rotation></LatLonBox></GroundOverlay>\
             <Placemark><Region><LatLonAltBox><north>2</north></LatLonAltBox><LatLonBox><north>3</north></LatLonBox></Region></Placemark></kml>"
        ));
        assert!(out.contains("<LatLonBox><north>1</north><rotation>10</rotation></LatLonBox>"), "{out}");
        assert!(out.contains("<Region><LatLonAltBox><north>2</north></LatLonAltBox></Region>"), "{out}");
        assert!(report.renamed.is_empty());
        assert_eq!(report.dropped_warning().unwrap(), "kml: KML 2.2 has no place for <LatLonBox> in <Region>; dropped");
    }

    #[test]
    fn a_document_with_no_namespace_gains_ogcs() {
        let (out, report) = lowered("<kml version=\"2.3\"><Document><name>n</name></Document></kml>");
        assert_eq!(body(&out), r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><name>n</name></Document></kml>"#);
        assert_eq!(report, Changes::default(), "a version stamp alone is not reported");
    }

    #[test]
    fn a_prefixed_document_writes_its_kml_prefix_and_reuses_its_gx_binding() {
        let (out, _) = lowered(
            r#"<k:kml xmlns:k="http://www.opengis.net/kml/2.2" xmlns:g="http://www.google.com/kml/ext/2.2" version="2.3"><k:Placemark><k:LookAt><g:TimeStamp><k:when>t</k:when></g:TimeStamp></k:LookAt><k:Track/></k:Placemark></k:kml>"#,
        );
        assert_eq!(
            body(&out),
            r#"<k:kml xmlns:k="http://www.opengis.net/kml/2.2" xmlns:g="http://www.google.com/kml/ext/2.2"><k:Placemark><k:LookAt><g:TimeStamp><k:when>t</k:when></g:TimeStamp></k:LookAt><g:Track/></k:Placemark></k:kml>"#
        );
    }

    #[test]
    fn a_survey_reports_what_the_lowering_does() {
        let source = format!(
            "<kml {NS} version=\"2.3\"><Placemark><Track><coord>1</coord></Track><Region><LatLonQuad/></Region>\
             <Region><LatLonBox><rotation>0.0</rotation></LatLonBox></Region><Region><LatLonBox><rotation>5</rotation></LatLonBox></Region>\
             <ExtendedData><Data name=\"d\" uom=\"m\"><value>1</value></Data></ExtendedData></Placemark></kml>"
        );
        let doc = model::parse(&source).unwrap();
        let mut surveyed = Changes::default();
        for child in &doc.root.children {
            survey(child, (KmlElement::Kml, Spelling::Kml), &mut surveyed);
        }
        assert_eq!(lowered(&source).1, surveyed);
    }

    #[test]
    fn a_lowered_document_reads_back_the_same_model() {
        // Respelling and reordering change how it is written, not what it is.
        let source = format!(
            "<kml {NS} version=\"2.3\"><Document><Placemark><Track><when>1</when><coord>1 2 3</coord></Track>\
             <name>t</name></Placemark></Document></kml>"
        );
        let (out, _) = lowered(&source);
        let identities = |s: &str| {
            fn walk(n: &Node, out: &mut Vec<KmlElement>) {
                out.extend(n.element());
                n.children.iter().for_each(|c| walk(c, out));
            }
            let mut v = Vec::new();
            walk(&model::parse(s).unwrap().root, &mut v);
            v.sort();
            v
        };
        assert_eq!(identities(&out), identities(&source));
        assert_eq!(xml::parse_document(&out).unwrap(), xml::parse_document(&lowered(&out).0).unwrap(), "lowering is idempotent");
    }

    #[test]
    fn every_element_has_a_2_2_spelling() {
        let all: Vec<_> = model::vocabulary().collect();
        assert_eq!(all.len(), 168);
        for element in all {
            assert!(in_2_2(element) || element.gx_name().is_some(), "{}", element.name());
        }
    }
}
