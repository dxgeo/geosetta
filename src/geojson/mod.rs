//! Interpret a parsed [`JsonValue`] as GeoJSON.
//!
//! Accepts a `FeatureCollection`, a single `Feature`, or a bare geometry;
//! the latter two are wrapped in a one-element collection so downstream code
//! only ever deals with a list of features.

use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::{Geometry, Position};
use crate::json::{self, JsonValue};

/// Interpret a JSON document as GeoJSON.
pub fn from_json(value: &JsonValue) -> Result<FeatureCollection> {
    let ty = value
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| Error::GeoJson("missing \"type\" member".into()))?;

    match ty {
        "FeatureCollection" => {
            let features = value.get("features").and_then(JsonValue::as_array).ok_or_else(
                || Error::GeoJson("FeatureCollection missing \"features\" array".into()),
            )?;
            let features = features.iter().map(parse_feature).collect::<Result<_>>()?;
            Ok(FeatureCollection { features })
        }
        "Feature" => Ok(FeatureCollection {
            features: vec![parse_feature(value)?],
        }),
        // A bare geometry object.
        _ => Ok(FeatureCollection {
            features: vec![Feature {
                geometry: Some(parse_geometry(value)?),
                properties: Vec::new(),
            }],
        }),
    }
}

// --- serialization ---------------------------------------------------------

/// Serialize a [`FeatureCollection`] straight to GeoJSON text, byte-for-byte
/// identical to [`to_json`] + [`JsonValue::to_json_string`] but without building
/// the intermediate `JsonValue` tree (which allocates a `Vec`/`Object` per
/// coordinate, ring, and feature). This is the writer the CLI uses.
pub fn to_geojson_string(fc: &FeatureCollection) -> String {
    // Rough guess: geometry + a little overhead per feature.
    let mut out = String::with_capacity(fc.features.len() * 96 + 64);
    out.push_str("{\"type\":\"FeatureCollection\",\"features\":[");
    for (i, f) in fc.features.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_feature(&mut out, f);
    }
    out.push_str("]}");
    out
}

fn write_feature(out: &mut String, f: &Feature) {
    out.push_str("{\"type\":\"Feature\",\"geometry\":");
    match &f.geometry {
        Some(g) => write_geometry(out, g),
        None => out.push_str("null"),
    }
    out.push_str(",\"properties\":{");
    for (i, (k, v)) in f.properties.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json::escape_into(k, out);
        out.push(':');
        v.write_json_to(out);
    }
    out.push_str("}}");
}

fn write_geometry(out: &mut String, g: &Geometry) {
    if let Geometry::GeometryCollection(geoms) = g {
        out.push_str("{\"type\":\"GeometryCollection\",\"geometries\":[");
        for (i, sub) in geoms.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_geometry(out, sub);
        }
        out.push_str("]}");
        return;
    }
    out.push_str("{\"type\":\"");
    out.push_str(g.type_name());
    out.push_str("\",\"coordinates\":");
    match g {
        Geometry::Point(p) => write_position(out, *p),
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => write_positions(out, ps),
        Geometry::Polygon(r) | Geometry::MultiLineString(r) => write_rings(out, r),
        Geometry::MultiPolygon(polys) => {
            out.push('[');
            for (i, p) in polys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_rings(out, p);
            }
            out.push(']');
        }
        Geometry::GeometryCollection(_) => unreachable!("handled above"),
    }
    out.push('}');
}

fn write_position(out: &mut String, p: Position) {
    use std::fmt::Write;
    out.push('[');
    let _ = write!(out, "{}", p[0]);
    out.push(',');
    let _ = write!(out, "{}", p[1]);
    out.push(']');
}

fn write_positions(out: &mut String, ps: &[Position]) {
    out.push('[');
    for (i, p) in ps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_position(out, *p);
    }
    out.push(']');
}

fn write_rings(out: &mut String, rings: &[Vec<Position>]) {
    out.push('[');
    for (i, r) in rings.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_positions(out, r);
    }
    out.push(']');
}

/// Render a [`FeatureCollection`] back to a [`JsonValue`] (the inverse of
/// [`from_json`]). Kept as the reference model that [`to_geojson_string`] is
/// tested byte-for-byte against; the CLI writes directly via that function, so
/// this tree builder is only compiled for tests.
#[cfg(test)]
pub fn to_json(fc: &FeatureCollection) -> JsonValue {
    let features = fc.features.iter().map(feature_to_json).collect();
    obj(vec![
        ("type", JsonValue::String("FeatureCollection".into())),
        ("features", JsonValue::Array(features)),
    ])
}

#[cfg(test)]
fn feature_to_json(f: &Feature) -> JsonValue {
    let geometry = match &f.geometry {
        Some(g) => geometry_to_json(g),
        None => JsonValue::Null,
    };
    obj(vec![
        ("type", JsonValue::String("Feature".into())),
        ("geometry", geometry),
        ("properties", JsonValue::Object(f.properties.clone())),
    ])
}

#[cfg(test)]
fn geometry_to_json(g: &Geometry) -> JsonValue {
    if let Geometry::GeometryCollection(geoms) = g {
        let arr = geoms.iter().map(geometry_to_json).collect();
        return obj(vec![
            ("type", JsonValue::String("GeometryCollection".into())),
            ("geometries", JsonValue::Array(arr)),
        ]);
    }
    let coords = match g {
        Geometry::Point(p) => position_to_json(*p),
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => positions_to_json(ps),
        Geometry::Polygon(r) | Geometry::MultiLineString(r) => rings_to_json(r),
        Geometry::MultiPolygon(polys) => {
            JsonValue::Array(polys.iter().map(|p| rings_to_json(p)).collect())
        }
        Geometry::GeometryCollection(_) => unreachable!("handled above"),
    };
    obj(vec![
        ("type", JsonValue::String(g.type_name().into())),
        ("coordinates", coords),
    ])
}

#[cfg(test)]
fn position_to_json(p: Position) -> JsonValue {
    JsonValue::Array(vec![coord(p[0]), coord(p[1])])
}

#[cfg(test)]
fn positions_to_json(ps: &[Position]) -> JsonValue {
    JsonValue::Array(ps.iter().map(|p| position_to_json(*p)).collect())
}

#[cfg(test)]
fn rings_to_json(rings: &[Vec<Position>]) -> JsonValue {
    JsonValue::Array(rings.iter().map(|r| positions_to_json(r)).collect())
}

/// A coordinate ordinate as a (non-integer) JSON number.
#[cfg(test)]
fn coord(v: f64) -> JsonValue {
    JsonValue::Number {
        value: v,
        is_int: false,
    }
}

/// Build an object from `&str` keys, saving `.to_string()` at each call site.
#[cfg(test)]
fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn parse_feature(value: &JsonValue) -> Result<Feature> {
    let geometry = match value.get("geometry") {
        None | Some(JsonValue::Null) => None,
        Some(g) => Some(parse_geometry(g)?),
    };
    let properties = match value.get("properties") {
        Some(JsonValue::Object(members)) => members.clone(),
        _ => Vec::new(),
    };
    Ok(Feature {
        geometry,
        properties,
    })
}

fn parse_geometry(value: &JsonValue) -> Result<Geometry> {
    let ty = value
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| Error::GeoJson("geometry missing \"type\"".into()))?;

    if ty == "GeometryCollection" {
        let geoms = value
            .get("geometries")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| Error::GeoJson("GeometryCollection missing \"geometries\"".into()))?;
        let geoms = geoms.iter().map(parse_geometry).collect::<Result<_>>()?;
        return Ok(Geometry::GeometryCollection(geoms));
    }

    let coords = value
        .get("coordinates")
        .ok_or_else(|| Error::GeoJson(format!("{ty} missing \"coordinates\"")))?;

    match ty {
        "Point" => Ok(Geometry::Point(position(coords)?)),
        "LineString" => Ok(Geometry::LineString(positions(coords)?)),
        "Polygon" => Ok(Geometry::Polygon(rings(coords)?)),
        "MultiPoint" => Ok(Geometry::MultiPoint(positions(coords)?)),
        "MultiLineString" => Ok(Geometry::MultiLineString(rings(coords)?)),
        "MultiPolygon" => Ok(Geometry::MultiPolygon(polygons(coords)?)),
        other => Err(Error::GeoJson(format!("unknown geometry type \"{other}\""))),
    }
}

/// A single `[x, y]` (extra ordinates ignored).
fn position(value: &JsonValue) -> Result<Position> {
    let arr = value
        .as_array()
        .ok_or_else(|| Error::GeoJson("coordinate is not an array".into()))?;
    if arr.len() < 2 {
        return Err(Error::GeoJson("coordinate needs at least 2 numbers".into()));
    }
    let x = arr[0]
        .as_f64()
        .ok_or_else(|| Error::GeoJson("coordinate x is not a number".into()))?;
    let y = arr[1]
        .as_f64()
        .ok_or_else(|| Error::GeoJson("coordinate y is not a number".into()))?;
    Ok([x, y])
}

/// An array of positions.
fn positions(value: &JsonValue) -> Result<Vec<Position>> {
    array(value)?.iter().map(position).collect()
}

/// An array of position-arrays (polygon rings / multi-linestring lines).
fn rings(value: &JsonValue) -> Result<Vec<Vec<Position>>> {
    array(value)?.iter().map(positions).collect()
}

/// An array of ring-arrays (multi-polygon).
fn polygons(value: &JsonValue) -> Result<Vec<Vec<Vec<Position>>>> {
    array(value)?.iter().map(rings).collect()
}

fn array(value: &JsonValue) -> Result<&[JsonValue]> {
    value
        .as_array()
        .ok_or_else(|| Error::GeoJson("expected a coordinate array".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn parses_feature_collection() {
        let doc = r#"{
            "type": "FeatureCollection",
            "features": [
                {"type": "Feature",
                 "geometry": {"type": "Point", "coordinates": [1.0, 2.0]},
                 "properties": {"name": "a", "n": 3}}
            ]
        }"#;
        let fc = from_json(&parse(doc).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 1);
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point([1.0, 2.0])));
        assert_eq!(fc.features[0].properties[0].0, "name");
    }

    #[test]
    fn wraps_bare_geometry_and_feature() {
        let g = from_json(&parse(r#"{"type":"Point","coordinates":[0,0]}"#).unwrap()).unwrap();
        assert_eq!(g.features.len(), 1);
        let f = from_json(
            &parse(r#"{"type":"Feature","geometry":null,"properties":null}"#).unwrap(),
        )
        .unwrap();
        assert!(f.features[0].geometry.is_none());
        assert!(f.features[0].properties.is_empty());
    }

    #[test]
    fn direct_writer_matches_tree_serialization() {
        // to_geojson_string must be byte-for-byte identical to the JsonValue
        // path across every geometry type, null geometry, and typed/nested
        // properties (so switching the CLI to it changes nothing observable).
        let doc = r#"{
            "type": "FeatureCollection",
            "features": [
                {"type":"Feature","geometry":{"type":"Point","coordinates":[-73.9857,40.7484]},
                 "properties":{"name":"Café ☕","h":381,"ok":true,"r":4.7,"tags":["a","b"],"z":null}},
                {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1.5,2.25]]},
                 "properties":{}},
                {"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]},
                 "properties":{"n":12}},
                {"type":"Feature","geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]},
                 "properties":{}},
                {"type":"Feature","geometry":null,"properties":{"only":"props"}}
            ]
        }"#;
        let fc = from_json(&parse(doc).unwrap()).unwrap();
        assert_eq!(to_geojson_string(&fc), to_json(&fc).to_json_string());
    }

    #[test]
    fn parses_polygon_rings() {
        let doc = r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}"#;
        let fc = from_json(&parse(doc).unwrap()).unwrap();
        match &fc.features[0].geometry {
            Some(Geometry::Polygon(rings)) => {
                assert_eq!(rings.len(), 1);
                assert_eq!(rings[0].len(), 4);
            }
            _ => panic!(),
        }
    }
}
