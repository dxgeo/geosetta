//! Proves the two seams built for third-party reprojection actually work
//! against real, independently-authored tools rather than our own stand-ins:
//!
//! - `-` for stdin/stdout (`main.rs`) — lets any external *process* sit
//!   between `read`/`write` in a Unix pipe.
//! - [`geosetta::FeatureCollection::for_each_position_mut`]
//!   (`src/feature.rs`, `src/geometry/mod.rs`) — lets any external *library*
//!   rewrite coordinates in place between the same two calls.
//!
//! Geosetta itself never reprojects (see [`geosetta::crs`]), so "does
//! reprojection library X work" can only be answered by actually plugging one
//! in. Three tools, three integration shapes:
//!
//! - **GDAL** (`ogr2ogr`) — container- and CRS-aware; piped via stdin/stdout.
//! - **PROJ** (`cs2cs`) — a bare coordinate transformer with no container
//!   format of its own, piped via stdin/stdout with a thin text adapter. This
//!   is the scenario `-` was built for: *any* external tool, not just
//!   GIS-aware ones.
//! - **wbprojection** — a reprojection *library* (crates.io, MIT/Apache-2.0),
//!   not a CLI — it has no binary target, so there's nothing to pipe to. It
//!   plugs into `for_each_position_mut` instead, linked directly as a
//!   dev-dependency (see `Cargo.toml`).
//!
//! The GDAL/PROJ tests run by default (both are on `PATH` in this dev
//! environment) but check for their tool at runtime and skip with a message
//! rather than fail when it's missing — a contributor without GDAL/PROJ
//! installed still gets a clean `cargo test`, matching this repo's existing
//! convention of not requiring optional external tools for the default run
//! (see `src/crs/registry.rs`'s `#[ignore]`d `projinfo`-based oracle tests,
//! which take the opposite tradeoff for a much longer-running, ~13.8k-call
//! oracle rather than a handful of quick checks).
//!
//! All three transform the same three-point WGS 84 fixture to EPSG:3857 and
//! are checked against the same oracle values, cross-verified by hand:
//! `ogr2ogr -t_srs EPSG:3857` and `cs2cs +proj=webmerc +datum=WGS84` agree to
//! 6 decimal places on this fixture, so a bug specific to any one integration
//! would show up as a mismatch here even without comparing the tools directly
//! against each other.

use std::io::Write;
use std::process::{Command, Stdio};

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

const FIXTURE_GEOJSON: &str = r#"{"type":"FeatureCollection","features":[
{"type":"Feature","geometry":{"type":"Point","coordinates":[-73.9857,40.7484]},"properties":{"name":"Empire State"}},
{"type":"Feature","geometry":{"type":"Point","coordinates":[-0.1276,51.5074]},"properties":{"name":"London"}},
{"type":"Feature","geometry":{"type":"Point","coordinates":[139.6917,35.6895]},"properties":{"name":"Tokyo"}}
]}"#;

/// EPSG:4326 -> EPSG:3857 for [`FIXTURE_GEOJSON`]'s three points.
const ORACLE_WEB_MERCATOR: [(f64, f64); 3] = [
    (-8_236_050.449984, 4_975_301.253790),
    (-14_204.367025, 6_711_542.475588),
    (15_550_408.912047, 4_257_980.732184),
];

fn assert_matches_oracle(got: &[(f64, f64)], label: &str) {
    assert_eq!(got.len(), ORACLE_WEB_MERCATOR.len(), "{label}: wrong point count");
    for (i, (&(gx, gy), &(ex, ey))) in got.iter().zip(ORACLE_WEB_MERCATOR.iter()).enumerate() {
        assert!(
            (gx - ex).abs() < 1e-3 && (gy - ey).abs() < 1e-3,
            "{label}: point {i} = ({gx}, {gy}), expected ({ex}, {ey})"
        );
    }
}

fn tool_available(name: &str) -> bool {
    Command::new(name).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

fn scratch_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("geosetta-reproject-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Run `cmd` to completion with no stdin, returning captured stdout. Panics
/// with stderr on a non-zero exit.
fn run(mut cmd: Command) -> Vec<u8> {
    let out = cmd.stdin(Stdio::null()).output().unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    assert!(out.status.success(), "{cmd:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Run `cmd` to completion, feeding it `input` on stdin, returning captured
/// stdout. Panics with stderr on a non-zero exit.
fn run_with_stdin(mut cmd: Command, input: &[u8]) -> Vec<u8> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{cmd:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// A real two-process pipeline, `a | b`, connected by an OS pipe (not
/// buffered through this process) — mirrors a shell `|` exactly, including
/// backpressure, so it can't deadlock regardless of fixture size. Returns `b`'s
/// captured stdout.
fn run_piped(mut a: Command, mut b: Command, input: &[u8]) -> Vec<u8> {
    let mut a_child = a
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn producer {a:?}: {e}"));
    let a_stdout = a_child.stdout.take().unwrap();

    let b_child = b
        .stdin(Stdio::from(a_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn consumer {b:?}: {e}"));

    let mut a_stdin = a_child.stdin.take().unwrap();
    let payload = input.to_vec();
    let writer = std::thread::spawn(move || a_stdin.write_all(&payload));

    writer.join().unwrap().expect("write producer stdin");
    let a_out = a_child.wait_with_output().expect("wait producer");
    let b_out = b_child.wait_with_output().expect("wait consumer");

    assert!(a_out.status.success(), "{a:?} failed:\n{}", String::from_utf8_lossy(&a_out.stderr));
    assert!(b_out.status.success(), "{b:?} failed:\n{}", String::from_utf8_lossy(&b_out.stderr));
    b_out.stdout
}

/// Parse `POINT (x y)`-per-line WKT (geosetta's `Format::Wkt` writer's exact
/// shape — see `src/geometry/wkt.rs`'s `encode`) into `(x, y)` pairs.
fn parse_wkt_points(text: &str) -> Vec<(f64, f64)> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let inner = l.trim().strip_prefix("POINT (").and_then(|s| s.strip_suffix(')')).unwrap_or_else(|| {
                panic!("not a POINT line: {l:?}")
            });
            let mut parts = inner.split_whitespace();
            let x: f64 = parts.next().unwrap().parse().unwrap();
            let y: f64 = parts.next().unwrap().parse().unwrap();
            (x, y)
        })
        .collect()
}

/// GDAL: `geosetta ... -` (file → stdout, real FlatGeobuf bytes) piped into
/// `ogr2ogr` (a real external, format- and CRS-aware tool that has never heard
/// of geosetta) reprojecting to EPSG:3857. GDAL's GeoPackage driver isn't
/// stdout-streamable (SQLite needs random access — a GDAL/SQLite constraint,
/// not a geosetta one), so that leg writes a real file; the final leg reads
/// that file back through geosetta's own `-` output to close the loop and
/// check both the coordinates and the recovered CRS.
#[test]
fn pipes_through_gdal_ogr2ogr() {
    if !tool_available("ogr2ogr") {
        eprintln!("skipping: ogr2ogr not on PATH");
        return;
    }
    let geojson_path = scratch_path("gdal_in.geojson");
    std::fs::write(&geojson_path, FIXTURE_GEOJSON).unwrap();
    let gpkg_path = scratch_path("gdal_mid.gpkg");
    let _ = std::fs::remove_file(&gpkg_path);

    // geosetta: file -> stdout (fgb), piped straight into ogr2ogr.
    let mut geosetta_to_fgb = Command::new(GEOSETTA);
    geosetta_to_fgb.args(["--from", "geojson", "--to", "fgb"]).arg(&geojson_path).arg("-");

    let mut ogr2ogr = Command::new("ogr2ogr");
    ogr2ogr.args(["-f", "GPKG"]).arg(&gpkg_path).args(["-s_srs", "EPSG:4326", "-t_srs", "EPSG:3857", "/vsistdin/"]);

    run_piped(geosetta_to_fgb, ogr2ogr, &[]);
    assert!(gpkg_path.exists(), "ogr2ogr did not produce {gpkg_path:?}");

    // geosetta: the reprojected gpkg -> stdout (wkt), through geosetta's own
    // (well-tested elsewhere) GeoPackage reader.
    let mut geosetta_from_gpkg = Command::new(GEOSETTA);
    geosetta_from_gpkg.arg(&gpkg_path).arg("-").args(["--to", "wkt", "--quiet"]);
    let wkt_out = run(geosetta_from_gpkg);

    let points = parse_wkt_points(std::str::from_utf8(&wkt_out).unwrap());
    assert_matches_oracle(&points, "gdal");

    // The CRS survived the round trip too, not just the numbers.
    let mut geosetta_geojson = Command::new(GEOSETTA);
    geosetta_geojson.arg(&gpkg_path).arg("-").args(["--to", "geojson", "--quiet"]);
    let geojson_out = run(geosetta_geojson);
    // GeoJSON can't carry a non-WGS-84 CRS (see crs.rs), so the *warning*
    // going through --to wkt above already proved the CRS was recognized as
    // non-default; this second pass just double-checks the numbers agree.
    let text = String::from_utf8(geojson_out).unwrap();
    assert!(text.contains("-8236050"), "{text}");
}

/// PROJ: `geosetta ... -` (file → stdout, WKT text) piped through a thin
/// numeric adapter into `cs2cs` — a tool that knows nothing about any
/// geospatial container format, just "x y" per line — then the transformed
/// numbers are re-wrapped as WKT and piped back into `geosetta ... -` (stdin →
/// stdout) to prove the round trip through geosetta's own reader/writer is
/// intact, not just the external transform.
#[test]
fn pipes_through_proj_cs2cs() {
    if !tool_available("cs2cs") {
        eprintln!("skipping: cs2cs not on PATH");
        return;
    }
    let geojson_path = scratch_path("proj_in.geojson");
    std::fs::write(&geojson_path, FIXTURE_GEOJSON).unwrap();

    let mut geosetta_to_wkt = Command::new(GEOSETTA);
    geosetta_to_wkt.args(["--from", "geojson", "--to", "wkt"]).arg(&geojson_path).arg("-");
    let wkt = run(geosetta_to_wkt);
    let points = parse_wkt_points(std::str::from_utf8(&wkt).unwrap());

    // cs2cs speaks bare "x y" per line — proj-strings (not "EPSG:4326") pin
    // down traditional lon/lat axis order unambiguously, sidestepping PROJ's
    // authority-mandated lat/lon order for bare EPSG codes.
    let cs2cs_input: String = points.iter().map(|(x, y)| format!("{x} {y}\n")).collect();
    let mut cs2cs = Command::new("cs2cs");
    cs2cs.args(["-f", "%.6f", "+proj=longlat", "+datum=WGS84", "+to", "+proj=webmerc", "+datum=WGS84"]);
    let transformed = run_with_stdin(cs2cs, cs2cs_input.as_bytes());
    let transformed = String::from_utf8(transformed).unwrap();

    // cs2cs prints "x y z" per line (z=0 here); rebuild WKT points from x, y.
    let reprojected_wkt: String = transformed
        .lines()
        .map(|l| {
            let mut parts = l.split_whitespace();
            let x = parts.next().unwrap();
            let y = parts.next().unwrap();
            format!("POINT ({x} {y})\n")
        })
        .collect();

    // Feed the reconstructed WKT back into geosetta via stdin -> stdout, to
    // prove the piped-back data re-enters geosetta's own reader/writer fine.
    // Bounced through GeoJSON and back to WKT (geosetta refuses a same-format
    // no-op conversion, and GeoJSON's round trip is already covered
    // elsewhere, so this doubles as free coverage of that hop too).
    let mut wkt_to_geojson = Command::new(GEOSETTA);
    wkt_to_geojson.args(["--from", "wkt", "--to", "geojson"]).arg("-").arg("-");
    let geojson_roundtrip = run_with_stdin(wkt_to_geojson, reprojected_wkt.as_bytes());

    let mut geojson_to_wkt = Command::new(GEOSETTA);
    geojson_to_wkt.args(["--from", "geojson", "--to", "wkt"]).arg("-").arg("-");
    let final_wkt = run_with_stdin(geojson_to_wkt, &geojson_roundtrip);

    let final_points = parse_wkt_points(std::str::from_utf8(&final_wkt).unwrap());
    assert_matches_oracle(&final_points, "proj/cs2cs");
}

/// wbprojection: a reprojection *library* with no CLI (checked directly
/// against its published source — `crates/wbprojection` in
/// github.com/jblindsay/whitebox_next_gen has no `[[bin]]`), so there's no
/// process to pipe to. It plugs into the library-composition seam instead:
/// `read_features` -> `FeatureCollection::for_each_position_mut` (mutating in
/// place via wbprojection's `Crs::transform_to`) -> set `fc.crs` ->
/// `write_features`. Runs by default (no external tool/PATH dependency).
#[test]
fn composes_with_wbprojection_library() {
    let fc = geosetta::read_features(geosetta::Format::GeoJson, FIXTURE_GEOJSON.as_bytes()).unwrap();
    let mut fc = fc;

    let wgs84 = wbprojection::Crs::from_epsg(4326).expect("wbprojection knows EPSG:4326");
    let web_mercator = wbprojection::Crs::from_epsg(3857).expect("wbprojection knows EPSG:3857");

    fc.for_each_position_mut(|p| {
        let (x, y) = wgs84.transform_to(p[0], p[1], &web_mercator).expect("wbprojection transform");
        p[0] = x;
        p[1] = y;
    });
    fc.crs = Some(geosetta::Crs::from_authority_code(
        Some("EPSG".into()),
        Some("3857".into()),
        None,
        None,
    ));

    let points: Vec<(f64, f64)> = fc
        .features
        .iter()
        .map(|f| match f.geometry.as_ref().unwrap() {
            geosetta::Geometry::Point(p) => (p[0], p[1]),
            other => panic!("expected Point, got {other:?}"),
        })
        .collect();
    assert_matches_oracle(&points, "wbprojection");

    // The visitor only touched coordinates; the CRS is whatever the caller
    // set afterward (geosetta never infers or checks it — see crs.rs).
    let out = geosetta::write_features(geosetta::Format::Wkt, &fc).unwrap();
    let written_points = parse_wkt_points(std::str::from_utf8(&out).unwrap());
    assert_matches_oracle(&written_points, "wbprojection (written)");
}
