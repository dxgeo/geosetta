//! CSV spoke: tabular rows with a WKT geometry column and property columns.
//!
//! On read, the geometry column is the first header named (case-insensitively)
//! `geometry`, `geom`, `wkt`, or `the_geom`; its cells are parsed as WKT and the
//! rest become properties with lightly inferred types. On write, property
//! columns (inferred by scanning all features) come first, then a `geometry`
//! column holding WKT. RFC 4180 quoting is handled both ways.

use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::{from_wkt, to_wkt};
use crate::json::JsonValue;
use crate::schema::{Cell, infer_columns};
use std::rc::Rc;

const GEOMETRY_NAMES: [&str; 4] = ["geometry", "geom", "wkt", "the_geom"];

/// Parse CSV bytes into a feature collection.
pub fn read(bytes: &[u8]) -> Result<FeatureCollection> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Convert("csv: input is not valid utf-8".into()))?;
    let rows = parse(text);
    let Some(header) = rows.first() else {
        return Ok(FeatureCollection::new(Vec::new()));
    };

    let geom_col = header
        .iter()
        .position(|h| GEOMETRY_NAMES.contains(&h.trim().to_ascii_lowercase().as_str()));

    // Intern each property column's key once so every row shares one `Rc`.
    let keys: Vec<Rc<str>> = header.iter().map(|name| Rc::from(name.as_str())).collect();

    let mut features = Vec::new();
    for row in &rows[1..] {
        if row.iter().all(|f| f.is_empty()) {
            continue; // blank line
        }
        let geometry = match geom_col {
            Some(gi) => match row.get(gi).map(|s| s.trim()) {
                Some(w) if !w.is_empty() => Some(from_wkt(w)?),
                _ => None,
            },
            None => None,
        };
        let properties = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != geom_col)
            .map(|(i, key)| (Rc::clone(key), infer_value(row.get(i).map_or("", |s| s))))
            .collect();
        features.push(Feature {
            geometry,
            properties,
        });
    }
    Ok(FeatureCollection::new(features))
}

/// Serialize a feature collection to CSV bytes.
pub fn write(fc: &FeatureCollection) -> Vec<u8> {
    let columns = infer_columns(&fc.features);

    let mut out = String::new();
    let header: Vec<String> = columns
        .iter()
        .map(|c| field(&c.name))
        .chain(std::iter::once("geometry".to_string()))
        .collect();
    out.push_str(&header.join(","));
    out.push('\n');

    for (row, feat) in fc.features.iter().enumerate() {
        let mut fields: Vec<String> = columns
            .iter()
            .map(|c| field(&cell_text(&c.values[row])))
            .collect();
        let wkt = feat.geometry.as_ref().map(to_wkt).unwrap_or_default();
        fields.push(field(&wkt));
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out.into_bytes()
}

// --- CSV lexing (RFC 4180) -------------------------------------------------

/// Split CSV text into rows of fields, honoring quoted fields (`""` escapes a
/// quote; quoted fields may contain commas and newlines).
fn parse(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                _ => field.push(c),
            }
        }
    }
    // Flush a final row that wasn't newline-terminated.
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Quote a field if it contains a comma, quote, or newline.
fn field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Lightly infer a JSON value from a raw CSV cell.
fn infer_value(cell: &str) -> JsonValue {
    if cell.is_empty() {
        return JsonValue::Null;
    }
    if cell.eq_ignore_ascii_case("true") {
        return JsonValue::Bool(true);
    }
    if cell.eq_ignore_ascii_case("false") {
        return JsonValue::Bool(false);
    }
    // Only attempt numbers on something that starts numeric, so words like
    // "NaN"/"inf" (which parse as floats) stay strings.
    let numeric_start = cell
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'));
    if numeric_start {
        if let Ok(i) = cell.parse::<i64>() {
            return JsonValue::Number {
                value: i as f64,
                is_int: true,
            };
        }
        if let Ok(f) = cell.parse::<f64>()
            && f.is_finite()
        {
            return JsonValue::Number {
                value: f,
                is_int: false,
            };
        }
    }
    JsonValue::String(cell.to_string())
}

fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(n) => n.to_string(),
        Cell::Double(d) => d.to_string(),
        Cell::Str(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Geometry, Position};

    #[test]
    fn reads_wkt_and_typed_properties() {
        let text = "name,pop,geometry\nA,100,\"POINT (1 2)\"\n\"B, city\",50,\"LINESTRING (0 0, 1 1)\"\n";
        let fc = read(text.as_bytes()).unwrap();
        assert_eq!(fc.features.len(), 2);
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::new(1.0, 2.0))));
        assert_eq!(fc.features[0].properties[0], ("name".into(), JsonValue::String("A".into())));
        assert_eq!(
            fc.features[0].properties[1],
            ("pop".into(), JsonValue::Number { value: 100.0, is_int: true })
        );
        // Quoted field with an embedded comma survives.
        assert_eq!(fc.features[1].properties[0].1.as_str(), Some("B, city"));
    }

    #[test]
    fn round_trips_through_csv() {
        let fc = FeatureCollection::new(vec![
                Feature {
                    geometry: Some(Geometry::Point(Position::new(-73.9, 40.7))),
                    properties: vec![
                        ("name".into(), JsonValue::String("x".into())),
                        ("n".into(), JsonValue::Number { value: 3.0, is_int: true }),
                    ],
                },
                Feature {
                    geometry: Some(Geometry::Polygon(vec![vec![
                        Position::new(0.0, 0.0),
                        Position::new(1.0, 0.0),
                        Position::new(1.0, 1.0),
                        Position::new(0.0, 0.0),
                    ]])),
                    properties: vec![
                        ("name".into(), JsonValue::String("y".into())),
                        ("n".into(), JsonValue::Null),
                    ],
                },
            ]);
        let bytes = write(&fc);
        let back = read(&bytes).unwrap();
        assert_eq!(back.features.len(), 2);
        assert_eq!(back.features[0].geometry, fc.features[0].geometry);
        assert_eq!(back.features[1].geometry, fc.features[1].geometry);
        assert_eq!(back.features[0].properties[1].1, JsonValue::Number { value: 3.0, is_int: true });
    }

    #[test]
    fn no_geometry_column_is_ok() {
        let fc = read(b"a,b\n1,2\n").unwrap();
        assert_eq!(fc.features.len(), 1);
        assert!(fc.features[0].geometry.is_none());
        assert_eq!(fc.features[0].properties.len(), 2);
    }
}
