//! Encode a [`FeatureCollection`] as FlatGeobuf, using the from-scratch
//! [`crate::flatbuffers::Builder`]. Writes an index-less file
//! (`index_node_size = 0`); building the packed Hilbert R-tree is a deferred
//! enhancement. See `plans/flatgeobuf.org`.

use crate::feature::FeatureCollection;
use crate::flatbuffers::Builder;
use crate::geometry::{Bbox, Geometry, Position};
use crate::schema::{Cell, ColumnType, infer_columns};

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
    pub const CRS: usize = 10;
    pub const NUM_FIELDS: usize = 11;
}
mod crs {
    pub const ORG: usize = 0;
    pub const CODE: usize = 1;
    pub const WKT: usize = 4;
    pub const NUM_FIELDS: usize = 6;
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

    // A packed Hilbert R-tree needs a bbox per feature, so index only when every
    // feature has geometry; otherwise write index-less in input order.
    let feature_bboxes: Vec<Bbox> = fc.features.iter().map(feature_bbox).collect();
    let indexed = !fc.features.is_empty() && fc.features.iter().all(|f| f.geometry.is_some());
    let order: Vec<usize> = if indexed {
        crate::spatial::hilbert_order(&feature_bboxes)
    } else {
        (0..fc.features.len()).collect()
    };
    let node_size = if indexed { NODE_SIZE } else { 0 };

    // Encode features in `order`, recording each framed feature's byte offset
    // within the feature section (for the R-tree leaf pointers).
    let mut feature_section = Vec::new();
    let mut offsets = Vec::with_capacity(order.len());
    for &idx in &order {
        offsets.push(feature_section.len() as u64);
        let bytes = build_feature(fc.features[idx].geometry.as_ref(), &columns, idx);
        feature_section.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        feature_section.extend_from_slice(&bytes);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    let header = build_header(
        &columns,
        &bbox,
        header_type,
        fc.features.len() as u64,
        node_size,
        fc.crs.as_ref(),
    );
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    if indexed {
        let ordered: Vec<Bbox> = order.iter().map(|&i| feature_bboxes[i]).collect();
        out.extend_from_slice(&packed_rtree(&ordered, &offsets, node_size));
    }
    out.extend_from_slice(&feature_section);
    out
}

fn feature_bbox(f: &crate::feature::Feature) -> Bbox {
    f.geometry.as_ref().map(|g| g.bbox()).unwrap_or_else(Bbox::empty)
}

fn build_header(
    columns: &[crate::schema::Column],
    bbox: &Bbox,
    header_type: u8,
    features_count: u64,
    node_size: u16,
    crs: Option<&crate::crs::Crs>,
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
    let name = b.create_string("geosetta");
    let envelope = (!bbox.is_empty())
        .then(|| b.create_f64_vector(&[bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y]));
    // The CRS sub-table (built before the header that references it), carrying
    // whatever the source recorded through unchanged.
    let crs_off = build_crs(&mut b, crs);

    b.start_table(header::NUM_FIELDS);
    b.add_offset(header::NAME, name);
    if let Some(env) = envelope {
        b.add_offset(header::ENVELOPE, env);
    }
    b.add_u8(header::GEOMETRY_TYPE, header_type, gtype::UNKNOWN);
    b.add_offset(header::COLUMNS, columns_vec);
    b.add_u64(header::FEATURES_COUNT, features_count, 0);
    // Default index_node_size is 16, so an indexed file (16) omits the field and
    // an index-less one writes 0.
    b.add_u16(header::INDEX_NODE_SIZE, node_size, 16);
    if let Some(off) = crs_off {
        b.add_offset(header::CRS, off);
    }
    let root = b.end_table();
    b.finish(root)
}

/// Build the FlatGeobuf `Crs` sub-table, returning its offset, or `None` when
/// the collection has no CRS (so the header omits the field). WGS 84 is written
/// as EPSG:4326, the spelling GDAL uses.
fn build_crs(b: &mut Builder, crs: Option<&crate::crs::Crs>) -> Option<usize> {
    use crate::crs::Crs;
    // FlatGeobuf's Crs.code is int32 on the wire — genuinely numeric, unlike the
    // IR's string code (which also holds IGNF/OGC/PROJ/NKG's alphanumeric
    // codes). A non-numeric code can't be represented here; it falls back to
    // the same "no code" 0/unset sentinel as below, same as GeoPackage's
    // synthetic-id fallback — org and wkt still carry through.
    let (org, code, wkt) = match crs? {
        Crs::Wgs84 => (Some("EPSG".to_string()), Some(4326), None),
        Crs::Named(n) => (
            n.authority.clone(),
            n.code.as_deref().and_then(|c| c.parse::<i64>().ok()),
            n.wkt.clone().or_else(|| n.structural_wkt()),
        ),
    };
    // Nothing worth recording.
    if org.is_none() && code.is_none() && wkt.is_none() {
        return None;
    }
    let org_off = org.as_deref().map(|s| b.create_string(s));
    let wkt_off = wkt.as_deref().map(|s| b.create_string(s));
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
                geometry: Some(Geometry::Point([(i % 10) as f64, (i / 10) as f64])),
                properties: vec![(
                    "id".into(),
                    JsonValue::Number { value: i as f64, is_int: true },
                )],
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

    fn one_point(crs: Option<crate::crs::Crs>) -> FeatureCollection {
        let mut fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point([1.0, 2.0])),
            properties: vec![],
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
