//! CSV spoke: tabular rows with a WKT geometry column and property columns.
//!
//! On read, the geometry column is the first header named (case-insensitively)
//! `geometry`, `geom`, `wkt`, or `the_geom`; its cells are parsed as WKT and the
//! rest become properties with lightly inferred types. On write, property
//! columns (inferred by scanning all features) come first, then a `geometry`
//! column holding WKT. RFC 4180 quoting is handled both ways.
//!
//! **"Lightly" is now a rule rather than a disposition.** A cell is typed only
//! when this module's own writer renders the value back to the exact text that
//! was read, so `007` stays `"007"` and `1.50` stays `"1.50"` — see
//! [`infer_value`] and `plans/same-format-fidelity.org` D17. Every cell's text
//! therefore survives a `csv -> csv` run; what does not is the geometry
//! column's WKT spelling and a `Double` column that one fractional row
//! widened, which is what `Format::same_format_losses` reports.

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

    // D17's gate decides per *column*, not per cell: one cell whose text this
    // spoke's writer cannot reproduce types the whole column as text. A ZIP
    // column is the case that forces it — `[007, 08001, 12345]` read per cell
    // returns two strings and a number, typed differently for no reason the
    // data supports, and `12345` is a ZIP in exactly the way `007` is.
    //
    // *Only a refused cell gates the column, not any untyped one.* A plain
    // `hello` beside a `7` is an ordinary mixed column and always was: the
    // type lattice in `schema::infer_columns` settles that one downstream, and
    // widening D17 to cover it would be a different decision than the one
    // taken.
    let typed: Vec<bool> = (0..keys.len())
        .map(|i| {
            Some(i) == geom_col
                || !rows[1..].iter().any(|row| refuses_typing(row.get(i).map_or("", |s| s)))
        })
        .collect();

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
            .map(|(i, key)| (Rc::clone(key), cell_value(row.get(i).map_or("", |s| s), typed[i])))
            .collect();
        features.push(Feature {
            geometry,
            properties,
            native: None,
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

/// The value for one cell, given whether its *column* survived the gate.
///
/// A cell is typed only when the inference is *reversible* — when this spoke's
/// own writer renders the inferred value back to the exact text that was read,
/// for every cell in the column ([`refuses_typing`]). An un-inferred cell
/// stays a string, which is always a correct reading of a CSV cell.
///
/// # Why the gate is here and not a list of exceptions
///
/// A CSV cell is untyped text, so the reader chooses whether it is a number;
/// the text is the only statement the file makes. Inferring unconditionally
/// discarded that statement, and `plans/native-inventory.org` § CSV measured
/// what it cost: `007` → `7` and `08001` → `8001` (leading zeros are data in
/// this domain — ZIP, FIPS, census tract), `12345678901234567890` →
/// `12345678901234567000` (past `i64`, through an `f64`, so *a different
/// number*), `1.50` → `1.5`, `TRUE` → `true`.
///
/// The gate closes all of those at once, and two nobody enumerated: `+7` and
/// `1e2` both normalised silently too. That is the argument for a rule over
/// the named-exception list this nearly became — see
/// `plans/same-format-fidelity.org` D17.
///
/// # It fails safe, in two directions
///
/// An un-inferred cell is a correct string property rather than a wrong
/// number. And the verdict is column-wide, so a column holding both
/// `12345678901234567890` and `42` keeps both as text rather than typing one
/// and not the other — which is also what [`crate::schema::infer_columns`]
/// would have settled downstream for a tabular target, now settled once at
/// the read instead of twice with different answers.
fn cell_value(cell: &str, typed: bool) -> JsonValue {
    if cell.is_empty() {
        return JsonValue::Null;
    }
    // `typed` already carries the gate's verdict for this whole column, so the
    // render is not repeated per row.
    match infer_typed(cell).filter(|_| typed) {
        Some(value) => value,
        None => JsonValue::String(cell.to_string()),
    }
}

/// Whether the gate *refuses* `cell`: a typed reading exists and this spoke's
/// writer does not render it back to the text that was read.
///
/// One of these in a column types the whole column as text. A cell with no
/// typed reading at all (`hello`) returns `false` — it is not a refusal, just
/// a string, and it gates nothing.
fn refuses_typing(cell: &str) -> bool {
    infer_typed(cell).is_some_and(|value| cell_text(&unwidened(&value)) != cell)
}

/// The typed reading of `cell`, before the reversibility gate — `None` when
/// nothing but a string fits.
fn infer_typed(cell: &str) -> Option<JsonValue> {
    if cell.eq_ignore_ascii_case("true") {
        return Some(JsonValue::Bool(true));
    }
    if cell.eq_ignore_ascii_case("false") {
        return Some(JsonValue::Bool(false));
    }
    // Only attempt numbers on something that starts numeric, so words like
    // "NaN"/"inf" (which parse as floats) stay strings.
    let numeric_start = cell
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'));
    if numeric_start {
        if let Ok(i) = cell.parse::<i64>() {
            return Some(JsonValue::Int(i));
        }
        if let Ok(f) = cell.parse::<f64>()
            && f.is_finite()
        {
            return Some(JsonValue::Float(f));
        }
    }
    None
}

/// The [`Cell`] this spoke's writer holds `value` in when nothing else in the
/// column widens it — the reference [`infer_value`]'s gate renders through, so
/// the gate is defined against the real writer rather than a second opinion
/// about how a value is spelled.
///
/// *A column type is a property of the column, not the cell*, so this cannot
/// see a widening another row forces: a mixed `[9007199254740993, 1.5]` column
/// is typed `Double` and loses the integer's low bits on the way out, which
/// `Format::same_format_losses` still declares.
fn unwidened(value: &JsonValue) -> Cell {
    match value {
        JsonValue::Bool(b) => Cell::Bool(*b),
        JsonValue::Int(n) => Cell::Int(*n),
        JsonValue::Float(f) => Cell::Double(*f),
        other => Cell::Str(other.to_json_string()),
    }
}

fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(n) => n.to_string(),
        Cell::Double(d) => d.to_string(),
        Cell::Str(s) => s.clone(),
        // CSV is text; bytes render as hex, the same spelling the JSON
        // serializer and `materialize` use when a byte column meets a target
        // with no byte-level type.
        Cell::Bytes(b) => b.iter().map(|byte| format!("{byte:02x}")).collect(),
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
            ("pop".into(), JsonValue::Int(100))
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
                        ("n".into(), JsonValue::Int(3)),
                    ],
                    native: None,
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
                    native: None,
                },
            ]);
        let bytes = write(&fc);
        let back = read(&bytes).unwrap();
        assert_eq!(back.features.len(), 2);
        assert_eq!(back.features[0].geometry, fc.features[0].geometry);
        assert_eq!(back.features[1].geometry, fc.features[1].geometry);
        assert_eq!(back.features[0].properties[1].1, JsonValue::Int(3));
    }

    /// D17's gate, one row per spelling — the table in
    /// `plans/same-format-fidelity.org` D17, asserted rather than described.
    ///
    /// A *refusal* is the case where a typed reading exists and the writer
    /// cannot put the text back; that is what gates a column. A cell with no
    /// typed reading at all is not a refusal and gates nothing.
    #[test]
    fn a_cell_is_refused_only_when_the_writer_cannot_put_its_text_back() {
        // Reversible: the inference survives.
        for cell in ["7", "-7", "0", "1.5", "-0.25", "true", "false"] {
            assert!(!refuses_typing(cell), "{cell:?} round-trips and should be typed");
        }
        // Not reversible: the text is the only honest reading.
        for cell in [
            "007",                    // a leading zero is data in this domain
            "08001",                  // and this one made 8001, a different ZIP
            "+7",                     // a sign the writer does not re-emit
            "1.50",                   // the trailing zero
            "22.250",                 //
            "1.0",                    // `Cell::Double`'s own rendering is `1`
            "1e2",                    // notation
            "3E-1",                   //
            "TRUE",                   // letter case
            "False",                  //
            "12345678901234567890",   // past `i64`, so an `f64` and a wrong number
            "-0.0",                   // the sign of zero
        ] {
            assert!(refuses_typing(cell), "{cell:?} does not survive a re-render");
        }
        // Never typeable, and so never a refusal: an ordinary mixed column is
        // the type lattice's business, not D17's.
        for cell in ["hello", "NaN", "inf", "", "POINT (1 2)"] {
            assert!(!refuses_typing(cell), "{cell:?} has no typed reading to refuse");
        }
    }

    /// The verdict is column-wide: one refused cell types its column as text,
    /// so a ZIP column does not come back with `12345` as a number beside
    /// `007` as a string.
    ///
    /// This is the case that chose per-column over per-cell. Both readings
    /// keep every cell's *text*, so a `csv -> csv` run cannot tell them apart
    /// — the difference is only visible in a target with per-feature property
    /// types, which is why it is asserted on the values rather than on output
    /// bytes.
    #[test]
    fn one_refused_cell_types_its_whole_column_as_text() {
        let fc = read(b"zip,geometry\n007,\"POINT (1 2)\"\n08001,\"POINT (3 4)\"\n12345,\"POINT (5 6)\"\n")
            .unwrap();
        let zips: Vec<&JsonValue> = fc.features.iter().map(|f| &f.properties[0].1).collect();
        assert_eq!(
            zips,
            vec![
                &JsonValue::String("007".into()),
                &JsonValue::String("08001".into()),
                &JsonValue::String("12345".into()),
            ],
            "a ZIP with no leading zero was typed differently from its column",
        );
    }

    /// And a column with nothing refused keeps its types, so the gate is not
    /// a blanket retreat to text.
    ///
    /// Paired with the test above deliberately: together they pin that the
    /// verdict follows the *column's* cells and nothing else.
    #[test]
    fn a_column_with_nothing_refused_is_still_typed() {
        let fc = read(b"n,f,b,mixed,geometry\n7,1.5,true,7,\"POINT (1 2)\"\n8,2.5,false,hello,\"POINT (3 4)\"\n")
            .unwrap();
        let row0 = &fc.features[0].properties;
        assert_eq!(row0[0].1, JsonValue::Int(7));
        assert_eq!(row0[1].1, JsonValue::Float(1.5));
        assert_eq!(row0[2].1, JsonValue::Bool(true));
        // `hello` has no typed reading, so it is not a refusal and does not
        // gate the `7` beside it — that mix is the type lattice's to settle.
        assert_eq!(row0[3].1, JsonValue::Int(7));
        assert_eq!(fc.features[1].properties[3].1, JsonValue::String("hello".into()));
    }

    /// The gate is defined against `cell_text`, so this spoke's reader and
    /// writer are a *fixed point* on cell text: whatever text survives one
    /// hop survives every later one.
    ///
    /// Stated as its own test because the property is what makes D17 a rule
    /// rather than a list — a future change to `cell_text` that broke it would
    /// otherwise only show up as a surprising warning.
    #[test]
    fn surviving_text_survives_a_second_hop_too() {
        let src = "zip,big,money,flag,geometry\n\
                   007,12345678901234567890,1.50,TRUE,\"POINT (1 2)\"\n";
        let once = write(&read(src.as_bytes()).unwrap());
        let twice = write(&read(&once).unwrap());
        assert_eq!(
            once, twice,
            "a second hop changed cells the first one preserved:\n{}\n{}",
            String::from_utf8_lossy(&once),
            String::from_utf8_lossy(&twice),
        );
        let text = String::from_utf8(once).unwrap();
        for cell in ["007", "12345678901234567890", "1.50", "TRUE"] {
            assert!(text.contains(cell), "{cell:?} missing from {text:?}");
        }
    }

    /// A refused cell does not cost its column mates their *text* — the
    /// fail-safe direction of the gate, and the reason a past-`i64` integer
    /// beside a small one keeps both.
    #[test]
    fn a_refused_cell_does_not_cost_its_column_mates_their_text() {
        let src = "big,geometry\n12345678901234567890,\"POINT (1 2)\"\n42,\"POINT (3 4)\"\n";
        let text = String::from_utf8(write(&read(src.as_bytes()).unwrap())).unwrap();
        assert!(text.contains("12345678901234567890"), "{text:?}");
        assert!(text.contains("\n42,"), "the small integer was reformatted: {text:?}");
    }

    #[test]
    fn no_geometry_column_is_ok() {
        let fc = read(b"a,b\n1,2\n").unwrap();
        assert_eq!(fc.features.len(), 1);
        assert!(fc.features[0].geometry.is_none());
        assert_eq!(fc.features[0].properties.len(), 2);
    }
}
