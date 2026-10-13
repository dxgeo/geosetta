//! Appending into a GeoPackage that already holds container content.
//!
//! `plans/container-reconciliation.org` § 3. Until 2026-10-13 the destination's
//! own container was **destroyed** on every append — its layers survived,
//! because `write_layers` reads and re-emits them, and everything else did not
//! (defect 3, applied to the destination rather than to the source). The
//! asymmetry was reported rather than hidden, at the second
//! `container_drop_warnings` call, which is the warning these tests watch go
//! silent.
//!
//! These invoke the binary rather than the library because the reconciliation
//! is wired in `main.rs`: the library never sees a path, and "the output file
//! already exists" is a statement about a path.

use std::process::Command;

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

/// `sqlite_constraints.gpkg` — two feature layers and a container holding four
/// user tables, an explicit index, a trigger on a user's own table, and the
/// metadata pair. `tests/fixtures/HOSTILE.org` has the inventory.
const DESTINATION: &str = "tests/fixtures/sqlite_constraints.gpkg";
/// `sqlite_contents.gpkg` — a tile pyramid, an `attributes` dataset, and the
/// metadata pair. Shares `gdal_hostile.gpkg`'s `side_notes` with the above,
/// which is what makes it the collision case.
const COLLIDING: &str = "tests/fixtures/sqlite_contents.gpkg";

/// A temp directory private to the calling test, on `same_format.rs`'s
/// reasoning: libtest runs a thread per test and these write under fixed names.
fn tmp_dir() -> std::path::PathBuf {
    let label = std::thread::current().name().unwrap_or("main").replace("::", "-");
    let dir =
        std::env::temp_dir().join(format!("geosetta-append-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(GEOSETTA).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Every `sqlite_master` name in a GeoPackage on disk, via the carrying channel
/// — which is the right lens here: these tests are about what *survives being
/// carried*, and `reproduced_names` is the writer's own answer to that.
fn carried_names(path: &std::path::Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap();
    let mut names = geosetta::geopackage::read_container(&bytes).unwrap().reproduced_names();
    names.sort();
    names
}

/// ***The destination's container survives an ordinary append.***
///
/// A GeoJSON source carries no container at all, so this is the narrowest
/// possible statement of the change: nothing arrives to merge, and the question
/// is purely whether what was already there is kept. Before § 3 the answer was
/// no — six tables, an index and a trigger went, and the run said so.
#[test]
fn an_append_keeps_the_destinations_own_container() {
    let dir = tmp_dir();
    let out = dir.join("out.gpkg");
    std::fs::copy(DESTINATION, &out).unwrap();
    let before = carried_names(&out);
    // The premise: the destination carries content worth losing.
    assert!(
        before.iter().any(|n| n == "side_notes") && before.iter().any(|n| n == "gpkg_metadata"),
        "the fixture should carry a container: {before:?}"
    );

    let source = dir.join("towns.geojson");
    std::fs::write(
        &source,
        r#"{"type":"FeatureCollection","features":[
             {"type":"Feature","geometry":{"type":"Point","coordinates":[1,2]},
              "properties":{"name":"a"}}]}"#,
    )
    .unwrap();
    let (ok, stderr) = run(&[
        "--layer",
        "towns",
        source.to_str().unwrap(),
        out.to_str().unwrap(),
    ]);
    assert!(ok, "the append should succeed: {stderr}");

    assert_eq!(carried_names(&out), before, "the destination's container is unchanged");
    // And the warning that had reported the loss is silent, which is the half a
    // names-only check cannot see: a survey that still named surviving content
    // would be bug 13's mistake in a third table.
    assert!(
        !stderr.contains("is not carried"),
        "nothing is destroyed, so nothing may be reported as destroyed: {stderr}"
    );
}

/// ***The appended layer is there too***, which is the property the append
/// existed for and which the reconciliation must not cost. Kept separate from
/// the test above so a regression says which half broke.
#[test]
fn an_append_still_adds_its_layer() {
    let dir = tmp_dir();
    let out = dir.join("out.gpkg");
    std::fs::copy(DESTINATION, &out).unwrap();

    let source = dir.join("towns.geojson");
    std::fs::write(
        &source,
        r#"{"type":"FeatureCollection","features":[
             {"type":"Feature","geometry":{"type":"Point","coordinates":[1,2]},
              "properties":{"name":"a"}}]}"#,
    )
    .unwrap();
    let (ok, stderr) =
        run(&["--layer", "towns", source.to_str().unwrap(), out.to_str().unwrap()]);
    assert!(ok, "{stderr}");

    let bytes = std::fs::read(&out).unwrap();
    let mut layers: Vec<String> =
        geosetta::geopackage::read_layers(&bytes).unwrap().into_iter().map(|(n, _)| n).collect();
    layers.sort();
    assert_eq!(layers, ["cities", "second", "towns"], "the destination's layers plus the new one");
}

/// ***A collision refuses, and the destination is left exactly as it was.***
///
/// Both fixtures descend from `gdal_hostile.gpkg` and so both carry
/// `side_notes` — which is § THE QUESTION IS NARROWER AND HARDER THAN "WHICH
/// WINS" demonstrated without constructing anything: *any* two files with a
/// producer in common collide.
///
/// The second assertion is the one that matters operationally. A refusal that
/// had already truncated the output file would be worse than the silent loss it
/// replaced, because the destination is a file the user was accumulating into.
#[test]
fn a_colliding_append_refuses_without_touching_the_destination() {
    let dir = tmp_dir();
    let out = dir.join("out.gpkg");
    std::fs::copy(DESTINATION, &out).unwrap();
    let untouched = std::fs::read(&out).unwrap();

    let (ok, stderr) = run(&[COLLIDING, out.to_str().unwrap()]);
    assert!(!ok, "a same-named user table has no union: {stderr}");
    assert!(
        stderr.contains("side_notes") && stderr.contains("D5"),
        "the refusal must name the table and cite the decision: {stderr}"
    );
    assert_eq!(
        std::fs::read(&out).unwrap(),
        untouched,
        "a refused run must not have written anything"
    );
}
