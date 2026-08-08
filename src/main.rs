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
mod spatial;
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
        (_, Format::Gpkg) => return run_geopackage_write(&args),
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
    let output = if args.sort_hilbert {
        let mut fc = convert::read_features(args.from, &input)?;
        convert::reorder_hilbert(&mut fc);
        convert::write_features(args.to, &fc)?
    } else {
        convert::convert(args.from, args.to, &input)?
    };
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
    if args.sort_hilbert {
        for (_, fc) in &mut layers {
            convert::reorder_hilbert(fc);
        }
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

/// Write a layer into a GeoPackage, creating it or appending (upserting the
/// layer if it already exists). The layer name defaults to the input file stem.
fn run_geopackage_write(args: &cli::Args) -> Result<()> {
    let input = std::fs::read(&args.input)?;

    let mut new_layers = if args.from == Format::Gpkg {
        // gpkg -> gpkg: carry over all input layers (optionally one via --layer).
        let mut ls = geopackage::read_layers(&input)?;
        if let Some(name) = &args.layer {
            ls.retain(|(n, _)| n == name);
            if ls.is_empty() {
                return Err(Error::Usage(format!("no layer named \"{name}\"")));
            }
        }
        ls
    } else {
        let fc = convert::read_features(args.from, &input)?;
        let name = args
            .layer
            .clone()
            .unwrap_or_else(|| layer_stem(&args.input));
        vec![(name, fc)]
    };
    if args.sort_hilbert {
        for (_, fc) in &mut new_layers {
            convert::reorder_hilbert(fc);
        }
    }

    // Append into the existing GeoPackage if the output already exists.
    let existing = std::fs::read(&args.output).ok();
    let bytes = geopackage::write_layers(existing.as_deref(), &new_layers, args.rtree)?;
    std::fs::write(&args.output, &bytes)?;
    eprintln!("wrote {} ({} bytes)", args.output, bytes.len());
    Ok(())
}

/// The file stem of `path`, used as a default layer name.
fn layer_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "layer".into())
}
