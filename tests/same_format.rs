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

// --- The D6 fixture registry ----------------------------------------------
//
// `plans/same-format-fidelity.org` D6: the new-spoke obligation is enforced by
// a catch-all-free `match` here rather than by a `Format::native_members()`
// constant, because a constant returning `&[]` is simultaneously the right
// answer for a spoke that discards nothing and the shape of having forgotten.
// A fixture cannot be empty and still pass a round-trip assertion.

/// One format's hostile fixture: the file set to hand geosetta, and why these
/// files are the ones.
struct Fixture {
    /// The file geosetta is pointed at, relative to the crate root.
    path: &'static str,
    /// Sibling files that must travel beside it, by extension. Empty for every
    /// format whose on-disk shape is one file.
    siblings: &'static [&'static str],
    /// What this fixture carries that a naive reader would drop. Recorded here
    /// because `plans/same-format-fidelity.org` § TESTING asks for it: a
    /// fixture whose purpose is undocumented gets simplified away later.
    carries: &'static str,
}

/// **The D6 enforcement.** One hostile fixture per format, no catch-all arm —
/// a tenth `Format` variant cannot compile this test suite until someone
/// supplies one, and cannot pass it without the spoke actually being faithful.
///
/// Every file here is documented in `tests/fixtures/HOSTILE.org`, and every one
/// was confirmed readable before being committed: a fixture the reader rejects
/// tests nothing.
fn hostile_fixture(format: Format) -> Fixture {
    match format {
        Format::GeoJson => Fixture {
            path: "tests/fixtures/hand_hostile.geojson",
            siblings: &[],
            carries: "foreign members at all three levels (collection, feature, geometry), \
                      a numeric and a string `id`, an explicit null, a nested and an empty \
                      object, and both JSON unicode forms",
        },
        Format::Parquet => Fixture {
            path: "tests/fixtures/gdal_hostile.parquet",
            siblings: &[],
            carries: "real ogr2ogr `key_value_metadata` beside `geo` (`ARROW:schema`, \
                      `gdal:metadata`) and the primary column's `geo.orientation`",
        },
        Format::FlatGeobuf => Fixture {
            path: "tests/fixtures/gdal_hostile.fgb",
            siblings: &[],
            carries: "header `title`/`description`/`metadata`, a `Crs` sub-table carrying \
                      full WKT2 and the name `WGS 84`, and per-column descriptive slots",
        },
        Format::Csv => Fixture {
            path: "tests/fixtures/hand_hostile.csv",
            siblings: &[],
            carries: "a cell of text per branch of D17's gate — leading zero, past-`i64`, \
                      trailing zero, letter case, padding, non-ASCII, embedded comma",
        },
        Format::Wkt => Fixture {
            path: "tests/fixtures/hand_hostile.wkt",
            siblings: &[],
            carries: "trailing zeros, a lowercase keyword, scientific notation, the \
                      parenthesised MULTIPOINT dialect, a ZM linestring and a ring",
        },
        Format::Gpkg => Fixture {
            path: "tests/fixtures/gdal_hostile.gpkg",
            siblings: &[],
            carries: "a layer's `gpkg_contents` title/description, `gpkg_data_columns` rows \
                      and the constraints they name — the container classes are \
                      `container_append.rs`'s and `gpkg_oracles.rs`'s",
        },
        Format::Shapefile => Fixture {
            path: "tests/fixtures/gdal_hostile.shp",
            siblings: &["shx", "dbf", "prj", "cpg"],
            carries: "declared `.dbf` widths an 80-char field cannot have got from its \
                      6-byte rows, a `Real (24.15)`, a boolean subtype, a `.prj` and a `.cpg`",
        },
        Format::Kml => Fixture {
            path: "tests/fixtures/hand_hostile.kml",
            siblings: &[],
            carries: "a `<Style>` and `styleUrl`, document name/description, `<Folder>` \
                      nesting, `<visibility>`/`<TimeStamp>`/`<altitudeMode>`, a `<Model>` \
                      placemark and a `<GroundOverlay>`",
        },
        Format::Kmz => Fixture {
            path: "tests/fixtures/hand_hostile.kmz",
            siblings: &[],
            carries: "the hostile KML as `doc.kml` plus `files/overlay.png` and \
                      `files/notes.txt` — the sidecars M3c owes",
        },
        // No catch-all arm. See this function's doc comment: a new spoke must
        // supply a fixture before this suite will build.
    }
}

/// One read/write/read cycle in the format's own on-disk shape, returning the
/// IR before and after so a caller can compare them.
///
/// **Also catch-all-free, and that is deliberate.** D6's obligation is not just
/// "supply a fixture" but "say how your spoke round-trips": the three arms here
/// are the three on-disk shapes this crate has — one buffer, a container of
/// layers, and a set of sibling files — and a fourth cannot be assumed.
fn round_trip(format: Format, fixture: &Fixture) -> (Vec<FeatureCollection>, Vec<FeatureCollection>) {
    let bytes = std::fs::read(fixture.path).unwrap();
    match format {
        Format::GeoJson
        | Format::Parquet
        | Format::FlatGeobuf
        | Format::Csv
        | Format::Wkt
        | Format::Kml
        | Format::Kmz => {
            let first = geosetta::read_features(format, &bytes)
                .unwrap_or_else(|e| panic!("{format:?}: reading {} failed: {e}", fixture.path));
            let written = geosetta::write_features(format, &first).unwrap();
            let again = geosetta::read_features(format, &written)
                .unwrap_or_else(|e| panic!("{format:?}: re-reading our own output failed: {e}"));
            (vec![first], vec![again])
        }
        // A GeoPackage is a container of layers, so the comparison is over the
        // layer *set*. Its container-scoped content is `container_append.rs`'s
        // and `gpkg_oracles.rs`' subject, not this test's — see § THE DOMAIN OF
        // THE RULE on why the two halves are tested apart.
        Format::Gpkg => {
            let carried = geosetta::geopackage::read_container(&bytes).unwrap();
            let first = geosetta::geopackage::read_layers(&bytes).unwrap();
            let written =
                geosetta::geopackage::write_layers(None, &first, &carried, false, false).unwrap();
            let again = geosetta::geopackage::read_layers(&written).unwrap();
            (
                first.into_iter().map(|(_, fc)| fc).collect(),
                again.into_iter().map(|(_, fc)| fc).collect(),
            )
        }
        // A Shapefile is one layer across several files, so the sibling bytes
        // have to be carried through the hop by hand — which is exactly why it
        // has its own entry points rather than `read_features`.
        Format::Shapefile => {
            let sib = |ext: &str| {
                std::fs::read(fixture.path.replace(".shp", &format!(".{ext}"))).ok()
            };
            // The registry names this spoke's file set, and this arm routes it
            // by role. Checked rather than assumed: a sibling added to the
            // registry that nothing here reads would otherwise be silently
            // left out of the round trip, which is the shape of omission D6
            // exists to catch one level up.
            for ext in fixture.siblings {
                assert!(
                    matches!(*ext, "shx" | "dbf" | "prj" | "cpg"),
                    ".{ext} is in the registry's file set but this arm does not route it",
                );
            }
            let dbf = sib("dbf").expect("a .shp fixture needs its .dbf");
            let prj = sib("prj").map(|b| String::from_utf8(b).unwrap());
            let cpg = sib("cpg").map(|b| String::from_utf8(b).unwrap());
            let first =
                geosetta::shapefile::read(&bytes, &dbf, prj.as_deref(), cpg.as_deref()).unwrap();
            let enc = geosetta::shapefile::write(&first).unwrap();
            // The writer declares its own encoding now, so the hop reads back
            // through `enc.cpg` rather than the source's. /Until 2026-10-21 it
            // emitted no `.cpg` at all and the re-read had to borrow the
            // fixture's — `native-inventory.org` § Shapefile's one-sided gap,
            // now closed./
            let again = geosetta::shapefile::read(
                &enc.shp,
                &enc.dbf,
                enc.prj.as_deref(),
                Some(enc.cpg),
            )
            .unwrap();
            (vec![first], vec![again])
        }
        // No catch-all arm. See this function's doc comment.
    }
}

/// **Every format puts back everything its reader recovered.** Read a hostile
/// fixture, write the same format, read again, and assert the IR is equal
/// *including its natives* — for all nine spokes, driven by the D6 registry.
///
/// `plans/same-format-fidelity.org` § TESTING's first bullet, and D6's real
/// subject. This is the test that would have caught most of § THE DEFECTS.
///
/// # What it proves, and the one thing it cannot
///
/// `FeatureCollection`'s `PartialEq` covers the geometry, the properties, the
/// CRS, the name, the column schema *and* the `Native` payload, so a member the
/// reader recovered and the writer dropped fails here. That is the half of the
/// invariant a spoke can get wrong on its own.
///
/// *It cannot see a member dropped at **read**.* A KML `<Style>` never reaches
/// the IR, so both sides agree about a file that lost it. That half is what
/// `Format::same_format_losses` declares and what the per-spoke tests pin.
/// **So this test is not a licence to stop reading the inventory** — it is the
/// guarantee that whatever a reader does recover survives a write, which is
/// why the registry records what each fixture carries.
///
/// /An earlier draft compared two generations of *output bytes* instead. It
/// passed on all nine immediately, and that was the tell: a writer that drops
/// a member uniformly is idempotent, so generation 2 agrees with generation 1
/// about the loss. Byte equality across a hop measures the writer against
/// itself; this measures it against the reader./
#[test]
fn every_format_puts_back_what_its_reader_recovered() {
    for format in Format::ALL {
        let fixture = hostile_fixture(format);
        let (first, again) = round_trip(format, &fixture);

        assert!(
            !first.is_empty() && first.iter().any(|fc| !fc.features.is_empty()),
            "{format:?}: {} read as nothing, so this assertion proves nothing",
            fixture.path,
        );
        assert_eq!(
            first.len(),
            again.len(),
            "{format:?}: the layer count changed across a same-format hop",
        );
        for (i, (before, after)) in first.iter().zip(&again).enumerate() {
            assert_eq!(
                before.features.len(),
                after.features.len(),
                "{format:?} layer {i}: the feature count changed. Carries: {}",
                fixture.carries,
            );
            assert_eq!(
                before.native, after.native,
                "{format:?} layer {i}: the natives did not survive the write. Carries: {}",
                fixture.carries,
            );
            assert_eq!(
                before.schema, after.schema,
                "{format:?} layer {i}: the column schema did not survive. Carries: {}",
                fixture.carries,
            );
            for (f, (b, a)) in before.features.iter().zip(&after.features).enumerate() {
                assert_eq!(
                    b.native, a.native,
                    "{format:?} layer {i} feature {f}: a feature native was dropped",
                );
                assert_eq!(
                    b.properties, a.properties,
                    "{format:?} layer {i} feature {f}: properties changed",
                );
                assert_eq!(
                    b.geometry, a.geometry,
                    "{format:?} layer {i} feature {f}: geometry changed",
                );
            }
            assert_eq!(
                before, after,
                "{format:?} layer {i}: the collection changed in a member not \
                 broken out above. Carries: {}",
                fixture.carries,
            );
        }
    }
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

/// Every cell of `hand_hostile.csv` comes back with its text intact, through
/// the real binary.
///
/// D17 (`plans/same-format-fidelity.org`), and this is the fixture the
/// inventory measured the loss on: before the reversibility gate this run
/// returned `7`, `8001`, `12345678901234567000`, `1.5`, `22.25` and `true`.
/// Asserted on the *bytes* rather than on a re-read collection, which is the
/// one place in this file that is right — the claim is about text, and a
/// re-read would compare the reader's own guess against itself.
#[test]
fn the_hostile_csv_keeps_every_cell_s_text() {
    let dir = tmp_dir();
    let dst = dir.join("out.csv");
    let (ok, _) = run(&["tests/fixtures/hand_hostile.csv", dst.to_str().unwrap()]);
    assert!(ok);
    let out = std::fs::read_to_string(&dst).unwrap();
    for cell in [
        "007",
        "08001",
        "12345678901234567890",
        "1.50",
        "22.250",
        "TRUE",
        "false",
        "  spaced  ",
        "København",
        "Zürich, CH",
    ] {
        assert!(out.contains(cell), "{cell:?} did not survive csv -> csv:\n{out}");
    }
}

/// And the warning now names only what is still lost, which is the half of
/// D17 that is a claim about the *message* rather than the output.
///
/// The two entries this row used to carry are gone; a test pins that, because
/// a warning that over-reports is the failure mode `same-format-output.org`
/// D3 was written against — a user who is told the text will not survive has
/// no reason to try.
#[test]
fn the_csv_warning_no_longer_claims_the_cell_text_is_lost() {
    let losses = Format::Csv.same_format_losses().join("; ");
    assert!(!losses.contains("exact text"), "{losses}");
    assert!(!losses.contains("beyond 64 bits"), "{losses}");
    assert!(losses.contains("coordinate spelling"), "{losses}");
    assert!(losses.contains("2^53"), "{losses}");
}

/// **A foreign sidecar at the output basename is reported, never removed, and
/// a `.cpg` is written so the one case we can fix is fixed.**
///
/// `plans/shapefile.org` and `plans/native-inventory.org` § Shapefile. The
/// measurements this pins, all taken 2026-10-21 before the change:
///
/// - a stale `.cpg` reading `ISO-8859-1` turned `København` into `KÃ¸benhavn`
///   for `ogrinfo` **and for geosetta's own reader** — correct bytes written,
///   corrupt text read back;
/// - a stale `.prj` made a CRS-less CSV's degree coordinates come back as
///   `NAD83 / UTM zone 18N` in metres, from both tools.
///
/// The `.cpg` case is fixed by writing one. The `.prj` case cannot be — OGC
/// WKT has no spelling for "unknown" — so it is warned about, which is the
/// whole answer there and deliberately not half of one.
#[test]
fn a_foreign_sidecar_is_warned_about_and_the_cpg_is_overwritten() {
    let dir = tmp_dir();
    let src = dir.join("uni.csv");
    std::fs::write(&src, "name,geometry\nKøbenhavn,\"POINT (12.5683 55.6761)\"\n").unwrap();

    let stem = dir.join("out");
    // A destination already carrying three files this run will not produce,
    // plus a `.cpg` it will.
    std::fs::write(dir.join("out.cpg"), "ISO-8859-1").unwrap();
    std::fs::write(dir.join("out.prj"), "GEOGCS[\"bogus\",UNIT[\"Degree\",0.0174532925199433]]")
        .unwrap();
    std::fs::write(dir.join("out.qix"), b"stale").unwrap();
    std::fs::write(dir.join("out.sbn"), b"stale").unwrap();

    let shp = format!("{}.shp", stem.display());
    let (ok, err) = run(&[src.to_str().unwrap(), &shp]);
    assert!(ok, "{err}");

    // Nothing was deleted: geosetta writes what it can and removes nothing.
    for ext in ["prj", "qix", "sbn"] {
        assert!(
            dir.join(format!("out.{ext}")).exists(),
            ".{ext} was removed, and this writer must not delete files it did not write",
        );
    }
    // Each survivor is named, with what it will be read as.
    for (ext, phrase) in
        [("prj", "read as this output's CRS"), ("qix", "quadtree"), ("sbn", "Esri spatial index")]
    {
        assert!(err.contains(&format!("out.{ext}")), ".{ext} went unreported: {err}");
        assert!(err.contains(phrase), "the .{ext} warning did not say what it means: {err}");
    }

    // The `.cpg` is ours now, and the text round-trips because of it.
    assert_eq!(std::fs::read_to_string(dir.join("out.cpg")).unwrap(), "UTF-8");
    let back = Command::new(GEOSETTA).args([&shp, "-", "--to", "csv"]).output().unwrap();
    assert!(back.status.success(), "{}", String::from_utf8_lossy(&back.stderr));
    let text = String::from_utf8_lossy(&back.stdout);
    assert!(text.contains("København"), "the .dbf text did not survive its own .cpg: {text}");
}

/// A clean destination gets no sidecar warnings at all, so the check above
/// cannot become background noise on an ordinary run.
///
/// Paired with it deliberately: a warning that fires every time is one users
/// learn to skip, and this one has to stay rare enough to read.
#[test]
fn an_ordinary_shapefile_write_reports_no_sidecars() {
    let dir = tmp_dir();
    let src = dir.join("in.csv");
    std::fs::write(&src, "a,geometry\n1,\"POINT (1 2)\"\n").unwrap();
    let shp = format!("{}.shp", dir.join("fresh").display());

    let (ok, err) = run(&[src.to_str().unwrap(), &shp]);
    assert!(ok, "{err}");
    assert!(!err.contains("already exists"), "a clean write must be quiet: {err}");

    // And re-running the identical conversion stays quiet, which is the case a
    // naive pre-write existence check gets wrong: every file it finds is one
    // it is about to replace with identical bytes.
    let (ok, err) = run(&[src.to_str().unwrap(), &shp]);
    assert!(ok, "{err}");
    assert!(!err.contains("already exists"), "re-running the same write must be quiet: {err}");
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
