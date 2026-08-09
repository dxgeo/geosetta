//! Interpret a parsed [`JsonValue`] as GeoJSON.
//!
//! Accepts a `FeatureCollection`, a single `Feature`, or a bare geometry;
//! the latter two are wrapped in a one-element collection so downstream code
//! only ever deals with a list of features.

use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::{Geometry, Position};
use crate::json::{self, JsonValue, Parser};
use std::collections::HashSet;
use std::rc::Rc;

/// Interns property keys within one collection so a name repeated across
/// features is allocated once and shared as an `Rc`, not re-allocated per
/// feature. Lookups borrow the key as `&str` (via `Rc<str>: Borrow<str>`), so a
/// hit costs only a refcount bump.
#[derive(Default)]
struct KeyInterner {
    seen: HashSet<Rc<str>>,
}

impl KeyInterner {
    fn intern(&mut self, key: &str) -> Rc<str> {
        if let Some(rc) = self.seen.get(key) {
            return Rc::clone(rc);
        }
        let rc: Rc<str> = Rc::from(key);
        self.seen.insert(Rc::clone(&rc));
        rc
    }
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
            let mut keys = KeyInterner::default();
            let features = features
                .iter()
                .map(|f| parse_feature(f, &mut keys))
                .collect::<Result<_>>()?;
            Ok(wgs84(features))
        }
        "Feature" => Ok(wgs84(vec![parse_feature(
            value,
            &mut KeyInterner::default(),
        )?])),
        // A bare geometry object.
        _ => Ok(wgs84(vec![Feature {
            geometry: Some(parse_geometry(value)?),
            properties: Vec::new(),
        }])),
    }
}

/// Wrap features as a collection in WGS 84 (OGC:CRS84). GeoJSON coordinates are
/// always WGS 84 longitude/latitude per RFC 7946; a file carries no other CRS,
/// so reading one always yields the spec default.
fn wgs84(features: Vec<Feature>) -> FeatureCollection {
    FeatureCollection {
        features,
        crs: Some(crate::crs::Crs::Wgs84),
    }
}

// --- streaming reader ------------------------------------------------------

/// Parse GeoJSON text straight into the [`FeatureCollection`] IR, without first
/// building a [`JsonValue`] tree (which allocates an `Array`/`Object` per
/// coordinate, ring, and feature). This handles the common, hot case — a
/// `FeatureCollection` whose `features` array we stream feature-by-feature —
/// and falls back to [`from_json`] over the parsed tree for the rare and tiny
/// single-`Feature` / bare-geometry documents. Output is identical to
/// `from_json(&parse(input))`.
pub fn from_geojson_str(input: &str) -> Result<FeatureCollection> {
    let mut p = Parser::new(input);
    p.skip_ws();
    // Only an object can be a FeatureCollection; anything else (array, scalar,
    // garbage) goes to the tree path so its errors match exactly.
    if p.peek() != Some(b'{') {
        return from_json(&crate::json::parse(input)?);
    }
    match stream_collection(&mut p)? {
        Some(fc) => Ok(fc),
        // Well-formed object but no `features` member: a single Feature or a
        // bare geometry — small, so the tree path handles it.
        None => from_json(&crate::json::parse(input)?),
    }
}

/// Stream the top-level object, returning its features if it is a
/// `FeatureCollection` (has a `features` array), or `None` if it is some other
/// well-formed object. Other members are parsed and discarded so malformed
/// input still errors exactly as the tree parser would.
fn stream_collection(p: &mut Parser) -> Result<Option<FeatureCollection>> {
    p.expect(b'{')?;
    let mut features: Option<Vec<Feature>> = None;
    p.skip_ws();
    if p.peek() == Some(b'}') {
        p.bump();
    } else {
        loop {
            p.skip_ws();
            let key = p.parse_string()?;
            p.skip_ws();
            p.expect(b':')?;
            if key == "features" && features.is_none() {
                features = Some(stream_features(p)?);
            } else {
                p.skip_value()?;
            }
            p.skip_ws();
            match p.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(p.err("expected ',' or '}'")),
            }
        }
    }
    match features {
        Some(f) => {
            if !p.at_end() {
                return Err(p.err("unexpected trailing characters"));
            }
            Ok(Some(wgs84(f)))
        }
        None => Ok(None),
    }
}

/// Stream a JSON array of Feature objects.
fn stream_features(p: &mut Parser) -> Result<Vec<Feature>> {
    p.skip_ws();
    p.expect(b'[')?;
    let mut features = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b']') {
        p.bump();
        return Ok(features);
    }
    let mut keys = KeyInterner::default();
    loop {
        features.push(stream_feature(p, &mut keys)?);
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b']') => break,
            _ => return Err(p.err("expected ',' or ']' in features")),
        }
    }
    Ok(features)
}

/// Stream one Feature object into a [`Feature`], dispatching on member key so
/// member order doesn't matter. `geometry` is parsed typed; `properties` stay
/// arbitrary JSON; every other member (`type`, `id`, `bbox`, …) is skipped.
fn stream_feature(p: &mut Parser, keys: &mut KeyInterner) -> Result<Feature> {
    p.skip_ws();
    p.expect(b'{')?;
    let mut geometry = None;
    let mut properties: Vec<(Rc<str>, JsonValue)> = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b'}') {
        p.bump();
        return Ok(Feature { geometry, properties });
    }
    loop {
        p.skip_ws();
        let key = p.parse_string()?;
        p.skip_ws();
        p.expect(b':')?;
        match key.as_str() {
            "geometry" => {
                p.skip_ws();
                if p.peek() == Some(b'n') {
                    p.skip_value()?; // null geometry
                } else {
                    geometry = Some(stream_geometry(p)?);
                }
            }
            // Properties are arbitrary JSON; a non-object (incl. null) means none.
            "properties" => {
                if let JsonValue::Object(m) = p.parse_value()? {
                    properties = m
                        .into_iter()
                        .map(|(k, v)| (keys.intern(&k), v))
                        .collect();
                }
            }
            _ => p.skip_value()?,
        }
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b'}') => break,
            _ => return Err(p.err("expected ',' or '}' in feature")),
        }
    }
    Ok(Feature { geometry, properties })
}

/// Stream one geometry object. `coordinates` are parsed straight into typed
/// vectors when the type is already known (the universal case); if they appear
/// before `type`, they are buffered as a `JsonValue` and reshaped once the type
/// is known.
fn stream_geometry(p: &mut Parser) -> Result<Geometry> {
    p.skip_ws();
    p.expect(b'{')?;
    let mut ty: Option<String> = None;
    let mut geom: Option<Geometry> = None;
    let mut deferred: Option<JsonValue> = None;
    let mut geoms: Option<Vec<Geometry>> = None;
    p.skip_ws();
    if p.peek() == Some(b'}') {
        p.bump();
        return Err(p.err("geometry missing \"type\""));
    }
    loop {
        p.skip_ws();
        let key = p.parse_string()?;
        p.skip_ws();
        p.expect(b':')?;
        match key.as_str() {
            "type" => {
                p.skip_ws();
                ty = Some(p.parse_string()?);
            }
            "coordinates" => match &ty {
                Some(t) => geom = Some(stream_coordinates(p, t)?),
                None => deferred = Some(p.parse_value()?),
            },
            "geometries" => geoms = Some(stream_geometry_array(p)?),
            _ => p.skip_value()?,
        }
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b'}') => break,
            _ => return Err(p.err("expected ',' or '}' in geometry")),
        }
    }
    let ty = ty.ok_or_else(|| p.err("geometry missing \"type\""))?;
    if ty == "GeometryCollection" {
        let geoms = geoms.ok_or_else(|| Error::GeoJson("GeometryCollection missing \"geometries\"".into()))?;
        return Ok(Geometry::GeometryCollection(geoms));
    }
    if let Some(g) = geom {
        return Ok(g);
    }
    if let Some(coords) = deferred {
        return geometry_from_coords(&ty, &coords);
    }
    Err(Error::GeoJson(format!("{ty} missing \"coordinates\"")))
}

fn stream_geometry_array(p: &mut Parser) -> Result<Vec<Geometry>> {
    p.skip_ws();
    p.expect(b'[')?;
    let mut out = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b']') {
        p.bump();
        return Ok(out);
    }
    loop {
        out.push(stream_geometry(p)?);
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b']') => break,
            _ => return Err(p.err("expected ',' or ']' in geometries")),
        }
    }
    Ok(out)
}

/// Parse a `coordinates` value into the shape implied by `ty`.
fn stream_coordinates(p: &mut Parser, ty: &str) -> Result<Geometry> {
    p.skip_ws();
    match ty {
        "Point" => Ok(Geometry::Point(stream_position(p)?)),
        "LineString" => Ok(Geometry::LineString(stream_positions(p)?)),
        "MultiPoint" => Ok(Geometry::MultiPoint(stream_positions(p)?)),
        "Polygon" => Ok(Geometry::Polygon(stream_rings(p)?)),
        "MultiLineString" => Ok(Geometry::MultiLineString(stream_rings(p)?)),
        "MultiPolygon" => Ok(Geometry::MultiPolygon(stream_polygons(p)?)),
        other => Err(Error::GeoJson(format!("unknown geometry type \"{other}\""))),
    }
}

/// A single `[x, y]` position; extra ordinates (z/m) are skipped, matching the
/// tree reader.
fn stream_position(p: &mut Parser) -> Result<Position> {
    p.skip_ws();
    p.expect(b'[')?;
    p.skip_ws();
    let x = p.parse_f64()?;
    p.skip_ws();
    p.expect(b',')?;
    p.skip_ws();
    let y = p.parse_f64()?;
    loop {
        p.skip_ws();
        match p.bump() {
            Some(b']') => break,
            Some(b',') => {
                p.skip_ws();
                p.skip_value()?; // extra ordinate (z/m), ignored
            }
            _ => return Err(p.err("expected ',' or ']' in coordinate")),
        }
    }
    Ok([x, y])
}

fn stream_positions(p: &mut Parser) -> Result<Vec<Position>> {
    stream_array(p, stream_position)
}

fn stream_rings(p: &mut Parser) -> Result<Vec<Vec<Position>>> {
    stream_array(p, stream_positions)
}

fn stream_polygons(p: &mut Parser) -> Result<Vec<Vec<Vec<Position>>>> {
    stream_array(p, stream_rings)
}

/// Parse a JSON array, applying `elem` to each element.
fn stream_array<T>(p: &mut Parser, elem: fn(&mut Parser) -> Result<T>) -> Result<Vec<T>> {
    p.skip_ws();
    p.expect(b'[')?;
    let mut out = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b']') {
        p.bump();
        return Ok(out);
    }
    loop {
        out.push(elem(p)?);
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b']') => break,
            _ => return Err(p.err("expected ',' or ']' in array")),
        }
    }
    Ok(out)
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
        (
            "properties",
            JsonValue::Object(
                f.properties
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ),
        ),
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

fn parse_feature(value: &JsonValue, keys: &mut KeyInterner) -> Result<Feature> {
    let geometry = match value.get("geometry") {
        None | Some(JsonValue::Null) => None,
        Some(g) => Some(parse_geometry(g)?),
    };
    let properties = match value.get("properties") {
        Some(JsonValue::Object(members)) => members
            .iter()
            .map(|(k, v)| (keys.intern(k), v.clone()))
            .collect(),
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
    geometry_from_coords(ty, coords)
}

/// Build a (non-collection) geometry from its type name and a `coordinates`
/// [`JsonValue`]. Shared by the tree reader and the streaming reader's
/// out-of-order fallback (coordinates seen before type).
fn geometry_from_coords(ty: &str, coords: &JsonValue) -> Result<Geometry> {
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
        assert_eq!(fc.features[0].properties[0].0.as_ref(), "name");
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
    fn streaming_reader_matches_tree_reader() {
        // from_geojson_str must produce exactly what from_json(parse(..)) does,
        // across geometry types, null geometry, nested/typed properties, extra
        // (z) ordinates, out-of-order members (coordinates before type,
        // geometry before type), and a GeometryCollection.
        let docs = [
            r#"{"type":"FeatureCollection","features":[
                {"type":"Feature","geometry":{"type":"Point","coordinates":[-73.9857,40.7484,12.5]},
                 "properties":{"name":"Café ☕","h":381,"ok":true,"r":4.7,"tags":["a",{"b":1}],"z":null}},
                {"type":"Feature","geometry":{"coordinates":[[0,0],[1.5,2.25]],"type":"LineString"},
                 "properties":null},
                {"type":"Feature","properties":{"only":"props"},"geometry":null},
                {"geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]},"type":"Feature","id":7},
                {"type":"Feature","geometry":{"type":"GeometryCollection","geometries":[
                    {"type":"Point","coordinates":[1,2]},
                    {"type":"MultiPoint","coordinates":[[3,4],[5,6]]}]},"properties":{}}
            ],"bbox":[0,0,1,1]}"#,
            // Single Feature and bare geometry (the tree-fallback paths).
            r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[1,2]},"properties":{"a":1}}"#,
            r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}"#,
            r#"{"type":"FeatureCollection","features":[]}"#,
            // Pretty-printed, with whitespace after every colon and comma.
            "{\n  \"type\": \"FeatureCollection\",\n  \"features\": [\n    {\n      \"type\": \"Feature\",\n      \"geometry\": { \"type\": \"Point\", \"coordinates\": [ 1.0, 2.0 ] },\n      \"properties\": { \"a\": 1 }\n    }\n  ]\n}",
        ];
        for doc in docs {
            let streamed = from_geojson_str(doc).unwrap();
            let treed = from_json(&parse(doc).unwrap()).unwrap();
            assert_eq!(streamed, treed, "mismatch for: {doc}");
        }
    }

    #[test]
    fn streaming_reader_rejects_malformed() {
        // Trailing junk and truncation must still error (matching the tree path).
        assert!(from_geojson_str(r#"{"type":"FeatureCollection","features":[]} garbage"#).is_err());
        assert!(from_geojson_str(r#"{"type":"FeatureCollection","features":[{"type":"Feature""#).is_err());
        assert!(from_geojson_str(r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[1]}}]}"#).is_err());
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
