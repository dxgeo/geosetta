//! A layout is a memory and time setting only (`plans/columnar.org` D-C14):
//! every fixture, converted to every format under every layout, writes the
//! same bytes and says the same things as the plain read.
//!
//! Until the readers build tables themselves (C2--C4), `Rows` and `Columns`
//! transpose what a reader built into a table and back, so this is C1's run of
//! `from_features` and `into_features` over real files. `bench/equiv.py` is
//! the wider check, once `--layout` reaches the binary in C2.

use std::path::Path;

use geosetta::pipeline::{self, ConvertOptions, Event, Output, Source};
use geosetta::{Format, Layout, ReadOptions};

/// Everything a conversion produced: its outputs, then its warnings, then
/// its error.
fn convert(from: Format, to: Format, input: &Path, options: &ConvertOptions) -> (Vec<Vec<u8>>, Vec<String>, String) {
    let mut warnings = Vec::new();
    let mut events = |e: Event<'_>| {
        if let Event::Warning(w) = e {
            warnings.push(w);
        }
    };
    let bytes = std::fs::read(input).unwrap();
    let sibling = |ext: &str| std::fs::read(input.with_extension(ext)).ok();
    let (dbf, prj, cpg) = (sibling("dbf"), sibling("prj"), sibling("cpg"));
    let text = |b: &Option<Vec<u8>>| b.as_ref().map(|b| String::from_utf8_lossy(b).into_owned());
    let (prj, cpg) = (text(&prj), text(&cpg));
    let source = match (from, &dbf) {
        (Format::Shapefile, Some(dbf)) => {
            Source::Shapefile { shp: &bytes, dbf, prj: prj.as_deref(), cpg: cpg.as_deref() }
        }
        _ => Source::Bytes(&bytes),
    };
    let mut outputs = Vec::new();
    let result = (|| {
        let prepared = pipeline::prepare(from, to, source, options.clone(), &mut events)?;
        if to == Format::Gpkg {
            outputs.push(prepared.into_geopackage(None, &mut events)?);
            return Ok(());
        }
        for i in 0..prepared.layers().len() {
            match prepared.encode(i, &mut events)? {
                Output::Bytes(b) => outputs.push(b),
                Output::Shapefile(e) => {
                    outputs.extend([e.shp, e.shx, e.dbf, e.prj.unwrap_or_default().into_bytes()]);
                }
            }
        }
        Ok::<(), geosetta::Error>(())
    })();
    (outputs, warnings, result.err().map(|e| e.to_string()).unwrap_or_default())
}

#[test]
fn every_fixture_writes_the_same_under_every_layout() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut inputs: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    inputs.sort();
    let mut checked = 0;
    for input in &inputs {
        let Some(from) = Format::from_path(input) else { continue };
        for to in Format::ALL {
            for sort_hilbert in [false, true] {
                let options = |layout| ConvertOptions {
                    read: ReadOptions { layout, ..Default::default() },
                    sort_hilbert,
                    ..ConvertOptions::default()
                };
                let plain = convert(from, to, input, &options(Layout::Auto));
                for layout in [Layout::Rows, Layout::Columns] {
                    let held = convert(from, to, input, &options(layout));
                    assert!(
                        held == plain,
                        "{} → {to:?} (sort {sort_hilbert}) differs under {layout:?}",
                        input.display()
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 500, "only {checked} conversions found to check");
}
