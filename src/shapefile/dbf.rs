//! dBase III/IV attribute table (`.dbf`) codec — Shapefile's attribute half.
//!
//! Layout: a 32-byte header (record count, header/record byte sizes, a
//! language-driver byte that is unreliable in practice — `.cpg`, when present,
//! is the trustworthy encoding source), then one 32-byte field descriptor per
//! column terminated by `0x0D`, then fixed-width records: a 1-byte deletion
//! flag followed by each field's fixed-width, space-padded text.

use crate::error::{Error, Result};
use crate::json::JsonValue;
use crate::schema::{Cell, Column, ColumnType, DeclType, FieldDecl, declared_or};
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Encoding {
    Utf8,
    /// ISO-8859-1, declared as such: every byte maps directly to the Unicode
    /// code point of the same value, which is exactly what the codepage means.
    Latin1,
    /// A `.cpg` naming a codepage this build has no table for (Windows-1252,
    /// a DOS codepage, a multi-byte one), decoded as ISO-8859-1 anyway — an
    /// approximation, not a full codepage library, to keep the crate
    /// dependency-free. Exact for ASCII bytes and wrong wherever the named
    /// codepage differs; [`TextLosses::approximated`] counts the values that
    /// held a byte it could be wrong about (`plans/encodings.org` E1).
    Latin1Approximating,
}

/// Resolve the encoding to decode field text with, from an optional `.cpg`
/// file's contents. Absent a `.cpg`, UTF-8 is attempted first at decode time
/// with a Latin-1 fallback (see [`decode_text`]); this only handles the case
/// where a codepage *was* declared.
pub fn encoding_from_cpg(cpg: Option<&str>) -> Encoding {
    let Some(cpg) = cpg else {
        return Encoding::Utf8;
    };
    match cpg.trim().to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
        "utf8" => Encoding::Utf8,
        "iso88591" | "88591" | "latin1" | "l1" | "cp819" => Encoding::Latin1,
        _ => Encoding::Latin1Approximating,
    }
}

/// What decoding a `.dbf`'s text could not do exactly, counted per value, so
/// the reader can say so (`plans/lossy-conversion-warnings.org` § 2b).
#[derive(Debug, Default, PartialEq)]
pub struct TextLosses {
    /// Values holding a non-ASCII byte, decoded under
    /// [`Encoding::Latin1Approximating`]. An all-ASCII value is exact under
    /// every codepage, so it is not counted.
    pub approximated: usize,
    /// Values a `.cpg` declared UTF-8 that were not, whose invalid bytes were
    /// replaced with U+FFFD.
    pub replaced: usize,
}

/// Parse a `.dbf` file into its records. `cpg_declared` is whether a `.cpg`
/// file was present (see [`encoding_from_cpg`]) — when it wasn't, a UTF-8
/// decode failure per field falls back to Latin-1 rather than erroring.
/// `losses` counts the text values that did not decode exactly.
pub fn read(data: &[u8], encoding: Encoding, cpg_declared: bool, losses: &mut TextLosses) -> Result<Vec<Record>> {
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
            let value = decode_field(field, raw, encoding, cpg_declared, losses)?;
            properties.push((Rc::clone(key), value));
        }
        records.push(Record { deleted, properties });
    }
    Ok(records)
}

/// The field descriptors' *declarations*, independent of any record — the
/// widths and decimal counts the `.dbf` header states, which are exactly what
/// re-deriving a schema from today's values throws away.
///
/// A separate entry point rather than a second return value from [`read`]
/// because it costs only the header: [`read`]'s callers that do not carry a
/// schema pay nothing, and this reads no records at all.
///
/// The mapping to [`FieldDecl`]'s FlatGeobuf-derived vocabulary follows GDAL's
/// own, verified against `ogr2ogr`-written FlatGeobuf: a character field's
/// length is a *width*, a numeric field's length is a *precision* (significant
/// digits) with its `decimals` as the *scale*. dBase declares nothing about
/// nullability, uniqueness or keys, so those three stay `None` — a `.dbf` that
/// went through a FlatGeobuf transit carrying them will lose them here, and
/// that is the target's incapacity rather than a defect.
pub fn read_schema(data: &[u8]) -> Result<Vec<FieldDecl>> {
    if data.len() < HEADER_LEN {
        return Ok(Vec::new());
    }
    Ok(read_fields(data)?
        .into_iter()
        .map(|f| {
            let mut decl = FieldDecl::new(f.name);
            // The one-byte field type is a declaration in its own right: a
            // `.dbf` `L` column holding no rows is still a logical column, and
            // `D` is a date rather than the text it is stored as.
            decl.ty = match f.ty {
                b'L' => Some(DeclType::Bool),
                b'D' => Some(DeclType::Date),
                b'C' => Some(DeclType::String),
                // dBase numerics are decimal text; `decimals == 0` is the only
                // signal distinguishing an integer column from a fractional one.
                b'N' if f.decimals == 0 => Some(DeclType::Int64),
                b'N' | b'F' => Some(DeclType::Float64),
                _ => None,
            };
            match f.ty {
                b'N' | b'F' => {
                    decl.precision = Some(f.len as u32);
                    decl.scale = Some(f.decimals as u32);
                }
                _ => decl.width = Some(f.len as u32),
            }
            decl
        })
        .filter(|d| !d.is_empty())
        .collect())
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

fn decode_field(
    field: &Field,
    raw: &[u8],
    encoding: Encoding,
    cpg_declared: bool,
    losses: &mut TextLosses,
) -> Result<JsonValue> {
    Ok(match field.ty {
        b'C' => JsonValue::String(decode_text(raw, encoding, cpg_declared, losses).trim_end().to_string()),
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
                // `decimals == 0` is dBase's only signal that a numeric
                // field is an integer, so parse it as one — text straight to
                // `i64`, never via an `f64`, so a 19-digit key stays itself.
                false if field.decimals == 0 => match text.parse::<i64>() {
                    Ok(n) => JsonValue::Int(n),
                    // Too wide for an `i64`, or not an integer after all:
                    // fall back rather than invent a value.
                    Err(_) => match text.parse::<f64>() {
                        Ok(v) => JsonValue::Float(v),
                        Err(_) => JsonValue::Null,
                    },
                },
                false => match text.parse::<f64>() {
                    Ok(v) => JsonValue::Float(v),
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

fn decode_text(raw: &[u8], encoding: Encoding, cpg_declared: bool, losses: &mut TextLosses) -> String {
    match encoding {
        Encoding::Utf8 => match std::str::from_utf8(raw) {
            Ok(s) => s.to_string(),
            // No .cpg declared UTF-8 explicitly (encoding_from_cpg's default
            // guess) and the bytes aren't valid UTF-8: fall back to Latin-1
            // rather than lossily replacing bytes.
            Err(_) if !cpg_declared => latin1(raw),
            Err(_) => {
                losses.replaced += 1;
                String::from_utf8_lossy(raw).into_owned()
            }
        },
        Encoding::Latin1 => latin1(raw),
        Encoding::Latin1Approximating => {
            if !raw.is_ascii() {
                losses.approximated += 1;
            }
            latin1(raw)
        }
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
/// Encode property columns (already resolved by [`crate::schema::infer_columns`])
/// into a `.dbf` byte buffer, plus any non-fatal warnings about lossy
/// truncation dBase's fixed-width fields forced — Shapefile's `.dbf` is the
/// *only* format in this crate with fixed-width fields, so it's the only one
/// that can lose data this way. Two kinds:
/// - a property *value* longer than 255 bytes (dBase's per-field byte-width
///   limit): truncated at a char boundary, same as before, now warned about.
/// - a property *name* longer than 10 bytes (dBase's field-name limit):
///   truncated at a char boundary and warned about.
///
/// Errors, rather than truncating, when truncation can't safely paper over
/// the loss: two property names truncating to the *same* 10-byte field name
/// (silently merging two distinct columns into one ambiguous field is worse
/// than either losing data outright), a numeric value whose formatted width
/// exceeds even the 255-byte cap (unlike text, cutting off a number's digits
/// produces a *different, wrong* number, never an acceptable silent choice —
/// this also closes a real bug: the old fixed 32-byte cap on `Double` fields
/// could silently emit more bytes than the field's declared width for a
/// large-magnitude value, desyncing every field/row after it), or a
/// header/record byte count past what dBase's 16-bit size fields can encode.
pub fn write(columns: &[Column]) -> Result<(Vec<u8>, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut fields = Vec::with_capacity(columns.len());
    for col in columns {
        fields.push(dbf_field(col, &mut warnings)?);
    }
    resolve_field_names(columns, &mut fields, &mut warnings)?;

    let num_rows = columns.first().map(|c| c.values.len()).unwrap_or(0);
    let header_size = HEADER_LEN + fields.len() * FIELD_DESCRIPTOR_LEN + 1;
    let record_size = 1 + fields.iter().map(|f| f.len).sum::<usize>();
    if header_size > u16::MAX as usize || record_size > u16::MAX as usize {
        return Err(Error::Convert(format!(
            "shapefile: too many/too-wide .dbf columns to encode — header would be {header_size} bytes, \
             record {record_size} bytes, but dBase's own size fields are 16-bit (max {})",
            u16::MAX
        )));
    }

    let mut out = Vec::with_capacity(header_size + record_size * num_rows + 1);
    // Header.
    out.push(0x03); // dBase III, no memo
    out.extend_from_slice(&[0, 1, 1]); // last-update date: not meaningfully knowable here
    out.extend_from_slice(&(num_rows as u32).to_le_bytes());
    out.extend_from_slice(&(header_size as u16).to_le_bytes());
    out.extend_from_slice(&(record_size as u16).to_le_bytes());
    out.extend_from_slice(&[0u8; 20]); // reserved/flags/language-driver

    // Field descriptors. `f.name` is already truncated to <=10 bytes by
    // `resolve_field_names`, but the fixed-size copy below still clamps
    // defensively rather than trusting that invariant blindly.
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
    Ok((out, warnings))
}

/// Truncate each field's name to dBase's 10-byte limit (char-boundary safe,
/// like [`truncate_at_char_boundary`] already does for values), warning when
/// that actually lost anything, and erroring if two distinct property names
/// collapse to the same truncated name.
fn resolve_field_names(columns: &[Column], fields: &mut [DbfField], warnings: &mut Vec<String>) -> Result<()> {
    let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (col, field) in columns.iter().zip(fields.iter_mut()) {
        let truncated = truncate_at_char_boundary(&col.name, 10).to_string();
        if truncated.len() < col.name.len() {
            warnings.push(format!(
                "shapefile: property name \"{}\" truncated to \"{truncated}\" in the .dbf (dBase field-name limit is 10 bytes)",
                col.name
            ));
        }
        field.name = truncated;
    }
    for (col, field) in columns.iter().zip(fields.iter()) {
        if let Some(&other) = seen.get(field.name.as_str()) {
            return Err(Error::Convert(format!(
                "shapefile: properties \"{other}\" and \"{}\" both truncate to the same 10-byte .dbf field name \"{}\" — rename one to avoid an ambiguous output file",
                col.name, field.name
            )));
        }
        seen.insert(field.name.as_str(), col.name.as_str());
    }
    Ok(())
}

/// Resolve one column to a `.dbf` field descriptor.
///
/// Every size below is the width the *data* needs, then widened by whatever
/// the source declared (see [`declared_or`]) — never narrowed to it. Without
/// that second step an 80-character field holding one-character values comes
/// back out one character wide, which is the single most workflow-breaking
/// loss in `plans/native-inventory.org`: the file still holds today's rows and
/// can no longer hold tomorrow's.
///
/// Which declaration slot applies depends on the type, per [`FieldDecl`]:
/// character fields take `width`, numeric fields take `precision` for the
/// field length and `scale` for the decimal count. dBase's logical field is
/// exactly one byte wide by definition, so a declaration cannot move it.
fn dbf_field(col: &Column, warnings: &mut Vec<String>) -> Result<DbfField> {
    let name = col.name.clone();
    let decl = col.decl.as_ref();
    // Two declared types have a `.dbf` field type of their own that the
    // inferred `ColumnType` cannot reach: a logical column and a date column
    // both store as text, so a column with no values left to infer from would
    // otherwise be written `C` and stop being either. The remaining declared
    // types add nothing here — dBase has one numeric field type, sized by
    // `precision`/`scale`, which the arms below already honour.
    match col.declared_type() {
        Some(DeclType::Bool) => return Ok(DbfField { name, ty: b'L', len: 1, decimals: 0 }),
        Some(DeclType::Date) => return Ok(DbfField { name, ty: b'D', len: 8, decimals: 0 }),
        _ => {}
    }
    match col.ty {
        ColumnType::Bool => Ok(DbfField { name, ty: b'L', len: 1, decimals: 0 }),
        ColumnType::Int64 => {
            // A genuine `i64`'s formatted length never exceeds 20 bytes
            // (`i64::MIN` is `-9223372036854775808`), well under dBase's
            // 255-byte cap — sized to the longest observed value with no
            // separate overflow case needed, unlike `Double` below.
            let len = col
                .values
                .iter()
                .filter_map(|c| match c {
                    Cell::Int(v) => Some(v.to_string().len()),
                    _ => None,
                })
                .max()
                .unwrap_or(1)
                .clamp(1, 255);
            // `decimals` stays 0 whatever the source declared: an integer
            // column has no decimal part, and `write_cell` formats an `Int`
            // without one, so a non-zero declaration here would put a width in
            // the header that the records do not match.
            let len = declared_or(decl.and_then(|d| d.precision), len).min(255);
            Ok(DbfField { name, ty: b'N', len, decimals: 0 })
        }
        ColumnType::Double => {
            // Sized to the longest *actual* formatted width (never clamped
            // downward from that — clamping down was the bug: a fixed-point
            // decimal rendering of a large-magnitude double can need far
            // more than a few dozen bytes, and Rust's `{:>width$}` pads a
            // too-narrow width rather than truncating, so a hardcoded cap
            // here used to silently desync every field/row after this one).
            // Only the *lower* bound is clamped, to cover an empty/all-null
            // column.
            // What `ogr2ogr` writes for an undeclared double: a `.dbf` field
            // of `Real (24.15)`. Was 6 until 2026-08-21, which silently rounded
            // `1.23456789012345` to `1.234568` — no warning, and no reason
            // beyond the number having been picked first. Fifteen is also about
            // as many decimal digits as an `f64` carries meaningfully, so it is
            // the reference implementation's answer *and* the representation's.
            //
            // Only the fallback: a source that declared a scale still wins,
            // just below.
            const DECIMALS: usize = 15;
            // A declared scale replaces the fallback *before* the widths are
            // measured, since the decimal count is what the formatted width is
            // measured against.
            let decimals = decl.and_then(|d| d.scale).map(|s| s as usize).unwrap_or(DECIMALS);
            // A huge magnitude needs its *integer* digits and has no room for
            // fractional ones: `1e240` is 241 characters before the decimal
            // point, and an `f64` carries about 17 significant digits total, so
            // every one of those decimals is padding. Trim the fallback until
            // the widest value fits dBase's 255 bytes rather than refusing a
            // file over digits that were never information.
            //
            // Only the *fallback* is trimmed — a declared scale is the source
            // stating a fact, and is left alone to fail loudly below if it
            // cannot fit.
            let integer_digits = col
                .values
                .iter()
                .filter_map(|c| match c {
                    Cell::Double(v) => Some(format!("{v:.0}").len()),
                    _ => None,
                })
                .max()
                .unwrap_or(1);
            let decimals = match decl.and_then(|d| d.scale) {
                Some(_) => decimals,
                // 255 = integer digits + '.' + decimals.
                None => decimals.min(255usize.saturating_sub(integer_digits + 1)),
            };
            let len = col
                .values
                .iter()
                .filter_map(|c| match c {
                    Cell::Double(v) => Some(format!("{v:.decimals$}").len()),
                    _ => None,
                })
                .max()
                .unwrap_or(1 + decimals + 1)
                .max(decimals + 2);
            if len > 255 {
                return Err(Error::Convert(format!(
                    "shapefile: property \"{}\" has a value {len} bytes wide as fixed-point decimal text, \
                     wider than a .dbf numeric field can be (255-byte limit) — truncating a number's digits \
                     would silently produce a different, wrong value",
                    col.name
                )));
            }
            // The declaration widens the field but cannot push it past
            // dBase's own 255-byte limit; the error above already established
            // that the *data* fits inside it.
            let len = declared_or(decl.and_then(|d| d.precision), len).min(255);
            Ok(DbfField { name, ty: b'N', len, decimals })
        }
        // dBase has no byte-level field type — GDAL's shapefile driver has no
        // OFTBinary either — so bytes land in a character field as the hex the
        // rest of the crate renders them with. The declaration is a genuine
        // capability gap and `decl_recast_warning` says so.
        ColumnType::Binary | ColumnType::String => {
            // Sized to the longest observed UTF-8 byte length, capped at DBF's
            // 255-byte field-width limit. Longer values are truncated on write
            // (at a char boundary) — an accepted, documented gap (unlike the
            // numeric case above, text truncation is at least still valid,
            // readable text) rather than an error, but now surfaced as a
            // warning instead of a silent one.
            let true_max = col
                .values
                .iter()
                .filter_map(|c| match c {
                    Cell::Str(s) => Some(s.len()),
                    _ => None,
                })
                .max()
                .unwrap_or(1);
            if true_max > 255 {
                warnings.push(format!(
                    "shapefile: property \"{}\" has values up to {true_max} bytes long, truncated to 255 bytes in the .dbf (dBase field-width limit)",
                    col.name
                ));
            }
            let len = declared_or(decl.and_then(|d| d.width), true_max).clamp(1, 255);
            Ok(DbfField { name, ty: b'C', len, decimals: 0 })
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
        // dBase stores a date as eight digits, `YYYYMMDD`. The reader hands
        // dates back in ISO form, so accept that spelling too rather than
        // requiring a caller to un-format what we just formatted; anything
        // that is not eight digits after separators are dropped is written
        // blank, which is dBase's own "no date".
        (b'D', Cell::Str(s)) => {
            let digits: String = s.chars().filter(char::is_ascii_digit).collect();
            match digits.len() == 8 {
                true => out.extend_from_slice(digits.as_bytes()),
                false => out.extend(std::iter::repeat_n(b' ', 8)),
            }
        }
        (b'D', _) => out.extend(std::iter::repeat_n(b' ', 8)),
        _ => unreachable!("dbf_field only emits L/N/C/D"),
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
    /// **Every `.dbf` field type this reader accepts, and what it means.**
    /// Last of the four inbound tables (see `geopackage/reader.rs`,
    /// `flatgeobuf/reader.rs`, `parquet/reader.rs`), all written 2026-08-21.
    ///
    /// dBase is the narrowest vocabulary of the four — one numeric field type,
    /// disambiguated only by `decimals` — so this table is short and the
    /// *widening* it forces is the point: an `Int16` column written here comes
    /// back `Int64`, which is correct, and `Format::recast` says so.
    #[test]
    fn every_dbf_field_type_declares_what_dbase_gives_it() {
        use crate::schema::DeclType::*;
        // A one-field `.dbf` header, so field types this crate's own writer
        // never emits (`F`, `M`) are still covered — the round-trip sweep in
        // `convert.rs` can only reach the ones we write.
        let decl_of = |ty: u8, decimals: u8| {
            let mut data = vec![0u8; HEADER_LEN];
            let mut desc = vec![0u8; FIELD_DESCRIPTOR_LEN];
            desc[0] = b'v';
            desc[11] = ty;
            desc[16] = 10; // length
            desc[17] = decimals;
            data.extend_from_slice(&desc);
            data.push(FIELD_TERMINATOR);
            read_schema(&data).unwrap()[0].ty
        };
        assert_eq!(decl_of(b'L', 0), Some(Bool));
        assert_eq!(decl_of(b'D', 0), Some(Date));
        assert_eq!(decl_of(b'C', 0), Some(String));
        // `decimals == 0` is the only thing separating an integer column from a
        // fractional one: dBase stores both as decimal text.
        assert_eq!(decl_of(b'N', 0), Some(Int64));
        assert_eq!(decl_of(b'N', 2), Some(Float64));
        assert_eq!(decl_of(b'F', 0), Some(Float64), "F is floating whatever its decimals");
        assert_eq!(decl_of(b'M', 0), None, "a memo field declares nothing");
    }

    use super::*;
    use crate::schema::infer_columns;
    use crate::feature::Feature;

    fn features(props: &[&[(&str, JsonValue)]]) -> Vec<Feature> {
        props
            .iter()
            .map(|p| Feature {
                geometry: None,
                properties: p.iter().map(|(k, v)| (Rc::from(*k), v.clone())).collect(),
                native: None,
            })
            .collect()
    }

    /// `gdal_hostile.dbf` is `ogr2ogr`'s own output and declares exactly the
    /// widths this crate used to throw away: `String (80.0)` three times and
    /// `Real (24.15)`.
    const HOSTILE_DBF: &[u8] = include_bytes!("../../tests/fixtures/gdal_hostile.dbf");

    fn decl(name: &str) -> FieldDecl {
        read_schema(HOSTILE_DBF).unwrap().into_iter().find(|d| d.name == name).unwrap()
    }

    #[test]
    fn read_schema_recovers_declared_character_widths() {
        // The 80 is on disk; the values in it are 6 and 8 bytes long. Nothing
        // about the rows could produce this number.
        assert_eq!(decl("name").width, Some(80));
        assert_eq!(decl("zip").width, Some(80));
        assert_eq!(decl("note").width, Some(80));
    }

    #[test]
    fn read_schema_maps_a_numeric_field_to_precision_and_scale() {
        // `.dbf` says len 24, decimals 15 — GDAL calls that `Real (24.15)`.
        // In FlatGeobuf's vocabulary, which `FieldDecl` follows, the field
        // length is a *precision* and the decimals a *scale*; `width` stays
        // unset, exactly as GDAL writes it.
        let price = decl("price");
        assert_eq!((price.precision, price.scale), (Some(24), Some(15)));
        assert_eq!(price.width, None);
    }

    #[test]
    fn read_schema_declares_nothing_about_nulls_or_keys() {
        // dBase has no such concepts; a `.dbf` cannot state them, so the
        // reader must not invent them.
        for d in read_schema(HOSTILE_DBF).unwrap() {
            assert_eq!((d.nullable, d.unique, d.primary_key), (None, None, None), "{}", d.name);
        }
    }

    /// The defect this whole mechanism exists to fix: without a declaration an
    /// 80-character field comes back sized to today's longest value.
    #[test]
    fn a_declared_width_survives_the_writer_and_re_derivation_does_not() {
        let feats = features(&[&[("name", JsonValue::String("Zürich".into()))]]);
        let mut columns = infer_columns(&feats);

        let (narrowed, _) = write(&columns).unwrap();
        assert_eq!(read_schema(&narrowed).unwrap()[0].width, Some(7), "no declaration: sized to the data");

        columns[0].decl = Some(FieldDecl { name: "name".into(), width: Some(80), ..FieldDecl::default() });
        let (honoured, _) = write(&columns).unwrap();
        assert_eq!(read_schema(&honoured).unwrap()[0].width, Some(80));
        // And the values still read back intact at the wider size.
        assert_eq!(
            read(&honoured, Encoding::Utf8, true, &mut TextLosses::default()).unwrap()[0].properties[0].1,
            JsonValue::String("Zürich".into())
        );
    }

    #[test]
    fn a_declaration_narrower_than_the_data_is_widened_not_obeyed() {
        // The mid-pipeline case: a middle stage lengthened the value but left
        // the declaration alone. Obeying it would truncate what it wrote.
        let long = "x".repeat(40);
        let feats = features(&[&[("name", JsonValue::String(long.clone()))]]);
        let mut columns = infer_columns(&feats);
        columns[0].decl = Some(FieldDecl { name: "name".into(), width: Some(10), ..FieldDecl::default() });
        let (bytes, warnings) = write(&columns).unwrap();
        assert_eq!(read_schema(&bytes).unwrap()[0].width, Some(40));
        assert!(warnings.is_empty(), "widening is not a loss and must not warn: {warnings:?}");
        assert_eq!(read(&bytes, Encoding::Utf8, true, &mut TextLosses::default()).unwrap()[0].properties[0].1, JsonValue::String(long));
    }

    #[test]
    fn a_declared_scale_sets_the_numeric_field_s_decimals() {
        let feats = features(&[&[("price", JsonValue::Float(1.5))]]);
        let mut columns = infer_columns(&feats);
        columns[0].decl =
            Some(FieldDecl { name: "price".into(), precision: Some(24), scale: Some(15), ..FieldDecl::default() });
        let (bytes, _) = write(&columns).unwrap();
        let d = &read_schema(&bytes).unwrap()[0];
        assert_eq!((d.precision, d.scale), (Some(24), Some(15)));
        assert_eq!(
            read(&bytes, Encoding::Utf8, true, &mut TextLosses::default()).unwrap()[0].properties[0].1,
            JsonValue::Float(1.5)
        );
    }

    #[test]
    fn a_declaration_cannot_push_a_field_past_dbase_s_own_limit() {
        let feats = features(&[&[("name", JsonValue::String("x".into()))]]);
        let mut columns = infer_columns(&feats);
        columns[0].decl = Some(FieldDecl { name: "name".into(), width: Some(4000), ..FieldDecl::default() });
        let (bytes, _) = write(&columns).unwrap();
        assert_eq!(read_schema(&bytes).unwrap()[0].width, Some(255));
    }

    #[test]
    fn round_trips_all_field_types() {
        let feats = features(&[
            &[
                ("name", JsonValue::String("Alice".into())),
                ("age", JsonValue::Int(30)),
                ("score", JsonValue::Float(1.5)),
                ("active", JsonValue::Bool(true)),
            ],
            &[
                ("name", JsonValue::String("Bob".into())),
                ("age", JsonValue::Int(-5)),
                ("score", JsonValue::Float(-2.25)),
                ("active", JsonValue::Bool(false)),
            ],
        ]);
        let columns = infer_columns(&feats);
        let (bytes, warnings) = write(&columns).unwrap();
        assert!(warnings.is_empty());
        let records = read(&bytes, Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
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
            &[("a", JsonValue::Int(1))],
            &[], // "a" missing -> Null
        ]);
        let columns = infer_columns(&feats);
        let (bytes, _warnings) = write(&columns).unwrap();
        let records = read(&bytes, Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
        let get = |props: &[(Rc<str>, JsonValue)], k: &str| {
            props.iter().find(|(name, _)| &**name == k).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(get(&records[1].properties, "a"), JsonValue::Null);
    }

    #[test]
    fn deleted_flag_is_detected() {
        // Hand-build a minimal one-field, one-record dbf with the deletion flag set.
        let columns = infer_columns(&features(&[&[("a", JsonValue::String("x".into()))]]));
        let (mut bytes, _warnings) = write(&columns).unwrap();
        // Record starts right after the header + 1 field descriptor + terminator.
        let header_size = u16_at(&bytes, 8).unwrap() as usize;
        bytes[header_size] = b'*';
        let records = read(&bytes, Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
        assert!(records[0].deleted);
    }

    #[test]
    fn latin1_fallback_decodes_non_utf8_bytes() {
        // 0xE9 is 'é' in Latin-1 but not valid standalone UTF-8.
        let raw = [0xE9u8, b' ', b' '];
        let field = Field { name: "n".into(), ty: b'C', len: 3, decimals: 0 };
        let v = decode_field(&field, &raw, Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
        assert_eq!(v.as_str(), Some("é"));
    }

    #[test]
    fn date_field_renders_iso8601() {
        let field = Field { name: "d".into(), ty: b'D', len: 8, decimals: 0 };
        let v = decode_field(&field, b"20200101", Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
        assert_eq!(v.as_str(), Some("2020-01-01"));
    }

    #[test]
    fn long_string_is_truncated_at_char_boundary() {
        let feats = features(&[&[("s", JsonValue::String("a".repeat(300)))]]);
        let columns = infer_columns(&feats);
        let (bytes, warnings) = write(&columns).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains('s') && warnings[0].contains("300") && warnings[0].contains("255"), "{warnings:?}");
        let records = read(&bytes, Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
        assert_eq!(records[0].properties[0].1.as_str().unwrap().len(), 255);
    }

    #[test]
    fn long_field_name_is_truncated_and_warned() {
        let feats = features(&[&[("a_very_long_property_name", JsonValue::String("x".into()))]]);
        let columns = infer_columns(&feats);
        let (bytes, warnings) = write(&columns).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("a_very_long_property_name"), "{warnings:?}");
        assert!(warnings[0].contains("a_very_lon"), "{warnings:?}");
        let records = read(&bytes, Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
        assert_eq!(records[0].properties[0].0.as_ref(), "a_very_lon");
    }

    #[test]
    fn two_names_truncating_to_the_same_field_name_errors() {
        let feats = features(&[&[
            ("a_very_long_property_name", JsonValue::String("x".into())),
            ("a_very_long_property_other", JsonValue::String("y".into())),
        ]]);
        let columns = infer_columns(&feats);
        let msg = match write(&columns) {
            Ok(_) => panic!("two names truncating to the same 10-byte field name should error"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("a_very_long_property_name") && msg.contains("a_very_long_property_other"), "{msg}");
    }

    #[test]
    fn an_oversized_double_errors_instead_of_corrupting_the_file() {
        // 1e300 formatted at 6 fixed decimal places needs ~308 bytes, past
        // even dBase's true 255-byte field-width ceiling — genuinely
        // unrepresentable, not just past the old (buggy) 32-byte cap this
        // fix raised. Old behavior: `{:>width$}` pads a too-narrow width
        // but doesn't truncate, so this used to silently emit more bytes
        // than the field's declared width, desyncing every field/row after
        // it. Now a clear error instead.
        let feats = features(&[&[("v", JsonValue::Float(1e300))]]);
        let columns = infer_columns(&feats);
        let msg = match write(&columns) {
            Ok(_) => panic!("an oversized numeric value should error rather than corrupt the file"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains('v'), "{msg}");
    }

    #[test]
    fn a_large_but_realistic_double_writes_without_truncation() {
        // Not pathological — e.g. a large projected-CRS coordinate or an
        // epoch-nanoseconds timestamp — should round-trip exactly, not just
        // avoid erroring. Confirms the raised cap (32 -> 255) actually fixed
        // the realistic case, not just widened the error threshold.
        let feats = features(&[&[("v", JsonValue::Float(123_456_789_012.5))]]);
        let columns = infer_columns(&feats);
        let (bytes, warnings) = write(&columns).unwrap();
        assert!(warnings.is_empty());
        let records = read(&bytes, Encoding::Utf8, false, &mut TextLosses::default()).unwrap();
        assert_eq!(records[0].properties[0].1.as_f64(), Some(123_456_789_012.5));
    }
}
