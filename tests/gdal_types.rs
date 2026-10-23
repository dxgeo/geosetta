//! **The reference cross-check for declared column types.**
//!
//! `#[ignore]` by default, like `perf.rs`: these shell out to GDAL, which not
//! every machine has. Run them with
//!
//! ```text
//! cargo test --test gdal_types -- --ignored --nocapture
//! ```
//!
//! # Why this exists, and why it is differential
//!
//! The in-tree sweeps (`convert.rs`'s `every_declared_type_survives_or_is_admitted`
//! and the four inbound tables in each reader) assert that geosetta agrees with
//! *itself*. That is most of the value and it is not all of it: the `REAL` bug
//! fixed on 2026-08-21 was a case where geosetta was perfectly self-consistent
//! — it wrote `FLOAT` and read `FLOAT` back — and simply disagreed with the
//! rest of the world about what a spelling means. Only a second implementation
//! can catch that.
//!
//! So these do not hardcode GDAL's type vocabulary, which would just move the
//! guess. They compare **GDAL reading the input** against **GDAL reading our
//! output** and require the field types to be identical, except where
//! `Format::recast` has already declared a narrowing and the CLI has warned
//! about it. That is exactly the procedure that found both the `REAL` bug and
//! the unsigned narrowing, written down so it runs on demand instead of when
//! someone happens to think of it.

use std::process::Command;

/// Skip rather than fail when GDAL is absent: a machine without `ogrinfo` is
/// not a machine with a bug.
fn gdal_present() -> bool {
    Command::new("ogrinfo").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// The field-type lines `ogrinfo -so` reports for a layer, e.g.
/// `c_smallint: Integer(Int16) (0.0)`. Only the columns of the types fixture,
/// so geometry and fid lines cannot make a diff pass or fail spuriously.
fn field_types(path: &str, layer: &str) -> Vec<String> {
    let out = Command::new("ogrinfo")
        .args(["-so", path, layer])
        .output()
        .expect("ogrinfo failed to run");
    assert!(out.status.success(), "ogrinfo failed on {path}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("c_"))
        .map(|l| l.trim().to_string())
        .collect()
}

/// The feature *values* `ogrinfo -al` reports, one `key = value` line per
/// property. The companion to [`field_types`], and the reason it exists is
/// worth stating: for a day this file checked only the schema, and a
/// GeoPackage round trip that turned every blob into NULL passed it cleanly —
/// the `BLOB` declaration survived, so the types matched, and the data was
/// gone. **A type-fidelity test that never looks at a value can be perfectly
/// green while the values are being destroyed.**
fn field_values(path: &str, layer: &str) -> Vec<String> {
    let out = Command::new("ogrinfo")
        .args(["-al", "-q", path, layer])
        .output()
        .expect("ogrinfo failed to run");
    assert!(out.status.success(), "ogrinfo failed on {path}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.trim_start().starts_with("c_"))
        .map(|l| l.trim().to_string())
        .collect()
}

fn geosetta() -> &'static str {
    // The workspace target dir; `geosetta/target/` is a stale leftover.
    env!("CARGO_BIN_EXE_geosetta")
}

fn run(args: &[&str]) -> String {
    let out = Command::new(geosetta()).args(args).output().expect("geosetta failed to run");
    assert!(out.status.success(), "geosetta {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const FIXTURE: &str = "tests/fixtures/gdal_types.gpkg";

/// **Same-format: nothing may change.** Every GeoPackage declared type, out
/// through this crate's writer and back, with GDAL reporting the same field
/// type on both sides. This is the invariant `same-format-fidelity.org` states,
/// checked by something other than us.
#[test]
#[ignore = "requires GDAL; run with --ignored"]
fn gdal_sees_the_same_types_after_a_geopackage_round_trip() {
    if !gdal_present() {
        eprintln!("skipping: no ogrinfo on PATH");
        return;
    }
    let dir = std::env::temp_dir().join("geosetta_gdal_types");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("same.gpkg");
    let _ = std::fs::remove_file(&out);

    let stderr = run(&[FIXTURE, out.to_str().unwrap(), "--to", "gpkg"]);
    assert!(
        !stderr.contains("cannot hold the declared type"),
        "a same-format run must not recast anything, but warned: {stderr}"
    );
    assert_eq!(
        field_types(FIXTURE, "types"),
        field_types(out.to_str().unwrap(), "types"),
        "GDAL sees different column types after a gpkg -> gpkg run"
    );
    assert_eq!(
        field_values(FIXTURE, "types"),
        field_values(out.to_str().unwrap(), "types"),
        "GDAL sees different *values* after a gpkg -> gpkg run"
    );
}

/// **Cross-format: only the declared narrowings, and only with a warning.**
///
/// A transit through FlatGeobuf or GeoParquet and back may change a type only
/// where `Format::recast` says it must — FlatGeobuf has no `DATE` and neither
/// format has a byte-level cell — and the CLI must have said so. Any *other*
/// difference is a recast nobody declared, which is the whole bug class.
#[test]
#[ignore = "requires GDAL; run with --ignored"]
fn a_transit_recasts_only_what_it_warned_about() {
    if !gdal_present() {
        eprintln!("skipping: no ogrinfo on PATH");
        return;
    }
    let dir = std::env::temp_dir().join("geosetta_gdal_types");
    std::fs::create_dir_all(&dir).unwrap();

    // (transit format, the columns it is allowed to change, and why)
    let legs: &[(&str, &[&str])] = &[
        // FlatGeobuf has no DATE, only DATETIME — the same recast `ogr2ogr`
        // makes on this hop. `c_blob` used to be here too, until the IR gained
        // a byte-level value type (2026-08-21) and FlatGeobuf's own `BINARY`
        // column became reachable.
        ("fgb", &["c_date"]),
        // Nothing: GeoParquet names every declared type this fixture carries.
        ("parquet", &[]),
    ];
    let before = field_types(FIXTURE, "types");

    for (via, allowed) in legs {
        let mid = dir.join(format!("mid.{via}"));
        let back = dir.join(format!("back_{via}.gpkg"));
        let _ = std::fs::remove_file(&mid);
        let _ = std::fs::remove_file(&back);

        let stderr = run(&[FIXTURE, mid.to_str().unwrap(), "--to", via]);
        // `--layer types` because GeoParquet has no layer-name slot: after that
        // transit the collection has no name and the writer falls back to the
        // file stem. Naming it keeps this test about *types*.
        run(&[mid.to_str().unwrap(), back.to_str().unwrap(), "--to", "gpkg", "--layer", "types"]);
        let after = field_types(back.to_str().unwrap(), "types");

        assert_eq!(before.len(), after.len(), "a column went missing via {via}");
        // Values, for every column that was *not* recast. A declared recast
        // legitimately changes how GDAL renders a value — a `Date` written to
        // FlatGeobuf comes back declared `DATETIME`, so `2019/05/01` prints as
        // `2019/05/01 00:00:00` even though the stored text never changed. That
        // is what the warning is for. Every other column must be untouched, and
        // this is the assertion that would have caught blobs turning into NULL.
        let untouched = |lines: Vec<String>| -> Vec<String> {
            lines
                .into_iter()
                .filter(|l| {
                    let column = l.split_whitespace().next().unwrap_or("");
                    !allowed.contains(&column)
                })
                .collect()
        };
        assert_eq!(
            untouched(field_values(FIXTURE, "types")),
            untouched(field_values(back.to_str().unwrap(), "types")),
            "via {via}, GDAL sees different values in a column that was not recast"
        );
        for (b, a) in before.iter().zip(&after) {
            if b == a {
                continue;
            }
            let column = b.split(':').next().unwrap();
            assert!(
                allowed.contains(&column),
                "via {via}, GDAL sees {column} change from {b:?} to {a:?}, \
                 which Format::recast does not predict"
            );
            assert!(
                stderr.contains(column),
                "via {via}, {column} was recast without a warning naming it: {stderr}"
            );
        }
        // And every declared narrowing must actually happen — a warning about
        // a loss that did not occur is its own kind of wrong.
        for column in *allowed {
            let i = before.iter().position(|l| l.starts_with(&format!("{column}:"))).unwrap();
            assert_ne!(
                before[i], after[i],
                "via {via}, {column} was warned about but survived unchanged"
            );
        }
    }
}

/// **GDAL's spatial filter through an index packed over *unsorted* leaves
/// returns exactly the right features.**
///
/// This is the oracle the 2026-10-21 FlatGeobuf change rests on. That change
/// stopped the writer Hilbert-sorting features of its own accord — it was
/// permuting content on a same-format run, and making `--sort-hilbert` a dead
/// flag for the one format where the sort was mandatory. The claim it needs is
/// that a *packed Hilbert* R-tree is packed bottom-up over whatever order its
/// leaves are in, and that only the locality the name describes suffers, not
/// the correctness of a query.
///
/// Self-consistency cannot establish that: our own reader walks the same tree
/// our own writer packed, so the two would agree about a broken one. Hence
/// GDAL, and hence a *brute-force* expectation computed in this file rather
/// than read off either implementation.
///
/// The input is deliberately anti-sorted (descending by latitude), and 500
/// features is past `NODE_SIZE` twice over, so the tree has interior nodes
/// whose bounding boxes genuinely overlap — the case a sorted build is
/// supposed to avoid and the one a filter could get wrong.
#[test]
#[ignore = "requires GDAL; run with --ignored"]
fn gdal_filters_correctly_through_an_index_over_unsorted_leaves() {
    if !gdal_present() {
        eprintln!("skipping: ogrinfo not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("geosetta-fgb-index-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // A deterministic spread, then sorted *against* spatial locality.
    let mut points: Vec<(f64, f64)> = (0..500)
        .map(|i| {
            let x = f64::from((i * 149) % 359) - 179.0;
            let y = f64::from((i * 71) % 169) - 84.0;
            (x, y)
        })
        .collect();
    points.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let mut csv = String::from("id,geometry\n");
    for (i, (x, y)) in points.iter().enumerate() {
        csv.push_str(&format!("{i},\"POINT ({x} {y})\"\n"));
    }
    let src = dir.join("anti.csv");
    std::fs::write(&src, &csv).unwrap();

    let fgb = dir.join("anti.fgb");
    let out = Command::new(env!("CARGO_BIN_EXE_geosetta"))
        .args([src.to_str().unwrap(), fgb.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // The box, and the answer computed here rather than asked of anyone.
    let (x0, y0, x1, y1) = (-20.0, 10.0, 40.0, 50.0);
    let expected = points
        .iter()
        .filter(|(x, y)| *x >= x0 && *x <= x1 && *y >= y0 && *y <= y1)
        .count();
    assert!(expected > 0 && expected < points.len(), "the box must be a real filter");

    let count_with = |args: &[&str]| -> usize {
        let out = Command::new("ogrinfo")
            .args(["-al", "-q"])
            .args(args)
            .arg(fgb.to_str().unwrap())
            .output()
            .expect("ogrinfo failed to run");
        assert!(out.status.success(), "ogrinfo: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).matches("POINT").count()
    };

    assert_eq!(count_with(&[]), points.len(), "GDAL could not read the whole file");
    assert_eq!(
        count_with(&["-spat", "-20", "10", "40", "50"]),
        expected,
        "GDAL's filter through our index disagreed with a brute-force count",
    );
    // A box containing nothing must return nothing rather than everything,
    // which is how a tree walked wrongly tends to fail.
    assert_eq!(count_with(&["-spat", "200", "10", "201", "11"]), 0);
}
