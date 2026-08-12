//! dBase III/IV attribute table (`.dbf`) codec — Shapefile's attribute half.
//!
//! Layout: a 32-byte header (record count, header/record byte sizes, a
//! language-driver byte that is unreliable in practice — `.cpg`, when present,
//! is the trustworthy encoding source), then one 32-byte field descriptor per
//! column terminated by `0x0D`, then fixed-width records: a 1-byte deletion
//! flag followed by each field's fixed-width, space-padded text.

use crate::error::{Error, Result};
use crate::json::JsonValue;
use crate::schema::{Cell, Column, ColumnType};
use std::rc::Rc;

const HEADER_LEN: usize = 32;
const FIELD_DESCRIPTOR_LEN: usize = 32;
const FIELD_TERMINATOR: u8 = 0x0D;
const FILE_TERMINATOR: u8 = 0x1A;

/// A decoded field descriptor.
struct Field {
    name: String,
    ty: u8,
    len: usize,
    decimals: usize,
}

/// One physical `.dbf` record. `deleted` records must be dropped in lockstep
/// with the corresponding `.shp` record by the caller (see
/// `shapefile::reader`), since `.dbf` and `.shp` are zipped by row position and
/// `.shp` has no deletion flag of its own.
pub struct Record {
    pub deleted: bool,
    pub properties: Vec<(Rc<str>, JsonValue)>,
}

/// The text encoding to decode `C` (character) field bytes with.
#[derive(Clone, Copy)]
pub enum Encoding {
    Utf8,
    /// ISO-8859-1: every byte maps directly to the Unicode code point of the
    /// same value. Used both as the declared Latin-1 encoding and as the
    /// fallback when `.cpg` names a codepage we don't have a table for (e.g.
    /// Windows-1252) — an approximation documented as a known gap, not a full
    /// codepage library, to keep the crate dependency-free.
    Latin1,
}

/// Resolve the encoding to decode field text with, from an optional `.cpg`
/// file's contents. Absent a `.cpg`, UTF-8 is attempted first at decode time
/// with a Latin-1 fallback (see [`decode_text`]); this only handles the case
/// where a codepage *was* declared.
pub fn encoding_from_cpg(cpg: Option<&str>) -> Encoding {
    match cpg {
        Some(s) if s.trim().eq_ignore_ascii_case("utf-8") || s.trim().eq_ignore_ascii_case("utf8") => {
            Encoding::Utf8
        }
        Some(_) => Encoding::Latin1,
        None => Encoding::Utf8,
    }
}

/// Parse a `.dbf` file into its records. `cpg_declared` is whether a `.cpg`
/// file was present (see [`encoding_from_cpg`]) — when it wasn't, a UTF-8
/// decode failure per field falls back to Latin-1 rather than erroring.
pub fn read(data: &[u8], encoding: Encoding, cpg_declared: bool) -> Result<Vec<Record>> {
    if data.len() < HEADER_LEN {
        return Err(Error::Convert("shapefile: dbf file shorter than its header".into()));
    }
    let num_records = u32_at(data, 4)? as usize;
    let header_size = u16_at(data, 8)? as usize;
    let record_size = u16_at(data, 10)? as usize;

    let fields = read_fields(data)?;
    let keys: Vec<Rc<str>> = fields.iter().map(|f| Rc::from(f.name.as_str())).collect();

    let mut records = Vec::with_capacity(num_records);
    let mut pos = header_size;
    for _ in 0..num_records {
        if pos >= data.len() || data[pos] == FILE_TERMINATOR {
            break;
        }
        let rec = data
            .get(pos..pos + record_size)
            .ok_or_else(|| Error::Convert("shapefile: truncated dbf record".into()))?;
        pos += record_size;

        let deleted = rec[0] == b'*';
        let mut properties = Vec::with_capacity(fields.len());
        let mut field_pos = 1usize; // skip the deletion flag byte
        for (field, key) in fields.iter().zip(&keys) {
            let raw = rec
                .get(field_pos..field_pos + field.len)
                .ok_or_else(|| Error::Convert("shapefile: dbf record shorter than its schema".into()))?;
            field_pos += field.len;
            let value = decode_field(field, raw, encoding, cpg_declared)?;
            properties.push((Rc::clone(key), value));
        }
        records.push(Record { deleted, properties });
    }
    Ok(records)
}

fn read_fields(data: &[u8]) -> Result<Vec<Field>> {
    let mut fields = Vec::new();
    let mut pos = HEADER_LEN;
    while pos < data.len() && data[pos] != FIELD_TERMINATOR {
        let desc = data
            .get(pos..pos + FIELD_DESCRIPTOR_LEN)
            .ok_or_else(|| Error::Convert("shapefile: truncated dbf field descriptor".into()))?;
        let name_end = desc[0..11].iter().position(|&b| b == 0).unwrap_or(11);
        let name = String::from_utf8_lossy(&desc[0..name_end]).into_owned();
        fields.push(Field {
            name,
            ty: desc[11],
            len: desc[16] as usize,
            decimals: desc[17] as usize,
        });
        pos += FIELD_DESCRIPTOR_LEN;
    }
    Ok(fields)
}

fn decode_field(field: &Field, raw: &[u8], encoding: Encoding, cpg_declared: bool) -> Result<JsonValue> {
    Ok(match field.ty {
        b'C' => JsonValue::String(decode_text(raw, encoding, cpg_declared).trim_end().to_string()),
        b'D' => {
            // Raw YYYYMMDD digits, rendered as ISO-8601 text (matches
            // parquet/reader.rs's date-as-formatted-string convention).
            let text = std::str::from_utf8(raw).unwrap_or("").trim();
            if text.len() == 8 && text.bytes().all(|b| b.is_ascii_digit()) {
                JsonValue::String(format!("{}-{}-{}", &text[0..4], &text[4..6], &text[6..8]))
            } else {
                JsonValue::Null
            }
        }
        b'L' => match raw.first() {
            Some(b'T' | b't' | b'Y' | b'y') => JsonValue::Bool(true),
            Some(b'F' | b'f' | b'N' | b'n') => JsonValue::Bool(false),
            _ => JsonValue::Null,
        },
        b'N' | b'F' => {
            let text = std::str::from_utf8(raw).unwrap_or("").trim();
            match text.is_empty() {
                true => JsonValue::Null,
                false => match text.parse::<f64>() {
                    Ok(v) => JsonValue::Number { value: v, is_int: field.decimals == 0 },
                    Err(_) => JsonValue::Null, // e.g. an overflow-marker field of '*'s
                },
            }
        }
        // Memo (M, needs a companion .dbt) and anything else unrecognized: out
        // of scope for this first version — record as null rather than
        // misinterpreting the raw bytes.
        _ => JsonValue::Null,
    })
}

fn decode_text(raw: &[u8], encoding: Encoding, cpg_declared: bool) -> String {
    match encoding {
        Encoding::Utf8 => match std::str::from_utf8(raw) {
            Ok(s) => s.to_string(),
            // No .cpg declared UTF-8 explicitly (encoding_from_cpg's default
            // guess) and the bytes aren't valid UTF-8: fall back to Latin-1
            // rather than lossily replacing bytes.
            Err(_) if !cpg_declared => latin1(raw),
            Err(_) => String::from_utf8_lossy(raw).into_owned(),
        },
        Encoding::Latin1 => latin1(raw),
    }
}

fn latin1(raw: &[u8]) -> String {
    raw.iter().map(|&b| b as char).collect()
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(at..at + 2)
            .ok_or_else(|| Error::Convert("shapefile: dbf header truncated".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4)
            .ok_or_else(|| Error::Convert("shapefile: dbf header truncated".into()))?
            .try_into()
            .unwrap(),
    ))
}

// --- writer ------------------------------------------------------------

/// A resolved DBF field type, sized from the actual column data.
struct DbfField {
    name: String,
    ty: u8,
    len: usize,
    decimals: usize,
}

/// Encode property columns (already resolved by [`crate::schema::infer_columns`])
/// as a `.dbf` byte string, all records active (undeleted).
pub fn write(columns: &[Column]) -> Vec<u8> {
    let num_rows = columns.first().map(|c| c.values.len()).unwrap_or(0);
    let fields: Vec<DbfField> = columns.iter().map(dbf_field).collect();

    let header_size = HEADER_LEN + fields.len() * FIELD_DESCRIPTOR_LEN + 1;
    let record_size = 1 + fields.iter().map(|f| f.len).sum::<usize>();

    let mut out = Vec::with_capacity(header_size + record_size * num_rows + 1);
    // Header.
    out.push(0x03); // dBase III, no memo
    out.extend_from_slice(&[0, 1, 1]); // last-update date: not meaningfully knowable here
    out.extend_from_slice(&(num_rows as u32).to_le_bytes());
    out.extend_from_slice(&(header_size as u16).to_le_bytes());
    out.extend_from_slice(&(record_size as u16).to_le_bytes());
    out.extend_from_slice(&[0u8; 20]); // reserved/flags/language-driver

    // Field descriptors.
    for f in &fields {
        let mut name = [0u8; 11];
        let bytes = f.name.as_bytes();
        let n = bytes.len().min(10);
        name[..n].copy_from_slice(&bytes[..n]);
        out.extend_from_slice(&name);
        out.push(f.ty);
        out.extend_from_slice(&[0u8; 4]); // field data address (unused on disk)
        out.push(f.len as u8);
        out.push(f.decimals as u8);
        out.extend_from_slice(&[0u8; 14]); // reserved
    }
    out.push(FIELD_TERMINATOR);

    // Records.
    for row in 0..num_rows {
        out.push(b' '); // active
        for (field, col) in fields.iter().zip(columns) {
            write_cell(&mut out, field, &col.values[row]);
        }
    }
    out.push(FILE_TERMINATOR);
    out
}

fn dbf_field(col: &Column) -> DbfField {
    let name = col.name.clone();
    match col.ty {
        ColumnType::Bool => DbfField { name, ty: b'L', len: 1, decimals: 0 },
        ColumnType::Int64 => {
            let len = col
                .values
                .iter()
                .filter_map(|c| match c {
                    Cell::Int(v) => Some(v.to_string().len()),
                    _ => None,
                })
                .max()
                .unwrap_or(1)
                .clamp(1, 20);
            DbfField { name, ty: b'N', len, decimals: 0 }
        }
        ColumnType::Double => {
            const DECIMALS: usize = 6;
            let len = col
                .values
                .iter()
                .filter_map(|c| match c {
                    Cell::Double(v) => Some(format!("{v:.DECIMALS$}").len()),
                    _ => None,
                })
                .max()
                .unwrap_or(1 + DECIMALS + 1)
                .clamp(DECIMALS + 2, 32);
            DbfField { name, ty: b'N', len, decimals: DECIMALS }
        }
        ColumnType::String => {
            // Sized to the longest observed UTF-8 byte length, capped at DBF's
            // 255-byte field-width limit. Longer values are truncated on write
            // (at a char boundary) — an accepted, documented gap rather than an
            // error (mirrors no other format in this crate having a per-field
            // width cap to begin with).
            let len = col
                .values
                .iter()
                .filter_map(|c| match c {
                    Cell::Str(s) => Some(s.len()),
                    _ => None,
                })
                .max()
                .unwrap_or(1)
                .clamp(1, 255);
            DbfField { name, ty: b'C', len, decimals: 0 }
        }
    }
}

fn write_cell(out: &mut Vec<u8>, field: &DbfField, cell: &Cell) {
    let start = out.len();
    match (field.ty, cell) {
        (b'L', Cell::Bool(v)) => out.push(if *v { b'T' } else { b'F' }),
        (b'L', _) => out.push(b'?'),
        (b'N', Cell::Int(v)) => out.extend_from_slice(format!("{v:>0$}", field.len).as_bytes()),
        (b'N', Cell::Double(v)) => {
            out.extend_from_slice(format!("{v:>0$.1$}", field.len, field.decimals).as_bytes())
        }
        (b'N', _) => out.extend(std::iter::repeat_n(b' ', field.len)),
        (b'C', Cell::Str(s)) => {
            let truncated = truncate_at_char_boundary(s, field.len);
            out.extend_from_slice(truncated.as_bytes());
            out.extend(std::iter::repeat_n(b' ', field.len - truncated.len()));
        }
        (b'C', _) => out.extend(std::iter::repeat_n(b' ', field.len)),
        _ => unreachable!("dbf_field only emits L/N/C"),
    }
    debug_assert_eq!(out.len() - start, field.len, "dbf field write must be exactly its declared width");
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::infer_columns;
    use crate::feature::Feature;

    fn features(props: &[&[(&str, JsonValue)]]) -> Vec<Feature> {
        props
            .iter()
            .map(|p| Feature {
                geometry: None,
                properties: p.iter().map(|(k, v)| (Rc::from(*k), v.clone())).collect(),
            })
            .collect()
    }

    #[test]
    fn round_trips_all_field_types() {
        let feats = features(&[
            &[
                ("name", JsonValue::String("Alice".into())),
                ("age", JsonValue::Number { value: 30.0, is_int: true }),
                ("score", JsonValue::Number { value: 1.5, is_int: false }),
                ("active", JsonValue::Bool(true)),
            ],
            &[
                ("name", JsonValue::String("Bob".into())),
                ("age", JsonValue::Number { value: -5.0, is_int: true }),
                ("score", JsonValue::Number { value: -2.25, is_int: false }),
                ("active", JsonValue::Bool(false)),
            ],
        ]);
        let columns = infer_columns(&feats);
        let bytes = write(&columns);
        let records = read(&bytes, Encoding::Utf8, false).unwrap();
        assert_eq!(records.len(), 2);
        assert!(!records[0].deleted);

        let get = |props: &[(Rc<str>, JsonValue)], k: &str| {
            props.iter().find(|(name, _)| &**name == k).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(get(&records[0].properties, "name").as_str(), Some("Alice"));
        assert_eq!(get(&records[0].properties, "age").as_f64(), Some(30.0));
        assert_eq!(get(&records[0].properties, "score").as_f64(), Some(1.5));
        assert_eq!(get(&records[0].properties, "active"), JsonValue::Bool(true));
        assert_eq!(get(&records[1].properties, "age").as_f64(), Some(-5.0));
        assert_eq!(get(&records[1].properties, "score").as_f64(), Some(-2.25));
        assert_eq!(get(&records[1].properties, "active"), JsonValue::Bool(false));
    }

    #[test]
    fn null_cells_round_trip_as_null() {
        let feats = features(&[
            &[("a", JsonValue::Number { value: 1.0, is_int: true })],
            &[], // "a" missing -> Null
        ]);
        let columns = infer_columns(&feats);
        let bytes = write(&columns);
        let records = read(&bytes, Encoding::Utf8, false).unwrap();
        let get = |props: &[(Rc<str>, JsonValue)], k: &str| {
            props.iter().find(|(name, _)| &**name == k).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(get(&records[1].properties, "a"), JsonValue::Null);
    }

    #[test]
    fn deleted_flag_is_detected() {
        // Hand-build a minimal one-field, one-record dbf with the deletion flag set.
        let columns = infer_columns(&features(&[&[("a", JsonValue::String("x".into()))]]));
        let mut bytes = write(&columns);
        // Record starts right after the header + 1 field descriptor + terminator.
        let header_size = u16_at(&bytes, 8).unwrap() as usize;
        bytes[header_size] = b'*';
        let records = read(&bytes, Encoding::Utf8, false).unwrap();
        assert!(records[0].deleted);
    }

    #[test]
    fn latin1_fallback_decodes_non_utf8_bytes() {
        // 0xE9 is 'é' in Latin-1 but not valid standalone UTF-8.
        let raw = [0xE9u8, b' ', b' '];
        let field = Field { name: "n".into(), ty: b'C', len: 3, decimals: 0 };
        let v = decode_field(&field, &raw, Encoding::Utf8, false).unwrap();
        assert_eq!(v.as_str(), Some("é"));
    }

    #[test]
    fn date_field_renders_iso8601() {
        let field = Field { name: "d".into(), ty: b'D', len: 8, decimals: 0 };
        let v = decode_field(&field, b"20200101", Encoding::Utf8, false).unwrap();
        assert_eq!(v.as_str(), Some("2020-01-01"));
    }

    #[test]
    fn long_string_is_truncated_at_char_boundary() {
        let feats = features(&[&[("s", JsonValue::String("a".repeat(300)))]]);
        let columns = infer_columns(&feats);
        let bytes = write(&columns);
        let records = read(&bytes, Encoding::Utf8, false).unwrap();
        assert_eq!(records[0].properties[0].1.as_str().unwrap().len(), 255);
    }
}
