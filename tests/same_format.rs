//! Same-format runs: `geosetta a.fgb b.fgb` used to be refused outright, and
//! is now the ordinary case.
//!
//! `plans/same-format-output.org` M1 deletes the `args.from == out.format`
//! check. Geosetta stands at *both* ends of every pipeline it takes part in, so
//! a transit format colliding with the endpoint format is not a user error —
//! and per `plans/edit-composability.org` § THE TRANSIT COROLLARY it is the
//! *only* configuration in which an edit loop is lossless end to end.
//!
//! Three properties are tested here, and they pull against each other, which is
//! why they share a file:
//!
//! 1. **The run works**, and re-reads to an equal collection. Not byte
//!    equality — D2 says same format is a re-encode, never a copy.
//! 2. **The run warns** when the format still loses something, because the
//!    cross-format defence ("the target has no slot for it") evaporates when
//!    the target *is* the source's format.
//! 3. **The run refuses** to overwrite its own input, which the deleted check
//!    had been providing by accident.
//!
//! These invoke the binary rather than the library because the warning and the
//! refusal both live in `main.rs`, which is where the input and output *paths*
//! are — the library never sees a path.

use std::process::Command;

use geosetta::{Feature, FeatureCollection, Format, Geometry, JsonValue, Position};

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

/// A temp directory private to the calling test, on the same reasoning as
/// `crs_external.rs`'s: libtest runs a thread per test by default, and these
/// tests write fixtures under fixed names.
fn tmp_dir() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let label = match std::thread::current().name() {
        Some(name) if name != "main" => name.replace("::", "-"),
        _ => format!("seq{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
    };
    let dir = std::env::temp_dir()
        .join(format!("geosetta-same-format-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(GEOSETTA).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn sample() -> FeatureCollection {
    FeatureCollection::new(vec![Feature {
        geometry: Some(Geometry::Point(Position::new(12.5683, 55.6761))),
        properties: vec![
            ("name".into(), JsonValue::String("København".into())),
            ("n".into(), JsonValue::Int(7)),
        ],
        native: None,
    }])
}

/// Every single-buffer format converts to itself and comes back equal through
/// the feature model.
///
/// The assertion is on the *re-read collection*, not on bytes, and that is
/// deliberate: D2 rules out a byte-copy fast path, so the output is whatever
/// this crate's own writer produces. Byte equality would be asserting the
/// wrong thing and would fail on formats that legitimately normalise.
#[test]
fn every_format_converts_to_itself() {
    for format in [Format::GeoJson, Format::FlatGeobuf, Format::Parquet, Format::Csv] {
        let ext = format.extension();
        let dir = tmp_dir();
        let src = dir.join(format!("in.{ext}"));
        let dst = dir.join(format!("out.{ext}"));
        std::fs::write(&src, geosetta::write_features(format, &sample()).unwrap()).unwrap();

        let (ok, err) = run(&[src.to_str().unwrap(), dst.to_str().unwrap()]);
        assert!(ok, "{format:?} to itself failed: {err}");

        let back = geosetta::read_features(format, &std::fs::read(&dst).unwrap()).unwrap();
        assert_eq!(back.features.len(), 1, "{format:?}");
        assert_eq!(
            back.features[0].properties.iter().find(|(k, _)| &**k == "name").map(|(_, v)| v),
            Some(&JsonValue::String("København".into())),
            "{format:?} lost the property value"
        );
    }
}

/// The stdin/stdout pass-through stage, which is the shape an edit loop
/// actually uses — `geosetta - - --from fgb --to fgb` in the middle of a pipe.
///
/// It was refused before M1, which is why no format had a lossless edit loop:
/// the transit corollary needs transit and endpoint to be the same format, and
/// that was precisely the run the check rejected.
#[test]
fn the_pipe_pass_through_stage_runs() {
    let dir = tmp_dir();
    let src = dir.join("in.fgb");
    std::fs::write(&src, geosetta::write_features(Format::FlatGeobuf, &sample()).unwrap())
        .unwrap();
    let dst = dir.join("out.fgb");

    let piped = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{GEOSETTA} {} - --to fgb | {GEOSETTA} - {} --from fgb --to fgb",
            src.display(),
            dst.display()
        ))
        .output()
        .unwrap();
    assert!(piped.status.success(), "{}", String::from_utf8_lossy(&piped.stderr));

    let back = geosetta::read_features(Format::FlatGeobuf, &std::fs::read(&dst).unwrap()).unwrap();
    assert_eq!(back.features.len(), 1);
}

/// The warning fires where the format still loses something, and stays silent
/// where it does not.
///
/// Both halves matter. A warning that fires on every run teaches users to
/// ignore it — that is `plans/same-format-fidelity.org` § THE RULE's noise bar
/// — so the silent case is as much the specification as the loud one.
///
/// **The silent case is Shapefile, and the plan said it would be WKT.**
/// `same-format-output.org` § THE FIDELITY PROBLEM offered `geosetta a.wkt
/// b.wkt` as the example that warns about nothing, reasoning that the WKT
/// reader recovers geometry only and so discards no *members*. Measured, WKT
/// loses the *lexeme*: `POINT (1.50 2.00)` comes back `POINT (1.5 2)` and
/// `MULTIPOINT ((1 2), (3 4))` comes back in the bare dialect. Shapefile is the
/// format that actually round-trips clean.
#[test]
fn the_same_format_warning_fires_only_where_something_is_lost() {
    let dir = tmp_dir();

    let src = dir.join("in.geojson");
    std::fs::write(&src, geosetta::write_features(Format::GeoJson, &sample()).unwrap()).unwrap();
    let (ok, err) = run(&[src.to_str().unwrap(), dir.join("out.geojson").to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(err.contains("does not preserve"), "GeoJSON to itself should warn: {err}");
    assert!(err.contains("bbox"), "the GeoJSON warning should name bbox: {err}");

    // Shapefile: nothing material is lost, so nothing is said.
    let shp = dir.join("in.shp");
    std::fs::write(&shp, std::fs::read("tests/fixtures/gdal_hostile.shp").unwrap()).unwrap();
    for ext in ["shx", "dbf", "prj", "cpg"] {
        let from = format!("tests/fixtures/gdal_hostile.{ext}");
        if let Ok(bytes) = std::fs::read(&from) {
            std::fs::write(dir.join(format!("in.{ext}")), bytes).unwrap();
        }
    }
    let (ok, err) = run(&[shp.to_str().unwrap(), dir.join("out.shp").to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(!err.contains("does not preserve"), "Shapefile to itself should be silent: {err}");
}

/// A cross-format run never gets this warning, because there the loss has a
/// defence and a different mechanism owns it.
#[test]
fn a_cross_format_run_does_not_get_the_same_format_warning() {
    let dir = tmp_dir();
    let src = dir.join("in.geojson");
    std::fs::write(&src, geosetta::write_features(Format::GeoJson, &sample()).unwrap()).unwrap();
    let (ok, err) = run(&[src.to_str().unwrap(), dir.join("out.fgb").to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(!err.contains("does not preserve"), "cross-format must not warn here: {err}");
}

/// Every format's loss list is a claim about a measurement, so the ones
/// asserted here are the ones that were actually run.
///
/// This is a unit assertion rather than an end-to-end one because it pins the
/// *table*: an entry silently emptied would make the warning above stop firing
/// with no test failing anywhere else.
#[test]
fn the_loss_table_says_what_was_measured() {
    assert!(Format::Shapefile.same_format_losses().is_empty());
    assert!(Format::Gpkg.same_format_losses().is_empty(), "gpkg is container_drop_warnings'");
    for format in
        [Format::GeoJson, Format::Csv, Format::Wkt, Format::FlatGeobuf, Format::Parquet, Format::Kml]
    {
        assert!(!format.same_format_losses().is_empty(), "{format:?} was measured lossy");
    }
}

/// An output that would overwrite its own input is refused, and the input
/// survives.
///
/// `plans/same-format-output.org` D4. The happy path would survive an in-place
/// run — every reader completes before any writer opens a file — but
/// `std::fs::write` truncates first, so a write-side failure after that point
/// has destroyed the only copy.
#[test]
fn a_run_that_would_overwrite_its_own_input_is_refused() {
    let dir = tmp_dir();
    let src = dir.join("in.fgb");
    let bytes = geosetta::write_features(Format::FlatGeobuf, &sample()).unwrap();
    std::fs::write(&src, &bytes).unwrap();

    // The second spelling is the point: a redundant `.` component names the
    // same file, so the guard has to canonicalise rather than compare strings.
    for target in
        [src.to_str().unwrap().to_string(), format!("{}/./in.fgb", dir.display())]
    {
        let (ok, err) = run(&[src.to_str().unwrap(), &target]);
        assert!(!ok, "should have refused {target}");
        assert!(err.contains("same file"), "{err}");
        assert_eq!(std::fs::read(&src).unwrap(), bytes, "input was modified by {target}");
    }
}

/// The Shapefile case, which is a *file set* rather than a path.
///
/// `geosetta roads.shp roads --to shp` names the same `roads.{shp,shx,dbf,prj}`
/// set from both ends without repeating a single path, so a comparison of
/// output path against input path would miss it entirely.
#[test]
fn the_shapefile_refusal_compares_the_whole_file_set() {
    let dir = tmp_dir();
    for ext in ["shp", "shx", "dbf", "prj", "cpg"] {
        let from = format!("tests/fixtures/gdal_hostile.{ext}");
        if let Ok(bytes) = std::fs::read(&from) {
            std::fs::write(dir.join(format!("roads.{ext}")), bytes).unwrap();
        }
    }
    let shp = dir.join("roads.shp");
    let before = std::fs::read(&shp).unwrap();
    let dbf_before = std::fs::read(dir.join("roads.dbf")).unwrap();

    // The output names no path the input names, and is still the same file set.
    let (ok, err) = run(&[shp.to_str().unwrap(), dir.join("roads").to_str().unwrap(), "--to", "shp"]);
    assert!(!ok, "bare stem naming its own set should be refused");
    assert!(err.contains("same file"), "{err}");
    assert_eq!(std::fs::read(&shp).unwrap(), before);
    assert_eq!(std::fs::read(dir.join("roads.dbf")).unwrap(), dbf_before);
}

/// A GeoPackage in-place run is refused too, which is a **behaviour change**.
///
/// It used to be permitted, and worked, but only because the append path reads
/// the output before overwriting it — luck rather than design, and inconsistent
/// with `ogr2ogr`, which refuses the same run.
#[test]
fn the_geopackage_in_place_run_is_now_refused() {
    let dir = tmp_dir();
    let gpkg = dir.join("in.gpkg");
    std::fs::write(&gpkg, std::fs::read("tests/fixtures/gdal_hostile.gpkg").unwrap()).unwrap();
    let before = std::fs::read(&gpkg).unwrap();

    let (ok, err) = run(&[gpkg.to_str().unwrap(), gpkg.to_str().unwrap()]);
    assert!(!ok, "in-place gpkg should now be refused");
    assert!(err.contains("same file"), "{err}");
    assert_eq!(std::fs::read(&gpkg).unwrap(), before, "the container was modified");
}

/// `-` is never the same file as anything, including itself.
///
/// stdin and stdout are different streams even when both are the same
/// terminal, so the guard must not reach for a path that isn't there.
#[test]
fn stdin_and_stdout_are_never_the_same_file() {
    let dir = tmp_dir();
    let src = dir.join("in.fgb");
    std::fs::write(&src, geosetta::write_features(Format::FlatGeobuf, &sample()).unwrap())
        .unwrap();
    let piped = Command::new("sh")
        .arg("-c")
        .arg(format!("{GEOSETTA} {} - --to fgb | {GEOSETTA} - - --from fgb --to fgb | wc -c", src.display()))
        .output()
        .unwrap();
    assert!(piped.status.success(), "{}", String::from_utf8_lossy(&piped.stderr));
    let bytes: usize = String::from_utf8_lossy(&piped.stdout).trim().parse().unwrap();
    assert!(bytes > 0, "the pass-through produced nothing");
}
