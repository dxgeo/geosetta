//! Orchestrates a GeoJSON → GeoParquet conversion.

use std::collections::BTreeSet;

use crate::error::Result;
use crate::geometry::{to_wkb, Bbox};
use crate::{geojson, json, parquet};

/// Convert the text of a GeoJSON document into GeoParquet bytes.
pub fn geojson_to_geoparquet(input: &str) -> Result<Vec<u8>> {
    let value = json::parse(input)?;
    let fc = geojson::from_json(&value)?;

    // Property columns (schema inferred by scanning all features).
    let columns = parquet::infer_columns(&fc.features);

    // Geometry column: WKB per feature, plus bbox and the set of types.
    let mut bbox = Bbox::empty();
    let mut types: BTreeSet<&'static str> = BTreeSet::new();
    let mut geometry: Vec<Option<Vec<u8>>> = Vec::with_capacity(fc.features.len());
    for feature in &fc.features {
        match &feature.geometry {
            Some(g) => {
                g.extend_bbox(&mut bbox);
                types.insert(g.type_name());
                geometry.push(Some(to_wkb(g)));
            }
            None => geometry.push(None),
        }
    }

    let type_names: Vec<String> = types.into_iter().map(String::from).collect();
    let geo = parquet::geo_metadata(&type_names, &bbox);

    Ok(parquet::write_geoparquet(&columns, &geometry, &geo))
}
