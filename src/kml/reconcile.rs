//! Writing a KML read back to KML: the model, with the IR's edits applied
//! (`plans/kml-standard.org` D-K4, K4).
//!
//! *The IR wins for what it projects; the model keeps everything else.* Each
//! feature's stored element is projected again ([`project`]) and compared with
//! the feature as it now stands. An unedited feature is written exactly as it
//! was read. An edited one has only the members that changed rewritten —
//! `<name>`, `<description>`, its time, an overlay's icon, an altitude member,
//! an `<ExtendedData>` value, the geometry — and keeps its styles, views,
//! foreign content and comments. Precedent: `geopackage/reconcile.rs`.
//!
//! An element the edit adds goes where `ogckml22.xsd`'s sequence for its
//! parent puts it, which 2.3's unordered groups also accept.

use std::fmt::Write as _;
use std::rc::Rc;

use crate::crs::Crs;
use crate::feature::{Feature, FeatureCollection};
use crate::format::{Format, KmlVersion};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::kml::document::{self, FeatureModel, Skeleton};
use crate::kml::model::{self, KmlDocument, KmlElement, Layout, Node};
use crate::kml::place::{place, Placement, Time};
use crate::kml::project::{
    self, can_carry, carriers, project, Context, LatLonBox, Vertical, ALTITUDE_MEMBERS,
};
use crate::kml::writer::{
    altitude_element, feature_to_placemark, geometry_rank, geometry_to_element, property_text, time_element, Typed,
};
use crate::native::NativePayload;
use crate::tally::Tally;
use crate::xml::{Content, XmlElement};

/// The stored element of a feature read from KML or KMZ, if it has one.
pub(crate) fn feature_model(f: &Feature) -> Option<&FeatureModel> {
    stored(f).map(|(m, _)| m)
}

/// A feature's stored element and which of the source's documents it was
/// read from (0 for the first, `n` for layer `n - 1`).
pub(crate) fn stored(f: &Feature) -> Option<(&FeatureModel, usize)> {
    let native = f.native.as_ref()?;
    match native.payload_for(Format::Kml).or_else(|| native.payload_for(Format::Kmz))? {
        NativePayload::KmlFeature(k) => Some((&k.feature, k.document)),
        _ => None,
    }
}

/// `features` put back into `skeleton`, a document of `version`: each with a
/// stored element of this document at its address, reconciled with it; each
/// without one built fresh. Which features belong to the document is the
/// caller's to say (`crate::kml::writer`), since a layered `.kmz` holds
/// several.
pub(crate) fn assemble<'f>(
    features: impl Iterator<Item = (&'f Feature, Option<&'f FeatureModel>)>,
    skeleton: &Skeleton,
    version: KmlVersion,
    compact: bool,
) -> KmlDocument {
    let ctx = Context::of(&skeleton.doc.root);
    let features: Vec<_> = features.collect();
    // The layout is the whole document's: with no `<Document>`, every element
    // but the root's may have been split out with its feature.
    let layout = Layout::of(&skeleton.doc.root)
        .or_else(|| features.iter().find_map(|(_, m)| Layout::of(&(*m)?.node)))
        .filter(|_| !compact);
    let w = Writing { prefix: skeleton.doc.root.prefix.as_deref(), version, ctx: &ctx, layout: layout.as_ref() };
    let lead_of = |m: &FeatureModel| m.leading.last().filter(|c| c.is_space()).and_then(Content::text).map(Rc::from);
    // How a feature added since the read is indented, and whether its inside
    // is laid out: as the last feature of the container it joins was, else
    // as the container lays out what it kept.
    let joins = document::default_container(&skeleton.doc.root);
    let new_lead: Option<(Rc<str>, bool)> = layout.as_ref().and_then(|l| {
        let sibling = features.iter().rev().filter_map(|(_, m)| *m).find(|m| m.address.container == joins);
        match sibling.and_then(|m| Some((lead_of(m)?, m))) {
            Some((lead, m)) => Some((lead, !m.node.keeps_inside_on_one_line())),
            None => document::lead_for_new(skeleton, l),
        }
    });
    let features = features
        .into_iter()
        .map(|(f, model)| match model {
            Some(m) => (Some(m.address.clone()), m.leading.clone(), w.reconcile(&m.node, f, lead_of(m).as_deref())),
            None => {
                let leading = new_lead.iter().map(|(l, _)| Content::Text(l.clone())).collect();
                let inside = new_lead.as_ref().filter(|(_, inside)| *inside).map(|(l, _)| &**l);
                (None, leading, w.fresh(f, inside))
            }
        })
        .collect();
    document::assemble(skeleton, features)
}

/// What reconciling into one document needs to know about it.
struct Writing<'a> {
    /// The document's KML prefix — a document whose root is `<k:kml>` has no
    /// default namespace, so an unprefixed element would be in none.
    prefix: Option<&'a str>,
    version: KmlVersion,
    ctx: &'a Context,
    /// The document's layout, which what an edit adds is written in
    /// (`plans/kml-layout.org` D-L3).
    layout: Option<&'a Layout>,
}

/// What building a node into a feature's element needs: the feature's KML
/// prefix, and the document's layout for placing it.
#[derive(Clone, Copy)]
struct Build<'a> {
    prefix: Option<&'a str>,
    layout: Option<&'a Layout>,
}

impl Build<'_> {
    /// A writer-built element as a model node, in the feature's prefix. It is
    /// laid out where it is inserted.
    fn node(&self, el: &XmlElement) -> Node {
        let mut node = model::from_built(el);
        node.set_prefix(self.prefix);
        node
    }
}

/// An edit that leaves an overlay with geometry it cannot hold — a
/// `<GroundOverlay>`'s footprint that is no longer four corners, a
/// `<PhotoOverlay>`'s that is no longer a point.
#[derive(Debug, PartialEq, Eq)]
struct Unwritable;

impl Writing<'_> {
    /// A feature with no stored element, built from the IR and laid out for a
    /// place indented by `lead`. Its values go in `<Data>`: the document's
    /// schemas were written for the features it held.
    fn fresh(&self, f: &Feature, lead: Option<&str>) -> Node {
        let b = Build { prefix: self.prefix, layout: self.layout };
        let mut node = b.node(&feature_to_placemark(f, self.version, &Typed::default()));
        if let (Some(layout), Some(lead)) = (self.layout, lead) {
            layout.lay_out_under(&mut node, lead);
        }
        node
    }

    /// `stored`, indented by `lead`, with `f`'s edits applied.
    fn reconcile(&self, stored: &Node, f: &Feature, lead: Option<&str>) -> Node {
        let Ok(projected) = project(stored, self.ctx) else {
            // It projected when it was read, so this cannot happen; if it did,
            // the IR is the only statement of the feature left.
            return self.fresh(f, lead);
        };
        if projected.properties == f.properties && projected.geometry == f.geometry {
            return stored.clone();
        }
        let element = stored.element().expect("a projected feature is a KML element");
        let prefix = stored.prefix.clone();
        let b = Build { prefix: prefix.as_deref(), layout: self.layout };
        let mut node = stored.clone();
        // The geometry first, so the properties are placed against the
        // geometry that will be written.
        if projected.geometry != f.geometry && reconcile_geometry(&mut node, f.geometry.as_ref(), self.version, b).is_err() {
            // What the overlay was is gone with its geometry; the feature is
            // a Placemark now, and `drop_warnings` says so.
            return self.fresh(f, lead);
        }
        let placement = place(f, element, |member| {
            project::geometry_of(&node).is_some_and(|g| {
                carriers(g).iter().any(|c| c.element().is_some_and(|k| can_carry(k, member)))
            })
        });
        self.apply(&mut node, &placement, b);
        node
    }

    fn apply(&self, node: &mut Node, placement: &Placement, b: Build<'_>) {
        set_leaf(node, KmlElement::Name, placement.name.as_deref(), b);
        set_leaf(node, KmlElement::Description, placement.description.as_deref(), b);
        reconcile_time(node, &placement.time, b);
        if node.element().is_some_and(KmlElement::is_overlay) {
            reconcile_icon(node, placement.icon.as_deref(), b);
        }
        reconcile_altitude(node, &placement.altitude, self.version, b);
        reconcile_extended_data(node, &placement.extended, self.ctx, b);
    }
}

/// Make `parent`'s `element` child hold `want`: its text set, removed for
/// `None`, or added where the schema puts it. A child already holding the text
/// is left as it is.
fn set_leaf(parent: &mut Node, element: KmlElement, want: Option<&str>, b: Build<'_>) {
    let at = parent.children.iter().position(|c| c.is(element));
    match (want, at) {
        (None, Some(i)) => parent.remove_child(i),
        (Some(v), Some(i)) => {
            if parent.children[i].text_trimmed() != v {
                parent.children[i].set_text(v);
            }
        }
        (Some(v), None) => {
            insert_in_order(parent, b, b.node(&XmlElement::leaf(element.name(), v)));
        }
        (None, None) => {}
    }
}

/// Bring the feature's time primitive in line with `want`, keeping the one
/// it has — its `id`, its foreign content — when it is the same kind.
fn reconcile_time(node: &mut Node, want: &Time, b: Build<'_>) {
    if Time::of(&project::time_values(node)) == *want {
        return;
    }
    let at = node.children.iter().position(|c| c.element().is_some_and(KmlElement::is_time_primitive));
    let kind = at.and_then(|i| node.children[i].element());
    match (want, at) {
        (Time::None, Some(i)) => node.remove_child(i),
        (Time::None, None) => {}
        (Time::Stamp(when), Some(i)) if kind == Some(KmlElement::TimeStamp) => {
            set_leaf(&mut node.children[i], KmlElement::When, Some(when), b);
        }
        (Time::Span(begin, end), Some(i)) if kind == Some(KmlElement::TimeSpan) => {
            let span = &mut node.children[i];
            set_leaf(span, KmlElement::Begin, begin.as_deref(), b);
            set_leaf(span, KmlElement::End, end.as_deref(), b);
        }
        (want, at) => {
            let fresh = b.node(&time_element(want).expect("a time to write"));
            match at {
                Some(i) => node.replace_child(i, fresh, b.layout),
                None => {
                    insert_in_order(node, b, fresh);
                }
            }
        }
    }
}

/// Bring an overlay's `<Icon><href>` in line with `want`. Removing it leaves
/// the rest of `<Icon>` — its refresh settings — where it was.
fn reconcile_icon(node: &mut Node, want: Option<&str>, b: Build<'_>) {
    if project::icon_href(node) == want {
        return;
    }
    let existing = node.children.iter().position(|c| c.is(KmlElement::Icon));
    let at = match existing {
        Some(i) => i,
        None if want.is_none() => return,
        None => insert_in_order(node, b, b.node(&XmlElement::with_children("Icon", Vec::new()))),
    };
    set_leaf(&mut node.children[at], KmlElement::Href, want, b);
    if existing.is_none() {
        node.lay_out_child(at, b.layout);
    }
}

/// Bring the geometry's altitude members in line with `want`. A member the
/// IR changed is set on every carrier that can hold it, and one it removed
/// is removed from every carrier; a member whose carriers disagree was never
/// projected, so an edit elsewhere leaves it alone. Setting one altitude mode
/// removes the other, which the schema makes the same choice.
fn reconcile_altitude(node: &mut Node, want: &[(KmlElement, String)], version: KmlVersion, b: Build<'_>) {
    let have: Vec<(&str, String)> =
        project::altitude_values(node).into_iter().map(|(k, v)| (k, v.to_string())).collect();
    let Some(gi) = geometry_index(node) else { return };
    for (key, member) in ALTITUDE_MEMBERS {
        let h = have.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
        let w = want.iter().find(|(m, _)| *m == member).map(|(_, v)| v.as_str());
        if h == w {
            continue;
        }
        for_each_carrier(&mut node.children[gi], &mut |carrier| {
            if !carrier.element().is_some_and(|k| can_carry(k, member)) {
                return;
            }
            let at = carrier.children.iter().position(|c| c.is(member));
            match (w, at) {
                (None, Some(i)) => carrier.remove_child(i),
                (None, None) => {}
                (Some(v), Some(i)) => {
                    if carrier.children[i].text_trimmed() != v {
                        carrier.children[i].set_text(v);
                    }
                }
                (Some(v), None) => {
                    let other = match member {
                        KmlElement::AltitudeMode => Some(KmlElement::SeaFloorAltitudeMode),
                        KmlElement::SeaFloorAltitudeMode => Some(KmlElement::AltitudeMode),
                        _ => None,
                    };
                    if let Some(j) = other.and_then(|o| carrier.children.iter().position(|c| c.is(o))) {
                        carrier.remove_child(j);
                    }
                    insert_in_order(carrier, b, b.node(&altitude_element(member, v, version)));
                }
            }
        });
    }
}

/// Where the feature's geometry element is among its children.
fn geometry_index(node: &Node) -> Option<usize> {
    match node.element()? {
        KmlElement::Placemark => node.children.iter().position(|c| c.element().is_some_and(KmlElement::is_geometry)),
        KmlElement::PhotoOverlay => node.children.iter().position(|c| c.is(KmlElement::Point)),
        _ => None,
    }
}

/// [`carriers`], mutably: `geometry`, and every member of a `<MultiGeometry>`
/// or `<MultiTrack>`, recursively.
fn for_each_carrier(geometry: &mut Node, f: &mut dyn FnMut(&mut Node)) {
    f(geometry);
    if matches!(geometry.element(), Some(KmlElement::MultiGeometry | KmlElement::MultiTrack)) {
        for child in geometry.children.iter_mut().filter(|c| c.element().is_some_and(KmlElement::is_geometry)) {
            for_each_carrier(child, f);
        }
    }
}

/// Bring `<ExtendedData>`'s values in line with `want`, the properties no
/// member of the feature holds: each existing `<Data>`/`<SimpleData>` keeps
/// its place and is updated or removed, and a property it has no element for
/// is appended as `<Data>`. Values are compared as the projection reads them,
/// so a typed `<SimpleData>` whose value did not change keeps its spelling.
fn reconcile_extended_data(node: &mut Node, want: &[(&str, &JsonValue)], ctx: &Context, b: Build<'_>) {
    let at = node.children.iter().position(|c| c.is(KmlElement::ExtendedData));
    let have = at.map(|i| project::extended_data_values(&node.children[i], ctx)).unwrap_or_default();
    if have.len() == want.len() && have.iter().zip(want).all(|((hk, hv), (wk, wv))| hk == wk && hv == *wv) {
        return;
    }
    let i = match at {
        Some(i) => i,
        None => insert_in_order(node, b, b.node(&XmlElement::with_children("ExtendedData", Vec::new()))),
    };
    let ed = &mut node.children[i];
    // Which of `want` an element already holds, so a repeated key goes to the
    // next element of that name rather than the first.
    let mut placed = vec![false; want.len()];
    let mut claim = |key: &str| {
        let j = want.iter().zip(&placed).position(|((k, _), p)| *k == key && !*p)?;
        placed[j] = true;
        Some(want[j].1)
    };
    let mut j = 0;
    while j < ed.children.len() {
        let child = &ed.children[j];
        let key = child.attr("name").map(str::to_string);
        match (child.element(), key) {
            (Some(KmlElement::Data), Some(key)) => match claim(&key) {
                Some(v) => {
                    if project::data_value(&ed.children[j]) != *v {
                        set_data_value(&mut ed.children[j], &property_text(v), b);
                    }
                    j += 1;
                }
                None => ed.remove_child(j),
            },
            (Some(KmlElement::SchemaData), _) => {
                let schema_data = &mut ed.children[j];
                let url = schema_data.attr("schemaUrl").map(str::to_string);
                let mut s = 0;
                while s < schema_data.children.len() {
                    let simple = &schema_data.children[s];
                    let key = simple.attr("name").map(str::to_string);
                    match (simple.is(KmlElement::SimpleData), key) {
                        (true, Some(key)) => match claim(&key) {
                            Some(v) => {
                                if project::simple_data_value(simple, url.as_deref(), ctx) != *v {
                                    schema_data.children[s].set_text(&property_text(v));
                                }
                                s += 1;
                            }
                            None => schema_data.remove_child(s),
                        },
                        _ => s += 1,
                    }
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    for ((k, v), placed) in want.iter().zip(placed) {
        if !placed {
            let data = XmlElement::with_children("Data", vec![XmlElement::leaf("value", property_text(v))]).with_attr("name", *k);
            // After the last `<Data>`, else before the first `<SchemaData>`:
            // 2.2's sequence holds every `<Data>` ahead of every `<SchemaData>`.
            let at = match ed.children.iter().rposition(|c| c.is(KmlElement::Data)) {
                Some(last) => last + 1,
                None => ed.children.iter().position(|c| c.is(KmlElement::SchemaData)).unwrap_or(ed.children.len()),
            };
            ed.insert_laid_out(at, b.node(&data), b.layout);
        }
    }
    if ed.children.is_empty() && ed.content.iter().all(|(_, c)| c.is_space()) {
        node.remove_child(i);
    } else if at.is_none() {
        node.lay_out_child(i, b.layout);
    }
}

fn set_data_value(data: &mut Node, value: &str, b: Build<'_>) {
    set_leaf(data, KmlElement::Value, Some(value), b);
}

/// Replace the feature's geometry with the IR's.
///
/// A Placemark's element takes the new coordinates in place where it can —
/// the same `<Point>`, a line string or ring edited as a line string, a track
/// whose vertex count did not change — so `<extrude>`, `<altitudeMode>`, a
/// track's `<when>` series and any foreign content stay; otherwise it is
/// replaced in its position. An overlay keeps its own geometry element
/// (D-K14): see [`reconcile_footprint`] and [`reconcile_photo_point`].
fn reconcile_geometry(
    node: &mut Node,
    geometry: Option<&Geometry>,
    version: KmlVersion,
    b: Build<'_>,
) -> Result<(), Unwritable> {
    match node.element() {
        Some(KmlElement::GroundOverlay) => return reconcile_footprint(node, geometry, version, b),
        Some(KmlElement::PhotoOverlay) => return reconcile_photo_point(node, geometry, b),
        _ => {}
    }
    let at = node.children.iter().position(|c| c.element().is_some_and(KmlElement::is_geometry));
    let Some(geometry) = geometry else {
        if let Some(i) = at {
            node.remove_child(i);
        }
        return Ok(());
    };
    if let Some(i) = at
        && update_in_place(&mut node.children[i], geometry)
    {
        return Ok(());
    }
    let fresh = b.node(&geometry_to_element(geometry));
    match at {
        Some(i) => node.replace_child(i, fresh, b.layout),
        None => {
            insert_in_order(node, b, fresh);
        }
    }
    Ok(())
}

/// A `<GroundOverlay>`'s footprint edited: a quadrilateral that is still a box
/// turned about its centre is written back into its `<LatLonBox>`, and one
/// that is not into a `<LatLonQuad>` — 2.2's `<gx:LatLonQuad>`, declared where
/// it is written. One the overlay already gives as a quad stays a quad.
/// Anything but a four-cornered polygon is no footprint.
fn reconcile_footprint(
    node: &mut Node,
    geometry: Option<&Geometry>,
    version: KmlVersion,
    b: Build<'_>,
) -> Result<(), Unwritable> {
    let at_box = node.children.iter().position(|c| c.is(KmlElement::LatLonBox));
    let at_quad = node.children.iter().position(|c| c.is(KmlElement::LatLonQuad));
    let Some(geometry) = geometry else {
        for i in [at_box, at_quad].into_iter().flatten().rev() {
            node.remove_child(i);
        }
        return Ok(());
    };
    let corners = match geometry {
        Geometry::Polygon(rings) if rings.len() == 1 => project::quad_corners(&rings[0]).ok_or(Unwritable)?,
        _ => return Err(Unwritable),
    };
    if let Some(i) = at_quad {
        let quad = &mut node.children[i];
        if !set_coordinates(quad, &corners) {
            set_leaf(quad, KmlElement::Coordinates, Some(&coordinates_text(&corners)), b);
        }
        return Ok(());
    }
    if let Some(bounds) = LatLonBox::from_ring(&corners) {
        let i = match at_box {
            Some(i) => i,
            None => insert_in_order(node, b, b.node(&XmlElement::with_children("LatLonBox", Vec::new()))),
        };
        let lat_lon_box = &mut node.children[i];
        let stored = LatLonBox::read(lat_lon_box).ok();
        for (element, value) in [
            (KmlElement::North, bounds.north),
            (KmlElement::South, bounds.south),
            (KmlElement::East, bounds.east),
            (KmlElement::West, bounds.west),
        ] {
            // An edge the edit did not move keeps its spelling.
            let unchanged = stored.is_some_and(|s| edge(&s, element) == value) && lat_lon_box.child(element).is_some();
            if !unchanged {
                set_leaf(lat_lon_box, element, Some(&value.to_string()), b);
            }
        }
        let had_rotation = lat_lon_box.child(KmlElement::Rotation).is_some();
        if bounds.rotation != 0.0 || had_rotation {
            let unchanged = stored.is_some_and(|s| s.rotation == bounds.rotation) && had_rotation;
            if !unchanged {
                set_leaf(lat_lon_box, KmlElement::Rotation, Some(&bounds.rotation.to_string()), b);
            }
        }
        if at_box.is_none() {
            node.lay_out_child(i, b.layout);
        }
        return Ok(());
    }
    let mut quad = XmlElement::with_children("LatLonQuad", vec![XmlElement::leaf("coordinates", coordinates_text(&corners))]);
    if version == KmlVersion::V2_2 {
        quad.name = KmlElement::LatLonQuad.gx_name().expect("an adopted element").to_string();
        quad.prefix = Some(model::GX_PREFIX.into());
        quad = quad.with_ns_decl(Some(model::GX_PREFIX), model::GX_NS);
    }
    let quad = b.node(&quad);
    match at_box {
        Some(i) => node.replace_child(i, quad, b.layout),
        None => {
            insert_in_order(node, b, quad);
        }
    }
    Ok(())
}

fn edge(b: &LatLonBox, element: KmlElement) -> f64 {
    match element {
        KmlElement::North => b.north,
        KmlElement::South => b.south,
        KmlElement::East => b.east,
        _ => b.west,
    }
}

/// A `<PhotoOverlay>`'s `<Point>` edited: its coordinates in place, keeping
/// its altitude members. Anything but a point is no place for a photo.
fn reconcile_photo_point(node: &mut Node, geometry: Option<&Geometry>, b: Build<'_>) -> Result<(), Unwritable> {
    let at = node.children.iter().position(|c| c.is(KmlElement::Point));
    match (geometry, at) {
        (None, Some(i)) => node.remove_child(i),
        (None, None) => {}
        (Some(Geometry::Point(p)), Some(i)) => {
            let point = &mut node.children[i];
            if !set_coordinates(point, std::slice::from_ref(p)) {
                set_leaf(point, KmlElement::Coordinates, Some(&coordinates_text(std::slice::from_ref(p))), b);
            }
        }
        (Some(g @ Geometry::Point(_)), None) => {
            insert_in_order(node, b, b.node(&geometry_to_element(g)));
        }
        (Some(_), _) => return Err(Unwritable),
    }
    Ok(())
}

/// Write `geometry`'s coordinates into `existing` without replacing it, when
/// its element can hold them. Returns whether it did.
fn update_in_place(existing: &mut Node, geometry: &Geometry) -> bool {
    match (existing.element(), geometry) {
        (Some(KmlElement::Point), Geometry::Point(p)) => set_coordinates(existing, std::slice::from_ref(p)),
        (Some(KmlElement::LineString | KmlElement::LinearRing), Geometry::LineString(points)) => {
            set_coordinates(existing, points)
        }
        (Some(KmlElement::Track), Geometry::LineString(points)) => {
            let coords: Vec<usize> =
                existing.children.iter().enumerate().filter(|(_, c)| c.is(KmlElement::Coord)).map(|(i, _)| i).collect();
            if coords.len() != points.len() {
                return false;
            }
            for (i, p) in coords.into_iter().zip(points) {
                existing.children[i].set_text(&track_coord(p));
            }
            true
        }
        _ => false,
    }
}

fn set_coordinates(existing: &mut Node, points: &[Position]) -> bool {
    let Some(i) = existing.children.iter().position(|c| c.is(KmlElement::Coordinates)) else {
        return false;
    };
    existing.children[i].replace_text(coordinates_text(points));
    true
}

/// `<coordinates>`'s text for `points`, as the writer spells it.
fn coordinates_text(points: &[Position]) -> String {
    geometry_to_element(&Geometry::LineString(points.to_vec()))
        .children
        .into_iter()
        .find(|c| c.name == "coordinates")
        .map(|c| c.text().to_string())
        .unwrap_or_default()
}

/// A track coordinate: `lon lat alt`, space-separated.
fn track_coord(p: &Position) -> String {
    let mut text = String::new();
    match p.z {
        Some(z) => {
            let _ = write!(text, "{} {} {}", p.x, p.y, z);
        }
        None => {
            let _ = write!(text, "{} {}", p.x, p.y);
        }
    }
    text
}

/// A child's place in `ogckml22.xsd`'s sequence for `parent`. `None` for
/// what the sequence does not name — extension content, which constrains
/// nothing.
///
/// A feature's own members come first, in `AbstractFeatureType`'s order,
/// then its kind's: a Placemark's geometry; an overlay's colour, draw order
/// and icon, then a `<GroundOverlay>`'s altitude and box — `gx:LatLonQuad`
/// after it, in the extension group — or a `<PhotoOverlay>`'s view and point.
fn rank(parent: KmlElement, child: KmlElement) -> Option<u8> {
    use KmlElement as K;
    if parent.is_geometry() {
        return Some(geometry_rank(child));
    }
    let sequence: &[&[KmlElement]] = match parent {
        K::TimeSpan => &[&[K::Begin], &[K::End]],
        K::LatLonBox => &[&[K::North], &[K::South], &[K::East], &[K::West], &[K::Rotation]],
        K::Icon => &[&[K::Href]],
        K::Data => &[&[K::DisplayName], &[K::Value]],
        _ => &[],
    };
    if !sequence.is_empty() {
        return sequence.iter().position(|group| group.contains(&child)).map(|p| p as u8);
    }
    Some(match child {
        K::Name => 0,
        K::Visibility => 1,
        K::Open => 2,
        K::Address => 3,
        K::PhoneNumber => 4,
        K::SnippetUpper => 5,
        K::Snippet => 6,
        K::Description => 7,
        e if e.is_view() => 8,
        e if e.is_time_primitive() => 9,
        K::StyleUrl => 10,
        e if e.is_style_selector() => 11,
        K::Region => 12,
        K::Metadata => 13,
        K::ExtendedData => 14,
        e if parent == K::Placemark && e.is_geometry() => 15,
        K::Color => 15,
        K::DrawOrder => 16,
        K::Icon => 17,
        K::Altitude if parent == K::GroundOverlay => 18,
        K::AltitudeMode | K::SeaFloorAltitudeMode if parent == K::GroundOverlay => 19,
        K::LatLonBox => 20,
        K::LatLonQuad => 21,
        K::Rotation if parent == K::PhotoOverlay => 18,
        K::ViewVolume => 19,
        K::ImagePyramid => 20,
        K::Point if parent == K::PhotoOverlay => 21,
        K::Shape => 22,
        _ => return None,
    })
}

/// Insert `child` after every sibling the sequence puts at or before it, laid
/// out as the document is, and return its index.
fn insert_in_order(node: &mut Node, b: Build<'_>, child: Node) -> usize {
    let parent = node.element();
    let rank_of = |n: &Node| rank(parent?, n.element()?);
    let at = match rank_of(&child) {
        Some(r) => node.children.iter().rposition(|c| rank_of(c).is_some_and(|cr| cr <= r)).map_or(0, |i| i + 1),
        None => node.children.len(),
    };
    node.insert_laid_out(at, child, b.layout);
    at
}

/// What writing `fc` as `target` cannot carry of its KML features' elements —
/// pre-write, in `plans/lossy-conversion-warnings.org` § 1's shape.
///
/// `carried` is whether this write gives the model back: a KML or KMZ write,
/// in either version, with its document intact. When it does not — another
/// format, a layered `.kmz` — a `<Model>` and a
/// track's per-vertex data have nowhere to go (D-K11), and on another format
/// a Z that is no EGM96 height has no CRS to say what it is (D-K12). When it
/// does, only an edit loses something: a track whose vertex count changed can
/// no longer keep its per-vertex series, and an overlay whose geometry is no
/// longer one it can hold is written as a Placemark.
pub(crate) fn drop_warnings(fc: &FeatureCollection, target: Format, carried: bool) -> Vec<String> {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Cause {
        Model,
        TrackData,
        EditedTrack,
        EditedOverlay,
        Vertical,
    }
    let mut causes = Tally::default();
    let to_kml = matches!(target, Format::Kml | Format::Kmz);
    let stated = fc.crs.as_ref().is_some_and(Crs::is_wgs84_egm96);
    for f in &fc.features {
        let Some(m) = feature_model(f) else {
            continue;
        };
        let unprojected = project::unprojected(&m.node);
        if !carried {
            if unprojected.contains(&project::Unprojected::Model) {
                causes.add(Cause::Model);
            }
            if unprojected.contains(&project::Unprojected::TrackData) {
                causes.add(Cause::TrackData);
            }
            if !to_kml && !stated && project::vertical(&m.node) != Vertical::Flat {
                causes.add(Cause::Vertical);
            }
        } else {
            if unprojected.contains(&project::Unprojected::TrackData) && track_edit_drops_data(&m.node, f) {
                causes.add(Cause::EditedTrack);
            }
            if overlay_edit_unwritable(&m.node, f) {
                causes.add(Cause::EditedOverlay);
            }
        }
    }
    let name = target.display_name();
    let mut out = Vec::new();
    causes.report(&mut out, |cause, n| match cause {
        Cause::Model => {
            format!("kml: {n} feature(s) hold <Model>, which this {name} write cannot carry; written without that geometry")
        }
        Cause::TrackData => format!(
            "kml: {n} track(s) carry per-vertex data (<when>, <angles>, <ExtendedData>) this {name} write cannot \
             carry; written as LINESTRING Z"
        ),
        Cause::EditedTrack => format!(
            "kml: {n} track(s) were edited to a different number of vertices, so their per-vertex data (<when>, \
             <angles>, <ExtendedData>) no longer lines up and is dropped; written as LINESTRING Z"
        ),
        Cause::EditedOverlay => format!(
            "kml: {n} overlay(s) were edited to a geometry an overlay cannot hold (a ground overlay takes a \
             four-cornered polygon, a photo overlay a point); written as Placemarks, without their image"
        ),
        Cause::Vertical => format!(
            "kml: {n} feature(s) have Z values that are not EGM96 heights (their altitudeMode is not absolute, \
             and the default is clampToGround), and this {name} write has no CRS that says what they are; written \
             as Z in WGS 84, with altitudeMode as a property where the source stated one"
        ),
    });
    out
}

/// Whether reconciling `f` into its stored element will replace a track
/// rather than update it in place.
fn track_edit_drops_data(stored: &Node, f: &Feature) -> bool {
    let Ok(projected) = project(stored, &Context::default()) else {
        return false;
    };
    if projected.geometry == f.geometry {
        return false;
    }
    let Some(track) = stored.children.iter().find(|c| c.element().is_some_and(KmlElement::is_geometry)) else {
        return false;
    };
    let mut probe = track.clone();
    !matches!(f.geometry, Some(ref g) if update_in_place(&mut probe, g))
}

/// Whether reconciling `f` into its stored overlay leaves it a geometry the
/// overlay cannot hold.
fn overlay_edit_unwritable(stored: &Node, f: &Feature) -> bool {
    if !stored.element().is_some_and(KmlElement::is_overlay) {
        return false;
    }
    let Ok(projected) = project(stored, &Context::default()) else {
        return false;
    };
    projected.geometry != f.geometry
        && reconcile_geometry(&mut stored.clone(), f.geometry.as_ref(), KmlVersion::V2_3, Build { prefix: None, layout: None })
            .is_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::KmlVersion;
    use crate::json::JsonValue;
    use std::rc::Rc;

    const DOC: &str = r#"<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:x="urn:x"><Document>
        <Style id="s"><LineStyle><width>3</width></LineStyle></Style>
        <Folder><name>f</name>
          <!-- keep me -->
          <Placemark id="p"><name>a</name><description>old</description><TimeStamp><when>2026</when></TimeStamp>
            <styleUrl>#s</styleUrl><x:ext k="v"/>
            <ExtendedData><Data name="k1"><value>1</value></Data><Data name="k2"><value>2</value></Data></ExtendedData>
            <Point><extrude>1</extrude><altitudeMode>absolute</altitudeMode><coordinates>1,2,3</coordinates></Point>
          </Placemark>
        </Folder></Document></kml>"#;

    fn read(source: &str) -> FeatureCollection {
        crate::kml::reader::read(source.as_bytes(), &mut Vec::new()).unwrap()
    }

    fn write_back(fc: &FeatureCollection) -> String {
        String::from_utf8(crate::kml::writer::write(fc, None, false)).unwrap()
    }

    /// Set, add or remove a property, keeping its place when it had one.
    fn set(f: &mut Feature, key: &str, value: Option<&str>) {
        let at = f.properties.iter().position(|(k, _)| &**k == key);
        match (value, at) {
            (Some(v), Some(i)) => f.properties[i].1 = JsonValue::String(v.into()),
            (Some(v), None) => f.properties.push((Rc::from(key), JsonValue::String(v.into()))),
            (None, Some(i)) => {
                f.properties.remove(i);
            }
            (None, None) => {}
        }
    }

    #[test]
    fn an_unedited_document_is_written_as_it_was_read() {
        let written = write_back(&read(DOC));
        let parse = |s: &str| crate::xml::parse_document(s).unwrap();
        assert_eq!(parse(&written), parse(DOC), "{written}");
    }

    #[test]
    fn an_edit_rewrites_only_what_it_changed() {
        let mut fc = read(DOC);
        let f = &mut fc.features[0];
        set(f, "name", Some("renamed"));
        set(f, "description", None);
        set(f, "k1", Some("one"));
        set(f, "k2", None);
        set(f, "k3", Some("three"));
        let written = write_back(&fc);
        for kept in [
            r#"<Style id="s">"#,
            "<!-- keep me -->",
            "<TimeStamp><when>2026</when></TimeStamp>",
            "<styleUrl>#s</styleUrl>",
            r#"<x:ext k="v"/>"#,
            "<extrude>1</extrude>",
        ] {
            assert!(written.contains(kept), "lost {kept}: {written}");
        }
        assert!(written.contains("<name>renamed</name>"), "{written}");
        assert!(!written.contains("description"), "{written}");
        assert!(written.contains(r#"<Data name="k1"><value>one</value></Data>"#), "{written}");
        assert!(!written.contains("k2"), "{written}");
        assert!(written.contains(r#"<Data name="k3"><value>three</value></Data>"#), "{written}");
        // And it reads back as edited, in the same folder.
        let again = read(&written);
        assert_eq!(again.features[0].properties, fc.features[0].properties);
        assert_eq!(feature_model(&again.features[0]).unwrap().address.container, vec![0, 1]);
    }

    #[test]
    fn an_added_element_goes_where_the_schema_puts_it() {
        let mut fc = read(r#"<kml><Placemark><name>a</name><styleUrl>#s</styleUrl><Point><coordinates>1,2</coordinates></Point></Placemark></kml>"#);
        set(&mut fc.features[0], "description", Some("new"));
        let written = write_back(&fc);
        assert!(written.contains("<name>a</name><description>new</description><styleUrl>"), "{written}");
    }

    #[test]
    fn an_edited_point_keeps_its_other_children() {
        let mut fc = read(DOC);
        fc.features[0].geometry = Some(Geometry::Point(Position::with_z(5.0, 6.0, 7.0)));
        let written = write_back(&fc);
        assert!(
            written.contains("<Point><extrude>1</extrude><altitudeMode>absolute</altitudeMode><coordinates>5,6,7</coordinates></Point>"),
            "{written}"
        );
    }

    const TRACK: &str = r#"<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2"><Placemark><gx:Track><when>t1</when><when>t2</when><gx:coord>1 2 3</gx:coord><gx:coord>4 5 6</gx:coord></gx:Track></Placemark></kml>"#;

    #[test]
    fn a_track_edited_to_the_same_vertex_count_keeps_its_times() {
        let mut fc = read(TRACK);
        fc.features[0].geometry =
            Some(Geometry::LineString(vec![Position::with_z(1.5, 2.0, 3.0), Position::with_z(4.0, 5.0, 6.0)]));
        assert!(drop_warnings(&fc, Format::Kml, true).is_empty());
        let written = write_back(&fc);
        assert!(written.contains("<when>t1</when><when>t2</when><gx:coord>1.5 2 3</gx:coord>"), "{written}");
    }

    #[test]
    fn a_track_edited_to_another_vertex_count_is_replaced_and_says_so() {
        let mut fc = read(TRACK);
        fc.features[0].geometry = Some(Geometry::LineString(vec![Position::with_z(1.0, 2.0, 3.0)]));
        let warnings = drop_warnings(&fc, Format::Kml, true);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("edited to a different number of vertices"), "{warnings:?}");
        let written = write_back(&fc);
        assert!(written.contains("<LineString><coordinates>1,2,3</coordinates></LineString>"), "{written}");
        assert!(!written.contains("when"), "{written}");
    }

    #[test]
    fn a_new_feature_joins_the_first_container_in_the_documents_prefix() {
        let mut fc = read(r#"<k:kml xmlns:k="http://www.opengis.net/kml/2.2"><k:Document><k:name>d</k:name></k:Document></k:kml>"#);
        fc.features.push(Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![(Rc::from("name"), JsonValue::String("new".into()))],
            native: None,
        });
        let written = write_back(&fc);
        assert!(written.contains("<k:Placemark><k:name>new</k:name>"), "{written}");
        let again = read(&written);
        assert_eq!(again.features.len(), 1);
        assert_eq!(again.features[0].geometry, Some(Geometry::Point(Position::new(1.0, 2.0))));
    }

    #[test]
    fn a_removed_feature_is_gone_and_nothing_else_is() {
        let mut fc = read(DOC);
        fc.features.clear();
        let written = write_back(&fc);
        assert!(!written.contains("Placemark"), "{written}");
        // The comment sat directly before the placemark, so it went with it.
        assert!(!written.contains("keep me"), "{written}");
        assert!(written.contains(r#"<Style id="s">"#), "{written}");
    }

    #[test]
    fn a_version_change_carries_the_document_and_reports_only_its_respellings() {
        let fc = read(DOC);
        let written = String::from_utf8(crate::kml::writer::write(&fc, Some(KmlVersion::V2_3), false)).unwrap();
        assert!(written.contains(r#"<Style id="s">"#) && written.contains("<!-- keep me -->"), "{written}");
        assert!(crate::kml::version_change_warnings(&fc, Format::Kml, KmlVersion::V2_3).is_empty(), "nothing was respelled");
        // A track is respelled, and it carries its times into either version.
        let fc = read(TRACK);
        assert_eq!(
            crate::kml::version_change_warnings(&fc, Format::Kml, KmlVersion::V2_3),
            ["kml: input is KML 2.2; written as KML 2.3 (gx:Track→Track, gx:coord→coord ×2)"]
        );
        assert!(crate::kml::drop_warnings(&fc, Format::Kml).is_empty());
        let written = String::from_utf8(crate::kml::writer::write(&fc, Some(KmlVersion::V2_3), false)).unwrap();
        assert!(written.contains("<Track><when>t1</when><when>t2</when><coord>1 2 3</coord>"), "{written}");
    }

    fn write_as(fc: &FeatureCollection, version: KmlVersion) -> String {
        String::from_utf8(crate::kml::writer::write(fc, Some(version), false)).unwrap()
    }

    fn edited(source: &str, edit: impl FnOnce(&mut Feature)) -> (FeatureCollection, String) {
        let mut fc = read(source);
        edit(&mut fc.features[0]);
        let written = write_back(&fc);
        (fc, written)
    }

    #[test]
    fn an_edited_time_keeps_its_primitive_and_changes_kind_when_it_must() {
        let source = r#"<kml><Placemark><name>a</name><TimeStamp id="t"><when>2026</when></TimeStamp></Placemark></kml>"#;
        let (_, written) = edited(source, |f| set(f, "timestamp", Some("2027-01-01")));
        assert!(written.contains(r#"<TimeStamp id="t"><when>2027-01-01</when></TimeStamp>"#), "{written}");
        let (_, written) = edited(source, |f| {
            set(f, "timestamp", None);
            set(f, "begin", Some("2026-01"));
        });
        assert!(written.contains("<name>a</name><TimeSpan><begin>2026-01</begin></TimeSpan>"), "{written}");
        // A value no time element admits is not lost: it moves to
        // <ExtendedData> under its own key, and reads back the same.
        let (fc, written) = edited(source, |f| set(f, "timestamp", Some("last tuesday")));
        assert!(!written.contains("TimeStamp"), "{written}");
        assert!(written.contains(r#"<Data name="timestamp"><value>last tuesday</value></Data>"#), "{written}");
        assert_eq!(read(&written).features[0].properties, fc.features[0].properties);
    }

    #[test]
    fn an_edited_altitude_mode_is_set_where_the_geometry_has_it() {
        let source = "<kml><Placemark><MultiGeometry><Point><altitudeMode>relativeToGround</altitudeMode>\
            <coordinates>1,2,3</coordinates></Point><LineString><altitudeMode>relativeToGround</altitudeMode>\
            <coordinates>1,2,3 4,5,6</coordinates></LineString></MultiGeometry></Placemark></kml>";
        let (_, written) = edited(source, |f| {
            set(f, "altitudeMode", Some("absolute"));
            set(f, "tessellate", Some("1"));
        });
        assert_eq!(written.matches("<altitudeMode>absolute</altitudeMode>").count(), 2, "{written}");
        assert!(written.contains("<tessellate>1</tessellate><altitudeMode>absolute</altitudeMode><coordinates>1,2,3 4,5,6"), "{written}");
        assert_eq!(read(&written).crs, Some(crate::crs::Crs::wgs84_egm96()));
        // A Point has no <tessellate>, so the key went on the line only.
        assert_eq!(written.matches("tessellate").count(), 2, "{written}");
    }

    #[test]
    fn a_sea_floor_mode_added_to_a_2_2_document_is_gx_and_declares_itself() {
        let source = "<kml><Placemark><Point><altitudeMode>absolute</altitudeMode><coordinates>1,2,3</coordinates></Point></Placemark></kml>";
        let (fc, written) = edited(source, |f| {
            set(f, "altitudeMode", None);
            set(f, "seaFloorAltitudeMode", Some("clampToSeaFloor"));
        });
        assert!(
            written.contains(r#"<Point><gx:altitudeMode xmlns:gx="http://www.google.com/kml/ext/2.2">clampToSeaFloor</gx:altitudeMode><coordinates>"#),
            "{written}"
        );
        assert_eq!(read(&written).features[0].properties, fc.features[0].properties);
    }

    const OVERLAY: &str = r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><GroundOverlay id="g"><name>o</name>
        <color>7fffffff</color><Icon><href>img.png</href><refreshMode>onInterval</refreshMode></Icon>
        <LatLonBox><north>61</north><south>59</south><east>12</east><west>8</west><rotation>45</rotation></LatLonBox>
        </GroundOverlay></Document></kml>"#;

    fn shifted(g: &Geometry, dx: f64) -> Geometry {
        let mut g = g.clone();
        g.for_each_position_mut(&mut |p: &mut Position| p.x += dx);
        g
    }

    #[test]
    fn a_moved_ground_overlay_keeps_its_box_and_its_rotation() {
        let (fc, written) = edited(OVERLAY, |f| f.geometry = Some(shifted(f.geometry.as_ref().unwrap(), 1.0)));
        assert!(written.contains("<GroundOverlay id=\"g\">"), "{written}");
        assert!(written.contains("<north>61</north><south>59</south><east>13</east><west>9</west><rotation>45</rotation>"), "{written}");
        assert!(drop_warnings(&fc, Format::Kml, true).is_empty());
    }

    #[test]
    fn a_reshaped_ground_overlay_becomes_a_quad_in_its_versions_spelling() {
        let reshape = |f: &mut Feature| {
            f.geometry = Some(Geometry::Polygon(vec![vec![
                Position::new(0.0, 0.0),
                Position::new(2.0, 0.0),
                Position::new(2.5, 1.0),
                Position::new(0.0, 1.0),
                Position::new(0.0, 0.0),
            ]]))
        };
        let (fc, written) = edited(OVERLAY, reshape);
        assert!(
            written.contains(r#"<gx:LatLonQuad xmlns:gx="http://www.google.com/kml/ext/2.2"><coordinates>0,0 2,0 2.5,1 0,1</coordinates></gx:LatLonQuad>"#),
            "{written}"
        );
        assert!(!written.contains("LatLonBox"), "{written}");
        assert_eq!(read(&written).features[0].geometry, fc.features[0].geometry);
        // A 2.3 document spells it in its own namespace.
        let (_, written) = edited(&OVERLAY.replace("<kml ", r#"<kml version="2.3" "#), reshape);
        assert!(written.contains("<LatLonQuad><coordinates>0,0 2,0 2.5,1 0,1</coordinates></LatLonQuad>"), "{written}");
    }

    #[test]
    fn an_overlay_edited_to_a_shape_it_cannot_hold_is_a_placemark_and_says_so() {
        let (fc, written) = edited(OVERLAY, |f| {
            f.geometry = Some(Geometry::Polygon(vec![vec![
                Position::new(0.0, 0.0),
                Position::new(1.0, 0.0),
                Position::new(0.0, 1.0),
                Position::new(0.0, 0.0),
            ]]))
        });
        assert!(!written.contains("GroundOverlay"), "{written}");
        assert!(written.contains(r#"<Data name="icon"><value>img.png</value></Data>"#), "{written}");
        let warnings = drop_warnings(&fc, Format::Kml, true);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("kml: 1 overlay(s) were edited to a geometry an overlay cannot hold"), "{warnings:?}");
    }

    #[test]
    fn an_overlays_icon_edit_keeps_the_rest_of_its_icon() {
        let (_, written) = edited(OVERLAY, |f| set(f, "icon", Some("other.png")));
        assert!(written.contains("<Icon><href>other.png</href><refreshMode>onInterval</refreshMode></Icon>"), "{written}");
        let (_, written) = edited(OVERLAY, |f| set(f, "icon", None));
        assert!(written.contains("<Icon><refreshMode>onInterval</refreshMode></Icon>"), "{written}");
    }

    #[test]
    fn a_photo_overlay_moves_its_point_and_refuses_a_line() {
        let source = "<kml><PhotoOverlay><Icon><href>p.jpg</href></Icon><Point><altitudeMode>absolute</altitudeMode>\
            <coordinates>1,2,3</coordinates></Point></PhotoOverlay></kml>";
        let (_, written) = edited(source, |f| f.geometry = Some(Geometry::Point(Position::with_z(4.0, 5.0, 6.0))));
        assert!(written.contains("<Point><altitudeMode>absolute</altitudeMode><coordinates>4,5,6</coordinates></Point></PhotoOverlay>"), "{written}");
        let (_, written) = edited(source, |f| {
            f.geometry = Some(Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]))
        });
        assert!(written.contains("<Placemark><ExtendedData><Data name=\"icon\">"), "{written}");
    }

    #[test]
    fn an_added_property_goes_ahead_of_schema_data() {
        let source = "<kml><Document><Schema id='s'><SimpleField name='n' type='int'/></Schema><Placemark>\
            <ExtendedData><SchemaData schemaUrl='#s'><SimpleData name='n'>7</SimpleData></SchemaData></ExtendedData>\
            </Placemark></Document></kml>";
        let (_, written) = edited(source, |f| set(f, "added", Some("x")));
        assert!(
            written.contains(r#"<ExtendedData><Data name="added"><value>x</value></Data><SchemaData"#),
            "{written}"
        );
    }

    #[test]
    fn a_typed_value_an_edit_did_not_touch_keeps_its_spelling() {
        let source = "<kml><Document><Schema id='s'><SimpleField name='d' type='double'/><SimpleField name='n' type='int'/></Schema>\
            <Placemark><name>a</name><ExtendedData><SchemaData schemaUrl='#s'><SimpleData name='d'>1.50</SimpleData>\
            <SimpleData name='n'>007</SimpleData></SchemaData></ExtendedData></Placemark></Document></kml>";
        let (_, written) = edited(source, |f| {
            set(f, "name", Some("b"));
            f.properties.iter_mut().find(|(k, _)| &**k == "n").unwrap().1 = JsonValue::Int(8);
        });
        assert!(written.contains("<SimpleData name=\"d\">1.50</SimpleData><SimpleData name=\"n\">8</SimpleData>"), "{written}");
    }

    #[test]
    fn heights_no_datum_states_are_reported_on_a_cross_format_write() {
        let fc = read("<kml><Placemark><Point><coordinates>1,2,3</coordinates></Point></Placemark></kml>");
        let warnings = drop_warnings(&fc, Format::GeoJson, false);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("kml: 1 feature(s) have Z values that are not EGM96 heights"), "{warnings:?}");
        assert!(drop_warnings(&fc, Format::Kml, false).is_empty(), "a KML write keeps what the mode says");
        let fc = read("<kml><Placemark><Point><altitudeMode>absolute</altitudeMode><coordinates>1,2,3</coordinates></Point></Placemark></kml>");
        assert!(drop_warnings(&fc, Format::GeoJson, false).is_empty(), "EPSG:9707 says it, and the CRS warning owns its loss");
    }

    #[test]
    fn a_version_change_keeps_times_altitude_and_types() {
        let source = "<kml><Document><Schema id='s'><SimpleField name='n' type='int'/></Schema>\
            <Placemark><TimeStamp><when>2026-01-01T00:00:00Z</when></TimeStamp><ExtendedData><SchemaData schemaUrl='#s'>\
            <SimpleData name='n'>7</SimpleData></SchemaData></ExtendedData><Point><altitudeMode>absolute</altitudeMode>\
            <coordinates>1,2,3</coordinates></Point></Placemark></Document></kml>";
        let fc = read(source);
        let written = write_as(&fc, KmlVersion::V2_3);
        assert!(written.contains("<TimeStamp><when>2026-01-01T00:00:00Z</when></TimeStamp>"), "{written}");
        assert!(written.contains(r#"<Schema id="s"><SimpleField name="n" type="int"/></Schema>"#), "{written}");
        assert!(written.contains("<Point><altitudeMode>absolute</altitudeMode>"), "{written}");
        let again = read(&written);
        assert_eq!(again.features[0].properties, fc.features[0].properties);
        assert_eq!(again.schema, fc.schema);
        assert_eq!(again.crs, fc.crs);
    }

    const LAID_OUT: &str = "<kml xmlns=\"http://www.opengis.net/kml/2.2\">\n  <Document>\n    <name>d</name>\n    \
        <Placemark>\n      <name>a</name>\n      <description>gone</description>\n      <Point>\n        \
        <coordinates>1,2</coordinates>\n      </Point>\n    </Placemark>\n    <Placemark>\n      <name>b</name>\n    \
        </Placemark>\n  </Document>\n</kml>\n";

    #[test]
    fn what_an_edit_adds_is_laid_out_as_the_document_is_and_what_it_removes_leaves_no_line() {
        let mut fc = read(LAID_OUT);
        let mut added = Feature { native: None, ..fc.features[0].clone() };
        added.properties.retain(|(k, _)| &**k == "name");
        set(&mut added, "name", Some("c"));
        set(&mut fc.features[0], "description", None);
        set(&mut fc.features[0], "k", Some("v"));
        fc.features[1] = added;
        assert_eq!(
            write_back(&fc),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <kml xmlns=\"http://www.opengis.net/kml/2.2\">\n  <Document>\n    <name>d</name>\n    <Placemark>\n      \
             <name>a</name>\n      <ExtendedData>\n        <Data name=\"k\">\n          <value>v</value>\n        </Data>\n      \
             </ExtendedData>\n      <Point>\n        <coordinates>1,2</coordinates>\n      </Point>\n    </Placemark>\n    \
             <Placemark>\n      <name>c</name>\n      <Point>\n        <coordinates>1,2</coordinates>\n      </Point>\n    \
             </Placemark>\n  </Document>\n</kml>\n"
        );
    }

    #[test]
    fn an_edit_keeps_the_documents_newline_and_indent() {
        let (_, written) = edited("<kml>\r\n\t<Placemark>\r\n\t\t<name>a</name>\r\n\t</Placemark>\r\n</kml>", |f| {
            set(f, "description", Some("d"));
            set(f, "k", Some("v"));
        });
        assert!(
            written.ends_with(
                "<kml>\r\n\t<Placemark>\r\n\t\t<name>a</name>\r\n\t\t<description>d</description>\r\n\t\t<ExtendedData>\r\n\
                 \t\t\t<Data name=\"k\">\r\n\t\t\t\t<value>v</value>\r\n\t\t\t</Data>\r\n\t\t</ExtendedData>\r\n\t</Placemark>\r\n</kml>"
            ),
            "{written:?}"
        );
    }

    #[test]
    fn a_compact_document_and_a_compact_sibling_keep_their_habit() {
        let (_, written) = edited("<kml><Placemark><name>a</name></Placemark></kml>", |f| set(f, "k", Some("v")));
        assert!(written.ends_with(r#"<Placemark><name>a</name><ExtendedData><Data name="k"><value>v</value></Data></ExtendedData></Placemark></kml>"#), "{written}");
        let mut fc = read("<kml>\n  <Document>\n    <Placemark><name>a</name></Placemark>\n  </Document>\n</kml>");
        let mut added = Feature { native: None, ..fc.features[0].clone() };
        set(&mut added, "name", Some("c"));
        fc.features.push(added);
        let written = write_back(&fc);
        assert!(written.ends_with("    <Placemark><name>a</name></Placemark>\n    <Placemark><name>c</name></Placemark>\n  </Document>\n</kml>"), "{written:?}");
    }

    #[test]
    fn a_rewritten_leaf_keeps_the_whitespace_around_its_value() {
        let (_, written) = edited("<kml><Placemark><name>\n  Zürich\n</name></Placemark></kml>", |f| set(f, "name", Some("Bern")));
        assert!(written.contains("<name>\n  Bern\n</name>"), "{written}");
    }

    #[test]
    fn what_lowering_drops_takes_its_line_with_it() {
        let source = "<kml xmlns=\"http://www.opengis.net/kml/2.2\" version=\"2.3\">\n  <Placemark>\n    <Region>\n      \
            <LatLonAltBox><north>1</north><south>0</south><east>1</east><west>0</west></LatLonAltBox>\n      \
            <LatLonBox><north>1</north><south>0</south><east>1</east><west>0</west></LatLonBox>\n    </Region>\n  \
            </Placemark>\n</kml>\n";
        let written = write_as(&read(source), KmlVersion::V2_2);
        assert!(
            written.contains("<Region>\n      <LatLonAltBox><north>1</north><south>0</south><east>1</east><west>0</west></LatLonAltBox>\n    </Region>"),
            "{written}"
        );
    }
}
