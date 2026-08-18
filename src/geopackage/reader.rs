//! Decode a GeoPackage into its layers.

use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::from_wkb;
use crate::json::JsonValue;
use crate::sqlite::{Database, Table, Value};
use std::rc::Rc;

/// Read every feature layer from a GeoPackage as `(layer_name, features)`.
pub fn read_layers(bytes: &[u8]) -> Result<Vec<(String, FeatureCollection)>> {
    let db = Database::open(bytes)?;
    let tables = db.tables()?;

    // gpkg_contents lists every dataset; keep the ones with data_type='features'.
    let feature_tables = feature_layer_names(&db, &tables)?;
    // gpkg_geometry_columns says which column holds geometry (and its SRS) per
    // table; gpkg_spatial_ref_sys defines each SRS.
    let geom_columns = geometry_columns(&db, &tables)?;
    let srs = spatial_ref_systems(&db, &tables)?;

    let mut layers = Vec::new();
    for name in feature_tables {
        let Some(table) = tables.iter().find(|t| t.name == name) else {
            continue;
        };
        let geom = geom_columns.iter().find(|g| g.table == name);
        let geom_idx =
            geom.and_then(|g| table.columns.iter().position(|c| *c == g.column));

        let mut fc = read_layer(&db, table, geom_idx)?;
        // Carry the layer's coordinate reference system through unchanged.
        fc.crs = geom.and_then(|g| resolve_crs(g.srs_id, &srs));
        layers.push((name, fc));
    }
    Ok(layers)
}

/// A row of `gpkg_geometry_columns`: which column holds geometry, in which SRS.
struct GeomColumn {
    table: String,
    column: String,
    srs_id: i64,
}

/// A row of `gpkg_spatial_ref_sys`.
struct SrsRow {
    srs_id: i64,
    organization: String,
    organization_coordsys_id: i64,
    definition: String,
}

/// Turn a layer's `srs_id` into a [`Crs`], looking its definition up in
/// `gpkg_spatial_ref_sys`. The two GeoPackage "undefined" SRSes (`0`, `-1`) and
/// an unknown `srs_id` mean no CRS; EPSG:4326 collapses to [`Crs::Wgs84`]; every
/// other row is carried through as a [`Crs::Named`] with its WKT `definition`.
fn resolve_crs(srs_id: i64, srs: &[SrsRow]) -> Option<Crs> {
    if srs_id == 0 || srs_id == -1 {
        return None;
    }
    let row = srs.iter().find(|s| s.srs_id == srs_id)?;
    let wkt = (row.definition != "undefined" && !row.definition.is_empty())
        .then(|| row.definition.clone());
    Some(Crs::from_authority_code(
        Some(row.organization.clone()),
        Some(row.organization_coordsys_id.to_string()),
        wkt,
        None,
    ))
}

/// Read `gpkg_spatial_ref_sys` into rows. Absent (a malformed GeoPackage) is
/// treated as no known SRS definitions.
fn spatial_ref_systems(db: &Database, tables: &[Table]) -> Result<Vec<SrsRow>> {
    let Some(t) = tables.iter().find(|t| t.name == "gpkg_spatial_ref_sys") else {
        return Ok(Vec::new());
    };
    let id_col = column_index(t, "srs_id")?;
    let org_col = column_index(t, "organization")?;
    let org_id_col = column_index(t, "organization_coordsys_id")?;
    let def_col = column_index(t, "definition")?;

    let mut out = Vec::new();
    for row in db.read_rows(t)? {
        // srs_id is an INTEGER PRIMARY KEY (rowid alias); the reader has already
        // backfilled the rowid into that column, so it reads as a plain Int.
        let srs_id = match row.get(id_col) {
            Some(Value::Int(n)) => *n,
            _ => continue,
        };
        let text = |i: usize| match row.get(i) {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let org_id = match row.get(org_id_col) {
            Some(Value::Int(n)) => *n,
            _ => srs_id,
        };
        out.push(SrsRow {
            srs_id,
            organization: text(org_col),
            organization_coordsys_id: org_id,
            definition: text(def_col),
        });
    }
    Ok(out)
}

fn read_layer(db: &Database, table: &Table, geom_idx: Option<usize>) -> Result<FeatureCollection> {
    let rowid_alias = table.rowid_alias();
    let mut features = Vec::new();
    // Intern each column's key once so every row shares one `Rc`.
    let keys: Vec<Rc<str>> = table.columns.iter().map(|n| Rc::from(n.as_str())).collect();

    for row in db.read_rows(table)? {
        let geometry = match geom_idx.and_then(|i| row.get(i)) {
            Some(Value::Blob(b)) => Some(from_wkb(strip_gpkg_header(b)?)?),
            _ => None,
        };
        // Every column except the geometry and the rowid-alias becomes a property.
        let properties = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != geom_idx && Some(*i) != rowid_alias)
            .map(|(i, key)| (Rc::clone(key), value_to_json(&row[i])))
            .collect();
        features.push(Feature {
            geometry,
            properties,
        });
    }
    // The caller fills in `crs` from the layer's srs_id.
    Ok(FeatureCollection::new(features))
}

/// Names of tables whose `gpkg_contents.data_type` is `features`.
fn feature_layer_names(db: &Database, tables: &[Table]) -> Result<Vec<String>> {
    let contents = tables
        .iter()
        .find(|t| t.name == "gpkg_contents")
        .ok_or_else(|| Error::Convert("geopackage: missing gpkg_contents".into()))?;
    let name_col = column_index(contents, "table_name")?;
    let type_col = column_index(contents, "data_type")?;

    let mut names = Vec::new();
    for row in db.read_rows(contents)? {
        if let (Some(Value::Text(name)), Some(Value::Text(kind))) =
            (row.get(name_col), row.get(type_col))
            && kind == "features"
        {
            names.push(name.clone());
        }
    }
    Ok(names)
}

/// Geometry column + SRS per table, from `gpkg_geometry_columns`.
fn geometry_columns(db: &Database, tables: &[Table]) -> Result<Vec<GeomColumn>> {
    let Some(gc) = tables.iter().find(|t| t.name == "gpkg_geometry_columns") else {
        return Ok(Vec::new());
    };
    let table_col = column_index(gc, "table_name")?;
    let column_col = column_index(gc, "column_name")?;
    let srs_col = column_index(gc, "srs_id")?;

    let mut out = Vec::new();
    for row in db.read_rows(gc)? {
        if let (Some(Value::Text(t)), Some(Value::Text(c))) =
            (row.get(table_col), row.get(column_col))
        {
            let srs_id = match row.get(srs_col) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            out.push(GeomColumn {
                table: t.clone(),
                column: c.clone(),
                srs_id,
            });
        }
    }
    Ok(out)
}

fn column_index(table: &Table, name: &str) -> Result<usize> {
    table
        .columns
        .iter()
        .position(|c| c == name)
        .ok_or_else(|| Error::Convert(format!("geopackage: {} has no {name} column", table.name)))
}

/// Strip the GeoPackage Binary header, returning the wrapped WKB. Layout:
/// `"GP"`, version, flags, srs_id (4), then an optional envelope, then WKB.
fn strip_gpkg_header(blob: &[u8]) -> Result<&[u8]> {
    if blob.len() < 8 || &blob[0..2] != b"GP" {
        return Err(Error::Convert("geopackage: bad geometry blob header".into()));
    }
    let flags = blob[3];
    let envelope_bytes = match (flags >> 1) & 0x07 {
        0 => 0,
        1 => 32, // XY
        2 => 48, // XYZ
        3 => 48, // XYM
        4 => 64, // XYZM
        _ => return Err(Error::Convert("geopackage: invalid envelope flag".into())),
    };
    let header_len = 8 + envelope_bytes;
    blob.get(header_len..)
        .ok_or_else(|| Error::Convert("geopackage: truncated geometry blob".into()))
}

fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Int(n) => JsonValue::Number {
            value: *n as f64,
            is_int: true,
        },
        Value::Real(f) => JsonValue::Number {
            value: *f,
            is_int: false,
        },
        Value::Text(s) => JsonValue::String(s.clone()),
        // Non-geometry blobs have no GeoJSON representation.
        Value::Blob(_) => JsonValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Geometry, Position};

    #[test]
    fn reads_a_geopackage_layer() {
        // DuckDB-written GPKG, layer "places" (fid/geom/name/pop/score).
        let bytes = include_bytes!("../../tests/fixtures/points.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers.len(), 1);
        let (name, fc) = &layers[0];
        assert_eq!(name, "places");
        assert_eq!(fc.features.len(), 2);

        // Geometry: GPKG-binary header stripped, WKB decoded.
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::new(10.0, 20.0))));
        // Properties exclude the geometry column and the fid rowid-alias.
        let props: Vec<&str> = fc.features[0].properties.iter().map(|(k, _)| &**k).collect();
        assert_eq!(props, vec!["name", "pop", "score"]);
        assert_eq!(fc.features[1].properties[0].1.as_str(), Some("Beta"));
        assert_eq!(fc.features[1].properties[1].1.as_f64(), Some(200.0));
    }

    #[test]
    fn reads_a_real_envelope_bearing_z_fixture() {
        // Real `ogr2ogr -f GPKG` output, found after `strip_gpkg_header`'s
        // envelope-size branch turned out to have no fixture coverage at
        // all: a `Point`'s degenerate bbox gets no envelope from GDAL, but
        // a real `LineString Z` does — flags=0x05, envelope indicator 2
        // (XYZ, 48 bytes), envelope values (0,20,0,10,0,10) confirmed by
        // hand-decoding the raw file to exactly match the geometry's true
        // bounds before committing this fixture. This is the real-fixture
        // exercise of the envelope-skip path the synthetic test below
        // stood in for.
        let bytes = include_bytes!("../../tests/fixtures/gdal_linestring_z.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(
            layers[0].1.features[0].geometry,
            Some(Geometry::LineString(vec![
                Position::with_z(0.0, 0.0, 0.0),
                Position::with_z(10.0, 10.0, 10.0),
                Position::with_z(20.0, 5.0, 3.0),
            ]))
        );
    }

    #[test]
    fn reads_a_real_m_bearing_fixture() {
        // GeoPackage was M5's "zero production code changes" milestone —
        // `strip_gpkg_header`/`from_wkb` are dimension-agnostic wrappers, so
        // M should already work for free the same way Z did, purely because
        // the WKB codec underneath (M2) is dimension-generic. Never had a
        // fixture confirming that until now. Sourced via `ogr2ogr -f GPKG`
        // from a `LINESTRING ZM (...)` WKT/CSV source (GDAL has no direct
        // GeoJSON-to-M path, since GeoJSON itself has no M concept — see
        // `plans/zm-geometry.org`'s M7 M follow-up for the same sourcing
        // trick applied to Shapefile). Confirmed via direct SQLite blob
        // inspection before trusting the fixture: the GPB header's envelope
        // indicator is 2 (XYZ, 48 bytes — GDAL's envelope only ever tracks
        // Z, not M, even for a ZM geometry) and the inner WKB's own type
        // code is `0x0BBA` = 3002 = ISO SFA `LineString` + 3000 (ZM),
        // matching `ogrinfo`/DuckDB both independently reading it back as
        // `LINESTRING ZM (0 0 1 10,10 10 2 20,20 5 3 30)`.
        let bytes = include_bytes!("../../tests/fixtures/gdal_linestring_zm.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(
            layers[0].1.features[0].geometry,
            Some(Geometry::LineString(vec![
                Position::with_zm(0.0, 0.0, 1.0, 10.0),
                Position::with_zm(10.0, 10.0, 2.0, 20.0),
                Position::with_zm(20.0, 5.0, 3.0, 30.0),
            ]))
        );
    }

    #[test]
    fn strip_gpkg_header_skips_every_envelope_size_per_the_spec_table() {
        // GeoPackage §2.1.3's envelope contents indicator code: 0 = none (0
        // bytes), 1 = XY (32), 2 = XYZ (48), 3 = XYM (48), 4 = XYZM (64).
        // Indicator 2 (XYZ) is now also covered by a real fixture (above);
        // this test still exercises every code, including the two (XYM,
        // XYZM) no real tool tried here happened to produce.
        let wkb = crate::geometry::to_wkb(&Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));
        for (indicator, envelope_len) in [(0u8, 0usize), (1, 32), (2, 48), (3, 48), (4, 64)] {
            let mut blob = Vec::new();
            blob.extend_from_slice(b"GP");
            blob.push(0); // version
            blob.push(0x01 | (indicator << 1)); // flags: LE + this envelope size
            blob.extend_from_slice(&0i32.to_le_bytes()); // srs_id
            blob.resize(blob.len() + envelope_len, 0); // dummy envelope bytes
            blob.extend_from_slice(&wkb);
            let stripped = strip_gpkg_header(&blob).unwrap();
            assert_eq!(stripped, wkb.as_slice(), "indicator {indicator} (envelope {envelope_len} bytes)");
        }
    }
}
