//! What `plans/cli-review.org` found by running the binary, run again. `cli.rs`
//! tests its parsing alone; these are the fixes that only show in a whole run:
//! exit codes, what reaches which stream, and what lands on disk.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

const GEOJSON: &str = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"a":1},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;

/// A temp directory private to the calling test, on `same_format.rs`'s
/// reasoning: libtest runs a thread per test and these write under fixed names.
fn tmp_dir() -> PathBuf {
    let label = std::thread::current().name().unwrap_or("main").replace("::", "-");
    let dir = std::env::temp_dir().join(format!("geosetta-cli-review-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(GEOSETTA).args(args).output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn source(dir: &Path) -> String {
    let input = dir.join("in.geojson");
    std::fs::write(&input, GEOJSON).unwrap();
    input.display().to_string()
}

/// The panic this must not be: 101 is what Rust exits a panic with.
fn assert_no_panic(out: &Output) {
    assert_ne!(out.status.code(), Some(101), "{}", stderr(out));
    assert!(!stderr(out).contains("panicked"), "{}", stderr(out));
}

/// C6: help is an answer, on stdout, and so is the version.
#[test]
fn help_and_version_go_to_stdout_and_succeed() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("usage: geosetta"));
        assert!(out.stderr.is_empty());
    }
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("geosetta {}\n", env!("CARGO_PKG_VERSION")));
}

/// A closed pipe, as `geosetta ... | true` gives one. The read end closes
/// before the child runs, so the write fails every time and not only by a race.
fn run_into_closed_pipe(args: &[&str]) -> Output {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    Command::new(GEOSETTA).args(args).stdout(Stdio::from(writer)).stderr(Stdio::piped()).output().unwrap()
}

/// C8: every way of writing to stdout fails the same way, and none panics.
#[test]
fn a_closed_pipe_is_an_error_and_not_a_panic() {
    let dir = tmp_dir();
    let input = source(&dir);
    let crs = dir.join("crs.wkt");
    std::fs::write(&crs, r#"GEOGCS["GDA94",DATUM["GDA94",SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#).unwrap();
    let with_crs = dir.join("with_crs.fgb");
    let made = run(&[&input, with_crs.to_str().unwrap(), "--crs", crs.to_str().unwrap()]);
    assert!(made.status.success(), "{}", stderr(&made));
    let with_crs = with_crs.to_str().unwrap();
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec![input.as_str(), "--print-crs-code"],
        vec![with_crs, "--print-crs"],
        vec![with_crs, "--print-crs", "--escape"],
        vec![input.as_str(), "-", "--to", "wkt"],
    ] {
        let out = run_into_closed_pipe(&args);
        assert_no_panic(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("geosetta: i/o error"), "{args:?}: {}", stderr(&out));
    }
}

/// C3's table: GeoJSON and FlatGeobuf keep the name, and read it back.
#[test]
fn layer_names_an_output_that_can_hold_a_name() {
    let dir = tmp_dir();
    let input = source(&dir);
    for ext in ["geojson", "fgb"] {
        let output = dir.join(format!("out.{ext}"));
        let out = run(&[&input, output.to_str().unwrap(), "--layer", "cities"]);
        assert!(out.status.success(), "{ext}: {}", stderr(&out));

        let gpkg = dir.join(format!("{ext}.gpkg"));
        let out = run(&[output.to_str().unwrap(), gpkg.to_str().unwrap()]);
        assert!(out.status.success(), "{ext}: {}", stderr(&out));
        // The table is what the name became; asking for it by name finds it.
        let out = run(&[gpkg.to_str().unwrap(), "--print-crs-code", "--layer", "cities"]);
        assert!(out.status.success(), "{ext}: {}", stderr(&out));
    }
}

/// C3's table: no slot, so a refusal, naming the flag and writing nothing.
#[test]
fn layer_is_refused_where_one_file_has_nowhere_to_put_it() {
    let dir = tmp_dir();
    let input = source(&dir);
    for ext in ["parquet", "csv", "wkt", "kml", "kmz", "shp"] {
        let output = dir.join(format!("out.{ext}"));
        let out = run(&[&input, output.to_str().unwrap(), "--layer", "cities"]);
        assert_eq!(out.status.code(), Some(1), "{ext}");
        assert!(stderr(&out).contains("--layer"), "{ext}: {}", stderr(&out));
        assert!(!output.exists(), "{ext}");
    }
}

/// C3's table: the fix the first review proposed would have broken this.
#[test]
fn layer_names_the_files_of_a_directory_output() {
    let dir = tmp_dir();
    let input = source(&dir);
    for ext in ["parquet", "csv", "wkt", "kml", "kmz", "shp", "fgb", "geojson"] {
        let od = dir.join(format!("od-{ext}/"));
        let out = run(&[&input, &format!("{}/", od.display()), "--to", ext, "--layer", "cities"]);
        assert!(out.status.success(), "{ext}: {}", stderr(&out));
        assert!(od.join(format!("cities.{ext}")).exists(), "{ext}");
    }
    // An existing directory counts, with no trailing slash.
    let od = dir.join("existing");
    std::fs::create_dir_all(&od).unwrap();
    let out = run(&[&input, od.to_str().unwrap(), "--to", "csv", "--layer", "cities"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(od.join("cities.csv").exists());
}

/// C3's table: a GeoPackage source is selecting, so any one file will do.
#[test]
fn layer_selects_from_a_geopackage_for_any_output() {
    let dir = tmp_dir();
    let input = source(&dir);
    let gpkg = dir.join("in.gpkg");
    assert!(run(&[&input, gpkg.to_str().unwrap(), "--layer", "cities"]).status.success());
    for ext in ["parquet", "csv", "wkt", "kml", "kmz", "shp", "fgb", "geojson"] {
        let output = dir.join(format!("out.{ext}"));
        let out = run(&[gpkg.to_str().unwrap(), output.to_str().unwrap(), "--layer", "cities"]);
        assert!(out.status.success(), "{ext}: {}", stderr(&out));
    }
}

/// A default name is the file's stem, which no output should start recording
/// unasked: this is what keeps `equiv.py` byte-identical.
#[test]
fn no_layer_records_no_name() {
    let dir = tmp_dir();
    let input = source(&dir);
    let output = dir.join("out.geojson");
    assert!(run(&[&input, output.to_str().unwrap()]).status.success());
    assert!(!std::fs::read_to_string(&output).unwrap().contains("\"name\""));
}

/// The review's smaller issue: which file was missing.
#[test]
fn a_file_error_names_the_path() {
    let dir = tmp_dir();
    let input = source(&dir);
    let missing = dir.join("missing.geojson");
    let cases = [
        (vec![missing.to_str().unwrap().to_string(), "out.fgb".to_string()], "missing.geojson"),
        (vec![input.clone(), dir.join("no/such/out.fgb").display().to_string()], "out.fgb"),
        (vec![input.clone(), dir.join("o.fgb").display().to_string(), "--crs".into(), dir.join("nope.wkt").display().to_string()], "nope.wkt"),
    ];
    for (args, name) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(stderr(&out).contains(name), "{args:?}: {}", stderr(&out));
    }
}

/// C1: no argument panics, here where no such file can exist too.
#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_does_not_panic() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = tmp_dir();
    let input = source(&dir);
    let odd = OsStr::from_bytes(b"\xff\xfe.geojson");
    let out = Command::new(GEOSETTA).arg(odd).arg(dir.join("o.parquet")).output().unwrap();
    assert_no_panic(&out);
    assert_eq!(out.status.code(), Some(1));

    let out = Command::new(GEOSETTA).arg(&input).arg(dir.join("o.gpkg")).arg("--layer").arg(odd).output().unwrap();
    assert_no_panic(&out);
    assert!(stderr(&out).contains("--layer") && stderr(&out).contains("UTF-8"), "{}", stderr(&out));
}

/// C1 in full: macOS refuses to create such a file (errno 92), so only here can
/// a whole conversion run, and a Shapefile find its siblings.
#[cfg(target_os = "linux")]
#[test]
fn a_file_named_in_no_encoding_converts() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = tmp_dir();
    let name = |bytes: &[u8]| dir.join(OsStr::from_bytes(bytes));
    let input = name(b"\xff\xfe.geojson");
    std::fs::write(&input, GEOJSON).unwrap();

    let parquet = name(b"\xfd.parquet");
    let out = Command::new(GEOSETTA).arg(&input).arg(&parquet).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(parquet.exists());

    let shp = name(b"\xfc.shp");
    let out = Command::new(GEOSETTA).arg(&input).arg(&shp).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(name(b"\xfc.dbf").exists());
    let back = dir.join("back.geojson");
    let out = Command::new(GEOSETTA).arg(&shp).arg(&back).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));

    // A directory whose own name is not text keeps it.
    let od = name(b"\xfb");
    std::fs::create_dir(&od).unwrap();
    let out = Command::new(GEOSETTA).arg(&input).arg(&od).args(["--to", "csv", "--layer", "cities"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(od.join("cities.csv").exists());

    // The default layer name is text, so it cannot be this one, and says so.
    let gpkg = dir.join("out.gpkg");
    let out = Command::new(GEOSETTA).arg(&input).arg(&gpkg).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("not UTF-8"), "{}", stderr(&out));
}
