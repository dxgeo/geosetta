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

use std::io::{Read, Write};

use geosetta::{cli, convert, geopackage, shapefile};
use geosetta::Crs;
use geosetta::{Error, FeatureCollection, Format, Result};

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

    // `--print-crs-code` reads a source and reports on it: no output, no
    // conversion. It returns before every write-side path below, GeoPackage's
    // included (that container's multi-layer read has its own answer for what
    // to report — see `print_crs_code`).
    let out = match &args.mode {
        cli::Mode::PrintCrsCode => return print_crs_code(&args),
        cli::Mode::Convert { output, to } => Output { path: output, format: *to },
    };

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

    if args.from == out.format {
        return Err(Error::Usage(format!(
            "input and output are the same format ({:?}); nothing to convert",
            args.from
        )));
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
    print_warnings(&collect_conversion_warnings(&fc, out.format), args.quiet);
    if args.progress {
        eprintln!("writing {}...", out.format.extension());
    }
    write_collection(out.format, out.path, &fc, args.quiet)
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
/// WKT). Note that a GeoJSON or KML source is *not* that case: its spec fixes it
/// at WGS 84, so it has a real identity that simply isn't written in the file
/// (see `Crs::authority_code`).
fn print_crs_code(args: &cli::Args) -> Result<()> {
    let sources: Vec<FeatureCollection> = if args.from == Format::Gpkg {
        let input = read_bytes(&args.input)?;
        let mut layers = geopackage::read_layers(&input)?;
        if let Some(name) = &args.layer {
            layers.retain(|(n, _)| n == name);
            if layers.is_empty() {
                return Err(Error::Usage(format!("no layer named \"{name}\"")));
            }
        }
        layers.into_iter().map(|(_, fc)| fc).collect()
    } else {
        vec![read_input(args)?]
    };

    let mut codes: Vec<String> = Vec::new();
    for fc in &sources {
        if let Some(code) = fc.crs.as_ref().and_then(Crs::authority_code)
            && !codes.contains(&code)
        {
            codes.push(code);
        }
    }
    if codes.is_empty() {
        return Err(Error::Usage(format!(
            "{}: no CRS authority code to report — the source carries no CRS, or one with no \
             authority and code (an id-less WKT definition). Supply a definition with --crs \
             instead of asking for a code to resolve.",
            io_label(&args.input, "stdin"),
        )));
    }
    for code in &codes {
        println!("{code}");
    }
    Ok(())
}

/// Read the input into the Feature IR. Shapefile is multi-file, so its
/// sibling files (`.dbf` required, `.prj`/`.cpg` optional) are located next to
/// `args.input` and read explicitly (and can't be `"-"` — see the module doc
/// comment); every other format reads a single buffer — `args.input`'s bytes,
/// or all of stdin when it's `"-"` — through [`convert::read_features`].
fn read_input(args: &cli::Args) -> Result<FeatureCollection> {
    if args.from == Format::Shapefile {
        if args.input == "-" {
            return Err(no_stdio_for_multifile("Shapefile"));
        }
        return read_shapefile_from_path(&args.input, args.progress);
    }
    let input = read_bytes(&args.input)?;
    if args.progress {
        eprintln!("read {} ({} bytes)", io_label(&args.input, "stdin"), input.len());
    }
    convert::read_features(args.from, &input)
}

/// Write the Feature IR to `output_path`. Shapefile is multi-file, so its
/// sibling files are written explicitly under `output_path`'s stem (and
/// `output_path` can't be `"-"` — see the module doc comment); every other
/// format writes a single buffer — to `output_path`, or to stdout when it's
/// `"-"` — through [`convert::write_features`]. Shared by the plain
/// single-collection path and GeoPackage's per-layer fan-out
/// (`run_geopackage_read`), so a multi-layer `.gpkg` → Shapefile conversion
/// gets one `layer.shp` sibling set per layer for free.
fn write_collection(to: Format, output_path: &str, fc: &FeatureCollection, quiet: bool) -> Result<()> {
    if to == Format::Shapefile {
        if output_path == "-" {
            return Err(no_stdio_for_multifile("Shapefile"));
        }
        return write_shapefile_to_path(output_path, fc, quiet);
    }
    let bytes = convert::write_features(to, fc)?;
    write_bytes(output_path, &bytes)?;
    eprintln!("wrote {} ({} bytes)", io_label(output_path, "stdout"), bytes.len());
    Ok(())
}

/// Read a Shapefile given its `.shp` path, locating `.dbf` (required),
/// `.prj`, and `.cpg` (both optional) alongside it.
fn read_shapefile_from_path(shp_path: &str, progress: bool) -> Result<FeatureCollection> {
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
    shapefile::read(&shp, &dbf_bytes, prj.as_deref(), cpg.as_deref())
}

/// Write a Shapefile's sibling files under `output_path`'s stem (a trailing
/// `.shp`/`.SHP`, if present, is stripped so `roads.shp` and `roads` both name
/// the same `roads.{shp,shx,dbf,prj}` set).
fn write_shapefile_to_path(output_path: &str, fc: &FeatureCollection, quiet: bool) -> Result<()> {
    let encoded = shapefile::write(fc)?;
    print_warnings(&encoded.warnings, quiet);
    let stem = strip_shp_extension(output_path);
    std::fs::write(format!("{stem}.shp"), &encoded.shp)?;
    std::fs::write(format!("{stem}.shx"), &encoded.shx)?;
    std::fs::write(format!("{stem}.dbf"), &encoded.dbf)?;
    if let Some(prj) = &encoded.prj {
        std::fs::write(format!("{stem}.prj"), prj)?;
    }
    eprintln!(
        "wrote {stem}.shp + .shx + .dbf{} ({} features)",
        if encoded.prj.is_some() { " + .prj" } else { "" },
        fc.features.len(),
    );
    Ok(())
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

    // "-" (stdout) is never a directory, regardless of what happens to exist
    // in the current directory — a multi-layer .gpkg piped out is rejected
    // below by the same "give a directory or --layer" error a plain file path
    // gets, since a single stream can't fan out to more than one layer.
    let as_dir = out.path != "-"
        && (out.path.ends_with('/') || std::path::Path::new(out.path).is_dir());
    if layers.len() == 1 && !as_dir {
        let (_, fc) = &layers[0];
        return write_collection(out.format, out.path, fc, args.quiet);
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
        write_collection(out.format, &path.to_string_lossy(), fc, args.quiet)?;
    }
    Ok(())
}

/// Write a layer into a GeoPackage, creating it or appending (upserting the
/// layer if it already exists). The layer name defaults to the input file stem.
fn run_geopackage_write(args: &cli::Args, out: &Output, crs_override: Option<&Crs>) -> Result<()> {
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
        ls
    } else {
        // Shapefile input's siblings are located via read_input; every other
        // format reads a single buffer the same way it always has.
        let fc = read_input(args)?;
        let name = args
            .layer
            .clone()
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
    if args.progress {
        let verb = if existing.is_some() { "appending to" } else { "writing" };
        let idx = if args.rtree { " (+rtree index)" } else { "" };
        eprintln!("{verb} {}{idx}...", io_label(out.path, "stdout"));
    }
    let bytes = geopackage::write_layers(existing.as_deref(), &new_layers, args.rtree)?;
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
fn collect_conversion_warnings(fc: &FeatureCollection, target: Format) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(w) = fc.crs.as_ref().and_then(|c| c.downgrade_warning(target)) {
        warnings.push(w);
    }
    if let Some(w) = fc.m_downgrade_warning(target) {
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

/// The file stem of `path`, used as a default layer name. `"-"` (stdin) has
/// no filename to draw one from, so it falls back to `"layer"` same as an
/// extension-less/empty path does.
fn layer_stem(path: &str) -> String {
    if path == "-" {
        return "layer".into();
    }
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "layer".into())
}
