//! `.kml` (plain XML) -> Feature IR.
//!
//! KML coordinates are always WGS 84 longitude/latitude — the spec has no
//! other option, no per-file CRS element at all. What a document *does* say
//! is what its heights are: a Z in `altitudeMode` `absolute` is an EGM96
//! height. So a collection is [`Crs::wgs84_egm96`] (EPSG:9707) when every Z
//! it has is one, and [`Crs::Wgs84`] otherwise (`plans/kml-standard.org`
//! D-K12). None of the CRS resolution machinery the richer spokes need
//! applies here. See `plans/kml.org`.
//!
//! Elements are matched by namespace as well as local name
//! (`plans/kml-standard.org` D-K6): a KML element is one in a KML namespace,
//! or in none when the document declares none (D-K13), and an extension's
//! element that shares a KML local name is not read as KML.

use std::rc::Rc;

use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::format::{Format, KmlVersion};
use crate::kml::document::{self, Skeleton};
use crate::kml::model::{self, Kind, KmlDocument, KmlElement, Node, Spelling, GOOGLE_KML_2_2_NS};
use crate::kml::project::{self, project, Context, Vertical};
use crate::native::{KmlFeatureNative, KmlNative, Native, NativePayload};
use crate::schema::{DeclType, FieldDecl};
use crate::xml;

/// Google's 2.0 and 2.1, which are refused (D-K8): `(namespace, version)`.
const GOOGLE_PRE_OGC: [(&str, &str); 2] =
    [("http://earth.google.com/kml/2.1", "2.1"), ("http://earth.google.com/kml/2.0", "2.0")];

fn err(message: &str) -> Error {
    Error::Convert(format!("kml: {message}"))
}

/// Parse a `.kml` document's bytes into the Feature IR.
///
/// Reports nothing at read, by design: what the IR does not project — a
/// `<Model>`, a track's per-vertex data, styles, folders — is carried in the
/// model (`plans/kml-standard.org` D-K4, D-K11), and what a write to another
/// format then loses is that write's warning (`crate::kml::drop_warnings`).
/// The sink is the read-side channel every reader takes.
pub(crate) fn read(input: &[u8], _warnings: &mut Vec<String>) -> Result<FeatureCollection> {
    let text = xml::decode(input)?;
    let document = read_document(&text, Format::Kml)?;
    let facts = document.facts;
    Ok(finish(document.features, document.native, Some(document.skeleton), facts, Format::Kml))
}

/// One parsed KML document: what it says about itself, its skeleton, and its
/// projected features, each carrying its own element as a native tagged
/// `format`.
pub(crate) struct ReadDocument {
    pub native: KmlNative,
    pub skeleton: Skeleton,
    pub features: Vec<Feature>,
    pub facts: Facts,
}

/// What a document says about the collection as a whole, beyond its
/// features: the column types its `<Schema>`s declare (D-K9) and what its
/// heights are (D-K12). A `.kmz`'s documents merge theirs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Facts {
    pub declarations: Vec<(String, DeclType)>,
    pub vertical: Vertical,
}

impl Facts {
    /// Both documents' facts: a column two documents type differently is
    /// typed by neither, and one document's heights that are not EGM96 make
    /// the collection's not.
    pub fn merge(mut self, other: Facts) -> Facts {
        let mut conflicted = Vec::new();
        for (name, ty) in other.declarations {
            match self.declarations.iter().find(|(n, _)| *n == name) {
                Some((_, t)) if *t != ty => conflicted.push(name),
                Some(_) => {}
                None => self.declarations.push((name, ty)),
            }
        }
        self.declarations.retain(|(n, _)| !conflicted.contains(n));
        Facts { declarations: self.declarations, vertical: self.vertical.max(other.vertical) }
    }
}

/// Parse, detect, raise, split and project one document's text — the piece
/// `.kmz` repeats per entry.
pub(crate) fn read_document(text: &str, format: Format) -> Result<ReadDocument> {
    let parsed = model::parse(text)?;
    let native = detect(&parsed)?;
    let (skeleton, models) = document::split(parsed);
    // A `<Schema>` is document content, so it stays in the skeleton, and the
    // writer reads it from there again to project what it compares against.
    let ctx = Context::of(&skeleton.doc.root);
    let mut vertical = Vertical::Flat;
    let features = models
        .into_iter()
        .map(|m| {
            let projection = project(&m.node, &ctx)?;
            vertical = vertical.max(project::vertical(&m.node));
            let native = Native::new(format, NativePayload::KmlFeature(KmlFeatureNative { feature: Rc::new(m) }));
            Ok(Feature { geometry: projection.geometry, properties: projection.properties, native: Some(native) })
        })
        .collect::<Result<_>>()?;
    let facts = Facts { declarations: ctx.declarations(), vertical };
    Ok(ReadDocument { native, skeleton, features, facts })
}

/// Wrap collected features as a collection with the document's provenance —
/// with its skeleton, when there is one document to give back — tagged with
/// the format that read it, in the CRS its heights call for and with the
/// column declarations it makes.
pub(crate) fn finish(
    features: Vec<Feature>,
    mut document: KmlNative,
    skeleton: Option<Skeleton>,
    facts: Facts,
    format: Format,
) -> FeatureCollection {
    document.document = skeleton.map(Rc::new);
    let mut fc = FeatureCollection::new(features);
    fc.crs = Some(match facts.vertical {
        Vertical::Absolute => Crs::wgs84_egm96(),
        Vertical::Flat | Vertical::Other => Crs::Wgs84,
    });
    let mut schema: Vec<FieldDecl> = facts
        .declarations
        .into_iter()
        .map(|(name, ty)| FieldDecl { ty: Some(ty), ..FieldDecl::new(name) })
        .collect();
    for key in [project::TIMESTAMP, project::BEGIN, project::END] {
        if !schema.iter().any(|d| d.name == key) && is_date_time_column(&fc.features, key) {
            schema.push(FieldDecl { ty: Some(DeclType::DateTime), ..FieldDecl::new(key) });
        }
    }
    fc.schema = schema;
    fc.native = Some(Native::new(format, NativePayload::Kml(document)));
    fc
}

/// Whether the time column `key` is a date-time column: it has a value, and
/// every value is a whole XML Schema `dateTime`. A `date`, `gYearMonth` or
/// `gYear` — which KML's time admits too — leaves the column text, since no
/// date-time holds one without inventing the rest (D-K15, D17 applied per
/// column).
fn is_date_time_column(features: &[Feature], key: &str) -> bool {
    let mut values = features.iter().flat_map(|f| f.properties.iter().filter(|(k, _)| &**k == key)).peekable();
    values.peek().is_some()
        && values.all(|(_, v)| matches!(v, crate::json::JsonValue::String(s) if project::is_xsd_date_time(s)))
}

/// Which KML this is, per `plans/kml-standard.org` § VERSION DETECTION, or why
/// it is not one this crate reads (D-K1).
fn detect(doc: &KmlDocument) -> Result<KmlNative> {
    let root = &doc.root;
    let namespace = doc.root_ns.as_deref();
    if let Some((_, version)) = GOOGLE_PRE_OGC.iter().find(|(ns, _)| Some(*ns) == namespace) {
        return Err(err(&format!(
            "the document is Google's pre-OGC KML {version} (namespace \"{}\"); only OGC KML 2.2 and 2.3 are \
             supported",
            namespace.unwrap_or_default()
        )));
    }
    if let Kind::Foreign(foreign) = &root.kind {
        // Read by local name alone this document would yield whatever it
        // happens to share with KML; matched by namespace it yields nothing,
        // silently. Neither is a reading of it.
        return Err(err(&format!(
            "the root element <{}> is in namespace \"{}\", which is not a KML namespace",
            foreign.qualified_name,
            namespace.unwrap_or_default()
        )));
    }
    let version_attr = root.attr("version");
    let stamped = match version_attr {
        None => KmlVersion::V2_2,
        Some(v) => parse_version_stamp(v)
            .ok_or_else(|| err(&format!("unknown KML version \"{v}\" (only OGC KML 2.2 and 2.3 are supported)")))?,
    };
    // A 2.3-only component makes the document 2.3 whatever it declares; the
    // schema's assertions are written about OGC's namespace, which Google's
    // pre-OGC 2.2 namespace predates.
    let version = if stamped == KmlVersion::V2_2 && namespace != Some(GOOGLE_KML_2_2_NS) && uses_2_3_only(root) {
        KmlVersion::V2_3
    } else {
        stamped
    };
    Ok(KmlNative {
        version,
        namespace: namespace.map(str::to_string),
        version_attr: version_attr.map(str::to_string),
        document: None,
    })
}

/// A `version` attribute's value, by `ogckml23.xsd`'s `kmlVersionType`:
/// `2.[2-3](.(0|[1-9][0-9]?))?`.
fn parse_version_stamp(value: &str) -> Option<KmlVersion> {
    let (version, rest) = match value.get(..3)? {
        "2.2" => (KmlVersion::V2_2, &value[3..]),
        "2.3" => (KmlVersion::V2_3, &value[3..]),
        _ => return None,
    };
    let patch_ok = match rest.strip_prefix('.') {
        None => rest.is_empty(),
        Some(p) => p == "0" || (matches!(p.len(), 1 | 2) && !p.starts_with('0') && p.bytes().all(|b| b.is_ascii_digit())),
    };
    patch_ok.then_some(version)
}

/// Whether the document uses a component KML 2.3 added, which makes it 2.3
/// whatever it declares. The list is `ogckml23.xsd`'s own, transcribed from the
/// 23 assertions on `kml:kml` (schema lines 770--924, SHA-256
/// `5222be37...695bcd`), each of the form
/// `if (.//X) then starts-with(@version,'2.3')`:
///
/// | lines   | `.//` path                                                        |
/// |---------+-------------------------------------------------------------------|
/// | 770--811 | `Track`, `MultiTrack`, `Tour`, `LatLonQuad`, `seaFloorAltitudeMode`, `altitudeOffset` |
/// | 812     | `balloonVisibility`                                                |
/// | 819--846 | `Camera/TimeStamp`, `Camera/TimeSpan`, `LookAt/TimeStamp`, `LookAt/TimeSpan` |
/// | 847     | `SimpleArrayField`                                                 |
/// | 854--861 | `SimpleField/@uom`, `Data/@uom`                                   |
/// | 868--875 | `Camera/horizFov`, `LookAt/horizFov`                              |
/// | 882     | `Create/MultiGeometry` — the only `Create` child listed            |
/// | 889--924 | `Delete/` `MultiGeometry`, `Point`, `LineString`, `LinearRing`, `Polygon`, `Model` |
///
/// Only elements spelled in the KML namespace match: `gx:Track` is 2.2 + `gx`,
/// not `kml:Track`, and a `gx:TimeStamp` in a view is 2.2's own extension.
/// `.//` also reaches into extension subtrees, which the model carries as
/// their source bytes and this walk does not enter; a 2.3 component nested
/// inside foreign content is the one case the walk misses.
fn uses_2_3_only(node: &Node) -> bool {
    use KmlElement as K;
    let in_kml = |n: &Node| matches!(n.kind, Kind::Kml { spelling: Spelling::Kml, .. });
    let kml_children = || node.children.iter().filter(|c| in_kml(c)).filter_map(Node::element);
    let found = in_kml(node)
        && match node.element() {
            Some(
                K::Track
                | K::MultiTrack
                | K::Tour
                | K::LatLonQuad
                | K::SeaFloorAltitudeMode
                | K::AltitudeOffset
                | K::BalloonVisibility
                | K::SimpleArrayField,
            ) => true,
            Some(K::SimpleField | K::Data) => node.attr("uom").is_some(),
            Some(K::Camera | K::LookAt) => {
                kml_children().any(|e| matches!(e, K::TimeStamp | K::TimeSpan | K::HorizFov))
            }
            Some(K::Create) => kml_children().any(|e| e == K::MultiGeometry),
            Some(K::Delete) => kml_children().any(|e| {
                matches!(e, K::MultiGeometry | K::Point | K::LineString | K::LinearRing | K::Polygon | K::Model)
            }),
            _ => false,
        };
    found || node.children.iter().any(uses_2_3_only)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry::*;
    use crate::json::JsonValue;
    use crate::geometry::Position;

    fn read(input: &[u8]) -> Result<FeatureCollection> {
        super::read(input, &mut Vec::new())
    }

    #[test]
    fn reads_a_point_with_name_and_description() {
        let kml = r#"<kml><Document><Placemark>
            <name>Empire State</name>
            <description>A building</description>
            <Point><coordinates>-73.9857,40.7484,10</coordinates></Point>
        </Placemark></Document></kml>"#;
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(fc.crs, Some(Crs::Wgs84));
        assert_eq!(fc.features.len(), 1);
        let f = &fc.features[0];
        assert_eq!(f.geometry, Some(Point(Position::with_z(-73.9857, 40.7484, 10.0))));
        let prop = |k: &str| f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone());
        assert_eq!(prop("name").unwrap().as_str(), Some("Empire State"));
        assert_eq!(prop("description").unwrap().as_str(), Some("A building"));
    }

    #[test]
    fn reads_line_string_and_polygon_with_hole() {
        let kml = r#"<kml>
            <Placemark><LineString><coordinates>0,0 1,1 2,0</coordinates></LineString></Placemark>
            <Placemark><Polygon>
                <outerBoundaryIs><LinearRing><coordinates>0,0 4,0 4,4 0,4 0,0</coordinates></LinearRing></outerBoundaryIs>
                <innerBoundaryIs><LinearRing><coordinates>1,1 2,1 2,2 1,2 1,1</coordinates></LinearRing></innerBoundaryIs>
            </Polygon></Placemark>
        </kml>"#;
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(fc.features.len(), 2);
        assert_eq!(
            fc.features[0].geometry,
            Some(LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)]))
        );
        assert_eq!(
            fc.features[1].geometry,
            Some(Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 4.0), Position::new(0.0, 0.0)],
                vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)],
            ]))
        );
    }

    #[test]
    fn multi_geometry_same_type_collapses_to_multi_variant() {
        let kml = r#"<kml><Placemark><MultiGeometry>
            <Point><coordinates>0,0</coordinates></Point>
            <Point><coordinates>1,1</coordinates></Point>
        </MultiGeometry></Placemark></kml>"#;
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(fc.features[0].geometry, Some(MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)])));
    }

    #[test]
    fn multi_geometry_mixed_type_becomes_geometry_collection() {
        let kml = r#"<kml><Placemark><MultiGeometry>
            <Point><coordinates>0,0</coordinates></Point>
            <LineString><coordinates>0,0 1,1</coordinates></LineString>
        </MultiGeometry></Placemark></kml>"#;
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(GeometryCollection(vec![
                Point(Position::new(0.0, 0.0)),
                LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]),
            ]))
        );
    }

    #[test]
    fn reads_extended_data_both_forms() {
        let kml = r#"<kml><Placemark>
            <ExtendedData>
                <Data name="population"><value>8000000</value></Data>
                <SchemaData><SimpleData name="rank">1</SimpleData></SchemaData>
            </ExtendedData>
            <Point><coordinates>0,0</coordinates></Point>
        </Placemark></kml>"#;
        let fc = read(kml.as_bytes()).unwrap();
        let prop = |k: &str| {
            fc.features[0]
                .properties
                .iter()
                .find(|(n, _)| &**n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(prop("population").unwrap().as_str(), Some("8000000"));
        assert_eq!(prop("rank").unwrap().as_str(), Some("1"));
    }

    #[test]
    fn flattens_folders_and_nested_documents() {
        let kml = r#"<kml><Document><Folder><Folder>
            <Placemark><Point><coordinates>0,0</coordinates></Point></Placemark>
        </Folder></Folder></Document></kml>"#;
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(fc.features.len(), 1);
    }

    #[test]
    fn placemark_with_no_geometry_has_none() {
        let kml = r#"<kml><Placemark><name>No shape</name></Placemark></kml>"#;
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(fc.features[0].geometry, None);
    }

    #[test]
    fn tolerates_newline_separated_coordinates() {
        let kml = "<kml><Placemark><LineString><coordinates>\n0,0\n1,1\n</coordinates></LineString></Placemark></kml>";
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(fc.features[0].geometry, Some(LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)])));
    }

    #[test]
    fn rejects_malformed_coordinates() {
        let kml = "<kml><Placemark><Point><coordinates>not-a-number,0</coordinates></Point></Placemark></kml>";
        assert!(read(kml.as_bytes()).is_err());
    }

    #[test]
    fn a_2d_point_has_no_z() {
        let kml = "<kml><Placemark><Point><coordinates>0,0</coordinates></Point></Placemark></kml>";
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(fc.features[0].geometry, Some(Point(Position::new(0.0, 0.0))));
    }

    #[test]
    fn line_string_carries_z_per_point() {
        let kml = "<kml><Placemark><LineString><coordinates>0,0,1 1,1,2 2,0,3</coordinates></LineString></Placemark></kml>";
        let fc = read(kml.as_bytes()).unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(LineString(vec![
                Position::with_z(0.0, 0.0, 1.0),
                Position::with_z(1.0, 1.0, 2.0),
                Position::with_z(2.0, 0.0, 3.0),
            ]))
        );
    }

    #[test]
    fn rejects_non_numeric_altitude() {
        let kml = "<kml><Placemark><Point><coordinates>0,0,not-a-number</coordinates></Point></Placemark></kml>";
        assert!(read(kml.as_bytes()).is_err());
    }

    /// The collection, and what writing it to GeoJSON would lose of its KML —
    /// where a `<Model>` and a track's per-vertex data are reported, since a
    /// KML write carries them.
    fn read_warning(kml: &str) -> (FeatureCollection, Vec<String>) {
        let mut read_warnings = Vec::new();
        let fc = super::read(kml.as_bytes(), &mut read_warnings).unwrap();
        assert!(read_warnings.is_empty(), "a KML read reports nothing: {read_warnings:?}");
        let warnings = crate::kml::drop_warnings(&fc, Format::GeoJson);
        (fc, warnings)
    }

    const NS: &str = r#"xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2""#;

    #[test]
    fn a_bare_linear_ring_is_a_line_string() {
        let (fc, warnings) =
            read_warning("<kml><Placemark><LinearRing><coordinates>0,0 1,0 1,1 0,0</coordinates></LinearRing></Placemark></kml>");
        assert_eq!(
            fc.features[0].geometry,
            Some(LineString(vec![
                Position::new(0.0, 0.0),
                Position::new(1.0, 0.0),
                Position::new(1.0, 1.0),
                Position::new(0.0, 0.0),
            ]))
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_gx_track_is_a_line_string_z_and_its_times_are_reported() {
        let (fc, warnings) = read_warning(&format!(
            "<kml {NS}><Placemark><gx:Track><altitudeMode>absolute</altitudeMode><when>2026-01-01T00:00:00Z</when>\
             <when>2026-01-01T00:01:00Z</when><gx:coord>1 2 3</gx:coord><gx:coord>4 5 6</gx:coord></gx:Track>\
             </Placemark></kml>"
        ));
        assert_eq!(
            fc.features[0].geometry,
            Some(LineString(vec![Position::with_z(1.0, 2.0, 3.0), Position::with_z(4.0, 5.0, 6.0)]))
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("kml: 1 track(s) carry per-vertex data"), "{warnings:?}");
        assert!(crate::kml::drop_warnings(&fc, Format::Kml).is_empty(), "a KML write carries them");
    }

    #[test]
    fn a_kml_2_3_track_reads_as_its_gx_spelling_does() {
        let (fc, _) = read_warning(
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3"><Placemark><Track><coord>1 2 3</coord></Track></Placemark></kml>"#,
        );
        assert_eq!(fc.features[0].geometry, Some(LineString(vec![Position::with_z(1.0, 2.0, 3.0)])));
    }

    #[test]
    fn a_track_with_only_coordinates_loses_nothing_and_says_nothing() {
        let (_, warnings) =
            read_warning(&format!(
                "<kml {NS}><Placemark><gx:Track><altitudeMode>absolute</altitudeMode><gx:coord>1 2 3</gx:coord>\
                 </gx:Track></Placemark></kml>"
            ));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn an_undeclared_gx_prefix_is_still_a_track() {
        let (fc, _) = read_warning("<kml><Placemark><gx:Track><gx:coord>1 2 3</gx:coord></gx:Track></Placemark></kml>");
        assert!(fc.features[0].geometry.is_some());
    }

    #[test]
    fn a_multi_track_is_a_multi_line_string() {
        let (fc, _) = read_warning(&format!(
            "<kml {NS}><Placemark><gx:MultiTrack><gx:interpolate>0</gx:interpolate>\
             <gx:Track><gx:coord>1 2 3</gx:coord></gx:Track><gx:Track><gx:coord>4 5 6</gx:coord></gx:Track>\
             </gx:MultiTrack></Placemark></kml>"
        ));
        assert_eq!(
            fc.features[0].geometry,
            Some(MultiLineString(vec![vec![Position::with_z(1.0, 2.0, 3.0)], vec![Position::with_z(4.0, 5.0, 6.0)]]))
        );
    }

    #[test]
    fn models_are_kept_without_geometry_and_counted_once() {
        let model = "<Model><Location><longitude>8.5</longitude><latitude>47.4</latitude></Location></Model>";
        let (fc, warnings) = read_warning(&format!(
            "<kml {NS}><Placemark><name>a</name>{model}</Placemark><Placemark><name>b</name>{model}</Placemark></kml>"
        ));
        assert_eq!(fc.features.len(), 2);
        assert!(fc.features.iter().all(|f| f.geometry.is_none()));
        assert_eq!(
            warnings,
            vec!["kml: 2 feature(s) hold <Model>, which this GeoJSON write cannot carry; written without that geometry"]
        );
        assert!(crate::kml::drop_warnings(&fc, Format::Kml).is_empty(), "a KML write carries them");
    }

    #[test]
    fn a_model_inside_a_multi_geometry_leaves_the_rest_and_is_counted() {
        let (fc, warnings) = read_warning(&format!(
            "<kml {NS}><Placemark><MultiGeometry><Point><coordinates>1,2</coordinates></Point><Model/></MultiGeometry>\
             </Placemark><Placemark><MultiGeometry><Model/></MultiGeometry></Placemark></kml>"
        ));
        assert_eq!(fc.features[0].geometry, Some(MultiPoint(vec![Position::new(1.0, 2.0)])));
        assert_eq!(fc.features[1].geometry, None);
        assert!(warnings[0].starts_with("kml: 2 feature(s) hold <Model>"), "{warnings:?}");
    }

    #[test]
    fn an_extension_element_with_a_kml_local_name_is_not_kml() {
        let (fc, _) = read_warning(
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:x="urn:x"><Placemark><x:name>not a name</x:name><x:Point><x:coordinates>1,2</x:coordinates></x:Point></Placemark></kml>"#,
        );
        assert!(fc.features[0].geometry.is_none());
        assert!(fc.features[0].properties.is_empty());
    }

    #[test]
    fn a_prefixed_kml_document_reads() {
        let (fc, _) = read_warning(
            r#"<k:kml xmlns:k="http://www.opengis.net/kml/2.2"><k:Placemark><k:name>p</k:name><k:Point><k:coordinates>1,2</k:coordinates></k:Point></k:Placemark></k:kml>"#,
        );
        assert_eq!(fc.features[0].geometry, Some(Point(Position::new(1.0, 2.0))));
        assert_eq!(fc.features[0].properties[0].1.as_str(), Some("p"));
    }

    #[test]
    fn a_root_in_a_foreign_namespace_is_refused_rather_than_read_as_empty() {
        let err = super::read(br#"<kml xmlns="urn:not-kml"><Placemark/></kml>"#, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("urn:not-kml"), "{err}");
    }

    #[test]
    fn utf16_kml_reads() {
        let doc = r#"<?xml version="1.0" encoding="UTF-16"?><kml><Placemark><name>Zürich</name></Placemark></kml>"#;
        let mut bytes = vec![0xFF, 0xFE];
        for u in doc.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let fc = read(&bytes).unwrap();
        assert_eq!(fc.features[0].properties[0].1.as_str(), Some("Zürich"));
    }

    fn document(kml: &str) -> KmlNative {
        match read(kml.as_bytes()).unwrap().native.unwrap().payload_for(Format::Kml) {
            Some(NativePayload::Kml(d)) => d.clone(),
            other => panic!("expected a KML native, got {other:?}"),
        }
    }

    fn version_of(kml: &str) -> KmlVersion {
        document(kml).version
    }

    const OGC: &str = r#"xmlns="http://www.opengis.net/kml/2.2""#;

    #[test]
    fn ogc_kml_with_no_stamp_and_nothing_2_3_only_is_2_2() {
        assert_eq!(version_of(&format!("<kml {OGC}><Placemark/></kml>")), KmlVersion::V2_2);
    }

    #[test]
    fn the_version_stamp_is_read_in_every_form_the_schema_admits() {
        for (stamp, v) in [("2.2", KmlVersion::V2_2), ("2.3", KmlVersion::V2_3), ("2.3.0", KmlVersion::V2_3), ("2.2.1", KmlVersion::V2_2), ("2.3.12", KmlVersion::V2_3)] {
            assert_eq!(version_of(&format!(r#"<kml {OGC} version="{stamp}"/>"#)), v, "{stamp}");
        }
    }

    #[test]
    fn a_version_the_schema_does_not_admit_is_refused_by_name() {
        for stamp in ["2.4", "3.0", "2.3.01", "2.3.", "two"] {
            let err = super::read(format!(r#"<kml {OGC} version="{stamp}"/>"#).as_bytes(), &mut Vec::new()).unwrap_err();
            assert!(err.to_string().contains(&format!("\"{stamp}\"")), "{stamp}: {err}");
        }
    }

    #[test]
    fn a_2_3_only_component_makes_the_document_2_3_whatever_it_declares() {
        for body in [
            "<Placemark><Track/></Placemark>",
            "<Placemark><balloonVisibility>1</balloonVisibility></Placemark>",
            "<Document><Schema><SimpleField name='d' type='double' uom='urn:m'/></Schema></Document>",
            "<Placemark><Camera><horizFov>60</horizFov></Camera></Placemark>",
            "<Placemark><LookAt><TimeStamp/></LookAt></Placemark>",
            "<NetworkLinkControl><Update><Delete><Point/></Delete></Update></NetworkLinkControl>",
        ] {
            assert_eq!(version_of(&format!("<kml {OGC}>{body}</kml>")), KmlVersion::V2_3, "{body}");
            assert_eq!(version_of(&format!(r#"<kml {OGC} version="2.2">{body}</kml>"#)), KmlVersion::V2_3, "{body}");
        }
    }

    #[test]
    fn what_the_schema_does_not_list_leaves_the_document_2_2() {
        for body in [
            // gx:Track is 2.2 + gx, not kml:Track.
            r#"<Placemark><gx:Track xmlns:gx="http://www.google.com/kml/ext/2.2"/></Placemark>"#,
            // Create is listed only with a MultiGeometry child.
            "<NetworkLinkControl><Update><Create><Point/></Create></Update></NetworkLinkControl>",
            // A TimeStamp on a feature, not a view.
            "<Placemark><TimeStamp/></Placemark>",
            // uom is listed on SimpleField and Data only.
            "<Placemark><name uom='x'/></Placemark>",
        ] {
            assert_eq!(version_of(&format!("<kml {OGC}>{body}</kml>")), KmlVersion::V2_2, "{body}");
        }
    }

    #[test]
    fn googles_pre_ogc_2_2_namespace_is_2_2_with_its_namespace_recorded() {
        let d = document(r#"<kml xmlns="http://earth.google.com/kml/2.2"><Placemark><Track/></Placemark></kml>"#);
        assert_eq!(d.version, KmlVersion::V2_2, "the 2.3 assertions are about OGC's namespace");
        assert_eq!(d.namespace.as_deref(), Some("http://earth.google.com/kml/2.2"));
    }

    #[test]
    fn googles_kml_2_0_and_2_1_are_refused_saying_what_is_supported() {
        for v in ["2.0", "2.1"] {
            let kml = format!(r#"<kml xmlns="http://earth.google.com/kml/{v}"><Placemark/></kml>"#);
            let err = super::read(kml.as_bytes(), &mut Vec::new()).unwrap_err().to_string();
            assert!(err.contains(&format!("KML {v}")), "{err}");
            assert!(err.contains("only OGC KML 2.2 and 2.3"), "{err}");
        }
    }

    #[test]
    fn a_document_with_no_namespace_is_2_2_with_its_absence_and_stamp_recorded() {
        let d = document("<kml><Placemark/></kml>");
        assert_eq!((d.version, d.namespace, d.version_attr), (KmlVersion::V2_2, None, None));
        let d = document(r#"<kml version="2.3.0"/>"#);
        assert_eq!((d.version, d.version_attr.as_deref()), (KmlVersion::V2_3, Some("2.3.0")));
    }

    #[test]
    fn a_declared_latin_1_document_reads_as_what_it_declared() {
        let fc = read(b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><kml><Placemark><name>K\xF8benhavn</name></Placemark></kml>")
            .unwrap();
        assert_eq!(fc.features[0].properties[0].1.as_str(), Some("København"));
    }

    fn props(f: &Feature) -> Vec<(&str, JsonValue)> {
        f.properties.iter().map(|(k, v)| (&**k, v.clone())).collect()
    }

    fn text(s: &str) -> JsonValue {
        JsonValue::String(s.into())
    }

    fn decl(fc: &FeatureCollection, name: &str) -> Option<DeclType> {
        fc.schema.iter().find(|d| d.name == name).and_then(|d| d.ty)
    }

    #[test]
    fn a_features_time_projects_as_gdals_names_verbatim() {
        let fc = read(
            b"<kml><Placemark><TimeStamp id='t'><when>2026-10-22T09:30:00-05:00</when></TimeStamp></Placemark>\
              <Placemark><TimeSpan><begin>2026-01</begin><end>2026-03-15</end></TimeSpan></Placemark>\
              <Placemark><TimeSpan><end>1997</end></TimeSpan></Placemark></kml>",
        )
        .unwrap();
        assert_eq!(props(&fc.features[0]), [("timestamp", text("2026-10-22T09:30:00-05:00"))]);
        assert_eq!(props(&fc.features[1]), [("begin", text("2026-01")), ("end", text("2026-03-15"))]);
        assert_eq!(props(&fc.features[2]), [("end", text("1997"))]);
        // Only a column of whole date-times is declared one; `2026-01` is no
        // instant, so neither span column is.
        assert_eq!(decl(&fc, "timestamp"), Some(DeclType::DateTime));
        assert_eq!(decl(&fc, "begin"), None);
        assert_eq!(decl(&fc, "end"), None);
    }

    #[test]
    fn altitude_members_project_only_where_the_source_has_them() {
        let fc = read(
            b"<kml><Placemark><Point><extrude>1</extrude><altitudeMode>absolute</altitudeMode>\
              <coordinates>1,2,3</coordinates></Point></Placemark>\
              <Placemark><Point><coordinates>1,2,3</coordinates></Point></Placemark></kml>",
        )
        .unwrap();
        assert_eq!(props(&fc.features[0]), [("altitudeMode", text("absolute")), ("extrude", text("1"))]);
        assert!(fc.features[1].properties.is_empty(), "no element, no property: GDAL's extrude=0 is not invented");
    }

    #[test]
    fn a_multi_geometry_projects_the_altitude_members_its_members_agree_on() {
        let fc = read(
            b"<kml><Placemark><MultiGeometry>\
              <Point><altitudeMode>absolute</altitudeMode><extrude>1</extrude><coordinates>1,2,3</coordinates></Point>\
              <LineString><altitudeMode>absolute</altitudeMode><extrude>0</extrude><tessellate>1</tessellate>\
              <coordinates>1,2,3 4,5,6</coordinates></LineString></MultiGeometry></Placemark></kml>",
        )
        .unwrap();
        assert_eq!(props(&fc.features[0]), [("altitudeMode", text("absolute")), ("tessellate", text("1"))]);
    }

    #[test]
    fn the_collection_is_egm96_when_every_height_is_absolute() {
        let absolute = "<Point><altitudeMode>absolute</altitudeMode><coordinates>1,2,3</coordinates></Point>";
        let relative = "<Point><altitudeMode>relativeToGround</altitudeMode><coordinates>1,2,3</coordinates></Point>";
        let clamped = "<Point><coordinates>1,2,3</coordinates></Point>";
        let flat = "<Point><altitudeMode>relativeToGround</altitudeMode><coordinates>1,2</coordinates></Point>";
        let crs = |placemarks: &[&str]| {
            let body: String = placemarks.iter().map(|p| format!("<Placemark>{p}</Placemark>")).collect();
            read(format!("<kml>{body}</kml>").as_bytes()).unwrap().crs.unwrap()
        };
        assert_eq!(crs(&[absolute, absolute, flat]), Crs::wgs84_egm96());
        assert_eq!(crs(&[absolute, relative]), Crs::Wgs84);
        assert_eq!(crs(&[clamped]), Crs::Wgs84, "an absent mode is clampToGround");
        assert_eq!(crs(&[flat]), Crs::Wgs84, "no Z, no vertical datum to state");
        let multi_track = "<gx:MultiTrack xmlns:gx='http://www.google.com/kml/ext/2.2'><altitudeMode>absolute</altitudeMode>\
                           <gx:Track><gx:coord>1 2 3</gx:coord></gx:Track></gx:MultiTrack>";
        assert_eq!(crs(&[multi_track]), Crs::wgs84_egm96(), "a track takes its multi-track's mode");
        let sea_floor = "<Point><gx:altitudeMode xmlns:gx='http://www.google.com/kml/ext/2.2'>clampToSeaFloor</gx:altitudeMode>\
                         <coordinates>1,2,3</coordinates></Point>";
        assert_eq!(crs(&[sea_floor]), Crs::Wgs84);
    }

    const SCHEMA: &str = "<Schema id='s'><SimpleField name='n' type='int'/><SimpleField name='d' type='double'/>\
        <SimpleField name='b' type='bool'/><SimpleField name='t' type='string'/></Schema>";

    #[test]
    fn simple_data_is_typed_by_the_schema_it_names() {
        let fc = read(
            format!(
                "<kml><Document>{SCHEMA}<Placemark><ExtendedData><SchemaData schemaUrl='#s'>\
                 <SimpleData name='n'>007</SimpleData><SimpleData name='d'>1.50</SimpleData>\
                 <SimpleData name='b'>1</SimpleData><SimpleData name='t'>42</SimpleData></SchemaData>\
                 <SchemaData schemaUrl='other.kml#s'><SimpleData name='n'>8</SimpleData></SchemaData>\
                 </ExtendedData></Placemark></Document></kml>"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            props(&fc.features[0]),
            [
                ("n", JsonValue::Int(7)),
                ("d", JsonValue::Float(1.5)),
                ("b", JsonValue::Bool(true)),
                ("t", text("42")),
                // A schema in another document is not this one's to read.
                ("n", text("8")),
            ]
        );
        let decls: Vec<_> = fc.schema.iter().map(|d| (d.name.as_str(), d.ty)).collect();
        assert_eq!(
            decls,
            [("n", Some(DeclType::Int32)), ("d", Some(DeclType::Float64)), ("b", Some(DeclType::Bool)), ("t", Some(DeclType::String))]
        );
    }

    #[test]
    fn a_name_two_schemas_type_differently_is_declared_by_neither() {
        let fc = read(
            b"<kml><Document><Schema id='a'><SimpleField name='n' type='int'/><SimpleField name='k' type='int'/></Schema>\
              <Schema id='b'><SimpleField name='n' type='string'/></Schema></Document></kml>",
        )
        .unwrap();
        assert_eq!(decl(&fc, "n"), None);
        assert_eq!(decl(&fc, "k"), Some(DeclType::Int32));
    }

    #[test]
    fn a_ground_overlay_is_its_footprint_with_its_icon() {
        let fc = read(
            b"<kml><GroundOverlay><name>o</name><Icon><href>img.png</href></Icon>\
              <LatLonBox><north>56</north><south>55</south><east>13</east><west>12</west></LatLonBox></GroundOverlay>\
              <GroundOverlay><gx:LatLonQuad xmlns:gx='http://www.google.com/kml/ext/2.2'>\
              <coordinates>0,0 2,0 2.5,1 0,1</coordinates></gx:LatLonQuad></GroundOverlay></kml>",
        )
        .unwrap();
        let ring = |coords: &[(f64, f64)]| Some(Polygon(vec![coords.iter().map(|&(x, y)| Position::new(x, y)).collect()]));
        assert_eq!(props(&fc.features[0]), [("name", text("o")), ("icon", text("img.png"))]);
        assert_eq!(fc.features[0].geometry, ring(&[(12.0, 55.0), (13.0, 55.0), (13.0, 56.0), (12.0, 56.0), (12.0, 55.0)]));
        assert_eq!(fc.features[1].geometry, ring(&[(0.0, 0.0), (2.0, 0.0), (2.5, 1.0), (0.0, 1.0), (0.0, 0.0)]));
    }

    #[test]
    fn a_photo_overlay_is_its_point_and_a_screen_overlay_is_no_feature() {
        let fc = read(
            b"<kml><Document><PhotoOverlay><Icon><href>p.jpg</href></Icon><Point><altitudeMode>absolute</altitudeMode>\
              <coordinates>1,2,3</coordinates></Point></PhotoOverlay>\
              <ScreenOverlay><Icon><href>logo.png</href></Icon></ScreenOverlay></Document></kml>",
        )
        .unwrap();
        assert_eq!(fc.features.len(), 1);
        assert_eq!(fc.features[0].geometry, Some(Point(Position::with_z(1.0, 2.0, 3.0))));
        assert_eq!(props(&fc.features[0]), [("icon", text("p.jpg")), ("altitudeMode", text("absolute"))]);
        assert_eq!(fc.crs, Some(Crs::wgs84_egm96()));
    }
}
