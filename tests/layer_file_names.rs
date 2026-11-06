//! A directory output names each file after its layer, and a layer's name comes
//! from the input: a GeoJSON `name`, a FlatGeobuf header, a GeoPackage table.
//! So it is untrusted text on its way to being a path
//! (`plans/cli-review.org` C9).
//!
//! These invoke the binary rather than the library because the library never
//! sees a path: turning a name into one is `main.rs`'s `write_layers`.

use std::path::{Path, PathBuf};
use std::process::Command;

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

/// Names that are not one plain file name. An absolute one depends on the test's
/// own directory, so [`names_that_are_paths_are_refused`] adds it.
const NOT_FILE_NAMES: &[&str] = &["../escaped", "../../escaped", "a/b", "./x", "x/", "x/.", "..", ".", ""];

/// A temp directory private to the calling test, on `same_format.rs`'s
/// reasoning: libtest runs a thread per test and these write under fixed names.
fn tmp_dir() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let label = match std::thread::current().name() {
        Some(name) if name != "main" => name.replace("::", "-"),
        _ => format!("seq{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
    };
    let dir = std::env::temp_dir().join(format!("geosetta-layer-names-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(GEOSETTA).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// A one-point GeoJSON whose `name` member is `name`, already JSON-escaped.
fn geojson(name: &str) -> String {
    format!(
        r#"{{"type":"FeatureCollection","name":"{name}","features":[{{"type":"Feature","properties":{{"a":1}},"geometry":{{"type":"Point","coordinates":[1,2]}}}}]}}"#
    )
}

/// Every file under `dir`, relative to it and sorted.
fn files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Run `input` into a directory `work/od/` under `root`, expecting a refusal
/// that names `name` and writes nothing, the directory included.
fn assert_refused(root: &Path, input: &Path, name: &str, to: &str) {
    let od = root.join("work/od/");
    let before = files(root);
    let (ok, err) = run(&[input.to_str().unwrap(), &format!("{}/", od.display()), "--to", to]);
    assert!(!ok, "layer {name:?} to {to}: ran, and said {err}");
    assert!(err.contains("not a plain file name"), "layer {name:?} to {to}: {err}");
    assert!(err.contains(&format!("{name:?}")), "the refusal names the layer {name:?}: {err}");
    assert_eq!(files(root), before, "layer {name:?} to {to} wrote something");
    assert!(!od.exists(), "layer {name:?} to {to} made the directory");
}

#[test]
fn names_that_are_paths_are_refused() {
    let root = tmp_dir();
    std::fs::create_dir_all(root.join("work")).unwrap();
    let input = root.join("in.geojson");
    let absolute = root.join("victim").display().to_string();
    for name in NOT_FILE_NAMES.iter().copied().chain([absolute.as_str()]) {
        std::fs::write(&input, geojson(name)).unwrap();
        // Shapefile too: its siblings are written from the same path.
        for to in ["csv", "fgb", "parquet", "geojson", "kml", "shp"] {
            assert_refused(&root, &input, name, to);
        }
    }
}

/// An absolute name replaces the directory in `Path::join`, so this is the case
/// that reaches furthest. An existing file there must survive.
#[test]
fn an_absolute_name_does_not_overwrite_what_it_names() {
    let root = tmp_dir();
    std::fs::create_dir_all(root.join("work")).unwrap();
    let victim = root.join("victim");
    let target = root.join("victim.csv");
    std::fs::write(&target, "precious").unwrap();
    let input = root.join("in.geojson");
    std::fs::write(&input, geojson(victim.to_str().unwrap())).unwrap();
    assert_refused(&root, &input, victim.to_str().unwrap(), "csv");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
}

/// The name reaches the path from each format that records one.
#[test]
fn a_flatgeobuf_header_and_a_geopackage_table_are_refused_alike() {
    let root = tmp_dir();
    std::fs::create_dir_all(root.join("work")).unwrap();
    let source = root.join("in.geojson");
    std::fs::write(&source, geojson("../escaped")).unwrap();
    for ext in ["fgb", "gpkg"] {
        let input = root.join(format!("in.{ext}"));
        let (ok, err) = run(&[source.to_str().unwrap(), input.to_str().unwrap()]);
        assert!(ok, "{err}");
        assert_refused(&root, &input, "../escaped", "csv");
    }
}

/// The name typed as `--layer` goes the same way; a user who wants that path can
/// give it as the output.
#[test]
fn a_typed_layer_is_refused_alike() {
    let root = tmp_dir();
    let input = root.join("in.geojson");
    std::fs::write(&input, geojson("fine")).unwrap();
    let od = root.join("od/");
    let (ok, err) = run(&[input.to_str().unwrap(), &format!("{}/", od.display()), "--to", "csv", "--layer", "../escaped"]);
    assert!(!ok, "{err}");
    assert!(err.contains("not a plain file name"), "{err}");
    assert!(!root.join("escaped.csv").exists());
}

/// All of a multi-layer source is checked before the first file, so one bad name
/// does not leave half a directory. `--layer` still reaches the good layer.
#[test]
fn one_bad_layer_stops_the_run_and_the_others_stay_reachable() {
    let root = tmp_dir();
    std::fs::create_dir_all(root.join("work")).unwrap();
    let source = root.join("in.geojson");
    let gpkg = root.join("in.gpkg");
    for name in ["aaa_good", "../escaped"] {
        std::fs::write(&source, geojson("x")).unwrap();
        let (ok, err) = run(&[source.to_str().unwrap(), gpkg.to_str().unwrap(), "--layer", name]);
        assert!(ok, "{err}");
    }
    assert_refused(&root, &gpkg, "../escaped", "csv");

    let od = root.join("work/od/");
    let (ok, err) = run(&[gpkg.to_str().unwrap(), &format!("{}/", od.display()), "--to", "csv", "--layer", "aaa_good"]);
    assert!(ok, "{err}");
    assert_eq!(files(&od), ["aaa_good.csv"]);
}

/// The name came from the file, so the refusal must not hand a terminal the
/// bytes it carries (`stdout-security.org`).
#[test]
fn the_refusal_does_not_print_control_bytes() {
    let root = tmp_dir();
    let input = root.join("in.geojson");
    std::fs::write(&input, geojson("../evil\\u001b]0;PWNED\\u0007")).unwrap();
    let (ok, err) = run(&[input.to_str().unwrap(), &format!("{}/", root.join("od").display()), "--to", "csv"]);
    assert!(!ok, "{err}");
    assert!(!err.contains('\u{1b}') && !err.contains('\u{7}'), "{err:?}");
}

/// What is odd but still one file name keeps working, each under its own name.
#[test]
fn plain_names_still_name_their_files() {
    for name in ["cities", "two words", "a.b", "..hidden", "...", "Réunion", "-dash", "back\\\\slash"] {
        let root = tmp_dir();
        let input = root.join("in.geojson");
        std::fs::write(&input, geojson(name)).unwrap();
        let od = root.join("od/");
        let (ok, err) = run(&[input.to_str().unwrap(), &format!("{}/", od.display()), "--to", "csv"]);
        // On Windows a backslash separates, and is refused above with the rest.
        if name.contains('\\') && cfg!(windows) {
            assert!(!ok, "{err}");
            continue;
        }
        assert!(ok, "{name:?}: {err}");
        assert_eq!(files(&od), [format!("{}.csv", name.replace("\\\\", "\\"))], "{name:?}");
    }
}

/// A plain output path uses the path it was given, so the name never reaches
/// it and nothing is refused.
#[test]
fn a_plain_output_path_ignores_the_name() {
    let root = tmp_dir();
    let input = root.join("in.geojson");
    std::fs::write(&input, geojson("../escaped")).unwrap();
    let output = root.join("out.csv");
    let (ok, err) = run(&[input.to_str().unwrap(), output.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(output.exists());
}
