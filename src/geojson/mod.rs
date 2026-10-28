//! Interpret a parsed [`JsonValue`] as GeoJSON.
//!
//! Accepts a `FeatureCollection`, a single `Feature`, or a bare geometry;
//! the latter two are wrapped in a one-element collection so downstream code
//! only ever deals with a list of features.

use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::format::Format;
use crate::geometry::{Geometry, Position};
use crate::json::{self, JsonValue, Parser};
use crate::native::{Native, NativePayload};
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
            native: None,
        }])),
    }
}

/// Wrap features as a collection in WGS 84 (OGC:CRS84). GeoJSON coordinates are
/// always WGS 84 longitude/latitude per RFC 7946; a file carries no other CRS,
/// so reading one always yields the spec default.
fn wgs84(features: Vec<Feature>) -> FeatureCollection {
    wgs84_with(features, None, Vec::new())
}

/// [`wgs84`], plus the collection's own `name` and whatever else the reader
/// skipped at collection level.
///
/// `name` is modelled ([`FeatureCollection::name`]); the rest becomes a
/// [`Native`] tagged `GeoJson`, so only the GeoJSON writer replays it.
fn wgs84_with(
    features: Vec<Feature>,
    name: Option<String>,
    unmodelled: Vec<(Rc<str>, JsonValue)>,
) -> FeatureCollection {
    FeatureCollection {
        features,
        crs: Some(crate::crs::Crs::Wgs84),
        name,
        schema: Vec::new(),
        native: native_from(unmodelled),
    }
}

/// Tag skipped members as a GeoJSON native, or `None` when there were none —
/// the common case, which must not allocate.
fn native_from(unmodelled: Vec<(Rc<str>, JsonValue)>) -> Option<Native> {
    (!unmodelled.is_empty())
        .then(|| Native::new(Format::GeoJson, NativePayload::GeoJson(unmodelled)))
}

/// Whether a skipped member is worth stashing for replay.
///
/// `type` is structural — the writer re-emits it unconditionally, so replaying
/// it would duplicate a key. `bbox` is *derived*: `plans/same-format-fidelity.org`
/// D4 forbids replaying derived data, because a middle stage that edits geometry
/// leaves a stale extent behind, and a stale extent is worse than none (GDAL
/// trusts a cached one on the spatial-filter path). Recomputing it on write is
/// follow-on work; dropping it here is deliberate, not an oversight.
fn is_replayable(key: &str) -> bool {
    !matches!(key, "type" | "bbox" | "geometry" | "properties" | "features")
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
    let mut name: Option<String> = None;
    let mut unmodelled: Vec<(Rc<str>, JsonValue)> = Vec::new();
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
            } else if key == "name" && name.is_none() {
                // Modelled, not stashed: several formats have a first-class
                // slot for a collection's name (see `FeatureCollection::name`).
                name = match p.parse_value()? {
                    JsonValue::String(s) => Some(s),
                    // A non-string `name` is not the member RFC 7946 tooling
                    // writes; keep it verbatim rather than coercing it.
                    other => {
                        unmodelled.push((Rc::from(key.as_str()), other));
                        None
                    }
                };
            } else if is_replayable(&key) {
                unmodelled.push((Rc::from(key.as_str()), p.parse_value()?));
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
            Ok(Some(wgs84_with(f, name, unmodelled)))
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
    let mut unmodelled: Vec<(Rc<str>, JsonValue)> = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b'}') {
        p.bump();
        return Ok(Feature { geometry, properties, native: None });
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
            // `id` and any foreign member are kept verbatim so a GeoJSON →
            // GeoJSON run can put them back; `type`/`bbox` are filtered by
            // `is_replayable`.
            k if is_replayable(k) => unmodelled.push((keys.intern(k), p.parse_value()?)),
            _ => p.skip_value()?,
        }
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b'}') => break,
            _ => return Err(p.err("expected ',' or '}' in feature")),
        }
    }
    Ok(Feature { geometry, properties, native: native_from(unmodelled) })
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

/// A single `[x, y, z?]` position, matching the tree reader: a third
/// ordinate is read as Z; a fourth (M — RFC 7946 gives it no defined
/// meaning) is parsed only far enough to skip it.
fn stream_position(p: &mut Parser) -> Result<Position> {
    p.skip_ws();
    p.expect(b'[')?;
    p.skip_ws();
    let x = p.parse_f64()?;
    p.skip_ws();
    p.expect(b',')?;
    p.skip_ws();
    let y = p.parse_f64()?;
    let mut z = None;
    loop {
        p.skip_ws();
        match p.bump() {
            Some(b']') => break,
            Some(b',') => {
                p.skip_ws();
                if z.is_none() {
                    z = Some(p.parse_f64()?);
                } else {
                    p.skip_value()?; // a fourth+ ordinate (M or beyond), ignored
                }
            }
            _ => return Err(p.err("expected ',' or ']' in coordinate")),
        }
    }
    Ok(match z {
        Some(z) => Position::with_z(x, y, z),
        None => Position::new(x, y),
    })
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

/// Serialize a [`FeatureCollection`] straight to GeoJSON text without
/// building the intermediate `JsonValue` tree (which allocates a
/// `Vec`/`Object` per coordinate, ring, and feature). This is the writer the
/// CLI uses.
///
/// *Laid out by default* (`plans/kml-layout.org` D-L8): the collection, each
/// feature and each geometry one member per line, indented two spaces, and
/// every value inside them — coordinates, a property, a foreign member —
/// compact on its line, so a large polygon is one line rather than one per
/// number. `compact` writes it all on one line, byte-for-byte identical to
/// [`to_json`] + [`JsonValue::to_json_string`].
pub fn to_geojson_string(fc: &FeatureCollection, compact: bool) -> String {
    // Rough guess: geometry + a little overhead per feature.
    let mut w = Writer { out: String::with_capacity(fc.features.len() * 96 + 64), compact, depth: 0 };
    w.open('{');
    w.member("type");
    w.out.push_str("\"FeatureCollection\"");
    if let Some(name) = &fc.name {
        w.member("name");
        json::escape_into(name, &mut w.out);
    }
    w.natives(fc.native.as_ref());
    w.member("features");
    w.open('[');
    for f in &fc.features {
        w.item();
        w.feature(f);
    }
    w.close(']');
    w.close('}');
    if !compact {
        w.out.push('\n');
    }
    w.out
}

/// The GeoJSON writer's output and where it is in the layout.
struct Writer {
    out: String,
    compact: bool,
    depth: usize,
}

impl Writer {
    fn open(&mut self, bracket: char) {
        self.out.push(bracket);
        self.depth += 1;
    }

    /// Close an object or array: on a line of its own when laid out and it
    /// holds anything.
    fn close(&mut self, bracket: char) {
        self.depth -= 1;
        let empty = self.out.ends_with(['{', '[']);
        if !self.compact && !empty {
            self.newline();
        }
        self.out.push(bracket);
    }

    fn newline(&mut self) {
        self.out.push('\n');
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
    }

    /// Start the next member or element: after a comma unless it is the
    /// first, and on a line of its own when laid out.
    fn item(&mut self) {
        if !self.out.ends_with(['{', '[']) {
            self.out.push(',');
        }
        if !self.compact {
            self.newline();
        }
    }

    /// Start a member: its key, and the colon.
    fn member(&mut self, key: &str) {
        self.item();
        json::escape_into(key, &mut self.out);
        self.out.push_str(if self.compact { ":" } else { ": " });
    }

    /// Re-emit the members a GeoJSON source carried that the IR does not
    /// model.
    ///
    /// Replays only when the native was produced by *this* format
    /// ([`Native::payload_for`]) — a FlatGeobuf-tagged payload is ignored
    /// here, and that is an ordinary cross-format conversion rather than an
    /// error. Emits nothing at all when there is no native, which is the
    /// common case.
    fn natives(&mut self, native: Option<&Native>) {
        let Some(NativePayload::GeoJson(members)) = native.and_then(|n| n.payload_for(Format::GeoJson)) else {
            return;
        };
        for (k, v) in members {
            self.member(k);
            v.write_json_to(&mut self.out);
        }
    }

    fn feature(&mut self, f: &Feature) {
        self.open('{');
        self.member("type");
        self.out.push_str("\"Feature\"");
        self.natives(f.native.as_ref());
        self.member("geometry");
        match &f.geometry {
            Some(g) => self.geometry(g),
            None => self.out.push_str("null"),
        }
        self.member("properties");
        self.open('{');
        for (k, v) in &f.properties {
            self.member(k);
            v.write_json_to(&mut self.out);
        }
        self.close('}');
        self.close('}');
    }

    fn geometry(&mut self, g: &Geometry) {
        self.open('{');
        self.member("type");
        self.out.push('"');
        self.out.push_str(g.type_name());
        self.out.push('"');
        if let Geometry::GeometryCollection(geoms) = g {
            self.member("geometries");
            self.open('[');
            for sub in geoms {
                self.item();
                self.geometry(sub);
            }
            self.close(']');
        } else {
            self.member("coordinates");
            write_coordinates(&mut self.out, g);
        }
        self.close('}');
    }
}

/// A geometry's `coordinates` value, compact.
fn write_coordinates(out: &mut String, g: &Geometry) {
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
        Geometry::GeometryCollection(_) => unreachable!("a collection has geometries, not coordinates"),
    }
}

fn write_position(out: &mut String, p: Position) {
    // Through `write_json_f64`, not `write!`, so this agrees with the tree
    // serializer in `json::value` — `direct_writer_matches_tree_serialization`
    // is the test that holds them together, and it caught them diverging the
    // moment whole floats started keeping their `.0`.
    out.push('[');
    crate::json::write_json_f64(p.x, out);
    out.push(',');
    crate::json::write_json_f64(p.y, out);
    if let Some(z) = p.z {
        out.push(',');
        crate::json::write_json_f64(z, out);
    }
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
    let mut members = vec![("type", JsonValue::String("FeatureCollection".into()))];
    if let Some(name) = &fc.name {
        members.push(("name", JsonValue::String(name.clone())));
    }
    let mut members = obj_members(members, fc.native.as_ref());
    members.push(("features".to_string(), JsonValue::Array(features)));
    JsonValue::Object(members)
}

/// Shared by both tree builders: turn `&str`-keyed members into owned ones and
/// append the replayed natives, so member *order* matches
/// [`to_geojson_string`] exactly (natives sit after `type`/`name` and before
/// `features`/`geometry`).
#[cfg(test)]
fn obj_members(
    base: Vec<(&str, JsonValue)>,
    native: Option<&Native>,
) -> Vec<(String, JsonValue)> {
    let mut out: Vec<(String, JsonValue)> =
        base.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    if let Some(NativePayload::GeoJson(members)) =
        native.and_then(|n| n.payload_for(Format::GeoJson))
    {
        out.extend(members.iter().map(|(k, v)| (k.to_string(), v.clone())));
    }
    out
}

#[cfg(test)]
fn feature_to_json(f: &Feature) -> JsonValue {
    let geometry = match &f.geometry {
        Some(g) => geometry_to_json(g),
        None => JsonValue::Null,
    };
    let mut members = obj_members(
        vec![("type", JsonValue::String("Feature".into()))],
        f.native.as_ref(),
    );
    members.push(("geometry".to_string(), geometry));
    members.push((
        "properties".to_string(),
        JsonValue::Object(
            f.properties
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ),
    ));
    JsonValue::Object(members)
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
    let mut v = vec![coord(p.x), coord(p.y)];
    if let Some(z) = p.z {
        v.push(coord(z));
    }
    JsonValue::Array(v)
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
    JsonValue::Float(v)
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
    // Must match `stream_feature`'s stash exactly: the two paths are required
    // to produce identical output (see `from_geojson_str`).
    let unmodelled = match value {
        JsonValue::Object(members) => members
            .iter()
            .filter(|(k, _)| is_replayable(k))
            .map(|(k, v)| (keys.intern(k), v.clone()))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Feature {
        geometry,
        properties,
        native: native_from(unmodelled),
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

/// A single `[x, y, z?]` position; a fourth+ ordinate (M — RFC 7946 gives it
/// no defined meaning) is ignored, matching the streaming reader.
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
    match arr.get(2) {
        None => Ok(Position::new(x, y)),
        Some(z) => {
            let z = z.as_f64().ok_or_else(|| Error::GeoJson("coordinate z is not a number".into()))?;
            Ok(Position::with_z(x, y, z))
        }
    }
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

    /// The round-trip invariant for this spoke: read a hostile fixture, write
    /// the same format, and get every unmodelled member back.
    ///
    /// The fixture is `tests/fixtures/hand_hostile.geojson`, described in
    /// `tests/fixtures/HOSTILE.org`; it is inlined here rather than read from
    /// disk so this stays a unit test. Keep the two in step.
    const HOSTILE: &str = r#"{"type":"FeatureCollection",
 "name":"hostile",
 "vendor:extension":{"produced_by":"hand","v":2},
 "features":[
  {"type":"Feature","id":42,"vendor:featureNote":"keep-me",
   "geometry":{"type":"Point","coordinates":[12.5683,55.6761]},
   "properties":{"zip":"007","note":null}},
  {"type":"Feature","id":"a-string-id",
   "geometry":{"type":"Point","coordinates":[8.5417,47.3769]},
   "properties":{"zip":"08001"}}]}"#;

    #[test]
    fn same_format_round_trip_keeps_unmodelled_members() {
        let fc = from_geojson_str(HOSTILE).unwrap();

        // Modelled, not stashed.
        assert_eq!(fc.name.as_deref(), Some("hostile"));

        let out = to_geojson_string(&fc, true);
        // Collection-level foreign member.
        assert!(out.contains(r#""vendor:extension":{"produced_by":"hand","v":2}"#), "{out}");
        // Feature `id`, in both of the forms GeoJSON permits.
        assert!(out.contains(r#""id":42"#), "{out}");
        assert!(out.contains(r#""id":"a-string-id""#), "{out}");
        // Feature-level foreign member.
        assert!(out.contains(r#""vendor:featureNote":"keep-me""#), "{out}");
        // And the modelled name is re-emitted.
        assert!(out.contains(r#""name":"hostile""#), "{out}");

        // The real assertion: reading the output back yields an equal IR.
        assert_eq!(from_geojson_str(&out).unwrap(), fc);
    }

    /// A native must never leak into a format that did not produce it. This is
    /// the safety property the format tag exists for.
    #[test]
    fn a_foreign_native_is_ignored_rather_than_replayed() {
        let mut fc = from_geojson_str(HOSTILE).unwrap();
        let with_natives = to_geojson_string(&fc, true);

        // Re-tag every native as if some other spoke had produced it.
        let retag = |n: &mut Option<crate::native::Native>| {
            if let Some(n) = n {
                n.format = Format::FlatGeobuf;
            }
        };
        retag(&mut fc.native);
        for f in &mut fc.features {
            retag(&mut f.native);
        }
        let ignored = to_geojson_string(&fc, true);

        assert_ne!(with_natives, ignored);
        assert!(!ignored.contains("vendor:"), "{ignored}");
        assert!(!ignored.contains(r#""id":"#), "{ignored}");
        // `name` is modelled, not a native, so it is unaffected by the tag.
        assert!(ignored.contains(r#""name":"hostile""#), "{ignored}");
    }

    /// `None` in the common case (D3): a collection with nothing unmodelled
    /// must not allocate a payload, on any feature or on itself.
    #[test]
    fn a_plain_collection_carries_no_native() {
        let plain = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1,2]},
             "properties":{"a":1}}]}"#;
        let fc = from_geojson_str(plain).unwrap();
        assert!(fc.native.is_none());
        assert!(fc.name.is_none());
        assert!(fc.features.iter().all(|f| f.native.is_none()));
    }

    /// `bbox` is derived, so D4 forbids replaying it — a middle stage that
    /// edits geometry would leave a stale extent behind. It is dropped on
    /// purpose; recomputing it on write is separate work.
    #[test]
    fn bbox_is_not_stashed_because_it_is_derived() {
        let with_bbox = r#"{"type":"FeatureCollection","bbox":[0,0,1,1],"features":[
            {"type":"Feature","bbox":[0,0,1,1],
             "geometry":{"type":"Point","coordinates":[1,2]},"properties":{}}]}"#;
        let fc = from_geojson_str(with_bbox).unwrap();
        assert!(fc.native.is_none(), "collection bbox was stashed");
        assert!(fc.features[0].native.is_none(), "feature bbox was stashed");
        assert!(!to_geojson_string(&fc, true).contains("bbox"));
    }
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
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::new(1.0, 2.0))));
        assert_eq!(fc.features[0].properties[0].0.as_ref(), "name");
    }

    #[test]
    fn parses_and_writes_a_z_bearing_point() {
        let doc = r#"{"type":"Point","coordinates":[1.0,2.0,3.0]}"#;
        let fc = from_json(&parse(doc).unwrap()).unwrap();
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::with_z(1.0, 2.0, 3.0))));

        // Streaming path agrees.
        let streamed = from_geojson_str(doc).unwrap();
        assert_eq!(streamed, fc);

        // Round trip through the real writer: Z survives, re-parses back the same.
        let written = to_geojson_string(&fc, true);
        let back = from_json(&parse(&written).unwrap()).unwrap();
        assert_eq!(back, fc);
        assert!(written.contains("[1,2,3]") || written.contains("[1.0,2.0,3.0]"), "unexpected coordinates rendering: {written}");
    }

    #[test]
    fn a_bare_xy_point_writes_with_no_third_element() {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![],
            native: None,
        }]);
        let written = to_geojson_string(&fc, true);
        assert!(written.contains("\"coordinates\":[1,2]}") || written.contains("\"coordinates\":[1.0,2.0]}"));
    }

    #[test]
    fn fourth_ordinate_m_is_parsed_past_but_not_kept() {
        // RFC 7946 gives a 4th coordinate no defined meaning; both readers
        // must still accept it (real producers emit it) without keeping it.
        let doc = r#"{"type":"Point","coordinates":[1.0,2.0,3.0,4.0]}"#;
        let fc = from_json(&parse(doc).unwrap()).unwrap();
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::with_z(1.0, 2.0, 3.0))));
        let streamed = from_geojson_str(doc).unwrap();
        assert_eq!(streamed, fc);
    }

    #[test]
    fn rejects_a_non_numeric_z() {
        let doc = r#"{"type":"Point","coordinates":[1.0,2.0,"bad"]}"#;
        assert!(from_json(&parse(doc).unwrap()).is_err());
        assert!(from_geojson_str(doc).is_err());
    }

    #[test]
    fn z_bearing_geojson_round_trips_through_geoparquet() {
        // The real payoff of M2 (the WKB codec) + M3 (this GeoJSON change):
        // GeoJSON -> GeoParquet -> GeoJSON now carries Z all the way through,
        // reachable via the actual conversion pipeline, not just this module's
        // own reader/writer in isolation.
        let doc = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1.5,2.5,100.0]},"properties":{}}
        ]}"#;
        let fc = from_json(&parse(doc).unwrap()).unwrap();
        let wkb = crate::geometry::to_wkb(fc.features[0].geometry.as_ref().unwrap());
        let back = crate::geometry::from_wkb(&wkb).unwrap();
        assert_eq!(back, Geometry::Point(Position::with_z(1.5, 2.5, 100.0)));
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
                {"type":"Feature","geometry":{"coordinates":[[0,0],[1.5, 2.25]],"type":"LineString"},
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
                {"type":"Feature","geometry":{"type":"Point","coordinates":[-73.9857, 40.7484]},
                 "properties":{"name":"Café ☕","h":381,"ok":true,"r":4.7,"tags":["a","b"],"z":null}},
                {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1.5, 2.25]]},
                 "properties":{}},
                {"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]},
                 "properties":{"n":12}},
                {"type":"Feature","geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]},
                 "properties":{}},
                {"type":"Feature","geometry":null,"properties":{"only":"props"}}
            ]
        }"#;
        let fc = from_json(&parse(doc).unwrap()).unwrap();
        assert_eq!(to_geojson_string(&fc, true), to_json(&fc).to_json_string());
        // Laid out, it is the same JSON.
        assert_eq!(parse(&to_geojson_string(&fc, false)).unwrap(), to_json(&fc));
    }

    #[test]
    fn the_default_layout_puts_each_member_on_a_line_and_each_value_on_one() {
        let doc = r#"{"type":"FeatureCollection","name":"n","x-extra":{"a":[1,2]},"features":[
            {"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]},"properties":{"k":[1,2],"e":{}}},
            {"type":"Feature","geometry":{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[1,2]}]},"properties":{}},
            {"type":"Feature","geometry":null,"properties":{}}]}"#;
        let fc = from_geojson_str(doc).unwrap();
        assert_eq!(
            to_geojson_string(&fc, false),
            r#"{
  "type": "FeatureCollection",
  "name": "n",
  "x-extra": {"a":[1,2]},
  "features": [
    {
      "type": "Feature",
      "geometry": {
        "type": "Polygon",
        "coordinates": [[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,0.0]]]
      },
      "properties": {
        "k": [1,2],
        "e": {}
      }
    },
    {
      "type": "Feature",
      "geometry": {
        "type": "GeometryCollection",
        "geometries": [
          {
            "type": "Point",
            "coordinates": [1.0,2.0]
          }
        ]
      },
      "properties": {}
    },
    {
      "type": "Feature",
      "geometry": null,
      "properties": {}
    }
  ]
}
"#
        );
        let empty = FeatureCollection::new(vec![]);
        assert_eq!(to_geojson_string(&empty, false), "{\n  \"type\": \"FeatureCollection\",\n  \"features\": []\n}\n");
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
