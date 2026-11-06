//! Geosetta — the `geosetta` command-line front end.
//!
//! A wrapper on the library crate ([`geosetta`]): every step of a conversion is
//! [`geosetta::pipeline`]'s, and this binary parses arguments, moves bytes
//! between files or stdio and the pipeline, and presents what the pipeline
//! reports — `warning: ` lines, `--quiet`, `--progress`. What stays here is
//! what touches a path, a stream or a terminal (`plans/library-pipeline.org`
//! § WHAT STAYS IN THE BINARY).
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
//! the target *is* the source's format. See `convert::conversion_warnings`.
//!
//! The same-file refusal ([`refuse_same_file`]) is the other half, and it is
//! not merely tidiness: the format check that used to reject `a.fgb a.fgb` had
//! been serving as an accidental in-place guard, and removing it without
//! replacing it would newly expose a run that truncates its only copy before
//! writing.

mod cli;

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use geosetta::pipeline::{self, ConvertOptions, Event, Output, Prepared, Source};
use geosetta::shapefile;
use geosetta::Crs;
use geosetta::{Error, FeatureCollection, Format, Result};

/// An i/o failure, saying which path it was about: `No such file or directory`
/// alone does not say which of an input, an output and a `--crs` is missing.
fn at(path: impl AsRef<Path>) -> impl FnOnce(std::io::Error) -> Error {
    move |e| Error::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.as_ref().display())))
}

/// Read `path`'s bytes, or all of stdin when `path` is `"-"`.
fn read_bytes(path: &OsStr) -> Result<Vec<u8>> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin().lock().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        std::fs::read(path).map_err(at(path))
    }
}

/// Write `bytes` to `path`, or to stdout when `path` is `"-"`.
fn write_bytes(path: &OsStr, bytes: &[u8]) -> Result<()> {
    if path == "-" {
        write_stdout(bytes)
    } else {
        std::fs::write(path, bytes).map_err(at(path))
    }
}

/// Write `bytes` to stdout, where a closed pipe is an error like any other.
/// `println!` panics on one: `geosetta in.geojson --print-crs-code | true`.
fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush().map_err(Error::from)
}

/// A human label for `--progress`/status messages: `path` as text, or
/// `"stdin"`/`"stdout"` for `"-"` (which direction depends on the caller). Lossy,
/// which is why it is only ever for a message and never for a file.
fn io_label<'a>(path: &'a OsStr, when_dash: &'a str) -> Cow<'a, str> {
    if path == "-" { Cow::Borrowed(when_dash) } else { path.to_string_lossy() }
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
    let args = match cli::parse(std::env::args_os())? {
        cli::Invocation::Run(args) => args,
        cli::Invocation::Help => return write_stdout(format!("{}\n", cli::USAGE).as_bytes()),
        cli::Invocation::Version => {
            return write_stdout(format!("geosetta {}\n", env!("CARGO_PKG_VERSION")).as_bytes());
        }
    };

    // The `--print-crs*` flags read a source and report on it: no output, no
    // conversion. They return before every write-side path below, GeoPackage's
    // included (that container's multi-layer read has its own answer for what to
    // report — see `print_crs_code` and `print_crs`).
    let out = match &args.mode {
        cli::Mode::PrintCrsCode => return print_crs_code(&args),
        cli::Mode::PrintCrs => return print_crs(&args),
        cli::Mode::Convert { output, to } => Destination { path: output, format: *to },
    };

    // Before anything is read or written: an output that would overwrite its
    // own input is refused outright (D4).
    refuse_same_file(&args.input, args.from, &out)?;
    refuse_layer_with_nowhere_to_go(&args, &out)?;

    // Read the `--crs` override up front: a malformed one should fail before
    // anything has been read or written, not midway through.
    let crs = load_crs_override(&args)?;

    // Every format routes through the library's one pipeline, so any input
    // format converts to any output format the writers support. What is left
    // here is getting the bytes in and out: Shapefile's siblings are located
    // on disk, a GeoPackage output is appended to an existing file, and a
    // multi-layer source fans out to a directory.
    let input = read_input(&args)?;
    let mut report = reporter(&args, out.format);
    let prepared = pipeline::prepare(args.from, out.format, input.source(), convert_options(&args, &out, crs), &mut report)?;
    print_warnings(&lossy_layer_name_warning(&args.input, prepared.layers()), args.quiet);
    if out.format == Format::Gpkg {
        return write_geopackage(&out, prepared, args.progress, &mut report);
    }
    write_layers(&args, &out, &prepared, &mut report)
}

/// The destination half of a conversion — where output goes and in what format.
/// Lifted out of [`cli::Mode::Convert`] once, in [`run`], so nothing downstream
/// has to re-match the mode to find out whether it has an output at all.
struct Destination<'a> {
    path: &'a OsStr,
    format: Format,
}

/// What the pipeline does beyond the encode, from the flags. The default layer
/// name is the input's file stem, and the GeoPackage survey messages name each
/// container by its path, or `stdin`/`stdout`.
fn convert_options(args: &cli::Args, out: &Destination, crs: Option<Crs>) -> ConvertOptions {
    ConvertOptions {
        read: args.read_options(),
        write: args.write_options(),
        crs,
        sort_hilbert: args.sort_hilbert,
        layer: args.layer.clone(),
        default_layer_name: Some(layer_stem(&args.input)),
        rtree: args.rtree,
        envelope: args.envelope,
        source_label: Some(io_label(&args.input, "stdin").into_owned()),
        destination_label: Some(io_label(out.path, "stdout").into_owned()),
    }
}

/// Present the pipeline's events: each warning as a `warning: ` line unless
/// `--quiet`, and the stage boundaries as `--progress` lines.
///
/// The `warning: ` prefix the convention requires is added *here*, not by each
/// check, so a new check cannot forget it or spell it differently — which is
/// exactly what had happened: the `.dbf` warnings were written with the
/// `shapefile: ` tag the shapefile *errors* use, and so read as errors. Checks
/// return the message body; presentation is this function's job. A message may
/// still carry its own component tag after the prefix (`warning: shapefile: …`).
fn reporter(args: &cli::Args, to: Format) -> impl FnMut(Event<'_>) + '_ {
    move |event| match event {
        Event::Warning(w) => print_warnings(&[w], args.quiet),
        Event::Read { layers } if args.progress => {
            if args.from == Format::Gpkg {
                eprintln!("found {} layer(s)", layers.len());
                for (name, fc) in layers {
                    eprintln!("  layer {name}: {} features", fc.len());
                }
            } else {
                let features: usize = layers.iter().map(|(_, fc)| fc.len()).sum();
                eprintln!("parsed {features} features from {}", args.from.extension());
            }
        }
        // A GeoPackage output names its layers, so say which.
        Event::Selected { layers } if args.progress && to == Format::Gpkg => {
            for (name, fc) in layers {
                eprintln!("layer {name}: {} features", fc.len());
            }
        }
        Event::Sorted { features } if args.progress => {
            eprintln!("sorted {features} features by Hilbert locality");
        }
        Event::Encoding if args.progress => eprintln!("writing {}...", to.extension()),
        _ => {}
    }
}

/// The input, read: one buffer, or a Shapefile's siblings.
enum Input {
    Bytes(Vec<u8>),
    Shapefile { shp: Vec<u8>, dbf: Vec<u8>, prj: Option<String>, cpg: Option<String> },
}

impl Input {
    fn source(&self) -> Source<'_> {
        match self {
            Input::Bytes(bytes) => Source::Bytes(bytes),
            Input::Shapefile { shp, dbf, prj, cpg } => {
                Source::Shapefile { shp, dbf, prj: prj.as_deref(), cpg: cpg.as_deref() }
            }
        }
    }
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
        std::fs::read_to_string(path).map_err(at(path))?
    };
    Crs::from_definition_text(&text).map(Some)
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
    write_stdout(codes.iter().map(|code| format!("{code}\n")).collect::<String>().as_bytes())
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
    let input = read_input(args)?;
    let options = ConvertOptions { read: args.read_options(), layer: args.layer.clone(), ..ConvertOptions::default() };
    let layers = pipeline::read(args.from, input.source(), &options, &mut reporter(args, args.from))?;
    let multi_layer = args.from == Format::Gpkg;
    Ok(layers
        .into_iter()
        .map(|(name, fc)| (multi_layer.then_some(name), fc))
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
            let text = if args.escape { escape_control_bytes(body) } else { (*body).to_string() };
            write_stdout(format!("{text}\n").as_bytes())
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

/// Read the input. Shapefile is multi-file, so its sibling files (`.dbf`
/// required, `.prj`/`.cpg` optional) are located next to `args.input` and read
/// explicitly (and can't be `"-"` — see the module doc comment); every other
/// format reads a single buffer — `args.input`'s bytes, or all of stdin when
/// it's `"-"`.
fn read_input(args: &cli::Args) -> Result<Input> {
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
    Ok(Input::Bytes(input))
}

/// Write every layer of a conversion to a target that is not GeoPackage.
/// Output naming: a directory (existing or trailing `/`) gets one `layer.ext`
/// file each; a single layer goes to the plain output path; multiple layers to
/// a plain path is an error unless `--layer` selects one.
fn write_layers(
    args: &cli::Args,
    out: &Destination,
    prepared: &Prepared,
    report: &mut dyn FnMut(Event<'_>),
) -> Result<()> {
    let layers = prepared.layers();
    // "-" (stdout) is never a directory, regardless of what happens to exist
    // in the current directory — a multi-layer .gpkg piped out is rejected
    // below by the same "give a directory or --layer" error a plain file path
    // gets, since a single stream can't fan out to more than one layer.
    let as_dir = is_directory_output(out.path);
    if layers.len() == 1 && !as_dir {
        return write_layer(out.format, out.path, prepared, 0, args.quiet, report);
    }
    if !as_dir {
        return Err(Error::Usage(
            "GeoPackage has multiple layers: give an output directory (trailing '/') or --layer NAME".into(),
        ));
    }

    // Every name before the first file, so one bad name leaves nothing behind.
    for (name, _) in layers {
        refuse_unless_file_name(name, out.path)?;
    }
    std::fs::create_dir_all(out.path).map_err(at(out.path))?;
    let dir = std::path::Path::new(out.path);
    for (i, (name, _)) in layers.iter().enumerate() {
        let path = dir.join(format!("{name}.{}", out.format.extension()));
        write_layer(out.format, path.as_os_str(), prepared, i, args.quiet, report)?;
    }
    Ok(())
}

/// Whether `path` names a directory to fan out into: one that exists, or that a
/// trailing `/` asks for. `"-"` never does, whatever is in the current directory.
fn is_directory_output(path: &OsStr) -> bool {
    path != "-" && (path.as_encoded_bytes().ends_with(b"/") || Path::new(path).is_dir())
}

/// Refuse `--layer` where it could do nothing: it names no layer of the source,
/// and the output has nowhere to keep a name.
///
/// A GeoPackage source is selecting. A directory names its files after their
/// layers. Otherwise the format decides ([`Format::records_name`]). Here and not
/// in `cli.rs` because a directory is a question for the disk, and not in the
/// library because a caller there gets the name back from
/// [`Prepared::layers`], so the option is never ignored for it
/// (`plans/cli-review.org` C3).
fn refuse_layer_with_nowhere_to_go(args: &cli::Args, out: &Destination) -> Result<()> {
    if args.layer.is_none() || args.from == Format::Gpkg || out.format.records_name() || is_directory_output(out.path) {
        return Ok(());
    }
    Err(Error::Usage(format!(
        "--layer names the output, and one {} file has nowhere to record a name; give a \
         directory as the output to name the file after it",
        out.format.display_name()
    )))
}

/// Refuse a layer name that would not stay in `dir` as one file.
///
/// A name comes from the input — a GeoJSON `name`, a FlatGeobuf header, a
/// GeoPackage table — so it is untrusted, and `Path::join` would take `../x` past
/// the directory and an absolute name in place of it, overwriting whatever is
/// there (`plans/cli-review.org` C9). Refused rather than rewritten, because a
/// quietly changed name is a file nobody asked for. The name is printed by
/// `{:?}` since it may hold bytes that would act on a terminal.
fn refuse_unless_file_name(name: &str, dir: &OsStr) -> Result<()> {
    let mut parts = Path::new(name).components();
    // Comparing the part to the whole catches what `components` normalises away:
    // `x/` and `x/.` are each one part.
    if matches!((parts.next(), parts.next()), (Some(Component::Normal(part)), None) if part == OsStr::new(name)) {
        return Ok(());
    }
    Err(Error::Usage(format!(
        "layer {name:?} is not a plain file name, so it cannot name a file in {}; \
         write it to a file path, or pick another layer with --layer",
        Path::new(dir).display(),
    )))
}

/// Encode layer `index` and write it to `output_path`. Shapefile is
/// multi-file, so its sibling files are written explicitly under
/// `output_path`'s stem (and `output_path` can't be `"-"` — see the module doc
/// comment); every other format writes a single buffer — to `output_path`, or
/// to stdout when it's `"-"`. One layer at a time, so a multi-layer fan-out
/// writes each file before encoding the next.
fn write_layer(
    to: Format,
    output_path: &OsStr,
    prepared: &Prepared,
    index: usize,
    quiet: bool,
    report: &mut dyn FnMut(Event<'_>),
) -> Result<()> {
    if to == Format::Shapefile && output_path == "-" {
        return Err(no_stdio_for_multifile("Shapefile"));
    }
    match prepared.encode(index, report)? {
        Output::Bytes(bytes) => {
            write_bytes(output_path, &bytes)?;
            eprintln!("wrote {} ({} bytes)", io_label(output_path, "stdout"), bytes.len());
            Ok(())
        }
        Output::Shapefile(encoded) => {
            write_shapefile_to_path(output_path, &encoded, prepared.layers()[index].1.len(), quiet)
        }
    }
}

/// Read a Shapefile given its `.shp` path, locating `.dbf` (required),
/// `.prj`, and `.cpg` (both optional) alongside it.
fn read_shapefile_from_path(shp_path: &OsStr, progress: bool) -> Result<Input> {
    let shown = Path::new(shp_path).display();
    let shp = std::fs::read(shp_path).map_err(at(shp_path))?;
    let dbf_path = sibling_path(shp_path, "dbf")
        .ok_or_else(|| Error::Usage(format!("shapefile: no .dbf found alongside {shown}")))?;
    let dbf = std::fs::read(&dbf_path).map_err(at(&dbf_path))?;
    let text = |ext| sibling_path(shp_path, ext).map(|p| std::fs::read_to_string(&p).map_err(at(&p))).transpose();
    let prj = text("prj")?;
    let cpg = text("cpg")?;
    if progress {
        eprintln!(
            "read {shown} ({} bytes) + {} ({} bytes){}",
            shp.len(),
            dbf_path.display(),
            dbf.len(),
            if prj.is_some() { " + .prj" } else { "" },
        );
    }
    Ok(Input::Shapefile { shp, dbf, prj, cpg })
}

/// Every path a side of the conversion touches, canonicalised for comparison.
///
/// A Shapefile is a *file set*, not a path, so it contributes all its
/// siblings: the collision `plans/same-format-output.org` D4 is about is
/// `geosetta roads.shp roads.dbf` as much as `roads.shp roads.shp`.
///
/// `-` contributes nothing — stdin and stdout are different streams even when
/// both are the same terminal.
fn touched_paths(path: &OsStr, format: Format) -> Vec<PathBuf> {
    if path == "-" {
        return Vec::new();
    }
    let raw: Vec<PathBuf> = if format == Format::Shapefile {
        let stem = strip_shp_extension(path);
        ["shp", "shx", "dbf", "prj", "cpg"].iter().map(|e| with_suffix(&stem, e)).collect()
    } else {
        vec![PathBuf::from(path)]
    };
    raw.iter().filter_map(|p| canonical_for_compare(p)).collect()
}

/// Canonicalise `path` even when it does not exist yet.
///
/// The output usually does not, which is the whole difficulty: `canonicalize`
/// fails on a missing file, so fall back to canonicalising the *parent* — which
/// does exist — and rejoining the file name. That still resolves `./a.fgb`,
/// `a.fgb` and a symlinked directory to one answer, which is the point.
fn canonical_for_compare(p: &Path) -> Option<PathBuf> {
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
fn refuse_same_file(input: &OsStr, from: Format, out: &Destination) -> Result<()> {
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
/// the same `roads.{shp,shx,dbf,cpg,prj}` set). The encode's own `.dbf`
/// warnings were reported by the pipeline already.
///
/// Writes every file it can and deletes none, then reports the sidecars it left
/// behind — see [`stale_sidecar_warnings`] and `plans/shapefile.org`
/// § THE SIDECAR POLICY.
fn write_shapefile_to_path(output_path: &OsStr, encoded: &shapefile::Encoded, features: usize, quiet: bool) -> Result<()> {
    let stem = strip_shp_extension(output_path);
    let write = |ext: &str, bytes: &[u8]| {
        let path = with_suffix(&stem, ext);
        std::fs::write(&path, bytes).map_err(at(&path))
    };
    write("shp", &encoded.shp)?;
    write("shx", &encoded.shx)?;
    write("dbf", &encoded.dbf)?;
    write("cpg", encoded.cpg.as_bytes())?;
    if let Some(prj) = &encoded.prj {
        write("prj", prj.as_bytes())?;
    }
    // After writing: say what of the destination's existing file set this run
    // left behind, because those files now describe the output beside them.
    // See `stale_sidecar_warnings` on why this cannot be checked beforehand.
    print_warnings(&stale_sidecar_warnings(&stem, encoded), quiet);
    eprintln!(
        "wrote {}.shp + .shx + .dbf + .cpg{} ({features} features)",
        Path::new(&stem).display(),
        if encoded.prj.is_some() { " + .prj" } else { "" },
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
fn stale_sidecar_warnings(stem: &OsStr, encoded: &shapefile::Encoded) -> Vec<String> {
    let stem_path = Path::new(stem);
    let dir = match stem_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let Some(base) = stem_path.file_name() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut warnings = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        // Only this output's own basename, and only a sidecar of it. Every
        // extension here is ASCII, so one that is not text is no sidecar.
        let Some(ext) = name
            .as_encoded_bytes()
            .strip_prefix(base.as_encoded_bytes())
            .and_then(|rest| rest.strip_prefix(b"."))
            .and_then(|rest| std::str::from_utf8(rest).ok())
        else {
            continue;
        };
        let lower = ext.to_ascii_lowercase();
        let path = dir.join(&name);

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
                    "shapefile: {} already exists and this run wrote {}.{lower}, so both \
                     spellings are present and readers disagree about which one wins \
                     (plans/shapefile.org)",
                    path.display(),
                    stem_path.display(),
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

fn strip_shp_extension(path: &OsStr) -> OsString {
    let p = Path::new(path);
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("shp") => p.with_extension("").into_os_string(),
        _ => path.to_os_string(),
    }
}

/// `stem.ext`. Not `Path::with_extension`, which would replace what follows a
/// dot already in the stem: `a.b` and `shp` make `a.b.shp`.
fn with_suffix(stem: &OsStr, ext: &str) -> PathBuf {
    let mut name = stem.to_os_string();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// Find a sibling file next to `shp_path` with extension `ext`, trying the
/// `.shp` path's own case first and its opposite second — real-world
/// shapefiles mix `.DBF`/`.dbf` casing. `None` when neither casing exists.
fn sibling_path(shp_path: &OsStr, ext: &str) -> Option<PathBuf> {
    let path = Path::new(shp_path);
    let stem = path.file_stem()?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let shp_ext_is_upper = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.chars().next().is_some_and(|c| c.is_ascii_uppercase()));
    let candidates =
        if shp_ext_is_upper { [ext.to_ascii_uppercase(), ext.to_string()] } else { [ext.to_string(), ext.to_ascii_uppercase()] };
    candidates.into_iter().find_map(|e| {
        let name = with_suffix(stem, &e);
        let p = match dir {
            Some(d) => d.join(&name),
            None => name,
        };
        p.exists().then_some(p)
    })
}

/// Write a conversion into a GeoPackage, creating it or appending (upserting
/// each layer if it already exists).
///
/// Append into the existing GeoPackage if the output already exists. "-"
/// (stdout) is never "existing" — there's nothing to read back from a pipe
/// you're about to write to, so piping to a .gpkg always starts fresh. The
/// destination is never the source here: that run is refused by
/// `same-format-output.org` D4 before anything is read.
fn write_geopackage(
    out: &Destination,
    prepared: Prepared,
    progress: bool,
    report: &mut dyn FnMut(Event<'_>),
) -> Result<()> {
    let existing = if out.path == "-" { None } else { std::fs::read(out.path).ok() };
    let (rtree, envelope) = (prepared.options().rtree, prepared.options().envelope);
    let mut events = |event: Event<'_>| match event {
        Event::Encoding => {
            if progress {
                let verb = if existing.is_some() { "appending to" } else { "writing" };
                let idx = if rtree { " (+rtree index)" } else { "" };
                let env = if envelope { " (+envelopes)" } else { "" };
                eprintln!("{verb} {}{idx}{env}...", io_label(out.path, "stdout"));
            }
        }
        event => report(event),
    };
    let bytes = prepared.into_geopackage(existing.as_deref(), &mut events)?;
    write_bytes(out.path, &bytes)?;
    eprintln!("wrote {} ({} bytes)", io_label(out.path, "stdout"), bytes.len());
    Ok(())
}

/// Print each warning to stderr as `warning: …`, unless `quiet`. Every warning
/// reaches the user through here — the pipeline's, by way of [`reporter`], and
/// the binary's own [`stale_sidecar_warnings`] — see
/// `plans/lossy-conversion-warnings.org`.
fn print_warnings(warnings: &[String], quiet: bool) {
    if quiet {
        return;
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
}

/// The file stem of `path`, used as a default layer name of *last* resort:
/// `--layer` and the collection's own [`FeatureCollection::name`] are both
/// consulted first. `"-"` (stdin) has no filename to draw one from, so it falls
/// back to `"layer"` same as an extension-less/empty path does. A layer's name
/// is text, so this is lossy for a stem that is not:
/// [`lossy_layer_name_warning`] says so when it mattered.
fn layer_stem(path: &OsStr) -> String {
    if path == "-" {
        return "layer".into();
    }
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "layer".into())
}

/// The warning for a layer that took its name from a stem which is not text, so
/// that the output's name differs from the input's.
fn lossy_layer_name_warning(input: &OsStr, layers: &[(String, FeatureCollection)]) -> Vec<String> {
    let lossy = Path::new(input).file_stem().is_some_and(|stem| stem.to_str().is_none());
    let stem = layer_stem(input);
    if !lossy || !layers.iter().any(|(name, _)| *name == stem) {
        return Vec::new();
    }
    vec![format!("the input's file name is not UTF-8, so its layer is named {stem:?}; give --layer to choose")]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C1's lossy default name. On macOS no such file can exist, so this is
    /// tested on the name alone.
    #[cfg(unix)]
    #[test]
    fn a_layer_named_from_a_stem_that_is_not_text_says_so() {
        use std::os::unix::ffi::OsStrExt;
        let input = OsStr::from_bytes(b"\xff.geojson");
        let stem = layer_stem(input);
        let layer = |name: &str| vec![(name.to_string(), FeatureCollection::default())];
        assert_eq!(lossy_layer_name_warning(input, &layer(&stem)).len(), 1);
        assert_eq!(lossy_layer_name_warning(input, &layer("chosen")), Vec::<String>::new());
        assert_eq!(lossy_layer_name_warning(OsStr::new("a.geojson"), &layer("a")), Vec::<String>::new());
    }

    #[test]
    fn a_suffix_keeps_the_dots_in_the_stem() {
        assert_eq!(with_suffix(OsStr::new("dir/a.b"), "shp"), Path::new("dir/a.b.shp"));
        assert_eq!(strip_shp_extension(OsStr::new("a.b.SHP")), "a.b");
        assert_eq!(strip_shp_extension(OsStr::new("a.b")), "a.b");
    }

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
