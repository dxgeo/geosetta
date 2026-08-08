//! The GeoPackage RTree extension: SQLite's R*Tree virtual table, written from
//! scratch (opt-in `--rtree`). See `plans/spatial-index.org` milestone D.
//!
//! A GeoPackage spatial index is a SQLite `rtree` virtual table named
//! `rtree_<table>_<geom>` whose data lives in three shadow tables — `_node`
//! (the tree, one blob per node), `_rowid` (feature rowid → leaf node), and
//! `_parent` (node → parent node). We build a *packed* R-tree bottom-up over
//! the features' bounding boxes (Hilbert-ordered for locality, reusing
//! [`crate::spatial`]), which needs no incremental insertion.
//!
//! Node blob format (matching `rtree.c`, all big-endian): a 4-byte header —
//! 2-byte tree depth (only meaningful on the root) then 2-byte cell count —
//! followed by fixed-size cells. Each cell is an 8-byte integer (the feature
//! rowid in a leaf, the child node number in an interior node) plus four 32-bit
//! floats `minx, maxx, miny, maxy`. Coordinates are rounded *outward* when
//! narrowing f64 → f32 so every stored box still contains its geometry. Nodes
//! are zero-padded to a fixed size; SQLite reads that size from the root blob's
//! length, so every node must be ≤ it — we make them all equal.

use crate::feature::FeatureCollection;
use crate::geometry::Bbox;
use crate::sqlite::{MasterEntry, TableSpec, Value};

/// Fixed node blob size — SQLite's own default for a 4 KiB page and 2-D float
/// coordinates (`4 + 24 * 51`).
const NODE_SIZE: usize = 1228;
/// Bytes per cell: an 8-byte rowid/child pointer plus four 4-byte floats.
const CELL_BYTES: usize = 24;
/// Cells that fit one node after the 4-byte header.
const MAX_CELLS: usize = (NODE_SIZE - 4) / CELL_BYTES;

/// The SQLite artifacts implementing one layer's spatial index: the shadow
/// tables (real b-trees) and the schema-only virtual-table + trigger entries.
pub struct Rtree {
    pub tables: Vec<TableSpec>,
    pub master: Vec<MasterEntry>,
}

/// Build the R*Tree extension for feature table `table` (geometry column
/// `geom`), or `None` if the layer has no geometry to index (an empty index is
/// still emitted so downstream tools find the virtual table).
pub fn build(table: &str, fc: &FeatureCollection) -> Rtree {
    // One leaf entry per feature that has geometry: (rowid, bbox). The rowid is
    // the feature's fid — its 1-based position, matching the feature table.
    let mut entries: Vec<(i64, Bbox)> = Vec::new();
    for (i, feat) in fc.features.iter().enumerate() {
        if let Some(g) = &feat.geometry {
            let b = g.bbox();
            if !b.is_empty() {
                entries.push(((i + 1) as i64, b));
            }
        }
    }
    // Hilbert-order the entries for leaf locality (query pruning); the rowid
    // mapping keeps them tied to their features regardless of order.
    let bboxes: Vec<Bbox> = entries.iter().map(|(_, b)| *b).collect();
    let order = crate::spatial::hilbert_order(&bboxes);
    let ordered: Vec<(i64, [f32; 4])> = order
        .into_iter()
        .map(|i| {
            let (id, b) = entries[i];
            (id, round_out(&b))
        })
        .collect();

    let levels = pack(&ordered);
    serialize(table, &levels)
}

/// A node during construction: its cells plus the union of their boxes.
struct Node {
    entries: Vec<Ent>,
    bbox: [f32; 4],
    nodeno: u32,
}

/// A cell: a leaf holds a feature rowid + its box; an interior cell points at a
/// child node by its `(level, index)` position (resolved to a node number once
/// numbers are assigned).
enum Ent {
    Leaf(i64, [f32; 4]),
    Child(usize, usize),
}

/// Pack entries into a bottom-up tree: `levels[0]` are leaves, the last level is
/// the single root. Assigns node numbers (root = 1) before returning.
fn pack(entries: &[(i64, [f32; 4])]) -> Vec<Vec<Node>> {
    let mut levels: Vec<Vec<Node>> = Vec::new();

    // Leaf level (always at least one node, even when empty — the root must
    // exist so SQLite can read the tree depth and node size).
    let mut leaves = Vec::new();
    if entries.is_empty() {
        leaves.push(Node { entries: Vec::new(), bbox: [0.0; 4], nodeno: 0 });
    } else {
        for chunk in entries.chunks(MAX_CELLS) {
            let mut bbox = EMPTY;
            let ents = chunk
                .iter()
                .map(|(id, c)| {
                    bbox = union(bbox, *c);
                    Ent::Leaf(*id, *c)
                })
                .collect();
            leaves.push(Node { entries: ents, bbox, nodeno: 0 });
        }
    }
    levels.push(leaves);

    // Interior levels until a single root remains.
    while levels.last().unwrap().len() > 1 {
        let below = levels.len() - 1;
        let n = levels[below].len();
        let mut parents = Vec::new();
        let mut i = 0;
        while i < n {
            let end = (i + MAX_CELLS).min(n);
            let mut bbox = EMPTY;
            let mut ents = Vec::new();
            // `j` indexes the level below and is stored in each cell, so this is
            // an index loop rather than an iterator.
            #[allow(clippy::needless_range_loop)]
            for j in i..end {
                bbox = union(bbox, levels[below][j].bbox);
                ents.push(Ent::Child(below, j));
            }
            parents.push(Node { entries: ents, bbox, nodeno: 0 });
            i = end;
        }
        levels.push(parents);
    }

    // Number the nodes: the root is always node 1; the rest follow.
    let top = levels.len() - 1;
    levels[top][0].nodeno = 1;
    let mut next = 2u32;
    for l in (0..top).rev() {
        for node in &mut levels[l] {
            node.nodeno = next;
            next += 1;
        }
    }
    levels
}

/// Serialize the packed tree into the `_node`/`_rowid`/`_parent` shadow tables
/// and the virtual-table + trigger schema entries.
fn serialize(table: &str, levels: &[Vec<Node>]) -> Rtree {
    let depth = (levels.len() - 1) as u16;
    let rtree_name = format!("rtree_{table}_geom");

    let mut node_rows: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut rowid_rows: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut parent_rows: Vec<(i64, Vec<Value>)> = Vec::new();

    for (l, level) in levels.iter().enumerate() {
        for node in level {
            let mut blob = vec![0u8; NODE_SIZE];
            // Header: depth (root only) + cell count.
            let d = if node.nodeno == 1 { depth } else { 0 };
            blob[0..2].copy_from_slice(&d.to_be_bytes());
            blob[2..4].copy_from_slice(&(node.entries.len() as u16).to_be_bytes());

            for (k, ent) in node.entries.iter().enumerate() {
                let off = 4 + k * CELL_BYTES;
                let (id, coords) = match ent {
                    Ent::Leaf(id, c) => (*id, *c),
                    Ent::Child(cl, ci) => {
                        let child = &levels[*cl][*ci];
                        // Record the child's parent link while we're here.
                        parent_rows
                            .push((child.nodeno as i64, vec![Value::Null, Value::Int(node.nodeno as i64)]));
                        (child.nodeno as i64, child.bbox)
                    }
                };
                blob[off..off + 8].copy_from_slice(&id.to_be_bytes());
                for (m, coord) in coords.iter().enumerate() {
                    let o = off + 8 + m * 4;
                    blob[o..o + 4].copy_from_slice(&coord.to_be_bytes());
                }
            }
            node_rows.push((node.nodeno as i64, vec![Value::Null, Value::Blob(blob)]));

            // Leaf entries map each feature rowid to this node.
            if l == 0 {
                for ent in &node.entries {
                    if let Ent::Leaf(id, _) = ent {
                        rowid_rows.push((*id, vec![Value::Null, Value::Int(node.nodeno as i64)]));
                    }
                }
            }
        }
    }

    // Shadow-table rows must be rowid-ordered.
    node_rows.sort_by_key(|(k, _)| *k);
    rowid_rows.sort_by_key(|(k, _)| *k);
    parent_rows.sort_by_key(|(k, _)| *k);

    let tables = vec![
        TableSpec {
            name: format!("{rtree_name}_node"),
            sql: format!("CREATE TABLE \"{rtree_name}_node\"(nodeno INTEGER PRIMARY KEY,data)"),
            rows: node_rows,
        },
        TableSpec {
            name: format!("{rtree_name}_rowid"),
            sql: format!("CREATE TABLE \"{rtree_name}_rowid\"(rowid INTEGER PRIMARY KEY,nodeno)"),
            rows: rowid_rows,
        },
        TableSpec {
            name: format!("{rtree_name}_parent"),
            sql: format!("CREATE TABLE \"{rtree_name}_parent\"(nodeno INTEGER PRIMARY KEY,parentnode)"),
            rows: parent_rows,
        },
    ];

    let mut master = vec![MasterEntry {
        kind: "table".into(),
        name: rtree_name.clone(),
        tbl_name: rtree_name.clone(),
        sql: format!("CREATE VIRTUAL TABLE \"{rtree_name}\" USING rtree(id, minx, maxx, miny, maxy)"),
    }];
    for (suffix, sql) in triggers(table, &rtree_name) {
        master.push(MasterEntry {
            kind: "trigger".into(),
            name: format!("{rtree_name}_{suffix}"),
            tbl_name: table.into(),
            sql,
        });
    }

    Rtree { tables, master }
}

/// The GeoPackage-standard RTree maintenance triggers keeping the index in sync
/// with edits (they call `ST_*` SQL functions provided by GeoPackage-aware
/// readers; plain SQLite parses but never fires them). `fid`/`geom` are our
/// fixed primary-key and geometry column names.
fn triggers(table: &str, rtree: &str) -> Vec<(&'static str, String)> {
    let ins = |name: &str, when: &str| {
        format!(
            "CREATE TRIGGER \"{name}\" AFTER {when} ON \"{table}\" \
             WHEN (new.\"geom\" NOT NULL AND NOT ST_IsEmpty(NEW.\"geom\")) \
             BEGIN INSERT OR REPLACE INTO \"{rtree}\" VALUES (\
             NEW.\"fid\",ST_MinX(NEW.\"geom\"),ST_MaxX(NEW.\"geom\"),\
             ST_MinY(NEW.\"geom\"),ST_MaxY(NEW.\"geom\")); END"
        )
    };
    let full_insert = format!(
        "INSERT OR REPLACE INTO \"{rtree}\" VALUES (\
         NEW.\"fid\",ST_MinX(NEW.\"geom\"),ST_MaxX(NEW.\"geom\"),\
         ST_MinY(NEW.\"geom\"),ST_MaxY(NEW.\"geom\"));"
    );
    vec![
        ("insert", ins(&format!("{rtree}_insert"), "INSERT")),
        (
            "update1",
            format!(
                "CREATE TRIGGER \"{rtree}_update1\" AFTER UPDATE OF \"geom\" ON \"{table}\" \
                 WHEN OLD.\"fid\"=NEW.\"fid\" AND \
                 (NEW.\"geom\" NOTNULL AND NOT ST_IsEmpty(NEW.\"geom\")) \
                 BEGIN {full_insert} END"
            ),
        ),
        (
            "update2",
            format!(
                "CREATE TRIGGER \"{rtree}_update2\" AFTER UPDATE OF \"geom\" ON \"{table}\" \
                 WHEN OLD.\"fid\"=NEW.\"fid\" AND \
                 (NEW.\"geom\" ISNULL OR ST_IsEmpty(NEW.\"geom\")) \
                 BEGIN DELETE FROM \"{rtree}\" WHERE id=OLD.\"fid\"; END"
            ),
        ),
        (
            "update3",
            format!(
                "CREATE TRIGGER \"{rtree}_update3\" AFTER UPDATE ON \"{table}\" \
                 WHEN OLD.\"fid\"!=NEW.\"fid\" AND \
                 (NEW.\"geom\" NOTNULL AND NOT ST_IsEmpty(NEW.\"geom\")) \
                 BEGIN DELETE FROM \"{rtree}\" WHERE id=OLD.\"fid\"; {full_insert} END"
            ),
        ),
        (
            "update4",
            format!(
                "CREATE TRIGGER \"{rtree}_update4\" AFTER UPDATE ON \"{table}\" \
                 WHEN OLD.\"fid\"!=NEW.\"fid\" AND \
                 (NEW.\"geom\" ISNULL OR ST_IsEmpty(NEW.\"geom\")) \
                 BEGIN DELETE FROM \"{rtree}\" WHERE id IN (OLD.\"fid\",NEW.\"fid\"); END"
            ),
        ),
        (
            "delete",
            format!(
                "CREATE TRIGGER \"{rtree}_delete\" AFTER DELETE ON \"{table}\" \
                 WHEN old.\"geom\" NOT NULL \
                 BEGIN DELETE FROM \"{rtree}\" WHERE id=OLD.\"fid\"; END"
            ),
        ),
    ]
}

/// The `gpkg_extensions` DDL and one registration row per indexed layer,
/// declaring the RTree extension.
pub fn extensions_table(layers: &[String]) -> TableSpec {
    let rows = layers
        .iter()
        .enumerate()
        .map(|(i, name)| {
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("geom".into()),
                    Value::Text("gpkg_rtree_index".into()),
                    Value::Text("http://www.geopackage.org/spec120/#extension_rtree".into()),
                    Value::Text("write-only".into()),
                ],
            )
        })
        .collect();
    TableSpec {
        name: "gpkg_extensions".into(),
        sql: "CREATE TABLE gpkg_extensions (table_name TEXT, column_name TEXT, extension_name TEXT NOT NULL, definition TEXT NOT NULL, scope TEXT NOT NULL)".into(),
        rows,
    }
}

// --- coordinate rounding ---------------------------------------------------

const EMPTY: [f32; 4] = [f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY];

/// Union of two `[minx, maxx, miny, maxy]` boxes.
fn union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0].min(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].max(b[3])]
}

/// A feature bbox narrowed to f32, rounded *outward* so the f32 box still
/// contains the true f64 box (mins toward −∞, maxs toward +∞).
fn round_out(b: &Bbox) -> [f32; 4] {
    [
        round_down(b.min_x),
        round_up(b.max_x),
        round_down(b.min_y),
        round_up(b.max_y),
    ]
}

/// Largest f32 ≤ `v` (the nearest f32, nudged down if it rounded up).
fn round_down(v: f64) -> f32 {
    let f = v as f32;
    if f as f64 > v { f.next_down() } else { f }
}

/// Smallest f32 ≥ `v` (the nearest f32, nudged up if it rounded down).
fn round_up(v: f64) -> f32 {
    let f = v as f32;
    if (f as f64) < v { f.next_up() } else { f }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::Feature;
    use crate::geometry::Geometry;

    fn point_feat(x: f64, y: f64) -> Feature {
        Feature { geometry: Some(Geometry::Point([x, y])), properties: vec![] }
    }

    #[test]
    fn rounds_boxes_outward() {
        // 0.1 has no exact f32/f64 representation; the outward-rounded box must
        // still bracket the f64 value.
        let b = Bbox { min_x: 0.1, min_y: 0.1, max_x: 0.1, max_y: 0.1 };
        let [minx, maxx, miny, maxy] = round_out(&b);
        assert!((minx as f64) <= 0.1 && (maxx as f64) >= 0.1);
        assert!((miny as f64) <= 0.1 && (maxy as f64) >= 0.1);
    }

    #[test]
    fn empty_layer_still_has_a_root_node() {
        let fc = FeatureCollection { features: vec![] };
        let rt = build("empty", &fc);
        let node = rt.tables.iter().find(|t| t.name == "rtree_empty_geom_node").unwrap();
        assert_eq!(node.rows.len(), 1); // the (empty) root
        assert_eq!(node.rows[0].0, 1); // nodeno 1
    }

    #[test]
    fn single_leaf_holds_every_feature() {
        let fc = FeatureCollection {
            features: (0..10).map(|i| point_feat(i as f64, i as f64)).collect(),
        };
        let rt = build("pts", &fc);
        let node = rt.tables.iter().find(|t| t.name == "rtree_pts_geom_node").unwrap();
        let rowid = rt.tables.iter().find(|t| t.name == "rtree_pts_geom_rowid").unwrap();
        // Ten features fit one leaf, which is the root.
        assert_eq!(node.rows.len(), 1);
        assert_eq!(rowid.rows.len(), 10);
        // Root header: depth 0, ten cells.
        let Value::Blob(blob) = &node.rows[0].1[1] else { panic!() };
        assert_eq!(u16::from_be_bytes([blob[0], blob[1]]), 0);
        assert_eq!(u16::from_be_bytes([blob[2], blob[3]]), 10);
    }

    #[test]
    fn many_features_build_interior_levels() {
        // More than one node's worth forces a root over multiple leaves.
        let n = MAX_CELLS * 3 + 5;
        let fc = FeatureCollection {
            features: (0..n).map(|i| point_feat(i as f64, (i * 7 % 13) as f64)).collect(),
        };
        let rt = build("big", &fc);
        let node = rt.tables.iter().find(|t| t.name == "rtree_big_geom_node").unwrap();
        let parent = rt.tables.iter().find(|t| t.name == "rtree_big_geom_parent").unwrap();
        let rowid = rt.tables.iter().find(|t| t.name == "rtree_big_geom_rowid").unwrap();

        assert_eq!(rowid.rows.len(), n); // every feature indexed
        // 4 leaves + 1 root = 5 nodes; the 4 leaves each have a parent link.
        assert_eq!(node.rows.len(), 5);
        assert_eq!(parent.rows.len(), 4);
        // Root (nodeno 1) has depth 1 and one cell per leaf.
        let root = node.rows.iter().find(|(k, _)| *k == 1).unwrap();
        let Value::Blob(blob) = &root.1[1] else { panic!() };
        assert_eq!(u16::from_be_bytes([blob[0], blob[1]]), 1); // depth
        assert_eq!(u16::from_be_bytes([blob[2], blob[3]]), 4); // four children
    }
}
