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
