//! Orchestrates conversions between formats, routed through the shared
//! [`FeatureCollection`] IR: `read(from) -> FeatureCollection -> write(to)`.
//! Adding a format is one reader and one writer against the IR; every from/to
//! pair then composes automatically (hub-and-spoke).

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::format::Format;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::{from_wkb, from_wkt, to_wkb, to_wkt, Bbox, Geometry, Position};
use crate::{csv, flatgeobuf, geojson, kml, parquet};
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
        Format::Kml => kml::read_kml(input),
        Format::Kmz => kml::read_kmz(input),
        // GeoPackage is multi-layer, so it's read via geopackage::read_layers in
        // main.rs rather than through this single-collection path.
        Format::Gpkg => Err(Error::Usage(
            "GeoPackage input is handled per-layer; this path should not be reached".into(),
        )),
        // Shapefile is multi-file (.shp/.dbf/.prj), so it's read via
        // shapefile::read in main.rs, which locates the sibling files, rather
        // than through this single-buffer path.
        Format::Shapefile => Err(Error::Usage(
            "Shapefile input is handled via its sibling files; this path should not be reached".into(),
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
        Format::Kml => Ok(kml::write_kml(fc)),
        Format::Kmz => Ok(kml::write_kmz(fc)),
        // GeoPackage and Shapefile are handled outside this single-buffer path
        // (see read_features above) — GeoPackage via geopackage::write_layers,
        // Shapefile via shapefile::write, both called from main.rs.
        Format::Gpkg => Err(Error::Usage("writing GeoPackage is not supported yet".into())),
        Format::Shapefile => Err(Error::Usage(
            "Shapefile output is handled via its sibling files; this path should not be reached".into(),
        )),
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

    // Geometry column: WKB per feature, plus bbox and the set of types. Each
    // type name carries GeoParquet's own `" Z"`/`" M"`/`" ZM"` suffix per the
    // spec (M8) — derived per-geometry, not file-wide, since a mixed
    // 2D/3D dataset needs *both* "Point" and "Point Z" as independent set
    // entries (confirmed against real DuckDB-written GeoParquet `geo`
    // metadata for the Z case, and GDAL's for M/ZM, before implementing).
    let mut bbox = Bbox::empty();
    let mut types: BTreeSet<String> = BTreeSet::new();
    let mut geometry: Vec<Option<Vec<u8>>> = Vec::with_capacity(fc.features.len());
    for feature in &fc.features {
        match &feature.geometry {
            Some(g) => {
                g.extend_bbox(&mut bbox);
                types.insert(format!("{}{}", g.type_name(), geoparquet_dim_suffix(g)));
                geometry.push(Some(to_wkb(g)));
            }
            None => geometry.push(None),
        }
    }

    let type_names: Vec<String> = types.into_iter().collect();
    let geo = parquet::geo_metadata(&type_names, &bbox, fc.crs.as_ref());
    parquet::write_geoparquet(&columns, &geometry, &geo)
}

/// GeoParquet's own `geometry_types` dimensionality suffix: `" Z"`, `" M"`,
/// or `" ZM"` per the geometry's first position, empty for plain 2D —
/// mirroring `wkb.rs`'s own `dim_of_first`/`first_position` convention
/// (kept as an independent copy here, matching every other format spoke's
/// self-contained style, since this is GeoParquet-metadata-specific naming,
/// not a general geometry operation).
///
/// *Correction, 2026-08-17*: an earlier version of this function never
/// emitted `" M"`/`" ZM"`, reasoning from DuckDB's GeoParquet writer
/// producing *no* `geo` metadata at all for a `POINT M`/`POINT ZM` source
/// and concluding the *spec* had no M vocabulary. That was wrong — the
/// GeoParquet spec text itself defines all three suffixes explicitly
/// (`" Z"`/`" M"`/`" ZM"`), and GDAL's own writer confirms it, producing
/// `"geometry_types":["Point ZM"]` for the identical source. DuckDB's
/// omission was a writer-specific gap, not a spec limitation — generalizing
/// from one tool's behavior to "the spec" without reading the spec text was
/// the mistake; fixed by checking the actual spec
/// (=raw.githubusercontent.com/opengeospatial/geoparquet/main/format-specs/geoparquet.md=)
/// rather than inferring it from DuckDB alone.
fn geoparquet_dim_suffix(g: &Geometry) -> &'static str {
    let (z, m) = geoparquet_first_position_dim(g);
    match (z, m) {
        (true, true) => " ZM",
        (true, false) => " Z",
        (false, true) => " M",
        (false, false) => "",
    }
}

fn geoparquet_first_position_dim(g: &Geometry) -> (bool, bool) {
    fn dim(p: Position) -> (bool, bool) {
        (p.z.is_some(), p.m.is_some())
    }
    match g {
        Geometry::Point(p) => dim(*p),
        Geometry::LineString(ps) | Geometry::MultiPoint(ps) => {
            ps.first().map(|p| dim(*p)).unwrap_or((false, false))
        }
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            rings.first().and_then(|r| r.first()).map(|p| dim(*p)).unwrap_or((false, false))
        }
        Geometry::MultiPolygon(polys) => polys
            .first()
            .and_then(|p| p.first())
            .and_then(|r| r.first())
            .map(|p| dim(*p))
            .unwrap_or((false, false)),
        Geometry::GeometryCollection(geoms) => {
            geoms.first().map(geoparquet_first_position_dim).unwrap_or((false, false))
        }
    }
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
    use crate::geometry::Position;

    const SAMPLE: &str = r#"{
        "type": "FeatureCollection",
        "features": [
            {"type": "Feature",
             "geometry": {"type": "Point", "coordinates": [-73.9857, 40.7484]},
             "properties": {"name": "Empire State", "height_m": 381, "landmark": true, "rating": 4.7}},
            {"type": "Feature",
             "geometry": {"type": "LineString", "coordinates": [[-73.99, 40.75],[-73.98, 40.76]]},
             "properties": {"name": "A Path", "landmark": false, "rating": 3.2}},
            {"type": "Feature",
             "geometry": {"type": "Polygon", "coordinates": [[[-74,40.7],[-73.95, 40.7],[-73.95, 40.75],[-74,40.7]]]},
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
            Some(crate::geometry::Geometry::Point(Position::new(-73.9857, 40.7484)))
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
                Some(crate::geometry::Geometry::Point(Position::new(g, g * 2.0)))
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
                Some(crate::geometry::Geometry::Point(Position::new(i as f64, (i * 2) as f64)))
            );
        }
    }

    /// Shared checker for the pyarrow DECIMAL/INT96/FIXED_LEN_BYTE_ARRAY
    /// fixtures — one PLAIN-encoded, one dictionary-encoded, same 40 rows
    /// (`i` from 0 to 39), generated with `store_decimal_as_integer=True` and
    /// `use_deprecated_int96_timestamps=True` so all three DECIMAL physical
    /// encodings and the legacy INT96 timestamp are exercised:
    ///   decimal_i32   DECIMAL(6,2)  on INT32               (i-20)*12.34
    ///   decimal_i64   DECIMAL(15,3) on INT64               (i-20)*1234.567
    ///   decimal_flba  DECIMAL(25,4) on FIXED_LEN_BYTE_ARRAY (i-20)*100000.1234
    ///   ts_ns         TIMESTAMP(ns) on INT96                1_600_000_000e9 + i*1_000_000_001
    ///   raw4          plain FIXED_LEN_BYTE_ARRAY(4), non-DECIMAL           [i,i+1,i+2,i+3]
    ///   geometry      WKB Point(i, i*2)
    fn check_pyarrow_types_fixture(bytes: &[u8]) {
        let out = geoparquet_to_geojson(bytes).unwrap();
        let fc = geojson::from_json(&json::parse(&out).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 40);

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
        };
        let cases: &[(usize, &str, &str, &str, &str, &str)] = &[
            (0, "-246.80", "-24691.340", "-2000002.4680", "2020-09-13T12:26:40", "00010203"),
            (1, "-234.46", "-23456.773", "-1900002.3446", "2020-09-13T12:26:41.000000001", "01020304"),
            (20, "0.00", "0.000", "0.0000", "2020-09-13T12:27:00.000000020", "14151617"),
            (39, "234.46", "23456.773", "1900002.3446", "2020-09-13T12:27:19.000000039", "2728292a"),
        ];
        for &(i, dec_i32, dec_i64, dec_flba, ts, raw) in cases {
            let f = &fc.features[i];
            assert_eq!(prop(f, "decimal_i32").as_str(), Some(dec_i32), "row {i} decimal_i32");
            assert_eq!(prop(f, "decimal_i64").as_str(), Some(dec_i64), "row {i} decimal_i64");
            assert_eq!(prop(f, "decimal_flba").as_str(), Some(dec_flba), "row {i} decimal_flba");
            assert_eq!(prop(f, "ts_ns").as_str(), Some(ts), "row {i} ts_ns");
            assert_eq!(prop(f, "raw4").as_str(), Some(raw), "row {i} raw4");
            assert_eq!(
                f.geometry,
                Some(crate::geometry::Geometry::Point(Position::new(i as f64, i as f64 * 2.0))),
                "row {i} geometry"
            );
        }
    }

    #[test]
    fn reads_pyarrow_decimal_int96_flba_dictionary() {
        check_pyarrow_types_fixture(include_bytes!("../tests/fixtures/pyarrow_types_dict.parquet"));
    }

    #[test]
    fn reads_pyarrow_decimal_int96_flba_plain() {
        check_pyarrow_types_fixture(include_bytes!("../tests/fixtures/pyarrow_types_plain.parquet"));
    }

    /// Shared checker for the pyarrow DATA_PAGE_V2 fixtures (`data_page_version=
    /// "2.0"`) — one PLAIN-encoded, one dictionary-encoded, same 60 rows:
    ///   id        INT32 REQUIRED (non-nullable)             i
    ///   bucket    INT64 OPTIONAL, null every 5th row         i*3
    ///   color     BYTE_ARRAY OPTIONAL, null every 7th row    "color_{i%4}"
    ///   req       INT32 REQUIRED (non-nullable, def level 0) i*2
    ///   geometry  BYTE_ARRAY REQUIRED                        WKB Point(i, i*2)
    /// `id`/`req` exercise DATA_PAGE_V2's max_def_level==0 (bare-values, no
    /// definition-level section) path; `bucket`/`color` exercise the
    /// present-mask path with a real null pattern.
    fn check_data_page_v2_fixture(bytes: &[u8]) {
        let out = geoparquet_to_geojson(bytes).unwrap();
        let fc = geojson::from_json(&json::parse(&out).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 60);

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
        };
        for i in 0..60usize {
            let f = &fc.features[i];
            assert_eq!(prop(f, "id").as_f64(), Some(i as f64), "row {i} id");
            assert_eq!(prop(f, "req").as_f64(), Some((i * 2) as f64), "row {i} req");
            if i % 5 == 0 {
                assert_eq!(prop(f, "bucket"), crate::json::JsonValue::Null, "row {i} bucket");
            } else {
                assert_eq!(prop(f, "bucket").as_f64(), Some((i * 3) as f64), "row {i} bucket");
            }
            if i % 7 == 0 {
                assert_eq!(prop(f, "color"), crate::json::JsonValue::Null, "row {i} color");
            } else {
                assert_eq!(prop(f, "color").as_str(), Some(format!("color_{}", i % 4).as_str()), "row {i} color");
            }
            assert_eq!(
                f.geometry,
                Some(crate::geometry::Geometry::Point(Position::new(i as f64, i as f64 * 2.0))),
                "row {i} geometry"
            );
        }
    }

    #[test]
    fn reads_data_page_v2_dictionary() {
        check_data_page_v2_fixture(include_bytes!("../tests/fixtures/pyarrow_data_page_v2_dict.parquet"));
    }

    #[test]
    fn reads_data_page_v2_plain() {
        check_data_page_v2_fixture(include_bytes!("../tests/fixtures/pyarrow_data_page_v2_plain.parquet"));
    }

    /// Real GDAL fixture (`ogr2ogr -f Parquet -lco USE_PARQUET_GEO_TYPES=ONLY
    /// -lco GEOMETRY_NAME=shape`): the geometry column is named `shape`, not
    /// `geometry`, and the file carries *no* `geo` key/value metadata at all —
    /// ONLY mode drops the classic GeoParquet convention entirely in favor of
    /// Parquet's native `GEOMETRY` logical type (`SchemaElement.logicalType`).
    /// Exercises the schema-only geometry-column detection path: without it,
    /// this file's "shape" column would be read as an ordinary property and
    /// fail UTF-8 validation on its WKB bytes.
    #[test]
    fn reads_gdal_native_geometry_with_a_non_default_column_name() {
        let bytes = include_bytes!("../tests/fixtures/gdal_native_geometry_custom_name.parquet");
        let fc = parquet_to_features(bytes).unwrap();
        assert_eq!(fc.features.len(), 3);
        assert_eq!(fc.crs, Some(crate::crs::Crs::Wgs84));

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
        };
        let expected = [("a", Position::new(1.5, 2.5)), ("b", Position::new(3.5, 4.5)), ("c", Position::new(-5.25, 10.75))];
        for (f, (name, coords)) in fc.features.iter().zip(expected) {
            assert_eq!(prop(f, "name").as_str(), Some(name));
            assert_eq!(f.geometry, Some(crate::geometry::Geometry::Point(coords)));
        }
    }

    /// Real GDAL fixture (`ogr2ogr -f Parquet -lco USE_PARQUET_GEO_TYPES=ONLY
    /// -a_srs EPSG:3857`): default geometry column name, no `geo` metadata,
    /// CRS assigned (not reprojected — coordinates are untouched) to
    /// EPSG:3857 and carried in the native `GeometryType.crs` field as
    /// PROJJSON. Exercises CRS recovery from the schema when `geo` metadata
    /// isn't there to supply it.
    #[test]
    fn reads_gdal_native_geometry_crs_without_geo_metadata() {
        use crate::crs::Crs;
        let bytes = include_bytes!("../tests/fixtures/gdal_native_geometry_3857.parquet");
        let fc = parquet_to_features(bytes).unwrap();
        assert_eq!(fc.features.len(), 3);
        match fc.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("3857"));
            }
            other => panic!("expected EPSG:3857 recovered from the native logicalType, got {other:?}"),
        }
        assert_eq!(fc.features[0].geometry, Some(crate::geometry::Geometry::Point(Position::new(1.5, 2.5))));
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
        assert_eq!(by_id(1), LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)]));
        assert_eq!(
            by_id(2),
            Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 4.0), Position::new(0.0, 0.0)],
                vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)],
            ])
        );
        assert_eq!(
            by_id(3),
            MultiPolygon(vec![
                vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0), Position::new(1.0, 1.0), Position::new(0.0, 0.0)]],
                vec![vec![Position::new(5.0, 5.0), Position::new(6.0, 5.0), Position::new(6.0, 6.0), Position::new(5.0, 5.0)]],
            ])
        );
        assert_eq!(by_id(4), MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0)]));
    }

    #[test]
    fn reads_a_real_gdal_flatgeobuf_z_fixture() {
        use crate::geometry::Geometry::LineString;
        // `ogr2ogr -f FlatGeobuf` output for a real 3D LineString, confirmed
        // via `ogrinfo -al` to read back as `LINESTRING Z (0 0 1,1 1 2,2 0 3)`
        // before trusting the fixture (see M6 of `plans/zm-geometry.org`).
        let fc = fgb_to_fc(include_bytes!("../tests/fixtures/gdal_linestring_z.fgb"));
        assert_eq!(fc.features.len(), 1);
        assert_eq!(
            fc.features[0].geometry,
            Some(LineString(vec![
                Position::with_z(0.0, 0.0, 1.0),
                Position::with_z(1.0, 1.0, 2.0),
                Position::with_z(2.0, 0.0, 3.0),
            ]))
        );
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
        assert_eq!(feat.geometry, Some(crate::geometry::Geometry::Point(Position::new(-73.9, 40.7))));
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
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::new(10.0, 20.0))));
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
            Feature { geometry: Some(Point(Position::new(100.0, 100.0))), properties: vec![] },
            Feature { geometry: Some(Point(Position::new(0.0, 0.0))), properties: vec![] },
            Feature { geometry: Some(Point(Position::new(1.0, 1.0))), properties: vec![] },
        ]);
        let before = sorted_geoms(&fc);
        reorder_hilbert(&mut fc);
        assert_eq!(sorted_geoms(&fc), before); // same set, reordered
        // The far point ends up last; the origin cluster is first.
        assert_eq!(fc.features.last().unwrap().geometry, Some(Point(Position::new(100.0, 100.0))));
    }

    #[test]
    fn geoparquet_z_bearing_point_gets_the_z_suffix_in_geometry_types() {
        // M8: confirmed against real DuckDB-written GeoParquet
        // (`parquet_kv_metadata`) before implementing — `POINT Z` produces
        // `"geometry_types":["Point Z"]`, not the bare `"Point"` this crate
        // wrote before M8.
        let doc = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,2.0,3.0]},"properties":{}}
        ]}"#;
        let pq = geojson_to_geoparquet(doc).unwrap();
        let text = String::from_utf8_lossy(&pq);
        assert!(text.contains("\"geometry_types\":[\"Point Z\"]"), "missing Z-suffixed geometry_types");
    }

    #[test]
    fn geoparquet_mixed_2d_and_3d_points_get_both_type_entries() {
        // Confirmed against real DuckDB output: a mixed-dimensionality
        // dataset gets *both* "Point" and "Point Z" as independent set
        // entries, not a single file-wide flag — this is derived
        // per-geometry, matching that real-world behavior.
        let doc = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,2.0]},"properties":{}},
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,2.0,3.0]},"properties":{}}
        ]}"#;
        let pq = geojson_to_geoparquet(doc).unwrap();
        let text = String::from_utf8_lossy(&pq);
        assert!(text.contains("\"geometry_types\":[\"Point\",\"Point Z\"]"), "expected both dimensionalities listed");
    }

    #[test]
    fn geoparquet_2d_only_point_gets_no_suffix() {
        // Degraded-mode bar: 2D-only output is unaffected by M8.
        let doc = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,2.0]},"properties":{}}
        ]}"#;
        let pq = geojson_to_geoparquet(doc).unwrap();
        let text = String::from_utf8_lossy(&pq);
        assert!(text.contains("\"geometry_types\":[\"Point\"]"));
        assert!(!text.contains("Point Z"));
    }

    #[test]
    fn geoparquet_m_and_zm_geometries_get_the_correct_suffix() {
        // GeoJSON can't carry M, so this constructs the Feature IR directly
        // rather than parsing GeoJSON. Confirmed against real GDAL-written
        // GeoParquet before implementing (`ogr2ogr -f Parquet` from a
        // `POINT ZM` WKT source produces `"geometry_types":["Point ZM"]`) —
        // this test would have caught the earlier bug where M/ZM were never
        // suffixed at all.
        let fc = FeatureCollection::new(vec![
            Feature {
                geometry: Some(crate::geometry::Geometry::Point(Position::with_m(1.0, 2.0, 5.0))),
                properties: vec![],
            },
            Feature {
                geometry: Some(crate::geometry::Geometry::Point(Position::with_zm(3.0, 4.0, 5.0, 6.0))),
                properties: vec![],
            },
        ]);
        let pq = features_to_parquet(&fc);
        let text = String::from_utf8_lossy(&pq);
        assert!(text.contains("\"geometry_types\":[\"Point M\",\"Point ZM\"]"), "{text}");
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
            code: Some("3857".into()),
            wkt: None,
            projjson: Some(projjson.into()),
        }));
        let pq = features_to_parquet(&fc);
        let back = parquet_to_features(&pq).unwrap();
        match back.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.code.as_deref(), Some("3857"));
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
            geometry: Some(crate::geometry::Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![],
        }]);
        src.crs = Some(Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: Some("PROJCS[\"Web Mercator\"]".into()),
            projjson: None,
        }));
        let gpkg = crate::geopackage::write_layers(None, &[("l".into(), src)], false).unwrap();
        let layers = crate::geopackage::read_layers(&gpkg).unwrap();
        let fgb = flatgeobuf::write(&layers[0].1);
        let back = flatgeobuf::read(&fgb).unwrap();
        match back.crs {
            Some(Crs::Named(n)) => assert_eq!(n.code.as_deref(), Some("3857")),
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
            geometry: Some(crate::geometry::Geometry::Point(Position::new(1.0, 2.0))),
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
                assert_eq!(n.code.as_deref(), Some("7844"));
            }
            other => panic!("expected recovered EPSG:7844, got {other:?}"),
        }

        // …and that recovered code reaches GeoParquet as a minimal id reference
        // instead of dropping to the WGS 84 default.
        let recovered = flatgeobuf::read(&fgb).unwrap();
        let pq = features_to_parquet(&recovered);
        match parquet_to_features(&pq).unwrap().crs {
            Some(Crs::Named(n)) => assert_eq!(n.code.as_deref(), Some("7844")),
            other => panic!("expected EPSG:7844 through Parquet, got {other:?}"),
        }
    }

    // --- Shapefile ---------------------------------------------------------
    // Real DuckDB-spatial-generated fixtures (`COPY ... TO 'x.shp' WITH
    // (FORMAT GDAL, DRIVER 'ESRI Shapefile')`, the same sourcing method used
    // for the FlatGeobuf/GeoPackage fixtures above), one per geometry family,
    // oracle-checked by hand against `duckdb`'s own `ST_Read`/`ST_AsText`
    // before being committed. Shapefile isn't routed through
    // `convert::read_features` (it's multi-file — see that match arm), so
    // these call `shapefile::read` directly, mirroring `fgb_to_fc` above.

    #[test]
    fn reads_shapefile_point() {
        use crate::geometry::Geometry::Point;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/duckdb_point.shp"),
            include_bytes!("../tests/fixtures/duckdb_point.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(fc.features.len(), 1);
        assert_eq!(fc.features[0].geometry, Some(Point(Position::new(1.5, 2.5))));
        let prop = |k: &str| {
            fc.features[0].properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(prop("id").as_f64(), Some(1.0));
        assert_eq!(prop("name").as_str(), Some("alpha"));
    }

    #[test]
    fn reads_shapefile_multilinestring() {
        use crate::geometry::Geometry::MultiLineString;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/duckdb_multiline.shp"),
            include_bytes!("../tests/fixtures/duckdb_multiline.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(MultiLineString(vec![
                vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0)],
                vec![Position::new(5.0, 5.0), Position::new(6.0, 6.0), Position::new(7.0, 5.0)],
            ]))
        );
    }

    #[test]
    fn reads_shapefile_multipoint() {
        use crate::geometry::Geometry::MultiPoint;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/duckdb_multipoint.shp"),
            include_bytes!("../tests/fixtures/duckdb_multipoint.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(fc.features[0].geometry, Some(MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, -1.0)])));
    }

    #[test]
    fn reads_shapefile_polygon_with_hole() {
        use crate::geometry::Geometry::Polygon;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/duckdb_poly_hole.shp"),
            include_bytes!("../tests/fixtures/duckdb_poly_hole.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(0.0, 4.0), Position::new(4.0, 4.0), Position::new(4.0, 0.0), Position::new(0.0, 0.0)],
                vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)],
            ]))
        );
    }

    #[test]
    fn reads_shapefile_multipolygon() {
        use crate::geometry::Geometry::MultiPolygon;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/duckdb_multipoly.shp"),
            include_bytes!("../tests/fixtures/duckdb_multipoly.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(MultiPolygon(vec![
                vec![vec![Position::new(0.0, 0.0), Position::new(0.0, 1.0), Position::new(1.0, 1.0), Position::new(1.0, 0.0), Position::new(0.0, 0.0)]],
                vec![vec![Position::new(5.0, 5.0), Position::new(5.0, 6.0), Position::new(6.0, 6.0), Position::new(6.0, 5.0), Position::new(5.0, 5.0)]],
            ]))
        );
    }

    #[test]
    fn reads_a_real_gdal_point_z_shapefile() {
        // `ogr2ogr -f "ESRI Shapefile"` output for a real 3D point, confirmed
        // via `ogrinfo -al` (`POINT Z (-73.9857 40.7484 381)`) and by
        // hand-decoding the raw record bytes before trusting the fixture
        // (see M7 of `plans/zm-geometry.org`) — PointZ has no bbox/Z-range
        // fields at all (unlike PolyLineZ/PolygonZ/MultiPointZ below), a
        // genuinely different record shape worth its own real fixture.
        use crate::geometry::Geometry::Point;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/gdal_point_z.shp"),
            include_bytes!("../tests/fixtures/gdal_point_z.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(fc.features[0].geometry, Some(Point(Position::with_z(-73.9857, 40.7484, 381.0))));
    }

    #[test]
    fn reads_a_real_gdal_polygon_z_shapefile() {
        // Same sourcing/verification method as above, for a PolygonZ with a
        // hole — exercises the shared numParts/numPoints/Z-array path
        // `read_parts(.., true)` also uses for PolyLineZ.
        use crate::geometry::Geometry::Polygon;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/gdal_polygon_z.shp"),
            include_bytes!("../tests/fixtures/gdal_polygon_z.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(Polygon(vec![
                vec![
                    Position::with_z(0.0, 0.0, 1.0),
                    Position::with_z(0.0, 4.0, 1.0),
                    Position::with_z(4.0, 4.0, 1.0),
                    Position::with_z(4.0, 0.0, 1.0),
                    Position::with_z(0.0, 0.0, 1.0),
                ],
                vec![
                    Position::with_z(1.0, 1.0, 2.0),
                    Position::with_z(2.0, 1.0, 2.0),
                    Position::with_z(2.0, 2.0, 2.0),
                    Position::with_z(1.0, 2.0, 2.0),
                    Position::with_z(1.0, 1.0, 2.0),
                ],
            ]))
        );
    }

    #[test]
    fn reads_a_real_gdal_point_m_shapefile() {
        // `ogr2ogr -f "ESRI Shapefile"` output from a `POINT M (1 2 5)` WKT
        // source (via a CSV with a `wkt` column, GDAL's `GEOM_POSSIBLE_NAMES`
        // option) — confirmed via `ogrinfo -al` ("Measured Point") and by
        // hand-decoding the raw record bytes (shape type 21, content
        // type+X+Y+M, no Z field at all) before trusting the fixture.
        use crate::geometry::Geometry::Point;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/gdal_point_m.shp"),
            include_bytes!("../tests/fixtures/gdal_point_m.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(fc.features[0].geometry, Some(Point(Position::with_m(1.0, 2.0, 5.0))));
    }

    #[test]
    fn reads_a_real_gdal_point_zm_shapefile() {
        // Same sourcing method, from `POINT ZM (-73.9857 40.7484 381 12.5)`
        // — confirmed via `ogrinfo -al` ("3D Measured Point") that M rides
        // alongside Z on shape type 11 (`PointZ`), not a separate code.
        use crate::geometry::Geometry::Point;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/gdal_point_zm.shp"),
            include_bytes!("../tests/fixtures/gdal_point_zm.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(fc.features[0].geometry, Some(Point(Position::with_zm(-73.9857, 40.7484, 381.0, 12.5))));
    }

    #[test]
    fn reads_a_real_gdal_linestring_zm_shapefile() {
        // From `LINESTRING ZM (0 0 1 10, 1 1 2 20, 2 0 3 30)` — confirmed via
        // both `ogrinfo -al` ("3D Measured Line String") and DuckDB's
        // `ST_AsText(ST_Read(...))` reading the same fixture back
        // byte-identical before trusting it.
        use crate::geometry::Geometry::LineString;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/gdal_linestring_zm.shp"),
            include_bytes!("../tests/fixtures/gdal_linestring_zm.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(LineString(vec![
                Position::with_zm(0.0, 0.0, 1.0, 10.0),
                Position::with_zm(1.0, 1.0, 2.0, 20.0),
                Position::with_zm(2.0, 0.0, 3.0, 30.0),
            ]))
        );
    }

    #[test]
    fn reads_a_real_gdal_polygon_zm_shapefile() {
        // From `POLYGON ZM ((0 0 1 10, 0 4 1 11, 4 4 1 12, 4 0 1 13, 0 0 1
        // 10),(1 1 2 20, 2 1 2 21, 2 2 2 22, 1 2 2 23, 1 1 2 20))` —
        // exercises both a shell and a hole carrying independent Z and M
        // ranges together, hand-decoded before trusting the fixture.
        use crate::geometry::Geometry::Polygon;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/gdal_polygon_zm.shp"),
            include_bytes!("../tests/fixtures/gdal_polygon_zm.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(Polygon(vec![
                vec![
                    Position::with_zm(0.0, 0.0, 1.0, 10.0),
                    Position::with_zm(0.0, 4.0, 1.0, 11.0),
                    Position::with_zm(4.0, 4.0, 1.0, 12.0),
                    Position::with_zm(4.0, 0.0, 1.0, 13.0),
                    Position::with_zm(0.0, 0.0, 1.0, 10.0),
                ],
                vec![
                    Position::with_zm(1.0, 1.0, 2.0, 20.0),
                    Position::with_zm(2.0, 1.0, 2.0, 21.0),
                    Position::with_zm(2.0, 2.0, 2.0, 22.0),
                    Position::with_zm(1.0, 2.0, 2.0, 23.0),
                    Position::with_zm(1.0, 1.0, 2.0, 20.0),
                ],
            ]))
        );
    }

    #[test]
    fn reads_a_real_gdal_multipoint_zm_shapefile() {
        // From `MULTIPOINT ZM ((0 0 1 10), (1 1 2 20))`.
        use crate::geometry::Geometry::MultiPoint;
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/gdal_multipoint_zm.shp"),
            include_bytes!("../tests/fixtures/gdal_multipoint_zm.dbf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fc.features[0].geometry,
            Some(MultiPoint(vec![Position::with_zm(0.0, 0.0, 1.0, 10.0), Position::with_zm(1.0, 1.0, 2.0, 20.0)]))
        );
    }

    #[test]
    fn reads_shapefile_prj_as_id_less_named_crs() {
        // DuckDB/GDAL's WGS 84 .prj carries no AUTHORITY node (real-world Esri
        // shape), so it recovers as a Named CRS with no lifted id, not the
        // Wgs84 default: geosetta reads what a format records and never infers
        // an identity from a CRS's name. `--print-crs-code` therefore has
        // nothing to report for such a source, and `--crs` is the way to give
        // it one.
        let prj = std::str::from_utf8(include_bytes!("../tests/fixtures/duckdb_crs_pt.prj")).unwrap();
        let fc = crate::shapefile::read(
            include_bytes!("../tests/fixtures/duckdb_crs_pt.shp"),
            include_bytes!("../tests/fixtures/duckdb_crs_pt.dbf"),
            Some(prj),
            None,
        )
        .unwrap();
        match fc.crs {
            Some(crate::crs::Crs::Named(n)) => {
                assert_eq!((n.authority, n.code), (None, None));
                assert!(n.wkt.as_deref().unwrap().contains("GCS_WGS_1984"));
            }
            other => panic!("expected an id-less Named CRS, got {other:?}"),
        }
    }

    fn shp_to_fc(shp: &[u8], dbf: &[u8]) -> FeatureCollection {
        crate::shapefile::read(shp, dbf, None, None).unwrap()
    }

    #[test]
    fn shapefile_composes_to_parquet_via_hub() {
        // The hub payoff, mirroring flatgeobuf_composes_to_parquet_via_hub:
        // Shapefile -> Parquet (a path never written explicitly) then Parquet
        // -> GeoJSON must reproduce the same features as reading the shapefile
        // directly.
        let shp = include_bytes!("../tests/fixtures/duckdb_multipoly.shp");
        let dbf = include_bytes!("../tests/fixtures/duckdb_multipoly.dbf");
        let direct = shp_to_fc(shp, dbf);
        let pq = features_to_parquet(&direct);
        let via_parquet = parquet_to_features(&pq).unwrap();
        assert_eq!(sorted_geoms(&direct), sorted_geoms(&via_parquet));
    }

    #[test]
    fn shapefile_write_round_trips_all_geometry_types() {
        // Read each real DuckDB fixture, rewrite it with our writer, read
        // again — geometry survives (mirrors
        // flatgeobuf_write_round_trips_all_geometry_types).
        for (shp, dbf) in [
            (
                &include_bytes!("../tests/fixtures/duckdb_point.shp")[..],
                &include_bytes!("../tests/fixtures/duckdb_point.dbf")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/duckdb_multiline.shp")[..],
                &include_bytes!("../tests/fixtures/duckdb_multiline.dbf")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/duckdb_poly_hole.shp")[..],
                &include_bytes!("../tests/fixtures/duckdb_poly_hole.dbf")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/duckdb_multipoly.shp")[..],
                &include_bytes!("../tests/fixtures/duckdb_multipoly.dbf")[..],
            ),
            (
                &include_bytes!("../tests/fixtures/duckdb_multipoint.shp")[..],
                &include_bytes!("../tests/fixtures/duckdb_multipoint.dbf")[..],
            ),
        ] {
            let original = shp_to_fc(shp, dbf);
            let encoded = crate::shapefile::write(&original).unwrap();
            let reread = crate::shapefile::read(&encoded.shp, &encoded.dbf, None, None).unwrap();
            assert_eq!(sorted_geoms(&original), sorted_geoms(&reread));
        }
    }

    #[test]
    fn geojson_to_shapefile_preserves_features() {
        // GeoJSON -> Shapefile (our writer) -> back, via shapefile::read
        // directly (Shapefile isn't in the hub's single-buffer path).
        let geojson_fc = geojson::from_json(&json::parse(SAMPLE).unwrap()).unwrap();
        // SAMPLE mixes Point/LineString/Polygon, which a single .shp can't
        // represent (one shape family per file) — isolate the Point features,
        // matching the real-world constraint this spoke's writer enforces.
        let points_only = FeatureCollection::new(
            geojson_fc.features.iter().filter(|f| matches!(f.geometry, Some(crate::geometry::Geometry::Point(_)))).cloned().collect(),
        );
        let encoded = crate::shapefile::write(&points_only).unwrap();
        let back = crate::shapefile::read(&encoded.shp, &encoded.dbf, None, None).unwrap();
        assert_eq!(back.features.len(), points_only.features.len());
        assert_eq!(sorted_geoms(&back), sorted_geoms(&points_only));
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

    // --- KML ----------------------------------------------------------------

    #[test]
    fn reads_real_duckdb_kml_fixture() {
        // Real DuckDB-spatial output (COPY ... TO 'x.kml' WITH (FORMAT GDAL,
        // DRIVER 'KML')), the same sourcing method used for the FlatGeobuf/
        // GeoPackage/Shapefile fixtures. Exercises real-world shapes our
        // hand-written test fixtures don't: a <Folder>/<Schema> wrapper, the
        // <SchemaData>/<SimpleData> ExtendedData form, and a <Style> element
        // (LineStyle/PolyStyle) that must be ignored, not just absent.
        use crate::geometry::Geometry::{LineString, Point, Polygon};
        let bytes = include_bytes!("../tests/fixtures/duckdb_geoms.kml");
        let fc = convert(Format::Kml, Format::GeoJson, bytes).unwrap();
        let fc = geojson::from_json(&json::parse(std::str::from_utf8(&fc).unwrap()).unwrap()).unwrap();
        assert_eq!(fc.features.len(), 3);

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
        };
        let by_name = |name: &str| fc.features.iter().find(|f| prop(f, "name").as_str() == Some(name)).unwrap();

        let alpha = by_name("alpha");
        assert_eq!(prop(alpha, "n").as_str(), Some("1"));
        assert_eq!(alpha.geometry, Some(Point(Position::new(1.5, 2.5))));

        let beta = by_name("beta");
        assert_eq!(prop(beta, "n").as_str(), Some("2"));
        assert_eq!(beta.geometry, Some(LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)])));

        let gamma = by_name("gamma");
        assert_eq!(prop(gamma, "n").as_str(), Some("3"));
        assert_eq!(
            gamma.geometry,
            Some(Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 4.0), Position::new(0.0, 0.0)],
                vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 2.0), Position::new(1.0, 1.0)],
            ]))
        );
    }

    #[test]
    fn kml_composes_to_parquet_via_hub() {
        // The hub payoff, mirroring flatgeobuf_composes_to_parquet_via_hub:
        // KML -> Parquet (a path never written explicitly) then Parquet ->
        // GeoJSON must reproduce the same features as KML -> GeoJSON.
        let kml = include_bytes!("../tests/fixtures/duckdb_geoms.kml");
        let direct = geojson::from_json(
            &json::parse(std::str::from_utf8(&convert(Format::Kml, Format::GeoJson, kml).unwrap()).unwrap()).unwrap(),
        )
        .unwrap();
        let pq = convert(Format::Kml, Format::Parquet, kml).unwrap();
        let via_parquet = geojson::from_json(&json::parse(&geoparquet_to_geojson(&pq).unwrap()).unwrap()).unwrap();
        assert_eq!(sorted_geoms(&direct), sorted_geoms(&via_parquet));
    }

    #[test]
    fn kml_writer_round_trips_geometry_and_properties() {
        // GeoJSON -> KML (our writer) -> GeoJSON via the hub.
        let kml = convert(Format::GeoJson, Format::Kml, SAMPLE.as_bytes()).unwrap();
        let back = geojson::from_json(
            &json::parse(std::str::from_utf8(&convert(Format::Kml, Format::GeoJson, &kml).unwrap()).unwrap()).unwrap(),
        )
        .unwrap();
        let orig = geojson::from_json(&json::parse(SAMPLE).unwrap()).unwrap();
        assert_eq!(back.features.len(), orig.features.len());
        assert_eq!(sorted_geoms(&orig), sorted_geoms(&back));
    }

    #[test]
    fn kml_read_produces_wgs84() {
        // KML has no CRS channel at all — always WGS 84 by spec.
        let bytes = include_bytes!("../tests/fixtures/duckdb_geoms.kml");
        let fc = read_features(Format::Kml, bytes).unwrap();
        assert_eq!(fc.crs, Some(crate::crs::Crs::Wgs84));
    }

    // --- KMZ ------------------------------------------------------------

    #[test]
    fn kmz_writer_round_trips_through_our_own_reader() {
        // GeoJSON -> .kmz (our writer) -> GeoJSON via the hub.
        let kmz = convert(Format::GeoJson, Format::Kmz, SAMPLE.as_bytes()).unwrap();
        let back = geojson::from_json(
            &json::parse(std::str::from_utf8(&convert(Format::Kmz, Format::GeoJson, &kmz).unwrap()).unwrap()).unwrap(),
        )
        .unwrap();
        let orig = geojson::from_json(&json::parse(SAMPLE).unwrap()).unwrap();
        assert_eq!(back.features.len(), orig.features.len());
        assert_eq!(sorted_geoms(&orig), sorted_geoms(&back));
    }

    #[test]
    fn reads_a_real_gdal_libkml_networklink_kmz() {
        // Real `ogr2ogr -f LIBKML` output: a root doc.kml that is *only* a
        // <NetworkLink> pointing at layers/geoms.kml, where the actual
        // Placemarks live — the real-world shape that requires flattening
        // every *.kml archive entry, not just the first (see
        // kml::read_kmz's doc comment). Sourced from the same geometries as
        // duckdb_geoms.kml's oracle fixture: a polygon with a hole, a mixed
        // GeometryCollection, and a MultiPolygon. LIBKML's own writer always
        // emits an explicit third `,0` altitude ordinate on every coordinate
        // here, even though the source geometries carried no real elevation
        // — confirmed by inspecting the fixture's raw `layers/geoms.kml`
        // directly, not assumed — so every position below is `z: Some(0.0)`
        // once M9 routes KML's altitude into `Position::z` instead of
        // discarding it.
        use crate::geometry::Geometry::{GeometryCollection, LineString, MultiPolygon, Point, Polygon};
        let bytes = include_bytes!("../tests/fixtures/gdal_networklink.kmz");
        let fc = read_features(Format::Kmz, bytes).unwrap();
        assert_eq!(fc.crs, Some(crate::crs::Crs::Wgs84));
        assert_eq!(fc.features.len(), 3);

        let prop = |f: &Feature, k: &str| {
            f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v.clone()).unwrap()
        };
        let by_name = |name: &str| fc.features.iter().find(|f| prop(f, "name").as_str() == Some(name)).unwrap();

        assert_eq!(
            by_name("square-with-hole").geometry,
            Some(Polygon(vec![
                vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(4.0, 0.0, 0.0), Position::with_z(4.0, 4.0, 0.0), Position::with_z(0.0, 4.0, 0.0), Position::with_z(0.0, 0.0, 0.0)],
                vec![Position::with_z(1.0, 1.0, 0.0), Position::with_z(2.0, 1.0, 0.0), Position::with_z(2.0, 2.0, 0.0), Position::with_z(1.0, 2.0, 0.0), Position::with_z(1.0, 1.0, 0.0)],
            ]))
        );
        assert_eq!(
            by_name("mixed-collection").geometry,
            Some(GeometryCollection(vec![
                Point(Position::with_z(10.0, 10.0, 0.0)),
                LineString(vec![Position::with_z(10.0, 10.0, 0.0), Position::with_z(11.0, 11.0, 0.0)]),
            ]))
        );
        assert_eq!(
            by_name("multipoly").geometry,
            Some(MultiPolygon(vec![
                vec![vec![Position::with_z(0.0, 0.0, 0.0), Position::with_z(1.0, 0.0, 0.0), Position::with_z(1.0, 1.0, 0.0), Position::with_z(0.0, 0.0, 0.0)]],
                vec![vec![Position::with_z(5.0, 5.0, 0.0), Position::with_z(6.0, 5.0, 0.0), Position::with_z(6.0, 6.0, 0.0), Position::with_z(5.0, 5.0, 0.0)]],
            ]))
        );
    }

    #[test]
    fn kmz_composes_to_parquet_via_hub() {
        // The hub payoff, mirroring kml_composes_to_parquet_via_hub: KMZ ->
        // Parquet then Parquet -> GeoJSON must reproduce the same features
        // as KMZ -> GeoJSON directly.
        let kmz = include_bytes!("../tests/fixtures/gdal_networklink.kmz");
        let direct = geojson::from_json(
            &json::parse(std::str::from_utf8(&convert(Format::Kmz, Format::GeoJson, kmz).unwrap()).unwrap()).unwrap(),
        )
        .unwrap();
        let pq = convert(Format::Kmz, Format::Parquet, kmz).unwrap();
        let via_parquet = geojson::from_json(&json::parse(&geoparquet_to_geojson(&pq).unwrap()).unwrap()).unwrap();
        assert_eq!(sorted_geoms(&direct), sorted_geoms(&via_parquet));
    }
}
