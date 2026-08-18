//! Command-line argument parsing (standard library only).
//!
//! Usage:
//! ```text
//! geosetta <input> <output> [--from FMT] [--to FMT] [--crs PATH]
//! geosetta <input> --print-crs-code
//! ```
//! Formats are inferred from file extensions when not given explicitly. `-` as
//! `<input>`/`<output>` means stdin/stdout — see `main.rs`'s module doc
//! comment for why (piping an external tool, e.g. a reprojection step, in
//! between) — and requires the corresponding `--from`/`--to`, since there's no
//! extension to infer from. Not accepted for Shapefile, which is multi-file.
//!
//! `--crs` and `--print-crs-code` are the two halves of resolving a CRS that
//! geosetta itself cannot: the second reports the source's authority code so
//! *some other tool* can resolve it, the first accepts the definition that tool
//! produced. Geosetta never runs that tool — it only reads and writes text, so
//! every external step is one the user typed (`plans/crs-external-resolution.org`).

use crate::error::{Error, Result};
use crate::format::Format;

/// What this invocation is asking for.
///
/// `--print-crs-code` is a read-only diagnostic: it has an input but no output
/// at all. Keeping the output path and target format *inside* [`Mode::Convert`],
/// rather than as `Option` fields on [`Args`], is what stops the diagnostic path
/// from reaching for an output it was never given — the compiler enforces that,
/// so no converting code has to defensively unwrap.
#[derive(Debug)]
pub enum Mode {
    /// Convert `<input>` into `output`, written as `to` — the normal invocation.
    Convert { output: String, to: Format },
    /// Report the source's `AUTHORITY:CODE` CRS identity on stdout and exit.
    PrintCrsCode,
}

/// Parsed command-line arguments.
#[derive(Debug)]
pub struct Args {
    pub input: String,
    pub from: Format,
    /// What to do with `input` — convert it, or just report its CRS code.
    pub mode: Mode,
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
    /// `--crs`: path to a file holding a CRS definition as WKT or PROJJSON text
    /// (`-` for stdin), installed as the output CRS in place of whatever the
    /// source carried. Geosetta neither produces nor fetches this text — see
    /// the module doc comment.
    pub crs: Option<String>,
}

/// One-line usage string.
pub const USAGE: &str = "usage: geosetta <input> <output> [--from FMT] [--to FMT] [--layer NAME] [--crs PATH] [--sort-hilbert] [--rtree] [--progress] [--quiet]\n       geosetta <input> --print-crs-code [--from FMT] [--layer NAME]\n  \"-\" for <input>/<output> means stdin/stdout (needs --from/--to; not for Shapefile)\n  --crs PATH reads WKT or PROJJSON text (\"-\" for stdin) and uses it as the output CRS";

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
    let mut crs: Option<String> = None;
    let mut print_crs_code = false;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(Error::Usage(USAGE.to_string())),
            "--sort-hilbert" => sort_hilbert = true,
            "--rtree" => rtree = true,
            "--progress" => progress = true,
            "--quiet" | "--no-warn" => quiet = true,
            "--print-crs-code" => print_crs_code = true,
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
            "--crs" => {
                crs = Some(
                    iter.next()
                        .ok_or_else(|| Error::Usage("--crs needs a value (a path, or \"-\" for stdin)".into()))?,
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

    // The two flags are opposites, not companions: one asks what identity the
    // source declares, the other replaces that identity. Combining them would
    // report a code while overwriting it in the same breath.
    if print_crs_code && crs.is_some() {
        return Err(Error::Usage(
            "--print-crs-code reports the source's own CRS code; it cannot be combined with \
             --crs, which replaces the source's CRS"
                .into(),
        ));
    }

    // The diagnostic mode writes nothing, so it takes an input and no output.
    let expected_positionals = if print_crs_code { 1 } else { 2 };
    if positional.len() != expected_positionals {
        return Err(Error::Usage(if print_crs_code {
            "--print-crs-code takes an input and no output: geosetta <input> --print-crs-code".into()
        } else {
            USAGE.to_string()
        }));
    }
    let input = positional[0].clone();

    // Both would consume the same stream, and picking one silently would make
    // whichever lost look like it had simply found nothing.
    if crs.as_deref() == Some("-") && input == "-" {
        return Err(Error::Usage(
            "--crs - and \"-\" as the input both read stdin; give --crs a file path instead \
             (or read the input from one)"
                .into(),
        ));
    }

    let from = from.or_else(|| Format::from_path(&input)).ok_or_else(|| {
        if input == "-" {
            Error::Usage("--from is required when reading from stdin (\"-\")".into())
        } else {
            Error::Usage(format!("cannot infer input format from \"{input}\""))
        }
    })?;

    let mode = if print_crs_code {
        Mode::PrintCrsCode
    } else {
        let output = positional[1].clone();
        let to = to.or_else(|| Format::from_path(&output)).ok_or_else(|| {
            if output == "-" {
                Error::Usage("--to is required when writing to stdout (\"-\")".into())
            } else {
                Error::Usage(format!("cannot infer output format from \"{output}\""))
            }
        })?;
        Mode::Convert { output, to }
    };

    Ok(Args {
        input,
        from,
        mode,
        layer,
        sort_hilbert,
        rtree,
        progress,
        quiet,
        crs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Result<Args> {
        parse(items.iter().map(|s| s.to_string()))
    }

    /// The output half of a conversion, for tests that assert on it. Panics on
    /// `--print-crs-code`, which deliberately has no output at all.
    fn convert_parts(a: &Args) -> (&str, Format) {
        match &a.mode {
            Mode::Convert { output, to } => (output.as_str(), *to),
            Mode::PrintCrsCode => panic!("expected a conversion, got --print-crs-code"),
        }
    }

    #[test]
    fn infers_formats_from_extensions() {
        let a = args(&["geosetta", "in.geojson", "out.parquet"]).unwrap();
        assert_eq!(a.from, Format::GeoJson);
        assert_eq!(convert_parts(&a).1, Format::Parquet);
        assert_eq!(a.input, "in.geojson");
    }

    #[test]
    fn explicit_flags_override() {
        let a = args(&["geosetta", "in.txt", "out.bin", "--from", "geojson", "--to", "parquet"])
            .unwrap();
        assert_eq!(a.from, Format::GeoJson);
        assert_eq!(convert_parts(&a).1, Format::Parquet);
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
        assert_eq!(convert_parts(&a), ("-", Format::Wkt));
        assert_eq!(a.from, Format::GeoJson);
    }

    #[test]
    fn dash_input_requires_explicit_from() {
        let err = args(&["geosetta", "-", "out.geojson"]).unwrap_err();
        assert!(matches!(err, Error::Usage(m) if m.contains("--from") && m.contains("stdin")));
        // Supplying --from clears it (--to is still inferred from the extension).
        let a = args(&["geosetta", "-", "out.geojson", "--from", "wkt"]).unwrap();
        assert_eq!(a.from, Format::Wkt);
        assert_eq!(convert_parts(&a).1, Format::GeoJson);
    }

    #[test]
    fn dash_output_requires_explicit_to() {
        let err = args(&["geosetta", "in.geojson", "-"]).unwrap_err();
        assert!(matches!(err, Error::Usage(m) if m.contains("--to") && m.contains("stdout")));
        let a = args(&["geosetta", "in.geojson", "-", "--to", "wkt"]).unwrap();
        assert_eq!(convert_parts(&a).1, Format::Wkt);
    }

    #[test]
    fn parses_crs_override_path() {
        let a = args(&["geosetta", "in.parquet", "out.shp", "--crs", "gda2020.wkt"]).unwrap();
        assert_eq!(a.crs.as_deref(), Some("gda2020.wkt"));
        assert!(args(&["geosetta", "in.parquet", "out.shp", "--crs"]).is_err());
        assert_eq!(args(&["geosetta", "in.parquet", "out.shp"]).unwrap().crs, None);
    }

    #[test]
    fn print_crs_code_takes_an_input_and_no_output() {
        let a = args(&["geosetta", "in.parquet", "--print-crs-code"]).unwrap();
        assert_eq!(a.input, "in.parquet");
        assert_eq!(a.from, Format::Parquet);
        assert!(matches!(a.mode, Mode::PrintCrsCode));
        // A second positional is the convert form, not this one.
        let err = args(&["geosetta", "in.parquet", "out.shp", "--print-crs-code"]).unwrap_err();
        assert!(matches!(err, Error::Usage(m) if m.contains("no output")));
    }

    #[test]
    fn print_crs_code_still_needs_a_knowable_input_format() {
        // It reads the source to find its CRS, so it needs to know how — same
        // rule as a conversion, including for stdin.
        assert!(args(&["geosetta", "mystery.xyz", "--print-crs-code"]).is_err());
        let a = args(&["geosetta", "-", "--print-crs-code", "--from", "parquet"]).unwrap();
        assert_eq!(a.from, Format::Parquet);
    }

    #[test]
    fn print_crs_code_and_crs_override_are_mutually_exclusive() {
        // One reports the source's identity, the other replaces it.
        let err = args(&["geosetta", "in.parquet", "--print-crs-code", "--crs", "x.wkt"])
            .unwrap_err();
        assert!(matches!(err, Error::Usage(m) if m.contains("--crs")));
    }

    #[test]
    fn crs_from_stdin_conflicts_with_input_from_stdin() {
        let err = args(&["geosetta", "-", "out.shp", "--from", "fgb", "--crs", "-"]).unwrap_err();
        assert!(matches!(err, Error::Usage(m) if m.contains("stdin")));
        // Either one alone is fine.
        assert!(args(&["geosetta", "-", "out.shp", "--from", "fgb", "--crs", "c.wkt"]).is_ok());
        assert!(args(&["geosetta", "in.fgb", "out.shp", "--crs", "-"]).is_ok());
    }
}
