//! Read GeoParquet back into features.
//!
//! Handles the shape Pantograph writes and the common shape other tools
//! (DuckDB, Arrow, GDAL) emit: multiple row groups, one dictionary page plus
//! one or more data pages per column chunk, PLAIN and dictionary
//! (`PLAIN_DICTIONARY` / `RLE_DICTIONARY`) value encodings, RLE/bit-pack
//! definition levels, SNAPPY or no compression, flat (non-nested) 2D WKB
//! geometry. Anything outside that — other codecs (ZSTD/GZIP), `DATA_PAGE_V2`,
//! nested/repeated columns — is reported as a specific error rather than
//! misread. See `plans/arbitrary-geoparquet.org` for what remains.

use super::geo::GEOMETRY_COLUMN;
use super::thrift::{CompactReader, Field};
use super::types::{codec, encoding, page, ptype, repetition};
use super::{snappy, zstd};
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

    // Accumulate each column across all row groups, in file order.
    let mut properties: Vec<PropertyColumn> = Vec::new();
    let mut geometry: Vec<Option<Vec<u8>>> = Vec::new();
    let mut has_geometry = false;

    for (rg_idx, rg) in meta.row_groups.iter().enumerate() {
        let rg_rows = rg.num_rows as usize;
        let mut prop_idx = 0;
        for col in &rg.columns {
            let data = decode_column(bytes, col, rg_rows)?;
            if col.name == meta.geometry_column {
                has_geometry = true;
                match data {
                    ColumnData::Bytes(v) => geometry.extend(v),
                    _ => return Err(Error::Parquet("geometry column is not BYTE_ARRAY".into())),
                }
            } else {
                let values = column_to_json(data)?;
                if rg_idx == 0 {
                    properties.push(PropertyColumn {
                        name: col.name.clone(),
                        values,
                    });
                } else {
                    properties
                        .get_mut(prop_idx)
                        .ok_or_else(|| Error::Parquet("row groups disagree on columns".into()))?
                        .values
                        .extend(values);
                }
                prop_idx += 1;
            }
        }
    }

    if !has_geometry {
        geometry = vec![None; num_rows];
    }
    Ok(GeoParquet {
        num_rows,
        properties,
        geometry,
    })
}

// --- footer parsing --------------------------------------------------------

struct Meta {
    num_rows: i64,
    row_groups: Vec<RowGroup>,
    geometry_column: String,
}

struct RowGroup {
    num_rows: i64,
    columns: Vec<ColumnMeta>,
}

struct ColumnMeta {
    name: String,
    physical: i32,
    codec: i32,
    data_page_offset: i64,
    dictionary_page_offset: Option<i64>,
    /// 0 for a REQUIRED leaf (no definition levels), 1 for an OPTIONAL one.
    max_def_level: u32,
}

fn parse_file_metadata(footer: &[u8]) -> Result<Meta> {
    let mut r = CompactReader::new(footer);
    let mut num_rows = 0i64;
    let mut row_groups: Vec<RowGroup> = Vec::new();
    let mut geometry_column = GEOMETRY_COLUMN.to_string();
    // Leaf name -> repetition_type, used to derive each column's def level.
    let mut reps: Vec<(String, i32)> = Vec::new();

    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                2 => {
                    // schema: list<SchemaElement>
                    let (_elem, len) = r.read_list_header()?;
                    for _ in 0..len {
                        reps.push(parse_schema_element(&mut r)?);
                    }
                }
                3 => num_rows = r.read_i64()?,
                4 => {
                    let (_elem, len) = r.read_list_header()?;
                    for _ in 0..len {
                        row_groups.push(parse_row_group(&mut r)?);
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
                _ => r.skip(ty)?, // version (1), created_by (6), etc.
            },
        }
    }
    r.struct_end();

    // Attach the max definition level to every column from the schema.
    for rg in &mut row_groups {
        for col in &mut rg.columns {
            let rep = reps
                .iter()
                .find(|(name, _)| *name == col.name)
                .map(|(_, r)| *r)
                .unwrap_or(repetition::OPTIONAL);
            col.max_def_level = if rep == repetition::REQUIRED { 0 } else { 1 };
        }
    }

    Ok(Meta {
        num_rows,
        row_groups,
        geometry_column,
    })
}

/// A schema element reduced to `(name, repetition_type)`. The root element and
/// any non-leaf carry a name too, but they never match a column path, so we
/// keep them all and look up leaves by name.
fn parse_schema_element(r: &mut CompactReader) -> Result<(String, i32)> {
    let mut name = String::new();
    let mut rep = repetition::REQUIRED;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                3 => rep = r.read_i32()?,
                4 => name = r.read_string()?,
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    Ok((name, rep))
}

fn parse_row_group(r: &mut CompactReader) -> Result<RowGroup> {
    let mut columns = Vec::new();
    let mut num_rows = 0i64;
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
                3 => num_rows = r.read_i64()?,
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    Ok(RowGroup { num_rows, columns })
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
    let mut dictionary_page_offset = None;
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
                11 => dictionary_page_offset = Some(r.read_i64()?),
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
        dictionary_page_offset,
        max_def_level: 1, // set from the schema in parse_file_metadata
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

/// A decoded dictionary page, indexed by the data pages that follow it.
enum Dict {
    Int(Vec<i64>),
    Double(Vec<f64>),
    Bytes(Vec<Vec<u8>>),
}

struct PageHeader {
    page_type: i32,
    compressed_size: usize,
    uncompressed_size: usize,
    /// Value count from the data- or dictionary-page sub-header.
    num_values: i32,
    /// Value encoding from the sub-header.
    encoding: i32,
}

/// Decode one column chunk (all its pages, across the whole chunk) into
/// `rg_rows` aligned values.
fn decode_column(file: &[u8], col: &ColumnMeta, rg_rows: usize) -> Result<ColumnData> {
    // A dictionary page, if present, precedes the data pages.
    let start = col
        .dictionary_page_offset
        .filter(|&o| o >= 0)
        .unwrap_or(col.data_page_offset) as usize;

    let mut out = empty_column(col.physical)?;
    let mut dict: Option<Dict> = None;
    let mut pos = start;
    let mut rows_done = 0usize;

    while rows_done < rg_rows {
        let after = file
            .get(pos..)
            .ok_or_else(|| Error::Parquet("page offset out of range".into()))?;
        let mut r = CompactReader::new(after);
        let ph = parse_page_header(&mut r)?;
        let body_start = pos + r.position();
        let comp = file
            .get(body_start..body_start + ph.compressed_size)
            .ok_or_else(|| Error::Parquet("page body out of range".into()))?;
        let body = decompress(col.codec, comp, ph.uncompressed_size)?;
        pos = body_start + ph.compressed_size;

        match ph.page_type {
            t if t == page::DICTIONARY_PAGE => {
                dict = Some(decode_dictionary(&body, col.physical, ph.num_values as usize)?);
            }
            t if t == page::DATA_PAGE => {
                let page_rows = ph.num_values as usize;
                decode_data_page(&body, col, ph.encoding, page_rows, dict.as_ref(), &mut out)?;
                rows_done += page_rows;
            }
            t if t == page::DATA_PAGE_V2 => {
                return Err(Error::Parquet("DATA_PAGE_V2 is not supported yet".into()));
            }
            other => return Err(Error::Parquet(format!("unsupported page type {other}"))),
        }
    }
    Ok(out)
}

/// Decompress a page body and check it against the header's expected size.
fn decompress(codec: i32, comp: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    let body = match codec {
        c if c == codec::SNAPPY => {
            snappy::decompress(comp).ok_or_else(|| Error::Parquet("snappy decode failed".into()))?
        }
        c if c == codec::UNCOMPRESSED => comp.to_vec(),
        c if c == codec::ZSTD => zstd::decompress(comp, uncompressed_size)?,
        other => {
            return Err(Error::Parquet(format!(
                "unsupported compression codec {}",
                codec_name(other)
            )));
        }
    };
    if body.len() != uncompressed_size {
        return Err(Error::Parquet("page size mismatch after decompression".into()));
    }
    Ok(body)
}

/// Name a compression codec by its `CompressionCodec` id, for error messages.
fn codec_name(id: i32) -> String {
    match id {
        2 => "GZIP".into(),
        3 => "LZO".into(),
        4 => "BROTLI".into(),
        5 => "LZ4".into(),
        6 => "ZSTD".into(),
        7 => "LZ4_RAW".into(),
        other => format!("#{other}"),
    }
}

fn empty_column(physical: i32) -> Result<ColumnData> {
    Ok(match physical {
        p if p == ptype::BOOLEAN => ColumnData::Bool(Vec::new()),
        p if p == ptype::INT64 => ColumnData::Int(Vec::new()),
        p if p == ptype::DOUBLE => ColumnData::Double(Vec::new()),
        p if p == ptype::BYTE_ARRAY => ColumnData::Bytes(Vec::new()),
        other => return Err(Error::Parquet(format!("unsupported physical type {other}"))),
    })
}

fn decode_dictionary(body: &[u8], physical: i32, count: usize) -> Result<Dict> {
    Ok(match physical {
        p if p == ptype::INT64 => Dict::Int(plain_i64(body, count)?),
        p if p == ptype::DOUBLE => Dict::Double(plain_f64(body, count)?),
        p if p == ptype::BYTE_ARRAY => Dict::Bytes(plain_byte_arrays(body, count)?),
        other => {
            return Err(Error::Parquet(format!(
                "dictionary encoding unsupported for physical type {other}"
            )));
        }
    })
}

/// Decode one DATA_PAGE body, appending its rows to `out`.
fn decode_data_page(
    body: &[u8],
    col: &ColumnMeta,
    page_encoding: i32,
    page_rows: usize,
    dict: Option<&Dict>,
    out: &mut ColumnData,
) -> Result<()> {
    let (present, values) = split_definition_levels(body, col.max_def_level, page_rows)?;
    let n_present = present.iter().filter(|&&d| d == 1).count();

    match page_encoding {
        e if e == encoding::PLAIN => decode_plain(out, values, &present, n_present),
        e if e == encoding::PLAIN_DICTIONARY || e == encoding::RLE_DICTIONARY => {
            let dict = dict.ok_or_else(|| {
                Error::Parquet("dictionary-encoded data page without a dictionary".into())
            })?;
            decode_dict_indices(out, dict, values, &present, n_present)
        }
        other => Err(Error::Parquet(format!(
            "unsupported page encoding {other}"
        ))),
    }
}

/// Split a data-page body into a per-row present/null mask and the value bytes.
/// A REQUIRED column (`max_def_level == 0`) carries no definition levels.
fn split_definition_levels(
    body: &[u8],
    max_def_level: u32,
    page_rows: usize,
) -> Result<(Vec<u64>, &[u8])> {
    if max_def_level == 0 {
        return Ok((vec![1; page_rows], body));
    }
    let bit_width = bits_needed(max_def_level);
    let rle_len = body
        .get(..4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
        .ok_or_else(|| Error::Parquet("page body too short".into()))?;
    let levels = body
        .get(4..4 + rle_len)
        .ok_or_else(|| Error::Parquet("definition-level section out of range".into()))?;
    let values = &body[4 + rle_len..];
    let present = decode_levels(levels, bit_width, page_rows)?
        .into_iter()
        .map(|d| (d == max_def_level as u64) as u64)
        .collect();
    Ok((present, values))
}

fn decode_plain(out: &mut ColumnData, values: &[u8], present: &[u64], n: usize) -> Result<()> {
    match out {
        ColumnData::Bool(v) => v.extend(align(present, plain_bools(values, n)?)),
        ColumnData::Int(v) => v.extend(align(present, plain_i64(values, n)?)),
        ColumnData::Double(v) => v.extend(align(present, plain_f64(values, n)?)),
        ColumnData::Bytes(v) => v.extend(align(present, plain_byte_arrays(values, n)?)),
    }
    Ok(())
}

/// Decode a dictionary-index data page: `[1 byte bit width][RLE/bit-pack
/// indices]`, mapped through `dict`.
fn decode_dict_indices(
    out: &mut ColumnData,
    dict: &Dict,
    values: &[u8],
    present: &[u64],
    n: usize,
) -> Result<()> {
    let indices = if n == 0 {
        Vec::new()
    } else {
        let bit_width = *values
            .first()
            .ok_or_else(|| Error::Parquet("dictionary page missing bit width".into()))?
            as u32;
        decode_levels(&values[1..], bit_width, n)?
    };

    match (out, dict) {
        (ColumnData::Int(v), Dict::Int(d)) => {
            v.extend(align(present, map_indices(&indices, d, |x| *x)?))
        }
        (ColumnData::Double(v), Dict::Double(d)) => {
            v.extend(align(present, map_indices(&indices, d, |x| *x)?))
        }
        (ColumnData::Bytes(v), Dict::Bytes(d)) => {
            v.extend(align(present, map_indices(&indices, d, |x| x.clone())?))
        }
        _ => return Err(Error::Parquet("dictionary type mismatch".into())),
    }
    Ok(())
}

/// Map dictionary indices to values via `f`, erroring on any out-of-range index.
fn map_indices<T, U>(indices: &[u64], dict: &[T], f: impl Fn(&T) -> U) -> Result<Vec<U>> {
    indices
        .iter()
        .map(|&i| {
            dict.get(i as usize)
                .map(&f)
                .ok_or_else(|| Error::Parquet("dictionary index out of range".into()))
        })
        .collect()
}

fn parse_page_header(r: &mut CompactReader) -> Result<PageHeader> {
    let mut page_type = -1i32;
    let mut compressed = -1i32;
    let mut uncompressed = -1i32;
    let mut num_values = 0i32;
    let mut encoding = -1i32;

    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                1 => page_type = r.read_i32()?,
                2 => uncompressed = r.read_i32()?,
                3 => compressed = r.read_i32()?,
                // data_page_header (5) and dictionary_page_header (7) both begin
                // with num_values (1) then the value encoding (2).
                5 | 7 => {
                    let (nv, enc) = parse_page_subheader(r)?;
                    num_values = nv;
                    encoding = enc;
                }
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();

    if compressed < 0 || uncompressed < 0 || page_type < 0 {
        return Err(Error::Parquet("incomplete page header".into()));
    }
    Ok(PageHeader {
        page_type,
        compressed_size: compressed as usize,
        uncompressed_size: uncompressed as usize,
        num_values,
        encoding,
    })
}

fn parse_page_subheader(r: &mut CompactReader) -> Result<(i32, i32)> {
    let mut num_values = 0i32;
    let mut encoding = -1i32;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                1 => num_values = r.read_i32()?,
                2 => encoding = r.read_i32()?,
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    Ok((num_values, encoding))
}

/// Bits needed to represent values in `0..=max`.
fn bits_needed(max: u32) -> u32 {
    if max == 0 { 0 } else { 32 - max.leading_zeros() }
}

/// Distribute `n` present values across rows: a `1` level takes the next value,
/// a `0` level is null.
fn align<T>(levels: &[u64], values: Vec<T>) -> Vec<Option<T>> {
    let mut it = values.into_iter();
    levels
        .iter()
        .map(|&d| if d == 1 { it.next() } else { None })
        .collect()
}

/// Decode an RLE/bit-pack hybrid stream (definition levels or dictionary
/// indices), yielding `count` values at `bit_width`.
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
    fn decode_levels_bit_packed() {
        // One bit-packed group of 8 values at bit width 3: 0..8.
        // header = (1 groups << 1) | 1 = 3. Packed LSB-first, 3 bytes.
        let data = [0x03, 0b1000_1000, 0b1100_0110, 0b1111_1010];
        let out = decode_levels(&data, 3, 8).unwrap();
        assert_eq!(out, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn bits_needed_matches_dictionary_sizing() {
        assert_eq!(bits_needed(0), 0);
        assert_eq!(bits_needed(1), 1);
        assert_eq!(bits_needed(2), 2);
        assert_eq!(bits_needed(4), 3);
        assert_eq!(bits_needed(255), 8);
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

    #[test]
    fn dictionary_indices_map_through_dict() {
        // Dict of 3 strings; a data page selecting them by RLE_DICTIONARY.
        let dict = Dict::Bytes(vec![b"red".to_vec(), b"green".to_vec(), b"blue".to_vec()]);
        // 4 present values, indices [2,1,0,2], bit width 2, one bit-packed group.
        // values = [bit_width][run header][packed bytes]; run header (1<<1)|1 = 3
        // means one bit-packed group of 8 (2 bytes at width 2). Packed LSB-first:
        // idx0=10, idx1=01, idx2=00, idx3=10 -> byte0 = 0x86, byte1 (unused) = 0.
        let values = [2u8, 0x03, 0x86, 0x00];
        let present = vec![1u64; 4];
        let mut out = ColumnData::Bytes(Vec::new());
        decode_dict_indices(&mut out, &dict, &values, &present, 4).unwrap();
        match out {
            ColumnData::Bytes(v) => assert_eq!(
                v,
                vec![
                    Some(b"blue".to_vec()),
                    Some(b"green".to_vec()),
                    Some(b"red".to_vec()),
                    Some(b"blue".to_vec()),
                ]
            ),
            _ => panic!("wrong variant"),
        }
    }
}
