//! Pantograph — convert between open vector geospatial formats.
//!
//! First conversion: GeoJSON → GeoParquet, implemented with the standard
//! library only (no third-party crates).

mod cli;
mod compress;
mod convert;
mod csv;
mod error;
mod feature;
mod flatbuffers;
mod flatgeobuf;
mod geojson;
mod geometry;
mod geopackage;
mod json;
mod parquet;
mod schema;
mod sqlite;

use cli::Format;
use error::{Error, Result};

fn main() {
    if let Err(e) = run() {
        eprintln!("panto: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = cli::parse(std::env::args())?;

    // GeoPackage is a multi-layer container, so it doesn't fit the plain
    // single-collection convert path.
    match (args.from, args.to) {
        (Format::Gpkg, Format::Gpkg) | (_, Format::Gpkg) => {
            return Err(Error::Usage("writing GeoPackage is not supported yet".into()));
        }
        (Format::Gpkg, _) => return run_geopackage_read(&args),
        _ => {}
    }

    if args.from == args.to {
        return Err(Error::Usage(format!(
            "input and output are the same format ({:?}); nothing to convert",
            args.from
        )));
    }

    // Everything else routes through the shared feature IR, so any input format
    // converts to any output format the writers support.
    let input = std::fs::read(&args.input)?;
    let output = convert::convert(args.from, args.to, &input)?;
    std::fs::write(&args.output, &output)?;
    eprintln!("wrote {} ({} bytes)", args.output, output.len());
    Ok(())
}

/// Read a GeoPackage (all layers) and fan out to the target format. Output
/// naming: a directory (existing or trailing `/`) gets one `layer.ext` file
/// each; a single layer goes to the plain output path; multiple layers to a
/// plain path is an error unless `--layer` selects one.
fn run_geopackage_read(args: &cli::Args) -> Result<()> {
    let input = std::fs::read(&args.input)?;
    let mut layers = geopackage::read_layers(&input)?;

    if let Some(name) = &args.layer {
        layers.retain(|(n, _)| n == name);
        if layers.is_empty() {
            return Err(Error::Usage(format!("no layer named \"{name}\"")));
        }
    }
    if layers.is_empty() {
        return Err(Error::Usage("GeoPackage has no feature layers".into()));
    }

    let as_dir = args.output.ends_with('/') || std::path::Path::new(&args.output).is_dir();
    if layers.len() == 1 && !as_dir {
        let (_, fc) = &layers[0];
        let bytes = convert::write_features(args.to, fc)?;
        std::fs::write(&args.output, &bytes)?;
        eprintln!("wrote {} ({} bytes)", args.output, bytes.len());
        return Ok(());
    }
    if !as_dir {
        return Err(Error::Usage(
            "GeoPackage has multiple layers: give an output directory (trailing '/') or --layer NAME".into(),
        ));
    }

    std::fs::create_dir_all(&args.output)?;
    let dir = std::path::Path::new(&args.output);
    for (name, fc) in &layers {
        let bytes = convert::write_features(args.to, fc)?;
        let path = dir.join(format!("{name}.{}", args.to.extension()));
        std::fs::write(&path, &bytes)?;
        eprintln!("wrote {} ({} bytes)", path.display(), bytes.len());
    }
    Ok(())
}
