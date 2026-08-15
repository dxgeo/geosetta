//! Read GeoParquet back into features.
//!
//! Handles the shape Geosetta writes and the common shape other tools
//! (DuckDB, Arrow, GDAL, pyarrow) emit: multiple row groups, one dictionary
//! page plus one or more data pages per column chunk (DATA_PAGE and
//! DATA_PAGE_V2 both), PLAIN and dictionary (`PLAIN_DICTIONARY` /
//! `RLE_DICTIONARY`) value encodings, RLE/bit-pack definition levels,
//! SNAPPY/GZIP/ZSTD/LZ4_RAW or no compression, INT96/DECIMAL/JSON alongside
//! the base physical types, and flat 2D WKB geometry.
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
//! LZ4). See `plans/arbitrary-geoparquet.org` for what remains.
//!
//! The geometry column itself doesn't have to be named by `geo` metadata: a
//! schema leaf tagged Parquet's native `GEOMETRY`/`GEOGRAPHY` logical type
//! (`SchemaElement.logicalType`, `parse_geometry_logical_type`) is recognized
//! too, filling in the column name and CRS when `geo` doesn't supply them —
//! some writers (e.g. `ogr2ogr -lco USE_PARQUET_GEO_TYPES=ONLY`) drop `geo`
//! entirely in favor of it. A second native-tagged column that isn't the
//! chosen primary errors clearly rather than misdecoding as text; true
//! multi-geometry support is still open.

use super::geo::GEOMETRY_COLUMN;
use super::thrift::{ct, CompactReader, Field};
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
            // A second native GEOMETRY/GEOGRAPHY-tagged column (see
            // `ColumnMeta::is_native_geometry`) — Geosetta's `Feature` IR has
            // room for exactly one geometry per feature, so a real second one
            // can't be decoded as a property (it's WKB, not text) or silently
            // dropped (that would lose real geometry data). Multi-geometry
            // support is future work; until then, fail clearly.
            if col.is_native_geometry && !is_geometry {
                return Err(Error::Parquet(format!(
                    "column \"{}\" is a native GEOMETRY/GEOGRAPHY column, but \"{}\" is already the primary geometry column — multiple geometry columns are not supported yet",
                    col.path.join("."),
                    meta.geometry_column
                )));
            }
            // A second geometry column declared in the `geo` metadata's
            // `columns` object (see `Meta::geo_geometry_columns`) — same
            // reasoning as the native-tagged case above, just the other
            // GeoParquet convention for naming a geometry column.
            if !is_geometry && meta.geo_geometry_columns.iter().any(|c| c == &col.name) {
                return Err(Error::Parquet(format!(
                    "column \"{}\" is a geometry column (per the \"geo\" metadata), but \"{}\" is already the primary geometry column — multiple geometry columns are not supported yet",
                    col.path.join("."),
                    meta.geometry_column
                )));
            }
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
    /// Every geometry column name declared in the `geo` metadata's `columns`
    /// object (see `geo_geometry_columns`) — a superset of `geometry_column`
    /// (the primary one) when a writer emits more than one. Used only to
    /// detect and reject a second geometry column; the `Feature` IR holds one
    /// geometry per feature.
    geo_geometry_columns: Vec<String>,
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
    /// `ColumnMetaData.num_values` (thrift field 5) — the chunk's total value
    /// count. For an ordinary column this always equals the row group's row
    /// count, so [`decode_column`]'s page loop uses `rg_rows` directly and
    /// never needs this field. A list-eligible column's occurrences don't
    /// equal its row count, and a single row's elements aren't guaranteed to
    /// stay within one physical page — so [`decode_list_column`] uses this
    /// (not a row count) as its only correct stopping bound.
    num_values: i64,
    /// The number of OPTIONAL/REPEATED ancestors (including the leaf itself)
    /// from the schema root, computed by walking the schema tree — *not*
    /// just the leaf's own repetition type, which is only correct for
    /// top-level columns (see `flatten_schema`).
    max_def_level: u32,
    /// Count of REPEATED ancestors (see `LeafInfo::max_rep_level`) — 0 for an
    /// ordinary column, 1 for a decodable single-level list. Only meaningful
    /// when `unsupported_repeated` is `false`.
    max_rep_level: u32,
    /// See `LeafInfo::list_group_def_level`. Only meaningful when
    /// `max_rep_level > 0` and `!unsupported_repeated`.
    list_group_def_level: u32,
    /// Set when the leaf is REPEATED (or has a REPEATED ancestor) in a shape
    /// this reader can't decode: a list of structs (the REPEATED group has
    /// more than one child) or a list nested inside another list
    /// (`max_rep_level >= 2`) — see `LeafInfo::list_eligible`. A *decodable*
    /// single-level list of scalars leaves this `false` and instead carries
    /// `max_rep_level == 1`.
    unsupported_repeated: bool,
    /// The schema's converted_type, if any (e.g. DATE, TIMESTAMP_*).
    converted_type: Option<i32>,
    /// `FIXED_LEN_BYTE_ARRAY`'s element width in bytes (`SchemaElement.type_length`).
    /// `None` for every other physical type.
    type_length: Option<i32>,
    /// `DECIMAL`'s scale (digits after the point), from `SchemaElement.scale`.
    /// `None` when the column isn't `DECIMAL`.
    scale: Option<i32>,
    /// Whether `SchemaElement.logicalType` tags this leaf `GeometryType` or
    /// `GeographyType` (Parquet's native geometry logical type, distinct from
    /// the GeoParquet `geo` key/value metadata convention). Set from the
    /// schema regardless of which column `read_geoparquet` treats as *the*
    /// geometry column — a native-tagged column that isn't chosen as primary
    /// is rejected with a clear error rather than misdecoded as text.
    is_native_geometry: bool,
}

fn parse_file_metadata(footer: &[u8]) -> Result<Meta> {
    let mut r = CompactReader::new(footer);
    let mut num_rows = 0i64;
    let mut row_groups: Vec<RowGroup> = Vec::new();
    let mut geometry_column = GEOMETRY_COLUMN.to_string();
    let mut geo_primary_found = false;
    let mut crs: Option<crate::crs::Crs> = None;
    let mut covering_paths: Vec<Vec<String>> = Vec::new();
    let mut geo_geometry_columns: Vec<String> = Vec::new();
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
                                geo_primary_found = true;
                            }
                            crs = super::geo::parse_crs(geo);
                            covering_paths = covering_bbox_paths(geo);
                            geo_geometry_columns = self::geo_geometry_columns(geo);
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
                    col.max_rep_level = leaf.max_rep_level;
                    col.list_group_def_level = leaf.list_group_def_level;
                    col.unsupported_repeated = leaf.max_rep_level > 0 && !leaf.list_eligible;
                    col.converted_type = leaf.converted_type;
                    col.type_length = leaf.type_length;
                    col.scale = leaf.scale;
                    col.is_native_geometry = leaf.is_geometry;
                    // A list-eligible column's `name` (used as the decoded
                    // property's JSON key) must be the *outer* list
                    // property's name — `path[0]`, e.g. "tags" — not the
                    // innermost leaf's own bare name (`path.last()`, e.g.
                    // "element" for the standard 3-level shape), which is
                    // never a name the source data actually used.
                    if leaf.list_eligible
                        && let Some(top) = leaf.path.first()
                    {
                        col.name = top.clone();
                    }
                }
                // No schema entry for this path (shouldn't happen in a
                // well-formed file) — fall back to the old conservative
                // default rather than fail outright.
                None => col.max_def_level = 1,
            }
        }
    }

    // Parquet's native GEOMETRY/GEOGRAPHY logical type (`SchemaElement.
    // logicalType`, tags 17/18 — see `parse_geometry_logical_type`) fills in
    // whatever the `geo` key/value metadata left unset. Some writers (e.g.
    // GDAL's `ogr2ogr -lco USE_PARQUET_GEO_TYPES=ONLY`) drop `geo` entirely in
    // favor of the native type, so the primary column's name and CRS have to
    // be recoverable from the schema alone in that case. `geo` still wins
    // when it named a primary column — this only fills gaps, never overrides.
    let native_geometry: Vec<&LeafInfo> = leaves.iter().filter(|l| l.is_geometry).collect();
    if !geo_primary_found
        && let [only] = native_geometry.as_slice()
        && let Some(name) = only.path.last()
    {
        geometry_column = name.clone();
    }
    if crs.is_none()
        && let Some(primary) = native_geometry
            .iter()
            .find(|l| l.path.last().map(String::as_str) == Some(geometry_column.as_str()))
    {
        crs = super::geo::parse_native_geometry_crs(primary.geometry_crs.as_deref());
    }

    Ok(Meta {
        num_rows,
        row_groups,
        geometry_column,
        crs,
        covering_paths,
        geo_geometry_columns,
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
    /// `type_length` (field 2) — `FIXED_LEN_BYTE_ARRAY`'s element width.
    type_length: Option<i32>,
    /// `scale` (field 7) — `DECIMAL`'s digit count after the point.
    scale: Option<i32>,
    /// Whether `logicalType` (field 10) tags this element `GeometryType` (union
    /// tag 17) or `GeographyType` (tag 18) — Parquet's native geometry logical
    /// type, layered on top of an ordinary `BYTE_ARRAY` WKB column (same
    /// physical encoding [`decode_plain`]/[`decode_dict_indices`] already
    /// handle for `geo`-metadata-style geometry — this only changes how the
    /// column is *found*, not how it's decoded).
    is_geometry: bool,
    /// `GeometryType`/`GeographyType`'s own `crs` field (union member 1): a
    /// raw PROJJSON string, empirically confirmed against a real
    /// `ogr2ogr -f Parquet -lco USE_PARQUET_GEO_TYPES=ONLY` fixture. Absent
    /// means the format's own default, OGC:CRS84 (mirrors `geo` metadata's
    /// absent/null `crs` convention) — see
    /// [`super::geo::parse_native_geometry_crs`].
    geometry_crs: Option<String>,
}

fn parse_schema_element(r: &mut CompactReader) -> Result<SchemaNode> {
    let mut name = String::new();
    let mut rep = repetition::REQUIRED;
    let mut num_children = 0i32;
    let mut converted = None;
    let mut type_length = None;
    let mut scale = None;
    let mut is_geometry = false;
    let mut geometry_crs = None;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                2 => type_length = Some(r.read_i32()?),
                3 => rep = r.read_i32()?,
                4 => name = r.read_string()?,
                5 => num_children = r.read_i32()?,
                6 => converted = Some(r.read_i32()?),
                7 => scale = Some(r.read_i32()?),
                10 => (is_geometry, geometry_crs) = parse_geometry_logical_type(r)?,
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();
    Ok(SchemaNode {
        name,
        repetition: rep,
        num_children,
        converted_type: converted,
        type_length,
        scale,
        is_geometry,
        geometry_crs,
    })
}

/// Parse `SchemaElement.logicalType` (a thrift union: exactly one member set)
/// looking only for `GeometryType` (member 17) or `GeographyType` (member 18)
/// — every other member (`StringType`, `DecimalType`, `TimestampType`, …) is
/// skipped whole, since `converted_type` already covers what this reader acts
/// on for those. When found, also reads the member's own `crs` field (field 1
/// of `GeometryType`/`GeographyType`, both share the shape empirically) if
/// present. Field IDs (`GeometryType` = 17, `crs` = 1) were confirmed against
/// a real `ogr2ogr -f Parquet -lco USE_PARQUET_GEO_TYPES=ONLY` fixture's raw
/// thrift bytes — parquet-format's public docs don't tabulate them plainly.
fn parse_geometry_logical_type(r: &mut CompactReader) -> Result<(bool, Option<String>)> {
    let mut is_geometry = false;
    let mut crs = None;
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => {
                if id == 17 || id == 18 {
                    is_geometry = true;
                    r.struct_begin();
                    loop {
                        match r.read_field()? {
                            Field::Stop => break,
                            Field::Begin { id: inner_id, ty: inner_ty } => {
                                if inner_id == 1 {
                                    crs = Some(r.read_string()?);
                                } else {
                                    r.skip(inner_ty)?;
                                }
                            }
                        }
                    }
                    r.struct_end();
                } else {
                    r.skip(ty)?;
                }
            }
        }
    }
    r.struct_end();
    Ok((is_geometry, crs))
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
    /// Count of REPEATED elements from the root down to and including this
    /// leaf — 0 for an ordinary column, 1 for a single-level list (whether a
    /// bare `repeated` leaf or the standard 3-level `<list><element>` shape),
    /// 2+ for a list nested inside another list (unsupported — see
    /// `list_eligible`).
    max_rep_level: u32,
    /// Whether this leaf is decodable as "a list of scalars": `max_rep_level
    /// == 1` *and* every REPEATED ancestor group along the path had exactly
    /// one child. A REPEATED group with more than one child is a list of
    /// structs (each element has several fields, so its repetition and
    /// definition levels can't be read off one leaf's column chunk alone);
    /// `max_rep_level >= 2` is a list nested inside another list. Both are
    /// real Parquet shapes but a harder problem this reader doesn't attempt
    /// yet — see `plans/arbitrary-geoparquet.org` milestone 7.
    list_eligible: bool,
    /// The cumulative definition level *at* the REPEATED ancestor itself
    /// (whether that's a wrapping group in the 3-level shape, or this same
    /// leaf in the bare 2-level shape) — the threshold `group_list_values`
    /// uses to tell "list absent/empty for this row" (`def <=
    /// list_group_def_level - 1`) from "this occurrence is a real element
    /// slot" (`def >= list_group_def_level`). Only meaningful when
    /// `list_eligible`; `0` otherwise.
    list_group_def_level: u32,
    converted_type: Option<i32>,
    type_length: Option<i32>,
    scale: Option<i32>,
    is_geometry: bool,
    geometry_crs: Option<String>,
}

/// Reconstruct the schema tree from the file's flat pre-order `SchemaNode`
/// list (root first, each group immediately followed by its `num_children`
/// descendants) and flatten it back out into one [`LeafInfo`] per leaf, with
/// the definition level, repetition level, and list-eligibility accumulated
/// through every ancestor — not just the leaf's own repetition type, which
/// alone is only correct when nothing is nested. `nodes[0]` is the root
/// message and is consumed without contributing to any leaf's definition or
/// repetition level, matching the Parquet spec's algorithm for both.
fn flatten_schema(nodes: &[SchemaNode]) -> Vec<LeafInfo> {
    /// State threaded down through `walk`'s recursion — bundled into one
    /// struct (rather than four separate parameters) purely to keep the
    /// function's arg count reasonable; each field's meaning is documented
    /// where it's produced, below.
    #[derive(Clone, Copy)]
    struct State {
        def: u32,
        rep: u32,
        /// Whether every REPEATED ancestor *above* the current node had
        /// exactly one child — i.e. no ancestor was a "list of structs"
        /// branch point. A node's own repeatedness doesn't affect this
        /// value for its own leaf entry (a REPEATED leaf has no children to
        /// be unclean about); it only narrows what gets passed to *its*
        /// children.
        clean: bool,
        /// The def level of the nearest REPEATED ancestor found so far
        /// along this path (including the current node itself, once
        /// computed) — see `LeafInfo::list_group_def_level`. `None` until
        /// the first REPEATED node is seen.
        list_group_def: Option<u32>,
    }

    fn walk(nodes: &[SchemaNode], idx: &mut usize, path: &mut Vec<String>, parent: State, leaves: &mut Vec<LeafInfo>) {
        let Some(node) = nodes.get(*idx) else { return };
        *idx += 1;
        let is_repeated = node.repetition == repetition::REPEATED;
        let def = parent.def + u32::from(node.repetition != repetition::REQUIRED);
        let rep = parent.rep + u32::from(is_repeated);
        // Only relevant once we descend into this node's own children: if
        // *this* node is a REPEATED group, its children stay "clean" only
        // when it has exactly one of them.
        let clean_for_children = if is_repeated { parent.clean && node.num_children == 1 } else { parent.clean };
        let list_group_def = if is_repeated { Some(def) } else { parent.list_group_def };

        path.push(node.name.clone());
        if node.num_children <= 0 {
            leaves.push(LeafInfo {
                path: path.clone(),
                max_def_level: def,
                max_rep_level: rep,
                list_eligible: rep == 1 && parent.clean,
                list_group_def_level: list_group_def.unwrap_or(0),
                converted_type: node.converted_type,
                type_length: node.type_length,
                scale: node.scale,
                is_geometry: node.is_geometry,
                geometry_crs: node.geometry_crs.clone(),
            });
        } else {
            let child_state = State { def, rep, clean: clean_for_children, list_group_def };
            for _ in 0..node.num_children {
                walk(nodes, idx, path, child_state, leaves);
            }
        }
        path.pop();
    }

    let mut leaves = Vec::new();
    let Some(root) = nodes.first() else { return leaves };
    let mut idx = 1usize;
    let mut path = Vec::new();
    let root_state = State { def: 0, rep: 0, clean: true, list_group_def: None };
    for _ in 0..root.num_children {
        walk(nodes, &mut idx, &mut path, root_state, &mut leaves);
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
    let mut num_values = 0i64;
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
                5 => num_values = r.read_i64()?,
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
        num_values,
        data_page_offset,
        dictionary_page_offset,
        max_def_level: 1,          // set from the schema in parse_file_metadata
        max_rep_level: 0,          // set from the schema in parse_file_metadata
        list_group_def_level: 0,   // set from the schema in parse_file_metadata
        unsupported_repeated: false, // set from the schema in parse_file_metadata
        converted_type: None,      // set from the schema in parse_file_metadata
        type_length: None,         // set from the schema in parse_file_metadata
        scale: None,               // set from the schema in parse_file_metadata
        is_native_geometry: false, // set from the schema in parse_file_metadata
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

/// Every key of the GeoParquet `geo` metadata's `columns` object — per spec,
/// *each* entry there names a geometry column, not just whichever one
/// `primary_column` points at (a writer like DuckDB can emit more than one,
/// e.g. a table with two geometry-typed columns). Used to reject a second
/// geometry column with a clear error instead of misdecoding its WKB as text
/// (see the `geo_geometry_columns` check in `read_geoparquet`).
fn geo_geometry_columns(geo: &str) -> Vec<String> {
    let Ok(root) = json::parse(geo) else { return Vec::new() };
    let Some(columns) = root.get("columns").and_then(JsonValue::as_object) else {
        return Vec::new();
    };
    columns.iter().map(|(name, _)| name.clone()).collect()
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
    /// `INT96` legacy timestamps: `(nanos_of_day, julian_day)` per entry.
    Int96(Vec<(i64, i32)>),
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
    /// `DataPageHeaderV2`'s extra fields, present only for a `DATA_PAGE_V2` page.
    v2: Option<V2Header>,
}

/// The `DataPageHeaderV2` fields the V1 sub-header doesn't have. `num_nulls`/
/// `num_rows` aren't kept: `num_values` (nulls included) already gives the page's
/// row count for any non-repeated column, the only kind [`decode_column`] ever
/// reaches — a genuinely repeated column is rejected earlier, at the
/// `unsupported_repeated` check in `read_geoparquet`, before a page is ever read.
struct V2Header {
    /// Byte length of the (unprefixed, unlike V1) RLE definition-level stream.
    def_levels_byte_length: usize,
    /// Byte length of the repetition-level stream — nonzero only for a
    /// list-eligible column (`max_rep_level > 0`); checked against the
    /// schema's own `max_rep_level` in [`decode_data_page_v2_body`] rather
    /// than assumed.
    rep_levels_byte_length: usize,
    /// Whether the *values* section (not the levels, which are never
    /// compressed) is compressed with the column's codec. Thrift-optional,
    /// defaults to `true` when absent.
    is_compressed: bool,
}

/// One page's parsed header plus its (still compressed) body bytes, with
/// `pos` already advanced past it — the bookkeeping [`decode_column`] and
/// [`decode_list_column`]'s page loops share.
struct PageSlice<'a> {
    header: PageHeader,
    comp: &'a [u8],
}

fn next_page<'a>(file: &'a [u8], pos: &mut usize) -> Result<PageSlice<'a>> {
    let after = file
        .get(*pos..)
        .ok_or_else(|| Error::Parquet("page offset out of range".into()))?;
    let mut r = CompactReader::new(after);
    let header = parse_page_header(&mut r)?;
    let body_start = *pos + r.position();
    let comp = file
        .get(body_start..body_start + header.compressed_size)
        .ok_or_else(|| Error::Parquet("page body out of range".into()))?;
    *pos = body_start + header.compressed_size;
    Ok(PageSlice { header, comp })
}

/// Decode one column chunk (all its pages, across the whole chunk) into
/// `rg_rows` aligned values. A geometry column keeps raw WKB bytes; every other
/// column decodes straight to per-row JSON. A list-eligible column
/// (`max_rep_level > 0`) delegates to [`decode_list_column`] instead — its
/// occurrence count doesn't equal `rg_rows`, so it needs its own stopping
/// bound and a row-grouping pass the ordinary path has no use for.
fn decode_column(
    file: &[u8],
    col: &ColumnMeta,
    rg_rows: usize,
    is_geometry: bool,
) -> Result<ColumnSink> {
    if col.max_rep_level > 0 {
        if is_geometry {
            return Err(Error::Parquet(
                "geometry column can't be list-valued (repeated)".into(),
            ));
        }
        return Ok(ColumnSink::Json {
            out: decode_list_column(file, col)?,
            converted: col.converted_type,
        });
    }

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
        let PageSlice { header: ph, comp } = next_page(file, &mut pos)?;
        match ph.page_type {
            t if t == page::DICTIONARY_PAGE => {
                let body = decompress(col.codec, comp, ph.uncompressed_size)?;
                dict = Some(decode_dictionary(&body, col, ph.num_values as usize)?);
            }
            t if t == page::DATA_PAGE => {
                let body = decompress(col.codec, comp, ph.uncompressed_size)?;
                let page_rows = ph.num_values as usize;
                decode_data_page(&body, col, ph.encoding, page_rows, dict.as_ref(), &mut out)?;
                rows_done += page_rows;
            }
            t if t == page::DATA_PAGE_V2 => {
                let v2 = ph
                    .v2
                    .as_ref()
                    .ok_or_else(|| Error::Parquet("DATA_PAGE_V2 missing its sub-header".into()))?;
                let body = decode_data_page_v2_body(
                    comp,
                    ph.uncompressed_size,
                    col.codec,
                    v2,
                    col.max_rep_level,
                    col.max_def_level,
                )?;
                let page_rows = ph.num_values as usize;
                decode_data_page(&body, col, ph.encoding, page_rows, dict.as_ref(), &mut out)?;
                rows_done += page_rows;
            }
            other => return Err(Error::Parquet(format!("unsupported page type {other}"))),
        }
    }
    Ok(out)
}

/// Decode a list-eligible (`max_rep_level > 0`) column chunk into one
/// [`JsonValue`] per row (`Array`, possibly empty, or `Null`).
///
/// Unlike [`decode_column`]'s ordinary path, this can't stop once it's seen
/// `rg_rows` row-start markers (`rep_level == 0`): a single row's elements
/// aren't guaranteed to stay within one physical page, so `col.num_values`
/// (`ColumnMetaData.num_values`, the chunk's total *occurrence* count) is
/// the only stopping bound that's correct regardless of where a writer chose
/// to split pages — reading until every occurrence is consumed, rather than
/// until every row has *started*, means a row split across a page boundary
/// still gets all its elements. Repetition levels, definition levels, and
/// decoded per-occurrence values (via [`decode_values`], the same dispatch
/// the ordinary path uses) accumulate flat across every page in the chunk,
/// and are only grouped into rows once, at the end, by [`group_list_values`].
fn decode_list_column(file: &[u8], col: &ColumnMeta) -> Result<Vec<JsonValue>> {
    let start = col
        .dictionary_page_offset
        .filter(|&o| o >= 0)
        .unwrap_or(col.data_page_offset) as usize;
    let total_occurrences = col.num_values.max(0) as usize;

    let mut dict: Option<Dict> = None;
    let mut pos = start;
    let mut occurrences_done = 0usize;
    let mut rep_levels: Vec<u64> = Vec::with_capacity(total_occurrences);
    let mut def_levels: Vec<u64> = Vec::with_capacity(total_occurrences);
    let mut flat_values: Vec<JsonValue> = Vec::with_capacity(total_occurrences);

    while occurrences_done < total_occurrences {
        let PageSlice { header: ph, comp } = next_page(file, &mut pos)?;
        match ph.page_type {
            t if t == page::DICTIONARY_PAGE => {
                let body = decompress(col.codec, comp, ph.uncompressed_size)?;
                dict = Some(decode_dictionary(&body, col, ph.num_values as usize)?);
            }
            t if t == page::DATA_PAGE => {
                let body = decompress(col.codec, comp, ph.uncompressed_size)?;
                let n = ph.num_values as usize;
                let (rep, def, values) = split_list_levels(&body, col.max_rep_level, col.max_def_level, n)?;
                decode_list_page_values(col, ph.encoding, values, &def, dict.as_ref(), &mut flat_values)?;
                occurrences_done += n;
                rep_levels.extend(rep);
                def_levels.extend(def);
            }
            t if t == page::DATA_PAGE_V2 => {
                let v2 = ph
                    .v2
                    .as_ref()
                    .ok_or_else(|| Error::Parquet("DATA_PAGE_V2 missing its sub-header".into()))?;
                let body = decode_data_page_v2_body(
                    comp,
                    ph.uncompressed_size,
                    col.codec,
                    v2,
                    col.max_rep_level,
                    col.max_def_level,
                )?;
                let n = ph.num_values as usize;
                let (rep, def, values) = split_list_levels(&body, col.max_rep_level, col.max_def_level, n)?;
                decode_list_page_values(col, ph.encoding, values, &def, dict.as_ref(), &mut flat_values)?;
                occurrences_done += n;
                rep_levels.extend(rep);
                def_levels.extend(def);
            }
            other => return Err(Error::Parquet(format!("unsupported page type {other}"))),
        }
    }
    Ok(group_list_values(&rep_levels, &def_levels, flat_values, col.list_group_def_level))
}

/// Decode one list data-page's values (given its already-split raw
/// definition levels) into `flat_values`, reusing [`decode_values`] — the
/// same PLAIN/dictionary dispatch [`decode_data_page`] uses, just fed a
/// [`Present`] mask built directly from the raw def levels rather than
/// [`split_definition_levels`]'s.
fn decode_list_page_values(
    col: &ColumnMeta,
    page_encoding: i32,
    values: &[u8],
    def_levels: &[u64],
    dict: Option<&Dict>,
    flat_values: &mut Vec<JsonValue>,
) -> Result<()> {
    let present = Present::Mask(def_levels.iter().map(|&d| d == col.max_def_level as u64).collect());
    let mut page_out = ColumnSink::Json {
        out: Vec::with_capacity(def_levels.len()),
        converted: col.converted_type,
    };
    decode_values(&mut page_out, col, page_encoding, values, &present, dict)?;
    match page_out {
        ColumnSink::Json { out, .. } => flat_values.extend(out),
        ColumnSink::Wkb(_) => unreachable!("decode_list_page_values only ever constructs ColumnSink::Json"),
    }
    Ok(())
}

/// Reassemble a `DATA_PAGE_V2` page into the byte shape
/// [`split_definition_levels`]/[`split_list_levels`] already know how to
/// parse (V1's `[u32 rle_len][RLE levels][values]`, repetition section first
/// when present), so the rest of the pipeline needs no V2-specific code.
///
/// V2's on-disk layout differs from V1 in two ways this bridges: neither
/// level stream has a 4-byte length prefix (their lengths are already in the
/// header, as `rep_levels_byte_length`/`def_levels_byte_length`) — prefixes
/// are synthesized here — and only the *values* portion is ever compressed,
/// never the levels, so decompression applies to `comp[levels_len..]` alone
/// rather than the whole page body.
fn decode_data_page_v2_body(
    comp: &[u8],
    uncompressed_size: usize,
    codec: i32,
    v2: &V2Header,
    max_rep_level: u32,
    max_def_level: u32,
) -> Result<Vec<u8>> {
    if max_rep_level == 0 && v2.rep_levels_byte_length != 0 {
        return Err(Error::Parquet(
            "DATA_PAGE_V2 has repetition levels but the schema says this column isn't repeated".into(),
        ));
    }
    let rep_len = v2.rep_levels_byte_length;
    let def_len = v2.def_levels_byte_length;
    let levels_len = rep_len + def_len;
    let levels = comp
        .get(..levels_len)
        .ok_or_else(|| Error::Parquet("DATA_PAGE_V2 levels out of range".into()))?;
    let (rep_bytes, def_bytes) = levels.split_at(rep_len);
    let values_comp = &comp[levels_len..];
    let values_uncompressed_len = uncompressed_size
        .checked_sub(levels_len)
        .ok_or_else(|| Error::Parquet("DATA_PAGE_V2 uncompressed_size shorter than its levels".into()))?;
    let values = if v2.is_compressed {
        decompress(codec, values_comp, values_uncompressed_len)?
    } else {
        if values_comp.len() != values_uncompressed_len {
            return Err(Error::Parquet("DATA_PAGE_V2 value size mismatch".into()));
        }
        values_comp.to_vec()
    };

    // REQUIRED, non-repeated column (max_def_level == 0): no level sections
    // at all — a REPEATED node always contributes to the definition level,
    // so max_def_level == 0 implies max_rep_level == 0 too, meaning there's
    // never a rep section to lose here. V1's `split_definition_levels`
    // expects the body to be bare values in this case, so match that rather
    // than wrapping a length prefix around zero level bytes.
    if max_def_level == 0 {
        return Ok(values);
    }
    let mut body = Vec::with_capacity(8 + levels.len() + values.len());
    if max_rep_level > 0 {
        body.extend_from_slice(&(rep_len as u32).to_le_bytes());
        body.extend_from_slice(rep_bytes);
    }
    body.extend_from_slice(&(def_len as u32).to_le_bytes());
    body.extend_from_slice(def_bytes);
    body.extend_from_slice(&values);
    Ok(body)
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

fn decode_dictionary(body: &[u8], col: &ColumnMeta, count: usize) -> Result<Dict> {
    Ok(match col.physical {
        p if p == ptype::INT32 => Dict::Int(plain_i32(body, count)?),
        p if p == ptype::INT64 => Dict::Int(plain_i64(body, count)?),
        p if p == ptype::INT96 => Dict::Int96(plain_i96(body, count)?),
        p if p == ptype::FLOAT => Dict::Double(plain_f32(body, count)?),
        p if p == ptype::DOUBLE => Dict::Double(plain_f64(body, count)?),
        p if p == ptype::BYTE_ARRAY => Dict::Bytes(plain_byte_arrays(body, count)?),
        p if p == ptype::FIXED_LEN_BYTE_ARRAY => {
            let width = col.type_length.ok_or_else(|| {
                Error::Parquet("FIXED_LEN_BYTE_ARRAY column missing type_length".into())
            })?;
            Dict::Bytes(plain_fixed_len_byte_arrays(body, count, width as usize)?)
        }
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
    decode_values(out, col, page_encoding, values, &present, dict)
}

/// Decode a page's already-`present`-masked value bytes into `out`, dispatching
/// on encoding. Shared by the ordinary (one-value-per-row) path
/// ([`decode_data_page`]) and the list-column path ([`decode_list_column`]),
/// which differ only in what `present` means (present *row* vs. present
/// *occurrence*) — the value decoding itself doesn't care.
fn decode_values(
    out: &mut ColumnSink,
    col: &ColumnMeta,
    page_encoding: i32,
    values: &[u8],
    present: &Present,
    dict: Option<&Dict>,
) -> Result<()> {
    let n_present = present.count();
    match page_encoding {
        e if e == encoding::PLAIN => decode_plain(out, col, values, present, n_present),
        e if e == encoding::PLAIN_DICTIONARY || e == encoding::RLE_DICTIONARY => {
            let dict = dict.ok_or_else(|| {
                Error::Parquet("dictionary-encoded data page without a dictionary".into())
            })?;
            decode_dict_indices(out, col, dict, values, present, n_present)
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

/// Parse a DATA_PAGE (or V2-bridged, see [`decode_data_page_v2_body`]) body
/// for a list-eligible (`max_rep_level > 0`) column: `[u32 rep_len][RLE rep
/// levels]?[u32 def_len][RLE def levels][values]`, rep section first when
/// `max_rep_level > 0` (always true for the columns that call this).
///
/// Unlike [`split_definition_levels`], this returns the *raw* per-occurrence
/// level values rather than collapsing them into a [`Present`] mask —
/// grouping list elements into rows needs to distinguish "list absent for
/// this row" from "list present but empty" from "an element is null within a
/// present list", three states [`Present`] alone can't tell apart (see
/// [`group_list_values`]).
fn split_list_levels(
    body: &[u8],
    max_rep_level: u32,
    max_def_level: u32,
    occurrences: usize,
) -> Result<(Vec<u64>, Vec<u64>, &[u8])> {
    let mut pos = 0usize;
    let rep_levels = if max_rep_level > 0 {
        read_prefixed_levels(body, &mut pos, bits_needed(max_rep_level), occurrences)?
    } else {
        vec![0u64; occurrences]
    };
    let def_levels = if max_def_level > 0 {
        read_prefixed_levels(body, &mut pos, bits_needed(max_def_level), occurrences)?
    } else {
        vec![0u64; occurrences]
    };
    let values = body.get(pos..).ok_or_else(|| Error::Parquet("page body too short".into()))?;
    Ok((rep_levels, def_levels, values))
}

/// Read one `[u32 len][RLE/bit-pack levels]` section, decode it, and advance
/// `pos` past it. Shared by `split_list_levels`'s repetition and definition
/// sections (same on-disk shape, different bit width).
fn read_prefixed_levels(body: &[u8], pos: &mut usize, bit_width: u32, count: usize) -> Result<Vec<u64>> {
    let len = body
        .get(*pos..*pos + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
        .ok_or_else(|| Error::Parquet("page body too short".into()))?;
    let start = *pos + 4;
    let levels = body
        .get(start..start + len)
        .ok_or_else(|| Error::Parquet("level section out of range".into()))?;
    *pos = start + len;
    decode_levels(levels, bit_width, count)
}

/// Group a column chunk's flat per-occurrence repetition levels, definition
/// levels, and already-decoded values (one [`JsonValue`] per occurrence —
/// `Null` wherever `def < max_def_level`, the real value otherwise, exactly
/// what [`decode_values`] already produces) into one [`JsonValue`] per row:
/// `Null` for an absent list, `Array` (possibly empty) for a present one.
///
/// `rep_levels[i] == 0` starts a new row (Parquet's convention: every row
/// emits at least one occurrence, the first at repetition level 0); anything
/// higher continues the previous row's list. Within a row, an occurrence's
/// definition level relative to `list_group_def_level` (the def level
/// achieved by the REPEATED ancestor itself — see its doc comment) says what
/// that occurrence represents:
/// - `def < list_group_def_level - 1`: some ancestor *above* the repeated
///   field is absent — the row's list itself is `Null` (only reachable when
///   the repeated field has an OPTIONAL ancestor above it, e.g. the 3-level
///   `optional group tags (LIST) { repeated ... }` shape's outer `tags`).
/// - `def == list_group_def_level - 1`: the repeated field is present but
///   has zero occurrences this row — an empty (not null) list.
/// - `def >= list_group_def_level`: a real element slot — push `values[i]`,
///   which is already `Null` (the element itself is null) or the decoded
///   value, aligned by [`decode_values`]/[`align`] the same way every other
///   column's present/absent values already are.
fn group_list_values(rep_levels: &[u64], def_levels: &[u64], values: Vec<JsonValue>, list_group_def_level: u32) -> Vec<JsonValue> {
    let empty_threshold = u64::from(list_group_def_level.saturating_sub(1));
    let mut rows = Vec::new();
    let mut current: Vec<JsonValue> = Vec::new();
    let mut row_is_null = false;
    let mut started = false;
    for ((&rep, &def), value) in rep_levels.iter().zip(def_levels.iter()).zip(values) {
        if rep == 0 {
            if started {
                rows.push(if row_is_null { JsonValue::Null } else { JsonValue::Array(std::mem::take(&mut current)) });
            }
            started = true;
            row_is_null = false;
        }
        if def < empty_threshold {
            row_is_null = true;
        } else if def > empty_threshold {
            current.push(value);
        }
        // def == empty_threshold: present-but-empty list — nothing to push.
    }
    if started {
        rows.push(if row_is_null { JsonValue::Null } else { JsonValue::Array(current) });
    }
    rows
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
    col: &ColumnMeta,
    values: &[u8],
    present: &Present,
    n: usize,
) -> Result<()> {
    let physical = col.physical;
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
            p if p == ptype::INT32 && *converted == Some(converted::DECIMAL) => {
                push_json_decimal(out, present, plain_i32(values, n)?, col.scale)
            }
            p if p == ptype::INT32 => emit_ints(out, *converted, present, plain_i32(values, n)?),
            p if p == ptype::INT64 && *converted == Some(converted::DECIMAL) => {
                push_json_decimal(out, present, plain_i64(values, n)?, col.scale)
            }
            p if p == ptype::INT64 => emit_ints(out, *converted, present, plain_i64(values, n)?),
            p if p == ptype::INT96 => {
                push_json(out, present, plain_i96(values, n)?, |(nanos, jd)| {
                    JsonValue::String(format_int96(nanos, jd))
                })
            }
            p if p == ptype::FLOAT => push_json(out, present, plain_f32(values, n)?, json_number),
            p if p == ptype::DOUBLE => push_json(out, present, plain_f64(values, n)?, json_number),
            p if p == ptype::BYTE_ARRAY && *converted == Some(converted::DECIMAL) => {
                push_json_decimal_bytes(out, present, plain_byte_arrays(values, n)?, col.scale)?
            }
            p if p == ptype::BYTE_ARRAY && *converted == Some(converted::JSON) => {
                push_json_embedded(out, present, plain_byte_arrays(values, n)?)?
            }
            p if p == ptype::BYTE_ARRAY => push_json_strings(out, present, plain_byte_arrays(values, n)?)?,
            p if p == ptype::FIXED_LEN_BYTE_ARRAY => {
                let width = col.type_length.ok_or_else(|| {
                    Error::Parquet("FIXED_LEN_BYTE_ARRAY column missing type_length".into())
                })? as usize;
                let raw = plain_fixed_len_byte_arrays(values, n, width)?;
                if *converted == Some(converted::DECIMAL) {
                    push_json_decimal_bytes(out, present, raw, col.scale)?
                } else {
                    push_json(out, present, raw, |b| JsonValue::String(to_hex(&b)))
                }
            }
            other => return Err(Error::Parquet(format!("unsupported physical type {other}"))),
        },
    }
    Ok(())
}

/// Decode a dictionary-index data page: `[1 byte bit width][RLE/bit-pack
/// indices]`, mapped through `dict` and emitted straight to the sink.
fn decode_dict_indices(
    out: &mut ColumnSink,
    col: &ColumnMeta,
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
    let is_decimal = col.converted_type == Some(converted::DECIMAL);

    match out {
        ColumnSink::Wkb(v) => match dict {
            Dict::Bytes(d) => v.extend(align(present, map_indices(&indices, d, |x| x.clone())?)),
            _ => return Err(Error::Parquet("geometry column is not BYTE_ARRAY".into())),
        },
        ColumnSink::Json { out, converted } => match dict {
            Dict::Int(d) if is_decimal => {
                push_json_decimal(out, present, map_indices(&indices, d, |x| *x)?, col.scale)
            }
            Dict::Int(d) => emit_ints(out, *converted, present, map_indices(&indices, d, |x| *x)?),
            Dict::Double(d) => {
                push_json(out, present, map_indices(&indices, d, |x| *x)?, json_number)
            }
            Dict::Int96(d) => push_json(out, present, map_indices(&indices, d, |x| *x)?, |(nanos, jd)| {
                JsonValue::String(format_int96(nanos, jd))
            }),
            Dict::Bytes(d) if is_decimal => {
                push_json_decimal_bytes(out, present, map_indices(&indices, d, |x| x.clone())?, col.scale)?
            }
            Dict::Bytes(d) if col.converted_type == Some(converted::JSON) => {
                push_json_embedded(out, present, map_indices(&indices, d, |x| x.clone())?)?
            }
            // FIXED_LEN_BYTE_ARRAY with no logical meaning of its own: hex text.
            Dict::Bytes(d) if col.physical == ptype::FIXED_LEN_BYTE_ARRAY => {
                push_json(out, present, map_indices(&indices, d, |x| x.clone())?, |b| {
                    JsonValue::String(to_hex(&b))
                })
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
    let mut v2 = None;

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
                8 => {
                    let (nv, enc, header) = parse_data_page_header_v2(r)?;
                    num_values = nv;
                    encoding = enc;
                    v2 = Some(header);
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
        v2,
    })
}

/// Parse a `DataPageHeaderV2` struct (`PageHeader` field 8): `num_values`(1),
/// `num_nulls`(2, skipped — see [`V2Header`]'s doc), `num_rows`(3, skipped),
/// `encoding`(4), `definition_levels_byte_length`(5),
/// `repetition_levels_byte_length`(6), `is_compressed`(7, thrift-optional
/// bool — the compact protocol encodes a bool's value *in the field header's
/// type nibble* (`BOOL_TRUE`/`BOOL_FALSE`), not as a following byte, so it's
/// read directly off `ty` rather than via a `read_*` call).
fn parse_data_page_header_v2(r: &mut CompactReader) -> Result<(i32, i32, V2Header)> {
    let mut num_values = 0i32;
    let mut encoding = -1i32;
    let mut def_len = -1i32;
    let mut rep_len = -1i32;
    let mut is_compressed = true; // thrift default when the field is absent
    r.struct_begin();
    loop {
        match r.read_field()? {
            Field::Stop => break,
            Field::Begin { id, ty } => match id {
                1 => num_values = r.read_i32()?,
                4 => encoding = r.read_i32()?,
                5 => def_len = r.read_i32()?,
                6 => rep_len = r.read_i32()?,
                7 => is_compressed = ty == ct::BOOL_TRUE,
                _ => r.skip(ty)?,
            },
        }
    }
    r.struct_end();

    if def_len < 0 || rep_len < 0 {
        return Err(Error::Parquet("incomplete DataPageHeaderV2".into()));
    }
    Ok((
        num_values,
        encoding,
        V2Header {
            def_levels_byte_length: def_len as usize,
            rep_levels_byte_length: rep_len as usize,
            is_compressed,
        },
    ))
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

/// `INT96`: 12 bytes per value, `[8-byte LE nanos-of-day][4-byte LE Julian day]`
/// — the legacy Impala/Hive on-disk timestamp encoding. No `converted_type`
/// or `logicalType` marks it; the physical type alone means "this is a
/// timestamp" (see [`format_int96`]).
fn plain_i96(data: &[u8], count: usize) -> Result<Vec<(i64, i32)>> {
    let slots = fixed_slots(data, count, 12)?;
    Ok(slots
        .map(|s| {
            let nanos = i64::from_le_bytes(s[0..8].try_into().unwrap());
            let julian_day = i32::from_le_bytes(s[8..12].try_into().unwrap());
            (nanos, julian_day)
        })
        .collect())
}

/// `FIXED_LEN_BYTE_ARRAY`: `count` back-to-back `width`-byte slices, no
/// per-value length prefix (unlike `BYTE_ARRAY`) since the schema's
/// `type_length` already fixes it.
fn plain_fixed_len_byte_arrays(data: &[u8], count: usize, width: usize) -> Result<Vec<Vec<u8>>> {
    Ok(fixed_slots(data, count, width)?.map(|s| s.to_vec()).collect())
}

/// Render an `INT96` value as ISO 8601, at nanosecond precision. The Julian
/// day is converted to a Unix day count (`JDN 2440588` = 1970-01-01) and
/// formatted through the same civil-calendar math as [`format_timestamp`];
/// day and time-of-day are kept separate throughout (rather than summed into
/// one nanoseconds-since-epoch `i64`) so dates well outside `i64` nanosecond
/// range (~year 1677-2262) still format correctly.
fn format_int96(nanos_of_day: i64, julian_day: i32) -> String {
    const UNIX_EPOCH_JULIAN_DAY: i64 = 2_440_588;
    let unix_days = julian_day as i64 - UNIX_EPOCH_JULIAN_DAY;
    let secs_of_day = nanos_of_day.div_euclid(1_000_000_000);
    let nanos = nanos_of_day.rem_euclid(1_000_000_000);
    let (y, m, d) = civil_from_days(unix_days);
    let (hh, mm, ss) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    let mut s = format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}");
    if nanos != 0 {
        s.push_str(&format!(".{nanos:09}"));
    }
    s
}

/// Big-endian two's-complement bytes (as `DECIMAL` on `FIXED_LEN_BYTE_ARRAY`
/// or `BYTE_ARRAY` stores its unscaled value) to `i128`. `i128` bounds a
/// 16-byte value, which covers every precision the Parquet spec's own table
/// maps to a byte width (`precision <= 38`); a longer value is rejected
/// rather than silently truncated.
fn be_bytes_to_i128(bytes: &[u8]) -> Result<i128> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err(Error::Parquet(format!(
            "decimal value is {} bytes, expected 1-16",
            bytes.len()
        )));
    }
    let sign_extend = if bytes[0] & 0x80 != 0 { 0xFFu8 } else { 0u8 };
    let mut buf = [sign_extend; 16];
    buf[16 - bytes.len()..].copy_from_slice(bytes);
    Ok(i128::from_be_bytes(buf))
}

/// Render a `DECIMAL`'s unscaled integer + scale as an exact base-10 string
/// (never a float — `f64` can't hold 38 significant decimal digits without
/// loss). `scale` is `SchemaElement.scale`; a missing scale (malformed file)
/// falls back to 0, i.e. the raw unscaled integer.
fn format_decimal(unscaled: i128, scale: Option<i32>) -> String {
    let scale = scale.unwrap_or(0);
    if scale <= 0 {
        format!("{unscaled}{}", "0".repeat((-scale) as usize))
    } else {
        let neg = unscaled < 0;
        let digits = unscaled.unsigned_abs().to_string();
        let scale = scale as usize;
        let padded = if digits.len() <= scale {
            format!("{}{digits}", "0".repeat(scale - digits.len() + 1))
        } else {
            digits
        };
        let (int_part, frac_part) = padded.split_at(padded.len() - scale);
        format!("{}{int_part}.{frac_part}", if neg { "-" } else { "" })
    }
}

/// Lowercase-hex text for a byte string with no more specific logical
/// meaning — the fallback rendering for a plain `FIXED_LEN_BYTE_ARRAY`
/// column (not `DECIMAL`), since Geosetta's IR has no raw-bytes value type.
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// [`push_json`] for `DECIMAL`-on-integer columns (`INT32`/`INT64`): always a
/// string, never a JSON number, so the scale is never lost to float rounding.
fn push_json_decimal(out: &mut Vec<JsonValue>, present: &Present, values: Vec<i64>, scale: Option<i32>) {
    push_json(out, present, values, |v| JsonValue::String(format_decimal(v as i128, scale)));
}

/// [`push_json_decimal`] for `DECIMAL`-on-bytes columns (`FIXED_LEN_BYTE_ARRAY`
/// or `BYTE_ARRAY`): each value is the big-endian unscaled integer.
fn push_json_decimal_bytes(
    out: &mut Vec<JsonValue>,
    present: &Present,
    values: Vec<Vec<u8>>,
    scale: Option<i32>,
) -> Result<()> {
    let decoded: Vec<i128> = values.iter().map(|b| be_bytes_to_i128(b)).collect::<Result<_>>()?;
    push_json(out, present, decoded, |v| JsonValue::String(format_decimal(v, scale)));
    Ok(())
}

/// A `BYTE_ARRAY` column annotated `JSON` (`ConvertedType`/`LogicalType`
/// `JSON`) already holds JSON text — embed it as a real value instead of
/// re-stringifying it into a JSON string of a string. Falls back to a plain
/// string on a parse failure (a technically-invalid file) rather than erroring.
fn push_json_embedded(out: &mut Vec<JsonValue>, present: &Present, values: Vec<Vec<u8>>) -> Result<()> {
    let decoded: Vec<JsonValue> = values
        .into_iter()
        .map(|b| match String::from_utf8(b) {
            Ok(s) => json::parse(&s).unwrap_or(JsonValue::String(s)),
            Err(e) => JsonValue::String(String::from_utf8_lossy(e.as_bytes()).into_owned()),
        })
        .collect();
    push_json(out, present, decoded, |v| v);
    Ok(())
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
    fn formats_int96_timestamps() {
        // Julian day 2440588 = 1970-01-01 (the Unix epoch); nanos_of_day 0 = midnight.
        assert_eq!(format_int96(0, 2_440_588), "1970-01-01T00:00:00");
        // Matches the pyarrow fixture's row 1 (1_600_000_001_000_000_001 ns since
        // epoch = day 18518, 44801s + 1ns into the day): 2020-09-13T12:26:41 +1ns.
        assert_eq!(
            format_int96(44_801 * 1_000_000_000 + 1, 2_440_588 + 18_518),
            "2020-09-13T12:26:41.000000001"
        );
        // Before the epoch stays correct through civil_from_days' proleptic math.
        assert_eq!(format_int96(0, 2_440_587), "1969-12-31T00:00:00");
    }

    #[test]
    fn formats_decimals() {
        assert_eq!(format_decimal(12345, Some(2)), "123.45");
        assert_eq!(format_decimal(-12345, Some(2)), "-123.45");
        assert_eq!(format_decimal(5, Some(2)), "0.05"); // pads leading zeros
        assert_eq!(format_decimal(-5, Some(2)), "-0.05");
        assert_eq!(format_decimal(0, Some(2)), "0.00");
        assert_eq!(format_decimal(123, Some(0)), "123"); // scale 0: no point
        assert_eq!(format_decimal(123, None), "123"); // missing scale defaults to 0
        assert_eq!(format_decimal(123, Some(-2)), "12300"); // negative scale: trailing zeros
    }

    #[test]
    fn be_bytes_to_i128_round_trips_two_complement() {
        assert_eq!(be_bytes_to_i128(&[0x00, 0x01]).unwrap(), 1);
        assert_eq!(be_bytes_to_i128(&[0xFF, 0xFF]).unwrap(), -1);
        assert_eq!(be_bytes_to_i128(&[0x80, 0x00]).unwrap(), -32768);
        assert_eq!(be_bytes_to_i128(&[0x01]).unwrap(), 1);
        assert!(be_bytes_to_i128(&[]).is_err());
        assert!(be_bytes_to_i128(&[0u8; 17]).is_err());
    }

    #[test]
    fn hex_renders_lowercase_fixed_width() {
        assert_eq!(to_hex(&[0x00, 0x01, 0xAB, 0xFF]), "0001abff");
        assert_eq!(to_hex(&[]), "");
    }

    /// A minimal `ColumnMeta` for tests that exercise page/dictionary decoding
    /// directly and don't go through `parse_column_meta`/schema attachment.
    fn test_col(physical: i32) -> ColumnMeta {
        ColumnMeta {
            name: "col".into(),
            path: vec!["col".into()],
            physical,
            codec: codec::UNCOMPRESSED,
            num_values: 0,
            data_page_offset: 0,
            dictionary_page_offset: None,
            max_def_level: 1,
            max_rep_level: 0,
            list_group_def_level: 0,
            unsupported_repeated: false,
            converted_type: None,
            type_length: None,
            scale: None,
            is_native_geometry: false,
        }
    }

    #[test]
    fn decode_plain_routes_decimal_int32_through_scale() {
        let mut col = test_col(ptype::INT32);
        col.converted_type = Some(converted::DECIMAL);
        col.scale = Some(2);
        let mut out = ColumnSink::Json { out: Vec::new(), converted: col.converted_type };
        let values = 12345i32.to_le_bytes();
        decode_plain(&mut out, &col, &values, &Present::All(1), 1).unwrap();
        match out {
            ColumnSink::Json { out, .. } => assert_eq!(out, vec![JsonValue::String("123.45".into())]),
            _ => unreachable!(),
        }
    }

    #[test]
    fn decode_plain_routes_plain_fixed_len_byte_array_to_hex() {
        let mut col = test_col(ptype::FIXED_LEN_BYTE_ARRAY);
        col.type_length = Some(2);
        let mut out = ColumnSink::Json { out: Vec::new(), converted: None };
        let values = [0x00u8, 0x01, 0xAB, 0xFF]; // two 2-byte values
        decode_plain(&mut out, &col, &values, &Present::All(2), 2).unwrap();
        match out {
            ColumnSink::Json { out, .. } => assert_eq!(
                out,
                vec![JsonValue::String("0001".into()), JsonValue::String("abff".into())]
            ),
            _ => unreachable!(),
        }
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
        let col = test_col(ptype::BYTE_ARRAY);
        decode_dict_indices(&mut out, &col, &dict, &values, &present, 4).unwrap();
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
        SchemaNode {
            name: name.into(),
            repetition: rep,
            num_children,
            converted_type: None,
            type_length: None,
            scale: None,
            is_geometry: false,
            geometry_crs: None,
        }
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
        assert!(leaves.iter().all(|l| l.max_rep_level == 0));
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
            assert_eq!(l.max_rep_level, 0);
        }
    }

    #[test]
    fn a_clean_single_level_list_of_scalars_is_list_eligible() {
        // root -> tags (REPEATED group with exactly one child) -> item — the
        // standard 3-level list-of-scalars encoding (e.g. Parquet's `LIST`
        // logical type over a `repeated group list { optional binary
        // element }`). Decodable: milestone 7's target shape.
        let schema = vec![
            node("schema", repetition::REQUIRED, 1),
            node("tags", repetition::REPEATED, 1),
            node("item", repetition::OPTIONAL, 0),
        ];
        let leaves = flatten_schema(&schema);
        assert_eq!(leaves.len(), 1);
        assert!(leaves[0].list_eligible);
        assert_eq!(leaves[0].max_rep_level, 1);
        // A REPEATED ancestor also counts toward the definition level, same
        // as OPTIONAL — tags(REPEATED)=1 + item(OPTIONAL)=1.
        assert_eq!(leaves[0].max_def_level, 2);
        // The REPEATED "tags" node's own def level is 1 — an occurrence with
        // def==0 means the list is absent/empty for that row; def==1 means a
        // present element (null or, at def==2, the max, a real value).
        assert_eq!(leaves[0].list_group_def_level, 1);
    }

    #[test]
    fn a_bare_repeated_leaf_is_also_list_eligible() {
        // root -> tags (REPEATED leaf directly, the older 2-level encoding
        // with no wrapping <list><element> group).
        let schema = vec![node("schema", repetition::REQUIRED, 1), node("tags", repetition::REPEATED, 0)];
        let leaves = flatten_schema(&schema);
        assert_eq!(leaves.len(), 1);
        assert!(leaves[0].list_eligible);
        assert_eq!(leaves[0].max_rep_level, 1);
        // The repeated leaf's own def level (1) coincides with max_def_level
        // (1) here — a bare repeated leaf has no separate "element is null
        // within a present list" state, only present-with-N-elements or
        // present-with-zero.
        assert_eq!(leaves[0].list_group_def_level, 1);
        assert_eq!(leaves[0].max_def_level, 1);
    }

    #[test]
    fn a_repeated_group_with_more_than_one_child_is_not_list_eligible() {
        // root -> tags (REPEATED group with TWO children) -> [a, b] — a list
        // of structs, not a list of scalars: each element has several
        // fields, so one leaf's column chunk alone can't decode a row's
        // elements. Not supported yet.
        let schema = vec![
            node("schema", repetition::REQUIRED, 1),
            node("tags", repetition::REPEATED, 2),
            node("a", repetition::OPTIONAL, 0),
            node("b", repetition::OPTIONAL, 0),
        ];
        let leaves = flatten_schema(&schema);
        assert_eq!(leaves.len(), 2);
        assert!(leaves.iter().all(|l| !l.list_eligible));
        assert!(leaves.iter().all(|l| l.max_rep_level == 1));
    }

    #[test]
    fn a_list_nested_inside_another_list_is_not_list_eligible() {
        // root -> outer (REPEATED group, one child) -> inner (REPEATED
        // group, one child) -> item — a list of lists. Two REPEATED
        // ancestors, not supported yet.
        let schema = vec![
            node("schema", repetition::REQUIRED, 1),
            node("outer", repetition::REPEATED, 1),
            node("inner", repetition::REPEATED, 1),
            node("item", repetition::OPTIONAL, 0),
        ];
        let leaves = flatten_schema(&schema);
        assert_eq!(leaves.len(), 1);
        assert!(!leaves[0].list_eligible);
        assert_eq!(leaves[0].max_rep_level, 2);
    }

    // --- list-column decode: repetition levels + row grouping --------------

    #[test]
    fn split_list_levels_parses_rep_then_def_then_values() {
        // 3 occurrences. rep = [0, 1, 0] (row 1 has two elements, row 2 has
        // one), each its own single-run RLE at bit width 1 (max_rep_level=1).
        let rep_bytes = [0x02, 0x00, 0x02, 0x01, 0x02, 0x00];
        // def = [2, 2, 1] (max_def_level=2): two runs, RLE at bit width 2.
        let def_bytes = [0x04, 0x02, 0x02, 0x01];
        let mut body = Vec::new();
        body.extend_from_slice(&(rep_bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&rep_bytes);
        body.extend_from_slice(&(def_bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&def_bytes);
        body.extend_from_slice(b"ab");

        let (rep, def, values) = split_list_levels(&body, 1, 2, 3).unwrap();
        assert_eq!(rep, vec![0, 1, 0]);
        assert_eq!(def, vec![2, 2, 1]);
        assert_eq!(values, b"ab");
    }

    #[test]
    fn group_list_values_handles_multi_element_empty_and_null_element_rows() {
        // list_group_def_level=1, max_def_level=2 (the "tags REPEATED
        // directly, item OPTIONAL beneath it" shape from
        // `a_clean_single_level_list_of_scalars_is_list_eligible`). No
        // "whole list is null" state is reachable here (see the next test
        // for that) — only "present with N elements" (N >= 0), possibly
        // with a null element inside.
        //
        // Row A: two real elements ["x", "y"].
        // Row B: present, zero elements (empty list).
        // Row C: one element, itself null.
        // Row D: three elements, the middle one null: ["p", null, "q"].
        let rep = vec![0, 1, /*B*/ 0, /*C*/ 0, /*D*/ 0, 1, 1];
        let def = vec![2, 2, /*B*/ 0, /*C*/ 1, /*D*/ 2, 1, 2];
        let values = vec![
            JsonValue::String("x".into()),
            JsonValue::String("y".into()),
            JsonValue::Null, // B's single occurrence carries no value
            JsonValue::Null, // C: null element
            JsonValue::String("p".into()),
            JsonValue::Null, // D's middle element: null
            JsonValue::String("q".into()),
        ];
        let rows = group_list_values(&rep, &def, values, 1);
        assert_eq!(
            rows,
            vec![
                JsonValue::Array(vec![JsonValue::String("x".into()), JsonValue::String("y".into())]),
                JsonValue::Array(vec![]),
                JsonValue::Array(vec![JsonValue::Null]),
                JsonValue::Array(vec![
                    JsonValue::String("p".into()),
                    JsonValue::Null,
                    JsonValue::String("q".into())
                ]),
            ]
        );
    }

    #[test]
    fn group_list_values_distinguishes_null_list_from_empty_list() {
        // list_group_def_level=2 (an OPTIONAL wrapper above the REPEATED
        // group, and a REQUIRED leaf beneath it — no null-element state,
        // only whole-list-null vs. present-empty vs. present-with-values).
        // Row X: the whole list property is null (wrapper absent).
        // Row Y: wrapper present, repeated field has zero occurrences.
        // Row Z: one real element.
        let rep = vec![0, 0, 0];
        let def = vec![0, 1, 2];
        let values = vec![JsonValue::Null, JsonValue::Null, JsonValue::String("v".into())];
        let rows = group_list_values(&rep, &def, values, 2);
        assert_eq!(
            rows,
            vec![JsonValue::Null, JsonValue::Array(vec![]), JsonValue::Array(vec![JsonValue::String("v".into())])]
        );
    }

    /// Encode a `logicalType` union field's body the way a real writer does:
    /// one member (`member_id` — 17 for `GeometryType`, 18 for `GeographyType`,
    /// anything else for "not geometry") set as a `STRUCT`, optionally with a
    /// `crs` string at its own field 1. Matches the shape confirmed against a
    /// real `ogr2ogr -f Parquet -lco USE_PARQUET_GEO_TYPES=ONLY` fixture's raw
    /// thrift bytes.
    fn encode_logical_type(member_id: i16, crs: Option<&str>) -> Vec<u8> {
        use super::super::thrift::CompactWriter;
        let mut w = CompactWriter::new();
        w.struct_begin(); // the union itself
        w.field_struct(member_id); // the one member that's set
        if let Some(crs) = crs {
            w.field_string(1, crs);
        }
        w.struct_end(); // close the member struct
        w.struct_end(); // close the union
        w.into_bytes()
    }

    #[test]
    fn geometry_logical_type_detects_geometry_and_geography_with_crs() {
        let bytes = encode_logical_type(17, Some(r#"{"type":"GeographicCRS"}"#));
        let (is_geom, crs) = parse_geometry_logical_type(&mut CompactReader::new(&bytes)).unwrap();
        assert!(is_geom);
        assert_eq!(crs.as_deref(), Some(r#"{"type":"GeographicCRS"}"#));

        // No crs field at all (the default-CRS case): still detected as geometry.
        let bytes = encode_logical_type(17, None);
        let (is_geom, crs) = parse_geometry_logical_type(&mut CompactReader::new(&bytes)).unwrap();
        assert!(is_geom);
        assert_eq!(crs, None);

        // GeographyType (tag 18) is geometry too.
        let bytes = encode_logical_type(18, None);
        let (is_geom, _) = parse_geometry_logical_type(&mut CompactReader::new(&bytes)).unwrap();
        assert!(is_geom);

        // A different logicalType member (e.g. StringType, tag 1) is not geometry.
        let bytes = encode_logical_type(1, None);
        let (is_geom, crs) = parse_geometry_logical_type(&mut CompactReader::new(&bytes)).unwrap();
        assert!(!is_geom);
        assert_eq!(crs, None);
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

    #[test]
    fn geo_geometry_columns_reads_every_key_of_the_columns_object() {
        let geo = r#"{"version":"1.0.0","primary_column":"geom","columns":{
            "geom2":{"encoding":"WKB"},
            "geom":{"encoding":"WKB"}
        }}"#;
        let mut cols = geo_geometry_columns(geo);
        cols.sort();
        assert_eq!(cols, vec!["geom".to_string(), "geom2".to_string()]);
    }

    #[test]
    fn geo_geometry_columns_is_empty_without_a_columns_object() {
        let geo = r#"{"version":"1.0.0"}"#;
        assert!(geo_geometry_columns(geo).is_empty());
    }

    #[test]
    fn rejects_a_real_duckdb_fixture_with_two_geometry_columns_named_by_geo_metadata() {
        // `CREATE TABLE t (geom, geom2, ...)` with both columns spatial, then
        // `COPY t TO ... (FORMAT PARQUET)` — DuckDB's spatial extension
        // declares *both* under the "geo" metadata's "columns" object
        // (primary_column: "geom"), a real file no synthesis was needed for.
        let bytes = include_bytes!("../../tests/fixtures/duckdb_multi_geometry.parquet");
        let msg = match read_geoparquet(bytes) {
            Ok(_) => panic!("a second geo-metadata geometry column should error"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("geom2"), "{msg}");
        assert!(msg.contains("multiple geometry columns are not supported yet"), "{msg}");
    }

    #[test]
    fn rejects_a_pyarrow_fixture_with_two_geometry_columns_named_by_geo_metadata() {
        // Real WKB bytes for two columns written via pyarrow, with a
        // hand-authored spec-conformant "geo" JSON declaring both under
        // "columns" (pyarrow itself has no geometry concept, so this is the
        // only way to get this shape without a writer that natively supports
        // it — see plans/arbitrary-geoparquet.org's 6b notes). A second, real
        // fixture on an independent code path from the DuckDB one above.
        let bytes = include_bytes!("../../tests/fixtures/pyarrow_multi_geometry.parquet");
        let msg = match read_geoparquet(bytes) {
            Ok(_) => panic!("a second geo-metadata geometry column should error"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("geom2"), "{msg}");
        assert!(msg.contains("multiple geometry columns are not supported yet"), "{msg}");
    }

    // --- milestone 7: nested/repeated (list-valued) columns ----------------

    fn array_of(strs: &[&str]) -> JsonValue {
        JsonValue::Array(strs.iter().map(|s| JsonValue::String((*s).into())).collect())
    }

    #[test]
    fn reads_a_real_duckdb_fixture_with_a_list_valued_property() {
        // `CREATE TABLE t(id, geom, tags) ...` with `tags` a `LIST(VARCHAR)`
        // column, `COPY ... TO parquet` — DuckDB's default `LIST` shape: an
        // OPTIONAL "tags" group (converted_type LIST) wrapping a REPEATED
        // "list" group wrapping an OPTIONAL "element" leaf. 3 rows: a
        // 2-element list, an empty (present, zero-element) list, and a
        // 1-element list — PLAIN-encoded, single row group/page.
        let bytes = include_bytes!("../../tests/fixtures/duckdb_list_column.parquet");
        let parsed = read_geoparquet(bytes).expect("reads the DuckDB list-column fixture");
        assert_eq!(parsed.num_rows, 3);

        let tags = &parsed.properties.iter().find(|c| c.name == "tags").unwrap().values;
        assert_eq!(tags[0], array_of(&["park", "historic"]));
        assert_eq!(tags[1], JsonValue::Array(vec![]));
        assert_eq!(tags[2], array_of(&["school"]));

        assert_eq!(parsed.geometry.len(), 3);
        assert!(parsed.geometry.iter().all(Option::is_some));
    }

    #[test]
    fn reads_a_real_duckdb_fixture_with_a_dictionary_encoded_list_column() {
        // Same `LIST(VARCHAR)` shape, but 60 rows over a 3-value vocabulary
        // ("a", "b", "c") — enough for DuckDB to dictionary-encode the
        // "element" leaf (`PLAIN_DICTIONARY`), the harder decode path:
        // `decode_list_page_values` has to route dictionary indices through
        // `decode_values` correctly, not just PLAIN bytes. Every 5th row is
        // an empty list, every 3rd (non-5th) is `["a"]`, everything else is
        // `["a", "b", "c"]` — mirrors the generating query in this file's
        // history (see the fixture's own generation notes in
        // plans/arbitrary-geoparquet.org if regenerated).
        let bytes = include_bytes!("../../tests/fixtures/duckdb_list_column_dict.parquet");
        let parsed = read_geoparquet(bytes).expect("reads the dictionary-encoded list fixture");
        assert_eq!(parsed.num_rows, 60);

        let tags = &parsed.properties.iter().find(|c| c.name == "tags").unwrap().values;
        let expected = |i: i64| -> JsonValue {
            if i % 5 == 0 {
                JsonValue::Array(vec![])
            } else if i % 3 == 0 {
                array_of(&["a"])
            } else {
                array_of(&["a", "b", "c"])
            }
        };
        for i in 0..60i64 {
            assert_eq!(tags[i as usize], expected(i), "row {i}");
        }
    }

    #[test]
    fn reads_a_real_gdal_fixture_combining_a_list_column_and_a_covering_bbox() {
        // `ogr2ogr -f Parquet` on a GeoJSON with a `tags` StringList
        // property — GDAL's Parquet driver writes the same standard 3-level
        // list shape DuckDB does, *and* (as it does by default, per
        // `reads_a_real_gdal_fixture_with_a_covering_bbox_column`) a
        // `geometry_bbox` covering-bbox struct alongside the geometry. Real
        // file exercising both milestone 7 (list decode) and §3c (nested
        // OPTIONAL-group definition levels) at once, from an independent
        // tool (GDAL, not DuckDB).
        let bytes = include_bytes!("../../tests/fixtures/gdal_list_column.parquet");
        let parsed = read_geoparquet(bytes).expect("reads the GDAL list-column fixture");
        assert_eq!(parsed.num_rows, 3);

        // The covering-bbox columns are still excluded, not surfaced as properties.
        let names: Vec<&str> = parsed.properties.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "tags"]);

        let tags = &parsed.properties.iter().find(|c| c.name == "tags").unwrap().values;
        assert_eq!(tags[0], array_of(&["park", "historic"]));
        assert_eq!(tags[1], JsonValue::Array(vec![]));
        assert_eq!(tags[2], array_of(&["school"]));

        assert_eq!(parsed.geometry.len(), 3);
        assert!(parsed.geometry.iter().all(Option::is_some));
    }
}
