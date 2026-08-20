//! Assemble a complete GeoParquet file in memory.
//!
//! Layout produced:
//! ```text
//! "PAR1"                       magic
//! <data page> * N              one Snappy-compressed DATA_PAGE per column
//! <FileMetaData>               Thrift compact protocol
//! <u32 le>                     length of FileMetaData
//! "PAR1"                       magic
//! ```
//! Every column is OPTIONAL (max definition level 1, no repetition levels),
//! so each page body is `[u32 len][RLE definition levels][PLAIN values]`.

use super::geo::GEOMETRY_COLUMN;
use crate::schema::{Cell, Column, ColumnType, DeclType};
use crate::compress::snappy;
use super::thrift::{ct, CompactWriter};
use super::types::{codec, converted, encoding, page, ptype, repetition};

const MAGIC: &[u8; 4] = b"PAR1";
/// Format version recorded in `FileMetaData` (data pages are v1).
const FORMAT_VERSION: i32 = 1;

/// A column reduced to its physical type plus per-row definition levels and
/// PLAIN-encoded present values.
struct Prepared {
    name: String,
    ptype: i32,
    /// The `converted_type` annotation this column carries, if any.
    converted: Option<i32>,
    num_values: usize,
    /// One byte per row: 1 = present, 0 = null.
    def_levels: Vec<u8>,
    /// PLAIN-encoded values, present rows only.
    values: Vec<u8>,
}

/// Where a written column chunk landed, for the footer. Sizes include the page
/// header (which is never compressed) and the page body.
struct ChunkInfo {
    name: String,
    ptype: i32,
    converted: Option<i32>,
    data_page_offset: i64,
    compressed_size: i64,
    uncompressed_size: i64,
    num_values: i64,
}

/// Build a GeoParquet file from property `columns`, a per-row `geometry` WKB
/// column, and the pre-rendered `geo_metadata` JSON.
pub fn write_geoparquet(
    columns: &[Column],
    geometry: &[Option<Vec<u8>>],
    geo_metadata: &str,
) -> Vec<u8> {
    let num_rows = geometry.len() as i64;

    // Property columns first, geometry last (GeoParquet's primary column).
    let mut prepared: Vec<Prepared> = columns.iter().map(prepare_property).collect();
    prepared.push(prepare_geometry(geometry));

    let mut file: Vec<u8> = Vec::new();
    file.extend_from_slice(MAGIC);

    let mut infos: Vec<ChunkInfo> = Vec::with_capacity(prepared.len());
    for pc in &prepared {
        infos.push(write_column_page(&mut file, pc));
    }

    let total_byte_size: i64 = infos.iter().map(|c| c.uncompressed_size).sum();
    let footer = build_file_metadata(&infos, num_rows, total_byte_size, geo_metadata);

    file.extend_from_slice(&footer);
    file.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    file.extend_from_slice(MAGIC);
    file
}

// --- column preparation ---------------------------------------------------

/// How one column's cells are laid down: its Parquet physical type, its
/// `converted_type` annotation, and which encoding to use.
///
/// Parquet's type system is wider than the hub's four, so a column that
/// arrived carrying a declaration is written as *that* type rather than as
/// whatever its values re-infer to — an `Int16` stays 16-bit and a `DateTime`
/// stays a timestamp instead of becoming text (defect 5).
#[derive(Clone, Copy, PartialEq)]
enum Plan {
    Bool,
    /// Physical `INT32`, from a `Cell::Int`.
    Int32(Option<i32>),
    /// Physical `INT64`, from a `Cell::Int`.
    Int64(Option<i32>),
    Float,
    Double,
    /// Physical `BYTE_ARRAY`, from a `Cell::Str`.
    Text(Option<i32>),
    /// `INT32` days since the epoch, parsed from an ISO date string.
    DateDays,
    /// `INT64` milliseconds since the epoch, parsed from an ISO date-time.
    TimestampMillis,
}

impl Plan {
    fn ptype(self) -> i32 {
        match self {
            Plan::Bool => ptype::BOOLEAN,
            Plan::Int32(_) | Plan::DateDays => ptype::INT32,
            Plan::Int64(_) | Plan::TimestampMillis => ptype::INT64,
            Plan::Float => ptype::FLOAT,
            Plan::Double => ptype::DOUBLE,
            Plan::Text(_) => ptype::BYTE_ARRAY,
        }
    }

    fn converted(self) -> Option<i32> {
        match self {
            Plan::Int32(c) | Plan::Int64(c) | Plan::Text(c) => c,
            Plan::DateDays => Some(converted::DATE),
            Plan::TimestampMillis => Some(converted::TIMESTAMP_MILLIS),
            _ => None,
        }
    }
}

/// Choose a column's on-disk shape: the declaration if it still fits the data,
/// else the hub type the values inferred.
///
/// The two temporal plans are conditional on *every* non-null cell parsing.
/// The IR holds a date as text, so a declaration this writer cannot honour
/// exactly would otherwise turn a value into a wrong number — declining to
/// text is the same "data wins where it contradicts" rule `type_for` applies.
fn plan_for(col: &Column) -> Plan {
    let text_default = || match col.ty {
        ColumnType::Bool => Plan::Bool,
        ColumnType::Int64 => Plan::Int64(None),
        ColumnType::Double => Plan::Double,
        ColumnType::String => Plan::Text(Some(converted::UTF8)),
    };
    let Some(declared) = col.declared_type() else {
        return text_default();
    };
    let strs = || col.values.iter().filter_map(|c| match c {
        Cell::Str(s) => Some(s.as_str()),
        _ => None,
    });
    match declared {
        DeclType::Bool => Plan::Bool,
        DeclType::Int8 => Plan::Int32(Some(converted::INT_8)),
        DeclType::UInt8 => Plan::Int32(Some(converted::UINT_8)),
        DeclType::Int16 => Plan::Int32(Some(converted::INT_16)),
        DeclType::UInt16 => Plan::Int32(Some(converted::UINT_16)),
        DeclType::Int32 => Plan::Int32(None),
        DeclType::UInt32 => Plan::Int32(Some(converted::UINT_32)),
        DeclType::Int64 => Plan::Int64(None),
        DeclType::UInt64 => Plan::Int64(Some(converted::UINT_64)),
        DeclType::Float32 => Plan::Float,
        DeclType::Float64 => Plan::Double,
        DeclType::Json => Plan::Text(Some(converted::JSON)),
        DeclType::Date if strs().all(|s| parse_date_days(s).is_some()) => Plan::DateDays,
        DeclType::DateTime if strs().all(|s| parse_timestamp_millis(s).is_some()) => {
            Plan::TimestampMillis
        }
        // `String`, a temporal type whose text this writer cannot parse back,
        // and `Binary` (whose cells are hex text, not bytes — see the reader's
        // `decl_type`) all land on plain UTF8.
        _ => Plan::Text(Some(converted::UTF8)),
    }
}

fn prepare_property(col: &Column) -> Prepared {
    let plan = plan_for(col);

    let mut def = Vec::with_capacity(col.values.len());
    let mut values = Vec::new();
    // Booleans are bit-packed together, so collect them separately.
    let mut bits: Vec<bool> = Vec::new();

    for cell in &col.values {
        let mut present = true;
        match (plan, cell) {
            (_, Cell::Null) => present = false,
            (Plan::Bool, Cell::Bool(b)) => bits.push(*b),
            (Plan::Int32(_), Cell::Int(n)) => {
                values.extend_from_slice(&(*n as i32).to_le_bytes())
            }
            (Plan::Int64(_), Cell::Int(n)) => values.extend_from_slice(&n.to_le_bytes()),
            (Plan::Float, Cell::Double(d)) => {
                values.extend_from_slice(&(*d as f32).to_le_bytes())
            }
            (Plan::Double, Cell::Double(d)) => values.extend_from_slice(&d.to_le_bytes()),
            (Plan::Text(_), Cell::Str(s)) => plain_byte_array(&mut values, s.as_bytes()),
            // `plan_for` only chooses these when every cell parses.
            (Plan::DateDays, Cell::Str(s)) => {
                values.extend_from_slice(&(parse_date_days(s).unwrap_or(0) as i32).to_le_bytes())
            }
            (Plan::TimestampMillis, Cell::Str(s)) => {
                values.extend_from_slice(&parse_timestamp_millis(s).unwrap_or(0).to_le_bytes())
            }
            // Type/cell mismatches cannot arise from `infer_columns`, but be
            // safe and treat them as null.
            _ => present = false,
        }
        def.push(present as u8);
    }

    if plan == Plan::Bool {
        values = pack_bits(&bits);
    }

    Prepared {
        name: col.name.clone(),
        ptype: plan.ptype(),
        converted: plan.converted(),
        num_values: col.values.len(),
        def_levels: def,
        values,
    }
}

/// Days since 1970-01-01 for `YYYY-MM-DD`, the inverse of the reader's
/// `format_date`. Howard Hinnant's `days_from_civil`, the same proleptic
/// Gregorian algorithm `civil_from_days` inverts.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// `YYYY-MM-DD` → days since the epoch, or `None` if it is not that shape.
fn parse_date_days(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m as u32, d as u32))
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]` → milliseconds since the epoch, the inverse of
/// the reader's `format_timestamp` at millisecond precision.
///
/// Sub-millisecond digits are *rejected* rather than truncated: a microsecond
/// source that round-tripped through this would silently lose precision, and
/// declining leaves it as exact text instead.
fn parse_timestamp_millis(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once(['T', ' '])?;
    let days = parse_date_days(date)?;
    let (time, frac) = match rest.split_once('.') {
        Some((t, f)) => (t, Some(f)),
        None => (rest, None),
    };
    let parts: Vec<&str> = time.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let n = |p: &str| p.parse::<i64>().ok();
    let (hh, mm, ss) = (n(parts[0])?, n(parts[1])?, n(parts[2])?);
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let millis = match frac {
        None => 0,
        Some(f) if f.len() <= 3 && f.bytes().all(|c| c.is_ascii_digit()) => {
            n(f)? * 10i64.pow(3 - f.len() as u32)
        }
        Some(_) => return None,
    };
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000 + millis)
}

fn prepare_geometry(geometry: &[Option<Vec<u8>>]) -> Prepared {
    let mut def = Vec::with_capacity(geometry.len());
    let mut values = Vec::new();
    for g in geometry {
        match g {
            Some(wkb) => {
                def.push(1);
                plain_byte_array(&mut values, wkb);
            }
            None => def.push(0),
        }
    }
    Prepared {
        name: GEOMETRY_COLUMN.to_string(),
        ptype: ptype::BYTE_ARRAY,
        converted: None,
        num_values: geometry.len(),
        def_levels: def,
        values,
    }
}

/// PLAIN BYTE_ARRAY value: 4-byte little-endian length, then the bytes.
fn plain_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// PLAIN BOOLEAN: one bit per value, LSB first, zero-padded to whole bytes.
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        if *b {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

// --- page + footer serialization ------------------------------------------

/// Append one Snappy-compressed DATA_PAGE (header + body) for `pc` and report
/// its placement.
fn write_column_page(file: &mut Vec<u8>, pc: &Prepared) -> ChunkInfo {
    let data_page_offset = file.len() as i64;

    // Body = [u32 len][RLE definition levels (bit width 1)][PLAIN values].
    let mut body = Vec::new();
    let rle = encode_rle_levels(&pc.def_levels, 1);
    body.extend_from_slice(&(rle.len() as u32).to_le_bytes());
    body.extend_from_slice(&rle);
    body.extend_from_slice(&pc.values);

    // The codec applies to the page body only; the header stays uncompressed.
    let compressed = snappy::compress(&body);
    let uncompressed_page_size = body.len() as i32;
    let compressed_page_size = compressed.len() as i32;

    // PageHeader (compact protocol).
    let mut h = CompactWriter::new();
    h.struct_begin();
    h.field_i32(1, page::DATA_PAGE); // type
    h.field_i32(2, uncompressed_page_size); // uncompressed_page_size
    h.field_i32(3, compressed_page_size); // compressed_page_size
    h.field_struct(5); // data_page_header
    h.field_i32(1, pc.num_values as i32);
    h.field_i32(2, encoding::PLAIN); // value encoding
    h.field_i32(3, encoding::RLE); // definition level encoding
    h.field_i32(4, encoding::RLE); // repetition level encoding
    h.struct_end(); // data_page_header
    h.struct_end(); // PageHeader
    let header = h.into_bytes();

    file.extend_from_slice(&header);
    file.extend_from_slice(&compressed);

    ChunkInfo {
        name: pc.name.clone(),
        ptype: pc.ptype,
        converted: pc.converted,
        data_page_offset,
        compressed_size: (header.len() + compressed.len()) as i64,
        uncompressed_size: (header.len() + body.len()) as i64,
        num_values: pc.num_values as i64,
    }
}

fn build_file_metadata(
    infos: &[ChunkInfo],
    num_rows: i64,
    total_byte_size: i64,
    geo_metadata: &str,
) -> Vec<u8> {
    let prepared = &infos; // alias for readability
    let mut w = CompactWriter::new();
    w.struct_begin();

    w.field_i32(1, FORMAT_VERSION);

    // schema: root element followed by one leaf per column.
    w.field_list(2, ct::STRUCT, 1 + prepared.len());
    // Root.
    w.struct_begin();
    w.field_string(4, "schema");
    w.field_i32(5, prepared.len() as i32); // num_children
    w.struct_end();
    // Leaves.
    for c in *prepared {
        let converted = converted_type_for(c);
        w.struct_begin();
        w.field_i32(1, c.ptype); // type
        w.field_i32(3, repetition::OPTIONAL); // repetition_type
        w.field_string(4, &c.name); // name
        if let Some(conv) = converted {
            w.field_i32(6, conv); // converted_type (UTF8 for strings)
        }
        w.struct_end();
    }

    w.field_i64(3, num_rows);

    // row_groups: exactly one.
    w.field_list(4, ct::STRUCT, 1);
    w.struct_begin();
    // columns
    w.field_list(1, ct::STRUCT, prepared.len());
    for c in *prepared {
        w.struct_begin();
        w.field_i64(2, c.data_page_offset); // file_offset
        w.field_struct(3); // meta_data (ColumnMetaData)
        w.field_i32(1, c.ptype); // type
        w.field_list(2, ct::I32, 2); // encodings
        w.raw_i32(encoding::PLAIN);
        w.raw_i32(encoding::RLE);
        w.field_list(3, ct::BINARY, 1); // path_in_schema
        w.raw_string(&c.name);
        w.field_i32(4, codec::SNAPPY); // codec
        w.field_i64(5, c.num_values); // num_values
        w.field_i64(6, c.uncompressed_size); // total_uncompressed_size
        w.field_i64(7, c.compressed_size); // total_compressed_size
        w.field_i64(9, c.data_page_offset); // data_page_offset
        w.struct_end(); // ColumnMetaData
        w.struct_end(); // ColumnChunk
    }
    w.field_i64(2, total_byte_size);
    w.field_i64(3, num_rows);
    w.struct_end(); // RowGroup

    // key_value_metadata: the GeoParquet "geo" entry.
    w.field_list(5, ct::STRUCT, 1);
    w.struct_begin();
    w.field_string(1, "geo");
    w.field_string(2, geo_metadata);
    w.struct_end();

    w.field_string(6, "geosetta");
    w.struct_end(); // FileMetaData
    w.into_bytes()
}

/// The annotation decided when the column was prepared. Geometry is raw WKB
/// and carries none; everything else was settled by [`plan_for`].
fn converted_type_for(c: &ChunkInfo) -> Option<i32> {
    match c.name == GEOMETRY_COLUMN {
        true => None,
        false => c.converted,
    }
}

// --- RLE definition levels ------------------------------------------------

/// Encode definition `levels` with the RLE/bit-pack hybrid, using only RLE
/// runs (each maximal run of equal levels becomes one run). `bit_width` is 1
/// for our optional-only schema.
fn encode_rle_levels(levels: &[u8], bit_width: u32) -> Vec<u8> {
    let value_bytes = (bit_width as usize).div_ceil(8);
    let mut out = Vec::new();
    let mut i = 0;
    while i < levels.len() {
        let v = levels[i];
        let mut run = 1usize;
        while i + run < levels.len() && levels[i + run] == v {
            run += 1;
        }
        // RLE run header: (run_length << 1) as unsigned varint (low bit 0).
        write_uvarint(&mut out, (run as u64) << 1);
        let vv = v as u64;
        for b in 0..value_bytes {
            out.push(((vv >> (8 * b)) & 0xff) as u8);
        }
        i += run;
    }
    out
}

/// Unsigned LEB128 varint.
fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_levels_single_run() {
        // Five present rows -> one RLE run of value 1.
        let out = encode_rle_levels(&[1, 1, 1, 1, 1], 1);
        // header varint(5<<1=10)=0x0a, value byte 0x01
        assert_eq!(out, vec![0x0a, 0x01]);
    }

    #[test]
    fn rle_levels_alternating() {
        // 1,0,1 -> three runs of length 1.
        let out = encode_rle_levels(&[1, 0, 1], 1);
        // each: varint(1<<1=2)=0x02, value byte
        assert_eq!(out, vec![0x02, 0x01, 0x02, 0x00, 0x02, 0x01]);
    }

    #[test]
    fn pack_bits_lsb_first() {
        assert_eq!(pack_bits(&[true, false, true]), vec![0b0000_0101]);
        assert_eq!(pack_bits(&[false; 9]), vec![0, 0]);
    }

    #[test]
    fn file_has_magic_bookends() {
        let cols: Vec<Column> = Vec::new();
        let geom = vec![Some(vec![0x01, 0x02])];
        let out = write_geoparquet(&cols, &geom, "{}");
        assert_eq!(&out[..4], MAGIC);
        assert_eq!(&out[out.len() - 4..], MAGIC);
        // 4-byte length precedes the trailing magic.
        let len_pos = out.len() - 8;
        let footer_len =
            u32::from_le_bytes(out[len_pos..len_pos + 4].try_into().unwrap()) as usize;
        assert!(footer_len > 0 && footer_len < out.len());
    }
}
