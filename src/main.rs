//! Pantograph — convert between open vector geospatial formats.
//!
//! First conversion: GeoJSON → GeoParquet, implemented with the standard
//! library only (no third-party crates).

mod cli;
mod convert;
mod error;
mod geojson;
mod geometry;
mod json;
mod parquet;

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

    match (args.from, args.to) {
        (Format::GeoJson, Format::Parquet) => {
            let input = std::fs::read_to_string(&args.input)?;
            let bytes = convert::geojson_to_geoparquet(&input)?;
            std::fs::write(&args.output, &bytes)?;
            eprintln!("wrote {} ({} bytes)", args.output, bytes.len());
            Ok(())
        }
        (Format::Parquet, Format::GeoJson) => {
            let bytes = std::fs::read(&args.input)?;
            let text = convert::geoparquet_to_geojson(&bytes)?;
            std::fs::write(&args.output, text.as_bytes())?;
            eprintln!("wrote {} ({} bytes)", args.output, text.len());
            Ok(())
        }
        (from, to) if from == to => Err(Error::Usage(format!(
            "input and output are the same format ({from:?}); nothing to convert"
        ))),
        _ => Err(Error::Usage(
            "only geojson <-> parquet is supported so far".into(),
        )),
    }
}
