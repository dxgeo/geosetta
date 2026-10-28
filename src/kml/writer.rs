//! Feature IR -> `.kml` (plain XML) bytes. The inverse of `reader.rs`.
//!
//! KML coordinates are always WGS 84 longitude/latitude; a non-WGS-84 source
//! is a CRS-loss case already surfaced by `Crs::downgrade_warning`'s merged
//! `GeoJson | Kml` arm, so this writer doesn't need to look at the source CRS
//! at all — it just emits whatever coordinates the geometry carries.

use crate::feature::{Feature, FeatureCollection};
use crate::format::{Format, KmlVersion};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::kml::document::Skeleton;
use crate::kml::{lower, raise};
use crate::kml::model::{self, KmlElement, GX_NS, GX_PREFIX, KML_NS};
use crate::kml::place::{place, Time};
use crate::kml::project::{self, can_carry, FieldType};
use crate::kml::reconcile;
use crate::native::{KmlNative, NativePayload};
use crate::xml::{self, XmlElement};

/// Encode the Feature IR as a `.kml` document: a `<Document>` containing one
/// `<Placemark>` per feature, in `version` — or, when that is `None`, in the
/// source's own version if a KML read recorded one, else KML 2.2
/// (`plans/kml-standard.org` D-K2).
///
/// *A KML read written back as its own version* gives back the document it
/// was read from, with the IR's edits applied: every folder, style, overlay
/// and comment, each feature in its place (`crate::kml::reconcile`). *A read
/// written as the other version* is the same document lowered to 2.2
/// (`crate::kml::lower`) or raised to 2.3 (`crate::kml::raise`).
/// Any other write is built from the IR alone, in the house layout:
/// placemarks with their name,
/// description, time, `ExtendedData` and geometry with its altitude members,
/// and a `<Schema>` for the columns the collection declares a type for
/// (D-K9).
///
/// `compact` writes it on one line, with no whitespace between elements — a
/// carried document's own layout included (`plans/kml-layout.org` D-L8).
pub(crate) fn write(fc: &FeatureCollection, version: Option<KmlVersion>, compact: bool) -> Vec<u8> {
    if carries_documents(fc, Format::Kml) {
        return write_document(fc, 0, version, compact);
    }
    let written = version.or(source_document(fc).map(|s| s.version)).unwrap_or(KmlVersion::V2_2);
    let typed = Typed::of(fc);
    let mut children: Vec<XmlElement> = typed.schema().into_iter().collect();
    children.extend(fc.features.iter().map(|f| feature_to_placemark(f, written, &typed)));
    let document = XmlElement::with_children("Document", children);
    let mut root = root_element(source_document(fc), version, vec![document]);
    if compact {
        return xml::write(&root).into_bytes();
    }
    // The house style (`plans/kml-layout.org` D-L2): one element per line,
    // indented two spaces, and a final newline.
    xml::lay_out(&mut root, "\n", "", model::HOUSE_INDENT);
    let mut out = xml::write(&root);
    out.push('\n');
    out.into_bytes()
}

/// The columns a document built from the IR types with a `<Schema>`: each
/// one the collection declares a type for that still describes its values
/// (`FieldDecl::type_for`), as the KML field type nearest it. A key a
/// Placemark holds as an element of its own is not a column of the schema.
#[derive(Debug, Default)]
pub(crate) struct Typed {
    id: String,
    name: Option<String>,
    fields: Vec<(String, FieldType)>,
}

impl Typed {
    pub fn of(fc: &FeatureCollection) -> Typed {
        if fc.schema.is_empty() {
            return Typed::default();
        }
        let own: Vec<&str> = project::element_keys(KmlElement::Placemark).collect();
        let fields = crate::schema::columns_for(fc)
            .into_iter()
            .filter(|c| !own.contains(&c.name.as_str()))
            .filter_map(|c| Some((c.name.clone(), FieldType::for_decl(c.declared_type()?))))
            .collect();
        // GDAL names a schema after its layer; an `id` must be an XML name.
        let id = fc.name.clone().filter(|n| is_ncname(n)).unwrap_or_else(|| "schema".into());
        Typed { id, name: fc.name.clone(), fields }
    }

    /// The `<Schema>` element, when there is a column to type.
    fn schema(&self) -> Option<XmlElement> {
        if self.fields.is_empty() {
            return None;
        }
        let fields = self
            .fields
            .iter()
            .map(|(name, ty)| XmlElement::with_children("SimpleField", Vec::new()).with_attr("type", ty.kml_name()).with_attr("name", name))
            .collect();
        let schema = XmlElement::with_children("Schema", fields);
        let schema = match &self.name {
            Some(name) => schema.with_attr("name", name),
            None => schema,
        };
        Some(schema.with_attr("id", &self.id))
    }

    fn types(&self, key: &str) -> bool {
        self.fields.iter().any(|(n, _)| n == key)
    }
}

/// Whether `s` is an XML `NCName`, in its ASCII subset.
fn is_ncname(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Whether a write of `fc` as `target` gives the source's documents back: it
/// was read from KML or KMZ, and the target holds as many documents as it
/// had. A `.kmz` holds any number; a `.kml` holds one, so a layered `.kmz`
/// written as `.kml` is built from the IR. Either output version gives them
/// back, since a version change lowers or raises each (K5).
pub(crate) fn carries_documents(fc: &FeatureCollection, target: Format) -> bool {
    source_document(fc).is_some_and(|s| {
        s.document.is_some()
            && match target {
                Format::Kmz => true,
                Format::Kml => s.layers.is_empty(),
                _ => false,
            }
    })
}

/// The source's document `index` — 0 for the first, `n` for layer `n - 1` —
/// written back with its own features, in `version` when one is asked for.
/// A feature added since the read, or holding an element of no document this
/// collection keeps, joins the first document, built fresh. `compact` as
/// [`write`] takes it.
pub(crate) fn write_document(fc: &FeatureCollection, index: usize, version: Option<KmlVersion>, compact: bool) -> Vec<u8> {
    let source = source_document(fc).expect("a carried document has a source");
    let (skeleton, own): (&Skeleton, KmlVersion) = match index {
        0 => (source.document.as_deref().expect("the first document is kept"), source.version),
        n => (&source.layers[n - 1].skeleton, source.layers[n - 1].version),
    };
    let documents = 1 + source.layers.len();
    let features = fc.features.iter().filter_map(|f| match reconcile::stored(f) {
        Some((m, d)) if d < documents => (d == index).then_some((f, Some(m))),
        _ => (index == 0).then_some((f, None)),
    });
    let mut doc = reconcile::assemble(features, skeleton, own, compact);
    match (own, version) {
        (KmlVersion::V2_3, Some(KmlVersion::V2_2)) => {
            lower::lower(&mut doc);
        }
        (KmlVersion::V2_2, Some(KmlVersion::V2_3)) => {
            raise::raise(&mut doc);
        }
        _ => {}
    }
    if compact {
        model::flatten(&mut doc);
    }
    model::write(&doc).into_bytes()
}

/// The provenance a KML or KMZ read recorded. Both writers read either tag:
/// the two formats hold one document, so a `.kml` → `.kmz` run keeps its
/// version as a `.kml` → `.kml` run does.
pub(crate) fn source_document(fc: &FeatureCollection) -> Option<&KmlNative> {
    let native = fc.native.as_ref()?;
    match native.payload_for(Format::Kml).or_else(|| native.payload_for(Format::Kmz))? {
        NativePayload::Kml(document) => Some(document),
        _ => None,
    }
}

/// `<kml>`, with the namespace and `version` stamp the output version calls
/// for. When that version is the source's own, the source's spellings are
/// written back as they were (D-K5) — no namespace, Google's, a stamp of
/// `2.3.0` or none. Otherwise the namespace is OGC's and the stamp is the
/// version's own: none for 2.2, whose schema has no `version` attribute at
/// all, and `2.3` for 2.3.
fn root_element(source: Option<&KmlNative>, version: Option<KmlVersion>, children: Vec<XmlElement>) -> XmlElement {
    let root = XmlElement::with_children("kml", children);
    let version = version.or(source.map(|s| s.version)).unwrap_or(KmlVersion::V2_2);
    match source.filter(|s| s.version == version) {
        Some(source) => {
            let root = match &source.namespace {
                Some(ns) => root.with_ns_decl(None, ns),
                None => root,
            };
            match &source.version_attr {
                Some(stamp) => root.with_attr("version", stamp),
                None => root,
            }
        }
        None => {
            let root = root.with_ns_decl(None, KML_NS);
            match version {
                KmlVersion::V2_2 => root,
                KmlVersion::V2_3 => root.with_attr("version", "2.3"),
            }
        }
    }
}

/// A feature as a `<Placemark>` in `version`: each property its own element
/// where KML has one that admits it (`crate::kml::place`), the columns
/// `typed` declares in `<SchemaData>`, and every other property in
/// `<ExtendedData><Data name="k"><value>v</value></Data></ExtendedData>`.
pub(crate) fn feature_to_placemark(f: &Feature, version: KmlVersion, typed: &Typed) -> XmlElement {
    let mut geometry = f.geometry.as_ref().map(geometry_to_element);
    let placement = place(f, KmlElement::Placemark, |member| geometry.as_ref().is_some_and(|g| carries(g, member)));
    let mut children = Vec::new();
    if let Some(name) = placement.name {
        children.push(XmlElement::leaf("name", name));
    }
    if let Some(description) = placement.description {
        children.push(XmlElement::leaf("description", description));
    }
    if let Some(time) = time_element(&placement.time) {
        children.push(time);
    }
    let mut data = Vec::new();
    let mut simple = Vec::new();
    for (key, value) in placement.extended {
        let text = property_text(value);
        if typed.types(key) && !simple.iter().any(|s: &XmlElement| s.attr("name") == Some(key)) {
            simple.push(XmlElement::leaf("SimpleData", text).with_attr("name", key));
        } else {
            data.push(XmlElement::with_children("Data", vec![XmlElement::leaf("value", text)]).with_attr("name", key));
        }
    }
    if !simple.is_empty() {
        data.push(XmlElement::with_children("SchemaData", simple).with_attr("schemaUrl", format!("#{}", typed.id)));
    }
    if !data.is_empty() {
        children.push(XmlElement::with_children("ExtendedData", data));
    }
    if let Some(g) = &mut geometry {
        add_altitude(g, &placement.altitude, version);
        children.push(geometry.unwrap());
    }
    XmlElement::with_children("Placemark", children)
}

/// The time primitive for `time`, if it has one.
pub(crate) fn time_element(time: &Time) -> Option<XmlElement> {
    match time {
        Time::None => None,
        Time::Stamp(when) => Some(XmlElement::with_children("TimeStamp", vec![XmlElement::leaf("when", when.clone())])),
        Time::Span(begin, end) => {
            let members = [("begin", begin), ("end", end)];
            let children = members.into_iter().filter_map(|(n, v)| Some(XmlElement::leaf(n, v.clone()?))).collect();
            Some(XmlElement::with_children("TimeSpan", children))
        }
    }
}

/// An altitude member as an element: `<seaFloorAltitudeMode>` is 2.3's
/// spelling, and 2.2's is `<gx:altitudeMode>`, declared where it is written.
pub(crate) fn altitude_element(member: KmlElement, text: &str, version: KmlVersion) -> XmlElement {
    if member == KmlElement::SeaFloorAltitudeMode && version == KmlVersion::V2_2 {
        let mut el = XmlElement::leaf(member.gx_name().expect("an adopted element"), text).with_ns_decl(Some(GX_PREFIX), GX_NS);
        el.prefix = Some(GX_PREFIX.into());
        return el;
    }
    XmlElement::leaf(member.name(), text)
}

/// The kind of a geometry element this writer built.
fn kind(el: &XmlElement) -> Option<KmlElement> {
    KmlElement::from_name(&el.name)
}

/// Whether `geometry` or any member of it can hold `member`.
fn carries(geometry: &XmlElement, member: KmlElement) -> bool {
    kind(geometry).is_some_and(|k| can_carry(k, member))
        || (kind(geometry) == Some(KmlElement::MultiGeometry) && geometry.children.iter().any(|c| carries(c, member)))
}

/// Put `members` on `geometry` and on each member of it that can hold them,
/// ahead of its coordinates, in the schema's order: `<extrude>`,
/// `<tessellate>`, then the altitude mode.
fn add_altitude(geometry: &mut XmlElement, members: &[(KmlElement, String)], version: KmlVersion) {
    if kind(geometry) == Some(KmlElement::MultiGeometry) {
        for child in &mut geometry.children {
            add_altitude(child, members, version);
        }
        return;
    }
    let Some(k) = kind(geometry) else { return };
    let mut leading: Vec<(u8, XmlElement)> = members
        .iter()
        .filter(|(m, _)| can_carry(k, *m))
        .map(|(m, v)| (geometry_rank(*m), altitude_element(*m, v, version)))
        .collect();
    leading.sort_by_key(|(r, _)| *r);
    let rest = std::mem::take(&mut geometry.children);
    geometry.children = leading.into_iter().map(|(_, e)| e).chain(rest).collect();
}

/// A geometry child's place among a geometry's members: `<extrude>`,
/// `<tessellate>`, the altitude mode, then the rest — the order every 2.2
/// geometry type's sequence shares.
pub(crate) fn geometry_rank(member: KmlElement) -> u8 {
    match member {
        KmlElement::Extrude => 0,
        KmlElement::Tessellate => 1,
        KmlElement::AltitudeMode | KmlElement::SeaFloorAltitudeMode => 2,
        _ => 3,
    }
}

/// A property value as `<Data>`'s plain text. Nested arrays/objects fall back
/// to compact JSON, the same convention `schema::infer_columns` uses for
/// heterogeneous/nested properties elsewhere.
pub(crate) fn property_text(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => s.clone(),
        JsonValue::Null => String::new(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Int(n) => n.to_string(),
        // Via the JSON serializer so a whole float keeps its `.0` here too.
        JsonValue::Float(_) => value.to_json_string(),
        // KML `<Data>` is text; bytes render as the hex the JSON serializer
        // produces, minus its quotes.
        JsonValue::Binary(_) => value.to_json_string().trim_matches('"').to_string(),
        JsonValue::Array(_) | JsonValue::Object(_) => value.to_json_string(),
    }
}

pub(crate) fn geometry_to_element(g: &Geometry) -> XmlElement {
    match g {
        Geometry::Point(p) => point_element(p),
        Geometry::LineString(pts) => line_string_element(pts),
        Geometry::Polygon(rings) => polygon_element(rings),
        Geometry::MultiPoint(pts) => {
            XmlElement::with_children("MultiGeometry", pts.iter().map(point_element).collect())
        }
        Geometry::MultiLineString(lines) => XmlElement::with_children(
            "MultiGeometry",
            lines.iter().map(|l| line_string_element(l)).collect(),
        ),
        Geometry::MultiPolygon(polys) => {
            XmlElement::with_children("MultiGeometry", polys.iter().map(|p| polygon_element(p)).collect())
        }
        Geometry::GeometryCollection(geoms) => {
            XmlElement::with_children("MultiGeometry", geoms.iter().map(geometry_to_element).collect())
        }
    }
}

fn point_element(p: &Position) -> XmlElement {
    XmlElement::with_children("Point", vec![coordinates_element(std::slice::from_ref(p))])
}

fn line_string_element(pts: &[Position]) -> XmlElement {
    XmlElement::with_children("LineString", vec![coordinates_element(pts)])
}

fn polygon_element(rings: &[Vec<Position>]) -> XmlElement {
    let mut children = Vec::new();
    if let Some(outer) = rings.first() {
        children.push(boundary_element("outerBoundaryIs", outer));
    }
    for inner in rings.iter().skip(1) {
        children.push(boundary_element("innerBoundaryIs", inner));
    }
    XmlElement::with_children("Polygon", children)
}

fn boundary_element(tag: &str, ring: &[Position]) -> XmlElement {
    XmlElement::with_children(
        tag,
        vec![XmlElement::with_children("LinearRing", vec![coordinates_element(ring)])],
    )
}

fn coordinates_element(points: &[Position]) -> XmlElement {
    use std::fmt::Write;
    let mut text = String::new();
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        match p.z {
            Some(z) => {
                let _ = write!(text, "{},{},{}", p.x, p.y, z);
            }
            None => {
                let _ = write!(text, "{},{}", p.x, p.y);
            }
        }
    }
    XmlElement::leaf("coordinates", text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kml::reader;
    use std::rc::Rc;

    /// `bytes` with the house layout's whitespace between tags taken out, so
    /// a test of what is written reads it on one line;
    /// [`a_document_built_from_the_ir_is_in_the_house_layout`] pins the layout.
    fn squeezed(bytes: &[u8]) -> String {
        let text = std::str::from_utf8(bytes).unwrap();
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find('>') {
            out.push_str(&rest[..=at]);
            rest = &rest[at + 1..];
            let space = rest.len() - rest.trim_start().len();
            if rest[space..].starts_with('<') {
                rest = &rest[space..];
            }
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn a_document_built_from_the_ir_is_in_the_house_layout() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Point(Position::new(1.0, 2.0))),
            vec![("name", JsonValue::String("a".into())), ("description", JsonValue::String("<b>x</b>".into()))],
        )]);
        assert_eq!(
            String::from_utf8(write(&fc, None, false)).unwrap(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"http://www.opengis.net/kml/2.2\">\n  <Document>\n    \
             <Placemark>\n      <name>a</name>\n      <description>&lt;b&gt;x&lt;/b&gt;</description>\n      <Point>\n        \
             <coordinates>1,2</coordinates>\n      </Point>\n    </Placemark>\n  </Document>\n</kml>\n"
        );
    }

    fn feature(geometry: Option<Geometry>, properties: Vec<(&str, JsonValue)>) -> Feature {
        Feature {
            geometry,
            properties: properties.into_iter().map(|(k, v)| (Rc::from(k), v)).collect(),
            native: None,
        }
    }

    #[test]
    fn writes_name_description_and_extended_data() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Point(Position::new(1.0, 2.0))),
            vec![
                ("name", JsonValue::String("A Point".into())),
                ("description", JsonValue::String("desc".into())),
                ("height_m", JsonValue::Int(381)),
            ],
        )]);
        let text = squeezed(&write(&fc, None, false));
        assert!(text.contains("<name>A Point</name>"));
        assert!(text.contains("<description>desc</description>"));
        assert!(text.contains("<Data name=\"height_m\"><value>381</value></Data>"));
        assert!(text.contains("<Point><coordinates>1,2</coordinates></Point>"));
    }

    #[test]
    fn writes_z_bearing_point_with_third_coordinate() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Point(Position::with_z(-73.9857, 40.7484, 381.0))),
            vec![],
        )]);
        let text = squeezed(&write(&fc, None, false));
        assert!(text.contains("<Point><coordinates>-73.9857,40.7484,381</coordinates></Point>"));
    }

    #[test]
    fn writes_polygon_with_hole() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 4.0), Position::new(0.0, 0.0)],
                vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)],
            ])),
            vec![],
        )]);
        let text = squeezed(&write(&fc, None, false));
        assert!(text.contains("<outerBoundaryIs><LinearRing><coordinates>0,0 4,0 4,4 0,4 0,0</coordinates></LinearRing></outerBoundaryIs>"));
        assert!(text.contains("<innerBoundaryIs><LinearRing><coordinates>1,1 2,1 2,2 1,2 1,1</coordinates></LinearRing></innerBoundaryIs>"));
    }

    #[test]
    fn writes_multi_point_as_multi_geometry() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)])),
            vec![],
        )]);
        let text = squeezed(&write(&fc, None, false));
        assert!(text.contains("<MultiGeometry><Point><coordinates>0,0</coordinates></Point><Point><coordinates>1,1</coordinates></Point></MultiGeometry>"));
    }

    #[test]
    fn geojson_to_kml_to_geojson_round_trips_geometry_and_properties() {
        use crate::geojson;
        use crate::json;
        let src = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[-73.9857, 40.7484, 381]},
             "properties":{"name":"Empire State","height_m":381,"landmark":true}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[-73.99, 40.75],[-73.98, 40.76]]},
             "properties":{"name":"A Path"}}
        ]}"#;
        let original = geojson::from_json(&json::parse(src).unwrap()).unwrap();
        let kml_bytes = write(&original, None, false);
        let back = reader::read(&kml_bytes, &mut Vec::new()).unwrap();
        assert_eq!(back.features.len(), original.features.len());
        for (a, b) in original.features.iter().zip(back.features.iter()) {
            assert_eq!(a.geometry, b.geometry);
            let name = |f: &Feature| f.properties.iter().find(|(k, _)| &**k == "name").unwrap().1.clone();
            assert_eq!(name(a), name(b));
        }
    }

    #[test]
    fn escapes_special_characters_in_name() {
        let fc = FeatureCollection::new(vec![feature(
            None,
            vec![("name", JsonValue::String("Fish & Chips <Shop>".into()))],
        )]);
        let text = squeezed(&write(&fc, None, false));
        assert!(text.contains("<name>Fish &amp; Chips &lt;Shop&gt;</name>"));
    }

    fn root_tag(bytes: &[u8]) -> String {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let start = text.find("<kml").unwrap();
        text[start..start + text[start..].find('>').unwrap() + 1].to_string()
    }

    fn reread_and_write(kml: &str, version: Option<KmlVersion>) -> String {
        let fc = crate::kml::reader::read(kml.as_bytes(), &mut Vec::new()).unwrap();
        root_tag(&write(&fc, version, false))
    }

    const OGC_ROOT: &str = r#"<kml xmlns="http://www.opengis.net/kml/2.2">"#;

    #[test]
    fn a_collection_from_another_format_is_written_as_unstamped_ogc_2_2() {
        assert_eq!(root_tag(&write(&FeatureCollection::new(vec![]), None, false)), OGC_ROOT);
    }

    #[test]
    fn asking_for_2_3_stamps_it() {
        assert_eq!(
            root_tag(&write(&FeatureCollection::new(vec![]), Some(KmlVersion::V2_3), false)),
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3">"#
        );
    }

    #[test]
    fn a_same_version_write_gives_back_the_root_it_was_given() {
        for root in [
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3.0">"#,
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.2">"#,
            r#"<kml xmlns="http://earth.google.com/kml/2.2">"#,
            "<kml>",
        ] {
            let kml = format!("{root}<Document/></kml>");
            assert_eq!(reread_and_write(&kml, None), root, "default: the source's version");
        }
        let kml = r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3.0"><Document/></kml>"#;
        assert_eq!(reread_and_write(kml, Some(KmlVersion::V2_3)), r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3.0">"#);
    }

    #[test]
    fn a_version_change_writes_the_new_versions_own_root() {
        let from_2_3 = r#"<kml version="2.3"><Document/></kml>"#;
        assert_eq!(reread_and_write(from_2_3, Some(KmlVersion::V2_2)), OGC_ROOT);
        let from_google = r#"<kml xmlns="http://earth.google.com/kml/2.2"><Document/></kml>"#;
        assert_eq!(
            reread_and_write(from_google, Some(KmlVersion::V2_3)),
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3">"#
        );
    }

    #[test]
    fn a_kmz_read_keeps_its_version_into_a_kml_write() {
        let doc = br#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3"><Document/></kml>"#;
        let kmz = crate::zip::write(&[("doc.kml", &doc[..])]);
        let fc = crate::kml::read_kmz(&kmz, &mut Vec::new()).unwrap();
        assert_eq!(root_tag(&write(&fc, None, false)), r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3">"#);
    }

    fn text(s: &str) -> JsonValue {
        JsonValue::String(s.into())
    }

    fn written(fc: &FeatureCollection, version: Option<KmlVersion>) -> String {
        squeezed(&write(fc, version, false))
    }

    #[test]
    fn time_and_altitude_properties_are_written_as_their_elements() {
        let fc = FeatureCollection::new(vec![
            feature(
                Some(Geometry::Point(Position::with_z(1.0, 2.0, 3.0))),
                vec![("timestamp", text("2026-10-22T14:30:00Z")), ("altitudeMode", text("absolute")), ("extrude", JsonValue::Bool(true))],
            ),
            feature(
                Some(Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)])),
                vec![("begin", text("2026-01")), ("tessellate", text("1"))],
            ),
        ]);
        let out = written(&fc, None);
        assert!(
            out.contains("<Placemark><TimeStamp><when>2026-10-22T14:30:00Z</when></TimeStamp><Point><extrude>true</extrude><altitudeMode>absolute</altitudeMode><coordinates>1,2,3</coordinates></Point></Placemark>"),
            "{out}"
        );
        assert!(out.contains("<TimeSpan><begin>2026-01</begin></TimeSpan><LineString><tessellate>1</tessellate><coordinates>"), "{out}");
        let back = reader::read(out.as_bytes(), &mut Vec::new()).unwrap();
        assert_eq!(back.features[0].properties[2], (Rc::from("extrude"), text("true")));
    }

    #[test]
    fn a_property_no_element_admits_goes_to_extended_data_under_its_key() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Point(Position::new(1.0, 2.0))),
            vec![
                ("timestamp", text("soon")),
                ("altitudeMode", text("floating")),
                ("tessellate", text("1")),
                ("begin", text("2026")),
                ("icon", text("i.png")),
            ],
        )]);
        let out = written(&fc, None);
        for key in ["timestamp", "altitudeMode", "tessellate", "icon"] {
            assert!(out.contains(&format!(r#"<Data name="{key}">"#)), "{key}: {out}");
        }
        // The stamp was refused, so the span holds the begin.
        assert!(out.contains("<TimeSpan><begin>2026</begin></TimeSpan>"), "{out}");
        let back = reader::read(out.as_bytes(), &mut Vec::new()).unwrap();
        let mut keys: Vec<_> = back.features[0].properties.iter().map(|(k, _)| k.to_string()).collect();
        keys.sort();
        assert_eq!(keys, ["altitudeMode", "begin", "icon", "tessellate", "timestamp"]);
    }

    #[test]
    fn a_sea_floor_mode_is_spelled_for_the_version_written() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Point(Position::with_z(1.0, 2.0, 3.0))),
            vec![("seaFloorAltitudeMode", text("relativeToSeaFloor"))],
        )]);
        assert!(
            written(&fc, None).contains(r#"<gx:altitudeMode xmlns:gx="http://www.google.com/kml/ext/2.2">relativeToSeaFloor</gx:altitudeMode>"#)
        );
        assert!(written(&fc, Some(KmlVersion::V2_3)).contains("<seaFloorAltitudeMode>relativeToSeaFloor</seaFloorAltitudeMode>"));
        for version in [None, Some(KmlVersion::V2_3)] {
            let back = reader::read(&write(&fc, version, false), &mut Vec::new()).unwrap();
            assert_eq!(back.features[0].properties, fc.features[0].properties, "{version:?}");
        }
    }

    #[test]
    fn declared_columns_are_written_as_a_schema() {
        use crate::schema::{DeclType, FieldDecl};
        let mut fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Point(Position::new(1.0, 2.0))),
            vec![("name", text("a")), ("n", JsonValue::Int(7)), ("free", text("x")), ("big", JsonValue::Int(1 << 40))],
        )]);
        fc.name = Some("layer one".into());
        fc.schema = vec![
            FieldDecl { ty: Some(DeclType::Int16), ..FieldDecl::new("n") },
            FieldDecl { ty: Some(DeclType::Int64), ..FieldDecl::new("big") },
            FieldDecl { ty: Some(DeclType::String), ..FieldDecl::new("name") },
        ];
        let out = written(&fc, None);
        assert!(
            out.contains(r#"<Document><Schema name="layer one" id="schema"><SimpleField type="short" name="n"/><SimpleField type="string" name="big"/></Schema>"#),
            "{out}"
        );
        assert!(
            out.contains(r##"<Data name="free"><value>x</value></Data><SchemaData schemaUrl="#schema"><SimpleData name="n">7</SimpleData><SimpleData name="big">1099511627776</SimpleData></SchemaData>"##),
            "{out}"
        );
        let back = reader::read(out.as_bytes(), &mut Vec::new()).unwrap();
        let decls: Vec<_> = back.schema.iter().map(|d| (d.name.as_str(), d.ty.unwrap())).collect();
        assert_eq!(decls, [("n", DeclType::Int16), ("big", DeclType::String)]);
        assert_eq!(back.features[0].properties[1], (Rc::from("free"), text("x")));
        assert_eq!(back.features[0].properties[2], (Rc::from("n"), JsonValue::Int(7)));
    }
}
