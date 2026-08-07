//! Well-Known Binary (WKB) encoding of [`Geometry`], the encoding GeoParquet
//! uses for its geometry column.
//!
//! Output is little-endian, 2D (XY). Multi-geometries and geometry
//! collections embed complete WKB sub-geometries, each with their own
//! byte-order marker and type code, per the OGC specification.

use super::{Geometry, Position};

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
