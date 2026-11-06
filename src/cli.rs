//! Command-line argument parsing (standard library only).
//!
//! Usage:
//! ```text
//! geosetta <input> <output> [--from FMT] [--to FMT] [--layer NAME] [--crs PATH]
//!          [--sort-hilbert] [--rtree] [--envelope] [--kml-version 2.2|2.3]
//!          [--compact] [--layout auto|rows|columns] [--progress] [--quiet]
//! geosetta <input> --print-crs-code [--from FMT] [--layer NAME] [--layout L]
//! geosetta <input> --print-crs [--escape] [--from FMT] [--layer NAME] [--layout L]
//! geosetta --help | --version
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
//!
//! Paths are `OsString`s, since a file's name need not be UTF-8 on Unix; every
//! other value is text, and refuses what is not.

use std::ffi::{OsStr, OsString};

use geosetta::pipeline::ConvertOptions;
use geosetta::{Error, Format, KmlVersion, Layout, ReadOptions, Result, WriteOptions};

/// What the command line asks for. `--help` and `--version` need no input, which
/// every [`Args`] has.
#[derive(Debug)]
pub enum Invocation {
    Run(Args),
    Help,
    Version,
}

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
    Convert { output: OsString, to: Format },
    /// Report the source's `AUTHORITY:CODE` CRS identity on stdout and exit.
    PrintCrsCode,
    /// Report the source's CRS *definition body* on stdout, verbatim, and exit.
    PrintCrs,
}

/// Parsed command-line arguments.
#[derive(Debug)]
pub struct Args {
    pub input: OsString,
    pub from: Format,
    /// What to do with `input` — convert it, or just report its CRS.
    pub mode: Mode,
    /// The layer of a GeoPackage source to read, or the name to give a
    /// single-collection source. Refused where neither applies: see `main.rs`'s
    /// `refuse_layer_with_nowhere_to_go` and `parse`.
    pub layer: Option<String>,
    /// Reorder features by Hilbert-curve locality before writing, clustering
    /// rows for GeoParquet row groups and giving a FlatGeobuf index its
    /// locality.
    ///
    /// *This said it was "a no-op ordering-wise for already-sorted
    /// FlatGeobuf" until 2026-10-21, and had it backwards*: the FlatGeobuf
    /// writer sorted on every run of its own accord, so the flag was a no-op
    /// because the sort was *mandatory*, not because the input was already
    /// ordered — and a same-format run permuted the user's features either
    /// way. The writer now packs its index over input order, so this flag is
    /// the only thing that reorders anything, for every format alike.
    pub sort_hilbert: bool,
    /// Write a spatial index into GeoPackage output (the opt-in GeoPackage
    /// RTree extension). Refused for any other output.
    pub rtree: bool,
    /// Cache each geometry's bounding box in the GeoPackage Binary header
    /// (GPB §2.1.3's optional envelope). Opt-in, matching `--rtree` and
    /// `--sort-hilbert`: it is a size/query optimization, not a correctness
    /// requirement, and off by default keeps output byte-stable. Refused for any
    /// other output.
    pub envelope: bool,
    /// Report each conversion stage (bytes read, features parsed, bytes
    /// written) to stderr, so long conversions visibly advance.
    pub progress: bool,
    /// `--quiet`, or `--no-warn`: drop every `warning: ` line. Conversion
    /// succeeds or fails the same either way, and the `wrote` line stays.
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
    pub crs: Option<OsString>,
    /// `--kml-version`: the OGC KML version to write. Valid only when the
    /// output is KML or KMZ; without it the output is the source's own version
    /// after a KML read, and KML 2.2 otherwise (`plans/kml-standard.org` D-K2).
    pub kml_version: Option<KmlVersion>,
    /// `--compact`: write a GeoJSON or KML output on one line, to save space,
    /// rather than laid out (`plans/kml-layout.org` D-L8). Like `--rtree`, it
    /// is refused for an output with no layout.
    pub compact: bool,
    /// `--layout`: how the features are held between read and write
    /// (`plans/columnar.org` D-C14). A memory and time setting only: the
    /// output is the same under every layout.
    pub layout: Layout,
}

impl Args {
    /// What the read takes from the flags.
    pub fn read_options(&self) -> ReadOptions {
        ReadOptions { layout: self.layout, ..Default::default() }
    }

    /// What the write takes from the flags.
    pub fn write_options(&self) -> WriteOptions {
        WriteOptions { kml_version: self.kml_version, compact: self.compact }
    }
}

/// What `--help` prints, and a bad command line's reminder.
pub const USAGE: &str = "usage: geosetta <input> <output> [--from FMT] [--to FMT] [--layer NAME] [--crs PATH] [--sort-hilbert] [--rtree] [--envelope] [--kml-version 2.2|2.3] [--compact] [--layout auto|rows|columns] [--progress] [--quiet]
       geosetta <input> --print-crs-code [--from FMT] [--layer NAME] [--layout auto|rows|columns] [--progress] [--quiet]
       geosetta <input> --print-crs [--escape] [--from FMT] [--layer NAME] [--layout auto|rows|columns] [--progress] [--quiet]
       geosetta --help | --version
  \"-\" for <input>/<output> means stdin/stdout (needs --from/--to; not for Shapefile)
  \"--\" ends the flags, for a file whose name starts with \"-\"
  an <output> that is a directory, or ends in \"/\", gets one file per layer, named after it
  --layer NAME picks a GeoPackage source's layer, or names the output's where it can hold
  a name: GeoPackage, GeoJSON, FlatGeobuf, or a file in a directory output
  --crs PATH reads WKT or PROJJSON text (\"-\" for stdin) and uses it as the output CRS
  --sort-hilbert reorders the features by Hilbert-curve locality
  --rtree adds a spatial index to a GeoPackage output
  --envelope caches each geometry's bounding box in a GeoPackage output
  --print-crs-code prints the source's AUTHORITY:CODE; --print-crs prints the CRS
  definition the source recorded, verbatim (PROJJSON when it carries both)
  --escape renders control bytes visibly (cat -v style) for reading at a terminal;
  it is not round-trip safe, so leave it off when piping
  --compact writes GeoJSON and KML on one line, to save space, instead of indented
  --layout holds the features as rows or columns between read and write; it
  changes memory and time only, never the output
  --progress reports each stage on stderr
  --quiet, or --no-warn, drops the warning lines";

/// Every flag, which a flag's value may not be: `--layer --rtree` is a forgotten
/// value, far more often than a table named `--rtree`.
const FLAGS: &[&str] = &[
    "-h", "--help", "--version", "--", "--sort-hilbert", "--rtree", "--envelope", "--progress", "--quiet", "--no-warn",
    "--print-crs-code", "--print-crs", "--escape", "--compact", "--from", "--to", "--layer", "--kml-version", "--layout",
    "--crs",
];

/// The value after `flag`, once: a flag given twice has no answer to which it
/// meant. `given` is whether it already was, and `wanted` says what to pass.
fn value(args: &mut impl Iterator<Item = OsString>, flag: &str, given: bool, wanted: &str) -> Result<OsString> {
    if given {
        return Err(Error::Usage(format!("{flag} was given twice; give it once")));
    }
    let v = args.next().ok_or_else(|| Error::Usage(format!("{flag} needs a value{wanted}")))?;
    if let Some(other) = v.to_str().filter(|v| FLAGS.contains(v)) {
        return Err(Error::Usage(format!("{flag} needs a value{wanted}, and {other} is a flag")));
    }
    Ok(v)
}

/// [`value`], for a flag whose value is text and not a path.
fn text(args: &mut impl Iterator<Item = OsString>, flag: &str, given: bool, wanted: &str) -> Result<String> {
    value(args, flag, given, wanted)?
        .into_string()
        .map_err(|_| Error::Usage(format!("{flag} takes text, and this value is not UTF-8")))
}

/// Parse arguments from an iterator (typically `std::env::args_os()`), whose
/// first item is the program name.
pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Invocation> {
    let mut iter = args.into_iter();
    let _program = iter.next();

    let mut positional: Vec<OsString> = Vec::new();
    let mut from: Option<Format> = None;
    let mut to: Option<Format> = None;
    let mut layer: Option<String> = None;
    let mut sort_hilbert = false;
    let mut rtree = false;
    let mut envelope = false;
    let mut progress = false;
    let mut quiet = false;
    let mut crs: Option<OsString> = None;
    let mut print_crs_code = false;
    let mut print_crs = false;
    let mut escape = false;
    let mut kml_version: Option<KmlVersion> = None;
    let mut layout: Option<Layout> = None;
    let mut compact = false;

    while let Some(arg) = iter.next() {
        // Lossy only to compare: a flag is ASCII, so nothing a replacement
        // character is in can match one.
        match arg.to_string_lossy().as_ref() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "--version" => return Ok(Invocation::Version),
            "--" => {
                positional.extend(iter.by_ref());
            }
            "--sort-hilbert" => sort_hilbert = true,
            "--rtree" => rtree = true,
            "--envelope" => envelope = true,
            "--progress" => progress = true,
            "--quiet" | "--no-warn" => quiet = true,
            "--print-crs-code" => print_crs_code = true,
            "--print-crs" => print_crs = true,
            "--escape" => escape = true,
            "--compact" => compact = true,
            "--from" => from = Some(Format::parse(&text(&mut iter, "--from", from.is_some(), "")?)?),
            "--to" => to = Some(Format::parse(&text(&mut iter, "--to", to.is_some(), "")?)?),
            "--layer" => layer = Some(text(&mut iter, "--layer", layer.is_some(), "")?),
            "--kml-version" => {
                let v = text(&mut iter, "--kml-version", kml_version.is_some(), " (2.2 or 2.3)")?;
                kml_version = Some(KmlVersion::parse(&v)?);
            }
            "--layout" => {
                let v = text(&mut iter, "--layout", layout.is_some(), " (auto, rows or columns)")?;
                layout = Some(Layout::parse(&v)?);
            }
            "--crs" => crs = Some(value(&mut iter, "--crs", crs.is_some(), " (a path, or \"-\" for stdin)")?),
            // Bare "-" is the stdin/stdout placeholder, not an option flag —
            // it falls through to the positional bucket like any path.
            other if other.starts_with('-') && other != "-" => {
                return Err(Error::Usage(format!(
                    "unknown option \"{other}\" (a file named so goes after \"--\", or as ./{other})"
                )));
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

    // Each of these says something about the file a diagnostic does not write.
    // `--layer`, `--from`, `--layout`, `--quiet` and `--progress` are about the
    // read, which it does.
    if let Some(flag) = diagnostic {
        for (given, other) in [
            (to.is_some(), "--to"),
            (sort_hilbert, "--sort-hilbert"),
            (rtree, "--rtree"),
            (envelope, "--envelope"),
            (compact, "--compact"),
        ] {
            if given {
                return Err(Error::Usage(format!("{flag} writes no file, so {other} has no output to act on")));
            }
        }
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
    if crs.as_deref() == Some(OsStr::new("-")) && input == "-" {
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
            Error::Usage(format!("cannot infer input format from \"{}\"", input.to_string_lossy()))
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
                Error::Usage(format!("cannot infer output format from \"{}\"", output.to_string_lossy()))
            }
        })?;
        // Checked here, before anything is read, by the library's own rule.
        ConvertOptions {
            write: WriteOptions { kml_version, compact },
            layer: layer.clone(),
            rtree,
            envelope,
            ..ConvertOptions::default()
        }
        .check(to)?;
        Mode::Convert { output, to }
    };
    // A diagnostic reads one collection unless the source is a GeoPackage, so
    // there is nothing to pick. A conversion's `--layer` depends on whether the
    // output is a directory, which is `main.rs`'s to ask.
    if let (Some(flag), Some(_), false) = (diagnostic, &layer, from == Format::Gpkg) {
        return Err(Error::Usage(format!(
            "{flag} reads one collection from {}, so --layer has no layer to pick",
            from.display_name()
        )));
    }
    if let (Some(flag), Some(_)) = (diagnostic, kml_version) {
        return Err(Error::Usage(format!("{flag} writes no file, so --kml-version has no output to choose a version for")));
    }

    Ok(Invocation::Run(Args {
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
        kml_version,
        compact,
        layout: layout.unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(items: &[&str]) -> Result<Invocation> {
        parse(items.iter().map(OsString::from))
    }

    fn args(items: &[&str]) -> Result<Args> {
        match invocation(items)? {
            Invocation::Run(args) => Ok(args),
            other => panic!("expected a run, got {other:?}"),
        }
    }

    /// The refusal `items` gets, as its message.
    fn refusal(items: &[&str]) -> String {
        match args(items).unwrap_err() {
            Error::Usage(message) => message,
            other => panic!("not a usage error: {other}"),
        }
    }

    /// The output half of a conversion, for tests that assert on it. Panics on
    /// the `--print-crs*` diagnostics, which deliberately have no output at all.
    fn convert_parts(a: &Args) -> (&str, Format) {
        match &a.mode {
            Mode::Convert { output, to } => (output.to_str().unwrap(), *to),
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
    fn parses_kml_version_for_kml_and_kmz_output() {
        for out in ["out.kml", "out.kmz"] {
            let a = args(&["geosetta", "in.geojson", out, "--kml-version", "2.3"]).unwrap();
            assert_eq!(a.kml_version, Some(KmlVersion::V2_3));
        }
        assert_eq!(args(&["geosetta", "in.geojson", "out.kml"]).unwrap().kml_version, None);
    }

    #[test]
    fn kml_version_is_a_usage_error_for_any_other_output() {
        let err = args(&["geosetta", "in.kml", "out.geojson", "--kml-version", "2.2"]).unwrap_err().to_string();
        assert!(err.contains("GeoJSON"), "{err}");
        assert!(args(&["geosetta", "in.kml", "--print-crs", "--kml-version", "2.2"]).is_err());
    }

    #[test]
    fn kml_version_refuses_versions_it_cannot_write() {
        for v in ["2.1", "2.3.0", "3"] {
            assert!(args(&["geosetta", "in.geojson", "out.kml", "--kml-version", v]).is_err(), "{v}");
        }
        assert!(args(&["geosetta", "in.geojson", "out.kml", "--kml-version"]).is_err());
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
        // Warnings print unless asked otherwise; --no-warn is an accepted alias.
        assert!(!args(&["geosetta", "in.gpkg", "out.geojson"]).unwrap().quiet);
        assert!(args(&["geosetta", "in.gpkg", "out.geojson", "--quiet"]).unwrap().quiet);
        assert!(args(&["geosetta", "in.gpkg", "out.geojson", "--no-warn"]).unwrap().quiet);
    }

    #[test]
    fn parses_compact_flag() {
        assert!(args(&["geosetta", "in.gpkg", "out.geojson", "--compact"]).unwrap().write_options().compact);
        assert!(!args(&["geosetta", "in.gpkg", "out.geojson"]).unwrap().write_options().compact);
    }

    #[test]
    fn errors_on_bad_usage() {
        assert!(args(&["geosetta", "only-one.geojson"]).is_err());
        assert!(args(&["geosetta", "a.xyz", "b.parquet"]).is_err()); // unknown input ext
    }

    /// C6: asking for help is not a failure, wherever it is asked.
    #[test]
    fn help_and_version_need_no_input() {
        for help in ["-h", "--help"] {
            assert!(matches!(invocation(&["geosetta", help]), Ok(Invocation::Help)));
            assert!(matches!(invocation(&["geosetta", "in.gpkg", help]), Ok(Invocation::Help)));
        }
        assert!(matches!(invocation(&["geosetta", "--version"]), Ok(Invocation::Version)));
    }

    /// C2: a flag is not its neighbour's value; anything else with a dash can be.
    #[test]
    fn a_flag_is_not_the_value_before_it() {
        for flag in ["--layer", "--crs", "--from", "--to", "--layout", "--kml-version"] {
            for next in ["--rtree", "--quiet", "--help", "-h", "--"] {
                let message = refusal(&["geosetta", "in.gpkg", "out.gpkg", flag, next]);
                assert!(message.contains(flag) && message.contains(next), "{message}");
            }
        }
        let a = args(&["geosetta", "in.gpkg", "out.gpkg", "--layer", "-dash"]).unwrap();
        assert_eq!(a.layer.as_deref(), Some("-dash"));
        // Every flag `parse` matches is in the list, by trying each as a value.
        for flag in ["--version", "--sort-hilbert", "--envelope", "--progress", "--no-warn", "--print-crs-code", "--print-crs", "--escape", "--compact"] {
            assert!(refusal(&["geosetta", "in.gpkg", "out.gpkg", "--layer", flag]).contains("is a flag"), "{flag}");
        }
    }

    /// C4: nothing a diagnostic does not write can be asked of it.
    #[test]
    fn a_diagnostic_refuses_what_only_a_write_can_use() {
        for diagnostic in ["--print-crs", "--print-crs-code"] {
            for flag in [&["--to", "parquet"][..], &["--sort-hilbert"], &["--rtree"], &["--envelope"], &["--compact"]] {
                let mut items = vec!["geosetta", "in.gpkg", diagnostic];
                items.extend(flag);
                let message = refusal(&items);
                assert!(message.contains(flag[0]) && message.contains(diagnostic), "{message}");
            }
            for flag in [&["--layer", "a"][..], &["--from", "gpkg"], &["--layout", "rows"], &["--quiet"], &["--progress"]] {
                let mut items = vec!["geosetta", "in.gpkg", diagnostic];
                items.extend(flag);
                assert!(args(&items).is_ok(), "{items:?}");
            }
        }
    }

    /// C3: a diagnostic reads a single collection from anything but GeoPackage.
    #[test]
    fn a_diagnostic_needs_a_geopackage_for_layer() {
        let message = refusal(&["geosetta", "in.geojson", "--print-crs-code", "--layer", "nope"]);
        assert!(message.contains("--layer") && message.contains("GeoJSON"), "{message}");
    }

    /// C5: which of two values was meant has no answer.
    #[test]
    fn a_flag_with_a_value_is_given_once() {
        for items in [
            &["--to", "parquet", "--to", "wkt"][..],
            &["--from", "wkt", "--from", "geojson"],
            &["--layout", "rows", "--layout", "columns"],
            &["--layer", "a", "--layer", "b"],
            &["--crs", "a", "--crs", "b"],
            &["--kml-version", "2.2", "--kml-version", "2.3"],
        ] {
            let mut all = vec!["geosetta", "in.geojson", "out.kml"];
            all.extend(items);
            let message = refusal(&all);
            assert!(message.contains(items[0]) && message.contains("twice"), "{message}");
        }
        // A switch said twice is still only one thing.
        assert!(args(&["geosetta", "in.geojson", "out.gpkg", "--rtree", "--rtree"]).unwrap().rtree);
    }

    /// C7: only a file's own extension counts.
    #[test]
    fn a_bare_format_name_is_no_extension() {
        assert!(refusal(&["geosetta", "in.geojson", "wkt"]).contains("cannot infer output format"));
        assert!(refusal(&["geosetta", "geojson", "out.fgb"]).contains("cannot infer input format"));
        assert!(refusal(&["geosetta", ".geojson", "out.fgb"]).contains("cannot infer input format"));
        assert!(refusal(&["geosetta", "in.geojson", "out.fgb/"]).contains("cannot infer output format"));
        assert_eq!(convert_parts(&args(&["geosetta", "in.geojson", "dir.fgb/x.WKT"]).unwrap()).1, Format::Wkt);
    }

    /// `--rtree` and its like, by the library's own check.
    #[test]
    fn a_write_flag_needs_an_output_that_uses_it() {
        for (flag, works, not) in [("--rtree", "gpkg", "fgb"), ("--envelope", "gpkg", "parquet"), ("--compact", "kmz", "csv")] {
            assert!(args(&["geosetta", "in.geojson", &format!("out.{works}"), flag]).is_ok(), "{flag}");
            let message = refusal(&["geosetta", "in.geojson", &format!("out.{not}"), flag]);
            assert!(message.contains(flag), "{message}");
        }
    }

    #[test]
    fn an_empty_layer_is_refused() {
        assert!(refusal(&["geosetta", "in.geojson", "out.gpkg", "--layer", ""]).contains("--layer"));
    }

    /// `--` ends the flags, and an unknown one says it exists.
    #[test]
    fn a_double_dash_ends_the_flags() {
        let a = args(&["geosetta", "--to", "wkt", "--", "-in.geojson", "--out"]).unwrap();
        assert_eq!(a.input, "-in.geojson");
        assert_eq!(convert_parts(&a), ("--out", Format::Wkt));
        let message = refusal(&["geosetta", "-in.geojson", "out.wkt"]);
        assert!(message.contains("\"--\"") && message.contains("./-in.geojson"), "{message}");
    }

    /// C1: a path need not be text, and a value that is not one must be.
    #[cfg(unix)]
    #[test]
    fn a_path_need_not_be_utf8_and_a_value_must_be() {
        use std::os::unix::ffi::OsStringExt;
        let odd = |name: &[u8]| OsString::from_vec(name.to_vec());
        let run = |items: Vec<OsString>| match parse(items) {
            Ok(Invocation::Run(args)) => Ok(args),
            Ok(other) => panic!("expected a run, got {other:?}"),
            Err(e) => Err(e.to_string()),
        };
        let word = OsString::from;

        let input = odd(b"\xff\xfe.geojson");
        let output = odd(b"\xff.parquet");
        let a = run(vec![word("geosetta"), input.clone(), output.clone(), word("--crs"), odd(b"\xfe.wkt")]).unwrap();
        assert_eq!(a.input, input);
        assert_eq!(a.from, Format::GeoJson);
        assert!(matches!(&a.mode, Mode::Convert { output: o, to: Format::Parquet } if *o == output));

        for flag in ["--layer", "--from", "--to", "--layout", "--kml-version"] {
            let message = run(vec![word("geosetta"), word("in.gpkg"), word("out.gpkg"), word(flag), odd(b"\xff")]).unwrap_err();
            assert!(message.contains(flag) && message.contains("UTF-8"), "{message}");
        }
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
        assert_eq!(a.crs.as_deref(), Some(OsStr::new("gda2020.wkt")));
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
