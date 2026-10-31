//! **Does a library caller hear about a loss the CLI would print?**
//!
//! `plans/library-warnings.org` L1: the pre-write checks used to be listed in
//! `main.rs`, so a caller of the crate got none of them unless it copied the
//! list, and nothing kept the copy current. Now there is one list,
//! `geosetta::conversion_warnings`, and the binary calls it too.
//!
//! So the property is *agreement*: for the same input, `convert_with` returns
//! exactly the message bodies the binary prints after `warning: `, in the same
//! order. An integration test on purpose, as in `public_api.rs` — it sees the
//! API a dependent sees.
//!
//! It also holds the GeoPackage write path to the same list, which is
//! `plans/cross-format-audit.org` F1: that path asked for every check but the
//! KML spoke's, so a KML source written to `.gpkg` lost a `<Model>` silently.

use std::process::Command;

use geosetta::{convert_with, Feature, FeatureCollection, Format, Geometry, JsonValue, Position, WriteOptions};

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

/// A temp directory private to the calling test, as in `same_format.rs`.
fn tmp_dir() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let label = match std::thread::current().name() {
        Some(name) if name != "main" => name.replace("::", "-"),
        _ => format!("seq{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
    };
    let dir = std::env::temp_dir()
        .join(format!("geosetta-library-warnings-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The message bodies the binary prints converting `input` to `output`.
fn cli_warnings(input: &std::path::Path, output: &std::path::Path) -> Vec<String> {
    let out = Command::new(GEOSETTA).arg(input).arg(output).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    stderr.lines().filter_map(|l| l.strip_prefix("warning: ")).map(String::from).collect()
}

fn sample() -> FeatureCollection {
    FeatureCollection::new(vec![Feature {
        geometry: Some(Geometry::Point(Position::new(12.5683, 55.6761))),
        properties: vec![("name".into(), JsonValue::String("København".into()))],
        native: None,
    }])
}

/// The library and the binary say the same thing, for a read-side loss (the
/// KMZ archive's other entries), the KML write checks, and the same-format
/// warning — the three that used to be separate calls in `main.rs`.
#[test]
fn convert_with_returns_what_the_cli_prints() {
    let dir = tmp_dir();
    let geojson = dir.join("in.geojson");
    std::fs::write(&geojson, geosetta::write_features(Format::GeoJson, &sample()).unwrap()).unwrap();
    let cases = [
        ("tests/fixtures/hand_hostile.kml".into(), Format::Kml, Format::FlatGeobuf, "fgb"),
        ("tests/fixtures/hand_hostile.kmz".into(), Format::Kmz, Format::FlatGeobuf, "fgb"),
        ("tests/fixtures/hand_hostile_2_3.kml".into(), Format::Kml, Format::Csv, "csv"),
        (geojson, Format::GeoJson, Format::GeoJson, "geojson"),
    ];
    for (input, from, to, ext) in cases {
        let input: std::path::PathBuf = input;
        let lib = convert_with(from, to, &std::fs::read(&input).unwrap(), WriteOptions::default()).unwrap();
        let cli = cli_warnings(&input, &dir.join(format!("out.{ext}")));
        assert!(!lib.warnings.is_empty(), "{input:?} -> {ext} should warn");
        assert_eq!(lib.warnings, cli, "{input:?} -> {ext}");
    }
}

/// And the silent case is silent through both, so agreement is not just two
/// copies of the same noise.
#[test]
fn a_conversion_that_loses_nothing_returns_no_warnings() {
    let dir = tmp_dir();
    let geojson = dir.join("in.geojson");
    let bytes = geosetta::write_features(Format::GeoJson, &sample()).unwrap();
    std::fs::write(&geojson, &bytes).unwrap();
    let lib = convert_with(Format::GeoJson, Format::FlatGeobuf, &bytes, WriteOptions::default()).unwrap();
    assert_eq!(lib.warnings, Vec::<String>::new());
    assert_eq!(cli_warnings(&geojson, &dir.join("out.fgb")), Vec::<String>::new());
}

/// `plans/cross-format-audit.org` F1 / A1: a KML or KMZ source written to
/// GeoPackage prints the KML spoke's warnings, as a FlatGeobuf write does —
/// the same messages, naming the target they were written for.
#[test]
fn a_geopackage_write_prints_the_kml_warnings() {
    let dir = tmp_dir();
    for fixture in ["hand_hostile.kml", "hand_hostile.kmz", "hand_hostile_2_3.kml"] {
        let input = std::path::Path::new("tests/fixtures").join(fixture);
        let fgb = cli_warnings(&input, &dir.join("out.fgb"));
        let gpkg_path = dir.join("out.gpkg");
        let _ = std::fs::remove_file(&gpkg_path);
        let gpkg = cli_warnings(&input, &gpkg_path);
        assert!(!fgb.is_empty(), "{fixture} should warn writing fgb");
        let expected: Vec<String> = fgb.iter().map(|w| w.replace("FlatGeobuf", "GeoPackage")).collect();
        assert_eq!(gpkg, expected, "{fixture}");
    }
}
