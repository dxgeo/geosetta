//! Command-line argument parsing (standard library only).
//!
//! Usage:
//! ```text
//! geosetta <input> <output> [--from FMT] [--to FMT] [--crs PATH]
//! geosetta <input> --print-crs-code
//! geosetta <input> --print-crs [--escape]
//! ```
//! Formats are inferred from file extensions when not given explicitly. `-` as
//! `<input>`/`<output>` means stdin/stdout — see `main.rs`'s module doc
//! comment for why (piping an external tool, e.g. a reprojection step, in
//! between) — and requires the corresponding `--from`/`--to`, since there's no
//! extension to infer from. Not accepted for Shapefile, which is multi-file.
//!
//! `--crs` and the two `--print-crs*` flags are the halves of resolving a CRS
//! that geosetta itself cannot: the printing flags report what the source
//! carries so *some other tool* can resolve it, and `--crs` accepts the
//! definition that tool produced. Geosetta never runs that tool — it only reads
//! and writes text, so every external step is one the user typed
//! (`plans/crs-external-resolution.org`).
//!
//! The two printing flags answer the same question at different resolutions.
//! `--print-crs-code` reports the *identity* (`EPSG:7844`), which resolves
//! trivially but does not exist for an id-less definition. `--print-crs`
//! reports the *definition body* the source recorded, verbatim, which is what a
//! name-recovery tool needs precisely when there is no code to report
//! (`plans/crs-definition-output.org`).

use crate::error::{Error, Result};
use crate::format::Format;

/// What this invocation is asking for.
///
/// The `--print-crs*` flags are read-only diagnostics: they have an input but no
/// output at all. Keeping the output path and target format *inside*
/// [`Mode::Convert`], rather than as `Option` fields on [`Args`], is what stops
/// a diagnostic path from reaching for an output it was never given — the
/// compiler enforces that, so no converting code has to defensively unwrap.
#[derive(Debug)]
pub enum Mode {
    /// Convert `<input>` into `output`, written as `to` — the normal invocation.
    Convert { output: String, to: Format },
    /// Report the source's `AUTHORITY:CODE` CRS identity on stdout and exit.
    PrintCrsCode,
    /// Report the source's CRS *definition body* on stdout, verbatim, and exit.
    PrintCrs,
}

/// Parsed command-line arguments.
#[derive(Debug)]
pub struct Args {
    pub input: String,
    pub from: Format,
    /// What to do with `input` — convert it, or just report its CRS.
    pub mode: Mode,
    /// A specific GeoPackage layer to read (or the layer name when writing one).
    pub layer: Option<String>,
    /// Reorder features by Hilbert-curve locality before writing (clusters rows
    /// for GeoParquet; a no-op ordering-wise for already-sorted FlatGeobuf).
    pub sort_hilbert: bool,
    /// Write a spatial index into GeoPackage output (the opt-in GeoPackage
    /// RTree extension). Ignored for other output formats.
    pub rtree: bool,
    /// Cache each geometry's bounding box in the GeoPackage Binary header
    /// (GPB §2.1.3's optional envelope). Opt-in, matching `--rtree` and
    /// `--sort-hilbert`: it is a size/query optimization, not a correctness
    /// requirement, and off by default keeps output byte-stable. Ignored for
    /// other output formats.
    pub envelope: bool,
    /// Report each conversion stage (bytes read, features parsed, bytes
    /// written) to stderr, so long conversions visibly advance.
    pub progress: bool,
    /// Suppress CRS-loss warnings (on by default). They fire on stderr when the
    /// target format cannot record the source CRS (e.g. a non-WGS 84 dataset to
    /// GeoJSON/CSV/WKT); conversion still succeeds either way.
    pub quiet: bool,
    /// `--escape`: render control bytes visibly in `--print-crs`'s output
    /// instead of passing them through. Valid only with that flag, off by
    /// default, and *deliberately not round-trip-safe* — it exists for a human
    /// eyeballing a suspicious file at a terminal, not for a pipeline stage.
    pub escape: bool,
    /// `--crs`: path to a file holding a CRS definition as WKT or PROJJSON text
    /// (`-` for stdin), installed as the output CRS in place of whatever the
    /// source carried. Geosetta neither produces nor fetches this text — see
    /// the module doc comment.
    pub crs: Option<String>,
}

/// One-line usage string.
pub const USAGE: &str = "usage: geosetta <input> <output> [--from FMT] [--to FMT] [--layer NAME] [--crs PATH] [--sort-hilbert] [--rtree] [--envelope] [--progress] [--quiet]\n       geosetta <input> --print-crs-code [--from FMT] [--layer NAME]\n       geosetta <input> --print-crs [--escape] [--from FMT] [--layer NAME]\n  \"-\" for <input>/<output> means stdin/stdout (needs --from/--to; not for Shapefile)\n  --crs PATH reads WKT or PROJJSON text (\"-\" for stdin) and uses it as the output CRS\n  --print-crs-code prints the source's AUTHORITY:CODE; --print-crs prints the CRS\n  definition the source recorded, verbatim (PROJJSON when it carries both)\n  --escape renders control bytes visibly (cat -v style) for reading at a terminal;\n  it is not round-trip safe, so leave it off when piping";

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
    let mut envelope = false;
    let mut progress = false;
    let mut quiet = false;
    let mut crs: Option<String> = None;
    let mut print_crs_code = false;
    let mut print_crs = false;
    let mut escape = false;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(Error::Usage(USAGE.to_string())),
            "--sort-hilbert" => sort_hilbert = true,
            "--rtree" => rtree = true,
            "--envelope" => envelope = true,
            "--progress" => progress = true,
            "--quiet" | "--no-warn" => quiet = true,
            "--print-crs-code" => print_crs_code = true,
            "--print-crs" => print_crs = true,
            "--escape" => escape = true,
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

    // Which diagnostic was asked for, if any — the name, because every rule
    // below reports itself and a message naming the wrong flag is worse than no
    // message. The two are one question at two resolutions, both answered on
    // stdout, so asking for both has no defined answer to give.
    let diagnostic: Option<&str> = match (print_crs, print_crs_code) {
        (true, true) => {
            return Err(Error::Usage(
                "--print-crs prints the CRS definition the source recorded and \
                 --print-crs-code prints its AUTHORITY:CODE identity; ask for one or the \
                 other, not both"
                    .into(),
            ));
        }
        (true, false) => Some("--print-crs"),
        (false, true) => Some("--print-crs-code"),
        (false, false) => None,
    };

    // A diagnostic and `--crs` are opposites, not companions: one asks what CRS
    // the source declares, the other replaces it. Combining them would report a
    // CRS while overwriting it in the same breath.
    if let Some(flag) = diagnostic
        && crs.is_some()
    {
        return Err(Error::Usage(format!(
            "{flag} reports the source's own CRS; it cannot be combined with --crs, \
             which replaces the source's CRS"
        )));
    }

    // `--escape` only changes how `--print-crs` renders bytes it is printing, so
    // on any other invocation there is nothing for it to act on. Rejecting is
    // the crate's standing convention for an argument that cannot mean anything
    // — silently ignoring it would let a user believe output was protected when
    // it was never being printed in the first place.
    if escape && !print_crs {
        return Err(Error::Usage(
            "--escape renders control bytes visibly in the definition --print-crs writes; \
             it has no meaning without --print-crs"
                .into(),
        ));
    }

    // A diagnostic writes no file, so it takes an input and no output.
    let expected_positionals = if diagnostic.is_some() { 1 } else { 2 };
    if positional.len() != expected_positionals {
        return Err(Error::Usage(match diagnostic {
            Some(flag) => format!("{flag} takes an input and no output: geosetta <input> {flag}"),
            None => USAGE.to_string(),
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

    let mode = if print_crs {
        Mode::PrintCrs
    } else if print_crs_code {
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
        envelope,
        progress,
        quiet,
        escape,
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
    /// the `--print-crs*` diagnostics, which deliberately have no output at all.
    fn convert_parts(a: &Args) -> (&str, Format) {
        match &a.mode {
            Mode::Convert { output, to } => (output.as_str(), *to),
            Mode::PrintCrsCode => panic!("expected a conversion, got --print-crs-code"),
            Mode::PrintCrs => panic!("expected a conversion, got --print-crs"),
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
    fn parses_envelope_flag() {
        let a = args(&["geosetta", "in.geojson", "out.gpkg", "--envelope"]).unwrap();
        assert!(a.envelope);
        let b = args(&["geosetta", "in.geojson", "out.gpkg"]).unwrap();
        assert!(!b.envelope, "off by default — it is an opt-in optimization");
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
    fn print_crs_takes_an_input_and_no_output() {
        let a = args(&["geosetta", "in.parquet", "--print-crs"]).unwrap();
        assert_eq!(a.input, "in.parquet");
        assert_eq!(a.from, Format::Parquet);
        assert!(matches!(a.mode, Mode::PrintCrs));
        // A second positional is the convert form, not this one — and the error
        // names the flag that was actually passed, not its sibling.
        let err = args(&["geosetta", "in.parquet", "out.shp", "--print-crs"]).unwrap_err();
        assert!(
            matches!(&err, Error::Usage(m) if m.contains("no output")),
            "{err}"
        );
        assert!(
            matches!(&err, Error::Usage(m) if m.contains("--print-crs takes")),
            "{err}"
        );
    }

    #[test]
    fn print_crs_still_needs_a_knowable_input_format() {
        // It reads the source to find its CRS, so it needs to know how — same
        // rule as a conversion and as --print-crs-code, including for stdin.
        assert!(args(&["geosetta", "mystery.xyz", "--print-crs"]).is_err());
        let a = args(&["geosetta", "-", "--print-crs", "--from", "parquet"]).unwrap();
        assert_eq!(a.from, Format::Parquet);
    }

    #[test]
    fn print_crs_and_crs_override_are_mutually_exclusive() {
        // One reports the source's CRS, the other replaces it.
        let err = args(&["geosetta", "in.parquet", "--print-crs", "--crs", "x.wkt"]).unwrap_err();
        assert!(
            matches!(&err, Error::Usage(m) if m.contains("--crs")),
            "{err}"
        );
        assert!(
            matches!(&err, Error::Usage(m) if m.contains("--print-crs ")),
            "{err}"
        );
    }

    #[test]
    fn the_two_printing_flags_are_mutually_exclusive() {
        // Both write to stdout, so asking for both has no answer to give: the
        // code and the definition body are one question at two resolutions.
        let err = args(&["geosetta", "in.parquet", "--print-crs", "--print-crs-code"]).unwrap_err();
        assert!(
            matches!(&err, Error::Usage(m) if m.contains("one or the other")),
            "{err}"
        );
        // Order doesn't change the answer.
        assert!(args(&["geosetta", "in.parquet", "--print-crs-code", "--print-crs"]).is_err());
    }

    #[test]
    fn print_crs_accepts_the_flags_that_narrow_what_it_reads() {
        // --layer and --from select *what* to report on, which every mode needs;
        // they are not conversion options.
        let a = args(&["geosetta", "in.gpkg", "--print-crs", "--layer", "roads"]).unwrap();
        assert!(matches!(a.mode, Mode::PrintCrs));
        assert_eq!(a.layer.as_deref(), Some("roads"));
        assert!(a.crs.is_none());
    }

    #[test]
    fn escape_is_valid_only_alongside_print_crs() {
        // It renders bytes --print-crs is writing; with no such output there is
        // nothing for it to act on, and silently accepting it would suggest a
        // protection that was never applied.
        let a = args(&["geosetta", "in.parquet", "--print-crs", "--escape"]).unwrap();
        assert!(matches!(a.mode, Mode::PrintCrs));
        assert!(a.escape);

        for bad in [
            vec!["geosetta", "in.parquet", "out.shp", "--escape"],
            vec!["geosetta", "in.parquet", "--print-crs-code", "--escape"],
            vec!["geosetta", "in.parquet", "--escape"],
        ] {
            let err = args(&bad).unwrap_err();
            assert!(
                matches!(&err, Error::Usage(m) if m.contains("--escape")),
                "for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn escape_defaults_off() {
        // The verbatim contract is the default; protection is opt-in.
        assert!(
            !args(&["geosetta", "in.parquet", "--print-crs"])
                .unwrap()
                .escape
        );
        assert!(!args(&["geosetta", "in.parquet", "out.shp"]).unwrap().escape);
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
