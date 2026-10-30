//! Geosetta — the `geosetta` command-line front end.
//!
//! All conversion logic lives in the library crate ([`geosetta`]); this
//! binary is a thin CLI wrapper that parses arguments, reads/writes files, and
//! reports `--progress`.
//!
//! `-` as `<input>` or `<output>` means stdin/stdout (the standard Unix
//! convention), so any external tool composes with geosetta through an
//! ordinary pipe — including a reprojection step, since geosetta itself never
//! reprojects (see [`geosetta::crs`]):
//! ```text
//! reproject-tool --to EPSG:3857 < in.geojson | geosetta --from geojson --to fgb - out.fgb
//! ```
//! The same principle runs through `--crs` / `--print-crs-code`: geosetta
//! reports the CRS code it cannot resolve and accepts the definition someone
//! else resolved, but never runs that someone else itself, so every external
//! step is visible in the command the user typed (see
//! `plans/crs-external-resolution.org`).
//!
//! Shapefile is excluded: it's sibling files (`.shp`/`.shx`/`.dbf`/`.prj`),
//! not a single byte stream, so there's nothing to pipe. Route it through a
//! single-buffer format instead (FlatGeobuf and GeoParquet both carry the CRS
//! faithfully): `geosetta in.shp - --to fgb | ... | geosetta - out.shp --from fgb`.
//!
//! # Input and output may be the same format
//!
//! They may not be the same *file*. Both halves of that are recent, and they
//! arrived together (`plans/same-format-output.org`).
//!
//! A same-format run is not a no-op: it re-encodes through the feature model,
//! which normalises the file and validates it end to end — a parse failure is
//! the finding. It is also the *only* shape in which an edit loop is lossless,
//! because a transit format that differs from the endpoint format is entitled
//! to drop whatever it cannot hold:
//! ```text
//! geosetta in.fgb - --to fgb | your-editor | geosetta - out.fgb --from fgb
//! ```
//! Where the re-encode still loses something, the run says so — the
//! cross-format defence ("the target has no slot for it") does not apply when
//! the target *is* the source's format. See `same_format_warning`.
//!
//! The same-file refusal ([`refuse_same_file`]) is the other half, and it is
//! not merely tidiness: the format check that used to reject `a.fgb a.fgb` had
//! been serving as an accidental in-place guard, and removing it without
//! replacing it would newly expose a run that truncates its only copy before
//! writing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use geosetta::{cli, convert, geopackage, shapefile};
use geosetta::Crs;
use geosetta::{Error, FeatureCollection, Format, Result, WriteOptions};

/// Read `path`'s bytes, or all of stdin when `path` is `"-"`.
fn read_bytes(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin().lock().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(std::fs::read(path)?)
    }
}

/// Write `bytes` to `path`, or to stdout when `path` is `"-"`.
fn write_bytes(path: &str, bytes: &[u8]) -> Result<()> {
    if path == "-" {
        std::io::stdout().lock().write_all(bytes)
    } else {
        std::fs::write(path, bytes)
    }
    .map_err(Error::from)
}

/// A human label for `--progress`/status messages: `path` verbatim, or
/// `"stdin"`/`"stdout"` for `"-"` (which direction depends on the caller).
fn io_label<'a>(path: &'a str, when_dash: &'a str) -> &'a str {
    if path == "-" { when_dash } else { path }
}

/// An error for a multi-file format (Shapefile) asked to read/write `"-"` —
/// there's no single byte stream to pipe; see the module doc comment.
fn no_stdio_for_multifile(format_name: &str) -> Error {
    Error::Usage(format!(
        "{format_name} is multi-file; piping it through stdin/stdout (\"-\") isn't supported — \
         convert through a single-buffer format instead (e.g. FlatGeobuf or GeoParquet)"
    ))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("geosetta: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = cli::parse(std::env::args())?;

    // The `--print-crs*` flags read a source and report on it: no output, no
    // conversion. They return before every write-side path below, GeoPackage's
    // included (that container's multi-layer read has its own answer for what to
    // report — see `print_crs_code` and `print_crs`).
    let out = match &args.mode {
        cli::Mode::PrintCrsCode => return print_crs_code(&args),
        cli::Mode::PrintCrs => return print_crs(&args),
        cli::Mode::Convert { output, to } => Output { path: output, format: *to },
    };

    // Before anything is read or written: an output that would overwrite its
    // own input is refused outright (D4).
    refuse_same_file(&args.input, args.from, &out)?;

    // Read the `--crs` override up front: a malformed one should fail before
    // anything has been read or written, not midway through.
    let crs_override = load_crs_override(&args)?;

    // GeoPackage is a multi-layer container, so it doesn't fit the plain
    // single-collection convert path.
    match (args.from, out.format) {
        (_, Format::Gpkg) => return run_geopackage_write(&args, &out, crs_override.as_ref()),
        (Format::Gpkg, _) => return run_geopackage_read(&args, &out, crs_override.as_ref()),
        _ => {}
    }

    // Everything else routes through the shared feature IR, so any input format
    // converts to any output format the writers support. The stages are made
    // visible under --progress (the pipeline is batch, so this is per-stage, not
    // sub-stage). Shapefile is multi-file (read_input/write_collection handle
    // locating/writing its .shp/.shx/.dbf/.prj siblings) but otherwise composes
    // through the IR exactly like every other format.
    let mut fc = read_input(&args)?;
    if args.progress {
        eprintln!("parsed {} features from {}", fc.features.len(), args.from.extension());
    }
    if args.sort_hilbert {
        convert::reorder_hilbert(&mut fc);
        if args.progress {
            eprintln!("sorted {} features by Hilbert locality", fc.features.len());
        }
    }
    // Before the warnings: a supplied override is exactly as authoritative as a
    // CRS read from the file, so it must be in place before anything predicts
    // what the target can record.
    if let Some(crs) = &crs_override {
        install_crs_override(&mut fc, crs);
    }
    print_warnings(&same_format_warning(args.from, out.format), args.quiet);
    print_warnings(&collect_conversion_warnings(&fc, out.format), args.quiet);
    print_warnings(&convert::kml_write_warnings(&fc, out.format, args.kml_version), args.quiet);
    release_foreign_natives(&mut fc, out.format);
    if args.progress {
        eprintln!("writing {}...", out.format.extension());
    }
    write_collection(out.format, out.path, &fc, args.quiet, args.write_options())
}

/// Drop every native the target's writer will not read, once the warnings
/// that inspect them have run.
///
/// Only a memory saving: a writer replays a payload only when its tag matches
/// (`Native::payload_for`), so this changes no output. It matters for KML,
/// whose natives are each feature's whole element — on a large `kml -> fgb`
/// run they would otherwise be held through the write for nothing. KML and KMZ
/// read each other's natives, since the two hold one document.
fn release_foreign_natives(fc: &mut FeatureCollection, target: Format) {
    let kept = |format: Format| {
        format == target || matches!((format, target), (Format::Kml, Format::Kmz) | (Format::Kmz, Format::Kml))
    };
    if fc.native.as_ref().is_some_and(|n| !kept(n.format)) {
        fc.native = None;
    }
    for f in &mut fc.features {
        if f.native.as_ref().is_some_and(|n| !kept(n.format)) {
            f.native = None;
        }
    }
}

/// The destination half of a conversion — where output goes and in what format.
/// Lifted out of [`cli::Mode::Convert`] once, in [`run`], so nothing downstream
/// has to re-match the mode to find out whether it has an output at all.
struct Output<'a> {
    path: &'a str,
    format: Format,
}

/// Read and parse the `--crs` override, if one was given: text from a file, or
/// from stdin for `-`.
///
/// Whatever produced that text — PROJ, GDAL, a web service, a registry crate, a
/// human — is the user's business and is invisible here. Geosetta spawns nothing
/// (`plans/crs-external-resolution.org`), so the only thing that crosses this
/// boundary is bytes the user pointed it at.
fn load_crs_override(args: &cli::Args) -> Result<Option<Crs>> {
    let Some(path) = &args.crs else { return Ok(None) };
    let text = if path == "-" {
        let mut s = String::new();
        std::io::stdin().lock().read_to_string(&mut s)?;
        s
    } else {
        std::fs::read_to_string(path)?
    };
    Crs::from_definition_text(&text).map(Some)
}

/// Install a `--crs` override on one collection.
///
/// `--crs` is *assign, not enrich*: it replaces whatever the source carried — or
/// its lack of a CRS — outright, `ogr2ogr -a_srs` style. Geosetta deliberately
/// does not reconcile the override against the source's own declared identity,
/// because it has no view on where the text came from or why the user is
/// supplying it. And it relabels only: no coordinate is touched, here or
/// anywhere else in the crate.
fn install_crs_override(fc: &mut FeatureCollection, crs: &Crs) {
    fc.crs = Some(crs.clone());
}

/// [`install_crs_override`] across a GeoPackage's layers, plus the one warning
/// only a multi-layer source can earn.
///
/// Overriding is the user's explicit instruction, so a single CRS being replaced
/// has nothing to report. But when the layers *disagreed*, one identity now
/// stands in for several — a real relabel the user may not have pictured when
/// they pointed `--crs` at a container rather than a file. Announcing it rather
/// than performing it quietly is the same convention every other lossy path in
/// this crate follows (`plans/lossy-conversion-warnings.org`).
fn apply_crs_override(layers: &mut [(String, FeatureCollection)], crs: &Crs) -> Vec<String> {
    let mut distinct: Vec<Option<Crs>> = Vec::new();
    for (_, fc) in layers.iter() {
        if !distinct.contains(&fc.crs) {
            distinct.push(fc.crs.clone());
        }
    }
    for (_, fc) in layers.iter_mut() {
        install_crs_override(fc, crs);
    }
    if distinct.len() < 2 {
        return Vec::new();
    }
    vec![format!(
        "--crs relabels all {} layers as {}, but the source declared {} different CRSes; \
         the override replaces every one of them.",
        layers.len(),
        crs.authority_code().unwrap_or_else(|| "the supplied definition".into()),
        distinct.len(),
    )]
}

/// `--print-crs-code`: report the source's CRS identity as `AUTHORITY:CODE` on
/// stdout and exit without converting anything.
///
/// Unconditional by design. It prints whenever the source *has* an identity,
/// never only when geosetta judges that identity to need resolving — deciding
/// that on the user's behalf would make the flag's behavior situational, and a
/// user who wants to run a resolver should always be able to.
///
/// A multi-layer GeoPackage can carry more than one code, so they are printed
/// one per line, de-duplicated in layer order: the usual single-CRS file still
/// yields exactly one line for a `$(...)` substitution, and `--layer` narrows
/// anything else. Empty stdout with a nonzero exit means there was genuinely
/// nothing to report — no CRS at all, or one with no authority code (an id-less
/// WKT, or a GeoPackage's placeholder `NONE` id, which
/// [`Crs::authority_code`] declines to report because geosetta invented it).
/// Note that a GeoJSON or KML source is *not* that case: its spec fixes it at
/// WGS 84 — or, for a KML document whose heights are all `absolute`, at
/// EPSG:9707 — so it has a real identity that simply isn't written in the file
/// (see `Crs::authority_code`).
fn print_crs_code(args: &cli::Args) -> Result<()> {
    let sources = read_crs_sources(args)?;

    let mut codes: Vec<String> = Vec::new();
    for (_, fc) in &sources {
        if let Some(code) = fc.crs.as_ref().and_then(Crs::authority_code)
            && !codes.contains(&code)
        {
            codes.push(code);
        }
    }
    if codes.is_empty() {
        return Err(Error::Usage(format!(
            "{}: no CRS authority code to report — the source carries no CRS, or one with no \
             authority and code (an id-less WKT definition, or a GeoPackage's placeholder \
             NONE id). Use --print-crs to print the definition itself, or supply one with \
             --crs, instead of asking for a code to resolve.",
            io_label(&args.input, "stdin"),
        )));
    }
    for code in &codes {
        println!("{code}");
    }
    Ok(())
}

/// The collections a `--print-crs*` diagnostic should report on, each paired
/// with its layer name when the source has them.
///
/// GeoPackage is the only multi-layer container, so it fans out (narrowed by
/// `--layer`, which errors rather than silently reporting nothing when the name
/// matches no layer); every other format is one collection, tagged `None`. Both
/// diagnostics read exactly this set, so neither can drift from the other on
/// which layers are in scope.
fn read_crs_sources(args: &cli::Args) -> Result<Vec<(Option<String>, FeatureCollection)>> {
    if args.from != Format::Gpkg {
        return Ok(vec![(None, read_input(args)?)]);
    }
    let input = read_bytes(&args.input)?;
    let mut layers = geopackage::read_layers(&input)?;
    if let Some(name) = &args.layer {
        layers.retain(|(n, _)| n == name);
        if layers.is_empty() {
            return Err(Error::Usage(format!("no layer named \"{name}\"")));
        }
    }
    Ok(layers
        .into_iter()
        .map(|(name, fc)| (Some(name), fc))
        .collect())
}

/// `--print-crs`: write the CRS *definition text* the source recorded to stdout,
/// verbatim, and exit without converting anything.
///
/// The companion to `print_crs_code`, and its complement: that flag reports the
/// source's *identity* so an external tool can resolve it, which structurally
/// cannot work for a definition carrying no authority code. This one hands over
/// the definition itself, which is exactly what a name-recovery tool needs
/// (`geoscribe --identify`, `projinfo @file`) — closing the case where the tool
/// that could identify an id-less definition could not reach it.
///
/// Output is byte-identical to what the source recorded, plus one trailing
/// newline: geosetta does not reformat, re-indent, re-order, or re-dialect a
/// definition it never interprets, so piping this into `--crs -` on the same
/// file is a no-op. **PROJJSON wins when a source carries both dialects** — see
/// [`Crs::definition_body`], which states the rule and why.
///
/// Empty stdout with a nonzero exit means there was genuinely nothing to print,
/// the same contract `print_crs_code` keeps. Note that the two flags disagree
/// about a GeoJSON or KML source, and correctly: its spec fixes its CRS (WGS 84,
/// or EPSG:9707 for KML heights that are all `absolute`), so it has an identity
/// to report but recorded no text to quote.
///
/// Unlike a code, a definition body can span multiple lines, so more than one
/// distinct definition among the selected layers is an error rather than a
/// delimited stream — there is no record separator geosetta could invent that a
/// receiver would already know how to split on.
///
/// Known limitation, per `plans/crs-definition-output.org`: the multi-layer and
/// dialect-precedence paths are exercised against constructed fixtures only. No
/// real-world file carrying an id-less definition has been confirmed to exist
/// (PROJ/GDAL always emit a root `id`), so the shape of one is reasoned about
/// rather than observed.
fn print_crs(args: &cli::Args) -> Result<()> {
    let sources = read_crs_sources(args)?;

    // Distinct definitions in layer order, each tagged with the first layer that
    // carried it, so the multi-CRS error can name what to pass to `--layer`.
    let mut bodies: Vec<(&str, Option<&str>)> = Vec::new();
    for (layer, fc) in &sources {
        if let Some(body) = fc.crs.as_ref().and_then(Crs::definition_body)
            && !bodies.iter().any(|(seen, _)| *seen == body)
        {
            bodies.push((body, layer.as_deref()));
        }
    }

    match bodies.as_slice() {
        [] => Err(nothing_to_print(args, &sources)),
        [(body, _)] => {
            if args.escape {
                println!("{}", escape_control_bytes(body));
            } else {
                println!("{body}");
            }
            Ok(())
        }
        many => Err(Error::Usage(format!(
            "{}: the selected layers carry {} different CRS definitions, and a definition \
             body can span several lines — there is no unambiguous way to print more than \
             one. Narrow to a single layer with --layer ({}).",
            io_label(&args.input, "stdin"),
            many.len(),
            many.iter()
                .filter_map(|(_, layer)| *layer)
                .map(|l| format!("\"{l}\""))
                .collect::<Vec<_>>()
                .join(", "),
        ))),
    }
}

/// Render the bytes that would act on a terminal so they are read instead of
/// obeyed — `--escape`, and only `--escape`.
///
/// A definition body is arbitrary content from an arbitrary input file, so a
/// crafted `name` can carry ANSI/OSC sequences that a terminal interprets:
/// title-bar spoofing, cursor manipulation. `--print-crs` prints raw anyway by
/// default, which is the same call `cat` and GDAL's own CLI tools make and what
/// the round-trip contract requires (see
/// `plans/crs-definition-output.org` § DECISIONS). This is the opt-in escape
/// hatch for the other case: a human eyeballing a file they do not trust.
///
/// The notation is `cat -v`'s, matched against its observed *output* rather
/// than ported from any implementation of it:
///
/// - C0 controls become `^` plus the byte XOR `0x40`, so `ESC` (`0x1B`) reads
///   as `^[` and `BEL` (`0x07`) as `^G`.
/// - `DEL` (`0x7F`) becomes `^?`.
/// - A byte with the high bit set becomes `M-` plus the same notation applied
///   to its low seven bits.
///
/// **Tab and newline pass through untouched.** Pretty-printed WKT and PROJJSON
/// are routinely multi-line and indented, so escaping those would mangle every
/// ordinary input rather than only the crafted ones — which would make the flag
/// useless for the very case it exists to serve.
///
/// One consequence worth knowing: this is byte-oriented, like `cat -v`, so a
/// non-ASCII character in a legitimate CRS name (`Réunion`, `Bogotá` — EPSG is
/// full of them) renders as `M-` sequences too. That is correct for a flag whose
/// job is "show me every byte that is not plain ASCII", and it is another reason
/// this is display-only and never the default.
fn escape_control_bytes(text: &str) -> String {
    /// `b` in caret notation, for a byte already known to need it.
    fn caret(b: u8, out: &mut String) {
        if b == 0x7F {
            out.push_str("^?");
        } else {
            out.push('^');
            out.push((b ^ 0x40) as char);
        }
    }

    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        match b {
            // The two whitespace bytes a definition legitimately contains.
            b'\t' | b'\n' => out.push(b as char),
            0x00..=0x1F | 0x7F => caret(b, &mut out),
            0x80.. => {
                out.push_str("M-");
                let low = b & 0x7F;
                match low {
                    0x00..=0x1F | 0x7F => caret(low, &mut out),
                    _ => out.push(low as char),
                }
            }
            _ => out.push(b as char),
        }
    }
    out
}

/// The error for a `--print-crs` that found no definition text, which is two
/// genuinely different situations and says which.
///
/// A source whose CRS is the format's implicit WGS 84 default recorded no
/// definition *because the format never writes one* — `--print-crs-code` reports
/// `OGC:CRS84` for it and exits 0, and that code resolves trivially, so the
/// message sends the user there rather than implying the file is deficient.
/// Anything else — no CRS at all, or an authority and code with no definition
/// text beside it — is the ordinary "nothing to report" case.
fn nothing_to_print(args: &cli::Args, sources: &[(Option<String>, FeatureCollection)]) -> Error {
    let label = io_label(&args.input, "stdin");
    if sources
        .iter()
        .any(|(_, fc)| fc.crs.as_ref() == Some(&Crs::Wgs84))
    {
        return Error::Usage(format!(
            "{label}: no CRS definition body to print — the source's CRS is the format's \
             implicit WGS 84 default, which it records no definition for. Use \
             --print-crs-code (prints {}) and resolve that instead.",
            geosetta::WGS84_CRS_CODE,
        ));
    }
    Error::Usage(format!(
        "{label}: no CRS definition body to print — the source carries no CRS, or carries \
         only an authority and code with no definition text beside it. Use --print-crs-code \
         to report the code and resolve that instead."
    ))
}

/// Read the input into the Feature IR. Shapefile is multi-file, so its
/// sibling files (`.dbf` required, `.prj`/`.cpg` optional) are located next to
/// `args.input` and read explicitly (and can't be `"-"` — see the module doc
/// comment); every other format reads a single buffer — `args.input`'s bytes,
/// or all of stdin when it's `"-"` — through [`convert::read_features`].
///
/// What the read itself dropped is printed here, as the read finishes — before
/// any write-side warning, since it happened first
/// (`plans/lossy-conversion-warnings.org` § 2b).
fn read_input(args: &cli::Args) -> Result<FeatureCollection> {
    let mut warnings = Vec::new();
    let fc = if args.from == Format::Shapefile {
        if args.input == "-" {
            return Err(no_stdio_for_multifile("Shapefile"));
        }
        read_shapefile_from_path(&args.input, args.progress, &mut warnings)?
    } else {
        let input = read_bytes(&args.input)?;
        if args.progress {
            eprintln!("read {} ({} bytes)", io_label(&args.input, "stdin"), input.len());
        }
        convert::read_features(args.from, &input, &mut warnings)?
    };
    print_warnings(&warnings, args.quiet);
    Ok(fc)
}

/// Write the Feature IR to `output_path`. Shapefile is multi-file, so its
/// sibling files are written explicitly under `output_path`'s stem (and
/// `output_path` can't be `"-"` — see the module doc comment); every other
/// format writes a single buffer — to `output_path`, or to stdout when it's
/// `"-"` — through [`convert::write_features`]. Shared by the plain
/// single-collection path and GeoPackage's per-layer fan-out
/// (`run_geopackage_read`), so a multi-layer `.gpkg` → Shapefile conversion
/// gets one `layer.shp` sibling set per layer for free. `options` are
/// `--kml-version`, which the CLI only accepts for KML/KMZ output, and
/// `--compact`.
fn write_collection(
    to: Format,
    output_path: &str,
    fc: &FeatureCollection,
    quiet: bool,
    options: WriteOptions,
) -> Result<()> {
    if to == Format::Shapefile {
        if output_path == "-" {
            return Err(no_stdio_for_multifile("Shapefile"));
        }
        return write_shapefile_to_path(output_path, fc, quiet);
    }
    let bytes = convert::write_features_with(to, fc, options)?;
    write_bytes(output_path, &bytes)?;
    eprintln!("wrote {} ({} bytes)", io_label(output_path, "stdout"), bytes.len());
    Ok(())
}

/// Read a Shapefile given its `.shp` path, locating `.dbf` (required),
/// `.prj`, and `.cpg` (both optional) alongside it.
fn read_shapefile_from_path(shp_path: &str, progress: bool, warnings: &mut Vec<String>) -> Result<FeatureCollection> {
    let shp = std::fs::read(shp_path)?;
    let dbf_path = sibling_path(shp_path, "dbf")
        .ok_or_else(|| Error::Usage(format!("shapefile: no .dbf found alongside {shp_path}")))?;
    let dbf_bytes = std::fs::read(&dbf_path)?;
    let prj = sibling_path(shp_path, "prj").map(std::fs::read_to_string).transpose()?;
    let cpg = sibling_path(shp_path, "cpg").map(std::fs::read_to_string).transpose()?;
    if progress {
        eprintln!(
            "read {shp_path} ({} bytes) + {dbf_path} ({} bytes){}",
            shp.len(),
            dbf_bytes.len(),
            if prj.is_some() { " + .prj" } else { "" },
        );
    }
    shapefile::read(&shp, &dbf_bytes, prj.as_deref(), cpg.as_deref(), warnings)
}

/// Every path a side of the conversion touches, canonicalised for comparison.
///
/// A Shapefile is a *file set*, not a path, so it contributes all its
/// siblings: the collision `plans/same-format-output.org` D4 is about is
/// `geosetta roads.shp roads.dbf` as much as `roads.shp roads.shp`.
///
/// `-` contributes nothing — stdin and stdout are different streams even when
/// both are the same terminal.
fn touched_paths(path: &str, format: Format) -> Vec<PathBuf> {
    if path == "-" {
        return Vec::new();
    }
    let raw = if format == Format::Shapefile {
        let stem = strip_shp_extension(path);
        ["shp", "shx", "dbf", "prj", "cpg"].iter().map(|e| format!("{stem}.{e}")).collect()
    } else {
        vec![path.to_string()]
    };
    raw.iter().filter_map(|p| canonical_for_compare(p)).collect()
}

/// Canonicalise `path` even when it does not exist yet.
///
/// The output usually does not, which is the whole difficulty: `canonicalize`
/// fails on a missing file, so fall back to canonicalising the *parent* — which
/// does exist — and rejoining the file name. That still resolves `./a.fgb`,
/// `a.fgb` and a symlinked directory to one answer, which is the point.
fn canonical_for_compare(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let parent = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    Some(parent.canonicalize().ok()?.join(p.file_name()?))
}

/// Refuse a run whose output would overwrite its own input.
///
/// `plans/same-format-output.org` D4, and it is the load-bearing half of
/// deleting the format check: same format almost always meant same extension,
/// so `geosetta a.fgb a.fgb` was refused *for the wrong reason* and became
/// reachable the moment that check went.
///
/// The happy path would survive it — every reader completes before any writer
/// opens a file — but the unhappy path does not. `std::fs::write` truncates
/// first, so any write-side failure after that point has destroyed the only
/// copy, and a Shapefile write is *four* files in sequence: a failure at
/// `.dbf` leaves a `.shp` of new data, a `.dbf` of neither, and no original to
/// retry from. There is no atomic write in this crate to fall back on.
///
/// This deliberately changes the GeoPackage path's behaviour, which used to
/// permit an in-place run. That permissiveness rested on the append path
/// reading the output before overwriting it — luck, not design.
fn refuse_same_file(input: &str, from: Format, out: &Output) -> Result<()> {
    let ins = touched_paths(input, from);
    let outs = touched_paths(out.path, out.format);
    let Some(clash) = outs.iter().find(|o| ins.contains(o)) else {
        return Ok(());
    };
    Err(Error::Usage(format!(
        "input and output are the same file ({}); write to a different path, \
         since a failed write would leave no original to retry from",
        clash.display()
    )))
}

/// Write a Shapefile's sibling files under `output_path`'s stem (a trailing
/// `.shp`/`.SHP`, if present, is stripped so `roads.shp` and `roads` both name
/// the same `roads.{shp,shx,dbf,cpg,prj}` set).
///
/// Writes every file it can and deletes none, then reports the sidecars it left
/// behind — see [`stale_sidecar_warnings`] and `plans/shapefile.org`
/// § THE SIDECAR POLICY.
fn write_shapefile_to_path(output_path: &str, fc: &FeatureCollection, quiet: bool) -> Result<()> {
    let encoded = shapefile::write(fc)?;
    print_warnings(&encoded.warnings, quiet);
    let stem = strip_shp_extension(output_path);
    std::fs::write(format!("{stem}.shp"), &encoded.shp)?;
    std::fs::write(format!("{stem}.shx"), &encoded.shx)?;
    std::fs::write(format!("{stem}.dbf"), &encoded.dbf)?;
    std::fs::write(format!("{stem}.cpg"), encoded.cpg)?;
    if let Some(prj) = &encoded.prj {
        std::fs::write(format!("{stem}.prj"), prj)?;
    }
    // After writing: say what of the destination's existing file set this run
    // left behind, because those files now describe the output beside them.
    // See `stale_sidecar_warnings` on why this cannot be checked beforehand.
    print_warnings(&stale_sidecar_warnings(&stem, &encoded), quiet);
    eprintln!(
        "wrote {stem}.shp + .shx + .dbf + .cpg{} ({} features)",
        if encoded.prj.is_some() { " + .prj" } else { "" },
        fc.features.len(),
    );
    Ok(())
}

/// Sidecar files a Shapefile can have that this writer does not produce, with
/// what each one would misdescribe if left beside our output.
///
/// `.shp`/`.shx`/`.dbf`/`.cpg` are absent because this writer always writes
/// them, so a pre-existing one is overwritten rather than left to disagree.
/// `.prj` is handled separately: it is written only when the collection *has* a
/// CRS, and the case where it does not is the sharpest one on the list.
const UNWRITTEN_SIDECARS: &[(&str, &str)] = &[
    ("qix", "a quadtree spatial index describing different geometry"),
    ("sbn", "an Esri spatial index describing different geometry"),
    ("sbx", "an Esri spatial index describing different geometry"),
    ("fbn", "a read-only Esri spatial index describing different geometry"),
    ("fbx", "a read-only Esri spatial index describing different geometry"),
    ("ain", "an attribute index describing different attributes"),
    ("aih", "an attribute index describing different attributes"),
    ("atx", "an attribute index describing different attributes"),
    ("ixs", "a geocoding index describing different features"),
    ("mxs", "a geocoding index describing different features"),
    ("qpj", "a CRS, in QGIS's sidecar, that this run did not write"),
    ("shp.xml", "metadata describing a different dataset"),
];

/// Warn about files at the output basename that this run did not overwrite.
///
/// **Geosetta writes the files it can and deletes nothing** — so when a
/// Shapefile is written where one already exists, part of the resulting file
/// set can be somebody else's, and those files *describe* the part we wrote. A
/// stale `.prj` is read as the output's CRS; a stale index is read as its
/// geometry's layout. The user is told which files that applies to and left to
/// decide, rather than having data removed as a side effect of a conversion.
///
/// # Why warning is the whole answer here and not half of one
///
/// Two cases cannot be fixed by writing more. `.sbn`/`.sbx` are Esri's
/// proprietary format, which GDAL itself declines to write. And `.prj` has
/// nothing to say when the source carries no CRS — OGC WKT has no spelling for
/// "unknown", so inventing one would be the guess that `shapefile::writer`'s
/// `.prj` doc already refuses to make. /`ogr2ogr -overwrite` removes the prior
/// file set instead (measured), which is the other coherent answer and not
/// this project's: a conversion must not delete files nobody asked it to
/// touch./
///
/// # Called *after* the write, which is not an accident
///
/// A case-differing spelling cannot be judged before writing, because whether
/// `out.PRJ` and `out.prj` are two files or one is a property of the
/// filesystem rather than of the names — on macOS and Windows the write lands
/// in the existing file and there is nothing to report, while on a
/// case-sensitive filesystem both survive and readers disagree about which
/// wins. So the only honest test is to write first and then compare bytes: a
/// differently-spelled file that still differs from what we just wrote is a
/// real second file. /A first version probed `Path::exists` for the
/// upper-cased spelling before writing and reported every sidecar twice on
/// this machine, which is how the distinction was noticed./
///
/// # The blind spot, stated because it is not obvious
///
/// This fires on the *write*. A later `geosetta out.shp …` reads a stale
/// `.prj` as simply the file's CRS and says nothing, because by then there is
/// no evidence anything is wrong. That asymmetry is why `.cpg` is written
/// rather than warned about — encoding is the one member of the set whose
/// right answer this writer always knows.
fn stale_sidecar_warnings(stem: &str, encoded: &shapefile::Encoded) -> Vec<String> {
    let stem_path = std::path::Path::new(stem);
    let dir = match stem_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let Some(base) = stem_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut warnings = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Only this output's own basename, and only a sidecar of it.
        let Some(ext) = name.strip_prefix(base).and_then(|rest| rest.strip_prefix('.')) else {
            continue;
        };
        let lower = ext.to_ascii_lowercase();
        let path = dir.join(name);

        // An extension we wrote. Ours landed at the lower-case spelling, so
        // anything else is only a second file if its bytes still differ.
        let written = match lower.as_str() {
            "shp" => Some(encoded.shp.clone()),
            "shx" => Some(encoded.shx.clone()),
            "dbf" => Some(encoded.dbf.clone()),
            "cpg" => Some(encoded.cpg.as_bytes().to_vec()),
            "prj" => encoded.prj.as_ref().map(|p| p.as_bytes().to_vec()),
            _ => None,
        };
        if let Some(ours) = written {
            if ext != lower && std::fs::read(&path).is_ok_and(|on_disk| on_disk != ours) {
                warnings.push(format!(
                    "shapefile: {} already exists and this run wrote {stem}.{lower}, so both \
                     spellings are present and readers disagree about which one wins \
                     (plans/shapefile.org)",
                    path.display(),
                ));
            }
            continue;
        }

        // A `.prj` we could not write at all: the source had no CRS to state.
        if lower == "prj" {
            warnings.push(format!(
                "shapefile: {} already exists and this run wrote no .prj, so that file will be \
                 read as this output's CRS (plans/shapefile.org)",
                path.display(),
            ));
            continue;
        }

        if let Some((_, what)) = UNWRITTEN_SIDECARS.iter().find(|(e, _)| *e == lower) {
            warnings.push(format!(
                "shapefile: {} already exists and this run does not write it, so it will be read \
                 as {what} (plans/shapefile.org)",
                path.display(),
            ));
        }
    }
    warnings.sort();
    warnings
}

fn strip_shp_extension(path: &str) -> String {
    let p = std::path::Path::new(path);
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("shp") => p.with_extension("").to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

/// Find a sibling file next to `shp_path` with extension `ext`, trying the
/// `.shp` path's own case first and its opposite second — real-world
/// shapefiles mix `.DBF`/`.dbf` casing. `None` when neither casing exists.
fn sibling_path(shp_path: &str, ext: &str) -> Option<String> {
    let path = std::path::Path::new(shp_path);
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let shp_ext_is_upper = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.chars().next().is_some_and(|c| c.is_ascii_uppercase()));
    let candidates =
        if shp_ext_is_upper { [ext.to_ascii_uppercase(), ext.to_string()] } else { [ext.to_string(), ext.to_ascii_uppercase()] };
    candidates.into_iter().find_map(|e| {
        let name = format!("{stem}.{e}");
        let p = match dir {
            Some(d) => d.join(&name),
            None => std::path::PathBuf::from(&name),
        };
        p.exists().then(|| p.to_string_lossy().into_owned())
    })
}

/// Read a GeoPackage (all layers) and fan out to the target format. Output
/// naming: a directory (existing or trailing `/`) gets one `layer.ext` file
/// each; a single layer goes to the plain output path; multiple layers to a
/// plain path is an error unless `--layer` selects one.
fn run_geopackage_read(args: &cli::Args, out: &Output, crs_override: Option<&Crs>) -> Result<()> {
    let input = read_bytes(&args.input)?;
    if args.progress {
        eprintln!("read {} ({} bytes)", io_label(&args.input, "stdin"), input.len());
    }
    let mut layers = geopackage::read_layers(&input)?;
    if args.progress {
        eprintln!("found {} layer(s)", layers.len());
        for (name, fc) in &layers {
            eprintln!("  layer {name}: {} features", fc.features.len());
        }
    }

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
    if let Some(crs) = crs_override {
        print_warnings(&apply_crs_override(&mut layers, crs), args.quiet);
    }
    print_warnings(&collect_layer_warnings(&layers, out.format), args.quiet);
    // And what a KML or KMZ write leaves out of each layer -- its nulls
    // (`plans/kml-standard.org` D-K17) -- once per distinct message, as above.
    let mut seen = std::collections::BTreeSet::new();
    let kml: Vec<String> = layers
        .iter()
        .flat_map(|(_, fc)| convert::kml_write_warnings(fc, out.format, args.kml_version))
        .filter(|w| seen.insert(w.clone()))
        .collect();
    print_warnings(&kml, args.quiet);

    // "-" (stdout) is never a directory, regardless of what happens to exist
    // in the current directory — a multi-layer .gpkg piped out is rejected
    // below by the same "give a directory or --layer" error a plain file path
    // gets, since a single stream can't fan out to more than one layer.
    let as_dir = out.path != "-"
        && (out.path.ends_with('/') || std::path::Path::new(out.path).is_dir());
    if layers.len() == 1 && !as_dir {
        let (_, fc) = &layers[0];
        return write_collection(out.format, out.path, fc, args.quiet, args.write_options());
    }
    if !as_dir {
        return Err(Error::Usage(
            "GeoPackage has multiple layers: give an output directory (trailing '/') or --layer NAME".into(),
        ));
    }

    std::fs::create_dir_all(out.path)?;
    let dir = std::path::Path::new(out.path);
    for (name, fc) in &layers {
        let path = dir.join(format!("{name}.{}", out.format.extension()));
        write_collection(out.format, &path.to_string_lossy(), fc, args.quiet, args.write_options())?;
    }
    Ok(())
}

/// Write a layer into a GeoPackage, creating it or appending (upserting the
/// layer if it already exists). The layer name defaults to the input file stem.
fn run_geopackage_write(args: &cli::Args, out: &Output, crs_override: Option<&Crs>) -> Result<()> {
    // Independent container content the source holds — a tile pyramid today —
    // read once here and handed to the writer, which is what stops a
    // `gpkg -> gpkg` run from destroying it (defect 3, the carrying half).
    // Empty for every other source format: there is no container to carry.
    let mut carried = geopackage::Carried::default();
    let mut new_layers = if args.from == Format::Gpkg {
        // gpkg -> gpkg: carry over all input layers (optionally one via --layer).
        let input = read_bytes(&args.input)?;
        if args.progress {
            eprintln!("read {} ({} bytes)", io_label(&args.input, "stdin"), input.len());
        }
        let mut ls = geopackage::read_layers(&input)?;
        if let Some(name) = &args.layer {
            ls.retain(|(n, _)| n == name);
            if ls.is_empty() {
                return Err(Error::Usage(format!("no layer named \"{name}\"")));
            }
        }
        // Carried *before* the warning is built, because the warning's job is
        // to name what does not survive and this is what does. `--layer` does
        // not narrow it: a pyramid is not a layer, so selecting one layer is
        // not a statement about the container around it.
        carried = geopackage::read_container(&input)?;
        // Defect 3: the write below reproduces the layers, the tables they
        // require, and whatever `carried` holds — and nothing else the source
        // container has. Reported here, where the source's bytes are still in
        // hand.
        print_warnings(
            &geopackage::container_drop_warnings(
                &input,
                io_label(&args.input, "stdin"),
                &names_of(&ls),
                &carried.reproduced_names(),
                args.rtree,
            ),
            args.quiet,
        );
        ls
    } else {
        // Shapefile input's siblings are located via read_input; every other
        // format reads a single buffer the same way it always has.
        let fc = read_input(args)?;
        // `--layer` wins, then the name the *source* recorded, then the input
        // file's stem. The middle step is what makes an edit loop replace a
        // layer instead of appending a duplicate: with `-` for input there is
        // no stem to draw on, so before collections carried a name every piped
        // write landed in a layer called `layer`
        // (`plans/edit-composability.org` D4).
        let name = args
            .layer
            .clone()
            .or_else(|| fc.name.clone())
            .unwrap_or_else(|| layer_stem(&args.input));
        vec![(name, fc)]
    };
    if args.progress {
        for (name, fc) in &new_layers {
            eprintln!("layer {name}: {} features", fc.features.len());
        }
    }
    if args.sort_hilbert {
        for (_, fc) in &mut new_layers {
            convert::reorder_hilbert(fc);
        }
    }
    if let Some(crs) = crs_override {
        print_warnings(&apply_crs_override(&mut new_layers, crs), args.quiet);
    }
    print_warnings(&collect_layer_warnings(&new_layers, out.format), args.quiet);

    // Append into the existing GeoPackage if the output already exists. "-"
    // (stdout) is never "existing" — there's nothing to read back from a pipe
    // you're about to write to, so piping to a .gpkg always starts fresh.
    let existing = if out.path == "-" { None } else { std::fs::read(out.path).ok() };
    // The destination's own container content, merged with what the source
    // carries — `plans/container-reconciliation.org` § 3. Read here rather than
    // in the writer because the warning below has to be built against the
    // merged result: what the destination keeps is no longer destroyed, and a
    // survey that still named it would report surviving content as lost, which
    // is bug 13's mistake.
    //
    // *Not read when the destination is the source.* That run is refused by
    // `same-format-output.org` D4 before reaching here for a `gpkg` source, and
    // for any other source there is no second container to merge.
    if let Some(bytes) = existing.as_deref()
        && !(args.from == Format::Gpkg && args.input == out.path)
    {
        carried = geopackage::reconcile_containers(geopackage::read_container(bytes)?, &carried)?;
        print_warnings(
            &geopackage::container_drop_warnings(
                bytes,
                io_label(out.path, "stdout"),
                &names_of(&new_layers),
                &carried.reproduced_names(),
                args.rtree,
            ),
            args.quiet,
        );
    }
    // A carried metadata pair is the one member that can survive in *part*:
    // references naming something the output will not contain are dropped, and
    // the container survey no longer names the pair at all now that it is
    // carried. Reported here, after `existing` is in hand, because **an append
    // supplies layers `new_layers` does not** — a reference to a layer only the
    // destination has still survives, and `write_layers` composes the two
    // before deciding. Handing this call `new_layers` alone made it claim
    // losses the same run did not incur.
    print_warnings(
        &geopackage::metadata_drop_warnings(existing.as_deref(), &new_layers, &carried),
        args.quiet,
    );
    if args.progress {
        let verb = if existing.is_some() { "appending to" } else { "writing" };
        let idx = if args.rtree { " (+rtree index)" } else { "" };
        let env = if args.envelope { " (+envelopes)" } else { "" };
        eprintln!("{verb} {}{idx}{env}...", io_label(out.path, "stdout"));
    }
    let bytes = geopackage::write_layers(
        existing.as_deref(),
        &new_layers,
        &carried,
        args.rtree,
        args.envelope,
    )?;
    write_bytes(out.path, &bytes)?;
    eprintln!("wrote {} ({} bytes)", io_label(out.path, "stdout"), bytes.len());
    Ok(())
}

/// Every pre-write predictive lossy-conversion check this crate knows about,
/// run against one collection and its target format. Each check is an
/// independent `Option<String>` function (see [`Crs::downgrade_warning`],
/// [`FeatureCollection::m_downgrade_warning`]) — adding a new kind of loss
/// (a new format spoke's capability gap, a new IR field) is one line here,
/// not a new wrapper-function pair. See `plans/lossy-conversion-warnings.org`.
/// The warning that has to ship with the same-format deletion, not after it.
///
/// `plans/same-format-output.org` D3: M1 alone is a working feature that
/// silently drops data, which the crate's own convention forbids. On a
/// cross-format run a drop has a defence — the target may have no slot — and
/// `collect_conversion_warnings` handles that case. Here there is none: the
/// target is the source's own format, so it provably can hold what the source
/// held.
///
/// Silent by design when the formats differ, and when the format round-trips
/// clean (Shapefile), so this does not become furniture on every run.
fn same_format_warning(from: Format, target: Format) -> Vec<String> {
    if from != target {
        return Vec::new();
    }
    let losses = target.same_format_losses();
    if losses.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "{} to {} is a re-encode through the feature model, not a copy, so it does not preserve: \
         {}. (plans/same-format-output.org)",
        target.display_name(),
        target.display_name(),
        losses.join("; "),
    )]
}

fn collect_conversion_warnings(fc: &FeatureCollection, target: Format) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(w) = fc.crs.as_ref().and_then(|c| c.downgrade_warning(target)) {
        warnings.push(w);
    }
    if let Some(w) = fc.m_downgrade_warning(target) {
        warnings.push(w);
    }
    if let Some(w) = fc.schema_drop_warning(target) {
        warnings.push(w);
    }
    if let Some(w) = fc.decl_recast_warning(target) {
        warnings.push(w);
    }
    warnings
}

/// [`collect_conversion_warnings`] across a GeoPackage's layers,
/// de-duplicating identical messages so a single-CRS `.gpkg` (the usual
/// case) warns just once even when it fans out to many output files.
fn collect_layer_warnings(layers: &[(String, FeatureCollection)], target: Format) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (_, fc) in layers {
        for w in collect_conversion_warnings(fc, target) {
            if seen.insert(w.clone()) {
                out.push(w);
            }
        }
    }
    out
}

/// Print each warning to stderr, unless `quiet`. Shared by both
/// lossy-conversion mechanisms this crate uses: the pre-write predictive
/// checks above, and a writer's own post-write `Encoded.warnings` (e.g.
/// Shapefile's `.dbf` field-truncation warnings in `write_shapefile_to_path`)
/// — see `plans/lossy-conversion-warnings.org`.
///
/// The `warning: ` prefix the convention requires is added *here*, not by each
/// check, so a new check cannot forget it or spell it differently — which is
/// exactly what had happened: the `.dbf` warnings were written with the
/// `shapefile: ` tag the shapefile *errors* use, and so read as errors. Checks
/// return the message body; presentation is this function's job. A message may
/// still carry its own component tag after the prefix (`warning: shapefile: …`).
fn print_warnings(warnings: &[String], quiet: bool) {
    if quiet {
        return;
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
}

/// The layer names of a `(name, collection)` list, which is the shape both
/// GeoPackage warning call sites need and neither should spell out inline.
fn names_of(layers: &[(String, FeatureCollection)]) -> Vec<String> {
    layers.iter().map(|(n, _)| n.clone()).collect()
}

/// The file stem of `path`, used as a default layer name of *last* resort:
/// `--layer` and the collection's own [`FeatureCollection::name`] are both
/// consulted first. `"-"` (stdin) has no filename to draw one from, so it falls
/// back to `"layer"` same as an extension-less/empty path does.
fn layer_stem(path: &str) -> String {
    if path == "-" {
        return "layer".into();
    }
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "layer".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_renders_the_bytes_that_would_act_on_a_terminal() {
        // The three notations, on the sequence `stdout-security.org` finding 1
        // actually used: an OSC title-bar spoof (ESC + BEL) inside a CRS name.
        assert_eq!(
            escape_control_bytes("GEOGCRS[\"EVIL\u{1b}]0;PWNED\u{7}NAME\"]"),
            "GEOGCRS[\"EVIL^[]0;PWNED^GNAME\"]"
        );
        // Both ends of the C0 range, and DEL, which is not in it.
        assert_eq!(escape_control_bytes("\u{0}\u{1}\u{1f}\u{7f}"), "^@^A^_^?");
    }

    #[test]
    fn escape_leaves_tab_and_newline_alone() {
        // Pretty-printed WKT and PROJJSON are routinely multi-line and indented,
        // so escaping these would mangle every ordinary input rather than only
        // the crafted ones.
        let pretty = "{\n\t\"type\": \"GeographicCRS\",\n\t\"name\": \"WGS 84\"\n}";
        assert_eq!(escape_control_bytes(pretty), pretty);
        // But a carriage return is neither, and does move a terminal's cursor.
        assert_eq!(escape_control_bytes("a\rb"), "a^Mb");
    }

    #[test]
    fn escape_renders_high_bit_bytes_in_m_notation() {
        // Byte-oriented, like `cat -v` in the C locale: `é` is two high-bit
        // bytes (0xC3 0xA9) and reads as `M-C M-)`. Verified against
        // `LC_ALL=C cat -v` directly, which is the reference this matches —
        // macOS's own `cat -v` under a UTF-8 locale passes valid multibyte
        // through instead, so a reader comparing the two must fix the locale.
        assert_eq!(escape_control_bytes("Réunion"), "RM-CM-)union");
        // The two notations compose when a high-bit byte's low seven bits are
        // themselves a control byte: U+0080 encodes as `C2 80`, and `80 & 7F`
        // is NUL, so the second byte reads `M-` + `^@`.
        assert_eq!(escape_control_bytes("\u{80}"), "M-BM-^@");
    }

    #[test]
    fn escape_is_the_identity_on_ordinary_definition_text() {
        // The overwhelmingly common case must cost nothing and change nothing.
        let wkt = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]]]"#;
        assert_eq!(escape_control_bytes(wkt), wkt);
    }
}
