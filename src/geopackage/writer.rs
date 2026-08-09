//! Write GeoPackage files: build the required GPKG metadata tables plus one
//! feature table per layer, and serialize the whole thing with the from-scratch
//! [`crate::sqlite`] writer. Append is read-modify-write — read the existing
//! layers, upsert, and rewrite the complete file (see `plans/geopackage.org`).

use crate::error::Result;
use crate::feature::FeatureCollection;
use crate::geometry::{to_wkb, Bbox, Geometry};
use crate::schema::{infer_columns, Cell, ColumnType};
use crate::sqlite::{MasterEntry, TableSpec, Value, write_database};

use super::reader::read_layers;
use super::rtree;

const APPLICATION_ID: u32 = 0x4750_4B47; // "GPKG"
const USER_VERSION: u32 = 10200; // 1.2.0
const SRS_ID: i64 = 4326;
const LAST_CHANGE: &str = "1970-01-01T00:00:00.000Z";

const WGS84_WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4326\"]]";

/// Upsert `new_layers` into an existing GeoPackage (or a fresh one if `existing`
/// is `None`), returning the complete file. Layers of the same name are
/// replaced. With `rtree`, every layer gets a SQLite R*Tree spatial index (the
/// opt-in GeoPackage RTree extension).
pub fn write_layers(
    existing: Option<&[u8]>,
    new_layers: &[(String, FeatureCollection)],
    rtree: bool,
) -> Result<Vec<u8>> {
    let mut layers: Vec<(String, FeatureCollection)> = match existing {
        Some(bytes) => read_layers(bytes)?,
        None => Vec::new(),
    };
    for (name, fc) in new_layers {
        layers.retain(|(n, _)| n != name);
        layers.push((name.clone(), fc.clone()));
    }
    build(&layers, rtree)
}

fn build(layers: &[(String, FeatureCollection)], rtree: bool) -> Result<Vec<u8>> {
    let mut specs = vec![spatial_ref_sys(), gpkg_contents(layers), geometry_columns(layers)];
    if rtree {
        let names: Vec<String> = layers.iter().map(|(n, _)| n.clone()).collect();
        specs.push(rtree::extensions_table(&names));
    }
    for (name, fc) in layers {
        specs.push(feature_table(name, fc));
    }

    // The spatial indexes (shadow tables) and their virtual-table + trigger
    // schema entries, if requested. Shadow tables are appended after the
    // feature tables so their b-trees are built alongside the rest.
    let mut master: Vec<MasterEntry> = Vec::new();
    if rtree {
        for (name, fc) in layers {
            let index = rtree::build(name, fc);
            specs.extend(index.tables);
            master.extend(index.master);
        }
    }

    write_database(&specs, &master, APPLICATION_ID, USER_VERSION)
}

fn spatial_ref_sys() -> TableSpec {
    // srs_id is INTEGER PRIMARY KEY, so its column value is Null and the rowid
    // carries it; rows must be rowid-ordered (-1, 0, 4326).
    let row = |srs_id: i64, name: &str, org: &str, org_id: i64, def: &str, desc: &str| {
        (
            srs_id,
            vec![
                Value::Text(name.into()),
                Value::Null,
                Value::Text(org.into()),
                Value::Int(org_id),
                Value::Text(def.into()),
                Value::Text(desc.into()),
            ],
        )
    };
    TableSpec {
        name: "gpkg_spatial_ref_sys".into(),
        sql: "CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT NOT NULL, srs_id INTEGER NOT NULL PRIMARY KEY, organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL, definition TEXT NOT NULL, description TEXT)".into(),
        rows: vec![
            row(-1, "Undefined cartesian SRS", "NONE", -1, "undefined", "undefined cartesian"),
            row(0, "Undefined geographic SRS", "NONE", 0, "undefined", "undefined geographic"),
            row(SRS_ID, "WGS 84 geodetic", "EPSG", 4326, WGS84_WKT, "WGS 84"),
        ],
    }
}

fn gpkg_contents(layers: &[(String, FeatureCollection)]) -> TableSpec {
    let rows = layers
        .iter()
        .enumerate()
        .map(|(i, (name, fc))| {
            let (minx, miny, maxx, maxy) = bbox(fc);
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("features".into()),
                    Value::Text(name.clone()),
                    Value::Text(String::new()),
                    Value::Text(LAST_CHANGE.into()),
                    minx,
                    miny,
                    maxx,
                    maxy,
                    Value::Int(SRS_ID),
                ],
            )
        })
        .collect();
    TableSpec {
        name: "gpkg_contents".into(),
        // No TEXT PRIMARY KEY / UNIQUE: those would require auto-indexes we do
        // not build. The columns are otherwise the GPKG-standard ones.
        sql: "CREATE TABLE gpkg_contents (table_name TEXT NOT NULL, data_type TEXT NOT NULL, identifier TEXT, description TEXT, last_change DATETIME NOT NULL, min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE, srs_id INTEGER)".into(),
        rows,
    }
}

fn geometry_columns(layers: &[(String, FeatureCollection)]) -> TableSpec {
    let rows = layers
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("geom".into()),
                    Value::Text("GEOMETRY".into()),
                    Value::Int(SRS_ID),
                    Value::Int(0),
                    Value::Int(0),
                ],
            )
        })
        .collect();
    TableSpec {
        name: "gpkg_geometry_columns".into(),
        sql: "CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, z TINYINT NOT NULL, m TINYINT NOT NULL)".into(),
        rows,
    }
}

fn feature_table(name: &str, fc: &FeatureCollection) -> TableSpec {
    let columns = infer_columns(&fc.features);

    // DDL: fid (rowid), geom, then one column per property.
    let mut sql = format!("CREATE TABLE {} (\"fid\" INTEGER PRIMARY KEY NOT NULL, \"geom\" GEOMETRY", quote(name));
    for col in &columns {
        sql.push_str(&format!(", {} {}", quote(&col.name), sqlite_type(col.ty)));
    }
    sql.push(')');

    let rows = fc
        .features
        .iter()
        .enumerate()
        .map(|(i, feat)| {
            let mut values = Vec::with_capacity(columns.len() + 2);
            values.push(Value::Null); // fid, carried by the rowid
            values.push(match &feat.geometry {
                Some(g) => Value::Blob(gpkg_geometry(g)),
                None => Value::Null,
            });
            for col in &columns {
                values.push(cell_to_value(&col.values[i]));
            }
            ((i + 1) as i64, values)
        })
        .collect();

    TableSpec {
        name: name.to_string(),
        sql,
        rows,
    }
}

/// GeoPackage Binary: an 8-byte header ("GP", version, flags, LE srs_id, no
/// envelope) wrapping standard WKB.
fn gpkg_geometry(g: &Geometry) -> Vec<u8> {
    let wkb = to_wkb(g);
    let mut out = Vec::with_capacity(8 + wkb.len());
    out.extend_from_slice(b"GP");
    out.push(0); // version
    out.push(0x01); // flags: little-endian header ints, no envelope
    out.extend_from_slice(&(SRS_ID as i32).to_le_bytes());
    out.extend_from_slice(&wkb);
    out
}

fn bbox(fc: &FeatureCollection) -> (Value, Value, Value, Value) {
    let mut b = Bbox::empty();
    for f in &fc.features {
        if let Some(g) = &f.geometry {
            g.extend_bbox(&mut b);
        }
    }
    if b.is_empty() {
        (Value::Null, Value::Null, Value::Null, Value::Null)
    } else {
        (
            Value::Real(b.min_x),
            Value::Real(b.min_y),
            Value::Real(b.max_x),
            Value::Real(b.max_y),
        )
    }
}

fn sqlite_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Int64 => "INTEGER",
        ColumnType::Double => "REAL",
        ColumnType::String => "TEXT",
    }
}

fn cell_to_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Int(*b as i64),
        Cell::Int(n) => Value::Int(*n),
        Cell::Double(d) => Value::Real(*d),
        Cell::Str(s) => Value::Text(s.clone()),
    }
}

/// Quote a SQL identifier, doubling any embedded quotes.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::Feature;
    use crate::json::JsonValue;

    fn point(x: f64, y: f64) -> Option<Geometry> {
        Some(Geometry::Point([x, y]))
    }
    fn fc(features: Vec<Feature>) -> FeatureCollection {
        FeatureCollection { features }
    }

    #[test]
    fn write_then_read_round_trips() {
        let layer = fc(vec![
            Feature {
                geometry: point(1.0, 2.0),
                properties: vec![
                    ("name".into(), JsonValue::String("a".into())),
                    ("n".into(), JsonValue::Number { value: 5.0, is_int: true }),
                ],
            },
            Feature {
                geometry: point(3.0, 4.0),
                properties: vec![
                    ("name".into(), JsonValue::String("b".into())),
                    ("n".into(), JsonValue::Null),
                ],
            },
        ]);
        let bytes = write_layers(None, &[("places".into(), layer)], false).unwrap();
        let back = read_layers(&bytes).unwrap();

        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "places");
        assert_eq!(back[0].1.features.len(), 2);
        assert_eq!(back[0].1.features[0].geometry, point(1.0, 2.0));
        // fid is excluded from properties.
        let names: Vec<&str> = back[0].1.features[0].properties.iter().map(|(k, _)| &**k).collect();
        assert_eq!(names, vec!["name", "n"]);
        assert_eq!(back[0].1.features[0].properties[1].1.as_f64(), Some(5.0));
    }

    #[test]
    fn rtree_index_is_emitted_and_round_trips() {
        // A layer big enough to force interior rtree nodes (> one node of cells)
        // and a multi-page sqlite_master (many trigger rows).
        let feats: Vec<Feature> = (0..120)
            .map(|i| Feature { geometry: point(i as f64, (i % 7) as f64), properties: vec![] })
            .collect();
        let bytes = write_layers(None, &[("grid".into(), fc(feats))], true).unwrap();

        // The virtual table, its shadow tables, and gpkg_extensions are present.
        use crate::sqlite::Database;
        let db = Database::open(&bytes).unwrap();
        let names: Vec<String> = db.tables().unwrap().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "gpkg_extensions"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_node"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_rowid"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_parent"));
        // The rtree virtual table itself has rootpage 0, so our table reader
        // (which needs a real root) skips it — a layer read still sees only the
        // feature layer.
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "grid");
        assert_eq!(back[0].1.features.len(), 120);
    }

    #[test]
    fn append_adds_a_layer_then_upsert_replaces() {
        let a = fc(vec![Feature { geometry: point(0.0, 0.0), properties: vec![] }]);
        let b = fc(vec![
            Feature { geometry: point(1.0, 1.0), properties: vec![] },
            Feature { geometry: point(2.0, 2.0), properties: vec![] },
        ]);

        let g1 = write_layers(None, &[("a".into(), a)], false).unwrap();
        let g2 = write_layers(Some(&g1), &[("b".into(), b)], false).unwrap();
        let layers = read_layers(&g2).unwrap();
        assert_eq!(layers.len(), 2);
        assert!(layers.iter().any(|(n, _)| n == "a"));
        assert!(layers.iter().any(|(n, l)| n == "b" && l.features.len() == 2));

        // Upsert "a" with three features -> still two layers, "a" replaced.
        let a2 = fc(vec![Feature { geometry: point(9.0, 9.0), properties: vec![] }; 3]);
        let g3 = write_layers(Some(&g2), &[("a".into(), a2)], false).unwrap();
        let layers = read_layers(&g3).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers.iter().find(|(n, _)| n == "a").unwrap().1.features.len(), 3);
    }
}
