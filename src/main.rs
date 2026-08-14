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
//! Shapefile is excluded: it's sibling files (`.shp`/`.shx`/`.dbf`/`.prj`),
//! not a single byte stream, so there's nothing to pipe. Route it through a
//! single-buffer format instead (FlatGeobuf and GeoParquet both carry the CRS
//! faithfully): `geosetta in.shp - --to fgb | ... | geosetta - out.shp --from fgb`.

use std::io::{Read, Write};

use geosetta::{cli, convert, geopackage, shapefile};
use geosetta::{Crs, Error, FeatureCollection, Format, Result};

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
    warn_crs_loss(&args, fc.crs.as_ref());
    if args.progress {
        eprintln!("writing {}...", args.to.extension());
    }
    write_collection(args.to, &args.output, &fc)
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
fn write_collection(to: Format, output_path: &str, fc: &FeatureCollection) -> Result<()> {
    if to == Format::Shapefile {
        if output_path == "-" {
            return Err(no_stdio_for_multifile("Shapefile"));
        }
        return write_shapefile_to_path(output_path, fc);
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
fn write_shapefile_to_path(output_path: &str, fc: &FeatureCollection) -> Result<()> {
    let encoded = shapefile::write(fc)?;
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
fn run_geopackage_read(args: &cli::Args) -> Result<()> {
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
    warn_crs_loss_layers(args, &layers);

    // "-" (stdout) is never a directory, regardless of what happens to exist
    // in the current directory — a multi-layer .gpkg piped out is rejected
    // below by the same "give a directory or --layer" error a plain file path
    // gets, since a single stream can't fan out to more than one layer.
    let as_dir = args.output != "-"
        && (args.output.ends_with('/') || std::path::Path::new(&args.output).is_dir());
    if layers.len() == 1 && !as_dir {
        let (_, fc) = &layers[0];
        return write_collection(args.to, &args.output, fc);
    }
    if !as_dir {
        return Err(Error::Usage(
            "GeoPackage has multiple layers: give an output directory (trailing '/') or --layer NAME".into(),
        ));
    }

    std::fs::create_dir_all(&args.output)?;
    let dir = std::path::Path::new(&args.output);
    for (name, fc) in &layers {
        let path = dir.join(format!("{name}.{}", args.to.extension()));
        write_collection(args.to, &path.to_string_lossy(), fc)?;
    }
    Ok(())
}

/// Write a layer into a GeoPackage, creating it or appending (upserting the
/// layer if it already exists). The layer name defaults to the input file stem.
fn run_geopackage_write(args: &cli::Args) -> Result<()> {
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
    warn_crs_loss_layers(args, &new_layers);

    // Append into the existing GeoPackage if the output already exists. "-"
    // (stdout) is never "existing" — there's nothing to read back from a pipe
    // you're about to write to, so piping to a .gpkg always starts fresh.
    let existing = if args.output == "-" { None } else { std::fs::read(&args.output).ok() };
    if args.progress {
        let verb = if existing.is_some() { "appending to" } else { "writing" };
        let idx = if args.rtree { " (+rtree index)" } else { "" };
        eprintln!("{verb} {}{idx}...", io_label(&args.output, "stdout"));
    }
    let bytes = geopackage::write_layers(existing.as_deref(), &new_layers, args.rtree)?;
    write_bytes(&args.output, &bytes)?;
    eprintln!("wrote {} ({} bytes)", io_label(&args.output, "stdout"), bytes.len());
    Ok(())
}

/// Warn on stderr when the target format cannot record `crs` (unless `--quiet`).
/// A correctness warning — the conversion still proceeds and succeeds; see
/// [`Crs::downgrade_warning`].
fn warn_crs_loss(args: &cli::Args, crs: Option<&Crs>) {
    if args.quiet {
        return;
    }
    if let Some(w) = crs.and_then(|c| c.downgrade_warning(args.to)) {
        eprintln!("{w}");
    }
}

/// [`warn_crs_loss`] across a GeoPackage's layers, de-duplicating identical
/// messages so a single-CRS `.gpkg` (the usual case) warns just once even when
/// it fans out to many output files.
fn warn_crs_loss_layers(args: &cli::Args, layers: &[(String, FeatureCollection)]) {
    if args.quiet {
        return;
    }
    let mut seen = std::collections::BTreeSet::new();
    for (_, fc) in layers {
        if let Some(w) = fc.crs.as_ref().and_then(|c| c.downgrade_warning(args.to))
            && seen.insert(w.clone())
        {
            eprintln!("{w}");
        }
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
