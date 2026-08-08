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
mod json;
mod parquet;
mod schema;
mod sqlite;

use error::{Error, Result};

fn main() {
    if let Err(e) = run() {
        eprintln!("panto: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = cli::parse(std::env::args())?;

    if args.from == args.to {
        return Err(Error::Usage(format!(
            "input and output are the same format ({:?}); nothing to convert",
            args.from
        )));
    }

    // Everything routes through the shared feature IR, so any input format
    // converts to any output format the writers support.
    let input = std::fs::read(&args.input)?;
    let output = convert::convert(args.from, args.to, &input)?;
    std::fs::write(&args.output, &output)?;
    eprintln!("wrote {} ({} bytes)", args.output, output.len());
    Ok(())
}
