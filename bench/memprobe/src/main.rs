//! Where a conversion's bytes go (`plans/performance.org` § The probe).
//!
//! Links geosetta as a library under a counting `#[global_allocator]` and
//! splits one conversion into its phases: the input bytes, the read
//! (`pipeline::prepare`), the IR left resident once the input is dropped, and
//! the write (`Prepared::encode`), whose scratch is its peak less the IR and
//! the output. RSS says how much; this says where.
//!
//! ```sh
//! cargo run --release --manifest-path bench/memprobe/Cargo.toml -- in.geojson kml
//! cargo run --release --manifest-path bench/memprobe/Cargo.toml -- in.parquet geojson --compact
//! ```
//!
//! Arguments mirror the CLI's: `<input> <output format> [--from FMT]
//! [--compact] [--kml-version 2.2|2.3] [--sort-hilbert] [--layout L]`. The
//! read is the binary's own: `prepare` decodes, sorts when asked, releases
//! natives the target cannot replay, and leaves the collection in the form
//! the target's writer reads (`plans/columnar.org` D-C15). A single-buffer
//! input only: not a GeoPackage, and not a Shapefile.
use geosetta::pipeline::{self, ConvertOptions, Event, Output, Source};
use geosetta::{Format, KmlVersion, ReadOptions, WriteOptions};
use std::alloc::{GlobalAlloc, Layout, System};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
            ALLOCS.fetch_add(1, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new) };
        if !q.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            grow(new);
            ALLOCS.fetch_add(1, Relaxed);
        }
        q
    }
}

fn grow(by: usize) {
    let now = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// One phase's peak, its allocations and its time.
struct Phase {
    peak: usize,
    allocs: usize,
    ms: f64,
}

fn phase<T>(f: impl FnOnce() -> T) -> (T, Phase) {
    PEAK.store(LIVE.load(Relaxed), Relaxed);
    let allocs = ALLOCS.load(Relaxed);
    let t = Instant::now();
    let value = f();
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let p = Phase { peak: PEAK.load(Relaxed), allocs: ALLOCS.load(Relaxed) - allocs, ms };
    (value, p)
}

fn mb(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: memprobe <input> <output format> [--from FMT] [--compact] [--kml-version 2.2|2.3] [--sort-hilbert] [--layout auto|rows|columns]";
    let (Some(path), Some(to)) = (args.first(), args.get(1)) else { return Err(usage.into()) };
    let to = Format::parse(to).map_err(|e| e.to_string())?;
    let mut from = Format::from_path(path);
    let mut options = WriteOptions::default();
    let mut sort = false;
    let mut read_options = ReadOptions::default();
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--compact" => options.compact = true,
            "--sort-hilbert" => sort = true,
            "--from" => from = Some(Format::parse(rest.next().ok_or(usage)?).map_err(|e| e.to_string())?),
            "--layout" => {
                read_options.layout =
                    geosetta::Layout::parse(rest.next().ok_or(usage)?).map_err(|e| e.to_string())?
            }
            "--kml-version" => {
                options.kml_version = Some(KmlVersion::parse(rest.next().ok_or(usage)?).map_err(|e| e.to_string())?)
            }
            other => return Err(format!("unknown option {other:?}\n{usage}")),
        }
    }
    let from = from.ok_or("cannot tell the input's format from its name; pass --from")?;

    let base = LIVE.load(Relaxed);
    let (input, read_in) = phase(|| std::fs::read(path).map_err(|e| format!("{path}: {e}")));
    let input = input?;
    let size = input.len();
    let convert = ConvertOptions { read: read_options, write: options, sort_hilbert: sort, ..ConvertOptions::default() };
    let mut quiet = |_: Event<'_>| {};
    let (prepared, read) = phase(|| pipeline::prepare(from, to, Source::Bytes(&input), convert, &mut quiet));
    let prepared = prepared.map_err(|e| e.to_string())?;
    drop(input);
    let ir = LIVE.load(Relaxed) - base;
    let (out, write) = phase(|| prepared.encode(0, &mut quiet));
    let out = match out.map_err(|e| e.to_string())? {
        Output::Bytes(bytes) => bytes.len(),
        Output::Shapefile(_) => return Err("a Shapefile output is several files; write one buffer".into()),
    };
    let features = prepared.layers()[0].1.len();
    let rows = features.max(1);

    println!("{path} -> {to:?} ({}): {features} features", read_options.layout.as_str());
    println!("  input           {:>10}", mb(size));
    println!(
        "  read            peak {:>10}  {:>8.1}M allocs  {:>7.0} ms   (reading the file: {:.0} ms)",
        mb(read.peak - base),
        read.allocs as f64 / 1e6,
        read.ms,
        read_in.ms
    );
    println!("  IR resident     {:>10}  ({:.0} B/feature)", mb(ir), ir as f64 / rows as f64);
    println!(
        "  write           peak {:>10}  {:>8.1}M allocs  {:>7.0} ms",
        mb(write.peak - base),
        write.allocs as f64 / 1e6,
        write.ms
    );
    println!("  write scratch   {:>10}  (peak less IR and output)", mb((write.peak - base).saturating_sub(ir + out)));
    println!("  output          {:>10}", mb(out));
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("memprobe: {e}");
            ExitCode::FAILURE
        }
    }
}
