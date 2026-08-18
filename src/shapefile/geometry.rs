//! `.shp`/`.shx` geometry codec (ESRI Shapefile Technical Description).
//!
//! The file header and each record header are big-endian; shape-record
//! content (coordinates, part/point counts) is little-endian — every helper
//! below is explicit about which half it touches. `.shx` is read-optional (a
//! sequential scan of `.shp` walks each record's own header) but
//! write-required (real-world readers expect it alongside a valid `.shp`).
//!
//! Shapefile has no distinct MultiPolygon shape type — type `5` covers both
//! `Polygon` and `MultiPolygon`, disambiguated by ring winding (shoelace
//! signed area: clockwise = shell, counter-clockwise = hole) and containment.
//! Real-world writers emit shells followed immediately by their holes, so
//! grouping by that appearance order (rather than a full point-in-polygon
//! containment test) is the implemented heuristic — see `plans/shapefile.org`.

use crate::error::{Error, Result};
use crate::geometry::{Bbox, Geometry, Position};

mod gtype {
    pub const NULL: i32 = 0;
    pub const POINT: i32 = 1;
    pub const POLYLINE: i32 = 3;
    pub const POLYGON: i32 = 5;
    pub const MULTIPOINT: i32 = 8;
    pub const POINT_Z: i32 = 11;
    pub const POLYLINE_Z: i32 = 13;
    pub const POLYGON_Z: i32 = 15;
    pub const MULTIPOINT_Z: i32 = 18;
    pub const POINT_M: i32 = 21;
    pub const POLYLINE_M: i32 = 23;
    pub const POLYGON_M: i32 = 25;
    pub const MULTIPOINT_M: i32 = 28;
}

/// ESRI's "no data" convention for an individual M value within an
/// otherwise M-bearing record: any value at or below this threshold means
/// "not measured" for that point (ESRI Shapefile Technical Description).
/// No real fixture could exercise the *per-point* case — WKT has no way to
/// express a partially-measured geometry — so that specific path is
/// verified only by this crate's own encoder/decoder agreeing with each
/// other, the same bar M7's Z `0.0` fallback used; the M section's overall
/// presence/absence and its values *were* confirmed against real
/// `ogr2ogr`-written bytes (see M7's M follow-up in `plans/zm-geometry.org`).
const NO_DATA_M: f64 = -1e38;

fn m_or_none(v: f64) -> Option<f64> {
    if v <= NO_DATA_M { None } else { Some(v) }
}

fn m_value(m: Option<f64>) -> f64 {
    m.unwrap_or(NO_DATA_M)
}

/// The Z-bearing shape type for a base (2D) shape type, or the type
/// unchanged if it has no Z variant (`NULL`, or an already-Z type). Real
/// byte layouts for all four confirmed against `ogr2ogr -f "ESRI
/// Shapefile"` output before implementing (see `plans/zm-geometry.org`'s
/// M7): `PointZ`=11, `PolyLineZ`=13, `PolygonZ`=15, `MultiPointZ`=18. Each
/// optionally carries a trailing M section on top of Z — see `has_m`
/// handling below — the type code itself doesn't change when M joins Z.
fn z_variant(base: i32) -> i32 {
    match base {
        gtype::POINT => gtype::POINT_Z,
        gtype::POLYLINE => gtype::POLYLINE_Z,
        gtype::POLYGON => gtype::POLYGON_Z,
        gtype::MULTIPOINT => gtype::MULTIPOINT_Z,
        other => other,
    }
}

/// The M-only (Z-absent) shape type for a base (2D) shape type, or the type
/// unchanged if it has no M variant. Real byte layouts confirmed against
/// `ogr2ogr`-written bytes from a `POINT M (...)` WKT source: `PointM`=21
/// is `type+X+Y+M` (28 bytes, no Z field at all) — genuinely different from
/// `PointZ` without M (also 28 bytes, but `type+X+Y+Z`); the M-only family
/// exists because Shapefile has no way to say "Z absent, M present" within
/// the Z-variant type codes.
fn m_variant(base: i32) -> i32 {
    match base {
        gtype::POINT => gtype::POINT_M,
        gtype::POLYLINE => gtype::POLYLINE_M,
        gtype::POLYGON => gtype::POLYGON_M,
        gtype::MULTIPOINT => gtype::MULTIPOINT_M,
        other => other,
    }
}

// --- reader ------------------------------------------------------------

/// Parse a `.shp` file into one `Option<Geometry>` per record, in file order
/// (`None` for a Null-shape record). Reads sequentially to EOF using each
/// record's own content-length prefix — `.shx` is not consulted.
pub fn read(data: &[u8]) -> Result<Vec<Option<Geometry>>> {
    if data.len() < 100 {
        return Err(Error::Convert("shapefile: .shp file shorter than its header".into()));
    }
    if i32_be(data, 0)? != 9994 {
        return Err(Error::Convert("shapefile: bad .shp file code (expected 9994)".into()));
    }

    let mut geometries = Vec::new();
    let mut pos = 100usize;
    while pos + 8 <= data.len() {
        let content_len = as_len(i32_be(data, pos + 4)?, "record content length")? * 2;
        let content = data
            .get(pos + 8..pos + 8 + content_len)
            .ok_or_else(|| Error::Convert("shapefile: truncated .shp record".into()))?;
        geometries.push(decode_shape(content)?);
        pos += 8 + content_len;
    }
    Ok(geometries)
}

fn decode_shape(content: &[u8]) -> Result<Option<Geometry>> {
    let shape_type = i32_le(content, 0)?;
    Ok(match shape_type {
        gtype::NULL => None,
        // `POINT_M`'s M sits exactly where a plain `POINT` record simply
        // ends (offset 20), so a real 2D `POINT` record (20 bytes, no room
        // for `content.len() >= 28`) and a real `POINT_M` record share this
        // one arm safely — `m_or_none` only ever fires when the bytes exist.
        gtype::POINT | gtype::POINT_M => {
            let x = f64_le(content, 4)?;
            let y = f64_le(content, 12)?;
            let m = if content.len() >= 28 { m_or_none(f64_le(content, 20)?) } else { None };
            Some(Geometry::Point(Position { x, y, z: None, m }))
        }
        gtype::POINT_Z => {
            let x = f64_le(content, 4)?;
            let y = f64_le(content, 12)?;
            let z = f64_le(content, 20)?;
            let m = if content.len() >= 36 { m_or_none(f64_le(content, 28)?) } else { None };
            Some(Geometry::Point(Position { x, y, z: Some(z), m }))
        }
        gtype::MULTIPOINT | gtype::MULTIPOINT_M => {
            let n = as_len(i32_le(content, 36)?, "MultiPoint numPoints")?;
            let xy_off = 40;
            let points = apply_m_if_present(read_points(content, xy_off, n)?, content, xy_off + 16 * n, n)?;
            Some(Geometry::MultiPoint(points))
        }
        gtype::MULTIPOINT_Z => {
            let n = as_len(i32_le(content, 36)?, "MultiPointZ numPoints")?;
            let xy_off = 40;
            let points = apply_z(read_points(content, xy_off, n)?, content, xy_off + 16 * n)?;
            let after_z = xy_off + 16 * n + 16 + 8 * n;
            let points = apply_m_if_present(points, content, after_z, n)?;
            Some(Geometry::MultiPoint(points))
        }
        gtype::POLYLINE | gtype::POLYLINE_M => {
            let parts = read_parts(content, false)?;
            Some(if parts.len() == 1 {
                Geometry::LineString(parts.into_iter().next().unwrap())
            } else {
                Geometry::MultiLineString(parts)
            })
        }
        gtype::POLYLINE_Z => {
            let parts = read_parts(content, true)?;
            Some(if parts.len() == 1 {
                Geometry::LineString(parts.into_iter().next().unwrap())
            } else {
                Geometry::MultiLineString(parts)
            })
        }
        gtype::POLYGON | gtype::POLYGON_M => Some(classify_rings(read_parts(content, false)?)),
        gtype::POLYGON_Z => Some(classify_rings(read_parts(content, true)?)),
        other => return Err(Error::Convert(format!("shapefile: unsupported shape type {other} (MultiPatch out of scope)"))),
    })
}

/// Split a PolyLine/Polygon body into its parts (rings/lines): bbox, numParts,
/// numPoints, part start-indices, and a flat point array, plus (if `has_z`) a
/// trailing Zmin/Zmax/Z-array section — same layout for both the 2D and Z
/// shape types, confirmed via real `ogr2ogr`-written `.shp` bytes before
/// implementing (see M7 of `plans/zm-geometry.org`). An M section (present
/// or not, regardless of `has_z` — the M-only shape types share this same
/// tail shape minus the Z section) is detected purely by remaining content
/// length, not by the declared shape type — see `apply_m_if_present`.
fn read_parts(content: &[u8], has_z: bool) -> Result<Vec<Vec<Position>>> {
    let num_parts = as_len(i32_le(content, 36)?, "numParts")?;
    let num_points = as_len(i32_le(content, 40)?, "numPoints")?;
    let parts_off = 44;
    let mut starts = Vec::with_capacity(num_parts);
    for i in 0..num_parts {
        starts.push(as_len(i32_le(content, parts_off + 4 * i)?, "part start index")?);
    }
    let xy_off = parts_off + 4 * num_parts;
    let mut points = read_points(content, xy_off, num_points)?;
    let mut m_off = xy_off + 16 * num_points;
    if has_z {
        points = apply_z(points, content, m_off)?;
        m_off += 16 + 8 * num_points;
    }
    points = apply_m_if_present(points, content, m_off, num_points)?;

    let mut parts = Vec::with_capacity(num_parts);
    for i in 0..num_parts {
        let start = starts[i];
        let end = starts.get(i + 1).copied().unwrap_or(num_points);
        let part = points
            .get(start..end)
            .ok_or_else(|| Error::Convert("shapefile: part index out of range".into()))?;
        parts.push(part.to_vec());
    }
    Ok(parts)
}

fn read_points(content: &[u8], offset: usize, n: usize) -> Result<Vec<Position>> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(Position::new(f64_le(content, offset + 16 * i)?, f64_le(content, offset + 16 * i + 8)?));
    }
    Ok(out)
}

/// Fill in each position's `z` from a Z section starting at `z_off`
/// (`Zmin`, `Zmax`, then one `f64` per point in the same order as `points`).
fn apply_z(mut points: Vec<Position>, content: &[u8], z_off: usize) -> Result<Vec<Position>> {
    let z_array_off = z_off + 16; // skip Zmin/Zmax
    for (i, p) in points.iter_mut().enumerate() {
        p.z = Some(f64_le(content, z_array_off + 8 * i)?);
    }
    Ok(points)
}

/// Fill in each position's `m` from an M section starting at `m_off`
/// (`Mmin`, `Mmax`, then one `f64` per point), *if the record actually has
/// room for one*. The M section is always optional, disambiguated purely by
/// the record's own content length — confirmed via real `ogr2ogr`-written
/// `.shp` bytes: an M-bearing WKT source writes the full section, while a
/// Z-only (unmeasured) source writes zero trailing bytes at all, not a
/// sentinel-filled placeholder. A raw value at or below the ESRI "no data"
/// threshold becomes `None` even when the section is present, marking that
/// one point unmeasured within an otherwise M-bearing shape.
fn apply_m_if_present(mut points: Vec<Position>, content: &[u8], m_off: usize, n: usize) -> Result<Vec<Position>> {
    if content.len() < m_off + 16 + 8 * n {
        return Ok(points);
    }
    let m_array_off = m_off + 16; // skip Mmin/Mmax
    for (i, p) in points.iter_mut().enumerate() {
        p.m = m_or_none(f64_le(content, m_array_off + 8 * i)?);
    }
    Ok(points)
}

// --- ring classification (Polygon vs MultiPolygon) ----------------------

/// Signed area via the shoelace formula: positive => counter-clockwise
/// (a hole), negative => clockwise (a shell).
fn signed_area(ring: &[Position]) -> f64 {
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let p0 = ring[i];
        let p1 = ring[(i + 1) % ring.len()];
        sum += p0.x * p1.y - p1.x * p0.y;
    }
    sum / 2.0
}

fn is_clockwise(ring: &[Position]) -> bool {
    signed_area(ring) < 0.0
}

/// Group parsed rings into one or more polygons by winding + appearance order:
/// a clockwise ring starts a new shell; a counter-clockwise ring joins the
/// most recent shell as a hole (or starts one itself if none precedes it — a
/// malformed input, handled gracefully rather than panicking).
fn classify_rings(rings: Vec<Vec<Position>>) -> Geometry {
    let mut polys: Vec<Vec<Vec<Position>>> = Vec::new();
    for ring in rings {
        if polys.is_empty() || is_clockwise(&ring) {
            polys.push(vec![ring]);
        } else {
            polys.last_mut().unwrap().push(ring);
        }
    }
    if polys.len() <= 1 {
        Geometry::Polygon(polys.into_iter().next().unwrap_or_default())
    } else {
        Geometry::MultiPolygon(polys)
    }
}

/// Force a polygon's own rings to the Shapefile winding convention: shell
/// (index 0) clockwise, holes counter-clockwise. Used on write, since Geosetta
/// controls what it emits even when it can't guarantee an arbitrary input
/// geometry's winding.
fn polygon_rings(rings: &[Vec<Position>]) -> Vec<Vec<Position>> {
    rings.iter().enumerate().map(|(i, r)| ensure_winding(r, i == 0)).collect()
}

fn ensure_winding(ring: &[Position], want_clockwise: bool) -> Vec<Position> {
    if is_clockwise(ring) == want_clockwise {
        ring.to_vec()
    } else {
        let mut r = ring.to_vec();
        r.reverse();
        r
    }
}

// --- writer --------------------------------------------------------------

/// Encode geometries (in order) as a `.shp`/`.shx` pair. Errors if the
/// geometries mix incompatible Shapefile shape families (e.g. Point and
/// Polygon) — a single `.shp` can only declare one shape type — or contain a
/// `GeometryCollection`, which Shapefile cannot represent at all.
pub fn write(geometries: &[Option<Geometry>]) -> Result<(Vec<u8>, Vec<u8>)> {
    let base_shape_type = header_shape_type(geometries)?;
    // A `.shp` declares one shape type for the whole file, so any Z-bearing
    // geometry promotes every record to the Z-variant shape type — a
    // geometry that itself lacks Z (a mixed 2D/3D source) falls back to
    // `0.0` per point, the same convention `wkb.rs`/`flatgeobuf/writer.rs`
    // use for the same reason (no per-point "no data" marker for Z, unlike
    // M's sentinel, which does have one — see `m_value`). M alone (no Z)
    // promotes to the M-only shape type instead; M alongside Z stays on the
    // Z-variant type code with an extra trailing section, matching real
    // `ogr2ogr`-written `POINT ZM`/`LINESTRING ZM`/etc. bytes.
    let has_z = geometries.iter().flatten().any(geometry_has_z);
    let has_m = geometries.iter().flatten().any(geometry_has_m);
    let shape_type = match (has_z, has_m) {
        (true, _) => z_variant(base_shape_type),
        (false, true) => m_variant(base_shape_type),
        (false, false) => base_shape_type,
    };

    let mut bbox = Bbox::empty();
    for g in geometries.iter().flatten() {
        g.extend_bbox(&mut bbox);
    }
    let z_range = fold_range(geometries, has_z, fold_z);
    let m_range = fold_range(geometries, has_m, fold_m);

    let mut shp = vec![0u8; 100];
    let mut shx = vec![0u8; 100];
    for (i, g) in geometries.iter().enumerate() {
        let content = encode_shape(g.as_ref(), has_z, has_m)?;
        let content_words = (content.len() / 2) as i32;
        let record_offset_words = (shp.len() / 2) as i32;

        shp.extend_from_slice(&((i + 1) as i32).to_be_bytes());
        shp.extend_from_slice(&content_words.to_be_bytes());
        shp.extend_from_slice(&content);

        shx.extend_from_slice(&record_offset_words.to_be_bytes());
        shx.extend_from_slice(&content_words.to_be_bytes());
    }

    write_header(&mut shp, shape_type, &bbox, z_range, m_range);
    write_header(&mut shx, shape_type, &bbox, z_range, m_range);
    Ok((shp, shx))
}

fn fold_range(
    geometries: &[Option<Geometry>],
    present: bool,
    fold: impl Fn(&Geometry, &mut f64, &mut f64),
) -> (f64, f64) {
    if !present {
        return (0.0, 0.0);
    }
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for g in geometries.iter().flatten() {
        fold(g, &mut lo, &mut hi);
    }
    (lo, hi)
}

/// Whether any position in `g` carries Z — used to decide, file-wide,
/// whether to promote the whole `.shp` to its Z-variant shape type.
fn geometry_has_z(g: &Geometry) -> bool {
    match g {
        Geometry::Point(p) => p.z.is_some(),
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => ps.iter().any(|p| p.z.is_some()),
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            rings.iter().flatten().any(|p| p.z.is_some())
        }
        Geometry::MultiPolygon(polys) => polys.iter().flatten().flatten().any(|p| p.z.is_some()),
        Geometry::GeometryCollection(geoms) => geoms.iter().any(geometry_has_z),
    }
}

/// Whether any position in `g` carries M — used to decide, file-wide,
/// whether to promote the whole `.shp` to an M-bearing shape type (either
/// the Z-variant with an extra M section, or the M-only variant if Z is
/// absent — see `write()`).
fn geometry_has_m(g: &Geometry) -> bool {
    match g {
        Geometry::Point(p) => p.m.is_some(),
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => ps.iter().any(|p| p.m.is_some()),
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            rings.iter().flatten().any(|p| p.m.is_some())
        }
        Geometry::MultiPolygon(polys) => polys.iter().flatten().flatten().any(|p| p.m.is_some()),
        Geometry::GeometryCollection(geoms) => geoms.iter().any(geometry_has_m),
    }
}

/// Fold `g`'s Z values (0.0 fallback for a position missing Z) into a
/// running `[lo, hi]` range — used for the file header's Zmin/Zmax, matching
/// `extend_bbox`'s own traversal shape but kept independent since `Bbox`
/// itself stays 2D-only (see `plans/zm-geometry.org`'s OPEN QUESTIONS).
fn fold_z(g: &Geometry, lo: &mut f64, hi: &mut f64) {
    match g {
        Geometry::Point(p) => {
            let z = p.z.unwrap_or(0.0);
            *lo = lo.min(z);
            *hi = hi.max(z);
        }
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => {
            for p in ps {
                let z = p.z.unwrap_or(0.0);
                *lo = lo.min(z);
                *hi = hi.max(z);
            }
        }
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            for p in rings.iter().flatten() {
                let z = p.z.unwrap_or(0.0);
                *lo = lo.min(z);
                *hi = hi.max(z);
            }
        }
        Geometry::MultiPolygon(polys) => {
            for p in polys.iter().flatten().flatten() {
                let z = p.z.unwrap_or(0.0);
                *lo = lo.min(z);
                *hi = hi.max(z);
            }
        }
        Geometry::GeometryCollection(geoms) => {
            for g in geoms {
                fold_z(g, lo, hi);
            }
        }
    }
}

/// Fold `g`'s M values into a running `[lo, hi]` range — *skipping* any
/// position missing M entirely, rather than defaulting it to a value, since
/// an unmeasured point shouldn't drag the header's summary range toward the
/// sentinel; matches ESRI's own convention that only genuinely measured
/// points extend Mmin/Mmax.
fn fold_m(g: &Geometry, lo: &mut f64, hi: &mut f64) {
    match g {
        Geometry::Point(p) => {
            if let Some(m) = p.m {
                *lo = lo.min(m);
                *hi = hi.max(m);
            }
        }
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => {
            for p in ps {
                if let Some(m) = p.m {
                    *lo = lo.min(m);
                    *hi = hi.max(m);
                }
            }
        }
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            for p in rings.iter().flatten() {
                if let Some(m) = p.m {
                    *lo = lo.min(m);
                    *hi = hi.max(m);
                }
            }
        }
        Geometry::MultiPolygon(polys) => {
            for p in polys.iter().flatten().flatten() {
                if let Some(m) = p.m {
                    *lo = lo.min(m);
                    *hi = hi.max(m);
                }
            }
        }
        Geometry::GeometryCollection(geoms) => {
            for g in geoms {
                fold_m(g, lo, hi);
            }
        }
    }
}

fn header_shape_type(geometries: &[Option<Geometry>]) -> Result<i32> {
    let mut found: Option<i32> = None;
    for g in geometries.iter().flatten() {
        let t = base_shape_type(g)?;
        match found {
            None => found = Some(t),
            Some(prev) if prev != t => {
                return Err(Error::Convert(format!(
                    "shapefile: cannot write mixed geometry types in one .shp (found shape types \
                     {prev} and {t}); split the collection by geometry type first"
                )));
            }
            _ => {}
        }
    }
    Ok(found.unwrap_or(gtype::NULL))
}

fn base_shape_type(g: &Geometry) -> Result<i32> {
    Ok(match g {
        Geometry::Point(_) => gtype::POINT,
        Geometry::MultiPoint(_) => gtype::MULTIPOINT,
        Geometry::LineString(_) | Geometry::MultiLineString(_) => gtype::POLYLINE,
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) => gtype::POLYGON,
        Geometry::GeometryCollection(_) => {
            return Err(Error::Convert("shapefile: GeometryCollection cannot be represented in a .shp".into()));
        }
    })
}

fn encode_shape(g: Option<&Geometry>, has_z: bool, has_m: bool) -> Result<Vec<u8>> {
    let Some(g) = g else {
        return Ok(gtype::NULL.to_le_bytes().to_vec());
    };
    Ok(match g {
        Geometry::Point(p) => encode_point(*p, has_z, has_m),
        Geometry::MultiPoint(ps) => encode_multipoint(ps, has_z, has_m),
        Geometry::LineString(ps) => encode_parts(gtype::POLYLINE, std::slice::from_ref(ps), has_z, has_m),
        Geometry::MultiLineString(parts) => encode_parts(gtype::POLYLINE, parts, has_z, has_m),
        Geometry::Polygon(rings) => encode_parts(gtype::POLYGON, &polygon_rings(rings), has_z, has_m),
        Geometry::MultiPolygon(polys) => {
            let rings: Vec<Vec<Position>> = polys.iter().flat_map(|r| polygon_rings(r)).collect();
            encode_parts(gtype::POLYGON, &rings, has_z, has_m)
        }
        Geometry::GeometryCollection(_) => unreachable!("rejected by base_shape_type"),
    })
}

/// The shape type for a base (2D) type given file-wide `has_z`/`has_m`: Z
/// wins the type-code slot regardless of `has_m` (M just adds a trailing
/// section on top — see `write_m_section`); M alone promotes to the M-only
/// family instead. Real `ogr2ogr`-written bytes confirmed both: a `POINT
/// ZM` source still writes shape type 11 (`PointZ`) with M appended, while
/// a `POINT M` source (no Z) writes shape type 21 (`PointM`).
fn dim_shape_type(base: i32, has_z: bool, has_m: bool) -> i32 {
    match (has_z, has_m) {
        (true, _) => z_variant(base),
        (false, true) => m_variant(base),
        (false, false) => base,
    }
}

fn encode_point(p: Position, has_z: bool, has_m: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&dim_shape_type(gtype::POINT, has_z, has_m).to_le_bytes());
    out.extend_from_slice(&p.x.to_le_bytes());
    out.extend_from_slice(&p.y.to_le_bytes());
    if has_z {
        out.extend_from_slice(&p.z.unwrap_or(0.0).to_le_bytes());
    }
    if has_m {
        out.extend_from_slice(&m_value(p.m).to_le_bytes());
    }
    out
}

fn encode_multipoint(points: &[Position], has_z: bool, has_m: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&dim_shape_type(gtype::MULTIPOINT, has_z, has_m).to_le_bytes());
    write_bbox_le(&mut out, &points_bbox(points));
    out.extend_from_slice(&(points.len() as i32).to_le_bytes());
    for p in points {
        out.extend_from_slice(&p.x.to_le_bytes());
        out.extend_from_slice(&p.y.to_le_bytes());
    }
    if has_z {
        write_z_section(&mut out, points);
    }
    if has_m {
        write_m_section(&mut out, points);
    }
    out
}

fn encode_parts(shape_type: i32, parts: &[Vec<Position>], has_z: bool, has_m: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&dim_shape_type(shape_type, has_z, has_m).to_le_bytes());
    let all_points: Vec<Position> = parts.iter().flatten().copied().collect();
    write_bbox_le(&mut out, &points_bbox(&all_points));
    out.extend_from_slice(&(parts.len() as i32).to_le_bytes());
    out.extend_from_slice(&(all_points.len() as i32).to_le_bytes());
    let mut start = 0i32;
    for part in parts {
        out.extend_from_slice(&start.to_le_bytes());
        start += part.len() as i32;
    }
    for p in &all_points {
        out.extend_from_slice(&p.x.to_le_bytes());
        out.extend_from_slice(&p.y.to_le_bytes());
    }
    if has_z {
        write_z_section(&mut out, &all_points);
    }
    if has_m {
        write_m_section(&mut out, &all_points);
    }
    out
}

/// `Zmin`, `Zmax`, then one `f64` per point (0.0 fallback for a position
/// missing Z), matching the layout confirmed against real `ogr2ogr`-written
/// `.shp` bytes for `MultiPointZ`/`PolyLineZ`/`PolygonZ` (M7's verification
/// step).
fn write_z_section(out: &mut Vec<u8>, points: &[Position]) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    let zs: Vec<f64> = points
        .iter()
        .map(|p| {
            let z = p.z.unwrap_or(0.0);
            lo = lo.min(z);
            hi = hi.max(z);
            z
        })
        .collect();
    let (lo, hi) = if zs.is_empty() { (0.0, 0.0) } else { (lo, hi) };
    out.extend_from_slice(&lo.to_le_bytes());
    out.extend_from_slice(&hi.to_le_bytes());
    for z in zs {
        out.extend_from_slice(&z.to_le_bytes());
    }
}

/// `Mmin`, `Mmax` (over genuinely measured points only, matching `fold_m`'s
/// convention — an unmeasured point doesn't drag the range toward the
/// sentinel), then one `f64` per point using the "no data" sentinel
/// (`m_value`) for any position missing M. Matches the layout confirmed
/// against real `ogr2ogr`-written `.shp` bytes for a `ZM`-bearing
/// LineString/Polygon/MultiPoint and a pure `M`-only Point.
fn write_m_section(out: &mut Vec<u8>, points: &[Position]) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in points {
        if let Some(m) = p.m {
            lo = lo.min(m);
            hi = hi.max(m);
        }
    }
    let (lo, hi) = if lo.is_finite() { (lo, hi) } else { (0.0, 0.0) };
    out.extend_from_slice(&lo.to_le_bytes());
    out.extend_from_slice(&hi.to_le_bytes());
    for p in points {
        out.extend_from_slice(&m_value(p.m).to_le_bytes());
    }
}

fn points_bbox(points: &[Position]) -> Bbox {
    let mut b = Bbox::empty();
    for p in points {
        b.add(*p);
    }
    b
}

fn write_bbox_le(out: &mut Vec<u8>, bbox: &Bbox) {
    let (xmin, ymin, xmax, ymax) =
        if bbox.is_empty() { (0.0, 0.0, 0.0, 0.0) } else { (bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y) };
    for v in [xmin, ymin, xmax, ymax] {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn write_header(buf: &mut [u8], shape_type: i32, bbox: &Bbox, z_range: (f64, f64), m_range: (f64, f64)) {
    let file_len_words = (buf.len() / 2) as i32;
    buf[0..4].copy_from_slice(&9994i32.to_be_bytes());
    buf[24..28].copy_from_slice(&file_len_words.to_be_bytes());
    buf[28..32].copy_from_slice(&1000i32.to_le_bytes());
    buf[32..36].copy_from_slice(&shape_type.to_le_bytes());
    let (xmin, ymin, xmax, ymax) =
        if bbox.is_empty() { (0.0, 0.0, 0.0, 0.0) } else { (bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y) };
    let (zmin, zmax) = z_range;
    let (mmin, mmax) = m_range;
    for (i, v) in [xmin, ymin, xmax, ymax, zmin, zmax, mmin, mmax].into_iter().enumerate() {
        buf[36 + 8 * i..44 + 8 * i].copy_from_slice(&v.to_le_bytes());
    }
}

// --- endianness helpers ----------------------------------------------------

fn i32_be(b: &[u8], at: usize) -> Result<i32> {
    Ok(i32::from_be_bytes(
        b.get(at..at + 4).ok_or_else(oob)?.try_into().unwrap(),
    ))
}

fn i32_le(b: &[u8], at: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(
        b.get(at..at + 4).ok_or_else(oob)?.try_into().unwrap(),
    ))
}

fn f64_le(b: &[u8], at: usize) -> Result<f64> {
    Ok(f64::from_le_bytes(
        b.get(at..at + 8).ok_or_else(oob)?.try_into().unwrap(),
    ))
}

fn oob() -> Error {
    Error::Convert("shapefile: read past end of .shp record".into())
}

/// A negative count field (malformed input) errors instead of silently
/// wrapping to a huge `usize` via `as`.
fn as_len(v: i32, what: &str) -> Result<usize> {
    if v < 0 {
        return Err(Error::Convert(format!("shapefile: negative {what} ({v})")));
    }
    Ok(v as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(geoms: Vec<Option<Geometry>>) -> Vec<Option<Geometry>> {
        let (shp, _shx) = write(&geoms).unwrap();
        read(&shp).unwrap()
    }

    #[test]
    fn point_round_trips() {
        let geoms = vec![Some(Geometry::Point(Position::new(1.5, -2.5))), None, Some(Geometry::Point(Position::new(0.0, 0.0)))];
        assert_eq!(round_trip(geoms.clone()), geoms);
    }

    #[test]
    fn multipoint_round_trips() {
        let geoms = vec![Some(Geometry::MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, -1.0)]))];
        assert_eq!(round_trip(geoms.clone()), geoms);
    }

    #[test]
    fn linestring_and_multilinestring_round_trip() {
        let line = Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)]);
        let multi = Geometry::MultiLineString(vec![
            vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0)],
            vec![Position::new(5.0, 5.0), Position::new(6.0, 6.0), Position::new(7.0, 5.0)],
        ]);
        assert_eq!(round_trip(vec![Some(line.clone())]), vec![Some(line)]);
        assert_eq!(round_trip(vec![Some(multi.clone())]), vec![Some(multi)]);
    }

    #[test]
    fn polygon_with_hole_round_trips() {
        // Esri winding: shell clockwise, hole counter-clockwise. The writer
        // normalizes to this convention regardless of input winding, so a
        // fixture already in that convention round-trips byte-for-byte
        // (vertex order included) rather than merely geometrically.
        let shell = vec![Position::new(0.0, 0.0), Position::new(0.0, 4.0), Position::new(4.0, 4.0), Position::new(4.0, 0.0), Position::new(0.0, 0.0)]; // CW
        let hole = vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)]; // CCW
        let poly = Geometry::Polygon(vec![shell, hole]);
        assert_eq!(round_trip(vec![Some(poly.clone())]), vec![Some(poly)]);
    }

    #[test]
    fn multipolygon_two_shells_round_trips() {
        let a = vec![Position::new(0.0, 0.0), Position::new(0.0, 1.0), Position::new(1.0, 1.0), Position::new(1.0, 0.0), Position::new(0.0, 0.0)]; // CW
        let b = vec![Position::new(5.0, 5.0), Position::new(5.0, 6.0), Position::new(6.0, 6.0), Position::new(6.0, 5.0), Position::new(5.0, 5.0)]; // CW
        let multi = Geometry::MultiPolygon(vec![vec![a], vec![b]]);
        assert_eq!(round_trip(vec![Some(multi.clone())]), vec![Some(multi)]);
    }

    #[test]
    fn multipolygon_with_hole_in_one_shell_round_trips() {
        let shell1 = vec![Position::new(0.0, 0.0), Position::new(0.0, 4.0), Position::new(4.0, 4.0), Position::new(4.0, 0.0), Position::new(0.0, 0.0)]; // CW
        let hole1 = vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)]; // CCW
        let shell2 = vec![Position::new(10.0, 10.0), Position::new(10.0, 11.0), Position::new(11.0, 11.0), Position::new(11.0, 10.0), Position::new(10.0, 10.0)]; // CW
        let multi = Geometry::MultiPolygon(vec![vec![shell1, hole1], vec![shell2]]);
        assert_eq!(round_trip(vec![Some(multi.clone())]), vec![Some(multi)]);
    }

    #[test]
    fn ring_classification_is_correct_independent_of_file_io() {
        // Shell CW, hole CCW (by construction), a disjoint second shell CW.
        let shell = vec![Position::new(0.0, 0.0), Position::new(0.0, 4.0), Position::new(4.0, 4.0), Position::new(4.0, 0.0), Position::new(0.0, 0.0)]; // CW
        assert!(is_clockwise(&shell));
        let hole = vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)]; // CCW
        assert!(!is_clockwise(&hole));
        let shell2 = vec![Position::new(10.0, 0.0), Position::new(10.0, 4.0), Position::new(14.0, 4.0), Position::new(14.0, 0.0), Position::new(10.0, 0.0)]; // CW

        match classify_rings(vec![shell.clone(), hole.clone(), shell2.clone()]) {
            Geometry::MultiPolygon(polys) => {
                assert_eq!(polys.len(), 2);
                assert_eq!(polys[0], vec![shell, hole]);
                assert_eq!(polys[1], vec![shell2]);
            }
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn single_shell_no_holes_is_a_plain_polygon() {
        let shell = vec![Position::new(0.0, 0.0), Position::new(0.0, 1.0), Position::new(1.0, 1.0), Position::new(1.0, 0.0), Position::new(0.0, 0.0)];
        match classify_rings(vec![shell.clone()]) {
            Geometry::Polygon(rings) => assert_eq!(rings, vec![shell]),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn mixed_shape_families_error_on_write() {
        let geoms = vec![Some(Geometry::Point(Position::new(0.0, 0.0))), Some(Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]))];
        assert!(write(&geoms).is_err());
    }

    #[test]
    fn geometry_collection_errors_on_write() {
        let geoms = vec![Some(Geometry::GeometryCollection(vec![Geometry::Point(Position::new(0.0, 0.0))]))];
        assert!(write(&geoms).is_err());
    }

    #[test]
    fn z_bearing_point_round_trips() {
        let geoms = vec![Some(Geometry::Point(Position::with_z(1.5, -2.5, 381.0))), None];
        assert_eq!(round_trip(geoms.clone()), geoms);
    }

    #[test]
    fn z_bearing_multipoint_round_trips() {
        let geoms = vec![Some(Geometry::MultiPoint(vec![
            Position::with_z(0.0, 0.0, 1.0),
            Position::with_z(1.0, 1.0, 2.0),
        ]))];
        assert_eq!(round_trip(geoms.clone()), geoms);
    }

    #[test]
    fn z_bearing_linestring_and_multilinestring_round_trip() {
        let line = Geometry::LineString(vec![
            Position::with_z(0.0, 0.0, 1.0),
            Position::with_z(1.0, 1.0, 2.0),
            Position::with_z(2.0, 0.0, 3.0),
        ]);
        let multi = Geometry::MultiLineString(vec![
            vec![Position::with_z(0.0, 0.0, 1.0), Position::with_z(1.0, 0.0, 2.0)],
            vec![
                Position::with_z(5.0, 5.0, 3.0),
                Position::with_z(6.0, 6.0, 4.0),
                Position::with_z(7.0, 5.0, 5.0),
            ],
        ]);
        assert_eq!(round_trip(vec![Some(line.clone())]), vec![Some(line)]);
        assert_eq!(round_trip(vec![Some(multi.clone())]), vec![Some(multi)]);
    }

    #[test]
    fn z_bearing_polygon_with_hole_round_trips() {
        let shell = vec![
            Position::with_z(0.0, 0.0, 1.0),
            Position::with_z(0.0, 4.0, 1.0),
            Position::with_z(4.0, 4.0, 1.0),
            Position::with_z(4.0, 0.0, 1.0),
            Position::with_z(0.0, 0.0, 1.0),
        ]; // CW
        let hole = vec![
            Position::with_z(1.0, 1.0, 2.0),
            Position::with_z(2.0, 1.0, 2.0),
            Position::with_z(2.0, 2.0, 2.0),
            Position::with_z(1.0, 2.0, 2.0),
            Position::with_z(1.0, 1.0, 2.0),
        ]; // CCW
        let poly = Geometry::Polygon(vec![shell, hole]);
        assert_eq!(round_trip(vec![Some(poly.clone())]), vec![Some(poly)]);
    }

    #[test]
    fn a_2d_only_collection_writes_the_base_shape_type() {
        // No promotion to the Z-variant shape type when nothing carries Z —
        // confirms `has_z` detection doesn't false-positive.
        let geoms = vec![Some(Geometry::Point(Position::new(1.0, 2.0)))];
        let (shp, _shx) = write(&geoms).unwrap();
        assert_eq!(i32_le(&shp, 32).unwrap(), gtype::POINT);
    }

    #[test]
    fn one_z_bearing_feature_promotes_the_whole_file_and_2d_siblings_fall_back_to_zero() {
        // A mixed 2D/3D source (e.g. one GeoJSON point missing its third
        // ordinate) still produces a single valid PointZ file — the 2D
        // point's Z falls back to 0.0 rather than the file mixing shape
        // types, which Shapefile cannot represent.
        let geoms = vec![
            Some(Geometry::Point(Position::with_z(1.0, 2.0, 381.0))),
            Some(Geometry::Point(Position::new(3.0, 4.0))),
        ];
        let (shp, _shx) = write(&geoms).unwrap();
        assert_eq!(i32_le(&shp, 32).unwrap(), gtype::POINT_Z);
        let back = read(&shp).unwrap();
        assert_eq!(back[0], Some(Geometry::Point(Position::with_z(1.0, 2.0, 381.0))));
        assert_eq!(back[1], Some(Geometry::Point(Position::with_z(3.0, 4.0, 0.0))));
    }

    #[test]
    fn m_only_point_round_trips() {
        let geoms = vec![Some(Geometry::Point(Position::with_m(1.0, 2.0, 5.0)))];
        assert_eq!(round_trip(geoms.clone()), geoms);
        let (shp, _shx) = write(&geoms).unwrap();
        assert_eq!(i32_le(&shp, 32).unwrap(), gtype::POINT_M);
    }

    #[test]
    fn zm_point_round_trips_on_the_z_variant_shape_type() {
        // M alongside Z stays on the Z-variant type code (real ogr2ogr
        // behavior: a `POINT ZM` source still writes shape type 11).
        let geoms = vec![Some(Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)))];
        assert_eq!(round_trip(geoms.clone()), geoms);
        let (shp, _shx) = write(&geoms).unwrap();
        assert_eq!(i32_le(&shp, 32).unwrap(), gtype::POINT_Z);
    }

    #[test]
    fn m_only_multipoint_round_trips() {
        let geoms = vec![Some(Geometry::MultiPoint(vec![
            Position::with_m(0.0, 0.0, 10.0),
            Position::with_m(1.0, 1.0, 20.0),
        ]))];
        assert_eq!(round_trip(geoms.clone()), geoms);
        let (shp, _shx) = write(&geoms).unwrap();
        assert_eq!(i32_le(&shp, 32).unwrap(), gtype::MULTIPOINT_M);
    }

    #[test]
    fn zm_linestring_round_trips() {
        let line = Geometry::LineString(vec![
            Position::with_zm(0.0, 0.0, 1.0, 10.0),
            Position::with_zm(1.0, 1.0, 2.0, 20.0),
            Position::with_zm(2.0, 0.0, 3.0, 30.0),
        ]);
        assert_eq!(round_trip(vec![Some(line.clone())]), vec![Some(line)]);
    }

    #[test]
    fn m_only_polygon_with_hole_round_trips() {
        let shell = vec![
            Position::with_m(0.0, 0.0, 1.0),
            Position::with_m(0.0, 4.0, 1.0),
            Position::with_m(4.0, 4.0, 1.0),
            Position::with_m(4.0, 0.0, 1.0),
            Position::with_m(0.0, 0.0, 1.0),
        ]; // CW
        let hole = vec![
            Position::with_m(1.0, 1.0, 2.0),
            Position::with_m(2.0, 1.0, 2.0),
            Position::with_m(2.0, 2.0, 2.0),
            Position::with_m(1.0, 2.0, 2.0),
            Position::with_m(1.0, 1.0, 2.0),
        ]; // CCW
        let poly = Geometry::Polygon(vec![shell, hole]);
        assert_eq!(round_trip(vec![Some(poly.clone())]), vec![Some(poly)]);
    }

    #[test]
    fn a_point_with_no_m_within_an_m_bearing_file_round_trips_as_none_not_the_sentinel() {
        // The per-point "no data" sentinel path: no real WKT source can
        // express this mix, so this is verified purely as a self-consistency
        // round trip (encoder writes NO_DATA_M for the missing point, decoder
        // maps it back to None) — the same bar M7's Z 0.0-fallback used.
        let geoms = vec![
            Some(Geometry::Point(Position::with_m(1.0, 2.0, 5.0))),
            Some(Geometry::Point(Position::new(3.0, 4.0))),
        ];
        let (shp, _shx) = write(&geoms).unwrap();
        assert_eq!(i32_le(&shp, 32).unwrap(), gtype::POINT_M);
        let back = read(&shp).unwrap();
        assert_eq!(back[0], Some(Geometry::Point(Position::with_m(1.0, 2.0, 5.0))));
        assert_eq!(back[1], Some(Geometry::Point(Position::new(3.0, 4.0))));
    }

    #[test]
    fn header_m_range_ignores_unmeasured_points() {
        // Mmin/Mmax should reflect only the genuinely measured point (5.0),
        // not be dragged toward the NO_DATA_M sentinel by the unmeasured one.
        let geoms = vec![
            Some(Geometry::Point(Position::with_m(1.0, 2.0, 5.0))),
            Some(Geometry::Point(Position::new(3.0, 4.0))),
        ];
        let (shp, _shx) = write(&geoms).unwrap();
        assert_eq!(f64_le(&shp, 84).unwrap(), 5.0); // Mmin
        assert_eq!(f64_le(&shp, 92).unwrap(), 5.0); // Mmax
    }

    #[test]
    fn bad_file_code_errors() {
        let mut bytes = vec![0u8; 100];
        bytes[0..4].copy_from_slice(&0i32.to_be_bytes());
        assert!(read(&bytes).is_err());
    }
}
