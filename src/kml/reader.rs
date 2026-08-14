//! `.kml` (plain XML) -> Feature IR.
//!
//! KML coordinates are always WGS 84 longitude/latitude — the spec has no
//! other option, no per-file CRS element at all — so every collection this
//! produces carries [`Crs::Wgs84`] outright; none of the CRS resolution
//! machinery the richer spokes need applies here. See `plans/kml.org`.

use std::rc::Rc;

use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::xml::{self, XmlElement};

fn err(message: &str) -> Error {
    Error::Convert(format!("kml: {message}"))
}

/// Parse a `.kml` document's bytes into the Feature IR.
pub(crate) fn read(input: &[u8]) -> Result<FeatureCollection> {
    let text =
        std::str::from_utf8(input).map_err(|_| err("input is not valid utf-8"))?;
    let mut features = Vec::new();
    collect_features(text, &mut features)?;
    Ok(finish(features))
}

/// Parse one `.kml` document's text and append its `Placemark`s to `out` —
/// the piece `.kmz` reuses to flatten *every* internal `.kml` file into one
/// collection (see [`crate::kml::read_kmz`]: a real multi-layer KMZ, e.g.
/// GDAL/LIBKML's, stores a root `doc.kml` that's only a `<NetworkLink>`
/// pointing at the real data in `layers/*.kml`, so reading just the first
/// `.kml` entry silently produces zero features).
pub(crate) fn collect_features(text: &str, out: &mut Vec<Feature>) -> Result<()> {
    let root = xml::parse(text)?;
    collect_placemarks(&root, out)
}

/// Wrap collected features as a collection with KML's fixed CRS.
pub(crate) fn finish(features: Vec<Feature>) -> FeatureCollection {
    let mut fc = FeatureCollection::new(features);
    fc.crs = Some(Crs::Wgs84);
    fc
}

/// Walk the tree for `Placemark` elements anywhere under it. KML's
/// `Document`/`Folder` nesting exists only for display grouping, so it's
/// flattened away rather than modeled — the same "flatten, don't model
/// hierarchy" call `plans/kml.org` makes.
fn collect_placemarks(el: &XmlElement, out: &mut Vec<Feature>) -> Result<()> {
    if el.name == "Placemark" {
        out.push(placemark_to_feature(el)?);
        return Ok(());
    }
    for child in &el.children {
        collect_placemarks(child, out)?;
    }
    Ok(())
}

/// `<name>`/`<description>` become ordinary properties (cheap, commonly
/// consumed, unlike the rest of KML's styling/presentation tree, which is
/// dropped entirely — see `plans/kml.org`'s scoping call).
fn placemark_to_feature(el: &XmlElement) -> Result<Feature> {
    let mut properties: Vec<(Rc<str>, JsonValue)> = Vec::new();
    if let Some(name) = el.child("name") {
        properties.push((Rc::from("name"), JsonValue::String(name.text_trimmed().to_string())));
    }
    if let Some(description) = el.child("description") {
        properties.push((
            Rc::from("description"),
            JsonValue::String(description.text_trimmed().to_string()),
        ));
    }
    if let Some(extended_data) = el.child("ExtendedData") {
        collect_extended_data(extended_data, &mut properties);
    }
    let geometry = el
        .children
        .iter()
        .find_map(parse_geometry)
        .transpose()?;
    Ok(Feature { geometry, properties })
}

/// `<Data name="k"><value>v</value></Data>` and the schema-typed
/// `<SchemaData><SimpleData name="k">v</SimpleData></SchemaData>` forms both
/// flatten into properties, read as strings — the declared type in a
/// `<SchemaData>`'s companion `<Schema>` isn't chased (matches DBF's
/// loosely-typed text fields, left for `schema::infer_columns` downstream).
/// A `Data`/`SimpleData` with no `name` attribute has no property key to
/// attach to, so it's skipped rather than guessed.
fn collect_extended_data(extended_data: &XmlElement, properties: &mut Vec<(Rc<str>, JsonValue)>) {
    for data in extended_data.children_named("Data") {
        if let Some(name) = data.attr("name") {
            let value = data.child("value").map(XmlElement::text_trimmed).unwrap_or("");
            properties.push((Rc::from(name), JsonValue::String(value.to_string())));
        }
    }
    for schema_data in extended_data.children_named("SchemaData") {
        for simple in schema_data.children_named("SimpleData") {
            if let Some(name) = simple.attr("name") {
                properties.push((Rc::from(name), JsonValue::String(simple.text_trimmed().to_string())));
            }
        }
    }
}

/// Dispatch a child element to its geometry parser by local name, or `None`
/// if it isn't a geometry element at all (e.g. `<name>`, `<ExtendedData>`,
/// `<Style>`) — used to find a `Placemark`'s one geometry child among its
/// other children.
fn parse_geometry(el: &XmlElement) -> Option<Result<Geometry>> {
    match el.name.as_str() {
        "Point" => Some(parse_point(el)),
        "LineString" => Some(parse_line_string(el)),
        "Polygon" => Some(parse_polygon(el)),
        "MultiGeometry" => Some(parse_multi_geometry(el)),
        _ => None,
    }
}

fn coordinates_of(el: &XmlElement) -> Result<&XmlElement> {
    el.child("coordinates").ok_or_else(|| err(&format!("<{}> missing <coordinates>", el.name)))
}

fn parse_point(el: &XmlElement) -> Result<Geometry> {
    let positions = parse_coordinates(coordinates_of(el)?.text_trimmed())?;
    let pos = positions
        .into_iter()
        .next()
        .ok_or_else(|| err("<Point><coordinates> is empty"))?;
    Ok(Geometry::Point(pos))
}

fn parse_line_string(el: &XmlElement) -> Result<Geometry> {
    let positions = parse_coordinates(coordinates_of(el)?.text_trimmed())?;
    Ok(Geometry::LineString(positions))
}

/// A ring's coordinates, at `<outerBoundaryIs>`/`<innerBoundaryIs>` ->
/// `<LinearRing>` -> `<coordinates>`.
fn parse_ring(boundary: &XmlElement) -> Result<Vec<Position>> {
    let ring = boundary
        .child("LinearRing")
        .ok_or_else(|| err(&format!("<{}> missing <LinearRing>", boundary.name)))?;
    parse_coordinates(coordinates_of(ring)?.text_trimmed())
}

fn parse_polygon(el: &XmlElement) -> Result<Geometry> {
    let outer = el
        .child("outerBoundaryIs")
        .ok_or_else(|| err("<Polygon> missing <outerBoundaryIs>"))?;
    let mut rings = vec![parse_ring(outer)?];
    for inner in el.children_named("innerBoundaryIs") {
        rings.push(parse_ring(inner)?);
    }
    Ok(Geometry::Polygon(rings))
}

/// `<MultiGeometry>` wraps arbitrary children. Same-type children collapse to
/// the matching `Multi*` variant (KML has no `MultiPoint`/etc. element of its
/// own — only the singular forms plus this wrapper); mixed types become a
/// `GeometryCollection`, matching GeoJSON's variant of the same name.
fn parse_multi_geometry(el: &XmlElement) -> Result<Geometry> {
    let mut geoms = Vec::new();
    for child in &el.children {
        if let Some(g) = parse_geometry(child) {
            geoms.push(g?);
        }
    }
    if geoms.is_empty() {
        return Err(err("<MultiGeometry> has no recognized child geometries"));
    }
    Ok(combine(geoms))
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
/// uses. Altitude, if present, is parsed (to catch malformed input) and
/// dropped, matching the project's 2D-only stance.
fn parse_coordinates(text: &str) -> Result<Vec<Position>> {
    text.split_ascii_whitespace().map(parse_tuple).collect()
}

fn parse_tuple(tuple: &str) -> Result<Position> {
    let bad = || err(&format!("invalid coordinate tuple \"{tuple}\""));
    let mut parts = tuple.split(',');
    let lon: f64 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    let lat: f64 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    if let Some(alt) = parts.next() {
        alt.parse::<f64>().map_err(|_| bad())?;
    }
    Ok([lon, lat])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry::*;

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
        assert_eq!(f.geometry, Some(Point([-73.9857, 40.7484])));
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
            Some(LineString(vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]))
        );
        assert_eq!(
            fc.features[1].geometry,
            Some(Polygon(vec![
                vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]],
                vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]],
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
        assert_eq!(fc.features[0].geometry, Some(MultiPoint(vec![[0.0, 0.0], [1.0, 1.0]])));
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
                Point([0.0, 0.0]),
                LineString(vec![[0.0, 0.0], [1.0, 1.0]]),
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
        assert_eq!(fc.features[0].geometry, Some(LineString(vec![[0.0, 0.0], [1.0, 1.0]])));
    }

    #[test]
    fn rejects_malformed_coordinates() {
        let kml = "<kml><Placemark><Point><coordinates>not-a-number,0</coordinates></Point></Placemark></kml>";
        assert!(read(kml.as_bytes()).is_err());
    }
}
