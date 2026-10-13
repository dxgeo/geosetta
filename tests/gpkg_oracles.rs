//! **`sqlite3`'s own verdict on every GeoPackage this crate writes.**
//!
//! `#[ignore]` by default, like `gdal_types.rs`: it shells out to `sqlite3`,
//! which not every machine has. Run it with
//!
//! ```text
//! cargo test --test gpkg_oracles -- --ignored --nocapture
//! ```
//!
//! # Why this exists
//!
//! Because the oracle that was not run is the one that caught us.
//!
//! The tile-carrying work shipped on 2026-08-24 with 562 passing tests, clean
//! clippy, green `cargo doc`, and `PRAGMA integrity_check` reported clean on
//! the carried output — *of one fixture*. A second fixture in the same tree
//! came out as a database `sqlite3` refuses to open at all: `malformed
//! database schema (sqlite_autoindex_imagery_1) - orphan index`. That is
//! `handoff.org` § BUGS FOUND entry 20, and it was found by running the oracle
//! across every fixture while measuring something else.
//!
//! Running an oracle on one file is a statement about that file. This is the
//! loop, so the statement is about the corpus and nobody has to remember to
//! make it.
//!
//! # Why it is `sqlite3` rather than this crate's own reader
//!
//! `crate::sqlite` wrote these bytes, so asking it whether they are valid asks
//! the same opinion twice. `integrity_check` cross-examines the b-trees, the
//! index entries and the schema against a second implementation — and the
//! orphan-index case above is one this crate's reader is perfectly happy with,
//! because it never consults a DDL to ask whether an index is justified.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Skip rather than fail when `sqlite3` is absent: a machine without it is not
/// a machine with a bug.
fn sqlite3_present() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// What `sqlite3` says when asked `pragma` of `path`.
///
/// A file it cannot open fails at *prepare*, before any pragma runs, so the
/// error text goes to stderr and the exit status is non-zero — which is
/// precisely the orphan-index shape. Both streams are returned so that case
/// reads as a failure rather than as an empty answer.
fn pragma(path: &Path, pragma: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(path)
        .arg(format!("PRAGMA {pragma};"))
        .output()
        .expect("sqlite3 failed to run");
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() { stdout } else { format!("{stdout}{stderr}") }
}

/// Every `.gpkg` under `tests/fixtures`, so a fixture added later is covered
/// without anyone editing this file.
fn gpkg_fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "gpkg"))
        .collect();
    out.sort();
    assert!(out.len() >= 8, "the fixture sweep found almost nothing: {out:?}");
    out
}


/// Fixtures whose `gpkg -> gpkg` write is **refused by design**, so these
/// oracles have no output to check.
///
/// /Added 2026-10-13 with `sqlite_nullkey.gpkg`, and the list is asserted
/// rather than merely subtracted — see [`write_or_designed_refusal`]. A
/// fixture quietly skipped is a fixture that stops testing anything, which is
/// the failure mode `gpkg_fixtures`'s automatic sweep exists to prevent./
const REFUSED_BY_DESIGN: [&str; 1] = ["sqlite_nullkey.gpkg"];

/// A fixture's `gpkg -> gpkg` output, or `None` where the write is refused on
/// purpose.
///
/// ***The exclusion is a third assertion, not a hole.*** A fixture named in
/// [`REFUSED_BY_DESIGN`] must actually be refused: if it starts writing
/// successfully the list has rotted and the test says so, rather than skipping
/// a file that now has an output worth checking. And a fixture *not* named
/// there must write, which is what it always had to do.
fn write_or_designed_refusal(name: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    let carried = geosetta::geopackage::read_container(bytes).expect("read container");
    let layers = geosetta::geopackage::read_layers(bytes).expect("read layers");
    let written = geosetta::geopackage::write_layers(None, &layers, &carried, false, false);
    match (written, REFUSED_BY_DESIGN.contains(&name)) {
        (Ok(bytes), false) => Some(bytes),
        (Err(_), true) => None,
        (Ok(_), true) => {
            panic!("{name} is listed as refused by design and now writes; drop it from the list")
        }
        (Err(e), false) => panic!("{name}: write: {e:?}"),
    }
}

/// ***Every fixture's `gpkg -> gpkg` output satisfies `sqlite3`'s own two
/// structural oracles.***
///
/// `integrity_check` validates the b-trees, the index entries against their
/// tables, and the schema; `foreign_key_check` catches a carried reference
/// into a numbering the write did not reproduce — which is how M3b
/// requirement 1's absence would show up.
///
/// Failures are collected rather than asserted one at a time, so one bad
/// fixture does not hide the others.
#[test]
#[ignore = "needs the sqlite3 binary; run with --ignored"]
fn every_fixtures_geopackage_output_satisfies_sqlite3() {
    if !sqlite3_present() {
        eprintln!("skipping: no sqlite3 on PATH");
        return;
    }
    let dir = tempdir();
    let mut failures = Vec::new();
    for fixture in gpkg_fixtures() {
        let name = fixture.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(&fixture).expect("fixture");
        let Some(written) = write_or_designed_refusal(&name, &bytes) else { continue };

        let out = dir.join(&name);
        std::fs::write(&out, &written).expect("write output");

        let integrity = pragma(&out, "integrity_check");
        if integrity != "ok" {
            failures.push(format!("{name}: integrity_check said {integrity:?}"));
        }
        let fk = pragma(&out, "foreign_key_check");
        if !fk.is_empty() {
            failures.push(format!("{name}: foreign_key_check said {fk:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// ***Every table in every fixture's output can still have an `INSERT`
/// compiled against it.***
///
/// The third oracle, added 2026-08-25, and it exists because the other two
/// cannot express the question. A trigger whose body names a table the output
/// does not contain is created without complaint, `integrity_check` answers
/// `ok` and `foreign_key_check` says nothing — and then every `INSERT` on the
/// table it guards fails with `no such table`, storing no row. **A file that
/// reads perfectly, satisfies both structural pragmas, and cannot be written
/// to.**
///
/// That is reachable from ordinary GDAL output: `ogr2ogr` attaches two
/// `gpkg_ogr_contents` maintenance triggers to every table it registers,
/// including an aspatial one, and `gpkg_ogr_contents` is the one table this
/// writer deliberately does not emit. `carry::triggers_of` drops them; this is
/// the oracle that says so without knowing anything about that rule.
///
/// # Why `EXPLAIN`, and why `DEFAULT VALUES`
///
/// SQLite resolves a trigger's body at **prepare** time, so the failure
/// happens while compiling the statement — `EXPLAIN` gets that far and then
/// stops, which means the check runs against the real file and mutates
/// nothing. `DEFAULT VALUES` needs no column list, so one statement works for
/// any table whatever its shape; a `NOT NULL` it would violate is a *run*-time
/// constraint and never reached.
#[test]
#[ignore = "needs the sqlite3 binary; run with --ignored"]
fn every_fixtures_geopackage_output_can_be_inserted_into() {
    if !sqlite3_present() {
        eprintln!("skipping: no sqlite3 on PATH");
        return;
    }
    let dir = tempdir();
    let mut failures = Vec::new();
    for fixture in gpkg_fixtures() {
        let name = fixture.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(&fixture).expect("fixture");
        let Some(written) = write_or_designed_refusal(&name, &bytes) else { continue };

        let out = dir.join(&name);
        std::fs::write(&out, &written).expect("write output");

        for table in user_tables(&out) {
            let explained = run_sql(&out, &format!("EXPLAIN INSERT INTO \"{table}\" DEFAULT VALUES;"));
            if explained.contains("no such table") {
                failures.push(format!("{name}: INSERT INTO {table} will not compile: {explained}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every ordinary table in `path`, skipping SQLite's own bookkeeping.
fn user_tables(path: &Path) -> Vec<String> {
    run_sql(path, "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%';")
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `sqlite3 <path> <sql>`, with stderr folded in so a prepare failure is
/// visible rather than looking like an empty result.
fn run_sql(path: &Path, sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(path)
        .arg(sql)
        .output()
        .expect("sqlite3 failed to run");
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() { stdout } else { format!("{stdout}{stderr}") }
}

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("geosetta-oracles-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

