//! End-to-end coverage for the flags that let a CRS geosetta cannot resolve be
//! resolved by something else: `--print-crs-code` (report the source's authority
//! code), `--print-crs` (report the definition text it recorded), and `--crs`
//! (accept the definition that came back).
//!
//! The pair exists because geosetta *labels* CRS and never resolves or
//! reprojects: it carries whatever identity a format recorded, and when a source
//! offers only an authority code, targets that need a full definition —
//! GeoParquet's PROJJSON, Shapefile's `.prj` — can't be written faithfully. The
//! way out is composition, not a built-in registry: the user runs a resolver
//! themselves and hands geosetta the text.
//!
//! **Geosetta spawns nothing.** That is the point of the design (an earlier one
//! that had it shell out to a resolver was rejected on security grounds — see
//! `plans/crs-external-resolution.org`), and it is what makes these tests
//! simple: there is no subprocess to stub, because there is none to begin with.
//! Nothing here requires a particular tool to exist, and nothing in the flags'
//! implementation knows the name of one. The single test that does pipe from a
//! real resolver uses PROJ's `projinfo` — chosen only because this repo already
//! leans on it for the CRS oracles — and skips when it isn't installed.

use std::io::Write;
use std::process::{Command, Stdio};

use geosetta::crs::{Crs, NamedCrs};
use geosetta::{Feature, FeatureCollection, Format, Geometry, Position};

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

/// GDA2020 (EPSG:7844) as real WKT1 — the definition a resolver would hand back
/// for the bare code the fixtures below carry. Verbatim `projinfo -o WKT1_GDAL
/// -q EPSG:7844` output, so the piped test and the file-based tests are feeding
/// geosetta the same bytes by two different routes.
const GDA2020_WKT1: &str = r#"GEOGCS["GDA2020",DATUM["Geocentric_Datum_of_Australia_2020",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","1168"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Latitude",NORTH],AXIS["Longitude",EAST],AUTHORITY["EPSG","7844"]]"#;

const GDA2020_PROJJSON: &str = r#"{"type":"GeographicCRS","name":"GDA2020","datum":{"type":"GeodeticReferenceFrame","name":"Geocentric Datum of Australia 2020","ellipsoid":{"name":"GRS 1980","semi_major_axis":6378137,"inverse_flattening":298.257222101}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"},{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"}]},"id":{"authority":"EPSG","code":7844}}"#;

/// A temp directory private to the *calling test*.
///
/// This used to key on `std::process::id()` alone, giving one directory per
/// test binary rather than per test. Every test here writes its fixtures by a
/// fixed name — 13 of them call [`write_code_only_fgb`], which always writes
/// `code_only.fgb` — so under libtest's default thread-per-test parallelism
/// they raced on the same paths, one test truncating a file another was about
/// to read. It failed roughly one run in twelve, which is exactly often enough
/// to be dismissed as noise.
///
/// libtest names each worker thread after the test it runs, so that name both
/// isolates the directories and keeps them recognizable when a failing test
/// leaves something worth inspecting. The counter is the `--test-threads=1`
/// fallback, where every test shares the thread named `main` (harmless there —
/// nothing runs concurrently — but distinct directories still keep one test's
/// leftovers from being mistaken for another's).
fn tmp_dir() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let label = match std::thread::current().name() {
        Some(name) if name != "main" => name.replace("::", "-"),
        _ => format!(
            "seq{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ),
    };
    let dir = std::env::temp_dir().join(format!(
        "geosetta-crs-external-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn point_fc(crs: Option<Crs>) -> FeatureCollection {
    let mut fc = FeatureCollection::new(vec![Feature {
        geometry: Some(Geometry::Point(Position::new(149.13, -35.28))),
        properties: vec![],
    }]);
    fc.crs = crs;
    fc
}

/// A CRS carrying an authority code and nothing else — the exact gap these flags
/// exist for. FlatGeobuf records an authority + code natively, so writing one is
/// how a code-only source gets built without hand-rolling bytes.
fn code_only_crs() -> Crs {
    Crs::Named(NamedCrs {
        authority: Some("EPSG".into()),
        code: Some("7844".into()),
        wkt: None,
        projjson: None,
    })
}

fn write_code_only_fgb(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("code_only.fgb");
    let bytes =
        geosetta::write_features(Format::FlatGeobuf, &point_fc(Some(code_only_crs()))).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

struct Run {
    stdout: String,
    stderr: String,
    ok: bool,
}

impl Run {
    /// Every warning geosetta emits goes through `main.rs`'s `print_warnings`,
    /// which prefixes `warning: ` and prints one per line — so a warning is a
    /// *line starting with* that prefix, not merely the word appearing
    /// somewhere in stderr.
    ///
    /// The distinction is not pedantic: stderr also carries the informational
    /// `wrote <path> (N bytes)` line, so a plain `stderr.contains("warning")`
    /// also fires on any output path with "warning" in it — which is exactly
    /// what happened once [`tmp_dir`] started naming directories after the
    /// calling test, one of which is
    /// `crs_override_silences_the_unresolvable_geoparquet_warning`.
    fn warnings(&self) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|l| l.starts_with("warning: "))
            .collect()
    }
}

fn run(args: &[&str], stdin: Option<&str>) -> Run {
    let mut cmd = Command::new(GEOSETTA);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn geosetta");
    if let Some(text) = stdin {
        child.stdin.as_mut().unwrap().write_all(text.as_bytes()).unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("run geosetta");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        ok: out.status.success(),
    }
}

// ===========================================================================
// --print-crs-code
// ===========================================================================

#[test]
fn prints_the_authority_code_a_source_declares() {
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let r = run(&[fgb.to_str().unwrap(), "--print-crs-code"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "EPSG:7844\n");
}

#[test]
fn prints_unconditionally_even_when_nothing_needs_resolving() {
    // Deliberately not situational: a CRS that already converts faithfully
    // still reports its code, because deciding for the user whether they need a
    // resolver would make the flag unpredictable to script against.
    let dir = tmp_dir();
    let path = dir.join("wgs84.geojson");
    std::fs::write(&path, r#"{"type":"FeatureCollection","features":[]}"#).unwrap();
    let r = run(&[path.to_str().unwrap(), "--print-crs-code"], None);
    assert!(r.ok, "{}", r.stderr);
    // GeoJSON encodes no CRS at all — RFC 7946 *fixes* it at WGS 84 — so the
    // identity is real and known, merely not written down. That is a different
    // thing from having no CRS (below), and must not be reported as absent.
    assert_eq!(r.stdout, "OGC:CRS84\n");
}

#[test]
fn reports_nothing_when_the_source_has_no_crs_at_all() {
    // CSV and WKT have no CRS channel, so nothing was ever recorded. Empty
    // stdout plus a nonzero exit — the contract a shell substitution needs, so
    // it fails loudly rather than feeding an empty string to a resolver.
    let dir = tmp_dir();
    let path = dir.join("plain.wkt");
    std::fs::write(&path, "POINT (149.13 -35.28)\n").unwrap();
    let r = run(&[path.to_str().unwrap(), "--print-crs-code"], None);
    assert!(!r.ok, "should exit nonzero with nothing to report");
    assert_eq!(r.stdout, "");
    assert!(r.stderr.contains("no CRS authority code"), "{}", r.stderr);
}

#[test]
fn reports_nothing_for_an_id_less_definition() {
    // A real Esri-flavor .prj: a full WKT definition with no AUTHORITY node
    // anywhere, so there is no code to report even though there *is* a CRS.
    // Recovering an identity from such a definition's name is a different
    // mechanism entirely, and not one geosetta has.
    let r = run(&["tests/fixtures/duckdb_crs_pt.shp", "--print-crs-code"], None);
    assert!(!r.ok, "stdout was: {:?}", r.stdout);
    assert_eq!(r.stdout, "");
}

#[test]
fn prints_one_line_per_distinct_layer_crs() {
    // A GeoPackage is multi-layer and its layers need not agree. One code per
    // line, de-duplicated in layer order: the ordinary single-CRS file still
    // yields exactly one line for `$(...)`, and a mixed one fails loudly
    // downstream instead of silently reporting whichever layer came first.
    let dir = tmp_dir();
    let path = dir.join("mixed.gpkg");
    let epsg_3857 = Crs::Named(NamedCrs {
        authority: Some("EPSG".into()),
        code: Some("3857".into()),
        ..Default::default()
    });
    let layers = vec![
        ("a".to_string(), point_fc(Some(code_only_crs()))),
        ("b".to_string(), point_fc(Some(epsg_3857))),
        ("c".to_string(), point_fc(Some(code_only_crs()))),
    ];
    std::fs::write(&path, geosetta::geopackage::write_layers(None, &layers, false, false).unwrap())
        .unwrap();

    let r = run(&[path.to_str().unwrap(), "--print-crs-code"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "EPSG:7844\nEPSG:3857\n", "deduplicated, in layer order");

    // --layer narrows it to one, which is the escape hatch for a mixed file.
    let r = run(&[path.to_str().unwrap(), "--print-crs-code", "--layer", "b"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "EPSG:3857\n");
}

// ===========================================================================
// --print-crs
// ===========================================================================

/// A CRS carrying a *definition* and no authority code — the case
/// `--print-crs-code` structurally cannot report and this flag exists for. The
/// body is pretty-printed on purpose: verbatim has to mean the source's own
/// formatting, not a shape that happens to survive a re-serialization.
const ID_LESS_PROJJSON: &str = r#"{
  "type": "GeographicCRS",
  "name": "Some Unregistered Datum",
  "datum": {
    "type": "GeodeticReferenceFrame",
    "name": "Some Unregistered Datum",
    "ellipsoid": { "name": "GRS 1980", "semi_major_axis": 6378137, "inverse_flattening": 298.257222101 }
  }
}"#;

fn projjson_only_crs() -> Crs {
    Crs::Named(NamedCrs {
        authority: None,
        code: None,
        wkt: None,
        projjson: Some(ID_LESS_PROJJSON.to_string()),
    })
}

/// Two id-less WKT definitions, for the GeoPackage cases. GeoPackage records a
/// CRS as WKT in its `gpkg_spatial_ref_sys` table, so a PROJJSON fixture would
/// be translated on the way in and the test would be asserting on the
/// crosswalk's output rather than on what `--print-crs` does with it.
const ID_LESS_WKT_A: &str = r#"GEOGCS["Datum A",DATUM["Datum A",SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
const ID_LESS_WKT_B: &str = r#"GEOGCS["Datum B",DATUM["Datum B",SPHEROID["Clarke 1866",6378206.4,294.9786982]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;

fn wkt_only_crs(wkt: &str) -> Crs {
    Crs::Named(NamedCrs {
        authority: None,
        code: None,
        wkt: Some(wkt.to_string()),
        projjson: None,
    })
}

/// Write a GeoParquet whose `geo` metadata carries `ID_LESS_PROJJSON` — the
/// input the whole cross-repo pipeline is built around. No such file has been
/// confirmed to exist in the wild (PROJ and GDAL always emit a root `id`), so it
/// is constructed here; see `plans/crs-definition-output.org` § OPEN QUESTIONS.
fn write_id_less_parquet(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("id_less.parquet");
    let bytes =
        geosetta::write_features(Format::Parquet, &point_fc(Some(projjson_only_crs()))).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn prints_the_definition_body_a_source_recorded() {
    let dir = tmp_dir();
    let path = write_id_less_parquet(&dir);

    let r = run(&[path.to_str().unwrap(), "--print-crs"], None);
    assert!(r.ok, "{}", r.stderr);
    // Byte-for-byte, plus exactly one trailing newline: no re-indenting, no
    // re-ordering, no compaction.
    assert_eq!(r.stdout, format!("{ID_LESS_PROJJSON}\n"));
}

#[test]
fn the_definition_survives_a_round_trip_through_the_crs_override() {
    // The verbatim contract's real test: what --print-crs emits, fed back
    // through --crs on the same source, must convert to the same bytes as not
    // passing --crs at all. If the flag reformatted anything, this diverges.
    let dir = tmp_dir();
    let path = write_id_less_parquet(&dir);
    let src = path.to_str().unwrap();

    let printed = run(&[src, "--print-crs"], None);
    assert!(printed.ok, "{}", printed.stderr);

    let plain = dir.join("plain.fgb");
    assert!(run(&[src, plain.to_str().unwrap()], None).ok);

    let round_tripped = dir.join("round_tripped.fgb");
    let r = run(
        &[src, round_tripped.to_str().unwrap(), "--crs", "-"],
        Some(&printed.stdout),
    );
    assert!(r.ok, "{}", r.stderr);

    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&round_tripped).unwrap(),
        "piping --print-crs into --crs - must be a no-op",
    );
}

#[test]
fn a_shapefile_prj_prints_as_its_own_bytes() {
    // The sharpest check available on "verbatim": a `.prj` is loose WKT text on
    // disk, so the flag's stdout can be compared against the file itself rather
    // than against something geosetta also produced.
    let prj = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/duckdb_crs_pt.prj"
    );
    let shp = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/duckdb_crs_pt.shp"
    );

    let r = run(&[shp, "--print-crs"], None);
    assert!(r.ok, "{}", r.stderr);
    let on_disk = std::fs::read_to_string(prj).unwrap();
    assert_eq!(r.stdout, format!("{}\n", on_disk.trim_end()));
}

// The dialect-precedence rule (PROJJSON wins when a source carries both) has no
// end-to-end test here on purpose: no reader can produce that state. GeoParquet
// records PROJJSON only, FlatGeobuf and GeoPackage record WKT only, and a
// Shapefile `.prj` is WKT text on disk — so a `NamedCrs` holding both dialects
// is reachable only by constructing one. It is asserted directly as a unit test
// on `Crs::definition_body`, which is where the rule lives.

#[test]
fn a_geopackage_synthetic_id_is_not_reported_as_a_code() {
    // GeoPackage cannot store a CRS without an `srs_id`, so a WKT-only CRS is
    // written with organization NONE and an invented one. Round-tripping through
    // a `.gpkg` must not turn "no identity" into a code a resolver will choke on:
    // `--print-crs-code` reports nothing, and `--print-crs` — which can actually
    // help here — reports the definition.
    //
    // Without this the documented two-flag partition inverts for GeoPackage
    // alone: the code flag would *succeed* with `NONE:100000`, so a script
    // trying the code first would fail on it instead of falling through.
    let dir = tmp_dir();
    let path = dir.join("wkt_only.gpkg");
    let layers = vec![("a".to_string(), point_fc(Some(wkt_only_crs(ID_LESS_WKT_A))))];
    std::fs::write(
        &path,
        geosetta::geopackage::write_layers(None, &layers, false, false).unwrap(),
    )
    .unwrap();
    let src = path.to_str().unwrap();

    let r = run(&[src, "--print-crs-code"], None);
    assert!(!r.ok, "a synthetic id is not an identity to report");
    assert!(
        r.stdout.is_empty(),
        "and nothing may reach stdout: {:?}",
        r.stdout
    );

    let r = run(&[src, "--print-crs"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, format!("{ID_LESS_WKT_A}\n"));
}

#[test]
fn a_geopackage_with_a_real_code_still_reports_it() {
    // The guard is for the placeholder authority only.
    let dir = tmp_dir();
    let path = dir.join("real_code.gpkg");
    let layers = vec![("a".to_string(), point_fc(Some(code_only_crs())))];
    std::fs::write(
        &path,
        geosetta::geopackage::write_layers(None, &layers, false, false).unwrap(),
    )
    .unwrap();

    let r = run(&[path.to_str().unwrap(), "--print-crs-code"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "EPSG:7844\n");
}

#[test]
fn a_code_only_source_reports_nothing_and_names_the_other_flag() {
    // The complement of `reports_nothing_for_an_id_less_definition`: there the
    // code flag had nothing to say and this one would; here it is reversed. The
    // pair is what makes the two flags a usable diagnostic partition.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);

    let r = run(&[fgb.to_str().unwrap(), "--print-crs"], None);
    assert!(!r.ok, "a source with no definition text must exit nonzero");
    assert!(
        r.stdout.is_empty(),
        "stdout must stay clean: {:?}",
        r.stdout
    );
    assert!(r.stderr.contains("--print-crs-code"), "{}", r.stderr);

    // And that flag does work on this input, which is the point of naming it.
    let r = run(&[fgb.to_str().unwrap(), "--print-crs-code"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "EPSG:7844\n");
}

#[test]
fn a_wgs84_source_reports_nothing_here_but_a_code_there() {
    // The two flags disagree about a GeoJSON source, and correctly: its spec
    // fixes it at WGS 84, so there is a real identity to report and no text to
    // quote. A nonzero exit here is not "this file is broken".
    let dir = tmp_dir();
    let path = dir.join("wgs84.geojson");
    std::fs::write(
        &path,
        geosetta::write_features(Format::GeoJson, &point_fc(Some(Crs::Wgs84))).unwrap(),
    )
    .unwrap();
    let src = path.to_str().unwrap();

    let r = run(&[src, "--print-crs"], None);
    assert!(!r.ok);
    assert!(r.stdout.is_empty(), "{:?}", r.stdout);
    assert!(r.stderr.contains("implicit WGS 84 default"), "{}", r.stderr);
    assert!(
        r.stderr.contains("OGC:CRS84"),
        "names the code that does work: {}",
        r.stderr
    );

    let r = run(&[src, "--print-crs-code"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "OGC:CRS84\n");
}

#[test]
fn more_than_one_distinct_definition_is_an_error_naming_the_layers() {
    // A code is guaranteed single-line, so --print-crs-code can print one per
    // line. A definition body is not, so there is no delimiter to invent here:
    // error, and point at the flag that resolves it.
    let dir = tmp_dir();
    let path = dir.join("mixed.gpkg");
    let layers = vec![
        ("a".to_string(), point_fc(Some(wkt_only_crs(ID_LESS_WKT_A)))),
        ("b".to_string(), point_fc(Some(wkt_only_crs(ID_LESS_WKT_B)))),
    ];
    std::fs::write(
        &path,
        geosetta::geopackage::write_layers(None, &layers, false, false).unwrap(),
    )
    .unwrap();
    let src = path.to_str().unwrap();

    let r = run(&[src, "--print-crs"], None);
    assert!(!r.ok, "a mixed file must fail rather than pick one");
    assert!(r.stdout.is_empty(), "no partial output: {:?}", r.stdout);
    assert!(r.stderr.contains("--layer"), "{}", r.stderr);
    assert!(
        r.stderr.contains("\"a\"") && r.stderr.contains("\"b\""),
        "names them: {}",
        r.stderr
    );

    // --layer is the escape hatch, and it prints the one layer's body verbatim.
    let r = run(&[src, "--print-crs", "--layer", "a"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, format!("{ID_LESS_WKT_A}\n"));
}

#[test]
fn layers_sharing_one_definition_print_it_once() {
    // De-duplication, not concatenation: the ordinary case of a multi-layer file
    // whose layers agree still yields exactly one definition.
    let dir = tmp_dir();
    let path = dir.join("uniform.gpkg");
    let layers = vec![
        ("a".to_string(), point_fc(Some(wkt_only_crs(ID_LESS_WKT_A)))),
        ("b".to_string(), point_fc(Some(wkt_only_crs(ID_LESS_WKT_A)))),
        ("c".to_string(), point_fc(Some(wkt_only_crs(ID_LESS_WKT_A)))),
    ];
    std::fs::write(
        &path,
        geosetta::geopackage::write_layers(None, &layers, false, false).unwrap(),
    )
    .unwrap();

    let r = run(&[path.to_str().unwrap(), "--print-crs"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, format!("{ID_LESS_WKT_A}\n"));
}

#[test]
fn print_crs_reads_a_piped_source_from_stdin() {
    // `-` plus --from, same as every other mode: the flag is a reader like any
    // other and must not require a path on disk.
    let bytes =
        geosetta::write_features(Format::Parquet, &point_fc(Some(projjson_only_crs()))).unwrap();
    let dir = tmp_dir();
    let path = dir.join("piped.parquet");
    std::fs::write(&path, &bytes).unwrap();

    let mut cmd = Command::new(GEOSETTA);
    cmd.args(["-", "--print-crs", "--from", "parquet"])
        .stdin(Stdio::from(std::fs::File::open(&path).unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.spawn().expect("spawn").wait_with_output().expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{ID_LESS_PROJJSON}\n")
    );
}

#[test]
fn a_flatgeobuf_wkt_definition_prints_verbatim() {
    // Per-format coverage: FlatGeobuf records a CRS as WKT, so it exercises the
    // other dialect through the same flag. (GeoPackage's WKT path is covered by
    // the multi-layer cases below; GeoParquet's PROJJSON and a Shapefile `.prj`
    // are covered above.)
    let dir = tmp_dir();
    let path = dir.join("wkt_only.fgb");
    let fc = point_fc(Some(wkt_only_crs(ID_LESS_WKT_A)));
    std::fs::write(
        &path,
        geosetta::write_features(Format::FlatGeobuf, &fc).unwrap(),
    )
    .unwrap();

    let r = run(&[path.to_str().unwrap(), "--print-crs"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, format!("{ID_LESS_WKT_A}\n"));
}

// ===========================================================================
// --escape
// ===========================================================================

/// The crafted WKT from `stdout-security.org` finding 1 — an OSC title-bar
/// spoofing sequence (`ESC ] 0 ; PWNED BEL`) inside a CRS name, which is the
/// input `gdalsrsinfo` was checked against and reproduced byte-for-byte. The
/// tab and the newline are here on purpose: they must survive `--escape` while
/// everything around them is rendered.
const HOSTILE_WKT: &str =
    "GEOGCS[\"EVIL\u{1b}]0;PWNED\u{7}NAME\",\n\tDATUM[\"D\",SPHEROID[\"S\",6378137,298.26]]]";

fn write_hostile_fgb(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("hostile.fgb");
    let fc = point_fc(Some(wkt_only_crs(HOSTILE_WKT)));
    std::fs::write(
        &path,
        geosetta::write_features(Format::FlatGeobuf, &fc).unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn without_escape_the_hostile_bytes_reach_stdout_unchanged() {
    // The finding-1 regression test, and the decision it records: verbatim is
    // unconditional, matching GDAL's own CLI tools on this exact input. If this
    // ever starts passing only because something filtered the bytes, the
    // round-trip contract has been broken silently.
    let dir = tmp_dir();
    let path = write_hostile_fgb(&dir);

    let r = run(&[path.to_str().unwrap(), "--print-crs"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, format!("{HOSTILE_WKT}\n"));
    assert!(r.stdout.contains('\u{1b}'), "the ESC byte must survive");
    assert!(r.stdout.contains('\u{7}'), "the BEL byte must survive");
}

#[test]
fn escape_renders_the_hostile_bytes_readable_instead() {
    // Same input, opt-in flag: the escape sequence is shown rather than obeyed.
    let dir = tmp_dir();
    let path = write_hostile_fgb(&dir);

    let r = run(&[path.to_str().unwrap(), "--print-crs", "--escape"], None);
    assert!(r.ok, "{}", r.stderr);
    assert!(
        r.stdout.contains("^["),
        "ESC must render as ^[: {:?}",
        r.stdout
    );
    assert!(
        r.stdout.contains("^G"),
        "BEL must render as ^G: {:?}",
        r.stdout
    );
    assert!(
        !r.stdout.contains('\u{1b}'),
        "no raw ESC may remain: {:?}",
        r.stdout
    );
    assert!(
        !r.stdout.contains('\u{7}'),
        "no raw BEL may remain: {:?}",
        r.stdout
    );

    // Tab and newline pass through, so the definition stays readable as a
    // definition rather than becoming one escaped line.
    assert!(
        r.stdout.contains('\t'),
        "the tab must survive: {:?}",
        r.stdout
    );
    assert_eq!(
        r.stdout.lines().count(),
        2,
        "the interior newline must survive"
    );
}

#[test]
fn escape_renders_del_and_high_bit_bytes() {
    // The other two notations, end to end: `^?` for DEL and `M-` for a byte with
    // the high bit set (here the two bytes of `é`).
    let dir = tmp_dir();
    let wkt = "GEOGCS[\"R\u{e9}union\u{7f}\",DATUM[\"D\"]]";
    let path = dir.join("high_bit.fgb");
    let fc = point_fc(Some(wkt_only_crs(wkt)));
    std::fs::write(
        &path,
        geosetta::write_features(Format::FlatGeobuf, &fc).unwrap(),
    )
    .unwrap();

    let r = run(&[path.to_str().unwrap(), "--print-crs", "--escape"], None);
    assert!(r.ok, "{}", r.stderr);
    assert!(
        r.stdout.contains("^?"),
        "DEL must render as ^?: {:?}",
        r.stdout
    );
    assert!(
        r.stdout.contains("M-CM-)"),
        "é must render in M- notation: {:?}",
        r.stdout
    );
}

#[test]
fn escape_changes_nothing_about_which_definition_is_reported() {
    // It is a rendering pass, not a selection rule: the same source that has
    // nothing to report still reports nothing, with the same exit and message.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);

    let plain = run(&[fgb.to_str().unwrap(), "--print-crs"], None);
    let escaped = run(&[fgb.to_str().unwrap(), "--print-crs", "--escape"], None);
    assert!(!plain.ok && !escaped.ok);
    assert_eq!(plain.stdout, escaped.stdout, "both empty");
    assert_eq!(plain.stderr, escaped.stderr, "same message");
}

// ===========================================================================
// --crs
// ===========================================================================

#[test]
fn crs_override_reaches_the_shapefile_prj_writer() {
    // The payoff case: `.prj` is pure WKT text with no code slot, so a code-only
    // source produces no `.prj` at all. Supplying the definition fixes exactly
    // that, and the text lands verbatim.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let wkt_path = dir.join("gda2020.wkt");
    std::fs::write(&wkt_path, GDA2020_WKT1).unwrap();
    let out = dir.join("with_prj.shp");

    let bare = run(&[fgb.to_str().unwrap(), dir.join("no_prj.shp").to_str().unwrap()], None);
    assert!(bare.ok, "{}", bare.stderr);
    assert!(!dir.join("no_prj.prj").exists(), "a bare code cannot produce a .prj");
    assert!(bare.stderr.contains("--crs"), "the warning should name the way out: {}", bare.stderr);

    let r = run(
        &[fgb.to_str().unwrap(), out.to_str().unwrap(), "--crs", wkt_path.to_str().unwrap()],
        None,
    );
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(dir.join("with_prj.prj")).unwrap(), GDA2020_WKT1);
}

#[test]
fn crs_override_silences_the_unresolvable_geoparquet_warning() {
    // The regression this plan is really about: a code-only source bound for
    // GeoParquet warns that its CRS will be written as an id reference PROJ and
    // GDAL read as unknown. Supplying a definition must make both the warning
    // and the id-only output go away.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let projjson_path = dir.join("gda2020.projjson");
    std::fs::write(&projjson_path, GDA2020_PROJJSON).unwrap();

    let bare = run(&[fgb.to_str().unwrap(), dir.join("bare.parquet").to_str().unwrap()], None);
    assert!(bare.ok, "{}", bare.stderr);
    assert!(bare.stderr.contains("id reference"), "{}", bare.stderr);

    let out = dir.join("resolved.parquet");
    let r = run(
        &[fgb.to_str().unwrap(), out.to_str().unwrap(), "--crs", projjson_path.to_str().unwrap()],
        None,
    );
    assert!(r.ok, "{}", r.stderr);
    assert!(r.warnings().is_empty(), "no loss left to warn about: {:?}", r.warnings());

    // And the definition really is in the file, not just accepted at the door.
    let bytes = std::fs::read(&out).unwrap();
    let haystack = String::from_utf8_lossy(&bytes);
    assert!(haystack.contains("\"type\":\"GeographicCRS\""), "PROJJSON should reach the output");
    assert!(haystack.contains("Geocentric Datum of Australia 2020"));
}

#[test]
fn crs_override_reads_stdin() {
    // `-` is what makes a one-line pipe possible without a temp file.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let out = dir.join("from_stdin.shp");
    let r = run(&[fgb.to_str().unwrap(), out.to_str().unwrap(), "--crs", "-"], Some(GDA2020_WKT1));
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(dir.join("from_stdin.prj")).unwrap(), GDA2020_WKT1);
}

#[test]
fn crs_override_applies_to_every_geopackage_layer_and_says_so() {
    // GeoPackage branches out of the plain single-collection pipeline, so the
    // override has to be wired into its fan-out too or it would silently no-op
    // on every .gpkg. When the layers disagreed, collapsing them onto one
    // identity is a real relabel and is announced rather than done quietly.
    let dir = tmp_dir();
    let src = dir.join("mixed_src.gpkg");
    let epsg_3857 = Crs::Named(NamedCrs {
        authority: Some("EPSG".into()),
        code: Some("3857".into()),
        ..Default::default()
    });
    let layers = vec![
        ("a".to_string(), point_fc(Some(code_only_crs()))),
        ("b".to_string(), point_fc(Some(epsg_3857))),
    ];
    std::fs::write(&src, geosetta::geopackage::write_layers(None, &layers, false, false).unwrap())
        .unwrap();
    let wkt_path = dir.join("gda2020.wkt");
    std::fs::write(&wkt_path, GDA2020_WKT1).unwrap();

    let out = dir.join("relabeled.gpkg");
    let r = run(
        &[src.to_str().unwrap(), out.to_str().unwrap(), "--crs", wkt_path.to_str().unwrap()],
        None,
    );
    assert!(r.ok, "{}", r.stderr);
    assert!(r.stderr.contains("relabels all 2 layers"), "{}", r.stderr);
    assert!(r.stderr.contains("2 different CRSes"), "{}", r.stderr);

    // Every layer now carries the override, definition and all.
    for (_, fc) in geosetta::geopackage::read_layers(&std::fs::read(&out).unwrap()).unwrap() {
        match fc.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority_code(), Some(("EPSG", "7844")));
                assert!(n.wkt.as_deref().is_some_and(|w| w.contains("GDA2020")));
            }
            other => panic!("expected the override, got {other:?}"),
        }
    }

    // A single-layer source has nothing to announce — the user asked for the
    // relabel and got exactly it.
    let single = dir.join("single_src.gpkg");
    let one = vec![("a".to_string(), point_fc(Some(code_only_crs())))];
    std::fs::write(&single, geosetta::geopackage::write_layers(None, &one, false, false).unwrap())
        .unwrap();
    let r = run(
        &[
            single.to_str().unwrap(),
            dir.join("single_out.gpkg").to_str().unwrap(),
            "--crs",
            wkt_path.to_str().unwrap(),
        ],
        None,
    );
    assert!(r.ok, "{}", r.stderr);
    assert!(!r.stderr.contains("relabels"), "{}", r.stderr);
}

#[test]
fn a_malformed_override_is_an_error_not_a_shrug() {
    // Strict fallback: geosetta never guesses, so it must not quietly fall back
    // to the source's own CRS when handed something it can't read.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let bad = dir.join("bad.txt");
    std::fs::write(&bad, "EPSG:7844\n").unwrap();
    let r = run(
        &[fgb.to_str().unwrap(), dir.join("nope.shp").to_str().unwrap(), "--crs", bad.to_str().unwrap()],
        None,
    );
    assert!(!r.ok);
    assert!(r.stderr.contains("--crs"), "{}", r.stderr);
    assert!(!dir.join("nope.shp").exists(), "nothing should be written");
}

#[test]
fn crs_from_stdin_and_input_from_stdin_is_rejected() {
    // Both want the same stream; picking one silently would make the other look
    // as though it had simply found nothing.
    let r = run(&["-", "out.shp", "--from", "fgb", "--crs", "-"], Some(GDA2020_WKT1));
    assert!(!r.ok);
    assert!(r.stderr.contains("stdin"), "{}", r.stderr);
}

// ===========================================================================
// Composition with a real, unrelated resolver
// ===========================================================================

#[test]
fn composes_with_an_external_resolver_over_a_pipe() {
    // The whole design in one shell line, run for real: geosetta reports the
    // code it cannot resolve, a tool that has no idea geosetta exists resolves
    // it, and geosetta accepts the result on stdin. Every process here is one a
    // user would have typed; geosetta spawns none of them.
    //
    // `projinfo` is PROJ's, picked only because this repo already requires it
    // for the CRS oracles. Nothing about the flags is specific to it — any tool
    // that prints WKT or PROJJSON would substitute unchanged.
    if Command::new("projinfo").arg("--version").output().is_err() {
        eprintln!("skipping: projinfo (PROJ) not installed");
        return;
    }
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);

    let code = run(&[fgb.to_str().unwrap(), "--print-crs-code"], None);
    assert!(code.ok, "{}", code.stderr);
    let code = code.stdout.trim();
    assert_eq!(code, "EPSG:7844");

    let resolved = Command::new("projinfo")
        .args(["-o", "WKT1_GDAL", "-q", code])
        .output()
        .expect("run projinfo");
    let wkt = String::from_utf8_lossy(&resolved.stdout).into_owned();
    assert!(wkt.contains("GDA2020"), "projinfo returned: {wkt}");

    let out = dir.join("piped.shp");
    let r = run(&[fgb.to_str().unwrap(), out.to_str().unwrap(), "--crs", "-"], Some(&wkt));
    assert!(r.ok, "{}", r.stderr);
    let prj = std::fs::read_to_string(dir.join("piped.prj")).unwrap();
    assert!(prj.contains("GDA2020"), "{prj}");
    assert!(r.warnings().is_empty(), "the gap should be closed: {:?}", r.warnings());
}

// ===========================================================================
// The cross-repo pipeline: --print-crs -> geoscribe --identify -> --crs -
// ===========================================================================
// The case `--print-crs` was built for. `--print-crs-code` cannot help with an
// id-less definition (there is no code to report), and `geoscribe --identify`
// cannot reach one buried in a container — so this flag is the join between
// them. Gated on `geoscribe` being installed, exactly like the `projinfo` case
// above: nothing in geosetta knows this tool's name, and these tests are the
// only place in the crate that does.

/// Where to find `geoscribe`, or `None` when it is not available.
///
/// Prefers the sibling binary in this build's own output directory — the two
/// crates share a workspace, so `cargo test --workspace` has just built the
/// exact code these tests are meant to exercise, and testing against a
/// separately-installed copy would silently check the wrong version. Falls back
/// to `PATH` so the check still works when geosetta is built standalone (it is
/// published to crates.io on its own, where no sibling exists), and skips when
/// neither is there, exactly like the `projinfo` case above.
fn geoscribe_bin() -> Option<std::path::PathBuf> {
    let mut p = std::env::current_exe().ok()?;
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    let sibling = p.join("geoscribe");
    if sibling.exists() {
        return Some(sibling);
    }
    match Command::new("geoscribe").arg("--help").output() {
        Ok(_) => Some("geoscribe".into()),
        Err(_) => {
            eprintln!("skipping: geoscribe is neither built in this workspace nor on PATH");
            None
        }
    }
}

/// Run `geoscribe` over `stdin_text`, returning `(stdout, exit code)`.
fn geoscribe(args: &[&str], stdin_text: &str) -> (String, i32) {
    let mut child = Command::new(geoscribe_bin().expect("geoscribe available"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn geoscribe");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("run geoscribe");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn the_full_three_stage_pipeline_identifies_and_reinstalls_a_crs() {
    // An id-less Esri `.prj`: no AUTHORITY node anywhere, so `--print-crs-code`
    // reports nothing and only the definition body can be resolved. geoscribe
    // recovers the identity by name, validated against the WKT's own ellipsoid,
    // and hands back the authoritative definition, which `--crs -` installs.
    if geoscribe_bin().is_none() {
        return;
    }
    let dir = tmp_dir();
    let shp = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/duckdb_crs_pt.shp"
    );

    // Stage 1.
    let printed = run(&[shp, "--print-crs"], None);
    assert!(printed.ok, "{}", printed.stderr);
    assert!(printed.stdout.starts_with("GEOGCS["), "{}", printed.stdout);

    // Stage 2 — the tool geosetta never runs itself.
    let (definition, status) = geoscribe(&["--identify", "--projjson"], &printed.stdout);
    assert_eq!(status, 0, "geoscribe could not identify: {definition}");
    assert!(
        definition.contains("\"type\""),
        "expected PROJJSON: {definition}"
    );

    // Stage 3.
    let out = dir.join("piped.parquet");
    let r = run(
        &[shp, out.to_str().unwrap(), "--crs", "-"],
        Some(&definition),
    );
    assert!(r.ok, "{}", r.stderr);

    // And the identity actually landed: the recovered definition carries an id,
    // so the written GeoParquet reports a code the source never had.
    let r = run(&[out.to_str().unwrap(), "--print-crs-code"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(
        r.stdout, "OGC:CRS84\n",
        "the .prj is an Esri WGS 84 spelling"
    );
}

#[test]
fn an_ambiguous_identification_fails_the_pipeline_loudly() {
    // geoscribe refuses to pick between equally-supported candidates: exit 2,
    // nothing on stdout. That empty stdout must hit geosetta's hard error on an
    // empty `--crs` rather than being read as "no override" and silently
    // producing a file with the wrong CRS.
    if geoscribe_bin().is_none() {
        return;
    }
    // Several real CRSes share this name and ellipsoid (EPSG:6339, ESRI:102057).
    let ambiguous = r#"PROJCS["NAD_1983_2011_UTM_Zone_10N",GEOGCS["GCS_NAD_1983_2011",DATUM["D_NAD_1983_2011",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-123.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;

    let (definition, status) = geoscribe(&["--identify", "--projjson"], ambiguous);
    assert_eq!(status, 2, "expected the ambiguous exit");
    assert!(
        definition.is_empty(),
        "and nothing on stdout: {definition:?}"
    );

    let dir = tmp_dir();
    let shp = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/duckdb_crs_pt.shp"
    );
    let out = dir.join("should_not_exist.parquet");
    let r = run(
        &[shp, out.to_str().unwrap(), "--crs", "-"],
        Some(&definition),
    );
    assert!(
        !r.ok,
        "an empty override must fail, not be treated as no override"
    );
    assert!(!out.exists(), "and nothing may be written");
}

#[test]
fn the_pipeline_carries_an_id_less_geoparquet_definition() {
    // The GeoParquet half of the same loop, and the reason both plans said
    // neither was useful alone. This was `#[ignore]`d until
    // `geoscribe/plans/projjson-identify.org` landed on 2026-08-19: geoscribe
    // sniffed no dialect and ran its WKT tokenizer over the JSON. Both halves
    // now exist, so the loop closes.
    if geoscribe_bin().is_none() {
        return;
    }
    let dir = tmp_dir();

    // Build the input the way one actually comes to exist, rather than by hand:
    // convert an Esri Shapefile. Its `.prj` has no AUTHORITY node, so there is no
    // id for `Crs::from_authority_code` to lift, and the PROJJSON geosetta writes
    // into `geo` carries none either. `write_id_less_parquet`'s fixture is no use
    // here — its CRS name is invented, so it is correctly unidentifiable, which
    // is the right shape for testing `--print-crs` and the wrong one for testing
    // the loop.
    let shp = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/duckdb_crs_pt.shp"
    );
    let src_path = dir.join("id_less_from_shapefile.parquet");
    let src = src_path.to_str().unwrap();
    assert!(run(&[shp, src], None).ok);

    // Precondition, and the finding this test rests on: the file really is
    // id-less, so `--print-crs-code` has nothing to report and only the
    // definition body can be resolved.
    let code = run(&[src, "--print-crs-code"], None);
    assert!(
        !code.ok && code.stdout.is_empty(),
        "expected an id-less file: {:?}",
        code.stdout
    );

    let printed = run(&[src, "--print-crs"], None);
    assert!(printed.ok, "{}", printed.stderr);
    assert!(
        printed.stdout.starts_with('{'),
        "expected PROJJSON: {}",
        printed.stdout
    );

    let (definition, status) = geoscribe(&["--identify", "--projjson"], &printed.stdout);
    assert_eq!(status, 0, "geoscribe could not identify the PROJJSON");

    // FlatGeobuf, not Parquet: geosetta refuses a same-format conversion, and the
    // point here is the CRS crossing a format boundary intact anyway.
    let out = dir.join("identified.fgb");
    let r = run(
        &[src, out.to_str().unwrap(), "--crs", "-"],
        Some(&definition),
    );
    assert!(r.ok, "{}", r.stderr);

    // The loop closed: the output now reports an identity the input never had.
    let code = run(&[out.to_str().unwrap(), "--print-crs-code"], None);
    assert!(code.ok, "{}", code.stderr);
    assert_eq!(code.stdout, "OGC:CRS84\n");
}

// ===========================================================================
// The piping contract itself: clean stdout, clean failures
//
// These flags exist to be used inside `$(...)` and pipelines, so the shape of
// their output and their failures *is* the feature. A stray byte on stdout or a
// zero exit on a failed lookup would silently corrupt whatever consumes them.
// ===========================================================================

#[test]
fn print_crs_code_writes_nothing_but_the_code_to_stdout() {
    // Everything conversational — progress, warnings, errors — belongs on
    // stderr, so `$(geosetta … --print-crs-code)` captures a bare code and
    // nothing else. `--progress` is the adversarial case: it is the flag whose
    // whole job is to be chatty.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let r = run(&[fgb.to_str().unwrap(), "--print-crs-code", "--progress"], None);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "EPSG:7844\n", "stdout must carry the code alone");
    // Trailing newline only — nothing a shell substitution has to strip beyond
    // the one it strips anyway.
    assert_eq!(r.stdout.trim_end_matches('\n'), "EPSG:7844");
}

#[test]
fn print_crs_code_reads_a_piped_source_from_stdin() {
    // The input side of the pipe: a source that only exists as a stream still
    // reports its code, so `producer | geosetta - --print-crs-code --from fgb`
    // works the same as against a file.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let bytes = std::fs::read(&fgb).unwrap();

    let mut child = Command::new(GEOSETTA)
        .args(["-", "--print-crs-code", "--from", "fgb"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn geosetta");
    child.stdin.as_mut().unwrap().write_all(&bytes).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "EPSG:7844\n");
}

#[test]
fn a_full_two_stage_pipeline_carries_the_override_through() {
    // Both seams at once, which is the realistic shape: the *input* arrives on
    // stdin from an upstream geosetta, while `--crs` comes from a file. They
    // coexist precisely because only one of them claims stdin — the case the
    // conflict check exists to keep honest.
    //
    // The stages convert *between* formats (fgb -> parquet -> shp): a stage
    // whose input and output are the same format is rejected outright as having
    // nothing to convert, so a pass-through stage isn't a thing you can build.
    // The bare EPSG:7844 survives the intermediate hop as GeoParquet's id-only
    // reference — unresolvable downstream, which is exactly what `--crs` fixes
    // at the end of the pipe.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let wkt_path = dir.join("gda2020.wkt");
    std::fs::write(&wkt_path, GDA2020_WKT1).unwrap();

    let staged = Command::new(GEOSETTA)
        .args([fgb.to_str().unwrap(), "-", "--to", "parquet"])
        .output()
        .expect("stage one");
    assert!(staged.status.success(), "{}", String::from_utf8_lossy(&staged.stderr));
    assert!(!staged.stdout.is_empty(), "stage one should stream bytes to stdout");

    let out = dir.join("piped_stage_two.shp");
    let mut child = Command::new(GEOSETTA)
        .args([
            "-",
            out.to_str().unwrap(),
            "--from",
            "parquet",
            "--crs",
            wkt_path.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("stage two");
    child.stdin.as_mut().unwrap().write_all(&staged.stdout).unwrap();
    drop(child.stdin.take());
    let res = child.wait_with_output().unwrap();
    assert!(res.status.success(), "{}", String::from_utf8_lossy(&res.stderr));
    assert_eq!(std::fs::read_to_string(dir.join("piped_stage_two.prj")).unwrap(), GDA2020_WKT1);
}

#[test]
fn an_empty_override_is_rejected_rather_than_treated_as_no_override() {
    // The failure mode that matters most for piping: an upstream tool that
    // produced nothing (a bad code, a tool that errored) must not read as "the
    // user didn't ask for an override" — that would quietly emit the very
    // unresolvable CRS the pipeline was built to fix.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let out = dir.join("empty_override.shp");

    let r = run(&[fgb.to_str().unwrap(), out.to_str().unwrap(), "--crs", "-"], Some(""));
    assert!(!r.ok, "empty stdin must fail, not fall through");
    assert!(r.stderr.contains("empty"), "{}", r.stderr);
    assert!(!out.exists(), "nothing should be written");

    let empty_file = dir.join("empty.wkt");
    std::fs::write(&empty_file, "   \n\n").unwrap();
    let r = run(
        &[fgb.to_str().unwrap(), out.to_str().unwrap(), "--crs", empty_file.to_str().unwrap()],
        None,
    );
    assert!(!r.ok, "a whitespace-only file must fail too");
    assert!(!out.exists());
}

#[test]
fn a_missing_override_file_fails_before_anything_is_written() {
    // The override is read up front precisely so a typo'd path fails before the
    // input is parsed or the output touched, rather than midway through.
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let out = dir.join("never_written.shp");
    let r = run(
        &[
            fgb.to_str().unwrap(),
            out.to_str().unwrap(),
            "--crs",
            dir.join("does_not_exist.wkt").to_str().unwrap(),
        ],
        None,
    );
    assert!(!r.ok);
    assert!(!out.exists(), "no partial output");
    assert!(r.stdout.is_empty(), "errors belong on stderr");
}

#[test]
fn the_flags_reject_the_argument_shapes_that_cannot_mean_anything() {
    let dir = tmp_dir();
    let fgb = write_code_only_fgb(&dir);
    let fgb = fgb.to_str().unwrap();

    // A diagnostic that writes nothing cannot also be given somewhere to write.
    let r = run(&[fgb, "out.shp", "--print-crs-code"], None);
    assert!(!r.ok);
    assert!(r.stderr.contains("no output"), "{}", r.stderr);
    assert!(r.stdout.is_empty());

    // Asking what the source's code is *while replacing* it is contradictory.
    let r = run(&[fgb, "--print-crs-code", "--crs", "x.wkt"], None);
    assert!(!r.ok);
    assert!(r.stderr.contains("--crs"), "{}", r.stderr);

    // --crs with no value swallows nothing silently.
    let r = run(&[fgb, "out.shp", "--crs"], None);
    assert!(!r.ok);
    assert!(r.stderr.contains("--crs"), "{}", r.stderr);

    // Two claimants on one stdin.
    let r = run(&["-", "out.shp", "--from", "fgb", "--crs", "-"], Some(GDA2020_WKT1));
    assert!(!r.ok);
    assert!(r.stderr.contains("stdin"), "{}", r.stderr);
    assert!(r.stdout.is_empty());
}

#[test]
fn failed_lookups_and_failed_overrides_both_exit_nonzero() {
    // Explicitly pinned because `set -e` and `$(...)` depend on it: every way
    // these flags can fail must be distinguishable from success by exit status
    // alone, without parsing any message.
    let dir = tmp_dir();
    let no_crs = dir.join("plain.wkt");
    std::fs::write(&no_crs, "POINT (1 2)\n").unwrap();
    assert!(!run(&[no_crs.to_str().unwrap(), "--print-crs-code"], None).ok);

    let fgb = write_code_only_fgb(&dir);
    let bad = dir.join("bad.wkt");
    std::fs::write(&bad, "{ not json either").unwrap();
    assert!(
        !run(
            &[fgb.to_str().unwrap(), dir.join("x.shp").to_str().unwrap(), "--crs", bad.to_str().unwrap()],
            None
        )
        .ok
    );

    // ...and the success path really is zero, so the assertions above mean
    // something.
    assert!(run(&[fgb.to_str().unwrap(), "--print-crs-code"], None).ok);
}
