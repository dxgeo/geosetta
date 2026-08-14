//! Feature IR -> `.kml` (plain XML) bytes. The inverse of `reader.rs`.
//!
//! KML coordinates are always WGS 84 longitude/latitude; a non-WGS-84 source
//! is a CRS-loss case already surfaced by `Crs::downgrade_warning`'s merged
//! `GeoJson | Kml` arm, so this writer doesn't need to look at the source CRS
//! at all — it just emits whatever coordinates the geometry carries.

use crate::feature::{Feature, FeatureCollection};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::xml::{self, XmlElement};

/// Encode the Feature IR as a `.kml` document: a `<Document>` containing one
/// `<Placemark>` per feature.
pub(crate) fn write(fc: &FeatureCollection) -> Vec<u8> {
    let placemarks = fc.features.iter().map(feature_to_placemark).collect();
    let document = XmlElement::with_children("Document", placemarks);
    let root = XmlElement::with_children("kml", vec![document])
        .with_attr("xmlns", "http://www.opengis.net/kml/2.2");
    xml::write(&root).into_bytes()
}

/// `name`/`description` properties become their own elements (the inverse of
/// the reader's special-casing); every other property round-trips through
/// `<ExtendedData><Data name="k"><value>v</value></Data></ExtendedData>` —
/// the reader's other supported form, `<SchemaData>/<SimpleData>`, is read
/// but not written, since it needs a companion `<Schema>` this writer has no
/// reason to generate.
fn feature_to_placemark(f: &Feature) -> XmlElement {
    let mut children = Vec::new();
    let mut extended_data = Vec::new();
    for (key, value) in &f.properties {
        let text = property_text(value);
        match key.as_ref() {
            "name" => children.push(XmlElement::leaf("name", text)),
            "description" => children.push(XmlElement::leaf("description", text)),
            _ => extended_data.push(
                XmlElement::with_children("Data", vec![XmlElement::leaf("value", text)])
                    .with_attr("name", key.as_ref()),
            ),
        }
    }
    if !extended_data.is_empty() {
        children.push(XmlElement::with_children("ExtendedData", extended_data));
    }
    if let Some(g) = &f.geometry {
        children.push(geometry_to_element(g));
    }
    XmlElement::with_children("Placemark", children)
}

/// A property value as `<Data>`'s plain text. Nested arrays/objects fall back
/// to compact JSON, the same convention `schema::infer_columns` uses for
/// heterogeneous/nested properties elsewhere.
fn property_text(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => s.clone(),
        JsonValue::Null => String::new(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Number { value, is_int } => {
            if *is_int { (*value as i64).to_string() } else { value.to_string() }
        }
        JsonValue::Array(_) | JsonValue::Object(_) => value.to_json_string(),
    }
}

fn geometry_to_element(g: &Geometry) -> XmlElement {
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
    for (i, [lon, lat]) in points.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        let _ = write!(text, "{lon},{lat}");
    }
    XmlElement::leaf("coordinates", text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kml::reader;
    use std::rc::Rc;

    fn feature(geometry: Option<Geometry>, properties: Vec<(&str, JsonValue)>) -> Feature {
        Feature {
            geometry,
            properties: properties.into_iter().map(|(k, v)| (Rc::from(k), v)).collect(),
        }
    }

    #[test]
    fn writes_name_description_and_extended_data() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Point([1.0, 2.0])),
            vec![
                ("name", JsonValue::String("A Point".into())),
                ("description", JsonValue::String("desc".into())),
                ("height_m", JsonValue::Number { value: 381.0, is_int: true }),
            ],
        )]);
        let bytes = write(&fc);
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("<name>A Point</name>"));
        assert!(text.contains("<description>desc</description>"));
        assert!(text.contains("<Data name=\"height_m\"><value>381</value></Data>"));
        assert!(text.contains("<Point><coordinates>1,2</coordinates></Point>"));
    }

    #[test]
    fn writes_polygon_with_hole() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::Polygon(vec![
                vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]],
                vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]],
            ])),
            vec![],
        )]);
        let text = String::from_utf8(write(&fc)).unwrap();
        assert!(text.contains("<outerBoundaryIs><LinearRing><coordinates>0,0 4,0 4,4 0,4 0,0</coordinates></LinearRing></outerBoundaryIs>"));
        assert!(text.contains("<innerBoundaryIs><LinearRing><coordinates>1,1 2,1 2,2 1,2 1,1</coordinates></LinearRing></innerBoundaryIs>"));
    }

    #[test]
    fn writes_multi_point_as_multi_geometry() {
        let fc = FeatureCollection::new(vec![feature(
            Some(Geometry::MultiPoint(vec![[0.0, 0.0], [1.0, 1.0]])),
            vec![],
        )]);
        let text = String::from_utf8(write(&fc)).unwrap();
        assert!(text.contains("<MultiGeometry><Point><coordinates>0,0</coordinates></Point><Point><coordinates>1,1</coordinates></Point></MultiGeometry>"));
    }

    #[test]
    fn geojson_to_kml_to_geojson_round_trips_geometry_and_properties() {
        use crate::geojson;
        use crate::json;
        let src = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[-73.9857,40.7484]},
             "properties":{"name":"Empire State","height_m":381,"landmark":true}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[-73.99,40.75],[-73.98,40.76]]},
             "properties":{"name":"A Path"}}
        ]}"#;
        let original = geojson::from_json(&json::parse(src).unwrap()).unwrap();
        let kml_bytes = write(&original);
        let back = reader::read(&kml_bytes).unwrap();
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
        let text = String::from_utf8(write(&fc)).unwrap();
        assert!(text.contains("<name>Fish &amp; Chips &lt;Shop&gt;</name>"));
    }
}
