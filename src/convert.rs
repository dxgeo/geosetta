//! Orchestrates conversions between formats, routed through the shared
//! [`FeatureCollection`] IR: `read(from) -> FeatureCollection -> write(to)`.
//! Adding a format is one reader and one writer against the IR; every from/to
//! pair then composes automatically (hub-and-spoke).

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::format::Format;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::{from_wkb, from_wkt, to_wkb, to_wkt, Bbox};
use crate::{csv, flatgeobuf, geojson, parquet};
// `json` is only needed by the test helpers below (the read path streams
// GeoJSON directly rather than through a JsonValue tree).
#[cfg(test)]
use crate::json;

/// Convert `input` bytes from one format to another via the Feature IR. The
/// hub entry point in one call; `main` unrolls it into read/parse/write stages
/// so it can report `--progress`, so this is used mainly by the tests below.
#[allow(dead_code)]
pub fn convert(from: Format, to: Format, input: &[u8]) -> Result<Vec<u8>> {
    write_features(to, &read_features(from, input)?)
}

/// Reorder features by Hilbert-curve locality, so spatially-close features are
/// adjacent (better GeoParquet row-group clustering and compression).
pub fn reorder_hilbert(fc: &mut FeatureCollection) {
    let bboxes: Vec<Bbox> = fc
        .features
        .iter()
        .map(|f| f.geometry.as_ref().map(|g| g.bbox()).unwrap_or_else(Bbox::empty))
        .collect();
    let order = crate::spatial::hilbert_order(&bboxes);
    let mut slots: Vec<Option<Feature>> = std::mem::take(&mut fc.features).into_iter().map(Some).collect();
    fc.features = order.into_iter().map(|i| slots[i].take().unwrap()).collect();
}

/// Decode any supported input format into the shared Feature IR.
pub fn read_features(format: Format, input: &[u8]) -> Result<FeatureCollection> {
    match format {
        Format::GeoJson => {
            let text = std::str::from_utf8(input)
                .map_err(|_| Error::GeoJson("input is not valid utf-8".into()))?;
            geojson::from_geojson_str(text)
        }
        Format::Parquet => parquet_to_features(input),
        Format::FlatGeobuf => flatgeobuf::read(input),
        Format::Csv => csv::read(input),
        Format::Wkt => read_wkt_lines(input),
        // GeoPackage is multi-layer, so it's read via geopackage::read_layers in
        // main.rs rather than through this single-collection path.
        Format::Gpkg => Err(Error::Usage(
            "GeoPackage input is handled per-layer; this path should not be reached".into(),
        )),
    }
}

/// Encode the Feature IR into a supported output format.
pub fn write_features(format: Format, fc: &FeatureCollection) -> Result<Vec<u8>> {
    match format {
        Format::GeoJson => Ok(geojson::to_geojson_string(fc).into_bytes()),
        Format::Parquet => Ok(features_to_parquet(fc)),
        Format::FlatGeobuf => Ok(flatgeobuf::write(fc)),
        Format::Csv => Ok(csv::write(fc)),
        Format::Wkt => Ok(write_wkt_lines(fc)),
        Format::Gpkg => Err(Error::Usage("writing GeoPackage is not supported yet".into())),
    }
}

/// Read one WKT geometry per non-blank line (geometry only, no properties).
fn read_wkt_lines(input: &[u8]) -> Result<FeatureCollection> {
    let text = std::str::from_utf8(input)
        .map_err(|_| Error::Convert("wkt: input is not valid utf-8".into()))?;
    let mut features = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.is_empty() {
            features.push(Feature {
                geometry: Some(from_wkt(line)?),
                properties: Vec::new(),
            });
        }
    }
    // A .wkt file is bare geometry with no coordinate-reference metadata.
    Ok(FeatureCollection::new(features))
}

/// Write one WKT geometry per line (properties are dropped).
fn write_wkt_lines(fc: &FeatureCollection) -> Vec<u8> {
    let mut out = String::new();
    for f in &fc.features {
        if let Some(g) = &f.geometry {
            out.push_str(&to_wkt(g));
        }
        out.push('\n');
    }
    out.into_bytes()
}

/// Feature IR → GeoParquet bytes.
fn features_to_parquet(fc: &FeatureCollection) -> Vec<u8> {
    // Property columns (schema inferred by scanning all features).
    let columns = crate::schema::infer_columns(&fc.features);

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
    let geo = parquet::geo_metadata(&type_names, &bbox, fc.crs.as_ref());
    parquet::write_geoparquet(&columns, &geometry, &geo)
}

/// GeoParquet bytes → Feature IR.
fn parquet_to_features(bytes: &[u8]) -> Result<FeatureCollection> {
    let parsed = parquet::read_geoparquet(bytes)?;
    let num_cols = parsed.properties.len();

    // Seed one feature per row with its geometry and an empty property vec.
    let mut features = Vec::with_capacity(parsed.num_rows);
    for row in 0..parsed.num_rows {
        let geometry = match &parsed.geometry[row] {
            Some(wkb) => Some(from_wkb(wkb)?),
            None => None,
        };
        features.push(Feature {
            geometry,
            properties: Vec::with_capacity(num_cols),
        });
    }

    // Transpose columns → rows by *draining* each column (values move rather
    // than being cloned) and sharing one `Rc` key across every row. Columns are
    // consumed in file order, so each row keeps that property order.
    for col in parsed.properties {
        let key: Rc<str> = Rc::from(col.name);
        for (feature, value) in features.iter_mut().zip(col.values) {
            feature.properties.push((Rc::clone(&key), value));
        }
    }
    Ok(FeatureCollection {
        features,
        crs: parsed.crs,
    })
}

// Thin named wrappers used by the tests below.

#[cfg(test)]
fn geojson_to_geoparquet(input: &str) -> Result<Vec<u8>> {
    Ok(features_to_parquet(&geojson::from_json(&json::parse(input)?)?))
}

#[cfg(test)]
fn geoparquet_to_geojson(bytes: &[u8]) -> Result<String> {
    Ok(geojson::to_json(&parquet_to_features(bytes)?).to_json_string())
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
        let name = cafe.properties.iter().find(|(k, _)| &**k == "name").unwrap();
        assert_eq!(name.1.as_str(), Some("Café ☕"));
        // The nested-array property fell back to a JSON string on the way in,
        // so it comes back as that string.
        let tags = cafe.properties.iter().find(|(k, _)| &**k == "tags").unwrap();
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
                .find(|(name, _)| &**name == k)
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
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
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
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
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

    fn fgb_to_fc(bytes: &[u8]) -> FeatureCollection {
        let out = convert(Format::FlatGeobuf, Format::GeoJson, bytes).unwrap();
        geojson::from_json(&json::parse(std::str::from_utf8(&out).unwrap()).unwrap()).unwrap()
    }

    #[test]
    fn reads_flatgeobuf_geometries() {
        use crate::geometry::Geometry::*;
        // DuckDB-written FGB with mixed geometry types (per-feature type).
        let fc = fgb_to_fc(include_bytes!("../tests/fixtures/duckdb_geoms.fgb"));
        let by_id = |id: i64| {
            fc.features
                .iter()
                .find(|f| f.properties.iter().any(|(k, v)| &**k == "id" && v.as_f64() == Some(id as f64)))
                .unwrap()
                .geometry
                .clone()
                .unwrap()
        };
        assert_eq!(by_id(1), LineString(vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]));
        assert_eq!(
            by_id(2),
            Polygon(vec![
                vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]],
                vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]],
            ])
        );
        assert_eq!(
            by_id(3),
            MultiPolygon(vec![
                vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]],
                vec![vec![[5.0, 5.0], [6.0, 5.0], [6.0, 6.0], [5.0, 5.0]]],
            ])
        );
        assert_eq!(by_id(4), MultiPoint(vec![[0.0, 0.0], [1.0, 1.0]]));
    }

    #[test]
    fn reads_flatgeobuf_property_types() {
        let fc = fgb_to_fc(include_bytes!("../tests/fixtures/duckdb_props.fgb"));
        let feat = fc
            .features
            .iter()
            .find(|f| f.properties.iter().any(|(k, v)| &**k == "n" && v.as_f64() == Some(10.0)))
            .unwrap();
        let prop = |k: &str| feat.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(prop("label").as_str(), Some("alpha"));
        assert_eq!(prop("score").as_f64(), Some(1.5));
        assert_eq!(prop("ok"), crate::json::JsonValue::Bool(true));
        assert_eq!(feat.geometry, Some(crate::geometry::Geometry::Point([-73.9, 40.7])));
    }

    #[test]
    fn flatgeobuf_composes_to_parquet_via_hub() {
        // The hub payoff: FGB -> Parquet (a path never written explicitly) then
        // Parquet -> GeoJSON must reproduce the same features as FGB -> GeoJSON.
        let fgb = include_bytes!("../tests/fixtures/duckdb_geoms.fgb");
        let direct = fgb_to_fc(fgb);
        let pq = convert(Format::FlatGeobuf, Format::Parquet, fgb).unwrap();
        let via_parquet = geojson::from_json(
            &json::parse(&geoparquet_to_geojson(&pq).unwrap()).unwrap(),
        )
        .unwrap();
        let geoms = |fc: &FeatureCollection| {
            let mut g: Vec<_> = fc.features.iter().map(|f| f.geometry.clone()).collect();
            g.sort_by_key(|g| format!("{g:?}"));
            g
        };
        assert_eq!(geoms(&direct), geoms(&via_parquet));
    }

    fn sorted_geoms(fc: &FeatureCollection) -> Vec<Option<crate::geometry::Geometry>> {
        let mut g: Vec<_> = fc.features.iter().map(|f| f.geometry.clone()).collect();
        g.sort_by_key(|g| format!("{g:?}"));
        g
    }

    #[test]
    fn geojson_to_flatgeobuf_preserves_features() {
        // GeoJSON -> FGB (our writer) -> back, via the hub. FGB now writes a
        // packed Hilbert R-tree, which reorders features, so compare as a set.
        let fgb = convert(Format::GeoJson, Format::FlatGeobuf, SAMPLE.as_bytes()).unwrap();
        let back = fgb_to_fc(&fgb);
        let orig = geojson::from_json(&json::parse(SAMPLE).unwrap()).unwrap();
        assert_eq!(back.features.len(), orig.features.len());
        assert_eq!(sorted_geoms(&orig), sorted_geoms(&back));
    }

    #[test]
    fn flatgeobuf_write_round_trips_all_geometry_types() {
        // Read a DuckDB FGB, rewrite it with our writer, read again — all
        // geometry types (incl. polygon-with-hole and multipolygon) survive.
        let src = include_bytes!("../tests/fixtures/duckdb_geoms.fgb");
        let original = fgb_to_fc(src);
        let ours = convert(Format::FlatGeobuf, Format::FlatGeobuf, src).unwrap();
        let reread = fgb_to_fc(&ours);
        assert_eq!(sorted_geoms(&original), sorted_geoms(&reread));
    }

    #[test]
    fn reads_csv_with_wkt_and_types() {
        use crate::geometry::Geometry;
        let bytes = include_bytes!("../tests/fixtures/cities.csv");
        let out = convert(Format::Csv, Format::GeoJson, bytes).unwrap();
        let fc = geojson::from_json(&json::parse(std::str::from_utf8(&out).unwrap()).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 3);

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point([10.0, 20.0])));
        assert_eq!(prop(&fc.features[0], "population").as_f64(), Some(120000.0));
        assert_eq!(prop(&fc.features[0], "capital"), crate::json::JsonValue::Bool(true));
        // Quoted field with an embedded comma.
        assert_eq!(prop(&fc.features[1], "name").as_str(), Some("Beta, City"));
        assert!(matches!(
            fc.features[1].geometry,
            Some(Geometry::LineString(_))
        ));
        // Empty numeric cell -> null.
        assert_eq!(prop(&fc.features[2], "population"), crate::json::JsonValue::Null);
    }

    #[test]
    fn csv_composes_to_parquet_and_back() {
        // CSV -> GeoParquet -> GeoJSON reproduces the CSV's geometries (hub).
        let bytes = include_bytes!("../tests/fixtures/cities.csv");
        let direct = geojson::from_json(
            &json::parse(std::str::from_utf8(&convert(Format::Csv, Format::GeoJson, bytes).unwrap()).unwrap()).unwrap(),
        )
        .unwrap();
        let pq = convert(Format::Csv, Format::Parquet, bytes).unwrap();
        let via_pq = geojson::from_json(&json::parse(&geoparquet_to_geojson(&pq).unwrap()).unwrap()).unwrap();
        assert_eq!(sorted_geoms(&direct), sorted_geoms(&via_pq));
    }

    #[test]
    fn hilbert_reorder_preserves_the_feature_set() {
        use crate::geometry::Geometry::Point;
        let mut fc = FeatureCollection::new(vec![
            Feature { geometry: Some(Point([100.0, 100.0])), properties: vec![] },
            Feature { geometry: Some(Point([0.0, 0.0])), properties: vec![] },
            Feature { geometry: Some(Point([1.0, 1.0])), properties: vec![] },
        ]);
        let before = sorted_geoms(&fc);
        reorder_hilbert(&mut fc);
        assert_eq!(sorted_geoms(&fc), before); // same set, reordered
        // The far point ends up last; the origin cluster is first.
        assert_eq!(fc.features.last().unwrap().geometry, Some(Point([100.0, 100.0])));
    }

    #[test]
    fn geojson_to_parquet_uses_the_spec_default_crs() {
        // GeoJSON carries no CRS of its own (always WGS 84), so the GeoParquet
        // it becomes is the CRS84 default — the reader recovers Wgs84.
        let pq = geojson_to_geoparquet(SAMPLE).unwrap();
        let fc = parquet_to_features(&pq).unwrap();
        assert_eq!(fc.crs, Some(crate::crs::Crs::Wgs84));
    }

    #[test]
    fn parquet_crs_passes_through_the_hub() {
        use crate::crs::{Crs, NamedCrs};
        // A collection tagged with a non-default CRS carrying PROJJSON: it must
        // survive Feature IR -> GeoParquet -> Feature IR unchanged.
        let mut fc = geojson::from_json(&json::parse(SAMPLE).unwrap()).unwrap();
        let projjson = "{\"type\":\"ProjectedCRS\",\"id\":{\"authority\":\"EPSG\",\"code\":3857}}";
        fc.crs = Some(Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some(3857),
            wkt: None,
            projjson: Some(projjson.into()),
        }));
        let pq = features_to_parquet(&fc);
        let back = parquet_to_features(&pq).unwrap();
        match back.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.code, Some(3857));
                assert!(n.projjson.is_some());
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn geopackage_crs_composes_to_flatgeobuf() {
        use crate::crs::{Crs, NamedCrs};
        // GeoPackage -> FlatGeobuf (via the IR) preserves an EPSG code even
        // though neither format is the source: authority+code is the portable
        // token both speak.
        let mut src = FeatureCollection::new(vec![Feature {
            geometry: Some(crate::geometry::Geometry::Point([1.0, 2.0])),
            properties: vec![],
        }]);
        src.crs = Some(Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some(3857),
            wkt: Some("PROJCS[\"Web Mercator\"]".into()),
            projjson: None,
        }));
        let gpkg = crate::geopackage::write_layers(None, &[("l".into(), src)], false).unwrap();
        let layers = crate::geopackage::read_layers(&gpkg).unwrap();
        let fgb = flatgeobuf::write(&layers[0].1);
        let back = flatgeobuf::read(&fgb).unwrap();
        match back.crs {
            Some(Crs::Named(n)) => assert_eq!(n.code, Some(3857)),
            other => panic!("expected Named EPSG:3857, got {other:?}"),
        }
    }

    #[test]
    fn projjson_only_parquet_warns_when_written_to_a_wkt_target() {
        use crate::crs::{Crs, NamedCrs};
        // A hand-authored GeoParquet whose PROJJSON carries no `id` (no authority
        // code to lift): it round-trips as a code-less, PROJJSON-only CRS…
        let mut fc = geojson::from_json(&json::parse(SAMPLE).unwrap()).unwrap();
        fc.crs = Some(Crs::Named(NamedCrs {
            authority: None,
            code: None,
            wkt: None,
            projjson: Some("{\"type\":\"GeographicCRS\",\"name\":\"Custom Grid\"}".into()),
        }));
        let pq = features_to_parquet(&fc);
        let recovered = parquet_to_features(&pq).unwrap();
        match recovered.crs {
            Some(Crs::Named(ref n)) => {
                assert!(n.code.is_none() && n.authority.is_none());
                assert!(n.projjson.is_some());
            }
            other => panic!("expected code-less PROJJSON CRS, got {other:?}"),
        }

        // …so GeoParquet (same dialect) stays silent, but a WKT-dialect target
        // (FlatGeobuf/GeoPackage) announces that the CRS will be dropped.
        let crs = recovered.crs.as_ref().unwrap();
        assert_eq!(crs.downgrade_warning(Format::Parquet), None);
        assert!(crs.downgrade_warning(Format::FlatGeobuf).unwrap().contains("dropped"));
    }

    #[test]
    fn flatgeobuf_wkt_only_crs_recovers_authority_code() {
        use crate::crs::{Crs, NamedCrs};
        // Simulate a rich-format source that recorded only a WKT *definition*
        // with no separate authority+code (org/code absent on the wire). The
        // reader must lift the CRS's own id out of the WKT so the identity
        // survives to every authority+code target — here FlatGeobuf -> Parquet.
        let wkt = "GEOGCRS[\"GDA2020\",DATUM[\"GDA2020\",ELLIPSOID[\"GRS 1980\",6378137,298.257222101,ID[\"EPSG\",7019]]],CS[ellipsoidal,2],ID[\"EPSG\",7844]]";
        let mut fc = FeatureCollection::new(vec![Feature {
            geometry: Some(crate::geometry::Geometry::Point([1.0, 2.0])),
            properties: vec![],
        }]);
        fc.crs = Some(Crs::Named(NamedCrs {
            authority: None,
            code: None,
            wkt: Some(wkt.into()),
            projjson: None,
        }));

        // FlatGeobuf writes the WKT only; reading it back recovers EPSG:7844.
        let fgb = flatgeobuf::write(&fc);
        match flatgeobuf::read(&fgb).unwrap().crs {
            Some(Crs::Named(ref n)) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code, Some(7844));
            }
            other => panic!("expected recovered EPSG:7844, got {other:?}"),
        }

        // …and that recovered code reaches GeoParquet as a minimal id reference
        // instead of dropping to the WGS 84 default.
        let recovered = flatgeobuf::read(&fgb).unwrap();
        let pq = features_to_parquet(&recovered);
        match parquet_to_features(&pq).unwrap().crs {
            Some(Crs::Named(n)) => assert_eq!(n.code, Some(7844)),
            other => panic!("expected EPSG:7844 through Parquet, got {other:?}"),
        }
    }

    #[test]
    fn wkt_lines_round_trip() {
        // GeoJSON -> .wkt (geometry only) -> GeoJSON keeps the geometries.
        let orig = geojson::from_json(&json::parse(SAMPLE).unwrap()).unwrap();
        let wkt = convert(Format::GeoJson, Format::Wkt, SAMPLE.as_bytes()).unwrap();
        let back = geojson::from_json(
            &json::parse(std::str::from_utf8(&convert(Format::Wkt, Format::GeoJson, &wkt).unwrap()).unwrap()).unwrap(),
        )
        .unwrap();
        let geoms: Vec<_> = orig.features.iter().map(|f| f.geometry.clone()).collect();
        let back_geoms: Vec<_> = back.features.iter().map(|f| f.geometry.clone()).collect();
        assert_eq!(geoms, back_geoms);
    }
}
