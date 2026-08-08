//! Decode a GeoPackage into its layers.

use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::from_wkb;
use crate::json::JsonValue;
use crate::sqlite::{Database, Table, Value};

/// Read every feature layer from a GeoPackage as `(layer_name, features)`.
pub fn read_layers(bytes: &[u8]) -> Result<Vec<(String, FeatureCollection)>> {
    let db = Database::open(bytes)?;
    let tables = db.tables()?;

    // gpkg_contents lists every dataset; keep the ones with data_type='features'.
    let feature_tables = feature_layer_names(&db, &tables)?;
    // gpkg_geometry_columns says which column holds geometry per table.
    let geom_columns = geometry_columns(&db, &tables)?;

    let mut layers = Vec::new();
    for name in feature_tables {
        let Some(table) = tables.iter().find(|t| t.name == name) else {
            continue;
        };
        let geom_idx = geom_columns
            .iter()
            .find(|(t, _)| *t == name)
            .and_then(|(_, col)| table.columns.iter().position(|c| c == col));

        let fc = read_layer(&db, table, geom_idx)?;
        layers.push((name, fc));
    }
    Ok(layers)
}

fn read_layer(db: &Database, table: &Table, geom_idx: Option<usize>) -> Result<FeatureCollection> {
    let rowid_alias = table.rowid_alias();
    let mut features = Vec::new();

    for row in db.read_rows(table)? {
        let geometry = match geom_idx.and_then(|i| row.get(i)) {
            Some(Value::Blob(b)) => Some(from_wkb(strip_gpkg_header(b)?)?),
            _ => None,
        };
        // Every column except the geometry and the rowid-alias becomes a property.
        let properties = table
            .columns
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != geom_idx && Some(*i) != rowid_alias)
            .map(|(i, name)| (name.clone(), value_to_json(&row[i])))
            .collect();
        features.push(Feature {
            geometry,
            properties,
        });
    }
    Ok(FeatureCollection { features })
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

/// `(table_name, geometry_column_name)` pairs from `gpkg_geometry_columns`.
fn geometry_columns(db: &Database, tables: &[Table]) -> Result<Vec<(String, String)>> {
    let Some(gc) = tables.iter().find(|t| t.name == "gpkg_geometry_columns") else {
        return Ok(Vec::new());
    };
    let table_col = column_index(gc, "table_name")?;
    let column_col = column_index(gc, "column_name")?;

    let mut out = Vec::new();
    for row in db.read_rows(gc)? {
        if let (Some(Value::Text(t)), Some(Value::Text(c))) =
            (row.get(table_col), row.get(column_col))
        {
            out.push((t.clone(), c.clone()));
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
    use crate::geometry::Geometry;

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
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point([10.0, 20.0])));
        // Properties exclude the geometry column and the fid rowid-alias.
        let props: Vec<&str> = fc.features[0].properties.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(props, vec!["name", "pop", "score"]);
        assert_eq!(fc.features[1].properties[0].1.as_str(), Some("Beta"));
        assert_eq!(fc.features[1].properties[1].1.as_f64(), Some(200.0));
    }
}
