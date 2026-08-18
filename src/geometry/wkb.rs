//! Well-Known Binary (WKB) encoding of [`Geometry`], the encoding GeoParquet
//! uses for its geometry column.
//!
//! Output is little-endian. 2D (XY), Z (elevation), M (measure), and ZM are
//! all supported: a geometry's dimensionality is read off its first
//! position's `z`/`m` fields at encode time and folded into the WKB type
//! code per the ISO SFA convention — base type code plus 1000 for Z, 2000
//! for M, or 3000 for both (e.g. `PointZ` = 1001, `PointZM` = 3001) — rather
//! than PostGIS's EWKB high-bit-flag convention (`0x80000000`/`0x40000000`).
//! Confirmed, not assumed, against two independent real tools before
//! implementing: DuckDB's `ST_AsWKB(ST_GeomFromText('POINT Z (1 2 3)'))` and
//! GDAL's `ogr.Geometry.ExportToIsoWkb()` both emit type code `1001`
//! (`0xE9030000` little-endian) for a 3D point — GDAL's plain
//! `ExportToWkb()` (EWKB-style, `0x80000001`) does not match and is not
//! what GeoParquet producers emit. Multi-geometries and geometry
//! collections embed complete WKB sub-geometries, each with their own
//! byte-order marker and (independently dimension-suffixed) type code, per
//! the OGC specification — verified the same way: `MULTIPOINT Z (...)`'s
//! outer type code (1004) and each embedded sub-point's own type code
//! (1001) are both Z-suffixed.

use super::{Geometry, Position};
use crate::error::{Error, Result};

// Little-endian byte-order marker.
const LE: u8 = 0x01;

// Base (2D) geometry type codes. A written/read type code is one of these
// plus a `Dim::offset()` (0/1000/2000/3000) for Z/M/ZM.
const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOINT: u32 = 4;
const WKB_MULTILINESTRING: u32 = 5;
const WKB_MULTIPOLYGON: u32 = 6;
const WKB_GEOMETRYCOLLECTION: u32 = 7;

/// A position's dimensionality: whether it carries Z and/or M, matching one
/// of WKB's four ISO SFA type-code suffixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dim {
    z: bool,
    m: bool,
}

impl Dim {
    const XY: Dim = Dim { z: false, m: false };

    /// The dimensionality a position implies: `Some(_)` on `z`/`m` means
    /// that ordinate is present.
    fn of(p: Position) -> Self {
        Dim { z: p.z.is_some(), m: p.m.is_some() }
    }

    /// The ISO SFA type-code offset for this dimensionality.
    fn offset(self) -> u32 {
        match (self.z, self.m) {
            (false, false) => 0,
            (true, false) => 1000,
            (false, true) => 2000,
            (true, true) => 3000,
        }
    }

    /// The inverse of [`Self::offset`]: recover a dimensionality from a
    /// type code's thousands-offset. Errors on an offset that isn't one of
    /// the four ISO SFA values (a malformed or EWKB-flagged type code).
    fn from_offset(offset: u32) -> Result<Self> {
        match offset {
            0 => Ok(Dim::XY),
            1000 => Ok(Dim { z: true, m: false }),
            2000 => Ok(Dim { z: false, m: true }),
            3000 => Ok(Dim { z: true, m: true }),
            other => Err(Error::Convert(format!(
                "unknown WKB dimension offset {other} (expected 0, 1000, 2000, or 3000 — EWKB-style high-bit flags are not supported)"
            ))),
        }
    }

    /// Bytes per coordinate at this dimensionality: X/Y always, plus Z
    /// and/or M.
    fn coord_size(self) -> usize {
        16 + if self.z { 8 } else { 0 } + if self.m { 8 } else { 0 }
    }
}

/// The dimensionality implied by a `Vec<Position>`'s first element, or 2D
/// if empty — used to pick one WKB type-code suffix for an entire
/// `LineString`/`MultiPoint`/ring list, matching how WKB declares
/// dimensionality once per geometry rather than per point.
fn dim_of_first(ps: &[Position]) -> Dim {
    ps.first().copied().map(Dim::of).unwrap_or(Dim::XY)
}

/// The dimensionality implied by a ring list's first position (its first
/// ring's first point), or 2D if empty.
fn dim_of_rings(rings: &[Vec<Position>]) -> Dim {
    rings.first().map(|r| dim_of_first(r)).unwrap_or(Dim::XY)
}

/// The dimensionality of a geometry's very first position, found by
/// recursing into whichever variant it is — used for a `GeometryCollection`
/// wrapper's own type code (its members each carry their own independently).
fn dim_of_geometry(g: &Geometry) -> Dim {
    first_position(g).map(Dim::of).unwrap_or(Dim::XY)
}

fn first_position(g: &Geometry) -> Option<Position> {
    match g {
        Geometry::Point(p) => Some(*p),
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => ps.first().copied(),
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            rings.first().and_then(|r| r.first()).copied()
        }
        Geometry::MultiPolygon(polys) => {
            polys.first().and_then(|p| p.first()).and_then(|r| r.first()).copied()
        }
        Geometry::GeometryCollection(geoms) => geoms.first().and_then(first_position),
    }
}

/// Encode a geometry to a fresh WKB byte buffer.
pub fn encode(geom: &Geometry) -> Vec<u8> {
    // Pre-size the buffer to the exact byte length so the write never reallocs
    // (one `Vec` per feature on every GeoParquet/FlatGeobuf write).
    let mut out = Vec::with_capacity(wkb_size(geom));
    write_geometry(&mut out, geom);
    out
}

/// Exact WKB byte length of `geom`, mirroring [`write_geometry`].
fn wkb_size(geom: &Geometry) -> usize {
    const HEADER: usize = 5; // LE marker + u32 type code
    const COUNT: usize = 4; // u32 length prefix
    let ring = |r: &Vec<Position>, dim: Dim| COUNT + r.len() * dim.coord_size();
    let poly = |rings: &[Vec<Position>]| {
        let dim = dim_of_rings(rings);
        HEADER + COUNT + rings.iter().map(|r| ring(r, dim)).sum::<usize>()
    };
    match geom {
        Geometry::Point(p) => HEADER + Dim::of(*p).coord_size(),
        Geometry::LineString(ps) => HEADER + COUNT + ps.len() * dim_of_first(ps).coord_size(),
        Geometry::Polygon(rings) => poly(rings),
        Geometry::MultiPoint(ps) => {
            HEADER + COUNT + ps.iter().map(|p| HEADER + Dim::of(*p).coord_size()).sum::<usize>()
        }
        Geometry::MultiLineString(lines) => {
            HEADER
                + COUNT
                + lines
                    .iter()
                    .map(|l| HEADER + COUNT + l.len() * dim_of_first(l).coord_size())
                    .sum::<usize>()
        }
        Geometry::MultiPolygon(polys) => {
            HEADER + COUNT + polys.iter().map(|p| poly(p)).sum::<usize>()
        }
        Geometry::GeometryCollection(geoms) => {
            HEADER + COUNT + geoms.iter().map(wkb_size).sum::<usize>()
        }
    }
}

fn write_geometry(out: &mut Vec<u8>, geom: &Geometry) {
    match geom {
        Geometry::Point(p) => {
            let dim = Dim::of(*p);
            header(out, WKB_POINT, dim);
            point(out, *p, dim);
        }
        Geometry::LineString(ps) => {
            let dim = dim_of_first(ps);
            header(out, WKB_LINESTRING, dim);
            line(out, ps, dim);
        }
        Geometry::Polygon(rings) => {
            header(out, WKB_POLYGON, dim_of_rings(rings));
            polygon(out, rings);
        }
        Geometry::MultiPoint(ps) => {
            header(out, WKB_MULTIPOINT, dim_of_first(ps));
            out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
            for p in ps {
                let dim = Dim::of(*p);
                header(out, WKB_POINT, dim);
                point(out, *p, dim);
            }
        }
        Geometry::MultiLineString(lines) => {
            let outer_dim = lines.first().map(|l| dim_of_first(l)).unwrap_or(Dim::XY);
            header(out, WKB_MULTILINESTRING, outer_dim);
            out.extend_from_slice(&(lines.len() as u32).to_le_bytes());
            for l in lines {
                let dim = dim_of_first(l);
                header(out, WKB_LINESTRING, dim);
                line(out, l, dim);
            }
        }
        Geometry::MultiPolygon(polys) => {
            let outer_dim = polys.first().map(|p| dim_of_rings(p)).unwrap_or(Dim::XY);
            header(out, WKB_MULTIPOLYGON, outer_dim);
            out.extend_from_slice(&(polys.len() as u32).to_le_bytes());
            for poly in polys {
                header(out, WKB_POLYGON, dim_of_rings(poly));
                polygon(out, poly);
            }
        }
        Geometry::GeometryCollection(geoms) => {
            header(out, WKB_GEOMETRYCOLLECTION, dim_of_geometry(geom));
            out.extend_from_slice(&(geoms.len() as u32).to_le_bytes());
            for g in geoms {
                write_geometry(out, g);
            }
        }
    }
}

/// Byte-order marker followed by the little-endian, dimension-suffixed type
/// code.
fn header(out: &mut Vec<u8>, base_type: u32, dim: Dim) {
    out.push(LE);
    out.extend_from_slice(&(base_type + dim.offset()).to_le_bytes());
}

fn point(out: &mut Vec<u8>, p: Position, dim: Dim) {
    out.extend_from_slice(&p.x.to_le_bytes());
    out.extend_from_slice(&p.y.to_le_bytes());
    // A position whose z/m the geometry's declared dimensionality expects
    // but which itself lacks (an inconsistent input — no reader constructs
    // this today) falls back to 0.0 rather than panicking or silently
    // shrinking the record; WKB has no per-point "no data" marker the way
    // Shapefile's M does.
    if dim.z {
        out.extend_from_slice(&p.z.unwrap_or(0.0).to_le_bytes());
    }
    if dim.m {
        out.extend_from_slice(&p.m.unwrap_or(0.0).to_le_bytes());
    }
}

/// A `u32` count of positions followed by the raw coordinate tuples, each at
/// `dim`'s width.
fn line(out: &mut Vec<u8>, ps: &[Position], dim: Dim) {
    out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
    for p in ps {
        point(out, *p, dim);
    }
}

/// A `u32` ring count, each ring encoded like a linear ring of positions at
/// the ring list's own (first-position-derived) dimensionality.
fn polygon(out: &mut Vec<u8>, rings: &[Vec<Position>]) {
    let dim = dim_of_rings(rings);
    out.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    for ring in rings {
        line(out, ring, dim);
    }
}

// --- decoding --------------------------------------------------------------

/// Decode a WKB geometry (the inverse of [`encode`]). Little-endian; 2D,
/// Z, M, and ZM are all supported (see the module docs for the ISO SFA
/// type-code convention). Multi-geometries and collections recurse into
/// their embedded sub-WKB.
pub fn decode(bytes: &[u8]) -> Result<Geometry> {
    let mut r = Reader { buf: bytes, pos: 0 };
    read_geometry(&mut r)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or_else(truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.buf.get(self.pos..self.pos + 4).ok_or_else(truncated)?;
        self.pos += 4;
        Ok(u32::from_le_bytes(s.try_into().unwrap()))
    }

    fn f64(&mut self) -> Result<f64> {
        let s = self.buf.get(self.pos..self.pos + 8).ok_or_else(truncated)?;
        self.pos += 8;
        Ok(f64::from_le_bytes(s.try_into().unwrap()))
    }
}

fn truncated() -> Error {
    Error::Convert("truncated WKB geometry".into())
}

fn read_geometry(r: &mut Reader) -> Result<Geometry> {
    let order = r.u8()?;
    if order != LE {
        return Err(Error::Convert(
            "only little-endian WKB is supported".into(),
        ));
    }
    let code = r.u32()?;
    let base_type = code % 1000;
    let dim = Dim::from_offset(code - base_type)?;
    let geom = match base_type {
        WKB_POINT => Geometry::Point(read_point(r, dim)?),
        WKB_LINESTRING => Geometry::LineString(read_line(r, dim)?),
        WKB_POLYGON => Geometry::Polygon(read_polygon(r, dim)?),
        WKB_MULTIPOINT => {
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                match read_geometry(r)? {
                    Geometry::Point(p) => v.push(p),
                    _ => return Err(sub_mismatch("MultiPoint", "Point")),
                }
            }
            Geometry::MultiPoint(v)
        }
        WKB_MULTILINESTRING => {
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                match read_geometry(r)? {
                    Geometry::LineString(l) => v.push(l),
                    _ => return Err(sub_mismatch("MultiLineString", "LineString")),
                }
            }
            Geometry::MultiLineString(v)
        }
        WKB_MULTIPOLYGON => {
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                match read_geometry(r)? {
                    Geometry::Polygon(p) => v.push(p),
                    _ => return Err(sub_mismatch("MultiPolygon", "Polygon")),
                }
            }
            Geometry::MultiPolygon(v)
        }
        WKB_GEOMETRYCOLLECTION => {
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(read_geometry(r)?);
            }
            Geometry::GeometryCollection(v)
        }
        other => return Err(Error::Convert(format!("unknown WKB type code {other}"))),
    };
    Ok(geom)
}

fn sub_mismatch(parent: &str, expected: &str) -> Error {
    Error::Convert(format!("{parent} sub-geometry is not a {expected}"))
}

fn read_point(r: &mut Reader, dim: Dim) -> Result<Position> {
    let x = r.f64()?;
    let y = r.f64()?;
    let z = if dim.z { Some(r.f64()?) } else { None };
    let m = if dim.m { Some(r.f64()?) } else { None };
    Ok(Position { x, y, z, m })
}

fn read_line(r: &mut Reader, dim: Dim) -> Result<Vec<Position>> {
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(read_point(r, dim)?);
    }
    Ok(v)
}

fn read_polygon(r: &mut Reader, dim: Dim) -> Result<Vec<Vec<Position>>> {
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(read_line(r, dim)?);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_point_byte_for_byte() {
        let wkb = encode(&Geometry::Point(Position::new(1.0, 2.0)));
        let mut expected = vec![0x01];
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&1.0f64.to_le_bytes());
        expected.extend_from_slice(&2.0f64.to_le_bytes());
        assert_eq!(wkb, expected);
        assert_eq!(wkb.len(), 21);
    }

    #[test]
    fn encodes_linestring_length() {
        // header(5) + count(4) + 3 points * 16 = 57
        let g = Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)]);
        assert_eq!(encode(&g).len(), 5 + 4 + 3 * 16);
    }

    #[test]
    fn encodes_polygon_with_hole_length() {
        // header(5) + ringcount(4) + 2 rings, each count(4)+5 pts*16
        let outer = vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 4.0), Position::new(0.0, 0.0)];
        let hole = vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)];
        let g = Geometry::Polygon(vec![outer, hole]);
        assert_eq!(encode(&g).len(), 5 + 4 + 2 * (4 + 5 * 16));
    }

    fn round_trip(g: Geometry) {
        assert_eq!(decode(&encode(&g)).unwrap(), g);
    }

    #[test]
    fn decodes_every_variant() {
        round_trip(Geometry::Point(Position::new(-73.9857, 40.7484)));
        round_trip(Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)]));
        round_trip(Geometry::Polygon(vec![
            vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 4.0), Position::new(0.0, 0.0)],
            vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)],
        ]));
        round_trip(Geometry::MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]));
        round_trip(Geometry::MultiLineString(vec![
            vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)],
            vec![Position::new(2.0, 2.0), Position::new(3.0, 3.0)],
        ]));
        round_trip(Geometry::MultiPolygon(vec![vec![vec![
            Position::new(0.0, 0.0),
            Position::new(1.0, 0.0),
            Position::new(1.0, 1.0),
            Position::new(0.0, 0.0),
        ]]]));
        round_trip(Geometry::GeometryCollection(vec![
            Geometry::Point(Position::new(5.0, 6.0)),
            Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]),
        ]));
    }

    #[test]
    fn wkb_size_matches_encoded_length() {
        // The pre-sized capacity must equal the actual byte length for every
        // variant, so `encode` never reallocs.
        for g in [
            Geometry::Point(Position::new(-73.9857, 40.7484)),
            Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)]),
            Geometry::Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 0.0)],
                vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 1.0)],
            ]),
            Geometry::MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]),
            Geometry::MultiLineString(vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)], vec![Position::new(2.0, 2.0)]]),
            Geometry::MultiPolygon(vec![vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0), Position::new(1.0, 1.0), Position::new(0.0, 0.0)]]]),
            Geometry::GeometryCollection(vec![
                Geometry::Point(Position::new(5.0, 6.0)),
                Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]),
            ]),
        ] {
            assert_eq!(wkb_size(&g), encode(&g).len(), "size mismatch for {g:?}");
        }
    }

    #[test]
    fn rejects_truncated_and_unknown() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0x01, 0x01, 0x00]).is_err()); // point, missing coords
        // Big-endian byte order marker is unsupported.
        assert!(decode(&[0x00, 0x00, 0x00, 0x00, 0x01]).is_err());
        // Unknown type code 99.
        assert!(decode(&[0x01, 99, 0x00, 0x00, 0x00]).is_err());
    }

    #[test]
    fn multipoint_embeds_sub_wkb() {
        // header(5) + count(4) + 2 * point-wkb(21)
        let g = Geometry::MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]);
        let wkb = encode(&g);
        assert_eq!(wkb.len(), 5 + 4 + 2 * 21);
        // Type code is 4 (MultiPoint), little-endian at offset 1.
        assert_eq!(&wkb[1..5], &4u32.to_le_bytes());
        // First sub-geometry starts with its own LE marker + Point code.
        assert_eq!(wkb[9], 0x01);
        assert_eq!(&wkb[10..14], &1u32.to_le_bytes());
    }

    // --- Z/M ---------------------------------------------------------------
    //
    // Expected bytes below are taken verbatim from two independent real
    // tools (not hand-derived from the spec), matching this project's
    // established discipline of confirming wire-format details against real
    // output rather than a written spec alone (see the module docs):
    //   duckdb -c "LOAD spatial; SELECT hex(ST_AsWKB(ST_GeomFromText('POINT Z (1 2 3)')))"
    //   python3 -c "from osgeo import ogr; print(ogr.CreateGeometryFromWkt(
    //       'POINT Z (1 2 3)').ExportToIsoWkb(ogr.wkbNDR).hex())"
    // both emit 01 E9030000 000000000000F03F 0000000000000040 0000000000000840
    // (byte-order, type code 1001, x=1.0, y=2.0, z=3.0).

    #[test]
    fn encodes_point_z_matching_duckdb_and_gdal() {
        let wkb = encode(&Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));
        let mut expected = vec![0x01];
        expected.extend_from_slice(&1001u32.to_le_bytes());
        expected.extend_from_slice(&1.0f64.to_le_bytes());
        expected.extend_from_slice(&2.0f64.to_le_bytes());
        expected.extend_from_slice(&3.0f64.to_le_bytes());
        assert_eq!(wkb, expected);
    }

    #[test]
    fn encodes_point_m_with_2000_offset() {
        // duckdb: ST_GeomFromText('POINT M (1 2 4)') -> 01 D1070000 ... (type 2001)
        // Layout: marker(1) + type(4) + x(8) + y(8) + m(8).
        let wkb = encode(&Geometry::Point(Position::with_m(1.0, 2.0, 4.0)));
        assert_eq!(&wkb[1..5], &2001u32.to_le_bytes());
        assert_eq!(&wkb[21..29], &4.0f64.to_le_bytes());
    }

    #[test]
    fn encodes_point_zm_with_3000_offset_and_z_then_m_order() {
        // duckdb: ST_GeomFromText('POINT ZM (1 2 3 4)') -> 01 B90B0000 ... (type 3001)
        // Layout: marker(1) + type(4) + x(8) + y(8) + z(8) + m(8).
        let wkb = encode(&Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)));
        assert_eq!(&wkb[1..5], &3001u32.to_le_bytes());
        assert_eq!(&wkb[21..29], &3.0f64.to_le_bytes()); // z
        assert_eq!(&wkb[29..37], &4.0f64.to_le_bytes()); // m
    }

    #[test]
    fn encodes_linestring_z_matching_duckdb() {
        // duckdb: ST_GeomFromText('LINESTRING Z (0 0 0, 1 1 1)')
        // -> 01 EA030000 02000000 (0,0,0) (1,1,1), type 1002.
        let g = Geometry::LineString(vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)]);
        let wkb = encode(&g);
        assert_eq!(&wkb[1..5], &1002u32.to_le_bytes());
        assert_eq!(&wkb[5..9], &2u32.to_le_bytes());
        assert_eq!(wkb.len(), 5 + 4 + 2 * 24);
    }

    #[test]
    fn round_trips_z_m_and_zm_across_every_variant() {
        round_trip(Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));
        round_trip(Geometry::Point(Position::with_m(1.0, 2.0, 4.0)));
        round_trip(Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)));
        round_trip(Geometry::LineString(vec![
            Position::with_z(0.0, 0.0, 0.0),
            Position::with_z(1.0, 1.0, 2.0),
        ]));
        round_trip(Geometry::Polygon(vec![vec![
            Position::with_z(0.0, 0.0, 0.0),
            Position::with_z(4.0, 0.0, 0.0),
            Position::with_z(4.0, 4.0, 1.0),
            Position::with_z(0.0, 0.0, 0.0),
        ]]));
        round_trip(Geometry::MultiPoint(vec![
            Position::with_z(0.0, 0.0, 1.0),
            Position::with_z(1.0, 1.0, 2.0),
        ]));
        round_trip(Geometry::MultiLineString(vec![
            vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)],
            vec![Position::with_z(2.0, 2.0, 2.0), Position::with_z(3.0, 3.0, 3.0)],
        ]));
        round_trip(Geometry::MultiPolygon(vec![vec![vec![
            Position::with_z(0.0, 0.0, 0.0),
            Position::with_z(1.0, 0.0, 0.0),
            Position::with_z(1.0, 1.0, 0.0),
            Position::with_z(0.0, 0.0, 0.0),
        ]]]));
        round_trip(Geometry::GeometryCollection(vec![
            Geometry::Point(Position::with_z(5.0, 6.0, 7.0)),
            Geometry::LineString(vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)]),
        ]));
    }

    #[test]
    fn multipoint_z_outer_and_inner_type_codes_are_both_z_suffixed() {
        // duckdb: ST_GeomFromText('MULTIPOINT Z (0 0 0, 1 1 1)') -> outer 1004,
        // each embedded sub-point 1001 — both dimension-suffixed independently.
        let g = Geometry::MultiPoint(vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)]);
        let wkb = encode(&g);
        assert_eq!(&wkb[1..5], &1004u32.to_le_bytes());
        assert_eq!(wkb[9], 0x01);
        assert_eq!(&wkb[10..14], &1001u32.to_le_bytes());
    }

    #[test]
    fn mixed_dimensionality_within_one_geometry_falls_back_to_zero_rather_than_panicking() {
        // Not a real-world case (no reader constructs this yet), but the
        // codec must stay total: the geometry's dimensionality is taken from
        // the first position (has Z), so the second position's missing Z
        // encodes as 0.0 instead of panicking or silently truncating.
        let g = Geometry::LineString(vec![Position::with_z(0.0, 0.0, 5.0), Position::new(1.0, 1.0)]);
        let wkb = encode(&g);
        let decoded = decode(&wkb).unwrap();
        match decoded {
            Geometry::LineString(ps) => {
                assert_eq!(ps[0].z, Some(5.0));
                assert_eq!(ps[1].z, Some(0.0));
            }
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    /// Decode raw hex bytes, not just round-trip our own encoder.
    fn from_hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_real_duckdb_and_gdal_bytes_directly() {
        // The literal bytes captured from `duckdb -c "LOAD spatial; SELECT
        // hex(ST_AsWKB(ST_GeomFromText('POINT ZM (1 2 3 4)')))"` and
        // cross-checked identical from
        // `ogr.CreateGeometryFromWkt('POINT Z (1 2 3)').ExportToIsoWkb()` for
        // the Z-only case — decoding real external-tool output, not just
        // round-tripping our own encoder.
        let xyz = from_hex("01e9030000000000000000f03f00000000000000400000000000000840");
        assert_eq!(decode(&xyz).unwrap(), Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));

        let xym = from_hex("01d1070000000000000000f03f00000000000000400000000000001040");
        assert_eq!(decode(&xym).unwrap(), Geometry::Point(Position::with_m(1.0, 2.0, 4.0)));

        let xyzm = from_hex(
            "01b90b0000000000000000f03f000000000000004000000000000008400000000000001040",
        );
        assert_eq!(decode(&xyzm).unwrap(), Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)));

        let line_z = from_hex(
            "01ea03000002000000000000000000000000000000000000000000000000000000000000000000f03f000000000000f03f000000000000f03f",
        );
        assert_eq!(
            decode(&line_z).unwrap(),
            Geometry::LineString(vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)])
        );

        let multipoint_z = from_hex(
            "01ec0300000200000001e903000000000000000000000000000000000000000000000000000001e9030000000000000000f03f000000000000f03f000000000000f03f",
        );
        assert_eq!(
            decode(&multipoint_z).unwrap(),
            Geometry::MultiPoint(vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)])
        );
    }

    #[test]
    fn rejects_ewkb_style_high_bit_flag_type_codes() {
        // GDAL's plain (non-ISO) ExportToWkb() would emit 0x80000001 for a
        // 3D point — confirm we reject rather than silently misparse it as
        // an enormous, unknown base type.
        let mut bytes = vec![0x01];
        bytes.extend_from_slice(&0x80000001u32.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }
}
