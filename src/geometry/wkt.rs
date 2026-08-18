//! Well-Known Text (WKT) encoding/decoding of [`Geometry`] — the text sibling
//! of WKB, used as the geometry representation inside CSV.
//!
//! Z, M, and ZM are supported via the standard `POINT Z (...)`/`POINT M
//! (...)`/`POINT ZM (...)` dimensionality keyword, verified against real
//! DuckDB (`ST_AsText`) and GDAL (`ExportToIsoWkt`) output before
//! implementing — both always emit the explicit keyword. When decoding input
//! that has *no* keyword but still carries a bare extra ordinate (GDAL's
//! plain, non-ISO `ExportToWkt()` does exactly this — `POINT (1 2 3)` with no
//! `Z` at all, confirmed empirically, not assumed), the first bare extra
//! number is treated as Z and a second as M, matching this crate's own WKB
//! codec (`geometry/wkb.rs`) and GeoJSON reader's convention for an
//! unlabeled extra ordinate, and matching ISO SFA's X/Y/Z/M ordinate order.

use super::{Geometry, Position};
use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::Convert(format!("wkt: {msg}")))
}

/// A geometry's dimensionality, mirroring `wkb.rs`'s identically-named type
/// (kept as a separate, private copy — this module doesn't share code with
/// the WKB codec, matching how every format spoke in this crate stays
/// self-contained).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dim {
    z: bool,
    m: bool,
}

impl Dim {
    const XY: Dim = Dim { z: false, m: false };

    fn of(p: Position) -> Self {
        Dim { z: p.z.is_some(), m: p.m.is_some() }
    }
}

fn dim_of_first(ps: &[Position]) -> Dim {
    ps.first().copied().map(Dim::of).unwrap_or(Dim::XY)
}

fn dim_of_rings(rings: &[Vec<Position>]) -> Dim {
    rings.first().map(|r| dim_of_first(r)).unwrap_or(Dim::XY)
}

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

// --- encoding --------------------------------------------------------------

/// Render a geometry as WKT (e.g. `POINT (1 2)`, `POINT Z (1 2 3)`).
pub fn encode(g: &Geometry) -> String {
    let mut s = String::new();
    write_geometry(&mut s, g);
    s
}

fn write_geometry(s: &mut String, g: &Geometry) {
    match g {
        Geometry::Point(p) => {
            write_header(s, "POINT", Dim::of(*p));
            s.push('(');
            write_coord(s, *p);
            s.push(')');
        }
        Geometry::LineString(ps) => {
            write_header(s, "LINESTRING", dim_of_first(ps));
            write_coord_list(s, ps);
        }
        Geometry::Polygon(rings) => {
            write_header(s, "POLYGON", dim_of_rings(rings));
            write_rings(s, rings);
        }
        Geometry::MultiPoint(ps) => {
            write_header(s, "MULTIPOINT", dim_of_first(ps));
            write_coord_list(s, ps);
        }
        Geometry::MultiLineString(lines) => {
            let dim = lines.first().map(|l| dim_of_first(l)).unwrap_or(Dim::XY);
            write_header(s, "MULTILINESTRING", dim);
            write_rings(s, lines);
        }
        Geometry::MultiPolygon(polys) => {
            let dim = polys.first().map(|p| dim_of_rings(p)).unwrap_or(Dim::XY);
            write_header(s, "MULTIPOLYGON", dim);
            s.push('(');
            for (i, poly) in polys.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                write_rings(s, poly);
            }
            s.push(')');
        }
        Geometry::GeometryCollection(geoms) => {
            write_header(s, "GEOMETRYCOLLECTION", dim_of_geometry(g));
            s.push('(');
            for (i, g) in geoms.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                write_geometry(s, g);
            }
            s.push(')');
        }
    }
}

/// `NAME` (plus ` Z`/` M`/` ZM` when `dim` calls for it) followed by a single
/// trailing space, ready for the caller to append `(...)`. 2D output is
/// byte-identical to before this module supported Z/M (`Dim::XY` adds no
/// keyword).
fn write_header(s: &mut String, name: &str, dim: Dim) {
    s.push_str(name);
    match (dim.z, dim.m) {
        (false, false) => {}
        (true, false) => s.push_str(" Z"),
        (false, true) => s.push_str(" M"),
        (true, true) => s.push_str(" ZM"),
    }
    s.push(' ');
}

fn write_coord(s: &mut String, p: Position) {
    write_num(s, p.x);
    s.push(' ');
    write_num(s, p.y);
    if let Some(z) = p.z {
        s.push(' ');
        write_num(s, z);
    }
    if let Some(m) = p.m {
        s.push(' ');
        write_num(s, m);
    }
}

fn write_coord_list(s: &mut String, ps: &[Position]) {
    s.push('(');
    for (i, p) in ps.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        write_coord(s, *p);
    }
    s.push(')');
}

fn write_rings(s: &mut String, rings: &[Vec<Position>]) {
    s.push('(');
    for (i, r) in rings.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        write_coord_list(s, r);
    }
    s.push(')');
}

/// Format a coordinate: integers without a trailing `.0`.
fn write_num(s: &mut String, v: f64) {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e15 {
        s.push_str(&(v as i64).to_string());
    } else {
        s.push_str(&v.to_string());
    }
}

// --- decoding --------------------------------------------------------------

/// Parse a WKT string into a geometry.
pub fn decode(input: &str) -> Result<Geometry> {
    let mut p = Parser {
        b: input.as_bytes(),
        i: 0,
    };
    let g = p.geometry()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return err("trailing characters after geometry");
    }
    Ok(g)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            err(&format!("expected '{}'", c as char))
        }
    }

    /// An uppercase keyword (letters only).
    fn keyword(&mut self) -> Result<String> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        if self.i == start {
            return err("expected a geometry keyword");
        }
        Ok(std::str::from_utf8(&self.b[start..self.i])
            .unwrap()
            .to_ascii_uppercase())
    }

    /// An optional `Z`/`M`/`ZM` dimensionality token right after a geometry
    /// keyword. `None` means no token was present at all (distinct from a
    /// token being present and explicitly 2D, which can't happen — there's
    /// no WKT keyword for "explicitly 2D").
    fn dim_suffix(&mut self) -> Result<Option<Dim>> {
        self.skip_ws();
        let start = self.i;
        while matches!(self.peek(), Some(b'Z' | b'M' | b'z' | b'm')) {
            self.i += 1;
        }
        if self.i == start {
            return Ok(None);
        }
        match std::str::from_utf8(&self.b[start..self.i]).unwrap().to_ascii_uppercase().as_str() {
            "Z" => Ok(Some(Dim { z: true, m: false })),
            "M" => Ok(Some(Dim { z: false, m: true })),
            "ZM" => Ok(Some(Dim { z: true, m: true })),
            other => err(&format!("unknown WKT dimensionality token \"{other}\"")),
        }
    }

    fn geometry(&mut self) -> Result<Geometry> {
        let kw = self.keyword()?;
        let dim = self.dim_suffix()?;
        match kw.as_str() {
            "POINT" => Ok(Geometry::Point(self.paren_coord(dim)?)),
            "LINESTRING" => Ok(Geometry::LineString(self.coord_list(dim)?)),
            "POLYGON" => Ok(Geometry::Polygon(self.ring_list(dim)?)),
            "MULTIPOINT" => Ok(Geometry::MultiPoint(self.multipoint(dim)?)),
            "MULTILINESTRING" => Ok(Geometry::MultiLineString(self.ring_list(dim)?)),
            "MULTIPOLYGON" => Ok(Geometry::MultiPolygon(self.polygon_list(dim)?)),
            "GEOMETRYCOLLECTION" => Ok(Geometry::GeometryCollection(self.geometry_list()?)),
            other => err(&format!("unknown geometry keyword \"{other}\"")),
        }
    }

    fn number(&mut self) -> Result<f64> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i == start {
            return err("expected a number");
        }
        std::str::from_utf8(&self.b[start..self.i])
            .unwrap()
            .parse::<f64>()
            .map_err(|_| Error::Convert("wkt: invalid number".into()))
    }

    /// A coordinate: X, Y, plus whatever `dim` says to expect. `dim ==
    /// None` (no `Z`/`M`/`ZM` keyword on the geometry) still accepts extra
    /// bare ordinates, treating the first as Z and a second as M — the
    /// convention real, keyword-less WKT from GDAL's plain `ExportToWkt()`
    /// actually uses (see the module docs). Any ordinate beyond what `dim`
    /// (or this fallback) accounts for is consumed and dropped, matching
    /// the streaming/tree GeoJSON readers' handling of a stray 4th+ value.
    fn coord(&mut self, dim: Option<Dim>) -> Result<Position> {
        let x = self.number()?;
        let y = self.number()?;
        let mut extra = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b',') | Some(b')') | None => break,
                _ => extra.push(self.number()?),
            }
        }
        let mut it = extra.into_iter();
        let (z, m) = match dim {
            Some(Dim { z: true, m: true }) => (it.next(), it.next()),
            Some(Dim { z: true, m: false }) => (it.next(), None),
            Some(Dim { z: false, m: true }) => (None, it.next()),
            Some(Dim { z: false, m: false }) | None => (it.next(), it.next()),
        };
        Ok(Position { x, y, z, m })
    }

    fn paren_coord(&mut self, dim: Option<Dim>) -> Result<Position> {
        self.expect(b'(')?;
        let c = self.coord(dim)?;
        self.expect(b')')?;
        Ok(c)
    }

    /// `( coord, coord, … )`.
    fn coord_list(&mut self, dim: Option<Dim>) -> Result<Vec<Position>> {
        self.expect(b'(')?;
        let mut out = vec![self.coord(dim)?];
        while self.comma_or_close()? {
            out.push(self.coord(dim)?);
        }
        Ok(out)
    }

    /// `( ring, ring, … )` where each ring is a coord list — polygons and
    /// multi-linestrings share this shape.
    fn ring_list(&mut self, dim: Option<Dim>) -> Result<Vec<Vec<Position>>> {
        self.expect(b'(')?;
        let mut out = vec![self.coord_list(dim)?];
        while self.comma_or_close()? {
            out.push(self.coord_list(dim)?);
        }
        Ok(out)
    }

    /// MULTIPOINT members: either `x y` or `(x y)`.
    fn multipoint(&mut self, dim: Option<Dim>) -> Result<Vec<Position>> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'(') {
                out.push(self.paren_coord(dim)?);
            } else {
                out.push(self.coord(dim)?);
            }
            if !self.comma_or_close()? {
                break;
            }
        }
        Ok(out)
    }

    fn polygon_list(&mut self, dim: Option<Dim>) -> Result<Vec<Vec<Vec<Position>>>> {
        self.expect(b'(')?;
        let mut out = vec![self.ring_list(dim)?];
        while self.comma_or_close()? {
            out.push(self.ring_list(dim)?);
        }
        Ok(out)
    }

    /// `GEOMETRYCOLLECTION`'s members are complete, independent WKT
    /// geometries, each with its own type name and (optional) dimension
    /// keyword — confirmed against real DuckDB output
    /// (`GEOMETRYCOLLECTION Z (POINT Z (1 2 3))`, the inner `POINT`
    /// repeating `Z` itself) — so no outer `dim` is threaded in here,
    /// unlike every other multi-shape above.
    fn geometry_list(&mut self) -> Result<Vec<Geometry>> {
        self.expect(b'(')?;
        let mut out = vec![self.geometry()?];
        while self.comma_or_close()? {
            out.push(self.geometry()?);
        }
        Ok(out)
    }

    /// After an element: consume `,` (returns true) or `)` (returns false).
    fn comma_or_close(&mut self) -> Result<bool> {
        self.skip_ws();
        match self.peek() {
            Some(b',') => {
                self.i += 1;
                Ok(true)
            }
            Some(b')') => {
                self.i += 1;
                Ok(false)
            }
            _ => err("expected ',' or ')'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(g: Geometry) {
        let text = encode(&g);
        assert_eq!(decode(&text).unwrap(), g, "wkt round trip: {text}");
    }

    #[test]
    fn round_trips_all_types() {
        round_trip(Geometry::Point(Position::new(1.0, 2.5)));
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
    fn parses_gdal_style_input() {
        // Lowercase, extra whitespace, and MULTIPOINT with inner parens.
        assert_eq!(
            decode("point (1 2)").unwrap(),
            Geometry::Point(Position::new(1.0, 2.0))
        );
        assert_eq!(
            decode("MULTIPOINT ((1 2), (3 4))").unwrap(),
            Geometry::MultiPoint(vec![Position::new(1.0, 2.0), Position::new(3.0, 4.0)])
        );
        assert_eq!(
            decode("POLYGON  (( 0 0, 1 0 , 1 1, 0 0 ))").unwrap(),
            Geometry::Polygon(vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0), Position::new(1.0, 1.0), Position::new(0.0, 0.0)]])
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("").is_err());
        assert!(decode("POINT (1)").is_err());
        assert!(decode("NONSENSE (1 2)").is_err());
        assert!(decode("POINT (1 2) extra").is_err());
    }

    // --- Z/M -----------------------------------------------------------
    //
    // Expected text below is taken verbatim from two independent real
    // tools, matching this project's established discipline (see the
    // module docs):
    //   duckdb -c "LOAD spatial; SELECT ST_AsText(ST_GeomFromText('POINT Z (1 2 3)'))"
    //   python3 -c "from osgeo import ogr; print(ogr.CreateGeometryFromWkt(
    //       'POINT Z (1 2 3)').ExportToIsoWkt())"
    // both emit "POINT Z (1 2 3)".

    #[test]
    fn decodes_explicit_z_m_zm_keywords() {
        assert_eq!(decode("POINT Z (1 2 3)").unwrap(), Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));
        assert_eq!(decode("POINT M (1 2 4)").unwrap(), Geometry::Point(Position::with_m(1.0, 2.0, 4.0)));
        assert_eq!(decode("POINT ZM (1 2 3 4)").unwrap(), Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)));
        // Lowercase keyword.
        assert_eq!(decode("point z (1 2 3)").unwrap(), Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));
    }

    #[test]
    fn decodes_bare_extra_ordinate_as_z_matching_gdal_plain_wkt() {
        // GDAL's non-ISO ExportToWkt() emits exactly this: no keyword, a
        // bare third number, confirmed empirically (see module docs).
        assert_eq!(decode("POINT (1 2 3)").unwrap(), Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));
        // A bare fourth number (not real-world, but must not panic) is M.
        assert_eq!(decode("POINT (1 2 3 4)").unwrap(), Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)));
    }

    #[test]
    fn encodes_point_z_m_zm_matching_duckdb_and_gdal() {
        assert_eq!(encode(&Geometry::Point(Position::with_z(1.0, 2.0, 3.0))), "POINT Z (1 2 3)");
        assert_eq!(encode(&Geometry::Point(Position::with_m(1.0, 2.0, 4.0))), "POINT M (1 2 4)");
        assert_eq!(encode(&Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0))), "POINT ZM (1 2 3 4)");
        assert_eq!(encode(&Geometry::Point(Position::new(1.0, 2.0))), "POINT (1 2)");
    }

    #[test]
    fn encodes_linestring_and_polygon_z_matching_duckdb() {
        // duckdb: ST_AsText(ST_GeomFromText('LINESTRING Z (0 0 0, 1 1 1)'))
        let line = Geometry::LineString(vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)]);
        assert_eq!(encode(&line), "LINESTRING Z (0 0 0, 1 1 1)");

        // duckdb: ST_AsText(ST_GeomFromText('POLYGON Z ((0 0 0, 4 0 0, 4 4 0, 0 0 0))'))
        let poly = Geometry::Polygon(vec![vec![
            Position::with_z(0.0, 0.0, 0.0),
            Position::with_z(4.0, 0.0, 0.0),
            Position::with_z(4.0, 4.0, 0.0),
            Position::with_z(0.0, 0.0, 0.0),
        ]]);
        assert_eq!(encode(&poly), "POLYGON Z ((0 0 0, 4 0 0, 4 4 0, 0 0 0))");
    }

    #[test]
    fn multipoint_z_does_not_repeat_keyword_per_member_matching_duckdb() {
        // duckdb: ST_AsText(ST_GeomFromText('MULTIPOINT Z (0 0 0, 1 1 1)'))
        // — unlike WKB, WKT members inside MULTIPOINT/POLYGON share the
        // outer dimension keyword rather than each repeating it.
        let g = Geometry::MultiPoint(vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 1.0)]);
        assert_eq!(encode(&g), "MULTIPOINT Z (0 0 0, 1 1 1)");
    }

    #[test]
    fn geometrycollection_z_repeats_keyword_on_each_member_matching_duckdb() {
        // duckdb: ST_AsText(ST_GeomFromText('GEOMETRYCOLLECTION Z (POINT Z (1 2 3))'))
        let g = Geometry::GeometryCollection(vec![Geometry::Point(Position::with_z(1.0, 2.0, 3.0))]);
        assert_eq!(encode(&g), "GEOMETRYCOLLECTION Z (POINT Z (1 2 3))");
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
        round_trip(Geometry::MultiPoint(vec![Position::with_z(0.0, 0.0, 1.0), Position::with_z(1.0, 1.0, 2.0)]));
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
    fn rejects_unknown_dimension_token() {
        assert!(decode("POINT ZZZ (1 2 3)").is_err());
    }
}
