//! The IR as a projection of the KML model (`plans/kml-standard.org` D-K4, K4).
//!
//! One function, [`project`], says what a feature element *is* in IR terms —
//! its properties and geometry. The reader calls it to build each
//! [`crate::Feature`]; the writer calls it again on the stored node to tell
//! which members an edit changed, so the two can never disagree about what was
//! projected.
//!
//! Three feature elements project: `<Placemark>`, and the two overlays with
//! geography (D-K14) — `<GroundOverlay>` as the polygon its image covers and
//! `<PhotoOverlay>` as its `<Point>`. A feature's projected properties come
//! in a fixed order: its own `name` and `description`, its time (D-K15), an
//! overlay's `icon`, its geometry's altitude members (D-K12), then
//! `<ExtendedData>`, typed by its `<Schema>` where it has one (D-K9).

use std::collections::HashMap;
use std::rc::Rc;

use crate::error::{Error, Result};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::kml::model::{KmlElement, Node};
use crate::schema::DeclType;

fn err(message: &str) -> Error {
    Error::Convert(format!("kml: {message}"))
}

/// A feature element's projection: its properties, in the order the IR holds
/// them, and its geometry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Projection {
    pub properties: Vec<(Rc<str>, JsonValue)>,
    pub geometry: Option<Geometry>,
}

/// The property keys a feature's own elements project to; every other
/// property travels in `<ExtendedData>`.
pub(crate) const NAME: &str = "name";
pub(crate) const DESCRIPTION: &str = "description";
/// `<TimeStamp><when>`, and `<TimeSpan>`'s `<begin>`/`<end>` — GDAL LIBKML's
/// names, so a file GDAL wrote comes back to KML with its times (D-K15).
pub(crate) const TIMESTAMP: &str = "timestamp";
pub(crate) const BEGIN: &str = "begin";
pub(crate) const END: &str = "end";
/// An overlay's `<Icon><href>`, LIBKML's name for it too (D-K14).
pub(crate) const ICON: &str = "icon";

/// The members of a geometry that say what its Z means, projected under
/// their own element names (D-K12).
pub(crate) const ALTITUDE_MEMBERS: [(&str, KmlElement); 4] = [
    ("altitudeMode", KmlElement::AltitudeMode),
    ("seaFloorAltitudeMode", KmlElement::SeaFloorAltitudeMode),
    ("extrude", KmlElement::Extrude),
    ("tessellate", KmlElement::Tessellate),
];

/// Whether `element` is a feature the IR projects. Placemarks, and the
/// overlays that have geography (D-K14); a `<ScreenOverlay>` is fixed to the
/// screen and stays in the document's native.
pub(crate) fn is_projected(element: KmlElement) -> bool {
    matches!(element, KmlElement::Placemark | KmlElement::GroundOverlay | KmlElement::PhotoOverlay)
}

/// Every property key a feature element of `element`'s kind can hold as one
/// of its own elements. A property under one of these keys is written as that
/// element when its value is one the element admits, and in `<ExtendedData>`
/// otherwise — so a key is never lost, only moved.
pub(crate) fn element_keys(element: KmlElement) -> impl Iterator<Item = &'static str> {
    let overlay = element == KmlElement::GroundOverlay || element == KmlElement::PhotoOverlay;
    let geometry = element != KmlElement::GroundOverlay;
    [NAME, DESCRIPTION, TIMESTAMP, BEGIN, END]
        .into_iter()
        .chain(overlay.then_some(ICON))
        .chain(ALTITUDE_MEMBERS.into_iter().map(|(k, _)| k).filter(move |_| geometry))
}

/// What a feature's projection reads from the document around it: the
/// `<Schema>`s its `<SchemaData>` names, by the `#id` that names them.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Context {
    schemas: HashMap<String, Vec<(String, FieldType)>>,
}

impl Context {
    /// The schemas `root` declares. `<Schema>` is a child of `<Document>` only,
    /// so the walk goes through containers and no further.
    pub fn of(root: &Node) -> Context {
        let mut ctx = Context::default();
        ctx.collect(root);
        ctx
    }

    fn collect(&mut self, container: &Node) {
        for child in &container.children {
            match child.element() {
                Some(KmlElement::Schema) => {
                    let Some(id) = child.attr("id") else { continue };
                    let fields = child
                        .children_of(KmlElement::SimpleField)
                        .filter_map(|f| Some((f.attr("name")?.to_string(), FieldType::from_kml(f.attr("type")?)?)))
                        .collect();
                    self.schemas.entry(format!("#{id}")).or_insert(fields);
                }
                Some(e) if e.is_container() || e == KmlElement::Kml => self.collect(child),
                _ => {}
            }
        }
    }

    /// The declared type of field `name` in the schema `schema_url` names, if
    /// this document declares one.
    pub fn field_type(&self, schema_url: Option<&str>, name: &str) -> Option<FieldType> {
        let fields = self.schemas.get(schema_url?)?;
        fields.iter().find(|(n, _)| n == name).map(|(_, t)| *t)
    }

    /// Every field this document's schemas declare, as the IR's column
    /// declarations. A name two schemas declare with different types declares
    /// neither: there is no one type to give the column.
    pub fn declarations(&self) -> Vec<(String, DeclType)> {
        let mut ids: Vec<&String> = self.schemas.keys().collect();
        ids.sort();
        let mut out: Vec<(String, DeclType)> = Vec::new();
        let mut conflicted: Vec<String> = Vec::new();
        for id in ids {
            for (name, ty) in &self.schemas[id] {
                match out.iter().find(|(n, _)| n == name) {
                    Some((_, t)) if *t != ty.decl() => conflicted.push(name.clone()),
                    Some(_) => {}
                    None => out.push((name.clone(), ty.decl())),
                }
            }
        }
        out.retain(|(n, _)| !conflicted.contains(n));
        out
    }
}

/// A `<SimpleField>`'s `type` — the eight §9.9 names, which are XML Schema's
/// own types of the same names (D-K9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldType {
    String,
    Int,
    UInt,
    Short,
    UShort,
    Float,
    Double,
    Bool,
}

impl FieldType {
    pub fn from_kml(name: &str) -> Option<FieldType> {
        Some(match name.trim() {
            "string" => FieldType::String,
            "int" => FieldType::Int,
            "uint" => FieldType::UInt,
            "short" => FieldType::Short,
            "ushort" => FieldType::UShort,
            "float" => FieldType::Float,
            "double" => FieldType::Double,
            "bool" => FieldType::Bool,
            _ => return None,
        })
    }

    pub fn kml_name(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Int => "int",
            FieldType::UInt => "uint",
            FieldType::Short => "short",
            FieldType::UShort => "ushort",
            FieldType::Float => "float",
            FieldType::Double => "double",
            FieldType::Bool => "bool",
        }
    }

    /// The column declaration this field type reads as.
    pub fn decl(self) -> DeclType {
        match self {
            FieldType::String => DeclType::String,
            FieldType::Int => DeclType::Int32,
            FieldType::UInt => DeclType::UInt32,
            FieldType::Short => DeclType::Int16,
            FieldType::UShort => DeclType::UInt16,
            FieldType::Float => DeclType::Float32,
            FieldType::Double => DeclType::Float64,
            FieldType::Bool => DeclType::Bool,
        }
    }

    /// The field type a column declared `ty` is written as: its own where KML
    /// has one, the next wider integer for a byte, and `string` for what KML
    /// cannot type — a 64-bit integer, which no KML number holds exactly,
    /// and JSON, a time, a date or bytes. `Format::recast` states the
    /// composition with [`Self::decl`].
    pub fn for_decl(ty: DeclType) -> FieldType {
        match ty {
            DeclType::Bool => FieldType::Bool,
            DeclType::Int8 | DeclType::Int16 => FieldType::Short,
            DeclType::UInt8 | DeclType::UInt16 => FieldType::UShort,
            DeclType::Int32 => FieldType::Int,
            DeclType::UInt32 => FieldType::UInt,
            DeclType::Float32 => FieldType::Float,
            DeclType::Float64 => FieldType::Double,
            _ => FieldType::String,
        }
    }

    /// `text` as a value of this type, or as the text itself when it is not
    /// one — a value a producer got wrong is kept, not refused or guessed at.
    /// Empty text in a typed field is no value at all.
    pub fn value(self, text: &str) -> JsonValue {
        if self == FieldType::String {
            return JsonValue::String(text.to_string());
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return JsonValue::Null;
        }
        let range = match self {
            FieldType::Int => Some((i32::MIN as i64, i32::MAX as i64)),
            FieldType::UInt => Some((0, u32::MAX as i64)),
            FieldType::Short => Some((i16::MIN as i64, i16::MAX as i64)),
            FieldType::UShort => Some((0, u16::MAX as i64)),
            _ => None,
        };
        let typed = match self {
            _ if range.is_some() => {
                let (lo, hi) = range.unwrap();
                trimmed.parse::<i64>().ok().filter(|n| (lo..=hi).contains(n)).map(JsonValue::Int)
            }
            // XML Schema's float lexical space is digits, a sign, a point and
            // an exponent; Rust's parser also takes `inf` and `NaN`, which no
            // JSON number holds.
            FieldType::Float | FieldType::Double => trimmed
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
                .then(|| trimmed.parse::<f64>().ok())
                .flatten()
                .filter(|f| f.is_finite())
                .map(JsonValue::Float),
            FieldType::Bool => match trimmed {
                "true" | "1" => Some(JsonValue::Bool(true)),
                "false" | "0" => Some(JsonValue::Bool(false)),
                _ => None,
            },
            _ => None,
        };
        typed.unwrap_or_else(|| JsonValue::String(text.to_string()))
    }
}

/// Project a feature element: a `<Placemark>`, `<GroundOverlay>` or
/// `<PhotoOverlay>`.
///
/// `<name>`/`<description>` become ordinary properties (cheap, commonly
/// consumed); a `<TimeStamp>`'s `<when>` becomes `timestamp` and a
/// `<TimeSpan>` `begin`/`end`, each as the source's text (D-K15); an
/// overlay's `<Icon><href>` becomes `icon`; the geometry's altitude members
/// become properties of their own names, only where the source had them
/// (D-K12); then `<ExtendedData>`'s `<Data>` values as text and its
/// `<SimpleData>` values typed by the `<Schema>` they name (D-K9). A
/// `Data`/`SimpleData` with no `name` attribute has no key to attach to, so it
/// is left to the model rather than guessed at.
pub(crate) fn project(feature: &Node, ctx: &Context) -> Result<Projection> {
    let mut properties: Vec<(Rc<str>, JsonValue)> = Vec::new();
    let mut push = |key: &str, value: JsonValue| properties.push((Rc::from(key), value));
    if let Some(name) = feature.child(KmlElement::Name) {
        push(NAME, JsonValue::String(name.text_trimmed().to_string()));
    }
    if let Some(description) = feature.child(KmlElement::Description) {
        push(DESCRIPTION, JsonValue::String(description.text_trimmed().to_string()));
    }
    for (key, text) in time_values(feature) {
        push(key, JsonValue::String(text.to_string()));
    }
    if let Some(href) = icon_href(feature) {
        push(ICON, JsonValue::String(href.to_string()));
    }
    for (key, text) in altitude_values(feature) {
        push(key, JsonValue::String(text.to_string()));
    }
    if let Some(extended_data) = feature.child(KmlElement::ExtendedData) {
        for (key, value) in extended_data_values(extended_data, ctx) {
            push(key, value);
        }
    }
    let geometry = match feature.element() {
        Some(KmlElement::GroundOverlay) => footprint(feature)?,
        _ => match geometry_of(feature) {
            Some(g) => parse_geometry(g)?,
            None => None,
        },
    };
    Ok(Projection { properties, geometry })
}

/// The feature's time, as `(key, text)`: a `<TimeStamp>`'s `<when>`, or a
/// `<TimeSpan>`'s `<begin>` and `<end>`, whichever it has.
pub(crate) fn time_values(feature: &Node) -> Vec<(&'static str, &str)> {
    let Some(time) = feature.children.iter().find(|c| c.element().is_some_and(KmlElement::is_time_primitive)) else {
        return Vec::new();
    };
    let members: &[(&str, KmlElement)] = match time.element() {
        Some(KmlElement::TimeStamp) => &[(TIMESTAMP, KmlElement::When)],
        _ => &[(BEGIN, KmlElement::Begin), (END, KmlElement::End)],
    };
    members.iter().filter_map(|(key, el)| Some((*key, time.child(*el)?.text_trimmed()))).collect()
}

/// An overlay's `<Icon><href>`.
pub(crate) fn icon_href(feature: &Node) -> Option<&str> {
    if !feature.element().is_some_and(KmlElement::is_overlay) {
        return None;
    }
    Some(feature.child(KmlElement::Icon)?.child(KmlElement::Href)?.text_trimmed())
}

/// The geometry element a feature's altitude members and geometry come from:
/// a Placemark's geometry, or a `<PhotoOverlay>`'s `<Point>`. A
/// `<GroundOverlay>` has none — its footprint is a box on the ground.
pub(crate) fn geometry_of(feature: &Node) -> Option<&Node> {
    match feature.element()? {
        KmlElement::Placemark => feature.children.iter().find(|c| c.element().is_some_and(KmlElement::is_geometry)),
        KmlElement::PhotoOverlay => feature.child(KmlElement::Point),
        _ => None,
    }
}

/// The geometry elements under `geometry` that can each say what their own Z
/// means: itself, and every member of a `<MultiGeometry>` or `<MultiTrack>`,
/// recursively.
pub(crate) fn carriers(geometry: &Node) -> Vec<&Node> {
    let mut out = vec![geometry];
    if matches!(geometry.element(), Some(KmlElement::MultiGeometry | KmlElement::MultiTrack)) {
        for child in geometry.children.iter().filter(|c| c.element().is_some_and(KmlElement::is_geometry)) {
            out.extend(carriers(child));
        }
    }
    out
}

/// Whether a geometry element of kind `geometry` has `member` among its
/// children, by `ogckml23.xsd`: `<extrude>` on everything but a track and a
/// model, `<tessellate>` only on the line-like three, and an altitude mode on
/// every geometry but `<MultiGeometry>`, which has none of the four.
pub(crate) fn can_carry(geometry: KmlElement, member: KmlElement) -> bool {
    use KmlElement as K;
    match member {
        K::Extrude => matches!(geometry, K::Point | K::LineString | K::LinearRing | K::Polygon),
        K::Tessellate => matches!(geometry, K::LineString | K::LinearRing | K::Polygon),
        K::AltitudeMode | K::SeaFloorAltitudeMode => geometry.is_geometry() && geometry != K::MultiGeometry,
        _ => false,
    }
}

/// The feature's altitude members, as `(key, text)`: each one its geometry
/// states, when every carrier that states it states the same. Members that
/// disagree across a `<MultiGeometry>` have no one value to project, and stay
/// in the model.
pub(crate) fn altitude_values(feature: &Node) -> Vec<(&'static str, &str)> {
    let Some(geometry) = geometry_of(feature) else {
        return Vec::new();
    };
    let carriers = carriers(geometry);
    let mut out = Vec::new();
    for (key, member) in ALTITUDE_MEMBERS {
        let mut values = carriers.iter().filter_map(|c| c.child(member)).map(Node::text_trimmed);
        let Some(first) = values.next() else { continue };
        if values.all(|v| v == first) {
            out.push((key, first));
        }
    }
    out
}

/// Whether `value` is a time `<when>`, `<begin>` and `<end>` admit: KML's
/// `dateTimeType`, the union of XML Schema's `dateTime`, `date`, `gYearMonth`
/// and `gYear` (OGC 12-007r2 §16.11).
pub(crate) fn is_kml_time(value: &str) -> bool {
    let Some(rest) = year(value) else { return false };
    let rest = match rest.strip_prefix('-') {
        // `gYear`, perhaps zoned.
        None => return zone(rest),
        Some(r) => r,
    };
    let Some(rest) = two_digits(rest) else { return false };
    let rest = match rest.strip_prefix('-') {
        None => return zone(rest),
        Some(r) => r,
    };
    let Some(rest) = two_digits(rest) else { return false };
    match rest.strip_prefix('T') {
        None => zone(rest),
        Some(time) => time_of_day(time).is_some_and(zone),
    }
}

/// Whether `value` is a whole XML Schema `dateTime` — a date *and* a time of
/// day — the only one of KML's time spellings a date-time column can declare
/// (D-K15).
pub(crate) fn is_xsd_date_time(value: &str) -> bool {
    value.contains('T') && is_kml_time(value)
}

/// `-?YYYY+`, returning what follows.
fn year(s: &str) -> Option<&str> {
    let s = s.strip_prefix('-').unwrap_or(s);
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    (digits >= 4).then(|| &s[digits..])
}

fn two_digits(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_digit() && b[1].is_ascii_digit()).then(|| &s[2..])
}

/// `hh:mm:ss(.s+)?`, returning what follows.
fn time_of_day(s: &str) -> Option<&str> {
    let s = two_digits(s)?.strip_prefix(':')?;
    let s = two_digits(s)?.strip_prefix(':')?;
    let s = two_digits(s)?;
    match s.strip_prefix('.') {
        Some(frac) => {
            let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0).then(|| &frac[digits..])
        }
        None => Some(s),
    }
}

/// Nothing, `Z`, or `±hh:mm`.
fn zone(s: &str) -> bool {
    match s.as_bytes().first() {
        None => true,
        Some(b'Z') => s.len() == 1,
        Some(b'+' | b'-') => two_digits(&s[1..]).and_then(|r| r.strip_prefix(':')).and_then(two_digits) == Some(""),
        _ => false,
    }
}

/// `<ExtendedData>`'s named values, in document order: each `<Data>`'s
/// `<value>` as text, then each `<SchemaData>`'s `<SimpleData>`, typed by its
/// field's declaration where the `<Schema>` it names is in this document.
pub(crate) fn extended_data_values<'n>(extended_data: &'n Node, ctx: &Context) -> Vec<(&'n str, JsonValue)> {
    let mut out = Vec::new();
    for data in extended_data.children_of(KmlElement::Data) {
        if let Some(name) = data.attr("name") {
            out.push((name, data_value(data)));
        }
    }
    for schema_data in extended_data.children_of(KmlElement::SchemaData) {
        let url = schema_data.attr("schemaUrl");
        for simple in schema_data.children_of(KmlElement::SimpleData) {
            if let Some(name) = simple.attr("name") {
                out.push((name, simple_data_value(simple, url, ctx)));
            }
        }
    }
    out
}

/// A `<Data>`'s value: its `<value>`'s text, untyped (D17).
pub(crate) fn data_value(data: &Node) -> JsonValue {
    JsonValue::String(data.child(KmlElement::Value).map(Node::text_trimmed).unwrap_or("").to_string())
}

/// A `<SimpleData>`'s value in a `<SchemaData schemaUrl=url>`.
pub(crate) fn simple_data_value(simple: &Node, url: Option<&str>, ctx: &Context) -> JsonValue {
    let text = simple.text_trimmed();
    match simple.attr("name").and_then(|name| ctx.field_type(url, name)) {
        Some(ty) => ty.value(text),
        None => JsonValue::String(text.to_string()),
    }
}

/// A geometry element's IR geometry, or `None` for one the IR has no variant
/// for (`<Model>`), which the model carries. `node` must be a geometry.
pub(crate) fn parse_geometry(node: &Node) -> Result<Option<Geometry>> {
    let element = node.element().expect("a geometry element");
    Ok(Some(match element {
        KmlElement::Point => parse_point(node)?,
        KmlElement::LineString => parse_line_string(node)?,
        // A bare ring is a boundary, not an area: a line string, as LIBKML's
        // own round trip writes it.
        KmlElement::LinearRing => parse_line_string(node)?,
        KmlElement::Polygon => parse_polygon(node)?,
        KmlElement::MultiGeometry => return parse_multi_geometry(node),
        KmlElement::Track => parse_track(node)?,
        KmlElement::MultiTrack => parse_multi_track(node)?,
        KmlElement::Model => return Ok(None),
        other => unreachable!("{other:?} is not in the geometry group"),
    }))
}

fn coordinates_of(node: &Node) -> Result<&Node> {
    node.child(KmlElement::Coordinates)
        .ok_or_else(|| err(&format!("<{}> missing <coordinates>", node.element().map_or("?", KmlElement::name))))
}

fn parse_point(node: &Node) -> Result<Geometry> {
    let positions = parse_coordinates(coordinates_of(node)?.text_trimmed())?;
    let pos = positions.into_iter().next().ok_or_else(|| err("<Point><coordinates> is empty"))?;
    Ok(Geometry::Point(pos))
}

fn parse_line_string(node: &Node) -> Result<Geometry> {
    Ok(Geometry::LineString(parse_coordinates(coordinates_of(node)?.text_trimmed())?))
}

/// A ring's coordinates, at `<outerBoundaryIs>`/`<innerBoundaryIs>` ->
/// `<LinearRing>` -> `<coordinates>`.
fn parse_ring(boundary: &Node) -> Result<Vec<Position>> {
    let ring = boundary.child(KmlElement::LinearRing).ok_or_else(|| {
        err(&format!("<{}> missing <LinearRing>", boundary.element().map_or("?", KmlElement::name)))
    })?;
    parse_coordinates(coordinates_of(ring)?.text_trimmed())
}

fn parse_polygon(node: &Node) -> Result<Geometry> {
    let outer =
        node.child(KmlElement::OuterBoundaryIs).ok_or_else(|| err("<Polygon> missing <outerBoundaryIs>"))?;
    let mut rings = vec![parse_ring(outer)?];
    for inner in node.children_of(KmlElement::InnerBoundaryIs) {
        rings.push(parse_ring(inner)?);
    }
    Ok(Geometry::Polygon(rings))
}

/// A track — `<gx:Track>`, or 2.3's `<Track>` — as the line string through its
/// `<coord>`s, each `lon lat alt` separated by spaces rather than KML's usual
/// commas. The rest of a track is per-vertex data the IR has no slot for, and
/// the model carries it (D-K11). A track is three-dimensional by schema, so
/// each position carries its altitude.
fn parse_track(node: &Node) -> Result<Geometry> {
    let positions: Result<Vec<Position>> =
        node.children_of(KmlElement::Coord).map(|c| parse_track_coord(c.text_trimmed())).collect();
    Ok(Geometry::LineString(positions?))
}

fn parse_track_coord(text: &str) -> Result<Position> {
    let bad = || err(&format!("invalid track coordinate \"{text}\""));
    let parts: Vec<f64> =
        text.split_ascii_whitespace().map(|s| s.parse().map_err(|_| bad())).collect::<Result<_>>()?;
    match parts[..] {
        [lon, lat] => Ok(Position::new(lon, lat)),
        [lon, lat, alt] => Ok(Position::with_z(lon, lat, alt)),
        _ => Err(bad()),
    }
}

/// `<gx:MultiTrack>` / 2.3's `<MultiTrack>`: its tracks, in either spelling, as
/// a multi line string. `<interpolate>` says how a renderer joins the tracks
/// and has no geometry of its own.
fn parse_multi_track(node: &Node) -> Result<Geometry> {
    let mut lines = Vec::new();
    for track in node.children_of(KmlElement::Track) {
        match parse_track(track)? {
            Geometry::LineString(line) => lines.push(line),
            _ => unreachable!("parse_track builds a LineString"),
        }
    }
    Ok(Geometry::MultiLineString(lines))
}

/// `<MultiGeometry>` wraps arbitrary children. Same-type children collapse to
/// the matching `Multi*` variant (KML has no `MultiPoint`/etc. element of its
/// own — only the singular forms plus this wrapper); mixed types become a
/// `GeometryCollection`, matching GeoJSON's variant of the same name. A member
/// the IR cannot hold is left to the model; if that leaves nothing, the
/// feature has no geometry rather than the file failing to read.
fn parse_multi_geometry(node: &Node) -> Result<Option<Geometry>> {
    let mut geoms = Vec::new();
    let mut unsupported = false;
    for child in node.children.iter().filter(|c| c.element().is_some_and(KmlElement::is_geometry)) {
        match parse_geometry(child)? {
            Some(g) => geoms.push(g),
            None => unsupported = true,
        }
    }
    if geoms.is_empty() {
        if unsupported {
            return Ok(None);
        }
        return Err(err("<MultiGeometry> has no recognized child geometries"));
    }
    Ok(Some(combine(geoms)))
}

fn combine(geoms: Vec<Geometry>) -> Geometry {
    if geoms.iter().all(|g| matches!(g, Geometry::Point(_))) {
        Geometry::MultiPoint(
            geoms
                .into_iter()
                .map(|g| match g {
                    Geometry::Point(p) => p,
                    _ => unreachable!(),
                })
                .collect(),
        )
    } else if geoms.iter().all(|g| matches!(g, Geometry::LineString(_))) {
        Geometry::MultiLineString(
            geoms
                .into_iter()
                .map(|g| match g {
                    Geometry::LineString(l) => l,
                    _ => unreachable!(),
                })
                .collect(),
        )
    } else if geoms.iter().all(|g| matches!(g, Geometry::Polygon(_))) {
        Geometry::MultiPolygon(
            geoms
                .into_iter()
                .map(|g| match g {
                    Geometry::Polygon(p) => p,
                    _ => unreachable!(),
                })
                .collect(),
        )
    } else {
        Geometry::GeometryCollection(geoms)
    }
}

/// Whitespace-*or*-newline-separated `lon,lat[,alt]` tuples — real-world KML
/// (Google Earth exports especially) is inconsistent about which separator it
/// uses. Altitude, if present, becomes `Position::z` — mirroring GeoJSON's
/// third-array-element handling (M3) — rather than being parsed and dropped.
fn parse_coordinates(text: &str) -> Result<Vec<Position>> {
    text.split_ascii_whitespace().map(parse_tuple).collect()
}

fn parse_tuple(tuple: &str) -> Result<Position> {
    let bad = || err(&format!("invalid coordinate tuple \"{tuple}\""));
    let mut parts = tuple.split(',');
    let lon: f64 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    let lat: f64 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    if let Some(alt) = parts.next() {
        let alt: f64 = alt.parse().map_err(|_| bad())?;
        return Ok(Position::with_z(lon, lat, alt));
    }
    Ok(Position::new(lon, lat))
}

/// A `<GroundOverlay>`'s footprint: the polygon its image covers, from its
/// `<LatLonQuad>` as written or its `<LatLonBox>` turned by its `rotation`
/// (D-K14). `None` when it has neither.
fn footprint(overlay: &Node) -> Result<Option<Geometry>> {
    if let Some(quad) = overlay.child(KmlElement::LatLonQuad) {
        let corners = parse_coordinates(coordinates_of(quad)?.text_trimmed())?;
        if corners.len() != 4 {
            return Err(err(&format!("<LatLonQuad> has {} corners; it takes exactly 4", corners.len())));
        }
        return Ok(Some(closed(corners)));
    }
    let Some(lat_lon_box) = overlay.child(KmlElement::LatLonBox) else {
        return Ok(None);
    };
    Ok(Some(closed(LatLonBox::read(lat_lon_box)?.corners().to_vec())))
}

/// `corners` as a polygon's one ring, closed.
fn closed(mut corners: Vec<Position>) -> Geometry {
    corners.push(corners[0]);
    Geometry::Polygon(vec![corners])
}

/// A `<LatLonBox>`'s four edges and its rotation, in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LatLonBox {
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
    pub rotation: f64,
}

impl LatLonBox {
    /// The box `node` states. A missing edge takes `ogckml23.xsd`'s default —
    /// the whole globe's edge — and a missing rotation is none. 2.2's own
    /// defaults put `north` at 180°, which is not a latitude, so 2.3's are used
    /// for both versions.
    pub fn read(node: &Node) -> Result<LatLonBox> {
        let edge = |element: KmlElement, default: f64| -> Result<f64> {
            match node.child(element) {
                None => Ok(default),
                Some(n) => n
                    .text_trimmed()
                    .parse()
                    .map_err(|_| err(&format!("invalid <{}> \"{}\"", element.name(), n.text_trimmed()))),
            }
        };
        Ok(LatLonBox {
            north: edge(KmlElement::North, 90.0)?,
            south: edge(KmlElement::South, -90.0)?,
            east: edge(KmlElement::East, 180.0)?,
            west: edge(KmlElement::West, -180.0)?,
            rotation: edge(KmlElement::Rotation, 0.0)?,
        })
    }

    /// The four corners, counter-clockwise from the south-west one — the
    /// order `<LatLonQuad>` takes.
    ///
    /// *The rotation is on the ground*, as measured in Google Earth Web
    /// (`plans/kml-standard.org` § MEASURED IN A RENDERER): a box at 60°N
    /// turned 45° lands where turning it on a local metric plane puts it, and
    /// ~80 km from where turning it in degree space does. So each corner's
    /// longitude offset is scaled by the cosine of the centre's latitude,
    /// turned counter-clockwise, and scaled back. An unturned box is its
    /// edges exactly, with no arithmetic to round them.
    ///
    /// A box whose east edge is west of its west edge crosses the
    /// antimeridian, and its east corners are computed 360° on.
    pub fn corners(&self) -> [Position; 4] {
        let east = if self.east < self.west { self.east + 360.0 } else { self.east };
        let box_corners = [(self.west, self.south), (east, self.south), (east, self.north), (self.west, self.north)];
        if self.rotation == 0.0 {
            return box_corners.map(|(x, y)| Position::new(x, y));
        }
        let (cx, cy) = ((self.west + east) / 2.0, (self.north + self.south) / 2.0);
        let k = cy.to_radians().cos();
        let (sin, cos) = self.rotation.to_radians().sin_cos();
        box_corners.map(|(x, y)| {
            let (dx, dy) = ((x - cx) * k, y - cy);
            Position::new(cx + (dx * cos - dy * sin) / k, cy + dx * sin + dy * cos)
        })
    }

    /// The box whose [`Self::corners`] are `ring`'s, if it is one: four
    /// corners, closed or not, that are a rectangle on the ground turned about
    /// its centre. `None` for any other shape.
    pub fn from_ring(ring: &[Position]) -> Option<LatLonBox> {
        let corners = quad_corners(ring)?;
        let [(x0, y0), (x1, y1), (x2, y2), (x3, y3)] = corners.map(|p| (p.x, p.y));
        // Unturned: the edges are the coordinates themselves.
        if y0 == y1 && x1 == x2 && y2 == y3 && x3 == x0 && x0 != x1 && y0 < y2 {
            let east = if x1 > 180.0 { x1 - 360.0 } else { x1 };
            return Some(LatLonBox { north: y2, south: y0, east, west: x0, rotation: 0.0 });
        }
        let (cx, cy) = ((x0 + x1 + x2 + x3) / 4.0, (y0 + y1 + y2 + y3) / 4.0);
        let k = cy.to_radians().cos();
        if k <= 0.0 {
            return None;
        }
        let local = corners.map(|p| ((p.x - cx) * k, p.y - cy));
        let theta = (local[1].1 - local[0].1).atan2(local[1].0 - local[0].0);
        let (sin, cos) = (-theta).sin_cos();
        let unturned = local.map(|(dx, dy)| (dx * cos - dy * sin, dx * sin + dy * cos));
        let (a, b) = (unturned[1].0, unturned[2].1);
        let expected = [(-a, -b), (a, -b), (a, b), (-a, b)];
        let scale = a.abs().max(b.abs()).max(1.0);
        let fits = unturned.iter().zip(expected).all(|(u, e)| (u.0 - e.0).abs() <= 1e-9 * scale && (u.1 - e.1).abs() <= 1e-9 * scale);
        if !(fits && a > 0.0 && b > 0.0) {
            return None;
        }
        let east = cx + a / k;
        // Turning there and back rounds: a box moved 1° east comes back with
        // an edge at 13.000000000000002 and a turn of 44.99999999999994. Ten
        // decimal places of a degree is a hundredth of a millimetre, below
        // anything a coordinate means, and gives the edges back as written.
        let tidy = |v: f64| (v * 1e10).round() / 1e10;
        Some(LatLonBox {
            north: tidy(cy + b),
            south: tidy(cy - b),
            east: tidy(if east > 180.0 { east - 360.0 } else { east }),
            west: tidy(cx - a / k),
            rotation: tidy(theta.to_degrees()),
        })
    }
}

/// A polygon ring's four corners, when it has exactly four: closed (five
/// positions, the last the first) or not. Four positions that close are a
/// triangle.
pub(crate) fn quad_corners(ring: &[Position]) -> Option<[Position; 4]> {
    let closes = |r: &[Position]| r.first().zip(r.last()).is_some_and(|(a, b)| a.x == b.x && a.y == b.y);
    let ring = match ring.len() {
        5 if closes(ring) => &ring[..4],
        4 if !closes(ring) => ring,
        _ => return None,
    };
    Some([ring[0], ring[1], ring[2], ring[3]])
}

/// What a feature's model holds that the IR cannot: a `<Model>` anywhere in
/// its geometry, and a track's per-vertex data — `<when>`, `<angles>`,
/// `<ExtendedData>` arrays, an orienting `<Model>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Unprojected {
    Model,
    TrackData,
}

/// Which of [`Unprojected`] a feature element's geometry holds.
pub(crate) fn unprojected(placemark: &Node) -> Vec<Unprojected> {
    let mut out = Vec::new();
    if let Some(g) = geometry_of(placemark) {
        walk_unprojected(g, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn walk_unprojected(geometry: &Node, out: &mut Vec<Unprojected>) {
    match geometry.element() {
        Some(KmlElement::Model) => out.push(Unprojected::Model),
        Some(KmlElement::Track) => {
            let per_vertex = geometry.children.iter().any(|c| {
                c.element().is_some_and(|e| {
                    matches!(e, KmlElement::When | KmlElement::Angles | KmlElement::ExtendedData | KmlElement::Model)
                })
            });
            if per_vertex {
                out.push(Unprojected::TrackData);
            }
        }
        Some(KmlElement::MultiTrack | KmlElement::MultiGeometry) => {
            for child in &geometry.children {
                walk_unprojected(child, out);
            }
        }
        _ => {}
    }
}

/// What a feature's heights are, for the collection's CRS (D-K12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Vertical {
    /// No geometry carries a Z.
    Flat,
    /// Every geometry carrying a Z is `altitudeMode` `absolute` — the only mode
    /// in which a Z is an EGM96 height (OGC 12-007r2 §6.2, §9.20).
    Absolute,
    /// Some geometry carrying a Z is in another mode: clamped, so its Z is
    /// ignored, or relative to the ground or sea floor, so it is no height in
    /// any datum.
    Other,
}

/// Which [`Vertical`] a feature element's geometry is. Each leaf of a
/// `<MultiGeometry>` counts separately, and a track with no mode of its own
/// takes its `<MultiTrack>`'s. An absent mode is `clampToGround`, the
/// schema's default.
pub(crate) fn vertical(feature: &Node) -> Vertical {
    let Some(geometry) = geometry_of(feature) else {
        return Vertical::Flat;
    };
    let mut out = Vertical::Flat;
    walk_vertical(geometry, None, &mut out);
    out
}

fn walk_vertical<'n>(geometry: &'n Node, inherited: Option<&'n Node>, out: &mut Vertical) {
    let mode = geometry.child(KmlElement::SeaFloorAltitudeMode).or(geometry.child(KmlElement::AltitudeMode)).or(inherited);
    match geometry.element() {
        Some(KmlElement::MultiGeometry | KmlElement::MultiTrack) => {
            for child in geometry.children.iter().filter(|c| c.element().is_some_and(KmlElement::is_geometry)) {
                walk_vertical(child, mode, out);
            }
        }
        _ if has_z(geometry) => {
            let absolute = mode.is_some_and(|m| m.is(KmlElement::AltitudeMode) && m.text_trimmed() == "absolute");
            *out = (*out).max(if absolute { Vertical::Absolute } else { Vertical::Other });
        }
        _ => {}
    }
}

/// Whether a leaf geometry's coordinates carry a third ordinate, read from the
/// text rather than parsed again.
fn has_z(geometry: &Node) -> bool {
    let tuples_have_z = |coordinates: &Node| coordinates.text_trimmed().split_ascii_whitespace().any(|t| t.matches(',').count() >= 2);
    match geometry.element() {
        Some(KmlElement::Point | KmlElement::LineString | KmlElement::LinearRing) => {
            geometry.child(KmlElement::Coordinates).is_some_and(tuples_have_z)
        }
        Some(KmlElement::Polygon) => [KmlElement::OuterBoundaryIs, KmlElement::InnerBoundaryIs].iter().any(|b| {
            geometry
                .children_of(*b)
                .filter_map(|boundary| boundary.child(KmlElement::LinearRing)?.child(KmlElement::Coordinates))
                .any(tuples_have_z)
        }),
        Some(KmlElement::Track) => {
            geometry.children_of(KmlElement::Coord).any(|c| c.text_trimmed().split_ascii_whitespace().count() >= 3)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &Position, (x, y): (f64, f64)) -> bool {
        (a.x - x).abs() < 5e-4 && (a.y - y).abs() < 5e-4
    }

    /// The Google Earth Web measurement in `plans/kml-standard.org` § MEASURED
    /// IN A RENDERER: a 4°×2° box centred on 10°E 60°N turned 45°, whose
    /// corners landed on the ground-plane predictions.
    #[test]
    fn a_turned_box_turns_on_the_ground_as_google_earth_draws_it() {
        let b = LatLonBox { north: 61.0, south: 59.0, east: 12.0, west: 8.0, rotation: 45.0 };
        let [sw, se, ne, nw] = b.corners();
        assert!(close(&ne, (10.000, 61.414)), "{ne:?}");
        assert!(close(&nw, (7.172, 60.000)), "{nw:?}");
        assert!(close(&sw, (10.000, 58.586)), "{sw:?}");
        assert!(close(&se, (12.828, 60.000)), "{se:?}");
    }

    #[test]
    fn an_unturned_box_is_its_edges_exactly() {
        let b = LatLonBox { north: 56.1, south: 55.3, east: 13.7, west: 12.2, rotation: 0.0 };
        let corners = b.corners();
        assert_eq!(corners.map(|p| (p.x, p.y)), [(12.2, 55.3), (13.7, 55.3), (13.7, 56.1), (12.2, 56.1)]);
        assert_eq!(LatLonBox::from_ring(&corners), Some(b));
    }

    #[test]
    fn a_turned_box_is_recovered_from_its_corners() {
        for rotation in [45.0, -30.0, 90.0, 170.0, -179.0, 12.5] {
            let b = LatLonBox { north: 61.0, south: 59.0, east: 12.0, west: 8.0, rotation };
            let back = LatLonBox::from_ring(&b.corners()).unwrap();
            for (got, want) in [(back.north, b.north), (back.south, b.south), (back.east, b.east), (back.west, b.west), (back.rotation, rotation)] {
                assert!((got - want).abs() < 1e-9, "{rotation}: {back:?}");
            }
        }
    }

    #[test]
    fn a_quadrilateral_that_is_no_turned_box_is_not_one() {
        let ring = [Position::new(0.0, 0.0), Position::new(2.0, 0.0), Position::new(1.5, 1.0), Position::new(0.0, 1.0)];
        assert_eq!(LatLonBox::from_ring(&ring), None);
        assert_eq!(quad_corners(&ring[..3]), None);
    }

    #[test]
    fn a_box_across_the_antimeridian_keeps_its_edges() {
        let b = LatLonBox { north: 10.0, south: 0.0, east: -170.0, west: 170.0, rotation: 0.0 };
        assert_eq!(b.corners()[1].x, 190.0);
        assert_eq!(LatLonBox::from_ring(&b.corners()), Some(b));
    }

    #[test]
    fn kml_time_is_the_four_schema_types_and_nothing_else() {
        for ok in ["2026", "2026-01", "2026-03-15", "2026-10-22T14:30:00Z", "2026-10-22T09:30:00-05:00", "2026-10-22T14:30:00.25", "-0044-03-15", "12026"] {
            assert!(is_kml_time(ok), "{ok}");
        }
        for bad in ["", "26", "2026-1", "2026/01/01", "2026-01-01T14:30", "2026-01-01 14:30:00", "2026Z0", "yesterday", "2026-01-01T14:30:00+5"] {
            assert!(!is_kml_time(bad), "{bad}");
        }
        assert!(is_xsd_date_time("2026-10-22T14:30:00Z"));
        assert!(!is_xsd_date_time("2026-10-22"));
    }

    #[test]
    fn a_typed_field_keeps_a_value_it_cannot_type_as_text() {
        assert_eq!(FieldType::Int.value("007"), JsonValue::Int(7));
        assert_eq!(FieldType::Int.value("3000000000"), JsonValue::String("3000000000".into()));
        assert_eq!(FieldType::UInt.value("3000000000"), JsonValue::Int(3_000_000_000));
        assert_eq!(FieldType::UShort.value("-1"), JsonValue::String("-1".into()));
        assert_eq!(FieldType::Double.value("1.50"), JsonValue::Float(1.5));
        assert_eq!(FieldType::Double.value("NaN"), JsonValue::String("NaN".into()));
        assert_eq!(FieldType::Double.value("inf"), JsonValue::String("inf".into()));
        assert_eq!(FieldType::Bool.value("1"), JsonValue::Bool(true));
        assert_eq!(FieldType::Bool.value("yes"), JsonValue::String("yes".into()));
        assert_eq!(FieldType::Int.value(" "), JsonValue::Null);
        assert_eq!(FieldType::String.value(""), JsonValue::String("".into()));
    }

    #[test]
    fn every_declared_type_has_a_field_type_whose_declaration_admits_it_or_is_text() {
        for ty in DeclType::ALL {
            let landed = FieldType::for_decl(ty).decl();
            assert!(landed.admits(ty) || landed == DeclType::String, "{ty:?} -> {landed:?}");
        }
    }
}
