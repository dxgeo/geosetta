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
        gtype::POINT => Some(Geometry::Point([f64_le(content, 4)?, f64_le(content, 12)?])),
        gtype::MULTIPOINT => {
            let n = as_len(i32_le(content, 36)?, "MultiPoint numPoints")?;
            Some(Geometry::MultiPoint(read_points(content, 40, n)?))
        }
        gtype::POLYLINE => {
            let parts = read_parts(content)?;
            Some(if parts.len() == 1 {
                Geometry::LineString(parts.into_iter().next().unwrap())
            } else {
                Geometry::MultiLineString(parts)
            })
        }
        gtype::POLYGON => Some(classify_rings(read_parts(content)?)),
        other => return Err(Error::Convert(format!("shapefile: unsupported shape type {other} (Z/M/MultiPatch out of scope)"))),
    })
}

/// Split a PolyLine/Polygon body into its parts (rings/lines): bbox + numParts
/// + numPoints + part start-indices + a flat point array.
fn read_parts(content: &[u8]) -> Result<Vec<Vec<Position>>> {
    let num_parts = as_len(i32_le(content, 36)?, "numParts")?;
    let num_points = as_len(i32_le(content, 40)?, "numPoints")?;
    let parts_off = 44;
    let mut starts = Vec::with_capacity(num_parts);
    for i in 0..num_parts {
        starts.push(as_len(i32_le(content, parts_off + 4 * i)?, "part start index")?);
    }
    let points = read_points(content, parts_off + 4 * num_parts, num_points)?;

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
        out.push([f64_le(content, offset + 16 * i)?, f64_le(content, offset + 16 * i + 8)?]);
    }
    Ok(out)
}

// --- ring classification (Polygon vs MultiPolygon) ----------------------

/// Signed area via the shoelace formula: positive => counter-clockwise
/// (a hole), negative => clockwise (a shell).
fn signed_area(ring: &[Position]) -> f64 {
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let [x0, y0] = ring[i];
        let [x1, y1] = ring[(i + 1) % ring.len()];
        sum += x0 * y1 - x1 * y0;
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
    let shape_type = header_shape_type(geometries)?;

    let mut bbox = Bbox::empty();
    for g in geometries.iter().flatten() {
        g.extend_bbox(&mut bbox);
    }

    let mut shp = vec![0u8; 100];
    let mut shx = vec![0u8; 100];
    for (i, g) in geometries.iter().enumerate() {
        let content = encode_shape(g.as_ref())?;
        let content_words = (content.len() / 2) as i32;
        let record_offset_words = (shp.len() / 2) as i32;

        shp.extend_from_slice(&((i + 1) as i32).to_be_bytes());
        shp.extend_from_slice(&content_words.to_be_bytes());
        shp.extend_from_slice(&content);

        shx.extend_from_slice(&record_offset_words.to_be_bytes());
        shx.extend_from_slice(&content_words.to_be_bytes());
    }

    write_header(&mut shp, shape_type, &bbox);
    write_header(&mut shx, shape_type, &bbox);
    Ok((shp, shx))
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

fn encode_shape(g: Option<&Geometry>) -> Result<Vec<u8>> {
    let Some(g) = g else {
        return Ok(gtype::NULL.to_le_bytes().to_vec());
    };
    Ok(match g {
        Geometry::Point(p) => encode_point(*p),
        Geometry::MultiPoint(ps) => encode_multipoint(ps),
        Geometry::LineString(ps) => encode_parts(gtype::POLYLINE, std::slice::from_ref(ps)),
        Geometry::MultiLineString(parts) => encode_parts(gtype::POLYLINE, parts),
        Geometry::Polygon(rings) => encode_parts(gtype::POLYGON, &polygon_rings(rings)),
        Geometry::MultiPolygon(polys) => {
            let rings: Vec<Vec<Position>> = polys.iter().flat_map(|r| polygon_rings(r)).collect();
            encode_parts(gtype::POLYGON, &rings)
        }
        Geometry::GeometryCollection(_) => unreachable!("rejected by base_shape_type"),
    })
}

fn encode_point(p: Position) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.extend_from_slice(&gtype::POINT.to_le_bytes());
    out.extend_from_slice(&p[0].to_le_bytes());
    out.extend_from_slice(&p[1].to_le_bytes());
    out
}

fn encode_multipoint(points: &[Position]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&gtype::MULTIPOINT.to_le_bytes());
    write_bbox_le(&mut out, &points_bbox(points));
    out.extend_from_slice(&(points.len() as i32).to_le_bytes());
    for p in points {
        out.extend_from_slice(&p[0].to_le_bytes());
        out.extend_from_slice(&p[1].to_le_bytes());
    }
    out
}

fn encode_parts(shape_type: i32, parts: &[Vec<Position>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&shape_type.to_le_bytes());
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
        out.extend_from_slice(&p[0].to_le_bytes());
        out.extend_from_slice(&p[1].to_le_bytes());
    }
    out
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

fn write_header(buf: &mut [u8], shape_type: i32, bbox: &Bbox) {
    let file_len_words = (buf.len() / 2) as i32;
    buf[0..4].copy_from_slice(&9994i32.to_be_bytes());
    buf[24..28].copy_from_slice(&file_len_words.to_be_bytes());
    buf[28..32].copy_from_slice(&1000i32.to_le_bytes());
    buf[32..36].copy_from_slice(&shape_type.to_le_bytes());
    let (xmin, ymin, xmax, ymax) =
        if bbox.is_empty() { (0.0, 0.0, 0.0, 0.0) } else { (bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y) };
    for (i, v) in [xmin, ymin, xmax, ymax, 0.0, 0.0, 0.0, 0.0].into_iter().enumerate() {
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
        let geoms = vec![Some(Geometry::Point([1.5, -2.5])), None, Some(Geometry::Point([0.0, 0.0]))];
        assert_eq!(round_trip(geoms.clone()), geoms);
    }

    #[test]
    fn multipoint_round_trips() {
        let geoms = vec![Some(Geometry::MultiPoint(vec![[0.0, 0.0], [1.0, 1.0], [2.0, -1.0]]))];
        assert_eq!(round_trip(geoms.clone()), geoms);
    }

    #[test]
    fn linestring_and_multilinestring_round_trip() {
        let line = Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]);
        let multi = Geometry::MultiLineString(vec![
            vec![[0.0, 0.0], [1.0, 0.0]],
            vec![[5.0, 5.0], [6.0, 6.0], [7.0, 5.0]],
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
        let shell = vec![[0.0, 0.0], [0.0, 4.0], [4.0, 4.0], [4.0, 0.0], [0.0, 0.0]]; // CW
        let hole = vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]]; // CCW
        let poly = Geometry::Polygon(vec![shell, hole]);
        assert_eq!(round_trip(vec![Some(poly.clone())]), vec![Some(poly)]);
    }

    #[test]
    fn multipolygon_two_shells_round_trips() {
        let a = vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]; // CW
        let b = vec![[5.0, 5.0], [5.0, 6.0], [6.0, 6.0], [6.0, 5.0], [5.0, 5.0]]; // CW
        let multi = Geometry::MultiPolygon(vec![vec![a], vec![b]]);
        assert_eq!(round_trip(vec![Some(multi.clone())]), vec![Some(multi)]);
    }

    #[test]
    fn multipolygon_with_hole_in_one_shell_round_trips() {
        let shell1 = vec![[0.0, 0.0], [0.0, 4.0], [4.0, 4.0], [4.0, 0.0], [0.0, 0.0]]; // CW
        let hole1 = vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]]; // CCW
        let shell2 = vec![[10.0, 10.0], [10.0, 11.0], [11.0, 11.0], [11.0, 10.0], [10.0, 10.0]]; // CW
        let multi = Geometry::MultiPolygon(vec![vec![shell1, hole1], vec![shell2]]);
        assert_eq!(round_trip(vec![Some(multi.clone())]), vec![Some(multi)]);
    }

    #[test]
    fn ring_classification_is_correct_independent_of_file_io() {
        // Shell CW, hole CCW (by construction), a disjoint second shell CW.
        let shell = vec![[0.0, 0.0], [0.0, 4.0], [4.0, 4.0], [4.0, 0.0], [0.0, 0.0]]; // CW
        assert!(is_clockwise(&shell));
        let hole = vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]]; // CCW
        assert!(!is_clockwise(&hole));
        let shell2 = vec![[10.0, 0.0], [10.0, 4.0], [14.0, 4.0], [14.0, 0.0], [10.0, 0.0]]; // CW

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
        let shell = vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        match classify_rings(vec![shell.clone()]) {
            Geometry::Polygon(rings) => assert_eq!(rings, vec![shell]),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn mixed_shape_families_error_on_write() {
        let geoms = vec![Some(Geometry::Point([0.0, 0.0])), Some(Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0]]))];
        assert!(write(&geoms).is_err());
    }

    #[test]
    fn geometry_collection_errors_on_write() {
        let geoms = vec![Some(Geometry::GeometryCollection(vec![Geometry::Point([0.0, 0.0])]))];
        assert!(write(&geoms).is_err());
    }

    #[test]
    fn bad_file_code_errors() {
        let mut bytes = vec![0u8; 100];
        bytes[0..4].copy_from_slice(&0i32.to_be_bytes());
        assert!(read(&bytes).is_err());
    }
}
