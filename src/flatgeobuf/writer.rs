//! Encode a [`FeatureCollection`] as FlatGeobuf, using the from-scratch
//! [`crate::flatbuffers::Builder`]. Writes an index-less file
//! (`index_node_size = 0`); building the packed Hilbert R-tree is a deferred
//! enhancement. See `plans/flatgeobuf.org`.

use crate::feature::FeatureCollection;
use crate::flatbuffers::Builder;
use crate::geometry::{Bbox, Geometry, Position};
use crate::schema::{Cell, ColumnType, infer_columns};

const MAGIC: [u8; 8] = [b'f', b'g', b'b', 3, b'f', b'g', b'b', 1];

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

// ColumnType enum (ubyte).
mod ctype {
    pub const BOOL: u8 = 2;
    pub const LONG: u8 = 7;
    pub const DOUBLE: u8 = 10;
    pub const STRING: u8 = 11;
}

// Field indices.
mod header {
    pub const NAME: usize = 0;
    pub const ENVELOPE: usize = 1;
    pub const GEOMETRY_TYPE: usize = 2;
    pub const COLUMNS: usize = 7;
    pub const FEATURES_COUNT: usize = 8;
    pub const INDEX_NODE_SIZE: usize = 9;
    pub const NUM_FIELDS: usize = 10;
}
mod column {
    pub const NAME: usize = 0;
    pub const TYPE: usize = 1;
    pub const NUM_FIELDS: usize = 2;
}
mod feature {
    pub const GEOMETRY: usize = 0;
    pub const PROPERTIES: usize = 1;
    pub const NUM_FIELDS: usize = 2;
}
mod geometry {
    pub const ENDS: usize = 0;
    pub const XY: usize = 1;
    pub const TYPE: usize = 6;
    pub const PARTS: usize = 7;
    pub const NUM_FIELDS: usize = 8;
}

/// Serialize a feature collection to FlatGeobuf bytes.
pub fn write(fc: &FeatureCollection) -> Vec<u8> {
    // Columns (name + type) inferred by scanning all features; `values[row]` is
    // this row's cell for each column.
    let columns = infer_columns(&fc.features);

    // Bounding box and the geometry-type set across all features.
    let mut bbox = Bbox::empty();
    let mut types = std::collections::BTreeSet::new();
    for f in &fc.features {
        if let Some(g) = &f.geometry {
            g.extend_bbox(&mut bbox);
            types.insert(fgb_geometry_type(g));
        }
    }
    // A single shared type if uniform, else Unknown (per-feature type is always
    // written too).
    let header_type = if types.len() == 1 {
        *types.iter().next().unwrap()
    } else {
        gtype::UNKNOWN
    };

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);

    let header = build_header(&columns, &bbox, header_type, fc.features.len() as u64);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    // No spatial index (index_node_size = 0).

    for (row, feat) in fc.features.iter().enumerate() {
        let bytes = build_feature(feat.geometry.as_ref(), &columns, row);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    out
}

fn build_header(
    columns: &[crate::schema::Column],
    bbox: &Bbox,
    header_type: u8,
    features_count: u64,
) -> Vec<u8> {
    let mut b = Builder::new();

    // Column sub-tables first (referenced objects precede their referrers).
    let mut col_offsets = Vec::with_capacity(columns.len());
    for col in columns {
        let name = b.create_string(&col.name);
        b.start_table(column::NUM_FIELDS);
        b.add_offset(column::NAME, name);
        b.add_u8(column::TYPE, fgb_column_type(col.ty), 255);
        col_offsets.push(b.end_table());
    }
    let columns_vec = b.create_offset_vector(&col_offsets);
    let name = b.create_string("pantograph");
    let envelope = (!bbox.is_empty())
        .then(|| b.create_f64_vector(&[bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y]));

    b.start_table(header::NUM_FIELDS);
    b.add_offset(header::NAME, name);
    if let Some(env) = envelope {
        b.add_offset(header::ENVELOPE, env);
    }
    b.add_u8(header::GEOMETRY_TYPE, header_type, gtype::UNKNOWN);
    b.add_offset(header::COLUMNS, columns_vec);
    b.add_u64(header::FEATURES_COUNT, features_count, 0);
    // Default index_node_size is 16; write 0 to declare "no index".
    b.add_u16(header::INDEX_NODE_SIZE, 0, 16);
    let root = b.end_table();
    b.finish(root)
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
/// (xy / ends / parts) are built before the table.
fn encode_geometry(b: &mut Builder, g: &Geometry) -> usize {
    let ty = fgb_geometry_type(g);
    match g {
        Geometry::Point(p) => {
            let xy = b.create_f64_vector(&[p[0], p[1]]);
            geom_table(b, ty, None, Some(xy), None)
        }
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => {
            let xy = b.create_f64_vector(&flatten(ps));
            geom_table(b, ty, None, Some(xy), None)
        }
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            let xy = b.create_f64_vector(&flatten_rings(rings));
            let ends = (rings.len() > 1).then(|| b.create_u32_vector(&ring_ends(rings)));
            geom_table(b, ty, ends, Some(xy), None)
        }
        Geometry::MultiPolygon(polys) => {
            let part_offsets: Vec<usize> = polys
                .iter()
                .map(|rings| {
                    let xy = b.create_f64_vector(&flatten_rings(rings));
                    let ends = (rings.len() > 1).then(|| b.create_u32_vector(&ring_ends(rings)));
                    geom_table(b, gtype::POLYGON, ends, Some(xy), None)
                })
                .collect();
            let parts = b.create_offset_vector(&part_offsets);
            geom_table(b, ty, None, None, Some(parts))
        }
        Geometry::GeometryCollection(geoms) => {
            let part_offsets: Vec<usize> = geoms.iter().map(|g| encode_geometry(b, g)).collect();
            let parts = b.create_offset_vector(&part_offsets);
            geom_table(b, ty, None, None, Some(parts))
        }
    }
}

fn geom_table(
    b: &mut Builder,
    ty: u8,
    ends: Option<usize>,
    xy: Option<usize>,
    parts: Option<usize>,
) -> usize {
    b.start_table(geometry::NUM_FIELDS);
    b.add_u8(geometry::TYPE, ty, gtype::UNKNOWN);
    if let Some(e) = ends {
        b.add_offset(geometry::ENDS, e);
    }
    if let Some(xy) = xy {
        b.add_offset(geometry::XY, xy);
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
        match (col.ty, cell) {
            (ColumnType::Bool, Cell::Bool(v)) => blob.push(*v as u8),
            (ColumnType::Int64, Cell::Int(v)) => blob.extend_from_slice(&v.to_le_bytes()),
            (ColumnType::Double, Cell::Double(v)) => blob.extend_from_slice(&v.to_le_bytes()),
            (ColumnType::String, Cell::Str(s)) => {
                blob.extend_from_slice(&(s.len() as u32).to_le_bytes());
                blob.extend_from_slice(s.as_bytes());
            }
            // infer_columns guarantees the cell matches the column type.
            _ => unreachable!("cell type does not match column type"),
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

fn fgb_column_type(ty: ColumnType) -> u8 {
    match ty {
        ColumnType::Bool => ctype::BOOL,
        ColumnType::Int64 => ctype::LONG,
        ColumnType::Double => ctype::DOUBLE,
        ColumnType::String => ctype::STRING,
    }
}

fn flatten(ps: &[Position]) -> Vec<f64> {
    ps.iter().flat_map(|p| [p[0], p[1]]).collect()
}

fn flatten_rings(rings: &[Vec<Position>]) -> Vec<f64> {
    rings.iter().flat_map(|r| flatten(r)).collect()
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
