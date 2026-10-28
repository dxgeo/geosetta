//! Orchestrates conversions between formats, routed through the shared
//! [`FeatureCollection`] IR: `read(from) -> FeatureCollection -> write(to)`.
//! Adding a format is one reader and one writer against the IR; every from/to
//! pair then composes automatically (hub-and-spoke).

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::format::{Format, KmlVersion};
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
///
/// It reports no warnings, read-side or write-side: a caller who needs to
/// know what a conversion lost calls [`read_features`] with a sink and runs
/// the checks `main` runs.
#[allow(dead_code)]
pub fn convert(from: Format, to: Format, input: &[u8]) -> Result<Vec<u8>> {
    write_features(to, &read_features(from, input, &mut Vec::new())?)
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
///
/// `warnings` receives what the read itself dropped — message bodies, without
/// the `warning: ` prefix the CLI adds — counted per cause, so a cause that
/// recurs on every feature is one line (`plans/lossy-conversion-warnings.org`
/// § 2b). A loss only the reader can see belongs here: by the time the IR
/// holds the collection, nothing records that it happened. Most formats never
/// write to it.
pub fn read_features(format: Format, input: &[u8], warnings: &mut Vec<String>) -> Result<FeatureCollection> {
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
        Format::Kml => kml::read_kml(input, warnings),
        Format::Kmz => kml::read_kmz(input, warnings),
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

/// How to write: what [`write_features_with`] takes beyond the format.
/// `Default` is what [`write_features`] writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// The KML version of a KML or KMZ output — `--kml-version`. `None` is
    /// the source's own version when the collection came from a KML or KMZ
    /// read, and KML 2.2 otherwise (`plans/kml-standard.org` D-K2).
    pub kml_version: Option<KmlVersion>,
    /// Write a GeoJSON or KML output on one line, with no whitespace between
    /// its members or elements — `--compact`, to save space. A KML read
    /// written back loses its own layout too, since that is what was asked
    /// for. Laid out otherwise (`plans/kml-layout.org` D-L2, D-L8). The other
    /// formats have no layout, and ignore it.
    pub compact: bool,
}

/// Encode the Feature IR into a supported output format, laid out where the
/// format has a layout.
pub fn write_features(format: Format, fc: &FeatureCollection) -> Result<Vec<u8>> {
    write_features_with(format, fc, WriteOptions::default())
}

/// [`write_features`], as `options` say.
///
/// A KML version is a statement about the output's content, so asking for
/// one is a usage error for any format but KML and KMZ rather than ignored.
pub fn write_features_with(format: Format, fc: &FeatureCollection, options: WriteOptions) -> Result<Vec<u8>> {
    if options.kml_version.is_some() && !matches!(format, Format::Kml | Format::Kmz) {
        return Err(Error::Usage(format!(
            "--kml-version chooses the KML version of the output, and the output here is {}",
            format.display_name()
        )));
    }
    let (version, compact) = (options.kml_version, options.compact);
    match format {
        Format::GeoJson => Ok(geojson::to_geojson_string(fc, compact).into_bytes()),
        Format::Parquet => Ok(features_to_parquet(fc)),
        Format::FlatGeobuf => Ok(flatgeobuf::write(fc)),
        Format::Csv => Ok(csv::write(fc)),
        Format::Wkt => Ok(write_wkt_lines(fc)),
        Format::Kml => Ok(kml::write_kml(fc, version, compact)),
        Format::Kmz => kml::write_kmz(fc, version, compact),
        // GeoPackage and Shapefile are handled outside this single-buffer path
        // (see read_features above) — GeoPackage via geopackage::write_layers,
        // Shapefile via shapefile::write, both called from main.rs.
        Format::Gpkg => Err(Error::Usage(
            "GeoPackage output is handled per-layer; this path should not be reached".into(),
        )),
        Format::Shapefile => Err(Error::Usage(
            "Shapefile output is handled via its sibling files; this path should not be reached".into(),
        )),
    }
}

/// [`write_features`] for KML or KMZ in a version the caller chooses —
/// `--kml-version`. Without it, [`write_features`] writes the source's own
/// version when the collection came from a KML or KMZ read, and KML 2.2
/// otherwise (`plans/kml-standard.org` D-K2).
///
/// A request for a version is a statement about the output's content, so it
/// is a usage error for any other format rather than ignored.
pub fn write_kml_features(format: Format, fc: &FeatureCollection, version: KmlVersion) -> Result<Vec<u8>> {
    write_features_with(format, fc, WriteOptions { kml_version: Some(version), ..WriteOptions::default() })
}

/// What a write of `fc` as `target` — in `kml_version` when the caller chose
/// one — cannot carry of what a KML or KMZ read held, as warning message
/// bodies. Empty for a collection that was not read from KML.
///
/// Pre-write, like the checks `main` collects for every conversion, and
/// separate from them because it needs the requested KML version: the same
/// collection written back to KML loses nothing as its own version and loses
/// its styles and folders as the other one (`plans/kml-standard.org` K4).
pub fn kml_write_warnings(fc: &FeatureCollection, target: Format, kml_version: Option<KmlVersion>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(version) = kml_version
        && matches!(target, Format::Kml | Format::Kmz)
    {
        out.extend(kml::version_change_warnings(fc, target, version));
    }
    out.extend(kml::drop_warnings(fc, target));
    out
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
                native: None,
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
    // Property columns. `columns_for`, not `infer_columns`: a column carrying
    // a declared type is written as that type rather than as whatever its
    // values re-infer to, which is what keeps an `Int16` 16-bit and a
    // `DateTime` a timestamp on the way out (defect 5).
    let columns = crate::schema::columns_for(fc);

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
    // The source's own file-level metadata, replayed only into its own format.
    let native = fc.native.as_ref().and_then(|n| match n.payload_for(Format::Parquet) {
        Some(crate::native::NativePayload::Parquet(p)) => Some(p),
        _ => None,
    });
    let geo =
        parquet::geo_metadata(&type_names, &bbox, fc.crs.as_ref(), native.and_then(|n| n.orientation.as_deref()));
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    parquet::write_geoparquet(&columns, &geometry, &geo, native, &names)
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
    let file = parquet::open_geoparquet(bytes)?;
    let num_cols = file.property_count();

    // Seed one feature per row with its geometry and an empty property vec,
    // freeing each row's WKB once it is decoded.
    let mut features = Vec::with_capacity(file.num_rows());
    let mut wkbs = file.geometry()?.into_iter();
    for _ in 0..file.num_rows() {
        let geometry = match wkbs.next() {
            Some(Some(wkb)) => Some(from_wkb(&wkb)?),
            Some(None) => None,
            None => return Err(Error::Parquet("the geometry column has fewer values than the file has rows".into())),
        };
        features.push(Feature {
            geometry,
            properties: Vec::with_capacity(num_cols),
            native: None,
        });
    }
    drop(wkbs);

    // Transpose columns → rows a column at a time: each is decoded, *drained*
    // into the rows (values move rather than being cloned, sharing one `Rc`
    // key across every row), and dropped before the next is decoded, so the
    // table is never held twice (`plans/performance.org`, *Memory*). Columns
    // are consumed in file order, so each row keeps that property order.
    let mut schema = Vec::new();
    for i in 0..num_cols {
        let col = file.property(i)?;
        // The type the file declared, kept so it can reach a target able to
        // express it — a `TIMESTAMP` column stops becoming text (defect 5).
        if let Some(ty) = col.decl {
            schema.push(crate::schema::FieldDecl {
                name: col.name.clone(),
                ty: Some(ty),
                ..Default::default()
            });
        }
        let key: Rc<str> = Rc::from(col.name);
        for (feature, value) in features.iter_mut().zip(col.values) {
            feature.properties.push((Rc::clone(&key), value));
        }
    }
    let (crs, native) = file.into_parts();
    Ok(FeatureCollection {
        features,
        crs,
        name: None,
        schema,
        native: (!native.is_empty()).then(|| {
            crate::native::Native::new(
                Format::Parquet,
                crate::native::NativePayload::Parquet(native),
            )
        }),
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
        // The `Z` is deliberate: `TIMESTAMP_MICROS` counts from the Unix
        // epoch, so it *is* UTC, and a bare `2020-01-01T00:00:00` is an
        // unzoned local time — a different fact. Added 2026-08-21, after a
        // `gpkg -> parquet -> gpkg` round trip was found to be quietly
        // dropping the designator GeoPackage requires.
        assert_eq!(prop(f0, "ts").as_str(), Some("2020-01-01T00:00:00Z"));
        let f2 = &fc.features[2];
        assert_eq!(prop(f2, "d").as_str(), Some("2020-01-03"));
        assert_eq!(prop(f2, "ts").as_str(), Some("2020-01-01T02:00:00Z"));
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
            Feature { geometry: Some(Point(Position::new(100.0, 100.0))), properties: vec![], native: None },
            Feature { geometry: Some(Point(Position::new(0.0, 0.0))), properties: vec![], native: None },
            Feature { geometry: Some(Point(Position::new(1.0, 1.0))), properties: vec![], native: None },
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
                native: None,
            },
            Feature {
                geometry: Some(crate::geometry::Geometry::Point(Position::with_zm(3.0, 4.0, 5.0, 6.0))),
                properties: vec![],
                native: None,
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
            native: None,
        }]);
        src.crs = Some(Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: Some("PROJCS[\"Web Mercator\"]".into()),
            projjson: None,
        }));
        let gpkg = crate::geopackage::write_layers(None, &[("l".into(), src)], &crate::geopackage::Carried::default(), false, false).unwrap();
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
            native: None,
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
        crate::shapefile::read(shp, dbf, None, None, &mut Vec::new()).unwrap()
    }

    /// The pipeline D1 in `plans/same-format-fidelity.org` is written to make
    /// work: a Shapefile's declared field widths survive a FlatGeobuf transit
    /// and land in the far-end `.dbf` intact.
    ///
    /// `gdal_hostile.*` is `ogr2ogr`'s own output and declares
    /// `String (80.0)` against values 6 and 8 bytes long, so nothing in the
    /// rows could produce the 80 — it can only have been carried.
    #[test]
    fn declared_shapefile_widths_survive_a_flatgeobuf_transit() {
        let source = shp_to_fc(
            include_bytes!("../tests/fixtures/gdal_hostile.shp"),
            include_bytes!("../tests/fixtures/gdal_hostile.dbf"),
        );
        let width = |fc: &FeatureCollection, name: &str| {
            fc.schema.iter().find(|d| d.name == name).and_then(|d| d.width)
        };
        assert_eq!(width(&source, "name"), Some(80), "the reader must recover the declaration");

        let transit = crate::flatgeobuf::read(&crate::flatgeobuf::write(&source)).unwrap();
        assert_eq!(width(&transit, "name"), Some(80), "FlatGeobuf must carry it across the pipe");

        let out = crate::shapefile::write(&transit).unwrap();
        let far_end = crate::shapefile::read(&out.shp, &out.dbf, None, None, &mut Vec::new()).unwrap();
        assert_eq!(width(&far_end, "name"), Some(80), "the far end must honour it");
        // The numeric declaration travels in its own two slots.
        let price = far_end.schema.iter().find(|d| d.name == "price").unwrap();
        assert_eq!((price.precision, price.scale), (Some(24), Some(15)));
    }

    /// The reason the schema is *modelled* rather than carried as a native
    /// (D1/D5): a middle stage can edit it. A native never crosses the pipe,
    /// so nothing but a modelled field could widen a declaration alongside the
    /// value that outgrew it.
    #[test]
    fn a_middle_stage_can_widen_a_declaration_mid_pipeline() {
        let source = shp_to_fc(
            include_bytes!("../tests/fixtures/gdal_hostile.shp"),
            include_bytes!("../tests/fixtures/gdal_hostile.dbf"),
        );
        let mut transit = crate::flatgeobuf::read(&crate::flatgeobuf::write(&source)).unwrap();

        // What an external tool does: lengthen a value and widen the
        // declaration that governs it.
        for f in &mut transit.features {
            if let Some((_, v)) = f.properties.iter_mut().find(|(k, _)| &**k == "name") {
                *v = crate::json::JsonValue::String("x".repeat(100));
            }
        }
        transit.schema.iter_mut().find(|d| d.name == "name").unwrap().width = Some(120);

        let out = crate::shapefile::write(&transit).unwrap();
        let far_end = crate::shapefile::read(&out.shp, &out.dbf, None, None, &mut Vec::new()).unwrap();
        assert_eq!(
            far_end.schema.iter().find(|d| d.name == "name").unwrap().width,
            Some(120),
            "the widened declaration, not the original 80"
        );
        assert_eq!(
            far_end.features[0].properties.iter().find(|(k, _)| &**k == "name").unwrap().1,
            crate::json::JsonValue::String("x".repeat(100)),
            "and the lengthened value survives it"
        );
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
            let reread = crate::shapefile::read(&encoded.shp, &encoded.dbf, None, None, &mut Vec::new()).unwrap();
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
        let back = crate::shapefile::read(&encoded.shp, &encoded.dbf, None, None, &mut Vec::new()).unwrap();
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
        // <SchemaData>/<SimpleData> ExtendedData form, typed by the schema's
        // `int` (`plans/kml-standard.org` D-K9), and a <Style> element
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
        assert_eq!(prop(alpha, "n"), json::JsonValue::Int(1));
        assert_eq!(alpha.geometry, Some(Point(Position::new(1.5, 2.5))));

        let beta = by_name("beta");
        assert_eq!(prop(beta, "n"), json::JsonValue::Int(2));
        assert_eq!(beta.geometry, Some(LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 0.0)])));

        let gamma = by_name("gamma");
        assert_eq!(prop(gamma, "n"), json::JsonValue::Int(3));
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
        let fc = read_features(Format::Kml, bytes, &mut Vec::new()).unwrap();
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
        let fc = read_features(Format::Kmz, bytes, &mut Vec::new()).unwrap();
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

    // --- M4: declared column types (defects 1, 2 and 5) --------------------

    /// Build a one-row collection whose single property is `value`, declared
    /// as `ty`. The declaration is what a real reader would have recovered.
    fn declared(name: &str, ty: crate::schema::DeclType, value: json::JsonValue) -> FeatureCollection {
        let mut fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![(Rc::from(name), value)],
            native: None,
        }]);
        fc.schema =
            vec![crate::schema::FieldDecl { name: name.into(), ty: Some(ty), ..Default::default() }];
        fc
    }

    fn decl_of(fc: &FeatureCollection, name: &str) -> Option<crate::schema::DeclType> {
        fc.schema.iter().find(|d| d.name == name).and_then(|d| d.ty)
    }

    /// Defect 5, FlatGeobuf: the hub's four column types cannot tell an
    /// `Int16` from an `Int64` or a `DateTime` from a `String`, so a spoke
    /// that re-infers from values alone silently widens or degrades every
    /// column. Each of these round-trips through the format's own writer and
    /// reader, which is the same-format invariant applied to the type.
    #[test]
    fn flatgeobuf_preserves_every_declared_column_type() {
        use crate::schema::DeclType::*;
        let int = |n: f64| json::JsonValue::Int((n) as i64);
        let cases = [
            (Bool, json::JsonValue::Bool(true)),
            (Int8, int(-7.0)),
            (UInt8, int(200.0)),
            (Int16, int(-3000.0)),
            (UInt16, int(60000.0)),
            (Int32, int(-70000.0)),
            (UInt32, int(3_000_000_000.0)),
            (Int64, int(9_000_000_000.0)),
            (UInt64, int(9_000_000_001.0)),
            (Float32, json::JsonValue::Float(1.5)),
            (Float64, json::JsonValue::Float(1.25)),
            (String, json::JsonValue::String("x".into())),
            (Json, json::JsonValue::String("{\"a\":1}".into())),
            (DateTime, json::JsonValue::String("2024-03-01T10:30:00Z".into())),
        ];
        for (ty, value) in cases {
            let fc = declared("v", ty, value.clone());
            let back = flatgeobuf::read(&flatgeobuf::write(&fc)).unwrap();
            assert_eq!(decl_of(&back, "v"), Some(ty), "{ty:?} did not survive");
            // The value has to survive the narrower on-disk encoding too — a
            // column written as `SHORT` stores two bytes, not eight.
            assert_eq!(back.features[0].properties[0].1, value, "{ty:?} value changed");
        }
    }

    /// Defect 1: FlatGeobuf encodes a null by *omitting* the cell, so a column
    /// null in every row has no cell anywhere — and a reader that materialized
    /// only present cells dropped the column entirely, declaration and all.
    /// This is the case that needs both halves of M4: the null to keep the
    /// column, and the declared type to keep it from re-inferring as `String`.
    #[test]
    fn flatgeobuf_keeps_an_all_null_column_and_its_declared_type() {
        let fc = declared("cnt", crate::schema::DeclType::Int16, json::JsonValue::Null);
        let back = flatgeobuf::read(&flatgeobuf::write(&fc)).unwrap();

        let keys: Vec<&str> = back.features[0].properties.iter().map(|(k, _)| &**k).collect();
        assert_eq!(keys, vec!["cnt"], "the all-null column vanished");
        assert_eq!(back.features[0].properties[0].1, json::JsonValue::Null);
        assert_eq!(decl_of(&back, "cnt"), Some(crate::schema::DeclType::Int16));
    }

    /// The precedence rule: a declaration is honoured where the data is silent
    /// and declined where the data contradicts it. A middle stage that writes
    /// 40000 into a declared `Int16` column has outgrown the declaration, and
    /// obeying it would truncate the number that was just written — the same
    /// asymmetry `declared_or` applies to widths.
    #[test]
    fn a_declared_type_the_data_outgrew_is_declined_not_obeyed() {
        let big = json::JsonValue::Int(40_000);
        let fc = declared("v", crate::schema::DeclType::Int16, big.clone());
        let back = flatgeobuf::read(&flatgeobuf::write(&fc)).unwrap();
        // Written as a wider type, so the value survives intact...
        assert_eq!(back.features[0].properties[0].1, big);
        // ...and the header now says what was actually written, not the stale
        // declaration.
        assert_ne!(decl_of(&back, "v"), Some(crate::schema::DeclType::Int16));

        // A type change the data contradicts outright is declined the same way.
        let text = declared("v", crate::schema::DeclType::Int16, json::JsonValue::String("no".into()));
        let back = flatgeobuf::read(&flatgeobuf::write(&text)).unwrap();
        assert_eq!(back.features[0].properties[0].1, json::JsonValue::String("no".into()));
        assert_eq!(decl_of(&back, "v"), Some(crate::schema::DeclType::String));
    }

    /// Defect 2: GeoPackage stores a `BOOLEAN` as SQLite `INTEGER` 0/1, per
    /// spec, so a reader that looks only at the storage class hands back a
    /// number. The writer was always right; the declaration was on disk and
    /// unread, which is why a boolean could not survive its own format.
    #[test]
    fn geopackage_round_trips_a_boolean_as_a_boolean() {
        let fc = declared("flag", crate::schema::DeclType::Bool, json::JsonValue::Bool(true));
        let bytes = crate::geopackage::write_layers(None, &[("t".into(), fc)], &crate::geopackage::Carried::default(), false, false).unwrap();
        let back = crate::geopackage::read_layers(&bytes).unwrap().remove(0).1;
        assert_eq!(back.features[0].properties[0].1, json::JsonValue::Bool(true));
        assert_eq!(decl_of(&back, "flag"), Some(crate::schema::DeclType::Bool));
    }

    /// GeoPackage's other declared types survive their own format too — the
    /// same defect-5 obligation, in SQL's vocabulary rather than FlatGeobuf's.
    #[test]
    fn geopackage_preserves_declared_column_types() {
        use crate::schema::DeclType::*;
        let int = |n: f64| json::JsonValue::Int((n) as i64);
        let cases = [
            (Int16, int(-3000.0)),
            (Int32, int(-70000.0)),
            (Int64, int(9_000_000_000.0)),
            (Float64, json::JsonValue::Float(1.25)),
            (String, json::JsonValue::String("x".into())),
            (DateTime, json::JsonValue::String("2024-03-01T10:30:00Z".into())),
            (Date, json::JsonValue::String("2024-03-01".into())),
        ];
        for (ty, value) in cases {
            let fc = declared("v", ty, value.clone());
            let bytes =
                crate::geopackage::write_layers(None, &[("t".into(), fc)], &crate::geopackage::Carried::default(), false, false).unwrap();
            let back = crate::geopackage::read_layers(&bytes).unwrap().remove(0).1;
            assert_eq!(decl_of(&back, "v"), Some(ty), "{ty:?} did not survive");
            assert_eq!(back.features[0].properties[0].1, value, "{ty:?} value changed");
        }
    }

    /// Shapefile's `.dbf` names five field types, two of which the inferred
    /// `ColumnType` cannot reach: `L` and `D` both store as text, so a column
    /// with nothing left to infer from would stop being logical or a date.
    #[test]
    fn shapefile_preserves_the_dbf_field_types_it_can_express() {
        use crate::schema::DeclType::*;
        let cases = [
            (Bool, json::JsonValue::Bool(true), Bool),
            (Date, json::JsonValue::String("2024-03-01".into()), Date),
            (String, json::JsonValue::String("x".into()), String),
            // dBase has one numeric field type; an integer is a zero-scale
            // numeric, which is what comes back.
            (Int64, json::JsonValue::Int(42), Int64),
        ];
        for (ty, value, expected) in cases {
            let fc = declared("v", ty, value);
            let out = crate::shapefile::write(&fc).unwrap();
            let back = crate::shapefile::read(&out.shp, &out.dbf, None, None, &mut Vec::new()).unwrap();
            assert_eq!(decl_of(&back, "v"), Some(expected), "{ty:?} did not survive");
        }
    }

    /// A declaration crossing a format with no slot for one is still lost —
    /// deliberately, and it warns. The asymmetry is the whole rule: the target's
    /// incapacity excuses the drop, and a test asserting the declaration is
    /// *gone* matters as much as the ones asserting it survived.
    #[test]
    fn formats_without_a_schema_slot_drop_the_declaration_and_say_so() {
        let fc = declared("cnt", crate::schema::DeclType::Int16,
            json::JsonValue::Int(7));

        for target in [Format::GeoJson, Format::Csv, Format::Wkt] {
            assert!(!target.carries_column_schema(), "{target:?}");
            let w = fc.schema_drop_warning(target).expect("the drop must warn");
            assert!(w.contains("cnt"), "{w}");

            // And it really is gone at the far end, not merely unwarned.
            let bytes = write_features(target, &fc).unwrap();
            let back = read_features(target, &bytes, &mut Vec::new()).unwrap();
            assert_eq!(decl_of(&back, "cnt"), None, "{target:?} kept a declaration it cannot hold");
        }
    }

    /// The spokes that *do* carry declarations stay silent, which is the
    /// other half of the same contract.
    #[test]
    fn formats_with_a_schema_slot_do_not_warn() {
        let fc = declared("cnt", crate::schema::DeclType::Int16,
            json::JsonValue::Int(7));
        for target in [Format::FlatGeobuf, Format::Shapefile, Format::Gpkg, Format::Parquet, Format::Kml, Format::Kmz] {
            assert!(target.carries_column_schema(), "{target:?}");
            assert_eq!(fc.schema_drop_warning(target), None, "{target:?}");
        }
    }

    /// GeoParquet was the last of the four declaring spokes to be wired up.
    /// Its type system is the widest of them: narrow integers ride an
    /// `INT32`/`INT64` physical type with the real width in `converted_type`,
    /// and a timestamp is a genuine `TIMESTAMP_MILLIS` rather than the text the
    /// IR holds — so this round trip exercises the parse/format inverse pair as
    /// well as the annotation.
    #[test]
    fn geoparquet_preserves_every_declared_column_type() {
        use crate::schema::DeclType::*;
        let int = |n: f64| json::JsonValue::Int((n) as i64);
        let cases = [
            (Bool, json::JsonValue::Bool(true)),
            (Int8, int(-7.0)),
            (UInt8, int(200.0)),
            (Int16, int(-3000.0)),
            (UInt16, int(60000.0)),
            (Int32, int(-70000.0)),
            (UInt32, int(3_000_000_000.0)),
            (Int64, int(9_000_000_000.0)),
            (Float32, json::JsonValue::Float(1.5)),
            (Float64, json::JsonValue::Float(1.25)),
            (String, json::JsonValue::String("x".into())),
            // A JSON-annotated column decodes to *structured* JSON, not text
            // (`push_json_embedded`), so the round trip is Object -> Object:
            // the writer stringifies it and the annotation makes the reader
            // parse it back.
            (
                Json,
                json::JsonValue::Object(vec![(
                    "a".into(),
                    json::JsonValue::Int(1),
                )]),
            ),
            (Date, json::JsonValue::String("2024-03-01".into())),
            (DateTime, json::JsonValue::String("2024-03-01T10:30:00Z".into())),
        ];
        for (ty, value) in cases {
            let fc = declared("v", ty, value.clone());
            let back = parquet_to_features(&features_to_parquet(&fc)).unwrap();
            assert_eq!(decl_of(&back, "v"), Some(ty), "{ty:?} did not survive");
            assert_eq!(back.features[0].properties[0].1, value, "{ty:?} value changed");
        }
    }

    /// A declared temporal type whose text this writer cannot parse back is
    /// declined rather than guessed at — writing a wrong epoch number would be
    /// worse than keeping exact text. Sub-millisecond precision is declined for
    /// the same reason: truncating it would silently lose digits.
    #[test]
    fn geoparquet_declines_a_timestamp_it_cannot_encode_exactly() {
        for text in ["not a date", "2024-03-01T10:30:00.123456"] {
            let fc = declared(
                "v",
                crate::schema::DeclType::DateTime,
                json::JsonValue::String(text.into()),
            );
            let back = parquet_to_features(&features_to_parquet(&fc)).unwrap();
            // Kept as exact text rather than turned into a wrong instant.
            assert_eq!(back.features[0].properties[0].1, json::JsonValue::String(text.into()));
            assert_eq!(decl_of(&back, "v"), Some(crate::schema::DeclType::String));
        }
    }

    // --- M3: the GeoParquet spoke's natives ---------------------------------

    fn parquet_native(fc: &FeatureCollection) -> &crate::native::ParquetNative {
        match fc.native.as_ref().and_then(|n| n.payload_for(Format::Parquet)) {
            Some(crate::native::NativePayload::Parquet(p)) => p,
            _ => panic!("expected a GeoParquet native"),
        }
    }

    /// The invariant for this spoke: real `ogr2ogr` output carries three
    /// `key_value_metadata` entries and geosetta read exactly one, so a
    /// GeoParquet rewritten by this crate used to lose `ARROW:schema` (what the
    /// whole Arrow ecosystem reads) and `gdal:metadata` outright.
    #[test]
    fn geoparquet_carries_its_file_metadata_through_its_own_format() {
        let source = parquet_to_features(include_bytes!(
            "../tests/fixtures/gdal_covering_bbox.parquet"
        ))
        .unwrap();
        let keys: Vec<&str> =
            parquet_native(&source).key_value.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"ARROW:schema"), "{keys:?}");
        assert!(keys.contains(&"gdal:metadata"), "{keys:?}");

        let back = parquet_to_features(&features_to_parquet(&source)).unwrap();
        assert_eq!(parquet_native(&back).key_value, parquet_native(&source).key_value);
        // `orientation` rides in the `geo` object rather than beside it, and is
        // the one member of that object this crate had never read.
        assert_eq!(parquet_native(&back).orientation, parquet_native(&source).orientation);
    }

    /// `ARROW:schema` restates the file's own column types, so it is the one
    /// carried key that can go *stale*. An Arrow reader trusts it over the
    /// Parquet schema, so replaying it into a file whose columns a middle stage
    /// changed would hand back types the file does not have — D4's rule against
    /// replaying derived data, applied to the only derived key.
    #[test]
    fn a_stale_arrow_schema_is_dropped_once_the_columns_change() {
        let source = parquet_to_features(include_bytes!(
            "../tests/fixtures/gdal_covering_bbox.parquet"
        ))
        .unwrap();
        assert!(
            parquet_native(&source).key_value.iter().any(|(k, _)| k == "ARROW:schema"),
            "the fixture must carry one for this test to mean anything"
        );

        // What a middle stage does: add a column the source never had.
        let mut edited = source.clone();
        for f in &mut edited.features {
            f.properties.push((Rc::from("added"), json::JsonValue::Bool(true)));
        }

        let back = parquet_to_features(&features_to_parquet(&edited)).unwrap();
        let keys: Vec<&str> =
            parquet_native(&back).key_value.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains(&"ARROW:schema"), "a stale schema was replayed: {keys:?}");
        // The opaque key is not schema-derived, so it still travels.
        assert!(keys.contains(&"gdal:metadata"), "{keys:?}");
    }

    // --- the recast matrix ------------------------------------------------
    //
    // Every declared type, through every format that carries a declaration.
    // These exist because the type surface is eight `match` tables (a reader
    // and a writer per declaring spoke) and every test that touched it before
    // 2026-08-21 was an *example* test — one type someone thought of. Three
    // silent recasts survived that regime: `REAL` read back 32-bit, GeoPackage
    // writing every unsigned width as the same-width *signed* spelling (GDAL
    // read 3000000000 back as -1294967296), and FlatGeobuf turning a `Binary`
    // column into text. See `plans/lossy-conversion-warnings.org`.

    /// A value of the right kind for `ty`, so the declaration is not
    /// contradicted by the data.
    ///
    /// **Catch-all-free on purpose.** `FieldDecl::type_for` lets the data win
    /// where it *contradicts* the declaration (decision 11), so handing an
    /// integer to a `Date` column measures that rule instead of the recast and
    /// reports a loss that is not there — which is exactly what the first draft
    /// of this sweep did. A seventeenth `DeclType` stops the build here until
    /// someone says what a value of it looks like.
    fn representative_value(ty: crate::schema::DeclType) -> json::JsonValue {
        use crate::schema::DeclType::*;
        let int = json::JsonValue::Int;
        let text = |s: &str| json::JsonValue::String(s.into());
        match ty {
            Bool => json::JsonValue::Bool(true),
            Int8 => int(-42),
            UInt8 => int(200),
            Int16 => int(-9000),
            UInt16 => int(60_000),
            Int32 => int(-2_000_000_000),
            // Above `i32::MAX`, so a declaration narrowed to `Int32` is not
            // merely wrong on paper — it cannot hold the value beside it.
            UInt32 => int(3_000_000_000),
            // Past 2^53, where an `f64` stops carrying integers exactly. These
            // used to be small enough to survive a double, which is how a
            // `gpkg -> gpkg` run turning 9007199254740993 into ...992 went
            // unnoticed by a sweep that tested every type and every format.
            Int64 => int(9_007_199_254_740_993),
            UInt64 => int(9_007_199_254_740_993),
            Float32 => json::JsonValue::Float(1.5),
            // Enough digits to need 64 bits, but not so large that its
            // fixed-point decimal text overruns a `.dbf` field — a magnitude
            // like 1e300 is 308 characters and the Shapefile writer refuses it,
            // correctly, which measures the width rule rather than the recast.
            Float64 => json::JsonValue::Float(1.234_567_890_123_45),
            String => text("plain"),
            Json => text("{\"a\":1}"),
            DateTime => text("2019-05-01T12:00:00Z"),
            Date => text("2019-05-01"),
            // Real bytes, not text that looks like bytes: the whole point of
            // `JsonValue::Binary` is that a writer can tell the difference.
            Binary => json::JsonValue::Binary(vec![0x00, 0x01, 0xfe, 0xff]),
        }
    }

    /// A one-column collection declaring `ty`, with a value that fits it.
    fn one_declared_column(ty: crate::schema::DeclType) -> FeatureCollection {
        let mut fc = FeatureCollection::new(vec![crate::feature::Feature {
            geometry: Some(crate::geometry::Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![("v".into(), representative_value(ty))],
            native: None,
        }]);
        fc.schema = vec![crate::schema::FieldDecl {
            name: "v".into(),
            ty: Some(ty),
            ..Default::default()
        }];
        fc
    }

    /// Write `fc` as `format`, read it back, and return the column's value.
    ///
    /// The companion to [`declared_after`], and it exists because for a day
    /// this file checked only declarations: a GeoPackage round trip that
    /// turned every blob into `Null` passed the whole sweep cleanly, because
    /// the `BLOB` *declaration* survived intact. **A type-fidelity test that
    /// never looks at a value can be green while the values are destroyed.**
    fn value_after(format: Format, fc: &FeatureCollection) -> json::JsonValue {
        let back = round_trip(format, fc);
        back.features[0]
            .properties
            .iter()
            .find(|(k, _)| &**k == "v")
            .map(|(_, v)| v.clone())
            .unwrap_or(json::JsonValue::Null)
    }

    /// Write `fc` as `format`, read it back, and return what the column is
    /// declared as on the far side. Every declaring spoke, one seam.
    fn declared_after(format: Format, fc: &FeatureCollection) -> Option<crate::schema::DeclType> {
        round_trip(format, fc).schema.iter().find(|d| d.name == "v").and_then(|d| d.ty)
    }

    fn round_trip(format: Format, fc: &FeatureCollection) -> FeatureCollection {
        match format {
            Format::Gpkg => {
                let layers = vec![("l".to_string(), fc.clone())];
                let bytes = crate::geopackage::write_layers(None, &layers, &crate::geopackage::Carried::default(), false, false).unwrap();
                crate::geopackage::read_layers(&bytes).unwrap().remove(0).1
            }
            Format::FlatGeobuf => {
                crate::flatgeobuf::read(&crate::flatgeobuf::write(fc)).unwrap()
            }
            Format::Parquet => parquet_to_features(&features_to_parquet(fc)).unwrap(),
            Format::Shapefile => {
                let encoded = crate::shapefile::write(fc).unwrap();
                crate::shapefile::read(&encoded.shp, &encoded.dbf, None, None, &mut Vec::new()).unwrap()
            }
            Format::Kml | Format::Kmz => read_features(format, &write_features(format, fc).unwrap(), &mut Vec::new()).unwrap(),
            other => panic!("{other:?} carries no column schema"),
        }
    }

    fn declaring_formats() -> Vec<Format> {
        [
            Format::GeoJson,
            Format::Parquet,
            Format::FlatGeobuf,
            Format::Csv,
            Format::Wkt,
            Format::Gpkg,
            Format::Shapefile,
            Format::Kml,
            Format::Kmz,
        ]
        .into_iter()
        .filter(|f| f.carries_column_schema())
        .collect()
    }

    /// **The sweep.** For every declared type and every declaring format:
    /// what comes back must be what `Format::recast` says, and that must
    /// either admit the original or warn.
    ///
    /// This is what makes `Format::recast` safe to consult — it is a ninth
    /// statement of a mapping that lives in eight `match` arms, and this test
    /// is the only thing keeping it honest.
    #[test]
    fn every_declared_type_survives_or_is_admitted() {
        let mut narrowed = Vec::new();
        for format in declaring_formats() {
            for ty in crate::schema::DeclType::ALL {
                let fc = one_declared_column(ty);
                let landed = declared_after(format, &fc);
                let predicted = format.recast(ty);
                assert_eq!(
                    landed,
                    predicted,
                    "{format:?} recast {ty:?} to {landed:?}, but Format::recast predicts \
                     {predicted:?} — one of the two is wrong, and they must not drift"
                );

                let Some(landed) = landed else { continue };
                let warned = fc.decl_recast_warning(format).is_some();
                if landed.admits(ty) {
                    assert!(!warned, "{format:?} keeps {ty:?} as {landed:?} but warned anyway");
                } else {
                    assert!(warned, "{format:?} narrowed {ty:?} to {landed:?} *silently*");
                    narrowed.push(format!("{format:?} {ty:?}->{landed:?}"));
                }
            }
        }
        // Pinned so a *new* narrowing cannot be added without appearing here.
        // Each entry is a genuine capability gap, not a mapping bug: dBase has
        // one numeric field type and no JSON/timestamp/blob, and FlatGeobuf has
        // no DATE and cannot materialize a BINARY cell.
        assert_eq!(
            narrowed,
            [
                // FlatGeobuf has no DATE. DATETIME keeps it temporal, which is
                // what ogr2ogr does, but a date is not an instant.
                "FlatGeobuf Date->DateTime",
                // dBase has no JSON, no timestamp and no blob field type, and
                // neither does GDAL's shapefile driver.
                "Shapefile Json->String",
                "Shapefile DateTime->String",
                "Shapefile Binary->String",
                // KML's eight field types (OGC 12-007r2 §9.9) hold no 64-bit
                // integer exactly and name no JSON, time, date or bytes. A
                // `timestamp`/`begin`/`end` column is a `<TimeStamp>` or
                // `<TimeSpan>` and keeps `DateTime`; that is a key's rule, not
                // a type's, so the sweep's `v` column does not see it.
                "Kml Int64->String",
                "Kml UInt64->String",
                "Kml Json->String",
                "Kml DateTime->String",
                "Kml Date->String",
                "Kml Binary->String",
                "Kmz Int64->String",
                "Kmz UInt64->String",
                "Kmz Json->String",
                "Kmz DateTime->String",
                "Kmz Date->String",
                "Kmz Binary->String",
            ]
        );
    }

    /// **Values, not declarations.** Every declared type through every
    /// declaring format, asserting the *value* comes back unchanged wherever
    /// the format kept the type.
    ///
    /// This is the assertion whose absence let a GeoPackage round trip destroy
    /// every blob in a file while the sweep above stayed green: `BLOB` in,
    /// `BLOB` out, and `X'0102'` silently replaced by `NULL` in between,
    /// because the IR had nowhere to put bytes and the reader returned `Null`.
    #[test]
    fn a_value_survives_wherever_its_declared_type_does() {
        for format in declaring_formats() {
            for ty in crate::schema::DeclType::ALL {
                // Where the type is recast the value may legitimately be
                // re-rendered (bytes become hex in a format with no blob
                // column), and `decl_recast_warning` announces it. Where the
                // type survives, nothing may touch the value.
                if format.recast(ty) != Some(ty) {
                    continue;
                }
                let fc = one_declared_column(ty);
                let before = fc.features[0].properties[0].1.clone();
                let after = value_after(format, &fc);

                // A `Json` column is compared *as JSON*, not as text: the
                // GeoParquet reader parses one, deliberately, so an object
                // comes back an object rather than a quoted string. That is a
                // change of representation and not of content — but it is also
                // not byte-preserving (a source with `{"a": 1}` loses the
                // space), which makes it an instance of the open
                // canonicalisation question in `native-inventory.org`
                // § QUESTIONS. Left as-is here rather than settled in passing.
                if ty == crate::schema::DeclType::Json {
                    let parse = |v: &json::JsonValue| match v {
                        json::JsonValue::String(text) => json::parse(text).unwrap(),
                        other => other.clone(),
                    };
                    assert_eq!(
                        parse(&after),
                        parse(&before),
                        "{format:?} changed the content of a Json column"
                    );
                    continue;
                }
                assert_eq!(
                    after, before,
                    "{format:?} kept the {ty:?} declaration but changed the value"
                );
            }
        }
    }

    /// The same sweep with *no* declaration, which is where the `REAL` bug
    /// actually lived: the GeoPackage writer emits `REAL` for an undeclared
    /// double, and the reader read `REAL` as `Float32` — so a plain double
    /// acquired a `Float32` declaration out of nowhere. The declared path
    /// (`Float32` → `FLOAT` → `Float32`) was closed the whole time, so no
    /// amount of declared-only testing could have found it.
    #[test]
    fn an_undeclared_column_never_acquires_a_narrower_declaration() {
        for format in declaring_formats() {
            for ty in crate::schema::DeclType::ALL {
                let mut fc = one_declared_column(ty);
                let hub = ty.hub();
                fc.schema.clear();
                let landed = declared_after(format, &fc);
                let Some(landed) = landed else { continue };
                // The hub must survive *unless* the format genuinely has no
                // such column, which `Format::recast` already states: Shapefile
                // has no blob field type, so bytes become text there and the
                // hub legitimately changes. Deriving the expectation from
                // `recast` rather than exempting a case by name keeps the two
                // from disagreeing.
                let expected_hub = format.recast(ty).map(|t| t.hub()).unwrap_or(hub);
                assert!(
                    landed.hub() == expected_hub,
                    "{format:?} read an undeclared {hub:?} column back as {landed:?}, \
                     but recast predicts a {expected_hub:?} column"
                );
                // Whatever it invents must hold every value the hub type can.
                if let Some(widest) = match hub {
                    crate::schema::ColumnType::Int64 => Some(crate::schema::DeclType::Int64),
                    crate::schema::ColumnType::Double => Some(crate::schema::DeclType::Float64),
                    _ => None,
                } {
                    assert!(
                        landed.admits(widest),
                        "{format:?} gave an undeclared {hub:?} column the narrower \
                         declaration {landed:?}"
                    );
                }
            }
        }
    }

    /// **A documented open case, not a regression.** `#[ignore]`d on purpose:
    /// it passes, and it is here to state a behaviour rather than to guard one.
    ///
    /// A column holding *both* a large integer and a fractional value infers as
    /// `Double`, so the integer loses its low bits above 2^53. That is
    /// inference's judgement rather than a lost guarantee — the column is a
    /// double because something *else* in it was fractional, and no format this
    /// crate writes can hold both exactly. GDAL's `OFTReal` behaves the same.
    ///
    /// **So the thing that is missing is not exactness, it is the warning.**
    /// Anyone picking this up should not try to make the value survive; they
    /// should add a fifth check to `collect_conversion_warnings` and then
    /// replace the assertions below with one that the warning fired. The reason
    /// it has not been written is the noise bar in
    /// `plans/lossy-conversion-warnings.org` § THE RULE: this fires only for a
    /// genuinely mixed column *and* an integer past 2^53, which may be rare
    /// enough that the check costs more than it returns. That is a judgement
    /// call, and it is recorded as one.
    ///
    /// The fixture carries the contrast in one file: `pure_int` is every-value-
    /// an-integer and survives exactly, `mixed` does not.
    #[test]
    #[ignore = "documents a known, inherent case; see the doc comment"]
    fn a_mixed_number_column_loses_precision_above_2_53() {
        let text = include_str!("../tests/fixtures/hand_mixed_number.geojson");
        let fc = geojson::from_json(&json::parse(text).unwrap()).unwrap();
        let round_tripped = crate::flatgeobuf::read(&crate::flatgeobuf::write(&fc)).unwrap();

        let column = |name: &str| -> Vec<json::JsonValue> {
            round_tripped
                .features
                .iter()
                .map(|f| f.properties.iter().find(|(k, _)| &**k == name).unwrap().1.clone())
                .collect()
        };

        // Every value an integer: the column is `Int64` and 2^53 is no barrier.
        // This is what the number model was split for, and it holds.
        assert!(
            column("pure_int").contains(&json::JsonValue::Int(9_007_199_254_740_993)),
            "a pure-integer column must survive exactly: {:?}",
            column("pure_int")
        );

        // The same integer beside a fractional value: the column is `Double`,
        // and the integer arrives one short.
        let mixed = column("mixed");
        assert!(
            mixed.contains(&json::JsonValue::Float(9_007_199_254_740_992.0)),
            "expected the documented loss, got {mixed:?}"
        );
        assert!(
            !mixed.contains(&json::JsonValue::Int(9_007_199_254_740_993)),
            "the mixed column became exact — good news, and this test needs rewriting"
        );
    }

    fn gpkg_native(fc: &FeatureCollection) -> &crate::native::GpkgNative {
        match fc.native.as_ref().and_then(|n| n.payload_for(Format::Gpkg)) {
            Some(crate::native::NativePayload::Gpkg(g)) => g,
            _ => panic!("expected a GeoPackage native"),
        }
    }

    /// One layer of `gdal_hostile.gpkg`, by name.
    fn hostile_layer(name: &str) -> FeatureCollection {
        let bytes = include_bytes!("../tests/fixtures/gdal_hostile.gpkg");
        crate::geopackage::read_layers(bytes)
            .unwrap()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, fc)| fc)
            .unwrap_or_else(|| panic!("no layer {name}"))
    }

    /// The invariant for the GeoPackage spoke: a layer's title, description and
    /// column descriptions are members the format models first-class and this
    /// crate used to overwrite — `gpkg_contents.identifier` was hardcoded to the
    /// table name and `description` to the empty string, the same shape as
    /// defect 4's hardcoded FlatGeobuf header name.
    #[test]
    fn a_geopackage_layers_own_description_survives_its_own_format() {
        let source = hostile_layer("cities");
        let native = gpkg_native(&source).clone();
        assert_eq!(native.identifier.as_deref(), Some("Bykerne — København og Zürich"));
        assert!(native.description.as_deref().unwrap().starts_with("Hostile fixture"));
        assert_eq!(native.data_columns.len(), 2);
        assert_eq!(native.constraints.len(), 1);

        // Out and back through this crate's own writer.
        let layers = vec![("cities".to_string(), source.clone())];
        let bytes = crate::geopackage::write_layers(None, &layers, &crate::geopackage::Carried::default(), false, false).unwrap();
        let back = crate::geopackage::read_layers(&bytes).unwrap();
        assert_eq!(*gpkg_native(&back[0].1), native, "the layer's members did not survive");
    }

    /// **A `gpkg -> gpkg` run keeps the source's `gpkg_contents.last_change`**
    /// instead of overwriting it with the writer's sentinel.
    ///
    /// Until 2026-08-24 it did overwrite it, and that was a real loss under
    /// § THE RULE: a value the source held that the output did not. The standard
    /// defines this column — "timestamp of last change to content" — unlike
    /// `gpkg_metadata_reference.timestamp`, which it leaves undefined, so there
    /// is something definite being destroyed.
    ///
    /// *The stamp is never regenerated, only carried*, because the rule is that
    /// it follows whoever last changed the content and that is never geosetta.
    /// A cross-format source has none to carry and gets the sentinel — asserted
    /// below too, since that is the half a future change would break first.
    #[test]
    fn a_geopackage_last_change_survives_its_own_format() {
        let source = hostile_layer("second");
        let carried = gpkg_native(&source).last_change.clone().expect("ogr2ogr wrote one");
        assert_ne!(carried, crate::geopackage::LAST_CHANGE, "fixture is not exercising this");

        let bytes = crate::geopackage::write_layers(
            None,
            &[("second".to_string(), source)], &crate::geopackage::Carried::default(),
            false,
            false)
        .unwrap();
        assert_eq!(last_change_of(&bytes, "second"), carried, "the source's stamp was overwritten");

        // A layer with no native — every cross-format source — still gets the
        // sentinel, never a clock reading.
        let bare = crate::FeatureCollection::new(Vec::new());
        let bytes = crate::geopackage::write_layers(
            None,
            &[("fresh".to_string(), bare)], &crate::geopackage::Carried::default(),
            false,
            false)
        .unwrap();
        assert_eq!(
            last_change_of(&bytes, "fresh"),
            crate::geopackage::LAST_CHANGE,
            "a layer with nothing to carry did not get the sentinel"
        );
    }

    /// **A GeoPackage this crate wrote carries no `last_change` payload**, so
    /// D3's "None when nothing was unmodelled" still holds for our own output.
    ///
    /// The reader drops the stamp when it is the writer's own sentinel, on the
    /// `identifier` precedent — a value the writer regenerates is not content.
    /// Without that filter every geosetta-written file would round-trip into one
    /// carrying a native, which is the payload-on-every-file outcome
    /// [`crate::GpkgNative`]'s `description` reasoning exists to avoid.
    ///
    /// /Written because a mutation went uncaught: deleting the reader's filter
    /// broke nothing in the suite. The writer half was already covered by
    /// `a_geopackage_last_change_survives_its_own_format`; this is its mirror./
    #[test]
    fn our_own_last_change_sentinel_is_not_carried_back() {
        let layer = crate::FeatureCollection::new(Vec::new());
        let bytes =
            crate::geopackage::write_layers(None, &[("fresh".to_string(), layer)], &crate::geopackage::Carried::default(), false, false)
                .unwrap();
        let back = crate::geopackage::read_layers(&bytes).unwrap();
        assert!(
            back[0].1.native.is_none(),
            "our own output round-tripped into a payload: {:?}",
            back[0].1.native
        );
    }

    /// `gpkg_contents.last_change` for one table, read back out of written bytes.
    fn last_change_of(bytes: &[u8], table: &str) -> String {
        let db = crate::sqlite::Database::open(bytes).unwrap();
        let tables = db.tables().unwrap();
        let contents = tables.iter().find(|t| t.name == "gpkg_contents").unwrap();
        let name_col = contents.columns.iter().position(|c| c == "table_name").unwrap();
        let stamp_col = contents.columns.iter().position(|c| c == "last_change").unwrap();
        db.read_rows(contents)
            .unwrap()
            .iter()
            .find(|row| matches!(&row[name_col], crate::sqlite::Value::Text(t) if t == table))
            .map(|row| match &row[stamp_col] {
                crate::sqlite::Value::Text(t) => t.clone(),
                other => panic!("last_change is not text: {other:?}"),
            })
            .expect("no gpkg_contents row")
    }

    /// D3: a spoke that recovered nothing constructs no payload. `ogr2ogr`
    /// writes `identifier = table_name` for every layer it creates, so treating
    /// that as content would put a native on essentially every GeoPackage.
    ///
    /// ***Narrowed 2026-08-24, when `last_change` started being carried.*** This
    /// asserted `native.is_none()`, which was a fair proxy for "the identifier
    /// was dropped" only while `identifier`, `description` and the schema rows
    /// were the whole payload. A GDAL-written `gpkg_contents` row also holds a
    /// real `last_change`, and carrying that is the point — so an `ogr2ogr`
    /// layer now *does* construct a payload, and the proxy no longer means what
    /// the test name says.
    ///
    /// /D3 is untouched by that: "None when nothing was unmodelled" still holds
    /// for the files this crate writes, whose `last_change` is the sentinel the
    /// reader drops. What changed is that an ogr2ogr file is no longer a file
    /// with nothing to carry./ The assertion is now on the member this test is
    /// actually about, plus the two that shared its reasoning.
    #[test]
    fn an_identifier_that_only_repeats_the_layer_name_is_not_carried() {
        // `second` is the fixture's untouched layer: ogr2ogr's defaults.
        let layer = hostile_layer("second");
        let native = gpkg_native(&layer);
        assert_eq!(native.identifier, None, "identifier repeating the name was carried");
        assert_eq!(native.description, None, "an empty description was carried");
        assert!(native.data_columns.is_empty() && native.constraints.is_empty());
        // And the only reason this layer has a payload at all:
        assert!(native.last_change.is_some(), "ogr2ogr's last_change was not carried");
    }

    /// D4, decision 14's shape: an opaque member can also be derived. A
    /// `gpkg_data_columns` row naming a column the layer no longer has is a
    /// description of nothing, and GDAL would report it as an alias on a
    /// missing field.
    #[test]
    fn a_data_column_row_is_dropped_when_its_column_goes() {
        let mut fc = hostile_layer("cities");
        for feature in &mut fc.features {
            feature.properties.retain(|(key, _)| &**key != "price");
        }
        fc.schema.retain(|decl| decl.name != "price");
        let layers = vec![("cities".to_string(), fc)];
        let bytes = crate::geopackage::write_layers(None, &layers, &crate::geopackage::Carried::default(), false, false).unwrap();
        let back = crate::geopackage::read_layers(&bytes).unwrap();
        let native = gpkg_native(&back[0].1);

        // `name`'s row stays; `price`'s goes, and the constraint only `price`
        // referred to goes with it rather than lingering as an unused domain.
        assert_eq!(native.data_columns.len(), 1);
        assert_eq!(native.data_columns[0].column_name, "name");
        assert!(native.constraints.is_empty(), "{:?}", native.constraints);
    }

    /// The tag check, per spoke: a GeoPackage payload must not reach another
    /// format's writer.
    #[test]
    fn a_geopackage_native_is_inert_in_another_format() {
        let source = hostile_layer("cities");
        let mut stripped = source.clone();
        stripped.native = None;
        assert_eq!(
            crate::flatgeobuf::write(&source),
            crate::flatgeobuf::write(&stripped),
            "a GeoPackage payload changed FlatGeobuf output"
        );
    }

    /// The tag check, for the newest payload: a GeoParquet native must be
    /// invisible to every other writer. Asserted as byte-identity against the
    /// same collection with no native at all.
    #[test]
    fn a_geoparquet_native_is_inert_in_another_format() {
        let source = parquet_to_features(include_bytes!(
            "../tests/fixtures/gdal_covering_bbox.parquet"
        ))
        .unwrap();
        let mut stripped = source.clone();
        stripped.native = None;
        assert_eq!(
            crate::flatgeobuf::write(&source),
            crate::flatgeobuf::write(&stripped),
            "a GeoParquet payload changed FlatGeobuf output"
        );
    }
}
