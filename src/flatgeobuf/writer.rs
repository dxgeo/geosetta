//! Encode a [`FeatureCollection`] as FlatGeobuf, using the from-scratch
//! [`crate::flatbuffers::Builder`]. See `plans/flatgeobuf.org`.
//!
//! **The packed R-tree is built**, not deferred — this doc said otherwise until
//! 2026-10-21, describing an index-less writer (`index_node_size = 0`) that had
//! stopped being the implementation. It is written whenever *every* feature has
//! a geometry, since a leaf needs a bbox; otherwise the file goes out
//! index-less.
//!
//! **Features are written in the order they arrive**, and the index is packed
//! over that order. Hilbert locality is a *caller's* request — `--sort-hilbert`,
//! applied to the IR before this writer runs — rather than something the writer
//! imposes, because imposing it permuted content on a same-format run and made
//! that flag a no-op for this format. See the comment in [`fn@write`].

use crate::feature::FeatureCollection;
use crate::flatbuffers::Builder;
use crate::format::Format;
use crate::native::{FgbNative, NativePayload};
use crate::geometry::{Bbox, Geometry, Position};
use crate::schema::{Cell, ColumnType, columns_for, DeclType};

const MAGIC: [u8; 8] = [b'f', b'g', b'b', 3, b'f', b'g', b'b', 1];

/// Packed R-tree fan-out (FlatGeobuf's default) and the on-disk NodeItem size
/// (4 × f64 bbox + u64 offset).
const NODE_SIZE: u16 = 16;
const NODE_ITEM_SIZE: usize = 40;

// GeometryType enum (ubyte).
mod gtype {
    pub const UNKNOWN: u8 = 0;
    pub const POINT: u8 = 1;
    pub const LINESTRING: u8 = 2;
    pub const POLYGON: u8 = 3;
    pub const MULTIPOINT: u8 = 4;
    pub const MULTILINESTRING: u8 = 5;
    pub const MULTIPOLYGON: u8 = 6;
    pub const GEOMETRYCOLLECTION: u8 = 7;
}

// ColumnType enum (ubyte). The full set, not just the four the hub infers:
// a column carrying a declared type is written as *that* type, so every value
// the reader can name has to be nameable here too.
use super::reader::ctype;

// Field indices.
mod header {
    pub const NAME: usize = 0;
    pub const ENVELOPE: usize = 1;
    pub const GEOMETRY_TYPE: usize = 2;
    pub const HAS_Z: usize = 3;
    pub const HAS_M: usize = 4;
    pub const COLUMNS: usize = 7;
    pub const FEATURES_COUNT: usize = 8;
    pub const INDEX_NODE_SIZE: usize = 9;
    pub const CRS: usize = 10;
    pub const TITLE: usize = 11;
    pub const DESCRIPTION: usize = 12;
    pub const METADATA: usize = 13;
    pub const NUM_FIELDS: usize = 14;
}
mod crs {
    pub const ORG: usize = 0;
    pub const CODE: usize = 1;
    pub const NAME: usize = 2;
    pub const DESCRIPTION: usize = 3;
    pub const WKT: usize = 4;
    pub const CODE_STRING: usize = 5;
    pub const NUM_FIELDS: usize = 6;
}
// Slots 4-9 are the declaration half, mirroring `reader.rs`'s `column`
// module; 2/3 (`title`/`description`) and 10 (`metadata`) are not written yet.
// `NUM_FIELDS` is the table's full width regardless, since a vtable is sized
// by the schema and not by which fields happen to be set.
mod column {
    pub const NAME: usize = 0;
    pub const TYPE: usize = 1;
    pub const WIDTH: usize = 4;
    pub const PRECISION: usize = 5;
    pub const SCALE: usize = 6;
    pub const NULLABLE: usize = 7;
    pub const UNIQUE: usize = 8;
    pub const PRIMARY_KEY: usize = 9;
    pub const TITLE: usize = 2;
    pub const DESCRIPTION: usize = 3;
    pub const METADATA: usize = 10;
    pub const NUM_FIELDS: usize = 11;
}
mod feature {
    pub const GEOMETRY: usize = 0;
    pub const PROPERTIES: usize = 1;
    pub const NUM_FIELDS: usize = 2;
}
mod geometry {
    pub const ENDS: usize = 0;
    pub const XY: usize = 1;
    pub const Z: usize = 2;
    pub const M: usize = 3;
    pub const TYPE: usize = 6;
    pub const PARTS: usize = 7;
    pub const NUM_FIELDS: usize = 8;
}

/// Serialize a feature collection to FlatGeobuf bytes.
pub fn write(fc: &FeatureCollection) -> Vec<u8> {
    // Columns (name + type) inferred by scanning all features; `values[row]` is
    // this row's cell for each column.
    let columns = columns_for(fc);

    // Bounding box, the geometry-type set, and Z/M presence across all
    // features (the header's `hasZ`/`hasM` are file-level hints; a source
    // whose Z-bearing-ness varies per geometry still gets each geometry's own
    // z/m vectors written correctly — see `dim_of_first`/`dim_of_rings`/
    // `dim_of_geometry` below, mirroring `wkb.rs`'s per-geometry dimension
    // derivation).
    let mut bbox = Bbox::empty();
    let mut types = std::collections::BTreeSet::new();
    let mut has_z = false;
    let mut has_m = false;
    for f in &fc.features {
        if let Some(g) = &f.geometry {
            g.extend_bbox(&mut bbox);
            types.insert(fgb_geometry_type(g));
            let dim = dim_of_geometry(g);
            has_z |= dim.0;
            has_m |= dim.1;
        }
    }
    // A single shared type if uniform, else Unknown (per-feature type is always
    // written too).
    let header_type = if types.len() == 1 {
        *types.iter().next().unwrap()
    } else {
        gtype::UNKNOWN
    };

    // A packed R-tree needs a bbox per feature, so index only when every feature
    // has geometry; otherwise write index-less.
    //
    // ***Features go out in the order they arrived.*** This writer used to
    // Hilbert-sort them here, on the reasoning that a *packed Hilbert* R-tree
    // is that sort — true of the locality the name describes, and not a
    // requirement of the structure: the tree is packed bottom-up over whatever
    // order the leaves are in, and a reader walks it by bbox either way.
    //
    // Sorting here cost two things that are not the writer's to spend. It
    // *silently permuted content* on a same-format run, which
    // `plans/same-format-fidelity.org` § THE RULE forbids — found 2026-10-21
    // by the D6 fixture registry on its first run. And it made `--sort-hilbert`
    // a dead flag for this format, since `main.rs` applies that flag to the IR
    // *before* calling this writer: the sort was mandatory where the CLI
    // advertised it as opt-in, and opt-in for GeoPackage's `--rtree` right
    // beside it. So locality is now the caller's request and the index is
    // unconditional, which is the combination that spends nothing.
    let feature_bboxes: Vec<Bbox> = fc.features.iter().map(feature_bbox).collect();
    let indexed = !fc.features.is_empty() && fc.features.iter().all(|f| f.geometry.is_some());
    let node_size = if indexed { NODE_SIZE } else { 0 };

    // Encode features in input order, recording each framed feature's byte
    // offset within the feature section (for the R-tree leaf pointers).
    let mut feature_section = Vec::new();
    let mut offsets = Vec::with_capacity(fc.features.len());
    for (idx, feature) in fc.features.iter().enumerate() {
        offsets.push(feature_section.len() as u64);
        let bytes = build_feature(feature.geometry.as_ref(), &columns, idx);
        feature_section.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        feature_section.extend_from_slice(&bytes);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    let header = build_header(fc, &columns, &bbox, header_type, has_z, has_m, node_size);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    if indexed {
        out.extend_from_slice(&packed_rtree(&feature_bboxes, &offsets, node_size));
    }
    out.extend_from_slice(&feature_section);
    out
}

fn feature_bbox(f: &crate::feature::Feature) -> Bbox {
    f.geometry.as_ref().map(|g| g.bbox()).unwrap_or_else(Bbox::empty)
}

#[allow(clippy::too_many_arguments)]
fn build_header(
    fc: &FeatureCollection,
    columns: &[crate::schema::Column],
    bbox: &Bbox,
    header_type: u8,
    has_z: bool,
    has_m: bool,
    node_size: u16,
) -> Vec<u8> {
    let mut b = Builder::new();
    // Whatever this collection's source recorded that the IR does not model,
    // but only when the source *was* FlatGeobuf — `payload_for` is the tag
    // check, and a GeoJSON foreign member must never reach these slots.
    let native = match fc.native.as_ref().and_then(|n| n.payload_for(Format::FlatGeobuf)) {
        Some(NativePayload::FlatGeobuf(f)) => Some(f),
        _ => None,
    };
    let col_natives = native.map(|n| &n.columns[..]).unwrap_or(&[]);

    // Column sub-tables first (referenced objects precede their referrers).
    let mut col_offsets = Vec::with_capacity(columns.len());
    for col in columns {
        let cn = col_natives.iter().find(|c| c.name == col.name);
        let name = b.create_string(&col.name);
        // Every string this table references must be built before the table is
        // started, so the three descriptive members are created up front.
        let title = cn.and_then(|c| c.title.as_deref()).map(|s| b.create_string(s));
        let description = cn.and_then(|c| c.description.as_deref()).map(|s| b.create_string(s));
        let metadata = cn.and_then(|c| c.metadata.as_deref()).map(|s| b.create_string(s));
        b.start_table(column::NUM_FIELDS);
        b.add_offset(column::NAME, name);
        b.add_u8(column::TYPE, fgb_column_type(col), 255);
        for (slot, off) in
            [(column::TITLE, title), (column::DESCRIPTION, description), (column::METADATA, metadata)]
        {
            if let Some(o) = off {
                b.add_offset(slot, o);
            }
        }
        write_decl(&mut b, col.decl.as_ref());
        col_offsets.push(b.end_table());
    }
    let columns_vec = b.create_offset_vector(&col_offsets);
    // The collection's own name. Writing a constant here was defect 4: it made
    // a layer name unable to survive an FGB transit, which is what let a
    // GeoPackage edit loop append a duplicate layer instead of replacing one
    // (`plans/edit-composability.org` D4). A nameless collection writes no
    // name rather than inventing one — naming an output is the caller's job,
    // and `main.rs` supplies one from `--layer` or the input's stem.
    let name = fc.name.as_deref().map(|n| b.create_string(n));
    let title = native.and_then(|n| n.title.as_deref()).map(|s| b.create_string(s));
    let description = native.and_then(|n| n.description.as_deref()).map(|s| b.create_string(s));
    let metadata = native.and_then(|n| n.metadata.as_deref()).map(|s| b.create_string(s));
    let envelope = (!bbox.is_empty())
        .then(|| b.create_f64_vector(&[bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y]));
    // The CRS sub-table (built before the header that references it), carrying
    // whatever the source recorded through unchanged.
    let crs_off = build_crs(&mut b, fc.crs.as_ref(), native);

    b.start_table(header::NUM_FIELDS);
    if let Some(n) = name {
        b.add_offset(header::NAME, n);
    }
    if let Some(env) = envelope {
        b.add_offset(header::ENVELOPE, env);
    }
    b.add_u8(header::GEOMETRY_TYPE, header_type, gtype::UNKNOWN);
    b.add_u8(header::HAS_Z, has_z as u8, 0);
    b.add_u8(header::HAS_M, has_m as u8, 0);
    b.add_offset(header::COLUMNS, columns_vec);
    b.add_u64(header::FEATURES_COUNT, fc.features.len() as u64, 0);
    // Default index_node_size is 16, so an indexed file (16) omits the field and
    // an index-less one writes 0.
    b.add_u16(header::INDEX_NODE_SIZE, node_size, 16);
    if let Some(off) = crs_off {
        b.add_offset(header::CRS, off);
    }
    for (slot, off) in
        [(header::TITLE, title), (header::DESCRIPTION, description), (header::METADATA, metadata)]
    {
        if let Some(o) = off {
            b.add_offset(slot, o);
        }
    }
    let root = b.end_table();
    b.finish(root)
}

/// Build the FlatGeobuf `Crs` sub-table, returning its offset, or `None` when
/// the collection has no CRS (so the header omits the field). WGS 84 is written
/// as EPSG:4326, the spelling GDAL uses.
fn build_crs(b: &mut Builder, crs: Option<&crate::crs::Crs>, native: Option<&FgbNative>) -> Option<usize> {
    use crate::crs::Crs;
    // FlatGeobuf's Crs.code is int32 on the wire — genuinely numeric, unlike the
    // IR's string code (which also holds IGNF/OGC/PROJ/NKG's alphanumeric
    // codes). A non-numeric code can't be represented here; it falls back to
    // the same "no code" 0/unset sentinel as below, same as GeoPackage's
    // synthetic-id fallback — org and wkt still carry through.
    // Not `crs?`: a source may have recorded a CRS *name* and nothing else,
    // and returning early on a missing `Crs` would drop the sub-table with the
    // native still in it.
    let (org, code, wkt) = match crs {
        Some(Crs::Wgs84) => (Some("EPSG".to_string()), Some(4326), None),
        Some(Crs::Named(n)) => (
            n.authority.clone(),
            n.code.as_deref().and_then(|c| c.parse::<i64>().ok()),
            n.wkt.clone().or_else(|| n.structural_wkt()),
        ),
        None => (None, None, None),
    };
    // The three members `Crs` itself has no slot for, carried across as a
    // native. `name` is populated in every `ogr2ogr`-written file observed.
    let extra = |f: fn(&FgbNative) -> Option<&String>| native.and_then(f).map(String::as_str);
    let crs_name = extra(|n| n.crs_name.as_ref());
    let crs_description = extra(|n| n.crs_description.as_ref());
    let crs_code_string = extra(|n| n.crs_code_string.as_ref());

    // Nothing worth recording. The natives count: a file whose only CRS
    // content was a name still recorded something, and dropping the sub-table
    // would lose it.
    if org.is_none()
        && code.is_none()
        && wkt.is_none()
        && crs_name.is_none()
        && crs_description.is_none()
        && crs_code_string.is_none()
    {
        return None;
    }
    let org_off = org.as_deref().map(|s| b.create_string(s));
    let wkt_off = wkt.as_deref().map(|s| b.create_string(s));
    let name_off = crs_name.map(|s| b.create_string(s));
    let desc_off = crs_description.map(|s| b.create_string(s));
    let code_string_off = crs_code_string.map(|s| b.create_string(s));
    b.start_table(crs::NUM_FIELDS);
    if let Some(o) = org_off {
        b.add_offset(crs::ORG, o);
    }
    // 0 is FlatGeobuf's default/"unset" code, so a real code of 0 would be
    // dropped; codes are positive in practice, so this is fine.
    b.add_i32(crs::CODE, code.unwrap_or(0) as i32, 0);
    if let Some(w) = wkt_off {
        b.add_offset(crs::WKT, w);
    }
    for (slot, off) in
        [(crs::NAME, name_off), (crs::DESCRIPTION, desc_off), (crs::CODE_STRING, code_string_off)]
    {
        if let Some(o) = off {
            b.add_offset(slot, o);
        }
    }
    Some(b.end_table())
}

fn build_feature(geom: Option<&Geometry>, columns: &[crate::schema::Column], row: usize) -> Vec<u8> {
    let mut b = Builder::new();

    let geom_off = geom.map(|g| encode_geometry(&mut b, g));
    let blob = encode_properties(columns, row);
    let props_off = (!blob.is_empty()).then(|| b.create_byte_vector(&blob));

    b.start_table(feature::NUM_FIELDS);
    if let Some(g) = geom_off {
        b.add_offset(feature::GEOMETRY, g);
    }
    if let Some(p) = props_off {
        b.add_offset(feature::PROPERTIES, p);
    }
    let root = b.end_table();
    b.finish(root)
}

/// Encode a geometry into a Geometry table, returning its rev-offset. Children
/// (xy / z / m / ends / parts) are built before the table.
fn encode_geometry(b: &mut Builder, g: &Geometry) -> usize {
    let ty = fgb_geometry_type(g);
    match g {
        Geometry::Point(p) => {
            let xy = b.create_f64_vector(&[p.x, p.y]);
            let z = p.z.map(|z| b.create_f64_vector(&[z]));
            let m = p.m.map(|m| b.create_f64_vector(&[m]));
            geom_table(b, ty, None, Some(xy), None, z, m)
        }
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => {
            let xy = b.create_f64_vector(&flatten(ps));
            let dim = dim_of_first(ps);
            let z = dim.0.then(|| b.create_f64_vector(&flatten_z(ps)));
            let m = dim.1.then(|| b.create_f64_vector(&flatten_m(ps)));
            geom_table(b, ty, None, Some(xy), None, z, m)
        }
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            let xy = b.create_f64_vector(&flatten_rings(rings));
            let ends = (rings.len() > 1).then(|| b.create_u32_vector(&ring_ends(rings)));
            let dim = dim_of_rings(rings);
            let z = dim.0.then(|| b.create_f64_vector(&flatten_rings_z(rings)));
            let m = dim.1.then(|| b.create_f64_vector(&flatten_rings_m(rings)));
            geom_table(b, ty, ends, Some(xy), None, z, m)
        }
        Geometry::MultiPolygon(polys) => {
            let part_offsets: Vec<usize> = polys
                .iter()
                .map(|rings| {
                    let xy = b.create_f64_vector(&flatten_rings(rings));
                    let ends = (rings.len() > 1).then(|| b.create_u32_vector(&ring_ends(rings)));
                    let dim = dim_of_rings(rings);
                    let z = dim.0.then(|| b.create_f64_vector(&flatten_rings_z(rings)));
                    let m = dim.1.then(|| b.create_f64_vector(&flatten_rings_m(rings)));
                    geom_table(b, gtype::POLYGON, ends, Some(xy), None, z, m)
                })
                .collect();
            let parts = b.create_offset_vector(&part_offsets);
            geom_table(b, ty, None, None, Some(parts), None, None)
        }
        Geometry::GeometryCollection(geoms) => {
            let part_offsets: Vec<usize> = geoms.iter().map(|g| encode_geometry(b, g)).collect();
            let parts = b.create_offset_vector(&part_offsets);
            geom_table(b, ty, None, None, Some(parts), None, None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn geom_table(
    b: &mut Builder,
    ty: u8,
    ends: Option<usize>,
    xy: Option<usize>,
    parts: Option<usize>,
    z: Option<usize>,
    m: Option<usize>,
) -> usize {
    b.start_table(geometry::NUM_FIELDS);
    b.add_u8(geometry::TYPE, ty, gtype::UNKNOWN);
    if let Some(e) = ends {
        b.add_offset(geometry::ENDS, e);
    }
    if let Some(xy) = xy {
        b.add_offset(geometry::XY, xy);
    }
    if let Some(z) = z {
        b.add_offset(geometry::Z, z);
    }
    if let Some(m) = m {
        b.add_offset(geometry::M, m);
    }
    if let Some(p) = parts {
        b.add_offset(geometry::PARTS, p);
    }
    b.end_table()
}

/// Encode this row's non-null properties: `[u16 col_index][typed value]…`.
fn encode_properties(columns: &[crate::schema::Column], row: usize) -> Vec<u8> {
    let mut blob = Vec::new();
    for (i, col) in columns.iter().enumerate() {
        let cell = &col.values[row];
        if matches!(cell, Cell::Null) {
            continue;
        }
        blob.extend_from_slice(&(i as u16).to_le_bytes());
        // The value must be encoded in the width the *header* declares for this
        // column, which is `fgb_column_type`'s answer and not necessarily the
        // hub type's: a column written as `SHORT` stores two bytes, and writing
        // eight would desynchronise every value after it in the row.
        match (fgb_column_type(col), cell) {
            (ctype::BOOL, Cell::Bool(v)) => blob.push(*v as u8),
            (ctype::BYTE, Cell::Int(v)) => blob.push(*v as i8 as u8),
            (ctype::UBYTE, Cell::Int(v)) => blob.push(*v as u8),
            (ctype::SHORT, Cell::Int(v)) => blob.extend_from_slice(&(*v as i16).to_le_bytes()),
            (ctype::USHORT, Cell::Int(v)) => blob.extend_from_slice(&(*v as u16).to_le_bytes()),
            (ctype::INT, Cell::Int(v)) => blob.extend_from_slice(&(*v as i32).to_le_bytes()),
            (ctype::UINT, Cell::Int(v)) => blob.extend_from_slice(&(*v as u32).to_le_bytes()),
            (ctype::LONG, Cell::Int(v)) => blob.extend_from_slice(&v.to_le_bytes()),
            (ctype::ULONG, Cell::Int(v)) => blob.extend_from_slice(&(*v as u64).to_le_bytes()),
            (ctype::FLOAT, Cell::Double(v)) => {
                blob.extend_from_slice(&(*v as f32).to_le_bytes())
            }
            (ctype::DOUBLE, Cell::Double(v)) => blob.extend_from_slice(&v.to_le_bytes()),
            (ctype::STRING | ctype::JSON | ctype::DATETIME, Cell::Str(s)) => {
                blob.extend_from_slice(&(s.len() as u32).to_le_bytes());
                blob.extend_from_slice(s.as_bytes());
            }
            (ctype::BINARY, Cell::Bytes(b)) => {
                blob.extend_from_slice(&(b.len() as u32).to_le_bytes());
                blob.extend_from_slice(b);
            }
            // `declared_type` only returns a declaration whose `hub()` matches
            // this column's inferred type, and `infer_columns` guarantees each
            // cell matches that — so every reachable pairing is above.
            _ => unreachable!("cell type does not match the column's on-disk type"),
        }
    }
    blob
}

fn fgb_geometry_type(g: &Geometry) -> u8 {
    match g {
        Geometry::Point(_) => gtype::POINT,
        Geometry::LineString(_) => gtype::LINESTRING,
        Geometry::Polygon(_) => gtype::POLYGON,
        Geometry::MultiPoint(_) => gtype::MULTIPOINT,
        Geometry::MultiLineString(_) => gtype::MULTILINESTRING,
        Geometry::MultiPolygon(_) => gtype::MULTIPOLYGON,
        Geometry::GeometryCollection(_) => gtype::GEOMETRYCOLLECTION,
    }
}

/// Replay a source's column declaration into the `Column` table being built.
///
/// This is the half of a FlatGeobuf file that makes it usable as *transit*: a
/// declaration written here survives into a pipeline's middle stage, where an
/// external tool can read and widen it before the far end writes a format that
/// has fixed-width fields. Nothing else in the crate carries a schema between
/// processes (see `plans/same-format-fidelity.org` D1/D5).
///
/// Each `add_*` is given the spec's own default, so a field equal to its
/// default costs no vtable slot — an undeclared column writes exactly the two
/// bytes it wrote before this existed.
fn write_decl(b: &mut Builder, decl: Option<&crate::schema::FieldDecl>) {
    let Some(d) = decl else { return };
    if let Some(w) = d.width {
        b.add_i32(column::WIDTH, w as i32, -1);
    }
    if let Some(p) = d.precision {
        b.add_i32(column::PRECISION, p as i32, -1);
    }
    if let Some(s) = d.scale {
        b.add_i32(column::SCALE, s as i32, -1);
    }
    if let Some(n) = d.nullable {
        b.add_u8(column::NULLABLE, n as u8, 1);
    }
    if let Some(u) = d.unique {
        b.add_u8(column::UNIQUE, u as u8, 0);
    }
    if let Some(k) = d.primary_key {
        b.add_u8(column::PRIMARY_KEY, k as u8, 0);
    }
}

/// The on-disk type for a column: what the source *declared*, when that
/// declaration still describes the data, else what the values inferred.
///
/// The declaration is consulted first because the hub's four types cannot tell
/// an `Int16` from an `Int64` or a `DateTime` from a `String` — re-deriving
/// from values alone is defect 5, and it is why a temporal column used to come
/// back as text. [`Column::declared_type`] is where the "still describes the
/// data" judgement lives.
fn fgb_column_type(col: &crate::schema::Column) -> u8 {
    if let Some(d) = col.declared_type() {
        return decl_column_type(d);
    }
    match col.ty {
        ColumnType::Bool => ctype::BOOL,
        ColumnType::Int64 => ctype::LONG,
        ColumnType::Double => ctype::DOUBLE,
        ColumnType::String => ctype::STRING,
        ColumnType::Binary => ctype::BINARY,
    }
}

fn decl_column_type(d: DeclType) -> u8 {
    match d {
        DeclType::Bool => ctype::BOOL,
        DeclType::Int8 => ctype::BYTE,
        DeclType::UInt8 => ctype::UBYTE,
        DeclType::Int16 => ctype::SHORT,
        DeclType::UInt16 => ctype::USHORT,
        DeclType::Int32 => ctype::INT,
        DeclType::UInt32 => ctype::UINT,
        DeclType::Int64 => ctype::LONG,
        DeclType::UInt64 => ctype::ULONG,
        DeclType::Float32 => ctype::FLOAT,
        DeclType::Float64 => ctype::DOUBLE,
        DeclType::Json => ctype::JSON,
        DeclType::DateTime => ctype::DATETIME,
        // No FlatGeobuf slot narrower than text, and `Binary` never reaches
        // here: the reader declines to declare it (its cells decode to null).
        DeclType::Binary => ctype::BINARY,
        // FlatGeobuf has no DATE. Writing a date as DATETIME keeps it *temporal*
        // rather than degrading it to text, which is what `ogr2ogr` does with
        // the same hop (measured: a gpkg DATE column arrives in FlatGeobuf as
        // `DateTime`). Still a recast — a date is not an instant, and
        // `decl_recast_warning` says so — but the better of the two available.
        DeclType::Date => ctype::DATETIME,
        DeclType::String => ctype::STRING,
    }
}

fn flatten(ps: &[Position]) -> Vec<f64> {
    ps.iter().flat_map(|p| [p.x, p.y]).collect()
}

fn flatten_rings(rings: &[Vec<Position>]) -> Vec<f64> {
    rings.iter().flat_map(|r| flatten(r)).collect()
}

/// A position list's `z`/`m` values, in the same order as `flatten`'s `xy`.
/// FlatGeobuf's `z`/`m` vectors have no per-point "no data" marker (unlike
/// Shapefile's M sentinel), so a position that lacks the ordinate a
/// Z/M-bearing geometry declares falls back to `0.0` — the same choice
/// `wkb.rs`'s encoder makes for the same reason.
fn flatten_z(ps: &[Position]) -> Vec<f64> {
    ps.iter().map(|p| p.z.unwrap_or(0.0)).collect()
}

fn flatten_m(ps: &[Position]) -> Vec<f64> {
    ps.iter().map(|p| p.m.unwrap_or(0.0)).collect()
}

fn flatten_rings_z(rings: &[Vec<Position>]) -> Vec<f64> {
    rings.iter().flat_map(|r| flatten_z(r)).collect()
}

fn flatten_rings_m(rings: &[Vec<Position>]) -> Vec<f64> {
    rings.iter().flat_map(|r| flatten_m(r)).collect()
}

/// The dimensionality implied by a position list's first element (or 2D if
/// empty), returned as `(has_z, has_m)` — used to pick one z/m-presence
/// decision for an entire `LineString`/`MultiPoint`/ring list, matching how
/// `wkb.rs`'s `dim_of_first` declares dimensionality once per geometry
/// rather than per point. Kept as an independent copy in this module rather
/// than shared, matching every other format spoke's self-contained style.
fn dim_of_first(ps: &[Position]) -> (bool, bool) {
    ps.first().map(|p| (p.z.is_some(), p.m.is_some())).unwrap_or((false, false))
}

/// The dimensionality implied by a ring list's first position (its first
/// ring's first point), or 2D if empty.
fn dim_of_rings(rings: &[Vec<Position>]) -> (bool, bool) {
    rings.first().map(|r| dim_of_first(r)).unwrap_or((false, false))
}

/// The dimensionality of a geometry's very first position, found by
/// recursing into whichever variant it is — used for the file-level
/// `hasZ`/`hasM` header hint (each geometry's own z/m vectors are still
/// derived independently in `encode_geometry`, so a per-feature dimension
/// mismatch never loses data, only the header's summary hint).
fn dim_of_geometry(g: &Geometry) -> (bool, bool) {
    first_position(g).map(|p| (p.z.is_some(), p.m.is_some())).unwrap_or((false, false))
}

fn first_position(g: &Geometry) -> Option<Position> {
    match g {
        Geometry::Point(p) => Some(*p),
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => ps.first().copied(),
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            rings.first().and_then(|r| r.first()).copied()
        }
        Geometry::MultiPolygon(polys) => {
            polys.first().and_then(|p| p.first()).and_then(|r| r.first()).copied()
        }
        Geometry::GeometryCollection(geoms) => geoms.first().and_then(first_position),
    }
}

/// Cumulative point counts marking the end of each ring/line.
fn ring_ends(rings: &[Vec<Position>]) -> Vec<u32> {
    let mut ends = Vec::with_capacity(rings.len());
    let mut total = 0u32;
    for r in rings {
        total += r.len() as u32;
        ends.push(total);
    }
    ends
}

// --- packed Hilbert R-tree -------------------------------------------------

/// Build the FlatGeobuf packed Hilbert R-tree. `bboxes` and `offsets` are in
/// Hilbert order; `offsets[i]` is feature i's byte offset in the feature
/// section. Returns the node array (`num_nodes * 40` bytes), root first down to
/// leaves — the layout GDAL and our reader expect.
fn packed_rtree(bboxes: &[Bbox], offsets: &[u64], node_size: u16) -> Vec<u8> {
    let node_size = (node_size as usize).max(2);
    let num_items = bboxes.len();

    // Level sizes, leaves first up to the single-node root.
    let mut level_sizes = vec![num_items];
    let mut n = num_items;
    let mut num_nodes = num_items;
    loop {
        n = n.div_ceil(node_size);
        num_nodes += n;
        level_sizes.push(n);
        if n == 1 {
            break;
        }
    }
    // Each level's [start, end) in the node array (same leaves-first order).
    let mut level_bounds = Vec::with_capacity(level_sizes.len());
    let mut acc = num_nodes;
    for &size in &level_sizes {
        acc -= size;
        level_bounds.push((acc, acc + size));
    }

    let mut nodes = vec![Node::empty(); num_nodes];
    let leaf_start = num_nodes - num_items;
    for i in 0..num_items {
        nodes[leaf_start + i] = Node::leaf(&bboxes[i], offsets[i]);
    }
    // Build parents bottom-up; a parent unions up to node_size children and
    // stores the first child's node index as its offset.
    for level in 0..level_bounds.len() - 1 {
        let (mut pos, end) = level_bounds[level];
        let mut parent = level_bounds[level + 1].0;
        while pos < end {
            let mut node = Node::internal(pos as u64);
            let mut k = 0;
            while k < node_size && pos < end {
                node.expand(&nodes[pos]);
                pos += 1;
                k += 1;
            }
            nodes[parent] = node;
            parent += 1;
        }
    }

    let mut out = Vec::with_capacity(num_nodes * NODE_ITEM_SIZE);
    for node in &nodes {
        out.extend_from_slice(&node.min_x.to_le_bytes());
        out.extend_from_slice(&node.min_y.to_le_bytes());
        out.extend_from_slice(&node.max_x.to_le_bytes());
        out.extend_from_slice(&node.max_y.to_le_bytes());
        out.extend_from_slice(&node.offset.to_le_bytes());
    }
    out
}

#[derive(Clone)]
struct Node {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    offset: u64,
}

impl Node {
    fn empty() -> Node {
        Node {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
            offset: 0,
        }
    }
    fn leaf(b: &Bbox, offset: u64) -> Node {
        Node {
            min_x: b.min_x,
            min_y: b.min_y,
            max_x: b.max_x,
            max_y: b.max_y,
            offset,
        }
    }
    fn internal(offset: u64) -> Node {
        Node {
            offset,
            ..Node::empty()
        }
    }
    fn expand(&mut self, o: &Node) {
        self.min_x = self.min_x.min(o.min_x);
        self.min_y = self.min_y.min(o.min_y);
        self.max_x = self.max_x.max(o.max_x);
        self.max_y = self.max_y.max(o.max_y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::Feature;
    use crate::json::JsonValue;

    #[test]
    fn indexed_round_trip_many_points() {
        // 50 points -> a packed R-tree with interior nodes. index_node_size
        // defaults to 16 (omitted from the header), so on read the index size is
        // computed and skipped; a correct round trip proves that sizing.
        let features: Vec<Feature> = (0..50)
            .map(|i| Feature {
                geometry: Some(Geometry::Point(Position::new((i % 10) as f64, (i / 10) as f64))),
                properties: vec![(
                    "id".into(),
                    JsonValue::Int((i as f64) as i64),
                )],
                native: None,
            })
            .collect();
        let bytes = write(&FeatureCollection::new(features));

        let back = crate::flatgeobuf::read(&bytes).unwrap();
        assert_eq!(back.features.len(), 50);
        // Features are Hilbert-reordered, so check the id set is intact.
        let mut ids: Vec<i64> = back
            .features
            .iter()
            .map(|f| f.properties.iter().find(|(k, _)| &**k == "id").unwrap().1.as_f64().unwrap() as i64)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn z_bearing_point_round_trips() {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::with_z(1.0, 2.0, 381.0))),
            properties: vec![],
            native: None,
        }]);
        let bytes = write(&fc);
        let back = crate::flatgeobuf::read(&bytes).unwrap();
        assert_eq!(back.features[0].geometry, Some(Geometry::Point(Position::with_z(1.0, 2.0, 381.0))));
    }

    #[test]
    fn zm_bearing_line_string_round_trips() {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::LineString(vec![
                Position::with_zm(0.0, 0.0, 1.0, 10.0),
                Position::with_zm(1.0, 1.0, 2.0, 20.0),
            ])),
            properties: vec![],
            native: None,
        }]);
        let bytes = write(&fc);
        let back = crate::flatgeobuf::read(&bytes).unwrap();
        assert_eq!(
            back.features[0].geometry,
            Some(Geometry::LineString(vec![
                Position::with_zm(0.0, 0.0, 1.0, 10.0),
                Position::with_zm(1.0, 1.0, 2.0, 20.0),
            ]))
        );
    }

    #[test]
    fn z_bearing_polygon_with_hole_round_trips() {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Polygon(vec![
                vec![
                    Position::with_z(0.0, 0.0, 1.0),
                    Position::with_z(4.0, 0.0, 1.0),
                    Position::with_z(4.0, 4.0, 1.0),
                    Position::with_z(0.0, 4.0, 1.0),
                    Position::with_z(0.0, 0.0, 1.0),
                ],
                vec![
                    Position::with_z(1.0, 1.0, 2.0),
                    Position::with_z(2.0, 1.0, 2.0),
                    Position::with_z(2.0, 2.0, 2.0),
                    Position::with_z(1.0, 2.0, 2.0),
                    Position::with_z(1.0, 1.0, 2.0),
                ],
            ])),
            properties: vec![],
            native: None,
        }]);
        let bytes = write(&fc);
        let back = crate::flatgeobuf::read(&bytes).unwrap();
        assert_eq!(back.features[0].geometry, fc.features[0].geometry);
    }

    fn hostile() -> FeatureCollection {
        crate::flatgeobuf::read(include_bytes!("../../tests/fixtures/gdal_hostile.fgb")).unwrap()
    }

    fn fgb_native(fc: &FeatureCollection) -> crate::native::FgbNative {
        match fc.native.as_ref().and_then(|n| n.payload_for(Format::FlatGeobuf)) {
            Some(NativePayload::FlatGeobuf(f)) => f.clone(),
            other => panic!("expected a FlatGeobuf native, got {other:?}"),
        }
    }

    /// The header members `gdal_hostile.fgb` actually carries, round-tripped.
    /// All three are real `ogr2ogr` output, not synthesized: the reader had no
    /// constant for `metadata` at all, and the writer used to put the literal
    /// `geosetta` in the name slot.
    #[test]
    fn a_real_header_s_name_metadata_and_crs_name_survive_a_round_trip() {
        let source = hostile();
        assert_eq!(source.name.as_deref(), Some("cities"));
        let native = fgb_native(&source);
        assert!(native.metadata.as_deref().unwrap().contains("surveyed 2019"), "{native:?}");
        assert_eq!(native.crs_name.as_deref(), Some("WGS 84"));

        let back = crate::flatgeobuf::read(&write(&source)).unwrap();
        assert_eq!(back.name, source.name);
        assert_eq!(fgb_native(&back), native);
        assert_eq!(back.crs, source.crs);
    }

    /// Defect 4: the writer hardcoded `geosetta` into the header's name slot,
    /// so no layer name could survive an FGB transit — which is what let a
    /// GeoPackage edit loop append a duplicate layer instead of replacing one.
    #[test]
    fn the_header_name_is_the_collection_s_own_not_a_constant() {
        let mut fc = FeatureCollection::new(vec![Feature::default()]);
        fc.name = Some("cities".into());
        assert_eq!(crate::flatgeobuf::read(&write(&fc)).unwrap().name.as_deref(), Some("cities"));

        // A nameless collection writes no name rather than inventing one:
        // naming an output is the caller's job.
        let anonymous = FeatureCollection::new(vec![Feature::default()]);
        assert_eq!(crate::flatgeobuf::read(&write(&anonymous)).unwrap().name, None);
    }

    /// The whole descriptive surface, including the slots no observed tool
    /// populates — carried because the spec has them, not because GDAL used
    /// them.
    #[test]
    fn every_descriptive_header_and_column_member_round_trips() {
        let mut fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![(std::rc::Rc::from("a"), crate::json::JsonValue::String("x".into()))],
            native: None,
        }]);
        let native = crate::native::FgbNative {
            title: Some("Cities".into()),
            description: Some("a description".into()),
            metadata: Some(r#"{"k":"v"}"#.into()),
            crs_name: Some("WGS 84".into()),
            crs_description: Some("the ensemble".into()),
            crs_code_string: Some("IGNF:LAMB93".into()),
            columns: vec![crate::native::FgbColumnNative {
                name: "a".into(),
                title: Some("Column A".into()),
                description: Some("what A means".into()),
                metadata: Some(r#"{"unit":"m"}"#.into()),
            }],
        };
        fc.native = Some(crate::Native::new(
            Format::FlatGeobuf,
            NativePayload::FlatGeobuf(native.clone()),
        ));
        assert_eq!(fgb_native(&crate::flatgeobuf::read(&write(&fc)).unwrap()), native);
    }

    /// The tag check, at the spoke rather than in `native.rs`: a payload from
    /// another format must not reach these slots.
    #[test]
    fn a_geojson_native_is_not_replayed_into_a_flatgeobuf_header() {
        let mut fc = FeatureCollection::new(vec![Feature::default()]);
        fc.native = Some(crate::Native::new(
            Format::GeoJson,
            NativePayload::GeoJson(vec![(std::rc::Rc::from("id"), crate::json::JsonValue::Null)]),
        ));
        assert_eq!(crate::flatgeobuf::read(&write(&fc)).unwrap().native, None);
    }

    /// D3 at the header: a collection with nothing unmodelled allocates no
    /// payload and writes no extra slots.
    #[test]
    fn a_plain_collection_carries_no_flatgeobuf_native() {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![],
            native: None,
        }]);
        assert_eq!(crate::flatgeobuf::read(&write(&fc)).unwrap().native, None);
    }

    /// A FlatGeobuf column declaration survives a write/read cycle. This is
    /// what makes FGB usable as *transit*: it is the only format in the crate
    /// that carries a schema between processes.
    #[test]
    fn a_column_declaration_round_trips_through_the_header() {
        let mut fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![
                (std::rc::Rc::from("name"), crate::json::JsonValue::String("Zürich".into())),
                (std::rc::Rc::from("price"), crate::json::JsonValue::Float(1.5)),
            ],
            native: None,
        }]);
        fc.schema = vec![
            crate::schema::FieldDecl {
                name: "name".into(),
                width: Some(80),
                nullable: Some(false),
                unique: Some(true),
                primary_key: Some(true),
                ..Default::default()
            },
            crate::schema::FieldDecl {
                name: "price".into(),
                precision: Some(24),
                scale: Some(15),
                ..Default::default()
            },
        ];
        let back = crate::flatgeobuf::read(&write(&fc)).unwrap();
        // Every column also comes back with a declared *type* now, which the
        // input did not set — the header always states one, so reading it is
        // how an `Int16` or a `DateTime` survives (defect 5). Compare the six
        // slots this test is about, with the type normalised away.
        let without_ty: Vec<_> = back
            .schema
            .iter()
            .map(|d| crate::schema::FieldDecl { ty: None, ..d.clone() })
            .collect();
        assert_eq!(without_ty, fc.schema);
        assert_eq!(back.schema[0].ty, Some(crate::schema::DeclType::String));
        assert_eq!(back.schema[1].ty, Some(crate::schema::DeclType::Float64));
    }

    /// The declaration slots cost nothing when there is nothing to declare —
    /// D3's rule, applied to the header rather than to a native.
    #[test]
    fn an_undeclared_column_writes_no_declaration_slots() {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![(std::rc::Rc::from("name"), crate::json::JsonValue::String("x".into()))],
            native: None,
        }]);
        let bytes = write(&fc);
        // The schema is not *empty* — the header always declares a type, and
        // the reader keeps it. What costs nothing is the six declaration slots.
        let back = crate::flatgeobuf::read(&bytes).unwrap();
        assert_eq!(back.schema.len(), 1);
        assert_eq!(
            back.schema[0],
            crate::schema::FieldDecl {
                name: "name".into(),
                ty: Some(crate::schema::DeclType::String),
                ..Default::default()
            }
        );
        // A declaration-free file is byte-identical to what this writer
        // produced before the slots existed, since each `add_*` is given the
        // spec's default and a default costs no vtable entry.
        let mut declared = fc.clone();
        declared.schema =
            vec![crate::schema::FieldDecl { name: "name".into(), width: Some(80), ..Default::default() }];
        assert!(write(&declared).len() > bytes.len());
    }

    #[test]
    fn z_bearing_multipolygon_round_trips_independent_part_dimensionality() {
        // Each MultiPolygon part derives its own z-presence independently
        // (mirroring wkb.rs's per-part dimension derivation) — one part is
        // Z-bearing, the other stays 2D.
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::MultiPolygon(vec![
                vec![vec![
                    Position::with_z(0.0, 0.0, 5.0),
                    Position::with_z(1.0, 0.0, 5.0),
                    Position::with_z(1.0, 1.0, 5.0),
                    Position::with_z(0.0, 0.0, 5.0),
                ]],
                vec![vec![
                    Position::new(5.0, 5.0),
                    Position::new(6.0, 5.0),
                    Position::new(6.0, 6.0),
                    Position::new(5.0, 5.0),
                ]],
            ])),
            properties: vec![],
            native: None,
        }]);
        let bytes = write(&fc);
        let back = crate::flatgeobuf::read(&bytes).unwrap();
        assert_eq!(back.features[0].geometry, fc.features[0].geometry);
    }

    /// **Features go out in the order they arrived, and the file is still
    /// indexed.** The two halves have to be asserted together: this writer
    /// sorted for the index's sake until 2026-10-21, so a test proving only
    /// the order would be satisfied by dropping the index, and one proving
    /// only the index would be satisfied by sorting again.
    ///
    /// The input is deliberately *anti*-sorted — descending by y, so Hilbert
    /// locality is as wrong as it can be — because an already-ordered input
    /// cannot tell a writer that preserves order from one that re-sorts. That
    /// is exactly how the old behaviour stayed invisible: it is idempotent
    /// after one hop, so it only shows against a *foreign* producer's order,
    /// which `tests/same_format.rs`'s D6 registry finally supplied.
    #[test]
    fn features_keep_their_input_order_and_the_file_is_still_indexed() {
        let anti_sorted: Vec<Feature> = (0..40)
            .map(|i| Feature {
                geometry: Some(Geometry::Point(Position::new(
                    f64::from(i % 7) * 13.0 - 40.0,
                    80.0 - f64::from(i) * 3.0,
                ))),
                properties: vec![("i".into(), crate::json::JsonValue::Int(i64::from(i)))],
                native: None,
            })
            .collect();
        let fc = FeatureCollection::new(anti_sorted);

        let bytes = write(&fc);
        let back = crate::flatgeobuf::read(&bytes).unwrap();

        let ids: Vec<i64> = back
            .features
            .iter()
            .map(|f| f.properties[0].1.as_i64().unwrap())
            .collect();
        assert_eq!(ids, (0..40).collect::<Vec<i64>>(), "features were reordered");

        // And the index is there: `index_node_size` is the default 16, which an
        // index-less file writes as 0.
        let table = crate::flatbuffers::Table::root(&bytes[8..]).unwrap();
        assert_eq!(
            table.read_u16(header::INDEX_NODE_SIZE, 16).unwrap(),
            NODE_SIZE,
            "the file came out index-less",
        );
    }

    #[test]
    fn a_2d_point_writes_no_z_or_m_vector() {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![],
            native: None,
        }]);
        let bytes = write(&fc);
        let back = crate::flatgeobuf::read(&bytes).unwrap();
        assert_eq!(back.features[0].geometry, Some(Geometry::Point(Position::new(1.0, 2.0))));
    }

    fn one_point(crs: Option<crate::crs::Crs>) -> FeatureCollection {
        let mut fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![],
            native: None,
        }]);
        fc.crs = crs;
        fc
    }

    #[test]
    fn wgs84_crs_round_trips() {
        use crate::crs::Crs;
        let bytes = write(&one_point(Some(Crs::Wgs84)));
        assert_eq!(crate::flatgeobuf::read(&bytes).unwrap().crs, Some(Crs::Wgs84));
    }

    #[test]
    fn named_crs_round_trips() {
        use crate::crs::{Crs, NamedCrs};
        let crs = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: Some("PROJCS[\"Web Mercator\"]".into()),
            projjson: None,
        });
        let bytes = write(&one_point(Some(crs)));
        match crate::flatgeobuf::read(&bytes).unwrap().crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("3857"));
                assert_eq!(n.wkt.as_deref(), Some("PROJCS[\"Web Mercator\"]"));
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn no_crs_stays_none() {
        let bytes = write(&one_point(None));
        assert_eq!(crate::flatgeobuf::read(&bytes).unwrap().crs, None);
    }

    #[test]
    fn alphanumeric_code_is_dropped_but_org_and_wkt_survive() {
        use crate::crs::{Crs, NamedCrs};
        // FlatGeobuf's Crs.code is int32 on the wire; a non-numeric code
        // (IGNF/OGC/PROJ/NKG-style) can't fit, so it's dropped rather than
        // mangled — but the authority and WKT, which *can* be represented,
        // still round-trip.
        let crs = Crs::Named(NamedCrs {
            authority: Some("IGNF".into()),
            code: Some("LAMB93".into()),
            wkt: Some("PROJCS[\"RGF93 Lambert 93\"]".into()),
            projjson: None,
        });
        let bytes = write(&one_point(Some(crs)));
        match crate::flatgeobuf::read(&bytes).unwrap().crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority.as_deref(), Some("IGNF"));
                assert_eq!(n.code, None);
                assert_eq!(n.wkt.as_deref(), Some("PROJCS[\"RGF93 Lambert 93\"]"));
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn id_less_projjson_round_trips_via_structural_translation() {
        use crate::crs::{Crs, NamedCrs};
        // The gap `plans/projjson-to-wkt.org` closes: a GeoParquet source
        // whose PROJJSON carries no authority code at all still reaches
        // FlatGeobuf's WKT slot via `NamedCrs::structural_wkt`.
        let pj = r#"{"type":"GeographicCRS","name":"custom","datum":{"type":"GeodeticReferenceFrame","name":"custom datum","ellipsoid":{"name":"custom ellipsoid","semi_major_axis":6378137,"inverse_flattening":298.257223563}}}"#;
        let crs = Crs::Named(NamedCrs {
            authority: None,
            code: None,
            wkt: None,
            projjson: Some(pj.into()),
        });
        let bytes = write(&one_point(Some(crs)));
        match crate::flatgeobuf::read(&bytes).unwrap().crs {
            Some(Crs::Named(n)) => {
                let wkt = n.wkt.expect("structural translation produced a WKT string");
                assert!(wkt.starts_with("GEOGCS[\"custom\""), "{wkt}");
                assert!(wkt.contains("SPHEROID[\"custom ellipsoid\",6378137,298.257223563]"), "{wkt}");
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }
}

