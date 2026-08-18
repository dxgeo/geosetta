//! End-to-end coverage for the two flags that let a CRS geosetta cannot resolve
//! be resolved by something else: `--print-crs-code` (report the source's
//! authority code) and `--crs` (accept the definition that came back).
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

fn tmp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("geosetta-crs-external-{}", std::process::id()));
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
    std::fs::write(&path, geosetta::geopackage::write_layers(None, &layers, false).unwrap())
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
    assert!(!r.stderr.contains("warning"), "no loss left to warn about: {}", r.stderr);

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
    std::fs::write(&src, geosetta::geopackage::write_layers(None, &layers, false).unwrap())
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
    std::fs::write(&single, geosetta::geopackage::write_layers(None, &one, false).unwrap())
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
    assert!(!r.stderr.contains("warning"), "the gap should be closed: {}", r.stderr);
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
