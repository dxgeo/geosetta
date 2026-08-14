//! Read GeoParquet back into features.
//!
//! Handles the shape Geosetta writes and the common shape other tools
//! (DuckDB, Arrow, GDAL) emit: multiple row groups, one dictionary page plus
//! one or more data pages per column chunk, PLAIN and dictionary
//! (`PLAIN_DICTIONARY` / `RLE_DICTIONARY`) value encodings, RLE/bit-pack
//! definition levels, SNAPPY/GZIP/ZSTD/LZ4_RAW or no compression, and flat 2D
//! WKB geometry.
//!
//! The schema itself doesn't have to be flat: `flatten_schema` walks the
//! file's schema tree (not just each leaf's own repetition type) so a leaf
//! nested under OPTIONAL groups still gets the right definition level — the
//! case that matters in practice is GDAL/OGR's GeoParquet 1.1
//! `geometry_bbox` "covering" column, a per-row bbox struct it writes
//! alongside the geometry by default (recognized via the `geo` metadata's
//! `covering` key and excluded from `properties`, not surfaced as a fake
//! column — see `tests/fixtures/gdal_covering_bbox.parquet`'s test). A
//! REPEATED ancestor (a genuinely list-valued column) is a different,
//! harder problem — decoding one needs repetition levels this reader
//! doesn't parse — so that's still reported as a specific error rather than
//! misread, along with the remaining codecs (LZO, BROTLI, Hadoop-framed
//! LZ4) and `DATA_PAGE_V2`. See `plans/arbitrary-geoparquet.org` for what
//! remains.

use super::geo::GEOMETRY_COLUMN;
use super::thrift::{CompactReader, Field};
use super::types::{codec, converted, encoding, page, ptype, repetition};
use crate::compress::{gzip, lz4, snappy, zstd};
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
    /// The CRS recovered from the `geo` metadata, carried through unchanged.
    pub crs: Option<crate::crs::Crs>,
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
            // A GeoParquet "covering" bbox index column (see
            // `Meta::covering_paths`) — a spatial-pruning optimization, not a
            // real feature property, so it's skipped entirely rather than
            // decoded. Checked consistently across every row group, so
            // `prop_idx` still lines up with `properties`.
            if meta.covering_paths.iter().any(|p| p == &col.path) {
                continue;
            }
            if col.unsupported_repeated {
                return Err(Error::Parquet(format!(
                    "column \"{}\" is a nested/repeated (list-valued) column, which is not supported yet",
                    col.path.join(".")
                )));
            }
            let is_geometry = col.name == meta.geometry_column;
            match decode_column(bytes, col, rg_rows, is_geometry)? {
                ColumnSink::Wkb(v) => {
                    has_geometry = true;
                    geometry.extend(v);
                }
                ColumnSink::Json { out: values, .. } => {
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
    }

    if !has_geometry {
        geometry = vec![None; num_rows];
    }
    Ok(GeoParquet {
        num_rows,
        properties,
        geometry,
        crs: meta.crs,
    })
}

// --- footer parsing --------------------------------------------------------

struct Meta {
    num_rows: i64,
    row_groups: Vec<RowGroup>,
    geometry_column: String,
    crs: Option<crate::crs::Crs>,
    /// Full schema paths of GeoParquet 1.1 "covering" bbox index columns (e.g.
    /// `["geometry_bbox", "xmin"]`) — a spatial-pruning optimization some
    /// writers (GDAL/OGR) emit alongside the geometry column, not a real
    /// feature property. Read as ordinary columns (their definition levels
    /// still need the schema-tree walk below to decode correctly) but
    /// excluded from `properties` in `read_geoparquet`.
    covering_paths: Vec<Vec<String>>,
}

struct RowGroup {
    num_rows: i64,
    columns: Vec<ColumnMeta>,
}

struct ColumnMeta {
    /// The leaf's own name — `path`'s last segment. Used for property JSON
    /// keys and the (always top-level) geometry-column match; schema lookups
    /// use the full `path` instead, since a bare name isn't unique once
    /// nested columns are in play (two different structs could each have an
    /// "xmin" leaf).
    name: String,
    /// Full dotted path from the schema root (`path_in_schema`), e.g.
    /// `["geometry_bbox", "xmin"]` for a nested leaf or `["height"]` for a
    /// top-level one.
    path: Vec<String>,
    physical: i32,
    codec: i32,
    data_page_offset: i64,
    dictionary_page_offset: Option<i64>,
    /// The number of OPTIONAL/REPEATED ancestors (including the leaf itself)
    /// from the schema root, computed by walking the schema tree — *not*
    /// just the leaf's own repetition type, which is only correct for
    /// top-level columns (see `flatten_schema`).
    max_def_level: u32,
    /// Set when the leaf or any ancestor group is REPEATED (a list-valued
    /// column) — decoding one needs repetition levels this reader doesn't
    /// parse, so it's reported as a clear error rather than misread.
    unsupported_repeated: bool,
    /// The schema's converted_type, if any (e.g. DATE, TIMESTAMP_*).
    converted_type: Option<i32>,
}

fn parse_file_metadata(footer: &[u8]) -> Result<Meta> {
    let mut r = CompactReader::new(footer);
    let mut num_rows = 0i64;
    let mut row_groups: Vec<RowGroup> = Vec::new();
    let mut geometry_column = GEOMETRY_COLUMN.to_string();
    let mut crs: Option<crate::crs::Crs> = None;
    let mut covering_paths: Vec<Vec<String>> = Vec::new();
    // The flat, pre-order SchemaElement list — a tree in disguise (see
    // `flatten_schema`), not one entry per column.
    let mut schema: Vec<SchemaNode> = Vec::new();

    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                2 => {
                    // schema: list<SchemaElement>
                    let (_elem, len) = r.read_list_header()?;
                    for _ in 0..len {
                        schema.push(parse_schema_element(&mut r)?);
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
                    // key_value_metadata: pull primary_column/covering from "geo".
                    let (_elem, len) = r.read_list_header()?;
                    for _ in 0..len {
                        let (k, v) = parse_key_value(&mut r)?;
                        if k == "geo"
                            && let Some(geo) = v.as_deref()
                        {
                            if let Some(pc) = primary_column(geo) {
                                geometry_column = pc;
                            }
                            crs = super::geo::parse_crs(geo);
                            covering_paths = covering_bbox_paths(geo);
                        }
                    }
                }
                _ => r.skip(ty)?, // version (1), created_by (6), etc.
            },
        }
    }
    r.struct_end();

    // Attach schema info (definition level, converted type) to every column,
    // matched by full path — a bare leaf name isn't unique once nested
    // columns are in play (see `ColumnMeta::path`'s doc comment).
    let leaves = flatten_schema(&schema);
    for rg in &mut row_groups {
        for col in &mut rg.columns {
            match leaves.iter().find(|l| l.path == col.path) {
                Some(leaf) => {
                    col.max_def_level = leaf.max_def_level;
                    col.unsupported_repeated = leaf.has_repeated;
                    col.converted_type = leaf.converted_type;
                }
                // No schema entry for this path (shouldn't happen in a
                // well-formed file) — fall back to the old conservative
                // default rather than fail outright.
                None => col.max_def_level = 1,
            }
        }
    }

    Ok(Meta {
        num_rows,
        row_groups,
        geometry_column,
        crs,
        covering_paths,
    })
}

/// One entry from the file's flat, pre-order `list<SchemaElement>` — the root
/// "message" element, an intermediate group, or a leaf. `num_children` is what
/// makes the list a *tree*: a leaf omits it (decoded as 0), a group gives the
/// count of elements immediately following it in the list that are its
/// children (recursively, so a child that's itself a group is followed by
/// its own children before the next sibling).
struct SchemaNode {
    name: String,
    repetition: i32,
    num_children: i32,
    converted_type: Option<i32>,
}

fn parse_schema_element(r: &mut CompactReader) -> Result<SchemaNode> {
    let mut name = String::new();
    let mut rep = repetition::REQUIRED;
    let mut num_children = 0i32;
    let mut converted = None;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                3 => rep = r.read_i32()?,
                4 => name = r.read_string()?,
                5 => num_children = r.read_i32()?,
                6 => converted = Some(r.read_i32()?),
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    Ok(SchemaNode { name, repetition: rep, num_children, converted_type: converted })
}

/// A leaf's identity and decoding-relevant facts, recovered by walking the
/// schema tree (see [`flatten_schema`]) rather than reading one
/// [`SchemaNode`] in isolation.
struct LeafInfo {
    /// Full dotted path from the schema root, matching a column chunk's
    /// `path_in_schema` (see `ColumnMeta::path`).
    path: Vec<String>,
    /// Count of OPTIONAL/REPEATED elements from the root down to and
    /// including this leaf — the definition level a present, non-null value
    /// at this leaf is written with. Parquet's schema root itself never
    /// contributes (its own repetition type is not meaningful).
    max_def_level: u32,
    /// Whether this leaf or any ancestor group is REPEATED.
    has_repeated: bool,
    converted_type: Option<i32>,
}

/// Reconstruct the schema tree from the file's flat pre-order `SchemaNode`
/// list (root first, each group immediately followed by its `num_children`
/// descendants) and flatten it back out into one [`LeafInfo`] per leaf, with
/// the definition level and repeated-ness accumulated through every ancestor
/// — not just the leaf's own repetition type, which alone is only correct
/// when nothing is nested. `nodes[0]` is the root message and is consumed
/// without contributing to any leaf's definition level, matching the Parquet
/// spec's definition-level algorithm.
fn flatten_schema(nodes: &[SchemaNode]) -> Vec<LeafInfo> {
    fn walk(
        nodes: &[SchemaNode],
        idx: &mut usize,
        path: &mut Vec<String>,
        parent_def: u32,
        parent_repeated: bool,
        leaves: &mut Vec<LeafInfo>,
    ) {
        let Some(node) = nodes.get(*idx) else { return };
        *idx += 1;
        let is_repeated = node.repetition == repetition::REPEATED;
        let def = parent_def + u32::from(node.repetition != repetition::REQUIRED);
        let has_repeated = parent_repeated || is_repeated;

        path.push(node.name.clone());
        if node.num_children <= 0 {
            leaves.push(LeafInfo {
                path: path.clone(),
                max_def_level: def,
                has_repeated,
                converted_type: node.converted_type,
            });
        } else {
            for _ in 0..node.num_children {
                walk(nodes, idx, path, def, has_repeated, leaves);
            }
        }
        path.pop();
    }

    let mut leaves = Vec::new();
    let Some(root) = nodes.first() else { return leaves };
    let mut idx = 1usize;
    let mut path = Vec::new();
    for _ in 0..root.num_children {
        walk(nodes, &mut idx, &mut path, 0, false, &mut leaves);
    }
    leaves
}

/// Column paths (full `path_in_schema` segments, e.g. `["geometry_bbox",
/// "xmin"]`) that a GeoParquet "covering" bbox index references — see
/// [`Meta::covering_paths`].
fn covering_bbox_paths(geo: &str) -> Vec<Vec<String>> {
    let Ok(root) = json::parse(geo) else { return Vec::new() };
    let Some(columns) = root.get("columns").and_then(JsonValue::as_object) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for (_, col) in columns {
        let Some(bbox) = col.get("covering").and_then(|c| c.get("bbox")).and_then(JsonValue::as_object) else {
            continue;
        };
        for (_, path) in bbox {
            if let Some(segments) = path.as_array() {
                let segments: Vec<String> = segments.iter().filter_map(JsonValue::as_str).map(String::from).collect();
                if !segments.is_empty() {
                    paths.push(segments);
                }
            }
        }
    }
    paths
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
        name: path.last().cloned().unwrap_or_default(),
        path,
        physical,
        codec,
        data_page_offset,
        dictionary_page_offset,
        max_def_level: 1,          // set from the schema in parse_file_metadata
        unsupported_repeated: false, // set from the schema in parse_file_metadata
        converted_type: None,      // set from the schema in parse_file_metadata
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

/// Where a decoded column chunk accumulates its rows.
///
/// Property columns decode *straight* into per-row [`JsonValue`] against the
/// definition-level present-mask — no intermediate `Vec<Option<T>>` and no
/// second pass to convert. The geometry column instead keeps its raw WKB blobs
/// as bytes, which the caller hands to `from_wkb`.
enum ColumnSink {
    Json {
        out: Vec<JsonValue>,
        /// The schema's converted_type, driving DATE/TIMESTAMP rendering.
        converted: Option<i32>,
    },
    Wkb(Vec<Option<Vec<u8>>>),
}

/// A decoded dictionary page, indexed by the data pages that follow it.
enum Dict {
    Int(Vec<i64>),
    Double(Vec<f64>),
    Bytes(Vec<Vec<u8>>),
}

/// Which rows of a data page carry a value.
///
/// The overwhelmingly common case — a column with no nulls, which Geosetta
/// always writes as a single RLE run — is [`Present::All`], so no per-row mask
/// is allocated or scanned. Only a page that actually contains nulls falls back
/// to a one-byte-per-row [`Present::Mask`] (was a `Vec<u64>`, eight bytes each).
enum Present {
    All(usize),
    Mask(Vec<bool>),
}

impl Present {
    /// How many rows carry a value.
    fn count(&self) -> usize {
        match self {
            Present::All(n) => *n,
            Present::Mask(m) => m.iter().filter(|&&p| p).count(),
        }
    }
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
/// `rg_rows` aligned values. A geometry column keeps raw WKB bytes; every other
/// column decodes straight to per-row JSON.
fn decode_column(
    file: &[u8],
    col: &ColumnMeta,
    rg_rows: usize,
    is_geometry: bool,
) -> Result<ColumnSink> {
    // A dictionary page, if present, precedes the data pages.
    let start = col
        .dictionary_page_offset
        .filter(|&o| o >= 0)
        .unwrap_or(col.data_page_offset) as usize;

    let mut out = if is_geometry {
        ColumnSink::Wkb(Vec::with_capacity(rg_rows))
    } else {
        ColumnSink::Json {
            out: Vec::with_capacity(rg_rows),
            converted: col.converted_type,
        }
    };
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
        c if c == codec::GZIP => gzip::decompress(comp, uncompressed_size)?,
        c if c == codec::ZSTD => zstd::decompress(comp, uncompressed_size)?,
        c if c == codec::LZ4_RAW => lz4::decompress(comp, uncompressed_size)?,
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

fn decode_dictionary(body: &[u8], physical: i32, count: usize) -> Result<Dict> {
    Ok(match physical {
        p if p == ptype::INT32 => Dict::Int(plain_i32(body, count)?),
        p if p == ptype::INT64 => Dict::Int(plain_i64(body, count)?),
        p if p == ptype::FLOAT => Dict::Double(plain_f32(body, count)?),
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
    out: &mut ColumnSink,
) -> Result<()> {
    let (present, values) = split_definition_levels(body, col.max_def_level, page_rows)?;
    let n_present = present.count();

    match page_encoding {
        e if e == encoding::PLAIN => decode_plain(out, col.physical, values, &present, n_present),
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

/// Split a data-page body into a per-row present mask and the value bytes.
/// A REQUIRED column (`max_def_level == 0`) carries no definition levels.
fn split_definition_levels(
    body: &[u8],
    max_def_level: u32,
    page_rows: usize,
) -> Result<(Present, &[u8])> {
    if max_def_level == 0 {
        return Ok((Present::All(page_rows), body));
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
    Ok((decode_present(levels, bit_width, max_def_level, page_rows)?, values))
}

/// Decode a definition-level stream into a [`Present`] mask.
fn decode_present(
    levels: &[u8],
    bit_width: u32,
    max_def_level: u32,
    page_rows: usize,
) -> Result<Present> {
    // Fast path: a single RLE run of the max level covering the whole page (what
    // an all-present column serializes to) is all-present — no mask needed.
    if is_all_present_rle(levels, bit_width, max_def_level, page_rows) {
        return Ok(Present::All(page_rows));
    }
    let mask = decode_levels(levels, bit_width, page_rows)?
        .into_iter()
        .map(|d| d == max_def_level as u64)
        .collect();
    Ok(Present::Mask(mask))
}

/// Whether `levels` is a single RLE run of `max_def_level` covering the page —
/// i.e. every row present — without materializing the levels.
fn is_all_present_rle(levels: &[u8], bit_width: u32, max_def_level: u32, page_rows: usize) -> bool {
    if bit_width == 0 {
        return max_def_level == 0;
    }
    let Ok((header, adv)) = read_uvarint(levels, 0) else {
        return false;
    };
    if header & 1 != 0 {
        return false; // bit-packed, not a single RLE run
    }
    if ((header >> 1) as usize) < page_rows {
        return false; // run doesn't cover the whole page
    }
    let byte_width = bit_width.div_ceil(8) as usize;
    let Some(val_bytes) = levels.get(adv..adv + byte_width) else {
        return false;
    };
    let mut val = 0u64;
    for (k, &b) in val_bytes.iter().enumerate() {
        val |= (b as u64) << (8 * k);
    }
    val == max_def_level as u64
}

fn decode_plain(
    out: &mut ColumnSink,
    physical: i32,
    values: &[u8],
    present: &Present,
    n: usize,
) -> Result<()> {
    match out {
        // Geometry: keep raw WKB bytes, aligned to rows.
        ColumnSink::Wkb(v) => {
            if physical != ptype::BYTE_ARRAY {
                return Err(Error::Parquet("geometry column is not BYTE_ARRAY".into()));
            }
            v.extend(align(present, plain_byte_arrays(values, n)?));
        }
        // Everything else: decode straight to per-row JSON.
        ColumnSink::Json { out, converted } => match physical {
            p if p == ptype::BOOLEAN => push_json(out, present, plain_bools(values, n)?, JsonValue::Bool),
            p if p == ptype::INT32 => emit_ints(out, *converted, present, plain_i32(values, n)?),
            p if p == ptype::INT64 => emit_ints(out, *converted, present, plain_i64(values, n)?),
            p if p == ptype::FLOAT => push_json(out, present, plain_f32(values, n)?, json_number),
            p if p == ptype::DOUBLE => push_json(out, present, plain_f64(values, n)?, json_number),
            p if p == ptype::BYTE_ARRAY => push_json_strings(out, present, plain_byte_arrays(values, n)?)?,
            other => return Err(Error::Parquet(format!("unsupported physical type {other}"))),
        },
    }
    Ok(())
}

/// Decode a dictionary-index data page: `[1 byte bit width][RLE/bit-pack
/// indices]`, mapped through `dict` and emitted straight to the sink.
fn decode_dict_indices(
    out: &mut ColumnSink,
    dict: &Dict,
    values: &[u8],
    present: &Present,
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

    match out {
        ColumnSink::Wkb(v) => match dict {
            Dict::Bytes(d) => v.extend(align(present, map_indices(&indices, d, |x| x.clone())?)),
            _ => return Err(Error::Parquet("geometry column is not BYTE_ARRAY".into())),
        },
        ColumnSink::Json { out, converted } => match dict {
            Dict::Int(d) => emit_ints(out, *converted, present, map_indices(&indices, d, |x| *x)?),
            Dict::Double(d) => {
                push_json(out, present, map_indices(&indices, d, |x| *x)?, json_number)
            }
            Dict::Bytes(d) => {
                push_json_strings(out, present, map_indices(&indices, d, |x| x.clone())?)?
            }
        },
    }
    Ok(())
}

/// A plain `f64` JSON number.
fn json_number(value: f64) -> JsonValue {
    JsonValue::Number { value, is_int: false }
}

/// Push `values` into `out` at the present rows, inserting `Null` for the rest —
/// a single pass, no `Option<T>` intermediate. `Present::All` skips the mask.
fn push_json<T>(
    out: &mut Vec<JsonValue>,
    present: &Present,
    values: Vec<T>,
    mut f: impl FnMut(T) -> JsonValue,
) {
    match present {
        Present::All(_) => {
            out.reserve(values.len());
            out.extend(values.into_iter().map(&mut f));
        }
        Present::Mask(mask) => {
            out.reserve(mask.len());
            let mut it = values.into_iter();
            for &p in mask {
                out.push(if p {
                    it.next().map_or(JsonValue::Null, &mut f)
                } else {
                    JsonValue::Null
                });
            }
        }
    }
}

/// Integers render as JSON numbers, except DATE/TIMESTAMP columns, which render
/// as ISO 8601 strings.
fn emit_ints(out: &mut Vec<JsonValue>, converted: Option<i32>, present: &Present, values: Vec<i64>) {
    if is_temporal(converted) {
        push_json(out, present, values, |i| {
            JsonValue::String(format_temporal(i, converted))
        });
    } else {
        push_json(out, present, values, |i| JsonValue::Number {
            value: i as f64,
            is_int: true,
        });
    }
}

/// Like [`push_json`] for `BYTE_ARRAY` string columns, validating UTF-8.
fn push_json_strings(
    out: &mut Vec<JsonValue>,
    present: &Present,
    values: Vec<Vec<u8>>,
) -> Result<()> {
    let to_json = |b: Vec<u8>| -> Result<JsonValue> {
        String::from_utf8(b)
            .map(JsonValue::String)
            .map_err(|_| Error::Parquet("string column has invalid utf-8".into()))
    };
    match present {
        Present::All(_) => {
            out.reserve(values.len());
            for b in values {
                out.push(to_json(b)?);
            }
        }
        Present::Mask(mask) => {
            out.reserve(mask.len());
            let mut it = values.into_iter();
            for &p in mask {
                out.push(if p {
                    match it.next() {
                        Some(b) => to_json(b)?,
                        None => JsonValue::Null,
                    }
                } else {
                    JsonValue::Null
                });
            }
        }
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

/// Distribute present values across rows: a present row takes the next value, a
/// null row is `None`. `Present::All` is simply every value wrapped in `Some`.
fn align<T>(present: &Present, values: Vec<T>) -> Vec<Option<T>> {
    match present {
        Present::All(_) => values.into_iter().map(Some).collect(),
        Present::Mask(mask) => {
            let mut it = values.into_iter();
            mask.iter()
                .map(|&p| if p { it.next() } else { None })
                .collect()
        }
    }
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

fn plain_i32(data: &[u8], count: usize) -> Result<Vec<i64>> {
    let slots = fixed_slots(data, count, 4)?;
    Ok(slots
        .map(|s| i32::from_le_bytes(s.try_into().unwrap()) as i64)
        .collect())
}

fn plain_i64(data: &[u8], count: usize) -> Result<Vec<i64>> {
    let slots = fixed_slots(data, count, 8)?;
    Ok(slots.map(|s| i64::from_le_bytes(s.try_into().unwrap())).collect())
}

fn plain_f32(data: &[u8], count: usize) -> Result<Vec<f64>> {
    let slots = fixed_slots(data, count, 4)?;
    Ok(slots
        .map(|s| f32::from_le_bytes(s.try_into().unwrap()) as f64)
        .collect())
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

/// Whether a converted type is a date or timestamp we render as a string.
fn is_temporal(converted: Option<i32>) -> bool {
    matches!(
        converted,
        Some(converted::DATE | converted::TIMESTAMP_MILLIS | converted::TIMESTAMP_MICROS)
    )
}

/// Format an integer temporal value as ISO 8601, given its converted type.
/// DATE is days since the Unix epoch; TIMESTAMP_* are milli/microseconds.
fn format_temporal(value: i64, converted: Option<i32>) -> String {
    match converted {
        Some(converted::DATE) => format_date(value),
        Some(converted::TIMESTAMP_MILLIS) => format_timestamp(value, 3),
        Some(converted::TIMESTAMP_MICROS) => format_timestamp(value, 6),
        _ => value.to_string(),
    }
}

/// Civil year/month/day from a day count since 1970-01-01 (Hinnant's algorithm,
/// valid for any day in the proleptic Gregorian calendar).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `subsecond_digits` is 3 for milliseconds, 6 for microseconds.
fn format_timestamp(value: i64, subsecond_digits: u32) -> String {
    let per_second = 10i64.pow(subsecond_digits);
    let secs = value.div_euclid(per_second);
    let frac = value.rem_euclid(per_second);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let mut s = format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}");
    if frac != 0 {
        s.push_str(&format!(".{frac:0width$}", width = subsecond_digits as usize));
    }
    s
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
        let out = align(&Present::Mask(vec![true, false, true]), vec![10i64, 20]);
        assert_eq!(out, vec![Some(10), None, Some(20)]);
        // The all-present fast path wraps every value in `Some`.
        assert_eq!(align(&Present::All(2), vec![1i64, 2]), vec![Some(1), Some(2)]);
    }

    #[test]
    fn all_present_rle_detects_single_max_run() {
        // Writer's all-present encoding: RLE run of `1`, length = rows.
        // header = (rows << 1) | 0, then one value byte (width 1 for max=1).
        assert!(is_all_present_rle(&[0x0a, 0x01], 1, 1, 5)); // 5 rows, all level 1
        // A run of level 0 (all null) is not all-present.
        assert!(!is_all_present_rle(&[0x0a, 0x00], 1, 1, 5));
        // A bit-packed run (header LSB set) is not the fast path.
        assert!(!is_all_present_rle(&[0x03, 0x00], 1, 1, 5));
        // A run shorter than the page is not all-present.
        assert!(!is_all_present_rle(&[0x06, 0x01], 1, 1, 5)); // run_len 3 < 5
    }

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01"); // epoch
        assert_eq!(format_date(18262), "2020-01-01");
        assert_eq!(format_date(-1), "1969-12-31"); // before epoch
        assert_eq!(format_date(59), "1970-03-01"); // 1970 not a leap year
    }

    #[test]
    fn formats_timestamps() {
        // 2020-01-01T00:00:00 in micros since epoch.
        let base = 18262i64 * 86_400 * 1_000_000;
        assert_eq!(format_timestamp(base, 6), "2020-01-01T00:00:00");
        assert_eq!(format_timestamp(base + 3_600_000_000, 6), "2020-01-01T01:00:00");
        // Sub-second micros are rendered with six digits.
        assert_eq!(format_timestamp(base + 123_456, 6), "2020-01-01T00:00:00.123456");
        // Milliseconds scale, three digits.
        assert_eq!(format_timestamp(18262 * 86_400 * 1000 + 500, 3), "2020-01-01T00:00:00.500");
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
        let present = Present::Mask(vec![true; 4]);
        // Use the WKB sink to observe the mapped-through-dict bytes directly.
        let mut out = ColumnSink::Wkb(Vec::new());
        decode_dict_indices(&mut out, &dict, &values, &present, 4).unwrap();
        match out {
            ColumnSink::Wkb(v) => assert_eq!(
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

    // --- flatten_schema / nested-group definition levels -------------------

    fn node(name: &str, rep: i32, num_children: i32) -> SchemaNode {
        SchemaNode { name: name.into(), repetition: rep, num_children, converted_type: None }
    }

    #[test]
    fn flat_schema_uses_each_leafs_own_repetition() {
        // root -> [name (OPTIONAL), height (REQUIRED)] — the shape every
        // fixture before GDAL's covering-bbox column exercised.
        let schema = vec![
            node("schema", repetition::REQUIRED, 2),
            node("name", repetition::OPTIONAL, 0),
            node("height", repetition::REQUIRED, 0),
        ];
        let leaves = flatten_schema(&schema);
        let by_path = |p: &[&str]| leaves.iter().find(|l| l.path == p).unwrap();
        assert_eq!(by_path(&["name"]).max_def_level, 1);
        assert_eq!(by_path(&["height"]).max_def_level, 0);
        assert!(leaves.iter().all(|l| !l.has_repeated));
    }

    #[test]
    fn nested_leaf_inherits_its_optional_groups_definition_level() {
        // The real shape from `tests/fixtures/gdal_covering_bbox.parquet`:
        // root -> [name, height, geometry (all OPTIONAL),
        //          geometry_bbox (OPTIONAL group) -> [xmin, ymin, xmax, ymax (REQUIRED)]].
        // xmin etc. are REQUIRED *within* the group, but the group itself is
        // OPTIONAL, so their true max_def_level is 1 — not 0, which is what
        // reading only the leaf's own repetition_type (the pre-fix behavior)
        // would give, and what caused the original "zero-length level run"
        // misparse against this exact file.
        let schema = vec![
            node("schema", repetition::REQUIRED, 4),
            node("name", repetition::OPTIONAL, 0),
            node("height", repetition::OPTIONAL, 0),
            node("geometry", repetition::OPTIONAL, 0),
            node("geometry_bbox", repetition::OPTIONAL, 4),
            node("xmin", repetition::REQUIRED, 0),
            node("ymin", repetition::REQUIRED, 0),
            node("xmax", repetition::REQUIRED, 0),
            node("ymax", repetition::REQUIRED, 0),
        ];
        let leaves = flatten_schema(&schema);
        assert_eq!(leaves.len(), 7);
        let by_path = |p: &[&str]| leaves.iter().find(|l| l.path == p).unwrap();
        for top in ["name", "height", "geometry"] {
            assert_eq!(by_path(&[top]).max_def_level, 1, "{top}");
        }
        for leaf in ["xmin", "ymin", "xmax", "ymax"] {
            let l = by_path(&["geometry_bbox", leaf]);
            assert_eq!(l.max_def_level, 1, "{leaf}: own repetition is REQUIRED, but the enclosing group is OPTIONAL");
            assert!(!l.has_repeated);
        }
    }

    #[test]
    fn repeated_ancestor_marks_the_leaf_unsupported() {
        // root -> tags (REPEATED group, e.g. a list<string>) -> list -> item
        let schema = vec![
            node("schema", repetition::REQUIRED, 1),
            node("tags", repetition::REPEATED, 1),
            node("item", repetition::OPTIONAL, 0),
        ];
        let leaves = flatten_schema(&schema);
        assert_eq!(leaves.len(), 1);
        assert!(leaves[0].has_repeated);
        // A REPEATED ancestor also counts toward the definition level, same
        // as OPTIONAL — tags(REPEATED)=1 + item(OPTIONAL)=1.
        assert_eq!(leaves[0].max_def_level, 2);
    }

    #[test]
    fn covering_bbox_paths_reads_the_geo_metadata_covering_key() {
        let geo = r#"{"version":"1.1.0","primary_column":"geometry","columns":{"geometry":{
            "encoding":"WKB",
            "covering":{"bbox":{
                "xmin":["geometry_bbox","xmin"],"ymin":["geometry_bbox","ymin"],
                "xmax":["geometry_bbox","xmax"],"ymax":["geometry_bbox","ymax"]
            }}
        }}}"#;
        let mut paths = covering_bbox_paths(geo);
        paths.sort();
        assert_eq!(
            paths,
            vec![
                vec!["geometry_bbox".to_string(), "xmax".to_string()],
                vec!["geometry_bbox".to_string(), "xmin".to_string()],
                vec!["geometry_bbox".to_string(), "ymax".to_string()],
                vec!["geometry_bbox".to_string(), "ymin".to_string()],
            ]
        );
    }

    #[test]
    fn covering_bbox_paths_is_empty_without_a_covering_key() {
        let geo = r#"{"version":"1.0.0","primary_column":"geometry","columns":{"geometry":{"encoding":"WKB"}}}"#;
        assert!(covering_bbox_paths(geo).is_empty());
    }

    #[test]
    fn reads_a_real_gdal_fixture_with_a_covering_bbox_column() {
        // `ogr2ogr -f Parquet` output for a 3-point GeoJSON (name/height
        // properties) — GDAL 3.13 / Arrow 25 writes a GeoParquet 1.1
        // "geometry_bbox" covering-bbox struct alongside the WKB geometry by
        // default. Before the schema-tree walk above, this file's xmin/ymin/
        // xmax/ymax columns were misread as REQUIRED (def_level 0) instead of
        // 1, causing the reader to misparse their data pages entirely
        // ("invalid parquet: zero-length level run").
        let bytes = include_bytes!("../../tests/fixtures/gdal_covering_bbox.parquet");
        let parsed = read_geoparquet(bytes).expect("reads the GDAL fixture");
        assert_eq!(parsed.num_rows, 3);

        // The covering-bbox columns are excluded, not surfaced as properties.
        let names: Vec<&str> = parsed.properties.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["name", "height"]);

        let prop = |col: &str, row: usize| {
            parsed.properties.iter().find(|c| c.name == col).unwrap().values[row].clone()
        };
        assert_eq!(prop("name", 0).as_str(), Some("Empire State"));
        assert_eq!(prop("height", 0).as_f64(), Some(381.0));
        assert_eq!(prop("name", 2).as_str(), Some("Tokyo"));
        assert_eq!(prop("height", 2).as_f64(), Some(40.0));

        assert_eq!(parsed.geometry.len(), 3);
        assert!(parsed.geometry.iter().all(Option::is_some));
    }
}
