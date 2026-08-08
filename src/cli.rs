//! Command-line argument parsing (standard library only).
//!
//! Usage:
//! ```text
//! panto <input> <output> [--from FMT] [--to FMT]
//! ```
//! Formats are inferred from file extensions when not given explicitly.

use crate::error::{Error, Result};

/// A supported vector format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    GeoJson,
    Parquet,
    FlatGeobuf,
    Csv,
    Wkt,
    Gpkg,
}

impl Format {
    /// Parse an explicit `--from`/`--to` value.
    fn parse(name: &str) -> Result<Format> {
        match name.to_ascii_lowercase().as_str() {
            "geojson" | "json" => Ok(Format::GeoJson),
            "parquet" | "geoparquet" => Ok(Format::Parquet),
            "flatgeobuf" | "fgb" => Ok(Format::FlatGeobuf),
            "csv" => Ok(Format::Csv),
            "wkt" => Ok(Format::Wkt),
            "gpkg" | "geopackage" => Ok(Format::Gpkg),
            other => Err(Error::Usage(format!("unknown format \"{other}\""))),
        }
    }

    /// Infer a format from a file path's extension.
    fn from_path(path: &str) -> Option<Format> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        match ext.as_str() {
            "geojson" | "json" => Some(Format::GeoJson),
            "parquet" => Some(Format::Parquet),
            "fgb" => Some(Format::FlatGeobuf),
            "csv" => Some(Format::Csv),
            "wkt" => Some(Format::Wkt),
            "gpkg" => Some(Format::Gpkg),
            _ => None,
        }
    }

    /// The canonical file extension, used to name fan-out outputs.
    pub fn extension(self) -> &'static str {
        match self {
            Format::GeoJson => "geojson",
            Format::Parquet => "parquet",
            Format::FlatGeobuf => "fgb",
            Format::Csv => "csv",
            Format::Wkt => "wkt",
            Format::Gpkg => "gpkg",
        }
    }
}

/// Parsed command-line arguments.
#[derive(Debug)]
pub struct Args {
    pub input: String,
    pub output: String,
    pub from: Format,
    pub to: Format,
    /// A specific GeoPackage layer to read (or the layer name when writing one).
    pub layer: Option<String>,
    /// Reorder features by Hilbert-curve locality before writing (clusters rows
    /// for GeoParquet; a no-op ordering-wise for already-sorted FlatGeobuf).
    pub sort_hilbert: bool,
    /// Write a spatial index into GeoPackage output (the opt-in GeoPackage
    /// RTree extension). Ignored for other output formats.
    pub rtree: bool,
}

/// One-line usage string.
pub const USAGE: &str = "usage: panto <input> <output> [--from FMT] [--to FMT] [--layer NAME] [--sort-hilbert] [--rtree]";

/// Parse arguments from an iterator (typically `std::env::args()`), whose
/// first item is the program name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Args> {
    let mut iter = args.into_iter();
    let _program = iter.next();

    let mut positional: Vec<String> = Vec::new();
    let mut from: Option<Format> = None;
    let mut to: Option<Format> = None;
    let mut layer: Option<String> = None;
    let mut sort_hilbert = false;
    let mut rtree = false;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(Error::Usage(USAGE.to_string())),
            "--sort-hilbert" => sort_hilbert = true,
            "--rtree" => rtree = true,
            "--from" => {
                let v = iter
                    .next()
                    .ok_or_else(|| Error::Usage("--from needs a value".into()))?;
                from = Some(Format::parse(&v)?);
            }
            "--to" => {
                let v = iter
                    .next()
                    .ok_or_else(|| Error::Usage("--to needs a value".into()))?;
                to = Some(Format::parse(&v)?);
            }
            "--layer" => {
                layer = Some(
                    iter.next()
                        .ok_or_else(|| Error::Usage("--layer needs a value".into()))?,
                );
            }
            other if other.starts_with('-') => {
                return Err(Error::Usage(format!("unknown option \"{other}\"")));
            }
            _ => positional.push(arg),
        }
    }

    if positional.len() != 2 {
        return Err(Error::Usage(USAGE.to_string()));
    }
    let input = positional[0].clone();
    let output = positional[1].clone();

    let from = from
        .or_else(|| Format::from_path(&input))
        .ok_or_else(|| Error::Usage(format!("cannot infer input format from \"{input}\"")))?;
    let to = to
        .or_else(|| Format::from_path(&output))
        .ok_or_else(|| Error::Usage(format!("cannot infer output format from \"{output}\"")))?;

    Ok(Args {
        input,
        output,
        from,
        to,
        layer,
        sort_hilbert,
        rtree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Result<Args> {
        parse(items.iter().map(|s| s.to_string()))
    }

    #[test]
    fn infers_formats_from_extensions() {
        let a = args(&["panto", "in.geojson", "out.parquet"]).unwrap();
        assert_eq!(a.from, Format::GeoJson);
        assert_eq!(a.to, Format::Parquet);
        assert_eq!(a.input, "in.geojson");
    }

    #[test]
    fn explicit_flags_override() {
        let a = args(&["panto", "in.txt", "out.bin", "--from", "geojson", "--to", "parquet"])
            .unwrap();
        assert_eq!(a.from, Format::GeoJson);
        assert_eq!(a.to, Format::Parquet);
    }

    #[test]
    fn parses_rtree_flag() {
        let a = args(&["panto", "in.geojson", "out.gpkg", "--rtree"]).unwrap();
        assert!(a.rtree);
        let b = args(&["panto", "in.geojson", "out.gpkg"]).unwrap();
        assert!(!b.rtree);
    }

    #[test]
    fn errors_on_bad_usage() {
        assert!(args(&["panto", "only-one.geojson"]).is_err());
        assert!(args(&["panto", "a.xyz", "b.parquet"]).is_err()); // unknown input ext
        assert!(args(&["panto", "--help"]).is_err());
    }
}
