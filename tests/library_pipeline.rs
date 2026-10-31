//! **Does the library run the conversion the binary runs?**
//!
//! `plans/library-pipeline.org`: the binary is a wrapper on
//! `geosetta::pipeline`, so for the same input and options the pipeline's
//! warnings are exactly the message bodies the binary prints after
//! `warning: `, in the same order — on the paths that used to be `main.rs`'s
//! own: a GeoPackage output, a GeoPackage source, a `--crs` override across
//! layers, and Shapefile on either side. An integration test on purpose, as in
//! `library_warnings.rs` — it sees the API a dependent sees.

use std::path::{Path, PathBuf};
use std::process::Command;

use geosetta::pipeline::{self, ConvertOptions, Event, Output, Source};
use geosetta::{geopackage, Crs, Feature, FeatureCollection, Format, Geometry, KmlVersion, Position, WriteOptions};

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

/// A temp directory private to the calling test, as in `same_format.rs`.
fn tmp_dir() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let label = match std::thread::current().name() {
        Some(name) if name != "main" => name.replace("::", "-"),
        _ => format!("seq{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
    };
    let dir = std::env::temp_dir()
        .join(format!("geosetta-library-pipeline-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The message bodies the binary prints converting `input` to `output`.
fn cli_warnings(input: &Path, output: &Path, flags: &[&str]) -> Vec<String> {
    let out = Command::new(GEOSETTA).arg(input).arg(output).args(flags).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    stderr.lines().filter_map(|l| l.strip_prefix("warning: ")).map(String::from).collect()
}

/// Options naming the containers as the binary does: by their paths.
fn labelled(input: &Path, output: &Path) -> ConvertOptions {
    ConvertOptions {
        source_label: Some(input.display().to_string()),
        destination_label: Some(output.display().to_string()),
        default_layer_name: input.file_stem().map(|s| s.to_string_lossy().into_owned()),
        ..ConvertOptions::default()
    }
}

/// Run the pipeline into a GeoPackage, collecting its warnings.
fn library_gpkg(from: Format, input: &Path, options: ConvertOptions, existing: Option<&[u8]>) -> Vec<String> {
    let bytes = std::fs::read(input).unwrap();
    let mut warnings = Vec::new();
    let mut events = |e: Event<'_>| {
        if let Event::Warning(w) = e {
            warnings.push(w);
        }
    };
    let prepared = pipeline::prepare(from, Format::Gpkg, Source::Bytes(&bytes), options, &mut events).unwrap();
    prepared.into_geopackage(existing, &mut events).unwrap();
    warnings
}

fn point(x: f64, y: f64) -> Feature {
    Feature { geometry: Some(Geometry::Point(Position::new(x, y))), properties: Vec::new(), native: None }
}

/// One fixture per source format, chosen to make the most warnings. **An
/// exhaustive match on purpose:** a new format does not compile here until it
/// has a fixture, so the sweep below covers it from its first commit.
fn fixture(format: Format) -> &'static str {
    match format {
        Format::GeoJson => "tests/fixtures/hand_hostile.geojson",
        Format::Parquet => "tests/fixtures/gdal_hostile.parquet",
        Format::FlatGeobuf => "tests/fixtures/gdal_hostile.fgb",
        Format::Csv => "tests/fixtures/hand_hostile.csv",
        Format::Wkt => "tests/fixtures/hand_hostile.wkt",
        Format::Gpkg => "tests/fixtures/gdal_hostile.gpkg",
        Format::Shapefile => "tests/fixtures/gdal_hostile.shp",
        Format::Kml => "tests/fixtures/hand_hostile.kml",
        Format::Kmz => "tests/fixtures/hand_hostile.kmz",
    }
}

const UTM_33N: &str = r#"PROJCS["WGS 84 / UTM zone 33N",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],UNIT["metre",1],AUTHORITY["EPSG","32633"]]"#;

/// The option sets the sweep runs: none, every flag at once, and the one flag
/// that is only valid for some targets.
fn option_sets() -> Vec<ConvertOptions> {
    vec![
        ConvertOptions::default(),
        ConvertOptions {
            write: WriteOptions { compact: true, ..WriteOptions::default() },
            crs: Some(Crs::from_definition_text(UTM_33N).unwrap()),
            sort_hilbert: true,
            layer: Some("cities".into()),
            rtree: true,
            envelope: true,
            ..ConvertOptions::default()
        },
        ConvertOptions {
            write: WriteOptions { kml_version: Some(KmlVersion::V2_3), ..WriteOptions::default() },
            ..ConvertOptions::default()
        },
    ]
}

/// The flags that ask the binary for `options`. **Destructured without `..`
/// on purpose:** a new option does not compile here until it has a flag, or a
/// stated reason it has none, so the sweep covers it from its first commit.
fn flags(options: &ConvertOptions, dir: &Path) -> Vec<String> {
    let ConvertOptions {
        write: WriteOptions { kml_version, compact },
        crs,
        sort_hilbert,
        layer,
        rtree,
        envelope,
        // The binary derives these from its paths; `run_library` passes what
        // it would derive.
        default_layer_name: _,
        source_label: _,
        destination_label: _,
    } = options;
    let mut out = Vec::new();
    if let Some(v) = kml_version {
        out.extend(["--kml-version".into(), v.as_str().into()]);
    }
    if let Some(crs) = crs {
        let path = dir.join("crs.txt");
        std::fs::write(&path, crs.definition_body().unwrap()).unwrap();
        out.extend(["--crs".into(), path.display().to_string()]);
    }
    if let Some(layer) = layer {
        out.extend(["--layer".into(), layer.clone()]);
    }
    for (on, flag) in [(*compact, "--compact"), (*sort_hilbert, "--sort-hilbert"), (*rtree, "--rtree"), (*envelope, "--envelope")] {
        if on {
            out.push(flag.into());
        }
    }
    out
}

/// What a run said: its warnings in order, then whether it failed and with
/// what message.
type Said = (Vec<String>, Result<(), String>);

/// The binary converting `input` to `output`.
fn run_cli(input: &Path, output: &Path, flags: &[String]) -> Said {
    let out = Command::new(GEOSETTA).arg(input).arg(output).args(flags).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let warnings = stderr.lines().filter_map(|l| l.strip_prefix("warning: ")).map(String::from).collect();
    let result = if out.status.success() {
        Ok(())
    } else {
        let error = stderr.lines().filter_map(|l| l.strip_prefix("geosetta: ")).collect::<Vec<_>>().join("\n");
        assert!(!error.is_empty(), "a failed run says why: {stderr}");
        Err(error)
    };
    (warnings, result)
}

/// The pipeline doing what the binary does with the same input: every layer
/// encoded, or written into one GeoPackage.
fn run_library(from: Format, to: Format, input: &Path, output: &Path, options: &ConvertOptions) -> Said {
    let mut warnings = Vec::new();
    let mut events = |e: Event<'_>| {
        if let Event::Warning(w) = e {
            warnings.push(w);
        }
    };
    let options = ConvertOptions {
        source_label: Some(input.display().to_string()),
        destination_label: Some(output.display().to_string()),
        default_layer_name: input.file_stem().map(|s| s.to_string_lossy().into_owned()),
        ..options.clone()
    };
    let bytes = std::fs::read(input).unwrap();
    let (dbf, prj, cpg);
    let source = if from == Format::Shapefile {
        let sibling = |ext: &str| input.with_extension(ext);
        dbf = std::fs::read(sibling("dbf")).unwrap();
        prj = std::fs::read_to_string(sibling("prj")).ok();
        cpg = std::fs::read_to_string(sibling("cpg")).ok();
        Source::Shapefile { shp: &bytes, dbf: &dbf, prj: prj.as_deref(), cpg: cpg.as_deref() }
    } else {
        Source::Bytes(&bytes)
    };
    let result = (|| {
        let prepared = pipeline::prepare(from, to, source, options, &mut events)?;
        if to == Format::Gpkg {
            prepared.into_geopackage(None, &mut events)?;
        } else {
            for i in 0..prepared.layers().len() {
                prepared.encode(i, &mut events)?;
            }
        }
        Ok::<(), geosetta::Error>(())
    })();
    (warnings, result.map_err(|e| e.to_string()))
}

/// **Every source, every target, every option set: the library and the binary
/// say the same thing** — the same warnings in the same order, and the same
/// error when either fails. The fixture and flag tables above are exhaustive,
/// so a new format or option joins this sweep by construction rather than by
/// someone remembering to (`plans/library-pipeline.org` P6).
#[test]
fn every_conversion_says_the_same_through_both() {
    let root = tmp_dir();
    let mut disagreements = Vec::new();
    let mut seq = 0;
    for from in Format::ALL {
        for to in Format::ALL {
            for options in option_sets() {
                seq += 1;
                let dir = root.join(format!("case{seq}"));
                std::fs::create_dir_all(&dir).unwrap();
                let input = Path::new(fixture(from));
                // A multi-layer source fans out to a directory, the only
                // output the binary accepts for it.
                let output = if from == Format::Gpkg && to != Format::Gpkg {
                    dir.join("out/")
                } else {
                    dir.join(format!("out.{}", to.extension()))
                };
                let mut flags = flags(&options, &dir);
                flags.extend(["--to".into(), to.extension().into()]);
                let cli = run_cli(input, &output, &flags);
                let lib = run_library(from, to, input, &output, &options);
                if cli != lib {
                    disagreements.push(format!("{from:?} -> {to:?} with {options:?}\n  cli: {cli:?}\n  lib: {lib:?}"));
                }
            }
        }
    }
    assert!(disagreements.is_empty(), "{} disagreement(s):\n{}", disagreements.len(), disagreements.join("\n"));
}

/// An append surveys the destination too, which only the binary used to do.
#[test]
fn an_append_surveys_the_destination_through_both() {
    let dir = tmp_dir();
    let input = Path::new("tests/fixtures/hand_hostile.kml");
    let output = dir.join("out.gpkg");
    std::fs::copy("tests/fixtures/gdal_linestring_z.gpkg", &output).unwrap();
    let existing = std::fs::read(&output).unwrap();
    let lib = library_gpkg(Format::Kml, input, labelled(input, &output), Some(&existing));
    let cli = cli_warnings(input, &output, &[]);
    assert!(lib.iter().any(|w| w.contains(&output.display().to_string())), "{lib:?}");
    assert_eq!(lib, cli);
}

/// `--crs` across layers that disagreed is a relabel worth a warning, and a
/// library caller now gets it: it used to be `main.rs`'s private
/// `apply_crs_override`.
#[test]
fn a_crs_override_across_disagreeing_layers_warns_through_both() {
    let dir = tmp_dir();
    let mercator = Crs::from_authority_code(Some("EPSG".into()), Some("3857".into()), None, None);
    let layers = vec![
        ("a".to_string(), FeatureCollection { crs: Some(Crs::Wgs84), ..FeatureCollection::new(vec![point(1.0, 2.0)]) }),
        ("b".to_string(), FeatureCollection { crs: Some(mercator), ..FeatureCollection::new(vec![point(3.0, 4.0)]) }),
    ];
    let input = dir.join("two.gpkg");
    std::fs::write(&input, geopackage::write_layers(None, &layers, &geopackage::Carried::default(), false, false).unwrap())
        .unwrap();
    let wkt = r#"PROJCS["WGS 84 / UTM zone 33N",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],UNIT["metre",1],AUTHORITY["EPSG","32633"]]"#;
    let crs_file = dir.join("crs.wkt");
    std::fs::write(&crs_file, wkt).unwrap();

    let output = dir.join("out.gpkg");
    let _ = std::fs::remove_file(&output);
    let options = ConvertOptions { crs: Some(Crs::from_definition_text(wkt).unwrap()), ..labelled(&input, &output) };
    let lib = library_gpkg(Format::Gpkg, &input, options, None);
    let cli = cli_warnings(&input, &output, &["--crs", crs_file.to_str().unwrap()]);
    assert!(lib.iter().any(|w| w.starts_with("--crs relabels all 2 layers as EPSG:32633")), "{lib:?}");
    assert_eq!(lib, cli);
}

/// A Shapefile source through `Source::Shapefile`, and a Shapefile output
/// through `Output::Shapefile`, warn as the binary does.
#[test]
fn shapefile_on_either_side_warns_the_same_through_both() {
    let dir = tmp_dir();
    let input = Path::new("tests/fixtures/gdal_hostile.shp");
    let shp = std::fs::read(input).unwrap();
    let dbf = std::fs::read("tests/fixtures/gdal_hostile.dbf").unwrap();
    let prj = std::fs::read_to_string("tests/fixtures/gdal_hostile.prj").unwrap();
    let source = Source::Shapefile { shp: &shp, dbf: &dbf, prj: Some(&prj), cpg: None };

    let mut lib = Vec::new();
    let mut events = |e: Event<'_>| {
        if let Event::Warning(w) = e {
            lib.push(w);
        }
    };
    let prepared = pipeline::prepare(Format::Shapefile, Format::GeoJson, source, ConvertOptions::default(), &mut events)
        .unwrap();
    assert!(matches!(prepared.encode(0, &mut events).unwrap(), Output::Bytes(_)));
    let cli = cli_warnings(input, &dir.join("out.geojson"), &[]);
    assert!(!lib.is_empty());
    assert_eq!(lib, cli);

    // And out to a Shapefile: a property name too long for dBase is a `.dbf`
    // warning the encode finds, which the binary used to print from the field.
    let fc = FeatureCollection::new(vec![Feature {
        properties: vec![("a_name_longer_than_ten".into(), geosetta::JsonValue::String("x".into()))],
        ..point(1.0, 2.0)
    }]);
    let geojson = dir.join("long.geojson");
    std::fs::write(&geojson, geosetta::write_features(Format::GeoJson, &fc).unwrap()).unwrap();
    let bytes = std::fs::read(&geojson).unwrap();
    let mut lib = Vec::new();
    let mut events = |e: Event<'_>| {
        if let Event::Warning(w) = e {
            lib.push(w);
        }
    };
    let prepared =
        pipeline::prepare(Format::GeoJson, Format::Shapefile, Source::Bytes(&bytes), ConvertOptions::default(), &mut events)
            .unwrap();
    assert!(matches!(prepared.encode(0, &mut events).unwrap(), Output::Shapefile(_)));
    let cli = cli_warnings(&geojson, &dir.join("out.shp"), &[]);
    assert!(lib.iter().any(|w| w.contains("a_name_longer_than_ten")), "{lib:?}");
    assert_eq!(lib, cli);
}

/// Each encode is a usage error on the other's target, rather than a write
/// in the wrong shape.
#[test]
fn each_encode_refuses_the_other_targets_shape() {
    let bytes = geosetta::write_features(Format::GeoJson, &FeatureCollection::new(vec![point(1.0, 2.0)])).unwrap();
    let mut events = |_: Event<'_>| {};
    let to_fgb =
        pipeline::prepare(Format::GeoJson, Format::FlatGeobuf, Source::Bytes(&bytes), ConvertOptions::default(), &mut events)
            .unwrap();
    assert!(to_fgb.into_geopackage(None, &mut events).is_err());
    let to_gpkg =
        pipeline::prepare(Format::GeoJson, Format::Gpkg, Source::Bytes(&bytes), ConvertOptions::default(), &mut events)
            .unwrap();
    assert!(to_gpkg.encode(0, &mut events).is_err());
}

/// A mistyped layer is an error, not an empty conversion.
#[test]
fn an_unknown_layer_is_an_error() {
    let bytes = std::fs::read("tests/fixtures/gdal_hostile.gpkg").unwrap();
    let options = ConvertOptions { layer: Some("nope".into()), ..ConvertOptions::default() };
    let err = pipeline::prepare(Format::Gpkg, Format::GeoJson, Source::Bytes(&bytes), options, &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("no layer named \"nope\""), "{err}");
}
