//! Orchestrates conversions between GeoJSON and GeoParquet, both directions.

use std::collections::BTreeSet;

use crate::error::Result;
use crate::geojson::{Feature, FeatureCollection};
use crate::geometry::{from_wkb, to_wkb, Bbox};
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

/// Convert GeoParquet bytes back into GeoJSON text.
pub fn geoparquet_to_geojson(bytes: &[u8]) -> Result<String> {
    let parsed = parquet::read_geoparquet(bytes)?;

    let mut features = Vec::with_capacity(parsed.num_rows);
    for row in 0..parsed.num_rows {
        let geometry = match &parsed.geometry[row] {
            Some(wkb) => Some(from_wkb(wkb)?),
            None => None,
        };
        // Rebuild each feature's properties from the columns, in column order.
        let properties = parsed
            .properties
            .iter()
            .map(|col| (col.name.clone(), col.values[row].clone()))
            .collect();
        features.push(Feature {
            geometry,
            properties,
        });
    }

    let fc = FeatureCollection { features };
    Ok(geojson::to_json(&fc).to_json_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "type": "FeatureCollection",
        "features": [
            {"type": "Feature",
             "geometry": {"type": "Point", "coordinates": [-73.9857, 40.7484]},
             "properties": {"name": "Empire State", "height_m": 381, "landmark": true, "rating": 4.7}},
            {"type": "Feature",
             "geometry": {"type": "LineString", "coordinates": [[-73.99,40.75],[-73.98,40.76]]},
             "properties": {"name": "A Path", "landmark": false, "rating": 3.2}},
            {"type": "Feature",
             "geometry": {"type": "Polygon", "coordinates": [[[-74,40.7],[-73.95,40.7],[-73.95,40.75],[-74,40.7]]]},
             "properties": {"name": "A Zone", "height_m": 12, "rating": 5}},
            {"type": "Feature",
             "geometry": {"type": "Point", "coordinates": [-73.968, 40.7851]},
             "properties": {"name": "Café ☕", "landmark": true, "tags": ["a", "b"]}}
        ]
    }"#;

    #[test]
    fn read_recovers_geometry_and_properties() {
        let pq = geojson_to_geoparquet(SAMPLE).unwrap();
        let back = geoparquet_to_geojson(&pq).unwrap();
        let fc = geojson::from_json(&json::parse(&back).unwrap()).unwrap();

        assert_eq!(fc.features.len(), 4);
        // Geometry survives the WKB round trip.
        assert_eq!(
            fc.features[0].geometry,
            Some(crate::geometry::Geometry::Point([-73.9857, 40.7484]))
        );
        // A present string property, including non-ASCII.
        let cafe = &fc.features[3];
        let name = cafe.properties.iter().find(|(k, _)| k == "name").unwrap();
        assert_eq!(name.1.as_str(), Some("Café ☕"));
        // The nested-array property fell back to a JSON string on the way in,
        // so it comes back as that string.
        let tags = cafe.properties.iter().find(|(k, _)| k == "tags").unwrap();
        assert_eq!(tags.1.as_str(), Some("[\"a\",\"b\"]"));
    }

    #[test]
    fn round_trip_is_byte_stable() {
        // geojson -> parquet -> geojson -> parquet reproduces the same bytes:
        // schema inference and encoding are deterministic and the reader is a
        // faithful inverse.
        let pq1 = geojson_to_geoparquet(SAMPLE).unwrap();
        let geojson = geoparquet_to_geojson(&pq1).unwrap();
        let pq2 = geojson_to_geoparquet(&geojson).unwrap();
        assert_eq!(pq1, pq2);
    }
}
