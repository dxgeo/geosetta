//! Interpret a parsed [`JsonValue`] as GeoJSON.
//!
//! Accepts a `FeatureCollection`, a single `Feature`, or a bare geometry;
//! the latter two are wrapped in a one-element collection so downstream code
//! only ever deals with a list of features.

use crate::error::{Error, Result};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;

/// A GeoJSON feature: an optional geometry plus ordered properties.
#[derive(Debug, Clone)]
pub struct Feature {
    pub geometry: Option<Geometry>,
    /// Property members, in document order. Empty when `properties` is null.
    pub properties: Vec<(String, JsonValue)>,
}

/// A collection of features.
#[derive(Debug, Clone)]
pub struct FeatureCollection {
    pub features: Vec<Feature>,
}

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

/// Render a [`FeatureCollection`] back to a [`JsonValue`] (the inverse of
/// [`from_json`]); stringify with [`JsonValue::to_json_string`].
pub fn to_json(fc: &FeatureCollection) -> JsonValue {
    let features = fc.features.iter().map(feature_to_json).collect();
    obj(vec![
        ("type", JsonValue::String("FeatureCollection".into())),
        ("features", JsonValue::Array(features)),
    ])
}

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

fn position_to_json(p: Position) -> JsonValue {
    JsonValue::Array(vec![coord(p[0]), coord(p[1])])
}

fn positions_to_json(ps: &[Position]) -> JsonValue {
    JsonValue::Array(ps.iter().map(|p| position_to_json(*p)).collect())
}

fn rings_to_json(rings: &[Vec<Position>]) -> JsonValue {
    JsonValue::Array(rings.iter().map(|r| positions_to_json(r)).collect())
}

/// A coordinate ordinate as a (non-integer) JSON number.
fn coord(v: f64) -> JsonValue {
    JsonValue::Number {
        value: v,
        is_int: false,
    }
}

/// Build an object from `&str` keys, saving `.to_string()` at each call site.
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
