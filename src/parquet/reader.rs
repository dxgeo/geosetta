//! Read back the GeoParquet that [`super::writer`] produces.
//!
//! This is the *round-trip* reader: it assumes the narrow shape Pantograph
//! writes — one row group, one PLAIN `DATA_PAGE` per column, SNAPPY (or
//! UNCOMPRESSED) codec, every column OPTIONAL with RLE definition levels, 2D
//! WKB geometry. It validates those assumptions and errors on anything else
//! rather than misreading. Reading foreign GeoParquet (dictionary encoding,
//! multiple pages/row groups, other codecs) is a separate effort — see
//! `plans/arbitrary-geoparquet.org`.

use super::geo::GEOMETRY_COLUMN;
use super::snappy;
use super::thrift::{CompactReader, Field};
use super::types::{codec, ptype};
use crate::error::{Error, Result};
use crate::json::{self, JsonValue};

const MAGIC: &[u8; 4] = b"PAR1";

/// One decoded property column, values aligned to rows (nulls as `Null`).
pub struct PropertyColumn {
    pub name: String,
    pub values: Vec<JsonValue>,
}

/// The decoded contents of a GeoParquet file.
pub struct GeoParquet {
    pub num_rows: usize,
    /// Non-geometry columns, in file order.
    pub properties: Vec<PropertyColumn>,
    /// One WKB blob per row (or `None` where the geometry was null).
    pub geometry: Vec<Option<Vec<u8>>>,
}

/// Parse a GeoParquet byte buffer.
pub fn read_geoparquet(bytes: &[u8]) -> Result<GeoParquet> {
    // "PAR1" ... <FileMetaData> <u32 footer len> "PAR1"
    if bytes.len() < 12 || &bytes[..4] != MAGIC || &bytes[bytes.len() - 4..] != MAGIC {
        return Err(Error::Parquet("not a parquet file (bad magic)".into()));
    }
    let len_pos = bytes.len() - 8;
    let footer_len = u32::from_le_bytes(bytes[len_pos..len_pos + 4].try_into().unwrap()) as usize;
    let footer_start = len_pos
        .checked_sub(footer_len)
        .filter(|&s| s >= 4)
        .ok_or_else(|| Error::Parquet("invalid footer length".into()))?;
    let footer = &bytes[footer_start..len_pos];

    let meta = parse_file_metadata(footer)?;
    let num_rows = meta.num_rows as usize;

    let mut properties = Vec::new();
    let mut geometry: Option<Vec<Option<Vec<u8>>>> = None;
    for col in &meta.columns {
        let data = decode_column(bytes, col, num_rows)?;
        if col.name == meta.geometry_column {
            geometry = Some(match data {
                ColumnData::Bytes(v) => v,
                _ => return Err(Error::Parquet("geometry column is not BYTE_ARRAY".into())),
            });
        } else {
            properties.push(PropertyColumn {
                name: col.name.clone(),
                values: column_to_json(data)?,
            });
        }
    }

    Ok(GeoParquet {
        num_rows,
        properties,
        geometry: geometry.unwrap_or_else(|| vec![None; num_rows]),
    })
}

// --- footer parsing --------------------------------------------------------

struct Meta {
    num_rows: i64,
    columns: Vec<ColumnMeta>,
    geometry_column: String,
}

struct ColumnMeta {
    name: String,
    physical: i32,
    codec: i32,
    data_page_offset: i64,
}

fn parse_file_metadata(footer: &[u8]) -> Result<Meta> {
    let mut r = CompactReader::new(footer);
    let mut num_rows = 0i64;
    let mut columns: Vec<ColumnMeta> = Vec::new();
    let mut geometry_column = GEOMETRY_COLUMN.to_string();

    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                3 => num_rows = r.read_i64()?, // num_rows
                4 => {
                    // row_groups: take the first, ignore any others.
                    let (elem, len) = r.read_list_header()?;
                    for i in 0..len {
                        let cols = parse_row_group(&mut r)?;
                        if i == 0 {
                            columns = cols;
                        }
                        let _ = elem;
                    }
                }
                5 => {
                    // key_value_metadata: pull primary_column from "geo".
                    let (_elem, len) = r.read_list_header()?;
                    for _ in 0..len {
                        let (k, v) = parse_key_value(&mut r)?;
                        if k == "geo"
                            && let Some(pc) = v.as_deref().and_then(primary_column)
                        {
                            geometry_column = pc;
                        }
                    }
                }
                // version (1), schema (2), created_by (6), etc.: not needed.
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();

    Ok(Meta {
        num_rows,
        columns,
        geometry_column,
    })
}

fn parse_row_group(r: &mut CompactReader) -> Result<Vec<ColumnMeta>> {
    let mut columns = Vec::new();
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                1 => {
                    let (_elem, len) = r.read_list_header()?;
                    for _ in 0..len {
                        columns.push(parse_column_chunk(r)?);
                    }
                }
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    Ok(columns)
}

fn parse_column_chunk(r: &mut CompactReader) -> Result<ColumnMeta> {
    let mut meta = None;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                3 => meta = Some(parse_column_meta(r)?), // meta_data (ColumnMetaData)
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    meta.ok_or_else(|| Error::Parquet("column chunk missing meta_data".into()))
}

fn parse_column_meta(r: &mut CompactReader) -> Result<ColumnMeta> {
    let mut physical = -1i32;
    let mut codec = -1i32;
    let mut data_page_offset = -1i64;
    let mut path: Vec<String> = Vec::new();

    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                1 => physical = r.read_i32()?,
                3 => {
                    // path_in_schema: list<string>; the leaf name is the last.
                    let (_elem, len) = r.read_list_header()?;
                    for _ in 0..len {
                        path.push(r.read_string()?);
                    }
                }
                4 => codec = r.read_i32()?,
                9 => data_page_offset = r.read_i64()?,
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();

    if data_page_offset < 0 {
        return Err(Error::Parquet("column missing data_page_offset".into()));
    }
    Ok(ColumnMeta {
        name: path.pop().unwrap_or_default(),
        physical,
        codec,
        data_page_offset,
    })
}

fn parse_key_value(r: &mut CompactReader) -> Result<(String, Option<String>)> {
    let mut key = String::new();
    let mut value = None;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                1 => key = r.read_string()?,
                2 => value = Some(r.read_string()?),
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    Ok((key, value))
}

/// Extract `primary_column` from the GeoParquet `geo` metadata JSON.
fn primary_column(geo: &str) -> Option<String> {
    json::parse(geo)
        .ok()?
        .get("primary_column")
        .and_then(JsonValue::as_str)
        .map(String::from)
}

// --- page + value decoding -------------------------------------------------

/// Present-or-null values, aligned to rows, one variant per physical type.
enum ColumnData {
    Bool(Vec<Option<bool>>),
    Int(Vec<Option<i64>>),
    Double(Vec<Option<f64>>),
    Bytes(Vec<Option<Vec<u8>>>),
}

struct PageHeader {
    compressed_size: usize,
    uncompressed_size: usize,
}

fn decode_column(file: &[u8], col: &ColumnMeta, num_rows: usize) -> Result<ColumnData> {
    let off = col.data_page_offset as usize;
    let after_offset = file
        .get(off..)
        .ok_or_else(|| Error::Parquet("data_page_offset out of range".into()))?;

    let mut r = CompactReader::new(after_offset);
    let ph = parse_page_header(&mut r)?;
    let body_start = off + r.position();
    let body = file
        .get(body_start..body_start + ph.compressed_size)
        .ok_or_else(|| Error::Parquet("page body out of range".into()))?;

    let body = match col.codec {
        c if c == codec::SNAPPY => {
            snappy::decompress(body).ok_or_else(|| Error::Parquet("snappy decode failed".into()))?
        }
        c if c == codec::UNCOMPRESSED => body.to_vec(),
        other => return Err(Error::Parquet(format!("unsupported codec {other}"))),
    };
    if body.len() != ph.uncompressed_size {
        return Err(Error::Parquet("page size mismatch after decompression".into()));
    }

    // Body = [u32 len][RLE definition levels (bit width 1)][PLAIN values].
    let rle_len = body
        .get(..4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
        .ok_or_else(|| Error::Parquet("page body too short".into()))?;
    let levels = body
        .get(4..4 + rle_len)
        .ok_or_else(|| Error::Parquet("definition-level section out of range".into()))?;
    let values = &body[4 + rle_len..];

    let present = decode_levels(levels, 1, num_rows)?;
    let n_present = present.iter().filter(|&&d| d == 1).count();

    let data = match col.physical {
        p if p == ptype::BOOLEAN => ColumnData::Bool(align(&present, plain_bools(values, n_present)?)),
        p if p == ptype::INT64 => ColumnData::Int(align(&present, plain_i64(values, n_present)?)),
        p if p == ptype::DOUBLE => ColumnData::Double(align(&present, plain_f64(values, n_present)?)),
        p if p == ptype::BYTE_ARRAY => {
            ColumnData::Bytes(align(&present, plain_byte_arrays(values, n_present)?))
        }
        other => return Err(Error::Parquet(format!("unsupported physical type {other}"))),
    };
    Ok(data)
}

fn parse_page_header(r: &mut CompactReader) -> Result<PageHeader> {
    let mut compressed = -1i32;
    let mut uncompressed = -1i32;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                2 => uncompressed = r.read_i32()?,
                3 => compressed = r.read_i32()?,
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    if compressed < 0 || uncompressed < 0 {
        return Err(Error::Parquet("page header missing sizes".into()));
    }
    Ok(PageHeader {
        compressed_size: compressed as usize,
        uncompressed_size: uncompressed as usize,
    })
}

/// Distribute `present` values across rows: a `1` level takes the next value,
/// a `0` level is null.
fn align<T>(levels: &[u64], values: Vec<T>) -> Vec<Option<T>> {
    let mut it = values.into_iter();
    levels
        .iter()
        .map(|&d| if d == 1 { it.next() } else { None })
        .collect()
}

/// Decode an RLE/bit-pack hybrid level stream, yielding `count` values. Our
/// writer only emits RLE runs at bit width 1, but the bit-packed form is
/// handled too so the decoder is correct for any conforming stream.
fn decode_levels(data: &[u8], bit_width: u32, count: usize) -> Result<Vec<u64>> {
    if bit_width == 0 {
        return Ok(vec![0; count]);
    }
    let byte_width = bit_width.div_ceil(8) as usize;
    let mut out = Vec::with_capacity(count);
    let mut pos = 0usize;

    while out.len() < count {
        let before = out.len();
        let (header, adv) = read_uvarint(data, pos)?;
        pos += adv;

        if header & 1 == 0 {
            // RLE run: `run_len` copies of one value.
            let run_len = (header >> 1) as usize;
            let val_bytes = data
                .get(pos..pos + byte_width)
                .ok_or_else(|| Error::Parquet("truncated RLE value".into()))?;
            pos += byte_width;
            let mut val = 0u64;
            for (k, &b) in val_bytes.iter().enumerate() {
                val |= (b as u64) << (8 * k);
            }
            for _ in 0..run_len {
                if out.len() == count {
                    break;
                }
                out.push(val);
            }
        } else {
            // Bit-packed run: `groups` groups of 8 values, LSB first.
            let groups = (header >> 1) as usize;
            let total_bytes = groups * bit_width as usize;
            let packed = data
                .get(pos..pos + total_bytes)
                .ok_or_else(|| Error::Parquet("truncated bit-packed run".into()))?;
            pos += total_bytes;
            let mut bit = 0usize;
            for _ in 0..groups * 8 {
                if out.len() == count {
                    break;
                }
                let mut v = 0u64;
                for b in 0..bit_width as usize {
                    let byte = packed[bit >> 3];
                    v |= (((byte >> (bit & 7)) & 1) as u64) << b;
                    bit += 1;
                }
                out.push(v);
            }
        }

        if out.len() == before {
            return Err(Error::Parquet("zero-length level run".into()));
        }
    }
    Ok(out)
}

/// Read an unsigned LEB128 varint from `data` at `pos`, returning the value and
/// the number of bytes consumed.
fn read_uvarint(data: &[u8], pos: usize) -> Result<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    let mut i = pos;
    loop {
        let b = *data
            .get(i)
            .ok_or_else(|| Error::Parquet("truncated varint".into()))?;
        result |= ((b & 0x7f) as u64) << shift;
        i += 1;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Parquet("varint too long".into()));
        }
    }
    Ok((result, i - pos))
}

// --- PLAIN value decoders --------------------------------------------------

fn plain_bools(data: &[u8], count: usize) -> Result<Vec<bool>> {
    if data.len() < count.div_ceil(8) {
        return Err(Error::Parquet("truncated boolean values".into()));
    }
    Ok((0..count).map(|i| (data[i / 8] >> (i % 8)) & 1 == 1).collect())
}

fn plain_i64(data: &[u8], count: usize) -> Result<Vec<i64>> {
    let slots = fixed_slots(data, count, 8)?;
    Ok(slots.map(|s| i64::from_le_bytes(s.try_into().unwrap())).collect())
}

fn plain_f64(data: &[u8], count: usize) -> Result<Vec<f64>> {
    let slots = fixed_slots(data, count, 8)?;
    Ok(slots.map(|s| f64::from_le_bytes(s.try_into().unwrap())).collect())
}

/// Iterator over `count` fixed-width slices, checking the buffer is long enough.
fn fixed_slots(data: &[u8], count: usize, width: usize) -> Result<impl Iterator<Item = &[u8]>> {
    if data.len() < count * width {
        return Err(Error::Parquet("truncated fixed-width values".into()));
    }
    Ok((0..count).map(move |i| &data[i * width..i * width + width]))
}

fn plain_byte_arrays(data: &[u8], count: usize) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(count);
    let mut pos = 0usize;
    for _ in 0..count {
        let len = data
            .get(pos..pos + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .ok_or_else(|| Error::Parquet("truncated byte-array length".into()))?;
        pos += 4;
        let bytes = data
            .get(pos..pos + len)
            .ok_or_else(|| Error::Parquet("truncated byte-array value".into()))?;
        out.push(bytes.to_vec());
        pos += len;
    }
    Ok(out)
}

/// Turn a decoded column into per-row JSON property values.
fn column_to_json(data: ColumnData) -> Result<Vec<JsonValue>> {
    let values = match data {
        ColumnData::Bool(v) => v
            .into_iter()
            .map(|o| o.map_or(JsonValue::Null, JsonValue::Bool))
            .collect(),
        ColumnData::Int(v) => v
            .into_iter()
            .map(|o| {
                o.map_or(JsonValue::Null, |i| JsonValue::Number {
                    value: i as f64,
                    is_int: true,
                })
            })
            .collect(),
        ColumnData::Double(v) => v
            .into_iter()
            .map(|o| {
                o.map_or(JsonValue::Null, |d| JsonValue::Number {
                    value: d,
                    is_int: false,
                })
            })
            .collect(),
        ColumnData::Bytes(v) => {
            let mut out = Vec::with_capacity(v.len());
            for cell in v {
                out.push(match cell {
                    None => JsonValue::Null,
                    Some(b) => JsonValue::String(
                        String::from_utf8(b)
                            .map_err(|_| Error::Parquet("string column has invalid utf-8".into()))?,
                    ),
                });
            }
            out
        }
    };
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_levels_single_rle_run() {
        // Mirror of the writer: five present rows -> [0x0a, 0x01].
        let levels = decode_levels(&[0x0a, 0x01], 1, 5).unwrap();
        assert_eq!(levels, vec![1, 1, 1, 1, 1]);
    }

    #[test]
    fn decode_levels_alternating() {
        // 1,0,1 as three RLE runs of length 1.
        let levels = decode_levels(&[0x02, 0x01, 0x02, 0x00, 0x02, 0x01], 1, 3).unwrap();
        assert_eq!(levels, vec![1, 0, 1]);
    }

    #[test]
    fn plain_byte_arrays_reads_lengths() {
        let mut data = Vec::new();
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(b"hi");
        data.extend_from_slice(&0u32.to_le_bytes());
        let out = plain_byte_arrays(&data, 2).unwrap();
        assert_eq!(out, vec![b"hi".to_vec(), Vec::new()]);
    }

    #[test]
    fn align_places_nulls() {
        let out = align(&[1, 0, 1], vec![10i64, 20]);
        assert_eq!(out, vec![Some(10), None, Some(20)]);
    }
}
