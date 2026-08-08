//! Well-Known Text (WKT) encoding/decoding of [`Geometry`] — the text sibling
//! of WKB, used as the geometry representation inside CSV. 2D only; any Z/M
//! ordinates on input are ignored.

use super::{Geometry, Position};
use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::Convert(format!("wkt: {msg}")))
}

// --- encoding --------------------------------------------------------------

/// Render a geometry as WKT (e.g. `POINT (1 2)`).
pub fn encode(g: &Geometry) -> String {
    let mut s = String::new();
    write_geometry(&mut s, g);
    s
}

fn write_geometry(s: &mut String, g: &Geometry) {
    match g {
        Geometry::Point(p) => {
            s.push_str("POINT (");
            write_coord(s, *p);
            s.push(')');
        }
        Geometry::LineString(ps) => {
            s.push_str("LINESTRING ");
            write_coord_list(s, ps);
        }
        Geometry::Polygon(rings) => {
            s.push_str("POLYGON ");
            write_rings(s, rings);
        }
        Geometry::MultiPoint(ps) => {
            s.push_str("MULTIPOINT ");
            write_coord_list(s, ps);
        }
        Geometry::MultiLineString(lines) => {
            s.push_str("MULTILINESTRING ");
            write_rings(s, lines);
        }
        Geometry::MultiPolygon(polys) => {
            s.push_str("MULTIPOLYGON (");
            for (i, poly) in polys.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                write_rings(s, poly);
            }
            s.push(')');
        }
        Geometry::GeometryCollection(geoms) => {
            s.push_str("GEOMETRYCOLLECTION (");
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

fn write_coord(s: &mut String, p: Position) {
    write_num(s, p[0]);
    s.push(' ');
    write_num(s, p[1]);
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

    fn geometry(&mut self) -> Result<Geometry> {
        let kw = self.keyword()?;
        // Skip an optional dimensionality token (Z / M / ZM).
        self.skip_ws();
        if matches!(self.peek(), Some(b'Z' | b'M' | b'z' | b'm')) {
            while matches!(self.peek(), Some(b'Z' | b'M' | b'z' | b'm')) {
                self.i += 1;
            }
        }
        match kw.as_str() {
            "POINT" => Ok(Geometry::Point(self.paren_coord()?)),
            "LINESTRING" => Ok(Geometry::LineString(self.coord_list()?)),
            "POLYGON" => Ok(Geometry::Polygon(self.ring_list()?)),
            "MULTIPOINT" => Ok(Geometry::MultiPoint(self.multipoint()?)),
            "MULTILINESTRING" => Ok(Geometry::MultiLineString(self.ring_list()?)),
            "MULTIPOLYGON" => Ok(Geometry::MultiPolygon(self.polygon_list()?)),
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

    /// A coordinate: two numbers; any extra Z/M ordinates are consumed and
    /// dropped.
    fn coord(&mut self) -> Result<Position> {
        let x = self.number()?;
        let y = self.number()?;
        // Consume optional trailing ordinates (Z, M) up to the next `,`/`)`.
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b',') | Some(b')') | None => break,
                _ => {
                    self.number()?;
                }
            }
        }
        Ok([x, y])
    }

    fn paren_coord(&mut self) -> Result<Position> {
        self.expect(b'(')?;
        let c = self.coord()?;
        self.expect(b')')?;
        Ok(c)
    }

    /// `( coord, coord, … )`.
    fn coord_list(&mut self) -> Result<Vec<Position>> {
        self.expect(b'(')?;
        let mut out = vec![self.coord()?];
        while self.comma_or_close()? {
            out.push(self.coord()?);
        }
        Ok(out)
    }

    /// `( ring, ring, … )` where each ring is a coord list — polygons and
    /// multi-linestrings share this shape.
    fn ring_list(&mut self) -> Result<Vec<Vec<Position>>> {
        self.expect(b'(')?;
        let mut out = vec![self.coord_list()?];
        while self.comma_or_close()? {
            out.push(self.coord_list()?);
        }
        Ok(out)
    }

    /// MULTIPOINT members: either `x y` or `(x y)`.
    fn multipoint(&mut self) -> Result<Vec<Position>> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'(') {
                out.push(self.paren_coord()?);
            } else {
                out.push(self.coord()?);
            }
            if !self.comma_or_close()? {
                break;
            }
        }
        Ok(out)
    }

    fn polygon_list(&mut self) -> Result<Vec<Vec<Vec<Position>>>> {
        self.expect(b'(')?;
        let mut out = vec![self.ring_list()?];
        while self.comma_or_close()? {
            out.push(self.ring_list()?);
        }
        Ok(out)
    }

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
        round_trip(Geometry::Point([1.0, 2.5]));
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
    fn parses_gdal_style_input() {
        // Lowercase, extra whitespace, and MULTIPOINT with inner parens.
        assert_eq!(
            decode("point (1 2)").unwrap(),
            Geometry::Point([1.0, 2.0])
        );
        assert_eq!(
            decode("MULTIPOINT ((1 2), (3 4))").unwrap(),
            Geometry::MultiPoint(vec![[1.0, 2.0], [3.0, 4.0]])
        );
        assert_eq!(
            decode("POLYGON  (( 0 0, 1 0 , 1 1, 0 0 ))").unwrap(),
            Geometry::Polygon(vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]])
        );
        // Z ordinate is ignored.
        assert_eq!(decode("POINT Z (1 2 3)").unwrap(), Geometry::Point([1.0, 2.0]));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("").is_err());
        assert!(decode("POINT (1)").is_err());
        assert!(decode("NONSENSE (1 2)").is_err());
        assert!(decode("POINT (1 2) extra").is_err());
    }
}
