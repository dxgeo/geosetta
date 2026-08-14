//! Command-line argument parsing (standard library only).
//!
//! Usage:
//! ```text
//! geosetta <input> <output> [--from FMT] [--to FMT]
//! ```
//! Formats are inferred from file extensions when not given explicitly. `-` as
//! `<input>`/`<output>` means stdin/stdout — see `main.rs`'s module doc
//! comment for why (piping an external tool, e.g. a reprojection step, in
//! between) — and requires the corresponding `--from`/`--to`, since there's no
//! extension to infer from. Not accepted for Shapefile, which is multi-file.

use crate::error::{Error, Result};
use crate::format::Format;

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
    /// Report each conversion stage (bytes read, features parsed, bytes
    /// written) to stderr, so long conversions visibly advance.
    pub progress: bool,
    /// Suppress CRS-loss warnings (on by default). They fire on stderr when the
    /// target format cannot record the source CRS (e.g. a non-WGS 84 dataset to
    /// GeoJSON/CSV/WKT); conversion still succeeds either way.
    pub quiet: bool,
}

/// One-line usage string.
pub const USAGE: &str = "usage: geosetta <input> <output> [--from FMT] [--to FMT] [--layer NAME] [--sort-hilbert] [--rtree] [--progress] [--quiet]\n  \"-\" for <input>/<output> means stdin/stdout (needs --from/--to; not for Shapefile)";

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
    let mut progress = false;
    let mut quiet = false;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(Error::Usage(USAGE.to_string())),
            "--sort-hilbert" => sort_hilbert = true,
            "--rtree" => rtree = true,
            "--progress" => progress = true,
            "--quiet" | "--no-warn" => quiet = true,
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
            // Bare "-" is the stdin/stdout placeholder, not an option flag —
            // it falls through to the positional bucket like any path.
            other if other.starts_with('-') && other != "-" => {
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

    let from = from.or_else(|| Format::from_path(&input)).ok_or_else(|| {
        if input == "-" {
            Error::Usage("--from is required when reading from stdin (\"-\")".into())
        } else {
            Error::Usage(format!("cannot infer input format from \"{input}\""))
        }
    })?;
    let to = to.or_else(|| Format::from_path(&output)).ok_or_else(|| {
        if output == "-" {
            Error::Usage("--to is required when writing to stdout (\"-\")".into())
        } else {
            Error::Usage(format!("cannot infer output format from \"{output}\""))
        }
    })?;

    Ok(Args {
        input,
        output,
        from,
        to,
        layer,
        sort_hilbert,
        rtree,
        progress,
        quiet,
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
        let a = args(&["geosetta", "in.geojson", "out.parquet"]).unwrap();
        assert_eq!(a.from, Format::GeoJson);
        assert_eq!(a.to, Format::Parquet);
        assert_eq!(a.input, "in.geojson");
    }

    #[test]
    fn explicit_flags_override() {
        let a = args(&["geosetta", "in.txt", "out.bin", "--from", "geojson", "--to", "parquet"])
            .unwrap();
        assert_eq!(a.from, Format::GeoJson);
        assert_eq!(a.to, Format::Parquet);
    }

    #[test]
    fn parses_rtree_flag() {
        let a = args(&["geosetta", "in.geojson", "out.gpkg", "--rtree"]).unwrap();
        assert!(a.rtree);
        let b = args(&["geosetta", "in.geojson", "out.gpkg"]).unwrap();
        assert!(!b.rtree);
    }

    #[test]
    fn parses_progress_flag() {
        let a = args(&["geosetta", "in.geojson", "out.parquet", "--progress"]).unwrap();
        assert!(a.progress);
        let b = args(&["geosetta", "in.geojson", "out.parquet"]).unwrap();
        assert!(!b.progress);
    }

    #[test]
    fn parses_quiet_flag() {
        // On by default warnings are suppressed only when asked; --no-warn is an
        // accepted alias.
        assert!(!args(&["geosetta", "in.gpkg", "out.geojson"]).unwrap().quiet);
        assert!(args(&["geosetta", "in.gpkg", "out.geojson", "--quiet"]).unwrap().quiet);
        assert!(args(&["geosetta", "in.gpkg", "out.geojson", "--no-warn"]).unwrap().quiet);
    }

    #[test]
    fn errors_on_bad_usage() {
        assert!(args(&["geosetta", "only-one.geojson"]).is_err());
        assert!(args(&["geosetta", "a.xyz", "b.parquet"]).is_err()); // unknown input ext
        assert!(args(&["geosetta", "--help"]).is_err());
    }

    #[test]
    fn bare_dash_is_a_positional_not_an_unknown_option() {
        // "-" starts with '-' like every flag, but it's the stdin/stdout
        // placeholder, not an option — it must reach the positional bucket
        // rather than tripping the "unknown option" branch.
        let a = args(&["geosetta", "-", "-", "--from", "geojson", "--to", "wkt"]).unwrap();
        assert_eq!(a.input, "-");
        assert_eq!(a.output, "-");
        assert_eq!(a.from, Format::GeoJson);
        assert_eq!(a.to, Format::Wkt);
    }

    #[test]
    fn dash_input_requires_explicit_from() {
        let err = args(&["geosetta", "-", "out.geojson"]).unwrap_err();
        assert!(matches!(err, Error::Usage(m) if m.contains("--from") && m.contains("stdin")));
        // Supplying --from clears it (--to is still inferred from the extension).
        let a = args(&["geosetta", "-", "out.geojson", "--from", "wkt"]).unwrap();
        assert_eq!(a.from, Format::Wkt);
        assert_eq!(a.to, Format::GeoJson);
    }

    #[test]
    fn dash_output_requires_explicit_to() {
        let err = args(&["geosetta", "in.geojson", "-"]).unwrap_err();
        assert!(matches!(err, Error::Usage(m) if m.contains("--to") && m.contains("stdout")));
        let a = args(&["geosetta", "in.geojson", "-", "--to", "wkt"]).unwrap();
        assert_eq!(a.to, Format::Wkt);
    }
}
