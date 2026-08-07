//! Well-Known Binary (WKB) encoding of [`Geometry`], the encoding GeoParquet
//! uses for its geometry column.
//!
//! Output is little-endian, 2D (XY). Multi-geometries and geometry
//! collections embed complete WKB sub-geometries, each with their own
//! byte-order marker and type code, per the OGC specification.

use super::{Geometry, Position};
use crate::error::{Error, Result};

// Little-endian byte-order marker.
const LE: u8 = 0x01;

// 2D geometry type codes.
const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOINT: u32 = 4;
const WKB_MULTILINESTRING: u32 = 5;
const WKB_MULTIPOLYGON: u32 = 6;
const WKB_GEOMETRYCOLLECTION: u32 = 7;

/// Encode a geometry to a fresh WKB byte buffer.
pub fn encode(geom: &Geometry) -> Vec<u8> {
    let mut out = Vec::new();
    write_geometry(&mut out, geom);
    out
}

fn write_geometry(out: &mut Vec<u8>, geom: &Geometry) {
    match geom {
        Geometry::Point(p) => {
            header(out, WKB_POINT);
            point(out, *p);
        }
        Geometry::LineString(ps) => {
            header(out, WKB_LINESTRING);
            line(out, ps);
        }
        Geometry::Polygon(rings) => {
            header(out, WKB_POLYGON);
            polygon(out, rings);
        }
        Geometry::MultiPoint(ps) => {
            header(out, WKB_MULTIPOINT);
            out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
            for p in ps {
                header(out, WKB_POINT);
                point(out, *p);
            }
        }
        Geometry::MultiLineString(lines) => {
            header(out, WKB_MULTILINESTRING);
            out.extend_from_slice(&(lines.len() as u32).to_le_bytes());
            for l in lines {
                header(out, WKB_LINESTRING);
                line(out, l);
            }
        }
        Geometry::MultiPolygon(polys) => {
            header(out, WKB_MULTIPOLYGON);
            out.extend_from_slice(&(polys.len() as u32).to_le_bytes());
            for poly in polys {
                header(out, WKB_POLYGON);
                polygon(out, poly);
            }
        }
        Geometry::GeometryCollection(geoms) => {
            header(out, WKB_GEOMETRYCOLLECTION);
            out.extend_from_slice(&(geoms.len() as u32).to_le_bytes());
            for g in geoms {
                write_geometry(out, g);
            }
        }
    }
}

/// Byte-order marker followed by the little-endian type code.
fn header(out: &mut Vec<u8>, type_code: u32) {
    out.push(LE);
    out.extend_from_slice(&type_code.to_le_bytes());
}

fn point(out: &mut Vec<u8>, p: Position) {
    out.extend_from_slice(&p[0].to_le_bytes());
    out.extend_from_slice(&p[1].to_le_bytes());
}

/// A `u32` count of positions followed by the raw coordinate pairs.
fn line(out: &mut Vec<u8>, ps: &[Position]) {
    out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
    for p in ps {
        point(out, *p);
    }
}

/// A `u32` ring count, each ring encoded like a linear ring of positions.
fn polygon(out: &mut Vec<u8>, rings: &[Vec<Position>]) {
    out.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    for ring in rings {
        line(out, ring);
    }
}

// --- decoding --------------------------------------------------------------

/// Decode a WKB geometry (the inverse of [`encode`]). Little-endian, 2D only;
/// multi-geometries and collections recurse into their embedded sub-WKB.
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
    let geom = match code {
        WKB_POINT => Geometry::Point(read_point(r)?),
        WKB_LINESTRING => Geometry::LineString(read_line(r)?),
        WKB_POLYGON => Geometry::Polygon(read_polygon(r)?),
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

fn read_point(r: &mut Reader) -> Result<Position> {
    Ok([r.f64()?, r.f64()?])
}

fn read_line(r: &mut Reader) -> Result<Vec<Position>> {
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(read_point(r)?);
    }
    Ok(v)
}

fn read_polygon(r: &mut Reader) -> Result<Vec<Vec<Position>>> {
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(read_line(r)?);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_point_byte_for_byte() {
        let wkb = encode(&Geometry::Point([1.0, 2.0]));
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
        let g = Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]);
        assert_eq!(encode(&g).len(), 5 + 4 + 3 * 16);
    }

    #[test]
    fn encodes_polygon_with_hole_length() {
        // header(5) + ringcount(4) + 2 rings, each count(4)+5 pts*16
        let outer = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]];
        let hole = vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]];
        let g = Geometry::Polygon(vec![outer, hole]);
        assert_eq!(encode(&g).len(), 5 + 4 + 2 * (4 + 5 * 16));
    }

    fn round_trip(g: Geometry) {
        assert_eq!(decode(&encode(&g)).unwrap(), g);
    }

    #[test]
    fn decodes_every_variant() {
        round_trip(Geometry::Point([-73.9857, 40.7484]));
        round_trip(Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]));
        round_trip(Geometry::Polygon(vec![
            vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]],
            vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]],
        ]));
        round_trip(Geometry::MultiPoint(vec![[0.0, 0.0], [1.0, 1.0]]));
        round_trip(Geometry::MultiLineString(vec![
            vec![[0.0, 0.0], [1.0, 1.0]],
            vec![[2.0, 2.0], [3.0, 3.0]],
        ]));
        round_trip(Geometry::MultiPolygon(vec![vec![vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 0.0],
        ]]]));
        round_trip(Geometry::GeometryCollection(vec![
            Geometry::Point([5.0, 6.0]),
            Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0]]),
        ]));
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
        let g = Geometry::MultiPoint(vec![[0.0, 0.0], [1.0, 1.0]]);
        let wkb = encode(&g);
        assert_eq!(wkb.len(), 5 + 4 + 2 * 21);
        // Type code is 4 (MultiPoint), little-endian at offset 1.
        assert_eq!(&wkb[1..5], &4u32.to_le_bytes());
        // First sub-geometry starts with its own LE marker + Point code.
        assert_eq!(wkb[9], 0x01);
        assert_eq!(&wkb[10..14], &1u32.to_le_bytes());
    }
}
