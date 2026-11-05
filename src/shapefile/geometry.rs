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
use crate::geometry::{Bbox, Position};
#[cfg(test)]
use crate::geometry::Geometry;
use crate::table::{GeometryBuilder, GeometryRef, Kind, Rings, Run, Shape};

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

/// A `.shp` file's records' contents, one per record, in file order. Reads
/// sequentially to EOF using each record's own content-length prefix --
/// `.shx` is not consulted. [`decode_into`] decodes each.
pub fn records(data: &[u8]) -> Result<Vec<&[u8]>> {
    if data.len() < 100 {
        return Err(Error::Convert("shapefile: .shp file shorter than its header".into()));
    }
    if i32_be(data, 0)? != 9994 {
        return Err(Error::Convert("shapefile: bad .shp file code (expected 9994)".into()));
    }

    let mut records = Vec::new();
    let mut pos = 100usize;
    while pos + 8 <= data.len() {
        let content_len = as_len(i32_be(data, pos + 4)?, "record content length")? * 2;
        let content = data
            .get(pos + 8..pos + 8 + content_len)
            .ok_or_else(|| Error::Convert("shapefile: truncated .shp record".into()))?;
        records.push(content);
        pos += 8 + content_len;
    }
    Ok(records)
}

/// Parse a `.shp` file into one `Option<Geometry>` per record, in file order
/// (`None` for a Null-shape record).
#[cfg(test)]
pub fn read(data: &[u8]) -> Result<Vec<Option<Geometry>>> {
    let mut built = GeometryBuilder::default();
    records(data)?
        .into_iter()
        .map(|content| {
            decode_into(content, &mut built)?;
            Ok(built.take())
        })
        .collect()
}

/// Decode one record's content into `out` (`plans/columnar.org` C6 step 5):
/// the positions go straight into the buffers a block's flat column copies,
/// with no `Geometry` built. A Null shape leaves it empty.
pub fn decode_into(content: &[u8], out: &mut GeometryBuilder) -> Result<()> {
    out.clear();
    let shape_type = i32_le(content, 0)?;
    match shape_type {
        gtype::NULL => {}
        // `POINT_M`'s M sits exactly where a plain `POINT` record simply
        // ends (offset 20), so a real 2D `POINT` record (20 bytes, no room
        // for `content.len() >= 28`) and a real `POINT_M` record share this
        // one arm safely — `m_or_none` only ever fires when the bytes exist.
        gtype::POINT | gtype::POINT_M => {
            let x = f64_le(content, 4)?;
            let y = f64_le(content, 12)?;
            let m = if content.len() >= 28 { m_or_none(f64_le(content, 20)?) } else { None };
            out.point(Position { x, y, z: None, m });
        }
        gtype::POINT_Z => {
            let x = f64_le(content, 4)?;
            let y = f64_le(content, 12)?;
            let z = f64_le(content, 20)?;
            let m = if content.len() >= 36 { m_or_none(f64_le(content, 28)?) } else { None };
            out.point(Position { x, y, z: Some(z), m });
        }
        gtype::MULTIPOINT | gtype::MULTIPOINT_M | gtype::MULTIPOINT_Z => {
            let has_z = shape_type == gtype::MULTIPOINT_Z;
            let what = if has_z { "MultiPointZ numPoints" } else { "MultiPoint numPoints" };
            let n = backed_len(content, 36, 40, 16, what)?;
            let points = Points::new(content, 40, n, has_z)?;
            for i in 0..n {
                out.position(points.get(i)?);
            }
            out.end_ring();
            out.end_part();
            out.finish(Kind::MultiPoint);
        }
        gtype::POLYLINE | gtype::POLYLINE_M | gtype::POLYLINE_Z => {
            let parts = parts_into(content, shape_type == gtype::POLYLINE_Z, out, |_| {})?;
            out.end_part();
            out.finish(if parts == 1 { Kind::LineString } else { Kind::MultiLineString });
        }
        gtype::POLYGON | gtype::POLYGON_M | gtype::POLYGON_Z => {
            // Ring classification (Polygon vs MultiPolygon), by winding and
            // appearance order: a clockwise ring starts a new shell; a
            // counter-clockwise ring joins the most recent shell as a hole
            // (or starts one itself if none precedes it — a malformed input,
            // handled gracefully rather than panicking).
            parts_into(content, shape_type == gtype::POLYGON_Z, out, |out| {
                let first = out.parts() == 0 && out.rings_in_open_part() == 0;
                if !first && is_clockwise_xy(out.open_ring()) {
                    out.end_part();
                }
            })?;
            out.end_part();
            out.finish(if out.parts() <= 1 { Kind::Polygon } else { Kind::MultiPolygon });
        }
        other => return Err(Error::Convert(format!("shapefile: unsupported shape type {other} (MultiPatch out of scope)"))),
    }
    Ok(())
}

/// A record's points: X and Y at `xy`, and a Z and an M section after them
/// where the record has one.
struct Points<'a> {
    content: &'a [u8],
    xy: usize,
    /// The Z array, past `Zmin`/`Zmax`.
    z: Option<usize>,
    /// The M array, past `Mmin`/`Mmax`, when the record has room for one.
    m: Option<usize>,
}

impl<'a> Points<'a> {
    /// `n` points from `xy`, checked to lie within the record: the same
    /// checks, in the same order, as reading every point would make.
    ///
    /// An M section (present or not, regardless of `has_z` — the M-only
    /// shape types share this same tail shape minus the Z section) is
    /// detected purely by remaining content length, not by the declared
    /// shape type, confirmed via real `ogr2ogr`-written `.shp` bytes: an
    /// M-bearing WKT source writes the full section, while a Z-only
    /// (unmeasured) source writes zero trailing bytes at all, not a
    /// sentinel-filled placeholder.
    fn new(content: &'a [u8], xy: usize, n: usize, has_z: bool) -> Result<Points<'a>> {
        let mut after = xy + 16 * n;
        if n > 0 {
            f64_le(content, after - 8)?;
        }
        let z = has_z.then(|| after + 16); // skip Zmin/Zmax
        if let Some(z) = z {
            if n > 0 {
                f64_le(content, z + 8 * (n - 1))?;
            }
            after += 16 + 8 * n;
        }
        let m = (content.len() >= after + 16 + 8 * n).then_some(after + 16); // skip Mmin/Mmax
        Ok(Points { content, xy, z, m })
    }

    /// Point `i`. A raw M at or below the ESRI "no data" threshold is
    /// `None` even when the section is present, marking that one point
    /// unmeasured within an otherwise M-bearing shape.
    fn get(&self, i: usize) -> Result<Position> {
        Ok(Position {
            x: f64_le(self.content, self.xy + 16 * i)?,
            y: f64_le(self.content, self.xy + 16 * i + 8)?,
            z: self.z.map(|z| f64_le(self.content, z + 8 * i)).transpose()?,
            m: self.m.map(|m| f64_le(self.content, m + 8 * i).map(m_or_none)).transpose()?.flatten(),
        })
    }
}

/// Write a PolyLine/Polygon body's parts (rings/lines) into `out`, each a
/// ring, returning how many: bbox, numParts, numPoints, part
/// start-indices, and a flat point array, plus (if `has_z`) a trailing
/// Zmin/Zmax/Z-array section — same layout for both the 2D and Z shape
/// types, confirmed via real `ogr2ogr`-written `.shp` bytes before
/// implementing (see M7 of `plans/zm-geometry.org`). `before_close` sees
/// each ring's positions written and the ring not yet closed.
fn parts_into(
    content: &[u8],
    has_z: bool,
    out: &mut GeometryBuilder,
    mut before_close: impl FnMut(&mut GeometryBuilder),
) -> Result<usize> {
    let num_parts = backed_len(content, 36, 44, 4, "numParts")?;
    let parts_off = 44;
    let num_points = backed_len(content, 40, parts_off + 4 * num_parts, 16, "numPoints")?;
    let mut starts = Vec::with_capacity(num_parts);
    for i in 0..num_parts {
        starts.push(as_len(i32_le(content, parts_off + 4 * i)?, "part start index")?);
    }
    let points = Points::new(content, parts_off + 4 * num_parts, num_points, has_z)?;
    for i in 0..num_parts {
        let start = starts[i];
        let end = starts.get(i + 1).copied().unwrap_or(num_points);
        if start > end || end > num_points {
            return Err(Error::Convert("shapefile: part index out of range".into()));
        }
        for k in start..end {
            out.position(points.get(k)?);
        }
        before_close(out);
        out.end_ring();
    }
    Ok(num_parts)
}

// --- ring winding --------------------------------------------------------

/// Signed area via the shoelace formula over `n` points, point `i` at
/// `at(i)`: positive => counter-clockwise (a hole), negative => clockwise
/// (a shell).
fn signed_area(n: usize, at: impl Fn(usize) -> (f64, f64)) -> f64 {
    let mut sum = 0.0;
    for i in 0..n {
        let (x0, y0) = at(i);
        let (x1, y1) = at((i + 1) % n);
        sum += x0 * y1 - x1 * y0;
    }
    sum / 2.0
}

fn is_clockwise(ring: &[Position]) -> bool {
    signed_area(ring.len(), |i| (ring[i].x, ring[i].y)) < 0.0
}

fn is_clockwise_xy(ring: &[[f64; 2]]) -> bool {
    signed_area(ring.len(), |i| (ring[i][0], ring[i][1])) < 0.0
}

// --- writer --------------------------------------------------------------

/// Encode geometries (in order) as a `.shp`/`.shx` pair. Errors if the
/// geometries mix incompatible Shapefile shape families (e.g. Point and
/// Polygon) — a single `.shp` can only declare one shape type — or contain a
/// `GeometryCollection`, which Shapefile cannot represent at all.
///
/// Read through views, so a flat block is written from its buffers: the
/// writer holds a view a row, not a copy of every geometry
/// (`plans/columnar.org` C6 step 4).
pub fn write(geometries: &[Option<GeometryRef<'_>>]) -> Result<(Vec<u8>, Vec<u8>)> {
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
    let has_z = geometries.iter().flatten().any(|g| geometry_has_z(*g));
    let has_m = geometries.iter().flatten().any(|g| geometry_has_m(*g));
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
        let content = encode_shape(*g, has_z, has_m)?;
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
    geometries: &[Option<GeometryRef<'_>>],
    present: bool,
    fold: impl Fn(GeometryRef<'_>, &mut f64, &mut f64),
) -> (f64, f64) {
    if !present {
        return (0.0, 0.0);
    }
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for g in geometries.iter().flatten() {
        fold(*g, &mut lo, &mut hi);
    }
    (lo, hi)
}

/// Every position of `g`, a collection's members' included, in order.
fn positions(g: GeometryRef<'_>, f: &mut impl FnMut(Position)) {
    match g.shape() {
        Shape::Point(p) => f(p),
        Shape::LineString(ps) | Shape::MultiPoint(ps) => ps.iter().for_each(f),
        Shape::Polygon(rings) | Shape::MultiLineString(rings) => rings.iter().flat_map(Run::iter).for_each(f),
        Shape::MultiPolygon(polys) => polys.iter().flat_map(Rings::iter).flat_map(Run::iter).for_each(f),
        Shape::GeometryCollection(geoms) => geoms.iter().for_each(|g| positions(g, f)),
    }
}

/// Whether `g` holds a position satisfying `test`.
fn any_position(g: GeometryRef<'_>, test: impl Fn(Position) -> bool) -> bool {
    let mut found = false;
    positions(g, &mut |p| found |= test(p));
    found
}

/// Whether any position in `g` carries Z — used to decide, file-wide,
/// whether to promote the whole `.shp` to its Z-variant shape type.
fn geometry_has_z(g: GeometryRef<'_>) -> bool {
    any_position(g, |p| p.z.is_some())
}

/// Whether any position in `g` carries M — used to decide, file-wide,
/// whether to promote the whole `.shp` to an M-bearing shape type (either
/// the Z-variant with an extra M section, or the M-only variant if Z is
/// absent — see `write()`).
fn geometry_has_m(g: GeometryRef<'_>) -> bool {
    any_position(g, |p| p.m.is_some())
}

/// Fold `g`'s Z values (0.0 fallback for a position missing Z) into a
/// running `[lo, hi]` range — used for the file header's Zmin/Zmax, matching
/// `extend_bbox`'s own traversal shape but kept independent since `Bbox`
/// itself stays 2D-only (see `plans/zm-geometry.org`'s OPEN QUESTIONS).
fn fold_z(g: GeometryRef<'_>, lo: &mut f64, hi: &mut f64) {
    positions(g, &mut |p| {
        let z = p.z.unwrap_or(0.0);
        *lo = lo.min(z);
        *hi = hi.max(z);
    });
}

/// Fold `g`'s M values into a running `[lo, hi]` range — *skipping* any
/// position missing M entirely, rather than defaulting it to a value, since
/// an unmeasured point shouldn't drag the header's summary range toward the
/// sentinel; matches ESRI's own convention that only genuinely measured
/// points extend Mmin/Mmax.
fn fold_m(g: GeometryRef<'_>, lo: &mut f64, hi: &mut f64) {
    positions(g, &mut |p| {
        if let Some(m) = p.m {
            *lo = lo.min(m);
            *hi = hi.max(m);
        }
    });
}

fn header_shape_type(geometries: &[Option<GeometryRef<'_>>]) -> Result<i32> {
    let mut found: Option<i32> = None;
    for g in geometries.iter().flatten() {
        let t = base_shape_type(g.shape())?;
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

fn base_shape_type(g: Shape<'_>) -> Result<i32> {
    Ok(match g {
        Shape::Point(_) => gtype::POINT,
        Shape::MultiPoint(_) => gtype::MULTIPOINT,
        Shape::LineString(_) | Shape::MultiLineString(_) => gtype::POLYLINE,
        Shape::Polygon(_) | Shape::MultiPolygon(_) => gtype::POLYGON,
        Shape::GeometryCollection(_) => {
            return Err(Error::Convert("shapefile: GeometryCollection cannot be represented in a .shp".into()));
        }
    })
}

/// A part of a record, and the winding it is written in: `Some(clockwise)`
/// for a polygon's ring -- a shell (index 0) clockwise, a hole
/// counter-clockwise, whatever the source's winding, since Geosetta controls
/// what it emits -- and `None` for a line, written as it is.
type Part<'a> = (Run<'a>, Option<bool>);

/// A polygon's rings, each with the winding Shapefile wants for it.
fn polygon_parts(rings: Rings<'_>) -> impl Iterator<Item = Part<'_>> {
    rings.iter().enumerate().map(|(i, r)| (r, Some(i == 0)))
}

fn encode_shape(g: Option<GeometryRef<'_>>, has_z: bool, has_m: bool) -> Result<Vec<u8>> {
    let Some(g) = g else {
        return Ok(gtype::NULL.to_le_bytes().to_vec());
    };
    Ok(match g.shape() {
        Shape::Point(p) => encode_point(p, has_z, has_m),
        Shape::MultiPoint(ps) => encode_multipoint(&ps.iter().collect::<Vec<_>>(), has_z, has_m),
        Shape::LineString(ps) => encode_parts(gtype::POLYLINE, [(ps, None)], has_z, has_m),
        Shape::MultiLineString(parts) => encode_parts(gtype::POLYLINE, parts.iter().map(|l| (l, None)), has_z, has_m),
        Shape::Polygon(rings) => encode_parts(gtype::POLYGON, polygon_parts(rings), has_z, has_m),
        Shape::MultiPolygon(polys) => encode_parts(gtype::POLYGON, polys.iter().flat_map(polygon_parts), has_z, has_m),
        Shape::GeometryCollection(_) => unreachable!("rejected by base_shape_type"),
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

fn encode_parts<'a>(shape_type: i32, parts: impl IntoIterator<Item = Part<'a>>, has_z: bool, has_m: bool) -> Vec<u8> {
    // The record's points, end to end, each ring in the winding it is
    // written in, and where each part starts among them.
    let mut all_points: Vec<Position> = Vec::new();
    let mut starts: Vec<i32> = Vec::new();
    for (run, clockwise) in parts {
        let start = all_points.len();
        starts.push(start as i32);
        all_points.extend(run.iter());
        if clockwise.is_some_and(|cw| is_clockwise(&all_points[start..]) != cw) {
            all_points[start..].reverse();
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&dim_shape_type(shape_type, has_z, has_m).to_le_bytes());
    write_bbox_le(&mut out, &points_bbox(&all_points));
    out.extend_from_slice(&(starts.len() as i32).to_le_bytes());
    out.extend_from_slice(&(all_points.len() as i32).to_le_bytes());
    for start in &starts {
        out.extend_from_slice(&start.to_le_bytes());
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

/// The count stored at `count_at`, rejected unless the record holds that
/// many `size`-byte items starting at `items_at`. Checked before anything is
/// sized from it: trusting `numParts`/`numPoints` let one 52-byte record
/// request `i32::MAX` of each -- a "capacity overflow" panic on wasm32.
fn backed_len(content: &[u8], count_at: usize, items_at: usize, size: usize, what: &str) -> Result<usize> {
    let n = as_len(i32_le(content, count_at)?, what)?;
    match n.checked_mul(size).and_then(|bytes| bytes.checked_add(items_at)) {
        Some(end) if end <= content.len() => Ok(n),
        _ => Err(Error::Convert(format!("shapefile: {what} ({n}) runs past the end of its .shp record"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::write`], from owned geometries.
    fn write(geoms: &[Option<Geometry>]) -> Result<(Vec<u8>, Vec<u8>)> {
        super::write(&geoms.iter().map(|g| g.as_ref().map(GeometryRef::owned)).collect::<Vec<_>>())
    }

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

    /// `rings`, in their own winding, as a Polygon record's parts, read
    /// back: what the reader's ring classification makes of them.
    fn classify_rings(rings: Vec<Vec<Position>>) -> Geometry {
        let content = encode_parts(gtype::POLYGON, rings.iter().map(|r| (Run::slice(r), None)), false, false);
        let mut built = GeometryBuilder::default();
        decode_into(&content, &mut built).unwrap();
        built.take().unwrap()
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

    #[test]
    fn rejects_counts_the_record_cannot_hold_before_allocating() {
        // One record with a valid header, then a count claiming i32::MAX
        // items and no items behind it. Each used to size a `Vec` from the
        // count first: a "capacity overflow" panic on wasm32.
        fn shp(shape_type: i32, body: &[u8]) -> Vec<u8> {
            let mut content = shape_type.to_le_bytes().to_vec();
            content.extend_from_slice(body);
            let mut out = vec![0u8; 100];
            out[0..4].copy_from_slice(&9994i32.to_be_bytes());
            out.extend_from_slice(&1i32.to_be_bytes());
            out.extend_from_slice(&((content.len() / 2) as i32).to_be_bytes());
            out.extend_from_slice(&content);
            out
        }
        // Natively the old code also errored, but only after the allocation,
        // so check for the up-front check's own error.
        fn assert_rejected_up_front(r: Result<Vec<Option<Geometry>>>, what: &str) {
            let e = r.unwrap_err().to_string();
            assert!(e.contains(what) && e.contains("runs past the end"), "{e}");
        }
        let max = i32::MAX.to_le_bytes();
        let bbox = [0u8; 32];
        let parts_and_points = |parts: &[u8; 4], points: &[u8; 4]| [&bbox[..], parts, points].concat();
        for shape_type in [gtype::POLYLINE, gtype::POLYGON, gtype::POLYGON_Z] {
            let huge_parts = shp(shape_type, &parts_and_points(&max, &0i32.to_le_bytes()));
            assert_rejected_up_front(read(&huge_parts), "numParts");
            let huge_points = shp(shape_type, &[parts_and_points(&1i32.to_le_bytes(), &max), vec![0; 4]].concat());
            assert_rejected_up_front(read(&huge_points), "numPoints");
        }
        for shape_type in [gtype::MULTIPOINT, gtype::MULTIPOINT_Z] {
            let huge = shp(shape_type, &[&bbox[..], &max].concat());
            assert_rejected_up_front(read(&huge), "numPoints");
        }
        // A count the record does hold still reads.
        let one_part = shp(gtype::POLYLINE, &[parts_and_points(&1i32.to_le_bytes(), &2i32.to_le_bytes()), vec![0; 4 + 32]].concat());
        assert_eq!(read(&one_part).unwrap().len(), 1);
    }
}
