//! Orchestrates conversions between GeoJSON and GeoParquet, both directions.

use std::collections::BTreeSet;

use crate::error::Result;
use crate::feature::{Feature, FeatureCollection};
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

    /// Shared checker for the DuckDB dictionary-encoded fixtures. Both were
    /// generated from the same table so the expected values are identical; they
    /// differ only in the page compression codec.
    ///   COPY (SELECT (i%7)::BIGINT bucket, ('color_'||(i%3)) color,
    ///                (i%2=0) even, ST_Point((i%4), (i%4)*2) geometry
    ///         FROM range(5000) t(i)) TO '...' (FORMAT PARQUET[, COMPRESSION ZSTD]);
    fn check_duckdb_fixture(bytes: &[u8]) {
        let out = geoparquet_to_geojson(bytes).unwrap();
        let fc = geojson::from_json(&json::parse(&out).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 5000);

        let prop = |f: &Feature, k: &str| {
            f.properties
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        for i in [0usize, 1, 3, 2499, 4999] {
            let f = &fc.features[i];
            assert_eq!(prop(f, "bucket").as_f64(), Some((i % 7) as f64));
            assert_eq!(prop(f, "color").as_str(), Some(format!("color_{}", i % 3).as_str()));
            assert_eq!(prop(f, "even"), crate::json::JsonValue::Bool(i % 2 == 0));
            let g = (i % 4) as f64;
            assert_eq!(
                f.geometry,
                Some(crate::geometry::Geometry::Point([g, g * 2.0]))
            );
        }
    }

    #[test]
    fn reads_duckdb_dictionary_snappy() {
        check_duckdb_fixture(include_bytes!("../tests/fixtures/duckdb_dict.parquet"));
    }

    #[test]
    fn reads_duckdb_dictionary_zstd() {
        // Same data, ZSTD page compression — exercises the from-scratch zstd
        // decoder through the full pipeline.
        check_duckdb_fixture(include_bytes!("../tests/fixtures/duckdb_zstd.parquet"));
    }

    #[test]
    fn reads_duckdb_dictionary_gzip() {
        // Same data, GZIP page compression — exercises the from-scratch
        // gzip/DEFLATE decoder through the full pipeline.
        check_duckdb_fixture(include_bytes!("../tests/fixtures/duckdb_gzip.parquet"));
    }

    #[test]
    fn reads_duckdb_dictionary_lz4() {
        // Same data, LZ4_RAW page compression — exercises the from-scratch lz4
        // block decoder through the full pipeline.
        check_duckdb_fixture(include_bytes!("../tests/fixtures/duckdb_lz4.parquet"));
    }

    #[test]
    fn reads_date_and_timestamp_columns() {
        // DuckDB file with a DATE (INT32) and TIMESTAMP_MICROS (INT64) column,
        // which the reader renders as ISO 8601 strings:
        //   COPY (SELECT DATE '2020-01-01' + i::INTEGER d,
        //                TIMESTAMP '2020-01-01' + (i*INTERVAL 1 HOUR) ts,
        //                ST_Point(i,0) geometry FROM range(300) t(i)) TO ...
        let bytes = include_bytes!("../tests/fixtures/duckdb_dates.parquet");
        let out = geoparquet_to_geojson(bytes).unwrap();
        let fc = geojson::from_json(&json::parse(&out).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 300);

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap()
        };
        let f0 = &fc.features[0];
        assert_eq!(prop(f0, "d").as_str(), Some("2020-01-01"));
        assert_eq!(prop(f0, "ts").as_str(), Some("2020-01-01T00:00:00"));
        let f2 = &fc.features[2];
        assert_eq!(prop(f2, "d").as_str(), Some("2020-01-03"));
        assert_eq!(prop(f2, "ts").as_str(), Some("2020-01-01T02:00:00"));
    }

    #[test]
    fn reads_int32_and_float_columns() {
        // DuckDB file with PLAIN INT32 (`i32`) and FLOAT (`f32`) columns:
        //   COPY (SELECT i::INTEGER i32, (i*1.25)::FLOAT f32,
        //                ST_Point(i, i*2) geometry FROM range(400) t(i)) TO ...
        // INT32 widens to a JSON integer, FLOAT to a JSON number.
        let bytes = include_bytes!("../tests/fixtures/duckdb_int32_float.parquet");
        let out = geoparquet_to_geojson(bytes).unwrap();
        let fc = geojson::from_json(&json::parse(&out).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 400);

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap()
        };
        for i in [0usize, 1, 7, 399] {
            let f = &fc.features[i];
            assert_eq!(prop(f, "i32").as_f64(), Some(i as f64));
            // f32 value round-trips exactly through f64.
            assert_eq!(prop(f, "f32").as_f64(), Some((i as f32 * 1.25) as f64));
            assert_eq!(
                f.geometry,
                Some(crate::geometry::Geometry::Point([i as f64, (i * 2) as f64]))
            );
        }
    }
}
